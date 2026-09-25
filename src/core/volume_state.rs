//! D03：每卷不可变状态根与差量发布（`.scratch/ltfs-ha/publication-root.md`、`commit-publication.md`）。
//!
//! 设计要点：
//! - 每卷一个不可变 [`VolumeRoot`]，读者 `load()` 取整根引用（O(1)），永不看到半发布状态。
//! - 已提交视图是目录树，更新走路径复制：新根只复制被修改路径上的目录节点，
//!   分配量与本批变更路径数 × 深度成正比，与目录总规模无关（[`PublishStats`] 可观测）。
//! - 所有更新经同一顺序器（一把锁）；慢 I/O 不在锁内。发布是单指针替换；
//!   被替换的根进入 `retired`，最后一个读者释放后由 [`VolumeState::reclaim`] 延后回收。
//! - 每次成功发布后预构造 `poisoned` 根（共享同一份视图引用、能力标记为不可信），
//!   发布失败时用它换入，不需要新的分配。
//! - 会话期限以调用方提供的单调时钟（BOOTTIME 语义）判定，只在顺序器处理事件时评估；
//!   携带旧 `epoch` 的到期检查若遇到更新的续期则作废。
//!
//! 这里不做设备 I/O、不持久化：磁带仍是已提交状态权威，本模块只是运行实例内的
//! 可重建状态。S4 证据由调用方（卷提交流程）提供。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub type Path = String;
pub type SessionId = u64;
pub type ReservationId = u64;
pub type BatchId = u64;
pub type InstanceId = u64;

// ---------- 已提交视图：结构共享的目录树 ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVersion {
    pub version: u64,
    pub len: u64,
    /// 仅前缀已提交（上传异常结束或 fsync 快照），见文件契约的 partial 状态。
    pub partial: bool,
}

#[derive(Debug, Default)]
pub struct DirNode {
    pub files: BTreeMap<String, Arc<FileVersion>>,
    pub subdirs: BTreeMap<String, Arc<DirNode>>,
}

impl DirNode {
    pub fn get(&self, path: &str) -> Option<Arc<FileVersion>> {
        let (dirs, name) = split_path(path)?;
        fn get_in(node: &DirNode, dirs: &[&str], name: &str) -> Option<Arc<FileVersion>> {
            match dirs.split_first() {
                None => node.files.get(name).cloned(),
                Some((d, rest)) => get_in(node.subdirs.get(*d)?, rest, name),
            }
        }
        get_in(self, &dirs, name)
    }

    pub fn count_files(&self) -> usize {
        self.files.len()
            + self
                .subdirs
                .values()
                .map(|d| d.count_files())
                .sum::<usize>()
    }

    fn with_directory(
        self: &Arc<Self>,
        parts: &[&str],
        remove: bool,
        stats: &mut PublishStats,
    ) -> Arc<Self> {
        let Some((name, rest)) = parts.split_first() else {
            return Arc::clone(self);
        };
        if remove && !rest.is_empty() && !self.subdirs.contains_key(*name) {
            return Arc::clone(self);
        }
        stats.nodes_copied += 1;
        let mut node = Self {
            files: self.files.clone(),
            subdirs: self.subdirs.clone(),
        };
        if rest.is_empty() {
            node.files.remove(*name);
            if remove {
                node.subdirs.remove(*name);
            } else {
                node.subdirs.entry(name.to_string()).or_default();
            }
        } else {
            node.files.remove(*name);
            let child = node.subdirs.entry(name.to_string()).or_default();
            *child = child.with_directory(rest, remove, stats);
        }
        Arc::new(node)
    }

