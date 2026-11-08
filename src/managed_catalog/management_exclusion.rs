//! EE-style administrative task exclusion within one physical library.
//! The original task owns the durable slot; timeout or HA never releases it.
use super::*;

fn prefix_for(namespace: Uuid, serial: &str) -> Vec<u8> {
    let mut key = prefix(namespace, b'E');
    append_component(&mut key, serial.as_bytes());
    key
}
pub(in crate::managed_catalog::control) fn key(namespace: Uuid, serial: &str) -> Vec<u8> {
    let mut key = prefix_for(namespace, serial);
    key.push(b'p');
    key
}
fn marker_prefix(namespace: Uuid, serial: &str) -> Vec<u8> {
    let mut key = prefix_for(namespace, serial);
    key.push(b'r');
    key
}

pub(in crate::managed_catalog::control) struct LibraryTaskClaim {
    namespace: Uuid,
    serial: String,
    slot: Vec<u8>,
    marker: Vec<u8>,
    token: Vec<u8>,
}
impl LibraryTaskClaim {
    fn new(value: Value) -> Result<Self> {
        let namespace = value["namespace"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(CatalogError::Corrupt("带库互斥命名空间无效"))?;
        let serial = value["library"]
            .as_str()
            .ok_or(CatalogError::Corrupt("带库互斥库身份无效"))?
            .to_owned();
        validate(&value, namespace, &serial)?;
        let mut marker = marker_prefix(namespace, &serial);
        marker.extend_from_slice(
            &value["epoch"]
                .as_u64()
                .expect("validated epoch")
                .to_be_bytes(),
        );
        marker.extend_from_slice(
            Uuid::parse_str(value["operation"].as_str().expect("validated operation"))
                .expect("validated UUID")
                .as_bytes(),
        );
        Ok(Self {
            namespace,
            slot: key(namespace, &serial),
            serial,
            marker,
            token: encode(&value),
        })
    }
    pub(in crate::managed_catalog::control) fn for_request(
        request: &ManagementRequest,
    ) -> Result<Self> {
        Self::new(
            json!({"version":1,"namespace":request.namespace().to_string(),
            "library":request.value()["library"],"kind":request.value()["change"]["kind"],
            "epoch":request.epoch(),"operation":request.operation().to_string(),"fingerprint":request.fingerprint()}),
        )
    }
    pub(in crate::managed_catalog::control) fn for_reclaim(parent: &Value) -> Result<Self> {
        Self::new(
            json!({"version":1,"namespace":parent["namespace"],"library":parent["library"],
            "kind":"media-reclaim","epoch":parent["epoch"],"operation":parent["operation"],"fingerprint":parent["fingerprint"]}),
        )
    }
    pub(in crate::managed_catalog::control) async fn require_owned(
        &self,
        tr: &Transaction,
    ) -> Result<()> {
        if tr.get(&self.slot, false).await?.as_deref() != Some(self.token.as_slice()) {
            return Err(CatalogError::Corrupt("带库管理互斥占用缺失或不属于原任务"));
        }
        let prefix = marker_prefix(self.namespace, &self.serial);
        let options = foundationdb::RangeOption {
            begin: foundationdb::KeySelector::first_greater_or_equal(prefix.clone()),
            end: foundationdb::KeySelector::first_greater_or_equal(prefix_end(&prefix)),
            limit: Some(2),
            ..Default::default()
        };
        let rows = tr.get_range(&options, 1, false).await?;
        if rows.len() != 1 || rows[0].key() != self.marker || rows[0].value() != self.token {
            return Err(CatalogError::Corrupt("带库管理互斥任务索引缺失或冲突"));
        }
        Ok(())
    }
    /// A replay may observe a complete successor, but never an orphaned claim.
    pub(in crate::managed_catalog::control) async fn require_released(
        &self,
        tr: &Transaction,
    ) -> Result<()> {
        if tr.get(&self.marker, false).await?.is_some() {
            return Err(CatalogError::Corrupt("原任务记录缺失但带库互斥占用仍在"));
        }
        match require_available_at(tr, self.namespace, &self.serial).await {
            Err(CatalogError::PoolBusy) => Ok(()),
            result => result,
        }
    }
    /// Caller must prove availability/ownership in this same transaction first.
    /// All encoding is complete, so these mutations cannot partially fail.
    pub(in crate::managed_catalog::control) fn acquire(&self, tr: &Transaction) {
        tr.set(&self.slot, &self.token);
        tr.set(&self.marker, &self.token);
    }
    pub(in crate::managed_catalog::control) fn release(&self, tr: &Transaction) {
        tr.clear(&self.slot);
        tr.clear(&self.marker);
    }
}

fn validate(value: &Value, namespace: Uuid, serial: &str) -> Result<()> {
    let fields = [
        "version",
        "namespace",
        "library",
        "kind",
        "epoch",
        "operation",
        "fingerprint",
    ];
    if !value
        .as_object()
        .is_some_and(|m| m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str())))
        || value["version"] != 1
        || value["namespace"] != namespace.to_string()
        || value["library"] != serial
        || namespace.is_nil()
        || validate_physical_serial(serial).is_err()
        || !matches!(
            value["kind"].as_str(),
            Some("library-rescan" | "media-reclaim" | "media-unassign")
        )
        || !value["epoch"].as_u64().is_some_and(|v| v > 0)
        || !value["operation"]
            .as_str()
            .is_some_and(|s| Uuid::parse_str(s).is_ok_and(|id| !id.is_nil() && id.to_string() == s))
        || !value["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(CatalogError::Corrupt("带库管理互斥记录字段或原身份无效"));
    }
    Ok(())
}

