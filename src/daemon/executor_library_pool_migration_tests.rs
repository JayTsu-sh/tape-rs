//! Public SDK and owner round trip over a private FDB and simulated tape.
use super::*;
use crate::client::{
    ClientError,
    fdb::{UploadCondition, management::ManagementIntent},
};
use crate::logical_library::{
    ChangeLogicalLibrary, CreateLogicalLibrary, LogicalLibraryResources, ReplaceLogicalLibrary,
};
use crate::managed_catalog::ActiveLibraryRoute;
use std::path::Path;

struct OwnerFixture {
    client: FdbClient,
    address: String,
    send: Sender<ExecRequest>,
    events: Receiver<ExecEvent>,
    status: SharedStatus,
    permit: Arc<Permission>,
    server: tokio::task::JoinHandle<()>,
    http: Arc<HttpContext>,
    worker: std::thread::JoinHandle<()>,
}

impl OwnerFixture {
    fn take_over(&self, round: u64) {
        let grant = |phase| {
            assert!(
                self.permit
                    .renew(round, phase, Instant::now() + Duration::from_secs(180))
            );
        };
        grant(Phase::Fencing);
        self.send.send(ExecRequest::Takeover { round }).unwrap();
        let event = wait_event(&self.events, |e| {
            matches!(e, ExecEvent::Fenced { .. } | ExecEvent::FenceFailed { .. })
        });
        assert!(
            matches!(event, ExecEvent::Fenced {round: r, ..} if r == round),
            "{event:?}"
        );
        grant(Phase::Execution);
        self.send.send(ExecRequest::Recover { round }).unwrap();
        let event = wait_event(&self.events, |e| {
            matches!(e, ExecEvent::Serving { .. } | ExecEvent::Lost { .. })
        });
        assert!(
            matches!(event, ExecEvent::Serving {round: r, ..} if r == round),
            "{event:?}"
        );
        grant(Phase::Serving);
        self.status.lock().unwrap().executor_ready = true;
    }

    fn stop(self) {
        self.server.abort();
        self.send
            .send(ExecRequest::Shutdown { done: None })
            .unwrap();
        self.worker.join().unwrap();
    }
}

fn start_owner(
    rt: &tokio::runtime::Runtime,
    root: &Path,
    catalog: &Arc<ManagedCatalog>,
    serial: &str,
    route: ActiveLibraryRoute,
    round: u64,
    provider: impl FnOnce(Arc<Permission>) -> fdb_observations_fixture::Provider,
) -> OwnerFixture {
    let permit = Arc::new(Permission::default());
    let status = fdb_observations_fixture::make_status(2, round, route.frozen_index);
    status.lock().unwrap().executor_ready = false;
    let (send, receive) = mpsc::channel();
    let (events, receive_events) = mpsc::channel();
    let files = FileService::new(root.join(format!("spool-{round}"))).unwrap();
    let control = FdbOwnerControl {
        catalog: catalog.clone(),
        route,
        serial: serial.to_owned(),
        storage: Default::default(),
    };
    let fixture_provider = provider(permit.clone());
    let owner_files = files.clone();
    let owner_status = status.clone();
    let worker = std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            run_fdb_writer(
                Box::new(fixture_provider),
                fdb_observations_fixture::options(2),
                receive,
                events,
                owner_files,
                owner_status,
                control,
            );
        })
        .unwrap();
    send.send(ExecRequest::AttachPermission {
        permission: permit.clone(),
    })
    .unwrap();
    let context = Arc::new(HttpContext {
        files,
        status: status.clone(),
        exec: Mutex::new(send.clone()),
        node: Mutex::new(mpsc::channel().0),
        client_addrs: Default::default(),
        wait_timeout: Duration::from_secs(15),
    });
    let router = fdb_http::writer_router(
        catalog.clone(),
        [(serial.to_owned(), route)].into(),
        [(serial.to_owned(), context.clone())].into(),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = rt.spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    OwnerFixture {
        client: FdbClient::new([address.as_str()])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(15)),
        address,
        send,
        events: receive_events,
        status,
        permit,
        server,
        http: context,
        worker,
    }
}

fn run_management(
    client: &FdbClient,
    serial: &str,
    intent: ManagementIntent,
    request: &Path,
) -> Value {
    client
        .prepare_management(serial, intent)
        .unwrap()
        .save_new(request)
        .unwrap();
    client.submit_management(request).unwrap();
    wait_management(client, request)
}

