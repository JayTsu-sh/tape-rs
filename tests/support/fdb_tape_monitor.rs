//! Isolated FoundationDB and simulated media; never opens a real device.
use super::*;
use tape_rs::managed_catalog::{ActiveUploadSpec, TapeMediaFault};

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；监控持久化、选带和 owner 栅栏，无真实设备"]
fn tape_monitor_polls_without_mounting_and_latches_media_fault_across_owners() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(fixture(&cluster));
    let owner = Owner::start_mode(&f, 100, json!({}), true);
    owner.expect(0);
    // Model the Raft control projection consumed by the actual publisher.
    // Its strong Serving checks must remain enabled in this fixture.
    {
        let mut status = owner.status.lock().unwrap();
        status.id = 71;
        status.executor = Some(tape_rs::daemon::state::Executor {
            node: 71,
            round: 100,
            term: 7,
            fenced: true,
        });
        status.executor_ready = true;
    }
    owner.trace.lock().unwrap().clear();
    let first = rt
        .block_on(f.catalog.active_library_inventory(&f.serial, f.route))
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2 * 30 + 8);
    let mut first_sample = None;
    let record = loop {
        let r = rt
            .block_on(
                f.catalog
                    .active_tape_monitor(&f.serial, f.route, BARCODES[0]),
            )
            .unwrap();
        let inventory = rt
            .block_on(f.catalog.active_library_inventory(&f.serial, f.route))
            .unwrap()
            .unwrap();
        if let Some(record) = r
            && let Some(sample) = &record.sample
        {
            if first_sample
                .as_ref()
                .is_some_and(|s| s != &sample.observed_at)
                && inventory["observed_at"] != first["observed_at"]
            {
                break record;
            }
            first_sample.get_or_insert_with(|| sample.observed_at.clone());
        }
        assert!(Instant::now() < deadline, "周期监控没有刷新");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(record.state(), "appendable");
    assert!(record.data_write_allowed());
    let trace = owner.trace.lock().unwrap().clone();
    for cdb in &trace {
        assert!(
            !matches!(
                cdb[0],
                0x1b | 0xa5 | 0xa6 | 0x0a | 0x10 | 0x8d | 0x04 | 0x19 | 0x11 | 0x92 | 0x2b | 0x01
            ),
            "监控出现机械、定位或写入命令 {cdb:?}"
        );
    }
    assert_eq!(
        trace.iter().filter(|c| c[0] == 0x4d).count(),
        2,
        "两个周期每个已核验驱动器各读一次 TapeAlert"
    );
    let sample = record.sample.clone().unwrap();
    assert_eq!(sample.write_protected, Some(false));
    assert!(sample.data.is_some() && sample.index.is_some());
    let head = rt
        .block_on(
            f.catalog
                .active_loaded_volume_head(&f.serial, BARCODES[0], f.route, 100),
        )
        .unwrap()
        .unwrap();
    let cache = f.root.join("owner-100.fdb-extents");
    if cache.exists() {
        fs::remove_dir_all(cache).unwrap();
    }
    owner.fail_read.store(true, Ordering::SeqCst);
    assert!(matches!(owner.read(0), Err(ReadError::Lost(_))));
    let fault = rt
        .block_on(
            f.catalog
                .active_tape_monitor(&f.serial, f.route, BARCODES[0]),
        )
        .unwrap()
        .unwrap();
    let error = fault.last_error.clone().unwrap();
    assert_eq!(error.cdb[0], 0x08);
    assert_eq!(
        (error.status, error.sense_key, error.asc, error.ascq),
        (Some(2), Some(3), Some(0x11), Some(0))
    );
    assert_eq!(fault.media_fault, Some(TapeMediaFault::NeedReplace));
    assert!(!fault.write_allowed());
    // A zero TapeAlert page and successful label/index validation cannot erase
    // a permanent payload fault or its original CDB/sense evidence.
    let retained = rt
        .block_on(f.catalog.record_tape_monitor(
            &f.serial,
            f.route,
            100,
            BARCODES[0],
            head.volume_id,
            Some(&sample),
            None,
            Some((head.index, false)),
        ))
        .unwrap();
    assert_eq!(retained.state(), "need_replace");
    assert_eq!(retained.last_error, Some(error.clone()));
    let mut transient = error.clone();
    transient.sense_key = Some(4);
    transient.asc = Some(0x44);
    transient.raw_sense = vec![0x72, 4, 0x44, 0, 0, 0, 0, 0];
    transient.received_sense_len = 8;
    let latest = rt
        .block_on(f.catalog.record_tape_monitor(
            &f.serial,
            f.route,
            100,
            BARCODES[0],
            head.volume_id,
            None,
            Some(&transient),
            None,
        ))
        .unwrap();
    assert_eq!(latest.media_fault_evidence, Some(error));
    assert_eq!(latest.last_error, Some(transient));
    assert_eq!(latest.state(), "need_replace");
    let spec = ActiveUploadSpec {
        pool_id: head.pool_id,
        path: b"/monitor-must-refuse".to_vec(),
        operation_id: Uuid::new_v4(),
        expected_version: None,
        size: 1,
        digest: ContentDigest::Sha256(Sha256::digest(b"x").into()),
    };
    assert!(matches!(
        rt.block_on(
            f.catalog
                .prepare_active_upload(&f.serial, f.route, 100, BARCODES[0], &spec)
        ),
        Err(CatalogError::MediaUnavailable)
    ));
    let library = rt
        .block_on(f.catalog.active_library_monitor(&f.serial, f.route))
        .unwrap()
        .unwrap();
    rt.block_on(f.catalog.record_library_monitor(
        &f.serial,
        f.route,
        100,
        &tape_rs::ltfs::label::ltfs_time_now(),
        Some("模拟库存失败"),
    ))
    .unwrap();
    let failed = rt
        .block_on(f.catalog.active_library_monitor(&f.serial, f.route))
        .unwrap()
        .unwrap();
    assert_eq!(failed["last_success"], library["last_success"]);

    drop(owner);
    let next = Owner::start_mode(&f, 101, json!({}), true);
    assert!(matches!(
        rt.block_on(f.catalog.record_tape_monitor(
            &f.serial,
            f.route,
            100,
            BARCODES[0],
            head.volume_id,
            Some(&sample),
            None,
            None
        )),
        Err(CatalogError::OwnerFenced)
    ));
    assert!(matches!(
        rt.block_on(f.catalog.record_library_monitor(
            &f.serial,
            f.route,
            100,
            &tape_rs::ltfs::label::ltfs_time_now(),
            None
        )),
        Err(CatalogError::OwnerFenced)
    ));
    assert_eq!(
        rt.block_on(
            f.catalog
                .active_tape_monitor(&f.serial, f.route, BARCODES[0])
        )
        .unwrap()
        .unwrap()
        .state(),
        "need_replace"
    );
    next.expect(0); // Replacement advice still permits an explicit salvage read.
    drop(next);
    fs::remove_dir_all(f.root).unwrap();
}
