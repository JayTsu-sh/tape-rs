use super::*;
use tape_rs::daemon::s3;

#[test]
#[ignore = "需要隔离 FDB；真实 S3 TCP/owner 与模拟磁带，不访问真实设备"]
fn rename_object_preserves_original_token_result_after_recreation_and_owner_loss() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_s3_rename_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::super::super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(15);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context.clone(),
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let path = format!("/{bucket}/file-1?renameObject");
    let token = Uuid::new_v4().to_string();
    let headers = format!(
        "x-amz-rename-source: {bucket}/file-0\r\nx-amz-client-token: {token}\r\nContent-Length: 0\r\n"
    );
    let mut remote = super::super::super::active_pools::context(&owner, 72, 12);
    Arc::get_mut(&mut remote).unwrap().wait_timeout = Duration::from_secs(30);
    let entry = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), remote.clone())]),
        remote,
        HashMap::from([(71, server.address.clone())]),
        true,
    ));
    let source_head = server.request(
        "HEAD",
        &format!("/{bucket}/file-0"),
        Some("x-amz-checksum-mode: ENABLED\r\n"),
    );
    lose_reply(&entry.address, &path, &headers);
    let response = server.request("PUT", &path, Some(&headers));
    assert_eq!(response.0, 200, "{}", String::from_utf8_lossy(&response.2));
    assert!(response.2.is_empty());
    let target_head = server.request(
        "HEAD",
        &format!("/{bucket}/file-1"),
        Some("x-amz-checksum-mode: ENABLED\r\n"),
    );
    for name in [
        "last-modified: ",
        "x-amz-checksum-sha256: ",
        "content-length: ",
    ] {
        assert_eq!(
            source_head.1.lines().find(|line| line.starts_with(name)),
            target_head.1.lines().find(|line| line.starts_with(name)),
            "{name}"
        );
    }

    assert_eq!(
        server.request("GET", &format!("/{bucket}/file-1"), None).2,
        b"content-0"
    );
    if let Ok(python) = std::env::var("TAPE_RS_S3_PYTHON") {
        let output = std::process::Command::new(python)
            .arg("-c")
            .arg(include_str!("fdb_s3_rename.py"))
            .args([&entry.address, &bucket])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    }
    let http = writer(&f, &owner, 12);
    let client = FdbClient::new([&http.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(10));
    mutate(&client, &f, "/file-0", json!({"kind":"create-directory"}));
    let before = client.stat(&f.serial, f.pools[0], "/file-0").unwrap();
    let commands = owner.trace.lock().unwrap().len();
    assert_eq!(server.request("PUT", &path, Some(&headers)).0, 200);
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/file-0").unwrap(),
        before
    );
    assert_eq!(owner.trace.lock().unwrap().len(), commands);
    let changed = server.request(
        "PUT",
        &format!("/{bucket}/other?renameObject"),
        Some(&headers),
    );
    assert_eq!(changed.0, 400);
    assert!(String::from_utf8_lossy(&changed.2).contains("IdempotencyParameterMismatch"));
    drop(entry);
    drop(http);
    drop(client);
    drop(owner);
    context.status.lock().unwrap().executor = None;
    assert_eq!(
        server.request("PUT", &path, Some(&headers)).0,
        200,
        "known token result must not need a live owner or original source path"
    );
    let database = foundationdb::Database::new(Some(&cluster)).unwrap();
    let mut key = b"tape-rs/catalog/22/@/".to_vec();
    key.extend_from_slice(f.route.route.namespace.as_bytes());
    key.extend_from_slice(b"QJ");
    key.extend_from_slice(&(f.serial.len() as u16).to_be_bytes());
    key.extend_from_slice(f.serial.as_bytes());
    key.extend_from_slice(f.pools[0].as_bytes());
    use sha2::{Digest, Sha256};
    key.extend_from_slice(&Sha256::digest(token.as_bytes()));
    let saved = rt.block_on(async {
        let transaction = database.create_trx().unwrap();
        let saved = transaction
            .get(&key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        transaction.set(&key, b"{}");
        transaction.commit().await.unwrap();
        saved
    });
    assert_eq!(server.request("PUT", &path, Some(&headers)).0, 503);
    rt.block_on(async {
        let transaction = database.create_trx().unwrap();
        assert_eq!(
            transaction.get(&key, false).await.unwrap().as_deref(),
            Some(&b"{}"[..])
        );
        transaction.set(&key, &saved);
        transaction.commit().await.unwrap();
    });
    assert_eq!(server.request("PUT", &path, Some(&headers)).0, 200);
    drop(server);
    fs::remove_dir_all(&f.root).unwrap();
}

