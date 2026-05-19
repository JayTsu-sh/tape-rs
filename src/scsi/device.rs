//! SCSI 通用设备封装，通过 SG_IO ioctl 发送原始 CDB

use std::fs::OpenOptions;
use std::os::unix::io::{AsRawFd, OwnedFd};

use log::debug;

use crate::error::{TapeError, Result};
use crate::scsi::sense::SenseInfo;
use crate::scsi::sg_io::{
    SG_DXFER_FROM_DEV, SG_DXFER_NONE, SG_DXFER_TO_DEV,
    SCSI_STATUS_BUSY, SCSI_STATUS_CHECK_CONDITION, SCSI_STATUS_GOOD,
    SCSI_STATUS_RESERVATION_CONFLICT, SCSI_STATUS_TASK_SET_FULL, SgIoHdr,
};

/// 数据传输方向
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    /// 无数据传输
    None,
    /// 从设备读取
    FromDevice,
    /// 写入设备
    ToDevice,
}

/// SCSI 命令执行结果
#[derive(Debug)]
pub struct ScsiResult {
    /// SCSI 状态码
    pub status: u8,
    /// Sense 数据
    pub sense: SenseInfo,
    /// 实际传输的字节数
    pub transferred: usize,
    /// 命令耗时（毫秒）
    pub duration_ms: u32,
}

/// SCSI 通用设备
pub struct ScsiDevice {
    fd: OwnedFd,
    path: String,
}

impl ScsiDevice {
    /// 打开 SCSI 通用设备（/dev/sg*）
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        let fd = OwnedFd::from(file);
        debug!("打开 SCSI 设备: {}", path);

        Ok(Self {
            fd,
            path: path.to_string(),
        })
    }

    /// 获取设备路径
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 发起 ioctl 并解析结果。dxferp/dxfer_len 由调用方按方向准备好。
    /// 注意：`dxferp` 必须在 `dxfer_len` 为 0 时写成 0；否则必须是有效的缓冲区指针，
    /// 其生命周期至少覆盖到 ioctl 返回。
    fn execute_raw(
        &self,
        cdb: &[u8],
        direction: Direction,
        dxferp: u64,
        dxfer_len: u32,
        timeout_ms: u32,
    ) -> Result<ScsiResult> {
        let mut sense_buf = [0u8; 64];
        let mut hdr = SgIoHdr::new();

        // 所有 CDB 来自 scsi::cdb 的 [u8; N] 固定数组（最大 16）；
        // 超过 u8::MAX 只能是逻辑 bug，留 debug_assert 捕获。
        debug_assert!(cdb.len() <= u8::MAX as usize, "CDB 超长: {}", cdb.len());
        hdr.cmd_len = cdb.len() as u8;
        hdr.cmdp = cdb.as_ptr() as u64;
        hdr.mx_sb_len = sense_buf.len() as u8;
        hdr.sbp = sense_buf.as_mut_ptr() as u64;
        hdr.timeout = timeout_ms;
        hdr.dxfer_direction = match direction {
            Direction::None => SG_DXFER_NONE,
            Direction::FromDevice => SG_DXFER_FROM_DEV,
            Direction::ToDevice => SG_DXFER_TO_DEV,
        };
        hdr.dxfer_len = dxfer_len;
        hdr.dxferp = dxferp;

        // 执行 SG_IO ioctl
        unsafe {
            crate::scsi::sg_io::sg_io(self.fd.as_raw_fd(), &mut hdr)?;
        }

        // 检查 host/driver 错误。driver_status 低 4 位的 0x08 (DRIVER_SENSE)
        // 只是表示"sense buffer 有效"，配合 CHECK CONDITION 一起来，不是错误。
        if hdr.host_status != 0 {
            return Err(TapeError::ScsiHost(hdr.host_status));
        }
        let driver_suggest = hdr.driver_status & 0xF0;
        let driver_code = hdr.driver_status & 0x0F;
        if driver_code != 0 && driver_code != 0x08 {
            return Err(TapeError::ScsiDriver(hdr.driver_status));
        }
        if driver_suggest != 0 && driver_suggest != 0x80 {
            // 0x80 = SUGGEST_SENSE，也无需当错误
            return Err(TapeError::ScsiDriver(hdr.driver_status));
        }

        let sense = SenseInfo::from_bytes(&sense_buf[..hdr.sb_len_wr as usize]);
        // 计算实际传输字节数。resid 理论上为 [0, dxfer_len]，
        // 但使用 saturating 运算避免驱动返回异常值时的溢出。
        let resid_nonneg = hdr.resid.max(0) as u32;
        let transferred = hdr.dxfer_len.saturating_sub(resid_nonneg) as usize;

        debug!(
            "SCSI 命令完成: status={:#04x}, transferred={}, duration={}ms, sense={}",
            hdr.status, transferred, hdr.duration, sense
        );

        // status != GOOD 要分门别类报错，而不是当成功返回 — 否则像 0x18
        // (RESERVATION CONFLICT)、0x08 (BUSY) 这类不带 sense 的状态会被吞掉，
        // 上层会以为命令成功执行（实际上 target 根本没动）。
        match hdr.status {
            SCSI_STATUS_GOOD => {}
            SCSI_STATUS_CHECK_CONDITION if !sense.is_ok() => {
                return Err(TapeError::ScsiCommand {
                    status: hdr.status,
                    sense_key: sense.sense_key,
                    asc: sense.asc,
                    ascq: sense.ascq,
                });
            }
            SCSI_STATUS_CHECK_CONDITION => {
                // sense 显示 NO SENSE（极少见），仍按成功对待但保留 status。
            }
            SCSI_STATUS_RESERVATION_CONFLICT => {
                return Err(TapeError::ReservationConflict { device: self.path.clone() });
            }
            SCSI_STATUS_BUSY => {
                return Err(TapeError::Busy { device: self.path.clone() });
            }
            SCSI_STATUS_TASK_SET_FULL => {
                return Err(TapeError::ScsiStatus { device: self.path.clone(), status: hdr.status });
            }
            _ => {
                return Err(TapeError::ScsiStatus { device: self.path.clone(), status: hdr.status });
            }
        }

        Ok(ScsiResult {
            status: hdr.status,
            sense,
            transferred,
            duration_ms: hdr.duration,
        })
    }

    /// 执行 SCSI 命令（兼容旧接口，保留可变缓冲区签名）
    pub fn execute(
        &self,
        cdb: &[u8],
        direction: Direction,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<ScsiResult> {
        let (dxferp, dxfer_len) = match direction {
            Direction::None => (0u64, 0u32),
            Direction::FromDevice => (buf.as_mut_ptr() as u64, buf.len() as u32),
            Direction::ToDevice => (buf.as_ptr() as u64, buf.len() as u32),
        };
        self.execute_raw(cdb, direction, dxferp, dxfer_len, timeout_ms)
    }

    /// 执行无数据传输的命令
    pub fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.execute_raw(cdb, Direction::None, 0, 0, timeout_ms)
    }

    /// 执行读取命令
    pub fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        let dxferp = buf.as_mut_ptr() as u64;
        let dxfer_len = buf.len() as u32;
        self.execute_raw(cdb, Direction::FromDevice, dxferp, dxfer_len, timeout_ms)
    }

    /// 执行写入命令。接受只读切片，避免调用方为了满足签名额外分配。
    pub fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        let dxferp = buf.as_ptr() as u64;
        let dxfer_len = buf.len() as u32;
        self.execute_raw(cdb, Direction::ToDevice, dxferp, dxfer_len, timeout_ms)
    }
}

impl std::fmt::Display for ScsiDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScsiDevice({})", self.path)
    }
}
