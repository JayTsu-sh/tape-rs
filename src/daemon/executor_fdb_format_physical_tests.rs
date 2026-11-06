// Explicit operator-owned hardware acceptance. Control initialization uses the
// existing isolated test fixture; this is not a three-node Raft acceptance.
mod physical_format_safety {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Trace {
        root: std::path::PathBuf,
        formats: AtomicUsize,
        fault_sent: AtomicBool,
        commands: Mutex<Vec<u8>>,
    }

    impl Trace {
        fn save(&self, name: &str, value: &Value) {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(name))
                .unwrap();
            file.write_all(value.to_string().as_bytes()).unwrap();
            file.sync_all().unwrap();
            std::fs::File::open(&self.root).unwrap().sync_all().unwrap();
        }
        fn wait(&self, name: &str) {
            let end = Instant::now() + Duration::from_secs(1800);
            while !self.root.join(name).is_file() {
                assert!(
                    Instant::now() < end,
                    "等待原实验控制器回执超时；不重放设备操作"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        fn command(&self, cdb: &[u8], kind: DeviceKind) {
            if kind == DeviceKind::Drive {
                self.commands.lock().unwrap().push(cdb[0]);
            }
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.root.join("CDB.jsonl"))
                .unwrap();
            writeln!(file, "{}", json!({"device":format!("{kind:?}"),"cdb":cdb})).unwrap();
            file.sync_data().unwrap();
        }
    }

    struct PhysicalDevice {
        inner: crate::scsi::device::ScsiDevice,
        kind: DeviceKind,
        trace: Arc<Trace>,
    }
    impl TapeTransport for PhysicalDevice {
        fn execute_no_data(
            &self,
            cdb: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.trace.command(cdb, self.kind);
            let result = self.inner.execute_no_data(cdb, timeout);
            if self.kind == DeviceKind::Drive && cdb[0] == 0x04 && result.is_ok() {
                self.trace.formats.fetch_add(1, Ordering::SeqCst);
                if !self.trace.fault_sent.swap(true, Ordering::SeqCst) {
                    self.trace.save(
                        "FORMAT-COMPLETED.json",
                        &json!({"status":"GOOD","cdb":cdb,"pid":std::process::id()}),
                    );
                    self.trace.wait("FAULT-APPLIED.json");
                }
            }
            result
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            data: &mut [u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.trace.command(cdb, self.kind);
            self.inner.execute_read(cdb, data, timeout)
        }
        fn execute_write(
            &self,
            cdb: &[u8],
            data: &[u8],
            timeout: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            self.trace.command(cdb, self.kind);
            self.inner.execute_write(cdb, data, timeout)
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
        fn session_epoch(&self) -> Option<u64> {
            self.inner.session_epoch()
        }
    }

    struct PhysicalProvider {
        trace: Arc<Trace>,
    }
    impl DeviceProvider for PhysicalProvider {
        fn open_all(&self) -> Result<Vec<ManagedDevice>> {
            let mut devices = Vec::new();
            for (path, serial, kind) in [
                ("/dev/sg4", "117018605B", DeviceKind::Drive),
                ("/dev/sg5", "55L3A78011W9LL01", DeviceKind::Changer),
            ] {
                assert_eq!(
                    crate::scsi::inquiry::fc_target_port(std::path::Path::new(path))?.as_deref(),
                    Some("0x5000e1117018605d")
                );
                let device = crate::scsi::device::ScsiDevice::open(path)?;
                assert_eq!(
                    crate::scsi::inquiry::read_unit_serial(&device).as_deref(),
                    Some(serial)
                );
                devices.push(ManagedDevice {
                    name: path.into(),
                    serial: serial.into(),
                    kind,
                    dev: Box::new(PhysicalDevice {
                        inner: device,
                        kind,
                        trace: self.trace.clone(),
                    }),
                });
            }
            Ok(devices)
        }
    }

    #[tokio::test]
    #[ignore = "破坏性：仅B主605d/AA47；须停原设备owner、独立FDB故障控制器及显式确认计划"]
    async fn real_b_format_guard_and_partition_protection() {
        use crate::managed_catalog::{ManagedCatalog, ManagementRequest};
        let root = std::path::PathBuf::from(std::env::var("TAPE_RS_FORMAT_ACCEPT_ROOT").unwrap());
        let plan: Value =
            serde_json::from_slice(&std::fs::read(root.join("PLAN.json")).unwrap()).unwrap();
        assert_eq!(plan["confirm_format"], true);
        assert_eq!(plan["barcode"], "8A0047L8");
        assert_eq!(plan["library"], "55L3A78011W9LL01");
        let quiet: Value =
            serde_json::from_slice(&std::fs::read(root.join("QUIET.json")).unwrap()).unwrap();
        assert_eq!(quiet["operation"], plan["operation"]);
        assert_eq!(quiet["all_original_nodes_stopped"], true);
        assert_eq!(quiet["all_B_devices_quiet"], true);
        let operation = uuid::Uuid::parse_str(plan["operation"].as_str().unwrap()).unwrap();
        let volume = uuid::Uuid::parse_str(plan["volume"].as_str().unwrap()).unwrap();
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        assert!(cluster.starts_with(root.to_str().unwrap()));
        let _network = unsafe { foundationdb::boot() };
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut f, control, pool, status) = Box::pin(native_fixture_for_serial(
            catalog.clone(),
            Some("55L3A78011W9LL01"),
        ))
        .await;
        let trace = Arc::new(Trace {
            root,
            formats: AtomicUsize::new(0),
            fault_sent: AtomicBool::new(false),
            commands: Mutex::new(Vec::new()),
        });
        let permit = Arc::new(Permission::default());
        assert!(permit.renew(
            f.owner.round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(3600)
        ));
        f.owner = takeover(
            &PhysicalProvider {
                trace: trace.clone(),
            },
            &f.opts,
            f.owner.round,
            Some(&permit),
            f.owner.files.clone(),
        )
        .unwrap();
        assert!(permit.renew(
            f.owner.round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(3600)
        ));
        f.permit = permit;
        let request=ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":operation.to_string(),"change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"8A0047L8","volume":volume.to_string(),"confirm_format":true,"force_format":true}})).unwrap();
        trace.save("TASK-INTENT.json", request.value());
        catalog
            .admit_management(&control.serial, control.route, f.owner.round, &request)
            .await
            .unwrap();
        let intake = catalog
            .start_native_media(&control.serial, control.route, f.owner.round, 1, operation)
            .await
            .unwrap()
            .unwrap();
        let task = intake.format_task().unwrap();
        let result = Box::pin(prepare_assign(
            &mut f.owner,
            &f.permit,
            &control,
            &f.opts,
            &intake,
        ))
        .await;
        assert!(
            matches!(result, Err(ReadError::Lost(_))),
            "失联不能继续完成: {result:?}"
        );
        assert_eq!(trace.formats.load(Ordering::SeqCst), 1);
        assert_eq!(
            trace.commands.lock().unwrap().last(),
            Some(&0x04),
            "FORMAT后没有任何新CDB"
        );
        trace.save("GUARD-STOPPED.json", &json!({"operation":operation.to_string(),"volume":volume.to_string(),"error":format!("{result:?}"),"formats":1,"last_cdb":4}));
        trace.wait("FDB-RESTORED.json");
        let state = catalog
            .native_device_status(&task, f.owner.round)
            .await
            .unwrap();
        assert_eq!(state["phase"], "format-started");
        let device = f.device();
        let checked = format_guard::Guard::new(device, || {
            format_authority(
                device,
                &f.permit,
                &control,
                &task,
                f.owner.round,
                "format-started",
            )
        });
        let drive = TapeDrive::new(&checked);
        drive.test_unit_ready().unwrap();
        assert_eq!(drive.defined_partitions().unwrap(), 2);
        let sentinel = format!("P1-preservation-{operation}").into_bytes();
        trace.save("P1-TEST-WRITE-INTENT.json", &json!({"operation":operation.to_string(),"partition":1,"block":0,"bytes":sentinel.len()}));
        drive.locate(1, 0, true).unwrap();
        drive.write_block(&sentinel).unwrap();
        drive.write_filemark(0).unwrap();
        let before = trace.commands.lock().unwrap().len();
        let rejected = format_allowed(&checked, f.owner.files.spool_dir(), f.budget(), false);
        assert!(
            matches!(rejected, Err(ReadError::Failed(_))),
            "非force必须拒绝: {rejected:?}"
        );
        assert!(
            !trace.commands.lock().unwrap()[before..]
                .iter()
                .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
        );
        drive.locate(1, 0, true).unwrap();
        let mut data = [0u8; 1024];
        let n = drive.read_block(&mut data).unwrap();
        assert_eq!(&data[..n], sentinel.as_slice());
        trace.save("NONFORCE-PRESERVED.json", &json!({"error":format!("{rejected:?}"),"P1_bytes_verified":n,"formats":trace.formats.load(Ordering::SeqCst)}));
        // The durable GOOD FORMAT receipt above proves partitioning completed.
        // Finish that same original UUID without replaying FORMAT; no unknown
        // execution is inferred from a missing label or from a timeout.
        trace.save("ORIGINAL-FORMAT-FINISH-INTENT.json", &json!({"operation":operation.to_string(),"volume":volume.to_string(),"quick":true,"known_good_format_receipt":"FORMAT-COMPLETED.json","discard_only_test_sentinel":true}));
        format_guard::format(
            device,
            &MkltfsOptions {
                volume_id: "8A0047".into(),
                volume_uuid: Some(volume),
                block_size: f.opts.block_size,
                quick: true,
                ..Default::default()
            },
            || {
                format_authority(
                    device,
                    &f.permit,
                    &control,
                    &task,
                    f.owner.round,
                    "format-started",
                )
            },
        )
        .unwrap();
        assert_eq!(trace.formats.load(Ordering::SeqCst), 1);
        drop(checked);
        assert!(
            Box::pin(prepare_assign(
                &mut f.owner,
                &f.permit,
                &control,
                &f.opts,
                &intake
            ))
            .await
            .unwrap()
        );
        for _ in 0..10 {
            Box::pin(fdb_intake::apply(
                &mut f.owner,
                &f.permit,
                &control,
                &f.opts,
                &status,
                1,
                operation,
            ))
            .await
            .unwrap();
            let receipt = catalog
                .management_task(&control.serial, control.route, 1, operation)
                .await
                .unwrap()
                .unwrap();
            if receipt["status"] == "succeeded" {
                trace.save("ASSIGN-COMPLETED.json", &receipt);
                break;
            }
        }
        let receipt = catalog
            .management_task(&control.serial, control.route, 1, operation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt["status"], "succeeded");
        let assignment = catalog
            .tape_assignment(&control.serial, "8A0047L8")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(assignment.volume_id, Some(volume));
        let unassign=ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":{"kind":"media-unassign","pool":pool.id.to_string(),"barcode":"8A0047L8","volume":volume.to_string(),"revision":assignment.revision}})).unwrap();
        let done = execute(&mut f, &control, &status, &unassign).await;
        assert_eq!(done["status"], "succeeded");
        trace.save("UNASSIGN-COMPLETED.json", &done);
        assert!(
            catalog
                .tape_assignment(&control.serial, "8A0047L8")
                .await
                .unwrap()
                .is_none()
        );
        let (_, rows) = elements(&f.owner).unwrap();
        assert_eq!(cartridge(&rows, "8A0047L8").unwrap().address, 1005);
        for d in &f.owner.devices {
            reservation::release_and_unregister(d.dev.as_ref(), f.owner.key).unwrap();
        }
        f.permit.close(f.owner.round);
        trace.save("PASS.json", &json!({"operation":operation.to_string(),"volume":volume.to_string(),"formats":trace.formats.load(Ordering::SeqCst),"nonforce_preserved_P1":true,"original_intent_recovered":true,"AA47_home":1005,"private_FDB_only":true,"control_fixture_not_Raft_acceptance":true,"PR_released":true}));
    }
}
