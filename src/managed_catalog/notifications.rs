//! Durable notifications: immutable events and explicit, consumer-specific ACKs.
//! No network I/O or process-local delivery authority belongs in this module.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

fn effective_types(types: &[String]) -> std::collections::BTreeSet<&str> {
    if types.is_empty() {
        EVENT_TYPES.iter().copied().collect()
    } else {
        types.iter().map(String::as_str).collect()
    }
}
fn type_expansion(old: &[String], new: &[String]) -> bool {
    effective_types(old).is_subset(&effective_types(new))
}
fn same_types(old: &[String], new: &[String]) -> bool {
    effective_types(old) == effective_types(new)
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
        if self
            .event_types
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != self.event_types.len()
        {
            return Err(CatalogError::Invalid("通知事件类型重复"));
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

fn now() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
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
fn canonical_uuid(value: &Value) -> Option<Uuid> {
    value.as_str().and_then(|s| {
        Uuid::parse_str(s)
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == s)
    })
}

fn decode_event(raw: &[u8], ns: Uuid, serial: &str, sequence: u64) -> Result<Value> {
    let event = decode(raw)?;
    let injected = match event["injection_id"] {
        Value::Null => event["injected"] == false && event["origin"] == "tape-rs",
        _ => {
            canonical_uuid(&event["injection_id"]).is_some()
                && event["injected"] == true
                && event["origin"] == "injected"
        }
    };
    if raw.len() > EVENT_LIMIT
        || event.to_string().as_bytes() != raw
        || !event.as_object().is_some_and(|v| {
            v.len() == 11
                && [
                    "version",
                    "event_id",
                    "event_type",
                    "sequence",
                    "namespace",
                    "library",
                    "observed_at",
                    "data",
                    "origin",
                    "injected",
                    "injection_id",
                ]
                .iter()
                .all(|k| v.contains_key(*k))
        })
        || event["version"] != 1
        || sequence == 0
        || event["sequence"] != sequence
        || event["namespace"] != ns.to_string()
        || event["library"] != serial
        || event["event_id"] != format!("{ns}:{serial}:{sequence}")
        || !event["event_type"]
            .as_str()
            .is_some_and(|s| EVENT_TYPES.contains(&s))
        || !event["observed_at"]
            .as_str()
            .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        || !event["data"].is_object()
        || !injected
    {
        return Err(CatalogError::Corrupt("通知事件身份或格式损坏"));
    }
    Ok(event)
}

fn decode_pending(raw: &[u8], consumer: &Value, head: u64) -> Result<Value> {
    let value = decode(raw)?;
    let from = consumer["acknowledged"]
        .as_u64()
        .ok_or(CatalogError::Corrupt("消费者确认位置损坏"))?;
    if !value.as_object().is_some_and(|v| v.len() == 6)
        || !value["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        || canonical_uuid(&value["delivery_id"]).is_none()
        || value["generation"] != consumer["generation"]
        || value["from"] != from
        || !value["through"]
            .as_u64()
            .is_some_and(|n| n > from && n <= head && n - from <= BATCH_LIMIT as u64)
        || !value["created_at_millis"].as_u64().is_some_and(|n| n > 0)
    {
        return Err(CatalogError::Corrupt("通知批次身份或位置损坏"));
    }
    Ok(value)
}

fn batch_fingerprint(receipt: &Value, events: &[Value]) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            json!({
                "delivery_id":receipt["delivery_id"],"generation":receipt["generation"],
                "from":receipt["from"],"through":receipt["through"],
                "created_at_millis":receipt["created_at_millis"],"events":events,
            })
            .to_string()
        )
    )
}

fn require_batch_fingerprint(receipt: &Value, events: &[Value]) -> Result<()> {
    if receipt["fingerprint"] != batch_fingerprint(receipt, events) {
        return Err(CatalogError::Corrupt("待确认通知批次内容或范围已改变"));
    }
    Ok(())
}

