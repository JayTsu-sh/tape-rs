#![cfg(feature = "fdb")]
//! Explicit private-FDB tests; devices are always in-memory simulators.
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tape_rs::daemon::{
    executor::{
        self, DeviceKind, DeviceProvider, ExecEvent, ExecOptions, ExecRequest, FdbOwnerControl,
        ManagedDevice,
    },
    files::FileService,
    node::NodeStatus,
    permission::{Permission, Phase},
};
use tape_rs::managed_catalog::{ActiveLibraryRoute, ManagedCatalog};
use tape_rs::scsi::sim::SimLibrary;
use uuid::Uuid;

struct Provider {
    library: SimLibrary,
    serial: String,
    opened: Arc<AtomicUsize>,
}
impl DeviceProvider for Provider {
    fn probe(&self) -> tape_rs::error::Result<usize> {
        Ok(1)
    }
    fn open_all(&self) -> tape_rs::error::Result<Vec<ManagedDevice>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(vec![
            ManagedDevice {
                name: "changer".into(),
                serial: self.serial.clone(),
                kind: DeviceKind::Changer,
                dev: Box::new(self.library.changer_as(2)),
            },
            ManagedDevice {
                name: "drive".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: Box::new(self.library.drive_as(0, 2)),
            },
        ])
    }
}
struct Attempt {
    event: ExecEvent,
    opened: usize,
    elapsed: Duration,
}
fn attempt(
    control: FdbOwnerControl,
    round: u64,
    status: NodeStatus,
    granted: bool,
    revoke: bool,
) -> Attempt {
    let root = std::env::temp_dir().join(format!("tapers-preflight-{}", Uuid::new_v4()));
    let files = FileService::new(root.join("spool")).unwrap();
    let permission = Arc::new(Permission::default());
    if granted {
        assert!(permission.renew(
            round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(30)
        ));
    }
    let opened = Arc::new(AtomicUsize::new(0));
    let library = SimLibrary::new(1, 1, 0);
    library.enable_changer_pr();
    let (requests, rx) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let provider = Provider {
        library,
        serial: control.serial.clone(),
        opened: opened.clone(),
    };
    let thread = std::thread::spawn(move || {
        executor::run_fdb_owner(
            Box::new(provider),
            ExecOptions {
                node_id: 2,
                interval: Duration::from_millis(20),
                demo_write: false,
                salvage: false,
                block_size: 65536,
                read_idle: Duration::from_secs(30),
            },
            rx,
            events,
            files,
            Arc::new(Mutex::new(status)),
            control,
        )
    });
    requests
        .send(ExecRequest::AttachPermission {
            permission: permission.clone(),
        })
        .unwrap();
    let start = Instant::now();
    requests.send(ExecRequest::Takeover { round }).unwrap();
    let revoker = revoke.then(|| {
        std::thread::spawn({
            let permission = permission.clone();
            move || {
                std::thread::sleep(Duration::from_millis(100));
                permission.close(round);
            }
        })
    });
    let event = loop {
        let event = event_rx.recv_timeout(Duration::from_secs(8)).unwrap();
        if !matches!(event, ExecEvent::Capability { .. }) {
            break event;
        }
    };
    let elapsed = start.elapsed();
    let (done, finished) = mpsc::channel();
    requests
        .send(ExecRequest::Shutdown { done: Some(done) })
        .unwrap();
    finished.recv_timeout(Duration::from_secs(8)).unwrap();
    thread.join().unwrap();
    if let Some(t) = revoker {
        t.join().unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
    Attempt {
        event,
        opened: opened.load(Ordering::SeqCst),
        elapsed,
    }
}
fn rejected(result: Attempt, round: u64) -> String {
    assert!(
        matches!(result.event,ExecEvent::FenceFailed {round:r,..} if r==round),
        "FDB不可用/事实不符时不得先Fenced: {:?}; device opens={}",
        result.event,
        result.opened
    );
    assert_eq!(result.opened, 0, "前置校验失败不能打开接管设备或修改PR");
    match result.event {
        ExecEvent::FenceFailed { reason, .. } => reason,
        _ => unreachable!(),
    }
}
#[test]
#[ignore = "需显式隔离FDB_TEST_CLUSTER；只模拟设备，验证前检失败不发起设备fencing"]
fn native_owner_prechecks_fdb_before_opening_or_fencing_devices() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("显式隔离FDB");
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("PRECHECK-{ns}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let proof =
        tape_rs::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
    let route = runtime
        .block_on(catalog.initialize_native_library(ns, &proof))
        .unwrap();
    let control = |catalog: Arc<ManagedCatalog>, route: ActiveLibraryRoute| FdbOwnerControl {
        catalog,
        serial: serial.clone(),
        route,
        storage: Default::default(),
    };
    let status = || NodeStatus {
        id: 2,
        catalog_frozen_index: Some(route.frozen_index),
        ..Default::default()
    };
    let root = std::env::temp_dir().join(format!("tapers-precheck-cluster-{ns}"));
    std::fs::create_dir(&root).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let path = root.join("unreachable.cluster");
    std::fs::write(
        &path,
        format!(
            "precheck:{}@{}",
            Uuid::new_v4().simple(),
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    let unavailable = Arc::new(ManagedCatalog::open(Some(path.to_str().unwrap())).unwrap());
    let round = route.initial_owner_epoch + 10;
    let result = attempt(
        control(unavailable.clone(), route),
        round,
        status(),
        true,
        false,
    );
    assert!(result.elapsed < Duration::from_secs(7));
    assert!(rejected(result, round).contains("FDB"));
    let result = attempt(control(unavailable, route), round + 1, status(), true, true);
    assert!(
        result.elapsed < Duration::from_secs(2),
        "失权应及时取消元数据等待"
    );
    rejected(result, round + 1);
    let mut wrong = route;
    wrong.route.namespace = Uuid::new_v4();
    rejected(
        attempt(
            control(catalog.clone(), wrong),
            round + 2,
            status(),
            true,
            false,
        ),
        round + 2,
    );
    let mut frozen = status();
    frozen.catalog_frozen_index = Some(route.frozen_index + 1);
    rejected(
        attempt(
            control(catalog.clone(), route),
            round + 3,
            frozen,
            true,
            false,
        ),
        round + 3,
    );
    let mut stopping = status();
    stopping.shutting_down = true;
    rejected(
        attempt(
            control(catalog.clone(), route),
            round + 4,
            stopping,
            true,
            false,
        ),
        round + 4,
    );
    let mut pending = status();
    pending.catalog_pending = true;
    rejected(
        attempt(
            control(catalog.clone(), route),
            round + 5,
            pending,
            true,
            false,
        ),
        round + 5,
    );
    rejected(
        attempt(
            control(catalog.clone(), route),
            round + 6,
            status(),
            false,
            false,
        ),
        round + 6,
    );
    runtime
        .block_on(catalog.claim_active_owner_epoch(&serial, round + 20))
        .unwrap();
    rejected(
        attempt(
            control(catalog.clone(), route),
            round + 19,
            status(),
            true,
            false,
        ),
        round + 19,
    );
    let healthy = attempt(
        control(catalog.clone(), route),
        round + 21,
        status(),
        true,
        false,
    );
    assert!(matches!(healthy.event,ExecEvent::Fenced {round:r} if r==round+21));
    assert_eq!(healthy.opened, 1);
    assert_eq!(
        runtime
            .block_on(catalog.active_library_owner(&serial))
            .unwrap(),
        Some((route, round + 20)),
        "前检不得提前claim FDB owner"
    );
    std::fs::remove_dir_all(root).unwrap();
}
