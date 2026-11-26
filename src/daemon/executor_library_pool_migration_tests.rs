//! Public SDK and owner round trip over a private FDB and simulated tape.
use super::*;
use crate::client::{
    ClientError,
    fdb::{UploadCondition, management::ManagementIntent},
};
use crate::logical_library::{
    ChangeLogicalLibrary, CreateLogicalLibrary, LogicalLibraryResources, ReplaceLogicalLibrary,
};
use std::path::Path;

struct OwnerFixture {
    client: FdbClient,
    address: String,
    send: Sender<ExecRequest>,
    events: Receiver<ExecEvent>,
    status: SharedStatus,
    permit: Arc<Permission>,
    server: tokio::task::JoinHandle<()>,
    worker: std::thread::JoinHandle<()>,
}

impl OwnerFixture {
    fn take_over(&self, round: u64) {
        let grant = |phase| {
            assert!(
                self.permit
                    .renew(round, phase, Instant::now() + Duration::from_secs(60))
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
        let permit = Arc::new(Permission::default());
        let status = fdb_observations_fixture::make_status(2, round, route.frozen_index);
        status.lock().unwrap().executor_ready = false;
        let (send, receive) = mpsc::channel();
        let (events, receive_events) = mpsc::channel();
        let files = FileService::new(root.join(format!("spool-{round}"))).unwrap();
        let control = FdbOwnerControl {
            catalog: catalog.clone(),
            route,
            serial: serial.clone(),
            storage: Default::default(),
        };
        let fixture_provider = provider(round, permit.clone());
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
            [(serial.clone(), route)].into(),
            [(serial.clone(), context)].into(),
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
            worker,
        }
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
