//! Durable, fenced physical maintenance. Device intentions survive ownership changes;
//! terminal results retain the last observation, never a fictitious media rollback.
use super::*;
use serde_json::{Value, json};

const MAX_STATE: usize = 48_000;

#[derive(Clone, Debug)]
pub struct NativeDeviceTask {
    request: ManagementRequest,
    route: ActiveLibraryRoute,
}
impl NativeDeviceTask {
    pub fn request(&self) -> &ManagementRequest {
        &self.request
    }
    pub fn route(&self) -> ActiveLibraryRoute {
        self.route
    }
    pub fn pool(&self) -> Option<Uuid> {
        self.request.value()["change"]["pool"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
    }
    pub fn barcode(&self) -> Option<&str> {
        self.request.value()["change"]["barcode"].as_str()
    }
    fn serial(&self) -> &str {
        self.request.value()["library"]
            .as_str()
            .expect("validated library")
    }
    fn kind(&self) -> &str {
        self.request.value()["change"]["kind"]
            .as_str()
            .expect("validated kind")
    }
    fn journal(&self) -> Vec<u8> {
        let mut key = domain(self.request.namespace(), b't');
        append_component(&mut key, self.serial().as_bytes());
        key.extend_from_slice(&self.request.epoch().to_be_bytes());
        key.extend_from_slice(self.request.operation().as_bytes());
        key
    }
    fn token(&self) -> Vec<u8> {
        encoded(
            &json!({"version":1,"epoch":self.request.epoch(),"operation":self.request.operation().to_string(),"fingerprint":self.request.fingerprint()}),
        )
    }
    fn gate(&self) -> Vec<u8> {
        library_gate(self.request.namespace(), self.serial())
    }
    fn pool_gate(&self) -> Option<Vec<u8>> {
        self.pool()
            .map(|pool| pool_gate(self.request.namespace(), pool))
    }
}

fn domain(ns: Uuid, kind: u8) -> Vec<u8> {
    let mut out = management::prefix(ns, b'K');
    out.push(kind);
    out
}
fn pool_gate(ns: Uuid, pool: Uuid) -> Vec<u8> {
    let mut out = domain(ns, b'p');
    out.extend_from_slice(pool.as_bytes());
    out
}
pub(super) fn library_gate(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut out = domain(ns, b'l');
    append_component(&mut out, serial.as_bytes());
    out
}
pub(super) fn mechanical_token(request: &ManagementRequest) -> Vec<u8> {
    encoded(
        &json!({"version":1,"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()}),
    )
}
pub(super) async fn require_import_gate_at(
    tr: &Transaction,
    import: &NativeMediaImport,
) -> Result<()> {
    let request = import.request();
    let serial = request.value()["library"].as_str().expect("library");
    if tr
        .get(&library_gate(request.namespace(), serial), false)
        .await?
        .as_deref()
        != Some(mechanical_token(request).as_slice())
    {
        return Err(CatalogError::Corrupt("介质导入原任务库屏障缺失或身份不符"));
    }
    Ok(())
}
pub(super) async fn acquire_import_gate_at(
    tr: &Transaction,
    import: &NativeMediaImport,
) -> Result<()> {
    let request = import.request();
    let serial = request.value()["library"].as_str().expect("library");
    require_library_available(tr, request.namespace(), serial).await?;
    tr.set(
        &library_gate(request.namespace(), serial),
        &mechanical_token(request),
    );
    Ok(())
}
pub(super) fn clear_import_gate_at(tr: &Transaction, import: &NativeMediaImport) {
    let request = import.request();
    tr.clear(&library_gate(
        request.namespace(),
        request.value()["library"].as_str().expect("library"),
    ));
}
fn home_key(ns: Uuid, serial: &str, barcode: &str) -> Vec<u8> {
    let mut out = domain(ns, b'h');
    append_component(&mut out, serial.as_bytes());
    append_component(&mut out, barcode.as_bytes());
    out
}
fn intake_home_key(import: &NativeMediaImport) -> Vec<u8> {
    let mut key = domain(import.request().namespace(), b'i');
    append_component(
        &mut key,
        import.request().value()["library"]
            .as_str()
            .expect("library")
            .as_bytes(),
    );
    key.extend_from_slice(&import.request().epoch().to_be_bytes());
    key.extend_from_slice(import.request().operation().as_bytes());
    key
}
pub(super) async fn stage_intake_home_at(
    tr: &Transaction,
    import: &NativeMediaImport,
    home: u16,
) -> Result<()> {
    let key = intake_home_key(import);
    if let Some(old) = tr.get(&key, false).await?
        && old.as_ref() != home.to_be_bytes()
    {
        return Err(CatalogError::Invalid("原导入任务的装载源槽位已改变"));
    }
    tr.set(&key, &home.to_be_bytes());
    Ok(())
}
pub(super) async fn publish_intake_home_at(
    tr: &Transaction,
    import: &NativeMediaImport,
    volume: Uuid,
) -> Result<()> {
    // Called in the publication transaction, after the medium UUID and source
    // chain have been proved. A transaction failure must roll publication back.
    let result = async {
        super::native_media::load::require_settled_at(tr, import).await?;
        let key = intake_home_key(import);
        let Some(raw) = tr.get(&key, false).await? else {
            super::native_media::load::clear_at(tr, import);
            return Ok(());
        };
        let bytes: [u8; 2] = raw.as_ref().try_into()
            .map_err(|_| CatalogError::Corrupt("导入装载源槽位编码"))?;
        let request = import.request();
        let value = json!({"version":1,"namespace":request.namespace().to_string(),"library":request.value()["library"],"barcode":import.barcode(),"pool":import.pool().to_string(),"volume":volume.to_string(),"home":u16::from_be_bytes(bytes),"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()});
        tr.set(&home_key(request.namespace(), request.value()["library"].as_str().expect("validated library"), import.barcode()), &encoded(&value));
        tr.clear(&key);
        super::native_media::load::clear_at(tr, import);
        Ok(())
    }.await;
    result.map_err(notifications::atomic_error)
}
pub(super) fn clear_intake_home_at(tr: &Transaction, import: &NativeMediaImport) {
    tr.clear(&intake_home_key(import));
    super::native_media::load::clear_at(tr, import);
}
fn record_home_at(tr: &Transaction, task: &NativeDeviceTask, state: &Value) {
    // The immutable plan and successful physical postcondition were verified
    // before the caller's first mutation. This records intent-derived evidence,
    // never an invented SMC source_address or permission to use an occupied slot.
    let value = json!({"version":1,"namespace":task.request.namespace().to_string(),"library":task.serial(),"barcode":task.barcode().expect("media task barcode"),"pool":task.request.value()["change"]["pool"],"volume":task.request.value()["change"]["volume"],"home":state["plan"]["home"],"epoch":task.request.epoch(),"operation":task.request.operation().to_string(),"fingerprint":task.request.fingerprint()});
    tr.set(
        &home_key(
            task.request.namespace(),
            task.serial(),
            task.barcode().expect("media barcode"),
        ),
        &encoded(&value),
    );
}
pub(super) fn record_native_assign_home_at(
    tr: &Transaction,
    request: &ManagementRequest,
    volume: Uuid,
    home: u16,
) {
    let serial = request.value()["library"].as_str().expect("library");
    let barcode = request.value()["change"]["barcode"]
        .as_str()
        .expect("barcode");
    let value = json!({"version":1,"namespace":request.namespace().to_string(),"library":serial,"barcode":barcode,"pool":request.value()["change"]["pool"],"volume":volume.to_string(),"home":home,"epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()});
    tr.set(
        &home_key(request.namespace(), serial, barcode),
        &encoded(&value),
    );
}

fn encoded(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}
fn new_state(task: &NativeDeviceTask) -> Value {
    json!({"version":1,"fingerprint":task.request.fingerprint(),"phase":"ready","plan":null,"observation":null,"cancel_requested":false,"media_may_have_changed":false,"last_error":null})
}
fn validate_state(value: &Value, task: &NativeDeviceTask) -> Result<()> {
    let keys = [
        "version",
        "fingerprint",
        "phase",
        "plan",
        "observation",
        "cancel_requested",
        "media_may_have_changed",
        "last_error",
    ];
    if encoded(value).len() > MAX_STATE
        || value["version"] != 1
        || value["fingerprint"] != task.request.fingerprint()
        || !value
            .as_object()
            .is_some_and(|m| m.len() == keys.len() && m.keys().all(|k| keys.contains(&k.as_str())))
        || !matches!(
            value["phase"].as_str(),
            Some(
                "ready"
                    | "planned"
                    | "load-issued"
                    | "loaded"
                    | "move-issued"
                    | "move-uncertain"
                    | "verified"
            )
        )
        || !value["cancel_requested"].is_boolean()
        || !value["media_may_have_changed"].is_boolean()
        || !(value["observation"].is_null() || value["observation"].is_object())
        || !(value["last_error"].is_null()
            || value["last_error"]
                .as_str()
                .is_some_and(|s| s.len() <= 8192))
        || ((value["phase"] == "ready") != value["plan"].is_null())
    {
        return Err(CatalogError::Corrupt("设备任务检查点格式或原请求不符"));
    }
    let valid_phase = match value["phase"].as_str().expect("validated phase") {
        "ready" | "planned" => true,
        "load-issued" | "loaded" => matches!(task.kind(), "media-validate" | "media-online"),
        "move-issued" | "move-uncertain" => matches!(task.kind(), "media-move" | "media-unassign"),
        "verified" => true,
        _ => false,
    };
    if !valid_phase {
        return Err(CatalogError::Corrupt("设备任务阶段与 kind 不符"));
    }
    if !value["plan"].is_null() {
        validate_plan(task, &value["plan"])?;
    }
    if !value["observation"].is_null() {
        validate_observation(
            task,
            &value["observation"],
            value["phase"].as_str().expect("validated phase"),
        )?;
    }
    Ok(())
}
fn validate_plan(task: &NativeDeviceTask, plan: &Value) -> Result<()> {
    let keys = [
        "version",
        "barcode",
        "source",
        "destination",
        "home",
        "drive_serial",
        "volume",
        "close_may_write",
    ];
    if encoded(plan).len() > 8192
        || plan["version"] != 1
        || !plan
            .as_object()
            .is_some_and(|m| m.len() == keys.len() && m.keys().all(|k| keys.contains(&k.as_str())))
        || !plan["close_may_write"].is_boolean()
    {
        return Err(CatalogError::Invalid("设备任务计划字段或大小无效"));
    }
    for key in ["source", "destination", "home"] {
        if !(plan[key].is_null() || plan[key].as_u64().is_some_and(|n| n <= u16::MAX as u64)) {
            return Err(CatalogError::Invalid("设备计划地址无效"));
        }
    }
    if !(plan["drive_serial"].is_null()
        || plan["drive_serial"]
            .as_str()
            .is_some_and(|s| validate_physical_serial(s).is_ok()))
    {
        return Err(CatalogError::Invalid("设备计划驱动序列号无效"));
    }
    if task.kind() == "library-rescan" {
        if keys[1..7].iter().any(|key| !plan[*key].is_null()) || plan["close_may_write"] != false {
            return Err(CatalogError::Invalid("rescan 不接受介质移动或写入计划"));
        }
        return Ok(());
    }
    if plan["barcode"]
        != task
            .barcode()
            .ok_or(CatalogError::Invalid("设备计划缺少条码"))?
        || plan["volume"] != task.request.value()["change"]["volume"]
        || ["source", "destination", "home"]
            .iter()
            .any(|key| plan[*key].is_null())
    {
        return Err(CatalogError::Invalid("设备计划条码、卷或地址与原请求不符"));
    }
    if matches!(task.kind(), "media-validate" | "media-online") {
        if plan["drive_serial"].is_null() || plan["close_may_write"] != false {
            return Err(CatalogError::Invalid(
                "装载计划必须指定驱动且不能混入退带写入",
            ));
        }
    } else if plan["close_may_write"].as_bool()
        != Some(!plan["drive_serial"].is_null() && task.pool().is_some())
    {
        return Err(CatalogError::Invalid("退带计划写入标记与源驱动不符"));
    }
    if (task.kind() == "media-unassign"
        || (task.kind() == "media-move"
            && task.request.value()["change"]["destination"] == "homeslot"))
        && plan["destination"] != plan["home"]
    {
        return Err(CatalogError::Invalid("归槽计划必须使用固定 storage 地址"));
    }
    Ok(())
}
fn validate_observation(task: &NativeDeviceTask, value: &Value, phase: &str) -> Result<()> {
    let keys = ["inventory", "diagnostic", "validation"];
    if encoded(value).len() > 32_000
        || !value
            .as_object()
            .is_some_and(|m| m.len() == keys.len() && m.keys().all(|k| keys.contains(&k.as_str())))
        || !(value["inventory"].is_null() || value["inventory"].is_object())
        || !(value["diagnostic"].is_null()
            || value["diagnostic"]
                .as_str()
                .is_some_and(|s| s.len() <= 8192))
    {
        return Err(CatalogError::Invalid("设备观察字段、诊断或大小无效"));
    }
    let validation = &value["validation"];
    if !validation.is_null() {
        let fields = [
            "volume",
            "generation",
            "index_sha256",
            "files",
            "scope",
            "payload_verified",
        ];

        if !validation.as_object().is_some_and(|m| {
            m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str()))
        }) || validation["volume"] != task.request.value()["change"]["volume"]
            || !validation["generation"].as_u64().is_some_and(|n| n > 0)
            || !validation["index_sha256"].as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
            || validation["files"].as_u64().is_none()
            || validation["scope"] != "label_and_index"
            || validation["payload_verified"] != false
        {
            return Err(CatalogError::Invalid("卷验证摘要的身份、范围或字段无效"));
        }
    }
    if phase == "verified"
        && matches!(task.kind(), "media-validate" | "media-online")
        && (validation.is_null() || value["diagnostic"].is_string())
    {
        return Err(CatalogError::Invalid("验证完成必须具备成功的介质摘要"));
    }
    Ok(())
}
async fn read_state(tr: &Transaction, task: &NativeDeviceTask, owner: u64) -> Result<Value> {
    routing::require_runtime_active_owner(tr, task.serial(), task.route, owner).await?;
    let receipt = management::read_task(tr, task.serial(), &task.request).await?;
    if receipt["status"] != "running" || receipt["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if tr.get(&task.gate(), false).await?.as_deref() != Some(task.token().as_slice()) {
        return Err(CatalogError::Corrupt("设备任务维护屏障缺失或身份不符"));
    }
    if let Some(key) = task.pool_gate()
        && tr.get(&key, false).await?.as_deref() != Some(task.token().as_slice())
    {
        return Err(CatalogError::Corrupt("设备任务池屏障缺失或身份不符"));
    }

    let raw = tr
        .get(&task.journal(), false)
        .await?
        .ok_or(CatalogError::Corrupt("设备任务检查点缺失"))?;
    let value: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("设备任务检查点 JSON"))?;
    if encoded(&value) != raw.as_ref() {
        return Err(CatalogError::Corrupt("设备任务检查点编码不规范"));
    }
    validate_state(&value, task)?;
    Ok(value)
}
/// Called by all existing pool admission/lookup barriers; no maintenance state
/// is inserted into the unrelated native-import gate format.
pub(super) async fn require_pool_available(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<()> {
    if tr.get(&pool_gate(ns, pool), false).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
pub(super) async fn require_library_available(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<()> {
    if tr.get(&library_gate(ns, serial), false).await?.is_some()
        || super::automatic_media::pending_at(tr, ns, serial)
            .await?
            .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

/// Pure pool metadata may progress beside another pool's mechanical task.
/// Verify the original Kl record; malformed gates never grant an exception.
pub(super) async fn require_pool_metadata_available(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
) -> Result<()> {
    if let Some(task) = management::moves::pending_at(tr, ns, serial).await? {
        let change = &task["request"]["change"];
        if change["pool"] == pool.to_string() {
            return Err(CatalogError::PoolBusy);
        }
    }
    native_media::require_pool_readable(tr, ns, pool).await?;
    if node_tasks::held_task_at(tr, ns, pool).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    reclaim::require_pool_write_at(tr, ns, pool).await
}
async fn require_assignment(
    tr: &Transaction,
    task: &NativeDeviceTask,
) -> Result<Option<TapeAssignment>> {
    let Some(barcode) = task.barcode() else {
        return Ok(None);
    };
    let assignment = ManagedCatalog::verified_assignment(tr, task.serial(), barcode).await?;
    let change = &task.request.value()["change"];
    if let Some(pool) = task.pool() {
        require_pool_library(tr, task.request.namespace(), pool, task.serial()).await?;
        let a = assignment.ok_or(CatalogError::Invalid("磁带已无池归属"))?;
        let volume = change["volume"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok());
        if a.namespace != task.request.namespace()
            || a.pool_id != pool
            || a.volume_id != volume
            || change["revision"] != a.revision
        {
            return Err(CatalogError::Invalid("磁带归属、卷或策略修订号已改变"));
        }
        if (task.kind() == "media-validate" && !a.online)
            || (matches!(task.kind(), "media-validate" | "media-online") && a.blacklisted)
        {
            return Err(CatalogError::Invalid("validate 要求磁带在线且未列入黑名单"));
        }
        if task.kind() == "media-move" && change["destination"] == "ieslot" && a.online {
            return Err(CatalogError::Invalid("移入 I/E 前必须先离线磁带"));
        }
        Ok(Some(a))
    } else if assignment.is_some()
        || tr
            .get(&barcode_key(task.serial(), barcode), false)
            .await?
            .is_some()
    {
        Err(CatalogError::Invalid(
            "原请求指定未分配磁带，但当前已有归属",
        ))
    } else {
        Ok(None)
    }
}
async fn require_unreferenced(
    tr: &Transaction,
    task: &NativeDeviceTask,
    volume: Uuid,
) -> Result<()> {
    require_volume_unreferenced_at(tr, task.request.namespace(), volume)
        .await
        .map_err(|error| match error {
            CatalogError::PoolBusy => {
                CatalogError::Invalid("磁带存在在途写入、恢复、协调或节点操作占用，不能解除分配")
            }
            other => other,
        })
}

pub(in crate::managed_catalog) async fn require_volume_unreferenced_at(
    tr: &Transaction,
    namespace: Uuid,
    volume: Uuid,
) -> Result<()> {
    // V is global across every scan and namespace; retained history is a live
    // dependency until explicitly reclaimed. Each proof is a bounded probe.
    let mut prefix = key(b'V');
    prefix.extend_from_slice(volume.as_bytes());
    if requests::prefix_has_rows(tr, &prefix).await? {
        return Err(CatalogError::Invalid(
            "磁带仍有保留副本或在途写入引用，不能解除分配",
        ));
    }
    require_volume_quiescent_at(tr, namespace, volume).await
}

pub(in crate::managed_catalog) async fn require_volume_quiescent_at(
    tr: &Transaction,
    namespace: Uuid,
    volume: Uuid,
) -> Result<()> {
    for kind in *b"24" {
        let mut prefix = key(kind);
        prefix.extend_from_slice(volume.as_bytes());
        if requests::prefix_has_rows(tr, &prefix).await? {
            return Err(CatalogError::PoolBusy);
        }
    }
    if requests::prefix_has_rows(tr, &active_batch::reverse_claim_prefix_for(volume)).await?
        || active_volume::recovery_active(tr, volume).await?
        || tr
            .get(&reconcile_volume_key(volume), false)
            .await?
            .is_some()
        || tr
            .get(&native_media::volume_claim_key(volume), false)
            .await?
            .is_some()
        || tr
            .get(&node_attempts::volume_key(volume), false)
            .await?
            .is_some()
        || requests::has_namespace_holds(tr, namespace).await?
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
async fn verify_validation_at(
    tr: &Transaction,
    task: &NativeDeviceTask,
    observation: &Value,
) -> Result<()> {
    let assignment = require_assignment(tr, task)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let volume = assignment.volume_id.ok_or(CatalogError::Incomplete)?;
    let (_, head) = active_volume::observed_at(tr, task.serial(), volume).await?;
    let sha = head
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if observation["validation"]["generation"] != head.generation
        || observation["validation"]["index_sha256"] != sha
    {
        return Err(CatalogError::Invalid(
            "validate 读回索引与 FDB 当前卷 head 不一致",
        ));
    }
    Ok(())
}

fn transition(kind: &str, from: &str, to: &str) -> bool {
    match (from, to) {
        ("ready", "planned") => true,
        ("planned", "load-issued") => {
            matches!(kind, "media-validate" | "media-online")
        }
        ("load-issued", "loaded") => {
            matches!(kind, "media-validate" | "media-online")
        }
        ("planned", "move-issued") => matches!(kind, "media-move" | "media-unassign"),
        ("move-issued" | "move-uncertain", "move-uncertain" | "verified") => {
            matches!(kind, "media-move" | "media-unassign")
        }
        ("planned", "verified") => {
            matches!(kind, "library-rescan" | "media-move" | "media-unassign")
        }
        ("loaded", "verified") => matches!(kind, "media-validate" | "media-online"),
        _ => false,
    }
}
impl ManagedCatalog {
    /// A prior successful plan is only a home-address proof. The owner must
    /// still verify the current address map, barcode and destination vacancy.
    /// expected_volume describes the current assignment; provisional assign
    /// reservations pass None, not the new UUID that has yet to be published.
    pub async fn native_device_home(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        barcode: &str,
        expected_volume: Option<Uuid>,
    ) -> Result<Option<u16>> {
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let assignment = Self::verified_assignment(&tr, serial, barcode).await?;
        if assignment
            .as_ref()
            .is_some_and(|a| a.namespace != ns || a.volume_id != expected_volume)
            || (assignment.is_none() && expected_volume.is_some())
        {
            return Err(CatalogError::Invalid(
                "home 查询的卷或 namespace 与当前归属不符",
            ));
        }
        let Some(raw) = tr.get(&home_key(ns, serial, barcode), false).await? else {
            return Ok(None);
        };
        let value: Value =
            serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("持久 home 编码"))?;
        let fields = [
            "version",
            "namespace",
            "library",
            "barcode",
            "pool",
            "volume",
            "home",
            "epoch",
            "operation",
            "fingerprint",
        ];
        let uuid = |v: &Value| {
            v.as_str().and_then(|s| {
                Uuid::parse_str(s)
                    .ok()
                    .filter(|id| !id.is_nil() && id.to_string() == s)
            })
        };
        if encoded(&value) != raw.as_ref()
            || raw.len() > 2048
            || value["version"] != 1
            || value["namespace"] != ns.to_string()
            || value["library"] != serial
            || value["barcode"] != barcode
            || !value.as_object().is_some_and(|m| {
                m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str()))
            })
            || !value["home"].as_u64().is_some_and(|n| n <= u16::MAX as u64)
            || !value["epoch"].as_u64().is_some_and(|n| n > 0)
            || uuid(&value["operation"]).is_none()
            || !value["fingerprint"].as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            || (value["pool"].is_null() != value["volume"].is_null())
            || (!value["pool"].is_null()
                && (uuid(&value["pool"]).is_none() || uuid(&value["volume"]).is_none()))
        {
            return Err(CatalogError::Corrupt("持久 home 的字段或介质身份不符"));
        }
        if value["volume"] != json!(expected_volume.map(|v| v.to_string())) {
            return Ok(None);
        }
        if let Some(a) = assignment
            && a.volume_id.is_some()
            && value["pool"] != a.pool_id.to_string()
        {
            return Ok(None);
        }
        Ok(value["home"].as_u64().map(|n| n as u16))
    }
    pub async fn start_native_device_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NativeDeviceTask>> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                        let (request, mut receipt) = management::task_for_identity(
                            &tr,
                            serial,
                            route.route.namespace,
                            epoch,
                            operation,
                        )
                        .await?;
                        let task = NativeDeviceTask { request, route };
                        if !management::device_kind(task.kind()) || task.kind() == "media-assign" {
                            return Err(CatalogError::Invalid("不是通用原生设备任务"));
                        }
                        if !matches!(receipt["status"].as_str(), Some("queued" | "running")) {
                            return Ok(None);
                        }
                        if receipt["status"] == "queued" {
                            let ns = task.request.namespace();
                            require_library_available(&tr, ns, serial).await?;
                            if let Some(pool) = task.pool() {
                                reclaim::require_pool_write_at(&tr, ns, pool).await?;
                                native_media::require_pool_readable(&tr, ns, pool).await?;
                            } else {
                                for prefix in [domain(ns, b'p'), management::prefix(ns, b'i')] {
                                    if requests::prefix_has_rows(&tr, &prefix).await? {
                                        return Err(CatalogError::PoolBusy);
                                    }
                                }
                            }
                            if requests::has_namespace_holds(&tr, ns).await? {
                                return Err(CatalogError::PoolBusy);
                            }
                            let assigned = require_assignment(&tr, &task).await?;
                            if task.kind() == "media-unassign" {
                                require_unreferenced(
                                    &tr,
                                    &task,
                                    assigned
                                        .and_then(|a| a.volume_id)
                                        .ok_or(CatalogError::Incomplete)?,
                                )
                                .await?;
                            }
                            tr.set(&task.gate(), &task.token());
                            if let Some(key) = task.pool_gate() {
                                tr.set(&key, &task.token());
                            }
                            tr.set(&task.journal(), &encoded(&new_state(&task)));
                        } else {
                            let old_owner = receipt["owner_epoch"]
                                .as_u64()
                                .ok_or(CatalogError::Corrupt("设备任务 owner"))?;
                            if let Some(key) = task.pool_gate()
                                && tr.get(&key, false).await?.as_deref()
                                    != Some(task.token().as_slice())
                            {
                                return Err(CatalogError::Corrupt("接任设备任务原池屏障不符"));
                            }
                            // Read identity without accepting the predecessor as runtime owner.
                            if tr.get(&task.gate(), false).await?.as_deref()
                                != Some(task.token().as_slice())
                            {
                                return Err(CatalogError::Corrupt("接任设备任务屏障不符"));
                            }
                            let raw = tr
                                .get(&task.journal(), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("接任设备检查点缺失"))?;
                            let value: Value = serde_json::from_slice(&raw)
                                .map_err(|_| CatalogError::Corrupt("接任设备检查点 JSON"))?;
                            validate_state(&value, &task)?;
                            if old_owner == 0 || old_owner > owner {
                                return Err(CatalogError::Corrupt("设备任务旧owner为零"));
                            }
                        }
                        receipt["status"] = json!("running");
                        receipt["owner_epoch"] = json!(owner);
                        management::write_task(&tr, &task.request, &receipt).await?;
                        if let Some(barcode) = task.barcode() {
                            notifications::tape_changed_at(
                                &tr,
                                task.request.namespace(),
                                task.serial(),
                                barcode,
                                "device_task_started",
                            )
                            .await?;
                        }
                        Ok(Some(task))
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
    pub async fn native_device_status(&self, task: &NativeDeviceTask, owner: u64) -> Result<Value> {
        read_state(&self.db.create_trx()?, task, owner).await
    }

    /// FDB-only synchronous probe for a blocking SCSI reader. Keep this path
    /// independent of Tokio reactors/timers; it runs on the device actor thread.
    pub(crate) fn native_device_status_blocking(
        &self,
        task: &NativeDeviceTask,
        owner: u64,
    ) -> Result<Value> {
        let tr = self.db.create_trx()?;
        tr.set_option(foundationdb::options::TransactionOption::Timeout(2_000))?;
        let state = futures_executor::block_on(Box::pin(read_state(&tr, task, owner)))?;
        Ok(state)
    }

    #[cfg(test)]
    pub(crate) fn native_device_cancelled_blocking(
        &self,
        task: &NativeDeviceTask,
        owner: u64,
    ) -> Result<bool> {
        Ok(self.native_device_status_blocking(task, owner)?["cancel_requested"] == true)
    }

    pub async fn checkpoint_native_device_task(
        &self,
        task: &NativeDeviceTask,
        owner: u64,
        expected_phase: &str,
        next_phase: &str,
        plan: &Value,
        observation: Option<&Value>,
    ) -> Result<Value> {
        validate_plan(task, plan)?;
        if let Some(observation) = observation {
            validate_observation(task, observation, next_phase)?;
        } else if next_phase == "verified" {
            return Err(CatalogError::Invalid("设备完成检查点缺少实际观察"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let mut value = read_state(&tr, task, owner).await?;
                        if matches!(task.kind(), "media-validate" | "media-online")
                            && next_phase == "verified"
                        {
                            verify_validation_at(
                                &tr,
                                task,
                                observation.ok_or(CatalogError::Incomplete)?,
                            )
                            .await?;
                        }
                        if !value["plan"].is_null() && value["plan"] != *plan {
                            return Err(CatalogError::Invalid(
                                "设备计划已固定，不允许替换目标或身份",
                            ));
                        }
                        let phase = value["phase"].as_str().expect("validated phase");
                        if phase == next_phase && value["plan"] == *plan {
                            if value["observation"] == observation.cloned().unwrap_or(Value::Null) {
                                return Ok(value);
                            }
                            if phase != "move-uncertain" {
                                return Err(CatalogError::Invalid("已完成检查点的观察不可替换"));
                            }
                        }
                        if phase != expected_phase || !transition(task.kind(), phase, next_phase) {
                            return Err(CatalogError::Invalid("设备检查点阶段已改变或转换不合法"));
                        }
                        if value["cancel_requested"] == true
                            && matches!(next_phase, "load-issued" | "move-issued")
                        {
                            return Err(CatalogError::Invalid("已取消任务不能开始新的设备操作"));
                        }
                        value["phase"] = json!(next_phase);
                        value["plan"] = plan.clone();
                        value["observation"] = observation.cloned().unwrap_or(Value::Null);
                        if next_phase == "move-issued" && !plan["drive_serial"].is_null() {
                            // UNLOAD can flush existing drive buffers even when
                            // this explicit unassigned move appends no new data.
                            value["media_may_have_changed"] = json!(true);
                        }
                        value["last_error"] = observation
                            .and_then(|v| v.get("diagnostic"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        validate_state(&value, task)?;

                        tr.set(&task.journal(), &encoded(&value));
                        Ok(value)
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
    pub async fn finish_native_device_task(
        &self,
        task: &NativeDeviceTask,
        owner: u64,
        result: &Value,
        failure: Option<&str>,
    ) -> Result<Value> {
        if task.kind() == "media-assign" {
            return Err(CatalogError::Invalid("assign 必须通过专属分配执行器发布"));
        }
        if !result.is_object()
            || encoded(result).len() > 32_000
            || failure.is_some_and(|s| s.len() > 8192)
        {
            return Err(CatalogError::Invalid("设备任务结果或错误过长"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(
                            &tr,
                            task.serial(),
                            task.route,
                            owner,
                        )
                        .await?;
                        let saved =
                            management::read_task(&tr, task.serial(), &task.request).await?;
                        if matches!(
                            saved["status"].as_str(),
                            Some("succeeded" | "failed" | "cancelled")
                        ) {
                            return Ok(saved);
                        }
                        let state = read_state(&tr, task, owner).await?;
                        let cancelled = state["cancel_requested"] == true;
                        if !matches!(
                            state["phase"].as_str(),
                            Some("ready" | "planned" | "loaded" | "verified")
                        ) {
                            return Err(CatalogError::Incomplete);
                        }
                        if failure.is_none() && !cancelled && state["phase"] != "verified" {
                            return Err(CatalogError::Incomplete);
                        }
                        let assignment = if failure.is_none() && !cancelled {
                            require_assignment(&tr, task).await?
                        } else {
                            None
                        };
                        if failure.is_none()
                            && !cancelled
                            && matches!(task.kind(), "media-validate" | "media-online")
                        {
                            verify_validation_at(&tr, task, &state["observation"]).await?;
                        }
                        let online =
                            if failure.is_none() && !cancelled && task.kind() == "media-online" {
                                let mut a = assignment.clone().ok_or(CatalogError::Incomplete)?;
                                a.revision = a
                                    .revision
                                    .checked_add(1)
                                    .ok_or(CatalogError::Invalid("磁带策略修订号溢出"))?;
                                a.online = true;
                                let mut attributes = tape_policy::read_at(&tr, &a).await?;
                                attributes["offline_message"] = Value::Null;
                                Some((a, attributes))
                            } else {
                                None
                            };
                        let mut writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                        let mut clears: Vec<Vec<u8>> = Vec::new();
                        let mut released_volume = None;
                        if failure.is_none() && !cancelled && task.kind() == "media-unassign" {
                            let a = assignment.ok_or(CatalogError::Incomplete)?;
                            let volume = a.volume_id.ok_or(CatalogError::Incomplete)?;
                            require_unreferenced(&tr, task, volume).await?;
                            released_volume = Some(volume);
                            if let Some(raw) = tr.get(&ready_volume_key(volume), false).await? {
                                let (scan, serial) = decode_ready_volume(&raw)?;
                                if serial != task.serial() {
                                    return Err(CatalogError::Corrupt("解绑卷就绪带库不符"));
                                }
                                let manifest = tr
                                    .get(&scan_key(volume, scan.id), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("解绑扫描缺失"))?;
                                if decode_scan_manifest(&manifest)?
                                    != (scan, serial.clone(), ScanState::Ready)
                                {
                                    return Err(CatalogError::Corrupt("解绑扫描状态不符"));
                                }
                                writes.push((
                                    scan_key(volume, scan.id),
                                    encode_scan_manifest(scan, &serial, ScanState::Retired),
                                ));
                                clears.push(ready_volume_key(volume));
                            }
                            clears.extend([
                                placement_key(volume),
                                barcode_key(
                                    task.serial(),
                                    task.barcode().expect("unassign barcode"),
                                ),
                                assignment_key(
                                    task.serial(),
                                    task.barcode().expect("unassign barcode"),
                                ),
                                namespace_assignment_key(
                                    task.request.namespace(),
                                    a.pool_id,
                                    task.serial(),
                                    task.barcode().expect("unassign barcode"),
                                ),
                            ]);
                        }
                        let mut receipt =
                            management::read_task(&tr, task.serial(), &task.request).await?;
                        receipt["status"] = json!(if failure.is_some() {
                            "failed"
                        } else if state["cancel_requested"] == true {
                            "cancelled"
                        } else {
                            "succeeded"
                        });
                        receipt["completed_at_millis"] = json!(
                            management::now().max(
                                receipt["created_at_millis"]
                                    .as_u64()
                                    .expect("validated time")
                            )
                        );
                        receipt["result"] = json!({"device":state,"result":result,"error":failure});
                        if encoded(&receipt).len() > 16_384 {
                            return Err(CatalogError::Invalid("设备任务终态超过 FDB 单记录上限"));
                        }
                        // Every fallible proof precedes mutation: domain errors returned by
                        // retry_result are committed by db.run, so partial unlink is unsafe.
                        if failure.is_none() && !cancelled {
                            inventory::record_at(
                                &tr,
                                task.serial(),
                                task.request.namespace(),
                                owner,
                                &result["observation"],
                            )
                            .await?;
                        }
                        if let Some((a, attributes)) = online {
                            tr.set(
                                &assignment_key(task.serial(), &a.barcode),
                                &encode_assignment_state(
                                    a.namespace,
                                    a.pool_id,
                                    a.volume_id,
                                    a.revision,
                                    true,
                                    a.blacklisted,
                                ),
                            );
                            tape_policy::write_at(&tr, &a, &attributes);
                        }
                        if released_volume.is_some() {
                            let a = require_assignment(&tr, task)
                                .await
                                .map_err(notifications::atomic_error)?
                                .ok_or_else(|| {
                                    notifications::atomic_error(CatalogError::Incomplete)
                                })?;
                            tape_policy::clear_at(&tr, &a);
                        }
                        for (key, value) in writes {
                            tr.set(&key, &value);
                        }
                        for key in clears {
                            tr.clear(&key);
                        }
                        if let Some(volume) = released_volume {
                            active_volume::clear_unassigned_cursor_at(&tr, volume);
                            tr.clear(&home_key(
                                task.request.namespace(),
                                task.serial(),
                                task.barcode().expect("unassign barcode"),
                            ));
                        }
                        if failure.is_none() && !cancelled && task.kind() == "media-move" {
                            record_home_at(&tr, task, &state);
                        }
                        management::write_task(&tr, &task.request, &receipt).await?;
                        management::clear_queued(&tr, &task.request);
                        tr.clear(&task.gate());
                        if let Some(key) = task.pool_gate() {
                            tr.clear(&key);
                        }
                        tr.clear(&task.journal());
                        if let Some(barcode) = task.barcode() {
                            notifications::tape_changed_at(
                                &tr,
                                task.request.namespace(),
                                task.serial(),
                                barcode,
                                "device_task_completed",
                            )
                            .await?;
                        }
                        Ok(receipt)
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}
pub(super) async fn request_cancel(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    request: &ManagementRequest,
) -> Result<()> {
    let task = NativeDeviceTask {
        request: request.clone(),
        route,
    };
    let raw = tr
        .get(&task.journal(), false)
        .await?
        .ok_or(CatalogError::Corrupt("取消设备任务检查点缺失"))?;
    let mut value: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("取消设备任务 JSON"))?;
    if task.serial() != serial {
        return Err(CatalogError::Corrupt("取消设备任务带库不符"));
    }
    validate_state(&value, &task)?;
    value["cancel_requested"] = json!(true);

    tr.set(&task.journal(), &encoded(&value));
    Ok(())
}

pub(super) async fn inventory_task(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    pool: Uuid,
    barcode: &str,
) -> Result<Option<Value>> {
    let Some(raw) = tr.get(&pool_gate(ns, pool), false).await? else {
        return Ok(None);
    };
    let gate: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("设备维护屏障编码"))?;
    let epoch = gate["epoch"]
        .as_u64()
        .ok_or(CatalogError::Corrupt("设备维护屏障代次"))?;
    let operation = gate["operation"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(CatalogError::Corrupt("设备维护屏障操作"))?;
    let (request, task) = management::task_for_identity(tr, serial, ns, epoch, operation).await?;
    if raw.as_ref() != mechanical_token(&request).as_slice()
        || tr.get(&library_gate(ns, serial), false).await?.as_deref() != Some(raw.as_ref())
        || gate["fingerprint"] != request.fingerprint()
        || request.value()["change"]["pool"] != pool.to_string()
    {
        return Err(CatalogError::Corrupt("设备维护屏障请求不符"));
    }
    if request.value()["change"]["barcode"] != barcode {
        return Ok(None);
    }
    Ok(Some(
        json!({"task_id":crate::management_task_id::ManagementTaskId::new(ns,serial,epoch,operation).map_err(CatalogError::Corrupt)?.to_string(),"progress":task_progress(tr,&task).await?}),
    ))
}
pub(super) async fn task_progress(tr: &Transaction, task: &Value) -> Result<Value> {
    if task["status"] != "running" {
        return Ok(task["result"]["device"].clone());
    }
    let request = ManagementRequest::parse(task["request"].clone())?;
    let mut journal = domain(request.namespace(), b't');
    append_component(
        &mut journal,
        request.value()["library"]
            .as_str()
            .expect("library")
            .as_bytes(),
    );
    journal.extend_from_slice(&request.epoch().to_be_bytes());
    journal.extend_from_slice(request.operation().as_bytes());
    let raw = tr
        .get(&journal, false)
        .await?
        .ok_or(CatalogError::Corrupt("设备任务进度检查点缺失"))?;
    let value: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("设备任务进度 JSON"))?;
    if encoded(&value) != raw.as_ref() {
        return Err(CatalogError::Corrupt("设备任务进度编码不规范"));
    }
    let serial = request.value()["library"]
        .as_str()
        .expect("validated library");
    let active = tr
        .get(&routing::active_key(serial), false)
        .await?
        .ok_or(CatalogError::Corrupt("设备任务活动路由缺失"))?;
    let route = routing::decode_active(&active)?;
    if route.route.namespace != request.namespace() {
        return Err(CatalogError::Corrupt("设备任务进度命名空间与路由不符"));
    }
    validate_state(&value, &NativeDeviceTask { request, route })?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::LtfsIndex;
    use crate::ltfs::recovery::IndexSourceIdentity;
    use sha2::{Digest, Sha256};

    #[test]
    fn physical_node_claim_key_is_global_and_separate_from_namespace_records() {
        let volume = Uuid::new_v4();
        let global = node_attempts::volume_key(volume);
        let mut expected = management::prefix(Uuid::nil(), b'Q');
        expected.push(b'v');
        expected.extend_from_slice(volume.as_bytes());
        assert_eq!(global, expected);
        assert_ne!(global, node_attempts::volume_key(Uuid::new_v4()));
        let mut obsolete = management::prefix(Uuid::new_v4(), b'Q');
        obsolete.push(b'v');
        obsolete.extend_from_slice(volume.as_bytes());
        assert_ne!(global, obsolete);
    }

    #[tokio::test]
    #[ignore = "requires FDB_TEST_CLUSTER pointing to a disposable FoundationDB cluster; no hardware"]
    async fn volume_reference_checks_cover_foreign_namespaces_and_conflict_with_new_claims() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        let foreign = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let mut retained = key(b'V');
        for id in [
            volume,
            Uuid::new_v4(),
            foreign,
            Uuid::new_v4(),
            Uuid::new_v4(),
        ] {
            retained.extend_from_slice(id.as_bytes());
        }
        let mut probes = vec![retained.clone()];
        for kind in *b"243" {
            let mut entry = key(kind);
            entry.extend_from_slice(volume.as_bytes());
            probes.push(entry);
        }
        let mut claim = active_batch::reverse_claim_prefix_for(volume);
        claim.extend_from_slice(Uuid::new_v4().as_bytes());
        probes.extend([
            claim,
            reconcile_volume_key(volume),
            native_media::volume_claim_key(volume),
            node_attempts::volume_key(volume),
        ]);
        for kind in *b"0IF" {
            let mut entry = requests::epoch_prefix(kind, namespace, 1);
            entry.extend_from_slice(Uuid::new_v4().as_bytes());
            probes.push(entry);
        }
        let mut node_hold = node_hold_prefix(namespace);
        node_hold.extend_from_slice(Uuid::new_v4().as_bytes());
        probes.push(node_hold);
        let foreign_claim = json!({"namespace":foreign.to_string(),"probe":"foreign-volume-hold"})
            .to_string()
            .into_bytes();
        for entry in &probes {
            let tr = catalog.db.create_trx().unwrap();
            require_volume_unreferenced_at(&tr, namespace, volume)
                .await
                .unwrap();
            tr.set(entry, &foreign_claim);
            tr.commit().await.unwrap();
            let check = catalog.db.create_trx().unwrap();
            assert!(
                require_volume_unreferenced_at(&check, namespace, volume)
                    .await
                    .is_err(),
                "{entry:?}"
            );
            assert_eq!(
                check.get(entry, false).await.unwrap().unwrap().as_ref(),
                foreign_claim
            );
            check.clear(entry);
            check.commit().await.unwrap();
        }
        // A source without placement must still respect a foreign global hold.
        let slot = node_attempts::volume_key(volume);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&slot, &foreign_claim);
        tr.commit().await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert!(matches!(
            node_attempts::require_no_pending_at(&tr, volume).await,
            Err(CatalogError::PoolBusy)
        ));
        require_volume_unreferenced_at(&tr, namespace, Uuid::new_v4())
            .await
            .unwrap();
        tr.clear(&slot);
        tr.commit().await.unwrap();

        // Both an absent global slot and an empty V range must be conflict
        // reads, so an erase checkpoint cannot commit a stale negative proof.
        let mut witness = management::prefix(namespace, b'!');
        witness.extend_from_slice(volume.as_bytes());
        for entry in [&slot, &retained] {
            let checked = catalog.db.create_trx().unwrap();
            require_volume_unreferenced_at(&checked, namespace, volume)
                .await
                .unwrap();
            let concurrent = catalog.db.create_trx().unwrap();
            concurrent.set(entry, &foreign_claim);
            concurrent.commit().await.unwrap();
            checked.set(&witness, b"stale-proof-must-not-commit");
            assert_eq!(checked.commit().await.unwrap_err().code(), 1020);
            let cleanup = catalog.db.create_trx().unwrap();
            assert!(cleanup.get(&witness, false).await.unwrap().is_none());
            assert_eq!(
                cleanup.get(entry, false).await.unwrap().unwrap().as_ref(),
                foreign_claim
            );
            cleanup.clear(entry);
            cleanup.commit().await.unwrap();
        }
        require_volume_unreferenced_at(&catalog.db.create_trx().unwrap(), namespace, volume)
            .await
            .unwrap();
    }

    fn request(serial: &str, ns: Uuid, change: Value) -> ManagementRequest {
        ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap()
    }
    async fn fixture(catalog: &ManagedCatalog) -> (String, ActiveLibraryRoute, PoolRecord) {
        let ns = Uuid::new_v4();
        let serial = format!("SIM-DEVICE-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
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
            name: "device".into(),
            file_limit: 100,
            created_at_millis: management::now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        (serial, route, pool)
    }
    fn inventory(serial: &str, owner: u64) -> Value {
        json!({"num_of_slots":8,"num_of_ieslots":1,"scsi_vendor_id":"SIM","scsi_product_id":"SIM-CHANGER","scsi_firmware_revision":"0001","library_serial":serial,"round":owner,"owner":1,"volumes":[],"drives":[],"observed_at":"2026-11-04T00:00:00Z"})
    }
    fn observation(serial: &str, owner: u64, validation: Value) -> Value {
        json!({"inventory":inventory(serial,owner),"diagnostic":null,"validation":validation})
    }
    fn plan(barcode: Option<&str>, volume: Option<Uuid>, drive: bool) -> Value {
        json!({"version":1,"barcode":barcode,"source":barcode.map(|_|10),"destination":barcode.map(|_|if drive{100}else{10}),"home":barcode.map(|_|10),"drive_serial":if drive{json!("SIMDRIVE")}else{Value::Null},"volume":volume.map(|v|v.to_string()),"close_may_write":false})
    }
    fn empty_source(volume: Uuid) -> NativeMediaSource {
        let mut index = LtfsIndex::empty(volume, "assign".into(), 'b');
        index.self_location.start_block = 5;
        let xml = index.to_xml().unwrap();
        NativeMediaSource {
            identity: IndexSourceIdentity::new(
                crate::ltfs::volume::CommittedIndexConstruct {
                    volume_uuid: volume,
                    generation: index.generation,
                    location: index.self_location,
                    sha256: Sha256::digest(xml).into(),
                    incremental: false,
                },
                Vec::new(),
            )
            .unwrap(),
            files: 0,
            paths: 0,
        }
    }
    #[test]
    fn device_requests_are_strict_and_require_explicit_format_authorization() {
        let pool = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let valid = json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-assign","pool":pool.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}});
        assert!(ManagementRequest::parse(valid.clone()).is_ok());
        for key in ["volume", "confirm_format", "force_format"] {
            let mut bad = valid.clone();
            bad["change"].as_object_mut().unwrap().remove(key);
            assert!(ManagementRequest::parse(bad).is_err());
        }
        let mut bad = valid.clone();
        bad["change"]["confirm_format"] = json!(false);
        assert!(ManagementRequest::parse(bad).is_err());
        let mut bad = valid;
        bad["change"]["path"] = json!("/");
        assert!(ManagementRequest::parse(bad).is_err());
    }

    #[test]
    fn only_assigned_drive_moves_may_close_a_managed_volume() {
        let ns = Uuid::new_v4();
        let route = ActiveLibraryRoute {
            route: routing::PreparedLibraryRoute {
                namespace: ns,
                revision: 1,
            },
            frozen_index: 1,
            certificate_sha256: [1; 32],
            initial_owner_epoch: 1,
        };
        let bare = NativeDeviceTask {
            request: request(
                "SIM",
                ns,
                json!({"kind":"media-move","pool":null,
                "barcode":"DEVICE0L8","volume":null,"revision":null,"destination":"homeslot"}),
            ),
            route,
        };
        let mut p = plan(Some("DEVICE0L8"), None, true);
        p["source"] = json!(1);
        p["destination"] = p["home"].clone();
        validate_plan(&bare, &p).unwrap();
        p["close_may_write"] = json!(true);
        assert!(validate_plan(&bare, &p).is_err());
        p["close_may_write"] = json!(false);
        p["destination"] = json!(11);
        assert!(
            validate_plan(&bare, &p).is_err(),
            "homeslot 请求不能替换固定 home"
        );

        let pool = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let assigned = NativeDeviceTask {
            request: request(
                "SIM",
                ns,
                json!({"kind":"media-move","pool":pool.to_string(),
                "barcode":"DEVICE0L8","volume":volume.to_string(),"revision":1,"destination":"homeslot"}),
            ),
            route,
        };
        p["destination"] = p["home"].clone();
        p["volume"] = json!(volume.to_string());
        assert!(validate_plan(&assigned, &p).is_err());
        p["close_may_write"] = json!(true);
        validate_plan(&assigned, &p).unwrap();
    }
    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；验证同步取消查询，不访问实际设备"]
    async fn blocking_cancellation_probe_needs_no_tokio_reactor_and_checks_current_authority() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = std::sync::Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (serial, route, pool) = fixture(&catalog).await;
        let volume = Uuid::new_v4();
        let proof = empty_source(volume);
        let seed = request(
            &serial,
            route.route.namespace,
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"PROBE0L8","volume":volume.to_string(),"path":"/","merge_tapes":true}),
        );
        catalog
            .admit_management(&serial, route, 100, &seed)
            .await
            .unwrap();
        let import = catalog
            .start_native_media(&serial, route, 100, 1, seed.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&import, 100)
            .await
            .unwrap();
        catalog
            .finish_native_media(&import, 100, &proof)
            .await
            .unwrap();
        let rescan = request(
            &serial,
            route.route.namespace,
            json!({"kind":"media-validate","pool":pool.id.to_string(),"barcode":"PROBE0L8","volume":volume.to_string(),"revision":1}),
        );
        catalog
            .admit_management(&serial, route, 100, &rescan)
            .await
            .unwrap();
        let task = catalog
            .start_native_device_task(&serial, route, 100, 1, rescan.operation())
            .await
            .unwrap()
            .unwrap();
        let probe = |owner| {
            let catalog = catalog.clone();
            let task = task.clone();
            std::thread::spawn(move || catalog.native_device_cancelled_blocking(&task, owner))
                .join()
                .unwrap()
        };
        assert!(!probe(100).unwrap());
        // Also safe on the current-thread actor's runtime: FDB's own network
        // thread, rather than Tokio, drives this bounded read-only future.
        assert!(
            !catalog
                .native_device_cancelled_blocking(&task, 100)
                .unwrap()
        );
        assert!(matches!(probe(101), Err(CatalogError::OwnerFenced)));
        catalog
            .cancel_management(&serial, route, 100, 1, rescan.operation())
            .await
            .unwrap();
        assert!(probe(100).unwrap());
        let done = catalog
            .finish_native_device_task(&task, 100, &json!({}), None)
            .await
            .unwrap();
        assert_eq!(done["status"], "cancelled");
        assert!(probe(100).is_err(), "终态任务不能继续读取设备");
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；只验证持久意图、owner fencing、无引用解绑，不操作设备"]
    async fn device_checkpoints_validation_and_unassign_are_fenced_and_atomic() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let ns = route.route.namespace;
        // Physical assignment is covered by the sole main assign actor. This
        // catalog fixture admits a real empty source through the import path.
        let volume = Uuid::new_v4();
        let source = empty_source(volume);
        let imported = request(
            &serial,
            ns,
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"path":"/","merge_tapes":true}),
        );
        catalog
            .admit_management(&serial, route, 100, &imported)
            .await
            .unwrap();
        let import = catalog
            .start_native_media(&serial, route, 100, 1, imported.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .bind_native_media_source(&import, 100, &source)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&import, 100)
            .await
            .unwrap();
        let done = catalog
            .finish_native_media(&import, 100, &source)
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        let index = source.index();
        let validation = json!({"volume":volume.to_string(),"generation":index.generation,"index_sha256":index.sha256.iter().map(|byte|format!("{byte:02x}")).collect::<String>(),"files":0,"scope":"label_and_index","payload_verified":false,"pool_mark_verified":true});
        let observed = observation(&serial, 101, validation);
        // Validation cannot certify an index that differs from the committed FDB head.
        let validate = request(
            &serial,
            ns,
            json!({"kind":"media-validate","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"revision":1}),
        );
        catalog
            .admit_management(&serial, route, 101, &validate)
            .await
            .unwrap();
        let task = catalog
            .start_native_device_task(&serial, route, 101, 1, validate.operation())
            .await
            .unwrap()
            .unwrap();
        let p = plan(Some("DEVICE0L8"), Some(volume), true);
        catalog
            .checkpoint_native_device_task(&task, 101, "ready", "planned", &p, None)
            .await
            .unwrap();
        catalog
            .checkpoint_native_device_task(&task, 101, "planned", "load-issued", &p, None)
            .await
            .unwrap();
        catalog
            .checkpoint_native_device_task(&task, 101, "load-issued", "loaded", &p, None)
            .await
            .unwrap();
        let mut validated = observed.clone();
        validated["validation"]
            .as_object_mut()
            .unwrap()
            .remove("pool_mark_verified");
        let mut bad = validated.clone();
        bad["validation"]["index_sha256"] = json!("0".repeat(64));
        assert!(
            catalog
                .checkpoint_native_device_task(&task, 101, "loaded", "verified", &p, Some(&bad))
                .await
                .is_err()
        );
        catalog
            .checkpoint_native_device_task(&task, 101, "loaded", "verified", &p, Some(&validated))
            .await
            .unwrap();
        let original_assignment = catalog.tape_assignment(&serial, "DEVICE0L8").await.unwrap();
        let original_inventory = catalog
            .active_library_inventory(&serial, route)
            .await
            .unwrap();
        let original_task = catalog
            .management_task(&serial, route, 1, validate.operation())
            .await
            .unwrap();
        let oversized = json!({"observation":inventory(&serial,101),"validation":validated["validation"],"extra":"x".repeat(17_000)});
        assert!(
            catalog
                .finish_native_device_task(&task, 101, &oversized, None)
                .await
                .is_err()
        );
        assert_eq!(
            catalog.tape_assignment(&serial, "DEVICE0L8").await.unwrap(),
            original_assignment
        );
        assert_eq!(
            catalog
                .active_library_inventory(&serial, route)
                .await
                .unwrap(),
            original_inventory
        );
        assert_eq!(
            catalog
                .management_task(&serial, route, 1, validate.operation())
                .await
                .unwrap(),
            original_task
        );
        assert!(matches!(
            catalog
                .require_active_library_readable(&serial, route, 101)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(catalog.finish_native_device_task(&task,101,&json!({"observation":inventory(&serial,101),"validation":validated["validation"]}),None).await.unwrap()["status"],"succeeded");
        // Cancellation leaves a successful inventory untouched and releases only its own gate.
        let cancelled = request(
            &serial,
            ns,
            json!({"kind":"media-validate","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"revision":1}),
        );
        catalog
            .admit_management(&serial, route, 101, &cancelled)
            .await
            .unwrap();
        let task = catalog
            .start_native_device_task(&serial, route, 101, 1, cancelled.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .cancel_management(&serial, route, 101, 1, cancelled.operation())
            .await
            .unwrap();
        assert_eq!(
            catalog
                .management_task(&serial, route, 1, cancelled.operation())
                .await
                .unwrap()
                .unwrap()["progress"]["cancel_requested"],
            true
        );
        assert_eq!(
            catalog
                .finish_native_device_task(&task, 101, &json!({}), None)
                .await
                .unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            catalog
                .active_library_inventory(&serial, route)
                .await
                .unwrap(),
            Some(inventory(&serial, 101))
        );
        // Every retained historical V reference blocks unassignment, even another namespace.
        let unassign = request(
            &serial,
            ns,
            json!({"kind":"media-unassign","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"revision":1}),
        );
        catalog
            .admit_management(&serial, route, 101, &unassign)
            .await
            .unwrap();
        let reverse = volume_key(
            volume,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&reverse, b"retained");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .start_native_device_task(&serial, route, 101, 1, unassign.operation())
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&reverse);
        tr.commit().await.unwrap();
        // A real empty-recovery protocol leaves a current cursor but no file references.
        let previous: ActiveIndexIdentity = source.index().into();
        assert_eq!(
            catalog
                .begin_active_volume_recovery(&serial, 101, previous)
                .await
                .unwrap(),
            ActiveRecoveryOutcome::Begun
        );
        assert!(
            catalog
                .start_native_device_task(&serial, route, 101, 1, unassign.operation())
                .await
                .is_err()
        );
        let recovered = ActiveIndexIdentity {
            generation: previous.generation + 1,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 20,
            },
            sha256: [8; 32],
            ..previous
        };
        assert_eq!(
            catalog
                .finish_active_volume_recovery(&serial, 101, previous, Some(recovered))
                .await
                .unwrap(),
            ActiveRecoveryOutcome::Recorded
        );
        let history_key = active_volume::generation_key(volume, recovered.generation);
        let tr = catalog.db.create_trx().unwrap();
        let history = tr.get(&history_key, false).await.unwrap().unwrap().to_vec();
        let original_source = tr
            .get(&source_key(volume, import.scan()), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        drop(tr);
        let task = catalog
            .start_native_device_task(&serial, route, 101, 1, unassign.operation())
            .await
            .unwrap()
            .unwrap();
        let p = plan(Some("DEVICE0L8"), Some(volume), false);
        catalog
            .checkpoint_native_device_task(&task, 101, "ready", "planned", &p, None)
            .await
            .unwrap();
        let obs = observation(&serial, 101, Value::Null);
        catalog
            .checkpoint_native_device_task(&task, 101, "planned", "verified", &p, Some(&obs))
            .await
            .unwrap();
        // A newly observed in-flight reservation at finish must preserve all assignment keys.
        let mut pending = key(b'2');
        pending.extend_from_slice(volume.as_bytes());
        pending.extend_from_slice(&77u64.to_be_bytes());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&pending, b"pending");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .finish_native_device_task(
                    &task,
                    101,
                    &json!({"observation":inventory(&serial,101)}),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .tape_assignment(&serial, "DEVICE0L8")
                .await
                .unwrap()
                .is_some()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&pending);
        tr.commit().await.unwrap();
        let done = catalog
            .finish_native_device_task(
                &task,
                101,
                &json!({"observation":inventory(&serial,101)}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        assert!(
            catalog
                .tape_assignment(&serial, "DEVICE0L8")
                .await
                .unwrap()
                .is_none()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&placement_key(volume), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tr.get(&ready_volume_key(volume), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            decode_scan_manifest(
                &tr.get(&scan_key(volume, import.scan()), false)
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap()
            .2,
            ScanState::Retired
        );
        assert_eq!(
            catalog
                .native_device_home(&serial, route, 101, "DEVICE0L8", None)
                .await
                .unwrap(),
            None
        );
        assert!(
            catalog
                .native_device_home(&serial, route, 101, "DEVICE0L8", Some(volume))
                .await
                .is_err()
        );
        assert_eq!(
            tr.get(&history_key, false).await.unwrap().unwrap().as_ref(),
            history.as_slice()
        );
        assert_eq!(
            tr.get(&source_key(volume, import.scan()), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            original_source.as_slice()
        );
        drop(tr);
        // Reimport the same physical UUID at its actual newer baseline. An old
        // current cursor must not be mistaken for a successor to this new scan.
        let again = request(
            &serial,
            ns,
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"path":"/","merge_tapes":true}),
        );
        catalog
            .admit_management(&serial, route, 101, &again)
            .await
            .unwrap();
        let imported = catalog
            .start_native_media(&serial, route, 101, 1, again.operation())
            .await
            .unwrap()
            .unwrap();
        let current = NativeMediaSource {
            identity: IndexSourceIdentity::new(
                crate::ltfs::volume::CommittedIndexConstruct {
                    volume_uuid: volume,
                    generation: recovered.generation,
                    location: recovered.location,
                    sha256: recovered.sha256,
                    incremental: false,
                },
                Vec::new(),
            )
            .unwrap(),
            files: 0,
            paths: 0,
        };
        catalog
            .bind_native_media_source(&imported, 101, &current)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&imported, 101)
            .await
            .unwrap();
        catalog
            .finish_native_media(&imported, 101, &current)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .active_loaded_volume_head(&serial, "DEVICE0L8", route, 101)
                .await
                .unwrap()
                .unwrap()
                .index,
            recovered
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&history_key, false).await.unwrap().unwrap().as_ref(),
            history.as_slice()
        );
        drop(tr);
        // A cancellation request cannot certify an unknown physical result.
        let cancel = request(
            &serial,
            ns,
            json!({"kind":"media-validate","pool":pool.id.to_string(),"barcode":"DEVICE0L8","volume":volume.to_string(),"revision":1}),
        );
        catalog
            .admit_management(&serial, route, 101, &cancel)
            .await
            .unwrap();
        let task = catalog
            .start_native_device_task(&serial, route, 101, 1, cancel.operation())
            .await
            .unwrap()
            .unwrap();
        let p = plan(Some("DEVICE0L8"), Some(volume), true);
        catalog
            .checkpoint_native_device_task(&task, 101, "ready", "planned", &p, None)
            .await
            .unwrap();
        catalog
            .checkpoint_native_device_task(&task, 101, "planned", "load-issued", &p, None)
            .await
            .unwrap();
        catalog
            .cancel_management(&serial, route, 101, 1, cancel.operation())
            .await
            .unwrap();
        assert!(
            catalog
                .finish_native_device_task(
                    &task,
                    101,
                    &json!({"observation":inventory(&serial,101)}),
                    None
                )
                .await
                .is_err()
        );
        assert!(matches!(
            catalog
                .require_active_library_readable(&serial, route, 101)
                .await,
            Err(CatalogError::PoolBusy)
        ));
        catalog
            .claim_active_owner_epoch(&serial, 102)
            .await
            .unwrap();
        let resumed = catalog
            .start_native_device_task(&serial, route, 102, 1, cancel.operation())
            .await
            .unwrap()
            .unwrap();
        let state = catalog.native_device_status(&resumed, 102).await.unwrap();
        assert_eq!(state["phase"], "load-issued");
        assert_eq!(state["cancel_requested"], true);
        assert!(
            catalog
                .finish_native_device_task(
                    &resumed,
                    102,
                    &json!({"observation":inventory(&serial,102)}),
                    Some("cancel is not a completion proof")
                )
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .management_task(&serial, route, 1, cancel.operation())
                .await
                .unwrap()
                .unwrap()["status"],
            "running"
        );
    }
}
