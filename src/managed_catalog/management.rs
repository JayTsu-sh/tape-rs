//! Durable management intent queue. No method here applies control changes or
//! performs media work. Running tasks require cancellation by their device actor.
use super::*;
use serde_json::{Value, json};

const MAX_QUEUED: usize = 100;

pub(crate) fn prefix(namespace: Uuid, kind: u8) -> Vec<u8> {
    let mut out = key(b'@');
    out.extend_from_slice(namespace.as_bytes());
    out.push(kind);
    out
}
fn library_prefix(namespace: Uuid, serial: &str, kind: u8) -> Vec<u8> {
    let mut out = prefix(namespace, kind);
    append_component(&mut out, serial.as_bytes());
    out
}
fn identity_key(namespace: Uuid, serial: &str, kind: u8, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = library_prefix(namespace, serial, kind);
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
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
async fn window(tr: &Transaction, namespace: Uuid) -> Result<RequestWindow> {
    let raw = tr
        .get(&prefix(namespace, b'w'), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    requests::decode_window(&raw)
}
fn require_retained(window: RequestWindow, epoch: u64, operation: Uuid) -> Result<()> {
    if operation.is_nil() || epoch < window.first_retained || epoch > window.accepting {
        return Err(CatalogError::Invalid("管理请求身份或代次不在保留窗口"));
    }
    Ok(())
}
fn now() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}
fn encode(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}
fn device_kind(kind: &str) -> bool {
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
fn task(bytes: &[u8], serial: &str, namespace: Uuid, epoch: u64, operation: Uuid) -> Result<Value> {
    if bytes.len() > 16384 {
        return Err(CatalogError::Corrupt("管理回执过大"));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("管理回执 JSON"))?;
    if !value.as_object().is_some_and(|v| {
        v.len() == 12
            && [
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
            ]
            .iter()
            .all(|k| v.contains_key(*k))
    }) {
        return Err(CatalogError::Corrupt("管理回执字段"));
    }
    let request = ManagementRequest::parse(value["request"].clone())
        .map_err(|_| CatalogError::Corrupt("管理回执请求"))?;
    if encode(&value) != bytes
        || request.namespace() != namespace
        || request.epoch() != epoch
        || request.operation() != operation
        || request.value()["library"] != serial
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
            request.value()["change"]["kind"].as_str(),
            Some("media-import")
        ) || request.value()["change"]["kind"]
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
    serial: &str,
    epoch: u64,
    operation: Uuid,
    task: &Value,
) -> Result<()> {
    let queue = tr
        .get(&identity_key(ns, serial, b'q', epoch, operation), false)
        .await?;
    let valid = if matches!(task["status"].as_str(), Some("queued" | "running")) {
        queue.as_deref() == Some(&[])
    } else {
        queue.is_none()
    };
    if !valid {
        return Err(CatalogError::Corrupt("管理回执与排队索引不符"));
    }
    Ok(())
}
async fn read_task(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    epoch: u64,
    operation: Uuid,
) -> Result<Option<Value>> {
    require_retained(window(tr, ns).await?, epoch, operation)?;
    let raw = tr
        .get(&identity_key(ns, serial, b'o', epoch, operation), false)
        .await?;
    let Some(raw) = raw else {
        if tr
            .get(&identity_key(ns, serial, b'q', epoch, operation), false)
            .await?
            .is_some()
        {
            return Err(CatalogError::Corrupt("管理队列存在孤立请求"));
        }
        notifications::require_changed_at(
            tr,
            ns,
            serial,
            &task_entity(epoch, operation),
            "task.status.changed",
            None,
        )
        .await?;
        return Ok(None);
    };
    let value = task(&raw, serial, ns, epoch, operation)?;
    require_queue(tr, ns, serial, epoch, operation, &value).await?;
    notifications::require_changed_at(
        tr,
        ns,
        serial,
        &task_entity(epoch, operation),
        "task.status.changed",
        Some(&task_event_view(&value)),
    )
    .await?;
    Ok(Some(value))
}
fn task_entity(epoch: u64, operation: Uuid) -> String {
    format!("task:{epoch}:{operation}")
}
fn task_event_view(value: &Value) -> Value {
    let change = &value["request"]["change"];
    let pool = if matches!(change["kind"].as_str(), Some("pool" | "pool-delete")) {
        &change["id"]
    } else {
        &change["pool"]
    };
    json!({"task_id":format!("{}:{}:{}:{}",value["namespace"].as_str().expect("validated namespace"),value["library_serial"].as_str().expect("validated library"),value["epoch"].as_u64().expect("validated epoch"),value["operation"].as_str().expect("validated operation")),"kind":change["kind"],"pool":pool,"barcode":change["barcode"],"status":value["status"]})
}
async fn write_task(tr: &Transaction, request: &ManagementRequest, value: &Value) -> Result<()> {
    let serial = request.value()["library"]
        .as_str()
        .expect("validated library");
    let raw = encode(value);
    task(
        &raw,
        serial,
        request.namespace(),
        request.epoch(),
        request.operation(),
    )?;
    let key = identity_key(
        request.namespace(),
        serial,
        b'o',
        request.epoch(),
        request.operation(),
    );
    let previous = match tr.get(&key, false).await? {
        Some(old) => {
            let old = task(
                &old,
                serial,
                request.namespace(),
                request.epoch(),
                request.operation(),
            )?;
            if old["fingerprint"] != value["fingerprint"]
                || old["created_at_millis"] != value["created_at_millis"]
                || (matches!(
                    old["status"].as_str(),
                    Some("succeeded" | "failed" | "cancelled")
                ) && old != *value)
            {
                return Err(CatalogError::Corrupt("任务原身份、创建时间或终态被改写"));
            }
            Some(task_event_view(&old))
        }
        None => {
            if value["status"] != "queued" {
                return Err(CatalogError::Corrupt("管理任务必须先持久排队"));
            }
            None
        }
    };
    tr.set(&key, &raw);
    notifications::changed_at(
        tr,
        request.namespace(),
        serial,
        &task_entity(request.epoch(), request.operation()),
        "task.status.changed",
        previous.as_ref(),
        task_event_view(value),
    )
    .await
}

impl ManagedCatalog {
    pub async fn active_management_window(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<RequestWindow> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        window(&tr, ns).await
    }
    /// Persist intent only. Every retry keeps the same operation identity.
    pub async fn admit_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
    ) -> Result<Value> {
        if request.value()["library"] != serial || request.namespace() != route.route.namespace {
            return Err(CatalogError::Invalid("管理请求与带库或命名空间不符"));
        }
        self.db.run(|tr,_|async move {requests::retry_result(async {
            routing::require_runtime_active_owner(&tr,serial,route,owner).await?;
            let ns=request.namespace();
            let w=window(&tr,ns).await?;
            if let Some(saved)=read_task(&tr,ns,serial,request.epoch(),request.operation()).await? {
                if saved["fingerprint"]!=request.fingerprint() {return Err(CatalogError::Invalid("管理请求身份已由不同参数使用"));}
                return Ok(saved);
            }
            if request.epoch()!=w.accepting {return Err(CatalogError::Invalid("管理请求代次已关闭"));}
            let queued=read_page(&tr,&library_prefix(ns,serial,b'q'),None,MAX_QUEUED,false).await?;
            if queued.items.len()>=MAX_QUEUED {return Err(CatalogError::ManagementQueueFull);}
            let value=json!({"version":1,"library_serial":serial,"namespace":ns.to_string(),
                "operation":request.operation().to_string(),"epoch":request.epoch(),"request":request.value(),
                "fingerprint":request.fingerprint(),"status":"queued","created_at_millis":now(),
                "completed_at_millis":null,"owner_epoch":0,"result":null});
            write_task(&tr,request,&value).await?;
            tr.set(&identity_key(ns,serial,b'q',request.epoch(),request.operation()),b"");
            Ok(value)
        }.await)}).await?
    }
    /// Cancel only work which has not started. Never release a running actor's resources.
    pub async fn cancel_management(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Value> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
                        let ns = route.route.namespace;
                        let mut value = read_task(&tr, ns, serial, epoch, operation)
                            .await?
                            .ok_or(CatalogError::Invalid("管理请求不存在"))?;
                        if value["status"] == "running" {
                            return Err(CatalogError::Invalid(
                                "执行中任务须由对应设备执行器安全取消",
                            ));
                        }
                        if value["status"] != "queued" {
                            return Ok(value);
                        }
                        let request = ManagementRequest::parse(value["request"].clone())?;
                        value["status"] = json!("cancelled");
                        value["owner_epoch"] = json!(owner);
                        value["result"] = json!({"cancelled":true});
                        value["completed_at_millis"] = json!(
                            now().max(value["created_at_millis"].as_u64().expect("validated time"))
                        );
                        write_task(&tr, &request, &value).await?;
                        tr.clear(&identity_key(ns, serial, b'q', epoch, operation));
                        Ok(value)
                    }
                    .await,
                )
            })
            .await?
    }
    pub async fn management_task(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        read_task(&tr, ns, serial, epoch, operation).await
    }
    pub async fn list_management_tasks(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<Value>> {
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid("管理分页须在1..=100"));
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let w = window(&tr, ns).await?;
        let prefix = library_prefix(ns, serial, b'o');
        if after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24) {
            return Err(CatalogError::Invalid("管理分页游标无效"));
        }
        let rows = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::new();
        for (key, raw) in rows.items {
            if key.len() != prefix.len() + 24 {
                return Err(CatalogError::Corrupt("管理请求键编码"));
            }
            let epoch = read_u64(&key[prefix.len()..prefix.len() + 8]);
            let operation = uuid_from(&key[prefix.len() + 8..])?;
            require_retained(w, epoch, operation)
                .map_err(|_| CatalogError::Corrupt("管理记录代次无效"))?;
            let value = task(&raw, serial, ns, epoch, operation)?;
            require_queue(&tr, ns, serial, epoch, operation, &value).await?;
            notifications::require_changed_at(
                &tr,
                ns,
                serial,
                &task_entity(epoch, operation),
                "task.status.changed",
                Some(&task_event_view(&value)),
            )
            .await?;
            items.push(value);
        }
        Ok(Page {
            items,
            next: rows.next,
        })
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
        let ns = route.route.namespace;
        let w = window(&tr, ns).await?;
        if let Some((epoch, id)) = after {
            require_retained(w, epoch, id)?;
        }
        let prefix = library_prefix(ns, serial, b'q');
        let cursor = after.map(|(epoch, id)| identity_key(ns, serial, b'q', epoch, id));
        let mut page = read_page(&tr, &prefix, cursor.as_deref(), 1, false).await?;
        if page.items.is_empty() && after.is_some() {
            page = read_page(&tr, &prefix, None, 1, false).await?;
        }
        let Some((key, value)) = page.items.first() else {
            return Ok(None);
        };
        if key.len() != prefix.len() + 24 || !value.is_empty() {
            return Err(CatalogError::Corrupt("管理队列索引编码"));
        }
        let epoch = read_u64(&key[prefix.len()..prefix.len() + 8]);
        let operation = uuid_from(&key[prefix.len() + 8..])?;
        read_task(&tr, ns, serial, epoch, operation)
            .await?
            .ok_or(CatalogError::Corrupt("管理队列存在孤立请求"))?;
        Ok(Some((epoch, operation)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(ns: Uuid, serial: &str, operation: Uuid, change: Value) -> ManagementRequest {
        ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":operation.to_string(),"change":change})).unwrap()
    }
    fn notify_head(ns: Uuid, serial: &str) -> Vec<u8> {
        let mut out = key(b'~');
        out.extend_from_slice(ns.as_bytes());
        append_component(&mut out, serial.as_bytes());
        out.push(b'h');
        out
    }
    #[tokio::test]
    #[ignore = "需要隔离FDB_TEST_CLUSTER；验证管理排队/通知原子性、接管、背压和安全取消，不访问设备"]
    async fn management_intents_are_atomic_bounded_fenced_and_library_scoped() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("LEDGER-{ns}");
        let challenge = Uuid::new_v4();
        let replies:Vec<_>=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,
            "term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},
            "members":{"1":"127.0.0.1:8001","2":"127.0.0.1:8002","3":"127.0.0.1:8003"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,
            "catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
        let route = c
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        let owner = route.initial_owner_epoch;
        let op = Uuid::new_v4();
        let first = request(ns, &serial, op, json!({"kind":"library-rescan"}));
        let mut projection_key = key(b'~');
        projection_key.extend_from_slice(ns.as_bytes());
        append_component(&mut projection_key, serial.as_bytes());
        projection_key.push(b's');
        append_component(&mut projection_key, format!("task:1:{op}").as_bytes());
        let orphan = json!({"task_id":format!("{ns}:{serial}:1:{op}"),"kind":"library-rescan","pool":null,"barcode":null,"status":"queued"});
        let tr = c.db.create_trx().unwrap();
        tr.set(&projection_key, orphan.to_string().as_bytes());
        tr.commit().await.unwrap();
        assert!(
            c.admit_management(&serial, route, owner, &first)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        for kind in *b"oq" {
            assert!(
                tr.get(&identity_key(ns, &serial, kind, 1, op), false)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            tr.get(&projection_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            orphan.to_string().as_bytes()
        );
        assert!(
            tr.get(&notify_head(ns, &serial), false)
                .await
                .unwrap()
                .is_none()
        );
        tr.clear(&projection_key);
        tr.commit().await.unwrap();
        let queued = c
            .admit_management(&serial, route, owner, &first)
            .await
            .unwrap();
        assert_eq!(queued["status"], "queued");
        assert_eq!(
            c.admit_management(&serial, route, owner, &first)
                .await
                .unwrap(),
            queued
        );
        let other = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            other.management_task(&serial, route, 1, op).await.unwrap(),
            Some(queued.clone())
        );
        let reused = request(
            ns,
            &serial,
            op,
            json!({"kind":"drive","serial":"DRV-A","revision":0,"patch":{"enabled":false}}),
        );
        assert!(matches!(
            c.admit_management(&serial, route, owner, &reused).await,
            Err(CatalogError::Invalid(_))
        ));
        assert_eq!(
            c.next_management(&serial, route, owner, None)
                .await
                .unwrap(),
            Some((1, op))
        );
        // Corrupt event state must abort task receipt AND queue mutation.
        let head = notify_head(ns, &serial);
        let tr = c.db.create_trx().unwrap();
        let saved_head = tr.get(&head, false).await.unwrap().unwrap().to_vec();
        tr.set(&head, b"bad");
        tr.commit().await.unwrap();
        let second = request(
            ns,
            &serial,
            Uuid::new_v4(),
            json!({"kind":"library-rescan"}),
        );
        assert!(
            c.admit_management(&serial, route, owner, &second)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        for kind in *b"oq" {
            assert!(
                tr.get(
                    &identity_key(ns, &serial, kind, 1, second.operation()),
                    false
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        tr.set(&head, &saved_head);
        tr.commit().await.unwrap();
        assert!(c.advance_owner_epoch(&serial, owner + 1).await.unwrap());
        assert!(matches!(
            c.admit_management(&serial, route, owner, &second).await,
            Err(CatalogError::OwnerFenced)
        ));
        assert!(matches!(
            c.cancel_management(&serial, route, owner, 1, op).await,
            Err(CatalogError::OwnerFenced)
        ));
        let owner = owner + 1;
        // A pre-written future projection cannot suppress cancellation events.
        let tr = c.db.create_trx().unwrap();
        let saved_projection = tr
            .get(&projection_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut future_projection: Value = serde_json::from_slice(&saved_projection).unwrap();
        future_projection["current"]["status"] = json!("cancelled");
        tr.set(&projection_key, &encode(&future_projection));
        tr.commit().await.unwrap();
        assert!(
            c.cancel_management(&serial, route, owner, 1, op)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&identity_key(ns, &serial, b'o', 1, op), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            encode(&queued)
        );
        assert!(
            tr.get(&identity_key(ns, &serial, b'q', 1, op), false)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            tr.get(&projection_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            encode(&future_projection)
        );
        tr.set(&projection_key, &saved_projection);
        tr.commit().await.unwrap();
        // The referenced original event must exist even when later events keep head healthy.
        c.inject_notification(
            &serial,
            route,
            Uuid::new_v4(),
            "library.monitor.changed",
            &json!({"test":"later log head"}),
        )
        .await
        .unwrap();
        let mut original_event_key = notify_head(ns, &serial);
        *original_event_key.last_mut().unwrap() = b'e';
        original_event_key.extend_from_slice(&1u64.to_be_bytes());
        let tr = c.db.create_trx().unwrap();
        let saved_event = tr
            .get(&original_event_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.clear(&original_event_key);
        tr.commit().await.unwrap();
        assert!(
            c.cancel_management(&serial, route, owner, 1, op)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&head, false).await.unwrap().unwrap().as_ref(),
            2u64.to_be_bytes()
        );
        assert_eq!(
            tr.get(&identity_key(ns, &serial, b'o', 1, op), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            encode(&queued)
        );
        assert!(
            tr.get(&identity_key(ns, &serial, b'q', 1, op), false)
                .await
                .unwrap()
                .is_some()
        );
        assert!(tr.get(&original_event_key, false).await.unwrap().is_none());
        tr.set(&original_event_key, &saved_event);
        tr.commit().await.unwrap();
        let cancelled = other
            .cancel_management(&serial, route, owner, 1, op)
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(
            c.cancel_management(&serial, route, owner, 1, op)
                .await
                .unwrap(),
            cancelled
        );
        assert_eq!(
            c.admit_management(&serial, route, owner, &first)
                .await
                .unwrap(),
            cancelled
        );
        assert_eq!(
            c.next_management(&serial, route, owner, None)
                .await
                .unwrap(),
            None
        );
        let consumer = c
            .create_notify_consumer(
                &serial,
                route,
                "tasks",
                &NotifyFilter {
                    event_types: vec!["task.status.changed".into()],
                    ..Default::default()
                },
                true,
            )
            .await
            .unwrap();
        let generation = Uuid::parse_str(consumer["generation"].as_str().unwrap()).unwrap();
        let events = c
            .notify_batch(&serial, route, "tasks", generation)
            .await
            .unwrap();
        assert_eq!(events["events"].as_array().unwrap().len(), 2);
        assert_eq!(events["events"][0]["data"]["current"]["status"], "queued");
        assert_eq!(
            events["events"][1]["data"]["current"]["status"],
            "cancelled"
        );
        assert_eq!(events["events"][0]["injected"], false);
        // Corrupt unrelated-library rows in the same namespace must be isolated.
        let foreign_op = Uuid::new_v4();
        let foreign_serial = "OTHER-LIBRARY";
        let tr = c.db.create_trx().unwrap();
        for kind in *b"oq" {
            tr.set(
                &identity_key(ns, foreign_serial, kind, 1, foreign_op),
                b"foreign-corrupt",
            );
        }
        tr.commit().await.unwrap();
        let listed = c
            .list_management_tasks(&serial, route, None, 100)
            .await
            .unwrap();
        assert_eq!(listed.items, vec![cancelled]);
        assert_eq!(
            c.next_management(&serial, route, owner, None)
                .await
                .unwrap(),
            None
        );
        assert!(
            c.list_management_tasks(
                &serial,
                route,
                Some(&identity_key(ns, foreign_serial, b'o', 1, foreign_op)),
                100
            )
            .await
            .is_err()
        );
        // Capacity is per library. A committed retry is still available at capacity.
        let mut pending = Vec::new();
        for _ in 0..MAX_QUEUED {
            let request = request(
                ns,
                &serial,
                Uuid::new_v4(),
                json!({"kind":"library-rescan"}),
            );
            let value = c
                .admit_management(&serial, route, owner, &request)
                .await
                .unwrap();
            pending.push((request, value));
        }
        assert!(matches!(
            c.admit_management(&serial, route, owner, &second).await,
            Err(CatalogError::ManagementQueueFull)
        ));
        assert_eq!(
            c.admit_management(&serial, route, owner, &pending[0].0)
                .await
                .unwrap(),
            pending[0].1
        );
        assert!(
            c.management_task(&serial, route, 1, second.operation())
                .await
                .unwrap()
                .is_none()
        );
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = c
                .list_management_tasks(&serial, route, cursor.as_deref(), 7)
                .await
                .unwrap();
            count += page.items.len();
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(count, MAX_QUEUED + 1);
        c.cancel_management(&serial, route, owner, 1, pending[0].0.operation())
            .await
            .unwrap();
        let third = request(
            ns,
            &serial,
            Uuid::new_v4(),
            json!({"kind":"library-rescan"}),
        );
        let (a, b) = tokio::join!(
            c.admit_management(&serial, route, owner, &second),
            other.admit_management(&serial, route, owner, &third)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        match (a, b) {
            (Ok(value), Err(CatalogError::ManagementQueueFull))
            | (Err(CatalogError::ManagementQueueFull), Ok(value)) => {
                assert_eq!(value["status"], "queued")
            }
            other => panic!("并发队列背压结果错误: {other:?}"),
        }
        // A synthetic running receipt cannot be cancelled by deleting its queue row.
        let running_op = pending[1].0.operation();
        let record = identity_key(ns, &serial, b'o', 1, running_op);
        let queue = identity_key(ns, &serial, b'q', 1, running_op);
        let mut running = pending[1].1.clone();
        running["status"] = json!("running");
        running["owner_epoch"] = json!(owner);
        let tr = c.db.create_trx().unwrap();
        write_task(&tr, &pending[1].0, &running).await.unwrap();
        tr.commit().await.unwrap();
        assert!(matches!(
            c.cancel_management(&serial, route, owner, 1, running_op)
                .await,
            Err(CatalogError::Invalid(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&record, false).await.unwrap().unwrap().as_ref(),
            encode(&running)
        );
        assert_eq!(tr.get(&queue, false).await.unwrap().unwrap().as_ref(), b"");
        // Own-library queue corruption fails closed without automatic repair.
        tr.set(&queue, b"corrupt");
        tr.commit().await.unwrap();
        assert!(matches!(
            c.management_task(&serial, route, 1, running_op).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            c.list_management_tasks(&serial, route, None, 100).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&queue, false).await.unwrap().unwrap().as_ref(),
            b"corrupt"
        );
        assert_eq!(
            tr.get(
                &identity_key(ns, foreign_serial, b'q', 1, foreign_op),
                false
            )
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
            b"foreign-corrupt"
        );
    }
}
