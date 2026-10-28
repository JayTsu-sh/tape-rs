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
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::core::volume_state::{
    DirNode, FileVersion, FrozenBatch, PendingState, S4Evidence, StateError, VolumeState,
};
use crate::ltfs::index::{DirectoryNode, LtfsIndex};
use crate::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION, is_tombstone_path};
use sha2::{Digest, Sha256};

use super::directory::Directory;
use super::executor::ExecRequest;
use super::state::FileRec;

/// 运行节点签发的冻结目录快照；文件描述符已脱离路径，HTTP 可以安全地流式发送。
pub struct CatalogSnapshotExport {
    pub file: std::fs::File,
    pub applied_index: u64,
    pub frozen_index: u64,
    pub sha256: [u8; 32],
    pub(super) lease: CatalogExportLease,
}

pub(super) struct CatalogExportLease(Arc<std::sync::atomic::AtomicBool>);

impl Drop for CatalogExportLease {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

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
                "node": {"readonly":f.meta.readonly,"creation_time":f.meta.creation_time,"change_time":f.meta.change_time,"modify_time":f.meta.modify_time,"access_time":f.meta.access_time,"backup_time":f.meta.backup_time},
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

/// 跨库复制只接受普通文件的用户属性；卷内版本、摘要及设备私有属性由目标重建。
pub(crate) fn copied_file_metadata(
    value: &serde_json::Value,
) -> Result<crate::ltfs::volume::FileMetadata, ServiceError> {
    if value.get("kind").is_some_and(|kind| kind != "file") {
        return Err(ServiceError::Io("复制源不是普通文件".into()));
    }
    let mut metadata = directory_metadata(value)?;
    for time in [
        &metadata.meta.creation_time,
        &metadata.meta.change_time,
        &metadata.meta.modify_time,
        &metadata.meta.access_time,
        &metadata.meta.backup_time,
    ] {
        crate::ltfs::index::MetadataChange {
            modify_time: Some(time.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|e| ServiceError::Io(format!("复制源时间属性无效：{e}")))?;
    }
    metadata.xattrs.retain(|attr| {
        !["tapers.", "tape.", "ltfs."]
            .iter()
            .any(|prefix| attr.key.starts_with(prefix))
    });
    let mut keys = std::collections::BTreeSet::new();
    for attr in &metadata.xattrs {
        writable_xattr_key(&format!("user.{}", attr.key))?;
        if !keys.insert(&attr.key) {
            return Err(ServiceError::Io("复制源扩展属性重复".into()));
        }
        if attr.base64 {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&attr.value)
                .map_err(|e| ServiceError::Io(format!("复制源扩展属性 Base64 无效：{e}")))?;
        }
    }
    Ok(metadata)
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
        Self {
            max_bytes: 8 << 30,
            max_files: 20_000,
            idle: Duration::from_secs(2),
            max_wait: Duration::from_secs(60),
        }
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
    /// 条件删除：当前文件不是调用方核对过的那个版本。
    VersionMismatch(String),
    IsDirectory(String),
    NotDirectory(String),
    NotEmpty(String),
    CrossDevice(String),
    NoAttribute(String),
    ProtectedAttribute(String),
    AttributeTooLarge,
    InsufficientCapacity {
        requested: u64,
        available: u64,
    },
    NotWritable(String),
    BadPath(String),
    Io(String),
    /// 当前磁带不能再接收这个文件，正在换下一盘。稍后重试即可。
    SwitchingTape(String),
    /// 池里没有可写的磁带了。确定的失败，重试无益，需要运维加带。
    NoTape(String),
    /// 文件比单盘磁带还大。确定的失败。
    TooLarge {
        requested: u64,
        tape_capacity: u64,
    },
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::NotServing(s) => write!(f, "本节点不在服务: {}", s),
            ServiceError::PathBusy(p) => write!(f, "路径或其父子路径正在变更: {}", p),
            ServiceError::Exists(p) => write!(f, "路径已存在: {}", p),
            ServiceError::NotFound(p) => write!(f, "路径不存在: {}", p),
            ServiceError::VersionMismatch(p) => write!(f, "路径版本已改变: {}", p),
            ServiceError::IsDirectory(p) => write!(f, "路径是目录: {}", p),
            ServiceError::CrossDevice(p) => write!(f, "此元数据操作要求对象位于当前写入卷: {}", p),
            ServiceError::NoAttribute(p) => write!(f, "扩展属性不存在: {}", p),
            ServiceError::ProtectedAttribute(p) => write!(f, "扩展属性不可修改: {}", p),
            ServiceError::AttributeTooLarge => write!(f, "扩展属性超过65536字节"),
            ServiceError::NotEmpty(p) => write!(f, "目录非空: {}", p),
            ServiceError::NotDirectory(p) => write!(f, "父路径不是目录: {}", p),
            ServiceError::InsufficientCapacity {
                requested,
                available,
            } => {
                write!(f, "空间不足: 需要 {} 可用 {}", requested, available)
            }
            ServiceError::NotWritable(s) => write!(f, "卷只读: {}", s),
            ServiceError::BadPath(p) => write!(f, "非法路径: {}", p),
            ServiceError::Io(e) => write!(f, "暂存区 I/O: {}", e),
            ServiceError::SwitchingTape(s) => write!(f, "正在换带: {}", s),
            ServiceError::NoTape(s) => write!(f, "池里没有可写的磁带: {}", s),
            ServiceError::TooLarge {
                requested,
                tape_capacity,
            } => {
                write!(
                    f,
                    "文件 {} 字节，超过单盘可用容量 {} 字节",
                    requested, tape_capacity
                )
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
    pub(crate) node_change: Option<crate::ltfs::index::MetadataChange>,
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

/// 目录列举所需的已提交类型与长度，不加载完整文件属性。
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeSummary {
    pub length: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
}

impl NodeSummary {
    pub(crate) fn new(length: u64, metadata: &serde_json::Value) -> Self {
        Self {
            length,
            is_dir: is_directory(metadata),
            is_symlink: metadata["kind"] == "symlink",
        }
    }
}

/// 目录里的一个直接子项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirEntry {
    /// `committed` 是已提交版本的长度；`in_flight` 同 `PathState`
    File {
        is_symlink: bool,
        committed: Option<u64>,
        in_flight: Option<(InFlight, u64)>,
    },
    Dir,
}

struct Serving {
    draining: bool,
    maintenance: bool,
    capacity_known: bool,
    round: u64,
    state: Arc<VolumeState>,
    queue: VecDeque<Upload>,
    queue_bytes: u64,
    /// 队列里最早一个文件的入队时刻、最近一次入队的时刻
    oldest: Option<Instant>,
    newest: Option<Instant>,
    in_batch: Vec<u64>,
    namespace_pending: BTreeSet<String>,
    external_pending: Vec<Arc<crate::core::volume_state::VolumeRoot>>,
    opened: Instant,
    tape: TapeIdent,
    limits: TapeLimits,
    /// 已准入的上传数（含尚未完成的），与带上已有文件数一起对照文件数上限
    admitted: u64,
    /// 当前物理卷索引条目数；其他卷覆盖同名路径不能减少这里的文件数。
    files_total: u64,
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

// 每个池只有一把准入锁和任务表。锁内切换当前驱动视图，避免跨驱动的路径准入竞态。
struct PoolState {
    current: Inner,
    lane: usize,
    others: BTreeMap<usize, Inner>,
    recent: HashMap<String, Stat>,
}

struct ServiceGuard<'a>(MutexGuard<'a, PoolState>);
impl std::ops::Deref for ServiceGuard<'_> {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0.current
    }
}
impl std::ops::DerefMut for ServiceGuard<'_> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.0.current
    }
}
impl Drop for ServiceGuard<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.0.current.serving.as_mut() {
            let recent = std::mem::take(&mut s.recent);
            self.0.recent = recent;
        }
    }
}

