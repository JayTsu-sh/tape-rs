//! 管理任务在现有 owner 线程执行，复用已隔离且有许可门控的设备句柄。
use super::*;
use crate::daemon::management as model;
use crate::daemon::state::Command;
use serde_json::{Value, json};

fn error(reason: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: reason.into(),
    }
}

fn read_management_capacity(
    dev: &dyn TapeTransport,
    generation: u64,
    partition: u8,
) -> Result<Value> {
    use crate::ltfs::mam::{ATTR_MAXIMUM_CAPACITY, ATTR_REMAINING_CAPACITY, Mam};
    let mam = Mam::with_partition(dev, partition);
    // 与单机 Library::observe_capacity 一致：两个属性都有效才记录，不能将缺失默认成零。
    let remaining = mam.read_u64(ATTR_REMAINING_CAPACITY)?;
    let maximum = mam.read_u64(ATTR_MAXIMUM_CAPACITY)?;
    let measured = maximum.zip(remaining).and_then(|(m, r)| {
        m.checked_mul(1 << 20)
            .zip(r.checked_mul(1 << 20))
            .filter(|(m, r)| *m > 0 && r <= m && *m <= i64::MAX as u64)
    });
    Ok(match measured {
        Some((maximum, remaining)) => {
            json!({"generation":generation,"partition":partition,"maximum":maximum,"remaining":remaining,"observed_at":crate::ltfs::label::ltfs_time_now()})
        }
        None => Value::Null,
    })
}

pub(super) fn refresh_capacity(
    dev: &dyn TapeTransport,
    vol: &LtfsVolume<'_>,
    round: u64,
    barcode: &str,
    tx: &Sender<ExecEvent>,
) -> Result<Value> {
    let read = |partition| -> Result<(Value, Option<String>)> {
        match read_management_capacity(dev, vol.index().generation, partition) {
            Ok(capacity) => {
                let error = capacity
                    .is_null()
                    .then(|| "MAM 容量属性缺失或无效".to_string());
                Ok((capacity, error))
            }
            Err(e) if ownership_lost(&e) => Err(e),
            Err(e) => {
                log::debug!("磁带 {barcode} 分区 {partition} 容量刷新失败: {e}");
                Ok((Value::Null, Some(e.to_string().chars().take(512).collect())))
            }
        }
    };
    let (capacity, error) = read(vol.recovery().dp.partition)?;
    let (index_capacity, index_error) = read(vol.recovery().ip.partition)?;
    let _=tx.send(ExecEvent::Management{round,command:Command::ObserveTape{barcode:barcode.into(),observation:json!({"volume_uuid":vol.label().volume_uuid.to_string(),"generation":vol.index().generation,"capacity":capacity,"index_capacity":index_capacity,"observed_at":crate::ltfs::label::ltfs_time_now(),"error":error,"index_error":index_error})}});
    let mut capacity = capacity;
    if !capacity.is_null() {
        capacity["index_capacity"] = index_capacity;
    }
    Ok(capacity)
}
fn report(tx: &Sender<ExecEvent>, round: u64, task: u64, phase: &str, result: Value) {
    let _ = tx.send(ExecEvent::Management {
        round,
        command: Command::ManagementReport {
            task,
            round,
            phase: phase.into(),
            result,
        },
    });
}

