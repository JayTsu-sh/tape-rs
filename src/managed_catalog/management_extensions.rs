//! Additional policy and intake operations on the canonical management ledger.
use super::*;
use std::collections::BTreeSet;
fn id(value: &Value, name: &str) -> Result<Uuid> {
    value[name]
        .as_str()
        .and_then(|s| {
            Uuid::parse_str(s)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == s)
        })
        .ok_or(CatalogError::Invalid("管理 UUID 无效或不规范"))
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .ok_or(CatalogError::Invalid("管理文本字段无效"))
}

fn number(value: &Value, name: &str) -> Result<u64> {
    value[name]
        .as_u64()
        .ok_or(CatalogError::Invalid("管理数值字段无效"))
}

fn revision(value: &Value) -> Result<u64> {
    number(value, "revision")?
        .checked_add(1)
        .ok_or(CatalogError::Invalid("管理修订号溢出"))
}

fn valid_drive(serial: &str) -> Result<()> {
    validate_serial(serial)?;
    if serial.contains('/') || serial.chars().any(char::is_whitespace) {
        return Err(CatalogError::Invalid("驱动器序列号无效"));
    }
    Ok(())
}

fn patch(value: &Value, pool: bool) -> Result<()> {
    let fields = value
        .as_object()
        .filter(|v| !v.is_empty())
        .ok_or(CatalogError::Invalid("策略 patch 必须是非空对象"))?;
    for (key, value) in fields {
        let good = match (pool, key.as_str()) {
            (true, "name") => value
                .as_str()
                .is_some_and(|name| validate_pool_name(name).is_ok()),
            (true, "file_limit") => value.as_u64().is_some_and(|n| n > 0),
            (true, "media_restriction") => value.as_str().is_some_and(valid_media_restriction),
            (true, "mount_limit" | "low_space_warning_threshold") => {
                value.as_u64().is_some_and(|n| n <= 32767)
            }
            (true, "low_space_warning_enabled" | "no_space_warning_enabled")
            | (false, "enabled") => value.is_boolean(),
            (false, "role") => value.as_str().is_some_and(|v| {
                !v.is_empty()
                    && v.len() <= 3
                    && v.bytes().all(|b| b"mrg".contains(&b))
                    && v.bytes().collect::<BTreeSet<_>>().len() == v.len()
            }),
            _ => false,
        };
        if !good {
            return Err(CatalogError::Invalid("策略字段、类型或范围无效"));
        }
    }
    Ok(())
}

fn resource_key(namespace: Uuid, kind: u8, target: &str) -> Vec<u8> {
    let mut out = prefix(namespace, kind);
    append_component(&mut out, target.as_bytes());
    out
}

async fn settings(tr: &Transaction, namespace: Uuid, target: &str) -> Result<Value> {
    let Some(bytes) = tr
        .get(&resource_key(namespace, b'd', target), false)
        .await?
    else {
        return Ok(json!({"revision":0,"enabled":true,"role":"mrg"}));
    };
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| CatalogError::Corrupt("驱动器策略编码"))?;
    if encode(&value) != bytes.as_ref() || !value["revision"].as_u64().is_some_and(|n| n > 0) {
        return Err(CatalogError::Corrupt("驱动器策略修订号"));
    }
    let mut attributes = value.clone();
    attributes
        .as_object_mut()
        .ok_or(CatalogError::Corrupt("驱动器策略对象"))?
        .remove("revision");
    patch(&attributes, false).map_err(|_| CatalogError::Corrupt("驱动器策略字段"))?;
    if value.as_object().is_none_or(|v| v.len() != 3) {
        return Err(CatalogError::Corrupt("驱动器策略字段数量"));
    }
    Ok(value)
}

fn apply_pool_patch(pool: &mut PoolRecord, patch: &Value) {
    for (key, value) in patch.as_object().expect("validated patch") {
        match key.as_str() {
            "name" => pool.name = value.as_str().expect("validated text").into(),
            "file_limit" => pool.file_limit = value.as_u64().expect("validated number"),
            "media_restriction" => {
                pool.policy.media_restriction = value.as_str().expect("validated text").into()
            }
            "mount_limit" => {
                pool.policy.mount_limit = value.as_u64().expect("validated number") as u16
            }
            "low_space_warning_enabled" => {
                pool.policy.low_space_warning_enabled = value.as_bool().expect("validated flag")
            }
            "low_space_warning_threshold" => {
                pool.policy.low_space_warning_threshold =
                    value.as_u64().expect("validated number") as u16
            }
            "no_space_warning_enabled" => {
                pool.policy.no_space_warning_enabled = value.as_bool().expect("validated flag")
            }
            _ => unreachable!("validated patch"),
        }
    }
}

