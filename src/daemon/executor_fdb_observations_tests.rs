use super::super::fdb_observations_fixture::{Provider, make_status, options};
use super::*;
use crate::managed_catalog::{ManagedCatalog, NotifyFilter, verify_native_barriers};
use crate::scsi::{
    cdb::opcode,
    sim::{Fault, FaultAction, SimCartridge, SimLibrary},
};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[test]
fn diagnostics_stay_valid_utf8_and_keep_the_storage_bound() {
    let text = "故障".repeat(2048);
    let bounded = bounded_error(&text);
    assert!(bounded.len() <= 2048 && bounded.ends_with('…'));
    assert_eq!(bounded_error("sense 70/05/24/00"), "sense 70/05/24/00");
}
#[test]
fn invalid_facts_are_permanent_and_owner_fences_stop_publication() {
    assert!(matches!(
        catalog_result::<()>(Err(CatalogError::Corrupt("bad cache"))),
        Publication::Failed {
            permanent: true,
            ..
        }
    ));
    assert!(matches!(
        catalog_result::<()>(Err(CatalogError::OwnerFenced)),
        Publication::Lost(_)
    ));
    for n in [0, 1, 2, 100, u32::MAX] {
        let d = retry_delay(n).as_millis();
        let base = (500u128 << n.min(6)).min(30_000);
        assert!((base..=base + 255).contains(&d));
    }
}

fn due(publisher: &mut Publisher) {
    publisher.next = Instant::now();
}
fn head(runtime: &tokio::runtime::Runtime, control: &FdbOwnerControl) -> u64 {
    runtime
        .block_on(
            control
                .catalog
                .notify_consumer(&control.serial, control.route, "observer"),
        )
        .unwrap()["head"]
        .as_u64()
        .unwrap()
}
fn inventory(runtime: &tokio::runtime::Runtime, control: &FdbOwnerControl) -> Value {
    runtime
        .block_on(
            control
                .catalog
                .active_library_inventory(&control.serial, control.route),
        )
        .unwrap()
        .unwrap()
}
fn monitor(runtime: &tokio::runtime::Runtime, control: &FdbOwnerControl) -> Value {
    runtime
        .block_on(
            control
                .catalog
                .active_library_monitor(&control.serial, control.route),
        )
        .unwrap()
        .unwrap()
}

