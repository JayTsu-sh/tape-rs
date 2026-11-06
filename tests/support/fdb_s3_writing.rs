//! Real FDB/TCP, simulated media; every content operation goes through S3.
use super::*;
use std::collections::BTreeMap;
use tape_rs::daemon::s3;

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；并发 S3 TCP 合批与模拟磁带，不访问真实设备"]
fn concurrent_s3_puts_share_s4_and_preserve_individual_checksums() {
    use base64::Engine as _;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let before = rt
        .block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 12),
        )
        .unwrap()
        .unwrap();
    let context = super::active_pools::context(&owner, 71, 12);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for n in 0..4 {
            let (server, bucket, barrier) = (&server, &bucket, &barrier);
            scope.spawn(move || {
                let bytes = vec![65 + n; 43];
                let crc = base64::engine::general_purpose::STANDARD.encode(crc32fast::hash(&bytes).to_be_bytes());
                barrier.wait();
                let response = server.request_body("PUT", &format!("/{bucket}/batch-s3-{n}"),
                    Some(&format!("Content-Length: 43\r\nIf-None-Match: *\r\nx-amz-sdk-checksum-algorithm: CRC32\r\nx-amz-checksum-crc32: {crc}\r\nx-amz-meta-sample: {n}\r\n")), &bytes);
                assert_eq!(response.0, 200, "{}", String::from_utf8_lossy(&response.2));
                assert!(response.1.contains(&format!("x-amz-checksum-crc32: {}", crc.to_ascii_lowercase())));
            });
        }
    });
    let after = rt
        .block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 12),
        )
        .unwrap()
        .unwrap();
    assert!(
        (1..4).contains(&(after.index.generation - before.index.generation)),
        "concurrent public S3 requests must share at least one media generation"
    );
    for n in 0..4 {
        let bytes = vec![65 + n; 43];
        let crc =
            base64::engine::general_purpose::STANDARD.encode(crc32fast::hash(&bytes).to_be_bytes());
        let get = server.request(
            "GET",
            &format!("/{bucket}/batch-s3-{n}"),
            Some("x-amz-checksum-mode: ENABLED\r\n"),
        );
        assert_eq!(get.0, 200);
        assert_eq!(get.2, bytes);
        assert!(get.1.contains(&format!(
            "x-amz-checksum-crc32: {}",
            crc.to_ascii_lowercase()
        )));
        assert!(get.1.contains(&format!("x-amz-meta-sample: {n}")));
    }
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 S3 TCP 与模拟磁带，不访问真实设备"]
fn consecutive_s3_puts_append_without_recovering_the_same_media_again() {
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
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let put = |name: &str, bytes: &[u8]| {
        let response = server.request_body(
            "PUT",
            &format!("/{bucket}/{name}"),
            Some(&format!(
                "Content-Length: {}\r\nIf-None-Match: *\r\n",
                bytes.len()
            )),
            bytes,
        );
        assert_eq!(response.0, 200, "{}", String::from_utf8_lossy(&response.2));
    };
    put("sequential-first", b"first");
    let before = owner.trace.lock().unwrap().len();
    put("sequential-second", b"second");
    let sequential = owner.trace.lock().unwrap()[before..].to_vec();
    assert!(
        sequential.iter().all(|cdb| cdb[0] != 0x08),
        "a completed, unchanged write session must not reread its label and index"
    );
    assert!(
        sequential.iter().all(|cdb| cdb[0] != 0x92),
        "consecutive writes must retain the confirmed append position"
    );
    assert!(sequential.iter().any(|cdb| cdb[0] == 0x0a));
    assert!(
        sequential.iter().any(|cdb| cdb[0] == 0x10),
        "S4 still flushes"
    );
    // Interleaved reads leave sequential-write mode; the next write must
    // recover the real append position instead of trusting a stale cursor.
    let read = server.request("GET", &format!("/{bucket}/sequential-first"), None);
    assert_eq!(read.0, 200);
    assert_eq!(read.2, b"first");
    let before = owner.trace.lock().unwrap().len();
    put("sequential-after-read", b"third");
    assert!(
        owner.trace.lock().unwrap()[before..]
            .iter()
            .any(|cdb| cdb[0] == 0x08)
    );
    for (name, expected) in [
        ("sequential-first", &b"first"[..]),
        ("sequential-second", &b"second"[..]),
        ("sequential-after-read", &b"third"[..]),
    ] {
        let get = server.request("GET", &format!("/{bucket}/{name}"), None);
        assert_eq!(get.0, 200);
        assert_eq!(get.2, expected);
    }
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

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
    let bad_crc = entry.request_body(
        "PUT", &path,
        Some("Content-Length: 3\r\nx-amz-sdk-checksum-algorithm: CRC32\r\nx-amz-checksum-crc32: AAAAAA==\r\n"),
        b"abc",
    );
    assert_eq!(bad_crc.0, 400);
    assert!(String::from_utf8_lossy(&bad_crc.2).contains("BadDigest"));
    assert_eq!(
        writes(),
        before,
        "CRC32 mismatch must precede parent creation"
    );
    let put=entry.request_body("PUT",&path,Some("Content-Length: 3\r\nContent-MD5: kAFQmDzST7DWlj99KOF/cg==\r\nx-amz-sdk-checksum-algorithm: CRC32\r\nx-amz-checksum-crc32: NSRBwg==\r\nContent-Type: text/plain\r\nCache-Control: max-age=60\r\nx-amz-meta-origin: s3-test\r\nIf-None-Match: *\r\n"),b"abc");
    assert_eq!(put.0, 200, "{}", String::from_utf8_lossy(&put.2));
    assert!(writes() > before);
    assert!(put.1.contains("x-amz-checksum-crc32: nsrbwg=="));
    let head = entry.request("HEAD", &path, None);
    assert_eq!(head.0, 200);
    assert!(head.1.contains("content-length: 3"));
    assert!(head.1.contains("content-type: text/plain"));
    assert!(head.1.contains("x-amz-meta-origin: s3-test"));
    assert!(head.1.contains("cache-control: max-age=60"));
    assert!(!head.1.contains("x-amz-checksum-"));
    for method in ["HEAD", "GET"] {
        let checked = entry.request(method, &path, Some("x-amz-checksum-mode: ENABLED\r\n"));
        assert_eq!(checked.0, 200);
        assert!(checked.1.contains("x-amz-checksum-crc32: nsrbwg=="));
        assert!(!checked.1.contains("x-amz-checksum-sha256:"));
    }
    let range = entry.request(
        "GET",
        &path,
        Some("Range: bytes=1-2\r\nx-amz-checksum-mode: ENABLED\r\n"),
    );
    assert_eq!(range.2, b"bc");
    assert!(!range.1.contains("x-amz-checksum-"));
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
                Some("Content-Length: 3\r\nIf-None-Match: *\r\nx-amz-sdk-checksum-algorithm: CRC32\r\nx-amz-checksum-crc32: 6466Zw==\r\n"),
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
    let replaced_checksum = entry.request("HEAD", &path, Some("x-amz-checksum-mode: ENABLED\r\n"));
    assert!(!replaced_checksum.1.contains("x-amz-checksum-crc32:"));
    assert!(replaced_checksum.1.contains("x-amz-checksum-sha256:"));
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
    if let Ok(python) = std::env::var("TAPE_RS_S3_PYTHON") {
        let output = std::process::Command::new(python)
            .arg("-c")
            .arg(include_str!("fdb_s3_crc32.py"))
            .arg(&entry.address)
            .arg(&bucket)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    }
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
