//! Atomic directory visibility and task completion after a typed node S4.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationPublication {
    attempt: NodeMutationAttempt,
    current: ActiveIndexIdentity,
    node: Uuid,
    version: u64,
    attributes: [u8; 32],
    parent_version: u64,
    parent_attributes: [u8; 32],
    copy: Option<(Uuid, PhysicalCopy)>,
    owner: u64,
    completed_at_millis: u64,
}
impl NodeMutationPublication {
    pub fn attempt(&self) -> &NodeMutationAttempt {
        &self.attempt
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn node(&self) -> Uuid {
        self.node
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn parent_version(&self) -> u64 {
        self.parent_version
    }
    pub fn attributes_digest(&self) -> [u8; 32] {
        self.attributes
    }
    pub fn completed_owner(&self) -> u64 {
        self.owner
    }
    pub fn is_directory(&self) -> bool {
        self.copy.is_none()
    }
    /// The immutable description of existing data or a native link.
    /// This is not a new upload and carries no upload receipt.
    pub fn physical_copy(&self) -> Option<(Uuid, &PhysicalCopy)> {
        self.copy.as_ref().map(|(id, copy)| (*id, copy))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCopy {
    id: String,
    record: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    kind: String,
    attempt: String,
    current: String,
    node: String,
    node_version: u64,
    attributes: [u8; 32],
    parent_version: u64,
    parent_attributes: [u8; 32],
    copy: Option<WireCopy>,
    owner: u64,
    completed_at_millis: u64,
}
fn key(namespace: Uuid, library: &str, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'r');
    append_component(&mut key, library.as_bytes());
    key.extend_from_slice(&epoch.to_be_bytes());
    key.extend_from_slice(operation.as_bytes());
    key
}
fn copy_id(attempt: &NodeMutationAttempt, node: Uuid) -> Uuid {
    let hash = Sha256::new()
        .chain_update(b"tape-rs/node-publication-copy/1")
        .chain_update(attempt.namespace.as_bytes())
        .chain_update(attempt.epoch.to_be_bytes())
        .chain_update(attempt.operation.as_bytes())
        .chain_update(attempt.id.as_bytes())
        .chain_update(attempt.object.digest())
        .chain_update(node.as_bytes())
        .finalize();
    let mut bytes: [u8; 16] = hash[..16].try_into().expect("SHA prefix");
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Uuid::from_bytes(bytes)
}

fn projected_copy(
    attempt: &NodeMutationAttempt,
    node: Uuid,
    version: u64,
    current: ActiveIndexIdentity,
) -> Result<Option<(Uuid, PhysicalCopy)>> {
    let Some((previous_id, source)) = &attempt.source_copy else {
        return Ok(None);
    };
    if source.version.checked_add(1) != Some(version)
        || source.volume_id != current.volume_uuid
        || source.volume_scan_id != attempt.scan
        || source.volume_generation > attempt.previous.generation
    {
        return Err(CatalogError::Corrupt("节点属性发布与原始数据副本身份不符"));
    }
    let id = copy_id(attempt, node);
    if id == *previous_id {
        return Err(CatalogError::Corrupt("节点属性发布复用了原副本身份"));
    }
    let mut copy = source.clone();
    copy.version = version;
    // The immutable body remains read from its original confirmed source.
    // The new metadata S4 is retained independently in the publication result.
    // origin and media.version describe the original content. Neither
    // is replaced by this independent management request's epoch or owner.
    Ok(Some((id, copy)))
}

fn plan_copy(
    attempt: &NodeMutationAttempt,
    plan: &NodeMutationPlan,
    projection: &node_mutation::NodePublicationProjection,
    current: ActiveIndexIdentity,
) -> Result<Option<(Uuid, PhysicalCopy)>> {
    let c = plan.context();
    let version = c.expected_version.unwrap_or(0) + 1;
    if attempt.source_copy.is_some() || projection.kind == NodeKind::Directory {
        return projected_copy(attempt, c.node, version, current);
    }
    if c.expected_version.is_some()
        || !matches!(
            plan.mutation(),
            node_mutation::NodeMutation::CreateSymlink { .. }
        )
        || projection.after.link_kind()? != crate::managed_catalog::link_kind::LinkKind::Native
    {
        return Err(CatalogError::Invalid("新文件发布要求原生符号链接创建计划"));
    }
    let uid = projection
        .after
        .meta()
        .ok_or(CatalogError::Incomplete)?
        .file_uid;
    Ok(Some((
        copy_id(attempt, c.node),
        PhysicalCopy {
            version,
            origin: CopyOrigin::NodeLink {
                management_epoch: attempt.epoch,
                operation: attempt.operation,
            },
            volume_id: current.volume_uuid,
            volume_generation: current.generation,
            volume_scan_id: attempt.scan,
            ltfs_file_uid: uid,
            size: 0,
            content_digest: ContentDigest::Unknown,
            media: Some(MediaProvenance {
                version: (0, 0),
                logical_file_uid: uid,
                physical_path: c.path.trim_start_matches('/').into(),
                ee_alias: false,
                native_symlink: true,
            }),
        },
    )))
}

/// Retained results are checked without the live plan payload. The completed
/// task's digest binds these original immutable fields; fresh/retry publication
/// additionally compares the exact path and assigned UID against the plan.
fn copy_matches(value: &NodeMutationPublication) -> Result<bool> {
    if value.attempt.source_copy.is_some() || value.copy.is_none() {
        return Ok(
            value.copy == projected_copy(&value.attempt, value.node, value.version, value.current)?
        );
    }
    let (id, copy) = value.copy.as_ref().expect("new native link");
    Ok(value.version == 1
        && copy.version == 1
        && *id == copy_id(&value.attempt, value.node)
        && copy.origin
            == (CopyOrigin::NodeLink {
                management_epoch: value.attempt.epoch,
                operation: value.attempt.operation,
            })
        && copy.volume_id == value.current.volume_uuid
        && copy.volume_generation == value.current.generation
        && copy.volume_scan_id == value.attempt.scan
        && copy.size == 0
        && copy.content_digest == ContentDigest::Unknown
        && copy.media.as_ref().is_some_and(|media| {
            media.native_symlink
                && !media.ee_alias
                && media.version == (0, 0)
                && media.logical_file_uid == copy.ltfs_file_uid
        }))
}

fn encode(value: &NodeMutationPublication) -> Result<Vec<u8>> {
    let before = value.attempt.previous;
    if value.current.volume_uuid != before.volume_uuid
        || before.generation.checked_add(1) != Some(value.current.generation)
        || value.current.location.partition != 'b'
        || value.current.location.start_block <= before.location.start_block
        || !copy_matches(value)?
        || value.node.is_nil()
        || value.version == 0
        || value.parent_version == 0
        || value.owner < value.attempt.owner
        || !(value.attempt.created_at_millis..=253_402_300_799_999)
            .contains(&value.completed_at_millis)
    {
        return Err(CatalogError::Corrupt("节点发布结果身份、版本或时间不符"));
    }
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(before.volume_uuid, &current)?;
    let bytes = node_mutation::canonical(&Wire {
        version: 1,
        kind: if value.copy.is_none() {
            "directory"
        } else {
            "file"
        }
        .into(),
        attempt: URL_SAFE_NO_PAD.encode(super::encode(&value.attempt)?),
        current: URL_SAFE_NO_PAD.encode(current),
        node: value.node.to_string(),
        node_version: value.version,
        attributes: value.attributes,
        parent_version: value.parent_version,
        parent_attributes: value.parent_attributes,
        copy: value
            .copy
            .as_ref()
            .map(|(id, copy)| -> Result<WireCopy> {
                Ok(WireCopy {
                    id: id.to_string(),
                    record: URL_SAFE_NO_PAD.encode(encode_physical_copy(copy)?),
                })
            })
            .transpose()?,
        owner: value.owner,
        completed_at_millis: value.completed_at_millis,
    })?;
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点发布结果过长"));
    }
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-mutation-publication/1")
        .chain_update(bytes)
        .finalize()
        .into()
}
fn decode(bytes: &[u8]) -> Result<NodeMutationPublication> {
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点发布结果过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点发布结果编码"))?;
    let raw = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| CatalogError::Corrupt("节点发布二进制字段"))
    };
    let attempt = super::decode(&raw(&wire.attempt)?)?;
    let value = NodeMutationPublication {
        current: active_volume::decode_index(attempt.previous.volume_uuid, &raw(&wire.current)?)?,
        attempt,
        node: node_mutation::id(&wire.node).map_err(|_| CatalogError::Corrupt("节点发布 inode"))?,
        version: wire.node_version,
        attributes: wire.attributes,
        parent_version: wire.parent_version,
        parent_attributes: wire.parent_attributes,
        copy: wire
            .copy
            .map(|copy| -> Result<(Uuid, PhysicalCopy)> {
                Ok((
                    node_mutation::id(&copy.id)?,
                    decode_copy(&raw(&copy.record)?)?,
                ))
            })
            .transpose()?,
        owner: wire.owner,
        completed_at_millis: wire.completed_at_millis,
    };
    let kind = if value.copy.is_none() {
        "directory"
    } else {
        "file"
    };
    if wire.version != 1 || wire.kind != kind || encode(&value)? != bytes {
        return Err(CatalogError::Corrupt("节点发布结果须为规范类型版本1"));
    }
    Ok(value)
}
pub(super) async fn at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<NodeMutationPublication>> {
    let Some(raw) = tr
        .get(&key(namespace, serial, epoch, operation), false)
        .await?
    else {
        return Ok(None);
    };
    let value = decode(&raw)?;
    if value.attempt.namespace != namespace
        || value.attempt.library != serial
        || value.attempt.epoch != epoch
        || value.attempt.operation != operation
    {
        return Err(CatalogError::Corrupt("节点发布结果与请求键不符"));
    }
    Ok(Some(value))
}

