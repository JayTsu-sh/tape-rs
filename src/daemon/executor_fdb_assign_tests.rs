//! Private FDB and actual Sim effects, never physical devices.
use super::super::fdb_observations_fixture::{make_status, options};
use super::*;
use crate::managed_catalog::{ManagedCatalog, PoolPolicy, PoolRecord, verify_native_barriers};
use crate::scsi::{
    cdb::opcode,
    sim::{SimCartridge, SimLibrary, SimTransport},
};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[test]
fn nonforce_large_nonempty_index_is_rejected_before_whole_tree_mount() {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 30), 0)
        .unwrap();
    lib.load_into_drive("TEST01L8", 0).unwrap();
    let dev = lib.drive(0);
    crate::ltfs::mkltfs::mkltfs(&dev, &Default::default()).unwrap();
    let mut volume = crate::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
    for id in 0..4000 {
        volume
            .append_file(&format!("file-{id}"), &mut std::io::Cursor::new(b"keep"))
            .unwrap();
    }
    volume.commit().unwrap();
    drop(volume);
    struct CappedReads<'a> {
        dev: &'a dyn TapeTransport,
        reads: Cell<usize>,
    }
    impl TapeTransport for CappedReads<'_> {
        fn identity(&self) -> &str {
            self.dev.identity()
        }
        fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
            assert_ne!(cdb[0], opcode::FORMAT_MEDIUM);
            self.dev.execute_no_data(cdb, timeout)
        }
        fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
            if cdb[0] == opcode::READ_6 {
                self.reads.set(self.reads.get() + 1);
                if self.reads.get() > 4 {
                    return Err(TapeError::ScsiHost(3));
                }
            }
            self.dev.execute_read(cdb, buf, timeout)
        }
        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
            panic!("nonforce inspection must never write")
        }
    }
    let guarded = CappedReads {
        dev: &dev,
        reads: Cell::new(0),
    };
    let result = nonforce_empty(&guarded, &crate::ltfs::recovery::RecoveryBudget::default());
    assert!(
        matches!(result, Ok(false)),
        "known first file must reject without reading/materializing the entire index: {result:?}; reads={}",
        guarded.reads.get()
    );
    assert!(guarded.reads.get() <= 4);
}

#[test]
fn nonforce_empty_full_requires_bounded_resources_and_matching_labels() {
    use crate::ltfs::recovery::RecoveryBudget;
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 30), 0)
        .unwrap();
    lib.load_into_drive("TEST01L8", 0).unwrap();
    let dev = lib.drive(0);
    crate::ltfs::mkltfs::mkltfs(&dev, &Default::default()).unwrap();
    assert!(nonforce_empty(&dev, &RecoveryBudget::default()).unwrap());
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.max_bytes = 0;
    assert!(matches!(
        nonforce_empty(&dev, &budget),
        Err(TapeError::LocalCapacity { .. })
    ));
    let mut volume = crate::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
    volume.set_root_xattr("large", &"x".repeat(8192)).unwrap();
    volume.commit().unwrap();
    drop(volume);
    budget = RecoveryBudget {
        max_index_bytes: 4096,
        ..RecoveryBudget::default()
    };
    assert!(matches!(
        nonforce_empty(&dev, &budget),
        Err(TapeError::LocalCapacity { .. })
    ));
    assert!(nonforce_empty(&dev, &RecoveryBudget::default()).unwrap());
    lib.with_cartridge_mut("TEST01L8", |cart| {
        if let crate::scsi::sim::LogicalObject::Record(xml) = &mut cart.partitions[1].objects[2] {
            let mut label = crate::ltfs::label::LtfsLabel::parse(xml).unwrap();
            label.volume_uuid = Uuid::new_v4();
            *xml = label.to_xml().unwrap().to_vec();
        } else {
            panic!("label must be a record");
        }
    });
    assert!(nonforce_empty(&dev, &RecoveryBudget::default()).is_err());
}

