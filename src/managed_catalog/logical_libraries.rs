//! Logical resource assignments are metadata. This module never operates devices.

use super::*;
use crate::logical_library::{
    ChangeLogicalLibrary, CreateLogicalLibrary, LogicalLibraryResources, MAX_REQUEST_BYTES,
    MAX_RESOURCES, valid_name,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug)]
pub(crate) enum Creation {
    Created(Value),
    Replayed(Value),
    Conflict(&'static str),
    InventoryUnavailable,
}

pub(crate) enum Change {
    Applied(Value),
    Conflict(&'static str),
    InventoryUnavailable,
}

fn at(ns: Uuid, serial: &str, kind: u8) -> Vec<u8> {
    let mut key = owner_key(serial);
    key.extend_from_slice(b"/logical-libraries/");
    key.extend_from_slice(ns.as_bytes());
    key.push(kind);
    key
}

fn by_id(mut key: Vec<u8>, id: Uuid) -> Vec<u8> {
    key.extend_from_slice(id.as_bytes());
    key
}

fn by_name(mut key: Vec<u8>, name: &str) -> Vec<u8> {
    append_component(&mut key, name.as_bytes());
    key
}

fn drive_key(prefix: &[u8], drive: &str) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.push(b'd');
    append_component(&mut key, drive.as_bytes());
    key
}

fn slot_key(prefix: &[u8], kind: u8, addr: u16) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.push(kind);
    key.extend_from_slice(&addr.to_be_bytes());
    key
}

fn resource_keys<'a>(
    prefix: &'a [u8],
    resources: &'a LogicalLibraryResources,
) -> impl Iterator<Item = Vec<u8>> + 'a {
    resources
        .drives
        .iter()
        .map(move |drive| drive_key(prefix, drive))
        .chain(
            resources
                .storage_slots
                .iter()
                .map(move |&addr| slot_key(prefix, b's', addr)),
        )
        .chain(
            resources
                .ie_slots
                .iter()
                .map(move |&addr| slot_key(prefix, b'i', addr)),
        )
}

fn decode(id: Uuid, raw: &[u8]) -> Result<CreateLogicalLibrary> {
    let error = || CatalogError::Corrupt("逻辑带库记录编码、身份或资源无效");
    if raw.len() > MAX_REQUEST_BYTES {
        return Err(error());
    }
    let mut record: CreateLogicalLibrary = serde_json::from_slice(raw).map_err(|_| error())?;
    record.normalize().map_err(|_| error())?;
    if record.id != id || serde_json::to_vec(&record).map_err(|_| error())? != raw {
        return Err(error());
    }
    Ok(record)
}

fn definition(record: &CreateLogicalLibrary, revision: u64) -> Value {
    json!({"version":record.version,"id":record.id,"name":record.name,"revision":revision,
        "resources":record.resources})
}

async fn revision_at(tr: &Transaction, ns: Uuid, serial: &str, id: Uuid) -> Result<u64> {
    let raw = tr
        .get(&by_id(at(ns, serial, b'v'), id), false)
        .await?
        .ok_or(CatalogError::Corrupt("逻辑带库缺少修订号"))?;
    if raw.len() != 8 || read_u64(&raw) == 0 {
        return Err(CatalogError::Corrupt("逻辑带库修订号无效"));
    }
    Ok(read_u64(&raw))
}

pub(in crate::managed_catalog) async fn require_library_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    id: Uuid,
) -> Result<()> {
    let raw = tr
        .get(&by_id(at(ns, serial, b'l'), id), false)
        .await?
        .ok_or(CatalogError::Invalid("目标逻辑带库不存在"))?;
    decode(id, &raw)?;
    Ok(())
}

pub(super) async fn require_pool_scope_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
    logical: Option<Uuid>,
) -> Result<()> {
    if logical.is_some() {
        require_pool_library(tr, ns, pool, serial).await?;
    }
    if pool_library_at(tr, ns, serial, pool).await? != logical {
        return Err(CatalogError::Invalid("介质请求的池不属于指定逻辑带库范围"));
    }
    Ok(())
}

