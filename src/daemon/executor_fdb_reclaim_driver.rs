//! Parent scheduling over the original FDB task, fixed physical layout and
//! independent migration/initialization receipts. No state is reconstructed
//! from a newer logical path after a takeover.
use super::*;
#[path = "executor_fdb_reclaim_copy.rs"]
mod copying;
#[path = "executor_fdb_reclaim_metadata.rs"]
mod metadata;
#[path = "executor_fdb_reclaim_upload_recovery.rs"]
mod upload_recovery;
use crate::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use crate::daemon::fdb_read::ActiveMediaReader;
use crate::daemon::fdb_reclaim_mechanics::ReclaimMechanics;
use crate::ltfs::recovery::{CapturedIndexSource, RecoveryBudget};
use crate::managed_catalog::{
    ReclaimDriveBinding, ReclaimLayout, ReclaimMediaAction, ReclaimReservation,
};

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
) -> Option<ReclaimDriveBinding> {
    let media: Vec<_> = rows
        .iter()
        .filter(|r| r.full && r.volume_tag.as_deref() == Some(assignment.barcode.as_str()))
        .collect();
    if media.len() != 1 {
        return None;
    }
    let media = media[0];
    let drive = if media.element_type == ElementType::DataTransfer {
        (Some(media.address) != avoid).then_some(media)?
    } else if media.element_type == ElementType::Storage {
        rows.iter().find(|r| {
            r.element_type == ElementType::DataTransfer
                && !r.full
                && Some(r.address) != avoid
                && r.drive_serial().is_some_and(|id| {
                    owner
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
        home: if media.element_type == ElementType::Storage {
            media.address
        } else {
            media.source_address?
        },
    })
}
async fn layout(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
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
            let Some(src) = binding(owner, &rows, &source, target_loaded) else {
                continue;
            };
            let Some(dst) = binding(owner, &rows, &target, Some(src.address)) else {
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
            if ![&src, &dst].iter().all(|b| {
                super::super::super::management::drive_allowed(&management, &b.drive_serial, 'm')
            }) {
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
                "reclaim 等待符合介质/健康/挂载上限策略的同池目的带及两个独立可用驱动器".into(),
            ));
        }
    }
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
    let (map, rows) = inventory(owner, permit, control, parent)?;
    if map != layout.map {
        return Err(lost("原固定 address map 改变"));
    }
    let media = rows
        .iter()
        .find(|r| r.full && r.volume_tag.as_deref() == Some(binding.barcode.as_str()))
        .ok_or_else(|| lost("原固定介质不在库内"))?;
    let actor = actor(owner, permit, control, drive)?;
    if media.element_type == ElementType::Storage && media.address == binding.home {
        let mut movement = layout.plan(binding, ReclaimMediaAction::Move);
        movement.source = binding.home;
        movement.destination = binding.address;
        actor.execute(parent, &movement).await.map_err(lost)?;
    } else if media.element_type != ElementType::DataTransfer || media.address != binding.address {
        return Err(lost("原固定介质已不在 home 或原驱动器"));
    }
    actor
        .execute(parent, &layout.plan(binding, ReclaimMediaAction::Load))
        .await
        .map_err(lost)?;
    owner.fdb_reads.remove(&drive);
    owner.fdb_read_session = None;
    owner.write_session = None;
    Ok(drive)
}
fn capture_destination(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
) -> std::result::Result<CapturedIndexSource, ReadError> {
    let drive = device(owner, &layout.destination.drive_serial)?;
    let checked =
        fdb_loading::operation_guard(owner, permit, owner.devices[drive].dev.as_ref(), || {
            futures_executor::block_on(control.catalog.require_reclaim_read_lane(
                parent,
                owner.round,
                layout.destination.volume,
            ))
            .map_err(|e| TapeError::Catalog(e.to_string()))
        });
    let label = crate::ltfs::volume::read_label_only(&checked).map_err(lost)?;
    if label.volume_uuid != layout.destination.volume {
        return Err(lost("原目的标签改变"));
    }
    let captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&checked),
        &label,
        &budget(owner, control),
    )
    .map_err(lost)?;
    checked.verify_proof().map_err(lost)?;
    Ok(captured)
}

// Construct the parent's large phase future outside the caller's poll frame.
pub(in crate::daemon::executor) fn advance<'a>(
    owner: &'a mut Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    Box::pin(async move {
        let page = control
            .catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, owner.round, None, 1)
            .await
            .map_err(parent_error)?;
        let Some(parent) = page.items.first() else {
            return Ok(false);
        };
        let result = async {
            fdb_loading::authority_guard(owner, permit, control).await?;
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
                control.catalog.set_reclaim_dependency(parent, owner.round, None).await.map_err(parent_error)?;
                return Ok(true);
            }
            let layout = layout(owner, permit, control, parent).await?;
            control.catalog.set_reclaim_dependency(parent, owner.round, None).await.map_err(parent_error)?;
            if let Some(plan) = control
                .catalog
                .reclaim_format_plan(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                finish(owner, permit, control, parent, &layout, plan).await?;
                return Ok(true);
            }
            if let Some(progress) = control
                .catalog
                .step_reclaim_obsolete_copies(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                if !progress.complete() {
                    return Ok(true);
                }
                if control
                    .catalog
                    .reclaim_source_has_retained_references(parent, owner.round)
                    .await
                    .map_err(parent_error)?
                {
                    control.catalog.set_reclaim_dependency(parent, owner.round, Some("源卷仍有历史版本、扫描或其他命名空间引用，等待按原保留策略合法释放；不会清除未知 V 或初始化介质")).await.map_err(parent_error)?;
                    return Ok(true);
                }
                control
                    .catalog
                    .set_reclaim_dependency(parent, owner.round, None)
                    .await
                    .map_err(parent_error)?;
                let drive = load(owner, permit, control, parent, &layout, &layout.source).await?;
                let proof = reader(owner, permit, control, drive)
                    .read_reclaim_format_source(parent, owner.files.spool_dir())
                    .await
                    .map_err(lost)?;
                control
                    .catalog
                    .prepare_reclaim_format(parent, owner.round, uuid::Uuid::new_v4(), &proof)
                    .await
                    .map_err(parent_error)?;
                return Ok(true);
            }
            let source_drive = load(owner, permit, control, parent, &layout, &layout.source).await?;
            load(owner, permit, control, parent, &layout, &layout.destination).await?;
            let source = reader(owner, permit, control, source_drive)
                .capture_reclaim_source(parent, owner.files.spool_dir())
                .await
                .map_err(lost)?;
            let inventory = crate::managed_catalog::ReclaimSourceInventory::from_captured(&source)
                .map_err(parent_error)?;
            if !control
                .catalog
                .seal_reclaim_source_inventory_page(parent, owner.round, &inventory)
                .await
                .map_err(parent_error)?
                .complete()
            {
                return Ok(true);
            }
            copying::step(
                owner,
                permit,
                control,
                status,
                parent,
                &layout,
                (&source, &inventory),
            )
            .await?;
            Ok(true)
        }.await;
        if let Err(error) = &result {
            // Preserve the original task and facts even when its dependency
            // fails. A diagnostic write failure is itself an unknown result.
            let mut reason: String = error.to_string().chars().take(600).collect();
            while reason.len() > 1800 {
                reason.pop();
            }
            if let Err(record) = control
                .catalog
                .set_reclaim_dependency(parent, owner.round, Some(&reason))
                .await
            {
                return Err(lost(format!("{error}; 依赖状态保存失败: {record}")));
            }
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
