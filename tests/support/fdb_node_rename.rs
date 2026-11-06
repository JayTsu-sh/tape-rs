//! Original two-path rename preconditions against real FDB and simulated media.
use super::*;
use tape_rs::client::fdb::FdbClient;
use tape_rs::managed_catalog::{NodeRenameRequest, NodeRenameRevision, NodeRenameSpec};

const TIME: &str = "2026-11-02T10:00:00.000000000Z";

#[path = "fdb_cli_rename.rs"]
mod cli;
#[path = "fdb_fuse_rename.rs"]
mod fuse;
#[path = "fdb_fuse_rename_journal.rs"]
mod fuse_journal;
#[path = "fdb_s3_rename.rs"]
mod s3;

fn revision(value: &serde_json::Value) -> NodeRenameRevision {
    NodeRenameRevision {
        namespace: Uuid::parse_str(value["namespace"].as_str().unwrap()).unwrap(),
        inode: Uuid::parse_str(value["id"].as_str().unwrap()).unwrap(),
        version: value["version"].as_u64().unwrap(),
    }
}
fn spec(client: &FdbClient, f: &Fixture, from: &str, to: &str, no_replace: bool) -> NodeRenameSpec {
    let stat = |path| client.stat(&f.serial, f.pools[0], path).unwrap();
    let parent = |path: &str| {
        let p = path.rsplit_once('/').unwrap().0;
        if p.is_empty() {
            "/".to_string()
        } else {
            p.to_string()
        }
    };
    NodeRenameSpec {
        pool: f.pools[0],
        operation: Uuid::new_v4(),
        from: from.into(),
        to: to.into(),
        source: revision(&stat(from).unwrap()),
        source_parent: revision(&stat(&parent(from)).unwrap()),
        destination_parent: revision(&stat(&parent(to)).unwrap()),
        target: stat(to).as_ref().map(revision),
        no_replace,
    }
}
fn mutate(client: &FdbClient, f: &Fixture, path: &str, mutation: serde_json::Value) {
    let expected = client
        .stat(&f.serial, f.pools[0], path)
        .unwrap()
        .map(|node| node["version"].as_u64().unwrap());
    let condition = original_condition(
        client,
        &f.serial,
        f.route.route.namespace,
        f.pools[0],
        path,
        expected,
    );
    let pathbuf = f.root.join(format!("rename-setup-{}", Uuid::new_v4()));
    client
        .prepare_node_mutation(&f.serial, f.pools[0], path, condition, mutation, &pathbuf)
        .unwrap();
    client.run_node_mutation_saved(&pathbuf).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；rename原两端条件/规范请求/接任，不接触真实设备"]
fn rename_preflight_binds_original_two_path_identities_and_survives_owner_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_preflight_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original = spec(&client, &f, "/file-0", "/file-1", false);
    let commands = owner.trace.lock().unwrap().len();
    let before0 = client.stat(&f.serial, f.pools[0], "/file-0").unwrap();
    let before1 = client.stat(&f.serial, f.pools[0], "/file-1").unwrap();
    let request = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 12, &original, TIME),
        )
        .unwrap();
    let bytes = request.encode().unwrap();
    let fingerprint = request.fingerprint();
    assert_eq!(NodeRenameRequest::decode(&bytes).unwrap(), request);
    let baseline = rt
        .block_on(f.catalog.check_node_rename_request(f.route, 12, &request))
        .unwrap();
    assert_eq!(baseline.spec, original);
    let ResolvedPath::File {
        placement: from, ..
    } = baseline.source
    else {
        panic!("source file")
    };
    let Some(ResolvedPath::File { placement: to, .. }) = baseline.target else {
        panic!("target file")
    };
    assert_ne!(
        from.volume_id, to.volume_id,
        "a same-pool rename may need multiple media plans"
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/file-0").unwrap(),
        before0
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/file-1").unwrap(),
        before1
    );
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 12, None, 100)
        )
        .unwrap()
        .items
        .is_empty()
    );
    assert_eq!(
        owner.trace.lock().unwrap().len(),
        commands,
        "preflight cannot touch media"
    );
    let object = rt
        .block_on(
            f.catalog
                .upload_node_rename_request(f.route, 12, &request, 8 << 20),
        )
        .unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .load_node_rename_request(f.route, 12, &object, 8 << 20)
        )
        .unwrap(),
        request
    );
    assert!(
        rt.block_on(
            f.catalog
                .load_node_mutation_request(f.route, 12, &object, 8 << 20)
        )
        .is_err(),
        "rename is a distinct original protocol, not a node mutation compatibility form"
    );
    assert!(
        rt.block_on(
            f.catalog
                .load_node_rename_request(f.route, 12, &object, bytes.len() - 1)
        )
        .is_err()
    );
    drop(client);
    drop(server);
    drop(owner);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let loaded = rt
        .block_on(
            f.catalog
                .load_node_rename_request(f.route, 13, &object, 8 << 20),
        )
        .unwrap();
    assert_eq!(loaded.encode().unwrap(), bytes);
    assert_eq!(loaded.fingerprint(), fingerprint);
    assert_eq!(
        rt.block_on(f.catalog.check_node_rename_request(f.route, 13, &loaded))
            .unwrap()
            .owner_epoch,
        13
    );
    assert!(
        rt.block_on(f.catalog.check_node_rename_request(f.route, 12, &loaded))
            .is_err()
    );

    // Absence is pinned, even when replacement was permitted in general.
    let absence = spec(&client, &f, "/file-0", "/appeared", false);
    let pending = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &absence, TIME),
        )
        .unwrap();
    mutate(&client, &f, "/appeared", json!({"kind":"create-directory"}));
    assert!(
        rt.block_on(f.catalog.check_node_rename_request(f.route, 13, &pending))
            .is_err()
    );
    let mut conditions = spec(&client, &f, "/file-0", "/appeared", false);
    conditions.target = None;
    assert!(
        rt.block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            13,
            &conditions,
            TIME
        ))
        .is_err()
    );
    let mismatched = spec(&client, &f, "/file-0", "/appeared", false);
    assert!(
        rt.block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            13,
            &mismatched,
            TIME
        ))
        .is_err()
    );

    for path in [
        "/left",
        "/right",
        "/left/tree",
        "/left/tree/child",
        "/right/empty",
        "/right/full",
        "/right/full/child",
    ] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    let directory = spec(&client, &f, "/left/tree", "/right/empty", false);
    let request_dir = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &directory, TIME),
        )
        .unwrap();
    let snapshot = rt
        .block_on(
            f.catalog
                .check_node_rename_request(f.route, 13, &request_dir),
        )
        .unwrap();
    assert!(matches!(snapshot.source, ResolvedPath::Directory { .. }));
    let nonempty = spec(&client, &f, "/left/tree", "/right/full", false);
    let error = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &nonempty, TIME),
        )
        .unwrap_err();
    assert!(error.to_string().contains("非空"), "{error}");
    let forbidden = spec(&client, &f, "/left/tree", "/right/empty", true);
    assert!(
        rt.block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &forbidden, TIME)
        )
        .is_err()
    );

    // Re-created destination parent with the same numeric version is a new inode.
    let fresh = spec(&client, &f, "/left/tree", "/right/empty/new", false);
    let prior = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &fresh, TIME),
        )
        .unwrap();
    mutate(
        &client,
        &f,
        "/right/empty",
        json!({"kind":"remove-directory"}),
    );
    mutate(
        &client,
        &f,
        "/right/empty",
        json!({"kind":"create-directory"}),
    );
    assert!(
        rt.block_on(f.catalog.check_node_rename_request(f.route, 13, &prior))
            .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &fresh, TIME)
        )
        .is_err()
    );

    // An existing target condition also pins its UUID, independently of parent CAS.
    let original_target = directory.target.unwrap();
    let mut replaced = spec(&client, &f, "/left/tree", "/right/empty", false);
    assert_ne!(replaced.target.unwrap().inode, original_target.inode);
    assert_eq!(replaced.target.unwrap().version, original_target.version);
    replaced.target = Some(original_target);
    assert!(
        rt.block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &replaced, TIME)
        )
        .is_err()
    );
    mutate(&client, &f, "/source", json!({"kind":"create-directory"}));
    let original_source = spec(&client, &f, "/source", "/unused", true);
    mutate(&client, &f, "/source", json!({"kind":"remove-directory"}));
    mutate(&client, &f, "/source", json!({"kind":"create-directory"}));
    let mut replaced = spec(&client, &f, "/source", "/unused", true);
    assert_ne!(replaced.source.inode, original_source.source.inode);
    assert_eq!(replaced.source.version, original_source.source.version);
    replaced.source = original_source.source;
    assert!(
        rt.block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, 13, &replaced, TIME)
        )
        .is_err()
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .load_node_rename_request(f.route, 13, &object, 8 << 20)
        )
        .unwrap()
        .encode()
        .unwrap(),
        bytes,
        "saved original does not resolve current paths"
    );
    assert!(
        rt.block_on(f.catalog.check_node_rename_request(f.route, 13, &loaded))
            .is_err()
    );
    drop(client);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；真实owner混合节点队列与rename池占用，模拟磁带不接触真实设备"]
fn rename_queue_does_not_starve_single_nodes_and_blocks_writes_until_original_cancel() {
    use tape_rs::managed_catalog::{NodeMutationTaskKind, NodeMutationTaskState};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_queue_workspace={}", f.root.display());
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false},BARCODES[1]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let before0 = client.stat(&f.serial, f.pools[0], "/file-0").unwrap();
    let before1 = client.stat(&f.serial, f.pools[0], "/file-1").unwrap();
    let first = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/file-0", "/file-1", false),
            TIME,
        ))
        .unwrap();
    let queued = rt
        .block_on(
            f.catalog
                .queue_node_rename_request(f.route, 12, &first, 8 << 20),
        )
        .unwrap();
    assert_eq!(queued.kind, NodeMutationTaskKind::Rename);
    // The live owner must keep serving ordinary tasks even with a queued rename.
    let other = f.root.join("rename-fair-other-pool");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[1],
            "/before-hold",
            original_condition(
                &client,
                &f.serial,
                f.route.route.namespace,
                f.pools[1],
                "/before-hold",
                None,
            ),
            json!({"kind":"create-directory"}),
            &other,
        )
        .unwrap();
    client.run_node_mutation_saved(&other).unwrap();
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            first.request_epoch(),
            first.spec().operation
        ))
        .unwrap()
        .unwrap(),
        queued
    );
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        12,
        first.request_epoch(),
        first.spec().operation,
    ))
    .unwrap();
    let request = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/file-0", "/file-1", false),
            TIME,
        ))
        .unwrap();
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 8 << 20),
    )
    .unwrap();
    let prepared = rt
        .block_on(f.catalog.prepare_node_rename_task(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    assert_eq!(prepared.state, NodeMutationTaskState::Prepared);
    let (status, _) = send(
        &server,
        &f,
        "DELETE",
        &format!(
            "{}/requests/{}/{}",
            base(&f),
            request.request_epoch(),
            request.spec().operation
        ),
        b"",
    );
    assert!(
        matches!(status, 400 | 409),
        "wrong protocol must not cancel rename: {status}"
    );
    let (status, _) = send(
        &server,
        &f,
        "GET",
        &format!(
            "{}/requests/{}/{}",
            base(&f),
            request.request_epoch(),
            request.spec().operation
        ),
        b"",
    );
    assert!(
        matches!(status, 400 | 409),
        "wrong protocol must not report a rename as a single-node mutation: {status}"
    );
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation
        ))
        .unwrap()
        .unwrap(),
        prepared
    );
    let local = f.root.join("behind-rename");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/after-hold",
            original_condition(
                &client,
                &f.serial,
                f.route.route.namespace,
                f.pools[0],
                "/after-hold",
                None,
            ),
            json!({"kind":"create-directory"}),
            &local,
        )
        .unwrap();
    let no_wait = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(3));
    assert!(no_wait.run_node_mutation_saved(&local).is_err());
    let pending = client.node_mutation_status_saved(&local).unwrap().unwrap();
    assert_eq!(pending["status"], "queued");
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/after-hold")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/file-0").unwrap(),
        before0
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/file-1").unwrap(),
        before1
    );
    let cancelled = rt
        .block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
    assert_eq!(cancelled.state, NodeMutationTaskState::Cancelled);
    assert_eq!(
        client.run_node_mutation_saved(&local).unwrap()["status"],
        "completed"
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .queue_node_rename_request(f.route, 12, &request, 8 << 20)
        )
        .unwrap(),
        cancelled,
        "cached original terminal survives parent changes without re-registration"
    );
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/after-hold")
            .unwrap()
            .is_some()
    );
    drop(no_wait);
    drop(client);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；rename原Prepared两端祖先/大属性/预算/owner检查，无rename介质操作"]
fn prepared_rename_inputs_capture_both_canonical_chains_and_original_large_attributes() {
    use tape_rs::managed_catalog::{NodeAttributes, NodeMutationTaskKind};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_inputs_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    for path in [
        "/a",
        "/a/src",
        "/a/dst",
        "/a/src/tree",
        "/a/src/tree/child",
        "/a/dst/empty",
    ] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    let large = "原".repeat(40_000);
    for path in ["/a", "/a/src/tree", "/a/dst/empty"] {
        mutate(
            &client,
            &f,
            path,
            json!({"kind":"set-xattr","attribute":{"key":"opaque","value":large,"base64":false},"mode":"upsert"}),
        );
    }
    let request = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/a/src/tree", "/a/dst/empty", false),
            TIME,
        ))
        .unwrap();
    let trace = Arc::clone(&owner.trace);
    drop(client);
    drop(server);
    drop(owner);
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 8 << 20),
    )
    .unwrap();
    assert!(
        rt.block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20
        ))
        .is_err(),
        "Queued is not a canonical snapshot hold"
    );
    let prepared = rt
        .block_on(f.catalog.prepare_node_rename_task(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    let commands = trace.lock().unwrap().len();
    assert!(
        rt.block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            128 << 10
        ))
        .is_err(),
        "declared large attributes must be charged before hydration"
    );
    let inputs = rt
        .block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    assert_eq!(inputs.task, prepared);
    assert_eq!(inputs.request, request);
    assert_eq!(
        inputs
            .source_ancestors
            .iter()
            .map(|a| a.path.as_str())
            .collect::<Vec<_>>(),
        ["/", "/a", "/a/src"]
    );
    assert_eq!(
        inputs
            .destination_ancestors
            .iter()
            .map(|a| a.path.as_str())
            .collect::<Vec<_>>(),
        ["/", "/a", "/a/dst"]
    );
    assert_eq!(inputs.source_ancestors[0], inputs.destination_ancestors[0]);
    assert_eq!(inputs.source_ancestors[1], inputs.destination_ancestors[1]);
    let parent = &inputs.source_ancestors[1].attributes;
    assert!(parent.meta().is_some());
    assert_eq!(
        parent
            .xattrs()
            .unwrap()
            .iter()
            .find(|a| a.key == "opaque")
            .unwrap()
            .value,
        large
    );
    for node in [
        &inputs.baseline.source,
        inputs.baseline.target.as_ref().unwrap(),
    ] {
        let ResolvedPath::Directory {
            attributes: Some(a),
            ..
        } = node
        else {
            panic!("fully loaded directory")
        };
        assert_eq!(
            a.xattrs()
                .unwrap()
                .iter()
                .find(|a| a.key == "opaque")
                .unwrap()
                .value,
            large
        );
    }
    rt.block_on(
        f.catalog
            .check_prepared_node_rename_inputs(f.route, 12, &inputs),
    )
    .unwrap();
    assert_eq!(
        trace.lock().unwrap().len(),
        commands,
        "canonical metadata capture cannot access media"
    );
    let mut wrong = inputs.clone();
    wrong.source_ancestors.swap(0, 1);
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &wrong)
        )
        .is_err()
    );
    let mut wrong = inputs.clone();
    wrong.destination_ancestors.pop();
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &wrong)
        )
        .is_err()
    );
    let mut wrong = inputs.clone();
    wrong.destination_ancestors[0].id = Uuid::new_v4();
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &wrong)
        )
        .is_err()
    );
    let mut wrong = inputs.clone();
    wrong.task.kind = NodeMutationTaskKind::SingleNode;
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &wrong)
        )
        .is_err()
    );
    let mut wrong = inputs.clone();
    let a = &mut wrong.source_ancestors[1].attributes;
    let mut meta = a.meta().unwrap().clone();
    meta.readonly = !meta.readonly;
    *a = NodeAttributes::new(meta, a.xattrs().unwrap().to_vec(), None).unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &wrong)
        )
        .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation
        ))
        .unwrap()
        .unwrap(),
        prepared
    );
    // Metadata qualification only: a newer owner is not given a media permit.
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
        .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 12, &inputs)
        )
        .is_err()
    );
    let reopened = tape_rs::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap();
    let next = rt
        .block_on(reopened.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    assert_eq!(next.task, prepared);
    assert_eq!(next.request, request);
    assert_eq!(next.baseline.owner_epoch, 13);
    assert_eq!(next.source_ancestors, inputs.source_ancestors);
    assert_eq!(next.destination_ancestors, inputs.destination_ancestors);
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        13,
        request.request_epoch(),
        request.spec().operation,
    ))
    .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .check_prepared_node_rename_inputs(f.route, 13, &next)
        )
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
            8 << 20
        ))
        .is_err()
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；有界规范子树分页/两卷原副本/外部属性/接任，无rename介质操作"]
fn prepared_rename_subtree_pages_keep_original_nodes_and_page_independent_digest() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_subtree_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    for path in [
        "/src",
        "/src/branch",
        "/src/empty",
        "/src/中",
        "/dst",
        "/src/stale",
    ] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    mutate(
        &client,
        &f,
        "/src/stale",
        json!({"kind":"remove-directory"}),
    );
    mutate(
        &client,
        &f,
        "/src/link",
        json!({"kind":"create-symlink","target":"branch/a"}),
    );
    mutate(
        &client,
        &f,
        "/src/中",
        json!({"kind":"set-xattr","attribute":{"key":"opaque","value":"原".repeat(40_000),"base64":false},"mode":"upsert"}),
    );
    mutate(
        &client,
        &f,
        "/src",
        json!({"kind":"set-xattr","attribute":{"key":"root-opaque","value":"原".repeat(40_000),"base64":false},"mode":"upsert"}),
    );
    let body = f.root.join("subtree-body");
    fs::write(&body, b"canonical original body").unwrap();
    let parent = revision(
        &client
            .stat(&f.serial, f.pools[0], "/src/branch")
            .unwrap()
            .unwrap(),
    );
    let outside_path = tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/file-0").unwrap();
    let outside = revision(
        &client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
    );
    let outside_saved = f.root.join("outside-replacement");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/file-0",
            &body,
            UploadCondition::Replace(tape_rs::client::FileVersion::Fdb {
                namespace: outside.namespace,
                inode: outside.inode,
                version: outside.version,
            }),
            Some(BARCODES[0]),
            &outside_saved,
        )
        .unwrap();
    client.upload_saved(&outside_saved).unwrap();
    let outside_before = rt
        .block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &outside_path),
        )
        .unwrap()
        .unwrap();
    mutate(
        &client,
        &f,
        "/file-0",
        json!({"kind":"set-xattr","attribute":{"key":"guard-outside","value":"retained","base64":false},"mode":"upsert"}),
    );

    for (i, name) in ["a", "b"].into_iter().enumerate() {
        let saved = f.root.join(format!("subtree-upload-{i}"));
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                &format!("/src/branch/{name}"),
                &body,
                UploadCondition::Create {
                    namespace: parent.namespace,
                    parent: parent.inode,
                },
                Some(BARCODES[i]),
                &saved,
            )
            .unwrap();
        client.upload_saved(&saved).unwrap();
    }
    let request = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/src", "/dst/moved", true),
            TIME,
        ))
        .unwrap();
    let trace = Arc::clone(&owner.trace);
    drop(client);
    drop(server);
    drop(owner);
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 8 << 20),
    )
    .unwrap();
    rt.block_on(f.catalog.prepare_node_rename_task(
        &f.serial,
        f.route,
        12,
        request.request_epoch(),
        request.spec().operation,
        8 << 20,
    ))
    .unwrap();
    let inputs = rt
        .block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    let commands = trace.lock().unwrap().len();
    let mut capture = rt
        .block_on(
            f.catalog
                .begin_node_rename_subtree_capture(f.route, 12, &inputs),
        )
        .unwrap();
    let initial = capture.summary();
    for (limit, budget) in [(0, 8 << 20), (1001, 8 << 20), (1, 1)] {
        assert!(
            rt.block_on(f.catalog.next_node_rename_subtree_page(
                f.route,
                12,
                &mut capture,
                limit,
                budget
            ))
            .is_err()
        );
        assert_eq!(capture.summary(), initial, "failed page must not advance");
    }
    let mut rows = Vec::new();
    while !capture.summary().complete {
        let page = rt
            .block_on(f.catalog.next_node_rename_subtree_page(
                f.route,
                12,
                &mut capture,
                1,
                16 << 10,
            ))
            .unwrap();
        assert!(page.entries.len() <= 1);
        rows.extend(page.entries);
        assert!(rows.len() <= 7);
    }
    assert_eq!(
        rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/src",
            "/src/branch",
            "/src/branch/a",
            "/src/branch/b",
            "/src/empty",
            "/src/link",
            "/src/中"
        ]
    );
    for row in &rows {
        assert_eq!(row.destination, format!("/dst/moved{}", &row.path[4..]));
        assert!(row.encode().unwrap().len() < 16 << 10);
    }
    let ResolvedPath::Directory {
        attributes: Some(attributes),
        ..
    } = &rows[6].node
    else {
        panic!("directory")
    };
    assert!(
        attributes.meta().is_none(),
        "descendant external attributes stay references"
    );
    let ResolvedPath::Directory {
        attributes: Some(root_attributes),
        ..
    } = &rows[0].node
    else {
        panic!("root directory")
    };
    assert!(
        root_attributes.meta().is_none(),
        "a hydrated root also returns a compact immutable reference"
    );
    let volumes = rows
        .iter()
        .filter_map(|r| match &r.node {
            ResolvedPath::File { copy, .. } if r.path.starts_with("/src/branch/") => {
                Some(copy.volume_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(volumes.len(), 2);
    assert_ne!(volumes[0], volumes[1]);
    let ResolvedPath::File { copy, .. } = &rows[5].node else {
        panic!("link")
    };
    assert!(copy.media.as_ref().unwrap().native_symlink);
    let summary = capture.summary();
    assert_eq!(
        (summary.entries, summary.files, summary.directories),
        (7, 3, 4)
    );
    let mut together = rt
        .block_on(
            f.catalog
                .begin_node_rename_subtree_capture(f.route, 12, &inputs),
        )
        .unwrap();
    let page = rt
        .block_on(f.catalog.next_node_rename_subtree_page(
            f.route,
            12,
            &mut together,
            1000,
            8 << 20,
        ))
        .unwrap();
    assert_eq!(page.entries, rows);
    assert_eq!(page.summary, summary);
    assert_eq!(
        trace.lock().unwrap().len(),
        commands,
        "capture is metadata-only"
    );
    let mut durable = rt
        .block_on(
            f.catalog
                .resume_node_rename_manifest_capture(f.route, 12, &inputs),
        )
        .unwrap();
    while !durable.summary().complete {
        rt.block_on(
            f.catalog
                .next_node_rename_manifest_page(f.route, 12, &mut durable),
        )
        .unwrap();
    }
    assert_eq!(durable.summary(), &summary);
    let manifest = rt
        .block_on(
            f.catalog
                .finish_node_rename_manifest_capture(f.route, 12, &durable),
        )
        .unwrap();
    assert_eq!(manifest.creator_owner_epoch(), 12);
    let mut manifest_reader = rt
        .block_on(
            f.catalog
                .open_node_rename_manifest_reader(f.route, 12, &manifest),
        )
        .unwrap();
    let mut decoded = Vec::new();
    while let Some(entries) = rt
        .block_on(
            f.catalog
                .next_node_rename_manifest_entries(f.route, 12, &mut manifest_reader),
        )
        .unwrap()
    {
        decoded.extend(entries);
    }
    assert!(manifest_reader.complete());
    assert_eq!(decoded, rows);
    let mut spool = rt
        .block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 12, &manifest, 16 << 20),
        )
        .unwrap();
    assert_eq!(
        (
            spool.stats().nodes,
            spool.stats().files,
            spool.stats().directories
        ),
        (7, 3, 4)
    );
    let mut expected_groups = std::collections::BTreeMap::new();
    for row in &rows {
        if let ResolvedPath::File { copy, .. } = &row.node {
            expected_groups
                .entry(copy.volume_id)
                .or_insert_with(Vec::new)
                .push(row.clone());
        }
    }
    let mut cursor = None;
    let mut groups = Vec::new();
    loop {
        let page = spool.volumes_page(cursor.as_deref(), 1).unwrap();
        for group in page.items {
            groups.push(group.volume);
            let mut expected = expected_groups[&group.volume].clone();
            expected.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
            assert_eq!(group.files, expected.len() as u64);
            let mut files = Vec::new();
            let mut file_cursor = None;
            loop {
                let page = spool
                    .volume_files_page(group.volume, file_cursor.as_deref(), 1, 16 << 10)
                    .unwrap();
                files.extend(page.items);
                file_cursor = page.next;
                if file_cursor.is_none() {
                    break;
                }
            }
            assert_eq!(files, expected);
            for row in files {
                let ResolvedPath::File { copy, .. } = &row.node else {
                    unreachable!()
                };
                let uid = copy
                    .media
                    .as_ref()
                    .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
                assert_eq!(spool.metadata_node(group.volume, uid).unwrap(), Some(row));
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(groups, expected_groups.keys().copied().collect::<Vec<_>>());
    let mut directories = rows
        .iter()
        .filter(|r| matches!(r.node, ResolvedPath::Directory { .. }))
        .cloned()
        .collect::<Vec<_>>();
    directories.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    assert_eq!(
        spool.directories_page(None, 1000, 8 << 20).unwrap().items,
        directories
    );
    assert!(
        rt.block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 12, &manifest, 1024)
        )
        .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation
        ))
        .unwrap()
        .unwrap(),
        inputs.task
    );
    let outside_now = rt
        .block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &outside_path),
        )
        .unwrap()
        .unwrap();
    let ResolvedPath::File {
        id: outside_id,
        head: outside_head,
        copy: outside_copy,
        ..
    } = &outside_now
    else {
        unreachable!()
    };
    let guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut spool,
            outside_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    let outside_uid = outside_copy
        .media
        .as_ref()
        .map_or(outside_copy.ltfs_file_uid, |m| m.logical_file_uid);
    let protected = guard.metadata(outside_uid).unwrap().unwrap();
    assert_eq!(
        protected.role,
        tape_rs::managed_catalog::NodeRenameReferenceRole::Other
    );
    assert_eq!(
        (
            protected.node.inode,
            protected.node.version,
            protected.copy_id
        ),
        (*outside_id, outside_head.version, outside_head.copy_id)
    );
    assert!(
        guard
            .data(outside_copy.ltfs_file_uid)
            .unwrap()
            .unwrap()
            .other
            >= 1
    );
    assert_eq!(
        guard.stats().source_files,
        expected_groups[&outside_copy.volume_id].len() as u64
    );
    let reverse = |node: &ResolvedPath| {
        let ResolvedPath::File { id, head, copy, .. } = node else {
            unreachable!()
        };
        let mut key = b"tape-rs/catalog/22/V/".to_vec();
        key.extend_from_slice(copy.volume_id.as_bytes());
        key.extend_from_slice(copy.volume_scan_id.as_bytes());
        key.extend_from_slice(f.route.route.namespace.as_bytes());
        key.extend_from_slice(id.as_bytes());
        key.extend_from_slice(head.copy_id.as_bytes());
        key
    };
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let current_reverse = reverse(&outside_now);
    // A current scan can retain an older copy row after interrupted cleanup.
    // Inject that row explicitly: successful ordinary writes need not retain it.
    let ResolvedPath::File { head: old_head, .. } = &outside_before else {
        unreachable!()
    };
    let mut historical_reverse = current_reverse.clone();
    let copy_offset = historical_reverse.len() - 16;
    historical_reverse[copy_offset..].copy_from_slice(old_head.copy_id.as_bytes());
    assert_ne!(historical_reverse, current_reverse);
    let original_reverse = rt.block_on(async {
        let tr = db.create_trx().unwrap();
        let original = tr
            .get(&current_reverse, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut historical = old_head.version.to_be_bytes().to_vec();
        historical.extend_from_slice(&[0x78; 32]);
        tr.set(&historical_reverse, &historical);
        tr.commit().await.unwrap();
        original
    });
    for broken in [
        None,
        Some(original_reverse[..original_reverse.len() - 1].to_vec()),
    ] {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            match &broken {
                Some(raw) => tr.set(&current_reverse, raw),
                None => tr.clear(&current_reverse),
            };
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(f.catalog.build_node_rename_volume_guard(
                f.route,
                12,
                &manifest,
                &mut spool,
                outside_copy.volume_id,
                16 << 20
            ))
            .is_err()
        );
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            assert_eq!(
                tr.get(&current_reverse, false)
                    .await
                    .unwrap()
                    .map(|r| r.to_vec()),
                broken
            );
            tr.set(&current_reverse, &original_reverse);
            tr.commit().await.unwrap();
        });
    }
    let actual_commands = trace.lock().unwrap().len();
    assert_eq!(
        actual_commands, commands,
        "manifest and disk projection are metadata-only"
    );

    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
        .unwrap();
    assert!(
        rt.block_on(f.catalog.next_node_rename_subtree_page(
            f.route,
            12,
            &mut capture,
            1,
            16 << 10
        ))
        .is_err()
    );
    assert_eq!(capture.summary(), summary);
    let inputs = rt
        .block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .next_node_rename_manifest_entries(f.route, 12, &mut manifest_reader)
        )
        .is_err()
    );
    let recovered = rt
        .block_on(
            f.catalog
                .resume_node_rename_manifest_capture(f.route, 13, &inputs),
        )
        .unwrap();
    assert_eq!(recovered.summary(), &summary);
    let manifest = rt
        .block_on(
            f.catalog
                .finish_node_rename_manifest_capture(f.route, 13, &recovered),
        )
        .unwrap();
    assert_eq!(manifest.creator_owner_epoch(), 12);
    let mut manifest_reader = rt
        .block_on(
            f.catalog
                .open_node_rename_manifest_reader(f.route, 13, &manifest),
        )
        .unwrap();
    while rt
        .block_on(
            f.catalog
                .next_node_rename_manifest_entries(f.route, 13, &mut manifest_reader),
        )
        .unwrap()
        .is_some()
    {}
    assert!(manifest_reader.complete());
    let takeover_spool = rt
        .block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 13, &manifest, 16 << 20),
        )
        .unwrap();
    assert_eq!(takeover_spool.stats(), spool.stats());
    let next_guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            13,
            &manifest,
            &mut spool,
            outside_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    assert_eq!(next_guard.stats().sha256, guard.stats().sha256);
    let mut takeover = rt
        .block_on(
            f.catalog
                .begin_node_rename_subtree_capture(f.route, 13, &inputs),
        )
        .unwrap();
    while !takeover.summary().complete {
        rt.block_on(f.catalog.next_node_rename_subtree_page(
            f.route,
            13,
            &mut takeover,
            2,
            16 << 10,
        ))
        .unwrap();
    }
    assert_eq!(
        takeover.summary(),
        summary,
        "original request digest excludes execution owner"
    );
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        13,
        request.request_epoch(),
        request.spec().operation,
    ))
    .unwrap();
    assert!(
        rt.block_on(f.catalog.next_node_rename_subtree_page(
            f.route,
            13,
            &mut takeover,
            2,
            16 << 10
        ))
        .is_err()
    );
    assert_eq!(takeover.summary(), summary);
    assert!(
        rt.block_on(
            f.catalog
                .next_node_rename_manifest_entries(f.route, 13, &mut manifest_reader)
        )
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 13, &manifest, 16 << 20)
        )
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            13,
            &manifest,
            &mut spool,
            outside_copy.volume_id,
            16 << 20
        ))
        .is_err()
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；不同卷源/替换目标/其他文件UID防护及owner切换，无rename介质操作"]
fn rename_volume_guards_bind_original_source_target_and_other_current_files() {
    rename_volume_component(false, false, false, false, false, false, false);
}

