use super::*;

fn observation(serial: &str, owner: u64) -> Value {
    json!({"library_serial":serial,"owner":1,"round":owner,"observed_at":"2026-11-05T00:00:00.000000000Z",
        "volumes":[{"barcode":"A001L8","address":1001,"element_type":2,"medium_type":1,"source_address":null,"home_slot":1},
            {"barcode":"B001L8","address":1002,"element_type":2,"medium_type":null,"source_address":null,"home_slot":2}],
        "drives":[{"serial":"DRIVE-A","address":256,"barcode":null},{"serial":"DRIVE-B","address":257,"barcode":null}]})
}
fn event_key(ns: Uuid, serial: &str, sequence: u64) -> Vec<u8> {
    let mut k = notify_key(ns, serial, b'e');
    k.extend_from_slice(&sequence.to_be_bytes());
    k
}
fn notify_key(ns: Uuid, serial: &str, kind: u8) -> Vec<u8> {
    let mut k = key(b'~');
    k.extend_from_slice(ns.as_bytes());
    append_component(&mut k, serial.as_bytes());
    k.push(kind);
    k
}
fn projection_key(ns: Uuid, serial: &str, entity: &str) -> Vec<u8> {
    let mut k = notify_key(ns, serial, b's');
    append_component(&mut k, entity.as_bytes());
    k
}
async fn rows(c: &ManagedCatalog, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
    let tr = c.db.create_trx().unwrap();
    let mut values = Vec::new();
    for k in keys {
        values.push(tr.get(k, false).await.unwrap().map(|v| v.to_vec()));
    }
    values
}
async fn setup(c: &ManagedCatalog) -> (Uuid, String, ActiveLibraryRoute) {
    let ns = Uuid::new_v4();
    let serial = format!("OBS-{ns}");
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
    (ns, serial, route)
}
#[test]
fn inventory_semantics_ignore_order_owner_and_poll_time_but_detect_slot_moves() {
    let original = observation("LIB", 100);
    let first = input("LIB", 100, &original).unwrap();
    let mut next = original.clone();
    next["volumes"].as_array_mut().unwrap().reverse();
    next["drives"].as_array_mut().unwrap().reverse();
    next["owner"] = json!(2);
    next["round"] = json!(101);
    next["observed_at"] = json!("2026-11-05T00:01:00Z");
    let unchanged = input("LIB", 101, &next).unwrap();
    assert_eq!(inventory_view(&first), inventory_view(&unchanged));
    next["volumes"][1]["address"] = json!(1003);
    next["volumes"][1]["home_slot"] = json!(3);
    let moved = input("LIB", 101, &next).unwrap();
    assert_ne!(inventory_view(&first), inventory_view(&moved));
    assert_eq!(
        inventory_view(&first)["num_of_volumes"],
        inventory_view(&moved)["num_of_volumes"]
    );
}
#[test]
fn inventory_rejects_inconsistent_duplicate_unbounded_and_malformed_facts() {
    let original = observation("LIB", 100);
    let mut variants = Vec::new();
    for (path, value) in [
        ("/round", json!(99)),
        ("/owner", json!(0)),
        ("/owner", Value::Null),
        ("/observed_at", json!("bad")),
        ("/volumes/0/address", json!(65536)),
        ("/volumes/0/address", json!(1.5)),
        ("/volumes/0/element_type", json!(0)),
        ("/volumes/0/home_slot", json!(0)),
        ("/volumes/0/medium_type", json!(256)),
        ("/volumes/0/source_address", json!(65536)),
        ("/volumes/0/barcode", json!("bad/name")),
        ("/volumes/0/medium_type", json!({"nested":[1,2]})),
        ("/drives/0/serial", json!("bad serial")),
        ("/drives/0/barcode", json!("NOT-LOADED")),
        ("/drives/0/address", json!(1001)),
        ("/volumes/1/barcode", json!("A001L8")),
        ("/volumes/1/address", json!(1001)),
        ("/drives/1/serial", json!("DRIVE-A")),
        ("/drives/1/address", json!(256)),
    ] {
        let mut v = original.clone();
        *v.pointer_mut(path).unwrap() = value;
        variants.push(v);
    }
    let mut missing = original.clone();
    missing["volumes"][0]
        .as_object_mut()
        .unwrap()
        .remove("medium_type");
    variants.push(missing);
    let mut unknown = original.clone();
    unknown["callback"] = json!("not-supported");
    variants.push(unknown);
    let mut too_many = original.clone();
    too_many["volumes"] = json!(vec![original["volumes"][0].clone(); MAX_ITEMS + 1]);
    variants.push(too_many);
    for v in variants {
        assert!(
            matches!(input("LIB", 100, &v), Err(CatalogError::Invalid(_))),
            "{v}"
        );
    }
    // Unconfigured transfer elements may appear in a complete changer snapshot.
    let mut detached = original.clone();
    detached["volumes"][0]["address"] = json!(258);
    detached["volumes"][0]["element_type"] = json!(4);
    detached["volumes"][0]["home_slot"] = Value::Null;
    assert!(input("LIB", 100, &detached).is_ok());
    detached["drives"][0]["serial"] = json!("d".repeat(255));
    assert!(input("LIB", 100, &detached).is_ok());
}
#[test]
fn escaped_monitor_errors_fit_the_same_writer_and_reader_bound() {
    let r = MonitorRecord {
        version: 1,
        namespace: Uuid::new_v4().to_string(),
        library_serial: "LIB".into(),
        owner_epoch: 100,
        interval_seconds: 30,
        last_attempt: "2026-11-05T00:00:00Z".into(),
        last_success: None,
        error: Some("\0".repeat(2048)),
    };
    let raw = encode(&r).unwrap();
    assert!(raw.len() > 4096 && raw.len() < MAX_MONITOR_RECORD);
    assert_eq!(
        json_value(&serde_json::from_slice::<MonitorRecord>(&raw).unwrap()),
        json_value(&r)
    );
}