// This is a retained operation result, not a current media read plan. Its
// digest is anchored in the atomically completed task; no live task attempt,
// retained plan payload or current Serving volume is required by the result.
pub(super) fn require_task_match(
    value: &NodeMutationPublication,
    task: &NodeMutationTask,
) -> Result<()> {
    if task.state != NodeMutationTaskState::Completed
        || value.attempt.pool != task.pool
        || value.attempt.logical_fingerprint != task.fingerprint
        || task.completed_owner_epoch != Some(value.owner)
        || task.completed_at_millis != Some(value.completed_at_millis)
        || task.publication_digest != Some(digest(&encode(value)?))
    {
        return Err(CatalogError::Corrupt("节点终态与不可变发布结果不符"));
    }
    Ok(())
}

fn require_match(
    value: &NodeMutationPublication,
    attempt: &NodeMutationAttempt,
    plan: &NodeMutationPlan,
    projection: &node_mutation::NodePublicationProjection,
) -> Result<()> {
    let c = plan.context();
    let parent_version = c
        .parent_version
        .checked_add(u64::from(plan.mutation().changes_parent_attributes()))
        .ok_or(CatalogError::Invalid("目录父版本溢出"))?;
    if value.attempt != *attempt
        || value.node != c.node
        || value.version != c.expected_version.unwrap_or(0) + 1
        || value.attributes != projection.after.digest()
        || value.parent_version != parent_version
        || value.parent_attributes != projection.parent_after.digest()
        || value.copy != plan_copy(attempt, plan, projection, value.current)?
        || (projection.kind == NodeKind::Directory) != value.copy.is_none()
    {
        return Err(CatalogError::Invalid("节点发布重试与原结果不符"));
    }
    Ok(())
}

