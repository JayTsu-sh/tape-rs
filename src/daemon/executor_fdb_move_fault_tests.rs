//! Private FDB plus real SimTransport effects. No paths to physical devices.
use super::super::fdb_observations_fixture::{make_status, options};
use super::*;
use crate::managed_catalog::{ManagedCatalog, verify_native_barriers};
use crate::scsi::{
    cdb::opcode,
    device::ScsiResult,
    sim::{SimCartridge, SimLibrary, SimTransport},
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[derive(Default)]
struct Faults {
    commands: Mutex<Vec<Vec<u8>>>,
    drop_move: AtomicBool,
    drop_unload: AtomicBool,
    revoke_unload: AtomicBool,
}
struct Trace {
    inner: SimTransport,
    faults: Arc<Faults>,
    permission: Arc<Permission>,
    round: u64,
}
impl TapeTransport for Trace {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.faults.commands.lock().unwrap().push(cdb.to_vec());
        let result = self.inner.execute_no_data(cdb, timeout)?;
        if cdb[0] == opcode::MOVE_MEDIUM && self.faults.drop_move.swap(false, Ordering::SeqCst) {
            return Err(TapeError::ScsiHost(3)); // actual effect already happened
        }
        if cdb[0] == opcode::LOAD_UNLOAD && cdb[4] == 0 {
            if self.faults.revoke_unload.swap(false, Ordering::SeqCst) {
                self.permission.close(self.round);
            }
            if self.faults.drop_unload.swap(false, Ordering::SeqCst) {
                return Err(TapeError::ScsiHost(3));
            }
        }
        Ok(result)
    }
    fn execute_read(&self, cdb: &[u8], buffer: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.faults.commands.lock().unwrap().push(cdb.to_vec());
        self.inner.execute_read(cdb, buffer, timeout)
    }
    fn execute_write(&self, cdb: &[u8], buffer: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.faults.commands.lock().unwrap().push(cdb.to_vec());
        self.inner.execute_write(cdb, buffer, timeout)
    }
}
struct Provider {
    library: SimLibrary,
    serial: String,
    faults: Arc<Faults>,
    permission: Arc<Permission>,
    round: u64,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let wrap = |inner| {
            Box::new(Trace {
                inner,
                faults: self.faults.clone(),
                permission: self.permission.clone(),
                round: self.round,
            }) as Box<dyn TapeTransport + Send>
        };
        Ok(vec![
            ManagedDevice {
                name: "changer".into(),
                serial: self.serial.clone(),
                kind: DeviceKind::Changer,
                dev: wrap(self.library.changer_as(2)),
            },
            ManagedDevice {
                name: "drive".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: wrap(self.library.drive_as(0, 2)),
            },
        ])
    }
}
fn enqueue(
    context: &Context<'_>,
    runtime: &tokio::runtime::Runtime,
    destination: &str,
) -> ManagementRequest {
    let c = context.control;
    let window = runtime
        .block_on(c.catalog.active_management_window(&c.serial, c.route))
        .unwrap();
    let request=ManagementRequest::parse(json!({"version":1,"library":c.serial,"namespace":c.route.route.namespace.to_string(),"epoch":window.accepting,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":destination}})).unwrap();
    runtime
        .block_on(
            c.catalog
                .admit_management(&c.serial, c.route, context.active.round, &request),
        )
        .unwrap();
    request
}
fn saved(context: &Context<'_>, runtime: &tokio::runtime::Runtime, r: &ManagementRequest) -> Value {
    runtime
        .block_on(context.control.catalog.management_task(
            &context.control.serial,
            context.control.route,
            r.epoch(),
            r.operation(),
        ))
        .unwrap()
        .unwrap()
}
fn finish(
    context: Context<'_>,
    runtime: &tokio::runtime::Runtime,
    job: &mut Job,
    publisher: &mut Publisher,
) -> Value {
    for _ in 0..14 {
        match job.poll(context, publisher, runtime).unwrap() {
            Publication::Confirmed(true) => return saved(&context, runtime, &job.request),
            Publication::Confirmed(false) => {}
            Publication::Failed {
                permanent: false, ..
            } => {}
            _ => panic!("unexpected publication"),
        }
    }
    panic!(
        "task did not settle: {}",
        saved(&context, runtime, &job.request)
    )
}
#[test]
#[ignore = "显式隔离FDB_TEST_CLUSTER；真实Sim副作用后丢回执/HA/maintenance，不访问物理设备"]
fn original_mechanical_checkpoint_survives_ack_loss_and_takeover_without_duplicate_moves() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("MOVE-FAULT-{ns}");
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
    let lib = SimLibrary::new(1, 3, 1);
    lib.enable_changer_pr();
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    let root = std::env::temp_dir().join(format!("tapers-move-fault-{ns}"));
    let files = FileService::new(root.join("spool")).unwrap();
    let permission = Arc::new(Permission::default());
    let faults = Arc::new(Faults::default());
    let provider = Provider {
        library: lib.clone(),
        serial: serial.clone(),
        faults: faults.clone(),
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
    let (map, _) = management::elements(&active.devices).unwrap();
    let status = make_status(2, round, route.frozen_index);
    let context = || Context {
        active: &active,
        permission: &permission,
        control: &control,
        status: &status,
        node: 2,
    };
    let request = enqueue(&context(), &runtime, "ieslot");
    let mut job = Job::new(request.clone(), &saved(&context(), &runtime, &request)).unwrap();
    let mut publisher = Publisher::default();
    faults.commands.lock().unwrap().clear();
    faults.drop_move.store(true, Ordering::SeqCst);
    // Prepare+phase ACK precedes actual MOVE. The delegate moved the cartridge,
    // but the wrapper returns a host error after that side effect.
    assert!(matches!(
        job.poll(context(), &mut publisher, &runtime).unwrap(),
        Publication::Confirmed(false)
    ));
    assert_eq!(
        saved(&context(), &runtime, &request)["execution"]["phase"],
        "moving"
    );
    assert_eq!(lib.loaded_barcode(0), None);
    assert!(matches!(
        runtime
            .block_on(catalog.require_active_library_readable(&serial, route, round))
            .unwrap_err()
            .domain(),
        CatalogError::PoolBusy
    ));
    let before_read = faults.commands.lock().unwrap().len();
    let reader = super::super::super::fdb_read::ActiveMediaReader {
        catalog: &catalog,
        device: active.devices[1].dev.as_ref(),
        serial: &serial,
        route,
        key: active.key,
        permission: &permission,
        storage: Default::default(),
    };
    assert!(matches!(
        runtime.block_on(reader.read_with_cache_scoped(
            b"/not-present",
            None,
            &root.join("cache"),
            &root
        )),
        Err(TapeError::MaintenanceBusy { .. })
    ));
    assert_eq!(
        faults.commands.lock().unwrap().len(),
        before_read,
        "maintenance在首个设备CDB前阻断读取"
    );
    let moved = finish(context(), &runtime, &mut job, &mut publisher);
    assert_eq!(moved["status"], "succeeded");
    assert_eq!(
        faults
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == opcode::MOVE_MEDIUM)
            .count(),
        1,
        "丢MOVE回执不得再次MOVE"
    );
    assert_eq!(
        faults
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == opcode::READ_ELEMENT_STATUS)
            .count(),
        4,
        "目标确认的原样本直接发布，不能另起一次设备采样"
    );
    runtime
        .block_on(catalog.require_active_library_readable(&serial, route, round))
        .unwrap();
    let count = faults.commands.lock().unwrap().len();
    assert!(matches!(
        job.poll(context(), &mut publisher, &runtime).unwrap(),
        Publication::Confirmed(true)
    ));
    assert_eq!(
        faults.commands.lock().unwrap().len(),
        count,
        "原终态重放无CDB"
    );
    let back = enqueue(&context(), &runtime, "homeslot");
    let mut back_job = Job::new(back.clone(), &saved(&context(), &runtime, &back)).unwrap();
    assert_eq!(
        finish(context(), &runtime, &mut back_job, &mut publisher)["status"],
        "succeeded"
    );
    // Simulate an ordinary loaded tape, then lose UNLOAD ACK. Source SVALID is
    // learned by the real simulated changer MOVE, not a synthetic task record.
    move_medium(&active.devices, map.storage_start, map.dt_start).unwrap();
    let home = enqueue(&context(), &runtime, "homeslot");
    let mut home_job = Job::new(home.clone(), &saved(&context(), &runtime, &home)).unwrap();
    faults.drop_unload.store(true, Ordering::SeqCst);
    faults.commands.lock().unwrap().clear();
    assert_eq!(
        finish(context(), &runtime, &mut home_job, &mut publisher)["status"],
        "succeeded"
    );
    let commands = faults.commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == opcode::MOVE_MEDIUM)
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == opcode::LOAD_UNLOAD && c[4] == 0)
            .count(),
        1,
        "UNLOAD已完成且Access=1，不盲重发"
    );
    drop(commands);
    assert!(home_job.evicted_drive.is_some());
    // A real FDB commit followed by loss of its local receipt. The unavailable
    // coordinator is a TCP blackhole, not a synthetic success/error result.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = root.join("unreachable.cluster");
    std::fs::write(
        &unreachable,
        format!(
            "move:{}@{}",
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
    let pending = enqueue(&context(), &runtime, "ieslot");
    let mut pending_job =
        Job::new(pending.clone(), &saved(&context(), &runtime, &pending)).unwrap();
    faults.commands.lock().unwrap().clear();
    assert!(matches!(
        pending_job
            .poll(
                Context {
                    control: &unavailable,
                    ..context()
                },
                &mut publisher,
                &runtime
            )
            .unwrap(),
        Publication::Failed {
            permanent: false,
            ..
        }
    ));
    assert!(
        faults.commands.lock().unwrap().is_empty(),
        "FDB不可用时准备前不采样或发机械CDB"
    );
    assert_eq!(saved(&context(), &runtime, &pending)["status"], "queued");
    let plan = Snapshot::capture(&context())
        .unwrap()
        .plan(&pending, None)
        .unwrap();
    runtime
        .block_on(catalog.prepare_media_move(&serial, route, round, &pending, &plan))
        .unwrap();
    runtime
        .block_on(catalog.stage_media_move(&serial, route, round, &pending, MovePhase::Moving))
        .unwrap();
    pending_job.plan = Some(plan.clone()); // drop both genuinely committed ACKs
    faults.commands.lock().unwrap().clear();
    assert!(matches!(
        pending_job
            .poll(
                Context {
                    control: &unavailable,
                    ..context()
                },
                &mut publisher,
                &runtime
            )
            .unwrap(),
        Publication::Failed {
            permanent: false,
            ..
        }
    ));
    assert!(
        faults.commands.lock().unwrap().is_empty(),
        "阶段回执未确认时不发MOVE"
    );
    let done = finish(context(), &runtime, &mut pending_job, &mut publisher);
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["execution"]["plan"], plan.value());
    assert_eq!(
        faults
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == opcode::MOVE_MEDIUM)
            .count(),
        1
    );
    faults.commands.lock().unwrap().clear();
    publisher.request_fresh();
    assert!(matches!(
        pending_job
            .poll(
                Context {
                    control: &unavailable,
                    ..context()
                },
                &mut publisher,
                &runtime
            )
            .unwrap(),
        Publication::Failed {
            permanent: false,
            ..
        }
    ));
    assert!(
        faults.commands.lock().unwrap().is_empty(),
        "真实已提交终态的ACK未知时保留原样本，不执行到期周期采样"
    );
    assert!(matches!(
        pending_job
            .poll(context(), &mut publisher, &runtime)
            .unwrap(),
        Publication::Confirmed(true)
    ));
    assert_eq!(saved(&context(), &runtime, &pending), done);
    assert!(faults.commands.lock().unwrap().is_empty());
    let back = enqueue(&context(), &runtime, "homeslot");
    let mut back_job = Job::new(back.clone(), &saved(&context(), &runtime, &back)).unwrap();
    assert_eq!(
        finish(context(), &runtime, &mut back_job, &mut publisher)["status"],
        "succeeded"
    );

    // Revoke precisely after UNLOAD: the original journal stays running and no
    // MOVE is sent under that old permission. The next fenced owner resumes it.
    move_medium(&active.devices, map.storage_start, map.dt_start).unwrap();
    let interrupted = enqueue(&context(), &runtime, "homeslot");
    let mut interrupted_job = Job::new(
        interrupted.clone(),
        &saved(&context(), &runtime, &interrupted),
    )
    .unwrap();
    faults.revoke_unload.store(true, Ordering::SeqCst);
    faults.commands.lock().unwrap().clear();
    assert!(matches!(
        interrupted_job.poll(context(), &mut publisher, &runtime),
        Err(TapeError::OwnershipLost { .. })
    ));
    assert!(matches!(
        interrupted_job.poll(context(), &mut publisher, &runtime),
        Ok(Publication::Paused)
    ));
    let original = saved(&context(), &runtime, &interrupted);
    assert_eq!(original["status"], "running");
    assert_eq!(original["execution"]["phase"], "unloading");
    assert_eq!(
        faults
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == opcode::MOVE_MEDIUM)
            .count(),
        0
    );
    let next = round + 1;
    drop(active);
    permission.renew(
        next,
        super::super::super::permission::Phase::Fencing,
        Instant::now() + Duration::from_secs(120),
    );
    let provider = Provider {
        round: next,
        ..provider
    };
    let mut active = takeover(
        &provider,
        &options(2),
        next,
        Some(&permission),
        files.clone(),
    )
    .unwrap();
    active.serving = true;
    runtime
        .block_on(catalog.claim_active_owner_epoch(&serial, next))
        .unwrap();
    permission.renew(
        next,
        super::super::super::permission::Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    let status = make_status(2, next, route.frozen_index);
    let context = Context {
        active: &active,
        permission: &permission,
        control: &control,
        status: &status,
        node: 2,
    };
    let rescan = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"library-rescan"}})).unwrap();
    runtime
        .block_on(catalog.admit_management(&serial, route, next, &rescan))
        .unwrap();
    let mut observer = super::super::fdb_rescan::Observer::default();
    faults.commands.lock().unwrap().clear();
    let mut receipt = None;
    for _ in 0..10 {
        observer.poll(context, &runtime).unwrap();
        assert_eq!(
            saved(&context, &runtime, &rescan)["status"],
            "queued",
            "必须优先恢复机械gate，不能先运行等待gate的rescan"
        );
        let task = saved(&context, &runtime, &interrupted);
        if task["status"] == "succeeded" {
            receipt = Some(task);
            break;
        }
    }
    let receipt = receipt.expect("新owner应先结清原机械task");
    assert_eq!(receipt["status"], "succeeded");
    assert_eq!(receipt["request"], original["request"]);
    assert_eq!(receipt["execution"]["plan"], original["execution"]["plan"]);
    assert_eq!(receipt["owner_epoch"], next);
    let commands = faults.commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == opcode::MOVE_MEDIUM)
            .count(),
        1
    );
    assert!(
        !commands.iter().any(|c| c[0] == opcode::LOAD_UNLOAD),
        "接管核实Access=1后直接结清原MOVE，不LOAD/UNLOAD"
    );
    drop(commands);
    assert!(
        runtime
            .block_on(catalog.active_library_mechanical_task(&serial, route, next))
            .unwrap()
            .is_none()
    );
    drop(active);
    drop(files);
    std::fs::remove_dir_all(root).unwrap();
}
