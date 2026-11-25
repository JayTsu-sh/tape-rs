//! Owner-fenced managed reads through sealed physical plans. The anonymous
//! body escapes only after medium, digest and final catalog checks succeed.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;

use md5::Md5;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::{
    RecoveryBudget, capture_complete_index_source, capture_recorded_construct_source,
    capture_recorded_index_source, verify_dp_ip_head_with_evidence,
};
use crate::ltfs::volume::{
    CommittedIndexConstruct, read_cached_file_to_writer, read_extent_plan_to_writer,
    read_label_only,
};
use crate::managed_catalog::{
    ActiveFileRequest, ActiveIndexIdentity, ActiveLibraryRoute, ActiveLoadedVolumeHead,
    ActiveReadSource, ContentDigest, ManagedCatalog, ReadMetadata, ReclaimFile,
    ReclaimFileMetadata, ReclaimNodeMetadata, ReclaimNodeReadRequest, ReclaimReservation,
    ReclaimSourceInventory, ReclaimSourceRead, VerifiedExistingCopyLayout, VerifiedPhysicalPlan,
};
use crate::scsi::reservation::{ReservationKey, verify_holder};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

use super::permission::{Permission, Phase};
pub use crate::local_read_storage::ReadStorageBudget;
use crate::local_read_storage::{download_file, reclaim_cache_space, retry_after_space_reclaim};

#[path = "fdb_read_session.rs"]
mod session;
pub(crate) use session::ReadSession;
#[path = "fdb_reclaim_raw_read.rs"]
mod raw_reclaim;
#[path = "fdb_reclaim_source_session.rs"]
mod reclaim_session;
pub(crate) use reclaim_session::ReclaimSourceSession;

#[path = "fdb_reclaim_group_read.rs"]
mod group_read;
pub(crate) use group_read::ReclaimGroupMemberReadback;
#[path = "fdb_reclaim_source_group_read.rs"]
mod source_group_read;

pub struct ActiveMediaRead {
    pub metadata: ReadMetadata,
    pub physical: VerifiedPhysicalPlan,
    /// Exact current medium head proved by this read. The file's historical
    /// source may predate it and must not be substituted as a health proof.
    pub verified_head: crate::managed_catalog::ActiveIndexIdentity,
    pub file: File,
}

/// The executor must serialize access to this drive for the entire call.
/// This reader neither loads media nor changes FDB or on-tape metadata.
pub struct ActiveMediaReader<'a> {
    pub catalog: &'a ManagedCatalog,
    pub device: &'a dyn TapeTransport,
    pub serial: &'a str,
    pub route: ActiveLibraryRoute,
    pub key: ReservationKey,
    pub permission: &'a Permission,
    pub storage: ReadStorageBudget,
}

#[derive(Clone, Copy)]
enum ReadTarget<'a> {
    Published(&'a [u8]),
    ReclaimSource(&'a ReclaimReservation, &'a ReclaimFile, &'a [u8]),
    ReclaimDestination(&'a ReclaimReservation, &'a [u8]),
    Reclaim(&'a ReclaimReservation, &'a ActiveFileRequest),
}
#[derive(Clone, Copy)]
enum ReadAuthority<'a> {
    Published,
    Reclaim(&'a ReclaimReservation, Uuid),
}

struct VerifiedMediaBounds {
    full_dp_eod: Option<u64>,
    ip_generation: Option<u64>,
}
impl<'a> ReadTarget<'a> {
    fn authority(self, volume: Uuid) -> ReadAuthority<'a> {
        match self {
            Self::Published(_) => ReadAuthority::Published,
            Self::ReclaimSource(parent, _, _)
            | Self::ReclaimDestination(parent, _)
            | Self::Reclaim(parent, _) => ReadAuthority::Reclaim(parent, volume),
        }
    }
    async fn resolve(self, reader: &ActiveMediaReader<'_>) -> Result<Option<ActiveReadSource>> {
        let result = match self {
            Self::Published(path)
            | Self::ReclaimSource(_, _, path)
            | Self::ReclaimDestination(_, path) => {
                if matches!(self, Self::Published(_)) {
                    reader.check_readable().await?;
                }
                reader
                    .catalog
                    .resolve_active_read_source_for_owner(
                        reader.serial,
                        path,
                        reader.route,
                        reader.key.round(),
                    )
                    .await
            }
            Self::Reclaim(reservation, request) => reader
                .catalog
                .reclaim_read_source(reservation, reader.key.round(), request)
                .await
                .map(|read| Some(read.into_source())),
        };
        let result = result.map_err(|error| TapeError::Catalog(error.to_string()))?;
        if let Self::ReclaimSource(parent, file, _) = self
            && let Some(source) = &result
            && (source.copy().volume_id != parent.spec().volume()
                || source.copy().version != file.version()
                || source.copy().size != file.size())
        {
            return Err(TapeError::Catalog("回收实读已不是原当前源副本".into()));
        }
        Ok(result)
    }
}