fn lose_reply(target: &str, path: &str, headers: &str) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let target = target.to_owned();
    let proxy = std::thread::spawn(move || {
        let (downstream, _) = listener.accept().unwrap();
        downstream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut input = BufReader::new(&downstream);
        let mut raw = Vec::new();
        loop {
            let mut line = String::new();
            assert_ne!(input.read_line(&mut line).unwrap(), 0);
            raw.extend_from_slice(line.as_bytes());
            if line == "\r\n" {
                break;
            }
        }
        let mut upstream = std::net::TcpStream::connect(target).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        upstream.write_all(&raw).unwrap();
        let mut response = Vec::new();
        upstream.read_to_end(&mut response).unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        // The owner and forwarding entry replied, but the caller receives no bytes.
    });
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(
        stream,
        "PUT {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    assert!(response.is_empty());
    proxy.join().unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；S3 条件失败不进入设备队列，不访问真实设备"]
fn rename_object_checks_both_objects_all_conditions_and_rejects_non_objects() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::super::super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(15);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context.clone(),
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let head = server.request("HEAD", &format!("/{bucket}/file-0"), None);
    let source_tag = head
        .1
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    let destination = format!("/{bucket}/file-1?renameObject");
    let mutations = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|cdb| matches!(cdb[0], 0x0a | 0x10 | 0x8d))
            .count()
    };
    let before = mutations();
    for condition in [
        "x-amz-rename-source-if-match: \"wrong\"".to_owned(),
        format!("x-amz-rename-source-if-none-match: {source_tag}"),
        "x-amz-rename-source-if-none-match: *".into(),
        "If-Match: \"wrong\"".into(),
        "If-None-Match: *".into(),
        "x-amz-rename-source-if-modified-since: Thu, 01 Jan 2099 00:00:00 GMT".into(),
        "x-amz-rename-source-if-unmodified-since: Thu, 01 Jan 1970 00:00:00 GMT".into(),
        "If-Modified-Since: Thu, 01 Jan 2099 00:00:00 GMT".into(),
        "If-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT".into(),
    ] {
        let response = server.request("PUT", &destination, Some(&format!(
            "x-amz-rename-source: file-0\r\nx-amz-client-token: {}\r\nContent-Length: 0\r\n{condition}\r\n", Uuid::new_v4())));
        assert_eq!(
            response.0,
            412,
            "{condition}: {}",
            String::from_utf8_lossy(&response.2)
        );
    }
    for (extra, status) in [
        ("x-amz-client-token: duplicate\r\n", 400),
        ("x-amz-rename-source-if-modified-since: invalid\r\n", 400),
        ("x-amz-rename-source-if-match: W/\"weak\"\r\n", 400),
        ("x-amz-rename-source-unrecognized: 1\r\n", 501),
        ("x-amz-copy-source: file-2\r\n", 501),
    ] {
        let response = server.request("PUT", &destination, Some(&format!(
            "x-amz-rename-source: {bucket}/file-0\r\nx-amz-client-token: original\r\nContent-Length: 0\r\n{extra}")));
        assert_eq!(
            response.0,
            status,
            "{}",
            String::from_utf8_lossy(&response.2)
        );
    }
    let foreign = s3::bucket_name(&f.serial, &f.pools[1].to_string());
    assert_eq!(
        server
            .request(
                "PUT",
                &destination,
                Some(&format!(
                    "x-amz-rename-source: {foreign}/file-0\r\nContent-Length: 0\r\n"
                ))
            )
            .0,
        400
    );
    assert_eq!(
        server
            .request_body(
                "PUT",
                &destination,
                Some("x-amz-rename-source: file-0\r\nContent-Length: 1\r\n"),
                b"x"
            )
            .0,
        400
    );
    assert_eq!(
        mutations(),
        before,
        "all failing preconditions must precede queue/media writes"
    );
    let readonly = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        false,
    ));
    assert_eq!(
        readonly
            .request(
                "PUT",
                &destination,
                Some("x-amz-rename-source: file-0\r\nContent-Length: 0\r\n")
            )
            .0,
        405
    );
    drop(readonly);
    // ETag predicates take precedence over the corresponding time predicates.
    let success = server.request("PUT", &destination, Some(&format!(
        "x-amz-rename-source: file-0\r\nx-amz-client-token: precedence\r\nContent-Length: 0\r\nx-amz-rename-source-if-match: {source_tag}\r\nx-amz-rename-source-if-unmodified-since: Thu, 01 Jan 1970 00:00:00 GMT\r\nIf-None-Match: \"other\"\r\nIf-Modified-Since: Thu, 01 Jan 2099 00:00:00 GMT\r\n")));
    assert_eq!(success.0, 200, "{}", String::from_utf8_lossy(&success.2));
    assert_eq!(
        server.request("GET", &format!("/{bucket}/file-1"), None).2,
        b"content-0"
    );
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；无介质超时后按同一 token 接任恢复，仅模拟磁带"]
fn rename_object_retains_original_pending_token_across_timeout_and_takeover() {
    use tape_rs::managed_catalog::S3RenameInput;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false}}}),
        true,
    );
    let mut context = super::super::super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_millis(100);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context.clone(),
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let path = format!("/{bucket}/file-1?renameObject");
    let input = S3RenameInput {
        library: f.serial.clone(),
        namespace: f.route.route.namespace,
        pool: f.pools[0],
        token: Uuid::new_v4().to_string(),
        from: "/file-0".into(),
        to: "/file-1".into(),
        conditions: Default::default(),
    };
    let headers = format!(
        "x-amz-rename-source: file-0\r\nx-amz-client-token: {}\r\nContent-Length: 0\r\n",
        input.token
    );
    let response = server.request("PUT", &path, Some(&headers));
    assert_eq!(response.0, 504, "{}", String::from_utf8_lossy(&response.2));
    let request = rt
        .block_on(f.catalog.s3_rename_request(f.route, &input))
        .unwrap()
        .unwrap();
    let task = rt
        .block_on(f.catalog.node_mutation_task_status(
            &f.serial,
            f.route,
            request.request_epoch(),
            request.spec().operation,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        task.task.state,
        tape_rs::managed_catalog::NodeMutationTaskState::Queued
    );
    assert_eq!(request.spec().from, "/file-0");
    // Competing first preparations for another token must select one original
    // operation even when the transactions start on separate runtime threads.
    let mut racing = input.clone();
    racing.token = Uuid::new_v4().to_string();
    let mut second: serde_json::Value = serde_json::from_slice(&request.encode().unwrap()).unwrap();
    second["operation"] = Uuid::new_v4().to_string().into();
    let second = NodeRenameRequest::decode(second.to_string().as_bytes()).unwrap();
    let selected = std::thread::scope(|scope| {
        let handles: Vec<_> = [request.clone(), second]
            .into_iter()
            .map(|candidate| {
                let catalog = f.catalog.clone();
                let input = racing.clone();
                let route = f.route;
                scope.spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(catalog.remember_s3_rename_request(route, 12, &input, &candidate))
                        .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(selected[0], selected[1]);
    assert_eq!(
        rt.block_on(f.catalog.s3_rename_request(f.route, &racing))
            .unwrap(),
        Some(selected[0].clone())
    );
    drop(owner);
    context.status.lock().unwrap().executor = None;
    assert_eq!(server.request("PUT", &path, Some(&headers)).0, 503);
    drop(server);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    online_tape(&f, 13, BARCODES[0]);
    let mut context = super::super::super::active_pools::context(&owner, 71, 13);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(15);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let response = server.request("PUT", &path, Some(&headers));
    assert_eq!(response.0, 200, "{}", String::from_utf8_lossy(&response.2));
    assert_eq!(
        rt.block_on(f.catalog.s3_rename_request(f.route, &input))
            .unwrap(),
        Some(request)
    );
    assert_eq!(
        server.request("GET", &format!("/{bucket}/file-1"), None).2,
        b"content-0"
    );
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
