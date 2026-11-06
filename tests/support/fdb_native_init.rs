//! Three HTTP nodes, the real CLI and a new FDB authority, without a SQLite export.
use super::*;
use std::collections::BTreeMap;
use tape_rs::daemon::{gateway, http::HttpContext, s3, state::Executor};

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；三节点 HTTP、原生 CLI 和模拟空带库，不访问真实设备"]
fn native_cli_initializes_without_exports_and_empty_owner_serves() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let namespace = Uuid::new_v4();
    let serial = format!("SIM-NATIVE-{namespace}");
    let root = std::env::temp_dir().join(format!("tape-rs-native-{namespace}"));
    fs::create_dir(&root).unwrap();
    let config = LibrariesConfig::parse(&json!({"default_library":serial,"libraries":[{
        "changer_serial":serial,"drive_serials":["SIMDRV0000","SIMDRV0001"]
    }]}))
    .unwrap();
    let listeners = (0..3)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect::<Vec<_>>();
    let members = listeners
        .iter()
        .enumerate()
        .map(|(i, listener)| ((i + 1) as u64, listener.local_addr().unwrap().to_string()))
        .collect::<HashMap<_, _>>();
    let mut contexts = Vec::new();
    let mut servers = Vec::new();
    for (i, listener) in listeners.into_iter().enumerate() {
        let ctx = Arc::new(HttpContext {
            files: FileService::new(root.join(format!("node-{i}"))).unwrap(),
            status: Arc::new(Mutex::new(NodeStatus {
                id: (i + 1) as u64,
                role: if i == 1 { "Leader" } else { "Follower" }.into(),
                term: 7,
                leader: 2,
                catalog_quiesce_index: Some(89),
                catalog_frozen_index: Some(90),
                applied_index: 92,
                ..Default::default()
            })),
            exec: Mutex::new(channel().0),
            node: Mutex::new(channel().0),
            client_addrs: members.clone(),
            wait_timeout: Duration::from_secs(1),
        });
        let router = gateway::router(
            config.clone(),
            BTreeMap::from([(serial.clone(), ctx.clone())]),
            ctx.clone(),
            members.clone(),
        );
        servers.push(ReadHttpServer::start_on(listener, router));
        contexts.push(ctx);
    }
    let invoke = |namespace: Uuid| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"));
        for id in 1..=3 {
            command.arg("-e").arg(&members[&id]);
        }
        command.args([
            "--library",
            &serial,
            "fdb",
            "init",
            "--cluster-file",
            &cluster,
            "--namespace",
            &namespace.to_string(),
        ]);
        command.output().unwrap()
    };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    // No redirect can turn three direct reads into three copies of the leader.
    contexts[0].status.lock().unwrap().catalog_frozen_index = None;
    let failed = invoke(namespace);
    assert!(!failed.status.success());
    assert_eq!(
        rt.block_on(catalog.active_library_route(&serial)).unwrap(),
        None
    );
    contexts[0].status.lock().unwrap().catalog_frozen_index = Some(90);
    let result = invoke(namespace);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["namespace"], namespace.to_string());
    assert_eq!(result["initial_owner_epoch"], 91);
    let route = rt
        .block_on(catalog.active_library_route(&serial))
        .unwrap()
        .unwrap();
    assert!(
        rt.block_on(catalog.list_active_pools_for_route(&serial, route, None, 10))
            .unwrap()
            .items
            .is_empty()
    );
    // A successful retry needs no frozen HTTP nodes and must preserve a newer owner.
    drop(servers);
    let library = SimLibrary::new(2, 8, 1);
    library.enable_changer_pr();
    let f = Fixture {
        root: root.clone(),
        library,
        catalog: catalog.clone(),
        serial: serial.clone(),
        route,
        pools: [Uuid::new_v4(), Uuid::new_v4()],
        pool_roots: [Uuid::nil(); 2],
    };
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let retry = invoke(namespace);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        rt.block_on(catalog.active_library_owner(&serial)).unwrap(),
        Some((route, 100))
    );
    assert!(!invoke(Uuid::new_v4()).status.success());
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x04 | 0x19))
    );
    {
        let mut status = owner.status.lock().unwrap();
        status.id = 71;
        status.executor = Some(Executor {
            node: 71,
            round: 100,
            term: 7,
            fenced: true,
        });
        status.executor_ready = true;
    }
    let ctx = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: owner.status.clone(),
        exec: Mutex::new(owner.tx.clone()),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(1),
    });
    let routes = BTreeMap::from([(serial.clone(), route)]);
    let http = ReadHttpServer::start(tape_rs::daemon::fdb_http::writer_router(
        catalog.clone(),
        routes.clone(),
        BTreeMap::from([(serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    assert_eq!(
        http.request("GET", &format!("/readyz?library={serial}"), None)
            .0,
        200
    );
    let s3 = ReadHttpServer::start(s3::fdb_router(
        catalog,
        routes,
        BTreeMap::from([(serial, ctx.clone())]),
        ctx,
        HashMap::new(),
        false,
    ));
    let listed = s3.request("GET", "/", None);
    assert_eq!(listed.0, 200);
    assert!(!String::from_utf8_lossy(&listed.2).contains("<Bucket>"));
    drop(http);
    drop(s3);
    drop(owner);
    drop(contexts);
    fs::remove_dir_all(root).unwrap();
}