#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；仅合成库存/巡检事实与事件回滚，不发SCSI或调用EE"]
async fn library_observations_are_scoped_deduplicated_fenced_and_atomic_with_events() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (ns, serial, route) = setup(&c).await;
    let owner = route.initial_owner_epoch;
    let (ns_b, serial_b, route_b) = setup(&c).await;
    let owner_b = route_b.initial_owner_epoch;
    assert_eq!(
        c.active_library_inventory(&serial, route).await.unwrap(),
        None
    );
    assert_eq!(
        c.active_library_monitor(&serial, route).await.unwrap(),
        None
    );
    let consumer = c
        .create_notify_consumer(
            &serial,
            route,
            "audit",
            &NotifyFilter {
                event_types: vec![INVENTORY_EVENT.into(), MONITOR_EVENT.into()],
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    let generation = Uuid::parse_str(consumer["generation"].as_str().unwrap()).unwrap();
    let original = observation(&serial, owner);
    c.record_active_library_inventory(&serial, route, owner, &original)
        .await
        .unwrap();
    let mut next = original.clone();
    next["observed_at"] = json!("2026-11-05T00:00:01Z");
    c.record_active_library_inventory(&serial, route, owner, &next)
        .await
        .unwrap();
    let head = notify_key(ns, &serial, b'h');
    assert_eq!(
        rows(&c, std::slice::from_ref(&head)).await,
        vec![Some(1u64.to_be_bytes().to_vec())]
    );
    assert_eq!(
        c.active_library_inventory(&serial, route)
            .await
            .unwrap()
            .unwrap()["observed_at"],
        next["observed_at"]
    );
    // Taking over does not rewrite the original observer identity during reads.
    let current = owner + 10;
    c.claim_active_owner_epoch(&serial, current).await.unwrap();
    assert_eq!(
        c.active_library_inventory(&serial, route)
            .await
            .unwrap()
            .unwrap()["round"],
        owner
    );
    next["round"] = json!(current);
    next["owner"] = json!(2);
    c.record_active_library_inventory(&serial, route, current, &next)
        .await
        .unwrap();
    assert_eq!(
        rows(&c, std::slice::from_ref(&head)).await,
        vec![Some(1u64.to_be_bytes().to_vec())]
    );
    let cache = inventory_key(ns, &serial);
    let before = rows(&c, &[cache.clone(), head.clone()]).await;
    assert!(matches!(
        c.record_active_library_inventory(&serial, route, owner, &original)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert_eq!(rows(&c, &[cache.clone(), head.clone()]).await, before);
    // Same cartridge count, different slot: an event must still be appended.
    next["volumes"][0]["address"] = json!(1003);
    next["volumes"][0]["home_slot"] = json!(3);
    c.record_active_library_inventory(&serial, route, current, &next)
        .await
        .unwrap();
    assert_eq!(
        rows(&c, std::slice::from_ref(&head)).await,
        vec![Some(2u64.to_be_bytes().to_vec())]
    );
    next["volumes"].as_array_mut().unwrap().reverse();
    next["drives"].as_array_mut().unwrap().reverse();
    c.record_active_library_inventory(&serial, route, current, &next)
        .await
        .unwrap();
    assert_eq!(
        rows(&c, std::slice::from_ref(&head)).await,
        vec![Some(2u64.to_be_bytes().to_vec())]
    );
    next = input(&serial, current, &next)
        .map(|v| json_value(&v))
        .unwrap();
    let a = next["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .position(|v| v["barcode"] == "A001L8")
        .unwrap();
    next["volumes"][a]["address"] = json!(256);
    next["volumes"][a]["element_type"] = json!(4);
    next["volumes"][a]["home_slot"] = Value::Null;
    next["volumes"][a]["source_address"] = json!(1003);
    next["drives"][0]["barcode"] = json!("A001L8");
    c.record_active_library_inventory(&serial, route, current, &next)
        .await
        .unwrap();
    let canonical = c
        .active_library_inventory(&serial, route)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical["drives"][0]["barcode"], "A001L8");
    c.record_library_monitor(&serial, route, current, Some("timeout"))
        .await
        .unwrap();
    assert!(
        c.active_library_monitor(&serial, route)
            .await
            .unwrap()
            .unwrap()["last_success"]
            .is_null()
    );
    c.record_library_monitor(&serial, route, current, Some("timeout"))
        .await
        .unwrap();
    let escaped = "\0".repeat(2048);
    c.record_library_monitor(&serial, route, current, Some(&escaped))
        .await
        .unwrap();
    assert_eq!(
        c.active_library_monitor(&serial, route)
            .await
            .unwrap()
            .unwrap()["error"],
        escaped
    );
    c.record_library_monitor(&serial, route, current, None)
        .await
        .unwrap();
    c.record_library_monitor(&serial, route, current, None)
        .await
        .unwrap();
    let success = c
        .active_library_monitor(&serial, route)
        .await
        .unwrap()
        .unwrap()["last_success"]
        .clone();
    assert!(success.is_string());
    // A successful record must carry this attempt's success time. Losing it
    // must be reported, not silently repaired by another healthy heartbeat.
    let monitor = monitor_key(ns, &serial);
    let tr = c.db.create_trx().unwrap();
    let saved_healthy = tr.get(&monitor, false).await.unwrap().unwrap().to_vec();
    let mut damaged: Value = serde_json::from_slice(&saved_healthy).unwrap();
    damaged["last_success"] = Value::Null;
    tr.set(&monitor, damaged.to_string().as_bytes());
    tr.commit().await.unwrap();
    assert!(matches!(
        c.active_library_monitor(&serial, route).await,
        Err(CatalogError::Corrupt(_))
    ));
    let before = rows(&c, std::slice::from_ref(&monitor)).await;
    assert!(
        c.record_library_monitor(&serial, route, current, None)
            .await
            .is_err()
    );
    assert_eq!(rows(&c, std::slice::from_ref(&monitor)).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.set(&monitor, &saved_healthy);
    tr.commit().await.unwrap();
    c.record_library_monitor(&serial, route, current, Some("drive unavailable"))
        .await
        .unwrap();
    assert_eq!(
        c.active_library_monitor(&serial, route)
            .await
            .unwrap()
            .unwrap()["last_success"],
        success
    );
    assert_eq!(
        rows(&c, std::slice::from_ref(&head)).await,
        vec![Some(7u64.to_be_bytes().to_vec())]
    );
    let keys = vec![cache.clone(), monitor_key(ns, &serial), head.clone()];
    let before = rows(&c, &keys).await;
    assert!(matches!(
        c.record_library_monitor(&serial, route, owner, None).await,
        Err(CatalogError::OwnerFenced)
    ));
    let mut oversized = observation(&serial, current);
    oversized["volumes"]=json!((0..700).map(|i|json!({"barcode":format!("{i:04}{}","X".repeat(60)),"address":i+1,"element_type":2,"medium_type":1,"source_address":null,"home_slot":i+1})).collect::<Vec<_>>());
    oversized["drives"] = json!([]);
    assert!(matches!(
        c.record_active_library_inventory(&serial, route, current, &oversized)
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(rows(&c, &keys).await, before);
    // Another library's broken cache cannot poison the healthy library.
    let other = observation(&serial_b, owner_b);
    c.record_active_library_inventory(&serial_b, route_b, owner_b, &other)
        .await
        .unwrap();
    c.record_library_monitor(&serial_b, route_b, owner_b, None)
        .await
        .unwrap();
    let other_key = inventory_key(ns_b, &serial_b);
    let tr = c.db.create_trx().unwrap();
    let saved_other = tr.get(&other_key, false).await.unwrap().unwrap().to_vec();
    tr.set(&other_key, b"broken");
    tr.commit().await.unwrap();
    assert!(
        c.active_library_inventory(&serial_b, route_b)
            .await
            .is_err()
    );
    assert_eq!(
        c.active_library_inventory(&serial, route).await.unwrap(),
        Some(canonical.clone())
    );
    assert!(
        c.record_active_library_inventory(&serial_b, route_b, owner_b, &other)
            .await
            .is_err()
    );
    assert_eq!(
        rows(&c, std::slice::from_ref(&other_key)).await,
        vec![Some(b"broken".to_vec())]
    );
    let tr = c.db.create_trx().unwrap();
    tr.set(&other_key, &saved_other);
    tr.commit().await.unwrap();
    // Missing projection, original event, or cache is rejected and never rebuilt.
    let projection = projection_key(ns, &serial, INVENTORY_ENTITY);
    let tr = c.db.create_trx().unwrap();
    let raw = tr.get(&projection, false).await.unwrap().unwrap().to_vec();
    let p: Value = serde_json::from_slice(&raw).unwrap();
    let event = event_key(ns, &serial, p["sequence"].as_u64().unwrap());
    for damaged in [projection.clone(), event.clone(), cache.clone()] {
        let tr = c.db.create_trx().unwrap();
        let saved = tr.get(&damaged, false).await.unwrap().unwrap().to_vec();
        tr.clear(&damaged);
        tr.commit().await.unwrap();
        let tracked = [keys.clone(), vec![projection.clone(), event.clone()]].concat();
        let before = rows(&c, &tracked).await;
        assert!(c.active_library_inventory(&serial, route).await.is_err());
        assert!(
            c.record_active_library_inventory(&serial, route, current, &canonical)
                .await
                .is_err()
        );
        assert_eq!(rows(&c, &tracked).await, before);
        let tr = c.db.create_trx().unwrap();
        tr.set(&damaged, &saved);
        tr.commit().await.unwrap();
    }
    // A well-formed but altered snapshot cannot borrow the old semantic proof.
    let tr = c.db.create_trx().unwrap();
    let saved_cache = tr.get(&cache, false).await.unwrap().unwrap().to_vec();
    let mut tampered: Value = serde_json::from_slice(&saved_cache).unwrap();
    tampered["observation"]["volumes"][0]["source_address"] = json!(12);
    tr.set(&cache, tampered.to_string().as_bytes());
    tr.commit().await.unwrap();
    let before = rows(&c, &keys).await;
    assert!(c.active_library_inventory(&serial, route).await.is_err());
    assert!(
        c.record_active_library_inventory(&serial, route, current, &canonical)
            .await
            .is_err()
    );
    assert_eq!(rows(&c, &keys).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.set(&cache, &saved_cache);
    tr.commit().await.unwrap();
    // Both writers fail at notification append after all their record validation.
    let mut changed = canonical.clone();
    changed["volumes"][0]["source_address"] = json!(12);
    let tr = c.db.create_trx().unwrap();
    let saved_head = tr.get(&head, false).await.unwrap().unwrap().to_vec();
    tr.set(&head, &u64::MAX.to_be_bytes());
    tr.commit().await.unwrap();
    let tracked = [
        keys.clone(),
        vec![
            projection.clone(),
            projection_key(ns, &serial, MONITOR_ENTITY),
        ],
    ]
    .concat();
    let before = rows(&c, &tracked).await;
    let error = c
        .record_active_library_inventory(&serial, route, current, &changed)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("通知序号耗尽"), "{error}");
    let error = c
        .record_library_monitor(&serial, route, current, Some("new error"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("通知序号耗尽"), "{error}");
    assert_eq!(rows(&c, &tracked).await, before);
    let tr = c.db.create_trx().unwrap();
    tr.set(&head, &saved_head);
    tr.commit().await.unwrap();
    c.record_active_library_inventory(&serial, route, current, &changed)
        .await
        .unwrap();
    c.record_library_monitor(&serial, route, current, Some("new error"))
        .await
        .unwrap();
    let batch = c
        .notify_batch(&serial, route, "audit", generation)
        .await
        .unwrap();
    assert_eq!(batch["through"], 9);
    assert_eq!(batch["events"].as_array().unwrap().len(), 9);
    assert_eq!(
        batch["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == INVENTORY_EVENT)
            .count(),
        4
    );
    assert!(
        batch["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["injected"] == false && e["library"] == serial)
    );
    assert_eq!(
        batch["events"][0]["data"]["current"]["num_of_volumes"],
        batch["events"][1]["data"]["current"]["num_of_volumes"]
    );
    assert_ne!(
        batch["events"][0]["data"]["current"]["snapshot_sha256"],
        batch["events"][1]["data"]["current"]["snapshot_sha256"]
    );
}