pub(super) async fn mechanical_resources_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    request: &ManagementRequest,
) -> Result<Option<LogicalLibraryResources>> {
    let change = &request.value()["change"];
    let logical = request.logical_library();
    if let Some(pool) = change["pool"].as_str() {
        let pool = Uuid::parse_str(pool).expect("validated pool");
        require_pool_scope_at(tr, ns, serial, pool, logical).await?;
    }
    let Some(id) = logical else { return Ok(None) };
    let raw = tr
        .get(&by_id(at(ns, serial, b'l'), id), false)
        .await?
        .ok_or(CatalogError::Invalid("目标逻辑带库不存在"))?;
    revision_at(tr, ns, serial, id).await?;
    Ok(Some(decode(id, &raw)?.resources))
}

pub(super) async fn require_mechanical_locations(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    request: &ManagementRequest,
    locations: &[&management::moves::MoveLocation],
    home: Option<u16>,
) -> Result<()> {
    use management::moves::MoveLocationKind;
    mechanical_resources_at(tr, ns, serial, request).await?;
    let expected = request.logical_library();
    let prefix = at(ns, serial, b'r');
    let mut keys = Vec::with_capacity(locations.len() + 1);
    keys.extend(home.map(|address| slot_key(&prefix, b's', address)));
    for location in locations {
        keys.push(match location.kind {
            MoveLocationKind::Storage => slot_key(&prefix, b's', location.address),
            MoveLocationKind::ImportExport => slot_key(&prefix, b'i', location.address),
            MoveLocationKind::Drive => {
                let drive = location
                    .drive_serial
                    .as_deref()
                    .ok_or(CatalogError::Corrupt("机械计划缺少驱动器序列号"))?;
                drive_key(&prefix, drive)
            }
        });
    }
    require_resource_claims(tr, keys, expected).await
}

async fn require_resource_claims(
    tr: &Transaction,
    keys: impl IntoIterator<Item = Vec<u8>>,
    expected: Option<Uuid>,
) -> Result<()> {
    for key in keys {
        let claim = tr.get(&key, false).await?;
        if claim.as_deref() != expected.as_ref().map(|id| id.as_bytes().as_slice()) {
            return Err(CatalogError::Invalid(
                "介质位置、驱动器或 home 超出指定逻辑带库范围",
            ));
        }
    }
    Ok(())
}

pub(super) async fn require_automatic_resource_claims(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
    drive: &str,
    storage: Option<u16>,
) -> Result<()> {
    let expected = pool_library_at(tr, ns, serial, pool).await?;
    let prefix = at(ns, serial, b'r');
    let keys = std::iter::once(drive_key(&prefix, drive))
        .chain(storage.map(|address| slot_key(&prefix, b's', address)));
    require_resource_claims(tr, keys, expected).await
}

pub(in crate::managed_catalog) fn pool_binding_key(ns: Uuid, serial: &str, pool: Uuid) -> Vec<u8> {
    by_id(at(ns, serial, b'p'), pool)
}

pub(super) fn pool_create_receipt_key(
    ns: Uuid,
    serial: &str,
    logical: Uuid,
    pool: Uuid,
) -> Vec<u8> {
    by_id(by_id(at(ns, serial, b'P'), logical), pool)
}

pub(in crate::managed_catalog) fn pool_membership_prefix(
    ns: Uuid,
    serial: &str,
    library: Uuid,
) -> Vec<u8> {
    by_id(at(ns, serial, b'm'), library)
}

pub(in crate::managed_catalog) fn pool_membership_key(
    ns: Uuid,
    serial: &str,
    library: Uuid,
    pool: Uuid,
) -> Vec<u8> {
    by_id(pool_membership_prefix(ns, serial, library), pool)
}

pub(in crate::managed_catalog) async fn pool_library_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
) -> Result<Option<Uuid>> {
    let Some(raw) = tr.get(&pool_binding_key(ns, serial, pool), false).await? else {
        return Ok(None);
    };
    let id = uuid_from(&raw)?;
    let indexed = tr
        .get(&pool_membership_key(ns, serial, id, pool), false)
        .await?;
    if id.is_nil() || !indexed.is_some_and(|value| value.is_empty()) {
        return Err(CatalogError::Corrupt("池与逻辑带库的归属索引不一致"));
    }
    Ok(Some(id))
}

