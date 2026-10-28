//! Shared, bounded FoundationDB catalog primitives for large managed libraries.
//!
//! The caller must boot the FoundationDB network once before opening a catalog.
//! Media commit evidence and Raft/PR fencing are prerequisites to `publish`.

use foundationdb::options::DatabaseOption;
use foundationdb::{Database, FdbBindingError, KeySelector, RangeOption, Transaction};
use uuid::Uuid;

mod attribute_objects;
mod attributes;
pub use attributes::NodeAttributes;
use attributes::{AttributeManifest, CHUNK_SIZE, INLINE_LIMIT, MAX_REPRESENTABLE};
mod active_volume;
pub use active_volume::{
    ActiveIndexIdentity, ActiveIndexOutcome, ActiveLoadedVolumeHead, ActiveRecoveryOutcome,
};
mod active_batch;
pub use active_batch::{
    ActiveBatchOutcome, ActiveBatchPhase, ActiveBatchRecoveryPage, ActiveBatchStatus,
    ActiveCompactionOutcome, ActiveFinalizationOutcome, ActiveMaterializationOutcome,
    ActivePreparationOutcome, ActivePublicationOutcome, ActiveSealedBatchPage,
};
mod verification;
pub use verification::{ScanDigest, ScanSummary, VerificationOutcome};
mod cleanup;
mod control;
mod cutover;
mod history;
mod import_materialize;
mod import_reconcile;
mod import_spool;
mod index_evidence;
mod lookup;
mod maintenance;
mod namespace_verification;
mod physical;
pub use physical::{
    ActiveFileIntent, PreparedActiveFile, prepare_active_file, prepare_committed_active_file,
};
mod requests;
mod reservation;
mod routing;
mod staged_import;
mod staged_sources;
pub use cleanup::PurgeProgress;
pub use control::{ControlOutcome, PoolPolicy, PoolRecord, TapeAssignment, VolumePlacement};
pub use cutover::{
    FrozenSnapshotIdentity, VerifiedNodeExport, inspect_frozen_snapshot, verify_node_export,
};
pub use history::CopyCompactionPage;
pub use import_materialize::{
    ImportDataCopy, ImportDataFile, ImportProjectionProgress, SealedStagingPurgeProgress,
};
pub use import_reconcile::{
    DirectoryAudit, ImportOrigin, ImportReconciler, ImportVolume, ReconciledPath, SourceCandidate,
};
pub use import_spool::{
    ImportCandidate, ImportCandidatePage, ImportCandidateStats, ImportDataSource, ImportNode,
    ImportPage, ImportSpool, ImportSpoolStats,
};
pub use index_evidence::{ImportSourceBinding, LocalVolumeSources, SourceOutcome};
pub use lookup::{ActiveReadSource, ResolvedActiveRead, ResolvedPath, ResolvedRead};
pub use maintenance::{
    HistoryReference, MaintenanceBlocker, MaintenanceDiagnosis, MaintenanceStatus, PendingRequestId,
};
pub use maintenance::{MaintenanceAction, MaintenanceDriver, MaintenancePolicy, MaintenanceReport};
pub use namespace_verification::{
    NamespaceDigest, NamespacePhase, NamespaceSummary, NamespaceVerificationOutcome,
};
pub use requests::{RequestWindow, WindowOutcome};
pub use routing::{
    ActiveLibraryRoute, ActiveOwnerOutcome, CutoverCertificate, CutoverNode, ForgetRouteOutcome,
    PrepareRouteOutcome, PreparedLibraryRoute,
};
pub use staged_import::{StagedImportEntry, StagedImportSummary, StagedSourceData};
pub use staged_sources::StagedSourceSummary;

