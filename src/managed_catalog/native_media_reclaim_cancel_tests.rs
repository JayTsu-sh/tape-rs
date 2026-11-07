use super::tests::{current_child_source, fixture, start_target, target_entries};
use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim建立源证明，不访问真实设备"]
async fn reclaim_prepared_cancel_settles_original_permit_once_and_survives_gc_and_owner() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("reclaim-children-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let mut captured_source = None;
    for (barcode, count) in [("RECLAIMS1L8", 1), ("RECLAIMD1L8", 0)] {
        let (captured, spool, proof) = current_child_source(&root, barcode, count != 0);
        if count != 0 {
            captured_source = Some(captured);
        }
        let import = start_target(&c, &serial, route, pool.id, 100, barcode, ("/", true)).await;
        c.bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        for entry in target_entries(&import, &spool) {
            c.project_native_media_entry(&import, 100, &entry, true)
                .await
                .unwrap();
        }
        c.finish_native_media(&import, 100, &proof).await.unwrap();
    }
    let source = c
        .tape_assignment(&serial, "RECLAIMS1L8")
        .await
        .unwrap()
        .unwrap();
    let change = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":false})).unwrap();
    let parent = Uuid::new_v4();
    let reservation = c
        .reserve_reclaim_pool(&serial, route, 100, 1, parent, &change)
        .await
        .unwrap();
    let scanned = loop {
        let state = c.step_reclaim_walk(&reservation, 100).await.unwrap();
        if let Some(file) = state.current() {
            break file.clone();
        }
        assert!(!state.complete());
    };
    assert_eq!(scanned.path(), "/docs/conflict.txt");
    let path = b"/docs/conflict.txt";
    let before = c
        .resolve_active_path_for_route(&serial, &pool_catalog_path(pool.id, path).unwrap(), route)
        .await
        .unwrap()
        .unwrap();
    let ResolvedPath::File { head, .. } = &before else {
        panic!("file");
    };
    let inventory =
        ReclaimSourceInventory::from_captured(captured_source.as_ref().unwrap()).unwrap();
    let cache_directory = root.join("extent-cache");
    let cache_path = cache_directory.join("current-source-extents.sqlite");
    let storage = crate::local_read_storage::ReadStorageBudget {
        index_bytes: 16 << 20,
        cache_bytes: 16 << 20,
        download_bytes: 16 << 20,
        headroom_bytes: 0,
    };
    let mut build = crate::local_read_storage::CacheBuild::new(&cache_directory, storage).unwrap();
    crate::catalog::volume_extents::VolumeExtentCache::build_captured(
        captured_source.as_ref().unwrap(),
        &cache_path,
        &mut build,
    )
    .unwrap();
    build.finish().unwrap();
    let original_read = c
        .resolve_active_read_source_for_owner(
            &serial,
            &pool_catalog_path(pool.id, path).unwrap(),
            route,
            100,
        )
        .await
        .unwrap()
        .unwrap()
        .plan_read(&cache_path)
        .unwrap();
    let source_head = c
        .observed_active_volume_index(&serial, source.volume_id.unwrap())
        .await
        .unwrap();
    let source_read = inventory
        .bind_verified_read(
            &reservation,
            &scanned,
            &original_read.read,
            source_head,
            Sha256::digest([]).into(),
            None,
        )
        .unwrap();
    let spec = ActiveUploadSpec {
        pool_id: pool.id,
        path: path.to_vec(),
        operation_id: scanned.operation(),
        expected_version: Some(head.version),
        size: 0,
        digest: ContentDigest::Sha256(Sha256::digest([]).into()),
    };

    let pending_before = c
        .maintenance_status(route.route.namespace)
        .await
        .unwrap()
        .pending;
    let saved = c
        .prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
        .await
        .unwrap();
    let request = saved.request();
    assert_eq!(
        c.maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending,
        pending_before + 1
    );
    let mut pin = requests::epoch_prefix(b'F', request.namespace, request.request_epoch);
    pin.extend_from_slice(request.operation_id.as_bytes());
    let mut op = requests::epoch_prefix(b'P', request.namespace, request.request_epoch);
    op.extend_from_slice(request.operation_id.as_bytes());
    let mut receipt_key = management::prefix(request.namespace, b'R');
    receipt_key.push(b'z');
    receipt_key.extend_from_slice(&1u64.to_be_bytes());
    receipt_key.extend_from_slice(parent.as_bytes());
    receipt_key.extend_from_slice(request.operation_id.as_bytes());
    let assert_pending = || async {
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            pending_before + 1
        );
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&op, false).await.unwrap().is_none());
        assert_eq!(
            c.resolve_active_path_for_route(
                &serial,
                &pool_catalog_path(pool.id, path).unwrap(),
                route
            )
            .await
            .unwrap()
            .unwrap(),
            before
        );
    };
    for bad in [None, Some(b"invalid".as_slice())] {
        let tr = c.db.create_trx().unwrap();
        assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
        match bad {
            None => tr.clear(&pin),
            Some(raw) => tr.set(&pin, raw),
        };
        tr.commit().await.unwrap();
        assert!(
            c.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
                .await
                .is_err()
        );
        assert_pending().await;
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&receipt_key, false).await.unwrap().is_none());
        tr.set(&pin, &[]);
        tr.commit().await.unwrap();
    }
    let mut q = requests::epoch_prefix(b'0', request.namespace, request.request_epoch);
    q.extend_from_slice(request.operation_id.as_bytes());
    let mut i = requests::epoch_prefix(b'I', request.namespace, request.request_epoch);
    i.extend_from_slice(request.operation_id.as_bytes());
    let mut reverse = reservation::volume_prefix(saved.volume_id, saved.volume_generation);
    reverse.extend_from_slice(&request.request_epoch.to_be_bytes());
    reverse.extend_from_slice(request.operation_id.as_bytes());
    let mut upgraded = reservation::upgraded_prefix(saved.volume_id, saved.volume_generation);
    upgraded.extend_from_slice(&request.request_epoch.to_be_bytes());
    upgraded.extend_from_slice(request.operation_id.as_bytes());
    for key in [
        q,
        i,
        reverse,
        upgraded,
        reservation::file_key_for(saved.namespace, saved.file_id),
        reservation::path_key_for(saved.namespace, saved.parent, &saved.name),
        active_batch::batch_key(saved.volume_id, saved.volume_generation),
        active_batch::prepared_batch_key(saved.volume_id, saved.volume_generation),
        receipt_key.clone(),
    ] {
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&key, false).await.unwrap().is_none());
        tr.set(&key, b"fixture-cancel-conflict");
        tr.commit().await.unwrap();
        assert!(
            c.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
                .await
                .is_err()
        );
        assert_pending().await;
        let tr = c.db.create_trx().unwrap();
        assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
        assert_eq!(
            tr.get(&key, false).await.unwrap().unwrap().as_ref(),
            b"fixture-cancel-conflict"
        );
        if key != receipt_key {
            assert!(tr.get(&receipt_key, false).await.unwrap().is_none());
        }
        tr.clear(&key);
        tr.commit().await.unwrap();
    }
    let mut wrong = request.clone();
    wrong.operation_id = Uuid::new_v4();
    assert!(
        c.cancel_unstarted_reclaim_upload(&reservation, 100, &wrong)
            .await
            .is_err()
    );
    assert_pending().await;
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let offline_path = root.join("unavailable.cluster");
    std::fs::write(
        &offline_path,
        format!(
            "offline:{}@{}\n",
            Uuid::new_v4().simple(),
            unavailable.local_addr().unwrap()
        ),
    )
    .unwrap();
    let offline = ManagedCatalog::open(Some(offline_path.to_str().unwrap())).unwrap();
    let failure = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        offline.cancel_unstarted_reclaim_upload(&reservation, 100, &request),
    )
    .await
    .expect("FDB不可达取消必须有界返回");
    assert!(
        matches!(failure, Err(CatalogError::Fdb(_) | CatalogError::Client(_))),
        "{failure:?}"
    );
    drop(offline);
    drop(unavailable);
    assert_pending().await;
    assert!(matches!(
        c.rotate_active_request_epoch(request.namespace, &serial, 100, request.request_epoch)
            .await
            .unwrap(),
        WindowOutcome::Advanced(_)
    ));
    let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (first, second) = tokio::join!(
        c.cancel_unstarted_reclaim_upload(&reservation, 100, &request),
        observer.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
    );
    assert_eq!(first.unwrap(), PublishOutcome::RequestCancelled);
    assert_eq!(second.unwrap(), PublishOutcome::RequestCancelled);
    assert_eq!(
        c.maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending,
        pending_before
    );
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&pin, false).await.unwrap().is_none());
    let original_receipt = tr.get(&receipt_key, false).await.unwrap().unwrap().to_vec();
    for key in [&receipt_key, &op] {
        let tr = c.db.create_trx().unwrap();
        let previous = tr.get(key, false).await.unwrap().unwrap().to_vec();
        tr.set(key, b"fixture-conflicting-cancel-terminal");
        tr.commit().await.unwrap();
        assert!(
            c.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
                .await
                .is_err()
        );
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            pending_before
        );
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(key, false).await.unwrap().unwrap().as_ref(),
            b"fixture-conflicting-cancel-terminal"
        );
        tr.set(key, &previous);
        tr.commit().await.unwrap();
    }
    // A different later batch must not make the original cancellation unknown.
    let later_batch = active_batch::batch_key(saved.volume_id, saved.volume_generation);
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&later_batch, false).await.unwrap().is_none());
    tr.set(&later_batch, b"fixture-later-other-batch");
    tr.commit().await.unwrap();
    assert_eq!(
        observer
            .cancel_unstarted_reclaim_upload(&reservation, 100, &request)
            .await
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&later_batch, false).await.unwrap().unwrap().as_ref(),
        b"fixture-later-other-batch"
    );
    tr.clear(&later_batch);
    tr.commit().await.unwrap();
    assert_eq!(
        c.active_upload_result(&request, route).await.unwrap(),
        Some(PublishOutcome::RequestCancelled)
    );
    assert_eq!(
        c.reserve_active_file(&saved).await.unwrap(),
        PublishOutcome::RequestCancelled
    );
    assert!(matches!(
        c.release_reserved_reclaim(&reservation, 100).await,
        Err(CatalogError::PoolBusy)
    ));
    assert!(matches!(
        c.advance_active_request_floor(
            request.namespace,
            &serial,
            100,
            request.request_epoch,
            request.request_epoch + 1
        )
        .await
        .unwrap(),
        WindowOutcome::Advanced(_)
    ));
    while c
        .purge_active_expired_requests(request.namespace, &serial, 100, 16)
        .await
        .unwrap()
        != 0
    {}
    let tr = c.db.create_trx().unwrap();
    assert!(tr.get(&op, false).await.unwrap().is_none());
    assert_eq!(
        c.reclaim_uploads_page(&reservation, 100, None, 1)
            .await
            .unwrap()
            .items,
        vec![request.clone()],
        "P已GC后，父恢复不能只扫描Q或P"
    );
    let original_retry = c
        .prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
        .await
        .unwrap();
    assert_eq!(
        original_retry.request(),
        request,
        "取消/GC/代次轮换不能创建另一份child"
    );
    assert_eq!(
        c.reserve_active_file(&original_retry).await.unwrap(),
        PublishOutcome::RequestExpired
    );
    assert_eq!(
        c.maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending,
        pending_before
    );
    assert_eq!(
        observer
            .cancel_unstarted_reclaim_upload(&reservation, 100, &request)
            .await
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    assert_eq!(
        c.maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending,
        pending_before
    );
    c.claim_active_owner_epoch(&serial, 101).await.unwrap();
    assert!(
        c.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
            .await
            .is_err()
    );
    assert!(
        c.cancel_unstarted_reclaim_upload(&reservation, 101, &request)
            .await
            .is_err()
    );
    let claimed = c
        .claim_reclaim_reservations_page(&serial, route, 101, None, 16)
        .await
        .unwrap();
    assert_eq!(claimed.items.len(), 1);
    assert_eq!(
        observer
            .cancel_unstarted_reclaim_upload(&claimed.items[0], 101, &request)
            .await
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    assert_eq!(
        c.maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending,
        pending_before
    );
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&receipt_key, false).await.unwrap().unwrap().as_ref(),
        original_receipt
    );
    assert_eq!(
        c.resolve_active_path_for_route(&serial, &pool_catalog_path(pool.id, path).unwrap(), route)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert!(matches!(
        c.check_active_pool_gate(&serial, route, 101, pool.id).await,
        Err(CatalogError::PoolBusy)
    ));
    drop(captured_source);
    std::fs::remove_dir_all(root).unwrap();
}
