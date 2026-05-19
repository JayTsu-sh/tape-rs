//! INQUIRY 子命令实现。

use tape_rs::error::Result;
use tape_rs::scsi::cdb;
use tape_rs::scsi::device::ScsiDevice;

pub fn cmd_inquiry(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let cdb_bytes = cdb::inquiry(96);
    let mut buf = [0u8; 96];
    dev.execute_read(&cdb_bytes, &mut buf, 10_000)?;

    println!("=== INQUIRY: {} ===", path);
    println!("  Device Type:   {:#04x}", buf[0] & 0x1F);
    println!("  Removable:     {}", if buf[1] & 0x80 != 0 { "Yes" } else { "No" });
    println!("  SCSI Version:  {:#04x}", buf[2]);

    let vendor = String::from_utf8_lossy(&buf[8..16]).trim().to_string();
    let product = String::from_utf8_lossy(&buf[16..32]).trim().to_string();
    let revision = String::from_utf8_lossy(&buf[32..36]).trim().to_string();

    println!("  Vendor:        {}", vendor);
    println!("  Product:       {}", product);
    println!("  Revision:      {}", revision);

    // Serial 读 VPD page 0x80（Unit Serial Number）——标准口径，
    // 不再从 standard INQUIRY byte 36+ 的 vendor-specific 区域猜。
    if let Some(serial) = read_unit_serial(&dev) {
        println!("  Serial:        {}", serial);
    }

    Ok(())
}

/// 读取 VPD page 0x80 (Unit Serial Number) 的序列号字段，失败/为空返回 None。
fn read_unit_serial(dev: &ScsiDevice) -> Option<String> {
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
    let serial = String::from_utf8_lossy(&buf[4..end])
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if serial.is_empty() { None } else { Some(serial) }
}