#[test]
#[ignore = "需要隔离FDB；仅Sim磁带，原按卷计划改写/完整提交/重挂载S4，不发布命名空间"]
fn rename_volume_media_commits_original_attempt_without_publishing_namespace() {
    rename_volume_component(true, false, false, false, false, false, false);
}

#[test]
#[ignore = "需要隔离FDB；可信全局参与卷封口/缺计划拒绝/接任/损坏不修复，无rename介质操作"]
fn rename_global_participants_keep_original_source_target_recipes_across_takeover() {
    rename_volume_component(false, true, false, false, false, false, false);
}

#[test]
#[ignore = "需要隔离FDB；原未武装尝试/共享卷占用/接任关闭/取消，不授权rename介质IO"]
fn rename_attempt_preparation_keeps_original_intents_and_closes_only_its_shared_claims() {
    rename_volume_component(false, true, true, false, false, false, false);
}

fn rename_volume_component(
    exercise_media: bool,
    exercise_participants: bool,
    exercise_attempts: bool,
    exercise_writing: bool,
    exercise_observation: bool,
    exercise_closure: bool,
    exercise_rebase: bool,
) {
    use tape_rs::managed_catalog::NodeRenameReferenceRole;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_uid_guard_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    // Finish original fixture materialization through the ordinary node queue.
    mutate(
        &client,
        &f,
        "/guard-ready",
        json!({"kind":"create-directory"}),
    );
    let request = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/file-0", "/file-1", false),
            TIME,
        ))
        .unwrap();
    let trace = Arc::clone(&owner.trace);
    let permission = Arc::clone(&owner.permission);
    drop(client);
    drop(server);
    drop(owner);
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 8 << 20),
    )
    .unwrap();
    rt.block_on(f.catalog.prepare_node_rename_task(
        &f.serial,
        f.route,
        12,
        request.request_epoch(),
        request.spec().operation,
        8 << 20,
    ))
    .unwrap();
    let inputs = rt
        .block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    let commands = trace.lock().unwrap().len();
    let mut capture = rt
        .block_on(
            f.catalog
                .resume_node_rename_manifest_capture(f.route, 12, &inputs),
        )
        .unwrap();
    while !capture.summary().complete {
        rt.block_on(
            f.catalog
                .next_node_rename_manifest_page(f.route, 12, &mut capture),
        )
        .unwrap();
    }
    let manifest = rt
        .block_on(
            f.catalog
                .finish_node_rename_manifest_capture(f.route, 12, &capture),
        )
        .unwrap();
    let mut view = rt
        .block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 12, &manifest, 16 << 20),
        )
        .unwrap();
    let source = &inputs.baseline.source;
    let target = inputs.baseline.target.as_ref().unwrap();
    let ResolvedPath::File {
        id: source_id,
        copy: source_copy,
        ..
    } = source
    else {
        unreachable!()
    };
    let ResolvedPath::File {
        id: target_id,
        copy: target_copy,
        ..
    } = target
    else {
        unreachable!()
    };
    assert_ne!(source_copy.volume_id, target_copy.volume_id);
    let mut source_guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut view,
            source_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    let mut target_guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut view,
            target_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    let source_uid = source_copy
        .media
        .as_ref()
        .map_or(source_copy.ltfs_file_uid, |m| m.logical_file_uid);
    let target_uid = target_copy
        .media
        .as_ref()
        .map_or(target_copy.ltfs_file_uid, |m| m.logical_file_uid);
    let source_ref = source_guard.metadata(source_uid).unwrap().unwrap();
    let target_ref = target_guard.metadata(target_uid).unwrap().unwrap();
    assert_eq!(
        (source_ref.node.inode, source_ref.role),
        (*source_id, NodeRenameReferenceRole::Source)
    );
    assert_eq!(
        (target_ref.node.inode, target_ref.role),
        (*target_id, NodeRenameReferenceRole::Target)
    );
    assert_eq!(
        (
            source_guard.stats().source_files,
            source_guard.stats().target_files
        ),
        (1, 0)
    );
    assert_eq!(
        (
            target_guard.stats().source_files,
            target_guard.stats().target_files
        ),
        (0, 1)
    );
    assert_eq!(
        source_guard
            .data(source_copy.ltfs_file_uid)
            .unwrap()
            .unwrap()
            .source,
        1
    );
    assert_eq!(
        target_guard
            .data(target_copy.ltfs_file_uid)
            .unwrap()
            .unwrap()
            .target,
        1
    );
    let other_path = tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/file-3").unwrap();
    let other = rt
        .block_on(f.catalog.resolve_path(f.route.route.namespace, &other_path))
        .unwrap()
        .unwrap();
    let ResolvedPath::File {
        id: other_id,
        copy: other_copy,
        ..
    } = other
    else {
        unreachable!()
    };
    let mut other_guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut view,
            other_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    let other_uid = other_copy
        .media
        .as_ref()
        .map_or(other_copy.ltfs_file_uid, |m| m.logical_file_uid);
    let other_ref = other_guard.metadata(other_uid).unwrap().unwrap();
    assert_eq!(
        (other_ref.node.inode, other_ref.role),
        (other_id, NodeRenameReferenceRole::Other)
    );
    // Read only simulated drives to obtain actual complete source indexes.
    // The inventory call itself receives no device handle and emits no CDB.
    if !fixture_drive_empty(&f.library) {
        fixture_return_to_storage(&f.library, 71);
    }
    let physical_trace = Arc::new(Mutex::new(Vec::new()));
    let mut original_source_physical = None;
    let mut original_source_projection = None;
    let mut original_source_plan = None;
    let mut media_components = Vec::new();
    let mut participant_capture = if exercise_participants {
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
                .unwrap()
                .is_none()
        );
        assert!(
            rt.block_on(f.catalog.begin_node_rename_participants(
                f.route,
                12,
                &manifest,
                &mut view,
                Some(source_copy.volume_id)
            ))
            .is_err(),
            "nonempty source cannot add an anchor"
        );
        Some(
            rt.block_on(
                f.catalog
                    .begin_node_rename_participants(f.route, 12, &manifest, &mut view, None),
            )
            .unwrap(),
        )
    } else {
        None
    };
    let mut original_global = None;
    for (barcode, guard, uid) in [
        (BARCODES[0], &mut source_guard, source_uid),
        (BARCODES[1], &mut target_guard, target_uid),
        (BARCODES[3], &mut other_guard, other_uid),
    ] {
        f.library.load_into_drive(barcode, 0).unwrap();
        let transport = ScheduleTransport {
            inner: Box::new(f.library.drive_as(0, 71)),
            trace: Arc::clone(&physical_trace),
            permission: Arc::clone(&permission),
            revoke_on_unload: Arc::new(AtomicBool::new(false)),
            error_after_move: Arc::new(AtomicBool::new(false)),
        };
        let volume = LtfsVolume::mount(&transport).unwrap();
        let before = volume.index().to_xml().unwrap();
        let before_commands = physical_trace.lock().unwrap().len();
        let mut inventory = guard
            .physical_source(&inputs.baseline, &mut view, volume.index(), 16 << 20)
            .unwrap();
        assert_eq!(inventory.source_index(), guard.source_index());
        assert_eq!(inventory.stats().source_files, guard.stats().source_files);
        assert_eq!(inventory.stats().target_files, guard.stats().target_files);
        assert_eq!(inventory.stats().other_files, guard.stats().other_files);
        assert_eq!(inventory.file(uid).unwrap().unwrap().1.meta.file_uid, uid);
        let mut wrong = volume.index().clone();
        wrong.generation += 1;
        assert!(
            guard
                .physical_source(&inputs.baseline, &mut view, &wrong, 16 << 20)
                .is_err()
        );
        let projected = rt.block_on(f.catalog.build_node_rename_volume_projection(
            f.route,
            12,
            &manifest,
            &mut view,
            &mut inventory,
            16 << 20,
        ));
        if guard.stats().source_files > 0 || guard.stats().target_files > 0 {
            let mut projected = projected.unwrap();
            assert!(
                rt.block_on(f.catalog.node_rename_volume_plan(
                    f.route,
                    12,
                    &manifest,
                    guard.source_index().volume_uuid
                ))
                .unwrap()
                .is_none()
            );
            let saved = rt
                .block_on(f.catalog.save_node_rename_volume_plan(
                    f.route,
                    12,
                    &manifest,
                    &mut projected,
                ))
                .unwrap();
            assert_eq!(saved.creator_owner_epoch(), 12);
            assert_eq!(saved.after_sha256(), projected.stats().after);
            assert_eq!(saved.before_sha256(), projected.stats().before);
            assert_eq!(saved.highest_file_uid(), projected.stats().highest_uid);
            assert_eq!(
                rt.block_on(f.catalog.node_rename_volume_plan(
                    f.route,
                    12,
                    &manifest,
                    guard.source_index().volume_uuid
                ))
                .unwrap(),
                Some(saved.clone())
            );
            assert_eq!(
                rt.block_on(f.catalog.save_node_rename_volume_plan(
                    f.route,
                    12,
                    &manifest,
                    &mut projected
                ))
                .unwrap(),
                saved
            );

            if exercise_participants && barcode == BARCODES[0] {
                let capture = participant_capture.as_mut().unwrap();
                let before = capture.summary().clone();
                assert!(
                    rt.block_on(
                        f.catalog
                            .next_node_rename_participants_page(f.route, 12, capture, &mut view)
                    )
                    .is_err(),
                    "missing original target recipe must prevent sealing"
                );
                assert_eq!(capture.summary(), &before);
                assert!(!capture.complete());
                assert!(
                    rt.block_on(
                        f.catalog
                            .finish_node_rename_participants(f.route, 12, capture)
                    )
                    .is_err()
                );
                assert!(
                    rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(projected.stats().source_files, guard.stats().source_files);
            if guard.stats().source_files > 0 {
                assert_eq!(projected.file(uid).unwrap().unwrap().0, request.spec().to);
                original_source_projection = Some(projected.stats().after);
                original_source_plan = Some(saved.clone());
                require_send(f.catalog.build_node_rename_volume_projection(
                    f.route,
                    12,
                    &manifest,
                    &mut view,
                    &mut inventory,
                    16 << 20,
                ));
                original_source_physical = Some(inventory);
            } else {
                assert!(projected.file(uid).unwrap().is_none());
            }
            if exercise_media || exercise_observation || exercise_closure {
                media_components.push((barcode, saved, projected));
            }
        } else {
            assert!(
                projected.is_err(),
                "unrelated volume cannot become a rename participant"
            );
        }
        let after_commands = physical_trace.lock().unwrap().len();
        assert_eq!(
            after_commands, before_commands,
            "physical source audit is device-free"
        );
        assert_eq!(volume.index().to_xml().unwrap(), before);
        drop(volume);
        fixture_return_to_storage(&f.library, 71);
    }
    if let Some(capture) = participant_capture.as_mut() {
        let mut participants = Vec::new();
        while !capture.complete() {
            let before = capture.summary().clone();
            let expected = rt
                .block_on(
                    f.catalog
                        .node_rename_next_participant_volumes(f.route, 12, capture, &mut view),
                )
                .unwrap();
            assert!(expected.len() <= 32);
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .node_rename_next_participant_volumes(f.route, 12, capture, &mut view)
                )
                .unwrap(),
                expected
            );
            assert_eq!(capture.summary(), &before);
            let page = rt
                .block_on(
                    f.catalog
                        .next_node_rename_participants_page(f.route, 12, capture, &mut view),
                )
                .unwrap();
            assert_eq!(
                page.iter()
                    .map(|member| member.volume())
                    .collect::<Vec<_>>(),
                expected
            );
            participants.extend(page);
        }
        assert_eq!(participants.len(), 2);
        assert_eq!(
            participants.iter().map(|p| p.source_files()).sum::<u64>(),
            1
        );
        assert_eq!(
            participants.iter().filter(|p| p.includes_target()).count(),
            1
        );
        assert!(participants.iter().all(|p| !p.is_anchor()));
        assert_eq!(
            participants
                .iter()
                .find(|p| p.source_files() == 1)
                .unwrap()
                .volume(),
            source_copy.volume_id
        );
        assert_eq!(
            participants
                .iter()
                .find(|p| p.includes_target())
                .unwrap()
                .volume(),
            target_copy.volume_id
        );
        let global = rt
            .block_on(
                f.catalog
                    .finish_node_rename_participants(f.route, 12, capture),
            )
            .unwrap();
        assert_eq!(
            (
                global.summary().volumes,
                global.summary().source_files,
                global.summary().target_volumes,
                global.summary().anchors
            ),
            (2, 1, 1, 0)
        );
        assert_eq!(global.creator_owner_epoch(), 12);
        assert_eq!(global.request(), &request);
        assert_eq!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
                .unwrap()
                .unwrap()
                .fingerprint(),
            global.fingerprint()
        );
        assert_eq!(
            rt.block_on(
                f.catalog
                    .finish_node_rename_participants(f.route, 12, capture)
            )
            .unwrap()
            .fingerprint(),
            global.fingerprint()
        );
        original_global = Some(global.fingerprint());
    }
    let foreign_path = tape_rs::managed_catalog::pool_catalog_path(f.pools[1], b"/file-2").unwrap();
    let foreign = rt
        .block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &foreign_path),
        )
        .unwrap()
        .unwrap();
    let ResolvedPath::File {
        copy: foreign_copy, ..
    } = foreign
    else {
        unreachable!()
    };
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut view,
            foreign_copy.volume_id,
            16 << 20
        ))
        .is_err()
    );
    fn require_send<T: Send>(_: T) {}
    require_send(f.catalog.build_node_rename_volume_guard(
        f.route,
        12,
        &manifest,
        &mut view,
        source_copy.volume_id,
        16 << 20,
    ));
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
        .unwrap();
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut view,
            source_copy.volume_id,
            16 << 20
        ))
        .is_err()
    );
    let renewed = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            13,
            &manifest,
            &mut view,
            source_copy.volume_id,
            16 << 20,
        ))
        .unwrap();
    assert_eq!(renewed.stats().sha256, source_guard.stats().sha256);
    let mut original_source_physical = original_source_physical.unwrap();
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_projection(
            f.route,
            12,
            &manifest,
            &mut view,
            &mut original_source_physical,
            16 << 20
        ))
        .is_err()
    );
    let mut renewed_projection = rt
        .block_on(f.catalog.build_node_rename_volume_projection(
            f.route,
            13,
            &manifest,
            &mut view,
            &mut original_source_physical,
            16 << 20,
        ))
        .unwrap();
    assert_eq!(
        Some(renewed_projection.stats().after),
        original_source_projection
    );

    if let Some(original) = original_global {
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
                .is_err()
        );
        let global = rt
            .block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
            .unwrap()
            .unwrap();
        assert_eq!(global.fingerprint(), original);
        assert_eq!(global.creator_owner_epoch(), 12);
        let mut resume = rt
            .block_on(
                f.catalog
                    .begin_node_rename_participants(f.route, 13, &manifest, &mut view, None),
            )
            .unwrap();
        while !resume.complete() {
            rt.block_on(f.catalog.next_node_rename_participants_page(
                f.route,
                13,
                &mut resume,
                &mut view,
            ))
            .unwrap();
        }
        assert_eq!(
            rt.block_on(
                f.catalog
                    .finish_node_rename_participants(f.route, 13, &resume)
            )
            .unwrap()
            .fingerprint(),
            original
        );
    }
    let saved = original_source_plan.unwrap();
    assert!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            12,
            &manifest,
            source_copy.volume_id
        ))
        .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            source_copy.volume_id
        ))
        .unwrap(),
        Some(saved.clone())
    );
    assert_eq!(
        rt.block_on(f.catalog.save_node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            &mut renewed_projection
        ))
        .unwrap(),
        saved
    );
    assert_eq!(saved.creator_owner_epoch(), 12);
    assert!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            other_copy.volume_id
        ))
        .unwrap()
        .is_none()
    );
    let mut saved_key = b"tape-rs/catalog/22/@/".to_vec();
    saved_key.extend_from_slice(f.route.route.namespace.as_bytes());
    saved_key.extend_from_slice(b"QM");
    assert!(!f.serial.as_bytes().contains(&0));
    saved_key.extend_from_slice(f.serial.as_bytes());
    saved_key.extend_from_slice(&[0, 0]);
    saved_key.extend_from_slice(&request.request_epoch().to_be_bytes());
    saved_key.extend_from_slice(request.spec().operation.as_bytes());
    saved_key.push(2);
    saved_key.extend_from_slice(source_copy.volume_id.as_bytes());
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let original_record = rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&saved_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec()
    });
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear(&saved_key);
        tr.commit().await.unwrap();
    });
    let missing = rt.block_on(f.catalog.node_rename_volume_plan(
        f.route,
        13,
        &manifest,
        source_copy.volume_id,
    ));
    if original_global.is_some() {
        assert!(missing.is_err(), "a referenced recipe must fail closed");
    } else {
        assert!(missing.unwrap().is_none());
    }
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        assert!(tr.get(&saved_key, false).await.unwrap().is_none());
        tr.set(&saved_key, &original_record);
        tr.commit().await.unwrap();
    });
    let mut foreign_key = saved_key.clone();
    let suffix = foreign_key.len() - 16;
    foreign_key[suffix..].copy_from_slice(other_copy.volume_id.as_bytes());
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&foreign_key, &original_record);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            other_copy.volume_id
        ))
        .is_err()
    );
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        assert_eq!(
            tr.get(&foreign_key, false).await.unwrap().unwrap().as_ref(),
            original_record
        );
        tr.clear(&foreign_key);
        tr.commit().await.unwrap();
    });
    let bad_record = original_record[..original_record.len() - 1].to_vec();
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&saved_key, &bad_record);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            source_copy.volume_id
        ))
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.save_node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            &mut renewed_projection
        ))
        .is_err()
    );
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        assert_eq!(
            tr.get(&saved_key, false).await.unwrap().unwrap().as_ref(),
            bad_record
        );
        tr.set(&saved_key, &original_record);
        tr.commit().await.unwrap();
    });
    if let Some(original) = original_global {
        use sha2::{Digest, Sha256};
        let root = &saved_key[..saved_key.len() - 17];
        let mut global_key = root.to_vec();
        global_key.push(3);
        let mut page_key = root.to_vec();
        page_key.push(4);
        page_key.extend_from_slice(&0u64.to_be_bytes());
        let read = |key: &[u8]| {
            rt.block_on(async {
                db.create_trx()
                    .unwrap()
                    .get(key, false)
                    .await
                    .unwrap()
                    .map(|raw| raw.to_vec())
            })
        };
        let original_header = read(&global_key).unwrap();
        let original_page = read(&page_key).unwrap();
        let page_value: serde_json::Value = serde_json::from_slice(&original_page).unwrap();
        let checksum = |value: &serde_json::Value| -> serde_json::Value {
            let bytes = serde_json::to_vec(&json!([value["facts"], value["creator"]])).unwrap();
            let hash: Vec<u8> = Sha256::new()
                .chain_update(b"tape-rs/node-rename-participant-record/1")
                .chain_update(bytes)
                .finalize()
                .to_vec();
            json!(hash)
        };
        assert_eq!(checksum(&page_value), page_value["checksum"]);
        for case in 0..5 {
            let mut value = page_value.clone();
            match case {
                0 => value["facts"]["index"] = 1.into(),
                1 => {
                    value["facts"]["manifest"][0] =
                        (value["facts"]["manifest"][0].as_u64().unwrap() ^ 0x80).into()
                }
                2 => {
                    value["facts"]["logical"][0] =
                        (value["facts"]["logical"][0].as_u64().unwrap() ^ 0x80).into()
                }
                3 => {
                    value["facts"]["members"][0]["source_files"] =
                        (value["facts"]["members"][0]["source_files"]
                            .as_u64()
                            .unwrap()
                            + 1)
                        .into()
                }
                _ => {
                    value["facts"]["members"][0]["recipe"][0] =
                        (value["facts"]["members"][0]["recipe"][0].as_u64().unwrap() ^ 0x80).into()
                }
            }
            value["checksum"] = checksum(&value);
            let bad = serde_json::to_vec(&value).unwrap();
            rt.block_on(async {
                let tr = db.create_trx().unwrap();
                tr.set(&page_key, &bad);
                tr.commit().await.unwrap();
            });
            assert!(
                rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                    .is_err()
            );
            assert_eq!(
                read(&page_key).unwrap(),
                bad,
                "corrupt page must remain unchanged"
            );
        }
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.clear(&global_key);
            tr.clear(&page_key);
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                .unwrap()
                .is_none()
        );
        assert!(read(&global_key).is_none());
        assert!(
            rt.block_on(f.catalog.finish_node_rename_participants(
                f.route,
                13,
                participant_capture.as_ref().unwrap()
            ))
            .is_err(),
            "a missing earlier page cannot produce a global header"
        );
        assert!(read(&global_key).is_none());
        assert!(read(&page_key).is_none());
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&global_key, &original_header);
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                .is_err(),
            "complete header cannot hide a missing page"
        );
        assert!(read(&page_key).is_none());
        let bad = original_page[..original_page.len() - 1].to_vec();
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&page_key, &bad);
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                .is_err()
        );
        assert_eq!(read(&page_key).unwrap(), bad);
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&page_key, &original_page);
            tr.commit().await.unwrap();
        });
        assert_eq!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                .unwrap()
                .unwrap()
                .fingerprint(),
            original
        );
    }
    if exercise_closure {
        let context = RenameAttemptContext {
            rt: &rt,
            f: &f,
            manifest: &manifest,
            request: &request,
            inputs: &inputs,
            db: &db,
            saved: &saved,
            source_copy,
            trace: &trace,
            commands,
        };
        if let Some(sealed) = exercise_rename_closure(context, media_components, exercise_rebase) {
            exercise_rename_publication(context, &sealed);
        }
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    if exercise_observation {
        exercise_rename_observation(
            RenameAttemptContext {
                rt: &rt,
                f: &f,
                manifest: &manifest,
                request: &request,
                inputs: &inputs,
                db: &db,
                saved: &saved,
                source_copy,
                trace: &trace,
                commands,
            },
            media_components,
        );
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    if exercise_attempts {
        exercise_rename_attempt_preparation(
            RenameAttemptContext {
                rt: &rt,
                f: &f,
                manifest: &manifest,
                request: &request,
                inputs: &inputs,
                db: &db,
                saved: &saved,
                source_copy,
                trace: &trace,
                commands,
            },
            exercise_writing,
        );
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    if exercise_media {
        use tape_rs::managed_catalog::NodeRenameMediaPlan;
        use tape_rs::scsi::reservation::{ReservationKey, fence};
        // Low-level data-only component test. The rename actor remains disabled;
        // this does not grant any production actor permission to write.
        let source_path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], request.spec().from.as_bytes())
                .unwrap();
        let target_path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], request.spec().to.as_bytes())
                .unwrap();
        let source_before = rt
            .block_on(
                f.catalog
                    .resolve_path(f.route.route.namespace, &source_path),
            )
            .unwrap();
        let target_before = rt
            .block_on(
                f.catalog
                    .resolve_path(f.route.route.namespace, &target_path),
            )
            .unwrap();
        for (i, (barcode, recipe, projected)) in media_components.into_iter().enumerate() {
            f.library.load_into_drive(barcode, 0).unwrap();
            let dev = f.library.drive_as(0, 71);
            let key = ReservationKey::new(71, 13 + 2 * i as u64);
            fence(&dev, key).unwrap();
            let plan = NodeRenameMediaPlan::new(recipe, Uuid::new_v4(), 12).unwrap();
            let mut volume = LtfsVolume::mount(&dev).unwrap();
            assert!(volume.node_rename_evidence(&plan, 16 << 20).is_err());
            volume.set_reservation_guard(Some(key));
            let unchanged = volume
                .node_rename_absence_evidence(&plan, 16 << 20)
                .unwrap();
            assert_eq!(
                unchanged.kind(),
                tape_rs::managed_catalog::NodeRenameAbsenceKind::Unchanged
            );
            let before_count = unchanged.file_count();
            let original_media = f.library.cartridge(barcode).unwrap();
            let before = volume.index().to_xml().unwrap();
            let before_source = volume.index().find_file(&request.spec().from).cloned();
            assert!(volume.node_rename_evidence(&plan, 16 << 20).is_err());
            volume
                .stage_node_rename(&plan, &projected, 16 << 20)
                .unwrap();
            assert_eq!(
                volume.index().to_xml().unwrap(),
                before,
                "staging cannot publish working state"
            );
            assert!(
                volume
                    .stage_node_rename(&plan, &projected, 16 << 20)
                    .is_err()
            );
            assert!(volume.node_rename_evidence(&plan, 16 << 20).is_err());
            volume.sync_batch().unwrap();
            let proof = volume.node_rename_evidence(&plan, 16 << 20).unwrap();
            assert!(
                !proof.current_index().incremental,
                "rename requires a full index"
            );
            if let Some(original) = before_source {
                let moved = volume.index().find_file(&request.spec().to).unwrap();
                assert_eq!(moved.meta.file_uid, original.meta.file_uid);
                assert_eq!(moved.extents, original.extents);
                assert_eq!(moved.length, original.length);
                assert!(volume.index().find_file(&request.spec().from).is_none());
            } else {
                assert!(volume.index().find_file(&request.spec().to).is_none());
            }
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .resolve_path(f.route.route.namespace, &source_path)
                )
                .unwrap(),
                source_before
            );
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .resolve_path(f.route.route.namespace, &target_path)
                )
                .unwrap(),
                target_before
            );
            drop(volume);
            let mut recovered = LtfsVolume::mount(&dev).unwrap();
            recovered.set_reservation_guard(Some(key));
            assert_eq!(
                recovered.node_rename_evidence(&plan, 16 << 20).unwrap(),
                proof
            );
            let other =
                NodeRenameMediaPlan::new(plan.recipe().clone(), plan.attempt(), 13).unwrap();
            assert!(recovered.node_rename_evidence(&other, 16 << 20).is_err());
            let newer = ReservationKey::new(71, 14 + 2 * i as u64);
            fence(&dev, newer).unwrap();
            assert!(recovered.node_rename_evidence(&plan, 16 << 20).is_err());
            drop(recovered);
            let mut recovered = LtfsVolume::mount(&dev).unwrap();
            recovered.set_reservation_guard(Some(newer));
            assert_eq!(
                recovered.node_rename_evidence(&plan, 16 << 20).unwrap(),
                proof,
                "takeover remount keeps the original physical identity"
            );
            drop(recovered);
            let committed_media = f.library.cartridge(barcode).unwrap();
            use tape_rs::scsi::cdb::opcode;
            use tape_rs::scsi::sim::{Fault, FaultAction};
            for (opcode, after, stage, has_s4) in [
                (opcode::LOCATE_16, 0, "locate", false),
                (opcode::WRITE_FILEMARKS_6, 0, "opening_fm", false),
                (opcode::WRITE_6, 0, "index_records", false),
                (opcode::WRITE_FILEMARKS_6, 1, "closing_fm", false),
                (opcode::WRITE_6, 1, "index_partition", true),
                (opcode::WRITE_FILEMARKS_6, 4, "barrier", true),
                (opcode::WRITE_ATTRIBUTE, 1, "vci", true),
            ] {
                f.library
                    .with_cartridge_mut(barcode, |c| *c = original_media.clone());
                let mut trial = LtfsVolume::mount(&dev).unwrap();
                trial.set_reservation_guard(Some(newer));
                trial
                    .stage_node_rename(&plan, &projected, 16 << 20)
                    .unwrap();
                if stage == "locate" {
                    TapeDrive::new(&dev).locate(0, 0, true).unwrap();
                }
                dev.inject(Fault {
                    opcode,
                    after,
                    remaining: 1,
                    action: FaultAction::CheckCondition {
                        sense_key: 3,
                        asc: 0x0c,
                        ascq: 0,
                    },
                });
                let error = trial.commit().unwrap_err();
                assert!(
                    matches!(error,tape_rs::error::TapeError::CommitFailed{stage:ref actual,..} if actual==stage),
                    "expected {stage}: {error}"
                );
                assert!(!trial.writable());
                assert_eq!(trial.index().to_xml().unwrap(), before);
                assert!(trial.node_rename_evidence(&plan, 16 << 20).is_err());
                assert!(trial.commit().is_err());
                dev.clear_faults();
                drop(trial);
                let mut fresh = LtfsVolume::mount(&dev).unwrap();
                fresh.set_reservation_guard(Some(newer));
                let proof = fresh.node_rename_evidence(&plan, 16 << 20);
                assert_eq!(proof.is_ok(), has_s4, "{stage}: {proof:?}");
                if has_s4 {
                    assert!(fresh.node_rename_absence_evidence(&plan, 16 << 20).is_err());
                    assert!(fresh.discard_node_rename_tail(&plan, 16 << 20).is_err());
                } else {
                    let absence = match fresh.node_rename_absence_evidence(&plan, 16 << 20) {
                        Ok(unchanged) => {
                            assert_eq!(
                                unchanged.kind(),
                                tape_rs::managed_catalog::NodeRenameAbsenceKind::Unchanged
                            );
                            unchanged
                        }
                        Err(_) => {
                            let proof = fresh.discard_node_rename_tail(&plan, 16 << 20).unwrap();
                            assert_eq!(
                                proof.kind(),
                                tape_rs::managed_catalog::NodeRenameAbsenceKind::Discarded
                            );
                            proof
                        }
                    };
                    assert_eq!(absence.plan(), &plan);
                    assert_eq!(absence.file_count(), before_count);
                    assert_eq!(
                        fresh.node_rename_absence_evidence(&plan, 16 << 20).unwrap(),
                        absence
                    );
                    assert!(fresh.node_rename_evidence(&plan, 16 << 20).is_err());
                }
            }
            f.library
                .with_cartridge_mut(barcode, |c| *c = committed_media);
            fixture_return_to_storage(&f.library, 71);
        }
    }
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        13,
        request.request_epoch(),
        request.spec().operation,
    ))
    .unwrap();
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            13,
            &manifest,
            &mut view,
            source_copy.volume_id,
            16 << 20
        ))
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.build_node_rename_volume_projection(
            f.route,
            13,
            &manifest,
            &mut view,
            &mut original_source_physical,
            16 << 20
        ))
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            source_copy.volume_id
        ))
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.save_node_rename_volume_plan(
            f.route,
            13,
            &manifest,
            &mut renewed_projection
        ))
        .is_err()
    );
    if original_global.is_some() {
        assert!(
            rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, &manifest))
                .is_err()
        );
    }
    let final_commands = trace.lock().unwrap().len();
    assert_eq!(final_commands, commands, "UID guards do not touch media");
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；空目录原合格锚卷封口，不移动介质或发布命名空间"]
fn rename_empty_directory_seals_one_original_qualified_anchor() {
    exercise_empty_directory(false, None, false);
}
fn exercise_empty_directory(
    publish: bool,
    execution: Option<RenameExecutionCase>,
    check_preparation: bool,
) {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_anchor_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    for path in ["/empty-source", "/destination"] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    let request = rt
        .block_on(f.catalog.prepare_node_rename_request(
            &f.serial,
            f.route,
            12,
            &spec(&client, &f, "/empty-source", "/destination/moved", true),
            TIME,
        ))
        .unwrap();
    drop(client);
    drop(server);
    drop(owner);
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 8 << 20),
    )
    .unwrap();
    rt.block_on(f.catalog.prepare_node_rename_task(
        &f.serial,
        f.route,
        12,
        request.request_epoch(),
        request.spec().operation,
        8 << 20,
    ))
    .unwrap();
    let inputs = rt
        .block_on(f.catalog.capture_prepared_node_rename_inputs(
            &f.serial,
            f.route,
            12,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap();
    let mut capture = rt
        .block_on(
            f.catalog
                .resume_node_rename_manifest_capture(f.route, 12, &inputs),
        )
        .unwrap();
    while !capture.summary().complete {
        rt.block_on(
            f.catalog
                .next_node_rename_manifest_page(f.route, 12, &mut capture),
        )
        .unwrap();
    }
    let manifest = rt
        .block_on(
            f.catalog
                .finish_node_rename_manifest_capture(f.route, 12, &capture),
        )
        .unwrap();
    assert_eq!(manifest.summary().files, 0);
    let mut projection = rt
        .block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 12, &manifest, 16 << 20),
        )
        .unwrap();
    assert!(
        rt.block_on(f.catalog.begin_node_rename_participants(
            f.route,
            12,
            &manifest,
            &mut projection,
            None
        ))
        .is_err()
    );
    let path = tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/file-0").unwrap();
    let Some(ResolvedPath::File { copy, .. }) = rt
        .block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
        .unwrap()
    else {
        panic!("anchor fixture file");
    };
    let volume = copy.volume_id;
    assert!(
        rt.block_on(f.catalog.resume_node_rename_participants(
            f.route,
            12,
            &manifest,
            &mut projection
        ))
        .is_err(),
        "missing original anchor page cannot select a fresh anchor"
    );
    let mut participants = rt
        .block_on(f.catalog.begin_node_rename_participants(
            f.route,
            12,
            &manifest,
            &mut projection,
            Some(volume),
        ))
        .unwrap();
    fn require_preview_send<T: Send>(_: T) {}
    require_preview_send(f.catalog.node_rename_next_participant_volumes(
        f.route,
        12,
        &participants,
        &mut projection,
    ));
    let preview_before = participants.summary().clone();
    for _ in 0..2 {
        assert_eq!(
            rt.block_on(f.catalog.node_rename_next_participant_volumes(
                f.route,
                12,
                &participants,
                &mut projection
            ))
            .unwrap(),
            vec![volume]
        );
        assert_eq!(participants.summary(), &preview_before);
        assert!(!participants.complete());
    }
    let before = participants.summary().clone();
    assert!(
        rt.block_on(f.catalog.next_node_rename_participants_page(
            f.route,
            12,
            &mut participants,
            &mut projection
        ))
        .is_err(),
        "an anchor needs its original retained recipe"
    );
    assert_eq!(participants.summary(), &before);
    if !fixture_drive_empty(&f.library) {
        fixture_return_to_storage(&f.library, 71);
    }
    f.library.load_into_drive(BARCODES[0], 0).unwrap();
    let dev = f.library.drive_as(0, 71);
    let permission = Arc::new(tape_rs::daemon::permission::Permission::default());
    let previous = Arc::new(tape_rs::daemon::permission::Permission::default());
    let injected = Arc::new(f.library.drive_as(0, 71));
    let guarded = tape_rs::daemon::permission::GuardedTransport::new(
        Box::new(RenameSharedTransport(Arc::clone(&injected))),
        Arc::clone(&permission),
        13,
    );
    let prior_guarded = tape_rs::daemon::permission::GuardedTransport::new(
        Box::new(RenameSharedTransport(Arc::clone(&injected))),
        Arc::clone(&previous),
        12,
    );
    if execution.is_some() {
        assert!(previous.renew(
            12,
            tape_rs::daemon::permission::Phase::Serving,
            std::time::Instant::now() + Duration::from_secs(60)
        ));
    }
    let transport: &dyn TapeTransport = if execution.is_some() {
        &prior_guarded
    } else {
        &dev
    };
    let mounted = LtfsVolume::mount(transport).unwrap();
    let xml = mounted.index().to_xml().unwrap();
    let mut guard = rt
        .block_on(f.catalog.build_node_rename_volume_guard(
            f.route,
            12,
            &manifest,
            &mut projection,
            volume,
            16 << 20,
        ))
        .unwrap();
    let mut source = guard
        .physical_source(&inputs.baseline, &mut projection, mounted.index(), 16 << 20)
        .unwrap();
    let mut view = rt
        .block_on(f.catalog.build_node_rename_volume_projection(
            f.route,
            12,
            &manifest,
            &mut projection,
            &mut source,
            16 << 20,
        ))
        .unwrap();
    assert_eq!(view.stats().source_files, 0);
    assert!(view.directory("/destination/moved").unwrap().is_some());
    let recipe = rt
        .block_on(
            f.catalog
                .save_node_rename_volume_plan(f.route, 12, &manifest, &mut view),
        )
        .unwrap();
    let members = rt
        .block_on(f.catalog.next_node_rename_participants_page(
            f.route,
            12,
            &mut participants,
            &mut projection,
        ))
        .unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].volume(), volume);
    assert!(members[0].is_anchor());
    assert!(!members[0].includes_target());
    assert_eq!(members[0].source_files(), 0);
    assert_eq!(members[0].recipe_fingerprint(), recipe.fingerprint());
    assert!(participants.complete());
    assert!(
        rt.block_on(f.catalog.node_rename_next_participant_volumes(
            f.route,
            12,
            &participants,
            &mut projection
        ))
        .unwrap()
        .is_empty()
    );

    let mut resumed = rt
        .block_on(f.catalog.resume_node_rename_participants(
            f.route,
            12,
            &manifest,
            &mut projection,
        ))
        .unwrap();
    while !resumed.complete() {
        rt.block_on(f.catalog.next_node_rename_participants_page(
            f.route,
            12,
            &mut resumed,
            &mut projection,
        ))
        .unwrap();
    }
    assert_eq!(resumed.summary(), participants.summary());
    let global = rt
        .block_on(
            f.catalog
                .finish_node_rename_participants(f.route, 12, &participants),
        )
        .unwrap();
    assert_eq!(
        (
            global.summary().volumes,
            global.summary().source_files,
            global.summary().target_volumes,
            global.summary().anchors
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(
        rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
            .unwrap()
            .unwrap()
            .fingerprint(),
        global.fingerprint()
    );
    assert_eq!(mounted.index().to_xml().unwrap(), xml);
    let source_path =
        tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/empty-source").unwrap();
    let destination_path =
        tape_rs::managed_catalog::pool_catalog_path(f.pools[0], b"/destination/moved").unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &source_path)
        )
        .unwrap(),
        Some(inputs.baseline.source.clone())
    );
    assert!(
        rt.block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &destination_path)
        )
        .unwrap()
        .is_none()
    );
    if check_preparation {
        assert_original_preparation_not_recreated(
            &rt,
            &f,
            &manifest,
            &request,
            &mut projection,
            &mut view,
            volume,
        );
        drop(mounted);
        fixture_return_to_storage(&f.library, 71);
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    if publish {
        let execution = execution.map(|case| RenameExecutionTest {
            case,
            permission: permission.as_ref(),
            previous: previous.as_ref(),
            transport: &guarded,
            device: injected.as_ref(),
        });
        let (mounted, key) =
            publish_empty_rename((&rt, &f, &inputs), &global, view, mounted, &dev, execution);
        drop(mounted);
        tape_rs::scsi::reservation::release_and_unregister(&dev, key).unwrap();
        fixture_return_to_storage(&f.library, 71);
        fs::remove_dir_all(f.root).unwrap();
        return;
    }
    drop(mounted);
    fixture_return_to_storage(&f.library, 71);
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        12,
        request.request_epoch(),
        request.spec().operation,
    ))
    .unwrap();
    assert!(
        rt.block_on(f.catalog.node_rename_global_plan(f.route, 12, &manifest))
            .is_err()
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[derive(Clone, Copy)]
struct RenameAttemptContext<'a> {
    rt: &'a tokio::runtime::Runtime,
    f: &'a Fixture,
    manifest: &'a tape_rs::managed_catalog::NodeRenameManifest,
    request: &'a NodeRenameRequest,
    inputs: &'a tape_rs::managed_catalog::PreparedNodeRenameInputs,
    db: &'a foundationdb::Database,
    saved: &'a tape_rs::managed_catalog::NodeRenameVolumePlan,
    source_copy: &'a tape_rs::managed_catalog::PhysicalCopy,
    trace: &'a Mutex<Vec<Vec<u8>>>,
    commands: usize,
}
fn exercise_rename_attempt_preparation(context: RenameAttemptContext<'_>, writing: bool) {
    let RenameAttemptContext {
        rt,
        f,
        manifest,
        request,
        inputs,
        db,
        saved,
        source_copy,
        trace,
        commands,
    } = context;
    use tape_rs::managed_catalog::NodeRenameAttemptPhase;
    let global = rt
        .block_on(f.catalog.node_rename_global_plan(f.route, 13, manifest))
        .unwrap()
        .unwrap();
    let original_id = Uuid::new_v4();
    let mut attempt = rt
        .block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            original_id,
        ))
        .unwrap();
    let original_fp = attempt.fingerprint();
    assert_eq!(attempt.phase(), NodeRenameAttemptPhase::Preparing);
    assert_eq!(attempt.original_owner_epoch(), 13);
    assert_eq!(
        rt.block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            original_id
        ))
        .unwrap()
        .fingerprint(),
        original_fp
    );
    assert!(
        rt.block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            Uuid::new_v4()
        ))
        .is_err()
    );
    let mut lost_reply = attempt.clone();
    let intents = rt
        .block_on(
            f.catalog
                .next_node_rename_attempt_preparation_page(f.route, 13, &mut attempt),
        )
        .unwrap();
    assert_eq!(intents.len(), 2);
    assert_eq!(attempt.phase(), NodeRenameAttemptPhase::Ready);
    assert_eq!(
        (attempt.prepared_pages(), attempt.prepared_volumes()),
        (1, 2)
    );
    assert_eq!(attempt.fingerprint(), original_fp);
    assert_eq!(
        rt.block_on(f.catalog.next_node_rename_attempt_preparation_page(
            f.route,
            13,
            &mut lost_reply
        ))
        .unwrap(),
        intents,
        "reply loss returns the original page and original IDs"
    );
    assert!(
        rt.block_on(
            f.catalog
                .next_node_rename_attempt_preparation_page(f.route, 13, &mut attempt)
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_prepared_volume_intents(f.route, 13, &attempt, 0)
        )
        .unwrap(),
        intents
    );
    let source_intent = intents
        .iter()
        .find(|intent| intent.volume() == source_copy.volume_id)
        .unwrap();
    assert_eq!(source_intent.group(), original_id);
    assert_eq!(
        source_intent.media_plan().recipe().fingerprint(),
        saved.fingerprint()
    );
    let mut slot = b"tape-rs/catalog/22/@/".to_vec();
    slot.extend_from_slice(f.route.route.namespace.as_bytes());
    slot.extend_from_slice(b"Qv");
    slot.extend_from_slice(source_copy.volume_id.as_bytes());
    let raw = rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&slot, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec()
    });
    assert_eq!(&raw[..2], &[1, 1]);
    assert!(
        matches!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                13,
                source_copy.volume_id
            )),
            Err(CatalogError::PoolBusy)
        ),
        "rename cannot be returned as a single-node plan"
    );
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&slot, &raw[2..]);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            13,
            source_copy.volume_id
        ))
        .is_err()
    );
    assert_eq!(
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(&slot, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec()
        }),
        raw[2..]
    );
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear(&slot);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(
            f.catalog
                .node_rename_prepared_volume_intents(f.route, 13, &attempt, 0)
        )
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            13,
            source_copy.volume_id
        ))
        .is_err(),
        "missing Qv cannot make a captured volume free"
    );
    assert!(rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&slot, false)
            .await
            .unwrap()
            .is_none()
    }));
    let mut intent_pointer = b"tape-rs/catalog/22/@/".to_vec();
    intent_pointer.extend_from_slice(f.route.route.namespace.as_bytes());
    intent_pointer.extend_from_slice(b"QM");
    intent_pointer.extend_from_slice(f.serial.as_bytes());
    intent_pointer.extend_from_slice(&[0, 0]);
    intent_pointer.extend_from_slice(&request.request_epoch().to_be_bytes());
    intent_pointer.extend_from_slice(request.spec().operation.as_bytes());
    intent_pointer.push(7);
    intent_pointer.extend_from_slice(original_id.as_bytes());
    intent_pointer.extend_from_slice(source_copy.volume_id.as_bytes());
    let original_pointer = rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&intent_pointer, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec()
    });
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear(&intent_pointer);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            13,
            source_copy.volume_id
        ))
        .is_err(),
        "the original ready page detects loss of both reverse indexes"
    );
    assert!(
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err(),
        "missing claims cannot be repaired or ignored during cancellation"
    );
    assert_eq!(
        rt.block_on(f.catalog.node_rename_prepared_attempt(f.route, 13, &global))
            .unwrap()
            .unwrap()
            .phase(),
        NodeRenameAttemptPhase::Ready
    );
    assert!(rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&slot, false)
            .await
            .unwrap()
            .is_none()
    }));
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&slot, &raw);
        tr.set(&intent_pointer, &original_pointer);
        tr.commit().await.unwrap();
    });
    let other = intents
        .iter()
        .find(|intent| intent.volume() != source_copy.volume_id)
        .unwrap();
    let mut other_slot = slot[..slot.len() - 16].to_vec();
    other_slot.extend_from_slice(other.volume().as_bytes());
    let other_raw = rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&other_slot, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec()
    });
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&slot, &other_raw);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err(),
        "foreign valid claim must not be released"
    );
    for key in [&slot, &other_slot] {
        assert_eq!(
            rt.block_on(async {
                db.create_trx()
                    .unwrap()
                    .get(key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .to_vec()
            }),
            other_raw
        );
    }
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&slot, &raw);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.reject_node_rename_task(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
            ""
        ))
        .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.node_rename_prepared_attempt(f.route, 13, &global))
            .unwrap()
            .unwrap()
            .phase(),
        NodeRenameAttemptPhase::Ready
    );
    let mut group_pointer = intent_pointer[..intent_pointer.len() - 33].to_vec();
    group_pointer.push(5);
    group_pointer.push(0);
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear(&group_pointer);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            Uuid::new_v4()
        ))
        .is_err(),
        "lost original group pointer must not select a fresh UUID"
    );
    assert!(rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&group_pointer, false)
            .await
            .unwrap()
            .is_none()
    }));
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&group_pointer, original_id.as_bytes());
        tr.commit().await.unwrap();
    });
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 14))
        .unwrap();
    assert!(
        rt.block_on(f.catalog.node_rename_prepared_attempt(f.route, 13, &global))
            .is_err()
    );
    let mut recovered = rt
        .block_on(f.catalog.node_rename_prepared_attempt(f.route, 14, &global))
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id(), original_id);
    assert_eq!(recovered.fingerprint(), original_fp);
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_prepared_volume_intents(f.route, 14, &recovered, 0)
        )
        .unwrap(),
        intents
    );
    assert!(
        rt.block_on(f.catalog.next_node_rename_attempt_preparation_page(
            f.route,
            14,
            &mut recovered
        ))
        .is_err(),
        "takeover cannot arm an old-owner physical context"
    );
    rt.block_on(
        f.catalog
            .close_prepared_node_rename_attempt(f.route, 14, &mut recovered),
    )
    .unwrap();
    assert_eq!(recovered.phase(), NodeRenameAttemptPhase::ClosedPrepared);
    assert_eq!(recovered.fingerprint(), original_fp);
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_prepared_volume_intents(f.route, 14, &recovered, 0)
        )
        .unwrap(),
        intents,
        "original closed intent history is retained"
    );
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            14,
            source_copy.volume_id
        ))
        .unwrap()
        .is_none()
    );
    assert!(
        rt.block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            14,
            &global,
            original_id
        ))
        .is_err()
    );
    let mut fresh = rt
        .block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            14,
            &global,
            Uuid::new_v4(),
        ))
        .unwrap();
    let fresh_intents = rt
        .block_on(
            f.catalog
                .next_node_rename_attempt_preparation_page(f.route, 14, &mut fresh),
        )
        .unwrap();
    for intent in &fresh_intents {
        let old = intents
            .iter()
            .find(|old| old.volume() == intent.volume())
            .unwrap();
        assert_ne!(intent.id(), old.id());
        assert_ne!(
            intent.media_plan().fingerprint(),
            old.media_plan().fingerprint()
        );
        assert_eq!(intent.media_plan().recipe(), old.media_plan().recipe());
        assert_eq!(intent.original_owner_epoch(), 14);
    }
    if writing {
        exercise_rename_writing(
            &RenameAttemptContext {
                rt,
                f,
                manifest,
                request,
                inputs,
                db,
                saved,
                source_copy,
                trace,
                commands,
            },
            &mut fresh,
            &fresh_intents,
        );
        return;
    }
    let source_path =
        tape_rs::managed_catalog::pool_catalog_path(f.pools[0], request.spec().from.as_bytes())
            .unwrap();
    let target_path =
        tape_rs::managed_catalog::pool_catalog_path(f.pools[0], request.spec().to.as_bytes())
            .unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &source_path)
        )
        .unwrap(),
        Some(inputs.baseline.source.clone())
    );
    assert_eq!(
        rt.block_on(
            f.catalog
                .resolve_path(f.route.route.namespace, &target_path)
        )
        .unwrap(),
        inputs.baseline.target.clone()
    );
    rt.block_on(f.catalog.cancel_node_rename_task(
        &f.serial,
        f.route,
        14,
        request.request_epoch(),
        request.spec().operation,
    ))
    .unwrap();
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            14,
            source_copy.volume_id
        ))
        .unwrap()
        .is_none()
    );
    assert!(rt.block_on(async {
        db.create_trx()
            .unwrap()
            .get(&slot, false)
            .await
            .unwrap()
            .is_none()
    }));
    assert_eq!(
        trace.lock().unwrap().len(),
        commands,
        "preparation and never-writing closure have no CDBs"
    );
}

