//! Original upload results must not depend on a live owner or the current path.
use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::os::unix::fs::OpenOptionsExt;
use tape_rs::client::ClientError;
use tape_rs::client::fdb::{FdbClient, PreparedUpload, UploadJournal, UploadStatus};

#[test]
#[ignore = "需要隔离FDB；原父目录创建条件与SimLibrary，不接触真实设备"]
fn saved_creation_keeps_original_parent_after_directory_recreation() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_upload_parent_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address]).unwrap();
    let change = |version, kind| {
        let saved = f.root.join(format!("parent-node-{}.json", Uuid::new_v4()));
        client
            .prepare_node_mutation(
                &f.serial,
                f.pools[0],
                "/opened-parent",
                original_condition(
                    &client,
                    &f.serial,
                    f.route.route.namespace,
                    f.pools[0],
                    "/opened-parent",
                    version,
                ),
                json!({"kind":kind}),
                &saved,
            )
            .unwrap();
        assert_eq!(
            client.run_node_mutation_saved(&saved).unwrap()["status"],
            "completed"
        );
    };
    change(None, "create-directory");
    let original_parent = client
        .stat(&f.serial, f.pools[0], "/opened-parent")
        .unwrap()
        .unwrap();
    let condition = UploadCondition::Create {
        namespace: f.route.route.namespace,
        parent: Uuid::parse_str(original_parent["id"].as_str().unwrap()).unwrap(),
    };
    let metadata = reader(&f);
    let queries = FdbClient::new([&metadata.address]).unwrap();
    let source = f.root.join("parent-source");
    let saved = f.root.join("parent-draft");
    fs::write(&source, b"original child bytes").unwrap();
    assert!(matches!(
        queries.prepare_file(
            &f.serial,
            f.pools[0],
            "/opened-parent/original-child",
            &source,
            condition,
            None,
            &saved,
        ),
        Err(ClientError::Rejected { status: 404, .. })
    ));
    let header = fs::read(saved.join("request.json")).unwrap();
    let draft: serde_json::Value = serde_json::from_slice(&header).unwrap();
    assert_eq!(draft["original"]["version"], 2);
    assert_eq!(draft["original"]["parent"], original_parent["id"]);
    assert!(draft["prepared"].is_null());
    fs::remove_file(&source).unwrap();
    change(original_parent["version"].as_u64(), "remove-directory");
    change(None, "create-directory");
    let current_parent = client
        .stat(&f.serial, f.pools[0], "/opened-parent")
        .unwrap()
        .unwrap();
    assert_ne!(original_parent["id"], current_parent["id"]);
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| cdb[0] == 0x0a)
            .count()
    };
    let before = writes();
    assert!(matches!(
        client.upload_saved(&saved),
        Err(ClientError::Verification(_))
    ));
    assert_eq!(
        writes(),
        before,
        "a changed parent must fail before media admission"
    );
    assert_eq!(fs::read(saved.join("request.json")).unwrap(), header);
    assert_eq!(
        fs::read(saved.join("payload")).unwrap(),
        b"original child bytes"
    );
    assert_eq!(
        client.status_saved(&saved).unwrap(),
        UploadStatus::NotPrepared
    );
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/opened-parent/original-child")
            .unwrap()
            .is_none()
    );

    // A new operation may explicitly capture the new parent. Its version can
    // advance when siblings are added without changing that parent identity.
    let current = UploadCondition::Create {
        namespace: f.route.route.namespace,
        parent: Uuid::parse_str(current_parent["id"].as_str().unwrap()).unwrap(),
    };
    fs::write(&source, b"new child bytes").unwrap();
    for name in ["sibling", "new-child"] {
        let state = f.root.join(format!("parent-{name}"));
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                &format!("/opened-parent/{name}"),
                &source,
                current,
                None,
                &state,
            )
            .unwrap();
        assert_upload_committed(client.upload_saved(&state).unwrap(), &state, 1);
    }
    let cli_state = f.root.join("parent-cli");
    let preparation = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &server.address, "--library", &f.serial, "--pool"])
        .arg(f.pools[0].to_string())
        .args(["fdb", "prepare"])
        .arg(&source)
        .arg("/opened-parent/cli-child")
        .arg("--request")
        .arg(&cli_state)
        .output()
        .unwrap();
    assert!(
        preparation.status.success(),
        "{}",
        String::from_utf8_lossy(&preparation.stderr)
    );
    let cli_header: serde_json::Value =
        serde_json::from_slice(&fs::read(cli_state.join("request.json")).unwrap()).unwrap();
    assert_eq!(cli_header["original"]["parent"], current_parent["id"]);
    assert_upload_committed(client.upload_saved(&cli_state).unwrap(), &cli_state, 1);
    drop(metadata);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；同卷多个原上传请求准入与SimLibrary，不接触真实设备"]
