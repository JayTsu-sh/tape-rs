#![cfg(feature = "fdb")]

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, channel};
use std::time::{Duration, Instant};

use raft::eraftpb::{ConfState, Message, Snapshot};
use serde_json::json;
use sha2::{Digest, Sha256};
use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use tape_rs::daemon::executor::{
    DeviceKind, DeviceProvider, ExecEvent, ExecOptions, ExecRequest, FdbOwnerControl,
    ManagedDevice, ReadError,
};
use tape_rs::daemon::fdb_media::{
    ActiveMediaUpload, commit_active_media_batch, establish_active_media_owner,
    publish_active_media_batch, reconcile_active_media_on_takeover, recover_active_media_tail,
    recover_sealed_active_media_batch, resume_observed_active_media_batch,
};
use tape_rs::daemon::fdb_read::ActiveMediaReader;
use tape_rs::daemon::files::FileService;
use tape_rs::daemon::multilib::{self, GATEWAY, LibrariesConfig, LibraryRuntime};
use tape_rs::daemon::net::{Network, NodeInput};
use tape_rs::daemon::node::{CoordinationConfig, NodeConfig, NodeStatus, SharedStatus};
use tape_rs::daemon::permission::{GuardedTransport, Permission, Phase};
use tape_rs::daemon::state::{ControlSnapshot, ControlState};
use tape_rs::daemon::store::{RaftStore, SnapshotPolicy};
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::{RecoveryBudget, TailKind};
use tape_rs::ltfs::volume::{LtfsVolume, TailPolicy};
use tape_rs::managed_catalog::{
    ActiveBatchOutcome, ActiveBatchPhase, ActiveFileIntent, ActiveFinalizationOutcome,
    ActiveIndexIdentity, ActiveLibraryRoute, ActiveMaterializationOutcome, ActiveOwnerOutcome,
    ActivePreparationOutcome, ActivePublicationOutcome, CatalogError, ContentDigest,
    ControlOutcome, ImportSourceBinding, ImportSpool, ImportVolume, LocalVolumeSources,
    ManagedCatalog, NamespaceVerificationOutcome, PoolPolicy, PoolRecord, PrepareRouteOutcome,
    PublishOutcome, ResolvedPath, ScanDigest, SourceOutcome, VerificationOutcome, VolumeOutcome,
    VolumePlacement, VolumeScan, inspect_frozen_snapshot, prepare_active_file, verify_node_export,
};
use tape_rs::scsi::reservation::{ReservationKey, fence};
use tape_rs::scsi::sim::{LogicalObject, SimCartridge, SimLibrary};
use tape_rs::scsi::transport::TapeTransport;
use tape_rs::tape::commands::TapeDrive;
use uuid::Uuid;

struct FdbOwnerSimProvider {
    library: SimLibrary,
    node: u8,
    enabled: Arc<AtomicBool>,
}

struct RevokeOnContent {
    inner: Box<dyn TapeTransport + Send>,
    permission: Arc<Permission>,
    content: Vec<u8>,
    round: u64,
}

impl TapeTransport for RevokeOnContent {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        buf: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        let result = self.inner.execute_read(cdb, buf, timeout)?;
        if cdb.first() == Some(&0x08) && buf[..result.transferred].starts_with(&self.content) {
            self.permission.close(self.round);
        }
        Ok(result)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        buf: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_write(cdb, buf, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

impl DeviceProvider for FdbOwnerSimProvider {
    fn open_all(&self) -> tape_rs::error::Result<Vec<ManagedDevice>> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(tape_rs::error::TapeError::NotReady("模拟 FC 已断开".into()));
        }
        Ok(vec![
            ManagedDevice {
                name: "模拟驱动器".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: Box::new(self.library.drive_as(0, self.node as u32)),
            },
            ManagedDevice {
                name: "模拟换带器".into(),
                serial: "SIMLIB0000000001".into(),
                kind: DeviceKind::Changer,
                dev: Box::new(self.library.changer_as(self.node as u32)),
            },
        ])
    }
}

fn next_owner_event(rx: &Receiver<ExecEvent>) -> ExecEvent {
    loop {
        let event = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if !matches!(event, ExecEvent::Capability { .. }) {
            return event;
        }
    }
}

struct OwnerRaftNet(Arc<HashMap<u64, mpsc::Sender<NodeInput>>>);

impl Network for OwnerRaftNet {
    fn send(&self, message: Message) {
        if let Some(tx) = self.0.get(&message.to) {
            let _ = tx.send(NodeInput::Raft(Box::new(message)));
        }
    }
}

