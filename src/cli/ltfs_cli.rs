//! LTFS 相关子命令。

use std::fs::File;
use std::io::{BufReader, BufWriter};

use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::mkltfs::{self, MkltfsOptions};
use tape_rs::ltfs::volume::{HashPolicy, HashVerdict, LtfsVolume};
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
            match vol.index().find_file(p).and_then(|f| f.symlink.as_deref()) {
                Some(target) => println!("  {:>12}  {} -> {}", "symlink", p, target),
                None => println!("  {:>12}  {}", sz, p),
            }
        }
    }
    if let Some(reason) = vol.restricted_reason() {
        println!();
        println!("只读: {}", reason);
    }
    Ok(())
}

/// 读出文件并与索引里的 ltfs.hash.* 比对。name 为空时校验全部文件。
pub fn cmd_ltfs_verify(path: &str, name: Option<&str>, show_xattrs: bool) -> Result<()> {
    let dev = ScsiDevice::open(path)?;
    let vol = LtfsVolume::mount(&dev)?;
    let targets: Vec<String> = match name {
        Some(n) => vec![n.to_string()],
        None => vol.list().into_iter().map(|(p, _)| p).collect(),
    };
    let mut bad = 0usize;
    for p in &targets {
        let verdict = vol.verify_file(p)?;
        match &verdict {
            HashVerdict::Match { algo } => println!("  一致   {:<10} {}", algo, p),
            HashVerdict::NoHash => println!("  无哈希 {:<10} {}", "-", p),
            HashVerdict::Mismatch { algo, expected, actual } => {
                bad += 1;
                println!("  不一致 {:<10} {}", algo, p);
                println!("         索引: {}", expected);
                println!("         实际: {}", actual);
            }
        }
        if show_xattrs {
            if let Some(f) = vol.index().find_file(p) {
                for x in &f.xattrs {
                    println!("         {} = {}{}", x.key, x.value, if x.base64 { " (base64)" } else { "" });
                }
            }
        }
    }
    if bad > 0 {
        return Err(TapeError::Ltfs(format!("{} 个文件哈希不一致", bad)));
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

pub fn cmd_ltfs_write(
    path: &str,
    file_path: &str,
    name: &str,
    xattrs: &[String],
    md5: bool,
) -> Result<()> {
    let pairs: Vec<(&str, &str)> = xattrs
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| TapeError::Ltfs(format!("--xattr 需要 KEY=VALUE 形式: {}", kv)))
        })
        .collect::<Result<_>>()?;
    let dev = ScsiDevice::open(path)?;
    let mut vol = LtfsVolume::mount(&dev)?;
    let f = File::open(file_path)?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let bs = vol.block_size() as usize;
    let mut r = BufReader::with_capacity(bs.max(64 * 1024), f);
    println!("写入 {} ({} 字节) → tape:{}", file_path, size, name);
    vol.set_hash_policy(HashPolicy { md5, sha256: true });
    let n = vol.append_file_with_xattrs(name, &mut r, &pairs)?;
    println!("已追加 {} 字节，执行 commit 写回 index...", n);
    vol.unmount()?;
    println!("完成");
    Ok(())
}
