//! Durable notifications: immutable events and explicit, consumer-specific ACKs.
//! No network I/O or process-local delivery authority belongs in this module.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const EVENT_LIMIT: usize = 90_000;
const BATCH_LIMIT: usize = 16;
const BATCH_BYTES: usize = 192 * 1024;
const EVENT_TYPES: &[&str] = &[
    "tape.state.changed",
    "tape.location.changed",
    "tape.assignment.changed",
    "tape.alert.raised",
    "library.monitor.changed",
    "library.inventory.changed",
    "task.status.changed",
];
fn selected_types(filter: &NotifyFilter) -> Vec<&str> {
    if filter.event_types.is_empty() {
        EVENT_TYPES.to_vec()
    } else {
        filter.event_types.iter().map(String::as_str).collect()
    }
}
fn initial_type_starts(filter: &NotifyFilter, start: u64) -> Value {
    Value::Object(
        selected_types(filter)
            .into_iter()
            .map(|kind| (kind.to_owned(), json!(start)))
            .collect(),
    )
}
async fn widen_type_starts(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    consumer: &mut Value,
    next: &NotifyFilter,
) -> Result<()> {
    let boundary = head_at(tr, ns, serial).await?;
    let starts = consumer["type_starts"]
        .as_object_mut()
        .ok_or(CatalogError::Corrupt("事件类型起点缺失"))?;
    for kind in selected_types(next) {
        starts
            .entry(kind.to_owned())
            .or_insert_with(|| json!(boundary));
    }
    Ok(())
}

/// Observational connection liveness only; never expires events or ACK state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyConnectionAction {
    Open,
    Heartbeat,
    Sent,
    Closed,
}