async fn tape_policy_at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    barcode: &str,
) -> Result<TapeAssignment> {
    let assignment = ManagedCatalog::verified_assignment(tr, serial, barcode)
        .await?
        .filter(|assignment| assignment.namespace == namespace)
        .ok_or(CatalogError::Invalid("磁带不属于当前活动带库命名空间"))?;
    assignment
        .volume_id
        .ok_or(CatalogError::Invalid("磁带尚未绑定卷"))?;
    Ok(assignment)
}

fn validate_admission_change(change: &Value) -> Result<()> {
    if change["kind"] == "tape" && change["patch"]["online"] == true {
        return Err(CatalogError::Invalid(
            "online=true 必须通过 tape online 的实带核验任务",
        ));
    }
    Ok(())
}

async fn apply_change(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    change: &Value,
) -> Result<Value> {
    validate_admission_change(change)?;
    match text(change, "kind")? {
        "pool-delete" => Err(CatalogError::Invalid("池操作必须由共享池执行器处理")),
        "tape" => {
            reclaim::require_pool_write_at(tr, namespace, id(change, "pool")?).await?;
            let mut assignment =
                tape_policy_at(tr, namespace, serial, text(change, "barcode")?).await?;
            super::native_media::require_pool_readable(tr, namespace, assignment.pool_id).await?;
            if assignment.pool_id != id(change, "pool")?
                || assignment.volume_id != Some(id(change, "volume")?)
            {
                return Err(CatalogError::Invalid("磁带池或卷身份已改变"));
            }
            if assignment.revision != number(change, "revision")? {
                return Err(CatalogError::Invalid("磁带策略修订号已改变"));
            }
            let mut attributes = tape_policy::read_at(tr, &assignment).await?;
            if change["patch"]["offline_message"].is_string()
                && assignment.online
                && change["patch"]["online"] != false
            {
                return Err(CatalogError::Invalid("offline_message 只能用于离线磁带"));
            }
            if change["patch"]["online"] == false {
                assignment.online = false;
                if attributes["offline_message"].is_null() {
                    attributes["offline_message"] =
                        json!(chrono::Utc::now().format("%Y%m%d").to_string());
                }
            }
            for key in ["offline_message"] {
                if let Some(value) = change["patch"].get(key) {
                    attributes[key] = value.clone();
                }
            }
            if let Some(blacklisted) = change["patch"]["blacklisted"].as_bool() {
                assignment.blacklisted = blacklisted;
            }
            assignment.revision = revision(change)?;
            tr.set(
                &assignment_key(serial, &assignment.barcode),
                &encode_assignment_state(
                    namespace,
                    assignment.pool_id,
                    assignment.volume_id,
                    assignment.revision,
                    assignment.online,
                    assignment.blacklisted,
                ),
            );
            tape_policy::write_at(tr, &assignment, &attributes);
            let mut view = assignment.to_json();
            for (k, v) in attributes.as_object().expect("validated attributes") {
                view[k] = v.clone();
            }
            Ok(json!({"tape":view}))
        }
        "pool" => {
            let id = id(change, "id")?;
            reclaim::require_pool_write_at(tr, namespace, id).await?;
            super::native_media::require_pool_readable(tr, namespace, id).await?;
            require_pool_library(tr, namespace, id, serial).await?;
            let raw = tr
                .get(&pool_key(namespace, id), false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let mut pool = decode_pool(id, &raw)?;
            active_pools::require_indexes(tr, namespace, &pool).await?;
            if pool.revision != number(change, "revision")? {
                return Err(CatalogError::Invalid("池修订号已改变"));
            }
            let old_name = pool.name.clone();
            apply_pool_patch(&mut pool, &change["patch"]);
            pool.revision = revision(change)?;
            validate_pool(&pool)?;
            let name = pool_name_key(namespace, &pool.name);
            if old_name != pool.name
                && let Some(existing) = tr.get(&name, false).await?
            {
                let occupied = uuid_from(&existing)?;
                if occupied.is_nil() || occupied == id {
                    return Err(CatalogError::Corrupt("目标名称存在孤立池引用"));
                }
                let raw = tr
                    .get(&pool_key(namespace, occupied), false)
                    .await?
                    .ok_or(CatalogError::Corrupt("目标名称引用的池不存在"))?;
                if decode_pool(occupied, &raw)?.name != pool.name {
                    return Err(CatalogError::Corrupt("目标名称与池主记录不符"));
                }
                // Validate before any name mutations: Invalid is saved
                // as a failed task by the caller in this same transaction.
                return Err(CatalogError::Invalid("池名称已被使用"));
            }
            if old_name != pool.name {
                tr.clear(&pool_name_key(namespace, &old_name));
                tr.set(&name, id.as_bytes());
            }
            tr.set(&pool_key(namespace, id), &encode_pool(&pool));
            Ok(json!({"pool":pool.to_json()?}))
        }
        "drive" => {
            let target = text(change, "serial")?;
            let current = settings(tr, namespace, target).await?;
            if current["revision"] != change["revision"] {
                return Err(CatalogError::Invalid("驱动器策略修订号已改变"));
            }
            let mut next = current;
            next["revision"] = json!(revision(change)?);
            for (key, value) in change["patch"].as_object().expect("validated patch") {
                next[key] = value.clone();
            }
            tr.set(&resource_key(namespace, b'd', target), &encode(&next));
            Ok(json!({"target":target,"policy":next}))
        }
        _ => Err(CatalogError::Invalid("未知管理变更")),
    }
}

impl ManagedCatalog {
    async fn finish_extended_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        cancel: bool,
    ) -> Result<Value> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                        let key =
                            identity_key(route.route.namespace, serial, b'o', epoch, operation);
                        let raw = tr
                            .get(&key, false)
                            .await?
                            .ok_or(CatalogError::Invalid("管理请求不存在"))?;
                        let mut value =
                            task(&raw, serial, route.route.namespace, epoch, operation)?;
                        require_queue(&tr, route.route.namespace, serial, epoch, operation, &value)
                            .await?;
                        if value["status"] == "running" && cancel {
                            let request = ManagementRequest::parse(value["request"].clone())?;
                            if request.value()["change"]["kind"]
                                .as_str()
                                .is_some_and(device_kind)
                            {
                                super::native_device::request_cancel(&tr, serial, route, &request)
                                    .await?;
                                return Ok(value);
                            }
                            super::native_media::request_abort(
                                &tr,
                                request.namespace(),
                                id(&request.value()["change"], "pool")?,
                                epoch,
                                operation,
                            )
                            .await?;
                            return Ok(value);
                        }
                        if value["status"] != "queued" {
                            return Ok(value);
                        }
                        if (matches!(
                            value["request"]["change"]["kind"].as_str(),
                            Some("media-import")
                        ) || value["request"]["change"]["kind"]
                            .as_str()
                            .is_some_and(device_kind))
                            && !cancel
                        {
                            return Err(CatalogError::Invalid("介质任务必须由设备执行器处理"));
                        }
                        let request = ManagementRequest::parse(value["request"].clone())?;
                        retained(&tr, &request).await?;
                        let queued =
                            identity_key(request.namespace(), serial, b'q', epoch, operation);
                        if tr.get(&queued, false).await?.as_deref() != Some(&[]) {
                            return Err(CatalogError::Corrupt("排队管理请求索引缺失"));
                        }
                        let (status, result) = if cancel {
                            ("cancelled", json!({"cancelled":true}))
                        } else {
                            match apply_change(
                                &tr,
                                request.namespace(),
                                serial,
                                &request.value()["change"],
                            )
                            .await
                            {
                                Ok(result) => ("succeeded", result),
                                Err(CatalogError::Invalid(reason)) => {
                                    ("failed", json!({"error":reason}))
                                }
                                Err(e) => return Err(e),
                            }
                        };
                        value["status"] = json!(status);
                        value["result"] = result;
                        value["owner_epoch"] = json!(owner);
                        value["completed_at_millis"] = json!(
                            now().max(value["created_at_millis"].as_u64().expect("validated time"))
                        );
                        write_task(&tr, &request, &value).await?;
                        tr.clear(&queued);
                        if request.value()["change"]["kind"] == "tape" {
                            notifications::tape_changed_at(
                                &tr,
                                request.namespace(),
                                serial,
                                text(&request.value()["change"], "barcode")?,
                                "policy_changed",
                            )
                            .await?;
                        }
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn reject_queued_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        reason: &str,
    ) -> Result<Value> {
        if reason.len() > 2048 {
            return Err(CatalogError::Invalid("任务失败上下文过长"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                        let (request, mut task) =
                            task_for_identity(&tr, serial, route.route.namespace, epoch, operation)
                                .await?;
                        if task["status"] != "queued" {
                            return Ok(task);
                        }
                        task["status"] = json!("failed");
                        task["owner_epoch"] = json!(owner);
                        task["result"] = json!({"error":reason});
                        task["completed_at_millis"] = json!(
                            now().max(task["created_at_millis"].as_u64().expect("validated time"))
                        );
                        write_task(&tr, &request, &task).await?;
                        clear_queued(&tr, &request);
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn active_resource_policy(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        kind: &str,
        target: &str,
    ) -> Result<Value> {
        if kind == "tape" {
            validate_barcode(target)?;
            let tr = self.db.create_trx()?;
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
            window(&tr, route.route.namespace).await?;
            let assignment = tape_policy_at(&tr, route.route.namespace, serial, target).await?;
            return tape_policy::view_at(&tr, &assignment).await;
        }
        if kind != "drive" {
            return Err(CatalogError::Invalid("策略资源无效"));
        }
        valid_drive(target)?;
        let tr = self.db.create_trx()?;
        routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        window(&tr, route.route.namespace).await?;
        settings(&tr, route.route.namespace, target).await
    }

    pub async fn active_management_policy(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        drives: &[String],
        pools: &[Uuid],
        barcodes: &[String],
    ) -> Result<Value> {
        if drives.len() > 64 || pools.len() > 65 || barcodes.len() > 65 {
            return Err(CatalogError::Invalid("设备策略快照超过上限"));
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        window(&tr, ns).await?;
        let mut view = json!({"pools":{},"drives":{},"tapes":{}});
        for serial in drives {
            let policy = settings(&tr, ns, serial).await?;
            view["drives"][serial] = json!({"enabled":policy["enabled"],"role":policy["role"]});
        }
        for barcode in barcodes.iter().collect::<BTreeSet<_>>() {
            let assignment = Self::verified_assignment(&tr, serial, barcode)
                .await?
                .filter(|assignment| assignment.namespace == ns)
                .ok_or(CatalogError::Corrupt("调度磁带缺少当前命名空间归属"))?;
            view["tapes"][barcode] = tape_policy::view_at(&tr, &assignment).await?;
        }
        for id in pools.iter().copied().collect::<BTreeSet<_>>() {
            require_pool_library(&tr, ns, id, serial).await?;
            let raw = tr
                .get(&pool_key(ns, id), false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let pool = decode_pool(id, &raw)?;
            active_pools::require_indexes(&tr, ns, &pool).await?;
            view["pools"][id.to_string()] = json!({"file_limit":pool.file_limit,"mediarestriction":pool.policy.media_restriction,"mountlimit":pool.policy.mount_limit.to_string()});
        }
        Ok(view)
    }

    pub async fn apply_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Value> {
        let value = self
            .management_task(serial, route, epoch, operation)
            .await?
            .ok_or(CatalogError::Invalid("管理请求不存在"))?;
        if matches!(
            value["request"]["change"]["kind"].as_str(),
            Some("pool" | "pool-delete")
        ) {
            return self
                .apply_pool_management(serial, route, owner, epoch, operation)
                .await;
        }
        self.finish_extended_management(serial, route, owner, epoch, operation, false)
            .await
    }
}
pub(super) async fn cancel_running(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    value: &Value,
) -> Result<Value> {
    let request = ManagementRequest::parse(value["request"].clone())?;
    match request.value()["change"]["kind"].as_str() {
        Some("media-unassign" | "media-validate" | "media-online") => {
            super::super::native_device::request_cancel(tr, serial, route, &request).await?
        }
        Some("media-import") => {
            super::super::native_media::request_abort(
                tr,
                request.namespace(),
                id(&request.value()["change"], "pool")?,
                request.epoch(),
                request.operation(),
            )
            .await?
        }
        _ => {
            return Err(CatalogError::Invalid(
                "执行中任务须由对应设备执行器安全取消",
            ));
        }
    }
    Ok(value.clone())
}
