//! Admission of a complete ordinary pool tape selection under one owner fence.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SelectedTape {
    barcode: String,
    source_address: u16,
    home_address: u16,
    status: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    version: u8,
    serial: String,
    logical: Option<Uuid>,
    pool: Vec<u8>,
    confirm_format: bool,
    owner_epoch: u64,
    inventory_observed_at: Value,
    tapes: Vec<SelectedTape>,
    completed: Vec<Uuid>,
    child_epoch: Option<u64>,
    detail: Option<String>,
}

fn key_for(ns: Uuid, kind: u8) -> Vec<u8> {
    let mut key = management::prefix(ns, b'C');
    key.push(kind);
    key
}

fn id_key(ns: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = key_for(ns, b'i');
    key.extend_from_slice(id.as_bytes());
    key
}

fn pending_prefix(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = key_for(ns, b'p');
    append_component(&mut key, serial.as_bytes());
    key
}

fn pending_key(ns: Uuid, serial: &str, id: Uuid) -> Vec<u8> {
    let mut key = pending_prefix(ns, serial);
    key.extend_from_slice(id.as_bytes());
    key
}

fn child_operation(ns: Uuid, pool: Uuid, barcode: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/selected-pool/child/1");
    hash.update(ns.as_bytes());
    hash.update(pool.as_bytes());
    hash.update(barcode.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("SHA prefix");
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Uuid::from_bytes(bytes)
}

fn name_key(ns: Uuid, name: &str) -> Vec<u8> {
    let mut key = key_for(ns, b'n');
    append_component(&mut key, name.as_bytes());
    key
}

fn barcode_key(ns: Uuid, serial: &str, barcode: &str) -> Vec<u8> {
    let mut key = key_for(ns, b'b');
    append_component(&mut key, serial.as_bytes());
    append_component(&mut key, barcode.as_bytes());
    key
}

fn home_key(ns: Uuid, serial: &str, address: u16) -> Vec<u8> {
    let mut key = key_for(ns, b'h');
    append_component(&mut key, serial.as_bytes());
    key.extend_from_slice(&address.to_be_bytes());
    key
}

pub(super) async fn name_available_at(tr: &Transaction, ns: Uuid, name: &str) -> Result<bool> {
    Ok(tr.get(&name_key(ns, name), false).await?.is_none())
}

pub(super) async fn reserved_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
    home: u16,
) -> Result<bool> {
    Ok(tr
        .get(&barcode_key(ns, serial, barcode), false)
        .await?
        .is_some()
        || tr.get(&home_key(ns, serial, home), false).await?.is_some())
}

