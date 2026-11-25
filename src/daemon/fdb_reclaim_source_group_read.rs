//! One bounded source read before original group admission. The temporary
//! files remain private until both physical observations have been checked.
use super::*;
use crate::daemon::executor::fdb_operation_guard::Guard;
use crate::ltfs::volume::read_extent_plan_sequential_to_writer;
use crate::managed_catalog::{ReclaimCopyGroup, ReclaimLayout};
use raw_reclaim::StreamHash;
use std::future::Future;
use std::pin::Pin;

pub(super) type GroupSourceRead = (u8, File, ReclaimSourceRead);

struct MemberRead<'a> {
    ordinal: u8,
    file: &'a ReclaimFile,
    path: Vec<u8>,
    source: ActiveReadSource,
}

struct PendingRead {
    physical: VerifiedPhysicalPlan,
    spool: File,
    sha256: [u8; 32],
    md5: Option<[u8; 16]>,
}

fn error(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(detail.to_string())
}

impl MemberRead<'_> {
    fn target<'a>(&'a self, parent: &'a ReclaimReservation) -> ReadTarget<'a> {
        ReadTarget::ReclaimSource(parent, self.file, &self.path)
    }

    async fn require_source(
        &self,
        reader: &ActiveMediaReader<'_>,
        parent: &ReclaimReservation,
    ) -> Result<()> {
        if self.target(parent).resolve(reader).await?.as_ref() != Some(&self.source) {
            return Err(error("回收组读取期间原源文件或FDB extent绑定改变"));
        }
        Ok(())
    }
}

