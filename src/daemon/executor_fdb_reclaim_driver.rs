//! Parent scheduling over the original FDB task, fixed physical layout and
//! independent migration/initialization receipts. No state is reconstructed
//! from a newer logical path after a takeover.
use super::*;
#[path = "executor_fdb_reclaim_qualification.rs"]
mod qualification;
#[path = "executor_fdb_reclaim_copy.rs"]
mod copying;
#[path = "executor_fdb_reclaim_checkpoint.rs"]
mod checkpoint;
#[path = "executor_fdb_reclaim_parallel.rs"]
mod parallel;
#[cfg(test)]
pub(in crate::daemon::executor) use checkpoint::probe_original as probe_original_for_test;
#[cfg(test)]
pub(in crate::daemon::executor) use checkpoint::arm_original_probe_seal_for_test;
#[cfg(test)]
pub(in crate::daemon::executor) use checkpoint::{arm_original_checkpoint_s4_stop_for_test, OriginalS4StopFacts};
#[cfg(test)]
pub(in crate::daemon::executor) use checkpoint::{
    ReadbackSealMode, arm_readback_seal_for_test, observe_group_read_for_test,
};
pub(in crate::daemon) use checkpoint::{OriginalAttemptProbeIo, FreshReclaimGroupIo, ObserveReclaimGroupIo};
#[path = "executor_fdb_reclaim_metadata.rs"]
mod metadata;
#[path = "executor_fdb_reclaim_upload_recovery.rs"]
mod upload_recovery;
use crate::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use crate::daemon::fdb_read::{ActiveMediaReader, ReclaimSourceSession};
use crate::daemon::fdb_reclaim_mechanics::{LoadPreIssue, ReclaimMechanics};
use crate::ltfs::recovery::{CapturedIndexSource, RecoveryBudget};
use crate::managed_catalog::{
    ReclaimDriveBinding, ReclaimLayout, ReclaimMediaAction, ReclaimReservation,
};
use log::warn;

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("原生 reclaim 继续执行需核实原状态: {error}"))
}
fn parent_error(error: crate::managed_catalog::CatalogError) -> ReadError {
    use crate::managed_catalog::CatalogError;
    match error.domain() {
        CatalogError::PoolBusy | CatalogError::PoolImporting | CatalogError::MediaUnavailable => {
            ReadError::Busy(error.to_string())
        }
        _ => lost(error),
    }
}
fn device(owner: &Active, serial: &str) -> std::result::Result<usize, ReadError> {
    let matches: Vec<_> = owner
        .devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == DeviceKind::Drive && d.serial == serial)
        .map(|(i, _)| i)
        .collect();
    if matches.len() != 1 {
        return Err(lost("原固定驱动器不能唯一匹配"));
    }
    Ok(matches[0])
}
fn actor<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    drive: usize,
) -> std::result::Result<ReclaimMechanics<'a>, ReadError> {
    Ok(ReclaimMechanics {
        catalog: &control.catalog,
        changer: owner
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Changer)
            .ok_or_else(|| lost("原换带器缺失"))?
            .dev
            .as_ref(),
        drive: owner.devices[drive].dev.as_ref(),
        route: control.route,
        key: owner.key,
        permission: permit,
    })
}
fn reader<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    drive: usize,
) -> ActiveMediaReader<'a> {
    ActiveMediaReader {
        catalog: &control.catalog,
        device: owner.devices[drive].dev.as_ref(),
        serial: &control.serial,
        route: control.route,
        key: owner.key,
        permission: permit,
        storage: control.storage,
    }
}

/// An observed original source, retained across the next parent tick only.
/// This does not replace the format admission or its fresh physical check.
pub(in crate::daemon::executor) struct PreparedReclaimFormatSource {
    pub(super) proof: crate::managed_catalog::ReclaimFormatSourceEvidence,
    pub(super) parent: serde_json::Value,
    pub(super) round: u64,
    pub(super) source: ReclaimDriveBinding,
    pub(super) device: String,
    pub(super) device_epoch: u64,
}

impl PreparedReclaimFormatSource {
    fn matches_loaded_source(
        &self,
        owner: &Active,
        parent: &ReclaimReservation,
        layout: &ReclaimLayout,
        drive: usize,
    ) -> bool {
        let device = &owner.devices[drive];
        self.parent == *parent.intent()
            && self.round == owner.round
            && self.source == layout.source
            && device.serial == self.source.drive_serial
            && self.device == device.dev.identity()
            && device.dev.session_epoch() == Some(self.device_epoch)
    }
}

fn budget(owner: &Active, control: &FdbOwnerControl) -> RecoveryBudget {
    let mut budget = RecoveryBudget::default().with_complete_full_history();
    budget.max_index_bytes = budget
        .max_index_bytes
        .min(usize::try_from(control.storage.index_bytes).unwrap_or(usize::MAX));
    budget.index_scratch.directory = owner.files.spool_dir().into();
    budget.index_scratch.max_bytes = control.storage.index_bytes;
    budget.index_scratch.headroom_bytes = control.storage.headroom_bytes;
    budget
}
fn unchanged_rounds(
    saved: &mut Option<(String, [u8; 32], u32)>,
    operation: &str,
    progress: [u8; 32],
) -> u32 {
    let count = match saved.as_ref() {
        Some((old_operation, old_progress, rounds))
            if old_operation == operation && old_progress == &progress =>
        {
            rounds.saturating_add(1)
        }
        _ => 0,
    };
    *saved = Some((operation.into(), progress, count));
    count
}

