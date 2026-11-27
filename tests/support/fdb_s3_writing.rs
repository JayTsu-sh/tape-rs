//! Real FDB/TCP, simulated media; every content operation goes through S3.
use super::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use tape_rs::daemon::s3;
use tape_rs::managed_catalog::{
    NodeMutation, NodeMutationTaskState, NodeRenameRevision, NodeRenameSpec, pool_catalog_path,
};

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
    let mut context = super::active_pools::context(&owner, 71, 12);
    // Allow for simulator scheduling within the forwarding and client's
    // 15-second budget. The volume generation below checks commit count.
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(12);
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
    let generation = || {
        rt.block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 12),
        )
        .unwrap()
        .unwrap()
        .index
        .generation
    };
    let before_generation = generation();
    let put=entry.request_body("PUT",&path,Some("Content-Length: 3\r\nContent-MD5: kAFQmDzST7DWlj99KOF/cg==\r\nx-amz-sdk-checksum-algorithm: CRC32\r\nx-amz-checksum-crc32: NSRBwg==\r\nContent-Type: text/plain\r\nCache-Control: max-age=60\r\nx-amz-meta-origin: s3-test\r\nIf-None-Match: *\r\n"),b"abc");
    assert_eq!(put.0, 200, "{}", String::from_utf8_lossy(&put.2));
    assert_eq!(
        generation(),
        before_generation + 1,
        "the first nested PUT must commit its parents and file in one LTFS generation"
    );
    assert!(writes() > before);
    assert!(put.1.contains("x-amz-checksum-crc32: nsrbwg=="));
    let listed = entry.request("GET", &format!("/{bucket}?list-type=2&prefix=new/"), None);
    assert_eq!(listed.0, 200);
    assert!(String::from_utf8_lossy(&listed.2).contains("<Key>new/dir/object</Key>"));
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

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；普通 S3 隐含目录提交后接管，仅访问模拟磁带"]
fn s3_nested_put_recovers_unobserved_s4_without_publishing_partial_paths() {
    interrupted_nested_put(NestedPutInterruption::Unobserved);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；验证 FDB 已观察但未发布时的接管，仅使用模拟磁带"]
fn s3_nested_put_recovers_observed_but_unpublished_s4() {
    interrupted_nested_put(NestedPutInterruption::ObservedUnpublished);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；普通 S3 错误后接管并释放原目录预约，仅访问模拟磁带"]
fn s3_nested_put_aborts_unwritten_attempt_and_releases_parent_path() {
    interrupted_nested_put(NestedPutInterruption::Unwritten);
}

enum NestedPutInterruption {
    Unwritten,
    Unobserved,
    ObservedUnpublished,
}

fn interrupted_nested_put(interruption: NestedPutInterruption) {
    let committed_to_tape = !matches!(interruption, NestedPutInterruption::Unwritten);
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let path = format!("/{bucket}/interrupted/parent/one");
    let sibling = format!("/{bucket}/interrupted/parent/two");
    let body = b"nested S3 original body";
    let start = |owner: &Owner, epoch| {
        let mut context = super::active_pools::context(owner, 71, epoch);
        Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(12);
        start_writable_s3(&f, context)
    };
    let resolve = |path: &str| {
        rt.block_on(
            f.catalog
                .resolve_active_path(&f.serial, format!("/{}{path}", f.pools[0]).as_bytes()),
        )
        .unwrap()
    };
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let old = rt
        .block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 12),
        )
        .unwrap()
        .unwrap();
    let server = start(&owner, 12);
    if committed_to_tape {
        owner.fail_vci.store(true, Ordering::SeqCst);
    } else {
        owner.fail_write.store(true, Ordering::SeqCst);
    }
    let response = server.request_body(
        "PUT",
        &path,
        Some(&format!("Content-Length: {}\r\n", body.len())),
        body,
    );
    let error = String::from_utf8_lossy(&response.2);
    assert_eq!(response.0, 503, "{error}");
    assert!(!owner.fail_vci.load(Ordering::SeqCst));
    let created_paths = [
        "/interrupted",
        "/interrupted/parent",
        "/interrupted/parent/one",
    ];
    for path in created_paths {
        assert!(resolve(path).is_none(), "{path} must remain invisible");
    }
    assert_eq!(
        rt.block_on(
            f.catalog
                .observed_active_volume_index(&f.serial, old.volume_id)
        )
        .unwrap(),
        old.index
    );
    assert_eq!(put_small(&server, &sibling, b"pending").0, 503);
    assert!(resolve("/interrupted/parent/two").is_none());
    drop(server);
    drop(owner);
    f.library.power_cut();

    let observed_request = if matches!(interruption, NestedPutInterruption::ObservedUnpublished) {
        // Recover with the original epoch, stopping after the volume index is
        // observed in FDB but before publishing the batch.
        let slot = (0..f.library.drive_count())
            .find(|&slot| f.library.loaded_barcode(slot).as_deref() == Some(BARCODES[0]))
            .expect("the simulated tape must remain in its drive");
        let drive = f.library.drive_as(slot, 71);
        let key = ReservationKey::new(71, 12);
        fence(&drive, key).unwrap();
        TapeDrive::new(&drive).load().unwrap();
        let mut mounted = LtfsVolume::mount(&drive).unwrap();
        mounted.set_reservation_guard(Some(key));
        let recovered = rt
            .block_on(recover_sealed_active_media_batch(
                &f.catalog, mounted, &f.serial, 12,
            ))
            .unwrap();
        let index = recovered.current_index();
        assert_eq!(index.generation, old.index.generation + 1);
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, old.volume_id)
            )
            .unwrap(),
            index
        );
        let batch = rt
            .block_on(
                f.catalog
                    .active_batch_status(&f.serial, old.volume_id, index.generation),
            )
            .unwrap()
            .unwrap();
        assert!(!batch.published);
        let sealed = rt
            .block_on(f.catalog.sealed_active_batch_page(
                &f.serial,
                12,
                old.volume_id,
                index.generation,
                batch.batch_id,
                0,
            ))
            .unwrap();
        let [(_, intent)] = sealed.files.as_slice() else {
            panic!("expected a sealed batch with exactly one file");
        };
        let request = intent.request();
        assert!(error.contains(&request.operation_id.to_string()), "{error}");
        assert_eq!(
            rt.block_on(f.catalog.active_upload_result(&request, f.route))
                .unwrap(),
            None
        );
        for path in created_paths {
            assert!(
                resolve(path).is_none(),
                "{path} is visible before publication"
            );
        }
        Some(request)
    } else {
        None
    };

    let successor = Owner::start_mode(&f, 13, json!({}), true);
    if let Some(request) = observed_request {
        assert_eq!(
            rt.block_on(f.catalog.active_upload_result(&request, f.route))
                .unwrap(),
            Some(PublishOutcome::AlreadyApplied(1))
        );
    }
    let server = start(&successor, 13);
    let recovered_generation = rt
        .block_on(
            f.catalog
                .observed_active_volume_index(&f.serial, old.volume_id),
        )
        .unwrap()
        .generation;
    if committed_to_tape {
        assert_eq!(recovered_generation, old.index.generation + 1);
    } else {
        assert!(resolve("/interrupted").is_none());
        assert_eq!(put_small(&server, &sibling, b"new").0, 200);
        let response = server.request_body(
            "PUT",
            &path,
            Some(&format!("Content-Length: {}\r\n", body.len())),
            body,
        );
        assert_eq!(response.0, 200, "{}", String::from_utf8_lossy(&response.2));
        assert_eq!(
            rt.block_on(
                f.catalog
                    .observed_active_volume_index(&f.serial, old.volume_id)
            )
            .unwrap()
            .generation,
            recovered_generation + 2,
        );
    }
    if committed_to_tape {
        assert_eq!(put_small(&server, &sibling, b"new").0, 200);
    }
    assert_eq!(server.request("GET", &sibling, None).2, b"new");
    for path in ["/interrupted", "/interrupted/parent"] {
        assert!(matches!(
            resolve(path),
            Some(ResolvedPath::Directory { .. })
        ));
    }
    let get = server.request("GET", &path, None);
    assert_eq!(get.0, 200);
    assert_eq!(get.2, body);
    assert_body_appended_once(&f.library, body);
    drop(server);
    drop(successor);
    fs::remove_dir_all(&f.root).unwrap();
}

