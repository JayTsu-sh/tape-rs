//! Isolated FDB + two simulated drives. No physical device path is accepted.
use super::*;
use tape_rs::changer::{commands::MediumChanger, element::ElementType};
use tape_rs::client::fdb::UploadCondition;

const BARCODES: [&str; 4] = ["READA0L8", "READA1L8", "READB0L8", "READA2L9"];

struct Fixture {
    root: std::path::PathBuf,
    library: SimLibrary,
    catalog: Arc<ManagedCatalog>,
    serial: String,
    route: ActiveLibraryRoute,
    pools: [Uuid; 2],
    pool_roots: [Uuid; 2],
}

fn assert_upload_committed(
    committed: tape_rs::client::fdb::CommittedUpload,
    directory: &std::path::Path,
    version: u64,
) {
    use base64::Engine;
    let prepared = upload_prepared(directory);
    let identity: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(prepared["metadata"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        committed.version(),
        tape_rs::client::FileVersion::Fdb {
            namespace: Uuid::parse_str(identity["namespace"].as_str().unwrap()).unwrap(),
            inode: Uuid::parse_str(identity["file"].as_str().unwrap()).unwrap(),
            version,
        }
    );
    assert_eq!(
        committed.operation(),
        Uuid::parse_str(identity["operation"].as_str().unwrap()).unwrap()
    );
    for field in ["namespace", "operation", "id", "size", "sha256"] {
        let key = if field == "id" { "file" } else { field };
        assert_eq!(committed.details()[field], identity[key], "{field}");
    }
}

fn upload_prepared(directory: &std::path::Path) -> serde_json::Value {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("request.json")).unwrap()).unwrap();
    assert_eq!(value["version"], 2);
    assert!(value["prepared"].is_object());
    value["prepared"].clone()
}

// A self-consistent local wire fixture for side-effect-free negative queries.
// There is deliberately no body, so it cannot admit an unconfirmed upload.
fn upload_query_fixture(prepared: &serde_json::Value) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let identity: serde_json::Value = serde_json::from_slice(
        &base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            prepared["metadata"].as_str().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let path = String::from_utf8(
        base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            prepared["path"].as_str().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let original = json!({"version":2,"library":prepared["library"],"pool":prepared["pool"],"path":path,
        "namespace":prepared["namespace"],"parent":if identity["previous"].is_null() {identity["parent"].clone()} else {serde_json::Value::Null},"inode":if identity["previous"].is_null() {serde_json::Value::Null} else {identity["file"].clone()},
        "previous":identity["previous"],"operation":identity["operation"],"size":identity["size"],"sha256":identity["sha256"],"barcode":identity["barcode"]});
    let mut value = json!({"version":2,"original":original,"prepared":prepared});
    value["record_digest"] = json!(format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"tape-rs/local-upload/2")
            .chain_update(value.to_string())
            .finalize()
    ));
    format!("{value}\n").into_bytes()
}

fn upload_replacement(node: &serde_json::Value) -> UploadCondition {
    UploadCondition::Replace(tape_rs::client::FileVersion::Fdb {
        namespace: Uuid::parse_str(node["namespace"].as_str().unwrap()).unwrap(),
        inode: Uuid::parse_str(node["id"].as_str().unwrap()).unwrap(),
        version: node["version"].as_u64().unwrap(),
    })
}

async fn fixture(cluster: &str) -> Fixture {
    fixture_with_objects(cluster, &[]).await
}

async fn fixture_with_objects(cluster: &str, objects: &[String]) -> Fixture {
    fixture_with_extra_pool(cluster, objects, None).await
}

