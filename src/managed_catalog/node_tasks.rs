//! Durable logical work registration. Queued requests own no logical path or
//! medium. Prepared tasks hold the pool against new writes, but do not authorize
//! a physical attempt; media selection and S4 recovery are separate transitions.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

const MAX_RECORD: usize = 4096;
const MAX_TIME: u64 = 253_402_300_799_999;
const MAX_REJECTION_BYTES: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeMutationTaskKind {
    SingleNode,
    Rename,
}

#[path = "node_tasks/rename_api.rs"]
mod rename_api;
pub(super) use rename_api::{require_rename_held_at, require_rename_prepared_at};
#[path = "node_tasks/request.rs"]
mod request;
use request::LogicalNodeRequest;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeMutationTaskState {
    Queued,
    Prepared,
    Writing,
    Cancelled,
    Rejected,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationTask {
    pub kind: NodeMutationTaskKind,
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool: Uuid,
    pub request_epoch: u64,
    pub operation: Uuid,
    pub object: NodePlanObject,
    pub fingerprint: [u8; 32],
    pub state: NodeMutationTaskState,
    pub accepted_owner_epoch: u64,
    pub created_at_millis: u64,
    pub prepared_owner_epoch: Option<u64>,
    pub prepared_at_millis: Option<u64>,
    pub active_attempt: Option<Uuid>,
    pub completed_owner_epoch: Option<u64>,
    pub completed_at_millis: Option<u64>,
    pub publication_digest: Option<[u8; 32]>,
    pub rejection_reason: Option<String>,
}

/// Retained logical outcome, independent of the current path and media owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationTaskOutcome {
    pub node: Uuid,
    pub version: u64,
    pub parent_version: u64,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationTaskStatus {
    pub task: NodeMutationTask,
    pub outcome: Option<NodeMutationTaskOutcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    kind: NodeMutationTaskKind,
    library: String,
    namespace: String,
    pool: String,
    epoch: u64,
    operation: String,
    object: String,
    fingerprint: [u8; 32],
    state: NodeMutationTaskState,
    accepted_owner: u64,
    created_at_millis: u64,
    prepared_owner: Option<u64>,
    prepared_at_millis: Option<u64>,
    active_attempt: Option<String>,
    completed_owner: Option<u64>,
    completed_at_millis: Option<u64>,
    publication_digest: Option<[u8; 32]>,
    rejection_reason: Option<String>,
}

fn namespace_prefix(namespace: Uuid, kind: u8) -> Vec<u8> {
    let mut out = management::prefix(namespace, b'Q');
    out.push(kind);
    out
}
fn library_prefix(namespace: Uuid, serial: &str, kind: u8) -> Vec<u8> {
    let mut out = namespace_prefix(namespace, b'l');
    append_component(&mut out, serial.as_bytes());
    out.push(kind);
    out
}
fn identity_key(namespace: Uuid, serial: &str, kind: u8, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = library_prefix(namespace, serial, kind);
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}
fn global_pending_key(namespace: Uuid, serial: &str, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = namespace_prefix(namespace, b'p');
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    append_component(&mut out, serial.as_bytes());
    out
}
fn valid_identity(epoch: u64, operation: Uuid) -> Result<()> {
    if epoch == 0 || operation.is_nil() {
        return Err(CatalogError::Invalid("节点任务身份无效"));
    }
    Ok(())
}
fn valid_rejection(reason: &str) -> bool {
    !reason.trim().is_empty()
        && reason.len() <= MAX_REJECTION_BYTES
        && !reason.chars().any(char::is_control)
}

fn validate(value: &NodeMutationTask) -> Result<()> {
    validate_physical_serial(&value.library_serial)
        .map_err(|_| CatalogError::Corrupt("节点任务带库身份"))?;
    if value.namespace.is_nil()
        || value.pool.is_nil()
        || value.request_epoch == 0
        || value.operation.is_nil()
        || value.object.namespace() != value.namespace
        || value.object.library() != value.library_serial
        || value.accepted_owner_epoch == 0
        || !(1..=MAX_TIME).contains(&value.created_at_millis)
    {
        return Err(CatalogError::Corrupt("节点任务作用域、引用或登记时间"));
    }
    if (value.state == NodeMutationTaskState::Writing) != value.active_attempt.is_some()
        || value.active_attempt.is_some_and(|id| id.is_nil())
        || (value.state == NodeMutationTaskState::Completed) != value.publication_digest.is_some()
    {
        return Err(CatalogError::Corrupt("节点 Writing 状态与物理尝试引用不符"));
    }
    if (value.state == NodeMutationTaskState::Rejected) != value.rejection_reason.is_some()
        || value
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| !valid_rejection(reason))
    {
        return Err(CatalogError::Corrupt("节点拒绝状态与原因不符"));
    }
    let prepared = match (value.prepared_owner_epoch, value.prepared_at_millis) {
        (None, None) => false,
        (Some(owner), Some(time))
            if owner >= value.accepted_owner_epoch
                && (value.created_at_millis..=MAX_TIME).contains(&time) =>
        {
            true
        }
        _ => return Err(CatalogError::Corrupt("节点准备状态与时间不一致")),
    };
    match (
        value.state,
        value.completed_owner_epoch,
        value.completed_at_millis,
    ) {
        (NodeMutationTaskState::Queued, None, None) if !prepared => Ok(()),
        (NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing, None, None)
            if prepared =>
        {
            Ok(())
        }
        (
            NodeMutationTaskState::Cancelled
            | NodeMutationTaskState::Rejected
            | NodeMutationTaskState::Completed,
            Some(owner),
            Some(time),
        ) if (value.state != NodeMutationTaskState::Completed || prepared)
            && owner
                >= value
                    .prepared_owner_epoch
                    .unwrap_or(value.accepted_owner_epoch)
            && (value.prepared_at_millis.unwrap_or(value.created_at_millis)..=MAX_TIME)
                .contains(&time) =>
        {
            Ok(())
        }
        _ => Err(CatalogError::Corrupt("节点任务终态与完成时间不一致")),
    }
}
fn encode(value: &NodeMutationTask) -> Result<Vec<u8>> {
    validate(value)?;
    let bytes = node_mutation::canonical(&Wire {
        version: 6,
        kind: value.kind,
        library: value.library_serial.clone(),
        namespace: value.namespace.to_string(),
        pool: value.pool.to_string(),
        epoch: value.request_epoch,
        operation: value.operation.to_string(),
        object: URL_SAFE_NO_PAD.encode(value.object.encode()),
        fingerprint: value.fingerprint,
        state: value.state,
        accepted_owner: value.accepted_owner_epoch,
        created_at_millis: value.created_at_millis,
        prepared_owner: value.prepared_owner_epoch,
        prepared_at_millis: value.prepared_at_millis,
        active_attempt: value.active_attempt.map(|id| id.to_string()),
        completed_owner: value.completed_owner_epoch,
        completed_at_millis: value.completed_at_millis,
        publication_digest: value.publication_digest,
        rejection_reason: value.rejection_reason.clone(),
    })?;
    if bytes.len() > MAX_RECORD {
        return Err(CatalogError::Corrupt("节点任务记录过长"));
    }
    Ok(bytes)
}
fn decode(
    bytes: &[u8],
    serial: &str,
    namespace: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<NodeMutationTask> {
    if bytes.len() > MAX_RECORD {
        return Err(CatalogError::Corrupt("节点任务记录过长"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("节点任务记录编码"))?;
    let id = |s: &str| node_mutation::id(s).map_err(|_| CatalogError::Corrupt("节点任务UUID编码"));
    let raw = URL_SAFE_NO_PAD
        .decode(&wire.object)
        .map_err(|_| CatalogError::Corrupt("节点任务对象引用编码"))?;
    let task = NodeMutationTask {
        kind: wire.kind,
        library_serial: wire.library,
        namespace: id(&wire.namespace)?,
        pool: id(&wire.pool)?,
        request_epoch: wire.epoch,
        operation: id(&wire.operation)?,
        object: NodePlanObject::decode(&raw)?,
        fingerprint: wire.fingerprint,
        state: wire.state,
        accepted_owner_epoch: wire.accepted_owner,
        created_at_millis: wire.created_at_millis,
        prepared_owner_epoch: wire.prepared_owner,
        prepared_at_millis: wire.prepared_at_millis,
        active_attempt: wire.active_attempt.as_deref().map(id).transpose()?,
        completed_owner_epoch: wire.completed_owner,
        completed_at_millis: wire.completed_at_millis,
        publication_digest: wire.publication_digest,
        rejection_reason: wire.rejection_reason,
    };
    validate(&task)?;
    if wire.version != 6
        || task.library_serial != serial
        || task.namespace != namespace
        || task.request_epoch != epoch
        || task.operation != operation
        || encode(&task)? != bytes
    {
        return Err(CatalogError::Corrupt("节点任务键值或规范编码不一致"));
    }
    Ok(task)
}

pub(in crate::managed_catalog) fn node_hold_prefix(namespace: Uuid) -> Vec<u8> {
    namespace_prefix(namespace, b'h')
}

fn hold_key(namespace: Uuid, pool: Uuid) -> Vec<u8> {
    let mut key = node_hold_prefix(namespace);
    key.extend_from_slice(pool.as_bytes());
    key
}

fn hold_value(task: &NodeMutationTask) -> Result<Vec<u8>> {
    node_mutation::canonical(&(
        2u8,
        task.kind,
        &task.library_serial,
        task.request_epoch,
        task.operation.to_string(),
        task.fingerprint,
    ))
}

/// Read both sides of the hold. An orphan or changed record is corruption,
/// never an available pool and never an invitation to repair the indexes.
pub(super) async fn held_task_at(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
) -> Result<Option<NodeMutationTask>> {
    let Some(bytes) = tr.get(&hold_key(namespace, pool), false).await? else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD {
        return Err(CatalogError::Corrupt("节点池占用记录过长"));
    }
    let (version, _, serial, epoch, operation, _): (
        u8,
        NodeMutationTaskKind,
        String,
        u64,
        String,
        [u8; 32],
    ) = serde_json::from_slice(&bytes).map_err(|_| CatalogError::Corrupt("节点池占用编码"))?;
    let operation =
        node_mutation::id(&operation).map_err(|_| CatalogError::Corrupt("节点池占用身份"))?;
    validate_physical_serial(&serial).map_err(|_| CatalogError::Corrupt("节点池占用带库"))?;
    if version != 2 || epoch == 0 {
        return Err(CatalogError::Corrupt("节点池占用版本或代次"));
    }
    let raw = tr
        .get(
            &identity_key(namespace, &serial, b'o', epoch, operation),
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("节点池占用缺少任务"))?;
    let task = decode(&raw, &serial, namespace, epoch, operation)?;
    if task.pool != pool
        || !matches!(
            task.state,
            NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
        )
        || hold_value(&task)?.as_slice() != bytes.as_ref()
        || tr
            .get(
                &identity_key(namespace, &serial, b'q', epoch, operation),
                false,
            )
            .await?
            .as_deref()
            != Some(&[])
        || tr
            .get(
                &global_pending_key(namespace, &serial, epoch, operation),
                false,
            )
            .await?
            .as_deref()
            != Some(serial.as_bytes())
    {
        return Err(CatalogError::Corrupt("节点池占用与任务或待处理索引不一致"));
    }
    if task.state == NodeMutationTaskState::Writing {
        match task.kind {
            NodeMutationTaskKind::SingleNode => {
                node_attempts::require_task_at(tr, &task).await?;
            }
            NodeMutationTaskKind::Rename => {
                node_rename::require_writing_task_at(tr, &task).await?;
            }
        }
    }
    Ok(Some(task))
}

/// Write-side barrier only. Queries continue to read the committed namespace.
pub(in crate::managed_catalog) async fn require_pool_writable_at(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
) -> Result<()> {
    require_pool_without_reclaim_at(tr, namespace, pool).await?;
    super::reclaim::require_pool_write_at(tr, namespace, pool).await
}

pub(in crate::managed_catalog) async fn require_pool_without_reclaim_at(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
) -> Result<()> {
    native_media::require_pool_readable(tr, namespace, pool).await?;
    if held_task_at(tr, namespace, pool).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

pub(super) async fn task_at(
    tr: &Transaction,
    serial: &str,
    namespace: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<NodeMutationTask>> {
    Ok(task_status_at(tr, serial, namespace, epoch, operation)
        .await?
        .map(|status| status.task))
}

async fn task_status_at(
    tr: &Transaction,
    serial: &str,
    namespace: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<NodeMutationTaskStatus>> {
    let raw = tr
        .get(
            &identity_key(namespace, serial, b'o', epoch, operation),
            false,
        )
        .await?;
    let queued = tr
        .get(
            &identity_key(namespace, serial, b'q', epoch, operation),
            false,
        )
        .await?;
    let global = tr
        .get(
            &global_pending_key(namespace, serial, epoch, operation),
            false,
        )
        .await?;
    let Some(raw) = raw else {
        if queued.is_some() || global.is_some() {
            return Err(CatalogError::Corrupt("节点队列存在孤立索引"));
        }
        node_attempts::task_outcome_at(tr, namespace, serial, epoch, operation, None).await?;
        return Ok(None);
    };
    let task = decode(&raw, serial, namespace, epoch, operation)?;
    let valid = match task.state {
        NodeMutationTaskState::Queued
        | NodeMutationTaskState::Prepared
        | NodeMutationTaskState::Writing => {
            queued.as_deref() == Some(&[]) && global.as_deref() == Some(serial.as_bytes())
        }
        NodeMutationTaskState::Cancelled
        | NodeMutationTaskState::Rejected
        | NodeMutationTaskState::Completed => queued.is_none() && global.is_none(),
    };
    if !valid {
        return Err(CatalogError::Corrupt("节点任务与待处理索引不一致"));
    }
    let held = held_task_at(tr, namespace, task.pool).await?;
    let owns_hold = held.as_ref().is_some_and(|held| held == &task);
    if matches!(
        task.state,
        NodeMutationTaskState::Prepared | NodeMutationTaskState::Writing
    ) != owns_hold
    {
        return Err(CatalogError::Corrupt("节点任务与池占用不一致"));
    }
    let outcome =
        node_attempts::task_outcome_at(tr, namespace, serial, epoch, operation, Some(&task))
            .await?;
    Ok(Some(NodeMutationTaskStatus { task, outcome }))
}

/// Bounded namespace-wide guard for a shared management retention window.
/// Future window advancement must check the same reverse range transactionally.
async fn queued_before(tr: &Transaction, namespace: Uuid, before: u64) -> Result<bool> {
    let prefix = namespace_prefix(namespace, b'p');
    let mut begin = prefix.clone();
    begin.extend_from_slice(&0u64.to_be_bytes());
    let mut end = prefix.clone();
    end.extend_from_slice(&before.to_be_bytes());
    let rows = tr
        .get_range(
            &RangeOption {
                begin: KeySelector::first_greater_or_equal(begin),
                end: KeySelector::first_greater_or_equal(end),
                limit: Some(1),
                ..Default::default()
            },
            1,
            false,
        )
        .await?;
    let Some(row) = rows.iter().next() else {
        return Ok(false);
    };
    if row.key().len() < prefix.len() + 26 {
        return Err(CatalogError::Corrupt("节点代次反向索引长度"));
    }
    let epoch = read_u64(&row.key()[prefix.len()..prefix.len() + 8]);
    let operation = uuid_from(&row.key()[prefix.len() + 8..prefix.len() + 24])?;
    let serial = std::str::from_utf8(row.value())
        .map_err(|_| CatalogError::Corrupt("节点代次反向索引带库编码"))?;
    validate_physical_serial(serial)
        .map_err(|_| CatalogError::Corrupt("节点代次反向索引带库身份"))?;
    if epoch == 0
        || operation.is_nil()
        || global_pending_key(namespace, serial, epoch, operation) != row.key()
    {
        return Err(CatalogError::Corrupt("节点代次反向索引键值不一致"));
    }
    let task = task_at(tr, serial, namespace, epoch, operation)
        .await?
        .ok_or(CatalogError::Corrupt("节点代次反向索引缺少任务"))?;
    if !matches!(
        task.state,
        NodeMutationTaskState::Queued
            | NodeMutationTaskState::Prepared
            | NodeMutationTaskState::Writing
    ) {
        return Err(CatalogError::Corrupt("节点代次反向索引指向终态任务"));
    }
    Ok(true)
}
pub(super) async fn retained(
    tr: &Transaction,
    namespace: Uuid,
    epoch: u64,
) -> Result<RequestWindow> {
    let window = management::window(tr, namespace).await?;
    if queued_before(tr, namespace, window.first_retained).await? {
        return Err(CatalogError::Corrupt("未完成节点任务已超出保留窗口"));
    }
    if epoch < window.first_retained || epoch > window.accepting {
        return Err(CatalogError::Invalid("节点任务代次不在保留窗口"));
    }
    Ok(window)
}
fn matches_request(
    task: &NodeMutationTask,
    request: &impl LogicalNodeRequest,
    object: &NodePlanObject,
) -> bool {
    let c = request.task_scope();
    task.kind == c.kind
        && task.library_serial == c.library_serial
        && task.namespace == c.namespace
        && task.pool == c.pool
        && task.request_epoch == c.request_epoch
        && task.operation == c.operation
        && task.fingerprint == request.fingerprint()
        && task.object == *object
}
async fn known_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    request: &impl LogicalNodeRequest,
    object: &NodePlanObject,
) -> Result<Option<NodeMutationTask>> {
    let c = request.task_scope();
    let namespace =
        routing::require_runtime_active_owner(tr, &c.library_serial, route, owner).await?;
    if namespace != c.namespace {
        return Err(CatalogError::Invalid("节点请求不属于当前命名空间"));
    }
    retained(tr, namespace, c.request_epoch).await?;
    let task = task_at(
        tr,
        &c.library_serial,
        namespace,
        c.request_epoch,
        c.operation,
    )
    .await?;
    if task
        .as_ref()
        .is_some_and(|task| !matches_request(task, request, object))
    {
        return Err(CatalogError::Invalid("节点请求身份已用于另一份内容"));
    }
    Ok(task)
}

async fn require_request_pool(tr: &Transaction, request: &impl LogicalNodeRequest) -> Result<()> {
    let c = request.task_scope();
    require_pool_library(tr, c.namespace, c.pool, &c.library_serial).await?;
    let pool = decode_pool(
        c.pool,
        &tr.get(&pool_key(c.namespace, c.pool), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    active_pools::require_indexes(tr, c.namespace, &pool).await
}

pub(super) async fn require_prepared_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    expected: &NodeMutationTask,
    request: &NodeMutationRequest,
) -> Result<()> {
    let current = known_at(tr, route, owner, request, &expected.object).await?;
    if expected.state != NodeMutationTaskState::Prepared || current.as_ref() != Some(expected) {
        return Err(CatalogError::Invalid(
            "节点输入要求原任务仍处于 Prepared 状态",
        ));
    }
    Ok(())
}

/// Called after every admission check and attempt encoding, before the
/// caller's infallible attempt/gate sets. No error may follow these writes.
pub(super) fn mark_writing_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
) -> Result<NodeMutationTask> {
    mark_kind_writing_at(tr, task, attempt, NodeMutationTaskKind::SingleNode)
}
pub(super) fn mark_rename_writing_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
) -> Result<NodeMutationTask> {
    mark_kind_writing_at(tr, task, attempt, NodeMutationTaskKind::Rename)
}
fn mark_kind_writing_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
    kind: NodeMutationTaskKind,
) -> Result<NodeMutationTask> {
    if task.kind != kind || task.state != NodeMutationTaskState::Prepared || attempt.is_nil() {
        return Err(CatalogError::Invalid(
            "节点尝试要求 Prepared 任务与非空身份",
        ));
    }
    let mut writing = task.clone();
    writing.state = NodeMutationTaskState::Writing;
    writing.active_attempt = Some(attempt);
    let encoded = encode(&writing)?;
    let held = hold_value(&writing)?;
    tr.set(
        &identity_key(
            task.namespace,
            &task.library_serial,
            b'o',
            task.request_epoch,
            task.operation,
        ),
        &encoded,
    );
    tr.set(&hold_key(task.namespace, task.pool), &held);
    Ok(writing)
}

/// A proven absent physical attempt returns to Prepared while the original
/// logical request, preparation timestamp, queue entries and pool hold remain.
pub(super) fn return_prepared_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
) -> Result<()> {
    if task.state != NodeMutationTaskState::Writing || task.active_attempt != Some(attempt) {
        return Err(CatalogError::Invalid("关闭节点尝试要求原 Writing 任务"));
    }
    let mut prepared = task.clone();
    prepared.state = NodeMutationTaskState::Prepared;
    prepared.active_attempt = None;
    let encoded = encode(&prepared)?;
    let held = hold_value(&prepared)?;
    tr.set(
        &identity_key(
            task.namespace,
            &task.library_serial,
            b'o',
            task.request_epoch,
            task.operation,
        ),
        &encoded,
    );
    tr.set(&hold_key(task.namespace, task.pool), &held);
    Ok(())
}

/// Called only after the publication result, canonical rows and object
/// references have been validated/encoded. No fallible work may follow.
pub(super) fn complete_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
    owner: u64,
    time: u64,
    digest: [u8; 32],
) -> Result<()> {
    complete_kind_at(
        tr,
        task,
        attempt,
        owner,
        time,
        digest,
        NodeMutationTaskKind::SingleNode,
    )
}

pub(super) fn complete_rename_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
    owner: u64,
    time: u64,
    digest: [u8; 32],
) -> Result<()> {
    complete_kind_at(
        tr,
        task,
        attempt,
        owner,
        time,
        digest,
        NodeMutationTaskKind::Rename,
    )
}

