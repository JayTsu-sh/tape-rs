use super::*;
use std::io::BufRead as _;
use tape_rs::client::ClientError;
use tape_rs::client::fdb::FdbClient;

fn cli(server: &ReadHttpServer, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &server.address])
        .args(args)
        .output()
        .unwrap()
}

fn cli_value(server: &ReadHttpServer, args: &[&str]) -> serde_json::Value {
    let output = cli(server, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_losing_reply(server: &ReadHttpServer, file: &std::path::Path) -> serde_json::Value {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let target = server.address.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let finished = stop.clone();
    let proxy = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = Vec::new();
        while !finished.load(Ordering::SeqCst) && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("{error}"),
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
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
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
    let client = FdbClient::new([address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let value = client.run_node_mutation_saved(file).unwrap();
    stop.store(true, Ordering::SeqCst);
    proxy.join().unwrap();
    value
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；默认 SDK/CLI、丢回复、真实 HTTP 和模拟介质，不接触真实设备"]
fn default_client_and_cli_mutate_saved_nodes_and_keep_original_outcomes() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_client_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let readonly = FdbClient::new([&reader.address]).unwrap();
    let ns = f.route.route.namespace;
    let mkdir = f.root.join("mkdir.json");
    let prepared = client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/sdk",
            original_condition(&client, &f.serial, ns, f.pools[0], "/sdk", None),
            json!({"kind":"create-directory"}),
            &mkdir,
        )
        .unwrap();

    let saved = fs::read(mkdir.join("request.json")).unwrap();
    assert!(client.node_mutation_status(&prepared).unwrap().is_none());
    assert!(cli_value(&reader, &["fdb", "fs", "status", mkdir.to_str().unwrap()]).is_null());
    let first = run_losing_reply(&server, &mkdir);
    assert_eq!(first["status"], "completed");
    assert_eq!(client.run_node_mutation_saved(&mkdir).unwrap(), first);
    assert_eq!(fs::read(mkdir.join("request.json")).unwrap(), saved);
    assert_eq!(client.cancel_node_mutation_saved(&mkdir).unwrap(), first);
    let original_file = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let metadata = f.root.join("file-metadata.json");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/file-0",
            original_condition(
                &client,
                &f.serial,
                ns,
                f.pools[0],
                "/file-0",
                original_file["version"].as_u64(),
            ),
            json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":null}),
            &metadata,
        )
        .unwrap();
    let changed = client.run_node_mutation_saved(&metadata).unwrap();
    assert_eq!(
        changed["result"]["version"],
        original_file["version"].as_u64().unwrap() + 1
    );
    let current = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    for field in ["id", "size", "volume_id", "barcode", "digest"] {
        assert_eq!(current[field], original_file[field], "{field}");
    }
    assert_eq!(
        cli_value(&reader, &["fdb", "fs", "run", mkdir.to_str().unwrap()]),
        first
    );
    let link = f.root.join("link.json");
    let pool = f.pools[0].to_string();
    let namespace = ns.to_string();
    let args = [
        "--library",
        &f.serial,
        "--pool",
        &pool,
        "fdb",
        "fs",
        "prepare",
        "/sdk/link",
        "--namespace",
        &namespace,
        "--request",
        link.to_str().unwrap(),
        "--change",
        r#"{"kind":"create-symlink","target":"/file-0"}"#,
    ];
    let output = cli(&server, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let link_request = tape_rs::client::fdb::NodeJournal::open(&link)
        .unwrap()
        .prepared()
        .cloned()
        .unwrap();
    assert!(
        client
            .node_mutation_status(&link_request)
            .unwrap()
            .is_none()
    );
    let link_saved = fs::read(link.join("request.json")).unwrap();
    let created = cli_value(&server, &["fdb", "fs", "run", link.to_str().unwrap()]);
    assert_eq!(created["result"]["version"], 1);
    let change = f.root.join("change.json");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/sdk/link",
            original_condition(&client, &f.serial, ns, f.pools[0], "/sdk/link", Some(1)),
            json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":null}),
            &change,
        )
        .unwrap();
    assert_eq!(
        client.run_node_mutation_saved(&change).unwrap()["result"]["version"],
        2
    );
    assert_eq!(
        cli_value(&reader, &["fdb", "fs", "status", link.to_str().unwrap()]),
        created
    );
    let unlink = f.root.join("unlink.json");
    let output = cli(
        &server,
        &[
            "--library",
            &f.serial,
            "--pool",
            &pool,
            "fdb",
            "fs",
            "prepare",
            "/sdk/link",
            "--expected-version",
            "2",
            "--namespace",
            &namespace,
            "--request",
            unlink.to_str().unwrap(),
            "--change",
            r#"{"kind":"unlink"}"#,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deleted = cli_value(&server, &["fdb", "fs", "run", unlink.to_str().unwrap()]);
    assert_eq!(deleted["result"]["deleted"], true);
    assert_eq!(deleted["result"]["version"], 3);
    assert_eq!(
        cli_value(&server, &["fdb", "fs", "run", link.to_str().unwrap()]),
        created
    );
    let recreated = f.root.join("recreated.json");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            "/sdk/link",
            original_condition(&client, &f.serial, ns, f.pools[0], "/sdk/link", None),
            json!({"kind":"create-directory"}),
            &recreated,
        )
        .unwrap();
    let new = client.run_node_mutation_saved(&recreated).unwrap();
    assert_ne!(new["result"]["node"], created["result"]["node"]);
    assert_eq!(client.run_node_mutation_saved(&unlink).unwrap(), deleted);
    assert_eq!(fs::read(link.join("request.json")).unwrap(), link_saved);
    assert!(
        !cli(
            &server,
            &[
                "--library",
                &f.serial,
                "fdb",
                "fs",
                "run",
                mkdir.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    let mut overwrite = args;
    overwrite[7] = "/not-admitted";
    assert!(!cli(&server, &overwrite).status.success());
    assert_eq!(fs::read(link.join("request.json")).unwrap(), link_saved);
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/not-admitted")
            .unwrap()
            .is_none()
    );
    owner.expect(0);
    drop(server);
    owner.permission.close(12);
    drop(owner);
    assert_eq!(
        readonly.node_mutation_status(&prepared).unwrap().unwrap(),
        first
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    assert_eq!(
        cli_value(&server, &["fdb", "fs", "run", mkdir.to_str().unwrap()]),
        first
    );
    assert_eq!(
        cli_value(&server, &["fdb", "fs", "run", unlink.to_str().unwrap()]),
        deleted
    );
    owner.expect(0);
    drop(server);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；默认 SDK 无介质不确定结果、只读查询、接管取消与 CLI 重试，不接触真实设备"]
fn node_clients_retain_pending_requests_and_cancel_original_identity_after_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let empty = Uuid::new_v4();
    let f = rt.block_on(Box::pin(fixture_with_extra_pool(
        &cluster,
        &[],
        Some(empty),
    )));
    eprintln!("node_client_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_millis(500));
    let readonly = FdbClient::new([&reader.address]).unwrap();
    let file = f.root.join("waiting.json");
    let prepared = client
        .prepare_node_mutation(
            &f.serial,
            empty,
            "/wait",
            original_condition(
                &client,
                &f.serial,
                f.route.route.namespace,
                empty,
                "/wait",
                None,
            ),
            json!({"kind":"create-directory"}),
            &file,
        )
        .unwrap();

    let original = fs::read(file.join("request.json")).unwrap();
    assert!(matches!(
        client.run_node_mutation_saved(&file),
        Err(ClientError::Indeterminate(_))
    ));
    let pending = client.node_mutation_status(&prepared).unwrap().unwrap();
    assert_eq!(pending["status"], "queued");
    assert_eq!(fs::read(file.join("request.json")).unwrap(), original);
    assert!(owner.permission.allows(12, Phase::Serving));
    drop(server);
    owner.permission.close(12);
    drop(owner);
    assert_eq!(
        readonly.node_mutation_status(&prepared).unwrap().unwrap(),
        pending
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let client = FdbClient::new([&server.address]).unwrap();
    let cancelled = client.cancel_node_mutation_saved(&file).unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["accepted_owner_epoch"], 12);
    assert_eq!(cancelled["completed_owner_epoch"], 13);
    assert_eq!(
        cli_value(&server, &["fdb", "fs", "cancel", file.to_str().unwrap()]),
        cancelled
    );
    assert!(matches!(
        client.run_node_mutation_saved(&file),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(
        !cli(&server, &["fdb", "fs", "run", file.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        readonly.node_mutation_status(&prepared).unwrap().unwrap(),
        cancelled
    );
    assert_eq!(fs::read(file.join("request.json")).unwrap(), original);
    drop(server);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
