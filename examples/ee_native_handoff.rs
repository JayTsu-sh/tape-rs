//! EE 中间态交接探针；必须先由操作者停止所有使用目标库/驱动器的服务。
//! 仅追加固定的新原生路径，或执行显式 checkpoint；不格式化、擦除或删除。
//! --manifest 必须是新路径：JSONL 每个阶段 write + sync_all，一次性使用。
//! 出错即停，保留现场/预留；禁止抢占，不自动归槽或修复。只编译不会接触设备。
use std::cell::Cell;
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tape_rs::changer::commands::MediumChanger;
use tape_rs::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::index::{FileNode, LtfsIndex};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::device::{ScsiDevice, ScsiResult};
use tape_rs::scsi::inquiry::{read_unit_serial, standard_inquiry};
use tape_rs::scsi::reservation::{self, FenceOutcome, ReservationKey};
use tape_rs::scsi::transport::TapeTransport;
use tape_rs::tape::commands::TapeDrive;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Sync,
    Incremental,
    Checkpoint,
    /// 强制核验历史链；不调用任何提交或追加接口。
    Audit,
}

#[derive(Debug, Parser)]
#[command(about = "独占 EE 测试卷，保留 DP sync/增量中间态并归槽；不自动清洁卸载")]
struct Args {
    #[arg(long)]
    changer: String,
    #[arg(long)]
    changer_serial: String,
    #[arg(long)]
    drive: String,
    #[arg(long)]
    drive_serial: String,
    #[arg(long)]
    barcode: String,
    #[arg(long)]
    uuid: Uuid,
    /// 1..=2^48-1；使用节点号 201 的独立探针预留键。
    #[arg(long)]
    round: u64,
    #[arg(long, value_enum)]
    mode: Mode,
    /// 审计时逐文件读取并核对带上SHA/MD5；不执行任何追加或提交。
    #[arg(long)]
    verify_data: bool,
    /// 不得已存在；父目录必须存在。追加式、逐阶段同步的 JSONL 证据。
    #[arg(long)]
    manifest: PathBuf,
}

fn refuse(reason: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: reason.into(),
    }
}

fn require(ok: bool, reason: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(refuse(reason)) }
}

// 共享失权闩锁阻止底层 UA 重试层在失权后继续命令；也禁止 fence 的隐式抢占分支。
struct ProbeDevice {
    device: ScsiDevice,
    lost: Rc<Cell<bool>>,
}

impl ProbeDevice {
    fn command<T>(&self, cdb: &[u8], call: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.lost.get() {
            return Err(TapeError::OwnershipLost {
                device: self.identity().into(),
                detail: "探针已观察到失权，禁止后续设备命令".into(),
            });
        }
        if cdb.first() == Some(&opcode::PERSISTENT_RESERVE_OUT)
            && cdb
                .get(1)
                .is_some_and(|sa| matches!(sa & 0x1f, 0x04 | 0x05))
        {
            return Err(refuse("探针禁止 PREEMPT/PREEMPT_ABORT；保留现场"));
        }
        let result = call();
        if result.as_ref().is_err_and(reservation::ownership_lost) {
            self.lost.set(true);
        }
        result
    }
}

impl TapeTransport for ProbeDevice {
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.command(cdb, || self.device.execute_no_data(cdb, timeout_ms))
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.command(cdb, || self.device.execute_read(cdb, buf, timeout_ms))
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        // 注册前再核验空注册表；与外部控制器并发时也不覆盖其现存注册。
        if cdb.first() == Some(&opcode::PERSISTENT_RESERVE_OUT)
            && cdb.get(1).is_some_and(|sa| sa & 0x1f == 0x06)
            && buf
                .get(8..16)
                .is_some_and(|key| key.iter().any(|&b| b != 0))
        {
            require_no_pr(self)?;
        }
        self.command(cdb, || self.device.execute_write(cdb, buf, timeout_ms))
    }
    fn identity(&self) -> &str {
        self.device.path()
    }
}

