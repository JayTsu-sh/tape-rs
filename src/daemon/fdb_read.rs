//! Owner-fenced reads from an exact local extent cache. No public HTTP route
//! uses this primitive yet; the returned anonymous file is published only
//! after the medium, file digest and final ownership checks succeed.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;

use md5::Md5;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::error::{Result, TapeError};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::{
    RecoveryBudget, capture_index_source, capture_recorded_index_source,
    verify_dp_ip_head_with_evidence,
};
use crate::ltfs::volume::{CommittedIndexConstruct, read_cached_file_to_writer, read_label_only};
use crate::managed_catalog::{
    ActiveFileRequest, ActiveIndexIdentity, ActiveLibraryRoute, ActiveLoadedVolumeHead,
    ActiveReadSource, ContentDigest, ManagedCatalog, ReclaimFile, ReclaimFileMetadata,
    ReclaimReservation, ReclaimSourceInventory, ReclaimSourceRead, ResolvedRead,
};
use crate::scsi::reservation::{ReservationKey, verify_holder};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

use super::permission::{Permission, Phase};
pub use crate::local_read_storage::ReadStorageBudget;
use crate::local_read_storage::{
    CacheBuild, CachePin, download_file, reclaim_cache_space, retry_after_space_reclaim,
};

pub struct ActiveMediaRead {
    pub metadata: ResolvedRead,
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
    Reclaim(&'a ReclaimReservation, &'a ActiveFileRequest),
}
impl ReadTarget<'_> {
    async fn resolve(self, reader: &ActiveMediaReader<'_>) -> Result<Option<ActiveReadSource>> {
        let result = match self {
            Self::Published(path) => {
                reader.check_readable().await?;
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
                .map(Some),
        };
        result.map_err(|error| TapeError::Catalog(error.to_string()))
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
        let mut read = self
            .read_with_cache_scoped(path.as_bytes(), Some(spec.pool()), cache_dir, spool_dir)
            .await?
            .ok_or_else(|| TapeError::Catalog("回收源文件不存在".into()))?;
        // Hash the exact anonymous spool returned by the checked reader, never
        // a caller-supplied pathname. Rewind before returning it to the writer.
        read.file.rewind()?;
        let mut sha = Sha256::new();
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
        }
        if remaining != 0 {
            return Err(TapeError::Catalog("源暂存正文短于已核验长度".into()));
        }
        read.file.rewind()?;
        self.check_device_owner()?;
        self.catalog
            .reserved_reclaim_status(reservation, self.key.round())
            .await
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        let evidence = inventory
            .bind_verified_read(
                reservation,
                file,
                &read.metadata,
                read.verified_head,
                sha.finalize().into(),
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
        let read = self
            .read_with_cache_scoped(&path, Some(pool), cache_dir, spool_dir)
            .await?
            .ok_or_else(|| TapeError::Catalog("回收目的文件不存在".into()))?;
        self.check_device_owner()?;
        let label = read_label_only(self.device)?;
        let mut budget = RecoveryBudget::default();
        budget.max_index_bytes = budget
            .max_index_bytes
            .min(usize::try_from(self.storage.index_bytes).unwrap_or(usize::MAX));
        budget.index_scratch.directory = spool_dir.to_path_buf();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let captured = retry_after_space_reclaim(
            || capture_index_source(&TapeDrive::new(self.device), &label, &budget),
            || reclaim_cache_space(cache_dir, self.storage),
        )?;
        let (_, head) = self
            .verify_media_head(
                &read.metadata.placement.barcode,
                read.metadata.copy.volume_id,
                spool_dir,
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
            .bind_verified_metadata(
                reservation,
                request,
                source.copy(),
                (&read.metadata, read.verified_head),
                &captured,
            )
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        Ok((read, evidence))
    }

    fn check_device_owner(&self) -> Result<()> {
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

    async fn verify_media_head(
        &self,
        barcode: &str,
        volume: Uuid,
        spool_dir: &Path,
        reclaim: impl FnOnce() -> Result<bool>,
    ) -> Result<(LtfsLabel, ActiveLoadedVolumeHead)> {
        let head = self
            .catalog
            .active_loaded_volume_head(self.serial, barcode, self.route, self.key.round())
            .await
            .map_err(|error| TapeError::Catalog(error.to_string()))?
            .ok_or_else(|| TapeError::Catalog("FDB 读取卷未绑定当前带库".into()))?;
        self.check_readable().await?;
        if !head.online || head.blacklisted {
            return Err(TapeError::MediaUnavailable {
                barcode: barcode.into(),
            });
        }
        let label = read_label_only(self.device)?;
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
        self.check_readable().await?;
        Ok((label, head))
    }

    /// Build a missing exact-index cache after proving the current medium
    /// head. Existing caches are never overwritten, including corrupt ones.
    /// The executor must serialize both cache rebuilding and content reads.
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
        self.read_target_with_cache(
            ReadTarget::Reclaim(reservation, request),
            request.pool_id,
            cache_dir,
            spool_dir,
        )
        .await
    }

    async fn read_target_with_cache(
        &self,
        target: ReadTarget<'_>,
        pool: Option<Uuid>,
        cache_dir: &Path,
        spool_dir: &Path,
    ) -> Result<Option<ActiveMediaRead>> {
        self.check_device_owner()?;
        let source = target.resolve(self).await?;
        let Some(source) = source else {
            return Ok(None);
        };
        require_pool(pool, source.placement().pool_id)?;
        let cache_path = cache_dir.join(cache_name(source.source, source.baseline.as_ref()));
        let _cache_pin = match CachePin::open(&cache_path) {
            Ok(pin) => pin,
            Err(TapeError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let (label, head) = self
                    .verify_media_head(
                        &source.placement().barcode,
                        source.source.volume_uuid,
                        spool_dir,
                        || reclaim_cache_space(cache_dir, self.storage),
                    )
                    .await?;
                let mut storage = CacheBuild::new(cache_dir, self.storage)?;
                if let Some(identity) = source
                    .baseline
                    .as_ref()
                    .filter(|identity| identity.head().incremental)
                {
                    let mut budget = RecoveryBudget::default();
                    budget.index_scratch.directory = spool_dir.to_path_buf();
                    budget.index_scratch.max_bytes = self.storage.index_bytes;
                    budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
                    let captured = capture_recorded_index_source(
                        &TapeDrive::new(self.device),
                        &label,
                        identity,
                        &budget,
                    )?;
                    VolumeExtentCache::build_captured(&captured, &cache_path, &mut storage)?;
                } else {
                    VolumeExtentCache::build_managed_from_media(
                        &TapeDrive::new(self.device),
                        &label,
                        &RecoveryBudget::default(),
                        &cache_path,
                        ExpectedIndex {
                            volume_uuid: source.source.volume_uuid,
                            generation: source.source.generation,
                            location: source.source.location,
                            sha256: source.source.sha256,
                        },
                        source.source.incremental,
                        &mut storage,
                    )?;
                }
                storage.finish()?;
                let pin = CachePin::open(&cache_path)?;
                self.check_device_owner()?;
                let final_source = target.resolve(self).await?;
                let final_head = self
                    .catalog
                    .active_loaded_volume_head(
                        self.serial,
                        &source.placement().barcode,
                        self.route,
                        self.key.round(),
                    )
                    .await
                    .map_err(|error| TapeError::Catalog(error.to_string()))?;
                if final_source.as_ref() != Some(&source) || final_head != Some(head) {
                    return Err(TapeError::Catalog(
                        "FDB 文件或卷头在缓存重建期间改变".into(),
                    ));
                }
                self.check_readable().await?;
                pin
            }
            Err(error) => return Err(error),
        };
        self.read_scoped(target, pool, &cache_path, spool_dir, Some(cache_dir))
            .await
    }

    /// Read one published file into an unlinked 0600 temporary file. No bytes
    /// escape before checksum and final route/epoch/copy/head checks pass.
    /// A cache miss is an error; rebuilding a cache is a separate owner task.
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
        cache_path: &Path,
        spool_dir: &Path,
        managed_cache: Option<&Path>,
    ) -> Result<Option<ActiveMediaRead>> {
        self.check_device_owner()?;
        let _cache_pin = CachePin::open(cache_path)?;
        let plan = target
            .resolve(self)
            .await?
            .map(|source| source.plan_read(cache_path))
            .transpose()
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        let Some(plan) = plan else {
            return Ok(None);
        };
        require_pool(pool, plan.read.placement.pool_id)?;
        let reclaim = || match managed_cache {
            Some(directory) => reclaim_cache_space(directory, self.storage),
            None => Ok(false),
        };
        let file = retry_after_space_reclaim(
            || download_file(spool_dir, plan.read.copy.size, self.storage),
            reclaim,
        )?;
        let (label, head) = self
            .verify_media_head(
                &plan.read.placement.barcode,
                plan.source.volume_uuid,
                spool_dir,
                reclaim,
            )
            .await?;

        if let Some(identity) = plan
            .baseline
            .as_ref()
            .filter(|identity| identity.head().incremental)
        {
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
            remaining: plan.read.copy.size,
            digest: match plan.read.copy.content_digest {
                ContentDigest::Unknown => ReadDigest::Unknown,
                ContentDigest::Md5(_) => ReadDigest::Md5(Md5::new()),
                ContentDigest::Sha256(_) => ReadDigest::Sha256(Sha256::new()),
            },
        };
        read_cached_file_to_writer(self.device, &label, &plan.read.physical, &mut writer)?;
        if writer.remaining != 0 {
            return Err(TapeError::Catalog("FDB 文件读取长度不足".into()));
        }
        let actual = match writer.digest {
            ReadDigest::Unknown => ContentDigest::Unknown,
            ReadDigest::Md5(hash) => ContentDigest::Md5(hash.finalize().into()),
            ReadDigest::Sha256(hash) => ContentDigest::Sha256(hash.finalize().into()),
        };
        if actual != plan.read.copy.content_digest {
            return Err(TapeError::Catalog("FDB 文件读取摘要不符".into()));
        }
        self.check_device_owner()?;
        let final_plan = target
            .resolve(self)
            .await?
            .map(|source| source.plan_read(cache_path))
            .transpose()
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        let final_head = self
            .catalog
            .active_loaded_volume_head(
                self.serial,
                &plan.read.placement.barcode,
                self.route,
                self.key.round(),
            )
            .await
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        if final_plan.as_ref() != Some(&plan) || final_head != Some(head) {
            return Err(TapeError::Catalog("FDB 文件或卷头在读取期间改变".into()));
        }
        self.check_readable().await?;
        writer.file.rewind()?;
        Ok(Some(ActiveMediaRead {
            metadata: plan.read,
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

fn cache_name(
    source: ActiveIndexIdentity,
    baseline: Option<&crate::ltfs::recovery::IndexSourceIdentity>,
) -> String {
    let mut hash = Sha256::new();
    if let Some(identity) = baseline.filter(|identity| identity.head().incremental) {
        hash.update(b"tape-rs/complete-replay-cache/1");
        hash.update(identity.fingerprint());
    }
    hash.update(source.volume_uuid.as_bytes());
    hash.update(source.generation.to_be_bytes());
    hash.update((source.location.partition as u32).to_be_bytes());
    hash.update(source.location.start_block.to_be_bytes());
    hash.update(source.sha256);
    hash.update([u8::from(source.incremental)]);
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("extent-{digest}.sqlite")
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
