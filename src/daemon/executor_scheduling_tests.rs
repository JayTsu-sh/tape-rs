use super::*;
use crate::daemon::{
    directory::PoolRow,
    files::{ServiceError, TaskStatus},
};
use crate::scsi::{
    device::ScsiResult,
    sim::{SimCartridge, SimLibrary, SimTransport},
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Activity {
    current: AtomicUsize,
    maximum: AtomicUsize,
    missing_capacity: std::sync::atomic::AtomicBool,
}
struct Observed {
    inner: SimTransport,
    activity: Arc<Activity>,
}
impl TapeTransport for Observed {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execute_no_data(&self, c: &[u8], t: u32) -> Result<ScsiResult> {
        self.inner.execute_no_data(c, t)
    }
    fn execute_read(&self, c: &[u8], b: &mut [u8], t: u32) -> Result<ScsiResult> {
        if c[0] == 0x8c
            && u16::from_be_bytes([c[8], c[9]]) <= 1
            && self.activity.missing_capacity.load(Ordering::Acquire)
        {
            return Err(TapeError::NotReady("测试：容量 MAM 不可用".into()));
        }
        self.inner.execute_read(c, b, t)
    }
    fn execute_write(&self, c: &[u8], b: &[u8], t: u32) -> Result<ScsiResult> {
        let data = c[0] == 0x0a;
        if data {
            let n = self.activity.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.activity.maximum.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
        }
        let r = self.inner.execute_write(c, b, t);
        if data {
            self.activity.current.fetch_sub(1, Ordering::SeqCst);
        }
        r
    }
}
struct Provider {
    lib: SimLibrary,
    activity: Arc<Activity>,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let mut devices = vec![ManagedDevice {
            name: "changer".into(),
            serial: "SIMLIB0000000001".into(),
            kind: DeviceKind::Changer,
            dev: Box::new(self.lib.changer()),
        }];
        for i in 0..2 {
            devices.push(ManagedDevice {
                name: format!("drive{i}"),
                serial: format!("SIMDRV{i:04}"),
                kind: DeviceKind::Drive,
                dev: Box::new(Observed {
                    inner: self.lib.drive(i),
                    activity: self.activity.clone(),
                }),
            });
        }
        Ok(devices)
    }
}
struct Fixture {
    lib: SimLibrary,
    root: Arc<FileService>,
    status: SharedStatus,
    active: Active,
    options: ExecOptions,
    tx: Sender<ExecEvent>,
    events: Receiver<ExecEvent>,
    activity: Arc<Activity>,
    dir: std::path::PathBuf,
}
impl Fixture {
    fn new(two_pools: bool, limit: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("scheduler-{}", uuid::Uuid::new_v4()));
        let root = FileService::with_options(
            dir.clone(),
            super::super::files::BatchPolicy {
                max_files: 2,
                idle: Duration::from_secs(60),
                max_wait: Duration::from_secs(60),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let lib = SimLibrary::new(2, 4, 1);
        for (slot, b) in ["TEST01L8", "TEST02L8"].iter().enumerate() {
            lib.insert_cartridge(SimCartridge::blank(b, 2 << 30), slot)
                .unwrap();
        }
        let pools = if two_pools {
            vec![
                PoolRow {
                    uuid: "pool1".into(),
                    name: "one".into(),
                    file_limit: 200000,
                    tapes: vec!["TEST01L8".into()],
                },
                PoolRow {
                    uuid: "pool2".into(),
                    name: "two".into(),
                    file_limit: 200000,
                    tapes: vec!["TEST02L8".into()],
                },
            ]
        } else {
            vec![PoolRow {
                uuid: "pool1".into(),
                name: "one".into(),
                file_limit: 200000,
                tapes: vec!["TEST01L8".into(), "TEST02L8".into()],
            }]
        };
        let status = Arc::new(Mutex::new(super::super::node::NodeStatus {
            pools,
            management: serde_json::json!({"pools":{"pool1":{"mountlimit":limit.to_string()}}})
                .into(),
            ..Default::default()
        }));
        let activity = Arc::new(Activity {
            current: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            missing_capacity: std::sync::atomic::AtomicBool::new(false),
        });
        let options = ExecOptions {
            node_id: 1,
            interval: Duration::from_secs(3),
            demo_write: false,
            salvage: false,
            block_size: 65536,
            read_idle: Duration::from_secs(30),
        };
        let provider = Provider {
            lib: lib.clone(),
            activity: activity.clone(),
        };
        let active = takeover(&provider, &options, 100, None, root.clone()).unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        Self {
            lib,
            root,
            status,
            active,
            options,
            tx,
            events,
            activity,
            dir,
        }
    }
    fn recover(&mut self) {
        ensure_writers(
            &mut self.active,
            &self.options,
            &self.root,
            &self.status,
            &self.tx,
        )
        .unwrap();
        self.active.serving = true;
        self.activity.maximum.store(0, Ordering::SeqCst);
    }
    fn stage(files: &FileService, path: &str, body: &[u8]) -> u64 {
        let h = files.begin(path, body.len() as u64).unwrap();
        std::fs::write(&h.spool, body).unwrap();
        files.ingest(&h, body.len() as u64).unwrap();
        files.finish(h, body.len() as u64).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.root.close_all("test done");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn reconcile_accepts_unmarked_external_volume_without_writing_pool_marker() {
    let mut f = Fixture::new(false, 1);
    f.lib.load_into_drive("TEST01L8", 0).unwrap();
    let di = f
        .active
        .devices
        .iter()
        .position(|d| d.serial == "SIMDRV0000")
        .unwrap();
    crate::ltfs::mkltfs::mkltfs(
        f.active.devices[di].dev.as_ref(),
        &crate::ltfs::mkltfs::MkltfsOptions::default(),
    )
    .unwrap();
    let inv = inventory(&f.active.devices).unwrap();
    let drive = inv.drives.iter().find(|d| d.dev == di).unwrap();
    unload_drive(&f.active.devices, &inv, drive).unwrap();
    let task = serde_json::json!({"id":1,"request":{"resource":"tapes","action":"reconcile","target":"TEST01L8","params":{}}});
    assert!(
        management::execute(&mut f.active, &f.options, &f.status, &f.root, &f.tx, &task).is_ok()
    );
    let inv = inventory(&f.active.devices).unwrap();
    let slot = inv.slots["TEST01L8"];
    let drive = inv.drives.iter().find(|d| d.dev == di).unwrap();
    move_medium(&f.active.devices, slot, drive.addr).unwrap();
    TapeDrive::new(f.active.devices[di].dev.as_ref())
        .load()
        .unwrap();
    let vol = LtfsVolume::mount(f.active.devices[di].dev.as_ref()).unwrap();
    assert!(vol.root_xattr(XATTR_POOL_UUID).is_none());
}

#[test]
fn reconcile_rejects_a_different_volume_with_the_same_barcode() {
    let mut f = Fixture::new(false, 1);
    f.recover();
    let lane = f
        .root
        .lanes()
        .into_iter()
        .find(|lane| lane.tape().is_some())
        .unwrap();
    let barcode = lane.tape().unwrap().barcode;
    activate_writer(&mut f.active, &lane);
    management::quiesce(&mut f.active, &lane, &f.tx).unwrap();
    let task = serde_json::json!({"id":1,"request":{"resource":"tapes","action":"reconcile","target":barcode,"params":{}}});
    let first =
        management::execute(&mut f.active, &f.options, &f.status, &lane, &f.tx, &task).unwrap();
    let uuid = first["volume_uuid"].as_str().unwrap().to_owned();
    let generation = first["generation"].as_u64().unwrap();
    for known_generation in [0, generation] {
        f.status.lock().unwrap().tapes.insert(
            barcode.clone(),
            super::super::node::TapeRow {
                volume_uuid: uuid::Uuid::new_v4().to_string(),
                generation: known_generation,
                ..Default::default()
            },
        );
        while f.events.try_recv().is_ok() {}
        let result = management::execute(&mut f.active, &f.options, &f.status, &lane, &f.tx, &task);
        assert!(
            result.is_err(),
            "不同卷 UUID 的相同条码不得对账: {result:?}"
        );
        assert!(
            f.events.try_iter().next().is_none(),
            "身份不符时不得发布任何事件"
        );
    }
    f.status.lock().unwrap().tapes.insert(
        barcode,
        super::super::node::TapeRow {
            volume_uuid: uuid,
            generation,
            ..Default::default()
        },
    );
    assert!(management::execute(&mut f.active, &f.options, &f.status, &lane, &f.tx, &task).is_ok());
    f.status.lock().unwrap().pools[0].uuid = "different-pool".into();
    while f.events.try_recv().is_ok() {}
    let result = management::execute(&mut f.active, &f.options, &f.status, &lane, &f.tx, &task);
    assert!(result.is_err(), "介质池标记不符不得对账: {result:?}");
    assert!(
        f.events.try_iter().next().is_none(),
        "池标记不符时不得发布任何事件"
    );
}

#[test]
fn same_pool_writes_overlap_and_share_conflicts_tasks_and_versions() {
    let mut f = Fixture::new(false, 2);
    f.root.bind_pool("pool1");
    f.root.add_lane("pool1");
    f.recover();
    let lanes: Vec<_> = f
        .root
        .lanes()
        .into_iter()
        .filter(|l| l.tape().is_some())
        .collect();
    assert_eq!(lanes.len(), 2);
    let h = lanes[0].begin("/reserved", 1).unwrap();
    assert!(matches!(
        lanes[1].begin("/reserved", 1),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        lanes[1].mkdir("/reserved/child"),
        Err(ServiceError::PathBusy(_))
    ));
    lanes[0].abort(h);
    let t1 = Fixture::stage(&lanes[0], "/one", b"first");
    let t2 = Fixture::stage(&lanes[1], "/two", b"second");
    assert_ne!(t1, t2);
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, None).unwrap();
    assert_eq!(f.activity.maximum.load(Ordering::SeqCst), 2);
    for lane in &lanes {
        assert!(matches!(lane.task(t1), Some(TaskStatus::Committed { .. })));
        assert!(matches!(lane.task(t2), Some(TaskStatus::Committed { .. })));
        assert_ne!(
            lane.stat("/one").unwrap().unwrap().version,
            lane.stat("/two").unwrap().unwrap().version
        );
    }
    for (lane, path, body) in [
        (lanes[0].clone(), "/one", b"first".as_slice()),
        (lanes[1].clone(), "/two", b"second".as_slice()),
    ] {
        activate_writer(&mut f.active, &lane);
        let mut out = Vec::new();
        read_any(
            &mut f.active,
            &f.options,
            &lane,
            &f.status,
            &f.tx,
            path,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, body);
    }
}

#[test]
fn pools_have_isolated_paths_spools_and_commits() {
    let mut f = Fixture::new(true, 0);
    f.recover();
    let one = f.root.select_pool("pool1", None, false, true).unwrap();
    let two = f.root.select_pool("pool2", None, false, true).unwrap();
    Fixture::stage(&one, "/same", b"one");
    Fixture::stage(&two, "/same", b"two");
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, Some("pool1")).unwrap();
    assert!(one.stat("/same").unwrap().is_some());
    assert!(two.stat("/same").unwrap().is_none());
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, None).unwrap();
    assert_ne!(
        one.stat("/same").unwrap().unwrap().sha256,
        two.stat("/same").unwrap().unwrap().sha256
    );
    assert_ne!(one.tape(), two.tape());
}

