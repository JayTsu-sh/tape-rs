use super::*;
use tape_rs::client::ClientError;
use tape_rs::client::fdb::FdbClient;
use tape_rs::managed_catalog::{CatalogError, ResolvedPath};

fn change(
    client: &FdbClient,
    f: &Fixture,
    path: &str,
    version: Option<u64>,
    mutation: serde_json::Value,
) -> serde_json::Value {
    let file = f.root.join(format!("attributes-{}.json", Uuid::new_v4()));
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            path,
            original_condition(
                client,
                &f.serial,
                f.route.route.namespace,
                f.pools[0],
                path,
                version,
            ),
            mutation,
            &file,
        )
        .unwrap();
    client.run_node_mutation_saved(&file).unwrap()
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 SDK/CLI 属性读取与模拟介质提交，只读查询不访问设备"]
fn attributes_read_original_large_and_native_metadata_without_a_live_owner() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_attributes_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let readonly = FdbClient::new([&reader.address]).unwrap();
    let root = readonly
        .node_attributes(&f.serial, f.pools[0], "/")
        .unwrap()
        .unwrap();
    assert_eq!(root["kind"], "directory");
    let original = readonly
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let metadata = json!({"kind":"change-metadata","readonly":true,"access_time":"2026-11-01T12:34:55.000000000Z","modify_time":"2026-11-01T12:34:56.000000000Z"});
    change(
        &client,
        &f,
        "/file-0",
        original["version"].as_u64(),
        metadata,
    );
    let file = readonly
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    assert_eq!(file["version"], original["version"].as_u64().unwrap() + 1);
    assert_eq!(file["metadata"]["readonly"], true);
    assert_eq!(
        file["metadata"]["modify_time"],
        "2026-11-01T12:34:56.000000000Z"
    );
    for field in ["file_uid", "creation_time", "backup_time"] {
        assert_eq!(
            file["metadata"][field], original["metadata"][field],
            "{field}"
        );
    }
    let path = "/属性";
    change(&client, &f, path, None, json!({"kind":"create-directory"}));
    let text = "属性".repeat(24_000);
    change(
        &client,
        &f,
        path,
        Some(1),
        json!({"kind":"set-xattr","attribute":{"key":"user.large","value":text,"base64":false},"mode":"create"}),
    );
    let catalog_path = format!("/{}{path}", f.pools[0]);
    let opaque = rt
        .block_on(f.catalog.resolve_active_metadata_for_route(
            &f.serial,
            catalog_path.as_bytes(),
            f.route,
        ))
        .unwrap()
        .unwrap();
    let ResolvedPath::Directory {
        attributes: Some(attributes),
        ..
    } = opaque
    else {
        panic!("directory attributes")
    };
    assert!(
        attributes.meta().is_none(),
        "large attributes must really use the external object store"
    );
    assert!(matches!(
        rt.block_on(f.catalog.resolve_active_attributes_for_route(
            &f.serial,
            catalog_path.as_bytes(),
            f.route,
            1
        )),
        Err(CatalogError::Invalid("节点属性超过读取字节预算"))
    ));
    let directory = readonly
        .node_attributes(&f.serial, f.pools[0], path)
        .unwrap()
        .unwrap();
    assert_eq!(directory["version"], 2);
    assert_eq!(
        directory["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "user.large")
            .unwrap()["value"],
        text
    );
    let link = "/属性/link";
    change(
        &client,
        &f,
        link,
        None,
        json!({"kind":"create-symlink","target":".LTFSEE_DATA/native-target"}),
    );
    change(
        &client,
        &f,
        link,
        Some(1),
        json!({"kind":"set-xattr","attribute":{"key":"user.binary","value":"AP8=","base64":true},"mode":"upsert"}),
    );
    let native = readonly
        .node_attributes(&f.serial, f.pools[0], link)
        .unwrap()
        .unwrap();
    assert_eq!(native["kind"], "symlink");
    assert_eq!(native["symlink_target"], ".LTFSEE_DATA/native-target");
    let binary = native["xattrs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "user.binary")
        .unwrap();
    assert_eq!(binary["value"], "AP8=");
    assert_eq!(binary["base64"], true);
    change(
        &client,
        &f,
        link,
        Some(2),
        json!({"kind":"remove-xattr","key":"user.binary"}),
    );
    let current = readonly
        .node_attributes(&f.serial, f.pools[0], link)
        .unwrap()
        .unwrap();
    assert!(
        !current["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "user.binary")
    );
    change(&client, &f, link, Some(3), json!({"kind":"unlink"}));
    assert!(
        readonly
            .node_attributes(&f.serial, f.pools[0], link)
            .unwrap()
            .is_none()
    );
    change(&client, &f, link, None, json!({"kind":"create-directory"}));
    let recreated = readonly
        .node_attributes(&f.serial, f.pools[0], link)
        .unwrap()
        .unwrap();
    assert_ne!(recreated["id"], native["id"]);
    assert_eq!(recreated["kind"], "directory");
    assert!(recreated["symlink_target"].is_null());
    let mut wrong = f.route;
    wrong.route.namespace = Uuid::new_v4();
    assert!(
        rt.block_on(f.catalog.resolve_active_attributes_for_route(
            &f.serial,
            catalog_path.as_bytes(),
            wrong,
            8 << 20
        ))
        .is_err()
    );
    owner.expect(0);
    drop(server);
    owner.permission.close(12);
    let trace = owner.trace.clone();
    drop(owner);
    let commands = trace.lock().unwrap().len();
    assert_eq!(
        readonly
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
        file
    );
    assert_eq!(
        readonly
            .node_attributes(&f.serial, f.pools[0], link)
            .unwrap()
            .unwrap(),
        recreated
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &reader.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "fdb",
            "fs",
            "attributes",
            path,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let after = readonly
        .node_attributes(&f.serial, f.pools[0], path)
        .unwrap()
        .unwrap();
    assert_eq!(cli, after);
    assert_eq!(
        trace.lock().unwrap().len(),
        commands,
        "metadata-only requests must issue zero SCSI commands"
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    assert_eq!(
        readonly
            .node_attributes(&f.serial, f.pools[0], path)
            .unwrap()
            .unwrap(),
        after
    );
    owner.expect(0);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际节点 S4 故障前后的已提交属性与原尝试恢复，不接触真实设备"]
fn attributes_keep_committed_values_until_original_node_s4_is_published_after_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_attributes_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(2));
    let readonly = FdbClient::new([&reader.address]).unwrap();
    let before = readonly
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let file = f.root.join("original-metadata.json");
    let prepared = client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/file-0",
            original_condition(
                &client,
                &f.serial,
                f.route.route.namespace,
                f.pools[0],
                "/file-0",
                before["version"].as_u64(),
            ),
            json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":null}),
            &file,
        )
        .unwrap();

    let bytes = fs::read(file.join("request.json")).unwrap();
    owner.fail_vci.store(true, Ordering::SeqCst);
    assert!(matches!(
        client.run_node_mutation_saved(&file),
        Err(ClientError::Indeterminate(_))
    ));
    assert_eq!(
        client.node_mutation_status(&prepared).unwrap().unwrap()["status"],
        "writing"
    );
    assert!(!owner.permission.allows(12, Phase::Serving));
    assert_eq!(
        readonly
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
        before
    );
    drop(server);
    drop(owner);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let client = FdbClient::new([&server.address]).unwrap();
    let completed = client.run_node_mutation_saved(&file).unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(fs::read(file.join("request.json")).unwrap(), bytes);
    let after = readonly
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    assert_eq!(after["id"], before["id"]);
    assert_eq!(after["version"], before["version"].as_u64().unwrap() + 1);
    assert_eq!(after["metadata"]["readonly"], true);
    assert_ne!(after["attributes_digest"], before["attributes_digest"]);
    assert!(
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .all(|cdb| !matches!(cdb[0], 0x0a | 0x10)),
        "takeover must not reappend the original S4"
    );
    owner.expect(0);
    drop(server);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实SDK与SimLibrary同卷/跨卷覆盖，不访问真实设备"]
fn content_overwrites_preserve_current_logical_attributes_on_same_and_other_volumes() {
    use sha2::{Digest, Sha256};
    use tape_rs::ltfs::index::XATTR_SHA256;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_overwrite_attributes_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    for (version, mutation) in [
        (
            1,
            json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"before overwrite","base64":false},"mode":"create"}),
        ),
        (
            2,
            json!({"kind":"set-xattr","attribute":{"key":"user.binary","value":"AP9B","base64":true},"mode":"create"}),
        ),
        (
            3,
            json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":"2020-01-02T03:04:05.000000000Z"}),
        ),
    ] {
        assert_eq!(
            change(&client, &f, "/file-0", Some(version), mutation)["status"],
            "completed"
        );
    }
    let mut previous = client
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let original_id = previous["id"].clone();
    let mut original_requests = Vec::new();
    for (index, barcode) in [BARCODES[0], BARCODES[1]].into_iter().enumerate() {
        let before_stat = client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap();
        let version = previous["version"].as_u64().unwrap();
        let body = f.root.join(format!("attribute-overwrite-{index}.body"));
        let bytes = format!("replacement body number {index}").into_bytes();
        fs::write(&body, &bytes).unwrap();
        let saved = f.root.join(format!("attribute-overwrite-{index}.json"));
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/file-0",
                &body,
                upload_replacement(
                    &client
                        .stat(&f.serial, f.pools[0], "/file-0")
                        .unwrap()
                        .unwrap(),
                ),
                Some(barcode),
                &saved,
            )
            .unwrap();
        assert_upload_committed(client.upload_saved(&saved).unwrap(), &saved, version + 1);
        let after = client
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap();
        let after_stat = client
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap();
        assert_eq!(after["id"], original_id);
        assert_eq!(after["version"], version + 1);
        for key in ["user.note", "user.binary"] {
            let before = previous["xattrs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == key)
                .unwrap();
            let retained = after["xattrs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == key);
            assert_eq!(
                retained,
                Some(before),
                "lost {key} when overwriting onto {barcode}"
            );
        }
        assert_eq!(after["metadata"]["readonly"], true);
        assert_ne!(
            after["metadata"]["modify_time"],
            previous["metadata"]["modify_time"]
        );
        assert_eq!(after_stat["barcode"], barcode);
        if after_stat["volume_id"] == before_stat["volume_id"] {
            assert_ne!(
                after["metadata"]["file_uid"],
                previous["metadata"]["file_uid"]
            );
            assert!(
                after_stat["generation"].as_u64().unwrap()
                    > before_stat["generation"].as_u64().unwrap()
            );
        }
        let digest = after["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == XATTR_SHA256)
            .unwrap();
        let expected = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(digest["value"], expected);
        assert_eq!(digest["base64"], false);
        assert!(
            !after["xattrs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "tapers.version")
        );
        let output = f.root.join(format!("attribute-download-{index}"));
        assert_eq!(
            client
                .download_new(&f.serial, f.pools[0], "/file-0", &output)
                .unwrap(),
            bytes.len() as u64
        );
        assert_eq!(fs::read(&output).unwrap(), bytes);
        original_requests.push((saved, version + 1));
        previous = after;
        if index == 0 {
            let version = previous["version"].as_u64().unwrap();
            assert_eq!(
                change(
                    &client,
                    &f,
                    "/file-0",
                    Some(version),
                    json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"latest logical attribute before cross-volume replacement","base64":false},"mode":"replace"})
                )["status"],
                "completed"
            );
            previous = client
                .node_attributes(&f.serial, f.pools[0], "/file-0")
                .unwrap()
                .unwrap();
        }
    }
    for (saved, first_version) in original_requests {
        assert_upload_committed(client.upload_saved(&saved).unwrap(), &saved, first_version);
    }
    assert_eq!(
        client
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
        previous
    );
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