fn type_expansion(old: &[String], new: &[String]) -> bool {
    new.is_empty() || (!old.is_empty() && old.iter().all(|kind| new.contains(kind)))
}
fn same_types(old: &[String], new: &[String]) -> bool {
    old.iter().collect::<std::collections::BTreeSet<_>>() == new.iter().collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyFilter {
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub barcodes: Vec<String>,
    #[serde(default)]
    pub event_types: Vec<String>,
}

impl NotifyFilter {
    fn validate(&self) -> Result<()> {
        if self.pool.as_ref().is_some_and(|id| {
            Uuid::parse_str(id)
                .ok()
                .is_none_or(|v| v.is_nil() || v.to_string() != *id)
        }) || self.barcodes.len() > 64
            || self.event_types.len() > 16
        {
            return Err(CatalogError::Invalid("通知过滤条件超限或池身份无效"));
        }
        for barcode in &self.barcodes {
            validate_barcode(barcode)?;
        }
        for kind in &self.event_types {
            if !matches!(
                kind.as_str(),
                "tape.state.changed"
                    | "tape.location.changed"
                    | "tape.assignment.changed"
                    | "tape.alert.raised"
                    | "library.monitor.changed"
                    | "library.inventory.changed"
                    | "task.status.changed"
            ) {
                return Err(CatalogError::Invalid("未知通知事件类型"));
            }
        }
        Ok(())
    }
    fn matches(&self, event: &Value) -> bool {
        let pool = self.pool.clone();
        let data = &event["data"];
        (pool.as_ref().is_none_or(|p| {
            data["pool"] == *p || data["previous"]["pool"] == *p || data["current"]["pool"] == *p
        })) && (self.barcodes.is_empty() || self.barcodes.iter().any(|b| data["barcode"] == *b))
            && (self.event_types.is_empty()
                || self.event_types.iter().any(|k| event["event_type"] == *k))
    }
}

fn base(ns: Uuid, serial: &str, kind: u8) -> Vec<u8> {
    let mut k = key(b'~');
    k.extend_from_slice(ns.as_bytes());
    append_component(&mut k, serial.as_bytes());
    k.push(kind);
    k
}
fn event_key(ns: Uuid, serial: &str, sequence: u64) -> Vec<u8> {
    let mut k = base(ns, serial, b'e');
    k.extend_from_slice(&sequence.to_be_bytes());
    k
}
fn entity_key(ns: Uuid, serial: &str, kind: u8, entity: &str) -> Vec<u8> {
    let mut k = base(ns, serial, kind);
    append_component(&mut k, entity.as_bytes());
    k
}
fn decode(raw: &[u8]) -> Result<Value> {
    serde_json::from_slice(raw).map_err(|_| CatalogError::Corrupt("通知记录 JSON 损坏"))
}
async fn head_at(tr: &Transaction, ns: Uuid, serial: &str) -> Result<u64> {
    match tr.get(&base(ns, serial, b'h'), false).await? {
        None => Ok(0),
        Some(v) if v.len() == 8 => Ok(read_u64(&v)),
        _ => Err(CatalogError::Corrupt("通知日志位置损坏")),
    }
}

// Existing control writers commit domain-error results. A notification failure
// after their writes must instead abort the transaction, including corruption.
pub(super) fn atomic_error(error: CatalogError) -> CatalogError {
    match error {
        CatalogError::Fdb(_) | CatalogError::Client(_) => error,
        other => CatalogError::Fdb(FdbBindingError::CustomError(Box::new(other))),
    }
}

async fn append_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    kind: &str,
    data: Value,
) -> Result<u64> {
    append_event_at(tr, ns, serial, kind, data, None).await
}
async fn append_event_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    kind: &str,
    data: Value,
    injection: Option<Uuid>,
) -> Result<u64> {
    let result = async {
        let sequence = head_at(tr, ns, serial)
            .await?
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("通知序号耗尽"))?;
        let event = json!({"version":1,"event_id":format!("{ns}:{serial}:{sequence}"),
            "event_type":kind,"sequence":sequence,"namespace":ns.to_string(),"library":serial,
            "observed_at":crate::ltfs::label::ltfs_time_now(),"data":data,
            "origin":if injection.is_some(){"injected"}else{"tape-rs"},"injected":injection.is_some(),"injection_id":injection.map(|id|id.to_string())});
        let raw = event.to_string();
        if raw.len() > EVENT_LIMIT {
            return Err(CatalogError::Invalid("通知事件超过记录上限"));
        }
        tr.set(&event_key(ns, serial, sequence), raw.as_bytes());
        tr.set(&base(ns, serial, b'h'), &sequence.to_be_bytes());
        Ok(sequence)
    }
    .await;
    result.map_err(atomic_error)
}

/// Shared by HTTP inventory and durable tape-state events. Physical absence is
/// meaningful only when the cached inventory belongs to the current owner.
pub fn tape_inventory_state<'a>(
    data: &'a Value,
    inventory: &Value,
    owner_matches: bool,
) -> &'a str {
    let barcode = &data["barcode"];
    let locations: Vec<_> = inventory["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["barcode"] == *barcode)
        .collect();
    let cleaning = crate::changer::element::cleaning_source(
        locations
            .first()
            .and_then(|v| v["medium_type"].as_u64())
            .and_then(|v| v.try_into().ok()),
        barcode.as_str(),
    )
    .is_some();
    let assignment = &data["assignment"];
    let monitor = &data["monitor"];
    if locations.len() > 1 {
        "duplicated"
    } else if cleaning {
        "non_supported"
    } else if assignment["blacklisted"] == true {
        "blacklisted"
    } else if assignment["online"] == false {
        "offline"
    } else if monitor["media_fault"].is_string() {
        monitor["state"].as_str().unwrap_or("unknown")
    } else if assignment.is_null() {
        "unassigned"
    } else if !data["task_id"].is_null() {
        "importing"
    } else if assignment["volume"].is_null() {
        "assigned_unverified"
    } else if locations.is_empty() && owner_matches {
        "missing"
    } else {
        monitor["state"].as_str().unwrap_or("unknown")
    }
}

