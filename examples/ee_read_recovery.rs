//! 已装载研究卷的只读取证；不注册/抢占/释放 PR，不写带、不卸载。
//! 必须由操作者确认当前介质与持有轮次；证据目录必须不存在。
use clap::Parser;
use std::io::Write;
use std::path::PathBuf;
use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::{
    index::LtfsIndex,
    label::LtfsLabel,
    recovery::{RecoveryBudget, recover},
};
use tape_rs::scsi::{
    device::ScsiDevice,
    inquiry::read_unit_serial,
    reservation::{self, ReservationKey},
};
use tape_rs::tape::commands::TapeDrive;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    drive: String,
    #[arg(long)]
    serial: String,
    #[arg(long)]
    round: u64,
    #[arg(long)]
    uuid: uuid::Uuid,
    #[arg(long)]
    evidence_dir: PathBuf,
    /// 直接取证的已知 DP 索引块，可重复；来自已保留的实际写入阶段记录。
    #[arg(long)]
    block: Vec<u64>,
}
fn run(a: Args) -> Result<()> {
    let dev = ScsiDevice::open(&a.drive)?;
    if read_unit_serial(&dev).as_deref() != Some(&a.serial) {
        return Err(TapeError::Ltfs("驱动序列号不符".into()));
    }
    let key = ReservationKey::new(201, a.round);
    reservation::verify_holder(&dev, key)?;
    std::fs::create_dir(&a.evidence_dir)?;
    let drive = TapeDrive::new(&dev);
    drive.locate(0, 2, true)?;
    let mut buf = vec![0; 64 << 10];
    let n = drive.read_block(&mut buf)?;
    let label = LtfsLabel::parse(&buf[..n])?;
    if label.volume_uuid != a.uuid {
        return Err(TapeError::Ltfs("卷 UUID 不符".into()));
    }
    let report = recover(&dev, &label, &RecoveryBudget::default())?;
    println!(
        "{}",
        serde_json::json!({"chain_ok":report.chain_ok,"notes":report.notes,
        "restricted":report.restricted_reason(),"dp_generation":report.dp.last_index.as_ref().map(|c|c.index.generation),
        "ip_generation":report.ip.last_index.as_ref().map(|c|c.index.generation)})
    );
    buf.resize(label.blocksize as usize, 0);
    for block in a.block {
        reservation::verify_holder(&dev, key)?;
        drive.locate(1, block, true)?;
        let mut xml = Vec::new();
        loop {
            reservation::verify_holder(&dev, key)?;
            let n = drive.read_block(&mut buf)?;
            if n == 0 {
                break;
            }
            if xml.len() + n > 64 << 20 {
                return Err(TapeError::Ltfs("索引证据超过64MiB".into()));
            }
            xml.extend_from_slice(&buf[..n]);
        }
        let index = LtfsIndex::parse(&xml)?;
        if index.volume_uuid != a.uuid || index.self_location.start_block != block {
            return Err(TapeError::Ltfs("原始索引身份/自指针不符".into()));
        }
        let path = a.evidence_dir.join(format!("dp-{block}.xml"));
        let mut file = std::fs::File::create_new(&path)?;
        file.write_all(&xml)?;
        file.sync_all()?;
        println!(
            "{}",
            serde_json::json!({"block":block,"generation":index.generation,
            "incremental":index.incremental,"bytes":xml.len(),"evidence":path})
        );
    }
    reservation::verify_holder(&dev, key)?;
    Ok(())
}
fn main() -> Result<()> {
    run(Args::parse())
}
