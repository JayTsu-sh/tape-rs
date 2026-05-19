//! 磁带机 (tape drive) 相关子命令。

use std::fs::File;
use std::io::{BufReader, BufWriter};

use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::cdb;
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::tape::commands::TapeDrive;

pub fn cmd_rewind(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.rewind()?;
    println!("倒带完成");
    Ok(())
}

pub fn cmd_position(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    let pos = drive.read_position()?;
    println!("=== Tape Position ===");
    println!("  Block:     {}", pos.block_number);
    println!("  Partition: {}", pos.partition);
    println!("  At BOT:    {}", pos.at_bot);
    println!("  At EOT:    {}", pos.at_eot);
    Ok(())
}

pub fn cmd_write(path: &str, file_path: &str, block_size: usize) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);

    let file = File::open(file_path)?;
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = BufReader::with_capacity(block_size.max(64 * 1024), file);

    println!("写入文件: {} ({} 字节, 块大小 {} 字节)", file_path, file_size, block_size);
    let written = drive.write_from_reader(&mut reader, block_size)?;
    println!("写入完成: {} 字节", written);
    Ok(())
}

pub fn cmd_read(path: &str, output_path: &str, block_size: usize, max_size: u64) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);

    let file = File::create(output_path)?;
    let mut writer = BufWriter::with_capacity(block_size.max(64 * 1024), file);

    let limit_label = if max_size == 0 { "无上限".to_string() } else { format!("{} 字节", max_size) };
    println!("读取磁带 → {} (块大小 {}, 上限 {})", output_path, block_size, limit_label);
    let total = drive.read_to_writer(&mut writer, block_size, max_size)?;
    println!("读取完成: {} 字节 → {}", total, output_path);
    Ok(())
}

pub fn cmd_status(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);

    let ready = drive.test_unit_ready()?;
    println!("=== Tape Drive Status: {} ===", path);
    println!("  Ready: {}", if ready { "Yes" } else { "No" });

    if ready {
        match drive.read_position() {
            Ok(pos) => {
                println!("  Block:     {}", pos.block_number);
                println!("  At BOT:    {}", pos.at_bot);
                println!("  At EOT:    {}", pos.at_eot);
            }
            Err(e) => println!("  Position:  Error ({})", e),
        }
    }
    Ok(())
}

pub fn cmd_drive_load(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.load()?;
    println!("装载完成");
    Ok(())
}

pub fn cmd_drive_unload(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.unload()?;
    println!("弹出完成");
    Ok(())
}

pub fn cmd_space(path: &str, mode: &str, count: i32) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    match mode {
        "block" => {
            drive.space_blocks(count)?;
            println!("已跳过 {} 个 block", count);
        }
        "filemark" => {
            drive.space_filemarks(count)?;
            println!("已跳过 {} 个 filemark", count);
        }
        "eod" => {
            // SPACE 到 End-of-Data: code=0x03, count 字段忽略
            let cdb_bytes = cdb::space(0x03, 0);
            dev.execute_no_data(&cdb_bytes, 600_000)?;
            println!("已定位到 EOD");
        }
        other => {
            return Err(TapeError::MoveFailed {
                reason: format!("未知的 space 模式 '{}'（可选 block / filemark / eod）", other),
            });
        }
    }
    Ok(())
}

pub fn cmd_locate(path: &str, partition: u8, block: u64, change_partition: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.locate(partition, block, change_partition)?;
    let pos = drive.read_position()?;
    println!("已定位: partition={}, block={}", pos.partition, pos.block_number);
    Ok(())
}

pub fn cmd_erase(path: &str, long: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.erase(long)?;
    println!("抹带完成");
    Ok(())
}

pub fn cmd_format(path: &str, mode: u8, verify: bool, yes_destroy: bool) -> Result<()> {
    if !yes_destroy {
        return Err(TapeError::Refused {
            reason: format!(
                "FORMAT MEDIUM 会擦除 {} 上的磁带，加 --yes-destroy 确认后再执行",
                path
            ),
        });
    }
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.format(mode, verify)?;
    println!("格式化完成");
    Ok(())
}