pub(in crate::managed_catalog) async fn pool_resources_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    pool: Uuid,
) -> Result<Option<LogicalLibraryResources>> {
    let Some(id) = pool_library_at(tr, ns, serial, pool).await? else {
        return Ok(None);
    };
    let raw = tr
        .get(&by_id(at(ns, serial, b'l'), id), false)
        .await?
        .ok_or(CatalogError::Corrupt("池引用的逻辑带库缺少配置"))?;
    revision_at(tr, ns, serial, id).await?;
    Ok(Some(decode(id, &raw)?.resources))
}

pub(super) fn note_mechanical_observation_stale(tr: &Transaction, ns: Uuid, serial: &str) {
    tr.set(
        &at(ns, serial, b't'),
        crate::ltfs::label::ltfs_time_now().as_bytes(),
    );
}

struct Inventory {
    observation: Option<Value>,
    resources: Option<LogicalLibraryResources>,
    status: &'static str,
}

impl Inventory {
    async fn read(tr: &Transaction, ns: Uuid, serial: &str, epoch: u64) -> Result<Self> {
        let (observation, monitor) = library_observations::observations_at(tr, ns, serial).await?;
        let Some(observation) = observation else {
            return Ok(Self {
                observation: None,
                resources: None,
                status: "unavailable",
            });
        };
        let timestamp = observation["observed_at"]
            .as_str()
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
            .ok_or(CatalogError::Corrupt("逻辑带库库存观测时间无效"))?;
        let age = Utc::now().signed_duration_since(timestamp);
        let changed_after_observation =
            if let Some(value) = tr.get(&at(ns, serial, b't'), false).await? {
                let changed = std::str::from_utf8(&value)
                    .ok()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .ok_or(CatalogError::Corrupt("逻辑带库库存变更时间无效"))?;
                changed >= timestamp
            } else {
                false
            };
        let fresh = age >= TimeDelta::seconds(-5) && age <= TimeDelta::seconds(90);
        let owner_current = observation["round"].as_u64() == Some(epoch)
            && management::handoff::allows_owner(tr, ns, serial, epoch).await?;
        let recent_failure = monitor.is_some_and(|monitor| {
            monitor["owner_epoch"].as_u64() == Some(epoch)
                && !monitor["error"].is_null()
                && monitor["last_attempt"]
                    .as_str()
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .is_some_and(|attempt| attempt >= timestamp)
        });
        let mut resources = None;
        let status = if !fresh || !owner_current || changed_after_observation {
            "stale"
        } else if recent_failure || observation["slot_layout"].is_null() {
            "incomplete"
        } else {
            let slots = |start: &str, count: &str| {
                let start = observation["slot_layout"][start]
                    .as_u64()
                    .expect("validated layout");
                let count = observation[count].as_u64().expect("validated slot count");
                (start..start + count).map(|addr| addr as u16).collect()
            };
            resources = Some(LogicalLibraryResources {
                drives: observation["drives"]
                    .as_array()
                    .expect("validated drives")
                    .iter()
                    .map(|drive| {
                        drive["serial"]
                            .as_str()
                            .expect("validated drive serial")
                            .to_owned()
                    })
                    .collect(),
                storage_slots: slots("storage_start", "num_of_slots"),
                ie_slots: slots("ie_start", "num_of_ieslots"),
            });
            "current"
        };
        Ok(Self {
            observation: Some(observation),
            resources,
            status,
        })
    }

    fn view(&self, selection: Option<&LogicalLibraryResources>) -> Value {
        let Some(mut observation) = self.observation.clone() else {
            return json!({"status":self.status,"observation":null});
        };
        if let Some(selection) = selection {
            let drives = observation["drives"]
                .as_array_mut()
                .expect("validated drives");
            drives.retain(|drive| selection.drives.iter().any(|id| drive["serial"] == *id));
            let drive_addresses: BTreeSet<_> = drives
                .iter()
                .filter_map(|d| d["address"].as_u64())
                .collect();
            observation["volumes"]
                .as_array_mut()
                .expect("validated volumes")
                .retain(|medium| {
                    let addr = medium["address"].as_u64().expect("validated address");
                    match medium["element_type"].as_u64() {
                        Some(2) => selection.storage_slots.contains(&(addr as u16)),
                        Some(3) => selection.ie_slots.contains(&(addr as u16)),
                        Some(4) => drive_addresses.contains(&addr),
                        _ => false,
                    }
                });
        }
        json!({"status":self.status,"observation":observation})
    }
}