fn complete_kind_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    attempt: Uuid,
    owner: u64,
    time: u64,
    digest: [u8; 32],
    kind: NodeMutationTaskKind,
) -> Result<()> {
    if task.kind != kind
        || task.state != NodeMutationTaskState::Writing
        || task.active_attempt != Some(attempt)
    {
        return Err(CatalogError::Invalid("目录发布要求原 Writing 任务"));
    }
    let mut completed = task.clone();
    completed.state = NodeMutationTaskState::Completed;
    completed.active_attempt = None;
    completed.completed_owner_epoch = Some(owner);
    completed.completed_at_millis = Some(time);
    completed.publication_digest = Some(digest);
    let encoded = encode(&completed)?;
    tr.set(
        &identity_key(
            task.namespace,
            &task.library_serial,
            b'o',
            task.request_epoch,
            task.operation,
        ),
        &encoded,
    );
    tr.clear(&identity_key(
        task.namespace,
        &task.library_serial,
        b'q',
        task.request_epoch,
        task.operation,
    ));
    tr.clear(&global_pending_key(
        task.namespace,
        &task.library_serial,
        task.request_epoch,
        task.operation,
    ));
    tr.clear(&hold_key(task.namespace, task.pool));
    Ok(())
}

impl ManagedCatalog {
    /// Freeze the original logical baseline against new pool writes. This is
    /// metadata admission only: no media has been selected or written yet.
    pub async fn prepare_node_mutation_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        let (_, request) = self
            .node_mutation_task_request(serial, route, owner, epoch, operation, max_bytes)
            .await?;
        self.prepare_logical_node_request(route, owner, &request, max_bytes)
            .await
    }
    /// Persist a stable request and its queue entries. Queued is metadata-only;
    /// it neither locks this baseline nor authorizes a physical attempt.
    pub async fn queue_node_mutation_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeMutationRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        self.queue_logical_node_request(route, owner, request, max_bytes)
            .await
    }

    pub async fn node_mutation_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeMutationTask>> {
        Ok(self
            .node_mutation_task_status(serial, route, epoch, operation)
            .await?
            .map(|status| status.task))
    }

    /// One read version validates the task, its indexes and its original result.
    /// This query does not require a live owner or load the original request.
    pub async fn node_mutation_task_status(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NodeMutationTaskStatus>> {
        validate_physical_serial(serial)?;
        valid_identity(epoch, operation)?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        retained(&tr, namespace, epoch).await?;
        task_status_at(&tr, serial, namespace, epoch, operation).await
    }

    pub async fn pending_node_mutation_tasks(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<NodeMutationTask>> {
        validate_physical_serial(serial)?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid("节点任务分页大小必须在 1..=100"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let window = retained(
            &tr,
            namespace,
            management::window(&tr, namespace).await?.accepting,
        )
        .await?;
        let prefix = library_prefix(namespace, serial, b'q');
        if let Some(cursor) = after {
            if !cursor.starts_with(&prefix) || cursor.len() != prefix.len() + 24 {
                return Err(CatalogError::Invalid("节点任务游标作用域或长度无效"));
            }
            let epoch = read_u64(&cursor[prefix.len()..prefix.len() + 8]);
            let operation = uuid_from(&cursor[prefix.len() + 8..])?;
            if epoch < window.first_retained || epoch > window.accepting || operation.is_nil() {
                return Err(CatalogError::Invalid("节点任务游标身份或代次无效"));
            }
        }
        let page = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, value) in page.items {
            if key.len() != prefix.len() + 24 || !value.is_empty() {
                return Err(CatalogError::Corrupt("节点待处理索引编码"));
            }
            let epoch = read_u64(&key[prefix.len()..prefix.len() + 8]);
            let operation = uuid_from(&key[prefix.len() + 8..])?;
            if epoch < window.first_retained || epoch > window.accepting || operation.is_nil() {
                return Err(CatalogError::Corrupt("节点待处理索引代次或身份"));
            }
            let task = task_at(&tr, serial, namespace, epoch, operation)
                .await?
                .ok_or(CatalogError::Corrupt("节点待处理索引缺少任务"))?;
            if !matches!(
                task.state,
                NodeMutationTaskState::Queued
                    | NodeMutationTaskState::Prepared
                    | NodeMutationTaskState::Writing
            ) {
                return Err(CatalogError::Corrupt("节点待处理索引指向终态任务"));
            }
            items.push(task);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }

    /// A missing task is not cancellation acknowledgement. Only a registered
    /// queued/prepared task can transition; holds, indexes and terminal are
    /// atomic. Prepared never authorizes a physical attempt in this protocol.
    pub async fn cancel_node_mutation_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<NodeMutationTask> {
        self.finish_unwritten_node_task(
            serial,
            route,
            owner,
            epoch,
            operation,
            (NodeMutationTaskKind::SingleNode, None),
        )
        .await
    }

    /// Retain a permanent refusal before any physical attempt. Writing must
    /// first be recovered and closed with opaque absence evidence.
    pub async fn reject_node_mutation_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        reason: &str,
    ) -> Result<NodeMutationTask> {
        self.finish_unwritten_node_task(
            serial,
            route,
            owner,
            epoch,
            operation,
            (NodeMutationTaskKind::SingleNode, Some(reason)),
        )
        .await
    }

    async fn finish_unwritten_node_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        termination: (NodeMutationTaskKind, Option<&str>),
    ) -> Result<NodeMutationTask> {
        let (kind, rejection) = termination;
        validate_physical_serial(serial)?;
        valid_identity(epoch, operation)?;
        if kind == NodeMutationTaskKind::Rename {
            let tr = self.db.create_trx()?;
            let namespace =
                routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
            retained(&tr, namespace, epoch).await?;
            let task = task_at(&tr, serial, namespace, epoch, operation).await?;
            drop(tr);
            if let Some(task) = task
                .filter(|task| task.kind == kind && task.state == NodeMutationTaskState::Prepared)
            {
                if rejection.is_some_and(|reason| !valid_rejection(reason)) {
                    return Err(CatalogError::Invalid(
                        "节点拒绝原因须为 1..=500 字节的非空无控制字符文本",
                    ));
                }
                self.close_rename_preparation_for_task(route, owner, &task)
                    .await?;
            }
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let namespace =
                            routing::require_runtime_active_owner(&tr, serial, route, owner)
                                .await?;
                        retained(&tr, namespace, epoch).await?;
                        let mut task = task_at(&tr, serial, namespace, epoch, operation)
                            .await?
                            .ok_or(CatalogError::Invalid("节点任务尚未登记"))?;
                        if task.kind != kind {
                            return Err(CatalogError::Invalid("节点终止接口与原请求类型不符"));
                        }
                        if matches!(
                            task.state,
                            NodeMutationTaskState::Cancelled
                                | NodeMutationTaskState::Rejected
                                | NodeMutationTaskState::Completed
                        ) {
                            return Ok(task);
                        }
                        if task.state == NodeMutationTaskState::Writing {
                            return Err(CatalogError::Invalid(
                                "Writing 节点任务须先恢复物理尝试，不能取消或拒绝",
                            ));
                        }
                        if rejection.is_some_and(|reason| !valid_rejection(reason)) {
                            return Err(CatalogError::Invalid(
                                "节点拒绝原因须为 1..=500 字节的非空无控制字符文本",
                            ));
                        }
                        let prepared = task.state == NodeMutationTaskState::Prepared;
                        if prepared && kind == NodeMutationTaskKind::Rename {
                            node_rename::require_no_prepared_attempt_at(&tr, &task).await?;
                        }
                        task.state = if rejection.is_some() {
                            NodeMutationTaskState::Rejected
                        } else {
                            NodeMutationTaskState::Cancelled
                        };
                        task.rejection_reason = rejection.map(str::to_owned);
                        task.completed_owner_epoch = Some(owner);
                        task.completed_at_millis = Some(
                            management::now()
                                .max(task.prepared_at_millis.unwrap_or(task.created_at_millis)),
                        );
                        let encoded = encode(&task)?;
                        tr.set(
                            &identity_key(namespace, serial, b'o', epoch, operation),
                            &encoded,
                        );
                        tr.clear(&identity_key(namespace, serial, b'q', epoch, operation));
                        tr.clear(&global_pending_key(namespace, serial, epoch, operation));
                        if prepared {
                            tr.clear(&hold_key(namespace, task.pool));
                        }
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Load the exact registered body for recovery. The returned snapshot is
    /// not a path hold or a write permit; execution must claim it atomically.
    pub async fn node_mutation_task_request(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<(NodeMutationTask, NodeMutationRequest)> {
        validate_physical_serial(serial)?;
        valid_identity(epoch, operation)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        retained(&tr, namespace, epoch).await?;
        let task = task_at(&tr, serial, namespace, epoch, operation)
            .await?
            .ok_or(CatalogError::Invalid("节点任务尚未登记"))?;
        if task.kind != NodeMutationTaskKind::SingleNode {
            return Err(CatalogError::Invalid("原任务不是单节点修改"));
        }
        drop(tr);
        let request = self
            .load_node_mutation_request(route, owner, &task.object, max_bytes)
            .await?;
        if !matches_request(&task, &request, &task.object) {
            return Err(CatalogError::Corrupt("节点任务与封口请求内容不一致"));
        }
        Ok((task, request))
    }
}

#[cfg(test)]
#[path = "node_tasks/rejection_tests.rs"]
mod rejection_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TIME: &str = "2026-10-31T08:00:00.000000000Z";

    #[test]
    fn rejected_task_codec_keeps_the_original_reason_and_terminal_identity() {
        let ns = Uuid::new_v4();
        let serial = "REJECTED-CODEC";
        let operation = Uuid::new_v4();
        let task = NodeMutationTask {
            kind: NodeMutationTaskKind::SingleNode,
            library_serial: serial.into(),
            namespace: ns,
            pool: Uuid::new_v4(),
            request_epoch: 1,
            operation,
            object: NodePlanObject::for_bytes(serial, ns, b"inert request", 100).unwrap(),
            fingerprint: [8; 32],
            state: NodeMutationTaskState::Queued,
            accepted_owner_epoch: 100,
            created_at_millis: 1_798_000_000_000,
            prepared_owner_epoch: None,
            prepared_at_millis: None,
            active_attempt: None,
            completed_owner_epoch: None,
            completed_at_millis: None,
            publication_digest: None,
            rejection_reason: None,
        };
        let mut value: serde_json::Value = serde_json::from_slice(&encode(&task).unwrap()).unwrap();
        value["state"] = json!("rejected");
        value["completed_owner"] = json!(101);
        value["completed_at_millis"] = json!(task.created_at_millis + 1);
        value["rejection_reason"] = json!("原节点版本条件已失效");
        let bytes = node_mutation::canonical(&value).unwrap();
        let rejected = decode(&bytes, serial, ns, 1, operation)
            .expect("未写入的拒绝任务必须保存原原因和完成身份");
        assert_eq!(encode(&rejected).unwrap(), bytes);
        assert_eq!(rejected.state, NodeMutationTaskState::Rejected);
        assert_eq!(
            rejected.rejection_reason.as_deref(),
            Some("原节点版本条件已失效")
        );
        for reason in [
            json!(null),
            json!(""),
            json!("   "),
            json!("bad\nreason"),
            json!("\0"),
            json!("原".repeat(167)),
        ] {
            let mut bad = value.clone();
            bad["rejection_reason"] = reason;
            assert!(
                decode(
                    &node_mutation::canonical(&bad).unwrap(),
                    serial,
                    ns,
                    1,
                    operation
                )
                .is_err()
            );
        }
        for (field, replacement) in [
            ("version", json!(4)),
            ("active_attempt", json!(Uuid::new_v4().to_string())),
            ("publication_digest", json!(vec![1u8; 32])),
            ("completed_owner", json!(99)),
            ("completed_at_millis", json!(task.created_at_millis - 1)),
        ] {
            let mut bad = value.clone();
            bad[field] = replacement;
            assert!(
                decode(
                    &node_mutation::canonical(&bad).unwrap(),
                    serial,
                    ns,
                    1,
                    operation
                )
                .is_err()
            );
        }
        let mut boundary = rejected.clone();
        boundary.rejection_reason = Some(format!("{}ab", "原".repeat(166)));
        assert_eq!(
            decode(&encode(&boundary).unwrap(), serial, ns, 1, operation).unwrap(),
            boundary
        );
        let mut queued = task;
        queued.rejection_reason = rejected.rejection_reason;
        assert!(encode(&queued).is_err());
    }

    #[test]
    fn queued_task_codec_and_library_epoch_indexes_are_strict() {
        let ns = Uuid::new_v4();
        let serial = "TASK-CODEC";
        let operation = Uuid::new_v4();
        let task = NodeMutationTask {
            kind: NodeMutationTaskKind::SingleNode,
            library_serial: serial.into(),
            namespace: ns,
            pool: Uuid::new_v4(),
            request_epoch: 1,
            operation,
            object: NodePlanObject::for_bytes(serial, ns, b"inert bytes", 100).unwrap(),
            fingerprint: [7; 32],
            state: NodeMutationTaskState::Queued,
            accepted_owner_epoch: 100,
            created_at_millis: 1_798_000_000_000,
            prepared_owner_epoch: None,
            prepared_at_millis: None,
            active_attempt: None,
            completed_owner_epoch: None,
            completed_at_millis: None,
            publication_digest: None,
            rejection_reason: None,
        };
        let bytes = encode(&task).unwrap();
        assert!(bytes.len() < MAX_RECORD);
        assert_eq!(decode(&bytes, serial, ns, 1, operation).unwrap(), task);
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut], serial, ns, 1, operation).is_err());
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for field in value.as_object().unwrap().keys() {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                decode(bad.to_string().as_bytes(), serial, ns, 1, operation).is_err(),
                "missing {field}"
            );
        }
        for (field, replacement) in [
            ("version", json!(0)),
            ("version", json!(1)),
            ("version", json!(2)),
            ("version", json!(3)),
            ("version", json!(4)),
            ("version", json!(5)),
            ("epoch", json!(0)),
            ("namespace", json!(Uuid::nil().to_string())),
            ("pool", json!(Uuid::nil().to_string())),
            ("operation", json!(Uuid::nil().to_string())),
            ("accepted_owner", json!(0)),
            ("created_at_millis", json!(0)),
            ("created_at_millis", json!(MAX_TIME + 1)),
            ("completed_owner", json!(100)),
            ("completed_at_millis", json!(1_798_000_000_001u64)),
            ("publication_digest", json!(vec![3u8; 32])),
            ("state", json!("writing")),
            ("state", json!("succeeded")),
            ("state", json!("cancelled")),
            ("object", json!("?")),
            ("extra", json!(true)),
        ] {
            let mut bad = value.clone();
            bad[field] = replacement;
            assert!(
                matches!(
                    decode(bad.to_string().as_bytes(), serial, ns, 1, operation),
                    Err(CatalogError::Corrupt(_))
                ),
                "bad {field}"
            );
        }
        assert!(decode(&bytes, "OTHER", ns, 1, operation).is_err());
        assert!(decode(&bytes, serial, Uuid::new_v4(), 1, operation).is_err());
        assert!(decode(&bytes, serial, ns, 2, operation).is_err());
        assert!(decode(&bytes, serial, ns, 1, Uuid::new_v4()).is_err());
        let mut cancelled = task.clone();
        cancelled.state = NodeMutationTaskState::Cancelled;
        cancelled.completed_owner_epoch = Some(101);
        cancelled.completed_at_millis = Some(task.created_at_millis + 1);
        assert_eq!(
            decode(&encode(&cancelled).unwrap(), serial, ns, 1, operation).unwrap(),
            cancelled
        );
        cancelled.completed_owner_epoch = Some(99);
        assert!(encode(&cancelled).is_err());
        cancelled.completed_owner_epoch = Some(101);
        cancelled.completed_at_millis = Some(task.created_at_millis - 1);
        assert!(encode(&cancelled).is_err());
        let mut prepared = task.clone();
        prepared.state = NodeMutationTaskState::Prepared;
        assert!(encode(&prepared).is_err());
        prepared.prepared_owner_epoch = Some(101);
        prepared.prepared_at_millis = Some(task.created_at_millis + 2);
        assert_eq!(
            decode(&encode(&prepared).unwrap(), serial, ns, 1, operation).unwrap(),
            prepared
        );
        let mut writing = prepared.clone();
        writing.state = NodeMutationTaskState::Writing;
        assert!(encode(&writing).is_err());
        writing.active_attempt = Some(Uuid::new_v4());
        assert_eq!(
            decode(&encode(&writing).unwrap(), serial, ns, 1, operation).unwrap(),
            writing
        );
        let mut rename_writing = writing.clone();
        rename_writing.kind = NodeMutationTaskKind::Rename;
        assert_eq!(
            decode(&encode(&rename_writing).unwrap(), serial, ns, 1, operation).unwrap(),
            rename_writing
        );
        rename_writing.state = NodeMutationTaskState::Completed;
        rename_writing.active_attempt = None;
        rename_writing.completed_owner_epoch = Some(102);
        rename_writing.completed_at_millis = Some(task.created_at_millis + 3);
        rename_writing.publication_digest = Some([8; 32]);
        assert_eq!(
            decode(&encode(&rename_writing).unwrap(), serial, ns, 1, operation).unwrap(),
            rename_writing
        );
        writing.active_attempt = Some(Uuid::nil());
        assert!(encode(&writing).is_err());
        writing.active_attempt = Some(Uuid::new_v4());
        writing.state = NodeMutationTaskState::Prepared;
        assert!(encode(&writing).is_err());
        let mut completed = prepared.clone();
        completed.state = NodeMutationTaskState::Completed;
        completed.completed_owner_epoch = Some(102);
        completed.completed_at_millis = Some(task.created_at_millis + 3);
        assert!(encode(&completed).is_err());
        completed.publication_digest = Some([8; 32]);
        assert_eq!(
            decode(&encode(&completed).unwrap(), serial, ns, 1, operation).unwrap(),
            completed
        );
        completed.active_attempt = Some(Uuid::new_v4());
        assert!(encode(&completed).is_err());
        completed.active_attempt = None;
        completed.state = NodeMutationTaskState::Prepared;
        assert!(encode(&completed).is_err());
        let mut bad = prepared.clone();
        bad.state = NodeMutationTaskState::Queued;
        assert!(encode(&bad).is_err());
        bad = prepared.clone();
        bad.prepared_owner_epoch = Some(99);
        assert!(encode(&bad).is_err());
        bad = prepared.clone();
        bad.prepared_at_millis = Some(task.created_at_millis - 1);
        assert!(encode(&bad).is_err());
        prepared.state = NodeMutationTaskState::Cancelled;
        prepared.completed_owner_epoch = Some(100);
        prepared.completed_at_millis = Some(task.created_at_millis + 3);
        assert!(encode(&prepared).is_err());
        prepared.completed_owner_epoch = Some(102);
        prepared.completed_at_millis = Some(task.created_at_millis + 1);
        assert!(encode(&prepared).is_err());
        prepared.completed_at_millis = Some(task.created_at_millis + 3);
        assert_eq!(
            decode(&encode(&prepared).unwrap(), serial, ns, 1, operation).unwrap(),
            prepared
        );
        let q = identity_key(ns, serial, b'q', 1, operation);
        assert!(q.starts_with(&library_prefix(ns, serial, b'q')));
        assert!(!q.starts_with(&library_prefix(ns, "TASK-CODEC-X", b'q')));
        assert!(!q.starts_with(&library_prefix(Uuid::new_v4(), serial, b'q')));
        assert!(q < identity_key(ns, serial, b'q', 2, operation));
        assert!(
            global_pending_key(ns, serial, 1, operation)
                < global_pending_key(ns, serial, 2, operation)
        );
        assert_ne!(
            global_pending_key(ns, serial, 1, operation),
            global_pending_key(ns, "OTHER", 1, operation)
        );
        assert_ne!(library_prefix(ns, serial, b'q'), namespace_prefix(ns, b'p'));
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；节点请求排队、取消、索引与接任，不操作介质"]
    async fn queued_node_tasks_are_idempotent_scoped_cancel_safe_and_recoverable() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-NODE-TASK-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "tasks".into(),
            file_limit: 200_000,
            created_at_millis: 1_798_000_000_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let spec = NodeMutationSpec {
            pool: pool.id,
            operation: Uuid::new_v4(),
            path: "/link".into(),
            expected_version: None,
            mutation: NodeMutation::CreateSymlink {
                target: "large-target".repeat(20_000),
            },
        };
        let request = catalog
            .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
            .await
            .unwrap();
        let bytes = request.encode().unwrap();
        let object = NodePlanObject::for_bytes(&serial, ns, &bytes, 1 << 20).unwrap();
        assert!(matches!(
            catalog
                .cancel_node_mutation_task(&serial, route, 100, 1, spec.operation)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &object, &mut &bytes[..64_003])
                .await
                .is_err()
        );
        assert!(
            catalog
                .node_mutation_task(&serial, route, 1, spec.operation)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 100, None, 100)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        // A result without a task is corruption, never a fresh operation.
        // The fixture deliberately uses a broken Qr payload; it is not S4.
        let mut orphan_result = namespace_prefix(ns, b'r');
        append_component(&mut orphan_result, serial.as_bytes());
        orphan_result.extend_from_slice(&1u64.to_be_bytes());
        orphan_result.extend_from_slice(spec.operation.as_bytes());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&orphan_result, b"orphan");
        tr.commit().await.unwrap();
        assert!(
            matches!(
                catalog
                    .node_mutation_task(&serial, route, 1, spec.operation)
                    .await,
                Err(CatalogError::Corrupt(_))
            ),
            "missing task must not ignore its orphan result"
        );
        assert!(matches!(
            catalog
                .queue_node_mutation_request(route, 100, &request, 1 << 20)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog
                .cancel_node_mutation_task(&serial, route, 100, 1, spec.operation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&orphan_result, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            b"orphan"
        );
        for row in [
            identity_key(ns, &serial, b'o', 1, spec.operation),
            identity_key(ns, &serial, b'q', 1, spec.operation),
            global_pending_key(ns, &serial, 1, spec.operation),
        ] {
            assert!(tr.get(&row, false).await.unwrap().is_none());
        }
        tr.clear(&orphan_result);
        tr.commit().await.unwrap();
        let queued = catalog
            .queue_node_mutation_request(route, 100, &request, 1 << 20)
            .await
            .unwrap();
        assert_eq!(queued.state, NodeMutationTaskState::Queued);
        assert_eq!(queued.accepted_owner_epoch, 100);
        assert_ne!(queued.fingerprint, queued.object.digest());
        for _ in 0..3 {
            assert_eq!(
                catalog
                    .queue_node_mutation_request(route, 100, &request, 1 << 20)
                    .await
                    .unwrap(),
                queued
            );
        }
        assert_eq!(
            catalog
                .node_mutation_task(&serial, route, 1, spec.operation)
                .await
                .unwrap()
                .unwrap(),
            queued
        );
        let conflicting = catalog
            .prepare_node_mutation_request(
                &serial,
                route,
                100,
                &spec,
                "2026-10-31T08:00:01.000000000Z",
            )
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .queue_node_mutation_request(route, 100, &conflicting, 1 << 20)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        let mut requests_to_queue = vec![request.clone()];
        for name in ["/second", "/third"] {
            let spec = NodeMutationSpec {
                operation: Uuid::new_v4(),
                path: name.into(),
                mutation: NodeMutation::CreateDirectory,
                ..spec.clone()
            };
            let next = catalog
                .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
                .await
                .unwrap();
            catalog
                .queue_node_mutation_request(route, 100, &next, 1 << 20)
                .await
                .unwrap();
            requests_to_queue.push(next);
        }
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = catalog
                .pending_node_mutation_tasks(&serial, route, 100, cursor.as_deref(), 1)
                .await
                .unwrap();
            seen.extend(page.items.into_iter().map(|task| task.operation));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        let mut expected = requests_to_queue
            .iter()
            .map(|r| r.context().operation)
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(seen, expected);
        for limit in [0, 101] {
            assert!(
                catalog
                    .pending_node_mutation_tasks(&serial, route, 100, None, limit)
                    .await
                    .is_err()
            );
        }
        let wrong_cursor = identity_key(ns, "OTHER", b'q', 1, spec.operation);
        assert!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 100, Some(&wrong_cursor), 1)
                .await
                .is_err()
        );
        assert!(
            queued_before(&catalog.db.create_trx().unwrap(), ns, 2)
                .await
                .unwrap()
        );
        assert!(
            !queued_before(&catalog.db.create_trx().unwrap(), ns, 1)
                .await
                .unwrap()
        );

        let other = &requests_to_queue[1];
        let op = other.context().operation;
        let record = identity_key(ns, &serial, b'o', 1, op);
        let queue = identity_key(ns, &serial, b'q', 1, op);
        let global = global_pending_key(ns, &serial, 1, op);
        for key in [&record, &queue, &global] {
            let tr = catalog.db.create_trx().unwrap();
            let saved = tr.get(key, false).await.unwrap().unwrap().to_vec();
            tr.clear(key);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog.node_mutation_task(&serial, route, 1, op).await,
                Err(CatalogError::Corrupt(_))
            ));
            assert!(matches!(
                catalog
                    .queue_node_mutation_request(route, 100, other, 1 << 20)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            assert!(matches!(
                catalog
                    .cancel_node_mutation_task(&serial, route, 100, 1, op)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert!(tr.get(key, false).await.unwrap().is_none());
            tr.set(key, &saved);
            tr.commit().await.unwrap();
        }
        let tr = catalog.db.create_trx().unwrap();
        let saved = tr.get(&record, false).await.unwrap().unwrap().to_vec();
        let mut corrupt: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        corrupt["fingerprint"] = json!(vec![3u8; 32]);
        tr.set(&record, corrupt.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .node_mutation_task_request(&serial, route, 100, 1, op, 1 << 20)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&record, &saved);
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .queue_node_mutation_request(route, 100, &request, 1 << 20)
                .await
                .unwrap(),
            queued
        );
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(
            catalog
                .queue_node_mutation_request(route, 100, &request, 1 << 20)
                .await
                .is_err()
        );
        assert!(
            catalog
                .cancel_node_mutation_task(&serial, route, 100, 1, op)
                .await
                .is_err()
        );
        assert!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 100, None, 100)
                .await
                .is_err()
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .pending_node_mutation_tasks(&serial, route, 101, None, 100)
                .await
                .unwrap()
                .items
                .len(),
            3
        );
        let (recovered, body) = reopened
            .node_mutation_task_request(&serial, route, 101, 1, spec.operation, 1 << 20)
            .await
            .unwrap();
        assert_eq!(recovered, queued);
        assert_eq!(body.encode().unwrap(), bytes);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 2,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 101, None, 100)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        let (retry, cancelled, retry_again) = tokio::join!(
            catalog.queue_node_mutation_request(route, 101, &request, 1 << 20),
            catalog.cancel_node_mutation_task(&serial, route, 101, 1, spec.operation),
            catalog.queue_node_mutation_request(route, 101, &request, 1 << 20)
        );
        let cancelled = cancelled.unwrap();
        assert_eq!(cancelled.state, NodeMutationTaskState::Cancelled);
        assert_eq!(cancelled.accepted_owner_epoch, 100);
        assert_eq!(cancelled.completed_owner_epoch, Some(101));
        for result in [retry.unwrap(), retry_again.unwrap()] {
            assert_eq!(result.fingerprint, queued.fingerprint);
            assert_eq!(result.created_at_millis, queued.created_at_millis);
        }
        assert_eq!(
            catalog
                .cancel_node_mutation_task(&serial, route, 101, 1, spec.operation)
                .await
                .unwrap(),
            cancelled
        );
        assert_eq!(
            catalog
                .queue_node_mutation_request(route, 101, &request, 1 << 20)
                .await
                .unwrap(),
            cancelled
        );
        for request in &requests_to_queue[1..] {
            catalog
                .cancel_node_mutation_task(&serial, route, 101, 1, request.context().operation)
                .await
                .unwrap();
        }
        assert!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 101, None, 100)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert!(
            !queued_before(&catalog.db.create_trx().unwrap(), ns, 2)
                .await
                .unwrap()
        );
        // A terminal replay precedes pool checks and cannot resurrect deletion.
        let stale_spec = NodeMutationSpec {
            operation: Uuid::new_v4(),
            path: "/stale".into(),
            mutation: NodeMutation::CreateDirectory,
            ..spec.clone()
        };
        let stale = catalog
            .prepare_node_mutation_request(&serial, route, 101, &stale_spec, TIME)
            .await
            .unwrap();
        let root = active_pools::root_id(ns, pool.id);
        let tr = catalog.db.create_trx().unwrap();
        let mut changed =
            decode_node(&tr.get(&node_key(ns, root), false).await.unwrap().unwrap()).unwrap();
        changed.directory_version = Some(2);
        tr.set(&node_key(ns, root), &encode_node(&changed));
        tr.commit().await.unwrap();
        assert!(
            catalog
                .check_node_mutation_request(route, 101, &stale)
                .await
                .is_err()
        );
        // Registration is durable queueing, not a promise that this stale
        // precondition is still valid or that a media attempt can begin.
        let stale_task = catalog
            .queue_node_mutation_request(route, 101, &stale, 1 << 20)
            .await
            .unwrap();
        assert_eq!(stale_task.state, NodeMutationTaskState::Queued);
        catalog
            .cancel_node_mutation_task(
                &serial,
                route,
                101,
                stale.context().request_epoch,
                stale.context().operation,
            )
            .await
            .unwrap();
        let fresh_spec = NodeMutationSpec {
            operation: Uuid::new_v4(),
            path: "/late".into(),
            mutation: NodeMutation::CreateDirectory,
            ..spec.clone()
        };
        let fresh = catalog
            .prepare_node_mutation_request(&serial, route, 101, &fresh_spec, TIME)
            .await
            .unwrap();
        let deletion=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":2,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool-delete","id":pool.id.to_string(),"revision":1}})).unwrap();
        catalog
            .admit_management(&serial, route, 101, &deletion)
            .await
            .unwrap();
        let result = catalog
            .apply_management(&serial, route, 101, deletion.epoch(), deletion.operation())
            .await
            .unwrap();
        assert_eq!(result["status"], "succeeded");
        assert_eq!(
            catalog
                .queue_node_mutation_request(route, 101, &request, 1 << 20)
                .await
                .unwrap(),
            cancelled
        );
        assert!(
            catalog
                .queue_node_mutation_request(route, 101, &fresh, 1 << 20)
                .await
                .is_err()
        );
        let fresh_bytes = fresh.encode().unwrap();
        let absent_object = NodePlanObject::for_bytes(&serial, ns, &fresh_bytes, 1 << 20).unwrap();
        assert!(matches!(
            catalog
                .load_node_mutation_request(route, 101, &absent_object, 1 << 20)
                .await,
            Err(CatalogError::Incomplete)
        ));
        assert!(
            catalog
                .node_mutation_task(&serial, route, 2, fresh.context().operation)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .is_none()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 2,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        assert!(
            catalog
                .node_mutation_task(&serial, route, 1, spec.operation)
                .await
                .is_err()
        );
        assert!(
            !requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&node_key(ns, request.context().node), false)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；节点准备占用、故障索引、互斥和接任，不操作介质"]
    async fn prepared_nodes_hold_pool_writes_and_cancel_atomically_across_owners() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-NODE-TASK-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "tasks".into(),
            file_limit: 200_000,
            created_at_millis: 1_798_000_000_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();

        let other_pool = PoolRecord {
            id: Uuid::new_v4(),
            name: "other".into(),
            ..pool.clone()
        };
        catalog
            .create_active_pool(&serial, route, 100, &other_pool)
            .await
            .unwrap();
        let mut requests = Vec::new();
        for (pool, path) in [
            (pool.id, "/first"),
            (pool.id, "/second"),
            (other_pool.id, "/other"),
            (pool.id, "/cancel-queued"),
        ] {
            let spec = NodeMutationSpec {
                pool,
                operation: Uuid::new_v4(),
                path: path.into(),
                expected_version: None,
                mutation: NodeMutation::CreateDirectory,
            };
            let request = catalog
                .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
                .await
                .unwrap();
            catalog
                .queue_node_mutation_request(route, 100, &request, 1 << 20)
                .await
                .unwrap();
            requests.push(request);
        }
        let op = requests[0].context().operation;
        let other = requests[1].context().operation;
        let third = requests[2].context().operation;
        // A stale queued request cannot acquire any hold or advance its state.
        let root_key = node_key(ns, active_pools::root_id(ns, pool.id));
        let tr = catalog.db.create_trx().unwrap();
        let original = tr.get(&root_key, false).await.unwrap().unwrap().to_vec();
        let mut changed = decode_node(&original).unwrap();
        changed.directory_version = Some(2);
        tr.set(&root_key, &encode_node(&changed));
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .prepare_node_mutation_task(&serial, route, 100, 1, op, 1 << 20)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        assert_eq!(
            catalog
                .node_mutation_task(&serial, route, 1, op)
                .await
                .unwrap()
                .unwrap()
                .state,
            NodeMutationTaskState::Queued
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(!requests::has_namespace_holds(&tr, ns).await.unwrap());
        tr.set(&root_key, &original);
        tr.commit().await.unwrap();
        // Every existing file protocol's namespace-owned hold blocks preparation.
        for kind in *b"0IF" {
            let marker = [
                key(kind),
                ns.as_bytes().to_vec(),
                b"admission-test".to_vec(),
            ]
            .concat();
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&marker, b"test hold");
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog
                    .prepare_node_mutation_task(&serial, route, 100, 1, op, 1 << 20)
                    .await,
                Err(CatalogError::Incomplete)
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert!(
                tr.get(&hold_key(ns, pool.id), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            tr.clear(&marker);
            tr.commit().await.unwrap();
        }
        let file_preflight = catalog.db.create_trx().unwrap();
        require_pool_writable_at(&file_preflight, ns, pool.id)
            .await
            .unwrap();
        let prepared = catalog
            .prepare_node_mutation_task(&serial, route, 100, 1, op, 1 << 20)
            .await
            .unwrap();
        assert_eq!(prepared.state, NodeMutationTaskState::Prepared);
        assert_eq!(prepared.prepared_owner_epoch, Some(100));
        assert!(prepared.prepared_at_millis.unwrap() >= prepared.created_at_millis);
        let canonical = catalog
            .capture_prepared_node_inputs(&serial, route, 100, 1, op, 1 << 20)
            .await
            .unwrap();
        assert_eq!(canonical.ancestors.len(), 1);
        assert_eq!(canonical.ancestors[0].path, "/");
        assert_eq!(canonical.baseline.parent.id, canonical.ancestors[0].id);
        assert!(canonical.baseline.current.is_none());
        assert_eq!(canonical.request, requests[0]);
        let cancelled_queued = catalog
            .cancel_node_mutation_task(&serial, route, 100, 1, requests[3].context().operation)
            .await
            .unwrap();
        assert_eq!(cancelled_queued.prepared_owner_epoch, None);
        file_preflight.set(b"node-admission-conflict-marker", b"not committed");
        assert_eq!(file_preflight.commit().await.unwrap_err().code(), 1020);
        assert!(
            requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );
        assert_eq!(
            catalog
                .prepare_node_mutation_task(&serial, route, 100, 1, op, 1 << 20)
                .await
                .unwrap(),
            prepared
        );
        assert_eq!(
            catalog
                .queue_node_mutation_request(route, 100, &requests[0], 1 << 20)
                .await
                .unwrap(),
            prepared
        );
        assert!(matches!(
            catalog
                .prepare_node_mutation_task(&serial, route, 100, 1, other, 1 << 20)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        assert!(matches!(
            catalog
                .check_active_pool_gate(&serial, route, 100, pool.id)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        catalog
            .check_active_pool_gate(&serial, route, 100, other_pool.id)
            .await
            .unwrap();
        let third_prepared = catalog
            .prepare_node_mutation_task(&serial, route, 100, 1, third, 1 << 20)
            .await
            .unwrap();
        assert_eq!(third_prepared.state, NodeMutationTaskState::Prepared);
        catalog
            .cancel_node_mutation_task(&serial, route, 100, 1, third)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .check_active_pool_gate(&serial, route, 100, pool.id)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        // Reads retain the committed namespace, with no synthetic new inode.
        assert!(matches!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    &pool_catalog_path(pool.id, b"/").unwrap(),
                    route
                )
                .await
                .unwrap(),
            Some(ResolvedPath::Directory { .. })
        ));
        assert!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    &pool_catalog_path(pool.id, b"/first").unwrap(),
                    route
                )
                .await
                .unwrap()
                .is_none()
        );
        let intake=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"ADMIT01L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        catalog
            .admit_management(&serial, route, 100, &intake)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .start_native_media(&serial, route, 100, 1, intake.operation())
                .await,
            Err(CatalogError::Incomplete)
        ));
        let deletion=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool-delete","id":pool.id.to_string(),"revision":1}})).unwrap();
        catalog
            .admit_management(&serial, route, 100, &deletion)
            .await
            .unwrap();
        assert_ne!(
            catalog
                .apply_management(&serial, route, 100, 1, deletion.operation())
                .await
                .unwrap()["status"],
            "succeeded"
        );
        assert!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .is_some()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 2,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .pending_node_mutation_tasks(&serial, route, 100, None, 100)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert!(matches!(
            catalog
                .prepare_node_mutation_task(&serial, route, 100, 1, op, 1 << 20)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        assert!(matches!(
            catalog
                .cancel_node_mutation_task(&serial, route, 100, 1, op)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        assert_eq!(
            reopened
                .prepare_node_mutation_task(&serial, route, 101, 1, op, 1 << 20)
                .await
                .unwrap(),
            prepared
        );
        assert!(
            reopened
                .pending_node_mutation_tasks(&serial, route, 101, None, 100)
                .await
                .unwrap()
                .items
                .contains(&prepared)
        );
        let (header, body) = reopened
            .node_mutation_task_request(&serial, route, 101, 1, op, 1 << 20)
            .await
            .unwrap();
        assert_eq!(header, prepared);
        assert_eq!(body, requests[0]);
        // Missing gate, queue or global index never gets repaired or cancelled.
        for broken in [
            hold_key(ns, pool.id),
            identity_key(ns, &serial, b'q', 1, op),
            global_pending_key(ns, &serial, 1, op),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            let saved = tr.get(&broken, false).await.unwrap().unwrap().to_vec();
            tr.clear(&broken);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog.node_mutation_task(&serial, route, 1, op).await,
                Err(CatalogError::Corrupt(_))
            ));
            assert!(matches!(
                catalog
                    .cancel_node_mutation_task(&serial, route, 101, 1, op)
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            let tr = catalog.db.create_trx().unwrap();
            assert!(tr.get(&broken, false).await.unwrap().is_none());
            tr.set(&broken, &saved);
            tr.commit().await.unwrap();
        }
        let held = hold_key(ns, pool.id);
        let tr = catalog.db.create_trx().unwrap();
        let saved = tr.get(&held, false).await.unwrap().unwrap().to_vec();
        tr.set(&held, b"bad");
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .check_active_pool_gate(&serial, route, 101, pool.id)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&held, false).await.unwrap().unwrap().as_ref(),
            b"bad"
        );
        tr.set(&held, &saved);
        tr.commit().await.unwrap();
        let (cancelled, successor) = tokio::join!(
            catalog.cancel_node_mutation_task(&serial, route, 101, 1, op),
            catalog.prepare_node_mutation_task(&serial, route, 101, 1, other, 1 << 20)
        );
        let cancelled = cancelled.unwrap();
        assert_eq!(cancelled.state, NodeMutationTaskState::Cancelled);
        assert_eq!(cancelled.prepared_owner_epoch, Some(100));
        assert_eq!(cancelled.completed_owner_epoch, Some(101));
        match successor {
            Ok(task) => assert_eq!(task.state, NodeMutationTaskState::Prepared),
            Err(CatalogError::PoolBusy) => {}
            other => panic!("{other:?}"),
        }
        catalog
            .prepare_node_mutation_task(&serial, route, 101, 1, other, 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .cancel_node_mutation_task(&serial, route, 101, 1, op)
                .await
                .unwrap(),
            cancelled
        );
        assert!(matches!(
            catalog
                .check_active_pool_gate(&serial, route, 101, pool.id)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        catalog
            .cancel_node_mutation_task(&serial, route, 101, 1, other)
            .await
            .unwrap();
        assert!(
            !requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );
        // Original cancelled identity does not reacquire a hold.
        assert_eq!(
            catalog
                .prepare_node_mutation_task(&serial, route, 101, 1, op, 1 << 20)
                .await
                .unwrap(),
            cancelled
        );
        catalog
            .check_active_pool_gate(&serial, route, 101, pool.id)
            .await
            .unwrap();
        for request in &requests {
            assert!(
                catalog
                    .db
                    .create_trx()
                    .unwrap()
                    .get(&node_key(ns, request.context().node), false)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[cfg(test)]
#[path = "node_tasks/rename_tests.rs"]
mod rename_tests;
