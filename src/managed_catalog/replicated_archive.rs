//! Parent publication and per-member upload identities share the same FDB.
use super::*;
use crate::replicated_pool::ReplicatedArchive;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::managed_catalog) struct CommittedMember {
    pub pool_id: Uuid,
    pub file_id: Uuid,
    pub copy_id: Uuid,
    pub volume: Uuid,
    pub generation: u64,
    pub scan: Uuid,
    pub file_uid: u64,
    pub barcode: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Published {
    pub operation: Uuid,
    pub version: u64,
    pub size: u64,
    pub sha256: [u8; 32],
    pub members: Vec<CommittedMember>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Child {
    pool: Uuid,
    request: Option<ActiveFileRequest>,
    permit: Option<u16>,
    commit: Option<CommittedMember>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Waiting,
    Cancelling,
    Cancelled,
    Committed,
}

#[derive(Clone, Deserialize, Serialize)]
struct Job {
    input: ReplicatedArchive,
    version: u64,
    state: State,
    children: Vec<Child>,
}

#[derive(Clone)]
pub(crate) struct ReplicaUpload {
    pub serial: String,
    pub spec: ActiveUploadSpec,
    pub request: Option<ActiveFileRequest>,
}

fn job_key(parent: Uuid, operation: Uuid) -> Vec<u8> {
    [task_key(parent, b'w').as_slice(), operation.as_bytes()].concat()
}

fn pending_key(parent: Uuid, path: &str) -> Vec<u8> {
    [task_key(parent, b'p').as_slice(), path.as_bytes()].concat()
}

fn child_key(ns: Uuid, operation: Uuid) -> Vec<u8> {
    [task_key(ns, b'u').as_slice(), operation.as_bytes()].concat()
}

fn child_operation(parent: Uuid, operation: Uuid, pool: Uuid) -> Uuid {
    let digest = Sha256::new()
        .chain_update(b"tape-rs/replica-upload/1")
        .chain_update(parent.as_bytes())
        .chain_update(operation.as_bytes())
        .chain_update(pool.as_bytes())
        .finalize();
    Uuid::from_bytes(digest[..16].try_into().expect("sha256"))
}

pub(super) fn member_path(parent: Uuid, operation: Uuid) -> String {
    format!("/.tapers-replica-{parent}-{operation}")
}

fn spec(parent: Uuid, job: &Job, pool: Uuid) -> ActiveUploadSpec {
    ActiveUploadSpec {
        pool_id: pool,
        path: member_path(parent, job.input.operation).into_bytes(),
        operation_id: child_operation(parent, job.input.operation, pool),
        expected_version: None,
        size: job.input.size,
        digest: ContentDigest::Sha256(job.input.sha256),
    }
}

fn same_input(wanted: &ActiveUploadSpec, actual: &ActiveUploadSpec) -> bool {
    wanted.pool_id == actual.pool_id
        && wanted.operation_id == actual.operation_id
        && wanted.path == actual.path
        && wanted.size == actual.size
        && wanted.digest == actual.digest
        && wanted.expected_version == actual.expected_version
}

async fn job_at(tr: &Transaction, parent: Uuid, operation: Uuid) -> Result<Option<Job>> {
    let Some(raw) = tr.get(&job_key(parent, operation), false).await? else {
        return Ok(None);
    };
    let job: Job = decode(&raw)?;
    if job.input.operation != operation || job.version != job.input.previous.unwrap_or(0) + 1 {
        return Err(CatalogError::Corrupt("多副本归档记录身份无效"));
    }
    Ok(Some(job))
}

async fn lookup_child(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    operation: Uuid,
) -> Result<Option<(Uuid, Job)>> {
    let Some(parent) = parent_at(tr, ns, pool).await? else {
        return Ok(None);
    };
    let raw = tr
        .get(&child_key(ns, operation), false)
        .await?
        .ok_or(CatalogError::Invalid("多副本成员写入须使用原归档任务"))?;
    let parent_operation = uuid_from(&raw)?;
    let job = job_at(tr, parent, parent_operation)
        .await?
        .ok_or(CatalogError::Corrupt("底层池写入缺少原归档任务"))?;
    if !job.children.iter().any(|child| child.pool == pool)
        || child_operation(parent, parent_operation, pool) != operation
    {
        return Err(CatalogError::IdempotencyMismatch);
    }
    Ok(Some((parent, job)))
}

pub(in crate::managed_catalog) async fn authorize_spec(
    tr: &Transaction,
    ns: Uuid,
    actual: &ActiveUploadSpec,
) -> Result<bool> {
    let Some((parent, job)) = lookup_child(tr, ns, actual.pool_id, actual.operation_id).await?
    else {
        return Ok(false);
    };
    if job.state != State::Waiting || !same_input(&spec(parent, &job, actual.pool_id), actual) {
        return Err(CatalogError::Invalid("多副本归档不能准备新副本"));
    }
    Ok(true)
}

pub(in crate::managed_catalog) async fn authorize_child_epoch(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<bool> {
    let Some((_, job)) = lookup_child(tr, ns, pool, operation).await? else {
        return Ok(false);
    };
    let request = job
        .children
        .iter()
        .find(|child| child.pool == pool)
        .and_then(|child| child.request.as_ref())
        .filter(|request| request.request_epoch == epoch)
        .ok_or(CatalogError::IdempotencyMismatch)?;
    match job.state {
        State::Waiting => Ok(true),
        State::Cancelling
            if reservation::active_file_attempt_at(tr, request)
                .await?
                .is_some() =>
        {
            Ok(true)
        }
        _ => Err(CatalogError::Invalid("原归档任务已结束或取消")),
    }
}

pub(in crate::managed_catalog) async fn authorize_request(
    tr: &Transaction,
    request: &ActiveFileRequest,
) -> Result<()> {
    let Some(pool) = request.pool_id else {
        return Ok(());
    };
    let Some((_, job)) = lookup_child(tr, request.namespace, pool, request.operation_id).await?
    else {
        return Ok(());
    };
    if !job
        .children
        .iter()
        .any(|child| child.request.as_ref() == Some(request))
    {
        return Err(CatalogError::IdempotencyMismatch);
    }
    if job.state == State::Waiting
        || job.state == State::Cancelling
            && reservation::active_file_attempt_at(tr, request)
                .await?
                .is_some()
    {
        Ok(())
    } else {
        Err(CatalogError::Invalid("原归档任务已结束或取消"))
    }
}

fn history_pin(request: &ActiveFileRequest) -> Vec<u8> {
    let mut key = requests::epoch_prefix(b'F', request.namespace, request.request_epoch);
    key.extend_from_slice(request.operation_id.as_bytes());
    key
}

/// Move the already charged quota to the ordinary reservation transaction.
pub(in crate::managed_catalog) async fn take_permit(
    tr: &Transaction,
    request: &ActiveFileRequest,
) -> Result<Option<u16>> {
    let Some(pool) = request.pool_id else {
        return Ok(None);
    };
    let Some((parent, mut job)) =
        lookup_child(tr, request.namespace, pool, request.operation_id).await?
    else {
        return Ok(None);
    };
    if job.state != State::Waiting {
        return Err(CatalogError::Invalid("原归档任务禁止发起新写入"));
    }
    let child = job
        .children
        .iter_mut()
        .find(|child| child.request.as_ref() == Some(request))
        .ok_or(CatalogError::IdempotencyMismatch)?;
    let permit = child
        .permit
        .take()
        .ok_or(CatalogError::Invalid("原成员上传许可已结清"))?;
    if tr.get(&history_pin(request), false).await?.as_deref() != Some(&[][..]) {
        return Err(CatalogError::Corrupt("原成员上传历史引用缺失"));
    }
    tr.set(&job_key(parent, job.input.operation), &encode(&job)?);
    Ok(Some(permit))
}

impl ManagedCatalog {
    pub(crate) async fn admit_replica_archive(
        &self,
        parent: Uuid,
        input: &ReplicatedArchive,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Value> {
        pool_catalog_path(parent, input.path.as_bytes())?;
        if input.operation.is_nil()
            || input.path == "/"
            || input.previous == Some(0)
            || input.previous == Some(u64::MAX)
            || !crate::ltfs::xml_text_valid(&input.path)
        {
            return Err(CatalogError::Invalid("多副本归档 UUID、路径或版本无效"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let creation = require_published(&tr, parent, routes).await?;
                        if let Some(old) = job_at(&tr, parent, input.operation).await? {
                            return if old.input == *input {
                                Ok(view(parent, &creation, &old))
                            } else {
                                Err(CatalogError::IdempotencyMismatch)
                            };
                        }
                        files::require_write_path(&tr, parent, &input.path, input.previous).await?;
                        let claim = pending_key(parent, &input.path);
                        if tr.get(&claim, false).await?.is_some() {
                            return Err(CatalogError::PoolBusy);
                        }
                        let pending =
                            read_page(&tr, &task_key(parent, b'p'), None, 32, false).await?;
                        if pending.items.len() >= 32 {
                            return Err(CatalogError::PoolBusy);
                        }
                        let mut children = Vec::with_capacity(creation.input.members.len());
                        for member in &creation.input.members {
                            require_member(&tr, parent, member).await?;
                            let key = child_key(
                                member.namespace,
                                child_operation(parent, input.operation, member.pool_id),
                            );
                            if tr.get(&key, false).await?.is_some() {
                                return Err(CatalogError::IdempotencyMismatch);
                            }
                            tr.set(&key, input.operation.as_bytes());
                            children.push(Child {
                                pool: member.pool_id,
                                request: None,
                                permit: None,
                                commit: None,
                            });
                        }
                        let job = Job {
                            input: input.clone(),
                            version: input.previous.unwrap_or(0) + 1,
                            state: State::Waiting,
                            children,
                        };
                        tr.set(&claim, input.operation.as_bytes());
                        tr.set(&job_key(parent, input.operation), &encode(&job)?);
                        Ok(view(parent, &creation, &job))
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    pub(crate) async fn replica_upload(
        &self,
        parent: Uuid,
        operation: Uuid,
        pool: Uuid,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<ReplicaUpload>> {
        let tr = self.db.create_trx()?;
        let creation = require_published(&tr, parent, routes).await?;
        let job = job_at(&tr, parent, operation)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let member = member_in(&creation, pool)?;
        let child = job
            .children
            .iter()
            .find(|child| child.pool == pool)
            .ok_or(CatalogError::Incomplete)?;
        if child.commit.is_some() || job.state != State::Waiting {
            return Ok(None);
        }
        Ok(Some(ReplicaUpload {
            serial: member.library_serial.clone(),
            spec: spec(parent, &job, pool),
            request: child.request.clone(),
        }))
    }

    /// A prepare result is immutable before any upload can reach the drive.
    pub(crate) async fn pin_replica_upload(
        &self,
        parent: Uuid,
        operation: Uuid,
        request: &ActiveFileRequest,
    ) -> Result<ActiveFileRequest> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        request.validate()?;
                        let pool = request.pool_id.ok_or(CatalogError::Invalid("缺少底层池"))?;
                        let (owner, mut job) =
                            lookup_child(&tr, request.namespace, pool, request.operation_id)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                        if owner != parent
                            || job.input.operation != operation
                            || job.state != State::Waiting
                        {
                            return Err(CatalogError::IdempotencyMismatch);
                        }
                        let wanted = spec(parent, &job, pool);
                        if request.ltfs_path()?.as_bytes() != wanted.path
                            || request.expected_size != wanted.size
                            || request.expected_digest != wanted.digest
                            || request.expected_version.is_some()
                            || request.barcode.is_some()
                        {
                            return Err(CatalogError::IdempotencyMismatch);
                        }
                        let child = job
                            .children
                            .iter_mut()
                            .find(|child| child.pool == pool)
                            .expect("validated");
                        if let Some(original) = &child.request {
                            return Ok(original.clone());
                        }
                        let window = requests::window(&tr, request.namespace).await?;
                        if request.request_epoch != window.accepting {
                            return Err(CatalogError::Incomplete);
                        }
                        let pin = history_pin(request);
                        if tr.get(&pin, false).await?.is_some() {
                            return Err(CatalogError::IdempotencyMismatch);
                        }
                        let permit = maintenance::reserve_identity(
                            &tr,
                            request.namespace,
                            request.request_epoch,
                            request.operation_id,
                        )
                        .await?
                        .ok_or(CatalogError::PoolBusy)?;
                        child.request = Some(request.clone());
                        child.permit = Some(permit);
                        tr.set(&pin, &[]);
                        tr.set(&job_key(parent, operation), &encode(&job)?);
                        Ok(request.clone())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    pub(crate) async fn replica_archive_status(
        &self,
        parent: Uuid,
        operation: Uuid,
        cancel: bool,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<Value>> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let creation = require_published(&tr, parent, routes).await?;
                        let Some(mut job) = job_at(&tr, parent, operation).await? else {
                            return Ok(None);
                        };
                        if matches!(job.state, State::Cancelled | State::Committed) {
                            return Ok(Some(view(parent, &creation, &job)));
                        }
                        let mut progressed = cancel && job.state == State::Waiting;
                        if progressed {
                            job.state = State::Cancelling;
                        }
                        let mut active = false;
                        for child in &mut job.children {
                            let Some(request) = &child.request else {
                                continue;
                            };
                            if child.commit.is_some() {
                                continue;
                            }
                            if let Some((receipt, barcode)) =
                                upload_receipts::replica_commit_at(&tr, request).await?
                            {
                                child.commit = Some(CommittedMember {
                                    pool_id: child.pool,
                                    file_id: receipt.file_id,
                                    copy_id: receipt.copy_id,
                                    volume: receipt.volume_id,
                                    generation: receipt.volume_generation,
                                    scan: receipt.volume_scan_id,
                                    file_uid: receipt.ltfs_file_uid,
                                    barcode,
                                });
                                if child.permit.is_some() {
                                    return Err(CatalogError::Corrupt(
                                        "已提交成员仍占用未使用许可",
                                    ));
                                }
                                tr.clear(&history_pin(request));
                                progressed = true;
                            } else if reservation::active_file_attempt_at(&tr, request)
                                .await?
                                .is_some()
                            {
                                active = true;
                            }
                        }
                        let claim = pending_key(parent, &job.input.path);
                        if tr.get(&claim, false).await?.as_deref() != Some(operation.as_bytes()) {
                            return Err(CatalogError::Corrupt("原归档路径占用缺失"));
                        }
                        if job.state == State::Cancelling && !active {
                            for child in &mut job.children {
                                if let Some(request) = &child.request {
                                    if let Some(permit) = child.permit.take() {
                                        maintenance::release(&tr, request.namespace, permit)
                                            .await?
                                            .apply(&tr);
                                    }
                                    tr.clear(&history_pin(request));
                                }
                            }
                            job.state = State::Cancelled;
                            tr.clear(&claim);
                            progressed = true;
                        } else if job.state == State::Waiting
                            && job.children.iter().all(|child| child.commit.is_some())
                        {
                            files::publish_archive(
                                &tr,
                                parent,
                                &job.input,
                                Published {
                                    operation,
                                    version: job.version,
                                    size: job.input.size,
                                    sha256: job.input.sha256,
                                    members: job
                                        .children
                                        .iter()
                                        .filter_map(|child| child.commit.clone())
                                        .collect(),
                                },
                            )
                            .await?;
                            job.state = State::Committed;
                            tr.clear(&claim);
                            progressed = true;
                        }
                        if progressed {
                            tr.set(&job_key(parent, operation), &encode(&job)?);
                        }
                        let mut view = view(parent, &creation, &job);
                        if active {
                            view["active_attempt"] = json!(true);
                        }
                        Ok(Some(view))
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}

pub(super) async fn require_published(
    tr: &Transaction,
    id: Uuid,
    routes: &BTreeMap<String, ActiveLibraryRoute>,
) -> Result<Creation> {
    let pool = read_at(tr, id)
        .await?
        .ok_or(CatalogError::Invalid("多副本池或成员不存在"))?;
    require_configured(&pool.input, routes)?;
    if !pool.published {
        return Err(CatalogError::PoolImporting);
    }
    Ok(pool)
}

pub(super) fn member_in(creation: &Creation, pool: Uuid) -> Result<&ReplicatedPoolMember> {
    creation
        .input
        .members
        .iter()
        .find(|member| member.pool_id == pool)
        .ok_or(CatalogError::Invalid("多副本池或成员不存在"))
}

fn view(parent: Uuid, creation: &Creation, job: &Job) -> Value {
    let members = creation.input.members.iter().zip(&job.children).map(|(member, child)| json!({
        "pool_id": child.pool, "library_serial": member.library_serial, "namespace": member.namespace,
        "prepared": child.request.is_some(), "commit": child.commit,
    })).collect::<Vec<_>>();
    json!({"id":parent,"operation":job.input.operation,"status":job.state,"path":job.input.path,
        "file_version":job.version,"size":job.input.size,"sha256":files::hex(&job.input.sha256),
        "members":members})
}

pub(super) async fn recovery_version(
    tr: &Transaction,
    parent: Uuid,
    operation: Uuid,
) -> Result<Option<Published>> {
    let Some(job) = job_at(tr, parent, operation).await? else {
        return Ok(None);
    };
    if !matches!(job.state, State::Waiting | State::Committed) {
        return Ok(None);
    }
    Ok(Some(Published {
        operation,
        version: job.version,
        size: job.input.size,
        sha256: job.input.sha256,
        members: job
            .children
            .into_iter()
            .filter_map(|child| child.commit)
            .collect(),
    }))
}