pub(in crate::managed_catalog::control) async fn require_available_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<()> {
    if let Some(raw) = tr.get(&key(ns, serial), false).await? {
        if raw.len() > 1024 {
            return Err(CatalogError::Corrupt("带库管理互斥记录过大"));
        }
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|_| CatalogError::Corrupt("带库管理互斥记录 JSON 损坏"))?;
        validate(&value, ns, serial)?;
        if encode(&value) != raw.as_ref() {
            return Err(CatalogError::Corrupt("带库管理互斥记录非规范编码"));
        }
        LibraryTaskClaim::new(value)?.require_owned(tr).await?;
        return Err(CatalogError::PoolBusy);
    }
    if requests::prefix_has_rows(tr, &marker_prefix(ns, serial)).await? {
        return Err(CatalogError::Corrupt(
            "带库管理互斥占用缺失但原任务索引仍在",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_tokens_bind_original_scope_and_task_without_execution_owner() {
        let ns = Uuid::new_v4();
        let serial = "EXCLUSION-LIB";
        let request = ManagementRequest::parse(json!({"version":1,"namespace":ns.to_string(),"library":serial,
            "epoch":u64::MAX,"operation":Uuid::new_v4().to_string(),"change":{"kind":"library-rescan"}})).unwrap();
        let raw = LibraryTaskClaim::for_request(&request).unwrap().token;
        let value: Value = serde_json::from_slice(&raw).unwrap();
        validate(&value, ns, serial).unwrap();
        assert!(validate(&value, Uuid::new_v4(), serial).is_err());
        assert!(validate(&value, ns, "OTHER-LIB").is_err());
        assert_ne!(key(ns, serial), key(ns, "OTHER-LIB"));
        assert_ne!(key(ns, serial), key(Uuid::new_v4(), serial));
        for (field, bad) in [
            ("kind", json!("tape-list")),
            ("epoch", json!(0)),
            ("operation", json!(Uuid::nil().to_string())),
            ("fingerprint", json!("FF".repeat(32))),
            ("version", json!(2)),
        ] {
            let mut changed = value.clone();
            changed[field] = bad;
            assert!(validate(&changed, ns, serial).is_err(), "{field}");
        }
    }

    async fn library(catalog: &ManagedCatalog) -> (String, ActiveLibraryRoute) {
        let ns = Uuid::new_v4();
        let serial = format!("EXCLUSION-{ns}");
        let challenge = Uuid::new_v4();
        let replies: Vec<_> = (1..=3).map(|node| json!({"version":1,"library_serial":serial,
            "challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,
            "role":if node==2 {"Leader"} else {"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,
            "catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
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
        (serial, route)
    }
    async fn rescan(
        c: &ManagedCatalog,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> ManagementRequest {
        let request = ManagementRequest::parse(json!({"version":1,"namespace":route.route.namespace.to_string(),
            "library":serial,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"library-rescan"}})).unwrap();
        c.admit_management(serial, route, 100, &request)
            .await
            .unwrap();
        request
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；同库任务互斥/异库并行及 HA 故障，不访问设备"]
    async fn library_task_exclusion_is_durable_scoped_and_never_repairs_missing_ownership() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (a, route_a) = library(&c).await;
        let (b, route_b) = library(&c).await;
        let first = rescan(&c, &a, route_a).await;
        let second = rescan(&c, &a, route_a).await;
        let other = rescan(&c, &b, route_b).await;
        assert!(
            matches!(
                c.start_native_device_task(&a, route_a, 100, first.epoch(), first.operation())
                    .await
                    .as_ref()
                    .map_err(|e| e.domain()),
                Err(CatalogError::Invalid(_))
            ),
            "rescan 不能从通用设备任务入口绕开专属互斥"
        );
        let (one, two) = tokio::join!(
            c.begin_library_rescan(&a, route_a, 100, &first),
            c.begin_library_rescan(&a, route_a, 100, &second),
        );
        let (first, second, started) = match (one, two) {
            (Ok(started), Err(error)) if matches!(error.domain(), CatalogError::PoolBusy) => {
                (first, second, started)
            }
            (Err(error), Ok(started)) if matches!(error.domain(), CatalogError::PoolBusy) => {
                (second, first, started)
            }
            other => panic!("同库并发只允许一个任务开始: {other:?}"),
        };
        let claim = LibraryTaskClaim::for_request(&first).unwrap();
        assert_eq!(started["status"], "running");
        assert!(matches!(
            c.begin_library_rescan(&a, route_a, 100, &second).await,
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            c.begin_library_rescan(&b, route_b, 100, &other)
                .await
                .unwrap()["status"],
            "running"
        );
        for key in [&claim.slot, &claim.marker] {
            let tr = c.db.create_trx().unwrap();
            let original = tr.get(key, false).await.unwrap().unwrap().to_vec();
            tr.clear(key);
            tr.commit().await.unwrap();
            assert!(
                c.begin_library_rescan(&a, route_a, 100, &first)
                    .await
                    .is_err()
            );
            assert!(
                c.begin_library_rescan(&a, route_a, 100, &second)
                    .await
                    .is_err()
            );
            let tr = c.db.create_trx().unwrap();
            assert!(tr.get(key, false).await.unwrap().is_none());
            tr.set(key, &original);
            tr.commit().await.unwrap();
        }
        c.claim_active_owner_epoch(&a, 101).await.unwrap();
        assert!(matches!(
            c.begin_library_rescan(&a, route_a, 100, &first).await,
            Err(CatalogError::OwnerFenced)
        ));
        let claimed = c
            .begin_library_rescan(&a, route_a, 101, &first)
            .await
            .unwrap();
        assert_eq!(claimed["request"], first.value().clone());
        assert_eq!(claimed["owner_epoch"], 101);
        let captured = "2026-11-07T00:00:00Z";
        c.record_library_monitor(&a, route_a, 101, captured, Some("test read failure"))
            .await
            .unwrap();
        let failed = c
            .complete_library_rescan(
                &a,
                route_a,
                101,
                &first,
                captured,
                Some("test read failure"),
            )
            .await
            .unwrap();
        assert_eq!(failed["status"], "failed");
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&claim.slot, false).await.unwrap().is_none());
        assert!(tr.get(&claim.marker, false).await.unwrap().is_none());
        c.begin_library_rescan(&a, route_a, 101, &second)
            .await
            .unwrap();
        let next = LibraryTaskClaim::for_request(&second).unwrap();
        assert_eq!(
            c.complete_library_rescan(
                &a,
                route_a,
                101,
                &first,
                captured,
                Some("test read failure")
            )
            .await
            .unwrap(),
            failed
        );
        next.require_owned(&c.db.create_trx().unwrap())
            .await
            .unwrap();
        assert_eq!(
            c.begin_library_rescan(&b, route_b, 100, &other)
                .await
                .unwrap()["owner_epoch"],
            100
        );
    }
}
