//! `TapeTransport`：SCSI 命令传输 seam。
//!
//! 上层（`TapeDrive`、`MediumChanger`、`Mam`、`LtfsVolume`）只依赖这个 trait，
//! 不关心命令最终走 `SG_IO` ioctl（[`ScsiDevice`]）还是内存模拟器
//! ([`crate::scsi::sim::SimTransport`])。三个 `execute_*` 的语义与
//! `ScsiDevice` 原有同名方法完全一致：GOOD 返回 `Ok`；CHECK CONDITION 且
//! sense 非 NO SENSE/RECOVERED 返回 `TapeError::ScsiCommand`；其余状态按
//! `ScsiDevice` 的分类返回对应错误。

use log::warn;

use crate::error::{Result, TapeError};
use crate::scsi::device::{ScsiDevice, ScsiResult};

pub trait TapeTransport {
    /// 无数据传输的命令。
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult>;

    /// 从设备读取到 `buf`；`ScsiResult::transferred` 为实际字节数。
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult>;

    /// 把 `buf` 写入设备。
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult>;

    /// 本句柄观察到的状态变化代数；未实现的transport不允许复用可写卷会话。
    /// 必须在错误/UA被重试层消费前更新；修改或定位命令也使旧快照失效。
    fn session_epoch(&self) -> Option<u64> {
        None
    }

    /// 设备身份描述，用于日志与错误信息（真实设备为 `/dev/sg*` 路径）。
    fn identity(&self) -> &str;
}

pub(crate) fn changes_session(cdb: &[u8]) -> bool {
    !matches!(
        cdb.first(),
        Some(0x00 | 0x05 | 0x08 | 0x12 | 0x1a | 0x34 | 0x4d | 0x5a | 0x5e | 0x8c)
    )
}

impl TapeTransport for ScsiDevice {
    fn session_epoch(&self) -> Option<u64> {
        Some(self.session_epoch_value())
    }

    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        ScsiDevice::execute_no_data(self, cdb, timeout_ms)
    }

    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        ScsiDevice::execute_read(self, cdb, buf, timeout_ms)
    }

    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        ScsiDevice::execute_write(self, cdb, buf, timeout_ms)
    }

    fn identity(&self) -> &str {
        self.path()
    }
}

/// UNIT ATTENTION 的有界重试。
///
/// UA（sense key 0x06）是设备"代替执行"上报的：命令本身没有执行，所以对**位置无关**的
/// 命令（TUR、REWIND、LOCATE、READ POSITION、LOAD/UNLOAD、MODE SELECT/SENSE、MAM、
/// 换带器命令）重发是安全的。依赖当前位置的命令（READ/WRITE/WRITE FILEMARKS/SPACE/
/// ERASE）**不得**使用本函数：UA 0x29（复位）意味着位置可能已丢失，盲目重试会写到
/// 错误位置；这些命令应把 UA 原样上抛，由提交冻结与恢复协议处理。
///
/// 实测依据：Holo-VTL 在 FORMAT MEDIUM 后对下一条命令返回 0x06/2A/01，在介质装入后
/// 连续返回多个 0x06/28/00。
pub fn retry_unit_attention<T>(what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    const MAX_UNIT_ATTENTIONS: usize = 5;
    let mut seen = 0;
    loop {
        match f() {
            Err(TapeError::ScsiCommand {
                sense_key: 0x06,
                asc,
                ascq,
                ..
            }) if seen < MAX_UNIT_ATTENTIONS && !(asc == 0x2A && matches!(ascq, 0x03..=0x05)) => {
                // 2A/03..05（注册被抢占、预留被释放/抢占）是失去执行资格的信号，
                // 不能当普通 UNIT ATTENTION 吞掉，原样上报。
                seen += 1;
                warn!(
                    "{}: UNIT ATTENTION asc={:#04x} ascq={:#04x}，重发（第 {} 次）",
                    what, asc, ascq, seen
                );
            }
            other => return other,
        }
    }
}
