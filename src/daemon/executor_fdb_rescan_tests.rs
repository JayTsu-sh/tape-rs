use super::super::fdb_observations_fixture::{Provider, make_status, options};
use super::*;
use crate::managed_catalog::{ManagedCatalog, verify_native_barriers};
use crate::scsi::{
    cdb::opcode,
    sim::{Fault, FaultAction, SimCartridge, SimLibrary},
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

fn saved(
    runtime: &tokio::runtime::Runtime,
    control: &FdbOwnerControl,
    request: &ManagementRequest,
) -> Value {
    runtime
        .block_on(control.catalog.management_task(
            &control.serial,
            control.route,
            request.epoch(),
            request.operation(),
        ))
        .unwrap()
        .unwrap()
}
fn enqueue(
    runtime: &tokio::runtime::Runtime,
    control: &FdbOwnerControl,
    round: u64,
) -> ManagementRequest {
    let w = runtime
        .block_on(
            control
                .catalog
                .active_management_window(&control.serial, control.route),
        )
        .unwrap();
    let r=ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":w.accepting,"operation":Uuid::new_v4().to_string(),"change":{"kind":"library-rescan"}})).unwrap();
    runtime
        .block_on(
            control
                .catalog
                .admit_management(&control.serial, control.route, round, &r),
        )
        .unwrap();
    r
}
fn queued(request: &ManagementRequest) -> Task {
    Task {
        request: request.clone(),
        running: false,
        completion: None,
    }
}
#[test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；仅模拟设备，验证rescan时序/回执未知/采样错误/取消/接管"]
fn rescan_keeps_original_intent_sample_and_receipt_across_dependency_failures() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("RESCAN-{ns}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let route = runtime
        .block_on(catalog.initialize_native_library(
            ns,
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
    let library = SimLibrary::new(1, 3, 0);
    library.enable_changer_pr();
    library
        .insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    let root = std::env::temp_dir().join(format!("tapers-native-rescan-{ns}"));
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
        super::super::super::permission::Phase::Fencing,
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
    permission.renew(
        round,
        super::super::super::permission::Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    let status = make_status(2, round, route.frozen_index);
    let context = || Context {
        active: &active,
        permission: &permission,
        control: &control,
        status: &status,
        node: 2,
    };
    let inventory = || {
        runtime
            .block_on(catalog.active_library_inventory(&serial, route))
            .unwrap()
            .unwrap()
    };
    let monitor = || {
        runtime
            .block_on(catalog.active_library_monitor(&serial, route))
            .unwrap()
            .unwrap()
    };
    let mut observer = Observer::default();
    observer.poll(context(), &runtime).unwrap();
    log.lock().unwrap().clear();
    let request = enqueue(&runtime, &control, round);
    observer.next = Instant::now();
    observer.poll(context(), &runtime).unwrap();
    let terminal = saved(&runtime, &control, &request);
    assert_eq!(terminal["status"], "succeeded");
    assert_eq!(terminal["request"], *request.value());
    assert_eq!(
        terminal["result"]["captured_at"],
        inventory()["observed_at"]
    );
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|op| **op == opcode::READ_ELEMENT_STATUS)
            .count(),
        1
    );
    // A terminal/cancelled original receipt must not trigger a device command.
    log.lock().unwrap().clear();
    observer.task = Some(queued(&request));
    observer.next = Instant::now();
    observer.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    let cancel = enqueue(&runtime, &control, round);
    runtime
        .block_on(catalog.cancel_management(
            &serial,
            route,
            round,
            cancel.epoch(),
            cancel.operation(),
        ))
        .unwrap();
    observer.task = Some(queued(&cancel));
    observer.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    // A device failure is a failed task only after its original monitor is durable.
    let before = inventory();
    let last_success = monitor()["last_success"].clone();
    let failed = enqueue(&runtime, &control, round);
    provider.changer.inject(Fault {
        opcode: opcode::READ_ELEMENT_STATUS,
        action: FaultAction::HostError(7),
        after: 0,
        remaining: 1,
    });
    observer.task = Some(queued(&failed));
    observer.poll(context(), &runtime).unwrap();
    assert_eq!(saved(&runtime, &control, &failed)["status"], "failed");
    assert_eq!(inventory(), before);
    assert_eq!(monitor()["last_success"], last_success);
    assert!(monitor()["error"].as_str().unwrap().contains("host"));
    // The running transaction committed but its ACK was lost. Retry the same
    // request, then capture once; no new identity or replacement task is created.
    let unknown = enqueue(&runtime, &control, round);
    runtime
        .block_on(catalog.begin_library_rescan(&serial, route, round, &unknown))
        .unwrap();
    assert!(
        runtime
            .block_on(catalog.cancel_management(
                &serial,
                route,
                round,
                unknown.epoch(),
                unknown.operation()
            ))
            .is_err()
    );
    observer.task = Some(queued(&unknown));
    observer.next = Instant::now();
    observer.poll(context(), &runtime).unwrap();
    assert_eq!(saved(&runtime, &control, &unknown)["status"], "succeeded");
    // Inject a real metadata timeout after a confirmed running receipt. Keep the
    // captured device payload until the original publication can be acknowledged.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = root.join("unreachable.cluster");
    std::fs::write(
        &unreachable,
        format!(
            "rescan:{}@{}",
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
    let before_begin = enqueue(&runtime, &control, round);
    observer.task = Some(queued(&before_begin));
    observer.next = Instant::now();
    log.lock().unwrap().clear();
    observer
        .poll(
            Context {
                control: &unavailable,
                ..context()
            },
            &runtime,
        )
        .unwrap();
    assert!(log.lock().unwrap().is_empty(), "running未确认时不发CDB");
    assert_eq!(saved(&runtime, &control, &before_begin)["status"], "queued");
    observer.next = Instant::now();
    observer.poll(context(), &runtime).unwrap();
    assert_eq!(
        saved(&runtime, &control, &before_begin)["status"],
        "succeeded"
    );
    let pending = enqueue(&runtime, &control, round);
    runtime
        .block_on(catalog.begin_library_rescan(&serial, route, round, &pending))
        .unwrap();
    observer.task = Some(Task {
        request: pending.clone(),
        running: true,
        completion: None,
    });
    observer.publisher.request_fresh();
    observer.next = Instant::now();
    log.lock().unwrap().clear();
    observer
        .poll(
            Context {
                control: &unavailable,
                ..context()
            },
            &runtime,
        )
        .unwrap();
    assert_eq!(saved(&runtime, &control, &pending)["status"], "running");
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|op| **op == opcode::READ_ELEMENT_STATUS)
            .count(),
        1
    );
    log.lock().unwrap().clear();
    std::thread::sleep(Duration::from_millis(1400));
    observer.poll(context(), &runtime).unwrap();
    assert!(
        log.lock().unwrap().is_empty(),
        "unknown publication retries the sample without another CDB"
    );
    let done = saved(&runtime, &control, &pending);
    assert_eq!(done["status"], "succeeded");
    // Drop a genuinely committed terminal ACK, then force the periodic deadline.
    // A pending terminal report must pin the old sample through an FDB timeout.
    observer.task = Some(Task {
        request: pending.clone(),
        running: true,
        completion: Some(Completion {
            captured_at: done["result"]["captured_at"].as_str().unwrap().into(),
            error: None,
        }),
    });
    observer.publisher.request_fresh();
    observer.next = Instant::now();
    observer
        .poll(
            Context {
                control: &unavailable,
                ..context()
            },
            &runtime,
        )
        .unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(inventory()["observed_at"], done["result"]["captured_at"]);
    observer.next = Instant::now();
    observer.poll(context(), &runtime).unwrap();
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(saved(&runtime, &control, &pending), done);
    // Loss of the actual transport permit during capture leaves the original
    // task running, without publishing its sample or pretending cancellation.
    let handover = enqueue(&runtime, &control, round);
    observer.task = Some(queued(&handover));
    observer.next = Instant::now();
    let before = inventory();
    revoke.store(true, Ordering::SeqCst);
    assert!(observer.poll(context(), &runtime).is_err());
    assert_eq!(inventory(), before);
    assert_eq!(saved(&runtime, &control, &handover)["status"], "running");
    let next = round + 1;
    let permit2 = Arc::new(Permission::default());
    let log2 = Arc::new(Mutex::new(Vec::new()));
    let provider2 = Provider {
        serial: serial.clone(),
        changer: Arc::new(library.changer_as(3)),
        drive: Arc::new(library.drive_as(0, 3)),
        log: log2.clone(),
        revoke: Arc::new(AtomicBool::new(false)),
        permission: permit2.clone(),
        round: next,
    };
    permit2.renew(
        next,
        super::super::super::permission::Phase::Fencing,
        Instant::now() + Duration::from_secs(120),
    );
    let mut active2 =
        takeover(&provider2, &options(3), next, Some(&permit2), files.clone()).unwrap();
    active2.serving = true;
    runtime
        .block_on(catalog.claim_active_owner_epoch(&serial, next))
        .unwrap();
    permit2.renew(
        next,
        super::super::super::permission::Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    let status2 = make_status(3, next, route.frozen_index);
    let context2 = Context {
        active: &active2,
        permission: &permit2,
        control: &control,
        status: &status2,
        node: 3,
    };
    assert!(
        runtime
            .block_on(catalog.complete_library_rescan(
                &serial,
                route,
                round,
                &handover,
                before["observed_at"].as_str().unwrap(),
                None
            ))
            .is_err()
    );
    observer.poll(context2, &runtime).unwrap();
    let adopted = saved(&runtime, &control, &handover);
    assert_eq!(adopted["status"], "succeeded");
    assert_eq!(adopted["owner_epoch"], next);
    assert_eq!(adopted["request"], *handover.value());
    assert_eq!(adopted["result"]["node"], 3);
    assert_eq!(adopted["result"]["round"], next);
    drop(active2);
    drop(active);
    drop(files);
    std::fs::remove_dir_all(root).unwrap();
}