fn prepared_uploads_share_head_without_losing_original_operations() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_upload_admission_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(10));
    for case in 0..4 {
        let mut requests = Vec::new();
        let mut journals = Vec::new();
        for (name, bytes) in [
            ("first", b"first body".as_slice()),
            ("second", b"second body".as_slice()),
        ] {
            let source = f.root.join(format!("{case}-{name}.source"));
            let saved = f.root.join(format!("{case}-{name}.request"));
            let path = format!("/case-{case}-{name}-prepared");
            fs::write(&source, bytes).unwrap();
            if case == 3 {
                let mut journal = UploadJournal::create(
                    &f.serial,
                    f.pools[0],
                    &path,
                    &source,
                    UploadCondition::Create {
                        namespace: f.route.route.namespace,
                        parent: f.pool_roots[0],
                    },
                    Some(BARCODES[0]),
                    &saved,
                )
                .unwrap();
                client.prepare_journal(&mut journal).unwrap();
                journals.push(journal);
            } else {
                client
                    .prepare_file(
                        &f.serial,
                        f.pools[0],
                        &path,
                        &source,
                        UploadCondition::Create {
                            namespace: f.route.route.namespace,
                            parent: f.pool_roots[0],
                        },
                        Some(BARCODES[0]),
                        &saved,
                    )
                    .unwrap();
            }
            let original: serde_json::Value =
                serde_json::from_slice(&fs::read(saved.join("request.json")).unwrap()).unwrap();
            let logical = unbound_identity(&saved);
            assert_eq!(logical.barcode.as_deref(), Some(BARCODES[0]));
            let prepared = upload_prepared(&saved);
            let raw =
                base64::Engine::decode(&URL_SAFE_NO_PAD, prepared["metadata"].as_str().unwrap())
                    .unwrap();
            let metadata: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert!(
                metadata.get("volume").is_none()
                    && metadata.get("generation").is_none()
                    && metadata.get("scan").is_none()
            );
            requests.push((saved, path, bytes.to_vec(), original["original"].clone()));
        }
        if case == 3 {
            let mut foreign = Vec::new();
            for (index, request) in requests.iter_mut().enumerate() {
                let original = request.0.clone();
                let held = f.root.join(format!("held-{index}"));
                fs::rename(&original, &held).unwrap();
                let source = f.root.join(format!("foreign-{index}"));
                fs::write(&source, b"foreign body").unwrap();
                let other = UploadJournal::create(
                    &f.serial,
                    f.pools[0],
                    &request.1,
                    &source,
                    UploadCondition::Create {
                        namespace: f.route.route.namespace,
                        parent: f.pool_roots[0],
                    },
                    Some(BARCODES[0]),
                    &original,
                )
                .unwrap();
                assert_ne!(other.operation(), journals[index].operation());
                foreign.push((
                    original.clone(),
                    fs::read(original.join("request.json")).unwrap(),
                    fs::read(original.join("payload")).unwrap(),
                ));
                drop(other);
                request.0 = held;
            }
            for index in [1, 0] {
                assert_upload_committed(
                    client.upload_journal(&mut journals[index]).unwrap(),
                    &requests[index].0,
                    1,
                );
            }
            for (path, header, body) in foreign {
                assert_eq!(fs::read(path.join("request.json")).unwrap(), header);
                assert_eq!(fs::read(path.join("payload")).unwrap(), body);
                assert!(!path.join("receipt.json").exists());
            }
        } else if case == 2 {
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    barrier.wait();
                    client.upload_saved(&requests[0].0)
                });
                let second = scope.spawn(|| {
                    barrier.wait();
                    client.upload_saved(&requests[1].0)
                });
                assert_upload_committed(first.join().unwrap().unwrap(), &requests[0].0, 1);
                assert_upload_committed(second.join().unwrap().unwrap(), &requests[1].0, 1);
            });
        } else {
            for index in if case == 0 { [1, 0] } else { [0, 1] } {
                assert_upload_committed(
                    client.upload_saved(&requests[index].0).unwrap(),
                    &requests[index].0,
                    1,
                );
            }
        }
        drop(journals);
        let mut receipts = Vec::new();
        for (saved, path, expected, original) in &requests {
            assert_eq!(
                client.status_saved(saved).unwrap(),
                UploadStatus::Committed(1)
            );
            let stored: serde_json::Value =
                serde_json::from_slice(&fs::read(saved.join("request.json")).unwrap()).unwrap();
            assert_eq!(stored["original"], *original);
            let receipt = client
                .upload_receipt(&PreparedUpload::load(saved).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(receipt["operation"], original["operation"]);
            receipts.push(receipt);
            let output = f
                .root
                .join(format!("{}.read", original["operation"].as_str().unwrap()));
            client
                .download_new(&f.serial, f.pools[0], path, &output)
                .unwrap();
            assert_eq!(fs::read(output).unwrap(), *expected);
        }
        assert_eq!(receipts[0]["volume"], receipts[1]["volume"]);
        assert_ne!(
            receipts[0]["generation"], receipts[1]["generation"],
            "each serial commit records its actual generation"
        );
    }
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

