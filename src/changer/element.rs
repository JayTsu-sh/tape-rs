//! 换带器 Element 类型定义

/// Element 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    /// 机械臂（Transport）
    Transport = 1,
    /// 存储槽位（Storage Slot）
    Storage = 2,
    /// 邮槽（Import/Export）
    ImportExport = 3,
    /// 数据传输（Tape Drive）
    DataTransfer = 4,
}

impl ElementType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Transport),
            2 => Some(Self::Storage),
            3 => Some(Self::ImportExport),
            4 => Some(Self::DataTransfer),
            _ => None,
        }
    }

}

/// Element 地址映射（通过 MODE SENSE page 0x1D 获取）
#[derive(Debug, Clone)]
pub struct ElementAddressMap {
    pub transport_start: u16,
    pub transport_count: u16,
    pub storage_start: u16,
    pub storage_count: u16,
    pub ie_start: u16,
    pub ie_count: u16,
    pub dt_start: u16,
    pub dt_count: u16,
}

/// 单个 Element 的状态
#[derive(Debug, Clone)]
pub struct ElementStatus {
    /// Element 地址
    pub address: u16,
    /// Element 类型
    pub element_type: ElementType,
    /// 是否有介质
    pub full: bool,
    /// Volume Tag（磁带条码）
    pub volume_tag: Option<String>,
    /// 源 element 地址（如果介质被移入）
    pub source_address: Option<u16>,
    /// DTE 元素返回的 device identifier（DVCID 位启用时），通常是驱动器序列号。
    pub drive_id: Option<String>,
}

