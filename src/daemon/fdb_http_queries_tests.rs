//! Opt-in private FDB and loopback HTTP only; no device provider or EE process.
use super::*;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use uuid::Uuid;

async fn request(address: &str, path: &str, ns: Option<&str>) -> (u16, Value) {
    let socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = hyper::Request::builder().uri(path).header("host", address);
    if let Some(ns) = ns {
        request = request.header("x-tapers-catalog-namespace", ns);
    }
    let response = sender
        .send_request(
            request
                .body(http_body_util::Empty::<bytes::Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status().as_u16();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    connection.abort();
    (
        code,
        serde_json::from_slice(&raw)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&raw)})),
    )
}
async fn seed_assignment(
    db: &foundationdb::Database,
    ns: Uuid,
    pool: Uuid,
    serial: &str,
    barcode: &str,
    offline: bool,
) {
    let component = |raw: &[u8]| {
        let mut value = raw.to_vec();
        value.extend_from_slice(b"\0\0");
        value
    };
    let mut suffix = component(serial.as_bytes());
    suffix.extend_from_slice(&component(barcode.as_bytes()));
    let mut primary = b"tape-rs/catalog/22/d/".to_vec();
    primary.extend_from_slice(&suffix);
    let volume = offline.then(Uuid::new_v4);
    let mut record = vec![0; if volume.is_some() { 60 } else { 44 }];
    record[0] = 1;
    record[1] = u8::from(volume.is_some());
    record[2..18].copy_from_slice(ns.as_bytes());
    record[18..34].copy_from_slice(pool.as_bytes());
    record[34..42].copy_from_slice(&(if offline { 2u64 } else { 1u64 }).to_be_bytes());
    record[42] = u8::from(!offline);
    record[43] = u8::from(offline);
    let mut index = b"tape-rs/catalog/22/e/".to_vec();
    index.extend_from_slice(ns.as_bytes());
    index.push(2);
    index.extend_from_slice(pool.as_bytes());
    index.extend_from_slice(&suffix);
    let mut identity = vec![serial.len() as u8];
    identity.extend_from_slice(serial.as_bytes());
    identity.push(barcode.len() as u8);
    identity.extend_from_slice(barcode.as_bytes());
    let tr = db.create_trx().unwrap();
    if let Some(volume) = volume {
        record[44..60].copy_from_slice(volume.as_bytes());
        let mut placement_key = b"tape-rs/catalog/22/b/".to_vec();
        placement_key.extend_from_slice(volume.as_bytes());
        let mut placement = vec![1];
        placement.extend_from_slice(ns.as_bytes());
        placement.extend_from_slice(pool.as_bytes());
        placement.extend_from_slice(&identity);
        tr.set(&placement_key, &placement);
        let mut reverse = b"tape-rs/catalog/22/c/".to_vec();
        reverse.extend_from_slice(&suffix);
        tr.set(&reverse, volume.as_bytes());
    }
    tr.set(&primary, &record);
    tr.set(&index, &identity);
    tr.commit().await.unwrap();
}
#[test]
#[ignore = "显式隔离FDB_TEST_CLUSTER；仅合成管理事实和loopback HTTP，不访问设备"]
fn native_library_and_tape_queries_use_fdb_facts_without_legacy_projection() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_queries(&cluster));
}
async fn run_queries(cluster: &str) {
    let catalog = Arc::new(ManagedCatalog::open(Some(cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("QUERY-{ns}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let proof =
        crate::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
    let route = catalog.initialize_native_library(ns, &proof).await.unwrap();
    let ctx = Arc::new(Context {
        catalog: catalog.clone(),
        routes: BTreeMap::from([(serial.clone(), route)]),
        owner: None,
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
        admin_requests: Arc::new(Semaphore::new(32)),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, metadata_router(ctx)).await.unwrap();
    });
    let (code, value) = request(&address, "/admin/libraries", None).await;
    assert_eq!(
        code, 200,
        "native library list must use real FDB route: {value}"
    );
    assert_eq!(value["items"][0]["library_serial"], serial);
    assert_eq!(value["items"][0]["namespace"], ns.to_string());
    assert!(value["items"][0]["inventory"].is_null());
    let prefix = format!("/libraries/{serial}/admin");
    let (code, value) = request(
        &address,
        &format!("{prefix}/library"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 200);
    assert!(value["library"]["inventory"].is_null());
    assert!(value["library"]["active_node"].is_null());
    let (code, value) = request(&address, &format!("{prefix}/tapes"), Some(&ns.to_string())).await;
    assert_eq!(code, 200);
    assert_eq!(value["items"], json!([]));
    let epoch = route.initial_owner_epoch;
    let first_pool = crate::managed_catalog::PoolRecord {
        id: Uuid::new_v4(),
        library_serial: serial.clone(),
        name: "archive alpha".into(),
        file_limit: 200000,
        created_at_millis: 1_793_405_000_123,
        revision: 1,
        policy: Default::default(),
    };
    let second_pool = crate::managed_catalog::PoolRecord {
        id: Uuid::new_v4(),
        name: "other".into(),
        ..first_pool.clone()
    };
    for pool in [&first_pool, &second_pool] {
        catalog
            .create_active_pool(&serial, route, epoch, pool)
            .await
            .unwrap();
    }
    // Synthetic records model future native assignments. The staging-only
    // assign API must continue rejecting Active namespaces; do not relax it.
    let db = foundationdb::Database::new(Some(cluster)).unwrap();
    for (barcode, pool, offline) in [
        ("AOFFL8", first_pool.id, true),
        ("T003L8", first_pool.id, false),
        ("T100L8", second_pool.id, false),
        ("ZLASTL8", second_pool.id, false),
    ] {
        seed_assignment(&db, ns, pool, &serial, barcode, offline).await;
    }
    let observed_at = crate::ltfs::label::ltfs_time_now();
    let physical=(0..105).rev().map(|i|json!({"barcode":format!("T{i:03}L8"),"address":1001+i,"element_type":2,"medium_type":1,"source_address":null,"home_slot":i+1})).collect::<Vec<_>>();
    let observation = json!({"library_serial":serial,"owner":2,"round":epoch,"observed_at":observed_at,"volumes":physical,"drives":[{"serial":"SIMDRV0000","address":1,"barcode":null}]});
    catalog
        .record_active_library_inventory(&serial, route, epoch, &observation)
        .await
        .unwrap();
    // A valid intermediate pair can have different sample times.
    catalog
        .record_library_monitor(&serial, route, epoch, "2026-11-01T00:00:00Z", None)
        .await
        .unwrap();
    let (code, view) = request(
        &address,
        &format!("{prefix}/library"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(view["library"]["num_of_volumes"], 105);
    assert!(
        view["library"]["inventory_owner_current"]
            .as_bool()
            .unwrap()
    );
    assert_ne!(
        view["library"]["inventory"]["observed_at"],
        view["library"]["monitor"]["last_attempt"]
    );
    let mut after = None;
    let mut items = Vec::new();
    let mut first_cursor = None;
    loop {
        let path = format!(
            "{prefix}/tapes?limit=7{}",
            after
                .as_ref()
                .map(|v| format!("&after={v}"))
                .unwrap_or_default()
        );
        let (code, page) = request(&address, &path, Some(&ns.to_string())).await;
        assert_eq!(code, 200, "{page}");
        assert!(page["items"].as_array().unwrap().len() <= 7);
        items.extend(page["items"].as_array().unwrap().clone());
        let next = page["next"].as_str().map(str::to_owned);
        if first_cursor.is_none() {
            first_cursor = next.clone();
        }
        assert!(next.is_none() || next != after);
        after = next;
        if after.is_none() {
            break;
        }
    }
    let expected = std::iter::once("AOFFL8".to_owned())
        .chain((0..105).map(|i| format!("T{i:03}L8")))
        .chain(std::iter::once("ZLASTL8".to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(
        items
            .iter()
            .map(|v| v["barcode"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(items[0]["presence"], "not_observed");
    assert_eq!(items[0]["assignment"]["online"], false);
    assert_eq!(items[0]["assignment"]["blacklisted"], true);
    assert!(
        items
            .iter()
            .all(|v| v["health"].is_null() && v["capacity"].is_null())
    );
    for selector in [first_pool.id.to_string(), "archive%20alpha".into()] {
        let (code, page) = request(
            &address,
            &format!("{prefix}/tapes?pool={selector}"),
            Some(&ns.to_string()),
        )
        .await;
        assert_eq!(code, 200, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["items"][0]["barcode"], "AOFFL8");
        assert_eq!(page["items"][1]["barcode"], "T003L8");
    }
    for query in [
        "limit=0".to_owned(),
        "limit=101".into(),
        "limit=1&limit=2".into(),
        "unknown=x".into(),
        format!("after={}&pool={}", first_cursor.unwrap(), first_pool.id),
    ] {
        assert_eq!(
            request(
                &address,
                &format!("{prefix}/tapes?{query}"),
                Some(&ns.to_string())
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        request(&address, &format!("{prefix}/tapes"), None).await.0,
        409
    );
    assert_eq!(
        request(
            &address,
            &format!("{prefix}/library"),
            Some(&Uuid::new_v4().to_string())
        )
        .await
        .0,
        409
    );
    assert_eq!(
        request(
            &address,
            &format!("{prefix}/tapes/UNKNOWN"),
            Some(&ns.to_string())
        )
        .await
        .0,
        404
    );
    let (code, value) = request(
        &address,
        &format!("{prefix}/tapes/T003L8"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        value["tape"]["assignment"]["pool"],
        first_pool.id.to_string()
    );
    assert_eq!(
        request(
            &address,
            &format!("{prefix}/tapes/T003L8?pool={}", second_pool.id),
            Some(&ns.to_string())
        )
        .await
        .0,
        404
    );
    let mut inventory_key = b"tape-rs/catalog/22/O/".to_vec();
    inventory_key.extend_from_slice(serial.as_bytes());
    inventory_key.extend_from_slice(b"\0\0/inventory/");
    inventory_key.extend_from_slice(ns.as_bytes());
    let tr = db.create_trx().unwrap();
    let original = tr
        .get(&inventory_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    tr.set(&inventory_key, b"corrupt-cache");
    tr.commit().await.unwrap();
    let (code, error) = request(
        &address,
        &format!("{prefix}/library"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 500);
    assert_eq!(error["retryable"], false);
    let tr = db.create_trx().unwrap();
    assert_eq!(
        tr.get(&inventory_key, false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"corrupt-cache"
    );
    tr.set(&inventory_key, &original);
    tr.commit().await.unwrap();
    // The public HTTP client must preserve the chosen namespace and scope.
    let (client_address, client_serial, client_ns, client_pool) = (
        address.clone(),
        serial.clone(),
        ns.to_string(),
        first_pool.id.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        let client = crate::client::fdb::FdbClient::new([client_address]).unwrap();
        assert_eq!(
            client.libraries(None).unwrap()["items"][0]["library_serial"],
            client_serial
        );
        assert_eq!(
            client.library(&client_serial, &client_ns).unwrap()["library"]["num_of_volumes"],
            105
        );
        let first = client
            .tape_page(&client_serial, &client_ns, None, None)
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 100);
        let second = client
            .tape_page(&client_serial, &client_ns, None, first["next"].as_str())
            .unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 7);
        assert!(second["next"].is_null());
        assert_eq!(
            client
                .tape_page(&client_serial, &client_ns, Some(&client_pool), None)
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            client
                .tape(&client_serial, &client_ns, Some(&client_pool), "AOFFL8")
                .unwrap()["tape"]["assignment"]["online"],
            false
        );
        assert!(
            client
                .library(&client_serial, &Uuid::new_v4().to_string())
                .is_err()
        );
    })
    .await
    .unwrap();
    let spool = std::env::temp_dir().join(format!("tapers-query-http-{ns}"));
    let files = crate::daemon::files::FileService::new(spool.clone()).unwrap();
    let status = Arc::new(std::sync::Mutex::new(crate::daemon::node::NodeStatus {
        id: 2,
        executor: Some(crate::daemon::state::Executor {
            node: 2,
            round: epoch,
            term: 7,
            fenced: true,
        }),
        executor_ready: true,
        catalog_frozen_index: Some(route.frozen_index),
        ..Default::default()
    }));
    let (exec, execution_requests) = std::sync::mpsc::channel();
    let (node, node_requests) = std::sync::mpsc::channel();
    let http = Arc::new(HttpContext {
        files: files.clone(),
        status: status.clone(),
        exec: std::sync::Mutex::new(exec),
        node: std::sync::Mutex::new(node),
        client_addrs: HashMap::new(),
        wait_timeout: std::time::Duration::from_secs(1),
    });
    let owner_ctx = Arc::new(Context {
        catalog: catalog.clone(),
        routes: BTreeMap::from([(serial.clone(), route)]),
        owner: Some(OwnerRuntime {
            libraries: BTreeMap::from([(serial.clone(), http)]),
            peers: HashMap::from([
                (1, "a:7411".into()),
                (2, "b:7411".into()),
                (3, "c:7411".into()),
            ]),
            requests: Arc::new(Semaphore::new(1)),
        }),
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
        admin_requests: Arc::new(Semaphore::new(32)),
    });
    let view = queries::library_view(&owner_ctx, &serial, route, true)
        .await
        .unwrap();
    assert_eq!(view["active_node"], 2);
    assert_eq!(view["nodes"].as_array().unwrap().len(), 3);
    assert!(view.to_string().find("nodegroup").is_none());
    assert!(
        view["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("group").is_none())
    );
    status.lock().unwrap().catalog_pending = true;
    assert!(
        queries::library_view(&owner_ctx, &serial, route, true)
            .await
            .unwrap()["active_node"]
            .is_null()
    );
    status.lock().unwrap().catalog_pending = false;
    let mut pool_key = b"tape-rs/catalog/22/T/".to_vec();
    pool_key.extend_from_slice(ns.as_bytes());
    pool_key.extend_from_slice(first_pool.id.as_bytes());
    let tr = db.create_trx().unwrap();
    let saved_pool = tr.get(&pool_key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&pool_key);
    tr.commit().await.unwrap();
    for path in [format!("{prefix}/tapes"), format!("{prefix}/tapes/AOFFL8")] {
        let (code, error) = request(&address, &path, Some(&ns.to_string())).await;
        assert_eq!(
            code, 500,
            "stored assignment references a missing pool: {error}"
        );
        assert_eq!(error["retryable"], false);
    }
    let tr = db.create_trx().unwrap();
    assert!(tr.get(&pool_key, false).await.unwrap().is_none());
    tr.set(&pool_key, &saved_pool);
    tr.commit().await.unwrap();
    let mut assignment_key = b"tape-rs/catalog/22/d/".to_vec();
    assignment_key.extend_from_slice(serial.as_bytes());
    assignment_key.extend_from_slice(b"\0\0AOFFL8\0\0");
    let tr = db.create_trx().unwrap();
    let saved_assignment = tr
        .get(&assignment_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    let mut wrong_assignment = saved_assignment.clone();
    wrong_assignment[2..18].copy_from_slice(Uuid::new_v4().as_bytes());
    tr.set(&assignment_key, &wrong_assignment);
    tr.commit().await.unwrap();
    assert_eq!(
        request(
            &address,
            &format!("{prefix}/tapes/AOFFL8"),
            Some(&ns.to_string())
        )
        .await
        .0,
        500
    );
    let tr = db.create_trx().unwrap();
    assert_eq!(
        tr.get(&assignment_key, false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        wrong_assignment
    );
    tr.set(&assignment_key, &saved_assignment);
    tr.commit().await.unwrap();
    catalog
        .claim_active_owner_epoch(&serial, epoch + 1)
        .await
        .unwrap();
    let (code, value) = request(
        &address,
        &format!("{prefix}/library"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(value["library"]["inventory_owner_current"], false);
    assert_eq!(value["library"]["inventory"]["round"], epoch);
    assert!(
        queries::library_view(&owner_ctx, &serial, route, true)
            .await
            .unwrap()["active_node"]
            .is_null()
    );
    assert!(execution_requests.try_recv().is_err());
    assert!(node_requests.try_recv().is_err());
    drop(owner_ctx);
    drop(files);
    std::fs::remove_dir_all(spool).unwrap();
    let mut head_key = b"tape-rs/catalog/22/~/".to_vec();
    head_key.extend_from_slice(ns.as_bytes());
    head_key.extend_from_slice(serial.as_bytes());
    head_key.extend_from_slice(b"\0\0h");
    let tr = db.create_trx().unwrap();
    let before_head = tr.get(&head_key, false).await.unwrap().map(|v| v.to_vec());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let fail_root = std::env::temp_dir().join(format!("tapers-query-unavailable-{ns}"));
    std::fs::create_dir(&fail_root).unwrap();
    let fail_path = fail_root.join("unavailable.cluster");
    std::fs::write(
        &fail_path,
        format!(
            "query:{}@{}",
            Uuid::new_v4().simple(),
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    let unavailable = Arc::new(Context {
        catalog: Arc::new(ManagedCatalog::open(Some(fail_path.to_str().unwrap())).unwrap()),
        routes: BTreeMap::from([(serial.clone(), route)]),
        owner: None,
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
        admin_requests: Arc::new(Semaphore::new(32)),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failed_address = listener.local_addr().unwrap().to_string();
    let failed_server = tokio::spawn(async move {
        axum::serve(listener, metadata_router(unavailable))
            .await
            .unwrap();
    });
    let (code, error) = request(
        &failed_address,
        &format!("{prefix}/library"),
        Some(&ns.to_string()),
    )
    .await;
    assert_eq!(code, 503);
    assert_eq!(error["retryable"], true);
    failed_server.abort();
    std::fs::remove_dir_all(fail_root).unwrap();
    let tr = db.create_trx().unwrap();
    assert_eq!(
        tr.get(&head_key, false).await.unwrap().map(|v| v.to_vec()),
        before_head,
        "管理查询不能生成事件或修改确认进度"
    );
    server.abort();
}