impl ActiveMediaReader<'_> {
    /// Read the current source before sealing its copy request. In particular,
    /// an EE file without a recorded digest must acquire its SHA from bytes.
    pub async fn read_reclaim_file_with_cache(
        &self,
        reservation: &ReclaimReservation,
        file: &ReclaimFile,
        inventory: &ReclaimSourceInventory<'_>,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<(ActiveMediaRead, ReclaimSourceRead)> {
        if reservation.intent()["library"] != self.serial
            || reservation.intent()["namespace"] != self.route.route.namespace.to_string()
        {
            return Err(TapeError::Catalog("源读取不属于当前带库或命名空间".into()));
        }
        self.catalog
            .reserved_reclaim_status(reservation, self.key.round())
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let spec = reservation.spec();
        let path = format!("/{}{}", spec.pool(), file.path());
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = crate::daemon::executor::fdb_operation_guard::Guard::new(self.device, || {
            futures_executor::block_on(
                self.check_readable_for(ReadAuthority::Reclaim(reservation, spec.volume())),
            )
        });
        let reader = ActiveMediaReader {
            device: &checked,
            ..*self
        };
        let mut read = reader
            .read_target_with_cache(
                ReadTarget::ReclaimSource(reservation, file, path.as_bytes()),
                Some(spec.pool()),
                cache_dir,
                spool_dir,
            )
            .await?
            .ok_or_else(|| TapeError::Catalog("回收源文件不存在".into()))?;
        // Hash the exact anonymous spool returned by the checked reader, never
        // a caller-supplied pathname. Rewind before returning it to the writer.
        read.file.rewind()?;
        let mut sha = Sha256::new();
        let expected_md5 = inventory
            .expected_read_md5(read.metadata.copy.ltfs_file_uid)
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let mut md5 = expected_md5.map(|_| Md5::new());
        let mut buffer = vec![0; 1024 * 1024];
        let mut remaining = read.metadata.copy.size;
        loop {
            let count = read.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            remaining = remaining
                .checked_sub(count as u64)
                .ok_or_else(|| TapeError::Catalog("源暂存正文超过已核验长度".into()))?;
            sha.update(&buffer[..count]);
            if let Some(hash) = md5.as_mut() {
                hash.update(&buffer[..count]);
            }
        }
        if remaining != 0 {
            return Err(TapeError::Catalog("源暂存正文短于已核验长度".into()));
        }
        let md5 = md5.map(|hash| <[u8; 16]>::from(hash.finalize()));
        if md5 != expected_md5 {
            return Err(TapeError::Catalog(
                "回收源实读 MD5 与原始数据节点不一致".into(),
            ));
        }
        read.file.rewind()?;
        checked.verify_proof()?;
        self.check_device_owner()?;
        self.catalog
            .reserved_reclaim_status(reservation, self.key.round())
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let evidence = inventory
            .bind_verified_read_plan(
                reservation,
                file,
                &read.metadata,
                &read.physical,
                read.verified_head,
                sha.finalize().into(),
                md5,
            )
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        Ok((read, evidence))
    }

    /// Read the destination bytes and its current raw metadata under the same
    /// serialized drive lease. Failed capture/ownership checks yield no proof.
    pub async fn read_reclaim_metadata_with_cache(
        &self,
        reservation: &ReclaimReservation,
        request: &ActiveFileRequest,
        inventory: &ReclaimSourceInventory<'_>,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<(ActiveMediaRead, ReclaimFileMetadata)> {
        if request.library_serial != self.serial
            || request.namespace != self.route.route.namespace
            || reservation.intent()["library"] != self.serial
            || reservation.intent()["namespace"] != request.namespace.to_string()
        {
            return Err(TapeError::Catalog(
                "元数据读取不属于当前带库或命名空间".into(),
            ));
        }
        let source = self
            .catalog
            .reclaim_read_source(reservation, self.key.round(), request)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let pool = request
            .pool_id
            .ok_or_else(|| TapeError::Catalog("回收元数据缺少原池".into()))?;
        let path = crate::managed_catalog::pool_catalog_path(
            pool,
            request
                .ltfs_path()
                .map_err(|e| TapeError::Catalog(e.to_string()))?
                .as_bytes(),
        )
        .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let current = self
            .catalog
            .resolve_active_read_source_for_owner(self.serial, &path, self.route, self.key.round())
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?
            .ok_or_else(|| TapeError::Catalog("回收目的文件不存在".into()))?;
        let authority = ReadAuthority::Reclaim(reservation, current.copy().volume_id);
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = crate::daemon::executor::fdb_operation_guard::Guard::new(self.device, || {
            futures_executor::block_on(self.check_readable_for(authority))
        });
        let reader = ActiveMediaReader {
            device: &checked,
            ..*self
        };
        let read = reader
            .read_target_with_cache(
                ReadTarget::ReclaimDestination(reservation, &path),
                Some(pool),
                cache_dir,
                spool_dir,
            )
            .await?
            .ok_or_else(|| TapeError::Catalog("回收目的文件不存在".into()))?;
        self.check_device_owner()?;
        let label = read_label_only(reader.device)?;
        let mut budget = RecoveryBudget::default();
        budget.max_index_bytes = budget
            .max_index_bytes
            .min(usize::try_from(self.storage.index_bytes).unwrap_or(usize::MAX));
        budget.index_scratch.directory = spool_dir.to_path_buf();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let captured = retry_after_space_reclaim(
            || capture_complete_index_source(&TapeDrive::new(reader.device), &label, &budget),
            || reclaim_cache_space(cache_dir, self.storage),
        )?;
        let (_, head, _) = reader
            .verify_media_head(
                &read.metadata.placement.barcode,
                read.metadata.copy.volume_id,
                spool_dir,
                authority,
                || reclaim_cache_space(cache_dir, self.storage),
            )
            .await?;
        if head.index != read.verified_head {
            return Err(TapeError::Catalog("目的索引在元数据读取期间改变".into()));
        }
        let final_source = self
            .catalog
            .reclaim_read_source(reservation, self.key.round(), request)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        if final_source != source {
            return Err(TapeError::Catalog("原回收来源在元数据读取期间改变".into()));
        }
        self.check_device_owner()?;
        let evidence = inventory
            .bind_verified_metadata_plan(
                reservation,
                request,
                source.source().copy(),
                (&read.metadata, &read.physical, read.verified_head),
                &captured,
            )
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        checked.verify_proof()?;
        Ok((read, evidence))
    }

    /// Observe 1..32 nodes using one actual capture of an already loaded tape.
    /// The whole batch validates before any evidence is returned. No data body,
    /// media loading, metadata change or erase authority is produced here.
    pub async fn read_reclaim_node_metadata(
        &self,
        reservation: &ReclaimReservation,
        inventory: &ReclaimSourceInventory<'_>,
        requests: &[ReclaimNodeReadRequest],
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Vec<ReclaimNodeMetadata>> {
        if !(1..=32).contains(&requests.len()) {
            return Err(TapeError::Catalog(
                "原始节点实读批次必须为 1..=32 项".into(),
            ));
        }
        let volume = requests[0].destination.volume;
        let mut targets = std::collections::HashSet::new();
        if reservation.intent()["library"] != self.serial
            || reservation.intent()["namespace"] != self.route.route.namespace.to_string()
            || volume == reservation.spec().volume()
            || volume.is_nil()
            || requests.iter().any(|request| {
                request.destination.volume != volume
                    || request.destination.uid == 0
                    || !targets.insert(request.destination.uid)
            })
        {
            return Err(TapeError::Catalog(
                "原始节点批次作用域、目的或重复 UID 无效".into(),
            ));
        }
        let uids: Vec<_> = requests
            .iter()
            .map(|request| request.original.uid())
            .collect();
        let before_source = self
            .catalog
            .reclaim_source_nodes(reservation, self.key.round(), inventory, &uids)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        if before_source
            .iter()
            .zip(requests)
            .any(|(source, request)| source != &request.original)
        {
            return Err(TapeError::Catalog(
                "原始节点批次不属于本任务完整封存清单".into(),
            ));
        }
        let placement = self
            .catalog
            .volume_placement(volume)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?
            .ok_or_else(|| TapeError::Catalog("原始节点目的磁带未分配".into()))?;
        if placement.namespace != self.route.route.namespace
            || placement.library_serial != self.serial
            || placement.pool_id != reservation.spec().pool()
        {
            return Err(TapeError::Catalog("原始节点目的磁带不属于原回收池".into()));
        }
        let authority = ReadAuthority::Reclaim(reservation, volume);
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = crate::daemon::executor::fdb_operation_guard::Guard::new(self.device, || {
            futures_executor::block_on(self.check_readable_for(authority))
        });
        let reader = ActiveMediaReader {
            device: &checked,
            ..*self
        };
        self.check_device_owner()?;
        let (label, before, _) = reader
            .verify_media_head(&placement.barcode, volume, spool_dir, authority, || {
                reclaim_cache_space(cache_dir, self.storage)
            })
            .await?;
        let mut budget = RecoveryBudget::default();
        budget.max_index_bytes = budget
            .max_index_bytes
            .min(usize::try_from(self.storage.index_bytes).unwrap_or(usize::MAX));
        budget.index_scratch.directory = spool_dir.to_path_buf();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let captured = retry_after_space_reclaim(
            || capture_complete_index_source(&TapeDrive::new(reader.device), &label, &budget),
            || reclaim_cache_space(cache_dir, self.storage),
        )?;
        let (_, after, _) = reader
            .verify_media_head(&placement.barcode, volume, spool_dir, authority, || {
                reclaim_cache_space(cache_dir, self.storage)
            })
            .await?;
        let after_placement = self
            .catalog
            .volume_placement(volume)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let after_source = self
            .catalog
            .reclaim_source_nodes(reservation, self.key.round(), inventory, &uids)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        self.check_readable_for(authority).await?;
        self.check_device_owner()?;
        if before != after
            || after_placement.as_ref() != Some(&placement)
            || after_source != before_source
        {
            return Err(TapeError::Catalog(
                "原始节点实读期间来源、目的归属或索引改变".into(),
            ));
        }
        let mut observations = Vec::with_capacity(requests.len());
        for request in requests {
            observations.push(
                inventory
                    .bind_verified_node_metadata(
                        reservation,
                        &request.original,
                        request.destination,
                        &placement,
                        after.index,
                        &captured,
                    )
                    .map_err(|e| TapeError::Catalog(e.to_string()))?,
            );
        }
        self.check_readable_for(authority).await?;
        self.check_device_owner()?;
        checked.verify_proof()?;
        Ok(observations)
    }

    fn check_device_permission(&self) -> Result<()> {
        let round = self.key.round();
        if !self.key.is_ours()
            || !(self.permission.allows(round, Phase::Execution)
                || self.permission.allows(round, Phase::Serving))
            || self.device.execution_guard()
                != Some((round, self.permission as *const Permission as usize))
        {
            return Err(TapeError::NotReady(
                "FDB 读取缺少当前轮次的多数派许可设备守护".into(),
            ));
        }
        Ok(())
    }

    fn check_device_owner(&self) -> Result<()> {
        self.check_device_permission()?;
        verify_holder(self.device, self.key)
    }

    async fn check_readable(&self) -> Result<()> {
        self.catalog
            .require_active_library_readable(self.serial, self.route, self.key.round())
            .await
            .map_err(|error| {
                if matches!(
                    error.domain(),
                    crate::managed_catalog::CatalogError::PoolBusy
                ) {
                    TapeError::MaintenanceBusy {
                        library: self.serial.into(),
                    }
                } else {
                    TapeError::Catalog(error.to_string())
                }
            })?;
        self.check_device_owner()
    }

    async fn check_readable_for(&self, authority: ReadAuthority<'_>) -> Result<()> {
        match authority {
            ReadAuthority::Published => self.check_readable().await,
            ReadAuthority::Reclaim(parent, volume) => {
                self.check_reclaim_read_lane(parent, volume).await?;
                self.check_device_owner()
            }
        }
    }

    async fn check_reclaim_read_lane(
        &self,
        parent: &ReclaimReservation,
        volume: Uuid,
    ) -> Result<()> {
        if parent.namespace() != self.route.route.namespace || parent.serial() != self.serial {
            return Err(TapeError::Catalog("回收读取授权不属于当前带库".into()));
        }
        self.catalog
            .require_reclaim_read_lane(parent, self.key.round(), volume)
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))
    }

    async fn verify_media_head(
        &self,
        barcode: &str,
        volume: Uuid,
        spool_dir: &Path,
        authority: ReadAuthority<'_>,
        reclaim: impl FnOnce() -> Result<bool>,
    ) -> Result<(LtfsLabel, ActiveLoadedVolumeHead, VerifiedMediaBounds)> {
        self.verify_media_head_with_prior_label(
            barcode, volume, spool_dir, authority, reclaim, None,
        )
        .await
    }

    async fn verify_media_head_with_prior_label(
        &self,
        barcode: &str,
        volume: Uuid,
        spool_dir: &Path,
        authority: ReadAuthority<'_>,
        reclaim: impl FnOnce() -> Result<bool>,
        prior_label: Option<&LtfsLabel>,
    ) -> Result<(LtfsLabel, ActiveLoadedVolumeHead, VerifiedMediaBounds)> {
        let head = self
            .catalog
            .active_loaded_volume_head(self.serial, barcode, self.route, self.key.round())
            .await
            .map_err(|error| TapeError::Catalog(error.to_string()))?
            .ok_or_else(|| TapeError::Catalog("FDB 读取卷未绑定当前带库".into()))?;
        self.check_readable_for(authority).await?;
        if !head.online || head.blacklisted {
            return Err(TapeError::MediaUnavailable {
                barcode: barcode.into(),
            });
        }
        let label = match prior_label {
            Some(label) => label.clone(),
            None => read_label_only(self.device)?,
        };
        if label.volume_uuid != head.volume_id || label.volume_uuid != volume {
            return Err(TapeError::Catalog(
                "FDB 读取介质标签与文件副本不一致".into(),
            ));
        }
        let drive = TapeDrive::new(self.device);
        let mut budget = RecoveryBudget::default();
        budget.index_scratch.directory = spool_dir.to_path_buf();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let proof = retry_after_space_reclaim(
            || {
                verify_dp_ip_head_with_evidence(
                    &drive,
                    &label,
                    CommittedIndexConstruct {
                        volume_uuid: head.index.volume_uuid,
                        generation: head.index.generation,
                        location: head.index.location,
                        sha256: head.index.sha256,
                        incremental: head.index.incremental,
                    },
                    &budget,
                )
            },
            reclaim,
        )?;
        let label = if prior_label.is_some() {
            // The source group already has a bracketed label from its first
            // endpoint. Finish DP and IP reads before rereading it on P0.
            let current = read_label_only(self.device)?;
            if current.volume_uuid != label.volume_uuid {
                return Err(TapeError::Catalog(
                    "FDB 读取介质标签与文件副本不一致".into(),
                ));
            }
            if current.to_xml()? != label.to_xml()? {
                return Err(TapeError::Catalog("回收组首尾实际源卷标签不同".into()));
            }
            current
        } else {
            label
        };
        self.catalog
            .verify_active_index_predecessors(
                self.serial,
                self.route,
                self.key.round(),
                head.index,
                &proof
                    .predecessors
                    .into_iter()
                    .map(ActiveIndexIdentity::from)
                    .collect::<Vec<_>>(),
            )
            .await
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        self.check_readable_for(authority).await?;
        Ok((
            label,
            head,
            VerifiedMediaBounds {
                full_dp_eod: proof.full_dp_eod,
                ip_generation: proof.ip_generation,
            },
        ))
    }

    /// Read through the FDB layout or freshly capture its original source.
    /// The compatibility cache argument is never used to serve metadata.
    pub async fn read_with_cache(
        &self,
        path: &[u8],
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        self.read_with_cache_scoped(path, None, cache_dir, spool_dir)
            .await
    }

    pub(crate) async fn read_with_cache_scoped(
        &self,
        path: &[u8],
        pool: Option<Uuid>,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        self.read_target_with_cache(ReadTarget::Published(path), pool, cache_dir, spool_dir)
            .await
    }

    pub async fn read_reclaim_with_cache(
        &self,
        reservation: &ReclaimReservation,
        request: &ActiveFileRequest,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = crate::daemon::executor::fdb_operation_guard::Guard::new(self.device, || {
            futures_executor::block_on(self.check_readable_for(ReadAuthority::Reclaim(
                reservation,
                reservation.spec().volume(),
            )))
        });
        let reader = ActiveMediaReader {
            device: &checked,
            ..*self
        };
        let result = reader
            .read_target_with_cache(
                ReadTarget::Reclaim(reservation, request),
                request.pool_id,
                cache_dir,
                spool_dir,
            )
            .await?;
        checked.verify_proof()?;
        Ok(result)
    }

    async fn read_target_with_cache(
        &self,
        target: ReadTarget<'_>,
        pool: Option<Uuid>,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        // The cache directory is retained in the public API for compatible
        // storage reclamation, never as a source of managed business metadata.
        self.read_scoped(target, pool, cache_dir, spool_dir, Some(cache_dir))
            .await
    }

    /// Read one published file into an unlinked 0600 temporary file. No bytes
    /// escape before checksum and final route/epoch/copy/head checks pass.
    /// A missing layout requires fresh verification of the original source.
    pub async fn read(
        &self,
        path: &[u8],
        cache_path: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        self.read_scoped(
            ReadTarget::Published(path),
            None,
            cache_path,
            spool_dir,
            None,
        )
        .await
    }

    async fn read_scoped(
        &self,
        target: ReadTarget<'_>,
        pool: Option<Uuid>,
        _cache_path: &Path,
        spool_dir: &Path,
        managed_cache: Option<&Path>,
    ) -> Result<Option<ActiveMediaRead>> {
        if !matches!(target, ReadTarget::Published(_)) {
            self.check_device_owner()?;
        }
        let Some(mut source) = target.resolve(self).await? else {
            return Ok(None);
        };
        if (source.layout_binding().is_none() || source.needs_marker_upgrade())
            && !source.has_existing_snapshot()
        {
            self.catalog
                .materialize_legacy_layout_source(
                    self.serial,
                    self.route,
                    self.key.round(),
                    &source,
                )
                .await
                .map_err(|e| TapeError::Catalog(e.to_string()))?;
            let refreshed = target
                .resolve(self)
                .await?
                .ok_or_else(|| TapeError::Catalog("原读取副本在归并后消失".into()))?;
            if !source.same_selection(&refreshed) || !refreshed.has_existing_snapshot() {
                return Err(TapeError::Catalog(
                    "原读取副本在归并期间改变或缺少真实 R".into(),
                ));
            }
            source = refreshed;
        }
        let metadata = source.metadata();
        require_pool(pool, metadata.placement.pool_id)?;
        let authority = target.authority(metadata.copy.volume_id);
        let reclaim = || match managed_cache {
            Some(directory) => reclaim_cache_space(directory, self.storage),
            None => Ok(false),
        };
        let file = retry_after_space_reclaim(
            || download_file(spool_dir, metadata.copy.size, self.storage),
            reclaim,
        )?;
        let (label, head, _) = self
            .verify_media_head(
                &metadata.placement.barcode,
                source.source.volume_uuid,
                spool_dir,
                authority,
                reclaim,
            )
            .await?;

        if source.layout_binding().is_none() || source.needs_marker_upgrade() {
            let mut budget = RecoveryBudget::default();
            budget.index_scratch.directory = spool_dir.to_path_buf();
            budget.index_scratch.max_bytes = self.storage.index_bytes;
            budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
            let captured = if let Some(identity) = &source.baseline {
                capture_recorded_index_source(
                    &TapeDrive::new(self.device),
                    &label,
                    identity,
                    &budget,
                )?
            } else {
                capture_recorded_construct_source(
                    &TapeDrive::new(self.device),
                    &label,
                    CommittedIndexConstruct {
                        volume_uuid: source.source.volume_uuid,
                        generation: source.source.generation,
                        location: source.source.location,
                        sha256: source.source.sha256,
                        incremental: source.source.incremental,
                    },
                    &budget,
                )?
            };
            self.check_device_owner()?;
            let proof = if source.is_retained() {
                VerifiedExistingCopyLayout::from_retained_captured(&source, &captured, &label)
            } else {
                VerifiedExistingCopyLayout::from_captured(&source, &captured, &label)
            }
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
            let attached = self
                .catalog
                .backfill_existing_body_layout(self.serial, self.route, self.key.round(), &proof)
                .await
                .map_err(|e| TapeError::Catalog(e.to_string()))?;
            self.check_device_owner()?;
            let refreshed = target
                .resolve(self)
                .await?
                .ok_or_else(|| TapeError::Catalog("回填后原读取副本消失".into()))?;
            if !source.same_selection(&refreshed) || refreshed.layout_binding() != Some(&attached) {
                return Err(TapeError::Catalog("回填后原读取副本或绑定改变".into()));
            }
            source = refreshed;
        }
        let binding = source
            .layout_binding()
            .ok_or_else(|| TapeError::Catalog("原副本缺少完整 FDB 正文布局".into()))?;
        let temporary = retry_after_space_reclaim(
            || {
                download_file(
                    spool_dir,
                    binding.encoded_bytes(),
                    ReadStorageBudget {
                        download_bytes: self.storage.index_bytes,
                        ..self.storage
                    },
                )
            },
            reclaim,
        )?;
        let physical = source
            .prepare_body_plan(self.catalog, temporary, self.storage.index_bytes)
            .await?;
        if physical.volume() != source.source.volume_uuid
            || physical.uid() != metadata.copy.ltfs_file_uid
            || physical.length() != metadata.copy.size
            || physical.block_size() != label.blocksize
            || physical.partitions() != [label.index_partition, label.data_partition]
        {
            return Err(TapeError::Catalog("正文布局与实际介质或副本不一致".into()));
        }
        if let Some(identity) = source.baseline.as_ref().filter(|identity| {
            identity.head().incremental || ActiveIndexIdentity::from(identity.head()) != head.index
        }) {
            crate::ltfs::recovery::verify_recorded_source_constructs(
                &TapeDrive::new(self.device),
                &label,
                identity,
                &RecoveryBudget::default(),
            )?;
            self.check_device_owner()?;
        }

        let mut writer = DigestingFile {
            file,
            remaining: metadata.copy.size,
            digest: match metadata.copy.content_digest {
                ContentDigest::Unknown => ReadDigest::Unknown,
                ContentDigest::Md5(_) => ReadDigest::Md5(Md5::new()),
                ContentDigest::Sha256(_) => ReadDigest::Sha256(Sha256::new()),
            },
        };
        read_extent_plan_to_writer(
            self.device,
            &label,
            physical.path(),
            physical.length(),
            physical.extents(),
            &mut writer,
        )?;
        if writer.remaining != 0 {
            return Err(TapeError::Catalog("FDB 文件读取长度不足".into()));
        }
        let actual = match writer.digest {
            ReadDigest::Unknown => ContentDigest::Unknown,
            ReadDigest::Md5(hash) => ContentDigest::Md5(hash.finalize().into()),
            ReadDigest::Sha256(hash) => ContentDigest::Sha256(hash.finalize().into()),
        };
        if actual != metadata.copy.content_digest {
            return Err(TapeError::Catalog("FDB 文件读取摘要不符".into()));
        }
        self.check_device_owner()?;
        let final_source = target.resolve(self).await?;
        let final_head = self
            .catalog
            .active_loaded_volume_head(
                self.serial,
                &metadata.placement.barcode,
                self.route,
                self.key.round(),
            )
            .await
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        if final_source.as_ref() != Some(&source) || final_head != Some(head) {
            return Err(TapeError::Catalog("FDB 文件或卷头在读取期间改变".into()));
        }
        self.check_readable_for(authority).await?;
        writer.file.rewind()?;
        Ok(Some(ActiveMediaRead {
            metadata,
            physical,
            verified_head: head.index,
            file: writer.file,
        }))
    }
}

fn require_pool(expected: Option<Uuid>, actual: Uuid) -> Result<()> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err(TapeError::Refused {
            reason: "FDB 文件不属于请求的池".into(),
        });
    }
    Ok(())
}

enum ReadDigest {
    Unknown,
    Md5(Md5),
    Sha256(Sha256),
}

struct DigestingFile {
    file: File,
    remaining: u64,
    digest: ReadDigest,
}

impl Write for DigestingFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "读取内容超过预留长度",
            ));
        }
        let count = self.file.write(bytes)?;
        self.remaining -= count as u64;
        match &mut self.digest {
            ReadDigest::Unknown => {}
            ReadDigest::Md5(hash) => hash.update(&bytes[..count]),
            ReadDigest::Sha256(hash) => hash.update(&bytes[..count]),
        }
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
