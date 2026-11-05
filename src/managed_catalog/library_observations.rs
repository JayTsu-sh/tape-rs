//! Owner-fenced historical observations, never an authority to issue device I/O.
//! Only semantic changes append events; poll timestamps remain in the cache.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const MAX_RECORD: usize = 90_000;
const MAX_ITEMS: usize = 4096;
const MAX_OWNER_EPOCH: u64 = 0xFFFF_FFFF_FFFF;
const MAX_MONITOR_RECORD: usize = 16 * 1024;
const INVENTORY_ENTITY: &str = "library-inventory";
const MONITOR_ENTITY: &str = "library-monitor";
const INVENTORY_EVENT: &str = "library.inventory.changed";
const MONITOR_EVENT: &str = "library.monitor.changed";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Volume {
    barcode: String,
    address: u16,
    element_type: u8,
    medium_type: Option<u8>,
    source_address: Option<u16>,
    home_slot: Option<u32>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Drive {
    serial: String,
    address: u16,
    barcode: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    library_serial: String,
    owner: u64,
    round: u64,
    observed_at: String,
    volumes: Vec<Volume>,
    drives: Vec<Drive>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryRecord {
    version: u8,
    namespace: String,
    owner_epoch: u64,
    observation: Observation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorRecord {
    version: u8,
    namespace: String,
    library_serial: String,
    owner_epoch: u64,
    interval_seconds: u16,
    last_attempt: String,
    last_success: Option<String>,
    error: Option<String>,
}
fn valid_time(value: &str) -> bool {
    value.len() <= 64 && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}
fn cache_key(ns: Uuid, serial: &str, kind: &[u8]) -> Vec<u8> {
    let mut key = owner_key(serial);
    key.extend_from_slice(kind);
    key.extend_from_slice(ns.as_bytes());
    key
}
fn inventory_key(ns: Uuid, serial: &str) -> Vec<u8> {
    cache_key(ns, serial, b"/inventory/")
}
fn monitor_key(ns: Uuid, serial: &str) -> Vec<u8> {
    cache_key(ns, serial, b"/monitor/")
}
fn json_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value)
        .expect("typed observation contains only JSON scalars and collections")
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let raw = json_value(value).to_string().into_bytes();
    if raw.len() > MAX_RECORD {
        return Err(CatalogError::Invalid("完整带库观测超过单条FDB记录上限"));
    }
    Ok(raw)
}
fn normalize(value: &mut Observation, serial: &str, owner: u64) -> Result<()> {
    validate_physical_serial(serial)?;
    if value.library_serial != serial
        || owner == 0
        || owner > MAX_OWNER_EPOCH
        || value.round != owner
        || value.owner == 0
        || !valid_time(&value.observed_at)
        || value.volumes.len() > MAX_ITEMS
        || value.drives.len() > MAX_ITEMS
    {
        return Err(CatalogError::Invalid("带库观测身份、时间或条目数无效"));
    }
    let mut barcodes = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for v in &value.volumes {
        validate_barcode(&v.barcode)?;
        if !barcodes.insert(&v.barcode)
            || !addresses.insert(v.address)
            || !(1..=4).contains(&v.element_type)
            || v.home_slot.is_some_and(|n| n == 0 || v.element_type != 2)
        {
            return Err(CatalogError::Invalid(
                "带库观测条码、地址重复或元素类型无效",
            ));
        }
    }
    let mut serials = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for d in &value.drives {
        if d.serial.is_empty()
            || d.serial.chars().any(char::is_control)
            || d.serial.len() > 255
            || d.serial.contains('/')
            || d.serial.chars().any(char::is_whitespace)
        {
            return Err(CatalogError::Invalid("驱动器序列号无效"));
        }
        if !serials.insert(&d.serial) || !addresses.insert(d.address) {
            return Err(CatalogError::Invalid("带库观测驱动器身份或地址重复"));
        }
        if let Some(barcode) = &d.barcode {
            validate_barcode(barcode)?;
        }
        let volume = value.volumes.iter().find(|v| v.address == d.address);
        match (volume, d.barcode.as_deref()) {
            (None, None) => {}
            (Some(v), Some(barcode)) if v.element_type == 4 && v.barcode == barcode => {}
            _ => return Err(CatalogError::Invalid("驱动器装载状态与完整库存观测不一致")),
        }
    }
    value.volumes.sort_by_key(|v| v.address);
    value.drives.sort_by_key(|d| d.address);
    Ok(())
}
fn fields(value: &Value, names: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|v| v.len() == names.len() && v.keys().all(|k| names.contains(&k.as_str())))
}
fn number(value: &Value, max: u64) -> bool {
    value.as_u64().is_some_and(|n| n <= max)
}
fn optional_number(value: &Value, max: u64) -> bool {
    value.is_null() || number(value, max)
}
fn input(serial: &str, owner: u64, value: &Value) -> Result<Observation> {
    // Validate scalar shape and limits before cloning any input subtree.
    let volumes = value["volumes"]
        .as_array()
        .ok_or(CatalogError::Invalid("库存volumes必须是数组"))?;
    let drives = value["drives"]
        .as_array()
        .ok_or(CatalogError::Invalid("库存drives必须是数组"))?;
    if !fields(
        value,
        &[
            "library_serial",
            "owner",
            "round",
            "observed_at",
            "volumes",
            "drives",
        ],
    ) || volumes.len() > MAX_ITEMS
        || drives.len() > MAX_ITEMS
        || !number(&value["owner"], u64::MAX)
        || !number(&value["round"], u64::MAX)
        || value["library_serial"]
            .as_str()
            .is_none_or(|s| s.len() > 64)
        || value["observed_at"].as_str().is_none_or(|s| s.len() > 64)
        || volumes.iter().any(|v| {
            !fields(
                v,
                &[
                    "barcode",
                    "address",
                    "element_type",
                    "medium_type",
                    "source_address",
                    "home_slot",
                ],
            ) || v["barcode"].as_str().is_none_or(|s| s.len() > 64)
                || !number(&v["address"], u16::MAX.into())
                || !number(&v["element_type"], u8::MAX.into())
                || !optional_number(&v["medium_type"], u8::MAX.into())
                || !optional_number(&v["source_address"], u16::MAX.into())
                || !optional_number(&v["home_slot"], u32::MAX.into())
        })
        || drives.iter().any(|d| {
            !fields(d, &["serial", "address", "barcode"])
                || d["serial"].as_str().is_none_or(|s| s.len() > 255)
                || !number(&d["address"], u16::MAX.into())
                || !(d["barcode"].is_null() || d["barcode"].as_str().is_some_and(|s| s.len() <= 64))
        })
    {
        return Err(CatalogError::Invalid("带库观测字段或大小无效"));
    }
    let mut observation: Observation = serde_json::from_value(value.clone())
        .map_err(|_| CatalogError::Invalid("带库观测字段类型无效"))?;
    normalize(&mut observation, serial, owner)?;
    Ok(observation)
}
fn inventory_view(observation: &Observation) -> Value {
    let physical = json!({"volumes":observation.volumes,"drives":observation.drives});
    json!({"num_of_volumes":observation.volumes.len(),"num_of_drives":observation.drives.len(),
        "snapshot_sha256":format!("{:x}",Sha256::digest(physical.to_string()))})
}
fn monitor_view(record: &MonitorRecord) -> Value {
    json!({"error":record.error,"status":if record.error.is_some(){"error"}else{"ok"}})
}
async fn inventory_at(tr: &Transaction, ns: Uuid, serial: &str) -> Result<Option<InventoryRecord>> {
    let raw = tr.get(&inventory_key(ns, serial), false).await?;
    let record = if let Some(raw) = raw {
        if raw.len() > MAX_RECORD {
            return Err(CatalogError::Corrupt("带库观测记录超过上限"));
        }
        let mut r: InventoryRecord =
            serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("带库观测编码"))?;
        if r.version != 1 || r.namespace != ns.to_string() {
            return Err(CatalogError::Corrupt("带库观测作用域"));
        }
        normalize(&mut r.observation, serial, r.owner_epoch)
            .map_err(|_| CatalogError::Corrupt("带库观测身份或内容"))?;
        if encode(&r)
            .map_err(|_| CatalogError::Corrupt("带库观测大小"))?
            .as_slice()
            != raw.as_ref()
        {
            return Err(CatalogError::Corrupt("带库观测非规范编码"));
        }
        Some(r)
    } else {
        None
    };
    let before = record.as_ref().map(|r| inventory_view(&r.observation));
    notifications::require_changed_at(
        tr,
        ns,
        serial,
        INVENTORY_ENTITY,
        INVENTORY_EVENT,
        before.as_ref(),
    )
    .await?;
    Ok(record)
}
async fn monitor_at(tr: &Transaction, ns: Uuid, serial: &str) -> Result<Option<MonitorRecord>> {
    let raw = tr.get(&monitor_key(ns, serial), false).await?;
    let record = if let Some(raw) = raw {
        if raw.len() > MAX_MONITOR_RECORD {
            return Err(CatalogError::Corrupt("带库巡检记录超过上限"));
        }
        let r: MonitorRecord =
            serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("带库巡检记录编码"))?;
        if r.version != 1
            || r.namespace != ns.to_string()
            || r.library_serial != serial
            || r.owner_epoch == 0
            || r.owner_epoch > MAX_OWNER_EPOCH
            || r.interval_seconds != 30
            || !valid_time(&r.last_attempt)
            || (r.error.is_none() && r.last_success.as_deref() != Some(r.last_attempt.as_str()))
            || r.last_success.as_ref().is_some_and(|s| !valid_time(s))
            || r.error.as_ref().is_some_and(|s| s.len() > 2048)
            || encode(&r)
                .map_err(|_| CatalogError::Corrupt("带库巡检大小"))?
                .as_slice()
                != raw.as_ref()
        {
            return Err(CatalogError::Corrupt("带库巡检作用域、内容或规范编码"));
        }
        Some(r)
    } else {
        None
    };
    let before = record.as_ref().map(monitor_view);
    notifications::require_changed_at(
        tr,
        ns,
        serial,
        MONITOR_ENTITY,
        MONITOR_EVENT,
        before.as_ref(),
    )
    .await?;
    Ok(record)
}
pub(super) async fn observations_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<(Option<Value>, Option<Value>)> {
    let inventory = inventory_at(tr, ns, serial)
        .await?
        .map(|r| json_value(&r.observation));
    let monitor = monitor_at(tr, ns, serial).await?.map(|r| json_value(&r));
    Ok((inventory, monitor))
}