#[test]
fn mount_limit_counts_writer_leases_and_not_loaded_read_media() {
    let mut f = Fixture::new(false, 1);
    f.recover();
    assert_eq!(
        f.root.lanes().iter().filter(|l| l.tape().is_some()).count(),
        1
    );
    // 第二盘带只读装载不占 migration 配额；已有租约数才决定能否再分配写驱动。
    let root = f.root.clone();
    activate_writer(&mut f.active, &root);
    assert!(
        !migration_allowed(&f.active, &f.status.lock().unwrap(), "pool1")
            || f.active.writers.iter().all(|w| w.drive.is_none())
    );
    assert!(
        inventory(&f.active.devices)
            .unwrap()
            .slots
            .contains_key("TEST02L8")
    );
    Arc::make_mut(&mut f.status.lock().unwrap().management)["pools"]["pool1"]["mountlimit"] =
        serde_json::json!("2");
    f.root.add_lane("pool1");
    f.recover();
    assert_eq!(
        f.root.lanes().iter().filter(|l| l.tape().is_some()).count(),
        2
    );
}

#[test]
fn incompatible_generation_never_moves_or_formats_cartridge() {
    let mut f = Fixture::new(false, 2);
    f.active.density_support.insert(1, [(0x60, true)].into());
    f.active.density_support.insert(2, [(0x60, true)].into());
    f.recover();
    assert!(f.root.lanes().iter().all(|l| l.tape().is_none()));
    let inv = inventory(&f.active.devices).unwrap();
    assert_eq!(inv.slots.len(), 2);
    assert!(inv.drives.iter().all(|d| d.loaded.is_none()));
    assert!(
        !f.events
            .try_iter()
            .any(|ev| matches!(ev, ExecEvent::TapeState { .. }))
    );
    assert_eq!(f.lib.cartridge("TEST01L8").unwrap().partitions.len(), 1);
}

