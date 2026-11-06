//! 原生 FDB 生命周期：持久意图在先，受保护的设备命令在后；不盲重放机械/格式化。
use super::*;
#[path = "executor_fdb_lifecycle_cancel.rs"]
mod cancel;
#[path = "executor_fdb_format_guard.rs"]
mod format_guard;
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
    if ownership_lost(&error) {
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
    fdb_loading::guard(owner, permit, control).await?;
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
fn elements(
    owner: &Active,
) -> std::result::Result<(ElementAddressMap, Vec<ElementStatus>), ReadError> {
    let changer = owner
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .ok_or_else(|| failed("没有换带器"))?;
    let mut changer = MediumChanger::new(changer.dev.as_ref());
    let map = changer.load_address_map().map_err(physical)?.clone();
    let rows = changer.read_all_status().map_err(physical)?;
    inventory_elements(&owner.devices, &rows, true).map_err(physical)?;
    Ok((map, rows))
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
fn checked_move(
    owner: &Active,
    plan: &Plan,
    intent: MoveIntent,
) -> std::result::Result<(), ReadError> {
    let (_, rows) = elements(owner)?;
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
    move_medium(&owner.devices, current.address, target.address).map_err(physical)?;
    moved(&elements(owner)?.1, plan)
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
    let (map, rows) = elements(owner)?;
    let barcode = task.barcode().ok_or_else(|| failed("介质任务缺少条码"))?;
    let item = cartridge(&rows, barcode)?;
    let inv = inventory_elements(&owner.devices, &rows, true).map_err(physical)?;
    let assignment = control
        .catalog
        .tape_assignment(&control.serial, barcode)
        .await
        .map_err(lost)?;
    // start_native_media 已以同一任务建立 volume=None 的预留归属；它不是已发布卷。
    if (kind(task) == "media-assign"
        && !assignment.as_ref().is_some_and(|a| {
            a.namespace == task.request().namespace()
                && Some(a.pool_id) == task.pool()
                && a.volume_id.is_none()
        }))
        || (kind(task) != "media-assign"
            && (assignment.as_ref().map(|a| a.pool_id) != task.pool()
                || assignment
                    .as_ref()
                    .is_some_and(|a| a.volume_id != plan.volume)))
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
                if !owner
                    .fdb_reads
                    .get(&drive.dev)
                    .is_some_and(|m| m.barcode == barcode && Some(m.volume) == plan.volume)
                {
                    return Err(failed("目标在非本轮已核验挂载中，不隐式卸带"));
                }
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
        return Err(failed("清洗带不能初始化或校验 LTFS"));
    }
    if item.element_type == ElementType::ImportExport
        || (kind(task) == "media-assign" && item.element_type != ElementType::Storage)
    {
        return Err(failed("维护前磁带必须归槽"));
    }
    let pools: Vec<_> = task.pool().into_iter().collect();
    let policy = fdb_loading::policy(owner, control, &pools, Some(barcode)).await?;
    let writable = kind(task) == "media-assign";
    if writable {
        let pool = task
            .pool()
            .ok_or_else(|| failed("assign 缺少池"))?
            .to_string();
        let restriction = policy["pools"][&pool]["mediarestriction"]
            .as_str()
            .ok_or_else(|| lost("池介质限制缺失"))?;
        if restriction != "none" && !barcode.ends_with(restriction) {
            return Err(failed("目标介质不满足池密度限制"));
        }
    } else if !assignment
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
            && !owner
                .fdb_reads
                .get(&drive.dev)
                .is_some_and(|m| m.barcode == *b && Some(m.volume) == plan.volume)
        {
            continue;
        }
        let capabilities = TapeDrive::new(owner.devices[drive.dev].dev.as_ref())
            .report_density_support(false)
            .map_err(physical)?;
        if super::super::scheduling::densities(&capabilities)
            .is_some_and(|c| super::super::scheduling::compatible(&c, barcode, writable))
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
    let (_, rows) = elements(owner)?;
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
    TapeDrive::new(owner.devices[drive].dev.as_ref())
        .unload()
        .map_err(physical)?;
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
    let value = management::observe(owner, opts).map_err(physical)?;
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
    let (_, rows) = elements(owner)?;
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
        checked_move(owner, plan, MoveIntent::NewlyIssued)?;
        live(owner, permit, control, task).await?;
        moved(&elements(owner)?.1, plan)?;
    }
    TapeDrive::new(owner.devices[drive].dev.as_ref())
        .load()
        .map_err(physical)?;
    wait_ready(owner.devices[drive].dev.as_ref()).map_err(physical)?;
    live(owner, permit, control, task).await?;
    moved(&elements(owner)?.1, plan)?;
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
    assign: bool,
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
    let mut result = json!({"volume":volume.to_string(),"generation":head.generation,"index_sha256":head.sha256.iter().map(|b|format!("{b:02x}")).collect::<String>(),"files":files,"scope":"label_and_index","payload_verified":false});
    if assign {
        if files != 0 || has_children || marker.is_none() || pool.is_none() {
            return Err(failed("分配恢复尚未确认完整空卷和 pool 标记"));
        }
        let barcode = crate::ltfs::mam::Mam::new(device)
            .read_attribute(crate::ltfs::mam::ATTR_BARCODE)
            .map_err(physical)?
            .ok_or_else(|| failed("格式化后的 MAM barcode 缺失"))?;
        if barcode.value.iter().all(|b| matches!(*b, b' ' | 0)) {
            return Err(failed("格式化后的 MAM barcode 为空"));
        }
        result["pool_mark_verified"] = json!(true);
    }
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
            let (map, rows) = elements(owner)?;
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
            let (_, rows) = elements(owner)?;
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
                checked_move(owner, &step, MoveIntent::NewlyIssued)?;
            }
        }
        live(owner, permit, control, task).await?;
        let (_, rows) = elements(owner)?;
        let inventory = observed(owner, permit, control, task, opts).await?;
        if let Err(error) = if at_destination(&rows, &plan) {
            Ok(())
        } else {
            checked_move(owner, &plan, MoveIntent::ObserveOnly)
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
    let tracked = fdb_monitor::Tracked::new(owner.devices[drive].dev.as_ref(), "validate");
    let validation = validate_index(
        &tracked,
        owner.files.spool_dir(),
        control.storage,
        plan.volume.ok_or_else(|| failed("校验缺少卷身份"))?,
        Some(task.pool().ok_or_else(|| failed("校验缺少池"))?),
        Some(head.index),
        false,
    );
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
    fdb_monitor::sample(
        owner,
        permit,
        control,
        drive,
        plan.barcode.as_deref().expect("barcode"),
        head.volume_id,
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

fn mark_pool(
    device: &dyn TapeTransport,
    key: ReservationKey,
    volume: uuid::Uuid,
    pool: uuid::Uuid,
    name: &str,
) -> std::result::Result<(), ReadError> {
    let mut mounted = LtfsVolume::mount(device).map_err(physical)?;
    if mounted.label().volume_uuid != volume {
        return Err(failed("补标记前卷 UUID 已变化"));
    }
    let mut occupied = false;
    mounted.index().walk_files(|_, _| occupied = true);
    if occupied || !mounted.index().root.subdirs.is_empty() {
        return Err(failed("补标记只允许完整空卷，不能写已有文件的卷"));
    }
    let uuid = pool.to_string();
    if mounted
        .root_xattr(XATTR_POOL_UUID)
        .is_some_and(|v| v != uuid)
        || mounted
            .root_xattr(XATTR_POOL_NAME)
            .is_some_and(|v| v != name)
    {
        return Err(failed("已有 pool 标记与原目标不同，不覆盖"));
    }
    if mounted.root_xattr(XATTR_POOL_UUID) == Some(uuid.as_str())
        && mounted.root_xattr(XATTR_POOL_NAME) == Some(name)
    {
        return Ok(());
    }
    mounted.set_reservation_guard(Some(key));
    mounted
        .set_root_xattr(XATTR_POOL_UUID, &uuid)
        .map_err(physical)?;
    mounted
        .set_root_xattr(XATTR_POOL_NAME, name)
        .map_err(physical)?;
    mounted.commit().map_err(physical)
}

#[allow(clippy::too_many_arguments)]
async fn uncertain(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
    task: &NativeDeviceTask,
    plan: &Plan,
    from: &str,
    reason: &str,
) -> std::result::Result<bool, ReadError> {
    live(owner, permit, control, task).await?;
    let seen = observation(management::observe(owner, opts).ok(), Some(reason), None);
    checkpoint(
        owner,
        permit,
        control,
        task,
        from,
        "format-uncertain",
        plan,
        &seen,
    )
    .await?;
    Ok(false)
}

fn format_allowed(
    device: &dyn TapeTransport,
    spool: &std::path::Path,
    budget: super::super::fdb_read::ReadStorageBudget,
    force: bool,
) -> std::result::Result<(), ReadError> {
    let checked = cancel::PreformatRead::new(device, || Ok(false));
    let result = format_allowed_readonly(&checked, spool, budget, force);
    let proof = checked.verify_proof().map_err(physical);
    match result {
        Err(error) => Err(error),
        Ok(()) => proof,
    }
}

fn format_allowed_readonly(
    device: &dyn TapeTransport,
    spool: &std::path::Path,
    budget: super::super::fdb_read::ReadStorageBudget,
    force: bool,
) -> std::result::Result<(), ReadError> {
    match probe_format(device).map_err(physical)? {
        FormatProbe::Blank => {
            let drive = TapeDrive::new(device);
            let mut buffer = vec![0; crate::ltfs::volume::DEFAULT_BLOCK_SIZE as usize];
            for partition in 0..drive.defined_partitions().map_err(physical)? {
                drive.locate(partition, 0, true).map_err(physical)?;
                match drive.read_block(&mut buffer) {
                    Err(
                        error @ TapeError::ScsiCommand {
                            sense_key: 0x08, ..
                        },
                    ) if cancel::current_read_error(&error) => {}
                    Err(error) => return Err(physical(error)),
                    Ok(_) => return Err(failed("介质仍有非空分区；销毁须显式 force_format")),
                }
            }
            Ok(())
        }
        FormatProbe::Foreign(_) | FormatProbe::Ltfs if force => Ok(()),
        FormatProbe::Ltfs => {
            let current = labels(device)?;
            let (summary, children) = validate_index(
                device,
                spool,
                budget,
                current.volume_uuid,
                None,
                None,
                false,
            )?;
            if summary["files"] != 0 || children {
                Err(failed(
                    "介质已有文件对象；保留文件请 import，销毁须显式 force_format",
                ))
            } else {
                Ok(())
            }
        }
        FormatProbe::Foreign(reason) => Err(failed(format!(
            "介质不是空白或空 LTFS：{reason}；不自动格式化"
        ))),
    }
}

fn format_authority(
    device: &dyn TapeTransport,
    permit: &Permission,
    control: &FdbOwnerControl,
    task: &NativeDeviceTask,
    owner: u64,
    phase: &str,
) -> crate::error::Result<()> {
    let permission = || {
        permit.allows(owner, super::super::permission::Phase::Serving)
            || permit.allows(owner, super::super::permission::Phase::Execution)
    };
    let lost = |detail: String| TapeError::OwnershipLost {
        device: device.identity().into(),
        detail,
    };
    if !permission() {
        return Err(lost("格式化执行许可已失效".into()));
    }
    control
        .catalog
        .native_format_authority_blocking(task, owner, phase)
        .map_err(|error| lost(error.to_string()))?;
    if !permission() {
        return Err(lost("格式化共享状态查询期间执行许可失效".into()));
    }
    Ok(())
}

/// 完成同一已授权格式化意图；unknown 时仅观察，绝不再 FORMAT。
pub(super) async fn prepare_assign(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
    intake: &crate::managed_catalog::NativeMediaImport,
) -> std::result::Result<bool, ReadError> {
    let task = intake
        .format_task()
        .ok_or_else(|| failed("分配缺少格式化任务句柄"))?;
    let request = &intake.request().value()["change"];
    if request["confirm_format"] != true {
        return Err(failed("分配初始化必须显式 confirm_format"));
    }
    let mut state = control
        .catalog
        .native_device_status(&task, owner.round)
        .await
        .map_err(lost)?;
    if state["phase"] == "format-verified" {
        if state["cancel_requested"] == true {
            control
                .catalog
                .abort_native_media(intake, owner.round, None)
                .await
                .map_err(lost)?;
            return Ok(false);
        }
        return Ok(true);
    }
    let key = format!("lifecycle-format:{}", intake.request().operation());
    if owner
        .deferred
        .get(&key)
        .is_some_and(|at| *at > Instant::now())
    {
        return Ok(false);
    }
    let _activity = permit
        .begin_device_activity(owner.round)
        .map_err(physical)?;
    state = live(owner, permit, control, &task).await?;
    let plan = if state["phase"] == "ready" {
        let plan = choose_plan(owner, permit, control, &task).await?;
        state = checkpoint(
            owner,
            permit,
            control,
            &task,
            "ready",
            "planned",
            &plan,
            &observation(
                Some(observed(owner, permit, control, &task, opts).await?),
                None,
                None,
            ),
        )
        .await?;
        plan
    } else {
        serde_json::from_value::<Plan>(state["plan"].clone()).map_err(lost)?
    };
    let phase = state["phase"]
        .as_str()
        .ok_or_else(|| lost("格式化任务缺少阶段"))?
        .to_owned();
    let newly_loaded = phase == "planned";
    if matches!(phase.as_str(), "planned" | "load-issued" | "loaded")
        && state["cancel_requested"] == true
    {
        return Err(failed("格式化前已请求取消"));
    }
    if newly_loaded {
        checkpoint(
            owner,
            permit,
            control,
            &task,
            "planned",
            "load-issued",
            &plan,
            &observation(None, None, None),
        )
        .await?;
    }
    let drive = if matches!(phase.as_str(), "planned" | "load-issued" | "loaded") {
        load(owner, permit, control, &task, &plan, newly_loaded).await?
    } else {
        // 不确定格式化的恢复只能读取；不能以 LOAD 或再次 FORMAT 猜测完成状态。
        let (_, rows) = elements(owner)?;
        if !at_destination(&rows, &plan) {
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                &phase,
                "格式化目标已不在固定驱动器，仅保留观察",
            )
            .await;
        }
        inventory_elements(&owner.devices, &rows, true)
            .map_err(physical)?
            .drives
            .into_iter()
            .find(|slot| {
                Some(slot.addr) == plan.destination
                    && Some(owner.devices[slot.dev].serial.as_str()) == plan.drive_serial.as_deref()
            })
            .ok_or_else(|| lost("格式化恢复驱动器身份不符"))?
            .dev
    };
    if matches!(phase.as_str(), "planned" | "load-issued") {
        checkpoint(
            owner,
            permit,
            control,
            &task,
            "load-issued",
            "loaded",
            &plan,
            &observation(
                Some(observed(owner, permit, control, &task, opts).await?),
                None,
                None,
            ),
        )
        .await?;
    }
    let volume = plan.volume.ok_or_else(|| failed("固定新 UUID 缺失"))?;
    let pool = intake.pool();
    let device = owner.devices[drive].dev.as_ref();
    let mut format_phase = phase.as_str();
    if matches!(format_phase, "planned" | "load-issued" | "loaded") {
        {
            let checked = cancel::PreformatRead::new(device, || {
                control
                    .catalog
                    .native_device_cancelled_blocking(&task, owner.round)
            });
            format_allowed(
                &checked,
                owner.files.spool_dir(),
                control.storage,
                request["force_format"] == true,
            )?;
        }
        checkpoint(
            owner,
            permit,
            control,
            &task,
            "loaded",
            "format-started",
            &plan,
            &observation(
                Some(observed(owner, permit, control, &task, opts).await?),
                None,
                None,
            ),
        )
        .await?;
        let result = format_guard::format(
            device,
            &MkltfsOptions {
                volume_id: intake.barcode().chars().take(6).collect(),
                block_size: opts.block_size,
                volume_uuid: Some(volume),
                ..Default::default()
            },
            || {
                format_authority(
                    device,
                    permit,
                    control,
                    &task,
                    owner.round,
                    "format-started",
                )
            },
        );
        live(owner, permit, control, &task).await?;
        if let Err(error) = result {
            if ownership_lost(&error) {
                return Err(lost(error));
            }
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                "format-started",
                &error.to_string(),
            )
            .await;
        }
        format_phase = "format-started";
    }
    // 任何接续都先证明固定 UUID 与完整空索引，再补原 pool 标记，不能重格式化。
    let proof_device = format_guard::Guard::new(device, || {
        format_authority(device, permit, control, &task, owner.round, format_phase)
    });
    let proof = validate_index(
        &proof_device,
        owner.files.spool_dir(),
        control.storage,
        volume,
        Some(pool),
        None,
        false,
    );
    match proof {
        Ok((summary, false)) if summary["files"] == 0 => {}
        Ok(_) => {
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                format_phase,
                "原格式化 UUID 上出现文件，不补标记、不发布",
            )
            .await;
        }
        Err(ReadError::Lost(reason)) => return Err(ReadError::Lost(reason)),
        Err(error) => {
            owner
                .deferred
                .insert(key, Instant::now() + Duration::from_secs(1));
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                format_phase,
                &error.to_string(),
            )
            .await;
        }
    }
    let target = control
        .catalog
        .active_pool_for_route(&control.serial, control.route, pool)
        .await
        .map_err(lost)?
        .ok_or_else(|| lost("原目标池已不存在"))?;
    if format_phase != "marking-started" {
        checkpoint(
            owner,
            permit,
            control,
            &task,
            format_phase,
            "marking-started",
            &plan,
            &observation(
                Some(observed(owner, permit, control, &task, opts).await?),
                None,
                None,
            ),
        )
        .await?;
    }
    live(owner, permit, control, &task).await?;
    let marking_device = format_guard::Guard::new(device, || {
        format_authority(
            device,
            permit,
            control,
            &task,
            owner.round,
            "marking-started",
        )
    });
    if let Err(error) = mark_pool(&marking_device, owner.key, volume, pool, &target.name) {
        if let ReadError::Lost(reason) = error {
            return Err(ReadError::Lost(reason));
        }
        return uncertain(
            owner,
            permit,
            control,
            opts,
            &task,
            &plan,
            "marking-started",
            &error.to_string(),
        )
        .await;
    }
    live(owner, permit, control, &task).await?;
    let validation = match validate_index(
        &marking_device,
        owner.files.spool_dir(),
        control.storage,
        volume,
        Some(pool),
        None,
        true,
    ) {
        Ok((validation, _)) => validation,
        Err(ReadError::Lost(reason)) => return Err(ReadError::Lost(reason)),
        Err(error) => {
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                "marking-started",
                &error.to_string(),
            )
            .await;
        }
    };
    let barcode = match crate::ltfs::mam::Mam::new(device)
        .read_attribute(crate::ltfs::mam::ATTR_BARCODE)
        .map_err(physical)
        .and_then(|value| value.ok_or_else(|| failed("格式化 MAM barcode 缺失")))
    {
        Ok(barcode) => barcode,
        Err(ReadError::Lost(reason)) => return Err(ReadError::Lost(reason)),
        Err(error) => {
            return uncertain(
                owner,
                permit,
                control,
                opts,
                &task,
                &plan,
                "marking-started",
                &error.to_string(),
            )
            .await;
        }
    };
    if std::str::from_utf8(&barcode.value)
        .ok()
        .map(|v| v.trim_matches([' ', '\0']))
        != Some(intake.barcode().get(..6).unwrap_or(intake.barcode()))
    {
        return uncertain(
            owner,
            permit,
            control,
            opts,
            &task,
            &plan,
            "marking-started",
            "格式化 MAM barcode 与原介质不符",
        )
        .await;
    }
    checkpoint(
        owner,
        permit,
        control,
        &task,
        "marking-started",
        "format-verified",
        &plan,
        &observation(
            Some(observed(owner, permit, control, &task, opts).await?),
            None,
            Some(validation),
        ),
    )
    .await?;
    owner.deferred.remove(&key);
    if live(owner, permit, control, &task).await?["cancel_requested"] == true {
        let _activity = permit
            .begin_device_activity(owner.round)
            .map_err(physical)?;
        control
            .catalog
            .abort_native_media(intake, owner.round, None)
            .await
            .map_err(lost)?;
        return Ok(false);
    }
    Ok(true)
}

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
        Ok(None)
        | Err(CatalogError::Incomplete | CatalogError::PoolBusy | CatalogError::PoolImporting) => {
            return Ok(());
        }
        Err(CatalogError::Invalid(reason)) => {
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
    if live(owner, permit, control, &task).await?["cancel_requested"] == true {
        let result = json!({"observation":observed(owner, permit, control, &task, opts).await?,"validation":null});
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
            lib.load_into_drive("LIFE00L8", 0).unwrap();
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
    fn lifecycle_assign_rejects_existing_files_without_force_and_never_formats_them() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        f.format(volume);
        let mut mounted = LtfsVolume::mount(f.device()).unwrap();
        mounted.set_reservation_guard(Some(f.owner.key));
        mounted
            .append_file("/kept", &mut Cursor::new(b"keep this payload"))
            .unwrap();
        mounted.commit().unwrap();
        drop(mounted);
        let formats = f.formats();
        assert!(format_allowed(f.device(), f.owner.files.spool_dir(), f.budget(), false).is_err());
        assert_eq!(f.formats(), formats);
        assert!(format_allowed(f.device(), f.owner.files.spool_dir(), f.budget(), true).is_ok());
        assert_eq!(
            f.formats(),
            formats,
            "force 只放行检查，FORMAT 仍必须由持久写前意图调用"
        );
        assert_eq!(
            LtfsVolume::mount(f.device()).unwrap().label().volume_uuid,
            volume
        );
    }

    #[test]
    fn lifecycle_resume_after_mkltfs_before_pool_mark_keeps_the_fixed_uuid_and_formats_once() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        let pool = uuid::Uuid::new_v4();
        f.format(volume);
        assert_eq!(f.formats(), 1);
        let (raw, children) = validate_index(
            f.device(),
            f.owner.files.spool_dir(),
            f.budget(),
            volume,
            Some(pool),
            None,
            false,
        )
        .unwrap();
        assert_eq!(raw["generation"], 1);
        assert!(!children);
        assert!(
            validate_index(
                f.device(),
                f.owner.files.spool_dir(),
                f.budget(),
                volume,
                Some(pool),
                None,
                true
            )
            .is_err(),
            "缺pool标记不能伪称verified"
        );
        // 模拟原actor退出后同一持久意图恢复：禁止再FORMAT，只补原目标标记。
        mark_pool(f.device(), f.owner.key, volume, pool, "archive").unwrap();
        let (verified, children) = validate_index(
            f.device(),
            f.owner.files.spool_dir(),
            f.budget(),
            volume,
            Some(pool),
            None,
            true,
        )
        .unwrap();
        assert_eq!(verified["volume"], volume.to_string());
        assert_eq!(verified["generation"], 2);
        assert_eq!(verified["files"], 0);
        assert_eq!(verified["pool_mark_verified"], true);
        assert_eq!(verified["payload_verified"], false);
        assert!(!children);
        assert_eq!(f.formats(), 1);
    }

    #[test]
    fn lifecycle_resume_after_mark_commit_before_receipt_does_not_reformat_or_commit_twice() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        let pool = uuid::Uuid::new_v4();
        f.format(volume);
        mark_pool(f.device(), f.owner.key, volume, pool, "archive").unwrap();
        let writes = f.writes();
        let first = validate_index(
            f.device(),
            f.owner.files.spool_dir(),
            f.budget(),
            volume,
            Some(pool),
            None,
            true,
        )
        .unwrap()
        .0;
        // 标记的物理commit成功，但其FDB回执尚未记录；重入同意图只能读到并复用它。
        mark_pool(f.device(), f.owner.key, volume, pool, "archive").unwrap();
        let second = validate_index(
            f.device(),
            f.owner.files.spool_dir(),
            f.budget(),
            volume,
            Some(pool),
            None,
            true,
        )
        .unwrap()
        .0;
        assert_eq!(first, second);
        assert_eq!(f.formats(), 1);
        assert_eq!(f.writes(), writes);
    }

    #[test]
    fn lifecycle_pool_mark_refuses_a_different_pool_without_overwriting_it() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        let pool = uuid::Uuid::new_v4();
        f.format(volume);
        mark_pool(f.device(), f.owner.key, volume, pool, "archive").unwrap();
        let writes = f.writes();
        assert!(
            mark_pool(
                f.device(),
                f.owner.key,
                volume,
                uuid::Uuid::new_v4(),
                "other"
            )
            .is_err()
        );
        assert_eq!(f.writes(), writes);
        assert_eq!(
            LtfsVolume::mount(f.device())
                .unwrap()
                .root_xattr(XATTR_POOL_UUID),
            Some(pool.to_string().as_str())
        );
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
            false,
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
                Some(expected),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn lifecycle_lost_majority_never_writes_a_pool_marker_or_formats() {
        let f = Fixture::new();
        let volume = uuid::Uuid::new_v4();
        f.format(volume);
        let writes = f.writes();
        f.permit.close(7);
        assert!(matches!(
            mark_pool(
                f.device(),
                f.owner.key,
                volume,
                uuid::Uuid::new_v4(),
                "archive"
            ),
            Err(ReadError::Lost(_))
        ));
        assert_eq!(f.formats(), 1);
        assert_eq!(f.writes(), writes);
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
                medium_type: None,
                volume_tag: None,
                source_address: None,
                drive_id: None,
            })
            .chain(std::iter::once(ElementStatus {
                address: 200,
                element_type: ElementType::ImportExport,
                full: true,
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
        let replies = (1..=3).map(|node| json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":1,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":1,"frozen_index":2,"applied_index":3,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
            control
                .catalog
                .admit_management(&control.serial, control.route, fixture.owner.round, request)
                .await
                .unwrap();
            for _ in 0..5 {
                let result = if matches!(
                    request.value()["change"]["kind"].as_str(),
                    Some("media-assign" | "media-import")
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
    #[tokio::test]
    #[ignore = "requires disposable FDB_TEST_CLUSTER; SimLibrary only, no real hardware"]
    async fn unassigned_loaded_move_runs_in_execution_and_replays_without_media_writes() {
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
        // A durable maintenance intent can run after fencing, before data
        // service, but Fencing permission alone must never issue its CDBs.
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
        let fenced_commands = fixture.commands.lock().unwrap().clone();
        assert!(
            apply(
                &mut fixture.owner,
                &fixture.permit,
                &control,
                &fixture.opts,
                move_home.epoch(),
                move_home.operation()
            )
            .await
            .is_err(),
            "仅Fencing许可不能退带或移动"
        );
        assert_eq!(*fixture.commands.lock().unwrap(), fenced_commands);
        fixture.permit.renew(
            8,
            Phase::Execution,
            Instant::now() + Duration::from_secs(60),
        );
        fixture.owner.fdb_pending_import.insert("LIFE00L8".into());
        let before = fixture.drive_log().len();
        let moved = Box::pin(execute(&mut fixture, &control, &status, &move_home)).await;
        assert_eq!(moved["status"], "succeeded", "{moved}");
        assert_eq!(
            moved["result"]["device"]["media_may_have_changed"], true,
            "UNLOAD 可能刷出设备原有缓冲，必须记录其潜在介质影响"
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

    #[tokio::test]
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
            metadata_receipts.push((file.clone(), intent.request()));
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
        for (file, request) in metadata_receipts {
            let resumed = observer
                .resume_reclaim_file_metadata(&reserved, fixture.owner.round, &file, &request)
                .await
                .unwrap()
                .unwrap();
            assert_eq!((resumed.files(), resumed.aliases()), (0, 2));
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

    #[tokio::test]
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

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实持久任务加 SimLibrary，不访问实际设备"]
    async fn lifecycle_real_fdb_and_sim_complete_assign_validate_move_unassign_and_replay() {
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
            (source_object_key, Some(b"fixture-corrupt-object".to_vec())),
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
        let claimed = catalog
            .claim_reclaim_reservations_page(&serial, route, next_round, None, 16)
            .await
            .unwrap();
        assert_eq!(claimed.items.len(), 1);
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
        assert_eq!(fixture.drive_log(), drive_before_replay);
        assert_eq!(fixture.changer_log(), changer_before_replay);
    }
    include!("executor_fdb_format_physical_tests.rs");
}

#[cfg(test)]
mod format_partition_tests {
    use super::*;
    use crate::scsi::sim::{SimCartridge, SimLibrary};

    #[test]
    fn nonforce_rejects_blank_index_partition_with_data_partition_content() {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("REV001L8", 64 << 20), 0)
            .unwrap();
        lib.load_into_drive("REV001L8", 0).unwrap();
        let dev = lib.drive(0);
        mkltfs(
            &dev,
            &MkltfsOptions {
                volume_id: "REV001".into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
        {
            let mut vol = crate::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
            vol.append_file(
                "/valuable.txt",
                &mut std::io::Cursor::new(b"retained P1 data"),
            )
            .unwrap();
            vol.commit().unwrap();
        }
        assert!(lib.with_cartridge_mut("REV001L8", |cart| {
            assert!(cart.partitions[1].objects.len() > 5);
            cart.partitions[0].objects.clear();
            cart.partitions[0].flushed = 0;
        }));
        assert_eq!(probe_format(&dev).unwrap(), FormatProbe::Blank);
        let result = format_allowed(
            &dev,
            &std::env::temp_dir(),
            super::super::super::fdb_read::ReadStorageBudget::default(),
            false,
        );
        assert!(
            result.is_err(),
            "non-force must reject P0 blank when P1 retains committed file data"
        );
    }
}
