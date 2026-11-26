use super::*;
use crate::client::fdb::FdbClient;
use crate::daemon::{fdb_http, http::HttpContext, permission::Phase};
use crate::managed_catalog::{ManagedCatalog, verify_native_barriers};
use crate::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary};
use serde_json::{Value, json};
use std::sync::{Mutex, atomic::AtomicBool, mpsc};
use uuid::Uuid;

#[path = "executor_library_pool_migration_tests.rs"]
mod pool_migration;

#[test]
fn reload_transport_error_cannot_authorize_registration_cleanup() {
    let sim = SimLibrary::new(1, 3, 0);
    let drive = sim.drive_as(0, 2);
    drive.inject(Fault {
        opcode: cdb::opcode::PERSISTENT_RESERVE_OUT,
        action: FaultAction::HostError(3),
        after: 1,
        remaining: 1,
    });
    let key = ReservationKey::new(2, 10);
    let error = reservation::reserve_for_reload(&drive, key).unwrap_err();
    assert!(matches!(error, TapeError::ScsiHost(3)));
    assert!(
        reservation::read_handoff_status(&drive)
            .unwrap()
            .keys
            .contains(&key.0)
    );
    assert!(!reload_can_clear(&error));
}

#[test]
fn health_preserves_reservation_conflict_for_executor_fencing() {
    let sim = SimLibrary::new(1, 3, 0);
    sim.enable_changer_pr();
    let changer = sim.changer_as(2);
    let drive = sim.drive_as(0, 2);
    let external = sim.drive_as(0, 3);
    let key = ReservationKey::new(2, 10);
    assert!(matches!(
        reservation::fence(&changer, key).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    assert!(matches!(
        reservation::fence(&drive, key).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    assert!(matches!(
        reservation::fence(&external, ReservationKey::new(3, 11)).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    let devices = vec![
        ManagedDevice {
            name: "changer".into(),
            serial: "SIMLIB".into(),
            kind: DeviceKind::Changer,
            dev: Box::new(changer),
        },
        ManagedDevice {
            name: "drive".into(),
            serial: "SIMDRV0000".into(),
            kind: DeviceKind::Drive,
            dev: Box::new(drive),
        },
    ];
    assert!(ownership_lost(&health(&devices).unwrap_err()));
}

fn wait_event(rx: &Receiver<ExecEvent>, expected: impl Fn(&ExecEvent) -> bool) -> ExecEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if expected(&event) {
            return event;
        }
    }
}

fn wait_phase(client: &FdbClient, serial: &str, ns: Uuid, id: Uuid, phase: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = client
            .library_handoff_status(serial, ns, id)
            .unwrap()
            .unwrap();
        if value["phase"] == phase {
            return value;
        }
        assert!(Instant::now() < deadline, "等待 {phase}，最后回执 {value}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；设备均为 SimLibrary"]
async fn logical_activation_requires_original_release_and_stable_inventory() {
    use crate::logical_library::{CreateLogicalLibrary, LogicalLibraryResources};
    use crate::managed_catalog::{
        ActivePoolCreation, LibraryHandoffPhase, LogicalLibraryCreation, PoolPolicy, PoolRecord,
    };
    struct Devices<'a>(&'a str, SimLibrary);
    impl DeviceProvider for Devices<'_> {
        fn open_all(&self) -> Result<Vec<ManagedDevice>> {
            Ok(vec![
                ManagedDevice {
                    name: "changer".into(),
                    serial: self.0.into(),
                    kind: DeviceKind::Changer,
                    dev: Box::new(self.1.changer_as(1)),
                },
                ManagedDevice {
                    name: "drive".into(),
                    serial: "SIMDRV0000".into(),
                    kind: DeviceKind::Drive,
                    dev: Box::new(self.1.drive_as(0, 1)),
                },
            ])
        }
    }

    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("ACTIVATE-{ns}");
    let challenge = Uuid::new_v4();
    let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,
        "challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2 {"Leader"} else {"Follower"},
        "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
        "applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
    let route = catalog
        .initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
        )
        .await
        .unwrap();
    catalog
        .enable_library_handoff_offline(&serial, ns)
        .await
        .unwrap();
    let epoch = route.initial_owner_epoch;
    let pool = PoolRecord {
        id: Uuid::new_v4(),
        library_serial: serial.clone(),
        name: "original".into(),
        file_limit: 100,
        created_at_millis: 1,
        revision: 1,
        policy: PoolPolicy::default(),
    };
    assert_eq!(
        catalog
            .create_admin_pool(&serial, route, epoch, None, &pool)
            .await
            .unwrap(),
        ActivePoolCreation::Created(pool.clone())
    );
    let sim = SimLibrary::new(1, 4, 1);
    sim.enable_changer_pr();
    sim.insert_cartridge(SimCartridge::blank("ORIGN0L8", 1 << 20), 0)
        .unwrap();
    let provider = Devices(&serial, sim);
    let devices = provider.open_all().unwrap();
    let saved_inventory = management::observe_devices(&devices, 2, epoch).unwrap();
    let storage_start = saved_inventory["slot_layout"]["storage_start"]
        .as_u64()
        .unwrap() as u16;
    catalog
        .record_active_library_inventory(&serial, route, epoch, &saved_inventory)
        .await
        .unwrap();
    drop(devices);
    let before_activation = catalog
        .create_logical_library(
            &serial,
            route,
            epoch,
            CreateLogicalLibrary {
                version: 1,
                id: Uuid::new_v4(),
                name: "premature".into(),
                resources: LogicalLibraryResources {
                    drives: vec![],
                    storage_slots: vec![storage_start + 1],
                    ie_slots: vec![],
                },
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(before_activation, LogicalLibraryCreation::Conflict(detail) if detail.contains("离线启用")),
        "{before_activation:?}"
    );
    let operation = Uuid::new_v4();
    let release = LibraryHandoff {
        library_serial: serial.clone(),
        namespace: ns,
        operation: Uuid::new_v4(),
        release: None,
        phase: LibraryHandoffPhase::Releasing,
        release_key: ReservationKey::new(2, epoch).0,
        release_retry_used: false,
        attempt_key: None,
        owner_epoch: epoch,
        devices: vec![serial.clone(), "SIMDRV0000".into()],
        detail: None,
    };
    let activate = |expected| {
        activate_logical_library_offline(
            &catalog,
            &serial,
            ns,
            operation,
            release.operation,
            expected,
            &provider,
        )
    };
    let blocked = activate(None).await.unwrap();
    assert_eq!(blocked["pools"][0]["id"], json!(pool.id));
    assert_eq!(blocked["inventory_provenance"], "persisted_observation");
    assert_eq!(blocked["resources"]["drives"], json!(["SIMDRV0000"]));
    assert!(!blocked["blocking_reasons"].as_array().unwrap().is_empty());
    assert!(blocked["plan_sha256"].is_null());
    catalog
        .begin_library_handoff(&release, route)
        .await
        .unwrap();
    let mut released = release.clone();
    released.phase = LibraryHandoffPhase::Released;
    catalog
        .update_library_handoff(route, &release, &released)
        .await
        .unwrap();
    let external = provider.1.drive_as(0, 3);
    let external_key = ReservationKey(0x1234);
    reservation::reserve_for_reload(&external, external_key).unwrap();
    let blocked = activate(None).await.unwrap();
    assert_eq!(blocked["pools"][0]["id"], json!(pool.id));
    assert!(blocked["plan_sha256"].is_null());
    reservation::clear_reload_registration(&external, external_key).unwrap();

    let preview = activate(None).await.unwrap();
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["pools"][0]["id"], json!(pool.id));
    assert_eq!(
        preview["logical_library"]["resources"]["drives"],
        json!(["SIMDRV0000"])
    );
    assert!(
        catalog
            .logical_library_activation_receipt(&serial, route, operation)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .require_logical_pool_scope(&serial, route, pool.id, Some(operation))
            .await
            .is_err()
    );
    let plan = preview["plan_sha256"].as_str().unwrap();
    provider
        .1
        .insert_cartridge(SimCartridge::blank("NEW000L8", 1 << 20), 2)
        .unwrap();
    assert!(activate(Some(plan)).await.is_err());
    assert!(
        catalog
            .logical_library_activation_receipt(&serial, route, operation)
            .await
            .unwrap()
            .is_none()
    );
    let preview = activate(None).await.unwrap();
    let plan = preview["plan_sha256"].as_str().unwrap();
    let enabled = activate(Some(plan)).await.unwrap();
    assert_eq!(enabled["status"], "enabled");
    assert_eq!(activate(Some(plan)).await.unwrap(), enabled);
    assert_eq!(
        catalog
            .logical_library_activation_receipt(&serial, route, operation)
            .await
            .unwrap(),
        Some(enabled)
    );
    assert!(
        catalog
            .require_logical_pool_scope(&serial, route, pool.id, None)
            .await
            .is_err()
    );
    catalog
        .require_logical_pool_scope(&serial, route, pool.id, Some(operation))
        .await
        .unwrap();
    assert_eq!(
        catalog
            .admin_pool(&serial, route, Some(operation), "original")
            .await
            .unwrap(),
        Some(pool.clone())
    );
    assert_eq!(
        catalog
            .library_handoff(&serial, route, Some(release.operation))
            .await
            .unwrap(),
        Some(released)
    );
    assert_eq!(
        catalog.active_library_route(&serial).await.unwrap(),
        Some(route)
    );
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；设备均为 SimLibrary"]
fn library_handoff_sdk_preserves_loaded_media_and_never_preempts_external_owner() {
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("HANDOFF-{ns}");
    let challenge = Uuid::new_v4();
    let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,"challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
    let route = rt
        .block_on(catalog.initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
        ))
        .unwrap();
    let round = route.initial_owner_epoch;
    rt.block_on(catalog.enable_library_handoff_offline(&serial, ns))
        .unwrap();
    let sim = SimLibrary::new(1, 3, 0);
    sim.enable_changer_pr();
    sim.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    sim.load_into_drive("TEST01L8", 0).unwrap();
    let changer = Arc::new(sim.changer_as(2));
    let drive = Arc::new(sim.drive_as(0, 2));
    let permit = Arc::new(Permission::default());
    let status = fdb_observations_fixture::make_status(2, round, route.frozen_index);
    let log = Arc::new(Mutex::new(Vec::new()));
    let provider = fdb_observations_fixture::Provider {
        serial: serial.clone(),
        changer: changer.clone(),
        drive: drive.clone(),
        log: log.clone(),
        revoke: Arc::new(AtomicBool::new(false)),
        permission: permit.clone(),
        round,
    };
    let root = std::env::temp_dir().join(format!("tapers-library-handoff-{ns}"));
    let files = FileService::new(root.join("spool")).unwrap();
    let (send, receive) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let owner = FdbOwnerControl {
        catalog: catalog.clone(),
        route,
        serial: serial.clone(),
        storage: Default::default(),
    };
    let executor_files = files.clone();
    let executor_status = status.clone();
    let worker = std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            run_fdb_writer(
                Box::new(provider),
                fdb_observations_fixture::options(2),
                receive,
                events,
                executor_files,
                executor_status,
                owner,
            )
        })
        .unwrap();
    send.send(ExecRequest::AttachPermission {
        permission: permit.clone(),
    })
    .unwrap();
    let grant = |round, phase| {
        assert!(permit.renew(round, phase, Instant::now() + Duration::from_secs(30)))
    };
    let take_over = |round| {
        grant(round, Phase::Fencing);
        let mut shared = status.lock().unwrap();
        shared.executor.as_mut().unwrap().round = round;
        shared.executor_ready = false;
        send.send(ExecRequest::Takeover { round }).unwrap();
    };
    let recover = |round| {
        grant(round, Phase::Execution);
        send.send(ExecRequest::Recover { round }).unwrap();
        let event = wait_event(&event_rx, |e| {
            matches!(e, ExecEvent::Serving { .. } | ExecEvent::Lost { .. })
        });
        assert!(
            matches!(event, ExecEvent::Serving {round: r,..} if r == round),
            "{event:?}"
        );
        status.lock().unwrap().executor_ready = true;
        grant(round, Phase::Serving);
    };
    take_over(round);
    assert!(matches!(
        wait_event(&event_rx, |e| matches!(
            e,
            ExecEvent::Fenced { .. } | ExecEvent::FenceFailed { .. }
        )),
        ExecEvent::Fenced { .. }
    ));
    recover(round);

    let context = Arc::new(HttpContext {
        files,
        status: status.clone(),
        exec: Mutex::new(send.clone()),
        node: Mutex::new(mpsc::channel().0),
        client_addrs: Default::default(),
        wait_timeout: Duration::from_secs(10),
    });
    let router = fdb_http::writer_router(
        catalog.clone(),
        [(serial.clone(), route)].into(),
        [(serial.clone(), context)].into(),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = rt.spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = FdbClient::new([addr])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(10));

    changer.inject(Fault {
        opcode: cdb::opcode::TEST_UNIT_READY,
        action: FaultAction::CheckCondition {
            sense_key: 4,
            asc: 0x44,
            ascq: 0,
        },
        after: 0,
        remaining: 1,
    });
    let failed = client.release_library(&serial, ns, Uuid::new_v4()).unwrap();
    assert_eq!(failed["phase"], "failed");
    reservation::verify_holder(changer.as_ref(), ReservationKey::new(2, round)).unwrap();
    reservation::verify_holder(drive.as_ref(), ReservationKey::new(2, round)).unwrap();

    let release = Uuid::new_v4();
    let since = log.lock().unwrap().len();
    // Fail the first changer PR OUT. The drive can still release, and the
    // original initiating node must settle the remaining changer registration.
    changer.inject(Fault {
        opcode: cdb::opcode::PERSISTENT_RESERVE_OUT,
        action: FaultAction::HostError(3),
        after: 0,
        remaining: 1,
    });
    assert_eq!(
        client.release_library(&serial, ns, release).unwrap()["phase"],
        "releasing"
    );
    wait_event(
        &event_rx,
        |e| matches!(e, ExecEvent::Lost { round: r, .. } if *r == round),
    );
    let partial = client
        .library_handoff_status(&serial, ns, release)
        .unwrap()
        .unwrap();
    assert_eq!(partial["phase"], "releasing");
    assert!(partial["detail"].as_str().unwrap().contains("未知"));
    let original = rt
        .block_on(catalog.library_handoff(&serial, route, Some(release)))
        .unwrap()
        .unwrap();
    let mut unknown = original.clone();
    unknown.release_retry_used = true;
    rt.block_on(catalog.update_library_handoff(route, &original, &unknown))
        .unwrap();
    let repeated = client.release_library(&serial, ns, release).unwrap();
    assert_eq!(repeated["release_retry_used"], true);
    assert_eq!(repeated["detail"], unknown.detail.as_deref().unwrap());
    rt.block_on(catalog.update_library_handoff(route, &unknown, &original))
        .unwrap();
    let mut in_flight = original.clone();
    in_flight.release_retry_used = true;
    in_flight.detail = None;
    rt.block_on(catalog.update_library_handoff(route, &original, &in_flight))
        .unwrap();
    // Another node can observe no registrations while the original command is
    // still in flight. That observation alone must not publish a receipt.
    let empty_sim = SimLibrary::new(1, 3, 0);
    empty_sim.enable_changer_pr();
    let observer = fdb_observations_fixture::Provider {
        serial: serial.clone(),
        changer: Arc::new(empty_sim.changer_as(3)),
        drive: Arc::new(empty_sim.drive_as(0, 3)),
        log: Arc::new(Mutex::new(Vec::new())),
        revoke: Arc::new(AtomicBool::new(false)),
        permission: Arc::new(Permission::default()),
        round,
    };
    let observer_control = FdbOwnerControl {
        catalog: catalog.clone(),
        route,
        serial: serial.clone(),
        storage: Default::default(),
    };
    let (observer_send, observer_receive) = mpsc::channel();
    observer_send
        .send(ExecRequest::Shutdown { done: None })
        .unwrap();
    let observer_files = FileService::new(root.join("observer-spool")).unwrap();
    let observer_status = fdb_observations_fixture::make_status(3, round, route.frozen_index);
    let control = FdbOwnerControl {
        catalog: catalog.clone(),
        route,
        serial: serial.clone(),
        storage: Default::default(),
    };
    std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            run_fdb_writer(
                Box::new(observer),
                fdb_observations_fixture::options(3),
                observer_receive,
                mpsc::channel().0,
                observer_files,
                observer_status,
                control,
            )
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(
        rt.block_on(catalog.library_handoff(&serial, route, Some(release)))
            .unwrap(),
        Some(in_flight.clone()),
        "其他节点不能仅凭空预留确认正在执行的交还"
    );
    let replay = client.release_library(&serial, ns, release).unwrap();
    assert_eq!(replay["release_retry_used"], true);
    assert!(replay["detail"].is_null());
    assert_eq!(
        rt.block_on(catalog.library_handoff(&serial, route, Some(release)))
            .unwrap(),
        Some(in_flight.clone())
    );
    rt.block_on(catalog.update_library_handoff(route, &in_flight, &original))
        .unwrap();
    assert!(
        reservation::read_status(drive.as_ref())
            .unwrap()
            .holder
            .is_none()
    );
    assert!(
        reservation::read_status(changer.as_ref())
            .unwrap()
            .holder
            .is_some()
    );
    take_over(round + 1);
    wait_event(
        &event_rx,
        |e| matches!(e, ExecEvent::FenceFailed {round: r,..} if *r == round + 1),
    );
    assert_eq!(
        client
            .library_handoff_status(&serial, ns, release)
            .unwrap()
            .unwrap()["phase"],
        "releasing",
        "结果未知时不能自动重发交还命令"
    );
    // Continue the reload scenarios with an explicitly completed simulated
    // operation. This fixture is not a production recovery mechanism.
    let fixture_provider = fdb_observations_fixture::Provider {
        serial: serial.clone(),
        changer: changer.clone(),
        drive: drive.clone(),
        log: log.clone(),
        revoke: Arc::new(AtomicBool::new(false)),
        permission: permit.clone(),
        round,
    };
    let before_verification = log.lock().unwrap().len();
    assert!(
        rt.block_on(verify_library_release_offline(
            &catalog,
            &serial,
            ns,
            release,
            &fixture_provider
        ))
        .is_err()
    );
    assert!(
        log.lock().unwrap()[before_verification..]
            .iter()
            .all(|&op| op == cdb::opcode::PERSISTENT_RESERVE_IN)
    );
    let fixture_devices = fixture_provider.open_all().unwrap();
    let result = resume_release(&fixture_devices, &original);
    assert!(result.is_ok());
    drop(fixture_devices);
    let command_count = log.lock().unwrap().len();
    let mut completed_intent = original.clone();
    completed_intent.detail = None;
    rt.block_on(catalog.update_library_handoff(route, &original, &completed_intent))
        .unwrap();
    let mut receipt = ReleaseReceipt::new(completed_intent, result);
    let restore_spool = root.join("restore-spool");
    let restore_files = FileService::new(restore_spool.clone()).unwrap();
    let checkpoint = ReleaseReceipt::checkpoint_path(&restore_files, &observer_control, 2);
    receipt.persist(&checkpoint, &observer_control).unwrap();
    // A separate, unreachable FDB cluster forces publication to fail after
    // the devices have returned success. Retry only the original receipt.
    let unresponsive = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cluster_file = root.join("unresponsive.cluster");
    std::fs::write(
        &cluster_file,
        format!("test:test@{}\n", unresponsive.local_addr().unwrap()),
    )
    .unwrap();
    let outage = FdbOwnerControl {
        catalog: Arc::new(ManagedCatalog::open(cluster_file.to_str()).unwrap()),
        route,
        serial: serial.clone(),
        storage: Default::default(),
    };
    assert!(!rt.block_on(receipt.poll(&outage, &checkpoint)));
    assert_eq!(
        client
            .library_handoff_status(&serial, ns, release)
            .unwrap()
            .unwrap()["phase"],
        "releasing"
    );
    assert!(ReleaseReceipt::restore(&checkpoint, &observer_control, 3).is_err());
    assert!(
        ReleaseReceipt::restore(&checkpoint, &observer_control, 2)
            .unwrap()
            .is_some()
    );
    drop(receipt);
    drop(restore_files);
    let restore_files = FileService::new(restore_spool).unwrap();
    let (restart_send, restart_receive) = mpsc::channel();
    restart_send
        .send(ExecRequest::Shutdown { done: None })
        .unwrap();
    let restart_status = fdb_observations_fixture::make_status(2, round, route.frozen_index);
    std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            run_fdb_writer(
                Box::new(fixture_provider),
                fdb_observations_fixture::options(2),
                restart_receive,
                mpsc::channel().0,
                restore_files,
                restart_status,
                observer_control,
            )
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(!checkpoint.exists());
    assert_eq!(log.lock().unwrap().len(), command_count);
    let released = wait_phase(&client, &serial, ns, release, "released");
    assert_eq!(
        client.release_library(&serial, ns, release).unwrap(),
        released
    );
    assert_eq!(sim.loaded_barcode(0).as_deref(), Some("TEST01L8"));
    assert!(
        log.lock().unwrap()[since..]
            .iter()
            .all(|op| !matches!(*op, cdb::opcode::LOAD_UNLOAD | cdb::opcode::MOVE_MEDIUM))
    );

    let external = sim.changer_as(17);
    let foreign = ReservationKey(0x2300_0000_0000_0001);
    assert!(matches!(
        reservation::fence(&external, foreign).unwrap(),
        FenceOutcome::Fenced { .. }
    ));
    take_over(round + 2);
    wait_event(
        &event_rx,
        |e| matches!(e, ExecEvent::FenceFailed {round: r,..} if *r == round + 2),
    );
    reservation::verify_holder(&external, foreign).unwrap();
    let operation = Uuid::new_v4();
    assert_eq!(
        client
            .acquire_library(&serial, ns, operation, release)
            .unwrap()["phase"],
        "loading"
    );
    take_over(round + 3);
    wait_event(
        &event_rx,
        |e| matches!(e, ExecEvent::FenceFailed {round: r,..} if *r == round + 3),
    );
    reservation::verify_holder(&external, foreign).unwrap();
    wait_phase(&client, &serial, ns, operation, "blocked");
    reservation::release_and_unregister(&external, foreign).unwrap();
    assert_eq!(
        client
            .acquire_library(&serial, ns, operation, release)
            .unwrap()["phase"],
        "loading"
    );
    take_over(round + 4);
    assert!(matches!(
        wait_event(
            &event_rx,
            |e| matches!(e, ExecEvent::Fenced {round: r,..} | ExecEvent::FenceFailed {round: r,..} if *r == round + 4)
        ),
        ExecEvent::Fenced { .. }
    ));
    recover(round + 4);
    wait_phase(&client, &serial, ns, operation, "online");
    assert_eq!(
        rt.block_on(catalog.active_library_inventory(&serial, route))
            .unwrap()
            .unwrap()["round"],
        round + 4
    );
    assert_eq!(sim.loaded_barcode(0).as_deref(), Some("TEST01L8"));
    assert!(
        client
            .acquire_library(&serial, ns, Uuid::new_v4(), release)
            .is_err()
    );

    server.abort();
    send.send(ExecRequest::Stop {
        round: None,
        reason: "测试结束".into(),
    })
    .unwrap();
    send.send(ExecRequest::Shutdown { done: None }).unwrap();
    worker.join().unwrap();
    let queued = Uuid::new_v4();
    let request = crate::managed_catalog::ManagementRequest::parse(json!({
        "version": 1, "library": serial, "namespace": ns, "epoch": 1,
        "operation": queued, "change": {"kind": "library-rescan"}
    }))
    .unwrap();
    rt.block_on(catalog.admit_management(&serial, route, round + 4, &request))
        .unwrap();
    let original = LibraryHandoff {
        library_serial: serial.clone(),
        namespace: ns,
        operation: Uuid::new_v4(),
        release: None,
        phase: super::Phase::Releasing,
        release_key: ReservationKey::new(2, round + 4).0,
        attempt_key: None,
        release_retry_used: false,
        owner_epoch: round + 4,
        devices: vec![serial.clone(), "SIMDRV0000".into()],
        detail: None,
    };
    let result = rt
        .block_on(catalog.begin_library_handoff(&original, route))
        .unwrap_err();
    assert!(matches!(result.domain(), CatalogError::LibraryHandoffBusy(ids) if ids == &[queued]));
    assert!(
        rt.block_on(catalog.library_handoff(&serial, route, Some(original.operation)))
            .unwrap()
            .is_none()
    );
    std::fs::remove_dir_all(root).unwrap();
}
