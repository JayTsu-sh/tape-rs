use super::*;
use crate::daemon::fdb_reclaim_mechanics::ReclaimMechanics;
use crate::managed_catalog::{
    ManagedCatalog, ManagementRequest, ReclaimMediaAction, ReclaimMediaPlan, ReclaimReservation,
    ReclaimSpec,
};
use crate::scsi::{device::ScsiResult, sense::SenseInfo};
use std::cell::Cell;
use std::ops::RangeInclusive;

/// Supply current 02/04/01 for selected TUR observations only. Real simulated
/// media state, PR responses and CDB traces are supplied by the original device.
struct BecomingReady<'a> {
    inner: &'a dyn TapeTransport,
    transient: RangeInclusive<usize>,
    observed: Cell<usize>,
    after_first_transient: Cell<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl<'a> BecomingReady<'a> {
    fn new(inner: &'a dyn TapeTransport, transient: RangeInclusive<usize>) -> Self {
        Self {
            inner,
            transient,
            observed: Cell::new(0),
            after_first_transient: Cell::new(None),
        }
    }
}

impl TapeTransport for BecomingReady<'_> {
    fn identity(&self) -> &str {
        self.inner.identity()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }

    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        if cdb[0] == 0x00 {
            let index = self.observed.get() + 1;
            self.observed.set(index);
            if self.transient.contains(&index) {
                if let Some(sender) = self.after_first_transient.take() {
                    sender.send(()).unwrap();
                }
                let raw = [0x70, 0, 2, 0, 0, 0, 0, 10, 0, 0, 0, 0, 4, 1, 0, 0, 0, 0];
                return Err(TapeError::ScsiCommand {
                    status: 2,
                    sense_key: 2,
                    asc: 4,
                    ascq: 1,
                    sense: Some(Box::new(SenseInfo::from_bytes(&raw))),
                });
            }
        }
        self.inner.execute_no_data(cdb, timeout)
    }

    fn execute_read(&self, cdb: &[u8], bytes: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_read(cdb, bytes, timeout)
    }

    fn execute_write(&self, cdb: &[u8], bytes: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_write(cdb, bytes, timeout)
    }
}

fn loaded_parent(
    catalog: Arc<ManagedCatalog>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = (Fixture, FdbOwnerControl, ReclaimReservation)>>,
> {
    Box::pin(async move {
        let (mut fixture, control, pool, status) =
            Box::pin(native_fixture_from(catalog.clone(), None, Fixture::new())).await;
        fixture.format(uuid::Uuid::new_v4());
        let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
        volume.append_bytes("/data", b"original source").unwrap();
        volume.commit().unwrap();
        drop(volume);
        let request = |change: Value| {
            ManagementRequest::parse(json!({
                "version":1,"library":control.serial,
                "namespace":control.route.route.namespace.to_string(),"epoch":1,
                "operation":uuid::Uuid::new_v4().to_string(),"change":change
            }))
            .unwrap()
        };
        let home = request(json!({
            "kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,
            "revision":null,"destination":"homeslot"
        }));
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(json!({
            "kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8",
            "volume":null,"path":"/","merge_tapes":true
        }));
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let spec = ReclaimSpec::parse(&json!({
            "kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,
            "volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,
            "confirm_format":true,"unassign":false
        }))
        .unwrap();
        let original = request(spec.to_json());
        catalog
            .admit_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                &original,
            )
            .await
            .unwrap();
        let parent = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        (fixture, control, parent)
    })
}

fn original_load<'a>(
    catalog: &'a ManagedCatalog,
    fixture: &'a Fixture,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
) -> (ReclaimMechanics<'a>, ReclaimMediaPlan) {
    let changer = fixture
        .owner
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .unwrap()
        .dev
        .as_ref();
    let mut robot = MediumChanger::new(changer);
    let map = robot.load_address_map().unwrap().clone();
    let rows = robot.read_all_status().unwrap();
    let row = rows
        .iter()
        .find(|r| r.full && r.volume_tag.as_deref() == Some(parent.spec().barcode()))
        .unwrap();
    assert_eq!(row.address, map.dt_start, "原源带在真实模拟驱动内");
    assert!(TapeDrive::new(fixture.device()).test_unit_ready().unwrap());
    let plan = ReclaimMediaPlan {
        action: ReclaimMediaAction::Load,
        barcode: parent.spec().barcode().into(),
        volume: parent.spec().volume(),
        drive_serial: fixture.owner.devices[0].serial.clone(),
        source: row.address,
        destination: row.address,
        home: row.source_address.unwrap(),
        map,
    };
    let actor = ReclaimMechanics {
        catalog,
        changer,
        drive: fixture.device(),
        route: control.route,
        key: fixture.owner.key,
        permission: &fixture.permit,
    };
    (actor, plan)
}

