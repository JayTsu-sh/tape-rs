//! Typed, deterministic LTFS node changes. A plan is not admission authority:
//! its owner must persist and fence the intent before staging it on a volume.
use super::{ActiveIndexIdentity, CatalogError, NodeAttributes, NodeKind, Result};
use crate::ltfs::index::{DirectoryNode, FileNode, LtfsIndex, MetadataChange, NodeMeta, Xattr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(crate) const MUTATION_MARKER: &str = "tapers.fdb.nodeMutation";
mod absence;
mod projection;
mod source_copy;
pub use absence::{NodeMutationAbsenceEvidence, NodeMutationAbsenceKind};
pub(in crate::managed_catalog) use source_copy::{
    PhysicalDataIdentity, logical_file_link_matches, validate_physical_file_copy,
};
pub(super) const DOMAIN: &[u8] = b"tape-rs/node-mutation/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum XattrMode {
    Upsert,
    Create,
    Replace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NodeMutation {
    CreateDirectory,
    CreateSymlink {
        target: String,
    },
    SetXattr {
        attribute: Xattr,
        mode: XattrMode,
    },
    RemoveXattr {
        key: String,
    },
    ChangeMetadata {
        readonly: Option<bool>,
        access_time: Option<String>,
        modify_time: Option<String>,
    },
    Unlink,
    RemoveDirectory,
}

impl NodeMutation {
    /// Shared syntax checks for logical preflight and physical projection.
    pub(super) fn validate_input(&self) -> Result<()> {
        match self {
            Self::CreateSymlink { target } => {
                if target.is_empty() || !crate::ltfs::xml_text_valid(target) {
                    return Err(invalid("符号链接目标为空或包含非法 XML 字符"));
                }
            }
            Self::SetXattr { attribute, .. } => {
                attr_key(&attribute.key)?;
                if !crate::ltfs::xml_text_valid(&attribute.value) {
                    return Err(invalid("扩展属性包含非法 XML 字符"));
                }
                if attribute.base64 {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&attribute.value)
                        .map_err(|_| invalid("扩展属性 base64 非法"))?;
                    if base64::engine::general_purpose::STANDARD.encode(bytes) != attribute.value {
                        return Err(invalid("扩展属性 base64 必须为规范编码"));
                    }
                }
            }
            Self::RemoveXattr { key } => attr_key(key)?,
            Self::ChangeMetadata {
                readonly,
                access_time,
                modify_time,
            } => {
                let change = MetadataChange {
                    readonly: *readonly,
                    access_time: access_time.clone(),
                    modify_time: modify_time.clone(),
                };
                change.validate().map_err(|_| invalid("节点属性时间非法"))?;
                if change.is_empty() {
                    return Err(invalid("节点属性修改为空"));
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn changes_parent_attributes(&self) -> bool {
        matches!(
            self,
            Self::CreateDirectory
                | Self::CreateSymlink { .. }
                | Self::Unlink
                | Self::RemoveDirectory
        )
    }
}

/// Logical and physical identity to be checked by the future admission ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationContext {
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool: Uuid,
    pub operation: Uuid,
    pub request_epoch: u64,
    pub owner_epoch: u64,
    pub parent: Uuid,
    pub parent_version: u64,
    pub node: Uuid,
    pub expected_version: Option<u64>,
    pub scan: Uuid,
    pub previous: ActiveIndexIdentity,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Snapshot {
    Missing,
    Directory {
        meta: NodeMeta,
        xattrs: Vec<Xattr>,
        children: u64,
    },
    File {
        node: FileNode,
    },
}

impl Snapshot {
    fn attributes(&self) -> Result<Option<NodeAttributes>> {
        match self {
            Self::Missing => Ok(None),
            Self::Directory { meta, xattrs, .. } => {
                NodeAttributes::new(meta.clone(), xattrs.clone(), None).map(Some)
            }
            Self::File { node } => NodeAttributes::from_file(node).map(Some),
        }
    }
    fn metadata_mut(&mut self) -> Result<(&mut NodeMeta, &mut Vec<Xattr>)> {
        match self {
            Self::Missing => Err(invalid("变更目标不存在")),
            Self::Directory { meta, xattrs, .. } => Ok((meta, xattrs)),
            Self::File { node } => Ok((&mut node.meta, &mut node.xattrs)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    library: String,
    namespace: String,
    pool: String,
    operation: String,
    request_epoch: u64,
    owner_epoch: u64,
    parent: String,
    parent_version: u64,
    node: String,
    expected_version: Option<u64>,
    scan: String,
    volume: String,
    generation: u64,
    partition: char,
    block: u64,
    sha256: Vec<u8>,
    incremental: bool,
    path: String,
    time: String,
    mutation: NodeMutation,
    highest_uid: u64,
    next_uid: u64,
    before: Snapshot,
    source_data: Option<source_copy::FileSource>,
    after: Snapshot,
    parent_before: Option<NodeMeta>,
    parent_after: NodeMeta,
    ancestors: Vec<projection::Ancestor>,
    target_attributes: Option<projection::LogicalAttributes>,
    target_kind: Option<TargetKind>,
    tree_before: Vec<u8>,
    tree_after: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TargetKind {
    Directory,
    File,
}

impl From<NodeKind> for TargetKind {
    fn from(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Directory => Self::Directory,
            NodeKind::File => Self::File,
        }
    }
}

/// Immutable plan; both the source and projected complete tree are bound by
/// domain-separated hashes. Only the affected node's attributes are copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationPlan {
    context: NodeMutationContext,
    wire: Wire,
}

/// Produced only after a clean, PR-checked S4 and complete semantic verification.
/// It proves a media result, not an FDB directory publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationAncestorEvidence {
    path: String,
    id: Uuid,
    version_before: u64,
    physical_uid: u64,
    newly_materialized: bool,
}

impl NodeMutationAncestorEvidence {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn version_before(&self) -> u64 {
        self.version_before
    }
    pub fn physical_uid(&self) -> u64 {
        self.physical_uid
    }
    pub fn newly_materialized(&self) -> bool {
        self.newly_materialized
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationEvidence {
    context: NodeMutationContext,
    current: ActiveIndexIdentity,
    fingerprint: [u8; 32],
    file_count: u64,
    attributes: Option<NodeAttributes>,
    parent_meta: NodeMeta,
    logical_attributes: Option<NodeAttributes>,
    logical_parent_attributes: NodeAttributes,
    ancestors: Vec<NodeMutationAncestorEvidence>,
}
impl NodeMutationEvidence {
    pub fn context(&self) -> &NodeMutationContext {
        &self.context
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
    pub fn file_count(&self) -> u64 {
        self.file_count
    }
    pub fn attributes(&self) -> Option<&NodeAttributes> {
        self.attributes.as_ref()
    }
    pub fn parent_meta(&self) -> &NodeMeta {
        &self.parent_meta
    }
    pub fn logical_attributes(&self) -> Option<&NodeAttributes> {
        self.logical_attributes.as_ref()
    }
    pub fn logical_parent_attributes(&self) -> &NodeAttributes {
        &self.logical_parent_attributes
    }
    pub fn ancestors(&self) -> &[NodeMutationAncestorEvidence] {
        &self.ancestors
    }
}

pub(in crate::managed_catalog) struct NodePublicationProjection {
    pub kind: super::NodeKind,
    pub ancestors: Vec<super::NodeMutationAncestor>,
    pub before: Option<NodeAttributes>,
    pub after: NodeAttributes,
    pub parent_after: NodeAttributes,
}

pub(in crate::managed_catalog) struct NodeDeletionProjection {
    pub kind: super::NodeKind,
    pub ancestors: Vec<super::NodeMutationAncestor>,
    pub before: NodeAttributes,
    pub parent_after: NodeAttributes,
}

fn invalid(message: &'static str) -> CatalogError {
    CatalogError::Invalid(message)
}
pub(super) fn id(text: &str) -> Result<Uuid> {
    text.parse::<Uuid>()
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or(invalid("节点变更 UUID 非法"))
}
pub(super) fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_value(value)
        .map(|value| value.to_string().into_bytes())
        .map_err(|_| invalid("节点变更编码失败"))
}
fn path_parts(path: &str) -> Result<(&str, &str)> {
    if !path.starts_with('/')
        || path == "/"
        || path.len() > 2048
        || !crate::ltfs::xml_text_valid(path)
        || path[1..]
            .split('/')
            .any(|name| matches!(name, "" | "." | ".."))
    {
        return Err(invalid("节点变更须使用规范池内非根绝对路径"));
    }
    let (parent, name) = path.rsplit_once('/').expect("absolute path");
    Ok((if parent.is_empty() { "/" } else { parent }, name))
}
fn snapshot(parent: &DirectoryNode, name: &str) -> Result<Snapshot> {
    let files: Vec<_> = parent.files.iter().filter(|f| f.name == name).collect();
    let dirs: Vec<_> = parent.subdirs.iter().filter(|d| d.name == name).collect();
    match (files.as_slice(), dirs.as_slice()) {
        ([], []) => Ok(Snapshot::Missing),
        ([file], []) if file.delta_fields.is_none() && !file.open_for_write.unwrap_or(false) => {
            Ok(Snapshot::File {
                node: (*file).clone(),
            })
        }
        ([], [dir]) if dir.delta_fields.is_none() => Ok(Snapshot::Directory {
            meta: dir.meta.clone(),
            xattrs: dir.xattrs.clone(),
            children: (dir.files.len() as u64)
                .checked_add(dir.subdirs.len() as u64)
                .ok_or(invalid("目录子项数溢出"))?,
        }),
        _ => Err(invalid("目标节点重复、尚未完整重放或仍在写入")),
    }
}
fn attr_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !crate::ltfs::xml_text_valid(key)
        || key.starts_with("ltfs.hash.")
        || matches!(
            key,
            MUTATION_MARKER | super::NODE_RENAME_MARKER | crate::ltfs::volume::XATTR_VERSION
        )
        || key == super::link_kind::NATIVE_SYMLINK
        || key == super::ee_internal::MARKER
    {
        return Err(invalid("扩展属性名称非法或属于受保护的提交/内容字段"));
    }
    Ok(())
}
fn project(wire: &Wire) -> Result<(Snapshot, NodeMeta, u64)> {
    wire.mutation.validate_input()?;
    let (_, name) = path_parts(&wire.path)?;
    MetadataChange {
        modify_time: Some(wire.time.clone()),
        ..Default::default()
    }
    .validate()
    .map_err(|_| invalid("节点变更时间不是规范 LTFS 时间"))?;
    let mut after = projection::target(wire)?;
    let mut parent = wire
        .ancestors
        .last()
        .ok_or(invalid("规范祖先链缺失"))?
        .meta();
    let mut highest = projection::highest_uid(wire);
    match &wire.mutation {
        NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. } => {
            if wire.before != Snapshot::Missing {
                return Err(invalid("创建路径已存在"));
            }
            highest = highest
                .checked_add(1)
                .ok_or(invalid("LTFS fileuid 已耗尽"))?;
            let meta = NodeMeta {
                file_uid: highest,
                readonly: false,
                creation_time: wire.time.clone(),
                change_time: wire.time.clone(),
                modify_time: wire.time.clone(),
                access_time: wire.time.clone(),
                backup_time: wire.time.clone(),
            };
            after = match &wire.mutation {
                NodeMutation::CreateSymlink { target } => Snapshot::File {
                    node: FileNode {
                        name: name.into(),
                        meta,
                        symlink: Some(target.clone()),
                        xattrs: vec![super::link_kind::native_marker()],
                        ..Default::default()
                    },
                },
                _ => Snapshot::Directory {
                    meta,
                    xattrs: vec![],
                    children: 0,
                },
            };
        }
        NodeMutation::Unlink => {
            if !matches!(after, Snapshot::File { .. }) {
                return Err(invalid("unlink 目标不是文件或链接"));
            }
            after = Snapshot::Missing;
        }
        NodeMutation::RemoveDirectory => {
            if !matches!(after, Snapshot::Directory { children: 0, .. }) {
                return Err(invalid("rmdir 目标不存在、类型错误或目录非空"));
            }
            after = Snapshot::Missing;
        }
        NodeMutation::SetXattr { attribute, mode } => {
            let (meta, attrs) = after.metadata_mut()?;
            let found = attrs.iter().position(|a| a.key == attribute.key);
            if (*mode == XattrMode::Create && found.is_some())
                || (*mode == XattrMode::Replace && found.is_none())
            {
                return Err(invalid("扩展属性 create/replace 条件不符"));
            }
            if let Some(at) = found {
                attrs[at] = attribute.clone();
            } else {
                attrs.push(attribute.clone());
            }
            meta.change_time = wire.time.clone();
        }
        NodeMutation::RemoveXattr { key } => {
            let (meta, attrs) = after.metadata_mut()?;
            let at = attrs
                .iter()
                .position(|a| a.key == *key)
                .ok_or(invalid("扩展属性不存在"))?;
            attrs.remove(at);
            meta.change_time = wire.time.clone();
        }
        NodeMutation::ChangeMetadata {
            readonly,
            access_time,
            modify_time,
        } => {
            let (meta, _) = after.metadata_mut()?;
            if let Some(value) = readonly {
                meta.readonly = *value;
            }
            if let Some(value) = access_time {
                meta.access_time = value.clone();
            }
            if let Some(value) = modify_time {
                meta.modify_time = value.clone();
            }
            meta.change_time = wire.time.clone();
        }
    }
    if wire.mutation.changes_parent_attributes() {
        parent.modify_time = wire.time.clone();
        parent.change_time = wire.time.clone();
    }
    // An existing logical directory can be absent on this particular volume.
    // Allocate a physical UID only when the result actually contains it;
    // removing an absent directory must not invent a physical node or UID.
    if wire.before == Snapshot::Missing
        && wire.target_kind == Some(TargetKind::Directory)
        && let Snapshot::Directory { meta, .. } = &mut after
    {
        highest = highest
            .checked_add(1)
            .ok_or(invalid("LTFS fileuid 已耗尽"))?;
        meta.file_uid = highest;
    }
    after.attributes()?;
    Ok((after, parent, highest))
}

impl NodeMutationPlan {
    pub fn prepare(
        context: NodeMutationContext,
        mutation: NodeMutation,
        index: &LtfsIndex,
        time: &str,
        ancestors: &[super::NodeMutationAncestor],
        target: Option<(NodeKind, &NodeAttributes)>,
    ) -> Result<Self> {
        if index.incremental && !index.materialized {
            return Err(invalid(
                "节点变更须使用完整重放视图，不能使用原始 delta 子集",
            ));
        }
        let (parent, name) = path_parts(&context.path)?;
        let dir = index.find_directory(parent);
        let before = dir
            .map(|dir| snapshot(dir, name))
            .transpose()?
            .unwrap_or(Snapshot::Missing);
        let previous = context.previous;
        let target_kind = target.map(|(kind, _)| TargetKind::from(kind));
        let (ancestors, target_attributes) =
            projection::capture(&context, index, ancestors, target.map(|(_, attrs)| attrs))?;
        let source_data = source_copy::capture(index, &context.path, &before)?;
        let mut wire = Wire {
            version: 5,
            library: context.library_serial.clone(),
            namespace: context.namespace.to_string(),
            pool: context.pool.to_string(),
            operation: context.operation.to_string(),
            request_epoch: context.request_epoch,
            owner_epoch: context.owner_epoch,
            parent: context.parent.to_string(),
            parent_version: context.parent_version,
            node: context.node.to_string(),
            expected_version: context.expected_version,
            scan: context.scan.to_string(),
            volume: previous.volume_uuid.to_string(),
            generation: previous.generation,
            partition: previous.location.partition,
            block: previous.location.start_block,
            sha256: previous.sha256.to_vec(),
            incremental: previous.incremental,
            path: context.path.clone(),
            time: time.into(),
            mutation,
            highest_uid: index.highest_file_uid,
            next_uid: index.highest_file_uid,
            before,
            source_data,
            after: Snapshot::Missing,
            parent_before: dir.map(|dir| dir.meta.clone()),
            parent_after: ancestors.last().expect("validated ancestor chain").meta(),
            ancestors,
            target_attributes,
            target_kind,
            tree_before: vec![],
            tree_after: vec![],
        };
        (wire.after, wire.parent_after, wire.next_uid) = project(&wire)?;
        wire.tree_before = tree_digest(index, None)?.to_vec();
        wire.tree_after = tree_digest(index, Some(&wire))?.to_vec();
        let plan = Self { context, wire };
        plan.validate()?;
        plan.require_source_header(index)?;
        Ok(plan)
    }
    pub fn context(&self) -> &NodeMutationContext {
        &self.context
    }
    pub fn mutation(&self) -> &NodeMutation {
        &self.wire.mutation
    }
    /// Logical publication must preserve the logical UID and intrinsic fields,
    /// especially for the virtual pool root. Physical parent evidence has a
    /// volume-local UID and may have different creation/private metadata.
    pub fn logical_parent_attributes_after(&self) -> Result<NodeAttributes> {
        let logical = &self
            .wire
            .ancestors
            .last()
            .ok_or(invalid("规范祖先链缺失"))?
            .logical;
        let mut meta = logical.meta.clone();
        if self.wire.mutation.changes_parent_attributes() {
            meta.modify_time = self.wire.time.clone();
            meta.change_time = self.wire.time.clone();
        }
        NodeAttributes::new(meta, logical.xattrs.clone(), None)
    }
    /// Existing targets retain their logical metadata UID. A newly created
    /// target uses this volume's assigned UID as a candidate; publication must
    /// persist which physical primary supplied the logical value.
    pub fn logical_attributes_after(&self) -> Result<Option<NodeAttributes>> {
        let Some(attributes) = self.wire.after.attributes()? else {
            return Ok(None);
        };
        let Some(logical) = &self.wire.target_attributes else {
            return Ok(Some(attributes));
        };
        let mut meta = attributes
            .meta()
            .expect("complete projected attributes")
            .clone();
        meta.file_uid = logical.meta.file_uid;
        NodeAttributes::new(
            meta,
            attributes
                .xattrs()
                .expect("complete projected attributes")
                .to_vec(),
            attributes.symlink().map(str::to_owned),
        )
        .map(Some)
    }

    fn require_logical_request(&self, request: &super::NodeMutationRequest) -> Result<()> {
        let c = &self.context;
        let r = request.context();
        if r.library_serial != c.library_serial
            || r.namespace != c.namespace
            || r.pool != c.pool
            || r.operation != c.operation
            || r.request_epoch != c.request_epoch
            || r.path != c.path
            || r.parent != c.parent
            || r.parent_version != c.parent_version
            || r.node != c.node
            || r.expected_version != c.expected_version
            || request.mutation() != &self.wire.mutation
            || request.time() != self.wire.time
        {
            return Err(invalid("节点发布计划与原逻辑请求不符"));
        }
        Ok(())
    }

    fn canonical_ancestors(&self) -> Result<Vec<super::NodeMutationAncestor>> {
        self.wire
            .ancestors
            .iter()
            .map(|ancestor| {
                Ok(super::NodeMutationAncestor {
                    path: ancestor.path.clone(),
                    id: id(&ancestor.inode)?,
                    version: ancestor.version,
                    attributes: ancestor.logical.attributes()?,
                })
            })
            .collect()
    }

    pub(in crate::managed_catalog) fn publication_projection(
        &self,
        request: &super::NodeMutationRequest,
    ) -> Result<NodePublicationProjection> {
        self.require_logical_request(request)?;
        let kind = match (&self.wire.before, &self.wire.after) {
            (Snapshot::Missing | Snapshot::Directory { .. }, Snapshot::Directory { .. }) => {
                super::NodeKind::Directory
            }
            (Snapshot::File { .. }, Snapshot::File { .. })
                if !self.wire.mutation.changes_parent_attributes() =>
            {
                super::NodeKind::File
            }
            (Snapshot::Missing, Snapshot::File { .. })
                if matches!(self.wire.mutation, NodeMutation::CreateSymlink { .. }) =>
            {
                super::NodeKind::File
            }
            _ => return Err(invalid("此发布接口仅处理创建节点和既有节点属性变更")),
        };
        Ok(NodePublicationProjection {
            kind,
            ancestors: self.canonical_ancestors()?,
            before: self
                .wire
                .target_attributes
                .as_ref()
                .map(|a| a.attributes())
                .transpose()?,
            after: self
                .logical_attributes_after()?
                .ok_or(invalid("目录发布缺少结果属性"))?,
            parent_after: self.logical_parent_attributes_after()?,
        })
    }

    pub(in crate::managed_catalog) fn deletion_projection(
        &self,
        request: &super::NodeMutationRequest,
    ) -> Result<NodeDeletionProjection> {
        self.require_logical_request(request)?;
        let kind = match (&self.wire.mutation, &self.wire.before, &self.wire.after) {
            (NodeMutation::Unlink, Snapshot::File { .. }, Snapshot::Missing) => {
                super::NodeKind::File
            }
            (
                NodeMutation::RemoveDirectory,
                Snapshot::Directory { children: 0, .. },
                Snapshot::Missing,
            ) => super::NodeKind::Directory,
            (NodeMutation::RemoveDirectory, Snapshot::Missing, Snapshot::Missing)
                if self.wire.target_kind == Some(TargetKind::Directory) =>
            {
                super::NodeKind::Directory
            }
            _ => return Err(invalid("删除发布要求 unlink 或空目录 rmdir 的原始计划")),
        };
        if self.context.expected_version.is_none() {
            return Err(invalid("删除发布缺少原逻辑版本"));
        }
        Ok(NodeDeletionProjection {
            kind,
            ancestors: self.canonical_ancestors()?,
            before: self
                .wire
                .target_attributes
                .as_ref()
                .ok_or(invalid("删除发布缺少原逻辑属性"))?
                .attributes()?,
            parent_after: self.logical_parent_attributes_after()?,
        })
    }
    /// Bind the logical entry and direct data object to the retained copy.
    /// Actual source and S4 checks also verify the plan's data-object proof.
    pub(in crate::managed_catalog) fn validate_source_copy(
        &self,
        copy: &super::PhysicalCopy,
    ) -> Result<()> {
        source_copy::validate_copy(&self.wire, copy)
    }

    /// Check logical inputs for a fresh attempt. This is not a lease or write
    /// permit. Recovery must keep its original saved physical plan and owner.
    pub fn validate_prepared_inputs(
        &self,
        inputs: &super::PreparedNodeMutationInputs,
    ) -> Result<()> {
        let c = &self.context;
        let b = &inputs.baseline;
        let request = &inputs.request;
        let encoded_request = request.encode()?;
        let object = super::NodePlanObject::for_bytes(
            &b.library_serial,
            b.namespace,
            &encoded_request,
            encoded_request.len(),
        )?;
        if inputs.task.state != super::NodeMutationTaskState::Prepared
            || inputs.task.fingerprint != request.fingerprint()
            || inputs.task.object != object
            || inputs.task.library_serial != b.library_serial
            || inputs.task.namespace != b.namespace
            || inputs.task.pool != b.pool
            || inputs.task.request_epoch != b.request_epoch
            || inputs.task.operation != b.operation
            || !request.matches_baseline(b)?
            || c.library_serial != b.library_serial
            || c.namespace != b.namespace
            || c.pool != b.pool
            || c.owner_epoch != b.owner_epoch
            || c.operation != b.operation
            || c.request_epoch != b.request_epoch
            || c.parent != b.parent.id
            || c.parent_version != b.parent.version
            || c.node != b.node
            || c.expected_version != request.context().expected_version
            || c.path != b.path
            || self.wire.mutation != *request.mutation()
            || self.wire.time != request.time()
            || self.wire.ancestors.len() != inputs.ancestors.len()
        {
            return Err(invalid("物理计划与 Prepared 逻辑输入作用域不符"));
        }
        for (planned, input) in self.wire.ancestors.iter().zip(&inputs.ancestors) {
            if planned.path != input.path
                || id(&planned.inode)? != input.id
                || planned.version != input.version
                || planned.logical.attributes()?.digest() != input.attributes.digest()
            {
                return Err(invalid("物理计划的规范祖先与 Prepared 输入不符"));
            }
        }
        let target = match &b.current {
            None => None,
            Some(super::ResolvedPath::Directory { attributes, .. }) => attributes
                .as_ref()
                .map(|attrs| (TargetKind::Directory, attrs.digest())),
            Some(super::ResolvedPath::File { attributes, .. }) => attributes
                .as_ref()
                .map(|attrs| (TargetKind::File, attrs.digest())),
        };
        if self
            .wire
            .target_attributes
            .as_ref()
            .map(|attributes| {
                attributes
                    .attributes()
                    .map(|attributes| (self.wire.target_kind, attributes.digest()))
            })
            .transpose()?
            != target.map(|(kind, digest)| (Some(kind), digest))
        {
            return Err(invalid("物理计划的规范目标属性与 Prepared 输入不符"));
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        canonical(&self.wire)
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(DOMAIN);
        hash.update(self.encode().expect("validated node plan"));
        hash.finalize().into()
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let wire: Wire =
            serde_json::from_slice(bytes).map_err(|_| invalid("节点变更计划格式非法"))?;
        if canonical(&wire)? != bytes {
            return Err(invalid("节点变更计划须为规范 JSON"));
        }
        let context = NodeMutationContext {
            library_serial: wire.library.clone(),
            namespace: id(&wire.namespace)?,
            pool: id(&wire.pool)?,
            operation: id(&wire.operation)?,
            request_epoch: wire.request_epoch,
            owner_epoch: wire.owner_epoch,
            parent: id(&wire.parent)?,
            parent_version: wire.parent_version,
            node: id(&wire.node)?,
            expected_version: wire.expected_version,
            scan: id(&wire.scan)?,
            previous: ActiveIndexIdentity {
                volume_uuid: id(&wire.volume)?,
                generation: wire.generation,
                location: crate::ltfs::index::IndexLocation {
                    partition: wire.partition,
                    start_block: wire.block,
                },
                sha256: wire
                    .sha256
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("节点变更来源摘要长度非法"))?,
                incremental: wire.incremental,
            },
            path: wire.path.clone(),
        };
        let plan = Self { context, wire };
        plan.validate()?;
        Ok(plan)
    }
    fn validate(&self) -> Result<()> {
        let c = &self.context;
        let w = &self.wire;
        super::control::validate_physical_serial(&c.library_serial)?;
        for value in [
            c.namespace,
            c.pool,
            c.operation,
            c.parent,
            c.node,
            c.scan,
            c.previous.volume_uuid,
        ] {
            if value.is_nil() {
                return Err(invalid("节点变更身份不能为 nil"));
            }
        }
        if w.version != 5
            || c.request_epoch == 0
            || c.owner_epoch == 0
            || c.parent == c.node
            || c.parent_version == 0
            || (c.parent_version == u64::MAX && w.mutation.changes_parent_attributes())
            || c.expected_version == Some(0)
            || c.expected_version == Some(u64::MAX)
            || c.previous.generation == 0
            || c.previous.generation == u64::MAX
            || c.previous.location.start_block == 0
            || !c.previous.location.partition.is_ascii_lowercase()
            || w.highest_uid == 0
            || w.tree_before.len() != 32
            || w.tree_after.len() != 32
            || w.sha256.len() != 32
        {
            return Err(invalid("节点变更版本、卷位置或计数非法"));
        }
        let creating = matches!(
            w.mutation,
            NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. }
        );
        if creating != c.expected_version.is_none() {
            return Err(invalid("节点变更创建/现有版本条件不符"));
        }
        projection::validate(self)?;
        source_copy::validate(w)?;
        let (after, parent, highest) = project(w)?;
        if after != w.after
            || parent != w.parent_after
            || highest != w.next_uid
            || w.parent_before
                .as_ref()
                .is_some_and(|meta| meta.file_uid == 0)
        {
            return Err(invalid("节点变更投影与声明操作不符"));
        }
        for node in [&w.before, &w.after] {
            if let Snapshot::File { node } = node
                && (node.name != path_parts(&w.path)?.1
                    || node.delta_fields.is_some()
                    || node.open_for_write.unwrap_or(false)
                    || (node.symlink.is_some() && (!node.extents.is_empty() || node.length != 0)))
            {
                return Err(invalid("节点变更文件快照非法"));
            }
            if let Some(attributes) = node.attributes()? {
                let meta = attributes.meta().expect("complete attributes");
                if meta.file_uid > w.next_uid {
                    return Err(invalid("节点 UID 超过索引最高 UID"));
                }
                let mut keys = std::collections::BTreeSet::new();
                for attr in attributes.xattrs().expect("complete attributes") {
                    if !keys.insert(&attr.key) {
                        return Err(invalid("节点包含重复扩展属性"));
                    }
                }
            }
        }
        Ok(())
    }
    fn require_source_header(&self, index: &LtfsIndex) -> Result<()> {
        source_copy::require(index, &self.wire)?;
        let markers: Vec<_> = index
            .root
            .xattrs
            .iter()
            .filter(|a| a.key == MUTATION_MARKER)
            .collect();
        if markers.len() > 1
            || markers.first().is_some_and(|marker| {
                marker.base64
                    || marker.value.len() != 66
                    || !marker.value.starts_with("1:")
                    || !marker.value.as_bytes()[2..]
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            })
        {
            return Err(invalid("来源根目录的节点变更标记损坏"));
        }
        let c = &self.context;
        let w = &self.wire;
        let (parent, name) = path_parts(&c.path)?;
        let dir = index.find_directory(parent);
        projection::require_source(index, &w.ancestors)?;
        if index.volume_uuid != c.previous.volume_uuid
            || index.generation != c.previous.generation
            || index.self_location != c.previous.location
            || index.incremental != c.previous.incremental
            || index.highest_file_uid != w.highest_uid
            || dir.map(|dir| &dir.meta) != w.parent_before.as_ref()
            || dir
                .map(|dir| snapshot(dir, name))
                .transpose()?
                .unwrap_or(Snapshot::Missing)
                != w.before
        {
            return Err(invalid("节点变更计划与当前卷、父目录或完整来源树不符"));
        }
        Ok(())
    }
    fn require_source(&self, index: &LtfsIndex) -> Result<()> {
        self.require_source_header(index)?;
        if tree_digest(index, None)?.as_slice() != self.wire.tree_before
            || tree_digest(index, Some(&self.wire))?.as_slice() != self.wire.tree_after
        {
            return Err(invalid("节点变更完整来源树或投影摘要不符"));
        }
        Ok(())
    }
    pub(crate) fn stage(&self, index: &mut LtfsIndex) -> Result<()> {
        self.require_source(index)?;
        let ee_attributes = if stamps_ee_directory(&self.wire) {
            let dir = index
                .find_directory("/.LTFSEE_DATA")
                .ok_or(invalid("EE 数据来源缺少根内部目录"))?;
            Some(super::ee_internal::stamp(&dir.xattrs)?)
        } else {
            None
        };
        projection::stage(index, &self.wire);
        if let Some(attributes) = ee_attributes {
            index
                .root
                .subdirs
                .iter_mut()
                .find(|dir| dir.name == super::ee_internal::DIRECTORY)
                .expect("validated EE source directory")
                .xattrs = attributes;
        }
        let (parent_path, name) = path_parts(&self.context.path)?;
        let mut parent = &mut index.root;
        for part in parent_path
            .trim_start_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
        {
            parent = parent
                .subdirs
                .iter_mut()
                .find(|d| d.name == part)
                .expect("validated parent");
        }
        match &self.wire.after {
            Snapshot::Missing => {
                parent.files.retain(|f| f.name != name);
                parent.subdirs.retain(|d| d.name != name);
            }
            Snapshot::File { node } => {
                if let Some(old) = parent.files.iter_mut().find(|f| f.name == name) {
                    *old = node.clone();
                } else {
                    parent.files.push(node.clone());
                }
            }
            Snapshot::Directory { meta, xattrs, .. } => {
                if let Some(old) = parent.subdirs.iter_mut().find(|d| d.name == name) {
                    old.meta = meta.clone();
                    old.xattrs = xattrs.clone();
                } else {
                    parent.subdirs.push(DirectoryNode {
                        name: name.into(),
                        meta: meta.clone(),
                        xattrs: xattrs.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        parent.meta = self.wire.parent_after.clone();
        index.highest_file_uid = self.wire.next_uid;
        index.root.xattrs.retain(|a| a.key != MUTATION_MARKER);
        index.root.xattrs.push(Xattr {
            key: MUTATION_MARKER.into(),
            value: self.marker(),
            base64: false,
        });
        Ok(())
    }
    fn marker(&self) -> String {
        let hex: String = self
            .fingerprint()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("1:{hex}")
    }
    pub(crate) fn evidence(
        &self,
        index: &LtfsIndex,
        current: ActiveIndexIdentity,
    ) -> Result<NodeMutationEvidence> {
        let before = self.context.previous;
        let markers: Vec<_> = index
            .root
            .xattrs
            .iter()
            .filter(|a| a.key == MUTATION_MARKER)
            .collect();
        if current.volume_uuid != before.volume_uuid
            || current.generation != before.generation + 1
            || current.location.partition != before.location.partition
            || current.location.start_block <= before.location.start_block
            || index.volume_uuid != current.volume_uuid
            || index.generation != current.generation
            || index.self_location != current.location
            || markers.len() != 1
            || markers[0].base64
            || markers[0].value != self.marker()
            || index.highest_file_uid != self.wire.next_uid
        {
            return Err(invalid("节点变更 S4、请求标记或完整结果树不符"));
        }
        source_copy::require(index, &self.wire)?;
        let (digest, file_count) = tree_summary(index, None)?;
        if digest.as_slice() != self.wire.tree_after {
            return Err(invalid("节点变更 S4、请求标记或完整结果树不符"));
        }
        Ok(NodeMutationEvidence {
            context: self.context.clone(),
            current,
            fingerprint: self.fingerprint(),
            file_count,
            attributes: self.wire.after.attributes()?,
            parent_meta: self.wire.parent_after.clone(),
            logical_attributes: self.logical_attributes_after()?,
            logical_parent_attributes: self.logical_parent_attributes_after()?,
            ancestors: self
                .wire
                .ancestors
                .iter()
                .map(|ancestor| {
                    Ok(NodeMutationAncestorEvidence {
                        path: ancestor.path.clone(),
                        id: id(&ancestor.inode)?,
                        version_before: ancestor.version,
                        physical_uid: ancestor.physical_uid,
                        newly_materialized: ancestor.before_meta.is_none(),
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

// Hash semantic nodes, independent of XML sibling order and delta navigation
// fields. The virtual overlay computes the expected tree without cloning it.
fn tree_digest(index: &LtfsIndex, overlay: Option<&Wire>) -> Result<[u8; 32]> {
    tree_summary(index, overlay).map(|(digest, _)| digest)
}
fn tree_summary(index: &LtfsIndex, overlay: Option<&Wire>) -> Result<([u8; 32], u64)> {
    tree_summary_with_root(index, overlay, None)
}
fn tree_summary_with_root(
    index: &LtfsIndex,
    overlay: Option<&Wire>,
    root_xattrs: Option<&[Xattr]>,
) -> Result<([u8; 32], u64)> {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/node-mutation-tree/1");
    hash.update(index.volume_uuid.as_bytes());
    hash.update(
        overlay
            .map_or(index.highest_file_uid, |w| w.next_uid)
            .to_be_bytes(),
    );
    let count = hash_directory(
        &mut hash,
        Some(&index.root),
        "/",
        root_xattrs.map(|attrs| (&index.root.meta, attrs)),
        overlay,
    )?;
    Ok((hash.finalize().into(), count))
}
fn text(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
fn node_meta(
    hash: &mut Sha256,
    meta: &NodeMeta,
    xattrs: &[Xattr],
    symlink: Option<&str>,
    root: bool,
) -> Result<()> {
    let attrs: Vec<_> = xattrs
        .iter()
        .filter(|a| !root || a.key != MUTATION_MARKER)
        .cloned()
        .collect();
    let attributes = NodeAttributes::new(meta.clone(), attrs, symlink.map(str::to_owned))?;
    hash.update(attributes.digest());
    Ok(())
}
fn stamps_ee_directory(wire: &Wire) -> bool {
    matches!(wire.mutation, NodeMutation::Unlink) && wire.source_data.is_some()
}

fn hash_file(hash: &mut Sha256, node: &FileNode) -> Result<()> {
    super::ee_internal::require_location(&node.xattrs, "", NodeKind::File)?;
    hash.update([1]);
    text(hash, node.name.as_bytes());
    node_meta(
        hash,
        &node.meta,
        &node.xattrs,
        node.symlink.as_deref(),
        false,
    )?;
    hash.update(node.length.to_be_bytes());
    hash.update([u8::from(node.open_for_write.unwrap_or(false))]);
    hash.update((node.extents.len() as u64).to_be_bytes());
    for extent in &node.extents {
        text(hash, &canonical(extent)?);
    }
    Ok(())
}
enum Entry<'a> {
    File(&'a FileNode),
    Directory(&'a DirectoryNode),
    Planned(&'a Snapshot),
    Ancestor,
}
fn directory_frame<'a>(
    hash: &mut Sha256,
    dir: Option<&'a DirectoryNode>,
    path: String,
    planned: Option<(&'a NodeMeta, &'a [Xattr])>,
    overlay: Option<&'a Wire>,
) -> Result<DirectoryFrame<'a>> {
    let canonical = overlay.and_then(|wire| projection::at(wire, &path));
    let canonical_attrs = canonical.map(|ancestor| {
        (
            projection::meta(overlay.expect("canonical overlay"), ancestor),
            ancestor.xattrs(),
        )
    });
    let (meta, xattrs) = if let Some((meta, xattrs)) = &canonical_attrs {
        (meta, xattrs.as_slice())
    } else if let Some(planned) = planned {
        planned
    } else {
        let dir = dir.expect("real directory");
        (&dir.meta, dir.xattrs.as_slice())
    };
    super::ee_internal::require_location(xattrs, &path, NodeKind::Directory)?;
    let ee_attributes = if path == "/.LTFSEE_DATA" && overlay.is_some_and(stamps_ee_directory) {
        Some(super::ee_internal::stamp(xattrs)?)
    } else {
        None
    };
    let xattrs = ee_attributes.as_deref().unwrap_or(xattrs);
    let parent = overlay
        .map(|w| path_parts(&w.path))
        .transpose()?
        .map(|v| v.0);
    let meta = if parent == Some(path.as_str()) {
        &overlay.expect("overlay").parent_after
    } else {
        meta
    };
    hash.update([0]);
    text(hash, path.as_bytes());
    node_meta(hash, meta, xattrs, None, path == "/")?;
    let mut entries = BTreeMap::new();
    if let Some(dir) = dir {
        for file in &dir.files {
            if entries
                .insert(file.name.as_str(), Entry::File(file))
                .is_some()
            {
                return Err(invalid("目录包含重复节点"));
            }
        }
        for child in &dir.subdirs {
            if entries
                .insert(child.name.as_str(), Entry::Directory(child))
                .is_some()
            {
                return Err(invalid("目录包含重复节点"));
            }
        }
    }
    if let Some(ancestor) = overlay.and_then(|wire| projection::next_missing(wire, &path)) {
        let (_, name) = path_parts(&ancestor.path)?;
        if entries.insert(name, Entry::Ancestor).is_some() {
            return Err(invalid("待物化祖先的物理路径已被占用"));
        }
    }
    if parent == Some(path.as_str()) {
        let wire = overlay.expect("overlay");
        let (_, name) = path_parts(&wire.path)?;
        if wire.after == Snapshot::Missing {
            entries.remove(name);
        } else {
            entries.insert(name, Entry::Planned(&wire.after));
        }
    }
    hash.update((entries.len() as u64).to_be_bytes());
    Ok(DirectoryFrame {
        dir,
        path,
        entries: entries.into_iter(),
    })
}

struct DirectoryFrame<'a> {
    dir: Option<&'a DirectoryNode>,
    path: String,
    entries: std::collections::btree_map::IntoIter<&'a str, Entry<'a>>,
}

fn hash_directory<'a>(
    hash: &mut Sha256,
    dir: Option<&'a DirectoryNode>,
    path: &str,
    planned: Option<(&'a NodeMeta, &'a [Xattr])>,
    overlay: Option<&'a Wire>,
) -> Result<u64> {
    let mut file_count = 0u64;
    let mut stack = vec![directory_frame(hash, dir, path.into(), planned, overlay)?];
    while let Some(frame) = stack.last_mut() {
        let Some((name, entry)) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        if matches!(name, "" | "." | "..")
            || name.contains('/')
            || !crate::ltfs::xml_text_valid(name)
        {
            return Err(invalid("索引节点名称非法"));
        }
        match entry {
            Entry::File(file) | Entry::Planned(Snapshot::File { node: file }) => {
                hash_file(hash, file)?;
                file_count = file_count.checked_add(1).ok_or(invalid("索引文件数溢出"))?;
            }
            Entry::Directory(_) | Entry::Ancestor | Entry::Planned(Snapshot::Directory { .. }) => {
                let path = if frame.path == "/" {
                    format!("/{name}")
                } else {
                    format!("{}/{name}", frame.path)
                };
                let (source, planned) = match entry {
                    Entry::Directory(child) => (Some(child), None),
                    Entry::Ancestor => (None, None),
                    Entry::Planned(Snapshot::Directory { meta, xattrs, .. }) => (
                        frame
                            .dir
                            .and_then(|dir| dir.subdirs.iter().find(|child| child.name == name)),
                        Some((meta, xattrs.as_slice())),
                    ),
                    _ => unreachable!("directory entry"),
                };
                stack.push(directory_frame(hash, source, path, planned, overlay)?);
            }
            Entry::Planned(Snapshot::Missing) => unreachable!("missing entries removed"),
        }
    }
    Ok(file_count)
}
