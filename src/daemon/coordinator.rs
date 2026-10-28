//! 控制 Leader 只做调度；设备 owner 可以是 Follower。所有设备结果经 Raft 提交。
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, mpsc::Sender};
use std::time::{Duration, Instant};

use raft::{RawNode, ReadState, StateRole};

use super::executor::{ExecEvent, ExecRequest};
use super::node::{CoordinationConfig, propose};
use super::permission::{Permission, Phase};
use super::state::{Command, ControlState};
use super::store::ControlStorage;
use crate::error::{Result, TapeError};

struct Check {
    context: Vec<u8>,
    term: u64,
    round: u64,
    phase: Phase,
    deadline: Instant,
}

/// 控制循环一次只准备一片目录；磁带目录的所有权直接移入队列，不复制完整目录。
enum PendingReport {
    Command(Command),
    Catalog(Box<CatalogReport>),
}

struct CatalogReport {
    barcode: String,
    volume_uuid: String,
    generation: u64,
    files_total: u64,
    bytes_used: u64,
    bytes_written: u64,
    full: bool,
    files: std::iter::Peekable<std::vec::IntoIter<super::state::FileRec>>,
    part: u32,
    finished: bool,
}
impl CatalogReport {
    fn next_command(&mut self) -> Command {
        let mut files = Vec::new();
        let mut size = 0usize;
        while let Some(file) = self.files.peek() {
            let cost = file.path.len() + file.sha256.len() + file.metadata.to_string().len() + 72;
            if size + cost > super::state::CATALOG_PART_BYTES && !files.is_empty() {
                break;
            }
            size += cost;
            files.push(self.files.next().expect("已检查目录条目"));
        }
        if !files.is_empty() {
            let part = self.part;
            self.part = self.part.checked_add(1).expect("目录分片计数超限");
            Command::CatalogPart {
                barcode: self.barcode.clone(),
                generation: self.generation,
                part,
                files,
            }
        } else {
            self.finished = true;
            Command::TapeCommitted {
                barcode: self.barcode.clone(),
                volume_uuid: self.volume_uuid.clone(),
                generation: self.generation,
                files: self.files_total,
                bytes_used: self.bytes_used,
                bytes_written: self.bytes_written,
                parts: self.part,
                full: self.full,
            }
        }
    }
}

pub(super) struct Coordinator {
    id: u64,
    instance: u64,
    cfg: CoordinationConfig,
    scope: Option<String>,
    cooldown: Duration,
    cooldown_until: Instant,
    permission: Arc<Permission>,
    capabilities: BTreeMap<u64, (u64, u64, Instant)>,
    drives: usize,
    up: u8,
    capability_sequence: u64,
    round: Option<u64>,
    started_at: Instant,
    started: bool,
    recovering: bool,
    catalog_ready: bool,
    report_sequence: u64,
    reports: VecDeque<Command>,
    pending: VecDeque<PendingReport>,
    check: Option<Check>,
    serial: u64,
    next_check: Instant,
    next_report: Instant,
    next_assign: Instant,
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.retire_pending();
        if let Some(round) = self.round {
            self.permission.close(round);
        }
    }
}

impl Coordinator {
    pub(super) fn new(
        id: u64,
        instance: u64,
        cfg: CoordinationConfig,
        cooldown: Duration,
        exec: &Sender<ExecRequest>,
    ) -> Result<Self> {
        if cfg.group.is_empty() || cfg.validity < Duration::from_millis(100) {
            return Err(TapeError::Ltfs(
                "控制/执行分离需要非空设备组和至少 100ms 的执行许可".into(),
            ));
        }
        let permission = Arc::new(Permission::default());
        exec.send(ExecRequest::AttachPermission {
            permission: permission.clone(),
        })
        .map_err(|_| TapeError::Ltfs("设备执行线程已退出".into()))?;
        let now = Instant::now();
        Ok(Self {
            id,
            instance,
            cfg,
            scope: None,
            cooldown,
            cooldown_until: now,
            permission,
            capabilities: BTreeMap::new(),
            drives: 0,
            up: 0,
            capability_sequence: 0,
            round: None,
            started_at: now,
            started: false,
            recovering: false,
            catalog_ready: true,
            report_sequence: 0,
            reports: VecDeque::new(),
            pending: VecDeque::new(),
            check: None,
            serial: 0,
            next_check: now,
            next_report: now,
            next_assign: now,
        })
    }