#[test]
fn nonforce_empty_delta_requires_final_replay_and_same_generation_ip() {
    use crate::ltfs::{index::IndexLocation, recovery::RecoveryBudget, volume::LtfsVolume};
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 30), 0)
        .unwrap();
    lib.load_into_drive("TEST01L8", 0).unwrap();
    let dev = lib.drive(0);
    crate::ltfs::mkltfs::mkltfs(&dev, &Default::default()).unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("keep", &mut std::io::Cursor::new(b"keep"))
        .unwrap();
    volume.commit_incremental().unwrap();
    assert!(!nonforce_empty(&dev, &RecoveryBudget::default()).unwrap());
    volume.remove_file("keep").unwrap();
    volume.commit_incremental().unwrap();
    assert!(volume.index().incremental && volume.index().root.files.is_empty());
    assert!(
        !nonforce_empty(&dev, &RecoveryBudget::default()).unwrap(),
        "IP debt cannot prove safe emptiness"
    );
    let mut ip = volume.index().clone();
    ip.incremental = false;
    ip.materialized = false;
    ip.previous_incremental_location = Some(ip.self_location);
    ip.self_location = IndexLocation {
        partition: 'a',
        start_block: crate::ltfs::volume::FIRST_INDEX_BLOCK,
    };
    let xml = ip.to_xml().unwrap();
    drop(volume);
    let drive = TapeDrive::new(&dev);
    drive
        .locate(0, crate::ltfs::volume::P1_DATA_START, true)
        .unwrap();
    drive.write_filemark(1).unwrap();
    drive.write_block(&xml).unwrap();
    drive.write_filemark(1).unwrap();
    drive.write_filemark(0).unwrap();
    assert!(
        nonforce_empty(&dev, &RecoveryBudget::default()).unwrap(),
        "disk replay must use the final deleted-empty view"
    );
    let budget = RecoveryBudget {
        max_chain_len: 1,
        ..RecoveryBudget::default()
    };
    assert!(
        matches!(
            nonforce_empty(&dev, &budget),
            Err(TapeError::LocalCapacity { .. })
        ),
        "bounded replay must preserve resource uncertainty"
    );
}

#[test]
fn nonforce_single_large_attribute_and_xml_depth_are_resource_limits() {
    use crate::ltfs::{index::LtfsIndex, recovery::RecoveryBudget, volume::LtfsVolume};
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 30), 0)
        .unwrap();
    lib.load_into_drive("TEST01L8", 0).unwrap();
    let dev = lib.drive(0);
    crate::ltfs::mkltfs::mkltfs(&dev, &Default::default()).unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .set_root_xattr("giant", &"x".repeat(17 << 20))
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    assert!(
        matches!(
            nonforce_empty(&dev, &RecoveryBudget::default()),
            Err(TapeError::LocalCapacity { .. })
        ),
        "single event must obey the hard16MiB limit"
    );
    let xml = format!(
        "<ltfsindex>{}{}</ltfsindex>",
        "<directory>".repeat(513),
        "</directory>".repeat(513)
    );
    assert!(matches!(
        LtfsIndex::scan_any_reader_to_disk(xml.as_bytes(), |_| Ok(())),
        Err(TapeError::LocalCapacity { .. })
    ));
}