#[test]
#[ignore = "需要隔离FDB；整组Writing原子准入/原记录接任恢复/拒绝重放与取消，无介质IO"]
fn rename_writing_admission_preserves_original_records_and_fences_takeover_replay() {
    rename_volume_component(false, true, true, true, false, false, false);
}

fn exercise_rename_writing(
    context: &RenameAttemptContext<'_>,
    attempt: &mut tape_rs::managed_catalog::NodeRenameAttempt,
    intents: &[tape_rs::managed_catalog::NodeRenameVolumeIntent],
) {
    use tape_rs::managed_catalog::{NodeMutationTaskState, NodeRenameAttemptPhase};
    let RenameAttemptContext {
        rt,
        f,
        manifest,
        request,
        db,
        trace,
        commands,
        ..
    } = context;
    let intent = &intents[0];
    let mut slot = b"tape-rs/catalog/22/@/".to_vec();
    slot.extend_from_slice(f.route.route.namespace.as_bytes());
    slot.extend_from_slice(b"Qv");
    slot.extend_from_slice(intent.volume().as_bytes());
    let read = |key: &[u8]| {
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(key, false)
                .await
                .unwrap()
                .map(|v| v.to_vec())
        })
    };
    let write = |key: &[u8], value: Option<&[u8]>| {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            match value {
                Some(v) => tr.set(key, v),
                None => tr.clear(key),
            }
            tr.commit().await.unwrap();
        })
    };
    let task = |owner| {
        rt.block_on(f.catalog.node_rename_task_request(
            &f.serial,
            f.route,
            owner,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap()
        .0
    };
    assert!(
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, intent)
        )
        .is_err()
    );
    let raw = read(&slot).unwrap();
    write(&slot, None);
    assert!(
        rt.block_on(f.catalog.admit_node_rename_attempt(f.route, 14, attempt))
            .is_err()
    );
    assert_eq!(task(14).state, NodeMutationTaskState::Prepared);
    assert_eq!(attempt.phase(), NodeRenameAttemptPhase::Ready);
    assert!(
        read(&slot).is_none(),
        "admission must not repair missing claims"
    );
    write(&slot, Some(&raw));
    let mut lost_reply = attempt.clone();
    rt.block_on(f.catalog.admit_node_rename_attempt(f.route, 14, attempt))
        .unwrap();
    rt.block_on(
        f.catalog
            .admit_node_rename_attempt(f.route, 14, &mut lost_reply),
    )
    .unwrap();
    assert_eq!(attempt.phase(), NodeRenameAttemptPhase::Writing);
    assert_eq!(attempt.id(), lost_reply.id());
    assert_eq!(attempt.fingerprint(), lost_reply.fingerprint());
    assert_eq!(task(14).state, NodeMutationTaskState::Writing);
    assert_eq!(task(14).active_attempt, Some(attempt.id()));
    for intent in intents {
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, intent),
        )
        .unwrap();
    }
    assert!(
        rt.block_on(f.catalog.node_mutation_attempt(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .node_rename_prepared_volume_intents(f.route, 14, attempt, 0)
        )
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .close_prepared_node_rename_attempt(f.route, 14, attempt)
        )
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err()
    );
    assert!(
        rt.block_on(f.catalog.reject_node_rename_task(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
            "已开始写入"
        ))
        .is_err()
    );
    assert_eq!(read(&slot).unwrap(), raw);
    assert_eq!(task(14).state, NodeMutationTaskState::Writing);
    // Both reverse indexes missing must still be detected from immutable ready pages.
    let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
    prefix.extend_from_slice(f.route.route.namespace.as_bytes());
    prefix.extend_from_slice(b"QM");
    prefix.extend_from_slice(f.serial.as_bytes());
    prefix.extend_from_slice(&[0, 0]);
    prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
    prefix.extend_from_slice(request.spec().operation.as_bytes());
    let mut pointer = prefix.clone();
    pointer.push(7);
    pointer.extend_from_slice(attempt.id().as_bytes());
    pointer.extend_from_slice(intent.volume().as_bytes());
    let old_pointer = read(&pointer).unwrap();
    write(&slot, None);
    write(&pointer, None);
    assert!(
        rt.block_on(f.catalog.pending_node_mutation_attempt(
            &f.serial,
            f.route,
            14,
            intent.volume()
        ))
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, intent)
        )
        .is_err()
    );
    assert!(read(&slot).is_none());
    assert!(read(&pointer).is_none());
    write(&slot, Some(&raw));
    write(&pointer, Some(&old_pointer));
    // The held Writing loader validates original headers; it never repairs a bad one.
    let mut header = prefix;
    header.push(0);
    let original_header = read(&header).unwrap();
    write(&header, Some(b"{}"));
    assert!(
        rt.block_on(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err()
    );
    assert_eq!(read(&header).unwrap(), b"{}");
    write(&header, Some(&original_header));
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 15))
        .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, intent)
        )
        .is_err()
    );
    let recovered_manifest = rt
        .block_on(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            15,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
    assert_eq!(recovered_manifest.summary(), manifest.summary());
    assert_eq!(
        recovered_manifest.creator_owner_epoch(),
        manifest.creator_owner_epoch()
    );
    let global = rt
        .block_on(
            f.catalog
                .node_rename_held_global_plan(f.route, 15, &recovered_manifest),
        )
        .unwrap();
    let mut recovered = rt
        .block_on(f.catalog.node_rename_attempt(f.route, 15, &global))
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id(), attempt.id());
    assert_eq!(recovered.original_owner_epoch(), 14);
    assert_eq!(recovered.fingerprint(), attempt.fingerprint());
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 15, &recovered, 0)
        )
        .unwrap(),
        intents
    );
    rt.block_on(
        f.catalog
            .admit_node_rename_attempt(f.route, 15, &mut recovered),
    )
    .unwrap();
    assert!(
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 15, &recovered, intent)
        )
        .is_err(),
        "readable original Writing data does not authorize a new owner to replay it"
    );
    assert!(
        rt.block_on(
            f.catalog
                .close_prepared_node_rename_attempt(f.route, 15, &mut recovered)
        )
        .is_err()
    );
    assert_eq!(read(&slot).unwrap(), raw);
    for (path, expected) in [
        (
            &request.spec().from,
            Some(context.inputs.baseline.source.clone()),
        ),
        (&request.spec().to, context.inputs.baseline.target.clone()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        assert_eq!(
            rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        trace.lock().unwrap().len(),
        *commands,
        "metadata admission/recovery performs no CDB"
    );
}

#[test]
#[ignore = "需要隔离FDB；仅Sim原多卷Writing与S4观察/提交失败接任/策略停写/独立历史校验"]
fn rename_s4_observation_recovers_original_multi_volume_results_without_namespace_publication() {
    rename_volume_component(false, true, false, false, true, false, false);
}
fn exercise_rename_observation(
    context: RenameAttemptContext<'_>,
    components: Vec<(
        &str,
        tape_rs::managed_catalog::NodeRenameVolumePlan,
        tape_rs::managed_catalog::NodeRenameVolumeProjection,
    )>,
) {
    use tape_rs::scsi::cdb::opcode;
    use tape_rs::scsi::reservation::{FenceOutcome, ReservationKey, fence, release_and_unregister};
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let RenameAttemptContext {
        rt,
        f,
        manifest,
        request,
        db,
        inputs,
        ..
    } = context;
    let global = rt
        .block_on(
            f.catalog
                .node_rename_held_global_plan(f.route, 13, manifest),
        )
        .unwrap();
    let mut attempt = rt
        .block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            Uuid::new_v4(),
        ))
        .unwrap();
    let intents = rt
        .block_on(
            f.catalog
                .next_node_rename_attempt_preparation_page(f.route, 13, &mut attempt),
        )
        .unwrap();
    assert_eq!(intents.len(), 2);
    rt.block_on(
        f.catalog
            .admit_node_rename_attempt(f.route, 13, &mut attempt),
    )
    .unwrap();
    let mut committed = Vec::new();
    let mut source_observation = None;
    for (position, (barcode, recipe, projection)) in components.into_iter().enumerate() {
        let intent = intents
            .iter()
            .find(|i| i.volume() == recipe.source_index().volume_uuid)
            .unwrap();
        assert_eq!(intent.media_plan().recipe(), &recipe);
        assert!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 13, intent))
                .unwrap()
                .is_none()
        );
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 13, &attempt, intent),
        )
        .unwrap();
        f.library.load_into_drive(barcode, 0).unwrap();
        let dev = f.library.drive_as(0, 71);
        let key = ReservationKey::new(71, 13);
        assert!(matches!(
            fence(&dev, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(key));
        volume
            .stage_node_rename(intent.media_plan(), &projection, 16 << 20)
            .unwrap();
        // The target has a real full DP S4 despite the IP commit error.
        if position == 1 {
            dev.inject(Fault {
                opcode: opcode::WRITE_6,
                after: 1,
                remaining: 1,
                action: FaultAction::CheckCondition {
                    sense_key: 3,
                    asc: 0x0c,
                    ascq: 0,
                },
            });
            let error = volume.commit().unwrap_err();
            assert!(
                matches!(error, tape_rs::error::TapeError::CommitFailed { stage: ref actual, .. } if actual == "index_partition")
            );
            assert!(
                volume
                    .node_rename_evidence(intent.media_plan(), 16 << 20)
                    .is_err()
            );
            dev.clear_faults();
            drop(volume);
            volume = LtfsVolume::mount(&dev).unwrap();
            volume.set_reservation_guard(Some(key));
        } else {
            volume.sync_batch().unwrap();
        }
        let evidence = volume
            .node_rename_evidence(intent.media_plan(), 16 << 20)
            .unwrap();
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, intent.volume())
            )
            .unwrap(),
            recipe.source_index()
        );
        if position == 0 {
            let other = intents
                .iter()
                .find(|i| i.volume() != intent.volume())
                .unwrap();
            assert!(
                rt.block_on(
                    f.catalog
                        .observe_node_rename_intent(f.route, 13, &attempt, other, &evidence)
                )
                .is_err()
            );
            let mut slot = b"tape-rs/catalog/22/@/".to_vec();
            slot.extend_from_slice(f.route.route.namespace.as_bytes());
            slot.extend_from_slice(b"Qv");
            slot.extend_from_slice(intent.volume().as_bytes());
            let claim = rt.block_on(async {
                let tr = db.create_trx().unwrap();
                let raw = tr.get(&slot, false).await.unwrap().unwrap().to_vec();
                tr.clear(&slot);
                tr.commit().await.unwrap();
                raw
            });
            assert!(
                rt.block_on(
                    f.catalog
                        .observe_node_rename_intent(f.route, 13, &attempt, intent, &evidence)
                )
                .is_err()
            );
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .observed_active_volume_index(&f.serial, intent.volume())
                )
                .unwrap(),
                recipe.source_index()
            );
            assert!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 13, intent))
                    .unwrap()
                    .is_none()
            );
            rt.block_on(async {
                let tr = db.create_trx().unwrap();
                assert!(tr.get(&slot, false).await.unwrap().is_none());
                tr.set(&slot, &claim);
                tr.commit().await.unwrap();
            });
            let saved = rt
                .block_on(
                    f.catalog
                        .observe_node_rename_intent(f.route, 13, &attempt, intent, &evidence),
                )
                .unwrap();
            assert_eq!(saved.observed_owner(), 13);
            assert_eq!(saved.file_count(), projection.stats().files);
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .observe_node_rename_intent(f.route, 13, &attempt, intent, &evidence)
                )
                .unwrap(),
                saved
            );
            source_observation = Some(saved);
        }
        committed.push((barcode, intent.clone(), evidence, projection.stats().files));
        drop(volume);
        release_and_unregister(&dev, key).unwrap();
        fixture_return_to_storage(&f.library, 71);
    }
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 14))
        .unwrap();
    let recovered_manifest = rt
        .block_on(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
    let recovered_global = rt
        .block_on(
            f.catalog
                .node_rename_held_global_plan(f.route, 14, &recovered_manifest),
        )
        .unwrap();
    let recovered = rt
        .block_on(
            f.catalog
                .node_rename_attempt(f.route, 14, &recovered_global),
        )
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id(), attempt.id());
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 14, &recovered, 0)
        )
        .unwrap(),
        intents
    );
    let mut observations = Vec::new();
    for (position, (barcode, intent, original_evidence, count)) in committed.into_iter().enumerate()
    {
        assert!(
            rt.block_on(f.catalog.observe_node_rename_intent(
                f.route,
                13,
                &attempt,
                &intent,
                &original_evidence
            ))
            .is_err()
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 14, &recovered, &intent)
            )
            .is_err()
        );
        // Later policy stops new writes, but cannot hide an already committed S4.
        let policy = rt
            .block_on(
                f.catalog
                    .active_resource_policy(&f.serial, f.route, "tape", barcode),
            )
            .unwrap();
        let window = rt
            .block_on(f.catalog.active_management_window(&f.serial, f.route))
            .unwrap();
        let change = tape_rs::managed_catalog::ManagementRequest::parse(json!({
            "version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),
            "epoch":window.accepting,"operation":Uuid::new_v4().to_string(),
            "change":{"kind":"tape","barcode":barcode,"pool":policy["pool"],"volume":policy["volume"],
                "revision":policy["revision"],"patch":{"online":false,"blacklisted":true}}
        })).unwrap();
        rt.block_on(f.catalog.admit_management(&f.serial, f.route, 14, &change))
            .unwrap();
        assert_eq!(
            rt.block_on(f.catalog.apply_management(
                &f.serial,
                f.route,
                14,
                change.epoch(),
                change.operation()
            ))
            .unwrap()["status"],
            "succeeded"
        );
        f.library.load_into_drive(barcode, 0).unwrap();
        let dev = f.library.drive_as(0, 71);
        let key = ReservationKey::new(71, 14);
        assert!(matches!(
            fence(&dev, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(key));
        let evidence = volume
            .node_rename_evidence(intent.media_plan(), 16 << 20)
            .unwrap();
        assert_eq!(evidence, original_evidence);
        let observed = rt
            .block_on(
                f.catalog
                    .observe_node_rename_intent(f.route, 14, &recovered, &intent, &evidence),
            )
            .unwrap();
        assert_eq!(observed.intent(), &intent);
        assert_eq!(observed.file_count(), count);
        assert_eq!(observed.current_index(), evidence.current_index());
        if position == 0 {
            assert_eq!(Some(&observed), source_observation.as_ref());
        } else {
            assert_eq!(observed.observed_owner(), 14);
        }
        assert_eq!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 14, &intent))
                .unwrap(),
            Some(observed.clone())
        );
        assert_eq!(
            rt.block_on(f.catalog.active_volume_file_count(
                &f.serial,
                f.route,
                14,
                intent.volume()
            ))
            .unwrap(),
            count
        );
        assert_eq!(
            rt.block_on(f.catalog.observed_active_volume_generation(
                &f.serial,
                intent.volume(),
                evidence.current_index().generation
            ))
            .unwrap(),
            Some(evidence.current_index())
        );
        assert!(
            rt.block_on(f.catalog.cancel_node_rename_task(
                &f.serial,
                f.route,
                14,
                request.request_epoch(),
                request.spec().operation
            ))
            .is_err()
        );
        observations.push(observed);
        drop(volume);
        release_and_unregister(&dev, key).unwrap();
        fixture_return_to_storage(&f.library, 71);
    }
    for (path, expected) in [
        (&request.spec().from, Some(inputs.baseline.source.clone())),
        (&request.spec().to, inputs.baseline.target.clone()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        assert_eq!(
            rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                .unwrap(),
            expected
        );
    }
    // Historical proof must survive removal of original plans and request chunks.
    let object = rt
        .block_on(f.catalog.node_rename_task_request(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap()
        .0
        .object;
    let mut object_prefix = b"tape-rs/catalog/22/@/".to_vec();
    object_prefix.extend_from_slice(object.namespace().as_bytes());
    object_prefix.push(b'b');
    object_prefix.extend_from_slice(&(object.library().len() as u16).to_be_bytes());
    object_prefix.extend_from_slice(object.library().as_bytes());
    object_prefix.extend_from_slice(&object.digest());
    object_prefix.extend_from_slice(&object.byte_len().to_be_bytes());
    let mut object_end = object_prefix.clone();
    object_end.push(255);
    let mut task_prefix = b"tape-rs/catalog/22/@/".to_vec();
    task_prefix.extend_from_slice(f.route.route.namespace.as_bytes());
    task_prefix.extend_from_slice(b"QM");
    task_prefix.extend_from_slice(f.serial.as_bytes());
    task_prefix.extend_from_slice(&[0, 0]);
    task_prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
    task_prefix.extend_from_slice(request.spec().operation.as_bytes());
    let mut end = task_prefix.clone();
    end.push(255);
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        let mut marker = object_prefix.clone();
        marker.push(0);
        assert!(tr.get(&marker, false).await.unwrap().is_some());
        tr.clear_range(&task_prefix, &end);
        tr.clear_range(&object_prefix, &object_end);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(f.catalog.node_rename_task_request(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
            8 << 20
        ))
        .is_err()
    );
    for observed in &observations {
        let intent = observed.intent();
        let current = observed.current_index();
        assert_eq!(
            rt.block_on(f.catalog.observed_active_volume_generation(
                &f.serial,
                intent.volume(),
                current.generation
            ))
            .unwrap(),
            Some(current)
        );
        assert_eq!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 14, intent))
                .unwrap()
                .as_ref(),
            Some(observed)
        );
        let mut history_key = b"tape-rs/catalog/22/r/".to_vec();
        history_key.extend_from_slice(intent.volume().as_bytes());
        history_key.extend_from_slice(&current.generation.to_be_bytes());
        let mut count_key = history_key.clone();
        count_key.push(b'f');
        for (key, wrong_kind) in [(&history_key, true), (&count_key, false)] {
            let original = rt.block_on(async {
                let tr = db.create_trx().unwrap();
                let raw = tr.get(key, false).await.unwrap().unwrap().to_vec();
                if wrong_kind {
                    assert_eq!(raw[0], 5);
                    let mut bad = raw.clone();
                    bad[0] = 3;
                    tr.set(key, &bad);
                } else {
                    tr.clear(key);
                }
                tr.commit().await.unwrap();
                raw
            });
            assert!(
                rt.block_on(f.catalog.observed_active_volume_generation(
                    &f.serial,
                    intent.volume(),
                    current.generation
                ))
                .is_err()
            );
            assert!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 14, intent))
                    .is_err()
            );
            rt.block_on(async {
                let tr = db.create_trx().unwrap();
                if wrong_kind {
                    assert_eq!(tr.get(key, false).await.unwrap().unwrap()[0], 3);
                } else {
                    assert!(tr.get(key, false).await.unwrap().is_none());
                }
                tr.set(key, &original);
                tr.commit().await.unwrap();
            });
        }
        let mut key = b"tape-rs/catalog/22/@/".to_vec();
        key.extend_from_slice(f.route.route.namespace.as_bytes());
        key.extend_from_slice(b"QR");
        key.extend_from_slice(intent.volume().as_bytes());
        key.extend_from_slice(&current.generation.to_be_bytes());
        let raw = rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(&key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec()
        });
        for bad in [None, Some(b"{}".as_slice())] {
            rt.block_on(async {
                let tr = db.create_trx().unwrap();
                match bad {
                    Some(v) => tr.set(&key, v),
                    None => tr.clear(&key),
                };
                tr.commit().await.unwrap();
            });
            assert!(
                rt.block_on(f.catalog.observed_active_volume_generation(
                    &f.serial,
                    intent.volume(),
                    current.generation
                ))
                .is_err()
            );
            assert!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 14, intent))
                    .is_err()
            );
            assert_eq!(
                rt.block_on(async {
                    db.create_trx()
                        .unwrap()
                        .get(&key, false)
                        .await
                        .unwrap()
                        .map(|v| v.to_vec())
                }),
                bad.map(|v| v.to_vec())
            );
        }
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&key, &raw);
            tr.commit().await.unwrap();
        });
        assert_eq!(
            rt.block_on(f.catalog.observed_active_volume_generation(
                &f.serial,
                intent.volume(),
                current.generation
            ))
            .unwrap(),
            Some(current)
        );
    }
}

