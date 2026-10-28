#![cfg(feature = "fdb")]

use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use tape_rs::ltfs::index::{Extent, FileNode, LtfsIndex, NodeMeta, XATTR_MD5, Xattr};
use tape_rs::managed_catalog::{
    CommitReceipt, ContentDigest, ControlOutcome, ImportSpool, LocalVolumeSources, ManagedCatalog,
    NamespaceDigest, NamespaceState, NamespaceVerificationOutcome, Node, NodeAttributes, NodeKind,
    PoolPolicy, PoolRecord, PublishOutcome, ScanDigest, SourceOutcome, VerificationOutcome,
    VolumeOutcome, VolumePlacement, VolumeScan, derive_copy_id,
};
use uuid::Uuid;

/// Run `seed`, interrupt an isolated FDB server or a cluster member, then run
/// `verify` in a new process. Run `audit` against a separately restored backup.
#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER、FDB_RESTART_STATE 和 FDB_RESTART_PHASE；仅用于隔离 FDB 服务重启验收"]
async fn isolated_fdb_server_restart_recovery() {
    let cluster_file = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let state_file = std::env::var("FDB_RESTART_STATE").expect("FDB_RESTART_STATE");
    let phase = std::env::var("FDB_RESTART_PHASE").expect("FDB_RESTART_PHASE");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster_file)).unwrap();

    let run = if phase == "seed" {
        Uuid::new_v4()
    } else {
        Uuid::parse_str(std::fs::read_to_string(&state_file).unwrap().trim()).unwrap()
    };
    let id = |offset| Uuid::from_u128(run.as_u128().wrapping_add(offset));
    let namespace = id(1);
    let serial = format!("RECOVERY_{}", run.simple());
    let pool = PoolRecord {
        id: id(2),
        library_serial: serial.clone(),
        name: "恢复验收池".into(),
        file_limit: 100,
        policy: PoolPolicy::default(),
        revision: 1,
    };
    let updated_pool = PoolRecord {
        name: "恢复验收池2".into(),
        file_limit: 101,
        policy: PoolPolicy {
            media_restriction: "L8".into(),
            mount_limit: 9,
            node_group: "G2".into(),
            low_space_warning_enabled: false,
            low_space_warning_threshold: 17,
            no_space_warning_enabled: true,
        },
        revision: 2,
        ..pool.clone()
    };
    let candidate = id(8);
    let candidate_serial = format!("IMPORT_{}", run.simple());
    let candidate_pool = PoolRecord {
        id: id(9),
        library_serial: candidate_serial.clone(),
        name: "故障恢复导入池".into(),
        file_limit: 200_000,
        policy: PoolPolicy::default(),
        revision: 1,
    };
    let volume = id(3);
    let scan = VolumeScan {
        generation: 1,
        id: id(4),
    };
    let placement = VolumePlacement {
        volume_id: volume,
        namespace,
        pool_id: pool.id,
        library_serial: serial.clone(),
        barcode: "TESTL8".into(),
    };
    let pending_barcode = "PENDINGL8";
    let pending_placement = VolumePlacement {
        volume_id: id(7),
        barcode: pending_barcode.into(),
        ..placement.clone()
    };
    let mut xattrs = vec![Xattr {
        key: XATTR_MD5.into(),
        value: "07".repeat(16),
        base64: false,
    }];
    xattrs.extend((0..9).map(|i| Xattr {
        key: format!("perf_padding_{i}"),
        value: "x".repeat(65_536),
        base64: false,
    }));
    let attributes = NodeAttributes::new(
        NodeMeta {
            readonly: true,
            modify_time: "2026-10-23T00:00:00.000000000Z".into(),
            file_uid: u64::MAX - 7,
            ..Default::default()
        },
        xattrs,
        None,
    )
    .unwrap();
    let mut receipt = CommitReceipt {
        namespace,
        parent: Uuid::nil(),
        name: b"restart-file".to_vec(),
        file_id: id(5),
        expected_version: None,
        operation_id: id(6),
        request_epoch: 1,
        copy_id: Uuid::nil(),
        library_serial: serial.clone(),
        pool_id: Some(pool.id),
        owner_epoch: 10,
        volume_id: volume,
        volume_generation: scan.generation,
        volume_scan_id: scan.id,
        ltfs_file_uid: u64::MAX - 7,
        size: 123,
        content_digest: ContentDigest::Md5([7; 16]),
        metadata_digest: Some(attributes.digest()),
    };
    receipt.copy_id = derive_copy_id(&receipt);
    let cache_path = std::env::temp_dir().join(format!("tape-rs-fdb-restart-source-{run}.db"));

    match phase.as_str() {
        "seed" => {
            assert!(catalog.begin_managed_import(candidate).await.unwrap());
            assert_eq!(
                catalog
                    .register_physical_library(candidate, &candidate_serial)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog
                    .create_pool(candidate, &candidate_pool)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog
                    .assign_tape(
                        candidate,
                        candidate_pool.id,
                        &candidate_serial,
                        "CANDIDATEL8"
                    )
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert!(catalog.create_managed_namespace(namespace).await.unwrap());
            assert_eq!(
                catalog
                    .register_physical_library(namespace, &serial)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog.create_pool(namespace, &pool).await.unwrap(),
                ControlOutcome::Created
            );
            assert!(catalog.register_node_group(namespace, "G2").await.unwrap());
            assert_eq!(
                catalog
                    .update_pool(namespace, &pool, &updated_pool)
                    .await
                    .unwrap(),
                ControlOutcome::Updated
            );
            assert_eq!(
                catalog
                    .assign_tape(namespace, pool.id, &serial, &placement.barcode)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog
                    .assign_tape(namespace, pool.id, &serial, pending_barcode)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
            assert_eq!(
                catalog
                    .register_volume_placement(&placement, 10)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog
                    .begin_volume_reconcile(&serial, 10, volume, scan)
                    .await
                    .unwrap(),
                VolumeOutcome::Reconciling
            );
            let summary = ScanDigest::new(volume, scan).unwrap().summary();
            assert_eq!(
                catalog
                    .freeze_volume_scan(&serial, 10, volume, scan, summary)
                    .await
                    .unwrap(),
                VerificationOutcome::SourceMissing
            );
            use sha2::Digest;
            let mut index = LtfsIndex::empty(volume, "FDB restart source".into(), 'b');
            index.self_location.start_block = 5;
            index.highest_file_uid = receipt.ltfs_file_uid;
            index.root.files.push(FileNode {
                name: "restart-file".into(),
                length: receipt.size,
                meta: attributes.meta().unwrap().clone(),
                extents: vec![Extent {
                    partition: 'b',
                    start_block: 6,
                    byte_offset: 0,
                    byte_count: receipt.size,
                    file_offset: 0,
                }],
                xattrs: attributes.xattrs().unwrap().to_vec(),
                ..Default::default()
            });
            let xml = index.to_xml().unwrap();
            let source = ExpectedIndex {
                volume_uuid: volume,
                generation: scan.generation,
                location: index.self_location,
                sha256: sha2::Sha256::digest(&xml).into(),
            };
            VolumeExtentCache::build(xml.as_ref(), &cache_path, source).unwrap();
            let spool_path = cache_path.with_extension("spool.db");
            ImportSpool::build(xml.as_ref(), &spool_path, source).unwrap();
            assert!(
                catalog
                    .record_volume_source(
                        &serial,
                        10,
                        volume,
                        scan,
                        source,
                        LocalVolumeSources {
                            extent_cache: &cache_path,
                            import_spool: &cache_path.with_extension("missing-spool.db"),
                        },
                    )
                    .await
                    .is_err()
            );
            let mut wrong_source = source;
            wrong_source.sha256[0] ^= 1;
            assert!(
                catalog
                    .record_volume_source(
                        &serial,
                        10,
                        volume,
                        scan,
                        wrong_source,
                        LocalVolumeSources {
                            extent_cache: &cache_path,
                            import_spool: &spool_path
                        },
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                catalog
                    .record_volume_source(
                        &serial,
                        10,
                        volume,
                        scan,
                        source,
                        LocalVolumeSources {
                            extent_cache: &cache_path,
                            import_spool: &spool_path
                        },
                    )
                    .await
                    .unwrap(),
                SourceOutcome::Recorded
            );
            assert_eq!(
                catalog
                    .record_volume_source(
                        &serial,
                        10,
                        volume,
                        scan,
                        source,
                        LocalVolumeSources {
                            extent_cache: &cache_path,
                            import_spool: &spool_path
                        },
                    )
                    .await
                    .unwrap(),
                SourceOutcome::AlreadyRecorded
            );
            std::fs::remove_file(spool_path).unwrap();
            assert_eq!(
                catalog
                    .freeze_volume_scan(&serial, 10, volume, scan, summary)
                    .await
                    .unwrap(),
                VerificationOutcome::Frozen
            );
            assert_eq!(
                catalog
                    .verify_volume_scan_page(&serial, 10, volume, scan, 100)
                    .await
                    .unwrap(),
                VerificationOutcome::CacheRequired
            );
            loop {
                match catalog
                    .verify_volume_scan_page_with_cache(&serial, 10, volume, scan, 100, &cache_path)
                    .await
                    .unwrap()
                {
                    VerificationOutcome::Progress(_) => {}
                    VerificationOutcome::Verified(actual) => {
                        assert_eq!(actual, summary);
                        break;
                    }
                    unexpected => panic!("unexpected verification: {unexpected:?}"),
                }
            }
            assert_eq!(
                catalog
                    .mark_volume_ready(&serial, 10, volume, scan)
                    .await
                    .unwrap(),
                VolumeOutcome::Ready
            );
            assert_eq!(
                catalog.register_request(&receipt).await.unwrap(),
                PublishOutcome::Registered
            );
            let mut wrong_hash = receipt.clone();
            wrong_hash.content_digest = ContentDigest::Unknown;
            assert!(
                catalog
                    .publish_with_attributes(&wrong_hash, &attributes)
                    .await
                    .is_err()
            );
            use std::io::Write;
            let mut state = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&state_file)
                .unwrap();
            writeln!(state, "{run}").unwrap();
            state.sync_all().unwrap();
        }
        "verify" => {
            assert_eq!(
                catalog.namespace_state(candidate).await.unwrap(),
                Some(NamespaceState::Building)
            );
            assert!(
                catalog
                    .tape_assignment(&candidate_serial, "CANDIDATEL8")
                    .await
                    .is_err()
            );
            let mut source = NamespaceDigest::new(candidate).unwrap();
            source
                .push(
                    Uuid::nil(),
                    &Node {
                        kind: NodeKind::Directory,
                        parent: Uuid::nil(),
                        name: Vec::new(),
                        attributes: None,
                    },
                    None,
                )
                .unwrap();
            assert_eq!(
                catalog
                    .freeze_namespace(candidate, source.summary())
                    .await
                    .unwrap(),
                NamespaceVerificationOutcome::Frozen
            );
            loop {
                match catalog.verify_namespace_page(candidate, 10).await.unwrap() {
                    NamespaceVerificationOutcome::Progress { .. } => {}
                    NamespaceVerificationOutcome::Verified(actual) => {
                        assert_eq!(actual, source.summary());
                        break;
                    }
                    other => panic!("candidate verification: {other:?}"),
                }
            }
            assert!(catalog.seal_import(candidate).await.unwrap());
            assert_eq!(
                catalog.pool(candidate, candidate_pool.id).await.unwrap(),
                Some(candidate_pool.clone())
            );
            assert_eq!(
                catalog
                    .tape_assignment(&candidate_serial, "CANDIDATEL8")
                    .await
                    .unwrap()
                    .unwrap()
                    .volume_id,
                None
            );
            assert_eq!(
                catalog
                    .tape_assignment(&serial, pending_barcode)
                    .await
                    .unwrap()
                    .unwrap()
                    .volume_id,
                None
            );
            assert_eq!(
                catalog.namespace_state(namespace).await.unwrap(),
                Some(NamespaceState::Ready)
            );
            assert_eq!(
                catalog.pool(namespace, pool.id).await.unwrap(),
                Some(updated_pool)
            );
            assert_eq!(
                catalog
                    .list_node_groups(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec!["G0", "G2"]
            );
            assert_eq!(
                catalog.volume_placement(volume).await.unwrap(),
                Some(placement)
            );
            assert_eq!(
                catalog
                    .volume_source(volume, scan)
                    .await
                    .unwrap()
                    .location
                    .start_block,
                5
            );
            let cache = catalog
                .open_volume_cache(volume, scan, &cache_path)
                .await
                .unwrap();
            let cached = cache.required_file_by_uid(receipt.ltfs_file_uid).unwrap();
            assert_eq!(cached.path, "restart-file");
            assert_eq!(cached.length, receipt.size);
            assert_eq!(cached.extents.len(), 1);
            drop(cache);
            std::fs::remove_file(&cache_path).unwrap();
            assert_eq!(
                catalog
                    .pending_requests(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec![receipt.clone()]
            );
            let mut changed_uid = receipt.clone();
            changed_uid.ltfs_file_uid -= 1;
            assert_eq!(
                catalog.register_request(&changed_uid).await.unwrap(),
                PublishOutcome::OperationConflict
            );
            assert!(catalog.advance_owner_epoch(&serial, 11).await.unwrap());
            assert_eq!(
                catalog
                    .register_volume_placement(&pending_placement, 10)
                    .await
                    .unwrap(),
                ControlOutcome::StaleOwner
            );
            assert_eq!(
                catalog
                    .register_volume_placement(&pending_placement, 11)
                    .await
                    .unwrap(),
                ControlOutcome::Created
            );
            assert_eq!(
                catalog
                    .publish_with_attributes(&receipt, &attributes)
                    .await
                    .unwrap(),
                PublishOutcome::StaleOwner
            );
            let successor = CommitReceipt {
                owner_epoch: 11,
                ..receipt.clone()
            };
            assert_eq!(
                catalog
                    .publish_with_attributes(&successor, &attributes)
                    .await
                    .unwrap(),
                PublishOutcome::Applied(1)
            );
            assert!(
                catalog
                    .pending_requests(namespace, None, 10)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert_eq!(
                catalog
                    .publish_with_attributes(&receipt, &attributes)
                    .await
                    .unwrap(),
                PublishOutcome::AlreadyApplied(1)
            );
            assert_eq!(
                catalog
                    .file_head(namespace, receipt.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                1
            );
            assert_eq!(
                catalog
                    .node(namespace, receipt.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .attributes,
                Some(attributes.clone())
            );
            assert_eq!(
                catalog
                    .physical_copy(namespace, receipt.file_id, receipt.copy_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .ltfs_file_uid,
                receipt.ltfs_file_uid
            );
            assert_eq!(
                catalog
                    .physical_copy(namespace, receipt.file_id, receipt.copy_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .size,
                receipt.size
            );
            assert_eq!(
                catalog
                    .physical_copy(namespace, receipt.file_id, receipt.copy_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .content_digest,
                ContentDigest::Md5([7; 16])
            );
        }
        "audit" => {
            assert_eq!(
                catalog.namespace_state(candidate).await.unwrap(),
                Some(NamespaceState::Ready)
            );
            assert_eq!(
                catalog.pool(candidate, candidate_pool.id).await.unwrap(),
                Some(candidate_pool.clone())
            );
            assert_eq!(
                catalog
                    .tape_assignment(&candidate_serial, "CANDIDATEL8")
                    .await
                    .unwrap()
                    .unwrap()
                    .volume_id,
                None
            );
            assert_eq!(
                catalog
                    .tape_assignment(&serial, pending_barcode)
                    .await
                    .unwrap()
                    .unwrap()
                    .volume_id,
                Some(pending_placement.volume_id)
            );
            assert_eq!(
                catalog.namespace_state(namespace).await.unwrap(),
                Some(NamespaceState::Ready)
            );
            assert_eq!(
                catalog.pool(namespace, pool.id).await.unwrap(),
                Some(updated_pool)
            );
            assert_eq!(
                catalog
                    .list_node_groups(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec!["G0", "G2"]
            );
            assert_eq!(
                catalog.volume_placement(volume).await.unwrap(),
                Some(placement)
            );
            assert!(
                catalog
                    .pending_requests(namespace, None, 10)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert_eq!(
                catalog
                    .file_head(namespace, receipt.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                1
            );
            assert_eq!(
                catalog
                    .node(namespace, receipt.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .attributes,
                Some(attributes.clone())
            );
            assert_eq!(
                catalog
                    .physical_copy(namespace, receipt.file_id, receipt.copy_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .ltfs_file_uid,
                receipt.ltfs_file_uid
            );
            assert_eq!(
                catalog
                    .physical_copy(namespace, receipt.file_id, receipt.copy_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .size,
                receipt.size
            );
            assert_eq!(
                catalog
                    .publish_with_attributes(&receipt, &attributes)
                    .await
                    .unwrap(),
                PublishOutcome::AlreadyApplied(1)
            );
        }
        _ => panic!("FDB_RESTART_PHASE 必须是 seed、verify 或 audit"),
    }
}
