//! Logical node preflight against actual imported LTFS files and directories.
use super::*;
use tape_rs::managed_catalog::{
    ActiveIndexIdentity, ActiveUploadSpec, CatalogError, CommitReceipt, ContentDigest,
    NodeAttributes, NodeMutation, NodeMutationContext, NodeMutationPlan, NodeMutationSpec,
    NodeMutationTaskState, PublishOutcome, ResolvedPath, XattrMode, derive_copy_id,
};
use tape_rs::scsi::reservation::{ReservationKey, fence};

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实模拟介质接入后的节点预检，不操作真实设备"]
fn node_preflight_preserves_imported_attributes_and_current_copy_without_media_io() {
    node_preflight(false);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实模拟介质的节点 Writing 原子准入与接任，不启动物理节点写入"]
fn node_writing_attempt_is_durable_fenced_and_blocks_generic_tail_recovery() {
    node_preflight(true);
}

fn node_preflight(writing: bool) {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let barcode = "BASE01L8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    let drive = f.library.drive(0);
    mkltfs(
        &drive,
        &MkltfsOptions {
            volume_id: "BAS001".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let fixture_key = ReservationKey::new(1, 99);
    fence(&drive, fixture_key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(fixture_key));
    volume.create_directory("/empty").unwrap();
    volume.create_directory("/parent").unwrap();
    volume
        .append_file("/parent/file", &mut std::io::Cursor::new(b"preserved body"))
        .unwrap();
    volume.add_symlink("/parent/link", "file").unwrap();
    volume
        .set_node_xattr(
            "/parent",
            tape_rs::ltfs::index::Xattr {
                key: "unknown.parent".into(),
                value: "opaque".repeat(40_000),
                base64: false,
            },
            false,
            false,
        )
        .unwrap();
    volume.commit().unwrap();
    let original =
        NodeAttributes::from_directory(volume.index().find_directory("/parent").unwrap()).unwrap();
    let physical_index = volume.index().clone();
    let physical_head =
        ActiveIndexIdentity::from(volume.verified_committed_index_construct().unwrap());
    drop(volume);
    fixture_return_to_storage(&f.library, 0);
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(15));
    let ns = f.route.route.namespace;
    for (pool, name) in [(f.pools[0], "nodes"), (f.pools[1], "other")] {
        client
            .create_pool(&f.serial, ns, pool, name, 200_000)
            .unwrap();
    }
    let import = client.prepare_management(&f.serial, ns,
        json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true})).unwrap();
    let saved = f.root.join("import.json");
    import.save_new(&saved).unwrap();
    let result = client.run_management_saved(&saved).unwrap();
    assert_eq!(result["result"]["source"]["files"], 2);
    // This scene inspects and deliberately corrupts intermediate records.
    // Stop the autonomous consumer after intake; runtime execution and healthy
    // HTTP backpressure are covered by owner_scheduling::nodes.
    owner.permission.close(100);
    let start = owner.trace.lock().unwrap().len();
    let create = NodeMutationSpec {
        pool: f.pools[0],
        operation: Uuid::new_v4(),
        path: "/parent/new".into(),
        expected_version: None,
        mutation: NodeMutation::CreateDirectory,
    };
    let baseline = rt
        .block_on(
            f.catalog
                .prepare_node_mutation_baseline(&f.serial, f.route, 100, &create),
        )
        .unwrap();
    assert!(baseline.current.is_none());
    assert_eq!(baseline.parent.version, 1);
    let reference = baseline.parent.attributes.as_ref().unwrap();
    assert!(
        reference.meta().is_none(),
        "大属性应保持不可变引用，不在预检事务内拼装"
    );
    assert_eq!(reference.digest(), original.digest());
    let path = tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/parent").unwrap();
    let Some(ResolvedPath::Directory { attributes, .. }) = rt
        .block_on(
            f.catalog
                .resolve_active_path_for_route(&f.serial, &path, f.route),
        )
        .unwrap()
    else {
        panic!("parent directory");
    };
    assert_eq!(attributes, Some(original));
    for (path, symlink) in [("/parent/file", false), ("/parent/link", true)] {
        let unlink = NodeMutationSpec {
            path: path.into(),
            expected_version: Some(1),
            mutation: NodeMutation::Unlink,
            ..create.clone()
        };
        let value = rt
            .block_on(
                f.catalog
                    .prepare_node_mutation_baseline(&f.serial, f.route, 100, &unlink),
            )
            .unwrap();
        let Some(ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            ..
        }) = value.current
        else {
            panic!("file or native symlink");
        };
        assert_eq!(value.node, id);
        assert_eq!(head.version, 1);
        assert_eq!(head.version, copy.version);
        assert_eq!(placement.pool_id, f.pools[0]);
        assert_eq!(placement.library_serial, f.serial);
        assert_eq!(copy.media.as_ref().unwrap().native_symlink, symlink);
        assert_eq!(head.size, if symlink { 0 } else { 14 });
        assert!(
            rt.block_on(f.catalog.prepare_node_mutation_baseline(
                &f.serial,
                f.route,
                100,
                &NodeMutationSpec {
                    expected_version: Some(2),
                    ..unlink.clone()
                }
            ))
            .is_err()
        );
        assert!(
            rt.block_on(f.catalog.prepare_node_mutation_baseline(
                &f.serial,
                f.route,
                100,
                &NodeMutationSpec {
                    mutation: NodeMutation::RemoveDirectory,
                    ..unlink
                }
            ))
            .is_err()
        );
    }
    let rm = NodeMutationSpec {
        path: "/empty".into(),
        expected_version: Some(1),
        mutation: NodeMutation::RemoveDirectory,
        ..create.clone()
    };
    assert!(
        rt.block_on(
            f.catalog
                .prepare_node_mutation_baseline(&f.serial, f.route, 100, &rm)
        )
        .is_ok()
    );
    for bad in [
        NodeMutationSpec {
            path: "/parent".into(),
            ..rm.clone()
        },
        NodeMutationSpec {
            path: "/parent/file/new".into(),
            ..create.clone()
        },
        NodeMutationSpec {
            pool: f.pools[1],
            ..rm
        },
        NodeMutationSpec {
            path: "/parent/file".into(),
            ..create.clone()
        },
    ] {
        assert!(
            rt.block_on(
                f.catalog
                    .prepare_node_mutation_baseline(&f.serial, f.route, 100, &bad)
            )
            .is_err()
        );
    }
    let change = NodeMutationSpec {
        operation: Uuid::new_v4(),
        path: "/parent/file".into(),
        expected_version: Some(1),
        mutation: NodeMutation::SetXattr {
            attribute: tape_rs::ltfs::index::Xattr {
                key: "new.logical".into(),
                value: "x".repeat(200_000),
                base64: false,
            },
            mode: XattrMode::Create,
        },
        ..create.clone()
    };
    let request = rt
        .block_on(f.catalog.prepare_node_mutation_request(
            &f.serial,
            f.route,
            100,
            &change,
            "2026-10-31T08:20:00.000000000Z",
        ))
        .unwrap();
    let bytes = request.encode().unwrap();
    let object = rt
        .block_on(
            f.catalog
                .upload_node_mutation_request(f.route, 100, &request, 1 << 20),
        )
        .unwrap();
    assert!(object.chunks() > 3);
    assert_eq!(
        rt.block_on(
            f.catalog
                .load_node_mutation_request(f.route, 100, &object, 1 << 20)
        )
        .unwrap(),
        request
    );
    assert!(
        rt.block_on(
            f.catalog
                .load_node_mutation_plan(f.route, 100, &object, 1 << 20)
        )
        .is_err()
    );
    let checked = rt
        .block_on(
            f.catalog
                .check_node_mutation_request(f.route, 100, &request),
        )
        .unwrap();
    let queued = rt
        .block_on(
            f.catalog
                .queue_node_mutation_request(f.route, 100, &request, 1 << 20),
        )
        .unwrap();
    assert_eq!(queued.fingerprint, request.fingerprint());
    assert_eq!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 100, None, 100)
        )
        .unwrap()
        .items,
        vec![queued.clone()]
    );
    let upload = ActiveUploadSpec {
        pool_id: f.pools[0],
        path: b"/reserved-other".to_vec(),
        operation_id: Uuid::new_v4(),
        expected_version: None,
        size: 1,
        digest: ContentDigest::Sha256([7; 32]),
    };
    let reserved = rt
        .block_on(
            f.catalog
                .prepare_active_upload(&f.serial, f.route, 100, barcode, &upload),
        )
        .unwrap();
    assert_eq!(
        rt.block_on(f.catalog.reserve_active_file(&reserved))
            .unwrap(),
        PublishOutcome::Registered
    );
    assert!(matches!(
        rt.block_on(f.catalog.prepare_node_mutation_task(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            1 << 20
        )),
        Err(CatalogError::Incomplete)
    ));
    assert_eq!(
        rt.block_on(f.catalog.cancel_active_file_reservation(&reserved))
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    let fresh_upload = ActiveUploadSpec {
        operation_id: Uuid::new_v4(),
        ..upload
    };
    let blocked = rt
        .block_on(
            f.catalog
                .prepare_active_upload(&f.serial, f.route, 100, barcode, &fresh_upload),
        )
        .unwrap();
    assert!(matches!(
        rt.block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            1 << 20
        )),
        Err(CatalogError::Invalid(_))
    ));
    let prepared = rt
        .block_on(f.catalog.prepare_node_mutation_task(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            1 << 20,
        ))
        .unwrap();
    assert_eq!(prepared.state, NodeMutationTaskState::Prepared);
    assert_eq!(prepared.fingerprint, queued.fingerprint);
    assert!(matches!(
        rt.block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            bytes.len() + 4096
        )),
        Err(CatalogError::Invalid(_))
    ));
    let canonical = rt
        .block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            1 << 20,
        ))
        .unwrap();
    assert_eq!(canonical.task, prepared);
    assert_eq!(canonical.request.encode().unwrap(), bytes);
    assert_eq!(
        canonical
            .ancestors
            .iter()
            .map(|node| node.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/", "/parent"]
    );
    assert_eq!(canonical.ancestors[1].id, checked.parent.id);
    assert_eq!(canonical.ancestors[1].version, checked.parent.version);
    assert_eq!(
        canonical.ancestors[1].attributes.digest(),
        reference.digest()
    );
    assert!(
        canonical.ancestors[1]
            .attributes
            .xattrs()
            .unwrap()
            .iter()
            .any(|attr| attr.key == "unknown.parent" && attr.value == "opaque".repeat(40_000))
    );
    assert_eq!(
        canonical.baseline.parent.attributes,
        Some(canonical.ancestors[1].attributes.clone())
    );
    let pool_record = rt
        .block_on(
            f.catalog
                .active_pool_for_route(&f.serial, f.route, change.pool),
        )
        .unwrap()
        .unwrap();
    let root_meta = canonical.ancestors[0].attributes.meta().unwrap();
    assert_eq!(root_meta.file_uid, 1);
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&root_meta.creation_time)
            .unwrap()
            .timestamp_millis() as u64,
        pool_record.created_at_millis
    );
    let Some(ResolvedPath::File {
        attributes: Some(target),
        ..
    }) = &canonical.baseline.current
    else {
        panic!("canonical existing file")
    };
    assert!(target.meta().is_some());
    let Some(ResolvedPath::File { copy, .. }) = &checked.current else {
        panic!("actual imported file");
    };
    let plan = NodeMutationPlan::prepare(
        NodeMutationContext {
            library_serial: f.serial.clone(),
            namespace: ns,
            pool: change.pool,
            operation: change.operation,
            request_epoch: checked.request_epoch,
            owner_epoch: 100,
            parent: checked.parent.id,
            parent_version: checked.parent.version,
            node: checked.node,
            expected_version: Some(1),
            scan: copy.volume_scan_id,
            previous: physical_head,
            path: change.path.clone(),
        },
        change.mutation.clone(),
        &physical_index,
        request.time(),
        &canonical.ancestors,
        Some((tape_rs::managed_catalog::NodeKind::File, target)),
    )
    .unwrap();
    plan.validate_prepared_inputs(&canonical).unwrap();
    let mut changed_inputs = canonical.clone();
    changed_inputs.ancestors[1].version += 1;
    assert!(plan.validate_prepared_inputs(&changed_inputs).is_err());
    let plan_object = rt
        .block_on(
            f.catalog
                .upload_node_mutation_plan(f.route, 100, &plan, 1 << 20),
        )
        .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .load_node_mutation_request(f.route, 100, &plan_object, 1 << 20)
        )
        .is_err()
    );
    if writing {
        let attempt_id = Uuid::new_v4();
        let raw_db = foundationdb::Database::new(Some(&cluster)).unwrap();
        let mut node_prefix = b"tape-rs/catalog/22/@/".to_vec();
        node_prefix.extend_from_slice(ns.as_bytes());
        node_prefix.push(b'Q');
        let mut volume_gate = node_prefix.clone();
        volume_gate.push(b'v');
        volume_gate.extend_from_slice(physical_head.volume_uuid.as_bytes());
        let preflight = raw_db.create_trx().unwrap();
        assert!(
            rt.block_on(preflight.get(&volume_gate, false))
                .unwrap()
                .is_none()
        );
        let mut stale_context = plan.context().clone();
        stale_context.previous.sha256[0] ^= 1;
        let stale = NodeMutationPlan::prepare(
            stale_context,
            change.mutation.clone(),
            &physical_index,
            request.time(),
            &canonical.ancestors,
            Some((tape_rs::managed_catalog::NodeKind::File, target)),
        )
        .unwrap();
        assert!(matches!(
            rt.block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                100,
                attempt_id,
                &canonical,
                &stale,
                1 << 20
            )),
            Err(CatalogError::Invalid(_))
        ));
        assert_eq!(
            rt.block_on(
                f.catalog
                    .node_mutation_task(&f.serial, f.route, 1, change.operation)
            )
            .unwrap(),
            Some(prepared.clone())
        );
        assert!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                100,
                physical_head.volume_uuid
            ))
            .unwrap()
            .is_none()
        );
        assert_eq!(
            rt.block_on(
                f.catalog
                    .begin_active_volume_recovery(&f.serial, 100, physical_head)
            )
            .unwrap(),
            tape_rs::managed_catalog::ActiveRecoveryOutcome::Begun
        );
        assert!(matches!(
            rt.block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                100,
                attempt_id,
                &canonical,
                &plan,
                1 << 20
            )),
            Err(CatalogError::Incomplete)
        ));
        assert_eq!(
            rt.block_on(f.catalog.finish_active_volume_recovery(
                &f.serial,
                100,
                physical_head,
                None
            ))
            .unwrap(),
            tape_rs::managed_catalog::ActiveRecoveryOutcome::Cleared
        );
        let attempt = rt
            .block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                100,
                attempt_id,
                &canonical,
                &plan,
                1 << 20,
            ))
            .unwrap();
        assert_eq!(attempt.id(), attempt_id);
        let mut probe = volume_gate.clone();
        probe.push(b'x');
        preflight.set(&probe, b"must-not-commit");
        assert_eq!(rt.block_on(preflight.commit()).unwrap_err().code(), 1020);
        assert_eq!(attempt.original_owner(), 100);
        assert_eq!(attempt.plan_object(), &plan_object);
        assert_eq!(attempt.previous(), physical_head);
        assert_eq!(attempt.source_copy().unwrap().1, copy.as_ref());
        assert_eq!(
            rt.block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                100,
                attempt_id,
                &canonical,
                &plan,
                1 << 20
            ))
            .unwrap(),
            attempt
        );
        assert!(
            rt.block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                100,
                Uuid::new_v4(),
                &canonical,
                &plan,
                1 << 20
            ))
            .is_err()
        );
        let current = rt
            .block_on(
                f.catalog
                    .node_mutation_task(&f.serial, f.route, 1, change.operation),
            )
            .unwrap()
            .unwrap();
        assert_eq!(current.state, NodeMutationTaskState::Writing);
        assert_eq!(current.active_attempt, Some(attempt_id));
        assert_eq!(current.prepared_owner_epoch, Some(100));
        assert_eq!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                100,
                physical_head.volume_uuid
            ))
            .unwrap(),
            Some(attempt.clone())
        );
        assert!(
            rt.block_on(f.catalog.cancel_node_mutation_task(
                &f.serial,
                f.route,
                100,
                1,
                change.operation
            ))
            .is_err()
        );
        assert!(matches!(
            rt.block_on(
                f.catalog
                    .begin_active_volume_recovery(&f.serial, 100, physical_head)
            ),
            Err(CatalogError::PoolBusy)
        ));
        assert!(matches!(
            rt.block_on(f.catalog.finish_active_volume_recovery(
                &f.serial,
                100,
                physical_head,
                None
            )),
            Err(CatalogError::PoolBusy)
        ));
        assert!(matches!(
            rt.block_on(f.catalog.reserve_active_file(&blocked)),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, physical_head.volume_uuid)
            )
            .unwrap(),
            physical_head
        );
        rt.block_on(
            f.catalog
                .check_active_pool_gate(&f.serial, f.route, 100, f.pools[1]),
        )
        .unwrap();
        let mut attempt_key = node_prefix.clone();
        attempt_key.push(b'a');
        attempt_key.extend_from_slice(f.serial.as_bytes());
        attempt_key.extend_from_slice(&[0, 0]);
        attempt_key.extend_from_slice(&1u64.to_be_bytes());
        attempt_key.extend_from_slice(change.operation.as_bytes());
        attempt_key.extend_from_slice(attempt_id.as_bytes());
        let mut pool_gate = node_prefix.clone();
        pool_gate.push(b'h');
        pool_gate.extend_from_slice(change.pool.as_bytes());
        let mut task_key = node_prefix.clone();
        task_key.push(b'l');
        task_key.extend_from_slice(f.serial.as_bytes());
        task_key.extend_from_slice(&[0, 0, b'o']);
        task_key.extend_from_slice(&1u64.to_be_bytes());
        task_key.extend_from_slice(change.operation.as_bytes());
        for key in [&volume_gate, &attempt_key, &pool_gate, &task_key] {
            let tr = raw_db.create_trx().unwrap();
            let saved = rt.block_on(tr.get(key, false)).unwrap().unwrap().to_vec();
            tr.clear(key);
            rt.block_on(tr.commit()).unwrap();
            assert!(matches!(
                rt.block_on(f.catalog.pending_node_mutation_attempt(
                    &f.serial,
                    f.route,
                    100,
                    physical_head.volume_uuid
                )),
                Err(CatalogError::Corrupt(_))
            ));
            assert!(
                rt.block_on(
                    f.catalog
                        .begin_active_volume_recovery(&f.serial, 100, physical_head)
                )
                .is_err()
            );
            let tr = raw_db.create_trx().unwrap();
            assert!(
                rt.block_on(tr.get(key, false)).unwrap().is_none(),
                "failed validation must not repair missing records"
            );
            tr.set(key, &saved);
            rt.block_on(tr.commit()).unwrap();
        }
        lifecycle::no_media_changes(&owner, start);
        drop(server);
        drop(ctx);
        drop(owner);
        let owner = Owner::start(&f, 101, json!({}));
        let start = owner.trace.lock().unwrap().len();
        assert!(matches!(
            rt.block_on(f.catalog.node_mutation_attempt(
                &f.serial,
                f.route,
                100,
                1,
                change.operation
            )),
            Err(CatalogError::OwnerFenced)
        ));
        let restored = rt
            .block_on(
                f.catalog
                    .node_mutation_attempt(&f.serial, f.route, 101, 1, change.operation),
            )
            .unwrap()
            .unwrap();
        assert_eq!(restored, attempt);
        let recovered_plan = rt
            .block_on(
                f.catalog
                    .load_node_mutation_attempt_plan(f.route, 101, &restored, 1 << 20),
            )
            .unwrap();
        assert_eq!(recovered_plan.encode().unwrap(), plan.encode().unwrap());
        assert_eq!(recovered_plan.context().owner_epoch, 100);
        assert_eq!(
            rt.block_on(f.catalog.admit_node_mutation_attempt(
                f.route,
                101,
                attempt_id,
                &canonical,
                &plan,
                1 << 20
            ))
            .unwrap(),
            attempt
        );
        assert!(
            rt.block_on(f.catalog.cancel_node_mutation_task(
                &f.serial,
                f.route,
                101,
                1,
                change.operation
            ))
            .is_err()
        );
        assert!(matches!(
            rt.block_on(
                f.catalog
                    .begin_active_volume_recovery(&f.serial, 101, physical_head)
            ),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            rt.block_on(
                f.catalog
                    .pending_node_mutation_tasks(&f.serial, f.route, 101, None, 100)
            )
            .unwrap()
            .items,
            vec![current]
        );
        lifecycle::no_media_changes(&owner, start);
        drop(owner);
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    assert!(matches!(
        rt.block_on(f.catalog.reserve_active_file(&blocked)),
        Err(CatalogError::PoolBusy)
    ));
    // A known terminal is returned before testing the new-write barrier.
    assert_eq!(
        rt.block_on(f.catalog.reserve_active_file(&reserved))
            .unwrap(),
        PublishOutcome::RequestCancelled
    );
    let mut direct = CommitReceipt {
        namespace: ns,
        parent: blocked.parent,
        name: blocked.name.clone(),
        file_id: blocked.file_id,
        expected_version: blocked.expected_version,
        operation_id: blocked.operation_id,
        request_epoch: blocked.request_epoch,
        copy_id: Uuid::nil(),
        library_serial: f.serial.clone(),
        pool_id: blocked.pool_id,
        owner_epoch: 100,
        volume_id: blocked.volume_id,
        volume_generation: blocked.volume_generation,
        volume_scan_id: blocked.volume_scan_id,
        ltfs_file_uid: 999,
        size: blocked.expected_size,
        content_digest: blocked.expected_digest,
        metadata_digest: None,
    };
    direct.copy_id = derive_copy_id(&direct);
    assert!(matches!(
        rt.block_on(f.catalog.register_active_request(&direct)),
        Err(CatalogError::PoolBusy)
    ));
    assert!(matches!(
        rt.block_on(
            f.catalog
                .stage_active_batch(Uuid::new_v4(), std::slice::from_ref(&direct))
        ),
        Err(CatalogError::PoolBusy)
    ));
    assert!(
        rt.block_on(f.catalog.active_batch_status(
            &f.serial,
            blocked.volume_id,
            blocked.volume_generation
        ))
        .unwrap()
        .is_none()
    );
    assert!(matches!(
        rt.block_on(f.catalog.prepare_active_upload(
            &f.serial,
            f.route,
            100,
            barcode,
            &fresh_upload
        )),
        Err(CatalogError::PoolBusy)
    ));
    let visible = rt
        .block_on(
            f.catalog.resolve_active_path_for_route(
                &f.serial,
                &tape_rs::managed_catalog::pool_catalog_path(change.pool, change.path.as_bytes())
                    .unwrap(),
                f.route,
            ),
        )
        .unwrap();
    assert!(matches!(visible, Some(ResolvedPath::File { head, .. }) if head.version == 1));
    lifecycle::no_media_changes(&owner, start);
    assert!(!owner.permission.allows(100, Phase::Serving));
    drop(server);
    drop(ctx);
    drop(owner);
    let owner = Owner::start(&f, 101, json!({}));
    let start = owner.trace.lock().unwrap().len();
    assert!(matches!(
        rt.block_on(
            f.catalog
                .prepare_node_mutation_baseline(&f.serial, f.route, 100, &create)
        ),
        Err(CatalogError::OwnerFenced)
    ));
    let adopted = rt
        .block_on(
            f.catalog
                .prepare_node_mutation_baseline(&f.serial, f.route, 101, &create),
        )
        .unwrap();
    assert_eq!(adopted.node, baseline.node);
    assert_eq!(adopted.parent, baseline.parent);
    let loaded = rt
        .block_on(
            f.catalog
                .load_node_mutation_request(f.route, 101, &object, 1 << 20),
        )
        .unwrap();
    assert_eq!(loaded.encode().unwrap(), bytes);
    rt.block_on(f.catalog.check_node_mutation_request(f.route, 101, &loaded))
        .unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .load_node_mutation_plan(f.route, 101, &plan_object, 1 << 20)
        )
        .unwrap(),
        plan
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 101, None, 100)
        )
        .unwrap()
        .items,
        vec![prepared.clone()]
    );
    let (recovered, body) = rt
        .block_on(f.catalog.node_mutation_task_request(
            &f.serial,
            f.route,
            101,
            request.context().request_epoch,
            request.context().operation,
            1 << 20,
        ))
        .unwrap();
    assert_eq!(recovered, prepared);
    assert!(matches!(
        rt.block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            100,
            1,
            change.operation,
            1 << 20
        )),
        Err(CatalogError::OwnerFenced)
    ));
    let resumed_inputs = rt
        .block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            101,
            1,
            change.operation,
            1 << 20,
        ))
        .unwrap();
    assert_eq!(resumed_inputs.task, canonical.task);
    assert_eq!(resumed_inputs.request, canonical.request);
    assert_eq!(resumed_inputs.ancestors, canonical.ancestors);
    assert_eq!(resumed_inputs.baseline.owner_epoch, 101);
    assert!(
        plan.validate_prepared_inputs(&resumed_inputs).is_err(),
        "an inert plan for the old owner is not a fresh-attempt permit"
    );
    assert_eq!(body.encode().unwrap(), bytes);
    let cancelled = rt
        .block_on(f.catalog.cancel_node_mutation_task(
            &f.serial,
            f.route,
            101,
            request.context().request_epoch,
            request.context().operation,
        ))
        .unwrap();
    assert_eq!(
        cancelled.state,
        tape_rs::managed_catalog::NodeMutationTaskState::Cancelled
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .queue_node_mutation_request(f.route, 101, &request, 1 << 20)
        )
        .unwrap(),
        cancelled
    );
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 101, None, 100)
        )
        .unwrap()
        .items
        .is_empty()
    );
    assert!(matches!(
        rt.block_on(f.catalog.capture_prepared_node_inputs(
            &f.serial,
            f.route,
            101,
            1,
            change.operation,
            1 << 20
        )),
        Err(CatalogError::Invalid(_))
    ));
    assert_eq!(cancelled.prepared_owner_epoch, Some(100));
    assert_eq!(cancelled.completed_owner_epoch, Some(101));
    rt.block_on(
        f.catalog
            .check_active_pool_gate(&f.serial, f.route, 101, f.pools[0]),
    )
    .unwrap();
    lifecycle::no_media_changes(&owner, start);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
