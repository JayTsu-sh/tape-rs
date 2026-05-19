//! 换带器 (medium changer) 相关子命令。

use std::collections::HashMap;

use tape_rs::catalog::{self, Catalog, CapacitySnapshot};
use tape_rs::changer::commands::MediumChanger;
use tape_rs::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::mam;
use tape_rs::scsi::cdb;
use tape_rs::scsi::device::ScsiDevice;

use super::format_util::format_size;

pub fn cmd_inventory(path: &str, catalog_path: Option<&str>, no_drive_scan: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let mut changer = MediumChanger::new(&dev);

    let map = changer.load_address_map()?.clone();
    print_address_map(&map);

    let elements = changer.read_all_status()?;

    // A: 自动扫 /dev/sg*，对有磁带的 tape drive 读 MAM barcode + 容量；key = barcode serial。
    let realtime_capacity = if no_drive_scan {
        HashMap::new()
    } else {
        scan_drive_capacities(path)
    };
    // C: catalog 缓存；允许不存在或为空。
    let cached_capacity = load_cached_capacity(catalog_path);

    print_drive_section(&elements, &map, &realtime_capacity, &cached_capacity);
    print_storage_section(&elements, &realtime_capacity, &cached_capacity);
    print_ie_section(&elements, &realtime_capacity, &cached_capacity);

    Ok(())
}

fn print_address_map(map: &ElementAddressMap) {
    println!("=== Element Address Map ===");
    println!("  Transport:    start={:#06x}, count={}", map.transport_start, map.transport_count);
    println!("  Storage:      start={:#06x}, count={}", map.storage_start, map.storage_count);
    println!("  I/E:          start={:#06x}, count={}", map.ie_start, map.ie_count);
    println!("  Data Transfer:start={:#06x}, count={}", map.dt_start, map.dt_count);
    println!();
}

fn print_drive_section(
    elements: &[ElementStatus],
    map: &ElementAddressMap,
    realtime: &HashMap<String, CapacitySnapshot>,
    cached: &HashMap<String, CapacitySnapshot>,
) {
    println!("=== Drive Status ===");
    for elem in elements.iter().filter(|e| e.element_type == ElementType::DataTransfer) {
        print!("  {}", elem);
        if let Some(src) = elem.source_address {
            print!("  <- {}", describe_source(map, src));
        }
        if elem.full {
            print_capacity(elem.volume_tag.as_deref(), realtime, cached);
        }
        println!();
    }
    println!();
}

fn print_storage_section(
    elements: &[ElementStatus],
    realtime: &HashMap<String, CapacitySnapshot>,
    cached: &HashMap<String, CapacitySnapshot>,
) {
    println!("=== Storage Slots ===");
    for elem in elements.iter().filter(|e| e.element_type == ElementType::Storage) {
        print!("  {}", elem);
        if elem.full {
            print_capacity(elem.volume_tag.as_deref(), realtime, cached);
        }
        println!();
    }
    println!();
}

fn print_ie_section(
    elements: &[ElementStatus],
    realtime: &HashMap<String, CapacitySnapshot>,
    cached: &HashMap<String, CapacitySnapshot>,
) {
    println!("=== I/E Slots ===");
    for elem in elements.iter().filter(|e| e.element_type == ElementType::ImportExport) {
        print!("  {}", elem);
        if elem.full {
            print_capacity(elem.volume_tag.as_deref(), realtime, cached);
        }
        println!();
    }
}

/// realtime 优先，miss 回落 cached；都 miss 不输出容量列。
fn print_capacity(
    tag: Option<&str>,
    realtime: &HashMap<String, CapacitySnapshot>,
    cached: &HashMap<String, CapacitySnapshot>,
) {
    if let Some(c) = lookup_cached(realtime, tag) {
        print!("  {}", format_capacity(c, "realtime"));
    } else if let Some(c) = lookup_cached(cached, tag) {
        print!("  {}", format_capacity(c, "cached"));
    }
}

