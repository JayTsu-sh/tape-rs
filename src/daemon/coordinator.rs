//! 控制 Leader 只做调度；设备 owner 可以是 Follower。所有设备结果经 Raft 提交。
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, mpsc::Sender};
use std::time::{Duration, Instant};

use raft::{RawNode, ReadState, StateRole};

use super::executor::{ExecEvent, ExecRequest};
use super::node::{CoordinationConfig, propose};
use super::permission::{Permission, Phase};
use super::state::{Command, ControlState, split_catalog};
use super::store::ControlStorage;
use crate::error::{Result, TapeError};

struct Check {
    context: Vec<u8>,
    term: u64,
    round: u64,
    phase: Phase,
    deadline: Instant,
}

pub(super) struct Coordinator {
    id: u64,
    instance: u64,
    cfg: CoordinationConfig,
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
    report_sequence: u64,
    reports: VecDeque<Command>,
    check: Option<Check>,
    serial: u64,
    next_check: Instant,
    next_report: Instant,
    next_assign: Instant,
}

impl Drop for Coordinator {
    fn drop(&mut self) {
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
            report_sequence: 0,
            reports: VecDeque::new(),
            check: None,
            serial: 0,
            next_check: now,
            next_report: now,
            next_assign: now,
        })
    }

    fn report(&mut self, round: u64, command: Command) {
        if self.round != Some(round) {
            return;
        }
        self.report_sequence += 1;
        self.reports.push_back(Command::ExecutorReport {
            node: self.id,
            instance: self.instance,
            round,
            sequence: self.report_sequence,
            command: Box::new(command),
        });
    }

    pub(super) fn event(
        &mut self,
        event: ExecEvent,
        node: &mut RawNode<ControlStorage>,
        ctl: &ControlState,
        local: &mut String,
    ) {
        match event {
            ExecEvent::Capability { drives, eligible } => {
                self.drives = drives;
                self.up = if eligible {
                    self.up.saturating_add(1)
                } else {
                    0
                };
                self.capability_sequence += 1;
                propose(
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
                    let parts = split_catalog(&files);
                    for (part, files) in parts.iter().enumerate() {
                        self.report(
                            round,
                            Command::CatalogPart {
                                barcode: barcode.clone(),
                                generation,
                                part: part as u32,
                                files: files.clone(),
                            },
                        );
                    }
                    self.report(
                        round,
                        Command::TapeCommitted {
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

    pub(super) fn observe(&mut self, ctl: &ControlState) {
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
        while self.reports.front().is_some_and(|c| matches!(c,Command::ExecutorReport {sequence,..} if *sequence<=ctl.executor_report_sequence)) {
            self.reports.pop_front();
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
            self.reports.clear();
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
                || (!self.started && now.duration_since(self.started_at) > self.cfg.validity))
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
                    "owner:{}:{}:{}:{}",
                    self.id, self.instance, round, self.serial
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
            if !self.started && self.permission.allows(round, Phase::Fencing) {
                self.started = true;
                *local = format!("轮次 {round}：正在隔离设备");
                let _ = exec.send(ExecRequest::Takeover { round });
            }
            if self.started
                && !self.recovering
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
                if let Some(cmd) = self.reports.front() {
                    propose(node, cmd);
                }
                self.next_report = now + Duration::from_millis(30);
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
            if !healthy_owner {
                let chosen = std::iter::once(self.id)
                    .chain(ctl.candidates.keys().copied().filter(|id| *id != self.id))
                    .find(|id| self.fresh(ctl, *id, now));
                if let Some(id) = chosen {
                    let instance = ctl.candidates[&id].instance;
                    propose(
                        node,
                        &Command::AssignExecutor {
                            node: id,
                            instance,
                            term: node.raft.term,
                            expected_round: ctl.executor.as_ref().map(|e| e.round),
                            group: self.cfg.group.clone(),
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
