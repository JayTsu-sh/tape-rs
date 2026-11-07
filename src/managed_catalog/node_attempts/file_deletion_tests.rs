use super::*;
use crate::daemon::fdb_media::{
    ActiveMediaUpload, commit_active_media_batch, publish_active_media_batch,
};

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实上传、删除、同名重建及旧物化重试，不接触硬件"]
async fn file_requests_stay_retired_when_materialization_retries_and_names_are_reused() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(true, true, false, true))
        .await
        .unwrap();
    let key = ReservationKey::new(3, fixture.owner);
    fence(&fixture.device, key).unwrap();
    let mut volume = LtfsVolume::mount(&fixture.device).unwrap();
    volume.set_reservation_guard(Some(key));
    let scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    Box::pin(workflow(scope, volume, &fixture.device, &fixture.root)).await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn upload<'a>(
    scope: &Scope<'_>,
    volume: LtfsVolume<'a>,
    root: &std::path::Path,
    file: Uuid,
    bytes: &[u8],
) -> (LtfsVolume<'a>, CommitReceipt, Uuid) {
    use std::io::Write;
    let operation = Uuid::new_v4();
    let spool = root.join(format!("actual-upload-{operation}.bin"));
    let mut stream = std::fs::File::create(&spool).unwrap();
    stream.write_all(bytes).unwrap();
    stream.sync_all().unwrap();
    drop(stream);
    let before = volume.committed_index_construct();
    let epoch = scope
        .catalog
        .request_window(scope.route.route.namespace)
        .await
        .unwrap()
        .accepting;
    let intent = ActiveFileIntent {
        barcode_constraint: None,
        namespace: scope.route.route.namespace,
        parent: active_pools::root_id(scope.route.route.namespace, scope.pool),
        parent_path: b"/".to_vec(),
        name: b"uploaded".to_vec(),
        file_id: file,
        expected_version: None,
        operation_id: operation,
        request_epoch: epoch,
        library_serial: scope.serial.into(),
        pool_id: Some(scope.pool),
        owner_epoch: scope.owner,
        volume_id: before.volume_uuid,
        volume_generation: before.generation + 1,
        volume_scan_id: scope.scan,
        expected_size: bytes.len() as u64,
        expected_digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
    };
    let batch = Uuid::new_v4();
    let committed = commit_active_media_batch(
        scope.catalog,
        volume,
        batch,
        &[ActiveMediaUpload { intent, spool }],
        64 << 20,
    )
    .await
    .unwrap();
    let receipt = committed.files()[0].receipt.clone();
    assert_eq!(receipt.file_id, file);
    assert_eq!(receipt.request_epoch, epoch);
    assert_eq!(
        receipt.volume_generation,
        committed.current_index().generation
    );
    let published = publish_active_media_batch(scope.catalog, committed)
        .await
        .unwrap();
    assert!(
        scope
            .catalog
            .active_batch_status(scope.serial, receipt.volume_id, receipt.volume_generation)
            .await
            .unwrap()
            .unwrap()
            .completed
    );
    (published.into_volume(), receipt, batch)
}

