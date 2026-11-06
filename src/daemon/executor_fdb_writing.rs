//! Explicit FDB writes run only on the serialized, fenced device executor.
use super::*;
use crate::daemon::fdb_media::{
    ActiveMediaUpload, commit_active_media_batch, establish_active_media_owner,
    publish_active_media_batch,
};
use crate::managed_catalog::{
    ActiveFileIntent, ActiveFileRequest, ActiveIndexIdentity, ActiveUploadSpec, CatalogError,
    ContentDigest, PublishOutcome,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::fd::AsRawFd;

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("FDB 写入结果需由接任恢复确认: {error}"))
}

fn media_error(error: TapeError) -> ReadError {
    match error {
        TapeError::MediaUnavailable { barcode } => {
            ReadError::Busy(format!("磁带 {barcode} 离线或在黑名单中"))
        }
        TapeError::PoolBusy { pool } => {
            ReadError::Busy(format!("池 {pool} 正在接入介质；保留原上传请求重试"))
        }
        error => lost(error),
    }
}

pub(super) async fn prepare(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    spec: &ActiveUploadSpec,
    barcode: Option<&str>,
) -> std::result::Result<ActiveFileIntent, ReadError> {
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    if spec.size > control.storage.download_bytes {
        return Err(ReadError::Failed("上传超过单文件暂存上限".into()));
    }
    fdb_loading::guard(owner, permit, control).await?;
    let inventory = inventory_inner(&owner.devices, true).map_err(lost)?;
    let management = fdb_loading::policy(owner, control, &[spec.pool_id], None).await?;
    let pool = spec.pool_id.to_string();
    let restriction = management["pools"][&pool]["mediarestriction"]
        .as_str()
        .ok_or_else(|| lost("FDB 池介质限制缺失"))?;
    let mut capabilities = std::collections::BTreeMap::new();
    for slot in &inventory.drives {
        if !super::super::management::drive_allowed(
            &management,
            &owner.devices[slot.dev].serial,
            'm',
        ) {
            continue;
        }
        let raw =
            crate::scsi::transport::retry_unit_attention("FDB 自动选带密度查询", || {
                TapeDrive::new(owner.devices[slot.dev].dev.as_ref()).report_density_support(false)
            });
        match raw {
            Ok(raw) => {
                if let Some(densities) = super::super::scheduling::densities(&raw) {
                    capabilities.insert(slot.dev, densities);
                }
            }
            Err(error) if ownership_lost(&error) => return Err(lost(error)),
            _ => {}
        }
    }
    let candidates = inventory
        .drives
        .iter()
        .filter_map(|slot| slot.loaded.as_ref().map(|(barcode, _)| barcode))
        .chain(inventory.slots.keys());
    for candidate in candidates {
        if owner.fdb_pending_import.contains(candidate)
            || barcode.is_some_and(|barcode| barcode != candidate)
            || (restriction != "none" && !candidate.ends_with(restriction))
            || inventory.cleaning.contains(candidate)
        {
            continue;
        }
        let schedulable = inventory.drives.iter().any(|slot| {
            capabilities
                .get(&slot.dev)
                .is_some_and(|caps| super::super::scheduling::compatible(caps, candidate, true))
                && (slot.loaded.is_none()
                    || slot
                        .loaded
                        .as_ref()
                        .is_some_and(|(loaded, _)| loaded == candidate)
                    || owner.fdb_reads.contains_key(&slot.dev))
        });
        if !schedulable {
            continue;
        }
        let assignment = control
            .catalog
            .tape_assignment(&control.serial, candidate)
            .await
            .map_err(lost)?;
        if !assignment.is_some_and(|assignment| {
            assignment.namespace == control.route.route.namespace
                && assignment.pool_id == spec.pool_id
                && assignment.volume_id.is_some()
                && assignment.allows_io()
        }) {
            continue;
        }
        match control
            .catalog
            .prepare_active_upload(&control.serial, control.route, owner.round, candidate, spec)
            .await
        {
            Ok(mut intent) => {
                intent.barcode_constraint = barcode.map(str::to_owned);
                fdb_loading::guard(owner, permit, control).await?;
                return Ok(intent);
            }
            Err(
                CatalogError::Incomplete
                | CatalogError::PoolImporting
                | CatalogError::PoolBusy
                | CatalogError::MediaUnavailable,
            ) => continue,
            Err(CatalogError::Invalid(reason)) => return Err(ReadError::Failed(reason.into())),
            Err(error) => return Err(lost(error)),
        }
    }
    Err(ReadError::Busy("池内没有当前可调度且已核验的写入卷".into()))
}