pub(super) async fn require_writing_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    attempt: &NodeMutationAttempt,
    plan: &NodeMutationPlan,
    request: &NodeMutationRequest,
    object: &NodePlanObject,
) -> Result<(NodeMutationTask, observation::NodeMutationObservation)> {
    let task = current_task_at(tr, route, owner, plan).await?;
    if task.state != NodeMutationTaskState::Writing
        || task.fingerprint != request.fingerprint()
        || task.object != *object
        || task.fingerprint != attempt.logical_fingerprint
    {
        return Err(CatalogError::Invalid("节点发布要求原 Writing 请求"));
    }
    if require_task_at(tr, &task).await? != *attempt {
        return Err(CatalogError::Invalid("节点发布的物理尝试已改变"));
    }
    let saved = observation::at(tr, attempt)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if active_volume::history_at(
        tr,
        &attempt.library,
        saved.current().volume_uuid,
        saved.current().generation,
    )
    .await?
        != (
            active_volume::IndexKind::NodeMutation(attempt.id),
            saved.current(),
        )
        || active_volume::observed_at(tr, &attempt.library, saved.current().volume_uuid)
            .await?
            .1
            != saved.current()
    {
        return Err(CatalogError::Corrupt("节点发布的节点 S4 与活动游标不符"));
    }
    node_plan_objects::require_sealed_at(tr, object).await?;
    Ok((task, saved))
}

