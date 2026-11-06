//! Bind canonical logical attributes to real per-volume identities without
//! changing the source tree used by recovery's before digest.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LogicalAttributes {
    pub meta: NodeMeta,
    pub xattrs: Vec<Xattr>,
    pub symlink: Option<String>,
}

impl LogicalAttributes {
    pub(super) fn attributes(&self) -> Result<NodeAttributes> {
        NodeAttributes::new(self.meta.clone(), self.xattrs.clone(), self.symlink.clone())
    }
    fn from_attributes(attributes: &NodeAttributes) -> Result<Self> {
        let value = Self {
            meta: attributes
                .meta()
                .ok_or(invalid("规范属性必须完整读取"))?
                .clone(),
            xattrs: attributes
                .xattrs()
                .ok_or(invalid("规范属性必须完整读取"))?
                .to_vec(),
            symlink: attributes.symlink().map(str::to_owned),
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        self.attributes()?;
        let mut keys = std::collections::BTreeSet::new();
        if self
            .xattrs
            .iter()
            .any(|attr| attr.key == MUTATION_MARKER || !keys.insert(&attr.key))
        {
            return Err(invalid("规范属性含节点私有标记或重复属性"));
        }
        Ok(())
    }
    fn physical_meta(&self, uid: u64) -> NodeMeta {
        NodeMeta {
            file_uid: uid,
            ..self.meta.clone()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ancestor {
    pub path: String,
    pub inode: String,
    pub version: u64,
    pub before_meta: Option<NodeMeta>,
    pub physical_uid: u64,
    pub before_xattrs: Vec<Xattr>,
    pub logical: LogicalAttributes,
}

impl Ancestor {
    pub(super) fn meta(&self) -> NodeMeta {
        if self.path == "/" {
            return self.before_meta.clone().expect("validated physical root");
        }
        self.logical.physical_meta(self.physical_uid)
    }
    pub(super) fn xattrs(&self) -> Vec<Xattr> {
        if self.path != "/" {
            return self.logical.xattrs.clone();
        }
        // A pool's virtual root is not an alias of any volume root. Keep the
        // volume's intrinsic/private metadata; bind logical root attributes in
        // the plan for logical publication. Only the mutation's own marker is
        // replaced separately after staging.
        self.before_xattrs.clone()
    }
}

fn paths(path: &str) -> Result<Vec<String>> {
    let (parent, _) = path_parts(path)?;
    let mut paths = vec!["/".into()];
    let mut current = String::new();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        paths.push(current.clone());
    }
    Ok(paths)
}

fn child_source<'a>(
    parent: Option<&'a DirectoryNode>,
    name: &str,
) -> Result<Option<&'a DirectoryNode>> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    if parent.files.iter().any(|file| file.name == name) {
        return Err(invalid("祖先物理路径是文件或链接"));
    }
    let mut matches = parent.subdirs.iter().filter(|child| child.name == name);
    let child = matches.next();
    if matches.next().is_some() {
        return Err(invalid("祖先物理目录存在重复名称"));
    }
    Ok(child)
}

pub(super) fn capture(
    context: &NodeMutationContext,
    index: &LtfsIndex,
    canonical: &[super::super::NodeMutationAncestor],
    target: Option<&NodeAttributes>,
) -> Result<(Vec<Ancestor>, Option<LogicalAttributes>)> {
    let expected = paths(&context.path)?;
    if canonical.len() != expected.len() {
        return Err(invalid("规范祖先链必须完整覆盖池根到父目录"));
    }
    let mut ancestors = Vec::with_capacity(canonical.len());
    let mut allocated = index.highest_file_uid;
    let mut source = Some(&index.root);
    for (offset, (input, path)) in canonical.iter().zip(&expected).enumerate() {
        if &input.path != path {
            return Err(invalid("规范祖先路径或顺序不符"));
        }
        if offset > 0 {
            source = child_source(source, path_parts(path)?.1)?;
        }
        let physical_uid = if let Some(source) = source {
            source.meta.file_uid
        } else {
            allocated = allocated
                .checked_add(1)
                .ok_or(invalid("物化祖先的 LTFS fileuid 已耗尽"))?;
            allocated
        };
        ancestors.push(Ancestor {
            path: path.clone(),
            inode: input.id.to_string(),
            version: input.version,
            before_meta: source.map(|source| source.meta.clone()),
            physical_uid,
            before_xattrs: source
                .map(|source| source.xattrs.clone())
                .unwrap_or_default(),
            logical: LogicalAttributes::from_attributes(&input.attributes)?,
        });
    }
    let logical = target.map(LogicalAttributes::from_attributes).transpose()?;
    validate_chain(context, &ancestors, index.highest_file_uid)?;
    Ok((ancestors, logical))
}

