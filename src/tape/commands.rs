//! 磁带机 SCSI 命令实现

use std::io::{Read, Write};

use bytes::Bytes;
use log::{debug, info};

use crate::error::{TapeError, Result};
use crate::scsi::cdb;
use crate::scsi::device::ScsiDevice;

/// LOG SENSE 响应缓冲区长度（大多数 log page ≤ 8 KiB）。
const LOG_SENSE_BUF_LEN: usize = 8 * 1024;
/// REPORT DENSITY SUPPORT 响应缓冲区长度。
const REPORT_DENSITY_BUF_LEN: usize = 1024;
/// RECEIVE DIAGNOSTIC RESULTS 响应缓冲区长度。
const DIAG_RESULT_BUF_LEN: usize = 4 * 1024;
/// READ(6) / WRITE(6) transfer length 最大值（CDB 字段 24-bit）。
const SCSI_6_TRANSFER_MAX: usize = 0xFF_FFFF;

/// 校验 SPACE(6) 的 24-bit 有符号 count 范围。
fn ensure_space_count(count: i32) -> Result<()> {
    if !(cdb::SPACE_COUNT_MIN..=cdb::SPACE_COUNT_MAX).contains(&count) {
        return Err(TapeError::MoveFailed {
            reason: format!(
                "SPACE count {} 超出 24-bit 有符号范围 [{}, {}]",
                count, cdb::SPACE_COUNT_MIN, cdb::SPACE_COUNT_MAX
            ),
        });
    }
    Ok(())
}

/// 磁带位置信息
#[derive(Debug, Clone)]
pub struct TapePosition {
    /// 当前块号
    pub block_number: u64,
    /// 当前 partition
    pub partition: u32,
    /// 是否在 BOT（Beginning Of Tape）
    pub at_bot: bool,
    /// 是否在 EOT（End Of Tape）
    pub at_eot: bool,
}

/// 磁带机高层封装
pub struct TapeDrive<'a> {
    device: &'a ScsiDevice,
}

impl<'a> TapeDrive<'a> {
    pub fn new(device: &'a ScsiDevice) -> Self {
        Self { device }
    }

