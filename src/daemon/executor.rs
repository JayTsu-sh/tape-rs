//! 设备执行线程。只有它接触磁带设备；Raft 循环通过消息驱动它，它把结论发回去。
//!
//! 纪律：抢占预留只发生在收到 `Takeover` 时；持有期间一旦发现失去资格就停下并上报，
//! 绝不自行重新预留。是否再次接管由 Raft 层在重新当选、取得新轮次之后决定。

use std::io::{Cursor, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use sha2::{Digest, Sha256};

use super::files::{FileService, TapeIdent, TapeLimits, catalog_of, is_directory};
use super::state::tape_state;
use super::node::SharedStatus;
use super::state::FileRec;
use crate::changer::commands::MediumChanger;
use crate::changer::element::ElementType;
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::TailKind;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::{
    FormatProbe, LtfsVolume, TailPolicy, XATTR_DELETED_PATH, XATTR_POOL_NAME, XATTR_POOL_UUID, XATTR_VERSION, is_tombstone_path, probe_format,
    tombstone_path,
};
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
    /// 分块写入调用方提供的临时文件；成功后交还句柄，慢客户端不阻塞设备线程。
    ReadTo { path: String, file: std::fs::File, reply: Sender<std::result::Result<std::fs::File, ReadError>> },
    /// 计划停机：落带、退带、释放预留，然后经 `done` 回执。见 `shut_down`。
    Shutdown { done: Option<Sender<()>> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// 此刻读不了，稍后重试即可：只有一个驱动器且写入侧正忙，或换带尚未完成。
    Busy(String),
    Failed(String),
    /// 读取期间失去设备所有权，必须先终止本轮再交给客户端换节点。
    Lost(String),
}

impl ReadError {
    fn device(e: TapeError, context: &str) -> Self {
        let reason = format!("{}: {}", context, e);
        if ownership_lost(&e) {
            Self::Lost(reason)
        } else {
            Self::Failed(reason)
        }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Busy(s) => write!(f, "稍后重试: {}", s),
            ReadError::Failed(s) | ReadError::Lost(s) => write!(f, "{}", s),
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
    Committed {
        round: u64,
        barcode: String,
        volume_uuid: String,
        generation: u64,
        files_total: u64,
        /// 索引里活着的字节
        bytes_used: u64,
        /// 带上实际写掉的字节（容量读数）；读不到时为 0
        bytes_written: u64,
        files: Vec<FileRec>,
        full: bool,
    },
    /// 持有期间失去资格（预留被抢占、设备复位、自检失败）。执行线程已停手。
    Lost { round: u64, reason: String },
    /// 一盘带回收完毕：内容已全部搬到同池的其他带上，源带已重新格式化并写好池标记。
    Reclaimed { round: u64, barcode: String, volume_uuid: String, files: u64, bytes: u64 },
    /// 一次回收没做完就放弃了。`state` 是带该回到的状态：资源不够（驱动器数、装不上、
    /// 路径一直被占）退回 `appendable`；数据对不上、读不出来这类问题标成 `check`。
    /// 带绝不能留在 `reclaiming` 上——那个状态没有别的出口，而且带还会被继续写。
    ReclaimAbandoned { round: u64, barcode: String, reason: String, state: String },
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
    /// 本轮已写下的文件数，同时是版本号里的序号
    seq: u64,
    reclaim: Option<Reclaim>,
    /// 连续几个周期没能开始回收（装不上、没有空驱动器）
    reclaim_failures: u32,
    /// 刚放弃回收的带和放弃后过了几个周期：状态里的 `reclaiming` 要等 TapeState 应用后才消失，
    /// 这期间不要再拿它开工
    reclaim_given_up: Option<(String, u32)>,
}

/// 正在进行的回收。状态在 Raft 里（磁带状态 `reclaiming`），这里只是本轮的工作现场：
/// 中途换届的话新执行者从状态里看到那盘带仍在回收，重新建一份工作现场接着干。
struct Reclaim {
    barcode: String,
    /// 源带所在驱动器在 `devices` 里的下标
    drive: usize,
    /// 还没处理的路径，从末尾取
    todo: Vec<String>,
    /// 这一遍先跳过的（客户端正在重写同一个路径）
    retry: Vec<String>,
    passes: u32,
    /// 等目录把搬迁记录应用上来的周期数
    waits: u32,
    copied: u64,
    bytes: u64,
    /// 当前版本已经不在源带上、不用搬的
    superseded: u64,
}

/// 同一个路径反复被客户端重写时，回收最多让路这么多遍。
const RECLAIM_MAX_PASSES: u32 = 20;
/// 开始回收（装源带）连续失败这么多个周期就放弃，把带退回可写。
const RECLAIM_START_ATTEMPTS: u32 = 5;
/// 放弃之后最多等这么多个周期让 TapeState 应用；超过了就当提案丢了，重新来过。
const RECLAIM_GIVE_UP_GRACE: u32 = 10;
/// 搬完之后最多等目录应用这么多个工作周期（默认 3 秒一个，约 5 分钟）。
const RECLAIM_MAX_WAITS: u32 = 100;

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
            Ok(ExecRequest::Shutdown { done }) => {
                if let Some(a) = active.take() {
                    shut_down(a, &files, &tx);
                }
                if let Some(d) = done {
                    let _ = d.send(());
                }
                return;
            }
            // Raft 循环没了：不能再证明自己还有资格，立刻停手，也不去动设备
            Err(RecvTimeoutError::Disconnected) => return,
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
                let was_serving = active.as_ref().is_some_and(|a| a.serving);
                let res = match active.as_mut().filter(|a| a.serving) {
                    Some(a) => {
                        let mut out = Vec::new();
                        read_any(a, &opts, &files, &status, &tx, &path, &mut out).map(|_| out)
                    },
                    None => Err(ReadError::Busy("本节点不在服务".to_string())),
                };
                if was_serving && active.as_ref().is_some_and(|a| !a.serving) { active = None; }
                let _ = reply.send(res);
            }
            Ok(ExecRequest::ReadTo { path, mut file, reply }) => {
                let was_serving = active.as_ref().is_some_and(|a| a.serving);
                let res = match active.as_mut().filter(|a| a.serving) {
                    Some(a) => read_any(a, &opts, &files, &status, &tx, &path, &mut file).map(|_| file),
                    None => Err(ReadError::Busy("本节点不在服务".to_string())),
                };
                if was_serving && active.as_ref().is_some_and(|a| !a.serving) { active = None; }
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
                    // 重新选带失败说明已经不能再服务了，这时不该再去做周期自检把结论盖掉
                    if worked.is_ok() {
                        worked = periodic(a, &opts);
                    }
                }
                // 回收：一次搬一个批次就回到这里，读请求和客户端上传不会被长时间的回收饿死
                if worked.is_ok() && (a.reclaim.is_some() || periodic_due) {
                    match drive_reclaim(a, &opts, &files, &status, &tx) {
                        Ok(true) => next_work = Instant::now(),
                        Ok(false) => {}
                        Err(e) => worked = Err(e),
                    }
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

/// 计划停机。顺序不能换：
///
/// 1. 关掉文件服务，不再收新的上传；
/// 2. 把队列里已完成的上传落带并提交，让卷停在一个完整的尾部（失败也继续——卷交给下一个
///    执行者按恢复协议收尾，那条路本来就存在）；
/// 3. 给驱动器发 UNLOAD。Holo 的 handler 不收到 UNLOAD 不落盘（P4 查出来的），真实驱动器上
///    这也是让介质回到可取走状态的一步；
/// 4. **最后**才释放预留。释放只是计划移交的优化：不释放同样是安全的（下一个执行者会抢占），
///    但会在设备上留下一个已经不存在的持有者，让下一轮多绕一次抢占。正因为它是优化，
///    绝不能提前做——只有确认本线程不会再碰设备之后，放掉这层保护才是对的。
fn shut_down(mut a: Active, files: &FileService, tx: &Sender<ExecEvent>) {
    info!("执行线程: 轮次 {} 计划停机", a.round);
    files.drain();
    if a.serving && let Err(e) = process_uploads(&mut a, files, tx, true) {
        warn!("执行线程: 停机前落带失败: {}；卷留给下一个执行者收尾", e);
        files.close("停机提交失败");
        return;
    }
    if a.serving && let Err(e) = checkpoint_write_tape(&a, files, tx) {
        warn!("执行线程: 停机检查点失败: {}；保留介质和预留，等待恢复", e);
        files.close("停机检查点失败");
        return;
    }
    files.close("正在停机");
    let mut drives: Vec<usize> =
        [a.drive, a.read.as_ref().map(|(i, ..)| *i), a.reclaim.as_ref().map(|r| r.drive)].into_iter().flatten().collect();
    drives.sort_unstable();
    drives.dedup();
    for i in drives {
        if let Err(e) = TapeDrive::new(a.devices[i].dev.as_ref()).unload() {
            warn!("执行线程: {} 退带失败: {}", a.devices[i].name, e);
        }
    }
    for d in &a.devices {
        match reservation::release_and_unregister(d.dev.as_ref(), a.key) {
            Ok(()) => info!("执行线程: {} 的预留已释放", d.name),
            // 已经被抢占的设备本来就不归我们了，不是错误
            Err(e) => warn!("执行线程: {} 释放预留失败: {}", d.name, e),
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
    Ok(Active { round, key, devices, serving: false, drive: None, read: None, seq: 0, reclaim: None, reclaim_failures: 0, reclaim_given_up: None })
}

/// 带上实际写掉的字节：数据分区的最大容量减去剩余容量。读不到就返回 0。
/// 它与索引里活着的字节之差，就是被重写的旧副本和收尾放弃的块占的空间。
fn bytes_written_on(dev: &dyn TapeTransport) -> u64 {
    crate::ltfs::mam::Mam::with_partition(dev, 1)
        .read_partition_capacity()
        .ok()
        .flatten()
        .map(|pc| pc.maximum.saturating_sub(pc.remaining))
        .unwrap_or(0)
}

/// 本轮写下的下一个版本号。轮次全局单调，一轮之内只有这一个执行者在串行写入。
fn next_version(a: &mut Active) -> String {
    a.seq += 1;
    format!("{}.{}", a.round, a.seq)
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
    TapeDrive::new(devices[drive.dev].dev.as_ref()).unload()?;
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
            .filter(|(_, b)| st.tapes.get(*b).is_none_or(|t| t.state == tape_state::APPENDABLE))
            .map(|(p, b)| Candidate {
                barcode: b.clone(),
                pool_uuid: p.uuid.clone(),
                pool_name: p.name.clone(),
                file_limit: p.file_limit,
                bytes_used: st.tapes.get(b).map(|t| t.bytes_used).unwrap_or(0),
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
    let (list, bytes_used) = catalog_of(vol.index());
    let files_now = list.iter().filter(|r| !is_directory(&r.metadata)).count() as u64;
    let mut index_entries = vol.list().len() as u64 + 1;
    vol.index().walk_directories(|_, _| index_entries += 1);
    let (total, remaining) = crate::ltfs::mam::Mam::with_partition(dev, 1)
        .read_partition_capacity()
        .ok()
        .flatten()
        .map(|pc| (pc.maximum, pc.remaining)) // 已经是字节
        .unwrap_or((1 << 50, 1 << 50));
    // 保留空间：任何时候都要写得下收尾所需的索引
    let index_bytes = index_entries * 1100;
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
    let _ = tx.send(ExecEvent::Committed {
        round: a.round,
        barcode: c.barcode.clone(),
        volume_uuid: vol.label().volume_uuid.to_string(),
        generation: vol.index().generation,
        files_total: list.iter().filter(|r| !is_directory(&r.metadata)).count() as u64,
        bytes_used,
        bytes_written: total.saturating_sub(remaining),
        files: list,
        full: true,
    });
    summary.push_str(&format!("；可用 {} MiB", (remaining - reserve) >> 20));
    Ok(Ok(summary))
}

/// 已接纳写入的卷在离开驱动器前收束增量；不触碰读带、拒绝候选或未分配介质。
fn checkpoint_write_tape(a: &Active, files: &FileService, tx: &Sender<ExecEvent>) -> Result<()> {
    let Some(i) = a.drive else { return Ok(()) };
    let tape = files.tape().ok_or_else(|| TapeError::NotReady("缺少写入卷身份，不能生成卸载检查点".into()))?;
    reservation::verify_holder(a.devices[i].dev.as_ref(), a.key)?;
    let inv = inventory(&a.devices)?;
    if !inv.drives.iter().any(|d| d.dev == i && d.loaded.as_ref().is_some_and(|(b, _)| *b == tape.barcode)) {
        return Err(TapeError::NotReady("卸载检查点的介质与写入卷不符".into()));
    }
    let dev = a.devices[i].dev.as_ref();
    let mut vol = LtfsVolume::mount(dev)?;
    if !vol.index().incremental { return Ok(()); }
    vol.set_reservation_guard(Some(a.key));
    vol.commit()?;
    let (list, bytes_used) = catalog_of(vol.index());
    let _ = tx.send(ExecEvent::Committed {
        round: a.round,
        barcode: tape.barcode,
        volume_uuid: vol.label().volume_uuid.to_string(),
        generation: vol.index().generation,
        files_total: list.iter().filter(|r| !is_directory(&r.metadata)).count() as u64,
        bytes_used,
        bytes_written: bytes_written_on(dev),
        files: list,
        full: true,
    });
    Ok(())
}

/// 当前带写满或到文件数上限：把队列里剩下的落带，标记状态，卸回槽位，换下一盘。
fn switch_tape(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>, new_state: &str) -> std::result::Result<String, String> {
    process_uploads(a, files, tx, true)?;
    checkpoint_write_tape(a, files, tx).map_err(|e| e.to_string())?;
    let barcode = files.tape().map(|t| t.barcode).unwrap_or_default();
    info!("执行线程: {} 转为 {}，换下一盘", barcode, new_state);
    files.close(&format!("{} 已 {}，正在换带", barcode, new_state));
    let _ = tx.send(ExecEvent::TapeState { round: a.round, barcode: barcode.clone(), state: new_state.to_string() });
    // 状态经 Raft 应用有延迟；本地先记住，免得马上又选中它
    {
        let mut st = status.lock().unwrap_or_else(|e| e.into_inner());
        st.tapes.entry(barcode.clone()).or_default().state = new_state.to_string();
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

fn prepare_parent_directories(
    vol: &mut LtfsVolume,
    path: &str,
    version: &str,
    files: &FileService,
    changed: &mut std::collections::BTreeSet<String>,
    touch_parent: bool,
) -> Result<()> {
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    let mut parent = String::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        parent.push('/');
        parent.push_str(part);
        let mut metadata = match files
            .stat(&parent)
            .map_err(|e| TapeError::Ltfs(e.to_string()))?
        {
            Some(st) if is_directory(&st.metadata) => {
                super::files::directory_metadata(&st.metadata)
                    .map_err(|e| TapeError::Ltfs(e.to_string()))?
            }
            Some(_) => return Err(TapeError::Ltfs(format!("父路径 {} 不是目录", parent))),
            None => crate::ltfs::volume::FileMetadata::replacement(Vec::new()),
        };
        if touch_parent && path.rsplit_once('/').is_some_and(|(p, _)| p == parent) {
            let now = crate::ltfs::label::ltfs_time_now();
            metadata.meta.modify_time = now.clone();
            metadata.meta.change_time = now;
        }
        vol.remove_file(&tombstone_path(&parent))?;
        vol.put_catalog_directory(&parent, version, &metadata)?;
        changed.insert(parent.clone());
    }
    Ok(())
}

/// 把文件服务里已完成的上传成批落带：一次挂载、逐个追加、一次提交，然后发布。
/// 返回 `Err(原因)` 表示本轮不能再继续（失去资格，或提交结果未定需要重新恢复）。
fn process_uploads(
    a: &mut Active,
    files: &FileService,
    tx: &Sender<ExecEvent>,
    flush: bool,
) -> std::result::Result<(), String> {
    let Some(i) = a.drive else {
        return Ok(());
    };
    while let Some((batch, uploads)) = if flush {
        files.take_batch_now(a.round)
    } else {
        files.take_batch(a.round)
    } {
        let dev = a.devices[i].dev.as_ref();
        let (round, key) = (a.round, a.key);
        let mut seq = a.seq;
        let result = (|| -> Result<ExecEvent> {
            let mut vol = LtfsVolume::mount(dev)?;
            vol.set_reservation_guard(Some(key));
            let mut changed = std::collections::BTreeSet::new();
            for u in &uploads {
                seq += 1;
                let ver = format!("{}.{}", round, seq);
                if let Some((key, attr)) = &u.xattr_change {
                    if let Some(attr) = attr {
                        vol.set_node_xattr(&u.path, attr.clone(), false, false)?;
                    } else {
                        vol.remove_node_xattr(&u.path, key)?;
                    }
                    vol.set_catalog_version(&u.path, &ver)?;
                    changed.insert(u.path.clone());
                    continue;
                }
                if let Some((from, root)) = &u.rename_from {
                    if *root {
                        // 防止把当前池视图已删除或被别带替代的旧子项随目录搬过去。
                        let expected: std::collections::BTreeSet<_> = uploads.iter().filter(|v| v.task == u.task)
                            .filter_map(|v| v.rename_from.as_ref().map(|(p, _)| p.clone())).collect();
                        let actual: std::collections::BTreeSet<_> = catalog_of(vol.index()).0.into_iter()
                            .filter(|r| !r.deleted && (r.path == *from || r.path.starts_with(&format!("{from}/"))))
                            .map(|r| r.path).collect();
                        if actual != expected { return Err(TapeError::Ltfs("改名源目录与当前卷不一致".into())); }
                        prepare_parent_directories(&mut vol, from, &ver, files, &mut changed, true)?;
                        prepare_parent_directories(&mut vol, &u.path, &ver, files, &mut changed, true)?;
                        vol.discard_catalog_path(&u.path)?;
                        vol.rename_path(from, &u.path)?;
                    }
                    vol.remove_file(&tombstone_path(&u.path))?;
                    vol.set_catalog_version(&u.path, &ver)?;
                    changed.insert(u.path.clone());
                    continue;
                }
                // 同一盘带上一个路径只留一条记录：删除去掉本带上的文件，写入去掉本带上的墓碑。
                // 别的带上的旧副本不动，由版本比较让它们失效（见 `TOMBSTONE_DIR`）。
                changed.insert(u.path.clone());
                let current = files
                    .lookup(&u.path)
                    .map_err(|e| TapeError::Ltfs(e.to_string()))?;
                let existed = current.as_ref().is_some_and(|st| !st.deleted);
                if !u.delete || existed {
                    prepare_parent_directories(
                        &mut vol,
                        &u.path,
                        &ver,
                        files,
                        &mut changed,
                        if u.delete { existed } else { !existed },
                    )?;
                }
                if u.delete {
                    vol.discard_catalog_path(&u.path)?;
                    let tomb = tombstone_path(&u.path);
                    vol.append_file_with_xattrs(
                        &tomb,
                        &mut std::io::empty(),
                        &[
                            (XATTR_VERSION, &ver),
                            (XATTR_DELETED_PATH, &u.path),
                            (
                                "tapers.deletedKind",
                                if u.directory { "directory" } else { "file" },
                            ),
                        ],
                    )?;
                } else if let Some(target) = &u.symlink_target {
                    vol.discard_catalog_path(&u.path)?;
                    vol.remove_file(&tombstone_path(&u.path))?;
                    vol.add_symlink_with_metadata(&u.path, target, u.metadata.as_ref())?;
                    vol.set_catalog_version(&u.path, &ver)?;
                } else if u.directory {
                    vol.remove_file(&tombstone_path(&u.path))?;
                    if u.metadata.is_none() {
                        vol.discard_catalog_path(&u.path)?;
                    }
                    let metadata = u.metadata.clone().unwrap_or_else(|| {
                        crate::ltfs::volume::FileMetadata::replacement(Vec::new())
                    });
                    vol.put_catalog_directory(&u.path, &ver, &metadata)?;
                } else {
                    vol.discard_catalog_path(&u.path)?;
                    vol.remove_file(&tombstone_path(&u.path))?;
                    let mut f = std::io::BufReader::new(std::fs::File::open(&u.spool)?);
                    if let Some(metadata) = &u.metadata {
                        vol.append_relocated_file(&u.path, &mut f, &ver, metadata)?;
                    } else {
                        vol.append_file_with_xattrs(&u.path, &mut f, &[(XATTR_VERSION, &ver)])?;
                    }
                }
            }
            vol.commit_incremental()?;
            let (all, bytes_used) = catalog_of(vol.index());
            let written: Vec<FileRec> = all
                .iter()
                .filter(|r| changed.contains(&r.path))
                .cloned()
                .collect();
            Ok(ExecEvent::Committed {
                round: a.round,
                barcode: files.tape().map(|t| t.barcode).unwrap_or_default(),
                volume_uuid: vol.label().volume_uuid.to_string(),
                generation: vol.index().generation,
                files_total: all.iter().filter(|r| !is_directory(&r.metadata)).count() as u64,
                bytes_used,
                // 一次 MAM 读，不动磁带：可回收空间要靠它才算得准
                bytes_written: bytes_written_on(dev),
                files: written,
                full: false,
            })
        })();
        a.seq = seq;
        match result {
            Ok(ev) => {
                if let ExecEvent::Committed {
                    generation,
                    files: written,
                    ..
                } = &ev
                {
                    info!(
                        "执行线程: 已提交 {} 个文件，索引 gen={}",
                        uploads.len(),
                        generation
                    );
                    let hashes = written
                        .iter()
                        .map(|r| (r.path.clone(), r.clone()))
                        .collect();
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

fn read_file<W: Write>(dev: &dyn TapeTransport, path: &str, out: &mut W) -> Result<u64> {
    let vol = LtfsVolume::mount(dev)?;
    vol.read_file_to_writer(path.trim_start_matches('/'), out)
}

/// 读一个文件，无论它在哪盘带上。目录给出条码；不在驱动器里就装载。
/// 有第二个驱动器就用它（读带留在里面，空闲后卸回）；只有一个驱动器就临时换带，读完换回。
fn read_any<W: Write>(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
    path: &str,
    out: &mut W,
) -> std::result::Result<u64, ReadError> {
    let result = read_any_inner(a, opts, files, status, tx, path, out);
    if let Err(ReadError::Lost(reason)) = &result {
        a.serving = false;
        files.close(reason);
        let _ = tx.send(ExecEvent::Lost {
            round: a.round,
            reason: reason.clone(),
        });
        // HTTP 的读取先写私有 spool，尚未向客户端发送内容，可以安全换节点重读。
        return Err(ReadError::Busy(reason.clone()));
    }
    result
}

fn read_any_inner<W: Write>(a: &mut Active, opts: &ExecOptions, files: &FileService, status: &SharedStatus, tx: &Sender<ExecEvent>, path: &str, out: &mut W) -> std::result::Result<u64, ReadError> {
    use ReadError::{Busy, Failed};
    let st = files.stat(path).map_err(|e| Failed(e.to_string()))?.ok_or_else(|| Failed(format!("文件不存在: {}", path)))?;
    let barcode = st.barcode;
    // 1. 就在写入带上
    if files.tape().is_some_and(|t| t.barcode == barcode) {
        if let Some(i) = a.drive {
            return read_file(a.devices[i].dev.as_ref(), path, out).map_err(|e| ReadError::device(e, "读取文件失败"));
        }
    }
    // 2. 就在读带上
    if let Some((i, b, _)) = &a.read {
        if *b == barcode {
            let i = *i;
            let out = read_file(a.devices[i].dev.as_ref(), path, out).map_err(|e| ReadError::device(e, "读取文件失败"))?;
            a.read = Some((i, barcode, Instant::now()));
            return Ok(out);
        }
    }
    // 3. 要装载。先找驱动器
    let inv = inventory(&a.devices).map_err(|e| ReadError::device(e, "读取库存失败"))?;
    let Some(&from) = inv.slots.get(&barcode) else {
        return Err(Failed(format!("{} 不在库里的存储槽位中", barcode)));
    };
    let write_dev = a.drive;
    let mut target: Option<usize> = None;
    // 3a. 之前的读带占着一个驱动器：卸回去
    if let Some((i, b, _)) = a.read.take() {
        if let Some(d) = inv.drives.iter().find(|d| d.dev == i) {
            unload_drive(&a.devices, &inv, d).map_err(|e| ReadError::device(e, &format!("卸下读带 {} 失败", b)))?;
            target = Some(i);
        }
    }
    let inv = inventory(&a.devices).map_err(|e| ReadError::device(e, "读取库存失败"))?;
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
        if let Err(e) = checkpoint_write_tape(a, files, tx) {
            let reason = format!("写入带检查点失败: {}", e);
            a.serving = false;
            files.close(&reason);
            let _ = tx.send(ExecEvent::Lost { round: a.round, reason: reason.clone() });
            return Err(Failed(reason));
        }
        files.close("暂时换带读取");
        if let Some(d) = inv.drives.iter().find(|d| d.dev == w) {
            unload_drive(&a.devices, &inv, d).map_err(|e| ReadError::device(e, "卸下写入带失败"))?;
        }
        a.drive = None;
        target = Some(w);
        swapped_write = true;
    }
    let i = target.expect("已确定驱动器");
    let inv = inventory(&a.devices).map_err(|e| ReadError::device(e, "读取库存失败"))?;
    let addr = inv.drives.iter().find(|d| d.dev == i).map(|d| d.addr).ok_or_else(|| Failed("驱动器不在库存里".to_string()))?;
    info!("执行线程: 为读取装载 {} -> {}", barcode, a.devices[i].name);
    let loaded = move_medium(&a.devices, from, addr)
        .and_then(|_| wait_ready(a.devices[i].dev.as_ref()))
        .and_then(|_| LtfsVolume::mount(a.devices[i].dev.as_ref()));
    let result = match loaded {
        Ok(vol) => {
            // 装载了就对账：目录若落后于磁带，以磁带为准
            let known = status.lock().unwrap_or_else(|e| e.into_inner()).tapes.get(&barcode).map(|t| t.generation).unwrap_or(0);
            if known != vol.index().generation {
                let (list, bytes_used) = catalog_of(vol.index());
                let _ = tx.send(ExecEvent::Committed {
                    round: a.round,
                    barcode: barcode.clone(),
                    volume_uuid: vol.label().volume_uuid.to_string(),
                    generation: vol.index().generation,
                    files_total: list.iter().filter(|r| !is_directory(&r.metadata)).count() as u64,
                    bytes_used,
                    bytes_written: bytes_written_on(a.devices[i].dev.as_ref()),
                    files: list,
                    full: true,
                });
            }
            let p = path.trim_start_matches('/');
            vol.read_file_to_writer(p, out).map_err(|e| ReadError::device(e, "读取文件失败"))
        }
        Err(e) => Err(ReadError::device(e, &format!("装载 {} 读取失败", barcode))),
    };
    if matches!(&result, Err(ReadError::Lost(_))) {
        return result;
    }
    if swapped_write {
        // 读完就换回：读带卸回槽位，重新选写入带
        let inv2 = inventory(&a.devices).map_err(|e| ReadError::device(e, "读取库存失败"))?;
        if let Some(d) = inv2.drives.iter().find(|d| d.dev == i) {
            unload_drive(&a.devices, &inv2, d).map_err(|e| ReadError::device(e, "读取后卸带失败"))?;
        }
        match ensure_write_tape(a, opts, files, status, tx) {
            Ok(summary) => {
                let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
            }
            Err(e) => return Err(ReadError::device(e, "读取后恢复写入带失败")),
        }
    } else {
        a.read = Some((i, barcode, Instant::now()));
    }
    result
}

// ---------- 回收（reclaim） ----------
//
// 一盘带上被重写、被替换的旧副本，以及历次收尾放弃的块，都是再也不会被读到的字节。
// 回收把带上**还活着**的文件搬到同池的其他带上，然后重新格式化源带，把这些字节要回来。
//
// 三条纪律：
//
// 1. **搬迁走的是普通上传那条路**（暂存区 → 文件服务 → 合批落带 → CatalogPart/TapeCommitted）。
//    这样目录的更新与卷提交是同一件事，不需要为回收另写一条容易走偏的目录路径；
//    客户端同时在写同一个路径时，`VolumeState` 的路径占用就是天然的互锁。
// 2. **读源带时重算 sha256 与索引里的对照**。这是在销毁这一份副本之前、对它的端到端验证。
// 3. **格式化前必须证明源带上没有任何还活着的内容**：源带索引里的每个路径，目录给出的
//    当前位置都不在源带上；而且目录里没有任何一条记录还指向源带。任何一条对不上就不格式化。
//    这是整个流程里唯一一次销毁数据，闸门只有这一道。

/// 状态里有没有一盘等着回收的带。
fn pending_reclaim(status: &SharedStatus) -> Option<String> {
    let st = status.lock().unwrap_or_else(|e| e.into_inner());
    st.tapes.iter().find(|(_, t)| t.state == tape_state::RECLAIMING).map(|(b, _)| b.clone())
}

fn pool_of(status: &SharedStatus, barcode: &str) -> Option<(String, String)> {
    let st = status.lock().unwrap_or_else(|e| e.into_inner());
    st.pools.iter().find(|p| p.tapes.iter().any(|b| b == barcode)).map(|p| (p.uuid.clone(), p.name.clone()))
}

/// 推进回收。返回 `Ok(true)` 表示还有活要干、应当立刻再来一轮。
/// 返回 `Err` 只用于必须结束本轮的情况（失去资格、落带结果未定）。
fn drive_reclaim(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
) -> std::result::Result<bool, String> {
    if a.reclaim.is_none() {
        let Some(barcode) = pending_reclaim(status) else {
            a.reclaim_given_up = None;
            return Ok(false);
        };
        // 刚放弃过这盘带：等状态机把 TapeState 应用上来，别马上又拿它开工
        if let Some((b, n)) = &mut a.reclaim_given_up {
            if *b == barcode && *n < RECLAIM_GIVE_UP_GRACE {
                *n += 1;
                return Ok(false);
            }
            a.reclaim_given_up = None;
        }
        match start_reclaim(a, opts, files, status, tx, &barcode) {
            Ok(()) => a.reclaim_failures = 0,
            Err(e) if ownership_lost(&e) => return Err(e.to_string()),
            Err(e) => {
                // 干不了。驱动器不够是确定的；装不上、没有空驱动器再试几个周期。
                // 之后都放弃并把带退回可写：留在 reclaiming 上既没有出口，又还会被继续写
                a.reclaim_failures += 1;
                let definite = a.devices.iter().filter(|d| d.kind == DeviceKind::Drive).count() < 2;
                if definite || a.reclaim_failures >= RECLAIM_START_ATTEMPTS {
                    a.reclaim_failures = 0;
                    give_up_reclaim(a, tx, &barcode, &e.to_string(), tape_state::APPENDABLE);
                } else {
                    warn!("执行线程: 暂时无法回收 {}: {}（第 {} 次）", barcode, e, a.reclaim_failures);
                }
                return Ok(false);
            }
        }
    }
    reclaim_step(a, opts, files, status, tx)
}

/// 放弃对一盘带的回收：上报原因和它该回到的状态，并记住一段时间内不再拿它开工。
fn give_up_reclaim(a: &mut Active, tx: &Sender<ExecEvent>, barcode: &str, reason: &str, state: &str) {
    error!("执行线程: 放弃回收 {}（状态改为 {}）：{}", barcode, state, reason);
    let _ = tx.send(ExecEvent::ReclaimAbandoned { round: a.round, barcode: barcode.to_string(), reason: reason.to_string(), state: state.to_string() });
    a.reclaim_given_up = Some((barcode.to_string(), 0));
}

/// 准备工作现场：必要时先换掉写入带，再把源带装进另一台驱动器，读出它的工作单。
fn start_reclaim(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
    barcode: &str,
) -> Result<()> {
    if a.devices.iter().filter(|d| d.kind == DeviceKind::Drive).count() < 2 {
        return Err(TapeError::NotReady("回收需要两台驱动器：一台放写入带，一台放源带".into()));
    }
    // 源带正好是当前写入带：先把队列落带，换一盘写入带，再来搬它
    if files.tape().is_some_and(|t| t.barcode == barcode) {
        let summary = switch_tape(a, opts, files, status, tx, tape_state::RECLAIMING).map_err(TapeError::Ltfs)?;
        let _ = tx.send(ExecEvent::Serving { round: a.round, summary });
    }
    let drive = load_aside(a, barcode)?;
    let vol = LtfsVolume::mount(a.devices[drive].dev.as_ref())?;
    let mut todo = Vec::new();
    vol.index().walk_files(|p, _| todo.push(format!("/{}", p)));
    vol.index().walk_directories(|p, _| {
        if p != ".tapers" && !p.starts_with(".tapers/") { todo.push(format!("/{}", p)); }
    });
    // 字典序搬，从末尾取，所以倒过来放。顺序固定便于换届之后接着看日志
    todo.sort();
    todo.reverse();
    info!(
        "执行线程: 开始回收 {}（第 {} 代，带上 {} 个文件）→ 搬到 {}",
        barcode,
        vol.index().generation,
        todo.len(),
        files.tape().map(|t| t.barcode).unwrap_or_else(|| "当前写入带".into())
    );
    a.reclaim = Some(Reclaim {
        barcode: barcode.to_string(),
        drive,
        todo,
        retry: Vec::new(),
        passes: 0,
        waits: 0,
        copied: 0,
        bytes: 0,
        superseded: 0,
    });
    Ok(())
}

/// 把一盘带装进一台**不是写入带**的驱动器。必要时把读带卸回槽位腾地方。
fn load_aside(a: &mut Active, barcode: &str) -> Result<usize> {
    {
        let inv = inventory(&a.devices)?;
        if let Some(d) = inv.drives.iter().find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == barcode)) {
            return if Some(d.dev) == a.drive {
                Err(TapeError::NotReady(format!("{} 还在写入驱动器里", barcode)))
            } else {
                Ok(d.dev)
            };
        }
        let free = inv.drives.iter().any(|d| d.loaded.is_none() && Some(d.dev) != a.drive);
        if !free
            && let Some((i, b, _)) = a.read.take()
            && let Some(d) = inv.drives.iter().find(|d| d.dev == i)
        {
            unload_drive(&a.devices, &inv, d)?;
            info!("执行线程: 为回收腾出驱动器，读带 {} 已卸回槽位", b);
        }
    }
    // 卸带改变了库存，重新读一遍再决定去哪台驱动器
    let inv = inventory(&a.devices)?;
    let from = *inv.slots.get(barcode).ok_or_else(|| TapeError::NotReady(format!("{} 不在库里的存储槽位中", barcode)))?;
    let d = inv
        .drives
        .iter()
        .find(|d| d.loaded.is_none() && Some(d.dev) != a.drive)
        .ok_or_else(|| TapeError::NotReady("没有空闲的驱动器".into()))?;
    let (i, addr) = (d.dev, d.addr);
    info!("执行线程: 为回收装载 {} -> {}", barcode, a.devices[i].name);
    move_medium(&a.devices, from, addr)?;
    wait_ready(a.devices[i].dev.as_ref())?;
    Ok(i)
}

/// 搬一批、落一批；搬完之后核对并格式化源带。
fn reclaim_step(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
) -> std::result::Result<bool, String> {
    let chunk = match copy_chunk(a, files) {
        Ok(c) => c,
        Err(e) if ownership_lost(&e) => return Err(e.to_string()),
        // NotReady 是"这次做不成"（路径一直被客户端占着），不是带有问题
        Err(e) => {
            let attention = !matches!(e, TapeError::NotReady(_));
            abort_reclaim(a, tx, &e.to_string(), attention);
            return Ok(false);
        }
    };
    // 落带失败意味着提交结果未定，本轮到此为止（与普通上传一样）
    process_uploads(a, files, tx, true)?;
    match chunk {
        Chunk::More => return Ok(true),
        // 只剩被客户端占着的路径了：按正常节奏回来，不要贴着它空转
        Chunk::Yield => return Ok(false),
        Chunk::Done => {}
    }
    match finish_reclaim(a, opts, files, status, tx) {
        // 干完了，或者还在等目录把搬迁记录应用上来。两种情况都按正常节奏回来
        Ok(_) => Ok(false),
        Err(e) if ownership_lost(&e) => Err(e.to_string()),
        Err(e) => {
            let attention = !matches!(e, TapeError::NotReady(_));
            abort_reclaim(a, tx, &e.to_string(), attention);
            Ok(false)
        }
    }
}

enum Chunk {
    /// 攒够一个批次了，落带之后马上接着搬
    More,
    /// 这一遍能搬的都搬了，只剩正被客户端重写的路径，下个周期再看
    Yield,
    /// 工作单空了
    Done,
}

/// 搬一个批次的量就返回，让主循环有机会处理读请求和客户端上传。
fn copy_chunk(a: &mut Active, files: &FileService) -> Result<Chunk> {
    let policy = files.policy();
    let round = a.round;
    let (di, source) = {
        let r = a.reclaim.as_ref().expect("回收进行中");
        (r.drive, r.barcode.clone())
    };
    let vol = LtfsVolume::mount(a.devices[di].dev.as_ref())?;
    loop {
        // 攒够一个批次就回去落带：暂存区同时只放得下一批
        if files.queued(round).is_some_and(|(n, b)| b >= policy.max_bytes || n >= policy.max_files) {
            return Ok(Chunk::More);
        }
        let Some(path) = a.reclaim.as_mut().expect("回收进行中").todo.pop() else { break };
        let outcome = copy_one(&vol, &source, &path, files)?;
        let r = a.reclaim.as_mut().expect("回收进行中");
        match outcome {
            Copied::Moved(n) => {
                r.copied += 1;
                r.bytes += n;
            }
            Copied::Superseded => r.superseded += 1,
            Copied::Retry => r.retry.push(path),
            Copied::Halt(why) => {
                // 池里暂时没有能收下它的带。源带毫发无损，等运维加带之后接着搬；
                // 这不是源带的毛病，所以既不标 check，也不消耗"让路"的次数。
                warn!("执行线程: 回收 {} 暂停：{}", r.barcode, why);
                r.todo.push(path);
                return Ok(Chunk::Yield);
            }
        }
    }
    let r = a.reclaim.as_mut().expect("回收进行中");
    if r.retry.is_empty() {
        return Ok(Chunk::Done);
    }
    r.passes += 1;
    if r.passes > RECLAIM_MAX_PASSES {
        return Err(TapeError::NotReady(format!(
            "{} 上有 {} 个路径一直被客户端占用（例如 {}），放弃这次回收",
            r.barcode,
            r.retry.len(),
            r.retry[0]
        )));
    }
    r.todo = std::mem::take(&mut r.retry);
    Ok(Chunk::Yield)
}

enum Copied {
    Moved(u64),
    /// 这个路径的当前版本已经不在源带上了，不用搬
    Superseded,
    /// 这一遍先让开，下一遍再看
    Retry,
    /// 现在搬不了，但不是这盘带的错（池里没有可写的带）：整个搬迁暂停
    Halt(String),
}

/// 搬一个文件。读源带的同时重算 sha256 与索引里的对照——销毁源带之前，这是对这一份副本的
/// 端到端验证；对不上宁可整次回收作废，也不能把读坏的内容写到新带上再把源带格式化掉。
fn copy_one(vol: &LtfsVolume, source: &str, path: &str, files: &FileService) -> Result<Copied> {
    let p = path.trim_start_matches('/');
    if let Some(dir) = vol.index().find_directory(p) {
        if files
            .lookup(path)
            .map_err(|e| TapeError::Ltfs(e.to_string()))?
            .is_some_and(|st| st.barcode != source || st.deleted)
        {
            return Ok(Copied::Superseded);
        }
        return match files.relocate_directory(path, dir.into()) {
            Ok(_) => Ok(Copied::Moved(0)),
            Err(super::files::ServiceError::Exists(_)) => Ok(Copied::Superseded),
            Err(
                super::files::ServiceError::PathBusy(_)
                | super::files::ServiceError::SwitchingTape(_),
            ) => Ok(Copied::Retry),
            Err(
                e @ (super::files::ServiceError::NoTape(_)
                | super::files::ServiceError::NotServing(_)),
            ) => Ok(Copied::Halt(e.to_string())),
            Err(e) => Err(TapeError::Ltfs(format!("回收目录 {} 失败: {}", path, e))),
        };
    }
    let Some(node) = vol.index().find_file(p) else {
        return Ok(Copied::Superseded);
    };
    if is_tombstone_path(p) {
        // 墓碑目录下没有被删路径的文件不是我们写的，没有要保住的东西
        let Some(target) = node.xattr(XATTR_DELETED_PATH) else {
            return Ok(Copied::Superseded);
        };
        return copy_tombstone(
            source,
            target,
            node.xattr("tapers.deletedKind") == Some("directory"),
            files,
        );
    }
    let len = node.length;
    let want = node
        .xattr(crate::ltfs::index::XATTR_SHA256)
        .unwrap_or("")
        .to_string();
    // 目录说这个路径的当前记录在别的带上（被重写或被删除）：源带上这一份是旧副本，不用搬。
    // 查不到的（目录还没经 Raft 应用到本地）照搬不误：多一份总比留在要被格式化的带上强。
    if files
        .lookup(path)
        .map_err(|e| TapeError::Ltfs(e.to_string()))?
        .is_some_and(|st| st.barcode != source || st.deleted)
    {
        return Ok(Copied::Superseded);
    }
    if let Some(target) = &node.symlink {
        return match files.relocate_symlink(path, target, node.into()) {
            Ok(_) => Ok(Copied::Moved(0)),
            Err(super::files::ServiceError::Exists(_)) => Ok(Copied::Superseded),
            Err(
                super::files::ServiceError::PathBusy(_)
                | super::files::ServiceError::SwitchingTape(_),
            ) => Ok(Copied::Retry),
            Err(
                e @ (super::files::ServiceError::NoTape(_)
                | super::files::ServiceError::NotServing(_)),
            ) => Ok(Copied::Halt(e.to_string())),
            Err(e) => Err(TapeError::Ltfs(format!(
                "回收符号链接 {} 失败: {}",
                path, e
            ))),
        };
    }
    let h = match files.begin(path, len) {
        Ok(h) => h,
        // 客户端正在重写同一个路径：它写的是更新的版本，让它先走
        Err(super::files::ServiceError::PathBusy(_))
        | Err(super::files::ServiceError::SwitchingTape(_)) => {
            return Ok(Copied::Retry);
        }
        // 池里已经没有可写的带了：加带是运维动作，等它，别把源带标成有问题
        Err(
            e @ (super::files::ServiceError::NoTape(_) | super::files::ServiceError::NotServing(_)),
        ) => {
            return Ok(Copied::Halt(e.to_string()));
        }
        Err(e) => return Err(TapeError::Ltfs(format!("回收 {} 准入失败: {}", path, e))),
    };
    let read = (|| -> Result<String> {
        let mut w = HashingWriter {
            inner: std::io::BufWriter::new(std::fs::File::create(&h.spool)?),
            hash: Sha256::new(),
        };
        vol.read_file_to_writer(p, &mut w)?;
        w.inner.flush()?;
        Ok(w.hash
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect())
    })();
    match read {
        Ok(got) if want.is_empty() || got == want => {
            files
                .ingest(&h, len)
                .map_err(|e| TapeError::Ltfs(e.to_string()))?;
            files
                .finish_with_metadata(h, len, Some(node.into()))
                .map_err(|e| TapeError::Ltfs(e.to_string()))?;
            Ok(Copied::Moved(len))
        }
        Ok(got) => {
            files.abort(h);
            Err(TapeError::Ltfs(format!(
                "{} 上的 {} 读出来是 {}，索引里记的是 {}",
                source, path, got, want
            )))
        }
        Err(e) => {
            files.abort(h);
            Err(e)
        }
    }
}

/// 搬一块墓碑：在当前写入带上以新版本重新落一次。墓碑在带上是活的——源带一格式化，
/// 别的带上的旧副本就会在下次对账时复活——所以和文件一样要有着落才能格式化。
fn copy_tombstone(
    source: &str,
    target: &str,
    directory: bool,
    files: &FileService,
) -> Result<Copied> {
    // 这个路径已由别的带上的记录（新上传的文件，或另一块墓碑）决定：源带上这块作废了
    if files
        .lookup(target)
        .map_err(|e| TapeError::Ltfs(e.to_string()))?
        .is_some_and(|st| st.barcode != source)
    {
        return Ok(Copied::Superseded);
    }
    match files.relocate_tombstone_kind(target, directory) {
        Ok(_) => Ok(Copied::Moved(0)),
        Err(super::files::ServiceError::Exists(_)) => Ok(Copied::Superseded),
        // 客户端正在重新上传这个路径：它的版本更新，让它先走
        Err(super::files::ServiceError::PathBusy(_))
        | Err(super::files::ServiceError::SwitchingTape(_)) => Ok(Copied::Retry),
        Err(
            e @ (super::files::ServiceError::NoTape(_) | super::files::ServiceError::NotServing(_)),
        ) => Ok(Copied::Halt(e.to_string())),
        Err(e) => Err(TapeError::Ltfs(format!(
            "回收 {} 的墓碑失败: {}",
            target, e
        ))),
    }
}

struct HashingWriter<W: std::io::Write> {
    inner: W,
    hash: Sha256,
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 全部搬完了。格式化之前先证明源带上没有任何还活着的内容，然后才重新格式化并写回池标记。
/// 返回 `false` 表示还要等目录把搬迁记录应用上来。
fn finish_reclaim(
    a: &mut Active,
    opts: &ExecOptions,
    files: &FileService,
    status: &SharedStatus,
    tx: &Sender<ExecEvent>,
) -> Result<bool> {
    let (di, source) = {
        let r = a.reclaim.as_ref().expect("回收进行中");
        (r.drive, r.barcode.clone())
    };
    // 闸门一：目录里不能还有任何一条记录指向源带。目录经 Raft 应用有延迟，等它跟上来
    if let Some((n, _)) = files.live_on(&source).filter(|(n, _)| *n > 0) {
        let r = a.reclaim.as_mut().expect("回收进行中");
        r.waits += 1;
        if r.waits > RECLAIM_MAX_WAITS {
            return Err(TapeError::Ltfs(format!("等了很久，目录里仍有 {} 条记录指向 {}", n, source)));
        }
        return Ok(false);
    }
    // 闸门二：源带索引里的每个路径，目录给出的当前位置都不在源带上。
    // 闸门一只看目录有什么，这一条还看带上有什么——带上有、目录压根不知道的文件也拦得住。
    let dev = a.devices[di].dev.as_ref();
    let mut orphans = Vec::new();
    {
        let vol = LtfsVolume::mount(dev)?;
        // 墓碑按它记的被删路径核对：那个路径的当前记录（新文件或搬过去的墓碑）必须在别的带上
        let mut paths = Vec::new();
        vol.index().walk_files(|p, f| {
            if !is_tombstone_path(p) {
                paths.push(format!("/{}", p));
            } else if let Some(target) = f.xattr(XATTR_DELETED_PATH) {
                paths.push(target.to_string());
            }
        });
        vol.index().walk_directories(|p, _| {
            if p != ".tapers" && !p.starts_with(".tapers/") { paths.push(format!("/{}", p)); }
        });
        for path in paths {
            match files.lookup(&path) {
                Ok(Some(st)) if st.barcode != source => {}
                _ => orphans.push(path),
            }
        }
    }
    if let Some(first) = orphans.first() {
        return Err(TapeError::Ltfs(format!("{} 上还有 {} 个路径没有着落（例如 {}）", source, orphans.len(), first)));
    }

    let (pool_uuid, pool_name) =
        pool_of(status, &source).ok_or_else(|| TapeError::NotReady(format!("{} 已不在任何池里", source)))?;
    let (copied, bytes, superseded) = {
        let r = a.reclaim.as_ref().expect("回收进行中");
        (r.copied, r.bytes, r.superseded)
    };
    info!("执行线程: {} 上的内容已全部有着落（搬走 {} 个 / {} MiB，跳过旧副本 {} 个），重新格式化", source, copied, bytes >> 20, superseded);
    let mk = MkltfsOptions { volume_id: source.chars().take(6).collect(), block_size: opts.block_size, ..Default::default() };
    mkltfs(dev, &mk)?;
    let volume_uuid = {
        let mut vol = LtfsVolume::mount(dev)?;
        vol.set_reservation_guard(Some(a.key));
        vol.set_root_xattr(XATTR_POOL_UUID, &pool_uuid)?;
        vol.set_root_xattr(XATTR_POOL_NAME, &pool_name)?;
        vol.commit()?;
        vol.label().volume_uuid.to_string()
    };
    let _ = tx.send(ExecEvent::Reclaimed { round: a.round, barcode: source.clone(), volume_uuid, files: copied, bytes });
    a.reclaim = None;
    // 空出驱动器：这盘带现在是池里一盘干净的可写带，下次需要时再装
    unload_reclaim_drive(a, di, &source);
    Ok(true)
}

fn unload_reclaim_drive(a: &Active, di: usize, barcode: &str) {
    match inventory(&a.devices) {
        Ok(inv) => {
            if let Some(d) = inv.drives.iter().find(|d| d.dev == di)
                && let Err(e) = unload_drive(&a.devices, &inv, d)
            {
                warn!("执行线程: 卸下 {} 失败: {}", barcode, e);
            }
        }
        Err(e) => warn!("执行线程: 回收后读库存失败: {}", e),
    }
}

/// 放弃这次回收。`needs_attention` 为真时把带标成待核验：数据对不上、读不出来这类问题
/// 必须有人来看；否则退回可写。两种情况都不能让它继续留在回收队列里被反复重试。
fn abort_reclaim(a: &mut Active, tx: &Sender<ExecEvent>, reason: &str, needs_attention: bool) {
    let Some(r) = a.reclaim.take() else { return };
    let state = if needs_attention { tape_state::CHECK } else { tape_state::APPENDABLE };
    give_up_reclaim(a, tx, &r.barcode, reason, state);
    unload_reclaim_drive(a, r.drive, &r.barcode);
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
        let ver = next_version(a);
        let dev = a.devices[i].dev.as_ref();
        let name = format!("/ltfsd-demo/round{:06}-{:04}.txt", a.round, a.seq);
        let body = format!("node={} round={} seq={}\n", opts.node_id, a.round, a.seq);
        let wrote = (|| -> Result<()> {
            let mut vol = LtfsVolume::mount(dev)?;
            vol.set_reservation_guard(Some(a.key));
            vol.append_file_with_xattrs(&name, &mut Cursor::new(body.into_bytes()), &[(XATTR_VERSION, &ver)])?;
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

#[cfg(test)]
mod read_error_tests {
    use super::*;

    #[test]
    fn ownership_errors_are_distinct_from_medium_errors() {
        for (key, asc, ascq, lost) in [(6, 0x2a, 3, true), (6, 0x29, 0, true), (3, 0x11, 0, false)]
        {
            let err = ReadError::device(
                TapeError::ScsiCommand {
                    status: 2,
                    sense_key: key,
                    asc,
                    ascq,
                },
                "读取测试",
            );
            assert_eq!(matches!(err, ReadError::Lost(_)), lost);
            assert!(err.to_string().contains("status=0x02"));
            assert!(err.to_string().contains("sense_key="));
        }
        assert!(matches!(
            ReadError::device(
                TapeError::ReservationConflict {
                    device: "drive0".into()
                },
                "读取测试"
            ),
            ReadError::Lost(_)
        ));
    }
}
