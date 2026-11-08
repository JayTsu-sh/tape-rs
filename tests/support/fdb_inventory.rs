use super::*;
use tape_rs::client::fdb::PreparedManagement;

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 HTTP/CLI 与模拟库存，不接触设备"]
fn inventory_queries_join_fdb_assignments_without_device_io() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    println!("inventory fixture {}", f.root.display());
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let libraries = BTreeMap::from([(f.serial.clone(), ctx.clone())]);
    let config=tape_rs::daemon::multilib::LibrariesConfig::parse(&json!({
        "default_library":f.serial,"libraries":[{"changer_serial":f.serial,"drive_serials":["DRIVE0","DRIVE1"]}]
    })).unwrap();
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            routes,
            libraries.clone(),
            HashMap::from([(71, "127.0.0.1:1".into()), (72, "127.0.0.1:2".into())]),
            Default::default(),
        )
        .expect("FDB HTTP worker startup")
        .merge(tape_rs::daemon::gateway::control_router(
            config,
            libraries,
            ctx.clone(),
            HashMap::new(),
        )),
    );
    let client = FdbClient::new([&server.address]).unwrap();
    client
        .create_pool(
            &f.serial,
            f.route.route.namespace,
            f.pools[0],
            "actual-pool",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    client
        .create_pool(
            &f.serial,
            f.route.route.namespace,
            f.pools[1],
            "other-pool",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    let request=client.prepare_control(&f.serial,f.route.route.namespace,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"ASSIGNL8","volume":null,"path":"/","merge_tapes":true})).unwrap();
    let saved = f.root.join("inventory-intake.json");
    request.save_new(&saved).unwrap();
    assert!(matches!(
        client.submit_management_saved(&saved).unwrap()["status"].as_str(),
        Some("queued" | "running")
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = client.management_status(&request).unwrap().unwrap();
        if status["status"] == "running" {
            assert_eq!(status["progress"]["phase"], "source_pending");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let trace = owner.trace.clone();
    drop(owner);
    {
        let mut status = ctx.status.lock().unwrap();
        status.management=json!({"inventory":{"library_serial":f.serial,"owner":71,"round":100,"observed_at":"2026-11-03T12:00:00.000000000Z", "scsi_vendor_id":"SIM","scsi_product_id":"TEST-LIBRARY","scsi_firmware_revision":"0001","num_of_slots":8,"num_of_ieslots":1,
            "volumes":[{"barcode":"UNUSEDL8","element_type":2,"address":1001,"medium_type":1,"source_address":null,"home_slot":1},{"barcode":"CLNU00L1","element_type":3,"address":101,"medium_type":2,"source_address":null,"home_slot":null}],"drives":[{"serial":"DRIVE0","address":1,"barcode":null},{"serial":"DRIVE1","address":2,"barcode":null}]}}).into();
    }
    let mut observation = ctx.status.lock().unwrap().management["inventory"].clone();
    observation["volumes"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|v| v["address"].as_u64());
    rt.block_on(
        f.catalog
            .record_active_library_inventory(&f.serial, f.route, 100, &observation),
    )
    .unwrap();
    let mut stale = observation.clone();
    stale["round"] = json!(99);
    assert!(
        rt.block_on(
            f.catalog
                .record_active_library_inventory(&f.serial, f.route, 99, &stale)
        )
        .is_err()
    );
    assert_eq!(
        rt.block_on(f.catalog.active_library_inventory(&f.serial, f.route))
            .unwrap(),
        Some(observation)
    );
    let page = client
        .tape_inventory_page(&f.serial, Some(f.pools[0]), 1, None)
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["barcode"], "ASSIGNL8");
    assert!(
        client
            .tape_inventory_page(&f.serial, Some(f.pools[1]), 1, None)
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .tape_inventory_entry(&f.serial, Some(f.pools[1]), "ASSIGNL8")
            .is_err()
    );
    assert!(
        client
            .tape_inventory_entry(&f.serial, Some(f.pools[0]), "UNUSEDL8")
            .is_err()
    );
    let cursor = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&(
                1u8,
                &f.serial,
                f.route.route.namespace.to_string(),
                f.pools[1].to_string(),
                "ASSIGNL8",
            ))
            .unwrap(),
        )
    };
    assert!(
        client
            .tape_inventory_page(&f.serial, Some(f.pools[0]), 1, Some(&cursor))
            .is_err()
    );
    let wrong_body = request.value().to_string();
    let headers = format!(
        "Content-Type: application/json\r\nx-tapers-catalog-namespace: {}\r\nContent-Length: {}\r\n",
        f.route.route.namespace,
        wrong_body.len()
    );
    assert_eq!(
        server
            .request_body(
                "POST",
                &format!(
                    "/libraries/{}/admin/pools/{}/tapes/ASSIGNL8/import",
                    f.serial, f.pools[1]
                ),
                Some(&headers),
                wrong_body.as_bytes()
            )
            .0,
        400
    );
    let before = trace.lock().unwrap().len();
    let drives = client
        .operational_inventory(&f.serial, "drives", None)
        .unwrap();
    assert_eq!(drives["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        client
            .operational_inventory(&f.serial, "drives", Some("DRIVE0"))
            .unwrap()["item"]["serial"],
        "DRIVE0"
    );
    let nodes = client
        .operational_inventory(&f.serial, "nodes", None)
        .unwrap();
    assert_eq!(nodes["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        client
            .operational_inventory(&f.serial, "nodes", Some("71"))
            .unwrap()["item"]["id"],
        71
    );
    let mut query = tape_rs::client::Client::new([&server.address]);
    let libraries = query.libraries().unwrap();
    assert_eq!(libraries["items"][0]["model"], "TEST-LIBRARY");
    assert_eq!(libraries["items"][0]["num_of_drives"], 2);
    let shown = client.library_inventory(&f.serial).unwrap();
    assert_eq!(shown["library"]["num_of_slots"], 8);
    let mut names = Vec::new();
    let mut path = "/admin/tapes?limit=1".to_owned();
    loop {
        let wire = server.request("GET", &path, None);
        assert_eq!(
            wire.0,
            200,
            "global inventory {path}: {}",
            String::from_utf8_lossy(&wire.2)
        );
        let page = query.admin_get(&path).unwrap();
        let rows = page["items"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        names.push(rows[0]["barcode"].as_str().unwrap().to_owned());
        let Some(next) = page["next"].as_str() else {
            break;
        };
        path = format!("/admin/tapes?limit=1&after={next}");
    }
    assert_eq!(names, ["ASSIGNL8", "CLNU00L1", "UNUSEDL8"]);
    let assigned = client
        .tape_inventory_entry(&f.serial, None, "ASSIGNL8")
        .unwrap();
    assert_eq!(assigned["tape"]["poolname"], "actual-pool");
    assert_eq!(assigned["tape"]["state"], "importing");
    assert_eq!(assigned["tape"]["task_progress"]["phase"], "source_pending");
    assert!(assigned["tape"]["usable"].is_null());
    let cleaning = client
        .tape_inventory_entry(&f.serial, None, "CLNU00L1")
        .unwrap();
    assert_eq!(cleaning["tape"]["cleaning"], true);
    assert_eq!(cleaning["tape"]["location_kind"], "ieslot");
    let unknown = client
        .tape_inventory_entry(&f.serial, None, "UNUSEDL8")
        .unwrap();
    assert_eq!(unknown["tape"]["state"], "unassigned");
    assert!(
        client
            .tape_inventory_entry(&f.serial, None, "NOTHERE8")
            .is_err()
    );
    assert!(query.admin_get("/admin/tapes?limit=0").is_err());
    for args in [
        vec!["library", "list", "--json"],
        vec!["library", "show", "-a", "num_of_slots", "--jsonpp"],
        vec!["tape", "list", "--json"],
        vec!["tape", "show", "ASSIGNL8", "-a", "poolname", "--json"],
        vec!["drive", "list"],
        vec!["drive", "show", "DRIVE0"],
        vec!["node", "list"],
        vec!["node", "show", "71"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args(["-e", &server.address, "-l", &f.serial])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    }
    assert_eq!(
        trace.lock().unwrap().len(),
        before,
        "list/show 不得访问设备"
    );
    drop(server);
    drop(ctx);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实管理 HTTP/CLI 与模拟器，不操作真实设备"]
fn admin_import_and_async_cli_keep_the_original_task() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    println!("admin import fixture {}", f.root.display());
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
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
    let client = FdbClient::new([&server.address]).unwrap();
    client
        .create_pool(
            &f.serial,
            f.route.route.namespace,
            f.pools[0],
            "target",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    let request=client.prepare_control(&f.serial,f.route.route.namespace,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"ABSENTL8","volume":null,"path":"/","merge_tapes":true})).unwrap();
    let body = request.value().to_string();
    let headers = format!(
        "Content-Type: application/json\r\nx-tapers-catalog-namespace: {}\r\nContent-Length: {}\r\n",
        f.route.route.namespace,
        body.len()
    );
    let route = format!(
        "/libraries/{}/admin/pools/{}/tapes/ABSENTL8/import",
        f.serial, f.pools[0]
    );
    let started = Instant::now();
    let status = server
        .request_body("POST", &route, Some(&headers), body.as_bytes())
        .0;
    assert!(matches!(status, 200 | 202));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "HTTP 受理不得等待介质任务完成"
    );
    assert!(matches!(
        client.management_status(&request).unwrap().unwrap()["status"].as_str(),
        Some("queued" | "running")
    ));
    let id = format!(
        "{}:{}:{}:{}",
        f.route.route.namespace,
        f.serial,
        request.value()["epoch"],
        request.value()["operation"].as_str().unwrap()
    )
    .parse::<tape_rs::management_task_id::ManagementTaskId>()
    .unwrap();
    assert_eq!(
        client.management_task_by_id(&id).unwrap().unwrap()["operation"],
        request.value()["operation"]
    );
    let wrong = format!(
        "{}:{}:1:{}",
        Uuid::new_v4(),
        f.serial,
        request.value()["operation"].as_str().unwrap()
    )
    .parse()
    .unwrap();
    assert!(client.cancel_management_task_by_id(&wrong).is_err());
    client.cancel_management_task_by_id(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if client.management_status(&request).unwrap().unwrap()["status"] == "cancelled" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let saved = f.root.join("async-cli.json");
    let started = Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "tape",
            "import",
            "ABSEN2L8",
            "-l",
            &f.serial,
            "-p",
            "target",
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
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "CLI 默认受理不得等待介质任务完成"
    );
    assert!(matches!(
        value["requests"][0]["task"]["status"].as_str(),
        Some("queued" | "running")
    ));
    let cli_id = value["requests"][0]["task_id"].as_str().unwrap();
    let original = fs::read(&saved).unwrap();
    let prepared = PreparedManagement::load(&saved).unwrap();
    assert_eq!(
        client
            .management_task_by_id(&cli_id.parse().unwrap())
            .unwrap()
            .unwrap()["operation"],
        prepared.value()["operation"]
    );
    client
        .cancel_management_task_by_id(&cli_id.parse().unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if client.management_status(&prepared).unwrap().unwrap()["status"] == "cancelled" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let retry = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "tape",
            "import",
            "--request",
            saved.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!retry.status.success());
    assert_eq!(fs::read(&saved).unwrap(), original);
    assert_eq!(
        client.management_status(&prepared).unwrap().unwrap()["status"],
        "cancelled"
    );
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；CLI/HTTP 离线、实带核验上线和原卷对账，仅访问 SimLibrary"]
fn native_tape_policy_online_and_assigned_import_share_http_cli_and_saved_tasks() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let barcode = "POLICY0L8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    mkltfs(
        &f.library.drive(0),
        &MkltfsOptions {
            volume_id: "POLICY".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    fixture_return_to_storage(&f.library, 0);
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
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
    let entry = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            BTreeMap::from([(f.serial.clone(), f.route)]),
            BTreeMap::from([(f.serial.clone(), context(&owner, 72, 100))]),
            HashMap::from([(71, server.address.clone())]),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([&entry.address]).unwrap();
    client
        .create_pool(
            &f.serial,
            f.route.route.namespace,
            f.pools[0],
            "policy-target",
            200_000,
            1_793_405_000_123,
        )
        .unwrap();
    fn wait(client: &FdbClient, request: &PreparedManagement) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let task = client.management_status(request).unwrap().unwrap();
            if matches!(
                task["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                assert_eq!(task["status"], "succeeded", "{task}");
                return task;
            }
            assert!(Instant::now() < deadline, "{task}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let ns = f.route.route.namespace;
    let pool = f.pools[0].to_string();
    let import=client.prepare_control(&f.serial,ns,json!({"kind":"media-import","pool":pool,"barcode":barcode,"volume":null,"path":"/original","merge_tapes":true})).unwrap();
    let saved = f.root.join("policy-import.json");
    import.save_new(&saved).unwrap();
    client.submit_management_saved(&saved).unwrap();
    wait(&client, &import);
    let before = client
        .management_policy(&f.serial, "tape", barcode)
        .unwrap()["policy"]
        .clone();
    let base = format!("/libraries/{}/admin/pools/{pool}/tapes/{barcode}", f.serial);
    let cli = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args(["-e", &server.address, "-l", &f.serial, "-p", &pool, "tape"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    // Private FDB + Sim only: use the actor's native MAM observation. The
    // poller may refresh its timestamp while HTTP and CLI share counters.
    for _ in 0..40 {
        rt.block_on(
            f.catalog
                .advance_active_pool_statistics(&f.serial, f.route, 100),
        )
        .unwrap();
        if rt
            .block_on(
                f.catalog
                    .active_tape_statistics(&f.serial, f.route, barcode),
            )
            .unwrap()["quality"]
            == "estimated"
        {
            break;
        }
    }
    let shown = client
        .tape_inventory_entry(&f.serial, Some(f.pools[0]), barcode)
        .unwrap();
    let percent = shown["tape"]["reclaimable"]
        .as_f64()
        .expect("estimated capacity");
    assert!((0.0..=100.0).contains(&percent));
    assert_eq!(shown["tape"]["reclaimable_unit"], "percent");
    assert_eq!(shown["tape"]["live_bytes"], 0);
    assert_eq!(shown["tape"]["statistics"]["quality"], "estimated");
    let listed = client
        .tape_inventory_page(&f.serial, Some(f.pools[0]), 10, None)
        .unwrap();
    assert_eq!(
        listed["items"][0]["reclaimable"],
        shown["tape"]["reclaimable"]
    );
    assert_eq!(
        listed["items"][0]["statistics"]["revision"],
        shown["tape"]["statistics"]["revision"]
    );
    let cli_shown = cli(&["show", barcode, "--json"]);
    assert_eq!(cli_shown["reclaimable"], shown["tape"]["reclaimable"]);
    assert_eq!(
        cli_shown["statistics"]["revision"],
        shown["tape"]["statistics"]["revision"]
    );
    let cli_list = cli(&["list", "--json"]);
    assert_eq!(
        cli_list["items"][0]["reclaimable"],
        shown["tape"]["reclaimable"]
    );
    let start = owner.trace.lock().unwrap().len();
    let offline = f.root.join("policy-offline.json");
    let accepted = cli(&[
        "offline",
        barcode,
        "-o",
        "device maintenance",
        "--request",
        offline.to_str().unwrap(),
    ]);
    assert_eq!(accepted["requests"].as_array().unwrap().len(), 1);
    let request = PreparedManagement::load(&offline).unwrap();
    let done = wait(&client, &request);
    assert_eq!(client.submit_management_saved(&offline).unwrap(), done);
    let policy = client
        .management_policy(&f.serial, "tape", barcode)
        .unwrap()["policy"]
        .clone();
    assert_eq!(policy["online"], false);
    assert_eq!(policy["offline_message"], "device maintenance");
    let shown = client
        .tape_inventory_entry(&f.serial, None, barcode)
        .unwrap();
    assert_eq!(shown["tape"]["offline_message"], "device maintenance");
    assert_eq!(
        shown["tape"]["assignment"]["offline_message"],
        "device maintenance"
    );
    let listed = client
        .tape_inventory_page(&f.serial, Some(f.pools[0]), 100, None)
        .unwrap();
    let row = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["barcode"] == barcode)
        .unwrap();
    assert_eq!(row["offline_message"], "device maintenance");
    assert_eq!(
        cli(&["show", barcode, "--json"])["offline_message"],
        "device maintenance"
    );
    assert_eq!(policy["volume"], before["volume"]);
    assert!(
        !owner.trace.lock().unwrap()[start..]
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0x04 | 0xa5))
    );
    let unsafe_online=json!({"version":1,"library":f.serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"tape","pool":pool,"barcode":barcode,"volume":policy["volume"],"revision":policy["revision"],"patch":{"online":true}}}).to_string();
    let headers = format!(
        "Content-Type: application/json\r\nx-tapers-catalog-namespace: {}\r\nContent-Length: {}\r\n",
        f.route.route.namespace,
        unsafe_online.len()
    );
    assert_eq!(
        server
            .request_body(
                "POST",
                &format!("{base}/set"),
                Some(&headers),
                unsafe_online.as_bytes()
            )
            .0,
        400
    );
    let online = f.root.join("policy-online.json");
    cli(&["online", barcode, "--request", online.to_str().unwrap()]);
    let request = PreparedManagement::load(&online).unwrap();
    let done = wait(&client, &request);
    assert_eq!(request.value()["change"]["kind"], "media-online");
    assert_eq!(
        done["result"]["result"]["validation"]["scope"],
        "label_and_index"
    );
    assert_eq!(
        done["result"]["result"]["validation"]["payload_verified"],
        false
    );
    let policy = client
        .management_policy(&f.serial, "tape", barcode)
        .unwrap()["policy"]
        .clone();
    assert_eq!(policy["online"], true);
    assert!(policy["offline_message"].is_null());
    assert!(
        owner.trace.lock().unwrap()[start..]
            .iter()
            .any(|c| c[0] == 0x08)
    );
    assert_eq!(
        server.request("POST", &format!("{base}/reconcile"), None).0,
        404
    );
    let refresh = f.root.join("policy-refresh.json");
    cli(&["import", barcode, "--request", refresh.to_str().unwrap()]);
    let request = PreparedManagement::load(&refresh).unwrap();
    let done = wait(&client, &request);
    assert_eq!(request.value()["change"]["kind"], "media-import");
    assert_eq!(done["result"]["refreshed"], true);
    let reads = owner
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c[0] == 0x08)
        .count();
    cli(&["import", "--request", refresh.to_str().unwrap()]);
    assert_eq!(
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x08)
            .count(),
        reads
    );
    let set = f.root.join("policy-set.json");
    cli(&[
        "set",
        barcode,
        "--patch",
        "{\"blacklisted\":true}",
        "--request",
        set.to_str().unwrap(),
    ]);
    wait(&client, &PreparedManagement::load(&set).unwrap());
    assert_eq!(
        client
            .tape_inventory_entry(&f.serial, Some(f.pools[0]), barcode)
            .unwrap()["tape"]["state"],
        "blacklisted"
    );
    assert!(
        !owner.trace.lock().unwrap()[start..]
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x04 | 0x19)),
        "维护接口不应写带、格式化或擦除"
    );
    drop(entry);
    drop(server);
    drop(owner);
}
