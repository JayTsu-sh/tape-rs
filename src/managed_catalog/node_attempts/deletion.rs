use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationDeletion {
    attempt: NodeMutationAttempt,
    current: ActiveIndexIdentity,
    node: Uuid,
    path: String,
    kind: NodeKind,
    version: u64,
    previous_attributes: [u8; 32],
    parent_version: u64,
    parent_attributes: [u8; 32],
    owner: u64,
    completed_at_millis: u64,
}

impl NodeMutationDeletion {
    pub fn attempt(&self) -> &NodeMutationAttempt {
        &self.attempt
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn node(&self) -> Uuid {
        self.node
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn kind(&self) -> NodeKind {
        self.kind
    }
    /// Version of the deletion event, not a new file head or directory inode.
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn parent_version(&self) -> u64 {
        self.parent_version
    }
    pub fn previous_attributes_digest(&self) -> [u8; 32] {
        self.previous_attributes
    }
    pub fn completed_owner(&self) -> u64 {
        self.owner
    }
    pub fn previous_copy(&self) -> Option<(Uuid, &PhysicalCopy)> {
        self.attempt
            .source_copy
            .as_ref()
            .map(|(id, copy)| (*id, copy))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    kind: String,
    attempt: String,
    current: String,
    node: String,
    path: String,
    deleted_version: u64,
    previous_attributes: [u8; 32],
    parent_version: u64,
    parent_attributes: [u8; 32],
    owner: u64,
    completed_at_millis: u64,
}

fn key(namespace: Uuid, serial: &str, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = management::prefix(namespace, b'Q');
    out.push(b'd');
    append_component(&mut out, serial.as_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}

fn previous_head(value: &NodeMutationDeletion) -> Result<Option<FileHead>> {
    match (value.kind, &value.attempt.source_copy) {
        (NodeKind::Directory, None) => Ok(None),
        (NodeKind::File, Some((id, copy)))
            if copy.version.checked_add(1) == Some(value.version)
                && copy.volume_id == value.attempt.previous.volume_uuid
                && copy.volume_scan_id == value.attempt.scan
                && copy.volume_generation <= value.attempt.previous.generation =>
        {
            Ok(Some(FileHead {
                version: copy.version,
                size: copy.size,
                copy_id: *id,
            }))
        }
        _ => Err(CatalogError::Corrupt("节点删除类型、版本与原始副本不符")),
    }
}

fn retirement(
    value: &NodeMutationDeletion,
    result_digest: [u8; 32],
) -> Result<retired_nodes::NodeRetirement> {
    Ok(retired_nodes::NodeRetirement {
        namespace: value.attempt.namespace,
        pool: value.attempt.pool,
        node: value.node,
        path: value.path.clone(),
        kind: value.kind,
        deleted_version: value.version,
        previous_attributes: value.previous_attributes,
        previous_head: previous_head(value)?,
        management_epoch: value.attempt.epoch,
        operation: value.attempt.operation,
        result_digest,
    })
}

fn encode(value: &NodeMutationDeletion) -> Result<Vec<u8>> {
    let before = value.attempt.previous;
    if value.current.volume_uuid != before.volume_uuid
        || before.generation.checked_add(1) != Some(value.current.generation)
        || value.current.location.partition != 'b'
        || value.current.location.start_block <= before.location.start_block
        || value.parent_version < 2
        || value.owner < value.attempt.owner
        || !(value.attempt.created_at_millis..=253_402_300_799_999)
            .contains(&value.completed_at_millis)
    {
        return Err(CatalogError::Corrupt("节点删除结果身份、版本或时间不符"));
    }
    // Validate the compact permanent identity without making its result digest
    // recursively part of the result being hashed.
    retired_nodes::encode(&retirement(value, [0; 32])?)?;
    let mut current = Vec::new();
    active_volume::encode_index(&mut current, value.current);
    active_volume::decode_index(before.volume_uuid, &current)?;
    let bytes = node_mutation::canonical(&Wire {
        version: 1,
        kind: match value.kind {
            NodeKind::Directory => "deleted-directory",
            NodeKind::File => "deleted-file",
        }
        .into(),
        attempt: URL_SAFE_NO_PAD.encode(super::encode(&value.attempt)?),
        current: URL_SAFE_NO_PAD.encode(current),
        node: value.node.to_string(),
        path: value.path.clone(),
        deleted_version: value.version,
        previous_attributes: value.previous_attributes,
        parent_version: value.parent_version,
        parent_attributes: value.parent_attributes,
        owner: value.owner,
        completed_at_millis: value.completed_at_millis,
    })?;
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点删除结果过长"));
    }
    Ok(bytes)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-mutation-deletion/1")
        .chain_update(bytes)
        .finalize()
        .into()
}

fn decode(bytes: &[u8]) -> Result<NodeMutationDeletion> {
    if bytes.len() > 2 * MAX_RECORD {
        return Err(CatalogError::Corrupt("节点删除结果过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点删除结果编码"))?;
    let raw = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| CatalogError::Corrupt("节点删除结果二进制字段"))
    };
    let attempt = super::decode(&raw(&wire.attempt)?)?;
    let value = NodeMutationDeletion {
        current: active_volume::decode_index(attempt.previous.volume_uuid, &raw(&wire.current)?)?,
        attempt,
        node: node_mutation::id(&wire.node)
            .map_err(|_| CatalogError::Corrupt("节点删除结果 inode"))?,
        path: wire.path,
        kind: match wire.kind.as_str() {
            "deleted-directory" => NodeKind::Directory,
            "deleted-file" => NodeKind::File,
            _ => return Err(CatalogError::Corrupt("节点删除结果类型")),
        },
        version: wire.deleted_version,
        previous_attributes: wire.previous_attributes,
        parent_version: wire.parent_version,
        parent_attributes: wire.parent_attributes,
        owner: wire.owner,
        completed_at_millis: wire.completed_at_millis,
    };
    if wire.version != 1 || encode(&value)? != bytes {
        return Err(CatalogError::Corrupt("节点删除结果须为规范版本1"));
    }
    Ok(value)
}

pub(super) async fn at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<NodeMutationDeletion>> {
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
        return Err(CatalogError::Corrupt("节点删除结果与请求键不符"));
    }
    Ok(Some(value))
}

pub(super) fn require_task_match(
    value: &NodeMutationDeletion,
    task: &NodeMutationTask,
) -> Result<()> {
    if task.state != NodeMutationTaskState::Completed
        || value.attempt.pool != task.pool
        || value.attempt.logical_fingerprint != task.fingerprint
        || task.completed_owner_epoch != Some(value.owner)
        || task.completed_at_millis != Some(value.completed_at_millis)
        || task.publication_digest != Some(digest(&encode(value)?))
    {
        return Err(CatalogError::Corrupt("节点终态与不可变删除结果不符"));
    }
    Ok(())
}

fn require_match(
    value: &NodeMutationDeletion,
    attempt: &NodeMutationAttempt,
    plan: &NodeMutationPlan,
    projection: &node_mutation::NodeDeletionProjection,
) -> Result<()> {
    let c = plan.context();
    if value.attempt != *attempt
        || value.node != c.node
        || value.path != c.path
        || value.kind != projection.kind
        || c.expected_version.and_then(|v| v.checked_add(1)) != Some(value.version)
        || value.previous_attributes != projection.before.digest()
        || c.parent_version.checked_add(1) != Some(value.parent_version)
        || value.parent_attributes != projection.parent_after.digest()
    {
        return Err(CatalogError::Invalid("节点删除重试与原结果不符"));
    }
    Ok(())
}

impl ManagedCatalog {
    pub async fn node_mutation_deletion(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeMutationDeletion>> {
        if epoch == 0 || operation.is_nil() {
            return Err(CatalogError::Invalid("节点删除请求身份无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        node_tasks::retained(&tr, namespace, epoch).await?;
        node_tasks::task_at(&tr, serial, namespace, epoch, operation).await?;
        at(&tr, namespace, serial, epoch, operation).await
    }

    /// Publish an observed unlink/rmdir as a negative namespace fact. Old
    /// copy/head records remain historical and are never reattached by retry.
    pub async fn publish_node_mutation_deletion(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
        plan: &NodeMutationPlan,
        request: &NodeMutationRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationDeletion> {
        if !attempt.matches_plan(plan) || request.fingerprint() != attempt.logical_fingerprint {
            return Err(CatalogError::Invalid("节点删除与原计划或请求不符"));
        }
        if plan.encode()?.len() > max_bytes {
            return Err(CatalogError::Invalid("节点删除计划超过字节预算"));
        }
        let projection = plan.deletion_projection(request)?;
        let object = NodePlanObject::for_bytes(
            &attempt.library,
            attempt.namespace,
            &request.encode()?,
            max_bytes,
        )?;
        let tr = self.db.create_trx()?;
        let task = current_task_at(&tr, route, owner, plan).await?;
        if task.fingerprint != request.fingerprint() || task.object != object {
            return Err(CatalogError::Invalid("节点删除逻辑请求已改变"));
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
        publication::require_writing_at(&tr, route, owner, attempt, plan, request, &object).await?;
        publication::require_inputs_at(
            &tr,
            route,
            owner,
            request,
            &projection.ancestors,
            Some(&projection.before),
        )
        .await?;
        drop(tr);
        self.upload_attributes(attempt.namespace, &projection.parent_after)
            .await?;
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
                            let (task, observed) = publication::require_writing_at(
                                &tr, route, owner, attempt, plan, request, object,
                            )
                            .await?;
                            let baseline = publication::require_inputs_at(
                                &tr,
                                route,
                                owner,
                                request,
                                &projection.ancestors,
                                Some(&projection.before),
                            )
                            .await?;
                            attribute_objects::require_attributes(
                                &tr,
                                attempt.namespace,
                                Some(&projection.parent_after),
                            )
                            .await?;
                            let c = plan.context();
                            let version = c
                                .expected_version
                                .and_then(|v| v.checked_add(1))
                                .ok_or(CatalogError::Invalid("节点删除版本溢出或缺失"))?;
                            let parent_version = c
                                .parent_version
                                .checked_add(1)
                                .ok_or(CatalogError::Invalid("节点删除父版本溢出"))?;
                            let previous_head =
                                match (&baseline.current, projection.kind, &attempt.source_copy) {
                                    (
                                        Some(ResolvedPath::Directory { id, version, .. }),
                                        NodeKind::Directory,
                                        None,
                                    ) if *id == c.node && Some(*version) == c.expected_version => {
                                        None
                                    }
                                    (
                                        Some(ResolvedPath::File { id, head, copy, .. }),
                                        NodeKind::File,
                                        Some((copy_id, source)),
                                    ) if *id == c.node
                                        && head.copy_id == *copy_id
                                        && source == copy.as_ref() =>
                                    {
                                        plan.validate_source_copy(copy)?;
                                        Some(head.clone())
                                    }
                                    _ => {
                                        return Err(CatalogError::Invalid(
                                            "节点删除目标或原始副本已改变",
                                        ));
                                    }
                                };
                            let name = c
                                .path
                                .rsplit('/')
                                .next()
                                .expect("validated path")
                                .as_bytes();
                            let child = child_key(attempt.namespace, c.parent, name);
                            let raw = tr
                                .get(&node_key(attempt.namespace, c.node), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let before = decode_node(&raw)?;
                            let head = tr.get(&head_key(attempt.namespace, c.node), false).await?;
                            if before.kind != projection.kind
                                || before.parent != c.parent
                                || before.name != name
                                || before.attributes.as_ref().map(NodeAttributes::digest)
                                    != Some(projection.before.digest())
                                || head.as_deref().map(decode_head).transpose()? != previous_head
                                || tr.get(&child, false).await?.as_deref()
                                    != Some(c.node.as_bytes().as_slice())
                            {
                                return Err(CatalogError::Corrupt(
                                    "节点删除 inode、head 或目录项与原输入不符",
                                ));
                            }
                            let parent = tr
                                .get(&node_key(attempt.namespace, c.parent), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let mut parent = decode_node(&parent)?;
                            if parent.kind != NodeKind::Directory
                                || parent.directory_version != Some(baseline.parent.version)
                            {
                                return Err(CatalogError::Corrupt("节点删除父目录与原版本不符"));
                            }
                            parent.directory_version = Some(parent_version);
                            parent.attributes = Some(projection.parent_after.clone());
                            let parent = encode_node(&parent);
                            let value = NodeMutationDeletion {
                                attempt: attempt.clone(),
                                current: observed.current(),
                                node: c.node,
                                path: c.path.clone(),
                                kind: projection.kind,
                                version,
                                previous_attributes: projection.before.digest(),
                                parent_version,
                                parent_attributes: projection.parent_after.digest(),
                                owner,
                                completed_at_millis: management::now()
                                    .max(observed.observed_at_millis()),
                            };
                            let encoded = encode(&value)?;
                            let result_digest = digest(&encoded);
                            let retirement = retirement(&value, result_digest)?;
                            let retired = retired_nodes::encode(&retirement)?;
                            let (barrier_key, barrier_value) =
                                retired_nodes::prepare_barrier(&tr, &retirement).await?;
                            let result_key = key(
                                attempt.namespace,
                                &attempt.library,
                                attempt.epoch,
                                attempt.operation,
                            );
                            // Every fallible operation precedes complete_at's first set.
                            node_tasks::complete_at(
                                &tr,
                                &task,
                                attempt.id,
                                owner,
                                value.completed_at_millis,
                                result_digest,
                            )?;
                            tr.clear(&child);
                            tr.set(&node_key(attempt.namespace, c.node), &retired);
                            tr.set(&node_key(attempt.namespace, c.parent), &parent);
                            tr.set(&barrier_key, &barrier_value);
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
mod tests {
    use super::*;

    fn fixture() -> NodeMutationDeletion {
        let namespace = Uuid::new_v4();
        let previous = ActiveIndexIdentity {
            volume_uuid: Uuid::new_v4(),
            generation: 2,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 11,
            },
            sha256: [7; 32],
            incremental: false,
        };
        // Inert metadata codec fixture; it cannot mint opaque media evidence.
        NodeMutationDeletion {
            attempt: NodeMutationAttempt {
                id: Uuid::new_v4(),
                library: "DELETE-CODEC".into(),
                namespace,
                pool: Uuid::new_v4(),
                epoch: 9,
                operation: Uuid::new_v4(),
                logical_fingerprint: [5; 32],
                owner: 100,
                created_at_millis: 1_798_000_000_000,
                object: NodePlanObject::for_bytes("DELETE-CODEC", namespace, b"inert", 100)
                    .unwrap(),
                scan: Uuid::new_v4(),
                previous,
                pool_revision: 1,
                assignment_revision: 1,
                source_copy: None,
            },
            current: ActiveIndexIdentity {
                generation: 3,
                location: crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 15,
                },
                sha256: [8; 32],
                ..previous
            },
            node: Uuid::new_v4(),
            path: "/directory".into(),
            kind: NodeKind::Directory,
            version: 4,
            previous_attributes: [3; 32],
            parent_version: 2,
            parent_attributes: [4; 32],
            owner: 101,
            completed_at_millis: 1_798_000_000_001,
        }
    }

    #[test]
    fn deletion_codec_is_self_contained_strict_and_preserves_previous_copy_origin() {
        let directory = fixture();
        let mut file = directory.clone();
        file.kind = NodeKind::File;
        file.path = "/body".into();
        file.attempt.source_copy = Some((
            Uuid::new_v4(),
            PhysicalCopy {
                version: 3,
                origin: CopyOrigin::FileRequest { request_epoch: 1 },
                volume_id: file.attempt.previous.volume_uuid,
                volume_generation: 1,
                volume_scan_id: file.attempt.scan,
                ltfs_file_uid: 19,
                size: 123,
                content_digest: ContentDigest::Sha256([9; 32]),
                media: Some(MediaProvenance {
                    version: (55, 6),
                    logical_file_uid: 19,
                    physical_path: "body".into(),
                    ee_alias: false,
                    native_symlink: false,
                }),
            },
        ));
        for value in [&directory, &file] {
            let bytes = encode(value).unwrap();
            assert_eq!(decode(&bytes).unwrap(), *value);
            for end in 0..bytes.len() {
                assert!(decode(&bytes[..end]).is_err());
            }
            let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            for field in wire.as_object().unwrap().keys() {
                let mut bad = wire.clone();
                bad.as_object_mut().unwrap().remove(field);
                assert!(
                    decode(bad.to_string().as_bytes()).is_err(),
                    "missing {field}"
                );
            }
            for (field, replacement) in [
                ("version", serde_json::json!(0)),
                ("version", serde_json::json!(2)),
                ("kind", serde_json::json!("file")),
                ("deleted_version", serde_json::json!(1)),
                ("parent_version", serde_json::json!(1)),
                ("node", serde_json::json!(Uuid::nil().to_string())),
                ("path", serde_json::json!("/")),
                ("owner", serde_json::json!(99)),
                (
                    "completed_at_millis",
                    serde_json::json!(1_797_999_999_999_u64),
                ),
                ("unknown", serde_json::json!(true)),
            ] {
                let mut bad = wire.clone();
                bad[field] = replacement;
                assert!(decode(bad.to_string().as_bytes()).is_err(), "{field}");
            }
            let retired = retirement(value, digest(&bytes)).unwrap();
            let raw = retired_nodes::encode(&retired).unwrap();
            assert_eq!(retired_nodes::decode(&raw).unwrap(), retired);
            let mut bad = bytes;
            bad.push(b' ');
            assert!(decode(&bad).is_err());
        }
        assert_eq!(
            file.previous_copy().unwrap().1.origin,
            CopyOrigin::FileRequest { request_epoch: 1 }
        );
        let mut bad = file.clone();
        bad.version += 1;
        assert!(encode(&bad).is_err());
        let mut bad = file.clone();
        bad.attempt
            .source_copy
            .as_mut()
            .unwrap()
            .1
            .volume_generation = 3;
        assert!(encode(&bad).is_err());
        let mut bad = file;
        bad.kind = NodeKind::Directory;
        assert!(encode(&bad).is_err());
    }
}
