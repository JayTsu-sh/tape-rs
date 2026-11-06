//! Original two-path rename requests, bounded manifests and fenced publication.
//! Preflight alone grants neither a namespace move nor permission to stage media.
use super::*;

pub(crate) const RENAME_MARKER: &str = crate::managed_catalog::NODE_RENAME_MARKER;

#[path = "node_rename/request.rs"]
mod request;
pub use request::NodeRenameRequest;
pub(super) use request::revalidate_at;
#[path = "node_rename/inputs.rs"]
mod inputs;
pub use inputs::PreparedNodeRenameInputs;
#[path = "node_rename/subtree.rs"]
mod subtree;
pub use subtree::{
    NodeRenameSubtreeCapture, NodeRenameSubtreeEntry, NodeRenameSubtreePage,
    NodeRenameSubtreeSummary,
};

#[path = "node_rename/entry.rs"]
mod entry;
#[path = "node_rename/manifest.rs"]
mod manifest;
pub use manifest::{
    NodeRenameGlobalPlan, NodeRenameManifest, NodeRenameManifestCapture, NodeRenameManifestReader,
    NodeRenameParticipant, NodeRenameParticipantsCapture, NodeRenameParticipantsSummary,
};

#[path = "node_rename/projection.rs"]
mod projection;
pub use projection::{
    NodeRenameProjectionSpool, NodeRenameProjectionStats, NodeRenameProjectionVolume,
};

#[path = "node_rename/live_guard.rs"]
mod live_guard;
pub use live_guard::{
    NodeRenameDataReferences, NodeRenameLiveFile, NodeRenameReferenceRole, NodeRenameVolumeGuard,
    NodeRenameVolumeGuardStats,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeRenameRevision {
    pub namespace: Uuid,
    pub inode: Uuid,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameSpec {
    pub pool: Uuid,
    pub operation: Uuid,
    pub from: String,
    pub to: String,
    pub source: NodeRenameRevision,
    pub source_parent: NodeRenameRevision,
    pub destination_parent: NodeRenameRevision,
    /// None is an original absence condition, not permission to replace a later node.
    pub target: Option<NodeRenameRevision>,
    pub no_replace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameBaseline {
    pub library_serial: String,
    pub owner_epoch: u64,
    pub request_epoch: u64,
    pub spec: NodeRenameSpec,
    pub source_parent: NodeMutationParent,
    pub destination_parent: NodeMutationParent,
    pub source: ResolvedPath,
    pub target: Option<ResolvedPath>,
}

fn invalid(reason: &'static str) -> CatalogError {
    CatalogError::Invalid(reason)
}

fn parent_path(path: &str) -> &str {
    let parent = path.rsplit_once('/').expect("validated nonroot path").0;
    if parent.is_empty() { "/" } else { parent }
}

fn validate(spec: &NodeRenameSpec) -> Result<(Vec<u8>, Vec<u8>)> {
    if spec.operation.is_nil() || spec.source.namespace.is_nil() {
        return Err(invalid("rename 操作或 namespace 无效"));
    }
    for revision in [
        Some(spec.source),
        Some(spec.source_parent),
        Some(spec.destination_parent),
        spec.target,
    ]
    .into_iter()
    .flatten()
    {
        if revision.namespace != spec.source.namespace
            || revision.inode.is_nil()
            || revision.version == 0
            || revision.version == u64::MAX
        {
            return Err(invalid(
                "rename 必须提供同一 namespace 的完整原身份和可递增版本",
            ));
        }
    }
    if spec.source.inode == spec.source_parent.inode
        || spec.source.inode == spec.destination_parent.inode
        || spec.target.is_some_and(|target| {
            [
                spec.source.inode,
                spec.source_parent.inode,
                spec.destination_parent.inode,
            ]
            .contains(&target.inode)
        })
        || (spec.no_replace && spec.target.is_some())
    {
        return Err(invalid("rename 原目标、父目录或替换条件冲突"));
    }
    let mut paths = Vec::new();
    for path in [&spec.from, &spec.to] {
        if path == "/" || !crate::ltfs::xml_text_valid(path) {
            return Err(invalid("rename 路径无效"));
        }
        paths.push(pool_catalog_path(spec.pool, path.as_bytes())?);
    }
    let within = |path: &str, ancestor: &str| {
        path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|suffix| suffix.starts_with('/'))
    };
    if within(&spec.from, &spec.to) || within(&spec.to, &spec.from) {
        return Err(invalid("rename 两端不能相同或构成祖先关系"));
    }
    let same_parent = parent_path(&spec.from) == parent_path(&spec.to);
    if (same_parent && spec.source_parent != spec.destination_parent)
        || (!same_parent && spec.source_parent.inode == spec.destination_parent.inode)
    {
        return Err(invalid("rename 父目录身份与两端路径不符"));
    }
    Ok((paths.remove(0), paths.remove(0)))
}

fn revision(namespace: Uuid, value: &ResolvedPath) -> NodeRenameRevision {
    match value {
        ResolvedPath::Directory { id, version, .. } => NodeRenameRevision {
            namespace,
            inode: *id,
            version: *version,
        },
        ResolvedPath::File { id, head, .. } => NodeRenameRevision {
            namespace,
            inode: *id,
            version: head.version,
        },
    }
}

async fn parent_at(
    tr: &Transaction,
    namespace: Uuid,
    components: &[&[u8]],
    expected: NodeRenameRevision,
) -> Result<NodeMutationParent> {
    match lookup::resolve_path_at_mode(tr, namespace, components.to_vec(), false, true).await? {
        Some(ResolvedPath::Directory {
            id,
            version,
            attributes,
        }) if id == expected.inode && version == expected.version => Ok(NodeMutationParent {
            id,
            version,
            attributes,
        }),
        _ => Err(invalid("rename 原父目录身份或版本已改变")),
    }
}

async fn path_available_at(
    tr: &Transaction,
    namespace: Uuid,
    parent: Uuid,
    name: &[u8],
) -> Result<()> {
    for key in [
        reservation::path_key_for(namespace, parent, name),
        active_batch::path_claim_key_for(namespace, parent, name),
    ] {
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::Incomplete);
        }
    }
    Ok(())
}