async fn load_wait_observes_original(
    catalog: &ManagedCatalog,
    fixture: &Fixture,
    control: &FdbOwnerControl,
    parent: &ReclaimReservation,
) {
    let _heartbeat = ParentPermitHeartbeat::new(fixture);
    let (actor, plan) = original_load(catalog, fixture, control, parent);
    let owner = fixture.owner.round;
    let original = catalog
        .reserved_reclaim_status(parent, owner)
        .await
        .unwrap();

    // One actual production sleep and a fresh real GOOD TUR, without issue.
    let drive_before = fixture.drive_log().len();
    let changer_before = fixture.changer_log().len();
    let transient = BecomingReady::new(fixture.device(), 1..=1);
    let transient_actor = ReclaimMechanics {
        drive: &transient,
        ..actor
    };
    let started = Instant::now();
    transient_actor.execute(parent, &plan).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(transient.observed.get(), 2);
    assert!(!fixture.drive_log()[drive_before..].contains(&0x1b));
    assert!(!fixture.changer_log()[changer_before..].contains(&0xa5));
    assert!(
        catalog
            .reclaim_media_pending(parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .reclaim_confirmed_media(parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog
            .reserved_reclaim_status(parent, owner)
            .await
            .unwrap(),
        original
    );

    // Exhaust one production budget. No local or durable command is issued.
    let persistent = BecomingReady::new(fixture.device(), 1..=usize::MAX);
    let persistent_actor = ReclaimMechanics {
        drive: &persistent,
        ..actor
    };
    let started = Instant::now();
    let why = persistent_actor.execute(parent, &plan).await.unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(30));
    assert_eq!(persistent.observed.get(), 5);
    let TapeError::NotReady(message) = why else {
        panic!("应保留原 Load 等待耗尽诊断: {why}");
    };
    assert!(message.contains(&format!("owner={owner}")));
    assert!(message.contains(parent.intent()["operation"].as_str().unwrap()));
    assert!(message.contains(&plan.drive_serial));
    assert!(message.contains("asc=0x04, ascq=0x01"));
    assert!(!fixture.drive_log()[drive_before..].contains(&0x1b));
    assert!(!fixture.changer_log()[changer_before..].contains(&0xa5));
    assert!(
        catalog
            .reclaim_media_pending(parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .reclaim_confirmed_media(parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog
            .reserved_reclaim_status(parent, owner)
            .await
            .unwrap(),
        original
    );

    // Move the drive to the known unloaded state using the original command.
    let unload = ReclaimMediaPlan {
        action: ReclaimMediaAction::Unload,
        ..plan.clone()
    };
    actor.execute(parent, &unload).await.unwrap();
    let before_issue = fixture.drive_log().len();
    let changer_before_issue = fixture.changer_log().len();
    let after_issue = BecomingReady::new(fixture.device(), 2..=usize::MAX);
    let issued_actor = ReclaimMechanics {
        drive: &after_issue,
        ..actor
    };
    assert!(issued_actor.execute(parent, &plan).await.is_err());
    assert_eq!(after_issue.observed.get(), 2);
    let pending = catalog
        .reclaim_media_pending(parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.owner(), owner);
    assert_eq!(pending.plan(), &plan);
    assert!(issued_actor.execute(parent, &plan).await.is_err());
    assert_eq!(after_issue.observed.get(), 3);
    assert_eq!(
        catalog
            .reclaim_media_pending(parent, owner)
            .await
            .unwrap()
            .as_ref(),
        Some(&pending)
    );
    assert!(!fixture.drive_log()[before_issue..].contains(&0x1b));
    assert!(!fixture.changer_log()[changer_before_issue..].contains(&0xa5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际原 reclaim 机械执行及 Sim TUR，不访问物理设备"]
async fn reclaim_load_wait_preserves_pre_issue_budget_and_original_pending() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let (fixture, control, parent) = loaded_parent(catalog.clone()).await;
    Box::pin(load_wait_observes_original(
        &catalog, &fixture, &control, &parent,
    ))
    .await;
}

/// The signal is sent in a synchronous guarded TUR. The biased select cannot
/// see it until execute yields, after its final scope/PR check and activity drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 等待期间真实许可关闭，不访问物理设备"]
async fn reclaim_load_wait_stops_before_new_cdb_when_permission_closes() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let (fixture, control, parent) = loaded_parent(catalog.clone()).await;
    let owner = fixture.owner.round;
    let (actor, plan) = original_load(&catalog, &fixture, &control, &parent);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let transient = BecomingReady::new(fixture.device(), 1..=usize::MAX);
    transient.after_first_transient.set(Some(sender));
    let actor = ReclaimMechanics {
        drive: &transient,
        ..actor
    };
    let mut call = Box::pin(actor.execute(&parent, &plan));
    tokio::select! {
        biased;
        result = &mut call => panic!("未进入等待就结束: {result:?}"),
        signal = receiver => signal.unwrap(),
    }
    assert_eq!(transient.observed.get(), 1);
    fixture.permit.close(owner);
    let drive_log = fixture.drive_log();
    let changer_log = fixture.changer_log();
    let why = call.await.unwrap_err();
    assert!(why.to_string().contains("多数派许可"));
    assert_eq!(transient.observed.get(), 1);
    assert_eq!(fixture.drive_log(), drive_log);
    assert_eq!(fixture.changer_log(), changer_log);
    assert!(
        catalog
            .reclaim_media_pending(&parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .reclaim_confirmed_media(&parent, owner)
            .await
            .unwrap()
            .is_none()
    );
}

struct LostPreparationMove {
    inner: Box<dyn TapeTransport + Send>,
    first: std::sync::atomic::AtomicBool,
}

impl TapeTransport for LostPreparationMove {
    fn identity(&self) -> &str {
        self.inner.identity()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }

    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        let result = self.inner.execute_no_data(cdb, timeout)?;
        if cdb[0] == crate::scsi::cdb::opcode::MOVE_MEDIUM
            && self.first.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(TapeError::ScsiHost(3));
        }
        Ok(result)
    }

    fn execute_read(&self, cdb: &[u8], bytes: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_read(cdb, bytes, timeout)
    }

    fn execute_write(&self, cdb: &[u8], bytes: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_write(cdb, bytes, timeout)
    }
}

async fn preparation_fixture(
    catalog: Arc<ManagedCatalog>,
) -> (Fixture, FdbOwnerControl, SharedStatus, ReclaimReservation) {
    let (mut fixture, control, pool, status) = Box::pin(native_fixture_from(
        catalog.clone(),
        None,
        Fixture::with_drives(2),
    ))
    .await;
    fixture
        .lib
        .insert_cartridge(SimCartridge::blank("LAYOUTL8", 128 << 20), 1)
        .unwrap();
    let request = |change| {
        ManagementRequest::parse(json!({
            "version":1, "library":control.serial, "namespace":control.route.route.namespace.to_string(),
            "epoch":1, "operation":uuid::Uuid::new_v4().to_string(), "change":change
        }))
        .unwrap()
    };
    for barcode in ["LIFE00L8", "LAYOUTL8"] {
        let assign = request(json!({
            "kind":"media-assign", "pool":pool.id.to_string(), "barcode":barcode,
            "volume":uuid::Uuid::new_v4().to_string(), "confirm_format":true, "force_format":false
        }));
        assert_eq!(
            execute(&mut fixture, &control, &status, &assign).await["status"],
            "succeeded"
        );
    }
    // Return both simulated tapes home before measuring the reclaim path.
    let device = fixture
        .owner
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Changer)
        .unwrap();
    let mut changer = MediumChanger::new(device.dev.as_ref());
    changer.load_address_map().unwrap();
    let rows = changer.read_all_status().unwrap();
    let mapping = inventory_elements(&fixture.owner.devices, &rows, true).unwrap();
    for drive in mapping.drives.iter().filter(|d| d.loaded.is_some()) {
        let row = rows.iter().find(|r| r.address == drive.addr).unwrap();
        assert!(matches!(
            row.volume_tag.as_deref(),
            Some("LIFE00L8" | "LAYOUTL8")
        ));
        TapeDrive::new(fixture.owner.devices[drive.dev].dev.as_ref())
            .unload()
            .unwrap();
        changer
            .move_medium(drive.addr, row.source_address.unwrap())
            .unwrap();
    }

    let source = catalog
        .tape_assignment(&control.serial, "LIFE00L8")
        .await
        .unwrap()
        .unwrap();
    let original = request(json!({
        "kind":"media-reclaim", "pool":pool.id.to_string(), "barcode":source.barcode,
        "volume":source.volume_id.unwrap().to_string(), "revision":source.revision,
        "confirm_format":true, "unassign":false, "mode":"group-only"
    }));
    let owner = fixture.owner.round;
    catalog
        .admit_management(&control.serial, control.route, owner, &original)
        .await
        .unwrap();
    let parent = catalog
        .start_reclaim_management(
            &control.serial,
            control.route,
            owner,
            original.epoch(),
            original.operation(),
        )
        .await
        .unwrap()
        .unwrap();
    (fixture, control, status, parent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；原 group-only 父任务控制真实 Sim 双驱动，不访问物理设备"]
async fn group_only_places_destination_before_observing_source_load() {
    use crate::scsi::cdb::opcode::{MOVE_MEDIUM, TEST_UNIT_READY};

    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let (mut fixture, control, status, parent) =
        Box::pin(preparation_fixture(catalog.clone())).await;
    let _heartbeat = ParentPermitHeartbeat::new(&fixture);
    let before = fixture.commands.lock().unwrap().len();
    let result = Box::pin(fdb_reclaim::advance(
        &mut fixture.owner,
        &fixture.permit,
        &control,
        &status,
    ))
    .await;
    let owner = fixture.owner.round;
    let layout = catalog
        .reclaim_layout(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(layout.source.address, layout.destination.address);
    let last = catalog
        .reclaim_confirmed_media(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.plan().barcode, "LAYOUTL8");
    assert_eq!(last.plan().action, ReclaimMediaAction::Move);
    assert_eq!(last.plan().source, layout.destination.home);
    assert_eq!(last.plan().destination, layout.destination.address);

    let logged = fixture.commands.lock().unwrap();
    let trace = &logged[before..];
    let moves: Vec<_> = trace
        .iter()
        .enumerate()
        .filter_map(|(index, op)| (*op == (DeviceKind::Changer, MOVE_MEDIUM)).then_some(index))
        .collect();
    assert_eq!(moves.len(), 2, "reclaim copy preparation: {result:?}");
    // Source MOVE readback and both destination MOVE checks contribute three
    // TURs here. The source Load pre-issue check belongs after destination MOVE.
    let between = trace[moves[0] + 1..moves[1]]
        .iter()
        .filter(|op| **op == (DeviceKind::Drive, TEST_UNIT_READY))
        .count();
    assert_eq!(
        between, 3,
        "source ready wait must overlap destination MOVE"
    );
    let after = trace[moves[1] + 1..]
        .iter()
        .filter(|op| **op == (DeviceKind::Drive, TEST_UNIT_READY))
        .count();
    assert!(
        after >= 3,
        "destination MOVE readback and both LOAD checks must run: {result:?}"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColdLoadEvent {
    SourceLabelLocate,
    SourceRead,
    TargetWaiting,
    TargetNoLongerWaiting,
    TargetLoad,
}

#[derive(Default)]
struct ColdLoadTrace {
    events: std::sync::Mutex<Vec<ColdLoadEvent>>,
    changed: std::sync::Condvar,
}

impl ColdLoadTrace {
    fn record(&self, event: ColdLoadEvent) {
        self.events.lock().unwrap().push(event);
        self.changed.notify_all();
    }

    fn wait_for(&self, event: ColdLoadEvent) {
        let _ = self
            .changed
            .wait_timeout_while(
                self.events.lock().unwrap(),
                Duration::from_secs(5),
                |events| !events.contains(&event),
            )
            .unwrap();
    }
}

struct ColdLoadDrive {
    inner: Box<dyn TapeTransport + Send>,
    source: bool,
    manual_load: bool,
    observations: Cell<usize>,
    issued: Cell<bool>,
    trace: Arc<ColdLoadTrace>,
}

impl TapeTransport for ColdLoadDrive {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        use crate::scsi::cdb::opcode::{LOAD_UNLOAD, TEST_UNIT_READY};

        if self.source
            && cdb == crate::scsi::cdb::locate_16(0, crate::ltfs::volume::LABEL_BLOCK, true, false)
        {
            self.trace.record(ColdLoadEvent::SourceLabelLocate);
        }
        if !self.source && cdb[0] == TEST_UNIT_READY {
            let index = self.observations.get();
            self.observations.set(index + 1);
            if index == 3 {
                // The prior three observations belong to the destination MOVE.
                self.trace.record(ColdLoadEvent::TargetWaiting);
                return Err(not_ready_current(1));
            }
            if index == 4 {
                self.trace.wait_for(ColdLoadEvent::SourceRead);
                self.trace.record(ColdLoadEvent::TargetNoLongerWaiting);
            }
            if index >= 4 && self.manual_load && !self.issued.get() {
                return Err(not_ready_current(2));
            }
        }
        if !self.source && cdb[0] == LOAD_UNLOAD {
            self.trace.record(ColdLoadEvent::TargetLoad);
            self.issued.set(true);
        }
        self.inner.execute_no_data(cdb, timeout)
    }

    fn execute_read(&self, cdb: &[u8], bytes: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        if self.source && cdb[0] == crate::scsi::cdb::opcode::READ_6 {
            self.trace.record(ColdLoadEvent::SourceRead);
            if self.manual_load {
                // Keep a source READ in flight until the target has left its
                // transient state, then let the source finish cooperatively.
                self.trace.wait_for(ColdLoadEvent::TargetNoLongerWaiting);
            }
        }
        self.inner.execute_read(cdb, bytes, timeout)
    }

    fn execute_write(&self, cdb: &[u8], bytes: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_write(cdb, bytes, timeout)
    }

    fn identity(&self) -> &str {
        self.inner.identity()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }

    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.inner.medium_change_token()
    }
}

fn not_ready_current(ascq: u8) -> TapeError {
    let raw = [0x70, 0, 2, 0, 0, 0, 0, 10, 0, 0, 0, 0, 4, ascq, 0, 0, 0, 0];
    TapeError::ScsiCommand {
        status: 2,
        sense_key: 2,
        asc: 4,
        ascq,
        sense: Some(Box::new(SenseInfo::from_bytes(&raw))),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；原 group-only 冷装载的实际 Sim 并行只读"]
async fn group_only_captures_source_while_destination_becomes_ready() {
    Box::pin(cold_destination_ready(false)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；原 group-only 冷装载的实际 Sim 串行 LOAD 回退"]
async fn group_only_joins_source_capture_before_physical_destination_load() {
    Box::pin(cold_destination_ready(true)).await;
}

async fn cold_destination_ready(manual_load: bool) {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let (mut fixture, control, status, parent) =
        Box::pin(preparation_fixture(catalog.clone())).await;
    let _heartbeat = ParentPermitHeartbeat::new(&fixture);
    let trace = Arc::new(ColdLoadTrace::default());
    for (drive, device) in fixture.owner.devices[..2].iter_mut().enumerate() {
        let inner = std::mem::replace(&mut device.dev, Box::new(fixture.lib.drive_as(drive, 1)));
        device.dev = Box::new(ColdLoadDrive {
            inner,
            source: drive == 0,
            manual_load,
            observations: Cell::new(0),
            issued: Cell::new(false),
            trace: trace.clone(),
        });
    }
    let result = Box::pin(fdb_reclaim::advance(
        &mut fixture.owner,
        &fixture.permit,
        &control,
        &status,
    ))
    .await;
    let owner = fixture.owner.round;
    let layout = catalog
        .reclaim_layout(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(layout.source.drive_serial, "SIMDRV0000");
    assert_eq!(layout.destination.drive_serial, "SIMDRV0001");
    assert!(
        catalog
            .reclaim_media_pending(&parent, owner)
            .await
            .unwrap()
            .is_none(),
        "physical and durable load must settle before returning: {result:?}"
    );
    let last = catalog
        .reclaim_confirmed_media(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        last.plan().action,
        if manual_load {
            ReclaimMediaAction::Load
        } else {
            ReclaimMediaAction::Move
        }
    );
    let events = trace.events.lock().unwrap().clone();
    let index = |event| events.iter().position(|value| *value == event).unwrap();
    let wait = index(ColdLoadEvent::TargetWaiting);
    let read = index(ColdLoadEvent::SourceRead);
    let ready = index(ColdLoadEvent::TargetNoLongerWaiting);
    assert!(wait < read && read < ready, "{events:?}, result={result:?}");
    if manual_load {
        let load = index(ColdLoadEvent::TargetLoad);
        assert!(events[ready + 1..load].contains(&ColdLoadEvent::SourceRead));
        assert!(events[load + 1..].contains(&ColdLoadEvent::SourceRead));
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == ColdLoadEvent::TargetLoad)
                .count(),
            1
        );
    } else {
        assert!(!events.contains(&ColdLoadEvent::TargetLoad));
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| **e == ColdLoadEvent::SourceLabelLocate)
            .count(),
        if manual_load { 2 } else { 1 },
        "the ready path reuses one capture, and the LOAD path starts a fresh one",
    );
    assert!(
        catalog
            .reclaim_group_only_status(&parent, owner)
            .await
            .unwrap()
            .is_some(),
        "the empty source must reach original inventory sealing and durable qualification: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；原 group-only MOVE 已生效但响应丢失，不访问物理设备"]
async fn group_only_observes_lost_source_move_before_opening_destination() {
    use crate::scsi::cdb::opcode::{LOAD_UNLOAD, MOVE_MEDIUM};

    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let (mut fixture, control, status, parent) =
        Box::pin(preparation_fixture(catalog.clone())).await;
    let _heartbeat = ParentPermitHeartbeat::new(&fixture);
    let device = fixture
        .owner
        .devices
        .iter_mut()
        .find(|d| d.kind == DeviceKind::Changer)
        .unwrap();
    let inner = std::mem::replace(&mut device.dev, Box::new(fixture.lib.changer_as(1)));
    device.dev = Box::new(LostPreparationMove {
        inner,
        first: true.into(),
    });

    let before_move = fixture.changer_log().len();
    let before_drive = fixture.drive_log().len();
    let result = Box::pin(fdb_reclaim::advance(
        &mut fixture.owner,
        &fixture.permit,
        &control,
        &status,
    ))
    .await;
    assert!(
        result.is_err(),
        "lost source MOVE reply must stop the prepare step"
    );
    let owner = fixture.owner.round;
    let pending = catalog
        .reclaim_media_pending(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    let layout = catalog
        .reclaim_layout(&parent, owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.owner(), owner);
    assert_eq!(pending.plan().action, ReclaimMediaAction::Move);
    assert_eq!(pending.plan().barcode, "LIFE00L8");
    assert_eq!(pending.plan().source, layout.source.home);
    assert_eq!(pending.plan().destination, layout.source.address);
    assert_eq!(
        fixture.changer_log()[before_move..]
            .iter()
            .filter(|&&op| op == MOVE_MEDIUM)
            .count(),
        1
    );
    assert!(!fixture.drive_log()[before_drive..].contains(&LOAD_UNLOAD));

    // Recovery consumes the original issue using the real moved tape. The
    // second drive stays untouched until a later copy-preparation step.
    assert!(
        Box::pin(fdb_reclaim::advance(
            &mut fixture.owner,
            &fixture.permit,
            &control,
            &status
        ))
        .await
        .unwrap()
    );
    assert!(
        catalog
            .reclaim_media_pending(&parent, owner)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog
            .reclaim_confirmed_media(&parent, owner)
            .await
            .unwrap(),
        Some(pending)
    );
    assert_eq!(
        fixture.changer_log()[before_move..]
            .iter()
            .filter(|&&op| op == MOVE_MEDIUM)
            .count(),
        1
    );
    assert!(!fixture.drive_log()[before_drive..].contains(&LOAD_UNLOAD));
}