fn event(file: &mut File, phase: &str, data: Value) -> Result<()> {
    serde_json::to_writer(
        &mut *file,
        &json!({
            "phase": phase, "observed_at": chrono::Utc::now().to_rfc3339(), "data": data,
        }),
    )
    .map_err(std::io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn require_no_pr(dev: &dyn TapeTransport) -> Result<()> {
    let status = reservation::read_status(dev)?;
    require(
        status.holder.is_none() && status.keys.is_empty(),
        &format!("{} 存在预留/注册：{status:?}", dev.identity()),
    )
}

fn acquire(dev: &dyn TapeTransport, key: ReservationKey) -> Result<()> {
    require_no_pr(dev)?;
    match reservation::fence(dev, key)? {
        FenceOutcome::Fenced { before, .. }
            if before.holder.is_none() && before.keys.is_empty() =>
        {
            Ok(())
        }
        other => Err(refuse(format!("未获得空闲设备的独占预留：{other:?}"))),
    }
}

fn verify_both(
    changer: &dyn TapeTransport,
    drive: &dyn TapeTransport,
    key: ReservationKey,
) -> Result<()> {
    reservation::verify_holder(changer, key)?;
    reservation::verify_holder(drive, key)
}

fn inventory_json(items: &[ElementStatus]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|e| {
                json!({
                    "address": e.address, "type": format!("{:?}", e.element_type),
                    "full": e.full, "barcode": e.volume_tag, "source_address": e.source_address,
                    "drive_id": e.drive_id,
                })
            })
            .collect(),
    )
}

fn targets(items: &[ElementStatus], map: &ElementAddressMap, args: &Args) -> Result<(u16, u16)> {
    let homes: Vec<_> = items
        .iter()
        .filter(|e| e.full && e.volume_tag.as_deref() == Some(&args.barcode))
        .collect();
    require(homes.len() == 1, "目标条码必须在库存中唯一出现")?;
    let home = homes[0];
    require(
        home.element_type == ElementType::Storage
            && (u32::from(map.storage_start)
                ..u32::from(map.storage_start) + u32::from(map.storage_count))
                .contains(&u32::from(home.address)),
        "目标条码必须位于 map 范围内的存储槽位",
    )?;
    let drives: Vec<_> = items
        .iter()
        .filter(|e| {
            e.element_type == ElementType::DataTransfer
                && e.drive_id
                    .as_deref()
                    .is_some_and(|id| id.split_ascii_whitespace().any(|s| s == args.drive_serial))
        })
        .collect();
    require(
        drives.len() == 1,
        "DVCID 必须精确分量匹配唯一目标驱动序列号",
    )?;
    let drive = drives[0];
    require(
        !drive.full
            && (u32::from(map.dt_start)..u32::from(map.dt_start) + u32::from(map.dt_count))
                .contains(&u32::from(drive.address)),
        "目标驱动必须为空且位于 map DTE 范围内",
    )?;
    Ok((home.address, drive.address))
}

fn verify_location(items: &[ElementStatus], barcode: &str, full: u16, empty: u16) -> Result<()> {
    require(
        items
            .iter()
            .filter(|e| e.full && e.volume_tag.as_deref() == Some(barcode))
            .count()
            == 1
            && items
                .iter()
                .any(|e| e.address == full && e.full && e.volume_tag.as_deref() == Some(barcode))
            && items.iter().any(|e| e.address == empty && !e.full),
        "移动前后条码或槽位/驱动状态不符",
    )
}

fn original_files(index: &LtfsIndex) -> Vec<(String, FileNode)> {
    let mut files = Vec::new();
    index.walk_files(|path, file| files.push((path.to_string(), file.clone())));
    files
}

fn check_originals(index: &LtfsIndex, original: &[(String, FileNode)]) -> Result<()> {
    for (path, file) in original {
        require(
            index.find_file(path) == Some(file),
            &format!("原文件元素发生变化：{path}"),
        )?;
    }
    Ok(())
}

fn snapshot(vol: &LtfsVolume<'_>, recovery_is_fresh: bool) -> Value {
    let index = vol.index();
    json!({
        "uuid": vol.label().volume_uuid.to_string(), "generation": index.generation,
        "incremental": index.incremental, "self_location": format!("{:?}", index.self_location),
        "previous_location": format!("{:?}", index.previous_location),
        "previous_incremental_location": format!("{:?}", index.previous_incremental_location),
        "needs_checkpoint": vol.needs_checkpoint(), "writable": vol.writable(),
        "recovery_is_fresh_mount": recovery_is_fresh,
        "index_debug": format!("{index:#?}"),
        "recovery_debug": format!("{:#?}", vol.recovery()),
        "file_elements": original_files(index).iter().map(|(path, node)| json!({
            "path": path, "element_debug": format!("{node:#?}"),
        })).collect::<Vec<_>>(),
    })
}