#[test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；只使用模拟设备，验证采样失败/原观测重试/owner栅栏"]
fn publisher_preserves_complete_samples_and_owner_boundaries() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let namespace = Uuid::new_v4();
    let serial = format!("POLL-{namespace}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let route = runtime
        .block_on(catalog.initialize_native_library(
            namespace,
            &verify_native_barriers(&serial, challenge, &replies).unwrap(),
        ))
        .unwrap();
    let round = route.initial_owner_epoch;
    let control = FdbOwnerControl {
        catalog: catalog.clone(),
        serial: serial.clone(),
        route,
        storage: Default::default(),
    };
    runtime
        .block_on(catalog.create_notify_consumer(
            &serial,
            route,
            "observer",
            &NotifyFilter::default(),
            false,
        ))
        .unwrap();
    let library = SimLibrary::new(1, 3, 0);
    library.enable_changer_pr();
    library
        .insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    library
        .insert_cartridge(SimCartridge::blank("TEST02L8", 1 << 20), 1)
        .unwrap();
    let root = std::env::temp_dir().join(format!("tapers-observation-publisher-{namespace}"));
    let files = FileService::new(root.join("spool")).unwrap();
    let permission = Arc::new(Permission::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    let revoke = Arc::new(AtomicBool::new(false));
    let provider = Provider {
        serial: serial.clone(),
        changer: Arc::new(library.changer_as(2)),
        drive: Arc::new(library.drive_as(0, 2)),
        log: log.clone(),
        revoke: revoke.clone(),
        permission: permission.clone(),
        round,
    };
    permission.renew(
        round,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120),
    );
    let mut active = takeover(
        &provider,
        &options(2),
        round,
        Some(&permission),
        files.clone(),
    )
    .unwrap();
    active.serving = true;
    log.lock().unwrap().clear();
    let status = make_status(2, round, route.frozen_index);
    let context = || Context {
        active: &active,
        permission: &permission,
        control: &control,
        status: &status,
        node: 2,
    };
    let mut publisher = Publisher::default();
    // A locally serving summary cannot replace the actual Serving permit.
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert!(
        runtime
            .block_on(catalog.active_library_inventory(&serial, route))
            .unwrap()
            .is_none()
    );
    permission.renew(
        round,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    status.lock().unwrap().shutting_down = true;
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    status.lock().unwrap().shutting_down = false;
    publisher.poll(context(), &runtime).unwrap();
    let first = inventory(&runtime, &control);
    assert_eq!(first["volumes"].as_array().unwrap().len(), 2);
    assert_eq!(first["drives"].as_array().unwrap().len(), 1);
    assert_eq!(head(&runtime, &control), 4);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|op| **op == opcode::READ_ELEMENT_STATUS)
            .count(),
        1
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|op| matches!(*op, 0x12 | 0x5a | 0xb8 | 0x5e))
    );
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    let stable = inventory(&runtime, &control);
    assert_eq!(head(&runtime, &control), 4);
    let last_success = monitor(&runtime, &control)["last_success"].clone();
    provider.changer.inject(Fault {
        opcode: opcode::READ_ELEMENT_STATUS,
        action: FaultAction::HostError(7),
        after: 0,
        remaining: 1,
    });
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert_eq!(inventory(&runtime, &control), stable);
    let failed = monitor(&runtime, &control);
    assert_eq!(failed["last_success"], last_success);
    assert!(failed["error"].as_str().unwrap().contains("host"));
    assert_eq!(head(&runtime, &control), 5);
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(monitor(&runtime, &control)["error"].is_null());
    assert_eq!(head(&runtime, &control), 6);
    // A nonresponsive, separate cluster injects a real client/RPC timeout without
    // stopping the shared private FDB. The same captured payload must be retried.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = root.join("unreachable.cluster");
    std::fs::write(
        &unreachable,
        format!(
            "observer:{}@{}",
            Uuid::new_v4().simple(),
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    let unavailable = FdbOwnerControl {
        catalog: Arc::new(ManagedCatalog::open(Some(unreachable.to_str().unwrap())).unwrap()),
        serial: serial.clone(),
        route,
        storage: Default::default(),
    };
    due(&mut publisher);
    publisher
        .poll(
            Context {
                control: &unavailable,
                ..context()
            },
            &runtime,
        )
        .unwrap();
    assert!(!publisher.blocked);
    let pending = publisher
        .pending
        .as_ref()
        .unwrap()
        .inventory
        .as_ref()
        .unwrap()
        .clone();
    assert!(!publisher.pending.as_ref().unwrap().inventory_confirmed);
    let changer = active
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .unwrap();
    let mut mc = MediumChanger::new(changer.dev.as_ref());
    let map = mc.load_address_map().unwrap().clone();
    mc.move_medium(map.storage_start, map.dt_start).unwrap();
    log.lock().unwrap().clear();
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(inventory(&runtime, &control), pending);
    assert_eq!(
        monitor(&runtime, &control)["last_attempt"],
        pending["observed_at"],
        "重试旧采样不能刷新巡检时间"
    );
    assert!(publisher.pending.is_none());
    assert_eq!(head(&runtime, &control), 6);
    // The SDK transaction really commits; the caller then loses that ACK and
    // resumes the original monitor stage. Neither timestamps nor events rewind.
    let captured_at = pending["observed_at"].as_str().unwrap().to_owned();
    publisher.pending = Some(Attempt {
        captured_at: captured_at.clone(),
        inventory: Some(pending.clone()),
        inventory_confirmed: true,
        error: None,
    });
    assert!(matches!(
        publish(
            &context(),
            &runtime,
            catalog.record_library_monitor(&serial, route, round, &captured_at, None)
        ),
        Publication::Confirmed(())
    ));
    let confirmed_monitor = monitor(&runtime, &control);
    assert!(
        !publisher
            .accept(
                &context(),
                "monitor",
                Publication::Failed {
                    detail: "测试丢弃真实成功回执".into(),
                    permanent: false
                }
            )
            .unwrap()
    );
    std::thread::sleep(Duration::from_millis(20));
    log.lock().unwrap().clear();
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(monitor(&runtime, &control), confirmed_monitor);
    assert_eq!(head(&runtime, &control), 6);
    assert!(
        runtime
            .block_on(catalog.record_library_monitor(&serial, route, round, "not-a-time", None))
            .is_err()
    );
    assert_eq!(monitor(&runtime, &control), confirmed_monitor);
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert_eq!(
        inventory(&runtime, &control)["drives"][0]["barcode"],
        "TEST01L8"
    );
    assert_eq!(head(&runtime, &control), 9);
    // Corrupt only this test's existing cache, prove no automatic repair, restore
    // its exact bytes, and require a new owner before a blocked publisher resumes.
    let db = foundationdb::Database::new(Some(&cluster)).unwrap();
    let mut key = b"tape-rs/catalog/22/O/".to_vec();
    key.extend_from_slice(serial.as_bytes());
    key.extend_from_slice(b"\0\0/inventory/");
    key.extend_from_slice(namespace.as_bytes());
    let saved = runtime.block_on(async {
        let tr = db.create_trx().unwrap();
        let saved = tr.get(&key, false).await.unwrap().unwrap().to_vec();
        tr.set(&key, b"invalid-cache");
        tr.commit().await.unwrap();
        saved
    });
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(publisher.blocked);
    runtime.block_on(async {
        let tr = db.create_trx().unwrap();
        assert_eq!(
            tr.get(&key, false).await.unwrap().unwrap().as_ref(),
            b"invalid-cache"
        );
        tr.set(&key, &saved);
        tr.commit().await.unwrap();
    });
    log.lock().unwrap().clear();
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    let next = round + 1;
    let permission2 = Arc::new(Permission::default());
    let log2 = Arc::new(Mutex::new(Vec::new()));
    let revoke2 = Arc::new(AtomicBool::new(false));
    let provider2 = Provider {
        serial: serial.clone(),
        changer: Arc::new(library.changer_as(3)),
        drive: Arc::new(library.drive_as(0, 3)),
        log: log2.clone(),
        revoke: revoke2.clone(),
        permission: permission2.clone(),
        round: next,
    };
    permission.close(round);
    permission2.renew(
        next,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120),
    );
    let mut active2 = takeover(
        &provider2,
        &options(3),
        next,
        Some(&permission2),
        files.clone(),
    )
    .unwrap();
    active2.serving = true;
    permission2.renew(
        next,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120),
    );
    runtime
        .block_on(catalog.claim_active_owner_epoch(&serial, next))
        .unwrap();
    permission2.renew(
        next,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    let status2 = make_status(3, next, route.frozen_index);
    let context2 = || Context {
        active: &active2,
        permission: &permission2,
        control: &control,
        status: &status2,
        node: 3,
    };
    publisher.poll(context2(), &runtime).unwrap();
    assert!(!publisher.blocked);
    assert_eq!(inventory(&runtime, &control)["round"], next);
    assert_eq!(inventory(&runtime, &control)["owner"], 3);
    assert_eq!(head(&runtime, &control), 9);
    log.lock().unwrap().clear();
    due(&mut publisher);
    publisher.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(publisher.round, next);
    let before_loss = inventory(&runtime, &control);
    let monitor_before = monitor(&runtime, &control);
    revoke2.store(true, Ordering::SeqCst);
    due(&mut publisher);
    assert!(publisher.poll(context2(), &runtime).is_err());
    assert_eq!(inventory(&runtime, &control), before_loss);
    assert_eq!(monitor(&runtime, &control), monitor_before);
    assert_eq!(head(&runtime, &control), 9);
    drop(active2);
    drop(active);
    drop(files);
    std::fs::remove_dir_all(root).unwrap();
}
