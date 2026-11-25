//! Actual Sim source plus original FDB child; no fabricated retirement receipt.
use super::tests::{fixture, start_target, target_entries};
use super::*;
use crate::local_read_storage::{CacheBuild, ReadStorageBudget, download_file};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{CapturedIndexSource, RecoveryBudget, capture_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;

struct ActualMedia {
    _library: SimLibrary,
    label: LtfsLabel,
    captured: CapturedIndexSource,
    spool: ImportSpool,
    proof: NativeMediaSource,
}

fn actual_media(root: &std::path::Path, barcode: &str, body: Option<&[u8]>) -> ActualMedia {
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
        .unwrap();
    library.load_into_drive(barcode, 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "LEDGER".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    if let Some(body) = body {
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume
            .append_file("/docs/conflict.txt", &mut std::io::Cursor::new(body))
            .unwrap();
        volume.commit().unwrap();
    }
    let label = read_label_only(&device).unwrap();
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.to_path_buf();
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_index_source(&TapeDrive::new(&device), &label, &budget).unwrap();
    let directory = root.join(barcode);
    let mut build = CacheBuild::new(&directory, storage()).unwrap();
    let path = directory.join("source.sqlite");
    ImportSpool::build_captured(&captured, &path, &mut build).unwrap();
    build.finish().unwrap();
    let head = captured.construct();
    let spool = ImportSpool::open(
        &path,
        ExpectedIndex {
            volume_uuid: head.volume_uuid,
            generation: head.generation,
            location: head.location,
            sha256: head.sha256,
        },
    )
    .unwrap();
    let proof = NativeMediaSource {
        identity: captured.identity().unwrap(),
        files: spool.stats().files,
        paths: spool.candidate_stats().entries,
    };
    ActualMedia {
        _library: library,
        label,
        captured,
        spool,
        proof,
    }
}

fn storage() -> ReadStorageBudget {
    ReadStorageBudget {
        // Complete Full capture keeps replay and comparison tickets alive;
        // each initially reserves 16 MiB. Leave room for an input history hint.
        index_bytes: 64 << 20,
        cache_bytes: 16 << 20,
        download_bytes: 16 << 20,
        headroom_bytes: 0,
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实S4/readback/retire前后原child scoped布局，默认栈"]
async fn retained_live_and_retired_layout_keep_original_child_bytes_and_reject_required_loss() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("retained-layout-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let source_bytes = (0..131_089usize)
        .map(|at| (at % 251) as u8)
        .collect::<Vec<_>>();
    let expected_sha: [u8; 32] = Sha256::digest(&source_bytes).into();
    let source = actual_media(&root, "RECLAIMS1L8", Some(&source_bytes));
    let destination = actual_media(&root, "RECLAIMD1L8", None);
    for (barcode, media) in [("RECLAIMS1L8", &source), ("RECLAIMD1L8", &destination)] {
        let import = start_target(&c, &serial, route, pool.id, 100, barcode, ("/", true)).await;
        c.bind_native_media_source(&import, 100, &media.proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        for entry in target_entries(&import, &media.spool) {
            c.project_native_media_entry(&import, 100, &entry, true)
                .await
                .unwrap();
        }
        c.finish_native_media(&import, 100, &media.proof)
            .await
            .unwrap();
    }
    let assignment = c
        .tape_assignment(&serial, "RECLAIMS1L8")
        .await
        .unwrap()
        .unwrap();
    let change = ReclaimSpec::parse(&json!({"kind":"media-reclaim", "pool":pool.id.to_string(),
        "barcode":assignment.barcode, "volume":assignment.volume_id.unwrap().to_string(),
        "revision":assignment.revision, "confirm_format":true, "unassign":false}))
    .unwrap();
    let reservation = c
        .reserve_reclaim_pool(&serial, route, 100, 1, Uuid::new_v4(), &change)
        .await
        .unwrap();
    let file = loop {
        let walk = c.step_reclaim_walk(&reservation, 100).await.unwrap();
        if let Some(file) = walk.current() {
            break file.clone();
        }
        assert!(!walk.complete());
    };
    let path = pool_catalog_path(pool.id, file.path().as_bytes()).unwrap();
    let ordinary = c
        .resolve_active_read_source_for_owner(&serial, &path, route, 100)
        .await
        .unwrap()
        .unwrap();
    assert!(ordinary.layout_binding().is_none());
    // This temporary verification adapter establishes the original byte proof;
    // it is never used by the new managed getter or retained FDB plan below.
    let scratch = root.join("verification");
    let cache = scratch.join("source.sqlite");
    let mut build = CacheBuild::new(&scratch, storage()).unwrap();
    crate::catalog::volume_extents::VolumeExtentCache::build_captured(
        &source.captured,
        &cache,
        &mut build,
    )
    .unwrap();
    build.finish().unwrap();
    let metadata = ordinary.metadata();
    let read = ordinary.plan_read(&cache).unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&source.captured).unwrap();
    let source_head = ActiveIndexIdentity::from(source.captured.construct());
    let byte_proof = inventory
        .bind_verified_read(
            &reservation,
            &file,
            &read.read,
            source_head,
            expected_sha,
            None,
        )
        .unwrap();
    drop(read);
    std::fs::remove_dir_all(scratch).unwrap();
    let upload = ActiveUploadSpec {
        pool_id: pool.id,
        path: file.path().as_bytes().to_vec(),
        operation_id: file.operation(),
        expected_version: Some(file.version()),
        size: file.size(),
        digest: ContentDigest::Sha256(expected_sha),
    };
    let intent = c
        .prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &upload, &byte_proof)
        .await
        .unwrap();
    let request = intent.request();
    let original_r_key = copy_key(route.route.namespace, metadata.id, metadata.head.copy_id);
    let tr = c.db.create_trx().unwrap();
    let original_r = tr
        .get(&original_r_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    drop(tr);

    let retained = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    let mut wrong_child = request.clone();
    wrong_child.operation_id = Uuid::new_v4();
    assert!(
        c.reclaim_read_source(&reservation, 100, &wrong_child)
            .await
            .is_err()
    );
    let mut wrong_file = request.clone();
    wrong_file.file_id = Uuid::new_v4();
    assert!(
        c.reclaim_read_source(&reservation, 100, &wrong_file)
            .await
            .is_err()
    );
    assert_eq!(retained.byte_proof(), &byte_proof);
    let authority = retained.source().retained_authority().unwrap();
    assert_eq!(authority.original_record(), original_r);
    assert!(retained.source().layout_binding().is_none());
    assert!(
        body_layouts::VerifiedExistingCopyLayout::from_captured(
            retained.source(),
            &source.captured,
            &source.label
        )
        .is_err()
    );
    let proof = body_layouts::VerifiedExistingCopyLayout::from_retained_captured(
        retained.source(),
        &source.captured,
        &source.label,
    )
    .unwrap();
    let scope_key = body_layouts::test_retained_layout_key(authority);
    let (global_key, marker_key) = body_layouts::test_copy_layout_keys(
        route.route.namespace,
        metadata.id,
        metadata.head.copy_id,
    );
    assert!(
        c.backfill_existing_body_layout(&serial, route, 99, &proof)
            .await
            .is_err()
    );
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&scope_key, false).await.unwrap().is_none());
    drop(tr);
    for changed_key in [
        original_r_key.clone(),
        head_key(route.route.namespace, metadata.id),
        source_key(metadata.copy.volume_id, metadata.copy.volume_scan_id),
    ] {
        // Delete then restore a genuine original row. No alternate R, source
        // identity or head is manufactured to satisfy the factory.
        let tr = c.db.create_trx().unwrap();
        let original = tr.get(&changed_key, false).await.unwrap().unwrap().to_vec();
        tr.clear(&changed_key);
        tr.commit().await.unwrap();
        assert!(
            c.backfill_existing_body_layout(&serial, route, 100, &proof)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&scope_key, false).await.unwrap().is_none());
        tr.set(&changed_key, &original);
        tr.commit().await.unwrap();
    }
    let expected_binding = proof.test_binding().clone();
    let before_submit = body_layouts::arm_attach_test_fault(
        route.route.namespace,
        body_layouts::AttachTestPoint::BeforeSubmit,
    );
    assert!(matches!(
        c.backfill_existing_body_layout(&serial, route, 100, &proof)
            .await,
        Err(CatalogError::Incomplete)
    ));
    assert!(before_submit.fired());
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&scope_key, false).await.unwrap().is_none());
    for key in body_layouts::test_small_layout_object_keys(&expected_binding) {
        assert!(tr.get(&key, false).await.unwrap().is_some());
    }
    drop(tr);
    assert!(
        c.reclaim_read_source(&reservation, 100, &request)
            .await
            .unwrap()
            .source()
            .layout_binding()
            .is_none()
    );
    drop(before_submit);
    let after_commit = body_layouts::arm_attach_test_fault(
        route.route.namespace,
        body_layouts::AttachTestPoint::AfterCommitted,
    );
    let unknown = c
        .backfill_existing_body_layout(&serial, route, 100, &proof)
        .await
        .unwrap_err();
    assert!(
        matches!(unknown.domain(), CatalogError::Fdb(foundationdb::FdbBindingError::NonRetryableFdbError(error)) if error.code() == 1021),
        "{unknown}"
    );
    assert!(after_commit.fired());
    let tr = c.db.create_trx().unwrap();
    let committed_scope_raw = tr.get(&scope_key, false).await.unwrap().unwrap().to_vec();
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(original_r.as_slice())
    );
    drop(tr);
    drop(after_commit);
    // Recover by querying the same original child/key, not a new operation.
    let recovered = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    let attached = recovered.source().layout_binding().unwrap().clone();
    assert_eq!(attached, expected_binding);
    assert_eq!(
        c.backfill_existing_body_layout(&serial, route, 100, &proof)
            .await
            .unwrap(),
        attached
    );
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&scope_key, false).await.unwrap().as_deref(),
        Some(committed_scope_raw.as_slice())
    );
    drop(tr);
    assert!(committed_scope_raw.len() <= 2_000);
    eprintln!(
        "ACTUAL_SCOPE_COMMIT_TEST_RECEIPT_LOST_RAW_HEX={}",
        committed_scope_raw
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let resolved = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    assert_eq!(resolved.source().layout_binding(), Some(&attached));
    let plan = resolved
        .source()
        .prepare_body_plan(
            &c,
            download_file(&root, attached.encoded_bytes(), storage()).unwrap(),
            16 << 20,
        )
        .await
        .unwrap();
    assert_eq!(
        plan.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    assert_eq!(plan.path(), "docs/conflict.txt");
    let (_, actual_node) = retained.source().captured_body(&source.captured).unwrap();
    assert!(plan.matches_extents(&actual_node.extents).unwrap());
    assert!(!actual_node.extents.is_empty());
    assert_eq!(plan.length(), source_bytes.len() as u64);
    assert_eq!(plan.uid(), metadata.copy.ltfs_file_uid);
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(original_r.as_slice())
    );
    assert!(tr.get(&scope_key, false).await.unwrap().is_some());
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    drop(tr);
    // A global promise cannot be hidden by the valid scoped legacy value.
    let tr = c.db.create_trx().unwrap();
    tr.set(
        &marker_key,
        &body_layouts::test_required_layout_value(&attached),
    );
    tr.commit().await.unwrap();
    let error = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap_err();
    assert!(
        matches!(error.domain(), CatalogError::Corrupt(_)),
        "{error}"
    );
    let tr = c.db.create_trx().unwrap();
    tr.clear(&marker_key);
    tr.commit().await.unwrap();
    c.reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    // Publish a real destination S4 and independently read its body/metadata.
    // Only the existing checked retirement API may produce x, marker and V.
    let drive = destination._library.drive(0);
    let key = crate::scsi::reservation::ReservationKey::new(7, 100);
    crate::scsi::reservation::fence(&drive, key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let body = root.join("body.bin");
    std::fs::write(&body, &source_bytes).unwrap();
    let committed = Box::pin(crate::daemon::fdb_media::commit_active_media_batch(
        &c,
        volume,
        Uuid::new_v4(),
        &[crate::daemon::fdb_media::ActiveMediaUpload {
            intent: intent.clone(),
            spool: body,
        }],
        16 << 20,
    ))
    .await
    .unwrap();
    let committed = Box::pin(crate::daemon::fdb_media::publish_active_media_batch(
        &c, committed,
    ))
    .await
    .unwrap();
    let destination_head = committed.current_index();
    drop(committed);
    let permission = std::sync::Arc::new(crate::daemon::permission::Permission::default());
    assert!(permission.renew(
        100,
        crate::daemon::permission::Phase::Execution,
        std::time::Instant::now() + std::time::Duration::from_secs(120)
    ));
    let guarded = crate::daemon::permission::GuardedTransport::new(
        Box::new(destination._library.drive(0)),
        permission.clone(),
        100,
    );
    let reader = crate::daemon::fdb_read::ActiveMediaReader {
        catalog: &c,
        device: &guarded,
        serial: &serial,
        route,
        key,
        permission: &permission,
        storage: storage(),
    };
    let read_cache = root.join("forbidden-service-cache");
    let read_spool = root.join("read-spool");
    std::fs::create_dir(&read_spool).unwrap();
    let (mut readback, _) = Box::pin(reader.read_reclaim_metadata_with_cache(
        &reservation,
        &request,
        &inventory,
        &read_cache,
        &read_spool,
    ))
    .await
    .unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut readback.file, &mut bytes).unwrap();
    assert_eq!(bytes, source_bytes);
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&bytes)), expected_sha);
    assert_eq!(readback.verified_head, destination_head);
    assert_eq!(
        readback.physical.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    assert!(!read_cache.exists());
    c.verify_reclaim_file_metadata(
        &reservation,
        100,
        &file,
        &request,
        &readback.metadata,
        readback.verified_head,
    )
    .await
    .unwrap();
    let live_before_retirement = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    live_before_retirement
        .source()
        .prepare_body_plan(
            &c,
            download_file(&root, attached.encoded_bytes(), storage()).unwrap(),
            16 << 20,
        )
        .await
        .unwrap();
    assert!(
        c.retire_reclaim_file_source(&reservation, 100, &file, &request)
            .await
            .unwrap()
    );
    let retired = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    assert_eq!(
        retired
            .source()
            .retained_authority()
            .unwrap()
            .original_record(),
        original_r
    );
    assert_eq!(retired.source().layout_binding(), Some(&attached));
    assert_eq!(retired.source().source, source_head);
    let retired_plan = retired
        .source()
        .prepare_body_plan(
            &c,
            download_file(&root, attached.encoded_bytes(), storage()).unwrap(),
            16 << 20,
        )
        .await
        .unwrap();
    assert_eq!(
        retired_plan.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    assert_eq!(retired_plan.length(), source_bytes.len() as u64);
    assert_eq!(retired_plan.uid(), metadata.copy.ltfs_file_uid);
    assert!(retired_plan.matches_extents(&actual_node.extents).unwrap());
    let source_drive = source._library.drive(0);
    crate::scsi::reservation::fence(&source_drive, key).unwrap();
    let source_guarded = crate::daemon::permission::GuardedTransport::new(
        Box::new(source._library.drive(0)),
        permission.clone(),
        100,
    );
    let source_reader = crate::daemon::fdb_read::ActiveMediaReader {
        device: &source_guarded,
        ..reader
    };
    let mut original_body = Box::pin(source_reader.read_reclaim_with_cache(
        &reservation,
        &request,
        &read_cache,
        &read_spool,
    ))
    .await
    .unwrap()
    .unwrap();
    let mut original_bytes = Vec::new();
    std::io::Read::read_to_end(&mut original_body.file, &mut original_bytes).unwrap();
    assert_eq!(original_bytes, source_bytes);
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(&original_bytes)),
        expected_sha
    );
    assert_eq!(
        original_body.physical.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    assert_eq!(original_body.physical.uid(), metadata.copy.ltfs_file_uid);
    assert!(!read_cache.exists());
    let (chunk_key, extent_byte) = body_layouts::test_first_extent_chunk(&attached);
    let tr = c.db.create_trx().unwrap();
    let original_chunk = tr.get(&chunk_key, false).await.unwrap().unwrap().to_vec();
    let mut wrong_extent = original_chunk.clone();
    wrong_extent[extent_byte] ^= 1;
    tr.set(&chunk_key, &wrong_extent);
    tr.commit().await.unwrap();
    let extent_error = Box::pin(source_reader.read_reclaim_with_cache(
        &reservation,
        &request,
        &read_cache,
        &read_spool,
    ))
    .await
    .err()
    .expect("changed physical extent must fail");
    assert!(
        extent_error.to_string().contains("正文布局"),
        "{extent_error}"
    );
    assert!(
        !read_cache.exists(),
        "known corrupt extent cannot create a local fallback"
    );
    let tr = c.db.create_trx().unwrap();
    tr.set(&chunk_key, &original_chunk);
    tr.commit().await.unwrap();
    let mut restored = Box::pin(source_reader.read_reclaim_with_cache(
        &reservation,
        &request,
        &read_cache,
        &read_spool,
    ))
    .await
    .unwrap()
    .unwrap();
    let mut restored_bytes = Vec::new();
    std::io::Read::read_to_end(&mut restored.file, &mut restored_bytes).unwrap();
    assert_eq!(restored_bytes, source_bytes);
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(&restored_bytes)),
        expected_sha
    );
    assert_eq!(
        restored.physical.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    assert!(!read_cache.exists());
    eprintln!(
        "AFTER_EXTENT_RESTORED_ACTUAL_GUARDED_READ_BYTES={}",
        restored_bytes.len()
    );
    eprintln!(
        "RETAINED_NONEMPTY_BYTES={} SHA256={:x} original_uid={} namespace={} child={} root={}",
        source_bytes.len(),
        Sha256::digest(&source_bytes),
        metadata.copy.ltfs_file_uid,
        route.route.namespace,
        request.operation_id,
        root.display()
    );
    let tr = c.db.create_trx().unwrap();
    let marker = tr
        .get(&original_r_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    assert_ne!(marker, original_r);
    assert_eq!(
        marker,
        encode_reclaimed_copy(metadata.copy.version, metadata.copy.origin)
    );
    assert!(matches!(
        decode_stored_copy(&marker).unwrap(),
        StoredCopy::Reclaimed { .. }
    ));
    let reverse = volume_key(
        metadata.copy.volume_id,
        metadata.copy.volume_scan_id,
        route.route.namespace,
        metadata.id,
        metadata.head.copy_id,
    );
    assert!(tr.get(&reverse, false).await.unwrap().is_none());
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    let actual_head_key = head_key(route.route.namespace, metadata.id);
    let actual_head = tr
        .get(&actual_head_key, false)
        .await
        .unwrap()
        .map(|v| v.to_vec());
    let [child_key, identity_key, retired_key] = retired
        .source()
        .retained_authority()
        .unwrap()
        .test_dependency_keys();
    let receipt = tr.get(&retired_key, false).await.unwrap().unwrap().to_vec();
    let receipt_value: Value = serde_json::from_slice(&receipt).unwrap();
    assert_eq!(
        receipt_value["source_copy_sha256"],
        format!("{:x}", Sha256::digest(&original_r))
    );
    drop(tr);
    // Missing legacy projection after retirement still uses the real child
    // original R and a fresh Retired authority, never the current marker.
    let tr = c.db.create_trx().unwrap();
    let original_scope = tr.get(&scope_key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&scope_key);
    tr.commit().await.unwrap();
    let missing = c
        .reclaim_read_source(&reservation, 100, &request)
        .await
        .unwrap();
    assert!(missing.source().layout_binding().is_none());
    let retired_proof = body_layouts::VerifiedExistingCopyLayout::from_retained_captured(
        missing.source(),
        &source.captured,
        &source.label,
    )
    .unwrap();
    for changed_key in [
        original_r_key.clone(),
        actual_head_key.clone(),
        source_key(metadata.copy.volume_id, metadata.copy.volume_scan_id),
    ] {
        // This is the Retired authority: current R is the real marker, H is
        // the actual target head, and source is the original recorded source.
        let tr = c.db.create_trx().unwrap();
        let raw = tr.get(&changed_key, false).await.unwrap().unwrap().to_vec();
        tr.clear(&changed_key);
        tr.commit().await.unwrap();
        assert!(
            c.backfill_existing_body_layout(&serial, route, 100, &retired_proof)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&scope_key, false).await.unwrap().is_none());
        tr.set(&changed_key, &raw);
        tr.commit().await.unwrap();
    }
    assert_eq!(
        c.backfill_existing_body_layout(&serial, route, 100, &retired_proof)
            .await
            .unwrap(),
        attached
    );
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&scope_key, false).await.unwrap().as_deref(),
        Some(original_scope.as_slice())
    );
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(marker.as_slice())
    );
    assert_eq!(
        tr.get(&retired_key, false).await.unwrap().as_deref(),
        Some(receipt.as_slice())
    );
    assert_eq!(
        tr.get(&actual_head_key, false).await.unwrap().as_deref(),
        actual_head.as_deref()
    );
    assert!(tr.get(&reverse, false).await.unwrap().is_none());
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    drop(tr);
    eprintln!("RETIRED_CURRENT_MARKER_H_SOURCE_DELETE_RESTORE_CAS_REJECTIONS=3");
    let tr = c.db.create_trx().unwrap();
    assert!(
        lookup::read_source_at(
            &tr,
            &serial,
            &path,
            ResolvedPath::File {
                id: metadata.id,
                head: metadata.head.clone(),
                copy: Box::new(metadata.copy.clone()),
                placement: metadata.placement.clone(),
                attributes: metadata.attributes.clone(),
                object_metadata: None,
            }
        )
        .await
        .is_err(),
        "scoped layout cannot resurrect an ordinary reclaimed copy"
    );
    drop(tr);
    let stale = live_before_retirement
        .source()
        .prepare_body_plan(
            &c,
            download_file(&root, attached.encoded_bytes(), storage()).unwrap(),
            16 << 20,
        )
        .await;
    assert!(
        stale.is_err(),
        "Live snapshot cannot silently change into Retired authority"
    );
    for dependency in [child_key, identity_key, retired_key] {
        let tr = c.db.create_trx().unwrap();
        let original = tr.get(&dependency, false).await.unwrap().unwrap().to_vec();
        tr.clear(&dependency);
        tr.commit().await.unwrap();
        assert!(
            c.reclaim_read_source(&reservation, 100, &request)
                .await
                .is_err()
        );
        let tr = c.db.create_trx().unwrap();
        tr.set(&dependency, &original);
        tr.commit().await.unwrap();
        c.reclaim_read_source(&reservation, 100, &request)
            .await
            .unwrap();
    }
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&original_r_key, false).await.unwrap().as_deref(),
        Some(marker.as_slice())
    );
    assert!(tr.get(&reverse, false).await.unwrap().is_none());
    assert_eq!(
        tr.get(&actual_head_key, false).await.unwrap().as_deref(),
        actual_head.as_deref()
    );
    drop(tr);
    c.claim_active_owner_epoch(&serial, 101).await.unwrap();
    assert!(
        c.reclaim_read_source(&reservation, 100, &request)
            .await
            .is_err()
    );
    assert!(
        c.backfill_existing_body_layout(&serial, route, 100, &proof)
            .await
            .is_err()
    );
    assert!(
        c.reclaim_read_source(&reservation, 101, &request)
            .await
            .is_err(),
        "new owner must recover original parent first"
    );
    std::fs::remove_dir_all(root).unwrap();
}

