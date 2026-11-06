use super::*;

#[path = "file_deletion_tests.rs"]
mod uploads;

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 Sim 删除、历史读取、原子终态与同名重建，不接触硬件"]
async fn nodes_delete_atomically_and_old_results_do_not_touch_recreated_names() {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(true, true, false, true))
        .await
        .unwrap();
    let key = ReservationKey::new(3, fixture.owner);
    fence(&fixture.device, key).unwrap();
    let mut volume = LtfsVolume::mount(&fixture.device).unwrap();
    volume.set_reservation_guard(Some(key));
    let mut scope = Scope {
        catalog: &fixture.catalog,
        serial: &fixture.serial,
        route: fixture.route,
        owner: fixture.owner,
        pool: fixture.pool,
        scan: fixture.scan,
    };
    check_deletions(&mut scope, &mut volume, &fixture.device, &fixture.root).await;
    check_deleted_ee_source(&volume, &fixture.device, &fixture.root);
    Box::pin(check_native_import(&scope, &fixture.root, None, "OWND01L8")).await;
    drop(volume);
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

async fn publish(
    scope: &Scope<'_>,
    request: &NodeMutationRequest,
    plan: &NodeMutationPlan,
    attempt: &NodeMutationAttempt,
) -> NodeMutationDeletion {
    scope
        .catalog
        .publish_node_mutation_deletion(scope.route, scope.owner, attempt, plan, request, 4 << 20)
        .await
        .unwrap()
}

async fn takeover(
    scope: &mut Scope<'_>,
    volume: &mut LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
) {
    scope.owner += 1;
    scope
        .catalog
        .claim_active_owner_epoch(scope.serial, scope.owner)
        .await
        .unwrap();
    let key = ReservationKey::new(u8::try_from(scope.owner % 3 + 1).unwrap(), scope.owner);
    fence(device, key).unwrap();
    volume.set_reservation_guard(Some(key));
}

