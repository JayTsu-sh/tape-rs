//! 原生 FDB 生命周期：持久意图在先，受保护的设备命令在后；不盲重放机械/格式化。
use super::*;
use crate::catalog::volume_extents::VolumeExtentCache;
use crate::changer::element::{ElementAddressMap, ElementStatus};
use crate::managed_catalog::{CatalogError, NativeDeviceTask};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
#[path = "executor_fdb_reclaim_tests.rs"]
mod parent_recovery;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    barcode: Option<String>,
    source: Option<u16>,
    destination: Option<u16>,
    home: Option<u16>,
    drive_serial: Option<String>,
    #[serde(with = "optional_uuid")]
    volume: Option<uuid::Uuid>,
    close_may_write: bool,
}

mod optional_uuid {
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        value: &Option<uuid::Uuid>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        value.map(|v| v.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<uuid::Uuid>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .as_deref()
            .map(uuid::Uuid::parse_str)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

fn failed(error: impl std::fmt::Display) -> ReadError {
    ReadError::Failed(format!("介质维护失败: {error}"))
}
fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("介质维护权限或状态不可用: {error}"))
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
        failed(error)
    }
}
fn kind(task: &NativeDeviceTask) -> &str {
    task.request().value()["change"]["kind"]
        .as_str()
        .expect("validated kind")
}

fn task_guard<'a>(
    owner: &Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    task: &'a NativeDeviceTask,
    device: &'a dyn TapeTransport,
) -> fdb_operation_guard::Guard<'a, impl Fn() -> Result<()> + 'a> {
    let round = owner.round;
    fdb_loading::operation_guard(owner, permit, device, move || {
        let state = control
            .catalog
            .native_device_status_blocking(task, round)
            .map_err(|error| TapeError::Catalog(error.to_string()))?;
        if task.request().value()["library"] != control.serial
            || task.request().namespace() != control.route.route.namespace
            || state["cancel_requested"] != false
        {
            return Err(TapeError::Catalog(
                "维护原任务作用域或取消状态已改变".into(),
            ));
        }
        Ok(())
    })
}
fn observation(
    inventory: Option<Value>,
    diagnostic: Option<&str>,
    validation: Option<Value>,
) -> Value {
    json!({"inventory":inventory,"diagnostic":diagnostic,"validation":validation})
}
async fn live(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
) -> std::result::Result<Value, ReadError> {
    if !(permit.allows(owner.round, super::super::permission::Phase::Serving)
        || permit.allows(owner.round, super::super::permission::Phase::Execution))
    {
        return Err(lost("设备执行许可已失效"));
    }
    fdb_loading::authority_guard(owner, permit, control).await?;
    control
        .catalog
        .native_device_status(task, owner.round)
        .await
        .map_err(lost)
}
#[allow(clippy::too_many_arguments)]
async fn checkpoint(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    from: &str,
    to: &str,
    plan: &Plan,
    observed: &Value,
) -> std::result::Result<Value, ReadError> {
    live(owner, permit, control, task).await?;
    control
        .catalog
        .checkpoint_native_device_task(task, owner.round, from, to, &json!(plan), Some(observed))
        .await
        .map_err(lost)
}
fn changer_device(owner: &Active) -> std::result::Result<&dyn TapeTransport, ReadError> {
    owner
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .map(|d| d.dev.as_ref() as &dyn TapeTransport)
        .ok_or_else(|| failed("没有换带器"))
}
fn elements_on(
    owner: &Active,
    device: &dyn TapeTransport,
) -> std::result::Result<(ElementAddressMap, Vec<ElementStatus>), ReadError> {
    let mut changer = MediumChanger::new(device);
    let map = changer.load_address_map().map_err(physical)?.clone();
    let rows = changer.read_all_status().map_err(physical)?;
    inventory_elements(&owner.devices, &rows, true).map_err(physical)?;
    Ok((map, rows))
}
#[cfg(test)]
fn elements(
    owner: &Active,
) -> std::result::Result<(ElementAddressMap, Vec<ElementStatus>), ReadError> {
    elements_on(owner, changer_device(owner)?)
}
fn task_elements(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
) -> std::result::Result<(ElementAddressMap, Vec<ElementStatus>), ReadError> {
    let checked = task_guard(owner, permit, control, task, changer_device(owner)?);
    let result = elements_on(owner, &checked);
    checked.verify_proof().map_err(physical)?;
    result
}
fn cartridge<'a>(
    rows: &'a [ElementStatus],
    barcode: &str,
) -> std::result::Result<&'a ElementStatus, ReadError> {
    let mut matches = rows
        .iter()
        .filter(|e| e.full && e.volume_tag.as_deref() == Some(barcode));
    let item = matches.next().ok_or_else(|| failed("条码不在实际库存中"))?;
    if matches.next().is_some() {
        return Err(failed("库存存在重复条码"));
    }
    Ok(item)
}
fn storage(map: &ElementAddressMap, address: u16) -> bool {
    (u32::from(map.storage_start)..u32::from(map.storage_start) + u32::from(map.storage_count))
        .contains(&u32::from(address))
}
fn empty(rows: &[ElementStatus], address: u16, kind: ElementType) -> bool {
    rows.iter()
        .any(|e| e.address == address && e.element_type == kind && !e.full)
}
fn at_destination(rows: &[ElementStatus], plan: &Plan) -> bool {
    plan.barcode
        .as_deref()
        .and_then(|b| cartridge(rows, b).ok())
        .is_some_and(|e| Some(e.address) == plan.destination)
        && (plan.source == plan.destination
            || rows
                .iter()
                .any(|e| Some(e.address) == plan.source && !e.full))
}
fn moved(rows: &[ElementStatus], plan: &Plan) -> std::result::Result<(), ReadError> {
    if at_destination(rows, plan) {
        Ok(())
    } else {
        Err(failed("移动结果未确认；保留原意图与实际位置，不重放 MOVE"))
    }
}

enum MoveIntent {
    NewlyIssued,
    ObserveOnly,
}
fn checked_move_on(
    owner: &Active,
    device: &dyn TapeTransport,
    plan: &Plan,
    intent: MoveIntent,
) -> std::result::Result<(), ReadError> {
    let (_, rows) = elements_on(owner, device)?;
    if at_destination(&rows, plan) {
        return Ok(());
    }
    if matches!(intent, MoveIntent::ObserveOnly) {
        return Err(failed("原 MOVE 结果未确认，仅观察，不重放"));
    }
    let current = cartridge(
        &rows,
        plan.barcode
            .as_deref()
            .ok_or_else(|| failed("计划缺少条码"))?,
    )?;
    if Some(current.address) != plan.source {
        return Err(failed("MOVE 前原条码位置改变"));
    }
    let target = rows
        .iter()
        .find(|e| {
            Some(e.address) == plan.destination
                && !e.full
                && matches!(
                    e.element_type,
                    ElementType::Storage | ElementType::ImportExport | ElementType::DataTransfer
                )
        })
        .ok_or_else(|| failed("MOVE 前固定目的位置已占用或无效"))?;
    let mut changer = MediumChanger::new(device);
    changer.load_address_map().map_err(physical)?;
    changer
        .move_medium(current.address, target.address)
        .map_err(physical)?;
    moved(&elements_on(owner, device)?.1, plan)
}

#[cfg(test)]
fn checked_move(
    owner: &Active,
    plan: &Plan,
    intent: MoveIntent,
) -> std::result::Result<(), ReadError> {
    checked_move_on(owner, changer_device(owner)?, plan, intent)
}
fn task_move(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    plan: &Plan,
    intent: MoveIntent,
) -> std::result::Result<(), ReadError> {
    let checked = task_guard(owner, permit, control, task, changer_device(owner)?);
    let result = checked_move_on(owner, &checked, plan, intent);
    checked.verify_proof().map_err(physical)?;
    result
}

fn choose_home(
    map: &ElementAddressMap,
    rows: &[ElementStatus],
    item: &ElementStatus,
    prior_home: Option<u16>,
    unassigned: bool,
) -> std::result::Result<u16, ReadError> {
    if item.element_type == ElementType::Storage {
        return storage(map, item.address)
            .then_some(item.address)
            .ok_or_else(|| failed("实际 storage 地址不在 address map 中"));
    }
    for known in [item.source_address, prior_home].into_iter().flatten() {
        if !storage(map, known) || !empty(rows, known, ElementType::Storage) {
            return Err(failed(format!(
                "已知 home {known:#06x} 无效、缺失或被占用，停止归槽"
            )));
        }
    }
    if let (Some(source), Some(prior)) = (item.source_address, prior_home)
        && source != prior
    {
        return Err(failed("SMC 原槽位与持久 home 证据冲突，停止归槽"));
    }
    if let Some(home) = item.source_address.or(prior_home) {
        return Ok(home);
    }
    if unassigned
        && matches!(
            item.element_type,
            ElementType::ImportExport | ElementType::DataTransfer
        )
    {
        return rows
            .iter()
            .find(|e| e.element_type == ElementType::Storage && storage(map, e.address) && !e.full)
            .map(|e| e.address)
            .ok_or_else(|| failed("无归属介质没有可信原槽位且没有空闲 storage"));
    }
    Err(failed("缺少可核验的原 home slot"))
}

async fn choose_plan(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
) -> std::result::Result<Plan, ReadError> {
    live(owner, permit, control, task).await?;
    let mut plan = Plan {
        version: 1,
        barcode: task.barcode().map(str::to_owned),
        source: None,
        destination: None,
        home: None,
        drive_serial: None,
        volume: task.request().value()["change"]["volume"]
            .as_str()
            .map(uuid::Uuid::parse_str)
            .transpose()
            .map_err(failed)?,
        close_may_write: false,
    };
    if kind(task) == "library-rescan" {
        return Ok(plan);
    }
    let (map, rows) = task_elements(owner, permit, control, task)?;
    let barcode = task.barcode().ok_or_else(|| failed("介质任务缺少条码"))?;
    let item = cartridge(&rows, barcode)?;
    let inv = inventory_elements(&owner.devices, &rows, true).map_err(physical)?;
    let assignment = control
        .catalog
        .tape_assignment(&control.serial, barcode)
        .await
        .map_err(lost)?;
    if assignment.as_ref().map(|a| a.pool_id) != task.pool()
        || assignment
            .as_ref()
            .is_some_and(|a| a.volume_id != plan.volume)
    {
        return Err(failed("实际池/卷归属与原请求不符"));
    }
    let prior_home = control
        .catalog
        .native_device_home(
            &control.serial,
            control.route,
            owner.round,
            barcode,
            assignment.as_ref().and_then(|a| a.volume_id),
        )
        .await
        .map_err(lost)?;
    let home = choose_home(&map, &rows, item, prior_home, assignment.is_none())?;
    plan.source = Some(item.address);
    plan.home = Some(home);
    let source_drive = inv.drives.iter().find(|d| d.addr == item.address);
    if matches!(kind(task), "media-move" | "media-unassign") {
        if let Some(drive) = source_drive {
            if assignment.is_none() {
                unassigned_drive_idle(owner, drive.dev)?;
                let policy = fdb_loading::policy(owner, control, &[], None).await?;
                if !super::super::management::drive_allowed(
                    &policy,
                    &owner.devices[drive.dev].serial,
                    'g',
                ) {
                    return Err(ReadError::Busy("维护退带被驱动策略禁止".into()));
                }
            } else {
                if owner
                    .fdb_reads
                    .get(&drive.dev)
                    .is_some_and(|m| m.barcode != barcode || Some(m.volume) != plan.volume)
                {
                    return Err(failed("退带目标与本轮已有挂载冲突"));
                }
                // Missing in-memory state after takeover is reconstructed only
                // by unload_for_maintenance, after persisting this exact plan
                // and checking the actual ready label against the FDB head.
                plan.close_may_write = true;
            }
            plan.drive_serial = Some(owner.devices[drive.dev].serial.clone());
        }
        let ie = kind(task) == "media-move"
            && task.request().value()["change"]["destination"] == "ieslot";
        if ie && assignment.as_ref().is_some_and(|a| a.online) {
            return Err(failed("已分配且 online 的磁带不能移到 I/E"));
        }
        plan.destination = Some(if ie {
            if item.element_type == ElementType::ImportExport {
                item.address
            } else {
                rows.iter()
                    .find(|e| e.element_type == ElementType::ImportExport && !e.full)
                    .ok_or_else(|| failed("没有空闲 I/E 槽位"))?
                    .address
            }
        } else {
            home
        });
        return Ok(plan);
    }
    if item.is_cleaning() {
        return Err(failed("清洗带不能校验 LTFS"));
    }
    if item.element_type == ElementType::ImportExport {
        return Err(failed("维护前磁带必须归槽"));
    }
    let pools: Vec<_> = task.pool().into_iter().collect();
    let policy = fdb_loading::policy(owner, control, &pools, Some(barcode)).await?;
    if !assignment
        .as_ref()
        .is_some_and(|a| !a.blacklisted && (a.online || kind(task) == "media-online"))
    {
        return Err(failed("校验要求 online 且非黑名单的已分配卷"));
    }
    let mut selected = None;
    for drive in &inv.drives {
        if !super::super::management::drive_allowed(&policy, &owner.devices[drive.dev].serial, 'g')
        {
            continue;
        }
        if drive.loaded.as_ref().is_some_and(|(b, _)| b != barcode) {
            continue;
        }
        if let Some((b, _)) = &drive.loaded
            && owner
                .fdb_reads
                .get(&drive.dev)
                .is_some_and(|m| m.barcode != *b || Some(m.volume) != plan.volume)
        {
            continue;
        }
        // A completed assign or HA takeover need not retain an in-memory
        // mount. The read-only validation below checks actual labels and the
        // complete index against the original request and published FDB head
        // before recording a new mount or allowing subsequent maintenance.
        let checked = task_guard(
            owner,
            permit,
            control,
            task,
            owner.devices[drive.dev].dev.as_ref(),
        );
        let capabilities = TapeDrive::new(&checked).report_density_support(false);
        checked.verify_proof().map_err(physical)?;
        let capabilities = capabilities.map_err(physical)?;
        if super::super::scheduling::densities(&capabilities)
            .is_some_and(|c| super::super::scheduling::compatible(&c, barcode, false))
        {
            selected = Some(drive);
            break;
        }
    }
    let drive = selected
        .ok_or_else(|| ReadError::Busy("没有兼容且空闲的维护驱动器；不自动淘汰其他介质".into()))?;
    if drive.loaded.is_none()
        && let Some(pool) = task.pool()
    {
        let limit = policy["pools"][pool.to_string()]["mountlimit"]
            .as_str()
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or_else(|| lost("池挂载上限缺失"))?;
        let mut occupied = 0;
        for slot in &inv.drives {
            if let Some((b, _)) = &slot.loaded {
                occupied += usize::from(
                    control
                        .catalog
                        .tape_assignment(&control.serial, b)
                        .await
                        .map_err(lost)?
                        .is_some_and(|a| a.pool_id == pool),
                );
            }
        }
        if limit != 0 && occupied >= limit {
            return Err(ReadError::Busy("维护装带达到池挂载上限".into()));
        }
    }
    plan.destination = Some(drive.addr);
    plan.drive_serial = Some(owner.devices[drive.dev].serial.clone());
    Ok(plan)
}

fn unassigned_drive_idle(owner: &Active, drive: usize) -> std::result::Result<(), ReadError> {
    if owner.fdb_reads.contains_key(&drive)
        || owner.drive == Some(drive)
        || owner.read.as_ref().is_some_and(|read| read.0 == drive)
    {
        return Err(failed("无归属介质与本轮已有挂载或写入状态冲突"));
    }
    Ok(())
}

async fn unload_unassigned(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    plan: &Plan,
    drive: usize,
) -> std::result::Result<(), ReadError> {
    let state = live(owner, permit, control, task).await?;
    if kind(task) != "media-move"
        || task.pool().is_some()
        || plan.volume.is_some()
        || plan.close_may_write
        || state["phase"] != "move-issued"
        || state["cancel_requested"] == true
        || state["plan"] != json!(plan)
        || plan.drive_serial.as_deref() != Some(owner.devices[drive].serial.as_str())
    {
        return Err(lost("未分配介质退带的原任务、驱动器或检查点不符"));
    }
    unassigned_drive_idle(owner, drive)?;
    if control
        .catalog
        .tape_assignment(&control.serial, task.barcode().expect("media barcode"))
        .await
        .map_err(lost)?
        .is_some()
    {
        return Err(lost("未分配介质在退带前已有归属"));
    }
    let policy = fdb_loading::policy(owner, control, &[], None).await?;
    if !super::super::management::drive_allowed(&policy, &owner.devices[drive].serial, 'g') {
        return Err(ReadError::Busy("维护退带被驱动策略禁止".into()));
    }
    let (_, rows) = task_elements(owner, permit, control, task)?;
    let source = cartridge(&rows, task.barcode().expect("media barcode"))?;
    let destination = plan
        .destination
        .ok_or_else(|| failed("移动缺少固定目的地址"))?;
    let target_kind = if plan.destination == plan.home {
        ElementType::Storage
    } else {
        ElementType::ImportExport
    };
    if Some(source.address) != plan.source
        || source.element_type != ElementType::DataTransfer
        || !empty(&rows, destination, target_kind)
    {
        return Err(failed("未分配介质退带前源或固定目的库存已改变"));
    }
    // The durable move-issued checkpoint precedes this UNLOAD. Recovery only
    // observes the original move; it never retries an indeterminate UNLOAD.
    let checked = task_guard(
        owner,
        permit,
        control,
        task,
        owner.devices[drive].dev.as_ref(),
    );
    let result = TapeDrive::new(&checked).unload();
    checked.verify_proof().map_err(physical)?;
    result.map_err(physical)?;
    live(owner, permit, control, task).await?;
    if control
        .catalog
        .tape_assignment(&control.serial, task.barcode().expect("media barcode"))
        .await
        .map_err(lost)?
        .is_some()
    {
        return Err(lost("退带后介质归属改变，停止 MOVE"));
    }
    Ok(())
}

async fn observed(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    opts: &ExecOptions,
) -> std::result::Result<Value, ReadError> {
    live(owner, permit, control, task).await?;
    let _activity = permit
        .begin_device_activity(owner.round)
        .map_err(physical)?;
    let (map, rows) = task_elements(owner, permit, control, task)?;
    let checked = task_guard(owner, permit, control, task, changer_device(owner)?);
    let descriptor = crate::scsi::inquiry::standard_inquiry(&checked);
    checked.verify_proof().map_err(physical)?;
    let descriptor = descriptor.map_err(physical)?;
    let value = management::observation_from_elements(
        &owner.devices,
        opts.node_id,
        owner.round,
        &map,
        &rows,
        &descriptor,
    )
    .map_err(physical)?;
    live(owner, permit, control, task).await?;
    Ok(value)
}
async fn load(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    plan: &Plan,
    issue_move: bool,
) -> std::result::Result<usize, ReadError> {
    let state = live(owner, permit, control, task).await?;
    if state["cancel_requested"] == true {
        return Err(failed("已请求取消，停止后续装带"));
    }
    let (_, rows) = task_elements(owner, permit, control, task)?;
    let barcode = plan
        .barcode
        .as_deref()
        .ok_or_else(|| failed("计划缺少条码"))?;
    let current = cartridge(&rows, barcode)?;
    let destination = plan.destination.ok_or_else(|| failed("计划缺少驱动地址"))?;
    let drive = inventory_elements(&owner.devices, &rows, true)
        .map_err(physical)?
        .drives
        .into_iter()
        .find(|d| {
            d.addr == destination
                && Some(owner.devices[d.dev].serial.as_str()) == plan.drive_serial.as_deref()
        })
        .ok_or_else(|| failed("固定驱动器身份或地址改变"))?
        .dev;
    if current.address != destination {
        if !issue_move
            || Some(current.address) != plan.source
            || !empty(&rows, destination, ElementType::DataTransfer)
        {
            return Err(failed("装带结果未确认，不重放旧 MOVE"));
        }
        task_move(owner, permit, control, task, plan, MoveIntent::NewlyIssued)?;
        live(owner, permit, control, task).await?;
        moved(&task_elements(owner, permit, control, task)?.1, plan)?;
    }
    let checked = task_guard(
        owner,
        permit,
        control,
        task,
        owner.devices[drive].dev.as_ref(),
    );
    if issue_move {
        // Only this invocation, immediately after its persisted intent, may
        // issue LOAD. A recovered task observes its original result first.
        let loaded = TapeDrive::new(&checked).load();
        checked.verify_proof().map_err(physical)?;
        loaded.map_err(physical)?;
        let ready = wait_ready(&checked);
        checked.verify_proof().map_err(physical)?;
        ready.map_err(physical)?;
    } else {
        let ready = TapeDrive::new(&checked).test_unit_ready();
        checked.verify_proof().map_err(physical)?;
        if !ready.map_err(physical)? {
            return Err(lost("原 LOAD 结果尚未确认，保留原任务，禁止重放"));
        }
    }
    live(owner, permit, control, task).await?;
    checked.verify_proof().map_err(physical)?;
    moved(&task_elements(owner, permit, control, task)?.1, plan)?;
    Ok(drive)
}

struct Workspace(std::path::PathBuf);
impl Workspace {
    fn new(parent: &std::path::Path) -> std::result::Result<Self, ReadError> {
        use std::os::unix::fs::DirBuilderExt;
        let path = parent.join(format!("fdb-maintenance-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(failed)?;
        Ok(Self(path))
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn labels(
    device: &dyn TapeTransport,
) -> std::result::Result<crate::ltfs::label::LtfsLabel, ReadError> {
    let a = crate::ltfs::volume::read_label_only(device).map_err(physical)?;
    let tape = TapeDrive::new(device);
    tape.locate(1, crate::ltfs::volume::LABEL_BLOCK, true)
        .map_err(physical)?;
    let mut bytes = vec![0; 64 * 1024];
    let length = tape.read_block(&mut bytes).map_err(physical)?;
    let b = crate::ltfs::label::LtfsLabel::parse(&bytes[..length]).map_err(physical)?;
    if a.location != a.index_partition
        || b.location != b.data_partition
        || (
            a.volume_uuid,
            &a.version,
            &a.creator,
            &a.format_time,
            a.index_partition,
            a.data_partition,
            a.blocksize,
            a.compression,
        ) != (
            b.volume_uuid,
            &b.version,
            &b.creator,
            &b.format_time,
            b.index_partition,
            b.data_partition,
            b.blocksize,
            b.compression,
        )
    {
        return Err(failed("两个分区的 LTFS 标签不一致"));
    }
    Ok(a)
}
#[allow(clippy::too_many_arguments)]
fn validate_index(
    device: &dyn TapeTransport,
    spool: &std::path::Path,
    budget: super::super::fdb_read::ReadStorageBudget,
    volume: uuid::Uuid,
    pool: Option<uuid::Uuid>,
    expected: Option<crate::managed_catalog::ActiveIndexIdentity>,
) -> std::result::Result<(Value, bool), ReadError> {
    let label = labels(device)?;
    if label.volume_uuid != volume {
        return Err(failed("卷 UUID 与原任务不符"));
    }
    let workspace = Workspace::new(spool)?;
    let mut recovery = crate::ltfs::recovery::RecoveryBudget::default();
    recovery.max_index_bytes = recovery
        .max_index_bytes
        .min(usize::try_from(budget.index_bytes).unwrap_or(usize::MAX));
    recovery.index_scratch.directory = workspace.0.clone();
    recovery.index_scratch.max_bytes = budget.index_bytes;
    recovery.index_scratch.headroom_bytes = budget.headroom_bytes;
    let captured =
        crate::ltfs::recovery::capture_index_source(&TapeDrive::new(device), &label, &recovery)
            .map_err(physical)?;
    let head = captured.construct();
    if expected.is_some_and(|e| {
        e.volume_uuid != volume
            || e.generation != head.generation
            || e.location != head.location
            || e.sha256 != head.sha256
            || e.incremental != head.incremental
    }) {
        return Err(failed("介质索引与 FDB 已发布身份不一致；不自动导入或修复"));
    }
    let marker = captured
        .header()
        .root
        .xattrs
        .iter()
        .find(|x| x.key == XATTR_POOL_UUID)
        .map(|x| x.value.as_str());
    if marker.is_some_and(|v| pool.is_some_and(|pool| uuid::Uuid::parse_str(v).ok() != Some(pool)))
    {
        return Err(failed("介质 pool UUID 标记不符"));
    }
    let path = workspace.0.join("extents.sqlite");
    let storage = crate::local_read_storage::ReadStorageBudget {
        cache_bytes: budget.cache_bytes.min(budget.index_bytes),
        ..budget
    };
    let mut reservation =
        crate::local_read_storage::CacheBuild::new(&workspace.0, storage).map_err(physical)?;
    VolumeExtentCache::build_captured(&captured, &path, &mut reservation).map_err(physical)?;
    let identity = captured.identity().map_err(physical)?;
    let files = VolumeExtentCache::open_captured(&path, &identity)
        .map_err(physical)?
        .file_count()
        .map_err(physical)?;
    let mut has_children = false;
    captured
        .for_each_entry(|entry| {
            match entry {
                crate::ltfs::index::IndexEntry::File { .. } => has_children = true,
                crate::ltfs::index::IndexEntry::Directory { path, .. } => {
                    has_children |= !path.is_empty() && path != "/"
                }
            }
            Ok(())
        })
        .map_err(physical)?;
    let result = json!({"volume":volume.to_string(),"generation":head.generation,"index_sha256":head.sha256.iter().map(|b|format!("{b:02x}")).collect::<String>(),"files":files,"scope":"label_and_index","payload_verified":false});

    Ok((result, has_children))
}

async fn run_device(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
    task: &NativeDeviceTask,
) -> std::result::Result<Value, ReadError> {
    let _activity = permit
        .begin_device_activity(owner.round)
        .map_err(physical)?;
    let mut state = live(owner, permit, control, task).await?;
    let plan = if state["phase"] == "ready" {
        let plan = choose_plan(owner, permit, control, task).await?;
        let seen = observation(
            Some(observed(owner, permit, control, task, opts).await?),
            None,
            None,
        );
        state = checkpoint(
            owner, permit, control, task, "ready", "planned", &plan, &seen,
        )
        .await?;
        plan
    } else {
        serde_json::from_value::<Plan>(state["plan"].clone()).map_err(lost)?
    };
    if state["cancel_requested"] == true {
        return Err(failed("原任务已请求取消，保留最后物理检查点"));
    }
    let phase = state["phase"]
        .as_str()
        .ok_or_else(|| lost("任务缺少阶段"))?;
    if phase == "verified" {
        return Ok(
            json!({"observation":state["observation"]["inventory"],"validation":state["observation"]["validation"]}),
        );
    }
    if kind(task) == "library-rescan" {
        let inventory = observed(owner, permit, control, task, opts).await?;
        let seen = observation(Some(inventory.clone()), None, None);
        checkpoint(
            owner, permit, control, task, "planned", "verified", &plan, &seen,
        )
        .await?;
        return Ok(json!({"observation":inventory,"validation":null}));
    }
    if matches!(kind(task), "media-move" | "media-unassign") {
        if plan.source != plan.destination && phase == "planned" {
            let seen = observation(
                Some(observed(owner, permit, control, task, opts).await?),
                None,
                None,
            );
            checkpoint(
                owner,
                permit,
                control,
                task,
                "planned",
                "move-issued",
                &plan,
                &seen,
            )
            .await?;
            let (map, rows) = task_elements(owner, permit, control, task)?;
            let current = cartridge(&rows, plan.barcode.as_deref().expect("media barcode"))?;
            if Some(current.address) != plan.source {
                return Err(failed("执行前原始条码位置改变"));
            }
            if let Some(serial) = plan.drive_serial.as_deref() {
                let drive = owner
                    .devices
                    .iter()
                    .position(|d| d.kind == DeviceKind::Drive && d.serial == serial)
                    .ok_or_else(|| failed("源驱动器已改变"))?;
                if task.pool().is_none() && plan.volume.is_none() {
                    unload_unassigned(owner, permit, control, task, &plan, drive).await?;
                } else {
                    fdb_loading::unload_for_maintenance(
                        owner,
                        permit,
                        control,
                        task,
                        drive,
                        plan.home.expect("fixed home"),
                    )
                    .await?;
                }
            }
            live(owner, permit, control, task).await?;
            let (_, rows) = task_elements(owner, permit, control, task)?;
            let current = cartridge(&rows, plan.barcode.as_deref().expect("barcode"))?;
            let destination = plan.destination.expect("destination");
            if current.address != destination {
                let target_kind = if storage(&map, destination) {
                    ElementType::Storage
                } else {
                    ElementType::ImportExport
                };
                if !empty(&rows, destination, target_kind) {
                    return Err(failed("固定目的槽位已占用"));
                }
                let mut step = plan.clone();
                step.source = Some(current.address);
                task_move(owner, permit, control, task, &step, MoveIntent::NewlyIssued)?;
            }
        }
        live(owner, permit, control, task).await?;
        let (_, rows) = task_elements(owner, permit, control, task)?;
        let inventory = observed(owner, permit, control, task, opts).await?;
        if let Err(error) = if at_destination(&rows, &plan) {
            Ok(())
        } else {
            task_move(owner, permit, control, task, &plan, MoveIntent::ObserveOnly)
        } {
            let seen = observation(Some(inventory), Some(&error.to_string()), None);
            let phase = if phase == "planned" {
                "move-issued"
            } else {
                phase
            };
            checkpoint(
                owner,
                permit,
                control,
                task,
                phase,
                "move-uncertain",
                &plan,
                &seen,
            )
            .await?;
            return Err(error);
        }
        let seen = observation(Some(inventory.clone()), None, None);
        checkpoint(
            owner,
            permit,
            control,
            task,
            if plan.source == plan.destination {
                "planned"
            } else if phase == "planned" {
                "move-issued"
            } else {
                phase
            },
            "verified",
            &plan,
            &seen,
        )
        .await?;
        if task.pool().is_none() && plan.volume.is_none() {
            owner
                .fdb_pending_import
                .remove(task.barcode().expect("media barcode"));
        }
        return Ok(json!({"observation":inventory,"validation":null}));
    }
    let newly_issued = phase == "planned";
    if newly_issued {
        checkpoint(
            owner,
            permit,
            control,
            task,
            "planned",
            "load-issued",
            &plan,
            &observation(None, None, None),
        )
        .await?;
    }
    let drive = load(owner, permit, control, task, &plan, newly_issued).await?;
    if phase != "loaded" {
        checkpoint(
            owner,
            permit,
            control,
            task,
            "load-issued",
            "loaded",
            &plan,
            &observation(
                Some(observed(owner, permit, control, task, opts).await?),
                None,
                None,
            ),
        )
        .await?;
    }
    let head = control
        .catalog
        .active_loaded_volume_head(
            &control.serial,
            plan.barcode.as_deref().expect("barcode"),
            control.route,
            owner.round,
        )
        .await
        .map_err(lost)?
        .ok_or_else(|| failed("校验卷已无已发布索引"))?;
    let checked = task_guard(
        owner,
        permit,
        control,
        task,
        owner.devices[drive].dev.as_ref(),
    );
    let tracked = fdb_monitor::Tracked::new(&checked, "validate");
    let validation = validate_index(
        &tracked,
        owner.files.spool_dir(),
        control.storage,
        plan.volume.ok_or_else(|| failed("校验缺少卷身份"))?,
        Some(task.pool().ok_or_else(|| failed("校验缺少池"))?),
        Some(head.index),
    );
    checked.verify_proof().map_err(physical)?;
    if let Err(error) = &validation {
        let event = tracked
            .error()
            .unwrap_or_else(|| crate::managed_catalog::TapeMonitorError {
                operation: "validate".into(),
                cdb: vec![],
                observed_at: crate::ltfs::label::ltfs_time_now(),
                status: None,
                sense_key: None,
                asc: None,
                ascq: None,
                raw_sense: vec![],
                received_sense_len: 0,
                message: error.to_string().chars().take(500).collect(),
            });
        fdb_monitor::io_error(
            owner,
            permit,
            control,
            plan.barcode.as_deref().expect("barcode"),
            head.volume_id,
            Some(event),
        )
        .await;
    }
    let (validation, _) = validation?;
    live(owner, permit, control, task).await?;
    let inventory = observed(owner, permit, control, task, opts).await?;
    fdb_monitor::index_state(
        owner,
        control,
        plan.barcode.as_deref().expect("barcode"),
        head.index,
        false,
    )
    .await?;
    fdb_monitor::sample_for_maintenance(
        owner,
        permit,
        control,
        drive,
        plan.barcode.as_deref().expect("barcode"),
        head.volume_id,
        task,
    )
    .await?;
    checkpoint(
        owner,
        permit,
        control,
        task,
        "loaded",
        "verified",
        &plan,
        &observation(Some(inventory.clone()), None, Some(validation.clone())),
    )
    .await?;
    owner.fdb_reads.insert(
        drive,
        fdb_loading::Mount {
            barcode: plan.barcode.expect("barcode"),
            volume: head.volume_id,
            pool: head.pool_id,
            last_used: Instant::now(),
        },
    );
    Ok(json!({"observation":inventory,"validation":validation}))
}

/// 推进原校验或退带任务；未知设备结果保留检查点，不重放命令。
pub(super) async fn apply(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
    epoch: u64,
    id: uuid::Uuid,
) -> std::result::Result<(), ReadError> {
    let task = match control
        .catalog
        .start_native_device_task(&control.serial, control.route, owner.round, epoch, id)
        .await
    {
        Ok(Some(task)) => task,
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
    let _activity = permit
        .begin_device_activity(owner.round)
        .map_err(physical)?;
    let current = live(owner, permit, control, &task).await?;
    if current["cancel_requested"] == true {
        if !matches!(
            current["phase"].as_str(),
            Some("ready" | "planned" | "loaded" | "verified")
        ) {
            // The cancellation request is durable, but is not proof that the
            // original in-flight LOAD/UNLOAD/MOVE has stopped or failed.
            return Ok(());
        }
        let result = json!({"observation":null,"validation":null});
        control
            .catalog
            .finish_native_device_task(&task, owner.round, &result, None)
            .await
            .map_err(lost)?;
        return Ok(());
    }
    match Box::pin(run_device(owner, permit, control, opts, &task)).await {
        Ok(result) => {
            live(owner, permit, control, &task).await?;
            control
                .catalog
                .finish_native_device_task(&task, owner.round, &result, None)
                .await
                .map_err(lost)?;
        }
        Err(ReadError::Busy(_)) => {}
        Err(ReadError::Lost(error)) => return Err(ReadError::Lost(error)),
        Err(ReadError::Failed(error)) => {
            let state = live(owner, permit, control, &task).await?;
            let result = json!({"observation":observed(owner, permit, control, &task, opts).await.ok(),"validation":null});
            control
                .catalog
                .finish_native_device_task(
                    &task,
                    owner.round,
                    &result,
                    (state["cancel_requested"] != true).then_some(error.as_str()),
                )
                .await
                .map_err(lost)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::permission::Phase;
    use crate::scsi::sim::{SimCartridge, SimLibrary};

    type Commands = Arc<std::sync::Mutex<Vec<(DeviceKind, u8)>>>;

    async fn verify_node_coverage(
        catalog: &crate::managed_catalog::ManagedCatalog,
        parent: &crate::managed_catalog::ReclaimReservation,
        owner: u64,
        inventory: &crate::managed_catalog::ReclaimSourceInventory<'_>,
        observations: &[crate::managed_catalog::ReclaimNodeMetadata],
        db: &foundationdb::Database,
    ) {
        let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
        prefix.extend_from_slice(parent.namespace().as_bytes());
        prefix.extend_from_slice(b"Rq");
        prefix.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
        prefix.extend_from_slice(
            uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        let mut progress_key = prefix.clone();
        let kind = progress_key.len() - 25;
        progress_key[kind] = b'y';
        assert!(
            catalog
                .step_reclaim_source_coverage(parent, owner, inventory)
                .await
                .is_err(),
            "实际源索引和观察清单本身不是完整覆盖"
        );
        for observation in observations {
            assert!(
                catalog
                    .seal_reclaim_node_coverage(parent, owner + 1, observation)
                    .await
                    .is_err()
            );
            if observation.target_path().is_empty() {
                assert!(
                    catalog
                        .seal_reclaim_node_coverage(parent, owner, observation)
                        .await
                        .is_err(),
                    "目的卷根仍由恢复机制修改，不能代替独立归档节点"
                );
                continue;
            }
            catalog
                .seal_reclaim_node_coverage(parent, owner, observation)
                .await
                .unwrap();
            catalog
                .seal_reclaim_node_coverage(parent, owner, observation)
                .await
                .unwrap();
        }
        for observation in observations
            .iter()
            .filter(|proof| !proof.target_path().is_empty())
        {
            let mut key = prefix.clone();
            key.extend_from_slice(&observation.source_uid().to_be_bytes());
            let tr = db.create_trx().unwrap();
            let raw = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            tr.clear(&key);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .step_reclaim_source_coverage(parent, owner, inventory)
                    .await
                    .is_err(),
                "根目录、空目录或链接任一缺失都阻止覆盖结清"
            );
            let tr = db.create_trx().unwrap();
            assert!(
                tr.get(&progress_key, false).await.unwrap().is_none(),
                "失败页不能部分推进"
            );
            tr.set(&key, &raw);
            tr.commit().await.unwrap();
        }
        assert!(
            catalog
                .step_reclaim_source_coverage(parent, owner, inventory)
                .await
                .is_err(),
            "缺少独立源根节点不得结清覆盖"
        );
        assert!(
            catalog
                .check_reclaim_source_unreferenced(parent, owner)
                .await
                .is_err(),
            "完整物理节点覆盖仍不能代替零 catalog 引用门槛"
        );
    }
    async fn verify_node_metadata_actor(
        catalog: &crate::managed_catalog::ManagedCatalog,
        fixture: &Fixture,
        control: &FdbOwnerControl,
        parent: &crate::managed_catalog::ReclaimReservation,
        source: &crate::ltfs::recovery::CapturedIndexSource,
        repair: bool,
        relocate: bool,
    ) {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{
            ReclaimNodeDestination, ReclaimNodeReadRequest, ReclaimSourceInventory,
        };
        let inventory = ReclaimSourceInventory::from_captured(source).unwrap();
        let root = catalog
            .reclaim_source_nodes(parent, fixture.owner.round, &inventory, &[1])
            .await
            .unwrap()
            .pop()
            .unwrap();
        let device = fixture.device();
        let budget = RecoveryBudget::default();
        let target = capture_index_source(
            &TapeDrive::new(device),
            &crate::ltfs::volume::read_label_only(device).unwrap(),
            &budget,
        )
        .unwrap();
        let mut plan = inventory
            .plan_reclaimed_node(parent, &root, &target, "reclaimed-source-root")
            .unwrap();
        let original_files = catalog
            .active_volume_file_count(
                &control.serial,
                control.route,
                fixture.owner.round,
                plan.volume(),
            )
            .await
            .unwrap();
        let mut task = catalog
            .prepare_reclaim_metadata_node(parent, fixture.owner.round, &plan)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .prepare_reclaim_metadata_node(parent, fixture.owner.round, &plan)
                .await
                .unwrap(),
            task
        );
        assert_eq!(
            catalog
                .reserved_reclaim_status(parent, fixture.owner.round)
                .await
                .unwrap()["phase"],
            "copying"
        );
        assert_eq!(
            catalog
                .reserved_reclaim_status(parent, fixture.owner.round)
                .await
                .unwrap()["children"],
            0
        );
        assert!(
            catalog
                .release_reserved_reclaim(parent, fixture.owner.round)
                .await
                .is_err(),
            "节点写入不能按只读空预留释放父任务"
        );
        assert!(
            catalog
                .reclaim_metadata_execution_context(parent, fixture.owner.round, &task)
                .await
                .is_err(),
            "节点任务不伪造文件子请求"
        );
        assert!(
            catalog
                .step_reclaim_source_coverage(parent, fixture.owner.round, &inventory)
                .await
                .is_err(),
            "未决元数据写入不能结清覆盖"
        );
        let fault = MetadataFault {
            inner: device,
            mode: if repair {
                MetadataExecutionCase::DiscardIntent
            } else {
                MetadataExecutionCase::Commit
            },
            matching: std::sync::atomic::AtomicUsize::new(0),
            fired: std::sync::atomic::AtomicBool::new(false),
            permission: &fixture.permit,
            round: fixture.owner.round,
        };
        let mut actor = crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter {
            catalog,
            device: &fault,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            budget: &budget,
        };
        if repair {
            assert!(
                matches!(actor.execute(parent,&task,Some(source)).await,Err(TapeError::CommitFailed {ref stage,..}) if stage=="closing_fm")
            );
            // Make the simulated partial construct durable explicitly.
            fixture.lib.with_cartridge_mut("NODES1L8", |cartridge| {
                for partition in &mut cartridge.partitions {
                    partition.flushed = partition.objects.len();
                }
            });
            let writing = catalog
                .reclaim_metadata_write_status(parent, fixture.owner.round, plan.source_uid())
                .await
                .unwrap()
                .unwrap();
            let goal = actor.begin_discard(parent, &writing).await.unwrap();
            let crate::daemon::fdb_reclaim_media::ReclaimMetadataRecovery::Observed(proof) =
                actor.recover(parent, &goal).await.unwrap()
            else {
                panic!("fresh recovery")
            };
            let resolved = catalog
                .resolve_reclaim_metadata_recovery(parent, fixture.owner.round, &goal, &proof)
                .await
                .unwrap();
            assert_eq!(
                resolved.kind(),
                crate::managed_catalog::ReclaimMetadataRecoveryKind::Discarded
            );
            let old = catalog
                .reclaim_metadata_write_status(parent, fixture.owner.round, plan.source_uid())
                .await
                .unwrap()
                .unwrap();
            let live = actor
                .read_source_replan(parent, &old, source)
                .await
                .unwrap();
            plan = live.plan().clone();
            let operation = uuid::Uuid::new_v4();
            let (one, two) = tokio::join!(
                catalog.replan_reclaim_metadata_source(
                    parent,
                    fixture.owner.round,
                    &old,
                    operation,
                    &live
                ),
                catalog.replan_reclaim_metadata_source(
                    parent,
                    fixture.owner.round,
                    &old,
                    operation,
                    &live
                )
            );
            task = one.unwrap();
            assert_eq!(two.unwrap(), task);
            assert_eq!(
                catalog
                    .reclaim_metadata_successor(parent, fixture.owner.round, &old, operation)
                    .await
                    .unwrap(),
                Some(task.clone())
            );
            assert!(
                catalog
                    .replan_reclaim_metadata_source(
                        parent,
                        fixture.owner.round,
                        &old,
                        uuid::Uuid::new_v4(),
                        &live
                    )
                    .await
                    .is_err()
            );
            let closed = catalog
                .close_prepared_reclaim_metadata_write(parent, fixture.owner.round, &task)
                .await
                .unwrap();
            task = catalog
                .retry_reclaim_metadata_write(parent, fixture.owner.round, &closed)
                .await
                .unwrap();
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(parent.namespace().as_bytes());
            key.extend_from_slice(b"RV");
            key.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
            key.extend_from_slice(
                uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                    .unwrap()
                    .as_bytes(),
            );
            key.extend_from_slice(&plan.source_uid().to_be_bytes());
            key.extend_from_slice(old.id().as_bytes());
            let db = foundationdb::Database::from_path(&std::env::var("FDB_TEST_CLUSTER").unwrap())
                .unwrap();
            let tr = db.create_trx().unwrap();
            let edge = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            tr.clear(&key);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .reclaim_metadata_checkpoint(parent, fixture.owner.round)
                    .await
                    .is_err(),
                "节点同计划重试不能绕过原重新规划来源"
            );
            assert_eq!(
                catalog
                    .observed_active_volume_index(&control.serial, plan.volume())
                    .await
                    .unwrap(),
                crate::managed_catalog::ActiveIndexIdentity::from(
                    plan.previous_source().unwrap().head()
                ),
                "独立恢复历史不依赖父任务的后继边"
            );
            let tr = db.create_trx().unwrap();
            tr.set(&key, &edge);
            tr.commit().await.unwrap();
        }
        let result = actor.execute(parent, &task, Some(source)).await.unwrap();
        let proof = result.commit_evidence().unwrap();
        assert_eq!(proof.observation().physical_files(), original_files);
        let published = catalog
            .publish_reclaim_metadata_write(parent, fixture.owner.round, result.task(), proof)
            .await
            .unwrap();
        let commands = fixture.drive_log();
        let replay = actor.execute(parent, &task, None).await.unwrap();
        assert!(replay.task().completed() && replay.commit_evidence().is_none());
        assert_eq!(
            fixture.drive_log(),
            commands,
            "原节点发布重放不能再次写索引"
        );
        let reader = crate::daemon::fdb_read::ActiveMediaReader {
            catalog,
            device,
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: fixture.budget(),
        };
        let observations = reader
            .read_reclaim_node_metadata(
                parent,
                &inventory,
                &[ReclaimNodeReadRequest {
                    original: root,
                    destination: ReclaimNodeDestination {
                        volume: plan.volume(),
                        uid: plan.uid(),
                    },
                }],
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            )
            .await
            .unwrap();
        catalog
            .record_reclaim_node_observations(parent, fixture.owner.round, &observations)
            .await
            .unwrap();
        catalog
            .seal_reclaim_node_coverage(parent, fixture.owner.round, &observations[0])
            .await
            .unwrap();
        let complete = catalog
            .step_reclaim_source_coverage(parent, fixture.owner.round, &inventory)
            .await
            .unwrap();
        assert!(complete.complete());
        assert_eq!(complete.objects(), 3);
        assert_eq!(complete.total_objects(), 3);
        assert_eq!(
            catalog
                .step_reclaim_source_coverage(parent, fixture.owner.round, &inventory)
                .await
                .unwrap(),
            complete
        );
        assert!(
            catalog
                .check_reclaim_source_unreferenced(parent, fixture.owner.round)
                .await
                .is_err(),
            "完整覆盖不是零引用门槛或 FORMAT 权限"
        );
        if relocate {
            Box::pin(verify_native_link_relocation(
                catalog, fixture, control, parent, source,
            ))
            .await;
            return;
        }
        let before = fixture.drive_log();
        let next = fixture.owner.round + 100;
        catalog
            .claim_active_owner_epoch(&control.serial, next)
            .await
            .unwrap();
        let parents = catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, next, None, 16)
            .await
            .unwrap();
        assert_eq!(parents.items.len(), 1);
        let claimed = catalog
            .claim_reclaim_metadata_write(&parents.items[0], next, plan.source_uid())
            .await
            .unwrap();
        assert!(claimed.completed());
        assert_eq!(
            catalog
                .reclaim_metadata_publication(&parents.items[0], next, &claimed)
                .await
                .unwrap(),
            Some(published)
        );
        assert_eq!(
            catalog
                .step_reclaim_source_coverage(&parents.items[0], next, &inventory)
                .await
                .unwrap(),
            complete
        );
        assert!(
            catalog
                .publish_reclaim_metadata_write(&parents.items[0], next, &claimed, proof)
                .await
                .is_err(),
            "旧 owner 的实读凭证不能跨接管复用"
        );
        assert_eq!(fixture.drive_log(), before);
        assert_eq!(fixture.formats(), 2, "只有源/目的两个预格式化夹具");
    }

    async fn verify_native_link_relocation(
        catalog: &crate::managed_catalog::ManagedCatalog,
        fixture: &Fixture,
        control: &FdbOwnerControl,
        parent: &crate::managed_catalog::ReclaimReservation,
        source: &crate::ltfs::recovery::CapturedIndexSource,
    ) {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{ReclaimSourceInventory, ResolvedPath};
        let file = loop {
            let walk = catalog
                .step_reclaim_walk(parent, fixture.owner.round)
                .await
                .unwrap();
            assert!(!walk.complete());
            if let Some(file) = walk.current() {
                break file.clone();
            }
        };
        assert_eq!(file.path(), "/link");
        let full = format!("/{}/link", parent.spec().pool());
        let before = catalog
            .resolve_active_path(&control.serial, full.as_bytes())
            .await
            .unwrap()
            .unwrap();
        let ResolvedPath::File {
            head: old_head,
            copy: old_copy,
            attributes: old_attrs,
            ..
        } = before
        else {
            panic!("file")
        };
        assert!(old_copy.media.as_ref().unwrap().native_symlink);
        let inventory = ReclaimSourceInventory::from_captured(source).unwrap();
        let original = catalog
            .reclaim_source_nodes(
                parent,
                fixture.owner.round,
                &inventory,
                &[old_copy.ltfs_file_uid],
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        let device = fixture.device();
        let budget = RecoveryBudget::default();
        let label = crate::ltfs::volume::read_label_only(device).unwrap();
        let current = capture_index_source(&TapeDrive::new(device), &label, &budget).unwrap();
        let plan = inventory
            .plan_reclaimed_node(parent, &original, &current, "reclaimed-source-root/link")
            .unwrap();
        let task = catalog
            .prepare_reclaim_metadata_node(parent, fixture.owner.round, &plan)
            .await
            .unwrap();
        let log = fixture.drive_log();
        assert!(
            catalog
                .relocate_reclaim_native_link(parent, fixture.owner.round, &file, &task, source)
                .await
                .is_err(),
            "Prepared 不能迁移逻辑副本"
        );
        assert_eq!(fixture.drive_log(), log);
        let mut writer = crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter {
            catalog,
            device,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            budget: &budget,
        };
        let result = writer.execute(parent, &task, Some(source)).await.unwrap();
        catalog
            .publish_reclaim_metadata_write(
                parent,
                fixture.owner.round,
                result.task(),
                result.commit_evidence().unwrap(),
            )
            .await
            .unwrap();
        let log = fixture.drive_log();
        let (one, two) = tokio::join!(
            catalog.relocate_reclaim_native_link(parent, fixture.owner.round, &file, &task, source),
            catalog.relocate_reclaim_native_link(parent, fixture.owner.round, &file, &task, source)
        );
        let progress = one.unwrap();
        assert_eq!(two.unwrap(), progress);
        assert!(progress.current().is_none());
        assert_eq!(progress.copied_files(), 1);
        assert_eq!(progress.copied_bytes(), 0);
        let after = catalog
            .resolve_active_path(&control.serial, full.as_bytes())
            .await
            .unwrap()
            .unwrap();
        let ResolvedPath::File {
            head,
            copy,
            attributes,
            ..
        } = after
        else {
            panic!("file")
        };
        assert_eq!(head.version, old_head.version + 1);
        assert_ne!(head.copy_id, old_head.copy_id);
        assert_eq!(copy.origin, old_copy.origin);
        assert_eq!(copy.volume_id, plan.volume());
        assert_eq!(copy.ltfs_file_uid, plan.uid());
        assert_eq!(copy.media.as_ref().unwrap().physical_path, plan.path());
        assert_eq!(attributes, old_attrs, "链接目标和所有逻辑属性保持原值");
        assert_eq!(
            catalog
                .relocate_reclaim_native_link(parent, fixture.owner.round, &file, &task, source)
                .await
                .unwrap(),
            progress
        );
        catalog
            .check_reclaim_source_unreferenced(parent, fixture.owner.round)
            .await
            .unwrap();
        assert_eq!(
            fixture.drive_log(),
            log,
            "逻辑提交、精确退休和回执重入不发 CDB"
        );
        let next = fixture.owner.round + 100;
        catalog
            .claim_active_owner_epoch(&control.serial, next)
            .await
            .unwrap();
        let parents = catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, next, None, 16)
            .await
            .unwrap();
        let recovered = catalog
            .claim_reclaim_metadata_write(&parents.items[0], next, plan.source_uid())
            .await
            .unwrap();
        assert!(
            catalog
                .relocate_reclaim_native_link(parent, fixture.owner.round, &file, &task, source)
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .relocate_reclaim_native_link(&parents.items[0], next, &file, &recovered, source)
                .await
                .unwrap(),
            progress
        );
        catalog
            .check_reclaim_source_unreferenced(&parents.items[0], next)
            .await
            .unwrap();
        assert_eq!(
            fixture.drive_log(),
            log,
            "接管已完成迁移没有 CDB 或新空上传"
        );
        // Both gates are required even though the native link is already safe.
        // Catalog-only fixture: original immutable manufacturer facts. The
        // device producer separately verifies these via guarded READ ATTRIBUTE.
        let cart = fixture.lib.cartridge("LIFE00L8").unwrap();
        let medium = serde_json::from_value(
            json!({"manufacturer":cart.mam[&(0,0x0400)].1,"serial":cart.mam[&(0,0x0401)].1}),
        )
        .unwrap();
        let crate::scsi::sim::LogicalObject::Record(label_bytes) =
            &cart.partitions[0].objects[crate::ltfs::volume::LABEL_BLOCK as usize]
        else {
            panic!("label")
        };
        let label = crate::ltfs::label::LtfsLabel::parse(label_bytes).unwrap();
        let source_evidence = crate::managed_catalog::ReclaimFormatSourceEvidence::from_read(
            &parents.items[0],
            next,
            source,
            medium,
            &label,
        )
        .unwrap();
        let output = uuid::Uuid::new_v4();
        assert!(
            catalog
                .prepare_reclaim_format(&parents.items[0], next, output, &source_evidence)
                .await
                .is_err(),
            "扫描尚未结清，不能仅依赖零 V 引用"
        );
        assert!(
            catalog
                .reclaim_format_plan(&parents.items[0], next)
                .await
                .unwrap()
                .is_none()
        );
        loop {
            if catalog
                .step_reclaim_walk(&parents.items[0], next)
                .await
                .unwrap()
                .complete()
            {
                break;
            }
        }
        let format = catalog
            .prepare_reclaim_format(&parents.items[0], next, output, &source_evidence)
            .await
            .unwrap();
        assert!(!format.issued());
        assert_eq!(format.owner(), next);
        assert_eq!(format.output(), output);
        assert_eq!(
            catalog
                .prepare_reclaim_format(&parents.items[0], next, output, &source_evidence)
                .await
                .unwrap(),
            format
        );
        assert!(
            catalog
                .prepare_reclaim_format(
                    &parents.items[0],
                    next,
                    uuid::Uuid::new_v4(),
                    &source_evidence
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .prepare_reclaim_metadata_node(&parents.items[0], next, &plan)
                .await
                .is_err(),
            "格式化阶段不能再启动元数据迁移"
        );
        let db =
            foundationdb::Database::from_path(&std::env::var("FDB_TEST_CLUSTER").unwrap()).unwrap();
        let mut foreign = b"tape-rs/catalog/22/V/".to_vec();
        foreign.extend_from_slice(parent.spec().volume().as_bytes());
        for _ in 0..4 {
            foreign.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        let tr = db.create_trx().unwrap();
        tr.set(&foreign, &[0; 40]);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .admit_reclaim_format(&parents.items[0], next, &format)
                .await
                .is_err(),
            "其他命名空间或扫描的保留引用阻止格式化"
        );
        assert_eq!(
            catalog
                .reclaim_format_plan(&parents.items[0], next)
                .await
                .unwrap(),
            Some(format.clone())
        );
        let tr = db.create_trx().unwrap();
        tr.clear(&foreign);
        tr.commit().await.unwrap();
        let (one, two) = tokio::join!(
            catalog.admit_reclaim_format(&parents.items[0], next, &format),
            catalog.admit_reclaim_format(&parents.items[0], next, &format)
        );
        let one = one.unwrap();
        let two = two.unwrap();
        assert_ne!(
            one.needs_observation(),
            two.needs_observation(),
            "并发只有一个首次格式化准入"
        );
        assert_eq!(one.plan(), two.plan());
        let issued = one.plan().clone();
        assert!(issued.issued());
        catalog
            .verify_reclaim_format(&parents.items[0], next, &issued)
            .await
            .unwrap();
        assert!(
            catalog
                .admit_reclaim_format(&parents.items[0], next, &format)
                .await
                .unwrap()
                .needs_observation(),
            "丢失准入回执只能核实原结果"
        );
        assert!(
            catalog
                .release_reserved_reclaim(&parents.items[0], next)
                .await
                .is_err()
        );
        let mut claim_key = b"tape-rs/catalog/22/@/".to_vec();
        claim_key.extend_from_slice(uuid::Uuid::nil().as_bytes());
        claim_key.extend_from_slice(b"Qv");
        claim_key.extend_from_slice(parent.spec().volume().as_bytes());
        let tr = db.create_trx().unwrap();
        let claim = tr.get(&claim_key, false).await.unwrap().unwrap().to_vec();
        tr.set(&claim_key, b"another-original-format");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .verify_reclaim_format(&parents.items[0], next, &issued)
                .await
                .is_err(),
            "不能覆盖或忽略另一个原始卷占用"
        );
        let tr = db.create_trx().unwrap();
        tr.set(&claim_key, &claim);
        tr.commit().await.unwrap();
        let successor = next + 100;
        catalog
            .claim_active_owner_epoch(&control.serial, successor)
            .await
            .unwrap();
        let again = catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, successor, None, 16)
            .await
            .unwrap();
        let adopted = catalog
            .reclaim_format_plan(&again.items[0], successor)
            .await
            .unwrap()
            .unwrap();
        let managed = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                successor,
                parent.intent()["epoch"].as_u64().unwrap(),
                uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(managed.intent(), again.items[0].intent());
        assert_eq!(adopted.output(), output);
        assert_eq!(adopted.scan(), format.scan());
        assert!(adopted.issued());
        assert!(
            catalog
                .verify_reclaim_format(&parents.items[0], next, &issued)
                .await
                .is_err()
        );
        assert!(
            catalog
                .admit_reclaim_format(&again.items[0], successor, &issued)
                .await
                .is_err(),
            "旧 owner 的格式化快照不能变成新写许可"
        );
        assert!(
            catalog
                .admit_reclaim_format(&again.items[0], successor, &adopted)
                .await
                .unwrap()
                .needs_observation()
        );
        catalog
            .verify_reclaim_format(&again.items[0], successor, &adopted)
            .await
            .unwrap();
        assert_eq!(
            fixture.drive_log(),
            log,
            "格式化意图、双门槛、并发与 HA 检查均不发 CDB"
        );
        Box::pin(verify_reclaim_formatter(
            catalog,
            fixture,
            control,
            &again.items[0],
            &adopted,
        ))
        .await;
        let target = catalog
            .volume_placement(plan.volume())
            .await
            .unwrap()
            .unwrap();
        assert!(
            catalog
                .active_loaded_volume_head(
                    &control.serial,
                    &target.barcode,
                    control.route,
                    successor,
                )
                .await
                .unwrap()
                .is_some(),
            "清理临时账本后目的历史仍由独立回执验证"
        );
    }

    async fn verify_reclaim_formatter(
        catalog: &crate::managed_catalog::ManagedCatalog,
        fixture: &Fixture,
        control: &FdbOwnerControl,
        parent: &crate::managed_catalog::ReclaimReservation,
        plan: &crate::managed_catalog::ReclaimFormatPlan,
    ) {
        use crate::daemon::fdb_reclaim_format::{ReclaimFormatOutcome, ReclaimFormatter};
        use crate::daemon::permission::GuardedTransport;
        use crate::ltfs::recovery::RecoveryBudget;
        let round = plan.owner();
        let permit = Arc::new(Permission::default());
        permit.renew(
            round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(60),
        );
        let key = ReservationKey::new(1, round);
        let drive = GuardedTransport::new(
            Box::new(Recorded {
                inner: Box::new(fixture.lib.drive_as(0, 1)),
                kind: DeviceKind::Drive,
                commands: fixture.commands.clone(),
            }),
            permit.clone(),
            round,
        );
        let changer = GuardedTransport::new(
            Box::new(Recorded {
                inner: Box::new(fixture.lib.changer_as(1)),
                kind: DeviceKind::Changer,
                commands: fixture.commands.clone(),
            }),
            permit.clone(),
            round,
        );
        reservation::fence(&drive, key).unwrap();
        reservation::fence(&changer, key).unwrap();
        permit.renew(
            round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60),
        );
        // Test precondition: restore the covered source to the only drive.
        // Production movement is a separate durable mechanical controller.
        let mut robot = MediumChanger::new(&changer);
        let map = robot.load_address_map().unwrap().clone();
        let inventory = robot.read_all_status().unwrap();
        let loaded = inventory
            .iter()
            .find(|e| e.address == map.dt_start)
            .unwrap();
        if loaded.volume_tag.as_deref() != Some(parent.spec().barcode()) {
            TapeDrive::new(&drive).unload().unwrap();
            let home = inventory
                .iter()
                .find(|e| {
                    e.address >= map.storage_start
                        && e.address < map.storage_start + map.storage_count
                        && !e.full
                })
                .unwrap()
                .address;
            robot.move_medium(map.dt_start, home).unwrap();
            let source = inventory
                .iter()
                .find(|e| e.volume_tag.as_deref() == Some(parent.spec().barcode()))
                .unwrap()
                .address;
            robot.move_medium(source, map.dt_start).unwrap();
            TapeDrive::new(&drive).load().unwrap();
        }
        let budget = RecoveryBudget::default();
        let mut formatter = ReclaimFormatter {
            catalog,
            device: &drive,
            route: control.route,
            key,
            permission: &permit,
            budget: &budget,
        };
        let log = fixture.drive_log();
        let replay = catalog
            .admit_reclaim_format(parent, round, plan)
            .await
            .unwrap();
        let ReclaimFormatOutcome::Retry(observed) =
            formatter.execute(parent, replay).await.unwrap()
        else {
            panic!("unwritten source")
        };
        assert!(
            !fixture.drive_log()[log.len()..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x55 | 0x8d))
        );
        let next = catalog
            .rearm_reclaim_format(parent, round, plan, &observed)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .rearm_reclaim_format(parent, round, plan, &observed)
                .await
                .unwrap(),
            next
        );
        let ticket = catalog
            .admit_reclaim_format(parent, round, &next)
            .await
            .unwrap();
        assert!(!ticket.needs_observation());
        let issued = ticket.plan().clone();
        assert_eq!(issued.attempt(), 2);
        // A write failure after VOL1 and its filemark leaves a genuine partial
        // IP, while the original DP must still match the sealed source.
        let boundary = MetadataFault {
            inner: &drive,
            mode: MetadataExecutionCase::IpFailure,
            matching: std::sync::atomic::AtomicUsize::new(0),
            fired: std::sync::atomic::AtomicBool::new(false),
            permission: &permit,
            round,
        };
        let mut faulty = ReclaimFormatter {
            device: &boundary,
            ..formatter
        };
        assert!(faulty.execute(parent, ticket).await.is_err());
        assert!(boundary.fired.load(std::sync::atomic::Ordering::SeqCst));
        fixture.lib.power_cut();
        permit.renew(
            round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60),
        );
        let ReclaimFormatOutcome::Retry(partial) =
            formatter.observe(parent, &issued).await.unwrap()
        else {
            panic!("partial initialization")
        };
        let next = catalog
            .rearm_reclaim_format(parent, round, &issued, &partial)
            .await
            .unwrap();
        let ticket = catalog
            .admit_reclaim_format(parent, round, &next)
            .await
            .unwrap();
        let final_plan = ticket.plan().clone();
        assert_eq!(final_plan.attempt(), 3);
        let ReclaimFormatOutcome::Committed(proof) =
            formatter.execute(parent, ticket).await.unwrap()
        else {
            panic!("fixed empty baseline")
        };
        let after = fixture.drive_log();
        assert!(
            !after[log.len()..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x55)),
            "quick reset never repartitions"
        );
        let replay = catalog
            .admit_reclaim_format(parent, round, &final_plan)
            .await
            .unwrap();
        let ReclaimFormatOutcome::Committed(again) =
            formatter.execute(parent, replay).await.unwrap()
        else {
            panic!("completed reset")
        };
        assert_eq!(again, proof);
        assert!(
            !fixture.drive_log()[after.len()..]
                .iter()
                .any(|op| matches!(op, 0x0a | 0x10 | 0x8d)),
            "lost completion only reads"
        );
        let home = if parent.spec().unassign() {
            use crate::daemon::fdb_reclaim_mechanics::ReclaimMechanics;
            use crate::managed_catalog::{ReclaimMediaAction, ReclaimMediaPlan};
            assert!(
                catalog
                    .complete_reclaim_format(parent, round, &final_plan, &proof, None)
                    .await
                    .is_err(),
                "空卷证明不能单独解除分配"
            );
            let rows = robot.read_all_status().unwrap();
            let source = rows.iter().find(|e| e.address == map.dt_start).unwrap();
            let home = source.source_address.unwrap();
            let unload = ReclaimMediaPlan {
                action: ReclaimMediaAction::Unload,
                barcode: parent.spec().barcode().into(),
                volume: parent.spec().volume(),
                drive_serial: "SIMDRV0000".into(),
                source: map.dt_start,
                destination: map.dt_start,
                home,
                map: map.clone(),
            };
            let mechanics = ReclaimMechanics {
                catalog,
                changer: &changer,
                drive: &drive,
                route: control.route,
                key,
                permission: &permit,
            };
            mechanics.execute(parent, &unload).await.unwrap();
            assert!(
                mechanics
                    .formatted_source_home(parent, &final_plan, &proof)
                    .await
                    .is_err(),
                "仅退带不能充当归位证明"
            );
            let movement = ReclaimMediaPlan {
                action: ReclaimMediaAction::Move,
                destination: home,
                ..unload
            };
            mechanics.execute(parent, &movement).await.unwrap();
            Some(
                mechanics
                    .formatted_source_home(parent, &final_plan, &proof)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let result = catalog
            .complete_reclaim_format(parent, round, &final_plan, &proof, home.as_ref())
            .await
            .unwrap();
        assert_eq!(
            catalog
                .complete_reclaim_format(parent, round, &final_plan, &proof, home.as_ref())
                .await
                .unwrap(),
            result
        );
        assert_eq!(
            catalog.reclaim_format_result(parent, round).await.unwrap(),
            Some(result.clone())
        );
        assert_eq!(
            catalog
                .reserved_reclaim_status(parent, round)
                .await
                .unwrap(),
            result.value()
        );
        let assignment = catalog
            .tape_assignment(&control.serial, parent.spec().barcode())
            .await
            .unwrap();
        if parent.spec().unassign() {
            assert!(assignment.is_none());
            assert!(
                catalog
                    .volume_placement(final_plan.output())
                    .await
                    .unwrap()
                    .is_none()
            );
        } else {
            let assignment = assignment.unwrap();
            assert_eq!(assignment.volume_id, Some(final_plan.output()));
            assert_ne!(assignment.volume_id, Some(parent.spec().volume()));
        }
        let task = catalog
            .management_task(
                &control.serial,
                control.route,
                parent.intent()["epoch"].as_u64().unwrap(),
                uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task["status"], "succeeded");
        assert_eq!(task["result"], result.value());
        assert!(
            catalog.reclaim_format_plan(parent, round).await.is_err(),
            "released parent cannot authorize more I/O"
        );
        let page = catalog
            .reclaim_cleanup_page(&control.serial, control.route, round, None, 16)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].intent(), parent.intent());
        let mut complete = false;
        for _ in 0..64 {
            if catalog.step_reclaim_cleanup(parent, round).await.unwrap() {
                complete = true;
                break;
            }
        }
        assert!(complete, "有界清理最终完成");
        assert!(catalog.step_reclaim_cleanup(parent, round).await.unwrap());
        assert!(
            catalog
                .reclaim_cleanup_page(&control.serial, control.route, round, None, 16)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            catalog.reclaim_format_result(parent, round).await.unwrap(),
            Some(result.clone())
        );
        assert_eq!(
            catalog
                .reserved_reclaim_status(parent, round)
                .await
                .unwrap(),
            result.value()
        );
        permit.close(round);
        let before = fixture.drive_log();
        assert!(formatter.observe(parent, &final_plan).await.is_err());
        assert_eq!(
            fixture.drive_log(),
            before,
            "closed permission sends no CDB"
        );
    }

    struct ReclaimMoveReplyLost<'a> {
        inner: &'a dyn TapeTransport,
        fired: std::sync::atomic::AtomicBool,
    }
    impl TapeTransport for ReclaimMoveReplyLost<'_> {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch()
        }
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            let result = self.inner.execute_no_data(cdb, timeout)?;
            if cdb[0] == 0xa5 && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(TapeError::ScsiHost(3));
            }
            Ok(result)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            bytes: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_read(cdb, bytes, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            bytes: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_write(cdb, bytes, timeout)
        }
    }
    async fn verify_reclaim_mechanical_actor(
        catalog: &crate::managed_catalog::ManagedCatalog,
        fixture: &Fixture,
        control: &FdbOwnerControl,
        parent: &crate::managed_catalog::ReclaimReservation,
    ) {
        use crate::daemon::fdb_reclaim_mechanics::ReclaimMechanics;
        use crate::managed_catalog::{ReclaimMediaAction, ReclaimMediaPlan};
        let changer = fixture
            .owner
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .dev
            .as_ref();
        let mut robot = MediumChanger::new(changer);
        let map = robot.load_address_map().unwrap().clone();
        let rows = robot.read_all_status().unwrap();
        let row = rows.iter().find(|e| e.address == map.dt_start).unwrap();
        let home = row.source_address.unwrap();
        let base = ReclaimMediaPlan {
            action: ReclaimMediaAction::Unload,
            barcode: parent.spec().barcode().into(),
            volume: parent.spec().volume(),
            drive_serial: fixture.owner.devices[0].serial.clone(),
            source: row.address,
            destination: row.address,
            home,
            map,
        };
        let actor = ReclaimMechanics {
            catalog,
            changer,
            drive: fixture.device(),
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
        };
        let log = fixture.drive_log();
        actor.execute(parent, &base).await.unwrap();
        let unloaded = fixture.drive_log();
        assert_eq!(
            unloaded[log.len()..]
                .iter()
                .filter(|op| **op == 0x1b)
                .count(),
            1
        );
        actor.execute(parent, &base).await.unwrap();
        assert!(
            !fixture.drive_log()[unloaded.len()..].contains(&0x1b),
            "已退带不重复 UNLOAD"
        );
        let to_home = ReclaimMediaPlan {
            action: ReclaimMediaAction::Move,
            destination: home,
            ..base.clone()
        };
        let fault = ReclaimMoveReplyLost {
            inner: changer,
            fired: std::sync::atomic::AtomicBool::new(false),
        };
        let faulty = ReclaimMechanics {
            changer: &fault,
            ..actor
        };
        assert!(matches!(
            faulty.execute(parent, &to_home).await,
            Err(TapeError::ScsiHost(3))
        ));
        assert!(fault.fired.load(std::sync::atomic::Ordering::SeqCst));
        let pending = catalog
            .reclaim_media_pending(parent, fixture.owner.round)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.plan(), &to_home);
        let task = catalog
            .management_task(
                &control.serial,
                control.route,
                parent.intent()["epoch"].as_u64().unwrap(),
                uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task["status"], "running");
        assert_eq!(task["progress"]["mechanical"]["phase"], "result_unknown");
        assert!(
            catalog
                .require_active_library_readable(
                    &control.serial,
                    control.route,
                    fixture.owner.round
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .cancel_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    parent.intent()["epoch"].as_u64().unwrap(),
                    uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap()
                )
                .await
                .is_err(),
            "未知机械命令不能借未开写取消丢掉"
        );
        let to_drive = ReclaimMediaPlan {
            action: ReclaimMediaAction::Move,
            source: home,
            destination: base.source,
            ..base.clone()
        };
        assert!(
            actor.execute(parent, &to_drive).await.is_err(),
            "不能用新装载替换原未知 MOVE"
        );
        let after_unknown = fixture.changer_log();
        actor.execute(parent, &to_home).await.unwrap();
        assert!(
            !fixture.changer_log()[after_unknown.len()..].contains(&0xa5),
            "丢失 MOVE 回执只观察，不再移动"
        );
        assert!(
            catalog
                .reclaim_media_pending(parent, fixture.owner.round)
                .await
                .unwrap()
                .is_none()
        );
        catalog
            .require_active_library_readable(&control.serial, control.route, fixture.owner.round)
            .await
            .unwrap();
        actor.execute(parent, &to_drive).await.unwrap();
        let load = ReclaimMediaPlan {
            action: ReclaimMediaAction::Load,
            ..base
        };
        actor.execute(parent, &load).await.unwrap();
        let loaded = fixture.drive_log();
        actor.execute(parent, &load).await.unwrap();
        assert!(
            !fixture.drive_log()[loaded.len()..].contains(&0x1b),
            "已装载不重复 LOAD"
        );
        assert!(
            !fixture.drive_log()[log.len()..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x8d)),
            "机械控制不初始化或改写正文/索引/MAM"
        );
        let other = Permission::default();
        other.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let wrong = ReclaimMechanics {
            permission: &other,
            ..actor
        };
        let log = fixture.drive_log();
        let changer_log = fixture.changer_log();
        assert!(wrong.execute(parent, &load).await.is_err());
        assert_eq!(fixture.drive_log(), log);
        assert_eq!(fixture.changer_log(), changer_log, "错误许可实例不发 CDB");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 完整原生回收父任务、默认分配、清理及冷读"]
    async fn native_reclaim_parent_drives_copy_coverage_and_initialization() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::Default,
        ))
        .await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生父任务、HA、未映射正文、归槽解绑及冷读"]
    async fn native_reclaim_parent_recovers_and_unassigns_with_unmapped_raw_file() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::Unassign,
        ))
        .await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原复制 Q/I、带上提交后回执未知、HA 恢复及父完成"]
    async fn native_reclaim_parent_recovers_original_child_before_writing() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::ChildHa,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原终态已保存而监控收尾未保存，再次 HA 接续"]
    async fn native_reclaim_parent_retries_monitor_finish_after_original_terminal() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::FinishHa,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原 Q/I 未写入正文、HA 取消及唯一新尝试完成"]
    async fn native_reclaim_parent_retries_cancelled_original_child() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::CancelledHa,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生链接缺失父目录、独立源覆盖及冷读"]
    async fn native_reclaim_parent_preserves_native_link_with_missing_target_parents() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::NativeLinks,
        ))
        .await;
    }

    mod reclaim_load_readiness {
        include!("executor_fdb_reclaim_load_tests.rs");
    }
    include!("executor_fdb_reclaim_history_tests.rs");
    include!("executor_fdb_reclaim_raw_spool_tests.rs");
    include!("executor_fdb_reclaim_permit_tests.rs");
    include!("executor_fdb_reclaim_group_happy_tests.rs");

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum NativeParentCase {
        Default,
        Unassign,
        RawBodySingleRead,
        RawBodyBudget,
        ChildHa,
        FinishHa,
        CancelledHa,
        NativeLinks,
        ObsoleteVersions,
        DeletedObject,
        EeAlias,
        MountLimit,
        StaleQueued,
        VendorDvcid,
        LongHistory,
        SourceCaptureReuse,
        SourceHistoryHints,
        SourceVcrUnsupported,
        StallDiagnostics,
        SourceSessionInvalidation,
        SourceOtherHandleChange,
        SourceOtherHandleStableVcr,
        SourceFreshTokenGap,
        SourcePostVcrGap,
        SourceNoChangeEvidence,
        SourceErrorRecovery,
        LongHistoryT2,
        LongHistoryIp,
    }
    impl NativeParentCase {
        fn long_history(self) -> bool {
            matches!(
                self,
                Self::LongHistory
                    | Self::SourceCaptureReuse
                    | Self::LongHistoryT2
                    | Self::LongHistoryIp
            )
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 超过64代Full后的原生元数据T2修复和完整回收"]
    async fn native_reclaim_parent_recovers_long_full_history_t2() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::LongHistoryT2,
        ))
        .await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 超过64代Full后的原生元数据IP修复和完整回收"]
    async fn native_reclaim_parent_recovers_long_full_history_ip() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::LongHistoryIp,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 源和目的均超过64代Full历史的完整原生回收"]
    async fn native_reclaim_parent_checks_long_full_source_and_destination_history() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::LongHistory,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 同一源卷分页封存复用完整历史捕获"]
    async fn native_reclaim_source_inventory_pages_reuse_unchanged_history() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceCaptureReuse,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 明确不支持VCR时保留原完整源捕获路径"]
    async fn native_reclaim_source_without_vcr_continues_without_retained_capture() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceVcrUnsupported,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原父任务诊断CAS不覆盖真实依赖或重复更新时间"]
    async fn native_reclaim_stall_diagnostic_preserves_other_dependencies() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::StallDiagnostics,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 源捕获的定位/缺epoch/失权反例"]
    async fn native_reclaim_source_capture_rejects_changed_epoch_and_lost_owner() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceSessionInvalidation,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 同initiator另一句柄写源且VCR固定"]
    async fn native_reclaim_source_rejects_other_handle_write_with_constant_vcr() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceOtherHandleChange,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 固定VCR下已保存可信token再由另一句柄写源"]
    async fn native_reclaim_source_rejects_other_handle_write_after_trusted_capture() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceOtherHandleStableVcr,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 完整capture返回后首次token采样前另handle写源"]
    async fn native_reclaim_fresh_capture_rejects_write_before_end_token() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceFreshTokenGap,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim post VCR明确unsupported不能豁免另handle写源"]
    async fn native_reclaim_fresh_capture_rejects_write_with_post_vcr_unsupported() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourcePostVcrGap,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 无可信变化证明时完整捕获且不留缓存"]
    async fn native_reclaim_source_without_change_evidence_uses_full_capture() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceNoChangeEvidence,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原源读取故障后原parent恢复清理诊断"]
    async fn native_reclaim_source_dependency_uses_existing_recovery_checkpoint_after_restart() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::SourceErrorRecovery,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim IBM 厂商/型号 DVCID 的完整原生回收"]
    async fn native_reclaim_parent_accepts_vendor_model_dvcid() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::VendorDvcid,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 失效 queued 回收保留原失败回执，不击落 owner 或发送设备命令"]
    async fn native_reclaim_rejects_stale_queued_request_without_losing_owner() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::StaleQueued,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 回收遵守实际池挂载上限，等待时零机械/写入副作用且可安全取消"]
    async fn native_reclaim_layout_waits_for_pool_mount_limit_before_loading() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::MountLimit,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim EE 别名不带摘要、数据节点同时带 SHA/MD5 的完整回收"]
    async fn native_reclaim_parent_preserves_ee_alias_body_hashes() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::EeAlias,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 两个已发布版本、合法旧引用回收及原回执保留"]
    async fn native_reclaim_parent_retires_superseded_active_copies_without_losing_receipts() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::ObsoleteVersions,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 已发布删除、合法清理旧引用、原删除结果及墓碑保留"]
    async fn native_reclaim_parent_retires_deleted_inode_copies_without_resurrection() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        Box::pin(native_reclaim_parent_case(
            catalog,
            &cluster,
            NativeParentCase::DeletedObject,
        ))
        .await;
    }

    fn delete_native_parent_fixture<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        pool: uuid::Uuid,
        cluster: &'a str,
    ) -> std::pin::Pin<
        Box<
            impl std::future::Future<Output = (crate::managed_catalog::NodeMutationDeletion, Vec<u8>)>
            + 'a,
        >,
    > {
        Box::pin(async move {
            use crate::managed_catalog::{
                NodeMutation, NodeMutationSpec, NodeMutationTaskState, ResolvedPath,
            };
            let full = crate::managed_catalog::pool_catalog_path(pool, b"/removed").unwrap();
            let Some(ResolvedPath::File { head, .. }) = control
                .catalog
                .resolve_active_path_for_route(&control.serial, &full, control.route)
                .await
                .unwrap()
            else {
                panic!("removed source")
            };
            let spec = NodeMutationSpec {
                pool,
                operation: uuid::Uuid::new_v4(),
                path: "/removed".into(),
                expected_version: Some(head.version),
                mutation: NodeMutation::Unlink,
            };
            let request = control
                .catalog
                .prepare_node_mutation_request(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &spec,
                    &crate::ltfs::label::ltfs_time_now(),
                )
                .await
                .unwrap();
            control
                .catalog
                .queue_node_mutation_request(
                    control.route,
                    fixture.owner.round,
                    &request,
                    control.storage.index_bytes as usize,
                )
                .await
                .unwrap();
            let mut after = None;
            for _ in 0..16 {
                renew_native_parent_permit(fixture);
                Box::pin(fdb_nodes::advance(
                    &mut fixture.owner,
                    &fixture.permit,
                    control,
                    status,
                    &mut after,
                ))
                .await
                .unwrap();
                let state = control
                    .catalog
                    .node_mutation_task_status(
                        &control.serial,
                        control.route,
                        request.context().request_epoch,
                        spec.operation,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                if state.task.state == NodeMutationTaskState::Completed {
                    assert!(state.outcome.unwrap().deleted);
                    let result = control
                        .catalog
                        .node_mutation_deletion(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            request.context().request_epoch,
                            spec.operation,
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    let db = foundationdb::Database::from_path(cluster).unwrap();
                    let tr = db.create_trx().unwrap();
                    let mut key = b"tape-rs/catalog/22/N/".to_vec();
                    key.extend_from_slice(control.route.route.namespace.as_bytes());
                    key.extend_from_slice(result.node().as_bytes());
                    let bytes = tr.get(&key, false).await.unwrap().unwrap().to_vec();
                    return (result, bytes);
                }
                assert!(!matches!(
                    state.task.state,
                    NodeMutationTaskState::Cancelled | NodeMutationTaskState::Rejected
                ));
            }
            panic!("原删除未完成");
        })
    }

    fn verify_deleted_reclaim_fault<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        parent: &'a crate::managed_catalog::ReclaimReservation,
        source: uuid::Uuid,
        deleted: &'a (crate::managed_catalog::NodeMutationDeletion, Vec<u8>),
        cluster: &'a str,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let db = foundationdb::Database::from_path(cluster).unwrap();
            let mut key = b"tape-rs/catalog/22/N/".to_vec();
            key.extend_from_slice(control.route.route.namespace.as_bytes());
            key.extend_from_slice(deleted.0.node().as_bytes());
            let mut prefix = b"tape-rs/catalog/22/V/".to_vec();
            prefix.extend_from_slice(source.as_bytes());
            let mut end = prefix.clone();
            while end.last() == Some(&255) {
                end.pop();
            }
            *end.last_mut().unwrap() += 1;
            let range = foundationdb::RangeOption {
                begin: foundationdb::KeySelector::first_greater_or_equal(prefix.clone()),
                end: foundationdb::KeySelector::first_greater_or_equal(end),
                limit: Some(16),
                ..Default::default()
            };
            let tr = db.create_trx().unwrap();
            let before: Vec<_> = tr
                .get_range(&range, 1, false)
                .await
                .unwrap()
                .iter()
                .map(|v| (v.key().to_vec(), v.value().to_vec()))
                .collect();
            assert_eq!(before.len(), 1, "此夹具应只剩已删除文件的源引用");
            let mut copies = Vec::new();
            for (reverse, _) in &before {
                let mut copy = b"tape-rs/catalog/22/R/".to_vec();
                copy.extend_from_slice(&reverse[prefix.len() + 16..]);
                let bytes = tr.get(&copy, false).await.unwrap().unwrap().to_vec();
                copies.push((copy, bytes));
            }
            assert_eq!(
                tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                deleted.1
            );
            let mut bad = deleted.1.clone();
            *bad.last_mut().unwrap() ^= 1;
            tr.set(&key, &bad);
            tr.commit().await.unwrap();
            let commands = fixture.commands.lock().unwrap().len();
            let result =
                fdb_reclaim::advance(&mut fixture.owner, &fixture.permit, control, status).await;
            assert!(
                matches!(result,Err(ReadError::Lost(ref e)) if e.contains("删除")),
                "{result:?}"
            );
            assert!(
                fixture.commands.lock().unwrap()[commands..]
                    .iter()
                    .all(|(_, op)| !matches!(*op, 0x04 | 0x0a | 0x10 | 0x55 | 0x8d)),
                "删除证据损坏只能观察，不能写介质"
            );
            let tr = db.create_trx().unwrap();
            let after: Vec<_> = tr
                .get_range(&range, 1, false)
                .await
                .unwrap()
                .iter()
                .map(|v| (v.key().to_vec(), v.value().to_vec()))
                .collect();
            assert_eq!(after, before, "出错不能清除源 V");
            for (key, bytes) in copies {
                assert_eq!(
                    tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                    bytes,
                    "出错不能改写原 C"
                );
            }
            assert_eq!(
                tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                bad,
                "损坏证据不能自动补齐"
            );
            assert!(
                control
                    .catalog
                    .reclaim_format_plan(parent, fixture.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            tr.set(&key, &deleted.1);
            tr.commit().await.unwrap();
        })
    }

    fn renew_native_parent_permit(fixture: &Fixture) {
        let phase = if fixture.permit.allows(fixture.owner.round, Phase::Serving) {
            Phase::Serving
        } else {
            Phase::Execution
        };
        assert!(
            fixture.permit.renew(
                fixture.owner.round,
                phase,
                Instant::now() + Duration::from_secs(60)
            ),
            "父流程夹具按当前阶段续租，不能降级或复活失效许可"
        );
    }

    fn verify_destination_reclaim_fault<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        parent: &'a crate::managed_catalog::ReclaimReservation,
        barcode: &'a str,
        cluster: &'a str,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let db = foundationdb::Database::from_path(cluster).unwrap();
            let mut key = b"tape-rs/catalog/22/O/".to_vec();
            key.extend_from_slice(control.serial.as_bytes());
            key.extend_from_slice(b"\0\0/tape-monitor/");
            key.extend_from_slice(control.route.route.namespace.as_bytes());
            key.extend_from_slice(barcode.as_bytes());
            key.extend_from_slice(b"\0\0");
            let tr = db.create_trx().unwrap();
            let original = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            let mut bad: Value = serde_json::from_slice(&original).unwrap();
            bad["requires_validation"] = json!(true);
            tr.set(&key, bad.to_string().as_bytes());
            tr.commit().await.unwrap();
            let commands = fixture.commands.lock().unwrap().len();
            let result =
                fdb_reclaim::advance(&mut fixture.owner, &fixture.permit, control, status).await;
            assert!(matches!(result, Err(ReadError::Busy(_))), "{result:?}");
            assert!(
                fixture.commands.lock().unwrap()[commands..]
                    .iter()
                    .all(|(_, op)| !matches!(*op, 0x04 | 0x0a | 0x10 | 0x55 | 0x8d)),
                "目的带待验证时不能初始化源带"
            );
            assert!(
                control
                    .catalog
                    .reclaim_format_plan(parent, fixture.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            let task = control
                .catalog
                .management_task(
                    &control.serial,
                    control.route,
                    parent.intent()["epoch"].as_u64().unwrap(),
                    parent.intent()["operation"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task["status"], "running");
            assert!(
                task["progress"]["dependency"]["reason"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
            );
            let tr = db.create_trx().unwrap();
            assert_eq!(
                tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                bad.to_string().as_bytes(),
                "父任务不能清除目的带故障"
            );
            tr.set(&key, &original); // Restore only this test's injected monitor fault.
            tr.commit().await.unwrap();
        })
    }

    fn set_native_parent_mount_limit<'a>(
        fixture: &'a Fixture,
        control: &'a FdbOwnerControl,
        policy: &'a crate::managed_catalog::ManagementRequest,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let catalog = &control.catalog;
            catalog
                .admit_management(&control.serial, control.route, fixture.owner.round, policy)
                .await
                .unwrap();
            assert_eq!(
                catalog
                    .apply_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        policy.epoch(),
                        policy.operation()
                    )
                    .await
                    .unwrap()["status"],
                "succeeded"
            );
        })
    }
    fn verify_native_parent_mount_wait<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        parent: &'a crate::managed_catalog::ReclaimReservation,
        original: &'a crate::managed_catalog::ManagementRequest,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let catalog = &control.catalog;
            let commands = fixture.commands.lock().unwrap().len();
            assert!(matches!(
                fdb_reclaim::advance(&mut fixture.owner, &fixture.permit, control, status).await,
                Err(ReadError::Busy(_))
            ));
            assert!(
                fixture.commands.lock().unwrap()[commands..]
                    .iter()
                    .all(|(_, op)| !matches!(*op, 0x1b | 0xa5 | 0x04 | 0x0a | 0x10 | 0x55 | 0x8d))
            );
            assert!(
                catalog
                    .reclaim_layout(parent, fixture.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            let task = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task["status"], "running");
            assert!(
                task["progress"]["dependency"]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("挂载上限")
            );
            assert_eq!(
                catalog
                    .cancel_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        original.epoch(),
                        original.operation()
                    )
                    .await
                    .unwrap()["status"],
                "cancelled"
            );
        })
    }

    fn verify_stale_queued_reclaim<'a>(
        fixture: &'a Fixture,
        control: &'a FdbOwnerControl,
        original: &'a crate::managed_catalog::ManagementRequest,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let catalog = &control.catalog;
            let commands = fixture.commands.lock().unwrap().len();
            let db = foundationdb::Database::from_path(&std::env::var("FDB_TEST_CLUSTER").unwrap())
                .unwrap();
            for kind in *b"jf" {
                let mut key = b"tape-rs/catalog/22/@/".to_vec();
                key.extend_from_slice(control.route.route.namespace.as_bytes());
                key.extend_from_slice(&[b'R', kind]);
                key.extend_from_slice(&original.epoch().to_be_bytes());
                key.extend_from_slice(original.operation().as_bytes());
                let tr = db.create_trx().unwrap();
                assert!(tr.get(&key, false).await.unwrap().is_none());
                tr.set(&key, b"isolated-original-state-marker");
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .start_reclaim_management(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            original.epoch(),
                            original.operation()
                        )
                        .await
                        .is_err(),
                    "已有原父/结果不能按未执行失败"
                );
                assert_eq!(
                    catalog
                        .admit_management(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            original
                        )
                        .await
                        .unwrap()["status"],
                    "queued"
                );
                let tr = db.create_trx().unwrap();
                assert_eq!(
                    tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                    b"isolated-original-state-marker"
                );
                tr.clear(&key); // Restore only this private-FDB fault injection.
                tr.commit().await.unwrap();
            }
            assert!(
                catalog
                    .start_reclaim_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        original.epoch(),
                        original.operation()
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            let task = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task["status"], "failed");
            assert_eq!(task["request"], *original.value());
            assert_eq!(task["result"]["source_unchanged"], true);
            assert!(
                catalog
                    .start_reclaim_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        original.epoch(),
                        original.operation()
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                catalog
                    .claim_reclaim_reservations_page(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        None,
                        16
                    )
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert!(
                catalog
                    .require_active_library_readable(
                        &control.serial,
                        control.route,
                        fixture.owner.round
                    )
                    .await
                    .is_ok()
            );
            assert!(
                fixture.permit.allows(fixture.owner.round, Phase::Serving)
                    || fixture.permit.allows(fixture.owner.round, Phase::Execution)
            );
            assert_eq!(fixture.commands.lock().unwrap().len(), commands);
        })
    }

    fn lengthen_native_target_history<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        pool: uuid::Uuid,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            // Deliberate external Full writes on Sim only, followed by the
            // supported assigned-tape import to refresh the original FDB head.
            let changer = fixture
                .owner
                .devices
                .iter()
                .find(|d| d.kind == DeviceKind::Changer)
                .unwrap();
            let mut robot = MediumChanger::new(changer.dev.as_ref());
            robot.load_address_map().unwrap();
            let rows = robot.read_all_status().unwrap();
            let media = rows
                .iter()
                .find(|r| r.full && r.volume_tag.as_deref() == Some("LAYOUTL8"))
                .unwrap();
            let address =
                if media.element_type == crate::changer::element::ElementType::DataTransfer {
                    media.address
                } else {
                    let drive = rows
                        .iter()
                        .find(|r| {
                            r.element_type == crate::changer::element::ElementType::DataTransfer
                                && !r.full
                        })
                        .unwrap();
                    robot.move_medium(media.address, drive.address).unwrap();
                    drive.address
                };
            let mapping = inventory_elements(
                &fixture.owner.devices,
                &robot.read_all_status().unwrap(),
                true,
            )
            .unwrap();
            let dev = mapping
                .drives
                .iter()
                .find(|d| d.addr == address)
                .unwrap()
                .dev;
            let device = fixture.owner.devices[dev].dev.as_ref();
            TapeDrive::new(device).load().unwrap();
            let mut volume = LtfsVolume::mount(device).unwrap();
            volume.set_reservation_guard(Some(fixture.owner.key));
            for i in 0..70 {
                volume
                    .set_node_xattr(
                        "/",
                        crate::ltfs::index::Xattr {
                            key: "destination-history".into(),
                            value: i.to_string(),
                            base64: false,
                        },
                        i == 0,
                        i > 0,
                    )
                    .unwrap();
                volume.commit().unwrap();
            }
            drop(volume);
            renew_native_parent_permit(fixture);
            let refresh = crate::managed_catalog::ManagementRequest::parse(json!({
                "version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),
                "epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":{
                    "kind":"media-import","pool":pool.to_string(),"barcode":"LAYOUTL8","volume":null,
                    "path":"/","merge_tapes":true
                }
            })).unwrap();
            assert_eq!(
                execute(fixture, control, status, &refresh).await["status"],
                "succeeded"
            );
        })
    }

    struct ParentPermitHeartbeat {
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl ParentPermitHeartbeat {
        fn new(fixture: &Fixture) -> Self {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let permit = fixture.permit.clone();
            let round = fixture.owner.round;
            let thread = std::thread::spawn(move || {
                // These Sim cases have no ownership change. Model the Raft
                // thread renewing the same lease during a long device phase.
                while !flag.load(std::sync::atomic::Ordering::Acquire) {
                    permit.renew(
                        round,
                        Phase::Serving,
                        Instant::now() + Duration::from_secs(60),
                    );
                    std::thread::park_timeout(Duration::from_millis(250));
                }
            });
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for ParentPermitHeartbeat {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.thread().unpark();
                thread.join().unwrap();
            }
        }
    }

    async fn native_reclaim_parent_case(
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        cluster: &str,
        case: NativeParentCase,
    ) {
        let raw_spool_case = matches!(
            case,
            NativeParentCase::RawBodySingleRead | NativeParentCase::RawBodyBudget
        );
        let unassign = case == NativeParentCase::Unassign || raw_spool_case;
        let interrupt_child = matches!(
            case,
            NativeParentCase::ChildHa | NativeParentCase::FinishHa | NativeParentCase::CancelledHa
        );
        let interrupt_finish = case == NativeParentCase::FinishHa;
        let cancel_child = case == NativeParentCase::CancelledHa;
        let native_links = case == NativeParentCase::NativeLinks;
        use crate::managed_catalog::{ManagementRequest, ReclaimSpec};
        let (mut fixture, control, pool, status) = Box::pin(native_fixture_from(
            catalog.clone(),
            None,
            Fixture::with_drives(2),
        ))
        .await;
        let _heartbeat =
            (case.long_history() || raw_spool_case).then(|| ParentPermitHeartbeat::new(&fixture));
        if case == NativeParentCase::VendorDvcid {
            fixture.lib.set_quirks(crate::scsi::sim::SimQuirks {
                dvcid_vendor_model: true,
                ..Default::default()
            });
        }
        fixture.format(uuid::Uuid::new_v4());
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        if case == NativeParentCase::EeAlias {
            volume.set_hash_policy(crate::ltfs::volume::HashPolicy {
                sha256: true,
                md5: true,
            });
            volume
                .append_bytes("/.LTFSEE_DATA/body", b"layout-source")
                .unwrap();
            volume.add_symlink("/data", ".LTFSEE_DATA/body").unwrap();
        } else {
            volume
                .append_file("/data", &mut std::io::Cursor::new(b"layout-source"))
                .unwrap();
        }
        if case == NativeParentCase::DeletedObject {
            volume
                .append_bytes("/removed", b"deliberately removed original body")
                .unwrap();
        }
        if native_links {
            volume.create_directory("/links").unwrap();
            volume.create_directory("/links/empty").unwrap();
            volume
                .add_symlink("/links/empty/native", "../../data")
                .unwrap();
        }
        if matches!(
            case,
            NativeParentCase::SourceCaptureReuse
                | NativeParentCase::SourceHistoryHints
                | NativeParentCase::SourceSessionInvalidation
                | NativeParentCase::SourceOtherHandleChange
                | NativeParentCase::SourceOtherHandleStableVcr
                | NativeParentCase::SourceNoChangeEvidence
                | NativeParentCase::SourceErrorRecovery
        ) {
            // Root, body and these directories exceed the real 32-node seal
            // page. The next parent advance must continue the original page
            // without moving or changing the source medium in between.
            for i in 0..32 {
                volume.create_directory(&format!("/empty-{i:02}")).unwrap();
            }
        }
        if unassign {
            volume
                .append_bytes("/.LTFSEE_DATA/orphan", b"unmapped original bytes")
                .unwrap();
            volume
                .set_node_xattr(
                    "/.LTFSEE_DATA",
                    crate::ltfs::index::Xattr {
                        key: "tapers.fdb.eeInternalDirectory".into(),
                        value: "1".into(),
                        base64: false,
                    },
                    true,
                    false,
                )
                .unwrap();
        }
        volume.commit().unwrap();
        if matches!(
            case,
            NativeParentCase::LongHistory | NativeParentCase::SourceCaptureReuse
        ) {
            for i in 0..70 {
                volume
                    .set_node_xattr(
                        "/",
                        crate::ltfs::index::Xattr {
                            key: "source-history".into(),
                            value: i.to_string(),
                            base64: false,
                        },
                        i == 0,
                        i > 0,
                    )
                    .unwrap();
                volume.commit().unwrap();
            }
        }
        let orphan = volume
            .index()
            .find_file(".LTFSEE_DATA/orphan")
            .map(|f| f.meta.file_uid);
        let raw_body_position = raw_spool_case.then(|| {
            let file = volume.index().find_file(".LTFSEE_DATA/orphan").unwrap();
            assert_eq!(file.extents.len(), 1);
            let extent = &file.extents[0];
            assert_eq!(extent.partition, volume.label().data_partition);
            assert_eq!(extent.byte_offset, 0);
            assert_eq!(extent.byte_count, b"unmapped original bytes".len() as u64);
            (1, extent.start_block)
        });
        drop(volume);
        let request = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("LAYOUTL8", 128 << 20), 1)
            .unwrap();
        let assign = request(
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LAYOUTL8","volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":false}),
        );
        let assigned = execute(&mut fixture, &control, &status, &assign).await;
        assert_eq!(assigned["status"], "succeeded", "{assigned}");
        if case.long_history() {
            lengthen_native_target_history(&mut fixture, &control, &status, pool.id).await;
        }
        let source = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let target = catalog
            .tape_assignment(&control.serial, "LAYOUTL8")
            .await
            .unwrap()
            .unwrap();
        let deleted = if case == NativeParentCase::DeletedObject {
            Some(
                delete_native_parent_fixture(&mut fixture, &control, &status, pool.id, cluster)
                    .await,
            )
        } else {
            None
        };
        let old_history = if case == NativeParentCase::ObsoleteVersions {
            let full = crate::managed_catalog::pool_catalog_path(pool.id, b"/data").unwrap();
            let Some(crate::managed_catalog::ResolvedPath::File { id, head, .. }) = catalog
                .resolve_active_path_for_route(&control.serial, &full, control.route)
                .await
                .unwrap()
            else {
                panic!("source")
            };
            let spec = crate::managed_catalog::ActiveUploadSpec {
                pool_id: pool.id,
                path: b"/data".to_vec(),
                operation_id: uuid::Uuid::new_v4(),
                expected_version: Some(head.version),
                size: b"layout-source".len() as u64,
                digest: crate::managed_catalog::ContentDigest::Sha256(
                    sha2::Sha256::digest(b"layout-source").into(),
                ),
            };
            let intent = catalog
                .prepare_active_upload(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &source.barcode,
                    &spec,
                )
                .await
                .unwrap();
            let mut file = crate::local_read_storage::download_file(
                fixture.owner.files.spool_dir(),
                spec.size,
                control.storage,
            )
            .unwrap();
            std::io::Write::write_all(&mut file, b"layout-source").unwrap();
            std::io::Seek::rewind(&mut file).unwrap();
            assert_eq!(
                fdb_writing::write(
                    &mut fixture.owner,
                    &fixture.permit,
                    &control,
                    &status,
                    intent.request(),
                    file
                )
                .await
                .unwrap(),
                head.version + 1
            );
            Some((id, head.copy_id, intent.request()))
        } else {
            None
        };
        if case == NativeParentCase::MountLimit {
            let policy = request(
                json!({"kind":"pool","id":pool.id.to_string(),"revision":pool.revision,"patch":{"mount_limit":1}}),
            );
            set_native_parent_mount_limit(&fixture, &control, &policy).await;
        }
        let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":unassign})).unwrap();
        let mut change = spec.to_json();
        if case == NativeParentCase::StaleQueued {
            change["revision"] = json!(source.revision + 1);
        }
        let original = request(change);
        catalog
            .admit_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                &original,
            )
            .await
            .unwrap();
        if case == NativeParentCase::StaleQueued {
            verify_stale_queued_reclaim(&fixture, &control, &original).await;
            for field in ["barcode", "volume"] {
                let mut value = original.value().clone();
                value["operation"] = json!(uuid::Uuid::new_v4().to_string());
                value["change"]["revision"] = json!(source.revision);
                value["change"][field] = if field == "barcode" {
                    json!("MISSINGL8")
                } else {
                    json!(uuid::Uuid::new_v4().to_string())
                };
                let invalid = ManagementRequest::parse(value).unwrap();
                catalog
                    .admit_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        &invalid,
                    )
                    .await
                    .unwrap();
                verify_stale_queued_reclaim(&fixture, &control, &invalid).await;
            }
            return;
        }
        let parent = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        if case == NativeParentCase::StallDiagnostics {
            let commands = fixture.commands.lock().unwrap().clone();
            let original_status = catalog
                .reserved_reclaim_status(&parent, fixture.owner.round)
                .await
                .unwrap();
            assert!(
                catalog
                    .reclaim_source_coverage_progress(&parent, fixture.owner.round)
                    .await
                    .is_err(),
                "缺少完整封存清单时不能制造源覆盖进度"
            );
            assert!(
                catalog
                    .reclaim_source_coverage_progress(&parent, fixture.owner.round + 1)
                    .await
                    .is_err(),
                "其他owner不能读原coverage进度"
            );
            let first_event = catalog
                .record_reclaim_failure(&parent, fixture.owner.round, "same actual error")
                .await
                .unwrap();
            let snapshot = catalog
                .reclaim_dependency_snapshot(&parent, fixture.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(snapshot.2, first_event);
            let second_event = catalog
                .record_reclaim_failure(&parent, fixture.owner.round, "same actual error")
                .await
                .unwrap();
            assert_ne!(first_event, second_event, "新同文事件仍有新token");
            assert!(
                !catalog
                    .clear_reclaim_dependency_if_unchanged(
                        &parent,
                        fixture.owner.round,
                        first_event
                    )
                    .await
                    .unwrap(),
                "原snapshot不能清同期新同文错误"
            );
            assert!(
                catalog
                    .clear_reclaim_dependency_if_unchanged(
                        &parent,
                        fixture.owner.round + 1,
                        second_event
                    )
                    .await
                    .is_err(),
                "旧snapshot不是其他owner的清理授权"
            );
            assert!(
                catalog
                    .clear_reclaim_dependency_if_unchanged(
                        &parent,
                        fixture.owner.round,
                        second_event
                    )
                    .await
                    .unwrap()
            );
            let reason = "测试范围：原父/封存/覆盖/元数据汇总检查点未变";
            assert!(
                catalog
                    .update_reclaim_dependency_if(&parent, fixture.owner.round, None, Some(reason))
                    .await
                    .unwrap()
            );
            let first = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap()["progress"]["dependency"]
                .clone();
            for _ in 0..210 {
                assert!(
                    !catalog
                        .update_reclaim_dependency_if(
                            &parent,
                            fixture.owner.round,
                            None,
                            Some(reason),
                        )
                        .await
                        .unwrap()
                );
            }
            let same = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap()["progress"]["dependency"]
                .clone();
            assert_eq!(same, first, "重复观察不能改写原诊断或observed_at");
            catalog
                .set_reclaim_dependency(&parent, fixture.owner.round, Some("实际设备依赖不可用"))
                .await
                .unwrap();
            for next in [None, Some(reason)] {
                assert!(
                    !catalog
                        .update_reclaim_dependency_if(
                            &parent,
                            fixture.owner.round,
                            Some(reason),
                            next,
                        )
                        .await
                        .unwrap(),
                    "进度诊断不能清除或覆盖实际依赖"
                );
            }
            let dependency = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap()["progress"]["dependency"]
                .clone();
            assert_eq!(dependency["reason"], "实际设备依赖不可用");
            assert!(
                catalog
                    .update_reclaim_dependency_if(
                        &parent,
                        fixture.owner.round + 1,
                        Some("实际设备依赖不可用"),
                        None,
                    )
                    .await
                    .is_err(),
                "非原owner不能更新诊断"
            );
            catalog
                .set_reclaim_dependency(&parent, fixture.owner.round, None)
                .await
                .unwrap();
            assert!(
                catalog
                    .update_reclaim_dependency_if(&parent, fixture.owner.round, None, Some(reason))
                    .await
                    .unwrap()
            );
            assert!(
                catalog
                    .update_reclaim_dependency_if(&parent, fixture.owner.round, Some(reason), None)
                    .await
                    .unwrap(),
                "真实汇总进度变化后可以只清理自己的原诊断"
            );
            assert_eq!(
                catalog
                    .reserved_reclaim_status(&parent, fixture.owner.round)
                    .await
                    .unwrap(),
                original_status,
                "诊断不能改变原parent、phase或children"
            );
            assert_eq!(*fixture.commands.lock().unwrap(), commands);
            return;
        }
        let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let history_fault = matches!(
            case,
            NativeParentCase::LongHistoryT2 | NativeParentCase::LongHistoryIp
        );
        let mut history_fault_seen = false;
        let history_fault_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if history_fault {
            let current = inventory(&fixture.owner.devices).unwrap();
            let dest = current
                .drives
                .iter()
                .find(|d| {
                    d.loaded
                        .as_ref()
                        .is_some_and(|(barcode, _)| barcode == "LAYOUTL8")
                })
                .expect("long-history destination remains loaded after refresh");
            assert!(dest.dev < fixture.lib.drive_count());
            let dev = dest.dev;
            let inner = std::mem::replace(
                &mut fixture.owner.devices[dev].dev,
                Box::new(fixture.lib.drive_as(dev, 1)),
            );
            fixture.owner.devices[dev].dev = Box::new(parent_recovery::FailOneAppend::new(
                inner,
                if case == NativeParentCase::LongHistoryT2 {
                    crate::scsi::cdb::opcode::WRITE_FILEMARKS_6
                } else {
                    crate::scsi::cdb::opcode::WRITE_6
                },
                1,
                history_fault_fired.clone(),
            ));
        }
        if case == NativeParentCase::MountLimit {
            verify_native_parent_mount_wait(&mut fixture, &control, &status, &parent, &original)
                .await;
            return;
        }
        if interrupt_child {
            for index in 0..fixture.lib.drive_count() {
                let inner = std::mem::replace(
                    &mut fixture.owner.devices[index].dev,
                    Box::new(fixture.lib.drive_as(index, 1)),
                );
                let boundary = if cancel_child {
                    parent_recovery::AfterChildBarrier::before_body
                } else {
                    parent_recovery::AfterChildBarrier::new
                };
                fixture.owner.devices[index].dev = Box::new(boundary(
                    inner,
                    catalog.clone(),
                    parent.clone(),
                    fixture.owner.round,
                    control.route,
                    interrupted.clone(),
                ));
            }
        }
        let finish_interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut deleted_reference_waits = 0;
        let mut deleted_fault_checked = false;
        let source_trace = Arc::new(std::sync::Mutex::new(Vec::new()));
        let no_source_epoch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let raw_body_reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let raw_body_physical_reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        if raw_spool_case {
            install_raw_body_counter(
                &mut fixture,
                &target.barcode,
                raw_body_position.unwrap(),
                raw_body_reads.clone(),
                raw_body_physical_reads.clone(),
            );
        }
        let mut raw_budget_hook = (case == NativeParentCase::RawBodyBudget).then(|| {
            super::super::fdb_reclaim_spool_test_hook::install(
                &parent,
                orphan.unwrap(),
                fixture.commands.clone(),
                raw_body_reads.clone(),
            )
        });
        let mut raw_budget_recovered = false;
        if case == NativeParentCase::SourceVcrUnsupported {
            let current = inventory(&fixture.owner.devices).unwrap();
            let target_drive = current
                .drives
                .iter()
                .find(|d| {
                    d.loaded
                        .as_ref()
                        .is_some_and(|(barcode, _)| barcode == &target.barcode)
                })
                .expect("assigned target remains loaded")
                .dev;
            let source_drive = (0..fixture.lib.drive_count())
                .find(|index| *index != target_drive)
                .unwrap();
            let inner = std::mem::replace(
                &mut fixture.owner.devices[source_drive].dev,
                Box::new(fixture.lib.drive_as(source_drive, 1)),
            );
            fixture.owner.devices[source_drive].dev = Box::new(UnsupportedSourceVcr {
                inner,
                rejected: std::sync::atomic::AtomicU64::new(0),
            });
        }
        if matches!(case, NativeParentCase::SourceErrorRecovery) {
            let current = inventory(&fixture.owner.devices).unwrap();
            let target_drive = current
                .drives
                .iter()
                .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &target.barcode))
                .unwrap()
                .dev;
            let source_drive = (0..fixture.lib.drive_count())
                .find(|d| *d != target_drive)
                .unwrap();
            let inner = std::mem::replace(
                &mut fixture.owner.devices[source_drive].dev,
                Box::new(fixture.lib.drive_as(source_drive, 1)),
            );
            fixture.owner.devices[source_drive].dev = Box::new(SourceReadFailure {
                inner,
                fail: std::sync::atomic::AtomicBool::new(true),
                failures: std::sync::atomic::AtomicU64::new(0),
            });
        }
        let fresh_capture_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if matches!(
            case,
            NativeParentCase::SourceFreshTokenGap | NativeParentCase::SourcePostVcrGap
        ) {
            let current = inventory(&fixture.owner.devices).unwrap();
            let target_drive = current
                .drives
                .iter()
                .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &target.barcode))
                .unwrap()
                .dev;
            let source_drive = (0..fixture.lib.drive_count())
                .find(|d| *d != target_drive)
                .unwrap();
            let inner = std::mem::replace(
                &mut fixture.owner.devices[source_drive].dev,
                Box::new(fixture.lib.drive_as(source_drive, 1)),
            );
            fixture.owner.devices[source_drive].dev = Box::new(FreshCaptureMutation {
                inner,
                lib: fixture.lib.clone(),
                drive: source_drive,
                key: fixture.owner.key,
                post_vcr: case == NativeParentCase::SourcePostVcrGap,
                token_calls: std::sync::atomic::AtomicUsize::new(0),
                armed: std::sync::atomic::AtomicBool::new(false),
                rejected: std::sync::atomic::AtomicU64::new(0),
                fired: fresh_capture_fired.clone(),
            });
        }
        if matches!(
            case,
            NativeParentCase::SourceCaptureReuse
                | NativeParentCase::SourceHistoryHints
                | NativeParentCase::SourceSessionInvalidation
                | NativeParentCase::SourceOtherHandleChange
                | NativeParentCase::SourceOtherHandleStableVcr
                | NativeParentCase::SourceFreshTokenGap
                | NativeParentCase::SourcePostVcrGap
                | NativeParentCase::SourceNoChangeEvidence
                | NativeParentCase::SourceErrorRecovery
        ) {
            for index in 0..fixture.lib.drive_count() {
                let inner = std::mem::replace(
                    &mut fixture.owner.devices[index].dev,
                    Box::new(fixture.lib.drive_as(index, 1)),
                );
                fixture.owner.devices[index].dev = Box::new(ParentSourceTrace {
                    inner,
                    drive: index,
                    commands: source_trace.clone(),
                    no_epoch: no_source_epoch.clone(),
                    trusted_change_evidence: !matches!(
                        case,
                        NativeParentCase::SourceNoChangeEvidence
                            | NativeParentCase::SourceHistoryHints
                    ),
                });
            }
        }
        if matches!(
            case,
            NativeParentCase::SourceOtherHandleStableVcr
                | NativeParentCase::SourceFreshTokenGap
                | NativeParentCase::SourcePostVcrGap
        ) {
            // Establish the constant-VCR quirk before the original capture, so
            // its token changes later only because of the alternate handle.
            fixture.lib.set_quirks(crate::scsi::sim::SimQuirks {
                vcr_never_changes: true,
                ..Default::default()
            });
        }
        let fresh_parent_before = if matches!(
            case,
            NativeParentCase::SourceFreshTokenGap | NativeParentCase::SourcePostVcrGap
        ) {
            Some(
                catalog
                    .reserved_reclaim_status(&parent, fixture.owner.round)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let reclaim_stage_started = Instant::now();
        for step in 0..64 {
            let trace_start = source_trace.lock().unwrap().len();
            if ((case == NativeParentCase::Unassign || interrupt_child) && step == 1)
                || (interrupt_finish && step == 2)
            {
                let round = fixture.owner.round + 100;
                let permit = Arc::new(Permission::default());
                permit.renew(
                    round,
                    Phase::Fencing,
                    Instant::now() + Duration::from_secs(60),
                );
                let adopted = takeover(
                    &Provider {
                        lib: fixture.lib.clone(),
                        commands: fixture.commands.clone(),
                    },
                    &fixture.opts,
                    round,
                    Some(&permit),
                    fixture.owner.files.clone(),
                )
                .unwrap();
                permit.renew(
                    round,
                    Phase::Execution,
                    Instant::now() + Duration::from_secs(60),
                );
                catalog
                    .claim_active_owner_epoch(&control.serial, round)
                    .await
                    .unwrap();
                let before = fixture.commands.lock().unwrap().clone();
                assert!(
                    Box::pin(fdb_reclaim::advance(
                        &mut fixture.owner,
                        &fixture.permit,
                        &control,
                        &status
                    ))
                    .await
                    .is_err()
                );
                assert_eq!(
                    *fixture.commands.lock().unwrap(),
                    before,
                    "旧 owner 被拒绝时不能发 CDB"
                );
                fixture.owner = adopted;
                fixture.permit = permit;
                if interrupt_child && step == 1 {
                    catalog
                        .claim_reclaim_reservations_page(
                            &control.serial,
                            control.route,
                            round,
                            None,
                            16,
                        )
                        .await
                        .unwrap();
                    let requests = catalog
                        .reclaim_uploads_page(&parent, round, None, 16)
                        .await
                        .unwrap();
                    let request = &requests.items[0];
                    let mut attempt = catalog
                        .active_file_attempt(request, control.route)
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(attempt.owner_epoch < round);
                    attempt.owner_epoch = round;
                    let before = fixture.commands.lock().unwrap().clone();
                    assert!(
                        catalog
                            .check_active_write_group(
                                &control.serial,
                                control.route,
                                round,
                                &[attempt]
                            )
                            .await
                            .unwrap_err()
                            .to_string()
                            .contains("上传原物理尝试已改变"),
                        "普通写守卫不能接纳未结清的旧 owner 尝试"
                    );
                    let mut foreign = request.clone();
                    foreign.expected_version = Some(foreign.expected_version.unwrap() + 1);
                    assert!(
                        catalog
                            .require_reclaim_upload_recovery_lane(&parent, round, &foreign)
                            .await
                            .is_err()
                    );
                    assert_eq!(
                        *fixture.commands.lock().unwrap(),
                        before,
                        "拒绝改动的原请求不发 CDB"
                    );
                    if interrupt_finish && step == 1 {
                        for index in 0..fixture.lib.drive_count() {
                            let inner = std::mem::replace(
                                &mut fixture.owner.devices[index].dev,
                                Box::new(fixture.lib.drive_as(index, 1)),
                            );
                            fixture.owner.devices[index].dev =
                                Box::new(parent_recovery::AfterChildBarrier::after_terminal(
                                    inner,
                                    catalog.clone(),
                                    parent.clone(),
                                    round,
                                    control.route,
                                    finish_interrupted.clone(),
                                    (request.clone(), target.barcode.clone()),
                                ));
                        }
                    }
                }
            }
            renew_native_parent_permit(&fixture);
            if let Some(deleted) = &deleted
                && deleted_reference_waits == 1
                && !deleted_fault_checked
            {
                verify_deleted_reclaim_fault(
                    &mut fixture,
                    &control,
                    &status,
                    &parent,
                    source.volume_id.unwrap(),
                    deleted,
                    cluster,
                )
                .await;
                deleted_fault_checked = true;
            }
            if matches!(
                case,
                NativeParentCase::Default | NativeParentCase::ObsoleteVersions
            ) && step == 1
            {
                // Deliberate isolated-FDB fault: even an unclassified retained
                // V must block initialization. Restore only this injected row;
                // unknown or still-retained references cannot be reclaimed.
                let db = foundationdb::Database::from_path(cluster).unwrap();
                let mut key = b"tape-rs/catalog/22/V/".to_vec();
                key.extend_from_slice(source.volume_id.unwrap().as_bytes());
                key.extend_from_slice(b"isolated-reclaim-reference-fault");
                let tr = db.create_trx().unwrap();
                assert!(tr.get(&key, false).await.unwrap().is_none());
                tr.set(&key, b"isolated-test-only");
                tr.commit().await.unwrap();
                let before = fixture.commands.lock().unwrap().len();
                for _ in 0..2 {
                    renew_native_parent_permit(&fixture);
                    Box::pin(fdb_reclaim::advance(
                        &mut fixture.owner,
                        &fixture.permit,
                        &control,
                        &status,
                    ))
                    .await
                    .unwrap();
                    assert!(
                        catalog
                            .reclaim_format_plan(&parent, fixture.owner.round)
                            .await
                            .unwrap()
                            .is_none()
                    );
                    let waiting = catalog
                        .management_task(
                            &control.serial,
                            control.route,
                            original.epoch(),
                            original.operation(),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(waiting["status"], "running");
                    assert!(
                        waiting["progress"]["dependency"]["reason"]
                            .as_str()
                            .unwrap()
                            .contains("历史版本")
                    );
                }
                assert!(
                    fixture.commands.lock().unwrap()[before..]
                        .iter()
                        .all(|(_, op)| !matches!(*op, 0x04 | 0x0a | 0x10 | 0x55 | 0x8d)),
                    "引用等待只能观察，不能写介质或初始化"
                );
                let tr = db.create_trx().unwrap();
                assert_eq!(
                    tr.get(&key, false).await.unwrap().as_deref(),
                    Some(b"isolated-test-only".as_slice())
                );
                tr.clear(&key);
                tr.commit().await.unwrap();
                if case == NativeParentCase::Default {
                    renew_native_parent_permit(&fixture);
                    verify_destination_reclaim_fault(
                        &mut fixture,
                        &control,
                        &status,
                        &parent,
                        &target.barcode,
                        cluster,
                    )
                    .await;
                }
            }
            renew_native_parent_permit(&fixture);
            let healthy_default = (case == NativeParentCase::Default).then(|| {
                HealthyParentPermit::new(fixture.permit.clone(), fixture.owner.round, &control)
            });
            let result = Box::pin(fdb_reclaim::advance(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
            ))
            .await;
            if let Some(healthy_default) = healthy_default {
                let report = healthy_default.finish();
                eprintln!(
                    "DEFAULT_HEALTHY_QUORUM step={step} renewals={} failure={:?}",
                    report.renewals, report.failure
                );
                assert!(report.failure.is_none(), "健康多数派模拟已停止: {report:?}");
            }
            if raw_budget_hook.as_ref().is_some_and(|hook| hook.fired()) {
                assert!(
                    matches!(&result, Err(ReadError::Lost(reason)) if reason.contains("文件超过下载暂存单次上限")),
                    "must fail in the real download_file admission: {result:?}"
                );
                let hook = raw_budget_hook.take().unwrap();
                Box::pin(recover_raw_spool_budget_failure(
                    &mut fixture,
                    &control,
                    &parent,
                    &original,
                    hook,
                    &source.barcode,
                ))
                .await;
                install_raw_body_counter(
                    &mut fixture,
                    &target.barcode,
                    raw_body_position.unwrap(),
                    raw_body_reads.clone(),
                    raw_body_physical_reads.clone(),
                );
                raw_budget_recovered = true;
                continue;
            }
            if history_fault && !history_fault_seen && result.is_err() {
                assert!(
                    history_fault_fired.load(std::sync::atomic::Ordering::SeqCst),
                    "fault must reach the original managed transport"
                );
                let expected = if case == NativeParentCase::LongHistoryT2 {
                    "closing_fm"
                } else {
                    "index_partition"
                };
                assert!(
                    matches!(&result, Err(ReadError::Lost(reason)) if reason.contains(expected)),
                    "{result:?}"
                );
                let saved = catalog
                    .reclaim_metadata_checkpoint(&parent, fixture.owner.round)
                    .await
                    .unwrap()
                    .expect("original metadata attempt retained");
                assert!(saved.writing());
                assert!(
                    catalog
                        .reclaim_format_plan(&parent, fixture.owner.round)
                        .await
                        .unwrap()
                        .is_none()
                );
                history_fault_seen = true;
                continue;
            }
            if interrupt_child && step == 0 {
                assert!(matches!(result, Err(ReadError::Lost(_))), "{result:?}");
                assert!(interrupted.load(std::sync::atomic::Ordering::SeqCst));
                let requests = catalog
                    .reclaim_uploads_page(&parent, fixture.owner.round, None, 16)
                    .await
                    .unwrap();
                assert_eq!(requests.items.len(), 1);
                let request = &requests.items[0];
                assert!(
                    catalog
                        .active_upload_result(request, control.route)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(
                    catalog
                        .active_file_attempt(request, control.route)
                        .await
                        .unwrap()
                        .is_some()
                );
                fixture.permit.close(fixture.owner.round);
                continue;
            }
            if interrupt_finish && step == 1 {
                assert!(matches!(result, Err(ReadError::Lost(_))), "{result:?}");
                assert!(finish_interrupted.load(std::sync::atomic::Ordering::SeqCst));
                let requests = catalog
                    .reclaim_uploads_page(&parent, fixture.owner.round, None, 16)
                    .await
                    .unwrap();
                assert!(
                    catalog
                        .reclaim_upload_recovery_pending(
                            &parent,
                            fixture.owner.round,
                            &requests.items[0]
                        )
                        .await
                        .unwrap()
                );
                catalog
                    .begin_reclaim_upload_recovery(&parent, fixture.owner.round, &requests.items[0])
                    .await
                    .unwrap();
                assert!(
                    matches!(
                        catalog
                            .seal_reclaim_upload_cancellation(
                                &parent,
                                fixture.owner.round,
                                &requests.items[0]
                            )
                            .await,
                        Err(crate::managed_catalog::CatalogError::PoolBusy)
                    ),
                    "恢复收尾前不得提前释放原历史保留引用"
                );
                assert!(matches!(
                    catalog
                        .active_upload_result(&requests.items[0], control.route)
                        .await
                        .unwrap(),
                    Some(crate::managed_catalog::PublishOutcome::AlreadyApplied(_))
                ));
                assert!(
                    catalog
                        .active_tape_monitor(&control.serial, control.route, &target.barcode)
                        .await
                        .unwrap()
                        .unwrap()
                        .requires_validation
                );
                fixture.permit.close(fixture.owner.round);
                continue;
            }
            if matches!(
                case,
                NativeParentCase::SourceFreshTokenGap | NativeParentCase::SourcePostVcrGap
            ) && step == 0
            {
                assert!(
                    fresh_capture_fired.load(std::sync::atomic::Ordering::SeqCst),
                    "精确边界上的另handle实际修改已触发"
                );
                let current = inventory(&fixture.owner.devices).unwrap();
                let source_drive = current
                    .drives
                    .iter()
                    .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &source.barcode))
                    .unwrap()
                    .dev;
                let target_writes = source_trace.lock().unwrap()[trace_start..]
                    .iter()
                    .filter(|(d, cdb)| {
                        *d != source_drive && matches!(cdb.first(), Some(0x0a | 0x10 | 0x8d))
                    })
                    .count();
                let metadata = catalog
                    .reclaim_metadata_checkpoint(&parent, fixture.owner.round)
                    .await
                    .unwrap();
                let coverage = catalog
                    .reclaim_source_coverage_progress(&parent, fixture.owner.round)
                    .await;
                let parent_state = catalog
                    .reserved_reclaim_status(&parent, fixture.owner.round)
                    .await
                    .unwrap();
                eprintln!(
                    "fresh capture interposer {:?}: target_writes={target_writes}, metadata_planned={}, coverage_admitted={}, parent={parent_state}, result={result:?}",
                    if case == NativeParentCase::SourceFreshTokenGap {
                        "end-token"
                    } else {
                        "post-vcr-unsupported"
                    },
                    metadata.is_some(),
                    coverage.is_ok()
                );
                assert!(
                    result.is_err(),
                    "完整capture过程源变化必须先拒绝: {result:?}"
                );
                assert_eq!(target_writes, 0, "完整capture变化未结清不能写目标");
                assert!(metadata.is_none(), "不能建立metadata q/attempt");
                assert!(coverage.is_err(), "不能发布源封存/覆盖清单");
                assert_eq!(
                    &parent_state,
                    fresh_parent_before.as_ref().unwrap(),
                    "拒绝变化前不能推进原parent/children/退休状态"
                );
                assert!(
                    catalog
                        .reclaim_format_plan(&parent, fixture.owner.round)
                        .await
                        .unwrap()
                        .is_none()
                );
                return;
            }
            if case == NativeParentCase::SourceVcrUnsupported && step == 0 {
                let current = inventory(&fixture.owner.devices).unwrap();
                let source_drive = current
                    .drives
                    .iter()
                    .find(|d| {
                        d.loaded
                            .as_ref()
                            .is_some_and(|(barcode, _)| barcode == &source.barcode)
                    })
                    .expect("original source stays loaded")
                    .dev;
                let reader = super::super::super::fdb_read::ActiveMediaReader {
                    catalog: &catalog,
                    device: fixture.owner.devices[source_drive].dev.as_ref(),
                    serial: &control.serial,
                    route: control.route,
                    key: fixture.owner.key,
                    permission: &fixture.permit,
                    storage: control.storage,
                };
                let before = reader.device.session_epoch().unwrap();
                let captured = reader
                    .capture_reclaim_source(&parent, fixture.owner.files.spool_dir())
                    .await
                    .expect("原无缓存完整捕获在不支持VCR时仍可核验源卷");
                assert_eq!(captured.header().volume_uuid, source.volume_id.unwrap());
                assert!(reader.device.session_epoch().unwrap() > before);
                assert!(
                    result.is_ok(),
                    "明确不支持VCR只能禁用缓存，不能阻断原完整捕获: {result:?}"
                );
                assert!(fixture.owner.fdb_reclaim_source_session.is_none());
                assert!(
                    catalog
                        .reclaim_format_plan(&parent, fixture.owner.round)
                        .await
                        .unwrap()
                        .is_none()
                );
                return;
            }
            if matches!(case, NativeParentCase::SourceErrorRecovery) && step == 0 {
                assert!(
                    matches!(result, Err(ReadError::Lost(_))),
                    "注入源READ POSITION host error必须走真实Lost边界: {result:?}"
                );
                let task = catalog
                    .management_task(
                        &control.serial,
                        control.route,
                        original.epoch(),
                        original.operation(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(task["progress"]["dependency"]["reason"].as_str().is_some());
                eprintln!(
                    "source recovery original failure: {}",
                    task["progress"]["dependency"]
                );
                // Match the production Lost boundary: the old permission is
                // closed, ownership is fenced anew, and the old Active is
                // destroyed before the next original-parent advance.
                fixture.permit.close(fixture.owner.round);
                let round = fixture.owner.round + 100;
                let permit = Arc::new(Permission::default());
                permit.renew(
                    round,
                    Phase::Fencing,
                    Instant::now() + Duration::from_secs(60),
                );
                let adopted = takeover(
                    &Provider {
                        lib: fixture.lib.clone(),
                        commands: fixture.commands.clone(),
                    },
                    &fixture.opts,
                    round,
                    Some(&permit),
                    fixture.owner.files.clone(),
                )
                .unwrap();
                drop(std::mem::replace(&mut fixture.owner, adopted));
                fixture.permit = permit;
                fixture.permit.renew(
                    round,
                    Phase::Execution,
                    Instant::now() + Duration::from_secs(60),
                );
                catalog
                    .claim_active_owner_epoch(&control.serial, round)
                    .await
                    .unwrap();
                assert!(fixture.owner.fdb_reclaim_source_session.is_none());
                continue;
            }
            assert!(result.is_ok(), "parent step {step}: {result:?}");
            if case == NativeParentCase::SourceHistoryHints && step == 0 {
                Box::pin(verify_source_history_hints(
                    &fixture,
                    &control,
                    &parent,
                    &source.barcode,
                    &source_trace,
                    cluster,
                ))
                .await;
                return;
            }
            if case == NativeParentCase::SourceErrorRecovery && step == 1 {
                let task = catalog
                    .management_task(
                        &control.serial,
                        control.route,
                        original.epoch(),
                        original.operation(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    task["progress"]["dependency"].is_null(),
                    "原源读取错误已恢复且原封存游标已持久推进时应清旧诊断: {}",
                    task["progress"]["dependency"]
                );
                return;
            }
            if case == NativeParentCase::SourceNoChangeEvidence {
                assert!(
                    fixture.owner.fdb_reclaim_source_session.is_none(),
                    "缺少可靠变化能力不能保留缓存"
                );
                if step == 1 {
                    let current = inventory(&fixture.owner.devices).unwrap();
                    let source_drive = current
                        .drives
                        .iter()
                        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &source.barcode))
                        .unwrap()
                        .dev;
                    assert!(
                        source_trace.lock().unwrap()[trace_start..].iter().any(
                            |(drive, cdb)| *drive == source_drive
                                && cdb.first() == Some(&0x92)
                                && cdb[3] == 1
                                && u64::from_be_bytes(cdb[4..12].try_into().unwrap()) == 5
                        ),
                        "无可信变化能力的下一轮仍完整核验历史"
                    );
                    return;
                }
            }
            if matches!(
                case,
                NativeParentCase::SourceOtherHandleChange
                    | NativeParentCase::SourceOtherHandleStableVcr
            ) && step == 0
            {
                Box::pin(verify_source_other_handle_change(
                    &mut fixture,
                    &control,
                    &status,
                    &parent,
                    &source.barcode,
                    &source_trace,
                    case == NativeParentCase::SourceOtherHandleStableVcr,
                ))
                .await;
                return;
            }
            if case == NativeParentCase::SourceSessionInvalidation && step == 0 {
                verify_source_session_invalidation(
                    &mut fixture,
                    &control,
                    &parent,
                    &source.barcode,
                    &source_trace,
                    &no_source_epoch,
                )
                .await;
                return;
            }
            if case == NativeParentCase::SourceCaptureReuse && step == 1 {
                let current = inventory(&fixture.owner.devices).unwrap();
                let source_drive = current
                    .drives
                    .iter()
                    .find(|d| {
                        d.loaded
                            .as_ref()
                            .is_some_and(|(barcode, _)| barcode == &source.barcode)
                    })
                    .expect("original source stays loaded")
                    .dev;
                let commands = source_trace.lock().unwrap();
                let source_commands: Vec<_> = commands[trace_start..]
                    .iter()
                    .filter(|(drive, _)| *drive == source_drive)
                    .map(|(_, cdb)| cdb)
                    .collect();
                let oldest_history_reads = source_commands
                    .iter()
                    .filter(|cdb| {
                        cdb.first() == Some(&0x92)
                            && cdb[3] == 1
                            && u64::from_be_bytes(cdb[4..12].try_into().unwrap()) == 5
                    })
                    .count();
                let holder_checks = source_commands
                    .iter()
                    .filter(|cdb| cdb.first() == Some(&0x5e))
                    .count();
                println!(
                    "source inventory page 2: source_commands={}, oldest_history_reads={oldest_history_reads}, holder_checks={holder_checks}",
                    source_commands.len()
                );
                assert!(holder_checks > 0, "复用捕获仍须实际核验设备执行权");
                assert_eq!(
                    oldest_history_reads, 0,
                    "原源未变化的第二封存页不能重扫70代完整历史"
                );
                assert_eq!(fixture.formats(), 2, "分页捕获不能初始化原源卷");
                let task = catalog
                    .management_task(
                        &control.serial,
                        control.route,
                        original.epoch(),
                        original.operation(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(task["status"], "running");
                assert!(
                    catalog
                        .reclaim_format_plan(&parent, fixture.owner.round)
                        .await
                        .unwrap()
                        .is_none()
                );
                return;
            }
            let task = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            if deleted.is_some()
                && task["progress"]["dependency"]["reason"]
                    .as_str()
                    .is_some_and(|v| v.contains("历史版本"))
            {
                deleted_reference_waits += 1;
                assert!(
                    deleted_reference_waits < 2,
                    "已发布的删除必须合法结清其旧引用，不能永久阻塞原父任务"
                );
            }
            if task["status"] == "succeeded" {
                let reclaim_stage_elapsed = reclaim_stage_started.elapsed();
                eprintln!(
                    "RECLAIM_STAGE_SECONDS={:.6}",
                    reclaim_stage_elapsed.as_secs_f64()
                );
                assert!(step > 0);
                let done = catalog
                    .reclaim_format_result(&parent, fixture.owner.round)
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(done.output(), source.volume_id.unwrap());
                if let Some((deletion, original_node)) = &deleted {
                    assert!(deleted_fault_checked, "删除损坏故障边界必须实际执行");
                    let current = catalog
                        .node_mutation_deletion(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            deletion.attempt().request_identity().0,
                            deletion.attempt().request_identity().1,
                        )
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(&current, deletion);
                    let db = foundationdb::Database::from_path(cluster).unwrap();
                    let tr = db.create_trx().unwrap();
                    let mut key = b"tape-rs/catalog/22/N/".to_vec();
                    key.extend_from_slice(control.route.route.namespace.as_bytes());
                    key.extend_from_slice(deletion.node().as_bytes());
                    assert_eq!(
                        tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                        original_node
                    );
                    assert!(
                        catalog
                            .file_head(control.route.route.namespace, deletion.node())
                            .await
                            .unwrap()
                            .is_none()
                    );
                    assert!(
                        catalog
                            .physical_copy(
                                control.route.route.namespace,
                                deletion.node(),
                                deletion.previous_copy().unwrap().0
                            )
                            .await
                            .unwrap()
                            .is_none()
                    );
                }

                if let Some((id, old, request)) = &old_history {
                    assert!(
                        catalog
                            .physical_copy(control.route.route.namespace, *id, *old)
                            .await
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        catalog
                            .active_upload_result(request, control.route)
                            .await
                            .unwrap(),
                        Some(crate::managed_catalog::PublishOutcome::AlreadyApplied(
                            request.expected_version.unwrap() + 1
                        ))
                    );
                }
                assert_eq!(
                    done.value()["migrated_logical_files"],
                    if native_links { 2 } else { 1 }
                );
                assert_eq!(done.value()["unassigned"], unassign);
                assert_eq!(
                    history_fault_seen, history_fault,
                    "long-history recovery must actually exercise the injected failure"
                );
                let source_after = catalog
                    .tape_assignment(&control.serial, &source.barcode)
                    .await
                    .unwrap();
                assert_eq!(source_after.is_none(), unassign);
                if unassign {
                    let changer = fixture
                        .owner
                        .devices
                        .iter()
                        .find(|d| d.kind == DeviceKind::Changer)
                        .unwrap();
                    let mut robot = MediumChanger::new(changer.dev.as_ref());
                    robot.load_address_map().unwrap();
                    let rows = robot.read_all_status().unwrap();
                    assert!(rows.iter().any(|r| r.full
                        && r.element_type == ElementType::Storage
                        && r.volume_tag.as_deref() == Some(source.barcode.as_str())));
                    assert!(!rows.iter().any(|r| r.full
                        && r.element_type == ElementType::DataTransfer
                        && r.volume_tag.as_deref() == Some(source.barcode.as_str())));
                }
                if native_links {
                    let full =
                        crate::managed_catalog::pool_catalog_path(pool.id, b"/links/empty/native")
                            .unwrap();
                    let Some(crate::managed_catalog::ResolvedPath::File {
                        copy,
                        attributes: Some(attributes),
                        ..
                    }) = catalog
                        .resolve_active_path_for_route(&control.serial, &full, control.route)
                        .await
                        .unwrap()
                    else {
                        panic!("native link")
                    };
                    assert_eq!(attributes.symlink(), Some("../../data"));
                    assert!(copy.media.as_ref().unwrap().native_symlink);
                    assert_eq!(
                        copy.media.as_ref().unwrap().physical_path,
                        "links/empty/native"
                    );
                    assert_eq!(copy.volume_id, target.volume_id.unwrap());
                    let inventory = inventory_inner(&fixture.owner.devices, true).unwrap();
                    let drive = inventory
                        .drives
                        .iter()
                        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &target.barcode))
                        .unwrap()
                        .dev;
                    let volume =
                        LtfsVolume::mount(fixture.owner.devices[drive].dev.as_ref()).unwrap();
                    assert_eq!(
                        volume
                            .index()
                            .find_file("links/empty/native")
                            .unwrap()
                            .symlink
                            .as_deref(),
                        Some("../../data")
                    );
                    let mut bytes = Vec::new();
                    volume
                        .read_file_to_writer("links/empty/native", &mut bytes)
                        .unwrap();
                    assert_eq!(bytes, b"layout-source");
                }
                if let Some(uid) = orphan {
                    let inventory = inventory_inner(&fixture.owner.devices, true).unwrap();
                    let drive = inventory
                        .drives
                        .iter()
                        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == &target.barcode))
                        .unwrap()
                        .dev;
                    let checked = fdb_loading::operation_guard(
                        &fixture.owner,
                        &fixture.permit,
                        fixture.owner.devices[drive].dev.as_ref(),
                        || {
                            futures_executor::block_on(catalog.require_active_library_readable(
                                &control.serial,
                                control.route,
                                fixture.owner.round,
                            ))
                            .map_err(|e| TapeError::Catalog(e.to_string()))
                        },
                    );
                    let volume = LtfsVolume::mount(&checked).unwrap();
                    let mut bytes = Vec::new();
                    volume
                        .read_file_to_writer(
                            &format!("_reclaim_{}/node-{uid}", original.operation()),
                            &mut bytes,
                        )
                        .unwrap();
                    assert_eq!(bytes, b"unmapped original bytes");
                    checked.verify_proof().unwrap();
                }
                let db = foundationdb::Database::from_path(cluster).unwrap();
                let tr = db.create_trx().unwrap();
                let mut prefix = b"tape-rs/catalog/22/V/".to_vec();
                prefix.extend_from_slice(source.volume_id.unwrap().as_bytes());
                let mut end = prefix.clone();
                while end.last() == Some(&255) {
                    end.pop();
                }
                *end.last_mut().unwrap() += 1;
                let range = foundationdb::RangeOption {
                    begin: foundationdb::KeySelector::first_greater_or_equal(prefix),
                    end: foundationdb::KeySelector::first_greater_or_equal(end),
                    limit: Some(1),
                    ..Default::default()
                };
                assert!(
                    tr.get_range(&range, 1, false).await.unwrap().is_empty(),
                    "旧卷的所有命名空间/代次 V 必须确实为空"
                );
                catalog
                    .require_active_library_readable(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                    )
                    .await
                    .unwrap();
                fdb_reclaim::cleanup(&fixture.owner, &fixture.permit, &control, true)
                    .await
                    .unwrap();
                fixture.permit.renew(
                    fixture.owner.round,
                    Phase::Serving,
                    Instant::now() + Duration::from_secs(60),
                );
                fixture.owner.fdb_reads.clear();
                fixture.owner.fdb_read_session = None;
                fixture.owner.write_session = None;
                let mut file = Box::pin(read_fdb(
                    &mut fixture.owner,
                    Some(&fixture.permit),
                    &control,
                    &status,
                    &format!("/{}/data", pool.id),
                    Some(&pool.id.to_string()),
                ))
                .await
                .unwrap();
                let mut body = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut body).unwrap();
                assert_eq!(body, b"layout-source");
                if raw_spool_case {
                    if case == NativeParentCase::RawBodyBudget {
                        assert!(
                            raw_budget_recovered && raw_budget_hook.is_none(),
                            "must reach and restore the exact raw UID budget fault"
                        );
                    }
                    let elapsed = reclaim_stage_elapsed;
                    eprintln!(
                        "C1 parent advance stage (excludes fixture setup) elapsed={elapsed:?} fault_injected={raw_budget_recovered}"
                    );
                    if case == NativeParentCase::RawBodySingleRead
                        && elapsed > Duration::from_secs(180)
                    {
                        eprintln!(
                            "performance warning: small-data reclaim advance stage exceeded the 180-second target: {elapsed:?}"
                        );
                    }
                    let count = raw_body_reads.load(std::sync::atomic::Ordering::SeqCst);
                    let physical =
                        raw_body_physical_reads.load(std::sync::atomic::Ordering::SeqCst);
                    assert!(physical >= count);
                    eprintln!(
                        "raw orphan source primary reads={count}, physical payload reads={physical}, audit gap reads={}; complete parent, target cold read and original coverage/FORMAT verified",
                        physical - count
                    );
                    assert_eq!(
                        count, 1,
                        "未映射raw正文应由首次核验spool直接交原writer，不能从源磁带读第二次"
                    );
                }
                return;
            }
        }
        panic!("原生 reclaim 父任务没有在有界步数内结清");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 双驱动布局固定、接管、机械授权及安全取消"]
    async fn reclaim_layout_fixes_two_drives_and_homes_across_takeover() {
        use crate::daemon::fdb_reclaim_mechanics::ReclaimMechanics;
        use crate::managed_catalog::{
            ManagedCatalog, ManagementRequest, ReclaimDriveBinding, ReclaimSpec,
        };
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture_from(
            catalog.clone(),
            None,
            Fixture::with_drives(2),
        ))
        .await;
        fixture.format(uuid::Uuid::new_v4());
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume
            .append_file("/data", &mut std::io::Cursor::new(b"layout-source"))
            .unwrap();
        volume.commit().unwrap();
        drop(volume);
        let request = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("LAYOUTL8", 128 << 20), 1)
            .unwrap();
        let assign = request(
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LAYOUTL8","volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":false}),
        );
        let assigned = execute(&mut fixture, &control, &status, &assign).await;
        assert_eq!(assigned["status"], "succeeded", "{assigned}");
        let source = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let target = catalog
            .tape_assignment(&control.serial, "LAYOUTL8")
            .await
            .unwrap()
            .unwrap();
        let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":false})).unwrap();
        let original = request(spec.to_json());
        catalog
            .admit_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                &original,
            )
            .await
            .unwrap();
        let parent = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        let changer = fixture
            .owner
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .dev
            .as_ref();
        let mut robot = MediumChanger::new(changer);
        let map = robot.load_address_map().unwrap().clone();
        let rows = robot.read_all_status().unwrap();
        let source_media = rows
            .iter()
            .find(|r| r.full && r.volume_tag.as_deref() == Some(source.barcode.as_str()))
            .unwrap();
        let target_media = rows
            .iter()
            .find(|r| r.full && r.volume_tag.as_deref() == Some(target.barcode.as_str()))
            .unwrap();
        let binding = |assignment: &crate::managed_catalog::TapeAssignment,
                       media: &ElementStatus,
                       avoid: Option<u16>| {
            let drive = if media.element_type == ElementType::DataTransfer {
                media
            } else {
                rows.iter()
                    .find(|r| {
                        r.element_type == ElementType::DataTransfer
                            && !r.full
                            && Some(r.address) != avoid
                            && (target_media.element_type != ElementType::DataTransfer
                                || r.address != target_media.address)
                    })
                    .unwrap()
            };
            ReclaimDriveBinding {
                barcode: assignment.barcode.clone(),
                volume: assignment.volume_id.unwrap(),
                revision: assignment.revision,
                drive_serial: drive.drive_id.clone().unwrap(),
                address: drive.address,
                home: if media.element_type == ElementType::Storage {
                    media.address
                } else {
                    media.source_address.unwrap()
                },
            }
        };
        let source_binding = binding(&source, source_media, None);
        let destination_binding = binding(&target, target_media, Some(source_binding.address));
        let source_index = fixture
            .owner
            .devices
            .iter()
            .position(|d| d.serial == source_binding.drive_serial)
            .unwrap();
        let actor = ReclaimMechanics {
            catalog: &catalog,
            changer,
            drive: fixture.owner.devices[source_index].dev.as_ref(),
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
        };
        let log = fixture.commands.lock().unwrap().clone();
        let layout = actor
            .prepare_layout(&parent, source_binding.clone(), destination_binding.clone())
            .await
            .unwrap();
        assert_eq!(layout.source, source_binding);
        assert_eq!(layout.destination, destination_binding);
        assert_eq!(layout.map, map);
        assert!(
            !fixture.commands.lock().unwrap()[log.len()..]
                .iter()
                .any(|(_, op)| matches!(op, 0x04 | 0x0a | 0x10 | 0x1b | 0x55 | 0x8d | 0xa5))
        );
        assert!(
            catalog
                .require_active_library_readable(
                    &control.serial,
                    control.route,
                    fixture.owner.round
                )
                .await
                .is_err()
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .reclaim_layout(&parent, fixture.owner.round)
                .await
                .unwrap(),
            Some(layout.clone())
        );
        let round = fixture.owner.round + 100;
        let permit = Arc::new(Permission::default());
        permit.renew(
            round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(60),
        );
        let mut adopted = takeover(
            &Provider {
                lib: fixture.lib.clone(),
                commands: fixture.commands.clone(),
            },
            &fixture.opts,
            round,
            Some(&permit),
            fixture.owner.files.clone(),
        )
        .unwrap();
        permit.renew(
            round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60),
        );
        catalog
            .claim_active_owner_epoch(&control.serial, round)
            .await
            .unwrap();
        let parents = catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, round, None, 16)
            .await
            .unwrap();
        let parent = &parents.items[0];
        catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap();
        let log = fixture.commands.lock().unwrap().clone();
        assert!(
            actor
                .execute(
                    parent,
                    &layout.plan(
                        &layout.source,
                        crate::managed_catalog::ReclaimMediaAction::Load
                    )
                )
                .await
                .is_err()
        );
        assert_eq!(
            *fixture.commands.lock().unwrap(),
            log,
            "旧 owner 在原布局接管后不发 CDB"
        );
        assert_eq!(
            catalog.reclaim_layout(parent, round).await.unwrap(),
            Some(layout.clone())
        );
        let actor = ReclaimMechanics {
            catalog: &catalog,
            changer: adopted
                .devices
                .iter()
                .find(|d| d.kind == DeviceKind::Changer)
                .unwrap()
                .dev
                .as_ref(),
            drive: adopted.devices[source_index].dev.as_ref(),
            route: control.route,
            key: adopted.key,
            permission: &permit,
        };
        if source_media.element_type == ElementType::Storage {
            let mut movement = layout.plan(
                &layout.source,
                crate::managed_catalog::ReclaimMediaAction::Move,
            );
            movement.source = layout.source.home;
            movement.destination = layout.source.address;
            actor.execute(parent, &movement).await.unwrap();
        }
        let load = layout.plan(
            &layout.source,
            crate::managed_catalog::ReclaimMediaAction::Load,
        );
        actor.execute(parent, &load).await.unwrap();
        let unload = layout.plan(
            &layout.source,
            crate::managed_catalog::ReclaimMediaAction::Unload,
        );
        actor.execute(parent, &unload).await.unwrap();
        actor.execute(parent, &load).await.unwrap();
        let cancelled = catalog
            .cancel_management(
                &control.serial,
                control.route,
                round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        catalog
            .require_active_library_readable(&control.serial, control.route, round)
            .await
            .unwrap();
        assert!(catalog.reclaim_layout(parent, round).await.is_err());
        // A fresh parent may claim the released layout. Only its sealed
        // original child can use the otherwise closed data lane.
        let original = request(spec.to_json());
        catalog
            .admit_management(&control.serial, control.route, round, &original)
            .await
            .unwrap();
        let parent = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        actor
            .prepare_layout(&parent, layout.source.clone(), layout.destination.clone())
            .await
            .unwrap();
        let reader = crate::daemon::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: adopted.devices[source_index].dev.as_ref(),
            serial: &control.serial,
            route: control.route,
            key: adopted.key,
            permission: &permit,
            storage: control.storage,
        };
        let cache = fixture.owner.files.extent_cache_dir();
        let spool = fixture.owner.files.spool_dir();
        let captured = reader.capture_reclaim_source(&parent, spool).await.unwrap();
        let inventory =
            crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).unwrap();
        loop {
            if catalog
                .seal_reclaim_source_inventory_page(&parent, round, &inventory)
                .await
                .unwrap()
                .complete()
            {
                break;
            }
        }
        let file = loop {
            let walk = catalog.step_reclaim_walk(&parent, round).await.unwrap();
            if let Some(file) = walk.current() {
                break file.clone();
            }
            assert!(!walk.complete());
        };
        let (read, evidence) = reader
            .read_reclaim_file_with_cache(&parent, &file, &inventory, &cache, spool)
            .await
            .unwrap();
        let upload = crate::managed_catalog::ActiveUploadSpec {
            pool_id: pool.id,
            path: file.path().as_bytes().to_vec(),
            operation_id: file.operation(),
            expected_version: Some(file.version()),
            size: file.size(),
            digest: crate::managed_catalog::ContentDigest::Sha256(evidence.sha256()),
        };
        let baseline = catalog
            .maintenance_status(control.route.route.namespace)
            .await
            .unwrap()
            .pending;
        let intent = catalog
            .prepare_reclaim_upload(&parent, round, &target.barcode, &upload, &evidence)
            .await
            .unwrap();
        let request = intent.request();
        assert_eq!(
            catalog
                .reclaim_upload_parent(&request, round)
                .await
                .unwrap()
                .unwrap()
                .intent(),
            parent.intent()
        );
        catalog
            .require_reclaim_upload_lane(&parent, round, &request)
            .await
            .unwrap();
        let log = fixture.commands.lock().unwrap().clone();
        assert_eq!(
            catalog
                .check_active_write_group(
                    &control.serial,
                    control.route,
                    round,
                    std::slice::from_ref(&intent)
                )
                .await
                .unwrap(),
            vec![false]
        );
        let mut foreign = intent.clone();
        foreign.operation_id = uuid::Uuid::new_v4();
        assert!(
            catalog
                .check_active_write_group(
                    &control.serial,
                    control.route,
                    round,
                    &[intent.clone(), foreign]
                )
                .await
                .is_err()
        );
        let mut changed = request.clone();
        changed.expected_version = Some(file.version() + 1);
        assert!(
            catalog
                .reclaim_upload_parent(&changed, round)
                .await
                .is_err()
        );
        assert_eq!(
            *fixture.commands.lock().unwrap(),
            log,
            "恢复原写入上下文及拒绝混入请求均不发 CDB"
        );
        let destination_index = adopted
            .devices
            .iter()
            .position(|d| d.serial == layout.destination.drive_serial)
            .unwrap();
        let actor = ReclaimMechanics {
            catalog: &catalog,
            changer: adopted
                .devices
                .iter()
                .find(|d| d.kind == DeviceKind::Changer)
                .unwrap()
                .dev
                .as_ref(),
            drive: adopted.devices[destination_index].dev.as_ref(),
            route: control.route,
            key: adopted.key,
            permission: &permit,
        };
        if target_media.element_type == ElementType::Storage {
            let mut movement = layout.plan(
                &layout.destination,
                crate::managed_catalog::ReclaimMediaAction::Move,
            );
            movement.source = layout.destination.home;
            movement.destination = layout.destination.address;
            actor.execute(&parent, &movement).await.unwrap();
        }
        actor
            .execute(
                &parent,
                &layout.plan(
                    &layout.destination,
                    crate::managed_catalog::ReclaimMediaAction::Load,
                ),
            )
            .await
            .unwrap();
        permit.renew(
            round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60),
        );
        let copied = Box::pin(fdb_writing::write(
            &mut adopted,
            &permit,
            &control,
            &status,
            request.clone(),
            read.file,
        ))
        .await
        .unwrap();
        let reader = crate::daemon::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: adopted.devices[destination_index].dev.as_ref(),
            serial: &control.serial,
            route: control.route,
            key: adopted.key,
            permission: &permit,
            storage: control.storage,
        };
        let (mut verified, _) = reader
            .read_reclaim_metadata_with_cache(&parent, &request, &inventory, &cache, spool)
            .await
            .unwrap();
        let mut actual = Vec::new();
        std::io::Read::read_to_end(&mut verified.file, &mut actual).unwrap();
        assert_eq!(actual, b"layout-source");
        assert_eq!(copied, verified.metadata.head.version);
        catalog
            .verify_reclaim_file_metadata(
                &parent,
                round,
                &file,
                &request,
                &verified.metadata,
                verified.verified_head,
            )
            .await
            .unwrap();
        catalog
            .retire_reclaim_file_source(&parent, round, &file, &request)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .maintenance_status(control.route.route.namespace)
                .await
                .unwrap()
                .pending,
            baseline
        );
        assert!(
            catalog
                .cancel_management(
                    &control.serial,
                    control.route,
                    round,
                    original.epoch(),
                    original.operation()
                )
                .await
                .is_err(),
            "取消子请求不能删除已进入复制阶段的父检查点"
        );
    }

    async fn verify_source_other_handle_change(
        fixture: &mut Fixture,
        control: &FdbOwnerControl,
        status: &SharedStatus,
        parent: &crate::managed_catalog::ReclaimReservation,
        barcode: &str,
        trace: &Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
        quirk_already_fixed: bool,
    ) {
        let current = inventory(&fixture.owner.devices).unwrap();
        let source = current
            .drives
            .iter()
            .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == barcode))
            .unwrap()
            .dev;
        assert!(
            fixture.owner.fdb_reclaim_source_session.is_some(),
            "原封存第一页保留完整源捕获"
        );
        let original = control
            .catalog
            .reclaim_source_head(parent, fixture.owner.round)
            .await
            .unwrap();
        let reader = fixture.owner.devices[source].dev.as_ref();
        let epoch = reader.session_epoch();
        let before_vcr = crate::ltfs::mam::Mam::with_partition(reader, 0)
            .read_vcr()
            .unwrap();
        assert!(
            before_vcr
                .as_ref()
                .is_some_and(|v| crate::ltfs::mam::vcr_is_valid(v))
        );
        if !quirk_already_fixed {
            // Preserve the original actual-RED fixture's timing.
            fixture.lib.set_quirks(crate::scsi::sim::SimQuirks {
                vcr_never_changes: true,
                ..Default::default()
            });
        }
        let before_write_token = reader.medium_change_token();
        if quirk_already_fixed {
            let saved_token = fixture
                .owner
                .fdb_reclaim_source_session
                .as_ref()
                .unwrap()
                .test_change_token();
            assert!(saved_token.is_some());
            assert_eq!(
                before_write_token, saved_token,
                "quirk配置完成且写入前仍是已保存的共享变化证明"
            );
        }
        // Same I_T nexus and PR holder, distinct handle-local epoch. Deliberate
        // Sim-only external mutation; never publish it into the original FDB head.
        let alternate = fixture.lib.drive_as(source, 1);
        let mut volume = LtfsVolume::mount(&alternate).unwrap();
        volume.set_reservation_guard(Some(fixture.owner.key));
        volume
            .append_bytes("/other-handle-change", b"new source metadata and body")
            .unwrap();
        volume.commit().unwrap();
        assert!(volume.index().generation > original.generation);
        drop(volume);
        if quirk_already_fixed {
            assert_ne!(
                reader.medium_change_token(),
                before_write_token,
                "只有另handle写入后共享变化证明才失效"
            );
        }
        assert_eq!(
            reader.session_epoch(),
            epoch,
            "别的句柄写带不改变本句柄epoch"
        );
        assert_eq!(
            crate::ltfs::mam::Mam::with_partition(reader, 0)
                .read_vcr()
                .unwrap(),
            before_vcr,
            "已建模固定VCR设备"
        );
        assert_eq!(
            control
                .catalog
                .reclaim_source_head(parent, fixture.owner.round)
                .await
                .unwrap(),
            original,
            "FDB原卷头未被外部写更新"
        );
        let before = trace.lock().unwrap().len();
        renew_native_parent_permit(fixture);
        let result = Box::pin(fdb_reclaim::advance(
            &mut fixture.owner,
            &fixture.permit,
            control,
            status,
        ))
        .await;
        let target_writes = trace.lock().unwrap()[before..]
            .iter()
            .filter(|(drive, cdb)| {
                *drive != source && matches!(cdb.first(), Some(0x0a | 0x10 | 0x8d))
            })
            .count();
        eprintln!(
            "same-initiator alternate handle: original_epoch={epoch:?}, target_writes={target_writes}, result={result:?}"
        );
        assert!(
            result.is_err(),
            "源物理头已变而FDB头不变时必须拒绝沿旧捕获继续: {result:?}"
        );
        assert_eq!(target_writes, 0, "未核实已改变源不能新增目标写");
        assert!(fixture.owner.fdb_reclaim_source_session.is_none());
        assert!(
            control
                .catalog
                .reclaim_format_plan(parent, fixture.owner.round)
                .await
                .unwrap()
                .is_none()
        );
    }

    async fn verify_source_session_invalidation(
        fixture: &mut Fixture,
        control: &FdbOwnerControl,
        parent: &crate::managed_catalog::ReclaimReservation,
        barcode: &str,
        trace: &Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
        no_epoch: &std::sync::atomic::AtomicBool,
    ) {
        let current = inventory(&fixture.owner.devices).unwrap();
        let source = current
            .drives
            .iter()
            .find(|drive| {
                drive
                    .loaded
                    .as_ref()
                    .is_some_and(|(loaded, _)| loaded == barcode)
            })
            .unwrap()
            .dev;
        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &control.catalog,
            device: fixture.owner.devices[source].dev.as_ref(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let saved = fixture
            .owner
            .fdb_reclaim_source_session
            .take()
            .expect("原封存第一页保留完整源捕获");
        let identity = saved.captured().identity().unwrap();
        let oldest_reads = |start: usize| {
            trace.lock().unwrap()[start..]
                .iter()
                .filter(|(drive, cdb)| {
                    *drive == source
                        && cdb.first() == Some(&0x92)
                        && cdb[3] == 1
                        && u64::from_be_bytes(cdb[4..12].try_into().unwrap()) == 5
                })
                .count()
        };
        let before = trace.lock().unwrap().len();
        let same = reader
            .capture_reclaim_source_session(parent, fixture.owner.files.spool_dir(), Some(saved))
            .await
            .unwrap();
        assert_eq!(oldest_reads(before), 0, "同epoch复用不重扫历史");
        assert_eq!(same.captured().identity().unwrap(), identity);

        // A normal, successful LOCATE changes the transport epoch even if it
        // neither writes nor changes the source head or VCR.
        TapeDrive::new(reader.device).locate(1, 5, true).unwrap();
        let before = trace.lock().unwrap().len();
        let recaptured = reader
            .capture_reclaim_source_session(parent, fixture.owner.files.spool_dir(), Some(same))
            .await
            .unwrap();
        assert!(oldest_reads(before) > 0, "变更epoch必须重新完整捕获");
        assert_eq!(recaptured.captured().identity().unwrap(), identity);

        no_epoch.store(true, std::sync::atomic::Ordering::SeqCst);
        let before = trace.lock().unwrap().len();
        let unsupported = reader
            .capture_reclaim_source_session(
                parent,
                fixture.owner.files.spool_dir(),
                Some(recaptured),
            )
            .await
            .unwrap();
        assert!(oldest_reads(before) > 0, "无epoch仍走原完整捕获");
        assert!(
            reader
                .finish_reclaim_source_session(parent, unsupported)
                .await
                .unwrap()
                .is_none(),
            "无epoch不能保存可复用证明"
        );
        no_epoch.store(false, std::sync::atomic::Ordering::SeqCst);
        let saved = reader
            .capture_reclaim_source_session(parent, fixture.owner.files.spool_dir(), None)
            .await
            .unwrap();
        let saved = reader
            .finish_reclaim_source_session(parent, saved)
            .await
            .unwrap()
            .unwrap();
        let parent_before = control
            .catalog
            .reserved_reclaim_status(parent, fixture.owner.round)
            .await
            .unwrap();
        fixture.lib.reset_drive(source);
        let before = trace.lock().unwrap().len();
        assert!(
            reader
                .capture_reclaim_source_session(
                    parent,
                    fixture.owner.files.spool_dir(),
                    Some(saved),
                )
                .await
                .is_err()
        );
        assert!(
            trace.lock().unwrap()[before..]
                .iter()
                .all(|(_, cdb)| cdb.first() == Some(&0x5e)),
            "复位/PR丢失后只能核对预留，不能沿旧捕获定位、读取或写带"
        );
        let before = trace.lock().unwrap().len();
        fixture.permit.close(fixture.owner.round);
        assert!(
            reader
                .capture_reclaim_source_session(parent, fixture.owner.files.spool_dir(), None)
                .await
                .is_err()
        );
        assert_eq!(trace.lock().unwrap().len(), before, "失权后不再发CDB");
        assert_eq!(
            control
                .catalog
                .reserved_reclaim_status(parent, fixture.owner.round)
                .await
                .unwrap(),
            parent_before
        );
        assert!(fixture.owner.fdb_reclaim_source_session.is_none());
    }

    struct FreshCaptureMutation {
        inner: Box<dyn TapeTransport + Send>,
        lib: SimLibrary,
        drive: usize,
        key: ReservationKey,
        post_vcr: bool,
        token_calls: std::sync::atomic::AtomicUsize,
        armed: std::sync::atomic::AtomicBool,
        rejected: std::sync::atomic::AtomicU64,
        fired: Arc<std::sync::atomic::AtomicBool>,
    }
    impl FreshCaptureMutation {
        fn mutate(&self) {
            assert!(
                !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst),
                "只注入一次外部修改"
            );
            let alternate = self.lib.drive_as(self.drive, 1);
            let mut volume = LtfsVolume::mount(&alternate).unwrap();
            volume.set_reservation_guard(Some(self.key));
            volume
                .append_bytes(
                    "/capture-boundary-mutation",
                    b"same-initiator external source change",
                )
                .unwrap();
            volume.commit().unwrap();
        }
    }
    impl TapeTransport for FreshCaptureMutation {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_write(cdb, data, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            if self.post_vcr
                && self.armed.load(std::sync::atomic::Ordering::SeqCst)
                && cdb.first() == Some(&0x8c)
                && cdb.get(8..10) == Some(&[0, 9])
                && !self.fired.load(std::sync::atomic::Ordering::SeqCst)
            {
                self.mutate();
                self.rejected
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut raw = [0; 18];
                raw[0] = 0x70;
                raw[2] = 5;
                raw[7] = 10;
                raw[12] = 0x24;
                return crate::scsi::device::finish_command(
                    2,
                    crate::scsi::sense::SenseInfo::from_bytes(&raw),
                    0,
                    0,
                    self.identity(),
                );
            }
            self.inner.execute_read(cdb, data, timeout)
        }
        fn medium_change_token(&self) -> Option<[u8; 32]> {
            if self
                .token_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 1
            {
                if self.post_vcr {
                    self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    self.mutate();
                }
            }
            self.inner.medium_change_token()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch().and_then(|e| {
                e.checked_add(self.rejected.load(std::sync::atomic::Ordering::SeqCst))
            })
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }

    struct SourceReadFailure {
        inner: Box<dyn TapeTransport + Send>,
        fail: std::sync::atomic::AtomicBool,
        failures: std::sync::atomic::AtomicU64,
    }
    impl TapeTransport for SourceReadFailure {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_write(cdb, data, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            if cdb.first() == Some(&0x34)
                && self.fail.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.failures
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(TapeError::ScsiHost(3));
            }
            self.inner.execute_read(cdb, data, timeout)
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch().and_then(|e| {
                e.checked_add(self.failures.load(std::sync::atomic::Ordering::SeqCst))
            })
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }

    struct UnsupportedSourceVcr {
        inner: Box<dyn TapeTransport + Send>,
        rejected: std::sync::atomic::AtomicU64,
    }
    impl TapeTransport for UnsupportedSourceVcr {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            buffer: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            if cdb.first() == Some(&0x8c) && cdb.get(8..10) == Some(&[0, 9]) {
                self.rejected
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut sense = [0; 18];
                sense[0] = 0x70;
                sense[2] = 5;
                sense[7] = 10;
                sense[12] = 0x24;
                return crate::scsi::device::finish_command(
                    2,
                    crate::scsi::sense::SenseInfo::from_bytes(&sense),
                    0,
                    0,
                    self.identity(),
                );
            }
            self.inner.execute_read(cdb, buffer, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            buffer: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_write(cdb, buffer, timeout)
        }
        fn medium_change_token(&self) -> Option<[u8; 32]> {
            self.inner.medium_change_token()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch().and_then(|epoch| {
                epoch.checked_add(self.rejected.load(std::sync::atomic::Ordering::SeqCst))
            })
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }

    struct ParentSourceTrace {
        inner: Box<dyn TapeTransport + Send>,
        drive: usize,
        commands: Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
        no_epoch: Arc<std::sync::atomic::AtomicBool>,
        trusted_change_evidence: bool,
    }
    impl ParentSourceTrace {
        fn record(&self, cdb: &[u8]) {
            self.commands
                .lock()
                .unwrap()
                .push((self.drive, cdb.to_vec()));
        }
    }
    impl TapeTransport for ParentSourceTrace {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            buffer: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            if fdb_reclaim::observe_group_read_for_test(&self.commands, self.drive, cdb) {
                return Err(TapeError::ScsiHost(0x21));
            }
            self.inner.execute_read(cdb, buffer, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            buffer: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            self.inner.execute_write(cdb, buffer, timeout)
        }
        fn medium_change_token(&self) -> Option<[u8; 32]> {
            self.trusted_change_evidence
                .then(|| self.inner.medium_change_token())
                .flatten()
        }
        fn session_epoch(&self) -> Option<u64> {
            if self.no_epoch.load(std::sync::atomic::Ordering::SeqCst) {
                None
            } else {
                self.inner.session_epoch()
            }
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }

    struct Recorded {
        inner: Box<dyn TapeTransport + Send>,
        kind: DeviceKind,
        commands: Commands,
    }
    impl TapeTransport for Recorded {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.commands.lock().unwrap().push((self.kind, cdb[0]));
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            buffer: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.commands.lock().unwrap().push((self.kind, cdb[0]));
            self.inner.execute_read(cdb, buffer, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            buffer: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.commands.lock().unwrap().push((self.kind, cdb[0]));
            self.inner.execute_write(cdb, buffer, timeout)
        }
        fn medium_change_token(&self) -> Option<[u8; 32]> {
            self.inner.medium_change_token()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch()
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }
    struct Provider {
        lib: SimLibrary,
        commands: Commands,
    }
    impl DeviceProvider for Provider {
        fn open_all(&self) -> Result<Vec<ManagedDevice>> {
            let mut devices = (0..self.lib.drive_count())
                .map(|index| ManagedDevice {
                    name: if index == 0 {
                        "drive".into()
                    } else {
                        format!("drive-{index}")
                    },
                    serial: format!("SIMDRV{index:04}"),
                    kind: DeviceKind::Drive,
                    dev: Box::new(Recorded {
                        inner: Box::new(self.lib.drive_as(index, 1)),
                        kind: DeviceKind::Drive,
                        commands: self.commands.clone(),
                    }),
                })
                .collect::<Vec<_>>();
            devices.push(ManagedDevice {
                name: "changer".into(),
                serial: "SIMLIB0000000001".into(),
                kind: DeviceKind::Changer,
                dev: Box::new(Recorded {
                    inner: Box::new(self.lib.changer_as(1)),
                    kind: DeviceKind::Changer,
                    commands: self.commands.clone(),
                }),
            });
            Ok(devices)
        }
    }
    struct Fixture {
        root: std::path::PathBuf,
        lib: SimLibrary,
        owner: Active,
        permit: Arc<Permission>,
        opts: ExecOptions,
        commands: Commands,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_drives(1)
        }
        fn with_drives(drives: usize) -> Self {
            let root =
                std::env::temp_dir().join(format!("native-lifecycle-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let lib = SimLibrary::new(drives, 3, 1);
            lib.enable_changer_pr();
            lib.insert_cartridge(SimCartridge::blank("LIFE00L8", 128 << 20), 0)
                .unwrap();
            // Exercise MOVE MEDIUM so READ ELEMENT STATUS carries the real
            // source address; a direct fixture load has no trusted home.
            let changer_device = lib.changer_as(1);
            let mut changer = MediumChanger::new(&changer_device);
            let map = changer.load_address_map().unwrap().clone();
            changer
                .move_medium(map.storage_start, map.dt_start)
                .unwrap();
            let permit = Arc::new(Permission::default());
            permit.renew(7, Phase::Fencing, Instant::now() + Duration::from_secs(60));
            let opts = ExecOptions {
                node_id: 1,
                interval: Duration::from_millis(10),
                demo_write: false,
                salvage: false,
                block_size: 64 << 10,
                read_idle: Duration::from_secs(60),
            };
            let files = FileService::new(root.join("spool")).unwrap();
            let commands = Commands::default();
            let owner = takeover(
                &Provider {
                    lib: lib.clone(),
                    commands: commands.clone(),
                },
                &opts,
                7,
                Some(&permit),
                files,
            )
            .unwrap();
            permit.renew(7, Phase::Serving, Instant::now() + Duration::from_secs(60));
            TapeDrive::new(owner.devices[0].dev.as_ref())
                .load()
                .unwrap();
            Self {
                root,
                lib,
                owner,
                permit,
                opts,
                commands,
            }
        }
        fn format(&self, uuid: uuid::Uuid) {
            mkltfs(
                self.device(),
                &MkltfsOptions {
                    volume_id: "LIFE00".into(),
                    volume_uuid: Some(uuid),
                    block_size: self.opts.block_size,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        fn device(&self) -> &dyn TapeTransport {
            self.owner.devices[0].dev.as_ref()
        }
        fn budget(&self) -> super::super::super::fdb_read::ReadStorageBudget {
            super::super::super::fdb_read::ReadStorageBudget {
                index_bytes: 64 << 20,
                cache_bytes: 64 << 20,
                download_bytes: 64 << 20,
                headroom_bytes: 0,
            }
        }
        fn formats(&self) -> usize {
            self.drive_log().iter().filter(|c| **c == 0x04).count()
        }
        fn writes(&self) -> usize {
            self.drive_log().iter().filter(|c| **c == 0x0a).count()
        }
    }
    impl Fixture {
        fn drive_log(&self) -> Vec<u8> {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter(|(kind, _)| *kind == DeviceKind::Drive)
                .map(|(_, opcode)| *opcode)
                .collect()
        }
        fn changer_log(&self) -> Vec<u8> {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter(|(kind, _)| *kind == DeviceKind::Changer)
                .map(|(_, opcode)| *opcode)
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn lifecycle_rescan_observes_inventory_without_moving_or_reading_any_index() {
        let f = Fixture::new();
        let before = f.drive_log().len();
        let moves = f.changer_log().iter().filter(|op| **op == 0xa5).count();
        let snapshot = management::observe(&f.owner, &f.opts).unwrap();
        assert_eq!(snapshot["library_serial"], "SIMLIB0000000001");
        assert_eq!(snapshot["volumes"][0]["barcode"], "LIFE00L8");
        assert!(
            !f.drive_log()[before..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x08 | 0x0a | 0x10 | 0x1b | 0x8d))
        );
        assert_eq!(
            f.changer_log().iter().filter(|op| **op == 0xa5).count(),
            moves
        );
    }

    #[test]
    fn unassigned_drive_move_refuses_any_current_managed_view() {
        let mut f = Fixture::new();
        unassigned_drive_idle(&f.owner, 0).unwrap();
        f.owner.drive = Some(0);
        assert!(unassigned_drive_idle(&f.owner, 0).is_err());
        f.owner.drive = None;
        f.owner.read = Some((0, "LIFE00L8".into(), Instant::now(), None));
        assert!(unassigned_drive_idle(&f.owner, 0).is_err());
        f.owner.read = None;
        f.owner.fdb_reads.insert(
            0,
            fdb_loading::Mount {
                barcode: "LIFE00L8".into(),
                volume: uuid::Uuid::new_v4(),
                pool: uuid::Uuid::new_v4(),
                last_used: Instant::now(),
            },
        );
        assert!(unassigned_drive_idle(&f.owner, 0).is_err());
    }

    #[test]
    fn lifecycle_validate_is_read_only_and_rejects_a_different_published_head() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        let pool = uuid::Uuid::new_v4();
        f.format(volume);
        let before = f.drive_log().len();
        let (summary, _) = validate_index(
            f.device(),
            f.owner.files.spool_dir(),
            f.budget(),
            volume,
            Some(pool),
            None,
        )
        .unwrap();
        assert_eq!(summary["scope"], "label_and_index");
        assert_eq!(summary["payload_verified"], false);
        assert!(
            !f.drive_log()[before..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x8d))
        );
        let expected = crate::managed_catalog::ActiveIndexIdentity {
            volume_uuid: volume,
            generation: 9,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 5,
            },
            sha256: [1; 32],
            incremental: false,
        };
        assert!(
            validate_index(
                f.device(),
                f.owner.files.spool_dir(),
                f.budget(),
                volume,
                Some(pool),
                Some(expected)
            )
            .is_err()
        );
    }

    fn home_fixture() -> (ElementAddressMap, Vec<ElementStatus>) {
        let map = ElementAddressMap {
            transport_start: 1,
            transport_count: 1,
            storage_start: 100,
            storage_count: 3,
            ie_start: 200,
            ie_count: 1,
            dt_start: 300,
            dt_count: 1,
        };
        let rows = (100..103)
            .map(|address| ElementStatus {
                address,
                element_type: ElementType::Storage,
                full: false,
                accessible: Some(true),
                exception: false,
                asc: 0,
                ascq: 0,
                disabled: false,
                medium_type: None,
                volume_tag: None,
                source_address: None,
                drive_id: None,
            })
            .chain(std::iter::once(ElementStatus {
                address: 200,
                element_type: ElementType::ImportExport,
                full: true,
                accessible: Some(true),
                exception: false,
                asc: 0,
                ascq: 0,
                disabled: false,
                medium_type: None,
                volume_tag: Some("HOME00L8".into()),
                source_address: None,
                drive_id: None,
            }))
            .collect();
        (map, rows)
    }

    #[test]
    fn lifecycle_home_rejects_occupied_known_home_instead_of_choosing_another_slot() {
        let (map, mut rows) = home_fixture();
        rows[0].full = true;
        rows[0].volume_tag = Some("OTHER0L8".into());
        assert!(choose_home(&map, &rows, &rows[3], Some(100), true).is_err());
    }

    #[test]
    fn lifecycle_home_rejects_conflicting_smc_and_durable_home() {
        let (map, mut rows) = home_fixture();
        rows[3].source_address = Some(100);
        assert!(choose_home(&map, &rows, &rows[3], Some(101), false).is_err());
    }

    #[test]
    fn lifecycle_home_rejects_missing_or_unmapped_known_home_without_fallback() {
        let (map, mut rows) = home_fixture();
        assert!(choose_home(&map, &rows, &rows[3], Some(999), true).is_err());
        rows[3].source_address = Some(999);
        assert!(choose_home(&map, &rows, &rows[3], Some(100), true).is_err());
        rows[3].source_address = None;
        let item = rows.pop().unwrap();
        rows.remove(0);
        assert!(choose_home(&map, &rows, &item, Some(100), true).is_err());
    }

    #[test]
    fn lifecycle_home_chooses_a_new_slot_for_unknown_unassigned_ie_or_drive() {
        let (map, mut rows) = home_fixture();
        assert_eq!(choose_home(&map, &rows, &rows[3], None, true).unwrap(), 100);
        assert!(choose_home(&map, &rows, &rows[3], None, false).is_err());
        rows[3].element_type = ElementType::DataTransfer;
        assert_eq!(choose_home(&map, &rows, &rows[3], None, true).unwrap(), 100);
        assert!(choose_home(&map, &rows, &rows[3], None, false).is_err());
        rows[3].element_type = ElementType::ImportExport;
        rows[3].source_address = Some(101);
        assert_eq!(
            choose_home(&map, &rows, &rows[3], Some(101), false).unwrap(),
            101
        );
        rows[0].full = true;
        rows[0].volume_tag = Some("HOME00L8".into());
        assert_eq!(
            choose_home(&map, &rows, &rows[0], Some(101), false).unwrap(),
            100
        );
    }

    #[test]
    fn lifecycle_move_uses_the_fixed_actual_plan_and_never_blindly_replays_it() {
        let f = Fixture::new();
        f.lib
            .insert_cartridge(SimCartridge::blank("MOVE00L8", 64 << 20), 1)
            .unwrap();
        let (_, rows) = elements(&f.owner).unwrap();
        let source = cartridge(&rows, "MOVE00L8").unwrap().address;
        let destination = rows
            .iter()
            .find(|e| e.element_type == ElementType::ImportExport && !e.full)
            .unwrap()
            .address;
        let plan = Plan {
            version: 1,
            barcode: Some("MOVE00L8".into()),
            source: Some(source),
            destination: Some(destination),
            home: Some(source),
            drive_serial: None,
            volume: None,
            close_may_write: false,
        };
        let count = f.changer_log().iter().filter(|op| **op == 0xa5).count();
        assert!(checked_move(&f.owner, &plan, MoveIntent::ObserveOnly).is_err());
        assert_eq!(
            f.changer_log().iter().filter(|op| **op == 0xa5).count(),
            count
        );
        checked_move(&f.owner, &plan, MoveIntent::NewlyIssued).unwrap();
        assert!(at_destination(&elements(&f.owner).unwrap().1, &plan));
        assert_eq!(
            f.changer_log().iter().filter(|op| **op == 0xa5).count(),
            count + 1
        );
        checked_move(&f.owner, &plan, MoveIntent::ObserveOnly).unwrap();
        assert_eq!(
            f.changer_log().iter().filter(|op| **op == 0xa5).count(),
            count + 1
        );
        assert_eq!(f.formats(), 0);
    }

    #[test]
    fn lifecycle_move_refuses_an_occupied_fixed_destination_without_another_move() {
        let f = Fixture::new();
        f.lib
            .insert_cartridge(SimCartridge::blank("MOVE00L8", 64 << 20), 1)
            .unwrap();
        f.lib
            .insert_cartridge(SimCartridge::blank("OTHER0L8", 64 << 20), 2)
            .unwrap();
        let (_, rows) = elements(&f.owner).unwrap();
        let source = cartridge(&rows, "MOVE00L8").unwrap().address;
        let destination = rows
            .iter()
            .find(|e| e.element_type == ElementType::ImportExport && !e.full)
            .unwrap()
            .address;
        move_medium(
            &f.owner.devices,
            cartridge(&rows, "OTHER0L8").unwrap().address,
            destination,
        )
        .unwrap();
        let plan = Plan {
            version: 1,
            barcode: Some("MOVE00L8".into()),
            source: Some(source),
            destination: Some(destination),
            home: Some(source),
            drive_serial: None,
            volume: None,
            close_may_write: false,
        };
        let count = f.changer_log().iter().filter(|op| **op == 0xa5).count();
        assert!(checked_move(&f.owner, &plan, MoveIntent::NewlyIssued).is_err());
        assert_eq!(
            f.changer_log().iter().filter(|op| **op == 0xa5).count(),
            count
        );
        assert_eq!(
            cartridge(&elements(&f.owner).unwrap().1, "MOVE00L8")
                .unwrap()
                .address,
            source
        );
    }

    async fn native_fixture(
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
    ) -> (
        Fixture,
        FdbOwnerControl,
        crate::managed_catalog::PoolRecord,
        SharedStatus,
    ) {
        native_fixture_for_serial(catalog, None).await
    }

    #[derive(Clone, Copy)]
    enum MechanicalBoundary {
        Load,
        Unload,
        Move,
    }
    struct IntakeLoadBoundary {
        trigger: MechanicalBoundary,
        inner: Box<dyn TapeTransport + Send>,
        loads: Arc<std::sync::atomic::AtomicUsize>,
        changed: Arc<std::sync::atomic::AtomicBool>,
        later: Arc<std::sync::Mutex<Vec<u8>>>,
        after_load: Arc<dyn Fn() -> Result<()> + Send + Sync>,
        fail_before_load: bool,
    }
    impl IntakeLoadBoundary {
        fn record(&self, cdb: &[u8]) {
            if self.changed.load(std::sync::atomic::Ordering::SeqCst) && cdb[0] != 0x5e {
                self.later.lock().unwrap().push(cdb[0]);
            }
        }
    }
    impl TapeTransport for IntakeLoadBoundary {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch()
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            let triggered = match self.trigger {
                MechanicalBoundary::Load => cdb[0] == 0x1b && cdb[4] & 1 != 0,
                MechanicalBoundary::Unload => cdb[0] == 0x1b && cdb[4] & 1 == 0,
                MechanicalBoundary::Move => cdb[0] == 0xa5,
            };
            if triggered {
                let first = self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                if first && self.fail_before_load {
                    self.changed
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return (self.after_load)().and_then(|()| Err(TapeError::ScsiHost(3)));
                }
                let result = self.inner.execute_no_data(cdb, timeout)?;
                if first {
                    self.changed
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    (self.after_load)()?;
                }
                return Ok(result);
            }
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            self.inner.execute_read(cdb, data, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.record(cdb);
            self.inner.execute_write(cdb, data, timeout)
        }
    }

    async fn begin_loading_import(
        control: &FdbOwnerControl,
        pool: uuid::Uuid,
        round: u64,
    ) -> crate::managed_catalog::NativeMediaImport {
        let request = crate::managed_catalog::ManagementRequest::parse(json!({
            "version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),
            "epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-import","pool":pool.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}
        })).unwrap();
        control
            .catalog
            .admit_management(&control.serial, control.route, round, &request)
            .await
            .unwrap();
        control
            .catalog
            .start_native_media(
                &control.serial,
                control.route,
                round,
                request.epoch(),
                request.operation(),
            )
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 装带中取消原任务，无实际设备"]
    async fn intake_load_cancellation_stops_before_the_next_device_command() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = native_fixture(catalog.clone()).await;
        TapeDrive::new(fixture.device()).unload().unwrap();
        let intake = begin_loading_import(&control, pool.id, fixture.owner.round).await;
        let cancelled = intake.clone();
        let round = fixture.owner.round;
        let loads = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(AtomicBool::new(false));
        let later = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = std::mem::replace(
            &mut fixture.owner.devices[0].dev,
            Box::new(fixture.lib.drive_as(0, 1)),
        );
        fixture.owner.devices[0].dev = Box::new(IntakeLoadBoundary {
            trigger: MechanicalBoundary::Load,
            inner,
            loads: loads.clone(),
            changed,
            later: later.clone(),
            fail_before_load: false,
            after_load: Arc::new(move || {
                futures_executor::block_on(catalog.abort_native_media(&cancelled, round, None))
                    .map_err(|e| TapeError::Catalog(e.to_string()))
            }),
        });
        let result = fdb_loading::select_intake(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &intake,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let later = later.lock().unwrap().clone();
        assert!(
            later.is_empty(),
            "原任务失效后仍向设备发送了 CDB: {later:?}"
        );
        assert!(matches!(
            control
                .catalog
                .rollback_native_media_page(&intake, round)
                .await
                .unwrap_err()
                .domain(),
            crate::managed_catalog::CatalogError::Incomplete
        ));
        assert!(
            control
                .catalog
                .require_active_library_readable(&control.serial, control.route, round)
                .await
                .is_err()
        );
        fdb_loading::settle_cancelled(&fixture.owner, &fixture.permit, &control, &intake)
            .await
            .unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1, "取消结清不能重发 LOAD");
        assert!(
            control
                .catalog
                .rollback_native_media_page(&intake, round)
                .await
                .unwrap()
        );
        control
            .catalog
            .require_active_library_readable(&control.serial, control.route, round)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim LOAD 回执未知及接管，不访问实际设备"]
    async fn intake_unknown_load_is_observed_without_reissue_after_takeover() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        for fail_before_load in [false, true] {
            let (mut fixture, control, pool, status) = native_fixture(catalog.clone()).await;
            TapeDrive::new(fixture.device()).unload().unwrap();
            let intake = begin_loading_import(&control, pool.id, fixture.owner.round).await;
            let loads = Arc::new(AtomicUsize::new(0));
            let changed = Arc::new(AtomicBool::new(false));
            let later = Arc::new(std::sync::Mutex::new(Vec::new()));
            let inner = std::mem::replace(
                &mut fixture.owner.devices[0].dev,
                Box::new(fixture.lib.drive_as(0, 1)),
            );
            fixture.owner.devices[0].dev = Box::new(IntakeLoadBoundary {
                trigger: MechanicalBoundary::Load,
                inner,
                loads: loads.clone(),
                changed: changed.clone(),
                later: later.clone(),
                fail_before_load,
                after_load: Arc::new(|| Err(TapeError::ScsiHost(3))),
            });
            assert!(matches!(
                fdb_loading::select_intake(
                    &mut fixture.owner,
                    &fixture.permit,
                    &control,
                    &status,
                    &intake
                )
                .await,
                Err(ReadError::Lost(_))
            ));
            let checkpoint = catalog
                .native_media_load_progress(&intake, fixture.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                checkpoint.phase,
                crate::managed_catalog::NativeMediaLoadPhase::LoadIssued
            );
            assert!(
                !catalog
                    .issue_native_media_load(&intake, fixture.owner.round)
                    .await
                    .unwrap(),
                "第二次调用不能取得原 LOAD 的授权"
            );
            let mut other = checkpoint.plan.clone();
            other.drive_serial.push_str("-OTHER");
            assert!(
                catalog
                    .prepare_native_media_load(&intake, fixture.owner.round, &other)
                    .await
                    .is_err(),
                "禁止改变原目标"
            );
            let next = fixture.owner.round + 1;
            fixture.permit.renew(
                next,
                Phase::Fencing,
                Instant::now() + Duration::from_secs(60),
            );
            fixture.owner = takeover(
                &Provider {
                    lib: fixture.lib.clone(),
                    commands: fixture.commands.clone(),
                },
                &fixture.opts,
                next,
                Some(&fixture.permit),
                fixture.owner.files.clone(),
            )
            .unwrap();
            fixture.permit.renew(
                next,
                Phase::Serving,
                Instant::now() + Duration::from_secs(60),
            );
            catalog
                .claim_active_owner_epoch(&control.serial, next)
                .await
                .unwrap();
            let original = intake.request();
            let resumed = catalog
                .start_native_media(
                    &control.serial,
                    control.route,
                    next,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            let inner = std::mem::replace(
                &mut fixture.owner.devices[0].dev,
                Box::new(fixture.lib.drive_as(0, 1)),
            );
            fixture.owner.devices[0].dev = Box::new(IntakeLoadBoundary {
                trigger: MechanicalBoundary::Load,
                inner,
                loads: loads.clone(),
                changed,
                later,
                fail_before_load: false,
                after_load: Arc::new(|| Ok(())),
            });
            let result = fdb_loading::select_intake(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &resumed,
            )
            .await;
            assert_eq!(
                loads.load(Ordering::SeqCst),
                1,
                "接管重放了原未知 LOAD，fail_before_load={fail_before_load}"
            );
            assert_eq!(result.is_ok(), !fail_before_load);
            assert!(
                !catalog.native_media_status(&resumed, next).await.unwrap()["abort"]
                    .as_bool()
                    .unwrap()
            );
            if fail_before_load {
                catalog
                    .abort_native_media(&resumed, next, None)
                    .await
                    .unwrap();
                assert!(
                    fdb_loading::settle_cancelled(
                        &fixture.owner,
                        &fixture.permit,
                        &control,
                        &resumed
                    )
                    .await
                    .is_err()
                );
                assert!(matches!(
                    catalog
                        .rollback_native_media_page(&resumed, next)
                        .await
                        .unwrap_err()
                        .domain(),
                    crate::managed_catalog::CatalogError::Incomplete
                ));
                assert!(
                    catalog
                        .require_active_library_readable(&control.serial, control.route, next)
                        .await
                        .is_err()
                );
                assert_eq!(loads.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 普通读写装带回执未知，不访问实际设备"]
    async fn ordinary_load_unknown_result_cannot_be_reissued_by_the_next_request() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        for fail_before_load in [false, true] {
            let (mut fixture, control, _, status) = assigned_batch_fixture(catalog.clone()).await;
            let head = catalog
                .active_loaded_volume_head(
                    &control.serial,
                    "LIFE00L8",
                    control.route,
                    fixture.owner.round,
                )
                .await
                .unwrap()
                .unwrap();
            let placement = catalog
                .volume_placement(head.volume_id)
                .await
                .unwrap()
                .unwrap();
            TapeDrive::new(fixture.device()).unload().unwrap();
            fixture.owner.fdb_reads.clear();
            let loads = Arc::new(AtomicUsize::new(0));
            let inner = std::mem::replace(
                &mut fixture.owner.devices[0].dev,
                Box::new(fixture.lib.drive_as(0, 1)),
            );
            fixture.owner.devices[0].dev = Box::new(IntakeLoadBoundary {
                trigger: MechanicalBoundary::Load,
                inner,
                loads: loads.clone(),
                changed: Arc::new(AtomicBool::new(false)),
                later: Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_before_load,
                after_load: Arc::new(|| Err(TapeError::ScsiHost(3))),
            });
            assert!(
                fdb_loading::select_write(
                    &mut fixture.owner,
                    &fixture.permit,
                    &control,
                    &status,
                    &placement,
                    head.index
                )
                .await
                .is_err()
            );
            let result = fdb_loading::select_write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &placement,
                head.index,
            )
            .await;
            assert_eq!(
                loads.load(Ordering::SeqCst),
                1,
                "新数据请求重发了原未知 LOAD，before={fail_before_load}"
            );
            assert_eq!(result.is_ok(), !fail_before_load, "{result:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim MOVE未知和取消保留原gate，不访问实际设备"]
    async fn intake_unknown_move_observes_fixed_destination_and_never_replays() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        for fail_before_load in [false, true] {
            let (mut fixture, control, pool, status) = native_fixture(catalog.clone()).await;
            TapeDrive::new(fixture.device()).unload().unwrap();
            let changer = fixture
                .owner
                .devices
                .iter()
                .position(|d| d.kind == DeviceKind::Changer)
                .unwrap();
            let mut medium = MediumChanger::new(fixture.owner.devices[changer].dev.as_ref());
            let map = medium.load_address_map().unwrap().clone();
            medium.move_medium(map.dt_start, map.storage_start).unwrap();
            let intake = begin_loading_import(&control, pool.id, fixture.owner.round).await;
            let moves = Arc::new(AtomicUsize::new(0));
            let inner = std::mem::replace(
                &mut fixture.owner.devices[changer].dev,
                Box::new(fixture.lib.changer_as(1)),
            );
            fixture.owner.devices[changer].dev = Box::new(IntakeLoadBoundary {
                trigger: MechanicalBoundary::Move,
                inner,
                loads: moves.clone(),
                changed: Arc::new(AtomicBool::new(false)),
                later: Arc::new(std::sync::Mutex::new(Vec::new())),
                fail_before_load,
                after_load: Arc::new(|| Err(TapeError::ScsiHost(3))),
            });
            assert!(
                fdb_loading::select_intake(
                    &mut fixture.owner,
                    &fixture.permit,
                    &control,
                    &status,
                    &intake
                )
                .await
                .is_err()
            );
            let result = fdb_loading::select_intake(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &intake,
            )
            .await;
            assert_eq!(moves.load(Ordering::SeqCst), 1, "未知MOVE被重发");
            assert_eq!(result.is_ok(), !fail_before_load, "{result:?}");
            if fail_before_load {
                catalog
                    .abort_native_media(&intake, fixture.owner.round, None)
                    .await
                    .unwrap();
                assert!(
                    fdb_loading::settle_cancelled(
                        &fixture.owner,
                        &fixture.permit,
                        &control,
                        &intake
                    )
                    .await
                    .is_err()
                );
                assert!(
                    catalog
                        .rollback_native_media_page(&intake, fixture.owner.round)
                        .await
                        .is_err()
                );
                assert!(
                    catalog
                        .require_active_library_readable(
                            &control.serial,
                            control.route,
                            fixture.owner.round
                        )
                        .await
                        .is_err()
                );
                assert_eq!(moves.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 自动UNLOAD/MOVE回执未知，不访问实际设备"]
    async fn ordinary_unload_unknown_keeps_original_bundle_and_excludes_new_tasks() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        for trigger in [MechanicalBoundary::Unload, MechanicalBoundary::Move] {
            for fail_before_load in [false, true] {
                let (mut fixture, control, _, status) =
                    assigned_batch_fixture(catalog.clone()).await;
                let head = catalog
                    .active_loaded_volume_head(
                        &control.serial,
                        "LIFE00L8",
                        control.route,
                        fixture.owner.round,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                let placement = catalog
                    .volume_placement(head.volume_id)
                    .await
                    .unwrap()
                    .unwrap();
                fdb_loading::select_write(
                    &mut fixture.owner,
                    &fixture.permit,
                    &control,
                    &status,
                    &placement,
                    head.index,
                )
                .await
                .unwrap();
                let device = match trigger {
                    MechanicalBoundary::Move => fixture
                        .owner
                        .devices
                        .iter()
                        .position(|d| d.kind == DeviceKind::Changer)
                        .unwrap(),
                    _ => 0,
                };
                let calls = Arc::new(AtomicUsize::new(0));
                let placeholder: Box<dyn TapeTransport + Send> = if device == 0 {
                    Box::new(fixture.lib.drive_as(0, 1))
                } else {
                    Box::new(fixture.lib.changer_as(1))
                };
                let inner = std::mem::replace(&mut fixture.owner.devices[device].dev, placeholder);
                fixture.owner.devices[device].dev = Box::new(IntakeLoadBoundary {
                    trigger,
                    inner,
                    loads: calls.clone(),
                    changed: Arc::new(AtomicBool::new(false)),
                    later: Arc::new(std::sync::Mutex::new(Vec::new())),
                    fail_before_load,
                    after_load: Arc::new(|| Err(TapeError::ScsiHost(3))),
                });
                assert!(
                    fdb_loading::unload(&mut fixture.owner, &fixture.permit, &control, 0, &['r'])
                        .await
                        .is_err()
                );
                let original = catalog
                    .automatic_media_pending(&control.serial, control.route, fixture.owner.round)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    catalog
                        .require_active_library_readable(
                            &control.serial,
                            control.route,
                            fixture.owner.round
                        )
                        .await
                        .is_err()
                );
                let (saved, grant) = catalog
                    .issue_automatic_media(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        &original.plan,
                    )
                    .await
                    .unwrap();
                assert!(!grant);
                assert_eq!(saved, original);
                let mut changed = original.plan.clone();
                changed.destination = changed.destination.wrapping_add(1);
                assert!(
                    catalog
                        .issue_automatic_media(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            &changed
                        )
                        .await
                        .is_err()
                );
                let _retry =
                    fdb_loading::unload(&mut fixture.owner, &fixture.permit, &control, 0, &['r'])
                        .await;
                assert_eq!(calls.load(Ordering::SeqCst), 1, "未知机械命令被重发");
                let pending = catalog
                    .automatic_media_pending(&control.serial, control.route, fixture.owner.round)
                    .await
                    .unwrap();
                let complete = matches!(trigger, MechanicalBoundary::Move) && !fail_before_load;
                assert_eq!(pending.is_none(), complete);
                if !complete {
                    assert_eq!(pending, Some(original));
                }
            }
        }
    }

    /// Cancel the real queued rename after a successful device response, while
    /// keeping its Raft execution permission valid. Only the FDB/task guard
    /// can prevent subsequent commands in the recipe's generic recovery path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim rename 准备中取消与未索引尾部，不访问实际设备"]
    async fn rename_recipe_stops_on_original_task_loss_during_mount_and_tail_recovery() {
        use crate::managed_catalog::{
            ManagedCatalog, ManagementRequest, NodeMutationTaskState, NodeRenameRevision,
            NodeRenameSpec, ResolvedPath,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Boundary {
            inner: Box<dyn TapeTransport + Send>,
            opcode: u8,
            fired: Arc<AtomicBool>,
            later: Arc<std::sync::Mutex<Vec<u8>>>,
            cancel: Arc<dyn Fn() -> Result<()> + Send + Sync>,
        }
        impl Boundary {
            fn before(&self, cdb: &[u8]) {
                if self.fired.load(Ordering::SeqCst) {
                    self.later.lock().unwrap().push(cdb[0]);
                }
            }
            fn after(&self, cdb: &[u8]) -> Result<()> {
                if cdb[0] == self.opcode && !self.fired.swap(true, Ordering::SeqCst) {
                    (self.cancel)()?;
                }
                Ok(())
            }
        }
        impl TapeTransport for Boundary {
            fn identity(&self) -> &str {
                self.inner.identity()
            }
            fn execution_guard(&self) -> Option<(u64, usize)> {
                self.inner.execution_guard()
            }
            fn execute_no_data(
                &self,
                cdb: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.before(cdb);
                let result = self.inner.execute_no_data(cdb, timeout)?;
                self.after(cdb)?;
                Ok(result)
            }
            fn execute_read(
                &self,
                cdb: &[u8],
                data: &mut [u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.before(cdb);
                let result = self.inner.execute_read(cdb, data, timeout)?;
                self.after(cdb)?;
                Ok(result)
            }
            fn execute_write(
                &self,
                cdb: &[u8],
                data: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.before(cdb);
                let result = self.inner.execute_write(cdb, data, timeout)?;
                self.after(cdb)?;
                Ok(result)
            }
        }
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        for opcode in [0x08, 0x0a] {
            let (mut fixture, control, pool, status) =
                Box::pin(native_fixture(catalog.clone())).await;
            fixture.format(uuid::Uuid::new_v4());
            let bytes = b"rename recipe original committed body";
            let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
            volume
                .append_file("/recipe-source.bin", &mut std::io::Cursor::new(bytes))
                .unwrap();
            volume.commit().unwrap();
            drop(volume);
            let home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
                "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}})).unwrap();
            assert_eq!(
                execute(&mut fixture, &control, &status, &home).await["status"],
                "succeeded"
            );
            let import=ManagementRequest::parse(json!({"version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
                "change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
            assert_eq!(
                execute(&mut fixture, &control, &status, &import).await["status"],
                "succeeded"
            );
            let namespace = control.route.route.namespace;
            let root_path = format!("/{}", pool.id);
            let source_path = format!("/{}/recipe-source.bin", pool.id);
            let root = catalog
                .resolve_active_path_for_route(&control.serial, root_path.as_bytes(), control.route)
                .await
                .unwrap()
                .unwrap();
            let source = catalog
                .resolve_active_path_for_route(
                    &control.serial,
                    source_path.as_bytes(),
                    control.route,
                )
                .await
                .unwrap()
                .unwrap();
            let revision = |node: &ResolvedPath| match node {
                ResolvedPath::Directory { id, version, .. } => NodeRenameRevision {
                    namespace,
                    inode: *id,
                    version: *version,
                },
                ResolvedPath::File { id, head, .. } => NodeRenameRevision {
                    namespace,
                    inode: *id,
                    version: head.version,
                },
            };
            let spec = NodeRenameSpec {
                pool: pool.id,
                operation: uuid::Uuid::new_v4(),
                from: "/recipe-source.bin".into(),
                to: "/recipe-target.bin".into(),
                source: revision(&source),
                source_parent: revision(&root),
                destination_parent: revision(&root),
                target: None,
                no_replace: true,
            };
            let request = catalog
                .prepare_node_rename_request(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &spec,
                    &crate::ltfs::label::ltfs_time_now(),
                )
                .await
                .unwrap();
            catalog
                .upload_node_rename_request(control.route, fixture.owner.round, &request, 16 << 20)
                .await
                .unwrap();
            let task = catalog
                .queue_node_rename_request(control.route, fixture.owner.round, &request, 16 << 20)
                .await
                .unwrap();
            // Deliberate Sim fixture damage only: a real unindexed DP record,
            // with all original committed file extents and FDB facts intact.
            let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
            volume
                .append_file(
                    "/uncommitted-fixture.bin",
                    &mut std::io::Cursor::new(b"unindexed tail"),
                )
                .unwrap();
            drop(volume);
            TapeDrive::new(fixture.device()).write_filemark(0).unwrap();
            let probe = LtfsVolume::mount(fixture.device()).unwrap();
            assert_eq!(
                probe.recovery().dp.tail,
                crate::ltfs::recovery::TailKind::UnindexedData
            );
            drop(probe);
            let fired = Arc::new(AtomicBool::new(false));
            let later = Arc::new(std::sync::Mutex::new(Vec::new()));
            let cancel_catalog = catalog.clone();
            let serial = control.serial.clone();
            let route = control.route;
            let round = fixture.owner.round;
            let original = task.clone();
            let inner = std::mem::replace(
                &mut fixture.owner.devices[0].dev,
                Box::new(fixture.lib.drive_as(0, 1)),
            );
            fixture.owner.devices[0].dev = Box::new(Boundary {
                inner,
                opcode,
                fired: fired.clone(),
                later: later.clone(),
                cancel: Arc::new(move || {
                    let ended = futures_executor::block_on(cancel_catalog.cancel_node_rename_task(
                        &serial,
                        route,
                        round,
                        original.request_epoch,
                        original.operation,
                    ))
                    .map_err(|e| TapeError::Catalog(e.to_string()))?;
                    assert_eq!(ended.state, NodeMutationTaskState::Cancelled);
                    Ok(())
                }),
            });
            let outcome = Box::pin(fdb_rename::advance(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &task,
            ))
            .await;
            assert!(
                fired.load(Ordering::SeqCst),
                "real recipe must reach injected response, opcode={opcode:#x}"
            );
            let later = later.lock().unwrap().clone();
            // The shared operation guard verifies PR before querying the
            // original task: one pair of read-only PR INs is its authority
            // check, not a continuation of media I/O. Once task loss is seen,
            // the latch must also prevent further PR checks on fallbacks.
            assert!(
                later.len() <= 2 && later.iter().all(|opcode| *opcode == 0x5e),
                "rename recipe continued media I/O or repeated authority probes after original task cancellation, opcode={opcode:#x}: {:?}",
                later
            );
            assert!(matches!(outcome, Err(ReadError::Lost(_))));
            assert_eq!(
                catalog
                    .node_mutation_task(
                        &control.serial,
                        control.route,
                        task.request_epoch,
                        task.operation
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                NodeMutationTaskState::Cancelled
            );
            assert_eq!(
                catalog
                    .resolve_active_path_for_route(
                        &control.serial,
                        source_path.as_bytes(),
                        control.route
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                source
            );
            assert!(
                catalog
                    .resolve_active_path_for_route(
                        &control.serial,
                        format!("/{}/recipe-target.bin", pool.id).as_bytes(),
                        control.route
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    async fn native_fixture_for_serial(
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        physical_serial: Option<&str>,
    ) -> (
        Fixture,
        FdbOwnerControl,
        crate::managed_catalog::PoolRecord,
        SharedStatus,
    ) {
        Box::pin(native_fixture_from(
            catalog,
            physical_serial,
            Fixture::new(),
        ))
        .await
    }
    async fn native_fixture_from(
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        physical_serial: Option<&str>,
        mut fixture: Fixture,
    ) -> (
        Fixture,
        FdbOwnerControl,
        crate::managed_catalog::PoolRecord,
        SharedStatus,
    ) {
        use crate::managed_catalog::{PoolPolicy, PoolRecord, verify_native_barriers};
        let namespace = uuid::Uuid::new_v4();
        let serial = physical_serial
            .map(str::to_owned)
            .unwrap_or_else(|| format!("SIM-LIFECYCLE-{namespace}"));
        // 此隔离模拟fixture以独立配置身份避免覆盖其他FDB测试域；真实VPD绑定另有测试。
        fixture
            .owner
            .devices
            .iter_mut()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .serial = serial.clone();
        let challenge = uuid::Uuid::new_v4();
        let replies = (1..=3).map(|node| json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":1,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":1,"frozen_index":2,"applied_index":3,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                namespace,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, fixture.owner.round)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: uuid::Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "lifecycle".into(),
            file_limit: 100,
            created_at_millis: 1,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, fixture.owner.round, &pool)
            .await
            .unwrap();
        let control = FdbOwnerControl {
            catalog: catalog.clone(),
            serial: serial.clone(),
            route,
            storage: fixture.budget(),
        };
        let status = Arc::new(std::sync::Mutex::new(
            super::super::super::node::NodeStatus::default(),
        ));
        (fixture, control, pool, status)
    }

    fn execute<'a>(
        fixture: &'a mut Fixture,
        control: &'a FdbOwnerControl,
        status: &'a SharedStatus,
        request: &'a crate::managed_catalog::ManagementRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + 'a>> {
        // Each independent job owns its future on the heap rather than
        // enlarging every state of the multi-job integration test future.
        Box::pin(async move {
            let admitted = control
                .catalog
                .admit_management(&control.serial, control.route, fixture.owner.round, request)
                .await
                .unwrap();
            if matches!(
                admitted["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                return control
                    .catalog
                    .management_task(
                        &control.serial,
                        control.route,
                        request.epoch(),
                        request.operation(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
            }
            if matches!(
                request.value()["change"]["kind"].as_str(),
                Some("media-assign" | "media-move" | "library-rescan")
            ) {
                return tokio::task::block_in_place(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    fixture.owner.serving = true;
                    {
                        let mut current = status.lock().unwrap();
                        current.id = u64::from(fixture.opts.node_id);
                        current.executor_ready = true;
                        current.catalog_frozen_index = Some(control.route.frozen_index);
                        current.executor = Some(super::super::super::state::Executor {
                            node: u64::from(fixture.opts.node_id),
                            round: fixture.owner.round,
                            term: 1,
                            fenced: true,
                        });
                    }
                    let mut observer = fdb_rescan::Observer::default();
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        observer
                            .poll_device(
                                &mut fixture.owner,
                                &fixture.permit,
                                control,
                                status,
                                fixture.opts.node_id,
                                &runtime,
                            )
                            .unwrap();
                        let task = runtime
                            .block_on(control.catalog.management_task(
                                &control.serial,
                                control.route,
                                request.epoch(),
                                request.operation(),
                            ))
                            .unwrap()
                            .unwrap();
                        if matches!(
                            task["status"].as_str(),
                            Some("succeeded" | "failed" | "cancelled")
                        ) {
                            return task;
                        }
                        assert!(Instant::now() < deadline, "原main任务未达到终态: {task}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
            }
            for _ in 0..5 {
                let result = if matches!(
                    request.value()["change"]["kind"].as_str(),
                    Some("media-import")
                ) {
                    fdb_intake::apply(
                        &mut fixture.owner,
                        &fixture.permit,
                        control,
                        &fixture.opts,
                        status,
                        request.epoch(),
                        request.operation(),
                    )
                    .await
                } else {
                    apply(
                        &mut fixture.owner,
                        &fixture.permit,
                        control,
                        &fixture.opts,
                        request.epoch(),
                        request.operation(),
                    )
                    .await
                };
                assert!(result.is_ok(), "executor: {result:?}");
                let task = control
                    .catalog
                    .management_task(
                        &control.serial,
                        control.route,
                        request.epoch(),
                        request.operation(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(
                    task["status"].as_str(),
                    Some("succeeded" | "failed" | "cancelled")
                ) {
                    return task;
                }
                eprintln!("pending lifecycle {}", task);
            }
            panic!("生命周期任务未达到终态");
        })
    }
    struct MetadataReadBoundary<'a> {
        inner: &'a dyn TapeTransport,
        after: &'a dyn Fn() -> Result<()>,
        triggered: std::sync::atomic::AtomicBool,
    }
    impl TapeTransport for MetadataReadBoundary<'_> {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        // A fault-capable proxy never offers a reusable write session epoch.
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            buf: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.inner.execute_write(cdb, buf, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            buf: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            let result = self.inner.execute_read(cdb, buf, timeout)?;
            if cdb.first() == Some(&0x08)
                && !self
                    .triggered
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                (self.after)()?;
            }
            Ok(result)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实Sim owner/PR目录与链接实读，不访问真实设备"]
    async fn reclaim_node_metadata_reader_requires_original_fences_and_actual_destination() {
        Box::pin(reclaim_node_metadata_case(false, false, false, false)).await;
    }

    async fn prepare_raw_body_target(
        fixture: &mut Fixture,
        control: &FdbOwnerControl,
        pool: &crate::managed_catalog::PoolRecord,
        status: &SharedStatus,
    ) -> uuid::Uuid {
        let request = |change: Value| {
            crate::managed_catalog::ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("RAW001L8", 128 << 20), 1)
            .unwrap();
        let changer = fixture
            .owner
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .dev
            .as_ref();
        let mut robot = MediumChanger::new(changer);
        let map = robot.load_address_map().unwrap().clone();
        robot
            .move_medium(map.storage_start + 1, map.dt_start)
            .unwrap();
        let id = uuid::Uuid::new_v4();
        mkltfs(
            fixture.device(),
            &MkltfsOptions {
                volume_id: "RAW001".into(),
                volume_uuid: Some(id),
                block_size: fixture.opts.block_size,
                ..Default::default()
            },
        )
        .unwrap();
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.create_directory("/archive").unwrap();
        volume.commit().unwrap();
        drop(volume);
        assert_eq!(execute(fixture, control, status, &request(json!({"kind":"media-move","pool":null,"barcode":"RAW001L8","volume":null,"revision":null,"destination":"homeslot"}))).await["status"], "succeeded");
        let imported = execute(fixture, control, status, &request(json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"RAW001L8","volume":null,"path":"/target","merge_tapes":true}))).await;
        assert_eq!(imported["status"], "succeeded", "{imported}");
        id
    }

    #[allow(clippy::too_many_arguments)]
    async fn verify_raw_body_actor(
        catalog: &crate::managed_catalog::ManagedCatalog,
        fixture: &mut Fixture,
        control: &FdbOwnerControl,
        status: &SharedStatus,
        parent: &crate::managed_catalog::ReclaimReservation,
        source: &crate::ltfs::recovery::CapturedIndexSource,
        proof: &crate::managed_catalog::ReclaimRawSourceRead,
        bytes: &[u8],
        target_uuid: uuid::Uuid,
        repair: bool,
    ) {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::ReclaimSourceInventory;
        let placement = catalog
            .volume_placement(target_uuid)
            .await
            .unwrap()
            .unwrap();
        let head = catalog
            .active_loaded_volume_head(
                &control.serial,
                &placement.barcode,
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            control,
            status,
            &placement,
            head.index,
        )
        .await
        .unwrap();
        let device = fixture.device();
        let label = crate::ltfs::volume::read_label_only(device).unwrap();
        let budget = RecoveryBudget::default();
        let target = capture_index_source(&TapeDrive::new(device), &label, &budget).unwrap();
        let inventory = ReclaimSourceInventory::from_captured(source).unwrap();
        let mut plan = inventory
            .plan_reclaimed_raw_body(parent, proof, &target, &label, "archive/raw")
            .unwrap();
        let mut task = catalog
            .prepare_reclaim_raw_body(parent, fixture.owner.round, &plan, proof)
            .await
            .unwrap();
        assert!(
            catalog
                .cancel_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    parent.intent()["epoch"].as_u64().unwrap(),
                    uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap(),
                )
                .await
                .is_err(),
            "已准备原迁移任务，取消不能单独释放父屏障"
        );
        assert_eq!(
            catalog
                .prepare_reclaim_raw_body(parent, fixture.owner.round, &plan, proof)
                .await
                .unwrap(),
            task
        );
        assert!(
            catalog
                .prepare_reclaim_metadata_node(parent, fixture.owner.round, &plan)
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .reserved_reclaim_status(parent, fixture.owner.round)
                .await
                .unwrap()["phase"],
            "copying"
        );
        assert!(
            catalog
                .release_reserved_reclaim(parent, fixture.owner.round)
                .await
                .is_err()
        );
        assert!(
            catalog
                .seal_reclaim_raw_file_coverage(
                    parent,
                    fixture.owner.round,
                    proof.source_node().uid(),
                    task.id()
                )
                .await
                .is_err()
        );
        let fault = MetadataFault {
            inner: device,
            mode: if repair {
                MetadataExecutionCase::DiscardIntent
            } else {
                MetadataExecutionCase::Commit
            },
            matching: std::sync::atomic::AtomicUsize::new(0),
            fired: std::sync::atomic::AtomicBool::new(false),
            permission: &fixture.permit,
            round: fixture.owner.round,
        };
        let mut writer = crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter {
            catalog,
            device: &fault,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            budget: &budget,
        };
        let before = fixture.drive_log();
        assert!(writer.execute(parent, &task, Some(source)).await.is_err());
        assert_eq!(
            fixture.drive_log(),
            before,
            "缺少正文不能进入 Writing 或发送 CDB"
        );
        if repair {
            let error = writer
                .execute_raw_body(
                    parent,
                    &task,
                    Some(source),
                    &mut std::io::Cursor::new(bytes),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, TapeError::CommitFailed { ref stage, .. } if stage == "closing_fm"),
                "{error}"
            );
            fixture.lib.with_cartridge_mut("RAW001L8", |cartridge| {
                for partition in &mut cartridge.partitions {
                    partition.flushed = partition.objects.len();
                }
            });
            let writing = catalog
                .reclaim_metadata_write_status(parent, fixture.owner.round, plan.source_uid())
                .await
                .unwrap()
                .unwrap();
            assert!(writing.writing());
            assert!(
                writer.execute(parent, &writing, None).await.is_err(),
                "残缺尾部不能重写正文或伪报成功"
            );
            assert!(
                catalog
                    .seal_reclaim_raw_file_coverage(
                        parent,
                        fixture.owner.round,
                        plan.source_uid(),
                        writing.id()
                    )
                    .await
                    .is_err()
            );
            fixture.permit.renew(
                fixture.owner.round,
                Phase::Serving,
                Instant::now() + Duration::from_secs(60),
            );
            let goal = writer.begin_discard(parent, &writing).await.unwrap();
            let crate::daemon::fdb_reclaim_media::ReclaimMetadataRecovery::Observed(observed) =
                writer.recover(parent, &goal).await.unwrap()
            else {
                panic!("fresh recovery")
            };
            let resolution = catalog
                .resolve_reclaim_metadata_recovery(parent, fixture.owner.round, &goal, &observed)
                .await
                .unwrap();
            assert_eq!(
                resolution.kind(),
                crate::managed_catalog::ReclaimMetadataRecoveryKind::Discarded
            );
            assert!(
                catalog
                    .seal_reclaim_raw_file_coverage(
                        parent,
                        fixture.owner.round,
                        plan.source_uid(),
                        writing.id()
                    )
                    .await
                    .is_err()
            );
            let old = catalog
                .reclaim_metadata_write_status(parent, fixture.owner.round, plan.source_uid())
                .await
                .unwrap()
                .unwrap();
            let live = writer
                .read_source_replan(parent, &old, source)
                .await
                .unwrap();
            plan = live.plan().clone();
            let operation = uuid::Uuid::new_v4();
            let (one, two) = tokio::join!(
                catalog.replan_reclaim_metadata_source(
                    parent,
                    fixture.owner.round,
                    &old,
                    operation,
                    &live
                ),
                catalog.replan_reclaim_metadata_source(
                    parent,
                    fixture.owner.round,
                    &old,
                    operation,
                    &live
                )
            );
            task = one.unwrap();
            assert_eq!(two.unwrap(), task);
            assert_eq!(
                catalog
                    .reclaim_metadata_successor(parent, fixture.owner.round, &old, operation)
                    .await
                    .unwrap(),
                Some(task.clone())
            );
            assert!(
                catalog
                    .replan_reclaim_metadata_source(
                        parent,
                        fixture.owner.round,
                        &old,
                        uuid::Uuid::new_v4(),
                        &live
                    )
                    .await
                    .is_err()
            );
            let closed = catalog
                .close_prepared_reclaim_metadata_write(parent, fixture.owner.round, &task)
                .await
                .unwrap();
            task = catalog
                .retry_reclaim_metadata_write(parent, fixture.owner.round, &closed)
                .await
                .unwrap();
        }
        fixture.permit.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let result = writer
            .execute_raw_body(
                parent,
                &task,
                Some(source),
                &mut std::io::Cursor::new(bytes),
            )
            .await
            .unwrap();
        let proof_commit = result.commit_evidence().unwrap();
        assert_eq!(
            proof_commit.observation().current().location.start_block,
            plan.previous_dp_eod() + (bytes.len() as u64).div_ceil(u64::from(label.blocksize)) + 1
        );
        let before = fixture.drive_log();
        fixture.permit.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let replay = writer.execute(parent, result.task(), None).await.unwrap();
        assert!(replay.commit_evidence().is_some());
        assert!(
            !fixture.drive_log()[before.len()..]
                .iter()
                .any(|op| matches!(*op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d)),
            "结果重入只读"
        );
        // Damage only the actual destination body while keeping the XML and FDB
        // intact. A replay must refuse a fresh publication capability.
        fixture.lib.with_cartridge_mut("RAW001L8", |cartridge| {
            let crate::scsi::sim::LogicalObject::Record(record) =
                &mut cartridge.partitions[1].objects[plan.previous_dp_eod() as usize]
            else {
                panic!("record")
            };
            record[0] ^= 1;
            cartridge.vcr += 1;
        });
        fixture.permit.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        assert!(
            writer.execute(parent, replay.task(), None).await.is_err(),
            "索引匹配而正文损坏不能产生提交证明"
        );
        assert!(
            catalog
                .seal_reclaim_raw_file_coverage(
                    parent,
                    fixture.owner.round,
                    plan.source_uid(),
                    task.id()
                )
                .await
                .is_err()
        );
        fixture.lib.with_cartridge_mut("RAW001L8", |cartridge| {
            let crate::scsi::sim::LogicalObject::Record(record) =
                &mut cartridge.partitions[1].objects[plan.previous_dp_eod() as usize]
            else {
                panic!("record")
            };
            record[0] ^= 1;
            cartridge.vcr += 1;
        });
        fixture.permit.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let final_read = writer.execute(parent, replay.task(), None).await.unwrap();
        let published = catalog
            .publish_reclaim_metadata_write(
                parent,
                fixture.owner.round,
                final_read.task(),
                final_read.commit_evidence().unwrap(),
            )
            .await
            .unwrap();
        catalog
            .seal_reclaim_raw_file_coverage(
                parent,
                fixture.owner.round,
                plan.source_uid(),
                task.id(),
            )
            .await
            .unwrap();
        assert_eq!(
            catalog
                .active_volume_file_count(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    target_uuid
                )
                .await
                .unwrap(),
            1
        );
        assert!(
            catalog
                .step_reclaim_source_coverage(parent, fixture.owner.round, &inventory)
                .await
                .is_err(),
            "只复制一个原始文件不能跳过其他源节点"
        );
        let before = fixture.drive_log();
        assert!(
            writer
                .execute(parent, &task, None)
                .await
                .unwrap()
                .task()
                .completed()
        );
        assert_eq!(fixture.drive_log(), before, "独立发布回执不再读带或写带");
        let next = fixture.owner.round + 100;
        catalog
            .claim_active_owner_epoch(&control.serial, next)
            .await
            .unwrap();
        let parents = catalog
            .claim_reclaim_reservations_page(&control.serial, control.route, next, None, 16)
            .await
            .unwrap();
        let recovered = catalog
            .claim_reclaim_metadata_write(&parents.items[0], next, plan.source_uid())
            .await
            .unwrap();
        assert!(recovered.completed());
        catalog
            .seal_reclaim_raw_file_coverage(&parents.items[0], next, plan.source_uid(), task.id())
            .await
            .unwrap();
        assert_eq!(
            catalog
                .reclaim_metadata_publication(&parents.items[0], next, &recovered)
                .await
                .unwrap(),
            Some(published)
        );
        assert_eq!(fixture.drive_log(), before, "HA 读取独立结果没有 CDB");
        assert_eq!(fixture.formats(), 2, "只有两个预格式化测试介质");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原始未映射正文实读、摘要和失权边界"]
    async fn reclaim_raw_source_reads_unmapped_ee_bytes_without_logical_publication() {
        Box::pin(reclaim_raw_body_case(false, false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 未映射原始正文追加、实读、发布、覆盖与接管"]
    async fn reclaim_raw_body_actor_streams_unmapped_data_and_reads_back_before_publication() {
        Box::pin(reclaim_raw_body_case(true, false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原始正文残缺索引、原目标修尾、实际重新规划与接管"]
    async fn reclaim_raw_body_recovery_replans_and_preserves_original_source_bytes() {
        Box::pin(reclaim_raw_body_case(true, true)).await;
    }

    async fn reclaim_raw_body_case(append: bool, repair: bool) {
        use crate::ltfs::index::IndexEntry;
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::ltfs::volume::HashPolicy;
        use crate::managed_catalog::{
            ManagedCatalog, ManagementRequest, ReclaimSourceInventory, ReclaimSpec,
        };
        use sha2::{Digest, Sha256};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        fixture.format(uuid::Uuid::new_v4());
        let bytes = b"unmapped-EE-body".repeat(10_000);
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.set_hash_policy(HashPolicy {
            sha256: false,
            md5: false,
        });
        volume.append_bytes("/.LTFSEE_DATA/orphan", &bytes).unwrap();
        volume.set_hash_policy(HashPolicy {
            sha256: true,
            md5: true,
        });
        volume
            .append_bytes("/.LTFSEE_DATA/known-body", b"known body")
            .unwrap();
        volume
            .add_symlink("/known", ".LTFSEE_DATA/known-body")
            .unwrap();
        volume.commit().unwrap();
        drop(volume);
        let captured = capture_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        let mut raw_uid = None;
        let mut known = None;
        captured
            .for_each_entry(|entry| {
                if let IndexEntry::File { path, node } = entry {
                    if path == ".LTFSEE_DATA/orphan" {
                        raw_uid = Some(node.meta.file_uid);
                    } else if path == ".LTFSEE_DATA/known-body" {
                        known = Some((node.meta.file_uid, node.extents[0].start_block));
                    }
                }
                Ok(())
            })
            .unwrap();
        let uid = raw_uid.unwrap();
        let home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}})).unwrap();
        let moved = execute(&mut fixture, &control, &status, &home).await;
        assert_eq!(moved["status"], "succeeded", "{moved}");
        let target_uuid = if append {
            Some(
                Box::pin(prepare_raw_body_target(
                    &mut fixture,
                    &control,
                    &pool,
                    &status,
                ))
                .await,
            )
        } else {
            None
        };
        let import=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        let imported = execute(&mut fixture, &control, &status, &import).await;
        assert_eq!(imported["status"], "succeeded", "{imported}");
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,
            "volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false})).unwrap();
        let parent = if append {
            let cancelled_request=ManagementRequest::parse(json!({"version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":spec.to_json()})).unwrap();
            let commands = fixture.drive_log();
            catalog
                .admit_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &cancelled_request,
                )
                .await
                .unwrap();
            let cancelled_parent = catalog
                .start_reclaim_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    cancelled_request.epoch(),
                    cancelled_request.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            assert!(
                catalog
                    .cancel_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round + 1,
                        cancelled_request.epoch(),
                        cancelled_request.operation()
                    )
                    .await
                    .is_err()
            );
            let cancelled = catalog
                .cancel_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    cancelled_request.epoch(),
                    cancelled_request.operation(),
                )
                .await
                .unwrap();
            assert_eq!(cancelled["status"], "cancelled");
            assert_eq!(cancelled["result"]["source_unchanged"], true);
            assert_eq!(
                catalog
                    .cancel_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        cancelled_request.epoch(),
                        cancelled_request.operation()
                    )
                    .await
                    .unwrap(),
                cancelled
            );
            assert!(
                catalog
                    .start_reclaim_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        cancelled_request.epoch(),
                        cancelled_request.operation()
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                catalog
                    .reserved_reclaim_status(&cancelled_parent, fixture.owner.round)
                    .await
                    .is_err()
            );
            assert_eq!(
                fixture.drive_log(),
                commands,
                "未开写取消、重复查询及错误 owner 均不发 CDB"
            );
            let original=ManagementRequest::parse(json!({"version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":spec.to_json()})).unwrap();
            let queued = catalog
                .admit_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &original,
                )
                .await
                .unwrap();
            assert_eq!(queued["status"], "queued");
            let before = fixture.drive_log();
            let reservation = catalog
                .start_reclaim_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                catalog
                    .start_reclaim_management(
                        &control.serial,
                        control.route,
                        fixture.owner.round,
                        original.epoch(),
                        original.operation()
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .intent(),
                reservation.intent()
            );
            let task = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task["status"], "running");
            assert_eq!(task["progress"]["phase"], "reserved");
            assert!(
                catalog
                    .release_reserved_reclaim(&reservation, fixture.owner.round)
                    .await
                    .is_err(),
                "不能单独清除运行中的异步父任务"
            );
            assert_eq!(
                fixture.drive_log(),
                before,
                "异步父回执与互斥占用原子绑定，没有设备命令"
            );
            reservation
        } else {
            catalog
                .reserve_reclaim_pool(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    1,
                    uuid::Uuid::new_v4(),
                    &spec,
                )
                .await
                .unwrap()
        };
        let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
        assert!(
            catalog
                .seal_reclaim_source_inventory_page(&parent, fixture.owner.round, &inventory)
                .await
                .unwrap()
                .complete()
        );
        if append {
            Box::pin(verify_reclaim_mechanical_actor(
                &catalog, &fixture, &control, &parent,
            ))
            .await;
        }
        let path = format!("/{}/.LTFSEE_DATA/orphan", pool.id);
        assert!(
            catalog
                .resolve_active_read_source_for_owner(
                    &control.serial,
                    path.as_bytes(),
                    control.route,
                    fixture.owner.round
                )
                .await
                .unwrap()
                .is_none(),
            "未映射数据对象不是一个用户文件"
        );
        let reader = crate::daemon::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.device(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: fixture.budget(),
        };
        struct Count {
            sha: Sha256,
            length: u64,
            max: usize,
        }
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.sha.update(bytes);
                self.length += bytes.len() as u64;
                self.max = self.max.max(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let before = fixture.drive_log().len();
        let mut sink = Count {
            sha: Sha256::new(),
            length: 0,
            max: 0,
        };
        let proof = reader
            .read_reclaim_raw_source_to(&parent, uid, fixture.owner.files.spool_dir(), &mut sink)
            .await
            .unwrap();
        assert_eq!(proof.source_node().uid(), uid);
        assert_eq!(proof.source_node().path(), ".LTFSEE_DATA/orphan");
        assert_eq!(proof.observed_owner(), fixture.owner.round);
        assert_eq!(sink.length, bytes.len() as u64);
        assert_eq!(proof.sha256(), <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(proof.md5(), None);
        assert!(
            sink.max < bytes.len(),
            "多记录正文必须分段输出，不能整体暂存在内存"
        );
        assert!(
            !fixture.drive_log()[before..]
                .iter()
                .any(|op| matches!(*op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
        );
        assert!(
            catalog
                .resolve_active_read_source_for_owner(
                    &control.serial,
                    path.as_bytes(),
                    control.route,
                    fixture.owner.round
                )
                .await
                .unwrap()
                .is_none(),
            "原始读取不能补建或发布用户路径"
        );
        assert!(
            catalog
                .step_reclaim_source_coverage(&parent, fixture.owner.round, &inventory)
                .await
                .is_err(),
            "源正文实读不是迁移/覆盖证明"
        );
        if let Some(target) = target_uuid {
            Box::pin(verify_raw_body_actor(
                &catalog,
                &mut fixture,
                &control,
                &status,
                &parent,
                &captured,
                &proof,
                &bytes,
                target,
                repair,
            ))
            .await;
            return;
        }
        let before = fixture.drive_log();
        assert!(
            reader
                .read_reclaim_raw_source_to(
                    &parent,
                    1,
                    fixture.owner.files.spool_dir(),
                    &mut std::io::sink()
                )
                .await
                .is_err()
        );
        assert_eq!(
            fixture.drive_log(),
            before,
            "目录 UID 在 FDB 预检阶段拒绝，不发送 CDB"
        );
        let (known_uid, block) = known.unwrap();
        fixture.lib.with_cartridge_mut("LIFE00L8", |cartridge| {
            let crate::scsi::sim::LogicalObject::Record(record) =
                &mut cartridge.partitions[1].objects[block as usize]
            else {
                panic!("record")
            };
            record[0] ^= 1;
            cartridge.vcr += 1;
        });
        assert!(
            reader
                .read_reclaim_raw_source_to(
                    &parent,
                    known_uid,
                    fixture.owner.files.spool_dir(),
                    &mut std::io::sink()
                )
                .await
                .is_err(),
            "FDB/索引仍保留原摘要，实际正文损坏必须拒绝证据"
        );
        fixture.lib.with_cartridge_mut("LIFE00L8", |cartridge| {
            let crate::scsi::sim::LogicalObject::Record(record) =
                &mut cartridge.partitions[1].objects[block as usize]
            else {
                panic!("record")
            };
            record[0] ^= 1;
            cartridge.vcr += 1;
        });
        let close = || -> Result<()> {
            fixture.permit.close(fixture.owner.round);
            Ok(())
        };
        let boundary = MetadataReadBoundary {
            inner: fixture.device(),
            after: &close,
            triggered: std::sync::atomic::AtomicBool::new(false),
        };
        let stopped = crate::daemon::fdb_read::ActiveMediaReader {
            device: &boundary,
            ..reader
        };
        assert!(
            stopped
                .read_reclaim_raw_source_to(
                    &parent,
                    uid,
                    fixture.owner.files.spool_dir(),
                    &mut std::io::sink()
                )
                .await
                .is_err()
        );
        assert!(boundary.triggered.load(std::sync::atomic::Ordering::SeqCst));
        catalog
            .release_reserved_reclaim(&parent, fixture.owner.round)
            .await
            .unwrap();
        assert_eq!(fixture.formats(), 1, "只有最初模拟源卷预格式化");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生节点写入/发布/接管，无真实设备"]
    async fn reclaim_node_metadata_actor_preserves_a_separate_root_and_survives_takeover() {
        Box::pin(reclaim_node_metadata_case(true, false, false, false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 节点 T2 修尾、实读重规划及接管"]
    async fn reclaim_node_metadata_recovery_replans_after_actual_index_read() {
        Box::pin(reclaim_node_metadata_case(true, true, false, false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生链接元数据提交、逻辑迁移、精确退休与接管"]
    async fn reclaim_native_link_relocates_current_copy_without_empty_upload() {
        Box::pin(reclaim_node_metadata_case(true, false, true, false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生回收初始化、实际归位和原子解绑"]
    async fn reclaim_format_unassign_requires_empty_baseline_and_confirmed_original_home() {
        Box::pin(reclaim_node_metadata_case(true, false, true, true)).await;
    }

    async fn reclaim_node_metadata_case(
        execute_node: bool,
        repair: bool,
        relocate: bool,
        unassign: bool,
    ) {
        use crate::ltfs::index::IndexEntry;
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{
            ManagedCatalog, ManagementRequest, ReclaimNodeDestination, ReclaimNodeReadRequest,
            ReclaimSourceInventory, ReclaimSpec,
        };
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        let request = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,
            "operation":uuid::Uuid::new_v4().to_string(),"change":change}))
            .unwrap()
        };
        fixture.format(uuid::Uuid::new_v4());
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.create_directory("/empty").unwrap();
        volume.add_symlink("/link", "../missing").unwrap();
        volume
            .set_node_xattr(
                "/link",
                crate::ltfs::index::Xattr {
                    key: "tapers.fdb.nativeSymlink".into(),
                    value: "1".into(),
                    base64: false,
                },
                false,
                false,
            )
            .unwrap();
        volume
            .set_node_xattr(
                "/empty",
                crate::ltfs::index::Xattr {
                    key: "user.binary".into(),
                    value: "AP9B".into(),
                    base64: true,
                },
                false,
                false,
            )
            .unwrap();
        volume.commit().unwrap();
        drop(volume);
        let captured = capture_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        let mut source_nodes = Vec::new();
        captured
            .for_each_entry(|entry| {
                source_nodes.push(entry);
                Ok(())
            })
            .unwrap();
        assert_eq!(source_nodes.len(), 3);
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let home = request(
            json!({"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );

        // Preformatted foreign media are fixture setup, before any import or
        // reclaim admission. No naked write is made during the managed read.
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("NODES1L8", 128 << 20), 1)
            .unwrap();
        let changer = fixture
            .owner
            .devices
            .iter()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .dev
            .as_ref();
        let mut robot = MediumChanger::new(changer);
        let map = robot.load_address_map().unwrap().clone();
        robot
            .move_medium(map.storage_start + 1, map.dt_start)
            .unwrap();
        let target_uuid = uuid::Uuid::new_v4();
        mkltfs(
            fixture.device(),
            &MkltfsOptions {
                volume_id: "NODES1".into(),
                volume_uuid: Some(target_uuid),
                block_size: fixture.opts.block_size,
                ..Default::default()
            },
        )
        .unwrap();
        let mut target = LtfsVolume::mount(fixture.device()).unwrap();
        target.create_directory("/padding").unwrap();
        target.create_directory("/empty").unwrap();
        target.add_symlink("/link", "../missing").unwrap();
        for entry in &source_nodes {
            let path = match entry {
                IndexEntry::Directory { path, .. } | IndexEntry::File { path, .. } => path,
            };
            target
                .preserve_reclaimed_metadata(&format!("/{path}"), entry)
                .unwrap();
        }
        target.commit_incremental().unwrap();
        let mut targets = Vec::new();
        for entry in &source_nodes {
            match entry {
                IndexEntry::Directory { path, node } => targets.push((
                    node.meta.file_uid,
                    target.index().find_directory(path).unwrap().meta.file_uid,
                )),
                IndexEntry::File { path, node } => targets.push((
                    node.meta.file_uid,
                    target.index().find_file(path).unwrap().meta.file_uid,
                )),
            }
        }
        drop(target);
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"NODES1L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"NODES1L8","volume":null,"path":"/mirror","merge_tapes":true}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":unassign})).unwrap();
        let reserved = if relocate {
            let original = request(spec.to_json());
            catalog
                .admit_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    &original,
                )
                .await
                .unwrap();
            catalog
                .start_reclaim_management(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap()
        } else {
            catalog
                .reserve_reclaim_pool(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    1,
                    uuid::Uuid::new_v4(),
                    &spec,
                )
                .await
                .unwrap()
        };
        let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
        assert!(
            catalog
                .seal_reclaim_source_inventory_page(&reserved, fixture.owner.round, &inventory)
                .await
                .unwrap()
                .complete()
        );
        let placement = catalog
            .volume_placement(target_uuid)
            .await
            .unwrap()
            .unwrap();
        let head = catalog
            .active_loaded_volume_head(
                &control.serial,
                &placement.barcode,
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &placement,
            head.index,
        )
        .await
        .unwrap();
        let cache_dir = fixture.owner.files.extent_cache_dir();
        let spool = fixture.owner.files.spool_dir();
        let before = fixture.drive_log().len();
        let changer_before = fixture.changer_log();
        let source_uids: Vec<_> = targets.iter().map(|(source, _)| *source).collect();
        let originals = catalog
            .reclaim_source_nodes(&reserved, fixture.owner.round, &inventory, &source_uids)
            .await
            .unwrap();
        let requests: Vec<_> = originals
            .iter()
            .zip(&targets)
            .map(|(original, (_, uid))| ReclaimNodeReadRequest {
                original: original.clone(),
                destination: ReclaimNodeDestination {
                    volume: target_uuid,
                    uid: *uid,
                },
            })
            .collect();
        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.device(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let batch = reader
            .read_reclaim_node_metadata(&reserved, &inventory, &requests, &cache_dir, spool)
            .await
            .unwrap();
        assert_eq!(batch.len(), 3);
        let batch_reads = fixture.drive_log()[before..]
            .iter()
            .filter(|op| **op == 0x08)
            .count();
        let single_begin = fixture.drive_log().len();
        for (request, observed) in requests.iter().zip(&batch) {
            let single = reader
                .read_reclaim_node_metadata(
                    &reserved,
                    &inventory,
                    std::slice::from_ref(request),
                    &cache_dir,
                    spool,
                )
                .await
                .unwrap();
            assert_eq!(single.as_slice(), std::slice::from_ref(observed));
            assert_eq!(observed.source_uid(), request.original.uid());
            assert_eq!(observed.destination(), request.destination);
            assert_eq!(observed.target_path(), request.original.path());
        }
        let single_reads = fixture.drive_log()[single_begin..]
            .iter()
            .filter(|op| **op == 0x08)
            .count();
        assert!(
            batch_reads > 0 && single_reads > batch_reads,
            "batch READ count {batch_reads}, three singleton READs {single_reads}"
        );
        eprintln!(
            "metadata readback READs: batch={batch_reads}, three singleton calls={single_reads}"
        );
        let expected_fingerprints: Vec<_> = batch
            .iter()
            .map(|proof| proof.fingerprint().unwrap())
            .collect();
        let observation_key = |uid: u64, fp: [u8; 32]| {
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(control.route.route.namespace.as_bytes());
            key.extend_from_slice(b"Rn");
            key.extend_from_slice(&reserved.intent()["epoch"].as_u64().unwrap().to_be_bytes());
            key.extend_from_slice(
                uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
                    .unwrap()
                    .as_bytes(),
            );
            key.extend_from_slice(&uid.to_be_bytes());
            key.extend_from_slice(&fp);
            key
        };
        let db_meta = foundationdb::Database::new(Some(&cluster)).unwrap();
        let keys: Vec<_> = batch
            .iter()
            .zip(&expected_fingerprints)
            .map(|(proof, fp)| observation_key(proof.source_uid(), *fp))
            .collect();
        assert!(
            catalog
                .reclaim_node_observation(
                    &reserved,
                    fixture.owner.round,
                    batch[0].source_uid(),
                    expected_fingerprints[0]
                )
                .await
                .unwrap()
                .is_none()
        );
        let tr = db_meta.create_trx().unwrap();
        tr.set(&keys[1], b"corrupt-observation");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .record_reclaim_node_observations(&reserved, fixture.owner.round, &batch)
                .await
                .is_err()
        );
        let tr = db_meta.create_trx().unwrap();
        assert!(tr.get(&keys[0], false).await.unwrap().is_none());
        assert!(tr.get(&keys[2], false).await.unwrap().is_none());
        assert_eq!(
            tr.get(&keys[1], false).await.unwrap().unwrap().as_ref(),
            b"corrupt-observation"
        );
        tr.clear(&keys[1]);
        tr.commit().await.unwrap();
        let no_record_io = fixture.drive_log();
        assert_eq!(
            catalog
                .record_reclaim_node_observations(&reserved, fixture.owner.round, &batch)
                .await
                .unwrap(),
            expected_fingerprints
        );
        assert_eq!(
            catalog
                .record_reclaim_node_observations(&reserved, fixture.owner.round, &batch)
                .await
                .unwrap(),
            expected_fingerprints,
            "原观察记录重提幂等"
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        for (proof, fp) in batch.iter().zip(&expected_fingerprints) {
            assert_eq!(
                reopened
                    .reclaim_node_observation(
                        &reserved,
                        fixture.owner.round,
                        proof.source_uid(),
                        *fp
                    )
                    .await
                    .unwrap()
                    .as_ref(),
                Some(proof)
            );
        }
        assert_eq!(
            fixture.drive_log(),
            no_record_io,
            "记录及接管读取观察结果不访问介质"
        );
        assert!(
            catalog
                .record_reclaim_node_observations(&reserved, fixture.owner.round + 1, &batch)
                .await
                .is_err()
        );
        assert!(
            catalog
                .reclaim_node_observation(
                    &reserved,
                    fixture.owner.round + 1,
                    batch[0].source_uid(),
                    expected_fingerprints[0]
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .reclaim_node_observation(
                    &reserved,
                    fixture.owner.round,
                    0,
                    expected_fingerprints[0]
                )
                .await
                .is_err()
        );
        for invalid in [
            Vec::new(),
            vec![batch[0].clone(); 33],
            vec![batch[0].clone(); 2],
        ] {
            assert!(
                catalog
                    .record_reclaim_node_observations(&reserved, fixture.owner.round, &invalid)
                    .await
                    .is_err()
            );
        }
        let tr = db_meta.create_trx().unwrap();
        let raw = tr.get(&keys[0], false).await.unwrap().unwrap().to_vec();
        let canonical_tamper = String::from_utf8(raw.clone())
            .unwrap()
            .replace("NODES1L8", "OTHER1L8")
            .into_bytes();
        assert_ne!(canonical_tamper, raw);
        tr.set(&keys[0], &canonical_tamper);
        tr.commit().await.unwrap();
        assert!(
            reopened
                .reclaim_node_observation(
                    &reserved,
                    fixture.owner.round,
                    batch[0].source_uid(),
                    expected_fingerprints[0]
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .record_reclaim_node_observations(&reserved, fixture.owner.round, &batch)
                .await
                .is_err(),
            "指纹不符不能自动修复或覆盖原记录"
        );
        let tr = db_meta.create_trx().unwrap();
        assert_eq!(
            tr.get(&keys[0], false).await.unwrap().unwrap().as_ref(),
            canonical_tamper.as_slice()
        );
        tr.set(&keys[0], &raw);
        tr.commit().await.unwrap();
        let no_io = fixture.drive_log();
        let mut mixed = requests.clone();
        mixed[1].destination.volume = uuid::Uuid::new_v4();
        let duplicate_source = vec![
            requests[0].clone(),
            ReclaimNodeReadRequest {
                original: requests[0].original.clone(),
                destination: requests[1].destination,
            },
        ];
        for bad in [
            Vec::new(),
            vec![requests[0].clone(); 33],
            vec![requests[0].clone(); 2],
            mixed,
            duplicate_source,
        ] {
            assert!(
                reader
                    .read_reclaim_node_metadata(&reserved, &inventory, &bad, &cache_dir, spool)
                    .await
                    .is_err()
            );
            assert_eq!(fixture.drive_log(), no_io, "非法批次不发CDB");
        }
        for uids in [
            Vec::new(),
            vec![source_uids[0]; 33],
            vec![source_uids[0]; 2],
            vec![source_uids[0], u64::MAX],
        ] {
            assert!(
                catalog
                    .reclaim_source_nodes(&reserved, fixture.owner.round, &inventory, &uids)
                    .await
                    .is_err()
            );
        }
        let mut missing_last = requests.clone();
        missing_last.last_mut().unwrap().destination.uid = u64::MAX;
        assert!(
            reader
                .read_reclaim_node_metadata(&reserved, &inventory, &missing_last, &cache_dir, spool)
                .await
                .is_err(),
            "最后一项失败不能返回前面已经核对的部分证明"
        );
        assert_eq!(
            fixture.changer_log(),
            changer_before,
            "元数据读取不移动磁带"
        );
        assert!(
            !fixture.drive_log()[before..]
                .iter()
                .any(|op| matches!(*op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d)),
            "元数据读取不写带"
        );
        let (source_uid, target_uid) = targets[0];
        let original = catalog
            .reclaim_source_nodes(&reserved, fixture.owner.round, &inventory, &[source_uid])
            .await
            .unwrap()
            .pop()
            .unwrap();
        let destination = ReclaimNodeDestination {
            volume: target_uuid,
            uid: target_uid,
        };
        let before = fixture.drive_log();
        let foreign = Permission::default();
        foreign.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let foreign_reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.device(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &foreign,
            storage: control.storage,
        };
        assert!(
            foreign_reader
                .read_reclaim_node_metadata(
                    &reserved,
                    &inventory,
                    &[ReclaimNodeReadRequest {
                        original: original.clone(),
                        destination
                    }],
                    &cache_dir,
                    spool
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.drive_log(), before, "不同许可实例不发送CDB");
        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.device(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        for bad in [
            ReclaimNodeDestination {
                volume: uuid::Uuid::nil(),
                ..destination
            },
            ReclaimNodeDestination {
                volume: spec.volume(),
                ..destination
            },
            ReclaimNodeDestination {
                uid: 0,
                ..destination
            },
            ReclaimNodeDestination {
                volume: uuid::Uuid::new_v4(),
                ..destination
            },
        ] {
            assert!(
                reader
                    .read_reclaim_node_metadata(
                        &reserved,
                        &inventory,
                        &[ReclaimNodeReadRequest {
                            original: original.clone(),
                            destination: bad
                        }],
                        &cache_dir,
                        spool
                    )
                    .await
                    .is_err()
            );
            assert_eq!(fixture.drive_log(), before);
        }
        let host_error = || Err(TapeError::ScsiHost(3));
        let boundary = MetadataReadBoundary {
            inner: fixture.device(),
            after: &host_error,
            triggered: std::sync::atomic::AtomicBool::new(false),
        };
        let faulty = super::super::super::fdb_read::ActiveMediaReader {
            device: &boundary,
            ..reader
        };
        let failed = faulty
            .read_reclaim_node_metadata(
                &reserved,
                &inventory,
                &[ReclaimNodeReadRequest {
                    original: original.clone(),
                    destination,
                }],
                &cache_dir,
                spool,
            )
            .await;
        assert!(matches!(failed, Err(TapeError::ScsiHost(3))), "{failed:?}");
        assert!(boundary.triggered.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            fixture.drive_log()[before.len()..]
                .iter()
                .filter(|op| **op == 0x08)
                .count(),
            1,
            "已生效但失败的READ不重放"
        );

        let mut node_key = b"tape-rs/catalog/22/@/".to_vec();
        node_key.extend_from_slice(control.route.route.namespace.as_bytes());
        node_key.extend_from_slice(b"Re");
        node_key.extend_from_slice(&reserved.intent()["epoch"].as_u64().unwrap().to_be_bytes());
        node_key.extend_from_slice(
            uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        node_key.extend_from_slice(&source_uid.to_be_bytes());
        let db = foundationdb::Database::new(Some(&cluster)).unwrap();
        let tr = db.create_trx().unwrap();
        let original_row = tr.get(&node_key, false).await.unwrap().unwrap().to_vec();
        let remove_source = || -> Result<()> {
            let tr = db
                .create_trx()
                .map_err(|e| TapeError::Catalog(e.to_string()))?;
            tr.clear(&node_key);
            futures_executor::block_on(tr.commit())
                .map_err(|e| TapeError::Catalog(e.to_string()))?;
            Ok(())
        };
        let boundary = MetadataReadBoundary {
            inner: fixture.device(),
            after: &remove_source,
            triggered: std::sync::atomic::AtomicBool::new(false),
        };
        let changed = super::super::super::fdb_read::ActiveMediaReader {
            device: &boundary,
            ..reader
        };
        assert!(
            changed
                .read_reclaim_node_metadata(
                    &reserved,
                    &inventory,
                    &[ReclaimNodeReadRequest {
                        original: original.clone(),
                        destination
                    }],
                    &cache_dir,
                    spool
                )
                .await
                .is_err(),
            "实读期间原源封存行消失，不能产生证明"
        );
        assert!(boundary.triggered.load(std::sync::atomic::Ordering::SeqCst));
        let tr = db.create_trx().unwrap();
        assert!(tr.get(&node_key, false).await.unwrap().is_none());
        tr.set(&node_key, &original_row);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .reclaim_source_nodes(&reserved, fixture.owner.round, &inventory, &[source_uid])
                .await
                .unwrap()
                .pop()
                .unwrap(),
            original
        );

        Box::pin(verify_node_coverage(
            &catalog,
            &reserved,
            fixture.owner.round,
            &inventory,
            &batch,
            &db_meta,
        ))
        .await;
        if execute_node {
            Box::pin(verify_node_metadata_actor(
                &catalog, &fixture, &control, &reserved, &captured, repair, relocate,
            ))
            .await;
            return;
        }
        let close_permission = || -> Result<()> {
            fixture.permit.close(fixture.owner.round);
            Ok(())
        };
        let boundary = MetadataReadBoundary {
            inner: fixture.device(),
            after: &close_permission,
            triggered: std::sync::atomic::AtomicBool::new(false),
        };
        let closed = super::super::super::fdb_read::ActiveMediaReader {
            device: &boundary,
            ..reader
        };
        assert!(
            closed
                .read_reclaim_node_metadata(
                    &reserved,
                    &inventory,
                    &[ReclaimNodeReadRequest {
                        original: original.clone(),
                        destination
                    }],
                    &cache_dir,
                    spool
                )
                .await
                .is_err(),
            "实读期间许可关闭不能继续产生证明"
        );
        assert!(boundary.triggered.load(std::sync::atomic::Ordering::SeqCst));
        let before_closed = fixture.drive_log();
        assert!(
            reader
                .read_reclaim_node_metadata(
                    &reserved,
                    &inventory,
                    &[ReclaimNodeReadRequest {
                        original: original.clone(),
                        destination
                    }],
                    &cache_dir,
                    spool
                )
                .await
                .is_err()
        );
        assert_eq!(
            fixture.drive_log(),
            before_closed,
            "已关闭的原许可不发送CDB，不能降级或重新开启同一轮次"
        );
        assert!(
            catalog
                .check_reclaim_source_unreferenced(&reserved, fixture.owner.round)
                .await
                .is_err(),
            "原始元数据实读不是逻辑引用迁移或FORMAT权限"
        );
        catalog
            .release_reserved_reclaim(&reserved, fixture.owner.round)
            .await
            .unwrap();
        let tr = db_meta.create_trx().unwrap();
        for key in &keys {
            assert!(
                tr.get(key, false).await.unwrap().is_none(),
                "reserved read-only observation cleanup只清原parent记录"
            );
        }
        assert_eq!(fixture.formats(), 2, "只有两个模拟预格式化介质");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires disposable FDB_TEST_CLUSTER; SimLibrary only, no real hardware"]
    async fn unassigned_loaded_move_waits_for_serving_and_replays_without_media_writes() {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, _pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        let serial = control.serial.clone();
        let route = control.route;
        let namespace = route.route.namespace;
        let (map, _) = elements(&fixture.owner).unwrap();
        let move_home=ManagementRequest::parse(json!({"version":1,"library":serial,
            "namespace":namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,
            "revision":null,"destination":"homeslot"}})).unwrap();
        // The canonical mechanical controller requires its fenced Serving
        // owner. Fencing and Execution alone cannot issue queued commands.
        fixture.permit.close(fixture.owner.round);
        let permission = Arc::new(Permission::default());
        permission.renew(8, Phase::Fencing, Instant::now() + Duration::from_secs(60));
        let files = fixture.owner.files.clone();
        fixture.owner = takeover(
            &Provider {
                lib: fixture.lib.clone(),
                commands: fixture.commands.clone(),
            },
            &fixture.opts,
            8,
            Some(&permission),
            files,
        )
        .unwrap();
        fixture
            .owner
            .devices
            .iter_mut()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .serial = serial.clone();
        fixture.permit = permission;
        catalog
            .claim_active_owner_epoch(&serial, fixture.owner.round)
            .await
            .unwrap();
        catalog
            .admit_management(&serial, route, fixture.owner.round, &move_home)
            .await
            .unwrap();
        fixture.owner.serving = true;
        {
            let mut current = status.lock().unwrap();
            current.id = 1;
            current.executor_ready = true;
            current.catalog_frozen_index = Some(route.frozen_index);
            current.executor = Some(super::super::super::state::Executor {
                node: 1,
                round: 8,
                term: 1,
                fenced: true,
            });
        }
        let fenced_commands = fixture.commands.lock().unwrap().clone();
        let mut observer = fdb_rescan::Observer::default();
        for phase in [Phase::Fencing, Phase::Execution] {
            fixture
                .permit
                .renew(8, phase, Instant::now() + Duration::from_secs(60));
            tokio::task::block_in_place(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                observer
                    .poll_device(
                        &mut fixture.owner,
                        &fixture.permit,
                        &control,
                        &status,
                        fixture.opts.node_id,
                        &runtime,
                    )
                    .unwrap();
            });
            assert_eq!(
                *fixture.commands.lock().unwrap(),
                fenced_commands,
                "没有 Serving 许可不能退带或移动"
            );
            assert_eq!(
                catalog
                    .management_task(&serial, route, move_home.epoch(), move_home.operation())
                    .await
                    .unwrap()
                    .unwrap()["status"],
                "queued"
            );
        }
        fixture
            .permit
            .renew(8, Phase::Serving, Instant::now() + Duration::from_secs(60));
        fixture.owner.fdb_pending_import.insert("LIFE00L8".into());
        let before = fixture.drive_log().len();
        let moved = Box::pin(execute(&mut fixture, &control, &status, &move_home)).await;
        assert_eq!(moved["status"], "succeeded", "{moved}");
        assert_eq!(
            moved["execution"]["phase"], "confirmed",
            "原持久阶段确认后才允许结束机械任务"
        );
        assert_eq!(
            cartridge(&elements(&fixture.owner).unwrap().1, "LIFE00L8")
                .unwrap()
                .address,
            map.storage_start
        );
        assert!(!fixture.owner.fdb_pending_import.contains("LIFE00L8"));
        let drive = fixture.drive_log();
        assert_eq!(drive[before..].iter().filter(|op| **op == 0x1b).count(), 1);
        assert!(
            !drive[before..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x08 | 0x0a | 0x10 | 0x8d))
        );
        let changer = fixture.changer_log();
        assert_eq!(
            Box::pin(execute(&mut fixture, &control, &status, &move_home)).await,
            moved
        );
        assert_eq!(fixture.drive_log(), drive);
        assert_eq!(fixture.changer_log(), changer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 实读 EE 数据 UID 的全部已知哈希，不访问实际设备"]
    async fn reclaim_source_read_rejects_conflicting_secondary_md5() {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{
            ContentDigest, ManagedCatalog, ManagementRequest, ReclaimSourceInventory, ReclaimSpec,
        };
        use sha2::{Digest, Sha256};
        use std::io::{Cursor, Read};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let bytes: Vec<u8> = (0..(128 * 1024 + 13)).map(|i| (i % 251) as u8).collect();
        let md5 = <[u8; 16]>::from(md5::Md5::digest(&bytes));
        let correct_md5 = md5.iter().map(|b| format!("{b:02x}")).collect::<String>();
        for (recorded_md5, sha256, valid) in [
            (Some("00".repeat(16)), true, false),
            (Some(correct_md5.clone()), true, true),
            (Some(correct_md5.to_uppercase()), false, true),
            (None, true, true),
        ] {
            let (mut fixture, control, pool, status) =
                Box::pin(native_fixture(catalog.clone())).await;
            fixture.format(uuid::Uuid::new_v4());
            let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
            volume.set_hash_policy(crate::ltfs::volume::HashPolicy { md5: false, sha256 });

            volume
                .append_file("/.LTFSEE_DATA/blob", &mut Cursor::new(&bytes))
                .unwrap();
            volume
                .add_symlink("/docs/alias", "../.LTFSEE_DATA/blob")
                .unwrap();
            volume
                .add_symlink("/docs/alias2", "../.LTFSEE_DATA/blob")
                .unwrap();
            let expected_md5 = recorded_md5.as_ref().map(|_| md5);
            if let Some(recorded) = recorded_md5 {
                volume
                    .set_node_xattr(
                        "/.LTFSEE_DATA/blob",
                        crate::ltfs::index::Xattr {
                            key: crate::ltfs::index::XATTR_MD5.into(),
                            value: recorded,
                            base64: false,
                        },
                        false,
                        false,
                    )
                    .unwrap();
            }
            volume.commit().unwrap();
            drop(volume);
            // Establish an actual SMC home through the public unassigned move;
            // direct Sim loading intentionally has no durable/source-address proof.
            let initial_home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,
            "revision":null,"destination":"homeslot"}})).unwrap();
            let homed = execute(&mut fixture, &control, &status, &initial_home).await;
            assert_eq!(homed["status"], "succeeded", "{homed}");
            let import=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,
            "operation":uuid::Uuid::new_v4().to_string(),"change":{"kind":"media-import",
            "pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
            let task = execute(&mut fixture, &control, &status, &import).await;
            assert_eq!(task["status"], "succeeded", "{task}");
            let current = catalog
                .tape_assignment(&control.serial, "LIFE00L8")
                .await
                .unwrap()
                .unwrap();
            let home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8",
            "volume":current.volume_id.unwrap().to_string(),"revision":current.revision,"destination":"homeslot"}})).unwrap();
            let homed = execute(&mut fixture, &control, &status, &home).await;
            assert_eq!(homed["status"], "succeeded", "{homed}");
            fixture
                .lib
                .insert_cartridge(SimCartridge::blank("EEDESTL8", 128 << 20), 1)
                .unwrap();
            let assign=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"EEDESTL8",
            "volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":false}})).unwrap();
            let assigned = execute(&mut fixture, &control, &status, &assign).await;
            assert_eq!(assigned["status"], "succeeded", "{assigned}");
            let assignment = catalog
                .tape_assignment(&control.serial, "LIFE00L8")
                .await
                .unwrap()
                .unwrap();
            let spec =
                ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),
            "revision":assignment.revision,"confirm_format":true,"unassign":false}))
                .unwrap();
            let reserved = catalog
                .reserve_reclaim_pool(
                    &control.serial,
                    control.route,
                    fixture.owner.round,
                    1,
                    uuid::Uuid::new_v4(),
                    &spec,
                )
                .await
                .unwrap();
            let file = loop {
                let walk = catalog
                    .step_reclaim_walk(&reserved, fixture.owner.round)
                    .await
                    .unwrap();
                if let Some(file) = walk.current() {
                    break file.clone();
                }
                assert!(!walk.complete());
            };
            assert_eq!(file.path(), "/docs/alias");
            let placement = catalog
                .volume_placement(spec.volume())
                .await
                .unwrap()
                .unwrap();
            let source_head = catalog
                .active_loaded_volume_head(
                    &control.serial,
                    spec.barcode(),
                    control.route,
                    fixture.owner.round,
                )
                .await
                .unwrap()
                .unwrap();
            fdb_loading::select_write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &placement,
                source_head.index,
            )
            .await
            .unwrap();
            let captured = capture_index_source(
                &TapeDrive::new(fixture.device()),
                &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
                &RecoveryBudget::default(),
            )
            .unwrap();
            let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
            loop {
                if catalog
                    .seal_reclaim_source_inventory_page(&reserved, fixture.owner.round, &inventory)
                    .await
                    .unwrap()
                    .complete()
                {
                    break;
                }
            }
            let before = fixture.drive_log();
            let before_changer = fixture.changer_log();
            let reader = super::super::super::fdb_read::ActiveMediaReader {
                catalog: &catalog,
                device: fixture.device(),
                serial: &control.serial,
                route: control.route,
                key: fixture.owner.key,
                permission: &fixture.permit,
                storage: control.storage,
            };
            let result = Box::pin(reader.read_reclaim_file_with_cache(
                &reserved,
                &file,
                &inventory,
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            ))
            .await;
            if valid {
                let (mut read, evidence) = result.unwrap();
                assert_eq!(
                    read.metadata.copy.content_digest,
                    if sha256 {
                        ContentDigest::Sha256(Sha256::digest(&bytes).into())
                    } else {
                        ContentDigest::Md5(md5)
                    }
                );
                assert_eq!(evidence.sha256(), <[u8; 32]>::from(Sha256::digest(&bytes)));
                assert_eq!(evidence.md5(), expected_md5);
                let mut actual = Vec::new();
                read.file.read_to_end(&mut actual).unwrap();
                assert_eq!(actual, bytes);
                let query_commands = fixture.drive_log();
                let spec = crate::managed_catalog::ActiveUploadSpec {
                    pool_id: pool.id,
                    path: file.path().as_bytes().to_vec(),
                    operation_id: file.operation(),
                    expected_version: Some(file.version()),
                    size: file.size(),
                    digest: ContentDigest::Sha256(evidence.sha256()),
                };
                let pending = catalog
                    .maintenance_status(control.route.route.namespace)
                    .await
                    .unwrap()
                    .pending;
                let child = catalog
                    .prepare_reclaim_upload(
                        &reserved,
                        fixture.owner.round,
                        "EEDESTL8",
                        &spec,
                        &evidence,
                    )
                    .await
                    .unwrap();
                let request = child.request();
                let saved = catalog
                    .reclaim_read_source(&reserved, fixture.owner.round, &request)
                    .await
                    .unwrap();
                assert_eq!(saved.byte_proof(), &evidence);
                assert_eq!(saved.byte_proof().md5(), expected_md5);
                assert_eq!(saved.source().copy(), &read.metadata.copy);
                let original_node = saved.byte_proof().source_node();
                assert_eq!(original_node.uid(), read.metadata.copy.ltfs_file_uid);
                assert_eq!(
                    catalog
                        .reclaim_source_nodes(
                            &reserved,
                            fixture.owner.round,
                            &inventory,
                            &[original_node.uid()]
                        )
                        .await
                        .unwrap(),
                    vec![original_node]
                );
                assert!(
                    catalog
                        .reclaim_read_source(&reserved, fixture.owner.round + 1, &request)
                        .await
                        .is_err()
                );
                let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
                assert_eq!(
                    reopened
                        .reclaim_read_source(&reserved, fixture.owner.round, &request)
                        .await
                        .unwrap(),
                    saved
                );
                // The original child fingerprint and reverse identity bind the
                // measured secondary hash as well as the preferred SHA.
                let mut key = b"tape-rs/catalog/22/@/".to_vec();
                key.extend_from_slice(request.namespace.as_bytes());
                key.extend_from_slice(b"Rc");
                key.extend_from_slice(&reserved.intent()["epoch"].as_u64().unwrap().to_be_bytes());
                key.extend_from_slice(
                    uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
                        .unwrap()
                        .as_bytes(),
                );
                key.extend_from_slice(request.operation_id.as_bytes());
                let db = foundationdb::Database::new(Some(&cluster)).unwrap();
                let tr = db.create_trx().unwrap();
                let original = tr.get(&key, false).await.unwrap().unwrap().to_vec();
                let mut changed: Value = serde_json::from_slice(&original).unwrap();
                let hash = if expected_md5.is_some() {
                    "md5"
                } else {
                    "sha256"
                };
                let first = changed["source_read"][hash][0].as_u64().unwrap();
                changed["source_read"][hash][0] = json!(first ^ 0x80);
                let changed = changed.to_string().into_bytes();
                tr.set(&key, &changed);
                tr.commit().await.unwrap();
                assert!(
                    reopened
                        .reclaim_read_source(&reserved, fixture.owner.round, &request)
                        .await
                        .is_err()
                );
                let tr = db.create_trx().unwrap();
                assert_eq!(
                    tr.get(&key, false).await.unwrap().unwrap().as_ref(),
                    changed.as_slice()
                );
                tr.set(&key, &original);
                tr.commit().await.unwrap();
                assert_eq!(
                    reopened
                        .reclaim_read_source(&reserved, fixture.owner.round, &request)
                        .await
                        .unwrap(),
                    saved
                );
                assert_eq!(
                    catalog
                        .cancel_unstarted_reclaim_upload(&reserved, fixture.owner.round, &request)
                        .await
                        .unwrap(),
                    crate::managed_catalog::PublishOutcome::RequestCancelled
                );
                assert_eq!(
                    catalog
                        .maintenance_status(request.namespace)
                        .await
                        .unwrap()
                        .pending,
                    pending
                );
                assert_eq!(
                    fixture.drive_log(),
                    query_commands,
                    "恢复原字节证明和取消未开写子任务均不发CDB"
                );
            } else {
                assert!(
                    result.is_err(),
                    "a valid preferred SHA must not hide conflicting raw data-node MD5"
                );
                assert!(result.err().unwrap().to_string().contains("MD5"));
            }
            assert!(
                !fixture.drive_log()[before.len()..]
                    .iter()
                    .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
            );
            assert_eq!(fixture.changer_log(), before_changer);
            assert_eq!(
                catalog
                    .reserved_reclaim_status(&reserved, fixture.owner.round)
                    .await
                    .unwrap()["children"],
                if valid { 1 } else { 0 }
            );
            if valid {
                assert!(
                    catalog
                        .release_reserved_reclaim(&reserved, fixture.owner.round)
                        .await
                        .is_err(),
                    "cancelled child does not remove original parent safety fence"
                );
            } else {
                catalog
                    .release_reserved_reclaim(&reserved, fixture.owner.round)
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim EE alias/Unknown 正文实读，不访问实际设备"]
    async fn reclaim_source_read_hashes_unknown_ee_body_and_binds_data_uid() {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{
            ContentDigest, ManagedCatalog, ManagementRequest, ReclaimSourceInventory, ReclaimSpec,
        };
        use sha2::{Digest, Sha256};
        use std::io::{Cursor, Read};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        fixture.format(uuid::Uuid::new_v4());
        let bytes: Vec<u8> = (0..(128 * 1024 + 13)).map(|i| (i % 251) as u8).collect();
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.set_hash_policy(crate::ltfs::volume::HashPolicy {
            md5: false,
            sha256: false,
        });

        volume
            .append_file("/.LTFSEE_DATA/blob", &mut Cursor::new(&bytes))
            .unwrap();
        volume
            .add_symlink("/docs/alias", "../.LTFSEE_DATA/blob")
            .unwrap();
        volume
            .add_symlink("/docs/alias2", "../.LTFSEE_DATA/blob")
            .unwrap();
        for (path, value) in [("/docs/alias", "first"), ("/docs/alias2", "second")] {
            volume
                .set_node_xattr(
                    path,
                    crate::ltfs::index::Xattr {
                        key: "reclaim.user.label".into(),
                        value: value.into(),
                        base64: false,
                    },
                    false,
                    false,
                )
                .unwrap();
            volume
                .set_node_xattr(
                    path,
                    crate::ltfs::index::Xattr {
                        key: "reclaim.user.binary".into(),
                        value: "AP9B".into(),
                        base64: true,
                    },
                    false,
                    false,
                )
                .unwrap();
            volume
                .change_node_metadata(
                    path,
                    &crate::ltfs::index::MetadataChange {
                        readonly: Some(true),
                        modify_time: Some("2020-01-02T03:04:05.000000000Z".into()),
                        access_time: Some("2021-02-03T04:05:06.000000000Z".into()),
                    },
                )
                .unwrap();
        }
        // More than one source page, without changing the two logical files
        // below: empty physical directories must remain visible to reclaim.
        for i in 0..40 {
            volume.create_directory(&format!("/empty-{i:02}")).unwrap();
        }
        volume.commit().unwrap();
        drop(volume);
        // Establish an actual SMC home through the public unassigned move;
        // direct Sim loading intentionally has no durable/source-address proof.
        let initial_home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,
            "revision":null,"destination":"homeslot"}})).unwrap();
        let homed = execute(&mut fixture, &control, &status, &initial_home).await;
        assert_eq!(homed["status"], "succeeded", "{homed}");
        let import=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,
            "operation":uuid::Uuid::new_v4().to_string(),"change":{"kind":"media-import",
            "pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        let task = execute(&mut fixture, &control, &status, &import).await;
        assert_eq!(task["status"], "succeeded", "{task}");
        let current = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8",
            "volume":current.volume_id.unwrap().to_string(),"revision":current.revision,"destination":"homeslot"}})).unwrap();
        let homed = execute(&mut fixture, &control, &status, &home).await;
        assert_eq!(homed["status"], "succeeded", "{homed}");
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("EEDESTL8", 128 << 20), 1)
            .unwrap();
        let assign=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"EEDESTL8",
            "volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":false}})).unwrap();
        let assigned = execute(&mut fixture, &control, &status, &assign).await;
        assert_eq!(assigned["status"], "succeeded", "{assigned}");
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let spec = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),
            "revision":assignment.revision,"confirm_format":true,"unassign":false}))
        .unwrap();
        let reserved = catalog
            .reserve_reclaim_pool(
                &control.serial,
                control.route,
                fixture.owner.round,
                1,
                uuid::Uuid::new_v4(),
                &spec,
            )
            .await
            .unwrap();
        let file = loop {
            let walk = catalog
                .step_reclaim_walk(&reserved, fixture.owner.round)
                .await
                .unwrap();
            if let Some(file) = walk.current() {
                break file.clone();
            }
            assert!(!walk.complete());
        };
        assert_eq!(file.path(), "/docs/alias");
        let placement = catalog
            .volume_placement(spec.volume())
            .await
            .unwrap()
            .unwrap();
        let source_head = catalog
            .active_loaded_volume_head(
                &control.serial,
                spec.barcode(),
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &placement,
            source_head.index,
        )
        .await
        .unwrap();
        let captured = capture_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
        let before_nodes = fixture.drive_log();
        let before_changer = fixture.changer_log();
        assert!(
            catalog
                .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                .await
                .is_err(),
            "unsealed inventory must not enumerate partial source nodes"
        );
        let mut sealed_pages = 0;
        loop {
            let progress = catalog
                .seal_reclaim_source_inventory_page(&reserved, fixture.owner.round, &inventory)
                .await
                .unwrap();
            sealed_pages += 1;
            if progress.complete() {
                break;
            }
            assert!(
                catalog
                    .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                    .await
                    .is_err(),
                "one sealed page is not a complete inventory"
            );
        }
        assert_eq!(sealed_pages, 2);
        assert_eq!(inventory.objects(), 46);
        assert_eq!(inventory.directories(), 43);
        assert_eq!(inventory.files(), 1);
        assert_eq!(inventory.symlinks(), 2);
        let first_nodes = catalog
            .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
            .await
            .unwrap();
        assert_eq!(first_nodes.items.len(), 32);
        let last_nodes = catalog
            .reclaim_source_nodes_page(
                &reserved,
                fixture.owner.round,
                &inventory,
                first_nodes.next.as_deref(),
                32,
            )
            .await
            .unwrap();
        assert_eq!(last_nodes.items.len(), 14);
        assert!(last_nodes.next.is_none());
        let nodes: Vec<_> = first_nodes.items.iter().chain(&last_nodes.items).collect();
        assert!(
            nodes
                .windows(2)
                .all(|pair| pair[0].path().as_bytes() < pair[1].path().as_bytes())
        );
        use crate::managed_catalog::ReclaimSourceNodeKind;
        let root_node = nodes.iter().find(|n| n.path().is_empty()).unwrap();
        assert_eq!(root_node.kind(), ReclaimSourceNodeKind::Directory);
        assert_eq!(root_node.length(), None);
        assert!(
            nodes
                .iter()
                .any(|n| n.path() == "empty-39" && n.kind() == ReclaimSourceNodeKind::Directory)
        );
        assert!(nodes.iter().any(|n| n.path() == ".LTFSEE_DATA/blob"
            && n.kind() == ReclaimSourceNodeKind::File
            && n.length() == Some(bytes.len() as u64)));
        assert!(
            nodes
                .iter()
                .any(|n| n.path() == "docs/alias2" && n.kind() == ReclaimSourceNodeKind::Symlink)
        );
        assert_eq!(
            catalog
                .reclaim_source_nodes(
                    &reserved,
                    fixture.owner.round,
                    &inventory,
                    &[root_node.uid()]
                )
                .await
                .unwrap()
                .pop()
                .unwrap(),
            **root_node
        );
        for uid in [0, u64::MAX] {
            assert!(
                catalog
                    .reclaim_source_nodes(&reserved, fixture.owner.round, &inventory, &[uid])
                    .await
                    .is_err()
            );
        }
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                .await
                .unwrap(),
            first_nodes
        );
        for limit in [0, 33, usize::MAX] {
            assert!(
                catalog
                    .reclaim_source_nodes_page(
                        &reserved,
                        fixture.owner.round,
                        &inventory,
                        None,
                        limit
                    )
                    .await
                    .is_err()
            );
        }
        let cursor = first_nodes.next.as_ref().unwrap();
        for bad in [cursor[..cursor.len() - 1].to_vec(), {
            let mut key = cursor.clone();
            key[0] ^= 1;
            key
        }] {
            assert!(
                catalog
                    .reclaim_source_nodes_page(
                        &reserved,
                        fixture.owner.round,
                        &inventory,
                        Some(&bad),
                        32
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            catalog
                .reclaim_source_nodes_page(&reserved, fixture.owner.round + 1, &inventory, None, 32)
                .await
                .is_err()
        );

        // Each persisted row must match its UID key and canonical encoding.
        // Corrupt data is diagnosed without changing the original ledger.
        let mut node_prefix = b"tape-rs/catalog/22/@/".to_vec();
        node_prefix.extend_from_slice(control.route.route.namespace.as_bytes());
        node_prefix.extend_from_slice(b"Re");
        node_prefix.extend_from_slice(&reserved.intent()["epoch"].as_u64().unwrap().to_be_bytes());
        node_prefix.extend_from_slice(
            uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        let mut node_key = node_prefix;
        node_key.extend_from_slice(&first_nodes.items[0].uid().to_be_bytes());
        let db = foundationdb::Database::new(Some(&cluster)).unwrap();
        let tr = db.create_trx().unwrap();
        let original = tr.get(&node_key, false).await.unwrap().unwrap().to_vec();
        let changed_uid = String::from_utf8(original.clone()).unwrap().replace(
            &format!("\"uid\":{}", first_nodes.items[0].uid()),
            &format!("\"uid\":{}", first_nodes.items[1].uid()),
        );
        assert_ne!(changed_uid.as_bytes(), original);
        let changed_path = String::from_utf8(original.clone())
            .unwrap()
            .replace("\"path\":\"\"", "\"path\":\"fake-root\"");
        assert_ne!(changed_path.as_bytes(), original);
        // A fully canonical same-UID replacement must not be returned as an
        // original node. Likewise a missing root must fail, not return 45 nodes.
        let tr = db.create_trx().unwrap();
        tr.clear(&node_key);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                .await
                .is_err()
        );
        let tr = db.create_trx().unwrap();
        assert!(tr.get(&node_key, false).await.unwrap().is_none());
        tr.set(&node_key, &original);
        tr.commit().await.unwrap();
        for corrupt in [
            b"{".to_vec(),
            [original.as_slice(), b" "].concat(),
            changed_uid.into_bytes(),
            changed_path.into_bytes(),
            vec![b'x'; 48_001],
        ] {
            let tr = db.create_trx().unwrap();
            tr.set(&node_key, &corrupt);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .reclaim_source_nodes(
                        &reserved,
                        fixture.owner.round,
                        &inventory,
                        &[first_nodes.items[0].uid()]
                    )
                    .await
                    .is_err()
            );
            let tr = db.create_trx().unwrap();
            assert_eq!(
                tr.get(&node_key, false).await.unwrap().unwrap().as_ref(),
                corrupt.as_slice()
            );
            tr.set(&node_key, &original);
            tr.commit().await.unwrap();
        }
        assert_eq!(
            catalog
                .reclaim_source_nodes_page(&reserved, fixture.owner.round, &inventory, None, 32)
                .await
                .unwrap(),
            first_nodes
        );
        assert_eq!(
            fixture.drive_log(),
            before_nodes,
            "source node paging and recovery issue no CDBs"
        );
        assert_eq!(fixture.changer_log(), before_changer);
        let mut next = Some(file);
        let mut previous_progress = None;
        let mut metadata_receipts = Vec::new();
        for at in 0..2 {
            let file = next.take().unwrap();
            assert_eq!(
                file.path(),
                if at == 0 {
                    "/docs/alias"
                } else {
                    "/docs/alias2"
                }
            );
            fdb_loading::select_write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                &placement,
                source_head.index,
            )
            .await
            .unwrap();
            let before = fixture.drive_log().len();
            let reader = super::super::super::fdb_read::ActiveMediaReader {
                catalog: &catalog,
                device: fixture.device(),
                serial: &control.serial,
                route: control.route,
                key: fixture.owner.key,
                permission: &fixture.permit,
                storage: control.storage,
            };
            let mut foreign_route = control.route;
            foreign_route.route.namespace = uuid::Uuid::new_v4();
            let foreign = super::super::super::fdb_read::ActiveMediaReader {
                route: foreign_route,
                ..reader
            };
            assert!(
                Box::pin(foreign.read_reclaim_file_with_cache(
                    &reserved,
                    &file,
                    &inventory,
                    &fixture.owner.files.extent_cache_dir(),
                    fixture.owner.files.spool_dir()
                ))
                .await
                .is_err()
            );
            assert_eq!(fixture.drive_log().len(), before);
            let reader = super::super::super::fdb_read::ActiveMediaReader {
                catalog: &catalog,
                device: fixture.device(),
                serial: &control.serial,
                route: control.route,
                key: fixture.owner.key,
                permission: &fixture.permit,
                storage: control.storage,
            };
            let (mut read, evidence) = Box::pin(reader.read_reclaim_file_with_cache(
                &reserved,
                &file,
                &inventory,
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            ))
            .await
            .unwrap();
            assert_eq!(read.metadata.copy.content_digest, ContentDigest::Unknown);
            let media = read.metadata.copy.media.as_ref().unwrap();
            assert!(media.ee_alias);
            assert_ne!(read.metadata.copy.ltfs_file_uid, media.logical_file_uid);
            assert_eq!(read.physical.path(), ".LTFSEE_DATA/blob");
            assert_eq!(evidence.sha256(), <[u8; 32]>::from(Sha256::digest(&bytes)));
            let mut actual = Vec::new();
            read.file.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, bytes);
            std::io::Seek::rewind(&mut read.file).unwrap();
            assert!(
                !fixture.drive_log()[before..]
                    .iter()
                    .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
            );
            let source_copy = read.metadata.copy.clone();
            let source_copy_id = read.metadata.head.copy_id;
            let upload = crate::managed_catalog::ActiveUploadSpec {
                pool_id: pool.id,
                path: file.path().as_bytes().to_vec(),
                operation_id: file.operation(),
                expected_version: Some(file.version()),
                size: file.size(),
                digest: ContentDigest::Sha256(evidence.sha256()),
            };
            let mut wrong = upload.clone();
            wrong.digest = ContentDigest::Sha256([42; 32]);
            assert!(
                catalog
                    .prepare_reclaim_upload(
                        &reserved,
                        fixture.owner.round,
                        "EEDESTL8",
                        &wrong,
                        &evidence
                    )
                    .await
                    .is_err(),
                "Unknown摘要的源也必须匹配真实源读取证明"
            );
            assert_eq!(
                catalog
                    .reserved_reclaim_status(&reserved, fixture.owner.round)
                    .await
                    .unwrap()["children"],
                at
            );
            let intent = catalog
                .prepare_reclaim_upload(
                    &reserved,
                    fixture.owner.round,
                    "EEDESTL8",
                    &upload,
                    &evidence,
                )
                .await
                .unwrap();
            Box::pin(fdb_writing::write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                intent.request(),
                read.file,
            ))
            .await
            .unwrap();
            let reader = super::super::super::fdb_read::ActiveMediaReader {
                catalog: &catalog,
                device: fixture.device(),
                serial: &control.serial,
                route: control.route,
                key: fixture.owner.key,
                permission: &fixture.permit,
                storage: control.storage,
            };
            if at == 0 {
                let mut wrong_scope = intent.request();
                wrong_scope.namespace = uuid::Uuid::new_v4();
                let before_scope = fixture.drive_log();
                assert!(
                    reader
                        .read_reclaim_metadata_with_cache(
                            &reserved,
                            &wrong_scope,
                            &inventory,
                            &fixture.owner.files.extent_cache_dir(),
                            fixture.owner.files.spool_dir()
                        )
                        .await
                        .is_err()
                );
                assert_eq!(fixture.drive_log(), before_scope);
                // A real filesystem error, not a synthetic metadata witness.
                let bad_spool = fixture.root.join("metadata-spool-is-file");
                std::fs::write(&bad_spool, b"not a directory").unwrap();
                assert!(
                    reader
                        .read_reclaim_metadata_with_cache(
                            &reserved,
                            &intent.request(),
                            &inventory,
                            &fixture.owner.files.extent_cache_dir(),
                            &bad_spool
                        )
                        .await
                        .is_err()
                );
            }
            let (mut verified, metadata_evidence) = reader
                .read_reclaim_metadata_with_cache(
                    &reserved,
                    &intent.request(),
                    &inventory,
                    &fixture.owner.files.extent_cache_dir(),
                    fixture.owner.files.spool_dir(),
                )
                .await
                .unwrap();
            let mut actual = Vec::new();
            verified.file.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, bytes);
            if at == 0 {
                let captured_target = capture_index_source(
                    &TapeDrive::new(fixture.device()),
                    &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
                    &RecoveryBudget::default(),
                )
                .unwrap();
                let recorded = crate::ltfs::recovery::capture_recorded_index_source(
                    &TapeDrive::new(fixture.device()),
                    &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
                    &captured_target.identity().unwrap(),
                    &RecoveryBudget::default(),
                )
                .unwrap();
                // Materialize only this test oracle for the existing explicit
                // negative-proof API; managed reads retain a sealed fd plan.
                let verified_legacy = crate::managed_catalog::ResolvedRead {
                    id: verified.metadata.id,
                    head: verified.metadata.head.clone(),
                    copy: verified.metadata.copy.clone(),
                    placement: verified.metadata.placement.clone(),
                    attributes: verified.metadata.attributes.clone(),
                    physical: crate::catalog::volume_extents::CachedFile {
                        path: verified.physical.path().to_owned(),
                        length: verified.physical.length(),
                        symlink: None,
                        extents: verified
                            .physical
                            .extents()
                            .collect::<crate::error::Result<Vec<_>>>()
                            .unwrap(),
                    },
                };
                let no_io = fixture.drive_log();
                assert!(
                    inventory
                        .bind_verified_metadata(
                            &reserved,
                            &intent.request(),
                            &source_copy,
                            (&verified_legacy, verified.verified_head),
                            &recorded
                        )
                        .is_err()
                );
                for case in 0..8 {
                    let mut bad = verified_legacy.clone();
                    match case {
                        0 => bad.copy.ltfs_file_uid = u64::MAX,
                        1 => bad.physical.path.push_str("-wrong"),
                        2 => bad.physical.extents[0].start_block += 1,
                        3 => bad.attributes = None,
                        4 => bad.copy.content_digest = ContentDigest::Sha256([42; 32]),
                        5 => bad.head.version += 1,
                        6 => bad.placement.namespace = uuid::Uuid::new_v4(),
                        _ => bad.physical.length += 1,
                    }
                    assert!(
                        inventory
                            .bind_verified_metadata(
                                &reserved,
                                &intent.request(),
                                &source_copy,
                                (&bad, verified.verified_head),
                                &captured_target
                            )
                            .is_err(),
                        "case {case}"
                    );
                }
                for case in 0..3 {
                    let mut bad = source_copy.clone();
                    match case {
                        0 => bad.ltfs_file_uid = u64::MAX,
                        1 => bad.media.as_mut().unwrap().logical_file_uid = bad.ltfs_file_uid,
                        _ => bad.media.as_mut().unwrap().native_symlink = true,
                    }
                    assert!(
                        inventory
                            .bind_verified_metadata(
                                &reserved,
                                &intent.request(),
                                &bad,
                                (&verified_legacy, verified.verified_head),
                                &captured_target
                            )
                            .is_err()
                    );
                }
                assert_eq!(fixture.drive_log(), no_io, "拒绝证明不触碰设备");
            }
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &file,
                    &intent.request(),
                    &verified.metadata,
                    verified.verified_head,
                )
                .await
                .unwrap();
            if at == 1 {
                let mut own_key = b"tape-rs/catalog/22/@/".to_vec();
                own_key.extend_from_slice(control.route.route.namespace.as_bytes());
                own_key.extend_from_slice(b"Rd");
                own_key.extend_from_slice(&1u64.to_be_bytes());
                own_key.extend_from_slice(
                    uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
                        .unwrap()
                        .as_bytes(),
                );
                own_key.extend_from_slice(&source_copy.ltfs_file_uid.to_be_bytes());
                own_key.extend_from_slice(intent.operation_id.as_bytes());
                let mut metadata_prefix = own_key[..own_key.len() - 24].to_vec();
                let kind = metadata_prefix.len() - 25;
                assert_eq!(metadata_prefix[kind], b'd');
                metadata_prefix[kind] = b'k';
                let metadata_progress_key = metadata_prefix.clone();
                metadata_prefix[kind] = b'h';
                let mut metadata_key = metadata_prefix;
                metadata_key.extend_from_slice(
                    &source_copy
                        .media
                        .as_ref()
                        .unwrap()
                        .logical_file_uid
                        .to_be_bytes(),
                );
                let db = foundationdb::Database::from_path(&cluster).unwrap();
                let tr = db.create_trx().unwrap();
                let previous_metadata_progress = tr
                    .get(&metadata_progress_key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_vec();
                assert!(tr.get(&metadata_key, false).await.unwrap().is_none());
                assert!(tr.get(&own_key, false).await.unwrap().is_none());
                tr.set(&own_key, b"fixture-second-destination-conflict");
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .seal_reclaim_file_metadata(
                            &reserved,
                            fixture.owner.round,
                            &file,
                            &intent.request(),
                            &metadata_evidence,
                        )
                        .await
                        .is_err()
                );
                assert_eq!(
                    catalog
                        .physical_copy(
                            control.route.route.namespace,
                            file.file_id(),
                            source_copy_id
                        )
                        .await
                        .unwrap(),
                    Some(source_copy)
                );
                let tr = db.create_trx().unwrap();
                assert_eq!(
                    tr.get(&own_key, false).await.unwrap().unwrap().as_ref(),
                    b"fixture-second-destination-conflict"
                );
                tr.clear(&own_key);
                tr.commit().await.unwrap();
                let tr = db.create_trx().unwrap();
                assert!(tr.get(&metadata_key, false).await.unwrap().is_none());
                assert_eq!(
                    tr.get(&metadata_progress_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    previous_metadata_progress
                );
                tr.set(&metadata_key, b"fixture-metadata-conflict");
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .seal_reclaim_file_metadata(
                            &reserved,
                            fixture.owner.round,
                            &file,
                            &intent.request(),
                            &metadata_evidence
                        )
                        .await
                        .is_err()
                );
                assert!(
                    catalog
                        .physical_copy(
                            control.route.route.namespace,
                            file.file_id(),
                            source_copy_id
                        )
                        .await
                        .unwrap()
                        .is_some()
                );
                let tr = db.create_trx().unwrap();
                assert!(tr.get(&own_key, false).await.unwrap().is_none());
                assert_eq!(
                    tr.get(&metadata_progress_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    previous_metadata_progress
                );
                assert_eq!(
                    tr.get(&metadata_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    b"fixture-metadata-conflict"
                );
                tr.clear(&metadata_key);
                tr.commit().await.unwrap();
            }
            let plan_target = capture_index_source(
                &TapeDrive::new(fixture.device()),
                &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
                &RecoveryBudget::default(),
            )
            .unwrap();
            let drive = fixture.drive_log();
            let changer = fixture.changer_log();
            assert!(
                catalog
                    .resume_reclaim_file_metadata(
                        &reserved,
                        fixture.owner.round,
                        &file,
                        &intent.request()
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            let metadata_progress = catalog
                .seal_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &file,
                    &intent.request(),
                    &metadata_evidence,
                )
                .await
                .unwrap();
            assert_eq!(
                (metadata_progress.files(), metadata_progress.aliases()),
                (0, at + 1)
            );
            Box::pin(catalog.seal_reclaim_alias_coverage(
                &reserved,
                fixture.owner.round,
                &file,
                &intent.request(),
            ))
            .await
            .unwrap();
            Box::pin(catalog.seal_reclaim_alias_coverage(
                &reserved,
                fixture.owner.round,
                &file,
                &intent.request(),
            ))
            .await
            .unwrap();
            assert!(
                Box::pin(catalog.step_reclaim_source_coverage(
                    &reserved,
                    fixture.owner.round,
                    &inventory
                ))
                .await
                .is_err(),
                "EE 别名已迁移不能跳过原数据对象及目录"
            );
            let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
            assert_eq!(
                observer
                    .seal_reclaim_file_metadata(
                        &reserved,
                        fixture.owner.round,
                        &file,
                        &intent.request(),
                        &metadata_evidence
                    )
                    .await
                    .unwrap(),
                metadata_progress
            );
            let progress = catalog
                .seal_reclaim_data_relocation(
                    &reserved,
                    fixture.owner.round,
                    &file,
                    &intent.request(),
                )
                .await
                .unwrap();
            assert_eq!(
                (progress.files(), progress.bytes(), progress.total_files()),
                (1, bytes.len() as u64, 1)
            );
            let input = catalog
                .reclaim_metadata_input(&reserved, fixture.owner.round, &file, &intent.request())
                .await
                .unwrap();
            assert_eq!(input.source().byte_proof(), &evidence);
            assert_eq!(
                input.source().byte_proof().source_node().uid(),
                read.metadata.copy.ltfs_file_uid
            );
            assert_eq!(
                input.destination().copy().content_digest,
                ContentDigest::Sha256(evidence.sha256())
            );
            assert_eq!(input.destination().copy().size, bytes.len() as u64);
            assert_eq!(input.destination().placement().pool_id, pool.id);
            assert_ne!(
                input.destination().copy().volume_id,
                read.metadata.copy.volume_id
            );
            let raw_plan = inventory
                .plan_reclaimed_body(&input, &plan_target, "raw-blob")
                .unwrap();
            assert_eq!(raw_plan.uid(), plan_target.header().highest_file_uid + 1);
            assert_eq!(raw_plan.path(), "raw-blob");
            assert_eq!(raw_plan.volume(), input.destination().copy().volume_id);
            assert!(
                inventory
                    .plan_reclaimed_body(&input, &captured, "raw-blob")
                    .is_err(),
                "源卷不能当作目标卷"
            );
            assert_eq!(
                observer
                    .reclaim_metadata_input(
                        &reserved,
                        fixture.owner.round,
                        &file,
                        &intent.request(),
                    )
                    .await
                    .unwrap(),
                input
            );
            metadata_receipts.push((file.clone(), intent.request(), input));
            if let Some(previous) = &previous_progress {
                assert_eq!(&progress, previous, "两个alias的同一数据UID只计数一次");
            }
            previous_progress = Some(progress);
            assert_eq!(fixture.drive_log(), drive);
            assert_eq!(fixture.changer_log(), changer);
            next = loop {
                let walk = catalog
                    .step_reclaim_walk(&reserved, fixture.owner.round)
                    .await
                    .unwrap();
                if walk.complete() {
                    break None;
                }
                if let Some(file) = walk.current() {
                    break Some(file.clone());
                }
            };
        }
        assert!(next.is_none());
        let latest_target = capture_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        assert!(
            inventory
                .plan_reclaimed_body(&metadata_receipts[0].2, &latest_target, "raw-blob")
                .is_err(),
            "后续已知卷头不能替代原实读正文的精确物理位置证明"
        );
        let drive_after = fixture.drive_log();
        let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
        let first = catalog
            .reclaim_uploads_page(&reserved, fixture.owner.round, None, 1)
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1);
        let second = catalog
            .reclaim_uploads_page(&reserved, fixture.owner.round, first.next.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next.is_none());
        assert_ne!(first.items[0].operation_id, second.items[0].operation_id);
        for (file, request, input) in metadata_receipts {
            let resumed = observer
                .resume_reclaim_file_metadata(&reserved, fixture.owner.round, &file, &request)
                .await
                .unwrap()
                .unwrap();
            assert_eq!((resumed.files(), resumed.aliases()), (0, 2));
            assert_eq!(
                observer
                    .reclaim_metadata_input(&reserved, fixture.owner.round, &file, &request,)
                    .await
                    .unwrap(),
                input,
                "later target generations retain the original actual donor proof"
            );
        }
        assert_eq!(
            fixture.drive_log(),
            drive_after,
            "后续generation提交后从持久证明恢复无CDB"
        );

        catalog
            .check_reclaim_source_unreferenced(&reserved, fixture.owner.round)
            .await
            .unwrap();
        assert!(
            catalog
                .release_reserved_reclaim(&reserved, fixture.owner.round)
                .await
                .is_err(),
            "数据迁移不等于原生alias/目录元数据完整覆盖"
        );
        assert_eq!(fixture.formats(), 2, "复制不能触发第三次FORMAT");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际Sim PR/mount恢复原Q，不访问真实设备"]
    async fn reclaim_recovered_cancel_keeps_source_after_actual_sim_media_recovery() {
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::managed_catalog::{
            ContentDigest, ManagedCatalog, ManagementRequest, ReclaimSourceInventory, ReclaimSpec,
        };
        use sha2::{Digest, Sha256};
        use std::io::{Cursor, Read};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        fixture.format(uuid::Uuid::new_v4());
        let bytes: Vec<u8> = (0..(128 * 1024 + 13)).map(|i| (i % 251) as u8).collect();
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.set_hash_policy(crate::ltfs::volume::HashPolicy {
            md5: false,
            sha256: false,
        });

        volume
            .append_file("/.LTFSEE_DATA/blob", &mut Cursor::new(&bytes))
            .unwrap();
        volume
            .add_symlink("/docs/alias", "../.LTFSEE_DATA/blob")
            .unwrap();
        volume
            .add_symlink("/docs/alias2", "../.LTFSEE_DATA/blob")
            .unwrap();
        volume.commit().unwrap();
        drop(volume);
        // Establish an actual SMC home through the public unassigned move;
        // direct Sim loading intentionally has no durable/source-address proof.
        let initial_home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,
            "revision":null,"destination":"homeslot"}})).unwrap();
        let homed = execute(&mut fixture, &control, &status, &initial_home).await;
        assert_eq!(homed["status"], "succeeded", "{homed}");
        let import=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,
            "operation":uuid::Uuid::new_v4().to_string(),"change":{"kind":"media-import",
            "pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        let task = execute(&mut fixture, &control, &status, &import).await;
        assert_eq!(task["status"], "succeeded", "{task}");
        let current = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let home=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8",
            "volume":current.volume_id.unwrap().to_string(),"revision":current.revision,"destination":"homeslot"}})).unwrap();
        let homed = execute(&mut fixture, &control, &status, &home).await;
        assert_eq!(homed["status"], "succeeded", "{homed}");
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("EEDESTL8", 128 << 20), 1)
            .unwrap();
        let assign=ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
            "change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"EEDESTL8",
            "volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":false}})).unwrap();
        let assigned = execute(&mut fixture, &control, &status, &assign).await;
        assert_eq!(assigned["status"], "succeeded", "{assigned}");
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let spec = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),
            "revision":assignment.revision,"confirm_format":true,"unassign":false}))
        .unwrap();
        let reserved = catalog
            .reserve_reclaim_pool(
                &control.serial,
                control.route,
                fixture.owner.round,
                1,
                uuid::Uuid::new_v4(),
                &spec,
            )
            .await
            .unwrap();
        let file = loop {
            let walk = catalog
                .step_reclaim_walk(&reserved, fixture.owner.round)
                .await
                .unwrap();
            if let Some(file) = walk.current() {
                break file.clone();
            }
            assert!(!walk.complete());
        };
        assert_eq!(file.path(), "/docs/alias");
        let placement = catalog
            .volume_placement(spec.volume())
            .await
            .unwrap()
            .unwrap();
        let source_head = catalog
            .active_loaded_volume_head(
                &control.serial,
                spec.barcode(),
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &placement,
            source_head.index,
        )
        .await
        .unwrap();
        let captured = capture_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
        loop {
            if catalog
                .seal_reclaim_source_inventory_page(&reserved, fixture.owner.round, &inventory)
                .await
                .unwrap()
                .complete()
            {
                break;
            }
        }
        assert_eq!(inventory.files(), 1);
        assert_eq!(inventory.symlinks(), 2);

        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.device(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let (mut read, evidence) = Box::pin(reader.read_reclaim_file_with_cache(
            &reserved,
            &file,
            &inventory,
            &fixture.owner.files.extent_cache_dir(),
            fixture.owner.files.spool_dir(),
        ))
        .await
        .unwrap();
        let mut original_body = Vec::new();
        read.file.read_to_end(&mut original_body).unwrap();
        assert_eq!(original_body, bytes);
        assert_eq!(evidence.sha256(), <[u8; 32]>::from(Sha256::digest(&bytes)));
        let source_copy = read.metadata.copy.clone();
        let source_copy_id = read.metadata.head.copy_id;
        let upload = crate::managed_catalog::ActiveUploadSpec {
            pool_id: pool.id,
            path: file.path().as_bytes().to_vec(),
            operation_id: file.operation(),
            expected_version: Some(file.version()),
            size: file.size(),
            digest: ContentDigest::Sha256(evidence.sha256()),
        };
        let intent = catalog
            .prepare_reclaim_upload(
                &reserved,
                fixture.owner.round,
                "EEDESTL8",
                &upload,
                &evidence,
            )
            .await
            .unwrap();
        let target = catalog
            .volume_placement(intent.volume_id)
            .await
            .unwrap()
            .unwrap();
        let target_head = catalog
            .active_loaded_volume_head(
                &control.serial,
                "EEDESTL8",
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &target,
            target_head.index,
        )
        .await
        .unwrap();
        assert_eq!(
            catalog.reserve_active_file(&intent).await.unwrap(),
            crate::managed_catalog::PublishOutcome::Registered
        );
        let pending = catalog
            .maintenance_status(control.route.route.namespace)
            .await
            .unwrap()
            .pending;
        assert!(
            catalog
                .cancel_unstarted_reclaim_upload(&reserved, fixture.owner.round, &intent.request())
                .await
                .is_err()
        );
        assert!(
            catalog
                .seal_reclaim_upload_cancellation(&reserved, fixture.owner.round, &intent.request())
                .await
                .is_err()
        );
        let before_recovery = fixture.drive_log();
        let formats = fixture.formats();
        let writes = fixture.writes();
        let mut mounted = LtfsVolume::mount(fixture.device()).unwrap();
        mounted.set_reservation_guard(Some(fixture.owner.key));
        // A stale FDB executor may not cancel an otherwise unchanged Q.
        assert!(
            Box::pin(super::super::super::fdb_media::recover_active_media_tail(
                &catalog,
                mounted,
                &control.serial,
                fixture.owner.round + 1
            ))
            .await
            .is_err()
        );
        assert!(
            catalog
                .active_file_attempt(&intent.request(), control.route)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog
                .maintenance_status(control.route.route.namespace)
                .await
                .unwrap()
                .pending,
            pending
        );
        let mut mounted = LtfsVolume::mount(fixture.device()).unwrap();
        mounted.set_reservation_guard(Some(fixture.owner.key));
        let recovered = Box::pin(super::super::super::fdb_media::recover_active_media_tail(
            &catalog,
            mounted,
            &control.serial,
            fixture.owner.round,
        ))
        .await
        .unwrap();
        assert_eq!(recovered.index(), target_head.index);
        drop(recovered);
        assert!(
            catalog
                .active_file_attempt(&intent.request(), control.route)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog
                .active_upload_result(&intent.request(), control.route)
                .await
                .unwrap(),
            Some(crate::managed_catalog::PublishOutcome::RequestCancelled)
        );
        let released = catalog
            .maintenance_status(control.route.route.namespace)
            .await
            .unwrap()
            .pending;
        assert_eq!(released + 1, pending);
        let original_children = catalog
            .reclaim_uploads_page(&reserved, fixture.owner.round, None, 1)
            .await
            .unwrap();
        assert_eq!(
            original_children.items,
            vec![intent.request()],
            "实际恢复清Q后仍能恢复原parent的child"
        );
        let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            observer
                .seal_reclaim_upload_cancellation(&reserved, fixture.owner.round, &intent.request())
                .await
                .unwrap(),
            crate::managed_catalog::PublishOutcome::RequestCancelled
        );
        assert_eq!(
            catalog
                .maintenance_status(control.route.route.namespace)
                .await
                .unwrap()
                .pending,
            released
        );
        assert_eq!(
            catalog
                .physical_copy(
                    control.route.route.namespace,
                    file.file_id(),
                    source_copy_id
                )
                .await
                .unwrap(),
            Some(source_copy)
        );
        assert_eq!(fixture.formats(), formats);
        assert_eq!(fixture.writes(), writes);
        assert!(
            !fixture.drive_log()[before_recovery.len()..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d)),
            "完整未写入介质的取消恢复不能FORMAT/写数据/写索引/写MAM"
        );
        assert!(
            catalog
                .release_reserved_reclaim(&reserved, fixture.owner.round)
                .await
                .is_err()
        );
    }

    async fn assigned_batch_fixture(
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
    ) -> (
        Fixture,
        FdbOwnerControl,
        crate::managed_catalog::PoolRecord,
        SharedStatus,
    ) {
        use crate::managed_catalog::ManagementRequest;
        let (mut fixture, control, pool, status) = native_fixture(catalog.clone()).await;
        let make = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,
            "namespace":control.route.route.namespace.to_string(),"epoch":1,
            "operation":uuid::Uuid::new_v4().to_string(),"change":change}))
            .unwrap()
        };
        let home = make(json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8",
            "volume":null,"revision":null,"destination":"homeslot"}));
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let volume = uuid::Uuid::new_v4();
        let assign = make(json!({"kind":"media-assign","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}));
        assert_eq!(
            execute(&mut fixture, &control, &status, &assign).await["status"],
            "succeeded"
        );
        (fixture, control, pool, status)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原序连续上传/读取的实际 owner 调用链，Sim，无真实设备"]
    async fn native_sequential_reads_reuse_verified_media_without_repositioning() {
        use crate::managed_catalog::{ActiveUploadSpec, ContentDigest, ManagedCatalog};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) =
            Box::pin(assigned_batch_fixture(catalog.clone())).await;
        let mut ordered = Vec::new();
        for (sequence, name) in ["z-first", "a-second", "m-third"].into_iter().enumerate() {
            let bytes = vec![sequence as u8 + 31; 4096];
            let spool = fixture.root.join(format!("sequential-{sequence}"));
            std::fs::write(&spool, &bytes).unwrap();
            let path = format!("/{name}");
            let spec = ActiveUploadSpec {
                pool_id: pool.id,
                path: path.as_bytes().to_vec(),
                operation_id: uuid::Uuid::new_v4(),
                expected_version: None,
                size: bytes.len() as u64,
                digest: ContentDigest::Sha256(Sha256::digest(&bytes).into()),
            };
            let intent = Box::pin(fdb_writing::prepare(
                &fixture.owner,
                &fixture.permit,
                &control,
                &spec,
                Some("LIFE00L8"),
            ))
            .await
            .unwrap();
            let results = Box::pin(fdb_writing::write_batch(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                vec![(intent.request(), std::fs::File::open(&spool).unwrap())],
            ))
            .await;
            assert_eq!(results, vec![Ok(1)]);
            ordered.push((format!("/{}{path}", pool.id), bytes));
        }
        let writes = fixture.writes();
        for (sequence, (path, expected)) in ordered.iter().enumerate() {
            let before = fixture.drive_log().len();
            let mut file = Box::pin(read_fdb(
                &mut fixture.owner,
                Some(&fixture.permit),
                &control,
                &status,
                path,
                Some(&pool.id.to_string()),
            ))
            .await
            .unwrap();
            let mut actual = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut actual).unwrap();
            assert_eq!(&actual, expected);
            if sequence > 0 {
                let commands = fixture.drive_log();
                assert_eq!(
                    commands[before..].iter().filter(|op| **op == 0x92).count(),
                    0,
                    "顺序读取相邻已提交对象不得重新挂载、往返索引分区或越过短间隔寻带"
                );
                assert!(
                    !commands[before..]
                        .iter()
                        .any(|op| matches!(op, 0x01 | 0x1b | 0x04 | 0x19 | 0x0a | 0x10 | 0x8d))
                );
            }
        }
        let before = fixture.drive_log().len();
        let mut reread = Box::pin(read_fdb(
            &mut fixture.owner,
            Some(&fixture.permit),
            &control,
            &status,
            &ordered[0].0,
            Some(&pool.id.to_string()),
        ))
        .await
        .unwrap();
        let mut actual = Vec::new();
        std::io::Read::read_to_end(&mut reread, &mut actual).unwrap();
        assert_eq!(actual, ordered[0].1);
        assert_eq!(
            fixture.drive_log()[before..]
                .iter()
                .filter(|op| **op == 0x92)
                .count(),
            1,
            "回退读取只定位目标正文，不重复访问索引分区"
        );
        assert_eq!(fixture.writes(), writes);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；长 Full 历史后的真实 owner 冷读，不接触设备"]
    async fn native_sequential_cold_read_avoids_historical_full_audit() {
        use crate::managed_catalog::ManagedCatalog;
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(assigned_batch_fixture(catalog)).await;
        let expected = b"immutable first body";
        let first = Box::pin(sequential_test_upload(
            &mut fixture,
            &control,
            &pool,
            &status,
            "/first",
            expected,
        ))
        .await;
        for number in 0..70 {
            fixture.permit.renew(
                fixture.owner.round,
                Phase::Serving,
                Instant::now() + Duration::from_secs(60),
            );
            Box::pin(sequential_test_upload(
                &mut fixture,
                &control,
                &pool,
                &status,
                &format!("/later-{number}"),
                b"later body",
            ))
            .await;
        }
        fixture.permit.renew(
            fixture.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60),
        );
        let head = control
            .catalog
            .active_loaded_volume_head(
                &control.serial,
                "LIFE00L8",
                control.route,
                fixture.owner.round,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            !head.index.incremental,
            "夹具必须为独立 Full，而非必要的增量链"
        );
        let writes = fixture.writes();
        let before = fixture.drive_log().len();
        let mut read = Box::pin(read_fdb(
            &mut fixture.owner,
            Some(&fixture.permit),
            &control,
            &status,
            &first,
            Some(&pool.id.to_string()),
        ))
        .await
        .unwrap();
        let mut actual = Vec::new();
        std::io::Read::read_to_end(&mut read, &mut actual).unwrap();
        assert_eq!(actual, expected);
        let locates = fixture.drive_log()[before..]
            .iter()
            .filter(|op| **op == 0x92)
            .count();
        assert!(
            locates < 32,
            "冷读不应逐代回扫独立 Full 历史，实际 LOCATE={locates}"
        );
        assert!(fixture.owner.fdb_read_session.is_some());
        assert_eq!(fixture.writes(), writes);
    }

    async fn sequential_test_upload(
        fixture: &mut Fixture,
        control: &FdbOwnerControl,
        pool: &crate::managed_catalog::PoolRecord,
        status: &SharedStatus,
        name: &str,
        bytes: &[u8],
    ) -> String {
        use crate::managed_catalog::{ActiveUploadSpec, ContentDigest};
        let spool = fixture
            .root
            .join(format!("seq-spool-{}", uuid::Uuid::new_v4()));
        std::fs::write(&spool, bytes).unwrap();
        let spec = ActiveUploadSpec {
            pool_id: pool.id,
            path: name.as_bytes().to_vec(),
            operation_id: uuid::Uuid::new_v4(),
            expected_version: None,
            size: bytes.len() as u64,
            digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
        };
        let intent = Box::pin(fdb_writing::prepare(
            &fixture.owner,
            &fixture.permit,
            control,
            &spec,
            Some("LIFE00L8"),
        ))
        .await
        .unwrap();
        assert_eq!(
            Box::pin(fdb_writing::write_batch(
                &mut fixture.owner,
                &fixture.permit,
                control,
                status,
                vec![(intent.request(), std::fs::File::open(spool).unwrap())],
            ))
            .await,
            vec![Ok(1)]
        );
        format!("/{}{name}", pool.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；顺序读证明的状态变更/失权/容量故障失效，Sim"]
    async fn native_sequential_read_session_invalidates_on_external_changes() {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        for case in 0..9 {
            let (mut fixture, mut control, pool, status) =
                Box::pin(assigned_batch_fixture(catalog.clone())).await;
            let bytes = b"original sequential body";
            let path = Box::pin(sequential_test_upload(
                &mut fixture,
                &control,
                &pool,
                &status,
                "/body",
                bytes,
            ))
            .await;
            let mut read = Box::pin(read_fdb(
                &mut fixture.owner,
                Some(&fixture.permit),
                &control,
                &status,
                &path,
                Some(&pool.id.to_string()),
            ))
            .await
            .unwrap();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut read, &mut body).unwrap();
            assert_eq!(body, bytes);
            assert!(fixture.owner.fdb_read_session.is_some());
            let wrong_permission = Permission::default();
            wrong_permission.renew(
                fixture.owner.round,
                Phase::Serving,
                Instant::now() + Duration::from_secs(60),
            );
            match case {
                0 => TapeDrive::new(fixture.device()).rewind().unwrap(),
                1 => {
                    Box::pin(sequential_test_upload(
                        &mut fixture,
                        &control,
                        &pool,
                        &status,
                        "/later",
                        b"new append",
                    ))
                    .await;
                }
                2 => {
                    let assignment = catalog
                        .tape_assignment(&control.serial, "LIFE00L8")
                        .await
                        .unwrap()
                        .unwrap();
                    let request = ManagementRequest::parse(json!({"version":1,"library":control.serial,
                        "namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),
                        "change":{"kind":"tape","pool":pool.id.to_string(),"barcode":"LIFE00L8",
                        "volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"patch":{"blacklisted":true}}})).unwrap();
                    catalog
                        .admit_management(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            &request,
                        )
                        .await
                        .unwrap();
                    let changed = catalog
                        .apply_management(
                            &control.serial,
                            control.route,
                            fixture.owner.round,
                            request.epoch(),
                            request.operation(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(changed["status"], "succeeded", "{changed}");
                }
                3 => {
                    catalog
                        .claim_active_owner_epoch(&control.serial, fixture.owner.round + 1)
                        .await
                        .unwrap();
                }
                4 => fixture.permit.close(fixture.owner.round),
                5 => {
                    // A separate same-initiator handle changes the actual VCR
                    // without changing this owner's local transport epoch.
                    let epoch = fixture.device().session_epoch();
                    let raw = fixture.lib.drive_as(0, 1);
                    let drive = TapeDrive::new(&raw);
                    drive.locate(1, 0, true).unwrap();
                    drive.space_to_eod().unwrap();
                    drive.write_block(b"external unindexed data").unwrap();
                    drive.write_filemark(0).unwrap();
                    assert_eq!(fixture.device().session_epoch(), epoch);
                }
                6 => control.storage.download_bytes = 0,
                7 => {}
                8 => fixture.lib.reset_drive(0),
                _ => unreachable!(),
            }
            let before = fixture.drive_log().len();
            let writes = fixture.writes();
            let permit = if case == 7 {
                &wrong_permission
            } else {
                fixture.permit.as_ref()
            };
            let result = Box::pin(read_fdb(
                &mut fixture.owner,
                Some(permit),
                &control,
                &status,
                &path,
                Some(&pool.id.to_string()),
            ))
            .await;
            if case <= 1 {
                let mut read = result.unwrap();
                let mut body = Vec::new();
                std::io::Read::read_to_end(&mut read, &mut body).unwrap();
                assert_eq!(body, bytes);
                assert!(
                    fixture.drive_log()[before..].contains(&0x92),
                    "变更后必须重新实读索引"
                );
                assert!(fixture.owner.fdb_read_session.is_some());
            } else {
                assert!(result.is_err(), "case {case} must refuse stale read proof");
                assert!(
                    fixture.owner.fdb_read_session.is_none(),
                    "case {case} must discard cached proof"
                );
                if matches!(case, 2 | 3 | 4 | 6 | 7 | 8) {
                    assert!(
                        !fixture.drive_log()[before..]
                            .iter()
                            .any(|op| matches!(op, 0x08 | 0x92)),
                        "case {case} rejects before label/index/body reads"
                    );
                }
            }
            assert_eq!(fixture.writes(), writes);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；旧 sealed S4 与新邻居按序恢复，不访问实际设备"]
    async fn native_batch_recovers_original_s4_before_preparing_new_neighbour() {
        use crate::daemon::fdb_media::{
            ActiveMediaUpload, commit_active_media_batch, establish_active_media_owner,
        };
        use crate::managed_catalog::{
            ActiveUploadSpec, ContentDigest, ManagedCatalog, PublishOutcome,
        };
        use crate::scsi::sim::{Fault, FaultAction, LogicalObject};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = assigned_batch_fixture(catalog.clone()).await;
        let mut uploads = Vec::new();
        for n in 0..2 {
            let bytes = format!("old-s4-and-new-neighbour-{n}").into_bytes();
            let spool = fixture.root.join(format!("mixed-spool-{n}"));
            std::fs::write(&spool, &bytes).unwrap();
            let spec = ActiveUploadSpec {
                pool_id: pool.id,
                path: format!("/mixed-{n}").into_bytes(),
                operation_id: uuid::Uuid::new_v4(),
                expected_version: None,
                size: bytes.len() as u64,
                digest: ContentDigest::Sha256(Sha256::digest(&bytes).into()),
            };
            let intent = fdb_writing::prepare(
                &fixture.owner,
                &fixture.permit,
                &control,
                &spec,
                Some("LIFE00L8"),
            )
            .await
            .unwrap();
            uploads.push(ActiveMediaUpload { intent, spool });
        }
        let before = catalog
            .observed_active_volume_index(&control.serial, uploads[0].intent.volume_id)
            .await
            .unwrap();
        // Same PR/permit and original intent: commit the old request's S4 but
        // lose VCI completion before FDB index observation. No invented rows.
        let raw = fixture.lib.drive_as(0, 1);
        raw.inject(Fault {
            opcode: 0x8d,
            action: FaultAction::HostError(3),
            after: 0,
            remaining: 1,
        });
        let device =
            GuardedTransport::new(Box::new(raw), fixture.permit.clone(), fixture.owner.round);
        let mounted = LtfsVolume::mount(&device).unwrap();
        let ready = Box::pin(establish_active_media_owner(
            &catalog,
            mounted,
            &control.serial,
            fixture.owner.key,
            &fixture.permit,
            control.storage.index_bytes as usize,
        ))
        .await
        .unwrap();
        let committed = Box::pin(commit_active_media_batch(
            &catalog,
            ready.into_volume(),
            uuid::Uuid::new_v4(),
            &uploads[..1],
            control.storage.index_bytes as usize,
        ))
        .await;
        assert!(matches!(&committed, Err(TapeError::CommitFailed { stage, .. }) if stage == "vci"));
        drop(committed);
        drop(device);
        assert_eq!(
            catalog
                .observed_active_volume_index(&control.serial, before.volume_uuid)
                .await
                .unwrap(),
            before
        );
        assert!(
            catalog
                .active_file_attempt(&uploads[0].intent.request(), control.route)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            catalog
                .active_file_attempt(&uploads[1].intent.request(), control.route)
                .await
                .unwrap()
                .is_none()
        );
        let requests = uploads
            .iter()
            .map(|upload| {
                (
                    upload.intent.request(),
                    std::fs::File::open(&upload.spool).unwrap(),
                )
            })
            .collect();
        let results = Box::pin(fdb_writing::write_batch(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            requests,
        ))
        .await;
        assert_eq!(results, vec![Ok(1), Ok(1)]);
        assert_eq!(
            catalog
                .observed_active_volume_index(&control.serial, before.volume_uuid)
                .await
                .unwrap()
                .generation,
            before.generation + 2,
            "old S4 recovery and new append must have distinct generations"
        );
        let cartridge = fixture.lib.cartridge("LIFE00L8").unwrap();
        for upload in uploads {
            assert_eq!(
                catalog
                    .active_upload_result(&upload.intent.request(), control.route)
                    .await
                    .unwrap(),
                Some(PublishOutcome::AlreadyApplied(1))
            );
            let bytes = std::fs::read(upload.spool).unwrap();
            assert_eq!(
                cartridge
                    .partitions
                    .iter()
                    .flat_map(|p| &p.objects)
                    .filter(
                        |object| matches!(object, LogicalObject::Record(record) if record == &bytes)
                    )
                    .count(),
                1
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；第二次追加注入池屏障错误，SimLibrary，不访问实际设备"]
    async fn native_batch_partial_append_gate_error_stops_later_requests() {
        use crate::managed_catalog::{ActiveUploadSpec, ContentDigest, ManagedCatalog};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        struct GateOnSecondAppend {
            inner: Box<dyn TapeTransport + Send>,
            pool: uuid::Uuid,
            writes: AtomicUsize,
            failed: AtomicBool,
            later_commands: Arc<AtomicUsize>,
        }
        impl GateOnSecondAppend {
            fn observe(&self, cdb: &[u8]) {
                if self.failed.load(Ordering::SeqCst) && cdb[0] != 0x5e {
                    self.later_commands.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        impl TapeTransport for GateOnSecondAppend {
            fn identity(&self) -> &str {
                self.inner.identity()
            }
            fn session_epoch(&self) -> Option<u64> {
                self.inner.session_epoch()
            }
            fn execution_guard(&self) -> Option<(u64, usize)> {
                self.inner.execution_guard()
            }
            fn execute_no_data(
                &self,
                cdb: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.observe(cdb);
                self.inner.execute_no_data(cdb, timeout)
            }
            fn execute_read(
                &self,
                cdb: &[u8],
                data: &mut [u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.observe(cdb);
                self.inner.execute_read(cdb, data, timeout)
            }
            fn execute_write(
                &self,
                cdb: &[u8],
                data: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.observe(cdb);
                if cdb[0] == 0x0a && self.writes.fetch_add(1, Ordering::SeqCst) == 1 {
                    self.failed.store(true, Ordering::SeqCst);
                    // Inject the batch kernel's gate error after one actual
                    // append; this is not a SCSI-reported PoolBusy condition.
                    return Err(TapeError::PoolBusy {
                        pool: self.pool.to_string(),
                    });
                }
                self.inner.execute_write(cdb, data, timeout)
            }
        }
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = assigned_batch_fixture(catalog.clone()).await;
        let mut requests = Vec::new();
        for n in 0..2 {
            let bytes = format!("partial-batch-body-{n}").into_bytes();
            let path = fixture.root.join(format!("partial-spool-{n}"));
            std::fs::write(&path, &bytes).unwrap();
            let spec = ActiveUploadSpec {
                pool_id: pool.id,
                path: format!("/partial-{n}").into_bytes(),
                operation_id: uuid::Uuid::new_v4(),
                expected_version: None,
                size: bytes.len() as u64,
                digest: ContentDigest::Sha256(Sha256::digest(&bytes).into()),
            };
            let intent = fdb_writing::prepare(
                &fixture.owner,
                &fixture.permit,
                &control,
                &spec,
                Some("LIFE00L8"),
            )
            .await
            .unwrap();
            requests.push((intent.request(), std::fs::File::open(path).unwrap()));
        }
        let mut later = requests[1].0.clone();
        later.operation_id = uuid::Uuid::new_v4();
        later.file_id = uuid::Uuid::new_v4();
        later.name = b"later-unavailable".to_vec();
        later.barcode = Some("MISSINGL8".into());
        requests.push((later, requests[1].1.try_clone().unwrap()));
        let originals: Vec<_> = requests
            .iter()
            .map(|(request, _)| request.clone())
            .collect();
        let later_commands = Arc::new(AtomicUsize::new(0));
        let inner = std::mem::replace(
            &mut fixture.owner.devices[0].dev,
            Box::new(fixture.lib.drive_as(0, 1)),
        );
        fixture.owner.devices[0].dev = Box::new(GateOnSecondAppend {
            inner,
            pool: pool.id,
            writes: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            later_commands: later_commands.clone(),
        });
        let results = Box::pin(fdb_writing::write_batch(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            requests,
        ))
        .await;
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(ReadError::Lost(_)))),
            "{results:?}"
        );
        assert_eq!(
            later_commands.load(Ordering::SeqCst),
            0,
            "partial append must not admit later requests"
        );
        assert!(fixture.owner.write_session.is_none());
        for request in &originals {
            assert!(
                catalog
                    .active_upload_result(request, control.route)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for request in &originals[..2] {
            assert!(
                catalog
                    .active_file_attempt(request, control.route)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            catalog
                .active_file_attempt(&originals[2], control.route)
                .await
                .unwrap()
                .is_none()
        );
        let cartridge = fixture.lib.cartridge("LIFE00L8").unwrap();
        for (body, count) in [
            (b"partial-batch-body-0".as_slice(), 1),
            (b"partial-batch-body-1".as_slice(), 0),
        ] {
            assert_eq!(cartridge.partitions.iter().flat_map(|p| &p.objects).filter(|object|
                matches!(object, crate::scsi::sim::LogicalObject::Record(record) if record == body)).count(), count);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；末条 MAM 命令故障，SimLibrary，不访问实际设备"]
    async fn maintenance_last_mam_unknown_keeps_task_loaded_without_publishing_sample() {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct LastAttributeUnknown {
            inner: Box<dyn TapeTransport + Send>,
            reads: AtomicUsize,
        }
        impl TapeTransport for LastAttributeUnknown {
            fn identity(&self) -> &str {
                self.inner.identity()
            }
            fn session_epoch(&self) -> Option<u64> {
                self.inner.session_epoch()
            }
            fn execution_guard(&self) -> Option<(u64, usize)> {
                self.inner.execution_guard()
            }
            fn execute_no_data(
                &self,
                cdb: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.inner.execute_no_data(cdb, timeout)
            }
            fn execute_read(
                &self,
                cdb: &[u8],
                data: &mut [u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                if cdb.first() == Some(&0x8c) && self.reads.fetch_add(1, Ordering::SeqCst) == 3 {
                    return Err(TapeError::ScsiHost(3));
                }
                self.inner.execute_read(cdb, data, timeout)
            }
            fn execute_write(
                &self,
                cdb: &[u8],
                data: &[u8],
                timeout: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.inner.execute_write(cdb, data, timeout)
            }
        }
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = native_fixture(catalog.clone()).await;
        let make = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,
                "operation":uuid::Uuid::new_v4().to_string(),"change":change}))
            .unwrap()
        };
        let home = make(json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8",
            "volume":null,"revision":null,"destination":"homeslot"}));
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let volume = uuid::Uuid::new_v4();
        let assign = make(json!({"kind":"media-assign","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}));
        assert_eq!(
            execute(&mut fixture, &control, &status, &assign).await["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let request = make(json!({"kind":"media-validate","pool":pool.id.to_string(),
            "barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision}));
        catalog
            .admit_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                &request,
            )
            .await
            .unwrap();
        let task = catalog
            .start_native_device_task(
                &control.serial,
                control.route,
                fixture.owner.round,
                request.epoch(),
                request.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        let plan = choose_plan(&fixture.owner, &fixture.permit, &control, &task)
            .await
            .unwrap();
        let seen = observation(None, None, None);
        for (from, to) in [
            ("ready", "planned"),
            ("planned", "load-issued"),
            ("load-issued", "loaded"),
        ] {
            checkpoint(
                &fixture.owner,
                &fixture.permit,
                &control,
                &task,
                from,
                to,
                &plan,
                &seen,
            )
            .await
            .unwrap();
        }
        // Four READ ATTRIBUTE commands: maximum/remaining on P1 then P0.
        // Only the last response is unknown; no subsequent CDB can expose
        // the latch on behalf of the caller's final proof check.
        let inner = std::mem::replace(
            &mut fixture.owner.devices[0].dev,
            Box::new(fixture.lib.drive_as(0, 1)),
        );
        fixture.owner.devices[0].dev = Box::new(LastAttributeUnknown {
            inner,
            reads: AtomicUsize::new(0),
        });
        assert!(
            catalog
                .active_tape_monitor(&control.serial, control.route, "LIFE00L8")
                .await
                .unwrap()
                .is_none()
        );
        let result = fdb_monitor::sample_for_maintenance(
            &fixture.owner,
            &fixture.permit,
            &control,
            0,
            "LIFE00L8",
            volume,
            &task,
        )
        .await;
        assert!(
            matches!(result, Err(ReadError::Lost(_))),
            "末条未知命令不能报告采样完成: {result:?}"
        );
        assert!(
            catalog
                .active_tape_monitor(&control.serial, control.route, "LIFE00L8")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog
                .native_device_status(&task, fixture.owner.round)
                .await
                .unwrap()["phase"],
            "loaded"
        );
        assert_eq!(
            catalog
                .management_task(
                    &control.serial,
                    control.route,
                    request.epoch(),
                    request.operation()
                )
                .await
                .unwrap()
                .unwrap()["status"],
            "running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实持久任务加 SimLibrary，不访问实际设备"]
    async fn lifecycle_real_fdb_and_sim_complete_assign_validate_move_unassign_and_replay() {
        Box::pin(native_lifecycle(MetadataCase::Prepared)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原正文实读/退休与元数据准入，不访问实际设备"]
    async fn reclaim_metadata_admission_retains_uncertain_writes_across_takeover() {
        Box::pin(native_lifecycle(MetadataCase::Admission)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim 索引提交与完整对象树观察，无实际设备"]
    async fn reclaim_metadata_observation_recognizes_only_the_exact_sim_commit() {
        Box::pin(native_lifecycle(MetadataCase::Observation {
            changed_root: false,
        }))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim 提交中篡改无关属性，拒绝错误覆盖"]
    async fn reclaim_metadata_observation_rejects_an_unrelated_sim_tree_change() {
        Box::pin(native_lifecycle(MetadataCase::Observation {
            changed_root: true,
        }))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；受原任务/PR/Permission 保护的 Sim 元数据执行，无实际设备"]
    async fn reclaim_metadata_actor_commits_once_and_reobserves_without_rewrite() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::Commit,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；受原任务/PR/Permission 保护的 Sim 元数据执行，无实际设备"]
    async fn reclaim_metadata_actor_observes_lost_admission_without_starting_a_write() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::LostAdmission,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；受原任务/PR/Permission 保护的 Sim 元数据执行，无实际设备"]
    async fn reclaim_metadata_actor_recognizes_dp_commit_after_ip_write_failure() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::IpFailure,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；受原任务/PR/Permission 保护的 Sim 元数据执行，无实际设备"]
    async fn reclaim_metadata_actor_retains_a_truncated_tail_without_repair_or_rewrite() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::ClosingFailure,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；WRITE 后撤回许可，Sim，无实际设备"]
    async fn reclaim_metadata_actor_stops_after_permission_loss_during_index_write() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::PermissionAfterWrite,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；最后 PR-IN 后撤权，真实内层守卫拒绝下一媒体 CDB"]
    async fn reclaim_metadata_actor_stops_when_permission_expires_after_final_pr_read() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::PermissionAfterPr,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原子发布真实 Sim 提交及原回执/历史/读取，无实际设备"]
    async fn reclaim_metadata_publication_atomically_advances_index_and_releases_only_own_claims() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::Publication,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；重新实读未开写任务后精确关闭，Sim，无实际设备"]
    async fn reclaim_metadata_unwritten_closure_requires_fresh_original_evidence() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::UnwrittenClose,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 clean DP/IP债务，无写关闭与原证据/HA，Sim"]
    async fn reclaim_metadata_unwritten_closure_preserves_verified_ip_debt_without_io() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::UnwrittenIpDebt,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；关闭后独立catalog/HA仅凭parent恢复唯一后继，Sim"]
    async fn reclaim_metadata_closed_retry_resumes_from_parent_without_cached_arguments() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::ClosedRetryContext,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；同 XML/SHA/树但 DP 记录边界改变，Sim"]
    async fn reclaim_metadata_observation_rejects_changed_original_dp_eod() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::UnwrittenFraming,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim 空 IP，保持原 Writing/占用且无设备写"]
    async fn reclaim_metadata_unwritten_closure_keeps_missing_ip_indeterminate() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::UnwrittenIpMissing,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim IP ahead，DP 未变也不能授权未写关闭"]
    async fn reclaim_metadata_unwritten_closure_rejects_ip_ahead() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::UnwrittenIpAhead,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；元数据活跃检查点缺失拒绝及原尝试历史，Sim，无实际设备"]
    async fn reclaim_metadata_active_registry_rejects_missing_indexes_and_retains_attempts() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::AttemptRegistry,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；显式多后继保留原句柄与 FDB HA，Sim，无实际设备"]
    async fn reclaim_metadata_attempt_successors_preserve_original_handles_and_ha() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::AttemptHistory,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim T2 前提/修尾/独立回执/未知结果与HA"]
    async fn reclaim_metadata_discard_intent_keeps_truncated_attempt_claims_across_ha() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::DiscardIntent,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim T1 前提/修尾/独立回执/未知结果与HA"]
    async fn reclaim_metadata_discard_intent_requires_actual_unindexed_original_tail() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::DiscardIntentT1,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；旧未写token不可跨入Discarding释放占用，Sim"]
    async fn reclaim_metadata_discard_intent_fences_an_earlier_unwritten_token() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::DiscardIntentFromUnchanged,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；Discarding 重入只读识别原正常提交，Sim"]
    async fn reclaim_metadata_discard_goal_recognizes_a_completed_original_commit() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::DiscardIntentCommitted,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原持久检查点目标、HA与IP恢复，Sim"]
    async fn reclaim_metadata_checkpoint_recovers_empty_ip_through_the_original_fdb_goal() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::CheckpointEmpty,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原持久检查点目标、HA与IP恢复，Sim"]
    async fn reclaim_metadata_checkpoint_recovers_opening_ip_through_the_original_fdb_goal() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::CheckpointOpening,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原持久检查点目标、HA与IP恢复，Sim"]
    async fn reclaim_metadata_checkpoint_recovers_truncated_ip_through_the_original_fdb_goal() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::CheckpointPrefix,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原持久检查点目标、HA与IP恢复，Sim"]
    async fn reclaim_metadata_checkpoint_recovers_committed_dp_with_incomplete_ip_through_the_original_fdb_goal()
     {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::CheckpointCommitted,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；原持久检查点目标、HA与IP恢复，Sim"]
    async fn reclaim_metadata_checkpoint_recovers_another_checkpoint_failure_through_the_original_fdb_goal()
     {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::CheckpointFault,
        )))
        .await;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MetadataExecutionCase {
        Commit,
        LostAdmission,
        IpFailure,
        ClosingFailure,
        DiscardIntent,
        DiscardIntentT1,
        DiscardIntentCommitted,
        CheckpointEmpty,
        CheckpointOpening,
        CheckpointPrefix,
        CheckpointCommitted,
        CheckpointFault,
        DiscardIntentFromUnchanged,
        PermissionAfterWrite,
        PermissionAfterPr,
        Publication,
        UnwrittenClose,
        UnwrittenIpDebt,
        ClosedRetryContext,
        UnwrittenIpMissing,
        UnwrittenFraming,
        UnwrittenIpAhead,
        AttemptRegistry,
        AttemptHistory,
    }

    impl MetadataExecutionCase {
        fn checkpoint(self) -> bool {
            matches!(
                self,
                Self::CheckpointEmpty
                    | Self::CheckpointOpening
                    | Self::CheckpointPrefix
                    | Self::CheckpointCommitted
                    | Self::CheckpointFault
            )
        }
        fn observes_admitted(self) -> bool {
            matches!(
                self,
                Self::LostAdmission
                    | Self::DiscardIntentFromUnchanged
                    | Self::UnwrittenClose
                    | Self::UnwrittenIpDebt
                    | Self::ClosedRetryContext
                    | Self::UnwrittenIpMissing
                    | Self::UnwrittenFraming
                    | Self::UnwrittenIpAhead
            ) || self.checkpoint() && self != Self::CheckpointCommitted
        }
    }

    // Construct each large async fixture phase in a separate stack frame.
    // Boxing an already constructed future inside its parent poll is too late:
    // debug builds still reserve space there for the entire temporary value.
    #[inline(never)]
    fn boxed_metadata_phase<F: std::future::Future>(
        make: impl FnOnce() -> F,
    ) -> std::pin::Pin<Box<F>> {
        Box::pin(make())
    }

    // No source UID, attempt ID, file or request enters this helper. All of
    // those facts must be rediscovered from the original parent in FDB.
    async fn closed_reclaim_context_from_parent(
        catalog: &crate::managed_catalog::ManagedCatalog,
        parent: &crate::managed_catalog::ReclaimReservation,
        owner: u64,
    ) -> (
        crate::managed_catalog::ReclaimMetadataWrite,
        crate::managed_catalog::ReclaimFile,
        crate::managed_catalog::ActiveFileRequest,
    ) {
        let saved = catalog
            .reclaim_metadata_checkpoint(parent, owner)
            .await
            .unwrap()
            .unwrap();
        assert!(saved.closed());
        assert!(
            catalog
                .pending_reclaim_metadata_write(parent, owner)
                .await
                .unwrap()
                .is_none()
        );
        if saved.owner() != owner {
            assert!(
                catalog
                    .reclaim_metadata_retry_context(parent, owner, &saved)
                    .await
                    .is_err()
            );
        }
        let claimed = catalog
            .claim_reclaim_metadata_attempt(parent, owner, &saved)
            .await
            .unwrap();
        assert!(
            catalog
                .reclaim_metadata_execution_context(parent, owner, &claimed)
                .await
                .is_err()
        );
        let (file, request) = catalog
            .reclaim_metadata_retry_context(parent, owner, &claimed)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .reclaim_metadata_checkpoint(parent, owner)
                .await
                .unwrap(),
            Some(claimed.clone())
        );
        (claimed, file, request)
    }

    fn assert_reclaim_media_unchanged(
        before: &crate::scsi::sim::SimCartridge,
        after: &crate::scsi::sim::SimCartridge,
    ) {
        assert_eq!(before.vcr, after.vcr);
        assert_eq!(before.mam, after.mam);
        assert_eq!(before.partitions.len(), after.partitions.len());
        for (before, after) in before.partitions.iter().zip(&after.partitions) {
            assert_eq!(
                before.objects, after.objects,
                "观察/关闭不能改变 IP/DP 字节或 filemarks"
            );
            assert_eq!(before.flushed, after.flushed);
            assert_eq!(before.capacity, after.capacity);
        }
    }

    fn resegment_metadata_fixture(
        fixture: &Fixture,
        drive: usize,
        barcode: &str,
        before: &crate::ltfs::recovery::CapturedIndexSource,
    ) {
        fixture.lib.with_cartridge_mut(barcode, |cartridge| {
            let dp = &mut cartridge.partitions[1];
            let block = before.construct().location.start_block as usize;
            let crate::scsi::sim::LogicalObject::Record(raw) = &mut dp.objects[block] else {
                panic!("原 DP 索引起点必须是 record")
            };
            assert!(raw.len() > 2);
            let remainder = raw.split_off(raw.len() / 2);
            dp.objects.insert(
                block + 1,
                crate::scsi::sim::LogicalObject::Record(remainder),
            );
            dp.flushed = dp.objects.len();
            cartridge.vcr += 1;
        });
        let device = fixture.owner.devices[drive].dev.as_ref();
        let after = crate::ltfs::recovery::capture_index_source(
            &TapeDrive::new(device),
            &crate::ltfs::volume::read_label_only(device).unwrap(),
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        assert_eq!(
            after.identity().unwrap(),
            before.identity().unwrap(),
            "原 XML SHA/完整索引身份不变"
        );
        assert_eq!(
            after.header().to_xml().unwrap(),
            before.header().to_xml().unwrap(),
            "原对象树头不变"
        );
        match (before.scope(), after.scope()) {
            (
                crate::ltfs::recovery::IndexSourceScope::Current {
                    dp_eod: old,
                    ip_generation: old_ip,
                    ..
                },
                crate::ltfs::recovery::IndexSourceScope::Current {
                    dp_eod: new,
                    ip_generation: new_ip,
                    ..
                },
            ) => {
                assert_eq!(new, old + 1);
                assert_eq!(new_ip, old_ip);
            }
            _ => panic!("必须实际捕获干净 Current"),
        }
    }

    fn assert_stale_metadata_staging_rejected(
        fixture: &Fixture,
        drive: usize,
        plan: &crate::managed_catalog::ReclaimMetadataPlan,
        source: &crate::ltfs::recovery::CapturedIndexSource,
        target: &crate::ltfs::recovery::CapturedIndexSource,
    ) {
        let mut volume =
            crate::ltfs::volume::LtfsVolume::mount(fixture.owner.devices[drive].dev.as_ref())
                .unwrap();
        volume.set_reservation_guard(Some(fixture.owner.key));
        assert_ne!(volume.recovery().dp.eod, plan.previous_dp_eod());
        assert!(
            plan.stage_reference(&mut volume, source, target).is_err(),
            "旧 Current 捕获不能替代重新挂载的实际追加边界"
        );
        assert!(volume.pending_files().is_empty());
    }

    fn metadata_tail_precondition(
        fixture: &Fixture,
        drive: usize,
        task: &crate::managed_catalog::ReclaimMetadataWrite,
        prepared: &crate::managed_catalog::ReclaimMetadataWrite,
        cached: &crate::ltfs::recovery::CapturedIndexSource,
        expected: crate::ltfs::recovery::TailKind,
    ) -> crate::managed_catalog::ReclaimMetadataTailEvidence {
        use crate::managed_catalog::ReclaimMetadataTailEvidence;
        let device = fixture.owner.devices[drive].dev.as_ref();
        let original = crate::ltfs::recovery::capture_recorded_index_source(
            &TapeDrive::new(device),
            &crate::ltfs::volume::read_label_only(device).unwrap(),
            &task.plan().previous_source().unwrap(),
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        let mut volume = LtfsVolume::mount_with_budget(
            device,
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        assert_eq!(volume.recovery().dp.tail, expected);
        assert!(
            volume
                .reclaim_metadata_tail_evidence(task, &original)
                .is_err(),
            "没有PR守卫不能生成前提"
        );
        volume.set_reservation_guard(Some(fixture.owner.key));
        assert!(
            volume
                .reclaim_metadata_tail_evidence(prepared, &original)
                .is_err(),
            "Prepared不是原未决写入"
        );
        assert!(
            volume.reclaim_metadata_tail_evidence(task, cached).is_err(),
            "缓存Current不能替代Recorded实读"
        );
        let current = volume.verified_recovery_index_construct().unwrap();
        let mut bad = volume.recovery().clone();
        bad.dp.tail = crate::ltfs::recovery::TailKind::Complete;
        assert!(
            ReclaimMetadataTailEvidence::from_report(
                task,
                fixture.owner.round,
                &original,
                &bad,
                current
            )
            .is_err()
        );
        let mut bad = volume.recovery().clone();
        bad.dp.eod = u64::MAX;
        assert!(
            ReclaimMetadataTailEvidence::from_report(
                task,
                fixture.owner.round,
                &original,
                &bad,
                current
            )
            .is_err()
        );
        let mut bad = volume.recovery().clone();
        bad.history_checked = false;
        assert!(
            ReclaimMetadataTailEvidence::from_report(
                task,
                fixture.owner.round,
                &original,
                &bad,
                current
            )
            .is_err()
        );
        let proof = volume
            .reclaim_metadata_tail_evidence(task, &original)
            .unwrap();
        assert!(proof.tail_eod() > task.plan().previous_dp_eod());
        proof
    }

    fn checkpoint_metadata_fixture(fixture: &Fixture, drive: usize) {
        // The ordinary data path deliberately leaves an IP checkpoint debt.
        // Build this positive fixture before preparing the metadata attempt;
        // only IP is checkpointed, and the exact DP head must stay unchanged.
        let mut volume =
            crate::ltfs::volume::LtfsVolume::mount(fixture.owner.devices[drive].dev.as_ref())
                .unwrap();
        volume.set_reservation_guard(Some(fixture.owner.key));
        let before = volume.verified_committed_index_construct().unwrap();
        assert!(
            !before.incremental && volume.needs_checkpoint(),
            "普通正文写入夹具应保留 IP 债务"
        );
        volume.commit().unwrap();
        assert_eq!(
            volume.verified_committed_index_construct().unwrap(),
            before,
            "准备前 IP checkpoint 不得推进 DP"
        );
        assert!(!volume.needs_checkpoint());
    }

    struct MetadataFault<'a> {
        inner: &'a dyn TapeTransport,
        mode: MetadataExecutionCase,
        matching: std::sync::atomic::AtomicUsize,
        fired: std::sync::atomic::AtomicBool,
        permission: &'a Permission,
        round: u64,
    }
    impl MetadataFault<'_> {
        fn before(&self, opcode: u8) -> Result<()> {
            let selected = match self.mode {
                MetadataExecutionCase::IpFailure | MetadataExecutionCase::CheckpointCommitted => {
                    Some(0x0a)
                }
                MetadataExecutionCase::ClosingFailure
                | MetadataExecutionCase::DiscardIntent
                | MetadataExecutionCase::DiscardIntentT1
                | MetadataExecutionCase::DiscardIntentCommitted => Some(0x10),
                _ => None,
            };
            if selected == Some(opcode)
                && self
                    .matching
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 1
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(TapeError::NotReady("注入元数据索引命令回执故障".into()));
            }
            Ok(())
        }
    }
    impl TapeTransport for MetadataFault<'_> {
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch()
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            self.inner.execution_guard()
        }
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.before(cdb[0])?;
            self.inner.execute_no_data(cdb, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.before(cdb[0])?;
            let result = self.inner.execute_read(cdb, data, timeout);
            if result.is_ok()
                && cdb[0] == 0x5e
                && cdb[1] & 0x1f == 1
                && self.mode == MetadataExecutionCase::PermissionAfterPr
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.permission.close(self.round);
            }
            result
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.before(cdb[0])?;
            let result = self.inner.execute_write(cdb, data, timeout);
            if result.is_ok()
                && cdb[0] == 0x0a
                && self.mode == MetadataExecutionCase::PermissionAfterWrite
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.permission.close(self.round);
            }
            result
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MetadataCase {
        Prepared,
        Admission,
        Observation { changed_root: bool },
        Execution(MetadataExecutionCase),
    }

    // Poll each owned phase only after its predecessor has returned. Keeping
    // the setup/metadata/actor frames nested exhausted the default test stack.
    type LifecyclePhase = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
    type LifecyclePhases =
        std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<LifecyclePhase>>>;

    async fn native_lifecycle(metadata_case: MetadataCase) {
        let _network = unsafe { foundationdb::boot() };
        let phases = LifecyclePhases::default();
        Box::pin(native_lifecycle_setup(metadata_case, phases.clone())).await;
        loop {
            let phase = { phases.borrow_mut().pop_front() };
            let Some(phase) = phase else {
                break;
            };
            phase.await;
        }
    }

    struct ReclaimRecoveryScenario {
        cluster: String,
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        lib: SimLibrary,
        permit: Arc<Permission>,
        commands: Commands,
        old_round: u64,
        next: u64,
        namespace: uuid::Uuid,
        route: crate::managed_catalog::ActiveLibraryRoute,
        serial: String,
        parent: crate::managed_catalog::ReclaimReservation,
        previous_owner_task: crate::managed_catalog::ReclaimMetadataWrite,
        recovered: crate::managed_catalog::ReclaimMetadataWrite,
        scanned: crate::managed_catalog::ReclaimFile,
        input_request: crate::managed_catalog::ActiveFileRequest,
        metadata_plan: crate::managed_catalog::ReclaimMetadataPlan,
        index: crate::managed_catalog::ActiveIndexIdentity,
        count: u64,
        barcode: String,
        mode: MetadataExecutionCase,
        metadata_task_key: Vec<u8>,
        metadata_active_key: Vec<u8>,
        metadata_volume_key: Vec<u8>,
        original_source: crate::ltfs::recovery::CapturedIndexSource,
    }
    impl ReclaimRecoveryScenario {
        fn run(
            self,
            phases: LifecyclePhases,
        ) -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
            Box::pin(async move {
                use crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter;
                let Self {
                    cluster,
                    catalog,
                    lib,
                    permit,
                    commands,
                    old_round,
                    next,
                    namespace,
                    route,
                    serial,
                    parent,
                    previous_owner_task,
                    recovered,
                    scanned,
                    input_request,
                    metadata_plan,
                    index,
                    count,
                    barcode,
                    mode,
                    metadata_task_key,
                    metadata_active_key,
                    metadata_volume_key,
                    original_source,
                } = self;
                let observer =
                    crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap();
                let raw_catalog = foundationdb::Database::from_path(&cluster).unwrap();
                let budget = crate::ltfs::recovery::RecoveryBudget::default();
                let checkpoint = mode.checkpoint();
                let (parent, recovered) = if checkpoint {
                    permit.renew(
                        old_round,
                        Phase::Serving,
                        Instant::now() + Duration::from_secs(60),
                    );
                    let device = GuardedTransport::new(
                        Box::new(Recorded {
                            inner: Box::new(lib.drive_as(0, 1)),
                            kind: DeviceKind::Drive,
                            commands: commands.clone(),
                        }),
                        permit.clone(),
                        old_round,
                    );
                    let actor = ReclaimMetadataWriter {
                        catalog: &catalog,
                        device: &device,
                        route,
                        key: ReservationKey::new(1, old_round),
                        permission: &permit,
                        budget: &budget,
                    };
                    let before = commands.lock().unwrap().len();
                    let prepared = actor.begin_checkpoint(&parent, &recovered).await.unwrap();
                    assert!(prepared.checkpointing());
                    assert_eq!(prepared.recovery_epoch(), 1);
                    assert_eq!(prepared.id(), recovered.id());
                    assert!(
                        !commands.lock().unwrap()[before..]
                            .iter()
                            .any(|(_, op)| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19 | 0xa5)),
                        "检查点意图准入无写命令"
                    );
                    assert_eq!(
                        observer
                            .pending_reclaim_metadata_write(&parent, old_round)
                            .await
                            .unwrap(),
                        Some(prepared.clone())
                    );
                    catalog
                        .claim_active_owner_epoch(&serial, next)
                        .await
                        .unwrap();
                    let parents = catalog
                        .claim_reclaim_reservations_page(&serial, route, next, None, 16)
                        .await
                        .unwrap();
                    let saved = observer
                        .reclaim_metadata_checkpoint(&parents.items[0], next)
                        .await
                        .unwrap()
                        .unwrap();
                    let claimed = catalog
                        .claim_reclaim_metadata_attempt(&parents.items[0], next, &saved)
                        .await
                        .unwrap();
                    assert!(claimed.checkpointing());
                    assert_eq!(claimed.recovery_epoch(), 1);
                    (parents.items[0].clone(), claimed)
                } else {
                    (parent, recovered)
                };
                use crate::daemon::fdb_reclaim_media::ReclaimMetadataRecovery;
                use crate::managed_catalog::ReclaimMetadataRecoveryKind;
                // Simulate a lost buffered tail after journaling. Recovery
                // must observe NoWrite instead of appending a Discard.
                let unwritten = mode == MetadataExecutionCase::DiscardIntentFromUnchanged
                    || checkpoint && mode != MetadataExecutionCase::CheckpointCommitted;
                let committed = mode == MetadataExecutionCase::DiscardIntentCommitted
                    || mode == MetadataExecutionCase::CheckpointCommitted;
                if mode == MetadataExecutionCase::DiscardIntentCommitted {
                    // Settle an ambiguous original closing FM. This is the
                    // original planned Full, not a recovery-created node.
                    lib.with_cartridge_mut(&barcode, |cartridge| {
                        let dp = &mut cartridge.partitions[1];
                        dp.objects.push(crate::scsi::sim::LogicalObject::Filemark);
                        dp.flushed = dp.objects.len();
                        cartridge.vcr += 1;
                    });
                }
                if mode == MetadataExecutionCase::DiscardIntentFromUnchanged {
                    lib.with_cartridge_mut(&barcode, |cartridge| {
                        cartridge.partitions[1].flushed = metadata_plan.previous_dp_eod() as usize;
                    });
                    lib.power_cut();
                }
                permit.close(old_round);
                permit.renew(
                    next,
                    Phase::Execution,
                    Instant::now() + Duration::from_secs(60),
                );
                let raw = lib.drive_as(0, 1);
                let key = ReservationKey::new(1, next);
                crate::scsi::reservation::fence(&raw, key).unwrap();
                if matches!(
                    mode,
                    MetadataExecutionCase::DiscardIntentT1 | MetadataExecutionCase::CheckpointFault
                ) {
                    raw.inject(crate::scsi::sim::Fault {
                        opcode: 0x10,
                        action: crate::scsi::sim::FaultAction::HostError(3),
                        after: 1,
                        remaining: 1,
                    });
                }
                let checked = GuardedTransport::new(
                    Box::new(Recorded {
                        inner: Box::new(raw),
                        kind: DeviceKind::Drive,
                        commands: commands.clone(),
                    }),
                    permit.clone(),
                    next,
                );
                let recovery = ReclaimMetadataWriter {
                    catalog: &catalog,
                    device: &checked,
                    route,
                    key,
                    permission: &permit,
                    budget: &budget,
                };
                assert!(
                    recovery
                        .recover(&parent, &previous_owner_task)
                        .await
                        .is_err(),
                    "旧owner句柄不能执行新owner修尾"
                );
                if matches!(
                    mode,
                    MetadataExecutionCase::DiscardIntentT1 | MetadataExecutionCase::CheckpointFault
                ) {
                    let error = recovery.recover(&parent, &recovered).await.unwrap_err();
                    assert!(
                        matches!(error,TapeError::CommitFailed { ref stage, .. } if stage == if checkpoint {"index_partition"} else {"closing_fm"})
                    );
                    assert_eq!(
                        catalog
                            .pending_reclaim_metadata_write(&parent, next)
                            .await
                            .unwrap(),
                        Some(recovered.clone())
                    );
                    assert_eq!(
                        catalog
                            .observed_active_volume_index(&serial, metadata_plan.volume())
                            .await
                            .unwrap(),
                        index
                    );
                }
                let before = commands.lock().unwrap().len();
                let first = boxed_metadata_phase(|| recovery.recover(&parent, &recovered))
                    .await
                    .unwrap();
                let ReclaimMetadataRecovery::Observed(proof) = first else {
                    panic!("first recovery must read actual media")
                };
                let expected = if unwritten {
                    ReclaimMetadataRecoveryKind::Unwritten
                } else if committed {
                    ReclaimMetadataRecoveryKind::Committed
                } else {
                    ReclaimMetadataRecoveryKind::Discarded
                };
                assert_eq!(proof.kind(), expected);
                assert!(
                    !commands.lock().unwrap()[before..]
                        .iter()
                        .any(|(_, c)| matches!(*c, 0x04 | 0x19 | 0xa5)),
                    "修尾不格式化、擦除或搬带"
                );
                // Lose the actual recovery result before FDB publication.
                // The second invocation must recognize the completed index.
                let media = lib.cartridge(&barcode).unwrap();
                let before = commands.lock().unwrap().len();
                let again = boxed_metadata_phase(|| recovery.recover(&parent, &recovered))
                    .await
                    .unwrap();
                let ReclaimMetadataRecovery::Observed(again) = again else {
                    panic!("unpublished proof must be freshly observed")
                };
                assert_eq!(again, proof);
                assert!(
                    !commands.lock().unwrap()[before..]
                        .iter()
                        .any(|(_, c)| matches!(*c, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d | 0xa5)),
                    "修尾未知结果只读观察，不重复追加"
                );
                assert_reclaim_media_unchanged(&media, &lib.cartridge(&barcode).unwrap());
                let mut receipt_key = b"tape-rs/catalog/22/@/".to_vec();
                receipt_key.extend_from_slice(namespace.as_bytes());
                receipt_key.extend_from_slice(b"RJ");
                receipt_key.extend_from_slice(recovered.id().as_bytes());
                let before = commands.lock().unwrap().len();
                let (a, b) = tokio::join!(
                    Box::pin(
                        catalog
                            .resolve_reclaim_metadata_recovery(&parent, next, &recovered, &proof)
                    ),
                    Box::pin(
                        observer
                            .resolve_reclaim_metadata_recovery(&parent, next, &recovered, &again)
                    )
                );
                let result = a.unwrap();
                assert_eq!(b.unwrap(), result);
                assert_eq!(result.kind(), expected);
                assert_eq!(result.physical_files(), count + u64::from(committed));
                assert_eq!(
                    result.current().generation,
                    index.generation + u64::from(!unwritten)
                );
                assert_eq!(
                    catalog
                        .observed_active_volume_index(&serial, metadata_plan.volume())
                        .await
                        .unwrap(),
                    result.current()
                );
                assert!(
                    catalog
                        .pending_reclaim_metadata_write(&parent, next)
                        .await
                        .unwrap()
                        .is_none()
                );
                let resolved = catalog
                    .reclaim_metadata_checkpoint(&parent, next)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(resolved.resolved() && !resolved.closed() && !resolved.completed());
                assert_eq!(
                    recovery.recover(&parent, &recovered).await.unwrap(),
                    ReclaimMetadataRecovery::Resolved(result.clone())
                );
                let mut wrong_request = input_request.clone();
                wrong_request.operation_id = uuid::Uuid::new_v4();
                assert!(
                    catalog
                        .prepare_reclaim_metadata_write(
                            &parent,
                            next,
                            &scanned,
                            &wrong_request,
                            &recovered.plan()
                        )
                        .await
                        .is_err(),
                    "不能用另一个请求重建已结清原任务"
                );
                assert_eq!(
                    commands.lock().unwrap().len(),
                    before,
                    "结清/已结清读取不执行CDB"
                );
                let tr = raw_catalog.create_trx().unwrap();
                for key in [&metadata_active_key, &metadata_volume_key] {
                    assert!(tr.get(key, false).await.unwrap().is_none());
                }
                let saved = tr.get(&receipt_key, false).await.unwrap().unwrap().to_vec();
                tr.clear(&receipt_key);
                tr.commit().await.unwrap();
                assert!(
                    observer
                        .reclaim_metadata_checkpoint(&parent, next)
                        .await
                        .is_err(),
                    "终态缺回执不能变成可重试状态"
                );
                if !unwritten {
                    assert!(
                        observer
                            .observed_active_volume_index(&serial, metadata_plan.volume())
                            .await
                            .is_err(),
                        "历史缺独立回执必须拒绝"
                    );
                }
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&receipt_key, &saved);
                tr.commit().await.unwrap();
                let tr = raw_catalog.create_trx().unwrap();
                let task_raw = tr
                    .get(&metadata_task_key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_vec();
                tr.clear(&metadata_task_key);
                tr.commit().await.unwrap();
                assert_eq!(
                    observer
                        .observed_active_volume_index(&serial, metadata_plan.volume())
                        .await
                        .unwrap(),
                    result.current(),
                    "历史不依赖可清理的原任务行"
                );
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&metadata_task_key, &task_raw);
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .retry_reclaim_metadata_write(&parent, next, &resolved)
                        .await
                        .is_err(),
                    "修尾终态不能套用旧计划Closed重试"
                );
                let newer = next + 100;
                catalog
                    .claim_active_owner_epoch(&serial, newer)
                    .await
                    .unwrap();
                let parents = catalog
                    .claim_reclaim_reservations_page(&serial, route, newer, None, 16)
                    .await
                    .unwrap();
                let claimed = catalog
                    .claim_reclaim_metadata_attempt(&parents.items[0], newer, &resolved)
                    .await
                    .unwrap();
                assert!(claimed.resolved());
                assert_eq!(claimed.owner(), newer);
                assert_eq!(
                    observer
                        .reclaim_metadata_resolution(&parents.items[0], newer, &claimed)
                        .await
                        .unwrap(),
                    Some(result.clone())
                );
                assert!(
                    catalog
                        .resolve_reclaim_metadata_recovery(
                            &parents.items[0],
                            newer,
                            &claimed,
                            &proof
                        )
                        .await
                        .is_err(),
                    "旧owner实读token不能跨接任结清"
                );
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(
                    tr.get(&receipt_key, false).await.unwrap().unwrap().as_ref(),
                    saved.as_slice(),
                    "接任不重写独立回执"
                );
                assert_eq!(
                    commands.lock().unwrap().len(),
                    before,
                    "接任终态与拒绝旧token不执行CDB"
                );
                phases.borrow_mut().push_back(
                    (ReclaimDonorScenario {
                        cluster,
                        catalog,
                        lib,
                        permit,
                        commands,
                        owner: newer,
                        namespace,
                        route,
                        serial,
                        parent: parents.items[0].clone(),
                        task: claimed,
                        file: scanned,
                        request: input_request,
                        original_source,
                        committed: result.kind() == ReclaimMetadataRecoveryKind::Committed,
                    })
                    .run(),
                );
            })
        }
    }

    struct ReclaimDonorScenario {
        cluster: String,
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        lib: SimLibrary,
        permit: Arc<Permission>,
        commands: Commands,
        owner: u64,
        namespace: uuid::Uuid,
        route: crate::managed_catalog::ActiveLibraryRoute,
        serial: String,
        parent: crate::managed_catalog::ReclaimReservation,
        task: crate::managed_catalog::ReclaimMetadataWrite,
        file: crate::managed_catalog::ReclaimFile,
        request: crate::managed_catalog::ActiveFileRequest,
        original_source: crate::ltfs::recovery::CapturedIndexSource,
        committed: bool,
    }
    impl ReclaimDonorScenario {
        fn run(self) -> std::pin::Pin<Box<impl std::future::Future<Output = ()>>> {
            Box::pin(async move {
                use crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter;
                let Self {
                    cluster,
                    catalog,
                    lib,
                    permit,
                    commands,
                    owner,
                    namespace,
                    route,
                    serial,
                    parent,
                    task,
                    file,
                    request,
                    original_source,
                    committed,
                } = self;
                let observer =
                    crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap();
                let raw_catalog = foundationdb::Database::from_path(&cluster).unwrap();
                permit.close(owner - 100);
                permit.renew(
                    owner,
                    Phase::Execution,
                    Instant::now() + Duration::from_secs(60),
                );
                let raw = lib.drive_as(0, 1);
                let key = ReservationKey::new(1, owner);
                crate::scsi::reservation::fence(&raw, key).unwrap();
                let checked = GuardedTransport::new(
                    Box::new(Recorded {
                        inner: Box::new(raw),
                        kind: DeviceKind::Drive,
                        commands: commands.clone(),
                    }),
                    permit.clone(),
                    owner,
                );
                let budget = crate::ltfs::recovery::RecoveryBudget::default();
                let mut actor = ReclaimMetadataWriter {
                    catalog: &catalog,
                    device: &checked,
                    route,
                    key,
                    permission: &permit,
                    budget: &budget,
                };
                let before = commands.lock().unwrap().len();
                if committed {
                    catalog
                        .seal_reclaim_raw_file_coverage(
                            &parent,
                            owner,
                            task.plan().source_uid(),
                            task.id(),
                        )
                        .await
                        .unwrap();
                    assert!(
                        catalog
                            .reclaim_metadata_replan_context(&parent, owner, &task)
                            .await
                            .is_err()
                    );
                    assert!(
                        actor
                            .read_donor(&parent, &file, &request, &task)
                            .await
                            .is_err()
                    );
                    assert_eq!(
                        commands.lock().unwrap().len(),
                        before,
                        "已提交元数据不能另建 donor/后继"
                    );
                    return;
                }
                assert_eq!(
                    catalog
                        .reclaim_metadata_replan_context(&parent, owner, &task)
                        .await
                        .unwrap(),
                    (file.clone(), request.clone())
                );
                let input = catalog
                    .reclaim_metadata_input(&parent, owner, &file, &request)
                    .await
                    .unwrap();
                let context = catalog
                    .reclaim_metadata_donor_context(&parent, owner, &file, &request, &task)
                    .await
                    .unwrap();
                let label = crate::ltfs::volume::read_label_only(&checked).unwrap();
                let captured = crate::ltfs::recovery::capture_index_source(
                    &TapeDrive::new(&checked),
                    &label,
                    &budget,
                )
                .unwrap();
                let node = context.require_donor(&captured).unwrap();
                let block = node.extents[0].start_block as usize;
                let barcode = input.destination().placement().barcode.clone();
                // Preserve every old FDB proof and raw index while changing a
                // body record. The uncached donor read must still detect it.
                let original =
                    lib.cartridge(&barcode).unwrap().partitions[1].objects[block].clone();
                lib.with_cartridge_mut(&barcode, |cartridge| {
                    let crate::scsi::sim::LogicalObject::Record(bytes) =
                        &mut cartridge.partitions[1].objects[block]
                    else {
                        panic!("donor record")
                    };
                    bytes[0] ^= 1;
                    cartridge.vcr += 1;
                });
                let before = commands.lock().unwrap().len();
                assert!(
                    actor
                        .read_donor(&parent, &file, &request, &task)
                        .await
                        .is_err(),
                    "保留旧摘要不能遮盖实际正文损坏"
                );
                assert!(
                    !commands.lock().unwrap()[before..]
                        .iter()
                        .any(|(_, op)| matches!(op, 0x0a | 0x10 | 0x8d | 0x04 | 0x19 | 0xa5))
                );
                lib.with_cartridge_mut(&barcode, |cartridge| {
                    cartridge.partitions[1].objects[block] = original;
                    cartridge.vcr += 1;
                });
                let donor = actor
                    .read_donor(&parent, &file, &request, &task)
                    .await
                    .unwrap();
                let inventory =
                    crate::managed_catalog::ReclaimSourceInventory::from_captured(&original_source)
                        .unwrap();
                if crate::managed_catalog::ActiveIndexIdentity::from(donor.target().construct())
                    != input.destination().index()
                {
                    assert!(
                        inventory
                            .plan_reclaimed_body(&input, donor.target(), task.plan().path())
                            .is_err(),
                        "首次计划接口不放宽到新头"
                    );
                }
                let plan = inventory
                    .plan_reclaimed_body_after_recovery(
                        &input,
                        donor.target(),
                        task.plan().path(),
                        donor.evidence(),
                    )
                    .unwrap();
                assert_eq!(plan.source_uid(), task.plan().source_uid());
                assert_eq!(
                    plan.previous_source().unwrap(),
                    donor.target().identity().unwrap()
                );
                let operation = uuid::Uuid::new_v4();
                assert!(
                    observer
                        .reclaim_metadata_successor(&parent, owner, &task, operation)
                        .await
                        .unwrap()
                        .is_none()
                );
                let wrong = inventory
                    .plan_reclaimed_body_after_recovery(
                        &input,
                        donor.target(),
                        "different-raw-request",
                        donor.evidence(),
                    )
                    .unwrap();
                assert!(
                    catalog
                        .replan_reclaim_metadata_write(
                            &parent,
                            owner,
                            &file,
                            &request,
                            &task,
                            operation,
                            &wrong,
                            donor.evidence()
                        )
                        .await
                        .is_err()
                );
                let before = commands.lock().unwrap().len();
                let (one, two) = tokio::join!(
                    catalog.replan_reclaim_metadata_write(
                        &parent,
                        owner,
                        &file,
                        &request,
                        &task,
                        operation,
                        &plan,
                        donor.evidence()
                    ),
                    observer.replan_reclaim_metadata_write(
                        &parent,
                        owner,
                        &file,
                        &request,
                        &task,
                        operation,
                        &plan,
                        donor.evidence()
                    )
                );
                let mut successor = one.unwrap();
                assert_eq!(two.unwrap(), successor);
                assert_eq!(successor.id(), operation);
                assert_eq!(successor.previous_attempt(), Some(task.id()));
                assert_eq!(successor.first_attempt(), task.first_attempt());
                assert_eq!(successor.recovery_epoch(), 0);
                assert_eq!(
                    observer
                        .reclaim_metadata_successor(&parent, owner, &task, operation)
                        .await
                        .unwrap(),
                    Some(successor.clone())
                );
                assert!(
                    catalog
                        .replan_reclaim_metadata_write(
                            &parent,
                            owner,
                            &file,
                            &request,
                            &task,
                            uuid::Uuid::new_v4(),
                            &plan,
                            donor.evidence()
                        )
                        .await
                        .is_err(),
                    "同一原尝试不得创建第二后继请求"
                );
                assert_eq!(
                    commands.lock().unwrap().len(),
                    before,
                    "登记、重放与拒绝只访问原 FDB"
                );
                assert_eq!(
                    catalog
                        .reclaim_metadata_input(&parent, owner, &file, &request)
                        .await
                        .unwrap(),
                    input,
                    "原正文/退休事实不重写"
                );
                let closed = catalog
                    .close_prepared_reclaim_metadata_write(&parent, owner, &successor)
                    .await
                    .unwrap();
                successor = catalog
                    .retry_reclaim_metadata_write(&parent, owner, &closed)
                    .await
                    .unwrap();
                let mut origin_edge = b"tape-rs/catalog/22/@/".to_vec();
                origin_edge.extend_from_slice(namespace.as_bytes());
                origin_edge.extend_from_slice(b"RV");
                origin_edge
                    .extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
                origin_edge.extend_from_slice(
                    uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                        .unwrap()
                        .as_bytes(),
                );
                origin_edge.extend_from_slice(&plan.source_uid().to_be_bytes());
                origin_edge.extend_from_slice(task.id().as_bytes());
                let tr = raw_catalog.create_trx().unwrap();
                let origin_raw = tr.get(&origin_edge, false).await.unwrap().unwrap().to_vec();
                tr.clear(&origin_edge);
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .reclaim_metadata_checkpoint(&parent, owner)
                        .await
                        .is_err(),
                    "同计划重试仍须核实最初的独立 donor 后继证明"
                );
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&origin_edge, &origin_raw);
                tr.commit().await.unwrap();
                permit.renew(
                    owner,
                    Phase::Execution,
                    Instant::now() + Duration::from_secs(60),
                );
                let execution = actor
                    .execute(&parent, &successor, Some(&original_source))
                    .await
                    .unwrap();
                let publication = catalog
                    .publish_reclaim_metadata_write(
                        &parent,
                        owner,
                        execution.task(),
                        execution.commit_evidence().unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(publication.plan(), plan);
                assert!(
                    catalog
                        .pending_reclaim_metadata_write(&parent, owner)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    catalog
                        .reclaim_metadata_input(&parent, owner, &file, &request)
                        .await
                        .unwrap(),
                    input
                );
                let mut edge = b"tape-rs/catalog/22/@/".to_vec();
                edge.extend_from_slice(namespace.as_bytes());
                edge.extend_from_slice(b"RV");
                edge.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
                edge.extend_from_slice(
                    uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                        .unwrap()
                        .as_bytes(),
                );
                edge.extend_from_slice(&plan.source_uid().to_be_bytes());
                edge.extend_from_slice(task.id().as_bytes());
                let tr = raw_catalog.create_trx().unwrap();
                let saved = tr.get(&edge, false).await.unwrap().unwrap().to_vec();
                tr.clear(&edge);
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .reclaim_metadata_checkpoint(&parent, owner)
                        .await
                        .is_err(),
                    "缺后继边不能重建或另发请求"
                );
                assert_eq!(
                    observer
                        .observed_active_volume_index(&serial, plan.volume())
                        .await
                        .unwrap(),
                    publication.observation().current(),
                    "新物理历史独立于可清理的后继边"
                );
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&edge, &saved);
                tr.commit().await.unwrap();
            })
        }
    }

    async fn native_lifecycle_setup(
        metadata_case: MetadataCase,
        recovery_scenario: LifecyclePhases,
    ) {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut fixture, control, pool, status) = Box::pin(native_fixture(catalog.clone())).await;
        let namespace = control.route.route.namespace;
        let route = control.route;
        let serial = control.serial.clone();
        let (map, _) = elements(&fixture.owner).unwrap();
        let original_home = map.storage_start;
        let make = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        let rescan = make(json!({"kind":"library-rescan"}));
        assert_eq!(
            execute(&mut fixture, &control, &status, &rescan).await["status"],
            "succeeded"
        );
        assert_eq!(fixture.formats(), 0);
        let move_home = make(json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8",
            "volume":null,"revision":null,"destination":"homeslot"}));
        let drive_before = fixture.drive_log().len();
        let moved = execute(&mut fixture, &control, &status, &move_home).await;
        assert_eq!(moved["status"], "succeeded", "{moved}");
        assert_eq!(
            cartridge(&elements(&fixture.owner).unwrap().1, "LIFE00L8")
                .unwrap()
                .address,
            original_home
        );
        let commands = fixture.drive_log();
        let movement_commands = &commands[drive_before..];
        assert_eq!(
            movement_commands.iter().filter(|op| **op == 0x1b).count(),
            1
        );
        assert!(
            !movement_commands
                .iter()
                .any(|op| matches!(op, 0x04 | 0x08 | 0x0a | 0x10 | 0x8d)),
            "未分配卷归槽不能读取索引、FORMAT、写正文、filemark 或 MAM"
        );
        let moves = fixture.changer_log();
        assert_eq!(
            execute(&mut fixture, &control, &status, &move_home).await,
            moved
        );
        assert_eq!(fixture.drive_log(), commands);
        assert_eq!(
            fixture.changer_log(),
            moves,
            "原终态重试不能重放 UNLOAD/MOVE"
        );
        let volume = uuid::Uuid::new_v4();
        let assign = make(
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}),
        );
        let receipt = execute(&mut fixture, &control, &status, &assign).await;
        assert_eq!(receipt["status"], "succeeded", "{receipt}");
        assert_eq!(fixture.formats(), 1);
        let assigned = catalog
            .tape_assignment(&serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(assigned.volume_id, Some(volume));
        assert_eq!(assigned.pool_id, pool.id);
        let writes = fixture.writes();
        assert_eq!(
            execute(&mut fixture, &control, &status, &assign).await,
            receipt
        );
        assert_eq!(fixture.formats(), 1);
        assert_eq!(fixture.writes(), writes);
        let validate = make(
            json!({"kind":"media-validate","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assigned.revision}),
        );
        let validation = execute(&mut fixture, &control, &status, &validate).await;
        assert_eq!(validation["status"], "succeeded", "{validation}");
        assert_eq!(
            validation["result"]["result"]["validation"]["payload_verified"],
            false
        );
        assert_eq!(fixture.writes(), writes);
        let offline = make(
            json!({"kind":"tape","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assigned.revision,"patch":{"online":false,"offline_message":"test offline"}}),
        );
        catalog
            .admit_management(&serial, route, fixture.owner.round, &offline)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(
                    &serial,
                    route,
                    fixture.owner.round,
                    offline.epoch(),
                    offline.operation()
                )
                .await
                .unwrap()["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let to_ie = make(
            json!({"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision,"destination":"ieslot"}),
        );
        let result = execute(&mut fixture, &control, &status, &to_ie).await;
        assert_eq!(result["status"], "succeeded", "{result}");
        assert_eq!(
            cartridge(&elements(&fixture.owner).unwrap().1, "LIFE00L8")
                .unwrap()
                .element_type,
            ElementType::ImportExport
        );
        let moves = fixture
            .changer_log()
            .iter()
            .filter(|op| **op == 0xa5)
            .count();
        assert_eq!(
            execute(&mut fixture, &control, &status, &to_ie).await,
            result
        );
        assert_eq!(
            fixture
                .changer_log()
                .iter()
                .filter(|op| **op == 0xa5)
                .count(),
            moves
        );
        let refused_online = make(
            json!({"kind":"media-online","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision}),
        );
        let refused = execute(&mut fixture, &control, &status, &refused_online).await;
        assert_eq!(refused["status"], "failed", "{refused}");
        assert_eq!(
            catalog
                .tape_assignment(&serial, "LIFE00L8")
                .await
                .unwrap()
                .unwrap(),
            assignment
        );
        let to_home = make(
            json!({"kind":"media-move","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision,"destination":"homeslot"}),
        );
        let result = execute(&mut fixture, &control, &status, &to_home).await;
        assert_eq!(result["status"], "succeeded", "{result}");
        let (_, rows) = elements(&fixture.owner).unwrap();
        let at = cartridge(&rows, "LIFE00L8").unwrap();
        assert_eq!(at.element_type, ElementType::Storage);
        assert_eq!(at.address, original_home);
        let online = make(
            json!({"kind":"media-online","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision}),
        );
        let online_result = execute(&mut fixture, &control, &status, &online).await;
        assert_eq!(online_result["status"], "succeeded", "{online_result}");
        assert_eq!(fixture.writes(), writes);
        assert_eq!(
            execute(&mut fixture, &control, &status, &online).await,
            online_result
        );
        let policy = catalog
            .active_resource_policy(&serial, route, "tape", "LIFE00L8")
            .await
            .unwrap();
        assert_eq!(policy["online"], true);
        assert_eq!(policy["offline_message"], Value::Null);
        let assignment = catalog
            .tape_assignment(&serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let unassign = make(
            json!({"kind":"media-unassign","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":volume.to_string(),"revision":assignment.revision}),
        );
        fixture.owner.fdb_reads.clear();
        let result = execute(&mut fixture, &control, &status, &unassign).await;
        assert_eq!(result["status"], "succeeded", "{result}");
        assert!(
            catalog
                .tape_assignment(&serial, "LIFE00L8")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.formats(), 1);
        assert_eq!(fixture.writes(), writes);
        assert_eq!(
            execute(&mut fixture, &control, &status, &unassign).await,
            result
        );

        // Exercise relocation through the ordinary Q/I/S4 path with one drive.
        // This is a private-FDB + in-memory-device proof, never a hardware test.
        use crate::managed_catalog::{ActiveUploadSpec, ContentDigest, ReclaimSpec};
        use sha2::{Digest, Sha256};
        use std::io::Read;
        fixture
            .lib
            .insert_cartridge(SimCartridge::blank("RECLD1L8", 128 << 20), 1)
            .unwrap();
        for (barcode, force) in [("LIFE00L8", true), ("RECLD1L8", false)] {
            let assign = make(
                json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":barcode,"volume":uuid::Uuid::new_v4().to_string(),"confirm_format":true,"force_format":force}),
            );
            let result = execute(&mut fixture, &control, &status, &assign).await;
            assert_eq!(result["status"], "succeeded", "{result}");
            let assigned = catalog
                .tape_assignment(&serial, barcode)
                .await
                .unwrap()
                .unwrap();
            let home = make(
                json!({"kind":"media-move","pool":pool.id.to_string(),"barcode":barcode,"volume":assigned.volume_id.unwrap().to_string(),"revision":assigned.revision,"destination":"homeslot"}),
            );
            assert_eq!(
                execute(&mut fixture, &control, &status, &home).await["status"],
                "succeeded"
            );
        }
        let bytes = b"The source remains readable after publishing its destination.";
        let spool = fixture.root.join("reclaim-payload");
        std::fs::write(&spool, bytes).unwrap();
        let upload = ActiveUploadSpec {
            pool_id: pool.id,
            path: b"/relocate.bin".to_vec(),
            operation_id: uuid::Uuid::new_v4(),
            expected_version: None,
            size: bytes.len() as u64,
            digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
        };
        let visibility_before_write = catalog
            .active_namespace_visibility(&serial, route)
            .await
            .unwrap();
        let first = fdb_writing::prepare(
            &fixture.owner,
            &fixture.permit,
            &control,
            &upload,
            Some("LIFE00L8"),
        )
        .await
        .unwrap();
        let version = fdb_writing::write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            first.request(),
            std::fs::File::open(&spool).unwrap(),
        )
        .await
        .unwrap();
        let visibility_after_write = catalog
            .active_namespace_visibility(&serial, route)
            .await
            .unwrap();
        assert!(visibility_after_write > visibility_before_write);
        let source = catalog
            .tape_assignment(&serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let reclaim=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":false})).unwrap();
        let reserved = catalog
            .reserve_reclaim_pool(
                &serial,
                route,
                fixture.owner.round,
                1,
                uuid::Uuid::new_v4(),
                &reclaim,
            )
            .await
            .unwrap();
        let blocked_upload = ActiveUploadSpec {
            operation_id: uuid::Uuid::new_v4(),
            ..upload.clone()
        };
        let commands_before = fixture.commands.lock().unwrap().clone();
        let blocked = fdb_writing::prepare(
            &fixture.owner,
            &fixture.permit,
            &control,
            &blocked_upload,
            None,
        )
        .await;
        assert!(
            matches!(&blocked, Err(ReadError::Busy(reason)) if reason.contains("池或带库正在执行互斥操作")),
            "普通上传须明确报告原回收占用: {blocked:?}"
        );
        assert_eq!(
            *fixture.commands.lock().unwrap(),
            commands_before,
            "已知池占用时不得访问驱动器或机械手"
        );
        let scanned = loop {
            let progress = catalog
                .step_reclaim_walk(&reserved, fixture.owner.round)
                .await
                .unwrap();
            if let Some(file) = progress.current() {
                break file.clone();
            }
            assert!(!progress.complete(), "源文件不能在扫描中丢失");
        };
        assert_eq!(scanned.path(), "/relocate.bin");
        assert_eq!(scanned.version(), version);
        assert_eq!(
            catalog
                .step_reclaim_walk(&reserved, fixture.owner.round)
                .await
                .unwrap()
                .current(),
            Some(&scanned)
        );
        let child = ActiveUploadSpec {
            operation_id: scanned.operation(),
            expected_version: Some(version),
            ..upload.clone()
        };
        let source_drive = fixture
            .owner
            .fdb_reads
            .iter()
            .find(|(_, mount)| mount.barcode == source.barcode)
            .unwrap()
            .0;
        let source_device = fixture.owner.devices[*source_drive].dev.as_ref();
        let captured = crate::ltfs::recovery::capture_index_source(
            &TapeDrive::new(source_device),
            &crate::ltfs::volume::read_label_only(source_device).unwrap(),
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        let inventory =
            crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).unwrap();
        loop {
            if catalog
                .seal_reclaim_source_inventory_page(&reserved, fixture.owner.round, &inventory)
                .await
                .unwrap()
                .complete()
            {
                break;
            }
        }
        let source_reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: source_device,
            serial: &serial,
            route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let (read_source, source_read) = source_reader
            .read_reclaim_file_with_cache(
                &reserved,
                &scanned,
                &inventory,
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            )
            .await
            .unwrap();
        assert_eq!(ContentDigest::Sha256(source_read.sha256()), child.digest);
        let mut wrong_body = child.clone();
        wrong_body.digest = ContentDigest::Sha256([42; 32]);
        assert!(
            catalog
                .prepare_reclaim_upload(
                    &reserved,
                    fixture.owner.round,
                    "RECLD1L8",
                    &wrong_body,
                    &source_read
                )
                .await
                .is_err()
        );
        let intent = catalog
            .prepare_reclaim_upload(
                &reserved,
                fixture.owner.round,
                "RECLD1L8",
                &child,
                &source_read,
            )
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .rotate_active_request_epoch(
                    namespace,
                    &serial,
                    fixture.owner.round,
                    intent.request_epoch
                )
                .await
                .unwrap(),
            crate::managed_catalog::WindowOutcome::Advanced(_)
        ));
        let original = catalog
            .reclaim_read_source(&reserved, fixture.owner.round, &intent.request())
            .await
            .unwrap();
        let logical_path = format!("/{}/relocate.bin", pool.id);
        let original_attributes = catalog
            .resolve_active_attributes_for_route(&serial, logical_path.as_bytes(), route, 1 << 20)
            .await
            .unwrap()
            .unwrap();
        let (original_copy_id, original_copy) = match &original_attributes {
            crate::managed_catalog::ResolvedPath::File { head, copy, .. } => {
                (head.copy_id, (**copy).clone())
            }
            _ => panic!("回收源必须是文件"),
        };
        let source_data_uid = original_copy.ltfs_file_uid;
        let writes = fixture.writes();
        assert!(
            fdb_writing::prepare(
                &fixture.owner,
                &fixture.permit,
                &control,
                &upload,
                Some("RECLD1L8")
            )
            .await
            .is_err()
        );
        assert_eq!(fixture.writes(), writes, "外部写被屏障拦在设备写前");
        let copied = fdb_writing::write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            intent.request(),
            read_source.file,
        )
        .await
        .unwrap();
        assert_eq!(copied, version + 1);
        let visibility_after_copy = catalog
            .active_namespace_visibility(&serial, route)
            .await
            .unwrap();
        assert!(visibility_after_copy > visibility_after_write);
        assert_eq!(
            fdb_writing::write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                intent.request(),
                std::fs::File::open(&spool).unwrap()
            )
            .await
            .unwrap(),
            copied
        );
        assert_eq!(
            catalog
                .active_namespace_visibility(&serial, route)
                .await
                .unwrap(),
            visibility_after_copy,
            "发布终态重试不推进可见修订"
        );

        assert_eq!(
            catalog
                .reclaim_read_source(&reserved, fixture.owner.round, &intent.request())
                .await
                .unwrap(),
            original,
            "发布新头不能改变封存源"
        );
        let after_attributes = catalog
            .resolve_active_attributes_for_route(&serial, logical_path.as_bytes(), route, 1 << 20)
            .await
            .unwrap()
            .unwrap();
        let attributes = |resolved: crate::managed_catalog::ResolvedPath| match resolved {
            crate::managed_catalog::ResolvedPath::File {
                attributes: Some(attrs),
                ..
            } => attrs,
            _ => panic!("file attributes"),
        };
        let before_attrs = attributes(original_attributes);
        let after_attrs = attributes(after_attributes);
        let mut before_meta = before_attrs.meta().unwrap().clone();
        let mut after_meta = after_attrs.meta().unwrap().clone();
        before_meta.file_uid = 0;
        after_meta.file_uid = 0;
        assert_eq!(before_meta, after_meta, "复制保留时间及只读属性");
        assert_eq!(before_attrs.xattrs(), after_attrs.xattrs());
        let path = logical_path;
        let mut destination = read_fdb(
            &mut fixture.owner,
            Some(&fixture.permit),
            &control,
            &status,
            &path,
            Some(&pool.id.to_string()),
        )
        .await
        .unwrap();
        let mut body = Vec::new();
        destination.read_to_end(&mut body).unwrap();
        assert_eq!(body, bytes);
        let drive = *fixture
            .owner
            .fdb_reads
            .iter()
            .find(|(_, mount)| mount.barcode == "RECLD1L8")
            .unwrap()
            .0;
        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.owner.devices[drive].dev.as_ref(),
            serial: &serial,
            route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let (verified, metadata_evidence) = reader
            .read_reclaim_metadata_with_cache(
                &reserved,
                &intent.request(),
                &inventory,
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            )
            .await
            .unwrap();
        if matches!(
            metadata_case,
            MetadataCase::Execution(
                MetadataExecutionCase::UnwrittenClose | MetadataExecutionCase::UnwrittenIpAhead
            )
        ) {
            checkpoint_metadata_fixture(&fixture, drive);
        }
        let captured_target = crate::ltfs::recovery::capture_index_source(
            &TapeDrive::new(fixture.owner.devices[drive].dev.as_ref()),
            &crate::ltfs::volume::read_label_only(fixture.owner.devices[drive].dev.as_ref())
                .unwrap(),
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        let mut wrong = verified.verified_head;
        wrong.sha256[0] ^= 1;
        assert!(
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                    &verified.metadata,
                    wrong
                )
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .step_reclaim_walk(&reserved, fixture.owner.round)
                .await
                .unwrap()
                .current(),
            Some(&scanned)
        );
        assert!(
            catalog
                .retire_reclaim_file_source(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                )
                .await
                .is_err(),
            "仅提交 S4，没有目标实读证明时不能退休源"
        );
        assert_eq!(
            catalog
                .physical_copy(route.route.namespace, scanned.file_id(), original_copy_id)
                .await
                .unwrap(),
            Some(original_copy.clone())
        );
        assert_eq!(
            catalog
                .advance_active_request_floor(
                    namespace,
                    &serial,
                    fixture.owner.round,
                    intent.request_epoch,
                    intent.request_epoch + 1
                )
                .await
                .unwrap(),
            crate::managed_catalog::WindowOutcome::RetainedReferences,
            "S4已提交、实读尚未封存时必须保留原结果代次"
        );
        assert!(
            catalog
                .seal_reclaim_data_relocation(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .is_err(),
            "没有目标实读不能封存数据迁移账本"
        );
        let raw_catalog = foundationdb::Database::from_path(&cluster).unwrap();
        let row_reservation = reserved.clone();
        let own_row = move |kind: u8| {
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(namespace.as_bytes());
            key.extend_from_slice(&[b'R', kind]);
            key.extend_from_slice(
                &row_reservation.intent()["epoch"]
                    .as_u64()
                    .unwrap()
                    .to_be_bytes(),
            );
            key.extend_from_slice(
                uuid::Uuid::parse_str(row_reservation.intent()["operation"].as_str().unwrap())
                    .unwrap()
                    .as_bytes(),
            );
            key
        };
        let mut data_body_key = own_row(b'b');
        data_body_key.extend_from_slice(&source_data_uid.to_be_bytes());
        let mut data_destination_key = own_row(b'd');
        data_destination_key.extend_from_slice(&source_data_uid.to_be_bytes());
        data_destination_key.extend_from_slice(intent.operation_id.as_bytes());
        let data_progress_key = own_row(b'g');
        let mut source_object_key = own_row(b'e');
        source_object_key.extend_from_slice(&source_data_uid.to_be_bytes());
        let mut retired_key = own_row(b'x');
        retired_key.extend_from_slice(intent.operation_id.as_bytes());
        let mut terminal_key = b"tape-rs/catalog/22/P/".to_vec();
        terminal_key.extend_from_slice(namespace.as_bytes());
        terminal_key.extend_from_slice(&intent.request_epoch.to_be_bytes());
        terminal_key.extend_from_slice(intent.operation_id.as_bytes());
        let tr = raw_catalog.create_trx().unwrap();
        let original_terminal = tr
            .get(&terminal_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.clear(&terminal_key);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                    &verified.metadata,
                    verified.verified_head
                )
                .await
                .is_err(),
            "首次验证不得凭缺失的原终态封存证明"
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert!(tr.get(&terminal_key, false).await.unwrap().is_none());
        tr.set(&terminal_key, &original_terminal);
        tr.commit().await.unwrap();
        let done = catalog
            .verify_reclaim_file_metadata(
                &reserved,
                fixture.owner.round,
                &scanned,
                &intent.request(),
                &verified.metadata,
                verified.verified_head,
            )
            .await
            .unwrap();
        let tr = raw_catalog.create_trx().unwrap();
        assert_eq!(
            tr.get(&terminal_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            original_terminal
        );
        tr.set(&terminal_key, b"mismatched-live-terminal");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                    &verified.metadata,
                    verified.verified_head
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .retire_reclaim_file_source(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .is_err(),
            "live 终态与封存不符时不能退休源"
        );
        assert!(
            catalog
                .seal_reclaim_data_relocation(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .is_err(),
            "最后的退休证明失败不能提前提交body计数"
        );
        let check = raw_catalog.create_trx().unwrap();
        for key in [&data_body_key, &data_progress_key, &retired_key] {
            assert!(check.get(key, false).await.unwrap().is_none());
        }
        let tr = raw_catalog.create_trx().unwrap();
        assert_eq!(
            tr.get(&terminal_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            b"mismatched-live-terminal"
        );
        tr.set(&terminal_key, &original_terminal);
        tr.commit().await.unwrap();
        assert_eq!(done.copied_files(), 1);
        assert_eq!(done.copied_bytes(), bytes.len() as u64);
        assert!(done.current().is_none());
        assert!(
            catalog
                .check_reclaim_source_unreferenced(&reserved, fixture.owner.round)
                .await
                .is_err(),
            "目标已读回仍不能忽略源的保留 V 引用"
        );
        assert_eq!(
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                    &verified.metadata,
                    verified.verified_head
                )
                .await
                .unwrap(),
            done,
            "验证重试不能重复计数"
        );
        assert!(
            catalog
                .retire_reclaim_file_source(
                    &reserved,
                    fixture.owner.round + 1,
                    &scanned,
                    &intent.request(),
                )
                .await
                .is_err(),
            "未领取的 owner 不能退休源"
        );
        let mut changed = intent.request();
        changed.expected_size += 1;
        assert!(
            catalog
                .retire_reclaim_file_source(&reserved, fixture.owner.round, &scanned, &changed)
                .await
                .is_err(),
            "完整原请求不符时不能退休源"
        );
        let references_before = catalog
            .copies_on_volume(reclaim.volume(), None, 100)
            .await
            .unwrap()
            .items;
        let drive_before_metadata = fixture.drive_log();
        let changer_before_metadata = fixture.changer_log();
        let assert_unretired = || async {
            assert_eq!(
                catalog
                    .physical_copy(namespace, scanned.file_id(), original_copy_id)
                    .await
                    .unwrap(),
                Some(original_copy.clone())
            );
            assert_eq!(
                catalog
                    .copies_on_volume(reclaim.volume(), None, 100)
                    .await
                    .unwrap()
                    .items,
                references_before
            );
            let tr = raw_catalog.create_trx().unwrap();
            for key in [&data_progress_key, &data_body_key, &retired_key] {
                assert!(tr.get(key, false).await.unwrap().is_none());
            }
        };
        assert!(catalog.reclaim_metadata_input(
            &reserved, fixture.owner.round, &scanned, &intent.request(),
        ).await.is_err(), "read-only input must not create a missing retirement or ledger");
        assert_unretired().await;
        // Genuine unavailable dependency, isolated from the running test FDB.
        // This port is reserved by our listener and has no FDB protocol server.
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_cluster = fixture.root.join("unavailable.cluster");
        std::fs::write(
            &unavailable_cluster,
            format!(
                "offline:{}@{}\n",
                uuid::Uuid::new_v4().simple(),
                unavailable.local_addr().unwrap()
            ),
        )
        .unwrap();
        let offline = ManagedCatalog::open(Some(unavailable_cluster.to_str().unwrap())).unwrap();
        let failure = tokio::time::timeout(
            Duration::from_secs(12),
            Box::pin(offline.seal_reclaim_data_relocation(
                &reserved,
                fixture.owner.round,
                &scanned,
                &intent.request(),
            )),
        )
        .await
        .expect("依赖不可达应在已有事务预算内返回");
        assert!(
            matches!(
                failure,
                Err(crate::managed_catalog::CatalogError::Fdb(_)
                    | crate::managed_catalog::CatalogError::Client(_))
            ),
            "{failure:?}"
        );
        let input_failure = tokio::time::timeout(
            Duration::from_secs(12),
            Box::pin(offline.reclaim_metadata_input(
                &reserved,
                fixture.owner.round,
                &scanned,
                &intent.request(),
            )),
        )
        .await
        .expect("unavailable input lookup must obey the existing transaction budget");
        assert!(matches!(
            input_failure,
            Err(crate::managed_catalog::CatalogError::Fdb(_)
                | crate::managed_catalog::CatalogError::Client(_))
        ));
        assert_unretired().await;
        let metadata_failure = tokio::time::timeout(
            Duration::from_secs(12),
            Box::pin(offline.seal_reclaim_file_metadata(
                &reserved,
                fixture.owner.round,
                &scanned,
                &intent.request(),
                &metadata_evidence,
            )),
        )
        .await
        .expect("元数据依赖不可达须有界返回");
        assert!(
            matches!(
                metadata_failure,
                Err(crate::managed_catalog::CatalogError::Fdb(_)
                    | crate::managed_catalog::CatalogError::Client(_))
            ),
            "{metadata_failure:?}"
        );
        drop(offline);
        drop(unavailable);
        assert_unretired().await;
        // Missing/corrupt manifest evidence and an orphan destination row must
        // all fail before either retirement or aggregate progress is written.
        for (key, replacement) in [
            (own_row(b'm'), None),
            (
                source_object_key.clone(),
                Some(b"fixture-corrupt-object".to_vec()),
            ),
            (
                data_destination_key.clone(),
                Some(b"fixture-conflicting-destination".to_vec()),
            ),
        ] {
            let tr = raw_catalog.create_trx().unwrap();
            let original = tr.get(&key, false).await.unwrap().map(|v| v.to_vec());
            match &replacement {
                Some(value) => tr.set(&key, value),
                None => tr.clear(&key),
            }
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .seal_reclaim_data_relocation(
                        &reserved,
                        fixture.owner.round,
                        &scanned,
                        &intent.request()
                    )
                    .await
                    .is_err()
            );
            assert_unretired().await;
            let tr = raw_catalog.create_trx().unwrap();
            assert_eq!(
                tr.get(&key, false).await.unwrap().map(|v| v.to_vec()),
                replacement
            );
            match original {
                Some(value) => tr.set(&key, &value),
                None => tr.clear(&key),
            }
            tr.commit().await.unwrap();
        }
        let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
        let relocation_request = intent.request();
        let (first, concurrent) = tokio::join!(
            Box::pin(catalog.seal_reclaim_data_relocation(
                &reserved,
                fixture.owner.round,
                &scanned,
                &relocation_request
            )),
            Box::pin(observer.seal_reclaim_data_relocation(
                &reserved,
                fixture.owner.round,
                &scanned,
                &relocation_request
            ))
        );
        let data_progress = first.unwrap();
        assert_eq!(
            concurrent.unwrap(),
            data_progress,
            "并发提交只退休和计数一次"
        );
        assert_eq!(
            (
                data_progress.files(),
                data_progress.bytes(),
                data_progress.total_files(),
                data_progress.total_bytes()
            ),
            (1, bytes.len() as u64, 1, bytes.len() as u64)
        );
        // Treat the first committed reply as undelivered and use a new client.
        let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            observer
                .seal_reclaim_data_relocation(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .unwrap(),
            data_progress
        );
        let next_phases = recovery_scenario.clone();
        // Keep the existing long fixture layout while moving phase ownership.
        #[rustfmt::skip]
        let phase = boxed_metadata_phase(move || async move {
            let recovery_scenario = next_phases;
            let inventory =
                crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).unwrap();
            let input_request = intent.request();
        let input = catalog
            .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &input_request)
            .await
            .unwrap();
        assert_eq!(input.source().byte_proof(), &source_read);
        assert_eq!(
            input.source().byte_proof().source_node().uid(),
            source_data_uid
        );
        assert_eq!(input.destination().copy(), &verified.metadata.copy);
        assert_eq!(input.destination().head(), &verified.metadata.head);
        assert_eq!(
            input.destination().placement(),
            &verified.metadata.placement
        );
        assert_eq!(input.destination().index(), verified.verified_head);
        let metadata_plan = inventory
            .plan_reclaimed_body(&input, &captured_target, "reclaimed-raw")
            .unwrap();
        assert_eq!(
            metadata_plan.uid(),
            captured_target.header().highest_file_uid + 1
        );
        assert_eq!(metadata_plan.volume(), verified.metadata.copy.volume_id);
        assert_eq!(metadata_plan.path(), "reclaimed-raw");
        for path in [
            "",
            "/absolute",
            "../escape",
            "double//slash",
            "missing/file",
            &input_request.ltfs_path().unwrap()[1..],
        ] {
            assert!(
                inventory
                    .plan_reclaimed_body(&input, &captured_target, path)
                    .is_err(),
                "{path}"
            );
        }
        assert!(
            catalog
                .prepare_reclaim_metadata_write(
                    &reserved,
                    fixture.owner.round + 1,
                    &scanned,
                    &input_request,
                    &metadata_plan
                )
                .await
                .is_err()
        );
        let mut latest_metadata_key = own_row(b'T');
        latest_metadata_key.extend_from_slice(&source_data_uid.to_be_bytes());
        let mut metadata_task_prefix = own_row(b'W');
        metadata_task_prefix.extend_from_slice(&source_data_uid.to_be_bytes());
        let metadata_header_key = own_row(b'Z');
        let metadata_active_key = own_row(b'A');
        let mut metadata_volume_key = b"tape-rs/catalog/22/@/".to_vec();
        metadata_volume_key.extend_from_slice(uuid::Uuid::nil().as_bytes());
        metadata_volume_key.extend_from_slice(b"Qv");
        metadata_volume_key.extend_from_slice(metadata_plan.volume().as_bytes());
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_path = fixture.root.join("metadata-unavailable.cluster");
        std::fs::write(
            &unavailable_path,
            format!(
                "offline:{}@{}\n",
                uuid::Uuid::new_v4().simple(),
                unavailable.local_addr().unwrap()
            ),
        )
        .unwrap();
        let offline = ManagedCatalog::open(Some(unavailable_path.to_str().unwrap())).unwrap();
        let unavailable_result = tokio::time::timeout(
            Duration::from_secs(12),
            Box::pin(offline.prepare_reclaim_metadata_write(
                &reserved,
                fixture.owner.round,
                &scanned,
                &input_request,
                &metadata_plan,
            )),
        )
        .await
        .expect("准备任务的 FDB 依赖故障须有界返回");
        assert!(matches!(
            unavailable_result,
            Err(crate::managed_catalog::CatalogError::Fdb(_)
                | crate::managed_catalog::CatalogError::Client(_))
        ));
        drop(offline);
        drop(unavailable);
        let mut pool_key = b"tape-rs/catalog/22/T/".to_vec();
        pool_key.extend_from_slice(namespace.as_bytes());
        pool_key.extend_from_slice(pool.id.as_bytes());
        let tr = raw_catalog.create_trx().unwrap();
        let pool_bytes = tr.get(&pool_key, false).await.unwrap().unwrap().to_vec();
        let mut full_pool = pool_bytes.clone();
        full_pool[9..17].copy_from_slice(&1u64.to_be_bytes());
        tr.set(&pool_key, &full_pool);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .prepare_reclaim_metadata_write(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &input_request,
                    &metadata_plan
                )
                .await
                .is_err(),
            "正文复用也必须占用物理文件名额"
        );
        let tr = raw_catalog.create_trx().unwrap();
        for key in [
            &latest_metadata_key,
            &metadata_header_key,
            &metadata_active_key,
            &metadata_volume_key,
        ] {
            assert!(
                tr.get(key, false).await.unwrap().is_none(),
                "失败准备不能留下半条任务"
            );
        }
        tr.set(&pool_key, &pool_bytes);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid)
                .await
                .unwrap(),
            None,
            "失败准备不能留下孤立原尝试"
        );
        assert_eq!(catalog.reclaim_metadata_checkpoint(&reserved, fixture.owner.round).await.unwrap(),None);
        let (first, duplicate) = tokio::join!(
            Box::pin(catalog.prepare_reclaim_metadata_write(
                &reserved,
                fixture.owner.round,
                &scanned,
                &input_request,
                &metadata_plan
            )),
            Box::pin(observer.prepare_reclaim_metadata_write(
                &reserved,
                fixture.owner.round,
                &scanned,
                &input_request,
                &metadata_plan
            ))
        );
        let metadata_write = first.unwrap();
        let mut metadata_task_key = metadata_task_prefix;
        metadata_task_key.extend_from_slice(metadata_write.id().as_bytes());
        assert_eq!(duplicate.unwrap(), metadata_write);
        assert_eq!(metadata_write.plan(), metadata_plan);
        assert!(!metadata_write.closed());
        assert_eq!(
            observer
                .reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid)
                .await
                .unwrap(),
            Some(metadata_write.clone())
        );
        let tr = raw_catalog.create_trx().unwrap();
        let active = tr
            .get(&metadata_active_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        if let MetadataCase::Execution(mode) = metadata_case {
                let next_phases = recovery_scenario.clone();
                let phase = boxed_metadata_phase(move || async move {
                    let recovery_scenario = next_phases;
                    use crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter;
                use crate::managed_catalog::ReclaimMetadataObservationKind;
                fixture.permit.renew(fixture.owner.round, Phase::Serving, Instant::now() + Duration::from_secs(60),
                    );
                let budget = crate::ltfs::recovery::RecoveryBudget::default();
                if mode == MetadataExecutionCase::Commit {
                    let raw = fixture.lib.drive_as(0, 1);
                    let mut unchecked = ReclaimMetadataWriter {
                        catalog: &catalog, device: &raw, route, key: fixture.owner.key,
                        permission: &fixture.permit, budget: &budget,
                    };
                    assert!(unchecked.execute(&reserved, &metadata_write, Some(&captured)).await.is_err(), "raw transport 必须在准入和设备访问前拒绝");
                    assert!(raw.command_log().is_empty(), "raw transport 拒绝必须零 I/O");
                }
                let device = MetadataFault {
                    inner: fixture.owner.devices[drive].dev.as_ref(), mode,
                    matching: std::sync::atomic::AtomicUsize::new(0),
                    fired: std::sync::atomic::AtomicBool::new(false),
                    permission: &fixture.permit,
                    round: fixture.owner.round,
                };
                let mut writer = ReclaimMetadataWriter {
                    catalog: &catalog, device: &device, route,
                    key: fixture.owner.key, permission: &fixture.permit, budget: &budget,
                };
                let original_commands = fixture.commands.lock().unwrap().clone();
                let foreign = Permission::default();
                foreign.renew(fixture.owner.round, Phase::Serving, Instant::now() + Duration::from_secs(60),
                    );
                let mut mismatched = ReclaimMetadataWriter {
                    catalog: &catalog, device: &device, route, key: fixture.owner.key,
                    permission: &foreign, budget: &budget,
                };
                assert!(mismatched.execute(&reserved, &metadata_write, Some(&captured)).await.is_err());
                assert_eq!(*fixture.commands.lock().unwrap(), original_commands, "异许可实例必须零设备 I/O");
                assert!(writer.execute(&reserved, &metadata_write, Some(&captured_target)).await.is_err(), "其他卷不是原源实读");
                assert_eq!(*fixture.commands.lock().unwrap(), original_commands);
                assert_eq!(catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap(), Some(metadata_write.clone()));
                if mode == MetadataExecutionCase::AttemptRegistry {
                    boxed_metadata_phase(|| async {
                        let commands=fixture.commands.lock().unwrap().clone();
                        let recovered=observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&metadata_write,
                                ).await.unwrap();
                        assert_eq!(recovered,(scanned.clone(),input_request.clone()),"独立目录实例只按原尝试恢复原执行上下文");
                        let mut child_key=own_row(b'c');child_key.extend_from_slice(input_request.operation_id.as_bytes());
                        for key in [&child_key,&data_body_key,&data_destination_key,&retired_key,
                            ] {
                            let tr=raw_catalog.create_trx().unwrap();let raw=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&metadata_write,
                                    ).await;
                            let tr=raw_catalog.create_trx().unwrap();let unchanged=tr.get(key,false).await.unwrap().is_none();tr.set(key,&raw);tr.commit().await.unwrap();
                            assert!(missing.is_err() && unchanged,"丢失原上下文证明必须拒绝且不修复");
                        }
                        let tr=raw_catalog.create_trx().unwrap();let original=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                        let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        let saved:Value=serde_json::from_slice(&original).unwrap();assert_eq!(saved["version"], 6);assert_eq!(saved["context"]["file"],serde_json::to_value(&scanned).unwrap());
                        for (field,changed) in [("epoch",json!(0)),("epoch",json!(input_request.request_epoch+1)),("request",json!([3u8;32].to_vec())),
                            ] {
                            let mut corrupt=saved["context"].clone();corrupt[field]=changed;
                            // Keep the record's canonical field order. Recompute both
                            // checksums so these are semantic, not codec-only failures.
                            let context=format!(r#"{{"file":{},"epoch":{},"request":{}}}"#,serde_json::to_string(&scanned).unwrap(),corrupt["epoch"],corrupt["request"]);
                            let original_text=std::str::from_utf8(&original).unwrap();
                            let start=original_text.find("\"context\":").unwrap()+"\"context\":".len();let end=original_text.find(",\"owner\":").unwrap();
                            let raw=format!("{}{}{}",&original_text[..start],context,&original_text[end..]).into_bytes();
                            let h:Value=serde_json::from_slice(&header[..header.len()-32]).unwrap();
                            let digest=Sha256::new().chain_update(b"tape-rs/reclaim-metadata-task/7").chain_update(&raw).finalize().to_vec();
                            let mut h=format!(r#"{{"version":{},"uid":{},"attempt":{},"task_sha256":{}}}"#,h["version"],h["uid"],h["attempt"],json!(digest)).into_bytes();
                            let parent=format!("{:x}",Sha256::digest(reserved.intent().to_string().as_bytes()));
                            let hash=Sha256::new().chain_update(b"tape-rs/reclaim-metadata-registry/1").chain_update(parent.as_bytes()).chain_update(&h).finalize();h.extend_from_slice(&hash);
                            let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&raw);tr.set(&metadata_header_key,&h);tr.commit().await.unwrap();
                            let discovered=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                            let changed=if corrupt["epoch"]==0 {assert!(discovered.is_err());None} else {
                                let handle=discovered.unwrap().unwrap();
                                Some(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&handle,
                                            ).await,
                                    )
                            };
                            let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(),raw.as_slice());tr.set(&metadata_task_key,&original);tr.set(&metadata_header_key,&header);tr.commit().await.unwrap();assert!(changed.is_none_or(|result|result.is_err()),"匹配外层摘要仍不得接受不同封存请求");
                        }
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"恢复或拒绝上下文不执行 CDB");
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(metadata_write.clone()));
                        assert!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.is_err());
                        let keys=[&metadata_active_key,&metadata_volume_key,&latest_metadata_key,&metadata_header_key,&metadata_task_key,
                            ];
                        for key in keys {
                            let tr=raw_catalog.create_trx().unwrap();let original=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                            let prepare=catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_plan,
                                    ).await;
                            let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(key,false).await.unwrap().is_none();tr.set(key,&original);tr.commit().await.unwrap();
                            assert!(missing.is_err() && prepare.is_err(),"缺失原索引不能报告空闲或准备新任务");
                            assert!(untouched,"查询与准备拒绝不能修复丢失的索引");
                        }
                        let fp=metadata_plan.fingerprint().unwrap();let mut wrong=fp;wrong[0]^=1;
                        let before=format!("\"fingerprint\":{}",serde_json::to_string(&fp).unwrap());let after=format!("\"fingerprint\":{}",serde_json::to_string(&wrong).unwrap());
                        let payload=std::str::from_utf8(&active[2..]).unwrap();assert!(payload.contains(&before));let mut corrupt_claim=active[..2].to_vec();corrupt_claim.extend_from_slice(payload.replace(&before,&after).as_bytes());
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_active_key,&corrupt_claim);tr.set(&metadata_volume_key,&corrupt_claim);tr.commit().await.unwrap();
                        let mismatch=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_active_key,&active);tr.set(&metadata_volume_key,&active);tr.commit().await.unwrap();assert!(mismatch.is_err(),"两索引互相相同仍须绑定原任务计划");
                        let tr=raw_catalog.create_trx().unwrap();let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        let mut corrupt=header.clone();corrupt[5]^=1;tr.set(&metadata_header_key,&corrupt);tr.commit().await.unwrap();
                        let error=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_header_key,false).await.unwrap().unwrap().as_ref(),corrupt.as_slice());tr.set(&metadata_header_key,&header);tr.commit().await.unwrap();assert!(error.is_err());
                        let tr=raw_catalog.create_trx().unwrap();let prepared=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                        let admitted=catalog.admit_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write,
                                ).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();let writing=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();tr.set(&metadata_task_key,&prepared);tr.commit().await.unwrap();
                        let rollback=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&writing);tr.commit().await.unwrap();assert!(rollback.is_err(),"保留检查点不能接受 Writing 回退 Prepared");
                        let observation=metadata_plan.observe_current(&captured_target,None).unwrap();
                        let observed=catalog.record_reclaim_metadata_observation(&reserved,fixture.owner.round,admitted.task(),&observation,
                                ).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();let with_observation=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();tr.set(&metadata_task_key,&writing);tr.commit().await.unwrap();
                        let rollback=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&with_observation);tr.commit().await.unwrap();assert!(rollback.is_err(),"保留检查点不能丢失已保存观察");
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(observed.clone()));
                        let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                        let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();let parent=&parents.items[0];
                        assert!(catalog.reclaim_metadata_execution_context(parent,next,&observed).await.is_err(),"发现不等于领取执行上下文");
                        let claimed=catalog.claim_reclaim_metadata_attempt(parent,next,&observed).await.unwrap();
                        assert!(catalog.reclaim_metadata_execution_context(parent,next,&observed).await.is_err(),"原 owner 句柄不能自动变成接任执行上下文");
                        assert_eq!(observer.reclaim_metadata_execution_context(parent,next,&claimed).await.unwrap(),(scanned.clone(),input_request.clone()));
                        assert!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&observed).await.is_err());
                        assert_eq!(claimed.id(),metadata_write.id());assert_eq!(claimed.observation().unwrap(),observed.observation().unwrap());
                        let tr=raw_catalog.create_trx().unwrap();let claimed_raw=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();tr.set(&metadata_task_key,&with_observation);tr.commit().await.unwrap();
                        let rollback=catalog.pending_reclaim_metadata_write(parent,next).await;
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&claimed_raw);tr.commit().await.unwrap();assert!(rollback.is_err(),"检查点不能接受 owner 回退");
                        assert_eq!(catalog.reclaim_metadata_attempt(parent,next,source_data_uid,claimed.id()).await.unwrap(),claimed);
                        assert!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.is_err());
                        assert!(catalog.retry_reclaim_metadata_write(parent,next,&claimed).await.is_err(),"Writing 不能发新尝试");
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"发现/领取/拒绝均无设备命令");
                    }).await;
                    return;
                }
                if mode == MetadataExecutionCase::AttemptHistory {
                    boxed_metadata_phase(|| async {
                        let commands=fixture.commands.lock().unwrap().clone();
                        let first=catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write,
                                ).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();let first_raw=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                        let (one,two)=tokio::join!(Box::pin(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&first)),Box::pin(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&first)));
                        let second=one.unwrap();assert_eq!(two.unwrap(),second);assert_ne!(second.id(),first.id());assert_eq!(second.first_attempt(),first.id());assert_eq!(second.previous_attempt(),Some(first.id()));
                        assert_eq!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.unwrap(),first);
                        assert_eq!(catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_plan).await.unwrap(),first,"首次准备重放不能变成最新尝试");
                        let second_closed=catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&second,
                                ).await.unwrap();
                        let third=catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&second_closed,
                                ).await.unwrap();
                        assert_ne!(third.id(),second.id());assert_eq!(third.first_attempt(),first.id());assert_eq!(third.previous_attempt(),Some(second.id()));
                        assert_eq!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&third).await.unwrap(),(scanned.clone(),input_request.clone()),"后继保持原执行上下文");
                        assert!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&first).await.is_err(),"旧终态不能恢复成新尝试的执行上下文");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&first).await.unwrap(),second_closed,"旧重试始终返回原后继，不返回第三次");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&second_closed).await.unwrap(),third);
                        assert_eq!(catalog.reclaim_metadata_attempt(&reserved,fixture.owner.round,source_data_uid,first.id()).await.unwrap(),first);
                        assert_eq!(catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap(),Some(third.clone()));
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(third.clone()));
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(),first_raw.as_slice());
                        let mut edge=own_row(b'U');edge.extend_from_slice(&source_data_uid.to_be_bytes());edge.extend_from_slice(second.id().as_bytes());
                        let original=tr.get(&edge,false).await.unwrap().unwrap().to_vec();assert_eq!(original,third.id().as_bytes());tr.clear(&edge);tr.commit().await.unwrap();
                        let missing=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let retry=catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&second_closed,
                                ).await;
                        let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(&edge,false).await.unwrap().is_none();tr.set(&edge,&original);tr.commit().await.unwrap();assert!(missing.is_err() && retry.is_err() && untouched,"丢失后继边不能重建或另发请求");
                        assert!(writer.execute(&reserved,&metadata_write,Some(&captured)).await.is_err(),"旧物理尝试不能解释成新尝试");
                        assert_eq!(*fixture.commands.lock().unwrap(),commands);
                        let execution=writer.execute(&reserved,&third,Some(&captured),
                                ).await.unwrap();
                        let published=catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,execution.task(),execution.commit_evidence().unwrap(),
                                ).await.unwrap();
                        assert_eq!(published.id(),third.id());
                        let completed=catalog.reclaim_metadata_attempt(&reserved,fixture.owner.round,source_data_uid,third.id(),
                                ).await.unwrap();assert!(completed.completed());
                        assert!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&completed).await.is_err(),"已发布仅查历史，不再执行恢复");
                        assert!(observer.reclaim_metadata_retry_context(&reserved,fixture.owner.round,&completed).await.is_err(),"已发布不能准备新物理重试");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&first).await.unwrap(),second_closed);
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&second_closed).await.unwrap(),completed);
                        let mut rows=Vec::new();let tr=raw_catalog.create_trx().unwrap();
                        for id in [first.id(),second.id(),third.id()] {let mut key=own_row(b'W');key.extend_from_slice(&source_data_uid.to_be_bytes());key.extend_from_slice(id.as_bytes());rows.push((key.clone(),tr.get(&key,false).await.unwrap().unwrap().to_vec(),
                                ));tr.clear(&key);}
                        for key in [&latest_metadata_key,&metadata_header_key,&own_row(b'm')] {rows.push((key.clone(),tr.get(key,false).await.unwrap().unwrap().to_vec(),
                                ));tr.clear(key);}
                        tr.commit().await.unwrap();
                        assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),published.observation().current(),"后继发布历史不依赖原尝试行/检查点/源清单");
                        let tr=raw_catalog.create_trx().unwrap();for (key,raw) in &rows {tr.set(key,raw);}tr.commit().await.unwrap();
                        let commands=fixture.commands.lock().unwrap().clone();let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                        let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();let parent=&parents.items[0];
                        let current=catalog.claim_reclaim_metadata_attempt(parent,next,&completed).await.unwrap();assert_eq!(current.id(),third.id());
                        let tr=raw_catalog.create_trx().unwrap();let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        let old=catalog.claim_reclaim_metadata_attempt(parent,next,&second_closed).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_header_key,false).await.unwrap().unwrap().as_ref(),header.as_slice(),"历史领取不能替换当前检查点");
                        assert_eq!(catalog.retry_reclaim_metadata_write(parent,next,&first).await.unwrap(),old);
                        assert_eq!(catalog.retry_reclaim_metadata_write(parent,next,&old).await.unwrap(),current);
                        assert_eq!(catalog.reclaim_metadata_publication(parent,next,&third).await.unwrap(),Some(published));
                        assert_eq!(catalog.reclaim_metadata_attempt(parent,next,source_data_uid,first.id()).await.unwrap(),first);
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"FDB HA 历史查询/领取不发设备命令");
                    }).await;
                    return;
                }
                if mode.observes_admitted() {
                    catalog.admit_reclaim_metadata_write(&reserved, fixture.owner.round, &metadata_write,
                            ).await.unwrap();
                }
                if mode == MetadataExecutionCase::UnwrittenIpAhead {
                    let old = format!("<generationnumber>{}</generationnumber>", captured_target.construct().generation);
                    let new = format!("<generationnumber>{}</generationnumber>", captured_target.construct().generation + 1);
                    let mut changed = 0;
                    fixture.lib.with_cartridge_mut(&input.destination().placement().barcode, |cartridge| {
                        for object in &mut cartridge.partitions[0].objects {
                            if let crate::scsi::sim::LogicalObject::Record(raw) = object
                                && let Ok(text) = std::str::from_utf8(raw)
                                && text.contains("<ltfsindex")
                            {
                                assert!(text.contains(&old));
                                *raw = text.replace(&old, &new).into_bytes();
                                changed += 1;
                            }
                        }
                        cartridge.vcr += 1;
                    },
                        );
                    assert_eq!(changed, 1, "实际只构造 IP 更高代次，DP 不变");
                    }
                    if mode.checkpoint() && mode != MetadataExecutionCase::CheckpointCommitted {
                        fixture.lib.with_cartridge_mut(
                            &input.destination().placement().barcode,
                            |cartridge| {
                                let ip = &mut cartridge.partitions[0];
                                ip.objects
                                    .truncate(crate::ltfs::volume::P1_DATA_START as usize);
                                if mode != MetadataExecutionCase::CheckpointEmpty {
                                    ip.objects.push(crate::scsi::sim::LogicalObject::Filemark);
                                }
                                if mode == MetadataExecutionCase::CheckpointPrefix {
                                    ip.objects.push(crate::scsi::sim::LogicalObject::Record(
                                        b"<ltfsindex version=\"2.5.0\">".to_vec(),
                                    ));
                                }
                                ip.flushed = ip.objects.len();
                                cartridge.vcr += 1;
                            },
                        );
                }
                if mode == MetadataExecutionCase::UnwrittenIpMissing {
                    fixture.lib.with_cartridge_mut(&input.destination().placement().barcode, |cartridge| {
                        let ip = &mut cartridge.partitions[0];
                        ip.objects.truncate(crate::ltfs::volume::P1_DATA_START as usize + 1);
                        assert!(matches!(ip.objects.last(), Some(crate::scsi::sim::LogicalObject::Filemark)));
                        ip.flushed = ip.objects.len();
                        cartridge.vcr += 1;
                    },
                        );
                }
                if mode == MetadataExecutionCase::UnwrittenFraming {
                    resegment_metadata_fixture(&fixture, drive, &input.destination().placement().barcode, &captured_target,
                        );
                }
                let debt_media = matches!(mode, MetadataExecutionCase::UnwrittenIpDebt | MetadataExecutionCase::ClosedRetryContext | MetadataExecutionCase::UnwrittenIpMissing | MetadataExecutionCase::UnwrittenFraming).then(|| {
                    let crate::ltfs::recovery::IndexSourceScope::Current { ip_generation: Some(ip), .. } = captured_target.scope() else {panic!("债务夹具必须有已核验完整旧 IP")};
                    assert!(ip < captured_target.construct().generation, "必须保留真实普通写入的IP债务，不能先做checkpoint");
                    fixture.lib.cartridge(&input.destination().placement().barcode).unwrap()
                });
                assert!(Box::pin(catalog.seal_reclaim_raw_file_coverage(&reserved, fixture.owner.round, source_data_uid, metadata_write.id())).await.is_err(),"未提交原始元数据不能覆盖源文件");
                let first = writer.execute(&reserved, &metadata_write,
                    if mode.observes_admitted() {None} else {Some(&captured)},
                        ).await;
                    if mode.checkpoint() {
                        if mode == MetadataExecutionCase::CheckpointCommitted {
                            assert!(
                                matches!(first,Err(TapeError::CommitFailed {ref stage,..}) if stage=="index_partition")
                            );
                        } else if let Ok(result) = first {
                            assert!(result.needs_checkpoint());
                            assert!(
                                result.commit_evidence().is_none()
                                    && result.unwritten_evidence().is_none()
                            );
                        }
                        let task = catalog
                            .reclaim_metadata_write_status(
                                &reserved,
                                fixture.owner.round,
                                source_data_uid,
                            )
                            .await
                            .unwrap()
                            .unwrap();
                        let index = catalog
                            .observed_active_volume_index(&serial, metadata_plan.volume())
                            .await
                            .unwrap();
                        let count = catalog
                            .active_volume_file_count(
                                &serial,
                                route,
                                fixture.owner.round,
                                metadata_plan.volume(),
                            )
                            .await
                            .unwrap();
                        recovery_scenario.borrow_mut().push_back(
                            (ReclaimRecoveryScenario {
                                cluster: cluster.clone(),
                                catalog: catalog.clone(),
                                lib: fixture.lib.clone(),
                                permit: fixture.permit.clone(),
                                commands: fixture.commands.clone(),
                                old_round: fixture.owner.round,
                                next: fixture.owner.round + 100,
                                namespace,
                                route,
                                serial: serial.clone(),
                                parent: reserved.clone(),
                                previous_owner_task: task.clone(),
                                recovered: task,
                                scanned: scanned.clone(),
                                input_request: input_request.clone(),
                                metadata_plan: metadata_plan.clone(),
                                index,
                                count,
                                barcode: input.destination().placement().barcode.clone(),
                                mode,
                                metadata_task_key: metadata_task_key.clone(),
                                metadata_active_key: metadata_active_key.clone(),
                                metadata_volume_key: metadata_volume_key.clone(),
                                original_source:captured,
                            })
                            .run(recovery_scenario.clone()),
                        );
                        return;
                    }
                    if mode == MetadataExecutionCase::UnwrittenIpAhead {
                    assert!(matches!(first, Err(TapeError::RecoveryRestricted { ref reason })
                        if reason.contains("IP 索引代数") && reason.contains("领先 FDB 已观察 DP")), "{first:?}");
                    let task = catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid,
                            ).await.unwrap().unwrap();
                    assert!(task.writing() && task.observation().unwrap().is_none());
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),crate::managed_catalog::ActiveIndexIdentity::from(captured_target.construct()));
                    let tr=raw_catalog.create_trx().unwrap();
                    assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(),active.as_slice());
                    assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),active.as_slice());
                    assert!(!fixture.commands.lock().unwrap()[original_commands.len()..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)),"IP ahead 只读拒绝，无写/修尾/搬带");
                    return;
                }
                if mode == MetadataExecutionCase::UnwrittenFraming {
                    assert!(first.is_err(), "原 XML 身份相同但 DP EOD 改变，不能观察为 Unchanged 或生成未写关闭证明: {first:?}");
                    assert_stale_metadata_staging_rejected(&fixture,drive,&metadata_plan,&captured,&captured_target,
                        );
                    let task = catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid,
                            ).await.unwrap().unwrap();
                    assert!(task.writing() && task.observation().unwrap().is_none());
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),crate::managed_catalog::ActiveIndexIdentity::from(captured_target.construct()));
                    let tr=raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}
                    assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap(),
                        );
                    assert!(!fixture.commands.lock().unwrap()[original_commands.len()..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)), "物理边界变化只能拒绝，不写/修尾/搬带");
                    return;
                }
                if mode == MetadataExecutionCase::UnwrittenIpMissing {
                    let execution = first.unwrap();
                    assert!(execution.task().writing() && execution.unwritten_evidence().is_none() && execution.commit_evidence().is_none());
                    assert_eq!(execution.task().observation().unwrap().unwrap().kind(),ReclaimMetadataObservationKind::Unchanged);
                    let tr = raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}
                    assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap(),
                        );
                    assert!(!fixture.commands.lock().unwrap()[original_commands.len()..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)),"空IP保持未知，不能隐式修复");
                    return;
                }
                if matches!(mode, MetadataExecutionCase::UnwrittenClose | MetadataExecutionCase::UnwrittenIpDebt | MetadataExecutionCase::ClosedRetryContext) {
                    boxed_metadata_phase(|| async {
                    let execution = first.unwrap();
                    let task = execution.task();
                    assert!(task.writing() && execution.commit_evidence().is_none());
                    assert_eq!(task.observation().unwrap().unwrap().kind(), ReclaimMetadataObservationKind::Unchanged);
                    assert!(catalog.close_prepared_reclaim_metadata_write(&reserved, fixture.owner.round, task).await.is_err());
                    let commands = fixture.commands.lock().unwrap().clone();
                    let index = catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.unwrap();
                    assert_eq!(index, crate::managed_catalog::ActiveIndexIdentity::from(captured_target.construct()));
                    assert!(execution.unwritten_evidence().is_some(), "本轮完整 DP 精确未变且完整 IP 仅落后，应可关闭原未写尝试而不先写 IP");
                    let mut count_key=b"tape-rs/catalog/22/r/".to_vec();count_key.extend_from_slice(metadata_plan.volume().as_bytes());count_key.extend_from_slice(&index.generation.to_be_bytes());count_key.push(b'f');
                    let coverage_keys=[count_key,own_row(b'g'),own_row(b'k'),own_row(b'm')];
                    let tr=raw_catalog.create_trx().unwrap();let mut coverage_before=Vec::new();
                    for key in &coverage_keys {coverage_before.push(tr.get(key,false).await.unwrap().map(|raw|raw.to_vec()));}
                    let proof = execution.unwritten_evidence().unwrap();
                    let mut changed_request = input_request.clone();changed_request.operation_id=uuid::Uuid::new_v4();
                    assert!(catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&changed_request,&task.plan()).await.is_err());
                    for key in [&metadata_active_key,&metadata_volume_key] {
                        let tr=raw_catalog.create_trx().unwrap();tr.clear(key);tr.commit().await.unwrap();
                        assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert!(tr.get(key,false).await.unwrap().is_none());
                        tr.set(key,&active);tr.commit().await.unwrap();
                    }
                    let mut next_history=b"tape-rs/catalog/22/r/".to_vec();next_history.extend_from_slice(metadata_plan.volume().as_bytes());next_history.extend_from_slice(&(index.generation+1).to_be_bytes());
                    let mut next_count=next_history.clone();next_count.push(b'f');
                    let mut next_receipt=own_row(b'H');next_receipt.truncate(next_receipt.len()-24);next_receipt.extend_from_slice(metadata_plan.volume().as_bytes());next_receipt.extend_from_slice(&(index.generation+1).to_be_bytes());
                    for key in [&next_history,&next_count,&next_receipt] {
                        let tr=raw_catalog.create_trx().unwrap();tr.set(key,b"unknown-original-result");tr.commit().await.unwrap();
                        assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),b"unknown-original-result");tr.clear(key);tr.commit().await.unwrap();
                    }
                    let (one,two)=tokio::join!(Box::pin(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof)),Box::pin(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof)));
                    let closed=one.unwrap();assert_eq!(two.unwrap(),closed);assert!(closed.closed());assert_eq!(closed.id(),task.id());assert_eq!(closed.observation().unwrap().as_ref(),Some(proof.observation()));
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),index);
                    let tr=raw_catalog.create_trx().unwrap();for (key,before) in coverage_keys.iter().zip(&coverage_before) {assert_eq!(&tr.get(key,false).await.unwrap().map(|raw|raw.to_vec()),before,"NoWrite关闭不能增加计数或源覆盖");}
                    let tr=raw_catalog.create_trx().unwrap();assert!(tr.get(&metadata_active_key,false).await.unwrap().is_none());assert!(tr.get(&metadata_volume_key,false).await.unwrap().is_none());
                    for key in [&next_history,&next_count,&next_receipt] {assert!(tr.get(key,false).await.unwrap().is_none());}
                    tr.set(&metadata_active_key,b"later-original-task");tr.set(&metadata_volume_key,b"later-original-task");tr.commit().await.unwrap();
                    assert_eq!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof).await.unwrap(),closed);
                    assert!(catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&changed_request,&task.plan()).await.is_err());
                    let tr=raw_catalog.create_trx().unwrap();for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),b"later-original-task");tr.clear(key);}tr.commit().await.unwrap();
                    assert!(writer.execute(&reserved,&closed,None).await.is_err());
                    assert_eq!(*fixture.commands.lock().unwrap(),commands,"关闭和终态重试均无设备命令");
                    let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                    let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();
                    if mode == MetadataExecutionCase::ClosedRetryContext {
                        let restarted=ManagedCatalog::open(Some(&cluster)).unwrap();let parent=&parents.items[0];
                        let (recovered,restored_file,restored_request)=closed_reclaim_context_from_parent(&restarted,parent,next).await;
                        assert_eq!(recovered.id(),closed.id());assert_eq!(restored_file,scanned);assert_eq!(restored_request,input_request);
                        assert!(restarted.reclaim_metadata_checkpoint(&reserved,fixture.owner.round).await.is_err());
                        assert!(restarted.reclaim_metadata_retry_context(parent,next,&closed).await.is_err(),"旧owner句柄不能自动重绑");
                        let tr=raw_catalog.create_trx().unwrap();let raw=tr.get(&data_body_key,false).await.unwrap().unwrap().to_vec();tr.clear(&data_body_key);tr.commit().await.unwrap();
                        let missing=restarted.reclaim_metadata_retry_context(parent,next,&recovered).await;
                        let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(&data_body_key,false).await.unwrap().is_none();tr.set(&data_body_key,&raw);tr.commit().await.unwrap();assert!(missing.is_err() && untouched,"Closed恢复缺原证明时不能修复或换请求");
                        let (one,two)=tokio::join!(Box::pin(restarted.retry_reclaim_metadata_write(parent,next,&recovered)),Box::pin(restarted.retry_reclaim_metadata_write(parent,next,&recovered)));
                        let successor=one.unwrap();assert_eq!(two.unwrap(),successor);assert_ne!(successor.id(),recovered.id());assert_eq!(successor.previous_attempt(),Some(recovered.id()));assert_eq!(successor.first_attempt(),recovered.first_attempt());
                        assert!(!successor.closed() && !successor.writing() && !successor.completed());
                        assert_eq!(restarted.reclaim_metadata_checkpoint(parent,next).await.unwrap(),Some(successor.clone()));
                        assert_eq!(restarted.pending_reclaim_metadata_write(parent,next).await.unwrap(),Some(successor.clone()));
                        assert!(restarted.reclaim_metadata_retry_context(parent,next,&successor).await.is_err(),"Prepared不能充当Closed重试参数");
                        assert_eq!(restarted.reclaim_metadata_execution_context(parent,next,&successor).await.unwrap(),(restored_file.clone(),restored_request.clone()));
                        let tr=raw_catalog.create_trx().unwrap();let held=tr.get(&metadata_active_key,false).await.unwrap().unwrap().to_vec();let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());
                        assert_eq!(restarted.reclaim_metadata_retry_context(parent,next,&recovered).await.unwrap(),(restored_file.clone(),restored_request.clone()));
                        assert_eq!(restarted.retry_reclaim_metadata_write(parent,next,&recovered).await.unwrap(),successor,"旧重试不能重复创建后继");
                        assert!(restarted.close_unwritten_reclaim_metadata_write(parent,next,&recovered,proof).await.is_err(),"旧owner实读证明不能授接任许可");
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());assert_eq!(tr.get(&metadata_header_key,false).await.unwrap().unwrap().as_ref(),header.as_slice());
                        assert_eq!(restarted.reclaim_metadata_attempt(parent,next,recovered.plan().source_uid(),recovered.id()).await.unwrap(),recovered);
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"独立catalog只从parent恢复、校验、领取及显式重试不发送CDB");
                        assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
                        return;
                    }
                    let recovered=catalog.claim_reclaim_metadata_write(&parents.items[0],next,source_data_uid).await.unwrap();
                    assert!(recovered.closed());assert_eq!(recovered.id(),task.id());
                    assert!(catalog.close_unwritten_reclaim_metadata_write(&parents.items[0],next,&recovered,proof).await.is_err(),"旧owner实读证明不能重绑");
                    assert_eq!(*fixture.commands.lock().unwrap(),commands);
                    if let Some(before) = debt_media {
                        let after = fixture.lib.cartridge(&input.destination().placement().barcode).unwrap();
                        assert_reclaim_media_unchanged(&before,&after);
                        assert!(!fixture.commands.lock().unwrap()[original_commands.len()..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)),"债务观察与关闭全过程无写/检查点/搬带");
                    }
                    }).await;
                    return;
                }
                if mode == MetadataExecutionCase::PermissionAfterPr {
                    assert!(first.is_err());
                    assert!(device.fired.load(std::sync::atomic::Ordering::SeqCst));
                    let commands = fixture.commands.lock().unwrap().clone();
                    assert!(commands[original_commands.len()..].iter().all(|(_, op)| *op == 0x5e), "最后 PR 核对之后撤权，内层拒绝第一条媒体 CDB");
                    let task = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid,
                            ).await.unwrap().unwrap();
                    assert!(task.writing() && task.observation().unwrap().is_none());
                    assert!(writer.execute(&reserved, &task, None).await.is_err());
                    assert_eq!(*fixture.commands.lock().unwrap(), commands);
                    let tr = raw_catalog.create_trx().unwrap();
                    assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                    assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                    return;
                }
                if mode == MetadataExecutionCase::Publication {
                    let execution = first.unwrap();
                    let task = execution.task();
                    let proof = execution.commit_evidence().unwrap();
                    assert!(task.writing());
                    let before = catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.unwrap();
                    assert_eq!(before, crate::managed_catalog::ActiveIndexIdentity::from(captured_target.construct()));
                    let commands = fixture.commands.lock().unwrap().clone();
                    let tr = raw_catalog.create_trx().unwrap();
                    let original = tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                    let corrupt = String::from_utf8(original.clone()).unwrap().replace(&task.id().to_string(), &uuid::Uuid::new_v4().to_string()).into_bytes();
                    assert_ne!(corrupt, original);
                    tr.set(&metadata_task_key, &corrupt); tr.commit().await.unwrap();
                    assert!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, task, proof).await.is_err());
                    let tr = raw_catalog.create_trx().unwrap();
                    assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(), corrupt.as_slice());
                    tr.set(&metadata_task_key, &original); tr.commit().await.unwrap();
                    let mut history_key=b"tape-rs/catalog/22/r/".to_vec();
                    history_key.extend_from_slice(metadata_plan.volume().as_bytes());
                    history_key.extend_from_slice(&proof.observation().current().generation.to_be_bytes(),
                        );
                    let mut count_key=history_key.clone();count_key.push(b'f');
                    let mut receipt_key=own_row(b'H');receipt_key.truncate(receipt_key.len()-24);
                    receipt_key.extend_from_slice(metadata_plan.volume().as_bytes());
                    receipt_key.extend_from_slice(&proof.observation().current().generation.to_be_bytes(),
                        );
                    for key in [&history_key,&count_key,&receipt_key] {
                        let tr=raw_catalog.create_trx().unwrap();
                        assert!(tr.get(key,false).await.unwrap().is_none());
                        tr.set(key,b"unexplained-original-slot");tr.commit().await.unwrap();
                        assert!(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof).await.is_err());
                        assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),before);
                        assert!(catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap().unwrap().writing());
                        let tr=raw_catalog.create_trx().unwrap();
                        assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),b"unexplained-original-slot");
                        assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(),active.as_slice());
                        assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),active.as_slice());
                        tr.clear(key);tr.commit().await.unwrap();
                    }
                    // Recording a physical fact must still work after the pool
                    // limit falls below the already committed physical count.
                    let tr = raw_catalog.create_trx().unwrap();
                    tr.set(&pool_key, &full_pool); tr.commit().await.unwrap();
                    let (first,concurrent)=tokio::join!(
                        Box::pin(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof)),
                        Box::pin(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,task,proof))
                    );
                    let published=first.unwrap();
                    assert_eq!(concurrent.unwrap(),published,"并发发布仅推进一次原代次");
                    assert_eq!(published.id(), task.id());
                    assert_eq!(published.observation(), proof.observation());
                    let completed = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid,
                            ).await.unwrap().unwrap();
                    assert!(completed.completed() && !completed.writing() && !completed.closed());
                    Box::pin(catalog.seal_reclaim_raw_file_coverage(&reserved, fixture.owner.round, source_data_uid, completed.id())).await.unwrap();
                    Box::pin(catalog.seal_reclaim_raw_file_coverage(&reserved, fixture.owner.round, source_data_uid, completed.id())).await.unwrap();
                    assert!(Box::pin(catalog.step_reclaim_source_coverage(&reserved, fixture.owner.round, &crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).unwrap())).await.is_err(),"一个原始文件提交不能跳过目录及其他未覆盖节点");
                    assert_eq!(completed.id(), task.id());
                    assert_eq!(catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.unwrap(), proof.observation().current());
                    assert_eq!(catalog.active_volume_file_count(&serial, route, fixture.owner.round, metadata_plan.volume()).await.unwrap(), proof.observation().physical_files());
                    let tr = raw_catalog.create_trx().unwrap();
                    assert!(tr.get(&metadata_active_key,false).await.unwrap().is_none());
                    assert!(tr.get(&metadata_volume_key,false).await.unwrap().is_none());
                    assert_eq!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, task, proof).await.unwrap(), published, "丢失发布回执只重读原发布，不再计数/释放");
                    let mut changed_request = input_request.clone();
                    changed_request.operation_id = uuid::Uuid::new_v4();
                    assert!(catalog.prepare_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned,&changed_request,&task.plan()).await.is_err(), "不能用另一个子请求重建已发布任务");
                    assert_eq!(catalog.reclaim_metadata_publication(&reserved, fixture.owner.round, &metadata_write).await.unwrap(), Some(published.clone()));
                    let replay = writer.execute(&reserved, &metadata_write, None).await.unwrap();
                    assert!(replay.task().completed() && replay.commit_evidence().is_none());
                    assert_eq!(*fixture.commands.lock().unwrap(), commands, "目录发布和已发布重放无设备命令");
                    let tr = raw_catalog.create_trx().unwrap();
                    tr.set(&metadata_active_key, b"another-original-task");
                    tr.set(&metadata_volume_key, b"another-original-task");
                    tr.commit().await.unwrap();
                    assert!(catalog.close_prepared_reclaim_metadata_write(&reserved, fixture.owner.round, &metadata_write).await.is_err());
                    assert_eq!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, task, proof).await.unwrap(), published);
                    let tr = raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {
                        assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(), b"another-original-task");
                        tr.clear(key);
                    }
                    tr.set(&pool_key, &pool_bytes); tr.commit().await.unwrap();
                    let tr = raw_catalog.create_trx().unwrap();
                    let receipt = tr.get(&receipt_key,false).await.unwrap().unwrap().to_vec();
                    for replacement in [None, Some({let mut b=receipt.clone(); b[5]^=1;b}),
                        ] {
                        let tr = raw_catalog.create_trx().unwrap();
                        if let Some(raw)=&replacement { tr.set(&receipt_key,raw); } else { tr.clear(&receipt_key); }
                        tr.commit().await.unwrap();
                        assert!(catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.is_err());
                        assert!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, task, proof).await.is_err());
                        let tr = raw_catalog.create_trx().unwrap();
                        assert_eq!(tr.get(&receipt_key,false).await.unwrap().as_deref(), replacement.as_deref());
                        tr.set(&receipt_key,&receipt);tr.commit().await.unwrap();
                    }
                    let tr=raw_catalog.create_trx().unwrap();
                    let mut ledger_rows=Vec::new();
                    for key in [&metadata_task_key,&latest_metadata_key,&own_row(b'm')] {
                        ledger_rows.push((key.clone(),tr.get(key,false).await.unwrap().unwrap().to_vec(),
                            ));
                        tr.clear(key);
                    }
                    tr.commit().await.unwrap();
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),proof.observation().current(),"历史读取不依赖可清理的原任务/清单");
                    let tr=raw_catalog.create_trx().unwrap();
                    for (key,raw) in &ledger_rows {tr.set(key,raw);}
                    tr.commit().await.unwrap();
                    let reader = crate::daemon::fdb_read::ActiveMediaReader {
                        catalog: &catalog, device: &device, serial: &serial, route,
                        key: fixture.owner.key, permission: &fixture.permit, storage: fixture.budget(),
                    };
                    let mut read = reader.read_with_cache(path.as_bytes(), &fixture.owner.files.extent_cache_dir(), fixture.owner.files.spool_dir(),
                            ).await.unwrap().unwrap();
                    let mut body=Vec::new(); std::io::Read::read_to_end(&mut read.file,&mut body).unwrap();
                    assert_eq!(body,bytes,"活动新索引仍可读取原正文");
                    let mut volume=LtfsVolume::mount_with_budget(&device,&budget).unwrap();
                    volume.set_reservation_guard(Some(fixture.owner.key));
                    let mut raw_body=Vec::new(); volume.read_file_to_writer(metadata_plan.path(),&mut raw_body).unwrap();
                    assert_eq!(raw_body,bytes,"新 raw 引用读取相同正文"); drop(volume);
                    let commands=fixture.commands.lock().unwrap().clone();
                    let next=fixture.owner.round+100;
                    catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                    let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();
                    assert_eq!(parents.items.len(),1);
                    let recovered=catalog.claim_reclaim_metadata_write(&parents.items[0],next,source_data_uid).await.unwrap();
                    assert!(recovered.completed());
                    assert_eq!(recovered.id(),task.id());
                    Box::pin(catalog.seal_reclaim_raw_file_coverage(&parents.items[0],next,source_data_uid,recovered.id())).await.unwrap();
                    assert_eq!(catalog.reclaim_metadata_publication(&parents.items[0],next,&recovered).await.unwrap(),Some(published));
                    assert!(catalog.publish_reclaim_metadata_write(&parents.items[0],next,&recovered,proof).await.is_err(),"旧owner实读token不可重绑新owner");
                    assert_eq!(*fixture.commands.lock().unwrap(),commands,"接任已发布结果不发设备命令");
                    return;
                }
                if matches!(mode, MetadataExecutionCase::DiscardIntent | MetadataExecutionCase::DiscardIntentT1 | MetadataExecutionCase::DiscardIntentFromUnchanged
                            | MetadataExecutionCase::DiscardIntentCommitted
                    ) {
                    boxed_metadata_phase(|| async {
                        let earlier = match first {
                            Ok(execution) if mode==MetadataExecutionCase::DiscardIntentFromUnchanged => {
                                assert!(execution.unwritten_evidence().is_some());Some(execution)
                            }
                            Err(error) if mode!=MetadataExecutionCase::DiscardIntentFromUnchanged => {
                                assert!(matches!(&error,TapeError::CommitFailed {stage,..} if stage == "closing_fm"),"{error}");None
                            }
                            other => panic!("unexpected metadata result: {other:?}"),
                        };
                        let task=catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap().unwrap();
                        assert!(task.writing());
                        if earlier.is_some() {assert_eq!(task.observation().unwrap().unwrap().kind(),ReclaimMetadataObservationKind::Unchanged);}
                        else {assert!(task.observation().unwrap().is_none());}
                        let expected=if matches!(mode,MetadataExecutionCase::DiscardIntentT1 | MetadataExecutionCase::DiscardIntentFromUnchanged) {
                            // Explicitly construct T1 after the retained failed attempt.
                            // Only this Sim's uncommitted tail is replaced; old DP bytes stay.
                            fixture.lib.with_cartridge_mut(&input.destination().placement().barcode,|cartridge| {
                                let dp=&mut cartridge.partitions[1];dp.objects.truncate(metadata_plan.previous_dp_eod() as usize);
                                dp.objects.push(crate::scsi::sim::LogicalObject::Record(b"unindexed-metadata-prefix".to_vec()));dp.flushed=dp.objects.len();cartridge.vcr+=1;
                            });crate::ltfs::recovery::TailKind::UnindexedData
                        } else {crate::ltfs::recovery::TailKind::TruncatedIndex};
                        // Exercise a persistent T1/T2 tail. LOCATE may settle
                        // previously buffered writes; that is not a new repair
                        // append. Keep the original v2 flush-only failure log.
                        fixture.lib.with_cartridge_mut(&input.destination().placement().barcode,|cartridge| {
                            for partition in &mut cartridge.partitions {partition.flushed=partition.objects.len();}
                        });
                        let media=fixture.lib.cartridge(&input.destination().placement().barcode).unwrap();
                        let proof=metadata_tail_precondition(&fixture,drive,&task,&metadata_write,&captured_target,expected);
                        let commands=fixture.commands.lock().unwrap().clone();
                        let mut wrong_request=input_request.clone();wrong_request.operation_id=uuid::Uuid::new_v4();
                        assert!(catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&wrong_request,&task.plan()).await.is_err());
                        for key in [&metadata_active_key,&metadata_volume_key,&data_body_key] {
                            let tr=raw_catalog.create_trx().unwrap();let raw=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof).await;
                            let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(key,false).await.unwrap().is_none();tr.set(key,&raw);tr.commit().await.unwrap();
                            assert!(missing.is_err() && untouched,"丢原占用/输入不补建或另发意图");
                        }
                        let index=catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap();
                        let count=catalog.active_volume_file_count(&serial,route,fixture.owner.round,metadata_plan.volume()).await.unwrap();
                        let mut next_key=b"tape-rs/catalog/22/r/".to_vec();next_key.extend_from_slice(metadata_plan.volume().as_bytes());next_key.extend_from_slice(&(index.generation+1).to_be_bytes());
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&next_key,b"unknown-result");tr.commit().await.unwrap();
                        assert!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&next_key,false).await.unwrap().unwrap().as_ref(),b"unknown-result");tr.clear(&next_key);tr.set(&pool_key,&full_pool);tr.commit().await.unwrap();
                        let (one,two)=tokio::join!(Box::pin(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof)),Box::pin(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof)));
                        let discarding=one.unwrap();assert_eq!(two.unwrap(),discarding,"并发只保存一个原修尾意图");
                        assert!(discarding.discarding() && !discarding.writing() && !discarding.closed() && !discarding.completed());
                        assert_eq!(discarding.id(),task.id());assert_eq!(discarding.plan(),task.plan());
                        assert_eq!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof).await.unwrap(),discarding,"未知准入结果保留同一phase/attempt");
                        assert!(catalog.admit_reclaim_metadata_write(&reserved,fixture.owner.round,&task).await.is_err());
                        assert!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.is_err(),"Prepared旧回执不能关闭Discarding");
                        assert!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&discarding).await.is_err());
                        if let Some(earlier)=&earlier {
                            assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,earlier.task(),earlier.unwritten_evidence().unwrap()).await.is_err(),"旧Writing阶段未写token不能释放Discarding占用");
                        }
                        assert!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&discarding).await.is_err());
                        assert!(catalog.reclaim_metadata_retry_context(&reserved,fixture.owner.round,&discarding).await.is_err());
                        let observation=metadata_plan.observe_current(&captured_target,None).unwrap();
                        assert!(catalog.record_reclaim_metadata_observation(&reserved,fixture.owner.round,&task,&observation).await.is_err());
                        assert_eq!(catalog.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&discarding).await.unwrap(),(scanned.clone(),input_request.clone()));
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(discarding.clone()));
                        assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),index);
                        assert_eq!(catalog.active_volume_file_count(&serial,route,fixture.owner.round,metadata_plan.volume()).await.unwrap(),count);
                        let tr=raw_catalog.create_trx().unwrap();for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}tr.set(&pool_key,&pool_bytes);tr.commit().await.unwrap();
                        assert!(writer.execute(&reserved,&task,None).await.is_err(),"普通writer不能接续Discarding");
                        let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                        let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();
                        let saved=observer.reclaim_metadata_checkpoint(&parents.items[0],next).await.unwrap().unwrap();assert_eq!(saved,discarding);
                        let recovered=catalog.claim_reclaim_metadata_attempt(&parents.items[0],next,&saved).await.unwrap();assert!(recovered.discarding());assert_eq!(recovered.id(),task.id());
                        assert_eq!(observer.reclaim_metadata_execution_context(&parents.items[0],next,&recovered).await.unwrap(),(scanned.clone(),input_request.clone()));
                        assert!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&task,&proof).await.is_err());
                        assert!(catalog.begin_reclaim_metadata_discard(&parents.items[0],next,&recovered,&proof).await.is_err(),"旧owner前提不能重绑接任");
                        assert!(catalog.close_prepared_reclaim_metadata_write(&parents.items[0],next,&recovered).await.is_err());
                        assert_eq!(catalog.pending_reclaim_metadata_write(&parents.items[0],next).await.unwrap(),Some(recovered.clone()));
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"FDB意图/上下文/HA均无设备命令");
                        assert_reclaim_media_unchanged(&media,&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
                        recovery_scenario.borrow_mut().push_back((ReclaimRecoveryScenario {
                            cluster: cluster.clone(), catalog: catalog.clone(), lib: fixture.lib.clone(),
                            permit: fixture.permit.clone(), commands: fixture.commands.clone(),
                            old_round: fixture.owner.round, next, namespace, route, serial: serial.clone(),
                            parent: parents.items[0].clone(), previous_owner_task: discarding, recovered, scanned: scanned.clone(),
                            input_request: input_request.clone(), metadata_plan: metadata_plan.clone(), index, count,
                            barcode: input.destination().placement().barcode.clone(), mode,
                            metadata_task_key: metadata_task_key.clone(), metadata_active_key: metadata_active_key.clone(),
                            metadata_volume_key: metadata_volume_key.clone(), original_source:captured,
                        }).run(recovery_scenario.clone()));
                    }).await;return;
                }
                let failure = matches!(mode, MetadataExecutionCase::IpFailure | MetadataExecutionCase::ClosingFailure | MetadataExecutionCase::PermissionAfterWrite);
                if failure {
                    let error = first.unwrap_err();
                    let stage = match mode {
                        MetadataExecutionCase::IpFailure => "index_partition",
                        MetadataExecutionCase::ClosingFailure => "closing_fm",
                        MetadataExecutionCase::PermissionAfterWrite => "index_records",
                        _ => unreachable!(),
                    };
                    assert!(matches!(error, TapeError::CommitFailed { stage: ref actual, .. } if actual == stage), "{error}");
                    assert!(device.fired.load(std::sync::atomic::Ordering::SeqCst));
                    let retained = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid,
                            ).await.unwrap().unwrap();
                    assert!(retained.writing());
                    assert!(retained.observation().unwrap().is_none());
                } else {
                    let result = first.unwrap();
                    assert!(result.task().writing());
                    if mode == MetadataExecutionCase::LostAdmission {
                        assert!(result.unwritten_evidence().is_some(), "完整旧IP债务不改变当前DP精确未写事实；仅返回证明，不自动关闭");
                    }
                    let expected = if mode == MetadataExecutionCase::LostAdmission {ReclaimMetadataObservationKind::Unchanged} else {ReclaimMetadataObservationKind::Committed};
                    assert_eq!(result.task().observation().unwrap().unwrap().kind(), expected);
                }
                let before_retry = fixture.commands.lock().unwrap().len();
                let replay = writer.execute(&reserved, &metadata_write, None).await;
                if matches!(mode, MetadataExecutionCase::ClosingFailure | MetadataExecutionCase::PermissionAfterWrite) {
                    assert!(replay.is_err(), "截断尾部不能观察为完成或自动修复");
                } else {
                    let replay = replay.unwrap();
                    let observed = replay.task().observation().unwrap().unwrap();
                    let expected = if mode == MetadataExecutionCase::LostAdmission {ReclaimMetadataObservationKind::Unchanged} else {ReclaimMetadataObservationKind::Committed};
                    assert_eq!(observed.kind(), expected);
                    let again = writer.execute(&reserved, replay.task(), None).await.unwrap();
                    assert_eq!(again, replay);
                }
                assert!(!fixture.commands.lock().unwrap()[before_retry..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)), "任何重试只读，不格式化/重写/MAM/搬带");
                let task = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid,
                        ).await.unwrap().unwrap();
                assert!(task.writing());
                assert!(catalog.close_prepared_reclaim_metadata_write(&reserved, fixture.owner.round, &task).await.is_err());
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                let before_fence = fixture.commands.lock().unwrap().clone();
                fixture.permit.close(fixture.owner.round);
                assert!(writer.execute(&reserved, &task, None).await.is_err());
                assert_eq!(*fixture.commands.lock().unwrap(), before_fence, "许可失效不再发送设备命令");
            });
                recovery_scenario.borrow_mut().push_back(phase);
            return;
        }
        if let MetadataCase::Observation { changed_root } = metadata_case {
            boxed_metadata_phase(|| async {
                use crate::managed_catalog::ReclaimMetadataObservationKind;
                let unchanged = metadata_plan
                    .observe_current(&captured_target, None)
                    .unwrap();
                assert_eq!(unchanged.kind(), ReclaimMetadataObservationKind::Unchanged);
                assert_eq!(
                    unchanged.plan_fingerprint(),
                    metadata_plan.fingerprint().unwrap()
                );
                assert!(
                    metadata_plan.observe_current(&captured, None).is_err(),
                    "其他卷的实际来源不是目的结果"
                );
                assert!(
                    catalog
                        .record_reclaim_metadata_observation(
                            &reserved,
                            fixture.owner.round,
                            &metadata_write,
                            &unchanged
                        )
                        .await
                        .is_err(),
                    "Prepared 不能冒充已开写观察"
                );
                let admitted = Box::pin(catalog.admit_reclaim_metadata_write(
                    &reserved,
                    fixture.owner.round,
                    &metadata_write,
                ))
                .await
                .unwrap();
                assert!(!admitted.needs_observation());
                let admitted_task = admitted.task().clone();
                assert!(
                    catalog
                        .record_reclaim_metadata_observation(
                            &reserved,
                            fixture.owner.round + 1,
                            &admitted_task,
                            &unchanged
                        )
                        .await
                        .is_err()
                );
                let writing = catalog
                    .record_reclaim_metadata_observation(
                        &reserved,
                        fixture.owner.round,
                        &admitted_task,
                        &unchanged,
                    )
                    .await
                    .unwrap();
                assert_eq!(writing.observation().unwrap(), Some(unchanged.clone()));
                assert_eq!(
                    catalog
                        .record_reclaim_metadata_observation(
                            &reserved,
                            fixture.owner.round,
                            &admitted_task,
                            &unchanged
                        )
                        .await
                        .unwrap(),
                    writing,
                    "丢失保存回执后只返回原快照"
                );
                let checked = fdb_loading::operation_guard(
                    &fixture.owner,
                    &fixture.permit,
                    fixture.owner.devices[drive].dev.as_ref(),
                    || {
                        futures_executor::block_on(async {
                            let current = catalog
                                .reclaim_metadata_write_status(
                                    &reserved,
                                    fixture.owner.round,
                                    source_data_uid,
                                )
                                .await
                                .map_err(|e| TapeError::Catalog(e.to_string()))?;
                            if current.as_ref() != Some(&writing) || !writing.writing() {
                                return Err(TapeError::Catalog("Sim 元数据观察原任务改变".into(),
                                    ));
                            }
                            Ok(())
                        })
                    },
                );
                let label = crate::ltfs::volume::read_label_only(&checked).unwrap();
                let recorded = crate::ltfs::recovery::capture_recorded_index_source(
                    &TapeDrive::new(&checked),
                    &label,
                    &captured_target.identity().unwrap(),
                    &crate::ltfs::recovery::RecoveryBudget::default(),
                )
                .unwrap();
                assert!(
                    metadata_plan.observe_current(&recorded, None).is_err(),
                    "历史来源不充当当前提交证明"
                );
                let source_entry = captured.entry_by_uid(source_data_uid).unwrap().unwrap();
                let donor_path = input_request.ltfs_path().unwrap();
                let mut volume = LtfsVolume::mount(&checked).unwrap();
                volume.set_reservation_guard(Some(fixture.owner.key));
                if changed_root {
                    volume.set_root_xattr("test.unrelated", "changed").unwrap();
                }
                volume
                    .reference_reclaimed_body(
                        metadata_plan.path(),
                        metadata_plan.uid(),
                        &donor_path,
                        input.destination().copy().ltfs_file_uid,
                        &source_entry,
                    )
                    .unwrap();
                volume.commit().unwrap();
                drop(volume);
                let actual = crate::ltfs::recovery::capture_index_source(
                    &TapeDrive::new(&checked),
                    &label,
                    &crate::ltfs::recovery::RecoveryBudget::default(),
                )
                .unwrap();
                let predecessor = crate::ltfs::recovery::capture_recorded_index_source(
                    &TapeDrive::new(&checked),
                    &label,
                    &metadata_plan.previous_source().unwrap(),
                    &crate::ltfs::recovery::RecoveryBudget::default(),
                )
                .unwrap();
                assert!(metadata_plan.observe_current(&actual, None).is_err());
                let saved = if changed_root {
                    let error = metadata_plan
                        .observe_current(&actual, Some(&predecessor))
                        .unwrap_err();
                    assert!(
                        error.to_string().contains("对象树"),
                        "新增节点正确仍须拒绝无关树变化: {error}"
                    );
                    writing.clone()
                } else {
                    let observed = metadata_plan
                        .observe_current(&actual, Some(&predecessor))
                        .unwrap();
                    assert_eq!(observed.kind(), ReclaimMetadataObservationKind::Committed);
                    assert_eq!(
                        observed.current(),
                        crate::managed_catalog::ActiveIndexIdentity::from(actual.construct())
                    );
                    assert_eq!(
                        observed.plan_fingerprint(),
                        metadata_plan.fingerprint().unwrap()
                    );
                    assert_eq!(observed.physical_files(), unchanged.physical_files() + 1);
                    assert_eq!(
                        metadata_plan
                            .observe_current(&actual, Some(&predecessor))
                            .unwrap(),
                        observed,
                        "重复观察不改变原证明"
                    );
                    let saved = catalog
                        .record_reclaim_metadata_observation(
                            &reserved,
                            fixture.owner.round,
                            &writing,
                            &observed,
                        )
                        .await
                        .unwrap();
                    assert_eq!(saved.observation().unwrap(), Some(observed.clone()));
                    assert_eq!(
                        catalog
                            .record_reclaim_metadata_observation(
                                &reserved,
                                fixture.owner.round,
                                &writing,
                                &observed
                            )
                            .await
                            .unwrap(),
                        saved,
                        "未取得保存回执仍只重读原记录"
                    );
                    assert!(
                        catalog
                            .record_reclaim_metadata_observation(
                                &reserved,
                                fixture.owner.round,
                                &admitted_task,
                                &unchanged
                            )
                            .await
                            .is_err(),
                        "旧快照不能回退已提交观察"
                    );
                    saved
                };
                assert_eq!(
                    catalog
                        .reclaim_metadata_write_status(
                            &reserved,
                            fixture.owner.round,
                            source_data_uid
                        )
                        .await
                        .unwrap(),
                    Some(saved.clone()),
                    "观察本身不完成任务或释放未知写入占用"
                );
                assert!(
                    catalog
                        .close_prepared_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &saved
                        )
                        .await
                        .is_err()
                );
                let retained = saved.observation().unwrap().unwrap();
                for key in [
                    &metadata_active_key,
                    &metadata_volume_key,
                    &latest_metadata_key,
                    &metadata_task_key,
                ] {
                    let tr = raw_catalog.create_trx().unwrap();
                    let original = tr.get(key, false).await.unwrap().unwrap().to_vec();
                    tr.clear(key);
                    tr.commit().await.unwrap();
                    assert!(
                        catalog
                            .record_reclaim_metadata_observation(
                                &reserved,
                                fixture.owner.round,
                                &saved,
                                &retained
                            )
                            .await
                            .is_err(),
                        "观察保存不能补建缺失的原任务或占用"
                    );
                    let tr = raw_catalog.create_trx().unwrap();
                    assert!(tr.get(key, false).await.unwrap().is_none());
                    tr.set(key, &original);
                    tr.commit().await.unwrap();
                }
                let tr = raw_catalog.create_trx().unwrap();
                let original = tr
                    .get(&metadata_task_key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_vec();
                for offset in [None, Some(51usize), Some(83)] {
                    let mut corrupt: Value = serde_json::from_slice(&original).unwrap();
                    let bytes = corrupt["observation"].as_array_mut().unwrap();
                    if let Some(offset) = offset {
                        bytes[offset] = json!(bytes[offset].as_u64().unwrap() ^ 1);
                    } else {
                        bytes.push(json!(0));
                    }
                    let corrupt = serde_json::to_vec(&corrupt).unwrap();
                    let tr = raw_catalog.create_trx().unwrap();
                    tr.set(&metadata_task_key, &corrupt);
                    tr.commit().await.unwrap();
                    assert!(
                        catalog
                            .reclaim_metadata_write_status(
                                &reserved,
                                fixture.owner.round,
                                source_data_uid
                            )
                            .await
                            .is_err()
                    );
                    assert!(
                        catalog
                            .record_reclaim_metadata_observation(
                                &reserved,
                                fixture.owner.round,
                                &saved,
                                &retained
                            )
                            .await
                            .is_err()
                    );
                    let tr = raw_catalog.create_trx().unwrap();
                    assert_eq!(
                        tr.get(&metadata_task_key, false)
                            .await
                            .unwrap()
                            .unwrap()
                            .as_ref(),
                        corrupt.as_slice()
                    );
                    tr.set(&metadata_task_key, &original);
                    tr.commit().await.unwrap();
                }
                let commands = fixture.commands.lock().unwrap().clone();
                let previous_owner = fixture.owner.round;
                let next = previous_owner + 100;
                catalog
                    .claim_active_owner_epoch(&serial, next)
                    .await
                    .unwrap();
                assert!(
                    catalog
                        .record_reclaim_metadata_observation(
                            &reserved,
                            previous_owner,
                            &saved,
                            &unchanged
                        )
                        .await
                        .is_err()
                );
                let parents = catalog
                    .claim_reclaim_reservations_page(&serial, route, next, None, 16)
                    .await
                    .unwrap();
                assert_eq!(parents.items.len(), 1);
                assert!(
                    catalog
                        .reclaim_metadata_write_status(&parents.items[0], next, source_data_uid)
                        .await
                        .is_err()
                );
                let recovered = catalog
                    .claim_reclaim_metadata_write(&parents.items[0], next, source_data_uid)
                    .await
                    .unwrap();
                assert_eq!(
                    recovered.observation().unwrap(),
                    saved.observation().unwrap()
                );
                assert!(recovered.writing());
                assert_eq!(recovered.plan(), metadata_plan);
                assert_eq!(
                    catalog
                        .record_reclaim_metadata_observation(
                            &parents.items[0],
                            next,
                            &recovered,
                            &retained
                        )
                        .await
                        .unwrap(),
                    recovered,
                    "新 owner 保留原观察回执"
                );
                assert!(
                    catalog
                        .close_prepared_reclaim_metadata_write(&parents.items[0], next, &recovered)
                        .await
                        .is_err()
                );
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(
                    tr.get(&metadata_active_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    active.as_slice()
                );
                assert_eq!(
                    tr.get(&metadata_volume_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    active.as_slice()
                );
                assert_eq!(
                    *fixture.commands.lock().unwrap(),
                    commands,
                    "保存与接管观察不发送 CDB"
                );
            })
            .await;
            return;
        }
        if metadata_case == MetadataCase::Admission {
            boxed_metadata_phase(|| async {
                let commands = fixture.commands.lock().unwrap().clone();
                assert!(!metadata_write.writing());
                assert!(
                    catalog
                        .admit_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round + 1,
                            &metadata_write
                        )
                        .await
                        .is_err()
                );
                // Capacity can change after preparation; admission must recheck it.
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&pool_key, &full_pool);
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .admit_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &metadata_write
                        )
                        .await
                        .is_err()
                );
                assert_eq!(
                    catalog
                        .reclaim_metadata_write_status(
                            &reserved,
                            fixture.owner.round,
                            source_data_uid
                        )
                        .await
                        .unwrap(),
                    Some(metadata_write.clone())
                );
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&pool_key, &pool_bytes);
                tr.commit().await.unwrap();
                let offline =
                    ManagedCatalog::open(Some(unavailable_path.to_str().unwrap())).unwrap();
                let failed = tokio::time::timeout(
                    Duration::from_secs(12),
                    Box::pin(offline.admit_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &metadata_write,
                    )),
                )
                .await
                .expect("准入的 FDB 依赖故障须有界返回");
                assert!(failed.is_err());
                let (a, b) = tokio::join!(
                    Box::pin(catalog.admit_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &metadata_write
                    )),
                    Box::pin(observer.admit_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &metadata_write
                    ))
                );
                let a = a.unwrap();
                let b = b.unwrap();
                assert_ne!(
                    a.needs_observation(),
                    b.needs_observation(),
                    "并发准入只有一次新转换，其余须观察原结果"
                );
                assert_eq!(a.task(), b.task());
                let writing = a.task().clone();
                assert!(writing.writing());
                assert!(!writing.closed());
                assert_eq!(writing.plan(), metadata_plan);
                assert_eq!(
                    observer
                        .reclaim_metadata_write_status(
                            &reserved,
                            fixture.owner.round,
                            source_data_uid
                        )
                        .await
                        .unwrap(),
                    Some(writing.clone())
                );
                assert_eq!(
                    catalog
                        .prepare_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &scanned,
                            &input_request,
                            &metadata_plan
                        )
                        .await
                        .unwrap(),
                    writing
                );
                // Lost admission reply is not permission to replay a possible write.
                assert!(
                    observer
                        .admit_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &metadata_write
                        )
                        .await
                        .unwrap()
                        .needs_observation()
                );
                let mut changed_request = input_request.clone();
                changed_request.operation_id = uuid::Uuid::new_v4();
                assert!(
                    catalog
                        .prepare_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &scanned,
                            &changed_request,
                            &writing.plan()
                        )
                        .await
                        .is_err(),
                    "新请求不能重新构造原 Writing 任务"
                );
                let keys = [
                    &latest_metadata_key,
                    &metadata_task_key,
                    &metadata_active_key,
                    &metadata_volume_key,
                ];
                let tr = raw_catalog.create_trx().unwrap();
                let mut rows = Vec::new();
                for key in keys {
                    rows.push(tr.get(key, false).await.unwrap().unwrap().to_vec());
                }
                for original in [&metadata_write, &writing] {
                    assert!(matches!(
                        catalog
                            .close_prepared_reclaim_metadata_write(
                                &reserved,
                                fixture.owner.round,
                                original
                            )
                            .await
                            .as_ref()
                            .map_err(|e| e.domain()),
                        Err(crate::managed_catalog::CatalogError::PoolBusy)
                    ));
                }
                // A retained Writing reply cannot become an unwritten close
                // when a canonical record's phase is rolled back.
                let raw_writing = rows[1].clone();
                let raw_prepared = String::from_utf8(raw_writing.clone())
                    .unwrap()
                    .replace("\"phase\":\"writing\"", "\"phase\":\"prepared\"")
                    .into_bytes();
                assert_ne!(raw_prepared, raw_writing);
                let tr = raw_catalog.create_trx().unwrap();
                tr.set(&metadata_task_key, &raw_prepared);
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .admit_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &writing
                        )
                        .await
                        .is_err()
                );
                assert!(
                    catalog
                        .close_prepared_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &writing
                        )
                        .await
                        .is_err()
                );
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(
                    tr.get(&metadata_task_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    raw_prepared.as_slice()
                );
                assert_eq!(
                    tr.get(&metadata_active_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    active.as_slice()
                );
                tr.set(&metadata_task_key, &raw_writing);
                tr.commit().await.unwrap();
                for (key, raw) in keys.into_iter().zip(&rows) {
                    let tr = raw_catalog.create_trx().unwrap();
                    assert_eq!(
                        tr.get(key, false).await.unwrap().unwrap().as_ref(),
                        raw.as_slice()
                    );
                    tr.clear(key);
                    tr.commit().await.unwrap();
                    assert!(
                        catalog
                            .admit_reclaim_metadata_write(
                                &reserved,
                                fixture.owner.round,
                                &writing
                            )
                            .await
                            .is_err()
                    );
                    assert!(
                        catalog
                            .close_prepared_reclaim_metadata_write(
                                &reserved,
                                fixture.owner.round,
                                &writing
                            )
                            .await
                            .is_err()
                    );
                    let tr = raw_catalog.create_trx().unwrap();
                    assert!(
                        tr.get(key, false).await.unwrap().is_none(),
                        "未知写入的缺失占用不得补建或清除"
                    );
                    tr.set(key, raw);
                    tr.commit().await.unwrap();
                }
                let old = fixture.owner.round;
                let next = old + 100;
                catalog
                    .claim_active_owner_epoch(&serial, next)
                    .await
                    .unwrap();
                assert!(
                    catalog
                        .admit_reclaim_metadata_write(
                            &reserved,
                            old,
                            &writing
                        )
                        .await
                        .is_err()
                );
                let parents = catalog
                    .claim_reclaim_reservations_page(&serial, route, next, None, 16)
                    .await
                    .unwrap();
                assert_eq!(parents.items.len(), 1);
                assert!(
                    catalog
                        .reclaim_metadata_write_status(&parents.items[0], next, source_data_uid)
                        .await
                        .is_err(),
                    "先领取父，再显式领取原元数据任务"
                );
                let recovered = catalog
                    .claim_reclaim_metadata_write(&parents.items[0], next, source_data_uid)
                    .await
                    .unwrap();
                assert!(recovered.writing());
                assert_eq!(recovered.plan(), metadata_plan);
                assert_eq!(recovered.owner(), next);
                assert!(
                    catalog
                        .admit_reclaim_metadata_write(
                            &parents.items[0],
                            next,
                            &recovered
                        )
                        .await
                        .unwrap()
                        .needs_observation()
                );
                assert!(
                    catalog
                        .close_prepared_reclaim_metadata_write(&parents.items[0], next, &recovered)
                        .await
                        .is_err(),
                    "接管 Writing 不能冒充未开写关闭"
                );
                assert_eq!(
                    catalog
                        .reclaim_metadata_write_status(&parents.items[0], next, source_data_uid)
                        .await
                        .unwrap(),
                    Some(recovered)
                );
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(
                    tr.get(&metadata_active_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    active.as_slice()
                );
                assert_eq!(
                    tr.get(&metadata_volume_key, false)
                        .await
                        .unwrap()
                        .unwrap()
                        .as_ref(),
                    active.as_slice()
                );
                assert_eq!(
                    *fixture.commands.lock().unwrap(),
                    commands,
                    "持久准入和未知结果恢复没有设备 I/O"
                );
            })
            .await;
            return;
        }
        // The normal node discovery path must recognize the typed claim and stop.
        assert!(matches!(
            catalog
                .pending_node_mutation_attempt(
                    &serial,
                    route,
                    fixture.owner.round,
                    metadata_plan.volume()
                )
                .await,
            Err(crate::managed_catalog::CatalogError::PoolBusy)
        ));
        assert!(matches!(
            catalog
                .pending_node_rename_task(
                    &serial,
                    route,
                    fixture.owner.round,
                    metadata_plan.volume()
                )
                .await,
            Err(crate::managed_catalog::CatalogError::PoolBusy)
        ));
        assert!(
            catalog
                .check_active_upload_gate(route, fixture.owner.round, &input_request)
                .await
                .is_err()
        );
        let changed_plan = inventory
            .plan_reclaimed_body(&input, &captured_target, "different-raw")
            .unwrap();
        assert!(
            catalog
                .prepare_reclaim_metadata_write(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &input_request,
                    &changed_plan
                )
                .await
                .is_err()
        );
        let original_task = tr
            .get(&metadata_task_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        for key in [
            &latest_metadata_key,
            &metadata_task_key,
            &metadata_active_key,
            &metadata_volume_key,
        ] {
            let tr = raw_catalog.create_trx().unwrap();
            let old = tr.get(key, false).await.unwrap().unwrap().to_vec();
            tr.clear(key);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .prepare_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &scanned,
                        &input_request,
                        &metadata_plan
                    )
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .close_prepared_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &metadata_write
                    )
                    .await
                    .is_err()
            );
            let tr = raw_catalog.create_trx().unwrap();
            assert!(
                tr.get(key, false).await.unwrap().is_none(),
                "缺失记录不得自动修复或解锁"
            );
            tr.set(key, &old);
            tr.commit().await.unwrap();
        }
        let tr = raw_catalog.create_trx().unwrap();
        // Preserve the typed wire's canonical field order; fail on the changed
        // immutable identity, rather than merely on JSON key ordering.
        let needle = format!(
            "\"expected\":{{\"path\":\"reclaimed-raw\",\"uid\":{}",
            metadata_plan.uid()
        );
        let replacement = format!(
            "\"expected\":{{\"path\":\"reclaimed-raw\",\"uid\":{}",
            metadata_plan.uid() + 1
        );
        let changed = std::str::from_utf8(&original_task)
            .unwrap()
            .replace(&needle, &replacement)
            .into_bytes();
        assert_ne!(changed, original_task);
        tr.set(&metadata_task_key, &changed);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid)
                .await
                .is_err()
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert_eq!(
            tr.get(&metadata_task_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            changed
        );
        assert_eq!(
            tr.get(&metadata_active_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            active
        );
        tr.set(&metadata_task_key, &original_task);
        tr.commit().await.unwrap();
        assert_eq!(
            observer
                .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &input_request,)
                .await
                .unwrap(),
            input
        );
        assert!(
            catalog
                .reclaim_metadata_input(
                    &reserved,
                    fixture.owner.round + 1,
                    &scanned,
                    &input_request,
                )
                .await
                .is_err()
        );
        let mut wrong_request = input_request.clone();
        wrong_request.expected_size += 1;
        assert!(
            catalog
                .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &wrong_request,)
                .await
                .is_err()
        );
        // Keep canonical field order while altering only the sealed source
        // index SHA: a valid-looking but different source head is not input.
        let manifest_key = own_row(b'm');
        let tr = raw_catalog.create_trx().unwrap();
        let manifest_raw = tr
            .get(&manifest_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let manifest: Value = serde_json::from_slice(&manifest_raw).unwrap();
        let before_head = serde_json::to_string(&manifest["summary"]["head"]).unwrap();
        let mut altered_head = manifest["summary"]["head"].clone();
        let head_cells = altered_head.as_array_mut().unwrap();
        let last = head_cells.last_mut().unwrap();
        *last = json!(last.as_u64().unwrap() ^ 1);
        let changed_manifest = std::str::from_utf8(&manifest_raw)
            .unwrap()
            .replace(
                &format!("\"head\":{before_head}"),
                &format!("\"head\":{}", serde_json::to_string(&altered_head).unwrap()),
            )
            .into_bytes();
        assert_ne!(changed_manifest, manifest_raw);
        tr.set(&manifest_key, &changed_manifest);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &input_request,)
                .await
                .is_err(),
            "source manifest head must match the original current source"
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert_eq!(
            tr.get(&manifest_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            changed_manifest.as_slice()
        );
        tr.set(&manifest_key, &manifest_raw);
        tr.commit().await.unwrap();
        let watched = [
            &data_body_key,
            &data_destination_key,
            &data_progress_key,
            &retired_key,
            &source_object_key,
        ];
        let tr = raw_catalog.create_trx().unwrap();
        let mut original_rows = Vec::new();
        for key in watched {
            original_rows.push((
                key.clone(),
                tr.get(key, false).await.unwrap().unwrap().to_vec(),
            ));
        }
        for (key, original) in &original_rows {
            let corrupt = if key == &data_body_key
                || key == &data_destination_key
                || key == &data_progress_key
            {
                let mut value: Value = serde_json::from_slice(original).unwrap();
                value["version"] = json!(2);
                value.to_string().into_bytes()
            } else {
                b"corrupt-original-input".to_vec()
            };
            for replacement in [None, Some(corrupt)] {
                let tr = raw_catalog.create_trx().unwrap();
                match &replacement {
                    Some(bytes) => tr.set(key, bytes),
                    None => tr.clear(key),
                }
                tr.commit().await.unwrap();
                assert!(
                    catalog
                        .reclaim_metadata_input(
                            &reserved,
                            fixture.owner.round,
                            &scanned,
                            &input_request,
                        )
                        .await
                        .is_err(),
                    "missing or corrupt original row must not produce donor input"
                );
                let tr = raw_catalog.create_trx().unwrap();
                for (other, expected) in &original_rows {
                    let expected = if other == key {
                        replacement.clone()
                    } else {
                        Some(expected.clone())
                    };
                    assert_eq!(
                        tr.get(other, false).await.unwrap().map(|v| v.to_vec()),
                        expected,
                        "failed lookup must never repair, clear or partially rewrite any original row"
                    );
                }
                tr.set(key, original);
                tr.commit().await.unwrap();
            }
        }
        // Ordinary result retention may end after the original verification:
        // use the exact terminal already sealed in the target readback proof.
        let tr = raw_catalog.create_trx().unwrap();
        let terminal = tr
            .get(&terminal_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.clear(&terminal_key);
        tr.commit().await.unwrap();
        assert_eq!(
            observer
                .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &input_request,)
                .await
                .unwrap(),
            input
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert!(tr.get(&terminal_key, false).await.unwrap().is_none());
        tr.set(&terminal_key, &terminal);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .reclaim_metadata_input(&reserved, fixture.owner.round, &scanned, &input_request,)
                .await
                .unwrap(),
            input
        );
        assert_eq!(fixture.drive_log(), drive_before_metadata);
        assert_eq!(fixture.changer_log(), changer_before_metadata);
        let metadata_progress = catalog
            .seal_reclaim_file_metadata(
                &reserved,
                fixture.owner.round,
                &scanned,
                &intent.request(),
                &metadata_evidence,
            )
            .await
            .unwrap();
        assert_eq!(
            (metadata_progress.files(), metadata_progress.aliases()),
            (1, 0)
        );
        assert_eq!(fixture.drive_log(), drive_before_metadata);
        assert_eq!(
            fixture.changer_log(),
            changer_before_metadata,
            "FDB失败与回执重放不能触发设备操作"
        );
        assert!(
            !catalog
                .retire_reclaim_file_source(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .unwrap(),
            "退休终态重试不能重复删除或改变回执"
        );
        assert_eq!(
            catalog
                .physical_copy(route.route.namespace, scanned.file_id(), original_copy_id)
                .await
                .unwrap(),
            None
        );
        assert!(
            catalog
                .copies_on_volume(reclaim.volume(), None, 100)
                .await
                .unwrap()
                .items
                .is_empty(),
            "仅删除精确原副本对应的 V 引用"
        );
        assert_eq!(
            catalog
                .physical_copy(
                    route.route.namespace,
                    scanned.file_id(),
                    verified.metadata.head.copy_id
                )
                .await
                .unwrap(),
            Some(verified.metadata.copy.clone()),
            "目标副本必须完整保留"
        );
        catalog
            .check_reclaim_source_unreferenced(&reserved, fixture.owner.round)
            .await
            .unwrap();
        let finished_walk = catalog
            .step_reclaim_walk(&reserved, fixture.owner.round)
            .await
            .unwrap();
        assert!(finished_walk.complete());
        // Scheduling here only selects the known source in this fixture. The
        // read itself independently proves the sealed copy, current head and PR.
        let placement = catalog
            .volume_placement(reclaim.volume())
            .await
            .unwrap()
            .unwrap();
        let head = catalog
            .active_loaded_volume_head(&serial, reclaim.barcode(), route, fixture.owner.round)
            .await
            .unwrap()
            .unwrap();
        let drive = fdb_loading::select_write(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status,
            &placement,
            head.index,
        )
        .await
        .unwrap();
        let reader = super::super::super::fdb_read::ActiveMediaReader {
            catalog: &catalog,
            device: fixture.owner.devices[drive].dev.as_ref(),
            serial: &serial,
            route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        let mut old = reader
            .read_reclaim_with_cache(
                &reserved,
                &intent.request(),
                &fixture.owner.files.extent_cache_dir(),
                fixture.owner.files.spool_dir(),
            )
            .await
            .unwrap()
            .unwrap()
            .file;
        let mut body = Vec::new();
        old.read_to_end(&mut body).unwrap();
        assert_eq!(body, bytes);
        // A closed parent gate cannot re-admit the child, but the original
        // completed file result must still be readable before admission.
        let mut parent_gate = b"tape-rs/catalog/22/@/".to_vec();
        parent_gate.extend_from_slice(namespace.as_bytes());
        parent_gate.extend_from_slice(b"Rp");
        parent_gate.extend_from_slice(pool.id.as_bytes());

        let tr = raw_catalog.create_trx().unwrap();
        let gate_value = tr.get(&parent_gate, false).await.unwrap().unwrap().to_vec();
        tr.clear(&parent_gate);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .check_active_upload_gate(route, fixture.owner.round, &intent.request())
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .active_upload_result(&intent.request(), route)
                .await
                .unwrap(),
            Some(crate::managed_catalog::PublishOutcome::AlreadyApplied(
                copied
            ))
        );
        let previous_writes = fixture.writes();
        let previous_formats = fixture.formats();
        let previous_moves = fixture.changer_log().len();
        let previous_drive = fixture.drive_log().len();
        assert_eq!(
            Box::pin(fdb_writing::write(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &status,
                intent.request(),
                std::fs::File::open(&spool).unwrap(),
            ))
            .await
            .unwrap(),
            copied
        );
        assert_eq!(fixture.writes(), previous_writes);
        assert_eq!(fixture.formats(), previous_formats);
        assert!(
            fixture.changer_log()[previous_moves..]
                .iter()
                .all(|op| *op == 0x5e),
            "原终态只允许当前执行权的 PR 查询，不能重放机械操作"
        );
        assert!(
            fixture.drive_log()[previous_drive..]
                .iter()
                .all(|op| *op == 0x5e),
            "原终态不能重新装带、读取正文或写介质"
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert!(tr.get(&parent_gate, false).await.unwrap().is_none());
        tr.set(&parent_gate, &gate_value);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .advance_active_request_floor(
                    namespace,
                    &serial,
                    fixture.owner.round,
                    intent.request_epoch,
                    intent.request_epoch + 1
                )
                .await
                .unwrap(),
            crate::managed_catalog::WindowOutcome::Advanced(_)
        ));
        loop {
            if catalog
                .purge_active_expired_requests(namespace, &serial, fixture.owner.round, 16)
                .await
                .unwrap()
                == 0
            {
                break;
            }
        }
        assert_eq!(
            catalog
                .active_upload_result(&intent.request(), route)
                .await
                .unwrap(),
            Some(crate::managed_catalog::PublishOutcome::RequestExpired)
        );
        assert!(
            raw_catalog
                .create_trx()
                .unwrap()
                .get(&terminal_key, false)
                .await
                .unwrap()
                .is_none(),
            "必须实际清除普通 P 记录后再验证独立证明"
        );
        assert_eq!(
            catalog
                .verify_reclaim_file_metadata(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request(),
                    &verified.metadata,
                    verified.verified_head
                )
                .await
                .unwrap(),
            finished_walk,
            "普通请求结果清理后仍应保留原实读证明"
        );
        assert!(
            !catalog
                .retire_reclaim_file_source(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .unwrap(),
            "清理不能使已完成退休回执丢失"
        );
        assert_eq!(
            observer
                .seal_reclaim_data_relocation(
                    &reserved,
                    fixture.owner.round,
                    &scanned,
                    &intent.request()
                )
                .await
                .unwrap(),
            data_progress
        );
        assert!(
            catalog
                .release_reserved_reclaim(&reserved, fixture.owner.round)
                .await
                .is_err(),
            "有子写入时不能直接解锁，更不能授权格式化"
        );
        assert_eq!(fixture.formats(), 3, "复制不触发额外格式化");
        assert_eq!(
            catalog
                .active_namespace_visibility(&serial, route)
                .await
                .unwrap(),
            visibility_after_copy,
            "读缓存、监控和扫描物化不推进可见修订"
        );
        let old_round = fixture.owner.round;
        let next_round = old_round + 1;
        assert!(fixture.permit.renew(
            next_round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(60)
        ));
        let mut next_owner = takeover(
            &Provider {
                lib: fixture.lib.clone(),
                commands: fixture.commands.clone(),
            },
            &fixture.opts,
            next_round,
            Some(&fixture.permit),
            fixture.owner.files.clone(),
        )
        .unwrap();
        next_owner
            .devices
            .iter_mut()
            .find(|d| d.kind == DeviceKind::Changer)
            .unwrap()
            .serial = serial.clone();
        fixture.owner = next_owner;
        assert!(fixture.permit.renew(
            next_round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60)
        ));
        catalog
            .claim_active_owner_epoch(&serial, next_round)
            .await
            .unwrap();
        assert!(
            observer
                .seal_reclaim_data_relocation(&reserved, old_round, &scanned, &intent.request())
                .await
                .is_err()
        );
        assert!(
            observer
                .seal_reclaim_data_relocation(&reserved, next_round, &scanned, &intent.request())
                .await
                .is_err(),
            "先领取原父任务再恢复子证明"
        );
        assert!(
            observer
                .reclaim_metadata_input(&reserved, old_round, &scanned, &input_request,)
                .await
                .is_err()
        );
        assert!(
            observer
                .reclaim_metadata_input(&reserved, next_round, &scanned, &input_request,)
                .await
                .is_err(),
            "new owner must first claim the original parent"
        );
        let claimed = catalog
            .claim_reclaim_reservations_page(&serial, route, next_round, None, 16)
            .await
            .unwrap();
        assert_eq!(claimed.items.len(), 1);
        assert!(
            observer
                .reclaim_metadata_write_status(&reserved, old_round, source_data_uid)
                .await
                .is_err()
        );
        assert!(
            observer
                .reclaim_metadata_write_status(&claimed.items[0], next_round, source_data_uid)
                .await
                .is_err(),
            "父任务领取后仍须显式领取原元数据任务"
        );
        let claimed_write = catalog
            .claim_reclaim_metadata_write(&claimed.items[0], next_round, source_data_uid)
            .await
            .unwrap();
        assert_eq!(claimed_write.plan(), metadata_plan);
        assert_eq!(claimed_write.owner(), next_round);
        assert!(!claimed_write.closed());
        assert!(
            catalog
                .close_prepared_reclaim_metadata_write(
                    &claimed.items[0],
                    old_round,
                    &metadata_write
                )
                .await
                .is_err()
        );
        let closed = catalog
            .close_prepared_reclaim_metadata_write(&claimed.items[0], next_round, &claimed_write,
                )
            .await
            .unwrap();
        assert!(closed.closed());
        assert_eq!(closed.plan(), metadata_plan);
        assert_eq!(
            observer
                .prepare_reclaim_metadata_write(
                    &claimed.items[0],
                    next_round,
                    &scanned,
                    &input_request,
                    &metadata_plan
                )
                .await
                .unwrap(),
            closed,
            "关闭后的普通重试只返回原终态，不能隐式重开任务"
        );
        assert_eq!(
            observer
                .close_prepared_reclaim_metadata_write(
                    &claimed.items[0],
                    next_round,
                    &claimed_write
                )
                .await
                .unwrap(),
            closed
        );
        assert_eq!(
            observer
                .reclaim_metadata_write_status(&claimed.items[0], next_round, source_data_uid)
                .await
                .unwrap(),
            Some(closed.clone())
        );
        let tr = raw_catalog.create_trx().unwrap();
        assert!(tr.get(&metadata_active_key, false).await.unwrap().is_none());
        assert!(tr.get(&metadata_volume_key, false).await.unwrap().is_none());
        assert!(tr.get(&metadata_task_key, false).await.unwrap().is_some());
        // Idempotent closure must not clear resources subsequently owned by
        // another operation, even when the old reply was lost.
        tr.set(&metadata_active_key, b"other-operation");
        tr.set(&metadata_volume_key, b"other-operation");
        tr.commit().await.unwrap();
        assert_eq!(
            observer
                .close_prepared_reclaim_metadata_write(
                    &claimed.items[0],
                    next_round,
                    &claimed_write
                )
                .await
                .unwrap(),
            closed
        );
        let tr = raw_catalog.create_trx().unwrap();
        for key in [&metadata_active_key, &metadata_volume_key] {
            assert_eq!(
                tr.get(key, false).await.unwrap().unwrap().as_ref(),
                b"other-operation"
            );
            tr.clear(key);
        }
        tr.commit().await.unwrap();
        let drive_before_replay = fixture.drive_log();
        let changer_before_replay = fixture.changer_log();
        assert_eq!(
            observer
                .seal_reclaim_data_relocation(
                    &claimed.items[0],
                    next_round,
                    &scanned,
                    &intent.request()
                )
                .await
                .unwrap(),
            data_progress
        );
        assert_eq!(
            catalog
                .resume_reclaim_file_metadata(&reserved, next_round, &scanned, &intent.request())
                .await
                .unwrap(),
            Some(metadata_progress)
        );
        assert_eq!(
            observer
                .reclaim_metadata_input(&claimed.items[0], next_round, &scanned, &input_request,)
                .await
                .unwrap(),
            input,
            "HA keeps the exact source and donor without another body read"
        );
        assert_eq!(fixture.drive_log(), drive_before_replay);
        assert_eq!(fixture.changer_log(), changer_before_replay);
        });
        recovery_scenario.borrow_mut().push_back(phase);
    }
}