const COPY_STALL_REASON: &str = "reclaim 连续多轮的父阶段/子请求计数、源封存/覆盖游标及元数据尝试汇总检查点未变；保留原任务等待核实，不代表所有物理或持久操作均未推进";

async fn observe_copy_progress(
    owner: &mut Active,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    sealed: &crate::managed_catalog::ReclaimSourceProgress,
) -> std::result::Result<(), ReadError> {
    let status = control
        .catalog
        .reserved_reclaim_status(parent, owner.round)
        .await
        .map_err(parent_error)?;
    let coverage = if sealed.complete() {
        Some(
            control
                .catalog
                .reclaim_source_coverage_progress(parent, owner.round)
                .await
                .map_err(parent_error)?,
        )
    } else {
        None
    };
    let metadata = control
        .catalog
        .reclaim_metadata_checkpoint(parent, owner.round)
        .await
        .map_err(parent_error)?
        .map(|task| {
            serde_json::json!({
                "id":task.id().to_string(), "recovery_epoch":task.recovery_epoch(),
                "writing":task.writing(), "recovering":task.recovering(),
                "resolved":task.resolved(), "closed":task.closed(), "completed":task.completed()
            })
        });
    let progress = serde_json::to_vec(&(&status, sealed, coverage, metadata)).map_err(lost)?;
    let digest = Sha256::digest(progress).into();
    let operation = parent.intent()["operation"]
        .as_str()
        .ok_or_else(|| lost("原父请求身份缺失"))?;
    let count = unchanged_rounds(&mut owner.reclaim_copy_stall, operation, digest);
    if count == 0 {
        control
            .catalog
            .update_reclaim_dependency_if(parent, owner.round, Some(COPY_STALL_REASON), None)
            .await
            .map_err(parent_error)?;
    } else if count == RECLAIM_COPY_STALL_ROUNDS {
        warn!(
            "reclaim {operation} 连续 {count} 轮的父阶段/子请求计数、源封存/覆盖游标及元数据尝试汇总检查点未变，保留原任务等待核实"
        );
        control
            .catalog
            .update_reclaim_dependency_if(parent, owner.round, None, Some(COPY_STALL_REASON))
            .await
            .map_err(parent_error)?;
    }
    Ok(())
}