pub(super) async fn tape_changed_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
    reason: &str,
) -> Result<()> {
    let result = async {
        // Staged imports have no active virtual pool roots yet. Notifications
        // begin with the live management model, never with provisional imports.
        let Some(active) = tr.get(&routing::active_key(serial), false).await? else { return Ok(()); };
        if routing::decode_active(&active)?.route.namespace != ns { return Ok(()); }
        let data = inventory::inventory_entry(tr, serial, ns, barcode).await?
            .unwrap_or_else(|| json!({"barcode":barcode,"assignment":null,"pool":null}));
        let observation = inventory::read_at(tr, serial, ns).await?.unwrap_or(Value::Null);
        let owner = tr.get(&owner_key(serial), false).await?;
        let owner_matches = owner.as_deref().is_some_and(|v| v.len() == 8
            && observation["round"].as_u64() == Some(read_u64(v)));
        let state = tape_inventory_state(&data, &observation, owner_matches);
        let locations: Vec<_> = observation["volumes"].as_array().into_iter().flatten()
            .filter(|v| v["barcode"] == barcode)
            .map(|v| json!({"address":v["address"],"element_type":v["element_type"],"medium_type":v["medium_type"]})).collect();
        let monitor = &data["monitor"];
        let sample = &monitor["sample"];
        // Timestamps, capacity byte counters, owner epochs and transient progress
        // do not create repeated state events. Alert history is deliberately sticky.
        let current = json!({"state":state,"pool":data["pool"]["id"],
            "volume_uuid":data["assignment"]["volume"],"online":data["assignment"]["online"],
            "blacklisted":data["assignment"]["blacklisted"],
            "offline_message":data["assignment"]["offline_message"],"locations":locations,
            "write_protected":sample["write_protected"],"alert_history":monitor["alert_history"],
            "alert_history_details":monitor["alert_history_details"],"media_fault_evidence":monitor["media_fault_evidence"],
            "media_fault":monitor["media_fault"],"import_required":monitor["import_required"],
            "requires_validation":monitor["requires_validation"],"task_id":data["task_id"]});
        let k = entity_key(ns, serial, b't', barcode);
        let previous = match tr.get(&k, false).await? { Some(v) => decode(&v)?, None => Value::Null };
        if previous == current { return Ok(()); }
        let data = json!({"barcode":barcode,"pool":current["pool"],
            "previous":previous,"current":current,"reason":reason});
        // A state subscriber must also see transitions that coincide with a
        // pool move, a new alert or a physical location change.
        append_at(tr, ns, serial, "tape.state.changed", data.clone()).await?;
        if !previous.is_null() {
            for (changed, kind) in [
                (previous["pool"] != current["pool"] || previous["volume_uuid"] != current["volume_uuid"], "tape.assignment.changed"),
                (previous["alert_history"] != current["alert_history"], "tape.alert.raised"),
                (previous["locations"] != current["locations"], "tape.location.changed"),
            ] {
                if changed { append_at(tr, ns, serial, kind, data.clone()).await?; }
            }
        }
        tr.set(&k, current.to_string().as_bytes());
        Ok(())
    }.await;
    result.map_err(atomic_error)
}

