//! Native intake starts from an empty authority, without a legacy DB export.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use tape_rs::client::fdb::PreparedManagement;

fn wait_media(
    client: &FdbClient,
    path: &std::path::Path,
) -> std::result::Result<serde_json::Value, String> {
    let request = PreparedManagement::load(path).map_err(|e| e.to_string())?;
    client
        .submit_management_saved(path)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = client
            .management_status(&request)
            .map_err(|e| e.to_string())?
            .ok_or("已受理介质任务丢失")?;
        match task["status"].as_str() {
            Some("succeeded") => return Ok(task),
            Some("failed" | "cancelled") => return Err(task.to_string()),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return Err(format!("等待介质异步任务超时: {task}")),
        }
    }
}

fn assert_readonly_trace(owner: &Owner) {
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19)),
        "接入不能写数据、文件标记、MAM、格式化或擦除"
    );
}

fn check_saved_upload_during_intake(
    f: &Fixture,
    owner: &Owner,
    server: &ReadHttpServer,
    client: &FdbClient,
    upload: &std::path::Path,
    body: &std::path::Path,
    terminal: bool,
) {
    let saved = fs::read(upload.join("request.json")).unwrap();
    let request = client.prepare_management(&f.serial, f.route.route.namespace,
        json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"ABSENT0L8","volume":null,"path":"/","merge_tapes":true})).unwrap();
    let path = f.root.join(format!("busy-upload-{terminal}.json"));
    request.save_new(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let value = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    assert_eq!(
        server
            .request_body(
                "POST",
                &format!(
                    "/admin/libraries/{}/pools/{}/tapes/ABSENT0L8/import",
                    f.serial, f.pools[0]
                ),
                Some(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    value.len()
                )),
                value
            )
            .0,
        202
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.management_status(&request).unwrap().unwrap()["status"] != "running" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let trace_start = owner.trace.lock().unwrap().len();
    if !terminal {
        let record = upload_prepared(upload);
        let payload = fs::read(body).unwrap();
        let route = format!(
            "/v1/fdb/files/{}?pool={}&path={}",
            f.serial,
            record["pool"].as_str().unwrap(),
            record["path"].as_str().unwrap()
        );
        let headers = format!(
            "Content-Type: application/octet-stream\r\nContent-Length: {}\r\nx-tapers-fdb-write: {}\r\nx-tapers-fdb-namespace: {}\r\n",
            payload.len(),
            record["metadata"].as_str().unwrap(),
            record["namespace"].as_str().unwrap()
        );
        assert_eq!(
            server
                .request_body("PUT", &route, Some(&headers), &payload)
                .0,
            503
        );
    }
    let short = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(2), Duration::from_secs(1));
    let result = short.upload_saved(upload);
    assert_eq!(
        result.is_ok(),
        terminal,
        "未提交上传应等待，已成功请求应直接返回原终态: {result:?}"
    );
    assert_eq!(fs::read(upload.join("request.json")).unwrap(), saved);
    assert!(owner.permission.allows(100, Phase::Serving));
    assert!(
        !owner.trace.lock().unwrap()[trace_start..]
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0xa5 | 0x04 | 0x19)),
        "池忙或终态重试不能MOVE、追加或写介质"
    );
    assert_eq!(
        client.management_status(&request).unwrap().unwrap()["status"],
        "running"
    );
    client.cancel_management_saved(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.management_status(&request).unwrap().unwrap()["status"] != "cancelled" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟介质和本机 HTTP/CLI，不操作真实设备"]
fn native_intake_reads_existing_media_and_then_allows_upload() {
    run_native_intake(false);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟增量介质和本机 HTTP/CLI，不操作真实设备"]
fn native_incremental_intake_survives_new_writes_restart_and_predecessor_corruption() {
    run_native_intake(true);
}

fn run_native_intake(incremental: bool) {
    run_native_intake_mode(incremental, false);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟已在驱动器中但未加载的磁带，不操作真实设备"]
fn native_intake_loads_a_present_unthreaded_cartridge() {
    run_native_intake_mode(false, true);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；异步CLI默认目录、读回及后续追加，仅模拟设备"]
fn native_intake_async_default_path_reads_and_appends() {
    native_loaded_refresh_scene(false);
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；原卷刷新在驱动器中接任，仅模拟设备"]
fn native_loaded_assigned_refresh_resumes_original_preflight_after_restart() {
    native_loaded_refresh_scene(true);
}

fn native_loaded_refresh_scene(resume_running: bool) {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let barcode = "DEFAULTL8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), 0)
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    let dev = f.library.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "DFLT".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/folder/original", &mut b"default-original".as_slice())
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    fixture_return_to_storage(&f.library, 0);
    let other = "OTHER00L8";
    f.library
        .insert_cartridge(SimCartridge::blank(other, 128 << 20), 1)
        .unwrap();
    f.library.load_into_drive(other, 0).unwrap();
    mkltfs(
        &f.library.drive(0),
        &MkltfsOptions {
            volume_id: "OTHER".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    fixture_return_to_storage(&f.library, 0);
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(30), Duration::from_secs(30));
    let ns = f.route.route.namespace;
    client
        .create_pool(&f.serial, ns, f.pools[0], "default", 200_000)
        .unwrap();
    let request_file = f.root.join("default-import.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "tape",
            "import",
            barcode,
            "--request",
            request_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(envelope["requests"][0]["task_id"].is_string());
    let prepared = PreparedManagement::load(&request_file).unwrap();
    assert_eq!(prepared.value()["change"]["path"], "/IMPORT");
    assert_eq!(prepared.value()["change"]["merge_tapes"], false);
    let task = wait_media(&client, &request_file).unwrap();
    assert_eq!(task["status"], "succeeded");
    let other_request = f.root.join("other-import.json");
    client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":other,"volume":null,"path":"/","merge_tapes":true})).unwrap().save_new(&other_request).unwrap();
    wait_media(&client, &other_request).unwrap();
    assert_readonly_trace(&owner);
    let original = f.root.join("default-readback");
    let base = format!("/IMPORT/{barcode}/folder");
    client
        .download_new(
            &f.serial,
            f.pools[0],
            &format!("{base}/original"),
            &original,
        )
        .unwrap();
    assert_eq!(fs::read(original).unwrap(), b"default-original");
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/folder/original")
            .unwrap()
            .is_none()
    );
    let source = f.root.join("default-new-body");
    fs::write(&source, b"append-after-default-import").unwrap();
    let upload = f.root.join("default-upload.json");
    let parent = client.stat(&f.serial, f.pools[0], &base).unwrap().unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            &format!("{base}/new"),
            &source,
            UploadCondition::Create {
                namespace: ns,
                parent,
            },
            None,
            &upload,
        )
        .unwrap();
    client.upload_saved(&upload).unwrap();
    let readback = f.root.join("default-appended-readback");
    client
        .download_new(&f.serial, f.pools[0], &format!("{base}/new"), &readback)
        .unwrap();
    assert_eq!(fs::read(readback).unwrap(), b"append-after-default-import");
    let before = owner.trace.lock().unwrap().len();
    let resumed = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "tape",
            "import",
            "--request",
            request_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let replay: serde_json::Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(replay["task"]["operation"], prepared.value()["operation"]);
    assert!(
        !owner.trace.lock().unwrap()[before..]
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x8d | 0x04 | 0x19))
    );
    drop(server);
    drop(ctx);
    drop(owner);
    // Normal exclusive handoff to an external LTFS writer. This fixture also
    // retains an imported prefix plus a later native upload on the same tape.
    f.library.load_into_drive(barcode, 0).unwrap();
    let dev = f.library.drive(0);
    TapeDrive::new(&dev).load().unwrap();
    let mut external = LtfsVolume::mount(&dev).unwrap();
    external
        .append_file("/folder/original", &mut b"external-replacement".as_slice())
        .unwrap();
    external
        .append_file("/external/ee.bin", &mut b"external-new-content".as_slice())
        .unwrap();
    external.commit().unwrap();
    external.unmount().unwrap();
    // The external writer leaves the assigned cartridge in the drive.
    // Startup must retain management access without admitting unknown data.
    let refreshed = f.root.join("refresh-existing.json");
    if resume_running {
        rt.block_on(async {
            let request = tape_rs::managed_catalog::ManagementRequest::parse(json!({
                "version":1,"library":f.serial,"namespace":ns.to_string(),"epoch":1,
                "operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import",
                "pool":f.pools[0].to_string(),"barcode":barcode,"volume":null,
                "path":"/IMPORT","merge_tapes":false}
            }))
            .unwrap();
            f.catalog
                .admit_management(&f.serial, f.route, 100, &request)
                .await
                .unwrap();
            assert!(
                f.catalog
                    .start_native_media(&f.serial, f.route, 100, 1, request.operation())
                    .await
                    .unwrap()
                    .unwrap()
                    .assigned_volume()
                    .unwrap()
                    .is_some()
            );
            fs::write(&refreshed, request.value().to_string()).unwrap();
        });
    }
    let owner = Owner::start_mode(&f, 101, json!({}), true);
    let ctx = context(&owner, 71, 101);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(30), Duration::from_secs(30));
    if !resume_running {
        let trace_start = owner.trace.lock().unwrap().len();
        let route = format!(
            "/v1/fdb/files/{}?pool={}&path={}",
            f.serial,
            f.pools[0],
            URL_SAFE_NO_PAD.encode(format!("{base}/original"))
        );
        assert_eq!(server.request("GET", &route, None).0, 503);
        let spec = tape_rs::managed_catalog::ActiveUploadSpec {
            pool_id: f.pools[0],
            path: format!("{base}/quarantine-probe").into_bytes(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: 0,
            digest: ContentDigest::Sha256(sha2::Sha256::digest([]).into()),
        };
        for pinned in [Some(barcode.to_string()), None] {
            let (reply, rx) = channel();
            owner
                .tx
                .send(ExecRequest::FdbPrepare {
                    spec: Box::new(spec.clone()),
                    barcode: pinned.clone(),
                    reply,
                })
                .unwrap();
            let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if pinned.is_some() {
                assert!(matches!(result, Err(ReadError::Busy(_))), "{result:?}");
            } else {
                assert_eq!(
                    result.unwrap().volume_id,
                    rt.block_on(f.catalog.tape_assignment(&f.serial, other))
                        .unwrap()
                        .unwrap()
                        .volume_id
                        .unwrap()
                );
            }
        }
        assert!(
            !owner.trace.lock().unwrap()[trace_start..]
                .iter()
                .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x8d | 0x04 | 0x19))
        );
    }
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"));
    command.args(["-e", &server.address, "tape", "import"]);
    if !resume_running {
        command.args([barcode, "-l", &f.serial, "-p", &f.pools[0].to_string()]);
    }
    let output = command
        .args(["--request", refreshed.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let task = wait_media(&client, &refreshed).unwrap_or_else(|error| {
        panic!(
            "{error}; executor events: {:?}",
            owner
                .events
                .try_iter()
                .filter(|event| matches!(event, ExecEvent::Lost { .. }))
                .collect::<Vec<_>>()
        )
    });
    assert_eq!(task["result"]["refreshed"], true);
    assert_readonly_trace(&owner);
    for (path, body, name) in [
        (
            format!("{base}/original"),
            b"external-replacement".as_slice(),
            "refresh-original",
        ),
        (
            format!("{base}/new"),
            b"append-after-default-import".as_slice(),
            "refresh-native-append",
        ),
        (
            format!("/IMPORT/{barcode}/external/ee.bin"),
            b"external-new-content".as_slice(),
            "refresh-external",
        ),
    ] {
        let out = f.root.join(name);
        client
            .download_new(&f.serial, f.pools[0], &path, &out)
            .unwrap();
        assert_eq!(fs::read(out).unwrap(), body);
    }
    assert!(
        client
            .stat(
                &f.serial,
                f.pools[0],
                &format!("/IMPORT/{barcode}/IMPORT/{barcode}/folder/new")
            )
            .unwrap()
            .is_none()
    );
    let original = client
        .stat(&f.serial, f.pools[0], &format!("{base}/original"))
        .unwrap()
        .unwrap();
    let trace = owner.trace.lock().unwrap().len();
    assert_eq!(
        wait_media(&client, &refreshed).unwrap()["operation"],
        task["operation"]
    );
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], &format!("{base}/original"))
            .unwrap()
            .unwrap(),
        original
    );
    assert!(
        !owner.trace.lock().unwrap()[trace..]
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x8d | 0x04 | 0x19))
    );
    let append_body = f.root.join("after-refresh-body");
    fs::write(&append_body, b"native-append-after-external-refresh").unwrap();
    let append_request = f.root.join("after-refresh-upload");
    let parent = client.stat(&f.serial, f.pools[0], &base).unwrap().unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let append_path = format!("{base}/after-refresh");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            &append_path,
            &append_body,
            UploadCondition::Create {
                namespace: ns,
                parent,
            },
            Some(barcode),
            &append_request,
        )
        .unwrap();
    client.upload_saved(&append_request).unwrap();
    let appended = f.root.join("after-refresh-readback");
    client
        .download_new(&f.serial, f.pools[0], &append_path, &appended)
        .unwrap();
    assert_eq!(
        fs::read(appended).unwrap(),
        b"native-append-after-external-refresh"
    );
    // Rewriting this logical path can select another tape in the same pool.
    // Refreshing the first tape must retain that newer logical head, including
    // on a second refresh and after the logical file has been unlinked.
    let logical = format!("{base}/original");
    let body = f.root.join("cross-tape-newest-body");
    fs::write(&body, b"newest-copy-on-another-cartridge").unwrap();
    let saved = f.root.join("cross-tape-newest-upload");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            &logical,
            &body,
            upload_replacement(&original),
            Some(other),
            &saved,
        )
        .unwrap();
    client.upload_saved(&saved).unwrap();
    let newest = client
        .stat(&f.serial, f.pools[0], &logical)
        .unwrap()
        .unwrap();
    let refresh_again = |label: &str| {
        let p = f.root.join(label);
        client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":barcode,"volume":null,"path":"/IMPORT","merge_tapes":false})).unwrap().save_new(&p).unwrap();
        wait_media(&client, &p).unwrap()
    };
    for label in [
        "refresh-superseded-once.json",
        "refresh-superseded-twice.json",
    ] {
        assert_eq!(refresh_again(label)["result"]["refreshed"], true);
        assert_eq!(
            client
                .stat(&f.serial, f.pools[0], &logical)
                .unwrap()
                .unwrap(),
            newest
        );
        let out = f.root.join(format!("{label}.body"));
        client
            .download_new(&f.serial, f.pools[0], &logical, &out)
            .unwrap();
        assert_eq!(fs::read(out).unwrap(), b"newest-copy-on-another-cartridge");
    }
    let parent = client.stat(&f.serial, f.pools[0], &base).unwrap().unwrap();
    let version = |v: &serde_json::Value| tape_rs::client::FileVersion::Fdb {
        namespace: ns,
        inode: v["id"].as_str().unwrap().parse().unwrap(),
        version: v["version"].as_u64().unwrap(),
    };
    let deletion = f.root.join("cross-tape-unlink");
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            &logical,
            tape_rs::client::fdb::NodeCondition {
                parent: version(&parent),
                node: Some(version(&newest)),
            },
            json!({"kind":"unlink"}),
            &deletion,
        )
        .unwrap();
    client.run_node_mutation_saved(&deletion).unwrap();
    assert!(
        client
            .stat(&f.serial, f.pools[0], &logical)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        refresh_again("refresh-after-cross-tape-unlink.json")["result"]["refreshed"],
        true
    );
    assert!(
        client
            .stat(&f.serial, f.pools[0], &logical)
            .unwrap()
            .is_none()
    );
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

