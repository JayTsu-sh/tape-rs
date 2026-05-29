//! INQUIRY 共用 helper：跨模块复用的标准 INQUIRY + VPD 解析 + sg 节点枚举。

use std::path::PathBuf;

use crate::error::Result;
use crate::scsi::cdb;
use crate::scsi::device::ScsiDevice;

/// 标准 INQUIRY 返回的基础字段。
#[derive(Debug, Clone)]
pub struct InquiryStandard {
    pub peripheral_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

/// 跑标准 INQUIRY (96 字节)，解析 peripheral type + vendor/product/revision。
pub fn standard_inquiry(dev: &ScsiDevice) -> Result<InquiryStandard> {
    let cdb_bytes = cdb::inquiry(96);
    let mut buf = [0u8; 96];
    dev.execute_read(&cdb_bytes, &mut buf, 10_000)?;
    Ok(InquiryStandard {
        peripheral_type: buf[0] & 0x1F,
        vendor: String::from_utf8_lossy(&buf[8..16]).trim().to_string(),
        product: String::from_utf8_lossy(&buf[16..32]).trim().to_string(),
        revision: String::from_utf8_lossy(&buf[32..36]).trim().to_string(),
    })
}

/// 枚举 `/dev/sg*` 节点，按 sgN 的数字 N 升序排（避免 sg10 排在 sg2 前面的字典序问题）。
pub fn enumerate_sg_nodes() -> Result<Vec<PathBuf>> {
    let dir = std::fs::read_dir("/dev")?;
    let mut nodes: Vec<PathBuf> = dir
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| is_sg_node(p))
        .collect();
    nodes.sort_by_key(|p| sg_index(p).unwrap_or(u32::MAX));
    Ok(nodes)
}

fn is_sg_node(p: &std::path::Path) -> bool {
    p.file_name()
        .and_then(|s| s.to_str())
        .map(|n| n.starts_with("sg") && n.len() > 2 && n[2..].chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

fn sg_index(p: &std::path::Path) -> Option<u32> {
    p.file_name()
        .and_then(|s| s.to_str())
        .and_then(|n| n[2..].parse().ok())
}

/// 读 VPD page 0x80 (Unit Serial Number)，返回 trim 后的非空字符串；失败或为空返回 None。
pub fn read_unit_serial(dev: &ScsiDevice) -> Option<String> {
    let cdb_bytes = cdb::inquiry_vpd(0x80, 252);
    let mut buf = [0u8; 252];
    let result = dev.execute_read(&cdb_bytes, &mut buf, 10_000).ok()?;
    if result.transferred < 4 || buf[1] != 0x80 {
        return None;
    }
    let page_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (4 + page_len).min(result.transferred);
    if end <= 4 {
        return None;
    }
    let s = String::from_utf8_lossy(&buf[4..end])
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if s.is_empty() { None } else { Some(s) }
}