async fn fixture_with_extra_pool(
    cluster: &str,
    objects: &[String],
    empty_pool: Option<Uuid>,
) -> Fixture {
    let catalog = ManagedCatalog::open(Some(cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pools = [Uuid::new_v4(), Uuid::new_v4()];
    let serial = format!("SIM-SCHEDULE-{namespace}");
    let root = std::env::temp_dir().join(format!("tape-rs-fdb-scheduling-{namespace}"));
    fs::create_dir(&root).unwrap();
    eprintln!("fdb_schedule_fixture_workspace={}", root.display());
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
    for (i, pool) in pools.iter().chain(empty_pool.iter()).enumerate() {
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
                        created_at_millis: 1_793_405_000_123,
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
        let mut volume = LtfsVolume::mount(&drive).unwrap();
        if i == 0 && !objects.is_empty() {
            for path in objects {
                let content = path
                    .strip_prefix("/file-")
                    .map(|id| format!("content-{id}").into_bytes())
                    .unwrap_or_else(|| b"object".to_vec());
                if path == "/k0000" {
                    let padding = "x".repeat(2_000_000);
                    volume.append_file_with_xattrs(path, &mut content.as_slice(), &[
                        ("tapers.s3.headers", r#"{"content-type":"text/plain","cache-control":"max-age=60","x-amz-meta-origin":"fdb-test"}"#),
                        ("unrelated-large-attribute", &padding),
                    ]).unwrap();
                } else {
                    volume.append_file(path, &mut content.as_slice()).unwrap();
                }
            }
            volume.commit().unwrap();
        }
        let baseline = ActiveIndexIdentity::from(volume.committed_index_construct());
        let xml = volume.index().to_xml().unwrap();
        if i == 0 && !objects.is_empty() {
            for record in tape_rs::daemon::files::catalog_of(volume.index()).0 {
                db.execute("INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", rusqlite::params![pool.to_string(), record.path, barcode, baseline.generation as i64, record.length as i64, record.sha256, record.version.0 as i64, record.version.1 as i64, i64::from(record.deleted), record.metadata.to_string()]).unwrap();
            }
        }
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
    if objects.is_empty() {
        assert_eq!(staged.paths, 0);
    } else {
        assert!(staged.paths >= objects.len() as u64);
    }
    catalog
        .confirm_bound_staged_import(namespace)
        .await
        .unwrap();
    while !catalog
        .materialize_bound_import_page(namespace, 100)
        .await
        .unwrap()
        .complete()
    {}
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
    let verified = catalog.verify_bound_import_copies(namespace).await.unwrap();
    if objects.is_empty() {
        assert_eq!(verified, 0);
    } else {
        assert!(verified >= objects.len() as u64);
    }
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

    let mut pool_roots = [Uuid::nil(); 2];
    for (i, pool) in pools.iter().enumerate() {
        let Some(ResolvedPath::Directory { id, .. }) = catalog
            .resolve_active_path(&serial, format!("/{pool}").as_bytes())
            .await
            .unwrap()
        else {
            panic!("missing pool root");
        };
        pool_roots[i] = id;
    }
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
            barcode_constraint: None,
            namespace,
            parent: pool_roots[usize::from(i == 2)],
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
            64 << 20,
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
        pool_roots,
    }
}

struct Provider {
    library: SimLibrary,
    serial: String,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
    permission: Arc<Permission>,
    fail_vci: Arc<AtomicBool>,
    fail_read: Arc<AtomicBool>,
}

struct FailVci {
    inner: Box<dyn TapeTransport + Send>,
    fail: Arc<AtomicBool>,
    fail_read: Arc<AtomicBool>,
}
impl TapeTransport for FailVci {
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
        bytes: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if cdb[0] == 0x08 && self.fail_read.swap(false, Ordering::SeqCst) {
            return Err(tape_rs::error::TapeError::ScsiCommand {
                status: 2,
                sense_key: 3,
                asc: 0x11,
                ascq: 0,
                sense: Some(Box::new(tape_rs::scsi::sense::SenseInfo::from_bytes(&[
                    0x72, 3, 0x11, 0, 0, 0, 0, 0,
                ]))),
            });
        }
        self.inner.execute_read(cdb, bytes, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        bytes: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if cdb[0] == 0x8d && self.fail.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("模拟 S4 后 VCI 写入结果丢失").into());
        }
        self.inner.execute_write(cdb, bytes, timeout)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
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
            serial: self.serial.clone(),
            kind: DeviceKind::Changer,
            dev: Box::new(self.library.changer_as(71)),
        });
        for device in &mut devices {
            let inner = std::mem::replace(&mut device.dev, Box::new(self.library.changer_as(71)));
            device.dev = Box::new(ScheduleTransport {
                inner: Box::new(FailVci {
                    inner,
                    fail: self.fail_vci.clone(),
                    fail_read: self.fail_read.clone(),
                }),
                trace: self.trace.clone(),
                permission: self.permission.clone(),
                revoke_on_unload: Arc::new(AtomicBool::new(false)),
                error_after_move: Arc::new(AtomicBool::new(false)),
            });
        }
        Ok(devices)
    }
}
// Explicit policy recovery for simulated-media acceptance scenes. A restart
// alone preserves offline state; only this durable device task may clear it.
fn online_tape(f: &Fixture, round: u64, barcode: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(online_tape_async(f, round, barcode));
}
async fn online_tape_async(f: &Fixture, round: u64, barcode: &str) {
    let assignment = f
        .catalog
        .tape_assignment(&f.serial, barcode)
        .await
        .unwrap()
        .unwrap();
    let policy = f
        .catalog
        .active_resource_policy(&f.serial, f.route, "tape", barcode)
        .await
        .unwrap();
    let window = f
        .catalog
        .request_window(f.route.route.namespace)
        .await
        .unwrap();
    let request = tape_rs::managed_catalog::ManagementRequest::parse(json!({
        "version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),
        "epoch":window.accepting,"operation":Uuid::new_v4().to_string(),
        "change":{"kind":"media-online","pool":assignment.pool_id.to_string(),
            "volume":assignment.volume_id.unwrap().to_string(),"barcode":barcode,
            "revision":policy["revision"]}
    }))
    .unwrap();
    f.catalog
        .admit_management(&f.serial, f.route, round, &request)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = f
            .catalog
            .management_task(&f.serial, f.route, request.epoch(), request.operation())
            .await
            .unwrap()
            .unwrap();
        if task["status"] == "succeeded" {
            break;
        }
        assert!(
            task["status"] != "failed" && Instant::now() < deadline,
            "{task}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
struct Owner {
    events: Receiver<ExecEvent>,
    permission: Arc<Permission>,
    pools: [Uuid; 2],
    tx: mpsc::Sender<ExecRequest>,
    join: Option<std::thread::JoinHandle<()>>,
    trace: Arc<Mutex<Vec<Vec<u8>>>>,
    writable: bool,
    files: Arc<FileService>,
    status: SharedStatus,
    fail_vci: Arc<AtomicBool>,
    fail_read: Arc<AtomicBool>,
}
impl Owner {
    // Scheduling scenes explicitly install native FDB policy before starting
    // an executor. Production never falls back to this frozen status JSON.
    fn start_with_policy(f: &Fixture, round: u64, management: serde_json::Value) -> Self {
        Self::start_with_policy_mode(f, round, management, false)
    }
    fn start_with_policy_mode(
        f: &Fixture,
        round: u64,
        management: serde_json::Value,
        writable: bool,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let owner = f.catalog.active_library_owner(&f.serial).await.unwrap().unwrap().1;
            let ns = f.route.route.namespace;
            let mut changes = Vec::new();
            for pool in f.pools {
                let saved = f.catalog.active_pool_for_route(&f.serial,f.route,pool).await.unwrap().unwrap();
                let limit = management["pools"][pool.to_string()]["mountlimit"].as_str().unwrap_or("0").parse::<u64>().unwrap();
                changes.push(json!({"kind":"pool","id":pool.to_string(),"revision":saved.revision,"patch":{"mount_limit":limit}}));
            }
            for serial in ["SIMDRV0000", "SIMDRV0001"] {
                let saved = f.catalog.active_resource_policy(&f.serial,f.route,"drive",serial).await.unwrap();
                let input = &management["drives"][serial];
                changes.push(json!({"kind":"drive","serial":serial,"revision":saved["revision"],"patch":{"role":input["role"].as_str().unwrap_or("mrg"),"enabled":input["enabled"].as_bool().unwrap_or(true)}}));
            }
            for barcode in BARCODES {
                let saved = f.catalog.active_resource_policy(&f.serial,f.route,"tape",barcode).await.unwrap();
                let input = &management["tapes"][barcode];
                let mut patch = json!({"blacklisted":input["blacklisted"].as_bool().unwrap_or(false)});
                if input["online"] == false { patch["online"] = json!(false); }
                else if input["online"] == true { assert_eq!(saved["online"], true, "上线必须执行实带验证任务"); }
                changes.push(json!({"kind":"tape","barcode":barcode,"pool":saved["pool"],"volume":saved["volume"],"revision":saved["revision"],"patch":patch}));
            }
            for change in changes {
                let request=tape_rs::managed_catalog::ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap();
                f.catalog.admit_management(&f.serial,f.route,owner,&request).await.unwrap();
                assert_eq!(f.catalog.apply_management(&f.serial,f.route,owner,1,request.operation()).await.unwrap()["status"],"succeeded");
            }
        });
        Self::start_mode(f, round, management, writable)
    }

    fn start(f: &Fixture, round: u64, management: serde_json::Value) -> Self {
        Self::start_mode(f, round, management, false)
    }
    fn start_mode(f: &Fixture, round: u64, management: serde_json::Value, writable: bool) -> Self {
        let files = FileService::new(f.root.join(format!("owner-{round}"))).unwrap();
        let status = Arc::new(Mutex::new(NodeStatus {
            catalog_frozen_index: Some(f.route.frozen_index),
            management: Arc::new(management),
            ..Default::default()
        }));
        let permission = Arc::new(Permission::default());
        let trace = Arc::new(Mutex::new(Vec::new()));
        let fail_vci = Arc::new(AtomicBool::new(false));
        let fail_read = Arc::new(AtomicBool::new(false));
        let provider = Provider {
            library: f.library.clone(),
            serial: f.serial.clone(),
            trace: trace.clone(),
            permission: permission.clone(),
            fail_vci: fail_vci.clone(),
            fail_read: fail_read.clone(),
        };
        let control = FdbOwnerControl {
            catalog: f.catalog.clone(),
            serial: f.serial.clone(),
            route: f.route,
            storage: Default::default(),
        };
        let (tx, rx) = channel();
        let (events, event_rx) = channel();
        let http_files = files.clone();
        let http_status = status.clone();
        let join = std::thread::spawn(move || {
            let run = if writable {
                tape_rs::daemon::executor::run_fdb_writer
            } else {
                tape_rs::daemon::executor::run_fdb_owner
            };
            run(
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
            events: event_rx,
            permission,
            pools: f.pools,
            tx,
            join: Some(join),
            trace,
            writable,
            files: http_files,
            status: http_status,
            fail_vci,
            fail_read,
        }
    }
    fn read(&self, i: usize) -> std::result::Result<Vec<u8>, ReadError> {
        let (reply, rx) = channel();
        self.tx
            .send(ExecRequest::Read {
                path: format!("/{}/file-{i}", self.pools[usize::from(i == 2)]),
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
                .any(|c| matches!(c[0], 0x04 | 0x19)
                    || (!self.writable && matches!(c[0], 0x0a | 0x10 | 0x8d))),
            "FDB 执行器禁止格式化/擦除，只读模式禁止写介质"
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
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅写入模拟磁带与随机命名空间"]
fn writable_owner_http_commit_retry_overwrite_and_readonly_rejection() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::collections::BTreeMap;
    use tape_rs::daemon::{fdb_http, http::HttpContext, state::Executor};
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let ResolvedPath::File { copy, .. } = rt
        .block_on(
            f.catalog
                .resolve_active_path(&f.serial, format!("/{}/file-0", f.pools[0]).as_bytes()),
        )
        .unwrap()
        .unwrap()
    else {
        panic!("seed");
    };
    let mut intent = ActiveFileIntent {
        barcode_constraint: None,
        namespace: f.route.route.namespace,
        parent: f.pool_roots[0],
        parent_path: b"/".to_vec(),
        name: b"uploaded".to_vec(),
        file_id: Uuid::new_v4(),
        expected_version: None,
        operation_id: Uuid::new_v4(),
        request_epoch: 1,
        library_serial: f.serial.clone(),
        pool_id: Some(f.pools[0]),
        owner_epoch: 12,
        volume_id: copy.volume_id,
        volume_generation: copy.volume_generation + 1,
        volume_scan_id: copy.volume_scan_id,
        expected_size: 3,
        expected_digest: ContentDigest::Sha256(Sha256::digest(b"abc").into()),
    };
    {
        let readonly = Owner::start(&f, 12, json!({}));
        let spool = f.root.join("readonly-upload");
        fs::write(&spool, b"abc").unwrap();
        let (reply, rx) = channel();
        readonly
            .tx
            .send(ExecRequest::FdbWrite {
                intent: Box::new(intent.request()),
                file: fs::File::open(&spool).unwrap(),
                reply,
            })
            .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Err(ReadError::Failed(_))
        ));
    }
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    {
        let mut status = owner.status.lock().unwrap();
        status.id = 71;
        status.executor = Some(Executor {
            node: 71,
            round: 13,
            term: 1,
            fenced: true,
        });
        status.executor_ready = true;
    }
    let (node, _) = channel();
    let context = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: owner.status.clone(),
        exec: Mutex::new(owner.tx.clone()),
        node: Mutex::new(node),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), context.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let mut remote_status = owner.status.lock().unwrap().clone();
    remote_status.id = 72;
    let (unused_exec, _unused_requests) = channel();
    let entry_context = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: Arc::new(Mutex::new(remote_status)),
        exec: Mutex::new(unused_exec),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let entry = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), entry_context)]),
            HashMap::from([(71, server.address.clone())]),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let readonly_server = ReadHttpServer::start(
        fdb_http::owner_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), context)]),
            HashMap::new(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let query = format!(
        "pool={}&path={}",
        f.pools[0],
        URL_SAFE_NO_PAD.encode(b"/uploaded")
    );
    let path = format!("/v1/fdb/files/{}?{query}", f.serial);
    let status_path = format!("/v1/fdb/uploads/{}?{query}", f.serial);
    let header = |intent: &ActiveFileIntent| {
        format!(
            "x-tapers-fdb-write: {}\r\nContent-Length: 3\r\n",
            fdb_http::encode_upload_intent(&intent.request()).unwrap()
        )
    };
    let original = intent.clone();
    let first = header(&intent);
    let status_header = format!(
        "x-tapers-fdb-write: {}\r\n",
        fdb_http::encode_upload_intent(&intent.request()).unwrap()
    );
    assert_eq!(
        entry.request("GET", &status_path, Some(&status_header)).0,
        202
    );
    assert_eq!(
        readonly_server
            .request_body("PUT", &path, Some(&first), b"abc")
            .0,
        405
    );
    let unsupported = format!("{first}If-Unmodified-Since: Wed, 01 Jan 2020 00:00:00 GMT\r\n");
    assert_eq!(
        server
            .request_body("PUT", &path, Some(&unsupported), b"abc")
            .0,
        400
    );
    let result = entry.request_body("PUT", &path, Some(&first), b"abc");
    assert_eq!(result.0, 201, "{}", String::from_utf8_lossy(&result.2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.2).unwrap()["version"],
        1
    );
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| cdb[0] == 0x0a)
            .count()
    };
    let count = writes();
    assert!(count > 0);
    assert_eq!(
        server.request_body("PUT", &path, Some(&first), b"abc").0,
        201
    );
    assert_eq!(writes(), count, "重复上传不能再次写带");
    assert_eq!(
        entry.request("GET", &status_path, Some(&status_header)).0,
        200
    );
    assert_eq!(server.request("GET", &path, None).2, b"abc");
    intent.operation_id = Uuid::new_v4();
    intent.expected_version = Some(1);
    intent.expected_digest = ContentDigest::Sha256(Sha256::digest(b"xyz").into());
    let overwrite = header(&intent);
    let result = server.request_body("PUT", &path, Some(&overwrite), b"xyz");
    assert_eq!(result.0, 201, "{}", String::from_utf8_lossy(&result.2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.2).unwrap()["version"],
        2
    );
    let count = writes();
    assert_eq!(
        server.request_body("PUT", &path, Some(&first), b"abc").0,
        201
    );
    assert_eq!(writes(), count, "覆盖后重试旧请求不能回退版本或重复写带");
    assert_eq!(server.request("GET", &path, None).2, b"xyz");
    intent.operation_id = Uuid::new_v4();
    intent.expected_version = Some(2);
    f.library.set_drive_densities(0, vec![(0x5e, false)]);
    f.library.set_drive_densities(1, vec![(0x5e, false)]);
    assert_eq!(
        server
            .request_body("PUT", &path, Some(&header(&intent)), b"xyz")
            .0,
        503
    );
    assert_eq!(writes(), count, "已装载驱动器不可写时禁止追加");
    f.library.set_drive_densities(0, vec![(0x5e, true)]);
    f.library.set_drive_densities(1, vec![(0x5e, true)]);
    let result = server.request_body("PUT", &path, Some(&header(&intent)), b"bad");
    assert_eq!(result.0, 503);
    assert_eq!(writes(), count, "摘要不符必须在写带之前拒绝");
    assert_eq!(server.request("GET", &path, None).2, b"xyz");
    drop(readonly_server);
    drop(entry);
    drop(server);
    drop(owner);
    let next = Owner::start_mode(&f, 14, json!({}), true);
    let spool = f.root.join("retry-old-owner");
    fs::write(&spool, b"abc").unwrap();
    let (reply, received) = channel();
    next.tx
        .send(ExecRequest::FdbWrite {
            intent: Box::new(original.request()),
            file: fs::File::open(&spool).unwrap(),
            reply,
        })
        .unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap(),
        1
    );
    assert!(!next.trace.lock().unwrap().iter().any(|cdb| cdb[0] == 0x0a));
    drop(next);
    fs::remove_dir_all(&f.root).unwrap();
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
        let o = Owner::start_with_policy(&f, 12, json!({}));
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
        let o = Owner::start_with_policy(
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
        let o = Owner::start_with_policy(
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
        let o = Owner::start_with_policy(&f, 15, json!({"drives":{"SIMDRV0000":{"role":"m"}}}));
        o.expect(0);
        o.expect(2);
        expect_loaded(&f, [None, Some(2)]);
        assert_eq!(o.moves(), 3, "不能使用无读取角色的空驱动器");
    }
    {
        f.library.load_into_drive(BARCODES[0], 0).unwrap();
        let o = Owner::start_with_policy(&f, 16, json!({"tapes":{BARCODES[0]:{"online":false}}}));
        o.expect(2);
        o.expect(1);
        expect_loaded(&f, [Some(0), Some(1)]);
        assert_eq!(o.moves(), 3, "不得腾退离线介质");
    }
    expect_loaded(&f, [Some(0), None]);
    {
        let o = Owner::start_with_policy(
            &f,
            17,
            json!({"pools":{f.pools[0].to_string():{"mountlimit":"1"}},"tapes":{BARCODES[0]:{"online":false}}}),
        );
        assert!(
            matches!(o.read(1), Err(ReadError::Busy(_))),
            "已装载但禁止腾退的同池介质仍应占用配额"
        );
        assert_eq!(o.moves(), 0);
    }
    fixture_return_to_storage(&f.library, 0);
    // Restore online state through the real media-validation task. A fresh
    // owner/configuration must never silently reset persistent tape policy.
    {
        let online_owner = Owner::start_mode(&f, 18, json!({}), true);
        let assignment = rt
            .block_on(f.catalog.tape_assignment(&f.serial, BARCODES[0]))
            .unwrap()
            .unwrap();
        let policy = rt
            .block_on(
                f.catalog
                    .active_resource_policy(&f.serial, f.route, "tape", BARCODES[0]),
            )
            .unwrap();
        let window = rt
            .block_on(f.catalog.request_window(f.route.route.namespace))
            .unwrap();
        let request = tape_rs::managed_catalog::ManagementRequest::parse(json!({
            "version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),
            "epoch":window.accepting,"operation":Uuid::new_v4().to_string(),
            "change":{"kind":"media-online","pool":assignment.pool_id.to_string(),
                "volume":assignment.volume_id.unwrap().to_string(),"barcode":BARCODES[0],
                "revision":policy["revision"]}
        }))
        .unwrap();
        rt.block_on(f.catalog.admit_management(&f.serial, f.route, 18, &request))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let task = rt
                .block_on(f.catalog.management_task(
                    &f.serial,
                    f.route,
                    request.epoch(),
                    request.operation(),
                ))
                .unwrap()
                .unwrap();
            if task["status"] == "succeeded" {
                break;
            }
            assert!(
                task["status"] != "failed" && Instant::now() < deadline,
                "{task}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(online_owner);
    }
    {
        let o = Owner::start_with_policy(
            &f,
            18,
            json!({"drives":{"SIMDRV0000":{"enabled":false},"SIMDRV0001":{"enabled":false}}}),
        );
        assert!(matches!(o.read(0), Err(ReadError::Busy(_))));
        assert_eq!(o.moves(), 0);
    }
    {
        let o = Owner::start_with_policy(&f, 19, json!({}));
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

#[test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；在模拟磁带 S4 后注入错误，不访问真实设备"]
fn writable_owner_recovers_unobserved_s4_without_reappending() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let ResolvedPath::File { copy, .. } = rt
        .block_on(
            f.catalog
                .resolve_active_path(&f.serial, format!("/{}/file-0", f.pools[0]).as_bytes()),
        )
        .unwrap()
        .unwrap()
    else {
        panic!("seed");
    };
    let bytes = b"recovery-payload";
    let spool = f.root.join("unobserved-upload");
    fs::write(&spool, bytes).unwrap();
    let mut intent = ActiveFileIntent {
        barcode_constraint: None,
        namespace: f.route.route.namespace,
        parent: f.pool_roots[0],
        parent_path: b"/".to_vec(),
        name: b"recover-upload".to_vec(),
        file_id: Uuid::new_v4(),
        expected_version: None,
        operation_id: Uuid::new_v4(),
        request_epoch: 1,
        library_serial: f.serial.clone(),
        pool_id: Some(f.pools[0]),
        owner_epoch: 12,
        volume_id: copy.volume_id,
        volume_generation: copy.volume_generation + 1,
        volume_scan_id: copy.volume_scan_id,
        expected_size: bytes.len() as u64,
        expected_digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
    };
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    // sync_batch flushes the DP construct before writing VCI. This failure
    // leaves a complete physical S4 that has not yet been observed in FDB.
    owner.fail_vci.store(true, Ordering::SeqCst);
    let (reply, received) = channel();
    owner
        .tx
        .send(ExecRequest::FdbWrite {
            intent: Box::new(intent.request()),
            file: fs::File::open(&spool).unwrap(),
            reply,
        })
        .unwrap();
    let result = received.recv_timeout(Duration::from_secs(15)).unwrap();
    assert!(matches!(result, Err(ReadError::Lost(_))), "{result:?}");
    drop(owner);
    assert_eq!(
        rt.block_on(
            f.catalog
                .observed_active_volume_index(&f.serial, copy.volume_id)
        )
        .unwrap()
        .generation,
        copy.volume_generation
    );
    f.library.power_cut();
    let next = Owner::start_mode(&f, 13, json!({}), true);
    intent.owner_epoch = 13;
    assert_eq!(
        rt.block_on(f.catalog.active_upload_result(&intent.request(), f.route))
            .unwrap(),
        Some(PublishOutcome::AlreadyApplied(1))
    );
    let (reply, received) = channel();
    next.tx
        .send(ExecRequest::Read {
            path: format!("/{}/recover-upload", f.pools[0]),
            reply,
        })
        .unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap(),
        bytes
    );
    let (reply, received) = channel();
    next.tx
        .send(ExecRequest::FdbWrite {
            intent: Box::new(intent.request()),
            file: fs::File::open(&spool).unwrap(),
            reply,
        })
        .unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap(),
        1
    );
    let mut copies = 0;
    assert!(f.library.with_cartridge_mut(BARCODES[0], |cartridge| {
        copies = cartridge
            .partitions
            .iter()
            .flat_map(|partition| &partition.objects)
            .filter(|object| matches!(object, LogicalObject::Record(record) if record == bytes))
            .count();
    }));
    assert_eq!(copies, 1, "恢复和原请求重试不能重复追加内容");
    drop(next);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 TCP/CLI 客户端加模拟磁带，不访问真实设备"]
fn pool_selected_upload_is_prepared_saved_and_resumed_by_client() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::collections::BTreeMap;
    use tape_rs::client::fdb::{FdbClient, PreparedUpload, UploadStatus};
    use tape_rs::daemon::{fdb_http, http::HttpContext, state::Executor};
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false}}}),
        true,
    );
    {
        let mut status = owner.status.lock().unwrap();
        status.id = 71;
        status.executor = Some(Executor {
            node: 71,
            round: 12,
            term: 1,
            fenced: true,
        });
        status.executor_ready = true;
    }
    let (node, _) = channel();
    let context = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: owner.status.clone(),
        exec: Mutex::new(owner.tx.clone()),
        node: Mutex::new(node),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), context.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let s3_server = ReadHttpServer::start(tape_rs::daemon::s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        false,
    ));
    let mut remote = owner.status.lock().unwrap().clone();
    remote.id = 72;
    let entry_context = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: Arc::new(Mutex::new(remote)),
        exec: Mutex::new(channel().0),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let entry = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), entry_context)]),
            HashMap::from([(71, server.address.clone())]),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([entry.address.clone()])
        .unwrap()
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(15));
    let source = f.root.join("client-source");
    let content = vec![0x5a; 70 * 1024]; // exercise Expect: 100-continue
    fs::write(&source, &content).unwrap();
    let state = f.root.join("client-request.json");
    let request = client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/client-upload",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &state,
        )
        .unwrap();

    let original = fs::read(state.join("request.json")).unwrap();
    let record = upload_prepared(&state);
    let metadata: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(record["metadata"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert!(
        metadata.get("volume").is_none(),
        "prepare must not freeze a physical target"
    );
    assert_eq!(owner.moves(), 0, "prepare 不装卸介质");
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|cdb| matches!(cdb[0], 0x0a | 0x10 | 0x8d))
    );
    assert!(
        rt.block_on(
            f.catalog
                .pending_active_file_reservations(f.route.route.namespace, None, 100)
        )
        .unwrap()
        .items
        .is_empty(),
        "prepare 不能占用路径或准入配额"
    );
    let busy_spec = tape_rs::managed_catalog::ActiveUploadSpec {
        pool_id: f.pools[0],
        path: b"/busy-path".to_vec(),
        operation_id: Uuid::new_v4(),
        expected_version: None,
        size: content.len() as u64,
        digest: ContentDigest::Sha256(Sha256::digest(&content).into()),
    };
    let held = rt
        .block_on(
            f.catalog
                .prepare_active_upload(&f.serial, f.route, 12, BARCODES[1], &busy_spec),
        )
        .unwrap();
    assert_eq!(
        rt.block_on(f.catalog.reserve_active_file(&held)).unwrap(),
        PublishOutcome::Registered
    );
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/busy-path",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_err(),
        "不能换另一卷绕过全局路径占用"
    );
    assert_eq!(
        rt.block_on(f.catalog.cancel_active_file_reservation(&held))
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    assert_eq!(client.status(&request).unwrap(), UploadStatus::Unconfirmed);
    assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
    let actual = client
        .upload_receipt(&PreparedUpload::load(&state).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        actual["barcode"], BARCODES[1],
        "实际写入须跳过离线介质和另一池"
    );

    let stat = client
        .stat(&f.serial, f.pools[0], "/client-upload")
        .unwrap()
        .unwrap();
    assert_eq!(stat["namespace"], f.route.route.namespace.to_string());
    assert_eq!(stat["pool_id"], f.pools[0].to_string());
    assert_eq!(stat["size"], content.len());
    assert_eq!(stat["version"], 1);
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/missing")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap()["kind"],
        "directory"
    );
    let mut cursor: Option<String> = None;
    let mut names = Vec::new();
    for _ in 0..10 {
        let page = client
            .list_children(&f.serial, f.pools[0], "/", 1, cursor.as_deref())
            .unwrap();
        assert_eq!(page["namespace"], f.route.route.namespace.to_string());
        names.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned()),
        );
        cursor = page["next"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none(), "分页必须终止");
    assert_eq!(names, ["client-upload", "file-0", "file-1", "file-3"]);
    let download = f.root.join("download");
    assert_eq!(
        client
            .download_new(&f.serial, f.pools[0], "/client-upload", &download)
            .unwrap(),
        content.len() as u64
    );
    assert_eq!(fs::read(&download).unwrap(), content);
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| cdb[0] == 0x0a)
            .count()
    };
    let count = writes();
    assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
    assert_eq!(writes(), count);
    assert_eq!(
        fs::read(state.join("request.json")).unwrap(),
        original,
        "重试不能换请求或目标卷"
    );
    let (reply, received) = channel();
    owner
        .tx
        .send(ExecRequest::Read {
            path: format!("/{}/client-upload", f.pools[0]),
            reply,
        })
        .unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap(),
        content
    );
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/client-upload",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_err(),
        "默认不覆盖已有文件"
    );
    let second_state = f.root.join("second-request.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/client-upload",
            &source,
            upload_replacement(
                &client
                    .stat(&f.serial, f.pools[0], "/client-upload")
                    .unwrap()
                    .unwrap(),
            ),
            None,
            &second_state,
        )
        .unwrap();

    assert_upload_committed(
        client.upload_saved(&second_state).unwrap(),
        &second_state,
        2,
    );
    assert_eq!(
        client
            .status(&PreparedUpload::load(&state).unwrap())
            .unwrap(),
        UploadStatus::Committed(1)
    );
    let mut changed = record;
    changed["namespace"] = json!(Uuid::new_v4().to_string());
    fs::write(state.join("request.json"), upload_query_fixture(&changed)).unwrap();
    assert!(
        PreparedUpload::load(&state).is_err(),
        "原请求不能跨 namespace 重放"
    );
    fs::write(state.join("request.json"), &original).unwrap();
    assert_eq!(
        client
            .status(&PreparedUpload::load(&state).unwrap())
            .unwrap(),
        UploadStatus::Committed(1)
    );
    let s3_key = format!(
        "/{}/client-upload",
        tape_rs::daemon::s3::bucket_name(&f.serial, &f.pools[0].to_string())
    );
    let read = s3_server.request("GET", &s3_key, None);
    assert_eq!(read.0, 200);
    assert_eq!(read.2, content, "新版上传须直接通过池内S3键读回");
    let other_source = f.root.join("other-pool-source");
    fs::write(&other_source, b"other-pool").unwrap();
    let other_state = f.root.join("other-pool-request.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[1],
            "/client-upload",
            &other_source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[1],
            },
            None,
            &other_state,
        )
        .unwrap();

    assert_upload_committed(client.upload_saved(&other_state).unwrap(), &other_state, 1);
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/client-upload")
            .unwrap()
            .unwrap()["version"],
        2
    );
    assert_eq!(
        client
            .stat(&f.serial, f.pools[1], "/client-upload")
            .unwrap()
            .unwrap()["version"],
        1
    );
    let read = s3_server.request(
        "GET",
        &format!(
            "/{}/client-upload",
            tape_rs::daemon::s3::bucket_name(&f.serial, &f.pools[1].to_string())
        ),
        None,
    );
    assert_eq!(read.0, 200);
    assert_eq!(read.2, b"other-pool");
    let cli_request = f.root.join("cli-request.json");
    let prepare = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &entry.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "fdb",
            "prepare",
        ])
        .arg(&source)
        .arg("/cli-upload")
        .arg("--request")
        .arg(&cli_request)
        .output()
        .unwrap();
    assert!(
        prepare.status.success(),
        "{}",
        String::from_utf8_lossy(&prepare.stderr)
    );
    let run_cli = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args(["-e", &entry.address, "fdb", "upload"])
            .arg(&cli_request)
            .output()
            .unwrap()
    };
    let upload = run_cli();
    assert!(
        upload.status.success(),
        "{}",
        String::from_utf8_lossy(&upload.stderr)
    );
    let count = writes();
    let retry = run_cli();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(writes(), count, "CLI 重启后必须复用保存的原请求");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &entry.address, "fdb", "status"])
        .arg(&cli_request)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    for (command, path) in [("stat", "/cli-upload"), ("children", "/")] {
        let query = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args([
                "-e",
                &entry.address,
                "--library",
                &f.serial,
                "--pool",
                &f.pools[0].to_string(),
                "fdb",
                command,
                path,
            ])
            .output()
            .unwrap();
        assert!(
            query.status.success(),
            "{}",
            String::from_utf8_lossy(&query.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
        assert_eq!(value["namespace"], f.route.route.namespace.to_string());
    }
    let cli_download = f.root.join("cli-download");
    let get = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args([
                "-e",
                &entry.address,
                "--library",
                &f.serial,
                "--pool",
                &f.pools[0].to_string(),
                "fdb",
                "get",
                "/cli-upload",
            ])
            .arg(&cli_download)
            .output()
            .unwrap()
    };
    let result = get();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&cli_download).unwrap(), content);
    assert!(!get().status.success(), "CLI 下载不能覆盖已有输出");
    assert_eq!(fs::read(&cli_download).unwrap(), content);
    assert_eq!(writes(), count, "查询与下载不能追加磁带");
    drop(s3_server);
    drop(entry);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；1000+ 对象排序分页，模拟磁带，不访问真实设备"]
