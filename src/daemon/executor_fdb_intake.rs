//! Explicit, read-only media intake on the sole fenced device actor.
use super::*;
use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::managed_catalog::{CatalogError, ImportSpool, NativeMediaImport, NativeMediaSource};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

struct Workspace(PathBuf);
impl Workspace {
    fn new(parent: &std::path::Path) -> std::result::Result<Self, ReadError> {
        let path = parent.join(format!("fdb-media-intake-{}", uuid::Uuid::new_v4()));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| ReadError::Failed(format!("创建介质接入暂存目录: {e}")))?;
        Ok(Self(path))
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("FDB 原生介质接入权威或状态不可用: {error}"))
}
fn physical(error: TapeError) -> ReadError {
    if ownership_lost(&error)
        || (matches!(&error, TapeError::ScsiCommand { .. })
            && !fdb_operation_guard::confirmed_device_failure(&error))
        || matches!(
            &error,
            TapeError::Io(_)
                | TapeError::Nix(_)
                | TapeError::ScsiHost(_)
                | TapeError::ScsiDriver(_)
                | TapeError::ScsiStatus { .. }
                | TapeError::InvalidResponse { .. }
                | TapeError::Busy { .. }
                | TapeError::NotReady(_)
        )
    {
        lost(error)
    } else {
        ReadError::Failed(format!("原生介质只读核验失败: {error}"))
    }
}
fn projection_error(error: CatalogError) -> ReadError {
    match error.domain() {
        CatalogError::PoolImporting => ReadError::Busy("介质任务已中止，等待回滚".into()),
        CatalogError::Invalid(reason) => ReadError::Failed((*reason).into()),
        error => lost(error),
    }
}
async fn live(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    intake: &NativeMediaImport,
) -> std::result::Result<serde_json::Value, ReadError> {
    if !permit.allows(owner.round, super::super::permission::Phase::Serving) {
        return Err(lost("Serving 许可已失效"));
    }
    fdb_loading::authority_guard(owner, permit, control).await?;
    let value = control
        .catalog
        .native_media_status(intake, owner.round)
        .await
        .map_err(lost)?;
    if value["abort"] == true {
        return Err(ReadError::Busy("原生介质接入正在回滚".into()));
    }
    Ok(value)
}

/// Capture and cross-check a newly formatted empty baseline using the caller's
/// original-task guarded transport. No synthetic XML or source hash is used.
pub(super) fn capture_empty_baseline(
    device: &dyn TapeTransport,
    parent: &std::path::Path,
    storage: crate::local_read_storage::ReadStorageBudget,
) -> Result<NativeMediaSource> {
    let workspace =
        Workspace::new(parent).map_err(|e| TapeError::Catalog(format!("空卷基线暂存: {e:?}")))?;
    let label = crate::ltfs::volume::read_label_only(device)?;
    let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
    budget.index_scratch.directory = workspace.0.clone();
    budget.index_scratch.max_bytes = storage.index_bytes;
    budget.index_scratch.headroom_bytes = storage.headroom_bytes;
    let captured =
        crate::ltfs::recovery::capture_index_source(&TapeDrive::new(device), &label, &budget)?;
    let identity = captured.identity()?;
    let expected = ExpectedIndex {
        volume_uuid: identity.head().volume_uuid,
        generation: identity.head().generation,
        location: identity.head().location,
        sha256: identity.head().sha256,
    };
    let spool_path = workspace.0.join("source.sqlite");
    let cache_path = workspace.0.join("extents.sqlite");
    let cache_storage = crate::local_read_storage::ReadStorageBudget {
        cache_bytes: storage.cache_bytes.min(storage.index_bytes),
        ..storage
    };
    {
        let mut reservation =
            crate::local_read_storage::CacheBuild::new(&workspace.0, cache_storage)?;
        ImportSpool::build_captured(&captured, &spool_path, &mut reservation)?;
    }
    {
        let mut reservation =
            crate::local_read_storage::CacheBuild::new(&workspace.0, cache_storage)?;
        VolumeExtentCache::build_captured(&captured, &cache_path, &mut reservation)?;
    }
    let spool = ImportSpool::open(&spool_path, expected)?;
    let cache = VolumeExtentCache::open_captured(&cache_path, &identity)?;
    spool.verify_all()?;
    spool.verify_extent_cache(&cache)?;
    let proof = NativeMediaSource {
        identity,
        files: cache.file_count()?,
        paths: spool.candidate_stats().entries,
    };
    if proof.files != 0 || proof.paths != 0 {
        return Err(TapeError::Refused {
            reason: "分配基线读回了非空索引，保留原任务核对".into(),
        });
    }
    Ok(proof)
}