impl ActiveMediaReader<'_> {
    /// Small groups and sources that need an additional historical baseline
    /// location or marker upgrade retain the per-file read. Once physical
    /// observation has begun, every failure invalidates the whole group read.
    pub(in crate::daemon) fn read_reclaim_source_group<'a>(
        &'a self,
        parent: &'a ReclaimReservation,
        group: &'a ReclaimCopyGroup,
        layout: &'a ReclaimLayout,
        inventory: &'a ReclaimSourceInventory<'a>,
        cache_dir: &'a Path,
        spool_dir: &'a Path,
    ) -> Pin<Box<impl Future<Output = Result<Option<Vec<GroupSourceRead>>>> + 'a>> {
        Box::pin(async move {
            if !parent.spec().group_only() || group.ordered_ordinals().count() < 3 {
                return Ok(None);
            }
            self.catalog
                .check_reclaim_group_source_selection(parent, self.key.round(), group, layout)
                .await
                .map_err(error)?;
            let expected_head = self.source_group_head(parent).await?;
            if expected_head.index.incremental {
                return Ok(None);
            }
            let files = group.files().collect::<Vec<_>>();
            let mut members = Vec::with_capacity(files.len());
            for ordinal in group.ordered_ordinals() {
                let file = files[usize::from(ordinal)];
                let path = format!("/{}{}", parent.spec().pool(), file.path()).into_bytes();
                let source = ReadTarget::ReclaimSource(parent, file, &path)
                    .resolve(self)
                    .await?
                    .ok_or_else(|| error("回收组源文件已丢失"))?;
                if source.is_retained()
                    || source.copy().media.is_some()
                    || !matches!(source.copy().content_digest, ContentDigest::Sha256(_))
                    || source.layout_binding().is_none()
                {
                    return Err(error("回收组源已失去原SHA256或FDB布局"));
                }
                if source.needs_marker_upgrade()
                    || source.baseline.as_ref().is_some_and(|identity| {
                        identity.head().incremental
                            || ActiveIndexIdentity::from(identity.head()) != expected_head.index
                    })
                {
                    return Ok(None);
                }
                members.push(MemberRead {
                    ordinal,
                    file,
                    path,
                    source,
                });
            }
            let authority = ReadAuthority::Reclaim(parent, parent.spec().volume());
            self.check_readable_for(authority).await?;
            let _activity = self.permission.begin_device_activity(self.key.round())?;
            let token = self.device.medium_change_token();
            let checked = Guard::new_bracketed_read(self.device, || {
                if self.device.medium_change_token() != token {
                    return Err(error("回收组读取期间源介质变化证明改变"));
                }
                futures_executor::block_on(
                    self.check_reclaim_read_lane(parent, expected_head.volume_id),
                )?;
                self.check_device_permission()?;
                crate::scsi::reservation::verify_holder_without_ua_retry(self.device, self.key)
            });
            let started = std::time::Instant::now();
            log::info!(
                "回收组源读取开始: 操作={} 组={} 源={} 驱动器={} 文件={}",
                parent.intent()["operation"],
                group.id(),
                parent.spec().barcode(),
                layout.source.drive_serial,
                members.len(),
            );
            let mut reader = ActiveMediaReader {
                device: &checked,
                ..*self
            };
            let label = Box::pin(reader.verify_source_group_endpoint(
                parent,
                inventory,
                expected_head,
                cache_dir,
                spool_dir,
                None,
            ))
            .await?;
            let mut pending = Vec::with_capacity(members.len());
            let mut index_bytes = self.storage.index_bytes;
            let reclaim = || reclaim_cache_space(cache_dir, self.storage);
            for member in &members {
                checked.verify_proof()?;
                member.require_source(self, parent).await?;
                self.require_source_group_head(parent, expected_head)
                    .await?;
                let binding = member
                    .source
                    .layout_binding()
                    .ok_or_else(|| error("回收组源缺少原FDB布局"))?;
                let temporary = retry_after_space_reclaim(
                    || {
                        download_file(
                            spool_dir,
                            binding.encoded_bytes(),
                            ReadStorageBudget {
                                download_bytes: index_bytes,
                                ..self.storage
                            },
                        )
                    },
                    reclaim,
                )?;
                let physical = Box::pin(member.source.prepare_body_plan(
                    self.catalog,
                    temporary,
                    index_bytes,
                ))
                .await?;
                index_bytes = index_bytes
                    .checked_sub(binding.encoded_bytes())
                    .ok_or_else(|| error("回收组FDB布局超出暂存预算"))?;
                let metadata = member.source.metadata();
                if physical.volume() != expected_head.volume_id
                    || physical.uid() != metadata.copy.ltfs_file_uid
                    || physical.length() != member.file.size()
                    || physical.block_size() != label.blocksize
                    || physical.partitions() != [label.index_partition, label.data_partition]
                {
                    return Err(error("回收组extent与源卷、副本或介质几何不同"));
                }
                let candidate = inventory
                    .reclaim_group_candidate(parent, member.file, &metadata, &physical)
                    .map_err(error)?
                    .ok_or_else(|| error("回收组extent已失去原物理顺序能力"))?;
                group
                    .require_source_candidate(member.ordinal, &candidate)
                    .map_err(error)?;
                let expected_md5 = inventory
                    .expected_read_md5(metadata.copy.ltfs_file_uid)
                    .map_err(error)?;
                let ContentDigest::Sha256(expected_sha) = metadata.copy.content_digest else {
                    return Err(error("回收组源正文缺少原SHA256"));
                };
                let spool = retry_after_space_reclaim(
                    || download_file(spool_dir, physical.length(), self.storage),
                    reclaim,
                )?;
                let (spool, sha256, md5) = read_member(
                    &checked,
                    &label,
                    &physical,
                    expected_sha,
                    expected_md5,
                    spool,
                )?;
                checked.verify_proof()?;
                member.require_source(self, parent).await?;
                self.require_source_group_head(parent, expected_head)
                    .await?;
                pending.push(PendingRead {
                    physical,
                    spool,
                    sha256,
                    md5,
                });
            }
            reader.storage.index_bytes = index_bytes;
            Box::pin(reader.verify_source_group_endpoint(
                parent,
                inventory,
                expected_head,
                cache_dir,
                spool_dir,
                Some(&label),
            ))
            .await?;
            let final_epoch = checked.session_epoch();
            for member in &members {
                member.require_source(self, parent).await?;
            }
            self.catalog
                .check_reclaim_group_source_selection(parent, self.key.round(), group, layout)
                .await
                .map_err(error)?;
            self.require_source_group_head(parent, expected_head)
                .await?;
            checked.verify_proof()?;
            if checked.session_epoch() != final_epoch {
                return Err(error("回收组末次FDB核验期间源驱动状态改变"));
            }
            let mut output = Vec::with_capacity(pending.len());
            for (member, read) in members.iter().zip(pending) {
                let proof = inventory
                    .bind_verified_read_plan(
                        parent,
                        member.file,
                        &member.source.metadata(),
                        &read.physical,
                        expected_head.index,
                        read.sha256,
                        read.md5,
                    )
                    .map_err(error)?;
                output.push((member.ordinal, read.spool, proof));
            }
            checked.verify_proof()?;
            if checked.session_epoch() != final_epoch {
                return Err(error("回收组封存读取证明时源驱动状态改变"));
            }
            log::info!(
                "回收组源读取完成: 操作={} 组={} 源={} 驱动器={} 文件={} 双端核验耗时={:.3}s",
                parent.intent()["operation"],
                group.id(),
                parent.spec().barcode(),
                layout.source.drive_serial,
                output.len(),
                started.elapsed().as_secs_f64(),
            );
            Ok(Some(output))
        })
    }

    async fn source_group_head(
        &self,
        parent: &ReclaimReservation,
    ) -> Result<ActiveLoadedVolumeHead> {
        let spec = parent.spec();
        let head = self
            .catalog
            .active_loaded_volume_head(self.serial, spec.barcode(), self.route, self.key.round())
            .await
            .map_err(error)?
            .ok_or_else(|| error("回收组未找到原装载卷"))?;
        if head.volume_id != spec.volume()
            || head.pool_id != spec.pool()
            || head.policy_revision != spec.revision()
            || !head.online
            || head.blacklisted
        {
            return Err(TapeError::MediaUnavailable {
                barcode: spec.barcode().into(),
            });
        }
        Ok(head)
    }

    async fn require_source_group_head(
        &self,
        parent: &ReclaimReservation,
        expected: ActiveLoadedVolumeHead,
    ) -> Result<()> {
        if self.source_group_head(parent).await? != expected {
            return Err(error("回收组源卷头或介质策略已改变"));
        }
        Ok(())
    }

    async fn verify_source_group_endpoint(
        &self,
        parent: &ReclaimReservation,
        inventory: &ReclaimSourceInventory<'_>,
        expected: ActiveLoadedVolumeHead,
        cache_dir: &Path,
        scratch: &Path,
        prior_label: Option<&LtfsLabel>,
    ) -> Result<LtfsLabel> {
        // Each endpoint proves the actual current Full DP and IP, and its
        // verified DP end. The driver's complete inventory remains the
        // authority for historical source coverage and later source retirement.
        let (label, head, observed) = self
            .verify_media_head_with_prior_label(
                parent.spec().barcode(),
                expected.volume_id,
                scratch,
                ReadAuthority::Reclaim(parent, expected.volume_id),
                || reclaim_cache_space(cache_dir, self.storage),
                prior_label,
            )
            .await?;
        if head != expected {
            return Err(error("回收组端点物理索引与原FDB卷头不同"));
        }
        inventory
            .require_group_read_endpoint(head.index, observed.full_dp_eod, observed.ip_generation)
            .map_err(error)?;
        Ok(label)
    }
}

fn read_member(
    device: &dyn TapeTransport,
    label: &LtfsLabel,
    physical: &VerifiedPhysicalPlan,
    expected_sha: [u8; 32],
    expected_md5: Option<[u8; 16]>,
    mut spool: File,
) -> Result<(File, [u8; 32], Option<[u8; 16]>)> {
    let mut writer = StreamHash {
        out: &mut spool,
        remaining: physical.length(),
        sha: Sha256::new(),
        md5: expected_md5.map(|_| Md5::new()),
    };
    let bytes = read_extent_plan_sequential_to_writer(
        device,
        label,
        physical.path(),
        physical.length(),
        physical.extents(),
        &mut writer,
    )?;
    let sha256 = writer.sha.finalize().into();
    let md5 = writer.md5.map(|hash| hash.finalize().into());
    if writer.remaining != 0
        || bytes != physical.length()
        || sha256 != expected_sha
        || md5 != expected_md5
    {
        return Err(error("回收组成员源实读长度、SHA256或原MD5不符"));
    }
    spool.rewind()?;
    Ok((spool, sha256, md5))
}
