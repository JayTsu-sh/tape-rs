//! Logical resource assignments are metadata. This module never operates devices.

use super::*;
use crate::logical_library::{
    CreateLogicalLibrary, LogicalLibraryResources, MAX_REQUEST_BYTES, MAX_RESOURCES, valid_name,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug)]
pub(crate) enum Creation {
    Created(Value),
    Replayed(Value),
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

fn definition(record: &CreateLogicalLibrary) -> Value {
    json!({"version":record.version,"id":record.id,"name":record.name,"revision":1,
        "resources":record.resources})
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
        let status = if !fresh || !owner_current {
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
                                    Creation::Replayed(definition(&original))
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
                            let Some(physical) = inventory.resources else {
                                return Ok(Creation::InventoryUnavailable);
                            };
                            ensure_subset(&request.resources, &physical)?;
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
                            Ok(Creation::Created(definition(request)))
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
            return Ok(Some(json!({"logical_library":definition(&record),
                "inventory":inventory.view(Some(&record.resources))})));
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
        Ok(Some(
            json!({"items": records.iter().map(definition).collect::<Vec<_>>(),
            "inventory": inventory.view(None), "unallocated_resources": available}),
        ))
    }
}