    /// 路径复制：返回一棵新树，只复制 `dirs` 路径上的节点。`file == None` 表示删除。
    fn with_file(
        self: &Arc<Self>,
        dirs: &[&str],
        name: &str,
        file: Option<Arc<FileVersion>>,
        stats: &mut PublishStats,
    ) -> Arc<DirNode> {
        if file.is_none() && !dirs.is_empty() && !self.subdirs.contains_key(dirs[0]) {
            return Arc::clone(self);
        }
        stats.nodes_copied += 1;
        let mut node = DirNode {
            files: self.files.clone(),
            subdirs: self.subdirs.clone(),
        };
        if dirs.is_empty() {
            match file {
                Some(f) => {
                    node.subdirs.remove(name);
                    node.files.insert(name.to_string(), f);
                }
                None => {
                    node.files.remove(name);
                }
            }
        } else {
            node.files.remove(dirs[0]);
            let child = match node.subdirs.get(dirs[0]) {
                Some(c) => c.clone(),
                None => Arc::new(DirNode::default()),
            };
            let new_child = child.with_file(&dirs[1..], name, file, stats);
            node.subdirs.insert(dirs[0].to_string(), new_child);
        }
        Arc::new(node)
    }
}

fn split_path(path: &str) -> Option<(Vec<&str>, &str)> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (name, dirs) = parts.split_last()?;
    Some((dirs.to_vec(), name))
}

// ---------- 未提交集合、账本、会话 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingState {
    InProgress,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEntry {
    pub session: SessionId,
    pub version: u64,
    pub state: PendingState,
    /// 已接收（落带或缓冲）的逻辑长度。
    pub staged_len: u64,
    /// 已被可靠索引覆盖的前缀长度。
    pub committed_prefix: u64,
    /// 最早未提交变更的登记时刻（time-based 触发计时起点，发布不重置）。
    pub earliest_registered: u64,
    pub reservation: ReservationId,
    /// 删除：发布时把路径从已提交视图里去掉，而不是写入一个版本。
    pub removal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub session: SessionId,
    pub path: Path,
    pub reserved: u64,
    pub consumed: u64,
    pub released: bool,
}

impl Reservation {
    pub fn unconsumed(&self) -> u64 {
        self.reserved.saturating_sub(self.consumed)
    }
}

/// 容量账本快照。`available()` 是当前可分配预算：基线减去未消耗预留减去切点后消耗。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ledger {
    pub baseline_free: u64,
    /// 基线对应的发布 epoch（校准切点）。
    pub calibration_epoch: u64,
    pub consumed_since_cut: u64,
    pub reservations: BTreeMap<ReservationId, Reservation>,
}

impl Ledger {
    pub fn reserved_unconsumed(&self) -> u64 {
        self.reservations
            .values()
            .filter(|r| !r.released)
            .map(Reservation::unconsumed)
            .sum()
    }

    pub fn available(&self) -> u64 {
        self.baseline_free
            .saturating_sub(self.reserved_unconsumed())
            .saturating_sub(self.consumed_since_cut)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Active,
    /// 已到期或取消，但仍被在途批次引用；引用归零后转 Expired。
    Closing,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub deadline: u64,
    pub state: SessionState,
    /// 在途批次引用数。
    pub refs: u32,
    /// 最近一次续期发生时的 epoch；早于它的到期检查作废。
    pub renewed_at_epoch: u64,
}

// ---------- 冻结批次与能力 ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverEntry {
    pub version: u64,
    pub len: u64,
    /// 覆盖的是完整文件（Ready）还是打开文件的快照前缀。
    pub complete: bool,
    /// 删除：发布时去掉该路径。
    pub removal: bool,
}

/// S2 冻结描述：本批要发布的精确覆盖与转换依据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenBatch {
    pub batch_id: BatchId,
    pub instance: InstanceId,
    pub base_epoch: u64,
    pub generation: u64,
    pub cover: BTreeMap<Path, CoverEntry>,
    pub sessions: Vec<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub trusted: bool,
    pub readable: bool,
    pub writable: bool,
    pub reason: Option<String>,
}

// ---------- 根 ----------

#[derive(Debug)]
pub struct VolumeRoot {
    pub epoch: u64,
    pub instance: InstanceId,
    pub committed: Arc<DirNode>,
    pub generation: u64,
    pub pending: Arc<BTreeMap<Path, PendingEntry>>,
    pub ledger: Arc<Ledger>,
    pub sessions: Arc<BTreeMap<SessionId, Session>>,
    pub commit_slot: Option<Arc<FrozenBatch>>,
    pub capability: Capability,
}