#[test]
#[ignore = "需要隔离FDB；仅Sim原Writing逐卷Unchanged/Discard关闭/接任/共享占用及缺记录拒绝"]
fn rename_absence_closure_keeps_group_claims_and_blocks_replay_after_takeover() {
    rename_volume_component(false, true, false, false, false, true, false);
}

fn exercise_rename_closure(
    context: RenameAttemptContext<'_>,
    components: Vec<(
        &str,
        tape_rs::managed_catalog::NodeRenameVolumePlan,
        tape_rs::managed_catalog::NodeRenameVolumeProjection,
    )>,
    exercise_rebase: bool,
) -> Option<tape_rs::managed_catalog::NodeRenameAttempt> {
    use tape_rs::managed_catalog::{NodeMutationTaskState, NodeRenameAbsenceKind};
    use tape_rs::scsi::cdb::opcode;
    use tape_rs::scsi::reservation::{FenceOutcome, ReservationKey, fence, release_and_unregister};
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let RenameAttemptContext {
        rt,
        f,
        manifest,
        request,
        inputs,
        db,
        ..
    } = context;
    let global = rt
        .block_on(
            f.catalog
                .node_rename_held_global_plan(f.route, 13, manifest),
        )
        .unwrap();
    let mut attempt = rt
        .block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            13,
            &global,
            Uuid::new_v4(),
        ))
        .unwrap();
    let intents = rt
        .block_on(
            f.catalog
                .next_node_rename_attempt_preparation_page(f.route, 13, &mut attempt),
        )
        .unwrap();
    assert_eq!(intents.len(), 2);
    rt.block_on(
        f.catalog
            .admit_node_rename_attempt(f.route, 13, &mut attempt),
    )
    .unwrap();
    let read = |key: &[u8]| {
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(key, false)
                .await
                .unwrap()
                .map(|v| v.to_vec())
        })
    };
    let write = |key: &[u8], bytes: Option<&[u8]>| {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            match bytes {
                Some(v) => tr.set(key, v),
                None => tr.clear(key),
            };
            tr.commit().await.unwrap();
        })
    };
    let mut closed_results = Vec::new();
    for (position, (barcode, recipe, projection)) in components.into_iter().enumerate() {
        let intent = intents
            .iter()
            .find(|i| i.volume() == recipe.source_index().volume_uuid)
            .unwrap();
        let mut slot = b"tape-rs/catalog/22/@/".to_vec();
        slot.extend_from_slice(f.route.route.namespace.as_bytes());
        slot.extend_from_slice(b"Qv");
        slot.extend_from_slice(intent.volume().as_bytes());
        let open_claim = read(&slot).unwrap();
        assert!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 13, intent))
                .unwrap()
                .is_none()
        );
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 13, &attempt, intent),
        )
        .unwrap();
        f.library.load_into_drive(barcode, 0).unwrap();
        let dev = f.library.drive_as(0, 71);
        let key = ReservationKey::new(71, 13);
        assert!(matches!(
            fence(&dev, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(key));
        if position == 1 {
            volume
                .stage_node_rename(intent.media_plan(), &projection, 16 << 20)
                .unwrap();
            dev.inject(Fault {
                opcode: opcode::WRITE_FILEMARKS_6,
                after: 1,
                remaining: 1,
                action: FaultAction::CheckCondition {
                    sense_key: 3,
                    asc: 0x0c,
                    ascq: 0,
                },
            });
            let error = volume.commit().unwrap_err();
            assert!(
                matches!(error,tape_rs::error::TapeError::CommitFailed{stage:ref s,..}if s=="closing_fm")
            );
            assert!(
                volume
                    .node_rename_absence_evidence(intent.media_plan(), 16 << 20)
                    .is_err()
            );
            dev.clear_faults();
            drop(volume);
            volume = LtfsVolume::mount(&dev).unwrap();
            volume.set_reservation_guard(Some(key));
            assert!(
                volume
                    .node_rename_absence_evidence(intent.media_plan(), 16 << 20)
                    .is_err()
            );
        }
        let proof = if position == 0 {
            volume
                .node_rename_absence_evidence(intent.media_plan(), 16 << 20)
                .unwrap()
        } else {
            volume
                .discard_node_rename_tail(intent.media_plan(), 16 << 20)
                .unwrap()
        };
        assert_eq!(
            proof.kind(),
            if position == 0 {
                NodeRenameAbsenceKind::Unchanged
            } else {
                NodeRenameAbsenceKind::Discarded
            }
        );
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, intent.volume())
            )
            .unwrap(),
            recipe.source_index()
        );
        let result = rt
            .block_on(
                f.catalog
                    .close_node_rename_intent(f.route, 13, &attempt, intent, &proof),
            )
            .unwrap();
        assert_eq!(result.intent(), intent);
        assert_eq!(result.current_index(), proof.current_index());
        assert_eq!(result.kind(), proof.kind());
        assert_eq!(result.file_count(), proof.file_count());
        assert_eq!(result.closed_owner(), 13);
        assert_eq!(
            rt.block_on(
                f.catalog
                    .close_node_rename_intent(f.route, 13, &attempt, intent, &proof)
            )
            .unwrap(),
            result
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 13, &attempt, intent)
            )
            .is_err(),
            "the same original owner cannot replay a closed T0 plan"
        );
        assert!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 13, intent))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                13,
                intent.volume()
            )),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, intent.volume())
            )
            .unwrap(),
            proof.current_index()
        );
        let closed_claim = read(&slot).unwrap();
        assert_ne!(closed_claim, open_claim);
        let mut closure_key = b"tape-rs/catalog/22/@/".to_vec();
        closure_key.extend_from_slice(f.route.route.namespace.as_bytes());
        closure_key.extend_from_slice(b"QC");
        closure_key.extend_from_slice(intent.volume().as_bytes());
        closure_key.extend_from_slice(intent.id().as_bytes());
        let record = read(&closure_key).unwrap();
        write(&closure_key, None);
        assert!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 13, intent))
                .is_err()
        );
        assert!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 13, intent))
                .is_err()
        );
        assert!(
            rt.block_on(
                f.catalog
                    .close_node_rename_intent(f.route, 13, &attempt, intent, &proof)
            )
            .is_err()
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 13, &attempt, intent)
            )
            .is_err()
        );
        assert!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                13,
                intent.volume()
            ))
            .is_err()
        );
        assert!(read(&closure_key).is_none());
        assert_eq!(read(&slot).unwrap(), closed_claim);
        write(&closure_key, Some(&record));
        write(&slot, Some(&open_claim));
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 13, &attempt, intent)
            )
            .is_err()
        );
        assert!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 13, intent))
                .is_err()
        );
        assert_eq!(read(&slot).unwrap(), open_claim);
        write(&slot, Some(&closed_claim));
        assert_eq!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 13, intent))
                .unwrap(),
            Some(result.clone())
        );
        closed_results.push((intent.clone(), proof, result));
        drop(volume);
        release_and_unregister(&dev, key).unwrap();
        fixture_return_to_storage(&f.library, 71);
    }
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 13, &attempt, 0)
        )
        .unwrap(),
        intents
    );
    let task = rt
        .block_on(f.catalog.node_rename_task_request(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
            8 << 20,
        ))
        .unwrap()
        .0;
    assert_eq!(task.state, NodeMutationTaskState::Writing);
    assert!(
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err()
    );
    assert!(
        rt.block_on(
            f.catalog
                .close_prepared_node_rename_attempt(f.route, 13, &mut attempt)
        )
        .is_err()
    );
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 14))
        .unwrap();
    for (intent, proof, result) in &closed_results {
        assert_eq!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 14, intent))
                .unwrap()
                .as_ref(),
            Some(result)
        );
        assert_eq!(
            rt.block_on(
                f.catalog
                    .close_node_rename_intent(f.route, 14, &attempt, intent, proof)
            )
            .unwrap(),
            *result
        );
    }
    for (path, expected) in [
        (&request.spec().from, Some(inputs.baseline.source.clone())),
        (&request.spec().to, inputs.baseline.target.clone()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        assert_eq!(
            rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                .unwrap(),
            expected
        );
    }
    if exercise_rebase {
        return Some(exercise_rename_rebase(context, &attempt, &closed_results));
    }
    let mut original = b"tape-rs/catalog/22/@/".to_vec();
    original.extend_from_slice(f.route.route.namespace.as_bytes());
    original.extend_from_slice(b"QM");
    original.extend_from_slice(f.serial.as_bytes());
    original.extend_from_slice(&[0, 0]);
    original.extend_from_slice(&request.request_epoch().to_be_bytes());
    original.extend_from_slice(request.spec().operation.as_bytes());
    let mut end = original.clone();
    end.push(255);
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear_range(&original, &end);
        tr.commit().await.unwrap();
    });
    for (intent, _, result) in &closed_results {
        assert_eq!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 14, intent))
                .unwrap()
                .as_ref(),
            Some(result)
        );
        if result.kind() == NodeRenameAbsenceKind::Discarded {
            assert_eq!(
                rt.block_on(f.catalog.observed_active_volume_generation(
                    &f.serial,
                    intent.volume(),
                    result.current_index().generation
                ))
                .unwrap(),
                Some(result.current_index())
            );
        }
    }
    None
}