pub(super) fn drive(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
) -> std::result::Result<(), String> {
    let st = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if st
        .executor
        .as_ref()
        .is_none_or(|e| e.node != u64::from(opts.node_id) || e.round != a.round || !e.fenced)
    {
        return Ok(());
    }
    let tasks = &st.management["tasks"];
    if a.management.is_none() {
        let next = tasks.as_object().and_then(|ts| {
            ts.values()
                .filter(|t| t["state"] == "queued")
                .min_by_key(|t| t["id"].as_u64())
        });
        let Some(task) = next else {
            return Ok(());
        };
        let id = task["id"].as_u64().ok_or("管理任务 ID 非法")?;
        report(tx, a.round, id, "running", Value::Null);
        a.management = Some((id, false));
        return Ok(());
    }
    let (id, sent) = a.management.unwrap();
    let Some(task) = tasks.get(id.to_string()) else {
        a.management = None;
        files.resume_admission(a.round);
        for writer in &a.writers {
            writer.files.resume_admission(a.round);
        }
        return Ok(());
    };
    if task["state"] != "queued" && task["state"] != "running" {
        a.management = None;
        // 只有管理任务实际借用了写入侧，才重新选带。
        if a.drive.is_some() {
            files.resume_admission(a.round);
            for writer in &a.writers {
                writer.files.resume_admission(a.round);
            }
        }
        if a.drive.is_none() {
            let summary =
                ensure_write_tape(a, opts, files, status, tx).map_err(|e| e.to_string())?;
            let _ = tx.send(ExecEvent::Serving {
                round: a.round,
                summary,
            });
        }
        return Ok(());
    }
    if sent || task["state"] != "running" || task["round"] != a.round {
        return Ok(());
    }
    if task["cancel_requested"] == true {
        report(
            tx,
            a.round,
            id,
            "cancelled",
            json!({"detail":"设备操作开始前取消"}),
        );
        a.management = Some((id, true));
        return Ok(());
    }
    let r = &task["request"];
    let resource = r["resource"].as_str().unwrap();
    let needs_volume = matches!(
        resource,
        "pools" | "tapes" | "drives" | "nodes" | "nodegroups"
    );
    if needs_volume {
        files.pause_admission();
        for writer in &a.writers {
            writer.files.pause_admission();
        }
        process_all_uploads(a, tx, true, usize::MAX, None)?;
        if a.writers
            .iter()
            .any(|w| w.drive.is_some() && !w.files.write_side_idle(a.round))
            || a.drive.is_some() && !files.write_side_idle(a.round)
        {
            return Ok(());
        }
    }
    if needs_volume {
        let original = a.files.clone();
        let lanes: Vec<_> = a
            .writers
            .iter()
            .map(|w| w.files.clone())
            .chain([original.clone()])
            .collect();
        for lane in lanes {
            activate_writer(a, &lane);
            quiesce(a, &lane, tx)
                .map_err(|e| format!("管理任务收束写入侧失败，须接管恢复: {e}"))?;
        }
        activate_writer(a, &original);
        quiesce(a, files, tx).map_err(|e| format!("管理任务收束写入侧失败，须接管恢复: {e}"))?;
    }
    let result = (|| -> Result<Value> {
        if a.reclaim.is_some() {
            return Err(error("回收正在执行，不能进行管理任务"));
        }

        let mut result = execute(a, opts, status, files, tx, task)?;
        result["inventory"] = observe(a, opts)?;
        if result.to_string().len() > model::MAX_RESULT {
            return Err(error("管理结果超过 256 KiB，请缩小查询范围"));
        }
        Ok(result)
    })();
    match result {
        Ok(value) => report(tx, a.round, id, "success", value),
        Err(e) if ownership_lost(&e) => return Err(format!("管理任务 {id} 失去执行资格: {e}")),
        Err(TapeError::Refused { reason }) if reason.starts_with("校验在文件之间收到取消请求") => {
            report(tx, a.round, id, "cancelled", json!({"detail":reason}))
        }
        Err(e) => report(
            tx,
            a.round,
            id,
            "failed",
            json!({"detail":e.to_string(),"effects":"须核对已发生的介质操作及 owner 本地输出，不自动回滚或重放"}),
        ),
    }
    a.management = Some((id, true));
    Ok(())
}

pub(super) fn quiesce(a: &mut Active, files: &FileService, tx: &Sender<ExecEvent>) -> Result<()> {
    checkpoint_write_tape(a, files, tx)?;
    let owned: Vec<usize> = [a.drive, a.read.as_ref().map(|r| r.0)]
        .into_iter()
        .flatten()
        .collect();
    for di in owned {
        let inv = inventory(&a.devices)?;
        if let Some(d) = inv.drives.iter().find(|d| d.dev == di) {
            unload_drive(&a.devices, &inv, d)?;
        }
    }
    let pool = files.tape().map(|t| t.pool_uuid);
    a.drive = None;
    a.write_session = None;
    a.read = None;
    files.close_waiting(a.round, pool, "管理维护期间暂停文件写入");
    Ok(())
}