async fn run_fdb_owner_raft_failover(
    cluster: &str,
    root: &std::path::Path,
    serial: &str,
    route: tape_rs::managed_catalog::ActiveLibraryRoute,
    library: &SimLibrary,
    catalog: &ManagedCatalog,
) {
    use tape_rs::daemon::executor::{self, FdbOwnerControl};
    use tape_rs::managed_catalog::ActiveOwnerOutcome;
    use tape_rs::scsi::reservation::verify_holder;

    let legacy_status = Arc::new(Mutex::new(NodeStatus::default()));
    legacy_status.lock().unwrap().catalog_frozen_index = Some(route.frozen_index);
    let legacy_files = FileService::new(root.join("frozen-legacy-spool")).unwrap();
    let (legacy_tx, legacy_rx) = mpsc::channel();
    let (legacy_events, legacy_event_rx) = mpsc::channel();
    let legacy = std::thread::spawn({
        let library = library.clone();
        move || {
            executor::run(
                Box::new(FdbOwnerSimProvider {
                    library,
                    node: 79,
                    enabled: Arc::new(AtomicBool::new(true)),
                }),
                ExecOptions {
                    node_id: 79,
                    interval: Duration::from_millis(50),
                    demo_write: false,
                    salvage: false,
                    block_size: 65536,
                    read_idle: Duration::from_secs(1),
                },
                legacy_rx,
                legacy_events,
                legacy_files,
                legacy_status,
            );
        }
    });
    legacy_tx.send(ExecRequest::Takeover { round: 19 }).unwrap();
    assert!(matches!(
        next_owner_event(&legacy_event_rx),
        ExecEvent::FenceFailed { round: 19, .. }
    ));
    let (done_tx, done_rx) = mpsc::channel();
    legacy_tx
        .send(ExecRequest::Shutdown {
            done: Some(done_tx),
        })
        .unwrap();
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    legacy.join().unwrap();
    let prior_key = ReservationKey::new(78, 18);
    verify_holder(&library.drive(0), prior_key).unwrap();

    let config = LibrariesConfig::parse(&json!({
        "default_library": serial,
        "libraries": [{"changer_serial": serial, "drive_serials": ["SIMDRV0000"]}]
    }))
    .unwrap();
    let frozen = route.frozen_index;
    let library_control = ControlState {
        applied_index: frozen,
        catalog_quiesce_index: Some(frozen - 1),
        catalog_frozen_index: Some(frozen),
        ..Default::default()
    };
    let gateway_control = ControlState {
        applied_index: frozen,
        ..Default::default()
    };
    let encode = |control| {
        serde_json::from_slice::<serde_json::Value>(
            &ControlSnapshot {
                control,
                source: 71,
                directory_index: 0,
            }
            .encode(),
        )
        .unwrap()
    };
    let snapshot_data = serde_json::to_vec(&json!({
        "cluster_version": 2,
        "default_library": serial,
        "source": 71,
        "directory_index": 0,
        "controls": {(serial): encode(library_control), (GATEWAY): encode(gateway_control)}
    }))
    .unwrap();
    let snapshot_value: serde_json::Value = serde_json::from_slice(&snapshot_data).unwrap();
    assert_eq!(
        snapshot_value["controls"][serial]["catalog_frozen_index"],
        frozen
    );
    assert_eq!(
        ControlSnapshot::decode(&serde_json::to_vec(&snapshot_value["controls"][serial]).unwrap())
            .unwrap()
            .control
            .catalog_frozen_index,
        Some(frozen)
    );
    let mut snapshot = Snapshot::default();
    snapshot.set_data(snapshot_data.into());
    snapshot.mut_metadata().index = frozen;
    snapshot.mut_metadata().term = 1;
    snapshot
        .mut_metadata()
        .set_conf_state(ConfState::from((vec![71, 72, 73], vec![])));

    let mut senders = HashMap::new();
    let mut receivers = HashMap::new();
    for id in 71..=73 {
        let (tx, rx) = mpsc::channel();
        senders.insert(id, tx);
        receivers.insert(id, rx);
    }
    let peers = Arc::new(senders);
    let mut statuses = HashMap::<u64, SharedStatus>::new();
    let mut flags = HashMap::new();
    let mut observed_events = HashMap::new();
    let mut joins = Vec::new();
    for id in 71..=73 {
        let node_dir = root.join(format!("raft-owner-{id}"));
        fs::create_dir_all(&node_dir).unwrap();
        let files = FileService::new(node_dir.join("fdb-spool")).unwrap();
        let status = Arc::new(Mutex::new(NodeStatus::default()));
        statuses.insert(id, status.clone());
        let enabled = Arc::new(AtomicBool::new(true));
        flags.insert(id, enabled.clone());
        let (exec, requests) = mpsc::channel();
        let (events, event_rx) = mpsc::channel();
        files.set_executor(exec.clone());
        let provider = Box::new(FdbOwnerSimProvider {
            library: library.clone(),
            node: id as u8,
            enabled,
        });
        let control = FdbOwnerControl {
            storage: Default::default(),
            catalog: Arc::new(ManagedCatalog::open(Some(cluster)).unwrap()),
            serial: serial.into(),
            route,
        };
        let (worker_files, worker_status) = (files.clone(), status.clone());
        std::thread::spawn(move || {
            executor::run_fdb_owner(
                provider,
                ExecOptions {
                    node_id: id as u8,
                    interval: Duration::from_millis(50),
                    demo_write: false,
                    salvage: false,
                    block_size: 65536,
                    read_idle: Duration::from_secs(1),
                },
                requests,
                events,
                worker_files,
                worker_status,
                control,
            );
        });
        let scoped = multilib::scoped_sender(serial.into(), peers[&id].clone());
        let (observed_tx, observed_rx) = mpsc::channel();
        observed_events.insert(id, observed_rx);
        std::thread::spawn(move || {
            for event in event_rx {
                let _ = observed_tx.send(event.clone());
                if scoped.send(NodeInput::Exec(event)).is_err() {
                    break;
                }
            }
        });
        let gateway_files = FileService::new(node_dir.join("gateway-spool")).unwrap();
        let gateway_status = Arc::new(Mutex::new(NodeStatus::default()));
        let (gateway_exec, gateway_requests) = mpsc::channel();
        let (gateway_events, gateway_event_rx) = mpsc::channel();
        std::thread::spawn({
            let gateway_files = gateway_files.clone();
            move || {
                multilib::run_gateway(
                    Box::new(multilib::GatewayProvider),
                    gateway_requests,
                    gateway_events,
                    gateway_files,
                )
            }
        });
        let forward = peers[&id].clone();
        std::thread::spawn(move || {
            for event in gateway_event_rx {
                if forward.send(NodeInput::Exec(event)).is_err() {
                    break;
                }
            }
        });
        let runtimes = vec![
            LibraryRuntime {
                id: serial.into(),
                group: "fdb-owner-raft".into(),
                drives: 1,
                directory_file: None,
                exec,
                status,
                files,
            },
            LibraryRuntime {
                id: GATEWAY.into(),
                group: "fdb-owner-raft:gateway".into(),
                drives: 0,
                directory_file: None,
                exec: gateway_exec,
                status: gateway_status,
                files: gateway_files,
            },
        ];
        let mut store = RaftStore::open(&node_dir.join("raft.db"), &[71, 72, 73]).unwrap();
        store.install_snapshot(&snapshot).unwrap();
        drop(store);
        let store = RaftStore::open(&node_dir.join("raft.db"), &[71, 72, 73]).unwrap();
        let cfg = NodeConfig {
            id,
            peers: vec![71, 72, 73],
            tick: Duration::from_millis(25),
            election_ticks: 30,
            heartbeat_ticks: 3,
            status_file: None,
            directory_file: None,
            cooldown: Duration::from_millis(200),
            snapshot: SnapshotPolicy {
                entries: 1000,
                bytes: 8 << 20,
            },
            shutdown_grace: Duration::from_secs(3),
            drives: 0,
            coordination: Some(CoordinationConfig {
                group: "fdb-owner-raft".into(),
                validity: Duration::from_secs(4),
            }),
        };
        let (config, net, tx, rx) = (
            config.clone(),
            Box::new(OwnerRaftNet(peers.clone())),
            peers[&id].clone(),
            receivers.remove(&id).unwrap(),
        );
        joins.push(std::thread::spawn(move || {
            multilib::run(cfg, config, store, net, None, tx, rx, runtimes)
        }));
    }
    let owner = || {
        let state = statuses[&71].lock().unwrap();
        state
            .executor
            .as_ref()
            .filter(|_| state.executor_ready)
            .map(|executor| (executor.node, executor.round))
    };
    let wait_owner = |different_from| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(current) = owner()
                && Some(current.0) != different_from
            {
                break current;
            }
            assert!(
                Instant::now() < deadline,
                "三节点 FDB owner 未收敛: {:?}",
                statuses
                    .iter()
                    .map(|(id, status)| (*id, status.lock().unwrap().clone()))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let first = wait_owner(None);
    assert_eq!(
        catalog
            .claim_active_owner_epoch(serial, first.1)
            .await
            .unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    let key = ReservationKey::new(first.0 as u8, first.1);
    verify_holder(&library.drive(0), key).unwrap();
    verify_holder(&library.changer(), key).unwrap();

    flags[&first.0].store(false, Ordering::SeqCst);
    let second = wait_owner(Some(first.0));
    assert!(second.1 > first.1);
    assert_eq!(
        catalog
            .claim_active_owner_epoch(serial, first.1)
            .await
            .unwrap(),
        ActiveOwnerOutcome::Stale
    );
    assert_eq!(
        catalog
            .claim_active_owner_epoch(serial, second.1)
            .await
            .unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    let key = ReservationKey::new(second.0 as u8, second.1);
    verify_holder(&library.drive(0), key).unwrap();
    verify_holder(&library.changer(), key).unwrap();
    assert_eq!(
        catalog.active_library_owner(serial).await.unwrap(),
        Some((route, second.1))
    );
    assert!(matches!(
        catalog
            .claim_active_owner_epoch(serial, second.1 + 10_000)
            .await
            .unwrap(),
        ActiveOwnerOutcome::Advanced
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = observed_events[&second.0]
            .recv_timeout(remaining)
            .expect("FDB owner 被新 epoch 超越后应报告 Lost");
        if let ExecEvent::Lost { round, reason } = event
            && round == second.1
        {
            assert!(reason.contains("FDB owner 运行期栅栏失效"), "{reason}");
            break;
        }
    }
    for id in 71..=73 {
        peers[&id].send(NodeInput::Shutdown).unwrap();
    }
    for join in joins {
        join.join().unwrap().unwrap();
    }
}

struct ReadHttpServer {
    address: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ReadHttpServer {
    fn start(router: axum::Router) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let finished = stop.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                while !finished.load(Ordering::SeqCst) {
                    if let Ok(Ok((stream, _))) =
                        tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
                    {
                        let router = router.clone();
                        tokio::spawn(async move {
                            hyper::server::conn::http1::Builder::new()
                                .serve_connection(
                                    hyper_util::rt::TokioIo::new(stream),
                                    hyper_util::service::TowerToHyperService::new(router),
                                )
                                .await
                                .unwrap();
                        });
                    }
                }
            });
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn request(&self, method: &str, path: &str, extra: Option<&str>) -> (u16, String, Vec<u8>) {
        let mut stream = std::net::TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{}\r\n",
            extra.unwrap_or("")
        )
        .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let split = bytes
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
        let code = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            code,
            headers.to_ascii_lowercase(),
            bytes[split + 4..].to_vec(),
        )
    }
}

impl Drop for ReadHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_fdb_http_loaded_reads(
    files: Arc<FileService>,
    status: SharedStatus,
    exec: mpsc::Sender<ExecRequest>,
    catalog: Arc<ManagedCatalog>,
    serial: &str,
    route: ActiveLibraryRoute,
    pool: Uuid,
    expected: &[u8],
) -> (ReadHttpServer, ReadHttpServer) {
    use std::collections::BTreeMap;
    use tape_rs::daemon::{fdb_http, http::HttpContext, state::Executor};
    let routes = BTreeMap::from([(serial.to_owned(), route)]);
    {
        let mut status = status.lock().unwrap();
        status.id = 71;
        status.executor = Some(Executor {
            node: 71,
            round: 11,
            term: 1,
            fenced: true,
        });
        status.executor_ready = true;
    }
    let (node, _) = channel();
    let context = Arc::new(HttpContext {
        files: files.clone(),
        status: status.clone(),
        exec: Mutex::new(exec.clone()),
        node: Mutex::new(node.clone()),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(10),
    });
    let owner = ReadHttpServer::start(fdb_http::owner_router(
        catalog.clone(),
        routes.clone(),
        BTreeMap::from([(serial.to_owned(), context)]),
        HashMap::new(),
    ));
    let mut remote = status.lock().unwrap().clone();
    remote.id = 72;
    let entry_context = Arc::new(HttpContext {
        files,
        status: Arc::new(Mutex::new(remote)),
        exec: Mutex::new(exec),
        node: Mutex::new(node),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(10),
    });
    let entry = ReadHttpServer::start(fdb_http::owner_router(
        catalog,
        routes,
        BTreeMap::from([(serial.to_owned(), entry_context.clone())]),
        HashMap::from([(71, owner.address.clone())]),
    ));
    let path = format!("/v1/fdb/files/{serial}?pool={pool}&path=L2FjdHVhbA");
    for server in [&owner, &entry] {
        let (ready, _, ready_body) = server.request("GET", "/readyz", None);
        assert_eq!(ready, 200);
        assert_eq!(
            server
                .request("GET", &format!("/readyz?library={serial}"), None)
                .0,
            200
        );
        assert_eq!(
            server.request("GET", "/readyz?library=missing", None).0,
            404
        );
        assert!(
            String::from_utf8(ready_body)
                .unwrap()
                .contains("fdb-owner-readonly")
        );
        let (code, headers, body) = server.request("GET", &path, None);
        assert_eq!(code, 200, "{headers} {body:?}");
        assert!(headers.contains(&format!("content-length: {}", expected.len())));
        assert_eq!(body, expected);
        let (code, headers, body) = server.request("GET", &path, Some("Range: bytes=1-3\r\n"));
        assert_eq!(code, 206, "{headers} {body:?}");
        assert!(headers.contains(&format!("content-range: bytes 1-3/{}", expected.len())));
        assert_eq!(body, expected[1..4]);
        let (code, headers, _) = server.request("GET", &path, Some("Range: bytes=999999-\r\n"));
        assert_eq!(code, 416);
        assert!(headers.contains(&format!("content-range: bytes */{}", expected.len())));
        assert_eq!(
            server
                .request(
                    "GET",
                    &format!("/v1/fdb/files/{serial}?pool={pool}&path=L21pc3Npbmc"),
                    None
                )
                .0,
            404
        );
        assert_eq!(
            server
                .request(
                    "GET",
                    &format!(
                        "/v1/fdb/files/{serial}?pool={}&path=L2FjdHVhbA",
                        Uuid::new_v4()
                    ),
                    None
                )
                .0,
            404
        );
        assert_eq!(
            server
                .request(
                    "GET",
                    &format!("/v1/fdb/files/{serial}?pool=bad&path=L2FjdHVhbA"),
                    None
                )
                .0,
            400
        );
        assert_eq!(
            server
                .request(
                    "GET",
                    &format!("/v1/fdb/files/{serial}?pool={pool}&path=Lw"),
                    None
                )
                .0,
            409
        );
        assert_eq!(
            server
                .request(
                    "GET",
                    &format!("/v1/fdb/files/not-configured?pool={pool}&path=L2FjdHVhbA"),
                    None
                )
                .0,
            404
        );
        assert_eq!(server.request("HEAD", &path, None).0, 405);
        assert_eq!(
            server
                .request("PUT", &path, Some("Content-Length: 0\r\n"))
                .0,
            405
        );
    }
    assert_eq!(
        entry
            .request("GET", &path, Some("X-Tapers-Forwarded: 1\r\n"))
            .0,
        503
    );
    entry_context.status.lock().unwrap().executor_ready = false;
    assert_eq!(entry.request("GET", &path, None).0, 503);
    entry_context.status.lock().unwrap().executor_ready = true;
    assert_eq!(entry.request("GET", &path, None).2, expected);
    (owner, entry)
}

struct ScheduleTransport {
    inner: Box<dyn TapeTransport + Send>,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
    revoke_on_unload: Arc<AtomicBool>,
    error_after_move: Arc<AtomicBool>,
    permission: Arc<Permission>,
}
impl TapeTransport for ScheduleTransport {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.trace.lock().unwrap().push(cdb.to_vec());
        let result = self.inner.execute_no_data(cdb, timeout)?;
        if cdb.first() == Some(&0x1b)
            && cdb[4] == 0
            && self.revoke_on_unload.swap(false, Ordering::SeqCst)
        {
            self.permission.close(11);
        }
        if cdb.first() == Some(&0xa5) && self.error_after_move.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("模拟 MOVE 已执行但返回结果丢失").into());
        }
        Ok(result)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        bytes: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.trace.lock().unwrap().push(cdb.to_vec());
        self.inner.execute_read(cdb, bytes, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        bytes: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.trace.lock().unwrap().push(cdb.to_vec());
        self.inner.execute_write(cdb, bytes, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}
struct ScheduleProvider {
    base: FdbOwnerSimProvider,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
    revoke_on_unload: Arc<AtomicBool>,
    error_after_move: Arc<AtomicBool>,
    permission: Arc<Permission>,
}
impl DeviceProvider for ScheduleProvider {
    fn open_all(&self) -> tape_rs::error::Result<Vec<ManagedDevice>> {
        Ok(self
            .base
            .open_all()?
            .into_iter()
            .map(|mut device| {
                device.dev = Box::new(ScheduleTransport {
                    inner: device.dev,
                    trace: self.trace.clone(),
                    revoke_on_unload: self.revoke_on_unload.clone(),
                    error_after_move: self.error_after_move.clone(),
                    permission: self.permission.clone(),
                });
                device
            })
            .collect())
    }
}
fn fixture_return_to_storage(library: &SimLibrary, node: u32) {
    use tape_rs::changer::{commands::MediumChanger, element::ElementType};
    let changer = library.changer_as(node);
    let mut changer = MediumChanger::new(&changer);
    changer.load_address_map().unwrap();
    let elements = changer.read_all_status().unwrap();
    let drive = elements
        .iter()
        .find(|element| element.element_type == ElementType::DataTransfer && element.full)
        .unwrap();
    let slot = elements
        .iter()
        .find(|element| element.element_type == ElementType::Storage && !element.full)
        .unwrap();
    TapeDrive::new(&library.drive_as(0, node)).unload().unwrap();
    changer.move_medium(drive.address, slot.address).unwrap();
}
fn fixture_drive_empty(library: &SimLibrary) -> bool {
    use tape_rs::changer::{commands::MediumChanger, element::ElementType};
    let changer = library.changer_as(71);
    let mut changer = MediumChanger::new(&changer);
    changer.load_address_map().unwrap();
    changer
        .read_all_status()
        .unwrap()
        .iter()
        .filter(|element| element.element_type == ElementType::DataTransfer)
        .all(|element| !element.full)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScheduleFault {
    ExternalPreempt,
    AfterUnload,
    AfterMove,
}

#[allow(clippy::too_many_arguments)]
async fn run_fdb_owner_loaded_reads(
    root: &std::path::Path,
    library: &SimLibrary,
    catalog: Arc<ManagedCatalog>,
    serial: &str,
    route: ActiveLibraryRoute,
    pool: Uuid,
    expected: &[u8],
    fault: ScheduleFault,
) {
    // Earlier direct-media checks used initiator 0; the executor provider
    // uses initiator 71. Retire those registrations before reusing round 11.
    tape_rs::scsi::reservation::release_and_unregister(
        &library.drive(0),
        ReservationKey::new(71, 11),
    )
    .unwrap();
    tape_rs::scsi::reservation::release_and_unregister(
        &library.changer_as(71),
        ReservationKey::new(71, 11),
    )
    .unwrap();
    let barcode = catalog
        .resolve_active_read_source_for_owner(serial, b"/actual", route, 11)
        .await
        .unwrap()
        .unwrap()
        .placement()
        .barcode
        .clone();
    fixture_return_to_storage(library, 71);
    let files = FileService::new(root.join("queued-owner-spool")).unwrap();
    let status = Arc::new(Mutex::new(NodeStatus::default()));
    status.lock().unwrap().catalog_frozen_index = Some(route.frozen_index);
    let permission = Arc::new(Permission::default());
    let trace = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let revoke_on_unload = Arc::new(AtomicBool::new(false));
    let error_after_move = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let (event_tx, event_rx) = channel();
    let join = {
        let files = files.clone();
        let library = library.clone();
        let serial = serial.to_owned();
        let catalog = catalog.clone();
        let status = status.clone();
        let trace = trace.clone();
        let revoke_on_unload = revoke_on_unload.clone();
        let error_after_move = error_after_move.clone();
        let permission = permission.clone();
        std::thread::spawn(move || {
            tape_rs::daemon::executor::run_fdb_owner(
                Box::new(ScheduleProvider {
                    base: FdbOwnerSimProvider {
                        library,
                        node: 71,
                        enabled: Arc::new(AtomicBool::new(true)),
                    },
                    trace,
                    revoke_on_unload,
                    error_after_move,
                    permission,
                }),
                ExecOptions {
                    node_id: 71,
                    interval: Duration::from_millis(100),
                    demo_write: false,
                    salvage: false,
                    block_size: 64 * 1024,
                    read_idle: Duration::from_secs(1),
                },
                rx,
                event_tx,
                files,
                status,
                FdbOwnerControl {
                    storage: Default::default(),
                    catalog,
                    serial,
                    route,
                },
            )
        })
    };
    tx.send(ExecRequest::AttachPermission {
        permission: permission.clone(),
    })
    .unwrap();
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ExecEvent::Capability { eligible: true, .. }
    ));
    assert!(permission.renew(
        11,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120)
    ));
    tx.send(ExecRequest::Takeover { round: 11 }).unwrap();
    let fenced = next_owner_event(&event_rx);
    assert!(
        matches!(&fenced, ExecEvent::Fenced { round: 11 }),
        "{fenced:?}"
    );
    let (read_tx, read_rx) = channel();
    tx.send(ExecRequest::ReadTo {
        path: "/actual".into(),
        file: files.read_spool().unwrap(),
        reply: read_tx,
    })
    .unwrap();
    assert!(matches!(
        read_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Busy(_))
    ));
    assert!(permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    tx.send(ExecRequest::Recover { round: 11 }).unwrap();
    assert!(matches!(
        next_owner_event(&event_rx),
        ExecEvent::Serving { round: 11, .. }
    ));
    assert!(permission.renew(
        11,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120)
    ));
    let cache_dir = root.join("queued-owner-spool.fdb-extents");
    assert!(!cache_dir.exists());
    let (wrong_pool_tx, wrong_pool_rx) = channel();
    tx.send(ExecRequest::Scoped {
        pool: Uuid::new_v4().to_string(),
        lane: 0,
        request: Box::new(ExecRequest::Read {
            path: "/actual".into(),
            reply: wrong_pool_tx,
        }),
    })
    .unwrap();
    assert!(matches!(
        wrong_pool_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Failed(_))
    ));
    assert!(!cache_dir.exists(), "错池请求不得重建 extent 缓存");
    assert!(
        !trace.lock().unwrap().iter().any(|cdb| cdb[0] == 0xa5),
        "错池不得移动介质"
    );
    // Capacity incompatibility, cleaning and foreign occupied drive all reject before MOVE.
    let read = || {
        let (reply, received) = channel();
        tx.send(ExecRequest::Read {
            path: "/actual".into(),
            reply,
        })
        .unwrap();
        received.recv_timeout(Duration::from_secs(10)).unwrap()
    };
    library.set_drive_densities(0, vec![(0x60, true)]);
    assert!(matches!(read(), Err(ReadError::Busy(_))));
    library.set_drive_densities(0, vec![(0x5e, true), (0x60, true)]);
    library.with_cartridge_mut(&barcode, |cartridge| cartridge.medium_type = 2);
    assert!(matches!(read(), Err(ReadError::Failed(_))));
    library.with_cartridge_mut(&barcode, |cartridge| cartridge.medium_type = 1);
    // This foreign cartridge is a fixture only; it is never formatted or read by the owner.
    let foreign = match fault {
        ScheduleFault::ExternalPreempt => "FOREI1L8",
        ScheduleFault::AfterUnload => "FOREI2L8",
        ScheduleFault::AfterMove => "FOREI3L8",
    };
    library
        .insert_cartridge(SimCartridge::blank(foreign, 128 << 20), 3)
        .unwrap();
    library.load_into_drive(foreign, 0).unwrap();
    let before = trace.lock().unwrap().len();
    assert!(matches!(read(), Err(ReadError::Busy(_))));
    assert!(
        !trace.lock().unwrap()[before..]
            .iter()
            .any(|cdb| matches!(cdb[0], 0xa5 | 0x08 | 0x1b)),
        "未归属介质不得被读取/卸载/移动"
    );
    fixture_return_to_storage(library, 71);
    assert!(!trace.lock().unwrap().iter().any(|cdb| cdb[0] == 0xa5));

    // A local capacity/admission refusal must leave the owner serving. Keep
    // the directory lock busy, then release it and retry the same request.
    fs::create_dir(&cache_dir).unwrap();
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(cache_dir.join(".cache-budget.lock"))
        .unwrap();
    let cache_lock =
        nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
    let (busy_tx, busy_rx) = channel();
    tx.send(ExecRequest::ReadTo {
        path: "/actual".into(),
        file: files.read_spool().unwrap(),
        reply: busy_tx,
    })
    .unwrap();
    assert!(matches!(
        busy_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Busy(_))
    ));
    drop(cache_lock);
    let (read_tx, read_rx) = channel();
    tx.send(ExecRequest::Scoped {
        pool: pool.to_string(),
        lane: 0,
        request: Box::new(ExecRequest::ReadTo {
            path: "/actual".into(),
            file: files.read_spool().unwrap(),
            reply: read_tx,
        }),
    })
    .unwrap();
    let mut file = read_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
    let (missing_tx, missing_rx) = channel();
    tx.send(ExecRequest::Read {
        path: "/missing".into(),
        reply: missing_tx,
    })
    .unwrap();
    assert!(matches!(
        missing_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Failed(_))
    ));
    let (invalid_tx, invalid_rx) = channel();
    tx.send(ExecRequest::Read {
        path: "/".into(),
        reply: invalid_tx,
    })
    .unwrap();
    assert!(matches!(
        invalid_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Failed(_))
    ));
    let (read_tx, read_rx) = channel();
    tx.send(ExecRequest::Read {
        path: "/actual".into(),
        reply: read_tx,
    })
    .unwrap();
    assert_eq!(
        read_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap(),
        expected
    );
    for target_pool in [
        pool.to_string(),
        Uuid::new_v4().to_string(),
        "invalid-pool".into(),
    ] {
        let (read_tx, read_rx) = channel();
        tx.send(ExecRequest::Scoped {
            pool: target_pool.clone(),
            lane: 0,
            request: Box::new(ExecRequest::ReadTo {
                path: "/actual".into(),
                file: files.read_spool().unwrap(),
                reply: read_tx,
            }),
        })
        .unwrap();
        let result = read_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if target_pool == pool.to_string() {
            let mut bytes = Vec::new();
            result.unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
        } else {
            assert!(matches!(result, Err(ReadError::Failed(_))), "{result:?}");
        }
        let (read_tx, read_rx) = channel();
        tx.send(ExecRequest::Scoped {
            pool: target_pool.clone(),
            lane: 0,
            request: Box::new(ExecRequest::Read {
                path: "/actual".into(),
                reply: read_tx,
            }),
        })
        .unwrap();
        let result = read_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if target_pool == pool.to_string() {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(result, Err(ReadError::Failed(_))), "{result:?}");
        }
    }
    // Rejected scopes leave the current owner available for a valid request.
    let (read_tx, read_rx) = channel();
    tx.send(ExecRequest::Scoped {
        pool: pool.to_string(),
        lane: 0,
        request: Box::new(ExecRequest::Read {
            path: "/actual".into(),
            reply: read_tx,
        }),
    })
    .unwrap();
    assert_eq!(
        read_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap(),
        expected
    );
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| cdb[0] == 0xa5)
            .count(),
        1,
        "连续读取应复用同一装载"
    );
    let idle_deadline = Instant::now() + Duration::from_secs(10);
    while !fixture_drive_empty(library) && Instant::now() < idle_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(fixture_drive_empty(library), "空闲读带应归槽");
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| cdb[0] == 0xa5)
            .count(),
        2
    );
    assert!(files.begin("/legacy-write-still-forbidden", 1).is_err());
    let (http_owner, http_entry) = run_fdb_http_loaded_reads(
        files.clone(),
        status,
        tx.clone(),
        catalog,
        serial,
        route,
        pool,
        expected,
    );
    if fault != ScheduleFault::ExternalPreempt {
        let before = trace.lock().unwrap().len();
        let trigger = if fault == ScheduleFault::AfterUnload {
            &revoke_on_unload
        } else {
            &error_after_move
        };
        trigger.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while trigger.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!trigger.load(Ordering::SeqCst), "没有触发装卸故障");
        std::thread::sleep(Duration::from_millis(100));
        let trace = trace.lock().unwrap();
        assert!(
            trace[before..]
                .iter()
                .any(|cdb| cdb[0] == 0x1b && cdb[4] == 0)
        );
        assert_eq!(
            trace[before..].iter().filter(|cdb| cdb[0] == 0xa5).count(),
            usize::from(fault == ScheduleFault::AfterMove),
            "UNLOAD 后失权不得 MOVE；MOVE 结果不明不得重试或补偿"
        );
    } else {
        fence(&library.drive_as(0, 72), ReservationKey::new(72, 11)).unwrap();
    }
    let (read_tx, read_rx) = channel();
    tx.send(ExecRequest::ReadTo {
        path: "/actual".into(),
        file: files.read_spool().unwrap(),
        reply: read_tx,
    })
    .unwrap();
    assert!(matches!(
        read_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(ReadError::Busy(_) | ReadError::Lost(_))
    ));
    assert!(matches!(
        next_owner_event(&event_rx),
        ExecEvent::Lost { round: 11, .. }
    ));
    let response = http_entry.request(
        "GET",
        &format!("/v1/fdb/files/{serial}?pool={pool}&path=L2FjdHVhbA"),
        None,
    );
    assert_eq!(response.0, 503, "失去 PR 后不得通过 HTTP 返回内容");
    assert_ne!(response.2, expected);
    assert_eq!(
        http_owner.request("GET", "/readyz", None).0,
        503,
        "owner 失去本地 Serving 许可后不能继续报告 ready"
    );
    assert_eq!(
        http_entry.request("GET", "/readyz", None).0,
        503,
        "入口必须向实际 owner 确认可服务状态，不能只看旧控制记录"
    );
    drop(http_entry);
    drop(http_owner);
    let (done_tx, done_rx) = channel();
    tx.send(ExecRequest::Shutdown {
        done: Some(done_tx),
    })
    .unwrap();
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    join.join().unwrap();
    let cache_dir = root.join("queued-owner-spool.fdb-extents");
    let cached = fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .unwrap();
    let cached_inode = fs::metadata(&cached).unwrap().ino();
    let _reopened = FileService::new(root.join("queued-owner-spool")).unwrap();
    assert_eq!(fs::metadata(&cached).unwrap().ino(), cached_inode);
    assert!(
        !trace
            .lock()
            .unwrap()
            .iter()
            .any(|cdb| matches!(cdb[0], 0x04 | 0x0a | 0x10 | 0x19 | 0x8d)),
        "只读装卸不得格式化、写文件/文件标记或写 MAM"
    );
    if fixture_drive_empty(library) {
        library.load_into_drive(&barcode, 0).unwrap();
    }
    if fault != ScheduleFault::ExternalPreempt {
        tape_rs::scsi::reservation::release_and_unregister(
            &library.drive_as(0, 71),
            ReservationKey::new(71, 11),
        )
        .unwrap();
    }
    fence(&library.drive(0), ReservationKey::new(71, 11)).unwrap();
    TapeDrive::new(&library.drive(0)).load().unwrap();
}