// Delivery and ACK use the same bounded verification; never ACK unseen gaps.
async fn scan_events(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    from: u64,
    through: Option<u64>,
) -> Result<(u64, Vec<Value>)> {
    let head = head_at(tr, ns, serial).await?;
    if from > head || through.is_some_and(|n| n <= from || n > head) {
        return Err(CatalogError::Corrupt("通知批次位置超过日志"));
    }
    let page = read_page(
        tr,
        &base(ns, serial, b'e'),
        Some(&event_key(ns, serial, from)),
        BATCH_LIMIT,
        false,
    )
    .await?;
    let mut end = from;
    let mut bytes = 0;
    let mut events = Vec::new();
    for (record, raw) in page.items {
        if through.is_some_and(|n| end == n) {
            break;
        }
        let next = end
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("通知序号耗尽"))?;
        if record != event_key(ns, serial, next) || next > head {
            return Err(CatalogError::Corrupt("通知历史存在缺口或日志头不符"));
        }
        let event = decode_event(&raw, ns, serial, next)?;
        if bytes + raw.len() > BATCH_BYTES {
            if through.is_some() {
                return Err(CatalogError::Corrupt("通知批次超过字节上限"));
            }
            break;
        }
        bytes += raw.len();
        end = next;
        events.push(event);
    }
    if through.is_some_and(|n| n != end) || (end == from && head > from) {
        return Err(CatalogError::Corrupt("通知历史缺失"));
    }
    Ok((end, events))
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
fn atomic_error(error: CatalogError) -> CatalogError {
    match error {
        CatalogError::Fdb(_) | CatalogError::Client(_) => error,
        other => CatalogError::Fdb(FdbBindingError::CustomError(Box::new(other))),
    }
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
        if !EVENT_TYPES.contains(&kind) || !data.is_object() || injection.is_some_and(|id| id.is_nil()) {
            return Err(CatalogError::Invalid("通知事件类型、内容或注入身份无效"));
        }
        let head = head_at(tr, ns, serial).await?;
        let sequence = head.checked_add(1)
            .ok_or(CatalogError::Corrupt("通知序号耗尽"))?;
        let event = json!({"version":1,"event_id":format!("{ns}:{serial}:{sequence}"),
            "event_type":kind,"sequence":sequence,"namespace":ns.to_string(),"library":serial,
            "observed_at":crate::ltfs::label::ltfs_time_now(),"data":data,
            "origin":if injection.is_some(){"injected"}else{"tape-rs"},"injected":injection.is_some(),"injection_id":injection.map(|id|id.to_string())});
        let raw = event.to_string();
        if raw.len() > EVENT_LIMIT {
            return Err(CatalogError::Invalid("通知事件超过记录上限"));
        }
        if head > 0 {
            let previous = tr.get(&event_key(ns, serial, head), false).await?.ok_or(CatalogError::Corrupt("通知日志头事件缺失"))?;
            decode_event(&previous, ns, serial, head)?;
        }
        if !read_page(tr, &base(ns, serial, b'e'), Some(&event_key(ns, serial, head)), 1, false).await?.items.is_empty() {
            return Err(CatalogError::Corrupt("通知日志头与已有事件不符，拒绝覆盖"));
        }
        tr.set(&event_key(ns, serial, sequence), raw.as_bytes());
        tr.set(&base(ns, serial, b'h'), &sequence.to_be_bytes());
        Ok(sequence)
    }
    .await;
    result.map_err(atomic_error)
}