fn validate_chain(
    context: &NodeMutationContext,
    ancestors: &[Ancestor],
    highest: u64,
) -> Result<()> {
    let expected = paths(&context.path)?;
    if ancestors.len() != expected.len() {
        return Err(invalid("规范祖先链长度不符"));
    }
    let mut inodes = std::collections::BTreeSet::new();
    let mut physical_uids = std::collections::BTreeSet::new();
    let mut missing = false;
    let mut allocated = highest;
    for (ancestor, path) in ancestors.iter().zip(&expected) {
        let inode = id(&ancestor.inode)?;
        if &ancestor.path != path
            || inode.is_nil()
            || inode == context.node
            || !inodes.insert(inode)
            || ancestor.version == 0
            || ancestor.physical_uid == 0
            || !physical_uids.insert(ancestor.physical_uid)
            || ancestor.logical.symlink.is_some()
        {
            return Err(invalid("规范祖先路径、类型、版本或身份非法"));
        }
        ancestor.logical.validate()?;
        let mut keys = std::collections::BTreeSet::new();
        if ancestor
            .before_xattrs
            .iter()
            .any(|attribute| !keys.insert(&attribute.key))
        {
            return Err(invalid("祖先物理来源含重复属性"));
        }
        if let Some(meta) = &ancestor.before_meta {
            if missing || meta.file_uid != ancestor.physical_uid || meta.file_uid > highest {
                return Err(invalid("祖先物理来源 UID 或缺失前缀不符"));
            }
            NodeAttributes::new(meta.clone(), ancestor.before_xattrs.clone(), None)?;
        } else {
            missing = true;
            allocated = allocated
                .checked_add(1)
                .ok_or(invalid("物化祖先的 LTFS fileuid 已耗尽"))?;
            if ancestor.path == "/"
                || ancestor.physical_uid != allocated
                || !ancestor.before_xattrs.is_empty()
            {
                return Err(invalid("缺失祖先必须按最高 UID 顺序物化"));
            }
        }
    }
    let last = ancestors.last().ok_or(invalid("规范祖先链为空"))?;
    if id(&last.inode)? != context.parent || last.version != context.parent_version {
        return Err(invalid("规范父目录身份或版本与请求不符"));
    }
    Ok(())
}

pub(super) fn validate(plan: &NodeMutationPlan) -> Result<()> {
    validate_chain(&plan.context, &plan.wire.ancestors, plan.wire.highest_uid)?;
    let last = plan.wire.ancestors.last().expect("validated chain");
    if last.before_meta != plan.wire.parent_before {
        return Err(invalid("规范祖先物理来源与父目录或最高 UID 不符"));
    }
    if last.before_meta.is_none() && plan.wire.before != Snapshot::Missing {
        return Err(invalid("缺失物理父目录不能包含既有目标"));
    }
    if let Some(attributes) = &plan.wire.target_attributes {
        attributes.validate()?;
    }
    if plan.wire.target_kind.is_some() != plan.wire.target_attributes.is_some()
        || plan.context.expected_version.is_some() != plan.wire.target_kind.is_some()
    {
        return Err(invalid("规范目标类型、属性与原逻辑版本条件不符"));
    }
    target(&plan.wire)?;
    Ok(())
}