fn inventory(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
) -> std::result::Result<(ElementAddressMap, Vec<ElementStatus>), ReadError> {
    let changer = owner
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .ok_or_else(|| lost("原换带器缺失"))?;
    let checked = fdb_loading::operation_guard(owner, permit, changer.dev.as_ref(), || {
        futures_executor::block_on(async {
            let status = control
                .catalog
                .reserved_reclaim_status(parent, owner.round)
                .await?;
            if status["owner_epoch"] != owner.round {
                return Err(crate::managed_catalog::CatalogError::OwnerFenced);
            }
            if control
                .catalog
                .reclaim_media_pending(parent, owner.round)
                .await?
                .is_some()
            {
                return Err(crate::managed_catalog::CatalogError::PoolBusy);
            }
            Ok(())
        })
        .map_err(|e: crate::managed_catalog::CatalogError| TapeError::Catalog(e.to_string()))
    });
    let mut robot = MediumChanger::new(&checked);
    let map = robot.load_address_map().map_err(lost)?.clone();
    let rows = robot.read_all_status().map_err(lost)?;
    checked.verify_proof().map_err(lost)?;
    // Reuse the strict ordinary inventory validator before selecting bindings.
    inventory_elements(&owner.devices, &rows, true).map_err(lost)?;
    Ok((map, rows))
}
fn binding(
    owner: &Active,
    rows: &[ElementStatus],
    assignment: &crate::managed_catalog::TapeAssignment,
    avoid: Option<u16>,
    drive_allowed: impl Fn(&str) -> bool,
    resources: Option<&crate::logical_library::LogicalLibraryResources>,
) -> Option<ReclaimDriveBinding> {
    let media: Vec<_> = rows
        .iter()
        .filter(|r| r.full && r.volume_tag.as_deref() == Some(assignment.barcode.as_str()))
        .collect();
    if media.len() != 1 {
        return None;
    }
    let media = media[0];
    let home = if media.element_type == ElementType::Storage {
        media.address
    } else {
        media.source_address?
    };
    if resources.is_some_and(|resources| !resources.storage_slots.contains(&home)) {
        return None;
    }
    let drive = if media.element_type == ElementType::DataTransfer {
        (Some(media.address) != avoid && media.drive_serial().is_some_and(&drive_allowed))
            .then_some(media)?
    } else if media.element_type == ElementType::Storage {
        rows.iter().find(|r| {
            r.element_type == ElementType::DataTransfer
                && !r.full
                && Some(r.address) != avoid
                && r.drive_serial().is_some_and(|id| {
                    drive_allowed(id)
                        && owner
                            .devices
                            .iter()
                            .any(|d| d.kind == DeviceKind::Drive && d.serial == id)
                })
        })?
    } else {
        return None;
    };
    Some(ReclaimDriveBinding {
        barcode: assignment.barcode.clone(),
        volume: assignment.volume_id?,
        revision: assignment.revision,
        drive_serial: drive.drive_serial()?.to_owned(),
        address: drive.address,
        home,
    })
}
async fn layout(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    resources: Option<&crate::logical_library::LogicalLibraryResources>,
) -> std::result::Result<ReclaimLayout, ReadError> {
    if let Some(layout) = control
        .catalog
        .reclaim_layout(parent, owner.round)
        .await
        .map_err(parent_error)?
    {
        return Ok(layout);
    }
    let (_, rows) = inventory(owner, permit, control, parent)?;
    let source = control
        .catalog
        .tape_assignment(&control.serial, parent.spec().barcode())
        .await
        .map_err(parent_error)?
        .ok_or_else(|| lost("原源归属缺失"))?;
    let management = fdb_loading::policy(owner, control, &[parent.spec().pool()], None).await?;
    let drive_allowed = |id: &str| {
        let allowed = super::super::super::management::drive_allowed(&management, id, 'm');
        allowed
            && resources.is_none_or(|r| {
                r.drives.iter().any(|serial| serial == id)
                    && super::super::super::management::drive_allowed(&management, id, 'g')
            })
    };
    let pool = &management["pools"][parent.spec().pool().to_string()];
    let limit = pool["mountlimit"]
        .as_str()
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or_else(|| lost("回收池挂载上限缺失"))?;
    let restriction = pool["mediarestriction"]
        .as_str()
        .ok_or_else(|| lost("回收池介质限制缺失"))?;
    let mut occupied = 0usize;
    if limit != 0 {
        for row in rows
            .iter()
            .filter(|r| r.full && r.element_type == ElementType::DataTransfer)
        {
            if let Some(barcode) = &row.volume_tag
                && let Some(assignment) = control
                    .catalog
                    .tape_assignment(&control.serial, barcode)
                    .await
                    .map_err(parent_error)?
                && assignment.namespace == parent.namespace()
                && assignment.pool_id == parent.spec().pool()
            {
                occupied += 1;
            }
        }
    }
    let mut after = None;
    loop {
        let page = control
            .catalog
            .active_tape_page_for_route(
                &control.serial,
                control.route,
                Some(parent.spec().pool()),
                after.as_deref(),
                16,
            )
            .await
            .map_err(parent_error)?;
        for value in page["items"]
            .as_array()
            .ok_or_else(|| lost("回收候选分页缺失"))?
        {
            let barcode = value["barcode"]
                .as_str()
                .ok_or_else(|| lost("回收候选条码缺失"))?;
            if barcode == source.barcode {
                continue;
            }
            let Some(target) = control
                .catalog
                .tape_assignment(&control.serial, barcode)
                .await
                .map_err(parent_error)?
            else {
                continue;
            };
            if !target.allows_io() || target.volume_id.is_none() {
                continue;
            }
            if restriction != "none" && !barcode.ends_with(restriction)
                || control
                    .catalog
                    .active_tape_monitor(&control.serial, control.route, barcode)
                    .await
                    .map_err(parent_error)?
                    .is_some_and(|record| !record.data_write_allowed())
            {
                continue;
            }
            // Prefer a destination already in a drive; choose the source's
            // empty drive around it rather than stealing that occupied address.
            let target_loaded = rows
                .iter()
                .find(|r| {
                    r.full
                        && r.element_type == ElementType::DataTransfer
                        && r.volume_tag.as_deref() == Some(barcode)
                })
                .map(|r| r.address);
            let Some(src) = binding(
                owner,
                &rows,
                &source,
                target_loaded,
                drive_allowed,
                resources,
            ) else {
                continue;
            };
            let Some(dst) = binding(
                owner,
                &rows,
                &target,
                Some(src.address),
                drive_allowed,
                resources,
            ) else {
                continue;
            };
            let additional = [&src, &dst]
                .iter()
                .filter(|binding| {
                    !rows.iter().any(|row| {
                        row.full
                            && row.element_type == ElementType::DataTransfer
                            && row.address == binding.address
                            && row.volume_tag.as_deref() == Some(binding.barcode.as_str())
                    })
                })
                .count();
            if limit != 0 && occupied.saturating_add(additional) > limit {
                continue;
            }
            let drive = device(owner, &src.drive_serial)?;
            match actor(owner, permit, control, drive)?
                .prepare_layout(parent, src, dst)
                .await
            {
                Ok(layout) => return Ok(layout),
                Err(e) => return Err(lost(e)),
            }
        }
        after = page["next"].as_str().map(str::to_owned);
        if after.is_none() {
            return Err(ReadError::Busy(
                "reclaim 等待同池目的带及范围内两个符合介质、健康和挂载策略的可用驱动器".into(),
            ));
        }
    }
}
async fn place(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    binding: &ReclaimDriveBinding,
    drive: usize,
) -> std::result::Result<bool, ReadError> {
    let (map, rows) = inventory(owner, permit, control, parent)?;
    if map != layout.map {
        return Err(lost("原固定 address map 改变"));
    }
    let media = rows
        .iter()
        .find(|r| r.full && r.volume_tag.as_deref() == Some(binding.barcode.as_str()))
        .ok_or_else(|| lost("原固定介质不在库内"))?;
    if media.element_type == ElementType::Storage && media.address == binding.home {
        let mut movement = layout.plan(binding, ReclaimMediaAction::Move);
        movement.source = binding.home;
        movement.destination = binding.address;
        actor(owner, permit, control, drive)?
            .execute(parent, &movement)
            .await
            .map_err(lost)?;
        return Ok(true);
    }
    if media.element_type != ElementType::DataTransfer || media.address != binding.address {
        return Err(lost("原固定介质已不在 home 或原驱动器"));
    }
    Ok(false)
}

