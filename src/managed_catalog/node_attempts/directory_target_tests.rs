use super::*;

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际跨卷目录投影、逻辑 UID 与缺失目录删除，不接触硬件"]
async fn directories_modify_and_retire_on_other_volumes_without_losing_logical_identity() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(true, false, false, true))
        .await
        .unwrap();
    let scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    let library = SimLibrary::new(2, 4, 1);
    for (slot, barcode) in ["DIRB01L8", "DIRC01L8"].into_iter().enumerate() {
        library
            .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), slot)
            .unwrap();
        library.load_into_drive(barcode, slot).unwrap();
    }
    let device_b = library.drive(0);
    let device_c = library.drive(1);
    Box::pin(workflow(
        &scope,
        &fixture.device,
        &device_b,
        &device_c,
        &fixture.root,
    ))
    .await;
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn intake<'a>(
    scope: &Scope<'_>,
    device: &'a crate::scsi::sim::SimTransport,
    root: &std::path::Path,
    barcode: &str,
) -> (LtfsVolume<'a>, Uuid) {
    mkltfs(
        device,
        &MkltfsOptions {
            volume_id: barcode[..6].into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let key = ReservationKey::new(1, scope.owner);
    fence(device, key).unwrap();
    let mut volume = LtfsVolume::mount(device).unwrap();
    volume.set_reservation_guard(Some(key));
    let index: ActiveIndexIdentity = volume.verified_committed_index_construct().unwrap().into();
    let xml = volume.index().to_xml().unwrap();
    assert!(!index.incremental);
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&xml)), index.sha256);
    let expected = ExpectedIndex {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.location,
        sha256: index.sha256,
    };
    let spool_path = root.join(format!("directory-{barcode}-source.sqlite"));
    let cache_path = root.join(format!("directory-{barcode}-cache.sqlite"));
    ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
    VolumeExtentCache::build(xml.as_ref(), &cache_path, expected).unwrap();
    let spool = ImportSpool::open(&spool_path, expected).unwrap();
    let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
    spool.verify_extent_cache(&cache).unwrap();
    let source = NativeMediaSource {
        identity: spool.identity().clone(),
        files: cache.file_count().unwrap(),
        paths: spool.candidate_stats().entries,
    };
    assert_eq!((source.files, source.paths), (0, 0));
    let request = ManagementRequest::parse(json!({"version":1,"library":scope.serial,
        "namespace":scope.route.route.namespace.to_string(),"epoch":9,
        "operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import",
        "pool":scope.pool.to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}}))
    .unwrap();
    scope
        .catalog
        .admit_management(scope.serial, scope.route, scope.owner, &request)
        .await
        .unwrap();
    let intake = scope
        .catalog
        .start_native_media(
            scope.serial,
            scope.route,
            scope.owner,
            9,
            request.operation(),
        )
        .await
        .unwrap()
        .unwrap();
    scope
        .catalog
        .bind_native_media_source(&intake, scope.owner, &source)
        .await
        .unwrap();
    scope
        .catalog
        .begin_native_media_projection(&intake, scope.owner)
        .await
        .unwrap();
    scope
        .catalog
        .finish_native_media(&intake, scope.owner, &source)
        .await
        .unwrap();
    (volume, intake.scan())
}