fn cache(
    volume: &LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
    name: &str,
) -> std::path::PathBuf {
    use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
    use crate::ltfs::recovery::RecoveryBudget;
    use crate::tape::commands::TapeDrive;
    let source = volume.committed_index_construct();
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.into();
    budget.index_scratch.max_bytes = 64 << 20;
    budget.index_scratch.headroom_bytes = 0;
    let path = root.join(format!("{name}-cache.sqlite"));
    // Match the runtime's FileRequest proof: read the actual recorded Full or
    // self-contained file-batch delta, without fabricating replayed XML/SHA.
    VolumeExtentCache::build_managed_from_media(
        &TapeDrive::new(device),
        volume.label(),
        &budget,
        &path,
        ExpectedIndex {
            volume_uuid: source.volume_uuid,
            generation: source.generation,
            location: source.location,
            sha256: source.sha256,
        },
        source.incremental,
        &mut CacheBuild::new(
            root,
            ReadStorageBudget {
                cache_bytes: 128 << 20,
                headroom_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .unwrap();
    path
}

async fn workflow(
    mut scope: Scope<'_>,
    mut volume: LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
) {
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    let ee_data = volume
        .index()
        .find_file("/.LTFSEE_DATA/data")
        .unwrap()
        .clone();
    // Make EE ownership an actual observed/published node change before the
    // file writer runs, not an unobserved edit of its bound source medium.
    let alias = scope.spec("/alias", Some(3), NodeMutation::Unlink);
    let (request, plan, attempt) = scope
        .admit(&mut volume, &alias, "2026-11-01T00:03:00.000000000Z")
        .await;
    scope.commit(&mut volume, &plan, &attempt).await;
    publish(&scope, &request, &plan, &attempt).await;
    let original_body = b"original FileRequest body kept after unlink";
    let original_file = Uuid::new_v4();
    let (mut volume, receipt, batch) =
        Box::pin(upload(&scope, volume, root, original_file, original_body)).await;
    let full = pool_catalog_path(scope.pool, b"/uploaded").unwrap();
    let original_copy = catalog
        .physical_copy(namespace, original_file, receipt.copy_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        original_copy.origin,
        CopyOrigin::FileRequest { request_epoch: 1 }
    );
    let old_cache = cache(&volume, device, root, "original-upload");
    let old_read = catalog
        .resolve_active_read_source_for_owner(scope.serial, &full, scope.route, scope.owner)
        .await
        .unwrap()
        .unwrap()
        .plan_read(&old_cache)
        .unwrap();
    let spec = scope.spec("/uploaded", Some(1), NodeMutation::Unlink);
    let (request, plan, attempt) = scope
        .admit(&mut volume, &spec, "2026-11-01T00:03:01.000000000Z")
        .await;
    takeover(&mut scope, &mut volume, device).await;
    scope.commit(&mut volume, &plan, &attempt).await;
    let deleted = publish(&scope, &request, &plan, &attempt).await;
    assert_eq!(deleted.version(), 2);
    assert_eq!(
        deleted.previous_copy(),
        Some((receipt.copy_id, &original_copy))
    );
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .node(namespace, original_file)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .file_head(namespace, original_file)
            .await
            .unwrap()
            .is_none()
    );
    let mut old_bytes = Vec::new();
    crate::ltfs::volume::read_cached_file_to_writer(
        device,
        volume.label(),
        &old_read.read.physical,
        &mut old_bytes,
    )
    .unwrap();
    assert_eq!(old_bytes, original_body);
    let retired = catalog
        .db
        .create_trx()
        .unwrap()
        .get(&node_key(namespace, original_file), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    let commands = device.command_log().len();
    assert_eq!(
        catalog
            .materialize_published_active_file(scope.serial, scope.owner, batch, 0, &receipt)
            .await
            .unwrap(),
        ActiveMaterializationOutcome::AlreadyMaterialized
    );
    assert_eq!(device.command_log().len(), commands);
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    let replacement_file = Uuid::new_v4();
    let replacement_body = b"replacement FileRequest body, a distinct inode";
    let (volume, replacement, _) = Box::pin(upload(
        &scope,
        volume,
        root,
        replacement_file,
        replacement_body,
    ))
    .await;
    assert_ne!(replacement.file_id, receipt.file_id);
    let replacement_head = catalog
        .file_head(namespace, replacement_file)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (replacement_head.version, replacement_head.copy_id),
        (1, replacement.copy_id)
    );
    let new_cache = cache(&volume, device, root, "replacement-upload");
    let new_read = catalog
        .resolve_active_read_source_for_owner(scope.serial, &full, scope.route, scope.owner)
        .await
        .unwrap()
        .unwrap()
        .plan_read(&new_cache)
        .unwrap();
    let commands = device.command_log().len();
    assert_eq!(
        catalog
            .materialize_published_active_file(scope.serial, scope.owner, batch, 0, &receipt)
            .await
            .unwrap(),
        ActiveMaterializationOutcome::AlreadyMaterialized
    );
    assert_eq!(publish(&scope, &request, &plan, &attempt).await, deleted);
    assert_eq!(device.command_log().len(), commands);
    assert_eq!(
        catalog
            .file_head(namespace, replacement_file)
            .await
            .unwrap()
            .unwrap(),
        replacement_head
    );
    let Some(ResolvedPath::File { id, head, copy, .. }) = catalog
        .resolve_active_path_for_route(scope.serial, &full, scope.route)
        .await
        .unwrap()
    else {
        panic!("replacement path must still resolve to the new file")
    };
    assert_eq!((id, head), (replacement_file, replacement_head));
    assert_eq!(
        copy.content_digest,
        ContentDigest::Sha256(Sha256::digest(replacement_body).into())
    );
    assert_eq!(
        catalog
            .db
            .create_trx()
            .unwrap()
            .get(&node_key(namespace, original_file), false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        retired
    );
    assert!(
        catalog
            .node(namespace, original_file)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog
            .physical_copy(namespace, original_file, receipt.copy_id)
            .await
            .unwrap(),
        Some(original_copy)
    );
    let mut bytes = Vec::new();
    crate::ltfs::volume::read_cached_file_to_writer(
        device,
        volume.label(),
        &new_read.read.physical,
        &mut bytes,
    )
    .unwrap();
    assert_eq!(bytes, replacement_body);
    assert_eq!(
        volume.index().find_file("/.LTFSEE_DATA/data").unwrap(),
        &ee_data
    );
    let ownership = volume.index().find_directory("/.LTFSEE_DATA").unwrap();
    assert_eq!(
        ownership
            .xattrs
            .iter()
            .filter(|attr| attr.key == "tapers.fdb.eeInternalDirectory"
                && attr.value == "1"
                && !attr.base64)
            .count(),
        1
    );
    assert_eq!(
        catalog.request_window(namespace).await.unwrap().accepting,
        1
    );
}