pub(super) async fn rescan_result_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    owner: u64,
    captured_at: &str,
    error: Option<&str>,
) -> Result<Value> {
    if !valid_time(captured_at) || error.is_some_and(|s| s.len() > 2048) {
        return Err(CatalogError::Invalid("rescan采样时间或诊断长度无效"));
    }
    let monitor = monitor_at(tr, ns, serial)
        .await?
        .ok_or(CatalogError::Corrupt("rescan巡检回执尚未发布"))?;
    if monitor.owner_epoch != owner
        || monitor.last_attempt != captured_at
        || monitor.error.as_deref() != error
    {
        return Err(CatalogError::Corrupt("rescan巡检事实与原采样不符"));
    }
    if let Some(error) = error {
        return Ok(json!({"captured_at":captured_at,"error":error}));
    }
    let inventory = inventory_at(tr, ns, serial)
        .await?
        .ok_or(CatalogError::Corrupt("rescan完整库存尚未发布"))?;
    if inventory.owner_epoch != owner || inventory.observation.observed_at != captured_at {
        return Err(CatalogError::Corrupt("rescan库存事实与原采样不符"));
    }
    let mut result = inventory_view(&inventory.observation);
    result["captured_at"] = json!(captured_at);
    result["node"] = json!(inventory.observation.owner);
    result["round"] = json!(inventory.observation.round);
    Ok(result)
}