/// changer 的 PVolTag 通常是 `<6 字符 serial><2 字符介质代码>`（例如 8A0042L8），
/// 而 MAM 0x0806 只写入 serial 主体（8A0042）。先按完整 tag 查，miss 时
/// 去掉末尾 2 字符再查一次。
fn lookup_cached(
    cache: &HashMap<String, CapacitySnapshot>,
    tag: Option<&str>,
) -> Option<CapacitySnapshot> {
    let tag = tag?;
    if let Some(c) = cache.get(tag) {
        return Some(*c);
    }
    if tag.len() > 2 {
        let trimmed = &tag[..tag.len() - 2];
        if let Some(c) = cache.get(trimmed) {
            return Some(*c);
        }
    }
    None
}

/// 把 used / total 格式化成一行："used=3.2T / total=12.0T (free 8.8T) [realtime]"。
fn format_capacity(cap: CapacitySnapshot, source: &str) -> String {
    format!(
        "used={} / total={} (free {}) [{}]",
        format_size(cap.used()),
        format_size(cap.total),
        format_size(cap.total.saturating_sub(cap.used())),
        source,
    )
}

/// 扫 `/dev/sg*`，跳过 changer 本身，对每个 sg 节点尝试 `probe_tape_drive`。
/// 返回 `barcode → CapacitySnapshot` 映射；任一节点失败只记 debug 并跳过。
fn scan_drive_capacities(changer_path: &str) -> HashMap<String, CapacitySnapshot> {
    let mut out = HashMap::new();
    let dir = match std::fs::read_dir("/dev") {
        Ok(d) => d,
        Err(e) => {
            log::debug!("无法枚举 /dev 寻找 sg 节点: {}", e);
            return out;
        }
    };

    let mut sg_nodes: Vec<std::path::PathBuf> = dir
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|n| n.starts_with("sg") && n.len() > 2 && n[2..].chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        })
        .collect();
    sg_nodes.sort();

    let canon_changer = std::fs::canonicalize(changer_path).ok();
    for path in sg_nodes {
        if canon_changer.as_ref().map(|c| c == &path).unwrap_or(false) || path.to_str() == Some(changer_path) {
            continue;
        }
        let s = match path.to_str() {
            Some(s) => s,
            None => continue,
        };
        match probe_tape_drive(s) {
            Ok(Some((barcode, cap))) => {
                log::debug!("drive {} barcode={} total={} remaining={}", s, barcode, cap.total, cap.remaining);
                out.insert(barcode, cap);
            }
            Ok(None) => log::debug!("{} 非磁带机或未载带，跳过", s),
            Err(e) => log::debug!("{} 探测失败: {}", s, e),
        }
    }
    out
}

/// 打开 sg 节点，通过 INQUIRY 过滤磁带机（peripheral_device_type=0x01），
/// 读 MAM 0x0806 barcode + 容量。非磁带机或未载带时返回 `Ok(None)`。
fn probe_tape_drive(path: &str) -> Result<Option<(String, CapacitySnapshot)>> {
    let dev = ScsiDevice::open(path)?;

    let cdb_bytes = cdb::inquiry(96);
    let mut buf = [0u8; 96];
    dev.execute_read(&cdb_bytes, &mut buf, 10_000)?;
    let ptype = buf[0] & 0x1F;
    if ptype != 0x01 {
        return Ok(None);
    }

    let mam_dev = mam::Mam::new(&dev);
    let barcode = match mam_dev.read_attribute(mam::ATTR_BARCODE)? {
        Some(a) => std::str::from_utf8(&a.value)
            .unwrap_or("")
            .trim_matches(|c: char| c == '\0' || c == ' ')
            .to_string(),
        None => return Ok(None),
    };
    if barcode.is_empty() {
        return Ok(None);
    }

    let cap = mam::read_volume_capacity(&dev)?;
    if cap.total == 0 {
        return Ok(None);
    }
    Ok(Some((
        barcode,
        CapacitySnapshot { total: cap.total, remaining: cap.remaining },
    )))
}