async fn ensure_loaded(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    binding: &ReclaimDriveBinding,
    drive: usize,
) -> std::result::Result<(), ReadError> {
    actor(owner, permit, control, drive)?
        .execute(parent, &layout.plan(binding, ReclaimMediaAction::Load))
        .await
        .map_err(lost)
}

async fn load(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    binding: &ReclaimDriveBinding,
) -> std::result::Result<usize, ReadError> {
    let drive = device(owner, &binding.drive_serial)?;
    place(owner, permit, control, parent, layout, binding, drive).await?;
    ensure_loaded(owner, permit, control, parent, layout, binding, drive).await?;
    owner.fdb_reads.remove(&drive);
    owner.fdb_read_session = None;
    owner.write_session = None;
    Ok(drive)
}

struct CopyMediaReady {
    source: usize,
    stayed_loaded: bool,
    capture: Option<Box<ReclaimSourceSession>>,
}

async fn confirmed_copy_move(
    owner: &Active,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    binding: &ReclaimDriveBinding,
) -> std::result::Result<bool, ReadError> {
    let mut movement = layout.plan(binding, ReclaimMediaAction::Move);
    movement.source = binding.home;
    movement.destination = binding.address;
    Ok(control
        .catalog
        .reclaim_confirmed_media(parent, owner.round)
        .await
        .map_err(parent_error)?
        .is_some_and(|command| command.owner() == owner.round && command.plan() == &movement))
}

struct ColdSourceCapture {
    session: Box<ReclaimSourceSession>,
    identity: String,
    epoch: u64,
    medium_change: Option<[u8; 32]>,
}

