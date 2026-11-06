//! Logical canonical inputs captured under an existing Prepared pool hold.
//! This never rewrites a physical source tree or grants permission to write it.
use super::*;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationAncestor {
    /// Pool-relative path; the virtual pool root is `/`.
    pub path: String,
    pub id: Uuid,
    pub version: u64,
    /// Logical metadata UID; it is not a per-volume physical UID assertion.
    pub attributes: NodeAttributes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedNodeMutationInputs {
    pub task: NodeMutationTask,
    pub request: NodeMutationRequest,
    /// Parent and existing target attributes are fully loaded. The baseline's
    /// owner is current; the request and original preparation remain unchanged.
    pub baseline: NodeMutationBaseline,
    /// Exactly root-to-parent, excluding the global namespace root.
    pub ancestors: Vec<NodeMutationAncestor>,
}

pub(super) fn charge(used: &mut usize, amount: usize, limit: usize) -> Result<()> {
    *used = used
        .checked_add(amount)
        .filter(|sum| *sum <= limit)
        .ok_or(CatalogError::Invalid("节点输入超过属性字节预算"))?;
    Ok(())
}

pub(super) fn charge_attributes(
    used: &mut usize,
    attributes: &NodeAttributes,
    limit: usize,
) -> Result<()> {
    let len = attributes
        .bytes()
        .map(<[u8]>::len)
        .or_else(|| attributes.manifest().map(|manifest| manifest.len as usize))
        .ok_or(CatalogError::Corrupt("节点属性没有内容或清单"))?;
    charge(used, len, limit)
}

pub(super) async fn ancestor_at(
    tr: &Transaction,
    namespace: Uuid,
    id: Uuid,
    parent: Uuid,
    name: &[u8],
    path: String,
) -> Result<NodeMutationAncestor> {
    let bytes = tr
        .get(&node_key(namespace, id), false)
        .await?
        .ok_or(CatalogError::Corrupt("节点逻辑祖先缺失"))?;
    let node = decode_node(&bytes)?;
    if node.kind != NodeKind::Directory || node.parent != parent || node.name != name {
        return Err(CatalogError::Corrupt("节点逻辑祖先身份或类型不符"));
    }
    Ok(NodeMutationAncestor {
        path,
        id,
        version: node
            .directory_version
            .ok_or(CatalogError::Corrupt("节点逻辑祖先版本缺失"))?,
        attributes: node
            .attributes
            .ok_or(CatalogError::Corrupt("节点逻辑祖先属性缺失"))?,
    })
}

pub(super) async fn require_inputs_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    inputs: &PreparedNodeMutationInputs,
) -> Result<()> {
    node_tasks::require_prepared_at(tr, route, owner, &inputs.task, &inputs.request).await?;
    node_plan_objects::require_sealed_at(tr, &inputs.task.object).await?;
    require_logical_inputs_at(
        tr,
        route,
        owner,
        &inputs.request,
        &inputs.baseline,
        &inputs.ancestors,
    )
    .await
}

/// Shared canonical checks for admission and an already admitted publication.
/// Task state/holds are validated by the caller in this same transaction.
pub(super) async fn require_logical_inputs_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    request: &NodeMutationRequest,
    baseline: &NodeMutationBaseline,
    ancestors: &[NodeMutationAncestor],
) -> Result<()> {
    if baseline.owner_epoch != owner || !request.matches_baseline(baseline)? {
        return Err(CatalogError::Invalid(
            "节点规范输入与原请求或当前 owner 不符",
        ));
    }
    let current = node_request::revalidate_at(tr, route, owner, request).await?;
    attribute_objects::require_attributes(
        tr,
        current.namespace,
        current.parent.attributes.as_ref(),
    )
    .await?;
    if let Some(
        ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. },
    ) = &current.current
    {
        attribute_objects::require_attributes(tr, current.namespace, attributes.as_ref()).await?;
    }

    require_ancestor_chain_at(
        tr,
        baseline.namespace,
        baseline.pool,
        &baseline.path,
        &baseline.parent,
        ancestors,
    )
    .await?;
    if let Some(target) = &baseline.current {
        let directory = matches!(target, ResolvedPath::Directory { .. });
        let (ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. }) =
            target;
        require_full_attributes(
            attributes
                .as_ref()
                .ok_or(CatalogError::Invalid("节点规范目标属性缺失"))?,
            directory,
        )?;
    }
    Ok(())
}

