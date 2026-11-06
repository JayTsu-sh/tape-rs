//! Native tasks through real HTTP, durable SDK requests and the device actor.
use super::*;
use tape_rs::client::fdb::PreparedManagement;

fn run_change(client: &FdbClient, f: &Fixture, change: serde_json::Value) -> serde_json::Value {
    let prepared = client
        .prepare_management(&f.serial, f.route.route.namespace, change)
        .unwrap();
    let path = f.root.join(format!("management-{}.json", Uuid::new_v4()));
    prepared.save_new(&path).unwrap();
    let value = client.run_management_saved(&path).unwrap();
    assert_eq!(client.run_management_saved(&path).unwrap(), value);
    value
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；本机 TCP、CLI 和 SimLibrary，不操作真实硬件"]
fn native_management_http_cli_retries_and_owner_resume() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let ns = f.route.route.namespace;
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
    let readonly = ReadHttpServer::start(fdb_http::owner_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
    ));
    let entry = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), context(&owner, 72, 100))]),
        HashMap::from([(71, server.address.clone())]),
        Default::default(),
    ));
    let client = FdbClient::new([&entry.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let original_pool = client
        .create_pool(&f.serial, ns, f.pools[0], "policy", 200_000)
        .unwrap();
    let original_root = client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap();
    let reader = FdbClient::new([&readonly.address]).unwrap();
    let prepared = reader
        .prepare_management(
            &f.serial,
            ns,
            json!({"kind":"drive","serial":"SIMDRV0000","revision":0,"patch":{"role":"r"}}),
        )
        .unwrap();
    let path = f.root.join("drive.json");
    prepared.save_new(&path).unwrap();
    assert!(matches!(
        reader.run_management_saved(&path),
        Err(tape_rs::client::ClientError::Rejected { status: 405, .. })
    ));
    assert!(reader.management_status(&prepared).unwrap().is_none());
    let created = client.run_management_saved(&path).unwrap();
    assert_eq!(created["status"], "succeeded");
    assert_eq!(client.run_management_saved(&path).unwrap(), created);
    let page = client.list_management(&f.serial, 1, None).unwrap();
    assert_eq!(page["items"][0]["operation"], prepared.value()["operation"]);
    let policy = run_change(
        &client,
        &f,
        json!({"kind":"pool","id":f.pools[0].to_string(),"revision":1,"patch":{"mount_limit":1}}),
    );
    assert_eq!(policy["result"]["pool"]["revision"], 2);
    run_change(
        &client,
        &f,
        json!({"kind":"drive","serial":"SIMDRV0000","revision":1,"patch":{"enabled":false,"role":"m"}}),
    );
    let snapshot = rt
        .block_on(f.catalog.active_management_policy(
            &f.serial,
            f.route,
            100,
            &["SIMDRV0000".into()],
            &[f.pools[0]],
            &[],
        ))
        .unwrap();
    assert!(snapshot.get("nodes").is_none());
    assert_eq!(snapshot["drives"]["SIMDRV0000"]["enabled"], false);
    assert_eq!(snapshot["pools"][f.pools[0].to_string()]["mountlimit"], "1");
    assert!(
        ctx.status
            .lock()
            .unwrap()
            .management
            .as_object()
            .unwrap()
            .is_empty()
    );
    let cli_request = f.root.join("cli.json");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &entry.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "pool",
            "set",
            &f.pools[0].to_string(),
            "--patch",
            r#"{"name":"renamed","mount_limit":2}"#,
            "--request",
            cli_request.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let done: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(done["result"]["pool"]["policy"]["mount_limit"], 2);
    assert_eq!(done["result"]["pool"]["name"], "renamed");
    assert_eq!(
        done["result"]["pool"]["created_at_millis"],
        original_pool["pool"]["created_at_millis"]
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap(),
        original_root
    );
    let latest = run_change(
        &client,
        &f,
        json!({"kind":"pool","id":f.pools[0].to_string(),"revision":3,"patch":{"name":"final-name"}}),
    );
    assert_eq!(latest["result"]["pool"]["revision"], 4);
    assert_eq!(client.run_management_saved(&cli_request).unwrap(), done);
    assert_eq!(
        client.pool(&f.serial, f.pools[0]).unwrap().unwrap()["pool"]["name"],
        "final-name"
    );
    client
        .create_pool(&f.serial, ns, f.pools[1], "policy", 17)
        .unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &entry.address,
            "fdb",
            "control",
            "status",
            cli_request.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        done
    );
    let listed = client.list_management(&f.serial, 1, None).unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert!(listed["next"].is_string());

    // A lost POST response causes byte-identical admission under the original
    // UUID, whether the actor has already applied it or it is still queued.
    let retry = client
        .prepare_management(
            &f.serial,
            ns,
            json!({"kind":"drive","serial":"SIMDRV0001","revision":0,"patch":{"role":"r"}}),
        )
        .unwrap();
    let retry_path = f.root.join("lost.json");
    retry.save_new(&retry_path).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let target = server.address.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let proxy = std::thread::spawn(move || {
        let mut posts = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut header = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                header.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length = header
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            assert!(length <= 4096);
            let post = header.starts_with("POST ");
            let mut bytes = header.into_bytes();
            let offset = bytes.len();
            bytes.resize(offset + length, 0);
            reader.read_exact(&mut bytes[offset..]).unwrap();
            drop(reader);
            let mut upstream = std::net::TcpStream::connect(&target).unwrap();
            upstream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            upstream.write_all(&bytes).unwrap();
            let mut response = Vec::new();
            upstream.read_to_end(&mut response).unwrap();
            if post {
                posts.push(bytes);
                if posts.len() == 1 {
                    continue;
                }
            }
            stream.write_all(&response).unwrap();
        }
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0], posts[1]);
    });
    let proxy_client = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let lost = proxy_client.run_management_saved(&retry_path).unwrap();
    assert_eq!(lost["status"], "succeeded");
    stop.store(true, Ordering::SeqCst);
    proxy.join().unwrap();
    assert_eq!(
        client
            .management_policy(&f.serial, "drive", "SIMDRV0001")
            .unwrap()["policy"]["revision"],
        1
    );

    // Queue under owner 100, revoke Serving, and let owner 101 resume it.
    let pending = client
        .prepare_management(
            &f.serial,
            ns,
            json!({"kind":"drive","serial":"SIMDRV0001","revision":1,"patch":{"enabled":false}}),
        )
        .unwrap();
    let pending_path = f.root.join("pending.json");
    pending.save_new(&pending_path).unwrap();
    owner.permission.close(100);
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(&pending_path).unwrap()).unwrap();
    let parsed = tape_rs::managed_catalog::ManagementRequest::parse(raw).unwrap();
    rt.block_on(f.catalog.admit_management(&f.serial, f.route, 100, &parsed))
        .unwrap();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(
        rt.block_on(
            f.catalog
                .management_task(&f.serial, f.route, 1, parsed.operation())
        )
        .unwrap()
        .unwrap()["status"],
        "queued"
    );
    assert!(matches!(
        client.cancel_management_saved(&pending_path),
        Err(tape_rs::client::ClientError::NoLeader(_))
    ));
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x04 | 0x19 | 0x8d))
    );
    drop(entry);
    drop(readonly);
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
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let resumed = client.run_management_saved(&pending_path).unwrap();
    assert_eq!(client.run_management_saved(&cli_request).unwrap(), done);
    let pool_after_restart = client.pool(&f.serial, f.pools[0]).unwrap().unwrap();
    assert_eq!(pool_after_restart["pool"]["name"], "final-name");
    assert_eq!(
        pool_after_restart["pool"]["created_at_millis"],
        original_pool["pool"]["created_at_millis"]
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap(),
        original_root
    );
    assert_eq!(resumed["owner_epoch"], 101);
    assert_eq!(
        client.cancel_management_saved(&pending_path).unwrap(),
        resumed
    );
    assert_eq!(
        client
            .management_status(&PreparedManagement::load(&path).unwrap())
            .unwrap(),
        Some(created)
    );
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x04 | 0x19 | 0x8d))
    );
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
