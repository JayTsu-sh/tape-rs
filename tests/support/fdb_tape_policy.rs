//! Native cartridge policy is metadata authority; only simulated media is used.
use super::*;
use sha2::Digest;
use std::collections::BTreeMap;
use tape_rs::client::fdb::FdbClient;
use tape_rs::daemon::fdb_http;
use tape_rs::managed_catalog::{ActiveUploadSpec, ContentDigest, ManagementRequest};

fn change(policy: &serde_json::Value, patch: serde_json::Value) -> serde_json::Value {
    json!({"kind":"tape","barcode":policy["barcode"],"pool":policy["pool"],"volume":policy["volume"],"revision":policy["revision"],"patch":patch})
}

async fn admit(f: &Fixture, owner: u64, change: serde_json::Value) -> ManagementRequest {
    let window = f
        .catalog
        .active_management_window(&f.serial, f.route)
        .await
        .unwrap();
    let request = ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":window.accepting,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap();
    f.catalog
        .admit_management(&f.serial, f.route, owner, &request)
        .await
        .unwrap();
    request
}

async fn apply(f: &Fixture, owner: u64, change: serde_json::Value) -> serde_json::Value {
    let request = admit(f, owner, change).await;
    f.catalog
        .apply_management(
            &f.serial,
            f.route,
            owner,
            request.epoch(),
            request.operation(),
        )
        .await
        .unwrap()
}

fn mutations(owner: &Owner) -> usize {
    owner
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|c| matches!(c[0], 0x0a | 0x10 | 0x8d | 0xa5 | 0x04 | 0x19))
        .count()
}