#[test]
#[ignore = "需要隔离FDB；仅Sim原闭卷重试/当前owner新UUID/保留成功卷/原谱系恢复及禁止UUID复用"]
fn rename_rebase_keeps_original_group_and_successes_while_advancing_only_closed_intents() {
    rename_volume_component(false, true, false, false, false, true, true);
}
fn exercise_rename_rebase(
    context: RenameAttemptContext<'_>,
    attempt: &tape_rs::managed_catalog::NodeRenameAttempt,
    closed_results: &[(
        tape_rs::managed_catalog::NodeRenameVolumeIntent,
        tape_rs::managed_catalog::NodeRenameAbsenceEvidence,
        tape_rs::managed_catalog::NodeRenameIntentClosure,
    )],
) -> tape_rs::managed_catalog::NodeRenameAttempt {
    use tape_rs::scsi::reservation::{FenceOutcome, ReservationKey, fence, release_and_unregister};
    let RenameAttemptContext {
        rt,
        f,
        manifest,
        request,
        inputs,
        db,
        source_copy,
        ..
    } = context;
    let held = rt
        .block_on(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
    let mut canonical = rt
        .block_on(
            f.catalog
                .build_node_rename_projection_spool(f.route, 14, &held, 16 << 20),
        )
        .unwrap();
    let global = rt
        .block_on(f.catalog.node_rename_held_global_plan(f.route, 14, &held))
        .unwrap();
    let original_global = global.fingerprint();
    let mut successes = Vec::new();
    for (position, (old, _, closed)) in closed_results.iter().enumerate() {
        let barcode = if old.volume() == source_copy.volume_id {
            BARCODES[0]
        } else {
            BARCODES[1]
        };
        f.library.load_into_drive(barcode, 0).unwrap();
        let dev = f.library.drive_as(0, 71);
        let key = ReservationKey::new(71, 14);
        assert!(matches!(
            fence(&dev, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(key));
        let mut guard = rt
            .block_on(f.catalog.build_node_rename_volume_guard(
                f.route,
                14,
                &held,
                &mut canonical,
                old.volume(),
                16 << 20,
            ))
            .unwrap();
        let mut physical = guard
            .physical_source(&inputs.baseline, &mut canonical, volume.index(), 16 << 20)
            .unwrap();
        let mut desired = rt
            .block_on(f.catalog.build_node_rename_volume_projection(
                f.route,
                14,
                &held,
                &mut canonical,
                &mut physical,
                16 << 20,
            ))
            .unwrap();
        let id = Uuid::new_v4();
        let mut current = rt
            .block_on(f.catalog.admit_node_rename_rebased_intent(
                f.route,
                14,
                attempt,
                closed,
                &mut desired,
                id,
            ))
            .unwrap();
        if position == 0 {
            let (_, _, foreign) = &closed_results[1];
            assert!(
                rt.block_on(f.catalog.admit_node_rename_rebased_intent(
                    f.route,
                    14,
                    attempt,
                    foreign,
                    &mut desired,
                    Uuid::new_v4()
                ))
                .is_err()
            );
            let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
            prefix.extend_from_slice(f.route.route.namespace.as_bytes());
            prefix.extend_from_slice(b"QM");
            prefix.extend_from_slice(f.serial.as_bytes());
            prefix.extend_from_slice(&[0, 0]);
            prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
            prefix.extend_from_slice(request.spec().operation.as_bytes());
            let mut recipe = prefix.clone();
            recipe.push(9);
            recipe.extend_from_slice(attempt.id().as_bytes());
            recipe.extend_from_slice(old.volume().as_bytes());
            recipe.extend_from_slice(id.as_bytes());
            let mut pointer = prefix;
            pointer.push(7);
            pointer.extend_from_slice(attempt.id().as_bytes());
            pointer.extend_from_slice(old.volume().as_bytes());
            let mut parent = b"tape-rs/catalog/22/@/".to_vec();
            parent.extend_from_slice(f.route.route.namespace.as_bytes());
            parent.extend_from_slice(b"QC");
            parent.extend_from_slice(old.volume().as_bytes());
            parent.extend_from_slice(old.id().as_bytes());
            let mut nonce = b"tape-rs/catalog/22/@/".to_vec();
            nonce.extend_from_slice(f.route.route.namespace.as_bytes());
            nonce.extend_from_slice(b"QI");
            nonce.extend_from_slice(old.volume().as_bytes());
            nonce.extend_from_slice(id.as_bytes());
            for key in [recipe, pointer, parent, nonce] {
                let raw = rt.block_on(async {
                    let tr = db.create_trx().unwrap();
                    let raw = tr.get(&key, false).await.unwrap().unwrap().to_vec();
                    tr.clear(&key);
                    tr.commit().await.unwrap();
                    raw
                });
                assert!(
                    rt.block_on(
                        f.catalog
                            .node_rename_volume_intents(f.route, 14, attempt, 0)
                    )
                    .is_err()
                );
                assert!(
                    rt.block_on(
                        f.catalog
                            .check_node_rename_write_intent(f.route, 14, attempt, &current)
                    )
                    .is_err()
                );
                assert!(
                    rt.block_on(f.catalog.admit_node_rename_rebased_intent(
                        f.route,
                        14,
                        attempt,
                        closed,
                        &mut desired,
                        id
                    ))
                    .is_err()
                );
                rt.block_on(async {
                    let tr = db.create_trx().unwrap();
                    assert!(tr.get(&key, false).await.unwrap().is_none());
                    tr.set(&key, &raw);
                    tr.commit().await.unwrap();
                });
            }
        }
        assert_eq!(current.id(), id);
        assert_eq!(current.group(), attempt.id());
        assert_eq!(current.original_owner_epoch(), 14);
        assert_ne!(current.id(), old.id());
        assert_ne!(
            current.media_plan().fingerprint(),
            old.media_plan().fingerprint()
        );
        assert_eq!(
            current.media_plan().recipe().source_index(),
            closed.current_index()
        );
        assert_eq!(
            rt.block_on(f.catalog.admit_node_rename_rebased_intent(
                f.route,
                14,
                attempt,
                closed,
                &mut desired,
                id
            ))
            .unwrap(),
            current
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 14, attempt, old)
            )
            .is_err()
        );
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, &current),
        )
        .unwrap();
        assert!(
            rt.block_on(f.catalog.admit_node_rename_rebased_intent(
                f.route,
                14,
                attempt,
                closed,
                &mut desired,
                Uuid::new_v4()
            ))
            .is_err()
        );
        let root_recipe = rt
            .block_on(
                f.catalog
                    .node_rename_volume_plan(f.route, 14, &held, old.volume()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(&root_recipe, old.media_plan().recipe());
        if position == 1 {
            // Repeated T0 closures stay on the original ready-page anchor.
            let first = current.clone();
            let proof = volume
                .node_rename_absence_evidence(current.media_plan(), 16 << 20)
                .unwrap();
            let parent = rt
                .block_on(
                    f.catalog
                        .close_node_rename_intent(f.route, 14, attempt, &current, &proof),
                )
                .unwrap();
            current = rt
                .block_on(f.catalog.admit_node_rename_rebased_intent(
                    f.route,
                    14,
                    attempt,
                    &parent,
                    &mut desired,
                    Uuid::new_v4(),
                ))
                .unwrap();
            let proof = volume
                .node_rename_absence_evidence(current.media_plan(), 16 << 20)
                .unwrap();
            let parent = rt
                .block_on(
                    f.catalog
                        .close_node_rename_intent(f.route, 14, attempt, &current, &proof),
                )
                .unwrap();
            let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
            prefix.extend_from_slice(f.route.route.namespace.as_bytes());
            prefix.extend_from_slice(b"QM");
            prefix.extend_from_slice(f.serial.as_bytes());
            prefix.extend_from_slice(&[0, 0]);
            prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
            prefix.extend_from_slice(request.spec().operation.as_bytes());
            let mut removed = Vec::new();
            for tag in [6, 9] {
                let mut key = prefix.clone();
                key.push(tag);
                key.extend_from_slice(attempt.id().as_bytes());
                key.extend_from_slice(first.volume().as_bytes());
                key.extend_from_slice(first.id().as_bytes());
                rt.block_on(async {
                    let tr = db.create_trx().unwrap();
                    assert!(tr.get(&key, false).await.unwrap().is_some());
                    tr.clear(&key);
                    tr.commit().await.unwrap();
                });
                removed.push(key);
            }
            assert!(
                rt.block_on(f.catalog.admit_node_rename_rebased_intent(
                    f.route,
                    14,
                    attempt,
                    &parent,
                    &mut desired,
                    first.id()
                ))
                .is_err(),
                "a retained physical nonce cannot be reused after old plan rows disappear"
            );
            for key in removed {
                assert!(rt.block_on(async {
                    db.create_trx()
                        .unwrap()
                        .get(&key, false)
                        .await
                        .unwrap()
                        .is_none()
                }));
            }
            current = rt
                .block_on(f.catalog.admit_node_rename_rebased_intent(
                    f.route,
                    14,
                    attempt,
                    &parent,
                    &mut desired,
                    Uuid::new_v4(),
                ))
                .unwrap();
            assert!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 14, &first))
                    .unwrap()
                    .is_none()
            );
        }
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 14, attempt, &current),
        )
        .unwrap();
        volume
            .stage_node_rename(current.media_plan(), &desired, 16 << 20)
            .unwrap();
        volume.sync_batch().unwrap();
        let evidence = volume
            .node_rename_evidence(current.media_plan(), 16 << 20)
            .unwrap();
        let saved = rt
            .block_on(
                f.catalog
                    .observe_node_rename_intent(f.route, 14, attempt, &current, &evidence),
            )
            .unwrap();
        assert_eq!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 14, old))
                .unwrap(),
            None
        );
        assert_eq!(
            rt.block_on(f.catalog.node_rename_intent_closure(f.route, 14, old))
                .unwrap()
                .as_ref(),
            Some(closed)
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 14, attempt, &current)
            )
            .is_err(),
            "already observed S4 cannot be appended again"
        );
        successes.push((current, saved));
        if successes.len() < closed_results.len() {
            require_incomplete_rename_seal(context, attempt);
        }
        for (intent, saved) in &successes {
            assert_eq!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 14, intent))
                    .unwrap()
                    .as_ref(),
                Some(saved)
            );
        }
        assert_eq!(
            rt.block_on(f.catalog.node_rename_held_global_plan(f.route, 14, &held))
                .unwrap()
                .fingerprint(),
            original_global
        );
        drop(volume);
        release_and_unregister(&dev, key).unwrap();
        fixture_return_to_storage(&f.library, 71);
    }
    let current = rt
        .block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 14, attempt, 0),
        )
        .unwrap();
    for (intent, _) in &successes {
        assert!(current.contains(intent));
    }
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 15))
        .unwrap();
    let held = rt
        .block_on(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            15,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap();
    let global = rt
        .block_on(f.catalog.node_rename_held_global_plan(f.route, 15, &held))
        .unwrap();
    let recovered = rt
        .block_on(f.catalog.node_rename_attempt(f.route, 15, &global))
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id(), attempt.id());
    assert_eq!(recovered.original_owner_epoch(), 13);
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 15, &recovered, 0)
        )
        .unwrap(),
        current
    );
    for (intent, saved) in &successes {
        assert_eq!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 15, intent))
                .unwrap()
                .as_ref(),
            Some(saved)
        );
        assert!(
            rt.block_on(
                f.catalog
                    .check_node_rename_write_intent(f.route, 15, &recovered, intent)
            )
            .is_err()
        );
    }
    for (path, expected) in [
        (&request.spec().from, Some(inputs.baseline.source.clone())),
        (&request.spec().to, inputs.baseline.target.clone()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        assert_eq!(
            rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                .unwrap(),
            expected
        );
    }
    assert_eq!(manifest.summary(), held.summary());
    exercise_rename_seal(context, attempt, recovered, &successes, &current)
}

