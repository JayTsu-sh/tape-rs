//! 文件服务：上传、查询、列目录。HTTP 线程与设备执行线程共享这一个对象。
//!
//! 语义（研究笔记 file-contract.md）：上传被接受只表示已暂存在本节点；**归档成功以卷提交为准**。
//! 每个卷的状态由 `core::volume_state::VolumeState` 维护：准入 → 接收 → 完成 → 冻结批次 →
//! 落带提交 → 发布。"完成并入队"和"取队列并冻结"在同一把锁下做，所以冻结批次覆盖的
//! 文件集合与实际写带的集合严格一致。
//!
//! 暂存区不复制。执行者更换时：仅暂存的任务失败（向新 Leader 重传）；已进入提交批次的
//! 任务结果未定（向新 Leader 查询该路径即可判定）。
//!
//! 删除与上传走同一条路：`delete` 准入一个删除（占住路径、不预留空间）并入队，执行线程在
//! 同一批里把墓碑写到当前写入带上（见 `ltfs::volume::TOMBSTONE_DIR`）。`/.tapers/` 是保留目录，
//! 客户端路径不能落在里面。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::core::volume_state::{DirNode, FileVersion, FrozenBatch, PendingState, S4Evidence, StateError, VolumeState};
use crate::ltfs::index::{DirectoryNode, LtfsIndex};
use crate::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION, is_tombstone_path};

use super::directory::Directory;
use super::executor::ExecRequest;
use super::state::FileRec;

/// 一盘带索引里的全部目录记录与活着的字节数。墓碑按它记的被删路径报告（`deleted`），
/// 墓碑目录下没有 `tapers.deletedPath` 的文件不认。同一盘带上一个路径只应有一条记录
/// （写入会去掉同带上的墓碑，删除会去掉同带上的文件）。外部 LTFS 改回旧名时
/// 可能保留私有墓碑；此时原生节点是本卷现状，优先于墓碑，但不提升其跨带版本。
pub fn catalog_of(index: &LtfsIndex) -> (Vec<FileRec>, u64) {
    let mut by_path: BTreeMap<String, FileRec> = BTreeMap::new();
    let mut bytes = 0u64;
    index.walk_files(|p, f| {
        let version = f.xattr(XATTR_VERSION).and_then(parse_version).unwrap_or((0, 0));
        let rec = if is_tombstone_path(p) {
            let Some(target) = f.xattr(XATTR_DELETED_PATH) else { return };
            FileRec { metadata: if f.xattr("tapers.deletedKind") == Some("directory") { serde_json::json!({"kind":"directory"}) } else { serde_json::Value::Null }, path: target.to_string(), length: 0, sha256: String::new(), version, deleted: true }
        } else {
            bytes += f.length;
            let mut metadata = serde_json::json!({
                "modify_time": f.meta.modify_time,
                "xattrs": f.xattrs.iter().map(|x| serde_json::json!({"key": x.key, "value": x.value, "base64": x.base64})).collect::<Vec<_>>()
            });
            if let Some(target) = &f.symlink {
                metadata["kind"] = serde_json::json!("symlink");
                metadata["symlink"] = serde_json::json!(target);
            }
            FileRec { metadata,
                path: format!("/{}", p),
                length: f.length,
                sha256: f.xattr(crate::ltfs::index::XATTR_SHA256).unwrap_or("").to_string(),
                version,
                deleted: false,
            }
        };
        match by_path.get(&rec.path) {
            Some(old)
                if (rec.deleted && !old.deleted)
                    || (old.deleted == rec.deleted && old.version >= rec.version) => {}
            _ => {
                by_path.insert(rec.path.clone(), rec);
            }
        }
    });
    index.walk_directories(|p, d| {
        if p == ".tapers" || p.starts_with(".tapers/") { return; }
        let version = d.xattrs.iter().find(|x| x.key == XATTR_VERSION).and_then(|x| parse_version(&x.value)).unwrap_or_default();
        let rec = FileRec {
            path: format!("/{}", p), length: 0, sha256: String::new(), version, deleted: false,
            metadata: serde_json::json!({"kind": "directory", "modify_time": d.meta.modify_time, "xattrs": d.xattrs.iter().map(|x| serde_json::json!({"key":x.key,"value":x.value,"base64":x.base64})).collect::<Vec<_>>(), "node": {"readonly":d.meta.readonly,"creation_time":d.meta.creation_time,"change_time":d.meta.change_time,"modify_time":d.meta.modify_time,"access_time":d.meta.access_time,"backup_time":d.meta.backup_time}}),
        };
        if by_path.get(&rec.path).is_none_or(|old| old.deleted || old.version < rec.version) {
            by_path.insert(rec.path.clone(), rec);
        }
    });
    (by_path.into_values().collect(), bytes)
}