async fn require_cold_source(
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    round: u64,
) -> std::result::Result<(), crate::managed_catalog::CatalogError> {
    control
        .catalog
        .require_reclaim_read_lane(parent, round, layout.source.volume)
        .await?;
    if control
        .catalog
        .reclaim_layout(parent, round)
        .await?
        .as_ref()
        != Some(layout)
    {
        return Err(crate::managed_catalog::CatalogError::Invalid(
            "并行源预检的原固定回收布局改变",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // The two independently owned drives share the same task.
async fn wait_destination_with_source_capture(
    owner: &mut Active,
    permit: &Arc<Permission>,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    source: usize,
    destination: usize,
    reason: TapeError,
) -> std::result::Result<Option<Box<ReclaimSourceSession>>, ReadError> {
    let source_permit = permit.clone();
    let source_control = FdbOwnerControl {
        catalog: control.catalog.clone(),
        serial: control.serial.clone(),
        route: control.route,
        storage: control.storage,
    };
    let source_parent = parent.clone();
    let source_layout = layout.clone();
    let spool = owner.files.spool_dir().to_owned();
    let key = owner.key;
    let mut worker = parallel::DriveWorker::start(owner, source, move |drive| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(lost)?;
        runtime.block_on(async {
            let require_source = || {
                require_cold_source(&source_control, &source_parent, &source_layout, key.round())
            };
            require_source().await.map_err(parent_error)?;
            let checked = fdb_operation_guard::Guard::new_bracketed_read(drive, || {
                futures_executor::block_on(require_source())
                    .map_err(|error| TapeError::Catalog(error.to_string()))
            });
            let reader = ActiveMediaReader {
                catalog: &source_control.catalog,
                device: &checked,
                serial: &source_control.serial,
                route: source_control.route,
                key,
                permission: &source_permit,
                storage: source_control.storage,
            };
            let session = reader
                .capture_reclaim_source_session(&source_parent, &spool, None)
                .await
                .map_err(lost)?;
            checked.verify_proof().map_err(lost)?;
            Ok(ColdSourceCapture {
                session,
                identity: drive.identity().to_owned(),
                epoch: drive
                    .session_epoch()
                    .ok_or_else(|| lost("并行源预检缺少设备代次"))?,
                medium_change: drive.medium_change_token(),
            })
        })
    })?;
    let target_ready = actor(worker.owner(), permit, control, destination)?
        .wait_load_preissue(
            parent,
            &layout.plan(&layout.destination, ReclaimMediaAction::Load),
            reason,
        )
        .await
        .map_err(lost)?;
    // Wait even when a real LOAD is needed. A fatal result from the source
    // still fails the step, and the scoped handle has returned before issue.
    let captured = worker.finish()?;
    if !target_ready {
        return Ok(None);
    }
    if !matches!(
        actor(owner, permit, control, source)?
            .observe_load_preissue(
                parent,
                &layout.plan(&layout.source, ReclaimMediaAction::Load),
            )
            .await
            .map_err(lost)?,
        LoadPreIssue::Ready
    ) {
        return Err(lost("目标就绪后无法确认原源带仍在同一驱动器且就绪"));
    }
    let drive = &owner.devices[source];
    let head =
        crate::managed_catalog::ActiveIndexIdentity::from(captured.session.captured().construct());
    let checked = fdb_operation_guard::Guard::new_bracketed_read(drive.dev.as_ref(), || {
        futures_executor::block_on(async {
            require_cold_source(control, parent, layout, owner.round).await?;
            if control
                .catalog
                .reclaim_source_head(parent, owner.round)
                .await?
                != head
            {
                return Err(crate::managed_catalog::CatalogError::Invalid(
                    "并行源预检的原封存索引已改变",
                ));
            }
            Ok(())
        })
        .map_err(|error: crate::managed_catalog::CatalogError| {
            TapeError::Catalog(error.to_string())
        })
    });
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    crate::scsi::reservation::verify_holder_without_ua_retry(&checked, key).map_err(lost)?;
    checked.verify_proof().map_err(lost)?;
    if drive.serial != layout.source.drive_serial
        || drive.dev.identity() != captured.identity
        || drive.dev.execution_guard() != Some((owner.round, Arc::as_ptr(permit) as usize))
        || drive.dev.session_epoch() != Some(captured.epoch)
        || drive.dev.medium_change_token() != captured.medium_change
        || !permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
    {
        return Err(lost("目标就绪期间原源驱动器、介质或索引已改变"));
    }
    log::info!(
        "回收已复用目标等待期间的原源预检: 操作={} 源驱动器={} 目标驱动器={}",
        parent.intent()["operation"],
        layout.source.drive_serial,
        layout.destination.drive_serial,
    );
    Ok(Some(captured.session))
}

async fn load_copy_media(
    owner: &mut Active,
    permit: &Arc<Permission>,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    allow_overlap: bool,
) -> std::result::Result<CopyMediaReady, ReadError> {
    if !parent.spec().group_only() {
        let source = load(owner, permit, control, parent, layout, &layout.source).await?;
        load(owner, permit, control, parent, layout, &layout.destination).await?;
        return Ok(CopyMediaReady {
            source,
            stayed_loaded: false,
            capture: None,
        });
    }

    let source = device(owner, &layout.source.drive_serial)?;
    let destination = device(owner, &layout.destination.drive_serial)?;
    // A MOVE can change the medium before an error reaches the executor.
    owner.fdb_reads.remove(&source);
    owner.fdb_reads.remove(&destination);
    owner.fdb_read_session = None;
    owner.write_session = None;

    let source_placed = place(
        owner,
        permit,
        control,
        parent,
        layout,
        &layout.source,
        source,
    )
    .await?;
    if !source_placed {
        ensure_loaded(
            owner,
            permit,
            control,
            parent,
            layout,
            &layout.source,
            source,
        )
        .await?;
    }
    let source_confirmed = allow_overlap
        && source_placed
        && source != destination
        && permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
        && owner.devices[source].dev.session_epoch().is_some()
        && confirmed_copy_move(owner, control, parent, layout, &layout.source).await?;
    // The source MOVE is durably confirmed now. Its drive may become ready
    // while the same changer performs the destination's separate MOVE.
    let destination_placed = place(
        owner,
        permit,
        control,
        parent,
        layout,
        &layout.destination,
        destination,
    )
    .await?;
    if source_placed {
        ensure_loaded(
            owner,
            permit,
            control,
            parent,
            layout,
            &layout.source,
            source,
        )
        .await?;
    }
    let stayed_loaded = !source_placed && !destination_placed;
    if source_confirmed
        && destination_placed
        && permit.allows(owner.round, crate::daemon::permission::Phase::Serving)
        && owner.devices[source].dev.session_epoch().is_some()
        && confirmed_copy_move(owner, control, parent, layout, &layout.destination).await?
    {
        let load = layout.plan(&layout.destination, ReclaimMediaAction::Load);
        match actor(owner, permit, control, destination)?
            .observe_load_preissue(parent, &load)
            .await
            .map_err(lost)?
        {
            LoadPreIssue::Ready => {
                return Ok(CopyMediaReady {
                    source,
                    stayed_loaded,
                    capture: None,
                });
            }
            LoadPreIssue::BecomingReady(reason) => {
                let capture = Box::pin(wait_destination_with_source_capture(
                    owner,
                    permit,
                    control,
                    parent,
                    layout,
                    source,
                    destination,
                    reason,
                ))
                .await?;
                if capture.is_some() {
                    return Ok(CopyMediaReady {
                        source,
                        stayed_loaded,
                        capture,
                    });
                }
                fdb_loading::authority_guard(owner, permit, control).await?;
            }
            LoadPreIssue::NeedsLoad | LoadPreIssue::Pending => {}
        }
    }
    ensure_loaded(
        owner,
        permit,
        control,
        parent,
        layout,
        &layout.destination,
        destination,
    )
    .await?;
    Ok(CopyMediaReady {
        source,
        stayed_loaded,
        capture: None,
    })
}
// This context is a readonly, parent-bound catalog snapshot. It is not
// metadata admission and does not replace the original prepare transaction.
fn capture_destination_with_history_context<'a>(
    owner: &'a Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    context: &'a crate::managed_catalog::DestinationHistoryContext,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<Output = std::result::Result<CapturedIndexSource, ReadError>> + 'a,
    >,
> {
    Box::pin(async move {
        let drive = device(owner, &layout.destination.drive_serial)?;
        let budget = budget(owner, control);
        let hint = control
            .catalog
            .load_reclaim_destination_history_hint(
                parent,
                owner.round,
                context,
                &budget.index_scratch,
            )
            .await
            .map_err(parent_error)?;
        let checked =
            fdb_loading::operation_guard(owner, permit, owner.devices[drive].dev.as_ref(), || {
                futures_executor::block_on(
                    control.catalog.require_reclaim_destination_history_context(
                        parent,
                        owner.round,
                        context,
                    ),
                )
                .map_err(|error| TapeError::Catalog(error.to_string()))
            });
        let label = crate::ltfs::volume::read_label_only(&checked).map_err(lost)?;
        if label.volume_uuid != layout.destination.volume {
            return Err(lost("原目的标签改变"));
        }
        let captured = crate::ltfs::recovery::capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&checked),
            &label,
            &budget,
            hint,
        )
        .map_err(lost)?;
        checked.verify_proof().map_err(lost)?;
        if control
            .catalog
            .publish_reclaim_destination_history_hint(parent, owner.round, context, &captured)
            .await
            .map_err(parent_error)?
        {
            control
                .catalog
                .collect_reclaim_destination_history(parent, owner.round, context)
                .await
                .map_err(parent_error)?;
        }
        Ok(captured)
    })
}

enum PreCopyOutcome {
    ContinueCopy,
    Handled,
}

// Keep mutually exclusive FORMAT/obsolete work out of normal copying's frame.
fn advance_existing_format_or_obsolete<'a>(
    owner: &'a mut Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    previous_token: Option<&'a [u8; 32]>,
) -> std::pin::Pin<
    Box<impl std::future::Future<Output = std::result::Result<PreCopyOutcome, ReadError>> + 'a>,
