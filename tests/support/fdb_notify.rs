//! Real FDB + HTTP/SSE failover, with simulated hardware only.
use super::active_pools::context;
use super::*;
use std::collections::BTreeMap;
use tape_rs::client::fdb::FdbClient;
use tape_rs::daemon::fdb_http;
use tape_rs::managed_catalog::{NotifyConnectionAction, NotifyFilter};

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；通知原子持久化、HTTP/SSE、ACK 和跨节点恢复，不接触真实设备"]
fn notify_survives_reconnect_owner_change_and_rejects_skipped_ack() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    owner.expect(0);
    let ctx = context(&owner, 71, 100);
    let router = || {
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup")
    };
    let first = ReadHttpServer::start(router());
    let second = ReadHttpServer::start(router());
    let a = FdbClient::new([&first.address]).unwrap();
    let b = FdbClient::new([&second.address]).unwrap();
    let original_assignment = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap();
    let injected_sub = a
        .create_notify_consumer(
            &f.serial,
            "injection",
            json!({"barcodes":["MOCKL8"],"event_types":["tape.state.changed"]}),
            false,
        )
        .unwrap();
    let injection = Uuid::new_v4();
    let payload = json!({"barcode":"MOCKL8","current":{"state":"need_replace"}});
    let event = a
        .inject_notification(&f.serial, injection, "tape.state.changed", payload.clone())
        .unwrap();
    assert_eq!(event["injected"], true);
    assert_eq!(event["origin"], "injected");
    assert_eq!(event["injection_id"], injection.to_string());
    assert_eq!(
        event,
        b.inject_notification(&f.serial, injection, "tape.state.changed", payload)
            .unwrap()
    );
    assert!(
        b.inject_notification(
            &f.serial,
            injection,
            "tape.state.changed",
            json!({"barcode":"different"})
        )
        .is_err()
    );
    assert!(
        b.inject_notification(&f.serial, Uuid::new_v4(), "unknown", json!({}))
            .is_err()
    );
    assert!(
        b.inject_notification(
            &f.serial,
            Uuid::new_v4(),
            "tape.state.changed",
            json!({"large":"x".repeat(8192)})
        )
        .is_err()
    );
    let injected_generation =
        Uuid::parse_str(injected_sub["generation"].as_str().unwrap()).unwrap();
    let delivered = b
        .notify_batch(&f.serial, "injection", injected_generation)
        .unwrap();
    assert_eq!(delivered["events"], json!([event]));
    assert_eq!(
        rt.block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
            .unwrap(),
        original_assignment
    );
    let filter = json!({"barcodes":[BARCODES[0]],"event_types":["tape.state.changed"]});
    let sub = a
        .create_notify_consumer(&f.serial, "ops", filter.clone(), false)
        .unwrap();
    let generation = Uuid::parse_str(sub["generation"].as_str().unwrap()).unwrap();
    assert_eq!(
        a.create_notify_consumer(&f.serial, "ops", filter.clone(), false)
            .unwrap(),
        sub
    );
    assert!(
        a.create_notify_consumer(&f.serial, "ops", json!({}), false)
            .is_err()
    );
    let other = b
        .create_notify_consumer(&f.serial, "other", json!({}), false)
        .unwrap();
    let other_gen = Uuid::parse_str(other["generation"].as_str().unwrap()).unwrap();
    let assignment = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let policy=a.prepare_control(&f.serial,f.route.route.namespace,json!({"kind":"tape","pool":f.pools[0].to_string(),"barcode":BARCODES[0],"volume":assignment.volume_id.unwrap().to_string(),"revision":1,"patch":{"online":false}})).unwrap();
    let saved = f.root.join("notify-policy.json");
    policy.save_new(&saved).unwrap();
    let task = a.submit_management_saved(&saved).unwrap();
    assert!(matches!(
        task["status"].as_str(),
        Some("queued" | "succeeded")
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if a.management_status(&policy).unwrap().unwrap()["status"] == "succeeded" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let batch = a.notify_batch(&f.serial, "ops", generation).unwrap();
    assert!(
        batch["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["current"]["state"] == "offline")
    );
    let delivery = Uuid::parse_str(batch["delivery_id"].as_str().unwrap()).unwrap();
    let through = batch["through"].as_u64().unwrap();
    assert!(
        b.acknowledge_notify(&f.serial, "ops", generation, delivery, through + 1)
            .is_err()
    );
    assert!(
        b.acknowledge_notify(&f.serial, "ops", generation, Uuid::new_v4(), through)
            .is_err()
    );
    assert_eq!(batch, b.notify_batch(&f.serial, "ops", generation).unwrap());
    // Actual SSE through Hyper's chunk decoder, not just the JSON poll route.
    let mut streamed = None;
    b.watch_notifications(&f.serial, "ops", generation, None, |v| {
        streamed = Some(v.clone());
        Ok(false)
    })
    .unwrap();
    assert_eq!(streamed, Some(batch.clone()));
    drop(first);
    assert_eq!(batch, b.notify_batch(&f.serial, "ops", generation).unwrap());
    let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
    assert_eq!(
        rt.block_on(reopened.notify_batch(&f.serial, f.route, "ops", generation))
            .unwrap(),
        batch
    );
    let ack = b
        .acknowledge_notify(&f.serial, "ops", generation, delivery, through)
        .unwrap();
    assert_eq!(
        ack,
        b.acknowledge_notify(&f.serial, "ops", generation, delivery, through)
            .unwrap()
    );
    assert_eq!(
        b.notify_consumer(&f.serial, "other").unwrap()["acknowledged"],
        other["acknowledged"]
    );
    assert!(!b.notify_batch(&f.serial, "other", other_gen).unwrap()["delivery_id"].is_null());
    // Timestamp-only monitor refreshes must not create state events.
    rt.block_on(f.catalog.record_library_monitor(
        &f.serial,
        f.route,
        100,
        &tape_rs::ltfs::label::ltfs_time_now(),
        None,
    ))
    .unwrap();
    let stable = b.notify_consumer(&f.serial, "ops").unwrap()["head"].clone();
    rt.block_on(f.catalog.record_library_monitor(
        &f.serial,
        f.route,
        100,
        &tape_rs::ltfs::label::ltfs_time_now(),
        None,
    ))
    .unwrap();
    assert_eq!(b.notify_consumer(&f.serial, "ops").unwrap()["head"], stable);
    // Multiple pages and an empty filtered batch still require explicit ACK.
    let paged = b
        .create_notify_consumer(
            &f.serial,
            "paged",
            json!({"event_types":["library.monitor.changed"]}),
            false,
        )
        .unwrap();
    let paged_generation = Uuid::parse_str(paged["generation"].as_str().unwrap()).unwrap();
    for n in 0..20 {
        let text = format!("isolated collector error {n}");
        rt.block_on(f.catalog.record_library_monitor(
            &f.serial,
            f.route,
            100,
            &tape_rs::ltfs::label::ltfs_time_now(),
            Some(&text),
        ))
        .unwrap();
    }
    let p = b
        .notify_batch(&f.serial, "paged", paged_generation)
        .unwrap();
    assert_eq!(p["events"].as_array().unwrap().len(), 16);
    let p2 = b
        .notify_batch(&f.serial, "paged", paged_generation)
        .unwrap();
    assert_eq!(p, p2);
    b.acknowledge_notify(
        &f.serial,
        "paged",
        paged_generation,
        Uuid::parse_str(p["delivery_id"].as_str().unwrap()).unwrap(),
        p["through"].as_u64().unwrap(),
    )
    .unwrap();
    let p = b
        .notify_batch(&f.serial, "paged", paged_generation)
        .unwrap();
    assert_eq!(p["events"].as_array().unwrap().len(), 4);
    let empty = b.notify_batch(&f.serial, "ops", generation).unwrap();
    assert!(empty["events"].as_array().unwrap().is_empty());
    assert!(!empty["delivery_id"].is_null());
    // Expanding the connection's event types must replay the unacknowledged
    // range unchanged; newly added types begin after the expansion boundary.
    let expanded = b
        .create_notify_consumer(
            &f.serial,
            "expanded",
            json!({"event_types":["tape.state.changed"]}),
            false,
        )
        .unwrap();
    let expanded_generation = Uuid::parse_str(expanded["generation"].as_str().unwrap()).unwrap();
    rt.block_on(f.catalog.record_library_monitor(
        &f.serial,
        f.route,
        100,
        &tape_rs::ltfs::label::ltfs_time_now(),
        Some("newly included monitor event"),
    ))
    .unwrap();
    let current = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let online=b.prepare_control(&f.serial,f.route.route.namespace,json!({"kind":"tape","pool":current.pool_id.to_string(),"barcode":BARCODES[0],"volume":current.volume_id.unwrap().to_string(),"revision":current.revision,"patch":{"blacklisted":true}})).unwrap();
    let saved = f.root.join("notify-online.json");
    online.save_new(&saved).unwrap();
    b.run_management_saved(&saved).unwrap();
    let narrow = b
        .notify_batch(&f.serial, "expanded", expanded_generation)
        .unwrap();
    assert!(!narrow["events"].as_array().unwrap().is_empty());
    let types = vec![
        "tape.state.changed".to_owned(),
        "library.monitor.changed".to_owned(),
    ];
    // Generic HTTP clients percent-encode the comma in a CSV query value.
    {
        use std::io::BufRead;
        let mut socket = std::net::TcpStream::connect(&second.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(socket,"GET /admin/notify/consumers/expanded/stream?library={}&generation={}&event_types=tape.state.changed%2Clibrary.monitor.changed HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",f.serial,expanded_generation,second.address).unwrap();
        let mut reader = std::io::BufReader::new(socket);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    }
    let mut wider = None;
    b.watch_notifications(
        &f.serial,
        "expanded",
        expanded_generation,
        Some(&types),
        |batch| {
            wider = Some(batch.clone());
            Ok(false)
        },
    )
    .unwrap();
    let wider = wider.unwrap();
    assert_eq!(narrow, wider); // Original pending delivery and ACK remain valid.
    let expanded_view = b.notify_consumer(&f.serial, "expanded").unwrap();
    assert_eq!(expanded_view["acknowledged"], expanded["acknowledged"]);
    let new_type_start = expanded_view["type_starts"]["library.monitor.changed"]
        .as_u64()
        .unwrap();
    assert!(new_type_start >= narrow["through"].as_u64().unwrap());
    assert!(
        wider["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["event_type"] != "library.monitor.changed")
    );
    rt.block_on(f.catalog.record_library_monitor(
        &f.serial,
        f.route,
        100,
        &tape_rs::ltfs::label::ltfs_time_now(),
        Some("fresh event after expansion"),
    ))
    .unwrap();
    assert_eq!(
        b.notify_batch(&f.serial, "expanded", expanded_generation)
            .unwrap(),
        narrow
    );
    b.acknowledge_notify(
        &f.serial,
        "expanded",
        expanded_generation,
        Uuid::parse_str(narrow["delivery_id"].as_str().unwrap()).unwrap(),
        narrow["through"].as_u64().unwrap(),
    )
    .unwrap();
    let fresh = b
        .notify_batch(&f.serial, "expanded", expanded_generation)
        .unwrap();
    assert_eq!(fresh["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        fresh["events"][0]["data"]["current"]["error"],
        "fresh event after expansion"
    );
    assert!(fresh["events"][0]["sequence"].as_u64().unwrap() > new_type_start);
    assert!(
        b.watch_notifications(
            &f.serial,
            "expanded",
            expanded_generation,
            Some(&["tape.state.changed".into()]),
            |_| Ok(false)
        )
        .is_err()
    );
    assert_eq!(
        b.notify_batch(&f.serial, "expanded", expanded_generation)
            .unwrap(),
        fresh
    );
    // Expanding to all preserves previous type boundaries and pending content.
    let all = b
        .create_notify_consumer(&f.serial, "expanded", json!({}), false)
        .unwrap();
    assert_eq!(all["generation"], expanded["generation"]);
    assert_eq!(
        all["type_starts"]["library.monitor.changed"],
        new_type_start
    );
    assert_eq!(
        b.notify_batch(&f.serial, "expanded", expanded_generation)
            .unwrap(),
        fresh
    );
    b.acknowledge_notify(
        &f.serial,
        "expanded",
        expanded_generation,
        Uuid::parse_str(fresh["delivery_id"].as_str().unwrap()).unwrap(),
        fresh["through"].as_u64().unwrap(),
    )
    .unwrap();
    let current = rt
        .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
        .unwrap()
        .unwrap();
    let offline=b.prepare_control(&f.serial,f.route.route.namespace,json!({"kind":"tape","pool":current.pool_id.to_string(),"barcode":BARCODES[0],"volume":current.volume_id.unwrap().to_string(),"revision":current.revision,"patch":{"online":false}})).unwrap();
    let saved = f.root.join("notify-offline-again.json");
    offline.save_new(&saved).unwrap();
    b.run_management_saved(&saved).unwrap();
    let all_batch = b
        .notify_batch(&f.serial, "expanded", expanded_generation)
        .unwrap();
    assert!(
        all_batch["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_type"] == "task.status.changed")
    );
    for event in all_batch["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == "task.status.changed")
    {
        assert!(
            event["sequence"].as_u64().unwrap()
                > all["type_starts"]["task.status.changed"].as_u64().unwrap()
        );
    }
    // New sessions fence old closes; liveness never mutates delivery progress.
    let old_session = (expanded_generation, Uuid::new_v4());
    let new_session = (expanded_generation, Uuid::new_v4());
    for token in [old_session, new_session] {
        assert!(
            rt.block_on(f.catalog.record_notify_connection(
                &f.serial,
                f.route,
                "expanded",
                token,
                NotifyConnectionAction::Open
            ))
            .unwrap()
        );
    }
    assert!(
        !rt.block_on(f.catalog.record_notify_connection(
            &f.serial,
            f.route,
            "expanded",
            old_session,
            NotifyConnectionAction::Closed
        ))
        .unwrap()
    );
    let connected = b.notify_consumer(&f.serial, "expanded").unwrap();
    assert_eq!(connected["connection_status"], "connected");
    assert_eq!(connected["delivery_status"], "waiting_ack");
    assert!(
        rt.block_on(f.catalog.record_notify_connection(
            &f.serial,
            f.route,
            "expanded",
            new_session,
            NotifyConnectionAction::Closed
        ))
        .unwrap()
    );
    let disconnected = b.notify_consumer(&f.serial, "expanded").unwrap();
    assert_eq!(disconnected["connection_status"], "disconnected");
    assert_eq!(disconnected["notification_status"], "interrupted");
    assert_eq!(
        b.notify_batch(&f.serial, "expanded", expanded_generation)
            .unwrap(),
        all_batch
    );
    // New owner must not erase events or consumer progress.
    drop(owner);
    assert!(matches!(
        rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 200))
            .unwrap(),
        ActiveOwnerOutcome::Advanced
    ));
    assert_eq!(
        b.notify_consumer(&f.serial, "ops").unwrap()["acknowledged"],
        through
    );
    b.delete_notify_consumer(&f.serial, "ops", generation)
        .unwrap();
    let fresh = b
        .create_notify_consumer(&f.serial, "ops", filter, true)
        .unwrap();
    let fresh_gen = Uuid::parse_str(fresh["generation"].as_str().unwrap()).unwrap();
    assert_ne!(fresh_gen, generation);
    assert!(
        b.acknowledge_notify(&f.serial, "ops", generation, delivery, through)
            .is_err()
    );
    assert!(
        b.delete_notify_consumer(&f.serial, "ops", generation)
            .is_err()
    );
    assert!(b.notify_batch(&f.serial, "ops", generation).is_err());
    assert!(
        !b.notify_batch(&f.serial, "ops", fresh_gen).unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        rt.block_on(f.catalog.create_notify_consumer(
            &f.serial,
            f.route,
            "bad",
            &NotifyFilter {
                pool: Some(Uuid::nil().to_string()),
                ..Default::default()
            },
            false
        ))
        .is_err()
    );
    drop(second);
    fs::remove_dir_all(&f.root).unwrap();
}
