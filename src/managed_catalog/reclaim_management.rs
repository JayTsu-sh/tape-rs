//! Bind the asynchronous management receipt and source reservation in one FDB
//! transaction. The reclaim's original claim is acquired once, never bridged
//! by releasing/reacquiring the physical library's exclusion slot.
use super::*;
fn journal(ns: Uuid, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut key = domain(ns, b'j');
    key.extend_from_slice(&epoch.to_be_bytes());
    key.extend_from_slice(operation.as_bytes());
    key
}

fn dependency_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = b'D';
    key
}

/// A queued semantic rejection is terminal only when this same read version
/// proves there was no parent, result or acquired execution claim to recover.
async fn require_unstarted_at(tr: &Transaction, request: &ManagementRequest) -> Result<()> {
    let key = journal(request.namespace(), request.epoch(), request.operation());
    if tr.get(&key, false).await?.is_some() {
        return Err(CatalogError::Corrupt(
            "排队回收已存在原父任务，不能按未执行失败",
        ));
    }
    let mut result = key;
    result[domain(request.namespace(), b'j').len() - 1] = b'f';
    if tr.get(&result, false).await?.is_some() {
        return Err(CatalogError::Corrupt("排队回收已存在原完成结果"));
    }
    let serial = request.value()["library"]
        .as_str()
        .expect("validated library");
    management::exclusion::require_available_at(tr, request.namespace(), serial).await?;
    let pool = ReclaimSpec::parse(&request.value()["change"])?.pool();
    require_pool_write_at(tr, request.namespace(), pool).await?;
    layout::require_idle_at(tr, request.namespace(), serial).await
}
pub(super) fn clear_dependency_at(tr: &Transaction, reservation: &ReclaimReservation) {
    tr.clear(&dependency_key(reservation));
}
fn dependency(raw: &[u8], parent: &Value) -> Result<Value> {
    let value = decode(raw)?;
    if value["version"] != 1
        || value["parent"] != parent["fingerprint"]
        || !value.as_object().is_some_and(|m| m.len() == 5)
        || !value["owner"]
            .as_u64()
            .is_some_and(|v| v > 0 && Some(v) <= parent["owner_epoch"].as_u64())
        || !value["observed_at_millis"].as_u64().is_some_and(|v| v > 0)
        || !value["reason"]
            .as_str()
            .is_some_and(|v| !v.is_empty() && v.len() <= 2000)
    {
        return Err(CatalogError::Corrupt("reclaim 等待依赖检查点无效"));
    }
    Ok(value)
}