#[test]
fn missing_capacity_refuses_admission_but_confirmed_commit_and_reads_survive() {
    let mut f = Fixture::new(false, 1);
    f.recover();
    let task = Fixture::stage(&f.root, "/confirmed", b"confirmed");
    f.activity.missing_capacity.store(true, Ordering::Release);
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, None).unwrap();
    assert!(matches!(
        f.root.task(task),
        Some(TaskStatus::Committed { .. })
    ));
    assert!(matches!(
        f.root.begin("/unsafe", 1),
        Err(ServiceError::NotServing(_))
    ));
    assert!(f.root.stat("/confirmed").unwrap().is_some());
    assert!(!f.events.try_iter().any(|e|matches!(e,ExecEvent::TapeState{ref state,..} if state==tape_state::FULL || state==tape_state::CHECK)));
    f.activity.missing_capacity.store(false, Ordering::Release);
    f.recover();
    let h = f.root.begin("/recovered", 1).unwrap();
    f.root.abort(h);
}

#[test]
fn repeated_takeover_registers_existing_pool_lanes_before_recovery() {
    let mut f = Fixture::new(true, 2);
    f.recover();
    f.root.close_all("next round");
    let provider = Provider {
        lib: f.lib.clone(),
        activity: f.activity.clone(),
    };
    f.active = takeover(&provider, &f.options, 101, None, f.root.clone()).unwrap();
    f.recover();
    assert_eq!(
        f.root.lanes().iter().filter(|l| l.tape().is_some()).count(),
        2
    );
    for lane in f.root.lanes() {
        assert_eq!(lane.serving_round(), Some(101));
    }
}

