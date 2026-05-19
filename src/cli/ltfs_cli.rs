//! LTFS 相关子命令。

use std::fs::File;
use std::io::{BufReader, BufWriter};

use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::mkltfs::{self, MkltfsOptions};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::device::ScsiDevice;

pub fn cmd_mkltfs(
    path: &str,
    volume_id: &str,
    owner: &str,
    block_size: u32,
    compression: bool,
    yes_destroy: bool,
    quick: bool,
) -> Result<()> {
    if !yes_destroy {
        return Err(TapeError::Refused {
            reason: "mkltfs 会抹掉磁带，请加 --yes-destroy 确认".into(),
        });
    }
    let dev = ScsiDevice::open(path)?;
    let opts = MkltfsOptions {
        volume_id: volume_id.to_string(),
        owner: owner.to_string(),
        block_size,
        compression,
        volume_uuid: None,
        quick,
    };
    if quick {
        println!("mkltfs quick 模式（跳过 FORMAT MEDIUM）...");
    } else {
        println!("mkltfs 开始（可能耗时数十分钟）...");
    }
    let uuid = mkltfs::mkltfs(&dev, &opts)?;
    println!("完成。Volume UUID = {}", uuid);
    Ok(())
}

pub fn cmd_ltfs_list(path: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let vol = LtfsVolume::mount(&dev)?;
    let label = vol.label();
    println!("=== LTFS Volume ===");
    println!("  UUID:        {}", label.volume_uuid);
    println!("  Version:     {}", label.version);
    println!("  Blocksize:   {}", label.blocksize);
    println!("  Compression: {}", label.compression);
    println!("  Generation:  {}", vol.index().generation);
    println!();
    println!("=== Files ===");
    let files = vol.list();
    if files.is_empty() {
        println!("  (empty)");
    } else {
        for (p, sz) in &files {
            println!("  {:>12}  {}", sz, p);
        }
    }
    Ok(())
}

pub fn cmd_ltfs_read(path: &str, name: &str, output: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let vol = LtfsVolume::mount(&dev)?;
    let file = File::create(output)?;
    let bs = vol.block_size() as usize;
    let mut w = BufWriter::with_capacity(bs.max(64 * 1024), file);
    println!("读取 {} → {}", name, output);
    let n = vol.read_file_to_writer(name, &mut w)?;
    println!("完成: {} 字节", n);
    Ok(())
}

pub fn cmd_ltfs_write(path: &str, file_path: &str, name: &str) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let mut vol = LtfsVolume::mount(&dev)?;
    let f = File::open(file_path)?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let bs = vol.block_size() as usize;
    let mut r = BufReader::with_capacity(bs.max(64 * 1024), f);
    println!("写入 {} ({} 字节) → tape:{}", file_path, size, name);
    let n = vol.append_file(name, &mut r)?;
    println!("已追加 {} 字节，执行 commit 写回 index...", n);
    vol.unmount()?;
    println!("完成");
    Ok(())
}