pub(super) async fn require_ancestor_chain_at(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
    path: &str,
    expected_parent: &NodeMutationParent,
    ancestors: &[NodeMutationAncestor],
) -> Result<()> {
    let components = lookup::path_components(path.as_bytes())?;
    let (_, parents) = components
        .split_last()
        .ok_or(CatalogError::Invalid("节点规范输入缺少目标路径"))?;
    if ancestors.len() != parents.len() + 1 {
        return Err(CatalogError::Invalid("节点规范输入祖先链不完整"));
    }
    let pool_name = pool.to_string();
    let root = active_pools::root_id(namespace, pool);
    let mut seen = HashSet::new();
    let mut parent = Uuid::nil();
    let mut path = String::new();
    for (offset, saved) in ancestors.iter().enumerate() {
        let name = if offset == 0 {
            pool_name.as_bytes()
        } else {
            path.push('/');
            path.push_str(
                std::str::from_utf8(parents[offset - 1])
                    .map_err(|_| CatalogError::Invalid("节点规范祖先路径不是 UTF-8"))?,
            );
            parents[offset - 1]
        };
        let expected_path = if offset == 0 { "/" } else { path.as_str() };
        let id = uuid_from(
            &tr.get(&child_key(namespace, parent, name), false)
                .await?
                .ok_or(CatalogError::Corrupt("节点规范祖先目录项缺失"))?,
        )?;
        if id.is_nil()
            || !seen.insert(id)
            || (offset == 0 && id != root)
            || saved.path != expected_path
            || saved.id != id
        {
            return Err(CatalogError::Invalid("节点规范祖先路径、身份或顺序已改变"));
        }
        require_full_attributes(&saved.attributes, true)?;
        let current = ancestor_at(tr, namespace, id, parent, name, expected_path.into()).await?;
        attribute_objects::require_attributes(tr, namespace, Some(&current.attributes)).await?;
        if current.version != saved.version
            || current.attributes.digest() != saved.attributes.digest()
        {
            return Err(CatalogError::Invalid("节点规范祖先版本或属性已改变"));
        }
        if tr.get(&head_key(namespace, id), false).await?.is_some() {
            return Err(CatalogError::Corrupt("节点规范祖先目录存在文件 head"));
        }
        parent = id;
    }
    let last = ancestors.last().expect("validated complete chain");
    let parent_attributes = expected_parent
        .attributes
        .as_ref()
        .ok_or(CatalogError::Invalid("节点规范父目录属性缺失"))?;
    require_full_attributes(parent_attributes, true)?;
    if last.id != expected_parent.id
        || last.version != expected_parent.version
        || last.attributes.digest() != parent_attributes.digest()
    {
        return Err(CatalogError::Invalid("节点规范祖先与原父目录前提不符"));
    }
    Ok(())
}

