//! Durable per-medium node attempts. These records precede physical I/O;
//! they are neither an S4 observation nor logical publication.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

const MAX_RECORD: usize = 8192;
#[path = "node_attempts/observation.rs"]
mod observation;
pub use observation::NodeMutationObservation;
pub(in crate::managed_catalog) use observation::require_observation_at;
#[path = "node_attempts/closure.rs"]
mod closure;
pub use closure::NodeMutationAttemptClosure;
pub(in crate::managed_catalog) use closure::require_discard_at;
#[path = "node_attempts/publication.rs"]
mod publication;
pub use publication::NodeMutationPublication;
#[path = "node_attempts/deletion.rs"]
mod deletion;
pub use deletion::NodeMutationDeletion;
#[path = "node_attempts/completion.rs"]
mod completion;
pub(super) use completion::task_outcome_at;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationAttempt {
    id: Uuid,
    library: String,
    namespace: Uuid,
    pool: Uuid,
    epoch: u64,
    operation: Uuid,
    logical_fingerprint: [u8; 32],
    owner: u64,
    created_at_millis: u64,
    object: NodePlanObject,
    scan: Uuid,
    previous: ActiveIndexIdentity,
    pool_revision: u64,
    assignment_revision: u64,
    source_copy: Option<(Uuid, PhysicalCopy)>,
}

