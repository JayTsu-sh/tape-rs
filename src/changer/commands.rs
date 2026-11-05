//! 换带器 SCSI 命令实现

use bytes::BytesMut;
use log::{debug, info};

use crate::error::{Result, TapeError};
use crate::scsi::cdb;
use crate::scsi::transport::{TapeTransport, retry_unit_attention};

use super::element::{ElementAddressMap, ElementStatus, ElementType};

/// Medium Changer 高层封装
pub struct MediumChanger<'a> {
    device: &'a dyn TapeTransport,
    address_map: Option<ElementAddressMap>,
}

impl<'a> MediumChanger<'a> {
    pub fn new(device: &'a dyn TapeTransport) -> Self {
        Self {
            device,
            address_map: None,
        }
    }

    /// 获取 element 地址映射（MODE SENSE page 0x1D）
    pub fn load_address_map(&mut self) -> Result<&ElementAddressMap> {
        let cdb_bytes = cdb::mode_sense_10(0x1D, 255);
        let mut buf = [0u8; 255];
        let result = retry_unit_attention("changer", || {
            self.device.execute_read(&cdb_bytes, &mut buf, 30_000)
        })?;

        // 解析 Mode Parameter Header (10-byte 版本: 8 字节 header)
        // 然后是 Block Descriptor（如果有），然后是 Page 数据
        let header_len = 8;
        let bd_len = u16::from_be_bytes([buf[6], buf[7]]) as usize;
        let page_offset = header_len + bd_len;

        if page_offset + 20 > result.transferred {
            return Err(TapeError::InvalidResponse {
                expected: page_offset + 20,
                actual: result.transferred,
            });
        }

        let page = &buf[page_offset..];
        // Page 0x1D: Element Address Assignment
        // Byte 2-3: First Transport Element Address
        // Byte 4-5: Number of Transport Elements
        // Byte 6-7: First Storage Element Address
        // Byte 8-9: Number of Storage Elements
        // Byte 10-11: First I/E Element Address
        // Byte 12-13: Number of I/E Elements
        // Byte 14-15: First Data Transfer Element Address
        // Byte 16-17: Number of Data Transfer Elements

        let map = ElementAddressMap {
            transport_start: u16::from_be_bytes([page[2], page[3]]),
            transport_count: u16::from_be_bytes([page[4], page[5]]),
            storage_start: u16::from_be_bytes([page[6], page[7]]),
            storage_count: u16::from_be_bytes([page[8], page[9]]),
            ie_start: u16::from_be_bytes([page[10], page[11]]),
            ie_count: u16::from_be_bytes([page[12], page[13]]),
            dt_start: u16::from_be_bytes([page[14], page[15]]),
            dt_count: u16::from_be_bytes([page[16], page[17]]),
        };

        debug!("Element Address Map: {:?}", map);
        Ok(&*self.address_map.insert(map))
    }

    /// 获取已加载的地址映射
    pub fn address_map(&self) -> Option<&ElementAddressMap> {
        self.address_map.as_ref()
    }

