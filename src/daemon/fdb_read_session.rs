//! One bounded, non-transferable read proof per library execution instance.
//! The client supplies read order; actual extent positioning remains verified.
use super::*;
use crate::ltfs::mam::{Mam, vcr_is_valid, vcr_matches};
use crate::ltfs::recovery::{CapturedReadIndex, capture_current_read_index};

pub(crate) struct ReadSession {
    device: String,
    route: ActiveLibraryRoute,
    key: ReservationKey,
    guard: (u64, usize),
    epoch: u64,
    vcr: Vec<u8>,
    label: LtfsLabel,
    head: ActiveLoadedVolumeHead,
    captured: CapturedReadIndex,
}

impl ActiveMediaReader<'_> {
    /// Reuse only a live, unchanged physical head. Unsupported transports,
    /// imported aliases and historical/hashless bodies use the original path.
    /// Taking the old proof makes every error discard it rather than renew it.
    pub(crate) async fn read_with_session(
        &self,
        path: &[u8],
        pool: Option<Uuid>,
        cache_dir: &Path,
        spool_dir: &Path,
        saved: &mut Option<ReadSession>,
    ) -> Result<Option<ActiveMediaRead>> {
        let previous = saved.take();
        self.check_readable().await?;
        let Some(source) = ReadTarget::Published(path).resolve(self).await? else {
            return Ok(None);
        };
        require_pool(pool, source.placement().pool_id)?;
        if source.copy().media.is_some()
            || !matches!(source.copy().content_digest, ContentDigest::Sha256(_))
            || self.device.session_epoch().is_none()
        {
            drop(previous);
            return self
                .read_with_cache_scoped(path, pool, cache_dir, spool_dir)
                .await;
        }
        // Admission precedes label/index/body reads and no bytes escape this
        // anonymous file before the final data, catalog and device checks.
        let file = retry_after_space_reclaim(
            || download_file(spool_dir, source.copy().size, self.storage),
            || reclaim_cache_space(cache_dir, self.storage),
        )?;
        let head = self
            .catalog
            .active_loaded_volume_head(
                self.serial,
                &source.placement().barcode,
                self.route,
                self.key.round(),
            )
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?
            .ok_or_else(|| TapeError::Catalog("顺序读卷缺少当前归属".into()))?;
        if head.volume_id != source.source.volume_uuid
            || head.pool_id != source.placement().pool_id
            || head.index.generation < source.source.generation
            || (head.index.generation == source.source.generation && head.index != source.source)
        {
            return Err(TapeError::Catalog("顺序读来源与当前卷头不一致".into()));
        }
        if !head.online || head.blacklisted {
            return Err(TapeError::MediaUnavailable {
                barcode: source.placement().barcode.clone(),
            });
        }
        let epoch = self.device.session_epoch();
        let guard = self
            .device
            .execution_guard()
            .ok_or_else(|| TapeError::NotReady("顺序读缺少原执行守卫".into()))?;
        // Do not consume a reset UA and then accept an old proof. Cold reads
        // still use the established label/recovery path below.
        if previous.as_ref().is_some_and(|s| {
            s.device == self.device.identity()
                && s.route == self.route
                && s.key == self.key
                && s.guard == guard
                && Some(s.epoch) == epoch
                && s.head == head
        }) {
            self.device
                .execute_no_data(&crate::scsi::cdb::test_unit_ready(), 10_000)?;
        }
        self.check_device_owner()?;
        let vcr = Mam::with_partition(self.device, 0).read_vcr()?;
        self.check_readable().await?;
        let Some(vcr) = vcr.filter(|v| vcr_is_valid(v)) else {
            drop(previous);
            drop(file);
            return self
                .read_with_cache_scoped(path, pool, cache_dir, spool_dir)
                .await;
        };
        let reusable = previous.filter(|s| {
            s.device == self.device.identity()
                && s.route == self.route
                && s.key == self.key
                && s.guard == guard
                && Some(s.epoch) == epoch
                && self.device.session_epoch() == epoch
                && s.head == head
                && vcr_matches(&s.vcr, &vcr)
        });
        let mut session = match reusable {
            Some(session) => session,
            None => {
                let label = read_label_only(self.device)?;
                if label.volume_uuid != head.volume_id {
                    return Err(TapeError::Catalog("顺序读介质卷 UUID 不符".into()));
                }
                let mut budget = RecoveryBudget::default();
                budget.index_scratch.directory = spool_dir.to_path_buf();
                budget.index_scratch.max_bytes = self.storage.index_bytes;
                budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
                let captured = retry_after_space_reclaim(
                    || capture_current_read_index(&TapeDrive::new(self.device), &label, &budget),
                    || reclaim_cache_space(cache_dir, self.storage),
                )?;
                if ActiveIndexIdentity::from(captured.construct()) != head.index {
                    return Err(TapeError::Catalog(
                        "顺序读实测索引与 FDB 当前卷头不符".into(),
                    ));
                }
                self.catalog
                    .verify_active_index_predecessors(
                        self.serial,
                        self.route,
                        self.key.round(),
                        head.index,
                        &captured
                            .predecessors()
                            .iter()
                            .copied()
                            .map(ActiveIndexIdentity::from)
                            .collect::<Vec<_>>(),
                    )
                    .await
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                self.check_readable().await?;
                ReadSession {
                    device: self.device.identity().into(),
                    route: self.route,
                    key: self.key,
                    guard,
                    epoch: 0,
                    vcr: vcr.to_vec(),
                    label,
                    head,
                    captured,
                }
            }
        };
        let plan = session
            .captured
            .entry_by_uid(source.copy().ltfs_file_uid)?
            .and_then(|entry| source.plan_current_body(entry));
        let Some(plan) = plan else {
            // A retained historical UID may no longer occur in the current
            // tree, or may describe a different version. Preserve its exact
            // original construct; never redirect it merely by name or size.
            drop(session);
            drop(file);
            return self
                .read_with_cache_scoped(path, pool, cache_dir, spool_dir)
                .await;
        };
        let tracked =
            crate::daemon::executor::fdb_monitor::Tracked::new(self.device, "sequential_read");
        let body_reader = ActiveMediaReader {
            catalog: self.catalog,
            device: &tracked,
            serial: self.serial,
            route: self.route,
            key: self.key,
            permission: self.permission,
            storage: self.storage,
        };
        let checked = crate::daemon::executor::fdb_operation_guard::Guard::new(&tracked, || {
            let reject_error = || {
                if let Some(error) = tracked.error() {
                    Err(TapeError::NotReady(format!(
                        "顺序读遇到介质或设备异常，须重新核验: {}",
                        error.message
                    )))
                } else {
                    Ok(())
                }
            };
            reject_error()?;
            futures_executor::block_on(body_reader.check_readable())?;
            reject_error()
        });
        let mut writer = DigestingFile {
            file,
            remaining: plan.copy.size,
            digest: ReadDigest::Sha256(Sha256::new()),
        };
        crate::ltfs::volume::read_cached_file_sequential_to_writer(
            &checked,
            &session.label,
            &plan.physical,
            &mut writer,
        )?;
        let ReadDigest::Sha256(hash) = writer.digest else {
            unreachable!("SHA-only sequential body proof")
        };
        if writer.remaining != 0
            || ContentDigest::Sha256(hash.finalize().into()) != plan.copy.content_digest
        {
            return Err(TapeError::Catalog("顺序读正文长度或 SHA-256 不符".into()));
        }
        checked.verify_proof()?;
        let final_source = ReadTarget::Published(path).resolve(&body_reader).await?;
        let final_head = self
            .catalog
            .active_loaded_volume_head(
                self.serial,
                &source.placement().barcode,
                self.route,
                self.key.round(),
            )
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        if final_source.as_ref() != Some(&source) || final_head != Some(head) {
            return Err(TapeError::Catalog("顺序读期间原文件或卷头改变".into()));
        }
        let finished_epoch = self.device.session_epoch();
        let after_vcr = Mam::with_partition(&checked, 0).read_vcr()?;
        if !after_vcr
            .as_deref()
            .is_some_and(|v| vcr_is_valid(v) && vcr_matches(v, &vcr))
        {
            return Err(TapeError::Catalog(
                "顺序读期间介质 VCR 改变或不可确认".into(),
            ));
        }
        checked.verify_proof()?;
        if self.device.session_epoch() != finished_epoch {
            return Err(TapeError::NotReady("顺序读结清期间设备代次改变".into()));
        }
        // LOCATE and expected filemarks used by this serialized read may
        // advance the epoch; only this successfully checked read renews it.
        session.epoch =
            finished_epoch.ok_or_else(|| TapeError::NotReady("顺序读设备代次不可确认".into()))?;
        session.vcr = after_vcr.expect("checked VCR").to_vec();
        writer.file.rewind()?;
        *saved = Some(session);
        Ok(Some(ActiveMediaRead {
            metadata: plan,
            verified_head: head.index,
            file: writer.file,
        }))
    }
}