impl Inner {
    fn empty() -> Self {
        Self {
            serving: None,
            why_not: "尚未接管".into(),
            no_tape: false,
            read_only: None,
            tasks: HashMap::new(),
            next_task: 1,
        }
    }
}

// Cached read-only catalog connection and its device/inode identity.
type DirectoryReader = (Directory, Option<(u64, u64)>);

pub struct FileService {
    permission: Arc<Mutex<Option<Arc<super::permission::Permission>>>>,
    /// Raft 已应用的旧目录暂停屏障；所有池/驱动 lane 共用。
    catalog_quiesced: Arc<std::sync::atomic::AtomicBool>,
    inner: Arc<Mutex<PoolState>>,
    lane: usize,
    target_pool: Mutex<Option<String>>,
    requested: std::sync::atomic::AtomicBool,
    parallelism: std::sync::atomic::AtomicUsize,
    peers: Mutex<Vec<Arc<FileService>>>,
    changed: Arc<Condvar>,
    spool_dir: PathBuf,
    exec: Arc<Mutex<Option<Sender<ExecRequest>>>>,
    task_events: Arc<Mutex<Option<Sender<super::executor::ExecEvent>>>>,
    policy: BatchPolicy,
    directory_path: Option<PathBuf>,
    export_active: Arc<std::sync::atomic::AtomicBool>,
    /// 只读连接，以及打开时该文件的 (设备号, inode)：文件被换掉就重开
    reader: Mutex<Option<DirectoryReader>>,
}

fn norm(path: &str) -> Result<String, ServiceError> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|p| *p == "." || *p == ".." || p.contains('\0'))
        || path.ends_with('/')
    {
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
    if s.external_pending.iter().any(|root| {
        root.pending
            .keys()
            .any(|p| p == path || descendant(p, path) || descendant(path, p))
    }) || s
        .state
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
        StateError::InsufficientCapacity {
            requested,
            available,
        } => ServiceError::InsufficientCapacity {
            requested,
            available,
        },
        StateError::NotWritable(s) => ServiceError::NotWritable(s),
        other => ServiceError::NotServing(format!("{:?}", other)),
    }
}

fn task_id(round: u64, counter: u64) -> Result<u64, ServiceError> {
    if round >= (1 << 31) || counter == 0 || counter > u32::MAX as u64 {
        return Err(ServiceError::Io("文件任务号已耗尽，拒绝重复使用".into()));
    }
    Ok((round << 32) | counter)
}

/// 把卷索引转成卷状态模块的已提交视图。
pub fn committed_view(index: &LtfsIndex) -> Arc<DirNode> {
    fn conv(d: &DirectoryNode) -> DirNode {
        let mut n = DirNode::default();
        for f in &d.files {
            n.files.insert(
                f.name.clone(),
                Arc::new(FileVersion {
                    version: f.meta.file_uid,
                    len: f.length,
                    partial: false,
                }),
            );
        }
        for s in &d.subdirs {
            n.subdirs.insert(s.name.clone(), Arc::new(conv(s)));
        }
        n
    }
    Arc::new(conv(&index.root))
}

impl FileService {
    #[cfg(feature = "fdb")]
    pub(crate) fn spool_dir(&self) -> &std::path::Path {
        &self.spool_dir
    }

    #[cfg(feature = "fdb")]
    pub(crate) fn extent_cache_dir(&self) -> PathBuf {
        let mut name = self
            .spool_dir
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        name.push(".fdb-extents");
        self.spool_dir.with_file_name(name)
    }