    pub(super) fn set_scope(&mut self, scope: String) {
        self.scope = Some(scope);
    }

    fn propose(&self, node: &mut RawNode<ControlStorage>, cmd: &Command) -> bool {
        match &self.scope {
            Some(scope) => node
                .propose(vec![], super::multilib::encode_command(scope, cmd))
                .is_ok(),
            None => propose(node, cmd),
        }
    }

    pub(super) fn set_catalog_ready(&mut self, ready: bool) {
        self.catalog_ready = ready;
    }

    fn report(&mut self, round: u64, command: Command) {
        if self.round != Some(round) {
            return;
        }
        self.pending.push_back(PendingReport::Command(command));
    }

    fn prepare_report(&mut self) {
        if !self.reports.is_empty() {
            return;
        }
        let Some(round) = self.round else {
            return;
        };
        let command = match self.pending.front_mut() {
            Some(PendingReport::Command(_)) => match self.pending.pop_front().unwrap() {
                PendingReport::Command(command) => command,
                _ => unreachable!(),
            },
            Some(PendingReport::Catalog(catalog)) => {
                let command = catalog.next_command();
                if catalog.finished {
                    self.pending.pop_front();
                }
                command
            }
            None => return,
        };
        self.report_sequence += 1;
        self.reports.push_back(Command::ExecutorReport {
            node: self.id,
            instance: self.instance,
            round,
            sequence: self.report_sequence,
            command: Box::new(command),
        });
    }

    fn retire_pending(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending
            .iter()
            .any(|p| matches!(p,PendingReport::Catalog(c) if c.files.len()>4096))
        {
            // 大目录换届后的析构也不应阻塞其他库的心跳；此线程只释放内存，不接触设备。
            std::thread::spawn(move || drop(pending));
        }
    }

