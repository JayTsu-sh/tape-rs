//! Original node identities and local terminal receipts on isolated FDB/Sim.
use super::*;
use tape_rs::client::fdb::{FdbClient, NodeCondition, NodeJournal};
use tape_rs::client::{ClientError, FileVersion};

fn version(node: &serde_json::Value) -> FileVersion {
    FileVersion::Fdb {
        namespace: Uuid::parse_str(node["namespace"].as_str().unwrap()).unwrap(),
        inode: Uuid::parse_str(node["id"].as_str().unwrap()).unwrap(),
        version: node["version"].as_u64().unwrap(),
    }
}
fn change(
    client: &FdbClient,
    f: &Fixture,
    path: &str,
    expected: Option<u64>,
    mutation: serde_json::Value,
) -> serde_json::Value {
    let condition = original_condition(
        client,
        &f.serial,
        f.route.route.namespace,
        f.pools[0],
        path,
        expected,
    );
    let directory = f.root.join(format!("node-{}", Uuid::new_v4()));
    client
        .prepare_node_mutation(&f.serial, f.pools[0], path, condition, mutation, &directory)
        .unwrap();
    client.run_node_mutation_saved(&directory).unwrap()
}

#[test]
#[ignore = "需要隔离FDB；NodeJournal原身份/持久终态与SimLibrary，不接触真实设备"]
fn original_node_journal_survives_no_owner_held_directory_recreation_and_offline_cli() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_node_journal_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let readonly = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let metadata = FdbClient::new([&readonly.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(2));
    let path = f.root.join("original-node");
    let condition = original_condition(
        &client,
        &f.serial,
        f.route.route.namespace,
        f.pools[0],
        "/journal-first",
        None,
    );
    let mut saved = NodeJournal::create(
        &f.serial,
        f.pools[0],
        "/journal-first",
        condition,
        json!({"kind":"create-directory"}),
        &path,
    )
    .unwrap();
    let operation = saved.operation();
    let time = saved.time().to_owned();
    let draft = fs::read(path.join("request.json")).unwrap();
    assert!(metadata.prepare_node_journal(&mut saved).is_err());
    assert!(saved.prepared().is_none());
    assert_eq!(fs::read(path.join("request.json")).unwrap(), draft);
    client.prepare_node_journal(&mut saved).unwrap();
    let ready = fs::read(path.join("request.json")).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&ready).unwrap();
    let epoch = envelope["prepared"]["request"]["request_epoch"]
        .as_u64()
        .unwrap();
    let held = f.root.join("held-original-node");
    fs::rename(&path, &held).unwrap();
    let foreign = NodeJournal::create(
        &f.serial,
        f.pools[0],
        "/journal-first",
        condition,
        json!({"kind":"create-directory"}),
        &path,
    )
    .unwrap();
    assert_ne!(foreign.operation(), operation);
    let foreign_header = fs::read(path.join("request.json")).unwrap();
    drop(foreign);
    let first = client.run_node_journal(&mut saved).unwrap();
    assert_eq!(first["status"], "completed");
    assert_eq!(first["operation"], operation.to_string());
    assert_eq!(saved.time(), time);
    assert_eq!(fs::read(held.join("request.json")).unwrap(), ready);
    assert_eq!(fs::read(path.join("request.json")).unwrap(), foreign_header);
    drop(saved);

    // The original parent is removed and recreated before pure preparation.
    change(
        &client,
        &f,
        "/guard-parent",
        None,
        json!({"kind":"create-directory"}),
    );
    let parent_condition = original_condition(
        &client,
        &f.serial,
        f.route.route.namespace,
        f.pools[0],
        "/guard-parent/child",
        None,
    );
    let guard = f.root.join("parent-original");
    let mut saved = NodeJournal::create(
        &f.serial,
        f.pools[0],
        "/guard-parent/child",
        parent_condition,
        json!({"kind":"create-directory"}),
        &guard,
    )
    .unwrap();
    let original_parent_header = fs::read(guard.join("request.json")).unwrap();
    let parent_op = saved.operation();
    change(
        &client,
        &f,
        "/guard-parent",
        Some(1),
        json!({"kind":"remove-directory"}),
    );
    change(
        &client,
        &f,
        "/guard-parent",
        None,
        json!({"kind":"create-directory"}),
    );
    let commands = owner.trace.lock().unwrap().len();
    assert!(matches!(
        client.prepare_node_journal(&mut saved),
        Err(ClientError::Verification(_))
    ));
    assert_eq!(owner.trace.lock().unwrap().len(), commands);
    assert_eq!(
        fs::read(guard.join("request.json")).unwrap(),
        original_parent_header
    );
    assert!(saved.prepared().is_none());
    assert!(
        rt.block_on(
            f.catalog
                .node_mutation_task(&f.serial, f.route, epoch, parent_op)
        )
        .unwrap()
        .is_none()
    );
    drop(saved);
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/guard-parent/child")
            .unwrap()
            .is_none()
    );

    // Bind an old inode at revision one, with the parent's current revision.
    // A new inode at the same path/version cannot satisfy this condition.
    let old = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    change(
        &client,
        &f,
        "/file-0",
        old["version"].as_u64(),
        json!({"kind":"unlink"}),
    );
    change(
        &client,
        &f,
        "/file-0",
        None,
        json!({"kind":"create-symlink","target":"/file-1"}),
    );
    let current = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    assert_ne!(current["id"], old["id"]);
    assert_eq!(current["version"], old["version"]);
    let parent = client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap();
    let condition = NodeCondition {
        parent: version(&parent),
        node: Some(version(&old)),
    };
    let guard = f.root.join("inode-original");
    let mut saved = NodeJournal::create(
        &f.serial,
        f.pools[0],
        "/file-0",
        condition,
        json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":null}),
        &guard,
    )
    .unwrap();
    let bytes = fs::read(guard.join("request.json")).unwrap();
    let commands = owner.trace.lock().unwrap().len();
    assert!(matches!(
        client.prepare_node_journal(&mut saved),
        Err(ClientError::Verification(_))
    ));
    assert_eq!(owner.trace.lock().unwrap().len(), commands);
    assert_eq!(fs::read(guard.join("request.json")).unwrap(), bytes);
    assert!(
        rt.block_on(
            f.catalog
                .node_mutation_task(&f.serial, f.route, epoch, saved.operation())
        )
        .unwrap()
        .is_none()
    );
    drop(saved);
    assert_eq!(
        client
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap()["metadata"]["readonly"],
        false
    );

    change(
        &client,
        &f,
        "/journal-first",
        Some(1),
        json!({"kind":"remove-directory"}),
    );
    change(
        &client,
        &f,
        "/journal-first",
        None,
        json!({"kind":"create-directory"}),
    );
    assert_ne!(
        client
            .stat(&f.serial, f.pools[0], "/journal-first")
            .unwrap()
            .unwrap()["id"],
        first["result"]["node"]
    );
    drop(server);
    drop(readonly);
    drop(owner);
    let offline = FdbClient::new(["127.0.0.1:0"]).unwrap();
    assert_eq!(
        offline.node_mutation_status_saved(&held).unwrap(),
        Some(first.clone())
    );
    assert_eq!(offline.run_node_mutation_saved(&held).unwrap(), first);
    assert_eq!(offline.cancel_node_mutation_saved(&held).unwrap(), first);
    for action in ["status", "run", "cancel"] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args(["-e", "127.0.0.1:0", "fdb", "fs", action])
            .arg(&held)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
            first
        );
    }
    let receipt = fs::read(held.join("receipt.json")).unwrap();
    let mut corrupt = receipt.clone();
    corrupt[10] ^= 1;
    fs::write(held.join("receipt.json"), &corrupt).unwrap();
    assert!(offline.run_node_mutation_saved(&held).is_err());
    assert_eq!(fs::read(held.join("receipt.json")).unwrap(), corrupt);
    assert_eq!(fs::read(held.join("request.json")).unwrap(), ready);
    fs::remove_dir_all(f.root).unwrap();
}