fn wait_management(client: &FdbClient, request: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = client.management_request_status(request, false).unwrap();
        match task["status"].as_str().unwrap() {
            "succeeded" => return task,
            "queued" | "running" if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25))
            }
            _ => panic!("原任务未完成：{task}"),
        }
    }
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；只使用模拟带库与回环 HTTP"]
fn a_written_physical_pool_survives_activation_and_resource_release() {
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("POOL-MIGRATE-{ns}");
    let challenge = Uuid::new_v4();
    let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,
        "challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2 {"Leader"} else {"Follower"},
        "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
        "applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
    let route = rt
        .block_on(catalog.initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
        ))
        .unwrap();
    rt.block_on(catalog.enable_library_handoff_offline(&serial, ns))
        .unwrap();
    let round = route.initial_owner_epoch;
    let sim = SimLibrary::new(1, 4, 1);
    sim.enable_changer_pr();
    let barcode = "MIG001L8";
    sim.insert_cartridge(SimCartridge::blank(barcode, 128 << 20), 0)
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let root = std::env::temp_dir().join(format!("tapers-pool-migration-{ns}"));
    let provider = |round, permit| fdb_observations_fixture::Provider {
        serial: serial.clone(),
        changer: Arc::new(sim.changer_as(2)),
        drive: Arc::new(sim.drive_as(0, 2)),
        log: log.clone(),
        revoke: Arc::new(AtomicBool::new(false)),
        permission: permit,
        round,
    };
    let start = |round| {
        start_owner(&rt, &root, &catalog, &serial, route, round, |permit| {
            provider(round, permit)
        })
    };
    let original = start(round);
    original.take_over(round);
    let client = &original.client;
    let pool = Uuid::new_v4();
    let created = client
        .create_pool(&serial, ns, pool, "archive", 100, 1)
        .unwrap();
    run_management(
        client,
        &serial,
        ManagementIntent::TapeAssign {
            barcode: barcode.into(),
            pool_selector: "archive".into(),
            confirm_format: true,
            force_format: false,
        },
        &root.join("assign.json"),
    );
    let source = root.join("source");
    let bytes = b"committed to the original physical pool";
    std::fs::write(&source, bytes).unwrap();
    let path = "/before-activation";
    let journal = root.join("upload");
    let stat = client.stat(&serial, pool, "/").unwrap().unwrap();
    client
        .prepare_file(
            &serial,
            pool,
            path,
            &source,
            UploadCondition::Create {
                namespace: ns,
                parent: stat["id"].as_str().unwrap().parse().unwrap(),
            },
            Some(barcode),
            &journal,
        )
        .unwrap();
    assert_eq!(
        client.upload_saved(&journal).unwrap().details()["version"],
        1
    );
    let before = client.stat(&serial, pool, path).unwrap().unwrap();
    let digest = format!("{:x}", Sha256::digest(bytes));
    assert_eq!(before["digest"]["hex"], digest);

    let inventory = rt
        .block_on(catalog.active_library_inventory(&serial, route))
        .unwrap()
        .unwrap();
    let home = inventory["slot_layout"]["storage_start"].as_u64().unwrap() as u16;
    let other_slot = home + 1;
    let additional = CreateLogicalLibrary {
        version: 1,
        id: Uuid::new_v4(),
        name: "additional".into(),
        resources: LogicalLibraryResources {
            drives: vec![],
            storage_slots: vec![other_slot],
            ie_slots: vec![],
        },
    };
    let early = client
        .create_logical_library(&serial, ns, &additional)
        .unwrap_err();
    assert!(
        matches!(early, ClientError::Rejected { status: 409, .. }),
        "{early:?}"
    );
    assert!(early.to_string().contains("离线启用"), "{early}");
    assert_eq!(
        client.list_logical_libraries(&serial, ns).unwrap()["items"],
        json!([])
    );
    run_management(
        client,
        &serial,
        ManagementIntent::TapeMove {
            barcode: barcode.into(),
            destination: "homeslot".into(),
            pool_selector: Some("archive".into()),
        },
        &root.join("home.json"),
    );
    assert!(sim.loaded_barcode(0).is_none());
    let formats = |log: &[u8]| {
        log.iter()
            .filter(|op| **op == cdb::opcode::FORMAT_MEDIUM)
            .count()
    };
    let before_migration = log.lock().unwrap().len();
    let release = Uuid::new_v4();
    client.release_library(&serial, ns, release).unwrap();
    wait_phase(client, &serial, ns, release, "released");
    original.stop();

    let id = Uuid::new_v4();
    let offline = provider(round, Arc::new(Permission::default()));
    let activate = |plan| {
        rt.block_on(activate_logical_library_offline(
            &catalog, &serial, ns, id, release, plan, &offline,
        ))
        .unwrap()
    };
    let plan = activate(None);
    assert_eq!(plan["pools"][0]["id"], json!(pool));
    let done = activate(Some(
        plan["plan_sha256"].as_str().expect("迁移须有可提交计划"),
    ));
    assert_eq!(done["status"], "enabled");
    assert_eq!(done["logical_library"]["id"], json!(id));

    let current = start(round + 1);
    let client = &current.client;
    let acquire = Uuid::new_v4();
    assert_eq!(
        client
            .acquire_library(&serial, ns, acquire, release)
            .unwrap()["phase"],
        "loading"
    );
    current.take_over(round + 1);
    wait_phase(client, &serial, ns, acquire, "online");
    assert_eq!(
        client
            .create_pool(&serial, ns, pool, "archive", 100, 1)
            .unwrap()["pool"],
        created["pool"]
    );
    assert!(client.stat(&serial, pool, path).unwrap().is_none());
    let scoped = FdbClient::new([current.address.as_str()])
        .unwrap()
        .with_logical_library(id)
        .unwrap();
    let after = scoped.stat(&serial, pool, path).unwrap().unwrap();
    for field in ["id", "version", "volume_id", "pool_id", "digest"] {
        assert!(!before[field].is_null(), "缺少文件身份字段 {field}");
        assert_eq!(before[field], after[field], "迁移改变了 {field}");
    }
    let output = root.join("cold-read");
    assert_eq!(
        scoped.download_new(&serial, pool, path, &output).unwrap(),
        bytes.len() as u64
    );
    assert_eq!(
        Sha256::digest(std::fs::read(output).unwrap()),
        Sha256::digest(bytes)
    );
    assert_eq!(formats(&log.lock().unwrap()[before_migration..]), 0);

    run_management(
        client,
        &serial,
        ManagementIntent::LibraryRescan,
        &root.join("rescan.json"),
    );
    let resources: LogicalLibraryResources =
        serde_json::from_value(done["logical_library"]["resources"].clone()).unwrap();
    let remove = |slot| {
        let mut replacement = resources.clone();
        replacement.storage_slots.retain(|address| *address != slot);
        ChangeLogicalLibrary {
            version: 1,
            operation: Uuid::new_v4(),
            revision: 1,
            replacement: Some(ReplaceLogicalLibrary {
                name: "default".into(),
                resources: replacement,
            }),
        }
    };
    let occupied = client
        .change_logical_library(&serial, ns, id, remove(home))
        .unwrap_err();
    assert!(
        matches!(occupied, ClientError::Rejected { status: 409, .. }),
        "{occupied}"
    );
    assert!(occupied.to_string().contains("home 引用"), "{occupied}");
    let not_released = client
        .create_logical_library(&serial, ns, &additional)
        .unwrap_err();
    assert!(
        matches!(not_released, ClientError::Rejected { status: 409, .. }),
        "{not_released}"
    );
    assert!(
        not_released.to_string().contains("已归属"),
        "{not_released}"
    );
    client
        .change_logical_library(&serial, ns, id, remove(other_slot))
        .unwrap();
    assert_eq!(
        client
            .create_logical_library(&serial, ns, &additional)
            .unwrap()["logical_library"]["id"],
        json!(additional.id)
    );

    current.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；仅模拟设备、回环 HTTP"]
