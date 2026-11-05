//! Persistent plans for explicit mechanical moves; no blind replay on takeover.
use super::*;
#[path = "management_move_plan.rs"]
mod plan;
pub use plan::{MoveExecution, MoveLocation, MoveLocationKind, MovePhase, MovePlan};

pub(super) fn validate_execution(task: &Value, request: &ManagementRequest) -> Result<()> {
    if request.value()["change"]["kind"] != "media-move" {
        return Ok(());
    }
    match task["status"].as_str() {
        Some("queued" | "cancelled" | "failed") if task["execution"].is_null() => Ok(()),
        Some("running" | "succeeded") => {
            let execution = MoveExecution::parse(task["execution"].clone(), request)?;
            if (task["status"] == "succeeded") != (execution.phase == MovePhase::Confirmed) {
                return Err(CatalogError::Corrupt("搬带任务终态与机械阶段不符"));
            }
            Ok(())
        }
        _ => Err(CatalogError::Corrupt("搬带任务执行检查点无效")),
    }
}

fn require_request(
    serial: &str,
    route: ActiveLibraryRoute,
    request: &ManagementRequest,
) -> Result<()> {
    if request.value()["library"] != serial
        || request.namespace() != route.route.namespace
        || request.value()["change"]["kind"] != "media-move"
    {
        return Err(CatalogError::Invalid("搬带请求与库、命名空间或种类不符"));
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
    .ok_or(CatalogError::Corrupt("搬带原任务不存在"))?;
    if task["fingerprint"] != request.fingerprint() {
        return Err(CatalogError::Corrupt("搬带原请求指纹不符"));
    }
    Ok(task)
}
async fn pending_at(tr: &Transaction, ns: Uuid, serial: &str) -> Result<Option<Value>> {
    let Some(raw) = tr
        .get(&native_device::library_gate(ns, serial), false)
        .await?
    else {
        return Ok(None);
    };
    if raw.len() > 1024 {
        return Err(CatalogError::Corrupt("机械任务gate过大"));
    }
    let gate: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("机械任务gate JSON损坏"))?;
    if encode(&gate) != raw.as_ref()
        || !gate.as_object().is_some_and(|g| {
            g.len() == 4
                && ["version", "epoch", "operation", "fingerprint"]
                    .iter()
                    .all(|k| g.contains_key(*k))
        })
        || gate["version"] != 1
    {
        return Err(CatalogError::Corrupt("机械任务gate字段或编码无效"));
    }
    let epoch = gate["epoch"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(CatalogError::Corrupt("机械任务gate代次无效"))?;
    let operation = gate["operation"]
        .as_str()
        .and_then(|s| {
            Uuid::parse_str(s)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == s)
        })
        .ok_or(CatalogError::Corrupt("机械任务gate UUID无效"))?;
    let task = read_task(tr, ns, serial, epoch, operation)
        .await?
        .ok_or(CatalogError::Corrupt("机械任务gate缺少原任务"))?;
    if task["status"] != "running"
        || task["request"]["change"]["kind"] != "media-move"
        || task["fingerprint"] != gate["fingerprint"]
    {
        return Err(CatalogError::Corrupt("机械任务gate与原执行检查点不符"));
    }
    Ok(Some(task))
}
async fn require_gate(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    request: &ManagementRequest,
) -> Result<Value> {
    let task = pending_at(tr, ns, serial)
        .await?
        .ok_or(CatalogError::Corrupt("机械任务检查点缺少gate"))?;
    if task["fingerprint"] != request.fingerprint()
        || task["operation"] != request.operation().to_string()
        || task["epoch"] != request.epoch()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(task)
}
fn home_key(ns: Uuid, serial: &str, barcode: &str) -> Vec<u8> {
    let mut key = prefix(ns, b'H');
    append_component(&mut key, serial.as_bytes());
    append_component(&mut key, barcode.as_bytes());
    key
}
async fn home_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
) -> Result<Option<MovePlan>> {
    let Some(raw) = tr.get(&home_key(ns, serial, barcode), false).await? else {
        return Ok(None);
    };
    if raw.len() > 1024 {
        return Err(CatalogError::Corrupt("home记录过大"));
    }
    let home: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("home记录 JSON损坏"))?;
    if encode(&home) != raw.as_ref()
        || !home.as_object().is_some_and(|h| {
            h.len() == 4
                && ["version", "epoch", "operation", "address"]
                    .iter()
                    .all(|k| h.contains_key(*k))
        })
        || home["version"] != 1
    {
        return Err(CatalogError::Corrupt("home记录字段或编码无效"));
    }
    let epoch = home["epoch"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(CatalogError::Corrupt("home来源代次无效"))?;
    let operation = home["operation"]
        .as_str()
        .and_then(|s| {
            Uuid::parse_str(s)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == s)
        })
        .ok_or(CatalogError::Corrupt("home来源任务 UUID无效"))?;
    let task = read_task(tr, ns, serial, epoch, operation)
        .await?
        .ok_or(CatalogError::Corrupt("home缺少原任务证明"))?;
    let request = ManagementRequest::parse(task["request"].clone())
        .map_err(|_| CatalogError::Corrupt("home原请求损坏"))?;
    let execution = MoveExecution::parse(task["execution"].clone(), &request)?;
    if execution.plan.barcode != barcode
        || execution.plan.home_address.map(u64::from) != home["address"].as_u64()
        || !matches!(task["status"].as_str(), Some("running" | "succeeded"))
    {
        return Err(CatalogError::Corrupt("home与原条码、检查点或任务状态不符"));
    }
    Ok(Some(execution.plan))
}
async fn assignment_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    request: &ManagementRequest,
) -> Result<()> {
    let change = &request.value()["change"];
    let barcode = change["barcode"].as_str().expect("validated barcode");
    let assignment = ManagedCatalog::verified_assignment(tr, serial, barcode).await?;
    match assignment {
        None if change["pool"].is_null() => Ok(()),
        Some(a)
            if a.namespace == ns
                && change["pool"] == a.pool_id.to_string()
                && change["volume"]
                    == a.volume_id
                        .map(|id| json!(id.to_string()))
                        .unwrap_or(Value::Null)
                && change["revision"] == a.revision =>
        {
            Ok(())
        }
        _ => Err(CatalogError::Invalid(
            "磁带归属、卷或修订号已改变，保留原搬带请求核对",
        )),
    }
}
async fn save(
    tr: &Transaction,
    request: &ManagementRequest,
    task: &mut Value,
    execution: &MoveExecution,
) -> Result<()> {
    task["execution"] = execution.value();
    write_task(tr, request, task).await
}
impl ManagedCatalog {
    /// Device readers and automatic loaders must yield to the original
    /// mechanical checkpoint. Metadata/task inspection remains available.
    pub async fn require_active_library_readable(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        if pending_at(&tr, ns, serial).await?.is_some() {
            return Err(CatalogError::PoolBusy);
        }
        Ok(())
    }
    /// Strict persistent recovery priority. A gate never grants execution rights.
    pub async fn active_library_mechanical_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let task = pending_at(&tr, ns, serial).await?;
        if task
            .as_ref()
            .is_some_and(|t| t["owner_epoch"].as_u64().is_none_or(|e| e > owner))
        {
            return Err(CatalogError::Corrupt("机械任务轮次超过当前owner"));
        }
        Ok(task)
    }
    pub async fn active_tape_home_plan(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        barcode: &str,
    ) -> Result<Option<MovePlan>> {
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        home_at(&tr, ns, serial, barcode).await
    }
    /// The original plan and shared exclusion must be acknowledged before UNLOAD
    /// or MOVE. Retry never changes a previously prepared destination or map.
    pub async fn prepare_media_move(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        plan: &MovePlan,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        plan.matches(request)?;
        self.db.run(|tr, _| async move {
            let outcome: Result<Value> = async {
                let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                let mut task = original(&tr, serial, request).await?;
                if !matches!(task["status"].as_str(), Some("queued" | "running")) { return Ok(task); }
                if task["status"] == "running" {
                    let current = require_gate(&tr, ns, serial, request).await?;
                    let execution = MoveExecution::parse(current["execution"].clone(), request)?;
                    if execution.plan != *plan { return Err(CatalogError::Corrupt("禁止替换已准备的原搬带计划")); }
                    if task["owner_epoch"].as_u64().is_none_or(|old| old > owner) { return Err(CatalogError::Corrupt("搬带原owner轮次无效")); }
                    // Adopt recovery even if an assignment changed. Every command
                    // stage and final receipt still check the pinned assignment;
                    // the new owner can then persist the unresolved diagnostic.
                    task["owner_epoch"] = json!(owner);
                    write_task(&tr, request, &task).await?;
                    return Ok(task);
                }
                assignment_at(&tr, ns, serial, request).await?;
                native_device::require_library_available(&tr, ns, serial).await?;
                let home = home_at(&tr, ns, serial, &plan.barcode).await?;
                if let Some(home) = home {
                    if home.map != plan.map || home.home_address != plan.home_address { return Err(CatalogError::Invalid("原home与当前计划map或地址不符")); }
                } else if let Some(address) = plan.home_address {
                    if plan.source.kind == MoveLocationKind::ImportExport
                        || (plan.source.kind == MoveLocationKind::Storage && address != plan.source.address) {
                        return Err(CatalogError::Invalid("不能从没有home证明的I/E介质学习home"));
                    }
                    tr.set(&home_key(ns, serial, &plan.barcode), &encode(&json!({"version":1,"epoch":request.epoch(),"operation":request.operation().to_string(),"address":address})));
                }
                task["status"] = json!("running");
                task["owner_epoch"] = json!(owner);
                save(&tr, request, &mut task, &MoveExecution { version:1, plan:plan.clone(), phase:MovePhase::Prepared, error:None }).await?;
                tr.set(&native_device::library_gate(ns, serial), &encode(&json!({"version":1,"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()})));
                Ok(task)
            }.await;
            requests::retry_result(outcome)
        })
        .await?
    }
    /// Persist the next exact command's intent before sending it to hardware.
    pub async fn stage_media_move(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        next: MovePhase,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
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
                        require_gate(&tr, ns, serial, request).await?;
                        if task["status"] != "running" || task["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        assignment_at(&tr, ns, serial, request).await?;
                        let mut execution =
                            MoveExecution::parse(task["execution"].clone(), request)?;
                        let valid = next == execution.phase
                            || matches!(
                                (execution.phase, next),
                                (
                                    MovePhase::Prepared,
                                    MovePhase::Unloading | MovePhase::Moving
                                ) | (MovePhase::Unloading, MovePhase::Moving)
                            );
                        if !valid
                            || next == MovePhase::Confirmed
                            || (execution.phase == MovePhase::Prepared
                                && next == MovePhase::Moving
                                && execution.plan.source.kind == MoveLocationKind::Drive)
                            || (next == MovePhase::Unloading
                                && execution.plan.source.kind != MoveLocationKind::Drive)
                        {
                            return Err(CatalogError::Invalid("搬带阶段不能回退或跳过机械检查点"));
                        }
                        execution.phase = next;
                        execution.error = None;
                        save(&tr, request, &mut task, &execution).await?;
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    /// A mechanical error can be an unknown result. Keep the gate and plan;
    /// expose the original diagnostic instead of reporting a safe cancellation.
    pub async fn defer_media_move(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        detail: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        if detail.is_empty() || detail.len() > 2048 {
            return Err(CatalogError::Invalid("搬带诊断长度无效"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = original(&tr, serial, request).await?;
                        if task["status"] != "running" || task["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        require_gate(&tr, ns, serial, request).await?;
                        let mut execution =
                            MoveExecution::parse(task["execution"].clone(), request)?;
                        execution.error = Some(detail.into());
                        save(&tr, request, &mut task, &execution).await?;
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    pub async fn fail_unprepared_media_move(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        detail: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
        if detail.is_empty() || detail.len() > 2048 {
            return Err(CatalogError::Invalid("搬带准备诊断长度无效"));
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
                        if task["status"] != "queued" || !task["execution"].is_null() {
                            return Err(CatalogError::Invalid(
                                "已准备机械计划不能作为未执行失败清理",
                            ));
                        }
                        if pending_at(&tr, ns, serial).await?.is_some_and(|t| {
                            t["operation"] == request.operation().to_string()
                                && t["epoch"] == request.epoch()
                        }) {
                            return Err(CatalogError::Corrupt("未准备任务意外持有机械gate"));
                        }
                        task["status"] = json!("failed");
                        task["owner_epoch"] = json!(owner);
                        task["completed_at_millis"] =
                            json!(now().max(task["created_at_millis"].as_u64().expect("time")));
                        task["result"] = json!({"error":detail,"effects":"not_started"});
                        write_task(&tr, request, &task).await?;
                        tr.clear(&identity_key(
                            ns,
                            serial,
                            b'q',
                            request.epoch(),
                            request.operation(),
                        ));
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    pub async fn complete_media_move(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        captured_at: &str,
    ) -> Result<Value> {
        require_request(serial, route, request)?;
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
                        require_gate(&tr, ns, serial, request).await?;
                        if task["status"] != "running" || task["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        assignment_at(&tr, ns, serial, request).await?;
                        let mut execution =
                            MoveExecution::parse(task["execution"].clone(), request)?;
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
                            inventory.ok_or(CatalogError::Corrupt("搬带完成缺少确认库存"))?;
                        let volumes = inventory["volumes"]
                            .as_array()
                            .ok_or(CatalogError::Corrupt("搬带完成库存格式无效"))?;
                        let plan = &execution.plan;
                        let destination_kind = match plan.destination.kind {
                            MoveLocationKind::Storage => 2,
                            MoveLocationKind::ImportExport => 3,
                            MoveLocationKind::Drive => {
                                return Err(CatalogError::Corrupt("搬带目标不能为驱动器"));
                            }
                        };
                        if !volumes.iter().any(|v| {
                            v["barcode"] == plan.barcode
                                && v["address"] == plan.destination.address
                                && v["element_type"] == destination_kind
                        }) || (plan.source.address != plan.destination.address
                            && volumes.iter().any(|v| v["address"] == plan.source.address))
                        {
                            return Err(CatalogError::Invalid(
                                "搬带目标或原源位置尚未由完整库存确认",
                            ));
                        }
                        result["barcode"] = json!(plan.barcode);
                        result["from"] = json!(plan.source.address);
                        result["to"] = json!(plan.destination.address);
                        result["home_address"] = json!(plan.home_address);
                        task["status"] = json!("succeeded");
                        task["result"] = result;
                        task["completed_at_millis"] =
                            json!(now().max(task["created_at_millis"].as_u64().expect("time")));
                        execution.phase = MovePhase::Confirmed;
                        execution.error = None;
                        save(&tr, request, &mut task, &execution).await?;
                        tr.clear(&identity_key(
                            ns,
                            serial,
                            b'q',
                            request.epoch(),
                            request.operation(),
                        ));
                        tr.clear(&native_device::library_gate(ns, serial));
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
#[path = "management_move_tests.rs"]
mod tests;
