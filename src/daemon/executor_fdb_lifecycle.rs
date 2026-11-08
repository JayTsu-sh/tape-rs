//! 原生 FDB 生命周期：持久意图在先，受保护的设备命令在后；不盲重放机械/格式化。
use super::*;
use crate::catalog::volume_extents::VolumeExtentCache;
use crate::changer::element::{ElementAddressMap, ElementStatus};
use crate::managed_catalog::{CatalogError, NativeDeviceTask};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
            Ok(vec![
                ManagedDevice {
                    name: "drive".into(),
                    serial: "SIMDRV0000".into(),
                    kind: DeviceKind::Drive,
                    dev: Box::new(Recorded {
                        inner: Box::new(self.lib.drive_as(0, 1)),
                        kind: DeviceKind::Drive,
                        commands: self.commands.clone(),
                    }),
                },
                ManagedDevice {
                    name: "changer".into(),
                    serial: "SIMLIB0000000001".into(),
                    kind: DeviceKind::Changer,
                    dev: Box::new(Recorded {
                        inner: Box::new(self.lib.changer_as(1)),
                        kind: DeviceKind::Changer,
                        commands: self.commands.clone(),
                    }),
                },
            ])
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
            let root =
                std::env::temp_dir().join(format!("native-lifecycle-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let lib = SimLibrary::new(1, 3, 1);
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
        use crate::managed_catalog::{PoolPolicy, PoolRecord, verify_native_barriers};
        let mut fixture = Fixture::new();
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
        let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false})).unwrap();
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
            assert_eq!(read.metadata.physical.path, ".LTFSEE_DATA/blob");
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
                let no_io = fixture.drive_log();
                assert!(
                    inventory
                        .bind_verified_metadata(
                            &reserved,
                            &intent.request(),
                            &source_copy,
                            (&verified.metadata, verified.verified_head),
                            &recorded
                        )
                        .is_err()
                );
                for case in 0..8 {
                    let mut bad = verified.metadata.clone();
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
                                (&verified.metadata, verified.verified_head),
                                &captured_target
                            )
                            .is_err()
                    );
                }
                assert_eq!(fixture.drive_log(), no_io, "拒绝证明不触碰设备");
            }
            catalog
                .verify_reclaim_file(
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
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim T2 前提/持久修尾意图/HA，无修尾写"]
    async fn reclaim_metadata_discard_intent_keeps_truncated_attempt_claims_across_ha() {
        Box::pin(native_lifecycle(MetadataCase::Execution(
            MetadataExecutionCase::DiscardIntent,
        )))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 Sim T1 前提/持久修尾意图/HA，无修尾写"]
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

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MetadataExecutionCase {
        Commit,
        LostAdmission,
        IpFailure,
        ClosingFailure,
        DiscardIntent,
        DiscardIntentT1,
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
                MetadataExecutionCase::IpFailure => Some(0x0a),
                MetadataExecutionCase::ClosingFailure
                | MetadataExecutionCase::DiscardIntent
                | MetadataExecutionCase::DiscardIntentT1 => Some(0x10),
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

    async fn native_lifecycle(metadata_case: MetadataCase) {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
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
                .verify_reclaim_file(
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
        let own_row = |kind: u8| {
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(namespace.as_bytes());
            key.extend_from_slice(&[b'R', kind]);
            key.extend_from_slice(&reserved.intent()["epoch"].as_u64().unwrap().to_be_bytes());
            key.extend_from_slice(
                uuid::Uuid::parse_str(reserved.intent()["operation"].as_str().unwrap())
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
                .verify_reclaim_file(
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
            .verify_reclaim_file(
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
                .verify_reclaim_file(
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
                .verify_reclaim_file(
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
        // Poll metadata validation separately from the deep ordinary-write
        // recovery setup; keep the default test/production stack unchanged.
        boxed_metadata_phase(|| async {
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
            boxed_metadata_phase(|| async {
                use crate::daemon::fdb_reclaim_media::ReclaimMetadataWriter;
                use crate::managed_catalog::ReclaimMetadataObservationKind;
                fixture.permit.renew(fixture.owner.round, Phase::Serving, Instant::now() + Duration::from_secs(60));
                let budget = crate::ltfs::recovery::RecoveryBudget::default();
                if mode == MetadataExecutionCase::Commit {
                    let raw = fixture.lib.drive_as(0, 1);
                    let mut unchecked = ReclaimMetadataWriter {
                        catalog: &catalog, device: &raw, route, key: fixture.owner.key,
                        permission: &fixture.permit, budget: &budget,
                    };
                    assert!(unchecked.execute(&reserved, &scanned, &input_request, &metadata_write, Some(&captured)).await.is_err(), "raw transport 必须在准入和设备访问前拒绝");
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
                foreign.renew(fixture.owner.round, Phase::Serving, Instant::now() + Duration::from_secs(60));
                let mut mismatched = ReclaimMetadataWriter {
                    catalog: &catalog, device: &device, route, key: fixture.owner.key,
                    permission: &foreign, budget: &budget,
                };
                assert!(mismatched.execute(&reserved, &scanned, &input_request, &metadata_write, Some(&captured)).await.is_err());
                assert_eq!(*fixture.commands.lock().unwrap(), original_commands, "异许可实例必须零设备 I/O");
                assert!(writer.execute(&reserved, &scanned, &input_request, &metadata_write, Some(&captured_target)).await.is_err(), "其他卷不是原源实读");
                assert_eq!(*fixture.commands.lock().unwrap(), original_commands);
                assert_eq!(catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap(), Some(metadata_write.clone()));
                if mode == MetadataExecutionCase::AttemptRegistry {
                    boxed_metadata_phase(|| async {
                        let commands=fixture.commands.lock().unwrap().clone();
                        let recovered=observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&metadata_write).await.unwrap();
                        assert_eq!(recovered,(scanned.clone(),input_request.clone()),"独立目录实例只按原尝试恢复原执行上下文");
                        let mut child_key=own_row(b'c');child_key.extend_from_slice(input_request.operation_id.as_bytes());
                        for key in [&child_key,&data_body_key,&data_destination_key,&retired_key] {
                            let tr=raw_catalog.create_trx().unwrap();let raw=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&metadata_write).await;
                            let tr=raw_catalog.create_trx().unwrap();let unchanged=tr.get(key,false).await.unwrap().is_none();tr.set(key,&raw);tr.commit().await.unwrap();
                            assert!(missing.is_err() && unchanged,"丢失原上下文证明必须拒绝且不修复");
                        }
                        let tr=raw_catalog.create_trx().unwrap();let original=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                        let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        let saved:Value=serde_json::from_slice(&original).unwrap();assert_eq!(saved["version"],5);assert_eq!(saved["context"]["file"],serde_json::to_value(&scanned).unwrap());
                        for (field,changed) in [("epoch",json!(0)),("epoch",json!(input_request.request_epoch+1)),("request",json!([3u8;32].to_vec()))] {
                            let mut corrupt=saved["context"].clone();corrupt[field]=changed;
                            // Keep the record's canonical field order. Recompute both
                            // checksums so these are semantic, not codec-only failures.
                            let context=format!(r#"{{"file":{},"epoch":{},"request":{}}}"#,serde_json::to_string(&scanned).unwrap(),corrupt["epoch"],corrupt["request"]);
                            let original_text=std::str::from_utf8(&original).unwrap();
                            let start=original_text.find("\"context\":").unwrap()+"\"context\":".len();let end=original_text.find(",\"owner\":").unwrap();
                            let raw=format!("{}{}{}",&original_text[..start],context,&original_text[end..]).into_bytes();
                            let h:Value=serde_json::from_slice(&header[..header.len()-32]).unwrap();
                            let digest=Sha256::new().chain_update(b"tape-rs/reclaim-metadata-task/5").chain_update(&raw).finalize().to_vec();
                            let mut h=format!(r#"{{"version":{},"uid":{},"attempt":{},"task_sha256":{}}}"#,h["version"],h["uid"],h["attempt"],json!(digest)).into_bytes();
                            let parent=format!("{:x}",Sha256::digest(reserved.intent().to_string().as_bytes()));
                            let hash=Sha256::new().chain_update(b"tape-rs/reclaim-metadata-registry/1").chain_update(parent.as_bytes()).chain_update(&h).finalize();h.extend_from_slice(&hash);
                            let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&raw);tr.set(&metadata_header_key,&h);tr.commit().await.unwrap();
                            let discovered=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                            let changed=if corrupt["epoch"]==0 {assert!(discovered.is_err());None} else {
                                let handle=discovered.unwrap().unwrap();
                                Some(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&handle).await)
                            };
                            let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(),raw.as_slice());tr.set(&metadata_task_key,&original);tr.set(&metadata_header_key,&header);tr.commit().await.unwrap();assert!(changed.is_none_or(|result|result.is_err()),"匹配外层摘要仍不得接受不同封存请求");
                        }
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"恢复或拒绝上下文不执行 CDB");
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(metadata_write.clone()));
                        assert!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_write).await.is_err());
                        let keys=[&metadata_active_key,&metadata_volume_key,&latest_metadata_key,&metadata_header_key,&metadata_task_key];
                        for key in keys {
                            let tr=raw_catalog.create_trx().unwrap();let original=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                            let prepare=catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_plan).await;
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
                        let admitted=catalog.admit_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_write).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();let writing=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();tr.set(&metadata_task_key,&prepared);tr.commit().await.unwrap();
                        let rollback=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&metadata_task_key,&writing);tr.commit().await.unwrap();assert!(rollback.is_err(),"保留检查点不能接受 Writing 回退 Prepared");
                        let observation=metadata_plan.observe_current(&captured_target,None).unwrap();
                        let observed=catalog.record_reclaim_metadata_observation(&reserved,fixture.owner.round,admitted.task(),&observation).await.unwrap();
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
                        assert!(catalog.retry_reclaim_metadata_write(parent,next,&scanned,&input_request,&claimed).await.is_err(),"Writing 不能发新尝试");
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"发现/领取/拒绝均无设备命令");
                    }).await;
                    return;
                }
                if mode == MetadataExecutionCase::AttemptHistory {
                    boxed_metadata_phase(|| async {
                        let commands=fixture.commands.lock().unwrap().clone();
                        let first=catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();let first_raw=tr.get(&metadata_task_key,false).await.unwrap().unwrap().to_vec();
                        let (one,two)=tokio::join!(Box::pin(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&first)),Box::pin(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&first)));
                        let second=one.unwrap();assert_eq!(two.unwrap(),second);assert_ne!(second.id(),first.id());assert_eq!(second.first_attempt(),first.id());assert_eq!(second.previous_attempt(),Some(first.id()));
                        assert_eq!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.unwrap(),first);
                        assert_eq!(catalog.prepare_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&metadata_plan).await.unwrap(),first,"首次准备重放不能变成最新尝试");
                        let second_closed=catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&second).await.unwrap();
                        let third=catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&second_closed).await.unwrap();
                        assert_ne!(third.id(),second.id());assert_eq!(third.first_attempt(),first.id());assert_eq!(third.previous_attempt(),Some(second.id()));
                        assert_eq!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&third).await.unwrap(),(scanned.clone(),input_request.clone()),"后继保持原执行上下文");
                        assert!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&first).await.is_err(),"旧终态不能恢复成新尝试的执行上下文");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&first).await.unwrap(),second_closed,"旧重试始终返回原后继，不返回第三次");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&second_closed).await.unwrap(),third);
                        assert_eq!(catalog.reclaim_metadata_attempt(&reserved,fixture.owner.round,source_data_uid,first.id()).await.unwrap(),first);
                        assert_eq!(catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap(),Some(third.clone()));
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(third.clone()));
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(),first_raw.as_slice());
                        let mut edge=own_row(b'U');edge.extend_from_slice(&source_data_uid.to_be_bytes());edge.extend_from_slice(second.id().as_bytes());
                        let original=tr.get(&edge,false).await.unwrap().unwrap().to_vec();assert_eq!(original,third.id().as_bytes());tr.clear(&edge);tr.commit().await.unwrap();
                        let missing=catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await;
                        let retry=catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&second_closed).await;
                        let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(&edge,false).await.unwrap().is_none();tr.set(&edge,&original);tr.commit().await.unwrap();assert!(missing.is_err() && retry.is_err() && untouched,"丢失后继边不能重建或另发请求");
                        assert!(writer.execute(&reserved,&scanned,&input_request,&metadata_write,Some(&captured)).await.is_err(),"旧物理尝试不能解释成新尝试");
                        assert_eq!(*fixture.commands.lock().unwrap(),commands);
                        let execution=writer.execute(&reserved,&scanned,&input_request,&third,Some(&captured)).await.unwrap();
                        let published=catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,execution.task(),execution.commit_evidence().unwrap()).await.unwrap();
                        assert_eq!(published.id(),third.id());
                        let completed=catalog.reclaim_metadata_attempt(&reserved,fixture.owner.round,source_data_uid,third.id()).await.unwrap();assert!(completed.completed());
                        assert!(observer.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&completed).await.is_err(),"已发布仅查历史，不再执行恢复");
                        assert!(observer.reclaim_metadata_retry_context(&reserved,fixture.owner.round,&completed).await.is_err(),"已发布不能准备新物理重试");
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&first).await.unwrap(),second_closed);
                        assert_eq!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&second_closed).await.unwrap(),completed);
                        let mut rows=Vec::new();let tr=raw_catalog.create_trx().unwrap();
                        for id in [first.id(),second.id(),third.id()] {let mut key=own_row(b'W');key.extend_from_slice(&source_data_uid.to_be_bytes());key.extend_from_slice(id.as_bytes());rows.push((key.clone(),tr.get(&key,false).await.unwrap().unwrap().to_vec()));tr.clear(&key);}
                        for key in [&latest_metadata_key,&metadata_header_key,&own_row(b'm')] {rows.push((key.clone(),tr.get(key,false).await.unwrap().unwrap().to_vec()));tr.clear(key);}
                        tr.commit().await.unwrap();
                        assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),published.observation().current(),"后继发布历史不依赖原尝试行/检查点/源清单");
                        let tr=raw_catalog.create_trx().unwrap();for (key,raw) in &rows {tr.set(key,raw);}tr.commit().await.unwrap();
                        let commands=fixture.commands.lock().unwrap().clone();let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                        let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();let parent=&parents.items[0];
                        let current=catalog.claim_reclaim_metadata_attempt(parent,next,&completed).await.unwrap();assert_eq!(current.id(),third.id());
                        let tr=raw_catalog.create_trx().unwrap();let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        let old=catalog.claim_reclaim_metadata_attempt(parent,next,&second_closed).await.unwrap();
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_header_key,false).await.unwrap().unwrap().as_ref(),header.as_slice(),"历史领取不能替换当前检查点");
                        assert_eq!(catalog.retry_reclaim_metadata_write(parent,next,&scanned,&input_request,&first).await.unwrap(),old);
                        assert_eq!(catalog.retry_reclaim_metadata_write(parent,next,&scanned,&input_request,&old).await.unwrap(),current);
                        assert_eq!(catalog.reclaim_metadata_publication(parent,next,&scanned,&input_request,&third).await.unwrap(),Some(published));
                        assert_eq!(catalog.reclaim_metadata_attempt(parent,next,source_data_uid,first.id()).await.unwrap(),first);
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"FDB HA 历史查询/领取不发设备命令");
                    }).await;
                    return;
                }
                if matches!(mode, MetadataExecutionCase::LostAdmission | MetadataExecutionCase::DiscardIntentFromUnchanged | MetadataExecutionCase::UnwrittenClose | MetadataExecutionCase::UnwrittenIpDebt | MetadataExecutionCase::ClosedRetryContext | MetadataExecutionCase::UnwrittenIpMissing | MetadataExecutionCase::UnwrittenFraming | MetadataExecutionCase::UnwrittenIpAhead) {
                    catalog.admit_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &input_request, &metadata_write).await.unwrap();
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
                    });
                    assert_eq!(changed, 1, "实际只构造 IP 更高代次，DP 不变");
                }
                if mode == MetadataExecutionCase::UnwrittenIpMissing {
                    fixture.lib.with_cartridge_mut(&input.destination().placement().barcode, |cartridge| {
                        let ip = &mut cartridge.partitions[0];
                        ip.objects.truncate(crate::ltfs::volume::P1_DATA_START as usize + 1);
                        assert!(matches!(ip.objects.last(), Some(crate::scsi::sim::LogicalObject::Filemark)));
                        ip.flushed = ip.objects.len();
                        cartridge.vcr += 1;
                    });
                }
                if mode == MetadataExecutionCase::UnwrittenFraming {
                    resegment_metadata_fixture(&fixture, drive, &input.destination().placement().barcode, &captured_target);
                }
                let debt_media = matches!(mode, MetadataExecutionCase::UnwrittenIpDebt | MetadataExecutionCase::ClosedRetryContext | MetadataExecutionCase::UnwrittenIpMissing | MetadataExecutionCase::UnwrittenFraming).then(|| {
                    let crate::ltfs::recovery::IndexSourceScope::Current { ip_generation: Some(ip), .. } = captured_target.scope() else {panic!("债务夹具必须有已核验完整旧 IP")};
                    assert!(ip < captured_target.construct().generation, "必须保留真实普通写入的IP债务，不能先做checkpoint");
                    fixture.lib.cartridge(&input.destination().placement().barcode).unwrap()
                });
                let first = writer.execute(&reserved, &scanned, &input_request, &metadata_write,
                    if matches!(mode, MetadataExecutionCase::LostAdmission | MetadataExecutionCase::DiscardIntentFromUnchanged | MetadataExecutionCase::UnwrittenClose | MetadataExecutionCase::UnwrittenIpDebt | MetadataExecutionCase::ClosedRetryContext | MetadataExecutionCase::UnwrittenIpMissing | MetadataExecutionCase::UnwrittenFraming | MetadataExecutionCase::UnwrittenIpAhead) {None} else {Some(&captured)}).await;
                if mode == MetadataExecutionCase::UnwrittenIpAhead {
                    assert!(matches!(first, Err(TapeError::RecoveryRestricted { ref reason })
                        if reason.contains("IP 索引代数") && reason.contains("领先 FDB 已观察 DP")), "{first:?}");
                    let task = catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap().unwrap();
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
                    assert_stale_metadata_staging_rejected(&fixture,drive,&metadata_plan,&captured,&captured_target);
                    let task = catalog.reclaim_metadata_write_status(&reserved,fixture.owner.round,source_data_uid).await.unwrap().unwrap();
                    assert!(task.writing() && task.observation().unwrap().is_none());
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),crate::managed_catalog::ActiveIndexIdentity::from(captured_target.construct()));
                    let tr=raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}
                    assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
                    assert!(!fixture.commands.lock().unwrap()[original_commands.len()..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)), "物理边界变化只能拒绝，不写/修尾/搬带");
                    return;
                }
                if mode == MetadataExecutionCase::UnwrittenIpMissing {
                    let execution = first.unwrap();
                    assert!(execution.task().writing() && execution.unwritten_evidence().is_none() && execution.commit_evidence().is_none());
                    assert_eq!(execution.task().observation().unwrap().unwrap().kind(),ReclaimMetadataObservationKind::Unchanged);
                    let tr = raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}
                    assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
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
                    assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&changed_request,task,proof).await.is_err());
                    for key in [&metadata_active_key,&metadata_volume_key] {
                        let tr=raw_catalog.create_trx().unwrap();tr.clear(key);tr.commit().await.unwrap();
                        assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert!(tr.get(key,false).await.unwrap().is_none());
                        tr.set(key,&active);tr.commit().await.unwrap();
                    }
                    let mut next_history=b"tape-rs/catalog/22/r/".to_vec();next_history.extend_from_slice(metadata_plan.volume().as_bytes());next_history.extend_from_slice(&(index.generation+1).to_be_bytes());
                    let mut next_count=next_history.clone();next_count.push(b'f');
                    let mut next_receipt=own_row(b'H');next_receipt.truncate(next_receipt.len()-24);next_receipt.extend_from_slice(metadata_plan.volume().as_bytes());next_receipt.extend_from_slice(&(index.generation+1).to_be_bytes());
                    for key in [&next_history,&next_count,&next_receipt] {
                        let tr=raw_catalog.create_trx().unwrap();tr.set(key,b"unknown-original-result");tr.commit().await.unwrap();
                        assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),b"unknown-original-result");tr.clear(key);tr.commit().await.unwrap();
                    }
                    let (one,two)=tokio::join!(Box::pin(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof)),Box::pin(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof)));
                    let closed=one.unwrap();assert_eq!(two.unwrap(),closed);assert!(closed.closed());assert_eq!(closed.id(),task.id());assert_eq!(closed.observation().unwrap().as_ref(),Some(proof.observation()));
                    assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),index);
                    let tr=raw_catalog.create_trx().unwrap();for (key,before) in coverage_keys.iter().zip(&coverage_before) {assert_eq!(&tr.get(key,false).await.unwrap().map(|raw|raw.to_vec()),before,"NoWrite关闭不能增加计数或源覆盖");}
                    let tr=raw_catalog.create_trx().unwrap();assert!(tr.get(&metadata_active_key,false).await.unwrap().is_none());assert!(tr.get(&metadata_volume_key,false).await.unwrap().is_none());
                    for key in [&next_history,&next_count,&next_receipt] {assert!(tr.get(key,false).await.unwrap().is_none());}
                    tr.set(&metadata_active_key,b"later-original-task");tr.set(&metadata_volume_key,b"later-original-task");tr.commit().await.unwrap();
                    assert_eq!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof).await.unwrap(),closed);
                    assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&changed_request,task,proof).await.is_err());
                    let tr=raw_catalog.create_trx().unwrap();for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),b"later-original-task");tr.clear(key);}tr.commit().await.unwrap();
                    assert!(writer.execute(&reserved,&scanned,&input_request,&closed,None).await.is_err());
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
                        let (one,two)=tokio::join!(Box::pin(restarted.retry_reclaim_metadata_write(parent,next,&restored_file,&restored_request,&recovered)),Box::pin(restarted.retry_reclaim_metadata_write(parent,next,&restored_file,&restored_request,&recovered)));
                        let successor=one.unwrap();assert_eq!(two.unwrap(),successor);assert_ne!(successor.id(),recovered.id());assert_eq!(successor.previous_attempt(),Some(recovered.id()));assert_eq!(successor.first_attempt(),recovered.first_attempt());
                        assert!(!successor.closed() && !successor.writing() && !successor.completed());
                        assert_eq!(restarted.reclaim_metadata_checkpoint(parent,next).await.unwrap(),Some(successor.clone()));
                        assert_eq!(restarted.pending_reclaim_metadata_write(parent,next).await.unwrap(),Some(successor.clone()));
                        assert!(restarted.reclaim_metadata_retry_context(parent,next,&successor).await.is_err(),"Prepared不能充当Closed重试参数");
                        assert_eq!(restarted.reclaim_metadata_execution_context(parent,next,&successor).await.unwrap(),(restored_file.clone(),restored_request.clone()));
                        let tr=raw_catalog.create_trx().unwrap();let held=tr.get(&metadata_active_key,false).await.unwrap().unwrap().to_vec();let header=tr.get(&metadata_header_key,false).await.unwrap().unwrap().to_vec();
                        assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());
                        assert_eq!(restarted.reclaim_metadata_retry_context(parent,next,&recovered).await.unwrap(),(restored_file.clone(),restored_request.clone()));
                        assert_eq!(restarted.retry_reclaim_metadata_write(parent,next,&restored_file,&restored_request,&recovered).await.unwrap(),successor,"旧重试不能重复创建后继");
                        assert!(restarted.close_unwritten_reclaim_metadata_write(parent,next,&restored_file,&restored_request,&recovered,proof).await.is_err(),"旧owner实读证明不能授接任许可");
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(),held.as_slice());assert_eq!(tr.get(&metadata_header_key,false).await.unwrap().unwrap().as_ref(),header.as_slice());
                        assert_eq!(restarted.reclaim_metadata_attempt(parent,next,recovered.plan().source_uid(),recovered.id()).await.unwrap(),recovered);
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"独立catalog只从parent恢复、校验、领取及显式重试不发送CDB");
                        assert_reclaim_media_unchanged(debt_media.as_ref().unwrap(),&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
                        return;
                    }
                    let recovered=catalog.claim_reclaim_metadata_write(&parents.items[0],next,source_data_uid).await.unwrap();
                    assert!(recovered.closed());assert_eq!(recovered.id(),task.id());
                    assert!(catalog.close_unwritten_reclaim_metadata_write(&parents.items[0],next,&scanned,&input_request,&recovered,proof).await.is_err(),"旧owner实读证明不能重绑");
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
                    let task = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap().unwrap();
                    assert!(task.writing() && task.observation().unwrap().is_none());
                    assert!(writer.execute(&reserved, &scanned, &input_request, &task, None).await.is_err());
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
                    assert!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &input_request, task, proof).await.is_err());
                    let tr = raw_catalog.create_trx().unwrap();
                    assert_eq!(tr.get(&metadata_task_key,false).await.unwrap().unwrap().as_ref(), corrupt.as_slice());
                    tr.set(&metadata_task_key, &original); tr.commit().await.unwrap();
                    let mut history_key=b"tape-rs/catalog/22/r/".to_vec();
                    history_key.extend_from_slice(metadata_plan.volume().as_bytes());
                    history_key.extend_from_slice(&proof.observation().current().generation.to_be_bytes());
                    let mut count_key=history_key.clone();count_key.push(b'f');
                    let mut receipt_key=own_row(b'H');receipt_key.truncate(receipt_key.len()-24);
                    receipt_key.extend_from_slice(metadata_plan.volume().as_bytes());
                    receipt_key.extend_from_slice(&proof.observation().current().generation.to_be_bytes());
                    for key in [&history_key,&count_key,&receipt_key] {
                        let tr=raw_catalog.create_trx().unwrap();
                        assert!(tr.get(key,false).await.unwrap().is_none());
                        tr.set(key,b"unexplained-original-slot");tr.commit().await.unwrap();
                        assert!(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof).await.is_err());
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
                        Box::pin(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof)),
                        Box::pin(catalog.publish_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,task,proof))
                    );
                    let published=first.unwrap();
                    assert_eq!(concurrent.unwrap(),published,"并发发布仅推进一次原代次");
                    assert_eq!(published.id(), task.id());
                    assert_eq!(published.observation(), proof.observation());
                    let completed = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap().unwrap();
                    assert!(completed.completed() && !completed.writing() && !completed.closed());
                    assert_eq!(completed.id(), task.id());
                    assert_eq!(catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.unwrap(), proof.observation().current());
                    assert_eq!(catalog.active_volume_file_count(&serial, route, fixture.owner.round, metadata_plan.volume()).await.unwrap(), proof.observation().physical_files());
                    let tr = raw_catalog.create_trx().unwrap();
                    assert!(tr.get(&metadata_active_key,false).await.unwrap().is_none());
                    assert!(tr.get(&metadata_volume_key,false).await.unwrap().is_none());
                    assert_eq!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &input_request, task, proof).await.unwrap(), published, "丢失发布回执只重读原发布，不再计数/释放");
                    let mut changed_request = input_request.clone();
                    changed_request.operation_id = uuid::Uuid::new_v4();
                    assert!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &changed_request, task, proof).await.is_err(), "已发布重试仍须绑定原对象请求");
                    assert_eq!(catalog.reclaim_metadata_publication(&reserved, fixture.owner.round, &scanned, &input_request, &metadata_write).await.unwrap(), Some(published.clone()));
                    let replay = writer.execute(&reserved, &scanned, &input_request, &metadata_write, None).await.unwrap();
                    assert!(replay.task().completed() && replay.commit_evidence().is_none());
                    assert_eq!(*fixture.commands.lock().unwrap(), commands, "目录发布和已发布重放无设备命令");
                    let tr = raw_catalog.create_trx().unwrap();
                    tr.set(&metadata_active_key, b"another-original-task");
                    tr.set(&metadata_volume_key, b"another-original-task");
                    tr.commit().await.unwrap();
                    assert!(catalog.close_prepared_reclaim_metadata_write(&reserved, fixture.owner.round, &metadata_write).await.is_err());
                    assert_eq!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &input_request, task, proof).await.unwrap(), published);
                    let tr = raw_catalog.create_trx().unwrap();
                    for key in [&metadata_active_key,&metadata_volume_key] {
                        assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(), b"another-original-task");
                        tr.clear(key);
                    }
                    tr.set(&pool_key, &pool_bytes); tr.commit().await.unwrap();
                    let tr = raw_catalog.create_trx().unwrap();
                    let receipt = tr.get(&receipt_key,false).await.unwrap().unwrap().to_vec();
                    for replacement in [None, Some({let mut b=receipt.clone(); b[5]^=1;b})] {
                        let tr = raw_catalog.create_trx().unwrap();
                        if let Some(raw)=&replacement { tr.set(&receipt_key,raw); } else { tr.clear(&receipt_key); }
                        tr.commit().await.unwrap();
                        assert!(catalog.observed_active_volume_index(&serial, metadata_plan.volume()).await.is_err());
                        assert!(catalog.publish_reclaim_metadata_write(&reserved, fixture.owner.round, &scanned, &input_request, task, proof).await.is_err());
                        let tr = raw_catalog.create_trx().unwrap();
                        assert_eq!(tr.get(&receipt_key,false).await.unwrap().as_deref(), replacement.as_deref());
                        tr.set(&receipt_key,&receipt);tr.commit().await.unwrap();
                    }
                    let tr=raw_catalog.create_trx().unwrap();
                    let mut ledger_rows=Vec::new();
                    for key in [&metadata_task_key,&latest_metadata_key,&own_row(b'm')] {
                        ledger_rows.push((key.clone(),tr.get(key,false).await.unwrap().unwrap().to_vec()));
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
                    let mut read = reader.read_with_cache(path.as_bytes(), &fixture.owner.files.extent_cache_dir(), fixture.owner.files.spool_dir()).await.unwrap().unwrap();
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
                    assert_eq!(catalog.reclaim_metadata_publication(&parents.items[0],next,&scanned,&input_request,&recovered).await.unwrap(),Some(published));
                    assert!(catalog.publish_reclaim_metadata_write(&parents.items[0],next,&scanned,&input_request,&recovered,proof).await.is_err(),"旧owner实读token不可重绑新owner");
                    assert_eq!(*fixture.commands.lock().unwrap(),commands,"接任已发布结果不发设备命令");
                    return;
                }
                if matches!(mode, MetadataExecutionCase::DiscardIntent | MetadataExecutionCase::DiscardIntentT1 | MetadataExecutionCase::DiscardIntentFromUnchanged) {
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
                        assert!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&wrong_request,&task,&proof).await.is_err());
                        for key in [&metadata_active_key,&metadata_volume_key,&data_body_key] {
                            let tr=raw_catalog.create_trx().unwrap();let raw=tr.get(key,false).await.unwrap().unwrap().to_vec();tr.clear(key);tr.commit().await.unwrap();
                            let missing=catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof).await;
                            let tr=raw_catalog.create_trx().unwrap();let untouched=tr.get(key,false).await.unwrap().is_none();tr.set(key,&raw);tr.commit().await.unwrap();
                            assert!(missing.is_err() && untouched,"丢原占用/输入不补建或另发意图");
                        }
                        let index=catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap();
                        let count=catalog.active_volume_file_count(&serial,route,fixture.owner.round,metadata_plan.volume()).await.unwrap();
                        let mut next_key=b"tape-rs/catalog/22/r/".to_vec();next_key.extend_from_slice(metadata_plan.volume().as_bytes());next_key.extend_from_slice(&(index.generation+1).to_be_bytes());
                        let tr=raw_catalog.create_trx().unwrap();tr.set(&next_key,b"unknown-result");tr.commit().await.unwrap();
                        assert!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof).await.is_err());
                        let tr=raw_catalog.create_trx().unwrap();assert_eq!(tr.get(&next_key,false).await.unwrap().unwrap().as_ref(),b"unknown-result");tr.clear(&next_key);tr.set(&pool_key,&full_pool);tr.commit().await.unwrap();
                        let (one,two)=tokio::join!(Box::pin(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof)),Box::pin(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof)));
                        let discarding=one.unwrap();assert_eq!(two.unwrap(),discarding,"并发只保存一个原修尾意图");
                        assert!(discarding.discarding() && !discarding.writing() && !discarding.closed() && !discarding.completed());
                        assert_eq!(discarding.id(),task.id());assert_eq!(discarding.plan(),task.plan());
                        assert_eq!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof).await.unwrap(),discarding,"未知准入结果保留同一phase/attempt");
                        assert!(catalog.admit_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&task).await.is_err());
                        assert!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&metadata_write).await.is_err(),"Prepared旧回执不能关闭Discarding");
                        assert!(catalog.close_prepared_reclaim_metadata_write(&reserved,fixture.owner.round,&discarding).await.is_err());
                        if let Some(earlier)=&earlier {
                            assert!(catalog.close_unwritten_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,earlier.task(),earlier.unwritten_evidence().unwrap()).await.is_err(),"旧Writing阶段未写token不能释放Discarding占用");
                        }
                        assert!(catalog.retry_reclaim_metadata_write(&reserved,fixture.owner.round,&scanned,&input_request,&discarding).await.is_err());
                        assert!(catalog.reclaim_metadata_retry_context(&reserved,fixture.owner.round,&discarding).await.is_err());
                        let observation=metadata_plan.observe_current(&captured_target,None).unwrap();
                        assert!(catalog.record_reclaim_metadata_observation(&reserved,fixture.owner.round,&task,&observation).await.is_err());
                        assert_eq!(catalog.reclaim_metadata_execution_context(&reserved,fixture.owner.round,&discarding).await.unwrap(),(scanned.clone(),input_request.clone()));
                        assert_eq!(catalog.pending_reclaim_metadata_write(&reserved,fixture.owner.round).await.unwrap(),Some(discarding.clone()));
                        assert_eq!(catalog.observed_active_volume_index(&serial,metadata_plan.volume()).await.unwrap(),index);
                        assert_eq!(catalog.active_volume_file_count(&serial,route,fixture.owner.round,metadata_plan.volume()).await.unwrap(),count);
                        let tr=raw_catalog.create_trx().unwrap();for key in [&metadata_active_key,&metadata_volume_key] {assert_eq!(tr.get(key,false).await.unwrap().unwrap().as_ref(),active.as_slice());}tr.set(&pool_key,&pool_bytes);tr.commit().await.unwrap();
                        assert!(writer.execute(&reserved,&scanned,&input_request,&task,None).await.is_err(),"普通writer不能接续Discarding");
                        let next=fixture.owner.round+100;catalog.claim_active_owner_epoch(&serial,next).await.unwrap();
                        let parents=catalog.claim_reclaim_reservations_page(&serial,route,next,None,16).await.unwrap();
                        let saved=observer.reclaim_metadata_checkpoint(&parents.items[0],next).await.unwrap().unwrap();assert_eq!(saved,discarding);
                        let recovered=catalog.claim_reclaim_metadata_attempt(&parents.items[0],next,&saved).await.unwrap();assert!(recovered.discarding());assert_eq!(recovered.id(),task.id());
                        assert_eq!(observer.reclaim_metadata_execution_context(&parents.items[0],next,&recovered).await.unwrap(),(scanned.clone(),input_request.clone()));
                        assert!(catalog.begin_reclaim_metadata_discard(&reserved,fixture.owner.round,&scanned,&input_request,&task,&proof).await.is_err());
                        assert!(catalog.begin_reclaim_metadata_discard(&parents.items[0],next,&scanned,&input_request,&recovered,&proof).await.is_err(),"旧owner前提不能重绑接任");
                        assert!(catalog.close_prepared_reclaim_metadata_write(&parents.items[0],next,&recovered).await.is_err());
                        assert_eq!(catalog.pending_reclaim_metadata_write(&parents.items[0],next).await.unwrap(),Some(recovered));
                        assert_eq!(*fixture.commands.lock().unwrap(),commands,"FDB意图/上下文/HA均无设备命令");
                        assert_reclaim_media_unchanged(&media,&fixture.lib.cartridge(&input.destination().placement().barcode).unwrap());
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
                    let retained = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap().unwrap();
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
                let replay = writer.execute(&reserved, &scanned, &input_request, &metadata_write, None).await;
                if matches!(mode, MetadataExecutionCase::ClosingFailure | MetadataExecutionCase::PermissionAfterWrite) {
                    assert!(replay.is_err(), "截断尾部不能观察为完成或自动修复");
                } else {
                    let replay = replay.unwrap();
                    let observed = replay.task().observation().unwrap().unwrap();
                    let expected = if mode == MetadataExecutionCase::LostAdmission {ReclaimMetadataObservationKind::Unchanged} else {ReclaimMetadataObservationKind::Committed};
                    assert_eq!(observed.kind(), expected);
                    let again = writer.execute(&reserved, &scanned, &input_request, replay.task(), None).await.unwrap();
                    assert_eq!(again, replay);
                }
                assert!(!fixture.commands.lock().unwrap()[before_retry..].iter().any(|(_,op)|matches!(op,0x04|0x0a|0x10|0x19|0x8d|0xa5)), "任何重试只读，不格式化/重写/MAM/搬带");
                let task = catalog.reclaim_metadata_write_status(&reserved, fixture.owner.round, source_data_uid).await.unwrap().unwrap();
                assert!(task.writing());
                assert!(catalog.close_prepared_reclaim_metadata_write(&reserved, fixture.owner.round, &task).await.is_err());
                let tr = raw_catalog.create_trx().unwrap();
                assert_eq!(tr.get(&metadata_active_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                assert_eq!(tr.get(&metadata_volume_key,false).await.unwrap().unwrap().as_ref(), active.as_slice());
                let before_fence = fixture.commands.lock().unwrap().clone();
                fixture.permit.close(fixture.owner.round);
                assert!(writer.execute(&reserved, &scanned, &input_request, &task, None).await.is_err());
                assert_eq!(*fixture.commands.lock().unwrap(), before_fence, "许可失效不再发送设备命令");
            }).await;
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
                    &scanned,
                    &input_request,
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
                                return Err(TapeError::Catalog("Sim 元数据观察原任务改变".into()));
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
                            &scanned,
                            &input_request,
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
                            &scanned,
                            &input_request,
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
                        &scanned,
                        &input_request,
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
                        &scanned,
                        &input_request,
                        &metadata_write
                    )),
                    Box::pin(observer.admit_reclaim_metadata_write(
                        &reserved,
                        fixture.owner.round,
                        &scanned,
                        &input_request,
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
                            &scanned,
                            &input_request,
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
                        .admit_reclaim_metadata_write(
                            &reserved,
                            fixture.owner.round,
                            &scanned,
                            &changed_request,
                            &writing
                        )
                        .await
                        .is_err(),
                    "观察原结果也不能接受另一个子请求"
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
                            &scanned,
                            &input_request,
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
                                &scanned,
                                &input_request,
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
                            &scanned,
                            &input_request,
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
                            &scanned,
                            &input_request,
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
                .verify_reclaim_file(
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
            .close_prepared_reclaim_metadata_write(&claimed.items[0], next_round, &claimed_write)
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
        }).await;
    }
}
