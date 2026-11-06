//! Native management requests, completed atomically by the device actor between
//! media operations. Pending requests survive owner changes.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(crate) fn prefix(namespace: Uuid, kind: u8) -> Vec<u8> {
    let mut out = key(b'@');
    out.extend_from_slice(namespace.as_bytes());
    out.push(kind);
    out
}
fn identity_key(namespace: Uuid, kind: u8, epoch: u64, id: Uuid) -> Vec<u8> {
    let mut out = prefix(namespace, kind);
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(id.as_bytes());
    out
}
pub(crate) fn initialize(tr: &Transaction, namespace: Uuid) {
    tr.set(
        &prefix(namespace, b'w'),
        &requests::encode_window(RequestWindow {
            first_retained: 1,
            accepting: 1,
        }),
    );
}
pub(super) async fn window(tr: &Transaction, namespace: Uuid) -> Result<RequestWindow> {
    let bytes = tr
        .get(&prefix(namespace, b'w'), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    requests::decode_window(&bytes)
}
fn fields(value: &Value, names: &[&str]) -> Result<()> {
    if !value
        .as_object()
        .is_some_and(|v| v.len() == names.len() && v.keys().all(|k| names.contains(&k.as_str())))
    {
        return Err(CatalogError::Invalid("管理请求字段不匹配"));
    }
    Ok(())
}
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
pub(super) fn device_kind(kind: &str) -> bool {
    matches!(
        kind,
        "library-rescan"
            | "media-move"
            | "media-unassign"
            | "media-validate"
            | "media-online"
            | "media-assign"
    )
}

fn validate_change(value: &Value) -> Result<()> {
    match text(value, "kind")? {
        "tape" => {
            fields(
                value,
                &["kind", "pool", "volume", "barcode", "revision", "patch"],
            )?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("磁带修订号不能为零"));
            }
            revision(value)?;
            let patch = value["patch"]
                .as_object()
                .filter(|v| !v.is_empty())
                .ok_or(CatalogError::Invalid("磁带策略 patch 不能为空"))?;
            for (name, value) in patch {
                let valid = match name.as_str() {
                    "online" => value.is_boolean(),
                    "blacklisted" => value.is_boolean(),
                    "offline_message" => tape_policy::valid_message(value),
                    _ => false,
                };
                if !valid {
                    return Err(CatalogError::Invalid(
                        "磁带策略仅支持 online=false、blacklisted 布尔值及 offline_message；上线须使用 tape online",
                    ));
                }
            }
        }
        "library-rescan" => fields(value, &["kind"])?,
        "media-assign" => {
            fields(
                value,
                &[
                    "kind",
                    "pool",
                    "barcode",
                    "volume",
                    "confirm_format",
                    "force_format",
                ],
            )?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if value["confirm_format"] != true || !value["force_format"].is_boolean() {
                return Err(CatalogError::Invalid(
                    "分配必须明确确认格式化及 force_format 布尔值",
                ));
            }
        }
        "media-unassign" | "media-validate" | "media-online" => {
            if value["kind"] == "media-online" {
                revision(value)?;
            }
            fields(value, &["kind", "pool", "barcode", "volume", "revision"])?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("磁带修订号不能为零"));
            }
        }
        "media-move" => {
            fields(
                value,
                &[
                    "kind",
                    "pool",
                    "barcode",
                    "volume",
                    "revision",
                    "destination",
                ],
            )?;
            validate_barcode(text(value, "barcode")?)?;
            if !matches!(text(value, "destination")?, "homeslot" | "ieslot") {
                return Err(CatalogError::Invalid("移动目的地必须为 homeslot 或 ieslot"));
            }
            if value["pool"].is_null() {
                if !value["volume"].is_null() || !value["revision"].is_null() {
                    return Err(CatalogError::Invalid("未分配磁带不得指定卷或修订号"));
                }
            } else {
                id(value, "pool")?;
                id(value, "volume")?;
                if number(value, "revision")? == 0 {
                    return Err(CatalogError::Invalid("磁带修订号不能为零"));
                }
            }
        }
        "media-import" => {
            fields(
                value,
                &["kind", "pool", "barcode", "volume", "path", "merge_tapes"],
            )?;
            let pool = id(value, "pool")?;
            validate_barcode(text(value, "barcode")?)?;
            let path = text(value, "path")?;
            pool_catalog_path(pool, path.as_bytes())?;
            if !value["merge_tapes"].is_boolean() {
                return Err(CatalogError::Invalid("导入 merge_tapes 必须为布尔值"));
            }
            if value["merge_tapes"] == false {
                pool_catalog_path(
                    pool,
                    format!("{}/{}", path.trim_end_matches('/'), text(value, "barcode")?)
                        .as_bytes(),
                )?;
            }
            if !value["volume"].is_null() {
                id(value, "volume")?;
            }
        }
        "pool" => {
            fields(value, &["kind", "id", "revision", "patch"])?;
            id(value, "id")?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("池修订号不能为零"));
            }
            revision(value)?;
            patch(&value["patch"], true)?;
        }
        "pool-delete" => {
            fields(value, &["kind", "id", "revision"])?;
            id(value, "id")?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("池修订号不能为零"));
            }
            revision(value)?;
        }
        "drive" => {
            fields(value, &["kind", "serial", "revision", "patch"])?;
            valid_drive(text(value, "serial")?)?;
            revision(value)?;
            patch(&value["patch"], false)?;
        }
        _ => return Err(CatalogError::Invalid("未知管理操作")),
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ManagementRequest {
    value: Value,
    namespace: Uuid,
    epoch: u64,
    operation: Uuid,
}
impl ManagementRequest {
    pub fn parse(value: Value) -> Result<Self> {
        let request = Self::decode_envelope(value)?;
        validate_change(&request.value["change"])?;
        Ok(request)
    }
    // Terminal audit records retain their original envelope and fingerprint.
    // Decoding them grants no permission to admit or execute a removed action.
    fn decode_envelope(value: Value) -> Result<Self> {
        fields(
            &value,
            &[
                "version",
                "library",
                "namespace",
                "epoch",
                "operation",
                "change",
            ],
        )?;
        if value["version"] != 1 || value.to_string().len() > 4096 {
            return Err(CatalogError::Invalid("管理请求版本或大小无效"));
        }
        validate_physical_serial(text(&value, "library")?)?;
        let namespace = id(&value, "namespace")?;
        let operation = id(&value, "operation")?;
        let epoch = number(&value, "epoch")?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("管理请求代次不能为零"));
        }
        if !value["change"].is_object()
            || !value["change"]["kind"].as_str().is_some_and(|kind| {
                !kind.is_empty()
                    && kind.len() <= 63
                    && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err(CatalogError::Invalid("管理变更类型或结构无效"));
        }
        Ok(Self {
            value,
            namespace,
            epoch,
            operation,
        })
    }
    /// Admission policy is not a record decoder: querying an immutable receipt
    /// must not rerun today's permission checks or issue device work.
    pub fn validate_admission(&self) -> Result<()> {
        validate_admission_change(&self.value["change"])
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn namespace(&self) -> Uuid {
        self.namespace
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn operation(&self) -> Uuid {
        self.operation
    }
    pub(super) fn fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(self.value.to_string().as_bytes()))
    }
}
pub(super) fn now() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}
fn encode(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}
fn task(bytes: &[u8], serial: &str, namespace: Uuid, epoch: u64, operation: Uuid) -> Result<Value> {
    if bytes.len() > 16384 {
        return Err(CatalogError::Corrupt("管理回执过大"));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("管理回执 JSON"))?;
    fields(
        &value,
        &[
            "version",
            "library_serial",
            "namespace",
            "operation",
            "epoch",
            "request",
            "fingerprint",
            "status",
            "created_at_millis",
            "completed_at_millis",
            "owner_epoch",
            "result",
        ],
    )
    .map_err(|_| CatalogError::Corrupt("管理回执字段"))?;
    let request = if matches!(
        value["status"].as_str(),
        Some("succeeded" | "failed" | "cancelled")
    ) {
        ManagementRequest::decode_envelope(value["request"].clone())
    } else {
        ManagementRequest::parse(value["request"].clone())
    }
    .map_err(|_| CatalogError::Corrupt("管理回执请求"))?;
    if encode(&value) != bytes
        || request.namespace != namespace
        || request.epoch != epoch
        || request.operation != operation
        || request.value["library"] != serial
        || value["library_serial"] != serial
        || value["namespace"] != namespace.to_string()
        || value["epoch"] != epoch
        || value["operation"] != operation.to_string()
        || value["version"] != 1
        || value["fingerprint"] != request.fingerprint()
        || !value["created_at_millis"].as_u64().is_some_and(|n| n > 0)
        || !matches!(
            value["status"].as_str(),
            Some("queued" | "running" | "succeeded" | "failed" | "cancelled")
        )
    {
        return Err(CatalogError::Corrupt("管理回执身份或状态不一致"));
    }
    if value["status"] == "queued" {
        if value["owner_epoch"] != 0
            || !value["result"].is_null()
            || !value["completed_at_millis"].is_null()
        {
            return Err(CatalogError::Corrupt("排队管理回执无效"));
        }
    } else if value["status"] == "running" {
        if !(matches!(
            request.value["change"]["kind"].as_str(),
            Some("media-import")
        ) || request.value["change"]["kind"]
            .as_str()
            .is_some_and(device_kind))
            || !value["owner_epoch"].as_u64().is_some_and(|n| n > 0)
            || !value["result"].is_null()
            || !value["completed_at_millis"].is_null()
        {
            return Err(CatalogError::Corrupt("执行中介质回执无效"));
        }
    } else if !value["owner_epoch"].as_u64().is_some_and(|n| n > 0)
        || !value["result"].is_object()
        || !value["completed_at_millis"]
            .as_u64()
            .is_some_and(|n| Some(n) >= value["created_at_millis"].as_u64())
    {
        return Err(CatalogError::Corrupt("终态管理回执无效"));
    }
    Ok(value)
}
async fn require_queue(
    tr: &Transaction,
    ns: Uuid,
    epoch: u64,
    operation: Uuid,
    value: &Value,
) -> Result<()> {
    let queued = tr
        .get(&identity_key(ns, b'q', epoch, operation), false)
        .await?;
    let valid = if matches!(value["status"].as_str(), Some("queued" | "running")) {
        queued.as_deref() == Some(&[])
    } else {
        queued.is_none()
    };
    if !valid {
        return Err(CatalogError::Corrupt("管理回执与排队索引不符"));
    }
    Ok(())
}