fn selected_pool_imports_existing_contents_and_recovers_lost_format_result() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("SELECT-{ns}");
    let challenge = Uuid::new_v4();
    let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,
        "challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2 {"Leader"} else {"Follower"},
        "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
        "applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
    let route = rt
        .block_on(catalog.initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
        ))
        .unwrap();
    rt.block_on(catalog.enable_library_handoff_offline(&serial, ns))
        .unwrap();
    let round = route.initial_owner_epoch;

    let existing = "EXIST1L8";
    let blank = "BLANK1L8";
    let source_volume = Uuid::new_v4();
    let source_body = b"selected existing LTFS bytes";
    let seed = SimLibrary::new(1, 1, 0);
    seed.insert_cartridge(SimCartridge::blank(existing, 128 << 20), 0)
        .unwrap();
    seed.load_into_drive(existing, 0).unwrap();
    let dev = seed.drive(0);
    crate::ltfs::mkltfs::mkltfs(
        &dev,
        &crate::ltfs::mkltfs::MkltfsOptions {
            volume_uuid: Some(source_volume),
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = crate::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/original", &mut std::io::Cursor::new(source_body))
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    TapeDrive::new(&dev).unload().unwrap();

    let sim = SimLibrary::new(1, 4, 1);
    sim.enable_changer_pr();
    sim.insert_cartridge(seed.cartridge(existing).unwrap(), 0)
        .unwrap();
    sim.insert_cartridge(SimCartridge::blank(blank, 128 << 20), 1)
        .unwrap();
    let root = std::env::temp_dir().join(format!("tapers-selected-pool-{ns}"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let drive = Arc::new(sim.drive_as(0, 2));
    let start = |round| {
        start_owner(&rt, &root, &catalog, &serial, route, round, |permission| {
            fdb_observations_fixture::Provider {
                serial: serial.clone(),
                changer: Arc::new(sim.changer_as(2)),
                drive: drive.clone(),
                log: log.clone(),
                revoke: Arc::new(AtomicBool::new(false)),
                permission,
                round,
            }
        })
    };
    let owner = start(round);
    owner.take_over(round);
    let client = &owner.client;
    run_management(
        client,
        &serial,
        ManagementIntent::LibraryRescan,
        &root.join("rescan.json"),
    );
    use crate::scsi::sim::{Fault, FaultAction};
    // The completed label and indexes must be read back after the final MAM
    // step fails, without issuing FORMAT a second time.
    drive.inject(Fault {
        opcode: cdb::opcode::WRITE_ATTRIBUTE,
        action: FaultAction::HostError(3),
        after: 6,
        remaining: 1,
    });
    let request = root.join("create.json");
    client
        .prepare_management(
            &serial,
            ManagementIntent::CreateSelected {
                name: "selected".into(),
                file_limit: 100,
                tapes: vec![existing.into(), blank.into()],
                confirm_format: true,
            },
        )
        .unwrap()
        .save_new(&request)
        .unwrap();
    let first = client.submit_management(&request).unwrap();
    assert_eq!(first["status"], "queued");
    let pool: Uuid = first["id"].as_str().unwrap().parse().unwrap();
    let before_takeover = Instant::now() + Duration::from_secs(20);
    while log
        .lock()
        .unwrap()
        .iter()
        .filter(|op| **op == cdb::opcode::WRITE_ATTRIBUTE)
        .count()
        < 7
    {
        assert!(
            Instant::now() < before_takeover,
            "未到达初始化原检查点 {}，owner 事件 {:?}",
            client.management_request_status(&request, false).unwrap(),
            owner.events.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    owner.permit.close(round);
    let interrupted = client.management_request_status(&request, false).unwrap();
    assert_eq!(interrupted["status"], "running");
    assert_eq!(interrupted["tapes"][0]["volume_uuid"], json!(source_volume));
    assert_eq!(
        interrupted["active_tape"]["progress"]["phase"],
        "format_result_pending"
    );
    assert!(matches!(
        client.pool(&serial, &ns.to_string(), "selected"),
        Err(ClientError::Rejected { status: 404, .. })
    ));
    let original_format_volume = interrupted["active_tape"]["checkpoint"]["format_volume"].clone();
    assert_eq!(
        original_format_volume,
        interrupted["active_tape"]["operation"]
    );
    owner.stop();

    let owner = start(round + 1);
    owner.take_over(round + 1);
    let client = &owner.client;
    let deadline = Instant::now() + Duration::from_secs(20);
    let done = loop {
        let task = client.management_request_status(&request, false).unwrap();
        match task["status"].as_str().unwrap() {
            "succeeded" => break task,
            "queued" | "running" if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => panic!(
                "创建任务未完成：{task}，owner 事件 {:?}",
                owner.events.try_iter().collect::<Vec<_>>()
            ),
        }
    };
    assert_eq!(done["tapes"][0]["volume_uuid"], json!(source_volume));
    assert_eq!(done["tapes"][1]["volume_uuid"], original_format_volume);
    assert_eq!(done["pool"]["id"], json!(pool));
    assert_eq!(client.submit_management(&request).unwrap(), done);
    let body = root.join("readback");
    assert_eq!(
        client
            .download_new(&serial, pool, "/original", &body)
            .unwrap(),
        source_body.len() as u64
    );
    assert_eq!(std::fs::read(&body).unwrap(), source_body);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|op| **op == cdb::opcode::FORMAT_MEDIUM)
            .count(),
        1
    );
    let interrupted_blank = "UNKN01L8";
    sim.insert_cartridge(SimCartridge::blank(interrupted_blank, 128 << 20), 2)
        .unwrap();
    run_management(
        client,
        &serial,
        ManagementIntent::LibraryRescan,
        &root.join("rescan-interrupted.json"),
    );
    drive.inject(Fault {
        opcode: cdb::opcode::FORMAT_MEDIUM,
        action: FaultAction::HostError(3),
        after: 0,
        remaining: 1,
    });
    let unknown = root.join("unknown.json");
    client
        .prepare_management(
            &serial,
            ManagementIntent::CreateSelected {
                name: "interrupted".into(),
                file_limit: 100,
                tapes: vec![interrupted_blank.into()],
                confirm_format: true,
            },
        )
        .unwrap()
        .save_new(&unknown)
        .unwrap();
    client.submit_management(&unknown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let checkpoint = loop {
        let task = client.management_request_status(&unknown, false).unwrap();
        if task["active_tape"]["checkpoint"]["detail"]
            .as_str()
            .is_some_and(|s| s.contains("初始化结果未知"))
        {
            break task;
        }
        assert!(Instant::now() < deadline, "未到达未知初始化检查点：{task}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        checkpoint["active_tape"]["progress"]["phase"],
        "format_result_pending"
    );
    owner.permit.close(round + 1);
    owner.stop();

    let owner = start(round + 2);
    owner.take_over(round + 2);
    std::thread::sleep(Duration::from_secs(2));
    let retained = owner
        .client
        .management_request_status(&unknown, false)
        .unwrap();
    assert_eq!(
        retained["active_tape"]["operation"],
        checkpoint["active_tape"]["operation"]
    );
    assert_eq!(
        retained["active_tape"]["progress"]["phase"],
        "format_result_pending"
    );
    assert_eq!(retained["status"], "running");
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|op| **op == cdb::opcode::FORMAT_MEDIUM)
            .count(),
        2
    );
    owner.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；两个模拟物理库与回环 HTTP"]
fn replicated_pool_resumes_after_owner_change_and_conflict_with_body_proof() {
    use crate::replicated_pool::{
        CreateReplicatedPool, ReplicatedPoolMember, ResolveReplicatedPoolConflict,
    };
    let _ = env_logger::builder().is_test(true).try_init();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let initialize = |prefix| {
        let namespace = Uuid::new_v4();
        let serial = format!("{prefix}-{namespace}");
        let challenge = Uuid::new_v4();
        let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,
            "challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2 {"Leader"} else {"Follower"},
            "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
            "applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
        let route = rt
            .block_on(catalog.initialize_native_library(
                namespace,
                &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
            ))
            .unwrap();
        rt.block_on(catalog.enable_library_handoff_offline(&serial, namespace))
            .unwrap();
        (serial, namespace, route)
    };
    let (serial_a, ns_a, route_a) = initialize("REPA");
    let (serial_b, ns_b, route_b) = initialize("REPB");
    let root = std::env::temp_dir().join(format!("tapers-replica-{ns_a}"));
    let seed = |sim: &SimLibrary, slot, barcode, entries: &[(&str, &[u8])]| {
        let src = SimLibrary::new(1, 1, 0);
        src.insert_cartridge(SimCartridge::blank(barcode, 128 << 20), 0)
            .unwrap();
        src.load_into_drive(barcode, 0).unwrap();
        let dev = src.drive(0);
        let uuid = Uuid::new_v4();
        crate::ltfs::mkltfs::mkltfs(
            &dev,
            &crate::ltfs::mkltfs::MkltfsOptions {
                volume_uuid: Some(uuid),
                ..Default::default()
            },
        )
        .unwrap();
        let mut volume = crate::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
        // Exercise LTFS indexes without any content checksum xattr.
        volume.set_hash_policy(crate::ltfs::volume::HashPolicy {
            md5: false,
            sha256: false,
        });
        for (path, body) in entries {
            volume
                .append_file(path, &mut std::io::Cursor::new(body))
                .unwrap();
        }
        volume.commit().unwrap();
        drop(volume);
        TapeDrive::new(&dev).unload().unwrap();
        sim.insert_cartridge(src.cartridge(barcode).unwrap(), slot)
            .unwrap();
        uuid
    };
    let a = SimLibrary::new(1, 5, 1);
    let b = SimLibrary::new(1, 4, 1);
    a.enable_changer_pr();
    b.enable_changer_pr();
    let uuid_a = seed(
        &a,
        0,
        "REPA01L8",
        &[
            ("/collision", b"A-side"),
            ("/same", b"shared"),
            ("/tree", b"file"),
        ],
    );
    seed(
        &a,
        1,
        "REPA02L8",
        &[("/same", b"shared"), ("/a-only", b"single")],
    );
    let uuid_b = seed(
        &b,
        0,
        "REPB01L8",
        &[
            ("/collision", b"B-side"),
            ("/same", b"shared"),
            ("/tree/child", b"child"),
        ],
    );
    b.insert_cartridge(SimCartridge::blank("REPB02L8", 128 << 20), 1)
        .unwrap();

    let log_a = Arc::new(Mutex::new(Vec::new()));
    let log_b = Arc::new(Mutex::new(Vec::new()));
    let provider =
        |serial: &str, sim: &SimLibrary, log: &Arc<Mutex<Vec<u8>>>, round, permission| {
            fdb_observations_fixture::Provider {
                serial: serial.to_owned(),
                changer: Arc::new(sim.changer_as(2)),
                drive: Arc::new(sim.drive_as(0, 2)),
                log: log.clone(),
                revoke: Arc::new(AtomicBool::new(false)),
                permission,
                round,
            }
        };
    let start_a = |round| {
        start_owner(
            &rt,
            &root.join("a"),
            &catalog,
            &serial_a,
            route_a,
            round,
            |permit| provider(&serial_a, &a, &log_a, round, permit),
        )
    };
    let start_b = |round| {
        start_owner(
            &rt,
            &root.join("b"),
            &catalog,
            &serial_b,
            route_b,
            round,
            |permit| provider(&serial_b, &b, &log_b, round, permit),
        )
    };
    let round_a = route_a.initial_owner_epoch;
    let round_b = route_b.initial_owner_epoch;
    let owner_a = start_a(round_a);
    owner_a.take_over(round_a);
    let owner_b = start_b(round_b);
    owner_b.take_over(round_b);
    for (owner, serial, prefix) in [(&owner_a, &serial_a, "a"), (&owner_b, &serial_b, "b")] {
        run_management(
            &owner.client,
            serial,
            ManagementIntent::LibraryRescan,
            &root.join(format!("{prefix}-scan.json")),
        );
    }
    owner_b.permit.close(round_b);
    owner_b.stop();

    let routes = [(serial_a.clone(), route_a), (serial_b.clone(), route_b)].into();
    let router = fdb_http::owner_router(
        catalog.clone(),
        routes,
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let server = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = server.local_addr().unwrap().to_string();
    let http = rt.spawn(async move { axum::serve(server, router).await.unwrap() });
    let client = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(10));
    let member = |serial: &str, namespace, name: &str, tapes: &[&str], confirm_format| {
        ReplicatedPoolMember {
            library_serial: serial.to_owned(),
            namespace,
            pool_id: Uuid::new_v4(),
            name: name.to_owned(),
            logical_library: None,
            tapes: tapes.iter().map(|barcode| (*barcode).to_owned()).collect(),
            confirm_format,
        }
    };
    let request = CreateReplicatedPool {
        version: 1,
        id: Uuid::new_v4(),
        name: format!("archives-{ns_a}"),
        created_at_millis: 1,
        members: vec![
            member(
                &serial_a,
                ns_a,
                "archives-a",
                &["REPA01L8", "REPA02L8"],
                false,
            ),
            member(
                &serial_b,
                ns_b,
                "archives-b",
                &["REPB01L8", "REPB02L8"],
                true,
            ),
        ],
    };
    let mut rejected = request.clone();
    rejected.members[1].tapes[0] = "MISSING".into();
    assert!(matches!(
        client.create_replicated_pool(&rejected),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        client.replicated_pool_status(request.id),
        Err(ClientError::Rejected { status: 404, .. })
    ));
    assert_eq!(
        client.create_replicated_pool(&request).unwrap()["id"],
        json!(request.id)
    );
    rt.block_on(catalog.claim_active_owner_epoch(&serial_b, round_b + 1))
        .unwrap();

    let wait = |predicate: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let status = client.replicated_pool_status(request.id).unwrap();
            if predicate(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "原创建任务等待超时 {status}");
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let paused = wait(&|status| status["members"][0]["status"] == "prepared");
    assert_eq!(paused["status"], "waiting_library");
    assert_eq!(paused["members"][1]["current_owner_epoch"], round_b + 1);
    assert!(matches!(
        client.replicated_pool_files(request.id, None, 20),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        owner_a
            .client
            .release_library(&serial_a, ns_a, Uuid::new_v4()),
        Err(ClientError::Rejected { status: 409, .. })
    ));

    let owner_b = start_b(round_b + 2);
    owner_b.take_over(round_b + 2);
    let blocked = wait(&|status| {
        status["conflict"]["path"] == "/collision"
            && status["members"][1]["tapes"][0]["status"] == "succeeded"
    });
    let conflict = &blocked["conflict"];
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(conflict["sources"][0]["volume_uuid"], json!(uuid_a));
    assert_eq!(conflict["sources"][1]["volume_uuid"], json!(uuid_b));
    assert_eq!(conflict["sources"][1]["barcode"], "REPB01L8");
    assert_eq!(conflict["sources"][1]["library_serial"], serial_b);
    assert_eq!(
        conflict["sources"][0]["size"],
        conflict["sources"][1]["size"]
    );
    assert_ne!(
        conflict["sources"][0]["sha256"],
        conflict["sources"][1]["sha256"]
    );
    assert!(!log_b.lock().unwrap().contains(&cdb::opcode::FORMAT_MEDIUM));

    let choice = ResolveReplicatedPoolConflict {
        version: 1,
        path: "/collision".into(),
        volume_uuid: uuid_b,
        revision: conflict["revision"].as_u64().unwrap(),
    };
    client
        .resolve_replicated_pool_conflict(request.id, &choice)
        .unwrap();
    let structural = wait(&|status| status["conflict"]["path"] == "/tree");
    let choice_file = ResolveReplicatedPoolConflict {
        version: 1,
        path: "/tree".into(),
        volume_uuid: uuid_a,
        revision: structural["conflict"]["revision"].as_u64().unwrap(),
    };
    assert!(matches!(
        client.resolve_replicated_pool_conflict(request.id, &choice_file),
        Err(ClientError::Rejected { status: 400, .. })
    ));
    client
        .resolve_replicated_pool_conflict(
            request.id,
            &ResolveReplicatedPoolConflict {
                volume_uuid: uuid_b,
                ..choice_file
            },
        )
        .unwrap();
    let completed = wait(&|status| status["status"] == "succeeded");
    assert_eq!(completed, client.create_replicated_pool(&request).unwrap());
    assert_eq!(
        completed,
        client
            .resolve_replicated_pool_conflict(request.id, &choice)
            .unwrap()
    );
    assert!(matches!(
        client.resolve_replicated_pool_conflict(
            request.id,
            &ResolveReplicatedPoolConflict {
                volume_uuid: uuid_a,
                ..choice.clone()
            }
        ),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert_eq!(
        log_b
            .lock()
            .unwrap()
            .iter()
            .filter(|code| **code == cdb::opcode::FORMAT_MEDIUM)
            .count(),
        1
    );
    assert!(!log_a.lock().unwrap().contains(&cdb::opcode::WRITE_6));

    let file_page = client.replicated_pool_files(request.id, None, 2).unwrap();
    let items = file_page["items"].as_array().unwrap();
    assert_eq!(items[0]["path"], "/a-only");
    assert_eq!(items[0]["replica_count"], 1);
    assert_eq!(items[1]["path"], "/collision");
    assert_eq!(
        items[1]["sha256"],
        format!("{:x}", Sha256::digest(b"B-side"))
    );
    assert_eq!(items[1]["replica_count"], 1);
    let last_page = client
        .replicated_pool_files(request.id, file_page["next"].as_str(), 2)
        .unwrap();
    assert_eq!(last_page["items"][0]["path"], "/same");
    assert_eq!(last_page["items"][0]["replica_count"], 2);
    assert_eq!(last_page["items"][1]["kind"], "directory");
    let descendants = client
        .replicated_pool_files(request.id, last_page["next"].as_str(), 10)
        .unwrap();
    assert_eq!(descendants["items"][0]["path"], "/tree/child");

    let member_a = &request.members[0];
    let attempt = root.join("delete-member.json");
    owner_a
        .client
        .prepare_management(
            &serial_a,
            ManagementIntent::Delete {
                selector: member_a.name.clone(),
            },
        )
        .unwrap()
        .save_new(&attempt)
        .unwrap();
    assert!(owner_a.client.submit_management(&attempt).is_err());
    assert!(
        owner_a
            .client
            .stat(&serial_a, member_a.pool_id, "/same")
            .is_err()
    );

    let mut duplicate = request.clone();
    duplicate.id = Uuid::new_v4();
    duplicate.name = "should-reject-existing-member".into();
    assert!(client.create_replicated_pool(&duplicate).is_err());
    for (index, barcode) in ["REPA01L8", "REPA02L8"].iter().enumerate() {
        if a.loaded_barcode(0).as_deref() == Some(*barcode) {
            run_management(
                &owner_a.client,
                &serial_a,
                ManagementIntent::TapeMove {
                    barcode: (*barcode).into(),
                    destination: "homeslot".into(),
                    pool_selector: Some(member_a.name.clone()),
                },
                &root.join(format!("replica-home-{index}.json")),
            );
        }
    }
    let release = Uuid::new_v4();
    owner_a
        .client
        .release_library(&serial_a, ns_a, release)
        .unwrap();
    wait_phase(&owner_a.client, &serial_a, ns_a, release, "released");
    owner_a.stop();
    let logical = Uuid::new_v4();
    let offline = provider(
        &serial_a,
        &a,
        &log_a,
        round_a,
        Arc::new(Permission::default()),
    );
    let activate = |proof| {
        rt.block_on(activate_logical_library_offline(
            &catalog, &serial_a, ns_a, logical, release, proof, &offline,
        ))
        .unwrap()
    };
    let plan = activate(None);
    assert_eq!(plan["pools"][0]["id"], json!(member_a.pool_id));
    let activation = activate(Some(plan["plan_sha256"].as_str().unwrap()));
    assert_eq!(activation["logical_library"]["id"], json!(logical));
    let owner_a = start_a(round_a + 1);
    let acquire = Uuid::new_v4();
    owner_a
        .client
        .acquire_library(&serial_a, ns_a, acquire, release)
        .unwrap();
    owner_a.take_over(round_a + 1);
    wait_phase(&owner_a.client, &serial_a, ns_a, acquire, "online");
    assert_eq!(client.create_replicated_pool(&request).unwrap(), completed);
    let migrated = client.replicated_pool_status(request.id).unwrap();
    assert_eq!(migrated["id"], json!(request.id));
    assert_eq!(migrated["members"][0]["pool_id"], json!(member_a.pool_id));
    assert_eq!(migrated["members"][0]["namespace"], json!(ns_a));
    assert!(migrated["members"][0]["original_logical_library"].is_null());
    assert_eq!(migrated["members"][0]["logical_library"], json!(logical));
    assert_eq!(
        client.replicated_pool_files(request.id, None, 2).unwrap(),
        file_page
    );
    run_management(
        &owner_a.client,
        &serial_a,
        ManagementIntent::LibraryRescan,
        &root.join("after-activation.json"),
    );

    let resources: LogicalLibraryResources =
        serde_json::from_value(activation["logical_library"]["resources"].clone()).unwrap();
    let slot = *resources.storage_slots.last().unwrap();
    let mut default_resources = resources;
    default_resources
        .storage_slots
        .retain(|address| *address != slot);
    owner_a
        .client
        .change_logical_library(
            &serial_a,
            ns_a,
            logical,
            ChangeLogicalLibrary {
                version: 1,
                operation: Uuid::new_v4(),
                revision: 1,
                replacement: Some(ReplaceLogicalLibrary {
                    name: "default".into(),
                    resources: default_resources,
                }),
            },
        )
        .unwrap();
    let second_logical = Uuid::new_v4();
    owner_a
        .client
        .create_logical_library(
            &serial_a,
            ns_a,
            &CreateLogicalLibrary {
                version: 1,
                id: second_logical,
                name: "secondary".into(),
                resources: LogicalLibraryResources {
                    drives: vec![],
                    storage_slots: vec![slot],
                    ie_slots: vec![],
                },
            },
        )
        .unwrap();

    let empty = |serial, ns, name, scope| ReplicatedPoolMember {
        logical_library: scope,
        ..member(serial, ns, name, &[], false)
    };
    let layouts = [
        vec![
            empty(&serial_a, ns_a, "same-first", Some(logical)),
            empty(&serial_a, ns_a, "same-second", Some(logical)),
        ],
        vec![
            empty(&serial_a, ns_a, "logical-first", Some(logical)),
            empty(&serial_a, ns_a, "logical-second", Some(second_logical)),
        ],
        vec![
            empty(&serial_a, ns_a, "mixed-first", Some(logical)),
            empty(&serial_b, ns_b, "mixed-second", None),
        ],
    ];
    for (index, members) in layouts.into_iter().enumerate() {
        let id = Uuid::new_v4();
        client
            .create_replicated_pool(&CreateReplicatedPool {
                version: 1,
                id,
                name: format!("layout-{index}-{id}"),
                created_at_millis: 1,
                members,
            })
            .unwrap();
        let cutoff = Instant::now() + Duration::from_secs(10);
        loop {
            let status = client.replicated_pool_status(id).unwrap();
            if status["status"] == "succeeded" {
                assert_eq!(
                    status["failure_domains"].as_array().unwrap().len(),
                    if index == 2 { 2 } else { 1 }
                );
                break;
            }
            assert!(Instant::now() < cutoff, "布局未完成 {status}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    verify_replica_archive(
        &rt,
        &catalog,
        &root,
        &request,
        [&owner_a, &owner_b],
        [route_a, route_b],
        (&a, logical, slot),
    );

    owner_a.stop();
    owner_b.stop();
    http.abort();
    std::fs::remove_dir_all(root).unwrap();
}

fn verify_replica_archive(
    rt: &tokio::runtime::Runtime,
    catalog: &Arc<ManagedCatalog>,
    root: &Path,
    pool: &crate::replicated_pool::CreateReplicatedPool,
    owners: [&OwnerFixture; 2],
    routes: [ActiveLibraryRoute; 2],
    (sim, logical, other_slot): (&SimLibrary, Uuid, u16),
) {
    use crate::replicated_pool::ReplicatedArchive;
    let drop_reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drop_next = drop_reply.clone();
    let router = fdb_http::writer_router(
        catalog.clone(),
        pool.members
            .iter()
            .zip(routes)
            .map(|(member, route)| (member.library_serial.clone(), route))
            .collect(),
        pool.members
            .iter()
            .zip(owners)
            .map(|(member, owner)| (member.library_serial.clone(), owner.http.clone()))
            .collect(),
        Default::default(),
        Default::default(),
    )
    .unwrap()
    .layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let drop_reply = drop_next.clone();
            async move {
                use axum::response::IntoResponse;
                let inject = request.method() == axum::http::Method::PUT
                    && request.uri().path().contains("/archives/");
                let response = next.run(request).await;
                if inject
                    && response.status().is_success()
                    && drop_reply.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return axum::http::StatusCode::GATEWAY_TIMEOUT.into_response();
                }
                response
            }
        },
    ));
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let client = FdbClient::new([listener.local_addr().unwrap().to_string()])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(25));
    let server = rt.spawn(async move { axum::serve(listener, router).await.unwrap() });
    let parent = pool.id;
    let a = pool.members[0].pool_id;
    let b = pool.members[1].pool_id;
    let download = |path: &str, member, expected: &[u8]| {
        let out = root.join(Uuid::new_v4().to_string());
        assert_eq!(
            client
                .download_replicated_new(parent, path, member, &out)
                .unwrap(),
            expected.len() as u64
        );
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        std::fs::remove_file(out).unwrap();
    };
    download("/collision", Some(b), b"B-side");
    let path = "/zz-archive";
    let input = |body: &[u8], previous| ReplicatedArchive {
        operation: Uuid::new_v4(),
        path: path.into(),
        previous,
        size: body.len() as u64,
        sha256: Sha256::digest(body).into(),
    };
    let first = input(b"version one", None);
    let body = root.join("archive-body");
    std::fs::write(&body, b"version one").unwrap();
    assert_eq!(
        client.admit_replicated_archive(parent, &first).unwrap()["status"],
        "waiting"
    );
    assert!(client.replicated_file_stat(parent, path).unwrap().is_none());
    assert!(matches!(
        client.archive_replicated(parent, &first, None),
        Err(ClientError::Indeterminate(_))
    ));
    assert!(
        client
            .admit_replicated_archive(parent, &input(b"collides", None))
            .is_err()
    );
    std::fs::write(&body, b"wrong bytes").unwrap();
    assert!(matches!(
        client.upload_replicated_member(parent, &first, a, &std::fs::File::open(&body).unwrap()),
        Err(ClientError::Rejected { status: 422, .. })
    ));
    std::fs::write(&body, b"version one").unwrap();
    drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        client
            .upload_replicated_member(parent, &first, a, &std::fs::File::open(&body).unwrap())
            .unwrap()["status"],
        "waiting"
    );
    assert!(client.replicated_file_stat(parent, path).unwrap().is_none());
    std::fs::remove_file(&body).unwrap();
    drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
    let committed = client.archive_replicated(parent, &first, None).unwrap();
    assert!(!drop_reply.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(committed["status"], "committed");
    assert_eq!(committed["file_version"], 1);
    assert_eq!(
        committed,
        client.admit_replicated_archive(parent, &first).unwrap()
    );
    assert_eq!(
        committed,
        client
            .replicated_archive_status(parent, first.operation)
            .unwrap()
    );
    for member in [a, b] {
        download(path, Some(member), b"version one");
    }

    let next = input(b"replacement", Some(1));
    std::fs::write(&body, b"replacement").unwrap();
    client.admit_replicated_archive(parent, &next).unwrap();
    owners[0].status.lock().unwrap().executor_ready = false;
    download(path, None, b"version one");
    let partial = client
        .upload_replicated_member(parent, &next, b, &std::fs::File::open(&body).unwrap())
        .unwrap();
    assert_eq!(partial["status"], "waiting");
    assert!(partial["members"][0]["commit"].is_null());
    assert_eq!(
        client.replicated_file_stat(parent, path).unwrap().unwrap()["file_version"],
        1
    );
    download(path, None, b"version one");
    owners[0].status.lock().unwrap().executor_ready = true;
    assert_eq!(
        client.archive_replicated(parent, &next, None).unwrap()["file_version"],
        2
    );
    for member in [a, b] {
        download(path, Some(member), b"replacement");
    }

    let archived = client
        .replicated_archive_status(parent, next.operation)
        .unwrap();
    let tape = archived["members"][0]["commit"]["barcode"]
        .as_str()
        .unwrap();
    use crate::scsi::sim::LogicalObject;
    let change_copy = |from: &[u8], to: &[u8]| {
        let mut changed = 0;
        assert!(sim.with_cartridge_mut(tape, |cartridge| {
            for record in cartridge
                .partitions
                .iter_mut()
                .flat_map(|part| &mut part.objects)
            {
                if let LogicalObject::Record(bytes) = record
                    && bytes.starts_with(from)
                {
                    bytes[..from.len()].copy_from_slice(to);
                    changed += 1;
                }
            }
        }));
        assert_eq!(changed, 1);
    };
    change_copy(b"replacement", b"bad payload");
    let corrupt = root.join("archive-corrupt-copy");
    assert!(
        client
            .download_replicated_new(parent, path, Some(a), &corrupt)
            .is_err()
    );
    assert!(!corrupt.exists());
    download(path, None, b"replacement");
    change_copy(b"bad payload", b"replacement");
    let serial = &pool.members[0].library_serial;
    let manager = &owners[0].client;
    let movement = |destination: &str, name: &str| {
        let row = manager.tape_inventory_entry(serial, None, tape).unwrap();
        let assignment = &row["tape"]["assignment"];
        let saved = root.join(name);
        manager.prepare_control(serial, pool.members[0].namespace, json!({
            "kind":"media-move","pool":a,"barcode":tape,"volume":assignment["volume"],
            "revision":assignment["revision"],"destination":destination,"logical_library":logical,
        })).unwrap().save_new(&saved).unwrap();
        manager.submit_management(&saved).unwrap();
        wait_management(manager, &saved)
    };
    let exported = movement("ieslot", "replica-to-ie.json");
    let home = exported["result"]["home_address"].as_u64().unwrap() as u16;
    assert_ne!(exported["result"]["to"], json!(home));
    download(path, None, b"replacement");
    assert_eq!(
        movement("homeslot", "replica-ie-home.json")["result"]["to"],
        json!(home)
    );
    download(path, Some(a), b"replacement");
    movement("homeslot", "replica-after-cold-home.json");

    let next_round = std::cell::Cell::new(routes[0].initial_owner_epoch + 2);
    // External moves are observed while reacquiring the library from the
    // original release receipt, which also fences every device first.
    let external_move = |source, target| {
        let ns = pool.members[0].namespace;
        let release = Uuid::new_v4();
        let (_, round) = rt
            .block_on(catalog.active_library_owner(serial))
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (reply, received) = mpsc::channel();
            owners[0]
                .send
                .send(ExecRequest::LibraryHandoff {
                    round,
                    operation: release,
                    deadline,
                    reply,
                })
                .unwrap();
            match received.recv_timeout(Duration::from_secs(5)).unwrap() {
                Ok(result)
                    if result.phase == crate::managed_catalog::LibraryHandoffPhase::Releasing =>
                {
                    break;
                }
                Err(ReadError::Busy(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                result => panic!("交还原模拟设备失败，操作 {release}: {result:?}"),
            }
        }
        wait_phase(manager, serial, ns, release, "released");
        let transport = sim.changer();
        let mut changer = MediumChanger::new(&transport);
        changer.load_address_map().unwrap();
        changer.move_medium(source, target).unwrap();
        let acquire = Uuid::new_v4();
        manager
            .acquire_library(serial, ns, acquire, release)
            .unwrap();
        let round = next_round.get();
        owners[0].take_over(round);
        owners[0]
            .status
            .lock()
            .unwrap()
            .executor
            .as_mut()
            .unwrap()
            .round = round;
        wait_phase(manager, serial, ns, acquire, "online");
        next_round.set(round + 1);
    };
    external_move(home, other_slot);
    let fault = manager
        .logical_library(serial, pool.members[0].namespace, &logical.to_string())
        .unwrap();
    assert!(fault.to_string().contains(tape), "{fault}");
    let blocked_input = input(b"replacement", Some(2));
    client
        .admit_replicated_archive(parent, &blocked_input)
        .unwrap();
    let write_result = client.upload_replicated_member(
        parent,
        &blocked_input,
        a,
        &std::fs::File::open(&body).unwrap(),
    );
    assert!(
        matches!(&write_result,
        Err(ClientError::Indeterminate(message)) if message.contains(tape)),
        "{write_result:?}"
    );
    download(path, None, b"replacement");
    assert_eq!(
        client
            .upload_replicated_member(
                parent,
                &blocked_input,
                b,
                &std::fs::File::open(&body).unwrap()
            )
            .unwrap()["status"],
        "waiting"
    );
    external_move(other_slot, home);
    let resumed = client
        .archive_replicated(parent, &blocked_input, None)
        .unwrap();
    assert_eq!(resumed["status"], "committed");
    assert_eq!(resumed["file_version"], 3);
    download(path, Some(a), b"replacement");

    let aborted = input(b"replacement", Some(3));
    client.admit_replicated_archive(parent, &aborted).unwrap();
    client
        .upload_replicated_member(parent, &aborted, a, &std::fs::File::open(&body).unwrap())
        .unwrap();
    let cancelled = client
        .cancel_replicated_archive(parent, aborted.operation)
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert!(!cancelled["members"][0]["commit"].is_null());
    assert_eq!(
        client.replicated_file_stat(parent, path).unwrap().unwrap()["file_version"],
        3
    );
    let reuse = input(b"replacement", Some(3));
    client.admit_replicated_archive(parent, &reuse).unwrap();
    assert_eq!(
        client
            .cancel_replicated_archive(parent, reuse.operation)
            .unwrap()["status"],
        "cancelled"
    );
    server.abort();
}