const PREFIX: &[u8] = b"tape-rs/catalog/22/";
const MAX_PAGE: usize = 1000;
const MAX_NAME: usize = 255;
const MAX_DEPTH: u16 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("FDB 目录操作失败: {0}")]
    Fdb(#[from] FdbBindingError),
    #[error("FDB 客户端初始化失败: {0}")]
    Client(#[from] foundationdb::FdbError),
    #[error("无效目录参数: {0}")]
    Invalid(&'static str),
    #[error("FDB 目录记录损坏: {0}")]
    Corrupt(&'static str),
    #[error("卷索引缓存校验失败: {0}")]
    Cache(String),
    #[error("目录索引未完成，暂不能判定文件是否存在")]
    Incomplete,
    #[error("目录维护任务已被新的执行者接管")]
    MaintenanceFenced,
    #[error("带库 owner 已由新的执行轮次接管")]
    OwnerFenced,
}

pub type Result<T> = std::result::Result<T, CatalogError>;

/// Preferred checksum carried by the LTFS index. `Unknown` does not prove
/// that the file's content has been verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentDigest {
    Unknown,
    Md5([u8; 16]),
    Sha256([u8; 32]),
}

impl ContentDigest {
    /// Extract the preferred checksum from a complete LTFS file entry.
    /// Duplicate or malformed hash xattrs are unsafe to import as evidence.
    pub fn from_ltfs_xattrs(xattrs: &[crate::ltfs::index::Xattr]) -> Result<Self> {
        use crate::ltfs::index::{XATTR_MD5, XATTR_SHA256};
        let mut sha256 = None;
        let mut md5 = None;
        for xattr in xattrs {
            match xattr.key.as_str() {
                XATTR_SHA256 => {
                    if sha256.is_some() {
                        return Err(CatalogError::Invalid("重复的 LTFS SHA-256 属性"));
                    }
                    sha256 = Some(parse_ltfs_hex::<32>(xattr)?);
                }
                XATTR_MD5 => {
                    if md5.is_some() {
                        return Err(CatalogError::Invalid("重复的 LTFS MD5 属性"));
                    }
                    md5 = Some(parse_ltfs_hex::<16>(xattr)?);
                }
                _ => {}
            }
        }
        Ok(if let Some(value) = sha256 {
            Self::Sha256(value)
        } else if let Some(value) = md5 {
            Self::Md5(value)
        } else {
            Self::Unknown
        })
    }

    fn tag(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Sha256(_) => 1,
            Self::Md5(_) => 2,
        }
    }

    fn compact_len(self) -> usize {
        match self {
            Self::Unknown => 0,
            Self::Md5(_) => 16,
            Self::Sha256(_) => 32,
        }
    }

    fn payload(self) -> [u8; 32] {
        let mut payload = [0; 32];
        match self {
            Self::Unknown => {}
            Self::Sha256(bytes) => payload = bytes,
            Self::Md5(bytes) => payload[..16].copy_from_slice(&bytes),
        }
        payload
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self> {
        if payload.len() != 32 {
            return Err(CatalogError::Corrupt("内容摘要长度"));
        }
        match tag {
            0 if payload.iter().all(|byte| *byte == 0) => Ok(Self::Unknown),
            1 => Ok(Self::Sha256(payload.try_into().expect("validated length"))),
            2 if payload[16..].iter().all(|byte| *byte == 0) => Ok(Self::Md5(
                payload[..16].try_into().expect("validated length"),
            )),
            _ => Err(CatalogError::Corrupt("内容摘要算法或编码")),
        }
    }

    fn decode_compact(tag: u8, payload: &[u8]) -> Result<Self> {
        match (tag, payload.len()) {
            (0, 0) => Ok(Self::Unknown),
            (1, 32) => Ok(Self::Sha256(payload.try_into().expect("validated length"))),
            (2, 16) => Ok(Self::Md5(payload.try_into().expect("validated length"))),
            _ => Err(CatalogError::Corrupt("内容摘要算法或编码")),
        }
    }
}

fn parse_ltfs_hex<const N: usize>(xattr: &crate::ltfs::index::Xattr) -> Result<[u8; N]> {
    let raw = xattr.value.as_bytes();
    if xattr.base64 || raw.len() != 2 * N {
        return Err(CatalogError::Invalid("LTFS 内容哈希格式无效"));
    }
    let mut result = [0; N];
    for (index, pair) in raw.as_chunks::<2>().0.iter().enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| CatalogError::Invalid("LTFS 内容哈希格式无效"))?;
        result[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| CatalogError::Invalid("LTFS 内容哈希格式无效"))?;
    }
    Ok(result)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReceipt {
    pub namespace: Uuid,
    pub parent: Uuid,
    pub name: Vec<u8>,
    pub file_id: Uuid,
    pub expected_version: Option<u64>,
    /// Stable retry identity within this namespace (not a global task ID).
    pub operation_id: Uuid,
    pub request_epoch: u64,
    pub copy_id: Uuid,
    pub library_serial: String,
    pub pool_id: Option<Uuid>,
    pub owner_epoch: u64,
    pub volume_id: Uuid,
    pub volume_generation: u64,
    pub volume_scan_id: Uuid,
    /// LTFS fileuid in this volume generation; used to resolve local extents.
    pub ltfs_file_uid: u64,
    pub size: u64,
    pub content_digest: ContentDigest,
    /// Hash of the exact LTFS attributes published with this media commit.
    pub metadata_digest: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    Registered,
    AdmissionLimited,
    RequestExpired,
    RequestEpochClosed,
    RequestNotRegistered,
    RequestCancelled,
    InvalidCopyId,
    Applied(u64),
    AlreadyApplied(u64),
    StaleOwner,
    VersionConflict,
    NameConflict,
    OperationConflict,
    /// The requested Active tape generation has already been observed.
    /// A pending request must be reconciled, not cancelled as unwritten.
    GenerationConflict,
    /// A sealed or partially staged media batch owns this Active request.
    BatchClaimed,
    MissingParent,
    NodeConflict,
    VolumeNotReady,
    PlacementMismatch,
    NamespaceNotReady,
    NamespaceNotBuilding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NamespaceState {
    Building = 0,
    Ready = 1,
    Verifying = 2,
    Verified = 3,
    Rejected = 4,
    Discarded = 5,
    /// Verified import that can be read but cannot accept ordinary writes.
    Sealed = 6,
    /// Cutover-selected namespace; only future owner-fenced APIs may mutate it.
    Active = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeOutcome {
    Ready,
    AlreadyReady,
    Retired,
    Reconciling,
    AlreadyReconciling,
    NotReconciling,
    ScanConflict,
    Cancelled,
    AlreadyRetired,
    NotVerified,
    StaleOwner,
    OlderGeneration,
    SerialConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryOutcome {
    Created,
    AlreadyExists,
    MissingParent,
    NameConflict,
    IdConflict,
    NamespaceNotReady,
    NamespaceNotBuilding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Uuid,
    pub name: Vec<u8>,
    pub attributes: Option<NodeAttributes>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    pub name: Vec<u8>,
    pub id: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque exclusive cursor; valid only for the same query prefix.
    pub next: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeCopy {
    pub namespace: Uuid,
    pub file_id: Uuid,
    pub copy_id: Uuid,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHead {
    pub version: u64,
    pub size: u64,
    pub copy_id: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalCopy {
    pub version: u64,
    pub request_epoch: u64,
    pub volume_id: Uuid,
    pub volume_generation: u64,
    pub volume_scan_id: Uuid,
    pub ltfs_file_uid: u64,
    pub size: u64,
    pub content_digest: ContentDigest,
    /// Exact on-medium identity for imported files. Legacy storage records may
    /// lack provenance; serving reads still require verified managed placement.
    pub media: Option<MediaProvenance>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaProvenance {
    pub version: (u64, u64),
    pub logical_file_uid: u64,
    /// Relative path in the verified LTFS index. EE aliases point at their
    /// `.LTFSEE_DATA` object; ordinary files use their own path.
    pub physical_path: String,
    pub ee_alias: bool,
    /// A zero-data LTFS link, retained as metadata and never read as payload.
    pub native_symlink: bool,
}

enum StoredCopy {
    Present(PhysicalCopy),
    Reclaimed { version: u64, request_epoch: u64 },
}

impl StoredCopy {
    fn version(&self) -> u64 {
        match self {
            Self::Present(copy) => copy.version,
            Self::Reclaimed { version, .. } => *version,
        }
    }

    fn request_epoch(&self) -> u64 {
        match self {
            Self::Present(copy) => copy.request_epoch,
            Self::Reclaimed { request_epoch, .. } => *request_epoch,
        }
    }
}

/// Stable across retries and owner takeover; a fresh ID isolates each scan,
/// including repeated scans of an unchanged LTFS generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeScan {
    pub generation: u64,
    pub id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ScanState {
    Building = 0,
    Ready = 1,
    Retired = 2,
    Verifying = 3,
    Verified = 4,
    Rejected = 5,
}

#[derive(Clone, Copy)]
enum ScanAction {
    Begin,
    Ready,
    Cancel,
}

pub struct ManagedCatalog {
    db: Database,
}

impl ManagedCatalog {
    pub fn open(cluster_file: Option<&str>) -> Result<Self> {
        let db = Database::new(cluster_file)?;
        db.set_option(DatabaseOption::TransactionTimeout(5000))?;
        db.set_option(DatabaseOption::TransactionRetryLimit(8))?;
        Ok(Self { db })
    }

    /// Initialize an empty namespace. The nil inode is its stable root.
    pub async fn create_namespace(&self, namespace: Uuid) -> Result<bool> {
        self.init_namespace(namespace, NamespaceState::Ready, false)
            .await
    }

    /// Create a new namespace whose file admissions require immutable volume placement.
    pub async fn create_managed_namespace(&self, namespace: Uuid) -> Result<bool> {
        self.init_namespace(namespace, NamespaceState::Ready, true)
            .await
    }

    /// Start a new, invisible namespace generation for a verified bulk import.
    /// Readers fail closed until `seal_import` succeeds.
    pub async fn begin_import(&self, namespace: Uuid) -> Result<bool> {
        self.init_namespace(namespace, NamespaceState::Building, false)
            .await
    }

    /// Build a managed logical library without exposing its files to readers.
    /// Media placement and the source catalog must be independently verified
    /// before `seal_import` makes this namespace available.
    pub async fn begin_managed_import(&self, namespace: Uuid) -> Result<bool> {
        self.init_namespace(namespace, NamespaceState::Building, true)
            .await
    }

    async fn init_namespace(
        &self,
        namespace: Uuid,
        state: NamespaceState,
        managed: bool,
    ) -> Result<bool> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace ID 不能为 nil"));
        }
        let root = node_key(namespace, Uuid::nil());
        let state_key = namespace_key(namespace);
        let managed_key = control::managed_key(namespace);
        let source_revision_key = source_revision_key(namespace);
        let default_group = control::node_group_key(namespace, "G0");
        let value = encode_node(&Node {
            kind: NodeKind::Directory,
            parent: Uuid::nil(),
            name: Vec::new(),
            attributes: None,
        });
        self.db
            .run(|tr, _| {
                let root = root.clone();
                let state_key = state_key.clone();
                let managed_key = managed_key.clone();
                let source_revision_key = source_revision_key.clone();
                let default_group = default_group.clone();
                let value = value.clone();
                async move {
                    let old_root = tr.get(&root, false).await?;
                    let old_state = tr.get(&state_key, false).await?;
                    let old_managed = tr.get(&managed_key, false).await?;
                    let old_source_revision = tr.get(&source_revision_key, false).await?;
                    let old_group = tr.get(&default_group, false).await?;
                    if old_state.as_deref() == Some(&[NamespaceState::Discarded as u8]) {
                        return Ok(Err(CatalogError::Invalid("废弃的 namespace ID 不能复用")));
                    }
                    match (old_root, old_state) {
                        (Some(root_bytes), Some(state_bytes)) => {
                            if root_bytes.as_ref() != value
                                || decode_namespace_state(&state_bytes).ok() != Some(state)
                            {
                                return Ok(Err(CatalogError::Corrupt("namespace root/state")));
                            }
                            if old_managed
                                .as_deref()
                                .is_some_and(|bytes| !bytes.is_empty())
                            {
                                return Ok(Err(CatalogError::Corrupt("受管命名空间标记")));
                            }
                            if old_managed.is_some() != managed {
                                return Ok(Err(CatalogError::Invalid(
                                    "namespace 管理模式不能更改",
                                )));
                            }
                            if managed
                                && old_source_revision
                                    .as_deref()
                                    .is_none_or(|bytes| bytes.len() != 8)
                            {
                                return Ok(Err(CatalogError::Corrupt(
                                    "受管命名空间扫描修订号缺失或损坏",
                                )));
                            }
                            if !managed && old_source_revision.is_some() {
                                return Ok(Err(CatalogError::Corrupt(
                                    "非受管命名空间存在扫描修订号",
                                )));
                            }
                            if old_group.as_deref() != Some(&[][..]) {
                                return Ok(Err(CatalogError::Corrupt("默认节点组缺失或损坏")));
                            }
                            let window = tr.get(&requests::window_key(namespace), false).await?;
                            if window
                                .as_deref()
                                .map(requests::decode_window)
                                .transpose()
                                .is_err()
                                || window.is_none()
                            {
                                return Ok(Err(CatalogError::Corrupt("请求代次窗口缺失或损坏")));
                            }
                            Ok(Ok(false))
                        }
                        (None, None) => {
                            if old_managed.is_some()
                                || old_group.is_some()
                                || old_source_revision.is_some()
                            {
                                return Ok(Err(CatalogError::Corrupt("受管命名空间缺少根记录")));
                            }
                            tr.set(&root, &value);
                            tr.set(&state_key, &[state as u8]);
                            tr.set(&default_group, b"");
                            if managed {
                                tr.set(&managed_key, b"");
                                tr.set(&source_revision_key, &0u64.to_be_bytes());
                            }
                            tr.set(
                                &requests::window_key(namespace),
                                &requests::encode_window(RequestWindow {
                                    first_retained: 1,
                                    accepting: 1,
                                }),
                            );
                            maintenance::initialize(&tr, namespace);
                            Ok(Ok(true))
                        }
                        _ => Ok(Err(CatalogError::Corrupt("namespace root/state 缺失"))),
                    }
                }
            })
            .await?
    }

    pub async fn namespace_state(&self, namespace: Uuid) -> Result<Option<NamespaceState>> {
        let tr = self.db.create_trx()?;
        tr.get(&namespace_key(namespace), true)
            .await?
            .map(|bytes| decode_namespace_state(&bytes))
            .transpose()
    }

    /// Expose an imported namespace only after all verification phases passed.
    pub async fn seal_import(&self, namespace: Uuid) -> Result<bool> {
        let state_key = namespace_key(namespace);
        let staged_key = staged_import::manifest_key(namespace);
        self.db
            .run(|tr, _| {
                let state_key = state_key.clone();
                let staged_key = staged_key.clone();
                async move {
                    if tr.get(&staged_key, false).await?.is_some() {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let state = tr.get(&state_key, false).await?;
                    let Some(state) = state else {
                        return Ok(Err(CatalogError::Incomplete));
                    };
                    match decode_namespace_state(&state) {
                        Ok(NamespaceState::Verified) => {
                            tr.set(&state_key, &[NamespaceState::Ready as u8]);
                            Ok(Ok(true))
                        }
                        Ok(NamespaceState::Ready) => Ok(Ok(false)),
                        Ok(_) => Ok(Err(CatalogError::Incomplete)),
                        Err(error) => Ok(Err(error)),
                    }
                }
            })
            .await?
    }

    pub async fn create_directory(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
        id: Uuid,
    ) -> Result<DirectoryOutcome> {
        self.create_directory_in_state(namespace, parent, name, id, NamespaceState::Ready, None)
            .await
    }

    pub async fn create_directory_with_attributes(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
        id: Uuid,
        attributes: &NodeAttributes,
    ) -> Result<DirectoryOutcome> {
        self.create_directory_in_state(
            namespace,
            parent,
            name,
            id,
            NamespaceState::Ready,
            Some(attributes),
        )
        .await
    }

    pub async fn create_import_directory(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
        id: Uuid,
    ) -> Result<DirectoryOutcome> {
        self.create_directory_in_state(namespace, parent, name, id, NamespaceState::Building, None)
            .await
    }

    pub async fn create_import_directory_with_attributes(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
        id: Uuid,
        attributes: &NodeAttributes,
    ) -> Result<DirectoryOutcome> {
        self.create_directory_in_state(
            namespace,
            parent,
            name,
            id,
            NamespaceState::Building,
            Some(attributes),
        )
        .await
    }

    /// The root has no parent entry; import its source attributes before
    /// freezing the namespace proof.
    pub async fn set_import_root_attributes(
        &self,
        namespace: Uuid,
        attributes: &NodeAttributes,
    ) -> Result<bool> {
        if namespace.is_nil() || attributes.symlink().is_some() {
            return Err(CatalogError::Invalid("根目录身份或 LTFS 属性无效"));
        }
        self.upload_attributes(namespace, attributes).await?;
        let state = namespace_key(namespace);
        let root = node_key(namespace, Uuid::nil());
        let value = encode_node(&Node {
            kind: NodeKind::Directory,
            parent: Uuid::nil(),
            name: Vec::new(),
            attributes: Some(attributes.clone()),
        });
        let attributes = attributes.clone();
        self.db
            .run(|tr, _| {
                let (state, root, value) = (state.clone(), root.clone(), value.clone());
                let attributes = attributes.clone();
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let old = tr
                        .get(&root, false)
                        .await?
                        .ok_or(CatalogError::Corrupt("命名空间根节点缺失"));
                    let old = match old {
                        Ok(old) => old,
                        Err(error) => return Ok(Err(error)),
                    };
                    let decoded = match decode_node(&old) {
                        Ok(node) => node,
                        Err(error) => return Ok(Err(error)),
                    };
                    if decoded.kind != NodeKind::Directory
                        || !decoded.parent.is_nil()
                        || !decoded.name.is_empty()
                    {
                        return Ok(Err(CatalogError::Corrupt("命名空间根节点损坏")));
                    }
                    if old.as_ref() == value {
                        return Ok(Ok(false));
                    }
                    if decoded.attributes.is_some() {
                        return Ok(Err(CatalogError::Invalid("根目录 LTFS 属性与已有导入冲突")));
                    }
                    if let Err(error) = write_attribute_chunks(
                        &tr,
                        namespace,
                        Uuid::nil(),
                        false,
                        Some(&attributes),
                    )
                    .await
                    {
                        return Ok(Err(error));
                    }
                    tr.set(&root, &value);
                    Ok(Ok(true))
                }
            })
            .await?
    }

    async fn create_directory_in_state(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
        id: Uuid,
        required: NamespaceState,
        attributes: Option<&NodeAttributes>,
    ) -> Result<DirectoryOutcome> {
        validate_name(name)?;
        if attributes.is_some_and(|attributes| attributes.symlink().is_some()) {
            return Err(CatalogError::Invalid("目录不能带符号链接属性"));
        }
        if namespace.is_nil() || id.is_nil() {
            return Err(CatalogError::Invalid("namespace 和目录 inode 不能为 nil"));
        }
        if let Some(attributes) = attributes {
            self.upload_attributes(namespace, attributes).await?;
        }
        let parent_key = node_key(namespace, parent);
        let child_key = child_key(namespace, parent, name);
        let node_key = node_key(namespace, id);
        let state_key = namespace_key(namespace);
        let name = name.to_vec();
        let attributes = attributes.cloned();
        self.db
            .run(|tr, _| {
                let (parent_key, child_key, node_key, state_key, name) = (
                    parent_key.clone(),
                    child_key.clone(),
                    node_key.clone(),
                    state_key.clone(),
                    name.clone(),
                );
                let attributes = attributes.clone();
                async move {
                    let state = tr.get(&state_key, false).await?;
                    match state
                        .as_ref()
                        .map(|b| decode_namespace_state(b))
                        .transpose()
                    {
                        Ok(Some(actual)) if actual == required => {}
                        Ok(Some(_)) => {
                            return Ok(Ok(if required == NamespaceState::Ready {
                                DirectoryOutcome::NamespaceNotReady
                            } else {
                                DirectoryOutcome::NamespaceNotBuilding
                            }));
                        }
                        Ok(None) => return Ok(Err(CatalogError::Incomplete)),
                        Err(error) => return Ok(Err(error)),
                    }
                    let Some(parent_bytes) = tr.get(&parent_key, false).await? else {
                        return Ok(Ok(DirectoryOutcome::MissingParent));
                    };
                    let parent_node = match decode_node(&parent_bytes) {
                        Ok(node) => node,
                        Err(error) => return Ok(Err(error)),
                    };
                    if parent_node.kind != NodeKind::Directory {
                        return Ok(Ok(DirectoryOutcome::MissingParent));
                    }
                    let value = encode_node(&Node {
                        kind: NodeKind::Directory,
                        parent,
                        name,
                        attributes: attributes.clone(),
                    });
                    let child = tr.get(&child_key, false).await?;
                    let node = tr.get(&node_key, false).await?;
                    if child.as_ref().is_some_and(|bytes| bytes.len() != 16) {
                        return Ok(Err(CatalogError::Corrupt("目录项 inode 长度")));
                    }
                    if let Some(bytes) = node.as_ref()
                        && let Err(error) = decode_node(bytes)
                    {
                        return Ok(Err(error));
                    }
                    if child.as_ref().is_some_and(|b| b.as_ref() == id.as_bytes())
                        && node.as_ref().is_some_and(|b| b.as_ref() == value)
                    {
                        return Ok(Ok(DirectoryOutcome::AlreadyExists));
                    }
                    if child.is_some() {
                        return Ok(Ok(DirectoryOutcome::NameConflict));
                    }
                    if node.is_some() {
                        return Ok(Ok(DirectoryOutcome::IdConflict));
                    }
                    if let Err(error) =
                        write_attribute_chunks(&tr, namespace, id, false, attributes.as_ref()).await
                    {
                        return Ok(Err(error));
                    }
                    tr.set(&child_key, id.as_bytes());
                    tr.set(&node_key, &value);
                    Ok(Ok(DirectoryOutcome::Created))
                }
            })
            .await?
    }

    pub async fn node(&self, namespace: Uuid, id: Uuid) -> Result<Option<Node>> {
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
            && let Some((locator, receipt, _)) =
                lookup::published_file_receipt_at(&tr, namespace, id).await?
        {
            let attributes = active_batch::prepared_attributes_at(&tr, locator.batch_id, &receipt)
                .await?
                .ok_or(CatalogError::Corrupt("已发布活动 inode 缺少属性"))?;
            drop(tr);
            return Ok(Some(Node {
                kind: NodeKind::File,
                parent: receipt.parent,
                name: receipt.name,
                attributes: Some(self.hydrate_attributes(namespace, attributes).await?),
            }));
        }
        let Some(bytes) = tr.get(&node_key(namespace, id), true).await? else {
            return Ok(None);
        };
        let mut node = decode_node(&bytes)?;
        drop(tr);
        if let Some(attributes) = node.attributes.take() {
            node.attributes = Some(self.hydrate_attributes(namespace, attributes).await?);
        }
        Ok(Some(node))
    }

    /// Advance the FDB fence only after the Raft owner and PR fence are confirmed.
    /// Older owners can then no longer publish into this physical library.
    pub async fn advance_owner_epoch(&self, serial: &str, epoch: u64) -> Result<bool> {
        validate_serial(serial)?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("owner epoch 必须大于零"));
        }
        let key = owner_key(serial);
        self.db
            .run(|tr, _| {
                let key = key.clone();
                async move {
                    let old = tr.get(&key, false).await?;
                    let old_epoch = match old.as_ref() {
                        Some(bytes) if bytes.len() == 8 => read_u64(bytes),
                        Some(_) => return Ok(Err(CatalogError::Corrupt("owner epoch 长度"))),
                        None => 0,
                    };
                    if epoch <= old_epoch {
                        return Ok(Ok(false));
                    }
                    tr.set(&key, &epoch.to_be_bytes());
                    Ok(Ok(true))
                }
            })
            .await?
    }

    /// Hide volume queries while a fresh scan is imported. Reusing the same
    /// token resumes import; frozen verification resumes via its page API.
    /// Another pending token is rejected.
    pub async fn begin_volume_reconcile(
        &self,
        serial: &str,
        epoch: u64,
        volume_id: Uuid,
        scan: VolumeScan,
    ) -> Result<VolumeOutcome> {
        self.transition_volume(serial, epoch, volume_id, scan, ScanAction::Begin)
            .await
    }

    /// Expose a scan only after the catalog verifies its full count and digest.
    /// Switching the pointer also retires the previous scan and fences imports.
    pub async fn mark_volume_ready(
        &self,
        serial: &str,
        epoch: u64,
        volume_id: Uuid,
        scan: VolumeScan,
    ) -> Result<VolumeOutcome> {
        self.transition_volume(serial, epoch, volume_id, scan, ScanAction::Ready)
            .await
    }

    /// Abandon only the exact pending scan. Retired IDs cannot be reused, so
    /// late imports cannot revive a cancelled candidate after its rows are purged.
    pub async fn cancel_volume_reconcile(
        &self,
        serial: &str,
        epoch: u64,
        volume_id: Uuid,
        scan: VolumeScan,
    ) -> Result<VolumeOutcome> {
        self.transition_volume(serial, epoch, volume_id, scan, ScanAction::Cancel)
            .await
    }

    async fn transition_volume(
        &self,
        serial: &str,
        epoch: u64,
        volume_id: Uuid,
        scan: VolumeScan,
        action: ScanAction,
    ) -> Result<VolumeOutcome> {
        validate_serial(serial)?;
        validate_scan(volume_id, scan)?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("owner epoch 必须有效"));
        }
        let owner = owner_key(serial);
        let ready_key = ready_volume_key(volume_id);
        let pending_key = reconcile_volume_key(volume_id);
        let manifest_key = scan_key(volume_id, scan.id);
        let value = encode_ready_volume(scan, serial);
        self.db
            .run(|tr, _| {
                let (owner, ready_key, pending_key, manifest_key, value) = (
                    owner.clone(),
                    ready_key.clone(),
                    pending_key.clone(),
                    manifest_key.clone(),
                    value.clone(),
                );
                async move {
                    let owner_epoch = tr.get(&owner, false).await?;
                    if owner_epoch.as_ref().is_some_and(|b| b.len() != 8) {
                        return Ok(Err(CatalogError::Corrupt("owner epoch 长度")));
                    }
                    if owner_epoch.as_ref().map(|b| read_u64(b)) != Some(epoch) {
                        return Ok(Ok(VolumeOutcome::StaleOwner));
                    }
                    match control::volume_serial_matches(&tr, volume_id, serial).await {
                        Ok(true) => {}
                        Ok(false) => return Ok(Ok(VolumeOutcome::SerialConflict)),
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    }
                    let managed_namespace =
                        match control::managed_volume_namespace(&tr, volume_id).await {
                            Ok(namespace) => namespace,
                            Err(CatalogError::Fdb(error)) => return Err(error),
                            Err(CatalogError::Client(error)) => return Err(error.into()),
                            Err(error) => return Ok(Err(error)),
                        };
                    if let Some(namespace) = managed_namespace {
                        let state = tr.get(&namespace_key(namespace), false).await?;
                        if state.as_deref() != Some(&[NamespaceState::Ready as u8][..])
                            && state.as_deref() != Some(&[NamespaceState::Building as u8][..])
                            && state.as_deref() != Some(&[NamespaceState::Verified as u8][..])
                        {
                            return Ok(Err(CatalogError::Invalid(
                                "已封口或活动的受管命名空间不能变更卷扫描",
                            )));
                        }
                    }
                    let unavailable = managed_namespace
                        .map(|namespace| unavailable_volume_key(namespace, volume_id));
                    let ready = tr.get(&ready_key, false).await?;
                    let pending = tr.get(&pending_key, false).await?;
                    for bytes in [ready.as_deref(), pending.as_deref()].into_iter().flatten() {
                        let (previous, previous_serial) = match decode_ready_volume(bytes) {
                            Ok(value) => value,
                            Err(error) => return Ok(Err(error)),
                        };
                        if previous_serial != serial {
                            return Ok(Ok(VolumeOutcome::SerialConflict));
                        }
                        if previous.generation > scan.generation {
                            return Ok(Ok(VolumeOutcome::OlderGeneration));
                        }
                    }
                    let manifest = tr.get(&manifest_key, false).await?;
                    let state = match manifest.as_deref().map(decode_scan_manifest).transpose() {
                        Ok(Some((saved_scan, saved_serial, state))) => {
                            if saved_scan != scan || saved_serial != serial {
                                return Ok(Ok(VolumeOutcome::ScanConflict));
                            }
                            Some(state)
                        }
                        Ok(None) => None,
                        Err(error) => return Ok(Err(error)),
                    };
                    let is_ready = ready.as_deref() == Some(value.as_slice());
                    let is_pending = pending.as_deref() == Some(value.as_slice());
                    match action {
                        ScanAction::Begin => {
                            if is_ready && pending.is_none() && state == Some(ScanState::Ready) {
                                return Ok(Ok(VolumeOutcome::AlreadyReady));
                            }
                            if is_pending && state == Some(ScanState::Building) {
                                return Ok(Ok(VolumeOutcome::AlreadyReconciling));
                            }
                            if manifest.is_some() {
                                return Ok(Ok(VolumeOutcome::ScanConflict));
                            }
                            if let Some(previous) = &pending {
                                let (previous_scan, previous_serial) =
                                    match decode_ready_volume(previous) {
                                        Ok(value) => value,
                                        Err(error) => return Ok(Err(error)),
                                    };
                                let previous_key = scan_key(volume_id, previous_scan.id);
                                let previous_manifest = tr.get(&previous_key, false).await?;
                                let retired = encode_scan_manifest(
                                    previous_scan,
                                    &previous_serial,
                                    ScanState::Retired,
                                );
                                if previous_manifest.as_deref() != Some(retired.as_slice()) {
                                    return Ok(Ok(VolumeOutcome::ScanConflict));
                                }
                            }
                            if let Some(namespace) = managed_namespace {
                                match advance_source_revision(&tr, namespace).await {
                                    Ok(()) => {}
                                    Err(CatalogError::Fdb(error)) => return Err(error),
                                    Err(CatalogError::Client(error)) => return Err(error.into()),
                                    Err(error) => return Ok(Err(error)),
                                }
                            }
                            tr.set(
                                &manifest_key,
                                &encode_scan_manifest(scan, serial, ScanState::Building),
                            );
                            tr.set(&pending_key, &value);
                            if let Some(ref unavailable) = unavailable {
                                tr.set(unavailable, &[]);
                            }
                            Ok(Ok(VolumeOutcome::Reconciling))
                        }
                        ScanAction::Ready => {
                            if is_ready && pending.is_none() && state == Some(ScanState::Ready) {
                                let source_ready = match index_evidence::managed_source_ready(
                                    &tr, volume_id, scan,
                                )
                                .await
                                {
                                    Ok(ready) => ready,
                                    Err(CatalogError::Fdb(error)) => return Err(error),
                                    Err(CatalogError::Client(error)) => return Err(error.into()),
                                    Err(error) => return Ok(Err(error)),
                                };
                                if !source_ready {
                                    return Ok(Ok(VolumeOutcome::NotVerified));
                                }
                                return Ok(Ok(VolumeOutcome::AlreadyReady));
                            }
                            if !is_pending || state == Some(ScanState::Retired) {
                                return Ok(Ok(VolumeOutcome::NotReconciling));
                            }
                            if state != Some(ScanState::Verified) {
                                return Ok(Ok(VolumeOutcome::NotVerified));
                            }
                            let source_ready =
                                match index_evidence::managed_source_ready(&tr, volume_id, scan)
                                    .await
                                {
                                    Ok(ready) => ready,
                                    Err(CatalogError::Fdb(error)) => return Err(error),
                                    Err(CatalogError::Client(error)) => return Err(error.into()),
                                    Err(error) => return Ok(Err(error)),
                                };
                            if !source_ready {
                                return Ok(Ok(VolumeOutcome::NotVerified));
                            }
                            let retired_old = if let Some(old) = ready {
                                let (old_scan, old_serial) = match decode_ready_volume(&old) {
                                    Ok(value) => value,
                                    Err(error) => return Ok(Err(error)),
                                };
                                let old_key = scan_key(volume_id, old_scan.id);
                                let old_manifest = tr.get(&old_key, false).await?;
                                let expected =
                                    encode_scan_manifest(old_scan, &old_serial, ScanState::Ready);
                                if old_manifest.as_deref() != Some(expected.as_slice()) {
                                    return Ok(Err(CatalogError::Corrupt("扫描就绪状态")));
                                }
                                Some((
                                    old_key,
                                    encode_scan_manifest(old_scan, &old_serial, ScanState::Retired),
                                ))
                            } else {
                                None
                            };
                            if let Some(namespace) = managed_namespace {
                                match advance_source_revision(&tr, namespace).await {
                                    Ok(()) => {}
                                    Err(CatalogError::Fdb(error)) => return Err(error),
                                    Err(CatalogError::Client(error)) => return Err(error.into()),
                                    Err(error) => return Ok(Err(error)),
                                }
                            }
                            if let Some((old_key, retired)) = retired_old {
                                tr.set(&old_key, &retired);
                            }
                            tr.set(
                                &manifest_key,
                                &encode_scan_manifest(scan, serial, ScanState::Ready),
                            );
                            tr.set(&ready_key, &value);
                            tr.clear(&pending_key);
                            if let Some(ref unavailable) = unavailable {
                                tr.clear(unavailable);
                            }
                            Ok(Ok(VolumeOutcome::Ready))
                        }
                        ScanAction::Cancel => {
                            if state == Some(ScanState::Retired) {
                                return Ok(Ok(VolumeOutcome::AlreadyRetired));
                            }
                            if !is_pending {
                                return Ok(Ok(VolumeOutcome::NotReconciling));
                            }
                            if !matches!(
                                state,
                                Some(
                                    ScanState::Building
                                        | ScanState::Verifying
                                        | ScanState::Verified
                                        | ScanState::Rejected
                                )
                            ) {
                                return Ok(Err(CatalogError::Corrupt("扫描候选状态")));
                            }
                            if let Some(namespace) = managed_namespace {
                                match advance_source_revision(&tr, namespace).await {
                                    Ok(()) => {}
                                    Err(CatalogError::Fdb(error)) => return Err(error),
                                    Err(CatalogError::Client(error)) => return Err(error.into()),
                                    Err(error) => return Ok(Err(error)),
                                }
                            }
                            tr.set(
                                &manifest_key,
                                &encode_scan_manifest(scan, serial, ScanState::Retired),
                            );
                            // Keep the observed generation as an unavailable barrier.
                            // A fresh scan may replace it; cancellation cannot prove
                            // that the former ready index still matches the medium.
                            Ok(Ok(VolumeOutcome::Cancelled))
                        }
                    }
                }
            })
            .await?
    }

    /// Publish one already verified media commit. The operation marker makes
    /// retries safe even if FDB reports an unknown commit result.
    pub async fn publish(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        self.publish_in_state(receipt, NamespaceState::Ready, None)
            .await
    }

    /// Publish the full LTFS attributes in the same transaction as the head,
    /// physical copy, reverse index and operation marker.
    pub async fn publish_with_attributes(
        &self,
        receipt: &CommitReceipt,
        attributes: &NodeAttributes,
    ) -> Result<PublishOutcome> {
        self.publish_in_state(receipt, NamespaceState::Ready, Some(attributes))
            .await
    }

    /// Import one verified tape entry into an invisible namespace generation.
    /// `seal_import` fences in-flight imports before any reader can see them.
    pub async fn publish_imported(&self, receipt: &CommitReceipt) -> Result<PublishOutcome> {
        self.publish_in_state(receipt, NamespaceState::Building, None)
            .await
    }

    pub async fn publish_imported_with_attributes(
        &self,
        receipt: &CommitReceipt,
        attributes: &NodeAttributes,
    ) -> Result<PublishOutcome> {
        self.publish_in_state(receipt, NamespaceState::Building, Some(attributes))
            .await
    }

    async fn publish_in_state(
        &self,
        receipt: &CommitReceipt,
        required: NamespaceState,
        attributes: Option<&NodeAttributes>,
    ) -> Result<PublishOutcome> {
        validate_receipt(receipt)?;
        if receipt.metadata_digest != attributes.map(NodeAttributes::digest) {
            return Err(CatalogError::Invalid("收据与 LTFS 节点属性摘要不匹配"));
        }
        if let Some(attributes) = attributes
            && attributes.symlink().is_none()
        {
            if attributes
                .meta()
                .ok_or(CatalogError::Invalid("不能发布未读取的 LTFS 属性"))?
                .file_uid
                != receipt.ltfs_file_uid
            {
                return Err(CatalogError::Invalid("LTFS 文件属性与副本 fileuid 不一致"));
            }
            if ContentDigest::from_ltfs_xattrs(
                attributes
                    .xattrs()
                    .ok_or(CatalogError::Invalid("不能发布未读取的 LTFS 属性"))?,
            )? != receipt.content_digest
            {
                return Err(CatalogError::Invalid("LTFS 文件属性与副本内容摘要不一致"));
            }
        }
        if let Some(attributes) = attributes {
            self.upload_attributes(receipt.namespace, attributes)
                .await?;
        }
        let attributes = attributes.cloned();
        let fingerprint = receipt_fingerprint(receipt);
        let op = op_key(
            receipt.namespace,
            receipt.request_epoch,
            receipt.operation_id,
        );
        let state_key = namespace_key(receipt.namespace);
        let owner = owner_key(&receipt.library_serial);
        let child = child_key(receipt.namespace, receipt.parent, &receipt.name);
        let parent_node = node_key(receipt.namespace, receipt.parent);
        let file_node = node_key(receipt.namespace, receipt.file_id);
        let head = head_key(receipt.namespace, receipt.file_id);
        let copy = copy_key(receipt.namespace, receipt.file_id, receipt.copy_id);
        let ready_volume = ready_volume_key(receipt.volume_id);
        let reconciling_volume = reconcile_volume_key(receipt.volume_id);
        let expected = receipt.expected_version.unwrap_or(0);
        let next = expected
            .checked_add(1)
            .ok_or(CatalogError::Invalid("文件版本溢出"))?;
        self.db
            .run(|tr, _| {
                let (
                    op,
                    state_key,
                    owner,
                    child,
                    parent_node,
                    file_node,
                    head,
                    copy,
                    ready_volume,
                    reconciling_volume,
                ) = (
                    op.clone(),
                    state_key.clone(),
                    owner.clone(),
                    child.clone(),
                    parent_node.clone(),
                    file_node.clone(),
                    head.clone(),
                    copy.clone(),
                    ready_volume.clone(),
                    reconciling_volume.clone(),
                );
                let fingerprint = fingerprint.clone();
                let receipt = receipt.clone();
                let attributes = attributes.clone();
                async move {
                    // A permanent discard tombstone wins even over a surviving
                    // dedup marker while bounded cleanup is still in progress.
                    let state = tr.get(&state_key, false).await?;
                    let state = match state
                        .as_ref()
                        .map(|b| decode_namespace_state(b))
                        .transpose()
                    {
                        Ok(state) => state,
                        Err(error) => return Ok(Err(error)),
                    };
                    if state == Some(NamespaceState::Discarded) {
                        return Ok(Ok(if required == NamespaceState::Ready {
                            PublishOutcome::NamespaceNotReady
                        } else {
                            PublishOutcome::NamespaceNotBuilding
                        }));
                    }
                    if state.is_some() {
                        let window = match requests::window(&tr, receipt.namespace).await {
                            Ok(window) => window,
                            Err(CatalogError::Fdb(error)) => return Err(error),
                            Err(CatalogError::Client(error)) => return Err(error.into()),
                            Err(error) => return Ok(Err(error)),
                        };
                        if receipt.request_epoch < window.first_retained {
                            return Ok(Ok(PublishOutcome::RequestExpired));
                        }
                        if receipt.request_epoch > window.accepting {
                            return Ok(Ok(PublishOutcome::RequestEpochClosed));
                        }
                    }
                    if let Some(old) = tr.get(&op, false).await? {
                        return Ok(requests::terminal_result(&old, &fingerprint));
                    }
                    if state != Some(required) {
                        return Ok(Ok(if required == NamespaceState::Ready {
                            PublishOutcome::NamespaceNotReady
                        } else {
                            PublishOutcome::NamespaceNotBuilding
                        }));
                    }
                    if receipt.copy_id != derive_copy_id(&receipt) {
                        return Ok(Ok(PublishOutcome::InvalidCopyId));
                    }
                    let intent = requests::intent_key(&receipt);
                    let admission = tr.get(&intent, false).await?;
                    let Some(admission) = admission else {
                        return Ok(Ok(PublishOutcome::RequestNotRegistered));
                    };
                    let permit = match requests::matching_permit(&admission, &receipt) {
                        Ok(Some(permit)) => permit,
                        Ok(None) => return Ok(Ok(PublishOutcome::OperationConflict)),
                        Err(error) => return Ok(Err(error)),
                    };
                    match control::receipt_placement_matches(&tr, &receipt).await {
                        Ok(true) => {}
                        Ok(false) => return Ok(Ok(PublishOutcome::PlacementMismatch)),
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    }
                    let epoch = tr.get(&owner, false).await?;
                    if epoch.as_ref().is_some_and(|b| b.len() != 8) {
                        return Ok(Err(CatalogError::Corrupt("owner epoch 长度")));
                    }
                    if epoch.as_ref().map(|b| read_u64(b)) != Some(receipt.owner_epoch) {
                        return Ok(Ok(PublishOutcome::StaleOwner));
                    }
                    let readiness_key = if required == NamespaceState::Building {
                        &reconciling_volume
                    } else {
                        &ready_volume
                    };
                    let readiness = tr.get(readiness_key, false).await?;
                    let Some(readiness) = readiness else {
                        return Ok(Ok(PublishOutcome::VolumeNotReady));
                    };
                    let (scan, serial) = match decode_ready_volume(&readiness) {
                        Ok(value) => value,
                        Err(error) => return Ok(Err(error)),
                    };
                    if scan.generation != receipt.volume_generation
                        || scan.id != receipt.volume_scan_id
                        || serial != receipt.library_serial
                    {
                        return Ok(Ok(PublishOutcome::VolumeNotReady));
                    }
                    let scan_state = if required == NamespaceState::Building {
                        ScanState::Building
                    } else {
                        ScanState::Ready
                    };
                    let manifest = tr.get(&scan_key(receipt.volume_id, scan.id), false).await?;
                    let expected_manifest = encode_scan_manifest(scan, &serial, scan_state);
                    if manifest.as_deref() != Some(expected_manifest.as_slice()) {
                        return Ok(Ok(PublishOutcome::VolumeNotReady));
                    }
                    if required == NamespaceState::Ready
                        && tr.get(&reconciling_volume, false).await?.is_some()
                    {
                        return Ok(Ok(PublishOutcome::VolumeNotReady));
                    }
                    let volume = volume_key(
                        receipt.volume_id,
                        receipt.volume_scan_id,
                        receipt.namespace,
                        receipt.file_id,
                        receipt.copy_id,
                    );
                    let parent = tr.get(&parent_node, false).await?;
                    let Some(parent) = parent else {
                        return Ok(Ok(PublishOutcome::MissingParent));
                    };
                    let parent = match decode_node(&parent) {
                        Ok(node) => node,
                        Err(error) => return Ok(Err(error)),
                    };
                    if parent.kind != NodeKind::Directory {
                        return Ok(Ok(PublishOutcome::MissingParent));
                    }
                    let file_node_value = encode_node(&Node {
                        kind: NodeKind::File,
                        parent: receipt.parent,
                        name: receipt.name.clone(),
                        attributes: attributes.clone(),
                    });
                    let mut old_external = false;
                    if let Some(node) = tr.get(&file_node, false).await? {
                        let old = match decode_node(&node) {
                            Ok(node) => node,
                            Err(error) => return Ok(Err(error)),
                        };
                        if old.kind != NodeKind::File
                            || old.parent != receipt.parent
                            || old.name != receipt.name
                        {
                            return Ok(Ok(PublishOutcome::NodeConflict));
                        }
                        if old.attributes.is_some() && attributes.is_none() {
                            return Ok(Ok(PublishOutcome::OperationConflict));
                        }
                        old_external = old
                            .attributes
                            .as_ref()
                            .is_some_and(NodeAttributes::is_external);
                    }
                    let previous = tr.get(&head, false).await?;
                    if let Some(bytes) = &previous
                        && let Err(error) = decode_head(bytes)
                    {
                        return Ok(Err(error));
                    }
                    let current = previous.as_ref().map(|b| read_u64(b)).unwrap_or(0);
                    if current != expected {
                        return Ok(Ok(PublishOutcome::VersionConflict));
                    }
                    if let Some(id) = tr.get(&child, false).await? {
                        if id.len() != 16 {
                            return Ok(Err(CatalogError::Corrupt("目录项 inode 长度")));
                        }
                        if id.as_ref() != receipt.file_id.as_bytes() {
                            return Ok(Ok(PublishOutcome::NameConflict));
                        }
                    } else if expected != 0 {
                        return Ok(Ok(PublishOutcome::NameConflict));
                    }
                    if tr.get(&copy, false).await?.is_some() {
                        return Ok(Ok(PublishOutcome::OperationConflict));
                    }
                    let release = match maintenance::release(&tr, receipt.namespace, permit).await {
                        Ok(release) => release,
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    };
                    if let Err(error) = write_attribute_chunks(
                        &tr,
                        receipt.namespace,
                        receipt.file_id,
                        old_external,
                        attributes.as_ref(),
                    )
                    .await
                    {
                        return Ok(Err(error));
                    }
                    release.apply(&tr);
                    tr.set(&child, receipt.file_id.as_bytes());
                    tr.set(&file_node, &file_node_value);
                    let head_value = encode_head(&FileHead {
                        version: next,
                        size: receipt.size,
                        copy_id: receipt.copy_id,
                    });
                    tr.set(&head, &head_value);
                    let copy_value = encode_copy(&receipt, next);
                    tr.set(&copy, &copy_value);
                    let mut volume_value = next.to_be_bytes().to_vec();
                    volume_value.extend_from_slice(&fingerprint);
                    tr.set(&volume, &volume_value);
                    tr.set(&op, &requests::terminal_value(&fingerprint, Some(next)));
                    tr.clear(&intent);
                    Ok(Ok(PublishOutcome::Applied(next)))
                }
            })
            .await?
    }

    pub async fn resolve_child(
        &self,
        namespace: Uuid,
        parent: Uuid,
        name: &[u8],
    ) -> Result<Option<Uuid>> {
        validate_name(name)?;
        let key = child_key(namespace, parent, name);
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
            && let Some(id) = lookup::published_child_id_at(&tr, namespace, parent, name).await?
        {
            return Ok(Some(id));
        }
        let value = tr.get(&key, true).await?;
        value.map(|b| uuid_from(b.as_ref())).transpose()
    }

    pub async fn file_head(&self, namespace: Uuid, file_id: Uuid) -> Result<Option<FileHead>> {
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
            && let Some((_, receipt, _)) =
                lookup::published_file_receipt_at(&tr, namespace, file_id).await?
        {
            return lookup::candidate_head(&receipt).map(Some);
        }
        let value = tr.get(&head_key(namespace, file_id), true).await?;
        value.map(|bytes| decode_head(&bytes)).transpose()
    }

    /// Reclaimed historical descriptions return None; the file's current head
    /// and every current-version description remain intact.
    pub async fn physical_copy(
        &self,
        namespace: Uuid,
        file_id: Uuid,
        copy_id: Uuid,
    ) -> Result<Option<PhysicalCopy>> {
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
            && let Some((_, receipt, _)) =
                lookup::published_file_receipt_at(&tr, namespace, file_id).await?
            && receipt.copy_id == copy_id
        {
            let head = lookup::candidate_head(&receipt)?;
            return Ok(Some(lookup::candidate_copy(&receipt, head.version)));
        }
        let value = tr.get(&copy_key(namespace, file_id, copy_id), true).await?;
        let copy = match value.as_deref().map(decode_stored_copy).transpose()? {
            Some(StoredCopy::Present(copy)) => Some(copy),
            Some(StoredCopy::Reclaimed { version, .. }) => {
                let head = tr.get(&head_key(namespace, file_id), true).await?;
                let head = decode_head(
                    head.as_deref()
                        .ok_or(CatalogError::Corrupt("回收副本的文件 head 缺失"))?,
                )?;
                if head.copy_id == copy_id || head.version <= version {
                    return Err(CatalogError::Corrupt("当前副本或版本被错误回收"));
                }
                None
            }
            None => None,
        };
        if let Some(copy) = &copy {
            lookup::require_serving_copy_at(&tr, namespace, copy).await?;
        }
        Ok(copy)
    }

    pub async fn list_children(
        &self,
        namespace: Uuid,
        parent: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<Child>> {
        let prefix = child_prefix(namespace, parent);
        if let Some(cursor) = after
            && (!cursor.starts_with(&prefix)
                || decode_name(&cursor[prefix.len()..]).is_err()
                || cursor.len() > prefix.len() + 2 * MAX_NAME + 2)
        {
            return Err(CatalogError::Invalid("目录分页游标无效"));
        }
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("分页大小必须在 1..=1000"));
        }
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
        {
            return lookup::list_active_children_at(&tr, namespace, parent, after, limit).await;
        }
        let rows = read_page(&tr, &prefix, after, limit, true).await?;
        let items = rows
            .items
            .into_iter()
            .map(|(key, value)| {
                Ok(Child {
                    name: decode_name(&key[prefix.len()..])?,
                    id: uuid_from(&value)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: rows.next,
        })
    }

    pub async fn copies_on_volume(
        &self,
        volume_id: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<VolumeCopy>> {
        let tr = self.db.create_trx()?;
        if let Some(placement) = control::verified_placement(&tr, volume_id).await?
            && tr
                .get(&namespace_key(placement.namespace), true)
                .await?
                .as_deref()
                == Some(&[NamespaceState::Active as u8][..])
        {
            if !(1..=100).contains(&limit) {
                return Err(CatalogError::Invalid("活动卷副本分页大小必须在 1..=100"));
            }
            let scan = require_volume_ready(&tr, volume_id, None).await?;
            let prefix = volume_prefix(volume_id, scan.id);
            if after.is_some_and(|cursor| {
                !cursor.starts_with(&prefix) || cursor.len() != prefix.len() + 48
            }) {
                return Err(CatalogError::Invalid("卷分页游标无效"));
            }
            return lookup::list_active_volume_copies_at(&tr, volume_id, scan, after, limit).await;
        }
        let scan = require_volume_ready(&tr, volume_id, None).await?;
        let prefix = volume_prefix(volume_id, scan.id);
        if after
            .is_some_and(|cursor| !cursor.starts_with(&prefix) || cursor.len() != prefix.len() + 48)
        {
            return Err(CatalogError::Invalid("卷分页游标无效"));
        }
        let rows = self
            .page_range(&prefix, after, limit, None, Some((volume_id, scan)))
            .await?;
        let items = rows
            .items
            .into_iter()
            .map(|(key, value)| {
                let suffix = &key[prefix.len()..];
                if suffix.len() != 48 || value.len() != 40 {
                    return Err(CatalogError::Corrupt("卷反向索引长度"));
                }
                Ok(VolumeCopy {
                    namespace: uuid_from(&suffix[..16])?,
                    file_id: uuid_from(&suffix[16..32])?,
                    copy_id: uuid_from(&suffix[32..])?,
                    version: read_u64(&value),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: rows.next,
        })
    }

    /// Delete at most `limit` rows of an explicitly retired scan. Keep its
    /// manifest as a tombstone so cleanup and interrupted requests are retryable.
    pub async fn purge_retired_volume_index(
        &self,
        volume_id: Uuid,
        scan: VolumeScan,
        limit: usize,
    ) -> Result<usize> {
        validate_scan(volume_id, scan)?;
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("清理批次必须在 1..=1000"));
        }
        let manifest_key = scan_key(volume_id, scan.id);
        let prefix = volume_prefix(volume_id, scan.id);
        let end = prefix_end(&prefix);
        self.db
            .run(|tr, _| {
                let manifest_key = manifest_key.clone();
                let prefix = prefix.clone();
                let end = end.clone();
                async move {
                    let manifest = tr.get(&manifest_key, false).await?;
                    let Some(manifest) = manifest else {
                        return Ok(Err(CatalogError::Invalid("扫描记录不存在")));
                    };
                    let (saved, _, state) = match decode_scan_manifest(&manifest) {
                        Ok(value) => value,
                        Err(error) => return Ok(Err(error)),
                    };
                    if saved != scan || state != ScanState::Retired {
                        return Ok(Err(CatalogError::Invalid("只能清理已退役扫描的索引")));
                    }
                    // A cancelled scan stays in the pending pointer as a read
                    // barrier, but its retired manifest prevents new imports.
                    if let Some(value) = tr.get(&ready_volume_key(volume_id), false).await? {
                        let (referenced, _) = match decode_ready_volume(&value) {
                            Ok(value) => value,
                            Err(error) => return Ok(Err(error)),
                        };
                        if referenced.id == scan.id {
                            return Ok(Err(CatalogError::Corrupt("退役扫描仍被引用")));
                        }
                    }
                    let range = RangeOption {
                        begin: KeySelector::first_greater_or_equal(prefix),
                        end: KeySelector::first_greater_or_equal(end),
                        limit: Some(limit),
                        ..RangeOption::default()
                    };
                    let rows = tr.get_range(&range, 1, false).await?;
                    let count = rows.len();
                    for row in rows.iter() {
                        tr.clear(row.key());
                    }
                    if count == 0 {
                        tr.clear(&verification::progress_key(volume_id, scan));
                    }
                    Ok(Ok(count))
                }
            })
            .await?
    }

    async fn page_range(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
        namespace: Option<Uuid>,
        volume: Option<(Uuid, VolumeScan)>,
    ) -> Result<Page<(Vec<u8>, Vec<u8>)>> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid("分页大小必须在 1..=1000"));
        }
        if after.is_some_and(|key| !key.starts_with(prefix)) {
            return Err(CatalogError::Invalid("分页游标不属于当前查询"));
        }
        let tr = self.db.create_trx()?;
        if let Some(namespace) = namespace {
            require_namespace_readable(&tr, namespace).await?;
        }
        if let Some((volume, scan)) = volume {
            require_volume_ready(&tr, volume, Some(scan)).await?;
        }
        Ok(read_page(&tr, prefix, after, limit, true).await?)
    }
}

async fn read_page(
    tr: &Transaction,
    prefix: &[u8],
    after: Option<&[u8]>,
    limit: usize,
    snapshot: bool,
) -> std::result::Result<Page<(Vec<u8>, Vec<u8>)>, FdbBindingError> {
    let end = prefix_end(prefix);
    let begin = after.map_or_else(
        || KeySelector::first_greater_or_equal(prefix.to_vec()),
        |key| KeySelector::first_greater_than(key.to_vec()),
    );
    let mut range = RangeOption {
        begin,
        end: KeySelector::first_greater_or_equal(end),
        limit: Some(limit + 1),
        ..RangeOption::default()
    };
    let mut rows = Vec::with_capacity(limit + 1);
    let mut iteration = 1;
    loop {
        let values = tr.get_range(&range, iteration, snapshot).await?;
        for kv in values.iter() {
            rows.push((kv.key().to_vec(), kv.value().to_vec()));
        }
        match range.next_range(&values) {
            Some(next) if rows.len() < limit + 1 => {
                range = next;
                iteration += 1;
            }
            _ => break,
        }
    }
    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|r| r.0.clone())
    } else {
        None
    };
    Ok(Page { items: rows, next })
}

fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(CatalogError::Invalid("文件名为空、过长或包含非法分量"));
    }
    Ok(())
}

fn validate_serial(serial: &str) -> Result<()> {
    if serial.is_empty() || serial.len() > 128 || serial.as_bytes().contains(&0) {
        return Err(CatalogError::Invalid("带库序列号为空、过长或包含 NUL"));
    }
    Ok(())
}

fn key(kind: u8) -> Vec<u8> {
    let mut out = PREFIX.to_vec();
    out.push(kind);
    out.push(b'/');
    out
}

fn append_component(key: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        if *byte == 0 {
            key.extend_from_slice(&[0, 0xff]);
        } else {
            key.push(*byte);
        }
    }
    key.extend_from_slice(&[0, 0]);
}

fn owner_key(serial: &str) -> Vec<u8> {
    let mut k = key(b'O');
    append_component(&mut k, serial.as_bytes());
    k
}
fn namespace_key(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'S');
    k.extend_from_slice(namespace.as_bytes());
    k
}

fn source_revision_key(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'v');
    k.extend_from_slice(namespace.as_bytes());
    k
}

async fn source_revision(tr: &Transaction, namespace: Uuid) -> Result<u64> {
    let bytes = tr
        .get(&source_revision_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if bytes.len() != 8 {
        return Err(CatalogError::Corrupt("扫描修订号长度"));
    }
    Ok(read_u64(&bytes))
}

async fn advance_source_revision(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let next = source_revision(tr, namespace)
        .await?
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("扫描修订号溢出"))?;
    tr.set(&source_revision_key(namespace), &next.to_be_bytes());
    Ok(())
}