#[test]
#[ignore = "需要隔离FDB；预约事务条码约束，不访问真实设备"]
fn file_reservation_requires_the_original_barcode_constraint() {
    async fn catalog_rows(cluster: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let database = foundationdb::Database::new(Some(cluster)).unwrap();
        let transaction = database.create_trx().unwrap();
        let begin = b"tape-rs/catalog/22/".to_vec();
        let mut end = begin.clone();
        *end.last_mut().unwrap() += 1;
        let mut range = foundationdb::RangeOption {
            begin: foundationdb::KeySelector::first_greater_or_equal(begin),
            end: foundationdb::KeySelector::first_greater_or_equal(end),
            limit: Some(1000),
            mode: foundationdb::options::StreamingMode::WantAll,
            ..Default::default()
        };
        let mut snapshot = Vec::new();
        let mut bytes = 0;
        loop {
            let rows = transaction.get_range(&range, 1, false).await.unwrap();
            for row in rows.iter() {
                bytes += row.key().len() + row.value().len();
                assert!(snapshot.len() < 100_000 && bytes <= 128 * 1024 * 1024);
                snapshot.push((row.key().to_vec(), row.value().to_vec()));
            }
            if !rows.more() {
                return snapshot;
            }
            range.begin = foundationdb::KeySelector::first_greater_than(
                rows.last()
                    .expect("a truncated range must advance")
                    .key()
                    .to_vec(),
            );
        }
    }
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let f = fixture(&cluster).await;
        eprintln!("fdb_reservation_barcode_workspace={}", f.root.display());
        let owner = f
            .catalog
            .active_library_owner(&f.serial)
            .await
            .unwrap()
            .unwrap()
            .1;
        let spec = ActiveUploadSpec {
            pool_id: f.pools[0],
            path: b"/barcode-reservation".to_vec(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: 4,
            digest: ContentDigest::Sha256(sha2::Sha256::digest(b"body").into()),
        };
        let intent = f
            .catalog
            .prepare_active_upload(&f.serial, f.route, owner, BARCODES[0], &spec)
            .await
            .unwrap();
        let mut wrong = intent.clone();
        wrong.barcode_constraint = Some(BARCODES[1].into());
        let before_rows = catalog_rows(&cluster).await;
        let before = f
            .catalog
            .pending_active_file_reservations(intent.namespace, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(
            f.catalog.reserve_active_file(&wrong).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        assert_eq!(
            catalog_rows(&cluster).await,
            before_rows,
            "rejection must leave every catalog key unchanged"
        );
        assert_eq!(
            f.catalog
                .pending_active_file_reservations(intent.namespace, None, 100)
                .await
                .unwrap()
                .items,
            before
        );
        assert_eq!(
            f.catalog.reserve_active_file(&intent).await.unwrap(),
            PublishOutcome::Registered
        );
        let registered_rows = catalog_rows(&cluster).await;
        assert_eq!(
            f.catalog.reserve_active_file(&wrong).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        assert_eq!(catalog_rows(&cluster).await, registered_rows);
        assert_eq!(
            f.catalog.reserve_active_file(&intent).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            f.catalog
                .cancel_active_file_reservation(&intent)
                .await
                .unwrap(),
            PublishOutcome::RequestCancelled
        );
        let mut automatic = intent.clone();
        automatic.operation_id = Uuid::new_v4();
        automatic.file_id = Uuid::new_v4();
        automatic.name = b"automatic-reservation".to_vec();
        automatic.barcode_constraint = None;
        assert_eq!(
            f.catalog.reserve_active_file(&automatic).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            f.catalog
                .cancel_active_file_reservation(&automatic)
                .await
                .unwrap(),
            PublishOutcome::RequestCancelled
        );
        fs::remove_dir_all(&f.root).unwrap();
    });
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；策略事务、模拟介质写入/修尾守护，不操作真实设备"]
fn tape_policy_is_atomic_fenced_scoped_and_guards_media_primitives() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let f = fixture(&cluster).await;
        let owner = f.catalog.active_library_owner(&f.serial).await.unwrap().unwrap().1;
        let original = f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap();
        assert_eq!(original["revision"],1);
        assert_eq!(original["online"],true);
        assert_eq!(original["blacklisted"],false);
        let cancel = admit(&f,owner,change(&original,json!({"online":false}))).await;
        let cancelled = f.catalog.cancel_management(&f.serial,f.route,owner,cancel.epoch(),cancel.operation()).await.unwrap();
        assert_eq!(cancelled["status"],"cancelled");
        assert_eq!(f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap(),original);
        let request = admit(&f,owner,change(&original,json!({"online":false}))).await;
        assert!(f.catalog.apply_management(&f.serial,f.route,owner-1,request.epoch(),request.operation()).await.is_err());
        let completed = f.catalog.apply_management(&f.serial,f.route,owner,request.epoch(),request.operation()).await.unwrap();
        assert_eq!(completed["status"],"succeeded");
        assert_eq!(completed["result"]["tape"]["revision"],2);
        assert_eq!(f.catalog.admit_management(&f.serial,f.route,owner,&request).await.unwrap(),completed);
        assert_eq!(f.catalog.apply_management(&f.serial,f.route,owner,request.epoch(),request.operation()).await.unwrap(),completed);
        let offline = f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap();
        assert_eq!(offline["online"],false);
        for (field, value) in [("volume",json!(Uuid::new_v4().to_string())),("pool",json!(f.pools[1].to_string())),("revision",json!(1))] {
            let mut invalid = change(&offline,json!({"blacklisted":true}));invalid[field] = value;
            assert_eq!(apply(&f,owner,invalid).await["status"],"failed");
            assert_eq!(f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap(),offline);
        }
        let peer = fixture(&cluster).await;
        let peer_before = peer.catalog.active_resource_policy(&peer.serial,peer.route,"tape",BARCODES[0]).await.unwrap();
        let mut wrong = request.value().clone();
        wrong["namespace"] = json!(peer.route.route.namespace.to_string());
        wrong["operation"] = json!(Uuid::new_v4().to_string());
        let wrong = ManagementRequest::parse(wrong).unwrap();
        assert!(f.catalog.admit_management(&f.serial,f.route,owner,&wrong).await.is_err());
        assert_eq!(peer.catalog.active_resource_policy(&peer.serial,peer.route,"tape",BARCODES[0]).await.unwrap(),peer_before);

        let queued = admit(&f,owner,change(&offline,json!({"blacklisted":true}))).await;
        let next_owner = owner + 1;
        f.catalog.claim_active_owner_epoch(&f.serial,next_owner).await.unwrap();
        assert!(f.catalog.apply_management(&f.serial,f.route,owner,queued.epoch(),queued.operation()).await.is_err());
        let next = f.catalog.apply_management(&f.serial,f.route,next_owner,queued.epoch(),queued.operation()).await.unwrap();
        assert_eq!(next["result"]["tape"]["revision"],3);
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(reopened.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap()["blacklisted"],true);
        assert_eq!(reopened.management_task(&f.serial,f.route,request.epoch(),request.operation()).await.unwrap().unwrap(),completed);
        let snapshot = reopened.active_management_policy(&f.serial,f.route,next_owner,&[],&[],&[BARCODES[0].into()]).await.unwrap();
        assert_eq!(snapshot["tapes"][BARCODES[0]]["blacklisted"],true);
        assert!(reopened.active_management_policy(&f.serial,f.route,next_owner,&[],&[],&vec![BARCODES[0].into();66]).await.is_err());

        // A pending intake reservation cannot receive an online/blacklist edit.
        let intake_request = admit(&f,next_owner,json!({"kind":"media-import","pool":f.pools[0].to_string(),"barcode":"WAIT001L8","volume":null,"path":"/","merge_tapes":true})).await;
        let intake = f.catalog.start_native_media(&f.serial,f.route,next_owner,intake_request.epoch(),intake_request.operation()).await.unwrap().unwrap();
        assert!(f.catalog.active_resource_policy(&f.serial,f.route,"tape","WAIT001L8").await.is_err());
        assert_eq!(apply(&f,next_owner,json!({"kind":"tape","barcode":"WAIT001L8","pool":f.pools[0].to_string(),"volume":Uuid::new_v4().to_string(),"revision":1,"patch":{"online":false}})).await["status"],"failed");
        f.catalog.cancel_management(&f.serial,f.route,next_owner,intake_request.epoch(),intake_request.operation()).await.unwrap();
        assert!(f.catalog.rollback_native_media_page(&intake,next_owner).await.unwrap());

        let blacklisted = f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap();
        assert_eq!(apply(&f,next_owner,change(&blacklisted,json!({"blacklisted":false}))).await["status"],"succeeded");
        let online_owner = Owner::start_mode(&f, next_owner, json!({}), true);
        online_tape_async(&f, next_owner, BARCODES[0]).await;
        drop(online_owner);
        let live = f.catalog.active_resource_policy(&f.serial,f.route,"tape",BARCODES[0]).await.unwrap();
        let body = b"primitive-policy-body";
        let spec = ActiveUploadSpec { pool_id:f.pools[0],path:b"/primitive-policy".to_vec(),operation_id:Uuid::new_v4(),expected_version:None,size:body.len() as u64,digest:ContentDigest::Sha256(sha2::Sha256::digest(body).into()) };
        let intent = f.catalog.prepare_active_upload(&f.serial,f.route,next_owner,BARCODES[0],&spec).await.unwrap();
        let spool = f.root.join("primitive-spool");fs::write(&spool,body).unwrap();
        f.library.load_into_drive(BARCODES[0],0).unwrap();
        let permission = Arc::new(Permission::default());
        permission.renew(next_owner,Phase::Fencing,Instant::now()+Duration::from_secs(120));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let provider = Provider {library:f.library.clone(),serial:f.serial.clone(),trace:trace.clone(),permission:permission.clone(),fail_vci:Arc::new(AtomicBool::new(false)),fail_read:Arc::new(AtomicBool::new(false))};
        let devices = provider.open_all().unwrap().into_iter().map(|mut device| {
            device.dev = Box::new(tape_rs::daemon::permission::GuardedTransport::new(device.dev,permission.clone(),next_owner));
            device
        }).collect::<Vec<_>>();
        let device = devices.iter().find(|device| device.serial=="SIMDRV0000").unwrap().dev.as_ref();
        let key = tape_rs::scsi::reservation::ReservationKey::new(71,next_owner);
        tape_rs::scsi::reservation::fence(device,key).unwrap();
        tape_rs::scsi::reservation::verify_holder(device,key).unwrap();
        permission.renew(next_owner,Phase::Execution,Instant::now()+Duration::from_secs(120));
        let volume = LtfsVolume::mount(device).unwrap();
        let recovered = tape_rs::daemon::fdb_media::establish_active_media_owner(&f.catalog,volume,&f.serial,key,&permission, 4 << 20).await.unwrap();
        assert_eq!(apply(&f,next_owner,change(&live,json!({"online":false}))).await["status"],"succeeded");
        let before = trace.lock().unwrap().iter().filter(|c| matches!(c[0],0x0a|0x10|0x8d|0xa5|0x04|0x19)).count();
        let error = tape_rs::daemon::fdb_media::commit_active_media_batch(&f.catalog,recovered.into_volume(),Uuid::new_v4(),&[tape_rs::daemon::fdb_media::ActiveMediaUpload{intent:intent.clone(),spool}], 64 << 20).await.err().expect("离线不能通过直接写入原语绕过选择器");
        assert!(matches!(error,tape_rs::error::TapeError::MediaUnavailable{..}),"{error}");
        assert_eq!(trace.lock().unwrap().iter().filter(|c| matches!(c[0],0x0a|0x10|0x8d|0xa5|0x04|0x19)).count(),before);
        assert!(f.catalog.active_upload_result(&intent.request(),f.route).await.unwrap().is_none());
        f.library.with_cartridge_mut(BARCODES[0],|cartridge| {
            let dp=&mut cartridge.partitions[1];
            dp.objects.push(tape_rs::scsi::sim::LogicalObject::Record(b"orphaned-data".to_vec()));
            dp.flushed=dp.objects.len();
        });
        let mut dirty = LtfsVolume::mount(device).unwrap();
        dirty.set_reservation_guard(Some(key));
        let error = tape_rs::daemon::fdb_media::recover_active_media_tail(&f.catalog,dirty,&f.serial,next_owner).await.err().expect("离线不能自动写索引修尾");
        assert!(matches!(error,tape_rs::error::TapeError::MediaUnavailable{..}),"{error}");
        assert_eq!(trace.lock().unwrap().iter().filter(|c| matches!(c[0],0x0a|0x10|0x8d|0xa5|0x04|0x19)).count(),before);
        // A dirty offline tape must stay offline. Verify refusal leaves no
        // recovery hold or new reservation rather than bypassing tape online.
        assert!(matches!(f.catalog.prepare_active_upload(&f.serial,f.route,next_owner,BARCODES[0],&spec).await,
            Err(tape_rs::managed_catalog::CatalogError::MediaUnavailable)));
        assert!(f.catalog.pending_active_file_reservations(f.route.route.namespace,None,100).await.unwrap().items.is_empty());
        assert!(f.catalog.active_upload_result(&intent.request(),f.route).await.unwrap().is_none());
        fs::remove_dir_all(peer.root).unwrap();
        fs::remove_dir_all(f.root).unwrap();
    });
}

