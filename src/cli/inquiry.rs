//! INQUIRY 子命令实现。
//!
//! 自动模式 (方案 B)：用 VPD page 0x83 (Device Identification) NAA designator 合并：
//!   * association = SCSI Target Device (10b) → 同一物理带库
//!   * association = Logical Unit (00b)      → 同一 LU 的多路径
//! VPD 0x83 不可用时退回 sysfs H:C:T 分组 + (vendor, product, serial) 比较。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::cdb;
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::{enumerate_sg_nodes, read_unit_serial, standard_inquiry};

const PT_TAPE: u8 = 0x01;
const PT_CHANGER: u8 = 0x08;

pub fn cmd_inquiry(json: bool) -> Result<()> {
    inquiry_auto(json)
}

// =========================================================
//   数据结构
// =========================================================

#[derive(Debug, Clone, Copy)]
struct Hctl {
    host: u32,
    channel: u32,
    target: u32,
    lun: u32,
}

#[derive(Debug, Clone)]
struct InquiryInfo {
    peripheral_type: u8,
    vendor: String,
    product: String,
    revision: String,
    serial: Option<String>,
}

#[derive(Debug, Clone)]
struct SgEntry {
    path: PathBuf,
    hctl: Option<Hctl>,
    inquiry: std::result::Result<InquiryInfo, String>,
    /// VPD 0x83 LU identifier (association = 00b)；多路径用同 NAA 合并。
    lu_id: Option<Vec<u8>>,
}

/// 一台物理带库（或独立驱动器集合）。
struct Library {
    changer_info: Option<InquiryInfo>,
    changer_paths: Vec<SgEntry>,
    drives: Vec<DriveCluster>,
}

/// 同一物理驱动器的全部 SCSI 路径。`key` 在 push 时算一次缓存，避免
/// add_path_to_cluster 每次 O(n) 比较时重新克隆 lu_id Vec<u8>。
struct DriveCluster {
    info: InquiryInfo,
    paths: Vec<SgEntry>,
    key: Vec<u8>,
}

// =========================================================
//   入口
// =========================================================

fn inquiry_auto(json: bool) -> Result<()> {
    let nodes = enumerate_sg_nodes()?;
    if nodes.is_empty() {
        return Err(TapeError::NotReady(
            "系统中未发现任何 /dev/sg* 节点".to_string(),
        ));
    }

    let entries: Vec<SgEntry> = nodes.iter().map(|p| probe_node(p)).collect();

    // build_libraries 内部对每个 entry 检查 peripheral_type；非 tape/changer 的连通
    // 分量会产生空 Library，最后 filter 掉，避免提前 clone。
    let libraries: Vec<Library> = build_libraries(&entries)
        .into_iter()
        .filter(|l| l.changer_info.is_some() || !l.drives.is_empty())
        .collect();

    if json {
        let out = build_json(&libraries);
        // serde_json::Value 序列化到 String 不涉及 IO，理论上不会失败。
        let s = serde_json::to_string_pretty(&out)
            .expect("serializing serde_json::Value to String is infallible");
        println!("{}", s);
        return Ok(());
    }

    if libraries.is_empty() {
        println!("未检测到磁带库 / 磁带驱动器。");
        println!("以下是系统中其它 SCSI generic 设备：");
        let groups = group_by_sysfs_target(&entries);
        for (i, (target, g)) in groups.iter().enumerate() {
            println!();
            print_other_target_group(i + 1, *target, g);
        }
        return Ok(());
    }

    println!(
        "已发现 {} 个磁带库 / 子系统（共扫描 {} 个 /dev/sg* 节点）",
        libraries.len(),
        nodes.len()
    );
    for (i, lib) in libraries.iter().enumerate() {
        println!();
        print_library(i + 1, lib);
    }
    Ok(())
}