fn decode_namespace_state(bytes: &[u8]) -> Result<NamespaceState> {
    match bytes {
        [0] => Ok(NamespaceState::Building),
        [1] => Ok(NamespaceState::Ready),
        [2] => Ok(NamespaceState::Verifying),
        [3] => Ok(NamespaceState::Verified),
        [4] => Ok(NamespaceState::Rejected),
        [5] => Ok(NamespaceState::Discarded),
        [6] => Ok(NamespaceState::Sealed),
        [7] => Ok(NamespaceState::Active),
        _ => Err(CatalogError::Corrupt("namespace state")),
    }
}

async fn require_namespace_readable(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let state = tr.get(&namespace_key(namespace), true).await?;
    match state.as_deref().map(decode_namespace_state).transpose()? {
        Some(NamespaceState::Ready | NamespaceState::Sealed | NamespaceState::Active) => Ok(()),
        _ => Err(CatalogError::Incomplete),
    }
}
fn ready_volume_key(volume: Uuid) -> Vec<u8> {
    let mut k = key(b'M');
    k.extend_from_slice(volume.as_bytes());
    k
}
fn reconcile_volume_key(volume: Uuid) -> Vec<u8> {
    let mut k = key(b'Q');
    k.extend_from_slice(volume.as_bytes());
    k
}

fn unavailable_volume_prefix(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'W');
    k.extend_from_slice(namespace.as_bytes());
    k
}

fn unavailable_volume_key(namespace: Uuid, volume: Uuid) -> Vec<u8> {
    let mut k = unavailable_volume_prefix(namespace);
    k.extend_from_slice(volume.as_bytes());
    k
}

fn scan_key(volume: Uuid, scan_id: Uuid) -> Vec<u8> {
    let mut k = key(b'G');
    k.extend_from_slice(volume.as_bytes());
    k.extend_from_slice(scan_id.as_bytes());
    k
}

fn source_key(volume: Uuid, scan_id: Uuid) -> Vec<u8> {
    let mut k = key(b'K');
    k.extend_from_slice(volume.as_bytes());
    k.extend_from_slice(scan_id.as_bytes());
    k
}

fn validate_scan(volume: Uuid, scan: VolumeScan) -> Result<()> {
    if volume.is_nil() || scan.id.is_nil() || scan.generation == 0 {
        return Err(CatalogError::Invalid("卷 UUID、扫描 ID 和代数必须有效"));
    }
    Ok(())
}

async fn require_volume_ready(
    tr: &Transaction,
    volume: Uuid,
    expected: Option<VolumeScan>,
) -> Result<VolumeScan> {
    if tr.get(&reconcile_volume_key(volume), true).await?.is_some() {
        return Err(CatalogError::Incomplete);
    }
    let ready = tr.get(&ready_volume_key(volume), true).await?;
    let Some(ready) = ready else {
        return Err(CatalogError::Incomplete);
    };
    let (scan, _) = decode_ready_volume(&ready)?;
    if expected.is_some_and(|expected| expected != scan) {
        return Err(CatalogError::Incomplete);
    }
    if !index_evidence::managed_source_ready(tr, volume, scan).await? {
        return Err(CatalogError::Incomplete);
    }
    Ok(scan)
}

fn encode_ready_volume(scan: VolumeScan, serial: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(26 + serial.len());
    value.extend_from_slice(&scan.generation.to_be_bytes());
    value.extend_from_slice(scan.id.as_bytes());
    value.extend_from_slice(&(serial.len() as u16).to_be_bytes());
    value.extend_from_slice(serial.as_bytes());
    value
}

fn decode_ready_volume(bytes: &[u8]) -> Result<(VolumeScan, String)> {
    if bytes.len() < 26 {
        return Err(CatalogError::Corrupt("卷扫描记录长度"));
    }
    let serial_len = u16::from_be_bytes([bytes[24], bytes[25]]) as usize;
    if serial_len == 0 || serial_len > 128 || bytes.len() != 26 + serial_len {
        return Err(CatalogError::Corrupt("卷扫描序列号长度"));
    }
    let serial =
        std::str::from_utf8(&bytes[26..]).map_err(|_| CatalogError::Corrupt("卷扫描序列号编码"))?;
    validate_serial(serial).map_err(|_| CatalogError::Corrupt("卷扫描序列号"))?;
    let scan = VolumeScan {
        generation: read_u64(bytes),
        id: uuid_from(&bytes[8..24])?,
    };
    if scan.generation == 0 || scan.id.is_nil() {
        return Err(CatalogError::Corrupt("卷扫描代数或 ID"));
    }
    Ok((scan, serial.to_string()))
}

fn encode_scan_manifest(scan: VolumeScan, serial: &str, state: ScanState) -> Vec<u8> {
    let mut value = encode_ready_volume(scan, serial);
    value.push(state as u8);
    value
}

fn decode_scan_manifest(bytes: &[u8]) -> Result<(VolumeScan, String, ScanState)> {
    let Some((state, value)) = bytes.split_last() else {
        return Err(CatalogError::Corrupt("扫描状态记录长度"));
    };
    let state = match state {
        0 => ScanState::Building,
        1 => ScanState::Ready,
        2 => ScanState::Retired,
        3 => ScanState::Verifying,
        4 => ScanState::Verified,
        5 => ScanState::Rejected,
        _ => return Err(CatalogError::Corrupt("扫描状态")),
    };
    let (scan, serial) = decode_ready_volume(value)?;
    Ok((scan, serial, state))
}
fn op_key(namespace: Uuid, epoch: u64, id: Uuid) -> Vec<u8> {
    let mut k = key(b'P');
    k.extend_from_slice(namespace.as_bytes());
    k.extend_from_slice(&epoch.to_be_bytes());
    k.extend_from_slice(id.as_bytes());
    k
}
fn child_prefix(ns: Uuid, parent: Uuid) -> Vec<u8> {
    let mut k = key(b'C');
    k.extend_from_slice(ns.as_bytes());
    k.extend_from_slice(parent.as_bytes());
    k
}
fn child_key(ns: Uuid, parent: Uuid, name: &[u8]) -> Vec<u8> {
    let mut k = child_prefix(ns, parent);
    append_component(&mut k, name);
    k
}
fn head_key(ns: Uuid, id: Uuid) -> Vec<u8> {
    let mut k = key(b'H');
    k.extend_from_slice(ns.as_bytes());
    k.extend_from_slice(id.as_bytes());
    k
}
fn node_key(ns: Uuid, id: Uuid) -> Vec<u8> {
    let mut k = key(b'N');
    k.extend_from_slice(ns.as_bytes());
    k.extend_from_slice(id.as_bytes());
    k
}

fn attribute_chunk_prefix(ns: Uuid, id: Uuid) -> Vec<u8> {
    let mut k = key(b'm');
    k.extend_from_slice(ns.as_bytes());
    k.extend_from_slice(id.as_bytes());
    k
}

async fn write_attribute_chunks(
    tr: &Transaction,
    ns: Uuid,
    id: Uuid,
    old_external: bool,
    attributes: Option<&NodeAttributes>,
) -> Result<()> {
    // Only retire old per-inode storage. Immutable M objects are retained;
    // the reference switch must never delete a version still held by a reader.
    if old_external {
        let prefix = attribute_chunk_prefix(ns, id);
        tr.clear_range(&prefix, &prefix_end(&prefix));
    }
    if let Some(manifest) = attributes.and_then(NodeAttributes::manifest) {
        attribute_objects::require_complete(tr, ns, manifest).await?;
    }
    Ok(())
}