pub fn cmd_log_sense(path: &str, page_hex: &str, raw: bool) -> Result<()> {
    let trimmed = page_hex.trim_start_matches("0x").trim_start_matches("0X");
    let page = u8::from_str_radix(trimmed, 16).map_err(|e| TapeError::MoveFailed {
        reason: format!("page code '{}' 解析失败: {}", page_hex, e),
    })?;

    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);

    if page == 0x2E && !raw {
        let alerts = drive.read_tape_alerts()?;
        println!("=== TapeAlert (page 0x2E) ===");
        if alerts.is_empty() {
            println!("  无告警");
        } else {
            for flag in &alerts {
                println!("  Flag #{}", flag);
            }
        }
        return Ok(());
    }

    let data = drive.log_sense(page, 0x00)?;
    println!("=== LOG SENSE page {:#04x}, {} 字节 ===", page, data.len());
    if raw || page != 0x2E {
        for (i, chunk) in data.chunks(16).enumerate() {
            print!("  {:04x}:", i * 16);
            for b in chunk {
                print!(" {:02x}", b);
            }
            println!();
        }
    }
    Ok(())
}

pub fn cmd_report_density(path: &str, media_only: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    let data = drive.report_density_support(media_only)?;

    if data.len() < 4 {
        return Err(TapeError::InvalidResponse { expected: 4, actual: data.len() });
    }
    let avail_len = u16::from_be_bytes([data[0], data[1]]) as usize;
    let end = (4 + avail_len).min(data.len());

    println!("=== Density Support ({}字节) ===", data.len());
    let mut offset = 4;
    // 每条 Density descriptor 52 字节（SSC-3）
    while offset + 52 <= end {
        let code = data[offset];
        let secondary = data[offset + 1];
        let wrtok = (data[offset + 2] & 0x80) != 0;
        let dup = (data[offset + 2] & 0x40) != 0;
        let deflt = (data[offset + 2] & 0x20) != 0;
        let bits_per_mm = u32::from_be_bytes([0, data[offset + 5], data[offset + 6], data[offset + 7]]);
        let tracks = u16::from_be_bytes([data[offset + 8], data[offset + 9]]);
        let capacity = u32::from_be_bytes([
            data[offset + 10], data[offset + 11], data[offset + 12], data[offset + 13],
        ]);
        let vendor = String::from_utf8_lossy(&data[offset + 14..offset + 22]).trim().to_string();
        let desc = String::from_utf8_lossy(&data[offset + 22..offset + 30]).trim().to_string();
        let name = String::from_utf8_lossy(&data[offset + 30..offset + 52]).trim().to_string();

        println!("  - code={:#04x} secondary={:#04x} wrtok={} dup={} default={}",
            code, secondary, wrtok, dup, deflt);
        println!("    bits/mm={} tracks={} capacity={}MB vendor={} desc={} name={}",
            bits_per_mm, tracks, capacity, vendor, desc, name);
        offset += 52;
    }
    Ok(())
}

pub fn cmd_diagnostic(path: &str, foreground: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let drive = TapeDrive::new(&dev);
    drive.send_diagnostic(foreground)?;
    println!("自检已{}", if foreground { "完成" } else { "提交（后台运行）" });

    if foreground {
        match drive.receive_diagnostic_results(None) {
            Ok(data) if !data.is_empty() => {
                println!("=== Diagnostic Results ({}字节) ===", data.len());
                for (i, chunk) in data.chunks(16).enumerate() {
                    print!("  {:04x}:", i * 16);
                    for b in chunk {
                        print!(" {:02x}", b);
                    }
                    println!();
                }
            }
            Ok(_) => println!("无诊断结果数据"),
            Err(e) => println!("读取诊断结果失败: {}", e),
        }
    }
    Ok(())
}