pub(super) async fn apply(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    _opts: &ExecOptions,
    status: &SharedStatus,
    epoch: u64,
    id: uuid::Uuid,
) -> std::result::Result<(), ReadError> {
    let intake = match control
        .catalog
        .start_native_media(&control.serial, control.route, owner.round, epoch, id)
        .await
    {
        Ok(Some(intake)) => intake,
        Ok(None) => return Ok(()),
        Err(error)
            if matches!(
                error.domain(),
                CatalogError::Incomplete | CatalogError::PoolBusy | CatalogError::PoolImporting
            ) =>
        {
            return Ok(());
        }
        Err(error) if matches!(error.domain(), CatalogError::Invalid(_)) => {
            let CatalogError::Invalid(reason) = error.domain() else {
                unreachable!()
            };
            control
                .catalog
                .reject_queued_management(
                    &control.serial,
                    control.route,
                    owner.round,
                    epoch,
                    id,
                    reason,
                )
                .await
                .map_err(lost)?;
            return Ok(());
        }
        Err(error) => return Err(lost(error)),
    };
    let current = control
        .catalog
        .native_media_status(&intake, owner.round)
        .await
        .map_err(lost)?;
    if current["abort"] == true {
        control
            .catalog
            .rollback_native_media_page(&intake, owner.round)
            .await
            .map_err(lost)?;
        return Ok(());
    }
    let result = intake_source(owner, permit, control, status, &intake).await;
    match result {
        Ok(()) => Ok(()),
        Err(ReadError::Busy(_)) => Ok(()),
        Err(ReadError::Lost(reason)) => Err(ReadError::Lost(reason)),
        Err(ReadError::Failed(reason)) => {
            match live(owner, permit, control, &intake).await {
                Ok(_) => {}
                Err(ReadError::Busy(_)) => return Ok(()),
                Err(error) => return Err(error),
            }
            control
                .catalog
                .abort_native_media(
                    &intake,
                    owner.round,
                    Some(&reason.chars().take(500).collect::<String>()),
                )
                .await
                .map_err(lost)?;
            control
                .catalog
                .rollback_native_media_page(&intake, owner.round)
                .await
                .map_err(lost)?;
            Ok(())
        }
    }
}

