//! Small producer/read fixture using the actual Sim transport and an isolated FDB.
use super::*;
use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::daemon::fdb_media::{
    ActiveMediaUpload, commit_active_media_batch, publish_active_media_batch,
    recover_sealed_active_media_batch, resume_observed_active_media_batch,
};
use crate::daemon::fdb_read::ActiveMediaReader;
use crate::daemon::permission::{GuardedTransport, Permission, Phase};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    catalog: ManagedCatalog,
    library: SimLibrary,
    root: PathBuf,
    namespace: Uuid,
    pool: Uuid,
    serial: String,
    baseline: ActiveIndexIdentity,
    scan: VolumeScan,
    route: ActiveLibraryRoute,
}

async fn fixture(cluster: &str) -> Fixture {
    let catalog = ManagedCatalog::open(Some(cluster)).unwrap();
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
                    created_at_millis: 1_793_405_000_123,
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

    Fixture {
        catalog,
        library,
        root,
        namespace,
        pool,
        serial,
        baseline,
        scan,
        route: active,
    }
}

fn write_count(library: &SimLibrary) -> usize {
    library
        .drive(0)
        .command_log()
        .iter()
        .filter(|&&op| {
            matches!(
                op,
                crate::scsi::cdb::opcode::WRITE_6 | crate::scsi::cdb::opcode::WRITE_FILEMARKS_6
            )
        })
        .count()
}

async fn intent(f: &Fixture, name: &str, generation: u64, bytes: &[u8]) -> ActiveFileIntent {
    let parent = match f
        .catalog
        .resolve_active_path(&f.serial, format!("/{}", f.pool).as_bytes())
        .await
        .unwrap()
        .unwrap()
    {
        ResolvedPath::Directory { id, .. } => id,
        _ => panic!("pool root must be a directory"),
    };
    ActiveFileIntent {
        namespace: f.namespace,
        parent,
        parent_path: b"/".to_vec(),
        name: name.as_bytes().to_vec(),
        file_id: Uuid::new_v4(),
        expected_version: None,
        operation_id: Uuid::new_v4(),
        request_epoch: 1,
        library_serial: f.serial.clone(),
        pool_id: Some(f.pool),
        barcode_constraint: None,
        owner_epoch: 11,
        volume_id: f.baseline.volume_uuid,
        volume_generation: generation,
        volume_scan_id: f.scan.id,
        expected_size: bytes.len() as u64,
        expected_digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
    }
}