fn assert_body_appended_once(library: &SimLibrary, body: &[u8]) {
    let mut copies = 0;
    assert!(library.with_cartridge_mut(BARCODES[0], |cartridge| {
        copies = cartridge
            .partitions
            .iter()
            .flat_map(|p| &p.objects)
            .filter(|object| matches!(object, LogicalObject::Record(record) if record == body))
            .count();
    }));
    assert_eq!(copies, 1, "the original body must be appended exactly once");
}

fn start_writable_s3(
    f: &Fixture,
    context: Arc<tape_rs::daemon::http::HttpContext>,
) -> ReadHttpServer {
    ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ))
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；普通 S3 并发创建隐含目录，仅访问模拟磁带"]
fn s3_concurrent_first_puts_reuse_the_published_prefix() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(12);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let barrier = std::sync::Barrier::new(2);
    let paths = ["/shared/parents/one", "/shared/parents/two"];
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .iter()
            .map(|path| {
                let (server, bucket, barrier) = (&server, &bucket, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    server.request_body(
                        "PUT",
                        &format!("/{bucket}{path}"),
                        Some(&format!(
                            "Content-Length: {}\r\nIf-None-Match: *\r\n",
                            path.len()
                        )),
                        path.as_bytes(),
                    )
                })
            })
            .collect();
        for (path, handle) in paths.iter().zip(handles) {
            let response = handle.join().unwrap();
            assert_eq!(
                response.0,
                200,
                "{path}: {}",
                String::from_utf8_lossy(&response.2)
            );
        }
    });
    for path in paths {
        let response = server.request("GET", &format!("/{bucket}{path}"), None);
        assert_eq!((response.0, response.2.as_slice()), (200, path.as_bytes()));
    }
    let listing = server.request(
        "GET",
        &format!("/{bucket}?list-type=2&prefix=shared/"),
        None,
    );
    assert_eq!(listing.0, 200);
    assert!(String::from_utf8_lossy(&listing.2).contains("<Key>shared/parents/one</Key>"));
    assert!(String::from_utf8_lossy(&listing.2).contains("<Key>shared/parents/two</Key>"));

    let path = format!("/{bucket}/another/conditional");
    let paused = pause_put(&server, &path);
    let winner = put_small(&server, &path, b"a");
    assert_eq!(winner.0, 200, "{}", String::from_utf8_lossy(&winner.2));
    assert_eq!(complete_put(paused, b'b'), 412);
    assert_eq!(server.request("GET", &path, None).2, b"a");
    assert_eq!(put_small(&server, &path, b"b").0, 412);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn put_small(server: &ReadHttpServer, path: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    server.request_body(
        "PUT",
        path,
        Some(&format!(
            "Content-Length: {}\r\nIf-None-Match: *\r\n",
            body.len()
        )),
        body,
    )
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；普通 S3 不同前缀并发首写，仅访问模拟磁带"]
fn s3_concurrent_first_puts_to_different_prefixes_both_commit() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::active_pools::context(&owner, 71, 12);
    let config = Arc::get_mut(&mut context).unwrap();
    config.wait_timeout = Duration::from_secs(30);
    let (bridge, requests) = channel::<ExecRequest>();
    let forward = std::mem::replace(config.exec.get_mut().unwrap(), bridge);
    let actor = forward.clone();
    let (held, writes) = channel();
    let proxy = std::thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            if matches!(request, ExecRequest::FdbWrite { .. }) {
                held.send(request).unwrap();
            } else {
                actor.send(request).unwrap();
            }
        }
    });
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let first_path = format!("/{bucket}/new-first/file");
    let second_path = format!("/{bucket}/new-second/file");
    let responses = std::thread::scope(|scope| {
        let first = scope.spawn(|| put_small(&server, &first_path, b"a"));
        let first_write = writes.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = scope.spawn(|| put_small(&server, &second_path, b"b"));
        // Both PUTs have finished preparation before either can reserve tape.
        let second_write = writes.recv_timeout(Duration::from_secs(5)).unwrap();
        forward.send(first_write).unwrap();
        let first_response = first.join().unwrap();
        forward.send(second_write).unwrap();
        (first_response, second.join().unwrap())
    });
    for (response, path, body) in [
        (&responses.0, &first_path, b"a"),
        (&responses.1, &second_path, b"b"),
    ] {
        assert_eq!(
            response.0,
            200,
            "{path}: {}",
            String::from_utf8_lossy(&response.2)
        );
        let read = server.request("GET", path, None);
        assert_eq!((read.0, read.2.as_slice()), (200, body.as_slice()));
    }
    drop(server);
    proxy.join().unwrap();
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn pause_put(server: &ReadHttpServer, path: &str) -> BufReader<std::net::TcpStream> {
    let stream = std::net::TcpStream::connect(&server.address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut stream = BufReader::new(stream);
    write!(
        stream.get_mut(),
        "PUT {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 1\r\nIf-None-Match: *\r\nExpect: 100-continue\r\n\r\n"
    ).unwrap();
    let mut status = String::new();
    stream.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 100 "), "{status}");
    stream.read_line(&mut String::new()).unwrap();
    stream
}

fn complete_put(mut stream: BufReader<std::net::TcpStream>, body: u8) -> u16 {
    stream.get_mut().write_all(&[body]).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 "), "{response}");
    response.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；验证 S3 正文等待期间的目录身份，仅使用模拟磁带"]
fn s3_waiting_first_put_preserves_shared_directory_identity() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(30);
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let directory = |path: &str| {
        let path = pool_catalog_path(f.pools[0], path.as_bytes()).unwrap();
        match rt
            .block_on(
                f.catalog
                    .resolve_active_metadata_for_route(&f.serial, &path, f.route),
            )
            .unwrap()
        {
            Some(ResolvedPath::Directory { id, version, .. }) => (id, version),
            other => panic!("expected a directory: {other:?}"),
        }
    };
    let mutate = |path, version, mutation| {
        let task = rt.block_on(super::nodes::queue(&f, 12, path, version, mutation));
        assert_eq!(
            super::nodes::terminal(&f, &rt, &task).state,
            NodeMutationTaskState::Completed
        );
    };

    let path = format!("/{bucket}/mkdir/child");
    let paused = pause_put(&server, &path);
    mutate("/mkdir", None, NodeMutation::CreateDirectory);
    let original = directory("/mkdir").0;
    assert_eq!(complete_put(paused, b'm'), 200);
    assert_eq!(directory("/mkdir").0, original);
    assert_eq!(server.request("GET", &path, None).2, b"m");

    mutate("/rename", None, NodeMutation::CreateDirectory);
    let path = format!("/{bucket}/rename/new/child");
    let paused = pause_put(&server, &path);
    let revision = |path| {
        let (inode, version) = directory(path);
        NodeRenameRevision {
            namespace: f.route.route.namespace,
            inode,
            version,
        }
    };
    let source = revision("/rename");
    let root = revision("/");
    let task = rt.block_on(async {
        let spec = NodeRenameSpec {
            pool: f.pools[0],
            operation: Uuid::new_v4(),
            from: "/rename".into(),
            to: "/moved".into(),
            source,
            source_parent: root,
            destination_parent: root,
            target: None,
            no_replace: true,
        };
        let request = f
            .catalog
            .prepare_node_rename_request(
                &f.serial,
                f.route,
                12,
                &spec,
                "2026-11-01T00:40:00.000000000Z",
            )
            .await
            .unwrap();
        f.catalog
            .queue_node_rename_request(f.route, 12, &request, 4 << 20)
            .await
            .unwrap()
    });
    assert_eq!(
        super::nodes::terminal(&f, &rt, &task).state,
        NodeMutationTaskState::Completed
    );
    assert_eq!(directory("/moved").0, source.inode);
    mutate("/rename", None, NodeMutation::CreateDirectory);
    assert_eq!(complete_put(paused, b'n'), 409);
    assert_eq!(server.request("GET", &path, None).0, 404);

    let path = format!("/{bucket}/rebuild/follower");
    let leader = format!("/{bucket}/rebuild/leader");
    let paused = pause_put(&server, &path);
    assert_eq!(put_small(&server, &leader, b"l").0, 200);
    assert_eq!(server.request("DELETE", &leader, None).0, 204);
    let (original, version) = directory("/rebuild");
    mutate("/rebuild", Some(version), NodeMutation::RemoveDirectory);
    mutate("/rebuild", None, NodeMutation::CreateDirectory);
    assert_ne!(directory("/rebuild").0, original);
    assert_eq!(complete_put(paused, b'f'), 409);
    assert_eq!(server.request("GET", &path, None).0, 404);
    assert_eq!(put_small(&server, &path, b"r").0, 200);
    assert_eq!(server.request("GET", &path, None).2, b"r");

    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；验证父路径类型冲突，仅使用模拟磁带"]
fn s3_waiting_put_rejects_file_and_symlink_parents() {
    use tape_rs::scsi::cdb::opcode::WRITE_6;

    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::active_pools::context(&owner, 71, 12);
    Arc::get_mut(&mut context).unwrap().wait_timeout = Duration::from_secs(30);
    let server = start_writable_s3(&f, context);
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let file_parent = format!("/{bucket}/file-parent");
    let target_file = format!("/{bucket}/target/existing");
    assert_eq!(put_small(&server, &target_file, b"target").0, 200);
    let file_put = pause_put(&server, &format!("{file_parent}/child"));
    let link_put = pause_put(&server, &format!("/{bucket}/link-parent/child"));
    assert_eq!(put_small(&server, &file_parent, b"original").0, 200);
    let symlink = rt.block_on(super::nodes::queue(
        &f,
        12,
        "/link-parent",
        None,
        NodeMutation::CreateSymlink {
            target: "/target".into(),
        },
    ));
    assert_eq!(
        super::nodes::terminal(&f, &rt, &symlink).state,
        NodeMutationTaskState::Completed
    );
    let index = || {
        rt.block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 12),
        )
        .unwrap()
        .unwrap()
        .index
    };
    let writes = || {
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command[0] == WRITE_6)
            .count()
    };
    let before = (index(), writes());
    assert_eq!(complete_put(file_put, b'f'), 409);
    assert_eq!(complete_put(link_put, b'l'), 409);
    assert_eq!((index(), writes()), before);
    let listing = server.request("GET", &format!("/{bucket}?list-type=2"), None);
    assert_eq!(listing.0, 200);
    let listing = String::from_utf8_lossy(&listing.2);
    for key in ["file-parent/child", "link-parent/child", "target/child"] {
        assert!(!listing.contains(&format!("<Key>{key}</Key>")));
    }
    for (path, body) in [
        (file_parent, b"original".as_slice()),
        (target_file, b"target"),
    ] {
        let read = server.request("GET", &path, None);
        assert_eq!((read.0, read.2.as_slice()), (200, body));
    }
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；注入明确未执行的设备线程拒绝，仅使用模拟磁带"]
fn s3_shared_parent_wait_is_released_after_known_failure_and_is_pool_local() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let mut context = super::active_pools::context(&owner, 71, 12);
    let config = Arc::get_mut(&mut context).unwrap();
    config.wait_timeout = Duration::from_secs(12);
    let (bridge, requests) = channel();
    let forward = std::mem::replace(config.exec.get_mut().unwrap(), bridge);
    let (held, admitted) = channel();
    let proxy = std::thread::spawn(move || {
        let mut reject = true;
        while let Ok(request) = requests.recv() {
            match request {
                ExecRequest::FdbWrite { intent, reply, .. }
                    if reject && intent.name == b"first" =>
                {
                    reject = false;
                    held.send((intent, reply)).unwrap();
                }
                request => forward.send(request).unwrap(),
            }
        }
    });
    let server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), context.clone())]),
        context,
        HashMap::new(),
        true,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let unrelated = s3::bucket_name(&f.serial, &f.pools[1].to_string());
    let first = format!("/{bucket}/shared/first");
    let second = format!("/{bucket}/shared/second");
    let rejected = std::thread::scope(|scope| {
        let leader = scope.spawn(|| put_small(&server, &first, b"a"));
        let (request, reject) = admitted.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut paused = pause_put(&server, &second);
        paused.get_mut().write_all(b"b").unwrap();
        let independent = put_small(&server, &format!("/{unrelated}/shared/outside"), b"u");
        assert_eq!(
            independent.0,
            200,
            "{}",
            String::from_utf8_lossy(&independent.2)
        );
        assert_eq!(server.request("GET", &second, None).0, 404);
        reject
            .send(Err(ReadError::Failed("内核准入前的明确拒绝".into())))
            .unwrap();
        let rejected = leader.join().unwrap();
        assert_eq!(rejected.0, 503);
        assert!(String::from_utf8_lossy(&rejected.2).contains("写带前已拒绝"));
        let mut status = String::new();
        paused.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 200 "), "{status}");
        request
    });
    assert_eq!(server.request("GET", &second, None).2, b"b");
    let spec = tape_rs::managed_catalog::ActiveUploadSpec {
        pool_id: f.pools[0],
        path: b"/shared/first".to_vec(),
        operation_id: rejected.operation_id,
        expected_version: rejected.expected_version,
        size: rejected.expected_size,
        digest: rejected.expected_digest,
    };
    rt.block_on(
        f.catalog
            .prepare_active_upload(&f.serial, f.route, 12, BARCODES[0], &spec),
    )
    .expect("未取得物理预约的明确拒绝应清理私有目录计划");
    assert_eq!(put_small(&server, &first, b"a").0, 200);
    assert_eq!(server.request("GET", &first, None).2, b"a");
    drop(server);
    proxy.join().unwrap();
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；验证发布后丢失写入回执，仅使用模拟磁带"]
fn s3_nested_put_original_receipt_survives_lost_reply() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let generation = |epoch| {
        rt.block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, epoch),
        )
        .unwrap()
        .unwrap()
        .index
        .generation
    };
    let before = generation(12);
    let mut context = super::active_pools::context(&owner, 71, 12);
    let config = Arc::get_mut(&mut context).unwrap();
    config.wait_timeout = Duration::from_secs(12);
    let (bridge, requests) = channel();
    let actor = std::mem::replace(config.exec.get_mut().unwrap(), bridge);
    let (committed, original) = channel();
    let proxy = std::thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            match request {
                ExecRequest::FdbWrite {
                    intent,
                    file,
                    reply,
                } => {
                    let request = (*intent).clone();
                    let (actual_reply, received) = channel();
                    actor
                        .send(ExecRequest::FdbWrite {
                            intent,
                            file,
                            reply: actual_reply,
                        })
                        .unwrap();
                    // Let the real owner finish the write and publication
                    // before disconnecting its reply to S3.
                    let version = received
                        .recv_timeout(Duration::from_secs(25))
                        .unwrap()
                        .unwrap();
                    committed.send((request, version)).unwrap();
                    drop(reply);
                }
                request => actor.send(request).unwrap(),
            }
        }
    });
    let server = start_writable_s3(&f, context);
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let path = format!("/{bucket}/receipt/parent/file");
    let body = b"original body with a deliberately unacknowledged commit";
    let response = put_small(&server, &path, body);
    let (request, version) = original.recv_timeout(Duration::from_secs(5)).unwrap();
    let error = String::from_utf8_lossy(&response.2);
    assert_eq!(response.0, 503, "{error}");
    assert!(error.contains("结果未确认"), "{error}");
    assert!(error.contains(&request.operation_id.to_string()), "{error}");
    let receipt = || {
        rt.block_on(f.catalog.active_upload_result(&request, f.route))
            .unwrap()
    };
    assert_eq!(generation(12), before + 1);
    assert_eq!(receipt(), Some(PublishOutcome::AlreadyApplied(version)));
    drop(server);
    proxy.join().unwrap();
    drop(owner);
    f.library.power_cut();

    let successor = Owner::start_mode(&f, 13, json!({}), true);
    let server = start_writable_s3(&f, super::active_pools::context(&successor, 71, 13));
    let read = server.request("GET", &path, None);
    assert_eq!((read.0, read.2.as_slice()), (200, body.as_slice()));
    for dir in ["/receipt", "/receipt/parent"] {
        let path = pool_catalog_path(f.pools[0], dir.as_bytes()).unwrap();
        assert!(matches!(
            rt.block_on(f.catalog.resolve_active_path(&f.serial, &path))
                .unwrap(),
            Some(ResolvedPath::Directory { .. })
        ));
    }
    assert_eq!(receipt(), Some(PublishOutcome::AlreadyApplied(version)));
    assert_eq!(put_small(&server, &path, body).0, 412);
    assert_eq!(generation(13), before + 1);
    assert_body_appended_once(&f.library, body);
    drop(server);
    drop(successor);
    fs::remove_dir_all(f.root).unwrap();
}