async fn intake_source(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    intake: &NativeMediaImport,
) -> std::result::Result<(), ReadError> {
    let _activity = permit
        .begin_device_activity(owner.round)
        .map_err(physical)?;
    live(owner, permit, control, intake).await?;
    let drive = fdb_loading::select_intake(owner, permit, control, status, intake).await?;
    live(owner, permit, control, intake).await?;
    let round = owner.round;
    let checked =
        fdb_loading::operation_guard(owner, permit, owner.devices[drive].dev.as_ref(), || {
            let value =
                futures_executor::block_on(control.catalog.native_media_status(intake, round))
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
            if value["abort"] == true {
                return Err(TapeError::Catalog("原导入已请求回滚，停止后续命令".into()));
            }
            Ok(())
        });
    let device = &checked as &dyn TapeTransport;
    let label = crate::ltfs::volume::read_label_only(device).map_err(physical)?;
    if intake
        .assigned_volume()
        .map_err(lost)?
        .is_some_and(|volume| volume != label.volume_uuid)
    {
        return Err(ReadError::Failed(
            "卷标签 UUID 与既有分配不符；拒绝刷新其他介质".into(),
        ));
    }
    if intake.request().value()["change"]["volume"]
        .as_str()
        .is_some_and(|v| v != label.volume_uuid.to_string())
    {
        return Err(ReadError::Failed("卷标签 UUID 与命令显式预期不符".into()));
    }
    let workspace = Workspace::new(owner.files.spool_dir())?;
    let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
    budget.max_index_bytes = budget
        .max_index_bytes
        .min(usize::try_from(control.storage.index_bytes).unwrap_or(usize::MAX));
    budget.index_scratch.directory = workspace.0.clone();
    budget.index_scratch.max_bytes = control.storage.index_bytes;
    budget.index_scratch.headroom_bytes = control.storage.headroom_bytes;
    let tape = TapeDrive::new(device);
    let captured =
        crate::ltfs::recovery::capture_index_source(&tape, &label, &budget).map_err(physical)?;
    let identity = captured.identity().map_err(physical)?;
    let evidence = captured.scope();
    let expected = ExpectedIndex {
        volume_uuid: identity.head().volume_uuid,
        generation: identity.head().generation,
        location: identity.head().location,
        sha256: identity.head().sha256,
    };
    let storage = crate::local_read_storage::ReadStorageBudget {
        cache_bytes: control.storage.cache_bytes.min(control.storage.index_bytes),
        ..control.storage
    };
    let spool_path = workspace.0.join("source.sqlite");
    {
        let mut reserved =
            crate::local_read_storage::CacheBuild::new(&workspace.0, storage).map_err(physical)?;
        ImportSpool::build_captured(&captured, &spool_path, &mut reserved).map_err(physical)?;
    }
    let spool = ImportSpool::open(&spool_path, expected).map_err(physical)?;
    let cache_path = workspace.0.join("extents.sqlite");
    {
        let mut reserved =
            crate::local_read_storage::CacheBuild::new(&workspace.0, storage).map_err(physical)?;
        VolumeExtentCache::build_captured(&captured, &cache_path, &mut reserved)
            .map_err(physical)?;
    }
    drop(captured);
    let cache = VolumeExtentCache::open_captured(&cache_path, &identity).map_err(physical)?;
    spool.verify_extent_cache(&cache).map_err(physical)?;
    let pool = control
        .catalog
        .active_pool_for_route(&control.serial, control.route, intake.pool())
        .await
        .map_err(lost)?
        .ok_or_else(|| ReadError::Failed("接入池已不存在".into()))?;
    let root = spool
        .node_by_path("")
        .map_err(physical)?
        .ok_or_else(|| ReadError::Failed("源索引缺少根目录".into()))?;
    for attr in root.attributes.xattrs().unwrap_or(&[]) {
        if (attr.key == crate::ltfs::volume::XATTR_POOL_UUID
            && uuid::Uuid::parse_str(&attr.value).ok() != Some(intake.pool()))
            || (attr.key == crate::ltfs::volume::XATTR_POOL_NAME && attr.value != pool.name)
        {
            return Err(ReadError::Failed(
                "源介质池标记与目标池不符；不自动重写标记".into(),
            ));
        }
    }
    let proof = NativeMediaSource {
        identity,
        files: cache.file_count().map_err(physical)?,
        paths: spool.candidate_stats().entries,
    };
    live(owner, permit, control, intake).await?;
    control
        .catalog
        .bind_native_media_source(intake, owner.round, &proof)
        .await
        .map_err(projection_error)?;
    let state = live(owner, permit, control, intake).await?;
    if state["phase"] == "preflight" {
        let mut after = None;
        loop {
            live(owner, permit, control, intake).await?;
            let page = spool
                .candidate_page(after.as_deref(), 8)
                .map_err(physical)?;
            for candidate in page.candidates {
                let entry = intake
                    .entry(&spool, candidate)
                    .map_err(|e| ReadError::Failed(e.to_string()))?;
                control
                    .catalog
                    .project_native_media_entry(intake, owner.round, &entry, false)
                    .await
                    .map_err(projection_error)?;
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        control
            .catalog
            .begin_native_media_projection(intake, owner.round)
            .await
            .map_err(projection_error)?;
    }
    let state = live(owner, permit, control, intake).await?;
    let mut after = state["after"].as_str().map(str::to_owned);
    loop {
        live(owner, permit, control, intake).await?;
        let page = spool
            .candidate_page(after.as_deref(), 8)
            .map_err(physical)?;
        for candidate in page.candidates {
            live(owner, permit, control, intake).await?;
            let entry = intake
                .entry(&spool, candidate)
                .map_err(|e| ReadError::Failed(e.to_string()))?;
            control
                .catalog
                .project_native_media_entry(intake, owner.round, &entry, true)
                .await
                .map_err(projection_error)?;
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    live(owner, permit, control, intake).await?;
    let latest_label = crate::ltfs::volume::read_label_only(device).map_err(physical)?;
    if latest_label.volume_uuid != label.volume_uuid {
        return Err(ReadError::Failed("发布前卷标签身份改变".into()));
    }
    let latest = crate::ltfs::recovery::capture_index_source(&tape, &latest_label, &budget)
        .map_err(physical)?;
    if latest.identity().map_err(physical)? != proof.identity {
        return Err(ReadError::Failed("发布前介质完整来源链改变".into()));
    }
    drop(latest);
    live(owner, permit, control, intake).await?;
    control
        .catalog
        .finish_native_media(intake, owner.round, &proof)
        .await
        .map_err(projection_error)?;
    // No write, filemark, VCI/MAM update, close-tail or format occurred. The
    // mounted identity becomes eligible for normal reads/writes only now.
    owner.fdb_pending_import.remove(intake.barcode());
    fdb_monitor::index_state(
        owner,
        control,
        intake.barcode(),
        proof.identity.head().into(),
        false,
    )
    .await?;
    owner.fdb_reads.insert(
        drive,
        fdb_loading::Mount {
            barcode: intake.barcode().into(),
            volume: proof.index().volume_uuid,
            pool: intake.pool(),
            last_used: Instant::now(),
        },
    );
    fdb_monitor::sample(
        owner,
        permit,
        control,
        drive,
        intake.barcode(),
        proof.index().volume_uuid,
    )
    .await?;
    info!(
        "FDB 原生介质接入完成: 条码={} 卷={} 路径={} 来源核验={:?}",
        intake.barcode(),
        proof.index().volume_uuid,
        proof.paths,
        evidence
    );
    Ok(())
}
