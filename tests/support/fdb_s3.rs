use super::*;
use std::collections::BTreeMap;
use tape_rs::daemon::{http::HttpContext, s3, state::Executor};

fn element(bytes: &[u8], name: &str) -> String {
    let text = std::str::from_utf8(bytes).unwrap();
    let value = text
        .split_once(&format!("<{name}>"))
        .unwrap()
        .1
        .split_once(&format!("</{name}>"))
        .unwrap()
        .0;
    quick_xml::escape::unescape(value).unwrap().into_owned()
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟磁带与真实 S3 TCP；TAPE_RS_S3_PYTHON 可启用 boto3"]
fn fdb_s3_readonly_lists_reads_and_forwards_without_legacy_fallback() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut objects = (0..1005).map(|i| format!("/k{i:04}")).collect::<Vec<_>>();
    objects.extend(["/a/x", "/a-/x", "/a.txt", "/a0", "/é/a", "/中文/a"].map(str::to_owned));
    let f = rt.block_on(fixture_with_objects(&cluster, &objects));
    let owner = Owner::start(&f, 12, json!({}));
    {
        let mut status = owner.status.lock().unwrap();
        status.id = 71;
        status.executor = Some(Executor {
            node: 71,
            round: 12,
            term: 1,
            fenced: true,
        });
        status.executor_ready = true;
        status.catalog_quiesce_index = Some(f.route.frozen_index);
        assert!(
            status.pools.is_empty(),
            "FDB buckets 不能依赖旧管理状态的池列表"
        );
    }
    owner.files.set_catalog_quiesced(true);
    let context = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: owner.status.clone(),
        exec: Mutex::new(owner.tx.clone()),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context.clone(),
        HashMap::new(),
        false,
    ));
    let mut status = owner.status.lock().unwrap().clone();
    status.id = 72;
    let remote = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: Arc::new(Mutex::new(status)),
        exec: Mutex::new(channel().0),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let entry = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        routes,
        BTreeMap::from([(f.serial.clone(), remote.clone())]),
        remote,
        HashMap::from([(71, server.address.clone())]),
        false,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let other = s3::bucket_name(&f.serial, &f.pools[1].to_string());
    let counts = || {
        let log = owner.trace.lock().unwrap();
        (
            log.iter().filter(|c| c[0] == 0x08).count(),
            log.iter().filter(|c| c[0] == 0x0a).count(),
            log.iter().filter(|c| c[0] == 0xa5).count(),
        )
    };
    let before = counts();
    let response = entry.request("GET", "/", None);
    assert_eq!(response.0, 200);
    let text = std::str::from_utf8(&response.2).unwrap();
    assert!(text.contains(&bucket) && text.contains(&other));
    assert_eq!(entry.request("HEAD", &format!("/{bucket}"), None).0, 200);
    let first = entry.request("GET", &format!("/{bucket}?list-type=2"), None);
    assert_eq!(first.0, 200);
    assert_eq!(element(&first.2, "KeyCount"), "1000");
    assert_eq!(
        String::from_utf8_lossy(&first.2)
            .matches("<LastModified>")
            .count(),
        1000
    );
    let token = element(&first.2, "NextContinuationToken");
    assert_eq!(element(&first.2, "Key"), "a-/x");
    assert!(!String::from_utf8_lossy(&first.2).contains(&format!("<Key>{}/", f.pools[0])));
    let second = entry.request(
        "GET",
        &format!("/{bucket}?list-type=2&continuation-token={token}"),
        None,
    );
    assert_eq!(second.0, 200);
    assert_eq!(element(&second.2, "IsTruncated"), "false");
    assert_eq!(element(&second.2, "KeyCount"), "14");
    assert_eq!(
        entry
            .request(
                "GET",
                &format!("/{other}?list-type=2&continuation-token={token}"),
                None
            )
            .0,
        400
    );
    let path = format!("/{bucket}/file-0");
    let head = entry.request("HEAD", &path, None);
    assert_eq!(head.0, 200);
    assert!(head.2.is_empty());
    assert!(head.1.contains("content-length: 9"));
    let modified = head
        .1
        .lines()
        .find_map(|s| s.strip_prefix("last-modified: "))
        .expect("Last-Modified");
    // The shared TCP helper lowercases response values. Recreate a valid HTTP
    // date before sending the validator back to the actual S3 server.
    let modified = chrono::DateTime::parse_from_rfc2822(modified)
        .unwrap()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let tag = head
        .1
        .lines()
        .find_map(|s| s.strip_prefix("etag: "))
        .unwrap();
    assert_eq!(
        entry
            .request(
                "GET",
                &path,
                Some(&format!("If-None-Match: {tag}\r\nRange: invalid\r\n"))
            )
            .0,
        304
    );
    assert_eq!(
        entry
            .request("GET", &path, Some("If-Match: \"wrong\"\r\n"))
            .0,
        412
    );
    assert_eq!(
        entry.request("GET", &path, Some("Range: bytes=999-\r\n")).0,
        416
    );
    assert_eq!(
        entry
            .request("GET", &path, Some("Range: bytes=0-1,3-4\r\n"))
            .0,
        400
    );
    assert_eq!(
        entry
            .request(
                "GET",
                &path,
                Some("Range: bytes=0-1\r\nRange: bytes=2-3\r\n")
            )
            .0,
        400
    );
    assert_eq!(
        entry
            .request(
                "GET",
                &path,
                Some(&format!(
                    "If-Modified-Since: {modified}\r\nRange: invalid\r\n"
                ))
            )
            .0,
        304
    );
    for (headers, expected) in [
        (
            "If-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n".to_owned(),
            412,
        ),
        (
            format!("If-Match: {tag}\r\nIf-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n"),
            200,
        ),
        (
            format!("If-None-Match: {tag}\r\nIf-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n"),
            304,
        ),
        (
            format!("If-None-Match: \"other\"\r\nIf-Modified-Since: {modified}\r\n"),
            200,
        ),
        (
            format!("If-Modified-Since: {modified}\r\nIf-Modified-Since: {modified}\r\n"),
            400,
        ),
    ] {
        assert_eq!(entry.request("HEAD", &path, Some(&headers)).0, expected);
    }
    let imported = entry.request("HEAD", &format!("/{bucket}/k0000"), None);
    assert_eq!(imported.0, 200);
    for header in [
        "content-type: text/plain",
        "cache-control: max-age=60",
        "x-amz-meta-origin: fdb-test",
        "last-modified: ",
    ] {
        assert!(imported.1.contains(header), "{}", imported.1);
    }
    // Query the small, digest-bound projection while a large opaque attribute
    // chunk is unavailable. Neither HEAD nor listing may hydrate that object.
    rt.block_on(async {
        let ResolvedPath::File {
            attributes: Some(attributes),
            ..
        } = f
            .catalog
            .resolve_active_metadata_for_route(
                &f.serial,
                format!("/{}/k0000", f.pools[0]).as_bytes(),
                f.route,
            )
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("imported attributes");
        };
        let db = foundationdb::Database::from_path(&cluster).unwrap();
        let mut prefix = b"tape-rs/catalog/22/M/".to_vec();
        prefix.extend_from_slice(f.route.route.namespace.as_bytes());
        prefix.extend_from_slice(&attributes.digest());
        let mut chunk_key = prefix.clone();
        chunk_key.extend_from_slice(&[1, 0, 0]);
        let mut metadata_key = prefix;
        metadata_key.push(2);
        let tr = db.create_trx().unwrap();
        let chunk = tr.get(&chunk_key, false).await.unwrap().unwrap().to_vec();
        let metadata = tr
            .get(&metadata_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.clear(&chunk_key);
        tr.commit().await.unwrap();
        assert_eq!(
            entry.request("HEAD", &format!("/{bucket}/k0000"), None).0,
            200
        );
        let listed = entry.request("GET", &format!("/{bucket}?list-type=2&prefix=k0000"), None);
        assert_eq!(listed.0, 200);
        assert_eq!(element(&listed.2, "KeyCount"), "1");
        assert!(!element(&listed.2, "LastModified").is_empty());
        let tr = db.create_trx().unwrap();
        tr.clear(&metadata_key);
        tr.commit().await.unwrap();
        assert_eq!(
            entry.request("HEAD", &format!("/{bucket}/k0000"), None).0,
            503
        );
        assert_eq!(
            entry
                .request("GET", &format!("/{bucket}?list-type=2&prefix=k0000"), None)
                .0,
            503
        );
        let tr = db.create_trx().unwrap();
        tr.set(&chunk_key, &chunk);
        tr.set(&metadata_key, &metadata);
        tr.commit().await.unwrap();
    });
    assert_eq!(
        entry.request("GET", &format!("/{other}/file-0"), None).0,
        404
    );
    assert_eq!(
        counts(),
        before,
        "列举、HEAD、条件失败和错池不能读带或移动机械"
    );
    let range = entry.request("GET", &path, Some("Range: bytes=2-5\r\n"));
    assert_eq!(range.0, 206);
    assert_eq!(range.2, b"nten");
    assert!(range.1.contains("content-range: bytes 2-5/9"));
    let full = entry.request("GET", &path, Some(&format!("If-Match: {tag}\r\n")));
    assert_eq!(full.0, 200);
    assert_eq!(full.2, b"content-0");
    assert!(full.1.contains(tag));
    let baseline = counts();
    for method in ["PUT", "DELETE", "POST", "PATCH", "OPTIONS"] {
        let denied = entry.request_body(method, &path, Some("Content-Length: 3\r\n"), b"abc");
        assert_eq!(denied.0, 405, "{method}");
        assert!(denied.1.contains("allow: get, head"));
    }
    assert_eq!(counts(), baseline, "S3 只读拒绝不能进入设备队列");
    let looped = entry.request("GET", &path, Some("x-tapers-forwarded: 1\r\n"));
    assert_eq!(looped.0, 503);
    assert!(String::from_utf8_lossy(&looped.2).contains("<Error>"));
    if let Ok(python) = std::env::var("TAPE_RS_S3_PYTHON") {
        let result = std::process::Command::new(python)
            .arg("-c")
            .arg(include_str!("fdb_s3_readonly.py"))
            .args([
                &entry.address,
                &bucket,
                &other,
                &f.pools[0].to_string(),
                "1014",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        println!("{}", String::from_utf8_lossy(&result.stdout));
    }
    assert_eq!(counts().1, before.1, "读取不能 WRITE(6)");
    // Let the real owner finish a verified read, then revoke its Serving
    // permission before the HTTP adapter receives the spool acknowledgement.
    let (relay_tx, relay_rx) = channel();
    let actual = owner.tx.clone();
    let permission = owner.permission.clone();
    let relay = std::thread::spawn(move || {
        let ExecRequest::Scoped {
            pool,
            lane,
            request,
        } = relay_rx.recv_timeout(Duration::from_secs(15)).unwrap()
        else {
            panic!("expected scoped read");
        };
        let ExecRequest::ReadTo { path, file, reply } = *request else {
            panic!("expected read spool");
        };
        let (finished, received) = channel();
        actual
            .send(ExecRequest::Scoped {
                pool,
                lane,
                request: Box::new(ExecRequest::ReadTo {
                    path,
                    file,
                    reply: finished,
                }),
            })
            .unwrap();
        let result = received.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(result.is_ok(), "真实 owner 读取须先完成");
        permission.close(12);
        reply.send(result).unwrap();
    });
    let relay_ctx = Arc::new(HttpContext {
        files: owner.files.clone(),
        status: owner.status.clone(),
        exec: Mutex::new(relay_tx),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let late = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), relay_ctx)]),
        context.clone(),
        HashMap::new(),
        false,
    ));
    let denied = late.request("GET", &path, None);
    assert_eq!(denied.0, 503, "回执前丢失 Serving 不能发布内容");
    assert!(!String::from_utf8_lossy(&denied.2).contains("content-0"));
    relay.join().unwrap();
    drop(late);
    let mut wrong = f.route;
    wrong.frozen_index += 1;
    let stale = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), wrong)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        false,
    ));
    assert_eq!(
        stale.request("GET", &path, None).0,
        503,
        "Active 路由失配不能变成404或旧目录回退"
    );
    owner.permission.close(12);
    let baseline = counts();
    assert_eq!(server.request("GET", &path, None).0, 503);
    assert_eq!(server.request("HEAD", &path, None).0, 503);
    assert_eq!(
        server
            .request("GET", &format!("/{bucket}?list-type=2"), None)
            .0,
        503
    );
    assert_eq!(counts(), baseline);
    drop(stale);
    drop(entry);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