async fn cancel_attempt_with_real_scoped_layout(
    c: &ManagedCatalog,
    route: ActiveLibraryRoute,
    parent: &ReclaimReservation,
    source: &ActualMedia,
    file: &ReclaimFile,
    root: &std::path::Path,
) -> (
    ActiveFileRequest,
    RetainedLedgerAuthority,
    CopyLayoutBinding,
) {
    let path = pool_catalog_path(parent.spec().pool(), file.path().as_bytes()).unwrap();
    let ordinary = c
        .resolve_active_read_source_for_owner(parent.serial(), &path, route, 100)
        .await
        .unwrap()
        .unwrap();
    assert!(ordinary.layout_binding().is_none());
    let scratch = root.join(format!("verify-{}", file.operation()));
    let cache = scratch.join("actual-source.sqlite");
    let mut build = CacheBuild::new(&scratch, storage()).unwrap();
    crate::catalog::volume_extents::VolumeExtentCache::build_captured(
        &source.captured,
        &cache,
        &mut build,
    )
    .unwrap();
    build.finish().unwrap();
    let read = ordinary.plan_read(&cache).unwrap();
    let drive = source._library.drive(0);
    let key = crate::scsi::reservation::ReservationKey::new(7, 100);
    crate::scsi::reservation::fence(&drive, key).unwrap();
    let permission = std::sync::Arc::new(crate::daemon::permission::Permission::default());
    assert!(permission.renew(
        100,
        crate::daemon::permission::Phase::Execution,
        std::time::Instant::now() + std::time::Duration::from_secs(120)
    ));
    let guarded = crate::daemon::permission::GuardedTransport::new(
        Box::new(source._library.drive(0)),
        permission,
        100,
    );
    let mut actual = Vec::new();
    let read_bytes = crate::ltfs::volume::read_cached_file_to_writer(
        &guarded,
        &source.label,
        &read.read.physical,
        &mut actual,
    )
    .unwrap();
    assert_eq!(read_bytes, file.size());
    let sha: [u8; 32] = Sha256::digest(&actual).into();
    let inventory = ReclaimSourceInventory::from_captured(&source.captured).unwrap();
    let byte_proof = inventory
        .bind_verified_read(
            parent,
            file,
            &read.read,
            source.captured.construct().into(),
            sha,
            None,
        )
        .unwrap();
    drop(read);
    std::fs::remove_dir_all(scratch).unwrap();
    let intent = c
        .prepare_reclaim_upload(
            parent,
            100,
            "RECLAIMD1L8",
            &ActiveUploadSpec {
                pool_id: parent.spec().pool(),
                path: file.path().as_bytes().to_vec(),
                operation_id: file.operation(),
                expected_version: Some(file.version()),
                size: file.size(),
                digest: ContentDigest::Sha256(sha),
            },
            &byte_proof,
        )
        .await
        .unwrap();
    let request = intent.request();
    let retained = c.reclaim_read_source(parent, 100, &request).await.unwrap();
    let authority = retained.source().retained_authority().unwrap().clone();
    let proof = body_layouts::VerifiedExistingCopyLayout::from_retained_captured(
        retained.source(),
        &source.captured,
        &source.label,
    )
    .unwrap();
    let binding = c
        .backfill_existing_body_layout(parent.serial(), route, 100, &proof)
        .await
        .unwrap();
    assert_eq!(
        c.cancel_unstarted_reclaim_upload(parent, 100, &request)
            .await
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    (request, authority, binding)
}

#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；真实cancel/retry双child清理kernel，不伪造parent完成"]
async fn scoped_cleanup_checks_full_real_child_page_before_mutation_and_preserves_shared_objects() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("retained-cleanup-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let source = actual_media(
        &root,
        "RECLAIMS1L8",
        Some(b"actual cancellation source body"),
    );
    let destination = actual_media(&root, "RECLAIMD1L8", None);
    for (barcode, media) in [("RECLAIMS1L8", &source), ("RECLAIMD1L8", &destination)] {
        let import = start_target(&c, &serial, route, pool.id, 100, barcode, ("/", true)).await;
        c.bind_native_media_source(&import, 100, &media.proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        for entry in target_entries(&import, &media.spool) {
            c.project_native_media_entry(&import, 100, &entry, true)
                .await
                .unwrap();
        }
        c.finish_native_media(&import, 100, &media.proof)
            .await
            .unwrap();
    }
    let assignment = c
        .tape_assignment(&serial, "RECLAIMS1L8")
        .await
        .unwrap()
        .unwrap();
    let change = ReclaimSpec::parse(&json!({"kind":"media-reclaim", "pool":pool.id.to_string(),
        "barcode":assignment.barcode, "volume":assignment.volume_id.unwrap().to_string(),
        "revision":assignment.revision, "confirm_format":true, "unassign":false}))
    .unwrap();
    let parent = c
        .reserve_reclaim_pool(&serial, route, 100, 1, Uuid::new_v4(), &change)
        .await
        .unwrap();
    let file = loop {
        let walk = c.step_reclaim_walk(&parent, 100).await.unwrap();
        if let Some(file) = walk.current() {
            break file.clone();
        }
        assert!(!walk.complete());
    };
    let (first_request, first_authority, first_binding) =
        cancel_attempt_with_real_scoped_layout(&c, route, &parent, &source, &file, &root).await;
    let successor = c
        .retry_cancelled_reclaim_upload(&parent, 100, &first_request)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(successor.operation(), file.operation());
    let (_, second_authority, second_binding) =
        cancel_attempt_with_real_scoped_layout(&c, route, &parent, &source, &successor, &root)
            .await;
    assert_eq!(
        first_binding.layout_digest(),
        second_binding.layout_digest()
    );
    let mut rows = Vec::new();
    for authority in [&first_authority, &second_authority] {
        let [child, identity, _] = authority.test_dependency_keys();
        let scope = body_layouts::test_retained_layout_key(authority);
        rows.push((child, identity, scope));
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let objects = body_layouts::test_small_layout_object_keys(&first_binding);
    assert_eq!(
        objects,
        body_layouts::test_small_layout_object_keys(&second_binding)
    );
    // This intentionally inert row proves prefix isolation only; it is not
    // another authenticated parent's authority or a deserializable binding.
    let neighbor = body_layouts::test_inert_neighbor_scope_key(&first_authority, Uuid::new_v4());
    let tr = c.db.create_trx().unwrap();
    tr.set(&neighbor, b"inert-prefix-sentinel");
    tr.commit().await.unwrap();
    let mut protected_keys = objects.clone();
    protected_keys.push(neighbor.clone());
    for (child, identity, scope) in &rows {
        protected_keys.extend([child.clone(), identity.clone(), scope.clone()]);
    }
    let tr = c.db.create_trx().unwrap();
    let mut original = Vec::new();
    for key in &protected_keys {
        original.push((
            key.clone(),
            tr.get(key, false).await.unwrap().unwrap().to_vec(),
        ));
    }
    drop(tr);
    assert!(
        c.step_reclaim_cleanup(&parent, 100).await.is_err(),
        "cancelled children do not forge a completed parent receipt"
    );
    let tr = c.db.create_trx().unwrap();
    for (key, raw) in &original {
        assert_eq!(
            tr.get(key, false).await.unwrap().as_deref(),
            Some(raw.as_slice())
        );
    }
    drop(tr);
    let last_identity = &rows.last().unwrap().1;
    let identity_raw = original
        .iter()
        .find(|(key, _)| key == last_identity)
        .unwrap()
        .1
        .clone();
    let tr = c.db.create_trx().unwrap();
    tr.clear(last_identity);
    tr.commit().await.unwrap();
    let tr = c.db.create_trx().unwrap();
    let error = first_authority.test_cleanup_page_at(&tr).await.unwrap_err();
    assert!(
        matches!(error.domain(), CatalogError::Corrupt(_)),
        "{error}"
    );
    // Deliberately commit this failed kernel's transaction: validation must
    // precede every mutation even without the production caller's rollback.
    tr.commit().await.unwrap();
    let tr = c.db.create_trx().unwrap();
    for (key, raw) in &original {
        if key == last_identity {
            assert!(tr.get(key, false).await.unwrap().is_none());
        } else {
            assert_eq!(
                tr.get(key, false).await.unwrap().as_deref(),
                Some(raw.as_slice())
            );
        }
    }
    tr.set(last_identity, &identity_raw);
    tr.commit().await.unwrap();
    let tr = c.db.create_trx().unwrap();
    assert!(first_authority.test_cleanup_page_at(&tr).await.unwrap());
    tr.commit().await.unwrap();
    let tr = c.db.create_trx().unwrap();
    for (child, identity, scope) in &rows {
        for key in [child, identity, scope] {
            assert!(tr.get(key, false).await.unwrap().is_none());
        }
    }
    for (key, raw) in &original {
        if objects.contains(key) || key == &neighbor {
            assert_eq!(
                tr.get(key, false).await.unwrap().as_deref(),
                Some(raw.as_slice())
            );
        }
    }
    drop(tr);
    eprintln!(
        "SCOPED_KERNEL_REAL_CHILDREN=2 SHARED_OBJECT_KEYS={} SENTINEL_ONLY_NEIGHBOR=true namespace={} root={}",
        objects.len(),
        route.route.namespace,
        root.display()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；真实global binding+LC1提交后测试层丢回执，不模拟clientlib重试"]
async fn original_present_copy_recovers_exact_global_layout_pair_after_lost_attach_receipt() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("global-attach-unknown-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let source = actual_media(
        &root,
        "RECLAIMS1L8",
        Some(b"actual original body with unknown attach receipt"),
    );
    let import = start_target(&c, &serial, route, pool.id, 100, "RECLAIMS1L8", ("/", true)).await;
    c.bind_native_media_source(&import, 100, &source.proof)
        .await
        .unwrap();
    c.begin_native_media_projection(&import, 100).await.unwrap();
    for entry in target_entries(&import, &source.spool) {
        c.project_native_media_entry(&import, 100, &entry, true)
            .await
            .unwrap();
    }
    c.finish_native_media(&import, 100, &source.proof)
        .await
        .unwrap();
    let path = pool_catalog_path(pool.id, b"/docs/conflict.txt").unwrap();
    let original = c
        .resolve_active_read_source_for_owner(&serial, &path, route, 100)
        .await
        .unwrap()
        .unwrap();
    let metadata = original.metadata();
    let r_key = copy_key(route.route.namespace, metadata.id, metadata.head.copy_id);
    let h_key = head_key(route.route.namespace, metadata.id);
    let (global_key, marker_key) = body_layouts::test_copy_layout_keys(
        route.route.namespace,
        metadata.id,
        metadata.head.copy_id,
    );
    let tr = c.db.create_trx().unwrap();
    let original_r = tr.get(&r_key, false).await.unwrap().unwrap().to_vec();
    let original_h = tr.get(&h_key, false).await.unwrap().map(|v| v.to_vec());
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    drop(tr);
    let proof = body_layouts::VerifiedExistingCopyLayout::from_captured(
        &original,
        &source.captured,
        &source.label,
    )
    .unwrap();
    let before_writes = source
        ._library
        .drive(0)
        .command_log()
        .iter()
        .filter(|&&op| matches!(op, 0x0a | 0x10))
        .count();
    let before = body_layouts::arm_attach_test_fault(
        route.route.namespace,
        body_layouts::AttachTestPoint::BeforeSubmit,
    );
    assert!(matches!(
        c.backfill_existing_body_layout(&serial, route, 100, &proof)
            .await,
        Err(CatalogError::Incomplete)
    ));
    assert!(before.fired());
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    for key in body_layouts::test_small_layout_object_keys(proof.test_binding()) {
        assert!(tr.get(&key, false).await.unwrap().is_some());
    }
    drop(tr);
    drop(before);
    let previous_binding = proof.test_binding().clone();
    c.claim_active_owner_epoch(&serial, 101).await.unwrap();
    assert!(
        c.backfill_existing_body_layout(&serial, route, 100, &proof)
            .await
            .is_err()
    );
    assert!(
        c.backfill_existing_body_layout(&serial, route, 101, &proof)
            .await
            .is_err(),
        "changing the argument cannot renew an old proof"
    );
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&global_key, false).await.unwrap().is_none());
    assert!(tr.get(&marker_key, false).await.unwrap().is_none());
    drop(tr);
    let drive = source._library.drive(0);
    let key = crate::scsi::reservation::ReservationKey::new(7, 101);
    crate::scsi::reservation::fence(&drive, key).unwrap();
    let permission = std::sync::Arc::new(crate::daemon::permission::Permission::default());
    assert!(permission.renew(
        101,
        crate::daemon::permission::Phase::Execution,
        std::time::Instant::now() + std::time::Duration::from_secs(120)
    ));
    let guarded = crate::daemon::permission::GuardedTransport::new(
        Box::new(source._library.drive(0)),
        permission,
        101,
    );
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let actual = crate::ltfs::recovery::capture_recorded_index_source(
        &TapeDrive::new(&guarded),
        &source.label,
        &source.proof.identity,
        &budget,
    )
    .unwrap();
    let fresh = c
        .resolve_active_read_source_for_owner(&serial, &path, route, 101)
        .await
        .unwrap()
        .unwrap();
    let proof =
        body_layouts::VerifiedExistingCopyLayout::from_captured(&fresh, &actual, &source.label)
            .unwrap();
    assert_eq!(
        proof.test_binding(),
        &previous_binding,
        "owner changes do not create another layout/copy identity"
    );
    let after = body_layouts::arm_attach_test_fault(
        route.route.namespace,
        body_layouts::AttachTestPoint::AfterCommitted,
    );
    let error = c
        .backfill_existing_body_layout(&serial, route, 101, &proof)
        .await
        .unwrap_err();
    assert!(
        matches!(error.domain(), CatalogError::Fdb(foundationdb::FdbBindingError::NonRetryableFdbError(error)) if error.code() == 1021),
        "{error}"
    );
    assert!(after.fired());
    let tr = c.db.create_trx().unwrap();
    let committed_binding = tr.get(&global_key, false).await.unwrap().unwrap().to_vec();
    let committed_marker = tr.get(&marker_key, false).await.unwrap().unwrap().to_vec();
    assert_eq!(
        committed_marker,
        body_layouts::test_required_layout_value(proof.test_binding())
    );
    assert_eq!(
        tr.get(&r_key, false).await.unwrap().as_deref(),
        Some(original_r.as_slice())
    );
    assert_eq!(
        tr.get(&h_key, false).await.unwrap().as_deref(),
        original_h.as_deref()
    );
    drop(tr);
    drop(after);
    let recovered = c
        .resolve_active_read_source_for_owner(&serial, &path, route, 101)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.metadata(), metadata);
    assert_eq!(recovered.layout_binding(), Some(proof.test_binding()));
    let plan = recovered
        .prepare_body_plan(
            &c,
            download_file(&root, proof.test_binding().encoded_bytes(), storage()).unwrap(),
            16 << 20,
        )
        .await
        .unwrap();
    assert_eq!(
        plan.provenance(),
        body_layouts::PhysicalPlanProvenance::FdbLayout
    );
    let after_writes = source
        ._library
        .drive(0)
        .command_log()
        .iter()
        .filter(|&&op| matches!(op, 0x0a | 0x10))
        .count();
    assert_eq!(after_writes, before_writes);
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&global_key, false).await.unwrap().as_deref(),
        Some(committed_binding.as_slice())
    );
    assert_eq!(
        tr.get(&marker_key, false).await.unwrap().as_deref(),
        Some(committed_marker.as_slice())
    );
    drop(tr);
    eprintln!(
        "ACTUAL_GLOBAL_PAIR_COMMITTED_THEN_TEST_RECEIPT_LOST=true binding_sha={:x} marker_sha={:x} namespace={} copy={}",
        Sha256::digest(&committed_binding),
        Sha256::digest(&committed_marker),
        route.route.namespace,
        metadata.head.copy_id
    );
    assert!(committed_binding.len() <= 2_000);
    eprintln!(
        "ACTUAL_GLOBAL_BINDING_RAW_HEX={} LC1_RAW_HEX={}",
        committed_binding
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        committed_marker
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    std::fs::remove_dir_all(root).unwrap();
}