pub(crate) fn directory_metadata(
    value: &serde_json::Value,
) -> Result<crate::ltfs::volume::FileMetadata, ServiceError> {
    let parse = || -> Option<crate::ltfs::volume::FileMetadata> {
        let n = &value["node"];
        Some(crate::ltfs::volume::FileMetadata {
            meta: crate::ltfs::index::NodeMeta {
                readonly: n["readonly"].as_bool()?,
                creation_time: n["creation_time"].as_str()?.into(),
                change_time: n["change_time"].as_str()?.into(),
                modify_time: n["modify_time"].as_str()?.into(),
                access_time: n["access_time"].as_str()?.into(),
                backup_time: n["backup_time"].as_str()?.into(),
                file_uid: 0,
            },
            xattrs: value["xattrs"]
                .as_array()?
                .iter()
                .map(|x| {
                    Some(crate::ltfs::index::Xattr {
                        key: x["key"].as_str()?.into(),
                        value: x["value"].as_str()?.into(),
                        base64: x["base64"].as_bool()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        })
    };
    parse().ok_or_else(|| ServiceError::Io("目录属性损坏，拒绝有损搬迁".into()))
}

pub(crate) fn is_directory(metadata: &serde_json::Value) -> bool {
    metadata["kind"] == "directory"
}

/// Linux用户属性到LTFS属性名；内部元数据和虚拟属性不允许绕过服务修改。
pub fn writable_xattr_key(name: &str) -> Result<&str, ServiceError> {
    let key = name
        .strip_prefix("user.")
        .ok_or_else(|| ServiceError::ProtectedAttribute(name.into()))?;
    if key.is_empty()
        || key.starts_with("user.")
        || name.len() > 255
        || name
            .chars()
            .any(|c| c.is_control() || c == '\u{fffe}' || c == '\u{ffff}')
    {
        return Err(ServiceError::BadPath("扩展属性名非法".into()));
    }
    let reserved = key.trim_start_matches("user.");
    if ["tapers.", "tape.", "ltfs."]
        .iter()
        .any(|p| reserved.starts_with(p))
    {
        return Err(ServiceError::ProtectedAttribute(name.into()));
    }
    Ok(key)
}

/// 版本属性的文本形式是 `轮次.序号`。认不出来的按 (0, 0) 处理：它抢不走任何路径。
pub fn parse_version(s: &str) -> Option<(u64, u64)> {
    let (round, seq) = s.split_once('.')?;
    Some((round.parse().ok()?, seq.parse().ok()?))
}

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
    /// 只创建：路径已有已提交的当前版本。
    Exists(String),
    /// 删除：路径没有已提交的当前版本（从未有过，或已被删除）。
    NotFound(String),
    IsDirectory(String),
    NotDirectory(String),
    NotEmpty(String),
    CrossDevice(String),
    NoAttribute(String),
    ProtectedAttribute(String),
    AttributeTooLarge,
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
            ServiceError::PathBusy(p) => write!(f, "路径或其父子路径正在变更: {}", p),
            ServiceError::Exists(p) => write!(f, "路径已存在: {}", p),
            ServiceError::NotFound(p) => write!(f, "路径不存在: {}", p),
            ServiceError::IsDirectory(p) => write!(f, "路径是目录: {}", p),
            ServiceError::CrossDevice(p) => write!(f, "此元数据操作要求对象位于当前写入卷: {}", p),
            ServiceError::NoAttribute(p) => write!(f, "扩展属性不存在: {}", p),
            ServiceError::ProtectedAttribute(p) => write!(f, "扩展属性不可修改: {}", p),
            ServiceError::AttributeTooLarge => write!(f, "扩展属性超过65536字节"),
            ServiceError::NotEmpty(p) => write!(f, "目录非空: {}", p),
            ServiceError::NotDirectory(p) => write!(f, "父路径不是目录: {}", p),
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
    /// 这是一次删除：没有内容，落带时写墓碑
    pub delete: bool,
    pub(crate) directory: bool,
    pub(crate) symlink_target: Option<String>,
    /// 同卷引用改名：(原路径，是否为整棵子树的根操作)。
    pub(crate) rename_from: Option<(String, bool)>,
    /// (索引属性名，设置值；None表示删除)，只修改原生节点。
    pub(crate) xattr_change: Option<(String, Option<crate::ltfs::index::Xattr>)>,
    pub(crate) metadata: Option<crate::ltfs::volume::FileMetadata>,
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
    /// 墓碑：路径在这里被删除（`barcode` 是墓碑所在的带）。`stat` 从不返回这种记录，
    /// 只有 `lookup` 会。
    pub deleted: bool,
    pub version: (u64, u64),
    pub metadata: serde_json::Value,
}

/// 路径上未提交的变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlight {
    /// 已准入，内容还在接收
    Uploading,
    /// 已完整暂存，等待合批落带
    Staged,
    /// 删除已受理，等待墓碑落带
    Deleting,
}

impl InFlight {
    pub fn as_str(self) -> &'static str {
        match self {
            InFlight::Uploading => "uploading",
            InFlight::Staged => "staged",
            InFlight::Deleting => "deleting",
        }
    }
}

/// 一个路径的全貌：已提交的当前版本，加上可能在途的新版本（契约 file-contract.md "文件状态"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathState {
    pub committed: Option<Stat>,
    /// 在途变更及其已接收的长度
    pub in_flight: Option<(InFlight, u64)>,
}

/// 目录里的一个直接子项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirEntry {
    /// `committed` 是已提交版本的长度；`in_flight` 同 `PathState`
    File { committed: Option<u64>, in_flight: Option<(InFlight, u64)> },
    Dir,
}

struct Serving {
    draining: bool,
    round: u64,
    state: Arc<VolumeState>,
    queue: VecDeque<Upload>,
    queue_bytes: u64,
    /// 队列里最早一个文件的入队时刻、最近一次入队的时刻
    oldest: Option<Instant>,
    newest: Option<Instant>,
    in_batch: Vec<u64>,
    namespace_pending: BTreeSet<String>,
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
    /// 只读连接，以及打开时该文件的 (设备号, inode)：文件被换掉就重开
    reader: Mutex<Option<(Directory, Option<(u64, u64)>)>>,
}

fn norm(path: &str) -> Result<String, ServiceError> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "." || *p == ".." || p.contains('\0')) || path.ends_with('/') {
        return Err(ServiceError::BadPath(path.to_string()));
    }
    // 墓碑目录（`ltfs::volume::TOMBSTONE_DIR`）所在的保留目录
    if parts[0] == ".tapers" {
        return Err(ServiceError::BadPath(path.to_string()));
    }
    Ok(format!("/{}", parts.join("/")))
}