pub(super) async fn changed_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    entity: &str,
    kind: &str,
    current: Value,
) -> Result<()> {
    let result = async {
        let k = entity_key(ns, serial, b's', entity);
        let previous = match tr.get(&k, false).await? {
            Some(v) => decode(&v)?,
            None => Value::Null,
        };
        if current != previous {
            append_at(
                tr,
                ns,
                serial,
                kind,
                json!({"previous":previous,"current":current,
                "pool":current["pool"],"barcode":current["barcode"]}),
            )
            .await?;
            tr.set(&k, current.to_string().as_bytes());
        }
        Ok(())
    }
    .await;
    result.map_err(atomic_error)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(CatalogError::Invalid(
            "消费者标识须为 1..=64 位字母、数字、横线或下划线",
        ));
    }
    Ok(())
}
async fn consumer_at(tr: &Transaction, ns: Uuid, serial: &str, id: &str) -> Result<Value> {
    let raw = tr
        .get(&entity_key(ns, serial, b'c', id), false)
        .await?
        .ok_or(CatalogError::Invalid("通知消费者不存在"))?;
    let value = decode(&raw)?;
    if value["version"] != 1
        || value["namespace"] != ns.to_string()
        || value["library"] != serial
        || value["id"] != id
        || value["generation"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .is_none()
        || value["acknowledged"].as_u64().is_none()
        || value["start"].as_u64().is_none()
    {
        return Err(CatalogError::Corrupt("通知消费者记录损坏"));
    }
    Ok(value)
}
fn consumer_key(ns: Uuid, serial: &str, id: &str) -> Vec<u8> {
    entity_key(ns, serial, b'c', id)
}
fn pending_key(ns: Uuid, serial: &str, id: &str) -> Vec<u8> {
    entity_key(ns, serial, b'p', id)
}
fn filter_of(value: &Value) -> Result<NotifyFilter> {
    let filter: NotifyFilter = serde_json::from_value(value["filter"].clone())
        .map_err(|_| CatalogError::Corrupt("通知过滤条件损坏"))?;
    filter.validate()?;
    Ok(filter)
}

async fn consumer_status_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    id: &str,
    mut value: Value,
) -> Result<Value> {
    let head = head_at(tr, ns, serial).await?;
    value["head"] = json!(head);
    value["unacknowledged_positions"] = json!(
        head.checked_sub(value["acknowledged"].as_u64().unwrap())
            .ok_or(CatalogError::Corrupt("消费者进度超过日志"))?
    );
    value["pending"] = match tr.get(&pending_key(ns, serial, id), false).await? {
        Some(v) => decode(&v)?,
        None => Value::Null,
    };
    let now = management::now();
    let connected = value["connection"]["connected"] == true
        && value["connection"]["expires_at_millis"]
            .as_u64()
            .is_some_and(|v| v > now);
    value["connection_status"] = json!(if connected {
        "connected"
    } else if value["connection"].is_null() {
        "never_connected"
    } else {
        "disconnected"
    });
    value["delivery_status"] = json!(if value["acknowledged"] == head {
        "caught_up"
    } else if value["pending"].is_null() {
        "backlogged"
    } else {
        "waiting_ack"
    });
    value["notification_status"] = json!(if !connected && value["acknowledged"] != head {
        "interrupted"
    } else {
        "normal"
    });
    value["waiting_ack_millis"] = value["pending"]["created_at_millis"]
        .as_u64()
        .map(|v| json!(now.saturating_sub(v)))
        .unwrap_or(Value::Null);
    Ok(value)
}