#[cfg(test)]
async fn hydrate_node(
    tr: &Transaction,
    ns: Uuid,
    _id: Uuid,
    mut node: Node,
    snapshot: bool,
) -> Result<Node> {
    let Some(manifest) = node
        .attributes
        .as_ref()
        .and_then(|attrs| attrs.is_external().then(|| attrs.manifest()).flatten())
    else {
        return Ok(node);
    };
    attribute_objects::require_complete(tr, ns, manifest).await?;
    let mut prefix = key(b'M');
    prefix.extend_from_slice(ns.as_bytes());
    prefix.extend_from_slice(&manifest.digest);
    prefix.push(1);
    let page = read_page(tr, &prefix, None, manifest.chunks as usize, snapshot).await?;
    if page.next.is_some() || page.items.len() != manifest.chunks as usize {
        return Err(CatalogError::Corrupt("LTFS 属性分块数量不符"));
    }
    let mut bytes = Vec::with_capacity(manifest.len as usize);
    for (index, (key, value)) in page.items.into_iter().enumerate() {
        if key != attribute_objects::chunk_key(ns, manifest, index as u16)
            || value.is_empty()
            || value.len() > CHUNK_SIZE
            || (index + 1 < manifest.chunks as usize && value.len() != CHUNK_SIZE)
        {
            return Err(CatalogError::Corrupt("LTFS 属性分块顺序或长度"));
        }
        bytes.extend_from_slice(&value);
    }
    if bytes.len() != manifest.len as usize {
        return Err(CatalogError::Corrupt("LTFS 属性分块总长度"));
    }
    let attributes = NodeAttributes::decode(&bytes)?;
    if attributes.digest() != manifest.digest {
        return Err(CatalogError::Corrupt("LTFS 属性分块摘要"));
    }
    node.attributes = Some(attributes);
    Ok(node)
}

fn encode_node(node: &Node) -> Vec<u8> {
    let attribute_len = node.attributes.as_ref().map_or(0, |attributes| {
        attributes.inline_bytes().map_or(38, |bytes| bytes.len())
    });
    let mut value = Vec::with_capacity(20 + node.name.len() + attribute_len);
    value.push(match node.kind {
        NodeKind::Directory => 0,
        NodeKind::File => 1,
    });
    value.extend_from_slice(node.parent.as_bytes());
    value.extend_from_slice(&(node.name.len() as u16).to_be_bytes());
    value.extend_from_slice(&node.name);
    match &node.attributes {
        None => value.push(0),
        Some(attributes) => {
            if let Some(bytes) = attributes.inline_bytes() {
                value.push(1);
                value.extend_from_slice(bytes);
            } else {
                let manifest = attributes.manifest().expect("属性必须有内联值或分块清单");
                value.push(3);
                value.extend_from_slice(&manifest.digest);
                value.extend_from_slice(&manifest.len.to_be_bytes());
                value.extend_from_slice(&manifest.chunks.to_be_bytes());
            }
        }
    }
    value
}

fn decode_node(value: &[u8]) -> Result<Node> {
    if value.len() < 20 {
        return Err(CatalogError::Corrupt("node 长度"));
    }
    let kind = match value[0] {
        0 => NodeKind::Directory,
        1 => NodeKind::File,
        _ => return Err(CatalogError::Corrupt("node 类型")),
    };
    let name_len = u16::from_be_bytes([value[17], value[18]]) as usize;
    if name_len > MAX_NAME || value.len() < 20 + name_len {
        return Err(CatalogError::Corrupt("node 名称长度"));
    }
    let name = value[19..19 + name_len].to_vec();
    if !name.is_empty() {
        validate_name(&name).map_err(|_| CatalogError::Corrupt("node 名称"))?;
    }
    let attributes = match value[19 + name_len] {
        0 if value.len() == 20 + name_len => None,
        1 if value.len() - 20 - name_len <= INLINE_LIMIT => {
            Some(NodeAttributes::decode(&value[20 + name_len..])?)
        }
        3 if value.len() == 20 + name_len + 38 => {
            let start = 20 + name_len;
            Some(NodeAttributes::from_manifest(AttributeManifest {
                digest: value[start..start + 32].try_into().unwrap(),
                len: u32::from_be_bytes(value[start + 32..start + 36].try_into().unwrap()),
                chunks: u16::from_be_bytes(value[start + 36..start + 38].try_into().unwrap()),
            })?)
        }
        _ => return Err(CatalogError::Corrupt("node 属性标志或尾部数据")),
    };
    if kind == NodeKind::Directory && attributes.as_ref().is_some_and(|a| a.symlink().is_some()) {
        return Err(CatalogError::Corrupt("目录不能是符号链接"));
    }
    Ok(Node {
        kind,
        parent: uuid_from(&value[1..17])?,
        name,
        attributes,
    })
}

fn encode_head(head: &FileHead) -> Vec<u8> {
    let mut value = Vec::with_capacity(32);
    value.extend_from_slice(&head.version.to_be_bytes());
    value.extend_from_slice(&head.size.to_be_bytes());
    value.extend_from_slice(head.copy_id.as_bytes());
    value
}

fn decode_head(bytes: &[u8]) -> Result<FileHead> {
    if bytes.len() != 32 || read_u64(bytes) == 0 {
        return Err(CatalogError::Corrupt("文件 head 长度或版本"));
    }
    let copy_id = uuid_from(&bytes[16..])?;
    if copy_id.is_nil() {
        return Err(CatalogError::Corrupt("文件 head 副本 ID"));
    }
    Ok(FileHead {
        version: read_u64(bytes),
        size: read_u64(&bytes[8..]),
        copy_id,
    })
}

fn decode_copy(bytes: &[u8]) -> Result<PhysicalCopy> {
    let digest_tag = *bytes
        .get(48)
        .ok_or(CatalogError::Corrupt("物理副本摘要算法"))?;
    let extended = digest_tag & 0x80 != 0;
    let digest_len = match digest_tag & 0x7f {
        0 => 0,
        1 => 32,
        2 => 16,
        _ => return Err(CatalogError::Corrupt("物理副本摘要算法")),
    };
    let epoch_offset = 49 + digest_len;
    let uid_offset = epoch_offset + 8;
    let size_offset = uid_offset + 8;
    let base_end = size_offset + 8;
    if bytes.len() < base_end
        || read_u64(bytes) == 0
        || read_u64(&bytes[epoch_offset..]) == 0
        || read_u64(&bytes[uid_offset..]) == 0
    {
        return Err(CatalogError::Corrupt("物理副本长度或版本"));
    }
    let volume_id = uuid_from(&bytes[8..24])?;
    let volume_generation = read_u64(&bytes[24..32]);
    let volume_scan_id = uuid_from(&bytes[32..48])?;
    validate_scan(
        volume_id,
        VolumeScan {
            generation: volume_generation,
            id: volume_scan_id,
        },
    )
    .map_err(|_| CatalogError::Corrupt("物理副本卷或扫描 ID"))?;
    let ltfs_file_uid = read_u64(&bytes[uid_offset..]);
    let media = if extended {
        Some(decode_media(&bytes[base_end..], ltfs_file_uid)?)
    } else if bytes.len() == base_end {
        None
    } else {
        return Err(CatalogError::Corrupt("旧物理副本含多余尾部"));
    };
    if media.as_ref().is_some_and(|media| media.native_symlink)
        && (read_u64(&bytes[size_offset..]) != 0 || digest_tag & 0x7f != 0)
    {
        return Err(CatalogError::Corrupt("原生符号链接不能包含数据副本"));
    }
    Ok(PhysicalCopy {
        version: read_u64(bytes),
        request_epoch: read_u64(&bytes[epoch_offset..]),
        volume_id,
        volume_generation,
        volume_scan_id,
        ltfs_file_uid,
        size: read_u64(&bytes[size_offset..]),
        content_digest: ContentDigest::decode_compact(digest_tag & 0x7f, &bytes[49..epoch_offset])?,
        media,
    })
}

fn validate_media(media: &MediaProvenance, data_uid: u64) -> Result<()> {
    if media.logical_file_uid == 0
        || media.physical_path.len() > u16::MAX as usize
        || import_spool::validate_path(&media.physical_path).is_err()
        || (media.ee_alias
            && (media.logical_file_uid == data_uid
                || !media.physical_path.starts_with(".LTFSEE_DATA/")))
        || (!media.ee_alias && media.logical_file_uid != data_uid)
        || (media.ee_alias && media.native_symlink)
    {
        return Err(CatalogError::Invalid("物理副本介质来源无效"));
    }
    Ok(())
}

fn decode_media(bytes: &[u8], data_uid: u64) -> Result<MediaProvenance> {
    if bytes.len() < 28 || !matches!((bytes[0], bytes[1]), (1, 0 | 1) | (2, 2)) {
        return Err(CatalogError::Corrupt("物理副本介质来源版本或长度"));
    }
    let length = u16::from_be_bytes(bytes[26..28].try_into().unwrap()) as usize;
    if bytes.len() != 28 + length {
        return Err(CatalogError::Corrupt("物理副本介质路径长度"));
    }
    let media = MediaProvenance {
        version: (read_u64(&bytes[2..10]), read_u64(&bytes[10..18])),
        logical_file_uid: read_u64(&bytes[18..26]),
        physical_path: std::str::from_utf8(&bytes[28..])
            .map_err(|_| CatalogError::Corrupt("物理副本介质路径 UTF-8"))?
            .to_owned(),
        ee_alias: bytes[1] == 1,
        native_symlink: bytes[1] == 2,
    };
    validate_media(&media, data_uid).map_err(|_| CatalogError::Corrupt("物理副本介质来源字段"))?;
    Ok(media)
}

fn encode_reclaimed_copy(version: u64, request_epoch: u64) -> Vec<u8> {
    let mut value = vec![0xff];
    value.extend_from_slice(&version.to_be_bytes());
    value.extend_from_slice(&request_epoch.to_be_bytes());
    value
}

fn decode_stored_copy(bytes: &[u8]) -> Result<StoredCopy> {
    if bytes.len() == 17
        && bytes[0] == 0xff
        && read_u64(&bytes[1..]) != 0
        && read_u64(&bytes[9..]) != 0
    {
        return Ok(StoredCopy::Reclaimed {
            version: read_u64(&bytes[1..]),
            request_epoch: read_u64(&bytes[9..]),
        });
    }
    decode_copy(bytes).map(StoredCopy::Present)
}

fn copy_prefix(ns: Uuid) -> Vec<u8> {
    let mut k = key(b'R');
    k.extend_from_slice(ns.as_bytes());
    k
}
fn copy_key(ns: Uuid, id: Uuid, copy: Uuid) -> Vec<u8> {
    let mut k = copy_prefix(ns);
    k.extend_from_slice(id.as_bytes());
    k.extend_from_slice(copy.as_bytes());
    k
}
fn volume_prefix(vol: Uuid, scan_id: Uuid) -> Vec<u8> {
    let mut k = key(b'V');
    k.extend_from_slice(vol.as_bytes());
    k.extend_from_slice(scan_id.as_bytes());
    k
}
fn volume_key(vol: Uuid, scan_id: Uuid, ns: Uuid, id: Uuid, copy: Uuid) -> Vec<u8> {
    let mut k = volume_prefix(vol, scan_id);
    k.extend_from_slice(ns.as_bytes());
    k.extend_from_slice(id.as_bytes());
    k.extend_from_slice(copy.as_bytes());
    k
}

fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while end.last() == Some(&0xff) {
        end.pop();
    }
    *end.last_mut()
        .expect("all catalog prefixes begin with ASCII") += 1;
    end
}

fn read_u64(value: &[u8]) -> u64 {
    value
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0)
}
fn uuid_from(bytes: &[u8]) -> Result<Uuid> {
    Uuid::from_slice(bytes).map_err(|_| CatalogError::Corrupt("UUID 长度"))
}

fn decode_name(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0 {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(0xff) => {
                out.push(0);
                i += 2;
            }
            Some(0) if i + 2 == bytes.len() => return Ok(out),
            _ => return Err(CatalogError::Corrupt("目录项编码")),
        }
    }
    Err(CatalogError::Corrupt("目录项缺少终止符"))
}

fn encode_copy(receipt: &CommitReceipt, version: u64) -> Vec<u8> {
    encode_physical_copy(&PhysicalCopy {
        version,
        request_epoch: receipt.request_epoch,
        volume_id: receipt.volume_id,
        volume_generation: receipt.volume_generation,
        volume_scan_id: receipt.volume_scan_id,
        ltfs_file_uid: receipt.ltfs_file_uid,
        size: receipt.size,
        content_digest: receipt.content_digest,
        media: None,
    })
    .expect("legacy media commit has no variable-length provenance")
}

fn encode_physical_copy(copy: &PhysicalCopy) -> Result<Vec<u8>> {
    if copy.media.is_some()
        && (copy.version == 0
            || copy.request_epoch == 0
            || copy.ltfs_file_uid == 0
            || validate_scan(
                copy.volume_id,
                VolumeScan {
                    generation: copy.volume_generation,
                    id: copy.volume_scan_id,
                },
            )
            .is_err())
    {
        return Err(CatalogError::Invalid("导入物理副本基础身份无效"));
    }
    let mut value = Vec::with_capacity(
        73 + copy.content_digest.compact_len()
            + copy
                .media
                .as_ref()
                .map_or(0, |media| 28 + media.physical_path.len()),
    );
    value.extend_from_slice(&copy.version.to_be_bytes());
    value.extend_from_slice(copy.volume_id.as_bytes());
    value.extend_from_slice(&copy.volume_generation.to_be_bytes());
    value.extend_from_slice(copy.volume_scan_id.as_bytes());
    value.push(copy.content_digest.tag() | if copy.media.is_some() { 0x80 } else { 0 });
    value.extend_from_slice(&copy.content_digest.payload()[..copy.content_digest.compact_len()]);
    value.extend_from_slice(&copy.request_epoch.to_be_bytes());
    value.extend_from_slice(&copy.ltfs_file_uid.to_be_bytes());
    value.extend_from_slice(&copy.size.to_be_bytes());
    if let Some(media) = &copy.media {
        validate_media(media, copy.ltfs_file_uid)?;
        if media.native_symlink && (copy.size != 0 || copy.content_digest != ContentDigest::Unknown)
        {
            return Err(CatalogError::Invalid("原生符号链接不能包含数据副本"));
        }
        value.push(if media.native_symlink { 2 } else { 1 });
        value.push(if media.native_symlink {
            2
        } else {
            u8::from(media.ee_alias)
        });
        value.extend_from_slice(&media.version.0.to_be_bytes());
        value.extend_from_slice(&media.version.1.to_be_bytes());
        value.extend_from_slice(&media.logical_file_uid.to_be_bytes());
        value.extend_from_slice(&(media.physical_path.len() as u16).to_be_bytes());
        value.extend_from_slice(media.physical_path.as_bytes());
    }
    Ok(value)
}

/// Stable copy identity. A fresh epoch/operation/file version cannot reuse an
/// expired request's copy ID after its per-copy tombstone has been collected.
pub fn derive_copy_id(r: &CommitReceipt) -> Uuid {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/copy-id/1");
    hash.update(r.namespace.as_bytes());
    hash.update(r.file_id.as_bytes());
    hash.update(r.request_epoch.to_be_bytes());
    hash.update(r.operation_id.as_bytes());
    hash.update(r.expected_version.unwrap_or(0).to_be_bytes());
    let digest = hash.finalize();
    Uuid::from_bytes(digest[..16].try_into().expect("SHA-256 prefix"))
}

fn validate_receipt(receipt: &CommitReceipt) -> Result<()> {
    validate_name(&receipt.name)?;
    validate_serial(&receipt.library_serial)?;
    validate_scan(
        receipt.volume_id,
        VolumeScan {
            generation: receipt.volume_generation,
            id: receipt.volume_scan_id,
        },
    )?;
    if receipt.namespace.is_nil()
        || receipt.file_id.is_nil()
        || receipt.operation_id.is_nil()
        || receipt.copy_id.is_nil()
        || receipt.pool_id.is_some_and(|id| id.is_nil())
    {
        return Err(CatalogError::Invalid(
            "namespace、文件、操作和副本 ID 不能为 nil",
        ));
    }
    if receipt.owner_epoch == 0
        || receipt.volume_generation == 0
        || receipt.request_epoch == 0
        || receipt.ltfs_file_uid == 0
    {
        return Err(CatalogError::Invalid(
            "owner、请求代次、卷代数和 LTFS fileuid 必须大于零",
        ));
    }
    receipt
        .expected_version
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(CatalogError::Invalid("文件版本溢出"))?;
    Ok(())
}

fn receipt_fingerprint(r: &CommitReceipt) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(r.namespace.as_bytes());
    hash.update(r.request_epoch.to_be_bytes());
    hash.update(r.parent.as_bytes());
    hash.update((r.name.len() as u32).to_be_bytes());
    hash.update(&r.name);
    hash.update(r.file_id.as_bytes());
    hash.update(r.expected_version.unwrap_or(0).to_be_bytes());
    hash.update(r.copy_id.as_bytes());
    hash.update((r.library_serial.len() as u32).to_be_bytes());
    hash.update(r.library_serial.as_bytes());
    match r.pool_id {
        Some(id) => {
            hash.update([1]);
            hash.update(id.as_bytes());
        }
        None => hash.update([0]),
    }
    // The owner epoch is an execution fence, not part of the logical operation.
    // A new owner must be able to recognize a previously committed operation.
    hash.update(r.volume_id.as_bytes());
    hash.update(r.volume_generation.to_be_bytes());
    hash.update(r.volume_scan_id.as_bytes());
    hash.update(r.ltfs_file_uid.to_be_bytes());
    hash.update(r.size.to_be_bytes());
    hash.update([r.content_digest.tag()]);
    hash.update(r.content_digest.payload());
    match r.metadata_digest {
        Some(digest) => {
            hash.update([1]);
            hash.update(digest);
        }
        None => hash.update([0]),
    }
    hash.finalize().to_vec()
}