async fn retained(tr: &Transaction, request: &ManagementRequest) -> Result<RequestWindow> {
    let window = window(tr, request.namespace).await?;
    if request.epoch < window.first_retained || request.epoch > window.accepting {
        return Err(CatalogError::Invalid("管理请求代次不在保留窗口"));
    }
    Ok(window)
}
pub(super) async fn task_for_identity(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<(ManagementRequest, Value)> {
    let raw = tr
        .get(&identity_key(ns, b'o', epoch, operation), false)
        .await?
        .ok_or(CatalogError::Invalid("管理请求不存在"))?;
    let value = task(&raw, serial, ns, epoch, operation)?;
    require_queue(tr, ns, epoch, operation, &value).await?;
    let request = ManagementRequest::parse(value["request"].clone())?;
    retained(tr, &request).await?;
    Ok((request, value))
}
pub(super) async fn read_task(
    tr: &Transaction,
    serial: &str,
    request: &ManagementRequest,
) -> Result<Value> {
    let (saved, value) = task_for_identity(
        tr,
        serial,
        request.namespace,
        request.epoch,
        request.operation,
    )
    .await?;
    if saved.value != request.value {
        return Err(CatalogError::Corrupt("介质回执原请求不符"));
    }
    Ok(value)
}
pub(super) async fn write_task(
    tr: &Transaction,
    request: &ManagementRequest,
    value: &Value,
) -> Result<()> {
    tr.set(
        &identity_key(request.namespace, b'o', request.epoch, request.operation),
        &encode(value),
    );
    let change = &request.value()["change"];
    let pool = if matches!(change["kind"].as_str(), Some("pool" | "pool-delete")) {
        &change["id"]
    } else {
        &change["pool"]
    };
    notifications::changed_at(tr, request.namespace(), request.value()["library"].as_str().expect("validated library"),
        &format!("task:{}:{}",request.epoch(),request.operation()), "task.status.changed",
        json!({"task_id":format!("{}:{}:{}",request.namespace(),request.epoch(),request.operation()),
            "kind":request.value()["change"]["kind"],"pool":pool,
            "barcode":request.value()["change"]["barcode"],"status":value["status"]})).await
}
pub(super) fn clear_queued(tr: &Transaction, request: &ManagementRequest) {
    tr.clear(&identity_key(
        request.namespace,
        b'q',
        request.epoch,
        request.operation,
    ));
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

async fn delete_empty_pool(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    change: &Value,
) -> Result<Value> {
    let id = id(change, "id")?;
    require_pool_library(tr, namespace, id, serial).await?;
    let mut pool = decode_pool(
        id,
        &tr.get(&pool_key(namespace, id), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    active_pools::require_indexes(tr, namespace, &pool).await?;
    if pool.revision != number(change, "revision")? {
        return Err(CatalogError::Invalid("池修订号已改变"));
    }
    match require_pool_writable_at(tr, namespace, id).await {
        Err(CatalogError::PoolImporting | CatalogError::PoolBusy) => {
            return Err(CatalogError::Invalid("池存在未完成的节点或介质任务"));
        }
        result => result?,
    }
    let root = active_pools::root_id(namespace, id);
    if requests::prefix_has_rows(tr, &child_prefix(namespace, root)).await? {
        return Err(CatalogError::Invalid("池目录非空，不能删除"));
    }
    for prefix in [
        reservation::path_prefix_for(namespace, root),
        active_batch::path_claim_prefix_for(namespace, root),
    ] {
        if requests::prefix_has_rows(tr, &prefix).await? {
            return Err(CatalogError::Invalid("池仍有活动写入目标，不能删除"));
        }
    }
    let assigned = read_page(tr, &pool_assignment_prefix(namespace, id), None, 1, false).await?;
    if let Some((key, value)) = assigned.items.first() {
        let (indexed_pool, library, barcode) = decode_namespace_assignment(namespace, key, value)?;
        let assignment = ManagedCatalog::verified_assignment(tr, &library, &barcode)
            .await?
            .ok_or(CatalogError::Corrupt("池归属索引缺少磁带主记录"))?;
        if indexed_pool != id || assignment.pool_id != id || assignment.namespace != namespace {
            return Err(CatalogError::Corrupt("池归属索引与磁带主记录不符"));
        }
        return Err(CatalogError::Invalid("池仍有磁带归属，不能删除"));
    }
    let final_revision = revision(change)?;
    let retirement = encode_pool_retirement(final_revision)?;
    // No validation or fallible work after the first mutation: Invalid is
    // recorded as a failed task in this same transaction, without rollback.
    tr.clear(&pool_key(namespace, id));
    tr.clear(&pool_name_key(namespace, &pool.name));
    tr.clear(&child_key(
        namespace,
        Uuid::nil(),
        id.to_string().as_bytes(),
    ));
    tr.clear(&node_key(namespace, root));
    tr.set(&retired_pool_key(namespace, id), &retirement);
    pool.revision = final_revision;
    Ok(json!({"pool":pool.to_json(),"deleted":true}))
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
        "pool-delete" => delete_empty_pool(tr, namespace, serial, change).await,
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
            Ok(json!({"pool":pool.to_json()}))
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
    pub async fn active_management_window(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<RequestWindow> {
        let tr = self.db.create_trx()?;
        routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        window(&tr, route.route.namespace).await
    }
    pub async fn admit_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
    ) -> Result<Value> {
        request.validate_admission()?;
        if request.value["library"] != serial || request.namespace != route.route.namespace {
            return Err(CatalogError::Invalid("管理请求与带库或命名空间不符"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                        let window = retained(&tr, request).await?;
                        let key =
                            identity_key(request.namespace, b'o', request.epoch, request.operation);
                        if let Some(raw) = tr.get(&key, false).await? {
                            let saved = task(
                                &raw,
                                serial,
                                request.namespace,
                                request.epoch,
                                request.operation,
                            )?;
                            require_queue(
                                &tr,
                                request.namespace,
                                request.epoch,
                                request.operation,
                                &saved,
                            )
                            .await?;
                            if saved["fingerprint"] != request.fingerprint() {
                                return Err(CatalogError::Invalid("管理请求身份已由不同参数使用"));
                            }
                            return Ok(saved);
                        }
                        let change = &request.value["change"];
                        let pool = match change["kind"].as_str() {
                            Some("pool" | "pool-delete") => change["id"].as_str(),
                            _ => change["pool"].as_str(),
                        };
                        if let Some(pool) = pool {
                            reclaim::require_pool_write_at(
                                &tr,
                                request.namespace,
                                Uuid::parse_str(pool)
                                    .map_err(|_| CatalogError::Invalid("管理池 UUID 无效"))?,
                            )
                            .await?;
                        }
                        if request.epoch != window.accepting {
                            return Err(CatalogError::Invalid("管理请求代次已关闭"));
                        }
                        let queued =
                            identity_key(request.namespace, b'q', request.epoch, request.operation);
                        if tr.get(&queued, false).await?.is_some() {
                            return Err(CatalogError::Corrupt("管理队列存在孤立请求"));
                        }
                        let value = json!({
                            "version": 1,
                            "library_serial": serial,
                            "namespace": request.namespace.to_string(),
                            "operation": request.operation.to_string(),
                            "epoch": request.epoch,
                            "request": request.value,
                            "fingerprint": request.fingerprint(),
                            "status": "queued",
                            "created_at_millis": now(),
                            "completed_at_millis": null,
                            "owner_epoch": 0,
                            "result": null,
                        });
                        write_task(&tr, request, &value).await?;
                        tr.set(&queued, b"");
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }

    pub async fn apply_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Value> {
        self.finish_management(serial, route, owner, epoch, operation, false)
            .await
    }
    pub async fn cancel_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Value> {
        self.finish_management(serial, route, owner, epoch, operation, true)
            .await
    }
    async fn finish_management(
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
                        let key = identity_key(route.route.namespace, b'o', epoch, operation);
                        let raw = tr
                            .get(&key, false)
                            .await?
                            .ok_or(CatalogError::Invalid("管理请求不存在"))?;
                        let mut value =
                            task(&raw, serial, route.route.namespace, epoch, operation)?;
                        require_queue(&tr, route.route.namespace, epoch, operation, &value).await?;
                        if value["status"] == "running" && cancel {
                            let request = ManagementRequest::parse(value["request"].clone())?;
                            if request.value["change"]["kind"]
                                .as_str()
                                .is_some_and(device_kind)
                            {
                                super::native_device::request_cancel(&tr, serial, route, &request)
                                    .await?;
                                return Ok(value);
                            }
                            super::native_media::request_abort(
                                &tr,
                                request.namespace,
                                id(&request.value["change"], "pool")?,
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
                        let queued = identity_key(request.namespace, b'q', epoch, operation);
                        if tr.get(&queued, false).await?.as_deref() != Some(&[]) {
                            return Err(CatalogError::Corrupt("排队管理请求索引缺失"));
                        }
                        let (status, result) = if cancel {
                            ("cancelled", json!({"cancelled":true}))
                        } else {
                            match apply_change(
                                &tr,
                                request.namespace,
                                serial,
                                &request.value["change"],
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
                        if request.value["change"]["kind"] == "tape" {
                            notifications::tape_changed_at(
                                &tr,
                                request.namespace,
                                serial,
                                text(&request.value["change"], "barcode")?,
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

    pub async fn next_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        after: Option<(u64, Uuid)>,
    ) -> Result<Option<(u64, Uuid)>> {
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        window(&tr, route.route.namespace).await?;
        let prefix = prefix(route.route.namespace, b'q');
        let after = after.map(|(epoch, id)| identity_key(route.route.namespace, b'q', epoch, id));
        let mut page = read_page(&tr, &prefix, after.as_deref(), 1, false).await?;
        if page.items.is_empty() && after.is_some() {
            page = read_page(&tr, &prefix, None, 1, false).await?;
        }
        let Some((key, value)) = page.items.first() else {
            return Ok(None);
        };
        if key.len() != prefix.len() + 24 || !value.is_empty() {
            return Err(CatalogError::Corrupt("管理队列索引编码"));
        }
        Ok(Some((
            read_u64(&key[prefix.len()..prefix.len() + 8]),
            uuid_from(&key[prefix.len() + 8..])?,
        )))
    }
    pub async fn management_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let w = window(&tr, route.route.namespace).await?;
        if epoch < w.first_retained || epoch > w.accepting {
            return Err(CatalogError::Invalid("管理请求代次不在保留窗口"));
        }
        let Some(raw) = tr
            .get(
                &identity_key(route.route.namespace, b'o', epoch, operation),
                false,
            )
            .await?
        else {
            if tr
                .get(
                    &identity_key(route.route.namespace, b'q', epoch, operation),
                    false,
                )
                .await?
                .is_some()
            {
                return Err(CatalogError::Corrupt("管理队列存在孤立请求"));
            }
            return Ok(None);
        };
        let mut value = task(&raw, serial, route.route.namespace, epoch, operation)?;
        require_queue(&tr, route.route.namespace, epoch, operation, &value).await?;
        let progress = super::native_media::task_progress(&tr, &value).await?;
        if !progress.is_null() {
            value["progress"] = progress;
        }
        Ok(Some(value))
    }

    pub async fn list_management_tasks(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<Value>> {
        let tr = self.db.create_trx()?;
        routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        window(&tr, route.route.namespace).await?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid("管理分页须在 1..=100"));
        }
        let prefix = prefix(route.route.namespace, b'o');
        if after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24) {
            return Err(CatalogError::Invalid("管理分页游标无效"));
        }
        let rows = read_page(&tr, &prefix, after, limit, true).await?;
        let mut items = Vec::new();
        for (key, raw) in rows.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("管理请求键编码"));
            }
            let epoch = read_u64(&key[prefix.len()..prefix.len() + 8]);
            let operation = uuid_from(&key[prefix.len() + 8..])?;
            let mut value = task(&raw, serial, route.route.namespace, epoch, operation)?;
            require_queue(&tr, route.route.namespace, epoch, operation, &value).await?;
            let progress = super::native_media::task_progress(&tr, &value).await?;
            if !progress.is_null() {
                value["progress"] = progress;
            }
            items.push(value);
        }
        Ok(Page {
            items,
            next: rows.next,
        })
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
    // Keep all scheduling inputs in one transaction and read version.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pool_delete_request_is_strict_and_cannot_force_data_removal() {
        let base = json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool-delete","id":Uuid::new_v4().to_string(),"revision":1}});
        assert!(ManagementRequest::parse(base.clone()).is_ok());
        for field in ["kind", "id", "revision"] {
            let mut bad = base.clone();
            bad["change"].as_object_mut().unwrap().remove(field);
            assert!(ManagementRequest::parse(bad).is_err());
        }
        for revision in [json!(0), json!(u64::MAX), json!("1")] {
            let mut bad = base.clone();
            bad["change"]["revision"] = revision;
            assert!(ManagementRequest::parse(bad).is_err());
        }
        for (field, value) in [
            ("force", json!(true)),
            ("barcode", json!("ANY000L8")),
            ("id", json!(Uuid::nil().to_string())),
        ] {
            let mut bad = base.clone();
            bad["change"][field] = value;
            assert!(ManagementRequest::parse(bad).is_err());
        }
    }

    #[test]
    fn pool_name_patch_uses_the_same_name_bounds_as_creation() {
        let ns = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let make = |name: Value| json!({"version":1,"library":"SIM","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool","id":pool.to_string(),"revision":1,"patch":{"name":name}}});
        for name in [
            "archive".into(),
            " ".into(),
            "档案".into(),
            "x".repeat(255),
            "汉".repeat(85),
        ] {
            assert!(
                ManagementRequest::parse(make(json!(name))).is_ok(),
                "{name}"
            );
        }
        for name in [
            Value::Null,
            json!(1),
            json!(true),
            json!({}),
            json!([]),
            json!(""),
            json!("/a"),
            json!("a\0b"),
            json!("x".repeat(256)),
            json!("汉".repeat(86)),
        ] {
            assert!(ManagementRequest::parse(make(name)).is_err());
        }
    }

    fn request(namespace: Uuid, serial: &str, change: Value) -> ManagementRequest {
        ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap()
    }

    #[test]
    fn tape_requests_require_complete_binding_revision_and_boolean_policy() {
        let base = json!({"kind":"tape","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"barcode":"TAPE001L8","revision":1,"patch":{"online":false,"blacklisted":true}});
        assert!(validate_change(&base).is_ok());
        for field in ["pool", "volume", "barcode", "revision", "patch"] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(validate_change(&value).is_err(), "missing {field}");
        }
        for patch in [
            json!({}),
            json!({"online":"false"}),
            json!({"recall_only":"true"}),
            json!({"offline_message":"bad\nmessage"}),
            json!({"blacklisted":0}),
            json!({"offline":true}),
            json!({"blacklist":true}),
            json!({"online":true,"enabled":true}),
            Value::Null,
        ] {
            let mut value = base.clone();
            value["patch"] = patch;
            assert!(validate_change(&value).is_err());
        }
        for revision in [0, u64::MAX] {
            let mut value = base.clone();
            value["revision"] = json!(revision);
            assert!(validate_change(&value).is_err());
        }
        for field in ["pool", "volume"] {
            let mut value = base.clone();
            value[field] = json!(Uuid::nil().to_string());
            assert!(validate_change(&value).is_err());
        }
    }

    #[test]
    fn completed_receipt_decoding_does_not_apply_current_write_admission() {
        let ns = Uuid::new_v4();
        let request = request(
            ns,
            "HISTORY",
            json!({"kind":"tape","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"barcode":"HISTORYL8","revision":1,"patch":{"online":true}}),
        );
        assert!(request.validate_admission().is_err());
        let value = json!({"version":1,"library_serial":"HISTORY","namespace":ns.to_string(),"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint(),"request":request.value(),"owner_epoch":100,"status":"succeeded","created_at_millis":1,"completed_at_millis":2,"result":{"online":true}});
        assert_eq!(
            task(
                &encode(&value),
                "HISTORY",
                ns,
                request.epoch(),
                request.operation()
            )
            .unwrap(),
            value
        );
    }

    #[test]
    fn terminal_audit_can_describe_removed_actions_without_restoring_admission() {
        let ns = Uuid::new_v4();
        let envelope = json!({"version":1,"library":"HISTORY","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"removed-action"}});
        assert!(ManagementRequest::parse(envelope.clone()).is_err());
        let decoded = ManagementRequest::decode_envelope(envelope.clone()).unwrap();
        let mut value = json!({"version":1,"library_serial":"HISTORY","namespace":ns.to_string(),"epoch":1,"operation":decoded.operation().to_string(),"fingerprint":decoded.fingerprint(),"request":envelope,"owner_epoch":100,"status":"succeeded","created_at_millis":1,"completed_at_millis":2,"result":{"completed":true}});
        assert_eq!(
            task(&encode(&value), "HISTORY", ns, 1, decoded.operation()).unwrap(),
            value
        );
        value["status"] = json!("queued");
        value["owner_epoch"] = json!(0);
        value["completed_at_millis"] = Value::Null;
        value["result"] = Value::Null;
        assert!(task(&encode(&value), "HISTORY", ns, 1, decoded.operation()).is_err());
    }

    #[test]
    fn management_request_rejects_unknown_fields_and_unsafe_patches() {
        let ns = Uuid::new_v4();
        for change in [
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"display_name":"rename"}}),
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"mount_limit":32768}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"role":"mm"}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":"false"}}),
            json!({"kind":"node","node":1,"revision":0,"group":"removed"}),
            json!({"kind":"group-create","name":"removed","id":Uuid::new_v4().to_string()}),
            json!({"kind":"group-delete","name":"removed","id":Uuid::new_v4().to_string()}),
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"node_group":"removed"}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"node_group":"removed"}}),
        ] {
            assert!(ManagementRequest::parse(json!({"version":1,"library":"SIM","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).is_err());
        }
        let good = request(
            ns,
            "SIM",
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":false}}),
        );
        let mut bad = good.value.clone();
        bad["extra"] = json!(true);
        assert!(ManagementRequest::parse(bad).is_err());
        let mut bad = good.value;
        bad["namespace"] = json!(ns.simple().to_string());
        assert!(ManagementRequest::parse(bad).is_err());
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；空池删除/退役及竞争，不操作设备"]
    async fn pool_delete_is_atomic_empty_only_and_never_reuses_an_id() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-DELETE-{ns}");
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
            name: "deletable".into(),
            file_limit: 200_000,
            created_at_millis: now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let root = active_pools::root_id(ns, pool.id);
        let deletion = |revision| {
            request(
                ns,
                &serial,
                json!({"kind":"pool-delete","id":pool.id.to_string(),"revision":revision}),
            )
        };
        let cancelled = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &cancelled)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .cancel_management(&serial, route, 100, 1, cancelled.operation())
                .await
                .unwrap()["status"],
            "cancelled"
        );
        let stale = deletion(2);
        catalog
            .admit_management(&serial, route, 100, &stale)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 100, 1, stale.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        // A real directory-shaped logical fixture, not a zero-length file or
        // a fabricated S4 receipt, exercises the independent nonempty guard.
        let child = Uuid::new_v4();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&child_key(ns, root, b"child"), child.as_bytes());
        tr.set(
            &node_key(ns, child),
            &encode_node(&Node {
                kind: NodeKind::Directory,
                directory_version: Some(1),
                parent: root,
                name: b"child".to_vec(),
                attributes: None,
            }),
        );
        tr.commit().await.unwrap();
        let nonempty = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &nonempty)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 100, 1, nonempty.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap(),
            Some(pool.clone())
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&node_key(ns, child), false).await.unwrap().is_some());
        assert!(
            tr.get(&retired_pool_key(ns, pool.id), false)
                .await
                .unwrap()
                .is_none()
        );
        tr.clear(&child_key(ns, root, b"child"));
        tr.clear(&node_key(ns, child));
        tr.commit().await.unwrap();
        let claim = reservation::path_key_for(ns, root, b"pending");
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&claim, b"pending");
        tr.commit().await.unwrap();
        let pending = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &pending)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 100, 1, pending.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&claim);
        tr.commit().await.unwrap();

        let intake = request(
            ns,
            &serial,
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"RESERVL8","volume":null,"path":"/","merge_tapes":true}),
        );
        catalog
            .admit_management(&serial, route, 100, &intake)
            .await
            .unwrap();
        let import = catalog
            .start_native_media(&serial, route, 100, 1, intake.operation())
            .await
            .unwrap()
            .unwrap();
        let busy = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &busy)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 100, 1, busy.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        let mut gate = prefix(ns, b'i');
        gate.extend_from_slice(pool.id.as_bytes());
        let tr = catalog.db.create_trx().unwrap();
        let gate_value = tr.get(&gate, false).await.unwrap().unwrap().to_vec();
        tr.clear(&gate);
        tr.commit().await.unwrap();
        // Fault injection: even a missing work gate must not turn an existing
        // pending tape assignment into an apparently unassigned pool.
        let assigned = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &assigned)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 100, 1, assigned.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap(),
            Some(pool.clone())
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&gate, &gate_value);
        tr.commit().await.unwrap();
        catalog
            .cancel_management(&serial, route, 100, 1, intake.operation())
            .await
            .unwrap();
        assert!(
            catalog
                .rollback_native_media_page(&import, 100)
                .await
                .unwrap()
        );

        let delete = deletion(1);
        catalog
            .admit_management(&serial, route, 100, &delete)
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .apply_management(&serial, route, 100, 1, delete.operation())
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        let done = catalog
            .apply_management(&serial, route, 101, 1, delete.operation())
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        assert_eq!(done["result"]["deleted"], true);
        assert_eq!(done["result"]["pool"]["revision"], 2);
        let tr = catalog.db.create_trx().unwrap();
        for key in [
            pool_key(ns, pool.id),
            pool_name_key(ns, &pool.name),
            child_key(ns, Uuid::nil(), pool.id.to_string().as_bytes()),
            node_key(ns, root),
        ] {
            assert!(tr.get(&key, false).await.unwrap().is_none());
        }
        assert_eq!(
            retired_pool_revision_at(&tr, ns, pool.id).await.unwrap(),
            Some(2)
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .admit_management(&serial, route, 101, &delete)
                .await
                .unwrap(),
            done
        );
        assert_eq!(
            reopened
                .apply_management(&serial, route, 101, 1, delete.operation())
                .await
                .unwrap(),
            done
        );
        assert_eq!(
            reopened
                .create_active_pool(&serial, route, 101, &pool)
                .await
                .unwrap(),
            ActivePoolCreation::Retired
        );
        let reused = PoolRecord {
            id: Uuid::new_v4(),
            ..pool.clone()
        };
        assert!(matches!(
            reopened
                .create_active_pool(&serial, route, 101, &reused)
                .await
                .unwrap(),
            ActivePoolCreation::Created(_)
        ));
        assert_eq!(
            reopened
                .admit_management(&serial, route, 101, &delete)
                .await
                .unwrap(),
            done
        );
        assert_eq!(
            reopened
                .active_pool_for_route(&serial, route, reused.id)
                .await
                .unwrap(),
            Some(reused)
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&retired_pool_key(ns, pool.id), b"bad");
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.create_active_pool(&serial, route, 101, &pool).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &retired_pool_key(ns, pool.id),
            &encode_pool_retirement(2).unwrap(),
        );
        tr.commit().await.unwrap();

        let race = PoolRecord {
            id: Uuid::new_v4(),
            name: "race-delete".into(),
            ..pool
        };
        catalog
            .create_active_pool(&serial, route, 101, &race)
            .await
            .unwrap();
        let delete = request(
            ns,
            &serial,
            json!({"kind":"pool-delete","id":race.id.to_string(),"revision":1}),
        );
        let intake = request(
            ns,
            &serial,
            json!({"kind":"media-import","pool":race.id.to_string(),"barcode":"RACE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        for task in [&delete, &intake] {
            catalog
                .admit_management(&serial, route, 101, task)
                .await
                .unwrap();
        }
        let (deletion, loading) = tokio::join!(
            catalog.apply_management(&serial, route, 101, 1, delete.operation()),
            catalog.start_native_media(&serial, route, 101, 1, intake.operation())
        );
        match (deletion.unwrap(), loading) {
            (value, Ok(Some(import))) if value["status"] == "failed" => {
                catalog
                    .cancel_management(&serial, route, 101, 1, intake.operation())
                    .await
                    .unwrap();
                assert!(
                    catalog
                        .rollback_native_media_page(&import, 101)
                        .await
                        .unwrap()
                );
                assert!(
                    catalog
                        .active_pool_for_route(&serial, route, race.id)
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            (value, Err(CatalogError::Invalid(_))) if value["status"] == "succeeded" => {
                catalog
                    .cancel_management(&serial, route, 101, 1, intake.operation())
                    .await
                    .unwrap();
                assert!(
                    catalog
                        .active_pool_for_route(&serial, route, race.id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            unexpected => panic!("删除与接入只能有一个成功: {unexpected:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；池名称CAS、原子索引和任务恢复，不操作设备"]
    async fn pool_rename_is_atomic_conflict_checked_and_replay_safe() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-RENAME-{ns}");
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
        let first = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "before".into(),
            file_limit: 200_000,
            created_at_millis: now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        let second = PoolRecord {
            id: Uuid::new_v4(),
            name: "occupied".into(),
            ..first.clone()
        };
        for pool in [&first, &second] {
            catalog
                .create_active_pool(&serial, route, 100, pool)
                .await
                .unwrap();
        }
        let path = pool_catalog_path(first.id, b"/").unwrap();
        let root = match catalog
            .resolve_active_metadata_for_route(&serial, &path, route)
            .await
            .unwrap()
            .unwrap()
        {
            ResolvedPath::Directory { id, .. } => id,
            _ => panic!("池根必须是目录"),
        };
        let root_key = node_key(ns, root);
        let root_before = catalog
            .db
            .create_trx()
            .unwrap()
            .get(&root_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let cancelled = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":1,"patch":{"name":"cancelled"}}),
        );
        catalog
            .admit_management(&serial, route, 100, &cancelled)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .cancel_management(&serial, route, 100, 1, cancelled.operation())
                .await
                .unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, first.id)
                .await
                .unwrap(),
            Some(first.clone())
        );
        let rename = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":1,"patch":{"name":"middle","mount_limit":2}}),
        );
        catalog
            .admit_management(&serial, route, 100, &rename)
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .apply_management(&serial, route, 100, 1, rename.operation())
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        let done = catalog
            .apply_management(&serial, route, 101, 1, rename.operation())
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        let renamed = catalog
            .active_pool_for_route(&serial, route, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "middle");
        assert_eq!(renamed.revision, 2);
        assert_eq!(renamed.created_at_millis, first.created_at_millis);
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&pool_name_key(ns, "before"), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tr.get(&pool_name_key(ns, "middle"), false)
                .await
                .unwrap()
                .as_deref(),
            Some(first.id.as_bytes().as_slice())
        );
        // Invalid changes are recorded as failed in the same transaction, so
        // every validation must precede unrelated name-index writes.
        let collision = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":2,"patch":{"name":"occupied","file_limit":17}}),
        );
        catalog
            .admit_management(&serial, route, 101, &collision)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 101, 1, collision.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, first.id)
                .await
                .unwrap(),
            Some(renamed.clone())
        );
        let stale = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":1,"patch":{"name":"stale"}}),
        );
        catalog
            .admit_management(&serial, route, 101, &stale)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 101, 1, stale.operation())
                .await
                .unwrap()["status"],
            "failed"
        );
        let final_name = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":2,"patch":{"name":"final"}}),
        );
        catalog
            .admit_management(&serial, route, 101, &final_name)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .apply_management(&serial, route, 101, 1, final_name.operation())
                .await
                .unwrap()["status"],
            "succeeded"
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .admit_management(&serial, route, 101, &rename)
                .await
                .unwrap(),
            done
        );
        assert_eq!(
            reopened
                .active_pool_for_route(&serial, route, first.id)
                .await
                .unwrap()
                .unwrap()
                .name,
            "final"
        );
        let reused = PoolRecord {
            id: Uuid::new_v4(),
            ..first.clone()
        };
        assert!(matches!(
            reopened
                .create_active_pool(&serial, route, 101, &reused)
                .await
                .unwrap(),
            ActivePoolCreation::Created(_)
        ));
        let occupied = PoolRecord {
            id: Uuid::new_v4(),
            name: "final".into(),
            ..first.clone()
        };
        assert_eq!(
            reopened
                .create_active_pool(&serial, route, 101, &occupied)
                .await
                .unwrap(),
            ActivePoolCreation::NameConflict
        );
        let orphan_key = pool_name_key(ns, "orphan");
        let orphan = request(
            ns,
            &serial,
            json!({"kind":"pool","id":first.id.to_string(),"revision":3,"patch":{"name":"orphan"}}),
        );
        catalog
            .admit_management(&serial, route, 101, &orphan)
            .await
            .unwrap();
        for value in [
            b"bad".to_vec(),
            Uuid::nil().as_bytes().to_vec(),
            first.id.as_bytes().to_vec(),
            Uuid::new_v4().as_bytes().to_vec(),
            second.id.as_bytes().to_vec(),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&orphan_key, &value);
            tr.commit().await.unwrap();
            assert!(matches!(
                catalog
                    .apply_management(&serial, route, 101, 1, orphan.operation())
                    .await,
                Err(CatalogError::Corrupt(_))
            ));
            assert_eq!(
                catalog
                    .active_pool_for_route(&serial, route, first.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .name,
                "final"
            );
        }
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&orphan_key);
        tr.commit().await.unwrap();
        catalog
            .cancel_management(&serial, route, 101, 1, orphan.operation())
            .await
            .unwrap();
        let a = PoolRecord {
            id: Uuid::new_v4(),
            name: "race-a".into(),
            ..first.clone()
        };
        let b = PoolRecord {
            id: Uuid::new_v4(),
            name: "race-b".into(),
            ..first.clone()
        };
        for pool in [&a, &b] {
            catalog
                .create_active_pool(&serial, route, 101, pool)
                .await
                .unwrap();
        }
        let changes = [&a, &b].map(|pool| request(ns, &serial, json!({"kind":"pool","id":pool.id.to_string(),"revision":1,"patch":{"name":"race-winner"}})));
        for change in &changes {
            catalog
                .admit_management(&serial, route, 101, change)
                .await
                .unwrap();
        }
        let (left, right) = tokio::join!(
            catalog.apply_management(&serial, route, 101, 1, changes[0].operation()),
            catalog.apply_management(&serial, route, 101, 1, changes[1].operation())
        );
        let results = [left.unwrap(), right.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|v| v["status"] == "succeeded")
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|v| v["status"] == "failed").count(),
            1
        );
        for (pool, result) in [&a, &b].into_iter().zip(results) {
            let stored = catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .unwrap();
            if result["status"] == "succeeded" {
                assert_eq!(stored.name, "race-winner");
                assert_eq!(stored.name, "race-winner");
            } else {
                assert_eq!(stored, *pool);
            }
        }
        assert_eq!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&root_key, false)
                .await
                .unwrap()
                .as_deref(),
            Some(root_before.as_slice())
        );
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER，不操作设备"]
    async fn management_tasks_are_atomic_fenced_cancelled_and_durable() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-MGMT-{ns}");
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
            name: "native".into(),
            file_limit: 200_000,
            created_at_millis: now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let change = request(
            ns,
            &serial,
            json!({"kind":"pool","id":pool.id.to_string(),"revision":1,"patch":{"mount_limit":1,"file_limit":17}}),
        );
        let queued = catalog
            .admit_management(&serial, route, 100, &change)
            .await
            .unwrap();
        assert_eq!(queued["status"], "queued");
        assert_eq!(
            catalog
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            catalog
                .admit_management(&serial, route, 100, &change)
                .await
                .unwrap(),
            queued
        );
        let mut different = change.value.clone();
        different["change"]["patch"]["mount_limit"] = json!(2);
        assert!(
            catalog
                .admit_management(
                    &serial,
                    route,
                    100,
                    &ManagementRequest::parse(different).unwrap()
                )
                .await
                .is_err()
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        reopened
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .apply_management(&serial, route, 100, 1, change.operation)
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        assert_eq!(
            reopened
                .next_management(&serial, route, 101, None)
                .await
                .unwrap(),
            Some((1, change.operation))
        );
        let done = reopened
            .apply_management(&serial, route, 101, 1, change.operation)
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        assert_eq!(done["owner_epoch"], 101);
        assert_eq!(
            reopened
                .admit_management(&serial, route, 101, &change)
                .await
                .unwrap(),
            done
        );
        assert!(
            reopened
                .next_management(&serial, route, 101, None)
                .await
                .unwrap()
                .is_none()
        );
        let saved = reopened
            .active_pool_for_route(&serial, route, pool.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                saved.revision,
                saved.file_limit,
                saved.policy.mount_limit,
                saved.created_at_millis
            ),
            (2, 17, 1, pool.created_at_millis)
        );
        let stale = request(ns, &serial, change.value["change"].clone());
        reopened
            .admit_management(&serial, route, 101, &stale)
            .await
            .unwrap();
        let failure = reopened
            .apply_management(&serial, route, 101, 1, stale.operation)
            .await
            .unwrap();
        assert_eq!(failure["status"], "failed");
        assert_eq!(
            reopened
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .unwrap(),
            saved
        );
        let cancelled = request(
            ns,
            &serial,
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":false}}),
        );
        reopened
            .admit_management(&serial, route, 101, &cancelled)
            .await
            .unwrap();
        let cancelled_result = reopened
            .cancel_management(&serial, route, 101, 1, cancelled.operation)
            .await
            .unwrap();
        assert_eq!(cancelled_result["status"], "cancelled");
        assert_eq!(
            reopened
                .apply_management(&serial, route, 101, 1, cancelled.operation)
                .await
                .unwrap(),
            cancelled_result
        );
        assert_eq!(
            reopened
                .active_resource_policy(&serial, route, "drive", "DRV")
                .await
                .unwrap()["revision"],
            0
        );
        let raced = request(
            ns,
            &serial,
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":false}}),
        );
        reopened
            .admit_management(&serial, route, 101, &raced)
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            reopened.apply_management(&serial, route, 101, 1, raced.operation),
            reopened.cancel_management(&serial, route, 101, 1, raced.operation)
        );
        let a = a.unwrap();
        assert_eq!(a, b.unwrap());
        assert!(matches!(
            a["status"].as_str(),
            Some("succeeded" | "cancelled")
        ));
        assert_eq!(
            reopened
                .active_resource_policy(&serial, route, "drive", "DRV")
                .await
                .unwrap()["revision"],
            if a["status"] == "succeeded" { 1 } else { 0 }
        );

        let page = reopened
            .list_management_tasks(&serial, route, None, 1)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.next.is_some());
        assert!(
            reopened
                .list_management_tasks(&serial, route, Some(b"other namespace"), 1)
                .await
                .is_err()
        );

        // Invalid persisted receipts fail closed and cannot partially apply a patch.
        let corrupt = request(
            ns,
            &serial,
            json!({"kind":"pool","id":pool.id.to_string(),"revision":2,"patch":{"mount_limit":2}}),
        );
        let mut value = reopened
            .admit_management(&serial, route, 101, &corrupt)
            .await
            .unwrap();
        value["extra"] = json!(true);
        let tr = reopened.db.create_trx().unwrap();
        tr.set(
            &identity_key(ns, b'o', 1, corrupt.operation),
            &encode(&value),
        );
        tr.commit().await.unwrap();
        assert!(matches!(
            reopened
                .apply_management(&serial, route, 101, 1, corrupt.operation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert_eq!(
            reopened
                .active_pool_for_route(&serial, route, pool.id)
                .await
                .unwrap()
                .unwrap(),
            saved
        );
    }
}