impl ManagedCatalog {
    /// Idempotent creation. Event types may widen; resources and ACK never rewind.
    /// from_start=false subscribes after the transaction's current log head.
    pub async fn create_notify_consumer(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        filter: &NotifyFilter,
        from_start: bool,
    ) -> Result<Value> {
        validate_id(id)?;
        filter.validate()?;
        let generation = Uuid::new_v4().to_string();
        self.db.run(|tr,_| {
            let generation = generation.clone();
            async move { requests::retry_result(async {
                let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
                let k = consumer_key(ns, serial, id);
                if tr.get(&k, false).await?.is_some() {
                    let mut existing = consumer_at(&tr, ns, serial, id).await?;
                    let old = filter_of(&existing)?;
                    if old.pool != filter.pool || old.barcodes != filter.barcodes || existing["from_start"] != from_start
                        || !type_expansion(&old.event_types,&filter.event_types) {
                        return Err(CatalogError::Invalid("只能扩大原事件类型，不能缩小范围、更换资源或重置起点"));
                    }
                    if !same_types(&old.event_types,&filter.event_types) {
                        widen_type_starts(&tr,ns,serial,&mut existing,filter).await?;
                        existing["filter"] = json!(filter);
                        tr.set(&k,existing.to_string().as_bytes());
                    }
                    return Ok(existing);
                }
                let start = if from_start { 0 } else { head_at(&tr, ns, serial).await? };
                let value = json!({"version":1,"id":id,"generation":generation,"namespace":ns.to_string(),
                    "library":serial,"filter":filter,"type_starts":initial_type_starts(filter,start),"from_start":from_start,"start":start,"acknowledged":start,
                    "created_at":crate::ltfs::label::ltfs_time_now()});
                tr.set(&k, value.to_string().as_bytes());
                Ok(value)
            }.await) }
        }).await?
    }
    pub async fn notify_consumer(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
    ) -> Result<Value> {
        validate_id(id)?;
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let value = consumer_at(&tr, ns, serial, id).await?;
        consumer_status_at(&tr, ns, serial, id, value).await
    }
    pub async fn list_notify_consumers(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid("消费者分页须在 1..=100"));
        }
        if let Some(id) = after {
            validate_id(id)?;
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let prefix = base(ns, serial, b'c');
        let cursor = after.map(|id| consumer_key(ns, serial, id));
        let page = read_page(&tr, &prefix, cursor.as_deref(), limit, false).await?;
        let mut values = Vec::new();
        for (_, raw) in page.items {
            let value = decode(&raw)?;
            let id = value["id"]
                .as_str()
                .ok_or(CatalogError::Corrupt("消费者标识缺失"))?
                .to_owned();
            values.push(consumer_status_at(&tr, ns, serial, &id, value).await?);
        }
        Ok(
            json!({"consumers":values,"next":page.next.and_then(|_| values.last().map(|v|v["id"].clone()))}),
        )
    }
    /// Deleting a durable consumer explicitly abandons its unacknowledged work.
    pub async fn delete_notify_consumer(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        generation: Uuid,
    ) -> Result<()> {
        validate_id(id)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        let k = consumer_key(ns, serial, id);
                        if tr.get(&k, false).await?.is_none() {
                            return Ok(());
                        }
                        let value = consumer_at(&tr, ns, serial, id).await?;
                        if value["generation"] != generation.to_string() {
                            return Err(CatalogError::Invalid("消费者已重建，禁止删除新的实例"));
                        }
                        tr.clear(&k);
                        tr.clear(&pending_key(ns, serial, id));
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }
    /// One outstanding batch per consumer. Reconnecting to any node returns the
    /// exact same delivery ID and immutable events until explicit ACK.
    pub async fn notify_batch(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        generation: Uuid,
    ) -> Result<Value> {
        validate_id(id)?;
        let delivery = Uuid::new_v4().to_string();
        self.db.run(|tr,_| {
            let delivery = delivery.clone();
            async move { requests::retry_result(async {
                let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
                let consumer = consumer_at(&tr, ns, serial, id).await?;
                if consumer["generation"] != generation.to_string() {
                    return Err(CatalogError::Invalid("消费者实例已变化，请重新查询订阅"));
                }
                let acknowledged = consumer["acknowledged"].as_u64().unwrap();
                let filter = filter_of(&consumer)?;
                let k = pending_key(ns, serial, id);
                let pending = tr.get(&k, false).await?;
                let mut receipt = match &pending {
                    Some(raw) => decode(raw)?,
                    None => json!({"delivery_id":delivery,"generation":generation.to_string(),"from":acknowledged,"through":acknowledged,"created_at_millis":management::now()}),
                };
                let through = receipt["through"].as_u64().ok_or(CatalogError::Corrupt("通知批次位置损坏"))?;
                if receipt["from"] != acknowledged || receipt["generation"] != generation.to_string()
                    || (pending.is_some() && through <= acknowledged)
                { return Err(CatalogError::Corrupt("通知批次与确认位置不符")); }
                let prefix = base(ns, serial, b'e');
                let page = read_page(&tr, &prefix, Some(&event_key(ns, serial, acknowledged)), BATCH_LIMIT, false).await?;
                let mut end = acknowledged;
                let mut events = Vec::new();
                let mut bytes = 0;
                for (_,raw) in page.items {
                    let event = decode(&raw)?;
                    let seq = event["sequence"].as_u64().ok_or(CatalogError::Corrupt("通知序号无效"))?;
                    if pending.is_some() && seq > through { break; }
                    if seq != end + 1 { return Err(CatalogError::Corrupt("通知历史存在缺口，禁止跳过")); }
                    if pending.is_none() && bytes + raw.len() > BATCH_BYTES && end > acknowledged { break; }
                    end = seq; bytes += raw.len();
                    if filter.matches(&event) {
                        let kind=event["event_type"].as_str().ok_or(CatalogError::Corrupt("通知事件类型缺失"))?;
                        let start=consumer["type_starts"][kind].as_u64().ok_or(CatalogError::Corrupt("通知事件类型起点缺失"))?;
                        if seq>start {events.push(event);}
                    }
                }
                if pending.is_some() && end != through { return Err(CatalogError::Corrupt("待确认通知事件缺失")); }
                if end == acknowledged {
                    if head_at(&tr, ns, serial).await? > end { return Err(CatalogError::Corrupt("通知历史缺失")); }
                    return Ok(json!({"generation":generation.to_string(),"events":[],"delivery_id":null,"through":acknowledged}));
                }
                receipt["through"] = json!(end);
                if pending.is_none() { tr.set(&k, receipt.to_string().as_bytes()); }
                receipt["events"] = json!(events);
                Ok(receipt)
            }.await) }
        }).await?
    }
    pub async fn acknowledge_notify(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        generation: Uuid,
        delivery: Uuid,
        through: u64,
    ) -> Result<Value> {
        validate_id(id)?;
        self.db.run(|tr,_| async move { requests::retry_result(async {
            let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
            let mut consumer = consumer_at(&tr, ns, serial, id).await?;
            if consumer["generation"] != generation.to_string() {
                return Err(CatalogError::Invalid("消费者实例已变化，拒绝旧确认"));
            }
            let acknowledged = consumer["acknowledged"].as_u64().unwrap();
            if through <= acknowledged {
                return Ok(json!({"generation":generation.to_string(),"acknowledged":acknowledged}));
            }
            let k = pending_key(ns, serial, id);
            let raw = tr.get(&k, false).await?.ok_or(CatalogError::Invalid("没有可确认的通知批次"))?;
            let pending = decode(&raw)?;
            if pending["delivery_id"] != delivery.to_string() || pending["generation"] != generation.to_string()
                || pending["from"] != acknowledged || pending["through"] != through {
                return Err(CatalogError::Invalid("确认必须对应完整的当前批次，禁止跳过事件"));
            }
            consumer["acknowledged"] = json!(through);
            consumer["last_ack_at_millis"] = json!(management::now());
            tr.set(&consumer_key(ns, serial, id), consumer.to_string().as_bytes());
            tr.clear(&k);
            Ok(json!({"generation":generation.to_string(),"acknowledged":through}))
        }.await) }).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notify_filters_include_both_sides_of_pool_moves() {
        let pool = Uuid::new_v4();
        let f = NotifyFilter {
            pool: Some(pool.to_string()),
            ..Default::default()
        };
        assert!(f.matches(
            &json!({"data":{"previous":{"pool":pool.to_string()},"current":{"pool":null}}})
        ));
        assert!(!f.matches(&json!({"data":{"previous":{"pool":Uuid::new_v4().to_string()}}})));
        assert!(
            NotifyFilter {
                event_types: vec!["bad".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        for id in ["", "a/b", "a\nb"] {
            assert!(validate_id(id).is_err());
        }
    }
    #[test]
    fn notify_type_expansion_never_discards_original_types() {
        let state = vec!["tape.state.changed".to_owned()];
        let wider = vec![
            "task.status.changed".to_owned(),
            "tape.state.changed".to_owned(),
        ];
        assert!(type_expansion(&state, &wider));
        assert!(!type_expansion(&wider, &state));
        assert!(type_expansion(&wider, &[]));
        assert!(!type_expansion(&[], &wider));
        assert!(type_expansion(&[], &[]));
        assert!(same_types(
            &wider,
            &[state[0].clone(), "task.status.changed".into()]
        ));
    }
    #[test]
    fn notify_state_precedence_matches_inventory() {
        let data = json!({"barcode":"AA0017L8","assignment":{"online":true,"volume":"v"},"monitor":{"state":"appendable"}});
        assert_eq!(tape_inventory_state(&data, &json!({}), true), "missing");
        assert_eq!(tape_inventory_state(&data, &json!({}), false), "appendable");
        let mut data = data;
        data["assignment"]["online"] = json!(false);
        data["assignment"]["blacklisted"] = json!(true);
        assert_eq!(tape_inventory_state(&data, &json!({}), true), "blacklisted");
        let physical = json!({"volumes":[{"barcode":"AA0017L8"},{"barcode":"AA0017L8"}]});
        assert_eq!(tape_inventory_state(&data, &physical, true), "duplicated");
    }
}

#[cfg(test)]
mod fdb_tests {
    use super::*;
    #[test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；验证通知故障回滚业务事务，不访问硬件"]
    fn notify_atomic_event_log_deduplicates_retries_and_aborts_partial_writes() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let c = ManagedCatalog::open(Some(&cluster)).unwrap();
            let ns = Uuid::new_v4();
            let serial = "NOTIFY_ATOMIC";
            for _ in 0..2 {
                c.db.run(|tr, _| async move {
                    requests::retry_result(
                        changed_at(
                            &tr,
                            ns,
                            serial,
                            "monitor",
                            "library.monitor.changed",
                            json!({"status":"ok"}),
                        )
                        .await,
                    )
                })
                .await
                .unwrap()
                .unwrap();
            }
            let tr = c.db.create_trx().unwrap();
            assert_eq!(head_at(&tr, ns, serial).await.unwrap(), 1);
            let first = tr
                .get(&event_key(ns, serial, 1), false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            c.db.run(|tr, _| async move {
                tr.set(&base(ns, serial, b'h'), b"corrupt");
                Ok(())
            })
            .await
            .unwrap();
            let marker = entity_key(ns, serial, b'x', "business-state");
            let result =
                c.db.run(|tr, _| {
                    let marker = marker.clone();
                    async move {
                        tr.set(&marker, b"must-not-commit");
                        requests::retry_result(
                            changed_at(
                                &tr,
                                ns,
                                serial,
                                "monitor",
                                "library.monitor.changed",
                                json!({"status":"failed"}),
                            )
                            .await,
                        )
                    }
                })
                .await;
            assert!(result.is_err());
            let tr = c.db.create_trx().unwrap();
            assert!(tr.get(&marker, false).await.unwrap().is_none());
            assert_eq!(
                tr.get(&event_key(ns, serial, 1), false)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_vec(),
                first
            );
        });
    }
}

impl ManagedCatalog {
    /// Widen only. Existing types retain their backlog; each new type starts
    /// after this transaction's log head. Pending delivery remains immutable.
    pub async fn expand_notify_types(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        generation: Uuid,
        types: &[String],
    ) -> Result<Value> {
        validate_id(id)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns =
                            routing::require_expected_active_namespace(&tr, serial, Some(route))
                                .await?;
                        let mut consumer = consumer_at(&tr, ns, serial, id).await?;
                        if consumer["generation"] != generation.to_string() {
                            return Err(CatalogError::Invalid("消费者实例已变化，拒绝修改旧订阅"));
                        }
                        let old = filter_of(&consumer)?;
                        let mut next = old.clone();
                        next.event_types = types.to_vec();
                        next.validate()?;
                        if !type_expansion(&old.event_types, types) {
                            return Err(CatalogError::Invalid(
                                "重连类型必须包含原订阅全部类型，不能缩小范围",
                            ));
                        }
                        if !same_types(&old.event_types, types) {
                            widen_type_starts(&tr, ns, serial, &mut consumer, &next).await?;
                            consumer["filter"] = json!(next);
                            tr.set(
                                &consumer_key(ns, serial, id),
                                consumer.to_string().as_bytes(),
                            );
                        }
                        Ok(consumer)
                    }
                    .await,
                )
            })
            .await?
    }
    /// Session fencing prevents an old connection's close/heartbeat from
    /// overwriting the liveness of a newer connection. The 45s lease is only
    /// diagnostic; expiry never advances ACK or deletes any event.
    pub async fn record_notify_connection(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        id: &str,
        token: (Uuid, Uuid),
        action: NotifyConnectionAction,
    ) -> Result<bool> {
        validate_id(id)?;
        self.db.run(|tr,_|async move {requests::retry_result(async {
            let ns=routing::require_expected_active_namespace(&tr,serial,Some(route)).await?;
            if tr.get(&consumer_key(ns,serial,id),false).await?.is_none() {return Ok(false);}
            let mut consumer=consumer_at(&tr,ns,serial,id).await?;
            if consumer["generation"]!=token.0.to_string() {return Ok(false);}
            if action!=NotifyConnectionAction::Open && consumer["connection"]["session"]!=token.1.to_string() {return Ok(false);}
            let now=management::now();
            match action {
                NotifyConnectionAction::Open=>{
                    consumer["connection"]=json!({"session":token.1.to_string(),"connected":true,"last_seen_at_millis":now,"expires_at_millis":now.saturating_add(45_000)});
                    consumer["last_connected_at_millis"]=json!(now);
                    consumer["last_connection_error"]=Value::Null;
                }
                NotifyConnectionAction::Closed=>{
                    consumer["connection"]["connected"]=json!(false);
                    consumer["connection"]["expires_at_millis"]=json!(now);
                    consumer["last_disconnected_at_millis"]=json!(now);
                    consumer["last_connection_error"]=json!("通知连接中断；未确认事件仍保留");
                }
                NotifyConnectionAction::Heartbeat|NotifyConnectionAction::Sent=>{
                    consumer["connection"]["last_seen_at_millis"]=json!(now);
                    consumer["connection"]["expires_at_millis"]=json!(now.saturating_add(45_000));
                    if action==NotifyConnectionAction::Sent {
                        consumer["last_sent_at_millis"]=json!(now);
                        consumer["delivery_attempts"]=json!(consumer["delivery_attempts"].as_u64().unwrap_or(0).saturating_add(1));
                    }
                }
            }
            tr.set(&consumer_key(ns,serial,id),consumer.to_string().as_bytes());
            Ok(true)
        }.await)}).await?
    }
}

