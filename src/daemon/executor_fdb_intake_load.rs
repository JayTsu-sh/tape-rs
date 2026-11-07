//! Fixed original-import mechanical plan. An issued command is observed, never replayed.
use super::*;
use crate::managed_catalog::{
    NativeMediaEvictionPlan, NativeMediaImport, NativeMediaLoadPhase as Phase,
    NativeMediaLoadPlan as Plan, NativeMediaLoadProgress as Progress,
};

fn original_drive(owner: &Active, serial: &str) -> std::result::Result<usize, ReadError> {
    owner
        .devices
        .iter()
        .position(|d| d.kind == DeviceKind::Drive && d.serial == serial)
        .ok_or_else(|| lost("原导入驱动器已不在当前配置中"))
}
fn target_at(inventory: &Inventory, plan: &Plan, barcode: &str, drive: usize) -> bool {
    inventory.drives.iter().any(|slot| {
        slot.dev == drive
            && slot.addr == plan.destination
            && slot
                .loaded
                .as_ref()
                .is_some_and(|(loaded, _)| loaded == barcode)
    }) && (plan.source == plan.destination || inventory.empty_slots.contains(&plan.source))
}
fn eviction_done(inventory: &Inventory, eviction: &NativeMediaEvictionPlan, drive: usize) -> bool {
    inventory.slots.get(&eviction.barcode) == Some(&eviction.home)
        && inventory
            .drives
            .iter()
            .any(|slot| slot.dev == drive && slot.addr == eviction.source && slot.loaded.is_none())
}

async fn choose(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    target: Target<'_>,
) -> std::result::Result<Plan, ReadError> {
    let scope = IoScope::Target(target);
    let inventory = checked_inventory(owner, permit, control, scope)?;
    let barcode = target.placement.barcode.as_str();
    let pool = target.placement.pool_id;
    let management = policy(owner, control, &[pool], Some(barcode)).await?;
    if !tape_allowed(&management, barcode) || inventory.cleaning.contains(barcode) {
        return Err(ReadError::Busy("导入磁带离线、在黑名单中或为清洗带".into()));
    }
    let allowed = |drive: usize| {
        super::super::super::management::drive_allowed(
            &management,
            &owner.devices[drive].serial,
            'r',
        )
    };
    if let Some(slot) = inventory.drives.iter().find(|slot| {
        slot.loaded
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == barcode)
    }) {
        if !allowed(slot.dev) {
            return Err(ReadError::Busy("原导入驱动器没有读取资格".into()));
        }
        let home = slot
            .loaded
            .as_ref()
            .and_then(|(_, home)| *home)
            .filter(|home| inventory.empty_slots.contains(home));
        return Ok(Plan {
            source: slot.addr,
            destination: slot.addr,
            drive_serial: owner.devices[slot.dev].serial.clone(),
            home,
            eviction: None,
        });
    }
    let source = *inventory
        .slots
        .get(barcode)
        .ok_or_else(|| ReadError::Busy("导入磁带不在可调度存储槽位中".into()))?;
    let mut compatible = Vec::new();
    for slot in inventory.drives.iter().filter(|slot| allowed(slot.dev)) {
        let checked = checked_device(owner, permit, control, slot.dev, scope);
        let result =
            crate::scsi::transport::retry_unit_attention("FDB 导入密度能力查询", || {
                TapeDrive::new(&checked).report_density_support(false)
            });
        checked.verify_proof().map_err(lost)?;
        match result {
            Ok(raw)
                if super::super::super::scheduling::densities(&raw).is_some_and(|densities| {
                    super::super::super::scheduling::compatible(&densities, barcode, false)
                }) =>
            {
                compatible.push(slot.dev)
            }
            Err(error) if ownership_lost(&error) => return Err(lost(error)),
            Err(error) => warn!("FDB 导入跳过未知密度能力驱动器: {error}"),
            _ => {}
        }
    }
    let limit = management["pools"][pool.to_string()]["mountlimit"]
        .as_str()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| lost("FDB 池挂载上限缺失"))?;
    let mut occupied = 0;
    if limit != 0 {
        for slot in &inventory.drives {
            if let Some((loaded, _)) = &slot.loaded
                && let Some(assignment) = control
                    .catalog
                    .tape_assignment(&control.serial, loaded)
                    .await
                    .map_err(lost)?
            {
                if assignment.namespace != control.route.route.namespace {
                    return Err(lost("装载磁带属于另一个命名空间"));
                }
                occupied += usize::from(assignment.pool_id == pool);
            }
        }
    }
    if limit != 0 && occupied > limit {
        return Err(ReadError::Busy(
            "池的实际挂载数超过当前上限；等待卸带或调整挂载上限后导入".into(),
        ));
    }
    let at_limit = limit != 0 && occupied >= limit;
    let empty = inventory
        .drives
        .iter()
        .find(|slot| slot.loaded.is_none() && compatible.contains(&slot.dev))
        .map(|slot| slot.dev);
    let (drive, eviction) = if let Some(drive) = empty.filter(|_| !at_limit) {
        (drive, None)
    } else {
        let (victim, mount) = owner
            .fdb_reads
            .iter()
            .filter(|(drive, mount)| {
                (empty.is_some() && at_limit || compatible.contains(drive))
                    && (!at_limit || mount.pool == pool)
                    && tape_allowed(&management, &mount.barcode)
                    && allowed(**drive)
            })
            .min_by_key(|(_, mount)| mount.last_used)
            .ok_or_else(|| ReadError::Busy("没有兼容空驱动器或可卸下的本轮受管读带".into()))?;
        let slot = inventory
            .drives
            .iter()
            .find(|slot| slot.dev == *victim)
            .ok_or_else(|| lost("退带驱动器不在库存中"))?;
        let (_, home) = slot
            .loaded
            .as_ref()
            .filter(|(loaded, _)| loaded == &mount.barcode)
            .ok_or_else(|| lost("退带原卷与实际条码不一致"))?;
        let home = home
            .filter(|h| inventory.empty_slots.contains(h))
            .or_else(|| inventory.empty_slots.first().copied())
            .ok_or_else(|| ReadError::Busy("没有空存储槽位可卸带".into()))?;
        (
            empty.unwrap_or(*victim),
            Some(NativeMediaEvictionPlan {
                drive_serial: owner.devices[*victim].serial.clone(),
                source: slot.addr,
                barcode: mount.barcode.clone(),
                volume: mount.volume,
                pool: mount.pool,
                home,
            }),
        )
    };
    let destination = inventory
        .drives
        .iter()
        .find(|slot| slot.dev == drive)
        .ok_or_else(|| lost("原目的驱动器库存缺失"))?
        .addr;
    Ok(Plan {
        source,
        destination,
        drive_serial: owner.devices[drive].serial.clone(),
        home: Some(source),
        eviction,
    })
}