#[cfg(test)]
fn test_receipt(mut receipt: CommitReceipt) -> CommitReceipt {
    receipt.copy_id = derive_copy_id(&receipt);
    receipt
}

#[cfg(test)]
async fn test_publish(catalog: &ManagedCatalog, receipt: &CommitReceipt) -> Result<PublishOutcome> {
    match catalog.register_request(receipt).await {
        Ok(_) | Err(CatalogError::Incomplete) => {}
        Err(error) => return Err(error),
    }
    catalog.publish(receipt).await
}

#[cfg(test)]
async fn test_publish_imported(
    catalog: &ManagedCatalog,
    receipt: &CommitReceipt,
) -> Result<PublishOutcome> {
    match catalog.register_request(receipt).await {
        Ok(_) | Err(CatalogError::Incomplete) => {}
        Err(error) => return Err(error),
    }
    catalog.publish_imported(receipt).await
}

// Legacy storage fixtures intentionally have no managed placement. Inspect their
// scan-qualified records for GC/recovery assertions, while also proving that the
// public serving API rejects them. Never use this helper in managed/owner tests.
#[cfg(test)]
impl ManagedCatalog {
    async fn legacy_copy_record_for_test(
        &self,
        namespace: Uuid,
        file_id: Uuid,
        copy_id: Uuid,
    ) -> Result<Option<PhysicalCopy>> {
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        let value = tr.get(&copy_key(namespace, file_id, copy_id), true).await?;
        let Some(StoredCopy::Present(copy)) =
            value.as_deref().map(decode_stored_copy).transpose()?
        else {
            // Keep the public reclaimed-head corruption checks exercised.
            return self.physical_copy(namespace, file_id, copy_id).await;
        };
        assert!(
            control::verified_placement(&tr, copy.volume_id)
                .await?
                .is_none()
        );
        let serving = self.physical_copy(namespace, file_id, copy_id).await;
        assert!(matches!(
            serving,
            Err(CatalogError::Corrupt("当前副本缺少卷放置") | CatalogError::Incomplete)
        ));
        require_volume_ready(
            &tr,
            copy.volume_id,
            Some(VolumeScan {
                generation: copy.volume_generation,
                id: copy.volume_scan_id,
            }),
        )
        .await?;
        assert!(matches!(
            serving,
            Err(CatalogError::Corrupt("当前副本缺少卷放置"))
        ));
        Ok(Some(copy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_encoding_preserves_order_and_nul() {
        let names = [
            b"a".as_slice(),
            b"a\0".as_slice(),
            b"a\x01".as_slice(),
            b"b".as_slice(),
        ];
        let keys: Vec<_> = names
            .iter()
            .map(|name| {
                let mut key = Vec::new();
                append_component(&mut key, name);
                key
            })
            .collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        for (name, key) in names.iter().zip(keys.iter()) {
            assert_eq!(&decode_name(key).unwrap(), name);
        }
        assert!(validate_name(b"a\0b").is_err());
    }

    #[test]
    fn prefix_end_handles_ff_uuid_suffix() {
        let prefix = child_prefix(Uuid::nil(), Uuid::from_bytes([0xff; 16]));
        let end = prefix_end(&prefix);
        assert!(prefix < end);
        assert!(child_key(Uuid::nil(), Uuid::from_bytes([0xff; 16]), b"z") < end);
    }

    #[test]
    fn fingerprint_changes_with_receipt_fields() {
        let mut r = test_receipt(CommitReceipt {
            request_epoch: 1,
            namespace: Uuid::nil(),
            parent: Uuid::nil(),
            name: b"x".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::new_v4(),
            library_serial: "serial".into(),
            pool_id: None,
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 1,
            size: 1,
            content_digest: ContentDigest::Sha256([0; 32]),
            metadata_digest: None,
        });
        let first = receipt_fingerprint(&r);
        r.owner_epoch += 1;
        assert_eq!(first, receipt_fingerprint(&r));
        r.volume_generation = 2;
        assert_ne!(first, receipt_fingerprint(&r));
        r.volume_generation = 1;
        r.volume_scan_id = Uuid::new_v4();
        assert_ne!(first, receipt_fingerprint(&r));
        let before_pool = receipt_fingerprint(&r);
        r.pool_id = Some(Uuid::new_v4());
        assert_ne!(before_pool, receipt_fingerprint(&r));
        let before_uid = receipt_fingerprint(&r);
        r.ltfs_file_uid = u64::MAX;
        assert_ne!(before_uid, receipt_fingerprint(&r));
        let before_attributes = receipt_fingerprint(&r);
        r.metadata_digest = Some([9; 32]);
        assert_ne!(before_attributes, receipt_fingerprint(&r));
    }

    #[test]
    fn physical_copy_preserves_full_ltfs_uid_and_rejects_missing_uid() {
        let receipt = test_receipt(CommitReceipt {
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            name: b"file".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::nil(),
            library_serial: "serial".into(),
            pool_id: None,
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: u64::MAX,
            size: u64::MAX - 1,
            content_digest: ContentDigest::Sha256([7; 32]),
            metadata_digest: None,
        });
        let bytes = encode_copy(&receipt, 1);
        assert_eq!(decode_copy(&bytes).unwrap().ltfs_file_uid, u64::MAX);
        assert_eq!(decode_copy(&bytes).unwrap().size, u64::MAX - 1);
        assert!(decode_copy(&bytes[..bytes.len() - 1]).is_err());
        let mut missing = bytes;
        missing[89..97].fill(0);
        assert!(decode_copy(&missing).is_err());
        let mut invalid = receipt;
        invalid.ltfs_file_uid = 0;
        assert!(validate_receipt(&invalid).is_err());
    }

    #[test]
    fn physical_copy_distinguishes_unknown_md5_and_sha256() {
        let mut receipt = test_receipt(CommitReceipt {
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            name: b"file".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::nil(),
            library_serial: "serial".into(),
            pool_id: None,
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 1,
            size: 1,
            content_digest: ContentDigest::Unknown,
            metadata_digest: None,
        });
        let unknown_fingerprint = receipt_fingerprint(&receipt);
        assert_eq!(encode_copy(&receipt, 1).len(), 73);
        assert_eq!(
            decode_copy(&encode_copy(&receipt, 1))
                .unwrap()
                .content_digest,
            ContentDigest::Unknown
        );
        receipt.content_digest = ContentDigest::Md5([7; 16]);
        assert_ne!(receipt_fingerprint(&receipt), unknown_fingerprint);
        let md5 = encode_copy(&receipt, 1);
        assert_eq!(md5.len(), 89);
        assert_eq!(
            decode_copy(&md5).unwrap().content_digest,
            receipt.content_digest
        );
        let mut invalid = md5.clone();
        invalid.push(1);
        assert!(decode_copy(&invalid).is_err());
        invalid = md5;
        invalid[48] = 3;
        assert!(decode_copy(&invalid).is_err());
        receipt.content_digest = ContentDigest::Sha256([0; 32]);
        assert_eq!(encode_copy(&receipt, 1).len(), 105);
        assert_eq!(
            decode_copy(&encode_copy(&receipt, 1))
                .unwrap()
                .content_digest,
            receipt.content_digest
        );
        assert_ne!(receipt_fingerprint(&receipt), unknown_fingerprint);
    }

    #[test]
    fn native_symlink_copy_codec_never_accepts_payload_or_alias_flags() {
        let mut copy = PhysicalCopy {
            version: 1,
            request_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 7,
            size: 0,
            content_digest: ContentDigest::Unknown,
            media: Some(MediaProvenance {
                version: (0, 0),
                logical_file_uid: 7,
                physical_path: "link".into(),
                ee_alias: false,
                native_symlink: true,
            }),
        };
        let bytes = encode_physical_copy(&copy).unwrap();
        assert_eq!(decode_copy(&bytes).unwrap(), copy);
        let mut corrupt = bytes.clone();
        corrupt[72] = 1;
        assert!(decode_copy(&corrupt).is_err());
        corrupt = bytes;
        corrupt[73] = 1; // Version 1 cannot encode a native link.
        assert!(decode_copy(&corrupt).is_err());
        copy.size = 1;
        assert!(encode_physical_copy(&copy).is_err());
        copy.size = 0;
        copy.content_digest = ContentDigest::Sha256([1; 32]);
        assert!(encode_physical_copy(&copy).is_err());
        copy.content_digest = ContentDigest::Unknown;
        copy.media.as_mut().unwrap().ee_alias = true;
        assert!(encode_physical_copy(&copy).is_err());
    }

    #[test]
    fn imported_copy_retains_media_version_and_ee_data_path() {
        let receipt = test_receipt(CommitReceipt {
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            name: b"alias".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::nil(),
            library_serial: "serial".into(),
            pool_id: None,
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 3,
            size: 1_048_576,
            content_digest: ContentDigest::Sha256([7; 32]),
            metadata_digest: None,
        });
        let legacy = encode_copy(&receipt, 1);
        assert!(decode_copy(&legacy).unwrap().media.is_none());
        let mut copy = decode_copy(&legacy).unwrap();
        copy.media = Some(MediaProvenance {
            version: (u64::MAX, u64::MAX),
            logical_file_uid: 33,
            physical_path: ".LTFSEE_DATA/object".into(),
            ee_alias: true,
            native_symlink: false,
        });
        let encoded = encode_physical_copy(&copy).unwrap();
        assert_eq!(decode_copy(&encoded).unwrap(), copy);
        for end in legacy.len()..encoded.len() {
            assert!(decode_copy(&encoded[..end]).is_err());
        }
        let mut bad = encoded.clone();
        bad[legacy.len()] = 2;
        assert!(decode_copy(&bad).is_err());
        bad = encoded.clone();
        bad[legacy.len() + 1] = 2;
        assert!(decode_copy(&bad).is_err());
        bad = encoded;
        bad[legacy.len() + 26] = 0;
        bad[legacy.len() + 27] = 1;
        assert!(decode_copy(&bad).is_err());

        let mut invalid = copy;
        invalid.media.as_mut().unwrap().physical_path = ".LTFSEE_DATA/../escape".into();
        assert!(encode_physical_copy(&invalid).is_err());
        invalid.media.as_mut().unwrap().physical_path = "dir/object".into();
        assert!(encode_physical_copy(&invalid).is_err());
        invalid.media.as_mut().unwrap().ee_alias = false;
        assert!(encode_physical_copy(&invalid).is_err());

        invalid.ltfs_file_uid = 33;
        invalid.media.as_mut().unwrap().logical_file_uid = 33;
        invalid.media.as_mut().unwrap().physical_path = "dir/file".into();
        invalid.media.as_mut().unwrap().version = (0, 0);
        let direct = encode_physical_copy(&invalid).unwrap();
        assert_eq!(decode_copy(&direct).unwrap(), invalid);
        invalid.ltfs_file_uid = 0;
        assert!(encode_physical_copy(&invalid).is_err());
    }

    #[test]
    fn ltfs_hash_selection_rejects_malformed_or_ambiguous_evidence() {
        use crate::ltfs::index::{XATTR_MD5, XATTR_SHA256, Xattr};
        let attr = |key: &str, value: &str, base64| Xattr {
            key: key.into(),
            value: value.into(),
            base64,
        };
        let md5 = attr(XATTR_MD5, &"12".repeat(16), false);
        let sha = attr(XATTR_SHA256, &"34".repeat(32), false);
        assert_eq!(
            ContentDigest::from_ltfs_xattrs(&[]).unwrap(),
            ContentDigest::Unknown
        );
        assert_eq!(
            ContentDigest::from_ltfs_xattrs(std::slice::from_ref(&md5)).unwrap(),
            ContentDigest::Md5([0x12; 16])
        );
        assert_eq!(
            ContentDigest::from_ltfs_xattrs(&[md5.clone(), sha.clone()]).unwrap(),
            ContentDigest::Sha256([0x34; 32])
        );
        for invalid in [
            vec![md5.clone(), md5],
            vec![sha.clone(), sha],
            vec![attr(XATTR_MD5, "a", false)],
            vec![attr(XATTR_SHA256, &"zz".repeat(32), false)],
            vec![attr(XATTR_MD5, &"12".repeat(16), true)],
        ] {
            assert!(ContentDigest::from_ltfs_xattrs(&invalid).is_err());
        }
    }

    #[test]
    fn node_encoding_roundtrips_directory_and_file() {
        for kind in [NodeKind::Directory, NodeKind::File] {
            let node = Node {
                kind,
                parent: Uuid::new_v4(),
                name: b"subdir".to_vec(),
                attributes: None,
            };
            assert_eq!(decode_node(&encode_node(&node)).unwrap(), node);
        }
        assert!(decode_node(&[0, 1]).is_err());
    }

    #[test]
    fn scan_manifest_rejects_truncation_invalid_state_and_nil_id() {
        let scan = VolumeScan {
            generation: 7,
            id: Uuid::new_v4(),
        };
        for state in [
            ScanState::Building,
            ScanState::Ready,
            ScanState::Retired,
            ScanState::Verifying,
            ScanState::Verified,
            ScanState::Rejected,
        ] {
            let bytes = encode_scan_manifest(scan, "serial", state);
            assert_eq!(
                decode_scan_manifest(&bytes).unwrap(),
                (scan, "serial".into(), state)
            );
            for end in 0..bytes.len() {
                assert!(decode_scan_manifest(&bytes[..end]).is_err());
            }
            let mut corrupt = bytes;
            *corrupt.last_mut().unwrap() = 6;
            assert!(decode_scan_manifest(&corrupt).is_err());
        }
        let mut invalid = encode_ready_volume(scan, "serial");
        invalid[8..24].fill(0);
        assert!(decode_ready_volume(&invalid).is_err());
    }

    pub(super) async fn verify_scan(
        catalog: &ManagedCatalog,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        receipts: &[CommitReceipt],
    ) {
        if let Some(path) =
            verify_scan_keep_cache(catalog, serial, epoch, volume, scan, receipts).await
        {
            std::fs::remove_file(path).unwrap();
        }
    }

    pub(super) async fn verify_scan_keep_cache(
        catalog: &ManagedCatalog,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
        receipts: &[CommitReceipt],
    ) -> Option<std::path::PathBuf> {
        let tr = catalog.db.create_trx().unwrap();
        let managed = control::volume_is_managed(&tr, volume).await.unwrap();
        drop(tr);
        let mut source_cache = None;
        if managed
            && catalog
                .db
                .create_trx()
                .unwrap()
                .get(&source_key(volume, scan.id), true)
                .await
                .unwrap()
                .is_none()
        {
            use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
            use crate::ltfs::index::{Extent, FileNode, LtfsIndex, NodeMeta};
            use crate::managed_catalog::ImportSpool;
            use sha2::Digest;

            let mut index = LtfsIndex::empty(volume, "FDB test source".into(), 'b');
            index.generation = scan.generation;
            index.self_location.start_block = 5;
            let mut files = std::collections::BTreeMap::new();
            for receipt in receipts {
                if let Some(previous) = files.insert(receipt.ltfs_file_uid, receipt.size) {
                    assert_eq!(previous, receipt.size, "same UID has different lengths");
                }
            }
            let root_uid = (1..)
                .find(|uid| !files.contains_key(uid))
                .expect("test index needs a root UID");
            index.root.meta.file_uid = root_uid;
            index.highest_file_uid = index.highest_file_uid.max(root_uid);
            for (offset, (uid, size)) in files.into_iter().enumerate() {
                index.highest_file_uid = index.highest_file_uid.max(uid);
                index.root.files.push(FileNode {
                    name: format!("uid-{uid}"),
                    length: size,
                    meta: NodeMeta {
                        file_uid: uid,
                        ..Default::default()
                    },
                    extents: (size != 0)
                        .then(|| Extent {
                            partition: 'b',
                            start_block: 6 + offset as u64,
                            byte_offset: 0,
                            byte_count: size,
                            file_offset: 0,
                        })
                        .into_iter()
                        .collect(),
                    ..Default::default()
                });
            }
            let xml = index.to_xml().unwrap();
            let source = ExpectedIndex {
                volume_uuid: volume,
                generation: scan.generation,
                location: index.self_location,
                sha256: sha2::Sha256::digest(&xml).into(),
            };
            let path =
                std::env::temp_dir().join(format!("tape-rs-fdb-scan-source-{}.db", Uuid::new_v4()));
            let spool_path = path.with_extension("spool.db");
            VolumeExtentCache::build(xml.as_ref(), &path, source).unwrap();
            ImportSpool::build(xml.as_ref(), &spool_path, source).unwrap();
            assert_eq!(
                catalog
                    .record_volume_source(
                        serial,
                        epoch,
                        volume,
                        scan,
                        source,
                        LocalVolumeSources {
                            extent_cache: &path,
                            import_spool: &spool_path
                        },
                    )
                    .await
                    .unwrap(),
                SourceOutcome::Recorded
            );
            std::fs::remove_file(spool_path).unwrap();
            source_cache = Some(path);
        }
        let mut receipts = receipts.to_vec();
        receipts.sort_by_key(ScanDigest::entry_key);
        let mut digest = ScanDigest::new(volume, scan).unwrap();
        for receipt in &receipts {
            digest.push(receipt).unwrap();
        }
        assert_eq!(
            catalog
                .freeze_volume_scan(serial, epoch, volume, scan, digest.summary())
                .await
                .unwrap(),
            VerificationOutcome::Frozen
        );
        loop {
            let result = if managed {
                catalog
                    .verify_volume_scan_page_with_cache(
                        serial,
                        epoch,
                        volume,
                        scan,
                        100,
                        source_cache.as_deref().expect("source cache"),
                    )
                    .await
                    .unwrap()
            } else {
                catalog
                    .verify_volume_scan_page(serial, epoch, volume, scan, 100)
                    .await
                    .unwrap()
            };
            match result {
                VerificationOutcome::Progress(_) => {}
                VerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, digest.summary());
                    break;
                }
                unexpected => panic!("unexpected verification: {unexpected:?}"),
            }
        }
        source_cache
    }

    pub(super) async fn verify_import_namespace(
        catalog: &ManagedCatalog,
        namespace: Uuid,
        receipts: &[CommitReceipt],
        directories: &[(Uuid, Node)],
    ) {
        let mut records = vec![(
            Uuid::nil(),
            Node {
                kind: NodeKind::Directory,
                parent: Uuid::nil(),
                name: Vec::new(),
                attributes: None,
            },
            None,
        )];
        for (id, node) in directories {
            records.push((*id, node.clone(), None));
        }
        for receipt in receipts {
            assert!(receipt.parent.is_nil(), "fixture helper expects root files");
            records.push((
                receipt.file_id,
                Node {
                    kind: NodeKind::File,
                    parent: receipt.parent,
                    name: receipt.name.clone(),
                    attributes: None,
                },
                Some(FileHead {
                    version: receipt.expected_version.unwrap_or(0) + 1,
                    size: receipt.size,
                    copy_id: receipt.copy_id,
                }),
            ));
        }
        records.sort_by_key(|(id, _, _)| *id);
        let mut digest = NamespaceDigest::new(namespace).unwrap();
        for (id, node, head) in &records {
            digest.push(*id, node, head.as_ref()).unwrap();
        }
        assert_eq!(
            catalog
                .freeze_namespace(namespace, digest.summary())
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        loop {
            match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
                NamespaceVerificationOutcome::Progress { .. } => {}
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, digest.summary());
                    break;
                }
                unexpected => panic!("unexpected namespace verification: {unexpected:?}"),
            }
        }
    }

    async fn same_generation_scan_recovery(catalog: &ManagedCatalog, cluster_file: &str) {
        let serial = format!("scan-{}", Uuid::new_v4());
        let volume = Uuid::new_v4();
        let first = VolumeScan {
            generation: 7,
            id: Uuid::new_v4(),
        };
        let second = VolumeScan {
            generation: 7,
            id: Uuid::new_v4(),
        };
        let cancelled = VolumeScan {
            generation: 8,
            id: Uuid::new_v4(),
        };
        assert!(catalog.advance_owner_epoch(&serial, 1).await.unwrap());
        let mut receipt = test_receipt(CommitReceipt {
            request_epoch: 1,
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            name: b"scan-file".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::new_v4(),
            library_serial: serial.clone(),
            pool_id: None,
            owner_epoch: 1,
            volume_id: volume,
            volume_generation: 7,
            volume_scan_id: first.id,
            ltfs_file_uid: 1,
            size: 1,
            content_digest: ContentDigest::Sha256([1; 32]),
            metadata_digest: None,
        });
        assert!(catalog.begin_import(receipt.namespace).await.unwrap());
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 1, volume, first)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            test_publish_imported(catalog, &receipt).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        verify_import_namespace(
            catalog,
            receipt.namespace,
            std::slice::from_ref(&receipt),
            &[],
        )
        .await;
        assert!(catalog.seal_import(receipt.namespace).await.unwrap());
        verify_scan(
            catalog,
            &serial,
            1,
            volume,
            first,
            std::slice::from_ref(&receipt),
        )
        .await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 1, volume, first)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        let old_receipt = receipt.clone();

        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 1, volume, second)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 1, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::ScanConflict
        );
        // Retrying the old completed scan must not clear the new pending scan.
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 1, volume, first)
                .await
                .unwrap(),
            VolumeOutcome::NotReconciling
        );
        assert!(matches!(
            catalog.copies_on_volume(volume, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        assert_eq!(
            reopened
                .begin_volume_reconcile(&serial, 1, volume, second)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyReconciling
        );
        receipt.namespace = Uuid::new_v4();
        receipt.copy_id = derive_copy_id(&receipt);
        receipt.operation_id = Uuid::new_v4();
        receipt.copy_id = derive_copy_id(&receipt);
        receipt.volume_scan_id = second.id;
        receipt.content_digest = ContentDigest::Sha256([2; 32]);
        assert!(catalog.begin_import(receipt.namespace).await.unwrap());
        assert_eq!(
            test_publish_imported(catalog, &receipt).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert!(catalog.advance_owner_epoch(&serial, 2).await.unwrap());
        let mut stale_new_operation = receipt.clone();
        stale_new_operation.operation_id = Uuid::new_v4();
        stale_new_operation.copy_id = derive_copy_id(&stale_new_operation);
        assert_eq!(
            test_publish_imported(catalog, &stale_new_operation)
                .await
                .unwrap(),
            PublishOutcome::StaleOwner
        );
        receipt.owner_epoch = 2;
        assert_eq!(
            reopened
                .begin_volume_reconcile(&serial, 2, volume, second)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyReconciling
        );
        assert_eq!(
            test_publish_imported(catalog, &receipt).await.unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        verify_import_namespace(
            catalog,
            receipt.namespace,
            std::slice::from_ref(&receipt),
            &[],
        )
        .await;
        assert!(catalog.seal_import(receipt.namespace).await.unwrap());
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 1, volume, second)
                .await
                .unwrap(),
            VolumeOutcome::StaleOwner
        );
        verify_scan(
            catalog,
            &serial,
            2,
            volume,
            second,
            std::slice::from_ref(&receipt),
        )
        .await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 2, volume, second)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(
                    old_receipt.namespace,
                    old_receipt.file_id,
                    old_receipt.copy_id
                )
                .await,
            Err(CatalogError::Incomplete)
        ));
        let current = catalog
            .legacy_copy_record_for_test(receipt.namespace, receipt.file_id, receipt.copy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.volume_scan_id, second.id);
        assert_eq!(current.content_digest, ContentDigest::Sha256([2; 32]));
        let old_cursor = volume_key(
            volume,
            first.id,
            old_receipt.namespace,
            old_receipt.file_id,
            old_receipt.copy_id,
        );
        assert!(matches!(
            catalog
                .copies_on_volume(volume, Some(&old_cursor), 10)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, first, 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, first, 1)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, first)
                .await
                .unwrap(),
            VolumeOutcome::ScanConflict
        );
        let changed_identity = VolumeScan {
            generation: 8,
            id: first.id,
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, changed_identity)
                .await
                .unwrap(),
            VolumeOutcome::ScanConflict
        );

        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let mut candidate = receipt.clone();
        candidate.namespace = Uuid::new_v4();
        candidate.copy_id = derive_copy_id(&candidate);
        candidate.operation_id = Uuid::new_v4();
        candidate.copy_id = derive_copy_id(&candidate);
        candidate.volume_scan_id = cancelled.id;
        candidate.volume_generation = cancelled.generation;
        assert!(catalog.begin_import(candidate.namespace).await.unwrap());
        assert_eq!(
            test_publish_imported(catalog, &candidate).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert!(matches!(
            catalog
                .purge_retired_volume_index(volume, cancelled, 1)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        let in_flight = catalog.db.create_trx().unwrap();
        assert!(
            in_flight
                .get(&reconcile_volume_key(volume), false)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            in_flight
                .get(&scan_key(volume, cancelled.id), false)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 1, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::StaleOwner
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 2, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::Cancelled
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 2, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyRetired
        );
        in_flight.set(
            &volume_key(
                volume,
                cancelled.id,
                candidate.namespace,
                candidate.file_id,
                Uuid::new_v4(),
            ),
            &1u64.to_be_bytes(),
        );
        assert!(in_flight.commit().await.is_err(), "取消必须拒绝在途导入");
        candidate.operation_id = Uuid::new_v4();
        candidate.copy_id = derive_copy_id(&candidate);
        assert_eq!(
            test_publish_imported(catalog, &candidate).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 2, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::NotReconciling
        );
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, cancelled, 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, cancelled, 1)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, cancelled)
                .await
                .unwrap(),
            VolumeOutcome::ScanConflict
        );
        assert!(matches!(
            catalog.copies_on_volume(volume, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(receipt.namespace, receipt.file_id, receipt.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        let mut blocked_write = receipt.clone();
        blocked_write.operation_id = Uuid::new_v4();
        blocked_write.copy_id = derive_copy_id(&blocked_write);
        assert_eq!(
            test_publish(catalog, &blocked_write).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        let older = VolumeScan {
            generation: 7,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, older)
                .await
                .unwrap(),
            VolumeOutcome::OlderGeneration
        );
        let resumed = VolumeScan {
            generation: 8,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 2, volume, resumed)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        receipt.namespace = Uuid::new_v4();
        receipt.copy_id = derive_copy_id(&receipt);
        receipt.operation_id = Uuid::new_v4();
        receipt.copy_id = derive_copy_id(&receipt);
        receipt.volume_scan_id = resumed.id;
        receipt.volume_generation = resumed.generation;
        assert!(catalog.begin_import(receipt.namespace).await.unwrap());
        assert_eq!(
            test_publish_imported(catalog, &receipt).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        verify_import_namespace(
            catalog,
            receipt.namespace,
            std::slice::from_ref(&receipt),
            &[],
        )
        .await;
        assert!(catalog.seal_import(receipt.namespace).await.unwrap());
        verify_scan(
            catalog,
            &serial,
            2,
            volume,
            resumed,
            std::slice::from_ref(&receipt),
        )
        .await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 2, volume, resumed)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        let copies = catalog.copies_on_volume(volume, None, 10).await.unwrap();
        assert_eq!(copies.items.len(), 1);
        assert_eq!(copies.items[0].copy_id, receipt.copy_id);
        assert!(
            catalog
                .legacy_copy_record_for_test(receipt.namespace, receipt.file_id, receipt.copy_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    async fn imported_attributes_are_atomic_and_verified(catalog: &ManagedCatalog) {
        use crate::ltfs::index::{NodeMeta, Xattr};

        let namespace = Uuid::new_v4();
        let serial = format!("attrs-{}", Uuid::new_v4());
        let volume_id = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        let root_attrs = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                ..Default::default()
            },
            vec![],
            None,
        )
        .unwrap();
        let dir_attrs = NodeAttributes::new(
            NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            vec![Xattr {
                key: "directory-tag".into(),
                value: "原样".into(),
                base64: false,
            }],
            None,
        )
        .unwrap();
        let mut file_xattrs = vec![Xattr {
            key: "gpfs.dmapi".into(),
            value: "AA==".into(),
            base64: true,
        }];
        file_xattrs.extend((0..9).map(|i| Xattr {
            key: format!("perf_padding_{i}"),
            value: "x".repeat(65_536),
            base64: false,
        }));
        let file_attrs = NodeAttributes::new(
            NodeMeta {
                readonly: true,
                modify_time: "2026-10-23T00:00:00.000000000Z".into(),
                file_uid: 3,
                ..Default::default()
            },
            file_xattrs,
            Some(".LTFSEE_DATA/3".into()),
        )
        .unwrap();
        assert!(catalog.begin_import(namespace).await.unwrap());
        assert!(
            catalog
                .set_import_root_attributes(namespace, &root_attrs)
                .await
                .unwrap()
        );
        assert!(
            !catalog
                .set_import_root_attributes(namespace, &root_attrs)
                .await
                .unwrap()
        );
        assert!(
            catalog
                .set_import_root_attributes(namespace, &dir_attrs)
                .await
                .is_err()
        );
        let directory = Uuid::new_v4();
        assert_eq!(
            catalog
                .create_import_directory_with_attributes(
                    namespace,
                    Uuid::nil(),
                    b"folder",
                    directory,
                    &dir_attrs,
                )
                .await
                .unwrap(),
            DirectoryOutcome::Created
        );
        assert_eq!(
            catalog
                .create_import_directory_with_attributes(
                    namespace,
                    Uuid::nil(),
                    b"folder",
                    directory,
                    &dir_attrs,
                )
                .await
                .unwrap(),
            DirectoryOutcome::AlreadyExists
        );
        assert!(catalog.advance_owner_epoch(&serial, 1).await.unwrap());
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 1, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let receipt = test_receipt(CommitReceipt {
            namespace,
            parent: directory,
            name: b"alias".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::nil(),
            library_serial: serial,
            pool_id: None,
            owner_epoch: 1,
            volume_id,
            volume_generation: 1,
            volume_scan_id: scan.id,
            ltfs_file_uid: 3,
            size: 7,
            content_digest: ContentDigest::Md5([7; 16]),
            metadata_digest: Some(file_attrs.digest()),
        });
        assert_eq!(
            catalog.register_request(&receipt).await.unwrap(),
            PublishOutcome::Registered
        );
        assert!(catalog.publish_imported(&receipt).await.is_err());
        assert!(
            catalog
                .publish_imported_with_attributes(&receipt, &dir_attrs)
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .publish_imported_with_attributes(&receipt, &file_attrs)
                .await
                .unwrap(),
            PublishOutcome::Applied(1)
        );
        assert_eq!(
            catalog
                .publish_imported_with_attributes(&receipt, &file_attrs)
                .await
                .unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        let changed_attrs = NodeAttributes::new(
            NodeMeta {
                readonly: false,
                modify_time: "2026-10-23T01:00:00.000000000Z".into(),
                file_uid: 3,
                ..Default::default()
            },
            file_attrs.xattrs().unwrap().to_vec(),
            Some(".LTFSEE_DATA/3".into()),
        )
        .unwrap();
        let mut replacement = CommitReceipt {
            expected_version: Some(1),
            operation_id: Uuid::new_v4(),
            metadata_digest: Some(changed_attrs.digest()),
            ..receipt.clone()
        };
        replacement.copy_id = derive_copy_id(&replacement);
        assert_eq!(
            catalog.register_request(&replacement).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            catalog
                .publish_imported_with_attributes(&replacement, &changed_attrs)
                .await
                .unwrap(),
            PublishOutcome::Applied(2)
        );
        let node_key = node_key(namespace, receipt.file_id);
        let chunk_key =
            attribute_objects::chunk_key(namespace, changed_attrs.manifest().unwrap(), 0);
        let tr = catalog.db.create_trx().unwrap();
        let raw = tr.get(&node_key, true).await.unwrap().unwrap();
        let first_chunk = tr.get(&chunk_key, true).await.unwrap().unwrap().to_vec();
        assert!(decode_node(&raw).unwrap().attributes.unwrap().is_external());
        drop(tr);
        catalog
            .db
            .run(|tr, _| {
                let chunk_key = chunk_key.clone();
                async move {
                    tr.clear(&chunk_key);
                    Ok(())
                }
            })
            .await
            .unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            hydrate_node(
                &tr,
                namespace,
                receipt.file_id,
                decode_node(&raw).unwrap(),
                true
            )
            .await
            .is_err()
        );
        drop(tr);
        catalog
            .db
            .run(|tr, _| {
                let (chunk_key, first_chunk) = (chunk_key.clone(), first_chunk.clone());
                async move {
                    tr.set(&chunk_key, &first_chunk);
                    Ok(())
                }
            })
            .await
            .unwrap();
        let mut nodes = vec![
            (
                Uuid::nil(),
                Node {
                    kind: NodeKind::Directory,
                    parent: Uuid::nil(),
                    name: Vec::new(),
                    attributes: Some(root_attrs.clone()),
                },
                None,
            ),
            (
                directory,
                Node {
                    kind: NodeKind::Directory,
                    parent: Uuid::nil(),
                    name: b"folder".to_vec(),
                    attributes: Some(dir_attrs.clone()),
                },
                None,
            ),
            (
                receipt.file_id,
                Node {
                    kind: NodeKind::File,
                    parent: directory,
                    name: b"alias".to_vec(),
                    attributes: Some(changed_attrs.clone()),
                },
                Some(FileHead {
                    version: 2,
                    size: 7,
                    copy_id: replacement.copy_id,
                }),
            ),
        ];
        nodes.sort_by_key(|(id, _, _)| *id);
        let mut digest = NamespaceDigest::new(namespace).unwrap();
        for (id, node, head) in &nodes {
            digest.push(*id, node, head.as_ref()).unwrap();
        }
        assert_eq!(
            catalog
                .freeze_namespace(namespace, digest.summary())
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        loop {
            match catalog.verify_namespace_page(namespace, 10).await.unwrap() {
                NamespaceVerificationOutcome::Progress { .. } => {}
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, digest.summary());
                    break;
                }
                other => panic!("属性命名空间校验异常: {other:?}"),
            }
        }
        assert!(catalog.seal_import(namespace).await.unwrap());
        assert_eq!(
            catalog.node(namespace, receipt.file_id).await.unwrap(),
            Some(
                nodes
                    .into_iter()
                    .find(|(id, _, _)| *id == receipt.file_id)
                    .unwrap()
                    .1
            )
        );
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER；仅写随机合成副本值并清理，不访问磁带"]
    async fn imported_copy_provenance_roundtrips_in_fdb() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        assert!(catalog.begin_import(namespace).await.unwrap());
        let file = Uuid::new_v4();
        let id = Uuid::new_v4();
        let copy = PhysicalCopy {
            version: 1,
            request_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 7,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 800,
            size: 1_048_576,
            content_digest: ContentDigest::Sha256([9; 32]),
            media: Some(MediaProvenance {
                version: (12, 34),
                logical_file_uid: 90,
                physical_path: ".LTFSEE_DATA/object".into(),
                ee_alias: true,
                native_symlink: false,
            }),
        };
        let key = copy_key(namespace, file, id);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&key, &encode_physical_copy(&copy).unwrap());
        tr.commit().await.unwrap();
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        let tr = reopened.db.create_trx().unwrap();
        let bytes = tr.get(&key, false).await.unwrap().unwrap();
        assert_eq!(decode_copy(&bytes).unwrap(), copy);
        drop(tr);
        let tr = reopened.db.create_trx().unwrap();
        tr.clear(&key);
        tr.commit().await.unwrap();
        assert!(reopened.discard_import(namespace).await.unwrap());
        loop {
            if reopened
                .purge_discarded_namespace(namespace, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
        let tr = reopened.db.create_trx().unwrap();
        assert!(tr.get(&key, false).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "需要显式设置 FDB_TEST_CLUSTER，连接隔离的本地 FDB；不会访问磁带设备"]
    async fn isolated_fdb_publish_and_page() {
        let cluster_file = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster_file)).unwrap();
        control::tests::exercise(&catalog, &cluster_file).await;
        maintenance::tests::exercise(&catalog, &cluster_file).await;
        requests::tests::exercise(&catalog, &cluster_file).await;
        history::tests::exercise(&catalog, &cluster_file).await;
        cleanup::tests::exercise(&catalog, &cluster_file).await;
        namespace_verification::tests::exercise(&catalog, &cluster_file).await;
        verification::tests::exercise(&catalog, &cluster_file).await;
        imported_attributes_are_atomic_and_verified(&catalog).await;
        same_generation_scan_recovery(&catalog, &cluster_file).await;
        let namespace = Uuid::new_v4();
        assert!(catalog.create_namespace(namespace).await.unwrap());
        assert!(!catalog.create_namespace(namespace).await.unwrap());
        let serial = format!("test-{}", Uuid::new_v4());
        assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
        assert!(!catalog.advance_owner_epoch(&serial, 9).await.unwrap());
        let volume_id = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        let make = |name: &[u8]| {
            test_receipt(CommitReceipt {
                request_epoch: 1,
                namespace,
                parent: Uuid::nil(),
                name: name.to_vec(),
                file_id: Uuid::new_v4(),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                copy_id: Uuid::new_v4(),
                library_serial: serial.clone(),
                pool_id: None,
                owner_epoch: 10,
                volume_id,
                volume_generation: 1,
                volume_scan_id: scan.id,
                ltfs_file_uid: 1,
                size: 7,
                content_digest: ContentDigest::Sha256([3; 32]),
                metadata_digest: None,
            })
        };
        let a = make(b"a");
        assert!(matches!(
            catalog.copies_on_volume(volume_id, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        assert_eq!(
            test_publish(&catalog, &a).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 9, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::StaleOwner
        );
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::NotReconciling
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyReconciling
        );
        assert!(matches!(
            catalog.copies_on_volume(volume_id, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        assert_eq!(
            test_publish(&catalog, &a).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        verify_scan(&catalog, &serial, 10, volume_id, scan, &[]).await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyReady
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::AlreadyReady
        );
        let mut missing_parent = a.clone();
        missing_parent.parent = Uuid::new_v4();
        missing_parent.operation_id = Uuid::new_v4();
        missing_parent.copy_id = derive_copy_id(&missing_parent);
        assert_eq!(
            test_publish(&catalog, &missing_parent).await.unwrap(),
            PublishOutcome::MissingParent
        );
        assert_eq!(
            test_publish(&catalog, &a).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert_eq!(
            catalog.node(namespace, a.file_id).await.unwrap(),
            Some(Node {
                kind: NodeKind::File,
                parent: Uuid::nil(),
                name: b"a".to_vec(),
                attributes: None,
            })
        );
        let dir_id = Uuid::new_v4();
        assert_eq!(
            catalog
                .create_directory(namespace, Uuid::nil(), b"dir", dir_id)
                .await
                .unwrap(),
            DirectoryOutcome::Created
        );
        assert_eq!(
            catalog
                .create_directory(namespace, Uuid::nil(), b"dir", dir_id)
                .await
                .unwrap(),
            DirectoryOutcome::AlreadyExists
        );
        assert_eq!(
            catalog
                .create_directory(namespace, a.file_id, b"invalid", Uuid::new_v4())
                .await
                .unwrap(),
            DirectoryOutcome::MissingParent
        );
        assert_eq!(
            test_publish(&catalog, &a).await.unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        assert_eq!(
            catalog.file_head(namespace, a.file_id).await.unwrap(),
            Some(FileHead {
                version: 1,
                size: 7,
                copy_id: a.copy_id,
            })
        );
        assert_eq!(
            catalog
                .legacy_copy_record_for_test(namespace, a.file_id, a.copy_id)
                .await
                .unwrap(),
            Some(PhysicalCopy {
                request_epoch: 1,
                version: 1,
                volume_id,
                volume_generation: 1,
                volume_scan_id: scan.id,
                ltfs_file_uid: 1,
                size: 7,
                content_digest: ContentDigest::Sha256([3; 32]),
                media: None,
            })
        );
        let mut changed = a.clone();
        changed.size += 1;
        assert_eq!(
            test_publish(&catalog, &changed).await.unwrap(),
            PublishOutcome::OperationConflict
        );
        let mut stale = make(b"b");
        stale.owner_epoch = 9;
        assert_eq!(
            test_publish(&catalog, &stale).await.unwrap(),
            PublishOutcome::StaleOwner
        );
        let mut wrong_generation = make(b"never");
        wrong_generation.volume_generation = 2;
        assert_eq!(
            test_publish(&catalog, &wrong_generation).await.unwrap(),
            PublishOutcome::VolumeNotReady
        );
        let b = make(b"b");
        assert_eq!(
            test_publish(&catalog, &b).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        let page = catalog
            .list_children(namespace, Uuid::nil(), None, 1)
            .await
            .unwrap();
        assert_eq!(
            page.items,
            vec![Child {
                name: b"a".to_vec(),
                id: a.file_id
            }]
        );
        let page = catalog
            .list_children(namespace, Uuid::nil(), page.next.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(
            page.items,
            vec![Child {
                name: b"b".to_vec(),
                id: b.file_id
            }]
        );
        let page = catalog
            .list_children(namespace, Uuid::nil(), page.next.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(
            page.items,
            vec![Child {
                name: b"dir".to_vec(),
                id: dir_id
            }]
        );
        assert!(page.next.is_none());
        assert_eq!(
            catalog
                .resolve_child(namespace, Uuid::nil(), b"b")
                .await
                .unwrap(),
            Some(b.file_id)
        );
        let copies = catalog.copies_on_volume(volume_id, None, 10).await.unwrap();
        assert_eq!(copies.items.len(), 2);
        assert!(copies.next.is_none());
        let same_name = make(b"a");
        assert_eq!(
            test_publish(&catalog, &same_name).await.unwrap(),
            PublishOutcome::NameConflict
        );
        let mut reused_inode = a.clone();
        reused_inode.operation_id = Uuid::new_v4();
        reused_inode.copy_id = derive_copy_id(&reused_inode);
        reused_inode.name = b"other".to_vec();
        assert_eq!(
            test_publish(&catalog, &reused_inode).await.unwrap(),
            PublishOutcome::NodeConflict
        );
        let mut wrong_version = a.clone();
        wrong_version.operation_id = Uuid::new_v4();
        wrong_version.copy_id = derive_copy_id(&wrong_version);
        assert_eq!(
            test_publish(&catalog, &wrong_version).await.unwrap(),
            PublishOutcome::VersionConflict
        );
        let mut overwrite = wrong_version;
        overwrite.expected_version = Some(1);
        overwrite.operation_id = Uuid::new_v4();
        overwrite.copy_id = derive_copy_id(&overwrite);
        assert_eq!(
            test_publish(&catalog, &overwrite).await.unwrap(),
            PublishOutcome::Applied(2)
        );
        assert_eq!(
            catalog
                .file_head(namespace, a.file_id)
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        assert!(
            catalog
                .list_children(namespace, Uuid::nil(), Some(b"foreign"), 1)
                .await
                .is_err()
        );
        assert!(catalog.advance_owner_epoch(&serial, 11).await.unwrap());
        let mut old_owner = make(b"c");
        old_owner.owner_epoch = 10;
        assert_eq!(
            test_publish(&catalog, &old_owner).await.unwrap(),
            PublishOutcome::StaleOwner
        );
        let old_cursor = catalog
            .copies_on_volume(volume_id, None, 1)
            .await
            .unwrap()
            .next
            .expect("旧代数至少有两个副本");

        let replacement_scan = VolumeScan {
            generation: 2,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 11, volume_id, replacement_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert!(matches!(
            catalog.copies_on_volume(volume_id, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(namespace, a.file_id, a.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        let replacement_ns = Uuid::new_v4();
        assert!(catalog.begin_import(replacement_ns).await.unwrap());
        let mut replacement = make(b"replacement");
        replacement.namespace = replacement_ns;
        replacement.copy_id = derive_copy_id(&replacement);
        replacement.owner_epoch = 11;
        replacement.volume_generation = 2;
        replacement.volume_scan_id = replacement_scan.id;
        replacement.content_digest = ContentDigest::Unknown;
        assert_eq!(
            test_publish_imported(&catalog, &replacement).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        verify_import_namespace(
            &catalog,
            replacement_ns,
            std::slice::from_ref(&replacement),
            &[],
        )
        .await;
        assert!(catalog.seal_import(replacement_ns).await.unwrap());
        verify_scan(
            &catalog,
            &serial,
            11,
            volume_id,
            replacement_scan,
            std::slice::from_ref(&replacement),
        )
        .await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 11, volume_id, replacement_scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        let current = catalog.copies_on_volume(volume_id, None, 10).await.unwrap();
        assert_eq!(current.items.len(), 1);
        assert_eq!(current.items[0].file_id, replacement.file_id);
        assert_eq!(
            catalog
                .legacy_copy_record_for_test(
                    replacement_ns,
                    replacement.file_id,
                    replacement.copy_id
                )
                .await
                .unwrap()
                .unwrap()
                .content_digest,
            ContentDigest::Unknown
        );
        assert!(matches!(
            catalog
                .copies_on_volume(volume_id, Some(&old_cursor), 10)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        assert!(matches!(
            catalog
                .purge_retired_volume_index(volume_id, replacement_scan, 1)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        let mut purged = 0;
        loop {
            let removed = catalog
                .purge_retired_volume_index(volume_id, scan, 1)
                .await
                .unwrap();
            if removed == 0 {
                break;
            }
            purged += removed;
        }
        assert_eq!(purged, 3);
        assert_eq!(
            catalog
                .copies_on_volume(volume_id, None, 10)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(namespace, a.file_id, a.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        assert!(
            catalog
                .legacy_copy_record_for_test(
                    replacement_ns,
                    replacement.file_id,
                    replacement.copy_id
                )
                .await
                .unwrap()
                .is_some()
        );

        let bulk_ns = Uuid::new_v4();
        assert!(catalog.create_namespace(bulk_ns).await.unwrap());
        catalog
            .db
            .run(|tr, _| async move {
                for n in 0..1001 {
                    tr.set(
                        &child_key(bulk_ns, Uuid::nil(), format!("f{n:04}").as_bytes()),
                        Uuid::nil().as_bytes(),
                    );
                }
                Ok(())
            })
            .await
            .unwrap();
        let first = catalog
            .list_children(bulk_ns, Uuid::nil(), None, 1000)
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1000);
        let second = catalog
            .list_children(bulk_ns, Uuid::nil(), first.next.as_deref(), 1000)
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next.is_none());

        let import_ns = Uuid::new_v4();
        assert!(catalog.begin_import(import_ns).await.unwrap());
        assert_eq!(
            catalog.namespace_state(import_ns).await.unwrap(),
            Some(NamespaceState::Building)
        );
        assert!(matches!(
            catalog.resolve_child(import_ns, Uuid::nil(), b"a").await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog
                .list_children(import_ns, Uuid::nil(), None, 10)
                .await,
            Err(CatalogError::Incomplete)
        ));
        let imported_dir = Uuid::new_v4();
        assert_eq!(
            catalog
                .create_directory(import_ns, Uuid::nil(), b"sub", imported_dir)
                .await
                .unwrap(),
            DirectoryOutcome::NamespaceNotReady
        );
        assert_eq!(
            catalog
                .create_import_directory(import_ns, Uuid::nil(), b"sub", imported_dir)
                .await
                .unwrap(),
            DirectoryOutcome::Created
        );
        let mut imported = make(b"imported");
        imported.namespace = import_ns;
        imported.copy_id = derive_copy_id(&imported);
        imported.volume_id = Uuid::new_v4();
        imported.volume_scan_id = Uuid::new_v4();
        let import_scan = VolumeScan {
            generation: 1,
            id: imported.volume_scan_id,
        };
        imported.file_id = Uuid::new_v4();
        imported.copy_id = derive_copy_id(&imported);
        imported.operation_id = Uuid::new_v4();
        imported.copy_id = derive_copy_id(&imported);
        imported.owner_epoch = 11;
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 11, imported.volume_id, import_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            test_publish(&catalog, &imported).await.unwrap(),
            PublishOutcome::NamespaceNotReady
        );
        assert_eq!(
            test_publish_imported(&catalog, &imported).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert!(matches!(
            catalog.copies_on_volume(imported.volume_id, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog.file_head(import_ns, imported.file_id).await,
            Err(CatalogError::Incomplete)
        ));
        verify_import_namespace(
            &catalog,
            import_ns,
            std::slice::from_ref(&imported),
            &[(
                imported_dir,
                Node {
                    kind: NodeKind::Directory,
                    parent: Uuid::nil(),
                    name: b"sub".to_vec(),
                    attributes: None,
                },
            )],
        )
        .await;
        assert!(catalog.seal_import(import_ns).await.unwrap());
        assert!(!catalog.seal_import(import_ns).await.unwrap());
        assert_eq!(
            catalog.namespace_state(import_ns).await.unwrap(),
            Some(NamespaceState::Ready)
        );
        assert_eq!(
            catalog
                .resolve_child(import_ns, Uuid::nil(), b"imported")
                .await
                .unwrap(),
            Some(imported.file_id)
        );
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(import_ns, imported.file_id, imported.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        verify_scan(
            &catalog,
            &serial,
            11,
            imported.volume_id,
            import_scan,
            std::slice::from_ref(&imported),
        )
        .await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 11, imported.volume_id, import_scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert_eq!(
            catalog
                .copies_on_volume(imported.volume_id, None, 10)
                .await
                .unwrap()
                .items
                .len(),
            1
        );

        let fence_volume = Uuid::new_v4();
        let fence_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 11, fence_volume, fence_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let in_flight_volume = catalog.db.create_trx().unwrap();
        assert!(
            in_flight_volume
                .get(&reconcile_volume_key(fence_volume), false)
                .await
                .unwrap()
                .is_some()
        );
        verify_scan(&catalog, &serial, 11, fence_volume, fence_scan, &[]).await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 11, fence_volume, fence_scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        in_flight_volume.set(
            &volume_key(
                fence_volume,
                fence_scan.id,
                import_ns,
                imported.file_id,
                Uuid::new_v4(),
            ),
            &1u64.to_be_bytes(),
        );
        assert!(
            in_flight_volume.commit().await.is_err(),
            "卷就绪必须冲突并拒绝在途导入"
        );
        assert_eq!(
            test_publish_imported(&catalog, &imported).await.unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        let mut late_import = imported.clone();
        late_import.name = b"late".to_vec();
        late_import.file_id = Uuid::new_v4();
        late_import.copy_id = derive_copy_id(&late_import);
        late_import.operation_id = Uuid::new_v4();
        late_import.copy_id = derive_copy_id(&late_import);
        assert_eq!(
            test_publish_imported(&catalog, &late_import).await.unwrap(),
            PublishOutcome::NamespaceNotBuilding
        );
        assert_eq!(
            catalog
                .create_import_directory(import_ns, Uuid::nil(), b"late-dir", Uuid::new_v4())
                .await
                .unwrap(),
            DirectoryOutcome::NamespaceNotBuilding
        );

        let fence_ns = Uuid::new_v4();
        assert!(catalog.begin_import(fence_ns).await.unwrap());
        let in_flight = catalog.db.create_trx().unwrap();
        assert!(
            in_flight
                .get(&namespace_key(fence_ns), false)
                .await
                .unwrap()
                .is_some()
        );
        verify_import_namespace(&catalog, fence_ns, &[], &[]).await;
        assert!(catalog.seal_import(fence_ns).await.unwrap());
        in_flight.set(&node_key(fence_ns, Uuid::new_v4()), b"late import");
        assert!(
            in_flight.commit().await.is_err(),
            "封口必须冲突并拒绝在途导入"
        );
    }
}