/// 容忍 catalog 缺失：打不开 / 查不到就返回空 map，不中断 inventory。
fn load_cached_capacity(catalog_path: Option<&str>) -> HashMap<String, CapacitySnapshot> {
    let path = match catalog::resolve_path(catalog_path) {
        Ok(p) => p,
        Err(e) => {
            log::debug!("catalog 路径解析失败: {}", e);
            return HashMap::new();
        }
    };
    // 不存在就直接返回空，避免自动创建一个空 DB。
    if !path.exists() {
        return HashMap::new();
    }
    match Catalog::open(&path).and_then(|c| catalog::query::capacity_by_barcode(&c)) {
        Ok(map) => map,
        Err(e) => {
            log::debug!("读 catalog 容量失败: {}", e);
            HashMap::new()
        }
    }
}

/// 把 element 地址翻成人看得懂的来源描述，和 ElementStatus::Display 用同一套编号（原始地址）。
fn describe_source(map: &ElementAddressMap, addr: u16) -> String {
    if addr >= map.storage_start && addr < map.storage_start + map.storage_count {
        format!("Storage {}", addr)
    } else if addr >= map.ie_start && addr < map.ie_start + map.ie_count {
        format!("I/E {}", addr)
    } else if addr >= map.dt_start && addr < map.dt_start + map.dt_count {
        format!("Drive {}", addr)
    } else if addr >= map.transport_start && addr < map.transport_start + map.transport_count {
        format!("Transport {}", addr)
    } else {
        format!("addr={:#06x}", addr)
    }
}

/// slot 为 1-based，转换为绝对 storage element 地址。
fn slot_to_addr(map: &ElementAddressMap, slot: u16) -> Result<u16> {
    if slot == 0 || slot > map.storage_count {
        return Err(TapeError::MoveFailed {
            reason: format!("slot {} 越界（可用 1..={}）", slot, map.storage_count),
        });
    }
    Ok(map.storage_start + slot - 1)
}

/// drive 为 0-based，转换为绝对 data transfer element 地址。
fn drive_to_addr(map: &ElementAddressMap, drive: u16) -> Result<u16> {
    if drive >= map.dt_count {
        return Err(TapeError::MoveFailed {
            reason: format!("drive {} 越界（可用 0..{}）", drive, map.dt_count),
        });
    }
    Ok(map.dt_start + drive)
}

fn parse_addr(s: &str) -> Result<u16> {
    let trimmed = s.trim_start_matches("0x").trim_start_matches("0X");
    let radix = if s.starts_with("0x") || s.starts_with("0X") { 16 } else { 10 };
    u16::from_str_radix(trimmed, radix).map_err(|e| TapeError::MoveFailed {
        reason: format!("无法解析地址 '{}': {}", s, e),
    })
}

/// MOVE MEDIUM 在 SCSI 层成功返回不代表物理动作真的发生：某些库 firmware
/// 在 drive 被外部 initiator PR-锁住时会静默拒绝。事后读一次 element status
/// 验证 source 已空、dest 已满，否则报 InconsistentState 让调用方明确感知。
///
/// 注意 verify 探针自身的 SCSI 错误（read_all_status 失败）会被包成
/// `InconsistentState`，不直接透传 — 因为此时 MOVE 已经发出，结果不可知，
/// 跟 "MOVE 本身失败" 的语义不同。
fn verify_move_result(
    changer: &MediumChanger,
    map: &ElementAddressMap,
    source_addr: u16,
    dest_addr: u16,
) -> Result<()> {
    let elements = changer.read_all_status().map_err(|e| {
        TapeError::InconsistentState(format!(
            "verify probe failed after MOVE MEDIUM {:#06x} → {:#06x}: {}; \
             move may or may not have completed, run `tape-rs inventory` to confirm",
            source_addr, dest_addr, e,
        ))
    })?;
    check_move_result(&elements, map, source_addr, dest_addr)
}