/// 构造 JSON 输出。每个 drive 自带 `device`（drive 自己的 sg）+ `control_path`
/// （同 SCSI target 上的 changer LU sg，即 IBM CPF 术语里的 control path），
/// 反映 LUN0/LUN1 同 target 的物理配对关系。
fn build_json(libraries: &[Library]) -> serde_json::Value {
    use serde_json::json;
    let libs: Vec<serde_json::Value> = libraries
        .iter()
        .map(|lib| {
            let changer = lib.changer_info.as_ref().map(|c| {
                json!({
                    "vendor": c.vendor,
                    "product": c.product,
                    "revision": c.revision,
                    "serial": c.serial,
                })
            });
            let drives: Vec<serde_json::Value> = lib
                .drives
                .iter()
                .map(|d| {
                    let first = d.paths.first();
                    let device = first.map(|e| e.path.display().to_string());
                    let control_path = first
                        .and_then(|e| paired_changer_path(e, &lib.changer_paths))
                        .map(|p| p.display().to_string());
                    json!({
                        "vendor": d.info.vendor,
                        "product": d.info.product,
                        "revision": d.info.revision,
                        "serial": d.info.serial,
                        "device": device,
                        "control_path": control_path,
                    })
                })
                .collect();
            json!({
                "changer": changer,
                "drives": drives,
            })
        })
        .collect();
    json!({
        "libraries": libs,
    })
}

/// 找出与 drive 共享 SCSI target 的 changer LU 路径。
/// 在 TS4300 等 IBM 带库里，drive 的控制器会把 changer 暴露为同 target 的 LUN 1。
fn paired_changer_path(drive_path: &SgEntry, changer_paths: &[SgEntry]) -> Option<PathBuf> {
    let dh = drive_path.hctl?;
    changer_paths
        .iter()
        .find(|c| {
            c.hctl
                .map(|h| (h.host, h.channel, h.target))
                == Some((dh.host, dh.channel, dh.target))
        })
        .map(|c| c.path.clone())
}

// =========================================================
//   合并：union-find（同 lu_id 或同 sysfs target 即合并）
// =========================================================

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self { parent: (0..n).collect() }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// 合并规则：
///   1. 同 sysfs (host, channel, target) → 同一 SCSI 控制器下的兄弟 LU，属于同一带库；
///   2. 同 VPD 0x83 LU NAA (association = 00b) → 同一 LU 的多条控制路径（CPF / multipath）；
///   3. lu_id 缺失时回退到 (vendor, product, serial) 比较（plan-A 兜底）。
/// 三种关系一起跑 union-find，连通分量 = 物理带库。
fn build_libraries(entries: &[SgEntry]) -> Vec<Library> {
    let n = entries.len();
    let mut uf = UnionFind::new(n);

    union_by_key(&mut uf, entries, |e| e.hctl.map(|h| (h.host, h.channel, h.target)));
    union_by_key(&mut uf, entries, |e| e.lu_id.clone());
    // 序列号兜底：所有带 serial 的条目都参与，混合固件下（部分路径无 lu_id）
    // 才能跨 lu_id 缺失的边界把同一物理设备连通。
    union_by_key(&mut uf, entries, |e| {
        let info = e.inquiry.as_ref().ok()?;
        let serial = info.serial.as_ref()?;
        Some(format!("{}|{}|{}", info.vendor, info.product, serial))
    });

    let mut libraries: Vec<Library> = components(&mut uf, entries.len())
        .into_iter()
        .map(|idxs| assemble_library(entries, &idxs))
        .collect();

    // 输出顺序：有 changer 的库在前，按 vendor/product 字母序。
    // sort_by_cached_key 避免每次比较重新克隆字符串。
    libraries.sort_by_cached_key(|l| {
        (
            l.changer_info.is_none(),
            l.changer_info
                .as_ref()
                .map(|c| (c.vendor.clone(), c.product.clone()))
                .unwrap_or_default(),
        )
    });
    libraries
}

/// 按提取 key 把 entries 分桶，组内两两 union。key 为 None 的条目不参与。
fn union_by_key<K, F>(uf: &mut UnionFind, entries: &[SgEntry], key_of: F)
where
    K: Ord,
    F: Fn(&SgEntry) -> Option<K>,
{
    let mut buckets: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(k) = key_of(e) {
            buckets.entry(k).or_default().push(i);
        }
    }
    for idxs in buckets.values() {
        for w in idxs.windows(2) {
            uf.union(w[0], w[1]);
        }
    }
}

/// 跑完 union 之后按 root 聚合，每个连通分量返回一组下标。
fn components(uf: &mut UnionFind, n: usize) -> Vec<Vec<usize>> {
    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = uf.find(i);
        by_root.entry(r).or_default().push(i);
    }
    by_root.into_values().collect()
}

