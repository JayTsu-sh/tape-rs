//! 显式无业务控制副本。绑定只允许观察原链；它不授予设备或目录写权限。
use super::{
    directory::{CutoverAudit, Directory},
    executor::{ExecEvent, ExecRequest, ReadError},
    multilib::LibrariesConfig,
};
#[cfg(feature = "fdb")]
pub mod archive_verify;
pub mod lineage;

use crate::error::{Result, TapeError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    mpsc::{Receiver, RecvTimeoutError, Sender},
};
use std::time::Duration;

fn invalid(detail: impl Into<String>) -> TapeError {
    TapeError::Ltfs(detail.into())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSeal {
    pub dev: u64,
    pub inode: u64,
    pub sha256: String,
}
impl FileSeal {
    pub fn read(path: &Path) -> Result<Self> {
        use std::io::Read;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NOATIME)
            .open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(invalid("绑定源不是普通文件"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        if named.file_type().is_symlink()
            || before.dev() != named.dev()
            || before.ino() != named.ino()
            || before.len() != after.len()
            || (before.mtime(), before.mtime_nsec()) != (after.mtime(), after.mtime_nsec())
            || (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec())
        {
            return Err(invalid("绑定源在读取期间发生变化"));
        }
        Ok(Self {
            dev: before.dev(),
            inode: before.ino(),
            sha256: digest(&bytes),
        })
    }
    fn verify(&self, path: &Path) -> Result<()> {
        if !sha(&self.sha256) || &Self::read(path)? != self {
            return Err(invalid(format!(
                "原始来源身份或内容不符：{}",
                path.display()
            )));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRoute {
    pub namespace: String,
    pub revision: u64,
    pub frozen_index: u64,
    pub certificate_sha256: String,
    pub initial_owner_epoch: u64,
}
impl ExpectedRoute {
    fn valid(&self) -> bool {
        uuid::Uuid::parse_str(&self.namespace)
            .is_ok_and(|id| !id.is_nil() && id.to_string() == self.namespace)
            && self.revision > 0
            && self.frozen_index > 0
            && self.initial_owner_epoch > 0
            && sha(&self.certificate_sha256)
            && self.certificate_sha256 != "0".repeat(64)
    }
}
/// 实现必须查询权威 FDB；错误与路由缺失均不能退回本地目录。
pub trait AuthorityReader: Send + Sync {
    fn active_route(&self, serial: &str) -> Result<Option<ExpectedRoute>>;
}
/// SDK 的 cluster 文件只来自已封存的 memfd；原路径仅用于漂移检测。
pub struct PinnedCluster {
    file: std::fs::File,
    original_path: PathBuf,
    original: FileSeal,
    length: u64,
}
impl PinnedCluster {
    pub fn new(path: &Path, expected: &str) -> Result<Self> {
        Self::with_sealer(path, expected, seal_cluster_file)
    }
    fn with_sealer(
        path: &Path,
        expected: &str,
        seal: impl FnOnce(&std::fs::File) -> std::io::Result<()>,
    ) -> Result<Self> {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NOATIME)
            .open(path)?;
        let before = source.metadata()?;
        if !before.is_file() || before.len() == 0 || before.len() > 1 << 20 {
            return Err(invalid("FDB cluster 文件长度或类型无效"));
        }
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut source)
            .take((1 << 20) + 1)
            .read_to_end(&mut bytes)?;
        let after = source.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        let identity = |m: &std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if identity(&before) != identity(&after)
            || identity(&before) != identity(&named)
            || named.file_type().is_symlink()
            || bytes.len() as u64 != before.len()
            || !sha(expected)
            || digest(&bytes) != expected
        {
            return Err(invalid("FDB cluster 文件在固定前变化或摘要不符"));
        }
        let name = std::ffi::CString::new("tape-rs-control-cluster").expect("固定 memfd 名");
        // SAFETY: 名称以 NUL 结束，flags 为 Linux memfd 常量；负返回值不构造 File。
        let fd = unsafe {
            nix::libc::memfd_create(
                name.as_ptr(),
                nix::libc::MFD_CLOEXEC | nix::libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fd 为上一步新建且唯一持有的有效描述符，File 接管关闭责任。
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        seal(&file)?;
        let pinned = Self {
            file,
            original_path: path.to_path_buf(),
            original: FileSeal {
                dev: before.dev(),
                inode: before.ino(),
                sha256: expected.to_owned(),
            },
            length: bytes.len() as u64,
        };
        pinned.verify_sealed()?;
        debug_assert!(pinned.file.as_raw_fd() >= 0);
        Ok(pinned)
    }
    #[cfg(any(feature = "fdb", test))]
    fn sdk_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }
    fn verify_sealed(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::FileExt;
        // SAFETY: F_GET_SEALS 不读取用户指针，File 在本调用期间始终有效。
        let flags = unsafe { nix::libc::fcntl(self.file.as_raw_fd(), nix::libc::F_GET_SEALS) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if flags & cluster_seals() != cluster_seals() || self.file.metadata()?.len() != self.length
        {
            return Err(invalid("FDB cluster memfd 未完整封存"));
        }
        let mut bytes = vec![0; self.length as usize];
        self.file.read_exact_at(&mut bytes, 0)?;
        if digest(&bytes) != self.original.sha256 {
            return Err(invalid("FDB cluster memfd 回读不符"));
        }
        Ok(())
    }
    pub fn verify_source(&self) -> Result<()> {
        self.verify_sealed()?;
        self.original.verify(&self.original_path)
    }
}
fn cluster_seals() -> nix::libc::c_int {
    nix::libc::F_SEAL_WRITE
        | nix::libc::F_SEAL_GROW
        | nix::libc::F_SEAL_SHRINK
        | nix::libc::F_SEAL_SEAL
}
fn seal_cluster_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: F_ADD_SEALS 的第三参数为常量位集，没有用户指针，fd 来自仍存活的 File。
    if unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_ADD_SEALS, cluster_seals()) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(any(feature = "fdb", test))]
struct ControlOnlyConnection<T> {
    catalog: Arc<T>,
    cluster: Arc<PinnedCluster>,
}
#[cfg(any(feature = "fdb", test))]
fn open_catalog_from_pin<T>(
    cluster: Arc<PinnedCluster>,
    open: impl FnOnce(&Path) -> Result<T>,
) -> Result<ControlOnlyConnection<T>> {
    cluster.verify_source()?;
    let catalog = Arc::new(open(&cluster.sdk_path())?);
    cluster.verify_source()?;
    Ok(ControlOnlyConnection { catalog, cluster })
}
#[cfg(test)]
fn open_bound_catalog<T>(
    path: &Path,
    expected: &str,
    open: impl FnOnce(&Path) -> Result<T>,
) -> Result<ControlOnlyConnection<T>> {
    open_catalog_from_pin(Arc::new(PinnedCluster::new(path, expected)?), open)
}
#[cfg(feature = "fdb")]
pub struct FdbAuthorityReader {
    connection: ControlOnlyConnection<crate::managed_catalog::ManagedCatalog>,
}
#[cfg(feature = "fdb")]
impl FdbAuthorityReader {
    pub fn open(cluster: Arc<PinnedCluster>) -> Result<Self> {
        let connection = open_catalog_from_pin(cluster, |input| {
            let path = input
                .to_str()
                .ok_or_else(|| invalid("FDB cluster 路径不是 UTF-8"))?;
            crate::managed_catalog::ManagedCatalog::open(Some(path))
                .map_err(|e| invalid(e.to_string()))
        })?;
        Ok(Self { connection })
    }
}
#[cfg(feature = "fdb")]
impl AuthorityReader for FdbAuthorityReader {
    fn active_route(&self, serial: &str) -> Result<Option<ExpectedRoute>> {
        self.connection.cluster.verify_source()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let route = runtime
            .block_on(self.connection.catalog.active_library_route(serial))
            .map(|r| {
                r.map(|r| ExpectedRoute {
                    namespace: r.route.namespace.to_string(),
                    revision: r.route.revision,
                    frozen_index: r.frozen_index,
                    certificate_sha256: r
                        .certificate_sha256
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                    initial_owner_epoch: r.initial_owner_epoch,
                })
            })
            .map_err(|e| invalid(format!("FDB 控制绑定依赖未确认：{e}")))?;
        self.connection.cluster.verify_source()?;
        Ok(route)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum CatalogExpectation {
    ExistingRoute { route: ExpectedRoute },
    AbsentAfterReset {},
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryBinding {
    pub directory: FileSeal,
    pub quiesce_index: u64,
    pub frozen_index: u64,
    pub catalog: CatalogExpectation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlOnlyBinding {
    pub version: u32,
    pub node: u64,
    pub members: BTreeMap<u64, String>,
    pub cluster_sha256: String,
    pub libraries_sha256: String,
    pub group: String,
    pub data_root: PathBuf,
    pub data_root_dev: u64,
    pub data_root_inode: u64,
    /// 原官方 codec 回放/取证清单的摘要；启动时同时核本节点原 raft 源。
    pub original_chain_sha256: String,
    pub original_chain_manifest: PathBuf,
    pub raft: FileSeal,
    pub manifest: FileSeal,
    pub libraries: BTreeMap<String, LibraryBinding>,
}
impl ControlOnlyBinding {
    pub fn load(path: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| invalid(format!("control-only 绑定无效：{e}")))
    }
    pub fn validate_local(
        &self,
        root: &Path,
        node: u64,
        peers: &HashMap<u64, String>,
        cluster: &Path,
        config: &LibrariesConfig,
    ) -> Result<()> {
        self.verify_static(root, cluster, config)?;
        if self.version != 1
            || self.node != node
            || self.members != peers.iter().map(|(k, v)| (*k, v.clone())).collect()
            || self.members.len() != 3
            || !self.members.contains_key(&node)
            || self.members.iter().any(|(id, a)| {
                !(1..=255).contains(id) || a.parse::<std::net::SocketAddr>().is_err()
            })
            || self
                .members
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != 3
            || !sha(&self.original_chain_sha256)
        {
            return Err(invalid("control-only 成员、节点或原链绑定不符"));
        }
        lineage::verify(self, config)?;
        for library in &config.libraries {
            let binding = &self.libraries[&library.changer_serial];
            let path = config
                .library_dir(root, &library.changer_serial)
                .join("directory.db");
            check_no_journal(&path)?;
            binding.directory.verify(&path)?;
            match &binding.catalog {
                CatalogExpectation::ExistingRoute { route }
                    if route.valid()
                        && route.frozen_index == binding.frozen_index
                        && binding.quiesce_index > 0
                        && binding.quiesce_index <= binding.frozen_index => {}
                CatalogExpectation::AbsentAfterReset {} => {
                    return Err(invalid(
                        "AbsentAfterReset 回执 decoder 尚未审定，本批拒绝启动",
                    ));
                }
                _ => return Err(invalid("control-only 原冻结点或 FDB 路由绑定无效")),
            }
        }
        Ok(())
    }
    fn verify_static(&self, root: &Path, cluster: &Path, config: &LibrariesConfig) -> Result<()> {
        let meta = std::fs::symlink_metadata(root)?;
        if meta.file_type().is_symlink()
            || !meta.is_dir()
            || root != self.data_root
            || !root.is_absolute()
            || std::fs::canonicalize(root)? != root
            || meta.dev() != self.data_root_dev
            || meta.ino() != self.data_root_inode
            || digest(&std::fs::read(cluster)?) != self.cluster_sha256
            || digest(&serde_json::to_vec(&config.value()).map_err(|e| invalid(e.to_string()))?)
                != self.libraries_sha256
            || self.group != self.libraries_sha256
            || self.libraries.len() != config.libraries.len()
            || config
                .libraries
                .iter()
                .any(|l| !self.libraries.contains_key(&l.changer_serial))
        {
            return Err(invalid("control-only 原目录、集群或多库配置绑定不符"));
        }
        self.manifest.verify(&root.join("libraries.json"))?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("libraries.json"))?)
                .map_err(|e| invalid(e.to_string()))?;
        if manifest != config.value() {
            return Err(invalid("原持久多库 manifest 与配置不符"));
        }
        Ok(())
    }
    pub fn audit_source(
        self: &Arc<Self>,
        serial: &str,
        cluster: PathBuf,
        config: LibrariesConfig,
        reader: Arc<dyn AuthorityReader>,
    ) -> Result<BoundDirectoryAudit> {
        if !self.libraries.contains_key(serial) {
            return Err(invalid("绑定没有该带库"));
        }
        Ok(BoundDirectoryAudit {
            binding: self.clone(),
            serial: serial.into(),
            cluster,
            config,
            reader,
        })
    }
}
fn check_no_journal(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::symlink_metadata(PathBuf::from(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(invalid("原目录存在未核清的 WAL/SHM/journal 来源")),
        }
    }
    if std::fs::canonicalize(path)? != path {
        return Err(invalid("原目录路径含符号链接或别名"));
    }
    Ok(())
}
#[derive(Clone)]
pub struct BoundDirectoryAudit {
    binding: Arc<ControlOnlyBinding>,
    serial: String,
    cluster: PathBuf,
    config: LibrariesConfig,
    reader: Arc<dyn AuthorityReader>,
}
impl BoundDirectoryAudit {
    fn verify(&self) -> Result<PathBuf> {
        self.binding
            .verify_static(&self.binding.data_root, &self.cluster, &self.config)?;
        lineage::verify(&self.binding, &self.config)?;
        let library = &self.binding.libraries[&self.serial];
        let CatalogExpectation::ExistingRoute { route } = &library.catalog else {
            return Err(invalid("不支持的重置绑定"));
        };
        if self.reader.active_route(&self.serial)?.as_ref() != Some(route) {
            return Err(invalid("FDB 原路由、证书或 namespace 已变化"));
        }
        let path = self
            .config
            .library_dir(&self.binding.data_root, &self.serial)
            .join("directory.db");
        check_no_journal(&path)?;
        library.directory.verify(&path)?;
        Ok(path)
    }
    pub fn verify_startup(
        &self,
        root: &Path,
        node: u64,
        peers: &HashMap<u64, String>,
        config: &LibrariesConfig,
    ) -> Result<()> {
        self.binding
            .validate_local(root, node, peers, &self.cluster, config)?;
        self.audit()?;
        Ok(())
    }
    pub fn audit(&self) -> Result<CutoverAudit> {
        let path = self.verify()?;
        // 将 SQLite 读取固定到已核对的原 fd，防止检查与打开之间路径被替换。
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NOATIME)
            .open(&path)?;
        let before = file.metadata()?;
        let expected_seal = &self.binding.libraries[&self.serial].directory;
        if before.dev() != expected_seal.dev
            || before.ino() != expected_seal.inode
            || !before.is_file()
        {
            return Err(invalid("SQLite 审计 fd 与原目录身份不符"));
        }
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let audit = Directory::open_immutable_reader(&pinned)?.cutover_audit()?;
        let after = file.metadata()?;
        if (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) {
            return Err(invalid("原目录在 SQLite 审计期间发生变化"));
        }
        let expected = &self.binding.libraries[&self.serial];
        self.verify()?;
        if audit.frozen_index != Some(expected.frozen_index)
            || audit.applied_index < expected.frozen_index
            || audit.has_staging
            || audit.has_unresolved_file_tasks
            || audit.has_catalog_rows
        {
            return Err(invalid("原冻结目录非空或冻结水位不符"));
        }
        Ok(audit)
    }
    pub(crate) fn verify_control_status(&self, status: &super::node::NodeStatus) -> Result<()> {
        let committed = lineage::verify(&self.binding, &self.config)?;
        let control = committed
            .controls
            .get(&self.serial)
            .ok_or_else(|| invalid("原链缺少带库状态"))?;
        let expected = &self.binding.libraries[&self.serial];
        if committed.term != status.term
            || control.applied_index != status.applied_index
            || control.executor != status.executor
            || control.executor_ready != status.executor_ready
            || control.candidates != status.candidates
            || status.id != self.binding.node
            || status.catalog_quiesce_index != Some(expected.quiesce_index)
            || status.catalog_frozen_index != Some(expected.frozen_index)
        {
            return Err(invalid("运行中节点或原 Frozen/quiesce 控制绑定不符"));
        }
        Ok(())
    }
    pub fn description(&self) -> serde_json::Value {
        serde_json::json!({"mode":"control_only","catalog":self.binding.libraries[&self.serial].catalog,
            "audit_source":self.binding.libraries[&self.serial].directory,"quiesce_index":self.binding.libraries[&self.serial].quiesce_index,
            "frozen_index":self.binding.libraries[&self.serial].frozen_index,"business_ready":false})
    }
}
/// 无 provider 参数；只公布真实零设备能力，绝不发布 Fenced/Serving/业务成功。
pub fn run_deny(requests: Receiver<ExecRequest>, events: Sender<ExecEvent>) {
    let capability = || {
        let _ = events.send(ExecEvent::Capability {
            drives: 0,
            eligible: false,
        });
    };
    capability();
    loop {
        match requests.recv_timeout(Duration::from_millis(500)) {
            Ok(ExecRequest::Shutdown { done }) => {
                if let Some(done) = done {
                    let _ = done.send(());
                }
                break;
            }
            Ok(request) => reject(request, &events),
            Err(RecvTimeoutError::Timeout) => capability(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
fn reject(request: ExecRequest, events: &Sender<ExecEvent>) {
    let reason = "显式 control-only 模式禁止业务与设备执行".to_string();
    match request {
        ExecRequest::Takeover { round } => {
            let _ = events.send(ExecEvent::FenceFailed { round, reason });
        }
        ExecRequest::Recover { round } => {
            let _ = events.send(ExecEvent::Lost { round, reason });
        }
        ExecRequest::Scoped { request, .. } => reject(*request, events),
        ExecRequest::Read { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed(reason)));
        }
        #[cfg(feature = "fdb")]
        ExecRequest::FdbReplicaRead { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed("控制面节点不能读带".into())));
        }
        ExecRequest::ReadTo { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed(reason)));
        }
        ExecRequest::Sync { reply, .. } => {
            let _ = reply.send(Err(reason));
        }
        #[cfg(feature = "fdb")]
        ExecRequest::FdbPrepare { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed(reason)));
        }
        #[cfg(feature = "fdb")]
        ExecRequest::FdbWrite { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed(reason)));
        }
        #[cfg(feature = "fdb")]
        ExecRequest::LibraryHandoff { reply, .. } => {
            let _ = reply.send(Err(ReadError::Failed(reason)));
        }
        ExecRequest::AttachPermission { .. } | ExecRequest::Stop { .. } | ExecRequest::Work => {}
        ExecRequest::Shutdown { .. } => {}
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod binding_fixture;
#[cfg(all(test, feature = "fdb"))]
mod sdk_smoke;