#[test]
#[ignore = "需要隔离FDB；仅Sim逐卷S4完整封口、证据缺失不修复、接任幂等及保持namespace/hold"]
fn rename_all_s4_seal_keeps_original_participants_and_fences_further_media_changes() {
    rename_volume_component(false, true, false, false, false, true, true);
}

fn seal_rename_s4s(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    owner: u64,
    attempt: &mut tape_rs::managed_catalog::NodeRenameAttempt,
) -> std::result::Result<(), tape_rs::managed_catalog::CatalogError> {
    rt.block_on(Box::pin(
        f.catalog.seal_node_rename_s4s(f.route, owner, attempt),
    ))
}

fn require_incomplete_rename_seal(
    context: RenameAttemptContext<'_>,
    attempt: &tape_rs::managed_catalog::NodeRenameAttempt,
) {
    let RenameAttemptContext { rt, f, request, .. } = context;
    let mut incomplete = attempt.clone();
    assert!(seal_rename_s4s(rt, f, 14, &mut incomplete).is_err());
    let held = rt
        .block_on(Box::pin(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            14,
            request.request_epoch(),
            request.spec().operation,
        )))
        .unwrap();
    let global = rt
        .block_on(Box::pin(
            f.catalog.node_rename_held_global_plan(f.route, 14, &held),
        ))
        .unwrap();
    assert_eq!(
        rt.block_on(f.catalog.node_rename_attempt(f.route, 14, &global))
            .unwrap()
            .unwrap()
            .phase(),
        tape_rs::managed_catalog::NodeRenameAttemptPhase::Writing
    );
}
fn exercise_rename_seal(
    context: RenameAttemptContext<'_>,
    attempt: &tape_rs::managed_catalog::NodeRenameAttempt,
    recovered: tape_rs::managed_catalog::NodeRenameAttempt,
    successes: &[(
        tape_rs::managed_catalog::NodeRenameVolumeIntent,
        tape_rs::managed_catalog::NodeRenameObservation,
    )],
    current: &[tape_rs::managed_catalog::NodeRenameVolumeIntent],
) -> tape_rs::managed_catalog::NodeRenameAttempt {
    let RenameAttemptContext {
        rt,
        f,
        request,
        inputs,
        db,
        ..
    } = context;
    let mut sealed = recovered.clone();
    for (intent, _) in successes {
        let mut key = b"tape-rs/catalog/22/@/".to_vec();
        key.extend_from_slice(f.route.route.namespace.as_bytes());
        key.extend_from_slice(b"QR");
        key.extend_from_slice(intent.volume().as_bytes());
        key.extend_from_slice(
            &intent
                .media_plan()
                .recipe()
                .source_index()
                .generation
                .checked_add(1)
                .unwrap()
                .to_be_bytes(),
        );
        let raw = rt.block_on(async {
            let tr = db.create_trx().unwrap();
            let raw = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            tr.clear(&key);
            tr.commit().await.unwrap();
            raw
        });
        assert!(
            seal_rename_s4s(rt, f, 15, &mut sealed).is_err(),
            "missing one participant S4 must never seal"
        );
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            assert!(tr.get(&key, false).await.unwrap().is_none());
            tr.set(&key, &raw);
            tr.commit().await.unwrap();
        });
    }
    assert!(seal_rename_s4s(rt, f, 14, &mut sealed).is_err());
    seal_rename_s4s(rt, f, 15, &mut sealed).unwrap();
    assert_eq!(
        sealed.phase(),
        tape_rs::managed_catalog::NodeRenameAttemptPhase::Publishing
    );
    assert_eq!(sealed.id(), attempt.id());
    assert_eq!(sealed.fingerprint(), attempt.fingerprint());
    assert_eq!(sealed.progress_revision(), recovered.progress_revision());
    assert!(sealed.progress_revision() > 1);
    seal_rename_s4s(rt, f, 15, &mut sealed).unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_volume_intents(f.route, 15, &sealed, 0)
        )
        .unwrap(),
        current.to_vec()
    );
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 16))
        .unwrap();
    let mut after_takeover = recovered;
    seal_rename_s4s(rt, f, 16, &mut after_takeover).unwrap();
    assert_eq!(after_takeover.phase(), sealed.phase());
    assert_eq!(
        after_takeover.progress_revision(),
        sealed.progress_revision()
    );
    assert!(
        rt.block_on(f.catalog.cancel_node_rename_task(
            &f.serial,
            f.route,
            16,
            request.request_epoch(),
            request.spec().operation
        ))
        .is_err()
    );
    for (intent, saved) in successes {
        assert_eq!(
            rt.block_on(f.catalog.node_rename_observation(f.route, 16, intent))
                .unwrap()
                .as_ref(),
            Some(saved)
        );
        assert!(
            rt.block_on(f.catalog.check_node_rename_write_intent(
                f.route,
                16,
                &after_takeover,
                intent
            ))
            .is_err()
        );
    }
    for (path, expected) in [
        (&request.spec().from, Some(inputs.baseline.source.clone())),
        (&request.spec().to, inputs.baseline.target.clone()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        assert_eq!(
            rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                .unwrap(),
            expected
        );
    }
    after_takeover
}
#[test]
#[ignore = "需要隔离FDB；仅Sim多卷原S4、保留池hold逐页释放、文件替换原子发布与独立回执"]
fn rename_publication_preserves_source_data_and_recovers_original_two_path_result() {
    rename_volume_component(false, true, false, false, false, true, true);
}

fn publish_rename(
    context: RenameAttemptContext<'_>,
    owner: u64,
    attempt: &tape_rs::managed_catalog::NodeRenameAttempt,
) -> std::result::Result<tape_rs::managed_catalog::NodeRenamePublication, CatalogError> {
    context
        .rt
        .block_on(Box::pin(context.f.catalog.publish_node_rename_attempt(
            context.f.route,
            owner,
            attempt,
            8 << 20,
        )))
}
fn exercise_rename_publication(
    context: RenameAttemptContext<'_>,
    sealed: &tape_rs::managed_catalog::NodeRenameAttempt,
) {
    let RenameAttemptContext {
        rt,
        f,
        request,
        inputs,
        db,
        ..
    } = context;
    let ns = f.route.route.namespace;
    let read = |key: &[u8]| {
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(key, false)
                .await
                .unwrap()
                .map(|r| r.to_vec())
        })
    };
    let set = |key: &[u8], raw: Option<&[u8]>| {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            if let Some(raw) = raw {
                tr.set(key, raw);
            } else {
                tr.clear(key);
            }
            tr.commit().await.unwrap();
        })
    };
    let inode_key = |tag: u8, node: Uuid| {
        let mut key = b"tape-rs/catalog/22/".to_vec();
        key.push(tag);
        key.push(b'/');
        key.extend_from_slice(ns.as_bytes());
        key.extend_from_slice(node.as_bytes());
        key
    };
    let (source_id, old_head, old_copy) = match &inputs.baseline.source {
        ResolvedPath::File { id, head, copy, .. } => (*id, head, copy),
        _ => panic!("file replacement fixture"),
    };
    let target_id = request.spec().target.unwrap().inode;
    let target_head_key = inode_key(b'H', target_id);
    let target_head = read(&target_head_key).unwrap();
    let source_node_key = inode_key(b'N', source_id);
    let source_node_before = read(&source_node_key).unwrap();
    let mut old_copy_key = inode_key(b'R', source_id);
    old_copy_key.extend_from_slice(old_head.copy_id.as_bytes());
    let old_copy_raw = read(&old_copy_key).unwrap();
    assert!(
        publish_rename(context, 16, sealed).is_err(),
        "cannot publish with participant claims still held"
    );
    assert_eq!(read(&source_node_key).unwrap(), source_node_before);
    let intents = rt
        .block_on(f.catalog.node_rename_volume_intents(f.route, 16, sealed, 0))
        .unwrap();
    let mut slots = Vec::new();
    for intent in &intents {
        let mut key = b"tape-rs/catalog/22/@/".to_vec();
        key.extend_from_slice(ns.as_bytes());
        key.extend_from_slice(b"Qv");
        key.extend_from_slice(intent.volume().as_bytes());
        slots.push((key.clone(), read(&key).unwrap()));
    }
    let mut releasing = sealed.clone();
    set(&slots[0].0, None);
    assert!(
        rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            16,
            &mut releasing
        )))
        .is_err()
    );
    assert!(read(&slots[0].0).is_none());
    assert_eq!(read(&slots[1].0).unwrap(), slots[1].1);
    set(&slots[0].0, Some(&slots[1].1));
    assert!(
        rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            16,
            &mut releasing
        )))
        .is_err(),
        "foreign slot cannot be cleared"
    );
    assert_eq!(read(&slots[0].0).unwrap(), slots[1].1);
    set(&slots[0].0, Some(&slots[0].1));
    assert!(
        rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            16,
            &mut releasing
        )))
        .unwrap()
    );
    assert!(
        !rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            16,
            &mut releasing
        )))
        .unwrap()
    );
    let mut lost_reply = sealed.clone();
    assert!(
        !rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            16,
            &mut lost_reply
        )))
        .unwrap()
    );
    seal_rename_s4s(rt, f, 16, &mut lost_reply).unwrap();
    for (intent, (slot, raw)) in intents.iter().zip(&slots) {
        assert!(read(slot).is_none());
        assert!(
            matches!(
                rt.block_on(f.catalog.pending_node_mutation_attempt(
                    &f.serial,
                    f.route,
                    16,
                    intent.volume()
                )),
                Err(CatalogError::PoolBusy)
            ),
            "released participant stays excluded under original pool hold"
        );
        set(slot, Some(raw));
        assert!(
            rt.block_on(
                f.catalog
                    .node_rename_volume_intents(f.route, 16, &lost_reply, 0)
            )
            .is_err(),
            "restored stale claim contradicts released progress"
        );
        set(slot, None);
    }
    assert_eq!(read(&source_node_key).unwrap(), source_node_before);
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 17))
        .unwrap();
    assert!(publish_rename(context, 16, &lost_reply).is_err());
    let published = publish_rename(context, 17, &lost_reply).unwrap();
    assert_eq!(published.request(), request);
    assert_eq!(published.group(), sealed.id());
    assert_eq!(published.source().inode, source_id);
    assert_eq!(published.source().version, old_head.version + 1);
    assert_eq!(published.source_parent(), published.destination_parent());
    assert_eq!(
        published.source_parent().version,
        request.spec().source_parent.version + 1
    );
    assert_eq!(published.replaced().unwrap().inode, target_id);
    assert_eq!(
        published.replaced().unwrap().version,
        request.spec().target.unwrap().version + 1
    );
    let (copy_id, after_copy) = published.physical_copy().unwrap();
    assert_ne!(copy_id, old_head.copy_id);
    let mut expected_copy = old_copy.as_ref().clone();
    expected_copy.version += 1;
    assert_eq!(after_copy, expected_copy);
    assert_eq!(read(&old_copy_key).unwrap(), old_copy_raw);
    assert_eq!(read(&target_head_key).unwrap(), target_head);
    assert!(
        read(&source_node_key).unwrap().starts_with(b"N\x01"),
        "moved source stays a live inode"
    );
    let retired = read(&inode_key(b'N', target_id)).unwrap();
    assert!(retired.starts_with(b"NR1"));
    assert_eq!(&retired[35..51], target_id.as_bytes());
    for (path, expected) in [
        (&request.spec().from, None),
        (&request.spec().to, Some(source_id)),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        let found = rt.block_on(f.catalog.resolve_path(ns, &path)).unwrap();
        assert_eq!(
            found.as_ref().map(|node| match node {
                ResolvedPath::File { id, .. } | ResolvedPath::Directory { id, .. } => *id,
            }),
            expected
        );
        if let Some(ResolvedPath::File {
            head,
            copy,
            attributes,
            ..
        }) = found
        {
            assert_eq!(head.version, old_head.version + 1);
            assert_eq!(head.copy_id, copy_id);
            assert_eq!(*copy, expected_copy);
            let attrs = attributes.unwrap();
            assert_eq!(attrs.digest(), published.attributes_digest());
            assert_eq!(
                attrs.meta().unwrap().file_uid,
                match &inputs.baseline.source {
                    ResolvedPath::File {
                        attributes: Some(attrs),
                        ..
                    } => attrs.meta().unwrap().file_uid,
                    _ => unreachable!(),
                }
            );
            assert_eq!(attrs.meta().unwrap().change_time, TIME);
        }
    }
    let status = rt
        .block_on(f.catalog.node_mutation_task_status(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        status.task.state,
        tape_rs::managed_catalog::NodeMutationTaskState::Completed
    );
    assert!(
        status.outcome.is_none(),
        "rename must not become a one-parent SingleNode acknowledgment"
    );
    assert_eq!(
        status.task.publication_digest,
        Some(published.fingerprint())
    );
    assert_eq!(publish_rename(context, 17, sealed).unwrap(), published);
    for intent in &intents {
        assert!(
            rt.block_on(f.catalog.pending_node_mutation_attempt(
                &f.serial,
                f.route,
                17,
                intent.volume()
            ))
            .unwrap()
            .is_none()
        );
    }
    verify_published_rename_data(context);
    // Remove temporary plans and request-object blocks; the original result is self-contained.
    let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
    prefix.extend_from_slice(ns.as_bytes());
    prefix.extend_from_slice(b"QM");
    prefix.extend_from_slice(f.serial.as_bytes());
    prefix.extend_from_slice(&[0, 0]);
    prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
    prefix.extend_from_slice(request.spec().operation.as_bytes());
    let mut end = prefix.clone();
    end.push(255);
    let object = &status.task.object;
    let mut object_prefix = b"tape-rs/catalog/22/@/".to_vec();
    object_prefix.extend_from_slice(ns.as_bytes());
    object_prefix.push(b'b');
    object_prefix.extend_from_slice(&(object.library().len() as u16).to_be_bytes());
    object_prefix.extend_from_slice(object.library().as_bytes());
    object_prefix.extend_from_slice(&object.digest());
    object_prefix.extend_from_slice(&object.byte_len().to_be_bytes());
    let mut object_end = object_prefix.clone();
    object_end.push(255);
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        let mut marker = object_prefix.clone();
        marker.push(0);
        assert!(tr.get(&marker, false).await.unwrap().is_some());
        tr.clear_range(&prefix, &end);
        tr.clear_range(&object_prefix, &object_end);
        tr.commit().await.unwrap();
    });
    assert_eq!(publish_rename(context, 17, sealed).unwrap(), published);
    let mut result_key = b"tape-rs/catalog/22/@/".to_vec();
    result_key.extend_from_slice(ns.as_bytes());
    result_key.extend_from_slice(b"Qn");
    result_key.extend_from_slice(f.serial.as_bytes());
    result_key.extend_from_slice(&[0, 0]);
    result_key.extend_from_slice(&request.request_epoch().to_be_bytes());
    result_key.extend_from_slice(request.spec().operation.as_bytes());
    let result = read(&result_key).unwrap();
    for bad in [None, Some(b"{}".as_slice())] {
        set(&result_key, bad);
        assert!(
            rt.block_on(f.catalog.node_mutation_task_status(
                &f.serial,
                f.route,
                request.request_epoch(),
                request.spec().operation
            ))
            .is_err()
        );
        assert!(publish_rename(context, 17, sealed).is_err());
        assert_eq!(
            read(&result_key).as_deref(),
            bad,
            "missing/corrupt completed result must not be repaired"
        );
        set(&result_key, Some(&result));
    }
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 18))
        .unwrap();
    assert_eq!(
        rt.block_on(f.catalog.node_rename_publication(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation
        ))
        .unwrap(),
        Some(published)
    );
}
#[test]
#[ignore = "需要隔离FDB；仅Sim空目录原锚卷S4/跨父发布及两父版本各推进一次"]
fn rename_empty_directory_publishes_across_parents_with_original_anchor_and_identity() {
    exercise_empty_directory(true, None, false);
}
fn publish_empty_rename<'v>(
    context: (
        &tokio::runtime::Runtime,
        &Fixture,
        &tape_rs::managed_catalog::PreparedNodeRenameInputs,
    ),
    global: &tape_rs::managed_catalog::NodeRenameGlobalPlan,
    mut view: tape_rs::managed_catalog::NodeRenameVolumeProjection,
    mut mounted: LtfsVolume<'v>,
    dev: &dyn TapeTransport,
    execution: Option<RenameExecutionTest<'v>>,
) -> (LtfsVolume<'v>, tape_rs::scsi::reservation::ReservationKey) {
    use tape_rs::daemon::fdb_rename_media::{NodeRenameExecution, NodeRenameVolumeOutcome};
    use tape_rs::daemon::permission::{Permission, Phase};
    use tape_rs::scsi::reservation::{FenceOutcome, ReservationKey, fence};
    let (rt, f, inputs) = context;
    let request = &inputs.request;
    let original_owner = match execution.map(|e| e.case) {
        None | Some(RenameExecutionCase::Fresh) => 13,
        _ => 12,
    };
    let key = ReservationKey::new(71, 13);
    if original_owner == 13 {
        if let Some(execution) = execution {
            drop(mounted);
            execution.previous.close(12);
            assert!(execution.permission.renew(
                13,
                Phase::Serving,
                std::time::Instant::now() + Duration::from_secs(60)
            ));
            rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
                .unwrap();
            assert!(matches!(
                fence(dev, key).unwrap(),
                FenceOutcome::Fenced { .. }
            ));
            mounted = LtfsVolume::mount(execution.transport).unwrap();
        } else {
            rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
                .unwrap();
            assert!(matches!(
                fence(dev, key).unwrap(),
                FenceOutcome::Fenced { .. }
            ));
        }
    }
    let mut attempt = rt
        .block_on(f.catalog.begin_node_rename_attempt_preparation(
            f.route,
            original_owner,
            global,
            Uuid::new_v4(),
        ))
        .unwrap();
    let mut intents = rt
        .block_on(f.catalog.next_node_rename_attempt_preparation_page(
            f.route,
            original_owner,
            &mut attempt,
        ))
        .unwrap();
    assert_eq!(intents.len(), 1);
    rt.block_on(Box::pin(f.catalog.admit_node_rename_attempt(
        f.route,
        original_owner,
        &mut attempt,
    )))
    .unwrap();
    let mut intent = intents.remove(0);
    if original_owner == 12 {
        let execution = execution.unwrap();
        let case = execution.case.media_case();
        if case != RenameExecutionCase::Unchanged {
            use tape_rs::scsi::cdb::opcode;
            use tape_rs::scsi::sim::{Fault, FaultAction};
            let previous_key = ReservationKey::new(71, 12);
            assert!(matches!(
                fence(dev, previous_key).unwrap(),
                FenceOutcome::Fenced { .. }
            ));
            mounted.set_reservation_guard(Some(previous_key));
            if case == RenameExecutionCase::Unknown {
                mounted
                    .append_file("/unexpected", &mut b"foreign".as_slice())
                    .unwrap();
                mounted.commit().unwrap();
            } else {
                mounted
                    .stage_node_rename(intent.media_plan(), &view, 16 << 20)
                    .unwrap();
                let (opcode, stage) = if case == RenameExecutionCase::Partial {
                    (opcode::WRITE_FILEMARKS_6, "closing_fm")
                } else {
                    (opcode::WRITE_6, "index_partition")
                };
                execution.device.inject(Fault {
                    opcode,
                    after: 1,
                    remaining: 1,
                    action: FaultAction::CheckCondition {
                        sense_key: 3,
                        asc: 0x0c,
                        ascq: 0,
                    },
                });
                let error = mounted.commit().unwrap_err();
                assert!(
                    matches!(error, tape_rs::error::TapeError::CommitFailed{stage:ref actual,..} if actual == stage)
                );
                execution.device.clear_faults();
            }
        }
        drop(mounted);
        execution.previous.close(12);
        if execution.case.through_owner() {
            let source_index = intent.media_plan().recipe().source_index();
            let owner = Owner::start_mode(f, 13, json!({}), true);
            let published = rt
                .block_on(f.catalog.node_rename_publication(
                    &f.serial,
                    f.route,
                    request.request_epoch(),
                    request.spec().operation,
                ))
                .unwrap()
                .unwrap();
            assert_eq!(published.group(), attempt.id());
            assert_eq!(published.completed_owner(), 13);
            assert_eq!(published.source().inode, request.spec().source.inode);
            assert_eq!(
                published.source().version,
                request.spec().source.version + 1
            );
            assert_eq!(
                published.source_parent().version,
                request.spec().source_parent.version + 1
            );
            assert_eq!(
                published.destination_parent().version,
                request.spec().destination_parent.version + 1
            );
            let head = rt
                .block_on(
                    f.catalog
                        .observed_active_volume_index(&f.serial, intent.volume()),
                )
                .unwrap();
            assert_eq!(
                head.generation,
                source_index.generation
                    + if case == RenameExecutionCase::Partial {
                        2
                    } else {
                        1
                    },
                "old S4 must not be replayed; T2 has one explicit Discard before its new S4"
            );
            let from = tape_rs::managed_catalog::pool_catalog_path(
                f.pools[0],
                request.spec().from.as_bytes(),
            )
            .unwrap();
            let to = tape_rs::managed_catalog::pool_catalog_path(
                f.pools[0],
                request.spec().to.as_bytes(),
            )
            .unwrap();
            assert!(
                rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &from))
                    .unwrap()
                    .is_none()
            );
            let Some(ResolvedPath::Directory { id, version, .. }) = rt
                .block_on(f.catalog.resolve_path(f.route.route.namespace, &to))
                .unwrap()
            else {
                panic!("owner must publish the original directory");
            };
            assert_eq!(id, request.spec().source.inode);
            assert_eq!(version, request.spec().source.version + 1);
            let server = writer(f, &owner, 13);
            let client = FdbClient::new([&server.address])
                .unwrap()
                .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
            let output = f.root.join("owner-recovery-original-content");
            client
                .download_new(&f.serial, f.pools[0], "/file-0", &output)
                .unwrap();
            assert_eq!(fs::read(output).unwrap(), b"content-0");
            drop(client);
            drop(server);
            drop(owner);
            assert!(execution.permission.renew(
                13,
                Phase::Serving,
                std::time::Instant::now() + Duration::from_secs(60)
            ));
            f.library.load_into_drive(BARCODES[0], 0).unwrap();
            assert!(matches!(
                fence(dev, key).unwrap(),
                FenceOutcome::Fenced { .. }
            ));
            mounted = LtfsVolume::mount(execution.transport).unwrap();
            mounted.set_reservation_guard(Some(key));
            let actual: tape_rs::managed_catalog::ActiveIndexIdentity =
                mounted.verified_committed_index_construct().unwrap().into();
            assert_eq!(actual, head);
            return (mounted, key);
        }
        assert!(execution.permission.renew(
            13,
            if case == RenameExecutionCase::Partial {
                Phase::Execution
            } else {
                Phase::Serving
            },
            std::time::Instant::now() + Duration::from_secs(60)
        ));
        rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
            .unwrap();
        assert!(matches!(
            fence(dev, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        mounted = LtfsVolume::mount(execution.transport).unwrap();
    }
    mounted.set_reservation_guard(Some(key));
    if let Some(execution) = execution {
        let case = execution.case.media_case();
        let before = mounted.index().to_xml().unwrap();
        let unrelated = Permission::default();
        assert!(unrelated.renew(
            13,
            Phase::Serving,
            std::time::Instant::now() + Duration::from_secs(60)
        ));
        let wrong =
            NodeRenameExecution::new(&f.catalog, f.route, key, &unrelated, 16 << 20).unwrap();
        assert!(matches!(
            rt.block_on(Box::pin(wrong.execute(mounted, &attempt, &intent, None))),
            Err(tape_rs::error::TapeError::NotReady(_))
        ));
        mounted = LtfsVolume::mount(execution.transport).unwrap();
        mounted.set_reservation_guard(Some(key));
        assert_eq!(mounted.index().to_xml().unwrap(), before);
        let executor =
            NodeRenameExecution::new(&f.catalog, f.route, key, execution.permission, 16 << 20)
                .unwrap();
        if case == RenameExecutionCase::Unknown {
            let error =
                match rt.block_on(Box::pin(executor.execute(mounted, &attempt, &intent, None))) {
                    Ok(_) => panic!("unknown higher index must not be adopted"),
                    Err(error) => error,
                };
            assert!(
                matches!(error, tape_rs::error::TapeError::Catalog(_)),
                "{error}"
            );
            mounted = LtfsVolume::mount(execution.transport).unwrap();
            mounted.set_reservation_guard(Some(key));
            assert_eq!(mounted.index().to_xml().unwrap(), before);
            assert!(
                rt.block_on(f.catalog.node_rename_observation(f.route, 13, &intent))
                    .unwrap()
                    .is_none()
            );
            assert!(
                rt.block_on(f.catalog.node_rename_intent_closure(f.route, 13, &intent))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                rt.block_on(f.catalog.node_mutation_task(
                    &f.serial,
                    f.route,
                    request.request_epoch(),
                    request.spec().operation
                ))
                .unwrap()
                .unwrap()
                .state,
                tape_rs::managed_catalog::NodeMutationTaskState::Writing
            );
            assert_eq!(
                rt.block_on(
                    f.catalog
                        .observed_active_volume_index(&f.serial, intent.volume())
                )
                .unwrap(),
                intent.media_plan().recipe().source_index()
            );
            for (path, exists) in [
                (request.spec().from.as_str(), true),
                (request.spec().to.as_str(), false),
                ("/unexpected", false),
            ] {
                let path = tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes())
                    .unwrap();
                assert_eq!(
                    rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
                        .unwrap()
                        .is_some(),
                    exists
                );
            }
            return (mounted, key);
        }
        let first = rt
            .block_on(Box::pin(executor.execute(mounted, &attempt, &intent, None)))
            .unwrap();
        let mut saved = None;
        match first.outcome() {
            NodeRenameVolumeOutcome::NeedsProjection => {
                assert_eq!(execution.case, RenameExecutionCase::Fresh)
            }
            NodeRenameVolumeOutcome::Observed(observation) => {
                assert_eq!(execution.case, RenameExecutionCase::Committed);
                assert_eq!(observation.intent().original_owner_epoch(), 12);
                assert_eq!(observation.observed_owner(), 13);
                saved = Some(observation.clone());
            }
            NodeRenameVolumeOutcome::Closed(closed) => {
                let expected = if case == RenameExecutionCase::Partial {
                    tape_rs::managed_catalog::NodeRenameAbsenceKind::Discarded
                } else {
                    tape_rs::managed_catalog::NodeRenameAbsenceKind::Unchanged
                };
                assert_eq!(closed.kind(), expected);
                assert_eq!(closed.intent().original_owner_epoch(), 12);
                assert_eq!(closed.closed_owner(), 13);
            }
        }
        let closed = match first.outcome() {
            NodeRenameVolumeOutcome::Closed(closed) => Some(closed.clone()),
            _ => None,
        };
        mounted = first.into_volume();
        if let Some(closed) = closed {
            view = rebuild_held_rename_projection(rt, f, request, mounted.index(), intent.volume());
            let new = rt
                .block_on(f.catalog.admit_node_rename_rebased_intent(
                    f.route,
                    13,
                    &attempt,
                    &closed,
                    &mut view,
                    Uuid::new_v4(),
                ))
                .unwrap();
            assert_ne!(new.id(), intent.id());
            assert_eq!(new.original_owner_epoch(), 13);
            assert_eq!(new.group(), attempt.id());
            let stale = intent.clone();
            intent = new;
            assert!(
                matches!(
                    rt.block_on(Box::pin(executor.execute(mounted, &attempt, &stale, None))),
                    Err(tape_rs::error::TapeError::Catalog(_))
                ),
                "stale closed intent cannot be retargeted"
            );
            mounted = LtfsVolume::mount(execution.transport).unwrap();
            mounted.set_reservation_guard(Some(key));
        }
        if saved.is_none() {
            let committed = rt
                .block_on(Box::pin(executor.execute(
                    mounted,
                    &attempt,
                    &intent,
                    Some(view),
                )))
                .unwrap();
            let NodeRenameVolumeOutcome::Observed(observation) = committed.outcome() else {
                panic!("new intent must observe S4")
            };
            saved = Some(observation.clone());
            mounted = committed.into_volume();
        }
        let xml = mounted.index().to_xml().unwrap();
        let repeated = rt
            .block_on(Box::pin(executor.execute(mounted, &attempt, &intent, None)))
            .unwrap();
        assert_eq!(
            repeated.outcome(),
            &NodeRenameVolumeOutcome::Observed(saved.unwrap())
        );
        assert_eq!(repeated.volume().index().to_xml().unwrap(), xml);
        mounted = repeated.into_volume();
    } else {
        rt.block_on(
            f.catalog
                .check_node_rename_write_intent(f.route, 13, &attempt, &intent),
        )
        .unwrap();
        mounted
            .stage_node_rename(intent.media_plan(), &view, 16 << 20)
            .unwrap();
        mounted.sync_batch().unwrap();
        let evidence = mounted
            .node_rename_evidence(intent.media_plan(), 16 << 20)
            .unwrap();
        rt.block_on(
            f.catalog
                .observe_node_rename_intent(f.route, 13, &attempt, &intent, &evidence),
        )
        .unwrap();
    }
    seal_rename_s4s(rt, f, 13, &mut attempt).unwrap();
    assert!(
        rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            13,
            &mut attempt
        )))
        .unwrap()
    );
    assert!(
        !rt.block_on(Box::pin(f.catalog.release_node_rename_publication_page(
            f.route,
            13,
            &mut attempt
        )))
        .unwrap()
    );
    let published = rt
        .block_on(Box::pin(f.catalog.publish_node_rename_attempt(
            f.route,
            13,
            &attempt,
            8 << 20,
        )))
        .unwrap();
    assert!(published.is_directory());
    assert!(published.physical_copy().is_none());
    assert!(published.replaced().is_none());
    assert_eq!(published.source().inode, request.spec().source.inode);
    assert_eq!(
        published.source().version,
        request.spec().source.version + 1
    );
    assert_ne!(
        published.source_parent().inode,
        published.destination_parent().inode
    );
    for (path, revision) in [
        ("/", published.source_parent()),
        ("/destination", published.destination_parent()),
        ("/destination/moved", published.source()),
    ] {
        let path =
            tape_rs::managed_catalog::pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        let node = rt
            .block_on(f.catalog.resolve_path(f.route.route.namespace, &path))
            .unwrap()
            .unwrap();
        let ResolvedPath::Directory {
            id,
            version,
            attributes,
        } = node
        else {
            panic!("directory publication")
        };
        assert_eq!(id, revision.inode);
        assert_eq!(version, revision.version);
        let attributes = attributes.unwrap();
        let meta = attributes.meta().unwrap();
        assert_eq!(meta.change_time, TIME);
        if id != published.source().inode {
            assert_eq!(meta.modify_time, TIME);
        }
    }
    let old =
        tape_rs::managed_catalog::pool_catalog_path(f.pools[0], request.spec().from.as_bytes())
            .unwrap();
    assert!(
        rt.block_on(f.catalog.resolve_path(f.route.route.namespace, &old))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        rt.block_on(Box::pin(f.catalog.publish_node_rename_attempt(
            f.route,
            13,
            &attempt,
            8 << 20
        )))
        .unwrap(),
        published
    );
    (mounted, key)
}