impl VolumeRoot {
    fn next(&self) -> VolumeRoot {
        VolumeRoot {
            epoch: self.epoch + 1,
            instance: self.instance,
            committed: Arc::clone(&self.committed),
            generation: self.generation,
            pending: Arc::clone(&self.pending),
            ledger: Arc::clone(&self.ledger),
            sessions: Arc::clone(&self.sessions),
            commit_slot: self.commit_slot.clone(),
            capability: self.capability.clone(),
        }
    }
}

// ---------- 等待者、统计、错误 ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiter {
    pub id: u64,
    pub path: Path,
    pub len_required: u64,
    pub notified: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishStats {
    /// 累计复制的目录节点数。
    pub nodes_copied: usize,
    /// 最近一次发布复制的目录节点数。
    pub last_publish_nodes_copied: usize,
    pub publishes: u64,
    pub poisoned: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    NotWritable(String),
    InsufficientCapacity {
        requested: u64,
        available: u64,
    },
    SessionInvalid(SessionId),
    PathBusy(Path),
    NoSuchPending(Path),
    CommitSlotOccupied(BatchId),
    /// 批次来自已被替换的运行实例/恢复上下文。
    StaleInstance {
        batch: InstanceId,
        current: InstanceId,
    },
    EvidenceMismatch {
        expected: BatchId,
        got: BatchId,
    },
    /// 根构造失败，卷已置为不可信；返回仍可通知的等待者。
    PublishFailed {
        notified: Vec<u64>,
    },
}

/// 发布结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub epoch: u64,
    pub generation: u64,
    /// 本次被覆盖并通知的等待者。
    pub notified: Vec<u64>,
    /// 重复发布：状态未变化。
    pub duplicate: bool,
}

/// S4 证据的最小表示：调用方证明该批次已可靠落带。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S4Evidence {
    pub batch_id: BatchId,
    pub generation: u64,
}

// ---------- 顺序器 ----------

struct Inner {
    current: Arc<VolumeRoot>,
    poison: Arc<VolumeRoot>,
    retired: Vec<Arc<VolumeRoot>>,
    waiters: Vec<Waiter>,
    next_batch: BatchId,
    next_version: u64,
    next_reservation: ReservationId,
    next_waiter: u64,
    /// 测试注入：下一次发布在 CP5 前失败。
    fail_next_publish: bool,
    stats: PublishStats,
}

/// 每卷状态顺序器。
pub struct VolumeState {
    inner: Mutex<Inner>,
}

fn build_poison(root: &Arc<VolumeRoot>) -> Arc<VolumeRoot> {
    let mut p = root.next();
    p.capability = Capability {
        trusted: false,
        readable: false,
        writable: false,
        reason: Some("发布失败，视图不可信，需重建".to_string()),
    };
    Arc::new(p)
}