pub(super) async fn select(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    _status: &SharedStatus,
    target: Target<'_>,
) -> std::result::Result<usize, ReadError> {
    let intake = target.intake.expect("original intake");
    intake_guard(owner, permit, control, Some(intake)).await?;
    let mut progress = match control
        .catalog
        .native_media_load_progress(intake, owner.round)
        .await
        .map_err(lost)?
    {
        Some(progress) => progress,
        None => {
            let plan = choose(owner, permit, control, target).await?;
            control
                .catalog
                .prepare_native_media_load(intake, owner.round, &plan)
                .await
                .map_err(lost)?
        }
    };
    let drive = original_drive(owner, &progress.plan.drive_serial)?;
    let scope = IoScope::Target(target);
    if let Some(eviction) = progress.plan.eviction.as_ref()
        && progress.phase == Phase::Planned
    {
        if control
            .catalog
            .issue_native_media_eviction(intake, owner.round)
            .await
            .map_err(lost)?
        {
            let victim = original_drive(owner, &eviction.drive_serial)?;
            if !owner.fdb_reads.contains_key(&victim) {
                // Reconstruct only a ready, identified original mount; never LOAD to retire it.
                let label = drive_io(owner, permit, control, scope, victim, |device| {
                    if !TapeDrive::new(device).test_unit_ready()? {
                        return Err(TapeError::Refused {
                            reason: "原退带卷未就绪，禁止重发 LOAD".into(),
                        });
                    }
                    crate::ltfs::volume::read_label_only(device)
                })?;
                if label.volume_uuid != eviction.volume {
                    return Err(lost("原退带卷 UUID 不符"));
                }
                owner.fdb_reads.insert(
                    victim,
                    Mount {
                        barcode: eviction.barcode.clone(),
                        volume: eviction.volume,
                        pool: eviction.pool,
                        last_used: Instant::now(),
                    },
                );
            }
            unload_inner(owner, permit, control, victim, &['r'], None, Some(intake)).await?;
        }
        progress.phase = Phase::EvictionIssued;
    }
    if progress.phase == Phase::EvictionIssued {
        let eviction = progress
            .plan
            .eviction
            .as_ref()
            .ok_or_else(|| lost("原退带计划缺失"))?;
        let victim = original_drive(owner, &eviction.drive_serial)?;
        if !eviction_done(
            &checked_inventory(owner, permit, control, scope)?,
            eviction,
            victim,
        ) {
            return Err(lost(
                "原退带结果尚未确认；保留原任务，不重发 UNLOAD 或 MOVE",
            ));
        }
        control
            .catalog
            .confirm_native_media_evicted(intake, owner.round)
            .await
            .map_err(lost)?;
        owner.fdb_reads.remove(&victim);
        progress.phase = Phase::Evicted;
    }
    if matches!(progress.phase, Phase::Planned | Phase::Evicted) {
        let before = checked_inventory(owner, permit, control, scope)?;
        if progress.plan.source == progress.plan.destination {
            if !target_at(&before, &progress.plan, intake.barcode(), drive) {
                return Err(lost("原驱动器的导入条码或地址改变"));
            }
        } else {
            if before.slots.get(intake.barcode()) != Some(&progress.plan.source)
                || !before.drives.iter().any(|slot| {
                    slot.dev == drive
                        && slot.addr == progress.plan.destination
                        && slot.loaded.is_none()
                })
            {
                return Err(lost("原导入源槽位或目的驱动器改变"));
            }
            if control
                .catalog
                .issue_native_media_move(intake, owner.round)
                .await
                .map_err(lost)?
            {
                checked_move(
                    owner,
                    permit,
                    control,
                    scope,
                    progress.plan.source,
                    progress.plan.destination,
                )?;
            }
            if !target_at(
                &checked_inventory(owner, permit, control, scope)?,
                &progress.plan,
                intake.barcode(),
                drive,
            ) {
                return Err(lost("原导入 MOVE 结果尚未确认"));
            }
        }
        control
            .catalog
            .confirm_native_media_moved(intake, owner.round)
            .await
            .map_err(lost)?;
        progress.phase = Phase::Moved;
    }
    if progress.phase == Phase::MoveIssued {
        if !target_at(
            &checked_inventory(owner, permit, control, scope)?,
            &progress.plan,
            intake.barcode(),
            drive,
        ) {
            return Err(lost("原 MOVE 结果未知；只观察原地址，不重发命令"));
        }
        control
            .catalog
            .confirm_native_media_moved(intake, owner.round)
            .await
            .map_err(lost)?;
        progress.phase = Phase::Moved;
    }
    if !target_at(
        &checked_inventory(owner, permit, control, scope)?,
        &progress.plan,
        intake.barcode(),
        drive,
    ) {
        return Err(lost("原导入条码、驱动器或地址改变"));
    }
    let ready = drive_io(owner, permit, control, scope, drive, |device| {
        TapeDrive::new(device).test_unit_ready()
    })?;
    if progress.phase == Phase::Moved && !ready {
        if control
            .catalog
            .issue_native_media_load(intake, owner.round)
            .await
            .map_err(lost)?
        {
            drive_io(owner, permit, control, scope, drive, |device| {
                TapeDrive::new(device).load()?;
                wait_ready(device)
            })?;
        } else if !drive_io(owner, permit, control, scope, drive, |device| {
            TapeDrive::new(device).test_unit_ready()
        })? {
            return Err(lost("原 LOAD 结果未知；禁止重发"));
        }
    } else if !ready {
        return Err(lost("原 LOAD 尚未确认就绪；只观察，不重发命令"));
    }
    control
        .catalog
        .confirm_native_media_loaded(intake, owner.round)
        .await
        .map_err(lost)?;
    intake_guard(owner, permit, control, Some(intake)).await?;
    Ok(drive)
}