    /// 下载暂存文件创建后立即 unlink，最后一个句柄关闭时由内核回收。
    pub fn read_spool(&self) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self
            .spool_dir
            .join(format!("read-{}", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        std::fs::remove_file(path)?;
        Ok(file)
    }

    pub fn new(spool_dir: PathBuf) -> std::io::Result<Arc<Self>> {
        Self::with_options(spool_dir, BatchPolicy::default(), None)
    }

    /// `directory_path` 是本节点目录库的文件；给了之后 `stat`/`list` 能回答本轮之前提交的文件。
    pub fn with_options(
        spool_dir: PathBuf,
        policy: BatchPolicy,
        directory_path: Option<PathBuf>,
    ) -> std::io::Result<Arc<Self>> {
        // 暂存区不跨进程保留：上一次运行留下的都是未落带的残留
        let _ = std::fs::remove_dir_all(&spool_dir);
        std::fs::create_dir_all(&spool_dir)?;
        Ok(Arc::new(Self {
            permission: Arc::new(Mutex::new(None)),
            catalog_quiesced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            inner: Arc::new(Mutex::new(PoolState {
                current: Inner::empty(),
                lane: 0,
                others: BTreeMap::new(),
                recent: HashMap::new(),
            })),
            lane: 0,
            target_pool: Mutex::new(None),
            requested: std::sync::atomic::AtomicBool::new(false),
            parallelism: std::sync::atomic::AtomicUsize::new(1),
            peers: Mutex::new(Vec::new()),
            changed: Arc::new(Condvar::new()),
            spool_dir,
            exec: Arc::new(Mutex::new(None)),
            task_events: Arc::new(Mutex::new(None)),
            policy,
            directory_path,
            export_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reader: Mutex::new(None),
        }))
    }

    pub fn set_executor(&self, tx: Sender<ExecRequest>) {
        *self.exec.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    pub fn set_task_events(&self, tx: Sender<super::executor::ExecEvent>) {
        *self.task_events.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    fn report_task(&self, task: u64, round: u64, pool: &str, path: &str, status: &TaskStatus) {
        let (name, generation, reason) = match status {
            TaskStatus::Staged => ("staged", None, String::new()),
            TaskStatus::Committed { generation } => ("committed", Some(*generation), String::new()),
            TaskStatus::Failed { reason } => ("failed", None, reason.clone()),
            TaskStatus::Indeterminate { reason } => ("indeterminate", None, reason.clone()),
        };
        if let Some(tx) = self
            .task_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(super::executor::ExecEvent::FileTask {
                task,
                round,
                pool: pool.to_owned(),
                path: path.to_owned(),
                status: name.into(),
                generation,
                reason: reason.chars().take(1024).collect(),
            });
        }
    }

    pub fn set_permission(&self, permission: Arc<super::permission::Permission>) {
        *self.permission.lock().unwrap_or_else(|e| e.into_inner()) = Some(permission);
    }

    pub fn set_catalog_quiesced(&self, quiesced: bool) {
        self.catalog_quiesced
            .store(quiesced, std::sync::atomic::Ordering::SeqCst);
    }

    fn require_catalog_writable(&self) -> Result<(), ServiceError> {
        if self
            .catalog_quiesced
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            Err(ServiceError::NotServing(
                "旧目录已暂停，等待切换或恢复".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// HTTP VIP 只跟随新 HA 协议的有效 owner；不要求池里已有可写磁带。
    pub(super) fn serving_deadline(&self, round: u64) -> Option<Instant> {
        self.permission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|p| p.deadline(round, super::permission::Phase::Serving))
    }

    fn check_permission(&self) -> Result<(), ServiceError> {
        let permission = self.permission.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = permission.as_ref() {
            let g = self.lock();
            let round = g
                .serving
                .as_ref()
                .map(|s| s.round)
                .or_else(|| g.read_only.as_ref().map(|(r, _)| *r));
            if !round.is_some_and(|r| p.allows(r, super::permission::Phase::Serving)) {
                return Err(ServiceError::NotServing(
                    "本轮尚未发布或多数派执行许可已失效".into(),
                ));
            }
        }
        Ok(())
    }

    fn lock(&self) -> ServiceGuard<'_> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.lane != self.lane {
            let mut next = g.others.remove(&self.lane).unwrap_or_else(Inner::empty);
            std::mem::swap(&mut next.tasks, &mut g.current.tasks);
            std::mem::swap(&mut next.next_task, &mut g.current.next_task);
            let old = std::mem::replace(&mut g.current, next);
            let lane = g.lane;
            g.others.insert(lane, old);
            g.lane = self.lane;
        }
        let external = g
            .others
            .values()
            .filter_map(|v| v.serving.as_ref())
            .map(|s| s.state.load())
            .collect();
        let recent = std::mem::take(&mut g.recent);
        if let Some(s) = g.current.serving.as_mut() {
            s.state.reclaim();
            s.external_pending = external;
            s.recent = recent;
        } else {
            g.recent = recent;
        }
        ServiceGuard(g)
    }

    pub(super) fn set_parallelism(&self, drives: usize) {
        self.parallelism
            .store(drives, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn lane_id(&self) -> usize {
        self.lane
    }

    pub(super) fn calibrate_capacity(&self, budget: Option<u64>) {
        if let Some(s) = self.lock().serving.as_mut() {
            s.capacity_known = budget.is_some();
            if budget == Some(0) {
                s.switch = Some(super::state::tape_state::FULL);
            }
            if let Some(free) = budget {
                // ingest 表示磁盘暂存消耗，尚未落带的部分不在这次 MAM 读数中。
                let staged = s
                    .state
                    .load()
                    .pending
                    .values()
                    .fold(0u64, |n, p| n.saturating_add(p.staged_len));
                s.state.calibrate(free.saturating_sub(staged));
            }
        }
    }

    pub(super) fn target_pool(&self) -> Option<String> {
        self.target_pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn bind_pool(&self, pool: &str) {
        *self.target_pool.lock().unwrap_or_else(|e| e.into_inner()) = Some(pool.into());
    }

    /// 新驱动共享同池的命名空间、任务号和发布视图；卷预算及批次仍各自独立。
    pub(super) fn add_lane(self: &Arc<Self>, pool: &str) -> Arc<Self> {
        self.add_lane_limited(pool, None).expect("不限制池视图创建")
    }

    fn add_lane_limited(self: &Arc<Self>, pool: &str, limit: Option<usize>) -> Option<Arc<Self>> {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let count = usize::from(self.target_pool().as_deref() == Some(pool))
            + peers
                .iter()
                .filter(|p| p.target_pool().as_deref() == Some(pool))
                .count();
        if limit.is_some_and(|limit| count >= limit) {
            return None;
        }
        let base = if self.target_pool().as_deref() == Some(pool) {
            Some(self.clone())
        } else {
            peers
                .iter()
                .find(|p| p.target_pool().as_deref() == Some(pool))
                .cloned()
        };
        let (inner, changed, lane) = match base {
            Some(base) => {
                let g = base.inner.lock().unwrap_or_else(|e| e.into_inner());
                let lane = g.others.keys().copied().chain([g.lane]).max().unwrap_or(0) + 1;
                (base.inner.clone(), base.changed.clone(), lane)
            }
            None => (
                Arc::new(Mutex::new(PoolState {
                    current: Inner::empty(),
                    lane: 0,
                    others: BTreeMap::new(),
                    recent: HashMap::new(),
                })),
                Arc::new(Condvar::new()),
                0,
            ),
        };
        // 立即登记，连续分配驱动时 lane 号不会重复。
        {
            let mut g = inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.lane != lane {
                g.others.entry(lane).or_insert_with(Inner::empty);
            }
        }
        let service = Arc::new(Self {
            permission: self.permission.clone(),
            catalog_quiesced: self.catalog_quiesced.clone(),
            inner,
            lane,
            target_pool: Mutex::new(Some(pool.into())),
            requested: std::sync::atomic::AtomicBool::new(false),
            parallelism: std::sync::atomic::AtomicUsize::new(1),
            peers: Mutex::new(Vec::new()),
            changed,
            spool_dir: self.spool_dir.clone(),
            exec: self.exec.clone(),
            task_events: self.task_events.clone(),
            policy: self.policy,
            directory_path: self.directory_path.clone(),
            export_active: self.export_active.clone(),
            reader: Mutex::new(None),
        });
        peers.push(service.clone());
        Some(service)
    }

    pub(super) fn lanes(self: &Arc<Self>) -> Vec<Arc<Self>> {
        std::iter::once(self.clone())
            .chain(
                self.peers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned(),
            )
            .collect()
    }

    pub(super) fn select_pool(
        self: &Arc<Self>,
        pool: &str,
        path: Option<&str>,
        pin: bool,
        write: bool,
    ) -> Option<Arc<Self>> {
        let mut lanes: Vec<_> = self
            .lanes()
            .into_iter()
            .filter(|p| p.target_pool().as_deref() == Some(pool))
            .collect();
        if pin
            && let Some(path) = path
            && let Some(st) = lanes.first().and_then(|p| p.stat(path).ok().flatten())
            && let Some(lane) = lanes
                .iter()
                .find(|p| p.tape_barcode().as_deref() == Some(&st.barcode))
        {
            return Some(lane.clone());
        }
        lanes.sort_by_key(|p| {
            let g = p.lock();
            g.serving
                .as_ref()
                .map(|s| {
                    (
                        s.draining || s.maintenance || !s.capacity_known || s.switch.is_some(),
                        s.admitted,
                    )
                })
                .unwrap_or((true, u64::MAX))
        });
        if write
            && !pin
            && lanes.len() < self.parallelism.load(std::sync::atomic::Ordering::Acquire)
            && lanes
                .first()
                .is_some_and(|lane| lane.lock().serving.as_ref().is_some_and(|s| s.admitted > 0))
        {
            let round = lanes[0].serving_round()?;
            let Some(lane) = self.add_lane_limited(
                pool,
                Some(self.parallelism.load(std::sync::atomic::Ordering::Acquire)),
            ) else {
                return lanes.into_iter().next();
            };
            lane.close_waiting(round, Some(pool.into()), "正在申请并行写入驱动器");
            lane.requested
                .store(true, std::sync::atomic::Ordering::Release);
            lane.kick();
            return Some(lane);
        }
        let lane = lanes.into_iter().next()?;
        if write && lane.tape_barcode().is_none() {
            lane.requested
                .store(true, std::sync::atomic::Ordering::Release);
            lane.kick();
        }
        Some(lane)
    }

    pub(super) fn needs_capacity(&self) -> bool {
        self.lock()
            .serving
            .as_ref()
            .is_some_and(|s| !s.capacity_known)
    }

    pub(super) fn requested(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn tape_barcode(&self) -> Option<String> {
        self.lock().serving.as_ref().map(|s| s.tape.barcode.clone())
    }

    pub(super) fn close_all(self: &Arc<Self>, reason: &str) {
        for lane in self.lanes() {
            lane.close(reason);
        }
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
        self.requested
            .store(false, std::sync::atomic::Ordering::Release);
        let state = Arc::new(VolumeState::new(
            round,
            committed_view(index),
            index.generation,
            free_bytes,
            writable,
        ));
        let catalog = catalog_of(index).0;
        let files_total = catalog
            .iter()
            .filter(|r| !is_directory(&r.metadata))
            .count() as u64;
        let mut records = Vec::new();
        for r in catalog {
            if self.directory_path.is_some() {
                match self.with_reader(|d| Some(d.lookup(&tape.pool_uuid, &r.path))) {
                    Some(Ok(Some(old)))
                        if old.barcode != tape.barcode && old.version >= r.version =>
                    {
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
        let mut recent = std::mem::take(&mut g.0.recent);
        if let Some(old) = g.serving.as_mut() {
            recent.extend(std::mem::take(&mut old.recent));
        }
        g.serving = Some(Serving {
            draining: false,
            maintenance: false,
            capacity_known: true,
            round,
            state,
            queue: VecDeque::new(),
            queue_bytes: 0,
            oldest: None,
            newest: None,
            in_batch: Vec::new(),
            namespace_pending: BTreeSet::new(),
            external_pending: Vec::new(),
            opened: Instant::now(),
            limits,
            admitted: 0,
            files_total,
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
        if let Some(s) = g.serving.as_mut() {
            for (path, old) in recent {
                if old.barcode != s.tape.barcode
                    && s.recent
                        .get(&path)
                        .is_none_or(|new| new.version < old.version)
                {
                    s.recent.insert(path, old);
                }
            }
        }
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

    pub(super) fn close_waiting(&self, round: u64, pool: Option<String>, reason: &str) {
        self.close_no_tape(round, pool, reason);
        self.lock().no_tape = false;
    }

    /// 写入侧是否空闲：队列空、没有正在提交的批、没有已准入但未完成的上传、没有待处理的换带。
    /// 只有一个驱动器时，只有这时才允许把写入带换下来读别的带。
    pub fn write_side_idle(&self, round: u64) -> bool {
        let g = self.lock();
        g.serving
            .as_ref()
            .filter(|s| s.round == round)
            .is_some_and(|s| {
                s.queue.is_empty() && s.in_batch.is_empty() && s.admitted == 0 && s.switch.is_none()
            })
    }

    /// 合批策略。回收按它的阈值切分搬迁的批次，与客户端上传共用同一个队列。
    pub fn policy(&self) -> BatchPolicy {
        self.policy
    }

    /// 队列里已完成、等着落带的 (文件数, 字节数)。
    pub fn queued(&self, round: u64) -> Option<(usize, u64)> {
        let g = self.lock();
        g.serving
            .as_ref()
            .filter(|s| s.round == round)
            .map(|s| (s.queue.len(), s.queue_bytes))
    }

    /// 目录里还指向这盘带的 (文件数, 字节数)。回收在格式化源带之前拿它做最后一道核对；
    /// 减去它就是这盘带上的可回收空间。
    pub fn live_on(&self, barcode: &str) -> Option<(u64, u64)> {
        self.with_reader(|d| d.live_on(barcode).ok())
    }

    /// 执行线程询问：当前这盘带是否需要换掉，换成什么状态。
    pub fn switch_requested(&self, round: u64) -> Option<&'static str> {
        self.lock()
            .serving
            .as_ref()
            .filter(|s| s.round == round)
            .and_then(|s| s.switch)
    }

    /// 维护期间关闭新操作准入，允许此前已准入上传完成并排队落带。
    pub fn pause_admission(&self) {
        if let Some(s) = self.lock().serving.as_mut() {
            s.maintenance = true;
        }
    }

    /// 维护任务在开始设备操作前取消时恢复准入；不会解除停机标记。
    pub fn resume_admission(&self, round: u64) {
        if let Some(s) = self.lock().serving.as_mut().filter(|s| s.round == round) {
            s.maintenance = false;
        }
    }

    /// 关闭新入队入口，保留已完成队列供计划停机落带。
    pub fn drain(&self) {
        if let Some(s) = self.lock().serving.as_mut() {
            s.draining = true;
        }
    }

    /// 停止服务。仅暂存的任务判失败，已进入提交批次的判结果未定。
    pub fn close(&self, reason: &str) {
        let mut g = self.lock();
        let Some(mut s) = g.serving.take() else {
            g.why_not = reason.to_string();
            g.no_tape = false;
            g.read_only = None;
            return;
        };
        g.0.recent = std::mem::take(&mut s.recent);
        for u in &s.queue {
            let _ = std::fs::remove_file(&u.spool);
            let status = TaskStatus::Failed {
                reason: format!("未落带：{}。请向新 Leader 重传", reason),
            };
            self.report_task(u.task, s.round, &s.tape.pool_uuid, &u.path, &status);
            g.tasks.insert(u.task, status);
        }
        for t in &s.in_batch {
            g.tasks.insert(
                *t,
                TaskStatus::Indeterminate {
                    reason: format!("提交未确认：{}。请向新 Leader 查询该路径", reason),
                },
            );
        }
        g.why_not = reason.to_string();
        g.no_tape = false;
        g.read_only = None;
        self.changed.notify_all();
    }

    pub(super) fn library_serving_round(self: &Arc<Self>) -> Option<u64> {
        self.lanes().iter().find_map(|lane| lane.serving_round())
    }

    pub fn serving_round(&self) -> Option<u64> {
        self.check_permission().ok()?;
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
        let by_wait = s
            .oldest
            .map(|t| (t + p.max_wait).saturating_duration_since(now));
        let by_idle = s
            .newest
            .map(|t| (t + p.idle).saturating_duration_since(now));
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
        self.batch_done_counted(round, batch, uploads, None, result);
    }

    pub(super) fn batch_done_counted(
        &self,
        round: u64,
        batch: &FrozenBatch,
        uploads: &[Upload],
        files_total: Option<u64>,
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
                        s.files_total = files_total
                            .unwrap_or_else(|| s.state.load().committed.count_files() as u64);
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
                            if s.recent
                                .get(&r.path)
                                .is_some_and(|old| old.version > r.version)
                            {
                                continue;
                            }
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
        let pool_uuid = s.tape.pool_uuid.clone();
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
            self.report_task(u.task, round, &pool_uuid, &u.path, &st);
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
        self.check_permission()?;
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
        self.check_permission()?;
        let path = norm(path)?;
        let (state, live) = self.namespace_snapshot(&path)?;
        let mut g = self.lock();
        self.require_catalog_writable()?;
        let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
        let why = g.why_not.clone();
        if g.serving.is_none() && g.no_tape {
            return Err(ServiceError::NoTape(why));
        }
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining || s.maintenance || !s.capacity_known {
            return Err(ServiceError::NotServing(if !s.capacity_known {
                "MAM 容量未知，暂停新写入并等待刷新".into()
            } else {
                "正在停机或管理维护".into()
            }));
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
        if s.files_total + s.admitted >= s.limits.file_limit {
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
            spool: self
                .spool_dir
                .join(format!("task-{}-{}.part", task, uuid::Uuid::new_v4())),
            path,
            round,
        })
    }

    pub fn ingest(&self, h: &UploadHandle, bytes: u64) -> Result<(), ServiceError> {
        self.check_permission()?;
        let g = self.lock();
        let s = g
            .serving
            .as_ref()
            .filter(|s| s.round == h.round)
            .ok_or(ServiceError::NotServing(g.why_not.clone()))?;
        s.state
            .ingest(&h.path, bytes)
            .map(|_| ())
            .map_err(map_state)
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
            let mut metadata = crate::ltfs::volume::FileMetadata::replacement(xattrs);
            metadata.meta.readonly = current.metadata["node"]["readonly"]
                .as_bool()
                .unwrap_or(false);
            Ok(Some(metadata))
        })();
        match metadata {
            Ok(metadata) => self.finish_with_metadata(h, len, metadata),
            Err(e) => {
                self.abort(h);
                Err(e)
            }
        }
    }

    pub(crate) fn finish_with_metadata(
        &self,
        h: UploadHandle,
        len: u64,
        metadata: Option<crate::ltfs::volume::FileMetadata>,
    ) -> Result<u64, ServiceError> {
        if let Err(e) = self.check_permission() {
            self.abort(h);
            return Err(e);
        }
        let pool = {
            let mut g = self.lock();
            let why = g.why_not.clone();
            let s = g
                .serving
                .as_mut()
                .filter(|s| s.round == h.round)
                .ok_or(ServiceError::NotServing(why))?;
            let pool = s.tape.pool_uuid.clone();
            if s.draining {
                let _ = s.state.cancel(h.task);
                s.admitted = s.admitted.saturating_sub(1);
                let _ = std::fs::remove_file(&h.spool);
                return Err(ServiceError::NotServing("正在停机，请重新上传".into()));
            }
            s.state.complete(&h.path).map_err(map_state)?;
            s.queue.push_back(Upload {
                task: h.task,
                path: h.path.clone(),
                spool: h.spool.clone(),
                len,
                delete: false,
                symlink_target: None,
                rename_from: None,
                xattr_change: None,
                node_change: None,
                directory: false,
                metadata,
            });
            s.queue_bytes += len;
            let now = Instant::now();
            s.oldest.get_or_insert(now);
            s.newest = Some(now);
            g.tasks.insert(h.task, TaskStatus::Staged);
            pool
        };
        self.report_task(h.task, h.round, &pool, &h.path, &TaskStatus::Staged);
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

    /// 复制目录是任务的权威状态；当前 owner 仅在受理报告尚未应用时提供本地暂态。
    pub fn task_for_pool(&self, pool: &str, id: u64) -> Option<TaskStatus> {
        if let Some(row) = self.with_reader(|d| d.file_task(pool, id).ok().flatten()) {
            return match row.status.as_str() {
                "staged" => Some(TaskStatus::Staged),
                "committed" => Some(TaskStatus::Committed {
                    generation: row.generation?,
                }),
                "failed" => Some(TaskStatus::Failed { reason: row.reason }),
                "indeterminate" => Some(TaskStatus::Indeterminate { reason: row.reason }),
                _ => None,
            };
        }
        (self.target_pool().as_deref() == Some(pool) && self.serving_round() == Some(id >> 32))
            .then(|| self.task(id))
            .flatten()
    }

    /// 在给调用方确认任务号前，等待受理/终态进入复制目录；无集群报告通道的单元测试沿用本地语义。
    pub fn task_recorded(&self, id: u64, expected: &TaskStatus, timeout: Duration) -> bool {
        if self
            .task_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return true;
        }
        let Some(pool) = self.target_pool() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            let row = self.with_reader(|d| d.file_task(&pool, id).ok().flatten());
            if let Some(row) = row {
                let accepted = match expected {
                    TaskStatus::Staged => true,
                    TaskStatus::Committed { generation } => {
                        row.status == "committed" && row.generation == Some(*generation)
                    }
                    TaskStatus::Failed { .. } => row.status == "failed",
                    TaskStatus::Indeterminate { .. } => row.status == "indeterminate",
                };
                if accepted {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn wait_task_for_pool(&self, pool: &str, id: u64, timeout: Duration) -> Option<TaskStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.task_for_pool(pool, id) {
                None if Instant::now() < deadline => {}
                Some(TaskStatus::Staged) if Instant::now() < deadline => {}
                status => return status,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// 等到任务离开 `Staged`，或超时。
    pub fn wait_task(&self, id: u64, timeout: Duration) -> Option<TaskStatus> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match g.current.tasks.get(&id) {
                None => return None,
                Some(TaskStatus::Staged) => {}
                Some(done) => return Some(done.clone()),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return g.current.tasks.get(&id).cloned();
            }
            g = self
                .changed
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// 直接打开当前目录文件做只读切换审计，避免命中快照替换前的缓存连接。
    pub fn catalog_cutover_audit(&self) -> crate::error::Result<super::directory::CutoverAudit> {
        let path = self
            .directory_path
            .as_ref()
            .ok_or_else(|| crate::error::TapeError::Ltfs("本节点没有目录库".into()))?;
        super::directory::Directory::open_reader(path)?.cutover_audit()
    }

    /// 从本节点正在应用的目录库制作一致的离线快照。调用方须再次核对控制状态，
    /// 因为 VACUUM 期间可能收到恢复命令；返回的 fd 已 unlink，不会被后续导出覆盖。
    pub fn catalog_export_frozen_snapshot(
        &self,
        expected_frozen_index: u64,
    ) -> crate::error::Result<CatalogSnapshotExport> {
        use crate::error::TapeError;
        if self
            .export_active
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(TapeError::Ltfs("本库已有目录快照导出正在执行".into()));
        }
        let lease = CatalogExportLease(self.export_active.clone());
        let path = self
            .directory_path
            .as_ref()
            .ok_or_else(|| TapeError::Ltfs("本节点没有目录库".into()))?;
        let live = Directory::open_reader(path)?;
        let before = live.cutover_audit()?;
        if expected_frozen_index == 0
            || before.frozen_index != Some(expected_frozen_index)
            || before.has_staging
            || before.has_unresolved_file_tasks
        {
            return Err(TapeError::Ltfs("目录未在指定冻结点排空".into()));
        }
        let dest = path.with_extension(format!("cutover-{}.db", uuid::Uuid::new_v4()));
        let tmp = dest.with_extension("tmp");
        let result = (|| {
            live.write_snapshot(&dest)?;
            let copy = Directory::open_reader(&dest)?.cutover_audit()?;
            if copy.applied_index < before.applied_index
                || copy.frozen_index != before.frozen_index
                || copy.has_staging
                || copy.has_unresolved_file_tasks
            {
                return Err(TapeError::Ltfs(
                    "目录快照冻结标记或债务与导出前审计不一致".into(),
                ));
            }
            let mut file = std::fs::File::open(&dest)?;
            std::fs::remove_file(&dest)?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 1024 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            file.rewind()?;
            Ok(CatalogSnapshotExport {
                file,
                applied_index: copy.applied_index,
                frozen_index: expected_frozen_index,
                sha256: digest.finalize().into(),
                lease,
            })
        })();
        let _ = std::fs::remove_file(dest);
        let _ = std::fs::remove_file(tmp);
        result
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
        self.check_permission()?;
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
                (None, Some((round, pool))) => {
                    if let Some(st) = g.0.recent.get(&path) {
                        return Ok((!st.deleted).then(|| st.clone()));
                    }
                    (*round, pool.clone())
                }
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
                (None, Some((round, pool))) => {
                    if let Some(st) = g.0.recent.get(&path) {
                        return Ok(Some(st.clone()));
                    }
                    (*round, pool.clone())
                }
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
        self.enqueue_removal(path, true, false, None)
    }

    pub fn delete_if_version(&self, path: &str, version: (u64, u64)) -> Result<u64, ServiceError> {
        let path = norm(path)?;
        self.enqueue_removal(path, true, false, Some(version))
    }

    /// 回收搬迁墓碑：源带上的墓碑要在别的带上重新落一次（新版本），源带才能格式化。
    /// 不检查路径是否存在——它本来就不存在。
    pub fn relocate_tombstone(&self, path: &str) -> Result<u64, ServiceError> {
        let path = norm(path)?;
        self.enqueue_removal(path, false, false, None)
    }

    pub(crate) fn relocate_tombstone_kind(
        &self,
        path: &str,
        directory: bool,
    ) -> Result<u64, ServiceError> {
        self.enqueue_removal(norm(path)?, false, directory, None)
    }

    fn enqueue_removal(
        &self,
        path: String,
        must_exist: bool,
        directory: bool,
        expected_version: Option<(u64, u64)>,
    ) -> Result<u64, ServiceError> {
        let observed = if expected_version.is_some() {
            self.lookup(&path)?
        } else {
            None
        };
        let (state, live) = self.namespace_snapshot(&path)?;
        {
            let mut g = self.lock();
            self.require_catalog_writable()?;
            let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
            let why = g.why_not.clone();
            if g.serving.is_none() && g.no_tape {
                return Err(ServiceError::NoTape(why));
            }
            let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
            if s.draining || s.maintenance || !s.capacity_known {
                return Err(ServiceError::NotServing(if !s.capacity_known {
                    "MAM 容量未知，暂停新写入并等待刷新".into()
                } else {
                    "正在停机或管理维护".into()
                }));
            }
            let live = Self::admission_view(s, &state, &path, live)?;
            if must_exist {
                check_file_namespace(s, &path, &live)?;
            } else {
                check_pending(s, &path)?;
                if s.recent
                    .get(&path)
                    .is_some_and(|st| st.barcode == s.tape.barcode)
                {
                    return Err(ServiceError::Exists(path));
                }
            }
            if must_exist && !live.contains_key(&path) {
                return Err(ServiceError::NotFound(path));
            }
            if let Some(expected) = expected_version {
                let actual = s
                    .recent
                    .get(&path)
                    .map(|st| st.version)
                    .or_else(|| observed.as_ref().map(|st| st.version));
                if actual != Some(expected) {
                    return Err(ServiceError::VersionMismatch(path));
                }
            }
            if let Some(state) = s.switch {
                return Err(ServiceError::SwitchingTape(format!(
                    "{} 已 {}",
                    s.tape.barcode, state
                )));
            }
            // 墓碑也是索引里的一条
            if s.files_total + s.admitted >= s.limits.file_limit {
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
            let report_path = path.clone();
            let report_pool = s.tape.pool_uuid.clone();
            let report_round = s.round;
            s.queue.push_back(Upload {
                task,
                path,
                spool: PathBuf::new(),
                len: 0,
                delete: true,
                directory,
                symlink_target: None,
                rename_from: None,
                xattr_change: None,
                node_change: None,
                metadata: None,
            });
            let now = Instant::now();
            s.oldest.get_or_insert(now);
            s.newest = Some(now);
            g.tasks.insert(task, TaskStatus::Staged);
            g.next_task += 1;
            self.report_task(
                task,
                report_round,
                &report_pool,
                &report_path,
                &TaskStatus::Staged,
            );
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
        self.require_catalog_writable()?;
        let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining || s.maintenance || !s.capacity_known {
            return Err(ServiceError::NotServing(if !s.capacity_known {
                "MAM 容量未知，暂停新写入并等待刷新".into()
            } else {
                "正在停机或管理维护".into()
            }));
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
        let report_path = path.clone();
        let report_pool = s.tape.pool_uuid.clone();
        let report_round = s.round;
        s.queue.push_back(Upload {
            task,
            path,
            spool: PathBuf::new(),
            len,
            delete: false,
            directory,
            symlink_target: None,
            rename_from: None,
            xattr_change: Some((key, attr)),
            node_change: None,
            metadata: None,
        });
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        self.report_task(
            task,
            report_round,
            &report_pool,
            &report_path,
            &TaskStatus::Staged,
        );
        drop(g);
        self.kick();
        Ok(task)
    }

    /// 当前写入卷上的原生权限/时间修改，与其他命名空间变更互斥。
    pub fn change_metadata(
        &self,
        path: &str,
        change: &crate::ltfs::index::MetadataChange,
    ) -> Result<u64, ServiceError> {
        change
            .validate()
            .map_err(|e| ServiceError::BadPath(e.to_string()))?;
        if change.is_empty() {
            return Err(ServiceError::BadPath("属性修改不能为空".into()));
        }
        let path = norm(path)?;
        let (state, live) = self.namespace_snapshot(&path)?;
        let mut g = self.lock();
        self.require_catalog_writable()?;
        let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining || s.maintenance || !s.capacity_known {
            return Err(ServiceError::NotServing(if !s.capacity_known {
                "MAM 容量未知，暂停新写入并等待刷新".into()
            } else {
                "正在停机或管理维护".into()
            }));
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
        let report_path = path.clone();
        let report_pool = s.tape.pool_uuid.clone();
        let report_round = s.round;
        s.queue.push_back(Upload {
            task,
            path,
            spool: PathBuf::new(),
            len,
            delete: false,
            directory,
            symlink_target: None,
            rename_from: None,
            xattr_change: None,
            node_change: Some(change.clone()),
            metadata: None,
        });
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        self.report_task(
            task,
            report_round,
            &report_pool,
            &report_path,
            &TaskStatus::Staged,
        );
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
        self.require_catalog_writable()?;
        let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining || s.maintenance || !s.capacity_known {
            return Err(ServiceError::NotServing(if !s.capacity_known {
                "MAM 容量未知，暂停新写入并等待刷新".into()
            } else {
                "正在停机或管理维护".into()
            }));
        }
        let live = Self::admission_view(s, &state, &path, live)?;
        check_pending(s, &path)?;
        // 回收源带不再可写；当前写带刚发布的同名记录已取代这份源副本。
        if metadata.is_some()
            && s.recent
                .get(&path)
                .is_some_and(|st| st.barcode == s.tape.barcode)
        {
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
        if symlink_target.is_some() && s.files_total + s.admitted >= s.limits.file_limit {
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
        let report_path = path.clone();
        let report_pool = s.tape.pool_uuid.clone();
        let report_round = s.round;
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
            node_change: None,
            metadata,
        });
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        self.report_task(
            task,
            report_round,
            &report_pool,
            &report_path,
            &TaskStatus::Staged,
        );
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
        self.require_catalog_writable()?;
        let task = task_id(g.serving.as_ref().map_or(0, |s| s.round), g.next_task)?;
        let why = g.why_not.clone();
        let s = g.serving.as_mut().ok_or(ServiceError::NotServing(why))?;
        if s.draining || s.maintenance || !s.capacity_known {
            return Err(ServiceError::NotServing(if !s.capacity_known {
                "MAM 容量未知，暂停新写入并等待刷新".into()
            } else {
                "正在停机或管理维护".into()
            }));
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
            let report_round = s.round;
            let report_pool = s.tape.pool_uuid.clone();
            g.tasks.insert(task, TaskStatus::Committed { generation });
            g.next_task += 1;
            self.report_task(task, report_round, &report_pool, &to, &TaskStatus::Staged);
            self.report_task(
                task,
                report_round,
                &report_pool,
                &to,
                &TaskStatus::Committed { generation },
            );
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
                symlink_target: None,
                rename_from: Some((path.clone(), *path == from)),
                xattr_change: None,
                node_change: None,
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
                symlink_target: None,
                rename_from: None,
                xattr_change: None,
                node_change: None,
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
        let report_pool = s.tape.pool_uuid.clone();
        let report_round = s.round;
        let now = Instant::now();
        s.oldest.get_or_insert(now);
        s.newest = Some(now);
        g.tasks.insert(task, TaskStatus::Staged);
        g.next_task += 1;
        self.report_task(task, report_round, &report_pool, &to, &TaskStatus::Staged);
        drop(g);
        self.kick();
        Ok(task)
    }

    pub fn list(&self) -> Result<BTreeMap<String, u64>, ServiceError> {
        Ok(self
            .list_nodes()?
            .into_iter()
            .filter(|(_, n)| !n.is_dir)
            .map(|(p, n)| (p, n.length))
            .collect())
    }

    fn list_nodes(&self) -> Result<BTreeMap<String, NodeSummary>, ServiceError> {
        self.list_nodes_scoped(None)
    }

    fn list_nodes_scoped(
        &self,
        dir: Option<&str>,
    ) -> Result<BTreeMap<String, NodeSummary>, ServiceError> {
        self.check_permission()?;
        let prefix = dir.map(|d| format!("{d}/"));
        let in_scope =
            |p: &str| dir.is_none_or(|d| p == d || p.starts_with(prefix.as_deref().unwrap()));
        let (pool, mut out, gone) = {
            let g = self.lock();
            match (&g.serving, &g.read_only) {
                (Some(s), _) => (
                    s.tape.pool_uuid.clone(),
                    s.recent
                        .iter()
                        .filter(|(p, st)| !st.deleted && in_scope(p))
                        .map(|(p, st)| (p.clone(), NodeSummary::new(st.len, &st.metadata)))
                        .collect::<BTreeMap<_, _>>(),
                    // 本轮删掉的路径盖住目录库里可能还没换成墓碑的旧行
                    s.recent
                        .iter()
                        .filter(|(p, st)| st.deleted && in_scope(p))
                        .map(|(p, _)| p.clone())
                        .collect::<std::collections::HashSet<_>>(),
                ),
                (None, Some((_, pool))) => (
                    pool.clone(),
                    g.0.recent
                        .iter()
                        .filter(|(p, st)| !st.deleted && in_scope(p))
                        .map(|(p, st)| (p.clone(), NodeSummary::new(st.len, &st.metadata)))
                        .collect(),
                    g.0.recent
                        .iter()
                        .filter(|(p, st)| st.deleted && in_scope(p))
                        .map(|(p, _)| p.clone())
                        .collect(),
                ),
                (None, None) => return Err(ServiceError::NotServing(g.why_not.clone())),
            }
        };
        let rows = if self.directory_path.is_some() {
            self.with_reader(|d| Some(d.list_nodes_scoped(&pool, dir)))
                .ok_or_else(|| ServiceError::Io("无法读取命名空间目录".into()))?
                .map_err(|e| ServiceError::Io(e.to_string()))?
        } else {
            Vec::new()
        };
        for (p, n) in rows {
            if !gone.contains(&p) {
                out.entry(p).or_insert(n);
            }
        }
        Ok(out)
    }

    /// 路径的全貌：已提交版本（同 `stat`）加在途变更。
    pub fn stat_full(&self, path: &str) -> Result<PathState, ServiceError> {
        let path = norm(path)?;
        let committed = self.stat(&path)?;
        let in_flight = self.in_flight(&path);
        Ok(PathState {
            committed,
            in_flight,
        })
    }

    fn in_flight(&self, path: &str) -> Option<(InFlight, u64)> {
        let g = self.lock();
        let s = g.serving.as_ref()?;
        if s.namespace_pending.contains(path) {
            return None;
        }
        if let Some(entry) = s
            .external_pending
            .iter()
            .find_map(|root| root.pending.get(path))
        {
            return Some((in_flight_of(entry), entry.staged_len));
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
            committed: Option<NodeSummary>,
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
                    is_symlink: false,
                    committed: None,
                    in_flight: None,
                }) {
                    DirEntry::File {
                        is_symlink,
                        committed: c,
                        in_flight: f,
                    } => {
                        if let Some(n) = committed {
                            *c = c.or(Some(n.length));
                            *is_symlink = n.is_symlink;
                        }
                        *f = f.or(flight);
                    }
                    // 同名的目录与文件：LTFS 里不会出现，目录优先
                    DirEntry::Dir => {}
                },
            }
        }
        let nodes =
            self.list_nodes_scoped((prefix != "/").then(|| prefix.trim_end_matches('/')))?;
        let own = nodes.get(prefix.trim_end_matches('/'));
        if own.is_some_and(|n| !n.is_dir) {
            return Err(ServiceError::NotDirectory(dir.into()));
        }
        let exists = prefix == "/" || own.is_some_and(|n| n.is_dir);
        for (p, node) in &nodes {
            if node.is_dir {
                add(&mut out, &prefix, &format!("{}/", p), None, None);
            } else {
                add(&mut out, &prefix, p, Some(*node), None);
            }
        }
        let flights: Vec<(String, (InFlight, u64))> = {
            let g = self.lock();
            match g.serving.as_ref() {
                Some(s) => s
                    .external_pending
                    .iter()
                    .flat_map(|root| root.pending.iter())
                    .filter(|(p, _)| p.starts_with(&prefix))
                    .map(|(p, e)| (p.clone(), (in_flight_of(e), e.staged_len)))
                    .chain(
                        s.state
                            .load()
                            .pending
                            .iter()
                            .filter(|(p, _)| {
                                p.starts_with(&prefix) && !s.namespace_pending.contains(*p)
                            })
                            .map(|(p, e)| (p.clone(), (in_flight_of(e), e.staged_len))),
                    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_quiesce_blocks_all_new_file_mutations_across_lanes() {
        let dir = std::env::temp_dir().join(format!("catalog-quiesce-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir.clone()).unwrap();
        files.bind_pool("pool");
        let other = files.add_lane("pool");
        let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        for (lane, barcode) in [(&files, "TEST01L8"), (&other, "TEST02L8")] {
            lane.open(
                1,
                TapeIdent {
                    pool_uuid: "pool".into(),
                    barcode: barcode.into(),
                },
                TapeLimits {
                    file_limit: 100,
                    usable_capacity: 100,
                },
                &index,
                100,
                true,
            );
        }
        let admitted = files.begin("/before", 0).unwrap();
        files.set_catalog_quiesced(true);
        let denied = |result: Result<u64, ServiceError>| {
            let detail = format!("{result:?}");
            assert!(
                matches!(result, Err(ServiceError::NotServing(s)) if s.contains("旧目录已暂停")),
                "{detail}"
            );
        };
        assert!(matches!(
            other.begin("/after", 0),
            Err(ServiceError::NotServing(s)) if s.contains("旧目录已暂停")
        ));
        denied(files.delete("/before"));
        denied(files.change_xattr("/before", "user.note", Some(b"x"), 0));
        denied(files.change_metadata(
            "/before",
            &crate::ltfs::index::MetadataChange {
                readonly: Some(true),
                ..Default::default()
            },
        ));
        denied(files.mkdir("/dir"));
        denied(files.symlink("/before", "/link"));
        denied(files.rename("/before", "/renamed", false));
        assert!(files.finish(admitted, 0).is_ok());
        files.set_catalog_quiesced(false);
        assert!(other.begin("/after", 0).is_ok());
        files.close_all("done");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copied_file_metadata_preserves_user_attributes_and_rejects_invalid_inputs() {
        let timestamp = "2026-10-22T00:00:00.000000000Z";
        let mut value = serde_json::json!({
            "node": {
                "readonly": true,
                "creation_time": timestamp,
                "change_time": timestamp,
                "modify_time": timestamp,
                "access_time": timestamp,
                "backup_time": timestamp,
            },
            "xattrs": [
                {"key":"note", "value":"cHJlc2VydmVk", "base64":true},
                {"key":"tapers.version", "value":"7.1", "base64":false},
            ],
        });
        let metadata = copied_file_metadata(&value).unwrap();
        assert!(metadata.meta.readonly);
        assert_eq!(metadata.meta.creation_time, timestamp);
        assert_eq!(metadata.xattrs.len(), 1);
        assert_eq!(metadata.xattrs[0].key, "note");
        value["xattrs"][0]["value"] = serde_json::json!("invalid!");
        assert!(copied_file_metadata(&value).is_err());
        value["xattrs"][0]["value"] = serde_json::json!("cHJlc2VydmVk");
        value["node"]["modify_time"] = serde_json::json!("2026-10-22T00:00:00Z");
        assert!(copied_file_metadata(&value).is_err());
    }

    #[test]
    fn another_volume_overwrite_does_not_free_physical_file_slots() {
        let dir = std::env::temp_dir().join(format!("physical-count-{}", uuid::Uuid::new_v4()));
        let one = FileService::new(dir.clone()).unwrap();
        one.bind_pool("pool");
        let two = one.add_lane("pool");
        let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        for (files, barcode, seq) in [(&one, "TEST01L8", 1), (&two, "TEST02L8", 2)] {
            files.open(
                1,
                TapeIdent {
                    pool_uuid: "pool".into(),
                    barcode: barcode.into(),
                },
                TapeLimits {
                    file_limit: 1,
                    usable_capacity: 100,
                },
                &index,
                100,
                true,
            );
            let h = files.begin("/same", 0).unwrap();
            files.finish(h, 0).unwrap();
            let (batch, uploads) = files.take_batch_now(1).unwrap();
            let rec = FileRec {
                path: "/same".into(),
                length: 0,
                sha256: String::new(),
                version: (1, seq),
                metadata: serde_json::json!({"xattrs":[]}),
                deleted: false,
            };
            files.batch_done_counted(
                1,
                &batch,
                &uploads,
                Some(1),
                Ok((2, [(rec.path.clone(), rec)].into())),
            );
        }
        assert_eq!(one.stat("/same").unwrap().unwrap().barcode, "TEST02L8");
        assert!(matches!(
            one.begin("/new", 0),
            Err(ServiceError::SwitchingTape(_))
        ));
        one.close_all("done");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fresh_mam_budget_still_accounts_for_uploads_not_yet_written() {
        let dir = std::env::temp_dir().join(format!("capacity-cut-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir.clone()).unwrap();
        let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        files.open(
            1,
            TapeIdent {
                barcode: "TEST01L8".into(),
                pool_uuid: "pool".into(),
            },
            TapeLimits {
                file_limit: 100,
                usable_capacity: 100,
            },
            &index,
            100,
            true,
        );
        let h = files.begin("/receiving", 80).unwrap();
        files.ingest(&h, 40).unwrap();
        files.calibrate_capacity(Some(100));
        assert_eq!(
            files
                .lock()
                .serving
                .as_ref()
                .unwrap()
                .state
                .load()
                .ledger
                .available(),
            20
        );
        files.ingest(&h, 40).unwrap();
        assert_eq!(
            files
                .lock()
                .serving
                .as_ref()
                .unwrap()
                .state
                .load()
                .ledger
                .available(),
            20
        );
        files.abort(h);
        files.close("done");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_count_triggers_batch_without_capping_its_size() {
        let dir = std::env::temp_dir().join(format!("file-service-batch-{}", uuid::Uuid::new_v4()));
        let files = FileService::with_options(
            dir.clone(),
            BatchPolicy {
                max_bytes: u64::MAX,
                max_files: 50,
                idle: Duration::from_secs(3600),
                max_wait: Duration::from_secs(3600),
            },
            None,
        )
        .unwrap();
        let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        files.open(
            1,
            TapeIdent {
                barcode: "TEST".into(),
                pool_uuid: "pool".into(),
            },
            TapeLimits {
                file_limit: 1000,
                usable_capacity: 1 << 20,
            },
            &index,
            4096,
            true,
        );
        // 不启动执行器：排队和取批由测试显式控制，阈值不依赖线程调度速度。
        for i in 0..51 {
            let h = files.begin(&format!("/file-{i}"), 1).unwrap();
            std::fs::write(&h.spool, b"x").unwrap();
            files.ingest(&h, 1).unwrap();
            files.finish(h, 1).unwrap();
            if i == 48 {
                assert!(files.due_in(1).unwrap() > Duration::ZERO);
                assert!(files.take_batch(1).is_none());
            } else if i >= 49 {
                assert_eq!(files.due_in(1), Some(Duration::ZERO));
            }
        }
        let (batch, uploads) = files.take_batch(1).unwrap();
        assert_eq!(uploads.len(), 51);
        assert_eq!(batch.cover.len(), 51);
        files.close("test complete");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn service_reclaims_old_roots_without_invalidating_readers() {
        let dir =
            std::env::temp_dir().join(format!("file-service-reclaim-{}", uuid::Uuid::new_v4()));
        let files = FileService::new(dir.clone()).unwrap();
        let index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        files.open(
            1,
            TapeIdent {
                barcode: "TEST".into(),
                pool_uuid: "pool".into(),
            },
            TapeLimits {
                file_limit: 1000,
                usable_capacity: 1024,
            },
            &index,
            1024,
            true,
        );
        let state = files.lock().serving.as_ref().unwrap().state.clone();
        let reader = state.load();
        for i in 0..100 {
            let handle = files.begin(&format!("/file-{i}"), 1).unwrap();
            files.ingest(&handle, 1).unwrap();
            files.abort(handle);
        }
        // 下一次服务操作回收无人引用的历史状态，但保留尚在读取的旧根。
        files.tape();
        assert_eq!(state.retired_count(), 1);
        assert!(reader.pending.is_empty());
        assert!(reader.capability.writable);
        assert!(state.load().pending.is_empty());
        drop(reader);
        files.tape();
        assert_eq!(state.retired_count(), 0);
        files.close("test complete");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