/// 把一个连通分量的下标列表拼成 Library：changer 走多路径合并，drive 按 lu_id 去重。
fn assemble_library(entries: &[SgEntry], idxs: &[usize]) -> Library {
    let mut changer_clusters: Vec<DriveCluster> = Vec::new();
    let mut drive_clusters: Vec<DriveCluster> = Vec::new();
    for &i in idxs {
        let e = &entries[i];
        if let Ok(info) = &e.inquiry {
            let target = match info.peripheral_type {
                PT_CHANGER => &mut changer_clusters,
                PT_TAPE => &mut drive_clusters,
                _ => continue,
            };
            add_path_to_cluster(target, info, e);
        }
    }
    let (changer_info, changer_paths) = if changer_clusters.is_empty() {
        (None, Vec::new())
    } else {
        if changer_clusters.len() > 1 {
            // 同一连通分量包含两个不同 LU id 的 changer：表示 union-find
            // 错误地把两台物理带库并到了一起。只能展示第一台，但要让用户知道。
            log::warn!(
                "library 合并异常：发现 {} 个不同的 changer LU 在同一连通分量内，仅显示第一个的身份信息",
                changer_clusters.len()
            );
        }
        let info = changer_clusters[0].info.clone();
        let paths: Vec<SgEntry> = changer_clusters.into_iter().flat_map(|c| c.paths).collect();
        (Some(info), paths)
    };
    Library { changer_info, changer_paths, drives: drive_clusters }
}

fn add_path_to_cluster(clusters: &mut Vec<DriveCluster>, info: &InquiryInfo, entry: &SgEntry) {
    let key = lu_key_of(info, entry);
    for c in clusters.iter_mut() {
        if c.key == key {
            c.paths.push(entry.clone());
            return;
        }
    }
    clusters.push(DriveCluster {
        info: info.clone(),
        paths: vec![entry.clone()],
        key,
    });
}

/// 优先用 VPD 0x83 LU NAA；没有就 (vendor, product, serial)；都没有用 path 兜底。
fn lu_key_of(info: &InquiryInfo, entry: &SgEntry) -> Vec<u8> {
    if let Some(id) = &entry.lu_id {
        return id.clone();
    }
    let mut k = Vec::new();
    k.extend(info.vendor.as_bytes());
    k.push(b'|');
    k.extend(info.product.as_bytes());
    k.push(b'|');
    match &info.serial {
        Some(s) => k.extend(s.as_bytes()),
        None => k.extend(entry.path.display().to_string().as_bytes()),
    }
    k
}

// =========================================================
//   探测
// =========================================================

fn probe_node(path: &Path) -> SgEntry {
    let sg_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let hctl = read_hctl(sg_name);
    let mut entry = SgEntry {
        path: path.to_path_buf(),
        hctl,
        inquiry: Err(String::new()),
        lu_id: None,
    };
    // 一次打开设备，标准 INQUIRY + VPD 0x80 (serial) + VPD 0x83 (LU id) 都复用同一 fd。
    let path_str = match path.to_str() {
        Some(s) => s,
        None => {
            entry.inquiry = Err(format!("invalid path {:?}", path));
            return entry;
        }
    };
    let dev = match ScsiDevice::open(path_str) {
        Ok(d) => d,
        Err(e) => {
            entry.inquiry = Err(e.to_string());
            return entry;
        }
    };
    match probe_inquiry(&dev) {
        Ok(info) => entry.inquiry = Ok(info),
        Err(e) => {
            entry.inquiry = Err(e.to_string());
            return entry;
        }
    }
    entry.lu_id = read_vpd83_lu_id(&dev);
    entry
}

fn probe_inquiry(dev: &ScsiDevice) -> Result<InquiryInfo> {
    let std = standard_inquiry(dev)?;
    Ok(InquiryInfo {
        peripheral_type: std.peripheral_type,
        vendor: std.vendor,
        product: std.product,
        revision: std.revision,
        serial: read_unit_serial(dev),
    })
}

/// 读 VPD page 0x83，挑出 association=00b (Logical Unit) 的 designator。
/// 优先级：NAA (type 0x3) > EUI-64 (0x2) > SCSI Name String (0x8) > T10 vendor ID (0x1)。
fn read_vpd83_lu_id(dev: &ScsiDevice) -> Option<Vec<u8>> {
    let cdb_bytes = cdb::inquiry_vpd(0x83, 252);
    let mut buf = [0u8; 252];
    let result = dev.execute_read(&cdb_bytes, &mut buf, 10_000).ok()?;
    let n = result.transferred;
    if n < 4 || buf[1] != 0x83 {
        return None;
    }
    let page_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (4 + page_len).min(n);
    let designators = parse_vpd83_designators(&buf[..end]);

    // 空 designator (id_length=0) 不可作为合并 key——多个返回空 designator
    // 的设备会误并成同一个 library，因此过滤掉。
    for ty in [3u8, 2, 8, 1] {
        if let Some(d) = designators
            .iter()
            .find(|d| d.association == 0 && d.designator_type == ty && !d.data.is_empty())
        {
            return Some(d.data.clone());
        }
    }
    None
}