pub(super) async fn check_deletions(
    scope: &mut Scope<'_>,
    volume: &mut LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
) {
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    let mut created = Vec::new();
    for (offset, path) in ["/body", "/link", "/alias"].into_iter().enumerate() {
        let full = pool_catalog_path(scope.pool, path.as_bytes()).unwrap();
        let Some(ResolvedPath::File {
            id,
            head,
            copy,
            attributes,
            ..
        }) = catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("real imported source")
        };
        let attributes = attributes.unwrap();
        assert_eq!(head.version, 3);
        let old_read = if copy.media.as_ref().unwrap().native_symlink {
            None
        } else {
            Some(
                catalog
                    .resolve_active_read_source_for_owner(
                        scope.serial,
                        &full,
                        scope.route,
                        scope.owner,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .plan_read(&root.join("cache.sqlite"))
                    .unwrap(),
            )
        };
        let old_copy_key = copy_key(namespace, id, head.copy_id);
        let old_reverse = crate::managed_catalog::volume_key(
            copy.volume_id,
            copy.volume_scan_id,
            namespace,
            id,
            head.copy_id,
        );
        let tr = catalog.db.create_trx().unwrap();
        let old_copy = tr
            .get(&old_copy_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let old_reverse_bytes = tr.get(&old_reverse, false).await.unwrap().unwrap().to_vec();
        let old_head = tr
            .get(&head_key(namespace, id), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let spec = scope.spec(path, Some(3), NodeMutation::Unlink);
        let (request, plan, attempt) = scope
            .admit(volume, &spec, "2026-11-01T00:02:00.000000000Z")
            .await;
        assert_eq!(request.context().request_epoch, 9);
        let old_owner = scope.owner;
        if offset == 0 {
            takeover(scope, volume, device).await;
        }
        volume.stage_node_mutation(&plan).unwrap();
        volume.sync_batch().unwrap();
        assert!(matches!(
            catalog
                .publish_node_mutation_deletion(
                    scope.route,
                    scope.owner,
                    &attempt,
                    &plan,
                    &request,
                    4 << 20
                )
                .await,
            Err(CatalogError::Incomplete)
        ));
        if offset == 1 {
            takeover(scope, volume, device).await;
        }
        let evidence = volume.node_mutation_evidence(&plan).unwrap();
        assert_eq!(evidence.file_count(), 3);
        if offset < 2 {
            assert!(matches!(
                catalog
                    .publish_node_mutation_deletion(
                        scope.route,
                        old_owner,
                        &attempt,
                        &plan,
                        &request,
                        4 << 20
                    )
                    .await,
                Err(CatalogError::OwnerFenced)
            ));
        }
        catalog
            .observe_node_mutation_attempt(scope.route, scope.owner, &attempt, &plan, &evidence)
            .await
            .unwrap();
        let child = child_key(namespace, plan.context().parent, &path.as_bytes()[1..]);
        let node = node_key(namespace, id);
        let parent_key = node_key(namespace, plan.context().parent);
        let tr = catalog.db.create_trx().unwrap();
        let parent_raw = tr.get(&parent_key, false).await.unwrap().unwrap().to_vec();
        let node_raw = tr.get(&node, false).await.unwrap().unwrap().to_vec();
        let mut bad_parent = decode_node(&parent_raw).unwrap();
        bad_parent.directory_version = Some(99);
        for (fault, bad) in [
            (parent_key.clone(), encode_node(&bad_parent)),
            (head_key(namespace, id), vec![0]),
            (node.clone(), vec![0]),
        ] {
            let tr = catalog.db.create_trx().unwrap();
            let original = tr.get(&fault, false).await.unwrap().unwrap().to_vec();
            tr.set(&fault, &bad);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .publish_node_mutation_deletion(
                        scope.route,
                        scope.owner,
                        &attempt,
                        &plan,
                        &request,
                        4 << 20
                    )
                    .await
                    .is_err()
            );
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(tr.get(&fault, false).await.unwrap().unwrap().as_ref(), bad);
            assert_eq!(
                tr.get(&child, false).await.unwrap().unwrap().as_ref(),
                id.as_bytes()
            );
            if fault != node {
                assert_eq!(
                    tr.get(&node, false).await.unwrap().unwrap().as_ref(),
                    node_raw
                );
            }
            assert_eq!(
                catalog
                    .node_mutation_task(scope.serial, scope.route, 9, spec.operation)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                NodeMutationTaskState::Writing
            );
            assert!(
                catalog
                    .pending_node_mutation_attempt(
                        scope.serial,
                        scope.route,
                        scope.owner,
                        evidence.current().volume_uuid
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
            tr.set(&fault, &original);
            tr.commit().await.unwrap();
        }
        let commands = device.command_log().len();
        let result = publish(scope, &request, &plan, &attempt).await;
        assert_eq!(device.command_log().len(), commands);
        if offset == 0 {
            let mut output = Vec::new();
            assert!(
                !catalog
                    .node_attributes_to(namespace, id, &mut output)
                    .await
                    .unwrap()
            );
            assert!(output.is_empty());
            Box::pin(check_retained_result(
                scope, &request, &plan, &attempt, &result,
            ))
            .await;
        }
        assert_eq!(
            (result.version(), result.parent_version()),
            (4, plan.context().parent_version + 1)
        );
        assert_eq!(result.previous_attributes_digest(), attributes.digest());
        assert_eq!(
            result.previous_copy().unwrap(),
            (head.copy_id, copy.as_ref())
        );
        assert_eq!(result.current(), evidence.current());
        assert!(
            catalog
                .resolve_active_path_for_route(scope.serial, &full, scope.route)
                .await
                .unwrap()
                .is_none()
        );
        assert!(catalog.node(namespace, id).await.unwrap().is_none());
        assert!(catalog.file_head(namespace, id).await.unwrap().is_none());
        assert_eq!(
            catalog
                .physical_copy(namespace, id, head.copy_id)
                .await
                .unwrap()
                .as_ref(),
            Some(copy.as_ref())
        );
        assert!(
            catalog
                .node_mutation_publication(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    9,
                    spec.operation
                )
                .await
                .unwrap()
                .is_none()
        );
        let tr = catalog.db.create_trx().unwrap();
        let raw = tr.get(&node, false).await.unwrap().unwrap();
        let retired = retired_nodes::from_node_record(namespace, id, &raw)
            .unwrap()
            .unwrap();
        let task = catalog
            .node_mutation_task(scope.serial, scope.route, 9, spec.operation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retired.result_digest, task.publication_digest.unwrap());
        assert_eq!(retired.previous_head, Some(head.clone()));
        assert_eq!(retired.management_epoch, 9);
        assert_eq!(
            tr.get(&head_key(namespace, id), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            old_head
        );
        assert_eq!(
            tr.get(&old_copy_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            old_copy
        );
        assert_eq!(
            tr.get(&old_reverse, false).await.unwrap().unwrap().as_ref(),
            old_reverse_bytes
        );
        assert!(
            retired_nodes::require_native_path_allowed(&tr, namespace, scope.pool, path)
                .await
                .is_err()
        );
        assert!(
            retired_nodes::require_native_path_allowed(
                &tr,
                namespace,
                scope.pool,
                &format!("{path}/old-child")
            )
            .await
            .is_err()
        );
        require_pool_writable_at(&tr, namespace, scope.pool)
            .await
            .unwrap();
        if offset == 0 {
            Box::pin(check_native_import(scope, root, Some("/body"), "SDEL01L8")).await;
        }
        if let Some(read) = old_read {
            let mut bytes = Vec::new();
            crate::ltfs::volume::read_cached_file_to_writer(
                device,
                volume.label(),
                &read.read.physical,
                &mut bytes,
            )
            .unwrap();
            assert_eq!(bytes, b"immutable original body");
        }
        // A corrupt live directory edge must not make the retired inode live.
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&child, id.as_bytes());
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .resolve_active_path_for_route(scope.serial, &full, scope.route)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&child, false).await.unwrap().unwrap().as_ref(),
            id.as_bytes()
        );
        tr.clear(&child);
        tr.commit().await.unwrap();
        let recreate = scope.spec(
            path,
            None,
            NodeMutation::CreateSymlink {
                target: ".LTFSEE_DATA/missing".into(),
            },
        );
        let (new_request, new_plan, new_attempt) = scope
            .admit(volume, &recreate, "2026-11-01T00:02:01.000000000Z")
            .await;
        assert_ne!(new_plan.context().node, id);
        assert_eq!(publish(scope, &request, &plan, &attempt).await, result);
        assert_eq!(
            catalog
                .pending_node_mutation_attempt(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    evidence.current().volume_uuid
                )
                .await
                .unwrap()
                .unwrap()
                .id(),
            new_attempt.id()
        );
        scope.commit(volume, &new_plan, &new_attempt).await;
        let new_result = scope.publish(&new_request, &new_plan, &new_attempt).await;
        assert_eq!(new_result.version(), 1);
        assert_eq!(publish(scope, &request, &plan, &attempt).await, result);
        let Some(ResolvedPath::File {
            id: new_id,
            head: new_head,
            ..
        }) = catalog
            .resolve_active_path_for_route(scope.serial, &full, scope.route)
            .await
            .unwrap()
        else {
            panic!("explicit recreated link")
        };
        assert_eq!((new_id, new_head.version), (new_result.node(), 1));
        assert_ne!(new_id, id);
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            retired_nodes::require_native_path_allowed(&tr, namespace, scope.pool, path)
                .await
                .is_err()
        );
        assert!(
            catalog
                .node_mutation_deletion(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    9,
                    recreate.operation
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog.request_window(namespace).await.unwrap().accepting,
            1
        );
        created.push((new_request, new_plan, new_attempt, new_result));
    }
    // Retiring a recreated NodeLink updates the path barrier but never forgets
    // the older inode's retirement or lets a successful create retry reattach it.
    let (created_request, created_plan, created_attempt, created_result) = &created[0];
    let spec = scope.spec("/body", Some(1), NodeMutation::Unlink);
    let (request, plan, attempt) = scope
        .admit(volume, &spec, "2026-11-01T00:02:02.000000000Z")
        .await;
    scope.commit(volume, &plan, &attempt).await;
    let deleted = publish(scope, &request, &plan, &attempt).await;
    assert_eq!(
        deleted.previous_copy().unwrap().1,
        created_result.physical_copy().unwrap().1
    );
    assert_eq!(
        scope
            .publish(created_request, created_plan, created_attempt)
            .await,
        *created_result
    );
    assert!(
        catalog
            .resolve_active_path_for_route(
                scope.serial,
                &pool_catalog_path(scope.pool, b"/body").unwrap(),
                scope.route
            )
            .await
            .unwrap()
            .is_none()
    );
    check_directories(scope, volume, device, root).await;
}

async fn check_directories(
    scope: &mut Scope<'_>,
    volume: &mut LtfsVolume<'_>,
    device: &crate::scsi::sim::SimTransport,
    root: &std::path::Path,
) {
    let catalog = scope.catalog;
    let namespace = scope.route.route.namespace;
    for path in ["/empty", "/empty/child"] {
        let spec = scope.spec(path, None, NodeMutation::CreateDirectory);
        let (request, plan, attempt) = scope
            .admit(volume, &spec, "2026-11-01T00:02:03.000000000Z")
            .await;
        scope.commit(volume, &plan, &attempt).await;
        scope.publish(&request, &plan, &attempt).await;
    }
    let nonempty = scope.spec("/empty", Some(2), NodeMutation::RemoveDirectory);
    let commands = device.command_log().len();
    assert!(
        catalog
            .prepare_node_mutation_request(
                scope.serial,
                scope.route,
                scope.owner,
                &nonempty,
                "2026-11-01T00:02:04.000000000Z"
            )
            .await
            .is_err()
    );
    assert_eq!(device.command_log().len(), commands);
    let mut parent_result = None;
    for (path, version) in [("/empty/child", 1), ("/empty", 3)] {
        let spec = scope.spec(path, Some(version), NodeMutation::RemoveDirectory);
        let (request, plan, attempt) = scope
            .admit(volume, &spec, "2026-11-01T00:02:05.000000000Z")
            .await;
        scope.commit(volume, &plan, &attempt).await;
        let commands = device.command_log().len();
        let result = publish(scope, &request, &plan, &attempt).await;
        assert_eq!(device.command_log().len(), commands);
        assert_eq!(result.version(), version + 1);
        assert!(result.previous_copy().is_none());
        assert_eq!(result.kind(), NodeKind::Directory);
        assert!(
            catalog
                .node(namespace, result.node())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .list_children(namespace, result.node(), None, 1000)
                .await
                .is_err()
        );
        parent_result = Some((request, plan, attempt, result));
    }
    let (request, plan, attempt, deleted) = parent_result.unwrap();
    let recreate = scope.spec("/empty", None, NodeMutation::CreateDirectory);
    let (new_request, new_plan, new_attempt) = scope
        .admit(volume, &recreate, "2026-11-01T00:02:06.000000000Z")
        .await;
    scope.commit(volume, &new_plan, &new_attempt).await;
    let recreated = scope.publish(&new_request, &new_plan, &new_attempt).await;
    assert_ne!(recreated.node(), deleted.node());
    assert_eq!(publish(scope, &request, &plan, &attempt).await, deleted);
    let tr = catalog.db.create_trx().unwrap();
    assert!(
        retired_nodes::require_native_path_allowed(
            &tr,
            namespace,
            scope.pool,
            "/empty/never-known"
        )
        .await
        .is_err()
    );
    assert_eq!(
        tr.get(
            &child_key(namespace, new_plan.context().parent, b"empty"),
            false
        )
        .await
        .unwrap()
        .unwrap()
        .as_ref(),
        recreated.node().as_bytes()
    );
    Box::pin(check_native_import(
        scope,
        root,
        Some("/empty/old-child"),
        "SDIR01L8",
    ))
    .await;
}

async fn check_retained_result(
    scope: &Scope<'_>,
    request: &NodeMutationRequest,
    plan: &NodeMutationPlan,
    attempt: &NodeMutationAttempt,
    result: &NodeMutationDeletion,
) {
    let catalog = scope.catalog;
    let namespace = attempt.namespace;
    let mut result_key = management::prefix(namespace, b'Q');
    result_key.push(b'd');
    append_component(&mut result_key, scope.serial.as_bytes());
    result_key.extend_from_slice(&attempt.epoch.to_be_bytes());
    result_key.extend_from_slice(attempt.operation.as_bytes());
    let tr = catalog.db.create_trx().unwrap();
    let original = tr.get(&result_key, false).await.unwrap().unwrap().to_vec();
    let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
    changed["parent_version"] = json!(result.parent_version() + 1);
    let changed = node_mutation::canonical(&changed).unwrap();
    for replacement in [None, Some(changed)] {
        let tr = catalog.db.create_trx().unwrap();
        match &replacement {
            None => tr.clear(&result_key),
            Some(bytes) => tr.set(&result_key, bytes),
        }
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .node_mutation_deletion(
                    scope.serial,
                    scope.route,
                    scope.owner,
                    attempt.epoch,
                    attempt.operation
                )
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog
                .publish_node_mutation_deletion(
                    scope.route,
                    scope.owner,
                    attempt,
                    plan,
                    request,
                    4 << 20
                )
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&result_key, false)
                .await
                .unwrap()
                .map(|v| v.to_vec()),
            replacement
        );
        tr.set(&result_key, &original);
        tr.commit().await.unwrap();
    }
    // Both records are individually valid; their coexistence is corruption,
    // even when the task digest still matches the original deletion.
    let task = catalog
        .node_mutation_task(scope.serial, scope.route, attempt.epoch, attempt.operation)
        .await
        .unwrap()
        .unwrap();
    let extra = NodeMutationPublication {
        attempt: attempt.clone(),
        current: result.current(),
        node: result.node(),
        version: result.version(),
        attributes: result.previous_attributes_digest(),
        parent_version: result.parent_version(),
        parent_attributes: [1; 32],
        copy: projected_copy(attempt, result.node(), result.version(), result.current()).unwrap(),
        owner: result.completed_owner(),
        completed_at_millis: task.completed_at_millis.unwrap(),
    };
    let extra_bytes = encode(&extra).unwrap();
    assert_eq!(decode(&extra_bytes).unwrap(), extra);
    let extra_key = key(namespace, scope.serial, attempt.epoch, attempt.operation);
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&extra_key, &extra_bytes);
    tr.commit().await.unwrap();
    assert!(matches!(
        catalog
            .node_mutation_task(scope.serial, scope.route, attempt.epoch, attempt.operation)
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&extra_key, false).await.unwrap().unwrap().as_ref(),
        extra_bytes
    );
    tr.clear(&extra_key);
    // Historical results remain usable after attempt pruning and temporary
    // loss of serving media; they do not authorize reading current content.
    tr.clear(&attempt_key(attempt));
    let ready_key = ready_volume_key(result.current().volume_uuid);
    let ready = tr.get(&ready_key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&ready_key);
    tr.commit().await.unwrap();
    assert_eq!(
        catalog
            .node_mutation_deletion(
                scope.serial,
                scope.route,
                scope.owner,
                attempt.epoch,
                attempt.operation
            )
            .await
            .unwrap()
            .as_ref(),
        Some(result)
    );
    assert_eq!(publish(scope, request, plan, attempt).await, *result);
    let tr = catalog.db.create_trx().unwrap();
    assert!(
        tr.get(&attempt_key(attempt), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(tr.get(&ready_key, false).await.unwrap().is_none());
    tr.set(&ready_key, &ready);
    tr.commit().await.unwrap();
}

// Build a separate, genuine committed medium: no fabricated generation,
// index digest, source receipt, or evidence from the already modified tape.
// None verifies an explicitly owned EE-only medium with zero user paths.
async fn check_native_import(
    scope: &Scope<'_>,
    root: &std::path::Path,
    path: Option<&str>,
    barcode: &str,
) {
    let library = SimLibrary::new(1, 4, 1);
    library
        .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
        .unwrap();
    library.load_into_drive(barcode, 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: barcode[..6].into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let key = ReservationKey::new(1, scope.owner);
    fence(&device, key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    volume
        .append_file(
            path.unwrap_or("/.LTFSEE_DATA/data"),
            &mut std::io::Cursor::new(b"obsolete imported body"),
        )
        .unwrap();
    if path.is_none() {
        // External-media fixture declaration; the separate node test proves
        // this marker is actually committed by the unlink plan and its S4.
        volume
            .set_node_xattr(
                "/.LTFSEE_DATA",
                Xattr {
                    key: "tapers.fdb.eeInternalDirectory".into(),
                    value: "1".into(),
                    base64: false,
                },
                true,
                false,
            )
            .unwrap();
    }
    volume.sync().unwrap();
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
    let spool_path = root.join(format!("stale-{barcode}.sqlite"));
    let cache_path = root.join(format!("stale-{barcode}-cache.sqlite"));
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
    let namespace = scope.route.route.namespace;
    let request = ManagementRequest::parse(json!({
        "version":1,"library":scope.serial,"namespace":namespace.to_string(),
        "epoch":9,"operation":Uuid::new_v4().to_string(),
        "change":{"kind":"media-import","pool":scope.pool.to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}
    })).unwrap();
    let catalog = scope.catalog;
    catalog
        .admit_management(scope.serial, scope.route, scope.owner, &request)
        .await
        .unwrap();
    let intake = catalog
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
    catalog
        .bind_native_media_source(&intake, scope.owner, &source)
        .await
        .unwrap();
    let Some(path) = path else {
        assert_eq!((source.files, source.paths), (1, 0));
        assert!(
            spool
                .candidate_page(None, 1000)
                .unwrap()
                .candidates
                .is_empty()
        );
        catalog
            .begin_native_media_projection(&intake, scope.owner)
            .await
            .unwrap();
        catalog
            .finish_native_media(&intake, scope.owner, &source)
            .await
            .unwrap();
        assert!(
            catalog
                .resolve_active_path_for_route(
                    scope.serial,
                    &pool_catalog_path(scope.pool, b"/.LTFSEE_DATA").unwrap(),
                    scope.route
                )
                .await
                .unwrap()
                .is_none()
        );
        let tr = catalog.db.create_trx().unwrap();
        require_pool_writable_at(&tr, namespace, scope.pool)
            .await
            .unwrap();
        return;
    };
    let candidate = spool
        .candidate_page(None, 1000)
        .unwrap()
        .candidates
        .into_iter()
        .find(|candidate| candidate.logical_path == path)
        .unwrap();
    let entry = intake.entry(&spool, candidate).unwrap();
    let before = catalog
        .native_media_status(&intake, scope.owner)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .project_native_media_entry(&intake, scope.owner, &entry, false)
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(
        catalog
            .native_media_status(&intake, scope.owner)
            .await
            .unwrap(),
        before
    );
    catalog
        .begin_native_media_projection(&intake, scope.owner)
        .await
        .unwrap();
    let before = catalog
        .native_media_status(&intake, scope.owner)
        .await
        .unwrap();
    assert!(matches!(
        catalog
            .project_native_media_entry(&intake, scope.owner, &entry, true)
            .await,
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(
        catalog
            .native_media_status(&intake, scope.owner)
            .await
            .unwrap(),
        before
    );
    catalog
        .abort_native_media(
            &intake,
            scope.owner,
            Some("deleted path must remain absent"),
        )
        .await
        .unwrap();
    while !catalog
        .rollback_native_media_page(&intake, scope.owner)
        .await
        .unwrap()
    {}
    assert!(
        catalog
            .resolve_active_path_for_route(
                scope.serial,
                &pool_catalog_path(scope.pool, path.as_bytes()).unwrap(),
                scope.route
            )
            .await
            .unwrap()
            .is_none()
    );
    let tr = catalog.db.create_trx().unwrap();
    require_pool_writable_at(&tr, namespace, scope.pool)
        .await
        .unwrap();
}

// Verify the real committed Full/delta source after the final EE alias was
// deleted. A serialization of the replayed tree is not a physical index proof.
fn check_deleted_ee_source(
    volume: &LtfsVolume<'_>,
    device: &dyn crate::scsi::transport::TapeTransport,
    root: &std::path::Path,
) {
    use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
    use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
    use crate::tape::commands::TapeDrive;
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.into();
    budget.index_scratch.max_bytes = 64 << 20;
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_index_source(&TapeDrive::new(device), volume.label(), &budget).unwrap();
    assert_eq!(captured.construct(), volume.committed_index_construct());
    let storage = ReadStorageBudget {
        cache_bytes: 128 << 20,
        headroom_bytes: 0,
        ..Default::default()
    };
    let spool_path = root.join("deleted-ee-spool.sqlite");
    let cache_path = root.join("deleted-ee-cache.sqlite");
    ImportSpool::build_captured(
        &captured,
        &spool_path,
        &mut CacheBuild::new(root, storage).unwrap(),
    )
    .unwrap();
    VolumeExtentCache::build_captured(
        &captured,
        &cache_path,
        &mut CacheBuild::new(root, storage).unwrap(),
    )
    .unwrap();
    let identity = captured.identity().unwrap();
    let spool = ImportSpool::open(
        &spool_path,
        crate::managed_catalog::index_evidence::expected_index(&identity),
    )
    .unwrap();
    let cache = VolumeExtentCache::open_captured(&cache_path, &identity).unwrap();
    spool.verify_extent_cache(&cache).unwrap();
    let candidates = spool.candidate_page(None, 1000).unwrap().candidates;
    assert!(
        candidates
            .iter()
            .all(|c| !c.logical_path.starts_with("/.LTFSEE_DATA"))
    );
    assert!(spool.node_by_path(".LTFSEE_DATA/data").unwrap().is_some());
    assert!(!candidates.iter().any(|c| c.logical_path == "/body"));
    for path in ["/link", "/alias"] {
        let candidate = candidates.iter().find(|c| c.logical_path == path).unwrap();
        let (attributes, _, data) =
            crate::managed_catalog::staged_import::source_evidence(&spool, candidate).unwrap();
        assert_eq!(
            attributes.link_kind().unwrap(),
            crate::managed_catalog::link_kind::LinkKind::Native
        );
        assert!(matches!(
            data,
            crate::managed_catalog::StagedSourceData::NativeSymlink
        ));
    }
}
