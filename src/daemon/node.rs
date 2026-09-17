//! Raft 循环：驱动 raft-rs，应用已提交的控制命令，并据此指挥设备执行线程。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use raft::eraftpb::{Entry, EntryType, Message};
use raft::storage::MemStorage;
use raft::{Config, RawNode, StateRole};
use serde_json::json;
use slog::Drain;

use super::executor::{ExecEvent, ExecRequest};
use super::directory::{Directory, PoolRow};
use super::net::{AdminReply, Network, NodeInput};
use super::state::{Applied, Command, ControlState, Executor, split_catalog};
use super::store::RaftStore;
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
}

pub type SharedStatus = Arc<Mutex<NodeStatus>>;

fn raft_err(e: raft::Error) -> TapeError {
    TapeError::Ltfs(format!("raft: {}", e))
}

pub fn run(
    cfg: NodeConfig,
    mut store: RaftStore,
    net: Box<dyn Network>,
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
    let mut node: RawNode<MemStorage> = RawNode::new(&raft_cfg, store.raft_storage(), &logger).map_err(raft_err)?;

    let mut ctl = ControlState::default();
    let mut directory = match &cfg.directory_file {
        Some(p) => Some(Directory::open(p)?),
        None => None,
    };
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
            Ok(NodeInput::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                let _ = exec.send(ExecRequest::Shutdown);
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
                    ExecEvent::Committed { round, barcode, volume_uuid, generation, files_total, bytes_used, files, full } => {
                        // 目录记录在卷提交成功之后才进日志。这里失败（不再是 Leader 等）没关系：
                        // 目录会落后于磁带，下次装载该带时按磁带对账补齐。
                        let known = ctl.tape_summary.get(&barcode).map(|t| t.generation).unwrap_or(0);
                        if my_round != Some(round) || !is_leader {
                            info!("{} 第 {} 代的目录记录未写入日志：已不是执行者", barcode, generation);
                        } else if full && known == generation {
                            // 接管时的完整列表：目录已经是这一代，不用重写
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
                                &Command::TapeCommitted { barcode, volume_uuid, generation, files: files_total, bytes_used, parts: parts.len() as u32, full },
                            );
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
            if Instant::now() < cooldown_until {
                if last_propose.is_none_or(|t| t.elapsed() > cfg.tick * 5) {
                    yield_leadership(&mut node, &cfg);
                    last_propose = Some(Instant::now());
                }
            } else if proposed_term != Some(term) && propose(&mut node, &Command::Takeover { node: cfg.id, term }) {
                proposed_term = Some(term);
            }
        }

        let committed = on_ready(&mut node, &mut store, net.as_ref())?;
        for entry in committed {
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
            if let Some(d) = directory.as_mut() {
                d.apply(entry.index, &cmd, &applied)?;
            }
            if let Applied::Admin(result) = &applied {
                let waiter = <[u8; 8]>::try_from(entry.context.as_ref()).ok().and_then(|b| pending_admin.remove(&u64::from_be_bytes(b)));
                if let Some(tx) = waiter {
                    let _ = tx.send(match result {
                        Ok(s) => AdminReply::Ok(s.clone()),
                        Err(e) => AdminReply::Rejected(e.clone()),
                    });
                }
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

        publish_status(&cfg, &node, &ctl, &local, &status, &mut last_status);
    }
}

fn propose(node: &mut RawNode<MemStorage>, cmd: &Command) -> bool {
    match node.propose(vec![], cmd.encode()) {
        Ok(()) => true,
        Err(e) => {
            warn!("提交 {:?} 失败: {}", cmd, e);
            false
        }
    }
}

/// 让出领导权：交给编号上的下一个节点。对方不可达时 raft-rs 会在选举超时后放弃移交。
fn yield_leadership(node: &mut RawNode<MemStorage>, cfg: &NodeConfig) {
    let mut others: Vec<u64> = cfg.peers.iter().copied().filter(|&p| p != cfg.id).collect();
    others.sort();
    if let Some(&to) = others.iter().find(|&&p| p > cfg.id).or(others.first()) {
        info!("让出领导权给节点 {}", to);
        node.transfer_leader(to);
    }
}

/// raft-rs 的 Ready 处理。先持久化、再发送需要持久化之后才能发的消息。返回已提交的条目。
fn on_ready(node: &mut RawNode<MemStorage>, store: &mut RaftStore, net: &dyn Network) -> Result<Vec<Entry>> {
    let mut committed = Vec::new();
    if !node.has_ready() {
        return Ok(committed);
    }
    let send = |msgs: Vec<Message>| {
        for m in msgs {
            net.send(m);
        }
    };
    let mut ready = node.ready();
    send(ready.take_messages());
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
    Ok(committed)
}

fn publish_status(
    cfg: &NodeConfig,
    node: &RawNode<MemStorage>,
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
    };
    let text = json!({
        "id": st.id, "role": st.role, "term": st.term, "leader": st.leader,
        "executor": st.executor.as_ref().map(|e| json!({"node": e.node, "round": e.round, "term": e.term, "fenced": e.fenced})),
        "local": st.local, "applied_index": st.applied_index,
        "pools": st.pools.iter().map(|p| json!({"uuid": p.uuid, "name": p.name, "file_limit": p.file_limit, "tapes": p.tapes})).collect::<Vec<_>>(),
    })
    .to_string();
    if text == *last {
        return;
    }
    *last = text.clone();
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = st;
    if let Some(path) = &cfg.status_file {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}