async fn overlay_read(f: &Fixture) {
    let drive = f.library.drive(0);
    let key = ReservationKey::new(71, 11);
    fence(&drive, key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let bytes = b"B1 committed producer and unmaterialized overlay";
    let request = intent(f, "layout", f.baseline.generation + 1, bytes).await;
    let spool = f.root.join("body.bin");
    fs::write(&spool, bytes).unwrap();
    let second_bytes = b"second real file in the same S4 batch";
    let second_request = intent(f, "batch-second", f.baseline.generation + 1, second_bytes).await;
    let second_spool = f.root.join("batch-second.bin");
    fs::write(&second_spool, second_bytes).unwrap();
    let batch = Uuid::new_v4();
    let committed = Box::pin(commit_active_media_batch(
        &f.catalog,
        volume,
        batch,
        &[
            ActiveMediaUpload {
                intent: request.clone(),
                spool,
            },
            ActiveMediaUpload {
                intent: second_request,
                spool: second_spool,
            },
        ],
        64 << 20,
    ))
    .await
    .unwrap();
    let current = committed.current_index();
    let prepared = committed.files()[0].clone();
    let second_prepared = committed.files()[1].clone();
    let receipt = &prepared.receipt;
    let mut volume = committed.into_volume();

    // Existing SDK attribute preparation cannot satisfy a newly declared
    // physical-layout requirement, even when its aggregate marker exists.
    f.catalog
        .prepare_observed_active_file(&f.serial, 11, batch, receipt, &prepared.attributes)
        .await
        .unwrap();
    f.catalog
        .prepare_observed_active_file(
            &f.serial,
            11,
            batch,
            &second_prepared.receipt,
            &second_prepared.attributes,
        )
        .await
        .unwrap();
    f.catalog
        .mark_observed_active_batch_page_prepared(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
            0,
        )
        .await
        .unwrap();
    f.catalog
        .seal_observed_active_preparation(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    f.catalog
        .require_observed_active_body_layouts(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    assert_eq!(
        f.catalog
            .publish_observed_active_batch(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::PageIncomplete
    );
    assert!(
        f.catalog
            .mark_observed_active_batch_page_prepared(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch,
                0
            )
            .await
            .is_err()
    );
    let source = CommittedLayoutSource::new(&mut volume).unwrap();
    let proof = source.prepared("/layout", &prepared).unwrap().unwrap();
    let binding = f
        .catalog
        .stage_observed_body_layout(&f.serial, 11, batch, &proof)
        .await
        .unwrap();
    assert_eq!(
        f.catalog
            .publish_observed_active_batch(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::PageIncomplete
    );
    assert!(
        f.catalog
            .mark_observed_active_batch_page_prepared(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch,
                0
            )
            .await
            .is_err(),
        "a partial batch projection cannot cover its unstaged second file"
    );
    let second_proof = source
        .prepared("/batch-second", &second_prepared)
        .unwrap()
        .unwrap();
    let second_binding = f
        .catalog
        .stage_observed_body_layout(&f.serial, 11, batch, &second_proof)
        .await
        .unwrap();
    assert_eq!(
        f.catalog
            .stage_observed_body_layout(&f.serial, 11, batch, &proof)
            .await
            .unwrap(),
        binding
    );
    f.catalog
        .mark_observed_active_batch_page_prepared(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
            0,
        )
        .await
        .unwrap();
    f.catalog
        .seal_observed_active_preparation(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    let tr = f.catalog.db.create_trx().unwrap();
    let requirement = active_batch::test_layout_requirement_at(
        &tr,
        &f.serial,
        current.volume_uuid,
        current.generation,
    )
    .await
    .unwrap();
    let old_page = requirement.page_key(0);
    let marker_page = requirement.marker_page_key(0);
    let old_page_raw = tr.get(&old_page, false).await.unwrap().unwrap().to_vec();
    assert!(tr.get(&marker_page, false).await.unwrap().is_some());
    let required_key = required_copy_key(f.namespace, receipt.file_id, receipt.copy_id);
    tr.clear(&marker_page);
    tr.clear(&required_key);
    tr.commit().await.unwrap();
    assert_eq!(
        f.catalog
            .publish_observed_active_batch(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::PageIncomplete
    );
    assert!(
        f.catalog
            .mark_observed_active_batch_page_prepared(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch,
                0
            )
            .await
            .is_err()
    );
    let tr = f.catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&old_page, false).await.unwrap().as_deref(),
        Some(old_page_raw.as_slice())
    );
    assert!(tr.get(&marker_page, false).await.unwrap().is_none());
    assert!(tr.get(&required_key, false).await.unwrap().is_none());
    drop(tr);
    let before_restore_writes = write_count(&f.library);
    assert_eq!(
        f.catalog
            .stage_observed_body_layout(&f.serial, 11, batch, &proof)
            .await
            .unwrap(),
        binding
    );
    assert_eq!(
        f.catalog
            .stage_observed_body_layout(&f.serial, 11, batch, &second_proof)
            .await
            .unwrap(),
        second_binding
    );
    f.catalog
        .mark_observed_active_batch_page_prepared(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
            0,
        )
        .await
        .unwrap();
    f.catalog
        .seal_observed_active_preparation(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    assert_eq!(write_count(&f.library), before_restore_writes);
    eprintln!(
        "PARTIAL_BATCH_REAL_S4_FILES=2 OLD_PAGE_WITHOUT_NEW_MARKER_PAGE_REFUSED=true SAME_BATCH={} NO_REWRITE_ON_RESTORE=true",
        batch
    );
    assert_eq!(
        f.catalog
            .publish_observed_active_batch(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::Published
    );
    let tr = f.catalog.db.create_trx().unwrap();
    assert!(
        tr.get(
            &copy_key(f.namespace, receipt.file_id, receipt.copy_id),
            false
        )
        .await
        .unwrap()
        .is_none(),
        "R has not materialized yet"
    );
    drop(tr);
    let status = f
        .catalog
        .active_batch_status(&f.serial, current.volume_uuid, current.generation)
        .await
        .unwrap()
        .unwrap();
    assert!(status.published && !status.completed);

    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let guarded = GuardedTransport::new(Box::new(f.library.drive(0)), permission.clone(), 11);
    let reader = ActiveMediaReader {
        catalog: &f.catalog,
        device: &guarded,
        serial: &f.serial,
        route: f.route,
        key,
        permission: &permission,
        storage: Default::default(),
    };
    let cache = f.root.join("layout-cache");
    let read_spool = f.root.join("read-spool");
    fs::create_dir(&read_spool).unwrap();
    let path = format!("/{}/layout", f.pool);
    let mut read = reader
        .read_with_cache(path.as_bytes(), &cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut actual = Vec::new();
    read.file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(read.verified_head, current);
    assert_eq!(
        binding.path(),
        "/layout",
        "stored binding remains canonical"
    );
    assert_eq!(
        read.physical.path(),
        "layout",
        "read descriptors preserve the streamed relative-path contract"
    );
    // This is a test oracle, separate from the service's absent cache directory.
    // Every original plan field must match; no metadata proof is weakened.
    assert!(!current.incremental);
    let oracle = f.root.join("path-oracle.db");
    VolumeExtentCache::build(
        &source.volume.index().to_xml().unwrap()[..],
        &oracle,
        ExpectedIndex {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
        },
    )
    .unwrap();
    let lookup = f
        .catalog
        .resolve_active_read_source_for_owner(&f.serial, path.as_bytes(), f.route, 11)
        .await
        .unwrap()
        .unwrap();
    let original = lookup.clone().plan_read(&oracle).unwrap();
    assert_eq!(original.source, current);
    assert_eq!(ReadMetadata::from(&original.read), read.metadata);
    assert!(
        read.physical
            .matches_extents(&original.read.physical.extents)
            .unwrap()
    );
    assert_eq!(read.physical.path(), original.read.physical.path);
    let mut wrong_extent = proof.layout.clone();
    wrong_extent.extents.to_mut()[0].start_block += 1;
    assert!(lookup.plan_from_layout(&wrong_extent).is_err());
    assert!(
        !cache.exists(),
        "FDB layout must avoid creating extent SQLite"
    );

    let key = binding_key(f.namespace, receipt.file_id, receipt.copy_id);
    let tr = f.catalog.db.create_trx().unwrap();
    tr.clear(&key);
    tr.commit().await.unwrap();
    assert!(
        reader
            .read_with_cache(path.as_bytes(), &cache, &read_spool)
            .await
            .is_err(),
        "a required published overlay must not silently become a fresh-source read"
    );
    assert!(!cache.exists());
    let tr = f.catalog.db.create_trx().unwrap();
    let mut wrong = binding.clone();
    wrong.copy = Uuid::new_v4();
    tr.set(&key, &wrong.encode().unwrap());
    tr.commit().await.unwrap();
    assert!(
        reader
            .read_with_cache(path.as_bytes(), &cache, &read_spool)
            .await
            .is_err()
    );
    assert!(!cache.exists());
    let tr = f.catalog.db.create_trx().unwrap();
    tr.set(&key, &binding.encode().unwrap());
    let mut unrooted = binding.encode().unwrap();
    let at = unrooted
        .windows(binding.path().len())
        .position(|value| value == binding.path().as_bytes())
        .unwrap();
    unrooted[at] = b'x';
    tr.set(&key, &unrooted);
    tr.commit().await.unwrap();
    assert!(
        reader
            .read_with_cache(path.as_bytes(), &cache, &read_spool)
            .await
            .is_err()
    );
    assert!(!cache.exists());
    let tr = f.catalog.db.create_trx().unwrap();
    tr.set(&key, &binding.encode().unwrap());
    let mut corrupt = proof.layout.chunk(binding.manifest, 0).unwrap();
    corrupt[0] ^= 1;
    tr.set(&chunk_key(f.namespace, binding.manifest, 0), &corrupt);
    tr.commit().await.unwrap();
    assert!(
        reader
            .read_with_cache(path.as_bytes(), &cache, &read_spool)
            .await
            .is_err()
    );
    assert!(!cache.exists());
    let tr = f.catalog.db.create_trx().unwrap();
    tr.set(
        &chunk_key(f.namespace, binding.manifest, 0),
        &proof.layout.chunk(binding.manifest, 0).unwrap(),
    );
    tr.commit().await.unwrap();
    drop(volume);
    let before = write_count(&f.library);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(71, 11)));
    Box::pin(resume_observed_active_media_batch(
        &f.catalog, volume, &f.serial, 11,
    ))
    .await
    .unwrap();
    assert_eq!(
        write_count(&f.library),
        before,
        "published receipt recovery cannot rewrite body/index"
    );
    assert!(
        f.catalog
            .active_batch_status(&f.serial, current.volume_uuid, current.generation)
            .await
            .unwrap()
            .unwrap()
            .completed
    );
}

async fn unknown_s4(f: &Fixture) {
    let drive = f.library.drive(0);
    let key = ReservationKey::new(71, 11);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let current = ActiveIndexIdentity::from(volume.committed_index_construct());
    let bytes = b"original sealed unknown S4";
    let request = intent(f, "unknown", current.generation + 1, bytes).await;
    assert_eq!(
        f.catalog.reserve_active_file(&request).await.unwrap(),
        PublishOutcome::Registered
    );
    volume
        .append_file("/unknown", &mut Cursor::new(bytes))
        .unwrap();
    let prepared = prepare_active_file(&volume, "/unknown", &request).unwrap();
    f.catalog
        .complete_active_file(&prepared.receipt)
        .await
        .unwrap();
    let batch = Uuid::new_v4();
    f.catalog
        .stage_active_batch(batch, &[prepared.receipt.clone()])
        .await
        .unwrap();
    volume.sync_batch().unwrap();
    let committed = ActiveIndexIdentity::from(volume.committed_index_construct());
    drop(volume);
    f.library.power_cut();
    let before = write_count(&f.library);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let recovered = Box::pin(recover_sealed_active_media_batch(
        &f.catalog, volume, &f.serial, 11,
    ))
    .await
    .unwrap();
    assert_eq!(recovered.current_index(), committed);
    assert_eq!(recovered.files()[0].receipt, prepared.receipt);
    let recovered = Box::pin(publish_active_media_batch(&f.catalog, recovered))
        .await
        .unwrap();
    assert_eq!(recovered.files()[0].receipt, prepared.receipt);
    assert_eq!(
        write_count(&f.library),
        before,
        "original S4 must recover without another WRITE"
    );
    let source = f
        .catalog
        .resolve_active_read_source_for_owner(
            &f.serial,
            format!("/{}/unknown", f.pool).as_bytes(),
            f.route,
            11,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        source
            .body_layout(&f.catalog, 8 << 20)
            .await
            .unwrap()
            .is_some()
    );
}

async fn legacy_read(f: &Fixture) {
    let drive = f.library.drive(0);
    let key = ReservationKey::new(71, 11);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let current = ActiveIndexIdentity::from(volume.committed_index_construct());
    let bytes = b"legacy SDK copy without extent projection";
    let request = intent(f, "legacy", current.generation + 1, bytes).await;
    let spool = f.root.join("legacy.bin");
    fs::write(&spool, bytes).unwrap();
    let batch = Uuid::new_v4();
    let committed = Box::pin(commit_active_media_batch(
        &f.catalog,
        volume,
        batch,
        &[ActiveMediaUpload {
            intent: request.clone(),
            spool,
        }],
        64 << 20,
    ))
    .await
    .unwrap();
    let current = committed.current_index();
    let prepared = committed.files()[0].clone();
    drop(committed);
    // An old SDK publisher has no new-domain requirement or layout producer.
    f.catalog
        .prepare_observed_active_file(
            &f.serial,
            11,
            batch,
            &prepared.receipt,
            &prepared.attributes,
        )
        .await
        .unwrap();
    f.catalog
        .mark_observed_active_batch_page_prepared(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
            0,
        )
        .await
        .unwrap();
    f.catalog
        .seal_observed_active_preparation(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    assert_eq!(
        f.catalog
            .publish_observed_active_batch(
                &f.serial,
                11,
                current.volume_uuid,
                current.generation,
                batch
            )
            .await
            .unwrap(),
        ActivePublicationOutcome::Published
    );
    let path = format!("/{}/legacy", f.pool);
    let source = f
        .catalog
        .resolve_active_read_source_for_owner(&f.serial, path.as_bytes(), f.route, 11)
        .await
        .unwrap()
        .unwrap();
    assert!(
        source
            .body_layout(&f.catalog, 8 << 20)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !source.has_existing_snapshot(),
        "published overlay is not an actual Present R"
    );
    let label = crate::ltfs::volume::read_label_only(&drive).unwrap();
    let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
    budget.index_scratch.directory = f.root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let actual = crate::ltfs::recovery::capture_recorded_construct_source(
        &crate::tape::commands::TapeDrive::new(&drive),
        &label,
        crate::ltfs::volume::CommittedIndexConstruct {
            volume_uuid: current.volume_uuid,
            generation: current.generation,
            location: current.location,
            sha256: current.sha256,
            incremental: current.incremental,
        },
        &budget,
    )
    .unwrap();
    assert!(
        VerifiedExistingCopyLayout::from_captured(&source, &actual, &label).is_err(),
        "the materialized factory must not fabricate R bytes for an overlay"
    );
    drop(actual);
    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        11,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let guarded = GuardedTransport::new(Box::new(f.library.drive(0)), permission.clone(), 11);
    let reader = ActiveMediaReader {
        catalog: &f.catalog,
        device: &guarded,
        serial: &f.serial,
        route: f.route,
        key,
        permission: &permission,
        storage: Default::default(),
    };
    let cache = f.root.join("legacy-cache");
    let read_spool = f.root.join("read-spool");
    let mut read = reader
        .read_with_cache(path.as_bytes(), &cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut actual = Vec::new();
    read.file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);
    assert!(
        !cache.exists(),
        "fresh recorded-source reads create no persistent extent mirror"
    );
    assert!(
        read.physical.provenance() == PhysicalPlanProvenance::FdbLayout,
        "legacy source must attach LB2 then re-resolve its FDB plan"
    );
    let attached = f
        .catalog
        .resolve_active_read_source_for_owner(&f.serial, path.as_bytes(), f.route, 11)
        .await
        .unwrap()
        .unwrap();
    let binding = attached.layout_binding().unwrap();
    assert!(matches!(
        binding.provenance,
        LayoutProvenance::VerifiedCopySource { .. }
    ));
    assert_eq!(binding.batch(), None);
    assert_eq!(binding.content_digest, source.copy().content_digest);
    let identity = attached
        .baseline
        .as_ref()
        .expect("LB2 must restore full source identity");
    assert_eq!(ActiveIndexIdentity::from(identity.head()), current);
    assert_eq!(identity.predecessors().is_empty(), !current.incremental);
    // An older LB2 value without LC1 is upgraded from this real recorded
    // source. Its exact value and original copy are not rewritten.
    let metadata = attached.metadata();
    let (global, marker) = test_copy_layout_keys(f.namespace, metadata.id, metadata.head.copy_id);
    let tr = f.catalog.db.create_trx().unwrap();
    let original_binding = tr.get(&global, false).await.unwrap().unwrap().to_vec();
    let original_r_key = copy_key(f.namespace, metadata.id, metadata.head.copy_id);
    let original_r = tr
        .get(&original_r_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    tr.clear(&marker);
    tr.commit().await.unwrap();
    let legacy = f
        .catalog
        .resolve_active_read_source_for_owner(&f.serial, path.as_bytes(), f.route, 11)
        .await
        .unwrap()
        .unwrap();
    assert!(legacy.needs_marker_upgrade());
    let mut upgraded = reader
        .read_with_cache(path.as_bytes(), &cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut upgraded_bytes = Vec::new();
    upgraded.file.read_to_end(&mut upgraded_bytes).unwrap();
    assert_eq!(upgraded_bytes, bytes);
    assert_eq!(
        upgraded.physical.provenance(),
        PhysicalPlanProvenance::FdbLayout
    );
    let tr = f.catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&global, false).await.unwrap().as_deref(),
        Some(original_binding.as_slice())
    );
    assert_eq!(
        tr.get(&marker, false).await.unwrap().as_deref(),
        Some(required_copy_value(&original_binding).as_slice())
    );
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(original_r.as_slice())
    );
    drop(tr);
    // Model the older LB1 format using the actual original receipt and a
    // newly checked committed volume. This SDK batch never declared LR1.
    let legacy_lb1_raw = {
        let mut volume = LtfsVolume::mount(&drive).unwrap();
        volume.set_reservation_guard(Some(key));
        let committed_source = CommittedLayoutSource::new(&mut volume).unwrap();
        let committed_proof = committed_source
            .file(&request, &prepared.receipt)
            .unwrap()
            .unwrap();
        let legacy_lb1 = CopyLayoutBinding::from_verified(batch, &committed_proof).unwrap();
        assert_eq!(legacy_lb1.manifest, binding.manifest);
        legacy_lb1.encode().unwrap()
    };
    assert_eq!(&legacy_lb1_raw[..3], b"LB1");
    let tr = f.catalog.db.create_trx().unwrap();
    tr.set(&global, &legacy_lb1_raw);
    tr.clear(&marker);
    tr.commit().await.unwrap();
    let before = write_count(&f.library);
    let mut upgraded = reader
        .read_with_cache(path.as_bytes(), &cache, &read_spool)
        .await
        .unwrap()
        .unwrap();
    let mut checked_bytes = Vec::new();
    upgraded.file.read_to_end(&mut checked_bytes).unwrap();
    assert_eq!(checked_bytes, bytes);
    assert_eq!(
        upgraded.physical.provenance(),
        PhysicalPlanProvenance::FdbLayout
    );
    let tr = f.catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&global, false).await.unwrap().as_deref(),
        Some(legacy_lb1_raw.as_slice())
    );
    assert_eq!(
        tr.get(&marker, false).await.unwrap().as_deref(),
        Some(required_copy_value(&legacy_lb1_raw).as_slice())
    );
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(original_r.as_slice())
    );
    assert_eq!(
        write_count(&f.library),
        before,
        "marker upgrade never rewrites the original body/index"
    );
    drop(tr);
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    Box::pin(resume_observed_active_media_batch(
        &f.catalog, volume, &f.serial, 11,
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际S4布局prepare后失权零新增/新owner原协议恢复，默认栈"]
async fn layout_stage_requires_current_owner_and_recovers_original_preparation() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let f = Box::pin(fixture(&cluster)).await;
    let drive = f.library.drive(0);
    let key = ReservationKey::new(71, 11);
    fence(&drive, key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let bytes = b"owner-fenced original S4 layout";
    let request = intent(&f, "fenced", f.baseline.generation + 1, bytes).await;
    let spool = f.root.join("fenced.bin");
    fs::write(&spool, bytes).unwrap();
    let batch = Uuid::new_v4();
    let committed = Box::pin(commit_active_media_batch(
        &f.catalog,
        volume,
        batch,
        &[ActiveMediaUpload {
            intent: request.clone(),
            spool,
        }],
        64 << 20,
    ))
    .await
    .unwrap();
    let current = committed.current_index();
    let prepared = committed.files()[0].clone();
    let fingerprint = receipt_fingerprint(&prepared.receipt);
    f.catalog
        .require_observed_active_body_layouts(
            &f.serial,
            11,
            current.volume_uuid,
            current.generation,
            batch,
        )
        .await
        .unwrap();
    let mut volume = committed.into_volume();
    let source = CommittedLayoutSource::new(&mut volume).unwrap();
    let proof = source.prepared("/fenced", &prepared).unwrap().unwrap();
    assert!(matches!(&proof.layout.extents, Cow::Borrowed(_)));
    let binding = CopyLayoutBinding::from_verified(batch, &proof).unwrap();
    // A real bounded chunk transaction committed; process loss happened before
    // its separate binding transaction. The original requirement stays intact.
    let tr = f.catalog.db.create_trx().unwrap();
    active_batch::require_layout_stage_at(
        &tr,
        &f.serial,
        11,
        batch,
        &prepared.receipt,
        proof.source,
    )
    .await
    .unwrap();
    assert_eq!(binding.manifest.chunks, 1);
    tr.set(
        &chunk_key(f.namespace, binding.manifest, 0),
        &proof.layout.chunk(binding.manifest, 0).unwrap(),
    );
    tr.set(
        &progress_key(f.namespace, binding.manifest),
        &progress(binding.manifest, 1),
    );
    tr.commit().await.unwrap();
    let tr = f.catalog.db.create_trx().unwrap();
    let before = layout_chunks_at(&tr, &namespace_prefix(f.namespace), None, 100)
        .await
        .unwrap();
    assert!(
        binding_at(&tr, f.namespace, request.file_id, prepared.receipt.copy_id)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    f.catalog
        .claim_active_owner_epoch(&f.serial, 12)
        .await
        .unwrap();
    assert!(matches!(
        f.catalog
            .stage_observed_body_layout(&f.serial, 11, batch, &proof)
            .await,
        Err(CatalogError::OwnerFenced)
    ));
    assert!(
        matches!(
            f.catalog
                .stage_observed_body_layout(&f.serial, 12, batch, &proof)
                .await,
            Err(CatalogError::OwnerFenced)
        ),
        "old proof cannot merely change its execution epoch argument"
    );
    let tr = f.catalog.db.create_trx().unwrap();
    assert_eq!(
        layout_chunks_at(&tr, &namespace_prefix(f.namespace), None, 100)
            .await
            .unwrap(),
        before,
        "lost owner cannot add chunks, progress, or binding"
    );
    drop(tr);
    drop(proof);
    drop(volume);

    let new_drive = f.library.drive_as(0, 72);
    let new_key = ReservationKey::new(72, 12);
    fence(&new_drive, new_key).unwrap();
    let mut volume = LtfsVolume::mount(&new_drive).unwrap();
    volume.set_reservation_guard(Some(new_key));
    let writes = write_count(&f.library);
    // This calls the real upgrade-intent/receipt and committed-volume factory;
    // it never edits the old proof or substitutes a new request/batch.
    let recovered = Box::pin(recover_sealed_active_media_batch(
        &f.catalog, volume, &f.serial, 12,
    ))
    .await
    .unwrap();
    assert_eq!(recovered.files()[0].receipt.owner_epoch, 12);
    assert_eq!(
        receipt_fingerprint(&recovered.files()[0].receipt),
        fingerprint
    );
    let recovered = Box::pin(publish_active_media_batch(&f.catalog, recovered))
        .await
        .unwrap();
    assert_eq!(write_count(&f.library), writes);
    assert_eq!(
        recovered.files()[0].receipt.copy_id,
        prepared.receipt.copy_id
    );
    let lookup = f
        .catalog
        .resolve_active_read_source_for_owner(
            &f.serial,
            format!("/{}/fenced", f.pool).as_bytes(),
            f.route,
            12,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        lookup
            .body_layout(&f.catalog, 8 << 20)
            .await
            .unwrap()
            .is_some()
    );
    drop(recovered);
    fs::remove_dir_all(&f.root).unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实Sim producer/overlay/read/S4恢复，默认栈，不接触硬件"]
async fn producer_layout_precedes_overlay_and_reads_without_sqlite() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(fixture(&cluster)).await;
    Box::pin(overlay_read(&fixture)).await;
    Box::pin(unknown_s4(&fixture)).await;
    Box::pin(legacy_read(&fixture)).await;
    fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际提交原 delta/发现真实链/既有持久链不放宽，不接触设备"]
async fn recorded_construct_capture_discovers_delta_and_keeps_exact_chain_checks() {
    use crate::ltfs::recovery::{
        RecoveryBudget, capture_recorded_construct_source, capture_recorded_index_source,
    };
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let f = Box::pin(fixture(&cluster)).await;
    let drive = f.library.drive(0);
    fence(&drive, ReservationKey::new(71, 11)).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(71, 11)));
    volume
        .append_file(
            "/delta-proof",
            &mut std::io::Cursor::new(b"actual delta body"),
        )
        .unwrap();
    volume.commit_incremental().unwrap();
    let construct = volume.committed_index_construct();
    assert!(construct.incremental);
    let label = volume.label().clone();
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = f.root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_recorded_construct_source(
        &crate::tape::commands::TapeDrive::new(&drive),
        &label,
        construct,
        &budget,
    )
    .unwrap();
    assert_eq!(captured.construct(), construct);
    assert_eq!(
        captured.scope(),
        crate::ltfs::recovery::IndexSourceScope::Recorded
    );
    let identity = captured.identity().unwrap();
    assert!(!identity.predecessors().is_empty());
    let mut predecessors = identity.predecessors().to_vec();
    predecessors[0].sha256[0] ^= 1;
    let wrong = crate::ltfs::recovery::IndexSourceIdentity::new(construct, predecessors).unwrap();
    assert!(
        capture_recorded_index_source(
            &crate::tape::commands::TapeDrive::new(&drive),
            &label,
            &wrong,
            &budget
        )
        .is_err()
    );
    assert!(
        capture_recorded_index_source(
            &crate::tape::commands::TapeDrive::new(&drive),
            &label,
            &identity,
            &budget
        )
        .is_ok()
    );
    drop(captured);
    drop(volume);
    fs::remove_dir_all(&f.root).unwrap();
}