/// Cancellation releases no gate until every issued mechanical action has a
/// positive read-only postcondition. No LOAD/UNLOAD/MOVE is permitted here.
pub(crate) async fn settle_cancelled(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    intake: &NativeMediaImport,
) -> std::result::Result<(), ReadError> {
    let Some(Progress { plan, phase }) = control
        .catalog
        .native_media_load_progress(intake, owner.round)
        .await
        .map_err(lost)?
    else {
        return Ok(());
    };
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    let scope = IoScope::IntakeObservation(intake);
    match phase {
        Phase::EvictionIssued => {
            let eviction = plan
                .eviction
                .as_ref()
                .ok_or_else(|| lost("原退带计划缺失"))?;
            if !eviction_done(
                &checked_inventory(owner, permit, control, scope)?,
                eviction,
                original_drive(owner, &eviction.drive_serial)?,
            ) {
                return Err(lost("取消等待原退带结果；保留机械占用"));
            }
            control
                .catalog
                .confirm_native_media_evicted(intake, owner.round)
                .await
                .map_err(lost)?;
        }
        Phase::MoveIssued => {
            if !target_at(
                &checked_inventory(owner, permit, control, scope)?,
                &plan,
                intake.barcode(),
                original_drive(owner, &plan.drive_serial)?,
            ) {
                return Err(lost("取消等待原 MOVE 结果；保留机械占用"));
            }
            control
                .catalog
                .confirm_native_media_moved(intake, owner.round)
                .await
                .map_err(lost)?;
        }
        Phase::LoadIssued => {
            let drive = original_drive(owner, &plan.drive_serial)?;
            if !target_at(
                &checked_inventory(owner, permit, control, scope)?,
                &plan,
                intake.barcode(),
                drive,
            ) || !drive_io(owner, permit, control, scope, drive, |device| {
                TapeDrive::new(device).test_unit_ready()
            })? {
                return Err(lost("取消等待原 LOAD 就绪；保留机械占用"));
            }
            control
                .catalog
                .confirm_native_media_loaded(intake, owner.round)
                .await
                .map_err(lost)?;
        }
        _ => {}
    }
    Ok(())
}