> {
    Box::pin(async move {
        if !control.catalog.reclaim_group_only_precopy_ready(parent,owner.round).await.map_err(parent_error)? {
            if control.catalog.reclaim_format_plan(parent,owner.round).await.map_err(parent_error)?.is_some() {
                return Err(lost("group-only源format检查点缺少原Ready资格，禁止恢复格式化"));
            }
            return Ok(PreCopyOutcome::ContinueCopy);
        }
        if let Some(plan) = control
            .catalog
            .reclaim_format_plan(parent, owner.round)
            .await
            .map_err(parent_error)?
        {
            finish(owner, permit, control, parent, layout, plan).await?;
            return Ok(PreCopyOutcome::Handled);
        }
        if let Some(progress) = control
            .catalog
            .step_reclaim_obsolete_copies(parent, owner.round)
            .await
            .map_err(parent_error)?
        {
            if !progress.complete() {
                return Ok(PreCopyOutcome::Handled);
            }
            if control
                .catalog
                .reclaim_source_has_retained_references(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                control.catalog.set_reclaim_dependency(parent, owner.round, Some("源卷仍有历史版本、扫描或其他命名空间引用，等待按原保留策略合法释放；不会清除未知 V 或初始化介质")).await.map_err(parent_error)?;
                return Ok(PreCopyOutcome::Handled);
            }
            if let Some(token) = previous_token {
                control
                    .catalog
                    .clear_reclaim_dependency_if_unchanged(parent, owner.round, *token)
                    .await
                    .map_err(parent_error)?;
            }
            let drive = load(owner, permit, control, parent, layout, &layout.source).await?;
            let proof = reader(owner, permit, control, drive)
                .read_reclaim_format_source(parent, owner.files.spool_dir())
                .await
                .map_err(lost)?;
            control
                .catalog
                .prepare_reclaim_format(parent, owner.round, uuid::Uuid::new_v4(), &proof)
                .await
                .map_err(parent_error)?;
            return Ok(PreCopyOutcome::Handled);
        }
        Ok(PreCopyOutcome::ContinueCopy)
    })
}