fn fdb_objects_follow_listobjects_v2_order_and_pagination() {
    use tape_rs::client::fdb::{FdbClient, ListObjectsOptions};
    use tape_rs::daemon::{fdb_http, http::HttpContext};
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut objects = (0..1005).map(|i| format!("/x{i:04}")).collect::<Vec<_>>();
    objects.extend(["/a/x", "/a-/x", "/a.txt", "/a0", "/é/a", "/中文/a"].map(str::to_owned));
    let f = rt.block_on(fixture_with_objects(&cluster, &objects));
    let (tx, _rx) = channel();
    let ctx = Arc::new(HttpContext {
        files: FileService::new(f.root.join("objects-readonly")).unwrap(),
        status: Arc::new(Mutex::new(NodeStatus::default())),
        exec: Mutex::new(tx),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let server = ReadHttpServer::start(
        fdb_http::owner_router(
            f.catalog.clone(),
            std::collections::BTreeMap::from([(f.serial.clone(), f.route)]),
            std::collections::BTreeMap::from([(f.serial.clone(), ctx)]),
            HashMap::new(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([server.address.clone()])
        .unwrap()
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(15));
    let mut options = ListObjectsOptions::default();
    let first = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(first["key_count"], 1000);
    assert_eq!(first["max_keys"], 1000);
    assert_eq!(first["is_truncated"], true);
    let token = first["next_continuation_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut keys = first["contents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    options.continuation_token = Some(token.clone());
    let second = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(second["is_truncated"], false);
    keys.extend(
        second["contents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap().to_owned()),
    );
    let mut expected = objects
        .iter()
        .map(|p| p.trim_start_matches('/').to_owned())
        .collect::<Vec<_>>();
    expected.extend(["file-0", "file-1", "file-3"].map(str::to_owned));
    expected.sort();
    assert_eq!(
        keys, expected,
        "全池完整对象键排序，包含跨目录与 Unicode，排除其他池"
    );
    assert_eq!(
        keys.iter().take(4).map(String::as_str).collect::<Vec<_>>(),
        ["a-/x", "a.txt", "a/x", "a0"]
    );
    options.continuation_token = None;
    options.prefix.clear();
    options.delimiter = "/".into();
    options.max_keys = 2;
    let page = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(page["common_prefixes"], json!(["a-/"]));
    assert_eq!(page["contents"][0]["key"], "a.txt");
    assert_eq!(page["key_count"], 2);
    options.continuation_token = page["next_continuation_token"].as_str().map(str::to_owned);
    let page = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(page["common_prefixes"], json!(["a/"]));
    assert_eq!(page["contents"][0]["key"], "a0");
    options.continuation_token = None;
    options.prefix = "a".into();
    options.start_after = Some("a/x".into());
    let page = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(page["contents"][0]["key"], "a0");
    assert_eq!(page["common_prefixes"], json!([]));
    options.prefix = "中文/".into();
    options.start_after = None;
    let page = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(page["contents"][0]["key"], "中文/a");
    options.prefix = "missing/".into();
    assert_eq!(
        client
            .list_objects_v2(&f.serial, f.pools[0], &options)
            .unwrap()["key_count"],
        0
    );
    options = ListObjectsOptions {
        max_keys: 0,
        ..Default::default()
    };
    let page = client
        .list_objects_v2(&f.serial, f.pools[0], &options)
        .unwrap();
    assert_eq!(page["key_count"], 0);
    assert_eq!(page["is_truncated"], false);
    for (prefix, pool, delimiter) in [
        ("x", f.pools[0], ""),
        ("", f.pools[1], ""),
        ("", f.pools[0], "/"),
    ] {
        options = ListObjectsOptions {
            prefix: prefix.into(),
            delimiter: delimiter.into(),
            continuation_token: Some(token.clone()),
            ..Default::default()
        };
        assert!(
            client.list_objects_v2(&f.serial, pool, &options).is_err(),
            "token 不能跨池或查询条件重用"
        );
    }
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "fdb",
            "list",
            "--prefix",
            "a",
            "--max-keys",
            "1000",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["key_count"], 4);
    drop(server);
    fs::remove_dir_all(f.root).unwrap();
}

#[path = "fdb_s3.rs"]
mod s3_readonly;

#[path = "fdb_s3_writing.rs"]
mod s3_writing;

#[path = "fdb_native_init.rs"]
mod native_init;

#[path = "fdb_active_pools.rs"]
mod active_pools;

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；空池虚拟根，无真实设备"]
fn empty_pool_has_a_root_without_any_assigned_tapes() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = Uuid::new_v4();
    let f = rt.block_on(fixture_with_extra_pool(&cluster, &[], Some(pool)));
    let path = tape_rs::managed_catalog::pool_catalog_path(pool, b"/").unwrap();
    let root = rt
        .block_on(
            f.catalog
                .resolve_active_metadata_for_route(&f.serial, &path, f.route),
        )
        .unwrap();
    assert!(matches!(root,Some(ResolvedPath::Directory{id,..}) if !id.is_nil()));
    assert!(
        rt.block_on(
            f.catalog
                .resolve_active_metadata_for_route(&f.serial, b"/", f.route)
        )
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .list_active_children_for_route(&f.serial, b"/", None, 100, f.route)
        )
        .is_err()
    );
    let page = rt
        .block_on(f.catalog.list_active_objects_for_route(
            &f.serial,
            f.route,
            tape_rs::managed_catalog::ObjectListRequest {
                pool,
                prefix: "",
                delimiter: "",
                max_keys: 1000,
                start_after: "",
            },
        ))
        .unwrap();
    assert!(page.items.is_empty() && page.next.is_none());
    fs::remove_dir_all(f.root).unwrap();
}

#[path = "fdb_policy.rs"]
mod policy;

#[path = "fdb_node_http.rs"]
mod node_http;
#[path = "fdb_node_owner.rs"]
mod nodes;
#[path = "fdb_tape_policy.rs"]
mod tape_policy;

#[path = "fdb_tape_monitor.rs"]
mod tape_monitor;

#[path = "fdb_notify.rs"]
mod notify;