    /// 读取所有 element 状态
    pub fn read_all_status(&self) -> Result<Vec<ElementStatus>> {
        let map = self.address_map.as_ref().ok_or_else(|| {
            TapeError::NotReady("address map not loaded, call load_address_map() first".into())
        })?;

        // 计算总 element 数量和起始地址。用 u32 累加防 u16 溢出
        // （理论上某些大型库房 >65535 元素时的防护）。
        let total_u32 = map.transport_count as u32
            + map.storage_count as u32
            + map.ie_count as u32
            + map.dt_count as u32;
        let total: u16 = u16::try_from(total_u32).map_err(|_| TapeError::InvalidResponse {
            expected: u16::MAX as usize,
            actual: total_u32 as usize,
        })?;
        let start = map
            .transport_start
            .min(map.storage_start)
            .min(map.ie_start)
            .min(map.dt_start);

        // 分配足够大的缓冲区: header(8) + 每种类型的 element page header(8) + 每个 element 的描述符
        // 描述符大小：PVolTag(12+36=48) + AVolTag(36) + DVCID 扩展(4+ID长度) → 余量到 192。
        let alloc_len: u32 = 8 + total_u32 * 192 + 64;
        let cdb_bytes = cdb::read_element_status(0, start, total, alloc_len, true, true);

        let mut buf = BytesMut::zeroed(alloc_len as usize);
        let result = retry_unit_attention("changer", || {
            self.device.execute_read(&cdb_bytes, &mut buf, 60_000)
        })?;

        let elements = parse_element_status_data(&buf[..result.transferred])?;
        // This call requests the whole map. A protocol-valid partial reply is
        // still insufficient to infer absence or choose a mechanical target.
        if elements.len() != total_u32 as usize {
            return Err(TapeError::Refused {
                reason: format!(
                    "READ ELEMENT STATUS 元素数 {} 与地址map中的 {} 不符",
                    elements.len(),
                    total_u32
                ),
            });
        }
        let mut addresses = std::collections::BTreeSet::new();
        for element in &elements {
            let (start, count) = match element.element_type {
                ElementType::Transport => (map.transport_start, map.transport_count),
                ElementType::Storage => (map.storage_start, map.storage_count),
                ElementType::ImportExport => (map.ie_start, map.ie_count),
                ElementType::DataTransfer => (map.dt_start, map.dt_count),
            };
            if !addresses.insert(element.address)
                || !(u32::from(start)..u32::from(start) + u32::from(count))
                    .contains(&u32::from(element.address))
            {
                return Err(TapeError::Refused {
                    reason: format!(
                        "READ ELEMENT STATUS 元素 {:#06x}/{:?} 重复或不属于当前地址map",
                        element.address, element.element_type
                    ),
                });
            }
        }
        Ok(elements)
    }

    /// 移动磁带
    pub fn move_medium(&self, source_addr: u16, dest_addr: u16) -> Result<()> {
        let map = self
            .address_map
            .as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;

        info!("移动介质: 从 {:#06x} 到 {:#06x}", source_addr, dest_addr);

        let cdb_bytes = cdb::move_medium(map.transport_start, source_addr, dest_addr);
        // MOVE MEDIUM 可能需要较长时间（机械操作）
        retry_unit_attention("changer", || {
            self.device.execute_no_data(&cdb_bytes, 300_000)
        })?;

        info!("移动完成");
        Ok(())
    }

    /// 初始化 element 状态（库存扫描）
    pub fn initialize_element_status(&self) -> Result<()> {
        info!("初始化 element 状态...");
        let cdb_bytes = cdb::initialize_element_status();
        // 初始化可能非常耗时
        retry_unit_attention("changer", || {
            self.device.execute_no_data(&cdb_bytes, 600_000)
        })?;
        info!("Element 状态初始化完成");
        Ok(())
    }

    /// EXCHANGE MEDIUM: 三方交换介质（A→B，B 原介质→C）
    pub fn exchange_medium(
        &self,
        source_addr: u16,
        dest1_addr: u16,
        dest2_addr: u16,
    ) -> Result<()> {
        let map = self
            .address_map
            .as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;

        info!(
            "交换介质: src={:#06x}, dest1={:#06x}, dest2={:#06x}",
            source_addr, dest1_addr, dest2_addr
        );
        let cdb_bytes = cdb::exchange_medium(
            map.transport_start,
            source_addr,
            dest1_addr,
            dest2_addr,
            false,
            false,
        );
        retry_unit_attention("changer", || {
            self.device.execute_no_data(&cdb_bytes, 600_000)
        })?;
        info!("交换完成");
        Ok(())
    }

    /// POSITION TO ELEMENT: 让机械臂定位到指定 element（不搬运介质）
    pub fn position_to_element(&self, dest_addr: u16) -> Result<()> {
        let map = self
            .address_map
            .as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;

        info!("机械臂定位到 {:#06x}", dest_addr);
        let cdb_bytes = cdb::position_to_element(map.transport_start, dest_addr, false);
        retry_unit_attention("changer", || {
            self.device.execute_no_data(&cdb_bytes, 120_000)
        })?;
        Ok(())
    }

    /// PREVENT/ALLOW MEDIUM REMOVAL: 锁/解锁 I/E 门
    pub fn prevent_medium_removal(&self, prevent: bool) -> Result<()> {
        info!("{} 介质移除", if prevent { "禁止" } else { "允许" });
        let cdb_bytes = cdb::prevent_allow_medium_removal(prevent);
        retry_unit_attention("changer", || {
            self.device.execute_no_data(&cdb_bytes, 10_000)
        })?;
        Ok(())
    }