#[test]
fn probe_failure_requires_complete_current_sense_before_releasing_unknown_result() {
    use crate::scsi::sense::SenseInfo;
    let error = |sense: Option<SenseInfo>, key, asc| TapeError::ScsiCommand {
        status: 2,
        sense_key: key,
        asc,
        ascq: 0,
        sense: sense.map(Box::new),
    };
    assert!(!probe_read_completed(&error(None, 3, 0x11)));
    let mut raw = [0; 18];
    raw[0] = 0x70;
    raw[2] = 3;
    raw[7] = 10;
    raw[12] = 0x11;
    for length in [0, 7, 13, 14, 17] {
        assert!(
            !probe_read_completed(&error(Some(SenseInfo::from_bytes(&raw[..length])), 3, 0x11)),
            "truncated sense length={length}"
        );
    }
    for response in [0x70, 0x71, 0x72, 0x73] {
        let mut bytes = raw;
        bytes[0] = response;
        if response >= 0x72 {
            bytes[1] = 3;
            bytes[2] = 0x11;
        }
        assert_eq!(
            probe_read_completed(&error(Some(SenseInfo::from_bytes(&bytes)), 3, 0x11)),
            matches!(response, 0x70 | 0x72),
            "response={response:#x}"
        );
    }
    for (key, asc) in [(5, 0x11), (3, 0x20)] {
        assert!(
            !probe_read_completed(&error(Some(SenseInfo::from_bytes(&raw)), key, asc)),
            "public fields disagree with captured sense"
        );
    }
    for key in [2, 6, 0x0b] {
        raw[2] = key;
        assert!(!probe_read_completed(&error(
            Some(SenseInfo::from_bytes(&raw)),
            key,
            0x11
        )));
    }
}
#[derive(Default)]
struct Faults {
    successful: Mutex<Vec<Vec<u8>>>,
    drop_format: AtomicBool,
    drop_final: AtomicBool,
    attributes_written: AtomicBool,
    hold_format: AtomicBool,
    format_inflight: AtomicBool,
    release_format: AtomicBool,
    worker: Mutex<Option<std::thread::JoinHandle<Result<ScsiResult>>>>,
    fail_index_reads: AtomicBool,
    index_region: AtomicBool,
}
struct Trace {
    inner: Arc<SimTransport>,
    faults: Arc<Faults>,
    drive: bool,
}
impl TapeTransport for Trace {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        if self.drive
            && cdb[0] == opcode::FORMAT_MEDIUM
            && self.faults.hold_format.swap(false, Ordering::SeqCst)
        {
            self.faults.format_inflight.store(true, Ordering::SeqCst);
            let inner = self.inner.clone();
            let faults = self.faults.clone();
            let command = cdb.to_vec();
            let worker = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !faults.release_format.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let result = inner.execute_no_data(&command, timeout);
                if result.is_ok() {
                    faults.successful.lock().unwrap().push(command);
                }
                faults.format_inflight.store(false, Ordering::SeqCst);
                result
            });
            *self.faults.worker.lock().unwrap() = Some(worker);
            return Err(TapeError::ScsiHost(3));
        }
        if self.drive && self.faults.format_inflight.load(Ordering::SeqCst) {
            return Err(TapeError::Busy {
                device: self.inner.identity().into(),
            });
        }
        if self.drive {
            if cdb[0] == opcode::LOCATE_16 {
                self.faults.index_region.store(
                    u64::from_be_bytes(cdb[4..12].try_into().unwrap()) >= 5,
                    Ordering::SeqCst,
                );
            }
            if cdb[0] == opcode::SPACE_6 {
                self.faults.index_region.store(true, Ordering::SeqCst);
            }
        }
        let r = self.inner.execute_no_data(cdb, timeout)?;
        self.faults.successful.lock().unwrap().push(cdb.to_vec());
        if cdb[0] == opcode::FORMAT_MEDIUM && self.faults.drop_format.swap(false, Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        if cdb[0] == opcode::LOCATE_16
            && cdb[3] == 0
            && cdb[4..12].iter().all(|b| *b == 0)
            && self.faults.attributes_written.load(Ordering::SeqCst)
            && self.faults.drop_final.swap(false, Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        Ok(r)
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        let result = self.inner.execute_read(cdb, buf, timeout)?;
        if self.drive
            && cdb[0] == opcode::READ_6
            && self.faults.fail_index_reads.load(Ordering::SeqCst)
            && self.faults.index_region.load(Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        Ok(result)
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout: u32) -> Result<ScsiResult> {
        let r = self.inner.execute_write(cdb, buf, timeout)?;
        if cdb[0] == opcode::WRITE_ATTRIBUTE {
            self.faults.attributes_written.store(true, Ordering::SeqCst);
        }
        Ok(r)
    }
}
struct Provider {
    library: SimLibrary,
    serial: String,
    faults: Arc<Faults>,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        Ok(vec![
            ManagedDevice {
                name: "changer".into(),
                serial: self.serial.clone(),
                kind: DeviceKind::Changer,
                dev: Box::new(Trace {
                    inner: Arc::new(self.library.changer_as(2)),
                    faults: self.faults.clone(),
                    drive: false,
                }),
            },
            ManagedDevice {
                name: "drive".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: Box::new(Trace {
                    inner: Arc::new(self.library.drive_as(0, 2)),
                    faults: self.faults.clone(),
                    drive: true,
                }),
            },
        ])
    }
}
fn saved(context: Context<'_>, runtime: &tokio::runtime::Runtime, r: &ManagementRequest) -> Value {
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
#[test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；真实Sim格式化/回执丢失/接管，不访问物理设备"]
fn assignment_format_unknown_keeps_original_uuid_without_reformatting() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    for case in 0..9 {
        let ns = Uuid::new_v4();
        let serial = format!("ASSIGN-FAULT-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = runtime
            .block_on(catalog.initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            ))
            .unwrap();
        let round = route.initial_owner_epoch;
        let mut control = FdbOwnerControl {
            catalog: catalog.clone(),
            serial: serial.clone(),
            route,
            storage: Default::default(),
        };
        if case == 8 {
            control.storage.index_bytes = 0;
        }
        let lib = SimLibrary::new(1, 3, 1);
        lib.enable_changer_pr();
        let mut cartridge = SimCartridge::blank("TEST01L8", 1 << 20);
        if case == 6 {
            use crate::scsi::sim::LogicalObject::{Filemark, Record};
            cartridge.partitions[0].objects = vec![
                Record(crate::ltfs::label::Vol1Label::encode("TEST01", "").to_vec()),
                Filemark,
                Record(b"<invalid-label/>".to_vec()),
                Filemark,
            ];
            cartridge.partitions[0].flushed = 4;
        }
        lib.insert_cartridge(cartridge, 0).unwrap();
        if case >= 7 {
            lib.load_into_drive("TEST01L8", 0).unwrap();
            crate::ltfs::mkltfs::mkltfs(
                &lib.drive(0),
                &crate::ltfs::mkltfs::MkltfsOptions::default(),
            )
            .unwrap();
            TapeDrive::new(&lib.drive(0)).unload().unwrap();
            let transport = lib.changer();
            let mut changer = MediumChanger::new(&transport);
            let map = changer.load_address_map().unwrap().clone();
            changer
                .move_medium(map.dt_start, map.storage_start)
                .unwrap();
        }
        let root = std::env::temp_dir().join(format!("tapers-assign-fault-{ns}"));
        let files = FileService::new(root.join("spool")).unwrap();
        let permission = Arc::new(Permission::default());
        let faults = Arc::new(Faults::default());
        if case == 7 {
            faults.fail_index_reads.store(true, Ordering::SeqCst);
        }
        let provider = Provider {
            library: lib.clone(),
            serial: serial.clone(),
            faults: faults.clone(),
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

        let mut status = make_status(2, round, route.frozen_index);
        let context = || Context {
            active: &active,
            permission: &permission,
            control: &control,
            status: &status,
            node: 2,
        };
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "pool".into(),
            file_limit: 100,
            created_at_millis: 1000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        runtime
            .block_on(catalog.create_active_pool(&serial, route, round, &pool))
            .unwrap();
        let volume = Uuid::new_v4();
        let request=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"TEST01L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}})).unwrap();
        let queued = runtime
            .block_on(catalog.admit_management(&serial, route, round, &request))
            .unwrap();
        let mut job = Job::new(request.clone(), &queued).unwrap();
        let mut publisher = Publisher::default();
        if case >= 6 {
            let mut terminal = false;
            for _ in 0..16 {
                let result = job.poll(context(), &mut publisher, &runtime).unwrap();
                if matches!(result, Publication::Confirmed(true)) {
                    terminal = true;
                    break;
                }
            }
            if case >= 7 && job.diagnostic.is_some() {
                let _ = job.poll(context(), &mut publisher, &runtime).unwrap();
            }
            let task = saved(context(), &runtime, &request);
            if case == 6 {
                assert!(
                    terminal && task["status"] == "failed",
                    "confirmed bad LTFS label must reject before FORMAT rather than remain running: {task}"
                );
                assert_eq!(task["execution"]["phase"], "inspecting");
                assert_eq!(task["result"]["effects"], "loaded_not_formatted");
                runtime
                    .block_on(catalog.require_active_library_readable(&serial, route, round))
                    .unwrap();
            } else {
                assert!(
                    !terminal && task["status"] == "running",
                    "transport uncertainty swallowed by recovery is not a confirmed format restriction: {task}"
                );
                assert_eq!(task["execution"]["phase"], "inspecting");
                assert!(
                    task["execution"]["error"]
                        .as_str()
                        .is_some_and(|e| e.contains(if case == 7 {
                            "SCSI host error"
                        } else {
                            "临时盘"
                        })),
                    "original dependency diagnostic must survive: {task}"
                );
                assert!(
                    runtime
                        .block_on(catalog.require_active_library_readable(&serial, route, round))
                        .is_err()
                );
            }
            assert!(
                runtime
                    .block_on(catalog.tape_assignment(&serial, "TEST01L8"))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                faults
                    .successful
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c[0] == opcode::FORMAT_MEDIUM)
                    .count(),
                0
            );
            std::fs::remove_dir_all(root).unwrap();
            continue;
        }
        if case == 0 {
            let snapshot = Snapshot::capture(&context()).unwrap();
            let original_plan = plan(&snapshot, &request, pool.revision, &status).unwrap();
            runtime
                .block_on(catalog.prepare_media_assign(
                    &serial,
                    route,
                    round,
                    &request,
                    &original_plan,
                ))
                .unwrap();
            runtime
                .block_on(catalog.stage_media_assign(
                    &serial,
                    route,
                    round,
                    &request,
                    AssignPhase::Moving,
                ))
                .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let unavailable_file = root.join("move-unavailable.cluster");
            std::fs::write(
                &unavailable_file,
                format!(
                    "assignmove:{}@{}",
                    Uuid::new_v4().simple(),
                    listener.local_addr().unwrap()
                ),
            )
            .unwrap();
            let unavailable = FdbOwnerControl {
                catalog: Arc::new(
                    ManagedCatalog::open(Some(unavailable_file.to_str().unwrap())).unwrap(),
                ),
                serial: serial.clone(),
                route,
                storage: Default::default(),
            };
            let count = faults.successful.lock().unwrap().len();
            assert!(
                send_move(
                    Context {
                        control: &unavailable,
                        ..context()
                    },
                    &runtime,
                    &request,
                    &original_plan,
                    &Cell::new(false)
                )
                .is_err(),
                "FDB loss after the acknowledged Moving stage must prevent MOVE"
            );
            assert_eq!(
                faults.successful.lock().unwrap().len(),
                count,
                "no device command at the unconfirmed mechanical boundary"
            );
            job = Job::new(request.clone(), &saved(context(), &runtime, &request)).unwrap();
        }
        for _ in 0..12 {
            let result = job.poll(context(), &mut publisher, &runtime).unwrap();
            assert!(!matches!(
                result,
                Publication::Lost(_) | Publication::Paused
            ));
            if saved(context(), &runtime, &request)["execution"]["phase"] == "formatting" {
                break;
            }
        }
        if case == 0 {
            let mut diagnostic_job =
                Job::new(request.clone(), &saved(context(), &runtime, &request)).unwrap();
            diagnostic_job.diagnostic =
                Some(("持久事实损坏，应停止本轮而非反复包裹原诊断".into(), true));
            assert!(
                matches!(
                    diagnostic_job
                        .poll(context(), &mut publisher, &runtime)
                        .unwrap(),
                    Publication::Failed {
                        permanent: true,
                        ..
                    }
                ),
                "persisted permanent diagnostics must reach the shared controller"
            );
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let unavailable_file = root.join("unavailable.cluster");
            std::fs::write(
                &unavailable_file,
                format!(
                    "assign:{}@{}",
                    Uuid::new_v4().simple(),
                    listener.local_addr().unwrap()
                ),
            )
            .unwrap();
            let unavailable = FdbOwnerControl {
                catalog: Arc::new(
                    ManagedCatalog::open(Some(unavailable_file.to_str().unwrap())).unwrap(),
                ),
                serial: serial.clone(),
                route,
                storage: Default::default(),
            };
            let count = faults.successful.lock().unwrap().len();
            let never = CommandGuard {
                context: Context {
                    control: &unavailable,
                    ..context()
                },
                runtime: &runtime,
                request: &request,
                phase: AssignPhase::Formatting,
                inner: active.devices[1].dev.as_ref(),
                permanent: &Cell::new(false),
                drive_serial: "SIMDRV0000",
                probe_fault: None,
            };
            assert!(TapeDrive::new(&never).format(1, false).is_err());
            assert_eq!(
                faults.successful.lock().unwrap().len(),
                count,
                "no device call while original FDB stage cannot be confirmed"
            );
        }
        let before = saved(context(), &runtime, &request);
        assert_eq!(before["execution"]["phase"], "formatting", "{before}");
        assert!(
            runtime
                .block_on(catalog.tape_assignment(&serial, "TEST01L8"))
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .block_on(catalog.require_active_library_readable(&serial, route, round))
                .is_err()
        );
        if matches!(case, 1 | 3 | 4) {
            faults.drop_final.store(true, Ordering::SeqCst);
        }
        if case == 2 {
            faults.drop_format.store(true, Ordering::SeqCst);
        }
        if case == 5 {
            faults.hold_format.store(true, Ordering::SeqCst);
        }
        let result = job.poll(context(), &mut publisher, &runtime).unwrap();
        if case != 0 {
            assert!(matches!(result, Publication::Failed { .. }));
        }
        if matches!(case, 3 | 4) {
            assert!(lib.with_cartridge_mut("TEST01L8", |cart| {
                let crate::scsi::sim::LogicalObject::Record(bytes) =
                    &mut cart.partitions[1].objects[crate::ltfs::volume::LABEL_BLOCK as usize]
                else {
                    panic!("missing actual P1 label");
                };
                if case == 3 {
                    let mut label = crate::ltfs::label::LtfsLabel::parse(bytes).unwrap();
                    label.volume_uuid = Uuid::new_v4();
                    *bytes = label.to_xml().unwrap().to_vec();
                } else {
                    bytes.truncate(5);
                }
            }));
        }
        let original = saved(context(), &runtime, &request);
        if case != 0 {
            permission.close(round);
            runtime
                .block_on(catalog.claim_active_owner_epoch(&serial, round + 1))
                .unwrap();
            permission.renew(
                round + 1,
                super::super::super::permission::Phase::Fencing,
                Instant::now() + Duration::from_secs(120),
            );
            active = takeover(
                &provider,
                &options(2),
                round + 1,
                Some(&permission),
                files.clone(),
            )
            .unwrap();
            active.serving = true;
            permission.renew(
                round + 1,
                super::super::super::permission::Phase::Serving,
                Instant::now() + Duration::from_secs(120),
            );
            status = make_status(2, round + 1, route.frozen_index);
            job = Job::new(request.clone(), &original).unwrap();
        }
        let context = || Context {
            active: &active,
            permission: &permission,
            control: &control,
            status: &status,
            node: 2,
        };
        let mut terminal = false;
        for _ in 0..10 {
            let result = job.poll(context(), &mut publisher, &runtime).unwrap();
            if matches!(result, Publication::Confirmed(true)) {
                terminal = true;
                break;
            }
        }
        let done = saved(context(), &runtime, &request);
        if case >= 2 {
            if case == 5 {
                assert!(
                    faults.format_inflight.load(Ordering::SeqCst),
                    "new epoch did not abort original FORMAT on the same initiator"
                );
                assert_eq!(
                    faults
                        .successful
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|c| c[0] == opcode::FORMAT_MEDIUM)
                        .count(),
                    0
                );
                faults.release_format.store(true, Ordering::SeqCst);
                faults
                    .worker
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .join()
                    .unwrap()
                    .unwrap();
            }
            assert!(
                !terminal,
                "partial format or corrupt P1 label must not publish assignment: {done}"
            );
            assert_eq!(done["status"], "running");
            assert_eq!(done["execution"]["phase"], "formatting");
            assert!(
                runtime
                    .block_on(catalog.require_active_library_readable(&serial, route, active.round))
                    .is_err()
            );
            assert!(
                runtime
                    .block_on(catalog.tape_assignment(&serial, "TEST01L8"))
                    .unwrap()
                    .is_none()
            );
            job = Job::new(request.clone(), &done).unwrap();
            for _ in 0..3 {
                let _ = job.poll(context(), &mut publisher, &runtime).unwrap();
            }
        } else {
            assert!(terminal, "{done}");
            assert_eq!(done["status"], "succeeded");
            assert_eq!(
                runtime
                    .block_on(catalog.tape_assignment(&serial, "TEST01L8"))
                    .unwrap()
                    .unwrap()
                    .volume_id,
                Some(volume)
            );
            let mut v =
                crate::ltfs::volume::LtfsVolume::mount(active.devices[1].dev.as_ref()).unwrap();
            v.set_reservation_guard(Some(active.key));
            assert_eq!(v.label().volume_uuid, volume);
            assert!(v.writable());
            assert_eq!(
                v.verified_committed_index_construct().unwrap().generation,
                1
            );
        }
        assert_eq!(
            faults
                .successful
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c[0] == opcode::FORMAT_MEDIUM)
                .count(),
            1,
            "must never repeat FORMAT even after a partial initialization"
        );
        assert_eq!(
            faults
                .successful
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c[0] == opcode::MOVE_MEDIUM)
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_blank_first_partition_does_not_authorize_erasing_other_partition_data() {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    lib.load_into_drive("TEST01L8", 0).unwrap();
    let transport = lib.drive(0);
    let drive = TapeDrive::new(&transport);
    let mut mode = vec![0u8; 24];
    mode[8] = 0x11;
    mode[9] = 14;
    mode[11] = 1;
    drive.mode_select(&mode, false).unwrap();
    drive.format(1, false).unwrap();
    drive.locate(1, 0, true).unwrap();
    drive.write_block(b"keep partition one data").unwrap();
    drive.write_filemark(0).unwrap();
    assert_eq!(
        crate::ltfs::volume::probe_format(&transport).unwrap(),
        crate::ltfs::volume::FormatProbe::Blank
    );
    assert!(!all_partitions_blank(&transport).unwrap());
    drive.locate(1, 0, true).unwrap();
    let mut buf = vec![0; 4096];
    let n = drive.read_block(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"keep partition one data");
}