impl NodeMutationAttempt {
    fn matches_plan(&self, plan: &NodeMutationPlan) -> bool {
        let c = plan.context();
        c.library_serial == self.library
            && c.namespace == self.namespace
            && c.pool == self.pool
            && c.request_epoch == self.epoch
            && c.operation == self.operation
            && c.owner_epoch == self.owner
            && c.previous == self.previous
            && c.scan == self.scan
            && plan.fingerprint() == self.object.digest()
    }
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn request_identity(&self) -> (u64, Uuid) {
        (self.epoch, self.operation)
    }
    pub fn original_owner(&self) -> u64 {
        self.owner
    }
    pub fn plan_object(&self) -> &NodePlanObject {
        &self.object
    }
    pub fn previous(&self) -> ActiveIndexIdentity {
        self.previous
    }
    pub fn pool_revision(&self) -> u64 {
        self.pool_revision
    }
    pub fn assignment_revision(&self) -> u64 {
        self.assignment_revision
    }
    pub fn source_copy(&self) -> Option<(Uuid, &PhysicalCopy)> {
        self.source_copy.as_ref().map(|(id, copy)| (*id, copy))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    id: String,
    library: String,
    namespace: String,
    pool: String,
    epoch: u64,
    operation: String,
    logical_fingerprint: [u8; 32],
    owner: u64,
    created_at_millis: u64,
    object: String,
    volume: String,
    scan: String,
    previous: String,
    pool_revision: u64,
    assignment_revision: u64,
    source_copy: Option<(String, String)>,
}

fn attempt_key(value: &NodeMutationAttempt) -> Vec<u8> {
    let mut key = management::prefix(value.namespace, b'Q');
    key.push(b'a');
    append_component(&mut key, value.library.as_bytes());
    key.extend_from_slice(&value.epoch.to_be_bytes());
    key.extend_from_slice(value.operation.as_bytes());
    key.extend_from_slice(value.id.as_bytes());
    key
}
fn task_attempt_key(task: &NodeMutationTask, id: Uuid) -> Vec<u8> {
    let mut key = management::prefix(task.namespace, b'Q');
    key.push(b'a');
    append_component(&mut key, task.library_serial.as_bytes());
    key.extend_from_slice(&task.request_epoch.to_be_bytes());
    key.extend_from_slice(task.operation.as_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}
pub(in crate::managed_catalog) fn volume_key(volume: Uuid) -> Vec<u8> {
    // A physical medium is globally unique, including across namespace changes.
    // Nil is reserved for global management records, never a user namespace.
    let mut key = management::prefix(Uuid::nil(), b'Q');
    key.push(b'v');
    key.extend_from_slice(volume.as_bytes());
    key
}
fn validate(value: &NodeMutationAttempt) -> Result<()> {
    validate_physical_serial(&value.library)?;
    if value.id.is_nil()
        || value.namespace.is_nil()
        || value.pool.is_nil()
        || value.operation.is_nil()
        || value.scan.is_nil()
        || value.epoch == 0
        || value.owner == 0
        || !(1..=253_402_300_799_999).contains(&value.created_at_millis)
        || value.pool_revision == 0
        || value.assignment_revision == 0
        || value.object.namespace() != value.namespace
        || value.object.library() != value.library
        || value.previous.volume_uuid.is_nil()
        || value.previous.location.partition != 'b'
        || value.previous.generation.checked_add(1).is_none()
    {
        return Err(CatalogError::Corrupt("节点物理尝试身份、引用或来源无效"));
    }
    let mut bytes = Vec::new();
    active_volume::encode_index(&mut bytes, value.previous);
    active_volume::decode_index(value.previous.volume_uuid, &bytes)?;
    if let Some((id, copy)) = &value.source_copy {
        if id.is_nil()
            || copy.volume_id != value.previous.volume_uuid
            || copy.volume_scan_id != value.scan
            || copy.volume_generation > value.previous.generation
        {
            return Err(CatalogError::Corrupt("节点尝试原副本与介质来源不符"));
        }
        encode_physical_copy(copy)?;
    }
    Ok(())
}
pub(super) fn encode(value: &NodeMutationAttempt) -> Result<Vec<u8>> {
    validate(value)?;
    let mut previous = Vec::new();
    active_volume::encode_index(&mut previous, value.previous);
    let bytes = node_mutation::canonical(&Wire {
        version: 1,
        id: value.id.to_string(),
        library: value.library.clone(),
        namespace: value.namespace.to_string(),
        pool: value.pool.to_string(),
        epoch: value.epoch,
        operation: value.operation.to_string(),
        logical_fingerprint: value.logical_fingerprint,
        owner: value.owner,
        created_at_millis: value.created_at_millis,
        object: URL_SAFE_NO_PAD.encode(value.object.encode()),
        volume: value.previous.volume_uuid.to_string(),
        scan: value.scan.to_string(),
        previous: URL_SAFE_NO_PAD.encode(previous),
        pool_revision: value.pool_revision,
        assignment_revision: value.assignment_revision,
        source_copy: value
            .source_copy
            .as_ref()
            .map(|(id, copy)| -> Result<_> {
                Ok((
                    id.to_string(),
                    URL_SAFE_NO_PAD.encode(encode_physical_copy(copy)?),
                ))
            })
            .transpose()?,
    })?;
    if bytes.len() > MAX_RECORD {
        return Err(CatalogError::Corrupt("节点物理尝试记录过长"));
    }
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> Result<NodeMutationAttempt> {
    if bytes.len() > MAX_RECORD {
        return Err(CatalogError::Corrupt("节点物理尝试记录过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点物理尝试编码"))?;
    let id = |s: &str| node_mutation::id(s).map_err(|_| CatalogError::Corrupt("节点物理尝试 UUID"));
    let raw = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| CatalogError::Corrupt("节点物理尝试二进制字段"))
    };
    let value = NodeMutationAttempt {
        id: id(&wire.id)?,
        library: wire.library,
        namespace: id(&wire.namespace)?,
        pool: id(&wire.pool)?,
        epoch: wire.epoch,
        operation: id(&wire.operation)?,
        logical_fingerprint: wire.logical_fingerprint,
        owner: wire.owner,
        created_at_millis: wire.created_at_millis,
        object: NodePlanObject::decode(&raw(&wire.object)?)?,
        scan: id(&wire.scan)?,
        previous: active_volume::decode_index(id(&wire.volume)?, &raw(&wire.previous)?)?,
        pool_revision: wire.pool_revision,
        assignment_revision: wire.assignment_revision,
        source_copy: wire
            .source_copy
            .map(|(copy_id, bytes)| -> Result<_> {
                Ok((id(&copy_id)?, decode_copy(&raw(&bytes)?)?))
            })
            .transpose()?,
    };
    if wire.version != 1 || encode(&value)? != bytes {
        return Err(CatalogError::Corrupt("节点物理尝试必须为规范版本1"));
    }
    Ok(value)
}

pub(super) async fn require_task_at(
    tr: &Transaction,
    task: &NodeMutationTask,
) -> Result<NodeMutationAttempt> {
    if task.kind != NodeMutationTaskKind::SingleNode {
        return Err(CatalogError::Invalid("单节点尝试不能读取 rename 任务"));
    }
    let id = task
        .active_attempt
        .ok_or(CatalogError::Corrupt("Writing 节点任务缺少物理尝试"))?;
    let raw = tr
        .get(&task_attempt_key(task, id), false)
        .await?
        .ok_or(CatalogError::Corrupt("节点物理尝试主记录缺失"))?;
    let value = decode(&raw)?;
    if task.state != NodeMutationTaskState::Writing
        || value.id != id
        || value.library != task.library_serial
        || value.namespace != task.namespace
        || value.pool != task.pool
        || value.epoch != task.request_epoch
        || value.operation != task.operation
        || value.logical_fingerprint != task.fingerprint
        || value.owner
            < task
                .prepared_owner_epoch
                .ok_or(CatalogError::Corrupt("Writing 缺少准备 owner"))?
        || value.created_at_millis
            < task
                .prepared_at_millis
                .ok_or(CatalogError::Corrupt("Writing 缺少准备时间"))?
        || tr
            .get(&volume_key(value.previous.volume_uuid), false)
            .await?
            .as_deref()
            != Some(
                node_claims::encode(&node_claims::NodeVolumeClaim::Single(Box::new(
                    value.clone(),
                )))?
                .as_slice(),
            )
    {
        return Err(CatalogError::Corrupt("节点任务、物理尝试与卷占用不一致"));
    }
    node_plan_objects::require_sealed_at(tr, &value.object).await?;
    if closure::at(tr, &value).await?.is_some() {
        return Err(CatalogError::Corrupt("已关闭节点尝试仍被标为 Writing"));
    }
    Ok(value)
}

pub(in crate::managed_catalog) async fn require_no_pending_at(
    tr: &Transaction,
    volume: Uuid,
) -> Result<()> {
    let Some(placement) = verified_placement(tr, volume).await? else {
        return if tr.get(&volume_key(volume), false).await?.is_some() {
            Err(CatalogError::PoolBusy)
        } else {
            Ok(())
        };
    };
    if pending_at(tr, &placement).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

async fn pending_at(
    tr: &Transaction,
    placement: &VolumePlacement,
) -> Result<Option<NodeMutationAttempt>> {
    let volume = placement.volume_id;
    // The pool side also detects a Writing record whose volume index was lost.
    let held_task = node_tasks::held_task_at(tr, placement.namespace, placement.pool_id).await?;
    let Some(raw) = tr.get(&volume_key(volume), false).await? else {
        if let Some(task) = held_task {
            Box::pin(node_rename::require_no_missing_prepared_claim_at(
                tr, placement, &task,
            ))
            .await?;
        }
        return Ok(None);
    };
    let value = match node_claims::decode(&raw)? {
        node_claims::NodeVolumeClaim::Single(value) => *value,
        node_claims::NodeVolumeClaim::Rename(value) => {
            Box::pin(node_rename::require_prepared_volume_claim_at(
                tr, placement, &value,
            ))
            .await?;
            return Err(CatalogError::PoolBusy);
        }
    };
    if value.namespace != placement.namespace
        || value.pool != placement.pool_id
        || value.library != placement.library_serial
        || value.previous.volume_uuid != volume
    {
        return Err(CatalogError::Corrupt("节点卷占用作用域不符"));
    }
    let task = node_tasks::task_at(
        tr,
        &value.library,
        value.namespace,
        value.epoch,
        value.operation,
    )
    .await?
    .ok_or(CatalogError::Corrupt("节点卷占用缺少任务"))?;
    if require_task_at(tr, &task).await? != value {
        return Err(CatalogError::Corrupt("节点卷占用与物理尝试不符"));
    }
    Ok(Some(value))
}

async fn current_task_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    plan: &NodeMutationPlan,
) -> Result<NodeMutationTask> {
    let c = plan.context();
    let namespace =
        routing::require_runtime_active_owner(tr, &c.library_serial, route, owner).await?;
    if namespace != c.namespace {
        return Err(CatalogError::Invalid("节点尝试与活动命名空间不符"));
    }
    node_tasks::retained(tr, namespace, c.request_epoch).await?;
    node_tasks::task_at(
        tr,
        &c.library_serial,
        namespace,
        c.request_epoch,
        c.operation,
    )
    .await?
    .ok_or(CatalogError::Invalid("节点尝试任务尚未登记"))
}
async fn known_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    id: Uuid,
    object: &NodePlanObject,
) -> Result<Option<NodeMutationAttempt>> {
    if task.state != NodeMutationTaskState::Writing {
        return Ok(None);
    }
    let saved = require_task_at(tr, task).await?;
    if saved.id != id || saved.object != *object {
        return Err(CatalogError::Invalid("节点任务已绑定另一物理尝试"));
    }
    Ok(Some(saved))
}

impl ManagedCatalog {
    /// Atomically install Writing and a per-volume pending record before any
    /// device staging. Repeats return the exact original plan/owner/time; this
    /// metadata result is not PR, an S4 proof, or permission to blindly replay.
    pub async fn admit_node_mutation_attempt(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        id: Uuid,
        inputs: &PreparedNodeMutationInputs,
        plan: &NodeMutationPlan,
        max_bytes: usize,
    ) -> Result<NodeMutationAttempt> {
        if id.is_nil() {
            return Err(CatalogError::Invalid("节点物理尝试身份为空"));
        }
        plan.validate_prepared_inputs(inputs)?;
        let c = plan.context();
        let object =
            NodePlanObject::for_bytes(&c.library_serial, c.namespace, &plan.encode()?, max_bytes)?;
        let tr = self.db.create_trx()?;
        let task = current_task_at(&tr, route, owner, plan).await?;
        if task.fingerprint != inputs.request.fingerprint() {
            return Err(CatalogError::Invalid("节点尝试逻辑请求不符"));
        }
        if let Some(saved) = known_at(&tr, &task, id, &object).await? {
            return Ok(saved);
        }
        node_inputs::require_inputs_at(&tr, route, owner, inputs).await?;
        drop(tr);
        self.upload_node_mutation_plan(route, owner, plan, max_bytes)
            .await?;
        self.db
            .run(|tr, _| {
                let object = &object;
                async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                256_000,
                            ))?;
                            let task = current_task_at(&tr, route, owner, plan).await?;
                            if task.fingerprint != inputs.request.fingerprint() {
                                return Err(CatalogError::Invalid("节点尝试逻辑请求不符"));
                            }
                            if let Some(saved) = known_at(&tr, &task, id, object).await? {
                                return Ok(saved);
                            }
                            node_inputs::require_inputs_at(&tr, route, owner, inputs).await?;
                            node_plan_objects::require_sealed_at(&tr, object).await?;
                            if c.owner_epoch != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let placement = verified_placement(&tr, c.previous.volume_uuid)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.namespace != c.namespace
                                || placement.pool_id != c.pool
                                || placement.library_serial != c.library_serial
                            {
                                return Err(CatalogError::Invalid("节点尝试卷与池或带库不符"));
                            }
                            let assignment = Self::verified_assignment(
                                &tr,
                                &c.library_serial,
                                &placement.barcode,
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if !assignment.allows_io() {
                                return Err(CatalogError::PoolBusy);
                            }
                            let (scan, previous) = active_volume::observed_at(
                                &tr,
                                &c.library_serial,
                                c.previous.volume_uuid,
                            )
                            .await?;
                            if scan.id != c.scan || previous != c.previous {
                                return Err(CatalogError::Invalid(
                                    "节点尝试的卷扫描或源索引已改变",
                                ));
                            }
                            let next = previous
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Invalid("节点尝试代数溢出"))?;
                            require_no_pending_at(&tr, previous.volume_uuid).await?;
                            if active_volume::recovery_active(&tr, previous.volume_uuid).await?
                                || !reservation::volume_reservations_empty(
                                    &tr,
                                    previous.volume_uuid,
                                    next,
                                )
                                .await?
                                || tr
                                    .get(
                                        &active_batch::batch_key(previous.volume_uuid, next),
                                        false,
                                    )
                                    .await?
                                    .is_some()
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            let source_copy =
                                if let Some(ResolvedPath::File { head, copy, .. }) =
                                    &inputs.baseline.current
                                {
                                    if copy.volume_id != previous.volume_uuid
                                        || copy.volume_scan_id != c.scan
                                    {
                                        return Err(CatalogError::Invalid(
                                            "节点现有文件尝试必须绑定当前主副本所在卷",
                                        ));
                                    }
                                    plan.validate_source_copy(copy)?;
                                    Some((head.copy_id, (**copy).clone()))
                                } else {
                                    None
                                };
                            let pool = decode_pool(
                                c.pool,
                                &tr.get(&pool_key(c.namespace, c.pool), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?,
                            )?;
                            let file_count = active_volume::file_count_at(
                                &tr,
                                &c.library_serial,
                                previous.volume_uuid,
                            )
                            .await?;
                            if matches!(plan.mutation(), NodeMutation::CreateSymlink { .. })
                                && file_count >= pool.file_limit
                            {
                                return Err(CatalogError::Invalid(
                                    "新建链接超过池的磁带文件数上限",
                                ));
                            }
                            let attempt = NodeMutationAttempt {
                                id,
                                library: c.library_serial.clone(),
                                namespace: c.namespace,
                                pool: c.pool,
                                epoch: c.request_epoch,
                                operation: c.operation,
                                logical_fingerprint: task.fingerprint,
                                owner,
                                created_at_millis: management::now().max(
                                    task.prepared_at_millis
                                        .ok_or(CatalogError::Corrupt("Prepared 缺少时间"))?,
                                ),
                                object: object.clone(),
                                scan: c.scan,
                                previous,
                                pool_revision: pool.revision,
                                assignment_revision: assignment.revision,
                                source_copy,
                            };
                            let encoded = encode(&attempt)?;
                            let volume_claim = node_claims::encode(
                                &node_claims::NodeVolumeClaim::Single(Box::new(attempt.clone())),
                            )?;
                            closure::require_unused_at(&tr, &attempt).await?;
                            let record = attempt_key(&attempt);
                            if let Some(raw) = tr.get(&record, false).await? {
                                let previous_attempt = decode(&raw)?;
                                if previous_attempt.id == id
                                    && previous_attempt.namespace == c.namespace
                                    && previous_attempt.library == c.library_serial
                                    && previous_attempt.epoch == c.request_epoch
                                    && previous_attempt.operation == c.operation
                                    && closure::at(&tr, &previous_attempt).await?.is_some()
                                {
                                    return Err(CatalogError::Invalid(
                                        "已关闭节点尝试身份不能复用",
                                    ));
                                }
                                return Err(CatalogError::Corrupt("未绑定任务已存在同名节点尝试"));
                            }
                            // Every fallible check and encoding precedes the first set.
                            node_tasks::mark_writing_at(&tr, &task, id)?;
                            tr.set(&record, &encoded);
                            tr.set(&volume_key(previous.volume_uuid), &volume_claim);
                            Ok(attempt)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Read the original attempt after takeover. This does not change its
    /// physical plan to the new owner or authorize an I/O replay.
    pub async fn node_mutation_attempt(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeMutationAttempt>> {
        if epoch == 0 || operation.is_nil() {
            return Err(CatalogError::Invalid("节点物理尝试任务身份无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        node_tasks::retained(&tr, namespace, epoch).await?;
        let Some(task) = node_tasks::task_at(&tr, serial, namespace, epoch, operation).await?
        else {
            return Ok(None);
        };
        if task.kind != NodeMutationTaskKind::SingleNode {
            return Err(CatalogError::Invalid("原节点任务类型不是单节点变更"));
        }
        if task.state != NodeMutationTaskState::Writing {
            return Ok(None);
        }
        require_task_at(&tr, &task).await.map(Some)
    }

    /// Discover the volume's original pending node work before generic file
    /// recovery. It remains discoverable across owner changes and policy stops.
    pub async fn pending_node_mutation_attempt(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        volume: Uuid,
    ) -> Result<Option<NodeMutationAttempt>> {
        if volume.is_nil() {
            return Err(CatalogError::Invalid("节点待恢复卷身份为空"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let Some(placement) = verified_placement(&tr, volume).await? else {
            return Ok(None);
        };
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Invalid("节点待恢复卷与活动带库不符"));
        }
        let value = pending_at(&tr, &placement).await?;
        if let Some(value) = &value {
            node_tasks::retained(&tr, namespace, value.epoch).await?;
        }
        Ok(value)
    }

    pub async fn load_node_mutation_attempt_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &NodeMutationAttempt,
        max_bytes: usize,
    ) -> Result<NodeMutationPlan> {
        let saved = self
            .node_mutation_attempt(
                &attempt.library,
                route,
                owner,
                attempt.epoch,
                attempt.operation,
            )
            .await?;
        if saved.as_ref() != Some(attempt) {
            return Err(CatalogError::Invalid("节点物理尝试已不再待恢复"));
        }
        let plan = self
            .load_node_mutation_plan(route, owner, &attempt.object, max_bytes)
            .await?;
        if !attempt.matches_plan(&plan) {
            return Err(CatalogError::Corrupt("节点物理尝试与保存计划身份不符"));
        }
        if self
            .node_mutation_attempt(
                &attempt.library,
                route,
                owner,
                attempt.epoch,
                attempt.operation,
            )
            .await?
            .as_ref()
            != Some(attempt)
        {
            return Err(CatalogError::Invalid("节点计划加载期间物理尝试状态已改变"));
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::IndexLocation;
    use serde_json::json;
    use sha2::Digest;

    #[test]
    fn source_copy_binding_keeps_alias_uid_separate_and_rejects_content_projection_changes() {
        use crate::ltfs::index::{FileNode, LtfsIndex, Xattr};
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "projection-fixture".into(), 'b');
        index.self_location.start_block = 11;
        index.highest_file_uid = 2;
        let digest = ContentDigest::Sha256(sha2::Sha256::digest([]).into());
        let mut file = FileNode {
            name: "file".into(),
            meta: index.root.meta.clone(),
            xattrs: vec![Xattr {
                key: "ltfs.hash.sha256sum".into(),
                value: digest
                    .payload()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                base64: false,
            }],
            ..Default::default()
        };
        file.meta.file_uid = 2;
        index.root.files.push(file.clone());
        let parent = Uuid::new_v4();
        let ancestors = vec![NodeMutationAncestor {
            path: "/".into(),
            id: parent,
            version: 1,
            attributes: NodeAttributes::from_directory(&index.root).unwrap(),
        }];
        let context = NodeMutationContext {
            library_serial: "COPY-BINDING".into(),
            namespace: Uuid::new_v4(),
            pool: Uuid::new_v4(),
            operation: Uuid::new_v4(),
            request_epoch: 1,
            owner_epoch: 100,
            parent,
            parent_version: 1,
            node: Uuid::new_v4(),
            expected_version: Some(1),
            scan: Uuid::new_v4(),
            previous: ActiveIndexIdentity {
                volume_uuid: index.volume_uuid,
                generation: index.generation,
                location: index.self_location,
                sha256: [7; 32],
                incremental: false,
            },
            path: "/file".into(),
        };
        let mutation = NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        };
        let prepare = |index: &LtfsIndex, attributes: &NodeAttributes| {
            NodeMutationPlan::prepare(
                context.clone(),
                mutation.clone(),
                index,
                "2026-10-31T08:00:00.000000000Z",
                &ancestors,
                Some((NodeKind::File, attributes)),
            )
            .unwrap()
        };
        let plan = prepare(&index, &NodeAttributes::from_file(&file).unwrap());
        let mut copy = PhysicalCopy {
            version: 1,
            origin: CopyOrigin::FileRequest { request_epoch: 1 },
            volume_id: index.volume_uuid,
            volume_generation: index.generation,
            volume_scan_id: context.scan,
            ltfs_file_uid: 2,
            size: 0,
            content_digest: digest,
            media: None,
        };
        plan.validate_source_copy(&copy).unwrap();
        copy.ltfs_file_uid = 3;
        assert!(plan.validate_source_copy(&copy).is_err());
        copy.ltfs_file_uid = 2;
        copy.size = 1;
        assert!(plan.validate_source_copy(&copy).is_err());
        copy.size = 0;
        let mut wrong = file.clone();
        wrong.xattrs[0].value = "00".repeat(32);
        let projected = prepare(&index, &NodeAttributes::from_file(&wrong).unwrap());
        assert!(projected.validate_source_copy(&copy).is_err());

        file.symlink = Some(".LTFSEE_DATA/body".into());
        file.xattrs.clear();
        index.root.files[0] = file.clone();
        // A full logical-index fixture, never a claimed device commit. Keep
        // the alias UID distinct from its direct body's UID and directory UID.
        let body_digest = ContentDigest::Sha256(sha2::Sha256::digest([b'x'; 17]).into());
        let mut body = FileNode {
            name: "body".into(),
            meta: file.meta.clone(),
            length: 17,
            extents: vec![crate::ltfs::index::Extent {
                partition: 'b',
                start_block: 20,
                byte_offset: 0,
                byte_count: 17,
                file_offset: 0,
            }],
            xattrs: vec![Xattr {
                key: "ltfs.hash.sha256sum".into(),
                value: body_digest
                    .payload()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                base64: false,
            }],
            ..Default::default()
        };
        body.meta.file_uid = 3;
        let mut directory = crate::ltfs::index::DirectoryNode {
            name: ".LTFSEE_DATA".into(),
            meta: index.root.meta.clone(),
            files: vec![body],
            ..Default::default()
        };
        directory.meta.file_uid = 4;
        index.root.subdirs.push(directory);
        index.highest_file_uid = 4;
        let alias = prepare(&index, &NodeAttributes::from_file(&file).unwrap());
        copy.ltfs_file_uid = 3;
        copy.size = 17;
        copy.content_digest = body_digest;
        copy.media = Some(MediaProvenance {
            version: (1, 0),
            logical_file_uid: 2,
            physical_path: ".LTFSEE_DATA/body".into(),
            ee_alias: true,
            native_symlink: false,
        });
        alias.validate_source_copy(&copy).unwrap();
        for field in ["alias_uid", "data_uid", "size", "hash", "path"] {
            let mut wrong = copy.clone();
            match field {
                "alias_uid" => wrong.media.as_mut().unwrap().logical_file_uid = 4,
                "data_uid" => wrong.ltfs_file_uid = 4,
                "size" => wrong.size += 1,
                "hash" => wrong.content_digest = ContentDigest::Unknown,
                "path" => wrong.media.as_mut().unwrap().physical_path = ".LTFSEE_DATA/other".into(),
                _ => unreachable!(),
            }
            assert!(alias.validate_source_copy(&wrong).is_err(), "{field}");
        }
        let mut wire: serde_json::Value = serde_json::from_slice(&alias.encode().unwrap()).unwrap();
        wire["source_data"]["sha256"] = serde_json::json!(vec![0_u8; 32]);
        let forged = NodeMutationPlan::decode(wire.to_string().as_bytes()).unwrap();
        let original_xml = index.to_xml().unwrap();
        assert!(forged.stage(&mut index).is_err());
        assert_eq!(index.to_xml().unwrap(), original_xml);
    }

    #[test]
    fn attempt_codec_is_canonical_scoped_and_preserves_source_copy() {
        let namespace = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let scan = Uuid::new_v4();
        // Codec fixtures are metadata only, never a claimed tape commit.
        let mut value = NodeMutationAttempt {
            id: Uuid::new_v4(),
            library: "ATTEMPT-CODEC".into(),
            namespace,
            pool: Uuid::new_v4(),
            epoch: 1,
            operation: Uuid::new_v4(),
            logical_fingerprint: [5; 32],
            owner: 100,
            created_at_millis: 1_798_000_000_000,
            object: NodePlanObject::for_bytes("ATTEMPT-CODEC", namespace, b"inert", 100).unwrap(),
            scan,
            previous: ActiveIndexIdentity {
                volume_uuid: volume,
                generation: 2,
                location: IndexLocation {
                    partition: 'b',
                    start_block: 11,
                },
                sha256: [7; 32],
                incremental: false,
            },
            pool_revision: 1,
            assignment_revision: 2,
            source_copy: None,
        };
        let bytes = encode(&value).unwrap();
        assert_eq!(decode(&bytes).unwrap(), value);
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
        for (field, wrong) in [
            ("version", json!(0)),
            ("version", json!(2)),
            ("id", json!(Uuid::nil().to_string())),
            ("namespace", json!(Uuid::nil().to_string())),
            ("pool", json!(Uuid::nil().to_string())),
            ("scan", json!(Uuid::nil().to_string())),
            ("operation", json!(Uuid::nil().to_string())),
            ("owner", json!(0)),
            ("epoch", json!(0)),
            ("created_at_millis", json!(0)),
            ("pool_revision", json!(0)),
            ("assignment_revision", json!(0)),
            ("previous", json!("?")),
            ("object", json!("?")),
            ("extra", json!(true)),
        ] {
            let mut bad = wire.clone();
            bad[field] = wrong;
            assert!(decode(bad.to_string().as_bytes()).is_err(), "bad {field}");
        }
        assert!(decode(&serde_json::to_vec_pretty(&wire).unwrap()).is_err());
        value.source_copy = Some((
            Uuid::new_v4(),
            PhysicalCopy {
                version: 9,
                origin: CopyOrigin::FileRequest { request_epoch: 3 },
                volume_id: volume,
                volume_generation: 2,
                volume_scan_id: scan,
                ltfs_file_uid: u64::MAX,
                size: 17,
                content_digest: ContentDigest::Sha256([8; 32]),
                media: None,
            },
        ));
        assert_eq!(decode(&encode(&value).unwrap()).unwrap(), value);
        value.source_copy.as_mut().unwrap().1.volume_scan_id = Uuid::new_v4();
        assert!(encode(&value).is_err());
    }
}