// 保存实际带上 XML，而非把已重放内存视图误当作原始增量。证据读取限 64 MiB。
fn capture_indexes(
    drive: &TapeDrive<'_>,
    vol: &LtfsVolume<'_>,
    manifest: &mut File,
    phase: &str,
) -> Result<()> {
    let mut buffer = vec![0; vol.block_size() as usize];
    for scan in [&vol.recovery().ip, &vol.recovery().dp] {
        let candidate = scan
            .last_index
            .as_ref()
            .ok_or_else(|| refuse("缺少可取证的分区末索引"))?;
        drive.locate(scan.partition, candidate.start_block, true)?;
        let mut xml = Vec::new();
        loop {
            let n = drive.read_block(&mut buffer)?;
            if n == 0 {
                break;
            }
            require(
                xml.len() + n <= 64 * 1024 * 1024,
                "原始索引证据超过 64 MiB 预算",
            )?;
            xml.extend_from_slice(&buffer[..n]);
        }
        let raw = LtfsIndex::parse(&xml)?;
        require(
            raw.volume_uuid == vol.label().volume_uuid
                && raw.generation == candidate.index.generation
                && raw.self_location == candidate.index.self_location,
            "带上原始索引与恢复候选不符",
        )?;
        event(
            manifest,
            phase,
            json!({"physical_partition": scan.partition, "start_block": candidate.start_block,
            "generation": raw.generation, "incremental": raw.incremental,
            "sha256": format!("{:x}", Sha256::digest(&xml)),
            "xml": std::str::from_utf8(&xml).map_err(|e| TapeError::Ltfs(e.to_string()))?}),
        )?;
    }
    Ok(())
}