pub(super) async fn require_home_available(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    address: u16,
    own_pool: Option<Uuid>,
) -> Result<()> {
    if let Some(raw) = tr.get(&home_key(ns, serial, address), false).await?
        && Some(uuid_from(&raw)?) != own_pool
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

pub(super) async fn require_tape_available(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
) -> Result<()> {
    if tr
        .get(&barcode_key(ns, serial, barcode), false)
        .await?
        .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

pub(super) async fn id_available_at(tr: &Transaction, ns: Uuid, id: Uuid) -> Result<bool> {
    Ok(tr.get(&id_key(ns, id), false).await?.is_none())
}

fn saved_selection(raw: &[u8], pool_id: Uuid) -> Result<Selection> {
    let selection: Selection =
        serde_json::from_slice(raw).map_err(|_| CatalogError::Corrupt("创建池介质预约编码无效"))?;
    let pool = decode_pool(pool_id, &selection.pool)?;
    let barcodes: Vec<_> = selection
        .tapes
        .iter()
        .map(|tape| tape.barcode.clone())
        .collect();
    if selection.version != 1
        || selection.completed.len() > selection.tapes.len()
        || selection.completed.iter().any(|id| id.is_nil())
        || (selection.child_epoch.is_some() && selection.completed.len() == selection.tapes.len())
        || selection.child_epoch == Some(0)
        || selection.owner_epoch == 0
        || selection.logical.is_some_and(|scope| scope.is_nil())
        || pool.library_serial != selection.serial
        || logical_libraries::validate_pool_selection(&barcodes).is_err()
        || selection.tapes.iter().any(|tape| tape.status != "eligible")
        || selection
            .inventory_observed_at
            .as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_none()
    {
        return Err(CatalogError::Corrupt("创建池介质预约身份或库存无效"));
    }
    Ok(selection)
}

fn receipt(ns: Uuid, pool: &PoolRecord, selection: &Selection) -> Result<Value> {
    let status = if selection.completed.len() == selection.tapes.len() {
        "succeeded"
    } else if selection.detail.is_some() {
        "blocked"
    } else if selection.child_epoch.is_some() || !selection.completed.is_empty() {
        "running"
    } else {
        "queued"
    };
    let mut tapes = serde_json::to_value(&selection.tapes)
        .map_err(|error| CatalogError::Cache(error.to_string()))?;
    for (index, volume) in selection.completed.iter().enumerate() {
        tapes[index]["volume_uuid"] = json!(volume);
        tapes[index]["status"] = json!("succeeded");
    }
    Ok(json!({
        "version":1,"library_serial":selection.serial,"namespace":ns,"logical_library":selection.logical,
        "id":pool.id,"name":pool.name,"file_limit":pool.file_limit,"status":status,
        "confirm_format":selection.confirm_format,"owner_epoch":selection.owner_epoch,
        "inventory_observed_at":selection.inventory_observed_at,"tapes":tapes,"detail":selection.detail,
        "pool":if status == "succeeded" { Some(pool.to_json()?) } else { None },
    }))
}

impl ManagedCatalog {
    pub(crate) async fn admit_selected_pool(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        logical: Option<Uuid>,
        pool: &PoolRecord,
        tapes: &[String],
        confirm_format: bool,
    ) -> Result<Value> {
        validate_pool(pool)?;
        logical_libraries::validate_pool_selection(tapes)?;
        if pool.revision != 1 || logical.is_some_and(|id| id.is_nil()) {
            return Err(CatalogError::Invalid("新池带库或初始修订号无效"));
        }
        let serial = pool.library_serial.as_str();
        self.db.run(|tr, _| async move {
            requests::retry_result(async {
                let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                let id = id_key(ns, pool.id);
                if let Some(raw) = tr.get(&id, false).await? {
                    let saved = saved_selection(&raw, pool.id)?;
                    if saved.serial != serial || saved.logical != logical
                        || saved.pool != encode_pool(pool) || saved.confirm_format != confirm_format
                        || !saved.tapes.iter().map(|row| &row.barcode).eq(tapes)
                    {
                        return Err(CatalogError::IdempotencyMismatch);
                    }
                    if saved.completed.len() == saved.tapes.len() {
                        return receipt(ns, pool, &saved);
                    }
                    for key in std::iter::once(name_key(ns, &pool.name)).chain(
                        saved.tapes.iter().flat_map(|tape| [
                            barcode_key(ns, serial, &tape.barcode),
                            home_key(ns, serial, tape.home_address),
                        ])
                    ) {
                        if tr.get(&key, false).await?.as_deref() != Some(pool.id.as_bytes().as_slice()) {
                            return Err(CatalogError::Corrupt("创建池预约索引缺失或冲突"));
                        }
                    }
                    return receipt(ns, pool, &saved);
                }
                management::moves::require_readable_at(&tr, ns, serial).await?;
                management::require_selected_tapes_idle(&tr, ns, serial, tapes).await?;
                let (observed_at, mut rows) = logical_libraries::pool_selection_at(
                    &tr, ns, serial, owner, logical, tapes,
                ).await?;
                let name_available = name_available_at(&tr, ns, &pool.name).await?
                    && tr.get(&pool_name_key(ns, &pool.name), false).await?.is_none();
                let id_available = tr.get(&pool_key(ns, pool.id), false).await?.is_none()
                    && retired_pool_revision_at(&tr, ns, pool.id).await?.is_none();
                for row in &mut rows {
                    if row["status"] == "eligible" && pool.policy.media_restriction != "none"
                        && !row["barcode"].as_str().expect("validated barcode").ends_with(&pool.policy.media_restriction)
                    {
                        row["status"] = json!("media_restriction");
                    }
                }
                if !name_available || !id_available || rows.iter().any(|row| row["status"] != "eligible") {
                    return Ok(json!({
                        "version":1,"library_serial":serial,"namespace":ns,"logical_library":logical,
                        "id":pool.id,"name":pool.name,"file_limit":pool.file_limit,"status":"blocked",
                        "id_available":id_available,"name_available":name_available,
                        "confirm_format":confirm_format,"owner_epoch":owner,
                        "inventory_observed_at":observed_at,"tapes":rows,
                    }));
                }
                let selection = Selection {
                    version: 1, serial: serial.into(), logical, pool: encode_pool(pool),
                    confirm_format, owner_epoch: owner, inventory_observed_at: observed_at,
                    completed: Vec::new(), child_epoch: None, detail: None,
                    tapes: rows.into_iter().map(|row| serde_json::from_value(row)
                        .map_err(|_| CatalogError::Corrupt("入池介质的源位置或 home 缺失")))
                        .collect::<Result<_>>()?,
                };
                let raw = serde_json::to_vec(&selection).map_err(|e| CatalogError::Cache(e.to_string()))?;
                let prefix = pending_prefix(ns, serial);
                if read_page(&tr, &prefix, None, 100, false).await?.items.len() >= 100 {
                    return Err(CatalogError::ManagementQueueFull);
                }
                tr.set(&id, &raw);
                tr.set(&pending_key(ns, serial, pool.id), b"");
                tr.set(&name_key(ns, &pool.name), pool.id.as_bytes());
                for tape in &selection.tapes {
                    tr.set(&barcode_key(ns, serial, &tape.barcode), pool.id.as_bytes());
                    tr.set(&home_key(ns, serial, tape.home_address), pool.id.as_bytes());
                }
                receipt(ns, pool, &selection)
            }.await)
        }).await?
    }
}

pub(super) async fn is_pending_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
) -> Result<bool> {
    Ok(tr
        .get(&pending_key(ns, serial, pool), false)
        .await?
        .is_some())
}

pub(in crate::managed_catalog::control) async fn pending_ids_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    limit: usize,
) -> Result<Vec<Uuid>> {
    let prefix = pending_prefix(ns, serial);
    let page = read_page(tr, &prefix, None, limit, false).await?;
    page.items
        .into_iter()
        .map(|(key, value)| {
            if !value.is_empty() {
                return Err(CatalogError::Corrupt("创建池队列索引无效"));
            }
            uuid_from(&key[prefix.len()..])
        })
        .collect()
}

