//! 换带器 SCSI 命令实现

use bytes::BytesMut;
use log::{debug, info};

use crate::error::{TapeError, Result};
use crate::scsi::cdb;
use crate::scsi::device::ScsiDevice;

use super::element::{ElementAddressMap, ElementStatus, ElementType};

/// Medium Changer 高层封装
pub struct MediumChanger<'a> {
    device: &'a ScsiDevice,
    address_map: Option<ElementAddressMap>,
}

impl<'a> MediumChanger<'a> {
    pub fn new(device: &'a ScsiDevice) -> Self {
        Self { device, address_map: None }
    }

    /// 获取 element 地址映射（MODE SENSE page 0x1D）
    pub fn load_address_map(&mut self) -> Result<&ElementAddressMap> {
        let cdb_bytes = cdb::mode_sense_10(0x1D, 255);
        let mut buf = [0u8; 255];
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 30_000)?;

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
        let map = self.address_map.as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded, call load_address_map() first".into()))?;

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
        let start = map.transport_start
            .min(map.storage_start)
            .min(map.ie_start)
            .min(map.dt_start);

        // 分配足够大的缓冲区: header(8) + 每种类型的 element page header(8) + 每个 element 的描述符
        // 每个 element 描述符: 12 bytes (无 voltag) 或 48 bytes (有 voltag)
        let alloc_len: u32 = 8 + total_u32 * 72 + 64;
        let cdb_bytes = cdb::read_element_status(0, start, total, alloc_len, true);

        let mut buf = BytesMut::zeroed(alloc_len as usize);
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 60_000)?;

        // 解析返回数据（parser 只需 &[u8]，通过 BytesMut 的 Deref 传入）
        parse_element_status_data(&buf[..result.transferred])
    }

    /// 移动磁带
    pub fn move_medium(&self, source_addr: u16, dest_addr: u16) -> Result<()> {
        let map = self.address_map.as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;

        info!("移动介质: 从 {:#06x} 到 {:#06x}", source_addr, dest_addr);

        let cdb_bytes = cdb::move_medium(map.transport_start, source_addr, dest_addr);
        // MOVE MEDIUM 可能需要较长时间（机械操作）
        self.device.execute_no_data(&cdb_bytes, 300_000)?;

        info!("移动完成");
        Ok(())
    }

    /// 初始化 element 状态（库存扫描）
    pub fn initialize_element_status(&self) -> Result<()> {
        info!("初始化 element 状态...");
        let cdb_bytes = cdb::initialize_element_status();
        // 初始化可能非常耗时
        self.device.execute_no_data(&cdb_bytes, 600_000)?;
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
        let map = self.address_map.as_ref()
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
        self.device.execute_no_data(&cdb_bytes, 600_000)?;
        info!("交换完成");
        Ok(())
    }

    /// POSITION TO ELEMENT: 让机械臂定位到指定 element（不搬运介质）
    pub fn position_to_element(&self, dest_addr: u16) -> Result<()> {
        let map = self.address_map.as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;

        info!("机械臂定位到 {:#06x}", dest_addr);
        let cdb_bytes = cdb::position_to_element(map.transport_start, dest_addr, false);
        self.device.execute_no_data(&cdb_bytes, 120_000)?;
        Ok(())
    }

    /// PREVENT/ALLOW MEDIUM REMOVAL: 锁/解锁 I/E 门
    pub fn prevent_medium_removal(&self, prevent: bool) -> Result<()> {
        info!("{} 介质移除", if prevent { "禁止" } else { "允许" });
        let cdb_bytes = cdb::prevent_allow_medium_removal(prevent);
        self.device.execute_no_data(&cdb_bytes, 10_000)?;
        Ok(())
    }

    /// 导入：把 I/E 口中的介质搬到 storage slot
    pub fn import(&self, ie_offset: u16, storage_slot: u16) -> Result<()> {
        let map = self.address_map.as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;
        if ie_offset >= map.ie_count {
            return Err(TapeError::MoveFailed {
                reason: format!("I/E 偏移 {} 越界（可用 0..{}）", ie_offset, map.ie_count),
            });
        }
        if storage_slot == 0 || storage_slot > map.storage_count {
            return Err(TapeError::MoveFailed {
                reason: format!("storage slot {} 越界（可用 1..={}）", storage_slot, map.storage_count),
            });
        }
        let src = map.ie_start + ie_offset;
        let dst = map.storage_start + storage_slot - 1;
        info!("导入: I/E {} ({:#06x}) → slot {} ({:#06x})", ie_offset, src, storage_slot, dst);
        self.move_medium(src, dst)
    }

    /// 导出：把 storage slot 介质搬到 I/E 口
    pub fn export(&self, storage_slot: u16, ie_offset: u16) -> Result<()> {
        let map = self.address_map.as_ref()
            .ok_or_else(|| TapeError::NotReady("address map not loaded".into()))?;
        if storage_slot == 0 || storage_slot > map.storage_count {
            return Err(TapeError::MoveFailed {
                reason: format!("storage slot {} 越界（可用 1..={}）", storage_slot, map.storage_count),
            });
        }
        if ie_offset >= map.ie_count {
            return Err(TapeError::MoveFailed {
                reason: format!("I/E 偏移 {} 越界（可用 0..{}）", ie_offset, map.ie_count),
            });
        }
        let src = map.storage_start + storage_slot - 1;
        let dst = map.ie_start + ie_offset;
        info!("导出: slot {} ({:#06x}) → I/E {} ({:#06x})", storage_slot, src, ie_offset, dst);
        self.move_medium(src, dst)
    }

}