fn verify_spool(
    file: &std::fs::File,
    intent: &ActiveFileRequest,
    limit: u64,
) -> std::result::Result<(), ReadError> {
    let metadata = file
        .metadata()
        .map_err(|error| ReadError::Failed(error.to_string()))?;
    if !metadata.is_file() || metadata.len() != intent.expected_size || metadata.len() > limit {
        return Err(ReadError::Failed("上传长度或暂存预算不匹配".into()));
    }
    let ContentDigest::Sha256(expected) = intent.expected_digest else {
        return Err(ReadError::Failed("FDB 上传必须提供 SHA-256".into()));
    };
    // Reopening the descriptor starts an independent cursor without retaining
    // a pathname that could be replaced while the request is queued.
    let mut input = std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|error| ReadError::Failed(error.to_string()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = input
            .read(&mut buffer)
            .map_err(|error| ReadError::Failed(error.to_string()))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if <[u8; 32]>::from(hash.finalize()) != expected {
        return Err(ReadError::Failed("上传 SHA-256 不匹配".into()));
    }
    Ok(())
}

pub(super) async fn write(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    status: &SharedStatus,
    request: ActiveFileRequest,
    file: std::fs::File,
) -> std::result::Result<u64, ReadError> {
    request
        .ltfs_path()
        .map_err(|error| ReadError::Failed(error.to_string()))?;
    if request.library_serial != control.serial
        || request.namespace != control.route.route.namespace
        || request.pool_id.is_none()
    {
        return Err(ReadError::Failed("上传不属于当前带库、命名空间或池".into()));
    }
    verify_spool(&file, &request, control.storage.download_bytes)?;
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    fdb_loading::guard(owner, permit, control).await?;
    if let Some(result) = control
        .catalog
        .active_upload_result(&request, control.route)
        .await
        .map_err(lost)?
    {
        return match result {
            PublishOutcome::AlreadyApplied(version) => Ok(version),
            other => Err(ReadError::Failed(format!("上传请求不能重用: {other:?}"))),
        };
    }
    control
        .catalog
        .check_active_upload_gate(control.route, owner.round, &request)
        .await
        .map_err(|error| match error {
            CatalogError::PoolImporting => {
                ReadError::Busy("池正在接入介质；保留原上传请求重试".into())
            }
            CatalogError::PoolBusy => ReadError::Busy("池存在节点修改；保留原上传请求重试".into()),
            CatalogError::Invalid(reason) => ReadError::Failed(reason.into()),
            error => lost(error),
        })?;
    let mut intent = match control
        .catalog
        .active_file_attempt(&request, control.route)
        .await
        .map_err(lost)?
    {
        Some(saved) => saved,
        None => {
            if let Some(saved) = control
                .catalog
                .reclaim_upload_attempt(control.route, owner.round, &request)
                .await
                .map_err(lost)?
            {
                saved
            } else {
                let spec = ActiveUploadSpec {
                    pool_id: request.pool_id.expect("validated pool"),
                    path: request.ltfs_path().map_err(lost)?.into_bytes(),
                    operation_id: request.operation_id,
                    expected_version: request.expected_version,
                    size: request.expected_size,
                    digest: request.expected_digest,
                };
                let mut selected =
                    prepare(owner, permit, control, &spec, request.barcode.as_deref()).await?;
                if request.expected_version.is_some() && selected.file_id != request.file_id {
                    return Err(ReadError::Failed("上传原 inode 已改变".into()));
                }
                // Creation inode and request epoch were fixed by the original
                // logical preflight; only this first physical attempt is selected now.
                selected.file_id = request.file_id;
                selected.request_epoch = request.request_epoch;
                if selected.request() != request {
                    return Err(ReadError::Failed(
                        "上传原路径、父目录或版本条件已改变".into(),
                    ));
                }
                selected
            }
        }
    };
    intent.owner_epoch = owner.round;
    let placement = control
        .catalog
        .volume_placement(intent.volume_id)
        .await
        .map_err(lost)?
        .filter(|placement| {
            placement.namespace == intent.namespace
                && placement.library_serial == control.serial
                && Some(placement.pool_id) == intent.pool_id
        })
        .ok_or_else(|| ReadError::Failed("目标卷不属于上传池".into()))?;
    if owner.fdb_pending_import.contains(&placement.barcode) {
        return Err(ReadError::Busy(
            "目标磁带等待显式索引导入，保留原上传请求重试".into(),
        ));
    }
    let head = control
        .catalog
        .active_loaded_volume_head(
            &control.serial,
            &placement.barcode,
            control.route,
            owner.round,
        )
        .await
        .map_err(lost)?
        .ok_or_else(|| ReadError::Failed("目标卷没有活动归属".into()))?;
    if head.volume_id != intent.volume_id
        || head.index.generation.checked_add(1) != Some(intent.volume_generation)
    {
        return Err(ReadError::Failed(
            "目标卷代数改变，请先查询原请求结果".into(),
        ));
    }
    if !head.online || head.blacklisted {
        return Err(ReadError::Busy(
            "目标磁带离线或在黑名单中；保留原上传请求重试".into(),
        ));
    }
    let pool = control
        .catalog
        .active_pool_for_route(&control.serial, control.route, placement.pool_id)
        .await
        .map_err(lost)?
        .ok_or_else(|| lost("上传池已不存在"))?;
    let count = control
        .catalog
        .active_volume_file_count(
            &control.serial,
            control.route,
            owner.round,
            intent.volume_id,
        )
        .await
        .map_err(lost)?;
    if count >= pool.file_limit {
        return Err(ReadError::Busy(
            "目标卷已达到池文件数上限；保留原请求查询结果".into(),
        ));
    }
    let drive =
        fdb_loading::select_write(owner, permit, control, status, &placement, head.index).await?;
    fdb_monitor::write_allowed(owner, control, &placement.barcode, true).await?;
    fdb_loading::guard(owner, permit, control).await?;
    let round = owner.round;
    let session = owner.write_session.take();
    let original_intent = intent.clone();
    let seen_reservation = std::cell::Cell::new(false);
    let checked =
        fdb_loading::operation_guard(owner, permit, owner.devices[drive].dev.as_ref(), || {
            futures_executor::block_on(async {
                control
                    .catalog
                    .require_active_library_readable(&control.serial, control.route, round)
                    .await
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                control
                    .catalog
                    .check_active_upload_gate(control.route, round, &original_intent.request())
                    .await
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                let current = control
                    .catalog
                    .active_file_attempt(&original_intent.request(), control.route)
                    .await
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                if let Some(current) = current {
                    if current != original_intent {
                        return Err(TapeError::Catalog("上传原物理尝试已改变".into()));
                    }
                    seen_reservation.set(true);
                } else if seen_reservation.get() {
                    return Err(TapeError::Catalog(
                        "上传原物理尝试已被移除，停止后续命令".into(),
                    ));
                }
                Ok(())
            })
        });
    let tracked = fdb_monitor::Tracked::new(&checked, "write");
    let mounted = match session.filter(|session| {
        session.matches_device(&tracked, owner.key)
            && ActiveIndexIdentity::from(session.committed_index_construct()) == head.index
    }) {
        Some(session) => {
            let resumed = session.resume(&tracked, owner.key);
            if resumed.is_ok() {
                log::debug!(
                    "FDB 复用顺序追加会话: {} gen={}",
                    placement.barcode,
                    head.index.generation
                );
            }
            resumed
        }
        None => LtfsVolume::mount(&tracked),
    };
    if mounted.is_err() {
        fdb_monitor::io_error(
            owner,
            permit,
            control,
            &placement.barcode,
            intent.volume_id,
            tracked.error(),
        )
        .await;
    }
    let volume = mounted.map_err(lost)?;
    let max_index_bytes = usize::try_from(control.storage.index_bytes).map_err(lost)?;
    let ready = establish_active_media_owner(
        &control.catalog,
        volume,
        &control.serial,
        owner.key,
        permit,
        max_index_bytes,
    )
    .await;
    if ready.is_err() {
        fdb_monitor::io_error(
            owner,
            permit,
            control,
            &placement.barcode,
            intent.volume_id,
            tracked.error(),
        )
        .await;
    }
    let ready = ready.map_err(media_error)?;
    let volume = ready.into_volume();
    if let Some(result) = control
        .catalog
        .active_upload_result(&request, control.route)
        .await
        .map_err(lost)?
    {
        drop(volume);
        owner.fdb_reads.insert(
            drive,
            fdb_loading::Mount {
                barcode: placement.barcode.clone(),
                volume: intent.volume_id,
                pool: placement.pool_id,
                last_used: Instant::now(),
            },
        );
        return match result {
            PublishOutcome::AlreadyApplied(version) => Ok(version),
            other => Err(ReadError::Failed(format!("原上传恢复结果拒绝: {other:?}"))),
        };
    }
    let batch = uuid::Uuid::new_v4();
    let upload = ActiveMediaUpload {
        intent: intent.clone(),
        spool: format!("/proc/self/fd/{}", file.as_raw_fd()).into(),
    };
    let committed =
        commit_active_media_batch(&control.catalog, volume, batch, &[upload], max_index_bytes)
            .await;
    if committed.is_err() {
        fdb_monitor::io_error(
            owner,
            permit,
            control,
            &placement.barcode,
            intent.volume_id,
            tracked.error(),
        )
        .await;
    }
    let committed = committed.map_err(media_error)?;
    let committed = publish_active_media_batch(&control.catalog, committed)
        .await
        .map_err(lost)?;
    // Keep only this clean S4 session, without a device borrow or another
    // checkpoint. A later request must recheck its device epoch, PR and the
    // exact FDB index before reusing its confirmed sequential append head.
    let session = committed.into_volume().into_session();
    let current = control
        .catalog
        .active_loaded_volume_head(
            &control.serial,
            &placement.barcode,
            control.route,
            owner.round,
        )
        .await
        .map_err(lost)?
        .ok_or_else(|| lost("写入后卷身份缺失"))?;
    fdb_monitor::index_state(owner, control, &placement.barcode, current.index, false).await?;
    fdb_monitor::sample(
        owner,
        permit,
        control,
        drive,
        &placement.barcode,
        intent.volume_id,
    )
    .await?;
    fdb_loading::guard(owner, permit, control).await?;
    owner.fdb_reads.insert(
        drive,
        fdb_loading::Mount {
            barcode: placement.barcode,
            volume: intent.volume_id,
            pool: placement.pool_id,
            last_used: Instant::now(),
        },
    );
    let result = match control
        .catalog
        .active_upload_result(&intent.request(), control.route)
        .await
        .map_err(lost)?
    {
        Some(PublishOutcome::AlreadyApplied(version)) => Ok(version),
        result => Err(lost(format!("发布后请求结果不一致: {result:?}"))),
    };
    if result.is_ok() {
        owner.write_session = session;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_gate_busy_preserves_serving_but_unknown_media_errors_do_not() {
        let pool = uuid::Uuid::new_v4();
        assert!(matches!(
            media_error(TapeError::PoolBusy {
                pool: pool.to_string()
            }),
            ReadError::Busy(_)
        ));
        assert!(matches!(
            media_error(TapeError::Catalog("unknown commit".into())),
            ReadError::Lost(_)
        ));
        assert!(matches!(
            media_error(TapeError::NotReady("unknown device state".into())),
            ReadError::Lost(_)
        ));
    }
    #[test]
    fn spool_validation_rejects_wrong_length_digest_and_budget_before_devices() {
        let root = std::env::temp_dir().join(format!("fdb-upload-{}", uuid::Uuid::new_v4()));
        let mut file = std::fs::File::create_new(&root).unwrap();
        std::io::Write::write_all(&mut file, b"abc").unwrap();
        let file = std::fs::File::open(&root).unwrap();
        std::fs::remove_file(root).unwrap();
        let mut intent = ActiveFileIntent {
            barcode_constraint: None,
            namespace: uuid::Uuid::new_v4(),
            parent: uuid::Uuid::nil(),
            parent_path: b"/".to_vec(),
            name: b"a".to_vec(),
            file_id: uuid::Uuid::new_v4(),
            expected_version: None,
            operation_id: uuid::Uuid::new_v4(),
            request_epoch: 1,
            library_serial: "SIM".into(),
            pool_id: Some(uuid::Uuid::new_v4()),
            owner_epoch: 1,
            volume_id: uuid::Uuid::new_v4(),
            volume_generation: 2,
            volume_scan_id: uuid::Uuid::new_v4(),
            expected_size: 3,
            expected_digest: ContentDigest::Sha256(Sha256::digest(b"abc").into()),
        };
        assert!(verify_spool(&file, &intent.request(), 3).is_ok());
        assert!(verify_spool(&file, &intent.request(), 2).is_err());
        intent.expected_size = 4;
        assert!(verify_spool(&file, &intent.request(), 4).is_err());
        intent.expected_size = 3;
        intent.expected_digest = ContentDigest::Sha256([0; 32]);
        assert!(verify_spool(&file, &intent.request(), 3).is_err());
    }
}