pub(super) async fn capture_ancestor_chain_at(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
    path: &str,
    expected_parent: &NodeMutationParent,
    budget: (&mut usize, usize),
) -> Result<Vec<NodeMutationAncestor>> {
    let (used, max_bytes) = budget;
    let root = active_pools::root_id(namespace, pool);
    let mut ancestors = vec![
        ancestor_at(
            tr,
            namespace,
            root,
            Uuid::nil(),
            pool.to_string().as_bytes(),
            "/".into(),
        )
        .await?,
    ];
    charge(used, ancestors[0].path.len() + 24, max_bytes)?;
    charge_attributes(used, &ancestors[0].attributes, max_bytes)?;
    let (parent_path, _) = path
        .rsplit_once('/')
        .ok_or(CatalogError::Corrupt("节点输入路径没有父目录"))?;
    let mut seen = HashSet::from([root]);
    let mut parent = root;
    let mut path = String::new();
    for name in parent_path.split('/').filter(|name| !name.is_empty()) {
        let id = uuid_from(
            &tr.get(&child_key(namespace, parent, name.as_bytes()), false)
                .await?
                .ok_or(CatalogError::Corrupt("节点逻辑祖先目录项缺失"))?,
        )?;
        if id.is_nil() || !seen.insert(id) {
            return Err(CatalogError::Corrupt("节点逻辑祖先含重复 inode"));
        }
        path.push('/');
        path.push_str(name);
        let ancestor =
            ancestor_at(tr, namespace, id, parent, name.as_bytes(), path.clone()).await?;
        charge(used, ancestor.path.len() + 24, max_bytes)?;
        charge_attributes(used, &ancestor.attributes, max_bytes)?;
        ancestors.push(ancestor);
        parent = id;
    }
    let last = ancestors.last().expect("pool root ancestor");
    if last.id != expected_parent.id
        || last.version != expected_parent.version
        || Some(last.attributes.digest())
            != expected_parent
                .attributes
                .as_ref()
                .map(NodeAttributes::digest)
    {
        return Err(CatalogError::Corrupt("节点祖先链与原父目录前提不符"));
    }
    Ok(ancestors)
}