/// 解析 READ ELEMENT STATUS 返回数据。
///
/// 布局（SMC-3）：
///   [Data header 8B] [Page header 8B][desc × N] [Page header 8B][desc × N] ...
fn parse_element_status_data(data: &[u8]) -> Result<Vec<ElementStatus>> {
    if data.len() < 8 {
        return Err(TapeError::InvalidResponse { expected: 8, actual: data.len() });
    }

    // Data header: byte 5-7 = Byte Count of Report Available（u24 BE）
    let byte_count = u32::from_be_bytes([0, data[5], data[6], data[7]]) as usize;
    let data_end = (8 + byte_count).min(data.len());

    let mut elements = Vec::new();
    let mut offset = 8;

    while offset + 8 <= data_end {
        let elem_type_code = data[offset];
        let voltag_flags = data[offset + 1];
        let desc_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let desc_byte_count =
            u32::from_be_bytes([0, data[offset + 5], data[offset + 6], data[offset + 7]]) as usize;
        offset += 8;

        if desc_len == 0 {
            break;
        }

        let has_pvoltag = (voltag_flags & 0x80) != 0;
        let page_end = (offset + desc_byte_count).min(data_end);

        while offset + desc_len <= page_end {
            let desc = &data[offset..offset + desc_len];
            elements.push(parse_descriptor(desc, elem_type_code, has_pvoltag));
            offset += desc_len;
        }
    }

    Ok(elements)
}

/// 解析单个 Element Descriptor。descriptor 最低 4 字节已由调用方保证存在。
fn parse_descriptor(desc: &[u8], elem_type_code: u8, has_pvoltag: bool) -> ElementStatus {
    let address = u16::from_be_bytes([desc[0], desc[1]]);
    let full = (desc[2] & 0x01) != 0;

    let source_address = if desc.len() >= 12 && (desc[9] & 0x80) != 0 {
        Some(u16::from_be_bytes([desc[10], desc[11]]))
    } else {
        None
    };

    let volume_tag = if has_pvoltag { extract_volume_tag(desc) } else { None };
    let element_type = ElementType::from_u8(elem_type_code).unwrap_or(ElementType::Storage);

    ElementStatus { address, element_type, full, volume_tag, source_address }
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