fn elements(
    a: &Active,
) -> Result<(
    crate::changer::element::ElementAddressMap,
    Vec<crate::changer::element::ElementStatus>,
)> {
    let changer = a
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .ok_or_else(|| error("没有换带器"))?;
    let mut mc = MediumChanger::new(changer.dev.as_ref());
    let map = mc.load_address_map()?.clone();
    let rows = mc.read_all_status()?;
    Ok((map, rows))
}
fn observe(a: &Active, opts: &ExecOptions) -> Result<Value> {
    let (map, rows) = elements(a)?;
    let volumes:Vec<Value>=rows.iter().filter(|e|e.full && e.volume_tag.is_some()).map(|e|json!({"barcode":e.volume_tag,"address":e.address,"element_type":e.element_type as u8,"medium_type":e.medium_type,"source_address":e.source_address,"home_slot":e.address.checked_sub(map.storage_start).filter(|s|*s<map.storage_count).map(|s|u32::from(s)+1)})).collect();
    let inv = inventory(&a.devices)?;
    let drives:Vec<Value>=inv.drives.iter().map(|d|json!({"serial":a.devices[d.dev].serial,"address":d.addr,"barcode":d.loaded.as_ref().map(|l|&l.0)})).collect();
    Ok(
        json!({"library_serial":a.devices.iter().find(|d|d.kind==DeviceKind::Changer).map(|d|&d.serial),"owner":opts.node_id,"round":a.round,"observed_at":crate::ltfs::label::ltfs_time_now(),"volumes":volumes,"drives":drives}),
    )
}

fn cancelled(status: &SharedStatus, id: u64) -> bool {
    status.lock().unwrap_or_else(|e| e.into_inner()).management["tasks"][id.to_string()]["cancel_requested"]
        == true
}