// Construct the parent's large phase future outside the caller's poll frame.
pub(in crate::daemon::executor) fn advance<'a>(
    owner: &'a mut Active,
    permit: &'a Arc<Permission>,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    Box::pin(async move {
        // A failed claim or any early phase drops the old capture. It cannot
        // survive an unverified round just because the Active object survives.
        let previous_format_source = owner.fdb_reclaim_format_source.take();
        let page = control
            .catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, owner.round, None, 1)
            .await
            .map_err(parent_error)?;
        let Some(parent) = page.items.first() else {
            owner.reclaim_copy_stall = None;
            return Ok(false);
        };
        // Preserve the original recovery checkpoints for real dependencies,
        // while leaving this execution's stable stall observation untouched.
        // Bind each clear to the complete record seen before this round's work.
        let previous_dependency = control
            .catalog
            .reclaim_dependency_snapshot(parent, owner.round)
            .await
            .map_err(parent_error)?
            .filter(|(reason, record_owner, _)| {
                reason != COPY_STALL_REASON || *record_owner != owner.round
            });
        let result = async {
            let epoch = parent.intent()["epoch"]
                .as_u64()
                .ok_or_else(|| lost("原父代次缺失"))?;
            let operation = uuid::Uuid::parse_str(
                parent.intent()["operation"]
                    .as_str()
                    .ok_or_else(|| lost("原父请求缺失"))?,
            )
            .map_err(lost)?;
            control
                .catalog
                .start_reclaim_management(
                    &control.serial,
                    control.route,
                    owner.round,
                    epoch,
                    operation,
                )
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("原父任务与异步回执不同"))?;
            if let Some(reason)=control.catalog.reclaim_group_only_status(parent,owner.round).await.map_err(parent_error)? {
                return Err(lost(format!("UnsupportedGroupOnly：未执行迁移，原请求保留：{reason}")));
            }
            match control
                .catalog
                .classify_reclaim_execution(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                crate::managed_catalog::ReclaimExecutionGate::Fresh => {}
                crate::managed_catalog::ReclaimExecutionGate::Blocked { reason } => {
                    return Err(ReadError::Busy(reason));
                }
                crate::managed_catalog::ReclaimExecutionGate::KnownFullCandidate { group } => {
                    let observation = control
                        .catalog
                        .begin_original_reclaim_observation(parent, owner.round, group)
                        .await
                        .map_err(parent_error)?;
                    let result = async {
                        // Stored layout lookup only: never inventory, LOAD, source
                        // capture or local cache on this limited recovery lane.
                        let layout = control
                            .catalog
                            .reclaim_layout(parent, owner.round)
                            .await
                            .map_err(parent_error)?
                            .ok_or_else(|| lost("原KnownFull布局缺失"))?;
                        let completed = checkpoint::resume_original(
                            owner,
                            permit,
                            control,
                            parent,
                            &layout,
                            group,
                            Some(&observation),
                        )
                        .await?;
                        for readback in &completed.readbacks {
                            let member = readback.context();
                            control
                                .catalog
                                .settle_reclaim_group_member(
                                    parent,
                                    owner.round,
                                    member.group_id(),
                                    member.attempt_id(),
                                    member.ordinal(),
                                    readback,
                                )
                                .await
                                .map_err(parent_error)?;
                        }
                        control
                            .catalog
                            .close_reclaim_copy_group(parent, owner.round, group)
                            .await
                            .map_err(parent_error)?;
                        control
                            .catalog
                            .finish_original_reclaim_observation(&observation)
                            .await
                            .map_err(parent_error)?;
                        Ok::<(), ReadError>(())
                    }
                    .await;
                    if let Err(error) = result {
                        // Failed persistence leaves ObservationStarted, which is
                        // equally closed to future automatic probe/append.
                        let _ = control
                            .catalog
                            .block_original_reclaim_observation(&observation, &error.to_string())
                            .await;
                        return Err(error);
                    }
                    return Ok(true);
                }
            }
            let resources = control
                .catalog
                .active_logical_pool_resources(
                    &control.serial,
                    control.route,
                    owner.round,
                    parent.spec().pool(),
                )
                .await
                .map_err(parent_error)?;
            if resources.as_ref().is_some_and(|r| r.drives.len() < 2)
                && control
                    .catalog
                    .reclaim_layout(parent, owner.round)
                    .await
                    .map_err(parent_error)?
                    .is_none()
            {
                return Err(ReadError::Busy(
                    "逻辑带库至少需要两个维护驱动器才能回收".into(),
                ));
            }
            fdb_loading::authority_guard(owner, permit, control).await?;
            let previous_source = owner.fdb_reclaim_source_session.take();
            control
                .catalog
                .reap_reclaim_parent_history_pins(parent, owner.round)
                .await
                .map_err(parent_error)?;
            if let Some(command) = control
                .catalog
                .reclaim_media_pending(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                let drive = device(owner, &command.plan().drive_serial)?;
                if !actor(owner, permit, control, drive)?
                    .observe(parent, &command)
                    .await
                    .map_err(lost)?
                {
                    return Err(ReadError::Busy(
                        "原 reclaim 机械命令结果未知，保留原 issue 等待观察".into(),
                    ));
                }
                if let Some((_, _, token)) = &previous_dependency {
                    control
                        .catalog
                        .clear_reclaim_dependency_if_unchanged(parent, owner.round, *token)
                        .await
                        .map_err(parent_error)?;
                }
                return Ok(true);
            }
            let layout = layout(owner, permit, control, parent, resources.as_ref()).await?;
            if let Some((_, _, token)) = &previous_dependency {
                control
                    .catalog
                    .clear_reclaim_dependency_if_unchanged(parent, owner.round, *token)
                    .await
                    .map_err(parent_error)?;
            }
            match advance_existing_format_or_obsolete(
                owner,
                permit,
                control,
                parent,
                &layout,
                previous_dependency.as_ref().map(|(_, _, token)| token),
            )
            .await?
            {
                PreCopyOutcome::Handled => return Ok(true),
                PreCopyOutcome::ContinueCopy => {}
            }
            let CopyMediaReady {
                source: source_drive,
                stayed_loaded,
                capture,
            } = Box::pin(load_copy_media(
                owner,
                permit,
                control,
                parent,
                &layout,
                previous_source.is_none(),
            ))
            .await?;
            let prepared_format_source = previous_format_source.filter(|prepared| {
                stayed_loaded
                    && prepared.matches_loaded_source(owner, parent, &layout, source_drive)
            });
            let session = match capture {
                Some(session) => session,
                None => reader(owner, permit, control, source_drive)
                    .capture_reclaim_source_session(
                        parent,
                        owner.files.spool_dir(),
                        previous_source,
                    )
                    .await
                    .map_err(lost)?,
            };
            let source = session.captured();
            let inventory = crate::managed_catalog::ReclaimSourceInventory::from_captured(source)
                .map_err(parent_error)?;
            let progress = control
                .catalog
                .seal_reclaim_source_inventory_page(parent, owner.round, &inventory)
                .await
                .map_err(parent_error)?;
            observe_copy_progress(owner, control, parent, &progress).await?;
            if progress.complete() {
                qualification::prepare(owner,control,parent,&inventory).await?;
                copying::step(
                    owner,
                    permit,
                    control,
                    status,
                    parent,
                    &layout,
                    (source, &inventory),
                    prepared_format_source,
                )
                .await?;
            }
            let retained = reader(owner, permit, control, source_drive)
                .finish_reclaim_source_session(parent, session)
                .await
                .map_err(lost)?;
            owner.fdb_reclaim_source_session = retained;
            Ok(true)
        }
        .await;
        if let Err(error) = &result {
            owner.fdb_reclaim_format_source = None;
            // Preserve the original task and facts even when its dependency
            // fails. A diagnostic write failure is itself an unknown result.
            let mut reason: String = error.to_string().chars().take(600).collect();
            while reason.len() > 1800 {
                reason.pop();
            }
            control
                .catalog
                .record_reclaim_failure(parent, owner.round, &reason)
                .await
                .map_err(|record| lost(format!("{error}; 依赖状态保存失败: {record}")))?;
        }
        result
    })
}