    /// 导入：把 I/E 口中的介质搬到 storage slot
    pub fn import(&self, ie_offset: u16, storage_slot: u16) -> Result<()> {
        let map = self
            .address_map
            .as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;
        if ie_offset >= map.ie_count {
            return Err(TapeError::MoveFailed {
                reason: format!("I/E 偏移 {} 越界（可用 0..{}）", ie_offset, map.ie_count),
            });
        }
        if storage_slot == 0 || storage_slot > map.storage_count {
            return Err(TapeError::MoveFailed {
                reason: format!(
                    "storage slot {} 越界（可用 1..={}）",
                    storage_slot, map.storage_count
                ),
            });
        }
        let src = map.ie_start + ie_offset;
        let dst = map.storage_start + storage_slot - 1;
        info!(
            "导入: I/E {} ({:#06x}) → slot {} ({:#06x})",
            ie_offset, src, storage_slot, dst
        );
        self.move_medium(src, dst)
    }

    /// 导出：把 storage slot 介质搬到 I/E 口
    pub fn export(&self, storage_slot: u16, ie_offset: u16) -> Result<()> {
        let map = self
            .address_map
            .as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;
        if storage_slot == 0 || storage_slot > map.storage_count {
            return Err(TapeError::MoveFailed {
                reason: format!(
                    "storage slot {} 越界（可用 1..={}）",
                    storage_slot, map.storage_count
                ),
            });
        }
        if ie_offset >= map.ie_count {
            return Err(TapeError::MoveFailed {
                reason: format!("I/E 偏移 {} 越界（可用 0..{}）", ie_offset, map.ie_count),
            });
        }
        let src = map.storage_start + storage_slot - 1;
        let dst = map.ie_start + ie_offset;
        info!(
            "导出: slot {} ({:#06x}) → I/E {} ({:#06x})",
            storage_slot, src, ie_offset, dst
        );
        self.move_medium(src, dst)
    }
}

/// 解析 READ ELEMENT STATUS 返回数据。
///
/// 布局（SMC-3）：
///   [Data header 8B] [Page header 8B][desc × N] [Page header 8B][desc × N] ...
fn parse_element_status_data(data: &[u8]) -> Result<Vec<ElementStatus>> {
    if data.len() < 8 {
        return Err(TapeError::InvalidResponse {
            expected: 8,
            actual: data.len(),
        });
    }

    // Data header: byte 5-7 = Byte Count of Report Available（u24 BE）
    let byte_count = u32::from_be_bytes([0, data[5], data[6], data[7]]) as usize;
    let data_end = 8 + byte_count;
    if data_end > data.len() {
        return Err(TapeError::InvalidResponse {
            expected: data_end,
            actual: data.len(),
        });
    }

    let mut elements = Vec::new();
    let mut offset = 8;

    while offset < data_end {
        if data_end - offset < 8 {
            return Err(TapeError::InvalidResponse {
                expected: 8,
                actual: data_end - offset,
            });
        }
        let elem_type_code = data[offset];
        if ElementType::from_u8(elem_type_code).is_none() {
            return Err(TapeError::Refused {
                reason: format!("READ ELEMENT STATUS 页类型 {elem_type_code} 无效"),
            });
        }
        let voltag_flags = data[offset + 1];
        let desc_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let desc_byte_count =
            u32::from_be_bytes([0, data[offset + 5], data[offset + 6], data[offset + 7]]) as usize;
        offset += 8;

        if desc_len == 0 {
            if desc_byte_count == 0 {
                continue;
            }
            return Err(TapeError::Refused {
                reason: "READ ELEMENT STATUS 非空页的描述符长度为零".into(),
            });
        }
        let has_pvoltag = (voltag_flags & 0x80) != 0;
        let has_avoltag = (voltag_flags & 0x40) != 0;
        // Fixed fields occupy 12 bytes; each declared volume tag occupies 36.
        // A missing tag is not an empty tag when the page says it is present.
        let fixed_and_tags = 12 + 36 * (usize::from(has_pvoltag) + usize::from(has_avoltag));
        if desc_len < fixed_and_tags {
            return Err(TapeError::InvalidResponse {
                expected: fixed_and_tags,
                actual: desc_len,
            });
        }
        let page_end = offset + desc_byte_count;
        if page_end > data_end {
            return Err(TapeError::InvalidResponse {
                expected: page_end,
                actual: data_end,
            });
        }
        if !desc_byte_count.is_multiple_of(desc_len) {
            return Err(TapeError::Refused {
                reason: format!(
                    "READ ELEMENT STATUS 页长度 {desc_byte_count} 不能整除描述符长度 {desc_len}"
                ),
            });
        }

        while offset + desc_len <= page_end {
            let desc = &data[offset..offset + desc_len];
            if elem_type_code == ElementType::DataTransfer as u8 && desc_len > fixed_and_tags {
                let id_start = fixed_and_tags + 4;
                if desc_len < id_start {
                    return Err(TapeError::InvalidResponse {
                        expected: id_start,
                        actual: desc_len,
                    });
                }
                let id_end = id_start + usize::from(desc[fixed_and_tags + 3]);
                if id_end > desc_len {
                    return Err(TapeError::InvalidResponse {
                        expected: id_end,
                        actual: desc_len,
                    });
                }
            }
            elements.push(parse_descriptor(
                desc,
                elem_type_code,
                has_pvoltag,
                has_avoltag,
            ));
            offset += desc_len;
        }
    }

    let reported = u16::from_be_bytes([data[2], data[3]]) as usize;
    if elements.len() != reported {
        return Err(TapeError::Refused {
            reason: format!(
                "READ ELEMENT STATUS 描述符数 {} 与声明元素数 {} 不符",
                elements.len(),
                reported
            ),
        });
    }
    Ok(elements)
}