pub(super) fn execute(
    a: &mut Active,
    opts: &ExecOptions,
    status: &SharedStatus,
    files: &FileService,
    tx: &Sender<ExecEvent>,
    task: &Value,
) -> Result<Value> {
    let id = task["id"]
        .as_u64()
        .ok_or_else(|| error("管理任务 ID 非法"))?;
    let r = &task["request"];
    let resource = r["resource"].as_str().unwrap();
    let action = r["action"].as_str().unwrap();
    let target = r["target"].as_str().unwrap();
    let p = &r["params"];
    let st = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let m = &st.management;
    if resource == "pools" {
        let pool = st
            .pools
            .iter()
            .find(|pool| pool.name == target || pool.uuid == target)
            .ok_or_else(|| error("池不存在"))?;
        let name = if p["attribute"] == "poolname" {
            p["value"].as_str().unwrap()
        } else {
            &pool.name
        };
        let limit = if p["attribute"] == "file_limit" {
            p["value"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .map_err(|_| error("文件数上限非法"))?
        } else {
            pool.file_limit
        };
        return Ok(
            json!({"pool_update":{"uuid":pool.uuid,"name":name,"file_limit":limit},"scope":"cluster_pool_policy"}),
        );
    }
    if resource == "libraries" {
        return Ok(json!({"scope":"inventory_only_no_initialize"}));
    }
    if resource == "drives" {
        let inv = inventory(&a.devices)?;
        let d = inv
            .drives
            .iter()
            .find(|d| a.devices[d.dev].serial == target)
            .ok_or_else(|| error("配置中没有此驱动器序列号"))?;
        if d.loaded.is_some() {
            return Err(error("驱动器仍有介质；先显式 offline 归槽，再更改调度策略"));
        }
        return Ok(json!({"serial":target,"scope":"scheduling_policy"}));
    }
    if resource == "nodes" {
        if target == opts.node_id.to_string()
            && inventory(&a.devices)?
                .drives
                .iter()
                .any(|d| d.loaded.is_some())
        {
            return Err(error("节点组变更前须先将受管介质 offline 归槽"));
        }
        return Ok(json!({"node":target,"scope":"cluster_scheduling_policy"}));
    }
    let (map, rows) = elements(a)?;
    let entry = rows
        .iter()
        .find(|e| e.full && e.volume_tag.as_deref() == Some(target))
        .ok_or_else(|| error("条码不在实际库存中"))?;
    if entry.is_cleaning()
        && [
            "assign",
            "online",
            "validate",
            "reconcile",
            "objects",
            "restore",
        ]
        .contains(&action)
    {
        return Err(error("清洗带不能分配到数据池或执行 LTFS 数据操作"));
    }
    if action == "assign" {
        if entry.element_type != ElementType::Storage {
            return Err(error("分配前磁带必须位于存储槽位"));
        }
        let pool = st
            .pools
            .iter()
            .find(|pool| pool.name == p["pool"] || pool.uuid == p["pool"])
            .ok_or_else(|| error("池不存在"))?;
        return Ok(
            json!({"assignment":{"barcode":target,"pool_uuid":pool.uuid},"home_address":entry.address}),
        );
    }
    if action == "unassign" {
        if entry.element_type != ElementType::Storage
            || st.tapes.get(target).is_some_and(|t| t.files > 0)
            || files.live_on(target).is_none_or(|(count, _)| count > 0)
        {
            return Err(error("解除分配要求磁带已归槽且无已知文件"));
        }
        return Ok(json!({"unassigned":target}));
    }
    if ["offline", "blacklist", "clear-blacklist"].contains(&action) {
        if entry.element_type == ElementType::DataTransfer {
            return Err(error("介质仍在非本实例持有的驱动器内，不能隐式操作"));
        }
        let mut result = json!({"barcode":target,"action":action});
        if entry.element_type == ElementType::Storage {
            result["home_address"] = json!(entry.address);
        }
        return Ok(result);
    }
    if action == "move" {
        if entry.element_type == ElementType::DataTransfer {
            return Err(error("磁带位于非本实例持有的驱动器，不搬运"));
        }
        let homeslot = p["location"] == "homeslot";
        let home = m["tapes"][target]["home_address"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok());
        let dest = if homeslot {
            if entry.element_type == ElementType::Storage {
                return Ok(json!({"barcode":target,"already_at":"homeslot"}));
            }
            home.filter(|h| {
                *h >= map.storage_start
                    && u32::from(*h) < u32::from(map.storage_start) + u32::from(map.storage_count)
            })
            .and_then(|h| rows.iter().find(|e| e.address == h && !e.full))
            .ok_or_else(|| error("原 home slot 不存在或已占用"))?
        } else {
            rows.iter()
                .find(|e| e.element_type == ElementType::ImportExport && !e.full)
                .ok_or_else(|| error("没有空闲 I/E 槽位"))?
        };
        move_medium(&a.devices, entry.address, dest.address)?;
        return Ok(json!({"barcode":target,"from":entry.address,"to":dest.address}));
    }
    if !["online", "validate", "reconcile"].contains(&action) && !model::tape_allowed(m, target) {
        return Err(error("磁带 offline 或被列入黑名单"));
    }
    if !m["tapes"][target]["blacklist"].is_null() {
        return Err(error("须先显式清除黑名单"));
    }
    let pool = st
        .pools
        .iter()
        .find(|pool| pool.tapes.iter().any(|b| b == target))
        .ok_or_else(|| error("磁带尚未分配"))?;
    let inv = inventory(&a.devices)?;
    let d = inv
        .drives
        .iter()
        .find(|d| {
            d.loaded.is_none()
                && model::drive_allowed(
                    m,
                    &a.devices[d.dev].serial,
                    u64::from(opts.node_id),
                    &pool.uuid,
                    'g',
                )
        })
        .ok_or_else(|| error("没有具备维护资格的空闲驱动器"))?;
    if entry.element_type != ElementType::Storage {
        return Err(error("维护前磁带必须位于存储槽位"));
    }
    move_medium(&a.devices, entry.address, d.addr)?;
    let di = d.dev;
    let operation = (|| -> Result<Value> {
        TapeDrive::new(a.devices[di].dev.as_ref()).load()?;
        wait_ready(a.devices[di].dev.as_ref())?;
        let mut vol = LtfsVolume::mount(a.devices[di].dev.as_ref())?;
        vol.set_reservation_guard(Some(a.key));
        if let Some(actual_pool) = vol.root_xattr(XATTR_POOL_UUID)
            && actual_pool != pool.uuid
        {
            return Err(error(format!(
                "磁带 {target} 的池标记是 {actual_pool}，与归属 {} 不符",
                pool.uuid
            )));
        }
        if let Some(summary) = st.tapes.get(target)
            && !summary.volume_uuid.is_empty()
            && summary.volume_uuid != vol.label().volume_uuid.to_string()
        {
            return Err(error(format!(
                "磁带 {target} 卷 UUID 不符：集群已知 {}，实际 {}",
                summary.volume_uuid,
                vol.label().volume_uuid
            )));
        }
        if let Some(summary) = st
            .tapes
            .get(target)
            .filter(|t| t.generation > vol.index().generation)
        {
            return Err(error(format!(
                "磁带代数 {} 小于集群已知 {}",
                vol.index().generation,
                summary.generation
            )));
        }
        let mut result = json!({"barcode":target,"volume_uuid":vol.label().volume_uuid.to_string(),"generation":vol.index().generation});
        result["capacity"] =
            refresh_capacity(a.devices[di].dev.as_ref(), &vol, a.round, target, tx)?;
        match action {
            "online" => {}
            "validate" => {
                let full = p["full_read"] == true;
                let mut entries = Vec::new();
                vol.index().walk_files(|path, file| {
                    entries.push((path.to_string(), file.length, file.symlink.is_some()))
                });
                let mut verified = 0u64;
                let mut no_hash = 0u64;
                let mut last_progress = 0;
                for (position, (path, len, symlink)) in entries.iter().enumerate() {
                    let progress = 10 + position * 80 / entries.len().max(1);
                    if progress / 10 > last_progress {
                        last_progress = progress / 10;
                        report(tx, a.round, id, "progress", json!({"progress":progress}));
                    }
                    if cancelled(status, id) {
                        return Err(error("校验在文件之间收到取消请求，已停止后续读取"));
                    }
                    if !full || *symlink {
                        continue;
                    }
                    use crate::ltfs::volume::HashVerdict;
                    match vol.verify_file(path)? {
                        HashVerdict::Match { .. } => verified += 1,
                        HashVerdict::NoHash => {
                            if vol.read_file_to_writer(path, &mut std::io::sink())? != *len {
                                return Err(error(format!("文件长度不符：{path}")));
                            }
                            no_hash += 1;
                        }
                        HashVerdict::Mismatch {
                            algo,
                            expected,
                            actual,
                        } => {
                            return Err(error(format!(
                                "checksum 不符：{path}, {algo}, expected={expected}, actual={actual}"
                            )));
                        }
                    }
                }
                result["files"] = json!(entries.len());
                result["full_read"] = json!(full);
                result["verified_hashes"] = json!(verified);
                result["read_without_hash"] = json!(no_hash);
            }
            "reconcile" => {
                let (records, bytes) = catalog_of(vol.index());
                let _ = tx.send(ExecEvent::Committed {
                    round: a.round,
                    barcode: target.into(),
                    volume_uuid: vol.label().volume_uuid.to_string(),
                    generation: vol.index().generation,
                    files_total: records
                        .iter()
                        .filter(|r| !is_directory(&r.metadata))
                        .count() as u64,
                    bytes_used: bytes,
                    bytes_written: bytes_written_on(a.devices[di].dev.as_ref()),
                    files: records,
                    full: true,
                });
                result["scope"] = json!("cluster_catalog_not_gpfs_hsm");
            }
            "objects" => {
                let manifest = crate::ltfs::ee::EeManifest::from_index(vol.index())?;
                let mut objects = Vec::new();
                let mut bytes = 0usize;
                let offset = p["offset"].as_u64().unwrap_or(0) as usize;
                let limit = p["limit"].as_u64().unwrap_or(100) as usize;
                for o in manifest.objects().iter().skip(offset).take(limit) {
                    let (kind, target) = match &o.kind {
                        crate::ltfs::ee::EeObjectKind::Directory => ("directory", None),
                        crate::ltfs::ee::EeObjectKind::RegularFile => ("file", None),
                        crate::ltfs::ee::EeObjectKind::Symlink { target } => {
                            ("symlink", Some(target))
                        }
                    };
                    let row = json!({"object_path":o.object_path,"saved_path":o.saved_path,"aliases":o.aliases,"kind":kind,"target":target,"length":o.length});
                    bytes += row.to_string().len();
                    if bytes > model::MAX_RESULT / 2 {
                        return Err(error("对象清单过大，不能写入管理任务结果"));
                    }
                    objects.push(row);
                }
                result["scope"] = json!("ee_saved_object_view");
                result["total"] = json!(manifest.objects().len());
                result["next_offset"] =
                    if offset.saturating_add(objects.len()) < manifest.objects().len() {
                        json!(offset + objects.len())
                    } else {
                        Value::Null
                    };
                result["objects"] = json!(objects);
            }
            "restore" => {
                if cancelled(status, id) {
                    return Err(error("恢复开始前已请求取消"));
                }
                let report = crate::ltfs::ee::restore(
                    &vol,
                    std::path::Path::new(p["destination"].as_str().unwrap()),
                )?;
                result["restore"] = json!({"destination":p["destination"],"node":opts.node_id,"scope":"ordinary_filesystem_not_hsm","directories":report.directories,"regular_files":report.regular_files,"symlinks":report.symlinks,"bytes_written":report.bytes_written});
            }
            _ => return Err(error("未知磁带维护操作")),
        }
        Ok(result)
    })();
    if operation.as_ref().is_err_and(ownership_lost) {
        return operation;
    }
    let cleanup = inventory(&a.devices).and_then(|inv| {
        let drive = inv
            .drives
            .iter()
            .find(|d| d.dev == di && d.loaded.as_ref().is_some_and(|(b, _)| b == target))
            .ok_or_else(|| error("维护收尾时介质身份不符"))?;
        unload_drive(&a.devices, &inv, drive)
    });
    if let Err(e) = cleanup.as_ref()
        && ownership_lost(e)
    {
        return cleanup.map(|_| json!({}));
    }
    match (operation, cleanup) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), Ok(())) => Err(e),
        (Ok(_), Err(e)) => Err(error(format!("维护收尾失败，须核实库存和预留: {e}"))),
        (Err(e), Err(cleanup)) => Err(error(format!("{e}；收尾失败: {cleanup}"))),
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use crate::scsi::{device::ScsiResult, sense::SenseInfo};

    struct Attributes {
        maximum: Option<Vec<u8>>,
        remaining: Option<Vec<u8>>,
        expected_partition: u8,
    }
    impl TapeTransport for Attributes {
        fn identity(&self) -> &str {
            "test:capacity-attributes"
        }
        fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
            panic!("容量查询不能改变设备")
        }
        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
            panic!("容量查询不能写入")
        }
        fn execute_read(&self, cdb: &[u8], buf: &mut [u8], _: u32) -> Result<ScsiResult> {
            assert_eq!(cdb[0], 0x8c);
            assert_eq!(cdb[7], self.expected_partition);
            let id = u16::from_be_bytes([cdb[8], cdb[9]]);
            let attr = match id {
                0 => &self.remaining,
                1 => &self.maximum,
                _ => panic!("未知容量属性"),
            };
            let mut body = Vec::new();
            if let Some(value) = attr {
                body.extend_from_slice(&id.to_be_bytes());
                body.push(0);
                body.extend_from_slice(&(value.len() as u16).to_be_bytes());
                body.extend_from_slice(value);
            }
            buf[..4].copy_from_slice(&(body.len() as u32).to_be_bytes());
            buf[4..4 + body.len()].copy_from_slice(&body);
            Ok(ScsiResult {
                status: 0,
                sense: SenseInfo::from_bytes(&[]),
                transferred: 4 + body.len(),
                duration_ms: 0,
            })
        }
    }

    #[test]
    fn incomplete_or_invalid_mam_capacity_is_unknown_not_full() {
        for (maximum, remaining) in [
            (Some(1024u64), None),
            (None, Some(0)),
            (None, None),
            (Some(0), Some(0)),
            (Some(1), Some(2)),
            (Some(u64::MAX), Some(0)),
            (Some(1024), Some(u64::MAX)),
        ] {
            let dev = Attributes {
                maximum: maximum.map(|n| n.to_be_bytes().to_vec()),
                remaining: remaining.map(|n| n.to_be_bytes().to_vec()),
                expected_partition: 1,
            };
            assert!(read_management_capacity(&dev, 7, 1).unwrap().is_null());
        }
        let mut dev = Attributes {
            maximum: Some(1024u64.to_be_bytes().to_vec()),
            remaining: Some(vec![0; 7]),
            expected_partition: 1,
        };
        assert!(read_management_capacity(&dev, 7, 1).unwrap().is_null());
        dev.remaining = Some(0u64.to_be_bytes().to_vec());
        let full = read_management_capacity(&dev, 7, 1).unwrap();
        assert_eq!(full["maximum"], 1u64 << 30);
        assert_eq!(full["remaining"], 0);
        assert_eq!(full["generation"], 7);
        dev.remaining = Some(512u64.to_be_bytes().to_vec());
        assert_eq!(
            read_management_capacity(&dev, 7, 1).unwrap()["remaining"],
            512u64 << 20
        );
        dev.expected_partition = 0;
        let index = read_management_capacity(&dev, 7, 0).unwrap();
        assert_eq!(index["partition"], 0);
        assert_eq!(index["maximum"], 1u64 << 30);
    }
}