fn ensure_subset(
    selected: &LogicalLibraryResources,
    physical: &LogicalLibraryResources,
) -> Result<()> {
    if selected
        .drives
        .iter()
        .any(|id| !physical.drives.contains(id))
        || selected
            .storage_slots
            .iter()
            .any(|addr| !physical.storage_slots.contains(addr))
        || selected
            .ie_slots
            .iter()
            .any(|addr| !physical.ie_slots.contains(addr))
    {
        return Err(CatalogError::Invalid(
            "请求的逻辑带库资源不属于本物理带库或元素类型不符",
        ));
    }
    Ok(())
}

impl ManagedCatalog {
    pub(crate) async fn active_mechanical_resources(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
    ) -> Result<Option<LogicalLibraryResources>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        mechanical_resources_at(&tr, ns, serial, request).await
    }

    pub(crate) async fn active_logical_pool_resources(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        pool: Uuid,
    ) -> Result<Option<LogicalLibraryResources>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, epoch).await?;
        pool_resources_at(&tr, ns, serial, pool).await
    }

    pub(crate) async fn active_pool_logical_libraries(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        pools: &[PoolRecord],
    ) -> Result<Vec<Option<Uuid>>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let mut scopes = Vec::with_capacity(pools.len());
        let mut verified = BTreeSet::new();
        for pool in pools {
            let logical = pool_library_at(&tr, ns, serial, pool.id).await?;
            if let Some(id) = logical
                && verified.insert(id)
            {
                require_library_at(&tr, ns, serial, id).await?;
            }
            scopes.push(logical);
        }
        Ok(scopes)
    }

    pub(crate) async fn require_logical_pool_scope(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        pool: Uuid,
        logical: Option<Uuid>,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        if pool_library_at(&tr, ns, serial, pool).await? != logical {
            return Err(CatalogError::Invalid("池不属于请求的逻辑带库范围"));
        }
        if let Some(logical) = logical {
            require_library_at(&tr, ns, serial, logical).await?;
        }
        Ok(())
    }
    pub(crate) async fn create_logical_library(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        mut request: CreateLogicalLibrary,
    ) -> Result<Creation> {
        request.normalize().map_err(CatalogError::Invalid)?;
        let encoded = serde_json::to_vec(&request)
            .map_err(|_| CatalogError::Invalid("逻辑带库请求编码无效"))?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(CatalogError::Invalid("逻辑带库资源清单超过上限"));
        }
        self.db
            .run(|tr, _| {
                let request = &request;
                let encoded = &encoded;
                async move {
                    requests::retry_result(
                        async {
                            let ns =
                                routing::require_runtime_active_owner(&tr, serial, route, epoch)
                                    .await?;
                            let receipt = by_id(at(ns, serial, b'c'), request.id);
                            if let Some(raw) = tr.get(&receipt, false).await? {
                                let original = decode(request.id, &raw)?;
                                return Ok(if original == *request {
                                    Creation::Replayed(definition(&original, 1))
                                } else {
                                    Creation::Conflict("同一逻辑带库 UUID 已绑定不同的原请求")
                                });
                            }
                            let primary = by_id(at(ns, serial, b'l'), request.id);
                            if tr.get(&primary, false).await?.is_some() {
                                return Err(CatalogError::Corrupt("逻辑带库存在但缺少原创建回执"));
                            }
                            let name = by_name(at(ns, serial, b'n'), &request.name);
                            if tr.get(&name, false).await?.is_some() {
                                return Ok(Creation::Conflict("本物理带库已使用该逻辑带库名称"));
                            }
                            let inventory = Inventory::read(&tr, ns, serial, epoch).await?;
                            let Some(physical) = inventory.resources.as_ref() else {
                                return Ok(Creation::InventoryUnavailable);
                            };
                            ensure_subset(&request.resources, physical)?;
                            native_device::require_library_available(&tr, ns, serial).await?;
                            management::require_no_queued_tasks(&tr, ns, serial).await?;
                            if !resources_idle(&tr, ns, serial, &inventory, &request.resources)
                                .await?
                            {
                                return Ok(Creation::Conflict(
                                    "待划分资源仍有介质占用或 home 引用",
                                ));
                            }
                            let resources: Vec<_> =
                                resource_keys(&at(ns, serial, b'r'), &request.resources).collect();
                            for batch in resources.chunks(64) {
                                let reads: Vec<_> =
                                    batch.iter().map(|key| tr.get(key, false)).collect();
                                for read in reads {
                                    if read.await?.is_some() {
                                        return Ok(Creation::Conflict(
                                            "请求的资源已归属其他逻辑带库",
                                        ));
                                    }
                                }
                            }
                            for key in &resources {
                                tr.set(key, request.id.as_bytes());
                            }
                            tr.set(&name, request.id.as_bytes());
                            tr.set(&primary, encoded);
                            tr.set(&receipt, encoded);
                            tr.set(
                                &by_id(at(ns, serial, b'v'), request.id),
                                &1u64.to_be_bytes(),
                            );
                            Ok(Creation::Created(definition(request, 1)))
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub(crate) async fn logical_library_view(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        selector: Option<&str>,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let epoch = tr
            .get(&owner_key(serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if epoch.len() != 8 {
            return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
        }
        let prefix = at(ns, serial, b'l');
        if let Some(selector) = selector {
            let id = if let Ok(id) = Uuid::parse_str(selector) {
                if id.is_nil() || id.to_string() != selector {
                    return Err(CatalogError::Invalid("逻辑带库 UUID 无效"));
                }
                id
            } else {
                if !valid_name(selector) {
                    return Err(CatalogError::Invalid("逻辑带库名称无效"));
                }
                let key = by_name(at(ns, serial, b'n'), selector);
                let Some(raw) = tr.get(&key, false).await? else {
                    return Ok(None);
                };
                uuid_from(&raw)?
            };
            let Some(raw) = tr.get(&by_id(prefix, id), false).await? else {
                if selector == id.to_string() {
                    return Ok(None);
                }
                return Err(CatalogError::Corrupt("逻辑带库名称索引缺少主记录"));
            };
            let record = decode(id, &raw)?;
            if selector != record.name && selector != id.to_string() {
                return Err(CatalogError::Corrupt("逻辑带库名称索引与主记录不符"));
            }
            let inventory = Inventory::read(&tr, ns, serial, read_u64(&epoch)).await?;
            let revision = revision_at(&tr, ns, serial, id).await?;
            return Ok(Some(
                json!({"logical_library":definition(&record, revision),
                "inventory":inventory.view(Some(&record.resources))}),
            ));
        }

        let page = read_page(&tr, &prefix, None, MAX_RESOURCES, false).await?;
        if page.next.is_some() {
            return Err(CatalogError::Corrupt("逻辑带库数量超过库存容量上限"));
        }
        let records = page
            .items
            .into_iter()
            .map(|(key, raw)| {
                let id = uuid_from(&key[prefix.len()..])?;
                decode(id, &raw)
            })
            .collect::<Result<Vec<_>>>()?;
        let inventory = Inventory::read(&tr, ns, serial, read_u64(&epoch)).await?;
        let resource_prefix = at(ns, serial, b'r');
        let allocated = records
            .iter()
            .flat_map(|r| resource_keys(&resource_prefix, &r.resources))
            .collect::<BTreeSet<_>>();
        let available = inventory.resources.clone().map(|mut resources| {
            resources
                .drives
                .retain(|drive| !allocated.contains(&drive_key(&resource_prefix, drive)));
            for (kind, slots) in [
                (b's', &mut resources.storage_slots),
                (b'i', &mut resources.ie_slots),
            ] {
                slots.retain(|&addr| !allocated.contains(&slot_key(&resource_prefix, kind, addr)));
            }
            resources
        });
        let mut items = Vec::with_capacity(records.len());
        for record in &records {
            items.push(definition(
                record,
                revision_at(&tr, ns, serial, record.id).await?,
            ));
        }
        Ok(Some(json!({"items": items,
            "inventory": inventory.view(None), "unallocated_resources": available})))
    }
}

fn removed_resources(
    old: &LogicalLibraryResources,
    next: Option<&LogicalLibraryResources>,
) -> LogicalLibraryResources {
    let Some(next) = next else { return old.clone() };
    LogicalLibraryResources {
        drives: old
            .drives
            .iter()
            .filter(|id| !next.drives.contains(id))
            .cloned()
            .collect(),
        storage_slots: old
            .storage_slots
            .iter()
            .copied()
            .filter(|addr| !next.storage_slots.contains(addr))
            .collect(),
        ie_slots: old
            .ie_slots
            .iter()
            .copied()
            .filter(|addr| !next.ie_slots.contains(addr))
            .collect(),
    }
}

async fn resources_idle(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    inventory: &Inventory,
    resources: &LogicalLibraryResources,
) -> Result<bool> {
    Ok(!inventory.has_occupied_resource(resources)
        && !native_device::has_home_in(tr, ns, serial, &resources.storage_slots).await?
        && !management::moves::has_home_in(tr, ns, serial, &resources.storage_slots).await?)
}

impl Inventory {
    fn has_occupied_resource(&self, removed: &LogicalLibraryResources) -> bool {
        let view = self.view(Some(removed));
        view["observation"]["volumes"]
            .as_array()
            .is_none_or(|volumes| !volumes.is_empty())
            || self.observation.as_ref().is_some_and(|current| {
                current["volumes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|volume| {
                        volume["source_address"]
                            .as_u64()
                            .and_then(|n| u16::try_from(n).ok())
                            .is_some_and(|source| removed.storage_slots.contains(&source))
                    })
            })
    }
}

async fn saved_change_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    operation: Uuid,
) -> Result<Option<Value>> {
    let Some(raw) = tr
        .get(&by_id(at(ns, serial, b'o'), operation), false)
        .await?
    else {
        return Ok(None);
    };
    let result: Value = serde_json::from_slice(&raw)
        .map_err(|_| CatalogError::Corrupt("逻辑带库变更回执编码无效"))?;
    if result["operation"] != operation.to_string()
        || !result["fingerprint"]
            .as_str()
            .is_some_and(|v| v.len() == 64)
        || result["deleted"].as_bool().is_none()
        || !result["logical_library"].is_object()
    {
        return Err(CatalogError::Corrupt("逻辑带库变更回执身份或内容无效"));
    }
    Ok(Some(result))
}

impl ManagedCatalog {
    pub(crate) async fn logical_library_change_receipt(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: Uuid,
        operation: Uuid,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        Ok(saved_change_at(&tr, ns, serial, operation)
            .await?
            .filter(|saved| saved["logical_library"]["id"] == id.to_string()))
    }

    pub(crate) async fn change_logical_library(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        id: Uuid,
        request: ChangeLogicalLibrary,
    ) -> Result<Change> {
        let revision = request
            .revision
            .checked_add(1)
            .filter(|_| request.revision != 0)
            .ok_or(CatalogError::Invalid("逻辑带库修订号无效或已耗尽"))?;
        if id.is_nil() || request.version != 1 || request.operation.is_nil() {
            return Err(CatalogError::Invalid("逻辑带库变更版本或身份无效"));
        }
        let replacement = request
            .replacement
            .map(|replacement| {
                let mut desired = CreateLogicalLibrary {
                    version: 1,
                    id,
                    name: replacement.name,
                    resources: replacement.resources,
                };
                desired.normalize().map_err(CatalogError::Invalid)?;
                Ok::<_, CatalogError>(desired)
            })
            .transpose()?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                json!({"id":id,"revision":request.revision,"replacement":replacement}).to_string()
            )
        );
        self.db.run(|tr, _| {
            let replacement = &replacement;
            let fingerprint = &fingerprint;
            async move { requests::retry_result(async {
                let ns = routing::require_runtime_active_owner(&tr, serial, route, epoch).await?;
                if let Some(saved) = saved_change_at(&tr, ns, serial, request.operation).await? {
                    return Ok(if saved["fingerprint"] == *fingerprint {
                        Change::Applied(saved)
                    } else {
                        Change::Conflict("变更操作 UUID 已绑定不同请求")
                    });
                }
                let primary = by_id(at(ns, serial, b'l'), id);
                let Some(raw) = tr.get(&primary, false).await? else {
                    return Ok(Change::Conflict("目标逻辑带库不存在"));
                };
                let old = decode(id, &raw)?;
                if revision_at(&tr, ns, serial, id).await? != request.revision {
                    return Ok(Change::Conflict("逻辑带库修订号已改变"));
                }
                if replacement.is_none() && requests::prefix_has_rows(&tr, &pool_membership_prefix(ns, serial, id)).await? {
                    return Ok(Change::Conflict("逻辑带库仍有 pool，不能删除"));
                }
                let result_record = replacement.as_ref().unwrap_or(&old);
                let deleting = replacement.is_none();
                let resource_change = deleting || result_record.resources != old.resources;
                if resource_change {
                    let inventory = Inventory::read(&tr, ns, serial, epoch).await?;
                    let Some(physical) = inventory.resources.as_ref() else { return Ok(Change::InventoryUnavailable) };
                    ensure_subset(&old.resources, physical)?;
                    ensure_subset(&result_record.resources, physical)?;
                    native_device::require_library_available(&tr, ns, serial).await?;
                    management::require_no_queued_tasks(&tr, ns, serial).await?;
                    let removed = removed_resources(&old.resources, replacement.as_ref().map(|r| &r.resources));
                    if !resources_idle(&tr, ns, serial, &inventory, &removed).await? {
                        return Ok(Change::Conflict("待释放资源仍有介质占用或 home 引用"));
                    }
                    if let Some(replacement) = replacement {
                        let added = removed_resources(&replacement.resources, Some(&old.resources));
                        if !resources_idle(&tr, ns, serial, &inventory, &added).await? {
                            return Ok(Change::Conflict("待加入资源仍有介质占用或 home 引用"));
                        }
                    }
                }

                let old_name = by_name(at(ns, serial, b'n'), &old.name);
                let new_name = by_name(at(ns, serial, b'n'), &result_record.name);
                if tr.get(&old_name, false).await?.as_deref() != Some(id.as_bytes().as_slice()) {
                    return Err(CatalogError::Corrupt("逻辑带库名称索引与主记录不符"));
                }
                if new_name != old_name && tr.get(&new_name, false).await?.is_some() {
                    return Ok(Change::Conflict("本物理带库已使用该逻辑带库名称"));
                }
                let prefix = at(ns, serial, b'r');
                let old_keys: BTreeSet<_> = resource_keys(&prefix, &old.resources).collect();
                let new_keys: BTreeSet<_> = replacement.iter().flat_map(|next| resource_keys(&prefix, &next.resources)).collect();
                for key in old_keys.difference(&new_keys) {
                    if tr.get(key, false).await?.as_deref() != Some(id.as_bytes().as_slice()) {
                        return Err(CatalogError::Corrupt("待释放资源的逻辑带库归属不符"));
                    }
                }
                for key in new_keys.difference(&old_keys) {
                    if tr.get(key, false).await?.is_some() {
                        return Ok(Change::Conflict("请求的资源已归属其他逻辑带库"));
                    }
                }
                let receipt = json!({"operation":request.operation,"deleted":deleting,
                    "logical_library":definition(result_record, revision),"fingerprint":fingerprint});
                let encoded = serde_json::to_vec(result_record).expect("logical library serializes");
                let receipt_bytes = receipt.to_string();
                if encoded.len() > MAX_REQUEST_BYTES || receipt_bytes.len() > MAX_REQUEST_BYTES + 1024 {
                    return Err(CatalogError::Invalid("逻辑带库变更或回执超过大小上限"));
                }
                for key in old_keys.difference(&new_keys) { tr.clear(key); }
                for key in new_keys.difference(&old_keys) { tr.set(key, id.as_bytes()); }
                if deleting || old_name != new_name { tr.clear(&old_name); }
                if deleting { tr.clear(&primary); } else {
                    tr.set(&primary, &encoded);
                    tr.set(&new_name, id.as_bytes());
                }
                tr.set(&by_id(at(ns, serial, b'v'), id), &revision.to_be_bytes());
                tr.set(&by_id(at(ns, serial, b'o'), request.operation), receipt_bytes.as_bytes());
                Ok(Change::Applied(receipt))
            }.await) }
        }).await?
    }
}