#[derive(Debug)]
struct Vpd83Designator {
    association: u8,
    designator_type: u8,
    data: Vec<u8>,
}

fn parse_vpd83_designators(page: &[u8]) -> Vec<Vpd83Designator> {
    let mut out = Vec::new();
    if page.len() < 4 {
        return out;
    }
    let mut i = 4;
    while i + 4 <= page.len() {
        let association = (page[i + 1] >> 4) & 0x03;
        let designator_type = page[i + 1] & 0x0F;
        let dlen = page[i + 3] as usize;
        if i + 4 + dlen > page.len() {
            break;
        }
        out.push(Vpd83Designator {
            association,
            designator_type,
            data: page[i + 4..i + 4 + dlen].to_vec(),
        });
        i += 4 + dlen;
    }
    out
}

fn read_hctl(sg_name: &str) -> Option<Hctl> {
    if sg_name.is_empty() {
        return None;
    }
    let link = std::fs::read_link(format!("/sys/class/scsi_generic/{}/device", sg_name)).ok()?;
    let last = link.file_name()?.to_str()?;
    let parts: Vec<&str> = last.split(':').collect();
    if parts.len() != 4 {
        return None;
    }
    Some(Hctl {
        host: parts[0].parse().ok()?,
        channel: parts[1].parse().ok()?,
        target: parts[2].parse().ok()?,
        lun: parts[3].parse().ok()?,
    })
}

fn group_by_sysfs_target(
    entries: &[SgEntry],
) -> BTreeMap<Option<(u32, u32, u32)>, Vec<SgEntry>> {
    let mut buckets: BTreeMap<Option<(u32, u32, u32)>, Vec<SgEntry>> = BTreeMap::new();
    for e in entries {
        let key = e.hctl.map(|h| (h.host, h.channel, h.target));
        buckets.entry(key).or_default().push(e.clone());
    }
    for v in buckets.values_mut() {
        v.sort_by_key(|e| e.hctl.map(|h| h.lun).unwrap_or(u32::MAX));
    }
    buckets
}

// =========================================================
//   输出
// =========================================================

fn print_library(idx: usize, lib: &Library) {
    // header
    match &lib.changer_info {
        Some(c) => {
            let serial_part = c
                .serial
                .as_ref()
                .map(|s| format!("   序列号 {}", s))
                .unwrap_or_default();
            println!("┌─ 带库 #{}   {} {}{}", idx, c.vendor, c.product, serial_part);
        }
        None => {
            println!("┌─ 独立驱动器 #{}（未检测到 changer）", idx);
        }
    }

    // 驱动器（按要求放在 changer 之前）：每个 drive 同时展示自己的 device 和
    // 同 SCSI target 上 changer LU 的 control_path（IBM CPF 配对关系）。
    for (i, drive) in lib.drives.iter().enumerate() {
        println!("│");
        let label = if lib.drives.len() > 1 {
            format!("驱动器 {}", (b'A' + i as u8) as char)
        } else {
            "驱动器".to_string()
        };
        let multipath = if drive.paths.len() > 1 {
            format!("   ({} 条路径)", drive.paths.len())
        } else {
            String::new()
        };
        println!(
            "│  {}   {} {}{}",
            label, drive.info.vendor, drive.info.product, multipath
        );
        println!("│    固件版本：{}", drive.info.revision);
        if let Some(s) = &drive.info.serial {
            println!("│    序列号：{}", s);
        }
        for drive_path in &drive.paths {
            println!("│    设备：{}", drive_path.path.display());
            if let Some(cp) = paired_changer_path(drive_path, &lib.changer_paths) {
                println!("│    控制路径：{}", cp.display());
            }
        }
    }

    // 换带器：身份信息为主，控制路径已下放到各 drive 条目下。
    if let Some(c) = &lib.changer_info {
        println!("│");
        let cpf = if lib.changer_paths.len() > 1 {
            format!("   ({} 条控制路径，故障切换 CPF)", lib.changer_paths.len())
        } else {
            String::new()
        };
        println!("│  换带器   {} {}{}", c.vendor, c.product, cpf);
        println!("│    固件版本：{}", c.revision);
        if let Some(s) = &c.serial {
            println!("│    序列号：{}", s);
        }
    }

    println!("└─");
}