fn cli(server: &ReadHttpServer, f: &Fixture, arguments: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args(["-e", &server.address, "--library", &f.serial, "fdb", "tape"])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；默认客户端、两台模拟驱动器和本机HTTP，不操作真实设备"]
fn native_tape_policy_http_cli_blocks_and_resumes_the_same_upload() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut f = rt.block_on(fixture(&cluster));
    let poison = json!({"tapes":BARCODES.iter().map(|barcode|((*barcode).to_string(),json!({"offline":"poison","blacklist":"poison"}))).collect::<serde_json::Map<_,_>>()});
    let owner = Owner::start_mode(&f, 100, poison.clone(), true);
    let mut ctx = active_pools::context(&owner, 71, 100);
    Arc::get_mut(&mut ctx).unwrap().wait_timeout = Duration::from_secs(30);
    let routes = BTreeMap::from([(f.serial.clone(), f.route)]);
    let server = ReadHttpServer::start(
        fdb_http::writer_router(
            f.catalog.clone(),
            routes.clone(),
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
            Default::default(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let readonly = ReadHttpServer::start(
        fdb_http::owner_router(
            f.catalog.clone(),
            routes,
            BTreeMap::from([(f.serial.clone(), ctx.clone())]),
            HashMap::new(),
        )
        .expect("FDB HTTP worker startup"),
    );
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(30));
    let reader = FdbClient::new([&readonly.address]).unwrap();
    let ns = f.route.route.namespace;
    let initial = reader
        .management_policy(&f.serial, "tape", BARCODES[0])
        .unwrap();
    assert_eq!(initial["policy"]["online"], true);
    let denied = reader
        .prepare_control(
            &f.serial,
            ns,
            change(&initial["policy"], json!({"online":false})),
        )
        .unwrap();
    let denied_path = f.root.join("readonly-policy.json");
    denied.save_new(&denied_path).unwrap();
    assert!(matches!(
        reader.run_management_saved(&denied_path),
        Err(tape_rs::client::ClientError::Rejected { status: 405, .. })
    ));
    assert!(reader.management_status(&denied).unwrap().is_none());
    let shown = cli(&server, &f, &["show", BARCODES[0]]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&shown.stdout).unwrap(),
        initial
    );
    let wrong_pool_path = f.root.join("wrong-pool-policy.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[1].to_string(),
            "fdb",
            "tape",
            "set",
            BARCODES[0],
            "--patch",
            "{\"online\":false}",
            "--namespace",
            &ns.to_string(),
            "--request",
            wrong_pool_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("磁带不属于指定池"));
    assert!(!wrong_pool_path.exists());
    assert_eq!(
        reader
            .management_policy(&f.serial, "tape", BARCODES[0])
            .unwrap(),
        initial
    );
    let body = f.root.join("upload-body");
    fs::write(&body, b"same-prepared-upload").unwrap();
    let upload_path = f.root.join("upload.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/policy-upload",
            &body,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            Some(BARCODES[0]),
            &upload_path,
        )
        .unwrap();

    let saved = fs::read(upload_path.join("request.json")).unwrap();
    let offline_path = f.root.join("offline.json");
    let before = mutations(&owner);
    let output = cli(
        &server,
        &f,
        &[
            "set",
            BARCODES[0],
            "--patch",
            r#"{"online":false}"#,
            "--namespace",
            &ns.to_string(),
            "--request",
            offline_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(mutations(&owner), before, "修改策略不装卸或写带");
    assert!(matches!(owner.read(0), Err(ReadError::Busy(_))));
    assert!(client.upload_saved(&upload_path).is_err());
    assert_eq!(fs::read(upload_path.join("request.json")).unwrap(), saved);
    assert_eq!(
        mutations(&owner),
        before,
        "离线目标不能MOVE/WRITE/MAM/FILEMARK"
    );
    assert!(owner.permission.allows(100, Phase::Serving));
    let other_path = f.root.join("other.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/different-target",
            &body,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &other_path,
        )
        .unwrap();

    assert_upload_committed(client.upload_saved(&other_path).unwrap(), &other_path, 1);
    let actual = client
        .upload_receipt(&tape_rs::client::fdb::PreparedUpload::load(&other_path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(actual["barcode"], BARCODES[1], "实际自动写入跳过离线磁带");
    let online_path = f.root.join("online.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "tape",
            "online",
            BARCODES[0],
            "--request",
            online_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let online = tape_rs::client::fdb::PreparedManagement::load(&online_path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = client.management_status(&online).unwrap().unwrap();
        if task["status"] == "succeeded" {
            break;
        }
        assert!(
            task["status"] != "failed" && Instant::now() < deadline,
            "{task}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    client.upload_saved(&upload_path).unwrap();
    let downloaded = f.root.join("downloaded");
    client
        .download_new(&f.serial, f.pools[0], "/policy-upload", &downloaded)
        .unwrap();
    assert_eq!(fs::read(downloaded).unwrap(), b"same-prepared-upload");
    owner.expect(0);
    let before = mutations(&owner);
    let current = client
        .management_policy(&f.serial, "tape", BARCODES[0])
        .unwrap();
    let blacklist = client
        .prepare_control(
            &f.serial,
            ns,
            change(&current["policy"], json!({"blacklisted":true})),
        )
        .unwrap();
    let blacklist_path = f.root.join("blacklist.json");
    blacklist.save_new(&blacklist_path).unwrap();
    // Keep the connection alive until admission, then discard its unread
    // response. The next client must discover that same persisted task.
    let mut connection = std::net::TcpStream::connect(&server.address).unwrap();
    {
        use std::io::Write;
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&blacklist_path).unwrap()).unwrap();
        let bytes = value["request"].to_string().into_bytes();
        write!(connection,"POST /libraries/{}/admin/requests HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nx-tapers-catalog-namespace: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",f.serial,ns,bytes.len()).unwrap();
        connection.write_all(&bytes).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.management_status(&blacklist).unwrap().is_none() {
        if Instant::now() >= deadline {
            use std::io::Read;
            connection
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut reply = String::new();
            let _ = connection.read_to_string(&mut reply);
            panic!("丢失应答前必须已接纳同一请求；原始应答：{reply}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(connection);
    let result = client.run_management_saved(&blacklist_path).unwrap();
    assert_eq!(result["result"]["tape"]["revision"], 4);
    assert_eq!(
        client.run_management_saved(&blacklist_path).unwrap(),
        result
    );
    assert_eq!(mutations(&owner), before);
    assert!(
        matches!(owner.read(0), Err(ReadError::Busy(_))),
        "已挂载暖缓存不能绕过黑名单"
    );
    client.upload_saved(&upload_path).unwrap();
    assert_eq!(mutations(&owner), before, "终态重试返回原结果且不再追加");
    assert!(owner.permission.allows(100, Phase::Serving));
    assert_eq!(
        reader
            .management_policy(&f.serial, "tape", BARCODES[0])
            .unwrap()["policy"]["blacklisted"],
        true
    );
    drop(readonly);
    drop(server);
    drop(ctx);
    drop(owner);
    f.catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let resumed = Owner::start_mode(&f, 101, poison, true);
    assert!(matches!(resumed.read(0), Err(ReadError::Busy(_))));
    assert_eq!(resumed.moves(), 0);
    let ctx = active_pools::context(&resumed, 71, 101);
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
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(30));
    assert_eq!(
        client.run_management_saved(&blacklist_path).unwrap(),
        result
    );
    let allow_path = f.root.join("allow.json");
    let output = cli(
        &server,
        &f,
        &[
            "set",
            BARCODES[0],
            "--patch",
            r#"{"blacklisted":false}"#,
            "--namespace",
            &ns.to_string(),
            "--request",
            allow_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    resumed.expect(0);
    let after = f.root.join("after-restart");
    client
        .download_new(&f.serial, f.pools[0], "/policy-upload", &after)
        .unwrap();
    assert_eq!(fs::read(after).unwrap(), b"same-prepared-upload");
    assert_eq!(
        client
            .management_policy(&f.serial, "tape", BARCODES[0])
            .unwrap()["policy"]["revision"],
        5
    );
    assert_eq!(
        client.run_management_saved(&blacklist_path).unwrap(),
        result
    );
    assert_eq!(
        client
            .management_policy(&f.serial, "tape", BARCODES[0])
            .unwrap()["policy"]["blacklisted"],
        false
    );
    assert!(resumed.permission.allows(101, Phase::Serving));
    drop(server);
    drop(ctx);
    drop(resumed);
    fs::remove_dir_all(f.root).unwrap();
}
