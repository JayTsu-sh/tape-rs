//! 设备执行线程。只有它接触磁带设备；Raft 循环通过消息驱动它，它把结论发回去。
//!
//! 纪律：抢占预留只发生在收到 `Takeover` 时；持有期间一旦发现失去资格就停下并上报，
//! 绝不自行重新预留。是否再次接管由 Raft 层在重新当选、取得新轮次之后决定。

use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use log::{error, info, warn};

use super::files::FileService;
use crate::error::{Result, TapeError};
use crate::ltfs::recovery::TailKind;
use crate::ltfs::volume::{LtfsVolume, TailPolicy};
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
                found.push(ManagedDevice { name: format!("{} ({})", s, p), kind, dev: Box::new(dev) });
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
    Stop { reason: String },
    /// 文件服务有已完成的上传等待落带。
    Work,
    /// 读出一个已提交文件的内容。
    Read { path: String, reply: Sender<std::result::Result<Vec<u8>, String>> },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    Fenced { round: u64 },
    FenceFailed { round: u64, reason: String },
    Serving { round: u64, summary: String },
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
}

struct Active {
    round: u64,
    key: ReservationKey,
    devices: Vec<ManagedDevice>,
    serving: bool,
    /// 正在服务的驱动器在 `devices` 里的下标
    drive: Option<usize>,
    seq: u64,
}

pub fn run(
    provider: Box<dyn DeviceProvider>,
    opts: ExecOptions,
    rx: Receiver<ExecRequest>,
    tx: Sender<ExecEvent>,
    files: Arc<FileService>,
) {
    let mut active: Option<Active> = None;
    let mut next_work = Instant::now() + opts.interval;
    loop {
        let wait = next_work.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(ExecRequest::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(ExecRequest::Stop { reason }) => {
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
                    continue;
                };
                match recover(a, &opts, &files) {
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
                if let Err(reason) = process_uploads(a, &files) {
                    let round = a.round;
                    error!("执行线程: 轮次 {} 落带失败，放弃本轮: {}", round, reason);
                    files.close(&reason);
                    active = None;
                    let _ = tx.send(ExecEvent::Lost { round, reason });
                }
            }
            Ok(ExecRequest::Read { path, reply }) => {
                let res = match active.as_ref().filter(|a| a.serving).and_then(|a| a.drive.map(|i| (a, i))) {
                    Some((a, i)) => read_file(a.devices[i].dev.as_ref(), &path).map_err(|e| e.to_string()),
                    None => Err("本节点不在服务".to_string()),
                };
                let _ = reply.send(res);
            }
            Err(RecvTimeoutError::Timeout) => {
                next_work = Instant::now() + opts.interval;
                let Some(a) = active.as_mut().filter(|a| a.serving) else {
                    continue;
                };
                let worked = process_uploads(a, &files).and_then(|_| periodic(a, &opts));
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
    Ok(Active { round, key, devices, serving: false, drive: None, seq: 0 })
}

/// 挂载第一个装有 LTFS 卷的驱动器；尾部不完整时自动收尾。
fn recover(a: &mut Active, opts: &ExecOptions, files: &FileService) -> Result<String> {
    for (i, d) in a.devices.iter().enumerate().filter(|(_, d)| d.kind == DeviceKind::Drive) {
        let mut vol = match LtfsVolume::mount(d.dev.as_ref()) {
            Ok(v) => v,
            Err(e) if ownership_lost(&e) => return Err(e),
            Err(e) => {
                info!("执行线程: {} 上没有可用的卷: {}", d.name, e);
                continue;
            }
        };
        vol.set_reservation_guard(Some(a.key));
        let tail = vol.recovery().dp.tail;
        let mut summary = format!("{}: gen={} 文件 {} 个 尾部 {:?}", d.name, vol.index().generation, vol.list().len(), tail);
        if !vol.writable() && matches!(tail, TailKind::UnindexedData | TailKind::TruncatedIndex) {
            let policy = if opts.salvage { TailPolicy::Salvage } else { TailPolicy::Discard };
            let rep = vol.close_tail(policy)?;
            summary.push_str(&format!(
                "；已收尾：放弃块 {}..{}，新索引 gen={}",
                rep.abandoned_first_block,
                rep.abandoned_end_block.saturating_sub(1),
                rep.generation
            ));
        } else if !vol.writable() {
            summary.push_str(&format!("；只读：{}", vol.restricted_reason().unwrap_or("-")));
        }
        // 容量属性的单位是 MiB；读不到时不设上限，由写带时的真实错误兜底
        let free = crate::ltfs::mam::read_volume_capacity(d.dev.as_ref())
            .map(|c| c.remaining.saturating_mul(1 << 20))
            .unwrap_or(1 << 50);
        files.open(a.round, vol.index(), free, vol.writable());
        a.drive = Some(i);
        return Ok(summary);
    }
    Ok("没有装载 LTFS 卷的驱动器，仅持有预留".to_string())
}

/// 把文件服务里已完成的上传成批落带：一次挂载、逐个追加、一次提交，然后发布。
/// 返回 `Err(原因)` 表示本轮不能再继续（失去资格，或提交结果未定需要重新恢复）。
fn process_uploads(a: &mut Active, files: &FileService) -> std::result::Result<(), String> {
    let Some(i) = a.drive else {
        return Ok(());
    };
    while let Some((batch, uploads)) = files.take_batch(a.round) {
        let dev = a.devices[i].dev.as_ref();
        let result = (|| -> Result<u64> {
            let mut vol = LtfsVolume::mount(dev)?;
            vol.set_reservation_guard(Some(a.key));
            for u in &uploads {
                let mut f = std::io::BufReader::new(std::fs::File::open(&u.spool)?);
                vol.append_file(&u.path, &mut f)?;
            }
            vol.commit()?;
            Ok(vol.index().generation)
        })();
        match result {
            Ok(generation) => {
                info!("执行线程: 已提交 {} 个文件，索引 gen={}", uploads.len(), generation);
                files.batch_done(a.round, &batch, &uploads, Ok(generation));
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

/// 服务期间的周期工作。返回 `Err(原因)` 表示已失去执行资格。
fn periodic(a: &mut Active, opts: &ExecOptions) -> std::result::Result<(), String> {
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