/// MOVE 后纯函数检查：给定 element 状态快照，判断 source/dest 是否符合预期。
/// 分离出来便于单测，verify_move_result 负责数据采集 + 错误包装。
fn check_move_result(
    elements: &[ElementStatus],
    map: &ElementAddressMap,
    source_addr: u16,
    dest_addr: u16,
) -> Result<()> {
    let source = elements.iter().find(|e| e.address == source_addr);
    let dest = elements.iter().find(|e| e.address == dest_addr);
    match (source, dest) {
        (Some(s), Some(d)) if !s.full && d.full => Ok(()),
        (Some(s), Some(d)) => Err(TapeError::InconsistentState(format!(
            "MOVE MEDIUM {} → {} 返回成功但 element 状态未变化: \
             source full={} (期望 false), dest full={} (期望 true)。\
             可能原因: 目标 drive 被另一个 initiator 持有 SCSI Persistent Reservation, \
             用 `sg_persist --in -r <drive>` 排查。",
            describe_source(map, source_addr),
            describe_source(map, dest_addr),
            s.full, d.full,
        ))),
        _ => Err(TapeError::InconsistentState(format!(
            "MOVE MEDIUM 后无法在 element status 中定位 source {:#06x} 或 dest {:#06x}",
            source_addr, dest_addr,
        ))),
    }
}

pub fn cmd_load(changer_path: &str, slot: u16, drive: u16) -> Result<()> {
    let dev = ScsiDevice::open(changer_path)?;
    let mut changer = MediumChanger::new(&dev);
    let map = changer.load_address_map()?.clone();

    let source_addr = slot_to_addr(&map, slot)?;
    let dest_addr = drive_to_addr(&map, drive)?;

    println!("装载: slot {} (addr {:#06x}) → drive {} (addr {:#06x})", slot, source_addr, drive, dest_addr);
    changer.move_medium(source_addr, dest_addr)?;
    verify_move_result(&changer, &map, source_addr, dest_addr)?;
    println!("装载完成");
    Ok(())
}

pub fn cmd_unload(changer_path: &str, drive: u16, slot: u16) -> Result<()> {
    let dev = ScsiDevice::open(changer_path)?;
    let mut changer = MediumChanger::new(&dev);
    let map = changer.load_address_map()?.clone();

    let source_addr = drive_to_addr(&map, drive)?;
    let dest_addr = slot_to_addr(&map, slot)?;

    println!("卸载: drive {} (addr {:#06x}) → slot {} (addr {:#06x})", drive, source_addr, slot, dest_addr);
    changer.move_medium(source_addr, dest_addr)?;
    verify_move_result(&changer, &map, source_addr, dest_addr)?;
    println!("卸载完成");
    Ok(())
}

pub fn cmd_move(changer_path: &str, from_slot: u16, to_slot: Option<u16>, to_drive: Option<u16>) -> Result<()> {
    let dev = ScsiDevice::open(changer_path)?;
    let mut changer = MediumChanger::new(&dev);
    let map = changer.load_address_map()?.clone();

    let source_addr = slot_to_addr(&map, from_slot)?;
    let dest_addr = if let Some(slot) = to_slot {
        slot_to_addr(&map, slot)?
    } else if let Some(drive) = to_drive {
        drive_to_addr(&map, drive)?
    } else {
        return Err(TapeError::MoveFailed {
            reason: "必须指定 --to-slot 或 --to-drive".into(),
        });
    };

    println!("移动: {:#06x} → {:#06x}", source_addr, dest_addr);
    changer.move_medium(source_addr, dest_addr)?;
    verify_move_result(&changer, &map, source_addr, dest_addr)?;
    println!("移动完成");
    Ok(())
}

pub fn cmd_init(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let changer = MediumChanger::new(&dev);
    changer.initialize_element_status()?;
    println!("库存扫描完成");
    Ok(())
}