#[test]
fn demand_for_a_second_pool_can_borrow_an_idle_writer_drive() {
    let mut f = Fixture::new(true, 1);
    Arc::make_mut(&mut f.status.lock().unwrap().management)["drives"] =
        serde_json::json!({"SIMDRV0001":{"enabled":false}});
    f.recover();
    assert!(
        f.root
            .select_pool("pool2", None, false, false)
            .unwrap()
            .tape()
            .is_none()
    );
    let two = f.root.select_pool("pool2", None, false, true).unwrap();
    f.recover();
    assert!(two.tape().is_some());
    assert!(f.root.tape().is_none());
    Fixture::stage(&two, "/borrowed", b"pool two");
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, None).unwrap();
    assert!(two.stat("/borrowed").unwrap().is_some());
}

#[test]
fn readonly_pool_can_borrow_another_pools_idle_writer() {
    let mut f = Fixture::new(true, 1);
    f.recover();
    let two = f.root.select_pool("pool2", None, false, false).unwrap();
    Fixture::stage(&two, "/read-me", b"pool two data");
    process_all_uploads(&mut f.active, &f.tx, true, usize::MAX, None).unwrap();
    activate_writer(&mut f.active, &two);
    let disabled = f.active.devices[f.active.drive.unwrap()].serial.clone();
    management::quiesce(&mut f.active, &two, &f.tx).unwrap();
    Arc::make_mut(&mut f.status.lock().unwrap().management)["drives"][&disabled] =
        serde_json::json!({"enabled":false});
    prepare_read(
        &mut f.active,
        &f.options,
        &two,
        &f.status,
        &f.tx,
        "/read-me",
    )
    .unwrap();
    assert!(f.root.tape().is_none(), "其他池已排空的写入租约应已让出");
    let mut data = Vec::new();
    read_any(
        &mut f.active,
        &f.options,
        &two,
        &f.status,
        &f.tx,
        "/read-me",
        &mut data,
    )
    .unwrap();
    assert_eq!(data, b"pool two data");
}

#[test]
fn one_due_batch_starts_other_completed_drive_queue_in_the_same_work_turn() {
    let mut f = Fixture::new(false, 2);
    f.root.bind_pool("pool1");
    f.root.add_lane("pool1");
    f.recover();
    let lanes = f.root.lanes();
    Fixture::stage(&lanes[0], "/a", b"a");
    Fixture::stage(&lanes[0], "/b", b"b");
    let other = Fixture::stage(&lanes[1], "/c", b"c");
    assert_eq!(lanes[0].due_in(100), Some(Duration::ZERO));
    assert!(lanes[1].due_in(100).unwrap() > Duration::ZERO);
    process_all_uploads(&mut f.active, &f.tx, false, usize::MAX, None).unwrap();
    assert!(matches!(
        lanes[1].task(other),
        Some(TaskStatus::Committed { .. })
    ));
    assert_eq!(f.activity.maximum.load(Ordering::SeqCst), 2);
}