async fn node_available_at(
    tr: &Transaction,
    serial: &str,
    namespace: Uuid,
    pool: Uuid,
    node: &ResolvedPath,
) -> Result<()> {
    let id = revision(namespace, node).inode;
    for key in [
        reservation::file_key_for(namespace, id),
        active_batch::file_claim_key_for(namespace, id),
    ] {
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::Incomplete);
        }
    }
    match node {
        ResolvedPath::File { placement, .. }
            if placement.namespace != namespace
                || placement.pool_id != pool
                || placement.library_serial != serial =>
        {
            Err(CatalogError::Corrupt("rename 文件副本归属不符"))
        }
        ResolvedPath::Directory { .. } => {
            if tr.get(&head_key(namespace, id), false).await?.is_some() {
                return Err(CatalogError::Corrupt("rename 目录存在孤立 head"));
            }
            let mut copies = copy_prefix(namespace);
            copies.extend_from_slice(id.as_bytes());
            if requests::prefix_has_rows(tr, &copies).await? {
                return Err(CatalogError::Corrupt("rename 目录存在孤立副本"));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(super) async fn baseline_at(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    spec: &NodeRenameSpec,
    epoch: Option<u64>,
) -> Result<NodeRenameBaseline> {
    let (from, to) = validate(spec)?;
    control::validate_physical_serial(serial)?;
    let namespace = routing::require_runtime_active_owner(tr, serial, route, owner).await?;
    if namespace != spec.source.namespace {
        return Err(invalid("rename 原 namespace 与活动路由不符"));
    }
    control::require_pool_library(tr, namespace, spec.pool, serial).await?;
    let pool = control::decode_pool(
        spec.pool,
        &tr.get(&control::pool_key(namespace, spec.pool), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    active_pools::require_indexes(tr, namespace, &pool).await?;
    control::native_media::require_pool_readable(tr, namespace, spec.pool).await?;
    let from = lookup::path_components(&from)?;
    let to = lookup::path_components(&to)?;
    let (from_name, from_ancestors) = from.split_last().expect("nonroot source");
    let (to_name, to_ancestors) = to.split_last().expect("nonroot destination");
    let source_parent = parent_at(tr, namespace, from_ancestors, spec.source_parent).await?;
    let destination_parent =
        parent_at(tr, namespace, to_ancestors, spec.destination_parent).await?;
    path_available_at(tr, namespace, source_parent.id, from_name).await?;
    path_available_at(tr, namespace, destination_parent.id, to_name).await?;
    let source = lookup::resolve_path_at_mode(tr, namespace, from, false, true)
        .await?
        .ok_or(invalid("rename 原源节点不存在"))?;
    let target = lookup::resolve_path_at_mode(tr, namespace, to, false, true).await?;
    if revision(namespace, &source) != spec.source
        || target.as_ref().map(|node| revision(namespace, node)) != spec.target
    {
        return Err(invalid("rename 原源或目标身份、版本、缺失条件已改变"));
    }
    node_available_at(tr, serial, namespace, spec.pool, &source).await?;
    if let Some(target) = &target {
        node_available_at(tr, serial, namespace, spec.pool, target).await?;
        if matches!(source, ResolvedPath::Directory { .. })
            != matches!(target, ResolvedPath::Directory { .. })
        {
            return Err(invalid("rename 源与替换目标类型不符"));
        }
        if let ResolvedPath::Directory { id, .. } = target {
            if requests::prefix_has_rows(tr, &child_prefix(namespace, *id)).await? {
                return Err(invalid("rename 替换目标目录非空"));
            }
            for prefix in [
                reservation::path_prefix_for(namespace, *id),
                active_batch::path_claim_prefix_for(namespace, *id),
            ] {
                if requests::prefix_has_rows(tr, &prefix).await? {
                    return Err(CatalogError::Incomplete);
                }
            }
        }
    }
    let window = control::management::window(tr, namespace).await?;
    let request_epoch = epoch.unwrap_or(window.accepting);
    if request_epoch < window.first_retained || request_epoch > window.accepting {
        return Err(invalid("rename 原请求代次不在保留窗口"));
    }
    Ok(NodeRenameBaseline {
        library_serial: serial.into(),
        owner_epoch: owner,
        request_epoch,
        spec: spec.clone(),
        source_parent,
        destination_parent,
        source,
        target,
    })
}

impl ManagedCatalog {
    pub async fn prepare_node_rename_baseline(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        spec: &NodeRenameSpec,
    ) -> Result<NodeRenameBaseline> {
        baseline_at(&self.db.create_trx()?, serial, route, owner, spec, None).await
    }
}

#[path = "node_rename/physical_source.rs"]
mod physical_source;
pub use physical_source::{NodeRenamePhysicalSource, NodeRenamePhysicalSourceStats};

#[path = "node_rename/volume_projection.rs"]
mod volume_projection;
pub use volume_projection::{NodeRenameVolumeProjection, NodeRenameVolumeProjectionStats};

#[path = "node_rename/volume_plan.rs"]
mod volume_plan;
pub use volume_plan::NodeRenameVolumePlan;

#[path = "node_rename/media.rs"]
mod media;
pub use media::{
    NodeRenameAbsenceEvidence, NodeRenameAbsenceKind, NodeRenameMediaPlan, NodeRenameS4Evidence,
};

pub use manifest::{NodeRenameAttempt, NodeRenameAttemptPhase, NodeRenameVolumeIntent};
pub(in crate::managed_catalog) use manifest::{
    require_no_prepared_attempt_at, require_prepared_volume_claim_at,
};

pub(in crate::managed_catalog) use manifest::require_no_missing_prepared_claim_at;

pub(in crate::managed_catalog) use manifest::require_writing_task_at;

pub use manifest::NodeRenameObservation;
pub(in crate::managed_catalog) use manifest::require_rename_observation_at;

pub use manifest::NodeRenameIntentClosure;
pub(in crate::managed_catalog) use manifest::require_rename_discard_at;

pub use manifest::NodeRenamePublication;
pub(in crate::managed_catalog) use manifest::require_rename_task_outcome_at;