pub(super) async fn require_changed_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    entity: &str,
    kind: &str,
    expected: Option<&Value>,
) -> Result<()> {
    let raw = tr.get(&entity_key(ns, serial, b's', entity), false).await?;
    match (raw, expected) {
        (None, None) => Ok(()),
        (Some(raw), Some(expected)) => {
            let state = decode(&raw)?;
            let sequence = state["sequence"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(CatalogError::Corrupt("通知投影序号损坏"))?;
            if state.to_string().as_bytes() != raw.as_ref()
                || !state.as_object().is_some_and(|v| {
                    v.len() == 4
                        && ["version", "sequence", "current", "event_sha256"]
                            .iter()
                            .all(|k| v.contains_key(*k))
                })
                || state["version"] != 1
                || state["current"] != *expected
                || !expected.is_object()
                || sequence > head_at(tr, ns, serial).await?
            {
                return Err(CatalogError::Corrupt("通知投影与原状态不符"));
            }
            let event = tr
                .get(&event_key(ns, serial, sequence), false)
                .await?
                .ok_or(CatalogError::Corrupt("通知投影对应的原事件缺失"))?;
            let value = decode_event(&event, ns, serial, sequence)?;
            if state["event_sha256"] != format!("{:x}", Sha256::digest(&event))
                || value["event_type"] != kind
                || value["injected"] != false
                || value["data"]["current"] != *expected
                || value["data"]["pool"] != expected["pool"]
                || value["data"]["barcode"] != expected["barcode"]
            {
                return Err(CatalogError::Corrupt("通知投影与原事件不符"));
            }
            Ok(())
        }
        _ => Err(CatalogError::Corrupt("通知投影孤立或缺失")),
    }
}

pub(super) async fn changed_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    entity: &str,
    kind: &str,
    previous: Option<&Value>,
    current: Value,
) -> Result<()> {
    let result=async {
        if !current.is_object() {return Err(CatalogError::Invalid("通知状态必须为 JSON 对象"));}
        require_changed_at(tr,ns,serial,entity,kind,previous).await?;
        if previous==Some(&current) {return Ok(());}
        let sequence=append_event_at(tr,ns,serial,kind,
            json!({"previous":previous,"current":current,"pool":current["pool"],"barcode":current["barcode"]}),None).await?;
        let event=tr.get(&event_key(ns,serial,sequence),false).await?.ok_or(CatalogError::Corrupt("刚写入的通知事件缺失"))?;
        tr.set(&entity_key(ns,serial,b's',entity),json!({"version":1,"sequence":sequence,"event_sha256":format!("{:x}",Sha256::digest(&event)),"current":current}).to_string().as_bytes());
        Ok(())
    }.await;
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
    decode_consumer(&raw, ns, serial, id, head_at(tr, ns, serial).await?)
}
fn decode_consumer(raw: &[u8], ns: Uuid, serial: &str, id: &str, head: u64) -> Result<Value> {
    let value = decode(raw)?;
    let generation = value["generation"].as_str().and_then(|v| {
        Uuid::parse_str(v)
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == v)
    });
    if value["version"] != 1
        || value["namespace"] != ns.to_string()
        || value["library"] != serial
        || value["id"] != id
        || generation.is_none()
        || !matches!((value["start"].as_u64(), value["acknowledged"].as_u64()), (Some(start),Some(ack)) if start <= ack && ack <= head)
    {
        return Err(CatalogError::Corrupt("通知消费者记录损坏"));
    }
    let filter = filter_of(&value).map_err(|_| CatalogError::Corrupt("通知过滤条件损坏"))?;
    let types = selected_types(&filter);
    let starts = value["type_starts"]
        .as_object()
        .ok_or(CatalogError::Corrupt("通知事件类型起点损坏"))?;
    if starts.len() != types.len()
        || !types.iter().all(|kind| {
            starts
                .get(*kind)
                .and_then(Value::as_u64)
                .is_some_and(|n| n >= value["start"].as_u64().unwrap() && n <= head)
        })
    {
        return Err(CatalogError::Corrupt("通知事件类型起点损坏"));
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
        Some(v) => {
            let pending = decode_pending(&v, &value, head)?;
            let (_, events) = scan_events(
                tr,
                ns,
                serial,
                value["acknowledged"].as_u64().unwrap(),
                pending["through"].as_u64(),
            )
            .await?;
            require_batch_fingerprint(&pending, &events)?;
            pending
        }
        None => Value::Null,
    };
    let now = now();
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
        let head = head_at(&tr, ns, serial).await?;
        for (record, raw) in page.items {
            let value = decode(&raw)?;
            let id = value["id"]
                .as_str()
                .ok_or(CatalogError::Corrupt("消费者标识缺失"))?
                .to_owned();
            validate_id(&id).map_err(|_| CatalogError::Corrupt("消费者标识损坏"))?;
            if record != consumer_key(ns, serial, &id) {
                return Err(CatalogError::Corrupt("消费者索引身份不符"));
            }
            let value = decode_consumer(&raw, ns, serial, &id, head)?;
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
                    Some(raw) => decode_pending(raw, &consumer, head_at(&tr, ns, serial).await?)?,
                    None => json!({"delivery_id":delivery,"generation":generation.to_string(),"from":acknowledged,"through":acknowledged,"created_at_millis":now()}),
                };
                let through = pending.as_ref().map(|_| receipt["through"].as_u64().unwrap());
                let (end, scanned) = scan_events(&tr, ns, serial, acknowledged, through).await?;
                receipt["through"] = json!(end);
                if pending.is_some() { require_batch_fingerprint(&receipt, &scanned)?; }
                else { receipt["fingerprint"] = json!(batch_fingerprint(&receipt, &scanned)); }
                let mut events = Vec::new();
                for event in scanned {
                    if filter.matches(&event) {
                        let kind = event["event_type"].as_str().expect("validated event type");
                        let start = consumer["type_starts"][kind].as_u64().expect("validated type start");
                        if event["sequence"].as_u64().unwrap() > start { events.push(event); }
                    }
                }
                if end == acknowledged {
                    return Ok(json!({"generation":generation.to_string(),"events":[],"delivery_id":null,"through":acknowledged}));
                }
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
            let pending = decode_pending(&raw, &consumer, head_at(&tr, ns, serial).await?)?;
            if pending["delivery_id"] != delivery.to_string() || pending["generation"] != generation.to_string()
                || pending["from"] != acknowledged || pending["through"] != through {
                return Err(CatalogError::Invalid("确认必须对应完整的当前批次，禁止跳过事件"));
            }
            let (_, events) = scan_events(&tr, ns, serial, acknowledged, Some(through)).await?;
            require_batch_fingerprint(&pending, &events)?;
            consumer["acknowledged"] = json!(through);
            consumer["last_ack_at_millis"] = json!(now());
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
        let full: Vec<String> = EVENT_TYPES.iter().map(|v| (*v).to_owned()).collect();
        assert!(type_expansion(&[], &full));
        assert!(same_types(&[], &full));
        assert!(same_types(&full, &[]));

        assert!(same_types(
            &wider,
            &[state[0].clone(), "task.status.changed".into()]
        ));
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
            let now=now();
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
                                let sequence = old["event"]["sequence"]
                                    .as_u64()
                                    .ok_or(CatalogError::Corrupt("注入回执事件缺失"))?;
                                let event = decode_event(
                                    old["event"].to_string().as_bytes(),
                                    ns,
                                    serial,
                                    sequence,
                                )?;
                                let logged = tr
                                    .get(&event_key(ns, serial, sequence), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("注入事件日志缺失"))?;
                                if !old.as_object().is_some_and(|v| v.len() == 2)
                                    || event["injection_id"] != operation.to_string()
                                    || event["event_type"] != kind
                                    || event["data"] != *data
                                    || decode_event(&logged, ns, serial, sequence)? != event
                                {
                                    return Err(CatalogError::Corrupt("注入回执与事件日志不符"));
                                }
                                return Ok(event);
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

