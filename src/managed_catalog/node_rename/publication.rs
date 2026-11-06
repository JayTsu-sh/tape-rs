//! One retained two-path result, published atomically with the namespace move.
use super::*;
const MAX_PUBLICATION: usize = 64_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplacedNode {
    attributes: [u8; 32],
    head: Option<FileHead>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenamePublication {
    request: NodeRenameRequest,
    group: GroupRecord,
    directory: bool,
    before_copy: Option<(Uuid, PhysicalCopy)>,
    attributes: [u8; 32],
    source_parent_attributes: [u8; 32],
    destination_parent_attributes: [u8; 32],
    replaced: Option<ReplacedNode>,
    owner: u64,
    time: u64,
}
impl NodeRenamePublication {
    pub fn request(&self) -> &NodeRenameRequest {
        &self.request
    }
    pub fn group(&self) -> Uuid {
        record_id(&self.group.id).expect("validated rename group")
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        digest(&encode(self).expect("validated rename result"))
    }
    pub fn source(&self) -> NodeRenameRevision {
        let mut source = self.request.spec().source;
        source.version += 1;
        source
    }
    pub fn source_parent(&self) -> NodeRenameRevision {
        let mut parent = self.request.spec().source_parent;
        parent.version += 1;
        parent
    }
    pub fn destination_parent(&self) -> NodeRenameRevision {
        let mut parent = self.request.spec().destination_parent;
        parent.version += 1;
        parent
    }
    pub fn replaced(&self) -> Option<NodeRenameRevision> {
        self.request.spec().target.map(|mut target| {
            target.version += 1;
            target
        })
    }
    pub fn is_directory(&self) -> bool {
        self.directory
    }
    pub fn attributes_digest(&self) -> [u8; 32] {
        self.attributes
    }
    pub fn source_parent_attributes_digest(&self) -> [u8; 32] {
        self.source_parent_attributes
    }
    pub fn destination_parent_attributes_digest(&self) -> [u8; 32] {
        self.destination_parent_attributes
    }
    pub fn completed_owner(&self) -> u64 {
        self.owner
    }
    pub fn completed_at_millis(&self) -> u64 {
        self.time
    }
    pub fn physical_copy(&self) -> Option<(Uuid, PhysicalCopy)> {
        after_copy(self).expect("validated rename copy")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyWire {
    id: String,
    record: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplacedWire {
    attributes: [u8; 32],
    head: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    request: String,
    group: String,
    directory: bool,
    before_copy: Option<CopyWire>,
    attributes: [u8; 32],
    source_parent_attributes: [u8; 32],
    destination_parent_attributes: [u8; 32],
    replaced: Option<ReplacedWire>,
    owner: u64,
    time: u64,
}
fn key(namespace: Uuid, serial: &str, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut key = management::prefix(namespace, b'Q');
    key.push(b'n');
    append_component(&mut key, serial.as_bytes());
    key.extend_from_slice(&epoch.to_be_bytes());
    key.extend_from_slice(operation.as_bytes());
    key
}
fn digest(raw: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-publication/1")
        .chain_update(raw)
        .finalize()
        .into()
}
fn after_copy(value: &NodeRenamePublication) -> Result<Option<(Uuid, PhysicalCopy)>> {
    let Some((previous, source)) = &value.before_copy else {
        return Ok(None);
    };
    let spec = value.request.spec();
    if previous.is_nil() || source.version != spec.source.version {
        return Err(invalid_attempt());
    }
    let hash = Sha256::new()
        .chain_update(b"tape-rs/node-rename-publication-copy/1")
        .chain_update(spec.source.namespace.as_bytes())
        .chain_update(value.group.fingerprint()?)
        .chain_update(value.request.fingerprint())
        .chain_update(spec.source.inode.as_bytes())
        .finalize();
    let mut bytes: [u8; 16] = hash[..16].try_into().expect("SHA prefix");
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    let id = Uuid::from_bytes(bytes);
    if id == *previous {
        return Err(invalid_attempt());
    }
    let mut copy = source.clone();
    copy.version = value.source().version;
    // Content origin, scan, generation, UID and media provenance stay original.
    encode_physical_copy(&copy)?;
    Ok(Some((id, copy)))
}
fn validate(value: &NodeRenamePublication) -> Result<()> {
    let request = &value.request;
    let spec = request.spec();
    let group = &value.group;
    group.validate()?;
    let completion = group.completion.as_ref().ok_or_else(invalid_attempt)?;
    if group.phase != NodeRenameAttemptPhase::Publishing
        || completion.released_pages != group.pages
        || group.library != request.library()
        || group.namespace != spec.source.namespace.to_string()
        || group.pool != spec.pool.to_string()
        || group.epoch != request.request_epoch()
        || group.operation != spec.operation.to_string()
        || group.logical != request.fingerprint()
        || value.owner < group.owner
        || !owner_valid(value.owner)
        || !(group.created..=MAX_MILLIS).contains(&value.time)
        || value.directory != value.before_copy.is_none()
        || spec.source_parent == spec.destination_parent
            && value.source_parent_attributes != value.destination_parent_attributes
    {
        return Err(invalid_attempt());
    }
    after_copy(value)?;
    match (&value.replaced, spec.target) {
        (None, None) => {}
        (Some(target), Some(original)) if value.directory == target.head.is_none() => {
            if let Some(head) = &target.head {
                decode_head(&encode_head(head))?;
                if head.version != original.version {
                    return Err(invalid_attempt());
                }
            }
        }
        _ => return Err(invalid_attempt()),
    }
    Ok(())
}
fn encode(value: &NodeRenamePublication) -> Result<Vec<u8>> {
    validate(value)?;
    let before_copy = value
        .before_copy
        .as_ref()
        .map(|(id, copy)| -> Result<CopyWire> {
            Ok(CopyWire {
                id: id.to_string(),
                record: URL_SAFE_NO_PAD.encode(encode_physical_copy(copy)?),
            })
        })
        .transpose()?;
    let wire = Wire {
        version: 1,
        request: URL_SAFE_NO_PAD.encode(value.request.encode()?),
        group: URL_SAFE_NO_PAD.encode(record_bytes(&value.group)?),
        directory: value.directory,
        before_copy,
        attributes: value.attributes,
        source_parent_attributes: value.source_parent_attributes,
        destination_parent_attributes: value.destination_parent_attributes,
        replaced: value.replaced.as_ref().map(|r| ReplacedWire {
            attributes: r.attributes,
            head: r
                .head
                .as_ref()
                .map(|h| URL_SAFE_NO_PAD.encode(encode_head(h))),
        }),
        owner: value.owner,
        time: value.time,
    };
    let raw = node_mutation::canonical(&wire)?;
    if raw.len() > MAX_PUBLICATION {
        return Err(invalid("rename 发布结果超过封口预算"));
    }
    Ok(raw)
}
fn decode(raw: &[u8]) -> Result<NodeRenamePublication> {
    if raw.len() > MAX_PUBLICATION {
        return Err(invalid_attempt());
    }
    let wire: Wire = serde_json::from_slice(raw).map_err(|_| invalid_attempt())?;
    if wire.version != 1 {
        return Err(invalid_attempt());
    }
    let bytes = |s: &str| URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid_attempt());
    let before_copy = wire
        .before_copy
        .map(|c| -> Result<_> { Ok((record_id(&c.id)?, decode_copy(&bytes(&c.record)?)?)) })
        .transpose()?;
    let replaced = wire
        .replaced
        .map(|r| -> Result<_> {
            Ok(ReplacedNode {
                attributes: r.attributes,
                head: r.head.map(|h| decode_head(&bytes(&h)?)).transpose()?,
            })
        })
        .transpose()?;
    let value = NodeRenamePublication {
        request: NodeRenameRequest::decode(&bytes(&wire.request)?)?,
        group: GroupRecord::decode(&bytes(&wire.group)?)?,
        directory: wire.directory,
        before_copy,
        attributes: wire.attributes,
        source_parent_attributes: wire.source_parent_attributes,
        destination_parent_attributes: wire.destination_parent_attributes,
        replaced,
        owner: wire.owner,
        time: wire.time,
    };
    if encode(&value)? != raw {
        return Err(invalid_attempt());
    }
    Ok(value)
}
pub(super) async fn at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<NodeRenamePublication>> {
    let Some(raw) = tr
        .get(&key(namespace, serial, epoch, operation), false)
        .await?
    else {
        return Ok(None);
    };
    let value = decode(&raw)?;
    if value.request.library() != serial
        || value.request.spec().source.namespace != namespace
        || value.request.request_epoch() != epoch
        || value.request.spec().operation != operation
    {
        return Err(invalid_attempt());
    }
    Ok(Some(value))
}
pub(super) fn require_task_match(
    value: &NodeRenamePublication,
    task: &NodeMutationTask,
) -> Result<()> {
    let encoded = value.request.encode()?;
    let object = NodePlanObject::for_bytes(
        value.request.library(),
        task.namespace,
        &encoded,
        encoded.len(),
    )?;
    if task.kind != NodeMutationTaskKind::Rename
        || task.state != NodeMutationTaskState::Completed
        || task.library_serial != value.request.library()
        || task.namespace != value.request.spec().source.namespace
        || task.pool != value.request.spec().pool
        || task.request_epoch != value.request.request_epoch()
        || task.operation != value.request.spec().operation
        || task.fingerprint != value.request.fingerprint()
        || task.object != object
        || task.publication_digest != Some(value.fingerprint())
        || task.completed_owner_epoch != Some(value.owner)
        || task.completed_at_millis != Some(value.time)
        || value.group.owner < task.prepared_owner_epoch.ok_or_else(invalid_attempt)?
        || value.group.created < task.prepared_at_millis.ok_or_else(invalid_attempt)?
    {
        return Err(invalid_attempt());
    }
    Ok(())
}
pub(in crate::managed_catalog) async fn require_task_outcome_at(
    tr: &Transaction,
    task: Option<&NodeMutationTask>,
    scope: (Uuid, &str, u64, Uuid),
) -> Result<()> {
    let (namespace, serial, epoch, operation) = scope;
    let result = at(tr, namespace, serial, epoch, operation).await?;
    match (task, result) {
        (Some(task), Some(value))
            if task.kind == NodeMutationTaskKind::Rename
                && task.state == NodeMutationTaskState::Completed =>
        {
            require_task_match(&value, task)
        }
        (Some(task), None)
            if task.kind == NodeMutationTaskKind::Rename
                && task.state == NodeMutationTaskState::Completed =>
        {
            Err(invalid_attempt())
        }
        (_, None) => Ok(()),
        (_, Some(_)) => Err(invalid_attempt()),
    }
}

pub(super) fn require_budget(request: &NodeRenameRequest, source: &ResolvedPath) -> Result<()> {
    let before_copy = match source {
        ResolvedPath::Directory { .. } => None,
        ResolvedPath::File { head, copy, .. } => Some(CopyWire {
            id: head.copy_id.to_string(),
            record: URL_SAFE_NO_PAD.encode(encode_physical_copy(copy)?),
        }),
    };
    let wire = Wire {
        version: 1,
        request: URL_SAFE_NO_PAD.encode(request.encode()?),
        group: "_".repeat(MAX_ATTEMPT_RECORD.div_ceil(3) * 4),
        directory: false,
        before_copy,
        attributes: [255; 32],
        source_parent_attributes: [255; 32],
        destination_parent_attributes: [255; 32],
        replaced: Some(ReplacedWire {
            attributes: [255; 32],
            head: Some("_".repeat(43)),
        }),
        owner: 0xFFFF_FFFF_FFFF,
        time: MAX_MILLIS,
    };
    if node_mutation::canonical(&wire)?.len() > MAX_PUBLICATION {
        return Err(invalid("rename 原请求无法封存完整最终回执"));
    }
    Ok(())
}
struct Projection {
    source: NodeAttributes,
    source_parent: NodeAttributes,
    destination_parent: NodeAttributes,
}
fn moved_attributes(before: &NodeAttributes, time: &str, parent: bool) -> Result<NodeAttributes> {
    let mut meta = before.meta().ok_or(CatalogError::Incomplete)?.clone();
    meta.change_time = time.into();
    if parent {
        meta.modify_time = time.into();
    }
    NodeAttributes::new(
        meta,
        before.xattrs().ok_or(CatalogError::Incomplete)?.to_vec(),
        before.symlink().map(str::to_owned),
    )
}
fn attributes(node: &ResolvedPath) -> Result<&NodeAttributes> {
    let (ResolvedPath::File { attributes, .. } | ResolvedPath::Directory { attributes, .. }) = node;
    attributes.as_ref().ok_or(CatalogError::Incomplete)
}
fn projection(inputs: &PreparedNodeRenameInputs) -> Result<Projection> {
    let baseline = &inputs.baseline;
    let source = moved_attributes(attributes(&baseline.source)?, inputs.request.time(), false)?;
    let source_parent = moved_attributes(
        baseline
            .source_parent
            .attributes
            .as_ref()
            .ok_or(CatalogError::Incomplete)?,
        inputs.request.time(),
        true,
    )?;
    let destination_parent = if baseline.source_parent.id == baseline.destination_parent.id {
        source_parent.clone()
    } else {
        moved_attributes(
            baseline
                .destination_parent
                .attributes
                .as_ref()
                .ok_or(CatalogError::Incomplete)?,
            inputs.request.time(),
            true,
        )?
    };
    Ok(Projection {
        source,
        source_parent,
        destination_parent,
    })
}
fn retirement(value: &NodeRenamePublication) -> Result<Option<retired_nodes::NodeRetirement>> {
    let Some(target) = &value.replaced else {
        return Ok(None);
    };
    let original = value.request.spec().target.ok_or_else(invalid_attempt)?;
    Ok(Some(retired_nodes::NodeRetirement {
        namespace: original.namespace,
        pool: value.request.spec().pool,
        node: original.inode,
        path: value.request.spec().to.clone(),
        kind: if value.directory {
            NodeKind::Directory
        } else {
            NodeKind::File
        },
        deleted_version: original.version + 1,
        previous_attributes: target.attributes,
        previous_head: target.head.clone(),
        management_epoch: value.request.request_epoch(),
        operation: value.request.spec().operation,
        result_digest: value.fingerprint(),
    }))
}
async fn parent_row(
    tr: &Transaction,
    namespace: Uuid,
    original: &NodeMutationParent,
    after: &NodeAttributes,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let key = node_key(namespace, original.id);
    let raw = tr.get(&key, false).await?.ok_or(CatalogError::Incomplete)?;
    let mut node = decode_node(&raw)?;
    if node.kind != NodeKind::Directory
        || node.directory_version != Some(original.version)
        || node.attributes.as_ref().map(NodeAttributes::digest)
            != original.attributes.as_ref().map(NodeAttributes::digest)
    {
        return Err(invalid_attempt());
    }
    node.directory_version = Some(
        original
            .version
            .checked_add(1)
            .ok_or_else(invalid_attempt)?,
    );
    node.attributes = Some(after.clone());
    Ok((key, encode_node(&node)))
}
fn require_original_result(
    value: &NodeRenamePublication,
    attempt: &NodeRenameAttempt,
) -> Result<()> {
    if value.request != attempt.global.manifest.request
        || value.group() != attempt.id()
        || value.group.fingerprint()? != attempt.fingerprint()
    {
        return Err(invalid_attempt());
    }
    Ok(())
}

impl ManagedCatalog {
    /// Typed retained result; it does not look up either current path.
    pub async fn node_rename_publication(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeRenamePublication>> {
        if epoch == 0 || operation.is_nil() {
            return Err(invalid_attempt());
        }
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        node_tasks::retained(&tr, namespace, epoch).await?;
        node_tasks::task_at(&tr, serial, namespace, epoch, operation).await?;
        at(&tr, namespace, serial, epoch, operation).await
    }

    /// All participants must already be sealed and their claims released in
    /// bounded pages. Only a constant number of canonical rows are switched.
    pub async fn publish_node_rename_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeRenameAttempt,
        max_bytes: usize,
    ) -> Result<NodeRenamePublication> {
        let global = &attempt.global;
        let manifest = &global.manifest;
        let request = &manifest.request;
        if let Some(value) = self
            .node_rename_publication(
                &manifest.task.library_serial,
                route,
                manifest.task.request_epoch,
                manifest.task.operation,
            )
            .await?
        {
            require_original_result(&value, attempt)?;
            return Ok(value);
        }
        let tr = self.db.create_trx()?;
        writing::require_held_original_at(&tr, route, owner, manifest).await?;
        original_global_at(&tr, manifest, &global.header).await?;
        let group = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if group.phase != NodeRenameAttemptPhase::Publishing
            || record_id(&group.id)? != attempt.id()
            || group.fingerprint()? != attempt.fingerprint()
            || group.owner > owner
            || group
                .completion
                .as_ref()
                .is_none_or(|c| c.released_pages != group.pages)
        {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);
        let inputs = self
            .capture_held_node_rename_inputs(
                route,
                owner,
                (
                    &manifest.task.library_serial,
                    manifest.task.request_epoch,
                    manifest.task.operation,
                ),
                max_bytes,
            )
            .await?;
        if inputs.request != *request {
            return Err(invalid_attempt());
        }
        let projection = projection(&inputs)?;
        for attrs in [
            &projection.source,
            &projection.source_parent,
            &projection.destination_parent,
        ] {
            self.upload_attributes(manifest.task.namespace, attrs)
                .await?;
        }
        let time = management::now();
        self.db
            .run(|tr, _| {
                let inputs = &inputs;
                let projection = &projection;
                async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                512_000,
                            ))?;
                            let namespace = routing::require_runtime_active_owner(
                                &tr,
                                request.library(),
                                route,
                                owner,
                            )
                            .await?;
                            node_tasks::retained(&tr, namespace, request.request_epoch()).await?;
                            let task = node_tasks::task_at(
                                &tr,
                                request.library(),
                                namespace,
                                request.request_epoch(),
                                request.spec().operation,
                            )
                            .await?
                            .ok_or_else(invalid_attempt)?;
                            if let Some(value) = at(
                                &tr,
                                namespace,
                                request.library(),
                                request.request_epoch(),
                                request.spec().operation,
                            )
                            .await?
                            {
                                require_original_result(&value, attempt)?;
                                require_task_match(&value, &task)?;
                                return Ok(value);
                            }
                            writing::require_held_original_at(&tr, route, owner, manifest).await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let group = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if group.phase != NodeRenameAttemptPhase::Publishing
                                || record_id(&group.id)? != attempt.id()
                                || group.fingerprint()? != attempt.fingerprint()
                                || group.owner > owner
                                || group
                                    .completion
                                    .as_ref()
                                    .is_none_or(|c| c.released_pages != group.pages)
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            inputs::require_held_inputs_at(&tr, route, owner, inputs).await?;
                            for attrs in [
                                &projection.source,
                                &projection.source_parent,
                                &projection.destination_parent,
                            ] {
                                attribute_objects::require_attributes(&tr, namespace, Some(attrs))
                                    .await?;
                            }
                            let spec = request.spec();
                            let directory =
                                matches!(inputs.baseline.source, ResolvedPath::Directory { .. });
                            let before_copy = match &inputs.baseline.source {
                                ResolvedPath::Directory { .. } => None,
                                ResolvedPath::File { head, copy, .. } => {
                                    Some((head.copy_id, copy.as_ref().clone()))
                                }
                            };
                            let replaced = inputs
                                .baseline
                                .target
                                .as_ref()
                                .map(|target| -> Result<_> {
                                    Ok(ReplacedNode {
                                        attributes: attributes(target)?.digest(),
                                        head: match target {
                                            ResolvedPath::Directory { .. } => None,
                                            ResolvedPath::File { head, .. } => Some(head.clone()),
                                        },
                                    })
                                })
                                .transpose()?;
                            let value = NodeRenamePublication {
                                request: request.clone(),
                                group: group.clone(),
                                directory,
                                before_copy,
                                attributes: projection.source.digest(),
                                source_parent_attributes: projection.source_parent.digest(),
                                destination_parent_attributes: projection
                                    .destination_parent
                                    .digest(),
                                replaced,
                                owner,
                                time: time.max(group.created),
                            };
                            let encoded = encode(&value)?;
                            let result_digest = digest(&encoded);
                            let name = spec
                                .to
                                .rsplit('/')
                                .next()
                                .expect("validated rename destination")
                                .as_bytes();
                            let source_node = encode_node(&Node {
                                kind: if directory {
                                    NodeKind::Directory
                                } else {
                                    NodeKind::File
                                },
                                directory_version: directory.then_some(value.source().version),
                                parent: spec.destination_parent.inode,
                                name: name.to_vec(),
                                attributes: Some(projection.source.clone()),
                            });
                            let parent_source = parent_row(
                                &tr,
                                namespace,
                                &inputs.baseline.source_parent,
                                &projection.source_parent,
                            )
                            .await?;
                            let parent_destination =
                                if spec.source_parent.inode == spec.destination_parent.inode {
                                    None
                                } else {
                                    Some(
                                        parent_row(
                                            &tr,
                                            namespace,
                                            &inputs.baseline.destination_parent,
                                            &projection.destination_parent,
                                        )
                                        .await?,
                                    )
                                };
                            let copy_rows = if let Some((id, copy)) = after_copy(&value)? {
                                let target = copy_key(namespace, spec.source.inode, id);
                                let reverse = crate::managed_catalog::volume_key(
                                    copy.volume_id,
                                    copy.volume_scan_id,
                                    namespace,
                                    spec.source.inode,
                                    id,
                                );
                                if tr.get(&target, false).await?.is_some()
                                    || tr.get(&reverse, false).await?.is_some()
                                {
                                    return Err(invalid_attempt());
                                }
                                let raw = encode_physical_copy(&copy)?;
                                let mut volume_value = copy.version.to_be_bytes().to_vec();
                                volume_value.extend_from_slice(&Sha256::digest(&raw));
                                Some((
                                    target,
                                    raw,
                                    reverse,
                                    volume_value,
                                    encode_head(&FileHead {
                                        version: copy.version,
                                        size: copy.size,
                                        copy_id: id,
                                    }),
                                ))
                            } else {
                                None
                            };
                            let retirement = if let Some(target) = retirement(&value)? {
                                let encoded = retired_nodes::encode(&target)?;
                                let barrier = retired_nodes::prepare_barrier(&tr, &target).await?;
                                Some((
                                    node_key(namespace, target.node),
                                    encoded,
                                    barrier,
                                    head_key(namespace, target.node),
                                ))
                            } else {
                                None
                            };
                            let departure = retired_nodes::NodeDeparture {
                                namespace,
                                pool: spec.pool,
                                node: spec.source.inode,
                                path: spec.from.clone(),
                                previous_version: spec.source.version,
                                management_epoch: request.request_epoch(),
                                operation: spec.operation,
                                result_digest,
                            };
                            let departure =
                                retired_nodes::prepare_departure(&tr, &departure).await?;
                            let from_name = spec
                                .from
                                .rsplit('/')
                                .next()
                                .expect("validated rename source")
                                .as_bytes();
                            let source_edge =
                                child_key(namespace, spec.source_parent.inode, from_name);
                            let destination_edge =
                                child_key(namespace, spec.destination_parent.inode, name);
                            let result_key = key(
                                namespace,
                                request.library(),
                                request.request_epoch(),
                                spec.operation,
                            );
                            // complete_rename_at encodes before its first set. Everything after it is infallible.
                            node_tasks::complete_rename_at(
                                &tr,
                                &task,
                                attempt.id(),
                                owner,
                                value.time,
                                result_digest,
                            )?;
                            tr.clear(&source_edge);
                            tr.set(&destination_edge, spec.source.inode.as_bytes());
                            tr.set(&node_key(namespace, spec.source.inode), &source_node);
                            tr.set(&parent_source.0, &parent_source.1);
                            if let Some((key, raw)) = parent_destination {
                                tr.set(&key, &raw);
                            }
                            if let Some((key, raw, reverse, volume_value, head)) = copy_rows {
                                tr.set(&key, &raw);
                                tr.set(&reverse, &volume_value);
                                tr.set(&head_key(namespace, spec.source.inode), &head);
                            }
                            if let Some((key, raw, (barrier_key, barrier_raw), head)) = retirement {
                                tr.set(&key, &raw);
                                tr.set(&barrier_key, &barrier_raw);
                                tr.clear(&head);
                            }
                            tr.set(&departure.0, &departure.1);
                            tr.set(&result_key, &encoded);
                            namespace_visibility::publish(&tr, namespace);
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
