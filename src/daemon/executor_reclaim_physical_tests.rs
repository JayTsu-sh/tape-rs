//! Operator-owned proof of the completed legacy reclaim checkpoint. No quorum
//! election, migration or multi-host HA claim is made by this fixture.
use super::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Evidence {
    root: PathBuf,
    operation: String,
    counts: Mutex<(usize, usize)>,
}
impl Evidence {
    fn save(&self, name: &str, value: Value) {
        let pending = self.root.join(format!(".{name}.pending"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)
            .unwrap();
        serde_json::to_writer(
            &mut file,
            &json!({"operation":self.operation,"value":value}),
        )
        .unwrap();
        file.sync_all().unwrap();
        std::fs::hard_link(&pending, self.root.join(name)).unwrap();
        std::fs::remove_file(pending).unwrap();
        std::fs::File::open(&self.root).unwrap().sync_all().unwrap();
    }
    fn command(&self, kind: DeviceKind, cdb: &[u8]) {
        let mut counts = self.counts.lock().unwrap();
        counts.0 += 1;
        if kind == DeviceKind::Drive && cdb[0] == 4 {
            counts.1 += 1;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.root.join("CDB.jsonl"))
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"device":format!("{kind:?}"),"cdb":cdb,"ordinal":counts.0})
        )
        .unwrap();
        file.sync_data().unwrap();
    }
}
struct Physical {
    device: ScsiDevice,
    kind: DeviceKind,
    evidence: Arc<Evidence>,
}
impl TapeTransport for Physical {
    fn identity(&self) -> &str {
        self.device.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.device.session_epoch()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<crate::scsi::device::ScsiResult> {
        self.evidence.command(self.kind, cdb);
        let result = self.device.execute_no_data(cdb, timeout);
        if self.kind == DeviceKind::Drive && cdb[0] == 4 {
            self.evidence.save("FORMAT-RETURN.json",json!({"status":result.as_ref().ok().map(|r|r.status),"error":result.as_ref().err().map(ToString::to_string)}));
            assert!(
                result.as_ref().is_ok_and(|r| r.status == 0),
                "FORMAT未取得GOOD，保留原实验，不重试: {result:?}"
            );
        }
        result
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        data: &mut [u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        self.evidence.command(self.kind, cdb);
        self.device.execute_read(cdb, data, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        self.evidence.command(self.kind, cdb);
        self.device.execute_write(cdb, data, timeout)
    }
}
struct Provider(Arc<Evidence>);
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let mut devices = Vec::new();
        for (path, serial, kind) in [
            ("/dev/sg4", "117018605B", DeviceKind::Drive),
            ("/dev/sg5", "55L3A78011W9LL01", DeviceKind::Changer),
        ] {
            assert_eq!(
                crate::scsi::inquiry::fc_target_port(Path::new(path))?.as_deref(),
                Some("0x5000e1117018605d")
            );
            let device = ScsiDevice::open(path)?;
            assert_eq!(read_unit_serial(&device).as_deref(), Some(serial));
            let prior = reservation::read_status(&device)?;
            assert!(
                prior.holder.is_none(),
                "原设备owner未释放，不抢占: {prior:?}"
            );
            self.0.save(
                &format!("PR-BEFORE-{kind:?}.json"),
                json!({"holder":null,"keys":prior.keys}),
            );
            devices.push(ManagedDevice {
                name: path.into(),
                serial: serial.into(),
                kind,
                dev: Box::new(Physical {
                    device,
                    kind,
                    evidence: self.0.clone(),
                }),
            });
        }
        Ok(devices)
    }
}

#[test]
#[ignore = "破坏性：B主605d/TD8/AA0000L8，清空该带；须显式operator计划、原三owner撤权及设备排空"]
fn real_b_completed_reclaim_waits_without_repeating_format() {
    use crate::daemon::directory::{Directory, PoolRow};
    use crate::daemon::node::{NodeStatus, TapeRow};
    use crate::daemon::permission::Phase;
    use crate::daemon::state::{Command, ControlState};
    let root = PathBuf::from(std::env::var("TAPE_RS_RECLAIM_ACCEPT_ROOT").unwrap());
    let plan: Value =
        serde_json::from_slice(&std::fs::read(root.join("PLAN.json")).unwrap()).unwrap();
    assert_eq!(plan["confirm_format"], true);
    assert_eq!(plan["allow_data_loss"], true);
    assert_eq!(plan["barcode"], "AA0000L8");
    assert_eq!(plan["library"], "55L3A78011W9LL01");
    let quiet: Value =
        serde_json::from_slice(&std::fs::read(root.join("QUIET.json")).unwrap()).unwrap();
    assert_eq!(quiet["operation"], plan["operation"]);
    assert_eq!(quiet["all_original_nodes_stopped"], true);
    assert_eq!(quiet["all_B_devices_quiet"], true);
    let evidence = Arc::new(Evidence {
        root: root.clone(),
        operation: plan["operation"].as_str().unwrap().into(),
        counts: Mutex::new((0, 0)),
    });
    let pool = plan["pool"].as_str().unwrap();
    let mut directory = Directory::open(&root.join("directory.db")).unwrap();
    let mut control = ControlState::default();
    for (index, command) in [
        Command::PoolCreate {
            uuid: pool.into(),
            name: "legacy-check".into(),
            file_limit: 100,
        },
        Command::TapeAssign {
            barcode: "AA0000L8".into(),
            pool: pool.into(),
        },
        Command::TapeState {
            barcode: "AA0000L8".into(),
            state: tape_state::RECLAIMING.into(),
        },
        Command::TapeState {
            barcode: "AA0000L8".into(),
            state: tape_state::REFORMATTING.into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        evidence.save(&format!("CONTROL-INTENT-{index}.json"),json!({"index":index+1,"command":serde_json::from_slice::<Value>(&command.encode()).unwrap()}));
        let applied = control.apply(index as u64 + 1, &command);
        directory
            .apply(index as u64 + 1, &command, &applied)
            .unwrap();
    }
    assert_eq!(control.tape_state["AA0000L8"], tape_state::REFORMATTING);
    let files = FileService::with_options(
        root.join("spool"),
        Default::default(),
        Some(root.join("directory.db")),
    )
    .unwrap();
    files.bind_pool(pool);
    assert_eq!(files.live_on("AA0000L8"), Some((0, 0)));
    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        7,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(3600)
    ));
    let options = ExecOptions {
        node_id: 1,
        interval: Duration::from_millis(100),
        demo_write: false,
        salvage: false,
        block_size: 64 << 10,
        read_idle: Duration::from_secs(60),
    };
    let mut active = takeover(
        &Provider(evidence.clone()),
        &options,
        7,
        Some(&permission),
        files.clone(),
    )
    .unwrap();
    assert!(permission.renew(
        7,
        Phase::Serving,
        Instant::now() + Duration::from_secs(3600)
    ));
    let inv = inventory(&active.devices).unwrap();
    assert_eq!(inv.slots.get("AA0000L8"), Some(&1002));
    let slot = inv
        .drives
        .iter()
        .find(|d| active.devices[d.dev].serial == "117018605B")
        .unwrap();
    assert!(slot.loaded.is_none());
    let drive = slot.dev;
    evidence.save("FORMAT-INTENT.json",json!({"barcode":"AA0000L8","home":1002,"original_directory_references":0,"operator_checkpoint_only_not_raft_quorum":true}));
    move_medium(&active.devices, 1002, slot.addr).unwrap();
    TapeDrive::new(active.devices[drive].dev.as_ref())
        .load()
        .unwrap();
    wait_ready(active.devices[drive].dev.as_ref()).unwrap();
    let status = Arc::new(Mutex::new(NodeStatus {
        pools: vec![PoolRow {
            uuid: pool.into(),
            name: "legacy-check".into(),
            file_limit: 100,
            tapes: vec!["AA0000L8".into()],
        }],
        tapes: std::collections::BTreeMap::from([(
            "AA0000L8".into(),
            TapeRow {
                state: tape_state::REFORMATTING.into(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }));
    active.reclaim = Some(Reclaim {
        barcode: "AA0000L8".into(),
        completed: false,
        drive,
        todo: vec![],
        retry: vec![],
        passes: 0,
        waits: 0,
        copied: 0,
        bytes: 0,
        superseded: 0,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(finish_reclaim(&mut active, &options, &files, &status, &tx).unwrap());
    let mut receipt = None;
    for event in rx.try_iter() {
        if let ExecEvent::Reclaimed {
            round,
            barcode,
            volume_uuid,
            ..
        } = event
        {
            assert_eq!(round, 7);
            assert_eq!(barcode, "AA0000L8");
            assert!(receipt.replace(volume_uuid).is_none());
        }
    }
    let volume = receipt.unwrap();
    let before = *evidence.counts.lock().unwrap();
    assert_eq!(before.1, 1);
    evidence.save("RECEIPT-WITHHELD.json",json!({"volume":volume,"state":control.tape_state["AA0000L8"],"counts":before,"completed":active.reclaim.as_ref().unwrap().completed}));
    for _ in 0..50 {
        assert!(!drive_reclaim(&mut active, &options, &files, &status, &tx).unwrap());
        assert!(active.reclaim.as_ref().unwrap().completed);
        assert_eq!(
            *evidence.counts.lock().unwrap(),
            before,
            "等待回执时不得续发设备命令"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    evidence.save(
        "WITHHELD-PROVED.json",
        json!({"calls":50,"new_CDBs":0,"formats":1,"volume":volume}),
    );
    let command = Command::TapeReclaimed {
        barcode: "AA0000L8".into(),
        volume_uuid: volume.clone(),
    };
    evidence.save("COMPLETION-APPLY-INTENT.json",json!({"volume":volume,"command":serde_json::from_slice::<Value>(&command.encode()).unwrap()}));
    let applied = control.apply(5, &command);
    directory.apply(5, &command, &applied).unwrap();
    assert_eq!(control.tape_state["AA0000L8"], tape_state::APPENDABLE);
    status
        .lock()
        .unwrap()
        .tapes
        .get_mut("AA0000L8")
        .unwrap()
        .state = control.tape_state["AA0000L8"].clone();
    assert!(!drive_reclaim(&mut active, &options, &files, &status, &tx).unwrap());
    assert!(active.reclaim.is_none());
    assert_eq!(*evidence.counts.lock().unwrap(), before);
    let inv = inventory(&active.devices).unwrap();
    assert_eq!(inv.slots.get("AA0000L8"), Some(&1002));
    assert!(
        inv.drives
            .iter()
            .find(|d| d.dev == drive)
            .unwrap()
            .loaded
            .is_none()
    );
    for d in &active.devices {
        reservation::release_and_unregister(d.dev.as_ref(), active.key).unwrap();
        assert!(
            reservation::read_status(d.dev.as_ref())
                .unwrap()
                .holder
                .is_none()
        );
    }
    permission.close(7);
    evidence.save("PASS.json",json!({"formats":1,"blocked_calls":50,"no_CDB_before_receipt_apply":true,"original_completion_applied":true,"AA0000_home":1002,"PR_released":true,"volume":volume,"operator_fixture_not_Raft_HA":true}));
}
