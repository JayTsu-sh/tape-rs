//! Raft 循环：驱动 raft-rs，应用已提交的控制命令，并据此指挥设备执行线程。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use raft::eraftpb::{Entry, EntryType, Message, Snapshot};
use raft::{Config, RawNode, StateRole, Storage as _};
use serde_json::json;
use slog::Drain;

use super::executor::{ExecEvent, ExecRequest};
use super::directory::{Directory, PoolRow};
use super::net::{AdminReply, DirectoryFetch, Network, NodeInput};
use super::state::{Applied, Command, ControlSnapshot, ControlState, Executor, split_catalog};
use super::store::{ControlStorage, RaftStore, SnapshotPolicy};
use crate::error::{Result, TapeError};

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub id: u64,
    pub peers: Vec<u64>,
    pub tick: Duration,
    pub election_ticks: usize,
    pub heartbeat_ticks: usize,
    /// 状态文件写到哪里；`None` 不写
    pub status_file: Option<PathBuf>,
    /// 本地目录库文件；`None` 不落库（测试里用）
    pub directory_file: Option<PathBuf>,
    /// 隔离失败或失去预留后，多久之内即使再次当选也不接管（直接让出领导权）
    pub cooldown: Duration,
    /// 多久做一次快照并压缩日志
    pub snapshot: SnapshotPolicy,
    /// 计划停机时最多等执行线程多久（落带、退带、释放预留）
    pub shutdown_grace: Duration,
}

/// 对外可见的节点状态快照。
#[derive(Debug, Clone, Default)]
pub struct NodeStatus {
    pub id: u64,
    pub role: String,
    pub term: u64,
    pub leader: u64,
    pub executor: Option<Executor>,
    /// 本节点执行线程的最新状态
    pub local: String,
    pub applied_index: u64,
    /// 池与磁带归属（来自已应用的日志；任何节点都可读，有界陈旧）
    pub pools: Vec<PoolRow>,
    /// 条码 → 概况
    pub tapes: std::collections::BTreeMap<String, TapeRow>,
}

/// 一盘带在控制状态里的概况。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapeRow {
    pub state: String,
    pub generation: u64,
    /// 索引里的文件数
    pub files: u64,
    /// 索引里这些文件的字节数（只算活着的）
    pub bytes_used: u64,
    /// 带上实际写掉的字节（容量读数）。减去 `bytes_used` 就是同一盘带上被重写的旧副本
    /// 和收尾放弃的块；再减去目录里还指向它的字节，才是完整的可回收空间。
    pub bytes_written: u64,
}

pub type SharedStatus = Arc<Mutex<NodeStatus>>;

fn raft_err(e: raft::Error) -> TapeError {
    TapeError::Ltfs(format!("raft: {}", e))
}

pub fn run(
    cfg: NodeConfig,
    store: RaftStore,
    net: Box<dyn Network>,
    inbox: Receiver<NodeInput>,
    exec: Sender<ExecRequest>,
    status: SharedStatus,
) -> Result<()> {
    run_with(cfg, store, net, None, None, inbox, exec, status)
}

