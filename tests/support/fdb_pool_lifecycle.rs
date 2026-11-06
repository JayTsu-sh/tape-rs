//! Pool lifecycle through the real local HTTP/CLI and SimLibrary.
use super::*;
use tape_rs::client::fdb::PreparedManagement;

fn deletion(client: &FdbClient, f: &Fixture, pool: Uuid) -> PreparedManagement {
    let current = client.pool(&f.serial, pool).unwrap().unwrap();
    client.prepare_management(&f.serial, f.route.route.namespace,
        json!({"kind":"pool-delete","id":pool.to_string(),"revision":current["pool"]["revision"]})).unwrap()
}

pub(super) fn no_media_changes(owner: &Owner, start: usize) {
    assert!(
        !owner.trace.lock().unwrap()[start..]
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0x8d | 0xa5 | 0x04 | 0x19)),
        "删除池和终态重试不能读取、移动或修改介质"
    );
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；空池删除、默认CLI、真实模拟空带归属及接任，不操作真实设备"]
fn empty_pool_delete_http_cli_retires_ids_and_preserves_assigned_media() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let barcode = "EMPTY0L8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    mkltfs(
        &f.library.drive(0),
        &MkltfsOptions {
            volume_id: "DEL001".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    fixture_return_to_storage(&f.library, 0);
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let entry = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), context(&owner, 72, 100))]),
        HashMap::from([(71, server.address.clone())]),
        Default::default(),
    ));
    let s3server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        ctx.clone(),
        HashMap::new(),
        false,
    ));
    let client = FdbClient::new([&entry.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(10));
    let ns = f.route.route.namespace;
    client
        .create_pool(&f.serial, ns, f.pools[0], "delete-me", 200_000)
        .unwrap();
    client
        .create_pool(&f.serial, ns, f.pools[1], "assigned", 200_000)
        .unwrap();
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    assert_eq!(
        s3server
            .request("GET", &format!("/{bucket}?list-type=2"), None)
            .0,
        200
    );
    let start = owner.trace.lock().unwrap().len();
    let saved = f.root.join("delete.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &entry.address,
            "--library",
            &f.serial,
            "pool",
            "delete",
            &f.pools[0].to_string(),
            "--request",
            saved.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let done: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["result"]["deleted"], true);
    assert!(client.pool(&f.serial, f.pools[0]).unwrap().is_none());
    assert_eq!(
        s3server
            .request("GET", &format!("/{bucket}?list-type=2"), None)
            .0,
        404
    );
    assert!(
        !String::from_utf8(s3server.request("GET", "/", None).2)
            .unwrap()
            .contains(&bucket)
    );
    assert!(matches!(
        client.create_pool(&f.serial, ns, f.pools[0], "delete-me", 200_000),
        Err(tape_rs::client::ClientError::Rejected { status: 409, .. })
    ));
    let replacement = Uuid::new_v4();
    client
        .create_pool(&f.serial, ns, replacement, "delete-me", 200_000)
        .unwrap();
    assert_eq!(client.run_management_saved(&saved).unwrap(), done);
    assert!(client.pool(&f.serial, replacement).unwrap().is_some());
    assert!(owner.permission.allows(100, Phase::Serving));
    no_media_changes(&owner, start);

    // Import an actual formatted empty Sim cartridge. An empty logical tree
    // does not permit deleting its pool while the media remains assigned.
    let import = client.prepare_management(&f.serial, ns,
        json!({"kind":"media-import","pool":f.pools[1].to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true})).unwrap();
    let import_path = f.root.join("import.json");
    import.save_new(&import_path).unwrap();
    let imported = client.run_management_saved(&import_path).unwrap();
    assert_eq!(imported["result"]["source"]["files"], 0);
    for disabled in [false, true] {
        let start = owner.trace.lock().unwrap().len();
        if disabled {
            let policy = client
                .management_policy(&f.serial, "tape", barcode)
                .unwrap();
            let value = &policy["policy"];
            let request = client.prepare_management(&f.serial, ns,
                json!({"kind":"tape","pool":value["pool"],"volume":value["volume"],"barcode":barcode,"revision":value["revision"],"patch":{"online":false,"blacklisted":true}})).unwrap();
            let path = f.root.join("disable.json");
            request.save_new(&path).unwrap();
            client.run_management_saved(&path).unwrap();
        }
        let request = deletion(&client, &f, f.pools[1]);
        let path = f.root.join(format!("assigned-{disabled}.json"));
        request.save_new(&path).unwrap();
        assert!(matches!(
            client.run_management_saved(&path),
            Err(tape_rs::client::ClientError::Rejected { status: 409, .. })
        ));
        assert_eq!(
            client.management_status(&request).unwrap().unwrap()["status"],
            "failed"
        );
        assert!(client.pool(&f.serial, f.pools[1]).unwrap().is_some());
        assert!(owner.permission.allows(100, Phase::Serving));
        no_media_changes(&owner, start);
    }
    let queued_pool = Uuid::new_v4();
    client
        .create_pool(&f.serial, ns, queued_pool, "queued-delete", 200_000)
        .unwrap();
    let request = deletion(&client, &f, queued_pool);
    let queued = f.root.join("queued.json");
    request.save_new(&queued).unwrap();
    owner.permission.close(100);
    let raw = serde_json::from_slice(&fs::read(&queued).unwrap()).unwrap();
    let parsed = tape_rs::managed_catalog::ManagementRequest::parse(raw).unwrap();
    rt.block_on(f.catalog.admit_management(&f.serial, f.route, 100, &parsed))
        .unwrap();
    assert_eq!(
        client.management_status(&request).unwrap().unwrap()["status"],
        "queued"
    );
    drop(s3server);
    drop(entry);
    drop(server);
    drop(ctx);
    drop(owner);
    let owner = Owner::start_mode(&f, 101, json!({}), true);
    let ctx = context(&owner, 71, 101);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        routes,
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(10));
    let resumed = client.run_management_saved(&queued).unwrap();
    assert_eq!(resumed["owner_epoch"], 101);
    assert_eq!(resumed["result"]["deleted"], true);
    assert_eq!(client.run_management_saved(&saved).unwrap(), done);
    assert!(client.pool(&f.serial, replacement).unwrap().is_some());
    assert!(client.pool(&f.serial, queued_pool).unwrap().is_none());
    assert!(client.pool(&f.serial, f.pools[1]).unwrap().is_some());
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