impl ManagedCatalog {
    /// Persist one complete observation from a current, externally guarded owner.
    /// Identical physical inventory updates its timestamp without another event.
    pub async fn record_active_library_inventory(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        value: &Value,
    ) -> Result<()> {
        let observation = input(serial, owner, value)?;
        let record = InventoryRecord {
            version: 1,
            namespace: route.route.namespace.to_string(),
            owner_epoch: owner,
            observation,
        };
        let raw = encode(&record)?;
        let record = &record;
        let raw = raw.as_slice();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let old = inventory_at(&tr, ns, serial).await?;
                        let before = old.as_ref().map(|r| inventory_view(&r.observation));
                        notifications::changed_at(
                            &tr,
                            ns,
                            serial,
                            INVENTORY_ENTITY,
                            INVENTORY_EVENT,
                            before.as_ref(),
                            inventory_view(&record.observation),
                        )
                        .await?;
                        tr.set(&inventory_key(ns, serial), raw);
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }
    /// Return a cached observation with its original node, round and timestamp.
    /// A later owner may have taken over; this is never proof of current location.
    pub async fn active_library_inventory(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        Ok(inventory_at(&tr, ns, serial)
            .await?
            .map(|r| json_value(&r.observation)))
    }
    /// Record a completed monitor attempt. The actual periodic device poller is
    /// separate; an absent error must only be supplied after a successful poll.
    /// Capture the completion timestamp before submitting; retries preserve it.
    pub async fn record_library_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        captured_at: &str,
        error: Option<&str>,
    ) -> Result<()> {
        if owner == 0
            || owner > MAX_OWNER_EPOCH
            || !valid_time(captured_at)
            || error.is_some_and(|s| s.len() > 2048)
        {
            return Err(CatalogError::Invalid(
                "带库巡检轮次、采样时间或错误长度无效",
            ));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let old = monitor_at(&tr, ns, serial).await?;
                        let record = MonitorRecord {
                            version: 1,
                            namespace: ns.to_string(),
                            library_serial: serial.into(),
                            owner_epoch: owner,
                            interval_seconds: 30,
                            last_attempt: captured_at.into(),
                            last_success: if error.is_none() {
                                Some(captured_at.into())
                            } else {
                                old.as_ref().and_then(|r| r.last_success.clone())
                            },
                            error: error.map(str::to_owned),
                        };
                        let raw = encode(&record)?;
                        if raw.len() > MAX_MONITOR_RECORD {
                            return Err(CatalogError::Invalid("带库巡检记录超过上限"));
                        }
                        let before = old.as_ref().map(monitor_view);
                        notifications::changed_at(
                            &tr,
                            ns,
                            serial,
                            MONITOR_ENTITY,
                            MONITOR_EVENT,
                            before.as_ref(),
                            monitor_view(&record),
                        )
                        .await?;
                        tr.set(&monitor_key(ns, serial), &raw);
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }
    /// Stored attempt timestamps and outcome only. Callers must assess freshness
    /// and current owner before displaying real-time health.
    pub async fn active_library_monitor(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        Ok(monitor_at(&tr, ns, serial).await?.map(|r| json_value(&r)))
    }
}

#[cfg(test)]
#[path = "library_observations_tests.rs"]
mod tests;