    /// TEST UNIT READY: 检查磁带机是否就绪
    ///
    /// LTO 驱动器在 power-on / 复位 / 介质更换后首条命令几乎一定返回
    /// UNIT ATTENTION (0x06)，该 sense 把 pending 状态清 0 后设备即可用；
    /// 因此收到 UA 后重发一次 TUR 再判定 ready 状态。
    pub fn test_unit_ready(&self) -> Result<bool> {
        let cdb_bytes = cdb::test_unit_ready();
        match self.device.execute_no_data(&cdb_bytes, 10_000) {
            Ok(_) => Ok(true),
            Err(TapeError::ScsiCommand { sense_key: 0x02, .. }) => Ok(false),
            Err(TapeError::ScsiCommand { sense_key: 0x06, .. }) => {
                match self.device.execute_no_data(&cdb_bytes, 10_000) {
                    Ok(_) => Ok(true),
                    Err(TapeError::ScsiCommand { sense_key: 0x02, .. }) => Ok(false),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// REWIND: 倒带到 BOT
    pub fn rewind(&self) -> Result<()> {
        info!("倒带...");
        let cdb_bytes = cdb::rewind();
        self.device.execute_no_data(&cdb_bytes, 300_000)?;
        info!("倒带完成");
        Ok(())
    }

    /// READ POSITION: 读取当前位置
    pub fn read_position(&self) -> Result<TapePosition> {
        let cdb_bytes = cdb::read_position();
        let mut buf = [0u8; 20];
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 10_000)?;

        if result.transferred < 20 {
            return Err(TapeError::InvalidResponse { expected: 20, actual: result.transferred });
        }

        // READ POSITION Short Form（SSC-5 service action 0x00）:
        // byte 0: flags（BOP/EOP 等）
        // byte 1: 1 字节 partition number
        // bytes 4..8: First Logical Object Location（当前块号，u32 BE）
        let at_bot = (buf[0] & 0x80) != 0;
        let at_eot = (buf[0] & 0x40) != 0;
        let partition = buf[1] as u32;
        let block_number = u64::from(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]));

        Ok(TapePosition { block_number, partition, at_bot, at_eot })
    }

    /// LOAD: 装载磁带
    pub fn load(&self) -> Result<()> {
        info!("装载磁带...");
        let cdb_bytes = cdb::load_unload(true);
        self.device.execute_no_data(&cdb_bytes, 300_000)?;
        info!("装载完成");
        Ok(())
    }

    /// UNLOAD: 弹出磁带
    pub fn unload(&self) -> Result<()> {
        info!("弹出磁带...");
        let cdb_bytes = cdb::load_unload(false);
        self.device.execute_no_data(&cdb_bytes, 300_000)?;
        info!("弹出完成");
        Ok(())
    }

    /// WRITE: 写入数据块（变长模式）
    pub fn write_block(&self, data: &[u8]) -> Result<usize> {
        if data.len() > SCSI_6_TRANSFER_MAX {
            return Err(TapeError::MoveFailed {
                reason: format!("WRITE(6) 块大小 {} 超过 24-bit 上限 {}", data.len(), SCSI_6_TRANSFER_MAX),
            });
        }
        let cdb_bytes = cdb::write_6(false, data.len() as u32);
        let result = self.device.execute_write(&cdb_bytes, data, 120_000)?;
        debug!("写入 {} 字节, 耗时 {}ms", result.transferred, result.duration_ms);
        Ok(result.transferred)
    }

    /// READ: 读取数据块（变长模式）
    pub fn read_block(&self, buf: &mut [u8]) -> Result<usize> {
        if buf.len() > SCSI_6_TRANSFER_MAX {
            return Err(TapeError::MoveFailed {
                reason: format!("READ(6) 块大小 {} 超过 24-bit 上限 {}", buf.len(), SCSI_6_TRANSFER_MAX),
            });
        }
        let cdb_bytes = cdb::read_6(false, buf.len() as u32);
        let result = self.device.execute_read(&cdb_bytes, buf, 120_000)?;
        debug!("读取 {} 字节, 耗时 {}ms", result.transferred, result.duration_ms);
        Ok(result.transferred)
    }

    /// WRITE FILEMARKS: 写入文件标记
    pub fn write_filemark(&self, count: u32) -> Result<()> {
        debug!("写入 {} 个 filemark", count);
        let cdb_bytes = cdb::write_filemarks(count);
        self.device.execute_no_data(&cdb_bytes, 60_000)?;
        Ok(())
    }

    /// SPACE: 向前跳过 filemarks
    pub fn space_filemarks(&self, count: i32) -> Result<()> {
        ensure_space_count(count)?;
        debug!("跳过 {} 个 filemark", count);
        let cdb_bytes = cdb::space(0x01, count);
        self.device.execute_no_data(&cdb_bytes, 300_000)?;
        Ok(())
    }

    /// SPACE: 向前跳过 blocks
    pub fn space_blocks(&self, count: i32) -> Result<()> {
        ensure_space_count(count)?;
        debug!("跳过 {} 个 block", count);
        let cdb_bytes = cdb::space(0x00, count);
        self.device.execute_no_data(&cdb_bytes, 300_000)?;
        Ok(())
    }

    /// SPACE: 定位到 End-of-Data（code=0x03, count 忽略）。
    pub fn space_to_eod(&self) -> Result<()> {
        debug!("SPACE to EOD");
        let cdb_bytes = cdb::space(0x03, 0);
        self.device.execute_no_data(&cdb_bytes, 600_000)?;
        Ok(())
    }

    /// 从 reader 流式写入磁带：按 block_size 分块，最后写 1 个 filemark。
    ///
    /// 内部只维护一个 block_size 大小的 buffer，不受源文件大小影响；
    /// 适用于任意大文件、pipe、网络流。变长块模式下最后一块允许 < block_size，
    /// 读回时 `read_block` 会返回同样的字节数。
    pub fn write_from_reader<R: Read>(&self, r: &mut R, block_size: usize) -> Result<u64> {
        if block_size == 0 || block_size > SCSI_6_TRANSFER_MAX {
            return Err(TapeError::MoveFailed {
                reason: format!("block_size {} 越界 [1, {}]", block_size, SCSI_6_TRANSFER_MAX),
            });
        }
        let mut buf = vec![0u8; block_size];
        let mut written: u64 = 0;

        loop {
            // 尽量填满一个 block：std::io::Read 允许 partial read，
            // 在 EOF 之前内层循环累积到 block_size。
            let mut filled = 0;
            while filled < block_size {
                match r.read(&mut buf[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            self.write_block(&buf[..filled])?;
            written += filled as u64;
            if filled < block_size {
                break;
            }
        }

        self.write_filemark(1)?;
        info!("文件写入完成: {} 字节", written);
        Ok(written)
    }

    /// LOCATE(16): 定位到指定 partition 的指定逻辑块
    pub fn locate(&self, partition: u8, block: u64, change_partition: bool) -> Result<()> {
        info!("定位到 partition={}, block={}", partition, block);
        let cdb_bytes = cdb::locate_16(partition, block, change_partition, false);
        // 大容量带上 LOCATE 可能较慢
        self.device.execute_no_data(&cdb_bytes, 600_000)?;
        Ok(())
    }

    /// ERASE: 抹带
    /// long_erase=true 会抹整条带（可能数小时），false 仅抹当前位置起
    pub fn erase(&self, long_erase: bool) -> Result<()> {
        info!("抹带（long={}）...", long_erase);
        // long erase 可能需要数小时
        let timeout = if long_erase { 28_800_000 } else { 600_000 };
        let cdb_bytes = cdb::erase_6(long_erase, false);
        self.device.execute_no_data(&cdb_bytes, timeout)?;
        info!("抹带完成");
        Ok(())
    }

    /// FORMAT MEDIUM: 格式化介质
    /// format: 0=默认单 partition，1=按默认 partition 参数格式化
    pub fn format(&self, format: u8, verify: bool) -> Result<()> {
        info!("格式化介质（format={}, verify={}）...", format, verify);
        let cdb_bytes = cdb::format_medium(format, false, verify);
        self.device.execute_no_data(&cdb_bytes, 28_800_000)?;
        info!("格式化完成");
        Ok(())
    }

    /// LOG SENSE: 读取指定 log page 原始数据
    ///
    /// scratch 走栈上 8 KiB 数组，最终 `Bytes::copy_from_slice` 分配恰好
    /// `transferred` 字节的堆缓冲，避免 `BytesMut::freeze` 后底层仍保留
    /// 未使用容量。
    pub fn log_sense(&self, page_code: u8, subpage: u8) -> Result<Bytes> {
        let mut buf = [0u8; LOG_SENSE_BUF_LEN];
        let cdb_bytes = cdb::log_sense(page_code, subpage, LOG_SENSE_BUF_LEN as u16);
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 30_000)?;
        Ok(Bytes::copy_from_slice(&buf[..result.transferred]))
    }

    /// 读取 TapeAlert（log page 0x2E），返回被触发的 flag 编号列表
    pub fn read_tape_alerts(&self) -> Result<Vec<u16>> {
        let data = self.log_sense(0x2E, 0x00)?;
        if data.len() < 4 {
            return Ok(Vec::new());
        }
        // Log page header: 4 bytes
        // 之后为若干 Log Parameter: header(4) + parameter data
        let page_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let end = (4 + page_len).min(data.len());
        let mut alerts = Vec::new();
        let mut offset = 4;
        while offset + 5 <= end {
            let param_code = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let param_len = data[offset + 3] as usize;
            let value_offset = offset + 4;
            if value_offset + param_len > end {
                break;
            }
            // TapeAlert flag 的 parameter data 长度为 1 字节：0/1
            if param_len >= 1 && data[value_offset] != 0 {
                alerts.push(param_code);
            }
            offset = value_offset + param_len;
        }
        Ok(alerts)
    }

    /// MODE SELECT(10): 下发原始 mode parameter list
    pub fn mode_select(&self, parameters: &[u8], save: bool) -> Result<()> {
        let param_len = u16::try_from(parameters.len()).map_err(|_| TapeError::MoveFailed {
            reason: format!("MODE SELECT 参数 {} 字节超出 u16", parameters.len()),
        })?;
        debug!("MODE SELECT(10): {} bytes", param_len);
        let cdb_bytes = cdb::mode_select_10(true, save, param_len);
        self.device.execute_write(&cdb_bytes, parameters, 30_000)?;
        Ok(())
    }

    /// REPORT DENSITY SUPPORT: 读取设备/介质支持的 density 列表，返回原始数据
    pub fn report_density_support(&self, media_only: bool) -> Result<Bytes> {
        let mut buf = [0u8; REPORT_DENSITY_BUF_LEN];
        let cdb_bytes = cdb::report_density_support(media_only, false, REPORT_DENSITY_BUF_LEN as u16);
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 30_000)?;
        Ok(Bytes::copy_from_slice(&buf[..result.transferred]))
    }

    /// SEND DIAGNOSTIC: 默认自检
    /// foreground=true 以 self_test_code=101b 发起前台 short self-test（阻塞至完成）；
    /// false 则用 SELF_TEST=1 触发设备默认自检（多数 LTO 驱动器为前台，但不指定具体 code）
    pub fn send_diagnostic(&self, foreground: bool) -> Result<()> {
        info!("启动自检（foreground={}）", foreground);
        let cdb_bytes = if foreground {
            cdb::send_diagnostic(0b101, false, false, 0)
        } else {
            // SELF_TEST=1 + code=0：默认自检，几乎所有设备都支持
            cdb::send_diagnostic(0, false, true, 0)
        };
        // 自检耗时可能长，给 30 min
        self.device.execute_no_data(&cdb_bytes, 1_800_000)?;
        info!("自检完成");
        Ok(())
    }

    /// RECEIVE DIAGNOSTIC RESULTS: 读取诊断结果
    pub fn receive_diagnostic_results(&self, page_code: Option<u8>) -> Result<Bytes> {
        let mut buf = [0u8; DIAG_RESULT_BUF_LEN];
        let (pcv, code) = match page_code {
            Some(p) => (true, p),
            None => (false, 0),
        };
        let cdb_bytes = cdb::receive_diagnostic_results(pcv, code, DIAG_RESULT_BUF_LEN as u16);
        let result = self.device.execute_read(&cdb_bytes, &mut buf, 30_000)?;
        Ok(Bytes::copy_from_slice(&buf[..result.transferred]))
    }

    /// 从磁带流式读取到 writer，遇 filemark / BLANK CHECK / 达到 max_size 停止。
    ///
    /// - `block_size`：单次 `READ(6)` 的 transfer 长度，应匹配写入时的块大小
    ///   （变长模式下 LTO 要求 read buffer ≥ 实际块）。
    /// - `max_size`：读取字节上限，`0` 表示不设上限（由 filemark 自然终止）。
    ///
    /// 内部只用一个 block_size 大小的 buffer 循环读写，不随磁带数据量增长。
    pub fn read_to_writer<W: Write>(
        &self,
        w: &mut W,
        block_size: usize,
        max_size: u64,
    ) -> Result<u64> {
        if block_size == 0 || block_size > SCSI_6_TRANSFER_MAX {
            return Err(TapeError::MoveFailed {
                reason: format!("block_size {} 越界 [1, {}]", block_size, SCSI_6_TRANSFER_MAX),
            });
        }
        let mut buf = vec![0u8; block_size];
        let mut total: u64 = 0;

        loop {
            if max_size != 0 && total >= max_size {
                break;
            }
            let want = if max_size == 0 {
                block_size
            } else {
                // 剩余空间不超 block_size，但 LTO 变长块仍要求 buffer ≥ 实际块，
                // 所以至少给一个完整 block_size；如果超限由外层 total 截断。
                let remain = max_size - total;
                if remain >= block_size as u64 { block_size } else { remain as usize }
            };
            match self.read_block(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    w.write_all(&buf[..n])?;
                    total += n as u64;
                }
                // BLANK CHECK (sense_key=0x08)：磁带空白区
                Err(TapeError::ScsiCommand { sense_key: 0x08, .. }) => break,
                // FILEMARK detected (NO SENSE + asc=0x00 ascq=0x01)
                Err(TapeError::ScsiCommand { sense_key: 0x00, asc: 0x00, ascq: 0x01, .. }) => break,
                Err(e) => return Err(e),
            }
        }

        w.flush()?;
        info!("文件读取完成: {} 字节", total);
        Ok(total)
    }
}