fn print_other_target_group(
    idx: usize,
    target: Option<(u32, u32, u32)>,
    group: &[SgEntry],
) {
    let topology = target
        .map(|(h, c, t)| format!("host{}:channel{}:target{}", h, c, t))
        .unwrap_or_else(|| "(sysfs 拓扑未知)".to_string());
    println!("[#{}] {}", idx, topology);
    for e in group {
        let lun = e.hctl.map(|h| h.lun.to_string()).unwrap_or_else(|| "?".into());
        match &e.inquiry {
            Ok(info) => {
                println!(
                    "  {}  (LUN {})  {} — {} {} rev {}",
                    e.path.display(),
                    lun,
                    role_label_cn(info.peripheral_type),
                    info.vendor,
                    info.product,
                    info.revision
                );
            }
            Err(err) => {
                println!(
                    "  {}  (LUN {})  INQUIRY 失败: {}",
                    e.path.display(),
                    lun,
                    err
                );
            }
        }
    }
}

fn role_label_cn(t: u8) -> &'static str {
    match t {
        PT_TAPE => "驱动器",
        PT_CHANGER => "换带器",
        0x00 => "块设备",
        0x05 => "光驱",
        0x0C => "阵列控制器",
        0x0D => "SES 机箱",
        _ => "其它",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- VPD 0x83 designator 解析 ----

    #[test]
    fn parse_vpd83_empty_page_returns_empty() {
        // page header + zero designators
        let buf = [0, 0x83, 0, 0];
        assert!(parse_vpd83_designators(&buf).is_empty());
    }

    #[test]
    fn parse_vpd83_one_naa_lu_designator() {
        // page header (4) + designator: code_set=binary, assoc=00b, type=NAA(3), len=8
        let mut buf = vec![0, 0x83, 0, 12];
        buf.extend_from_slice(&[
            0x01, // code_set = binary
            0x03, // association=00, type=3 (NAA)
            0x00, // reserved
            0x08, // designator length
            0x50, 0x00, 0xe1, 0x11, 0x70, 0x18, 0x60, 0x5e, // 8-byte NAA
        ]);
        let designators = parse_vpd83_designators(&buf);
        assert_eq!(designators.len(), 1);
        assert_eq!(designators[0].association, 0);
        assert_eq!(designators[0].designator_type, 3);
        assert_eq!(designators[0].data.len(), 8);
        assert_eq!(designators[0].data[0], 0x50);
    }

    #[test]
    fn parse_vpd83_truncated_designator_dropped() {
        // header claims 8 bytes designator, but only 4 bytes follow → should stop
        let buf = vec![0, 0x83, 0, 12, 0x01, 0x03, 0, 0x08, 0xde, 0xad];
        let designators = parse_vpd83_designators(&buf);
        assert!(designators.is_empty());
    }

    // ---- build_libraries 合并逻辑 ----

    fn mk_entry(
        path: &str,
        hctl: Option<(u32, u32, u32, u32)>,
        peripheral: u8,
        vendor: &str,
        product: &str,
        serial: Option<&str>,
        lu_id: Option<Vec<u8>>,
    ) -> SgEntry {
        SgEntry {
            path: PathBuf::from(path),
            hctl: hctl.map(|(h, c, t, l)| Hctl { host: h, channel: c, target: t, lun: l }),
            inquiry: Ok(InquiryInfo {
                peripheral_type: peripheral,
                vendor: vendor.into(),
                product: product.into(),
                revision: "x".into(),
                serial: serial.map(String::from),
            }),
            lu_id,
        }
    }

    #[test]
    fn build_libraries_empty() {
        assert!(build_libraries(&[]).is_empty());
    }

    #[test]
    fn build_libraries_ts4300_cpf_merges_to_one() {
        // 真带库典型场景：2 个 SCSI target，每个 target 内 LUN0=drive + LUN1=changer。
        // 两个 changer LU 共享同一 NAA → 同一物理 changer。
        let changer_naa = vec![0x50, 0x00, 0xe1, 0x11, 0x70, 0x18, 0x60, 0x5e];
        let entries = vec![
            mk_entry("/dev/sg1", Some((6, 0, 0, 0)), PT_TAPE, "IBM", "ULT3580-HH8", Some("HH8SERIAL"),
                Some(vec![0x50, 0x00, 0xe1, 0x11, 0x70, 0x18, 0x60, 0x6f])),
            mk_entry("/dev/sg2", Some((6, 0, 0, 1)), PT_CHANGER, "IBM", "3573-TL", Some("LIBSERIAL"),
                Some(changer_naa.clone())),
            mk_entry("/dev/sg3", Some((6, 0, 1, 0)), PT_TAPE, "IBM", "ULT3580-TD8", Some("TD8SERIAL"),
                Some(vec![0x50, 0x00, 0xe1, 0x11, 0x70, 0x18, 0x60, 0x5b])),
            mk_entry("/dev/sg4", Some((6, 0, 1, 1)), PT_CHANGER, "IBM", "3573-TL", Some("LIBSERIAL"),
                Some(changer_naa)),
        ];
        let libs = build_libraries(&entries);
        assert_eq!(libs.len(), 1, "CPF 应该合并为 1 个 library");
        let lib = &libs[0];
        assert_eq!(lib.changer_paths.len(), 2, "changer 有 2 条控制路径");
        assert_eq!(lib.drives.len(), 2, "2 个不同 drive");
    }

    #[test]
    fn build_libraries_vtl_no_shared_target_no_merge() {
        // VTL 风格：3 个 sg 各占独立 target，互不相关 LU id。
        let entries = vec![
            mk_entry("/dev/sg3", Some((4, 0, 0, 0)), PT_CHANGER, "IBM", "03584L32", Some("VTL_CHG"),
                Some(vec![1, 2, 3])),
            mk_entry("/dev/sg4", Some((3, 0, 0, 0)), PT_TAPE, "IBM", "ULT3580-TD8", Some("VTL_DRV_A"),
                Some(vec![4, 5, 6])),
            mk_entry("/dev/sg5", Some((5, 0, 0, 0)), PT_TAPE, "IBM", "ULT3580-TD8", Some("VTL_DRV_B"),
                Some(vec![7, 8, 9])),
        ];
        let libs = build_libraries(&entries);
        assert_eq!(libs.len(), 3, "VTL 没法合并，应保留 3 个独立项");
    }

    #[test]
    fn build_libraries_fallback_by_serial_when_no_lu_id() {
        // 老设备没 VPD 0x83 lu_id：靠 (vendor, product, serial) 兜底合并
        let entries = vec![
            mk_entry("/dev/sg1", Some((1, 0, 0, 1)), PT_CHANGER, "OLD", "LIB", Some("LIB1"), None),
            mk_entry("/dev/sg2", Some((1, 0, 1, 1)), PT_CHANGER, "OLD", "LIB", Some("LIB1"), None),
        ];
        let libs = build_libraries(&entries);
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].changer_paths.len(), 2);
    }

    #[test]
    fn build_libraries_mixed_lu_id_and_no_lu_id_same_serial_merges() {
        // 混合固件场景：一条路径返回 VPD 0x83 LU id，另一条没返回但 serial 相同。
        // 修 fix #1 前会拆成 2 个 library；修后靠 serial 兜底合并。
        let entries = vec![
            mk_entry("/dev/sg1", Some((1, 0, 0, 1)), PT_CHANGER, "MIX", "LIB", Some("MIXLIB"),
                Some(vec![0xa, 0xb, 0xc])),
            mk_entry("/dev/sg2", Some((1, 0, 1, 1)), PT_CHANGER, "MIX", "LIB", Some("MIXLIB"),
                None),
        ];
        let libs = build_libraries(&entries);
        assert_eq!(libs.len(), 1, "同 serial 应跨 lu_id 缺失合并");
        assert_eq!(libs[0].changer_paths.len(), 2);
    }

    #[test]
    fn parse_vpd83_zero_length_designator_kept_then_filtered_in_lu_id_pick() {
        // 解析器允许 dlen=0 designator 存在，但 read_vpd83_lu_id 上层会跳过空数据，
        // 避免空 Vec 作为 union key 把无关设备误并。
        let mut buf = vec![0, 0x83, 0, 4];
        buf.extend_from_slice(&[0x01, 0x03, 0x00, 0x00]); // assoc=00,type=NAA,len=0
        let designators = parse_vpd83_designators(&buf);
        assert_eq!(designators.len(), 1);
        assert_eq!(designators[0].data.len(), 0);
        // 上层 pick 逻辑会拒绝空 data；这里直接验证过滤条件。
        let pick = designators
            .iter()
            .find(|d| d.association == 0 && d.designator_type == 3 && !d.data.is_empty());
        assert!(pick.is_none(), "空 designator 不应被采纳为 lu_id");
    }
}