fn verify_published_rename_data(context: RenameAttemptContext<'_>) {
    let f = context.f;
    let owner = Owner::start_mode(f, 17, json!({}), true);
    let server = writer(f, &owner, 17);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let output = f.root.join("renamed-original-data");
    assert_eq!(
        client
            .download_new(&f.serial, f.pools[0], &context.request.spec().to, &output)
            .unwrap(),
        b"content-0".len() as u64
    );
    assert_eq!(fs::read(output).unwrap(), b"content-0");
    assert!(
        client
            .stat(&f.serial, f.pools[0], &context.request.spec().from)
            .unwrap()
            .is_none()
    );
    drop(client);
    drop(server);
    drop(owner);
}

#[test]
#[ignore = "需要隔离FDB；仅Sim逐卷执行、Permission身份、无投影不写、S4重试不重放"]
fn rename_volume_executor_requires_original_permission_and_observes_exact_s4_once() {
    exercise_empty_directory(true, Some(RenameExecutionCase::Fresh), false);
}
fn rebuild_held_rename_projection(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    request: &tape_rs::managed_catalog::NodeRenameRequest,
    index: &tape_rs::ltfs::index::LtfsIndex,
    volume: Uuid,
) -> tape_rs::managed_catalog::NodeRenameVolumeProjection {
    let manifest = rt
        .block_on(Box::pin(f.catalog.node_rename_held_manifest(
            &f.serial,
            f.route,
            13,
            request.request_epoch(),
            request.spec().operation,
        )))
        .unwrap();
    let baseline = rt
        .block_on(Box::pin(
            f.catalog.check_node_rename_request(f.route, 13, request),
        ))
        .unwrap();
    let mut spool = rt
        .block_on(Box::pin(f.catalog.build_node_rename_projection_spool(
            f.route,
            13,
            &manifest,
            16 << 20,
        )))
        .unwrap();
    let mut guard = rt
        .block_on(Box::pin(f.catalog.build_node_rename_volume_guard(
            f.route,
            13,
            &manifest,
            &mut spool,
            volume,
            16 << 20,
        )))
        .unwrap();
    let mut source = guard
        .physical_source(&baseline, &mut spool, index, 16 << 20)
        .unwrap();
    rt.block_on(Box::pin(f.catalog.build_node_rename_volume_projection(
        f.route,
        13,
        &manifest,
        &mut spool,
        &mut source,
        16 << 20,
    )))
    .unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameExecutionCase {
    Fresh,
    Unchanged,
    Partial,
    Committed,
    Unknown,
    OwnerUnchanged,
    OwnerPartial,
    OwnerCommitted,
}
impl RenameExecutionCase {
    fn media_case(self) -> Self {
        match self {
            Self::OwnerUnchanged => Self::Unchanged,
            Self::OwnerPartial => Self::Partial,
            Self::OwnerCommitted => Self::Committed,
            case => case,
        }
    }
    fn through_owner(self) -> bool {
        self != self.media_case()
    }
}
#[derive(Clone, Copy)]
struct RenameExecutionTest<'a> {
    case: RenameExecutionCase,
    permission: &'a tape_rs::daemon::permission::Permission,
    previous: &'a tape_rs::daemon::permission::Permission,
    transport: &'a dyn TapeTransport,
    device: &'a tape_rs::scsi::sim::SimTransport,
}
#[test]
#[ignore = "需要隔离FDB；仅Sim接任旧owner原T0关闭及当前owner新nonce，禁止重放旧意图"]
fn rename_volume_executor_closes_old_owner_t0_before_a_new_physical_intent() {
    exercise_empty_directory(true, Some(RenameExecutionCase::Unchanged), false);
}
#[test]
#[ignore = "需要隔离FDB；仅Sim原closingFM失败/T2显式修尾/关闭/新来源重试及最终发布"]
fn rename_volume_executor_recovers_partial_original_tail_and_rebases_verified_source() {
    exercise_empty_directory(true, Some(RenameExecutionCase::Partial), false);
}
#[test]
#[ignore = "需要隔离FDB；仅Sim原IP失败但DP已有S4，接任只接纳原nonce，不重放或关闭成功卷"]
fn rename_volume_executor_observes_old_owner_s4_without_replaying_it() {
    exercise_empty_directory(true, Some(RenameExecutionCase::Committed), false);
}
#[test]
#[ignore = "需要隔离FDB；仅Sim未知较高完整索引拒绝，原Writing/目录保留且不修复或改写"]
fn rename_volume_executor_refuses_unknown_higher_index_without_adopting_or_discarding_it() {
    exercise_empty_directory(true, Some(RenameExecutionCase::Unknown), false);
}

// Keep fault injection and session epochs on the exact handle behind both guards.
struct RenameSharedTransport(Arc<tape_rs::scsi::sim::SimTransport>);
impl TapeTransport for RenameSharedTransport {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        data: &mut [u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_read(cdb, data, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_write(cdb, data, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.0.session_epoch()
    }
    fn identity(&self) -> &str {
        self.0.identity()
    }
}

#[test]
#[ignore = "需要显式 FDB_TEST_CLUSTER；仅 SimLibrary，不访问真实设备"]
fn rename_preparation_refuses_to_recreate_referenced_recipes_and_pages_after_takeover() {
    exercise_empty_directory(false, None, true);
}

fn assert_original_preparation_not_recreated(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    manifest: &tape_rs::managed_catalog::NodeRenameManifest,
    request: &NodeRenameRequest,
    projection: &mut tape_rs::managed_catalog::NodeRenameProjectionSpool,
    view: &mut tape_rs::managed_catalog::NodeRenameVolumeProjection,
    volume: Uuid,
) {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let mut prefix = b"tape-rs/catalog/22/@/".to_vec();
    prefix.extend_from_slice(f.route.route.namespace.as_bytes());
    prefix.extend_from_slice(b"QM");
    prefix.extend_from_slice(f.serial.as_bytes());
    prefix.extend_from_slice(&[0, 0]);
    prefix.extend_from_slice(&request.request_epoch().to_be_bytes());
    prefix.extend_from_slice(request.spec().operation.as_bytes());
    let mut recipe_key = prefix.clone();
    recipe_key.push(2);
    recipe_key.extend_from_slice(volume.as_bytes());
    let mut header_key = prefix.clone();
    header_key.push(3);
    let mut page_key = prefix.clone();
    page_key.push(4);
    page_key.extend_from_slice(&0u64.to_be_bytes());
    let read = |key: &[u8]| {
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(key, false)
                .await
                .unwrap()
                .map(|v| v.to_vec())
        })
    };
    let recipe = read(&recipe_key).unwrap();
    let header = read(&header_key).unwrap();
    let page = read(&page_key).unwrap();
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 13))
        .unwrap();
    for remove_header in [false, true] {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.clear(&recipe_key);
            if remove_header {
                tr.clear(&header_key);
            }
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(
                f.catalog
                    .node_rename_volume_plan(f.route, 13, manifest, volume)
            )
            .is_err(),
            "referenced original recipe cannot become a fresh missing recipe"
        );
        assert!(
            rt.block_on(
                f.catalog
                    .save_node_rename_volume_plan(f.route, 13, manifest, view)
            )
            .is_err(),
            "takeover must not mint a replacement creator"
        );
        assert!(read(&recipe_key).is_none());
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&recipe_key, &recipe);
            tr.set(&header_key, &header);
            tr.commit().await.unwrap();
        });
    }
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.clear(&page_key);
        tr.commit().await.unwrap();
    });
    assert!(
        rt.block_on(
            f.catalog
                .resume_node_rename_participants(f.route, 13, manifest, projection)
        )
        .is_err()
    );
    let mut fresh = rt
        .block_on(f.catalog.begin_node_rename_participants(
            f.route,
            13,
            manifest,
            projection,
            Some(volume),
        ))
        .unwrap();
    let before = fresh.summary().clone();
    assert!(
        rt.block_on(
            f.catalog
                .next_node_rename_participants_page(f.route, 13, &mut fresh, projection)
        )
        .is_err(),
        "sealed first page must not be recreated"
    );
    assert_eq!(fresh.summary(), &before);
    assert!(read(&page_key).is_none());
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        tr.set(&page_key, &page);
        tr.commit().await.unwrap();
    });
    // A lost header/first page cannot hide later participant or attempt state.
    for tag in 4..=9u8 {
        let mut orphan_key = prefix.clone();
        orphan_key.push(tag);
        if tag == 4 {
            orphan_key.extend_from_slice(&1u64.to_be_bytes());
        } else {
            orphan_key.push(0);
        }
        let orphan = if tag == 4 { page.clone() } else { vec![0x7f] };
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.clear(&header_key);
            tr.clear(&page_key);
            tr.set(&orphan_key, &orphan);
            tr.commit().await.unwrap();
        });
        assert!(
            rt.block_on(
                f.catalog
                    .resume_node_rename_participants(f.route, 13, manifest, projection)
            )
            .is_err()
        );
        let mut fresh = rt
            .block_on(f.catalog.begin_node_rename_participants(
                f.route,
                13,
                manifest,
                projection,
                Some(volume),
            ))
            .unwrap();
        assert!(
            rt.block_on(
                f.catalog
                    .next_node_rename_participants_page(f.route, 13, &mut fresh, projection)
            )
            .is_err()
        );
        assert!(read(&page_key).is_none());
        assert!(read(&header_key).is_none());
        assert_eq!(read(&orphan_key).unwrap(), orphan);
        if tag >= 5 {
            assert!(
                rt.block_on(f.catalog.node_rename_global_plan(f.route, 13, manifest))
                    .is_err()
            );
        }
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.clear(&orphan_key);
            tr.set(&header_key, &header);
            tr.set(&page_key, &page);
            tr.commit().await.unwrap();
        });
    }
    let original = rt
        .block_on(
            f.catalog
                .node_rename_volume_plan(f.route, 13, manifest, volume),
        )
        .unwrap()
        .unwrap();
    assert_eq!(original.creator_owner_epoch(), 12);
    assert_eq!(read(&recipe_key).unwrap(), recipe);
    assert_eq!(read(&header_key).unwrap(), header);
    assert_eq!(read(&page_key).unwrap(), page);
}

#[test]
#[ignore = "需要隔离FDB；真实owner自动多卷rename与Sim/FUSE之外的原内容读取，不接触真实硬件"]
fn rename_owner_executes_original_file_replacement_subtree_and_empty_anchor_without_manual_steps() {
    use tape_rs::managed_catalog::NodeMutationTaskState;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_actor_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original_source = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let original_target = client
        .stat(&f.serial, f.pools[0], "/file-1")
        .unwrap()
        .unwrap();
    let root = client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap();
    let first = actor_rename(&rt, &f, &client, 12, "/file-0", "/file-1", false);
    assert!(!first.is_directory());
    assert_eq!(first.completed_owner(), 12);
    assert_eq!(
        first.replaced().unwrap().inode,
        revision(&original_target).inode
    );
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .is_none()
    );
    let moved = client
        .stat(&f.serial, f.pools[0], "/file-1")
        .unwrap()
        .unwrap();
    assert_eq!(moved["id"], original_source["id"]);
    assert_eq!(
        moved["version"].as_u64().unwrap(),
        original_source["version"].as_u64().unwrap() + 1
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap()["version"]
            .as_u64()
            .unwrap(),
        root["version"].as_u64().unwrap() + 1
    );
    let output = f.root.join("actor-replacement-content");
    client
        .download_new(&f.serial, f.pools[0], "/file-1", &output)
        .unwrap();
    assert_eq!(fs::read(&output).unwrap(), b"content-0");
    for path in [
        "/container",
        "/destination",
        "/container/empty",
        "/empty-anchor",
    ] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    actor_rename(&rt, &f, &client, 12, "/file-1", "/container/a", true);
    actor_rename(&rt, &f, &client, 12, "/file-3", "/container/b", true);
    let before_tree = client
        .stat(&f.serial, f.pools[0], "/container")
        .unwrap()
        .unwrap();
    let before_a = client
        .stat(&f.serial, f.pools[0], "/container/a")
        .unwrap()
        .unwrap();
    let before_b = client
        .stat(&f.serial, f.pools[0], "/container/b")
        .unwrap()
        .unwrap();
    let before_empty = client
        .stat(&f.serial, f.pools[0], "/container/empty")
        .unwrap()
        .unwrap();
    let source_parent = client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap();
    let target_parent = client
        .stat(&f.serial, f.pools[0], "/destination")
        .unwrap()
        .unwrap();
    let tree = actor_rename(
        &rt,
        &f,
        &client,
        12,
        "/container",
        "/destination/moved",
        true,
    );
    assert!(tree.is_directory());
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/destination/moved")
            .unwrap()
            .unwrap()["id"],
        before_tree["id"]
    );
    for (name, prior, content) in [
        ("a", before_a, Some(b"content-0".as_slice())),
        ("b", before_b, Some(b"content-3".as_slice())),
        ("empty", before_empty, None),
    ] {
        assert!(
            client
                .stat(&f.serial, f.pools[0], &format!("/container/{name}"))
                .unwrap()
                .is_none()
        );
        let path = format!("/destination/moved/{name}");
        let current = client.stat(&f.serial, f.pools[0], &path).unwrap().unwrap();
        assert_eq!(current["id"], prior["id"]);
        assert_eq!(
            current["version"], prior["version"],
            "subtree descendants retain their own original versions"
        );
        if let Some(content) = content {
            let output = f.root.join(format!("actor-subtree-content-{name}"));
            client
                .download_new(&f.serial, f.pools[0], &path, &output)
                .unwrap();
            assert_eq!(fs::read(output).unwrap(), content);
        }
    }
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap()["version"]
            .as_u64()
            .unwrap(),
        source_parent["version"].as_u64().unwrap() + 1
    );
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/destination")
            .unwrap()
            .unwrap()["version"]
            .as_u64()
            .unwrap(),
        target_parent["version"].as_u64().unwrap() + 1
    );
    let empty = actor_rename(
        &rt,
        &f,
        &client,
        12,
        "/empty-anchor",
        "/destination/anchor",
        true,
    );
    assert!(empty.is_directory());
    assert!(empty.physical_copy().is_none());
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 12, None, 32)
        )
        .unwrap()
        .items
        .is_empty()
    );
    let first_request = first.request();
    let epoch = first_request.request_epoch();
    let operation = first_request.spec().operation;
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_mutation_task(&f.serial, f.route, epoch, operation)
        )
        .unwrap()
        .unwrap()
        .state,
        NodeMutationTaskState::Completed
    );
    drop(client);
    drop(server);
    drop(owner);
    // First result remains independently readable with no owner, after both
    // names have changed again and the source is now inside a moved subtree.
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_publication(&f.serial, f.route, epoch, operation)
        )
        .unwrap()
        .unwrap(),
        first
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    assert_eq!(
        rt.block_on(
            f.catalog
                .node_rename_publication(&f.serial, f.route, epoch, operation)
        )
        .unwrap()
        .unwrap(),
        first
    );
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn actor_rename(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    client: &FdbClient,
    owner: u64,
    from: &str,
    to: &str,
    no_replace: bool,
) -> tape_rs::managed_catalog::NodeRenamePublication {
    use tape_rs::managed_catalog::NodeMutationTaskState;
    let original = spec(client, f, from, to, no_replace);
    let request = rt
        .block_on(
            f.catalog
                .prepare_node_rename_request(&f.serial, f.route, owner, &original, TIME),
        )
        .unwrap();
    rt.block_on(
        f.catalog
            .queue_node_rename_request(f.route, owner, &request, 8 << 20),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = rt
            .block_on(f.catalog.node_mutation_task(
                &f.serial,
                f.route,
                request.request_epoch(),
                original.operation,
            ))
            .unwrap()
            .unwrap();
        if task.state == NodeMutationTaskState::Completed {
            break;
        }
        assert!(
            !matches!(
                task.state,
                NodeMutationTaskState::Cancelled | NodeMutationTaskState::Rejected
            ),
            "original rename ended without publication: {task:?}"
        );
        assert!(
            Instant::now() < deadline,
            "owner did not finish original rename: {task:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let publication = rt
        .block_on(f.catalog.node_rename_publication(
            &f.serial,
            f.route,
            request.request_epoch(),
            original.operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(publication.request(), &request);
    publication
}

#[test]
#[ignore = "需要隔离FDB；真实owner启动Execution恢复旧T0并用新nonce提交，仅Sim"]
fn rename_owner_startup_closes_old_t0_and_publishes_original_request() {
    exercise_empty_directory(true, Some(RenameExecutionCase::OwnerUnchanged), false);
}
#[test]
#[ignore = "需要隔离FDB；真实owner启动Execution恢复实际T2并显式修尾后提交，仅Sim"]
fn rename_owner_startup_discards_partial_old_tail_and_publishes_original_request() {
    exercise_empty_directory(true, Some(RenameExecutionCase::OwnerPartial), false);
}
#[test]
#[ignore = "需要隔离FDB；真实owner启动Execution观察原DP-S4不重放，仅Sim"]
fn rename_owner_startup_observes_old_s4_without_replaying_original_intent() {
    exercise_empty_directory(true, Some(RenameExecutionCase::OwnerCommitted), false);
}

#[test]
#[ignore = "需要隔离FDB；真实HTTP/owner/Sim原双路径提交、丢应答、无owner终态，不访问真实硬件"]
fn rename_http_preserves_original_two_path_results_after_reply_loss_recreation_and_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_http_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let readonly = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original = spec(&client, &f, "/file-0", "/file-1", false);
    let input = rename_http_input(&original);
    let (status, prepared) = send(
        &server,
        &f,
        "POST",
        &format!("{}/prepare", rename_http_base(&f)),
        input.to_string().as_bytes(),
    );
    assert_eq!(status, 200, "{prepared}");
    assert_eq!(prepared["kind"], "rename");
    let request = NodeRenameRequest::decode(prepared["request"].to_string().as_bytes()).unwrap();
    assert_eq!(request.spec(), &original);
    assert_eq!(request.time(), TIME);
    assert_eq!(
        prepared["fingerprint"],
        request
            .fingerprint()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 12, None, 32)
        )
        .unwrap()
        .items
        .is_empty()
    );
    let route = rename_http_route(&f, &prepared);
    assert_eq!(send(&readonly, &f, "GET", &route, &[]).0, 404);
    for field in ["source", "source_parent", "destination_parent", "target"] {
        let mut bad = input.clone();
        bad[field]["inode"] = Uuid::new_v4().to_string().into();
        let (status, body) = send(
            &server,
            &f,
            "POST",
            &format!("{}/prepare", rename_http_base(&f)),
            bad.to_string().as_bytes(),
        );
        assert!(
            matches!(status, 400 | 409),
            "wrong original {field}: {status} {body}"
        );
    }
    let wrong_protocol = send(
        &server,
        &f,
        "POST",
        &format!("{}/requests", base(&f)),
        prepared["request"].to_string().as_bytes(),
    );
    assert!(matches!(wrong_protocol.0, 400 | 409));
    lose_rename_admission_reply(&server, &f, &prepared);
    let completed = rename_http_terminal(&readonly, &f, &prepared);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["kind"], "rename");
    assert_eq!(
        completed["result"]["source"]["inode"],
        original.source.inode.to_string()
    );
    assert_eq!(
        completed["result"]["source"]["version"],
        original.source.version + 1
    );
    assert_eq!(
        completed["result"]["source_parent"]["version"],
        original.source_parent.version + 1
    );
    assert_eq!(
        completed["result"]["destination_parent"]["version"],
        original.destination_parent.version + 1
    );
    assert_eq!(
        completed["result"]["replaced"]["inode"],
        original.target.unwrap().inode.to_string()
    );
    assert_eq!(
        completed["result"]["replaced"]["version"],
        original.target.unwrap().version + 1
    );
    assert_eq!(completed["result"]["directory"], false);
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .is_none()
    );
    let output = f.root.join("http-rename-original-content");
    client
        .download_new(&f.serial, f.pools[0], "/file-1", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"content-0");
    mutate(&client, &f, "/file-0", json!({"kind":"create-directory"}));
    assert_ne!(
        client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap()["id"],
        original.source.inode.to_string()
    );
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &format!("{}/requests", rename_http_base(&f)),
            prepared["request"].to_string().as_bytes()
        ),
        (200, completed.clone())
    );
    assert!(
        matches!(
            send(&server, &f, "GET", &super::route(&f, &prepared), &[]).0,
            400 | 409
        ),
        "wrong protocol must not expose a rename as SingleNode"
    );
    drop(client);
    drop(server);
    drop(owner);
    let original_object_marker =
        assert_http_rename_retains_original_result_without_repair_or_temporary_plans(
            &rt, &f, &readonly, &request,
        );
    assert_eq!(
        send(&readonly, &f, "GET", &route, &[]),
        (200, completed.clone())
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &format!("{}/requests", rename_http_base(&f)),
            prepared["request"].to_string().as_bytes()
        ),
        (200, completed.clone())
    );
    assert_eq!(send(&server, &f, "DELETE", &route, &[]), (200, completed));
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    assert!(
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(&original_object_marker, false)
                .await
                .unwrap()
        })
        .is_none(),
        "known terminal retries must not restore deleted temporary request blocks"
    );
    drop(server);
    drop(readonly);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn rename_http_base(f: &Fixture) -> String {
    format!("/v1/fdb/renames/{}", f.serial)
}
fn rename_http_input(spec: &NodeRenameSpec) -> serde_json::Value {
    let revision = |r: NodeRenameRevision| json!({"namespace":r.namespace.to_string(),"inode":r.inode.to_string(),"version":r.version});
    json!({"version":1,"pool":spec.pool.to_string(),"operation":spec.operation.to_string(),"from":spec.from,"to":spec.to,
        "source":revision(spec.source),"source_parent":revision(spec.source_parent),"destination_parent":revision(spec.destination_parent),
        "target":spec.target.map(revision),"no_replace":spec.no_replace,"time":TIME})
}
fn rename_http_route(f: &Fixture, prepared: &serde_json::Value) -> String {
    format!(
        "{}/requests/{}/{}",
        rename_http_base(f),
        prepared["request"]["request_epoch"],
        prepared["request"]["operation"].as_str().unwrap()
    )
}
fn rename_http_terminal(
    server: &ReadHttpServer,
    f: &Fixture,
    prepared: &serde_json::Value,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = send(server, f, "GET", &rename_http_route(f, prepared), &[]);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["fingerprint"], prepared["fingerprint"]);
        if matches!(
            body["status"].as_str(),
            Some("completed" | "cancelled" | "rejected")
        ) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "rename HTTP has not completed: {body}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn lose_rename_admission_reply(server: &ReadHttpServer, f: &Fixture, prepared: &serde_json::Value) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let target = server.address.clone();
    let body = prepared["request"].to_string();
    let request = format!("POST {}/requests HTTP/1.1\r\nHost: test\r\nConnection: close\r\nx-tapers-fdb-namespace: {}\r\nContent-Length: {}\r\n\r\n{}",rename_http_base(f),f.route.route.namespace,body.len(),body).into_bytes();
    let original = request.clone();
    let proxy = std::thread::spawn(move || {
        let (mut downstream, _) = listener.accept().unwrap();
        downstream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut received = vec![0; original.len()];
        downstream.read_exact(&mut received).unwrap();
        assert_eq!(received, original);
        let mut upstream = std::net::TcpStream::connect(target).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        upstream.write_all(&original).unwrap();
        let mut answer = Vec::new();
        upstream.read_to_end(&mut answer).unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 202") || answer.starts_with(b"HTTP/1.1 200"));
        // The real server completed admission. The client receives no result.
        drop(downstream);
    });
    let mut downstream = std::net::TcpStream::connect(address).unwrap();
    downstream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    downstream.write_all(&request).unwrap();
    let mut answer = Vec::new();
    downstream.read_to_end(&mut answer).unwrap();
    assert!(answer.is_empty());
    proxy.join().unwrap();
}