pub(super) fn target(wire: &Wire) -> Result<Snapshot> {
    match (&wire.before, wire.target_kind, &wire.target_attributes) {
        (Snapshot::Missing, None, None) => Ok(Snapshot::Missing),
        (Snapshot::Missing, Some(TargetKind::Directory), Some(attributes))
            if attributes.symlink.is_none() =>
        {
            Ok(Snapshot::Directory {
                meta: attributes.meta.clone(),
                xattrs: attributes.xattrs.clone(),
                children: 0,
            })
        }
        (
            Snapshot::Directory { meta, children, .. },
            Some(TargetKind::Directory),
            Some(attributes),
        ) if attributes.symlink.is_none() => Ok(Snapshot::Directory {
            meta: attributes.physical_meta(meta.file_uid),
            xattrs: attributes.xattrs.clone(),
            children: *children,
        }),
        (Snapshot::File { node }, Some(TargetKind::File), Some(attributes))
            if source_copy::logical_link_matches(wire, node, attributes)? =>
        {
            let mut after = node.clone();
            after.meta = attributes.physical_meta(node.meta.file_uid);
            after.xattrs = attributes.xattrs.clone();
            Ok(Snapshot::File { node: after })
        }
        _ => Err(invalid("规范目标属性缺失或目标类型、链接内容不符")),
    }
}

pub(super) fn at<'a>(wire: &'a Wire, path: &str) -> Option<&'a Ancestor> {
    wire.ancestors
        .binary_search_by(|ancestor| ancestor.path.as_str().cmp(path))
        .ok()
        .map(|index| &wire.ancestors[index])
}

pub(super) fn next_missing<'a>(wire: &'a Wire, path: &str) -> Option<&'a Ancestor> {
    let index = wire
        .ancestors
        .binary_search_by(|ancestor| ancestor.path.as_str().cmp(path))
        .ok()?;
    wire.ancestors
        .get(index + 1)
        .filter(|ancestor| ancestor.before_meta.is_none())
}

pub(super) fn highest_uid(wire: &Wire) -> u64 {
    wire.ancestors
        .iter()
        .map(|ancestor| ancestor.physical_uid)
        .fold(wire.highest_uid, u64::max)
}

pub(super) fn meta(wire: &Wire, ancestor: &Ancestor) -> NodeMeta {
    let mut meta = ancestor.meta();
    if ancestor.path == "/" && next_missing(wire, "/").is_some() {
        meta.modify_time = wire.time.clone();
        meta.change_time = wire.time.clone();
    }
    meta
}

pub(super) fn require_source(index: &LtfsIndex, ancestors: &[Ancestor]) -> Result<()> {
    let mut source = Some(&index.root);
    for (offset, ancestor) in ancestors.iter().enumerate() {
        if offset > 0 {
            source = child_source(source, path_parts(&ancestor.path)?.1)?;
        }
        if source.map(|source| &source.meta) != ancestor.before_meta.as_ref()
            || source.is_some_and(|source| source.xattrs != ancestor.before_xattrs)
        {
            return Err(invalid("规范祖先真实来源或缺失状态与计划不符"));
        }
    }
    Ok(())
}

pub(super) fn stage(index: &mut LtfsIndex, wire: &Wire) {
    let mut directory = &mut index.root;
    for (offset, ancestor) in wire.ancestors.iter().enumerate() {
        let meta = meta(wire, ancestor);
        let xattrs = ancestor.xattrs();
        if offset == 0 {
            directory.meta = meta;
            directory.xattrs = xattrs;
        } else {
            let name = path_parts(&ancestor.path)
                .expect("validated ancestor path")
                .1;
            if ancestor.before_meta.is_none() {
                directory.subdirs.push(DirectoryNode {
                    name: name.into(),
                    meta,
                    xattrs,
                    ..Default::default()
                });
                directory = directory.subdirs.last_mut().expect("new ancestor");
            } else {
                directory = directory
                    .subdirs
                    .iter_mut()
                    .find(|child| child.name == name)
                    .expect("validated ancestor");
                directory.meta = meta;
                directory.xattrs = xattrs;
            }
        }
    }
}