#[cfg(test)]
mod fdb_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；只验证持久通知、消费者与故障回滚，不访问设备"]
    async fn durable_notifications_replay_widen_ack_and_fence_sessions() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("NOTIFY-{ns}");
        let challenge = Uuid::new_v4();
        let replies: Vec<_> = (1..=3).map(|id| json!({
            "version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,
            "term":7,"leader":2,"role":if id==2 {"Leader"}else{"Follower"},
            "members":{"1":"127.0.0.1:8001","2":"127.0.0.1:8002","3":"127.0.0.1:8003"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,
            "executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true
        })).collect();
        let route = c
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        let task = "task.status.changed";
        let state = "tape.state.changed";
        let first = c
            .inject_notification(&serial, route, Uuid::new_v4(), task, &json!({"n":1}))
            .await
            .unwrap();
        let second = c
            .inject_notification(&serial, route, Uuid::new_v4(), state, &json!({"n":2}))
            .await
            .unwrap();
        c.inject_notification(&serial, route, Uuid::new_v4(), task, &json!({"n":3}))
            .await
            .unwrap();
        let filter = NotifyFilter {
            event_types: vec![state.into()],
            ..Default::default()
        };
        let consumer = c
            .create_notify_consumer(&serial, route, "external", &filter, true)
            .await
            .unwrap();
        let generation = Uuid::parse_str(consumer["generation"].as_str().unwrap()).unwrap();
        let batch = c
            .notify_batch(&serial, route, "external", generation)
            .await
            .unwrap();
        assert_eq!(batch["events"], json!([second.clone()]));
        assert_eq!(batch["through"], 3);
        let delivery = Uuid::parse_str(batch["delivery_id"].as_str().unwrap()).unwrap();
        let tr = c.db.create_trx().unwrap();
        let pending_record = pending_key(ns, &serial, "external");
        let saved_pending = tr
            .get(&pending_record, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut bad_pending = decode(&saved_pending).unwrap();
        bad_pending["through"] = json!(4);
        tr.set(&pending_record, bad_pending.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            c.acknowledge_notify(&serial, route, "external", generation, delivery, 4)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            consumer_at(&tr, ns, &serial, "external").await.unwrap()["acknowledged"],
            0
        );
        assert_eq!(
            decode(&tr.get(&pending_record, false).await.unwrap().unwrap()).unwrap(),
            bad_pending
        );
        tr.set(&pending_record, &saved_pending);
        tr.commit().await.unwrap();
        let mut shortened = decode(&saved_pending).unwrap();
        shortened["through"] = json!(2);
        let tr = c.db.create_trx().unwrap();
        tr.set(&pending_record, shortened.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            c.acknowledge_notify(&serial, route, "external", generation, delivery, 2)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            consumer_at(&tr, ns, &serial, "external").await.unwrap()["acknowledged"],
            0
        );
        assert_eq!(
            decode(&tr.get(&pending_record, false).await.unwrap().unwrap()).unwrap(),
            shortened
        );
        tr.set(&pending_record, &saved_pending);
        tr.commit().await.unwrap();
        let other = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            other
                .notify_batch(&serial, route, "external", generation)
                .await
                .unwrap(),
            batch
        );
        assert!(
            other
                .acknowledge_notify(&serial, route, "external", generation, Uuid::new_v4(), 3)
                .await
                .is_err()
        );
        assert!(
            other
                .acknowledge_notify(&serial, route, "external", generation, delivery, 2)
                .await
                .is_err()
        );
        let session1 = Uuid::new_v4();
        let session2 = Uuid::new_v4();
        for session in [session1, session2] {
            assert!(
                c.record_notify_connection(
                    &serial,
                    route,
                    "external",
                    (generation, session),
                    NotifyConnectionAction::Open
                )
                .await
                .unwrap()
            );
        }
        assert!(
            !c.record_notify_connection(
                &serial,
                route,
                "external",
                (generation, session1),
                NotifyConnectionAction::Closed
            )
            .await
            .unwrap()
        );
        assert!(
            other
                .record_notify_connection(
                    &serial,
                    route,
                    "external",
                    (generation, session2),
                    NotifyConnectionAction::Closed
                )
                .await
                .unwrap()
        );
        let status = c.notify_consumer(&serial, route, "external").await.unwrap();
        assert_eq!(status["connection_status"], "disconnected");
        assert_eq!(status["notification_status"], "interrupted");
        assert_eq!(status["acknowledged"], 0);
        let wider = vec![state.into(), task.into()];
        c.expand_notify_types(&serial, route, "external", generation, &wider)
            .await
            .unwrap();
        let fourth = c
            .inject_notification(&serial, route, Uuid::new_v4(), task, &json!({"n":4}))
            .await
            .unwrap();
        let operation = Uuid::new_v4();
        let fifth = c
            .inject_notification(&serial, route, operation, state, &json!({"n":5}))
            .await
            .unwrap();
        assert_eq!(
            c.inject_notification(&serial, route, operation, state, &json!({"n":5}))
                .await
                .unwrap(),
            fifth
        );
        assert!(
            c.inject_notification(&serial, route, operation, state, &json!({"n":6}))
                .await
                .is_err()
        );
        let injected_key = entity_key(ns, &serial, b'i', &operation.to_string());
        let tr = c.db.create_trx().unwrap();
        let original_injection = tr
            .get(&injected_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut bad_injection = decode(&original_injection).unwrap();
        bad_injection["event"] = Value::Null;
        tr.set(&injected_key, bad_injection.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            c.inject_notification(&serial, route, operation, state, &json!({"n":5}))
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            decode(&tr.get(&injected_key, false).await.unwrap().unwrap()).unwrap(),
            bad_injection
        );
        tr.set(&injected_key, &original_injection);
        tr.clear(&event_key(ns, &serial, 5));
        tr.commit().await.unwrap();
        assert!(matches!(
            c.inject_notification(&serial, route, operation, state, &json!({"n":5}))
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert!(
            tr.get(&event_key(ns, &serial, 5), false)
                .await
                .unwrap()
                .is_none()
        );
        tr.set(&event_key(ns, &serial, 5), fifth.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert_eq!(
            other
                .notify_batch(&serial, route, "external", generation)
                .await
                .unwrap(),
            batch
        );
        for client in [&other, &c] {
            assert_eq!(
                client
                    .acknowledge_notify(&serial, route, "external", generation, delivery, 3)
                    .await
                    .unwrap()["acknowledged"],
                3
            );
        }
        let next = other
            .notify_batch(&serial, route, "external", generation)
            .await
            .unwrap();
        assert_eq!(next["events"], json!([fourth, fifth]));
        assert_eq!(next["through"], 5);
        let next_delivery = Uuid::parse_str(next["delivery_id"].as_str().unwrap()).unwrap();
        c.acknowledge_notify(&serial, route, "external", generation, next_delivery, 5)
            .await
            .unwrap();
        assert!(
            c.expand_notify_types(&serial, route, "external", generation, &[state.into()])
                .await
                .is_err()
        );
        assert!(
            c.create_notify_consumer(&serial, route, "external", &filter, false)
                .await
                .is_err()
        );
        let drained = other
            .notify_batch(&serial, route, "external", generation)
            .await
            .unwrap();
        assert_eq!(drained["delivery_id"], Value::Null);
        assert_eq!(drained["through"], 5);
        assert_eq!(
            c.notify_consumer(&serial, route, "external").await.unwrap()["delivery_status"],
            "caught_up"
        );
        assert_eq!(
            c.notify_consumer(&serial, route, "external").await.unwrap()["head"],
            5
        );
        let all = NotifyFilter::default();
        let audit = c
            .create_notify_consumer(&serial, route, "audit", &all, true)
            .await
            .unwrap();
        let audit_generation = Uuid::parse_str(audit["generation"].as_str().unwrap()).unwrap();
        let explicit_all = NotifyFilter {
            event_types: EVENT_TYPES.iter().map(|v| (*v).to_owned()).collect(),
            ..Default::default()
        };
        let same_audit = c
            .create_notify_consumer(&serial, route, "audit", &explicit_all, true)
            .await
            .unwrap();
        assert_eq!(same_audit, audit);
        assert_eq!(
            c.expand_notify_types(
                &serial,
                route,
                "audit",
                audit_generation,
                &explicit_all.event_types
            )
            .await
            .unwrap(),
            audit
        );
        let audit_batch = other
            .notify_batch(&serial, route, "audit", audit_generation)
            .await
            .unwrap();
        assert_eq!(audit_batch["events"].as_array().unwrap().len(), 5);
        assert_eq!(audit_batch["events"][0], first);
        assert!(
            c.delete_notify_consumer(&serial, route, "external", Uuid::new_v4())
                .await
                .is_err()
        );
        c.delete_notify_consumer(&serial, route, "external", generation)
            .await
            .unwrap();
        let recreated = c
            .create_notify_consumer(&serial, route, "external", &all, true)
            .await
            .unwrap();
        assert_ne!(recreated["generation"], generation.to_string());
        assert!(
            c.acknowledge_notify(&serial, route, "external", generation, delivery, 3)
                .await
                .is_err()
        );
        assert!(
            !c.record_notify_connection(
                &serial,
                route,
                "external",
                (generation, session2),
                NotifyConnectionAction::Open
            )
            .await
            .unwrap()
        );
        assert!(
            c.delete_notify_consumer(&serial, route, "external", generation)
                .await
                .is_err()
        );
        // Missing log records must block delivery, never silently advance ACK.
        let tr = c.db.create_trx().unwrap();
        tr.clear(&event_key(ns, &serial, 2));
        tr.commit().await.unwrap();
        let new_generation = Uuid::parse_str(recreated["generation"].as_str().unwrap()).unwrap();
        assert!(matches!(
            c.notify_batch(&serial, route, "external", new_generation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert!(
            tr.get(&pending_key(ns, &serial, "external"), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            consumer_at(&tr, ns, &serial, "external").await.unwrap()["acknowledged"],
            0
        );
        tr.set(&event_key(ns, &serial, 2), second.to_string().as_bytes());
        tr.commit().await.unwrap();
        // Listing must validate durable consumer rows before formatting status.
        let audit_key = consumer_key(ns, &serial, "audit");
        let tr = c.db.create_trx().unwrap();
        let saved_audit = tr.get(&audit_key, false).await.unwrap().unwrap().to_vec();
        let mut damaged = decode(&saved_audit).unwrap();
        damaged["acknowledged"] = json!("corrupt");
        tr.set(&audit_key, damaged.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            c.list_notify_consumers(&serial, route, None, 100).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        tr.set(&audit_key, &saved_audit);
        tr.commit().await.unwrap();
        // A damaged but well-formed head must not overwrite an immutable event.
        let tr = c.db.create_trx().unwrap();
        tr.set(&base(ns, &serial, b'h'), &0u64.to_be_bytes());
        tr.commit().await.unwrap();
        assert!(
            c.inject_notification(
                &serial,
                route,
                Uuid::new_v4(),
                state,
                &json!({"bad":"head"})
            )
            .await
            .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            decode(
                &tr.get(&event_key(ns, &serial, 1), false)
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            first
        );
        tr.set(&base(ns, &serial, b'h'), &5u64.to_be_bytes());
        tr.commit().await.unwrap();
        // Event identity must match both its FDB key and this library namespace.
        let mut foreign = first.clone();
        foreign["namespace"] = json!(Uuid::new_v4().to_string());
        let tr = c.db.create_trx().unwrap();
        tr.set(&event_key(ns, &serial, 1), foreign.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            c.notify_batch(&serial, route, "external", new_generation)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert!(
            tr.get(&pending_key(ns, &serial, "external"), false)
                .await
                .unwrap()
                .is_none()
        );
        tr.set(&event_key(ns, &serial, 1), first.to_string().as_bytes());
        tr.commit().await.unwrap();
        // Event failure must abort preceding business writes, even through retry_result.
        let tr = c.db.create_trx().unwrap();
        tr.set(&base(ns, &serial, b'h'), b"corrupt");
        tr.commit().await.unwrap();
        let marker = entity_key(ns, &serial, b'x', "must-not-commit");
        let result =
            c.db.run(|tr, _| {
                let marker = marker.clone();
                let serial = serial.clone();
                async move {
                    tr.set(&marker, b"business mutation");
                    requests::retry_result(
                        append_event_at(&tr, ns, &serial, state, json!({"n":6}), None).await,
                    )
                }
            })
            .await;
        assert!(result.is_err());
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&marker, false).await.unwrap().is_none());
        assert!(
            tr.get(&event_key(ns, &serial, 6), false)
                .await
                .unwrap()
                .is_none()
        );
    }
}