pub(super) fn require_full_attributes(attributes: &NodeAttributes, directory: bool) -> Result<()> {
    if attributes.meta().is_none()
        || attributes.xattrs().is_none()
        || (directory && attributes.symlink().is_some())
    {
        return Err(CatalogError::Invalid(
            "节点规范属性未完整读取或目录类型不符",
        ));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Assemble the physical plan from one checked Prepared snapshot and the
    /// selected medium's real source. This grants no write permission: Writing
    /// admission must repeat the checks and durably claim the volume.
    pub(crate) async fn prepare_node_mutation_plan(
        &self,
        route: ActiveLibraryRoute,
        inputs: &PreparedNodeMutationInputs,
        source: ActiveIndexIdentity,
        index: &crate::ltfs::index::LtfsIndex,
    ) -> Result<NodeMutationPlan> {
        let baseline = &inputs.baseline;
        let tr = self.db.create_trx()?;
        require_inputs_at(&tr, route, baseline.owner_epoch, inputs).await?;
        let placement = verified_placement(&tr, source.volume_uuid)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != baseline.namespace
            || placement.pool_id != baseline.pool
            || placement.library_serial != baseline.library_serial
        {
            return Err(CatalogError::Invalid("节点计划介质不属于当前带库和池"));
        }
        let assignment =
            Self::verified_assignment(&tr, &baseline.library_serial, &placement.barcode)
                .await?
                .ok_or(CatalogError::Incomplete)?;
        if !assignment.allows_io() {
            return Err(CatalogError::MediaUnavailable);
        }
        let (scan, current) =
            active_volume::observed_at(&tr, &baseline.library_serial, source.volume_uuid).await?;
        if current != source {
            return Err(CatalogError::Invalid("节点计划实际源与已观察索引不符"));
        }
        let target = match &baseline.current {
            None => None,
            Some(ResolvedPath::Directory { attributes, .. }) => attributes
                .as_ref()
                .map(|attrs| (NodeKind::Directory, attrs)),
            Some(ResolvedPath::File {
                copy, attributes, ..
            }) => {
                if copy.volume_id != source.volume_uuid || copy.volume_scan_id != scan.id {
                    return Err(CatalogError::Invalid("节点现有文件只能在当前主副本卷执行"));
                }
                attributes.as_ref().map(|attrs| (NodeKind::File, attrs))
            }
        };
        drop(tr);
        let plan = NodeMutationPlan::prepare(
            NodeMutationContext {
                library_serial: baseline.library_serial.clone(),
                namespace: baseline.namespace,
                pool: baseline.pool,
                operation: baseline.operation,
                request_epoch: baseline.request_epoch,
                owner_epoch: baseline.owner_epoch,
                parent: baseline.parent.id,
                parent_version: baseline.parent.version,
                node: baseline.node,
                expected_version: inputs.request.context().expected_version,
                scan: scan.id,
                previous: source,
                path: baseline.path.clone(),
            },
            inputs.request.mutation().clone(),
            index,
            inputs.request.time(),
            &inputs.ancestors,
            target,
        )?;
        plan.validate_prepared_inputs(inputs)?;
        if let Some(ResolvedPath::File { copy, .. }) = &baseline.current {
            plan.validate_source_copy(copy)?;
        }
        Ok(plan)
    }

    /// Recheck captured logical inputs at one read version. This is not a
    /// persistent admission or physical write permit. Admission must use the
    /// same transaction for these reads and its durable claims.
    pub async fn check_prepared_node_inputs(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        inputs: &PreparedNodeMutationInputs,
    ) -> Result<()> {
        require_inputs_at(&self.db.create_trx()?, route, owner, inputs).await
    }

    /// Capture logical inputs only. The caller must later bind real per-volume
    /// source trees/UIDs in a physical attempt and revalidate before Writing.
    /// `max_bytes` bounds the original request plus encoded attribute contents
    /// and ancestor identity/path bytes. External values are charged before I/O.
    pub async fn capture_prepared_node_inputs(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<PreparedNodeMutationInputs> {
        let (task, request) = self
            .node_mutation_task_request(serial, route, owner, epoch, operation, max_bytes)
            .await?;
        let mut used = request.encode()?.len();
        let tr = self.db.create_trx()?;
        node_tasks::require_prepared_at(&tr, route, owner, &task, &request).await?;
        let mut baseline = node_request::revalidate_at(&tr, route, owner, &request).await?;
        let namespace = baseline.namespace;
        let mut ancestors = capture_ancestor_chain_at(
            &tr,
            namespace,
            baseline.pool,
            &baseline.path,
            &baseline.parent,
            (&mut used, max_bytes),
        )
        .await?;
        if let Some(current) = &baseline.current {
            let attributes = match current {
                ResolvedPath::Directory { attributes, .. }
                | ResolvedPath::File { attributes, .. } => attributes,
            }
            .as_ref()
            .ok_or(CatalogError::Corrupt("节点逻辑目标属性缺失"))?;
            charge_attributes(&mut used, attributes, max_bytes)?;
        }
        drop(tr);
        // The pool hold keeps these immutable references stable across short
        // attribute reads. Check current ownership and the same hold each time.
        for ancestor in &mut ancestors {
            node_tasks::require_prepared_at(&self.db.create_trx()?, route, owner, &task, &request)
                .await?;
            ancestor.attributes = self
                .hydrate_attributes(namespace, ancestor.attributes.clone())
                .await?;
            if ancestor.attributes.symlink().is_some() {
                return Err(CatalogError::Corrupt("节点逻辑祖先属性是链接"));
            }
        }
        baseline.parent.attributes = Some(
            ancestors
                .last()
                .expect("pool root ancestor")
                .attributes
                .clone(),
        );
        if let Some(current) = &mut baseline.current {
            let directory = matches!(current, ResolvedPath::Directory { .. });
            let (ResolvedPath::Directory { attributes, .. }
            | ResolvedPath::File { attributes, .. }) = current;
            node_tasks::require_prepared_at(&self.db.create_trx()?, route, owner, &task, &request)
                .await?;
            let value = self
                .hydrate_attributes(
                    namespace,
                    attributes.take().expect("validated target attributes"),
                )
                .await?;
            if directory && value.symlink().is_some() {
                return Err(CatalogError::Corrupt("节点目录目标属性是链接"));
            }
            *attributes = Some(value);
        }
        let inputs = PreparedNodeMutationInputs {
            task,
            request,
            baseline,
            ancestors,
        };
        self.check_prepared_node_inputs(route, owner, &inputs)
            .await?;
        Ok(inputs)
    }
}

#[cfg(test)]
#[path = "node_input_tests.rs"]
mod tests;