fn run(args: &Args, manifest: &mut File) -> Result<()> {
    require(
        !args.verify_data || args.mode == Mode::Audit,
        "逐文件校验仅允许audit模式",
    )?;
    let lost = Rc::new(Cell::new(false));
    let changer_dev = ProbeDevice {
        device: ScsiDevice::open(&args.changer)?,
        lost: lost.clone(),
    };
    let drive_dev = ProbeDevice {
        device: ScsiDevice::open(&args.drive)?,
        lost,
    };
    require(
        standard_inquiry(&changer_dev)?.peripheral_type == 8,
        "目标 changer 不是介质换带器",
    )?;
    require(
        read_unit_serial(&changer_dev).as_deref() == Some(args.changer_serial.as_str()),
        "changer VPD 序列号不符或读取失败",
    )?;
    require(
        standard_inquiry(&drive_dev)?.peripheral_type == 1,
        "目标 drive 不是顺序存取设备",
    )?;
    require(
        read_unit_serial(&drive_dev).as_deref() == Some(args.drive_serial.as_str()),
        "drive VPD 序列号不符或读取失败",
    )?;
    require_no_pr(&changer_dev)?;
    require_no_pr(&drive_dev)?;
    let mut changer = MediumChanger::new(&changer_dev);
    let map = changer.load_address_map()?.clone();
    let initial = changer.read_all_status()?;
    let (home, dte) = targets(&initial, &map, args)?;
    event(
        manifest,
        "preflight",
        json!({"home": home, "dte": dte,
        "map": format!("{map:?}"), "inventory": inventory_json(&initial),
        "changer_pr": format!("{:?}", reservation::read_status(&changer_dev)?),
        "drive_pr": format!("{:?}", reservation::read_status(&drive_dev)?)}),
    )?;
    let key = ReservationKey::new(201, args.round);
    acquire(&changer_dev, key)?;
    event(
        manifest,
        "changer_fenced",
        json!({"key": format!("{:016x}", key.0)}),
    )?;
    acquire(&drive_dev, key)?;
    verify_both(&changer_dev, &drive_dev, key)?;
    let current = changer.read_all_status()?;
    require(
        targets(&current, &map, args)? == (home, dte),
        "注册预留后库存改变",
    )?;
    event(manifest, "before_load", json!({"home": home, "dte": dte}))?;
    changer.move_medium(home, dte)?;
    let drive = TapeDrive::new(&drive_dev);
    verify_both(&changer_dev, &drive_dev, key)?;
    drive.load()?; // 保留 TapeDrive 的两小时 LTO-9 LOAD 超时。
    let ready_deadline = Instant::now() + Duration::from_secs(7_200);
    while !drive.test_unit_ready()? {
        require(
            Instant::now() < ready_deadline,
            "等待磁带就绪超时；保留现场",
        )?;
        verify_both(&changer_dev, &drive_dev, key)?;
        std::thread::sleep(Duration::from_secs(2));
    }
    verify_both(&changer_dev, &drive_dev, key)?;
    verify_location(&changer.read_all_status()?, &args.barcode, dte, home)?;
    let mut vol = if args.mode == Mode::Audit {
        LtfsVolume::mount_with_budget(&drive_dev, &Default::default())?
    } else {
        LtfsVolume::mount(&drive_dev)?
    };
    require(
        vol.label().volume_uuid == args.uuid,
        "实际卷 UUID 不符；不写入并保留现场",
    )?;
    event(manifest, "before_mutation", snapshot(&vol, true))?;
    require(vol.writable(), "卷不是可安全写入状态")?;
    capture_indexes(&drive, &vol, manifest, "before_raw_index")?;
    let original = original_files(vol.index());
    let path = match args.mode {
        Mode::Sync => Some("interop-completion-20261016/sync.bin"),
        Mode::Incremental => Some("interop-completion-20261016/incremental.bin"),
        Mode::Checkpoint | Mode::Audit => None,
    };
    let payload: Vec<u8> = (0..700 * 1024usize)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add((i >> 8) as u8))
        .collect();
    let expected_sha = format!("{:x}", Sha256::digest(&payload));
    if let Some(path) = path {
        require(
            vol.index()
                .find_file("interop-completion-20261016")
                .is_none(),
            "测试父路径是文件或链接，拒绝创建同名目录",
        )?;
        require(
            vol.index().find_file(path).is_none() && vol.index().find_directory(path).is_none(),
            "测试新路径已存在，拒绝覆盖",
        )?;
        if args.mode == Mode::Incremental {
            require(
                vol.index().self_location.partition == vol.label().data_partition
                    && vol.recovery().incremental_depth == 0
                    && !vol.index().incremental,
                "增量实验要求 DP Full 基线；先显式 checkpoint，避免 commit_incremental 回退 Full",
            )?;
        }
    }
    let initial_incremental = vol.index().incremental;
    let initial_debt = vol.needs_checkpoint();
    verify_both(&changer_dev, &drive_dev, key)?;
    vol.set_reservation_guard(Some(key));
    event(
        manifest,
        "mutation_intent",
        json!({"mode": format!("{:?}", args.mode), "path": path,
        "bytes": path.map(|_| payload.len()), "sha256": path.map(|_| &expected_sha)}),
    )?;
    if let Some(path) = path {
        require(
            vol.append_file(path, &mut Cursor::new(&payload))? == payload.len() as u64,
            "追加长度不符",
        )?;
    }
    match args.mode {
        Mode::Sync => vol.sync()?,
        Mode::Incremental => vol.commit_incremental()?,
        Mode::Checkpoint => vol.commit()?,
        Mode::Audit => {}
    }
    event(manifest, "after_commit", snapshot(&vol, false))?;
    check_originals(vol.index(), &original)?;
    let generation = vol.index().generation;
    drop(vol); // 不调用 unmount()：不能把 DP 中间态变成双分区 Full。
    verify_both(&changer_dev, &drive_dev, key)?;
    let fresh = if args.mode == Mode::Audit {
        LtfsVolume::mount_with_budget(&drive_dev, &Default::default())?
    } else {
        LtfsVolume::mount(&drive_dev)?
    };
    event(manifest, "fresh_mount", snapshot(&fresh, true))?;
    require(
        fresh.label().volume_uuid == args.uuid && fresh.index().generation == generation,
        "重挂卷身份或代数不符",
    )?;
    check_originals(fresh.index(), &original)?;
    if args.verify_data {
        for (path, file) in &original {
            let verdict = fresh.verify_file(path)?;
            event(
                manifest,
                "data_verified",
                json!({"path":path,"bytes":file.length,"verdict":format!("{verdict:?}")}),
            )?;
            require(
                matches!(verdict, tape_rs::ltfs::volume::HashVerdict::Match { .. }),
                format!("审计文件没有匹配的内容哈希：{path}").as_str(),
            )?;
        }
    }
    require(
        fresh.index().incremental
            == if args.mode == Mode::Audit {
                initial_incremental
            } else {
                args.mode == Mode::Incremental
            },
        "实际索引类型不符，实验未成立",
    )?;
    require(
        fresh.needs_checkpoint()
            == if args.mode == Mode::Audit {
                initial_debt
            } else {
                args.mode != Mode::Checkpoint
            },
        "实际 IP 检查点状态不符，实验未成立",
    )?;
    if let Some(path) = path {
        let mut readback = Vec::new();
        let count = fresh.read_file_to_writer(path, &mut readback)?;
        let actual_sha = format!("{:x}", Sha256::digest(&readback));
        event(
            manifest,
            "readback",
            json!({"path": path, "bytes": count, "sha256": actual_sha,
            "expected_sha256": expected_sha, "original_file_elements_unchanged": original.len()}),
        )?;
        require(
            count == payload.len() as u64 && actual_sha == expected_sha,
            "独立重挂读回 SHA256/长度不符",
        )?;
    }
    capture_indexes(&drive, &fresh, manifest, "after_raw_index")?;
    drop(fresh); // 同上，不清洁卸载。
    verify_both(&changer_dev, &drive_dev, key)?;
    verify_location(&changer.read_all_status()?, &args.barcode, dte, home)?;
    event(manifest, "before_return", json!({"home": home, "dte": dte}))?;
    drive.unload()?;
    verify_both(&changer_dev, &drive_dev, key)?;
    verify_location(&changer.read_all_status()?, &args.barcode, dte, home)?;
    changer.move_medium(dte, home)?;
    let final_inventory = changer.read_all_status()?;
    verify_location(&final_inventory, &args.barcode, home, dte)?;
    event(manifest, "returned_home", inventory_json(&final_inventory))?;
    verify_both(&changer_dev, &drive_dev, key)?;
    reservation::release_and_unregister(&drive_dev, key)?;
    reservation::verify_holder(&changer_dev, key)?;
    reservation::release_and_unregister(&changer_dev, key)?;
    require_no_pr(&drive_dev)?;
    require_no_pr(&changer_dev)?;
    event(
        manifest,
        "complete",
        json!({"generation": generation, "returned_home": true,
        "pr_released": true, "original_file_elements_unchanged": original.len()}),
    )
}