async fn workflow(
    scope: &Scope<'_>,
    original_device: &crate::scsi::sim::SimTransport,
    device_b: &crate::scsi::sim::SimTransport,
    device_c: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
) {
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    let path = "/parent/child";
    let full = pool_catalog_path(scope.pool, path.as_bytes()).unwrap();
    let Some(ResolvedPath::Directory {
        id,
        version,
        attributes,
        ..
    }) = catalog
        .resolve_active_path_for_route(scope.serial, &full, scope.route)
        .await
        .unwrap()
    else {
        panic!("canonical existing directory")
    };
    let logical_uid = attributes.unwrap().meta().unwrap().file_uid;
    let untouched_commands = original_device.command_log().len();
    let (mut volume_b, scan_b) = Box::pin(intake(scope, device_b, root, "DIRB01L8")).await;
    assert!(volume_b.index().find_directory("/parent").is_none());
    let scope_b = Scope {
        scan: scan_b,
        ..*scope
    };
    let spec = scope_b.spec(
        path,
        Some(version),
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    );
    let (request, plan, attempt) = scope_b
        .admit(&mut volume_b, &spec, "2026-11-01T00:04:00.000000000Z")
        .await;
    let parent_before = catalog
        .node(namespace, plan.context().parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.context().node, id);
    scope_b.commit(&mut volume_b, &plan, &attempt).await;
    let proof = volume_b.node_mutation_evidence(&plan).unwrap();
    assert_eq!(proof.attributes().unwrap().meta().unwrap().file_uid, 3);
    assert_eq!(
        proof.logical_attributes().unwrap().meta().unwrap().file_uid,
        logical_uid
    );
    assert_ne!(logical_uid, 3);
    let published = scope_b.publish(&request, &plan, &attempt).await;
    assert_eq!(published.version(), version + 1);
    assert_eq!(published.parent_version(), plan.context().parent_version);
    assert!(published.physical_copy().is_none());
    assert_eq!(volume_b.index().highest_file_uid, 3);
    assert_eq!(
        volume_b
            .index()
            .find_directory("/parent/child")
            .unwrap()
            .meta
            .file_uid,
        3
    );
    assert_eq!(
        catalog
            .node(namespace, plan.context().parent)
            .await
            .unwrap()
            .unwrap(),
        parent_before
    );
    let Some(ResolvedPath::Directory {
        id: new_id,
        version: new_version,
        attributes,
        ..
    }) = catalog
        .resolve_active_path_for_route(scope.serial, &full, scope.route)
        .await
        .unwrap()
    else {
        panic!("same logical directory after materialization")
    };
    assert_eq!((new_id, new_version), (id, version + 1));
    let attrs = attributes.unwrap();
    assert_eq!(attrs.meta().unwrap().file_uid, logical_uid);
    assert!(attrs.meta().unwrap().readonly);
    let (mut volume_c, scan_c) = Box::pin(intake(scope, device_c, root, "DIRC01L8")).await;
    let scope_c = Scope {
        scan: scan_c,
        ..*scope
    };
    assert!(volume_c.index().find_directory("/parent/child").is_none());
    let remove = scope_c.spec(path, Some(new_version), NodeMutation::RemoveDirectory);
    let (remove_request, remove_plan, remove_attempt) = scope_c
        .admit(&mut volume_c, &remove, "2026-11-01T00:04:01.000000000Z")
        .await;
    scope_c
        .commit(&mut volume_c, &remove_plan, &remove_attempt)
        .await;
    let deleted = catalog
        .publish_node_mutation_deletion(
            scope.route,
            scope.owner,
            &remove_attempt,
            &remove_plan,
            &remove_request,
            4 << 20,
        )
        .await
        .unwrap();
    assert_eq!(deleted.version(), new_version + 1);
    assert_eq!(
        deleted.parent_version(),
        remove_plan.context().parent_version + 1
    );
    assert!(deleted.previous_copy().is_none());
    // Only the missing parent was materialized. No phantom target UID is used.
    assert_eq!(volume_c.index().highest_file_uid, 2);
    assert!(volume_c.index().find_directory("/parent").is_some());
    assert!(volume_c.index().find_directory("/parent/child").is_none());
    assert!(catalog.node(namespace, id).await.unwrap().is_none());
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(scope_b.publish(&request, &plan, &attempt).await, published);
    assert!(
        catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
            .is_none()
    );
    let tr = catalog.db.create_trx().unwrap();
    assert!(
        retired_nodes::require_native_path_allowed(&tr, namespace, scope.pool, path)
            .await
            .is_err()
    );
    assert_eq!(original_device.command_log().len(), untouched_commands);
}