pub(super) async fn pending_in_scope(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    logical: Uuid,
) -> Result<bool> {
    for id in pending_ids_at(tr, ns, serial, 100).await? {
        let raw = tr
            .get(&id_key(ns, id), false)
            .await?
            .ok_or(CatalogError::Corrupt("创建池队列缺少原记录"))?;
        if saved_selection(&raw, id)?.logical == Some(logical) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn require_published_at(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<()> {
    if let Some(raw) = tr.get(&id_key(ns, pool), false).await? {
        let saved = saved_selection(&raw, pool)?;
        if saved.completed.len() != saved.tapes.len() {
            return Err(CatalogError::PoolImporting);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct OriginalSelection {
    pub source: u16,
    pub home: u16,
    pub format: Option<Uuid>,
}

pub(super) async fn original_tape_at(
    tr: &Transaction,
    request: &ManagementRequest,
) -> Result<Option<OriginalSelection>> {
    let ns = request.namespace();
    let serial = request.value()["library"]
        .as_str()
        .expect("validated library");
    let change = &request.value()["change"];
    let Some(barcode) = change["barcode"].as_str() else {
        return Ok(None);
    };
    let Some(raw) = tr.get(&barcode_key(ns, serial, barcode), false).await? else {
        return Ok(None);
    };
    let pool_id = uuid_from(&raw)?;
    let raw = tr
        .get(&id_key(ns, pool_id), false)
        .await?
        .ok_or(CatalogError::Corrupt("介质预约缺少创建池记录"))?;
    let selection = saved_selection(&raw, pool_id)?;
    let tape = selection
        .tapes
        .get(selection.completed.len())
        .ok_or(CatalogError::Corrupt("已完成建池仍持有介质预约"))?;
    if selection.child_epoch != Some(request.epoch())
        || request.operation() != child_operation(ns, pool_id, &tape.barcode)
        || selection.serial != serial
        || selection.logical != request.logical_library()
        || change["kind"] != "media-import"
        || change["pool"] != json!(pool_id)
        || tape.barcode != barcode
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(Some(OriginalSelection {
        source: tape.source_address,
        home: tape.home_address,
        format: selection.confirm_format.then_some(request.operation()),
    }))
}

async fn settle_or_enqueue_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool_id: Uuid,
) -> Result<bool> {
    let key = id_key(ns, pool_id);
    let raw = tr
        .get(&key, false)
        .await?
        .ok_or(CatalogError::Corrupt("创建池队列缺少原记录"))?;
    let mut selected = saved_selection(&raw, pool_id)?;
    if selected.serial != serial {
        return Err(CatalogError::Corrupt("创建池队列带库身份不符"));
    }
    if selected.detail.is_some() {
        return Ok(false);
    }
    let index = selected.completed.len();
    let tape = selected
        .tapes
        .get(index)
        .ok_or(CatalogError::Corrupt("创建池队列仍包含已完成请求"))?;
    let operation = child_operation(ns, pool_id, &tape.barcode);
    let pool = decode_pool(pool_id, &selected.pool)?;
    if let Some(epoch) = selected.child_epoch {
        let (_, task) = management::task_for_identity(tr, serial, ns, epoch, operation).await?;
        match task["status"].as_str() {
            Some("succeeded") => {
                let assignment = ManagedCatalog::verified_assignment(tr, serial, &tape.barcode)
                    .await?
                    .filter(|a| a.namespace == ns && a.pool_id == pool_id)
                    .ok_or(CatalogError::Corrupt("建池子任务缺少已验证介质归属"))?;
                selected.completed.push(
                    assignment
                        .volume_id
                        .ok_or(CatalogError::Corrupt("建池子任务缺少介质卷身份"))?,
                );
                selected.child_epoch = None;
            }
            Some("failed" | "cancelled") => {
                selected.detail = Some(format!("{} {}", tape.barcode, task["result"]));
            }
            Some("queued" | "running") => return Ok(false),
            _ => return Err(CatalogError::Corrupt("建池子任务状态无效")),
        }
    } else {
        if index == 0 {
            match active_pools::create_pool_at(tr, ns, selected.logical, true, true, &pool).await? {
                ActivePoolCreation::Created(_) => {}
                _ => return Err(CatalogError::Corrupt("已预约的新池目标不再可用")),
            }
        }
        let epoch = management::window(tr, ns).await?.accepting;
        let mut change = json!({"kind":"media-import","pool":pool_id,
            "barcode":tape.barcode,"volume":null,"path":"/","merge_tapes":true});
        if let Some(logical) = selected.logical {
            change["logical_library"] = json!(logical);
        }
        let request = ManagementRequest::parse(json!({"version":1,"library":serial,
            "namespace":ns,"epoch":epoch,"operation":operation,"change":change}))?;
        if management::read_task_at(tr, ns, serial, epoch, operation)
            .await?
            .is_some()
        {
            return Err(CatalogError::Corrupt("建池子任务身份已被占用"));
        }
        management::enqueue_at(tr, serial, &request).await?;
        selected.child_epoch = Some(epoch);
    }
    if selected.completed.len() == selected.tapes.len() {
        tr.clear(&pending_key(ns, serial, pool_id));
        tr.clear(&name_key(ns, &pool.name));
        for tape in &selected.tapes {
            tr.clear(&barcode_key(ns, serial, &tape.barcode));
            tr.clear(&home_key(ns, serial, tape.home_address));
        }
        for tape in &selected.tapes {
            notifications::tape_changed_at(
                tr,
                ns,
                serial,
                &tape.barcode,
                "pool_creation_completed",
            )
            .await?;
        }
        namespace_visibility::publish(tr, ns);
    }
    let bytes =
        serde_json::to_vec(&selected).map_err(|error| CatalogError::Cache(error.to_string()))?;
    tr.set(&key, &bytes);
    Ok(true)
}

impl ManagedCatalog {
    pub(crate) async fn selected_pool_create_status(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        logical: Option<Uuid>,
        pool_id: Uuid,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let Some(raw) = tr.get(&id_key(ns, pool_id), false).await? else {
            return Ok(None);
        };
        let selection = saved_selection(&raw, pool_id)?;
        if selection.serial != serial || selection.logical != logical {
            return Ok(None);
        }
        let pool = decode_pool(pool_id, &selection.pool)?;
        let mut value = receipt(ns, &pool, &selection)?;
        if let Some(epoch) = selection.child_epoch {
            let tape = &selection.tapes[selection.completed.len()];
            let (request, task) = management::task_for_identity(
                &tr,
                serial,
                ns,
                epoch,
                child_operation(ns, pool_id, &tape.barcode),
            )
            .await?;
            let checkpoint = native_media::selected_checkpoint_at(&tr, &request).await?;
            let mut progress = native_media::task_progress(&tr, &task).await?;
            if !checkpoint["format_volume"].is_null() && progress["phase"] == "source_pending" {
                progress["phase"] = json!("format_result_pending");
            }
            value["active_tape"] = json!({"barcode":tape.barcode,"epoch":epoch,
                "operation":task["operation"],"status":task["status"],"progress":progress,
                "checkpoint":checkpoint});
        }
        Ok(Some(value))
    }

    pub(crate) async fn advance_selected_pool_create(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        for pool in pending_ids_at(&tr, ns, serial, 100).await? {
                            if settle_or_enqueue_at(&tr, ns, serial, pool).await? {
                                break;
                            }
                        }
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}
