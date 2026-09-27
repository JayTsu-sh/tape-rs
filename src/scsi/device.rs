//! SCSI 通用设备封装，通过 SG_IO ioctl 发送原始 CDB

use std::fs::OpenOptions;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{debug, warn};

use crate::error::{Result, TapeError};
use crate::scsi::sense::SenseInfo;
use crate::scsi::sg_io::{
    SCSI_STATUS_BUSY, SCSI_STATUS_CHECK_CONDITION, SCSI_STATUS_GOOD,
    SCSI_STATUS_RESERVATION_CONFLICT, SCSI_STATUS_TASK_SET_FULL, SG_DXFER_FROM_DEV, SG_DXFER_NONE,
    SG_DXFER_TO_DEV, SG_FLAG_DIRECT_IO, SG_INFO_DIRECT_IO, SG_INFO_DIRECT_IO_MASK,
    SG_INFO_MIXED_IO, SgIoHdr,
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

/// 本句柄请求直接 I/O 后内核报告的结果；只统计已返回的命令。
#[derive(Debug, Default, Clone, Copy)]
pub struct IoCounts {
    pub direct: u64,
    pub mixed: u64,
    pub indirect: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DirectIoStats {
    pub read: IoCounts,
    pub write: IoCounts,
}

/// SCSI 通用设备
pub struct ScsiDevice {
    fd: OwnedFd,
    path: String,
    direct: [AtomicU64; 2],
    mixed: [AtomicU64; 2],
    indirect: [AtomicU64; 2],
    command: Mutex<()>,
    failed: AtomicBool,
}

/// 打开后、发出任何命令前配置；申请失败或不足仍可由 sg 动态分配间接缓冲。
fn configure_reserved_buffer(fd: &OwnedFd, path: &str) {
    let requested: i32 = 1024 * 1024;
    // SAFETY: fd 在本调用期间由 OwnedFd 保持有效；内核同步读取一个有效的 int，
    // 不保留此指针。此时尚未发布句柄，无并发命令、mmap 或在途 DMA。
    if let Err(e) = unsafe { crate::scsi::sg_io::sg_set_reserved_size(fd.as_raw_fd(), &requested) }
    {
        warn!("{}: SG 预留缓冲申请失败，继续使用内核间接分配: {}", path, e);
    }
    let mut actual: i32 = 0;
    // SAFETY: 同上；actual 为完整、可写且存活至 ioctl 返回的 int。
    match unsafe { crate::scsi::sg_io::sg_get_reserved_size(fd.as_raw_fd(), &mut actual) } {
        Ok(_) if actual >= requested => debug!("{}: SG 预留缓冲 {} 字节", path, actual),
        Ok(_) => warn!(
            "{}: SG 预留缓冲 {} 字节，小于申请的 {}，大请求使用间接分配",
            path, actual, requested
        ),
        Err(e) => warn!("{}: SG 预留缓冲回读失败，继续使用内核间接分配: {}", path, e),
    }
}

impl ScsiDevice {
    /// 打开 SCSI 通用设备（/dev/sg*）
    pub fn open(path: &str) -> Result<Self> {
        // 非阻塞打开：别的进程以 O_EXCL 持有该 sg 节点时（IBM LTFS 对它自己的驱动器就是这样），
        // 阻塞式 open 会在内核 sg_open/open_wait 里无限等待。O_NONBLOCK 下立即得到 EBUSY。
        // 打开后清掉该标志，保持 SG_IO 的同步语义。
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)?;

        let fd = OwnedFd::from(file);
        let flags = nix::fcntl::fcntl(fd.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL)?;
        let flags = nix::fcntl::OFlag::from_bits_truncate(flags) & !nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(fd.as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(flags))?;
        debug!("打开 SCSI 设备: {}", path);

        // direct I/O 的错误返回必须能查询尚未完成的命令，先核实当前 fd 支持 sg v3。
        pending_io(fd.as_raw_fd())?;
        // 与 LTFS sg 后端一致，按 fd 预留 1MiB，覆盖常用 512KiB 记录。
        // 仅调整内核缓冲，不改变介质/驱动模式或全机 allow_dio。
        configure_reserved_buffer(&fd, path);
        Ok(Self {
            fd,
            path: path.to_string(),
            direct: std::array::from_fn(|_| AtomicU64::new(0)),
            mixed: std::array::from_fn(|_| AtomicU64::new(0)),
            indirect: std::array::from_fn(|_| AtomicU64::new(0)),
            command: Mutex::new(()),
            failed: AtomicBool::new(false),
        })
    }

    /// 获取设备路径
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn direct_io_stats(&self) -> DirectIoStats {
        let counts = |i: usize| IoCounts {
            direct: self.direct[i].load(Ordering::Relaxed),
            mixed: self.mixed[i].load(Ordering::Relaxed),
            indirect: self.indirect[i].load(Ordering::Relaxed),
        };
        DirectIoStats {
            read: counts(0),
            write: counts(1),
        }
    }

    /// 发起 ioctl 并解析结果。dxferp/dxfer_len 由调用方按方向准备好。
    /// 注意：`dxferp` 必须在 `dxfer_len` 为 0 时写成 0；否则必须是有效的缓冲区指针，
    /// 其生命周期覆盖整个调用，包括 ioctl 错误后等待在途 DMA 完成。
    fn execute_raw(
        &self,
        cdb: &[u8],
        direction: Direction,
        dxferp: u64,
        dxfer_len: usize,
        timeout_ms: u32,
    ) -> Result<ScsiResult> {
        let _command = self.command.lock().unwrap_or_else(|e| e.into_inner());
        if self.failed.load(Ordering::Relaxed) {
            return Err(TapeError::Refused {
                reason: format!("{} 的 SG_IO 结果未定，需要重新打开并恢复", self.path),
            });
        }
        let mut sense_buf = [0u8; 64];
        let mut hdr = SgIoHdr::new();

        let (cmd_len, dxfer_len) = checked_lengths(cdb.len(), dxfer_len)?;
        hdr.cmd_len = cmd_len;
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
        hdr.dxferp = if dxfer_len == 0 { 0 } else { dxferp };
        // 只对磁带数据 READ/WRITE 请求 direct I/O。内核自动回退，不重发任何命令。
        hdr.flags = transfer_flags(cdb, direction, dxfer_len);

        // 执行 SG_IO ioctl
        // SG_IO 可在 EINTR/设备脱离时早于命令完成返回。此时仍保持调用方缓冲区借用，
        // 只查询请求表直到设备完成，绝不再次提交 CDB；之后冻结本句柄并报告原始错误。
        let result = unsafe { crate::scsi::sg_io::sg_io(self.fd.as_raw_fd(), &mut hdr) };
        if let Err(error) = result {
            self.failed.store(true, Ordering::Relaxed);
            wait_for_io_completion(
                || pending_io(self.fd.as_raw_fd()),
                || std::thread::sleep(std::time::Duration::from_millis(10)),
            );
            return Err(error.into());
        }

        if hdr.flags & SG_FLAG_DIRECT_IO != 0 {
            let i = usize::from(matches!(direction, Direction::ToDevice));
            let counter = match hdr.info & SG_INFO_DIRECT_IO_MASK {
                SG_INFO_DIRECT_IO => &self.direct[i],
                SG_INFO_MIXED_IO => &self.mixed[i],
                _ => &self.indirect[i],
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if hdr.sb_len_wr as usize > sense_buf.len() {
            return Err(TapeError::InvalidResponse {
                expected: sense_buf.len(),
                actual: hdr.sb_len_wr as usize,
            });
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
            "SCSI 命令完成: cdb={:02x?}, status={:#04x}, transferred={}, duration={}ms, info={:#x}, sense={}",
            cdb, hdr.status, transferred, hdr.duration, hdr.info, sense
        );

        finish_command(hdr.status, sense, transferred, hdr.duration, &self.path)
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
            Direction::None => (0u64, 0usize),
            Direction::FromDevice => (buf.as_mut_ptr() as u64, buf.len()),
            Direction::ToDevice => (buf.as_ptr() as u64, buf.len()),
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
        let dxfer_len = buf.len();
        self.execute_raw(cdb, Direction::FromDevice, dxferp, dxfer_len, timeout_ms)
    }

    /// 执行写入命令。接受只读切片，避免调用方为了满足签名额外分配。
    pub fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        let dxferp = buf.as_ptr() as u64;
        let dxfer_len = buf.len();
        self.execute_raw(cdb, Direction::ToDevice, dxferp, dxfer_len, timeout_ms)
    }
}

fn pending_io(fd: std::os::fd::RawFd) -> nix::Result<bool> {
    use crate::scsi::sg_io::{SG_MAX_QUEUE, SgRequestInfo, sg_get_request_table};
    let mut requests = [SgRequestInfo::default(); SG_MAX_QUEUE];
    // 数组长度、布局与 Linux SG_MAX_QUEUE/sg_req_info_t 一致；整个 ioctl 期间有效。
    unsafe {
        sg_get_request_table(fd, &mut requests)?;
    }
    Ok(requests.iter().any(|r| r.req_state == 1))
}

fn wait_for_io_completion(mut pending: impl FnMut() -> nix::Result<bool>, mut wait: impl FnMut()) {
    loop {
        match pending() {
            Ok(false) => return,
            Ok(true) | Err(nix::errno::Errno::EINTR | nix::errno::Errno::ENOMEM) => wait(),
            Err(e) => {
                // open 时已核实该 ioctl。无法再证明 DMA 完成时不能归还可能仍被设备使用的
                // 用户内存；panic 会展开并释放缓冲区，因此只能终止进程，留给上层恢复。
                log::error!("无法确认 SG_IO 已完成，拒绝释放仍可能被 DMA 使用的缓冲区: {e}");
                std::process::abort();
            }
        }
    }
}

fn checked_lengths(cdb_len: usize, data_len: usize) -> Result<(u8, u32)> {
    if cdb_len == 0 || cdb_len > u8::MAX as usize || data_len > u32::MAX as usize {
        return Err(TapeError::Refused {
            reason: format!("SG_IO 长度非法: CDB={cdb_len}, data={data_len}"),
        });
    }
    Ok((cdb_len as u8, data_len as u32))
}

fn transfer_flags(cdb: &[u8], direction: Direction, len: u32) -> u32 {
    if len != 0
        && matches!(
            (cdb.first(), direction),
            (Some(0x08), Direction::FromDevice) | (Some(0x0a), Direction::ToDevice)
        )
    {
        SG_FLAG_DIRECT_IO
    } else {
        0
    }
}

impl std::fmt::Display for ScsiDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScsiDevice({})", self.path)
    }
}

