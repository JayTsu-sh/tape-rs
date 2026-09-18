//! 设备执行线程。只有它接触磁带设备；Raft 循环通过消息驱动它，它把结论发回去。
//!
//! 纪律：抢占预留只发生在收到 `Takeover` 时；持有期间一旦发现失去资格就停下并上报，
//! 绝不自行重新预留。是否再次接管由 Raft 层在重新当选、取得新轮次之后决定。

use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use log::{error, info, warn};

use super::files::{FileService, TapeIdent, TapeLimits};
use super::state::tape_state;
use super::node::SharedStatus;
use super::state::FileRec;
use crate::changer::commands::MediumChanger;
use crate::changer::element::ElementType;
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::TailKind;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::{FormatProbe, LtfsVolume, TailPolicy, XATTR_POOL_NAME, XATTR_POOL_UUID, probe_format};
use crate::tape::commands::TapeDrive;
use crate::scsi::device::ScsiDevice;
use crate::scsi::inquiry::{enumerate_sg_nodes, read_unit_serial};
use crate::scsi::reservation::{self, FenceOutcome, ReservationKey, ownership_lost};
use crate::scsi::transport::TapeTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Changer,
    Drive,
}

pub struct ManagedDevice {
    pub name: String,
    /// VPD 0x80 序列号。换带器用它把驱动器元素和设备对应起来
    pub serial: String,
    pub kind: DeviceKind,
    pub dev: Box<dyn TapeTransport + Send>,
}

/// 打开受管逻辑库的全部设备。每次接管重新打开一遍。
pub trait DeviceProvider: Send {
    fn open_all(&self) -> Result<Vec<ManagedDevice>>;
}

/// 真实设备：按 VPD 0x80 序列号在 `/dev/sg*` 里定位，不依赖会变化的设备名。
pub struct SgProvider {
    pub changer_serial: Option<String>,
    pub drive_serials: Vec<String>,
}

impl DeviceProvider for SgProvider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let mut wanted: Vec<(String, DeviceKind)> =
            self.drive_serials.iter().map(|s| (s.clone(), DeviceKind::Drive)).collect();
        if let Some(c) = &self.changer_serial {
            wanted.push((c.clone(), DeviceKind::Changer));
        }
        let mut found = Vec::new();
        for path in enumerate_sg_nodes()? {
            let Some(p) = path.to_str() else { continue };
            // 被别的进程独占的设备（例如 IBM LTFS 自己的驱动器）会立即返回忙，跳过
            let Ok(dev) = ScsiDevice::open(p) else { continue };
            let Some(serial) = read_unit_serial(&dev) else { continue };
            if let Some(i) = wanted.iter().position(|(s, _)| *s == serial) {
                let (s, kind) = wanted.remove(i);
                found.push(ManagedDevice { name: format!("{} ({})", s, p), serial: s, kind, dev: Box::new(dev) });
            }
        }
        if let Some((s, _)) = wanted.first() {
            return Err(TapeError::NotReady(format!("找不到序列号为 {} 的设备", s)));
        }
        Ok(found)
    }
}