async fn finish(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
    plan: crate::managed_catalog::ReclaimFormatPlan,
) -> std::result::Result<(), ReadError> {
    let source = load(owner, permit, control, parent, layout, &layout.source).await?;
    let admission = control
        .catalog
        .admit_reclaim_format(parent, owner.round, &plan)
        .await
        .map_err(parent_error)?;
    let plan = admission.plan().clone();
    let budget = budget(owner, control);
    let mut formatter = crate::daemon::fdb_reclaim_format::ReclaimFormatter {
        catalog: &control.catalog,
        device: owner.devices[source].dev.as_ref(),
        route: control.route,
        key: owner.key,
        permission: permit,
        budget: &budget,
    };
    let proof = match formatter.execute(parent, admission).await.map_err(lost)? {
        crate::daemon::fdb_reclaim_format::ReclaimFormatOutcome::Committed(proof) => proof,
        crate::daemon::fdb_reclaim_format::ReclaimFormatOutcome::Retry(proof) => {
            control
                .catalog
                .rearm_reclaim_format(parent, owner.round, &plan, &proof)
                .await
                .map_err(parent_error)?;
            return Ok(());
        }
    };
    let home = if parent.spec().unassign() {
        let actor = actor(owner, permit, control, source)?;
        actor
            .execute(
                parent,
                &layout.plan(&layout.source, ReclaimMediaAction::Unload),
            )
            .await
            .map_err(lost)?;
        let mut movement = layout.plan(&layout.source, ReclaimMediaAction::Move);
        movement.source = layout.source.address;
        movement.destination = layout.source.home;
        actor.execute(parent, &movement).await.map_err(lost)?;
        Some(
            actor
                .formatted_source_home(parent, &plan, &proof)
                .await
                .map_err(lost)?,
        )
    } else {
        None
    };
    control
        .catalog
        .complete_reclaim_format(parent, owner.round, &plan, &proof, home.as_ref())
        .await
        .map_err(parent_error)?;
    owner.fdb_reads.remove(&source);
    owner.fdb_read_session = None;
    owner.write_session = None;
    Ok(())
}

#[cfg(test)]
mod progress_tests {
    use super::unchanged_rounds;

    #[test]
    fn stall_counts_only_unchanged_durable_progress_of_the_same_parent() {
        let mut saved = None;
        assert_eq!(unchanged_rounds(&mut saved, "parent", [0; 32]), 0);
        for expected in 1..=250 {
            assert_eq!(unchanged_rounds(&mut saved, "parent", [0; 32]), expected);
        }
        for progress in 1..=250 {
            assert_eq!(
                unchanged_rounds(&mut saved, "parent", [progress; 32]),
                0,
                "每次真实推进均须重置，而非累计进入 copying 次数"
            );
        }
        assert_eq!(unchanged_rounds(&mut saved, "next-parent", [250; 32]), 0);
        assert_eq!(unchanged_rounds(&mut saved, "next-parent", [250; 32]), 1);
    }
}
