//! 同一 Holo 介质上的直接库基线。只允许显式门控和 PF2701/PF2702 UUID；不格式化、不换带。
//! 装卸带由外部探针核对 inventory 后执行，计时单独保存。不要对业务介质运行。
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::json;
use sha2::{Digest, Sha256};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::read_unit_serial;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    device: String,
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    group: String,
    #[arg(long)]
    mode: String,
    #[arg(long)]
    path: Option<String>,
    #[arg(long)]
    once: bool,
}
struct Manifest {
    barcode: String,
    volume_uuid: uuid::Uuid,
    files: Vec<Entry>,
}
struct Entry {
    path: String,
    group: String,
    size: u64,
    sha256: String,
}
#[derive(Default)]
struct CheckedBytes {
    hash: Sha256,
    count: u64,
}
impl Write for CheckedBytes {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.hash.update(data);
        self.count += data.len() as u64;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if std::env::var("TAPE_RS_PERF_COMPARE").as_deref() != Ok("PF2701L08-PF2702L08") {
        return Err("缺少专用测试介质门控，拒绝访问设备".into());
    }
    if !matches!(args.mode.as_str(), "write" | "read" | "list") {
        return Err("mode 必须为 write/read/list".into());
    }
    let value: serde_json::Value = serde_json::from_reader(std::fs::File::open(&args.manifest)?)?;
    let manifest = Manifest {
        barcode: value["barcode"]
            .as_str()
            .ok_or("清单缺少 barcode")?
            .to_owned(),
        volume_uuid: value["volume_uuid"]
            .as_str()
            .ok_or("清单缺少 UUID")?
            .parse()?,
        files: value["files"]
            .as_array()
            .ok_or("清单缺少 files")?
            .iter()
            .map(|v| {
                Ok::<_, Box<dyn std::error::Error>>(Entry {
                    path: v["path"].as_str().ok_or("缺少 path")?.to_owned(),
                    group: v["group"].as_str().ok_or("缺少 group")?.to_owned(),
                    size: v["size"].as_u64().ok_or("缺少 size")?,
                    sha256: v["sha256"].as_str().ok_or("缺少 sha256")?.to_owned(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if !matches!(manifest.barcode.as_str(), "PF2701L08" | "PF2702L08") {
        return Err("不是专用性能测试介质".into());
    }
    let rows: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| {
            (args.group == "all" || f.group == args.group)
                && args.path.as_ref().is_none_or(|p| p == &f.path)
        })
        .collect();
    if rows.is_empty()
        || rows.iter().any(|f| {
            !f.path.starts_with("comparison/")
                || f.path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
        })
    {
        return Err("清单路径或分组非法".into());
    }
    let dev = ScsiDevice::open(&args.device)?;
    if read_unit_serial(&dev).as_deref() != Some("IBMlisa42299") {
        return Err("驱动器序列号不匹配，拒绝访问介质".into());
    }
    let start = Instant::now();
    let mut vol = LtfsVolume::mount(&dev)?;
    if vol.label().volume_uuid != manifest.volume_uuid {
        return Err("介质 UUID 不匹配，拒绝继续".into());
    }
    let mount_seconds = start.elapsed().as_secs_f64();
    match args.mode.as_str() {
        "write" => {
            if !vol.writable() {
                return Err("卷不可写".into());
            }
            // 先验证全部源文件；校验时间不计入设备写入阶段。
            for row in &rows {
                if vol.index().find_file(&row.path).is_some() {
                    return Err("路径已存在，拒绝覆盖".into());
                }
                let mut source = std::fs::File::open(args.source.join(&row.path))?;
                let mut hash = Sha256::new();
                let mut buffer = [0u8; 64 * 1024];
                let mut count = 0;
                loop {
                    let n = source.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    count += n as u64;
                    hash.update(&buffer[..n]);
                }
                if count != row.size || format!("{:x}", hash.finalize()) != row.sha256 {
                    return Err("源文件摘要不匹配".into());
                }
            }
            let start = Instant::now();
            let mut mixed_reads = 0;
            for (i, row) in rows.iter().enumerate() {
                let mut source =
                    std::io::BufReader::new(std::fs::File::open(args.source.join(&row.path))?);
                vol.append_file(&row.path, &mut source)?;
                if args.group == "mixed" && (i + 1) % 16 == 0 {
                    let large: Vec<_> = manifest
                        .files
                        .iter()
                        .filter(|f| f.group == "large")
                        .collect();
                    if large.len() != 4 {
                        return Err("混合负载缺少4个大文件".into());
                    }
                    for _ in 0..3 {
                        let input = large[mixed_reads % 4];
                        let mut out = CheckedBytes::default();
                        vol.read_file_to_writer(&input.path, &mut out)?;
                        if out.count != input.size
                            || format!("{:x}", out.hash.finalize()) != input.sha256
                        {
                            return Err("混合读摘要不匹配".into());
                        }
                        mixed_reads += 1;
                    }
                }
            }
            let data_seconds = start.elapsed().as_secs_f64();
            let commit = Instant::now();
            vol.commit()?;
            let commit_seconds = commit.elapsed().as_secs_f64();
            println!(
                "{}",
                json!({"interface":"rust-library","kind":"write","group":args.group,"count":rows.len(),"bytes":rows.iter().map(|f|f.size).sum::<u64>(),"mount_seconds":mount_seconds,"mixed_reads":mixed_reads,"data_seconds":data_seconds,"commit_seconds":commit_seconds,"total_seconds":start.elapsed().as_secs_f64(),"generation":vol.index().generation})
            );
        }
        "read" => {
            for label in if args.once {
                &["first"][..]
            } else {
                &["first", "repeat"][..]
            } {
                let start = Instant::now();
                for row in &rows {
                    let mut out = CheckedBytes::default();
                    vol.read_file_to_writer(&row.path, &mut out)?;
                    if out.count != row.size || format!("{:x}", out.hash.finalize()) != row.sha256 {
                        return Err("磁带内容摘要不匹配".into());
                    }
                }
                println!(
                    "{}",
                    json!({"interface":"rust-library","kind":"read","group":args.group,"label":label,"count":rows.len(),"bytes":rows.iter().map(|f|f.size).sum::<u64>(),"mount_seconds":mount_seconds,"seconds":start.elapsed().as_secs_f64(),"verified":true})
                );
            }
        }
        "list" => {
            let prefix = format!("comparison/{}", args.group);
            let mut samples = Vec::new();
            let mut count = 0;
            for _ in 0..10 {
                let start = Instant::now();
                let dir = vol.index().find_directory(&prefix).ok_or("目录不存在")?;
                let names: Vec<_> = dir
                    .files
                    .iter()
                    .map(|f| f.name.clone())
                    .chain(dir.subdirs.iter().map(|d| d.name.clone()))
                    .collect();
                count = std::hint::black_box(names).len();
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!(
                "{}",
                json!({"interface":"rust-library","kind":"list","group":args.group,"count":count,"mount_seconds":mount_seconds,"samples_ms":samples})
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}
