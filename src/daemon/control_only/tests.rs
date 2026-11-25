use super::super::{
    files::{FileService, ServiceError, TapeIdent, TapeLimits},
    http::HttpContext,
    node::NodeStatus,
};
use super::*;
use std::sync::Mutex;

struct Reader(Mutex<std::result::Result<Option<ExpectedRoute>, String>>);
impl AuthorityReader for Reader {
    fn active_route(&self, _: &str) -> Result<Option<ExpectedRoute>> {
        self.0.lock().unwrap().clone().map_err(invalid)
    }
}
struct Fixture {
    root: PathBuf,
    cluster: PathBuf,
    config: LibrariesConfig,
    binding: Arc<ControlOnlyBinding>,
    reader: Arc<Reader>,
    source: BoundDirectoryAudit,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("control-only-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let cluster = root.join("fdb.cluster");
        std::fs::write(&cluster, b"test-only:identity@127.0.0.1:1").unwrap();
        let config = LibrariesConfig::parse(&serde_json::json!({"default_library":"LIB-B","libraries":[{"changer_serial":"LIB-B","drive_serials":["D1","D2"]}]})).unwrap();
        std::fs::write(
            root.join("libraries.json"),
            serde_json::to_vec(&config.value()).unwrap(),
        )
        .unwrap();
        let mut store =
            super::super::store::RaftStore::open(&root.join("raft.db"), &[1, 2, 3]).unwrap();
        let control = super::super::state::ControlState {
            applied_index: 72616,
            catalog_quiesce_index: Some(2058),
            catalog_frozen_index: Some(2745),
            ..Default::default()
        };
        let value: serde_json::Value = serde_json::from_slice(
            &super::super::state::ControlSnapshot {
                control,
                source: 1,
                directory_index: 71998,
            }
            .encode(),
        )
        .unwrap();
        let mut snap = raft::eraftpb::Snapshot::default();
        snap.mut_metadata().index = 72616;
        snap.mut_metadata().term = 2;
        snap.mut_metadata()
            .set_conf_state(raft::eraftpb::ConfState::from((vec![1, 2, 3], vec![])));
        snap.set_data(serde_json::to_vec(&serde_json::json!({"cluster_version":2,"default_library":"LIB-B","source":1,"directory_index":71998,"controls":{"LIB-B":value,"@gateway":value}})).unwrap().into());
        store.install_snapshot(&snap).unwrap();
        drop(store);
        drop(Directory::open(&root.join("directory.db")).unwrap());
        let db = rusqlite::Connection::open(root.join("directory.db")).unwrap();
        db.execute_batch("INSERT OR REPLACE INTO meta VALUES ('applied_index',7133),('catalog_frozen_index',2745);").unwrap();
        drop(db);
        let route = ExpectedRoute {
            namespace: uuid::Uuid::new_v4().to_string(),
            revision: 17,
            frozen_index: 2745,
            certificate_sha256: "ab".repeat(32),
            initial_owner_epoch: 1,
        };
        let reader = Arc::new(Reader(Mutex::new(Ok(Some(route.clone())))));
        let meta = std::fs::metadata(&root).unwrap();
        let config_sha = digest(&serde_json::to_vec(&config.value()).unwrap());
        let mut binding = ControlOnlyBinding {
            version: 1,
            node: 1,
            members: members().into_iter().collect(),
            cluster_sha256: digest(&std::fs::read(&cluster).unwrap()),
            libraries_sha256: config_sha.clone(),
            group: config_sha,
            data_root: root.clone(),
            data_root_dev: meta.dev(),
            data_root_inode: meta.ino(),
            original_chain_sha256: String::new(),
            original_chain_manifest: PathBuf::new(),
            raft: FileSeal::read(&root.join("raft.db")).unwrap(),
            manifest: FileSeal::read(&root.join("libraries.json")).unwrap(),
            libraries: BTreeMap::from([(
                "LIB-B".into(),
                LibraryBinding {
                    directory: FileSeal::read(&root.join("directory.db")).unwrap(),
                    quiesce_index: 2058,
                    frozen_index: 2745,
                    catalog: CatalogExpectation::ExistingRoute { route },
                },
            )]),
        };
        super::binding_fixture::seal(&mut binding, &config);
        let binding = Arc::new(binding);
        binding
            .validate_local(&root, 1, &members(), &cluster, &config)
            .unwrap();
        let source = binding
            .audit_source("LIB-B", cluster.clone(), config.clone(), reader.clone())
            .unwrap();
        Self {
            root,
            cluster,
            config,
            binding,
            reader,
            source,
        }
    }
    fn files(&self) -> Arc<FileService> {
        FileService::control_only(Some(self.source.clone()))
    }
    fn context(&self, files: Arc<FileService>) -> Arc<HttpContext> {
        let (exec, _) = std::sync::mpsc::channel();
        let (node, _) = std::sync::mpsc::channel();
        Arc::new(HttpContext {
            files,
            status: Arc::new(Mutex::new(NodeStatus {
                id: 1,
                term: 2,
                leader: 1,
                role: "Leader".into(),
                applied_index: 72616,
                catalog_quiesce_index: Some(2058),
                catalog_frozen_index: Some(2745),
                ..Default::default()
            })),
            exec: Mutex::new(exec),
            node: Mutex::new(node),
            client_addrs: members(),
            wait_timeout: Duration::from_secs(1),
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn members() -> HashMap<u64, String> {
    HashMap::from([
        (1, "127.0.0.1:8011".into()),
        (2, "127.0.0.1:8012".into()),
        (3, "127.0.0.1:8013".into()),
    ])
}

#[test]
fn control_only_audit_is_bound_readonly_and_revalidates_authority() {
    let f = Fixture::new();
    let before = FileSeal::read(&f.root.join("directory.db")).unwrap();
    for _ in 0..3 {
        let files = f.files();
        let a = files.catalog_cutover_audit().unwrap();
        assert_eq!(a.applied_index, 7133);
        assert_eq!(a.frozen_index, Some(2745));
        drop(files);
    }
    assert_eq!(
        before,
        FileSeal::read(&f.root.join("directory.db")).unwrap()
    );
    check_no_journal(&f.root.join("directory.db")).unwrap();
    for fault in 0..8 {
        let original = f.reader.0.lock().unwrap().clone();
        match fault {
            0 => *f.reader.0.lock().unwrap() = Err("FDB unavailable".into()),
            1 => *f.reader.0.lock().unwrap() = Ok(None),
            _ => {
                let mut r = original.clone().unwrap().unwrap();
                match fault {
                    2 => r.namespace = uuid::Uuid::new_v4().to_string(),
                    3 => r.revision += 1,
                    4 => r.frozen_index += 1,
                    5 => r.certificate_sha256 = "ef".repeat(32),
                    _ => r.initial_owner_epoch += 1,
                };
                *f.reader.0.lock().unwrap() = Ok(Some(r));
            }
        }
        assert!(f.files().catalog_cutover_audit().is_err(), "fault {fault}");
        *f.reader.0.lock().unwrap() = original;
    }
    std::fs::write(&f.cluster, b"other cluster").unwrap();
    assert!(f.files().catalog_cutover_audit().is_err());
}
#[test]
fn control_only_source_replacement_missing_symlink_and_wal_fail_closed() {
    for fault in 0..5 {
        let f = Fixture::new();
        let path = f.root.join("directory.db");
        let original = std::fs::read(&path).unwrap();
        match fault {
            0 => {
                std::fs::remove_file(&path).unwrap();
            }
            1 => {
                std::fs::remove_file(&path).unwrap();
                std::fs::write(&path, &original).unwrap();
            }
            2 => {
                std::fs::rename(&path, f.root.join("other.db")).unwrap();
                std::os::unix::fs::symlink(f.root.join("other.db"), &path).unwrap();
            }
            3 => {
                std::fs::write(f.root.join("directory.db-wal"), b"unknown").unwrap();
            }
            _ => {
                std::fs::write(&path, b"bad sqlite").unwrap();
            }
        }
        assert!(f.files().catalog_cutover_audit().is_err(), "fault {fault}");
        if fault == 0 {
            assert!(!path.exists());
        }
    }
}
#[test]
fn control_only_direct_business_calls_and_upgrade_are_denied() {
    let f = Fixture::new();
    let files = f.files();
    let index = crate::ltfs::index::LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
    files.open(
        36011,
        TapeIdent {
            pool_uuid: "p".into(),
            barcode: "B".into(),
        },
        TapeLimits {
            file_limit: 100,
            usable_capacity: 1024,
        },
        &index,
        1024,
        true,
    );
    files.close_no_tape(36011, Some("p".into()), "test");
    let permission = Arc::new(super::super::permission::Permission::default());
    permission.renew(
        36011,
        super::super::permission::Phase::Serving,
        std::time::Instant::now() + Duration::from_secs(30),
    );
    files.set_permission(permission);
    assert!(files.serving_deadline(36011).is_none());
    let lane = files.add_lane("p");
    lane.open(
        36011,
        TapeIdent {
            pool_uuid: "p".into(),
            barcode: "B".into(),
        },
        TapeLimits {
            file_limit: 100,
            usable_capacity: 1024,
        },
        &index,
        1024,
        true,
    );
    assert!(lane.is_control_only());
    assert!(lane.serving_round().is_none());
    assert!(lane.list().is_err());

    files.set_catalog_quiesced(false);
    assert!(files.serving_round().is_none());
    assert!(files.read_spool().is_err());
    assert!(files.catalog_export_frozen_snapshot(2745).is_err());
    assert!(matches!(
        files.begin("/x", 1),
        Err(ServiceError::NotServing(_))
    ));
    assert!(files.stat("/x").is_err());
    assert!(files.lookup("/x").is_err());
    assert!(files.stat_full("/x").is_err());
    assert!(files.list().is_err());
    assert!(files.list_dir("/", true).is_err());
    assert!(files.delete("/x").is_err());
}
#[test]
fn control_only_executor_never_reports_fenced_serving_or_business_success() {
    let (tx, rx) = std::sync::mpsc::channel();
    let (events, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || run_deny(rx, events));
    tx.send(ExecRequest::Takeover { round: 36011 }).unwrap();
    tx.send(ExecRequest::Recover { round: 36011 }).unwrap();
    tx.send(ExecRequest::Stop {
        round: None,
        reason: "loss".into(),
    })
    .unwrap();
    let (reply, result) = std::sync::mpsc::channel();
    tx.send(ExecRequest::Read {
        path: "/x".into(),
        reply,
    })
    .unwrap();
    assert!(
        result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err()
    );
    let (done, stopped) = std::sync::mpsc::channel();
    tx.send(ExecRequest::Shutdown { done: Some(done) }).unwrap();
    stopped.recv_timeout(Duration::from_secs(1)).unwrap();
    worker.join().unwrap();
    let events: Vec<_> = received.try_iter().collect();
    assert!(events.contains(&ExecEvent::Capability {
        drives: 0,
        eligible: false
    }));
    assert!(events.iter().all(|event| matches!(
        event,
        ExecEvent::Capability {
            eligible: false,
            drives: 0
        } | ExecEvent::FenceFailed { .. }
            | ExecEvent::Lost { .. }
    )));
}
#[test]
fn control_only_http_is_a_local_allowlist_and_barrier_uses_real_actor() {
    let f = Fixture::new();
    let files = f.files();
    let ctx = f.context(files);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let app = super::super::gateway::control_only_router(
        f.config.clone(),
        BTreeMap::from([("LIB-B".into(), ctx.clone())]),
        f.context(FileService::control_only(None)),
        members(),
    );
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = |method: &str, path: &str| {
        use std::io::{Read, Write};
        let mut conn = std::net::TcpStream::connect(address).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        write!(conn,"{method} {path} HTTP/1.1\r\nHost: local\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let mut bytes = Vec::new();
        conn.read_to_end(&mut bytes).unwrap();
        String::from_utf8(bytes)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap()
    };
    for path in [
        "/files/x",
        "/list",
        "/stat/x",
        "/pools/p/list",
        "/tasks/1",
        "/libraries/LIB-B/list",
        "/libraries/LIB-B/pools/p/list",
        "/libraries/LIB-B/admin/catalog/export",
        "/libraries/LIB-B/admin/catalog/freeze",
        "/admin/libraries/LIB-B/rescan",
        "/libraries/LIB%2DB/admin/catalog/audit",
        "/libraries/LIB-B//admin/catalog/audit",
        "/unknown",
    ] {
        for method in ["GET", "POST", "PATCH", "DELETE"] {
            assert_eq!(request(method, path), 403, "{method} {path}");
        }
    }
    for path in [
        "/cluster",
        "/admin/libraries",
        "/libraries/LIB-B/admin/catalog/current",
        "/libraries/LIB-B/admin/catalog/audit",
    ] {
        assert_eq!(request("GET", path), 200, "{path}");
    }
    let barrier = format!(
        "/libraries/LIB-B/admin/catalog/native-barrier?challenge={}",
        uuid::Uuid::new_v4()
    );
    ctx.status.lock().unwrap().executor = Some(super::super::state::Executor {
        node: 1,
        term: 2,
        round: 36011,
        fenced: true,
    });
    ctx.status.lock().unwrap().executor_ready = true;
    assert_ne!(request("GET", &barrier), 200);
    ctx.status.lock().unwrap().executor = None;
    ctx.status.lock().unwrap().executor_ready = false;
    assert_eq!(request("GET", &barrier), 200);
    *f.reader.0.lock().unwrap() = Ok(None);
    assert_eq!(request("GET", &barrier), 503);
    server.abort();
}

#[test]
fn control_only_three_nodes_commit_abandoned_on_original_chain_without_business_io() {
    use super::super::{
        multilib::{self, GATEWAY, LibraryRuntime},
        net::{Network, NodeInput},
        node::{CoordinationConfig, NodeConfig},
        state::{Candidate, ControlSnapshot, ControlState, Executor},
        store::{RaftStore, SnapshotPolicy},
    };
    use raft::eraftpb::{ConfState, HardState, Snapshot};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Mesh {
        inboxes: Arc<HashMap<u64, Sender<NodeInput>>>,
        isolated: Arc<AtomicBool>,
    }
    impl Network for Mesh {
        fn send(&self, msg: raft::eraftpb::Message) {
            if self.isolated.load(Ordering::SeqCst) && (msg.from == 3 || msg.to == 3) {
                return;
            }
            let _ = self.inboxes[&msg.to].send(NodeInput::Raft(Box::new(msg)));
        }
    }
    let fixtures: Vec<_> = (1..=3).map(|_| Fixture::new()).collect();
    let mut inboxes = HashMap::new();
    let mut receivers = Vec::new();
    for id in 1..=3 {
        let (tx, rx) = std::sync::mpsc::channel();
        inboxes.insert(id, tx);
        receivers.push((id, rx));
    }
    let inboxes = Arc::new(inboxes);
    let isolated = Arc::new(AtomicBool::new(true));
    let mut nodes = Vec::new();
    let mut contexts = Vec::new();
    let mut executors = Vec::new();
    let group = fixtures[0].binding.group.clone();
    let mut source_hashes = Vec::new();
    let mut startup_bindings = Vec::new();
    for ((id, rx), f) in receivers.into_iter().zip(&fixtures) {
        let path = f.root.join("raft.db");
        std::fs::remove_file(&path).unwrap();
        let mut store = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let mut control = ControlState {
            catalog_quiesce_index: Some(2058),
            catalog_frozen_index: Some(2745),
            executor: Some(Executor {
                node: 1,
                term: 2,
                round: 36011,
                fenced: true,
            }),
            executor_instance: 1,
            executor_ready: true,
            device_activity: Some(super::super::permission::DeviceActivityState {
                node: 1,
                instance: 1,
                round: 36011,
                sequence: 7,
                busy: true,
            }),
            applied_index: 72616,
            ..Default::default()
        };
        control.candidates.insert(
            1,
            Candidate {
                instance: 1,
                sequence: 100,
                group: format!("{group}:LIB-B"),
                drives: 2,
                eligible: true,
            },
        );
        let mut gateway_control = control.clone();
        gateway_control.candidates.get_mut(&1).unwrap().group = format!("{group}:gateway");
        let encode = |control| {
            serde_json::from_slice::<serde_json::Value>(
                &ControlSnapshot {
                    control,
                    source: id,
                    directory_index: 71998,
                }
                .encode(),
            )
            .unwrap()
        };
        let data=serde_json::to_vec(&serde_json::json!({"cluster_version":2,"default_library":"LIB-B","source":id,"directory_index":71998,"controls":{"LIB-B":encode(control),GATEWAY:encode(gateway_control)}})).unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.mut_metadata().index = 72616;
        snapshot.mut_metadata().term = 2;
        snapshot
            .mut_metadata()
            .set_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
        snapshot.set_data(data.into());
        store.install_snapshot(&snapshot).unwrap();
        let mut hs = HardState::default();
        hs.term = 2;
        hs.commit = 72616;
        store.set_hardstate(&hs).unwrap();
        for _ in 0..4 {
            store.next_instance().unwrap();
        }
        drop(store);
        let store = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let task = f.root.join("original-task-evidence");
        std::fs::write(&task, b"original nonterminal FDB task bytes; no mutation").unwrap();
        let spool = f.root.join("spool");
        std::fs::create_dir(&spool).unwrap();
        std::fs::write(spool.join("original"), b"original spool body").unwrap();
        source_hashes.push(FileSeal::read(&f.root.join("directory.db")).unwrap());
        let mut binding = (*f.binding).clone();
        binding.node = id;
        binding.raft = FileSeal::read(&path).unwrap();
        super::binding_fixture::seal(&mut binding, &f.config);
        let binding = Arc::new(binding);
        startup_bindings.push(binding.clone());
        let source = binding
            .audit_source(
                "LIB-B",
                f.cluster.clone(),
                f.config.clone(),
                f.reader.clone(),
            )
            .unwrap();
        let files = FileService::control_only(Some(source));
        let mut runtimes = Vec::new();
        for name in ["LIB-B", GATEWAY] {
            let files = if name == "LIB-B" {
                files.clone()
            } else {
                FileService::control_only(None)
            };
            let status = Arc::new(Mutex::new(NodeStatus::default()));
            let (exec, requests) = std::sync::mpsc::channel();
            let (events, event_rx) = std::sync::mpsc::channel();
            executors.push(std::thread::spawn(move || run_deny(requests, events)));
            let scoped = multilib::scoped_sender(name.into(), inboxes[&id].clone());
            let forward = scoped.clone();
            std::thread::spawn(move || {
                for event in event_rx {
                    if forward.send(NodeInput::Exec(event)).is_err() {
                        break;
                    }
                }
            });
            if name == "LIB-B" {
                contexts.push(Arc::new(HttpContext {
                    files: files.clone(),
                    status: status.clone(),
                    exec: Mutex::new(exec.clone()),
                    node: Mutex::new(scoped),
                    client_addrs: members(),
                    wait_timeout: Duration::from_secs(1),
                }));
            }
            runtimes.push(LibraryRuntime {
                id: name.into(),
                group: format!("{group}:{}", if name == GATEWAY { "gateway" } else { name }),
                drives: if name == GATEWAY { 0 } else { 2 },
                directory_file: None,
                exec,
                status,
                files,
            });
        }
        let cfg = NodeConfig {
            id,
            peers: vec![1, 2, 3],
            tick: Duration::from_millis(20),
            election_ticks: 10,
            heartbeat_ticks: 1,
            status_file: None,
            directory_file: None,
            cooldown: Duration::from_millis(200),
            shutdown_grace: Duration::from_secs(2),
            drives: 0,
            snapshot: SnapshotPolicy::default(),
            coordination: Some(CoordinationConfig {
                group: group.clone(),
                validity: Duration::from_secs(1),
            }),
        };
        let config = f.config.clone();
        let mesh = Mesh {
            inboxes: inboxes.clone(),
            isolated: isolated.clone(),
        };
        let tx = inboxes[&id].clone();
        nodes.push(std::thread::spawn(move || {
            multilib::run(cfg, config, store, Box::new(mesh), None, tx, rx, runtimes)
        }));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if contexts[..2].iter().all(|c| {
            let s = c.status.lock().unwrap();
            s.applied_index > 72616 && s.executor.is_none() && !s.executor_ready
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "majority never committed Abandoned"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    while contexts[2].status.lock().unwrap().applied_index < 72616 {
        assert!(
            std::time::Instant::now() < deadline,
            "isolated node did not publish initial snapshot status: {:?}, worker_finished={}",
            *contexts[2].status.lock().unwrap(),
            nodes[2].is_finished()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        contexts[2].status.lock().unwrap().executor.is_some(),
        "isolated node must retain original actor"
    );
    isolated.store(false, Ordering::SeqCst);
    loop {
        if contexts.iter().all(|c| {
            let s = c.status.lock().unwrap();
            s.applied_index > 72616 && s.executor.is_none() && !s.executor_ready && s.leader > 0
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "third node did not apply original Abandoned"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    loop {
        let codes: Vec<_> = contexts
            .iter()
            .map(|ctx| {
                runtime.block_on(super::super::gateway::native_barrier_test_adapter(
                    "LIB-B",
                    &members(),
                    ctx.clone(),
                ))
            })
            .collect();
        if codes == vec![200, 200, 200] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "barrier remained unavailable: {codes:?}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    for tx in inboxes.values() {
        tx.send(NodeInput::Shutdown).unwrap();
    }
    for node in nodes {
        node.join().unwrap().unwrap();
    }
    for executor in executors {
        executor.join().unwrap();
    }
    // 全部停机后，原 data-root 与同一不可变 binding 再启动三节点。
    let mut receivers = Vec::new();
    let mut senders = HashMap::new();
    for id in 1..=3 {
        let (tx, rx) = std::sync::mpsc::channel();
        senders.insert(id, tx);
        receivers.push((id, rx));
    }
    let inboxes = Arc::new(senders);
    let isolated = Arc::new(AtomicBool::new(false));
    let mut contexts = Vec::new();
    let mut executors = Vec::new();
    let mut nodes = Vec::new();
    for ((id, rx), (f, binding)) in receivers
        .into_iter()
        .zip(fixtures.iter().zip(&startup_bindings))
    {
        assert_ne!(
            FileSeal::read(&f.root.join("raft.db")).unwrap().sha256,
            binding.raft.sha256,
            "normal control commits must change raft bytes"
        );
        binding
            .validate_local(&f.root, id, &members(), &f.cluster, &f.config)
            .expect("same immutable binding must survive legal control commits");
        let store = RaftStore::open(&f.root.join("raft.db"), &[1, 2, 3]).unwrap();
        let source = binding
            .audit_source(
                "LIB-B",
                f.cluster.clone(),
                f.config.clone(),
                f.reader.clone(),
            )
            .unwrap();
        let files = FileService::control_only(Some(source));
        let mut runtimes = Vec::new();
        for name in ["LIB-B", GATEWAY] {
            let files = if name == "LIB-B" {
                files.clone()
            } else {
                FileService::control_only(None)
            };
            let status = Arc::new(Mutex::new(NodeStatus::default()));
            let (exec, requests) = std::sync::mpsc::channel();
            let (events, event_rx) = std::sync::mpsc::channel();
            executors.push(std::thread::spawn(move || run_deny(requests, events)));
            let scoped = multilib::scoped_sender(name.into(), inboxes[&id].clone());
            let forward = scoped.clone();
            std::thread::spawn(move || {
                for event in event_rx {
                    if forward.send(NodeInput::Exec(event)).is_err() {
                        break;
                    }
                }
            });
            if name == "LIB-B" {
                contexts.push(Arc::new(HttpContext {
                    files: files.clone(),
                    status: status.clone(),
                    exec: Mutex::new(exec.clone()),
                    node: Mutex::new(scoped),
                    client_addrs: members(),
                    wait_timeout: Duration::from_secs(1),
                }));
            }
            runtimes.push(LibraryRuntime {
                id: name.into(),
                group: format!("{group}:{}", if name == GATEWAY { "gateway" } else { name }),
                drives: if name == GATEWAY { 0 } else { 2 },
                directory_file: None,
                exec,
                status,
                files,
            });
        }
        let cfg = NodeConfig {
            id,
            peers: vec![1, 2, 3],
            tick: Duration::from_millis(20),
            election_ticks: 10,
            heartbeat_ticks: 1,
            status_file: None,
            directory_file: None,
            cooldown: Duration::from_millis(200),
            shutdown_grace: Duration::from_secs(2),
            drives: 0,
            snapshot: SnapshotPolicy::default(),
            coordination: Some(CoordinationConfig {
                group: group.clone(),
                validity: Duration::from_secs(1),
            }),
        };
        let config = f.config.clone();
        let mesh = Mesh {
            inboxes: inboxes.clone(),
            isolated: isolated.clone(),
        };
        let tx = inboxes[&id].clone();
        nodes.push(std::thread::spawn(move || {
            multilib::run(cfg, config, store, Box::new(mesh), None, tx, rx, runtimes)
        }));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        let mut ready = true;
        for ctx in &contexts {
            let status = ctx.status.lock().unwrap().clone();
            if status.executor.is_some()
                || status.executor_ready
                || status.applied_index <= 72616
                || status.leader == 0
            {
                ready = false;
                continue;
            }
            let code = runtime.block_on(super::super::gateway::native_barrier_test_adapter(
                "LIB-B",
                &members(),
                ctx.clone(),
            ));
            if code != 200 {
                ready = false;
            }
        }
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "same-binding restarted barrier did not pass"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    for tx in inboxes.values() {
        tx.send(NodeInput::Shutdown).unwrap();
    }
    for node in nodes {
        node.join().unwrap().unwrap();
    }
    for executor in executors {
        executor.join().unwrap();
    }
    for (index, (f, binding)) in fixtures.iter().zip(&startup_bindings).enumerate() {
        binding
            .validate_local(&f.root, index as u64 + 1, &members(), &f.cluster, &f.config)
            .expect("normal Candidate/Abandoned must allow the same immutable binding on restart");
    }
    for (f, original) in fixtures.iter().zip(source_hashes) {
        assert_eq!(
            original,
            FileSeal::read(&f.root.join("directory.db")).unwrap()
        );
        check_no_journal(&f.root.join("directory.db")).unwrap();
        assert_eq!(
            std::fs::read(f.root.join("spool/original")).unwrap(),
            b"original spool body"
        );
        assert_eq!(
            std::fs::read(f.root.join("original-task-evidence")).unwrap(),
            b"original nonterminal FDB task bytes; no mutation"
        );
    }
}

#[test]
fn control_only_rejects_nonempty_or_wrong_watermark_even_with_a_matching_source_seal() {
    for sql in [
        "INSERT INTO pools(uuid,name,file_limit) VALUES ('p','pool',10)",
        "UPDATE meta SET v=2744 WHERE k='applied_index'",
        "UPDATE meta SET v=2746 WHERE k='catalog_frozen_index'",
    ] {
        let f = Fixture::new();
        let path = f.root.join("directory.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(sql).unwrap();
        drop(db);
        let mut binding = (*f.binding).clone();
        binding.libraries.get_mut("LIB-B").unwrap().directory = FileSeal::read(&path).unwrap();
        let source = Arc::new(binding)
            .audit_source(
                "LIB-B",
                f.cluster.clone(),
                f.config.clone(),
                f.reader.clone(),
            )
            .unwrap();
        assert!(source.audit().is_err(), "{sql}");
    }
}

#[test]
fn control_only_binding_rejects_wrong_source_identity_members_cluster_and_absent_reset() {
    let f = Fixture::new();
    for fault in 0..8 {
        let mut binding = (*f.binding).clone();
        match fault {
            0 => binding.node = 2,
            1 => {
                binding.members.remove(&3);
            }
            2 => binding.cluster_sha256 = "ee".repeat(32),
            3 => binding.libraries_sha256 = "ee".repeat(32),
            4 => binding.data_root_inode += 1,
            5 => binding.raft.sha256 = "ee".repeat(32),
            6 => {
                binding.libraries.get_mut("LIB-B").unwrap().catalog =
                    CatalogExpectation::AbsentAfterReset {}
            }
            _ => {
                let library = binding.libraries.remove("LIB-B").unwrap();
                binding.libraries.insert("OTHER".into(), library);
            }
        }
        assert!(
            binding
                .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
                .is_err(),
            "fault {fault}"
        );
    }
}

#[test]
fn control_only_arbitrary_lineage_seal_is_rejected() {
    let f = Fixture::new();
    let mut binding = (*f.binding).clone();
    binding.original_chain_sha256 = "ee".repeat(32);
    assert!(
        binding
            .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
            .is_err(),
        "unbacked arbitrary original-chain seal was accepted"
    );
}

#[test]
fn control_only_lineage_reads_committed_wal_and_refuses_stale_status() {
    use super::super::{state::Command, store::RaftStore};
    let f = Fixture::new();
    let mut writer = RaftStore::open(&f.root.join("raft.db"), &[1, 2, 3]).unwrap();
    let mut entry = raft::eraftpb::Entry::default();
    entry.index = 72617;
    entry.term = 2;
    entry.set_data(
        super::super::multilib::encode_command(
            "LIB-B",
            &Command::Candidate {
                node: 1,
                instance: 5,
                sequence: 1,
                group: format!("{}:LIB-B", f.binding.group),
                drives: 0,
                eligible: false,
            },
        )
        .into(),
    );
    writer.append(&[entry]).unwrap();
    writer.set_commit(72617).unwrap();
    assert!(std::fs::metadata(f.root.join("raft.db-wal")).unwrap().len() > 0);
    let verified = super::lineage::verify(&f.binding, &f.config).unwrap();
    assert_eq!(verified.controls["LIB-B"].applied_index, 72617);
    let ctx = f.context(f.files());
    let stale = ctx.status.lock().unwrap().clone();
    assert!(ctx.files.verify_control_only_status(&stale).is_err());
    let mut current = stale;
    current.applied_index = 72617;
    current.candidates = verified.controls["LIB-B"].candidates.clone();
    ctx.files.verify_control_only_status(&current).unwrap();
    drop(writer);
    f.binding
        .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
        .unwrap();
}

#[test]
fn control_only_lineage_rejects_wrong_suffix_and_snapshot_replacement() {
    use super::super::{state::Command, store::RaftStore};
    for fault in 0..7 {
        let f = Fixture::new();
        let mut writer = RaftStore::open(&f.root.join("raft.db"), &[1, 2, 3]).unwrap();
        let command = match fault {
            0 => Command::Abandoned {
                node: 1,
                round: 36012,
                reason: "wrong original round".into(),
            },
            1 => Command::Candidate {
                node: 4,
                instance: 5,
                sequence: 1,
                group: format!("{}:LIB-B", f.binding.group),
                drives: 0,
                eligible: false,
            },
            2 => Command::Candidate {
                node: 1,
                instance: 5,
                sequence: 1,
                group: "foreign".into(),
                drives: 0,
                eligible: false,
            },
            _ => Command::Candidate {
                node: 1,
                instance: 5,
                sequence: 1,
                group: format!("{}:LIB-B", f.binding.group),
                drives: if fault == 3 { 2 } else { 0 },
                eligible: fault == 3,
            },
        };
        let mut entry = raft::eraftpb::Entry::default();
        entry.index = 72617;
        entry.term = 2;
        entry.set_data(
            super::super::multilib::encode_command(
                if fault == 4 { "OTHER" } else { "LIB-B" },
                &command,
            )
            .into(),
        );
        writer.append(&[entry]).unwrap();
        writer.set_commit(72617).unwrap();
        drop(writer);
        if fault >= 5 {
            let db = rusqlite::Connection::open(f.root.join("raft.db")).unwrap();
            if fault == 5 {
                db.execute("UPDATE entries SET idx=72618 WHERE idx=72617", [])
                    .unwrap();
            } else {
                db.execute(
                    "UPDATE snapshot SET data=?1 WHERE id=1",
                    [b"wrong original snapshot".as_slice()],
                )
                .unwrap();
            }
        }
        assert!(
            f.binding
                .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
                .is_err(),
            "fault {fault}"
        );
    }
}

#[test]
fn control_only_connection_uses_verified_bytes_when_original_path_is_swapped_and_restored() {
    let f = Fixture::new();
    let expected = std::fs::read(&f.cluster).unwrap();
    let result = super::open_bound_catalog(&f.cluster, &f.binding.cluster_sha256, |sdk_input| {
        let saved = f.cluster.with_extension("original");
        std::fs::rename(&f.cluster, &saved)?;
        std::fs::write(&f.cluster, b"wrong cluster:identity@127.0.0.1:2")?;
        let read = std::fs::read(sdk_input);
        std::fs::remove_file(&f.cluster)?;
        std::fs::rename(saved, &f.cluster)?;
        read.map_err(Into::into)
    })
    .unwrap();
    assert_eq!(
        *result.catalog, expected,
        "SDK factory observed cluster bytes that were not verified"
    );
}

#[test]
fn control_only_pinned_cluster_has_all_seals_and_catalog_lifetime() {
    use std::io::Write;
    let f = Fixture::new();
    let bytes = std::fs::read(&f.cluster).unwrap();
    let connection = super::open_bound_catalog(&f.cluster, &f.binding.cluster_sha256, |input| {
        Ok(input.to_path_buf())
    })
    .unwrap();
    connection.cluster.verify_sealed().unwrap();
    let mut writer = connection.cluster.file.try_clone().unwrap();
    assert!(writer.write_all(b"changed").is_err());
    assert!(writer.set_len(1).is_err());
    assert!(writer.set_len(bytes.len() as u64 + 1).is_err());
    let pinned = connection.cluster.clone();
    let sdk_path = (*connection.catalog).clone();
    drop(connection);
    drop(f);
    assert_eq!(std::fs::read(&sdk_path).unwrap(), bytes);
    pinned.verify_sealed().unwrap();
    assert!(pinned.verify_source().is_err());
}

#[test]
fn control_only_seal_failure_stops_before_catalog_factory() {
    let f = Fixture::new();
    for fail in [true, false] {
        let mut calls = 0;
        let result = (|| {
            let pinned =
                super::PinnedCluster::with_sealer(&f.cluster, &f.binding.cluster_sha256, |_| {
                    if fail {
                        Err(std::io::Error::other("injected seal failure"))
                    } else {
                        Ok(())
                    }
                })?;
            super::open_catalog_from_pin(Arc::new(pinned), |_| {
                calls += 1;
                Ok(())
            })
        })();
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }
}

#[test]
fn control_only_lineage_rejects_changed_committed_prefix_and_unbridged_compaction() {
    use super::super::{state::ControlSnapshot, store::RaftStore};
    use protobuf::Message as _;
    for compact in [false, true] {
        let f = Fixture::new();
        let path = f.root.join("raft.db");
        let mut writer = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let mut entry = raft::eraftpb::Entry::default();
        entry.index = 72617;
        entry.term = 2;
        writer.append(&[entry.clone()]).unwrap();
        writer.set_commit(72617).unwrap();
        if compact {
            let states = super::lineage::verify(&f.binding, &f.config)
                .unwrap()
                .controls;
            let controls: serde_json::Map<String, serde_json::Value> = states
                .into_iter()
                .map(|(id, control)| {
                    (
                        id,
                        serde_json::from_slice(
                            &ControlSnapshot {
                                control,
                                source: 1,
                                directory_index: 72617,
                            }
                            .encode(),
                        )
                        .unwrap(),
                    )
                })
                .collect();
            let data=serde_json::to_vec(&serde_json::json!({"cluster_version":2,"default_library":"LIB-B","source":1,"directory_index":72617,"controls":controls})).unwrap();
            let snapshot = writer.build_snapshot(72617, data).unwrap();
            writer.compact_to(&snapshot).unwrap();
            drop(writer);
            assert!(
                f.binding
                    .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
                    .is_err()
            );
        } else {
            drop(writer);
            let mut binding = (*f.binding).clone();
            super::binding_fixture::seal(&mut binding, &f.config);
            binding
                .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
                .unwrap();
            entry.set_context(b"tampered original committed entry".to_vec().into());
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute(
                "UPDATE entries SET data=?1 WHERE idx=72617",
                [entry.write_to_bytes().unwrap()],
            )
            .unwrap();
            drop(db);
            assert!(
                binding
                    .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
                    .is_err()
            );
        }
    }
}

#[test]
fn control_only_lineage_rejects_local_instance_regression() {
    let f = Fixture::new();
    let mut writer =
        super::super::store::RaftStore::open(&f.root.join("raft.db"), &[1, 2, 3]).unwrap();
    for _ in 0..4 {
        writer.next_instance().unwrap();
    }
    drop(writer);
    let mut binding = (*f.binding).clone();
    super::binding_fixture::seal(&mut binding, &f.config);
    let db = rusqlite::Connection::open(f.root.join("raft.db")).unwrap();
    db.execute("UPDATE local_instance SET value=0 WHERE id=1", [])
        .unwrap();
    drop(db);
    assert!(
        binding
            .validate_local(&f.root, 1, &members(), &f.cluster, &f.config)
            .is_err()
    );
}