/// 调用方已校验固定字段、声明卷标及存在的 DVCID 扩展范围。
fn parse_descriptor(
    desc: &[u8],
    elem_type_code: u8,
    has_pvoltag: bool,
    has_avoltag: bool,
) -> ElementStatus {
    let address = u16::from_be_bytes([desc[0], desc[1]]);
    let full = (desc[2] & 0x01) != 0;

    let source_address = if desc.len() >= 12 && (desc[9] & 0x80) != 0 {
        Some(u16::from_be_bytes([desc[10], desc[11]]))
    } else {
        None
    };

    let volume_tag = if has_pvoltag {
        extract_volume_tag(desc)
    } else {
        None
    };
    let element_type = ElementType::from_u8(elem_type_code).unwrap_or(ElementType::Storage);

    // DVCID 扩展位于 PVolTag/AVolTag 之后。仅 DTE 元素带有意义的 device identifier。
    // PVolTag/AVolTag 各 36 字节（SMC-3 §6.10.1：32 字节 Volume Identification + 4 字节 Sequence Number）。
    let drive_id = if element_type == ElementType::DataTransfer {
        let mut id_off = 12;
        if has_pvoltag {
            id_off += 36;
        }
        if has_avoltag {
            id_off += 36;
        }
        extract_drive_id(desc, id_off)
    } else {
        None
    };

    ElementStatus {
        address,
        element_type,
        full,
        medium_type: desc.get(9).filter(|_| full).map(|b| b & 0x07),
        volume_tag,
        source_address,
        drive_id,
    }
}

/// DVCID 扩展（SMC-3）：
///   off+0: Code Set (低 4 bit)；2=ASCII, 3=UTF-8
///   off+1: Identifier Type
///   off+2: Reserved
///   off+3: Identifier Length (N)
///   off+4..off+4+N: Identifier
fn extract_drive_id(desc: &[u8], off: usize) -> Option<String> {
    if off + 4 > desc.len() {
        return None;
    }
    let id_len = desc[off + 3] as usize;
    let start = off + 4;
    let end = start + id_len;
    if end > desc.len() || id_len == 0 {
        return None;
    }
    let id = String::from_utf8_lossy(&desc[start..end])
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if id.is_empty() { None } else { Some(id) }
}

