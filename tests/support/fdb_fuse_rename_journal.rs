use super::*;
use nix::libc;
use tape_rs::tapefs::{Backend, FdbBackend, RenameState};

#[test]
#[ignore = "需要隔离 FDB；FUSE backend 原 rename 日志和持有目录恢复，仅模拟磁带"]
fn backend_rename_keeps_first_result_in_the_held_journal_and_recovers_changed_names() {
    let _ = env_logger::try_init();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(30), Duration::from_secs(10));
    let original = rename_sdk_spec(&client, &f, "/file-0", "/file-1", false);
    let root = f.root.join("rename-backend-journals");
    let backend = FdbBackend::new_writable(
        FdbClient::new([&server.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(30), Duration::from_secs(10)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &root,
    )
    .unwrap();
    let mut state = RenameState::new(
        &original.from,
        &original.to,
        original.condition,
        original.no_replace,
    );
    let first = backend.mutate_rename(&mut state).unwrap();
    assert!(!first.directory);
    assert_eq!(
        first.source,
        match original.condition.source {
            tape_rs::client::FileVersion::Fdb {
                namespace,
                inode,
                version,
            } => tape_rs::client::FileVersion::Fdb {
                namespace,
                inode,
                version: version + 1
            },
            _ => unreachable!(),
        }
    );
    let held = f.root.join("held-rename-backend-journals");
    fs::rename(&root, &held).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("foreign"), b"untouched").unwrap();
    assert_eq!(backend.mutate_rename(&mut state).unwrap(), first);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(fs::read(root.join("foreign")).unwrap(), b"untouched");
    mutate(&client, &f, "/file-0", json!({"kind":"create-directory"}));
    drop(state);
    drop(backend);
    let backend = FdbBackend::new_writable(
        FdbClient::new([&server.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(30), Duration::from_secs(10)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &held,
    )
    .unwrap();
    assert_eq!(fs::read_dir(&held).unwrap().count(), 1);
    let directory = fs::read_dir(&held).unwrap().next().unwrap().unwrap().path();
    let saved = tape_rs::client::fdb::RenameJournal::open(&directory).unwrap();
    assert_eq!(saved.spec(), &original);
    assert_eq!(saved.terminal().unwrap().unwrap()["status"], "completed");
    drop(saved);
    drop(backend);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；丢登记应答后由新 owner 和原日志恢复，仅模拟磁带"]
fn backend_rename_recovers_the_original_lost_admission_after_owner_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let direct = FdbClient::new([&server.address]).unwrap();
    let original = rename_sdk_spec(&direct, &f, "/file-0", "/file-1", false);
    let (address, proxy) = rename_sdk_loss_proxy(&server.address, false);
    let flaky = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(5));
    let root = f.root.join("rename-lost-journals");
    let backend = FdbBackend::new_writable(
        flaky,
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &root,
    )
    .unwrap();
    let mut state = RenameState::new(
        &original.from,
        &original.to,
        original.condition,
        original.no_replace,
    );
    assert_eq!(backend.mutate_rename(&mut state), Err(libc::EIO));
    let accepted = proxy.join().unwrap();
    assert_eq!(accepted["status"], "queued");
    let directory = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    assert!(
        tape_rs::client::fdb::RenameJournal::open(&directory).is_err(),
        "pending state must keep its exclusive lease"
    );
    let header: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("request.json")).unwrap()).unwrap();
    assert_eq!(header["original"]["operation"], accepted["operation"]);
    assert!(!directory.join("receipt.json").exists());
    drop(state);
    drop(backend);
    drop(server);
    drop(owner);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    online_tape(&f, 13, BARCODES[0]);
    let server = writer(&f, &owner, 13);
    let direct = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(15), Duration::from_secs(10));
    let backend = FdbBackend::new_writable(
        direct,
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &root,
    )
    .unwrap();
    let saved = tape_rs::client::fdb::RenameJournal::open(&directory).unwrap();
    let terminal = saved.terminal().unwrap().unwrap();
    assert_eq!(terminal["operation"], accepted["operation"]);
    assert_eq!(terminal["fingerprint"], accepted["fingerprint"]);
    assert_eq!(terminal["status"], "completed");
    assert_eq!(saved.spec(), &original);
    drop(saved);
    drop(backend);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；全部原日志先验证作用域再恢复，不访问真实设备"]
fn backend_rename_wrong_scope_blocks_all_recovery_before_a_valid_node_draft_is_prepared() {
    use std::os::unix::fs::DirBuilderExt;
    use tape_rs::client::{
        FileVersion,
        fdb::{NodeCondition, NodeJournal, RenameJournal},
    };
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address]).unwrap();
    let mut original = rename_sdk_spec(&client, &f, "/file-0", "/never", true);
    let root = f.root.join("bad-rename-scope");
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let node = root.join(format!("node-{}", Uuid::new_v4()));
    drop(
        NodeJournal::create(
            &f.serial,
            f.pools[0],
            "/should-not-create",
            NodeCondition {
                parent: original.condition.source_parent,
                node: None,
            },
            json!({"kind":"create-directory"}),
            &node,
        )
        .unwrap(),
    );
    let node_header = fs::read(node.join("request.json")).unwrap();
    let wrong_namespace = Uuid::new_v4();
    let replace_namespace = |value| match value {
        FileVersion::Fdb { inode, version, .. } => FileVersion::Fdb {
            namespace: wrong_namespace,
            inode,
            version,
        },
        _ => unreachable!(),
    };
    original.condition.source = replace_namespace(original.condition.source);
    original.condition.source_parent = replace_namespace(original.condition.source_parent);
    original.condition.destination_parent =
        replace_namespace(original.condition.destination_parent);
    drop(
        RenameJournal::create(
            &f.serial,
            &original,
            &root.join(format!("rename-{}", Uuid::new_v4())),
        )
        .unwrap(),
    );
    let result = FdbBackend::new_writable(
        FdbClient::new([&server.address]).unwrap(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &root,
    );
    let error = result.err().expect("wrong namespace must block mount");
    assert!(error.to_string().contains("原重命名日志"), "{error}");
    assert_eq!(fs::read(node.join("request.json")).unwrap(), node_header);
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/should-not-create")
            .unwrap(),
        None
    );
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