pub(in super::super) async fn cancel_reserved_management_at(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    task: &Value,
) -> Result<Value> {
    let request = ManagementRequest::parse(task["request"].clone())?;
    if task["status"] != "running"
        || task["owner_epoch"] != owner
        || request.value()["change"]["kind"] != "media-reclaim"
    {
        return Err(CatalogError::OwnerFenced);
    }
    let raw = tr
        .get(
            &journal(request.namespace(), request.epoch(), request.operation()),
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("运行中回收缺少原父检查点"))?;
    let parent = decode(&raw)?;
    let reservation = ReclaimReservation {
        intent: parent["intent"].clone(),
        route,
    };
    if reservation.intent["library"] != serial
        || reservation.intent["namespace"] != request.value()["namespace"]
        || reservation.intent["epoch"] != request.epoch()
        || reservation.intent["operation"] != request.operation().to_string()
        || reservation.intent["change"] != request.value()["change"]
    {
        return Err(CatalogError::Corrupt("回收取消改变原父任务身份"));
    }
    let parent = record_at(tr, &reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if parent["phase"] != "reserved" || parent["children"] != 0 {
        return Err(CatalogError::Invalid(
            "回收已开始迁移；保留原任务和迁移证明，不能按未开写取消",
        ));
    }
    if automatic_media::pending_at(tr, request.namespace(), serial)
        .await?
        .is_some()
        || tr
            .get(
                &native_device::library_gate(request.namespace(), serial),
                false,
            )
            .await?
            .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    source_state_at(tr, &reservation).await?;
    mechanics::require_parent_idle_at(tr, &reservation).await?;
    layout::at(tr, &reservation).await?;
    source::require_metadata_idle_at(tr, &reservation).await?;
    walk::clear_reserved_at(tr, &reservation).await?;
    let claim = management::exclusion::LibraryTaskClaim::for_reclaim(&reservation.token())?;
    claim.require_owned(tr).await?;
    let mut task = task.clone();
    task["status"] = json!("cancelled");
    task["result"] = json!({"cancelled":true,"phase":"before_migration","source_unchanged":true});
    task["completed_at_millis"] =
        json!(management::now().max(task["created_at_millis"].as_u64().expect("validated time")));
    management::write_task(tr, &request, &task).await?;
    management::clear_queued(tr, &request);
    source::clear_reserved_at(tr, &reservation);
    mechanics::clear_finished_at(tr, &reservation);
    layout::clear_at(tr, &reservation);
    clear_dependency_at(tr, &reservation);
    tr.clear(&gate_key(request.namespace(), reservation.spec().pool()));
    tr.clear(&reservation.journal_key());
    claim.release(tr);
    Ok(task)
}
pub(in super::super) async fn management_progress_at(
    tr: &Transaction,
    value: &Value,
) -> Result<Option<Value>> {
    let request = ManagementRequest::parse(value["request"].clone())?;
    let Some(raw) = tr
        .get(
            &journal(request.namespace(), request.epoch(), request.operation()),
            false,
        )
        .await?
    else {
        if value["status"] == "running" {
            return Err(CatalogError::Corrupt("运行中 reclaim 缺少原父检查点"));
        }
        return Ok(None);
    };
    let parent = decode(&raw)?;
    let intent = &parent["intent"];
    if intent["library"] != request.value()["library"]
        || intent["namespace"] != request.value()["namespace"]
        || intent["epoch"] != request.epoch()
        || intent["operation"] != request.operation().to_string()
        || intent["change"] != request.value()["change"]
        || parent["fingerprint"] != format!("{:x}", Sha256::digest(intent.to_string().as_bytes()))
    {
        return Err(CatalogError::Corrupt("reclaim 管理进度与原请求不同"));
    }
    let mechanical = if value["status"] == "running" {
        mechanics::management_progress_at(
            tr,
            request.namespace(),
            request.value()["library"]
                .as_str()
                .expect("validated library"),
            &parent,
        )
        .await?
    } else {
        Value::Null
    };
    let mut key = journal(request.namespace(), request.epoch(), request.operation());
    key[domain(request.namespace(), b'j').len() - 1] = b'D';
    let dependency = tr
        .get(&key, false)
        .await?
        .map(|raw| dependency(&raw, &parent))
        .transpose()?;
    Ok(Some(
        json!({"phase":parent["phase"],"children":parent["children"],"owner_epoch":parent["owner_epoch"],"mechanical":mechanical,"dependency":dependency}),
    ))
}
impl ManagedCatalog {
    /// A dependency wait retains all original claims and remains nonterminal.
    /// Failure to persist this diagnostic never permits more device commands.
    pub async fn set_reclaim_dependency(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        reason: Option<&str>,
    ) -> Result<()> {
        if reason.is_some_and(|v| v.is_empty() || v.len() > 2000) {
            return Err(CatalogError::Invalid("reclaim 等待原因长度无效"));
        }
        self.db.run(|tr, _| Box::pin(async move { requests::retry_result(async {
            let parent = record_at(&tr, reservation, owner).await?;
            if parent["owner_epoch"] != owner { return Err(CatalogError::OwnerFenced); }
            let key = dependency_key(reservation);
            let old = tr.get(&key, false).await?.map(|raw| dependency(&raw, &parent)).transpose()?;
            if let Some(reason) = reason {
                if old.as_ref().is_some_and(|v| v["owner"] == owner && v["reason"] == reason) { return Ok(()); }
                let value = json!({"version":1,"parent":parent["fingerprint"],"owner":owner,"reason":reason,"observed_at_millis":management::now()});
                tr.set(&key, value.to_string().as_bytes());
            } else { tr.clear(&key); }
            Ok(())
        }.await) })).await?
    }

    /// Start or adopt the same original task. A queued request never holds a
    /// physical-library claim; its exact parent acquires it atomically here.
    pub async fn start_reclaim_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<ReclaimReservation>> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            routing::require_runtime_active_owner(&tr, serial, route, owner)
                                .await?;
                            let (request, mut task) = management::task_for_identity(
                                &tr,
                                serial,
                                route.route.namespace,
                                epoch,
                                operation,
                            )
                            .await?;
                            if request.value()["change"]["kind"] != "media-reclaim" {
                                return Err(CatalogError::Invalid("不是原生 reclaim 管理任务"));
                            }
                            if !matches!(task["status"].as_str(), Some("queued" | "running")) {
                                return Ok(None);
                            }
                            if task["status"] == "running" {
                                if task["owner_epoch"].as_u64().is_none_or(|old| old > owner) {
                                    return Err(CatalogError::OwnerFenced);
                                }
                                if tr
                                    .get(&journal(request.namespace(), epoch, operation), false)
                                    .await?
                                    .is_none()
                                {
                                    return Err(CatalogError::Corrupt(
                                        "原运行中 reclaim 父检查点已丢失",
                                    ));
                                }
                            }
                            let spec = ReclaimSpec::parse(&request.value()["change"])?;
                            let reservation = match reserve_at(&tr, serial, route, owner, epoch, operation, &spec).await {
                                Ok(parent) => parent,
                                Err(error) if task["status"] == "queued"
                                    && matches!(error.domain(), CatalogError::Invalid(_) | CatalogError::MediaUnavailable) => {
                                    require_unstarted_at(&tr, &request).await?;
                                    task["status"] = json!("failed");
                                    task["owner_epoch"] = json!(owner);
                                    task["result"] = json!({"error":error.to_string(),"phase":"before_reservation","source_unchanged":true});
                                    task["completed_at_millis"] = json!(management::now().max(task["created_at_millis"].as_u64().expect("validated time")));
                                    management::write_task(&tr, &request, &task).await?;
                                    management::clear_queued(&tr, &request);
                                    return Ok(None);
                                }
                                Err(error) => return Err(error),
                            };
                            task["status"] = json!("running");
                            task["owner_epoch"] = json!(owner);
                            management::write_task(&tr, &request, &task).await?;
                            Ok(Some(reservation))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}