/// `fetcher` 与 `self_tx` 一起启用目录库拉取：收到快照的节点在别的线程上把 Leader 的
/// 目录库取回来，结果经 `self_tx` 送回本循环。两者缺一则只恢复控制状态（测试里用）。
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    cfg: NodeConfig,
    mut store: RaftStore,
    net: Box<dyn Network>,
    fetcher: Option<Arc<dyn DirectoryFetch>>,
    self_tx: Option<Sender<NodeInput>>,
    inbox: Receiver<NodeInput>,
    exec: Sender<ExecRequest>,
    status: SharedStatus,
) -> Result<()> {
    let raft_cfg = Config {
        id: cfg.id,
        election_tick: cfg.election_ticks,
        heartbeat_tick: cfg.heartbeat_ticks,
        check_quorum: true,
        pre_vote: true,
        ..Default::default()
    };
    raft_cfg.validate().map_err(raft_err)?;
    let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), slog::o!());
    let storage = store.raft_storage();
    let mut node: RawNode<ControlStorage> = RawNode::new(&raft_cfg, storage.clone(), &logger).map_err(raft_err)?;

    // 日志压缩之后，快照点之前的条目已经不在了：控制状态从快照恢复，而不是从头重放。
    let mut ctl = ControlState::default();
    let mut applied = store.snapshot_index();
    if let Some(snap) = store.loaded_snapshot() {
        match ControlSnapshot::decode(snap.get_data()) {
            Some(s) => {
                info!("从索引 {} 的快照恢复控制状态", applied);
                ctl = s.control;
            }
            None => return Err(TapeError::Ltfs("快照里的控制状态无法解析".into())),
        }
    }
    let mut directory = match &cfg.directory_file {
        Some(p) => Some(Directory::open(p)?),
        None => None,
    };
    // 收到快照后还没把目录库取回来。取回之前本节点不接管：它的 stat/list 会漏报。
    // 等待期间**不往本地目录库写**：拉回来的那一份会整份盖上去，期间写进去的会连同基础一起没了。
    // 装入之后再从本地日志把缺口补齐，所以还要记着快照点和那一刻的控制状态。
    let mut directory_pending: Option<(u64, u64)> = None;
    let mut fetch_running = false;
    let mut snapshot_ctl: Option<(u64, ControlState)> = None;
    // 本节点提交的管理命令：请求号 → 回复通道。请求号放在日志条目的 context 里。
    let mut pending_admin: std::collections::HashMap<u64, std::sync::mpsc::Sender<AdminReply>> = Default::default();
    let mut next_admin: u64 = 1;
    // 本节点在哪个任期已经提交过 Takeover；以及当前作为执行者的轮次
    let mut claimed_term: Option<u64> = None;
    let mut my_round: Option<u64> = None;
    // 不能用"现在减去一个大数"表示很久以前：单调时钟从开机算起，刚开机的机器上会下溢 panic
    let mut last_propose: Option<Instant> = None;
    let mut proposed_term: Option<u64> = None;
    let mut cooldown_until = Instant::now();
    let mut local = String::from("空闲");
    let mut next_tick = Instant::now() + cfg.tick;
    let mut last_status = String::new();

    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(wait) {
            Ok(NodeInput::Shutdown) => {
                // 等执行线程把队列落带、退带、放掉预留，再让进程退出。等不到就照常退：
                // 不释放预留是安全的，下一个执行者会抢占。
                let (done, wait_done) = std::sync::mpsc::channel();
                if exec.send(ExecRequest::Shutdown { done: Some(done) }).is_ok()
                    && wait_done.recv_timeout(cfg.shutdown_grace).is_err()
                {
                    warn!("等执行线程停机超过 {:?}，直接退出；设备上的预留留给下一个执行者抢占", cfg.shutdown_grace);
                }
                info!("节点 {} 停机", cfg.id);
                return Ok(());
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = exec.send(ExecRequest::Shutdown { done: None });
                return Ok(());
            }
            Ok(NodeInput::Raft(m)) => {
                if let Err(e) = node.step(*m) {
                    warn!("raft step: {}", e);
                }
            }
            Ok(NodeInput::Admin { cmd, reply }) => {
                if node.raft.state != StateRole::Leader {
                    let _ = reply.send(AdminReply::NotLeader(node.raft.leader_id));
                } else {
                    let id = next_admin;
                    next_admin += 1;
                    match node.propose(id.to_be_bytes().to_vec(), cmd.encode()) {
                        Ok(()) => {
                            pending_admin.insert(id, reply);
                        }
                        Err(e) => {
                            let _ = reply.send(AdminReply::Rejected(format!("提交失败: {}", e)));
                        }
                    }
                }
            }
            Ok(NodeInput::Directory(result)) => {
                fetch_running = false;
                match result {
                    Ok((index, file)) => {
                        let Some(live) = cfg.directory_file.clone() else { continue };
                        // 换文件之前必须放掉旧连接，否则新库改名上去了，旧连接还在读旧 inode
                        drop(directory.take());
                        match Directory::install(&live, &file, index) {
                            Ok(mut d) => {
                                info!("目录库已同步到索引 {}", d.applied_index());
                                // 拉来的那一份停在对方做快照的时刻；这之后的条目本节点已经收到了，
                                // 从本地日志重放补上。`Directory::apply` 跳过已落库的索引，重叠无害。
                                if let Some((at, base)) = &snapshot_ctl
                                    && let Err(e) = replay_into(&storage, *at, applied, base, &mut d)
                                {
                                    error!("补齐目录库失败: {}", e);
                                }
                                directory = Some(d);
                                directory_pending = None;
                                snapshot_ctl = None;
                                local = format!("目录库已同步到索引 {}", applied);
                            }
                            Err(e) => {
                                // 本地库已经被删了：重开一个空的，等下一次拉取
                                error!("装入目录库失败: {}", e);
                                local = format!("装入目录库失败: {}", e);
                                directory = Directory::open(&live).ok();
                            }
                        }
                    }
                    Err(why) => {
                        warn!("拉取目录库失败: {}", why);
                        local = format!("拉取目录库失败: {}", why);
                    }
                }
            }
            Ok(NodeInput::Exec(ev)) => {
                let is_leader = node.raft.state == StateRole::Leader;
                match ev {
                    ExecEvent::Fenced { round } if my_round == Some(round) && is_leader => {
                        local = format!("轮次 {}：全部设备已隔离，等待多数派确认", round);
                        propose(&mut node, &Command::Fenced { node: cfg.id, round });
                    }
                    ExecEvent::Fenced { round } => {
                        info!("轮次 {} 的隔离结果已过时，忽略", round);
                        let _ = exec.send(ExecRequest::Stop { round: Some(round), reason: "隔离完成时已不是该轮的执行者".into() });
                    }
                    ExecEvent::Committed { round, barcode, volume_uuid, generation, files_total, bytes_used, bytes_written, files, full } => {
                        // 目录记录在卷提交成功之后才进日志。这里失败（不再是 Leader 等）没关系：
                        // 目录会落后于磁带，下次装载该带时按磁带对账补齐。
                        let known = ctl.tape_summary.get(&barcode).map(|t| t.generation).unwrap_or(0);
                        if my_round != Some(round) || !is_leader {
                            info!("{} 第 {} 代的目录记录未写入日志：已不是执行者", barcode, generation);
                        } else if full && known == generation {
                            // 装载时的完整列表：目录已经是这一代，不用重写
                        } else if full && known > generation {
                            // 目录比磁带还新：不应出现（磁带被回退或换了盘）。不改目录，标记该带待核验并告警
                            warn!("{} 目录是第 {} 代，磁带只有第 {} 代：标记待核验，不自动修改", barcode, known, generation);
                            propose(&mut node, &Command::TapeState { barcode, state: super::state::tape_state::CHECK.to_string() });
                        } else {
                            if full {
                                info!("{} 目录停在第 {} 代，磁带是第 {} 代：以磁带为准重写该带的目录", barcode, known, generation);
                            }
                            let parts = split_catalog(&files);
                            for (i, part) in parts.iter().enumerate() {
                                propose(&mut node, &Command::CatalogPart { barcode: barcode.clone(), generation, part: i as u32, files: part.clone() });
                            }
                            propose(
                                &mut node,
                                &Command::TapeCommitted { barcode, volume_uuid, generation, files: files_total, bytes_used, bytes_written, parts: parts.len() as u32, full },
                            );
                        }
                    }
                    ExecEvent::Reclaimed { round, barcode, volume_uuid, files, bytes } => {
                        if my_round == Some(round) && is_leader {
                            info!("磁带 {} 回收完毕：搬走 {} 个文件 / {} MiB，已重新格式化", barcode, files, bytes >> 20);
                            propose(&mut node, &Command::TapeReclaimed { barcode, volume_uuid });
                        } else {
                            // 状态还是 reclaiming，新执行者会重新走一遍：这时源带已经是空的，
                            // 核对通过、再格式化一次、重新上报。
                            warn!("{} 的回收结果未写入日志：已不是执行者，由下一任重做收尾", barcode);
                        }
                    }
                    ExecEvent::TapeState { round, barcode, state } => {
                        if my_round == Some(round) && is_leader {
                            info!("磁带 {} 状态变为 {}", barcode, state);
                            propose(&mut node, &Command::TapeState { barcode, state });
                        }
                    }
                    ExecEvent::Serving { round, summary } => {
                        info!("轮次 {} 开始服务：{}", round, summary);
                        local = format!("轮次 {}：服务中。{}", round, summary);
                    }
                    ExecEvent::FenceFailed { round, reason } | ExecEvent::Lost { round, reason } => {
                        error!("轮次 {} 放弃：{}", round, reason);
                        local = format!("轮次 {} 已放弃：{}", round, reason);
                        if my_round == Some(round) {
                            my_round = None;
                        }
                        cooldown_until = Instant::now() + cfg.cooldown;
                        if is_leader {
                            propose(&mut node, &Command::Abandoned { node: cfg.id, round, reason });
                            yield_leadership(&mut node, &cfg);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_tick {
            node.tick();
            next_tick = Instant::now() + cfg.tick;
        }

        let is_leader = node.raft.state == StateRole::Leader;
        let term = node.raft.term;
        if !is_leader {
            proposed_term = None;
            // 不再是 Leader：这些命令也许会被新 Leader 提交，也许不会。让请求方自己查询后决定。
            pending_admin.clear();
        }
        if let Some(r) = my_round.take_if(|_| !is_leader) {
            // 尽早停手。安全性不靠这一步：设备会拒绝陈旧的执行者。
            let _ = exec.send(ExecRequest::Stop { round: Some(r), reason: "不再是 Leader".into() });
            local = "已停手：不再是 Leader".into();
        }
        // 每个任期只提交一次 Takeover。Leader 身份不变时已追加的条目终会提交，重复提交只会
        // 制造多余的轮次；proposed_term 在失去 Leader 身份时清零。
        if is_leader && claimed_term != Some(term) {
            // 目录库还没同步回来就接管，stat/list 会漏报快照点之前提交的文件。让别人来。
            if Instant::now() < cooldown_until || directory_pending.is_some() {
                if last_propose.is_none_or(|t| t.elapsed() > cfg.tick * 5) {
                    yield_leadership(&mut node, &cfg);
                    last_propose = Some(Instant::now());
                }
            } else if proposed_term != Some(term) && propose(&mut node, &Command::Takeover { node: cfg.id, term }) {
                proposed_term = Some(term);
            }
        }

        let (installed, committed) = on_ready(&mut node, &mut store, net.as_ref())?;
        if let Some(snap) = installed {
            // 收到快照：本地日志整段作废，控制状态整体替换。目录库另行拉取。
            let index = snap.get_metadata().index;
            let Some(s) = ControlSnapshot::decode(snap.get_data()) else {
                return Err(TapeError::Ltfs(format!("索引 {} 的快照无法解析", index)));
            };
            info!("装入节点 {} 的快照，日志从索引 {} 起", s.source, index);
            ctl = s.control;
            applied = index;
            if let Some(r) = my_round.take() {
                let _ = exec.send(ExecRequest::Stop { round: Some(r), reason: "被快照取代".into() });
            }
            if directory.is_some() && s.directory_index > 0 {
                directory_pending = Some((s.source, s.directory_index));
                snapshot_ctl = Some((index, ctl.clone()));
                local = format!("正在从节点 {} 同步目录库（到索引 {}）", s.source, s.directory_index);
            }
        }
        for entry in committed {
            applied = applied.max(entry.index);
            if entry.get_entry_type() != EntryType::EntryNormal || entry.data.is_empty() {
                continue;
            }
            let Some(cmd) = Command::decode(&entry.data) else {
                warn!("日志索引 {} 无法解码，跳过", entry.index);
                continue;
            };
            let is_leader = node.raft.state == StateRole::Leader;
            let term = node.raft.term;
            let applied = ctl.apply(entry.index, &cmd);
            // 等目录库拉回来期间不写本地库：它马上要被整份替换，写进去的会一起丢掉
            if let Some(d) = directory.as_mut().filter(|_| directory_pending.is_none()) {
                d.apply(entry.index, &cmd, &applied)?;
            }
            let waiter = <[u8; 8]>::try_from(entry.context.as_ref()).ok().and_then(|b| pending_admin.remove(&u64::from_be_bytes(b)));
            if let Some(tx) = waiter {
                let _ = tx.send(match &applied {
                    Applied::Admin(Ok(s)) => AdminReply::Ok(s.clone()),
                    Applied::Admin(Err(e)) => AdminReply::Rejected(e.clone()),
                    _ => AdminReply::Ok("已应用".to_string()),
                });
            }
            match applied {
                Applied::NewExecutor(e) if e.node == cfg.id && is_leader && e.term == term => {
                    info!("本节点成为执行者，轮次 {}（任期 {}）", e.round, e.term);
                    claimed_term = Some(term);
                    my_round = Some(e.round);
                    local = format!("轮次 {}：正在隔离设备", e.round);
                    let _ = exec.send(ExecRequest::Takeover { round: e.round });
                }
                Applied::NewExecutor(e) => {
                    if e.node != cfg.id {
                        info!("节点 {} 成为执行者，轮次 {}", e.node, e.round);
                    }
                    if let Some(r) = my_round.take() {
                        let _ = exec.send(ExecRequest::Stop { round: Some(r), reason: format!("被轮次 {} 取代", e.round) });
                        local = format!("已停手：被节点 {} 轮次 {} 取代", e.node, e.round);
                    }
                }
                Applied::FenceConfirmed(e) if e.node == cfg.id && my_round == Some(e.round) && is_leader => {
                    local = format!("轮次 {}：隔离已获多数派确认，正在恢复卷", e.round);
                    let _ = exec.send(ExecRequest::Recover { round: e.round });
                }
                Applied::FenceSuperseded { node: n, round } if n == cfg.id => {
                    warn!("轮次 {} 在确认前已被取代，作废", round);
                    if my_round == Some(round) {
                        my_round = None;
                    }
                    let _ = exec.send(ExecRequest::Stop { round: Some(round), reason: "本轮在确认前已被取代".into() });
                }
                _ => {}
            }
        }

        // 日志压缩：攒够了、或者有落后的节点在等快照，就现做一份。
        // 自己的目录库还没同步完时不做：那份快照会把空洞传给别人。
        let asked = storage.wanted().filter(|w| applied >= *w).is_some();
        if (store.due(&cfg.snapshot) || asked)
            && applied > store.snapshot_index()
            && directory_pending.is_none()
            && let Err(e) = take_snapshot(&cfg, &mut store, &ctl, directory.as_ref(), applied)
        {
            warn!("做快照失败: {}", e);
        }

        // 目录库拉取：在别的线程上做，别挡住选举计时
        if let (Some((source, want)), Some(f), Some(tx), false) =
            (directory_pending, fetcher.as_ref(), self_tx.as_ref(), fetch_running)
            && let Some(live) = cfg.directory_file.as_ref()
        {
            fetch_running = true;
            let (f, tx, dest) = (f.clone(), tx.clone(), Directory::incoming_path(live));
            let _ = std::thread::Builder::new().name("ltfsd-dirfetch".into()).spawn(move || {
                let r = f.fetch(source, want, &dest).map(|i| (i, dest)).map_err(|e| e.to_string());
                let _ = tx.send(NodeInput::Directory(r));
            });
        }

        publish_status(&cfg, &node, &ctl, &local, &status, &mut last_status);
    }
}

/// 做一份快照并压缩日志。快照 = 控制状态（进 Raft 消息）+ 目录库文件（另行拉取）。
///
/// 快照点绝不超过目录库已落库的位置：收到这份快照的节点会把目录库整份换成配套的那一个文件，
/// 之间但凡差一条，那条的目录记录就永远补不回来了（它已经在快照覆盖范围内，不会再经日志送达）。
fn take_snapshot(
    cfg: &NodeConfig,
    store: &mut RaftStore,
    ctl: &ControlState,
    directory: Option<&Directory>,
    applied: u64,
) -> Result<()> {
    let mut at = applied;
    let mut directory_index = 0;
    if let (Some(d), Some(live)) = (directory, cfg.directory_file.as_ref()) {
        directory_index = d.applied_index();
        at = at.min(directory_index);
        if at <= store.snapshot_index() {
            return Ok(());
        }
        d.write_snapshot(&Directory::snapshot_path(live))?;
    }
    let data = ControlSnapshot { control: ctl.clone(), source: cfg.id, directory_index }.encode();
    let snap = store.build_snapshot(at, data)?;
    store.compact_to(&snap)?;
    info!("已做快照并压缩日志到索引 {}（目录库到 {}）", at, directory_index);
    Ok(())
}

/// 把 `(at, upto]` 这段日志重放进刚装好的目录库。`base` 是 `at` 处的控制状态：
/// 目录库要的是状态机对每条命令的结论，所以得在一份影子状态上重算一遍。
fn replay_into(
    storage: &ControlStorage,
    at: u64,
    upto: u64,
    base: &ControlState,
    directory: &mut Directory,
) -> Result<()> {
    if upto <= at {
        return Ok(());
    }
    let entries = storage
        .entries(at + 1, upto + 1, None, raft::GetEntriesContext::empty(false))
        .map_err(|e| TapeError::Ltfs(format!("取索引 {}..={} 的日志失败: {}", at + 1, upto, e)))?;
    let mut shadow = base.clone();
    let mut done = 0;
    for entry in &entries {
        if entry.get_entry_type() != EntryType::EntryNormal || entry.data.is_empty() {
            continue;
        }
        let Some(cmd) = Command::decode(&entry.data) else { continue };
        let outcome = shadow.apply(entry.index, &cmd);
        directory.apply(entry.index, &cmd, &outcome)?;
        done += 1;
    }
    info!("目录库从索引 {} 重放到 {}（{} 条命令）", at, upto, done);
    Ok(())
}

fn propose(node: &mut RawNode<ControlStorage>, cmd: &Command) -> bool {
    match node.propose(vec![], cmd.encode()) {
        Ok(()) => true,
        Err(e) => {
            warn!("提交 {:?} 失败: {}", cmd, e);
            false
        }
    }
}

/// 让出领导权：交给编号上的下一个节点。对方不可达时 raft-rs 会在选举超时后放弃移交。
fn yield_leadership(node: &mut RawNode<ControlStorage>, cfg: &NodeConfig) {
    let mut others: Vec<u64> = cfg.peers.iter().copied().filter(|&p| p != cfg.id).collect();
    others.sort();
    if let Some(&to) = others.iter().find(|&&p| p > cfg.id).or(others.first()) {
        info!("让出领导权给节点 {}", to);
        node.transfer_leader(to);
    }
}

/// raft-rs 的 Ready 处理。先持久化、再发送需要持久化之后才能发的消息。
/// 返回 (本轮装入的快照, 已提交的条目)。
fn on_ready(
    node: &mut RawNode<ControlStorage>,
    store: &mut RaftStore,
    net: &dyn Network,
) -> Result<(Option<Snapshot>, Vec<Entry>)> {
    let mut committed = Vec::new();
    if !node.has_ready() {
        return Ok((None, committed));
    }
    let send = |msgs: Vec<Message>| {
        for m in msgs {
            net.send(m);
        }
    };
    let mut ready = node.ready();
    send(ready.take_messages());
    // 快照要在日志之前落盘：它会让本地整段日志作废，之后的条目才接得上
    let mut installed = None;
    if !ready.snapshot().is_empty() {
        let snap = ready.snapshot().clone();
        store.install_snapshot(&snap)?;
        installed = Some(snap);
    }
    committed.extend(ready.take_committed_entries());
    store.append(ready.entries())?;
    if let Some(hs) = ready.hs() {
        store.set_hardstate(hs)?;
    }
    send(ready.take_persisted_messages());
    let mut light = node.advance(ready);
    if let Some(commit) = light.commit_index() {
        store.set_commit(commit)?;
    }
    send(light.take_messages());
    committed.extend(light.take_committed_entries());
    node.advance_apply();
    Ok((installed, committed))
}

fn publish_status(
    cfg: &NodeConfig,
    node: &RawNode<ControlStorage>,
    ctl: &ControlState,
    local: &str,
    shared: &SharedStatus,
    last: &mut String,
) {
    let st = NodeStatus {
        id: cfg.id,
        role: format!("{:?}", node.raft.state),
        term: node.raft.term,
        leader: node.raft.leader_id,
        executor: ctl.executor.clone(),
        local: local.to_string(),
        applied_index: ctl.applied_index,
        pools: ctl
            .pools
            .iter()
            .map(|(uuid, p)| PoolRow {
                uuid: uuid.clone(),
                name: p.name.clone(),
                file_limit: p.file_limit,
                tapes: ctl.tapes.iter().filter(|(_, u)| *u == uuid).map(|(b, _)| b.clone()).collect(),
            })
            .collect(),
        tapes: ctl
            .tapes
            .keys()
            .map(|b| {
                let state = ctl.tape_state.get(b).cloned().unwrap_or_else(|| super::state::tape_state::APPENDABLE.to_string());
                let su = ctl.tape_summary.get(b).cloned().unwrap_or_default();
                (
                    b.clone(),
                    TapeRow {
                        state,
                        generation: su.generation,
                        files: su.files,
                        bytes_used: su.bytes_used,
                        bytes_written: su.bytes_written,
                    },
                )
            })
            .collect(),
    };
    let text = json!({
        "id": st.id, "role": st.role, "term": st.term, "leader": st.leader,
        "executor": st.executor.as_ref().map(|e| json!({"node": e.node, "round": e.round, "term": e.term, "fenced": e.fenced})),
        "local": st.local, "applied_index": st.applied_index,
        "tapes": st.tapes.iter().map(|(b, t)| json!({"barcode": b, "state": t.state, "generation": t.generation, "files": t.files, "bytes_used": t.bytes_used, "bytes_written": t.bytes_written})).collect::<Vec<_>>(),
        "pools": st.pools.iter().map(|p| json!({"uuid": p.uuid, "name": p.name, "file_limit": p.file_limit, "tapes": p.tapes})).collect::<Vec<_>>(),
    })
    .to_string();
    // 共享结构每轮都刷新：执行线程换带时会直接往里塞一条"这盘带已满"（免得马上又选中它），
    // 那条记录的代数和文件数是占位的 0。`last` 只用来决定要不要重写状态文件，
    // 拿它挡住共享结构的刷新，占位值会一直留在那里对外显示。
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = st;
    if text == *last {
        return;
    }
    *last = text.clone();
    if let Some(path) = &cfg.status_file {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}
