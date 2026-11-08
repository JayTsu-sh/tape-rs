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
    (status, serde_json::from_slice(&bytes).unwrap())
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
    let cursor = pools::pool_cursor(&serial, route, b"underlying-key");
    assert_eq!(
        pools::decode_pool_cursor(&serial, route, &cursor).unwrap(),
        b"underlying-key"
    );
    assert!(pools::decode_pool_cursor("OTHER", route, &cursor).is_err());
    let mut other = route;
    other.route.namespace = Uuid::new_v4();
    assert!(pools::decode_pool_cursor(&serial, other, &cursor).is_err());
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
    assert_eq!(code, 200);
    assert_eq!(list["items"], json!([]));
    assert_eq!(list["namespace"], namespace);
    assert!(list["next"].is_null());
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

    let assignment = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-assign","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"barcode":"TEST01L8","confirm_format":true,"force_format":false}});
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
    let movement = json!({"version":1,"library":serial,"namespace":namespace,"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":"ieslot"}});
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
        request(&b, "GET", &format!("{root}/pools"), Some(&namespace), b"")
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
    let result = request(
        &b,
        "POST",
        &format!("{root}/pools"),
        Some(&namespace),
        body.as_bytes(),
    )
    .await;
    assert_eq!(result.0, 201, "{result:?}");
    assert_eq!(
        request(
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
        request(
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
        request(
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
        request(&a, "GET", &poolpath, Some(&namespace), b"").await.1["pool"]["name"],
        "归档"
    );
    assert_eq!(
        request(
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
        request(
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
        json!({"kind":"pool","id":id.to_string(),"revision":1,"patch":{"media_restriction":"L8"}}),
    );
    // Reject duplicate fields, unsupported kinds, wrong resource and scopes before enqueue.
    let duplicate = intent
        .to_string()
        .replace("\"revision\":1", "\"revision\":1,\"revision\":2");
    assert_eq!(
        request(
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
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"name":"wrong"}}),
        ),
    ] {
        assert_eq!(
            request(
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
        request(
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
    let queued = request(
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
    let broken = request(&a, "GET", &poolpath, Some(&namespace), b"").await;
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
        request(
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
    let replay = request(
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
        request(&a, "GET", &poolpath, Some(&namespace), b"").await.1["pool"]["revision"],
        2
    );
    let stale =
        make(json!({"kind":"pool","id":id.to_string(),"revision":1,"patch":{"file_limit":321}}));
    assert_eq!(
        request(
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
    let deletion = make(json!({"kind":"pool-delete","id":id.to_string(),"revision":2}));
    assert_eq!(
        request(
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
        request(&a, "GET", &poolpath, Some(&namespace), b"").await.0,
        200
    );
    // A distinct delete is applied by the actual background worker, without any SCSI receiver.
    let deletion = make(json!({"kind":"pool-delete","id":id.to_string(),"revision":2}));
    assert_eq!(
        request(
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
            if request(&a, "GET", &poolpath, Some(&namespace), b"").await.0 == 404 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Retired original create cannot resurrect the pool.
    assert_eq!(
        request(
            &b,
            "POST",
            &format!("{root}/pools"),
            Some(&namespace),
            body.as_bytes()
        )
        .await
        .0,
        409
    );
    ta.abort();
    tb.abort();
    drop(owner);
    drop(readonly);
    drop(http);
    tokio::time::sleep(Duration::from_millis(350)).await;
    std::fs::remove_dir_all(spool).unwrap();
}
