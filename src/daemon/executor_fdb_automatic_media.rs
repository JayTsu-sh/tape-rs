//! Automatic data-path mechanics retain their original command across requests.
use super::*;
use crate::managed_catalog::{
    AutomaticMediaAction as Action, AutomaticMediaCommand as Command, AutomaticMediaPlan as Plan,
};

fn drive_for(owner: &Active, command: &Command) -> std::result::Result<usize, ReadError> {
    owner
        .devices
        .iter()
        .position(|device| {
            device.kind == DeviceKind::Drive && device.serial == command.plan.drive_serial
        })
        .ok_or_else(|| lost("原自动机械驱动器不在当前配置中"))
}
fn postcondition(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    command: &Command,
) -> std::result::Result<bool, ReadError> {
    let scope = IoScope::Automatic {
        command,
        original: None,
    };
    let inventory = checked_inventory(owner, permit, control, scope)?;
    let drive = drive_for(owner, command)?;
    let plan = &command.plan;
    Ok(match plan.action {
        Action::Load | Action::MoveIn => {
            inventory.drives.iter().any(|slot| {
                slot.dev == drive
                    && slot.addr == plan.destination
                    && slot
                        .loaded
                        .as_ref()
                        .is_some_and(|(loaded, _)| loaded == &plan.barcode)
            }) && (plan.action == Action::Load || inventory.empty_slots.contains(&plan.source))
                && (plan.action != Action::Load
                    || drive_io(owner, permit, control, scope, drive, |device| {
                        TapeDrive::new(device).test_unit_ready()
                    })?)
        }
        Action::UnloadMove => {
            inventory.slots.get(&plan.barcode) == Some(&plan.destination)
                && inventory.drives.iter().any(|slot| {
                    slot.dev == drive && slot.addr == plan.source && slot.loaded.is_none()
                })
        }
    })
}
async fn confirm(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    command: &Command,
) -> std::result::Result<(), ReadError> {
    if !postcondition(owner, permit, control, command)? {
        return Err(ReadError::Busy(format!(
            "原自动 {:?} 结果未知，保留原计划并只读核实",
            command.plan.action
        )));
    }
    control
        .catalog
        .confirm_automatic_media(&control.serial, control.route, owner.round, command)
        .await
        .map_err(lost)
}
pub(in crate::daemon::executor) async fn settle_pending(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
) -> std::result::Result<(), ReadError> {
    if let Some(command) = control
        .catalog
        .automatic_media_pending(&control.serial, control.route, owner.round)
        .await
        .map_err(lost)?
    {
        let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
        confirm(owner, permit, control, &command).await?;
    }
    Ok(())
}
fn plan(target: Target<'_>, owner: &Active, drive: usize, address: u16) -> Plan {
    Plan {
        action: Action::Load,
        barcode: target.placement.barcode.clone(),
        volume: target.placement.volume_id.to_string(),
        pool: target.placement.pool_id.to_string(),
        drive_serial: owner.devices[drive].serial.clone(),
        source: address,
        destination: address,
        role: if target.writable { 'm' } else { 'r' },
    }
}
async fn run(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    original: IoScope<'_>,
    plan: &Plan,
    execute: impl FnOnce(IoScope<'_>) -> std::result::Result<(), ReadError>,
) -> std::result::Result<(), ReadError> {
    let (command, granted) = control
        .catalog
        .issue_automatic_media(&control.serial, control.route, owner.round, plan)
        .await
        .map_err(lost)?;
    if granted {
        execute(IoScope::Automatic {
            command: &command,
            original: Some(&original),
        })?;
    }
    confirm(owner, permit, control, &command).await
}
pub(super) async fn load(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    target: Target<'_>,
    drive: usize,
    address: u16,
) -> std::result::Result<(), ReadError> {
    guard(owner, permit, control).await?;
    let scope = IoScope::Target(target);
    if drive_io(owner, permit, control, scope, drive, |device| {
        TapeDrive::new(device).test_unit_ready()
    })? {
        return Ok(());
    }
    run(
        owner,
        permit,
        control,
        scope,
        &plan(target, owner, drive, address),
        |scope| {
            drive_io(owner, permit, control, scope, drive, |device| {
                TapeDrive::new(device).load()?;
                wait_ready(device)
            })
        },
    )
    .await
}
pub(super) async fn move_in(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    target: Target<'_>,
    drive: usize,
    from: u16,
    to: u16,
) -> std::result::Result<(), ReadError> {
    let mut plan = plan(target, owner, drive, to);
    plan.action = Action::MoveIn;
    plan.source = from;
    run(
        owner,
        permit,
        control,
        IoScope::Target(target),
        &plan,
        |scope| checked_move(owner, permit, control, scope, from, to),
    )
    .await
}
pub(super) async fn unload_move(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    scope: IoScope<'_>,
    slot: &DriveSlot,
    destination: u16,
    role: char,
) -> std::result::Result<(), ReadError> {
    let IoScope::Unload {
        mount,
        drive,
        maintenance: None,
        intake: None,
        ..
    } = scope
    else {
        return Err(lost("自动退带必须绑定原普通挂载"));
    };
    if drive != slot.dev {
        return Err(lost("原自动退带驱动器映射改变"));
    }
    let plan = Plan {
        action: Action::UnloadMove,
        barcode: mount.barcode.clone(),
        volume: mount.volume.to_string(),
        pool: mount.pool.to_string(),
        drive_serial: owner.devices[drive].serial.clone(),
        source: slot.addr,
        destination,
        role,
    };
    run(owner, permit, control, scope, &plan, |scope| {
        drive_io(owner, permit, control, scope, drive, |device| {
            TapeDrive::new(device).unload()
        })?;
        let current = checked_inventory(owner, permit, control, scope)?;
        if !current.empty_slots.contains(&destination)
            || !current.drives.iter().any(|current| {
                current.dev == drive
                    && current.addr == slot.addr
                    && current
                        .loaded
                        .as_ref()
                        .is_some_and(|(loaded, _)| loaded == &mount.barcode)
            })
        {
            return Err(lost("原自动退带的源或目的库存改变"));
        }
        checked_move(owner, permit, control, scope, slot.addr, destination)
    })
    .await
}
