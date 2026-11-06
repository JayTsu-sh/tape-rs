//! Assignment owns the same mechanical exclusion as moves; formatting is never replayed here.
use super::*;
#[path = "management_assign_plan.rs"]
mod plan;
pub use plan::{AssignExecution, AssignPhase, AssignPlan};

// Global exclusion precedes formatting on any library. Successful claims are
// retained: retiring placement does not make a historical UUID reusable.
fn volume_claim_key(request: &ManagementRequest) -> Vec<u8> {
    let volume = Uuid::parse_str(
        request.value()["change"]["volume"]
            .as_str()
            .expect("volume"),
    )
    .expect("validated volume");
    let mut out = key(b'5');
    out.extend_from_slice(volume.as_bytes());
    out
}
fn volume_claim(request: &ManagementRequest) -> Vec<u8> {
    encode(
        &json!({"namespace":request.namespace().to_string(),"library":request.value()["library"],"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()}),
    )
}

pub(super) fn validate_execution(task: &Value, request: &ManagementRequest) -> Result<()> {
    if request.value()["change"]["kind"] != "media-assign" {
        return Ok(());
    }
    if matches!(task["status"].as_str(), Some("queued" | "cancelled")) {
        return if task["execution"].is_null() {
            Ok(())
        } else {
            Err(CatalogError::Corrupt("未开始分配任务包含执行计划"))
        };
    }
    if task["status"] == "failed" && task["execution"].is_null() {
        return Ok(());
    }
    let execution = AssignExecution::parse(task["execution"].clone(), request)?;
    if (task["status"] == "succeeded") != (execution.phase == AssignPhase::Confirmed)
        || (task["status"] == "failed" && execution.phase != AssignPhase::Inspecting)
    {
        return Err(CatalogError::Corrupt("分配任务状态与执行阶段不符"));
    }
    Ok(())
}
fn require_request(
    serial: &str,
    route: ActiveLibraryRoute,
    request: &ManagementRequest,
) -> Result<()> {
    if request.value()["library"] != serial
        || request.namespace() != route.route.namespace
        || request.value()["change"]["kind"] != "media-assign"
    {
        return Err(CatalogError::Invalid("分配请求与带库、命名空间或类型不符"));
    }
    Ok(())
}
async fn original(tr: &Transaction, serial: &str, request: &ManagementRequest) -> Result<Value> {
    let task = read_task(
        tr,
        request.namespace(),
        serial,
        request.epoch(),
        request.operation(),
    )
    .await?
    .ok_or(CatalogError::Corrupt("分配原任务缺失"))?;
    if task["fingerprint"] != request.fingerprint() {
        return Err(CatalogError::Corrupt("分配原请求指纹不符"));
    }
    Ok(task)
}
async fn eligible(
    tr: &Transaction,
    serial: &str,
    request: &ManagementRequest,
    plan: &AssignPlan,
) -> Result<()> {
    let change = &request.value()["change"];
    let pool_id = Uuid::parse_str(change["pool"].as_str().expect("pool")).expect("validated pool");
    let raw = tr
        .get(&pool_key(request.namespace(), pool_id), false)
        .await?
        .ok_or(CatalogError::Invalid("原分配池不存在"))?;
    let pool = decode_pool(pool_id, &raw)?;
    require_pool_library(tr, request.namespace(), pool_id, serial).await?;
    active_pools::require_indexes(tr, request.namespace(), &pool).await?;
    if pool.revision != plan.pool_revision {
        return Err(CatalogError::Invalid("原池策略修订号已改变"));
    }
    if pool.policy.media_restriction != "none"
        && !plan.barcode.ends_with(&pool.policy.media_restriction)
    {
        return Err(CatalogError::Invalid("条码与原池介质类型限制不符"));
    }
    if ManagedCatalog::verified_assignment(tr, serial, &plan.barcode)
        .await?
        .is_some()
    {
        return Err(CatalogError::Invalid("分配仅支持未分配磁带，请核对原归属"));
    }
    let volume =
        Uuid::parse_str(change["volume"].as_str().expect("volume")).expect("validated volume");
    if tr
        .get(&volume_claim_key(request), false)
        .await?
        .is_some_and(|raw| raw.as_ref() != volume_claim(request))
    {
        return Err(CatalogError::Invalid(
            "原计划的新卷UUID已由其他分配请求保留",
        ));
    }
    let mut scans = key(b'G');
    scans.extend_from_slice(volume.as_bytes());
    if !read_page(tr, &scans, None, 1, false)
        .await?
        .items
        .is_empty()
    {
        return Err(CatalogError::Invalid(
            "新卷UUID包含历史扫描，不能复用格式化身份",
        ));
    }
    for key in [
        placement_key(volume),
        ready_volume_key(volume),
        reconcile_volume_key(volume),
    ] {
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::Invalid("原计划的新卷UUID已被使用"));
        }
    }
    Ok(())
}
async fn owned(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    owner: u64,
    request: &ManagementRequest,
) -> Result<(Value, AssignExecution)> {
    let task = moves::require_gate(tr, ns, serial, request).await?;
    if task["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if tr.get(&volume_claim_key(request), false).await?.as_deref()
        != Some(volume_claim(request).as_slice())
    {
        return Err(CatalogError::Corrupt("分配原卷UUID保留记录缺失或身份不符"));
    }
    let execution = AssignExecution::parse(task["execution"].clone(), request)?;
    Ok((task, execution))
}
fn finish(tr: &Transaction, serial: &str, request: &ManagementRequest) {
    tr.clear(&identity_key(
        request.namespace(),
        serial,
        b'q',
        request.epoch(),
        request.operation(),
    ));
    tr.clear(&native_device::library_gate(request.namespace(), serial));
}
impl ManagedCatalog {
    /// Acknowledge an immutable plan before loading or formatting. Taking over
    /// adopts it for diagnosis as well as recovery; it grants no replay permission.
    pub async fn prepare_media_assign(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        plan: &AssignPlan,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        plan.matches(request)?;
        self.db.run(|tr, _| async move {
            requests::retry_result(async {
                let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                let mut task = original(&tr, serial, request).await?;
                if !matches!(task["status"].as_str(), Some("queued" | "running")) { return Ok(task); }
                if task["status"] == "running" {
                    moves::require_gate(&tr, ns, serial, request).await?;
                    let saved = AssignExecution::parse(task["execution"].clone(), request)?;
                    if saved.plan != *plan || task["owner_epoch"].as_u64().is_none_or(|e| e > owner) {
                        return Err(CatalogError::Corrupt("分配原计划或owner轮次被替换"));
                    }
                    task["owner_epoch"] = json!(owner);
                } else {
                    eligible(&tr, serial, request, plan).await?;
                    native_device::require_library_available(&tr, ns, serial).await?;
                    tr.set(&volume_claim_key(request),&volume_claim(request));
                    task["status"] = json!("running");
                    task["owner_epoch"] = json!(owner);
                    task["execution"] = AssignExecution { version:1, plan:plan.clone(), phase:AssignPhase::Prepared, error:None }.value();
                    tr.set(&native_device::library_gate(ns, serial), &encode(&json!({"version":1,"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()})));
                }
                write_task(&tr, request, &task).await?;
                Ok(task)
            }.await)
        }).await?
    }
    /// Persist command intent and recheck pinned pool/assignment before every stage.
    pub async fn stage_media_assign(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        next: AssignPhase,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let (mut task, mut execution) =
                            owned(&tr, serial, ns, owner, request).await?;
                        eligible(&tr, serial, request, &execution.plan).await?;
                        let valid = next == execution.phase
                            || matches!(
                                (execution.phase, next),
                                (
                                    AssignPhase::Prepared,
                                    AssignPhase::Moving | AssignPhase::Threading
                                ) | (AssignPhase::Moving, AssignPhase::Threading)
                                    | (AssignPhase::Threading, AssignPhase::Inspecting)
                                    | (AssignPhase::Inspecting, AssignPhase::Formatting)
                            );
                        if !valid
                            || matches!(next, AssignPhase::Prepared | AssignPhase::Confirmed)
                            || next == AssignPhase::Moving
                                && execution.plan.source == execution.plan.drive
                            || execution.phase == AssignPhase::Prepared
                                && next == AssignPhase::Threading
                                && execution.plan.source.kind != MoveLocationKind::Drive
                        {
                            return Err(CatalogError::Invalid("分配阶段不能回退或跳过检查点"));
                        }
                        execution.phase = next;
                        execution.error = None;
                        task["execution"] = execution.value();
                        write_task(&tr, request, &task).await?;
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    pub async fn defer_media_assign(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        detail: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        if detail.is_empty() || detail.len() > 2048 {
            return Err(CatalogError::Invalid("分配诊断长度无效"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let (mut task, mut execution) =
                            owned(&tr, serial, ns, owner, request).await?;
                        execution.error = Some(detail.into());
                        task["execution"] = execution.value();
                        write_task(&tr, request, &task).await?;
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    /// Reject only before formatting, after the device actor has confirmed there
    /// is no command in flight. A formatting result can never use this cleanup.
    pub async fn reject_media_assign(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        detail: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        if detail.is_empty() || detail.len() > 2048 {
            return Err(CatalogError::Invalid("分配拒绝诊断长度无效"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = original(&tr, serial, request).await?;
                        if !matches!(task["status"].as_str(), Some("queued" | "running")) {
                            return Ok(task);
                        }
                        let effects = if task["status"] == "running" {
                            let (_, execution) = owned(&tr, serial, ns, owner, request).await?;
                            if execution.phase != AssignPhase::Inspecting {
                                return Err(CatalogError::Invalid(
                                    "设备副作用尚未确认，不能清理分配任务",
                                ));
                            }
                            finish(&tr, serial, request);
                            tr.clear(&volume_claim_key(request));
                            "loaded_not_formatted"
                        } else {
                            if moves::pending_at(&tr, ns, serial).await?.is_some_and(|t| {
                                t["epoch"] == request.epoch()
                                    && t["operation"] == request.operation().to_string()
                            }) {
                                return Err(CatalogError::Corrupt("未开始分配任务意外持有gate"));
                            }
                            tr.clear(&identity_key(
                                ns,
                                serial,
                                b'q',
                                request.epoch(),
                                request.operation(),
                            ));
                            "not_started"
                        };
                        task["status"] = json!("failed");
                        task["owner_epoch"] = json!(owner);
                        task["result"] = json!({"error":detail,"effects":effects});
                        task["completed_at_millis"] =
                            json!(now().max(task["created_at_millis"].as_u64().expect("time")));
                        write_task(&tr, request, &task).await?;
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    /// Device proof is the exact complete, empty generation reread after format;
    /// no index-readiness or imported file heads are fabricated by assignment.
    pub async fn complete_media_assign(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        index: ActiveIndexIdentity,
        captured_at: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        let volume = Uuid::parse_str(
            request.value()["change"]["volume"]
                .as_str()
                .expect("volume"),
        )
        .expect("validated volume");
        if index.volume_uuid != volume
            || index.generation != 1
            || index.incremental
            || index.location.partition != crate::ltfs::label::PART_DATA
            || index.location.start_block != crate::ltfs::volume::FIRST_INDEX_BLOCK
        {
            return Err(CatalogError::Invalid("分配完成卷身份或初代索引位置不符"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = original(&tr, serial, request).await?;
                        if !matches!(task["status"].as_str(), Some("queued" | "running")) {
                            return Ok(task);
                        }
                        let (_, mut execution) = owned(&tr, serial, ns, owner, request).await?;
                        if execution.phase != AssignPhase::Formatting {
                            return Err(CatalogError::Invalid("分配尚未到达格式化确认阶段"));
                        }
                        eligible(&tr, serial, request, &execution.plan).await?;
                        let mut result = library_observations::rescan_result_at(
                            &tr,
                            ns,
                            serial,
                            owner,
                            captured_at,
                            None,
                        )
                        .await?;
                        let (inventory, _) =
                            library_observations::observations_at(&tr, ns, serial).await?;
                        let inventory =
                            inventory.ok_or(CatalogError::Corrupt("分配完成缺少库存确认"))?;
                        let drive = &execution.plan.drive;
                        if !inventory["drives"].as_array().is_some_and(|rows| {
                            rows.iter().any(|d| {
                                d["address"] == drive.address
                                    && d["serial"].as_str() == drive.drive_serial.as_deref()
                                    && d["barcode"] == execution.plan.barcode
                            })
                        }) {
                            return Err(CatalogError::Invalid("格式化完成未由原驱动器库存确认"));
                        }
                        let pool_id = Uuid::parse_str(
                            request.value()["change"]["pool"].as_str().expect("pool"),
                        )
                        .expect("pool");
                        let placement = VolumePlacement {
                            volume_id: volume,
                            namespace: ns,
                            pool_id,
                            library_serial: serial.into(),
                            barcode: execution.plan.barcode.clone(),
                        };
                        tr.set(&placement_key(volume), &encode_placement(&placement));
                        tr.set(&barcode_key(serial, &placement.barcode), volume.as_bytes());
                        tr.set(
                            &assignment_key(serial, &placement.barcode),
                            &encode_assignment(ns, pool_id, Some(volume)),
                        );
                        tr.set(
                            &namespace_assignment_key(ns, pool_id, serial, &placement.barcode),
                            &encode_namespace_assignment(serial, &placement.barcode),
                        );
                        tr.set(&unavailable_volume_key(ns, volume), &[]);
                        result["barcode"] = json!(placement.barcode);
                        result["pool"] = json!(pool_id.to_string());
                        result["volume"] = json!(volume.to_string());
                        result["index_imported"] = json!(false);
                        result["index_sha256"] = json!(
                            index
                                .sha256
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        );
                        task["status"] = json!("succeeded");
                        task["result"] = result;
                        task["completed_at_millis"] =
                            json!(now().max(task["created_at_millis"].as_u64().expect("time")));
                        execution.phase = AssignPhase::Confirmed;
                        execution.error = None;
                        task["execution"] = execution.value();
                        write_task(&tr, request, &task).await?;
                        finish(&tr, serial, request);
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
#[path = "management_assign_tests.rs"]
mod tests;