#[derive(Debug)]
pub enum ExecRequest {
    /// 本节点成为第 `round` 轮的执行者：隔离全部设备并回读确认。
    Takeover { round: u64 },
    /// `Fenced` 已被多数派确认：恢复卷、需要时收尾，然后开始服务。
    Recover { round: u64 },
    /// 不再是执行者（换届、作废）：立即停手，不做任何设备操作。
    /// `round` 指明要停的是哪一轮；执行线程若已在处理更新的一轮则忽略。`None` 表示无条件停。
    Stop { round: Option<u64>, reason: String },
    /// 文件服务有已完成的上传等待落带。
    Work,
    /// 读出一个已提交文件的内容。
    Read { path: String, reply: Sender<std::result::Result<Vec<u8>, ReadError>> },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// 此刻读不了，稍后重试即可：只有一个驱动器且写入侧正忙，或换带尚未完成。
    Busy(String),
    Failed(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Busy(s) => write!(f, "稍后重试: {}", s),
            ReadError::Failed(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    Fenced { round: u64 },
    FenceFailed { round: u64, reason: String },
    Serving { round: u64, summary: String },
    /// 磁带状态变化（写满、到文件数上限、池标记不符、装载或格式化失败）。
    TapeState { round: u64, barcode: String, state: String },
    /// 一次卷提交已经落带（或接管时读到了该带的完整列表，`full` 为真）。
    /// Raft 层据此把目录记录写进日志。
    Committed { round: u64, barcode: String, volume_uuid: String, generation: u64, files_total: u64, bytes_used: u64, files: Vec<FileRec>, full: bool },
    /// 持有期间失去资格（预留被抢占、设备复位、自检失败）。执行线程已停手。
    Lost { round: u64, reason: String },
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub node_id: u8,
    /// 服务期间每隔多久做一次工作：演示写入，或只做持有者自检。
    pub interval: Duration,
    /// 为真时周期性向卷里追加一个小文件，用来在故障切换时制造真实的写入活动。
    pub demo_write: bool,
    pub salvage: bool,
    /// 自动格式化空白带时用的块大小
    pub block_size: u32,
    /// 为读请求装载的带空闲这么久后卸回槽位
    pub read_idle: Duration,
}

struct Active {
    round: u64,
    key: ReservationKey,
    devices: Vec<ManagedDevice>,
    serving: bool,
    /// 正在服务的驱动器在 `devices` 里的下标
    drive: Option<usize>,
    /// 为读请求装载的带：驱动器下标、条码、最近一次使用
    read: Option<(usize, String, Instant)>,
    seq: u64,
}

pub fn run(
    provider: Box<dyn DeviceProvider>,
    opts: ExecOptions,
    rx: Receiver<ExecRequest>,
    tx: Sender<ExecEvent>,
    files: Arc<FileService>,
    status: SharedStatus,
) {
    let mut active: Option<Active> = None;
    let mut next_work = Instant::now() + opts.interval;
    loop {
        let mut wait = next_work.saturating_duration_since(Instant::now());
        // 有一批上传快到期时提前醒来
        if let Some(due) = active.as_ref().filter(|a| a.serving).and_then(|a| files.due_in(a.round)) {
            wait = wait.min(due);
        }
        match rx.recv_timeout(wait) {
            Ok(ExecRequest::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(ExecRequest::Stop { round, reason }) => {
                // 停手请求可能晚于更新一轮的接管请求到达：只停它指明的那一轮
                if round.is_some() && active.as_ref().map(|a| a.round) != round {
                    continue;
                }
                files.close(&reason);
                if let Some(a) = active.take() {
                    info!("执行线程: 停手（轮次 {}）: {}", a.round, reason);
                }
            }
            Ok(ExecRequest::Takeover { round }) => {
                files.close("正在接管");
                active = None;
                match takeover(provider.as_ref(), &opts, round) {
                    Ok(a) => {
                        active = Some(a);
                        let _ = tx.send(ExecEvent::Fenced { round });
                    }
                    Err(e) => {
                        warn!("执行线程: 轮次 {} 隔离失败: {}", round, e);
                        let _ = tx.send(ExecEvent::FenceFailed { round, reason: e.to_string() });
                    }
                }
            }
            Ok(ExecRequest::Recover { round }) => {
                let Some(a) = active.as_mut().filter(|a| a.round == round) else {
                    // 不能悄悄忽略：Raft 层正等着这一轮的结论
                    let reason = "执行线程没有该轮次的隔离状态".to_string();
                    warn!("执行线程: 轮次 {} 的恢复请求无法执行: {}", round, reason);
                    let _ = tx.send(ExecEvent::Lost { round, reason });
                    continue;
                };
                match ensure_write_tape(a, &opts, &files, &status, &tx) {
                    Ok(summary) => {
                        a.serving = true;
                        next_work = Instant::now() + opts.interval;
                        let _ = tx.send(ExecEvent::Serving { round, summary });
                    }
                    Err(e) => {
                        let reason = format!("恢复失败: {}", e);
                        files.close(&reason);
                        active = None;
                        let _ = tx.send(ExecEvent::Lost { round, reason });
                    }
                }
            }
            Ok(ExecRequest::Work) => {
                let Some(a) = active.as_mut().filter(|a| a.serving) else {
                    continue;
                };
                let mut worked = process_uploads(a, &files, &tx, false);
                if let (Ok(()), Some(new_state)) = (&worked, files.switch_requested(a.round)) {
                    worked = switch_tape(a, &opts, &files, &status, &tx, new_state).map(|summary| {
                        let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
                    });
                }
                if let Err(reason) = worked {
                    let round = a.round;
                    error!("执行线程: 轮次 {} 落带失败，放弃本轮: {}", round, reason);
                    files.close(&reason);
                    active = None;
                    let _ = tx.send(ExecEvent::Lost { round, reason });
                }
            }
            Ok(ExecRequest::Read { path, reply }) => {
                let res = match active.as_mut().filter(|a| a.serving) {
                    Some(a) => read_any(a, &opts, &files, &status, &tx, &path),
                    None => Err(ReadError::Failed("本节点不在服务".to_string())),
                };
                let _ = reply.send(res);
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some(a) = active.as_mut().filter(|a| a.serving) else {
                    next_work = Instant::now() + opts.interval;
                    continue;
                };
                // 提前醒来只为落带；周期工作（自检、演示写入、重试选带）仍按原节奏
                let periodic_due = Instant::now() >= next_work;
                if periodic_due {
                    next_work = Instant::now() + opts.interval;
                }
                let mut worked = process_uploads(a, &files, &tx, false);
                if let (Ok(()), Some(new_state)) = (&worked, files.switch_requested(a.round)) {
                    worked = switch_tape(a, &opts, &files, &status, &tx, new_state).map(|summary| {
                        let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
                    });
                }
                if periodic_due && worked.is_ok() {
                    if a.drive.is_none() {
                        // 还没有可服务的带（例如装着的带刚被归入池）：再试一次
                        match ensure_write_tape(a, &opts, &files, &status, &tx) {
                            Ok(summary) if a.drive.is_some() => {
                                let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
                            }
                            Ok(_) => {}
                            Err(e) => worked = Err(e.to_string()),
                        }
                    }
                    worked = periodic(a, &opts);
                }
                if let Err(reason) = worked {
                    let round = a.round;
                    error!("执行线程: 轮次 {} 失去执行资格: {}", round, reason);
                    files.close(&reason);
                    active = None;
                    let _ = tx.send(ExecEvent::Lost { round, reason });
                }
            }
        }
    }
}

fn takeover(provider: &dyn DeviceProvider, opts: &ExecOptions, round: u64) -> Result<Active> {
    let key = ReservationKey::new(opts.node_id, round);
    let devices = provider.open_all()?;
    for d in &devices {
        match reservation::fence(d.dev.as_ref(), key)? {
            FenceOutcome::Fenced { before, .. } => {
                info!("执行线程: {} 已隔离（原持有者 {:x?}）", d.name, before.holder.map(|h| h.0));
            }
            FenceOutcome::Unsupported if d.kind == DeviceKind::Changer => {
                warn!("执行线程: 换带器 {} 不支持持久预留，降级为库存核对", d.name);
            }
            FenceOutcome::Unsupported => {
                return Err(TapeError::NotReady(format!("驱动器 {} 不支持持久预留", d.name)));
            }
            FenceOutcome::Superseded { holder } => {
                return Err(TapeError::NotReady(format!(
                    "{} 已被更新的轮次持有（节点 {} 轮次 {}）",
                    d.name,
                    holder.node(),
                    holder.round()
                )));
            }
            FenceOutcome::Unconfirmed { reason, .. } => {
                return Err(TapeError::NotReady(format!("{} 隔离未能确认: {}", d.name, reason)));
            }
        }
    }
    Ok(Active { round, key, devices, serving: false, drive: None, read: None, seq: 0 })
}

fn catalog_of(index: &crate::ltfs::index::LtfsIndex) -> (Vec<FileRec>, u64) {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    index.walk_files(|p, f| {
        bytes += f.length;
        files.push(FileRec {
            path: format!("/{}", p),
            length: f.length,
            sha256: f.xattr(crate::ltfs::index::XATTR_SHA256).unwrap_or("").to_string(),
        });
    });
    (files, bytes)
}

/// 换带器视角下的一个驱动器。
struct DriveSlot {
    /// 在 `Active::devices` 里的下标
    dev: usize,
    /// 换带器里的元素地址
    addr: u16,
    /// 里面装着的磁带与它原来的槽位
    loaded: Option<(String, Option<u16>)>,
}

struct Inventory {
    drives: Vec<DriveSlot>,
    /// 条码 → 存储槽地址（不含已在驱动器里的）
    slots: std::collections::BTreeMap<String, u16>,
    empty_slots: Vec<u16>,
}

fn inventory(devices: &[ManagedDevice]) -> Result<Inventory> {
    let changer = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .ok_or_else(|| TapeError::NotReady("逻辑库没有配置换带器".into()))?;
    let mut mc = MediumChanger::new(changer.dev.as_ref());
    mc.load_address_map()?;
    let elems = mc.read_all_status()?;
    let tag = |e: &crate::changer::element::ElementStatus| e.volume_tag.as_deref().map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
    let mut inv = Inventory { drives: Vec::new(), slots: Default::default(), empty_slots: Vec::new() };
    for e in &elems {
        match e.element_type {
            ElementType::DataTransfer => {
                let Some(id) = e.drive_id.as_deref().map(str::trim) else { continue };
                let hit = devices.iter().position(|d| d.kind == DeviceKind::Drive && (id.contains(&d.serial) || d.serial.contains(id)));
                if let Some(dev) = hit {
                    inv.drives.push(DriveSlot { dev, addr: e.address, loaded: if e.full { tag(e).map(|b| (b, e.source_address)) } else { None } });
                }
            }
            ElementType::Storage if e.full => {
                if let Some(b) = tag(e) {
                    inv.slots.insert(b, e.address);
                }
            }
            ElementType::Storage => inv.empty_slots.push(e.address),
            _ => {}
        }
    }
    Ok(inv)
}

fn move_medium(devices: &[ManagedDevice], from: u16, to: u16) -> Result<()> {
    let changer = devices.iter().find(|d| d.kind == DeviceKind::Changer).ok_or_else(|| TapeError::NotReady("没有换带器".into()))?;
    let mut mc = MediumChanger::new(changer.dev.as_ref());
    mc.load_address_map()?;
    mc.move_medium(from, to)
}

/// 把驱动器里的带卸回原槽位（原槽位被占则用第一个空槽）。
fn unload_drive(devices: &[ManagedDevice], inv: &Inventory, drive: &DriveSlot) -> Result<()> {
    let Some((barcode, home)) = &drive.loaded else { return Ok(()) };
    let dest = home.filter(|h| inv.empty_slots.contains(h)).or(inv.empty_slots.first().copied()).ok_or_else(|| TapeError::NotReady("没有空槽位可卸带".into()))?;
    // 先让驱动器退带；有的库要求这样，模拟设备上是空操作
    let _ = TapeDrive::new(devices[drive.dev].dev.as_ref()).unload();
    info!("执行线程: 卸带 {} -> 槽位 {:#06x}", barcode, dest);
    move_medium(devices, drive.addr, dest)
}

fn wait_ready(dev: &dyn TapeTransport) -> Result<()> {
    let drive = TapeDrive::new(dev);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match drive.test_unit_ready() {
            Ok(true) => return Ok(()),
            Ok(false) | Err(TapeError::ScsiCommand { sense_key: 0x02, .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(false) => return Err(TapeError::NotReady("装载后驱动器一直未就绪".into())),
            Err(e) => return Err(e),
        }
    }
}

struct Candidate {
    barcode: String,
    pool_uuid: String,
    pool_name: String,
    file_limit: u64,
    bytes_used: u64,
}

/// 选一盘可写的带并开始服务：优先已在驱动器里的，其次已用得最多的（写满一盘再开下一盘，
/// 而不是把数据摊到所有带上），再其次按条码。需要时装载；空白带首次使用时格式化并写池标记。
/// 未归属的磁带绝不触碰；带上是别的数据、或池标记不符的，标记后跳过，等人处理。
fn ensure_write_tape(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>) -> Result<String> {
    a.drive = None;
    let mut inv = inventory(&a.devices)?;
    let mut cands: Vec<Candidate> = {
        let st = status.lock().unwrap_or_else(|e| e.into_inner());
        st.pools
            .iter()
            .flat_map(|p| p.tapes.iter().map(move |b| (p, b)))
            .filter(|(_, b)| st.tapes.get(*b).is_none_or(|t| t.0 == tape_state::APPENDABLE))
            .map(|(p, b)| Candidate {
                barcode: b.clone(),
                pool_uuid: p.uuid.clone(),
                pool_name: p.name.clone(),
                file_limit: p.file_limit,
                bytes_used: st.tapes.get(b).map(|t| t.3).unwrap_or(0),
            })
            .collect()
    };
    let in_drive = |inv: &Inventory, b: &str| inv.drives.iter().any(|d| d.loaded.as_ref().is_some_and(|(x, _)| x == b));
    cands.retain(|c| in_drive(&inv, &c.barcode) || inv.slots.contains_key(&c.barcode));
    cands.sort_by(|x, y| {
        in_drive(&inv, &y.barcode).cmp(&in_drive(&inv, &x.barcode)).then(y.bytes_used.cmp(&x.bytes_used)).then(x.barcode.cmp(&y.barcode))
    });
    let mut notes = Vec::new();
    let round = a.round;
    let mark = |barcode: &str, state: &str| {
        let _ = tx.send(ExecEvent::TapeState { round, barcode: barcode.to_string(), state: state.to_string() });
    };

    for c in &cands {
        // 1. 确保它在某个驱动器里
        let slot = match inv.drives.iter().position(|d| d.loaded.as_ref().is_some_and(|(b, _)| *b == c.barcode)) {
            Some(i) => i,
            None => {
                let Some(i) = inv.drives.iter().position(|d| d.loaded.is_none()) else {
                    notes.push("没有空闲的驱动器".to_string());
                    break;
                };
                let from = inv.slots[&c.barcode];
                info!("执行线程: 装载 {} (槽位 {:#06x}) -> 驱动器 {}", c.barcode, from, a.devices[inv.drives[i].dev].name);
                if let Err(e) = move_medium(&a.devices, from, inv.drives[i].addr).and_then(|_| wait_ready(a.devices[inv.drives[i].dev].dev.as_ref())) {
                    notes.push(format!("{} 装载失败: {}", c.barcode, e));
                    mark(&c.barcode, tape_state::CHECK);
                    continue;
                }
                inv.drives[i].loaded = Some((c.barcode.clone(), Some(from)));
                inv.slots.remove(&c.barcode);
                inv.empty_slots.retain(|s| *s != from);
                inv.empty_slots.insert(0, from);
                i
            }
        };
        let di = inv.drives[slot].dev;
        match open_candidate(a, opts, files, tx, c, di)? {
            Ok(mut summary) => {
                a.drive = Some(di);
                if !notes.is_empty() {
                    summary.push_str(&format!("（跳过：{}）", notes.join("；")));
                }
                return Ok(summary);
            }
            Err((note, state)) => {
                // 这盘不能用：记下原因，把它卸回槽位，驱动器留给下一盘候选
                notes.push(note);
                mark(&c.barcode, state);
                if let Err(e) = unload_drive(&a.devices, &inv, &inv.drives[slot]) {
                    warn!("执行线程: 卸带 {} 失败: {}", c.barcode, e);
                } else if let Some((b, home)) = inv.drives[slot].loaded.take() {
                    let dest = home.filter(|h| inv.empty_slots.contains(h)).or(inv.empty_slots.first().copied());
                    if let Some(dest) = dest {
                        inv.empty_slots.retain(|s| *s != dest);
                        inv.slots.insert(b, dest);
                    }
                }
            }
        }
    }

    // 没有可写的带。报告驱动器里那些我们不会碰的带，便于运维判断
    for d in &inv.drives {
        if let Some((b, _)) = &d.loaded {
            if !cands.iter().any(|c| c.barcode == *b) {
                notes.push(format!("{} 里的 {} 未归属或不可写，不触碰", a.devices[d.dev].name, b));
            }
        }
    }
    let why = if notes.is_empty() { "池里没有已归属且可写的磁带".to_string() } else { notes.join("；") };
    // 写不了不影响查：首版一个逻辑库一个池，查询落在这个池的目录上
    let pool = status.lock().unwrap_or_else(|e| e.into_inner()).pools.first().map(|p| p.uuid.clone());
    files.close_no_tape(a.round, pool, &why);
    Ok(format!("仅持有预留：{}", why))
}

/// 识别、（需要时）格式化、挂载并开放一盘已在驱动器里的候选带。
/// 外层 `Err` 是失去执行资格之类必须中止的错误；内层 `Err` 是"这盘不能用"及应标记的状态。
#[allow(clippy::type_complexity)]
fn open_candidate(
    a: &Active,
    opts: &ExecOptions,
    files: &FileService,
    tx: &Sender<ExecEvent>,
    c: &Candidate,
    di: usize,
) -> Result<std::result::Result<String, (String, &'static str)>> {
    let dev = a.devices[di].dev.as_ref();
    // 2. 带上是什么
    match probe_format(dev) {
        Ok(FormatProbe::Ltfs) => {}
        Ok(FormatProbe::Blank) => {
            info!("执行线程: {} 是空白带，归属池 {}，首次使用前格式化", c.barcode, c.pool_name);
            let mk = MkltfsOptions { volume_id: c.barcode.chars().take(6).collect(), block_size: opts.block_size, ..Default::default() };
            if let Err(e) = mkltfs(dev, &mk) {
                if ownership_lost(&e) {
                    return Err(e);
                }
                return Ok(Err((format!("{} 格式化失败: {}", c.barcode, e), tape_state::CHECK)));
            }
        }
        Ok(FormatProbe::Foreign(what)) => {
            return Ok(Err((format!("{} 上是别的数据（{}），不自动覆盖", c.barcode, what), tape_state::LABEL_MISMATCH)));
        }
        Err(e) if ownership_lost(&e) => return Err(e),
        Err(e) => {
            return Ok(Err((format!("{} 无法识别: {}", c.barcode, e), tape_state::CHECK)));
        }
    }

    // 3. 挂载（恢复协议），需要时收尾
    let mut vol = match LtfsVolume::mount(dev) {
        Ok(v) => v,
        Err(e) if ownership_lost(&e) => return Err(e),
        Err(e) => {
            return Ok(Err((format!("{} 无法挂载: {}", c.barcode, e), tape_state::CHECK)));
        }
    };
    vol.set_reservation_guard(Some(a.key));
    let tail = vol.recovery().dp.tail;
    let mut summary = format!("{} [{}]: gen={} 文件 {} 个 尾部 {:?}", a.devices[di].name, c.barcode, vol.index().generation, vol.list().len(), tail);
    if !vol.writable() && matches!(tail, TailKind::UnindexedData | TailKind::TruncatedIndex) {
        let policy = if opts.salvage { TailPolicy::Salvage } else { TailPolicy::Discard };
        let rep = vol.close_tail(policy)?;
        summary.push_str(&format!("；已收尾：放弃块 {}..{}，新索引 gen={}", rep.abandoned_first_block, rep.abandoned_end_block.saturating_sub(1), rep.generation));
    }
    if !vol.writable() {
        return Ok(Err((format!("{} 只读: {}", c.barcode, vol.restricted_reason().unwrap_or("-")), tape_state::CHECK)));
    }

    // 4. 池标记（LTFS 2.5.1 F.4）：没有就写上，不符就不用
    match vol.root_xattr(XATTR_POOL_UUID).map(str::to_string) {
        Some(u) if u == c.pool_uuid => {}
        Some(u) => {
            return Ok(Err((format!("{} 的池标记是 {}，与归属 {} 不符", c.barcode, u, c.pool_uuid), tape_state::LABEL_MISMATCH)));
        }
        None => {
            vol.set_root_xattr(XATTR_POOL_UUID, &c.pool_uuid)?;
            vol.set_root_xattr(XATTR_POOL_NAME, &c.pool_name)?;
            vol.commit()?;
            summary.push_str("；已写入池标记");
        }
    }

    // 5. 还写得下吗
    let files_now = vol.list().len() as u64;
    let (total, remaining) = crate::ltfs::mam::Mam::with_partition(dev, 1)
        .read_partition_capacity()
        .ok()
        .flatten()
        .map(|pc| (pc.maximum, pc.remaining)) // 已经是字节
        .unwrap_or((1 << 50, 1 << 50));
    // 保留空间：任何时候都要写得下收尾所需的索引
    let index_bytes = (files_now + 1) * 1100;
    let reserve = (total / 50).max(4 * index_bytes) + (total / 20).min(1 << 30);
    if files_now >= c.file_limit {
        return Ok(Err((format!("{} 文件数 {} 已到上限", c.barcode, files_now), tape_state::DATA_FULL)));
    }
    if remaining <= reserve {
        return Ok(Err((format!("{} 已满（剩余 {} MiB）", c.barcode, remaining >> 20), tape_state::FULL)));
    }

    files.open(
        a.round,
        TapeIdent { barcode: c.barcode.clone(), pool_uuid: c.pool_uuid.clone() },
        TapeLimits { file_limit: c.file_limit, usable_capacity: total.saturating_sub(reserve) },
        vol.index(),
        remaining - reserve,
        true,
    );
    // 把这盘带的完整列表交给 Raft 层：目录落后于磁带时据此对账（以磁带为准）
    let (list, bytes_used) = catalog_of(vol.index());
    let _ = tx.send(ExecEvent::Committed {
        round: a.round,
        barcode: c.barcode.clone(),
        volume_uuid: vol.label().volume_uuid.to_string(),
        generation: vol.index().generation,
        files_total: list.len() as u64,
        bytes_used,
        files: list,
        full: true,
    });
    summary.push_str(&format!("；可用 {} MiB", (remaining - reserve) >> 20));
    Ok(Ok(summary))
}

/// 当前带写满或到文件数上限：把队列里剩下的落带，标记状态，卸回槽位，换下一盘。
fn switch_tape(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>, new_state: &str) -> std::result::Result<String, String> {
    process_uploads(a, files, tx, true)?;
    let barcode = files.tape().map(|t| t.barcode).unwrap_or_default();
    info!("执行线程: {} 转为 {}，换下一盘", barcode, new_state);
    files.close(&format!("{} 已 {}，正在换带", barcode, new_state));
    let _ = tx.send(ExecEvent::TapeState { round: a.round, barcode: barcode.clone(), state: new_state.to_string() });
    // 状态经 Raft 应用有延迟；本地先记住，免得马上又选中它
    {
        let mut st = status.lock().unwrap_or_else(|e| e.into_inner());
        st.tapes.entry(barcode.clone()).or_insert_with(|| (String::new(), 0, 0, 0)).0 = new_state.to_string();
    }
    if let Ok(inv) = inventory(&a.devices) {
        if let Some(d) = inv.drives.iter().find(|d| d.loaded.as_ref().is_some_and(|(b, _)| *b == barcode)) {
            if let Err(e) = unload_drive(&a.devices, &inv, d) {
                warn!("执行线程: 卸带 {} 失败: {}", barcode, e);
            }
        }
    }
    ensure_write_tape(a, opts, files, status, tx).map_err(|e| e.to_string())
}

/// 把文件服务里已完成的上传成批落带：一次挂载、逐个追加、一次提交，然后发布。
/// 返回 `Err(原因)` 表示本轮不能再继续（失去资格，或提交结果未定需要重新恢复）。
fn process_uploads(a: &mut Active, files: &FileService, tx: &Sender<ExecEvent>, flush: bool) -> std::result::Result<(), String> {
    let Some(i) = a.drive else {
        return Ok(());
    };
    while let Some((batch, uploads)) = if flush { files.take_batch_now(a.round) } else { files.take_batch(a.round) } {
        let dev = a.devices[i].dev.as_ref();
        let result = (|| -> Result<ExecEvent> {
            let mut vol = LtfsVolume::mount(dev)?;
            vol.set_reservation_guard(Some(a.key));
            for u in &uploads {
                let mut f = std::io::BufReader::new(std::fs::File::open(&u.spool)?);
                vol.append_file(&u.path, &mut f)?;
            }
            vol.commit()?;
            let (all, bytes_used) = catalog_of(vol.index());
            let written: Vec<FileRec> =
                all.iter().filter(|r| uploads.iter().any(|u| u.path == r.path)).cloned().collect();
            Ok(ExecEvent::Committed {
                round: a.round,
                barcode: files.tape().map(|t| t.barcode).unwrap_or_default(),
                volume_uuid: vol.label().volume_uuid.to_string(),
                generation: vol.index().generation,
                files_total: all.len() as u64,
                bytes_used,
                files: written,
                full: false,
            })
        })();
        match result {
            Ok(ev) => {
                if let ExecEvent::Committed { generation, files: written, .. } = &ev {
                    info!("执行线程: 已提交 {} 个文件，索引 gen={}", uploads.len(), generation);
                    let hashes = written.iter().map(|r| (r.path.clone(), r.sha256.clone())).collect();
                    files.batch_done(a.round, &batch, &uploads, Ok((*generation, hashes)));
                }
                let _ = tx.send(ev);
            }
            Err(e) => {
                // 提交没有得到确认：这些文件结果未定。本轮到此为止，由下一次接管的恢复协议判定。
                files.batch_done(a.round, &batch, &uploads, Err(e.to_string()));
                return Err(e.to_string());
            }
        }
    }
    Ok(())
}

fn read_file(dev: &dyn TapeTransport, path: &str) -> Result<Vec<u8>> {
    const MAX: u64 = 256 << 20;
    let vol = LtfsVolume::mount(dev)?;
    let p = path.trim_start_matches('/');
    let len = vol.index().find_file(p).map(|f| f.length).ok_or_else(|| TapeError::Ltfs(format!("文件不存在: {}", path)))?;
    if len > MAX {
        return Err(TapeError::Ltfs(format!("文件 {} 字节，超过骨架读接口的上限", len)));
    }
    let mut out = Vec::with_capacity(len as usize);
    vol.read_file_to_writer(p, &mut out)?;
    Ok(out)
}

/// 读一个文件，无论它在哪盘带上。目录给出条码；不在驱动器里就装载。
/// 有第二个驱动器就用它（读带留在里面，空闲后卸回）；只有一个驱动器就临时换带，读完换回。
fn read_any(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>, path: &str) -> std::result::Result<Vec<u8>, ReadError> {
    read_any_inner(a, opts, files, status, tx, path)
}

fn read_any_inner(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>, path: &str) -> std::result::Result<Vec<u8>, ReadError> {
    use ReadError::{Busy, Failed};
    let st = files.stat(path).map_err(|e| Failed(e.to_string()))?.ok_or_else(|| Failed(format!("文件不存在: {}", path)))?;
    let barcode = st.barcode;
    // 1. 就在写入带上
    if files.tape().is_some_and(|t| t.barcode == barcode) {
        if let Some(i) = a.drive {
            return read_file(a.devices[i].dev.as_ref(), path).map_err(|e| Failed(e.to_string()));
        }
    }
    // 2. 就在读带上
    if let Some((i, b, _)) = &a.read {
        if *b == barcode {
            let i = *i;
            let out = read_file(a.devices[i].dev.as_ref(), path).map_err(|e| Failed(e.to_string()))?;
            a.read = Some((i, barcode, Instant::now()));
            return Ok(out);
        }
    }
    // 3. 要装载。先找驱动器
    let inv = inventory(&a.devices).map_err(|e| Failed(e.to_string()))?;
    let Some(&from) = inv.slots.get(&barcode) else {
        return Err(Failed(format!("{} 不在库里的存储槽位中", barcode)));
    };
    let write_dev = a.drive;
    let mut target: Option<usize> = None;
    // 3a. 之前的读带占着一个驱动器：卸回去
    if let Some((i, b, _)) = a.read.take() {
        if let Some(d) = inv.drives.iter().find(|d| d.dev == i) {
            unload_drive(&a.devices, &inv, d).map_err(|e| Failed(format!("卸下读带 {} 失败: {}", b, e)))?;
            target = Some(i);
        }
    }
    let inv = inventory(&a.devices).map_err(|e| Failed(e.to_string()))?;
    if target.is_none() {
        target = inv.drives.iter().find(|d| d.loaded.is_none() && Some(d.dev) != write_dev).map(|d| d.dev);
    }
    let mut swapped_write = false;
    if target.is_none() {
        // 3b. 没有第二个驱动器：把写入带暂时卸下（先落带），读完再换回
        let Some(w) = write_dev else { return Err(Failed("没有可用的驱动器".to_string())) };
        // 写入侧还在忙（队列、在途上传、提交中、待换带）就不换：让读请求稍后重试，
        // 而不是让写入等读。执行线程若在这里原地等，队列就没人落带了。
        if !files.write_side_idle(a.round) {
            return Err(Busy(format!("唯一的驱动器正在写入 {}，读 {} 上的文件要等写入侧空闲", files.tape().map(|t| t.barcode).unwrap_or_default(), barcode)));
        }
        files.close("暂时换带读取");
        if let Some(d) = inv.drives.iter().find(|d| d.dev == w) {
            unload_drive(&a.devices, &inv, d).map_err(|e| Failed(format!("卸下写入带失败: {}", e)))?;
        }
        a.drive = None;
        target = Some(w);
        swapped_write = true;
    }
    let i = target.expect("已确定驱动器");
    let inv = inventory(&a.devices).map_err(|e| Failed(e.to_string()))?;
    let addr = inv.drives.iter().find(|d| d.dev == i).map(|d| d.addr).ok_or_else(|| Failed("驱动器不在库存里".to_string()))?;
    info!("执行线程: 为读取装载 {} -> {}", barcode, a.devices[i].name);
    let loaded = move_medium(&a.devices, from, addr)
        .and_then(|_| wait_ready(a.devices[i].dev.as_ref()))
        .and_then(|_| LtfsVolume::mount(a.devices[i].dev.as_ref()));
    let result = match loaded {
        Ok(vol) => {
            // 装载了就对账：目录若落后于磁带，以磁带为准
            let known = status.lock().unwrap_or_else(|e| e.into_inner()).tapes.get(&barcode).map(|t| t.1).unwrap_or(0);
            if known != vol.index().generation {
                let (list, bytes_used) = catalog_of(vol.index());
                let _ = tx.send(ExecEvent::Committed {
                    round: a.round,
                    barcode: barcode.clone(),
                    volume_uuid: vol.label().volume_uuid.to_string(),
                    generation: vol.index().generation,
                    files_total: list.len() as u64,
                    bytes_used,
                    files: list,
                    full: true,
                });
            }
            let p = path.trim_start_matches('/');
            let mut out = Vec::new();
            vol.read_file_to_writer(p, &mut out).map(|_| out).map_err(|e| Failed(e.to_string()))
        }
        Err(e) => Err(Failed(format!("装载 {} 读取失败: {}", barcode, e))),
    };
    if swapped_write {
        // 读完就换回：读带卸回槽位，重新选写入带
        let inv2 = inventory(&a.devices).map_err(|e| Failed(e.to_string()))?;
        if let Some(d) = inv2.drives.iter().find(|d| d.dev == i) {
            let _ = unload_drive(&a.devices, &inv2, d);
        }
        match ensure_write_tape(a, opts, files, status, tx) {
            Ok(summary) => {
                let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
            }
            Err(e) => return Err(Failed(format!("读取后恢复写入带失败: {}", e))),
        }
    } else {
        a.read = Some((i, barcode, Instant::now()));
    }
    result
}

/// 读带空闲太久：卸回槽位，把驱动器腾出来。
fn unload_idle_read_tape(a: &mut Active, opts: &ExecOptions) {
    let Some((i, b, last)) = &a.read else { return };
    if last.elapsed() < opts.read_idle {
        return;
    }
    let (i, b) = (*i, b.clone());
    if let Ok(inv) = inventory(&a.devices) {
        if let Some(d) = inv.drives.iter().find(|d| d.dev == i) {
            match unload_drive(&a.devices, &inv, d) {
                Ok(()) => info!("执行线程: 读带 {} 空闲，已卸回槽位", b),
                Err(e) => warn!("执行线程: 卸下读带 {} 失败: {}", b, e),
            }
        }
    }
    a.read = None;
}

/// 服务期间的周期工作。返回 `Err(原因)` 表示已失去执行资格。
fn periodic(a: &mut Active, opts: &ExecOptions) -> std::result::Result<(), String> {
    unload_idle_read_tape(a, opts);
    if let (true, Some(i)) = (opts.demo_write, a.drive) {
        let dev = a.devices[i].dev.as_ref();
        a.seq += 1;
        let name = format!("/ltfsd-demo/round{:06}-{:04}.txt", a.round, a.seq);
        let body = format!("node={} round={} seq={}\n", opts.node_id, a.round, a.seq);
        let wrote = (|| -> Result<()> {
            let mut vol = LtfsVolume::mount(dev)?;
            vol.set_reservation_guard(Some(a.key));
            vol.append_file(&name, &mut Cursor::new(body.into_bytes()))?;
            vol.commit()
        })();
        match wrote {
            Ok(()) => info!("执行线程: 已提交 {}", name),
            Err(e) if ownership_lost(&e) => return Err(e.to_string()),
            Err(e) => warn!("执行线程: 写入 {} 失败: {}", name, e),
        }
    }
    // 无论写入结果如何，都以设备回读的持有者为准
    for d in &a.devices {
        match reservation::verify_holder(d.dev.as_ref(), a.key) {
            Ok(()) => {}
            Err(TapeError::ScsiCommand { sense_key: 0x05, .. }) if d.kind == DeviceKind::Changer => {}
            Err(e) if ownership_lost(&e) => return Err(format!("{}: {}", d.name, e)),
            Err(e) => warn!("执行线程: {} 持有者自检出错: {}", d.name, e),
        }
    }
    Ok(())
}
