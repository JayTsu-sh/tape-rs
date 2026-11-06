use super::*;
use tape_rs::managed_catalog::{
    ActiveUploadSpec, NodeMutation, NodeMutationSpec, NodeMutationTask, NodeMutationTaskState,
};

async fn queue(
    f: &Fixture,
    owner: u64,
    path: &str,
    version: Option<u64>,
    mutation: NodeMutation,
) -> NodeMutationTask {
    queue_in(f, owner, f.pools[0], path, version, mutation).await
}

async fn queue_in(
    f: &Fixture,
    owner: u64,
    pool: Uuid,
    path: &str,
    version: Option<u64>,
    mutation: NodeMutation,
) -> NodeMutationTask {
    let request = f
        .catalog
        .prepare_node_mutation_request(
            &f.serial,
            f.route,
            owner,
            &NodeMutationSpec {
                pool,
                operation: Uuid::new_v4(),
                path: path.into(),
                expected_version: version,
                mutation,
            },
            "2026-11-01T00:40:00.000000000Z",
        )
        .await
        .unwrap();
    f.catalog
        .queue_node_mutation_request(f.route, owner, &request, 4 << 20)
        .await
        .unwrap()
}

fn terminal(
    f: &Fixture,
    rt: &tokio::runtime::Runtime,
    task: &NodeMutationTask,
) -> NodeMutationTask {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = rt
            .block_on(f.catalog.node_mutation_task(
                &f.serial,
                f.route,
                task.request_epoch,
                task.operation,
            ))
            .unwrap()
            .unwrap();
        if matches!(
            current.state,
            NodeMutationTaskState::Completed
                | NodeMutationTaskState::Rejected
                | NodeMutationTaskState::Cancelled
        ) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not finish its durable Node task: {current:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实模拟 owner 自动消费节点队列，只读 owner 保留排队，不接触硬件"]
fn writable_owner_executes_queued_nodes_without_direct_media_calls() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let readonly = Owner::start(&f, 12, json!({}));
    let task = rt.block_on(async {
        let request = f
            .catalog
            .prepare_node_mutation_request(
                &f.serial,
                f.route,
                12,
                &NodeMutationSpec {
                    pool: f.pools[0],
                    operation: Uuid::new_v4(),
                    path: "/automatic".into(),
                    expected_version: None,
                    mutation: NodeMutation::CreateDirectory,
                },
                "2026-11-01T00:40:00.000000000Z",
            )
            .await
            .unwrap();
        f.catalog
            .queue_node_mutation_request(f.route, 12, &request, 4 << 20)
            .await
            .unwrap()
    });
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            task.request_epoch,
            task.operation
        ))
        .unwrap()
        .unwrap()
        .state,
        NodeMutationTaskState::Queued
    );
    drop(readonly);
    let writer = Owner::start_mode(&f, 13, json!({}), true);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let current = rt
            .block_on(f.catalog.node_mutation_task(
                &f.serial,
                f.route,
                task.request_epoch,
                task.operation,
            ))
            .unwrap()
            .unwrap();
        if current.state == NodeMutationTaskState::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "writable owner did not consume its durable Node task: {current:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let published = rt
        .block_on(f.catalog.node_mutation_publication(
            &f.serial,
            f.route,
            13,
            task.request_epoch,
            task.operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(published.version(), 1);
    assert_eq!(published.completed_owner(), 13);
    let path = format!("/{}/automatic", f.pools[0]);
    assert!(matches!(
        rt.block_on(f.catalog.resolve_active_path(&f.serial, path.as_bytes()))
            .unwrap(),
        Some(ResolvedPath::Directory { .. })
    ));
    for (version, mutation) in [
        (
            None,
            NodeMutation::CreateSymlink {
                target: ".LTFSEE_DATA/missing".into(),
            },
        ),
        (
            Some(1),
            NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
        ),
        (Some(2), NodeMutation::Unlink),
    ] {
        let task = rt.block_on(Box::pin(queue(
            &f,
            13,
            "/automatic/link",
            version,
            mutation,
        )));
        assert_eq!(
            terminal(&f, &rt, &task).state,
            NodeMutationTaskState::Completed
        );
    }
    let Some(ResolvedPath::Directory { version, .. }) = rt
        .block_on(f.catalog.resolve_active_path(&f.serial, path.as_bytes()))
        .unwrap()
    else {
        panic!("live directory");
    };
    let task = rt.block_on(Box::pin(queue(
        &f,
        13,
        "/automatic",
        Some(version),
        NodeMutation::RemoveDirectory,
    )));
    assert_eq!(
        terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    assert!(
        rt.block_on(f.catalog.resolve_active_path(&f.serial, path.as_bytes()))
            .unwrap()
            .is_none()
    );
    let file_path = format!("/{}/file-0", f.pools[0]);
    let Some(ResolvedPath::File { copy: original, .. }) = rt
        .block_on(
            f.catalog
                .resolve_active_path(&f.serial, file_path.as_bytes()),
        )
        .unwrap()
    else {
        panic!("original file");
    };
    let metadata = rt.block_on(Box::pin(queue(
        &f,
        13,
        "/file-0",
        Some(original.version),
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    )));
    assert_eq!(
        terminal(&f, &rt, &metadata).state,
        NodeMutationTaskState::Completed
    );
    let Some(ResolvedPath::File { copy: changed, .. }) = rt
        .block_on(
            f.catalog
                .resolve_active_path(&f.serial, file_path.as_bytes()),
        )
        .unwrap()
    else {
        panic!("changed file");
    };
    assert_eq!(changed.volume_id, original.volume_id);
    assert_eq!(changed.volume_generation, original.volume_generation);
    assert_eq!(changed.origin, original.origin);
    assert_eq!(changed.media, original.media);
    writer.expect(0);
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；等待节点不得取消已接受的文件预约，不接触硬件"]
fn node_queue_waits_for_file_reservations_without_aborting_them() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let writer = Owner::start_mode(&f, 12, json!({}), true);
    let intent = rt
        .block_on(f.catalog.prepare_active_upload(
            &f.serial,
            f.route,
            12,
            BARCODES[0],
            &ActiveUploadSpec {
                pool_id: f.pools[0],
                path: b"/waiting-file".to_vec(),
                operation_id: Uuid::new_v4(),
                expected_version: None,
                size: 3,
                digest: ContentDigest::Sha256(Sha256::digest(b"abc").into()),
            },
        ))
        .unwrap();
    rt.block_on(f.catalog.reserve_active_file(&intent)).unwrap();
    let task = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/after-file",
        None,
        NodeMutation::CreateDirectory,
    )));
    std::thread::sleep(Duration::from_millis(1400));
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            task.request_epoch,
            task.operation
        ))
        .unwrap()
        .unwrap()
        .state,
        NodeMutationTaskState::Queued
    );
    let reservations = rt
        .block_on(f.catalog.pending_active_volume_reservations(
            &f.serial,
            intent.volume_id,
            intent.volume_generation,
            None,
            100,
        ))
        .unwrap();
    assert!(
        reservations
            .items
            .iter()
            .any(|value| value.operation_id == intent.operation_id)
    );
    assert!(writer.permission.allows(12, Phase::Serving));
    rt.block_on(f.catalog.cancel_active_file_reservation(&intent))
        .unwrap();
    assert_eq!(
        terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；已准备节点跨 owner 与已装载介质恢复，不接触硬件"]
fn prepared_node_on_loaded_media_survives_owner_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let readonly = Owner::start(&f, 12, json!({}));
    readonly.expect(0);
    let task = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/prepared",
        None,
        NodeMutation::CreateDirectory,
    )));
    let prepared = rt
        .block_on(f.catalog.prepare_node_mutation_task(
            &f.serial,
            f.route,
            12,
            task.request_epoch,
            task.operation,
            4 << 20,
        ))
        .unwrap();
    readonly.permission.close(12);
    drop(readonly);
    assert!(loaded(&f).iter().any(Option::is_some));
    let writer = Owner::start_mode(&f, 13, json!({}), true);
    let completed = terminal(&f, &rt, &task);
    assert_eq!(completed.state, NodeMutationTaskState::Completed);
    assert_eq!(
        completed.prepared_owner_epoch,
        prepared.prepared_owner_epoch
    );
    assert_eq!(completed.prepared_at_millis, prepared.prepared_at_millis);
    let published = rt
        .block_on(f.catalog.node_mutation_publication(
            &f.serial,
            f.route,
            13,
            task.request_epoch,
            task.operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(published.attempt().original_owner(), 13);
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；运行中节点 S4 后故障的真实 owner 接管，不重复追加，不接触硬件"]
fn owner_recovers_failed_node_s4_without_reappending() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let writer = Owner::start_mode(&f, 12, json!({}), true);
    writer.fail_vci.store(true, Ordering::SeqCst);
    let task = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/interrupted",
        None,
        NodeMutation::CreateDirectory,
    )));
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.permission.allows(12, Phase::Serving) {
        assert!(
            Instant::now() < deadline,
            "node write did not reach its injected commit failure"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!writer.fail_vci.load(Ordering::SeqCst));
    let attempt = rt
        .block_on(f.catalog.node_mutation_attempt(
            &f.serial,
            f.route,
            12,
            task.request_epoch,
            task.operation,
        ))
        .unwrap()
        .unwrap();
    drop(writer);
    let resumed = Owner::start_mode(&f, 13, json!({}), true);
    assert_eq!(
        terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    let published = rt
        .block_on(f.catalog.node_mutation_publication(
            &f.serial,
            f.route,
            13,
            task.request_epoch,
            task.operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(published.attempt(), &attempt);
    assert_eq!(
        published.current().generation,
        attempt.previous().generation + 1
    );
    assert!(
        !resumed
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|command| matches!(command[0], 0x0a | 0x10 | 0x8d))
    );
    drop(resumed);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；失效节点条件自动拒绝并保留首次原因，不接触硬件"]
fn stale_queued_nodes_are_rejected_without_stalling_the_owner() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let readonly = Owner::start(&f, 12, json!({}));
    let first = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/first",
        None,
        NodeMutation::CreateDirectory,
    )));
    let second = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/second",
        None,
        NodeMutation::CreateDirectory,
    )));
    drop(readonly);
    let writer = Owner::start_mode(&f, 13, json!({}), true);
    let tasks = [terminal(&f, &rt, &first), terminal(&f, &rt, &second)];
    assert_eq!(
        tasks
            .iter()
            .filter(|task| task.state == NodeMutationTaskState::Completed)
            .count(),
        1
    );
    let refused = tasks
        .iter()
        .find(|task| task.state == NodeMutationTaskState::Rejected)
        .unwrap();
    assert!(!refused.rejection_reason.as_ref().unwrap().is_empty());
    assert!(refused.active_attempt.is_none());
    assert_eq!(
        rt.block_on(f.catalog.reject_node_mutation_task(
            &f.serial,
            f.route,
            13,
            refused.request_epoch,
            refused.operation,
            "changed reason"
        ))
        .unwrap(),
        *refused
    );
    assert!(writer.permission.allows(13, Phase::Serving));
    writer.expect(0);
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；无介质节点保持排队且允许同池介质接入后继续，不接触硬件"]
fn no_media_node_waits_without_blocking_intake_and_then_executes() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = Uuid::new_v4();
    let f = rt.block_on(Box::pin(fixture_with_extra_pool(&cluster, &[], Some(pool))));
    eprintln!("node_queue_workspace={}", f.root.display());
    let barcode = "NODE04L8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), BARCODES.len())
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    mkltfs(
        &f.library.drive(0),
        &MkltfsOptions {
            volume_id: "NODE04".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    fixture_return_to_storage(&f.library, 0);
    let writer = Owner::start_mode(&f, 12, json!({}), true);
    let task = rt.block_on(Box::pin(queue_in(
        &f,
        12,
        pool,
        "/after-intake",
        None,
        NodeMutation::CreateDirectory,
    )));
    std::thread::sleep(Duration::from_millis(1400));
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            task.request_epoch,
            task.operation
        ))
        .unwrap()
        .unwrap()
        .state,
        NodeMutationTaskState::Queued
    );
    rt.block_on(
        f.catalog
            .check_active_pool_gate(&f.serial, f.route, 12, pool),
    )
    .unwrap();
    let intake = rt.block_on(async {
        let window = f.catalog.active_management_window(&f.serial, f.route).await.unwrap();
        let request = tape_rs::managed_catalog::ManagementRequest::parse(json!({"version":1,"library":f.serial,
            "namespace":f.route.route.namespace.to_string(),"epoch":window.accepting,"operation":Uuid::new_v4().to_string(),
            "change":{"kind":"media-import","pool":pool.to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}})).unwrap();
        f.catalog.admit_management(&f.serial, f.route, 12, &request).await.unwrap();
        (window.accepting, request.operation())
    });
    assert_eq!(
        terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    let status = rt
        .block_on(
            f.catalog
                .management_task(&f.serial, f.route, intake.0, intake.1),
        )
        .unwrap()
        .unwrap();
    assert_eq!(status["status"], "succeeded");
    assert!(matches!(
        rt.block_on(
            f.catalog
                .resolve_active_path(&f.serial, format!("/{pool}/after-intake").as_bytes())
        )
        .unwrap(),
        Some(ResolvedPath::Directory { .. })
    ));
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；其他卷已热挂载时节点修改仍选文件主副本并保留原数据，不接触硬件"]
fn file_mutation_selects_its_primary_volume_with_another_volume_hot() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let writer = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"pools":{f.pools[0].to_string():{"mountlimit":"1"}}}),
        true,
    );
    writer.expect(1);
    assert!(
        !loaded(&f)
            .iter()
            .any(|barcode| barcode.as_deref() == Some(BARCODES[0]))
    );
    let path = format!("/{}/file-0", f.pools[0]);
    let Some(ResolvedPath::File { copy: original, .. }) = rt
        .block_on(f.catalog.resolve_active_path(&f.serial, path.as_bytes()))
        .unwrap()
    else {
        panic!("primary file");
    };
    let before = writer.moves();
    let task = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/file-0",
        Some(original.version),
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    )));
    assert_eq!(
        terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    let published = rt
        .block_on(f.catalog.node_mutation_publication(
            &f.serial,
            f.route,
            12,
            task.request_epoch,
            task.operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(published.current().volume_uuid, original.volume_id);
    let copy = published.physical_copy().unwrap().1;
    assert_eq!(copy.volume_id, original.volume_id);
    assert_eq!(copy.volume_generation, original.volume_generation);
    assert_eq!(copy.ltfs_file_uid, original.ltfs_file_uid);
    assert_eq!(copy.origin, original.origin);
    assert_eq!(copy.media, original.media);
    assert!(writer.moves() >= before + 2);
    writer.expect(0);
    drop(writer);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；节点占用期间 HTTP 上传保留请求且 owner 仍 Serving，不接触硬件"]
fn prepared_node_http_backpressure_preserves_request_and_serving() {
    use tape_rs::client::fdb::FdbClient;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_queue_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let context = super::active_pools::context(&owner, 71, 12);
    let server = ReadHttpServer::start(tape_rs::daemon::fdb_http::writer_router(
        f.catalog.clone(),
        std::collections::BTreeMap::from([(f.serial.clone(), f.route)]),
        std::collections::BTreeMap::from([(f.serial.clone(), context.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_millis(200), Duration::from_secs(2));
    let source = f.root.join("blocked-node-write.bin");
    fs::write(&source, b"pending").unwrap();
    let saved = f.root.join("blocked-node-write.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/blocked-http",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &saved,
        )
        .unwrap();
    let bytes = fs::read(saved.join("request.json")).unwrap();
    // Real native policy keeps this fixture Prepared while the writable
    // runtime and its HTTP authority remain healthy; no scheduler test flag.
    rt.block_on(async {
        for serial in ["SIMDRV0000", "SIMDRV0001"] {
            let policy = f.catalog.active_resource_policy(&f.serial, f.route, "drive", serial).await.unwrap();
            let window = f.catalog.active_management_window(&f.serial, f.route).await.unwrap();
            let request = tape_rs::managed_catalog::ManagementRequest::parse(json!({"version":1,"library":f.serial,
                "namespace":f.route.route.namespace.to_string(),"epoch":window.accepting,"operation":Uuid::new_v4().to_string(),
                "change":{"kind":"drive","serial":serial,"revision":policy["revision"],"patch":{"role":"r"}}})).unwrap();
            f.catalog.admit_management(&f.serial, f.route, 12, &request).await.unwrap();
            assert_eq!(f.catalog.apply_management(&f.serial, f.route, 12, window.accepting, request.operation()).await.unwrap()["status"], "succeeded");
        }
    });
    let task = rt.block_on(Box::pin(queue(
        &f,
        12,
        "/http-blocker",
        None,
        NodeMutation::CreateDirectory,
    )));
    rt.block_on(f.catalog.prepare_node_mutation_task(
        &f.serial,
        f.route,
        12,
        task.request_epoch,
        task.operation,
        4 << 20,
    ))
    .unwrap();
    assert!(client.upload_saved(&saved).is_err());
    assert_eq!(fs::read(saved.join("request.json")).unwrap(), bytes);
    assert!(owner.permission.allows(12, Phase::Serving));
    assert_eq!(server.request("GET", "/readyz", None).0, 200);
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            task.request_epoch,
            task.operation
        ))
        .unwrap()
        .unwrap()
        .state,
        NodeMutationTaskState::Prepared
    );
    rt.block_on(f.catalog.cancel_node_mutation_task(
        &f.serial,
        f.route,
        12,
        task.request_epoch,
        task.operation,
    ))
    .unwrap();
    drop(server);
    drop(context);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
