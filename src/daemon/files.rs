//! 文件服务：上传、查询、列目录。HTTP 线程与设备执行线程共享这一个对象。
//!
//! 语义（研究笔记 file-contract.md）：上传被接受只表示已暂存在本节点；**归档成功以卷提交为准**。
//! 每个卷的状态由 `core::volume_state::VolumeState` 维护：准入 → 接收 → 完成 → 冻结批次 →
//! 落带提交 → 发布。"完成并入队"和"取队列并冻结"在同一把锁下做，所以冻结批次覆盖的
//! 文件集合与实际写带的集合严格一致。
//!
//! 暂存区不复制。执行者更换时：仅暂存的任务失败（向新 Leader 重传）；已进入提交批次的
//! 任务结果未定（向新 Leader 查询该路径即可判定）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::core::volume_state::{DirNode, FileVersion, FrozenBatch, S4Evidence, StateError, VolumeState};
use crate::ltfs::index::{DirectoryNode, LtfsIndex};

use super::directory::Directory;
use super::executor::ExecRequest;

/// 合批策略：四个触发条件任一满足就提交一批。推导见研究笔记 pooling-slice-plan.md。
/// 字节数和文件数是触发阈值，不是硬上限：到达阈值就提交，取队列时把已到齐的全部带走。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchPolicy {
    /// 队列里累计的字节数
    pub max_bytes: u64,
    /// 队列里累计的文件数
    pub max_files: usize,
    /// 这么久没有新的上传完成就提交（负载轻时延迟低）
    pub idle: Duration,
    /// 队列里最早的文件最多等这么久（带 wait 的上传的延迟上界）
    pub max_wait: Duration,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self { max_bytes: 8 << 30, max_files: 20_000, idle: Duration::from_secs(2), max_wait: Duration::from_secs(60) }
    }
}

/// 当前服务的这盘带。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeIdent {
    pub barcode: String,
    pub pool_uuid: String,
}

/// 这盘带的限制，开放服务时给出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeLimits {
    /// 文件数软上限（来自池）
    pub file_limit: u64,
    /// 扣除保留空间后，一盘空带最多能放多少字节。用来判定"比单盘还大"
    pub usable_capacity: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// 已完整暂存在本节点，等待落带。不是持久化确认。
    Staged,
    /// 已随卷提交落带。`generation` 是包含它的索引代数。
    Committed { generation: u64 },
    /// 确定没有落带。可以安全重传。
    Failed { reason: String },
    /// 已进入提交批次，但提交没有得到确认。向当前 Leader 查询该路径判定。
    Indeterminate { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// 本节点当前不在服务（不是执行者，或还在接管途中）。
    NotServing(String),
    PathBusy(String),
    InsufficientCapacity { requested: u64, available: u64 },
    NotWritable(String),
    BadPath(String),
    Io(String),
    /// 当前磁带不能再接收这个文件，正在换下一盘。稍后重试即可。
    SwitchingTape(String),
    /// 池里没有可写的磁带了。确定的失败，重试无益，需要运维加带。
    NoTape(String),
    /// 文件比单盘磁带还大。确定的失败。
    TooLarge { requested: u64, tape_capacity: u64 },
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::NotServing(s) => write!(f, "本节点不在服务: {}", s),
            ServiceError::PathBusy(p) => write!(f, "路径正在上传: {}", p),
            ServiceError::InsufficientCapacity { requested, available } => {
                write!(f, "空间不足: 需要 {} 可用 {}", requested, available)
            }
            ServiceError::NotWritable(s) => write!(f, "卷只读: {}", s),
            ServiceError::BadPath(p) => write!(f, "非法路径: {}", p),
            ServiceError::Io(e) => write!(f, "暂存区 I/O: {}", e),
            ServiceError::SwitchingTape(s) => write!(f, "正在换带: {}", s),
            ServiceError::NoTape(s) => write!(f, "池里没有可写的磁带: {}", s),
            ServiceError::TooLarge { requested, tape_capacity } => {
                write!(f, "文件 {} 字节，超过单盘可用容量 {} 字节", requested, tape_capacity)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Upload {
    pub task: u64,
    pub path: String,
    pub spool: PathBuf,
    pub len: u64,
}

/// 上传途中的句柄。`finish` 或 `abort` 二选一。
pub struct UploadHandle {
    pub task: u64,
    pub path: String,
    pub spool: PathBuf,
    round: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub len: u64,
    pub generation: u64,
    pub round: u64,
    /// 所在磁带的条码
    pub barcode: String,
    /// 内容的 sha256（十六进制小写）；文件没有哈希属性时为空串
    pub sha256: String,
}

struct Serving {
    round: u64,
    state: Arc<VolumeState>,
    queue: VecDeque<Upload>,
    queue_bytes: u64,
    /// 队列里最早一个文件的入队时刻、最近一次入队的时刻
    oldest: Option<Instant>,
    newest: Option<Instant>,
    in_batch: Vec<u64>,
    opened: Instant,
    tape: TapeIdent,
    limits: TapeLimits,
    /// 已准入的上传数（含尚未完成的），与带上已有文件数一起对照文件数上限
    admitted: u64,
    /// 需要换带：新状态（data_full / full）。置位后不再准入，等执行线程把队列落带后换带
    switch: Option<&'static str>,
    /// 当前这盘带上的文件：开放服务时取自刚读到的索引，之后每次卷提交追加。
    /// 目录条目经 Raft 应用有延迟（也可能因换届而缺失，要等对账），当前带的查询不依赖它。
    recent: HashMap<String, Stat>,
}

struct Inner {
    serving: Option<Serving>,
    why_not: String,
    /// 不在服务的原因是"池里没有可写的带"（确定的失败），而不是"还没接管"（可重试）
    no_tape: bool,
    /// 没有可写的带时仍然可以回答查询：执行轮次与要查的池
    read_only: Option<(u64, String)>,
    tasks: HashMap<u64, TaskStatus>,
    next_task: u64,
}

pub struct FileService {
    inner: Mutex<Inner>,
    changed: Condvar,
    spool_dir: PathBuf,
    exec: Mutex<Option<Sender<ExecRequest>>>,
    policy: BatchPolicy,
    directory_path: Option<PathBuf>,
    reader: Mutex<Option<Directory>>,
}

fn norm(path: &str) -> Result<String, ServiceError> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "." || *p == "..") || path.ends_with('/') {
        return Err(ServiceError::BadPath(path.to_string()));
    }
    Ok(format!("/{}", parts.join("/")))
}