fn main() -> std::process::ExitCode {
    env_logger::init();
    let args = Args::parse();
    let validate = require(
        args.round > 0
            && args.round <= 0xffff_ffff_ffff
            && !args.barcode.is_empty()
            && args.barcode.trim() == args.barcode
            && !args.changer_serial.is_empty()
            && !args.drive_serial.is_empty()
            && args.changer != args.drive,
        "必须显式指定有效轮次、条码及独立设备/序列号",
    );
    let result = validate.and_then(|()| {
        let mut manifest = File::create_new(&args.manifest)?;
        event(&mut manifest, "intent", json!({"args": format!("{args:#?}"), "mode": format!("{:?}", args.mode),
            "barcode": args.barcode, "uuid": args.uuid.to_string(), "round": args.round, "node": 201}))?;
        // 父目录项也同步，保证副作用前的一次性门控已持久化。
        File::open(args.manifest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")))?.sync_all()?;
        eprintln!("将独占目标库与驱动器，移动 {} 并执行 {:?}；证据 {:?}", args.barcode, args.mode, args.manifest);
        let result = run(&args, &mut manifest);
        if let Err(error) = &result
            && let Err(record_error) = event(&mut manifest, "error", json!({"error": error.to_string(),
                "error_debug": format!("{error:?}"), "automatic_cleanup_attempted": false}))
        {
            eprintln!("记录错误失败：{record_error}");
        }
        result
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("探针停止：{error:?}；不执行额外机械清理，请按 manifest 核对现场");
            std::process::ExitCode::FAILURE
        }
    }
}
