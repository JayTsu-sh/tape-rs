//! SCSI CDB（Command Descriptor Block）构建器

/// SCSI 命令 opcode 常量
pub mod opcode {
    // 通用命令
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SENSE_6: u8 = 0x1A;
    pub const MODE_SENSE_10: u8 = 0x5A;
    pub const MODE_SELECT_6: u8 = 0x15;
    pub const MODE_SELECT_10: u8 = 0x55;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const SEND_DIAGNOSTIC: u8 = 0x1D;
    pub const RECEIVE_DIAGNOSTIC_RESULTS: u8 = 0x1C;
    pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
    pub const LOG_SENSE: u8 = 0x4D;

    // 磁带机命令
    pub const REWIND: u8 = 0x01;
    pub const FORMAT_MEDIUM: u8 = 0x04;
    pub const READ_6: u8 = 0x08;
    pub const WRITE_6: u8 = 0x0A;
    pub const WRITE_FILEMARKS_6: u8 = 0x10;
    pub const SPACE_6: u8 = 0x11;
    pub const ERASE_6: u8 = 0x19;
    pub const LOAD_UNLOAD: u8 = 0x1B;
    pub const READ_POSITION: u8 = 0x34;
    pub const REPORT_DENSITY_SUPPORT: u8 = 0x44;
    pub const LOCATE_16: u8 = 0x92;
    pub const READ_ATTRIBUTE: u8 = 0x8C;
    pub const WRITE_ATTRIBUTE: u8 = 0x8D;

    // 换带器命令
    pub const INITIALIZE_ELEMENT_STATUS: u8 = 0x07;
    pub const POSITION_TO_ELEMENT: u8 = 0x2B;
    pub const MOVE_MEDIUM: u8 = 0xA5;
    pub const EXCHANGE_MEDIUM: u8 = 0xA6;
    pub const READ_ELEMENT_STATUS: u8 = 0xB8;
}

