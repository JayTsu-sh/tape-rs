//! Real FDB/TCP, simulated media; every content operation goes through S3.
use super::*;
use std::collections::BTreeMap;
use tape_rs::daemon::s3;

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 S3 TCP 与模拟磁带，不访问真实设备"]
fn s3_put_overwrite_metadata_and_delete_use_fdb_and_media_commits() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let context = super::active_pools::context(&owner, 71, 12);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context.clone(),
        HashMap::new(),
        true,
    ));
    let mut status = context.status.lock().unwrap().clone();
    status.id = 72;
    let remote = Arc::new(tape_rs::daemon::http::HttpContext {
        status: Arc::new(Mutex::new(status)),
        files: owner.files.clone(),
        exec: Mutex::new(channel().0),
        node: Mutex::new(channel().0),
        client_addrs: HashMap::new(),
        wait_timeout: Duration::from_secs(15),
    });
    let entry = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), remote.clone())]),
        remote,
        HashMap::from([(71, server.address.clone())]),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let path = format!("/{bucket}/new/dir/object");
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x0a)
            .count()
    };
    let before = writes();
    let bad = entry.request_body(
        "PUT",
        &path,
        Some("Content-Length: 3\r\nContent-MD5: AAAAAAAAAAAAAAAAAAAAAA==\r\n"),
        b"abc",
    );
    assert_eq!(bad.0, 400, "{}", String::from_utf8_lossy(&bad.2));
    assert_eq!(
        writes(),
        before,
        "bad digest must not even create a directory"
    );
    let put=entry.request_body("PUT",&path,Some("Content-Length: 3\r\nContent-MD5: kAFQmDzST7DWlj99KOF/cg==\r\nContent-Type: text/plain\r\nCache-Control: max-age=60\r\nx-amz-meta-origin: s3-test\r\nIf-None-Match: *\r\n"),b"abc");
    assert_eq!(put.0, 200, "{}", String::from_utf8_lossy(&put.2));
    assert!(writes() > before);
    let head = entry.request("HEAD", &path, None);
    assert_eq!(head.0, 200);
    assert!(head.1.contains("content-length: 3"));
    assert!(head.1.contains("content-type: text/plain"));
    assert!(head.1.contains("x-amz-meta-origin: s3-test"));
    assert!(head.1.contains("cache-control: max-age=60"));
    let etag = head
        .1
        .lines()
        .find_map(|l| l.strip_prefix("etag: "))
        .unwrap();
    assert!(put.1.contains(&format!("etag: {etag}")));
    let get = entry.request("GET", &path, None);
    assert_eq!(get.0, 200);
    assert_eq!(get.2, b"abc");
    let before = writes();
    assert_eq!(
        entry
            .request_body(
                "PUT",
                &path,
                Some("Content-Length: 3\r\nIf-None-Match: *\r\n"),
                b"xyz"
            )
            .0,
        412
    );
    assert_eq!(
        entry
            .request_body(
                "PUT",
                &path,
                Some("Content-Length: 3\r\nIf-Match: \"wrong\"\r\n"),
                b"xyz"
            )
            .0,
        412
    );
    assert_eq!(writes(), before);
    let replaced = entry.request_body(
        "PUT",
        &path,
        Some(&format!("Content-Length: 3\r\nIf-Match: {etag}\r\n")),
        b"xyz",
    );
    assert_eq!(replaced.0, 200, "{}", String::from_utf8_lossy(&replaced.2));
    let head = entry.request("HEAD", &path, None);
    assert!(!head.1.contains("x-amz-meta-origin"));
    assert!(!head.1.contains("cache-control"));
    assert!(head.1.contains("content-type: application/octet-stream"));
    let replaced_get = entry.request("GET", &path, None);
    assert_eq!(replaced_get.2, b"xyz");
    assert!(
        !replaced_get.1.contains("cache-control:"),
        "S3 must not inherit internal download cache policy"
    );
    assert_eq!(
        entry.request("GET", &path, Some("Range: bytes=1-2\r\n")).2,
        b"yz"
    );
    let before = writes();
    assert_eq!(
        entry
            .request("DELETE", &path, Some("If-Match: \"wrong\"\r\n"))
            .0,
        412
    );
    assert_eq!(writes(), before);
    let deleted = entry.request("DELETE", &path, None);
    assert_eq!(deleted.0, 204, "{}", String::from_utf8_lossy(&deleted.2));
    assert!(writes() > before, "delete commits LTFS index");
    assert_eq!(entry.request("HEAD", &path, None).0, 404);
    assert_eq!(entry.request("DELETE", &path, None).0, 204);
    let listed = entry.request("GET", &format!("/{bucket}?list-type=2&prefix=new/"), None);
    assert_eq!(listed.0, 200);
    assert!(!String::from_utf8_lossy(&listed.2).contains("<Key>new/dir/object</Key>"));
    let empty = format!("/{bucket}/empty");
    assert_eq!(
        entry
            .request_body("PUT", &empty, Some("Content-Length: 0\r\n"), b"")
            .0,
        200
    );
    let empty_get = entry.request("GET", &empty, None);
    assert_eq!(empty_get.0, 200);
    assert!(empty_get.2.is_empty());
    assert!(
        rt.block_on(
            f.catalog
                .resolve_active_path(&f.serial, format!("/{}/empty", f.pools[0]).as_bytes())
        )
        .unwrap()
        .is_some()
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
            .request_body("PUT", &path, Some("Content-Length: 3\r\n"), b"abc")
            .0,
        405
    );
    assert_eq!(readonly.request("DELETE", &empty, None).0, 405);
    drop(readonly);
    drop(entry);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