impl VolumeState {
    /// 以恢复得到的已提交视图建立初始根。
    pub fn new(
        instance: InstanceId,
        committed: Arc<DirNode>,
        generation: u64,
        baseline_free: u64,
        writable: bool,
    ) -> Self {
        let root = Arc::new(VolumeRoot {
            epoch: 1,
            instance,
            committed,
            generation,
            pending: Arc::new(BTreeMap::new()),
            ledger: Arc::new(Ledger {
                baseline_free,
                calibration_epoch: 1,
                consumed_since_cut: 0,
                reservations: BTreeMap::new(),
            }),
            sessions: Arc::new(BTreeMap::new()),
            commit_slot: None,
            capability: Capability {
                trusted: true,
                readable: true,
                writable,
                reason: None,
            },
        });
        let poison = build_poison(&root);
        Self {
            inner: Mutex::new(Inner {
                current: root,
                poison,
                retired: Vec::new(),
                waiters: Vec::new(),
                next_batch: 1,
                next_version: 1,
                next_reservation: 1,
                next_waiter: 1,
                fail_next_publish: false,
                stats: PublishStats::default(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 读者取整根：O(1)，持有期间内容不变。
    pub fn load(&self) -> Arc<VolumeRoot> {
        Arc::clone(&self.lock().current)
    }

    pub fn stats(&self) -> PublishStats {
        self.lock().stats.clone()
    }

    /// 回收已无读者的旧根，返回回收数量；有读者的保留。
    pub fn reclaim(&self) -> usize {
        let mut g = self.lock();
        let before = g.retired.len();
        g.retired.retain(|r| Arc::strong_count(r) > 1);
        before - g.retired.len()
    }

    pub fn retired_count(&self) -> usize {
        self.lock().retired.len()
    }

    /// 测试注入：下一次 publish 在根构造后、替换前失败。
    pub fn fail_next_publish(&self) {
        self.lock().fail_next_publish = true;
    }

    /// 顺序器内的单指针替换；旧根进入 retired，并重建 poison。
    fn swap(g: &mut Inner, new_root: VolumeRoot) -> Arc<VolumeRoot> {
        let new_root = Arc::new(new_root);
        let old = std::mem::replace(&mut g.current, Arc::clone(&new_root));
        g.retired.push(old);
        g.poison = build_poison(&new_root);
        new_root
    }

    /// 失效标记：换入预构造的 poisoned 根，无分配。
    pub fn poison(&self) -> Arc<VolumeRoot> {
        let mut g = self.lock();
        Self::poison_locked(&mut g)
    }

    fn poison_locked(g: &mut Inner) -> Arc<VolumeRoot> {
        let p = Arc::clone(&g.poison);
        let old = std::mem::replace(&mut g.current, Arc::clone(&p));
        g.retired.push(old);
        g.stats.poisoned += 1;
        p
    }

    // ----- 会话 -----

    pub fn open_session(&self, session: SessionId, deadline: u64) -> u64 {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut sessions = (*root.sessions).clone();
        sessions.insert(
            session,
            Session {
                deadline,
                state: SessionState::Active,
                refs: 0,
                renewed_at_epoch: root.epoch,
            },
        );
        root.sessions = Arc::new(sessions);
        Self::swap(&mut g, root).epoch
    }

    /// 续期：只对 Active 会话有效；记录发生时的 epoch。
    pub fn renew(&self, session: SessionId, new_deadline: u64) -> Result<u64, StateError> {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut sessions = (*root.sessions).clone();
        let s = sessions
            .get_mut(&session)
            .ok_or(StateError::SessionInvalid(session))?;
        if s.state != SessionState::Active {
            return Err(StateError::SessionInvalid(session));
        }
        s.deadline = s.deadline.max(new_deadline);
        s.renewed_at_epoch = root.epoch;
        root.sessions = Arc::new(sessions);
        Ok(Self::swap(&mut g, root).epoch)
    }

    /// 到期检查事件：`observed_epoch` 是发起检查时读到的 epoch；对其后续期过的会话作废。
    /// 返回本次转为 Closing/Expired 的会话。
    pub fn tick(&self, now: u64, observed_epoch: u64) -> Vec<SessionId> {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut sessions = (*root.sessions).clone();
        let mut ledger = (*root.ledger).clone();
        let mut pending = (*root.pending).clone();
        let mut changed = Vec::new();
        for (id, s) in sessions.iter_mut() {
            if s.state != SessionState::Active
                || s.renewed_at_epoch > observed_epoch
                || now < s.deadline
            {
                continue;
            }
            changed.push(*id);
            Self::end_session(*id, s, &mut ledger, &mut pending);
        }
        if changed.is_empty() {
            return changed;
        }
        root.sessions = Arc::new(sessions);
        root.ledger = Arc::new(ledger);
        root.pending = Arc::new(pending);
        Self::swap(&mut g, root);
        changed
    }

    /// 取消：停止后续工作，不回滚；安全释放确定未消耗且无在途引用的预留。
    pub fn cancel(&self, session: SessionId) -> Result<u64, StateError> {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut sessions = (*root.sessions).clone();
        let mut ledger = (*root.ledger).clone();
        let mut pending = (*root.pending).clone();
        let s = sessions
            .get_mut(&session)
            .ok_or(StateError::SessionInvalid(session))?;
        if s.state == SessionState::Active {
            Self::end_session(session, s, &mut ledger, &mut pending);
        }
        root.sessions = Arc::new(sessions);
        root.ledger = Arc::new(ledger);
        root.pending = Arc::new(pending);
        Ok(Self::swap(&mut g, root).epoch)
    }

    fn end_session(
        id: SessionId,
        s: &mut Session,
        ledger: &mut Ledger,
        pending: &mut BTreeMap<Path, PendingEntry>,
    ) {
        s.state = if s.refs == 0 {
            SessionState::Expired
        } else {
            SessionState::Closing
        };
        // 未进入任何批次的 in_progress 条目结束；已冻结/就绪的保留由发布决定
        pending.retain(|_, e| {
            !(e.session == id && e.state == PendingState::InProgress && e.committed_prefix == 0)
        });
        if s.refs == 0 {
            for r in ledger.reservations.values_mut() {
                if r.session == id && !r.released {
                    r.released = true;
                }
            }
        }
    }

    // ----- 准入与接收 -----

    /// 准入：预留 `reserve` 字节预算，登记 in_progress 条目。
    pub fn admit(
        &self,
        session: SessionId,
        path: &str,
        reserve: u64,
        now: u64,
    ) -> Result<ReservationId, StateError> {
        self.admit_entry(session, path, reserve, now, false)
    }

    /// 准入一次删除：不预留预算，登记 in_progress 条目占住路径，之后 `complete` 入批。
    /// 发布时路径从已提交视图里消失。与写入一样，同一路径同时只能有一个未提交的变更。
    pub fn admit_removal(
        &self,
        session: SessionId,
        path: &str,
        now: u64,
    ) -> Result<ReservationId, StateError> {
        self.admit_entry(session, path, 0, now, true)
    }

    fn admit_entry(
        &self,
        session: SessionId,
        path: &str,
        reserve: u64,
        now: u64,
        removal: bool,
    ) -> Result<ReservationId, StateError> {
        let mut g = self.lock();
        let cur = Arc::clone(&g.current);
        if !cur.capability.writable {
            return Err(StateError::NotWritable(
                cur.capability.reason.clone().unwrap_or_default(),
            ));
        }
        match cur.sessions.get(&session) {
            Some(s) if s.state == SessionState::Active => {}
            _ => return Err(StateError::SessionInvalid(session)),
        }
        if cur.pending.contains_key(path) {
            return Err(StateError::PathBusy(path.to_string()));
        }
        let available = cur.ledger.available();
        if reserve > available {
            return Err(StateError::InsufficientCapacity {
                requested: reserve,
                available,
            });
        }
        let rid = g.next_reservation;
        g.next_reservation += 1;
        let version = g.next_version;
        g.next_version += 1;
        let mut root = cur.next();
        let mut ledger = (*root.ledger).clone();
        ledger.reservations.insert(
            rid,
            Reservation {
                session,
                path: path.to_string(),
                reserved: reserve,
                consumed: 0,
                released: false,
            },
        );
        let mut pending = (*root.pending).clone();
        pending.insert(
            path.to_string(),
            PendingEntry {
                session,
                version,
                state: PendingState::InProgress,
                staged_len: 0,
                committed_prefix: 0,
                earliest_registered: now,
                reservation: rid,
                removal,
            },
        );
        root.ledger = Arc::new(ledger);
        root.pending = Arc::new(pending);
        Self::swap(&mut g, root);
        Ok(rid)
    }

    /// 接收 `bytes` 字节：消耗预留、推进 staged_len。
    pub fn ingest(&self, path: &str, bytes: u64) -> Result<u64, StateError> {
        let mut g = self.lock();
        let cur = Arc::clone(&g.current);
        let e = cur
            .pending
            .get(path)
            .ok_or_else(|| StateError::NoSuchPending(path.to_string()))?;
        if e.state != PendingState::InProgress {
            return Err(StateError::PathBusy(path.to_string()));
        }
        match cur.sessions.get(&e.session) {
            Some(s) if s.state == SessionState::Active => {}
            _ => return Err(StateError::SessionInvalid(e.session)),
        }
        let mut ledger = (*cur.ledger).clone();
        let r = ledger
            .reservations
            .get_mut(&e.reservation)
            .expect("reservation exists");
        if bytes > r.unconsumed() {
            return Err(StateError::InsufficientCapacity {
                requested: bytes,
                available: r.unconsumed(),
            });
        }
        r.consumed += bytes;
        ledger.consumed_since_cut += bytes;
        let mut pending = (*cur.pending).clone();
        pending.get_mut(path).unwrap().staged_len += bytes;
        let mut root = cur.next();
        root.ledger = Arc::new(ledger);
        root.pending = Arc::new(pending);
        Ok(Self::swap(&mut g, root).epoch)
    }

    /// 同卷改名引用已有 extent：发布长度，但不重复消耗物理容量。
    pub(crate) fn stage_reference(&self, path: &str, len: u64) -> Result<(), StateError> {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut pending = (*root.pending).clone();
        let e = pending
            .get_mut(path)
            .ok_or_else(|| StateError::NoSuchPending(path.into()))?;
        if e.state != PendingState::InProgress || e.removal {
            return Err(StateError::PathBusy(path.into()));
        }
        e.staged_len = len;
        root.pending = Arc::new(pending);
        Self::swap(&mut g, root);
        Ok(())
    }

    pub fn complete(&self, path: &str) -> Result<u64, StateError> {
        let mut g = self.lock();
        let cur = Arc::clone(&g.current);
        if !cur.pending.contains_key(path) {
            return Err(StateError::NoSuchPending(path.to_string()));
        }
        let mut pending = (*cur.pending).clone();
        pending.get_mut(path).unwrap().state = PendingState::Ready;
        let mut root = cur.next();
        root.pending = Arc::new(pending);
        Ok(Self::swap(&mut g, root).epoch)
    }

    /// 登记等待者：要求 `path` 至少 `len_required` 字节被可靠提交。
    pub fn wait_for(&self, path: &str, len_required: u64) -> u64 {
        let mut g = self.lock();
        let id = g.next_waiter;
        g.next_waiter += 1;
        g.waiters.push(Waiter {
            id,
            path: path.to_string(),
            len_required,
            notified: false,
        });
        id
    }

    pub fn waiter(&self, id: u64) -> Option<Waiter> {
        self.lock().waiters.iter().find(|w| w.id == id).cloned()
    }

    // ----- 校准 -----

    /// 容量校准：用设备读数替换基线；切点为当前 epoch，切点后消耗清零。
    pub fn calibrate(&self, device_free: u64) -> u64 {
        let mut g = self.lock();
        let mut root = g.current.next();
        let mut ledger = (*root.ledger).clone();
        ledger.baseline_free = device_free;
        ledger.calibration_epoch = root.epoch;
        ledger.consumed_since_cut = 0;
        root.ledger = Arc::new(ledger);
        Self::swap(&mut g, root).epoch
    }

    // ----- S2 冻结 -----

    /// 冻结批次：全部 Ready 条目按完整文件覆盖；`snapshot_open` 中的 in_progress 条目按当前长度覆盖前缀。
    /// 首版每卷同时只允许一个批次进入索引写入。
    pub fn freeze(&self, snapshot_open: &[&str]) -> Result<Arc<FrozenBatch>, StateError> {
        let mut g = self.lock();
        let cur = Arc::clone(&g.current);
        if let Some(slot) = &cur.commit_slot {
            return Err(StateError::CommitSlotOccupied(slot.batch_id));
        }
        if !cur.capability.writable {
            return Err(StateError::NotWritable(
                cur.capability.reason.clone().unwrap_or_default(),
            ));
        }
        let mut cover = BTreeMap::new();
        let mut sessions_in_batch = Vec::new();
        for (path, e) in cur.pending.iter() {
            let complete = e.state == PendingState::Ready;
            let snap = snapshot_open.contains(&path.as_str());
            if !(complete || snap) {
                continue;
            }
            if e.staged_len <= e.committed_prefix && !complete {
                continue;
            }
            cover.insert(
                path.clone(),
                CoverEntry {
                    version: e.version,
                    len: e.staged_len,
                    complete,
                    removal: e.removal,
                },
            );
            if !sessions_in_batch.contains(&e.session) {
                sessions_in_batch.push(e.session);
            }
        }
        let batch_id = g.next_batch;
        g.next_batch += 1;
        let batch = Arc::new(FrozenBatch {
            batch_id,
            instance: cur.instance,
            base_epoch: cur.epoch,
            generation: cur.generation + 1,
            cover,
            sessions: sessions_in_batch.clone(),
        });
        let mut root = cur.next();
        let mut sessions = (*root.sessions).clone();
        for sid in &sessions_in_batch {
            if let Some(s) = sessions.get_mut(sid) {
                s.refs += 1;
            }
        }
        root.sessions = Arc::new(sessions);
        root.commit_slot = Some(Arc::clone(&batch));
        Self::swap(&mut g, root);
        Ok(batch)
    }

    // ----- CP2—CP6 发布 -----

    /// 发布：以 S4 证据为准，在当前根上计算差量并原子替换。
    pub fn publish(
        &self,
        batch: &FrozenBatch,
        evidence: S4Evidence,
    ) -> Result<PublishOutcome, StateError> {
        self.publish_namespace(batch, evidence, &std::collections::BTreeSet::new())
    }

    pub(crate) fn publish_namespace(
        &self,
        batch: &FrozenBatch,
        evidence: S4Evidence,
        directories: &std::collections::BTreeSet<String>,
    ) -> Result<PublishOutcome, StateError> {
        let mut g = self.lock();
        // CP2：证据必须精确对应批次
        if evidence.batch_id != batch.batch_id {
            return Err(StateError::EvidenceMismatch {
                expected: batch.batch_id,
                got: evidence.batch_id,
            });
        }
        // CP3：读取当前状态
        let cur = Arc::clone(&g.current);
        if cur.instance != batch.instance {
            return Err(StateError::StaleInstance {
                batch: batch.instance,
                current: cur.instance,
            });
        }
        let notified = Self::notify_locked(&mut g, batch);
        match &cur.commit_slot {
            Some(slot) if slot.batch_id == batch.batch_id => {}
            _ => {
                // 重复或已收束的批次：状态不再转换，只返回覆盖者
                return Ok(PublishOutcome {
                    epoch: cur.epoch,
                    generation: cur.generation,
                    notified,
                    duplicate: true,
                });
            }
        }

        // CP4：按身份计算差量（只读 cur 与 batch，不做 I/O）
        let mut stats_delta = PublishStats::default();
        let mut committed = Arc::clone(&cur.committed);
        let mut pending = (*cur.pending).clone();
        let mut ledger = (*cur.ledger).clone();
        let mut sessions = (*cur.sessions).clone();
        for (path, c) in &batch.cover {
            let Some((dirs, name)) = split_path(path) else {
                continue;
            };
            let fv = (!c.removal).then(|| {
                Arc::new(FileVersion {
                    version: c.version,
                    len: c.len,
                    partial: !c.complete,
                })
            });
            if directories.contains(path) {
                let mut parts = dirs;
                parts.push(name);
                committed = committed.with_directory(&parts, c.removal, &mut stats_delta);
            } else {
                committed = committed.with_file(&dirs, name, fv, &mut stats_delta);
            }
            match pending.get_mut(path) {
                Some(e) if e.version == c.version => {
                    if c.complete && e.staged_len == c.len {
                        let rid = e.reservation;
                        pending.remove(path);
                        if let Some(r) = ledger.reservations.get_mut(&rid)
                            && !r.released
                        {
                            r.released = true; // 只释放未消耗余额；已消耗的物理占用不退还
                        }
                    } else {
                        // 仅前缀被覆盖：保留剩余范围与最早登记时刻
                        e.committed_prefix = c.len;
                    }
                }
                _ => {
                    // 冻结后被替换（版本不同）或已取消移除：不触碰当前条目
                }
            }
        }
        for sid in &batch.sessions {
            if let Some(s) = sessions.get_mut(sid) {
                s.refs = s.refs.saturating_sub(1);
                if s.state == SessionState::Closing && s.refs == 0 {
                    s.state = SessionState::Expired;
                    for r in ledger.reservations.values_mut() {
                        if r.session == *sid && !r.released {
                            r.released = true;
                        }
                    }
                }
            }
        }
        let mut root = cur.next();
        root.committed = committed;
        root.generation = evidence.generation;
        root.pending = Arc::new(pending);
        root.ledger = Arc::new(ledger);
        root.sessions = Arc::new(sessions);
        root.commit_slot = None;

        // CP5：一致发布（或失效标记）
        if g.fail_next_publish {
            g.fail_next_publish = false;
            Self::poison_locked(&mut g);
            return Err(StateError::PublishFailed { notified });
        }
        let new_root = Self::swap(&mut g, root);
        g.stats.nodes_copied += stats_delta.nodes_copied;
        g.stats.last_publish_nodes_copied = stats_delta.nodes_copied;
        g.stats.publishes += 1;
        Ok(PublishOutcome {
            epoch: new_root.epoch,
            generation: new_root.generation,
            notified,
            duplicate: false,
        })
    }

    /// CP6：通知精确覆盖者；重复调用不重复通知。
    fn notify_locked(g: &mut Inner, batch: &FrozenBatch) -> Vec<u64> {
        let mut out = Vec::new();
        for w in g.waiters.iter_mut() {
            if w.notified {
                continue;
            }
            if let Some(c) = batch.cover.get(&w.path)
                && c.len >= w.len_required
            {
                w.notified = true;
                out.push(w.id);
            }
        }
        out
    }

    /// 通知重试：只返回本批已通知过的等待者，不转换任何状态。
    pub fn renotify(&self, batch: &FrozenBatch) -> Vec<u64> {
        let g = self.lock();
        g.waiters
            .iter()
            .filter(|w| w.notified && batch.cover.contains_key(&w.path))
            .map(|w| w.id)
            .collect()
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn rename_reference_uses_no_capacity_and_removals_do_not_recreate_ancestors() {
        let mut root = Arc::new(DirNode::default());
        let mut stats = PublishStats::default();
        root = root.with_directory(&["a", "empty"], false, &mut stats);
        root = root.with_file(
            &["a"],
            "file",
            Some(Arc::new(FileVersion {
                version: 1,
                len: 4096,
                partial: false,
            })),
            &mut stats,
        );
        let state = VolumeState::new(1, root, 1, 0, true);
        state.open_session(1, u64::MAX);
        for p in ["/a", "/a/empty", "/a/file"] {
            state.admit_removal(1, p, 0).unwrap();
            state.complete(p).unwrap();
        }
        for (p, n) in [("/b", 0), ("/b/empty", 0), ("/b/file", 4096)] {
            state.admit(1, p, 0, 0).unwrap();
            state.stage_reference(p, n).unwrap();
            state.complete(p).unwrap();
        }
        let batch = state.freeze(&[]).unwrap();
        state
            .publish_namespace(
                &batch,
                S4Evidence {
                    batch_id: batch.batch_id,
                    generation: batch.generation,
                },
                &["/a", "/a/empty", "/b", "/b/empty"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            )
            .unwrap();
        let root = state.load();
        assert!(!root.committed.subdirs.contains_key("a"));
        assert_eq!(root.committed.get("/b/file").unwrap().len, 4096);
        assert_eq!(root.ledger.consumed_since_cut, 0);
        assert!(root.pending.is_empty());
    }

    #[test]
    fn directory_publication_is_atomic_and_does_not_create_a_file() {
        let state = VolumeState::new(1, Arc::new(DirNode::default()), 1, 1024, true);
        let directories = ["/empty".to_string()].into_iter().collect();
        state.open_session(1, u64::MAX);
        state.admit(1, "/empty", 0, 0).unwrap();
        state.complete("/empty").unwrap();
        let batch = state.freeze(&[]).unwrap();
        let before = state.load();
        let proof = S4Evidence {
            batch_id: batch.batch_id,
            generation: batch.generation,
        };
        state
            .publish_namespace(&batch, proof, &directories)
            .unwrap();
        assert!(before.committed.subdirs.is_empty());
        let created = state.load();
        assert!(created.committed.subdirs.contains_key("empty"));
        assert!(created.committed.files.is_empty());
        assert!(created.pending.is_empty());
        state.open_session(2, u64::MAX);
        state.admit_removal(2, "/empty", 1).unwrap();
        state.complete("/empty").unwrap();
        let batch = state.freeze(&[]).unwrap();
        let proof = S4Evidence {
            batch_id: batch.batch_id,
            generation: batch.generation,
        };
        state
            .publish_namespace(&batch, proof, &directories)
            .unwrap();
        assert!(created.committed.subdirs.contains_key("empty"));
        assert!(state.load().committed.subdirs.is_empty());
        assert!(state.load().committed.files.is_empty());
    }
}