fn map_state(e: StateError) -> ServiceError {
    match e {
        StateError::PathBusy(p) => ServiceError::PathBusy(p),
        StateError::InsufficientCapacity { requested, available } => {
            ServiceError::InsufficientCapacity { requested, available }
        }
        StateError::NotWritable(s) => ServiceError::NotWritable(s),
        other => ServiceError::NotServing(format!("{:?}", other)),
    }
}

/// 把卷索引转成卷状态模块的已提交视图。
pub fn committed_view(index: &LtfsIndex) -> Arc<DirNode> {
    fn conv(d: &DirectoryNode) -> DirNode {
        let mut n = DirNode::default();
        for f in &d.files {
            n.files.insert(f.name.clone(), Arc::new(FileVersion { version: f.meta.file_uid, len: f.length, partial: false }));
        }
        for s in &d.subdirs {
            n.subdirs.insert(s.name.clone(), Arc::new(conv(s)));
        }
        n
    }
    Arc::new(conv(&index.root))
}

impl FileService {
    pub fn new(spool_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        Self::with_options(spool_dir, BatchPolicy::default(), None)
    }

    /// `directory_path` 是本节点目录库的文件；给了之后 `stat`/`list` 能回答本轮之前提交的文件。
    pub fn with_options(spool_dir: PathBuf, policy: BatchPolicy, directory_path: Option<PathBuf>) -> std::io::Result<Arc<Self>> {
        // 暂存区不跨进程保留：上一次运行留下的都是未落带的残留
        let _ = std::fs::remove_dir_all(&spool_dir);
        std::fs::create_dir_all(&spool_dir)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner { serving: None, why_not: "尚未接管".into(), no_tape: false, read_only: None, tasks: HashMap::new(), next_task: 1 }),
            changed: Condvar::new(),
            spool_dir,
            exec: Mutex::new(None),
            policy,
            directory_path,
            reader: Mutex::new(None),
        }))
    }

    pub fn set_executor(&self, tx: Sender<ExecRequest>) {
        *self.exec.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kick(&self) {
        if let Some(tx) = self.exec.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(ExecRequest::Work);
        }
    }

    // ---------- 执行线程一侧 ----------

    /// 接管完成：以恢复得到的视图开放服务。
    pub fn open(&self, round: u64, tape: TapeIdent, limits: TapeLimits, index: &LtfsIndex, free_bytes: u64, writable: bool) {
        let state = Arc::new(VolumeState::new(round, committed_view(index), index.generation, free_bytes, writable));
        let mut g = self.lock();
        g.serving = Some(Serving {
            round,
            state,
            queue: VecDeque::new(),
            queue_bytes: 0,
            oldest: None,
            newest: None,
            in_batch: Vec::new(),
            opened: Instant::now(),
            limits,
            admitted: 0,
            switch: None,
            recent: {
                // 当前装着的这盘带以刚读到的索引为准，不必等目录经 Raft 对账
                let mut m = HashMap::new();
                index.walk_files(|p, f| {
                    m.insert(
                        format!("/{}", p),
                        Stat {
                            len: f.length,
                            generation: index.generation,
                            round,
                            barcode: tape.barcode.clone(),
                            sha256: f.xattr(crate::ltfs::index::XATTR_SHA256).unwrap_or("").to_string(),
                        },
                    );
                });
                m
            },
            tape,
        });
        g.why_not.clear();
        g.no_tape = false;
        self.changed.notify_all();
    }

    /// 池里没有可写的磁带：上传以确定的失败拒绝，直到有带可用。
    /// 写不了不影响查：`pool_uuid` 给出时，`stat`/`list` 继续由目录库回答。
    pub fn close_no_tape(&self, round: u64, pool_uuid: Option<String>, reason: &str) {
        self.close(reason);
        let mut g = self.lock();
        g.no_tape = true;
        g.read_only = pool_uuid.map(|p| (round, p));
    }

    /// 写入侧是否空闲：队列空、没有正在提交的批、没有已准入但未完成的上传、没有待处理的换带。
    /// 只有一个驱动器时，只有这时才允许把写入带换下来读别的带。
    pub fn write_side_idle(&self, round: u64) -> bool {
        let g = self.lock();
        g.serving.as_ref().filter(|s| s.round == round).is_some_and(|s| s.queue.is_empty() && s.in_batch.is_empty() && s.admitted == 0 && s.switch.is_none())
    }

    /// 执行线程询问：当前这盘带是否需要换掉，换成什么状态。
    pub fn switch_requested(&self, round: u64) -> Option<&'static str> {
        self.lock().serving.as_ref().filter(|s| s.round == round).and_then(|s| s.switch)
    }

    /// 停止服务。仅暂存的任务判失败，已进入提交批次的判结果未定。
    pub fn close(&self, reason: &str) {
        let mut g = self.lock();
        let Some(s) = g.serving.take() else {
            g.why_not = reason.to_string();
            g.no_tape = false;
            g.read_only = None;
            return;
        };
        for u in &s.queue {
            let _ = std::fs::remove_file(&u.spool);
            g.tasks.insert(u.task, TaskStatus::Failed { reason: format!("未落带：{}。请向新 Leader 重传", reason) });
        }
        for t in &s.in_batch {
            g.tasks.insert(*t, TaskStatus::Indeterminate { reason: format!("提交未确认：{}。请向新 Leader 查询该路径", reason) });
        }
        g.why_not = reason.to_string();
        g.no_tape = false;
        g.read_only = None;
        self.changed.notify_all();
    }

    pub fn serving_round(&self) -> Option<u64> {
        self.lock().serving.as_ref().map(|s| s.round)
    }

    /// 换带前把队列里剩下的全部落带，不等合批条件。
    pub fn take_batch_now(&self, round: u64) -> Option<(Arc<FrozenBatch>, Vec<Upload>)> {
        self.take(round, true)
    }

    /// 距离下一批到期还有多久。`None` 表示队列为空或已有一批在写。
    pub fn due_in(&self, round: u64) -> Option<Duration> {
        let g = self.lock();
        let s = g.serving.as_ref().filter(|s| s.round == round)?;
        Self::due(&self.policy, s)
    }

    fn due(p: &BatchPolicy, s: &Serving) -> Option<Duration> {
        if s.queue.is_empty() || !s.in_batch.is_empty() {
            return None;
        }
        if s.queue_bytes >= p.max_bytes || s.queue.len() >= p.max_files {
            return Some(Duration::ZERO);
        }
        let now = Instant::now();
        let by_wait = s.oldest.map(|t| (t + p.max_wait).saturating_duration_since(now));
        let by_idle = s.newest.map(|t| (t + p.idle).saturating_duration_since(now));
        by_wait.into_iter().chain(by_idle).min()
    }

    /// 合批条件满足时，取出全部已完成的上传并冻结批次。两件事在同一把锁下完成。
    pub fn take_batch(&self, round: u64) -> Option<(Arc<FrozenBatch>, Vec<Upload>)> {
        self.take(round, false)
    }

    fn take(&self, round: u64, now: bool) -> Option<(Arc<FrozenBatch>, Vec<Upload>)> {
        let mut g = self.lock();
        let s = g.serving.as_mut().filter(|s| s.round == round)?;
        // 需要换带时也不再等：尽快把已收到的落带
        if Self::due(&self.policy, s)? > Duration::ZERO && !now && s.switch.is_none() {
            return None;
        }
        let batch = s.state.freeze(&[]).ok()?;
        let uploads: Vec<Upload> = s.queue.drain(..).collect();
        s.queue_bytes = 0;
        s.oldest = None;
        s.newest = None;
        debug_assert!(uploads.iter().all(|u| batch.cover.contains_key(&u.path)));
        s.in_batch = uploads.iter().map(|u| u.task).collect();
        Some((batch, uploads))
    }

    /// 批次落带结果。成功则发布到已提交视图；失败则这些任务结果未定，由调用方决定是否停止服务。
    /// `result` 成功时带索引代数，以及每个文件由写带过程算出的 sha256（路径 → 十六进制）。
    pub fn batch_done(
        &self,
        round: u64,
        batch: &FrozenBatch,
        uploads: &[Upload],
        result: Result<(u64, HashMap<String, String>), String>,
    ) {
        let mut g = self.lock();
        let Some(s) = g.serving.as_mut().filter(|s| s.round == round) else {
            return;
        };
        s.in_batch.clear();
        // 这些上传离开了流水线：成功的从此计入"带上已有文件"，不能再算在已准入里
        s.admitted = s.admitted.saturating_sub(uploads.len() as u64);
        let outcome = match result {
            Ok((tape_generation, hashes)) => {
                match s.state.publish(batch, S4Evidence { batch_id: batch.batch_id, generation: batch.generation }) {
                    Ok(_) => {
                        for u in uploads {
                            let st = Stat {
                                len: u.len,
                                generation: tape_generation,
                                round,
                                barcode: s.tape.barcode.clone(),
                                sha256: hashes.get(&u.path).cloned().unwrap_or_default(),
                            };
                            s.recent.insert(u.path.clone(), st);
                        }
                        Ok(tape_generation)
                    }
                    Err(e) => Err(format!("已落带但发布失败: {:?}", e)),
                }
            }
            Err(e) => Err(e),
        };
        for u in uploads {
            let _ = std::fs::remove_file(&u.spool);
            let st = match &outcome {
                Ok(generation) => TaskStatus::Committed { generation: *generation },
                Err(e) => TaskStatus::Indeterminate { reason: e.clone() },
            };
            g.tasks.insert(u.task, st);
        }
        self.changed.notify_all();
    }

    // ---------- 客户端一侧 ----------

    /// 准入：登记路径并预留空间。之后调用方把内容写进 `handle.spool`，边写边 `ingest`。
    pub fn begin(&self, path: &str, len: u64) -> Result<UploadHandle, ServiceError> {
        let path = norm(path)?;
        let mut g = self.lock();
        let task = g.next_task;
        let why = g.why_not.clone();
        if g.serving.is_none() && g.no_tape {
            return Err(ServiceError::NoTape(why));
        }
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if len > s.limits.usable_capacity {
            return Err(ServiceError::TooLarge { requested: len, tape_capacity: s.limits.usable_capacity });
        }
        if let Some(state) = s.switch {
            return Err(ServiceError::SwitchingTape(format!("{} 已 {}", s.tape.barcode, state)));
        }
        if s.recent.len() as u64 + s.admitted >= s.limits.file_limit {
            s.switch = Some(super::state::tape_state::DATA_FULL);
            let msg = format!("{} 的文件数已到上限 {}", s.tape.barcode, s.limits.file_limit);
            drop(g);
            self.kick();
            return Err(ServiceError::SwitchingTape(msg));
        }
        let now = s.opened.elapsed().as_secs();
        s.state.open_session(task, u64::MAX);
        match s.state.admit(task, &path, len, now) {
            Ok(_) => {}
            Err(StateError::InsufficientCapacity { requested, available }) => {
                // 这盘带放不下，但文件并不比单盘大：换下一盘，让调用方稍后重试
                s.switch = Some(super::state::tape_state::FULL);
                let msg = format!("{} 剩余 {} 字节，放不下 {} 字节", s.tape.barcode, available, requested);
                drop(g);
                self.kick();
                return Err(ServiceError::SwitchingTape(msg));
            }
            Err(e) => return Err(map_state(e)),
        }
        s.admitted += 1;
        let round = s.round;
        g.next_task += 1;
        Ok(UploadHandle { task, spool: self.spool_dir.join(format!("task-{}.part", task)), path, round })
    }

    pub fn ingest(&self, h: &UploadHandle, bytes: u64) -> Result<(), ServiceError> {
        let g = self.lock();
        let s = g.serving.as_ref().filter(|s| s.round == h.round).ok_or(ServiceError::NotServing(g.why_not.clone()))?;
        s.state.ingest(&h.path, bytes).map(|_| ()).map_err(map_state)
    }

    /// 内容已完整暂存：标记完成并入队，唤醒执行线程。
    pub fn finish(&self, h: UploadHandle, len: u64) -> Result<u64, ServiceError> {
        {
            let mut g = self.lock();
            let why = g.why_not.clone();
            let s = g.serving.as_mut().filter(|s| s.round == h.round).ok_or(ServiceError::NotServing(why))?;
            s.state.complete(&h.path).map_err(map_state)?;
            s.queue.push_back(Upload { task: h.task, path: h.path.clone(), spool: h.spool.clone(), len });
            s.queue_bytes += len;
            let now = Instant::now();
            s.oldest.get_or_insert(now);
            s.newest = Some(now);
            g.tasks.insert(h.task, TaskStatus::Staged);
        }
        self.kick();
        Ok(h.task)
    }

    pub fn abort(&self, h: UploadHandle) {
        let _ = std::fs::remove_file(&h.spool);
        let mut g = self.lock();
        if let Some(s) = g.serving.as_mut().filter(|s| s.round == h.round) {
            let _ = s.state.cancel(h.task);
            s.admitted = s.admitted.saturating_sub(1);
        }
    }

    pub fn task(&self, id: u64) -> Option<TaskStatus> {
        self.lock().tasks.get(&id).cloned()
    }

    /// 等到任务离开 `Staged`，或超时。
    pub fn wait_task(&self, id: u64, timeout: Duration) -> Option<TaskStatus> {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            match g.tasks.get(&id) {
                None => return None,
                Some(TaskStatus::Staged) => {}
                Some(done) => return Some(done.clone()),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return g.tasks.get(&id).cloned();
            }
            g = self.changed.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn with_reader<T>(&self, f: impl FnOnce(&Directory) -> Option<T>) -> Option<T> {
        let path = self.directory_path.as_ref()?;
        let mut g = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Directory::open_reader(path).ok();
        }
        f(g.as_ref()?)
    }

    /// 已提交的文件。先看本轮刚落带的，再看目录库（池内全部磁带、之前各轮提交的）。
    pub fn stat(&self, path: &str) -> Result<Option<Stat>, ServiceError> {
        let path = norm(path)?;
        let (round, pool) = {
            let g = self.lock();
            match (&g.serving, &g.read_only) {
                (Some(s), _) => {
                    if let Some(st) = s.recent.get(&path) {
                        return Ok(Some(st.clone()));
                    }
                    (s.round, s.tape.pool_uuid.clone())
                }
                (None, Some((round, pool))) => (*round, pool.clone()),
                (None, None) => return Err(ServiceError::NotServing(g.why_not.clone())),
            }
        };
        Ok(self.with_reader(|d| d.stat(&pool, &path).ok().flatten()).map(|f| Stat {
            len: f.length,
            generation: f.generation,
            round,
            barcode: f.barcode,
            sha256: f.sha256,
        }))
    }

    pub fn list(&self) -> Result<BTreeMap<String, u64>, ServiceError> {
        let (pool, mut out) = {
            let g = self.lock();
            match (&g.serving, &g.read_only) {
                (Some(s), _) => (s.tape.pool_uuid.clone(), s.recent.iter().map(|(p, st)| (p.clone(), st.len)).collect::<BTreeMap<_, _>>()),
                (None, Some((_, pool))) => (pool.clone(), BTreeMap::new()),
                (None, None) => return Err(ServiceError::NotServing(g.why_not.clone())),
            }
        };
        for (p, n) in self.with_reader(|d| d.list(&pool).ok()).unwrap_or_default() {
            out.entry(p).or_insert(n);
        }
        Ok(out)
    }

    /// 当前服务的磁带。
    pub fn tape(&self) -> Option<TapeIdent> {
        self.lock().serving.as_ref().map(|s| s.tape.clone())
    }

}