#[test]
#[ignore = "需要隔离FDB；rename HTTP缺介质保留Queued/取消/错协议与双路径身份拒绝，仅Sim"]
fn rename_http_rejects_recreated_parent_and_retains_unavailable_original_until_cancel() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_http_conditions_workspace={}", f.root.display());
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false},BARCODES[1]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let readonly = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original = spec(&client, &f, "/file-0", "/file-1", false);
    let input = rename_http_input(&original);
    let endpoint = format!("{}/prepare", rename_http_base(&f));
    for bad in [
        {
            let mut v = input.clone();
            v["target"] = serde_json::Value::Null;
            v
        },
        {
            let mut v = input.clone();
            v["no_replace"] = true.into();
            v
        },
        {
            let mut v = input.clone();
            v["target"]["namespace"] = Uuid::new_v4().to_string().into();
            v
        },
        {
            let mut v = input.clone();
            v.as_object_mut().unwrap().remove("target");
            v
        },
        {
            let mut v = input.clone();
            v["physical_plan"] = json!({});
            v
        },
    ] {
        let (status, body) = send(&server, &f, "POST", &endpoint, bad.to_string().as_bytes());
        assert!(matches!(status, 400 | 409), "{status} {body}");
    }
    let (status, prepared) = send(&server, &f, "POST", &endpoint, input.to_string().as_bytes());
    assert_eq!(status, 200, "{prepared}");
    let route = rename_http_route(&f, &prepared);
    let (status, queued) = send(
        &server,
        &f,
        "POST",
        &format!("{}/requests", rename_http_base(&f)),
        prepared["request"].to_string().as_bytes(),
    );
    assert_eq!(status, 202, "{queued}");
    assert_eq!(queued["status"], "queued");
    assert!(queued["prepared_owner_epoch"].is_null());
    assert!(queued["result"].is_null());
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        send(&readonly, &f, "GET", &route, &[]),
        (200, queued.clone())
    );
    let wrong = send(&server, &f, "DELETE", &super::route(&f, &prepared), &[]);
    assert!(matches!(wrong.0, 400 | 409));
    assert_eq!(send(&readonly, &f, "GET", &route, &[]), (200, queued));
    let (status, cancelled) = send(&server, &f, "DELETE", &route, &[]);
    assert_eq!(status, 200, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    assert!(cancelled["result"].is_null());
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &format!("{}/requests", rename_http_base(&f)),
            prepared["request"].to_string().as_bytes()
        ),
        (200, cancelled.clone())
    );
    // Other eligible media can perform ordinary nodes after cancellation.
    for path in ["/left", "/right", "/left/source", "/right/parent"] {
        mutate(&client, &f, path, json!({"kind":"create-directory"}));
    }
    let parent = spec(&client, &f, "/left/source", "/right/parent/moved", true);
    mutate(
        &client,
        &f,
        "/right/parent",
        json!({"kind":"remove-directory"}),
    );
    mutate(
        &client,
        &f,
        "/right/parent",
        json!({"kind":"create-directory"}),
    );
    let current = spec(&client, &f, "/left/source", "/right/parent/moved", true);
    assert_eq!(
        current.destination_parent.version,
        parent.destination_parent.version
    );
    assert_ne!(
        current.destination_parent.inode,
        parent.destination_parent.inode
    );
    assert_eq!(current.source, parent.source);
    assert_eq!(current.source_parent, parent.source_parent);
    let (status, body) = send(
        &server,
        &f,
        "POST",
        &endpoint,
        rename_http_input(&parent).to_string().as_bytes(),
    );
    assert!(
        matches!(status, 400 | 409),
        "recreated same-version parent: {status} {body}"
    );
    let mut uppercase = rename_http_input(&current);
    uppercase["operation"] = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into();
    assert!(matches!(
        send(
            &server,
            &f,
            "POST",
            &endpoint,
            uppercase.to_string().as_bytes()
        )
        .0,
        400 | 409
    ));
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 12, None, 32)
        )
        .unwrap()
        .items
        .is_empty()
    );
    drop(client);
    drop(server);
    drop(owner);
    assert_eq!(send(&readonly, &f, "GET", &route, &[]), (200, cancelled));
    drop(readonly);
    fs::remove_dir_all(f.root).unwrap();
}

fn assert_http_rename_retains_original_result_without_repair_or_temporary_plans(
    rt: &tokio::runtime::Runtime,
    f: &Fixture,
    server: &ReadHttpServer,
    request: &NodeRenameRequest,
) -> Vec<u8> {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let mut namespace = b"tape-rs/catalog/22/@/".to_vec();
    namespace.extend_from_slice(f.route.route.namespace.as_bytes());
    let mut identity = f.serial.as_bytes().to_vec();
    identity.extend_from_slice(&[0, 0]);
    identity.extend_from_slice(&request.request_epoch().to_be_bytes());
    identity.extend_from_slice(request.spec().operation.as_bytes());
    let mut result_key = namespace.clone();
    result_key.extend_from_slice(b"Qn");
    result_key.extend_from_slice(&identity);
    let route = format!(
        "{}/requests/{}/{}",
        rename_http_base(f),
        request.request_epoch(),
        request.spec().operation
    );
    let read = |key: &[u8]| {
        rt.block_on(async {
            db.create_trx()
                .unwrap()
                .get(key, false)
                .await
                .unwrap()
                .map(|value| value.to_vec())
        })
    };
    let original = read(&result_key).unwrap();
    for bad in [None, Some(b"{}".as_slice())] {
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            match bad {
                None => tr.clear(&result_key),
                Some(bytes) => tr.set(&result_key, bytes),
            }
            tr.commit().await.unwrap();
        });
        assert_eq!(
            send(server, f, "GET", &route, &[]).0,
            503,
            "missing/corrupt original result must not become an absent operation"
        );
        assert_eq!(
            read(&result_key).as_deref(),
            bad,
            "GET must not recreate or repair the original result"
        );
        rt.block_on(async {
            let tr = db.create_trx().unwrap();
            tr.set(&result_key, &original);
            tr.commit().await.unwrap();
        });
    }
    let task = rt
        .block_on(f.catalog.node_mutation_task(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap()
        .unwrap();
    let mut plans = namespace.clone();
    plans.extend_from_slice(b"QM");
    plans.extend_from_slice(&identity);
    let mut plans_end = plans.clone();
    plans_end.push(255);
    let mut object = namespace;
    object.push(b'b');
    object.extend_from_slice(&(task.object.library().len() as u16).to_be_bytes());
    object.extend_from_slice(task.object.library().as_bytes());
    object.extend_from_slice(&task.object.digest());
    object.extend_from_slice(&task.object.byte_len().to_be_bytes());
    let mut object_end = object.clone();
    object_end.push(255);
    let mut marker = object.clone();
    marker.push(0);
    rt.block_on(async {
        let tr = db.create_trx().unwrap();
        let mut canonical = plans.clone();
        canonical.push(0);
        assert!(tr.get(&canonical, false).await.unwrap().is_some());
        assert!(tr.get(&marker, false).await.unwrap().is_some());
        tr.clear_range(&plans, &plans_end);
        tr.clear_range(&object, &object_end);
        tr.commit().await.unwrap();
    });
    assert_eq!(read(&result_key).unwrap(), original);
    assert!(read(&marker).is_none());
    marker
}

#[test]
#[ignore = "需要隔离FDB；默认SDK双路径日志/丢准备与登记应答/FD根重命名/无owner与接任，仅Sim"]
fn rename_sdk_journal_keeps_original_identity_across_reply_loss_root_replacement_and_takeover() {
    use tape_rs::client::fdb::RenameJournal;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_sdk_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let readonly = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(5));
    let original = rename_sdk_spec(&client, &f, "/file-0", "/file-1", false);
    let path = f.root.join("rename-sdk-original");
    let held = f.root.join("rename-sdk-held");
    let mut journal = RenameJournal::create(&f.serial, &original, &path).unwrap();
    let operation = journal.operation();
    let time = journal.time().to_owned();
    let (address, proxy) = rename_sdk_loss_proxy(&server.address, true);
    let unreliable = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(5));
    assert!(unreliable.prepare_rename_journal(&mut journal).is_err());
    let lost_preparation = proxy.join().unwrap();
    assert!(journal.prepared().is_none());
    assert_eq!(journal.operation(), operation);
    assert_eq!(journal.time(), time);
    assert_eq!(journal.spec(), &original);
    drop(journal);
    let mut journal = RenameJournal::open(&path).unwrap();
    let prepared = client.prepare_rename_journal(&mut journal).unwrap();
    assert!(client.rename_status(&prepared).unwrap().is_none());
    assert_eq!(
        lost_preparation["request"]["operation"],
        operation.to_string()
    );
    assert_eq!(lost_preparation["request"]["time"], time);
    let expected =
        NodeRenameRequest::decode(lost_preparation["request"].to_string().as_bytes()).unwrap();
    assert_eq!(
        expected.spec().source.inode.to_string(),
        lost_preparation["request"]["source"]["inode"]
    );
    assert!(
        rt.block_on(
            f.catalog
                .pending_node_mutation_tasks(&f.serial, f.route, 12, None, 32)
        )
        .unwrap()
        .items
        .is_empty()
    );
    fs::rename(&path, &held).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("foreign"), b"untouched").unwrap();
    let (address, proxy) = rename_sdk_loss_proxy(&server.address, false);
    let unreliable = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(5));
    let error = unreliable.run_rename_journal(&mut journal).unwrap_err();
    assert!(
        matches!(error, tape_rs::client::ClientError::Indeterminate(_)),
        "{error}"
    );
    let admitted = proxy.join().unwrap();
    assert!(matches!(
        admitted["status"].as_str(),
        Some("queued" | "prepared" | "writing" | "completed")
    ));
    assert_eq!(admitted["operation"], operation.to_string());
    assert_eq!(admitted["fingerprint"], lost_preparation["fingerprint"]);
    assert!(journal.terminal().unwrap().is_none());
    drop(journal);
    let completed = client.run_rename_saved(&held).unwrap();
    assert_eq!(completed["operation"], operation.to_string());
    assert_eq!(completed["fingerprint"], lost_preparation["fingerprint"]);
    assert_eq!(completed["status"], "completed");
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .is_none()
    );
    let output = f.root.join("sdk-rename-original-content");
    client
        .download_new(&f.serial, f.pools[0], "/file-1", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"content-0");
    assert_eq!(fs::read(path.join("foreign")).unwrap(), b"untouched");
    assert!(!path.join("request.json").exists() && !path.join("receipt.json").exists());
    let journal = RenameJournal::open(&held).unwrap();
    assert_eq!(journal.operation(), operation);
    assert_eq!(journal.time(), time);
    assert_eq!(journal.spec(), &original);
    drop(journal);
    let offline = FdbClient::new(["127.0.0.1:0"])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_millis(20));
    assert_eq!(offline.run_rename_saved(&held).unwrap(), completed);
    assert_eq!(
        offline.rename_status_saved(&held).unwrap(),
        Some(completed.clone())
    );
    assert_eq!(offline.cancel_rename_saved(&held).unwrap(), completed);
    mutate(&client, &f, "/file-0", json!({"kind":"create-directory"}));
    let next = rename_sdk_spec(&client, &f, "/file-1", "/moved-again", true);
    let next_path = f.root.join("rename-sdk-next");
    client.prepare_rename(&f.serial, &next, &next_path).unwrap();
    client.run_rename_saved(&next_path).unwrap();
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/file-1")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        client.rename_status(&prepared).unwrap(),
        Some(completed.clone())
    );
    // A new inode at the same destination-parent version cannot be rebound
    // during a Draft's preparation retry.
    for name in ["/left", "/right", "/left/source", "/right/parent"] {
        mutate(&client, &f, name, json!({"kind":"create-directory"}));
    }
    let fixed = rename_sdk_spec(&client, &f, "/left/source", "/right/parent/moved", true);
    let fixed_path = f.root.join("rename-sdk-stale-parent");
    let mut fixed_journal = RenameJournal::create(&f.serial, &fixed, &fixed_path).unwrap();
    let fixed_operation = fixed_journal.operation();
    let fixed_time = fixed_journal.time().to_owned();
    mutate(
        &client,
        &f,
        "/right/parent",
        json!({"kind":"remove-directory"}),
    );
    mutate(
        &client,
        &f,
        "/right/parent",
        json!({"kind":"create-directory"}),
    );
    let now = rename_sdk_spec(&client, &f, "/left/source", "/right/parent/moved", true);
    assert_ne!(
        fixed.condition.destination_parent,
        now.condition.destination_parent
    );
    assert!(client.prepare_rename_journal(&mut fixed_journal).is_err());
    assert!(fixed_journal.prepared().is_none());
    assert_eq!(fixed_journal.spec(), &fixed);
    assert_eq!(fixed_journal.operation(), fixed_operation);
    assert_eq!(fixed_journal.time(), fixed_time);
    drop(fixed_journal);
    drop(client);
    drop(server);
    drop(owner);
    let observer = FdbClient::new([&readonly.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(3));
    let remote_terminal = f.root.join("rename-sdk-header-only");
    fs::create_dir(&remote_terminal).unwrap();
    fs::copy(
        held.join("request.json"),
        remote_terminal.join("request.json"),
    )
    .unwrap();
    assert!(!remote_terminal.join("receipt.json").exists());
    assert_eq!(
        observer.cancel_rename_saved(&remote_terminal).unwrap(),
        completed,
        "known remote completion must be resolved without a Serving-only DELETE"
    );
    assert!(remote_terminal.join("receipt.json").is_file());
    assert_eq!(
        observer.rename_status(&prepared).unwrap(),
        Some(completed.clone())
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let new_owner = FdbClient::new([&server.address]).unwrap();
    assert_eq!(
        new_owner.rename_status(&prepared).unwrap(),
        Some(completed.clone())
    );
    assert_eq!(offline.run_rename_saved(&held).unwrap(), completed);
    drop(new_owner);
    drop(observer);
    drop(server);
    drop(readonly);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn rename_sdk_spec(
    client: &FdbClient,
    f: &Fixture,
    from: &str,
    to: &str,
    no_replace: bool,
) -> tape_rs::client::fdb::RenameSpec {
    use tape_rs::client::{
        FileVersion,
        fdb::{RenameCondition, RenameSpec},
    };
    let original = spec(client, f, from, to, no_replace);
    let version = |r: NodeRenameRevision| FileVersion::Fdb {
        namespace: r.namespace,
        inode: r.inode,
        version: r.version,
    };
    RenameSpec {
        pool: original.pool,
        from: original.from,
        to: original.to,
        no_replace,
        condition: RenameCondition {
            source: version(original.source),
            source_parent: version(original.source_parent),
            destination_parent: version(original.destination_parent),
            target: original.target.map(version),
        },
    }
}

fn rename_sdk_loss_proxy(
    target: &str,
    prepare: bool,
) -> (String, std::thread::JoinHandle<serde_json::Value>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let target = target.to_owned();
    let proxy = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (mut downstream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "SDK never reached the intended rename RPC"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("rename proxy accept: {error}"),
            };
            downstream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&downstream);
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut raw = first.as_bytes().to_vec();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                raw.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("Content-Length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            assert!(length <= 8 << 20);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            raw.extend_from_slice(&body);
            let mut upstream = std::net::TcpStream::connect(&target).unwrap();
            upstream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            upstream.write_all(&raw).unwrap();
            let mut response = Vec::new();
            upstream.read_to_end(&mut response).unwrap();
            let uri = first.split_whitespace().nth(1).unwrap();
            let lost = first.starts_with("POST ")
                && if prepare {
                    uri.ends_with("/prepare")
                } else {
                    uri.ends_with("/requests")
                };
            if lost {
                assert!(
                    response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.1 202"),
                    "{}",
                    String::from_utf8_lossy(&response)
                );
                let start = response
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let reply = serde_json::from_slice(&response[start..]).unwrap();
                drop(downstream);
                return reply;
            }
            downstream.write_all(&response).unwrap();
        }
    });
    (address, proxy)
}

#[test]
#[ignore = "需要隔离FDB；默认SDK无介质保留原Queued/取消回执及离线恢复，仅Sim"]
fn rename_sdk_cancels_only_original_unavailable_request_and_caches_first_terminal() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_rename_sdk_cancel_workspace={}", f.root.display());
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false},BARCODES[1]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original = rename_sdk_spec(&client, &f, "/file-0", "/file-1", false);
    let path = f.root.join("sdk-rename-cancel");
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| matches!(cdb[0], 0x0a | 0x10 | 0x8d))
            .count()
    };
    let before = writes();
    let prepared = client.prepare_rename(&f.serial, &original, &path).unwrap();
    let no_wait = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(5));
    assert!(matches!(
        no_wait.run_rename_saved(&path),
        Err(tape_rs::client::ClientError::Indeterminate(_))
    ));
    let queued = client.rename_status(&prepared).unwrap().unwrap();
    assert_eq!(queued["status"], "queued");
    assert!(queued["prepared_owner_epoch"].is_null());
    let cancelled = client.cancel_rename_saved(&path).unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert!(cancelled["result"].is_null());
    assert_eq!(cancelled["operation"], queued["operation"]);
    assert_eq!(cancelled["fingerprint"], queued["fingerprint"]);
    assert_eq!(
        client.rename_status_saved(&path).unwrap(),
        Some(cancelled.clone())
    );
    let offline = FdbClient::new(["127.0.0.1:0"])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_millis(20));
    assert!(matches!(
        offline.run_rename_saved(&path),
        Err(tape_rs::client::ClientError::Rejected { status: 409, .. })
    ));
    assert_eq!(offline.cancel_rename_saved(&path).unwrap(), cancelled);
    assert_eq!(offline.rename_status_saved(&path).unwrap(), Some(cancelled));
    assert_eq!(
        writes(),
        before,
        "unavailable original files cannot admit any metadata or content append"
    );
    drop(no_wait);
    drop(client);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