/// 把一次命令的 SCSI 状态、sense 与传输结果归类为 `Result`。
///
/// `ScsiDevice` 与 `SimTransport` 共用这一逻辑，保证模拟器与真实设备对
/// CHECK CONDITION / RESERVATION CONFLICT / BUSY 等状态的处理完全一致：
/// status != GOOD 要分门别类报错，而不是当成功返回 — 否则像 0x18
/// (RESERVATION CONFLICT)、0x08 (BUSY) 这类不带 sense 的状态会被吞掉，
/// 上层会以为命令成功执行（实际上 target 根本没动）。
pub(crate) fn finish_command(
    status: u8,
    sense: SenseInfo,
    transferred: usize,
    duration_ms: u32,
    device: &str,
) -> Result<ScsiResult> {
    match status {
        SCSI_STATUS_GOOD => {}
        SCSI_STATUS_CHECK_CONDITION if !sense.is_ok() => {
            return Err(TapeError::ScsiCommand {
                status,
                sense_key: sense.sense_key,
                asc: sense.asc,
                ascq: sense.ascq,
            });
        }
        SCSI_STATUS_CHECK_CONDITION => {
            // sense 显示 NO SENSE（filemark / BOP / ILI 等提示），仍按成功对待但保留 status。
        }
        SCSI_STATUS_RESERVATION_CONFLICT => {
            return Err(TapeError::ReservationConflict {
                device: device.to_string(),
            });
        }
        SCSI_STATUS_BUSY => {
            return Err(TapeError::Busy {
                device: device.to_string(),
            });
        }
        SCSI_STATUS_TASK_SET_FULL => {
            return Err(TapeError::ScsiStatus {
                device: device.to_string(),
                status,
            });
        }
        _ => {
            return Err(TapeError::ScsiStatus {
                device: device.to_string(),
                status,
            });
        }
    }

    Ok(ScsiResult {
        status,
        sense,
        transferred,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_io_waits_for_completion_without_resubmitting() {
        let mut states = [Ok(true), Err(nix::errno::Errno::EINTR), Ok(true), Ok(false)].into_iter();
        let mut waits = 0;
        wait_for_io_completion(|| states.next().unwrap(), || waits += 1);
        assert_eq!(waits, 3);
        assert!(states.next().is_none());
    }

    #[test]
    fn sg_request_table_matches_linux_64_bit_layout() {
        use crate::scsi::sg_io::SgRequestInfo;
        assert_eq!(std::mem::size_of::<SgRequestInfo>(), 24);
        assert_eq!(std::mem::offset_of!(SgRequestInfo, usr_ptr), 8);
        assert_eq!(std::mem::offset_of!(SgRequestInfo, duration), 16);
    }

    #[test]
    fn direct_io_only_for_nonempty_tape_data_in_matching_direction() {
        assert_eq!(
            transfer_flags(&[0x08], Direction::FromDevice, 524288),
            SG_FLAG_DIRECT_IO
        );
        assert_eq!(
            transfer_flags(&[0x0a], Direction::ToDevice, 1),
            SG_FLAG_DIRECT_IO
        );
        for (cdb, dir, len) in [
            (0x08, Direction::ToDevice, 512),
            (0x0a, Direction::FromDevice, 512),
            (0x0a, Direction::ToDevice, 0),
            (0x8d, Direction::ToDevice, 512),
            (0x12, Direction::FromDevice, 512),
            (0x10, Direction::None, 0),
        ] {
            assert_eq!(transfer_flags(&[cdb], dir, len), 0);
        }
    }

    #[test]
    fn reject_sg_lengths_before_truncating() {
        assert!(checked_lengths(0, 0).is_err());
        assert!(checked_lengths(256, 0).is_err());
        assert!(checked_lengths(6, u32::MAX as usize + 1).is_err());
        assert_eq!(checked_lengths(16, 0).unwrap(), (16, 0));
    }
}