/// 必须按路径分量比较：/a 与 /ab 不冲突，/a 与 /a/b 冲突。
fn descendant(path: &str, parent: &str) -> bool {
    path.strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn check_pending(s: &Serving, path: &str) -> Result<(), ServiceError> {
    if s.state
        .load()
        .pending
        .keys()
        .any(|p| p == path || descendant(p, path) || descendant(path, p))
    {
        return Err(ServiceError::PathBusy(path.into()));
    }
    Ok(())
}

fn check_file_namespace(
    s: &Serving,
    path: &str,
    live: &BTreeMap<String, bool>,
) -> Result<(), ServiceError> {
    check_pending(s, path)?;
    if let Some((parent, _)) = live.iter().find(|(p, dir)| !**dir && descendant(path, p)) {
        return Err(ServiceError::NotDirectory(parent.clone()));
    }
    if live.get(path) == Some(&true) || live.keys().any(|p| descendant(p, path)) {
        return Err(ServiceError::IsDirectory(path.into()));
    }
    Ok(())
}

fn in_flight_of(e: &crate::core::volume_state::PendingEntry) -> InFlight {
    if e.removal {
        InFlight::Deleting
    } else if e.state == PendingState::Ready {
        InFlight::Staged
    } else {
        InFlight::Uploading
    }
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
    /// 下载暂存文件创建后立即 unlink，最后一个句柄关闭时由内核回收。
    pub fn read_spool(&self) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.spool_dir.join(format!("read-{}", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        std::fs::remove_file(path)?;
        Ok(file)
    }

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
    pub fn open(
        &self,
        round: u64,
        tape: TapeIdent,
        limits: TapeLimits,
        index: &LtfsIndex,
        free_bytes: u64,
        writable: bool,
    ) {
        let state = Arc::new(VolumeState::new(
            round,
            committed_view(index),
            index.generation,
            free_bytes,
            writable,
        ));
        let mut records = Vec::new();
        for r in catalog_of(index).0 {
            if self.directory_path.is_some() {
                match self.with_reader(|d| Some(d.lookup(&tape.pool_uuid, &r.path))) {
                    Some(Ok(Some(old))) if old.barcode != tape.barcode && old.version >= r.version => {
                        continue;
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        self.close("无法读取目录，不能安全开放命名空间");
                        return;
                    }
                }
            }
            records.push(r);
        }
        let mut g = self.lock();
        g.serving = Some(Serving {
            draining: false,
            round,
            state,
            queue: VecDeque::new(),
            queue_bytes: 0,
            oldest: None,
            newest: None,
            in_batch: Vec::new(),
            namespace_pending: BTreeSet::new(),
            opened: Instant::now(),
            limits,
            admitted: 0,
            switch: None,
            recent: {
                // 当前装着的这盘带以刚读到的索引为准，不必等目录经 Raft 对账。墓碑也记下：
                // 本带上删掉的路径不能再从目录库里查出旧副本
                records
                    .into_iter()
                    .map(|r| {
                        let st = Stat {
                            len: r.length,
                            generation: index.generation,
                            round,
                            barcode: tape.barcode.clone(),
                            sha256: r.sha256,
                            deleted: r.deleted,
                            version: r.version,
                            metadata: r.metadata,
                        };
                        (r.path, st)
                    })
                    .collect()
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

    /// 合批策略。回收按它的阈值切分搬迁的批次，与客户端上传共用同一个队列。
    pub fn policy(&self) -> BatchPolicy {
        self.policy
    }

    /// 队列里已完成、等着落带的 (文件数, 字节数)。
    pub fn queued(&self, round: u64) -> Option<(usize, u64)> {
        let g = self.lock();
        g.serving.as_ref().filter(|s| s.round == round).map(|s| (s.queue.len(), s.queue_bytes))
    }

    /// 目录里还指向这盘带的 (文件数, 字节数)。回收在格式化源带之前拿它做最后一道核对；
    /// 减去它就是这盘带上的可回收空间。
    pub fn live_on(&self, barcode: &str) -> Option<(u64, u64)> {
        self.with_reader(|d| d.live_on(barcode).ok())
    }

    /// 执行线程询问：当前这盘带是否需要换掉，换成什么状态。
    pub fn switch_requested(&self, round: u64) -> Option<&'static str> {
        self.lock().serving.as_ref().filter(|s| s.round == round).and_then(|s| s.switch)
    }

    /// 关闭新入队入口，保留已完成队列供计划停机落带。
    pub fn drain(&self) {
        if let Some(s) = self.lock().serving.as_mut() { s.draining = true; }
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
    /// `result` 成功时带索引代数，以及每个路径从已提交索引提取的目录记录（含哈希和元数据）。
    pub fn batch_done(
        &self,
        round: u64,
        batch: &FrozenBatch,
        uploads: &[Upload],
        result: Result<(u64, HashMap<String, FileRec>), String>,
    ) {
        let mut g = self.lock();
        let Some(s) = g.serving.as_mut().filter(|s| s.round == round) else {
            return;
        };
        s.in_batch.clear();
        for u in uploads {
            s.namespace_pending.remove(&u.path);
        }
        // 这些上传离开了流水线：成功的从此计入"带上已有文件"，不能再算在已准入里
        s.admitted = s.admitted.saturating_sub(uploads.len() as u64);
        let outcome = match result {
            Ok((tape_generation, hashes)) => {
                match s.state.publish_namespace(
                    batch,
                    S4Evidence {
                        batch_id: batch.batch_id,
                        generation: batch.generation,
                    },
                    &uploads
                        .iter()
                        .filter(|u| u.directory)
                        .map(|u| u.path.clone())
                        .collect(),
                ) {
                    Ok(_) => {
                        for u in uploads {
                            let st = Stat {
                                len: u.len,
                                generation: tape_generation,
                                round,
                                barcode: s.tape.barcode.clone(),
                                sha256: hashes
                                    .get(&u.path)
                                    .map(|r| r.sha256.clone())
                                    .unwrap_or_default(),
                                version: hashes.get(&u.path).map(|r| r.version).unwrap_or_default(),
                                metadata: hashes
                                    .get(&u.path)
                                    .map(|r| r.metadata.clone())
                                    .unwrap_or_default(),
                                deleted: u.delete,
                            };
                            s.recent.insert(u.path.clone(), st);
                        }
                        for r in hashes.values() {
                            s.recent.insert(
                                r.path.clone(),
                                Stat {
                                    len: r.length,
                                    generation: tape_generation,
                                    round,
                                    barcode: s.tape.barcode.clone(),
                                    sha256: r.sha256.clone(),
                                    version: r.version,
                                    metadata: r.metadata.clone(),
                                    deleted: r.deleted,
                                },
                            );
                        }
                        Ok(tape_generation)
                    }
                    Err(e) => Err(format!("已落带但发布失败: {:?}", e)),
                }
            }
            Err(e) => Err(e),
        };
        for u in uploads {
            if !u.delete {
                let _ = std::fs::remove_file(&u.spool);
            }
            let st = match &outcome {
                Ok(generation) => TaskStatus::Committed {
                    generation: *generation,
                },
                Err(e) => TaskStatus::Indeterminate { reason: e.clone() },
            };
            g.tasks.insert(u.task, st);
        }
        self.changed.notify_all();
    }

    // ---------- 客户端一侧 ----------

    /// 数据库查询不持有服务锁；准入时核对同一个卷实例，并用 recent 覆盖查询期间的发布。
    fn namespace_snapshot(
        &self,
        path: &str,
    ) -> Result<(Arc<VolumeState>, BTreeMap<String, bool>), ServiceError> {
        let (state, pool) = {
            let g = self.lock();
            let s = g.serving.as_ref().ok_or_else(|| {
                if g.no_tape {
                    ServiceError::NoTape(g.why_not.clone())
                } else {
                    ServiceError::NotServing(g.why_not.clone())
                }
            })?;
            (Arc::clone(&s.state), s.tape.pool_uuid.clone())
        };
        let live = if self.directory_path.is_some() {
            self.with_reader(|d| Some(d.namespace_nodes(&pool, path)))
                .ok_or_else(|| ServiceError::Io("无法读取目录，不能判定路径冲突".into()))?
                .map_err(|e| ServiceError::Io(format!("查询路径冲突失败: {}", e)))?
        } else {
            BTreeMap::new()
        };
        Ok((state, live))
    }

    fn admission_view(
        s: &Serving,
        state: &Arc<VolumeState>,
        path: &str,
        mut live: BTreeMap<String, bool>,
    ) -> Result<BTreeMap<String, bool>, ServiceError> {
        if !Arc::ptr_eq(&s.state, state) {
            return Err(ServiceError::NotServing(
                "查询期间服务卷已切换，请重试".into(),
            ));
        }
        for (p, st) in &s.recent {
            if p != path && !descendant(p, path) && !descendant(path, p) {
                continue;
            }
            if st.deleted {
                live.remove(p);
            } else {
                live.insert(p.clone(), is_directory(&st.metadata));
            }
        }
        Ok(live)
    }

    /// 准入：登记路径并预留空间。之后调用方把内容写进 `handle.spool`，边写边 `ingest`。
    pub fn begin(&self, path: &str, len: u64) -> Result<UploadHandle, ServiceError> {
        self.begin_with(path, len, false)
    }

    /// `create_only`：路径已有已提交的当前版本就拒绝（`Exists`）。在途的由路径预留拒绝（`PathBusy`）。
    pub fn begin_with(
        &self,
        path: &str,
        len: u64,
        create_only: bool,
    ) -> Result<UploadHandle, ServiceError> {
        let path = norm(path)?;
        let (state, live) = self.namespace_snapshot(&path)?;
        let mut g = self.lock();
        let task = g.next_task;
        let why = g.why_not.clone();
        if g.serving.is_none() && g.no_tape {
            return Err(ServiceError::NoTape(why));
        }
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining {
            return Err(ServiceError::NotServing("正在停机".into()));
        }
        let live = Self::admission_view(s, &state, &path, live)?;
        if create_only && live.contains_key(&path) {
            return Err(ServiceError::Exists(path));
        }
        check_file_namespace(s, &path, &live)?;
        if len > s.limits.usable_capacity {
            return Err(ServiceError::TooLarge {
                requested: len,
                tape_capacity: s.limits.usable_capacity,
            });
        }
        if let Some(state) = s.switch {
            return Err(ServiceError::SwitchingTape(format!(
                "{} 已 {}",
                s.tape.barcode, state
            )));
        }
        if s.recent
            .values()
            .filter(|st| !is_directory(&st.metadata))
            .count() as u64
            + s.admitted
            >= s.limits.file_limit
        {
            s.switch = Some(super::state::tape_state::DATA_FULL);
            let msg = format!(
                "{} 的文件数已到上限 {}",
                s.tape.barcode, s.limits.file_limit
            );
            drop(g);
            self.kick();
            return Err(ServiceError::SwitchingTape(msg));
        }
        let now = s.opened.elapsed().as_secs();
        s.state.open_session(task, u64::MAX);
        match s.state.admit(task, &path, len, now) {
            Ok(_) => {}
            Err(StateError::InsufficientCapacity {
                requested,
                available,
            }) => {
                // 这盘带放不下，但文件并不比单盘大：换下一盘，让调用方稍后重试
                s.switch = Some(super::state::tape_state::FULL);
                let msg = format!(
                    "{} 剩余 {} 字节，放不下 {} 字节",
                    s.tape.barcode, available, requested
                );
                drop(g);
                self.kick();
                return Err(ServiceError::SwitchingTape(msg));
            }
            Err(e) => return Err(map_state(e)),
        }
        s.admitted += 1;
        let round = s.round;
        g.next_task += 1;
        Ok(UploadHandle {
            task,
            spool: self.spool_dir.join(format!("task-{}.part", task)),
            path,
            round,
        })
    }

    pub fn ingest(&self, h: &UploadHandle, bytes: u64) -> Result<(), ServiceError> {
        let g = self.lock();
        let s = g.serving.as_ref().filter(|s| s.round == h.round).ok_or(ServiceError::NotServing(g.why_not.clone()))?;
        s.state.ingest(&h.path, bytes).map(|_| ()).map_err(map_state)
    }

    /// 内容已完整暂存：标记完成并入队，唤醒执行线程。
    pub fn finish(&self, h: UploadHandle, len: u64) -> Result<u64, ServiceError> {
        // 路径已经准入并持有预留，其他上传/删除不能在这里替换它。
        // 从目录的当前版本取属性，不能只查当前写带（覆盖可能换到另一盘带）。
        let metadata = (|| {
            let Some(current) = self.stat(&h.path)? else {
                return Ok(None);
            };
            let attrs = current.metadata["xattrs"]
                .as_array()
                .ok_or_else(|| ServiceError::Io("当前文件缺少扩展属性记录，拒绝有损覆盖".into()))?;
            let mut xattrs = Vec::with_capacity(attrs.len());
            for attr in attrs {
                let malformed = || ServiceError::Io("当前文件扩展属性记录损坏，拒绝覆盖".into());
                xattrs.push(crate::ltfs::index::Xattr {
                    key: attr["key"].as_str().ok_or_else(malformed)?.to_string(),
                    value: attr["value"].as_str().ok_or_else(malformed)?.to_string(),
                    base64: attr["base64"].as_bool().ok_or_else(malformed)?,
                });
            }
            Ok(Some(crate::ltfs::volume::FileMetadata::replacement(xattrs)))
        })();
        match metadata {
            Ok(metadata) => self.finish_with_metadata(h, len, metadata),
            Err(e) => {
                self.abort(h);
                Err(e)
            }
        }
    }

    pub(crate) fn finish_with_metadata(&self, h: UploadHandle, len: u64, metadata: Option<crate::ltfs::volume::FileMetadata>) -> Result<u64, ServiceError> {
        {
            let mut g = self.lock();
            let why = g.why_not.clone();
            let s = g.serving.as_mut().filter(|s| s.round == h.round).ok_or(ServiceError::NotServing(why))?;
            if s.draining {
                let _ = s.state.cancel(h.task);
                s.admitted = s.admitted.saturating_sub(1);
                let _ = std::fs::remove_file(&h.spool);
                return Err(ServiceError::NotServing("正在停机，请重新上传".into()));
            }
            s.state.complete(&h.path).map_err(map_state)?;
            s.queue.push_back(Upload { task: h.task, path: h.path.clone(), spool: h.spool.clone(), len, delete: false, symlink_target: None, rename_from: None, xattr_change: None, directory: false, metadata });
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
        // 目录库文件可能被整份换掉（收到 Raft 快照后从 Leader 拉回来）。缓存的连接还捏着
        // 旧的 inode，会一直读到旧内容，所以每次查询前看一眼文件还是不是同一个。
        let id = std::fs::metadata(path).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino())
        });
        let mut g = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if g.as_ref().is_none_or(|(_, seen)| *seen != id) {
            *g = Directory::open_reader(path).ok().map(|d| (d, id));
        }
        f(&g.as_ref()?.0)
    }

    /// 已提交的文件。先看本轮刚落带的，再看目录库（池内全部磁带、之前各轮提交的）。
    pub fn stat(&self, path: &str) -> Result<Option<Stat>, ServiceError> {
        let path = norm(path)?;
        let (round, pool) = {
            let g = self.lock();
            match (&g.serving, &g.read_only) {
                (Some(s), _) => {
                    // 本轮删掉的路径：目录库里那一行可能还没经 Raft 换成墓碑，不能再往下查
                    if let Some(st) = s.recent.get(&path) {
                        return Ok((!st.deleted).then(|| st.clone()));
                    }
                    (s.round, s.tape.pool_uuid.clone())
                }
                (None, Some((round, pool))) => (*round, pool.clone()),
                (None, None) => return Err(ServiceError::NotServing(g.why_not.clone())),
            }
        };
        let current = if self.directory_path.is_some() {
            self.with_reader(|d| Some(d.stat(&pool, &path)))
                .ok_or_else(|| ServiceError::Io("无法读取目录，不能判定文件是否存在".into()))?
                .map_err(|e| ServiceError::Io(format!("查询文件目录失败: {}", e)))?
        } else {
            None
        };
        Ok(current.map(|f| Stat {
            len: f.length,
            generation: f.generation,
            round,
            barcode: f.barcode,
            sha256: f.sha256,
            version: f.version,
            metadata: f.metadata,
            deleted: false,
        }))
    }

    /// 路径在已提交视图里的那条记录，墓碑也返回（`deleted`）。回收用它判断源带上的一份
    /// 是否已被别处的新版本取代——被删除也算取代。
    pub fn lookup(&self, path: &str) -> Result<Option<Stat>, ServiceError> {
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
        let row = if self.directory_path.is_some() {
            self.with_reader(|d| Some(d.lookup(&pool, &path)))
                .ok_or_else(|| ServiceError::Io("无法读取命名空间目录".into()))?
                .map_err(|e| ServiceError::Io(e.to_string()))?
        } else {
            None
        };
        Ok(row.map(|f| Stat {
            len: f.length,
            generation: f.generation,
            round,
            barcode: f.barcode,
            sha256: f.sha256,
            version: f.version,
            metadata: f.metadata,
            deleted: f.deleted,
        }))
    }

    /// 删除一个已提交的路径（文件契约"删除（墓碑）"）。受理即返回任务号，与上传一样
    /// `Staged` 之后随下一批落带变成 `Committed`。路径没有当前版本 → `NotFound`；
    /// 路径上有在途变更 → `PathBusy`。
    pub fn delete(&self, path: &str) -> Result<u64, ServiceError> {
        let path = norm(path)?;
        self.enqueue_removal(path, true, false)
    }

    /// 回收搬迁墓碑：源带上的墓碑要在别的带上重新落一次（新版本），源带才能格式化。
    /// 不检查路径是否存在——它本来就不存在。
    pub fn relocate_tombstone(&self, path: &str) -> Result<u64, ServiceError> {
        let path = norm(path)?;
        self.enqueue_removal(path, false, false)
    }

    pub(crate) fn relocate_tombstone_kind(
        &self,
        path: &str,
        directory: bool,
    ) -> Result<u64, ServiceError> {
        self.enqueue_removal(norm(path)?, false, directory)
    }

    fn enqueue_removal(
        &self,
        path: String,
        must_exist: bool,
        directory: bool,
    ) -> Result<u64, ServiceError> {
        let (state, live) = self.namespace_snapshot(&path)?;
        {
            let mut g = self.lock();
            let task = g.next_task;
            let why = g.why_not.clone();
            if g.serving.is_none() && g.no_tape {
                return Err(ServiceError::NoTape(why));
            }
            let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
            if s.draining {
                return Err(ServiceError::NotServing("正在停机".into()));
            }
            let live = Self::admission_view(s, &state, &path, live)?;
            if must_exist {
                check_file_namespace(s, &path, &live)?;
            } else {
                check_pending(s, &path)?;
                if s.recent.contains_key(&path) {
                    return Err(ServiceError::Exists(path));
                }
            }
            if must_exist && !live.contains_key(&path) {
                return Err(ServiceError::NotFound(path));
            }
            if let Some(state) = s.switch {
                return Err(ServiceError::SwitchingTape(format!(
                    "{} 已 {}",
                    s.tape.barcode, state
                )));
            }
            // 墓碑也是索引里的一条
            if s.recent
                .values()
                .filter(|st| !is_directory(&st.metadata))
                .count() as u64
                + s.admitted
                >= s.limits.file_limit
            {
                s.switch = Some(super::state::tape_state::DATA_FULL);
                let msg = format!(
                    "{} 的文件数已到上限 {}",
                    s.tape.barcode, s.limits.file_limit
                );
                drop(g);
                self.kick();
                return Err(ServiceError::SwitchingTape(msg));
            }
            let now = s.opened.elapsed().as_secs();
            s.state.open_session(task, u64::MAX);
            s.state.admit_removal(task, &path, now).map_err(map_state)?;
            s.state.complete(&path).map_err(map_state)?;
            s.admitted += 1;
            if directory {
                s.namespace_pending.insert(path.clone());
            }
            s.queue.push_back(Upload {
                task,
                path,
                spool: PathBuf::new(),
                len: 0,
                delete: true,
                directory,
                symlink_target: None, rename_from: None, xattr_change: None,
                metadata: None,
            });
            let now = Instant::now();
            s.oldest.get_or_insert(now);
            s.newest = Some(now);
            g.tasks.insert(task, TaskStatus::Staged);
            g.next_task += 1;
            drop(g);
            self.kick();
            Ok(task)
        }
    }

    /// 用户属性的原生元数据提交；不复制文件内容，不借用其他带的extent。
    pub fn change_xattr(
        &self,
        path: &str,
        name: &str,
        value: Option<&[u8]>,
        flags: u32,
    ) -> Result<u64, ServiceError> {
        use base64::Engine;
        let key = writable_xattr_key(name)?;
        if flags > 2 || (value.is_none() && flags != 0) {
            return Err(ServiceError::BadPath("扩展属性flags非法".into()));
        }
        if value.is_some_and(|v| v.len() > 65536) {
            return Err(ServiceError::AttributeTooLarge);
        }
        let path = norm(path)?;
        let (state, live) = self.namespace_snapshot(&path)?;
        let mut g = self.lock();
        let task = g.next_task;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining {
            return Err(ServiceError::NotServing("正在停机".into()));
        }
        let live = Self::admission_view(s, &state, &path, live)?;
        check_pending(s, &path)?;
        let directory = *live
            .get(&path)
            .ok_or_else(|| ServiceError::NotFound(path.clone()))?;
        let st = s
            .recent
            .get(&path)
            .filter(|st| !st.deleted && st.barcode == s.tape.barcode)
            .ok_or_else(|| ServiceError::CrossDevice(path.clone()))?;
        let attrs = st.metadata["xattrs"]
            .as_array()
            .ok_or_else(|| ServiceError::Io("属性目录缺失".into()))?;
        // LE索引通常不带user.前缀；兼容已有带前缀记录，避免生成同名别名。
        let existing = attrs.iter().find(|x| x["key"] == key || x["key"] == name);
        if existing.is_some() && flags == 1 {
            return Err(ServiceError::Exists(name.into()));
        }
        if existing.is_none() && (flags == 2 || value.is_none()) {
            return Err(ServiceError::NoAttribute(name.into()));
        }
        let key = existing
            .and_then(|x| x["key"].as_str())
            .unwrap_or(key)
            .to_string();
        let len = st.len;
        if let Some(reason) = s.switch {
            return Err(ServiceError::SwitchingTape(reason.into()));
        }
        s.state.open_session(task, u64::MAX);
        let admitted = s
            .state
            .admit(task, &path, 0, s.opened.elapsed().as_secs())
            .and_then(|_| s.state.stage_reference(&path, len))
            .and_then(|_| s.state.complete(&path));
        if let Err(e) = admitted {
            let _ = s.state.cancel(task);
            return Err(map_state(e));
        }
        s.namespace_pending.insert(path.clone());
        s.admitted += 1;
        let attr = value.map(|v| crate::ltfs::index::Xattr {
            key: key.clone(),
            value: base64::engine::general_purpose::STANDARD.encode(v),
            base64: true,
        });
        s.queue.push_back(Upload {
            task,
            path,
            spool: PathBuf::new(),
            len,
            delete: false,
            directory,
            symlink_target: None, rename_from: None,
            xattr_change: Some((key, attr)),
            metadata: None,
        });
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        drop(g);
        self.kick();
        Ok(task)
    }

    pub fn mkdir(&self, path: &str) -> Result<u64, ServiceError> {
        self.node_change(path, false, None, None)
    }

    pub fn rmdir(&self, path: &str) -> Result<u64, ServiceError> {
        self.node_change(path, true, None, None)
    }

    pub(crate) fn relocate_directory(
        &self,
        path: &str,
        metadata: crate::ltfs::volume::FileMetadata,
    ) -> Result<u64, ServiceError> {
        self.node_change(path, false, Some(metadata), None)
    }

    /// 创建原生符号链接；目标不解析，允许悬空和绝对路径。
    pub fn symlink(&self, target: &str, path: &str) -> Result<u64, ServiceError> {
        if target.is_empty()
            || !target.chars().all(|c| {
                matches!(c,
            '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' |
            '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
            })
        {
            return Err(ServiceError::BadPath(
                "符号链接目标为空或包含 XML 非法字符".into(),
            ));
        }
        self.node_change(path, false, None, Some(target.to_string()))
    }

    pub(crate) fn relocate_symlink(
        &self,
        path: &str,
        target: &str,
        metadata: crate::ltfs::volume::FileMetadata,
    ) -> Result<u64, ServiceError> {
        self.node_change(path, false, Some(metadata), Some(target.into()))
    }

    fn node_change(
        &self,
        path: &str,
        remove: bool,
        metadata: Option<crate::ltfs::volume::FileMetadata>,
        symlink_target: Option<String>,
    ) -> Result<u64, ServiceError> {
        let path = norm(path)?;
        let (state, live) = self.namespace_snapshot(&path)?;
        let mut g = self.lock();
        let task = g.next_task;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining {
            return Err(ServiceError::NotServing("正在停机".into()));
        }
        let live = Self::admission_view(s, &state, &path, live)?;
        check_pending(s, &path)?;
        // 回收源带不再可写；当前写带刚发布的同名记录已取代这份源副本。
        if metadata.is_some() && s.recent.contains_key(&path) {
            return Err(ServiceError::Exists(path));
        }
        if let Some((p, _)) = live.iter().find(|(p, dir)| !**dir && descendant(&path, p)) {
            return Err(ServiceError::NotDirectory(p.clone()));
        }
        let children = live.keys().any(|p| descendant(p, &path));
        if remove {
            if live.get(&path) == Some(&false) {
                return Err(ServiceError::NotDirectory(path));
            }
            if children {
                return Err(ServiceError::NotEmpty(path));
            }
            if live.get(&path) != Some(&true) {
                return Err(ServiceError::NotFound(path));
            }
        } else if metadata.is_none() {
            if live.contains_key(&path) || children {
                return Err(ServiceError::Exists(path));
            }
            let parent = path.rsplit_once('/').unwrap().0;
            if !parent.is_empty() && live.get(parent) != Some(&true) {
                return Err(ServiceError::NotFound(parent.into()));
            }
        } else if symlink_target.is_some() {
            if live.get(&path) == Some(&true) || children {
                return Err(ServiceError::IsDirectory(path));
            }
        } else if live.get(&path) == Some(&false) {
            return Err(ServiceError::NotDirectory(path));
        }
        if let Some(state) = s.switch {
            return Err(ServiceError::SwitchingTape(state.into()));
        }
        if symlink_target.is_some()
            && s.recent
                .values()
                .filter(|st| !is_directory(&st.metadata))
                .count() as u64
                + s.admitted
                >= s.limits.file_limit
        {
            s.switch = Some(super::state::tape_state::DATA_FULL);
            let message = format!(
                "{} 的文件数已到上限 {}",
                s.tape.barcode, s.limits.file_limit
            );
            drop(g);
            self.kick();
            return Err(ServiceError::SwitchingTape(message));
        }
        let now = s.opened.elapsed().as_secs();
        s.state.open_session(task, u64::MAX);
        if remove {
            s.state.admit_removal(task, &path, now).map_err(map_state)?;
        } else {
            s.state.admit(task, &path, 0, now).map_err(map_state)?;
        }
        s.state.complete(&path).map_err(map_state)?;
        s.admitted += 1;
        s.namespace_pending.insert(path.clone());
        s.queue.push_back(Upload {
            task,
            path,
            spool: PathBuf::new(),
            len: 0,
            delete: remove,
            directory: symlink_target.is_none(),
            symlink_target,
            rename_from: None,
            xattr_change: None,
            metadata,
        });
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        drop(g);
        self.kick();
        Ok(task)
    }

    /// 同写入卷原子改名。所有源/目标条目在同一锁内准入并进入同一批索引。
    pub fn rename(&self, from: &str, to: &str, no_replace: bool) -> Result<u64, ServiceError> {
        let from = norm(from)?;
        let to = norm(to)?;
        let (state, source) = self.namespace_snapshot(&from)?;
        let (target_state, target) = self.namespace_snapshot(&to)?;
        let mut g = self.lock();
        let task = g.next_task;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining {
            return Err(ServiceError::NotServing("正在停机".into()));
        }
        let source = Self::admission_view(s, &state, &from, source)?;
        let target = Self::admission_view(s, &target_state, &to, target)?;
        check_pending(s, &from)?;
        check_pending(s, &to)?;
        let directory = *source
            .get(&from)
            .ok_or_else(|| ServiceError::NotFound(from.clone()))?;
        if from == to {
            if no_replace {
                return Err(ServiceError::Exists(to));
            }
            let generation = s.state.load().generation;
            g.tasks.insert(task, TaskStatus::Committed { generation });
            g.next_task += 1;
            return Ok(task);
        }
        if descendant(&to, &from) || descendant(&from, &to) {
            return Err(ServiceError::BadPath("源和目标不能互为祖先".into()));
        }
        if let Some((p, _)) = target.iter().find(|(p, d)| !**d && descendant(&to, p)) {
            return Err(ServiceError::NotDirectory(p.clone()));
        }
        let parent = to.rsplit_once('/').unwrap().0;
        if !parent.is_empty() && target.get(parent) != Some(&true) {
            return Err(ServiceError::NotFound(parent.into()));
        }
        if let Some(target_dir) = target.get(&to) {
            if no_replace {
                return Err(ServiceError::Exists(to));
            }
            if directory && !target_dir {
                return Err(ServiceError::NotDirectory(to));
            }
            if !directory && *target_dir {
                return Err(ServiceError::IsDirectory(to));
            }
            if target.keys().any(|p| descendant(p, &to)) {
                return Err(ServiceError::NotEmpty(to));
            }
            if s.recent
                .get(&to)
                .is_none_or(|st| st.deleted || st.barcode != s.tape.barcode)
            {
                return Err(ServiceError::CrossDevice(to));
            }
        }
        if let Some(reason) = s.switch {
            return Err(ServiceError::SwitchingTape(reason.into()));
        }
        let mut uploads = Vec::new();
        for (path, dir) in source
            .iter()
            .filter(|(p, _)| **p == from || descendant(p, &from))
        {
            let st = s
                .recent
                .get(path)
                .filter(|st| !st.deleted && st.barcode == s.tape.barcode)
                .ok_or_else(|| ServiceError::CrossDevice(path.clone()))?;
            uploads.push(Upload {
                task,
                path: format!("{}{}", to, &path[from.len()..]),
                spool: PathBuf::new(),
                len: st.len,
                delete: false,
                directory: *dir,
                metadata: None,
                symlink_target: None, rename_from: Some((path.clone(), *path == from)), xattr_change: None,
            });
        }
        let removals: Vec<_> = uploads
            .iter()
            .rev()
            .map(|u| Upload {
                task,
                path: u.rename_from.as_ref().unwrap().0.clone(),
                spool: PathBuf::new(),
                len: 0,
                delete: true,
                directory: u.directory,
                metadata: None,
                symlink_target: None, rename_from: None, xattr_change: None,
            })
            .collect();
        uploads.extend(removals);
        let now = s.opened.elapsed().as_secs();
        s.state.open_session(task, u64::MAX);
        for u in &uploads {
            let admitted = if u.delete {
                s.state.admit_removal(task, &u.path, now)
            } else {
                s.state.admit(task, &u.path, 0, now)
            };
            if let Err(e) = admitted {
                let _ = s.state.cancel(task);
                return Err(map_state(e));
            }
        }
        // 尚未 complete 时取消可以整体撤销；长度只作视图发布，不消耗数据区预算。
        for u in &uploads {
            if !u.delete {
                s.state.stage_reference(&u.path, u.len).map_err(map_state)?;
            }
            s.state.complete(&u.path).map_err(map_state)?;
            s.namespace_pending.insert(u.path.clone());
        }
        s.admitted += uploads.len() as u64;
        s.queue.extend(uploads);
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        drop(g);
        self.kick();
        Ok(task)
    }

    pub fn list(&self) -> Result<BTreeMap<String, u64>, ServiceError> {
        Ok(self
            .list_nodes()?
            .into_iter()
            .filter(|(_, (_, d))| !d)
            .map(|(p, (n, _))| (p, n))
            .collect())
    }

    fn list_nodes(&self) -> Result<BTreeMap<String, (u64, bool)>, ServiceError> {
        let (pool, mut out, gone) = {
            let g = self.lock();
            match (&g.serving, &g.read_only) {
                (Some(s), _) => (
                    s.tape.pool_uuid.clone(),
                    s.recent
                        .iter()
                        .filter(|(_, st)| !st.deleted)
                        .map(|(p, st)| (p.clone(), (st.len, is_directory(&st.metadata))))
                        .collect::<BTreeMap<_, _>>(),
                    // 本轮删掉的路径盖住目录库里可能还没换成墓碑的旧行
                    s.recent
                        .iter()
                        .filter(|(_, st)| st.deleted)
                        .map(|(p, _)| p.clone())
                        .collect::<std::collections::HashSet<_>>(),
                ),
                (None, Some((_, pool))) => (pool.clone(), BTreeMap::new(), Default::default()),
                (None, None) => return Err(ServiceError::NotServing(g.why_not.clone())),
            }
        };
        let rows = if self.directory_path.is_some() {
            self.with_reader(|d| Some(d.list_nodes(&pool)))
                .ok_or_else(|| ServiceError::Io("无法读取命名空间目录".into()))?
                .map_err(|e| ServiceError::Io(e.to_string()))?
        } else {
            Vec::new()
        };
        for (p, n, d) in rows {
            if !gone.contains(&p) {
                out.entry(p).or_insert((n, d));
            }
        }
        Ok(out)
    }

    /// 路径的全貌：已提交版本（同 `stat`）加在途变更。
    pub fn stat_full(&self, path: &str) -> Result<PathState, ServiceError> {
        let path = norm(path)?;
        let committed = self.stat(&path)?;
        let in_flight = self.in_flight(&path);
        Ok(PathState { committed, in_flight })
    }

    fn in_flight(&self, path: &str) -> Option<(InFlight, u64)> {
        let g = self.lock();
        let s = g.serving.as_ref()?;
        if s.namespace_pending.contains(path) {
            return None;
        }
        let root = s.state.load();
        root.pending
            .get(path)
            .map(|e| (in_flight_of(e), e.staged_len))
    }

    /// 一个目录的直接子项。目录记录使空目录可见；旧 catalog 的文件前缀仍可推导非空目录。
    /// `pending` 为真时附上只在途、尚未提交的路径。
    pub fn list_dir(
        &self,
        dir: &str,
        pending: bool,
    ) -> Result<Option<BTreeMap<String, DirEntry>>, ServiceError> {
        let prefix = if dir.trim_matches('/').is_empty() {
            "/".to_string()
        } else {
            format!("{}/", norm(dir)?)
        };
        let mut out: BTreeMap<String, DirEntry> = BTreeMap::new();
        fn add(
            out: &mut BTreeMap<String, DirEntry>,
            prefix: &str,
            path: &str,
            committed: Option<u64>,
            flight: Option<(InFlight, u64)>,
        ) {
            let Some(rest) = path.strip_prefix(prefix) else {
                return;
            };
            if rest.is_empty() {
                return;
            }
            match rest.split_once('/') {
                Some((sub, _)) => {
                    out.entry(sub.to_string()).or_insert(DirEntry::Dir);
                }
                None => match out.entry(rest.to_string()).or_insert(DirEntry::File {
                    committed: None,
                    in_flight: None,
                }) {
                    DirEntry::File {
                        committed: c,
                        in_flight: f,
                    } => {
                        *c = c.or(committed);
                        *f = f.or(flight);
                    }
                    // 同名的目录与文件：LTFS 里不会出现，目录优先
                    DirEntry::Dir => {}
                },
            }
        }
        // TODO（目录库）：按前缀查询，而不是取全量再过滤
        let nodes = self.list_nodes()?;
        let own = nodes.get(prefix.trim_end_matches('/'));
        if own.is_some_and(|(_, d)| !d) {
            return Err(ServiceError::NotDirectory(dir.into()));
        }
        let exists = prefix == "/" || own.is_some_and(|(_, d)| *d);
        for (p, (len, directory)) in &nodes {
            if *directory {
                add(&mut out, &prefix, &format!("{}/", p), None, None);
            } else {
                add(&mut out, &prefix, p, Some(*len), None);
            }
        }
        let flights: Vec<(String, (InFlight, u64))> = {
            let g = self.lock();
            match g.serving.as_ref() {
                Some(s) => s
                    .state
                    .load()
                    .pending
                    .iter()
                    .filter(|(p, _)| !s.namespace_pending.contains(*p))
                    .map(|(p, e)| (p.clone(), (in_flight_of(e), e.staged_len)))
                    .collect(),
                None => Vec::new(),
            }
        };
        for (p, f) in flights {
            if pending {
                add(&mut out, &prefix, &p, None, Some(f));
            } else if let Some(DirEntry::File { in_flight, .. }) =
                p.strip_prefix(&prefix).and_then(|n| out.get_mut(n))
            {
                // 已提交的文件上正有新版本在途：照样标出来
                *in_flight = Some(f);
            }
        }
        if out.is_empty() && !exists {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// 当前服务的磁带。
    pub fn tape(&self) -> Option<TapeIdent> {
        self.lock().serving.as_ref().map(|s| s.tape.clone())
    }

}
