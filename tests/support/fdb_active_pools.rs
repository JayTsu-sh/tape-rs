use super::*;
use base64::Engine as _;
use std::collections::BTreeMap;
use std::io::BufRead as _;
use tape_rs::client::fdb::FdbClient;
use tape_rs::daemon::{fdb_http, http::HttpContext, s3, state::Executor};

async fn fixture(cluster: &str) -> Fixture {
    let catalog = Arc::new(ManagedCatalog::open(Some(cluster)).unwrap());
    let namespace = Uuid::new_v4();
    let challenge = Uuid::new_v4();
    let serial = format!("SIM-POOLS-{namespace}");
    let replies=(1..=3).map(|id| json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,
        "role":if id==2 {"Leader"} else {"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},
        "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let proof =
        tape_rs::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
    let route = catalog
        .initialize_native_library(namespace, &proof)
        .await
        .unwrap();
    let root = std::env::temp_dir().join(format!("tape-rs-active-pools-{namespace}"));
    fs::create_dir(&root).unwrap();
    let library = SimLibrary::new(2, 8, 1);
    library.enable_changer_pr();
    Fixture {
        root,
        library,
        catalog,
        serial,
        route,
        pools: [Uuid::new_v4(), Uuid::new_v4()],
        pool_roots: [Uuid::nil(); 2],
    }
}

pub(super) fn context(owner: &Owner, id: u64, round: u64) -> Arc<HttpContext> {
    let mut status = owner.status.lock().unwrap().clone();
    status.id = id;
    status.executor = Some(Executor {
        node: 71,
        round,
        term: 7,
        fenced: true,
    });
    status.executor_ready = true;
    Arc::new(HttpContext {
        files: owner.files.clone(),
        status: Arc::new(Mutex::new(status)),
        exec: Mutex::new(owner.tx.clone()),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(5),
    })
}

fn put(server: &ReadHttpServer, f: &Fixture, id: Uuid, value: &serde_json::Value) -> u16 {
    let mut input = value.clone();
    let namespace = input.as_object_mut().unwrap().remove("namespace").unwrap();
    input["version"] = json!(1);
    input["id"] = json!(id.to_string());
    input["created_at_millis"] = json!(1_793_405_000_123u64);
    input["policy"] = json!({"media_restriction":"none","mount_limit":0,
        "low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true});
    let body = input.to_string();
    server
        .request_body(
            "POST",
            &format!("/libraries/{}/admin/pools", f.serial),
            Some(&format!(
                "Content-Type: application/json\r\nx-tapers-catalog-namespace: {}\r\nContent-Length: {}\r\n",
                namespace.as_str().unwrap(), body.len()
            )),
            body.as_bytes(),
        )
        .0
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟空库、真实 HTTP/CLI，可用 TAPE_RS_S3_PYTHON 验证 boto3"]
fn active_pool_http_cli_and_s3_use_real_creation_time() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut f = rt.block_on(fixture(&cluster));
    let ns = f.route.route.namespace;
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            routes.clone(),
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let remote = context(&owner, 72, 100);
    let entry = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            routes.clone(),
            BTreeMap::from([(f.serial.clone(), remote.clone())]),
            HashMap::from([(71, server.address.clone())]),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let readonly = ReadHttpServer::start(
        fdb_http::owner_router(
            f.catalog.clone(),
            routes.clone(),
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([&entry.address])
        .unwrap()
        .with_timeouts(Duration::from_millis(200), Duration::from_secs(5));

    let first = client
        .create_pool(
            &f.serial,
            ns,
            f.pools[0],
            "alpha",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    let time = first["pool"]["created_at_millis"].as_u64().unwrap();
    assert_eq!(time, 1_793_405_000_123);
    let retry = client
        .create_pool(
            &f.serial,
            ns,
            f.pools[0],
            "alpha",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    assert_eq!(first["created"], true);
    assert_eq!(retry["created"], false);
    assert_eq!(first["pool"], retry["pool"]);
    assert_eq!(
        client.pool_by_id(&f.serial, f.pools[0]).unwrap().unwrap()["pool"],
        first["pool"]
    );
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap()["kind"],
        "directory"
    );
    for (id, name, limit) in [
        (Uuid::new_v4(), "alpha", 200_000),
        (f.pools[0], "different", 200_000),
        (f.pools[0], "alpha", 7),
    ] {
        assert!(matches!(
            client.create_pool(&f.serial, ns, id, name, limit, 1_793_405_000_123),
            Err(tape_rs::client::ClientError::Rejected { status: 409, .. })
        ));
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &entry.address,
            "--library",
            &f.serial,
            "pool",
            "create",
            "bravo",
            "--request",
            f.root.join("create-bravo.json").to_str().unwrap(),
            "--file-limit",
            "7",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let second: serde_json::Value =
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    f.pools[1] = second["pool"]["id"].as_str().unwrap().parse().unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join("create-bravo.json")).unwrap()).unwrap();
    assert_eq!(second["pool"]["id"], saved["request"]["id"]);
    assert_eq!(second["pool"]["file_limit"], 7);
    let page = client.list_pools(&f.serial, 1, None).unwrap();
    let token = page["next"].as_str().unwrap();
    let next = client.list_pools(&f.serial, 1, Some(token)).unwrap();
    assert!(next["next"].is_null());
    assert!(page["items"][0]["id"].as_str() < next["items"][0]["id"].as_str());
    let mut bad = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .unwrap();
    let namespace_at = bad
        .windows(16)
        .position(|v| v == ns.as_bytes())
        .expect("cursor binds namespace");
    bad[namespace_at] ^= 1;
    let bad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bad);
    assert!(matches!(
        client.list_pools(&f.serial, 1, Some(&bad)),
        Err(tape_rs::client::ClientError::Rejected { status: 400, .. })
    ));
    let body = json!({"namespace":ns.to_string(),"name":"readonly","file_limit":1});
    let readonly_pool = Uuid::new_v4();
    assert_eq!(put(&readonly, &f, readonly_pool, &body), 201);
    assert!(
        client
            .pool_by_id(&f.serial, readonly_pool)
            .unwrap()
            .is_some(),
        "owner-readonly permits pool management while file writes stay disabled"
    );
    let cleanup = client
        .prepare_management(
            &f.serial,
            tape_rs::client::fdb::management::ManagementIntent::Delete {
                selector: readonly_pool.to_string(),
            },
        )
        .unwrap();
    let cleanup_path = f.root.join("readonly-pool-delete.json");
    cleanup.save_new(&cleanup_path).unwrap();
    assert_eq!(
        client.run_management_saved(&cleanup_path).unwrap()["status"],
        "succeeded"
    );
    assert_eq!(
        put(
            &server,
            &f,
            Uuid::new_v4(),
            &json!({"namespace":Uuid::new_v4().to_string(),"name":"wrong-ns","file_limit":1})
        ),
        409
    );
    let invalid = Uuid::new_v4();
    assert_eq!(
        put(
            &server,
            &f,
            invalid,
            &json!({"namespace":ns.to_string(),"name":"a/b","file_limit":1})
        ),
        400
    );
    assert!(
        rt.block_on(f.catalog.active_pool_for_route(&f.serial, f.route, invalid))
            .unwrap()
            .is_none()
    );
    assert!(
        ctx.status.lock().unwrap().pools.is_empty(),
        "新池不能依赖旧管理目录"
    );
    let s3server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        ctx.clone(),
        HashMap::new(),
        false,
    ));
    let listed = s3server.request("GET", "/", None);
    assert_eq!(listed.0, 200);
    let text = String::from_utf8(listed.2).unwrap();
    let mut expected = serde_json::Map::new();
    for pool in [&first, &second] {
        let id = pool["pool"]["id"].as_str().unwrap();
        let bucket = s3::bucket_name(&f.serial, id);
        let date = pool["pool"]["created_at"].as_str().unwrap();
        assert!(text.contains(&format!(
            "<Name>{bucket}</Name><CreationDate>{date}</CreationDate>"
        )));
        expected.insert(bucket, json!(date));
    }
    if let Ok(python) = std::env::var("TAPE_RS_S3_PYTHON") {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fdb_pools_s3.py");
        let result = std::process::Command::new(python)
            .arg(script)
            .arg(format!("http://{}", s3server.address))
            .arg(serde_json::Value::Object(expected).to_string())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        println!("{}", String::from_utf8_lossy(&result.stdout));
    }
    // The proxy commits the first PUT but drops its response. The client must
    // resend exactly the same request, receiving the original pool and time.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let target = server.address.clone();
    let proxy_thread = std::thread::spawn(move || {
        let mut previous = None;
        for attempt in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("proxy accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse::<usize>()
                .unwrap();
            assert!(length < 4096);
            let mut request = headers.into_bytes();
            let start = request.len();
            request.resize(start + length, 0);
            reader.read_exact(&mut request[start..]).unwrap();
            drop(reader);
            if let Some(previous) = &previous {
                assert_eq!(previous, &request)
            } else {
                previous = Some(request.clone())
            }
            let mut upstream = std::net::TcpStream::connect(&target).unwrap();
            upstream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            upstream.write_all(&request).unwrap();
            let response = read_proxy_response(&mut upstream);
            let status = std::str::from_utf8(&response)
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            assert_eq!(status, if attempt == 0 { "201" } else { "200" });
            if attempt == 1 {
                stream.write_all(&response).unwrap()
            }
        }
    });
    let lost_id = Uuid::new_v4();
    let lost = FdbClient::new([proxy])
        .unwrap()
        .with_timeouts(Duration::from_secs(3), Duration::from_secs(5))
        .create_pool(
            &f.serial,
            ns,
            lost_id,
            "lost-response",
            11,
            1_793_405_000_123,
        )
        .unwrap();
    proxy_thread.join().unwrap();
    assert_eq!(
        client.pool_by_id(&f.serial, lost_id).unwrap().unwrap()["pool"],
        lost["pool"]
    );
    owner.permission.close(100);
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x04 | 0x19 | 0x8d))
    );
    assert_eq!(put(&server, &f, Uuid::new_v4(), &body), 503);
    assert_eq!(s3server.request("GET", "/", None).0, 503);
    assert_eq!(
        client.list_pools(&f.serial, 100, None).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    drop(entry);
    drop(readonly);
    drop(server);
    drop(s3server);
    drop(ctx);
    drop(remote);
    drop(owner);

    let owner = Owner::start_mode(&f, 101, json!({}), true);
    let ctx = context(&owner, 71, 101);
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            routes,
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([&server.address]).unwrap();
    assert_eq!(
        client
            .create_pool(
                &f.serial,
                ns,
                f.pools[0],
                "alpha",
                200_000,
                1_793_405_000_123
            )
            .unwrap()["pool"],
        first["pool"]
    );
    assert_eq!(
        client
            .create_pool(
                &f.serial,
                ns,
                lost_id,
                "lost-response",
                11,
                1_793_405_000_123
            )
            .unwrap(),
        lost
    );
    rt.block_on(f.catalog.claim_active_owner_epoch(&f.serial, 200))
        .unwrap();
    let fenced = Uuid::new_v4();
    assert_eq!(put(&server, &f, fenced, &body), 503);
    assert!(
        rt.block_on(f.catalog.active_pool_for_route(&f.serial, f.route, fenced))
            .unwrap()
            .is_none()
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

#[path = "fdb_management.rs"]
mod management;

#[path = "fdb_intake.rs"]
mod intake;
#[path = "fdb_inventory.rs"]
mod inventory;
#[path = "fdb_pool_lifecycle.rs"]
mod lifecycle;
#[path = "fdb_node_baseline.rs"]
mod node_baseline;
