//! SG_IO ioctl 结构体定义和常量

/// 数据传输方向（对齐 Linux `<scsi/sg.h>`：
/// `SG_DXFER_NONE = -1`、`SG_DXFER_TO_DEV = -2`、`SG_DXFER_FROM_DEV = -3`）。
/// 用错值（如 0/1）时读路径会被内核从 CDB 反推方向侥幸成功，
/// 写路径会走到 FCP 层被判成 "INVALID FIELD IN COMMAND INFORMATION UNIT"
/// (sense_key=0x05, asc=0x0E, ascq=0x03)。
pub const SG_DXFER_NONE: i32 = -1;
pub const SG_DXFER_TO_DEV: i32 = -2;
pub const SG_DXFER_FROM_DEV: i32 = -3;

/// SCSI 状态码（SPC-5 Table 50）
pub const SCSI_STATUS_GOOD: u8 = 0x00;
/// 命令未执行；目标暂忙。
pub const SCSI_STATUS_BUSY: u8 = 0x08;
pub const SCSI_STATUS_CHECK_CONDITION: u8 = 0x02;
/// 另一个 initiator 持有 Persistent Reservation，本命令未执行。
pub const SCSI_STATUS_RESERVATION_CONFLICT: u8 = 0x18;
/// 任务队列已满。
pub const SCSI_STATUS_TASK_SET_FULL: u8 = 0x28;

/// sg_io_hdr 结构体，用于 SG_IO ioctl
/// 参考 /usr/include/scsi/sg.h
#[repr(C)]
#[derive(Debug)]
pub struct SgIoHdr {
    /// 接口 ID，必须为 'S' (0x53)
    pub interface_id: i32,
    /// 数据传输方向
    pub dxfer_direction: i32,
    /// CDB 长度
    pub cmd_len: u8,
    /// sense buffer 最大长度
    pub mx_sb_len: u8,
    /// iovec 数量（0 表示不使用 scatter-gather）
    pub iovec_count: u16,
    /// 数据传输长度（字节）
    pub dxfer_len: u32,
    /// 数据缓冲区指针
    pub dxferp: u64,
    /// CDB 指针
    pub cmdp: u64,
    /// sense buffer 指针
    pub sbp: u64,
    /// 超时（毫秒）
    pub timeout: u32,
    /// 标志位
    pub flags: u32,
    /// pack id（未使用）
    pub pack_id: i32,
    /// 用户指针（未使用）
    pub usr_ptr: u64,
    /// [out] SCSI 状态
    pub status: u8,
    /// [out] masked status
    pub masked_status: u8,
    /// [out] message status
    pub msg_status: u8,
    /// [out] 实际写入 sense 的字节数
    pub sb_len_wr: u8,
    /// [out] host adapter 状态
    pub host_status: u16,
    /// [out] driver 状态
    pub driver_status: u16,
    /// [out] 剩余未传输字节数
    pub resid: i32,
    /// [out] 命令执行耗时（毫秒）
    pub duration: u32,
    /// [out] 辅助信息
    pub info: u32,
}

impl SgIoHdr {
    pub fn new() -> Self {
        Self {
            interface_id: b'S' as i32,
            dxfer_direction: SG_DXFER_NONE,
            cmd_len: 0,
            mx_sb_len: 0,
            iovec_count: 0,
            dxfer_len: 0,
            dxferp: 0,
            cmdp: 0,
            sbp: 0,
            timeout: 60_000,
            flags: 0,
            pack_id: 0,
            usr_ptr: 0,
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        }
    }
}

// SG_IO ioctl 号在内核 <scsi/sg.h> 里硬编码为 0x2285，
// 既不是 _IOWR('S', 0x85, sg_io_hdr_t)（那是 0xC0xx5385），
// 也不是 ('S'<<8 | 0x85) = 0x5385（request_code_none! 会算成这个）。
// 用错 ioctl 号的症状：驱动把命令路由到 SG_EMULATED_HOST 等其它 handler，
// 向 hdr 本身写回 84 字节 HBA 描述字符串而不向 dxferp 写 INQUIRY 数据，
// 并返回虚假的 host_status（本例里是 0x7562，即 "us" 两个 ASCII 字节）。
const SG_IO: u64 = 0x2285;
nix::ioctl_readwrite_bad!(sg_io, SG_IO, SgIoHdr);