fn altered_value(
    saved: &std::path::Path,
    edit: impl FnOnce(&mut serde_json::Value),
) -> serde_json::Value {
    let mut value: serde_json::Value = upload_prepared(saved);
    let bytes =
        base64::Engine::decode(&URL_SAFE_NO_PAD, value["metadata"].as_str().unwrap()).unwrap();
    let mut identity: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    edit(&mut identity);
    value["namespace"] = identity["namespace"].clone();
    value["metadata"] = json!(base64::Engine::encode(
        &URL_SAFE_NO_PAD,
        identity.to_string()
    ));
    value
}

fn altered(
    f: &Fixture,
    saved: &std::path::Path,
    edit: impl FnOnce(&mut serde_json::Value),
) -> PreparedUpload {
    let value = altered_value(saved, edit);
    let directory = f.root.join(format!("altered-upload-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let file = directory.join("request.json");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .unwrap()
        .write_all(&upload_query_fixture(&value))
        .unwrap();
    PreparedUpload::load(&directory).unwrap()
}

fn unbound_identity(saved: &std::path::Path) -> tape_rs::managed_catalog::ActiveFileRequest {
    let value: serde_json::Value = upload_prepared(saved);
    let raw =
        base64::Engine::decode(&URL_SAFE_NO_PAD, value["metadata"].as_str().unwrap()).unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let id = |key: &str| Uuid::parse_str(metadata[key].as_str().unwrap()).unwrap();
    let path = String::from_utf8(
        base64::Engine::decode(&URL_SAFE_NO_PAD, value["path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let (parent, name) = path.rsplit_once('/').unwrap();
    let hex = metadata["sha256"].as_str().unwrap();
    let mut digest = [0u8; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    tape_rs::managed_catalog::ActiveFileRequest {
        namespace: id("namespace"),
        parent: id("parent"),
        parent_path: if parent.is_empty() {
            b"/".to_vec()
        } else {
            parent.as_bytes().to_vec()
        },
        name: name.as_bytes().to_vec(),
        file_id: id("file"),
        expected_version: metadata["previous"].as_u64(),
        operation_id: id("operation"),
        request_epoch: metadata["epoch"].as_u64().unwrap(),
        library_serial: value["library"].as_str().unwrap().into(),
        pool_id: Some(Uuid::parse_str(value["pool"].as_str().unwrap()).unwrap()),
        barcode: metadata["barcode"].as_str().map(str::to_owned),
        expected_size: metadata["size"].as_u64().unwrap(),
        expected_digest: tape_rs::managed_catalog::ContentDigest::Sha256(digest),
    }
}

// An independent pending client models a crash before its local ACK was saved.
fn pending_client_copy(f: &Fixture, source: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    assert!(!source.join("receipt.json").exists());
    let directory = f.root.join(format!("pending-client-{}", Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    for name in ["request.json", "payload"] {
        fs::copy(source.join(name), directory.join(name)).unwrap();
    }
    directory
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实SDK/CLI原上传结果与SimLibrary，不接触真实设备"]
fn original_upload_results_survive_no_owner_recreation_and_takeover_without_media_io() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_upload_status_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let source = f.root.join("original-body");
    fs::write(&source, b"original bytes").unwrap();
    let saved = f.root.join("original-upload.json");
    let original = client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/saved-upload",
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
    let pending_original = pending_client_copy(&f, &saved);
    assert_upload_committed(client.upload_saved(&saved).unwrap(), &saved, 1);
    let first = client
        .stat(&f.serial, f.pools[0], "/saved-upload")
        .unwrap()
        .unwrap();
    let first_receipt = client.upload_receipt(&original).unwrap().unwrap();
    let original_attributes = client
        .node_attributes(&f.serial, f.pools[0], "/saved-upload")
        .unwrap()
        .unwrap();
    assert_eq!(first_receipt["attributes"], original_attributes);
    assert_eq!(first_receipt["id"], first["id"]);
    let large_note = "中".repeat(30_000);
    let attr_change = f.root.join("change-before-replacement.json");
    client.prepare_node_mutation(&f.serial, f.pools[0], "/saved-upload", original_condition(&client, &f.serial, f.route.route.namespace, f.pools[0], "/saved-upload", Some(1)), json!({"kind":"set-xattr","attribute":{"key":"user.original","value":large_note,"base64":false},"mode":"create"}), &attr_change).unwrap();
    assert_eq!(
        client.run_node_mutation_saved(&attr_change).unwrap()["status"],
        "completed"
    );
    assert_eq!(
        client.upload_receipt(&original).unwrap().unwrap(),
        first_receipt
    );
    let changed_source = f.root.join("changed-body");
    fs::write(&changed_source, b"other bytes").unwrap();
    let replacement = f.root.join("replacement.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/saved-upload",
            &changed_source,
            upload_replacement(
                &client
                    .stat(&f.serial, f.pools[0], "/saved-upload")
                    .unwrap()
                    .unwrap(),
            ),
            None,
            &replacement,
        )
        .unwrap();
    let pending_replacement = pending_client_copy(&f, &replacement);
    assert_upload_committed(client.upload_saved(&replacement).unwrap(), &replacement, 3);
    let replacement_request = PreparedUpload::load(&replacement).unwrap();
    let replacement_receipt = client
        .upload_receipt(&replacement_request)
        .unwrap()
        .unwrap();
    assert!(
        replacement_receipt["attributes"]["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "user.original" && a["value"] == large_note)
    );
    let unlink = f.root.join("unlink.json");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/saved-upload",
            original_condition(
                &client,
                &f.serial,
                f.route.route.namespace,
                f.pools[0],
                "/saved-upload",
                Some(3),
            ),
            json!({"kind":"unlink"}),
            &unlink,
        )
        .unwrap();
    assert_eq!(
        client.run_node_mutation_saved(&unlink).unwrap()["status"],
        "completed"
    );
    let recreated = f.root.join("recreated.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/saved-upload",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &recreated,
        )
        .unwrap();
    assert_upload_committed(client.upload_saved(&recreated).unwrap(), &recreated, 1);
    let current = client
        .stat(&f.serial, f.pools[0], "/saved-upload")
        .unwrap()
        .unwrap();
    assert_ne!(current["id"], first["id"]);
    assert_eq!(
        current["digest"], first["digest"],
        "same bytes do not establish operation identity"
    );
    let old_target = upload_replacement(&first);
    assert!(
        matches!(
            client.prepare_file(
                &f.serial,
                f.pools[0],
                "/saved-upload",
                &source,
                old_target,
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            ),
            Err(ClientError::Verification(_))
        ),
        "same path and version must not rebind the old inode condition"
    );
    assert!(matches!(
        client.prepare_file(
            &f.serial,
            f.pools[0],
            "/wrong-namespace-target",
            &source,
            UploadCondition::Create {
                namespace: Uuid::new_v4(),
                parent: f.pool_roots[0]
            },
            None,
            &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
        ),
        Err(ClientError::Verification(_))
    ));
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/saved-upload")
            .unwrap()
            .unwrap(),
        current
    );
    let never_saved = f.root.join("never-sent.json");
    let never_sent = client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/never-sent",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &never_saved,
        )
        .unwrap();

    let wrong_fingerprint = altered(&f, &saved, |identity| {
        identity["size"] = json!(identity["size"].as_u64().unwrap() + 1)
    });
    let wrong_namespace = altered(&f, &saved, |identity| {
        identity["namespace"] = json!(Uuid::new_v4().to_string())
    });
    let zero_previous = altered_value(&saved, |identity| identity["previous"] = json!(0));
    let other_operation = altered(&f, &saved, |identity| {
        identity["operation"] = json!(Uuid::new_v4().to_string())
    });
    let readonly = reader(&f);
    drop(server);
    owner.permission.close(12);
    let trace = owner.trace.clone();
    drop(owner);
    let count = trace.lock().unwrap().len();
    let queries = FdbClient::new([&readonly.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(3));
    assert_eq!(
        queries.status(&original).unwrap(),
        UploadStatus::Committed(1)
    );
    let raw_route = format!(
        "/v1/fdb/uploads/{}?pool={}&path={}",
        f.serial,
        zero_previous["pool"].as_str().unwrap(),
        zero_previous["path"].as_str().unwrap()
    );
    let raw_headers = format!(
        "x-tapers-fdb-namespace: {}\r\nx-tapers-fdb-write: {}\r\n",
        zero_previous["namespace"].as_str().unwrap(),
        zero_previous["metadata"].as_str().unwrap()
    );
    assert_eq!(
        readonly.request("GET", &raw_route, Some(&raw_headers)).0,
        400,
        "zero previous must not alias the original create-only request"
    );
    let original_header = upload_prepared(&saved);
    let receipt_route = format!(
        "/v1/fdb/upload-receipts/{}?pool={}&path={}",
        f.serial,
        original_header["pool"].as_str().unwrap(),
        original_header["path"].as_str().unwrap()
    );
    let receipt_headers = format!(
        "x-tapers-fdb-namespace: {}\r\nx-tapers-fdb-write: {}\r\n",
        original_header["namespace"].as_str().unwrap(),
        original_header["metadata"].as_str().unwrap()
    );
    let (receipt_status, receipt_body, _) =
        readonly.request("GET", &receipt_route, Some(&receipt_headers));
    assert_eq!(
        receipt_status, 200,
        "original detailed receipt: {receipt_body}"
    );
    assert_eq!(
        queries.upload_receipt(&original).unwrap().unwrap(),
        first_receipt
    );
    assert_eq!(
        queries
            .upload_receipt(&replacement_request)
            .unwrap()
            .unwrap(),
        replacement_receipt
    );
    assert!(queries.upload_receipt(&never_sent).unwrap().is_none());
    assert!(queries.upload_receipt(&other_operation).unwrap().is_none());
    assert!(matches!(
        queries.upload_receipt(&wrong_fingerprint),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        queries.upload_receipt(&wrong_namespace),
        Err(ClientError::Rejected { status: 400, .. })
    ));
    // Damage only this fixture's immutable evidence, then restore exact bytes.
    // A committed lightweight ACK must never be used to repair detailed data.
    let raw_db = foundationdb::Database::new(Some(&cluster)).unwrap();
    let original_identity = unbound_identity(&saved);
    let mut header_key = b"tape-rs/catalog/22/@/".to_vec();
    header_key.extend_from_slice(original_identity.namespace.as_bytes());
    header_key.push(b'u');
    header_key.extend_from_slice(&original_identity.request_epoch.to_be_bytes());
    header_key.extend_from_slice(original_identity.operation_id.as_bytes());
    let mut body_key = header_key.clone();
    body_key.push(0);
    let mut large_key = b"tape-rs/catalog/22/M/".to_vec();
    large_key.extend_from_slice(original_identity.namespace.as_bytes());
    let digest = replacement_receipt["attributes"]["attributes_digest"]
        .as_str()
        .unwrap();
    for i in 0..32 {
        large_key.push(u8::from_str_radix(&digest[2 * i..2 * i + 2], 16).unwrap());
    }
    large_key.extend_from_slice(&[1, 0, 0]);
    let offline = FdbClient::new(["127.0.0.1:0"])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_millis(50));
    for (key, original_request, saved_directory, acknowledged_directory) in [
        (&header_key, &original, &pending_original, &saved),
        (&body_key, &original, &pending_original, &saved),
        (
            &large_key,
            &replacement_request,
            &pending_replacement,
            &replacement,
        ),
    ] {
        let header: serde_json::Value = upload_prepared(saved_directory);
        let local_header = fs::read(saved_directory.join("request.json")).unwrap();
        let local_payload = fs::read(saved_directory.join("payload")).unwrap();
        let headers = format!(
            "x-tapers-fdb-namespace: {}\r\nx-tapers-fdb-write: {}\r\n",
            header["namespace"].as_str().unwrap(),
            header["metadata"].as_str().unwrap()
        );
        let route = format!(
            "/v1/fdb/upload-receipts/{}?pool={}&path={}",
            f.serial,
            header["pool"].as_str().unwrap(),
            header["path"].as_str().unwrap()
        );
        let saved_value = rt.block_on(async {
            let tr = raw_db.create_trx().unwrap();
            tr.get(key, false).await.unwrap().unwrap().to_vec()
        });
        for replacement in [None, Some(b"corrupt original evidence".as_slice())] {
            rt.block_on(async {
                let tr = raw_db.create_trx().unwrap();
                if let Some(value) = replacement {
                    tr.set(key, value);
                } else {
                    tr.clear(key);
                }
                tr.commit().await.unwrap();
            });
            assert_eq!(readonly.request("GET", &route, Some(&headers)).0, 503);
            assert!(queries.upload_receipt(original_request).is_err());
            assert!(queries.upload_saved(saved_directory).is_err());
            let acknowledged = offline.upload_saved(acknowledged_directory).unwrap();
            let expected = if acknowledged_directory == &saved {
                &first_receipt
            } else {
                &replacement_receipt
            };
            assert_eq!(acknowledged.details(), expected);
            assert_eq!(
                offline
                    .saved_receipt(acknowledged_directory)
                    .unwrap()
                    .unwrap()
                    .details(),
                expected
            );
            assert!(!acknowledged_directory.join("payload").exists());
            assert!(!saved_directory.join("receipt.json").exists());
            assert_eq!(
                fs::read(saved_directory.join("request.json")).unwrap(),
                local_header
            );
            assert_eq!(
                fs::read(saved_directory.join("payload")).unwrap(),
                local_payload
            );
            assert_eq!(
                queries.status(&original).unwrap(),
                UploadStatus::Committed(1)
            );
            let unchanged = rt.block_on(async {
                let tr = raw_db.create_trx().unwrap();
                tr.get(key, false).await.unwrap().map(|v| v.to_vec())
            });
            assert_eq!(
                unchanged.as_deref(),
                replacement,
                "query must not repair original evidence"
            );
        }
        rt.block_on(async {
            let tr = raw_db.create_trx().unwrap();
            tr.set(key, &saved_value);
            tr.commit().await.unwrap();
        });
    }
    let pending_header = fs::read(pending_original.join("request.json")).unwrap();
    let pending_body = fs::read(pending_original.join("payload")).unwrap();
    assert_eq!(
        queries
            .saved_receipt(&pending_original)
            .unwrap()
            .unwrap()
            .details(),
        &first_receipt
    );
    assert!(!pending_original.join("receipt.json").exists());
    assert_eq!(
        fs::read(pending_original.join("request.json")).unwrap(),
        pending_header
    );
    assert_eq!(
        fs::read(pending_original.join("payload")).unwrap(),
        pending_body
    );
    assert_eq!(
        queries.upload_saved(&pending_original).unwrap().details(),
        &first_receipt
    );
    assert!(pending_original.join("receipt.json").is_file());
    assert!(!pending_original.join("payload").exists());
    assert_eq!(
        queries.upload_receipt(&original).unwrap().unwrap(),
        first_receipt
    );
    assert_eq!(
        queries
            .upload_receipt(&replacement_request)
            .unwrap()
            .unwrap(),
        replacement_receipt
    );
    let unbound = unbound_identity(&saved);
    assert!(
        rt.block_on(f.catalog.active_upload_receipt(&unbound, f.route, 1))
            .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.active_upload_result(&unbound, f.route))
            .unwrap(),
        Some(tape_rs::managed_catalog::PublishOutcome::AlreadyApplied(1))
    );
    // A logical query request has no execution owner or physical target, so
    // it cannot be passed to reserve_active_file's physical intent API.

    assert_eq!(
        queries.status(&never_sent).unwrap(),
        UploadStatus::Unconfirmed
    );
    assert!(matches!(
        queries.upload_saved(&never_saved),
        Err(ClientError::Rejected { status: 405, .. })
    ));
    assert_eq!(
        queries.status(&never_sent).unwrap(),
        UploadStatus::Unconfirmed
    );
    assert_eq!(
        queries.status(&other_operation).unwrap(),
        UploadStatus::Unconfirmed
    );
    assert!(matches!(
        queries.status(&wrong_fingerprint),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(matches!(
        queries.status(&wrong_namespace),
        Err(ClientError::Rejected { status: 400, .. })
    ));
    assert_upload_committed(queries.upload_saved(&saved).unwrap(), &saved, 1);
    assert_eq!(
        queries
            .stat(&f.serial, f.pools[0], "/saved-upload")
            .unwrap()
            .unwrap(),
        current
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &readonly.address, "fdb", "status"])
        .arg(&saved)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("FDB 版本 1"));
    let detailed = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", "127.0.0.1:0", "fdb", "status", "--details"])
        .arg(&saved)
        .output()
        .unwrap();
    assert!(
        detailed.status.success(),
        "{}",
        String::from_utf8_lossy(&detailed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&detailed.stdout).unwrap(),
        first_receipt
    );
    assert_eq!(fs::read(saved.join("request.json")).unwrap(), bytes);
    assert_eq!(
        trace.lock().unwrap().len(),
        count,
        "read-only upload queries must issue zero device commands"
    );
    let owner = Owner::start(&f, 13, json!({}));
    assert_eq!(
        queries.status(&original).unwrap(),
        UploadStatus::Committed(1)
    );
    assert_eq!(
        queries.status(&never_sent).unwrap(),
        UploadStatus::Unconfirmed
    );
    let serving = writer(&f, &owner, 13);
    let output = f.root.join("read-current");
    FdbClient::new([&serving.address])
        .unwrap()
        .download_new(&f.serial, f.pools[0], "/saved-upload", &output)
        .unwrap();
    assert_eq!(fs::read(&output).unwrap(), b"original bytes");
    assert_eq!(
        queries
            .stat(&f.serial, f.pools[0], "/saved-upload")
            .unwrap()
            .unwrap()["id"],
        current["id"]
    );
    drop(serving);
    drop(readonly);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；真实SDK/默认CLI、SimLibrary及原正文目录，不访问真实设备"]
fn saved_upload_body_survives_source_changes_no_owner_and_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_upload_journal_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address]).unwrap();
    let source = f.root.join("original-source");
    let original = b"original bytes persisted before admission";
    fs::write(&source, original).unwrap();
    let state = f.root.join("original-upload");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/frozen-original",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            Some(BARCODES[0]),
            &state,
        )
        .unwrap();
    let header = fs::read(state.join("request.json")).unwrap();
    let identity = unbound_identity(&state);
    fs::write(&source, b"mutated source with different contents").unwrap();
    fs::remove_file(&source).unwrap();
    assert_eq!(fs::read(state.join("payload")).unwrap(), original);
    drop(server);
    drop(owner);
    let metadata = reader(&f);
    let queries = FdbClient::new([&metadata.address]).unwrap();
    let draft_source = f.root.join("draft-source");
    let draft = f.root.join("unprepared-upload");
    fs::write(&draft_source, b"original draft bytes before prepare RPC").unwrap();
    let preparation = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &metadata.address, "--library", &f.serial, "--pool"])
        .arg(f.pools[0].to_string())
        .args(["fdb", "prepare"])
        .arg(&draft_source)
        .arg("/draft-original")
        .arg("--request")
        .arg(&draft)
        .output()
        .unwrap();
    assert!(!preparation.status.success());
    assert!(
        draft.join("request.json").is_file() && draft.join("payload").is_file(),
        "failed prepare must retain original operation and independent payload: {}",
        String::from_utf8_lossy(&preparation.stderr)
    );
    let draft_header = fs::read(draft.join("request.json")).unwrap();
    let original_draft: serde_json::Value = serde_json::from_slice(&draft_header).unwrap();
    assert_eq!(original_draft["version"], 2);
    assert!(original_draft["prepared"].is_null());
    assert_eq!(
        queries.status_saved(&draft).unwrap(),
        UploadStatus::NotPrepared
    );
    assert!(matches!(
        queries.upload_saved(&draft),
        Err(ClientError::Rejected { status: 404, .. })
    ));
    assert_eq!(fs::read(draft.join("request.json")).unwrap(), draft_header);
    fs::write(&draft_source, b"changed external source").unwrap();
    fs::remove_file(&draft_source).unwrap();
    assert_eq!(
        fs::read(draft.join("payload")).unwrap(),
        b"original draft bytes before prepare RPC"
    );
    assert_eq!(
        queries
            .status(&PreparedUpload::load(&state).unwrap())
            .unwrap(),
        UploadStatus::Unconfirmed
    );
    assert!(matches!(
        queries.upload_saved(&state),
        Err(ClientError::Rejected { status: 405, .. })
    ));
    assert_eq!(fs::read(state.join("request.json")).unwrap(), header);
    assert_eq!(fs::read(state.join("payload")).unwrap(), original);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let resumed = FdbClient::new([&server.address]).unwrap();
    // Finish the already selected physical target before its volume generation advances.
    // The still-local draft selects its first physical target only afterwards.
    assert_upload_committed(resumed.upload_saved(&state).unwrap(), &state, 1);
    assert_upload_committed(resumed.upload_saved(&draft).unwrap(), &draft, 1);
    let prepared_draft: serde_json::Value =
        serde_json::from_slice(&fs::read(draft.join("request.json")).unwrap()).unwrap();
    assert_eq!(prepared_draft["original"], original_draft["original"]);
    assert_eq!(
        unbound_identity(&draft).operation_id.to_string(),
        original_draft["original"]["operation"].as_str().unwrap()
    );
    let draft_output = f.root.join("restored-draft");
    resumed
        .download_new(&f.serial, f.pools[0], "/draft-original", &draft_output)
        .unwrap();
    assert_eq!(
        fs::read(draft_output).unwrap(),
        b"original draft bytes before prepare RPC"
    );
    let published = resumed
        .stat(&f.serial, f.pools[0], "/frozen-original")
        .unwrap()
        .unwrap();
    assert_eq!(published["id"], identity.file_id.to_string());
    assert_eq!(published["version"], 1);
    let output = f.root.join("restored-original");
    resumed
        .download_new(&f.serial, f.pools[0], "/frozen-original", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), original);
    assert_eq!(fs::read(state.join("request.json")).unwrap(), header);
    drop(server);
    drop(owner);
    // A completed original result is authoritative even after its local payload is removed.
    assert!(!state.join("payload").exists());
    assert!(!draft.join("payload").exists());
    assert_upload_committed(queries.upload_saved(&draft).unwrap(), &draft, 1);
    assert_upload_committed(queries.upload_saved(&state).unwrap(), &state, 1);
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", "127.0.0.1:0", "fdb", "upload"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let reported = String::from_utf8_lossy(&result.stdout);
    let version = tape_rs::client::FileVersion::Fdb {
        namespace: identity.namespace,
        inode: identity.file_id,
        version: 1,
    };
    assert!(reported.contains(&format!("FDB 版本 {version}")));
    assert!(reported.contains(&identity.operation_id.to_string()));
    assert_eq!(fs::read(state.join("request.json")).unwrap(), header);
    drop(metadata);
    fs::remove_dir_all(&f.root).unwrap();
}