#[test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅写入内存模拟磁带和随机合成目录"]
fn simulated_media_commit_records_real_s4_index() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(simulated_media_commit());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn simulated_media_commit() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let serial = format!("SIM-MEDIA-{namespace}");
    let barcode = "SIMFDB1L8";
    let root = std::env::temp_dir().join(format!("tape-rs-fdb-media-{namespace}"));
    fs::create_dir(&root).unwrap();

    let library = SimLibrary::new(1, 4, 1);
    library.enable_changer_pr();
    library
        .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), 0)
        .unwrap();
    library.load_into_drive(barcode, 0).unwrap();
    let drive = library.drive(0);
    mkltfs(
        &drive,
        &MkltfsOptions {
            volume_id: "SFDB01".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let volume = LtfsVolume::mount(&drive).unwrap();
    let baseline_view = volume.index().clone();
    let baseline = ActiveIndexIdentity::from(volume.committed_index_construct());
    let xml = volume.index().to_xml().unwrap();
    let source = ExpectedIndex {
        volume_uuid: baseline.volume_uuid,
        generation: baseline.generation,
        location: baseline.location,
        sha256: Sha256::digest(&xml).into(),
    };
    assert_eq!(baseline.sha256, source.sha256);
    drop(volume);

    let spool_path = root.join("import.db");
    ImportSpool::build(&xml[..], &spool_path, source).unwrap();
    let spool = ImportSpool::open(&spool_path, source).unwrap();
    let snapshot = root.join("snapshot.db");
    let db = rusqlite::Connection::open(&snapshot).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
         CREATE TABLE pools(uuid TEXT PRIMARY KEY,name TEXT NOT NULL,file_limit INTEGER NOT NULL);
         CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
           volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL,
           files INTEGER NOT NULL DEFAULT 0,bytes_used INTEGER NOT NULL DEFAULT 0,
           bytes_written INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
           generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
           ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
           metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
         CREATE TABLE staging(barcode TEXT NOT NULL);
         CREATE TABLE file_tasks(pool_uuid TEXT NOT NULL,task INTEGER NOT NULL,
           round INTEGER NOT NULL,path TEXT NOT NULL,status TEXT NOT NULL,
           generation INTEGER,reason TEXT NOT NULL DEFAULT '');
         INSERT INTO meta(k,v) VALUES('applied_index',92),('catalog_frozen_index',90);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO pools(uuid,name,file_limit) VALUES(?1,'sim-media',1000000)",
        [pool.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            barcode,
            pool.to_string(),
            baseline.volume_uuid.to_string(),
            baseline.generation as i64
        ],
    )
    .unwrap();

    assert!(catalog.begin_managed_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert_eq!(
        catalog
            .create_pool(
                namespace,
                &PoolRecord {
                    library_serial: serial.clone(),
                    id: pool,
                    name: "sim-media".into(),
                    file_limit: 1_000_000,
                    policy: PoolPolicy::default(),
                    revision: 1,
                },
            )
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    assert_eq!(
        catalog
            .assign_tape(namespace, pool, &serial, barcode)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    let placement = VolumePlacement {
        volume_id: baseline.volume_uuid,
        namespace,
        pool_id: pool,
        library_serial: serial.clone(),
        barcode: barcode.into(),
    };
    assert_eq!(
        catalog
            .register_volume_placement(&placement, 10)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    let scan = VolumeScan {
        generation: baseline.generation,
        id: Uuid::new_v4(),
    };
    assert_eq!(
        catalog
            .begin_volume_reconcile(&serial, 10, baseline.volume_uuid, scan)
            .await
            .unwrap(),
        VolumeOutcome::Reconciling
    );
    let cache_path = root.join("baseline.extent.db");
    VolumeExtentCache::build(&xml[..], &cache_path, source).unwrap();
    assert_eq!(
        catalog
            .record_volume_source(
                &serial,
                10,
                baseline.volume_uuid,
                scan,
                source,
                LocalVolumeSources {
                    extent_cache: &cache_path,
                    import_spool: &spool_path,
                },
            )
            .await
            .unwrap(),
        SourceOutcome::Recorded
    );
    let empty = ScanDigest::new(baseline.volume_uuid, scan)
        .unwrap()
        .summary();
    assert_eq!(
        catalog
            .freeze_volume_scan(&serial, 10, baseline.volume_uuid, scan, empty)
            .await
            .unwrap(),
        VerificationOutcome::Frozen
    );
    assert_eq!(
        catalog
            .verify_volume_scan_page_with_cache(
                &serial,
                10,
                baseline.volume_uuid,
                scan,
                100,
                &cache_path,
            )
            .await
            .unwrap(),
        VerificationOutcome::Verified(empty)
    );
    assert_eq!(
        catalog
            .mark_volume_ready(&serial, 10, baseline.volume_uuid, scan)
            .await
            .unwrap(),
        VolumeOutcome::Ready
    );
    let binding = ImportSourceBinding {
        placement: placement.clone(),
        scan,
        source,
    };
    let volumes = || {
        vec![ImportVolume {
            pool_id: pool,
            barcode: barcode.into(),
            volume_id: baseline.volume_uuid,
            generation: baseline.generation,
            spool: &spool,
        }]
    };
    let staged = catalog
        .stage_audited_reconcile(namespace, &snapshot, &root, volumes(), &[binding])
        .await
        .unwrap();
    assert_eq!(staged.paths, 0);
    catalog
        .confirm_bound_staged_import(namespace)
        .await
        .unwrap();
    assert!(
        catalog
            .materialize_bound_import_page(namespace, 100)
            .await
            .unwrap()
            .complete()
    );
    assert_eq!(
        catalog.freeze_bound_staged_import(namespace).await.unwrap(),
        NamespaceVerificationOutcome::Frozen
    );
    loop {
        match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
            NamespaceVerificationOutcome::Progress { .. } => continue,
            NamespaceVerificationOutcome::Verified(_) => break,
            other => panic!("空目录核验失败：{other:?}"),
        }
    }
    assert_eq!(
        catalog.verify_bound_import_copies(namespace).await.unwrap(),
        0
    );
    catalog
        .inspect_bound_import_copy_fence(namespace)
        .await
        .unwrap();
    assert!(catalog.seal_bound_staged_import(namespace).await.unwrap());
    let route = match catalog
        .prepare_library_route(&serial, namespace, None)
        .await
        .unwrap()
    {
        PrepareRouteOutcome::Prepared(route) => route,
        other => panic!("准备路由失败：{other:?}"),
    };
    let proof = |node, path: &std::path::Path| {
        let identity = inspect_frozen_snapshot(path).unwrap();
        verify_node_export(
            path,
            node,
            identity.applied_index,
            identity.frozen_index,
            identity.snapshot_sha256,
        )
        .unwrap()
    };
    let node72 = root.join("node72.db");
    let node73 = root.join("node73.db");
    for (path, applied) in [(&node72, 93), (&node73, 94)] {
        fs::copy(&snapshot, path).unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute("UPDATE meta SET v=?1 WHERE k='applied_index'", [applied])
            .unwrap();
    }
    let evidence = [proof(71, &snapshot), proof(72, &node72), proof(73, &node73)];
    assert!(
        catalog
            .certify_prepared_cutover(&serial, route, 71, evidence)
            .await
            .unwrap()
    );
    let certificate = catalog
        .certified_prepared_cutover(&serial)
        .await
        .unwrap()
        .unwrap();
    let active = catalog
        .activate_prepared_cutover(&serial, certificate)
        .await
        .unwrap();
    assert_eq!(active.initial_owner_epoch, 11);
    let loaded_head = catalog
        .active_loaded_volume_head(&serial, barcode, active, 11)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded_head.volume_id, baseline.volume_uuid);
    assert_eq!(loaded_head.pool_id, pool);
    assert_eq!(loaded_head.index, baseline);
    assert!(
        catalog
            .active_loaded_volume_head(&serial, "UNASSIGNEDL8", active, 11)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        catalog
            .active_loaded_volume_head(&serial, barcode, active, 10)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    let mut wrong_route = active;
    wrong_route.route.revision += 1;
    assert!(
        catalog
            .active_loaded_volume_head(&serial, barcode, wrong_route, 11)
            .await
            .is_err()
    );
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        baseline
    );

    // The formal executor's FDB control mode must fence first, require the
    // exact frozen Raft index, then advance the FDB owner epoch. It never
    // mounts, writes, or unloads this loaded simulated cartridge.
    let owner_catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let owner_files = FileService::new(root.join("fdb-owner-spool")).unwrap();
    let owner_status = Arc::new(Mutex::new(NodeStatus::default()));
    owner_status.lock().unwrap().catalog_frozen_index = Some(active.frozen_index + 1);
    let owner_permission = Arc::new(Permission::default());
    let (owner_tx, owner_rx) = channel();
    let (event_tx, event_rx) = channel();
    let owner_handle = {
        let serial = serial.clone();
        let library = library.clone();
        let files = owner_files.clone();
        let status = owner_status.clone();
        let catalog = owner_catalog.clone();
        std::thread::spawn(move || {
            tape_rs::daemon::executor::run_fdb_owner(
                Box::new(FdbOwnerSimProvider {
                    library,
                    node: 71,
                    enabled: Arc::new(AtomicBool::new(true)),
                }),
                ExecOptions {
                    node_id: 71,
                    interval: Duration::from_millis(100),
                    demo_write: false,
                    salvage: false,
                    block_size: 64 * 1024,
                    read_idle: Duration::from_secs(1),
                },
                owner_rx,
                event_tx,
                files,
                status,
                FdbOwnerControl {
                    storage: Default::default(),
                    catalog,
                    serial,
                    route: active,
                },
            );
        })
    };
    owner_tx
        .send(ExecRequest::AttachPermission {
            permission: owner_permission.clone(),
        })
        .unwrap();
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ExecEvent::Capability {
            drives: 1,
            eligible: true
        }
    ));
    assert!(owner_permission.renew(
        10,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120)
    ));
    owner_tx.send(ExecRequest::Takeover { round: 10 }).unwrap();
    let first_event = next_owner_event(&event_rx);
    assert!(
        matches!(&first_event, ExecEvent::Fenced { round: 10 }),
        "{first_event:?}"
    );
    assert!(owner_permission.renew(
        10,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    owner_tx.send(ExecRequest::Recover { round: 10 }).unwrap();
    assert!(matches!(
        next_owner_event(&event_rx),
        ExecEvent::Lost { round: 10, .. }
    ));
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 11).await.unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    owner_status.lock().unwrap().catalog_frozen_index = Some(active.frozen_index);
    assert!(owner_permission.renew(
        11,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120)
    ));
    owner_tx.send(ExecRequest::Takeover { round: 11 }).unwrap();
    assert!(matches!(
        next_owner_event(&event_rx),
        ExecEvent::Fenced { round: 11 }
    ));
    assert!(owner_permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    owner_tx.send(ExecRequest::Recover { round: 11 }).unwrap();
    let serving = next_owner_event(&event_rx);
    assert!(
        matches!(&serving, ExecEvent::Serving { round: 11, summary } if summary.contains("已核对 1 卷 DP/IP 索引")),
        "{serving:?}"
    );
    std::thread::sleep(Duration::from_millis(400));
    while let Ok(event) = event_rx.try_recv() {
        assert!(matches!(event, ExecEvent::Capability { .. }), "{event:?}");
    }
    assert!(owner_permission.renew(
        11,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120)
    ));
    let (sync_tx, sync_rx) = channel();
    owner_tx
        .send(ExecRequest::Sync {
            round: 11,
            reply: sync_tx,
        })
        .unwrap();
    assert!(
        sync_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .is_err()
    );
    assert!(owner_files.begin("/legacy-write-forbidden", 1).is_err());
    let rival_key = ReservationKey::new(72, 11);
    fence(&library.drive_as(0, 72), rival_key).unwrap();
    assert!(matches!(
        next_owner_event(&event_rx),
        ExecEvent::Lost { round: 11, .. }
    ));
    let (done_tx, done_rx) = channel();
    owner_tx
        .send(ExecRequest::Shutdown {
            done: Some(done_tx),
        })
        .unwrap();
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    owner_handle.join().unwrap();

    let key = ReservationKey::new(71, 11);
    fence(&drive, key).unwrap();
    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let unguarded = LtfsVolume::mount(&drive).unwrap();
    assert!(
        establish_active_media_owner(&catalog, unguarded, &serial, key, &permission)
            .await
            .is_err()
    );
    let guarded_drive = GuardedTransport::new(Box::new(library.drive(0)), permission.clone(), 11);
    let volume = LtfsVolume::mount(&guarded_drive).unwrap();
    let ready = establish_active_media_owner(&catalog, volume, &serial, key, &permission)
        .await
        .unwrap();
    assert_eq!(ready.index(), baseline);
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 10).await.unwrap(),
        ActiveOwnerOutcome::Stale
    );
    drop(ready);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let bytes = b"actual FDB media batch";
    let data_path = root.join("upload.bin");
    let mut file = fs::File::create(&data_path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let intent = ActiveFileIntent {
        namespace,
        parent: Uuid::nil(),
        parent_path: b"/".to_vec(),
        name: b"actual".to_vec(),
        file_id: Uuid::new_v4(),
        expected_version: None,
        operation_id: Uuid::new_v4(),
        request_epoch: 1,
        library_serial: serial.clone(),
        pool_id: Some(pool),
        owner_epoch: 11,
        volume_id: baseline.volume_uuid,
        volume_generation: baseline.generation + 1,
        volume_scan_id: scan.id,
        expected_size: bytes.len() as u64,
        expected_digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
    };
    let batch_id = Uuid::new_v4();
    let committed = commit_active_media_batch(
        &catalog,
        volume,
        batch_id,
        &[ActiveMediaUpload {
            intent: intent.clone(),
            spool: data_path.clone(),
        }],
    )
    .await
    .unwrap();
    assert_eq!(committed.previous_index(), baseline);
    assert_eq!(
        committed.current_index().generation,
        baseline.generation + 1
    );
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        committed.current_index()
    );
    assert_eq!(committed.files().len(), 1);
    assert_eq!(
        committed.files()[0].receipt.ltfs_file_uid,
        committed.files()[0].attributes.meta().unwrap().file_uid
    );
    let expected_uid = committed.files()[0].receipt.ltfs_file_uid;
    let mut current = committed.current_index();
    let committed = publish_active_media_batch(&catalog, committed)
        .await
        .unwrap();
    assert!(matches!(
        catalog.resolve_active_path(&serial, b"/actual").await.unwrap(),
        Some(ResolvedPath::File { copy, .. }) if copy.ltfs_file_uid == expected_uid
    ));
    assert!(
        catalog
            .active_batch_status(&serial, baseline.volume_uuid, current.generation)
            .await
            .unwrap()
            .unwrap()
            .completed
    );
    let committed_xml = if current.incremental {
        committed
            .volume()
            .index()
            .incremental_since(&baseline_view)
            .unwrap()
            .to_xml()
            .unwrap()
    } else {
        committed.volume().index().to_xml().unwrap()
    };
    let expected_current = ExpectedIndex {
        volume_uuid: current.volume_uuid,
        generation: current.generation,
        location: current.location,
        sha256: current.sha256,
    };
    let committed_digest: [u8; 32] = Sha256::digest(&committed_xml).into();
    assert_eq!(committed_digest, current.sha256);
    let source_lookup = catalog
        .resolve_active_read_source_for_owner(&serial, b"/actual", active, 11)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source_lookup.source, current);
    assert!(
        source_lookup
            .clone()
            .plan_read(&root.join("absent-cache.db"))
            .is_err()
    );
    assert!(matches!(
        catalog
            .resolve_active_read_source_for_owner(&serial, b"/missing", active, 10)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    let current_cache = root.join("current.extent.db");
    if current.incremental {
        VolumeExtentCache::build_incremental(&committed_xml[..], &current_cache, expected_current)
            .unwrap();
    } else {
        VolumeExtentCache::build(&committed_xml[..], &current_cache, expected_current).unwrap();
    }
    let read = catalog
        .resolve_active_read(&serial, b"/actual", &current_cache)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.physical.length, bytes.len() as u64);
    let mut actual = Vec::new();
    committed
        .volume()
        .read_file_to_writer("/actual", &mut actual)
        .unwrap();
    assert_eq!(actual, bytes);
    let label = committed.volume().label().clone();
    drop(committed);
    let media_cache = root.join("current-from-media.extent.db");
    VolumeExtentCache::build_verified_latest_from_media(
        &TapeDrive::new(&drive),
        &label,
        &RecoveryBudget::default(),
        &media_cache,
        expected_current,
        current.incremental,
    )
    .unwrap();
    let media_read = catalog
        .resolve_active_read(&serial, b"/actual", &media_cache)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(media_read, read);
    let owner_read = catalog
        .resolve_active_read_for_owner(&serial, b"/actual", &media_cache, active, 11)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner_read.read, media_read);
    assert_eq!(owner_read.source, current);
    assert_eq!(source_lookup.plan_read(&media_cache).unwrap(), owner_read);
    let read_spool = root.join("read-spool");
    fs::create_dir(&read_spool).unwrap();
    let reader = ActiveMediaReader {
        catalog: &catalog,
        device: &guarded_drive,
        serial: &serial,
        route: active,
        key,
        permission: &permission,
        storage: Default::default(),
    };
    assert!(
        reader
            .read(b"/missing", &media_cache, &read_spool)
            .await
            .unwrap()
            .is_none()
    );
    let denied = ActiveMediaReader {
        storage: tape_rs::daemon::fdb_read::ReadStorageBudget {
            download_bytes: 0,
            ..reader.storage
        },
        ..reader
    };
    let reads_before = library
        .drive(0)
        .command_log()
        .iter()
        .filter(|op| **op == tape_rs::scsi::cdb::opcode::READ_6)
        .count();
    assert!(matches!(
        denied.read(b"/actual", &media_cache, &read_spool).await,
        Err(tape_rs::error::TapeError::LocalCapacity { .. })
    ));
    let reads_after = library
        .drive(0)
        .command_log()
        .iter()
        .filter(|op| **op == tape_rs::scsi::cdb::opcode::READ_6)
        .count();
    assert_eq!(
        reads_after, reads_before,
        "download admission precedes tape reads"
    );
    assert_eq!(fs::read_dir(&read_spool).unwrap().count(), 0);
    let mut verified_read = reader
        .read(b"/actual", &media_cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut verified_bytes = Vec::new();
    verified_read.file.read_to_end(&mut verified_bytes).unwrap();
    assert_eq!(verified_bytes, bytes);
    assert_eq!(verified_read.metadata, media_read);
    assert_eq!(fs::read_dir(&read_spool).unwrap().count(), 0);
    assert!(!current.incremental, "冷读夹具首先覆盖 Full 索引");
    let automatic_cache = root.join("automatic-extents");
    assert!(
        reader
            .read_with_cache(b"/missing", &automatic_cache, &read_spool)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!automatic_cache.exists());
    let mut cold_read = reader
        .read_with_cache(b"/actual", &automatic_cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut cold_bytes = Vec::new();
    cold_read.file.read_to_end(&mut cold_bytes).unwrap();
    assert_eq!(cold_bytes, bytes);
    let entries: Vec<_> = fs::read_dir(&automatic_cache)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .collect();
    assert_eq!(entries.len(), 1);
    let cached_path = &entries[0];
    let cached_inode = fs::metadata(cached_path).unwrap().ino();
    assert!(
        reader
            .read_with_cache(b"/actual", &automatic_cache, &read_spool)
            .await
            .unwrap()
            .is_some()
    );
    for fault in [
        ScheduleFault::ExternalPreempt,
        ScheduleFault::AfterUnload,
        ScheduleFault::AfterMove,
    ] {
        run_fdb_owner_loaded_reads(
            &root.join(match fault {
                ScheduleFault::ExternalPreempt => "loaded-read",
                ScheduleFault::AfterUnload => "unload-loss",
                ScheduleFault::AfterMove => "move-uncertain",
            }),
            &library,
            Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap()),
            &serial,
            active,
            pool,
            bytes,
            fault,
        )
        .await;
    }
    assert_eq!(fs::metadata(cached_path).unwrap().ino(), cached_inode);
    fs::write(cached_path, b"invalid cached SQLite").unwrap();
    assert!(
        reader
            .read_with_cache(b"/actual", &automatic_cache, &read_spool)
            .await
            .is_err()
    );
    assert_eq!(fs::read(cached_path).unwrap(), b"invalid cached SQLite");
    fs::remove_file(cached_path).unwrap();
    assert!(
        reader
            .read_with_cache(b"/actual", &automatic_cache, &read_spool)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        ActiveMediaReader {
            device: &drive,
            ..reader
        }
        .read(b"/actual", &media_cache, &read_spool)
        .await
        .is_err()
    );

    let data_block = media_read.physical.extents[0].start_block as usize;
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let LogicalObject::Record(record) = &mut cartridge.partitions[1].objects[data_block] else {
            panic!("expected file data");
        };
        record[0] ^= 1;
    }));
    let digest_error = reader
        .read(b"/actual", &media_cache, &read_spool)
        .await
        .err()
        .unwrap();
    assert!(
        digest_error.to_string().contains("摘要不符"),
        "{digest_error}"
    );
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let LogicalObject::Record(record) = &mut cartridge.partitions[1].objects[data_block] else {
            panic!("expected file data");
        };
        record[0] ^= 1;
    }));
    let revoked_permission = Arc::new(Permission::default());
    assert!(revoked_permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let revoking_drive = GuardedTransport::new(
        Box::new(RevokeOnContent {
            inner: Box::new(library.drive(0)),
            permission: revoked_permission.clone(),
            content: bytes.to_vec(),
            round: 11,
        }),
        revoked_permission.clone(),
        11,
    );
    assert!(
        ActiveMediaReader {
            device: &revoking_drive,
            permission: &revoked_permission,
            ..reader
        }
        .read(b"/actual", &media_cache, &read_spool)
        .await
        .is_err()
    );
    fence(&library.drive_as(0, 72), ReservationKey::new(72, 11)).unwrap();
    assert!(
        reader
            .read(b"/actual", &media_cache, &read_spool)
            .await
            .is_err()
    );
    fence(&drive, key).unwrap();
    assert_eq!(fs::read_dir(&read_spool).unwrap().count(), 0);
    assert!(matches!(
        catalog
            .resolve_active_read_for_owner(&serial, b"/actual", &media_cache, active, 10)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert!(
        catalog
            .resolve_active_read_for_owner(
                &serial,
                b"/actual",
                &media_cache,
                ActiveLibraryRoute {
                    frozen_index: active.frozen_index + 1,
                    ..active
                },
                11,
            )
            .await
            .is_err()
    );
    let bad_cache = root.join("bad-current.extent.db");
    let mut bad_index = expected_current;
    bad_index.sha256 = [0; 32];
    assert!(
        VolumeExtentCache::build_verified_latest_from_media(
            &TapeDrive::new(&drive),
            &label,
            &RecoveryBudget::default(),
            &bad_cache,
            bad_index,
            current.incremental,
        )
        .is_err()
    );
    assert!(!bad_cache.exists());

    // Make the committed Full index larger than one tape block so the next
    // one-file S4 is an actual incremental construct with an auditable delta.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let padding: Vec<_> = (0..256)
        .map(|number| ActiveMediaUpload {
            intent: ActiveFileIntent {
                name: format!("pad-{number:03}").into_bytes(),
                file_id: Uuid::new_v4(),
                operation_id: Uuid::new_v4(),
                volume_generation: current.generation + 1,
                ..intent.clone()
            },
            spool: data_path.clone(),
        })
        .collect();
    let padded = commit_active_media_batch(&catalog, volume, Uuid::new_v4(), &padding)
        .await
        .unwrap();
    current = padded.current_index();
    let mut ordered = padded.files().to_vec();
    ordered.sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
    let padded_batch = catalog
        .active_batch_status(&serial, baseline.volume_uuid, current.generation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(padded_batch.pages, 2);
    assert!(!padded_batch.published);
    for file in &ordered {
        assert_eq!(
            catalog
                .prepare_observed_active_file(
                    &serial,
                    11,
                    padded_batch.batch_id,
                    &file.receipt,
                    &file.attributes,
                )
                .await
                .unwrap(),
            ActivePreparationOutcome::Prepared
        );
    }
    for page in 0..padded_batch.pages {
        assert_eq!(
            catalog
                .mark_observed_active_batch_page_prepared(
                    &serial,
                    11,
                    baseline.volume_uuid,
                    current.generation,
                    padded_batch.batch_id,
                    page,
                )
                .await
                .unwrap(),
            ActivePreparationOutcome::Prepared
        );
    }
    assert_eq!(
        catalog
            .seal_observed_active_preparation(
                &serial,
                11,
                baseline.volume_uuid,
                current.generation,
                padded_batch.batch_id,
            )
            .await
            .unwrap(),
        ActivePreparationOutcome::Prepared
    );
    assert_eq!(
        catalog
            .publish_observed_active_batch(
                &serial,
                11,
                baseline.volume_uuid,
                current.generation,
                padded_batch.batch_id,
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::Published
    );
    assert!(
        catalog
            .active_batch_status(&serial, baseline.volume_uuid, current.generation)
            .await
            .unwrap()
            .unwrap()
            .published
    );
    assert_eq!(
        catalog
            .finalize_published_active_batch_page(
                &serial,
                11,
                baseline.volume_uuid,
                current.generation,
                padded_batch.batch_id,
                0,
            )
            .await
            .unwrap(),
        ActiveFinalizationOutcome::Finalized
    );
    assert_eq!(
        catalog
            .materialize_published_active_file(
                &serial,
                11,
                padded_batch.batch_id,
                0,
                &ordered[0].receipt,
            )
            .await
            .unwrap(),
        ActiveMaterializationOutcome::Materialized
    );
    drop(padded);
    library.power_cut();
    let mut wrong_guard = LtfsVolume::mount(&drive).unwrap();
    wrong_guard.set_reservation_guard(Some(ReservationKey::new(99, 11)));
    assert!(
        resume_observed_active_media_batch(&catalog, wrong_guard, &serial, 11)
            .await
            .is_err()
    );
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    drop(
        reconcile_active_media_on_takeover(&catalog, volume, &serial, 11)
            .await
            .unwrap(),
    );
    let recovered_status = catalog
        .active_batch_status(&serial, baseline.volume_uuid, current.generation)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered_status.completed);
    assert!(!recovered_status.published);
    assert!(matches!(
        catalog
            .resolve_active_path(&serial, b"/pad-255")
            .await
            .unwrap(),
        Some(ResolvedPath::File { .. })
    ));
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    drop(
        resume_observed_active_media_batch(&catalog, volume, &serial, 11)
            .await
            .unwrap(),
    );

    // A sealed S4 can reach tape before FDB observes it. Rebuild the exact
    // manifest from the committed incremental index and retry the observation
    // before publishing, without appending the file a second time.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let sealed_intent = ActiveFileIntent {
        name: b"sealed-recovery".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        volume_generation: current.generation + 1,
        ..intent.clone()
    };
    assert_eq!(
        catalog.reserve_active_file(&sealed_intent).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/sealed-recovery", &mut Cursor::new(bytes))
        .unwrap();
    let sealed_receipt = prepare_active_file(&volume, "/sealed-recovery", &sealed_intent)
        .unwrap()
        .receipt;
    assert_eq!(
        catalog.complete_active_file(&sealed_receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    let sealed_batch = Uuid::new_v4();
    assert_eq!(
        catalog
            .stage_active_batch(sealed_batch, &[sealed_receipt])
            .await
            .unwrap(),
        ActiveBatchOutcome::Staged
    );
    volume.sync_batch().unwrap();
    drop(volume);
    library.power_cut();
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        current
    );
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let sealed_commit = recover_sealed_active_media_batch(&catalog, volume, &serial, 11)
        .await
        .unwrap();
    assert!(sealed_commit.current_index().incremental);
    drop(sealed_commit);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let sealed_commit = recover_sealed_active_media_batch(&catalog, volume, &serial, 11)
        .await
        .unwrap();
    current = sealed_commit.current_index();
    drop(sealed_commit);
    assert!(
        !catalog
            .active_batch_status(&serial, baseline.volume_uuid, current.generation)
            .await
            .unwrap()
            .unwrap()
            .published
    );
    let mut wrong_guard = LtfsVolume::mount(&drive).unwrap();
    wrong_guard.set_reservation_guard(Some(ReservationKey::new(99, 11)));
    assert!(
        resume_observed_active_media_batch(&catalog, wrong_guard, &serial, 11)
            .await
            .is_err()
    );
    assert!(
        !catalog
            .active_batch_status(&serial, baseline.volume_uuid, current.generation)
            .await
            .unwrap()
            .unwrap()
            .published
    );
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    drop(
        resume_observed_active_media_batch(&catalog, volume, &serial, 11)
            .await
            .unwrap(),
    );
    assert!(matches!(
        catalog
            .resolve_active_path(&serial, b"/sealed-recovery")
            .await
            .unwrap(),
        Some(ResolvedPath::File { .. })
    ));
    assert!(current.incremental);
    let incremental_cache = root.join("incremental-from-media.extent.db");
    VolumeExtentCache::build_verified_latest_from_media(
        &TapeDrive::new(&drive),
        &label,
        &RecoveryBudget::default(),
        &incremental_cache,
        ExpectedIndex {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
        },
        true,
    )
    .unwrap();
    assert_eq!(
        catalog
            .resolve_active_read(&serial, b"/sealed-recovery", &incremental_cache)
            .await
            .unwrap()
            .unwrap()
            .physical
            .length,
        bytes.len() as u64
    );
    let mut predecessors = Vec::new();
    for generation in (baseline.generation..current.generation).rev() {
        let source = catalog
            .observed_active_volume_generation(&serial, baseline.volume_uuid, generation)
            .await
            .unwrap()
            .unwrap();
        predecessors.push(source);
        if !source.incremental {
            break;
        }
    }
    assert!(!predecessors.is_empty());
    catalog
        .verify_active_index_predecessors(&serial, active, 11, current, &predecessors)
        .await
        .unwrap();
    let mut wrong_predecessors = predecessors.clone();
    wrong_predecessors[0].sha256[0] ^= 1;
    assert!(
        catalog
            .verify_active_index_predecessors(&serial, active, 11, current, &wrong_predecessors)
            .await
            .is_err()
    );
    assert!(
        catalog
            .verify_active_index_predecessors(&serial, active, 11, current, &[])
            .await
            .is_err()
    );
    assert!(matches!(
        catalog
            .verify_active_index_predecessors(&serial, active, 10, current, &predecessors)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    let mut delta_read = reader
        .read_with_cache(b"/sealed-recovery", &automatic_cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut delta_bytes = Vec::new();
    delta_read.file.read_to_end(&mut delta_bytes).unwrap();
    assert_eq!(delta_bytes, bytes);
    // A predecessor can still be a valid XML construct after corruption.
    // The current delta's digest and the selected file bytes remain unchanged;
    // its old Full must nevertheless match FDB before any handle is returned.
    let full_block = predecessors.last().unwrap().location.start_block as usize;
    let mut saved = Vec::new();
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let LogicalObject::Record(record) = &mut cartridge.partitions[1].objects[full_block] else {
            panic!("expected Full predecessor record");
        };
        saved = record.clone();
        let text = std::str::from_utf8(record).unwrap();
        assert!(text.contains("<readonly>false</readonly>"));
        *record = text
            .replacen("<readonly>false</readonly>", "<readonly>true</readonly>", 1)
            .into_bytes();
    }));
    let error = reader
        .read_with_cache(b"/sealed-recovery", &automatic_cache, &read_spool)
        .await
        .err()
        .expect("前驱变化不得返回内容句柄");
    assert!(error.to_string().contains("前驱"), "{error}");
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        cartridge.partitions[1].objects[full_block] = LogicalObject::Record(saved);
    }));

    // A new S4 generation can exist on tape while FDB still points to its
    // predecessor; the single takeover entry must both observe and publish it.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let orchestrated = ActiveFileIntent {
        name: b"orchestrated".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        volume_generation: current.generation + 1,
        ..intent.clone()
    };
    assert_eq!(
        catalog.reserve_active_file(&orchestrated).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/orchestrated", &mut Cursor::new(bytes))
        .unwrap();
    let receipt = prepare_active_file(&volume, "/orchestrated", &orchestrated)
        .unwrap()
        .receipt;
    assert_eq!(
        catalog.complete_active_file(&receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    assert_eq!(
        catalog
            .stage_active_batch(Uuid::new_v4(), &[receipt])
            .await
            .unwrap(),
        ActiveBatchOutcome::Staged
    );
    assert!(current.incremental);
    volume.sync().unwrap();
    assert!(!volume.committed_index_construct().incremental);
    drop(volume);
    library.power_cut();
    let stale_cache = root.join("stale-index-from-media.extent.db");
    let stale_result = VolumeExtentCache::build_verified_latest_from_media(
        &TapeDrive::new(&drive),
        &label,
        &RecoveryBudget::default(),
        &stale_cache,
        ExpectedIndex {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
        },
        true,
    );
    assert!(stale_result.is_err(), "旧索引不能冒充介质的当前索引");
    assert!(!stale_cache.exists());
    let unknown_head_cache = root.join("unknown-head-cache");
    assert!(
        reader
            .read_with_cache(b"/actual", &unknown_head_cache, &read_spool)
            .await
            .is_err()
    );
    assert!(!unknown_head_cache.exists());
    assert!(
        reader
            .read(b"/actual", &media_cache, &read_spool)
            .await
            .is_err(),
        "介质已有 FDB 未观察 S4 时不能返回文件内容"
    );
    let historical_cache = root.join("historical-index-from-media.extent.db");
    VolumeExtentCache::build_verified_from_media(
        &TapeDrive::new(&drive),
        &label,
        &RecoveryBudget::default(),
        &historical_cache,
        ExpectedIndex {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
        },
        true,
    )
    .unwrap();
    let historical_source = current;
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let reconciled = reconcile_active_media_on_takeover(&catalog, volume, &serial, 11)
        .await
        .unwrap();
    current = reconciled.index();
    drop(reconciled);
    assert_eq!(
        catalog
            .resolve_active_read(&serial, b"/sealed-recovery", &historical_cache)
            .await
            .unwrap()
            .unwrap()
            .physical
            .length,
        bytes.len() as u64
    );
    let historical_read = catalog
        .resolve_active_read_for_owner(&serial, b"/sealed-recovery", &historical_cache, active, 11)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(historical_read.source, historical_source);
    let mut historical_content = reader
        .read(b"/sealed-recovery", &historical_cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut historical_bytes = Vec::new();
    historical_content
        .file
        .read_to_end(&mut historical_bytes)
        .unwrap();
    assert_eq!(historical_bytes, bytes);
    let historical_source_lookup = catalog
        .resolve_active_read_source_for_owner(&serial, b"/sealed-recovery", active, 11)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(historical_source_lookup.source, historical_source);
    assert!(historical_source.incremental);
    let mut cold_history = reader
        .read_with_cache(b"/sealed-recovery", &automatic_cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut cold_history_bytes = Vec::new();
    cold_history
        .file
        .read_to_end(&mut cold_history_bytes)
        .unwrap();
    assert_eq!(cold_history_bytes, bytes);
    assert_eq!(
        fs::read_dir(&automatic_cache)
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "sqlite"))
            .count(),
        2
    );
    assert!(matches!(
        catalog
            .resolve_active_path(&serial, b"/orchestrated")
            .await
            .unwrap(),
        Some(ResolvedPath::File { .. })
    ));

    // A wrong upload digest is discovered after a real append. The failed
    // path must never write S4; after a simulated power cut loses the
    // unflushed tail, the next fenced owner can cancel the Q reservation.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let failed_intent = ActiveFileIntent {
        name: b"rejected".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        volume_generation: current.generation + 1,
        expected_digest: ContentDigest::Sha256([0; 32]),
        ..intent
    };
    let failure = commit_active_media_batch(
        &catalog,
        volume,
        Uuid::new_v4(),
        &[ActiveMediaUpload {
            intent: failed_intent.clone(),
            spool: data_path,
        }],
    )
    .await;
    assert!(failure.is_err());
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        current
    );
    assert!(
        catalog
            .pending_active_file_reservations(namespace, None, 10)
            .await
            .unwrap()
            .items
            .contains(&failed_intent)
    );
    library.power_cut();
    let takeover_key = ReservationKey::new(72, 12);
    fence(&drive, takeover_key).unwrap();
    let mut recovered = LtfsVolume::mount(&drive).unwrap();
    recovered.set_reservation_guard(Some(takeover_key));
    assert_eq!(
        ActiveIndexIdentity::from(recovered.verified_committed_index_construct().unwrap()),
        current
    );
    assert!(
        establish_active_media_owner(&catalog, recovered, &serial, takeover_key, &permission)
            .await
            .is_err()
    );
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 11).await.unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    assert!(permission.renew(
        12,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let unguarded = LtfsVolume::mount(&drive).unwrap();
    assert!(
        establish_active_media_owner(&catalog, unguarded, &serial, takeover_key, &permission)
            .await
            .is_err()
    );
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 11).await.unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    let guarded_drive_12 =
        GuardedTransport::new(Box::new(library.drive(0)), permission.clone(), 12);
    let detached = Permission::default();
    assert!(detached.renew(
        12,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let wrong_permission = LtfsVolume::mount(&guarded_drive_12).unwrap();
    assert!(
        establish_active_media_owner(&catalog, wrong_permission, &serial, takeover_key, &detached,)
            .await
            .is_err()
    );
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 11).await.unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    let wrong_pr = LtfsVolume::mount(&guarded_drive_12).unwrap();
    assert!(
        establish_active_media_owner(
            &catalog,
            wrong_pr,
            &serial,
            ReservationKey::new(99, 12),
            &permission,
        )
        .await
        .is_err()
    );
    assert_eq!(
        catalog.claim_active_owner_epoch(&serial, 11).await.unwrap(),
        ActiveOwnerOutcome::AlreadyCurrent
    );
    let recovered = LtfsVolume::mount(&guarded_drive_12).unwrap();
    let recovered =
        establish_active_media_owner(&catalog, recovered, &serial, takeover_key, &permission)
            .await
            .unwrap();
    assert_eq!(
        catalog
            .cancel_active_file_reservation(&failed_intent)
            .await
            .unwrap(),
        PublishOutcome::StaleOwner
    );
    let takeover_intent = ActiveFileIntent {
        owner_epoch: 12,
        ..failed_intent
    };
    assert_eq!(recovered.index(), current);
    assert!(
        catalog
            .pending_active_file_reservations(namespace, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    drop(recovered);

    // This time the unindexed data really reached the medium. The new owner
    // must close it into an empty generation before releasing Q/I claims.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(takeover_key));
    let persistent_intent = ActiveFileIntent {
        name: b"persistent-tail".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        ..takeover_intent
    };
    assert!(
        commit_active_media_batch(
            &catalog,
            volume,
            Uuid::new_v4(),
            &[ActiveMediaUpload {
                intent: persistent_intent.clone(),
                spool: root.join("upload.bin"),
            }],
        )
        .await
        .is_err()
    );
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let partition = &mut cartridge.partitions[1];
        partition.flushed = partition.objects.len();
    }));
    library.power_cut();
    let final_key = ReservationKey::new(73, 13);
    fence(&drive, final_key).unwrap();
    assert!(permission.renew(
        13,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let guarded_drive_13 =
        GuardedTransport::new(Box::new(library.drive(0)), permission.clone(), 13);
    let volume = LtfsVolume::mount(&guarded_drive_13).unwrap();
    assert!(matches!(
        volume.recovery().dp.tail,
        TailKind::UnindexedData | TailKind::TruncatedIndex
    ));
    let recovered = establish_active_media_owner(&catalog, volume, &serial, final_key, &permission)
        .await
        .unwrap();
    assert_eq!(recovered.index().generation, current.generation + 1);
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        recovered.index()
    );
    assert!(
        catalog
            .pending_active_volume_reservations(
                &serial,
                baseline.volume_uuid,
                current.generation + 1,
                None,
                10,
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        catalog
            .resolve_active_path(&serial, b"/persistent-tail")
            .await
            .unwrap()
            .is_none()
    );
    let recovery_index = recovered.index();
    drop(recovered);

    // An upgraded I receipt can also be stranded before batch sealing. Crash
    // after close_tail S4 but before FDB observation, then let owner 15 resume.
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(final_key));
    let upgraded_intent = ActiveFileIntent {
        name: b"upgraded-tail".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        owner_epoch: 13,
        volume_generation: recovery_index.generation + 1,
        expected_digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
        ..persistent_intent
    };
    assert_eq!(
        catalog.reserve_active_file(&upgraded_intent).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/upgraded-tail", &mut Cursor::new(bytes))
        .unwrap();
    let prepared = prepare_active_file(&volume, "/upgraded-tail", &upgraded_intent).unwrap();
    assert_eq!(
        catalog
            .complete_active_file(&prepared.receipt)
            .await
            .unwrap(),
        PublishOutcome::Registered
    );
    drop(volume);
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let partition = &mut cartridge.partitions[1];
        partition.flushed = partition.objects.len();
    }));
    library.power_cut();
    assert!(catalog.advance_owner_epoch(&serial, 14).await.unwrap());
    let crash_key = ReservationKey::new(74, 14);
    fence(&drive, crash_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(crash_key));
    assert!(matches!(
        catalog
            .begin_active_volume_recovery(&serial, 14, recovery_index)
            .await
            .unwrap(),
        tape_rs::managed_catalog::ActiveRecoveryOutcome::Begun
    ));
    let blocked = ActiveFileIntent {
        name: b"blocked-during-recovery".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        owner_epoch: 14,
        ..upgraded_intent.clone()
    };
    assert_eq!(
        catalog.reserve_active_file(&blocked).await.unwrap(),
        PublishOutcome::VolumeNotReady
    );
    volume.close_tail(TailPolicy::Discard).unwrap();
    drop(volume);
    library.power_cut();
    assert!(catalog.advance_owner_epoch(&serial, 15).await.unwrap());
    let resume_key = ReservationKey::new(75, 15);
    fence(&drive, resume_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    assert_eq!(volume.recovery().dp.tail, TailKind::Complete);
    volume.set_reservation_guard(Some(resume_key));
    let recovered = recover_active_media_tail(&catalog, volume, &serial, 15)
        .await
        .unwrap();
    assert_eq!(recovered.index().generation, recovery_index.generation + 1);
    assert!(
        catalog
            .pending_active_volume_reservations(
                &serial,
                baseline.volume_uuid,
                recovered.index().generation,
                None,
                10
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        catalog
            .resolve_active_path(&serial, b"/upgraded-tail")
            .await
            .unwrap()
            .is_none()
    );
    drop(recovered);

    // Model a crash after the Open batch page moved Q into batch claims but
    // before the final Sealed header transaction. The I reverse index must
    // survive that page so the new owner can abort and release its permit.
    let open_previous = catalog
        .observed_active_volume_index(&serial, baseline.volume_uuid)
        .await
        .unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(resume_key));
    let open_intent = ActiveFileIntent {
        name: b"open-tail".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        owner_epoch: 15,
        volume_generation: open_previous.generation + 1,
        ..upgraded_intent
    };
    assert_eq!(
        catalog.reserve_active_file(&open_intent).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/open-tail", &mut Cursor::new(bytes))
        .unwrap();
    let open_receipt = prepare_active_file(&volume, "/open-tail", &open_intent)
        .unwrap()
        .receipt;
    assert_eq!(
        catalog.complete_active_file(&open_receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    let open_batch = Uuid::new_v4();
    assert_eq!(
        catalog
            .stage_active_batch(open_batch, &[open_receipt])
            .await
            .unwrap(),
        ActiveBatchOutcome::Staged
    );
    assert!(
        catalog
            .pending_active_volume_reservations(
                &serial,
                baseline.volume_uuid,
                open_intent.volume_generation,
                None,
                10
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        catalog
            .pending_active_volume_upgrades(
                &serial,
                baseline.volume_uuid,
                open_intent.volume_generation,
                None,
                10
            )
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let mut header_key = b"tape-rs/catalog/22/p/b/".to_vec();
    header_key.extend_from_slice(baseline.volume_uuid.as_bytes());
    header_key.extend_from_slice(&open_intent.volume_generation.to_be_bytes());
    let direct = foundationdb::Database::new(Some(&cluster)).unwrap();
    let tr = direct.create_trx().unwrap();
    let mut header = tr.get(&header_key, false).await.unwrap().unwrap().to_vec();
    assert_eq!(header[1], ActiveBatchPhase::Sealed as u8);
    header[1] = ActiveBatchPhase::Open as u8;
    tr.set(&header_key, &header);
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .active_batch_status(&serial, baseline.volume_uuid, open_intent.volume_generation)
            .await
            .unwrap()
            .unwrap()
            .phase,
        ActiveBatchPhase::Open
    );
    drop(volume);
    assert!(library.with_cartridge_mut(barcode, |cartridge| {
        let partition = &mut cartridge.partitions[1];
        partition.flushed = partition.objects.len();
    }));
    library.power_cut();
    assert!(catalog.advance_owner_epoch(&serial, 16).await.unwrap());
    let open_key = ReservationKey::new(76, 16);
    fence(&drive, open_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    assert_eq!(volume.recovery().dp.tail, TailKind::UnindexedData);
    volume.set_reservation_guard(Some(open_key));
    let recovered = recover_active_media_tail(&catalog, volume, &serial, 16)
        .await
        .unwrap();
    assert_eq!(recovered.index().generation, open_previous.generation + 1);
    assert!(
        catalog
            .active_batch_status(&serial, baseline.volume_uuid, open_intent.volume_generation)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .pending_active_volume_upgrades(
                &serial,
                baseline.volume_uuid,
                open_intent.volume_generation,
                None,
                10
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    drop(recovered);

    // A Full S4 may be durable while FDB still observes the preceding
    // generation. The new owner must prove that its complete file difference
    // is exactly the sealed batch before recording the generation.
    let before_full = catalog
        .observed_active_volume_index(&serial, baseline.volume_uuid)
        .await
        .unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(open_key));
    let full_intent = ActiveFileIntent {
        name: b"full-recovery".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        owner_epoch: 16,
        volume_generation: before_full.generation + 1,
        ..open_intent.clone()
    };
    assert_eq!(
        catalog.reserve_active_file(&full_intent).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/full-recovery", &mut Cursor::new(bytes))
        .unwrap();
    let full_receipt = prepare_active_file(&volume, "/full-recovery", &full_intent)
        .unwrap()
        .receipt;
    assert_eq!(
        catalog.complete_active_file(&full_receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    let full_batch = Uuid::new_v4();
    assert_eq!(
        catalog
            .stage_active_batch(full_batch, &[full_receipt])
            .await
            .unwrap(),
        ActiveBatchOutcome::Staged
    );
    volume.sync().unwrap();
    assert!(!volume.committed_index_construct().incremental);
    drop(volume);
    library.power_cut();
    assert!(catalog.advance_owner_epoch(&serial, 17).await.unwrap());
    let full_key = ReservationKey::new(77, 17);
    fence(&drive, full_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(full_key));
    let full_commit = recover_sealed_active_media_batch(&catalog, volume, &serial, 17)
        .await
        .unwrap();
    assert!(!full_commit.current_index().incremental);
    let full_commit = publish_active_media_batch(&catalog, full_commit)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .resolve_active_path(&serial, b"/full-recovery")
            .await
            .unwrap(),
        Some(ResolvedPath::File { .. })
    ));
    drop(full_commit);

    // A committed generation with a file and no matching empty-recovery
    // marker remains unknown to FDB and must never be silently discarded.
    let observed_before_unknown = catalog
        .observed_active_volume_index(&serial, baseline.volume_uuid)
        .await
        .unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(full_key));
    let hidden_intent = ActiveFileIntent {
        name: b"sealed-with-hidden".to_vec(),
        file_id: Uuid::new_v4(),
        operation_id: Uuid::new_v4(),
        owner_epoch: 17,
        volume_generation: observed_before_unknown.generation + 1,
        ..full_intent.clone()
    };
    assert_eq!(
        catalog.reserve_active_file(&hidden_intent).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/sealed-with-hidden", &mut Cursor::new(bytes))
        .unwrap();
    let hidden_receipt = prepare_active_file(&volume, "/sealed-with-hidden", &hidden_intent)
        .unwrap()
        .receipt;
    assert_eq!(
        catalog.complete_active_file(&hidden_receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    assert_eq!(
        catalog
            .stage_active_batch(Uuid::new_v4(), &[hidden_receipt])
            .await
            .unwrap(),
        ActiveBatchOutcome::Staged
    );
    volume
        .append_file("/unknown-s4", &mut Cursor::new(bytes))
        .unwrap();
    volume.sync().unwrap();
    drop(volume);
    library.power_cut();
    assert!(catalog.advance_owner_epoch(&serial, 18).await.unwrap());
    let unknown_key = ReservationKey::new(78, 18);
    fence(&drive, unknown_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(unknown_key));
    let error = match recover_sealed_active_media_batch(&catalog, volume, &serial, 18).await {
        Ok(_) => panic!("额外介质文件不得登记为封存批次"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("封存批次与介质文件差异不一致"),
        "{error}"
    );
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(unknown_key));
    assert!(
        recover_active_media_tail(&catalog, volume, &serial, 18)
            .await
            .is_err()
    );
    assert_eq!(
        catalog
            .observed_active_volume_index(&serial, baseline.volume_uuid)
            .await
            .unwrap(),
        observed_before_unknown
    );

    let stale_status = Arc::new(Mutex::new(NodeStatus::default()));
    stale_status.lock().unwrap().catalog_frozen_index = Some(active.frozen_index);
    let stale_permission = Arc::new(Permission::default());
    let (stale_tx, stale_rx) = channel();
    let (stale_events, stale_event_rx) = channel();
    let stale_handle = std::thread::spawn({
        let library = library.clone();
        let cluster = cluster.clone();
        let serial = serial.clone();
        let files = FileService::new(root.join("stale-head-spool")).unwrap();
        move || {
            tape_rs::daemon::executor::run_fdb_owner(
                Box::new(FdbOwnerSimProvider {
                    library,
                    node: 75,
                    enabled: Arc::new(AtomicBool::new(true)),
                }),
                ExecOptions {
                    node_id: 75,
                    interval: Duration::from_millis(100),
                    demo_write: false,
                    salvage: false,
                    block_size: 64 * 1024,
                    read_idle: Duration::from_secs(1),
                },
                stale_rx,
                stale_events,
                files,
                stale_status,
                FdbOwnerControl {
                    storage: Default::default(),
                    catalog: Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap()),
                    serial,
                    route: active,
                },
            );
        }
    });
    stale_tx
        .send(ExecRequest::AttachPermission {
            permission: stale_permission.clone(),
        })
        .unwrap();
    assert!(stale_permission.renew(
        19,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(120)
    ));
    stale_tx.send(ExecRequest::Takeover { round: 19 }).unwrap();
    assert!(matches!(
        next_owner_event(&stale_event_rx),
        ExecEvent::Fenced { round: 19 }
    ));
    assert!(stale_permission.renew(
        19,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    stale_tx.send(ExecRequest::Recover { round: 19 }).unwrap();
    let stale = next_owner_event(&stale_event_rx);
    assert!(
        matches!(&stale, ExecEvent::Lost { round: 19, reason } if reason.contains("FDB 索引不是介质末索引")),
        "{stale:?}"
    );
    let (done_tx, done_rx) = channel();
    stale_tx
        .send(ExecRequest::Shutdown {
            done: Some(done_tx),
        })
        .unwrap();
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    stale_handle.join().unwrap();

    // The tape above deliberately has an unobserved S4. Control-plane Raft
    // failover uses a separate empty device group; it must not classify that
    // newer tape head as ready merely to make an owner election pass.
    let control_library = SimLibrary::new(1, 4, 1);
    control_library.enable_changer_pr();
    fence(&control_library.drive(0), unknown_key).unwrap();
    run_fdb_owner_raft_failover(&cluster, &root, &serial, active, &control_library, &catalog).await;

    drop(db);
    drop(spool);
    fs::remove_dir_all(root).unwrap();
}

#[path = "support/fdb_owner_scheduling.rs"]
mod owner_scheduling;