pub fn cmd_exchange(path: &str, source: &str, dest1: &str, dest2: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let mut changer = MediumChanger::new(&dev);
    changer.load_address_map()?;

    let src = parse_addr(source)?;
    let d1 = parse_addr(dest1)?;
    let d2 = parse_addr(dest2)?;

    println!("交换: {:#06x} ↔ {:#06x} ↔ {:#06x}", src, d1, d2);
    changer.exchange_medium(src, d1, d2)?;
    println!("交换完成");
    Ok(())
}

pub fn cmd_prevent_removal(path: &str, prevent: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let changer = MediumChanger::new(&dev);
    changer.prevent_medium_removal(prevent)?;
    println!("{} 介质移除", if prevent { "已禁止" } else { "已允许" });
    Ok(())
}

pub fn cmd_import(path: &str, ie: u16, slot: u16) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let mut changer = MediumChanger::new(&dev);
    changer.load_address_map()?;
    changer.import(ie, slot)?;
    println!("导入完成");
    Ok(())
}

pub fn cmd_export(path: &str, slot: u16, ie: u16) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let mut changer = MediumChanger::new(&dev);
    changer.load_address_map()?;
    changer.export(slot, ie)?;
    println!("导出完成");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_map() -> ElementAddressMap {
        ElementAddressMap {
            transport_start: 0x0000, transport_count: 1,
            storage_start:   0x03e9, storage_count:   35,
            ie_start:        0x0065, ie_count:        5,
            dt_start:        0x0001, dt_count:        2,
        }
    }

    fn elem(addr: u16, ty: ElementType, full: bool, tag: Option<&str>) -> ElementStatus {
        ElementStatus {
            address: addr,
            element_type: ty,
            full,
            volume_tag: tag.map(String::from),
            source_address: None,
        }
    }

    #[test]
    fn check_move_result_ok_when_source_empty_dest_full() {
        let map = fixture_map();
        let src = 0x03e9;
        let dst = 0x0002;
        let elements = vec![
            elem(src, ElementType::Storage,      false, None),
            elem(dst, ElementType::DataTransfer, true,  Some("8A0043L8")),
        ];
        assert!(check_move_result(&elements, &map, src, dst).is_ok());
    }

    #[test]
    fn check_move_result_inconsistent_when_states_unchanged() {
        let map = fixture_map();
        let src = 0x03e9;
        let dst = 0x0002;
        // 模拟 firmware 静默拒绝: source 仍 Full, dest 仍 Empty
        let elements = vec![
            elem(src, ElementType::Storage,      true,  Some("8A0043L8")),
            elem(dst, ElementType::DataTransfer, false, None),
        ];
        let err = check_move_result(&elements, &map, src, dst).unwrap_err();
        match err {
            TapeError::InconsistentState(msg) => {
                assert!(msg.contains("source full=true"));
                assert!(msg.contains("dest full=false"));
            }
            other => panic!("expected InconsistentState, got {:?}", other),
        }
    }

    #[test]
    fn check_move_result_inconsistent_when_only_source_emptied() {
        let map = fixture_map();
        let src = 0x03e9;
        let dst = 0x0002;
        // 半成品状态: source 已被取走但 dest 没收到
        let elements = vec![
            elem(src, ElementType::Storage,      false, None),
            elem(dst, ElementType::DataTransfer, false, None),
        ];
        assert!(matches!(
            check_move_result(&elements, &map, src, dst),
            Err(TapeError::InconsistentState(_))
        ));
    }

    #[test]
    fn check_move_result_inconsistent_when_element_missing() {
        let map = fixture_map();
        let src = 0x03e9;
        let dst = 0x0002;
        // 模拟 READ ELEMENT STATUS 没返回 source 那一项
        let elements = vec![
            elem(dst, ElementType::DataTransfer, true, Some("8A0043L8")),
        ];
        let err = check_move_result(&elements, &map, src, dst).unwrap_err();
        match err {
            TapeError::InconsistentState(msg) => {
                assert!(msg.contains("0x03e9"));
            }
            other => panic!("expected InconsistentState, got {:?}", other),
        }
    }
}
