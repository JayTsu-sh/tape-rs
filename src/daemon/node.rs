//! Raft 循环：驱动 raft-rs，应用已提交的控制命令，并据此指挥设备执行线程。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use raft::eraftpb::{Entry, EntryType, Message, Snapshot};
use raft::{Config, RawNode, StateRole, Storage as _};
use serde_json::json;
use slog::Drain;

use super::coordinator::Coordinator;
use super::directory::{Directory, PoolRow};
use super::executor::{ExecEvent, ExecRequest};
use super::files::FileService;
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
    /// 本节点配置的驱动器数。回收需要两台（一台放写入带、一台放源带），
    /// 不够时 Leader 在受理阶段就同步拒绝，不进日志。
    pub drives: usize,
    /// 新的控制/执行分离协议。None 仅保留原公开调用者的旧协议行为。
    pub coordination: Option<CoordinationConfig>,
}

#[derive(Debug, Clone)]
pub struct CoordinationConfig {
    /// 同一受管逻辑库的精确设备集合标识，各节点必须一致。
    pub group: String,
    /// 从 ReadIndex 请求发起到许可失效的最大时长，不改变任何设备超时。
    pub validity: Duration,
}

/// 对外可见的节点状态快照。
#[derive(Debug, Clone, Default)]
pub struct NodeStatus {
    pub management: Arc<serde_json::Value>,
    pub vips: std::collections::BTreeMap<std::net::Ipv4Addr, super::state::VipRecord>,
    pub id: u64,
    /// 已开始计划停机；HTTP VIP 应先撤下，设备许可仍可续持到安全收尾。
    pub shutting_down: bool,
    pub role: String,
    pub term: u64,
    pub leader: u64,
    pub executor: Option<Executor>,
    pub executor_ready: bool,
    pub catalog_quiesce_index: Option<u64>,
    pub catalog_frozen_index: Option<u64>,
    /// 本库正在原子合并大目录；数据入口暂缓，设备许可继续续期。
    pub catalog_pending: bool,
    pub candidates: std::collections::BTreeMap<u64, super::state::Candidate>,
    /// 本节点执行线程的最新状态
    pub local: String,
    pub applied_index: u64,
    /// 池与磁带归属（来自已应用的日志；任何节点都可读，有界陈旧）
    pub pools: Vec<PoolRow>,
    /// 条码 → 概况
    pub tapes: std::collections::BTreeMap<String, TapeRow>,
    /// 本节点配置的驱动器数
    pub drives: usize,
    /// 本节点执行线程最近一次回收的结果（做完或放弃）。节点本地事实，换届后清空。
    pub last_reclaim: Option<ReclaimRecord>,
}

/// 最近一次回收的结果。`outcome` 是 `done` 或 `abandoned`，`detail` 是搬迁量或放弃原因。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimRecord {
    pub round: u64,
    pub barcode: String,
    pub outcome: String,
    pub detail: String,
}

/// 一盘带在控制状态里的概况。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapeRow {
    pub volume_uuid: String,
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
    store: RaftStore,
    net: Box<dyn Network>,
    fetcher: Option<Arc<dyn DirectoryFetch>>,
    self_tx: Option<Sender<NodeInput>>,
    inbox: Receiver<NodeInput>,
    exec: Sender<ExecRequest>,
    status: SharedStatus,
) -> Result<()> {
    run_with_catalog(cfg, store, net, fetcher, self_tx, inbox, exec, status, None)
}

