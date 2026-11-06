use super::*;
use std::collections::BTreeMap;
use tape_rs::client::fdb::FdbClient;
use tape_rs::daemon::fdb_http;

fn patch(client: &FdbClient, f: &Fixture, change: serde_json::Value) {
    let prepared = client
        .prepare_control(&f.serial, f.route.route.namespace, change)
        .unwrap();
    let path = f.root.join(format!("policy-{}.json", Uuid::new_v4()));
    prepared.save_new(&path).unwrap();
    assert_eq!(
        client.run_management_saved(&path).unwrap()["status"],
        "succeeded"
    );
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；只使用两台模拟驱动器"]
fn fdb_policies_control_real_selection_and_file_limit() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    // Deliberately contradictory frozen status must not disable native policy.
    let owner = Owner::start_mode(
        &f,
        12,
        json!({"drives":{"SIMDRV0000":{"enabled":false},"SIMDRV0001":{"enabled":false}}}),
        true,
    );
    let ctx = active_pools::context(&owner, 71, 12);
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let source = f.root.join("new-body");
    fs::write(&source, b"new-body").unwrap();
    let path = f.root.join("upload-limit.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/limit-one",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            Some(BARCODES[0]),
            &path,
        )
        .unwrap();

    patch(
        &client,
        &f,
        json!({"kind":"pool","id":f.pools[0].to_string(),"revision":1,"patch":{"file_limit":1}}),
    );
    let before = owner
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c[0] == 0x0a)
        .count();
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/no-room",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_err()
    );
    assert!(
        client.upload_saved(&path).is_err(),
        "旧 prepare 也必须重验当前上限"
    );
    assert_eq!(
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x0a)
            .count(),
        before
    );
    patch(
        &client,
        &f,
        json!({"kind":"pool","id":f.pools[0].to_string(),"revision":2,"patch":{"file_limit":2}}),
    );
    assert_upload_committed(client.upload_saved(&path).unwrap(), &path, 1);
    let first_receipt = client
        .upload_receipt(&tape_rs::client::fdb::PreparedUpload::load(&path).unwrap())
        .unwrap()
        .unwrap();
    let volume = Uuid::parse_str(first_receipt["volume"].as_str().unwrap()).unwrap();
    assert_eq!(
        rt.block_on(
            f.catalog
                .active_volume_file_count(&f.serial, f.route, 12, volume)
        )
        .unwrap(),
        2
    );
    assert_upload_committed(client.upload_saved(&path).unwrap(), &path, 1);
    let next_path = f.root.join("other.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/other-tape",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &next_path,
        )
        .unwrap();

    assert_upload_committed(client.upload_saved(&next_path).unwrap(), &next_path, 1);
    let next_receipt = client
        .upload_receipt(&tape_rs::client::fdb::PreparedUpload::load(&next_path).unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(
        next_receipt["volume"], first_receipt["volume"],
        "实际自动选带必须跳过满文件数卷"
    );
    patch(
        &client,
        &f,
        json!({"kind":"pool","id":f.pools[0].to_string(),"revision":3,"patch":{"file_limit":10,"media_restriction":"L9"}}),
    );
    let wrong = client.prepare_file(
        &f.serial,
        f.pools[0],
        "/wrong-media",
        &source,
        UploadCondition::Create {
            namespace: f.route.route.namespace,
            parent: f.pool_roots[0],
        },
        Some(BARCODES[0]),
        &f.root.join(format!("prepare-only-{}", Uuid::new_v4())),
    );
    assert!(wrong.is_err());
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/l9",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                Some(BARCODES[3]),
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_ok()
    );
    for serial in ["SIMDRV0000", "SIMDRV0001"] {
        patch(
            &client,
            &f,
            json!({"kind":"drive","serial":serial,"revision":0,"patch":{"role":"r"}}),
        );
    }
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/read-role-only",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_err()
    );
    // Read roles remain usable even after a tape reaches the write limit.
    let output = f.root.join("download");
    assert_eq!(
        client
            .download_new(&f.serial, f.pools[0], "/limit-one", &output)
            .unwrap(),
        8
    );
    assert_eq!(fs::read(output).unwrap(), b"new-body");
    patch(
        &client,
        &f,
        json!({"kind":"drive","serial":"SIMDRV0000","revision":1,"patch":{"role":"m","enabled":false}}),
    );
    assert!(
        client
            .prepare_file(
                &f.serial,
                f.pools[0],
                "/disabled",
                &source,
                UploadCondition::Create {
                    namespace: f.route.route.namespace,
                    parent: f.pool_roots[0]
                },
                None,
                &f.root.join(format!("prepare-only-{}", Uuid::new_v4()))
            )
            .is_err()
    );
    patch(
        &client,
        &f,
        json!({"kind":"drive","serial":"SIMDRV0000","revision":2,"patch":{"enabled":true}}),
    );
    let l9_path = f.root.join("l9.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/l9-policy",
            &source,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            Some(BARCODES[3]),
            &l9_path,
        )
        .unwrap();

    assert_upload_committed(client.upload_saved(&l9_path).unwrap(), &l9_path, 1);
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
