//! Isolated FDB + two simulated drives. No physical device path is accepted.
use super::*;
use tape_rs::changer::{commands::MediumChanger, element::ElementType};

const BARCODES: [&str; 4] = ["READA0L8", "READA1L8", "READB0L8", "READA2L9"];

struct Fixture {
    root: std::path::PathBuf,
    library: SimLibrary,
    catalog: Arc<ManagedCatalog>,
    serial: String,
    route: ActiveLibraryRoute,
    pools: [Uuid; 2],
}

async fn fixture(cluster: &str) -> Fixture {
    let catalog = ManagedCatalog::open(Some(cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pools = [Uuid::new_v4(), Uuid::new_v4()];
    let serial = format!("SIM-SCHEDULE-{namespace}");
    let root = std::env::temp_dir().join(format!("tape-rs-fdb-scheduling-{namespace}"));
    fs::create_dir(&root).unwrap();
    let library = SimLibrary::new(2, 8, 1);
    library.enable_changer_pr();
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

    assert!(catalog.begin_managed_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    for (i, pool) in pools.iter().enumerate() {
        let name = format!("pool-{i}");
        db.execute(
            "INSERT INTO pools(uuid,name,file_limit) VALUES(?1,?2,1000000)",
            rusqlite::params![pool.to_string(), name],
        )
        .unwrap();
        assert_eq!(
            catalog
                .create_pool(
                    namespace,
                    &PoolRecord {
                        library_serial: serial.clone(),
                        id: *pool,
                        name,
                        file_limit: 1_000_000,
                        policy: PoolPolicy::default(),
                        revision: 1
                    }
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
    }
    let mut entries = Vec::new();
    for (i, barcode) in BARCODES.into_iter().enumerate() {
        let pool = pools[usize::from(i == 2)];
        library
            .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), i)
            .unwrap();
        library.load_into_drive(barcode, 0).unwrap();
        let drive = library.drive(0);
        mkltfs(
            &drive,
            &MkltfsOptions {
                volume_id: format!("READ{i:02}"),
                block_size: 64 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let volume = LtfsVolume::mount(&drive).unwrap();
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

        let spool_path = root.join(format!("import-{i}.db"));
        ImportSpool::build(&xml[..], &spool_path, source).unwrap();
        let spool = ImportSpool::open(&spool_path, source).unwrap();

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
        let cache_path = root.join(format!("baseline-{i}.extent.db"));
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

        entries.push((binding, spool));
        fixture_return_to_storage(&library, 0);
    }
    let volumes = entries
        .iter()
        .map(|(b, spool)| ImportVolume {
            pool_id: b.placement.pool_id,
            barcode: b.placement.barcode.clone(),
            volume_id: b.placement.volume_id,
            generation: b.source.generation,
            spool,
        })
        .collect();
    let bindings: Vec<_> = entries.iter().map(|(b, _)| b.clone()).collect();
    let staged = catalog
        .stage_audited_reconcile(namespace, &snapshot, &root, volumes, &bindings)
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

    // Seed four published files via the real media/FDB commit path before starting readonly owners.
    let key = ReservationKey::new(71, 11);
    for (i, (binding, _)) in entries.iter().enumerate() {
        library
            .load_into_drive(&binding.placement.barcode, 0)
            .unwrap();
        let drive = library.drive(0);
        fence(&drive, key).unwrap();
        let mut volume = LtfsVolume::mount(&drive).unwrap();
        volume.set_reservation_guard(Some(key));
        let bytes = format!("content-{i}").into_bytes();
        let path = root.join(format!("upload-{i}"));
        fs::write(&path, &bytes).unwrap();
        let intent = ActiveFileIntent {
            namespace,
            parent: Uuid::nil(),
            parent_path: b"/".to_vec(),
            name: format!("file-{i}").into_bytes(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: serial.clone(),
            pool_id: Some(binding.placement.pool_id),
            owner_epoch: 11,
            volume_id: binding.source.volume_uuid,
            volume_generation: binding.source.generation + 1,
            volume_scan_id: binding.scan.id,
            expected_size: bytes.len() as u64,
            expected_digest: ContentDigest::Sha256(Sha256::digest(&bytes).into()),
        };
        let committed = commit_active_media_batch(
            &catalog,
            volume,
            Uuid::new_v4(),
            &[ActiveMediaUpload {
                intent,
                spool: path,
            }],
        )
        .await
        .unwrap();
        drop(
            publish_active_media_batch(&catalog, committed)
                .await
                .unwrap(),
        );
        fixture_return_to_storage(&library, 0);
        tape_rs::scsi::reservation::release_and_unregister(&drive, key).unwrap();
    }
    drop(entries);
    drop(db);
    Fixture {
        root,
        library,
        catalog: Arc::new(catalog),
        serial,
        route: active,
        pools,
    }
}

struct Provider {
    library: SimLibrary,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
    permission: Arc<Permission>,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> tape_rs::error::Result<Vec<ManagedDevice>> {
        let mut devices: Vec<_> = (0..2)
            .map(|i| ManagedDevice {
                name: format!("模拟驱动器 {i}"),
                serial: format!("SIMDRV{i:04}"),
                kind: DeviceKind::Drive,
                dev: Box::new(self.library.drive_as(i, 71)),
            })
            .collect();
        devices.push(ManagedDevice {
            name: "模拟换带器".into(),
            serial: "SIMLIB0000000001".into(),
            kind: DeviceKind::Changer,
            dev: Box::new(self.library.changer_as(71)),
        });
        for device in &mut devices {
            let inner = std::mem::replace(&mut device.dev, Box::new(self.library.changer_as(71)));
            device.dev = Box::new(ScheduleTransport {
                inner,
                trace: self.trace.clone(),
                permission: self.permission.clone(),
                revoke_on_unload: Arc::new(AtomicBool::new(false)),
                error_after_move: Arc::new(AtomicBool::new(false)),
            });
        }
        Ok(devices)
    }
}
struct Owner {
    tx: mpsc::Sender<ExecRequest>,
    join: Option<std::thread::JoinHandle<()>>,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl Owner {
    fn start(f: &Fixture, round: u64, management: serde_json::Value) -> Self {
        let files = FileService::new(f.root.join(format!("owner-{round}"))).unwrap();
        let status = Arc::new(Mutex::new(NodeStatus {
            catalog_frozen_index: Some(f.route.frozen_index),
            management: Arc::new(management),
            ..Default::default()
        }));
        let permission = Arc::new(Permission::default());
        let trace = Arc::new(Mutex::new(Vec::new()));
        let provider = Provider {
            library: f.library.clone(),
            trace: trace.clone(),
            permission: permission.clone(),
        };
        let control = FdbOwnerControl {
            catalog: f.catalog.clone(),
            serial: f.serial.clone(),
            route: f.route,
            storage: Default::default(),
        };
        let (tx, rx) = channel();
        let (events, event_rx) = channel();
        let join = std::thread::spawn(move || {
            tape_rs::daemon::executor::run_fdb_owner(
                Box::new(provider),
                ExecOptions {
                    node_id: 71,
                    interval: Duration::from_millis(100),
                    demo_write: false,
                    salvage: false,
                    block_size: 64 * 1024,
                    read_idle: Duration::from_secs(300),
                },
                rx,
                events,
                files,
                status,
                control,
            )
        });
        tx.send(ExecRequest::AttachPermission {
            permission: permission.clone(),
        })
        .unwrap();
        permission.renew(
            round,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(120),
        );
        tx.send(ExecRequest::Takeover { round }).unwrap();
        let event = next_owner_event(&event_rx);
        assert!(matches!(event, ExecEvent::Fenced { .. }), "{event:?}");
        permission.renew(
            round,
            Phase::Execution,
            Instant::now() + Duration::from_secs(120),
        );
        tx.send(ExecRequest::Recover { round }).unwrap();
        let event = next_owner_event(&event_rx);
        assert!(matches!(event, ExecEvent::Serving { .. }), "{event:?}");
        permission.renew(
            round,
            Phase::Serving,
            Instant::now() + Duration::from_secs(120),
        );
        Self {
            tx,
            join: Some(join),
            trace,
        }
    }
    fn read(&self, i: usize) -> std::result::Result<Vec<u8>, ReadError> {
        let (reply, rx) = channel();
        self.tx
            .send(ExecRequest::Read {
                path: format!("/file-{i}"),
                reply,
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(15)).unwrap()
    }
    fn expect(&self, i: usize) {
        assert_eq!(self.read(i).unwrap(), format!("content-{i}").into_bytes());
    }
    fn moves(&self) -> usize {
        self.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0xa5)
            .count()
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let (done, rx) = channel();
        let _ = self.tx.send(ExecRequest::Shutdown { done: Some(done) });
        let _ = rx.recv_timeout(Duration::from_secs(15));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        assert!(
            !self
                .trace
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c[0], 0x04 | 0x0a | 0x10 | 0x19 | 0x8d)),
            "只读调度写了介质"
        );
    }
}
fn loaded(f: &Fixture) -> Vec<Option<String>> {
    let dev = f.library.changer_as(71);
    let mut changer = MediumChanger::new(&dev);
    changer.load_address_map().unwrap();
    changer
        .read_all_status()
        .unwrap()
        .into_iter()
        .filter(|e| e.element_type == ElementType::DataTransfer)
        .map(|e| if e.full { e.volume_tag } else { None })
        .collect()
}
fn expect_loaded(f: &Fixture, indices: [Option<usize>; 2]) {
    assert_eq!(
        loaded(f),
        indices.map(|i| i.map(|i| BARCODES[i].to_owned()))
    );
}

#[test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅操作两台模拟驱动器及随机目录"]
fn multi_drive_lru_and_pool_mount_limit() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    {
        let o = Owner::start(&f, 12, json!({}));
        o.expect(0);
        o.expect(2);
        o.expect(0);
        o.expect(1);
        expect_loaded(&f, [Some(0), Some(1)]);
        assert_eq!(
            o.moves(),
            4,
            "应淘汰最久未读的 B 带，两个初始装载加一次换带"
        );
    }
    expect_loaded(&f, [None, None]);
    {
        let o = Owner::start(
            &f,
            13,
            json!({"pools":{f.pools[0].to_string():{"mountlimit":"1"}}}),
        );
        o.expect(2);
        o.expect(0);
        o.expect(1);
        expect_loaded(&f, [Some(2), Some(1)]);
        assert_eq!(o.moves(), 4, "配额满时应腾退同池，保留更老的另一池读带");
    }
    {
        f.library.set_drive_densities(0, vec![(0x5e, true)]);
        f.library.set_drive_densities(1, vec![(0x60, true)]);
        let o = Owner::start(
            &f,
            14,
            json!({"pools":{f.pools[0].to_string():{"mountlimit":"1"}}}),
        );
        o.expect(0);
        o.expect(3); // Free the quota on drive 0, then load the L9 tape into empty drive 1.
        expect_loaded(&f, [None, Some(3)]);
        assert_eq!(o.moves(), 3);
    }
    f.library
        .set_drive_densities(0, vec![(0x5e, true), (0x60, true)]);
    f.library
        .set_drive_densities(1, vec![(0x5e, true), (0x60, true)]);
    {
        let o = Owner::start(&f, 15, json!({"drives":{"SIMDRV0000":{"role":"m"}}}));
        o.expect(0);
        o.expect(2);
        expect_loaded(&f, [None, Some(2)]);
        assert_eq!(o.moves(), 3, "不能使用无读取角色的空驱动器");
    }
    {
        f.library.load_into_drive(BARCODES[0], 0).unwrap();
        let o = Owner::start(&f, 16, json!({"tapes":{BARCODES[0]:{"offline":true}}}));
        o.expect(2);
        o.expect(1);
        expect_loaded(&f, [Some(0), Some(1)]);
        assert_eq!(o.moves(), 3, "不得腾退离线介质");
    }
    expect_loaded(&f, [Some(0), None]);
    {
        let o = Owner::start(
            &f,
            17,
            json!({"pools":{f.pools[0].to_string():{"mountlimit":"1"}},"tapes":{BARCODES[0]:{"offline":true}}}),
        );
        assert!(
            matches!(o.read(1), Err(ReadError::Busy(_))),
            "已装载但禁止腾退的同池介质仍应占用配额"
        );
        assert_eq!(o.moves(), 0);
    }
    fixture_return_to_storage(&f.library, 0);
    {
        let o = Owner::start(
            &f,
            18,
            json!({"drives":{"SIMDRV0000":{"enabled":false},"SIMDRV0001":{"enabled":false}}}),
        );
        assert!(matches!(o.read(0), Err(ReadError::Busy(_))));
        assert_eq!(o.moves(), 0);
    }
    {
        let o = Owner::start(&f, 19, json!({}));
        o.expect(0);
        o.expect(2);
        for slot in 0..8 {
            let _ = f.library.insert_cartridge(
                SimCartridge::blank(&format!("FILL{slot:02}L8"), 128 << 20),
                slot,
            );
        }
        let before = o.moves();
        assert!(
            matches!(o.read(1), Err(ReadError::Busy(_))),
            "无空存储槽时不能先退带再寻找归槽位置"
        );
        expect_loaded(&f, [Some(0), Some(2)]);
        assert_eq!(o.moves(), before);
    }
    fs::remove_dir_all(&f.root).unwrap();
}