/// 生产入口把已应用的目录切换屏障同步到文件服务；旧测试入口仍可独立驱动状态机。
#[allow(clippy::too_many_arguments)]
pub fn run_with_catalog(
    cfg: NodeConfig,
    mut store: RaftStore,
    net: Box<dyn Network>,
    fetcher: Option<Arc<dyn DirectoryFetch>>,
    self_tx: Option<Sender<NodeInput>>,
    inbox: Receiver<NodeInput>,
    exec: Sender<ExecRequest>,
    status: SharedStatus,
    catalog_files: Option<Arc<FileService>>,
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
    let mut node: RawNode<ControlStorage> =
        RawNode::new(&raft_cfg, storage.clone(), &logger).map_err(raft_err)?;

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
    if let Some(files) = catalog_files.as_ref() {
        files.set_catalog_quiesced(ctl.catalog_quiesce_index.is_some());
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
    let mut pending_admin: std::collections::HashMap<u64, std::sync::mpsc::Sender<AdminReply>> =
        Default::default();
    let mut next_admin: u64 = 1;
    // 本节点在哪个任期已经提交过 Takeover；以及当前作为执行者的轮次
    let mut claimed_term: Option<u64> = None;
    let mut my_round: Option<u64> = None;
    // 不能用"现在减去一个大数"表示很久以前：单调时钟从开机算起，刚开机的机器上会下溢 panic
    let mut last_propose: Option<Instant> = None;
    let mut proposed_term: Option<u64> = None;
    let mut cooldown_until = Instant::now();
    let mut local = String::from("空闲");
    let mut last_reclaim: Option<ReclaimRecord> = None;
    let mut next_tick = Instant::now() + cfg.tick;
    let mut last_status = String::new();
    let mut shutdown: Option<(Instant, Receiver<()>)> = None;
    let mut coordinator = cfg
        .coordination
        .clone()
        .map(|c| Coordinator::new(cfg.id, store.next_instance()?, c, cfg.cooldown, &exec))
        .transpose()?;

    loop {
        if let Some((deadline, done)) = &shutdown {
            let finished = matches!(done.try_recv(), Ok(()) | Err(TryRecvError::Disconnected));
            let vip_held = ctl
                .vips
                .values()
                .any(|r| r.holder.as_ref().is_some_and(|h| h.node == cfg.id));
            if finished && !vip_held {
                info!("节点 {} 停机", cfg.id);
                return Ok(());
            }
            if Instant::now() >= *deadline {
                warn!(
                    "等待执行线程停机或 VIP 撤址确认超过 {:?}，直接退出；保留未确认的 VIP 占用记录和设备预留",
                    cfg.shutdown_grace
                );
                return Ok(());
            }
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(wait) {
            Ok(NodeInput::Library { .. }) => {
                warn!("单库节点拒绝多库输入");
            }
            Ok(NodeInput::Shutdown) => {
                if shutdown.is_some() {
                    continue;
                }
                status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shutting_down = true;
                // 等待期间仍驱动 Raft/ReadIndex，长机械操作不能阻塞多数派许可续期。
                // 宽限耗尽时退出且保留预留，下一任仍须按正常协议隔离。
                let (done, wait_done) = std::sync::mpsc::channel();
                if exec
                    .send(ExecRequest::Shutdown { done: Some(done) })
                    .is_err()
                {
                    return Ok(());
                }
                shutdown = Some((Instant::now() + cfg.shutdown_grace, wait_done));
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
            Ok(NodeInput::Vip(cmd)) => {
                let valid = match &cmd {
                    Command::VipAcquire { node, .. } => *node == cfg.id && shutdown.is_none(),
                    Command::VipRelease { node, .. } => *node == cfg.id,
                    _ => false,
                };
                if valid && coordinator.is_some() {
                    propose(&mut node, &cmd);
                }
            }
            Ok(NodeInput::Admin { cmd, reply }) => {
                if matches!(cmd, Command::VipAcquire { .. } | Command::VipRelease { .. }) {
                    let _ = reply.send(AdminReply::Rejected(
                        "VIP 控制命令仅接受本机网络线程".into(),
                    ));
                } else if shutdown.is_some() {
                    let _ = reply.send(AdminReply::Rejected(
                        "节点正在停机，不接受新的管理操作".into(),
                    ));
                } else if node.raft.state != StateRole::Leader {
                    let _ = reply.send(AdminReply::NotLeader(node.raft.leader_id));
                } else if let Command::TapeReclaim { barcode } = &cmd
                    && (if coordinator.is_some() {
                        ctl.executor
                            .as_ref()
                            .and_then(|e| ctl.candidates.get(&e.node))
                            .map_or(0, |c| c.drives)
                    } else {
                        cfg.drives
                    }) < 2
                {
                    // 与 EE 一致（GLESL154E）：资源不够是命令期失败，磁带状态不动。
                    // 驱动器数是本节点的事实，进不了状态机，所以在提案之前拦下
                    let _ = reply.send(AdminReply::Rejected(format!(
                        "回收 {} 需要两台驱动器（一台放写入带，一台放源带），当前设备 owner 资源不足",
                        barcode
                    )));
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
                        let Some(live) = cfg.directory_file.clone() else {
                            continue;
                        };
                        // 换文件之前必须放掉旧连接，否则新库改名上去了，旧连接还在读旧 inode
                        drop(directory.take());
                        match Directory::install(&live, &file, index) {
                            Ok(mut d) => {
                                info!("目录库已同步到索引 {}", d.applied_index());
                                // 拉来的那一份停在对方做快照的时刻；这之后的条目本节点已经收到了，
                                // 从本地日志重放补上。`Directory::apply` 跳过已落库的索引，重叠无害。
                                if let Some((at, base)) = &snapshot_ctl
                                    && let Err(e) =
                                        replay_into(&storage, *at, applied, base, &mut d)
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
                if let Some(c) = coordinator.as_mut() {
                    c.event(ev, &mut node, &ctl, &mut local);
                } else {
                    let is_leader = node.raft.state == StateRole::Leader;
                    match ev {
                        ExecEvent::Management { round, command } => {
                            if my_round == Some(round) && is_leader {
                                propose(&mut node, &command);
                            }
                        }
                        ExecEvent::FileTask {
                            task,
                            round,
                            pool,
                            path,
                            status,
                            generation,
                            reason,
                        } => {
                            if my_round == Some(round) && is_leader {
                                propose(
                                    &mut node,
                                    &Command::FileTask {
                                        task,
                                        round,
                                        pool,
                                        path,
                                        status,
                                        generation,
                                        reason,
                                    },
                                );
                            }
                        }
                        ExecEvent::Fenced { round } if my_round == Some(round) && is_leader => {
                            local = format!("轮次 {}：全部设备已隔离，等待多数派确认", round);
                            propose(
                                &mut node,
                                &Command::Fenced {
                                    node: cfg.id,
                                    round,
                                },
                            );
                        }
                        ExecEvent::Fenced { round } => {
                            info!("轮次 {} 的隔离结果已过时，忽略", round);
                            let _ = exec.send(ExecRequest::Stop {
                                round: Some(round),
                                reason: "隔离完成时已不是该轮的执行者".into(),
                            });
                        }
                        ExecEvent::Committed {
                            round,
                            barcode,
                            volume_uuid,
                            generation,
                            files_total,
                            bytes_used,
                            bytes_written,
                            files,
                            full,
                        } => {
                            // 目录记录在卷提交成功之后才进日志。这里失败（不再是 Leader 等）没关系：
                            // 目录会落后于磁带，下次装载该带时按磁带对账补齐。
                            let known = ctl
                                .tape_summary
                                .get(&barcode)
                                .map(|t| t.generation)
                                .unwrap_or(0);
                            if my_round != Some(round) || !is_leader {
                                info!(
                                    "{} 第 {} 代的目录记录未写入日志：已不是执行者",
                                    barcode, generation
                                );
                            } else if full && known > generation {
                                // 目录比磁带还新：不应出现（磁带被回退或换了盘）。不改目录，标记该带待核验并告警
                                warn!(
                                    "{} 目录是第 {} 代，磁带只有第 {} 代：标记待核验，不自动修改",
                                    barcode, known, generation
                                );
                                propose(
                                    &mut node,
                                    &Command::TapeState {
                                        barcode,
                                        state: super::state::tape_state::CHECK.to_string(),
                                    },
                                );
                            } else {
                                if full {
                                    // 同代也刷新：旧版 catalog 可能缺少原生空目录记录。
                                    info!(
                                        "{} 目录停在第 {} 代，磁带是第 {} 代：以磁带为准重写该带的目录",
                                        barcode, known, generation
                                    );
                                }
                                let parts = split_catalog(&files);
                                for (i, part) in parts.iter().enumerate() {
                                    propose(
                                        &mut node,
                                        &Command::CatalogPart {
                                            barcode: barcode.clone(),
                                            generation,
                                            part: i as u32,
                                            files: part.clone(),
                                        },
                                    );
                                }
                                propose(
                                    &mut node,
                                    &Command::TapeCommitted {
                                        barcode,
                                        volume_uuid,
                                        generation,
                                        files: files_total,
                                        bytes_used,
                                        bytes_written,
                                        parts: parts.len() as u32,
                                        full,
                                    },
                                );
                            }
                        }
                        ExecEvent::Reclaimed {
                            round,
                            barcode,
                            volume_uuid,
                            files,
                            bytes,
                        } => {
                            last_reclaim = Some(ReclaimRecord {
                                round,
                                barcode: barcode.clone(),
                                outcome: "done".into(),
                                detail: format!(
                                    "搬走 {} 个文件 / {} MiB，已重新格式化",
                                    files,
                                    bytes >> 20
                                ),
                            });
                            if my_round == Some(round) && is_leader {
                                info!(
                                    "磁带 {} 回收完毕：搬走 {} 个文件 / {} MiB，已重新格式化",
                                    barcode,
                                    files,
                                    bytes >> 20
                                );
                                propose(
                                    &mut node,
                                    &Command::TapeReclaimed {
                                        barcode,
                                        volume_uuid,
                                    },
                                );
                            } else {
                                // 状态仍为 reformatting，新执行者依据持久化的迁出核验，
                                // 再完成初始化并上报；标签尚未形成也可接续。
                                warn!(
                                    "{} 的回收结果未写入日志：已不是执行者，由下一任重做收尾",
                                    barcode
                                );
                            }
                        }
                        ExecEvent::TapeState {
                            round,
                            barcode,
                            state,
                        } => {
                            if my_round == Some(round) && is_leader {
                                info!("磁带 {} 状态变为 {}", barcode, state);
                                propose(&mut node, &Command::TapeState { barcode, state });
                            }
                        }
                        ExecEvent::ReclaimAbandoned {
                            round,
                            barcode,
                            reason,
                            state,
                        } => {
                            last_reclaim = Some(ReclaimRecord {
                                round,
                                barcode: barcode.clone(),
                                outcome: "abandoned".into(),
                                detail: reason.clone(),
                            });
                            if my_round == Some(round) && is_leader {
                                warn!(
                                    "磁带 {} 的回收已放弃（{}），状态改为 {}",
                                    barcode, reason, state
                                );
                                propose(&mut node, &Command::TapeState { barcode, state });
                            }
                        }
                        ExecEvent::Serving { round, summary } => {
                            info!("轮次 {} 开始服务：{}", round, summary);
                            local = format!("轮次 {}：服务中。{}", round, summary);
                        }
                        ExecEvent::FenceFailed { round, reason }
                        | ExecEvent::Lost { round, reason } => {
                            error!("轮次 {} 放弃：{}", round, reason);
                            local = format!("轮次 {} 已放弃：{}", round, reason);
                            last_reclaim = None;
                            if my_round == Some(round) {
                                my_round = None;
                            }
                            cooldown_until = Instant::now() + cfg.cooldown;
                            if is_leader {
                                propose(
                                    &mut node,
                                    &Command::Abandoned {
                                        node: cfg.id,
                                        round,
                                        reason,
                                    },
                                );
                                yield_leadership(&mut node, &cfg);
                            }
                        }
                        ExecEvent::Capability { .. } => {}
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
        if coordinator.is_none()
            && let Some(r) = my_round.take_if(|_| !is_leader)
        {
            // 尽早停手。安全性不靠这一步：设备会拒绝陈旧的执行者。
            let _ = exec.send(ExecRequest::Stop {
                round: Some(r),
                reason: "不再是 Leader".into(),
            });
            local = "已停手：不再是 Leader".into();
        }
        // 每个任期只提交一次 Takeover。Leader 身份不变时已追加的条目终会提交，重复提交只会
        // 制造多余的轮次；proposed_term 在失去 Leader 身份时清零。
        if coordinator.is_none() && is_leader && claimed_term != Some(term) {
            // 目录库还没同步回来就接管，stat/list 会漏报快照点之前提交的文件。让别人来。
            if Instant::now() < cooldown_until || directory_pending.is_some() {
                if last_propose.is_none_or(|t| t.elapsed() > cfg.tick * 5) {
                    yield_leadership(&mut node, &cfg);
                    last_propose = Some(Instant::now());
                }
            } else if proposed_term != Some(term)
                && propose(&mut node, &Command::Takeover { node: cfg.id, term })
            {
                proposed_term = Some(term);
            }
        }

        if let Some(c) = coordinator.as_mut() {
            c.tick(
                &mut node,
                &ctl,
                directory_pending.is_none(),
                &exec,
                &mut local,
            );
        }
        let (installed, committed, reads) = on_ready(&mut node, &mut store, net.as_ref())?;
        if let Some(snap) = installed {
            // 收到快照：本地日志整段作废，控制状态整体替换。目录库另行拉取。
            let index = snap.get_metadata().index;
            let Some(s) = ControlSnapshot::decode(snap.get_data()) else {
                return Err(TapeError::Ltfs(format!("索引 {} 的快照无法解析", index)));
            };
            info!("装入节点 {} 的快照，日志从索引 {} 起", s.source, index);
            ctl = s.control;
            if let Some(files) = catalog_files.as_ref() {
                files.set_catalog_quiesced(ctl.catalog_quiesce_index.is_some());
            }
            applied = index;
            if let Some(r) = my_round.take() {
                let _ = exec.send(ExecRequest::Stop {
                    round: Some(r),
                    reason: "被快照取代".into(),
                });
            }
            if directory.is_some() && s.directory_index > 0 {
                directory_pending = Some((s.source, s.directory_index));
                snapshot_ctl = Some((index, ctl.clone()));
                local = format!(
                    "正在从节点 {} 同步目录库（到索引 {}）",
                    s.source, s.directory_index
                );
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
            if let Some(files) = catalog_files.as_ref() {
                files.set_catalog_quiesced(ctl.catalog_quiesce_index.is_some());
            }
            // 等目录库拉回来期间不写本地库：它马上要被整份替换，写进去的会一起丢掉
            if let Some(d) = directory.as_mut().filter(|_| directory_pending.is_none()) {
                d.apply(entry.index, &cmd, &applied)?;
            }
            let waiter = <[u8; 8]>::try_from(entry.context.as_ref())
                .ok()
                .and_then(|b| pending_admin.remove(&u64::from_be_bytes(b)));
            if let Some(tx) = waiter {
                let _ = tx.send(match &applied {
                    Applied::Admin(Ok(s)) => AdminReply::Ok(s.clone()),
                    Applied::Admin(Err(e)) => AdminReply::Rejected(e.clone()),
                    Applied::CatalogRejected => {
                        AdminReply::Rejected("旧目录已暂停或冻结，拒绝新的变更".into())
                    }
                    _ => AdminReply::Ok("已应用".to_string()),
                });
            }
            if let Some(c) = coordinator.as_mut() {
                c.observe(&ctl, directory_pending.is_none());
                continue;
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
                        let _ = exec.send(ExecRequest::Stop {
                            round: Some(r),
                            reason: format!("被轮次 {} 取代", e.round),
                        });
                        local = format!("已停手：被节点 {} 轮次 {} 取代", e.node, e.round);
                    }
                }
                Applied::FenceConfirmed(e)
                    if e.node == cfg.id && my_round == Some(e.round) && is_leader =>
                {
                    local = format!("轮次 {}：隔离已获多数派确认，正在恢复卷", e.round);
                    let _ = exec.send(ExecRequest::Recover { round: e.round });
                }
                Applied::FenceSuperseded { node: n, round } if n == cfg.id => {
                    warn!("轮次 {} 在确认前已被取代，作废", round);
                    if my_round == Some(round) {
                        my_round = None;
                    }
                    let _ = exec.send(ExecRequest::Stop {
                        round: Some(round),
                        reason: "本轮在确认前已被取代".into(),
                    });
                }
                _ => {}
            }
        }
        // Confirm control application only after the snapshot and entries are applied.
        node.advance_apply_to(applied);
        if let Some(c) = coordinator.as_mut() {
            c.confirm(reads, &node, &ctl, applied, directory_pending.is_none());
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
        if let (Some((source, want)), Some(f), Some(tx), false) = (
            directory_pending,
            fetcher.as_ref(),
            self_tx.as_ref(),
            fetch_running,
        ) && let Some(live) = cfg.directory_file.as_ref()
        {
            fetch_running = true;
            let (f, tx, dest) = (f.clone(), tx.clone(), Directory::incoming_path(live));
            let _ = std::thread::Builder::new()
                .name("ltfsd-dirfetch".into())
                .spawn(move || {
                    let r = f
                        .fetch(source, want, &dest)
                        .map(|i| (i, dest))
                        .map_err(|e| e.to_string());
                    let _ = tx.send(NodeInput::Directory(r));
                });
        }

        publish_status(
            &cfg,
            &node,
            &ctl,
            &local,
            &last_reclaim,
            &status,
            &mut last_status,
        );
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
    let data = ControlSnapshot {
        control: ctl.clone(),
        source: cfg.id,
        directory_index,
    }
    .encode();
    let snap = store.build_snapshot(at, data)?;
    store.compact_to(&snap)?;
    info!(
        "已做快照并压缩日志到索引 {}（目录库到 {}）",
        at, directory_index
    );
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
        .entries(
            at + 1,
            upto + 1,
            None,
            raft::GetEntriesContext::empty(false),
        )
        .map_err(|e| TapeError::Ltfs(format!("取索引 {}..={} 的日志失败: {}", at + 1, upto, e)))?;
    let mut shadow = base.clone();
    let mut done = 0;
    for entry in &entries {
        if entry.get_entry_type() != EntryType::EntryNormal || entry.data.is_empty() {
            continue;
        }
        let Some(cmd) = Command::decode(&entry.data) else {
            continue;
        };
        let outcome = shadow.apply(entry.index, &cmd);
        directory.apply(entry.index, &cmd, &outcome)?;
        done += 1;
    }
    info!("目录库从索引 {} 重放到 {}（{} 条命令）", at, upto, done);
    Ok(())
}

pub(super) fn propose(node: &mut RawNode<ControlStorage>, cmd: &Command) -> bool {
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
pub(super) fn on_ready(
    node: &mut RawNode<ControlStorage>,
    store: &mut RaftStore,
    net: &dyn Network,
) -> Result<(Option<Snapshot>, Vec<Entry>, Vec<raft::ReadState>)> {
    let mut committed = Vec::new();
    if !node.has_ready() {
        return Ok((None, committed, Vec::new()));
    }
    let send = |msgs: Vec<Message>| {
        for m in msgs {
            net.send(m);
        }
    };
    let mut ready = node.ready();
    let reads = ready.take_read_states();
    send(ready.take_messages());
    // 同一 Ready 的快照、后续日志和完整 HardState 必须原子持久化。
    let installed = (!ready.snapshot().is_empty()).then(|| ready.snapshot().clone());
    store.persist_ready(installed.as_ref(), ready.entries(), ready.hs())?;
    committed.extend(ready.take_committed_entries());
    send(ready.take_persisted_messages());
    let mut light = node.advance_append(ready);
    if let Some(commit) = light.commit_index() {
        store.set_commit(commit)?;
    }
    send(light.take_messages());
    committed.extend(light.take_committed_entries());
    Ok((installed, committed, reads))
}

pub(super) fn publish_status(
    cfg: &NodeConfig,
    node: &RawNode<ControlStorage>,
    ctl: &ControlState,
    local: &str,
    last_reclaim: &Option<ReclaimRecord>,
    shared: &SharedStatus,
    last: &mut String,
) {
    let (shutting_down, catalog_pending) = {
        let previous = shared.lock().unwrap_or_else(|e| e.into_inner());
        (previous.shutting_down, previous.catalog_pending)
    };
    let st = NodeStatus {
        management: {
            let previous = shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .management
                .clone();
            if previous["revision"] == ctl.management["revision"]
                && (ctl.management.is_null() || ctl.management["revision"].is_number())
            {
                previous
            } else {
                Arc::new(ctl.management.clone())
            }
        },
        vips: ctl.vips.clone(),
        id: cfg.id,
        shutting_down,
        drives: cfg.drives,
        last_reclaim: last_reclaim.clone(),
        role: format!("{:?}", node.raft.state),
        term: node.raft.term,
        leader: node.raft.leader_id,
        executor: ctl.executor.clone(),
        executor_ready: ctl.executor_ready,
        catalog_quiesce_index: ctl.catalog_quiesce_index,
        catalog_frozen_index: ctl.catalog_frozen_index,
        catalog_pending,
        candidates: ctl.candidates.clone(),
        local: local.to_string(),
        applied_index: ctl.applied_index,
        pools: ctl
            .pools
            .iter()
            .map(|(uuid, p)| PoolRow {
                uuid: uuid.clone(),
                name: p.name.clone(),
                file_limit: p.file_limit,
                tapes: ctl
                    .tapes
                    .iter()
                    .filter(|(_, u)| *u == uuid)
                    .map(|(b, _)| b.clone())
                    .collect(),
            })
            .collect(),
        tapes: ctl
            .tapes
            .keys()
            .map(|b| {
                let state = ctl
                    .tape_state
                    .get(b)
                    .cloned()
                    .unwrap_or_else(|| super::state::tape_state::APPENDABLE.to_string());
                let su = ctl.tape_summary.get(b).cloned().unwrap_or_default();
                (
                    b.clone(),
                    TapeRow {
                        volume_uuid: su.volume_uuid,
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
        "shutting_down": st.shutting_down,
        "vips": st.vips.iter().map(|(a,r)| json!({"address":a.to_string(),"token":r.token,"holder":r.holder.as_ref().map(|h|json!({"node":h.node,"round":h.round}))})).collect::<Vec<_>>(),
        "executor": st.executor.as_ref().map(|e| json!({"node": e.node, "round": e.round, "term": e.term, "fenced": e.fenced})),
        "executor_ready": st.executor_ready,
        "candidates": st.candidates.iter().map(|(id,c)| json!({"node": id,"instance":c.instance,"sequence":c.sequence,"group":c.group,"drives":c.drives,"eligible":c.eligible})).collect::<Vec<_>>(),
        "local": st.local, "applied_index": st.applied_index, "drives": st.drives,
        "last_reclaim": st.last_reclaim.as_ref().map(|r| json!({"round": r.round, "barcode": r.barcode, "outcome": r.outcome, "detail": r.detail})),
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

#[cfg(test)]
mod ready_apply_tests {
    use super::*;
    use raft::eraftpb::{ConfState, HardState, MessageType};

    struct Sink;
    impl Network for Sink {
        fn send(&self, _: Message) {}
    }

    fn entry(index: u64, term: u64) -> Entry {
        Entry {
            index,
            term,
            data: Command::PoolCreate {
                uuid: format!("p{index}"),
                name: format!("p{index}"),
                file_limit: 100,
            }
            .encode()
            .into(),
            ..Default::default()
        }
    }

    fn follower(root: &std::path::Path) -> (RaftStore, RawNode<ControlStorage>) {
        std::fs::create_dir(root).unwrap();
        let mut store = RaftStore::open(&root.join("raft.db"), &[1, 2, 3]).unwrap();
        store.append(&[entry(1, 7), entry(2, 7)]).unwrap();
        store
            .set_hardstate(&HardState {
                term: 7,
                vote: 2,
                commit: 2,
                ..Default::default()
            })
            .unwrap();
        let node = RawNode::new(
            &Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &slog::Logger::root(slog::Discard, slog::o!()),
        )
        .unwrap();
        (store, node)
    }

    #[test]
    fn ready_apply_waits_for_single_library_control_and_directory() {
        let root = std::env::temp_dir().join(format!("ready-apply-{}", uuid::Uuid::new_v4()));
        let (mut store, mut node) = follower(&root);
        let mut control = ControlState::default();
        let mut directory = Directory::open(&root.join("directory.db")).unwrap();
        let (_, entries, _) = on_ready(&mut node, &mut store, &Sink).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(node.raft.raft_log.applied, 0, "Ready 交付不等于实际应用");
        for entry in entries {
            let command = Command::decode(&entry.data).unwrap();
            let outcome = control.apply(entry.index, &command);
            directory.apply(entry.index, &command, &outcome).unwrap();
            assert!(node.raft.raft_log.applied < entry.index);
            node.advance_apply_to(entry.index);
        }
        assert_eq!(control.applied_index, 2);
        assert_eq!(directory.applied_index(), 2);
        assert_eq!(node.raft.raft_log.applied, 2);
        let (snapshot, entries, reads) = on_ready(&mut node, &mut store, &Sink).unwrap();
        assert!(snapshot.is_none() && entries.is_empty() && reads.is_empty());
        assert_eq!(node.raft.raft_log.applied, 2, "空 Ready 不改变应用位置");
        drop((directory, node, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ready_apply_waits_for_light_ready_entries_including_the_leader_noop() {
        let root = std::env::temp_dir().join(format!("light-apply-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut store = RaftStore::open(&root.join("raft.db"), &[1]).unwrap();
        let mut node = RawNode::new(
            &Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &slog::Logger::root(slog::Discard, slog::o!()),
        )
        .unwrap();
        node.campaign().unwrap();
        node.propose(vec![], entry(2, 1).data.to_vec()).unwrap();
        assert_eq!(
            node.raft.raft_log.committed, 0,
            "只有持久化后才产生本轮 LightReady 提交"
        );
        let (_, entries, _) = on_ready(&mut node, &mut store, &Sink).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(entries[0].data.is_empty());
        assert_eq!(
            node.raft.raft_log.applied, 0,
            "LightReady 也必须等待实际应用"
        );
        let mut control = ControlState::default();
        for entry in entries {
            if let Some(command) = Command::decode(&entry.data) {
                control.apply(entry.index, &command);
            }
            node.advance_apply_to(entry.index);
        }
        assert_eq!(control.applied_index, 2);
        assert_eq!(node.raft.raft_log.applied, 2);
        node.propose(vec![], entry(3, 1).data.to_vec()).unwrap();
        let (_, entries, _) = on_ready(&mut node, &mut store, &Sink).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(node.raft.raft_log.applied, 2);
        control.apply(3, &Command::decode(&entries[0].data).unwrap());
        node.advance_apply_to(3);
        assert_eq!(node.raft.raft_log.applied, control.applied_index);
        drop((node, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ready_apply_waits_for_received_control_snapshot() {
        snapshot_case(false);
    }

    #[test]
    fn ready_apply_waits_for_received_snapshot_and_following_entries() {
        snapshot_case(true);
    }

    fn snapshot_case(with_entries: bool) {
        let root = std::env::temp_dir().join(format!("snapshot-apply-{}", uuid::Uuid::new_v4()));
        let (mut store, mut node) = follower(&root);
        let mut base = ControlState::default();
        base.apply(50, &Command::decode(&entry(50, 7).data).unwrap());
        let mut snapshot = Snapshot::default();
        snapshot.mut_metadata().index = 50;
        snapshot.mut_metadata().term = 7;
        snapshot
            .mut_metadata()
            .set_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
        snapshot.set_data(
            ControlSnapshot {
                control: base,
                source: 2,
                directory_index: 50,
            }
            .encode()
            .into(),
        );
        node.step(Message {
            msg_type: MessageType::MsgSnapshot,
            from: 2,
            to: 1,
            term: 8,
            snapshot: Some(snapshot).into(),
            ..Default::default()
        })
        .unwrap();
        if with_entries {
            let mut message = Message {
                msg_type: MessageType::MsgAppend,
                from: 2,
                to: 1,
                term: 8,
                index: 50,
                log_term: 7,
                commit: 52,
                ..Default::default()
            };
            message.set_entries(vec![entry(51, 8), entry(52, 8)].into());
            node.step(message).unwrap();
        }
        let (snapshot, entries, _) = on_ready(&mut node, &mut store, &Sink).unwrap();
        assert_eq!(
            node.raft.raft_log.applied, 0,
            "持久化快照不能代替控制状态恢复"
        );
        let snapshot = snapshot.unwrap();
        let restored = ControlSnapshot::decode(snapshot.get_data()).unwrap();
        let mut control = restored.control;
        let directory = Directory::open(&root.join("directory.db")).unwrap();
        let directory_pending = directory.applied_index() < restored.directory_index;
        node.advance_apply_to(snapshot.get_metadata().index);
        assert_eq!(node.raft.raft_log.applied, control.applied_index);
        assert_eq!(node.raft.raft_log.applied, 50);
        assert!(
            directory_pending,
            "控制恢复后目录仍须独立拉取，不能称目录已应用"
        );
        for entry in entries {
            control.apply(entry.index, &Command::decode(&entry.data).unwrap());
            node.advance_apply_to(entry.index);
        }
        assert_eq!(
            node.raft.raft_log.applied,
            if with_entries { 52 } else { 50 }
        );
        assert_eq!(directory.applied_index(), 0);
        drop((directory, node, store));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod ready_persistence_tests {
    use super::*;
    use protobuf::Message as _;
    use raft::eraftpb::{ConfState, HardState, MessageType};
    struct RecordingNetwork {
        messages: Mutex<Vec<Message>>,
        path: PathBuf,
    }
    impl Network for RecordingNetwork {
        fn send(&self, message: Message) {
            let db = rusqlite::Connection::open(&self.path).unwrap();
            let bytes: Vec<u8> = db
                .query_row("SELECT data FROM hardstate WHERE id=1", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let hs = HardState::parse_from_bytes(&bytes).unwrap();
            let entries: u64 = db
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE idx IN(51,52)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                (hs.term, hs.vote, hs.commit, entries),
                (8, 0, 52, 2),
                "成功回复前必须读到完整 Ready 的持久状态"
            );
            self.messages.lock().unwrap().push(message);
        }
    }
    #[test]
    fn a_ready_with_snapshot_and_entries_rolls_back_when_an_entry_write_fails() {
        ready_case(Some("entry"));
    }
    #[test]
    fn a_ready_with_snapshot_and_entries_rolls_back_when_hardstate_write_fails() {
        ready_case(Some("hardstate"));
    }
    #[test]
    fn a_complete_ready_is_durable_before_success_replies_and_survives_reopen() {
        ready_case(None);
    }
    fn ready_case(failure: Option<&str>) {
        let root = std::env::temp_dir().join(format!("ready-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("raft.db");
        let mut store = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let entry = |index, term| Entry {
            index,
            term,
            ..Default::default()
        };
        store.append(&[entry(1, 1), entry(2, 1)]).unwrap();
        store
            .set_hardstate(&HardState {
                term: 7,
                vote: 2,
                commit: 2,
                ..Default::default()
            })
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        if let Some(failure) = failure {
            connection.execute_batch(if failure == "entry" {
                "CREATE TRIGGER fail_entry BEFORE INSERT ON entries WHEN NEW.idx=52 BEGIN SELECT RAISE(ABORT,'injected entry failure'); END;"
            } else {
                "CREATE TRIGGER fail_hardstate BEFORE INSERT ON hardstate BEGIN SELECT RAISE(ABORT,'injected hardstate failure'); END;"
            }).unwrap();
        }
        drop(connection);
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let mut node = RawNode::new(
            &Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &logger,
        )
        .unwrap();
        let mut snap = Snapshot::default();
        snap.mut_metadata().set_index(50);
        snap.mut_metadata().set_term(7);
        snap.mut_metadata()
            .set_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
        let mut message = Message::default();
        message.set_msg_type(MessageType::MsgSnapshot);
        message.from = 2;
        message.to = 1;
        message.term = 8;
        message.set_snapshot(snap);
        node.step(message).unwrap();
        let mut message = Message::default();
        message.set_msg_type(MessageType::MsgAppend);
        message.from = 2;
        message.to = 1;
        message.term = 8;
        message.index = 50;
        message.log_term = 7;
        message.commit = 52;
        message.set_entries(vec![entry(51, 8), entry(52, 8)].into());
        node.step(message).unwrap();
        let network = RecordingNetwork {
            messages: Mutex::new(Vec::new()),
            path: path.clone(),
        };
        let outcome = on_ready(&mut node, &mut store, &network);
        assert_eq!(outcome.is_err(), failure.is_some());
        if failure.is_none() {
            let (snapshot, committed, _) = outcome.unwrap();
            assert_eq!(snapshot.unwrap().get_metadata().index, 50);
            assert_eq!(
                committed
                    .iter()
                    .map(|entry| entry.index)
                    .collect::<Vec<_>>(),
                vec![51, 52]
            );
        }
        let in_memory = store
            .raft_storage()
            .initial_state()
            .unwrap()
            .hard_state
            .commit;
        let replies = network.messages.lock().unwrap().len();
        drop(node);
        drop(store);
        let reopened = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let hs = reopened.raft_storage().initial_state().unwrap().hard_state;
        let restored = (
            reopened.snapshot_index(),
            reopened.raft_storage().last_index().unwrap(),
            hs.term,
            hs.vote,
            hs.commit,
        );
        RawNode::new(
            &Config {
                id: 1,
                ..Default::default()
            },
            reopened.raft_storage(),
            &logger,
        )
        .unwrap();
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
        if failure.is_some() {
            assert_eq!(replies, 0, "未落盘的 follower 不得回复复制成功");
            assert_eq!(in_memory, 2, "Ready 失败不得改变持久缓存");
            assert_eq!(
                restored,
                (0, 2, 7, 2, 2),
                "snapshot、entries、term、vote、commit 必须整轮原子回滚"
            );
        } else {
            assert!(replies > 0);
            assert_eq!(in_memory, 52);
            assert_eq!(restored, (50, 52, 8, 0, 52));
        }
    }
}