/// PVolTag 位于 descriptor byte 12..44，32 字节 ASCII。
/// 空槽的全部填 NUL 或 空格；两种都视为无 tag。
fn extract_volume_tag(desc: &[u8]) -> Option<String> {
    if desc.len() < 44 {
        return None;
    }
    let tag_bytes = &desc[12..44];
    if !tag_bytes.iter().any(|&b| b != 0 && b != b' ') {
        return None;
    }
    let tag = String::from_utf8_lossy(tag_bytes)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if tag.is_empty() { None } else { Some(tag) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_medium_type_ignores_source_valid_and_disabled_bits() {
        for ty in 1..=4 {
            let mut desc = [0u8; 48];
            desc[2] = 1;
            desc[9] = 0xca; // SValid、Invert、ED 和 medium type 2
            desc[10..12].copy_from_slice(&1007u16.to_be_bytes());
            let e = parse_descriptor(&desc, ty, false, false);
            assert_eq!(e.medium_type, Some(2));
            assert_eq!(e.source_address, Some(1007));
            assert!(e.is_cleaning()); // 无条码也可由设备识别
            desc[2] = 0;
            assert!(!parse_descriptor(&desc, ty, false, false).is_cleaning());
        }
        let e = parse_descriptor(&[0, 1, 1, 0], 2, false, false);
        assert_eq!(e.medium_type, None);
        assert!(!e.is_cleaning());
    }

    #[test]
    fn cleaning_barcode_fallback_does_not_classify_data_l1() {
        use crate::changer::element::cleaning_source;
        for barcode in ["CLNU03L1", "CLN001", "CLNI01L1"] {
            assert_eq!(cleaning_source(None, Some(barcode)), Some("barcode"));
        }
        for barcode in ["DATA01L1", "CLN", "CLNabcL1", "XCLNU03L1", "CLNU03L9"] {
            assert_eq!(cleaning_source(None, Some(barcode)), None);
        }
        assert_eq!(
            cleaning_source(Some(2), Some("CUSTOM")),
            Some("element_status")
        );
    }

    fn one_element_report() -> Vec<u8> {
        let mut bytes = vec![0u8; 28];
        bytes[0..2].copy_from_slice(&1001u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&1u16.to_be_bytes());
        bytes[7] = 20;
        bytes[8] = 2;
        bytes[11] = 12;
        bytes[15] = 12;
        bytes[16..18].copy_from_slice(&1001u16.to_be_bytes());
        bytes
    }
    #[test]
    fn complete_status_rejects_every_truncated_prefix_and_inconsistent_counts() {
        let bytes = one_element_report();
        assert_eq!(parse_element_status_data(&bytes).unwrap().len(), 1);
        for length in 0..bytes.len() {
            assert!(
                parse_element_status_data(&bytes[..length]).is_err(),
                "截断到{length}字节不能当完整状态"
            );
        }
        let mut wrong = bytes.clone();
        wrong[3] = 2;
        assert!(
            parse_element_status_data(&wrong).is_err(),
            "头部声称两个元素但只返回一个"
        );
        let mut partial = bytes.clone();
        partial.push(0);
        partial[7] += 1;
        partial[15] += 1;
        assert!(
            parse_element_status_data(&partial).is_err(),
            "页内残缺描述符不能忽略"
        );
        let mut tail = bytes.clone();
        tail.push(0);
        tail[7] += 1;
        assert!(
            parse_element_status_data(&tail).is_err(),
            "报告内残缺页头不能忽略"
        );
        let mut zero = bytes.clone();
        zero[11] = 0;
        assert!(
            parse_element_status_data(&zero).is_err(),
            "非空页不能使用零描述符长度"
        );
        let mut unknown = bytes.clone();
        unknown[8] = 5;
        assert!(
            parse_element_status_data(&unknown).is_err(),
            "未知页类型不能当作已知元素"
        );
        let mut empty_page = bytes[..8].to_vec();
        empty_page.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 0]);
        empty_page.extend_from_slice(&bytes[8..]);
        empty_page[7] += 8;
        assert_eq!(
            parse_element_status_data(&empty_page).unwrap().len(),
            1,
            "空页后仍须继续解析其他页"
        );
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 16]);
        assert_eq!(
            parse_element_status_data(&padded).unwrap().len(),
            1,
            "报告后填充不属于状态数据"
        );
    }
    fn descriptor_report(kind: u8, flags: u8, descriptor: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 16 + descriptor.len()];
        bytes[2..4].copy_from_slice(&1u16.to_be_bytes());
        bytes[5..8].copy_from_slice(&(8u32 + descriptor.len() as u32).to_be_bytes()[1..]);
        bytes[8] = kind;
        bytes[9] = flags;
        bytes[10..12].copy_from_slice(&(descriptor.len() as u16).to_be_bytes());
        bytes[13..16].copy_from_slice(&(descriptor.len() as u32).to_be_bytes()[1..]);
        bytes[16..].copy_from_slice(descriptor);
        bytes
    }
    #[test]
    fn complete_status_rejects_truncated_fixed_descriptor() {
        for length in [4, 11] {
            assert!(
                parse_element_status_data(&descriptor_report(2, 0, &vec![0; length])).is_err(),
                "固定字段不能截断到{length}"
            );
        }
    }
    #[test]
    fn complete_status_rejects_truncated_declared_volume_tags() {
        for (flags, minimum) in [(0x80, 48usize), (0x40, 48), (0xc0, 84)] {
            for length in [12, minimum - 4, minimum - 1] {
                assert!(
                    parse_element_status_data(&descriptor_report(2, flags, &vec![0; length]))
                        .is_err(),
                    "声明卷标{flags:#x}不能截断到{length}"
                );
            }
            assert_eq!(
                parse_element_status_data(&descriptor_report(2, flags, &vec![0; minimum]))
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    #[test]
    fn complete_status_rejects_truncated_device_identifier() {
        for length in 13..16 {
            assert!(
                parse_element_status_data(&descriptor_report(4, 0, &vec![0; length])).is_err(),
                "已出现的DVCID头不能缺字段"
            );
        }
        for (flags, base) in [(0, 12usize), (0x80, 48), (0xc0, 84)] {
            let mut descriptor = vec![0; base + 4];
            descriptor[base + 3] = 3;
            assert!(
                parse_element_status_data(&descriptor_report(4, flags, &descriptor)).is_err(),
                "DVCID声明正文不能越界"
            );
            descriptor.extend_from_slice(b"ABC");
            let rows =
                parse_element_status_data(&descriptor_report(4, flags, &descriptor)).unwrap();
            assert_eq!(rows[0].drive_id.as_deref(), Some("ABC"));
        }
    }
    struct OmitOneStorage(crate::scsi::sim::SimTransport);
    impl crate::scsi::transport::TapeTransport for OmitOneStorage {
        fn identity(&self) -> &str {
            self.0.identity()
        }
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.0.execute_no_data(cdb, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.0.execute_write(cdb, data, timeout)
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            let mut result = self.0.execute_read(cdb, data, timeout)?;
            if cdb[0] == crate::scsi::cdb::opcode::READ_ELEMENT_STATUS {
                let mut offset = 8;
                while offset + 8 <= result.transferred {
                    let size = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
                    let count = u32::from_be_bytes([
                        0,
                        data[offset + 5],
                        data[offset + 6],
                        data[offset + 7],
                    ]) as usize;
                    if data[offset] == 2 && count >= size && size > 0 {
                        let end = offset + 8 + count;
                        data.copy_within(end..result.transferred, end - size);
                        let remaining = (count - size) as u32;
                        data[offset + 5..offset + 8].copy_from_slice(&remaining.to_be_bytes()[1..]);
                        let elements = u16::from_be_bytes([data[2], data[3]]) - 1;
                        data[2..4].copy_from_slice(&elements.to_be_bytes());
                        let bytes =
                            u32::from_be_bytes([0, data[5], data[6], data[7]]) - size as u32;
                        data[5..8].copy_from_slice(&bytes.to_be_bytes()[1..]);
                        result.transferred -= size;
                        break;
                    }
                    offset += 8 + count;
                }
            }
            Ok(result)
        }
    }
    #[test]
    fn complete_status_requires_every_mapped_address_even_for_a_valid_short_report() {
        let library = crate::scsi::sim::SimLibrary::new(1, 3, 1);
        let device = library.changer();
        let mut changer = MediumChanger::new(&device);
        changer.load_address_map().unwrap();
        assert_eq!(changer.read_all_status().unwrap().len(), 6);
        let short = OmitOneStorage(library.changer());
        let mut changer = MediumChanger::new(&short);
        changer.load_address_map().unwrap();
        assert!(
            changer.read_all_status().is_err(),
            "协议长度自洽也不能缺少map中的一个槽位"
        );
    }
    #[test]
    fn short_descriptor_is_an_error_instead_of_a_panic() {
        for len in 1..4 {
            let mut data = vec![0u8; 16 + len];
            data[7] = (8 + len) as u8;
            data[8] = 2;
            data[11] = len as u8;
            data[15] = len as u8;
            assert!(parse_element_status_data(&data).is_err());
        }
    }
}
