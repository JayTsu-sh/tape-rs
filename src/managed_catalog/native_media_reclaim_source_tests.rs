use super::tests::{fixture, start_target, target_entries};
use super::*;
use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;
use std::io::Cursor;

fn ledger_prefix(ns: Uuid, epoch: u64, operation: Uuid, kind: u8) -> Vec<u8> {
    let mut key = management::prefix(ns, b'R');
    key.push(kind);
    key.extend_from_slice(&epoch.to_be_bytes());
    key.extend_from_slice(operation.as_bytes());
    key
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；Sim 原生 Full 源清单，不访问实际设备"]
async fn reclaim_source_inventory_is_atomic_resumable_and_never_retires_uncovered_data() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&catalog).await;
    let root = std::env::temp_dir().join(format!("reclaim-source-{}", Uuid::new_v4()));
    let scratch = root.join("capture");
    std::fs::create_dir_all(&scratch).unwrap();
    eprintln!("test_workspace={}", root.display());
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("PROOF0L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("PROOF0L8", 0).unwrap();
    let device = library.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "PROOF0".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    for i in 0..40 {
        volume
            .append_file(&format!("/f{i:02}"), &mut Cursor::new(b"kept"))
            .unwrap();
    }
    volume.create_directory("/empty").unwrap();
    volume.commit().unwrap();
    drop(volume);
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = scratch.clone();
    budget.index_scratch.max_bytes = 64 << 20;
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_index_source(
        &TapeDrive::new(&device),
        &read_label_only(&device).unwrap(),
        &budget,
    )
    .unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&captured).unwrap();
    assert_eq!(inventory.objects(), 42);
    let media_commands = device.command_log();
    let source_path = root.join("source.sqlite");
    {
        let storage = ReadStorageBudget {
            index_bytes: 16 << 20,
            cache_bytes: 16 << 20,
            download_bytes: 16 << 20,
            headroom_bytes: 0,
        };
        let mut reserved = CacheBuild::new(&root, storage).unwrap();
        ImportSpool::build_captured(&captured, &source_path, &mut reserved).unwrap();
    }
    let head = captured.construct();
    let expected = ExpectedIndex {
        volume_uuid: head.volume_uuid,
        generation: head.generation,
        location: head.location,
        sha256: head.sha256,
    };
    let spool = ImportSpool::open(&source_path, expected).unwrap();
    let stats = spool.stats();
    let proof = NativeMediaSource {
        identity: spool.identity().clone(),
        files: stats.files,
        // Import candidates omit the physical root. The reclaim source
        // inventory deliberately retains it and must not reuse this count.
        paths: spool.candidate_stats().entries,
    };
    let import = start_target(
        &catalog,
        &serial,
        route,
        pool.id,
        100,
        "PROOF0L8",
        ("/", true),
    )
    .await;
    catalog
        .bind_native_media_source(&import, 100, &proof)
        .await
        .unwrap();
    catalog
        .begin_native_media_projection(&import, 100)
        .await
        .unwrap();
    for entry in target_entries(&import, &spool) {
        catalog
            .project_native_media_entry(&import, 100, &entry, true)
            .await
            .unwrap();
    }
    catalog
        .finish_native_media(&import, 100, &proof)
        .await
        .unwrap();
    let assignment = catalog
        .tape_assignment(&serial, "PROOF0L8")
        .await
        .unwrap()
        .unwrap();
    let spec = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,"volume":head.volume_uuid.to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false})).unwrap();
    let operation = Uuid::new_v4();
    let reservation = catalog
        .reserve_reclaim_pool(&serial, route, 100, 1, operation, &spec)
        .await
        .unwrap();
    let first = catalog
        .seal_reclaim_source_inventory_page(&reservation, 100, &inventory)
        .await
        .unwrap();
    assert_eq!(first.objects(), 32);
    assert!(!first.complete());
    let checkpoint = ledger_prefix(route.route.namespace, 1, operation, b'm');
    let rows = ledger_prefix(route.route.namespace, 1, operation, b'e');
    let original = catalog
        .db
        .create_trx()
        .unwrap()
        .get(&checkpoint, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();

    // A conflict at the last row must not leak the earlier rows of this
    // attempted page through retry_result's domain-error commit semantics.
    let mut late_uid = None;
    captured
        .for_each_entry(|entry| {
            if let crate::ltfs::index::IndexEntry::File { path, node } = entry
                && path == "f39"
            {
                late_uid = Some(node.meta.file_uid);
            }
            Ok(())
        })
        .unwrap();
    let mut late_key = rows.clone();
    late_key.extend_from_slice(&late_uid.unwrap().to_be_bytes());
    let unexpected = b"test-owned-unexpected-late-row";
    let tr = catalog.db.create_trx().unwrap();
    assert!(tr.get(&late_key, false).await.unwrap().is_none());
    tr.set(&late_key, unexpected);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .seal_reclaim_source_inventory_page(&reservation, 100, &inventory)
            .await
            .is_err()
    );
    let tr = catalog.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&checkpoint, false).await.unwrap().unwrap().as_ref(),
        original
    );
    assert_eq!(
        read_page(&tr, &rows, None, 100, false)
            .await
            .unwrap()
            .items
            .len(),
        33
    );
    assert_eq!(
        tr.get(&late_key, false).await.unwrap().unwrap().as_ref(),
        unexpected
    );
    tr.clear(&late_key);
    tr.commit().await.unwrap();

    catalog
        .claim_active_owner_epoch(&serial, 101)
        .await
        .unwrap();
    assert!(
        catalog
            .seal_reclaim_source_inventory_page(&reservation, 100, &inventory)
            .await
            .is_err()
    );
    assert!(
        catalog
            .seal_reclaim_source_inventory_page(&reservation, 101, &inventory)
            .await
            .is_err(),
        "新owner必须先领取原父任务"
    );
    let claimed = catalog
        .claim_reclaim_reservations_page(&serial, route, 101, None, 1)
        .await
        .unwrap();
    assert_eq!(claimed.items.len(), 1);
    assert_eq!(claimed.items[0].intent(), reservation.intent());
    let resumed = &claimed.items[0];
    let complete = catalog
        .seal_reclaim_source_inventory_page(resumed, 101, &inventory)
        .await
        .unwrap();
    assert!(complete.complete());
    assert_eq!(complete.objects(), 42);
    assert_eq!(
        catalog
            .seal_reclaim_source_inventory_page(resumed, 101, &inventory)
            .await
            .unwrap(),
        complete
    );
    assert!(
        catalog
            .check_reclaim_source_unreferenced(resumed, 101)
            .await
            .is_err(),
        "封存源清单不能退休仍有效的数据"
    );
    assert_eq!(
        catalog.tape_assignment(&serial, "PROOF0L8").await.unwrap(),
        Some(assignment)
    );
    assert_eq!(
        device.command_log(),
        media_commands,
        "FDB清单封存/接任/取消不能发设备命令"
    );
    for kind in *b"ghkz" {
        let orphan = ledger_prefix(route.route.namespace, 1, operation, kind);
        let tr = catalog.db.create_trx().unwrap();
        let manifest_before = tr.get(&checkpoint, false).await.unwrap().unwrap().to_vec();
        assert!(tr.get(&orphan, false).await.unwrap().is_none());
        tr.set(&orphan, b"fixture-orphan-data-progress");
        tr.commit().await.unwrap();
        assert!(
            matches!(
                catalog.release_reserved_reclaim(resumed, 101).await,
                Err(CatalogError::Corrupt(_))
            ),
            "零child释放不能跳过孤立的迁移证明"
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&checkpoint, false).await.unwrap().unwrap().as_ref(),
            manifest_before
        );
        assert_eq!(
            tr.get(&orphan, false).await.unwrap().unwrap().as_ref(),
            b"fixture-orphan-data-progress"
        );
        tr.clear(&orphan);
        tr.commit().await.unwrap();
    }
    assert_eq!(device.command_log(), media_commands);
    catalog
        .release_reserved_reclaim(resumed, 101)
        .await
        .unwrap();
    let tr = catalog.db.create_trx().unwrap();
    assert!(tr.get(&checkpoint, false).await.unwrap().is_none());
    assert!(
        read_page(&tr, &rows, None, 1, false)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    catalog
        .check_active_pool_gate(&serial, route, 101, pool.id)
        .await
        .unwrap();
    drop(tr);
    drop(inventory);
    drop(captured);
    drop(spool);
    std::fs::remove_dir_all(root).unwrap();
}