/// 构建 INQUIRY CDB (6 bytes)
pub fn inquiry(alloc_len: u16) -> [u8; 6] {
    [
        opcode::INQUIRY,
        0x00,
        0x00,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 INQUIRY VPD CDB (6 bytes): EVPD=1
/// page_code 常用值：0x80 Unit Serial Number, 0x83 Device Identification
pub fn inquiry_vpd(page_code: u8, alloc_len: u16) -> [u8; 6] {
    [
        opcode::INQUIRY,
        0x01, // EVPD = 1
        page_code,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 TEST UNIT READY CDB (6 bytes)
pub fn test_unit_ready() -> [u8; 6] {
    [opcode::TEST_UNIT_READY, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// 构建 MODE SENSE(10) CDB (10 bytes)
/// page_code: 要查询的 page (如 0x1D = Element Address Assignment)
pub fn mode_sense_10(page_code: u8, alloc_len: u16) -> [u8; 10] {
    [
        opcode::MODE_SENSE_10,
        0x00,                          // DBD=0
        page_code & 0x3F,              // page code, PC=0 (current values)
        0x00,                          // subpage
        0x00, 0x00, 0x00,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 READ ELEMENT STATUS CDB (12 bytes)
/// element_type: 0=all, 1=transport, 2=storage, 3=I/E, 4=data transfer
/// start_addr: 起始 element 地址
/// count: 要查询的 element 数量
/// alloc_len: 分配的缓冲区长度
/// voltag: 是否返回 volume tag
pub fn read_element_status(
    element_type: u8,
    start_addr: u16,
    count: u16,
    alloc_len: u32,
    voltag: bool,
) -> [u8; 12] {
    let byte1 = (element_type & 0x0F) | if voltag { 0x10 } else { 0x00 };
    [
        opcode::READ_ELEMENT_STATUS,
        byte1,
        (start_addr >> 8) as u8,
        (start_addr & 0xFF) as u8,
        (count >> 8) as u8,
        (count & 0xFF) as u8,
        0x00, // CURDATA=0, DVCID=0
        (alloc_len >> 16) as u8,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
        0x00,
    ]
}

/// 构建 MOVE MEDIUM CDB (12 bytes)
/// transport_addr: 机械臂地址（通常 0x0000）
/// source_addr: 源 element 地址
/// dest_addr: 目标 element 地址
pub fn move_medium(transport_addr: u16, source_addr: u16, dest_addr: u16) -> [u8; 12] {
    [
        opcode::MOVE_MEDIUM,
        0x00,
        (transport_addr >> 8) as u8,
        (transport_addr & 0xFF) as u8,
        (source_addr >> 8) as u8,
        (source_addr & 0xFF) as u8,
        (dest_addr >> 8) as u8,
        (dest_addr & 0xFF) as u8,
        0x00,
        0x00,
        0x00,
        0x00,
    ]
}

/// 构建 INITIALIZE ELEMENT STATUS CDB (6 bytes)
pub fn initialize_element_status() -> [u8; 6] {
    [opcode::INITIALIZE_ELEMENT_STATUS, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// 构建 REWIND CDB (6 bytes)
pub fn rewind() -> [u8; 6] {
    [opcode::REWIND, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// 构建 READ(6) CDB
/// fixed: true 表示固定块模式，transfer_len 为块数量
/// fixed: false 表示可变块模式，transfer_len 为字节数
pub fn read_6(fixed: bool, transfer_len: u32) -> [u8; 6] {
    let byte1 = if fixed { 0x01 } else { 0x00 };
    [
        opcode::READ_6,
        byte1,
        ((transfer_len >> 16) & 0xFF) as u8,
        ((transfer_len >> 8) & 0xFF) as u8,
        (transfer_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 WRITE(6) CDB
pub fn write_6(fixed: bool, transfer_len: u32) -> [u8; 6] {
    let byte1 = if fixed { 0x01 } else { 0x00 };
    [
        opcode::WRITE_6,
        byte1,
        ((transfer_len >> 16) & 0xFF) as u8,
        ((transfer_len >> 8) & 0xFF) as u8,
        (transfer_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 WRITE FILEMARKS(6) CDB
pub fn write_filemarks(count: u32) -> [u8; 6] {
    [
        opcode::WRITE_FILEMARKS_6,
        0x00,
        ((count >> 16) & 0xFF) as u8,
        ((count >> 8) & 0xFF) as u8,
        (count & 0xFF) as u8,
        0x00,
    ]
}

/// SPACE(6) 的 24-bit 有符号 count 范围。
pub const SPACE_COUNT_MIN: i32 = -(1 << 23);
pub const SPACE_COUNT_MAX: i32 = (1 << 23) - 1;

/// 构建 SPACE(6) CDB
/// code: 0x00=blocks, 0x01=filemarks, 0x03=end-of-data
///
/// CDB 只携带 24-bit 有符号 count。调用方应先用
/// `SPACE_COUNT_MIN..=SPACE_COUNT_MAX` 校验；debug 构建下越界会 panic，
/// release 下会按 `i32` 高字节静默丢弃。
pub fn space(code: u8, count: i32) -> [u8; 6] {
    debug_assert!(
        (SPACE_COUNT_MIN..=SPACE_COUNT_MAX).contains(&count),
        "SPACE count {} 超出 24-bit 有符号范围",
        count
    );
    let count_bytes = count.to_be_bytes();
    [
        opcode::SPACE_6,
        code & 0x07,
        count_bytes[1],
        count_bytes[2],
        count_bytes[3],
        0x00,
    ]
}

/// 构建 LOAD/UNLOAD CDB (6 bytes)
/// load: true=装载, false=弹出
pub fn load_unload(load: bool) -> [u8; 6] {
    let byte4 = if load { 0x01 } else { 0x00 };
    [opcode::LOAD_UNLOAD, 0x00, 0x00, 0x00, byte4, 0x00]
}

/// 构建 READ POSITION CDB (10 bytes)
pub fn read_position() -> [u8; 10] {
    [opcode::READ_POSITION, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// 构建 EXCHANGE MEDIUM CDB (12 bytes)
/// transport_addr: 机械臂地址
/// source_addr: 源 element 地址
/// dest1_addr: 第一目标 element 地址（介质将放到此处）
/// dest2_addr: 第二目标 element 地址（source 处的介质最终位置）
/// inv1/inv2: 是否反转介质
pub fn exchange_medium(
    transport_addr: u16,
    source_addr: u16,
    dest1_addr: u16,
    dest2_addr: u16,
    inv1: bool,
    inv2: bool,
) -> [u8; 12] {
    let flags = (if inv1 { 0x01 } else { 0 }) | (if inv2 { 0x02 } else { 0 });
    [
        opcode::EXCHANGE_MEDIUM,
        0x00,
        (transport_addr >> 8) as u8,
        (transport_addr & 0xFF) as u8,
        (source_addr >> 8) as u8,
        (source_addr & 0xFF) as u8,
        (dest1_addr >> 8) as u8,
        (dest1_addr & 0xFF) as u8,
        (dest2_addr >> 8) as u8,
        (dest2_addr & 0xFF) as u8,
        flags,
        0x00,
    ]
}

/// 构建 POSITION TO ELEMENT CDB (10 bytes)
/// 让机械臂定位到指定 element，但不搬运介质
pub fn position_to_element(transport_addr: u16, dest_addr: u16, invert: bool) -> [u8; 10] {
    [
        opcode::POSITION_TO_ELEMENT,
        0x00,
        (transport_addr >> 8) as u8,
        (transport_addr & 0xFF) as u8,
        (dest_addr >> 8) as u8,
        (dest_addr & 0xFF) as u8,
        0x00,
        0x00,
        if invert { 0x01 } else { 0x00 },
        0x00,
    ]
}

/// 构建 PREVENT/ALLOW MEDIUM REMOVAL CDB (6 bytes)
/// prevent: true=锁定介质, false=允许弹出
pub fn prevent_allow_medium_removal(prevent: bool) -> [u8; 6] {
    [
        opcode::PREVENT_ALLOW_MEDIUM_REMOVAL,
        0x00,
        0x00,
        0x00,
        if prevent { 0x01 } else { 0x00 },
        0x00,
    ]
}

/// 构建 LOCATE(16) CDB
/// partition: 目标 partition
/// logical_id: 目标 block / logical object id
/// change_partition: 是否切换 partition
/// immed: 立即返回（后台完成）
pub fn locate_16(partition: u8, logical_id: u64, change_partition: bool, immed: bool) -> [u8; 16] {
    let byte1 = (if change_partition { 0x02 } else { 0x00 })
        | (if immed { 0x01 } else { 0x00 });
    let id = logical_id.to_be_bytes();
    [
        opcode::LOCATE_16,
        byte1,
        0x00,
        partition,
        id[0], id[1], id[2], id[3], id[4], id[5], id[6], id[7],
        0x00, 0x00, 0x00, 0x00,
    ]
}

/// 构建 ERASE(6) CDB
/// long_erase: true=整带 erase（耗时），false=短 erase
/// immed: 立即返回
pub fn erase_6(long_erase: bool, immed: bool) -> [u8; 6] {
    let byte1 = (if long_erase { 0x01 } else { 0x00 })
        | (if immed { 0x02 } else { 0x00 });
    [opcode::ERASE_6, byte1, 0x00, 0x00, 0x00, 0x00]
}

/// 构建 FORMAT MEDIUM CDB (6 bytes)
/// format: 0=默认（单 partition），1=按 partition 参数格式化，2=保留
/// immed: 立即返回
/// verify: 格式化后校验
pub fn format_medium(format: u8, immed: bool, verify: bool) -> [u8; 6] {
    let byte1 = (if immed { 0x01 } else { 0x00 })
        | (if verify { 0x02 } else { 0x00 });
    [opcode::FORMAT_MEDIUM, byte1, format & 0x0F, 0x00, 0x00, 0x00]
}

/// 构建 LOG SENSE CDB (10 bytes)
/// page_code: log page 编号（如 0x2E = TapeAlert）
/// subpage: 子页（通常 0）
/// alloc_len: 返回缓冲区长度
pub fn log_sense(page_code: u8, subpage: u8, alloc_len: u16) -> [u8; 10] {
    // PC=01b (current cumulative values)
    let byte2 = 0x40 | (page_code & 0x3F);
    [
        opcode::LOG_SENSE,
        0x00, // SP=0, PPC=0
        byte2,
        subpage,
        0x00,
        0x00, 0x00, // parameter pointer
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 MODE SELECT(10) CDB
/// pf: Page Format（新格式必须为 true）
/// sp: Save Pages
/// param_len: 参数列表长度
pub fn mode_select_10(pf: bool, sp: bool, param_len: u16) -> [u8; 10] {
    let byte1 = (if pf { 0x10 } else { 0x00 }) | (if sp { 0x01 } else { 0x00 });
    [
        opcode::MODE_SELECT_10,
        byte1,
        0x00, 0x00, 0x00, 0x00, 0x00,
        (param_len >> 8) as u8,
        (param_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 REPORT DENSITY SUPPORT CDB (10 bytes)
/// media: true=仅返回当前介质支持的 density，false=返回所有 density
/// medium_type: SSC-4+ 扩展，通常 false
pub fn report_density_support(media: bool, medium_type: bool, alloc_len: u16) -> [u8; 10] {
    let byte1 = (if media { 0x01 } else { 0x00 }) | (if medium_type { 0x02 } else { 0x00 });
    [
        opcode::REPORT_DENSITY_SUPPORT,
        byte1,
        0x00, 0x00, 0x00, 0x00, 0x00,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 SEND DIAGNOSTIC CDB (6 bytes)
/// self_test_code: 000=default, 001=background short, 010=background extended,
///                 100=abort, 101=foreground short, 110=foreground extended
/// pf: Page Format
/// self_test: 触发默认自检
/// param_len: 参数列表长度
pub fn send_diagnostic(
    self_test_code: u8,
    pf: bool,
    self_test: bool,
    param_len: u16,
) -> [u8; 6] {
    let byte1 = ((self_test_code & 0x07) << 5)
        | (if pf { 0x10 } else { 0x00 })
        | (if self_test { 0x04 } else { 0x00 });
    [
        opcode::SEND_DIAGNOSTIC,
        byte1,
        0x00,
        (param_len >> 8) as u8,
        (param_len & 0xFF) as u8,
        0x00,
    ]
}

/// 构建 READ ATTRIBUTE (0x8C) CDB (16 bytes)
/// service_action: 0x00 = VALUES（返回 attribute TLV 列表）
/// partition: MAM 所在 partition（VCI 等卷级属性通常都在 partition 0）
/// first_attr: 起始 attribute id
/// alloc_len: 返回缓冲区长度
pub fn read_attribute(service_action: u8, partition: u8, first_attr: u16, alloc_len: u32) -> [u8; 16] {
    let id = first_attr.to_be_bytes();
    let len = alloc_len.to_be_bytes();
    // SPC-5 Table 236：partition 位于 byte 7，byte 6 是 Logical Volume Number / reserved。
    // 旧代码曾把 partition 误放在 byte 6，于是对 partition=1 的请求被 drive 解释成非法
    // volume number + partition=0，表现为 CHECK CONDITION ILLEGAL REQUEST (0x05/0x24/0x00)。
    [
        opcode::READ_ATTRIBUTE,          // 0
        service_action & 0x1F,            // 1: service action
        0x00, 0x00,                       // 2-3: element address
        0x00,                             // 4: volume number
        0x00,                             // 5: reserved
        0x00,                             // 6: reserved
        partition,                        // 7: partition number
        id[0], id[1],                     // 8-9: first attribute id
        len[0], len[1], len[2], len[3],   // 10-13: allocation length
        0x00,                             // 14: cache flag
        0x00,                             // 15: control
    ]
}

/// 构建 WRITE ATTRIBUTE (0x8D) CDB (16 bytes)
/// wtc: Write Through Cache（通常为 true，立即写回介质）
/// partition: MAM 所在 partition
/// param_len: 参数列表长度（含 4 字节 total length header + attribute 条目）
pub fn write_attribute(wtc: bool, partition: u8, param_len: u32) -> [u8; 16] {
    let len = param_len.to_be_bytes();
    // 同 READ ATTRIBUTE：partition 在 byte 7，不是 byte 6。
    [
        opcode::WRITE_ATTRIBUTE,         // 0
        if wtc { 0x01 } else { 0x00 },   // 1: WTC bit
        0x00, 0x00,                      // 2-3: element address
        0x00,                            // 4: volume number
        0x00,                            // 5: reserved
        0x00,                            // 6: reserved
        partition,                       // 7: partition number
        0x00, 0x00,                      // 8-9: reserved
        len[0], len[1], len[2], len[3],  // 10-13: parameter list length
        0x00,                            // 14: reserved
        0x00,                            // 15: control
    ]
}

/// 构建 RECEIVE DIAGNOSTIC RESULTS CDB (6 bytes)
/// pcv: Page Code Valid
/// page_code: 当 pcv=true 时生效
/// alloc_len: 返回缓冲区长度
pub fn receive_diagnostic_results(pcv: bool, page_code: u8, alloc_len: u16) -> [u8; 6] {
    [
        opcode::RECEIVE_DIAGNOSTIC_RESULTS,
        if pcv { 0x01 } else { 0x00 },
        page_code,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xFF) as u8,
        0x00,
    ]
}