fn run_native_intake_mode(incremental: bool, unthreaded: bool) {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let barcode = "INTAKE0L8";
    f.library
        .insert_cartridge(SimCartridge::blank(barcode, 128 << 20), 0)
        .unwrap();
    f.library.load_into_drive(barcode, 0).unwrap();
    let dev = f.library.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "INTAKE".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file_with_xattrs(
            "/dir/original",
            &mut b"original-content".as_slice(),
            &[
                ("large-attribute", &"x".repeat(2_000_000)),
                (
                    "tapers.s3.headers",
                    r#"{"content-type":"text/plain","x-amz-meta-origin":"native-intake"}"#,
                ),
            ],
        )
        .unwrap();
    volume.commit().unwrap();
    if incremental {
        volume
            .append_file("/dir/deleted", &mut b"gone".as_slice())
            .unwrap();
        volume.commit_incremental().unwrap();
        volume.remove_file("/dir/deleted").unwrap();
        volume
            .set_node_xattr(
                "/dir/original",
                tape_rs::ltfs::index::Xattr {
                    key: "last-delta".into(),
                    value: "metadata-only-no-new-extents".into(),
                    base64: false,
                },
                false,
                false,
            )
            .unwrap();
        volume
            .append_file("/dir/delta", &mut b"delta-content".as_slice())
            .unwrap();
        volume.commit_incremental().unwrap();
    }
    let expected = ActiveIndexIdentity::from(volume.committed_index_construct());
    drop(volume);
    fixture_return_to_storage(&f.library, 0);
    // The second source collides with a committed Active upload, rather than
    // only with a file introduced by another media import.
    let conflict_barcode = "INTAKE1L8";
    f.library
        .insert_cartridge(SimCartridge::blank(conflict_barcode, 128 << 20), 1)
        .unwrap();
    f.library.load_into_drive(conflict_barcode, 0).unwrap();
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "CLASH".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let mut conflict_volume = LtfsVolume::mount(&dev).unwrap();
    conflict_volume
        .append_file("/dir/new/child", &mut b"conflict".as_slice())
        .unwrap();
    conflict_volume.commit().unwrap();
    drop(conflict_volume);
    fixture_return_to_storage(&f.library, 0);
    let rename_barcode = "INTAKE2L8";
    f.library
        .insert_cartridge(SimCartridge::blank(rename_barcode, 128 << 20), 2)
        .unwrap();
    f.library.load_into_drive(rename_barcode, 0).unwrap();
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "RENAME".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let mut rename_volume = LtfsVolume::mount(&dev).unwrap();
    rename_volume
        .append_file("/dir/new", &mut b"imported-conflict".as_slice())
        .unwrap();
    rename_volume.commit().unwrap();
    drop(rename_volume);
    fixture_return_to_storage(&f.library, 0);
    if unthreaded {
        f.library.load_into_drive(barcode, 0).unwrap();
        TapeDrive::new(&dev).unload().unwrap();
    }
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let readonly = ReadHttpServer::start(fdb_http::owner_router(
        f.catalog.clone(),
        routes.clone(),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(30), Duration::from_secs(30));
    let ns = f.route.route.namespace;
    client
        .create_pool(&f.serial, ns, f.pools[0], "native", 200_000)
        .unwrap();
    let change = json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true});
    let reader = FdbClient::new([&readonly.address]).unwrap();
    let prepared = reader
        .prepare_management(&f.serial, ns, change.clone())
        .unwrap();
    let denied = f.root.join("readonly-intake.json");
    prepared.save_new(&denied).unwrap();
    assert!(matches!(
        reader.submit_management_saved(&denied),
        Err(tape_rs::client::ClientError::Rejected { status: 405, .. })
    ));
    assert!(reader.management_status(&prepared).unwrap().is_none());
    assert_readonly_trace(&owner);
    // A definite UUID mismatch fails durably without registering or writing.
    let mut wrong_change = change.clone();
    wrong_change["volume"] = json!(Uuid::new_v4().to_string());
    let wrong = client
        .prepare_management(&f.serial, ns, wrong_change)
        .unwrap();
    let wrong_path = f.root.join("wrong-volume.json");
    wrong.save_new(&wrong_path).unwrap();
    assert!(wait_media(&client, &wrong_path).is_err());
    assert_eq!(
        client.management_status(&wrong).unwrap().unwrap()["status"],
        "failed"
    );
    assert!(
        rt.block_on(f.catalog.tape_assignment(&f.serial, barcode))
            .unwrap()
            .is_none()
    );
    assert_readonly_trace(&owner);

    let saved = f.root.join("native-intake.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "tape",
            "import",
            barcode,
            "--path",
            "/",
            "--merge-tapes",
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
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let admitted = &envelope["requests"][0]["task"];
    assert!(matches!(
        admitted["status"].as_str(),
        Some("queued" | "running" | "succeeded")
    ));
    let prepared = PreparedManagement::load(&saved).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        let task = client.management_status(&prepared).unwrap().unwrap();
        if !matches!(task["status"].as_str(), Some("queued" | "running")) {
            break task;
        }
        assert!(Instant::now() < deadline, "{task}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(result["status"], "succeeded");
    let identity = tape_rs::ltfs::recovery::IndexSourceIdentity::from_bytes(
        &serde_json::from_value::<Vec<u8>>(result["result"]["source"]["identity"].clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(ActiveIndexIdentity::from(identity.head()), expected);
    assert_eq!(
        identity.predecessors().len(),
        if incremental { 2 } else { 0 }
    );
    assert_eq!(
        result["result"]["source"]["files"],
        if incremental { 2 } else { 1 }
    );
    assert_eq!(wait_media(&client, &saved).unwrap(), result);
    assert_eq!(
        client
            .management_status(&PreparedManagement::load(&saved).unwrap())
            .unwrap()
            .unwrap(),
        result
    );
    assert_readonly_trace(&owner);
    let destination = f.root.join("downloaded");
    client
        .download_new(&f.serial, f.pools[0], "/dir/original", &destination)
        .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"original-content");
    if incremental {
        assert!(
            client
                .stat(&f.serial, f.pools[0], "/dir/deleted")
                .unwrap()
                .is_none()
        );
        let delta = f.root.join("download-delta");
        client
            .download_new(&f.serial, f.pools[0], "/dir/delta", &delta)
            .unwrap();
        assert_eq!(fs::read(delta).unwrap(), b"delta-content");
    }
    let stat = client
        .stat(&f.serial, f.pools[0], "/dir/original")
        .unwrap()
        .unwrap();
    assert_eq!(stat["size"], 16);
    let s3server = ReadHttpServer::start(s3::fdb_router(
        f.catalog.clone(),
        routes,
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        ctx.clone(),
        HashMap::new(),
        false,
    ));
    let bucket = s3::bucket_name(&f.serial, &f.pools[0].to_string());
    let response = s3server.request("HEAD", &format!("/{bucket}/dir/original"), None);
    assert_eq!(response.0, 200);
    assert!(
        response
            .1
            .to_ascii_lowercase()
            .contains("x-amz-meta-origin: native-intake")
    );
    assert_readonly_trace(&owner);
    let body = f.root.join("new-body");
    fs::write(&body, b"after-intake").unwrap();
    let upload_path = f.root.join("upload.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/dir/new",
            &body,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: Uuid::parse_str(
                    client.stat(&f.serial, f.pools[0], "/dir").unwrap().unwrap()["id"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap(),
            },
            None,
            &upload_path,
        )
        .unwrap();

    check_saved_upload_during_intake(&f, &owner, &server, &client, &upload_path, &body, false);
    client.upload_saved(&upload_path).unwrap();
    check_saved_upload_during_intake(&f, &owner, &server, &client, &upload_path, &body, true);
    let (reply, receive) = channel();
    owner
        .tx
        .send(ExecRequest::Read {
            path: format!("/{}/dir/new", f.pools[0]),
            reply,
        })
        .unwrap();
    assert_eq!(
        receive
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap(),
        b"after-intake"
    );
    let download = f.root.join("download-new");
    client
        .download_new(&f.serial, f.pools[0], "/dir/new", &download)
        .unwrap();
    assert_eq!(fs::read(download).unwrap(), b"after-intake");
    assert!(owner.trace.lock().unwrap().iter().any(|c| c[0] == 0x0a));
    let writes_before = owner
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
        .count();
    let conflict = client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":conflict_barcode,"volume":null,"path":"/","merge_tapes":true})).unwrap();
    let conflict_path = f.root.join("conflict.json");
    conflict.save_new(&conflict_path).unwrap();
    assert!(wait_media(&client, &conflict_path).is_err());
    let receipt = client.management_status(&conflict).unwrap().unwrap();
    assert_eq!(receipt["status"], "failed", "{receipt}");
    assert!(owner.permission.allows(100, Phase::Serving));
    assert!(
        rt.block_on(f.catalog.tape_assignment(&f.serial, conflict_barcode))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
            .count(),
        writes_before
    );
    let preserved = f.root.join("after-conflict");
    client
        .download_new(&f.serial, f.pools[0], "/dir/new", &preserved)
        .unwrap();
    assert_eq!(fs::read(preserved).unwrap(), b"after-intake");
    // The original comes from a normal Active upload, rather than an earlier
    // native import. Published upload targets must also participate in rename.
    let rename = client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":rename_barcode,"volume":null,"path":"/","merge_tapes":true})).unwrap();
    let rename_path = f.root.join("rename-import.json");
    rename.save_new(&rename_path).unwrap();
    assert_eq!(
        wait_media(&client, &rename_path).unwrap()["status"],
        "succeeded"
    );
    let renamed = f.root.join("renamed-import-body");
    client
        .download_new(&f.serial, f.pools[0], "/dir/new_1", &renamed)
        .unwrap();
    assert_eq!(fs::read(&renamed).unwrap(), b"imported-conflict");
    let original_after = f.root.join("original-upload-after-import");
    client
        .download_new(&f.serial, f.pools[0], "/dir/new", &original_after)
        .unwrap();
    assert_eq!(fs::read(original_after).unwrap(), b"after-intake");
    drop(s3server);
    drop(readonly);
    drop(server);
    drop(ctx);
    drop(owner);
    // Each owner has its own empty local FileService directory: restart must
    // rebuild the historical imported source cache under a newer media head.
    let resumed = Owner::start_mode(&f, 101, json!({}), true);
    let ctx = context(&resumed, 71, 101);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(30), Duration::from_secs(30));
    assert_eq!(wait_media(&client, &saved).unwrap(), result);
    let output = f.root.join("after-restart");
    client
        .download_new(&f.serial, f.pools[0], "/dir/original", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"original-content");
    if incremental {
        // This is a warm replay-cache read: a changed Full predecessor must
        // fail before exposing a download handle, despite an unchanged delta.
        f.library.with_cartridge_mut(barcode, |cartridge| {
            let previous = identity.predecessors().last().unwrap();
            assert!(!previous.incremental);
            let tape_rs::scsi::sim::LogicalObject::Record(bytes) =
                &mut cartridge.partitions[1].objects[previous.location.start_block as usize]
            else {
                panic!("前驱位置必须是实际 XML record");
            };
            // The 2 MB xattr spans records; selecting by substring could miss
            // it entirely. Mutate the recorded predecessor block explicitly.
            assert!(bytes.len() > 16);
            let offset = bytes.len() - 8;
            bytes[offset] ^= 1;
        });
        let rejected = f.root.join("must-not-escape");
        let corrupt_client = FdbClient::new([&server.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(30));
        assert!(
            corrupt_client
                .download_new(&f.serial, f.pools[0], "/dir/original", &rejected)
                .is_err()
        );
        assert!(!rejected.exists());
        assert!(
            !resumed.permission.allows(101, Phase::Serving),
            "介质身份不再可信时必须撤销 owner 许可"
        );
    }
    drop(server);
    drop(ctx);
    drop(resumed);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；等待、取消及队列公平性，仅模拟设备"]
fn pending_intake_can_be_cancelled_without_losing_owner_or_blocking_policy() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    f.library
        .insert_cartridge(SimCartridge::blank("BLANK00L8", 128 << 20), 1)
        .unwrap();
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    let ctx = context(&owner, 71, 100);
    let server = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(f.serial.clone(), ctx.clone())]),
        HashMap::new(),
        Default::default(),
    ));
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(2), Duration::from_secs(5));
    let ns = f.route.route.namespace;
    client
        .create_pool(&f.serial, ns, f.pools[0], "waiting", 200_000)
        .unwrap();
    client
        .create_pool(&f.serial, ns, f.pools[1], "unblocked", 200_000)
        .unwrap();
    // An explicitly requested cartridge absent from inventory waits without
    // moving any other medium. Submit once so the test can cancel while running.
    let request = client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"ABSENT0L8","volume":null,"path":"/","merge_tapes":true})).unwrap();
    let path = f.root.join("waiting.json");
    request.save_new(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    assert_eq!(
        server
            .request_body(
                "POST",
                &format!(
                    "/admin/libraries/{}/pools/{}/tapes/ABSENT0L8/import",
                    f.serial, f.pools[0]
                ),
                Some(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                )),
                body
            )
            .0,
        202
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if client.management_status(&request).unwrap().unwrap()["status"] == "running" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
    let (reply, receive) = channel();
    owner
        .tx
        .send(ExecRequest::Read {
            path: format!("/{}/missing", f.pools[0]),
            reply,
        })
        .unwrap();
    assert!(matches!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(ReadError::Busy(_))
    ));
    assert!(owner.permission.allows(100, Phase::Serving));
    let policy = client
        .prepare_management(
            &f.serial,
            ns,
            json!({"kind":"pool","id":f.pools[0].to_string(),"revision":1,"patch":{"file_limit":200_001}}),
        )
        .unwrap();
    let policy_path = f.root.join("policy.json");
    policy.save_new(&policy_path).unwrap();
    assert_eq!(
        client.submit_management_saved(&policy_path).unwrap()["status"],
        "queued"
    );
    // The importing pool keeps its policy frozen. A different pool must
    // still progress while the original import waits for absent media.
    let other_policy = client
        .prepare_management(
            &f.serial,
            ns,
            json!({"kind":"pool","id":f.pools[1].to_string(),"revision":1,"patch":{"file_limit":200_001}}),
        )
        .unwrap();
    let other_path = f.root.join("other-policy.json");
    other_policy.save_new(&other_path).unwrap();
    assert_eq!(
        client
            .run_management_saved(&other_path)
            .unwrap_or_else(|error| panic!(
                "{error}; executor events: {:?}",
                owner.events.try_iter().collect::<Vec<_>>()
            ))["status"],
        "succeeded"
    );
    assert_eq!(
        client.management_status(&policy).unwrap().unwrap()["status"],
        "queued"
    );
    client.cancel_management_saved(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = client.management_status(&request).unwrap().unwrap();
        if state["status"] == "cancelled" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{state}; events: {:?}",
            owner.events.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        rt.block_on(f.catalog.tape_assignment(&f.serial, "ABSENT0L8"))
            .unwrap()
            .is_none()
    );
    let done = client.run_management_saved(&policy_path).unwrap();
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["operation"], policy.value()["operation"]);
    assert_eq!(
        client.stat(&f.serial, f.pools[0], "/").unwrap().unwrap()["kind"],
        "directory"
    );
    assert!(owner.permission.allows(100, Phase::Serving));
    assert!(
        !owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x08 | 0x0a | 0x10 | 0xa5 | 0x04 | 0x19 | 0x8d))
    );
    // Loading blank media must fail only this task and never format it or
    // revoke an otherwise valid owner. The cartridge may stay in the drive.
    let blank = client.prepare_management(&f.serial,ns,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"BLANK00L8","volume":null,"path":"/","merge_tapes":true})).unwrap();
    let blank_path = f.root.join("blank.json");
    blank.save_new(&blank_path).unwrap();
    assert!(wait_media(&client, &blank_path).is_err());
    assert_eq!(
        client.management_status(&blank).unwrap().unwrap()["status"],
        "failed"
    );
    assert!(
        rt.block_on(f.catalog.tape_assignment(&f.serial, "BLANK00L8"))
            .unwrap()
            .is_none()
    );
    assert!(owner.permission.allows(100, Phase::Serving));
    assert_readonly_trace(&owner);
    assert!(owner.moves() > 0);
    drop(server);
    drop(ctx);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
