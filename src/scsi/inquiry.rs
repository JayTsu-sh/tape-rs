//! INQUIRY 共用 helper：跨模块复用的标准 INQUIRY + VPD 解析 + sg 节点枚举。

use std::path::PathBuf;

use crate::error::Result;
use crate::scsi::cdb;
use crate::scsi::transport::TapeTransport;

/// 规范化 FC 目标端口 WWPN；它与设备序列号一起标识换带器的预留域。
pub fn normalize_fc_target_port(value: &str) -> Result<String> {
    let hex = value.strip_prefix("0x").unwrap_or(value);
    if hex.len() != 16
        || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        || hex.bytes().all(|b| b == b'0')
    {
        return Err(crate::error::TapeError::Ltfs(
            "changer_target_port 必须是非零的 16 位十六进制 FC WWPN".into(),
        ));
    }
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

/// 从 Linux FC sysfs 读取 sg 节点实际连接的远端端口，不依赖 sgN/HCTL 顺序。
pub fn fc_target_port(path: &std::path::Path) -> Result<Option<String>> {
    fc_target_port_in(std::path::Path::new("/sys"), path)
}

fn fc_target_port_in(root: &std::path::Path, path: &std::path::Path) -> Result<Option<String>> {
    if !is_sg_node(path) {
        return Err(crate::error::TapeError::Ltfs("无效 sg 设备路径".into()));
    }
    let device = root
        .join("class/scsi_generic")
        .join(path.file_name().unwrap())
        .join("device");
    let device = std::fs::canonicalize(device)?;
    let Some(remote) = device
        .ancestors()
        .filter_map(|p| p.file_name())
        .find(|name| name.to_str().is_some_and(|name| name.starts_with("rport-")))
    else {
        return Ok(None);
    };
    let value = std::fs::read_to_string(
        root.join("class/fc_remote_ports")
            .join(remote)
            .join("port_name"),
    )?;
    normalize_fc_target_port(value.trim()).map(Some)
}

/// 标准 INQUIRY 返回的基础字段。
#[derive(Debug, Clone)]
pub struct InquiryStandard {
    pub peripheral_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

/// 跑标准 INQUIRY (96 字节)，解析 peripheral type + vendor/product/revision。
pub fn standard_inquiry(dev: &dyn TapeTransport) -> Result<InquiryStandard> {
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
pub fn read_unit_serial(dev: &dyn TapeTransport) -> Option<String> {
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

#[cfg(test)]
mod fc_port_tests {
    use super::*;

    #[test]
    fn fc_wwpn_requires_a_nonzero_exact_identifier() {
        assert_eq!(
            normalize_fc_target_port("5000E1117018605D").unwrap(),
            "0x5000e1117018605d"
        );
        assert_eq!(
            normalize_fc_target_port("0x5000e11170186070").unwrap(),
            "0x5000e11170186070"
        );
        for value in [
            "",
            "0000000000000000",
            "0x0",
            "5000e1117018607g",
            " 5000e11170186070",
            "5000e111701860700",
            "../../5000e11170",
        ] {
            assert!(normalize_fc_target_port(value).is_err(), "{value}");
        }
    }

    #[test]
    fn fc_sysfs_identity_is_independent_of_local_sg_order() {
        let root = std::env::temp_dir().join(format!("fc-sysfs-{}", uuid::Uuid::new_v4()));
        for (sg, remote, port) in [
            ("sg5", "rport-15:0-10", "0x5000e1117018605d"),
            ("sg7", "rport-15:0-11", "0x5000e11170186070"),
        ] {
            let device = root
                .join("devices/host15")
                .join(remote)
                .join("target15:0:1/15:0:1:1");
            std::fs::create_dir_all(&device).unwrap();
            let link = root.join("class/scsi_generic").join(sg);
            std::fs::create_dir_all(&link).unwrap();
            std::os::unix::fs::symlink(&device, link.join("device")).unwrap();
            let remote = root.join("class/fc_remote_ports").join(remote);
            std::fs::create_dir_all(&remote).unwrap();
            std::fs::write(remote.join("port_name"), format!("{port}\n")).unwrap();
            assert_eq!(
                fc_target_port_in(&root, std::path::Path::new(sg))
                    .unwrap()
                    .as_deref(),
                Some(port)
            );
        }
        let a = root.join("class/scsi_generic/sg5/device");
        let b = root.join("class/scsi_generic/sg7/device");
        let target_a = std::fs::read_link(&a).unwrap();
        let target_b = std::fs::read_link(&b).unwrap();
        std::fs::remove_file(&a).unwrap();
        std::fs::remove_file(&b).unwrap();
        std::os::unix::fs::symlink(target_b, &a).unwrap();
        std::os::unix::fs::symlink(target_a, &b).unwrap();
        assert_eq!(
            fc_target_port_in(&root, std::path::Path::new("sg5"))
                .unwrap()
                .as_deref(),
            Some("0x5000e11170186070")
        );
        assert_eq!(
            fc_target_port_in(&root, std::path::Path::new("sg7"))
                .unwrap()
                .as_deref(),
            Some("0x5000e1117018605d")
        );
        std::fs::remove_file(root.join("class/fc_remote_ports/rport-15:0-10/port_name")).unwrap();
        assert!(fc_target_port_in(&root, std::path::Path::new("sg7")).is_err());
        assert!(fc_target_port_in(&root, std::path::Path::new("sg0")).is_err());
        assert!(fc_target_port_in(&root, std::path::Path::new("../bad")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