    pub(super) fn event(
        &mut self,
        event: ExecEvent,
        node: &mut RawNode<ControlStorage>,
        ctl: &ControlState,
        local: &mut String,
    ) {
        match event {
            ExecEvent::Management { round, command } => self.report(round, command),
            ExecEvent::FileTask {
                task,
                round,
                pool,
                path,
                status,
                generation,
                reason,
            } => {
                self.report(
                    round,
                    Command::FileTask {
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
            ExecEvent::Capability { drives, eligible } => {
                self.drives = drives;
                self.up = if eligible {
                    self.up.saturating_add(1)
                } else {
                    0
                };
                // 多库模式下单库通路丢失必须撤销该库资格，不能等待另一候选出现。
                // 其他库的许可由各自 Coordinator 继续维护。
                if !eligible
                    && self.scope.is_some()
                    && let Some(round) = self.round
                {
                    self.event(
                        ExecEvent::Lost {
                            round,
                            reason: "本库完整设备组或入口网络资格已失效".into(),
                        },
                        node,
                        ctl,
                        local,
                    );
                }
                self.capability_sequence += 1;
                self.propose(
                    node,
                    &Command::Candidate {
                        node: self.id,
                        instance: self.instance,
                        sequence: self.capability_sequence,
                        group: self.cfg.group.clone(),
                        drives,
                        eligible: self.up >= 2 && Instant::now() >= self.cooldown_until,
                    },
                );
            }
            ExecEvent::Fenced { round } => self.report(
                round,
                Command::Fenced {
                    node: self.id,
                    round,
                },
            ),
            ExecEvent::Serving { round, summary } => {
                if self.round == Some(round) {
                    *local = format!("轮次 {round}：恢复完成，等待发布。{summary}");
                    self.report(
                        round,
                        Command::ExecutorReady {
                            node: self.id,
                            round,
                        },
                    );
                }
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
                if full
                    && ctl
                        .tape_summary
                        .get(&barcode)
                        .is_some_and(|s| s.generation > generation)
                {
                    self.report(
                        round,
                        Command::TapeState {
                            barcode,
                            state: super::state::tape_state::CHECK.into(),
                        },
                    );
                } else {
                    if self.round == Some(round) {
                        self.pending
                            .push_back(PendingReport::Catalog(Box::new(CatalogReport {
                                barcode,
                                volume_uuid,
                                generation,
                                files_total,
                                bytes_used,
                                bytes_written,
                                full,
                                files: files.into_iter().peekable(),
                                part: 0,
                                finished: false,
                            })));
                    } else if files.len() > 4096 {
                        std::thread::spawn(move || drop(files));
                    }
                }
            }
            ExecEvent::TapeState {
                round,
                barcode,
                state,
            }
            | ExecEvent::ReclaimAbandoned {
                round,
                barcode,
                state,
                ..
            } => self.report(round, Command::TapeState { barcode, state }),
            ExecEvent::Reclaimed {
                round,
                barcode,
                volume_uuid,
                ..
            } => self.report(
                round,
                Command::TapeReclaimed {
                    barcode,
                    volume_uuid,
                },
            ),
            ExecEvent::FenceFailed { round, reason } | ExecEvent::Lost { round, reason } => {
                if self.round == Some(round) {
                    self.permission.close(round);
                    self.cooldown_until = Instant::now() + self.cooldown;
                    self.up = 0;
                    *local = format!("轮次 {round} 已停止：{reason}");
                    self.report(
                        round,
                        Command::Abandoned {
                            node: self.id,
                            round,
                            reason,
                        },
                    );
                }
            }
        }
    }

    pub(super) fn observe(&mut self, ctl: &ControlState, directory_idle: bool) {
        let now = Instant::now();
        for (&id, c) in &ctl.candidates {
            if self
                .capabilities
                .get(&id)
                .is_none_or(|(inst, seq, _)| (*inst, *seq) != (c.instance, c.sequence))
            {
                self.capabilities.insert(id, (c.instance, c.sequence, now));
            }
        }
        if directory_idle {
            while self.reports.front().is_some_and(|c| matches!(c,Command::ExecutorReport {sequence,..} if *sequence<=ctl.executor_report_sequence)) {
                self.reports.pop_front();
                // 已确认的报告立刻让出发送窗口；未确认的报告只按有界间隔重试。
                self.next_report = Instant::now();
            }
        }
    }

    fn fresh(&self, ctl: &ControlState, id: u64, now: Instant) -> bool {
        ctl.candidates.get(&id).is_some_and(|c| {
            c.group == self.cfg.group
                && c.eligible
                && self.capabilities.get(&id).is_some_and(|(inst, seq, at)| {
                    *inst == c.instance
                        && *seq == c.sequence
                        && now.duration_since(*at) < self.cfg.validity * 2
                })
        })
    }

    pub(super) fn tick(
        &mut self,
        node: &mut RawNode<ControlStorage>,
        ctl: &ControlState,
        directory_ready: bool,
        exec: &Sender<ExecRequest>,
        local: &mut String,
    ) {
        let now = Instant::now();
        let own = ctl
            .executor
            .as_ref()
            .filter(|e| e.node == self.id && ctl.executor_instance == self.instance)
            .map(|e| e.round);
        if own != self.round {
            if let Some(round) = self.round.take() {
                self.permission.close(round);
                let _ = exec.send(ExecRequest::Stop {
                    round: Some(round),
                    reason: "设备 owner 已变更".into(),
                });
            }
            self.round = own;
            self.started_at = now;
            self.started = false;
            self.recovering = false;
            self.report_sequence = 0;
            self.next_report = now;
            self.reports.clear();
            self.retire_pending();
            self.check = None;
            self.next_check = now;
        }
        if let Some(round) = own {
            let e = ctl.executor.as_ref().unwrap();
            let phase = if ctl.executor_ready {
                Phase::Serving
            } else if e.fenced {
                Phase::Execution
            } else {
                Phase::Fencing
            };
            let alive = self.permission.allows(round, Phase::Fencing)
                || self.permission.allows(round, Phase::Execution)
                || self.permission.allows(round, Phase::Serving);
            if (!directory_ready
                || (self.started && !alive)
                || (!self.started
                    && !alive
                    && now.duration_since(self.started_at) > self.cfg.validity))
                && now >= self.cooldown_until
            {
                self.event(
                    ExecEvent::Lost {
                        round,
                        reason: "目录未就绪或多数派执行许可已失效".into(),
                    },
                    node,
                    ctl,
                    local,
                );
                let _ = exec.send(ExecRequest::Stop {
                    round: Some(round),
                    reason: local.clone(),
                });
            }
            if directory_ready && now >= self.cooldown_until && now >= self.next_check {
                self.serial += 1;
                let context = format!(
                    "owner:{}:{}:{}:{}:{}",
                    self.scope.as_deref().unwrap_or(""),
                    self.id,
                    self.instance,
                    round,
                    self.serial
                )
                .into_bytes();
                self.check = Some(Check {
                    context: context.clone(),
                    term: node.raft.term,
                    round,
                    phase,
                    deadline: now + self.cfg.validity,
                });
                node.read_index(context);
                self.next_check = now + self.cfg.validity / 4;
            }
            if !self.started && self.catalog_ready && self.permission.allows(round, Phase::Fencing)
            {
                self.started = true;
                *local = format!("轮次 {round}：正在隔离设备");
                let _ = exec.send(ExecRequest::Takeover { round });
            }
            if self.started
                && !self.recovering
                && self.catalog_ready
                && e.fenced
                && self.permission.allows(round, Phase::Execution)
            {
                self.recovering = true;
                *local = format!("轮次 {round}：隔离已提交，正在恢复卷");
                let _ = exec.send(ExecRequest::Recover { round });
            }
            if ctl.executor_ready && self.permission.allows(round, Phase::Serving) {
                *local = format!(
                    "轮次 {round}：服务中（设备 owner，控制 Leader {}）",
                    node.raft.leader_id
                );
            }
            if now >= self.next_report {
                self.prepare_report();
                if let Some(cmd) = self.reports.front() {
                    self.propose(node, cmd);
                }
                self.next_report = now + (self.cfg.validity / 4).min(Duration::from_secs(1));
            }
        }
        if node.raft.state == StateRole::Leader && now >= self.next_assign {
            let healthy_owner = ctl.executor.as_ref().is_some_and(|e| {
                self.fresh(ctl, e.node, now)
                    && ctl
                        .candidates
                        .get(&e.node)
                        .is_some_and(|c| c.instance == ctl.executor_instance)
            });
            // Leader 具备本设备组的有效资格时兼任 owner；无资格时保留健康 owner。
            let leader_should_own = self.fresh(ctl, self.id, now)
                && ctl.executor.as_ref().is_none_or(|e| e.node != self.id);
            if ctl.catalog_quiesce_index.is_some() && ctl.catalog_frozen_index.is_none() {
                // 暂停期间 AssignExecutor 会被状态机拒绝。重启/失联的旧实例
                // 不能再提交自己的退出报告，须经多数派撤销后才能完成冻结。
                if !healthy_owner && let Some(owner) = ctl.executor.as_ref() {
                    self.propose(
                        node,
                        &Command::Abandoned {
                            node: owner.node,
                            round: owner.round,
                            reason: "旧目录已暂停，撤销失效的 owner 实例".into(),
                        },
                    );
                }
            } else if leader_should_own || !healthy_owner {
                let chosen = std::iter::once(self.id)
                    .chain(ctl.candidates.keys().copied().filter(|id| *id != self.id))
                    .find(|id| self.fresh(ctl, *id, now));
                if let Some(id) = chosen {
                    let instance = ctl.candidates[&id].instance;
                    self.propose(
                        node,
                        &Command::AssignExecutor {
                            node: id,
                            instance,
                            term: node.raft.term,
                            expected_round: ctl.executor.as_ref().map(|e| e.round),
                            group: self.cfg.group.clone(),
                        },
                    );
                } else if self.scope.is_some()
                    && let Some(owner) = ctl.executor.as_ref()
                {
                    // 重启后旧实例可能永远不再上报 Abandoned；没有任何合格候选时显式撤销旧 owner。
                    self.propose(
                        node,
                        &Command::Abandoned {
                            node: owner.node,
                            round: owner.round,
                            reason: "本库没有具备有效完整通路的候选节点".into(),
                        },
                    );
                }
            }
            self.next_assign = now + Duration::from_millis(100);
        }
    }

    pub(super) fn confirm(
        &mut self,
        reads: Vec<ReadState>,
        node: &RawNode<ControlStorage>,
        ctl: &ControlState,
        applied: u64,
        directory_ready: bool,
    ) {
        let Some(check) = self.check.as_ref() else {
            return;
        };
        let current_phase = if ctl.executor_ready {
            Phase::Serving
        } else if ctl.executor.as_ref().is_some_and(|e| e.fenced) {
            Phase::Execution
        } else {
            Phase::Fencing
        };
        if directory_ready
            && node.raft.term == check.term
            && check.phase == current_phase
            && ctl.executor_instance == self.instance
            && ctl
                .executor
                .as_ref()
                .is_some_and(|e| e.node == self.id && e.round == check.round)
            && reads
                .iter()
                .any(|r| r.request_ctx == check.context && r.index <= applied)
        {
            self.permission
                .renew(check.round, check.phase, check.deadline);
            self.check = None;
        }
    }
}

#[cfg(test)]
mod catalog_latency_tests {
    use super::super::{
        state::{Candidate, FileRec},
        store::RaftStore,
    };
    use super::*;
    #[test]
    fn quiesced_directory_retires_stale_owner_without_assigning_a_replacement() {
        for (healthy, frozen) in [(false, false), (true, false), (true, true)] {
            let dir = std::env::temp_dir().join(format!("quiesced-owner-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let store = RaftStore::open(&dir.join("raft.db"), &[1]).unwrap();
            let mut node = RawNode::new(
                &raft::Config {
                    id: 1,
                    ..Default::default()
                },
                store.raft_storage(),
                &slog::Logger::root(slog::Discard, slog::o!()),
            )
            .unwrap();
            node.campaign().unwrap();
            let (tx, _rx) = std::sync::mpsc::channel();
            let mut c = Coordinator::new(
                1,
                1,
                CoordinationConfig {
                    group: "test".into(),
                    validity: Duration::from_secs(6),
                },
                Duration::ZERO,
                &tx,
            )
            .unwrap();
            let mut ctl = ControlState::default();
            ctl.apply(42, &Command::Takeover { node: 2, term: 1 });
            ctl.executor_instance = 1;
            ctl.catalog_quiesce_index = Some(43);
            ctl.catalog_frozen_index = frozen.then_some(44);
            for id in [1, 2] {
                ctl.candidates.insert(
                    id,
                    Candidate {
                        instance: if id == 2 && !healthy { 2 } else { 1 },
                        sequence: 1,
                        group: "test".into(),
                        drives: 1,
                        eligible: true,
                    },
                );
            }
            c.observe(&ctl, true);
            let before = node.raft.raft_log.last_index();
            c.tick(&mut node, &ctl, true, &tx, &mut String::new());
            if healthy && !frozen {
                assert_eq!(
                    node.raft.raft_log.last_index(),
                    before,
                    "暂停时不得提交必然被拒绝的新 owner 分配"
                );
            } else {
                let ready = node.ready();
                let command = Command::decode(&ready.entries().last().unwrap().data).unwrap();
                if frozen {
                    assert!(
                        matches!(command, Command::AssignExecutor { node: 1, .. }),
                        "冻结完成后保留 FDB owner 接管能力: {command:?}"
                    );
                } else {
                    assert!(
                        matches!(
                            command,
                            Command::Abandoned {
                                node: 2,
                                round: 42,
                                ..
                            }
                        ),
                        "暂停中的过期实例须先撤销: {command:?}"
                    );
                    ctl.apply(45, &command);
                    assert!(ctl.executor.is_none());
                    assert!(matches!(
                        ctl.apply(46, &Command::CatalogFreeze { quiesce_index: 43 }),
                        super::super::state::Applied::Admin(Ok(_))
                    ));
                }
            }
            drop(node);
            drop(store);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn busy_catalog_keeps_candidate_fresh_without_acking_report_early() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut c = Coordinator::new(
            72,
            2,
            CoordinationConfig {
                group: "test".into(),
                validity: Duration::from_secs(6),
            },
            Duration::ZERO,
            &tx,
        )
        .unwrap();
        c.capabilities
            .insert(71, (1, 8, Instant::now() - Duration::from_secs(20)));
        c.reports.push_back(Command::ExecutorReport {
            node: 72,
            instance: 2,
            round: 1,
            sequence: 1,
            command: Box::new(Command::ExecutorReady { node: 72, round: 1 }),
        });
        let mut ctl = ControlState::default();
        ctl.candidates.insert(
            71,
            Candidate {
                instance: 1,
                sequence: 9,
                group: "test".into(),
                drives: 2,
                eligible: true,
            },
        );
        ctl.executor_report_sequence = 1;
        c.observe(&ctl, false);
        assert!(c.fresh(&ctl, 71, Instant::now()));
        assert_eq!(c.reports.len(), 1);
        c.observe(&ctl, true);
        assert!(c.reports.is_empty());
    }

    #[test]
    fn catalog_stream_keeps_all_parts_before_commit_and_later_reports() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut c = Coordinator::new(
            1,
            1,
            CoordinationConfig {
                group: "test".into(),
                validity: Duration::from_secs(6),
            },
            Duration::ZERO,
            &tx,
        )
        .unwrap();
        c.round = Some(1);
        let records: Vec<_> = (0..2000)
            .map(|i| FileRec {
                path: format!("/entry/{i}"),
                length: 1,
                sha256: String::new(),
                version: (1, i),
                deleted: false,
                metadata: serde_json::json!({"note":"x".repeat(2048)}),
            })
            .collect();
        c.pending
            .push_back(PendingReport::Catalog(Box::new(CatalogReport {
                barcode: "TEST01L8".into(),
                volume_uuid: "uuid".into(),
                generation: 2,
                files_total: 2000,
                bytes_used: 2000,
                bytes_written: 3000,
                full: true,
                files: records.into_iter().peekable(),
                part: 0,
                finished: false,
            })));
        c.report(1, Command::ExecutorReady { node: 1, round: 1 });
        let mut paths = std::collections::BTreeSet::new();
        let mut parts = 0;
        let mut committed = false;
        let mut sequence = 0;
        while !c.pending.is_empty() || !c.reports.is_empty() {
            c.prepare_report();
            assert_eq!(c.reports.len(), 1);
            let cmd = c.reports.front().unwrap().clone();
            c.prepare_report();
            assert_eq!(c.reports.len(), 1, "未确认当前分片时不得准备后续报告");
            let Command::ExecutorReport {
                sequence: seq,
                command,
                ..
            } = cmd
            else {
                panic!()
            };
            sequence += 1;
            assert_eq!(seq, sequence);
            match *command {
                Command::CatalogPart { part, files, .. } => {
                    assert!(!committed);
                    assert_eq!(part, parts);
                    parts += 1;
                    for r in files {
                        assert!(paths.insert(r.path));
                    }
                }
                Command::TapeCommitted {
                    parts: count,
                    files,
                    ..
                } => {
                    assert_eq!(count, parts);
                    assert_eq!(files, 2000);
                    assert_eq!(paths.len(), 2000);
                    committed = true;
                }
                Command::ExecutorReady { .. } => assert!(committed),
                other => panic!("意外报告: {other:?}"),
            }
            let ctl = ControlState {
                executor_report_sequence: sequence,
                ..Default::default()
            };
            c.observe(&ctl, true);
        }
        assert!(committed);
        assert!(parts > 1);
    }

    #[test]
    fn pending_catalog_keeps_fresh_permission_but_delays_device_takeover() {
        let dir = std::env::temp_dir().join(format!("catalog-start-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = RaftStore::open(&dir.join("raft.db"), &[1]).unwrap();
        let mut node = RawNode::new(
            &raft::Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &slog::Logger::root(slog::Discard, slog::o!()),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut c = Coordinator::new(
            1,
            1,
            CoordinationConfig {
                group: "test".into(),
                validity: Duration::from_secs(6),
            },
            Duration::ZERO,
            &tx,
        )
        .unwrap();
        let mut ctl = ControlState::default();
        ctl.apply(1, &Command::Takeover { node: 1, term: 1 });
        ctl.executor_instance = 1;
        c.round = Some(1);
        c.started_at = Instant::now() - Duration::from_secs(60);
        c.next_assign = Instant::now() + Duration::from_secs(60);
        c.next_check = c.next_assign;
        c.permission
            .renew(1, Phase::Fencing, Instant::now() + Duration::from_secs(6));
        c.set_catalog_ready(false);
        c.tick(&mut node, &ctl, true, &tx, &mut String::new());
        assert!(!c.started);
        assert!(c.permission.allows(1, Phase::Fencing));
        c.set_catalog_ready(true);
        c.tick(&mut node, &ctl, true, &tx, &mut String::new());
        assert!(c.started);
        assert!(
            rx.try_iter()
                .any(|r| matches!(r, ExecRequest::Takeover { round: 1 }))
        );
        drop(node);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delayed_report_ack_does_not_flood_raft_with_duplicate_parts() {
        let dir = std::env::temp_dir().join(format!("catalog-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = RaftStore::open(&dir.join("raft.db"), &[1]).unwrap();
        let mut node = RawNode::new(
            &raft::Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &slog::Logger::root(slog::Discard, slog::o!()),
        )
        .unwrap();
        node.campaign().unwrap();
        assert_eq!(node.raft.state, StateRole::Leader);
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut c = Coordinator::new(
            1,
            1,
            CoordinationConfig {
                group: "test".into(),
                validity: Duration::from_secs(6),
            },
            Duration::ZERO,
            &tx,
        )
        .unwrap();
        let mut ctl = ControlState::default();
        ctl.apply(
            1,
            &Command::Takeover {
                node: 1,
                term: node.raft.term,
            },
        );
        ctl.executor_instance = 1;
        c.round = Some(1);
        c.started = true;
        c.recovering = true;
        c.next_assign = Instant::now() + Duration::from_secs(60);
        c.next_check = c.next_assign;
        c.permission
            .renew(1, Phase::Execution, Instant::now() + Duration::from_secs(6));
        c.report(
            1,
            Command::CatalogPart {
                barcode: "TEST01L8".into(),
                generation: 2,
                part: 0,
                files: vec![FileRec {
                    path: "/entry".into(),
                    length: 1,
                    sha256: String::new(),
                    version: (1, 1),
                    deleted: false,
                    metadata: serde_json::json!({"note":"x".repeat(4096)}),
                }],
            },
        );
        let before = node.raft.raft_log.last_index();
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            c.tick(&mut node, &ctl, true, &tx, &mut String::new());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            node.raft.raft_log.last_index() - before,
            1,
            "等待目录确认时不得每30ms追加同一分片"
        );
        c.report(1, Command::ExecutorReady { node: 1, round: 1 });
        ctl.executor_report_sequence = 1;
        c.observe(&ctl, true);
        c.tick(&mut node, &ctl, true, &tx, &mut String::new());
        assert_eq!(
            node.raft.raft_log.last_index() - before,
            2,
            "确认后应立即发送下一份报告"
        );
        drop(node);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_catalog_has_zero_parts_and_a_commit() {
        let mut catalog = CatalogReport {
            barcode: "empty".into(),
            volume_uuid: "uuid".into(),
            generation: 2,
            files_total: 0,
            bytes_used: 0,
            bytes_written: 0,
            full: true,
            files: Vec::new().into_iter().peekable(),
            part: 0,
            finished: false,
        };
        assert!(matches!(
            catalog.next_command(),
            Command::TapeCommitted {
                parts: 0,
                files: 0,
                ..
            }
        ));
        assert!(catalog.finished);
    }

    #[test]
    fn bulk_catalog_event_does_not_expire_the_control_loop_permit() {
        let dir = std::env::temp_dir().join(format!("catalog-permit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = RaftStore::open(&dir.join("raft.db"), &[1]).unwrap();
        let mut node = RawNode::new(
            &raft::Config {
                id: 1,
                ..Default::default()
            },
            store.raft_storage(),
            &slog::Logger::root(slog::Discard, slog::o!()),
        )
        .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut c = Coordinator::new(
            1,
            1,
            CoordinationConfig {
                group: "test".into(),
                validity: Duration::from_millis(100),
            },
            Duration::ZERO,
            &tx,
        )
        .unwrap();
        c.round = Some(1);
        let files: Vec<_> = (0..12000)
            .map(|i| FileRec {
                path: format!("/bulk/{i}"),
                length: 1,
                sha256: String::new(),
                version: (1, i),
                deleted: false,
                metadata: serde_json::json!({"note":"x".repeat(4096)}),
            })
            .collect();
        c.permission.renew(
            1,
            Phase::Serving,
            Instant::now() + Duration::from_millis(100),
        );
        let start = Instant::now();
        c.event(
            ExecEvent::Committed {
                round: 1,
                barcode: "TEST01L8".into(),
                volume_uuid: "u".into(),
                generation: 2,
                files_total: files.len() as u64,
                bytes_used: 0,
                bytes_written: 0,
                files,
                full: true,
            },
            &mut node,
            &ControlState::default(),
            &mut String::new(),
        );
        assert!(
            c.permission.allows(1, Phase::Serving),
            "单个目录事件占用控制循环 {:?}，导致许可超时",
            start.elapsed()
        );
        drop(c);
        drop(node);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