impl ManagedCatalog {
    /// Explicit test events enter only the notification log. They never update
    /// physical inventory, health, policy, task state or any device.
    pub async fn inject_notification(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        operation: Uuid,
        kind: &str,
        data: &Value,
    ) -> Result<Value> {
        use sha2::{Digest, Sha256};
        if operation.is_nil()
            || !EVENT_TYPES.contains(&kind)
            || !data.is_object()
            || data.to_string().len() > 8192
        {
            return Err(CatalogError::Invalid(
                "注入必须使用非空UUID、已知事件类型和不超过8KiB的JSON对象",
            ));
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(json!({"event_type":kind,"data":data}).to_string())
        );
        self.db
            .run(|tr, _| {
                let fingerprint = fingerprint.clone();
                async move {
                    requests::retry_result(
                        async {
                            let ns = routing::require_expected_active_namespace(
                                &tr,
                                serial,
                                Some(route),
                            )
                            .await?;
                            let k = entity_key(ns, serial, b'i', &operation.to_string());
                            if let Some(raw) = tr.get(&k, false).await? {
                                let old = decode(&raw)?;
                                if old["fingerprint"] != fingerprint {
                                    return Err(CatalogError::Invalid(
                                        "同一注入请求UUID的类型或内容不同",
                                    ));
                                }
                                return Ok(old["event"].clone());
                            }
                            let sequence = append_event_at(
                                &tr,
                                ns,
                                serial,
                                kind,
                                data.clone(),
                                Some(operation),
                            )
                            .await?;
                            let raw = tr
                                .get(&event_key(ns, serial, sequence), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("注入事件缺失"))
                                .map_err(atomic_error)?;
                            let event = decode(&raw).map_err(atomic_error)?;
                            tr.set(
                                &k,
                                json!({"fingerprint":fingerprint,"event":event})
                                    .to_string()
                                    .as_bytes(),
                            );
                            Ok(event)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}
