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

use super::executor::ExecRequest;

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
    pub version: u64,
    pub generation: u64,
    pub round: u64,
}

struct Serving {
    round: u64,
    state: Arc<VolumeState>,
    queue: VecDeque<Upload>,
    in_batch: Vec<u64>,
    opened: Instant,
}

struct Inner {
    serving: Option<Serving>,
    why_not: String,
    tasks: HashMap<u64, TaskStatus>,
    next_task: u64,
}

pub struct FileService {
    inner: Mutex<Inner>,
    changed: Condvar,
    spool_dir: PathBuf,
    exec: Mutex<Option<Sender<ExecRequest>>>,
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
        // 暂存区不跨进程保留：上一次运行留下的都是未落带的残留
        let _ = std::fs::remove_dir_all(&spool_dir);
        std::fs::create_dir_all(&spool_dir)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner { serving: None, why_not: "尚未接管".into(), tasks: HashMap::new(), next_task: 1 }),
            changed: Condvar::new(),
            spool_dir,
            exec: Mutex::new(None),
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
    pub fn open(&self, round: u64, index: &LtfsIndex, free_bytes: u64, writable: bool) {
        let state = Arc::new(VolumeState::new(round, committed_view(index), index.generation, free_bytes, writable));
        let mut g = self.lock();
        g.serving = Some(Serving { round, state, queue: VecDeque::new(), in_batch: Vec::new(), opened: Instant::now() });
        g.why_not.clear();
        self.changed.notify_all();
    }

    /// 停止服务。仅暂存的任务判失败，已进入提交批次的判结果未定。
    pub fn close(&self, reason: &str) {
        let mut g = self.lock();
        let Some(s) = g.serving.take() else {
            g.why_not = reason.to_string();
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
        self.changed.notify_all();
    }

    pub fn serving_round(&self) -> Option<u64> {
        self.lock().serving.as_ref().map(|s| s.round)
    }

    /// 取出全部已完成的上传并冻结批次。两件事在同一把锁下完成。
    pub fn take_batch(&self, round: u64) -> Option<(Arc<FrozenBatch>, Vec<Upload>)> {
        let mut g = self.lock();
        let s = g.serving.as_mut().filter(|s| s.round == round)?;
        if s.queue.is_empty() || !s.in_batch.is_empty() {
            return None;
        }
        let batch = s.state.freeze(&[]).ok()?;
        let uploads: Vec<Upload> = s.queue.drain(..).collect();
        debug_assert!(uploads.iter().all(|u| batch.cover.contains_key(&u.path)));
        s.in_batch = uploads.iter().map(|u| u.task).collect();
        Some((batch, uploads))
    }

    /// 批次落带结果。成功则发布到已提交视图；失败则这些任务结果未定，由调用方决定是否停止服务。
    pub fn batch_done(&self, round: u64, batch: &FrozenBatch, uploads: &[Upload], result: Result<u64, String>) {
        let mut g = self.lock();
        let Some(s) = g.serving.as_mut().filter(|s| s.round == round) else {
            return;
        };
        s.in_batch.clear();
        let outcome = match result {
            Ok(tape_generation) => {
                match s.state.publish(batch, S4Evidence { batch_id: batch.batch_id, generation: batch.generation }) {
                    Ok(_) => Ok(tape_generation),
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
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        let now = s.opened.elapsed().as_secs();
        s.state.open_session(task, u64::MAX);
        s.state.admit(task, &path, len, now).map_err(map_state)?;
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
            g.tasks.insert(h.task, TaskStatus::Staged);
        }
        self.kick();
        Ok(h.task)
    }

    pub fn abort(&self, h: UploadHandle) {
        let _ = std::fs::remove_file(&h.spool);
        let g = self.lock();
        if let Some(s) = g.serving.as_ref().filter(|s| s.round == h.round) {
            let _ = s.state.cancel(h.task);
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

    /// 已提交视图里的文件。只反映已落带的内容。
    pub fn stat(&self, path: &str) -> Result<Option<Stat>, ServiceError> {
        let path = norm(path)?;
        let g = self.lock();
        let s = g.serving.as_ref().ok_or(ServiceError::NotServing(g.why_not.clone()))?;
        let root = s.state.load();
        Ok(root.committed.get(&path).map(|f| Stat { len: f.len, version: f.version, generation: root.generation, round: s.round }))
    }

    pub fn list(&self) -> Result<BTreeMap<String, u64>, ServiceError> {
        let g = self.lock();
        let s = g.serving.as_ref().ok_or(ServiceError::NotServing(g.why_not.clone()))?;
        let root = s.state.load();
        let mut out = BTreeMap::new();
        fn walk(d: &DirNode, prefix: &str, out: &mut BTreeMap<String, u64>) {
            for (n, f) in &d.files {
                out.insert(format!("{}/{}", prefix, n), f.len);
            }
            for (n, s) in &d.subdirs {
                walk(s, &format!("{}/{}", prefix, n), out);
            }
        }
        walk(&root.committed, "", &mut out);
        Ok(out)
    }
}
