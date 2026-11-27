//! Opt-in private-FDB/loopback tests, never real devices.
use super::*;
use crate::daemon::{
    files::FileService,
    node::NodeStatus,
    permission::{Permission, Phase},
    state::Executor,
};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};
use uuid::Uuid;

async fn serve(ctx: Arc<Context>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, metadata_router(ctx)).await.unwrap();
        }),
    )
}
async fn request(
    address: &str,
    method: &str,
    path: &str,
    ns: Option<&str>,
    body: &[u8],
) -> (u16, Value) {
    request_scoped(address, method, path, ns, None, body).await
}

async fn request_scoped(
    address: &str,
    method: &str,
    path: &str,
    ns: Option<&str>,
    logical: Option<Uuid>,
    body: &[u8],
) -> (u16, Value) {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", address);
    if let Some(ns) = ns {
        req = req.header("x-tapers-catalog-namespace", ns);
    }
    if let Some(id) = logical {
        req = req.header("x-tapers-logical-library", id.to_string());
    }
    let response = sender
        .send_request(
            req.body(http_body_util::Full::new(bytes::Bytes::copy_from_slice(
                body,
            )))
            .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    connection.abort();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；仅回环HTTP，不访问设备"]
fn native_pool_discovery_uses_physical_scope_before_logical_activation() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
        let ns = Uuid::new_v4();
        let serial = format!("PREACT-{ns}");
        let challenge = Uuid::new_v4();
        let replies = (1..=3).map(|id| json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let proof = crate::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
        let route = catalog.initialize_native_library(ns, &proof).await.unwrap();
        let ctx = Arc::new(Context {
            catalog: catalog.clone(), routes: BTreeMap::from([(serial.clone(), route)]), owner: None,
            upload_budget: None, notify_streams: Arc::new(Semaphore::new(1)),
            notify_requests: Arc::new(Semaphore::new(1)), admin_requests: Arc::new(Semaphore::new(32)),
        });
        let (address, server) = serve(ctx).await;
        let path = format!("/libraries/{serial}/admin/pools");
        let (status, page) = request(&address, "GET", &path, Some(&ns.to_string()), b"").await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["items"], json!([]));
        assert_eq!(request_scoped(&address, "GET", &path, Some(&ns.to_string()), Some(Uuid::new_v4()), b"").await.0, 400);
        let preview = format!("/libraries/{serial}/admin/pool-create/preview");
        let body = |tapes: &[&str]| json!({"version":1,"name":"archive","file_limit":100,"tapes":tapes}).to_string();
        let run_preview = async |tapes: &[&str]| request(&address, "POST", &preview, Some(&ns.to_string()), body(tapes).as_bytes()).await;
        assert_eq!(run_preview(&["NEW001L8"]).await.0, 503);
        let owner = route.initial_owner_epoch;
        let mut observation = json!({"library_serial":serial,"owner":2,"round":owner,
            "observed_at":(chrono::Utc::now()-chrono::TimeDelta::minutes(3)).to_rfc3339(),
            "volumes":[
                {"barcode":"NEW001L8","address":1001,"element_type":2,"medium_type":1,"source_address":null,"home_slot":1},
                {"barcode":"CLN001L1","address":1002,"element_type":2,"medium_type":2,"source_address":null,"home_slot":2},
                {"barcode":"IE0001L8","address":101,"element_type":3,"medium_type":1,"source_address":null,"home_slot":null}],
            "drives":[{"serial":"SIMDRV0000","address":1,"barcode":null}],
            "num_of_slots":3,"num_of_ieslots":1,"slot_layout":{"storage_start":1001,"ie_start":101},
            "scsi_vendor_id":"SIM","scsi_product_id":"SIM-CHANGER","scsi_firmware_revision":"0001"});
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        assert_eq!(run_preview(&["NEW001L8"]).await.0, 503);
        observation["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        assert_eq!(run_preview(&["NEW001L8", "NEW001L8"]).await.0, 400);
        let (status, result) = run_preview(&["NEW001L8", "CLN001L1", "MISSING", "IE0001L8"]).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["status"], "blocked");
        assert_eq!(result["tapes"][0]["status"], "eligible");
        assert_eq!(result["tapes"][1]["status"], "cleaning");
        assert_eq!(result["tapes"][2]["status"], "not_observed");
        assert_eq!(result["tapes"][3]["status"], "not_in_storage");
        let (status, result) = run_preview(&["NEW001L8"]).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["status"], "eligible");
        let sdk_address = address.clone();
        let sdk_serial = serial.clone();
        let sdk = tokio::task::spawn_blocking(move || {
            crate::client::fdb::FdbClient::new([sdk_address]).unwrap()
                .preview_pool_create(&sdk_serial, &ns.to_string(), None, "archive", 100, &["NEW001L8".to_owned()])
                .unwrap()
        }).await.unwrap();
        assert_eq!(sdk, result);
        observation["volumes"][0]["address"] = json!(1003);
        observation["volumes"][0]["home_slot"] = json!(3);
        observation["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        let (status, moved) = run_preview(&["NEW001L8"]).await;
        assert_eq!(status, 200, "{moved}");
        assert_eq!(moved["tapes"][0]["home_address"], 1003);

        // An unselected cartridge is already loaded from the apparently empty
        // slot 1001. A second drive cannot use that slot as its home as well.
        let mut conflicting = observation.clone();
        conflicting["volumes"][0]["address"] = json!(1);
        conflicting["volumes"][0]["element_type"] = json!(4);
        conflicting["volumes"][0]["source_address"] = json!(1001);
        conflicting["volumes"][0]["home_slot"] = Value::Null;
        conflicting["volumes"].as_array_mut().unwrap().push(json!({
            "barcode":"OTHER1L8","address":2,"element_type":4,"medium_type":1,"source_address":1001,"home_slot":null
        }));
        conflicting["drives"] = json!([
            {"serial":"SIMDRV0000","address":1,"barcode":"NEW001L8"},
            {"serial":"SIMDRV0001","address":2,"barcode":"OTHER1L8"}
        ]);
        conflicting["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &conflicting).await.unwrap();
        let (status, blocked) = run_preview(&["NEW001L8"]).await;
        assert_eq!(status, 200, "{blocked}");
        assert_eq!(blocked["status"], "blocked");
        assert_eq!(blocked["tapes"][0]["status"], "home_occupied");
        observation["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        let (status, page) = request(&address, "GET", &path, Some(&ns.to_string()), b"").await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["items"], json!([]));
        let pool = crate::managed_catalog::PoolRecord {
            id: Uuid::new_v4(), library_serial: serial.clone(), name: "archive".into(),
            file_limit: 100, created_at_millis: 1, revision: 1, policy: Default::default(),
        };
        catalog.create_admin_pool(&serial, route, owner, None, &pool).await.unwrap();
        observation["volumes"].as_array_mut().unwrap().push(json!({
            "barcode":"SELECTL8","address":1001,"element_type":2,"medium_type":1,"source_address":null,"home_slot":1
        }));
        observation["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        let candidate = |name: &str| crate::managed_catalog::PoolRecord {
            id: Uuid::new_v4(), library_serial: serial.clone(), name: name.into(),
            file_limit: 100, created_at_millis: 1, revision: 1, policy: Default::default(),
        };
        let first = candidate("selected-first");
        let second = candidate("selected-second");
        let tapes = vec!["SELECTL8".into()];
        let rejected = catalog.admit_selected_pool(route, owner, None, &first,
            &["SELECTL8".into(), "CLN001L1".into()], false).await.unwrap();
        assert_eq!(rejected["status"], "blocked");
        let (one, two) = tokio::join!(
            catalog.admit_selected_pool(route, owner, None, &first, &tapes, false),
            catalog.admit_selected_pool(route, owner, None, &second, &tapes, false),
        );
        let (one, two) = (one.unwrap(), two.unwrap());
        let (accepted, rejected, saved) = if one["status"] == "queued" {
            (one, two, first)
        } else {
            (two, one, second)
        };
        assert_eq!(accepted["status"], "queued");
        assert_eq!(accepted["tapes"][0]["source_address"], 1001);
        assert_eq!(rejected["status"], "blocked");
        assert_eq!(rejected["tapes"][0]["status"], "reserved");
        assert_eq!(catalog.admit_selected_pool(route, owner, None, &saved, &tapes, false)
            .await.unwrap(), accepted);
        assert_eq!(request(&address, "GET", &format!("{path}/{}", saved.id), Some(&ns.to_string()), b"").await.0, 404);
        catalog.advance_selected_pool_create(&serial, route, owner).await.unwrap();
        let started = catalog.selected_pool_create_status(&serial, route, None, saved.id).await.unwrap().unwrap();
        let active = &started["active_tape"];
        let operation = active["operation"].as_str().unwrap().parse().unwrap();
        let epoch = active["epoch"].as_u64().unwrap();
        assert_eq!(request(&address, "GET", &format!("{path}/{}", saved.id), Some(&ns.to_string()), b"").await.0, 404);
        for medium in observation["volumes"].as_array_mut().unwrap() {
            if medium["barcode"] == "SELECTL8" {
                medium["address"] = json!(1003);
                medium["home_slot"] = json!(3);
            } else if medium["barcode"] == "NEW001L8" {
                medium["address"] = json!(1001);
                medium["home_slot"] = json!(1);
            }
        }
        observation["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
        catalog.record_active_library_inventory(&serial, route, owner, &observation).await.unwrap();
        assert!(catalog.start_native_media(&serial, route, owner, epoch, operation).await.unwrap().is_none());
        let paused = catalog.selected_pool_create_status(&serial, route, None, saved.id).await.unwrap().unwrap();
        assert_eq!(paused["active_tape"]["status"], "queued");
        assert!(paused["active_tape"]["checkpoint"]["detail"].as_str().unwrap().contains("SELECTL8"));
        assert_eq!(paused["tapes"][0]["source_address"], 1001);
        assert_eq!(paused["tapes"][0]["home_address"], 1001);
        let cancelled = catalog.cancel_management(&serial, route, owner, epoch, operation).await.unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        catalog.advance_selected_pool_create(&serial, route, owner).await.unwrap();
        let terminal = catalog.selected_pool_create_status(&serial, route, None, saved.id).await.unwrap().unwrap();
        assert_eq!(terminal["status"], "cancelled");
        assert_eq!(terminal["active_tape"]["operation"], json!(operation));
        assert_eq!(catalog.admit_selected_pool(route, owner, None, &saved, &tapes, false).await.unwrap(), terminal);
        let replacement = candidate("replacement");
        assert_eq!(catalog.admit_selected_pool(route, owner, None, &replacement, &tapes, false).await.unwrap()["status"], "queued");
        let operation = Uuid::new_v4();
        let import = crate::managed_catalog::ManagementRequest::parse(json!({
            "version":1,"library":serial,"namespace":ns,"epoch":1,"operation":operation,
            "change":{"kind":"media-import","pool":pool.id,"barcode":"NEW001L8","volume":null,"path":"/","merge_tapes":true}
        })).unwrap();
        catalog.admit_management(&serial, route, owner, &import).await.unwrap();
        assert!(catalog.start_native_media(&serial, route, owner, 1, operation).await.unwrap().is_some());
        let (status, result) = run_preview(&["NEW001L8"]).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["status"], "blocked");
        assert_eq!(result["name_available"], false);
        assert_eq!(result["tapes"][0]["status"], "assigned");

        server.abort();
    });
}
#[test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；原生池HTTP/异步队列，不访问设备"]
fn native_pool_metadata_routes_read_current_catalog_without_legacy_projection() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_pool_api(&cluster));
}
async fn run_pool_api(cluster: &str) {
    let catalog = Arc::new(ManagedCatalog::open(Some(cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("POOLAPI-{ns}");
    let namespace = ns.to_string();
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let proof =
        crate::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
    let route = catalog.initialize_native_library(ns, &proof).await.unwrap();
    let epoch = route.initial_owner_epoch;
    let cursor = pools::pool_cursor(&serial, route, None, b"underlying-key");
    assert_eq!(
        pools::decode_pool_cursor(&serial, route, None, &cursor).unwrap(),
        b"underlying-key"
    );
    assert!(pools::decode_pool_cursor("OTHER", route, None, &cursor).is_err());
    let mut other = route;
    other.route.namespace = Uuid::new_v4();
    assert!(pools::decode_pool_cursor(&serial, other, None, &cursor).is_err());
    let readonly = Arc::new(Context {
        catalog: catalog.clone(),
        routes: BTreeMap::from([(serial.clone(), route)]),
        owner: None,
        upload_budget: None,
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
        admin_requests: Arc::new(Semaphore::new(32)),
    });
    let (a, ta) = serve(readonly.clone()).await;
    let root = format!("/libraries/{serial}/admin");
    let (code, list) = request(
        &a,
        "GET",
        &format!("{root}/pools?limit=100"),
        Some(&namespace),
        b"",
    )
    .await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["items"], json!([]));
    assert_eq!(
        request(&a, "GET", &format!("{root}/pools"), None, b"")
            .await
            .0,
        409
    );
    assert_eq!(
        request(
            &a,
            "GET",
            &format!("{root}/pools"),
            Some(&Uuid::new_v4().to_string()),
            b""
        )
        .await
        .0,
        409
    );
    for query in ["limit=0", "limit=101", "limit=1&limit=2", "after=invalid!"] {
        assert_eq!(
            request(
                &a,
                "GET",
                &format!("{root}/pools?{query}"),
                Some(&namespace),
                b""
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        request(&a, "GET", "/libraries/missing/admin/context", None, b"")
            .await
            .0,
        404
    );
    assert_eq!(
        request(
            &a,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            b"{}"
        )
        .await
        .0,
        405
    );
    let spool = std::env::temp_dir().join(format!("tapers-pool-http-{ns}"));
    let files = FileService::new(spool.clone()).unwrap();
    let permit = Arc::new(Permission::default());
    assert!(permit.renew(
        epoch,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120)
    ));
    files.set_permission(permit.clone());
    let status = Arc::new(Mutex::new(NodeStatus {
        id: 2,
        executor: Some(Executor {
            node: 2,
            round: epoch,
            term: 7,
            fenced: true,
        }),
        executor_ready: true,
        catalog_frozen_index: Some(route.frozen_index),
        ..Default::default()
    }));
    let (exec, _) = mpsc::channel();
    let (node, _) = mpsc::channel();
    let http = Arc::new(HttpContext {
        files,
        status: status.clone(),
        exec: Mutex::new(exec),
        node: Mutex::new(node),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(1),
    });
    let owner = Arc::new(Context {
        owner: Some(OwnerRuntime {
            libraries: BTreeMap::from([(serial.clone(), http.clone())]),
            peers: HashMap::new(),
            requests: Arc::new(Semaphore::new(32)),
        }),
        ..(*readonly).clone()
    });
    let (b, tb) = serve(owner.clone()).await;
    let physical_pool = Uuid::new_v4();
    let body = json!({"version":1,"id":physical_pool,"name":"physical-pool","file_limit":100,
        "created_at_millis":1_793_405_000_123_u64,"policy":{"media_restriction":"none","mount_limit":0,
        "low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true}})
        .to_string();
    let pools_path = format!("{root}/pools");
    for expected in [201, 200] {
        let (code, receipt) =
            request(&b, "POST", &pools_path, Some(&namespace), body.as_bytes()).await;
        assert_eq!(code, expected, "{receipt}");
        assert_eq!(receipt["pool"]["id"], json!(physical_pool));
    }
    let (code, page) = request(&a, "GET", &pools_path, Some(&namespace), b"").await;
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["items"][0]["id"], json!(physical_pool));
    let (code, details) = request(
        &a,
        "GET",
        &format!("{pools_path}/physical-pool"),
        Some(&namespace),
        b"",
    )
    .await;
    assert_eq!(code, 200, "{details}");
    assert_eq!(details["pool"]["id"], json!(physical_pool));
    let address = a.clone();
    let library = serial.clone();
    tokio::task::spawn_blocking(move || {
        let client = crate::client::fdb::FdbClient::new([address.as_str()]).unwrap();
        let directory = client.stat(&library, physical_pool, "/").unwrap().unwrap();
        assert_eq!(directory["kind"], "directory");
        let scoped = client.with_logical_library(Uuid::new_v4()).unwrap();
        assert!(scoped.stat(&library, physical_pool, "/").unwrap().is_none());
    })
    .await
    .unwrap();
    let pool_path = format!("{pools_path}/{physical_pool}");
    let mut physical_cursor = None;
    for (method, change) in [
        (
            "PATCH",
            json!({"kind":"pool","id":physical_pool,"revision":1,"patch":{"file_limit":101}}),
        ),
        (
            "DELETE",
            json!({"kind":"pool-delete","id":physical_pool,"revision":2}),
        ),
    ] {
        let operation = Uuid::new_v4();
        let intent = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,
            "operation":operation,"change":change});
        let (code, task) = request(
            &b,
            method,
            &pool_path,
            Some(&namespace),
            intent.to_string().as_bytes(),
        )
        .await;
        assert_eq!(code, 202, "{task}");
        pools::poll(&owner, &serial, &mut physical_cursor)
            .await
            .unwrap();
        let (code, finished) = request(
            &a,
            "GET",
            &format!("{root}/tasks/1/{operation}"),
            Some(&namespace),
            b"",
        )
        .await;
        assert_eq!(code, 200, "{finished}");
        assert_eq!(finished["status"], "succeeded", "{finished}");
    }
    assert_eq!(
        request(&a, "GET", &pool_path, Some(&namespace), b"")
            .await
            .0,
        404
    );
    let (code, receipt) = request(&b, "POST", &pools_path, Some(&namespace), body.as_bytes()).await;
    assert_eq!(code, 200, "{receipt}");
    assert_eq!(receipt["pool"]["revision"], 1);
    assert_eq!(
        request(&a, "GET", &pool_path, Some(&namespace), b"")
            .await
            .0,
        404
    );
    catalog
        .enable_logical_business_fixture(&serial)
        .await
        .unwrap();
    let (code, receipt) = request(&b, "POST", &pools_path, Some(&namespace), body.as_bytes()).await;
    assert_eq!(code, 200, "{receipt}");
    assert_eq!(receipt["pool"]["id"], json!(physical_pool));
    let mut new_pool: Value = serde_json::from_str(&body).unwrap();
    new_pool["id"] = json!(Uuid::new_v4());
    assert_eq!(
        request(
            &b,
            "POST",
            &pools_path,
            Some(&namespace),
            new_pool.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request(&a, "GET", &pools_path, Some(&namespace), b"")
            .await
            .0,
        400
    );
    let (logical, assignment_pool) =
        logical_library_creation(&catalog, &serial, route, &b, &namespace).await;
    let scoped =
        async |address: &str, method: &str, path: &str, namespace: Option<&str>, body: &[u8]| {
            request_scoped(address, method, path, namespace, Some(logical), body).await
        };
    let rescan = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"library-rescan"}});
    let rescan_path = format!("{root}/library/rescan");
    assert_eq!(
        request(
            &a,
            "POST",
            &rescan_path,
            Some(&namespace),
            &serde_json::to_vec(&rescan).unwrap()
        )
        .await
        .0,
        405
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &rescan_path,
            None,
            &serde_json::to_vec(&rescan).unwrap()
        )
        .await
        .0,
        409
    );
    for malformed in [
        rescan.to_string().replace(
            "\"library-rescan\"",
            "\"library-rescan\",\"unexpected\":true",
        ),
        rescan
            .to_string()
            .replace("\"version\":1", "\"version\":1,\"version\":1"),
    ] {
        assert_eq!(
            request(
                &b,
                "POST",
                &rescan_path,
                Some(&namespace),
                malformed.as_bytes()
            )
            .await
            .0,
            400
        );
    }
    let (code, queued_rescan) = request(
        &b,
        "POST",
        &rescan_path,
        Some(&namespace),
        rescan.to_string().as_bytes(),
    )
    .await;
    assert_eq!(code, 202, "{queued_rescan}");
    assert_eq!(queued_rescan["status"], "queued");
    assert_eq!(queued_rescan["request"], rescan);
    assert_eq!(
        request(
            &b,
            "POST",
            &rescan_path,
            Some(&namespace),
            rescan.to_string().as_bytes()
        )
        .await,
        (202, queued_rescan)
    );
    let cancelled = catalog
        .cancel_management(
            &serial,
            route,
            epoch,
            1,
            Uuid::parse_str(rescan["operation"].as_str().unwrap()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        request(
            &b,
            "POST",
            &rescan_path,
            Some(&namespace),
            rescan.to_string().as_bytes()
        )
        .await,
        (202, cancelled)
    );

    let assignment = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-assign","pool":assignment_pool,"logical_library":logical,"volume":Uuid::new_v4().to_string(),"barcode":"TEST01L8","confirm_format":true,"force_format":false}});
    let assign_path = format!("{root}/tapes/TEST01L8/assign");
    assert_eq!(
        request(
            &a,
            "POST",
            &assign_path,
            Some(&namespace),
            assignment.to_string().as_bytes()
        )
        .await
        .0,
        405
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &assign_path,
            None,
            assignment.to_string().as_bytes()
        )
        .await
        .0,
        409
    );
    let (code, admitted) = request(
        &b,
        "POST",
        &assign_path,
        Some(&namespace),
        assignment.to_string().as_bytes(),
    )
    .await;
    assert_eq!(code, 202, "{admitted}");
    assert_eq!(admitted["request"], assignment);
    assert_eq!(
        request(
            &b,
            "POST",
            &assign_path,
            Some(&namespace),
            assignment.to_string().as_bytes()
        )
        .await,
        (202, admitted)
    );
    let mut bad = assignment.clone();
    bad["change"]["confirm_format"] = json!(false);
    assert_eq!(
        request(
            &b,
            "POST",
            &assign_path,
            Some(&namespace),
            bad.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    let mut bad = assignment.clone();
    bad["change"]["unknown"] = json!(true);
    assert_eq!(
        request(
            &b,
            "POST",
            &assign_path,
            Some(&namespace),
            bad.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    let duplicate = assignment.to_string().replace(
        "\"confirm_format\":true",
        "\"confirm_format\":true,\"confirm_format\":true",
    );
    assert_ne!(duplicate, assignment.to_string());
    assert_eq!(
        request(
            &b,
            "POST",
            &assign_path,
            Some(&namespace),
            duplicate.as_bytes()
        )
        .await
        .0,
        400
    );
    let movement = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":"ieslot","logical_library":logical}});
    let move_path = format!("{root}/tapes/TEST01L8/move");
    assert_eq!(
        request(
            &a,
            "POST",
            &move_path,
            Some(&namespace),
            movement.to_string().as_bytes()
        )
        .await
        .0,
        405
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &move_path,
            None,
            movement.to_string().as_bytes()
        )
        .await
        .0,
        409
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &format!("{root}/tapes/OTHER01L8/move"),
            Some(&namespace),
            movement.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    let (code, queued_move) = request(
        &b,
        "POST",
        &move_path,
        Some(&namespace),
        movement.to_string().as_bytes(),
    )
    .await;
    assert_eq!(code, 202, "{queued_move}");
    assert_eq!(queued_move["status"], "queued");
    assert_eq!(queued_move["request"], movement);
    assert!(queued_move["execution"].is_null());
    assert_eq!(
        request(
            &b,
            "POST",
            &move_path,
            Some(&namespace),
            movement.to_string().as_bytes()
        )
        .await,
        (202, queued_move)
    );
    let task_path = format!("{root}/tasks/1/{}", movement["operation"].as_str().unwrap());
    assert_eq!(
        request(&b, "GET", &task_path, Some(&namespace), b"")
            .await
            .1["request"],
        movement
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &format!("{task_path}/cancel"),
            Some(&namespace),
            b""
        )
        .await
        .1["status"],
        "cancelled"
    );
    assert_eq!(
        request(
            &b,
            "POST",
            &move_path,
            Some(&namespace),
            movement.to_string().as_bytes()
        )
        .await
        .1["status"],
        "cancelled"
    );

    // SDK-only recovery facts: readyz yields to mechanical maintenance while
    // management inspection remains usable. No hardware commands run here.
    assert_eq!(request(&b, "GET", "/readyz", None, b"").await.0, 200);
    let mut follower_status = status.lock().unwrap().clone();
    follower_status.id = 3;
    let follower = Arc::new(HttpContext {
        files: http.files.clone(),
        status: Arc::new(Mutex::new(follower_status)),
        exec: Mutex::new(http.exec.lock().unwrap().clone()),
        node: Mutex::new(http.node.lock().unwrap().clone()),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(1),
    });
    let observer = Arc::new(Context {
        owner: Some(OwnerRuntime {
            libraries: BTreeMap::from([(serial.clone(), follower)]),
            peers: HashMap::from([(2, b.clone())]),
            requests: Arc::new(Semaphore::new(2)),
        }),
        ..(*readonly).clone()
    });
    let (observer_address, observer_server) = serve(observer).await;
    assert_eq!(
        request(&observer_address, "GET", "/readyz", None, b"")
            .await
            .0,
        200
    );
    let mut gated = movement.clone();
    gated["operation"] = json!(Uuid::new_v4().to_string());
    let intent = crate::managed_catalog::ManagementRequest::parse(gated.clone()).unwrap();
    catalog
        .admit_management(&serial, route, epoch, &intent)
        .await
        .unwrap();
    let plan = crate::managed_catalog::MovePlan {
        version: 1,
        barcode: "TEST01L8".into(),
        captured_at: crate::ltfs::label::ltfs_time_now(),
        map: crate::changer::element::ElementAddressMap {
            transport_start: 0,
            transport_count: 1,
            storage_start: 1001,
            storage_count: 3,
            ie_start: 101,
            ie_count: 1,
            dt_start: 1,
            dt_count: 1,
        },
        source: crate::managed_catalog::MoveLocation {
            address: 1001,
            kind: crate::managed_catalog::MoveLocationKind::Storage,
            drive_serial: None,
        },
        destination: crate::managed_catalog::MoveLocation {
            address: 101,
            kind: crate::managed_catalog::MoveLocationKind::ImportExport,
            drive_serial: None,
        },
        home_address: Some(1001),
    };
    catalog
        .prepare_media_move(&serial, route, epoch, &intent, &plan)
        .await
        .unwrap();
    assert_eq!(request(&b, "GET", "/readyz", None, b"").await.0, 503);
    let busy = request(&b, "GET", "/readyz", None, b"").await;
    let forwarded_busy = request(&observer_address, "GET", "/readyz", None, b"").await;
    assert_eq!(
        forwarded_busy, busy,
        "entry must retain the owner's maintenance reason"
    );
    assert_eq!(busy.1["reason"], "library_busy");
    assert_eq!(busy.1["ready"], false);
    assert_eq!(busy.1["owner"], 2);
    assert_eq!(busy.1["round"], epoch);
    status.lock().unwrap().executor_ready = false;
    let lost = request(&b, "GET", "/readyz", None, b"").await;
    assert_eq!(lost.0, 503);
    assert_ne!(lost.1["reason"], "library_busy");
    assert_eq!(
        request(&observer_address, "GET", "/readyz", None, b"").await,
        lost
    );
    status.lock().unwrap().executor_ready = true;
    let path = format!("{root}/tasks/1/{}", intent.operation());
    assert_eq!(
        request(&b, "GET", &path, Some(&namespace), b"").await.1["execution"]["phase"],
        "prepared"
    );
    assert_eq!(
        request(&b, "POST", &format!("{path}/cancel"), Some(&namespace), b"")
            .await
            .0,
        400
    );
    assert_eq!(
        scoped(&b, "GET", &format!("{root}/pools"), Some(&namespace), b"")
            .await
            .0,
        200
    );
    let at = crate::ltfs::label::ltfs_time_now();
    let observed = json!({"num_of_slots":8,"num_of_ieslots":1,"scsi_vendor_id":"SIM","scsi_product_id":"SIM-CHANGER","scsi_firmware_revision":"0001","library_serial":serial,"owner":2,"round":epoch,"observed_at":at,"volumes":[{"barcode":"TEST01L8","address":101,"element_type":3,"medium_type":1,"source_address":1001,"home_slot":null}],"drives":[{"serial":"SIMDRV0000","address":1,"barcode":null}]});
    catalog
        .record_active_library_inventory(&serial, route, epoch, &observed)
        .await
        .unwrap();
    catalog
        .record_library_monitor(&serial, route, epoch, &at, None)
        .await
        .unwrap();
    catalog
        .complete_media_move(&serial, route, epoch, &intent, &at)
        .await
        .unwrap();
    assert_eq!(request(&b, "GET", "/readyz", None, b"").await.0, 200);
    assert_eq!(
        request(&observer_address, "GET", "/readyz", None, b"")
            .await
            .0,
        200
    );
    observer_server.abort();

    let id = Uuid::new_v4();
    let body=json!({"version":1,"id":id.to_string(),"name":"归档","file_limit":100,"created_at_millis":1_793_405_000_123u64,"policy":{"media_restriction":"none","mount_limit":0,"low_space_warning_enabled":true,"low_space_warning_threshold":0,"no_space_warning_enabled":true}}).to_string();
    let result = scoped(
        &b,
        "POST",
        &format!("{root}/pools"),
        Some(&namespace),
        body.as_bytes(),
    )
    .await;
    assert_eq!(result.0, 201, "{result:?}");
    assert_eq!(
        scoped(
            &b,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        200
    );
    // A physical entry node forwards a fixed create identity once to the owner.
    let mut remote_status = status.lock().unwrap().clone();
    remote_status.id = 3;
    let remote = Arc::new(HttpContext {
        files: http.files.clone(),
        status: Arc::new(Mutex::new(remote_status)),
        exec: Mutex::new(http.exec.lock().unwrap().clone()),
        node: Mutex::new(http.node.lock().unwrap().clone()),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(1),
    });
    let entry = Arc::new(Context {
        owner: Some(OwnerRuntime {
            libraries: BTreeMap::from([(serial.clone(), remote.clone())]),
            peers: HashMap::from([(2, b.clone())]),
            requests: Arc::new(Semaphore::new(2)),
        }),
        ..(*readonly).clone()
    });
    let (c, tc) = serve(entry).await;
    assert_eq!(
        scoped(
            &c,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        200
    );
    remote.status.lock().unwrap().executor_ready = false;
    assert_eq!(
        scoped(
            &c,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        503
    );
    tc.abort();
    let poolpath = format!("{root}/pools/{id}");
    assert_eq!(
        scoped(&a, "GET", &poolpath, Some(&namespace), b"").await.1["pool"]["name"],
        "归档"
    );
    assert_eq!(
        scoped(
            &a,
            "GET",
            &format!("{root}/pools/%E5%BD%92%E6%A1%A3"),
            Some(&namespace),
            b""
        )
        .await
        .1["pool"]["id"],
        id.to_string()
    );
    let mut conflict: Value = serde_json::from_str(&body).unwrap();
    conflict["id"] = json!(Uuid::new_v4().to_string());
    assert_eq!(
        scoped(
            &b,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            conflict.to_string().as_bytes()
        )
        .await
        .0,
        409
    );
    let make = |change: Value| json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change});
    let intent = make(
        json!({"kind":"pool","logical_library":logical,"id":id.to_string(),"revision":1,"patch":{"media_restriction":"L8"}}),
    );
    // Reject duplicate fields, unsupported kinds, wrong resource and scopes before enqueue.
    let duplicate = intent
        .to_string()
        .replace("\"revision\":1", "\"revision\":1,\"revision\":2");
    assert_eq!(
        scoped(
            &b,
            "PATCH",
            &poolpath,
            Some(&namespace),
            duplicate.as_bytes()
        )
        .await
        .0,
        400
    );
    for bad in [
        make(json!({"kind":"library-rescan"})),
        make(
            json!({"kind":"pool","logical_library":logical,"id":Uuid::new_v4().to_string(),"revision":1,"patch":{"name":"wrong"}}),
        ),
    ] {
        assert_eq!(
            scoped(
                &b,
                "PATCH",
                &poolpath,
                Some(&namespace),
                bad.to_string().as_bytes()
            )
            .await
            .0,
            400
        );
    }
    let mut wrong = intent.clone();
    wrong["library"] = json!("OTHER");
    assert_eq!(
        scoped(
            &b,
            "PATCH",
            &poolpath,
            Some(&namespace),
            wrong.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    let queued = scoped(
        &b,
        "PATCH",
        &poolpath,
        Some(&namespace),
        intent.to_string().as_bytes(),
    )
    .await;
    assert_eq!(queued.0, 202, "{queued:?}");
    assert_eq!(queued.1["status"], "queued");
    let taskpath = format!("{root}/tasks/1/{}", intent["operation"].as_str().unwrap());
    // Corrupt a real private-FDB name index. A permanent error must retain the
    // exact diagnostic and leave the queued task and pool untouched.
    let db = foundationdb::Database::new(Some(cluster)).unwrap();
    let mut namekey = b"tape-rs/catalog/22/X/".to_vec();
    namekey.extend_from_slice(ns.as_bytes());
    namekey.extend_from_slice("归档".as_bytes());
    namekey.extend_from_slice(&[0, 0]);
    let tr = db.create_trx().unwrap();
    let saved = tr.get(&namekey, false).await.unwrap().unwrap().to_vec();
    tr.set(&namekey, b"corrupt-index");
    tr.commit().await.unwrap();
    let broken = scoped(&a, "GET", &poolpath, Some(&namespace), b"").await;
    assert_eq!(broken.0, 500, "{broken:?}");
    assert_eq!(broken.1["retryable"], false);
    assert!(broken.1["detail"].as_str().unwrap().contains("索引"));
    let mut cursor = None;
    assert!(pools::poll(&owner, &serial, &mut cursor).await.is_err());
    assert_eq!(
        request(&a, "GET", &taskpath, Some(&namespace), b"").await.1["status"],
        "queued"
    );
    let tr = db.create_trx().unwrap();
    assert_eq!(
        tr.get(&namekey, false).await.unwrap().unwrap().as_ref(),
        b"corrupt-index"
    );
    tr.set(&namekey, &saved);
    tr.commit().await.unwrap();
    // Revocation leaves original durable intent queued; restoring local readiness does not revive a closed permit.
    permit.close(epoch);
    let mut after = None;
    pools::poll(&owner, &serial, &mut after).await.unwrap();
    assert_eq!(
        request(&a, "GET", &taskpath, Some(&namespace), b"").await.1["status"],
        "queued"
    );
    assert_eq!(
        scoped(
            &b,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        503
    );
    let renewed = Arc::new(Permission::default());
    renewed.renew(
        epoch,
        Phase::Serving,
        Instant::now() + Duration::from_secs(120),
    );
    http.files.set_permission(renewed.clone());
    pools::poll(&owner, &serial, &mut after).await.unwrap();
    assert_eq!(
        request(&a, "GET", &taskpath, Some(&namespace), b"").await.1["status"],
        "succeeded"
    );
    let replay = scoped(
        &b,
        "PATCH",
        &poolpath,
        Some(&namespace),
        intent.to_string().as_bytes(),
    )
    .await;
    assert_eq!(replay.0, 202);
    assert_eq!(replay.1["status"], "succeeded");
    assert_eq!(
        scoped(&a, "GET", &poolpath, Some(&namespace), b"").await.1["pool"]["revision"],
        2
    );
    let stale = make(
        json!({"kind":"pool","logical_library":logical,"id":id.to_string(),"revision":1,"patch":{"file_limit":321}}),
    );
    assert_eq!(
        scoped(
            &b,
            "PATCH",
            &poolpath,
            Some(&namespace),
            stale.to_string().as_bytes()
        )
        .await
        .0,
        202
    );
    pools::poll(&owner, &serial, &mut after).await.unwrap();
    assert_eq!(
        catalog
            .management_task(
                &serial,
                route,
                1,
                Uuid::parse_str(stale["operation"].as_str().unwrap()).unwrap()
            )
            .await
            .unwrap()
            .unwrap()["status"],
        "failed"
    );
    let deletion = make(
        json!({"kind":"pool-delete","logical_library":logical,"id":id.to_string(),"revision":2}),
    );
    assert_eq!(
        scoped(
            &b,
            "DELETE",
            &poolpath,
            Some(&namespace),
            deletion.to_string().as_bytes()
        )
        .await
        .0,
        202
    );
    let cancelpath = format!(
        "{root}/tasks/1/{}/cancel",
        deletion["operation"].as_str().unwrap()
    );
    assert_eq!(
        request(&b, "POST", &cancelpath, Some(&namespace), b"")
            .await
            .1["status"],
        "cancelled"
    );
    pools::poll(&owner, &serial, &mut after).await.unwrap();
    assert_eq!(
        scoped(&a, "GET", &poolpath, Some(&namespace), b"").await.0,
        200
    );
    // A distinct delete is applied by the actual background worker, without any SCSI receiver.
    let deletion = make(
        json!({"kind":"pool-delete","logical_library":logical,"id":id.to_string(),"revision":2}),
    );
    assert_eq!(
        scoped(
            &b,
            "DELETE",
            &poolpath,
            Some(&namespace),
            deletion.to_string().as_bytes()
        )
        .await
        .0,
        202
    );
    pools::start_worker(&owner).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if scoped(&a, "GET", &poolpath, Some(&namespace), b"").await.0 == 404 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Retired original create cannot resurrect the pool.
    assert_eq!(
        scoped(
            &b,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        scoped(&a, "GET", &poolpath, Some(&namespace), b"").await.0,
        404
    );
    ta.abort();
    tb.abort();
    drop(owner);
    drop(readonly);
    drop(http);
    tokio::time::sleep(Duration::from_millis(350)).await;
    std::fs::remove_dir_all(spool).unwrap();
}

async fn logical_library_creation(
    catalog: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    address: &str,
    namespace: &str,
) -> (Uuid, Uuid) {
    let base = format!("/libraries/{serial}/admin/logical-libraries");
    let id = Uuid::new_v4();
    let create = json!({"version":1,"id":id,"name":"archive-a","resources":{"drives":["SIMDRV0000"],
        "storage_slots":[1001],"ie_slots":[101]}});
    let create_body = create.to_string();
    let (code, error) = request(
        address,
        "POST",
        &base,
        Some(namespace),
        create_body.as_bytes(),
    )
    .await;
    assert_eq!(code, 412, "缺少本轮库存时须停止分配: {error}");
    assert_eq!(error["error"], "inventory_unavailable");
    let epoch = route.initial_owner_epoch;
    let observation = json!({"library_serial":serial,"owner":2,"round":epoch,
        "observed_at": crate::ltfs::label::ltfs_time_now(),"volumes":[],
        "drives":[{"serial":"SIMDRV0000","address":1,"barcode":null}],
        "num_of_slots":2,"num_of_ieslots":2,"slot_layout":{"storage_start":1001,"ie_start":101},
        "scsi_vendor_id":"SIM","scsi_product_id":"SIM-CHANGER","scsi_firmware_revision":"0001"});
    let mut incomplete = observation.clone();
    incomplete.as_object_mut().unwrap().remove("slot_layout");
    catalog
        .record_active_library_inventory(serial, route, epoch, &incomplete)
        .await
        .unwrap();
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            create_body.as_bytes()
        )
        .await
        .0,
        412
    );
    let mut occupied = observation.clone();
    occupied["volumes"] = json!([{"barcode":"BUSY01L8","address":101,"element_type":3,
        "medium_type":1,"source_address":null,"home_slot":null}]);
    catalog
        .record_active_library_inventory(serial, route, epoch, &occupied)
        .await
        .unwrap();
    let (status, busy) = request(
        address,
        "POST",
        &base,
        Some(namespace),
        create_body.as_bytes(),
    )
    .await;
    assert_eq!(status, 409, "{busy}");
    catalog
        .record_active_library_inventory(serial, route, epoch, &observation)
        .await
        .unwrap();
    let (code, created) = request(
        address,
        "POST",
        &base,
        Some(namespace),
        create_body.as_bytes(),
    )
    .await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["logical_library"]["id"], id.to_string());
    let (code, listed) = request(address, "GET", &base, Some(namespace), b"").await;
    assert_eq!(code, 200);
    let mut entry = created["logical_library"].clone();
    entry["reconciliation"] = Value::Null;
    assert_eq!(listed["items"], json!([entry]));
    assert_eq!(
        listed["unallocated_resources"]["storage_slots"],
        json!([1002])
    );
    assert_eq!(listed["inventory"]["status"], "current");

    let mut with_tapes = observation.clone();
    with_tapes["volumes"] = json!([
        {"barcode":"LOCAL1L8","address":1001,"element_type":2,"medium_type":1,"source_address":null,"home_slot":1},
        {"barcode":"OTHER1L8","address":1002,"element_type":2,"medium_type":1,"source_address":null,"home_slot":2},
    ]);
    catalog
        .record_active_library_inventory(serial, route, epoch, &with_tapes)
        .await
        .unwrap();
    let preview = format!("/libraries/{serial}/admin/pool-create/preview");
    let proposal = json!({"version":1,"name":"scope-preview","file_limit":100,"tapes":["LOCAL1L8","OTHER1L8"]}).to_string();
    let (code, result) = request_scoped(
        address,
        "POST",
        &preview,
        Some(namespace),
        Some(id),
        proposal.as_bytes(),
    )
    .await;
    assert_eq!(code, 200, "{result}");
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["tapes"][0]["status"], "eligible");
    assert_eq!(result["tapes"][1]["status"], "out_of_scope");
    catalog
        .record_active_library_inventory(serial, route, epoch, &observation)
        .await
        .unwrap();

    // Exercise the same public operation through the blocking Rust SDK.
    let endpoint = address.to_owned();
    let scoped_serial = serial.to_owned();
    let ns = Uuid::parse_str(namespace).unwrap();
    let detail = tokio::task::spawn_blocking(move || {
        crate::client::fdb::FdbClient::new([endpoint])
            .unwrap()
            .logical_library(&scoped_serial, ns, "archive-a")
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(detail["logical_library"], created["logical_library"]);
    assert_eq!(
        detail["inventory"]["observation"]["drives"][0]["serial"],
        "SIMDRV0000"
    );

    let competing = |name| {
        json!({"version":1,"id":Uuid::new_v4(),"name":name,
        "resources":{"drives":[],"storage_slots":[1002],"ie_slots":[]}})
    };
    let left = competing("archive-b").to_string();
    let right = competing("archive-c").to_string();
    let (left, right) = tokio::join!(
        request(address, "POST", &base, Some(namespace), left.as_bytes()),
        request(address, "POST", &base, Some(namespace), right.as_bytes()),
    );
    let mut codes = [left.0, right.0];
    codes.sort_unstable();
    assert_eq!(codes, [201, 409], "{left:?} {right:?}");

    let other = Uuid::parse_str(
        (if left.0 == 201 { &left.1 } else { &right.1 })["logical_library"]["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let pools = format!("/libraries/{serial}/admin/pools");
    let new_pool = json!({"version":1,"id":Uuid::new_v4(),"name":"logical-pool-test",
        "file_limit":100,"created_at_millis":1234,"policy":{
            "media_restriction":"none","mount_limit":0,"low_space_warning_enabled":true,
            "low_space_warning_threshold":0,"no_space_warning_enabled":true}});
    let body = new_pool.to_string();
    let (status, receipt) = request_scoped(
        address,
        "POST",
        &pools,
        Some(namespace),
        Some(id),
        body.as_bytes(),
    )
    .await;
    assert_eq!(status, 201, "{receipt}");
    assert_eq!(receipt["logical_library"], id.to_string());
    let path = format!("{pools}/logical-pool-test");
    assert_eq!(
        request_scoped(address, "GET", &path, Some(namespace), Some(id), b"")
            .await
            .0,
        200
    );
    assert_eq!(
        request_scoped(address, "GET", &path, Some(namespace), Some(other), b"")
            .await
            .0,
        404
    );
    assert_eq!(
        request(address, "GET", &path, Some(namespace), b"").await.0,
        400
    );
    assert_eq!(
        request_scoped(
            address,
            "POST",
            &pools,
            Some(namespace),
            Some(id),
            body.as_bytes()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request_scoped(
            address,
            "POST",
            &pools,
            Some(namespace),
            Some(other),
            body.as_bytes()
        )
        .await
        .0,
        409
    );

    let endpoint = address.to_owned();
    let library = serial.to_owned();
    let second_pool = tokio::task::spawn_blocking(move || {
        use crate::client::fdb::{FdbClient, management::ManagementIntent};
        let client = FdbClient::new([endpoint]).unwrap();
        let request = client
            .prepare_pool_management(
                &library,
                Some(id),
                ManagementIntent::Create {
                    name: "logical-pool-page".into(),
                    file_limit: 100,
                },
            )
            .unwrap();
        let path = std::env::temp_dir().join(format!("scoped-pool-{}.json", Uuid::new_v4()));
        request.save_new(&path).unwrap();
        let result = client.submit_management(&path).unwrap();
        let observed = client.management_request_status(&path, false).unwrap();
        assert_eq!(observed["pool"], result["pool"]);
        std::fs::remove_file(path).unwrap();
        result
    })
    .await
    .unwrap();
    assert_eq!(second_pool["logical_library"], id.to_string());
    let (_, page) = request_scoped(
        address,
        "GET",
        &format!("{pools}?limit=1"),
        Some(namespace),
        Some(id),
        b"",
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let next = page["next"].as_str().unwrap();
    assert_eq!(
        request_scoped(
            address,
            "GET",
            &format!("{pools}?after={next}"),
            Some(namespace),
            Some(other),
            b""
        )
        .await
        .0,
        400
    );

    let accepting = catalog
        .active_management_window(serial, route)
        .await
        .unwrap()
        .accepting;
    let pool_path = format!("{pools}/{}", new_pool["id"].as_str().unwrap());
    let operation = Uuid::new_v4();
    let mut change = json!({"version":1,"library":serial,"namespace":namespace,
        "epoch":accepting,"operation":operation,"change":{"kind":"pool","id":new_pool["id"],
        "logical_library":id,"revision":1,"patch":{"file_limit":200}}});
    assert_eq!(
        request_scoped(
            address,
            "PATCH",
            &pool_path,
            Some(namespace),
            Some(other),
            change.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request_scoped(
            address,
            "PATCH",
            &pool_path,
            Some(namespace),
            Some(id),
            change.to_string().as_bytes()
        )
        .await
        .0,
        202
    );
    let updated = catalog
        .apply_pool_management(serial, route, epoch, accepting, operation)
        .await
        .unwrap();
    assert_eq!(updated["status"], "succeeded", "{updated}");
    assert_eq!(updated["result"]["pool"]["file_limit"], 200);
    let (replayed_status, replayed) = request_scoped(
        address,
        "POST",
        &pools,
        Some(namespace),
        Some(id),
        body.as_bytes(),
    )
    .await;
    assert_eq!(replayed_status, 200);
    assert_eq!(replayed["pool"], receipt["pool"]);

    let operation = Uuid::new_v4();
    change["operation"] = json!(operation);
    change["change"] =
        json!({"kind":"pool-delete","id":new_pool["id"],"logical_library":id,"revision":2});
    assert_eq!(
        request_scoped(
            address,
            "DELETE",
            &pool_path,
            Some(namespace),
            Some(id),
            change.to_string().as_bytes()
        )
        .await
        .0,
        202
    );
    let deleted = catalog
        .apply_pool_management(serial, route, epoch, accepting, operation)
        .await
        .unwrap();
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    assert_eq!(
        request_scoped(address, "GET", &pool_path, Some(namespace), Some(id), b"")
            .await
            .0,
        404
    );

    let mut wrong_type = competing("wrong-type");
    wrong_type["resources"]["storage_slots"] = json!([101]);
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            wrong_type.to_string().as_bytes()
        )
        .await
        .0,
        400
    );
    let mut duplicate = competing("duplicate");
    duplicate["resources"]["storage_slots"] = json!([1002, 1002]);
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            duplicate.to_string().as_bytes()
        )
        .await
        .0,
        400
    );

    let mut stale = observation.clone();
    stale["observed_at"] = json!((chrono::Utc::now() - chrono::TimeDelta::minutes(5)).to_rfc3339());
    catalog
        .record_active_library_inventory(serial, route, epoch, &stale)
        .await
        .unwrap();
    let (code, listed) = request(address, "GET", &base, Some(namespace), b"").await;
    assert_eq!(code, 200);
    assert_eq!(listed["items"].as_array().unwrap().len(), 2);
    assert_eq!(listed["inventory"]["status"], "stale");
    assert!(listed["unallocated_resources"].is_null());
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            competing("stale").to_string().as_bytes()
        )
        .await
        .0,
        412
    );
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            create_body.as_bytes()
        )
        .await,
        (200, created)
    );
    let mut changed = create;
    changed["name"] = json!("changed-original");
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            changed.to_string().as_bytes()
        )
        .await
        .0,
        409
    );
    catalog
        .record_active_library_inventory(serial, route, epoch, &observation)
        .await
        .unwrap();
    let target = format!("{base}/{other}");
    let op = Uuid::new_v4();
    let edit = json!({"version":1,"operation":op,"revision":1,"replacement":{
        "name":"renamed-logical-library","resources":{"drives":[],"storage_slots":[1002],"ie_slots":[102]}}});
    let edit_body = edit.to_string();
    occupied["volumes"][0]["address"] = json!(102);
    occupied["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
    catalog
        .record_active_library_inventory(serial, route, epoch, &occupied)
        .await
        .unwrap();
    let (status, busy) = request(
        address,
        "PATCH",
        &target,
        Some(namespace),
        edit_body.as_bytes(),
    )
    .await;
    assert_eq!(status, 409, "{busy}");
    let mut empty = observation.clone();
    empty["observed_at"] = json!(crate::ltfs::label::ltfs_time_now());
    catalog
        .record_active_library_inventory(serial, route, epoch, &empty)
        .await
        .unwrap();
    let unrelated = Uuid::new_v4();
    let related = Uuid::new_v4();
    let move_path = format!("/libraries/{serial}/admin/tapes/TEST01L8/move");
    for (operation, logical) in [(unrelated, id), (related, other)] {
        let movement = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,
            "operation":operation,"change":{"kind":"media-move","pool":null,"volume":null,
                "revision":null,"barcode":"TEST01L8","destination":"ieslot","logical_library":logical}});
        assert_eq!(
            request(
                address,
                "POST",
                &move_path,
                Some(namespace),
                movement.to_string().as_bytes(),
            )
            .await
            .0,
            202
        );
    }
    assert_eq!(
        request(
            address,
            "PATCH",
            &target,
            Some(namespace),
            edit_body.as_bytes(),
        )
        .await
        .0,
        503
    );
    catalog
        .cancel_management(serial, route, epoch, 1, related)
        .await
        .unwrap();
    let (status, changed) = request(
        address,
        "PATCH",
        &target,
        Some(namespace),
        edit_body.as_bytes(),
    )
    .await;
    assert_eq!(status, 200, "{changed}");
    catalog
        .cancel_management(serial, route, epoch, 1, unrelated)
        .await
        .unwrap();
    assert_eq!(changed["logical_library"]["revision"], 2);
    let status_path = format!("{target}/changes/{op}");
    assert_eq!(
        request(address, "GET", &status_path, Some(namespace), b"").await,
        (200, changed.clone())
    );
    assert_eq!(
        request(
            address,
            "PATCH",
            &target,
            Some(namespace),
            edit_body.as_bytes()
        )
        .await,
        (200, changed)
    );
    let mut wrong_revision = edit;
    wrong_revision["operation"] = json!(Uuid::new_v4());
    assert_eq!(
        request(
            address,
            "PATCH",
            &target,
            Some(namespace),
            wrong_revision.to_string().as_bytes()
        )
        .await
        .0,
        409
    );

    let operation = Uuid::new_v4();
    let delete =
        json!({"version":1,"operation":operation,"revision":2,"replacement":null}).to_string();
    let occupied = json!([{"barcode":"BUSY01L8","address":102,"element_type":3,"medium_type":1,"source_address":null,"home_slot":null}]);
    let mut loaded = observation.clone();
    loaded["volumes"] = occupied;
    catalog
        .record_active_library_inventory(serial, route, epoch, &loaded)
        .await
        .unwrap();
    assert_eq!(
        request(
            address,
            "DELETE",
            &target,
            Some(namespace),
            delete.as_bytes()
        )
        .await
        .0,
        409
    );
    catalog
        .record_active_library_inventory(serial, route, epoch, &observation)
        .await
        .unwrap();
    let (status, deleted) = request(
        address,
        "DELETE",
        &target,
        Some(namespace),
        delete.as_bytes(),
    )
    .await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        request(
            address,
            "GET",
            &format!("{target}/changes/{operation}"),
            Some(namespace),
            b""
        )
        .await,
        (200, deleted)
    );

    let still_has_pool =
        json!({"version":1,"operation":Uuid::new_v4(),"revision":1,"replacement":null});
    assert_eq!(
        request(
            address,
            "DELETE",
            &format!("{base}/{id}"),
            Some(namespace),
            still_has_pool.to_string().as_bytes()
        )
        .await
        .0,
        409
    );
    let reassigned = competing("reassigned");
    let reassigned_id = Uuid::parse_str(reassigned["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        request(
            address,
            "POST",
            &base,
            Some(namespace),
            reassigned.to_string().as_bytes()
        )
        .await
        .0,
        201
    );

    let mut racing_pool = new_pool;
    racing_pool["id"] = json!(Uuid::new_v4());
    racing_pool["name"] = json!("create-during-logical-delete");
    let pool_body = racing_pool.to_string();
    let delete_body =
        json!({"version":1,"operation":Uuid::new_v4(),"revision":1,"replacement":null}).to_string();
    let target = format!("{base}/{reassigned_id}");
    let (creation, deletion) = tokio::join!(
        request_scoped(
            address,
            "POST",
            &pools,
            Some(namespace),
            Some(reassigned_id),
            pool_body.as_bytes()
        ),
        request(
            address,
            "DELETE",
            &target,
            Some(namespace),
            delete_body.as_bytes()
        ),
    );
    assert!(
        matches!((creation.0, deletion.0), (201, 409) | (400, 200)),
        "{creation:?} {deletion:?}"
    );
    (
        id,
        Uuid::parse_str(second_pool["pool"]["id"].as_str().unwrap()).unwrap(),
    )
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；回环 HTTP 和目录，不访问设备"]
async fn replicated_pool_publishes_two_physical_members_together_and_replays() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let mut routes = BTreeMap::new();
    let mut members = Vec::new();
    for index in 0..2 {
        let ns = Uuid::new_v4();
        let serial = format!("REPLICA-{ns}");
        let challenge = Uuid::new_v4();
        let replies = (1..=3)
            .map(|id| {
                json!({
                    "version":1,"library_serial":serial,"challenge":challenge,"node_id":id,
                    "term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},
                    "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
                    "applied_index":92,"executor_absent":true,"catalog_pending":false,
                    "shutting_down":false,"catalog_empty":true,
                })
            })
            .collect::<Vec<_>>();
        let proof =
            crate::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap();
        let route = catalog.initialize_native_library(ns, &proof).await.unwrap();
        members.push(json!({"library_serial":serial,"namespace":ns,
            "pool_id":Uuid::new_v4(),"name":format!("member-{index}")}));
        routes.insert(serial, route);
    }
    let ctx = Arc::new(Context {
        catalog: catalog.clone(),
        routes: routes.clone(),
        owner: Some(OwnerRuntime {
            libraries: BTreeMap::new(),
            peers: HashMap::new(),
            requests: Arc::new(Semaphore::new(1)),
        }),
        upload_budget: None,
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
        admin_requests: Arc::new(Semaphore::new(32)),
    });
    let (address, server) = serve(ctx).await;
    let id = Uuid::new_v4();
    let mut input = json!({"version":1,"id":id,"name":format!("replica-{id}"),"created_at_millis":1,"members":members});
    let base = "/admin/replicated-pools";
    let status = format!("{base}/{id}");
    let (code, result) = request(&address, "POST", base, None, input.to_string().as_bytes()).await;
    assert_eq!(code, 202, "{result}");
    assert_eq!(result["id"], json!(id));
    for member in &members {
        let serial = member["library_serial"].as_str().unwrap();
        let ns = member["namespace"].as_str().unwrap();
        let pool = member["pool_id"].as_str().unwrap();
        let path = format!("/libraries/{serial}/admin/pools/{pool}");
        assert_eq!(request(&address, "GET", &path, Some(ns), b"").await.0, 404);
    }
    let (serial, route) = routes.first_key_value().unwrap();
    catalog
        .advance_selected_pool_create(serial, *route, route.initial_owner_epoch)
        .await
        .unwrap();
    let (code, saved) = request(&address, "GET", &status, None, b"").await;
    assert_eq!(code, 200, "{saved}");
    assert_eq!(saved["status"], "succeeded");
    assert_eq!(saved["failure_domains"].as_array().unwrap().len(), 2);
    assert_eq!(
        request(&address, "POST", base, None, input.to_string().as_bytes()).await,
        (200, saved)
    );
    input["name"] = json!("different");
    assert_eq!(
        request(&address, "POST", base, None, input.to_string().as_bytes())
            .await
            .0,
        409
    );
    server.abort();
}