pub(super) async fn require_inputs_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    request: &NodeMutationRequest,
    ancestors: &[NodeMutationAncestor],
    before: Option<&NodeAttributes>,
) -> Result<NodeMutationBaseline> {
    let mut baseline = node_request::revalidate_at(tr, route, owner, request).await?;
    if baseline
        .parent
        .attributes
        .as_ref()
        .map(NodeAttributes::digest)
        != ancestors.last().map(|a| a.attributes.digest())
    {
        return Err(CatalogError::Invalid("节点发布的父属性已改变"));
    }
    baseline.parent.attributes = ancestors.last().map(|a| a.attributes.clone());
    match (&mut baseline.current, before) {
        (None, None) => {}
        (
            Some(
                ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. },
            ),
            Some(before),
        ) if attributes.as_ref().map(NodeAttributes::digest) == Some(before.digest()) => {
            *attributes = Some(before.clone());
        }
        _ => return Err(CatalogError::Invalid("节点发布目标类型或原属性不符")),
    }
    node_inputs::require_logical_inputs_at(tr, route, owner, request, &baseline, ancestors).await?;
    Ok(baseline)
}

impl ManagedCatalog {
    pub async fn node_mutation_publication(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeMutationPublication>> {
        if epoch == 0 || operation.is_nil() {
            return Err(CatalogError::Invalid("节点发布请求身份无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        node_tasks::retained(&tr, namespace, epoch).await?;
        node_tasks::task_at(&tr, serial, namespace, epoch, operation).await?;
        at(&tr, namespace, serial, epoch, operation).await
    }

    /// Publish directories, native links and existing-node attributes after an
    /// observed S4. Deletions require their own retained deletion model.
    pub async fn publish_node_mutation_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
        plan: &NodeMutationPlan,
        request: &NodeMutationRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationPublication> {
        if !attempt.matches_plan(plan) || request.fingerprint() != attempt.logical_fingerprint {
            return Err(CatalogError::Invalid("节点发布与原计划或请求不符"));
        }
        if plan.encode()?.len() > max_bytes {
            return Err(CatalogError::Invalid("节点发布计划超过字节预算"));
        }
        let projection = plan.publication_projection(request)?;
        let object = NodePlanObject::for_bytes(
            &attempt.library,
            attempt.namespace,
            &request.encode()?,
            max_bytes,
        )?;
        let tr = self.db.create_trx()?;
        let task = current_task_at(&tr, route, owner, plan).await?;
        if task.fingerprint != request.fingerprint() || task.object != object {
            return Err(CatalogError::Invalid("节点发布逻辑请求已改变"));
        }
        if let Some(saved) = at(
            &tr,
            attempt.namespace,
            &attempt.library,
            attempt.epoch,
            attempt.operation,
        )
        .await?
        {
            require_match(&saved, attempt, plan, &projection)?;
            return Ok(saved);
        }
        require_writing_at(&tr, route, owner, attempt, plan, request, &object).await?;
        require_inputs_at(
            &tr,
            route,
            owner,
            request,
            &projection.ancestors,
            projection.before.as_ref(),
        )
        .await?;
        drop(tr);
        self.upload_attributes(attempt.namespace, &projection.after)
            .await?;
        if plan.mutation().changes_parent_attributes() {
            self.upload_attributes(attempt.namespace, &projection.parent_after)
                .await?;
        }
        self.db
            .run(|tr, _| {
                let projection = &projection;
                let object = &object;
                async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                512_000,
                            ))?;
                            current_task_at(&tr, route, owner, plan).await?;
                            if let Some(saved) = at(
                                &tr,
                                attempt.namespace,
                                &attempt.library,
                                attempt.epoch,
                                attempt.operation,
                            )
                            .await?
                            {
                                require_match(&saved, attempt, plan, projection)?;
                                return Ok(saved);
                            }
                            let (task, observed) = require_writing_at(
                                &tr, route, owner, attempt, plan, request, object,
                            )
                            .await?;
                            let baseline = require_inputs_at(
                                &tr,
                                route,
                                owner,
                                request,
                                &projection.ancestors,
                                projection.before.as_ref(),
                            )
                            .await?;
                            attribute_objects::require_attributes(
                                &tr,
                                attempt.namespace,
                                Some(&projection.after),
                            )
                            .await?;
                            attribute_objects::require_attributes(
                                &tr,
                                attempt.namespace,
                                Some(&projection.parent_after),
                            )
                            .await?;
                            let c = plan.context();
                            let parent_version = c
                                .parent_version
                                .checked_add(u64::from(plan.mutation().changes_parent_attributes()))
                                .ok_or(CatalogError::Invalid("目录父版本溢出"))?;
                            let version = c.expected_version.unwrap_or(0) + 1;
                            let name = c
                                .path
                                .rsplit('/')
                                .next()
                                .expect("validated canonical path")
                                .as_bytes()
                                .to_vec();
                            let node = encode_node(&Node {
                                kind: projection.kind,
                                directory_version: (projection.kind == NodeKind::Directory)
                                    .then_some(version),
                                parent: c.parent,
                                name: name.clone(),
                                attributes: Some(projection.after.clone()),
                            });
                            let parent = if plan.mutation().changes_parent_attributes() {
                                let raw = tr
                                    .get(&node_key(attempt.namespace, c.parent), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                let mut node = decode_node(&raw)?;
                                if node.directory_version != Some(baseline.parent.version)
                                    || node.kind != NodeKind::Directory
                                {
                                    return Err(CatalogError::Corrupt(
                                        "节点发布父节点与原版本不符",
                                    ));
                                }
                                node.directory_version = Some(parent_version);
                                node.attributes = Some(projection.parent_after.clone());
                                Some(encode_node(&node))
                            } else {
                                None
                            };
                            let copy = plan_copy(attempt, plan, projection, observed.current())?;
                            if (projection.kind == NodeKind::Directory) != copy.is_none() {
                                return Err(CatalogError::Corrupt("节点发布类型与原始副本不符"));
                            }
                            let file_rows = if let Some((id, copy)) = &copy {
                                match (&baseline.current, &attempt.source_copy) {
                                    (
                                        Some(ResolvedPath::File {
                                            head, copy: before, ..
                                        }),
                                        Some((id, source)),
                                    ) if *id == head.copy_id && source == before.as_ref() => {
                                        plan.validate_source_copy(before)?;
                                    }
                                    (None, None)
                                        if matches!(
                                            plan.mutation(),
                                            node_mutation::NodeMutation::CreateSymlink { .. }
                                        ) => {}
                                    _ => {
                                        return Err(CatalogError::Invalid(
                                            "文件发布的原副本或创建基线已改变",
                                        ));
                                    }
                                }
                                let target = copy_key(attempt.namespace, c.node, *id);
                                let reverse = crate::managed_catalog::volume_key(
                                    copy.volume_id,
                                    copy.volume_scan_id,
                                    attempt.namespace,
                                    c.node,
                                    *id,
                                );
                                if tr.get(&target, false).await?.is_some()
                                    || tr.get(&reverse, false).await?.is_some()
                                {
                                    return Err(CatalogError::Corrupt(
                                        "节点属性发布存在孤立目标副本或反向记录",
                                    ));
                                }
                                let encoded = encode_physical_copy(copy)?;
                                let mut volume_value = version.to_be_bytes().to_vec();
                                volume_value.extend_from_slice(&Sha256::digest(&encoded));
                                Some((
                                    target,
                                    encoded,
                                    reverse,
                                    volume_value,
                                    encode_head(&FileHead {
                                        version,
                                        size: copy.size,
                                        copy_id: *id,
                                    }),
                                ))
                            } else {
                                None
                            };
                            let value = NodeMutationPublication {
                                attempt: attempt.clone(),
                                current: observed.current(),
                                node: c.node,
                                version,
                                attributes: projection.after.digest(),
                                parent_version,
                                parent_attributes: projection.parent_after.digest(),
                                copy,
                                owner,
                                completed_at_millis: management::now()
                                    .max(observed.observed_at_millis()),
                            };
                            let encoded = encode(&value)?;
                            let result_key = key(
                                attempt.namespace,
                                &attempt.library,
                                attempt.epoch,
                                attempt.operation,
                            );
                            // complete_at encodes before its first set; only infallible
                            // canonical/result writes and hold release follow it.
                            node_tasks::complete_at(
                                &tr,
                                &task,
                                attempt.id,
                                owner,
                                value.completed_at_millis,
                                digest(&encoded),
                            )?;
                            tr.set(&node_key(attempt.namespace, c.node), &node);
                            if let Some((target, record, reverse, volume_value, head)) = file_rows {
                                tr.set(&target, &record);
                                tr.set(&reverse, &volume_value);
                                tr.set(&head_key(attempt.namespace, c.node), &head);
                            }
                            tr.set(
                                &child_key(attempt.namespace, c.parent, &name),
                                c.node.as_bytes(),
                            );
                            if let Some(parent) = parent {
                                tr.set(&node_key(attempt.namespace, c.parent), &parent);
                            }
                            tr.set(&result_key, &encoded);
                            tr.clear(&volume_key(c.previous.volume_uuid));
                            namespace_visibility::publish(&tr, attempt.namespace);
                            Ok(value)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
