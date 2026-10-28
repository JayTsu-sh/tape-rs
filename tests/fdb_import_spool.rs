#![cfg(feature = "fdb")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};
use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use tape_rs::ltfs::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta, Xattr};
use tape_rs::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION, tombstone_path};
use tape_rs::managed_catalog::{
    ActiveBatchOutcome, ActiveBatchPhase, ActiveCompactionOutcome, ActiveFileIntent,
    ActiveFinalizationOutcome, ActiveIndexIdentity, ActiveIndexOutcome,
    ActiveMaterializationOutcome, ActivePreparationOutcome, ActivePublicationOutcome,
    CommitReceipt, ContentDigest, ControlOutcome, DirectoryOutcome, ForgetRouteOutcome,
    ImportReconciler, ImportSourceBinding, ImportSpool, ImportVolume, LocalVolumeSources,
    ManagedCatalog, NamespaceDigest, NamespaceState, NamespaceVerificationOutcome, Node,
    NodeAttributes, NodeKind, PoolPolicy, PoolRecord, PrepareRouteOutcome, PreparedLibraryRoute,
    PublishOutcome, ScanDigest, SourceOutcome, StagedSourceData, VerificationOutcome,
    VolumeOutcome, VolumePlacement, VolumeScan, WindowOutcome, derive_copy_id,
    inspect_frozen_snapshot, verify_node_export,
};
use uuid::Uuid;

struct TestDaemon(std::process::Child);

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fdb_http_probe(port: u16, method: &str, path: &str) -> String {
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut output = String::new();
    conn.read_to_string(&mut output).unwrap();
    output
}

#[test]
fn confirmed_ee_xml_builds_a_private_readable_import_source() {
    let xml = include_bytes!("fixtures/ee-recovery-dp-54.xml");
    let index = LtfsIndex::parse(xml).unwrap();
    let expected = ExpectedIndex {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.self_location,
        sha256: Sha256::digest(xml).into(),
    };
    let path = std::env::temp_dir().join(format!("tape-rs-fdb-import-{}.db", Uuid::new_v4()));
    let cache_path = path.with_extension("extent.db");
    let stats = ImportSpool::build(xml.as_slice(), &path, expected).unwrap();
    VolumeExtentCache::build(xml.as_slice(), &cache_path, expected).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let spool = ImportSpool::open(&path, expected).unwrap();
    let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
    assert_eq!(spool.stats(), stats);
    spool.verify_extent_cache(&cache).unwrap();
    let mut cursor = None;
    let mut files = 0u64;
    let mut directories = 0u64;
    let mut aliases = 0u64;
    loop {
        let page = spool.page(cursor.as_deref(), 7).unwrap();
        for node in &page.nodes {
            match node.kind {
                NodeKind::Directory => directories += 1,
                NodeKind::File => files += 1,
            }
            if node.attributes.symlink().is_some() {
                aliases += 1;
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!((files, directories), (stats.files, stats.directories));
    assert!(aliases > 0);
    let mut candidate_cursor = None;
    let mut candidate_count = 0u64;
    let mut last = String::new();
    let mut alias_candidate = false;
    loop {
        let page = spool
            .candidate_page(candidate_cursor.as_deref(), 7)
            .unwrap();
        for candidate in &page.candidates {
            assert!(candidate.logical_path.starts_with('/'));
            assert!(!candidate.logical_path.starts_with("/.LTFSEE_DATA"));
            assert!(last.as_str() < candidate.logical_path.as_str());
            last = candidate.logical_path.clone();
            let node = spool
                .node_by_path(&candidate.physical_path)
                .unwrap()
                .unwrap();
            alias_candidate |= node.attributes.symlink().is_some();
            candidate_count += 1;
        }
        candidate_cursor = page.next;
        if candidate_cursor.is_none() {
            break;
        }
    }
    assert_eq!(candidate_count, spool.candidate_stats().entries);
    assert!(alias_candidate);
    drop(cache);
    drop(spool);
    fs::remove_file(path).unwrap();
    fs::remove_file(cache_path).unwrap();
}

#[test]
#[ignore = "需要 TAPE_RS_DIRECTORY_SNAPSHOT 指向无磁带的离线一致目录快照；只读，不访问设备"]
fn offline_empty_directory_snapshot_is_auditable() {
    let path = std::env::var("TAPE_RS_DIRECTORY_SNAPSHOT").expect("TAPE_RS_DIRECTORY_SNAPSHOT");
    let mut reconciler = ImportReconciler::new(Vec::new()).unwrap();
    let audit = reconciler
        .audit_directory_snapshot(std::path::Path::new(&path))
        .unwrap();
    assert!(audit.applied_index > 0);
    assert_eq!((audit.paths, audit.tombstones), (0, 0));
}

async fn register_staged_source(
    catalog: &ManagedCatalog,
    placement: VolumePlacement,
    xml: &[u8],
    source: ExpectedIndex,
    spool_path: &std::path::Path,
) -> (ImportSourceBinding, std::path::PathBuf) {
    let namespace = placement.namespace;
    let pool = placement.pool_id;
    let serial = placement.library_serial.as_str();
    let barcode = placement.barcode.as_str();
    assert_eq!(
        catalog
            .assign_tape(namespace, pool, serial, barcode)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert_eq!(
        catalog
            .register_volume_placement(&placement, 10)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    let scan = VolumeScan {
        generation: source.generation,
        id: Uuid::new_v4(),
    };
    assert_eq!(
        catalog
            .begin_volume_reconcile(serial, 10, source.volume_uuid, scan)
            .await
            .unwrap(),
        VolumeOutcome::Reconciling
    );
    let cache_path = std::env::temp_dir().join(format!("tape-rs-stage-cache-{}.db", scan.id));
    VolumeExtentCache::build(xml, &cache_path, source).unwrap();
    assert_eq!(
        catalog
            .record_volume_source(
                serial,
                10,
                source.volume_uuid,
                scan,
                source,
                LocalVolumeSources {
                    extent_cache: &cache_path,
                    import_spool: spool_path,
                },
            )
            .await
            .unwrap(),
        SourceOutcome::Recorded
    );
    let empty = ScanDigest::new(source.volume_uuid, scan).unwrap().summary();
    assert_eq!(
        catalog
            .freeze_volume_scan(serial, 10, source.volume_uuid, scan, empty)
            .await
            .unwrap(),
        VerificationOutcome::Frozen
    );
    assert_eq!(
        catalog
            .verify_volume_scan_page_with_cache(
                serial,
                10,
                source.volume_uuid,
                scan,
                100,
                &cache_path,
            )
            .await
            .unwrap(),
        VerificationOutcome::Verified(empty)
    );
    assert_eq!(
        catalog
            .mark_volume_ready(serial, 10, source.volume_uuid, scan)
            .await
            .unwrap(),
        VolumeOutcome::Ready
    );
    (
        ImportSourceBinding {
            placement,
            scan,
            source,
        },
        cache_path,
    )
}

#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；合成外部源和三个本地冻结快照，不访问设备"]
async fn external_media_import_is_bound_sealed_and_requires_empty_node_exports() {
    use tape_rs::daemon::directory::Directory;
    use tape_rs::daemon::state::{Applied, Command, ControlState};
    use tape_rs::ltfs::index::Extent;
    use tape_rs::managed_catalog::ImportOrigin;
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let serial = format!("EXTERNAL-{namespace}");
    let root = std::env::temp_dir().join(format!("external-import-{namespace}"));
    fs::create_dir(&root).unwrap();
    let mut directory = Directory::open(&root.join("live.db")).unwrap();
    let mut control = ControlState::default();
    for (i, cmd) in [
        (20, Command::CatalogQuiesce),
        (21, Command::CatalogFreeze { quiesce_index: 20 }),
    ] {
        let applied = control.apply(i, &cmd);
        assert!(matches!(applied, Applied::Admin(Ok(_))));
        directory.apply(i, &cmd, &applied).unwrap();
    }
    let snapshot = root.join("node1.db");
    directory.write_snapshot(&snapshot).unwrap();
    let identity = inspect_frozen_snapshot(&snapshot).unwrap();
    assert_eq!(identity.row_counts, [0; 4]);
    let mut index = LtfsIndex::empty(Uuid::new_v4(), "external".into(), 'b');
    index.self_location.start_block = 5;
    index.highest_file_uid = 6;
    index.root.files.push(FileNode {
        name: "external".into(),
        xattrs: vec![Xattr {
            key: "large-preserved".into(),
            value: "p".repeat(6_000_000),
            base64: false,
        }],
        length: 3,
        extents: vec![Extent {
            partition: 'b',
            start_block: 100,
            byte_offset: 0,
            byte_count: 3,
            file_offset: 0,
        }],
        meta: NodeMeta {
            file_uid: 2,
            ..Default::default()
        },
        ..Default::default()
    });
    index.root.subdirs.push(DirectoryNode {
        name: ".LTFSEE_DATA".into(),
        meta: NodeMeta {
            file_uid: 3,
            ..Default::default()
        },
        files: vec![FileNode {
            name: "object".into(),
            length: 7,
            extents: vec![Extent {
                partition: 'b',
                start_block: 101,
                byte_offset: 0,
                byte_count: 7,
                file_offset: 0,
            }],
            meta: NodeMeta {
                file_uid: 4,
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    });
    index.root.files.push(FileNode {
        name: "alias".into(),
        meta: NodeMeta {
            file_uid: 5,
            ..Default::default()
        },
        symlink: Some(".LTFSEE_DATA/object".into()),
        ..Default::default()
    });
    index.root.files.push(FileNode {
        name: "native-link".into(),
        meta: NodeMeta {
            file_uid: 6,
            ..Default::default()
        },
        symlink: Some("../external".into()),
        ..Default::default()
    });
    let xml = index.to_xml().unwrap();
    let source = ExpectedIndex {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.self_location,
        sha256: Sha256::digest(&xml).into(),
    };
    let spool_path = root.join("import.db");
    ImportSpool::build(xml.as_ref(), &spool_path, source).unwrap();
    let spool = ImportSpool::open(&spool_path, source).unwrap();
    assert!(catalog.begin_managed_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert_eq!(
        catalog
            .create_pool(
                namespace,
                &PoolRecord {
                    library_serial: serial.clone(),
                    id: pool,
                    name: "external".into(),
                    file_limit: 1000,
                    policy: PoolPolicy::default(),
                    revision: 1
                }
            )
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    let (binding, cache) = register_staged_source(
        &catalog,
        VolumePlacement {
            volume_id: index.volume_uuid,
            namespace,
            pool_id: pool,
            library_serial: serial.clone(),
            barcode: "EXT001L8".into(),
        },
        xml.as_ref(),
        source,
        &spool_path,
    )
    .await;
    let volumes = || {
        vec![ImportVolume {
            pool_id: pool,
            barcode: "EXT001L8".into(),
            volume_id: index.volume_uuid,
            generation: index.generation,
            spool: &spool,
        }]
    };
    assert!(
        catalog
            .stage_audited_reconcile(
                namespace,
                &snapshot,
                &root,
                volumes(),
                std::slice::from_ref(&binding)
            )
            .await
            .is_err()
    );
    assert!(
        catalog
            .stage_external_import(namespace, &snapshot, &root, volumes(), &[])
            .await
            .is_err()
    );
    let mut wrong = binding.clone();
    wrong.source.sha256[0] ^= 1;
    assert!(
        catalog
            .stage_external_import(namespace, &snapshot, &root, volumes(), &[wrong])
            .await
            .is_err()
    );
    let staged = catalog
        .stage_external_import(
            namespace,
            &snapshot,
            &root,
            volumes(),
            std::slice::from_ref(&binding),
        )
        .await
        .unwrap();
    assert_eq!(staged.paths, 3);
    assert!(matches!(
        staged.audit.origin,
        ImportOrigin::ExternalMedia { .. }
    ));
    assert_eq!(
        catalog
            .stage_external_import(
                namespace,
                &snapshot,
                &root,
                volumes(),
                std::slice::from_ref(&binding)
            )
            .await
            .unwrap(),
        staged
    );
    assert!(
        catalog
            .active_library_route(&serial)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .prepare_library_route(&serial, namespace, None)
            .await
            .is_err()
    );
    catalog
        .confirm_bound_staged_import(namespace)
        .await
        .unwrap();
    while !catalog
        .materialize_bound_import_page(namespace, 10)
        .await
        .unwrap()
        .complete()
    {}
    assert_eq!(
        catalog.freeze_bound_staged_import(namespace).await.unwrap(),
        NamespaceVerificationOutcome::Frozen
    );
    loop {
        match catalog.verify_namespace_page(namespace, 10).await.unwrap() {
            NamespaceVerificationOutcome::Progress { .. } => continue,
            NamespaceVerificationOutcome::Verified(s) => {
                assert_eq!(s.files, 3);
                break;
            }
            other => panic!("external namespace failed: {other:?}"),
        }
    }
    assert!(catalog.seal_bound_staged_import(namespace).await.is_err());
    assert_eq!(
        catalog.verify_bound_import_copies(namespace).await.unwrap(),
        3
    );
    catalog
        .inspect_bound_import_copy_fence(namespace)
        .await
        .unwrap();
    assert!(catalog.seal_bound_staged_import(namespace).await.unwrap());
    let route = match catalog
        .prepare_library_route(&serial, namespace, None)
        .await
        .unwrap()
    {
        PrepareRouteOutcome::Prepared(r) => r,
        x => panic!("{x:?}"),
    };
    let proof = |node| {
        verify_node_export(
            &snapshot,
            node,
            identity.applied_index,
            identity.frozen_index,
            identity.snapshot_sha256,
        )
        .unwrap()
    };
    assert!(
        catalog
            .certify_prepared_cutover(&serial, route, 1, [proof(1), proof(1), proof(3)])
            .await
            .is_err()
    );
    assert!(
        catalog
            .certify_prepared_cutover(&serial, route, 1, [proof(1), proof(2), proof(3)])
            .await
            .unwrap()
    );
    let certificate = catalog
        .certified_prepared_cutover(&serial)
        .await
        .unwrap()
        .unwrap();
    let db = foundationdb::Database::new(Some(&cluster)).unwrap();
    let mut manifest = b"tape-rs/catalog/22/f/".to_vec();
    manifest.extend_from_slice(namespace.as_bytes());
    let tr = db.create_trx().unwrap();
    let original = tr.get(&manifest, false).await.unwrap().unwrap().to_vec();
    let mut changed = original.clone();
    changed[138] = 0;
    changed[139..].fill(0);
    tr.set(&manifest, &changed);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .activate_prepared_cutover(&serial, certificate)
            .await
            .is_err()
    );
    let tr = db.create_trx().unwrap();
    tr.set(&manifest, &original);
    tr.commit().await.unwrap();
    let active = catalog
        .activate_prepared_cutover(&serial, certificate)
        .await
        .unwrap();
    assert_eq!(active.frozen_index, 21);
    let read = catalog
        .resolve_active_read(&serial, format!("/{pool}/external").as_bytes(), &cache)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.head.size, 3);
    assert_eq!(read.physical.path, "external");
    let attributes = read.attributes.as_ref().unwrap();
    assert_eq!(
        attributes
            .xattrs()
            .unwrap()
            .iter()
            .find(|x| x.key == "large-preserved")
            .unwrap()
            .value
            .len(),
        6_000_000
    );
    let mut streamed = Vec::new();
    assert!(
        catalog
            .node_attributes_to(namespace, read.id, &mut streamed)
            .await
            .unwrap()
    );
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/node-attributes/1");
    hash.update(&streamed);
    assert_eq!(<[u8; 32]>::from(hash.finalize()), attributes.digest());
    // Core stat remains independent of optional xattr hydration. Corruption
    // still fails explicit full-attribute reads, never masquerading as a 404.
    let expected_route = catalog
        .active_library_route(&serial)
        .await
        .unwrap()
        .unwrap();
    let mut chunk = b"tape-rs/catalog/22/M/".to_vec();
    chunk.extend_from_slice(namespace.as_bytes());
    chunk.extend_from_slice(&attributes.digest());
    chunk.push(1);
    chunk.extend_from_slice(&0u16.to_be_bytes());
    let tr = db.create_trx().unwrap();
    tr.clear(&chunk);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .resolve_active_metadata_for_route(
                &serial,
                format!("/{pool}/external").as_bytes(),
                expected_route
            )
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        catalog
            .resolve_active_path_for_route(
                &serial,
                format!("/{pool}/external").as_bytes(),
                expected_route
            )
            .await
            .is_err()
    );
    let tr = db.create_trx().unwrap();
    tr.set(&chunk, &streamed[..64_000]);
    tr.commit().await.unwrap();
    let alias = catalog
        .resolve_active_read(&serial, format!("/{pool}/alias").as_bytes(), &cache)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alias.head.size, 7);
    assert_eq!(alias.physical.path, ".LTFSEE_DATA/object");
    let native = catalog
        .resolve_active_metadata_for_route(
            &serial,
            format!("/{pool}/native-link").as_bytes(),
            expected_route,
        )
        .await
        .unwrap()
        .unwrap();
    let tape_rs::managed_catalog::ResolvedPath::File {
        attributes, head, ..
    } = native
    else {
        panic!("native symlink must retain its LTFS file identity");
    };
    assert_eq!(head.size, 0);
    assert_eq!(attributes.unwrap().symlink(), Some("../external"));
    assert!(matches!(
        catalog
            .resolve_active_read(
                &serial,
                format!("/{pool}/native-link").as_bytes(),
                &root.join("absent-cache.db")
            )
            .await,
        Err(tape_rs::managed_catalog::CatalogError::Invalid(
            "原生符号链接不能作为普通文件读取"
        ))
    ));

    assert!(
        catalog
            .stage_external_import(
                namespace,
                &snapshot,
                &root,
                volumes(),
                std::slice::from_ref(&binding)
            )
            .await
            .is_err()
    );
    drop(spool);
    drop(directory);
    fs::remove_file(cache).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；只使用合成索引与临时目录快照"]
async fn audited_candidates_stage_in_fdb_without_becoming_visible() {
    staged_import_synthetic(false, false).await;
}

#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅物化合成 Building 文件"]
async fn confirmed_data_file_materializes_with_all_copies_and_replays() {
    staged_import_synthetic(true, false).await;
}

#[test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；发布随机合成的无墓碑目录"]
fn verified_data_import_seals_and_reclaims_staging() {
    // This end-to-end async fixture contains many independent checkpoints.
    // Keep its generated future off the test harness's small default stack.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(staged_import_synthetic(true, true));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；仅导入合成 LTFS 墓碑"]
async fn confirmed_tombstone_is_private_and_verified_with_full_scan() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let serial = format!("TOMB-{}", namespace);
    let barcode = "TOMB001L8";
    let path = "/gone";
    let mut index = LtfsIndex::empty(Uuid::new_v4(), "tomb-source".into(), 'b');
    index.self_location.start_block = 5;
    index.root.subdirs.push(DirectoryNode {
        name: ".tapers".into(),
        meta: NodeMeta {
            file_uid: 2,
            ..Default::default()
        },
        subdirs: vec![DirectoryNode {
            name: "deleted".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            files: [(path, 4, "2.1"), ("/gone2", 5, "2.2")]
                .into_iter()
                .map(|(path, uid, version)| FileNode {
                    name: tombstone_path(path).rsplit('/').next().unwrap().into(),
                    meta: NodeMeta {
                        file_uid: uid,
                        ..Default::default()
                    },
                    xattrs: vec![
                        Xattr {
                            key: XATTR_VERSION.into(),
                            value: version.into(),
                            base64: false,
                        },
                        Xattr {
                            key: XATTR_DELETED_PATH.into(),
                            value: path.into(),
                            base64: false,
                        },
                    ],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    });
    index.highest_file_uid = 5;
    let xml = index.to_xml().unwrap();
    let expected = ExpectedIndex {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.self_location,
        sha256: Sha256::digest(&xml).into(),
    };
    let spool_path = std::env::temp_dir().join(format!("tape-rs-tomb-spool-{namespace}.db"));
    let snapshot = std::env::temp_dir().join(format!("tape-rs-tomb-snap-{namespace}.db"));
    ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
    let spool = ImportSpool::open(&spool_path, expected).unwrap();
    let db = rusqlite::Connection::open(&snapshot).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
         CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
           volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL);
         CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
           generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
           ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
           metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
         CREATE TABLE staging(barcode TEXT NOT NULL);
         CREATE TABLE file_tasks(status TEXT NOT NULL);
         INSERT INTO meta(k,v) VALUES('applied_index',93),('catalog_frozen_index',90);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            barcode,
            pool.to_string(),
            index.volume_uuid.to_string(),
            index.generation as i64
        ],
    )
    .unwrap();
    for record in tape_rs::daemon::files::catalog_of(&index).0 {
        db.execute(
            "INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                pool.to_string(), record.path, barcode, index.generation as i64,
                record.length as i64, record.sha256, record.version.0 as i64,
                record.version.1 as i64, i64::from(record.deleted), record.metadata.to_string(),
            ],
        )
        .unwrap();
    }
    assert!(catalog.begin_managed_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert_eq!(
        catalog
            .create_pool(
                namespace,
                &PoolRecord {
                    library_serial: serial.clone(),
                    id: pool,
                    name: "tomb".into(),
                    file_limit: 1_000_000,
                    policy: PoolPolicy::default(),
                    revision: 1,
                },
            )
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    let placement = VolumePlacement {
        volume_id: index.volume_uuid,
        namespace,
        pool_id: pool,
        library_serial: serial.clone(),
        barcode: barcode.into(),
    };
    let (binding, cache_path) =
        register_staged_source(&catalog, placement, xml.as_ref(), expected, &spool_path).await;
    let volumes = || {
        vec![ImportVolume {
            pool_id: pool,
            barcode: barcode.into(),
            volume_id: index.volume_uuid,
            generation: index.generation,
            spool: &spool,
        }]
    };
    let staged = catalog
        .stage_audited_reconcile(
            namespace,
            &snapshot,
            &std::env::temp_dir(),
            volumes(),
            std::slice::from_ref(&binding),
        )
        .await
        .unwrap();
    assert_eq!((staged.paths, staged.audit.tombstones), (2, 2));
    catalog
        .confirm_bound_staged_import(namespace)
        .await
        .unwrap();
    assert!(
        catalog
            .verify_materialized_tombstone(namespace, 0)
            .await
            .is_err()
    );
    let plan = catalog
        .materialize_staged_tombstone(namespace, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (plan.path.as_str(), plan.version, plan.logical_file_uid),
        (path, (2, 1), 4)
    );
    assert_eq!(
        ManagedCatalog::open(Some(&cluster))
            .unwrap()
            .verify_materialized_tombstone(namespace, 0)
            .await
            .unwrap(),
        Some(plan.clone())
    );
    assert_eq!(
        catalog
            .materialize_staged_tombstone(namespace, 0)
            .await
            .unwrap(),
        Some(plan)
    );
    assert!(
        catalog
            .resolve_path(namespace, path.as_bytes())
            .await
            .is_err()
    );
    let mut key = b"tape-rs/catalog/22/t/".to_vec();
    key.extend_from_slice(namespace.as_bytes());
    key.extend_from_slice(pool.as_bytes());
    key.extend_from_slice(&Sha256::digest(path.as_bytes()));
    let direct = foundationdb::Database::new(Some(&cluster)).unwrap();
    let tr = direct.create_trx().unwrap();
    let value = tr.get(&key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&key);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .verify_materialized_tombstone(namespace, 0)
            .await
            .is_err()
    );
    let tr = direct.create_trx().unwrap();
    tr.set(&key, &value);
    tr.commit().await.unwrap();
    assert!(catalog.freeze_bound_staged_import(namespace).await.is_err());
    assert_eq!(
        catalog
            .materialize_bound_import_page(namespace, 1)
            .await
            .unwrap(),
        tape_rs::managed_catalog::ImportProjectionProgress { next: 1, total: 2 }
    );
    assert!(catalog.freeze_bound_staged_import(namespace).await.is_err());
    let mut cursor_key = b"tape-rs/catalog/22/u/".to_vec();
    cursor_key.extend_from_slice(namespace.as_bytes());
    let tr = direct.create_trx().unwrap();
    let cursor = tr.get(&cursor_key, false).await.unwrap().unwrap().to_vec();
    let mut corrupted = cursor.clone();
    corrupted[40] ^= 1;
    tr.set(&cursor_key, &corrupted);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .materialize_bound_import_page(namespace, 1)
            .await
            .is_err()
    );
    let tr = direct.create_trx().unwrap();
    tr.set(&cursor_key, &cursor);
    tr.commit().await.unwrap();
    let mut second_key = b"tape-rs/catalog/22/t/".to_vec();
    second_key.extend_from_slice(namespace.as_bytes());
    second_key.extend_from_slice(pool.as_bytes());
    second_key.extend_from_slice(&Sha256::digest(b"/gone2"));
    let tr = direct.create_trx().unwrap();
    tr.set(&second_key, b"conflicting tombstone");
    tr.commit().await.unwrap();
    assert!(
        catalog
            .materialize_bound_import_page(namespace, 1)
            .await
            .is_err()
    );
    let tr = direct.create_trx().unwrap();
    assert_eq!(
        tr.get(&cursor_key, false).await.unwrap().unwrap().as_ref(),
        cursor.as_slice()
    );
    tr.clear(&second_key);
    tr.commit().await.unwrap();
    assert_eq!(
        ManagedCatalog::open(Some(&cluster))
            .unwrap()
            .materialize_bound_import_page(namespace, 1)
            .await
            .unwrap(),
        tape_rs::managed_catalog::ImportProjectionProgress { next: 2, total: 2 }
    );
    assert!(
        catalog
            .materialize_bound_import_page(namespace, 1)
            .await
            .unwrap()
            .complete()
    );
    assert_eq!(
        catalog.freeze_bound_staged_import(namespace).await.unwrap(),
        NamespaceVerificationOutcome::Frozen
    );
    loop {
        match catalog.verify_namespace_page(namespace, 2).await.unwrap() {
            NamespaceVerificationOutcome::Progress { .. } => continue,
            NamespaceVerificationOutcome::Verified(summary) => {
                assert_eq!(summary.files, 0);
                break;
            }
            other => panic!("墓碑命名空间验证失败: {other:?}"),
        }
    }
    let tr = direct.create_trx().unwrap();
    tr.clear(&key);
    tr.commit().await.unwrap();
    assert!(catalog.verify_bound_import_copies(namespace).await.is_err());
    let tr = direct.create_trx().unwrap();
    tr.set(&key, &value);
    tr.commit().await.unwrap();
    let mut extra_key = b"tape-rs/catalog/22/t/".to_vec();
    extra_key.extend_from_slice(namespace.as_bytes());
    extra_key.extend_from_slice(pool.as_bytes());
    extra_key.extend_from_slice(&Sha256::digest(b"/orphan"));
    let tr = direct.create_trx().unwrap();
    tr.set(&extra_key, &value);
    tr.commit().await.unwrap();
    assert!(catalog.verify_bound_import_copies(namespace).await.is_err());
    let tr = direct.create_trx().unwrap();
    tr.clear(&extra_key);
    tr.commit().await.unwrap();
    assert_eq!(
        catalog.verify_bound_import_copies(namespace).await.unwrap(),
        0
    );
    assert!(catalog.seal_bound_staged_import(namespace).await.is_err());
    assert!(catalog.seal_import(namespace).await.is_err());
    assert!(catalog.discard_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .retire_discarded_volume_scan(namespace, &serial, 10, index.volume_uuid, binding.scan)
            .await
            .unwrap(),
        VolumeOutcome::Retired
    );
    assert_eq!(
        catalog
            .release_discarded_tape(
                namespace,
                pool,
                &serial,
                barcode,
                Some(index.volume_uuid),
                10
            )
            .await
            .unwrap(),
        ControlOutcome::Unassigned
    );
    loop {
        if catalog
            .purge_discarded_namespace(namespace, 100)
            .await
            .unwrap()
            .complete
        {
            break;
        }
    }
    drop(db);
    drop(spool);
    for file in [spool_path, snapshot, cache_path] {
        fs::remove_file(file).unwrap();
    }
}

async fn staged_import_synthetic(project_file: bool, seal: bool) {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    assert!(catalog.begin_managed_import(namespace).await.unwrap());

    let mut index = LtfsIndex::empty(Uuid::new_v4(), "stage-source".into(), 'b');
    index.self_location.start_block = 5;
    index.root.files.push(FileNode {
        name: "one".into(),
        meta: NodeMeta {
            file_uid: 2,
            ..Default::default()
        },
        xattrs: vec![
            Xattr {
                key: XATTR_VERSION.into(),
                value: "1.1".into(),
                base64: false,
            },
            Xattr {
                key: "user.large".into(),
                value: "X".repeat(80_000),
                base64: false,
            },
        ],
        ..Default::default()
    });
    index.highest_file_uid = 2;
    let xml = index.to_xml().unwrap();
    let expected = ExpectedIndex {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.self_location,
        sha256: Sha256::digest(&xml).into(),
    };
    let spool_path = std::env::temp_dir().join(format!("tape-rs-stage-{}.db", namespace));
    ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
    let spool = ImportSpool::open(&spool_path, expected).unwrap();
    let mut older = LtfsIndex::empty(Uuid::new_v4(), "stage-older".into(), 'b');
    older.self_location.start_block = 5;
    older.root.files.push(FileNode {
        name: "one".into(),
        meta: NodeMeta {
            file_uid: 4,
            ..Default::default()
        },
        xattrs: vec![Xattr {
            key: XATTR_VERSION.into(),
            value: "0.9".into(),
            base64: false,
        }],
        ..Default::default()
    });
    older.highest_file_uid = 4;
    let older_xml = older.to_xml().unwrap();
    let older_expected = ExpectedIndex {
        volume_uuid: older.volume_uuid,
        generation: older.generation,
        location: older.self_location,
        sha256: Sha256::digest(&older_xml).into(),
    };
    let older_path = std::env::temp_dir().join(format!("tape-rs-stage-older-{}.db", namespace));
    ImportSpool::build(older_xml.as_ref(), &older_path, older_expected).unwrap();
    let older_spool = ImportSpool::open(&older_path, older_expected).unwrap();
    let snapshot = std::env::temp_dir().join(format!("tape-rs-stage-{}.snap.db", namespace));
    let scratch_dir = std::env::temp_dir();
    let pool = Uuid::new_v4();
    let barcode = "STAGE01L8";
    let db = rusqlite::Connection::open(&snapshot).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
         CREATE TABLE pools(uuid TEXT PRIMARY KEY,name TEXT NOT NULL,file_limit INTEGER NOT NULL);
         CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
           volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL,
           files INTEGER NOT NULL DEFAULT 0,bytes_used INTEGER NOT NULL DEFAULT 0,
           bytes_written INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
           generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
           ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
           metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
         CREATE TABLE staging(barcode TEXT NOT NULL);
         CREATE TABLE file_tasks(pool_uuid TEXT NOT NULL,task INTEGER NOT NULL,
           round INTEGER NOT NULL,path TEXT NOT NULL,status TEXT NOT NULL,
           generation INTEGER,reason TEXT NOT NULL DEFAULT '');
         INSERT INTO meta(k,v) VALUES('applied_index',92),('catalog_frozen_index',90);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO pools(uuid,name,file_limit) VALUES(?1,'stage',1000000)",
        [pool.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            barcode,
            pool.to_string(),
            index.volume_uuid.to_string(),
            index.generation as i64
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            "STAGE00L8",
            pool.to_string(),
            older.volume_uuid.to_string(),
            older.generation as i64
        ],
    )
    .unwrap();
    for record in tape_rs::daemon::files::catalog_of(&index).0 {
        db.execute(
            "INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                pool.to_string(), record.path, barcode, index.generation as i64,
                record.length as i64, record.sha256, record.version.0 as i64,
                record.version.1 as i64, i64::from(record.deleted), record.metadata.to_string(),
            ],
        )
        .unwrap();
    }
    let volumes = || {
        vec![
            ImportVolume {
                pool_id: pool,
                barcode: barcode.into(),
                volume_id: index.volume_uuid,
                generation: index.generation,
                spool: &spool,
            },
            ImportVolume {
                pool_id: pool,
                barcode: "STAGE00L8".into(),
                volume_id: older.volume_uuid,
                generation: older.generation,
                spool: &older_spool,
            },
        ]
    };
    let serial = format!("STAGE-{}", namespace);
    assert_eq!(
        catalog
            .register_physical_library(namespace, &serial)
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert_eq!(
        catalog
            .create_pool(
                namespace,
                &PoolRecord {
                    library_serial: serial.clone(),
                    id: pool,
                    name: "stage".into(),
                    file_limit: 1_000_000,
                    policy: PoolPolicy::default(),
                    revision: 1,
                },
            )
            .await
            .unwrap(),
        ControlOutcome::Created
    );
    assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
    let (new_binding, new_cache) = register_staged_source(
        &catalog,
        VolumePlacement {
            volume_id: index.volume_uuid,
            namespace,
            pool_id: pool,
            library_serial: serial.clone(),
            barcode: barcode.into(),
        },
        xml.as_ref(),
        expected,
        &spool_path,
    )
    .await;
    let (old_binding, old_cache) = register_staged_source(
        &catalog,
        VolumePlacement {
            volume_id: older.volume_uuid,
            namespace,
            pool_id: pool,
            library_serial: serial.clone(),
            barcode: "STAGE00L8".into(),
        },
        older_xml.as_ref(),
        older_expected,
        &older_path,
    )
    .await;
    let bindings = vec![new_binding, old_binding];
    let mut revision_key = b"tape-rs/catalog/22/v/".to_vec();
    revision_key.extend_from_slice(namespace.as_bytes());
    let revision_db = foundationdb::Database::new(Some(&cluster)).unwrap();
    let tr = revision_db.create_trx().unwrap();
    assert_eq!(
        tr.get(&revision_key, false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        4u64.to_be_bytes().as_slice(),
        "两个受管卷各 Begin/Ready 一次应推进扫描修订号"
    );
    assert!(
        catalog
            .stage_audited_reconcile(
                namespace,
                &snapshot,
                &scratch_dir,
                volumes(),
                &bindings[..1]
            )
            .await
            .is_err()
    );
    let mut wrong_bindings = bindings.clone();
    wrong_bindings[1].source.sha256[0] ^= 1;
    assert!(
        catalog
            .stage_audited_reconcile(
                namespace,
                &snapshot,
                &scratch_dir,
                volumes(),
                &wrong_bindings
            )
            .await
            .is_err()
    );
    wrong_bindings = bindings.clone();
    wrong_bindings[1].scan.id = Uuid::new_v4();
    assert!(
        catalog
            .stage_audited_reconcile(
                namespace,
                &snapshot,
                &scratch_dir,
                volumes(),
                &wrong_bindings
            )
            .await
            .is_err()
    );
    wrong_bindings = bindings.clone();
    wrong_bindings[1] = wrong_bindings[0].clone();
    assert!(
        catalog
            .stage_audited_reconcile(
                namespace,
                &snapshot,
                &scratch_dir,
                volumes(),
                &wrong_bindings
            )
            .await
            .is_err()
    );
    db.execute("INSERT INTO staging(barcode) VALUES('STAGE01L8')", [])
        .unwrap();
    assert!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .is_err()
    );
    db.execute("DELETE FROM staging", []).unwrap();
    db.execute(
        "INSERT INTO file_tasks(pool_uuid,task,round,path,status) VALUES(?1,1,1,'/late','indeterminate')",
        [pool.to_string()],
    )
        .unwrap();
    assert!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .is_err()
    );
    assert!(
        catalog
            .staged_import_summary(namespace)
            .await
            .unwrap()
            .is_none()
    );
    db.execute("DELETE FROM file_tasks", []).unwrap();
    db.execute("DELETE FROM meta WHERE k='catalog_frozen_index'", [])
        .unwrap();
    assert!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .is_err()
    );
    db.execute(
        "INSERT INTO meta(k,v) VALUES('catalog_frozen_index',90)",
        [],
    )
    .unwrap();
    let staged = catalog
        .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
        .await
        .unwrap();
    assert_eq!((staged.paths, staged.audit.paths), (1, 1));
    assert_eq!(staged.audit.frozen_index, 90);
    let snapshot_digest: [u8; 32] = Sha256::digest(fs::read(&snapshot).unwrap()).into();
    assert_eq!(staged.audit.snapshot_sha256, snapshot_digest);
    assert!(staged.complete);
    assert_eq!(
        catalog.staged_import_summary(namespace).await.unwrap(),
        Some(staged)
    );
    assert_eq!(
        catalog.verify_staged_import(namespace).await.unwrap(),
        staged
    );
    let sources = catalog
        .verify_staged_source_bindings(namespace)
        .await
        .unwrap();
    assert_eq!((sources.staged, sources.volumes), (staged, 2));
    assert_eq!(
        ManagedCatalog::open(Some(&cluster))
            .unwrap()
            .verify_staged_source_bindings(namespace)
            .await
            .unwrap(),
        sources
    );
    assert!(catalog.confirm_staged_import(namespace).await.is_err());
    assert!(catalog.staged_data_file_plan(namespace, 0).await.is_err());
    assert!(catalog.freeze_bound_staged_import(namespace).await.is_err());
    assert!(
        catalog
            .materialize_staged_directory(namespace, 0)
            .await
            .is_err()
    );
    assert_eq!(
        ManagedCatalog::open(Some(&cluster))
            .unwrap()
            .confirm_bound_staged_import(namespace)
            .await
            .unwrap(),
        sources
    );
    assert_eq!(
        catalog
            .materialize_staged_directory(namespace, 0)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        catalog
            .bound_staged_import_summary(namespace)
            .await
            .unwrap(),
        sources
    );
    let plan = ManagedCatalog::open(Some(&cluster))
        .unwrap()
        .staged_data_file_plan(namespace, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.head.version, 2);
    assert_eq!(plan.copies.len(), 2);
    assert_eq!(
        plan.copies
            .iter()
            .filter(|copy| copy.id == plan.head.copy_id)
            .count(),
        1
    );
    for copy in &plan.copies {
        assert!(copy.copy.media.is_some());
        assert_eq!(copy.copy.media.as_ref().unwrap().physical_path, "one");
        assert_eq!(copy.binding.scan.id, copy.copy.volume_scan_id);
    }
    assert!(
        catalog
            .staged_row_sha256(namespace, 0)
            .await
            .unwrap()
            .is_some()
    );
    let expected_path = ImportReconciler::new(volumes())
        .unwrap()
        .next_path()
        .unwrap()
        .unwrap();
    assert_eq!(
        catalog.staged_path(namespace, 0).await.unwrap(),
        Some(expected_path)
    );
    let staged_entry = catalog.staged_entry(namespace, 0).await.unwrap().unwrap();
    assert_eq!(
        staged_entry.path,
        catalog.staged_path(namespace, 0).await.unwrap().unwrap()
    );
    assert_eq!(
        staged_entry.current_attributes,
        spool.node_by_path("one").unwrap().unwrap().attributes
    );
    assert_eq!(staged_entry.source_file_uids, vec![2, 4]);
    assert_eq!(
        staged_entry.source_data,
        Some(vec![
            StagedSourceData::Data {
                file_uid: 2,
                length: 0,
                content_digest: ContentDigest::Unknown,
                ee_alias: false,
                data_path: None,
            },
            StagedSourceData::Data {
                file_uid: 4,
                length: 0,
                content_digest: ContentDigest::Unknown,
                ee_alias: false,
                data_path: None,
            },
        ])
    );
    assert!(
        catalog
            .staged_row_sha256(namespace, 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        catalog.namespace_state(namespace).await.unwrap(),
        Some(NamespaceState::Building)
    );
    assert!(catalog.resolve_path(namespace, b"/one").await.is_err());
    assert_eq!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .unwrap(),
        staged
    );
    if project_file {
        assert!(
            catalog
                .verify_materialized_data_file(namespace, 0)
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .materialize_staged_data_file(namespace, 0)
                .await
                .unwrap(),
            Some(plan.id)
        );
        assert_eq!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .materialize_staged_data_file(namespace, 0)
                .await
                .unwrap(),
            Some(plan.id)
        );
        assert_eq!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .verify_materialized_data_file(namespace, 0)
                .await
                .unwrap(),
            Some(plan.id)
        );
        let mut unexpected_copy = b"tape-rs/catalog/22/R/".to_vec();
        unexpected_copy.extend_from_slice(namespace.as_bytes());
        unexpected_copy.extend_from_slice(plan.id.as_bytes());
        let unexpected_id = Uuid::new_v4();
        assert!(plan.copies.iter().all(|copy| copy.id != unexpected_id));
        unexpected_copy.extend_from_slice(unexpected_id.as_bytes());
        let direct = foundationdb::Database::new(Some(&cluster)).unwrap();
        let tr = direct.create_trx().unwrap();
        tr.set(&unexpected_copy, b"unexpected");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .verify_materialized_data_file(namespace, 0)
                .await
                .is_err()
        );
        let tr = direct.create_trx().unwrap();
        tr.clear(&unexpected_copy);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .verify_materialized_data_file(namespace, 0)
                .await
                .unwrap(),
            Some(plan.id)
        );
        assert!(catalog.freeze_bound_staged_import(namespace).await.is_err());
        assert!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .materialize_bound_import_page(namespace, 1)
                .await
                .unwrap()
                .complete()
        );
        assert_eq!(
            catalog.freeze_bound_staged_import(namespace).await.unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        loop {
            match catalog.verify_namespace_page(namespace, 2).await.unwrap() {
                NamespaceVerificationOutcome::Progress { .. } => continue,
                NamespaceVerificationOutcome::Verified(summary) => {
                    assert_eq!(summary.files, 1);
                    break;
                }
                other => panic!("导入投影全局核验失败: {other:?}"),
            }
        }
        assert!(matches!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .freeze_bound_staged_import(namespace)
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Verified(_)
        ));
        assert!(catalog.seal_bound_staged_import(namespace).await.is_err());
        let old_copy = plan
            .copies
            .iter()
            .find(|copy| copy.id != plan.head.copy_id)
            .unwrap();
        let mut old_key = b"tape-rs/catalog/22/R/".to_vec();
        old_key.extend_from_slice(namespace.as_bytes());
        old_key.extend_from_slice(plan.id.as_bytes());
        old_key.extend_from_slice(old_copy.id.as_bytes());
        let tr = direct.create_trx().unwrap();
        let old_value = tr.get(&old_key, false).await.unwrap().unwrap().to_vec();
        tr.clear(&old_key);
        tr.commit().await.unwrap();
        assert!(catalog.verify_bound_import_copies(namespace).await.is_err());
        let tr = direct.create_trx().unwrap();
        tr.set(&old_key, &old_value);
        tr.commit().await.unwrap();
        let mut old_reverse = b"tape-rs/catalog/22/V/".to_vec();
        old_reverse.extend_from_slice(old_copy.copy.volume_id.as_bytes());
        old_reverse.extend_from_slice(old_copy.copy.volume_scan_id.as_bytes());
        old_reverse.extend_from_slice(namespace.as_bytes());
        old_reverse.extend_from_slice(plan.id.as_bytes());
        old_reverse.extend_from_slice(old_copy.id.as_bytes());
        let tr = direct.create_trx().unwrap();
        let reverse_value = tr.get(&old_reverse, false).await.unwrap().unwrap().to_vec();
        tr.clear(&old_reverse);
        tr.commit().await.unwrap();
        assert!(catalog.verify_bound_import_copies(namespace).await.is_err());
        let tr = direct.create_trx().unwrap();
        tr.set(&old_reverse, &reverse_value);
        tr.commit().await.unwrap();
        let mut orphan_reverse = old_reverse[..old_reverse.len() - 16].to_vec();
        orphan_reverse.extend_from_slice(unexpected_id.as_bytes());
        let tr = direct.create_trx().unwrap();
        tr.set(&orphan_reverse, &reverse_value);
        tr.commit().await.unwrap();
        assert!(catalog.verify_bound_import_copies(namespace).await.is_err());
        let tr = direct.create_trx().unwrap();
        tr.clear(&orphan_reverse);
        tr.commit().await.unwrap();
        assert_eq!(
            ManagedCatalog::open(Some(&cluster))
                .unwrap()
                .verify_bound_import_copies(namespace)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            catalog.verify_bound_import_copies(namespace).await.unwrap(),
            2
        );
        catalog
            .inspect_bound_import_copy_fence(namespace)
            .await
            .unwrap();
        let committed_cache =
            std::env::temp_dir().join(format!("tape-rs-active-delta-{}.db", Uuid::new_v4()));
        if seal {
            assert!(catalog.seal_import(namespace).await.is_err());
            assert!(
                catalog
                    .prepare_library_route(&serial, namespace, None)
                    .await
                    .is_err()
            );
            assert!(catalog.seal_bound_staged_import(namespace).await.unwrap());
            assert_eq!(
                catalog.namespace_state(namespace).await.unwrap(),
                Some(NamespaceState::Sealed)
            );
            assert_eq!(
                catalog
                    .create_directory(namespace, Uuid::nil(), b"late", Uuid::new_v4())
                    .await
                    .unwrap(),
                DirectoryOutcome::NamespaceNotReady
            );
            assert!(
                catalog
                    .create_pool(
                        namespace,
                        &PoolRecord {
                            library_serial: serial.clone(),
                            id: Uuid::new_v4(),
                            name: "late".into(),
                            file_limit: 1,
                            policy: PoolPolicy::default(),
                            revision: 1,
                        }
                    )
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .begin_volume_reconcile(
                        &serial,
                        10,
                        bindings[0].placement.volume_id,
                        VolumeScan {
                            generation: bindings[0].scan.generation,
                            id: Uuid::new_v4(),
                        }
                    )
                    .await
                    .is_err()
            );
            let route = PreparedLibraryRoute {
                namespace,
                revision: 1,
            };
            assert_eq!(
                catalog
                    .prepare_library_route(&serial, namespace, None)
                    .await
                    .unwrap(),
                PrepareRouteOutcome::Prepared(route)
            );
            let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
            assert_eq!(
                reopened.prepared_library_route(&serial).await.unwrap(),
                Some(route)
            );
            let node72 = snapshot.with_extension("node72.db");
            let node73 = snapshot.with_extension("node73.db");
            let bad_node = snapshot.with_extension("bad-node.db");
            for (path, applied) in [(&node72, 93), (&node73, 94)] {
                fs::copy(&snapshot, path).unwrap();
                let copy = rusqlite::Connection::open(path).unwrap();
                copy.execute("UPDATE meta SET v=?1 WHERE k='applied_index'", [applied])
                    .unwrap();
            }
            fs::copy(&snapshot, &bad_node).unwrap();
            rusqlite::Connection::open(&bad_node)
                .unwrap()
                .execute("UPDATE pools SET file_limit=file_limit+1", [])
                .unwrap();
            let proof = |node_id, path: &std::path::Path| {
                let identity = inspect_frozen_snapshot(path).unwrap();
                verify_node_export(
                    path,
                    node_id,
                    identity.applied_index,
                    identity.frozen_index,
                    identity.snapshot_sha256,
                )
                .unwrap()
            };
            let evidence = [proof(71, &snapshot), proof(72, &node72), proof(73, &node73)];
            assert_eq!(evidence[0].identity().frozen_index, 90);
            assert_eq!(
                evidence[0].identity().catalog_sha256,
                evidence[2].identity().catalog_sha256
            );
            assert!(verify_node_export(&snapshot, 71, 92, 90, [0; 32]).is_err());
            assert!(
                catalog
                    .certify_prepared_cutover(&serial, route, 72, evidence)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .certify_prepared_cutover(
                        &serial,
                        route,
                        71,
                        [evidence[0], evidence[1], proof(73, &bad_node)]
                    )
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .certify_prepared_cutover(&serial, route, 71, evidence)
                    .await
                    .unwrap()
            );
            assert!(
                !catalog
                    .certify_prepared_cutover(&serial, route, 71, evidence)
                    .await
                    .unwrap()
            );
            let certificate = reopened
                .certified_prepared_cutover(&serial)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(certificate.route, route);
            assert_eq!(certificate.selected_node, 71);
            assert_eq!(certificate.frozen_index, 90);
            assert_eq!(certificate.nodes.map(|node| node.node_id), [71, 72, 73]);
            let mut source_revision_key = b"tape-rs/catalog/22/v/".to_vec();
            source_revision_key.extend_from_slice(namespace.as_bytes());
            let tr = direct.create_trx().unwrap();
            let source_revision = tr
                .get(&source_revision_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            let mut changed_revision = source_revision.clone();
            changed_revision[7] ^= 1;
            tr.set(&source_revision_key, &changed_revision);
            tr.commit().await.unwrap();
            assert!(reopened.prepared_library_route(&serial).await.is_err());
            assert!(reopened.certified_prepared_cutover(&serial).await.is_err());
            let tr = direct.create_trx().unwrap();
            tr.set(&source_revision_key, &source_revision);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened.prepared_library_route(&serial).await.unwrap(),
                Some(route)
            );
            assert_eq!(reopened.active_library_route(&serial).await.unwrap(), None);
            assert!(
                reopened
                    .activate_prepared_cutover(
                        &serial,
                        tape_rs::managed_catalog::CutoverCertificate {
                            selected_node: 72,
                            ..certificate
                        }
                    )
                    .await
                    .is_err()
            );
            let active = reopened
                .activate_prepared_cutover(&serial, certificate)
                .await
                .unwrap();
            assert_eq!(active.route, route);
            assert_eq!(active.frozen_index, 90);
            assert_eq!(active.initial_owner_epoch, 11);
            assert_eq!(
                catalog.namespace_state(namespace).await.unwrap(),
                Some(NamespaceState::Active)
            );
            assert!(catalog.discard_import(namespace).await.is_err());
            let mut active_request = CommitReceipt {
                namespace,
                parent: Uuid::nil(),
                name: b"active-admission-only".to_vec(),
                file_id: Uuid::new_v4(),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                request_epoch: 1,
                copy_id: Uuid::new_v4(),
                library_serial: serial.clone(),
                pool_id: Some(pool),
                owner_epoch: 11,
                volume_id: bindings[0].placement.volume_id,
                volume_generation: bindings[0].scan.generation + 1,
                volume_scan_id: bindings[0].scan.id,
                ltfs_file_uid: 42,
                size: 0,
                content_digest: ContentDigest::Unknown,
                metadata_digest: None,
            };
            active_request.copy_id = derive_copy_id(&active_request);
            let reservation = ActiveFileIntent {
                namespace,
                parent: active_request.parent,
                parent_path: b"/".to_vec(),
                name: b"reserved-before-data".to_vec(),
                file_id: Uuid::new_v4(),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                request_epoch: 1,
                library_serial: serial.clone(),
                pool_id: Some(pool),
                owner_epoch: 11,
                volume_id: active_request.volume_id,
                volume_generation: active_request.volume_generation,
                volume_scan_id: active_request.volume_scan_id,
                expected_size: 3,
                expected_digest: ContentDigest::Sha256([7; 32]),
            };
            assert_eq!(
                catalog.reserve_active_file(&reservation).await.unwrap(),
                PublishOutcome::Registered
            );
            assert_eq!(
                catalog
                    .pending_active_file_reservations(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec![reservation.clone()]
            );
            assert_eq!(
                catalog.reserve_active_file(&reservation).await.unwrap(),
                PublishOutcome::Registered
            );
            let mut competing_reservation = reservation.clone();
            competing_reservation.operation_id = Uuid::new_v4();
            competing_reservation.file_id = Uuid::new_v4();
            let mut wrong_parent_path = competing_reservation.clone();
            wrong_parent_path.parent_path = b"/missing-parent".to_vec();
            assert_eq!(
                catalog
                    .reserve_active_file(&wrong_parent_path)
                    .await
                    .unwrap(),
                PublishOutcome::MissingParent
            );
            assert_eq!(
                catalog
                    .reserve_active_file(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::NameConflict
            );
            let mut physical = active_request.clone();
            physical.name = reservation.name.clone();
            physical.file_id = reservation.file_id;
            physical.operation_id = reservation.operation_id;
            physical.size = reservation.expected_size;
            physical.content_digest = reservation.expected_digest;
            physical.copy_id = derive_copy_id(&physical);
            let mut wrong_physical = physical.clone();
            wrong_physical.size += 1;
            assert!(catalog.complete_active_file(&wrong_physical).await.is_err());
            assert_eq!(
                catalog.complete_active_file(&physical).await.unwrap(),
                PublishOutcome::Registered
            );
            assert_eq!(
                catalog.register_active_request(&physical).await.unwrap(),
                PublishOutcome::Registered
            );
            assert_eq!(
                catalog.complete_active_file(&physical).await.unwrap(),
                PublishOutcome::Registered
            );
            let mut missing = physical.clone();
            missing.operation_id = Uuid::new_v4();
            missing.file_id = Uuid::new_v4();
            missing.name = b"not-reserved".to_vec();
            missing.copy_id = derive_copy_id(&missing);
            let rejected_reservation_batch = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(rejected_reservation_batch, &[physical.clone(), missing])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::RequestConflict
            );
            assert_eq!(
                catalog
                    .pending_active_file_reservations(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec![reservation.clone()]
            );
            assert_eq!(
                catalog
                    .abort_open_active_batch(
                        &serial,
                        physical.volume_id,
                        physical.volume_generation,
                        rejected_reservation_batch,
                        11,
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Aborted
            );
            assert_eq!(
                catalog
                    .cancel_active_file_reservation(&reservation)
                    .await
                    .unwrap(),
                PublishOutcome::BatchClaimed
            );
            assert_eq!(
                catalog.cancel_request(&physical).await.unwrap(),
                PublishOutcome::RequestCancelled
            );
            assert!(
                catalog
                    .pending_active_file_reservations(namespace, None, 10)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert_eq!(
                catalog
                    .reserve_active_file(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::Registered
            );
            assert_eq!(
                catalog
                    .cancel_active_file_reservation(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::RequestCancelled
            );
            assert_eq!(
                catalog
                    .cancel_active_file_reservation(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::RequestCancelled
            );
            assert_eq!(
                catalog
                    .reserve_active_file(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::RequestCancelled
            );
            competing_reservation.expected_size += 1;
            assert_eq!(
                catalog
                    .reserve_active_file(&competing_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::OperationConflict
            );
            let mut old_generation_request = active_request.clone();
            old_generation_request.volume_generation = bindings[0].scan.generation;
            old_generation_request.operation_id = Uuid::new_v4();
            old_generation_request.copy_id = derive_copy_id(&old_generation_request);
            assert_eq!(
                catalog
                    .register_active_request(&old_generation_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::GenerationConflict
            );
            let mut future_generation_request = active_request.clone();
            future_generation_request.volume_generation += 1;
            future_generation_request.operation_id = Uuid::new_v4();
            future_generation_request.copy_id = derive_copy_id(&future_generation_request);
            assert_eq!(
                catalog
                    .register_active_request(&future_generation_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::VolumeNotReady
            );
            assert!(catalog.register_request(&active_request).await.is_err());
            let mut old_owner_request = active_request.clone();
            old_owner_request.owner_epoch = 10;
            assert_eq!(
                catalog
                    .register_active_request(&old_owner_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::StaleOwner
            );
            assert_eq!(
                catalog
                    .register_active_request(&active_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            assert_eq!(
                catalog
                    .pending_requests(namespace, None, 2)
                    .await
                    .unwrap()
                    .items
                    .len(),
                1
            );
            assert_eq!(
                catalog.cancel_request(&old_owner_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::StaleOwner
            );
            assert_eq!(
                catalog.cancel_request(&active_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            assert!(
                catalog
                    .pending_requests(namespace, None, 2)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            let mut fenced_reservation = reservation.clone();
            fenced_reservation.name = b"survives-owner-takeover".to_vec();
            fenced_reservation.file_id = Uuid::new_v4();
            fenced_reservation.operation_id = Uuid::new_v4();
            assert_eq!(
                catalog
                    .reserve_active_file(&fenced_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::Registered
            );
            assert!(catalog.advance_owner_epoch(&serial, 12).await.unwrap());
            assert_eq!(
                catalog
                    .cancel_active_file_reservation(&fenced_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::StaleOwner
            );
            fenced_reservation.owner_epoch = 12;
            let mut takeover_request = active_request.clone();
            takeover_request.operation_id = Uuid::new_v4();
            takeover_request.owner_epoch = 12;
            takeover_request.copy_id = derive_copy_id(&takeover_request);
            assert_eq!(
                catalog
                    .register_active_request(&active_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::StaleOwner
            );
            assert_eq!(
                catalog
                    .register_active_request(&takeover_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            assert!(catalog.rotate_request_epoch(namespace, 1).await.is_err());
            assert!(
                catalog
                    .rotate_active_request_epoch(namespace, &serial, 11, 1)
                    .await
                    .is_err()
            );
            assert!(matches!(
                catalog
                    .rotate_active_request_epoch(namespace, &serial, 12, 1)
                    .await
                    .unwrap(),
                WindowOutcome::Advanced(window) if window.accepting == 2
            ));
            assert_eq!(
                catalog
                    .advance_active_request_floor(namespace, &serial, 12, 1, 2)
                    .await
                    .unwrap(),
                WindowOutcome::PendingRequests
            );
            assert_eq!(
                catalog.cancel_request(&takeover_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            assert_eq!(
                catalog
                    .advance_active_request_floor(namespace, &serial, 12, 1, 2)
                    .await
                    .unwrap(),
                WindowOutcome::PendingRequests
            );
            assert!(
                catalog
                    .advance_request_floor(namespace, 1, 2)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .advance_active_request_floor(namespace, &serial, 11, 1, 2)
                    .await
                    .is_err()
            );
            assert_eq!(
                catalog
                    .cancel_active_file_reservation(&fenced_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::RequestCancelled
            );
            assert!(matches!(
                catalog
                    .advance_active_request_floor(namespace, &serial, 12, 1, 2)
                    .await
                    .unwrap(),
                WindowOutcome::Advanced(window) if window.first_retained == 2
            ));
            assert!(
                catalog
                    .purge_expired_requests(namespace, 100)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .purge_active_expired_requests(namespace, &serial, 11, 100)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .purge_active_expired_requests(namespace, &serial, 12, 100)
                    .await
                    .unwrap()
                    > 0
            );
            assert_eq!(
                catalog
                    .register_active_request(&takeover_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestExpired
            );
            let mut next_epoch_request = takeover_request.clone();
            next_epoch_request.request_epoch = 2;
            next_epoch_request.operation_id = Uuid::new_v4();
            next_epoch_request.copy_id = derive_copy_id(&next_epoch_request);
            assert_eq!(
                catalog
                    .register_active_request(&next_epoch_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            assert_eq!(
                catalog.cancel_request(&next_epoch_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            let mut committed_request = next_epoch_request.clone();
            committed_request.operation_id = Uuid::new_v4();
            let empty_sha = Sha256::digest(b"");
            committed_request.content_digest = ContentDigest::Sha256(empty_sha.into());
            let committed_attributes = NodeAttributes::new(
                NodeMeta {
                    file_uid: committed_request.ltfs_file_uid,
                    ..Default::default()
                },
                vec![
                    Xattr {
                        key: "user.large".into(),
                        value: "X".repeat(80_000),
                        base64: false,
                    },
                    Xattr {
                        key: tape_rs::ltfs::index::XATTR_SHA256.into(),
                        value: format!("{empty_sha:x}"),
                        base64: false,
                    },
                ],
                None,
            )
            .unwrap();
            committed_request.metadata_digest = Some(committed_attributes.digest());
            committed_request.copy_id = derive_copy_id(&committed_request);
            let committed_reservation = ActiveFileIntent {
                namespace,
                parent: committed_request.parent,
                parent_path: b"/".to_vec(),
                name: committed_request.name.clone(),
                file_id: committed_request.file_id,
                expected_version: committed_request.expected_version,
                operation_id: committed_request.operation_id,
                request_epoch: committed_request.request_epoch,
                library_serial: serial.clone(),
                pool_id: committed_request.pool_id,
                owner_epoch: committed_request.owner_epoch,
                volume_id: committed_request.volume_id,
                volume_generation: committed_request.volume_generation,
                volume_scan_id: committed_request.volume_scan_id,
                expected_size: committed_request.size,
                expected_digest: committed_request.content_digest,
            };
            assert_eq!(
                catalog
                    .reserve_active_file(&committed_reservation)
                    .await
                    .unwrap(),
                PublishOutcome::Registered
            );
            assert_eq!(
                catalog
                    .complete_active_file(&committed_request)
                    .await
                    .unwrap(),
                PublishOutcome::Registered
            );
            let baseline_source = bindings[0].source;
            let baseline_index = ActiveIndexIdentity::from(baseline_source);
            let volume_id = bindings[0].placement.volume_id;
            let mut duplicate_path = committed_request.clone();
            duplicate_path.operation_id = Uuid::new_v4();
            duplicate_path.file_id = Uuid::new_v4();
            duplicate_path.copy_id = derive_copy_id(&duplicate_path);
            assert!(
                catalog
                    .stage_active_batch(
                        Uuid::new_v4(),
                        &[committed_request.clone(), duplicate_path]
                    )
                    .await
                    .is_err()
            );
            let mut unregistered = committed_request.clone();
            unregistered.operation_id = Uuid::from_bytes([0xff; 16]);
            unregistered.file_id = Uuid::new_v4();
            unregistered.name = b"not-admitted".to_vec();
            unregistered.copy_id = derive_copy_id(&unregistered);
            let rejected_batch = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(rejected_batch, &[committed_request.clone(), unregistered])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::RequestConflict
            );
            assert_eq!(
                catalog
                    .pending_active_file_reservations(namespace, None, 10)
                    .await
                    .unwrap()
                    .items,
                vec![committed_reservation.clone()]
            );
            let mut early_claim = b"tape-rs/catalog/22/p/i/".to_vec();
            early_claim.extend_from_slice(namespace.as_bytes());
            early_claim.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            early_claim.extend_from_slice(committed_request.operation_id.as_bytes());
            let tr = direct.create_trx().unwrap();
            assert!(tr.get(&early_claim, false).await.unwrap().is_none());
            assert_eq!(
                catalog
                    .abort_open_active_batch(
                        &serial,
                        volume_id,
                        committed_request.volume_generation,
                        rejected_batch,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Aborted
            );
            let batch_id = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(batch_id, &[committed_request.clone()])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Staged
            );
            assert!(
                catalog
                    .pending_active_file_reservations(namespace, None, 10)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert_eq!(
                reopened
                    .active_batch_status(&serial, volume_id, committed_request.volume_generation)
                    .await
                    .unwrap()
                    .unwrap()
                    .phase,
                ActiveBatchPhase::Sealed
            );
            let mut path_locator_key = b"tape-rs/catalog/22/p/l/".to_vec();
            path_locator_key.extend_from_slice(namespace.as_bytes());
            path_locator_key.extend_from_slice(committed_request.parent.as_bytes());
            path_locator_key.extend_from_slice(&committed_request.name);
            path_locator_key.extend_from_slice(&[0, 0]);
            let mut file_locator_key = b"tape-rs/catalog/22/p/n/".to_vec();
            file_locator_key.extend_from_slice(namespace.as_bytes());
            file_locator_key.extend_from_slice(committed_request.file_id.as_bytes());
            let mut reverse_locator_key = b"tape-rs/catalog/22/p/o/".to_vec();
            reverse_locator_key.extend_from_slice(volume_id.as_bytes());
            reverse_locator_key.extend_from_slice(namespace.as_bytes());
            reverse_locator_key.extend_from_slice(committed_request.file_id.as_bytes());
            reverse_locator_key.extend_from_slice(committed_request.copy_id.as_bytes());
            let tr = direct.create_trx().unwrap();
            let path_locator = tr
                .get(&path_locator_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            assert_eq!(path_locator.len(), 81);
            assert_eq!(path_locator[0], 1);
            assert_eq!(&path_locator[1..17], batch_id.as_bytes());
            assert_eq!(&path_locator[17..33], volume_id.as_bytes());
            assert_eq!(
                &path_locator[33..41],
                &committed_request.volume_generation.to_be_bytes()
            );
            assert_eq!(
                &path_locator[49..65],
                committed_request.operation_id.as_bytes()
            );
            assert_eq!(&path_locator[65..81], committed_request.file_id.as_bytes());
            assert_eq!(
                tr.get(&file_locator_key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                path_locator.as_slice()
            );
            let reverse_locator = tr.get(&reverse_locator_key, false).await.unwrap().unwrap();
            assert_eq!(reverse_locator.len(), 49);
            assert_eq!(&reverse_locator[1..17], batch_id.as_bytes());
            assert_eq!(
                &reverse_locator[17..25],
                &committed_request.volume_generation.to_be_bytes()
            );
            assert_eq!(
                &reverse_locator[33..49],
                committed_request.operation_id.as_bytes()
            );
            let mut competing = committed_request.clone();
            competing.operation_id = Uuid::new_v4();
            competing.file_id = Uuid::new_v4();
            competing.volume_id = bindings[1].placement.volume_id;
            competing.volume_generation = bindings[1].scan.generation + 1;
            competing.volume_scan_id = bindings[1].scan.id;
            competing.copy_id = derive_copy_id(&competing);
            assert_eq!(
                catalog.register_active_request(&competing).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            let competing_batch = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(competing_batch, &[competing.clone()])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::BatchConflict
            );
            assert_eq!(
                catalog
                    .abort_open_active_batch(
                        &serial,
                        competing.volume_id,
                        competing.volume_generation,
                        competing_batch,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Aborted
            );
            assert_eq!(
                catalog.cancel_request(&competing).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            let mut competing_inode = competing.clone();
            competing_inode.operation_id = Uuid::new_v4();
            competing_inode.file_id = committed_request.file_id;
            competing_inode.name = b"different-name-same-inode".to_vec();
            competing_inode.copy_id = derive_copy_id(&competing_inode);
            assert_eq!(
                catalog
                    .register_active_request(&competing_inode)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            let competing_inode_batch = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(competing_inode_batch, &[competing_inode.clone()])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::BatchConflict
            );
            assert_eq!(
                catalog
                    .abort_open_active_batch(
                        &serial,
                        competing_inode.volume_id,
                        competing_inode.volume_generation,
                        competing_inode_batch,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Aborted
            );
            assert_eq!(
                catalog.cancel_request(&competing_inode).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            assert_eq!(
                reopened
                    .stage_active_batch(batch_id, &[committed_request.clone()])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::AlreadyStaged
            );
            assert_eq!(
                catalog.cancel_request(&committed_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::BatchClaimed
            );
            assert_eq!(
                reopened
                    .register_active_request(&committed_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::BatchClaimed
            );
            assert_eq!(
                catalog
                    .abort_open_active_batch(
                        &serial,
                        volume_id,
                        committed_request.volume_generation,
                        batch_id,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::BatchConflict
            );
            // Simulate a crash after the page transaction but before sealing.
            let mut batch_key = b"tape-rs/catalog/22/p/b/".to_vec();
            batch_key.extend_from_slice(volume_id.as_bytes());
            batch_key.extend_from_slice(&committed_request.volume_generation.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let mut open_header = tr.get(&batch_key, false).await.unwrap().unwrap().to_vec();
            open_header[1] = ActiveBatchPhase::Open as u8;
            tr.set(&batch_key, &open_header);
            tr.commit().await.unwrap();
            let mut digest_key = b"tape-rs/catalog/22/p/h/".to_vec();
            digest_key.extend_from_slice(volume_id.as_bytes());
            digest_key.extend_from_slice(&committed_request.volume_generation.to_be_bytes());
            digest_key.extend_from_slice(&0_u16.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_digest = tr.get(&digest_key, false).await.unwrap().unwrap().to_vec();
            tr.clear(&digest_key);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .stage_active_batch(batch_id, &[committed_request.clone()])
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&digest_key, &saved_digest);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened
                    .abort_open_active_batch(
                        &serial,
                        volume_id,
                        committed_request.volume_generation,
                        batch_id,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Aborted
            );
            assert!(
                direct
                    .create_trx()
                    .unwrap()
                    .get(&reverse_locator_key, false)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                reopened
                    .abort_open_active_batch(
                        &serial,
                        volume_id,
                        committed_request.volume_generation,
                        batch_id,
                        12
                    )
                    .await
                    .unwrap(),
                ActiveBatchOutcome::AlreadyAborted
            );
            assert_eq!(
                catalog.cancel_request(&committed_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::RequestCancelled
            );
            committed_request.operation_id = Uuid::new_v4();
            committed_request.copy_id = derive_copy_id(&committed_request);
            assert_eq!(
                catalog
                    .register_active_request(&committed_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        batch_id,
                        baseline_index,
                        ActiveIndexIdentity {
                            generation: baseline_index.generation + 1,
                            ..baseline_index
                        }
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::BatchConflict
            );
            assert_eq!(
                catalog
                    .stage_active_batch(batch_id, &[committed_request.clone()])
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Staged
            );
            assert_eq!(
                catalog
                    .observed_active_volume_index(&serial, volume_id)
                    .await
                    .unwrap(),
                baseline_index
            );
            assert_eq!(
                catalog
                    .observed_active_volume_generation(
                        &serial,
                        volume_id,
                        baseline_index.generation,
                    )
                    .await
                    .unwrap(),
                Some(baseline_index)
            );
            assert_eq!(
                catalog
                    .observed_active_volume_generation(
                        &serial,
                        volume_id,
                        baseline_index.generation + 1,
                    )
                    .await
                    .unwrap(),
                None
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        committed_request.volume_generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                reopened
                    .prepare_observed_active_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                        &committed_attributes,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::IndexNotObserved
            );
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        committed_request.volume_generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::IndexNotObserved
            );
            let mut committed_view = index.clone();
            committed_view.generation = baseline_index.generation + 1;
            committed_view.self_location.start_block = baseline_index.location.start_block + 2;
            committed_view.highest_file_uid = committed_request.ltfs_file_uid;
            committed_view.root.files.push(FileNode {
                name: String::from_utf8(committed_request.name.clone()).unwrap(),
                meta: NodeMeta {
                    file_uid: committed_request.ltfs_file_uid,
                    ..Default::default()
                },
                ..Default::default()
            });
            let committed_delta = committed_view.incremental_since(&index).unwrap();
            let committed_delta_xml = committed_delta.to_xml().unwrap();
            let committed_expected = ExpectedIndex {
                volume_uuid: volume_id,
                generation: committed_view.generation,
                location: committed_view.self_location,
                sha256: Sha256::digest(&committed_delta_xml).into(),
            };
            assert_eq!(
                VolumeExtentCache::build_incremental(
                    committed_delta_xml.as_ref(),
                    &committed_cache,
                    committed_expected,
                )
                .unwrap()
                .files,
                1
            );
            let observed_index = ActiveIndexIdentity {
                generation: baseline_index.generation + 1,
                location: tape_rs::ltfs::index::IndexLocation {
                    start_block: baseline_index.location.start_block + 2,
                    ..baseline_index.location
                },
                sha256: committed_expected.sha256,
                incremental: true,
                ..baseline_index
            };
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        11,
                        volume_id,
                        batch_id,
                        baseline_index,
                        observed_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::StaleOwner
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        Uuid::new_v4(),
                        baseline_index,
                        observed_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::BatchConflict
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        batch_id,
                        baseline_index,
                        observed_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::Recorded
            );
            let recovery = reopened
                .recover_observed_active_batch_page(
                    &serial,
                    12,
                    volume_id,
                    observed_index.generation,
                    batch_id,
                    0,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recovery.index, observed_index);
            assert_eq!(recovery.receipts, vec![committed_request.clone()]);
            assert_eq!(recovery.next_page, None);
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::PageIncomplete
            );
            assert!(
                reopened
                    .recover_observed_active_prepared_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                reopened
                    .prepare_observed_active_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                        &committed_attributes,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::Prepared
            );
            assert_eq!(
                catalog
                    .prepare_observed_active_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                        &committed_attributes,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            assert_eq!(
                reopened
                    .recover_observed_active_prepared_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                Some(committed_attributes.clone())
            );
            let mut prepared_chunk = b"tape-rs/catalog/22/p/a/".to_vec();
            prepared_chunk.extend_from_slice(volume_id.as_bytes());
            prepared_chunk.extend_from_slice(&observed_index.generation.to_be_bytes());
            prepared_chunk.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            prepared_chunk.extend_from_slice(committed_request.operation_id.as_bytes());
            prepared_chunk.extend_from_slice(&1_u16.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_chunk = tr
                .get(&prepared_chunk, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            let mut damaged_chunk = saved_chunk.clone();
            damaged_chunk[0] ^= 1;
            tr.set(&prepared_chunk, &damaged_chunk);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .recover_observed_active_prepared_file(
                        &serial,
                        12,
                        batch_id,
                        &committed_request,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&prepared_chunk, &saved_chunk);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::Prepared
            );
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            assert_eq!(
                reopened
                    .seal_observed_active_preparation(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::Prepared
            );
            assert_eq!(
                catalog
                    .seal_observed_active_preparation(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            let committed_path = [b"/".as_slice(), committed_request.name.as_slice()].concat();
            assert_eq!(
                reopened
                    .resolve_active_path(&serial, &committed_path)
                    .await
                    .unwrap(),
                None
            );
            assert!(
                !reopened
                    .list_children(namespace, Uuid::nil(), None, 100)
                    .await
                    .unwrap()
                    .items
                    .iter()
                    .any(|child| child.name == committed_request.name)
            );
            assert!(
                !reopened
                    .copies_on_volume(volume_id, None, 10)
                    .await
                    .unwrap()
                    .items
                    .iter()
                    .any(|copy| copy.copy_id == committed_request.copy_id)
            );
            let mut prepared_batch_key = b"tape-rs/catalog/22/p/z/".to_vec();
            prepared_batch_key.extend_from_slice(volume_id.as_bytes());
            prepared_batch_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            let mut publication_key = b"tape-rs/catalog/22/p/c/".to_vec();
            publication_key.extend_from_slice(volume_id.as_bytes());
            publication_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::Unpublished
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        Uuid::new_v4(),
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::BatchConflict
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::Published
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::AlreadyPublished
            );
            let tr = direct.create_trx().unwrap();
            let publication = tr
                .get(&prepared_batch_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            assert_eq!(
                tr.get(&publication_key, false)
                    .await
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                publication
            );
            assert_eq!(
                reopened
                    .register_active_request(&committed_request)
                    .await
                    .unwrap(),
                PublishOutcome::AlreadyApplied(1)
            );
            let mut mismatched_request = committed_request.clone();
            mismatched_request.size += 1;
            assert_eq!(
                reopened
                    .register_active_request(&mismatched_request)
                    .await
                    .unwrap(),
                PublishOutcome::OperationConflict
            );
            let mut intent_key = b"tape-rs/catalog/22/I/".to_vec();
            intent_key.extend_from_slice(namespace.as_bytes());
            intent_key.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            intent_key.extend_from_slice(committed_request.operation_id.as_bytes());
            let tr = direct.create_trx().unwrap();
            let intent = tr.get(&intent_key, false).await.unwrap().unwrap();
            let intent_json: serde_json::Value = serde_json::from_slice(&intent[32..]).unwrap();
            let permit = intent_json["permit"].as_u64().unwrap() as u16;
            let mut quota_key = b"tape-rs/catalog/22/B/".to_vec();
            quota_key.extend_from_slice(namespace.as_bytes());
            quota_key.extend_from_slice(&0_u64.to_be_bytes());
            quota_key.extend_from_slice(&permit.to_be_bytes());
            let quota_before = tr.get(&quota_key, false).await.unwrap().unwrap().to_vec();
            let pending_before = u64::from_be_bytes(quota_before.as_slice().try_into().unwrap());
            let mut terminal_key = b"tape-rs/catalog/22/P/".to_vec();
            terminal_key.extend_from_slice(namespace.as_bytes());
            terminal_key.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            terminal_key.extend_from_slice(committed_request.operation_id.as_bytes());
            let mut finalized_page_key = b"tape-rs/catalog/22/p/t/".to_vec();
            finalized_page_key.extend_from_slice(volume_id.as_bytes());
            finalized_page_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            finalized_page_key.extend_from_slice(&0_u16.to_be_bytes());
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        11,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::StaleOwner
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&intent_key, b"damaged-intent");
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            assert!(tr.get(&terminal_key, false).await.unwrap().is_none());
            assert!(tr.get(&finalized_page_key, false).await.unwrap().is_none());
            assert_eq!(
                tr.get(&quota_key, false).await.unwrap().unwrap().as_ref(),
                quota_before
            );
            tr.set(&intent_key, &intent);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::Finalized
            );
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::AlreadyFinalized
            );
            let tr = direct.create_trx().unwrap();
            assert_eq!(tr.get(&terminal_key, false).await.unwrap().unwrap()[0], 0);
            assert_eq!(
                tr.get(&quota_key, false)
                    .await
                    .unwrap()
                    .as_deref()
                    .map_or(0, |value| u64::from_be_bytes(value.try_into().unwrap())),
                pending_before - 1
            );
            assert!(tr.get(&finalized_page_key, false).await.unwrap().is_some());
            // Restore only this random synthetic fixture's pending reservation
            // so its existing end-of-test cancellation can clean it normally.
            tr.clear(&terminal_key);
            tr.clear(&finalized_page_key);
            tr.set(&quota_key, &quota_before);
            tr.commit().await.unwrap();
            let read_plan = reopened
                .resolve_active_read(&serial, &committed_path, &committed_cache)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read_plan.copy.volume_generation, observed_index.generation);
            assert_eq!(read_plan.physical.path.as_bytes(), committed_request.name);
            assert_eq!(read_plan.physical.length, committed_request.size);
            assert!(
                reopened
                    .resolve_active_read(&serial, &committed_path, &new_cache)
                    .await
                    .is_err()
            );
            for result in [
                reopened.resolve_active_path(&serial, &committed_path).await,
                reopened.resolve_path(namespace, &committed_path).await,
            ] {
                assert!(matches!(
                    result.unwrap(),
                    Some(tape_rs::managed_catalog::ResolvedPath::File {
                        id,
                        head,
                        attributes: Some(attributes),
                        ..
                    }) if id == committed_request.file_id
                        && head.version == 1
                        && attributes == committed_attributes
                ));
            }
            assert!(
                reopened
                    .list_children(namespace, Uuid::nil(), None, 100)
                    .await
                    .unwrap()
                    .items
                    .iter()
                    .any(|child| child.name == committed_request.name
                        && child.id == committed_request.file_id)
            );
            assert_eq!(
                reopened
                    .resolve_child(namespace, Uuid::nil(), &committed_request.name)
                    .await
                    .unwrap(),
                Some(committed_request.file_id)
            );
            assert_eq!(
                reopened
                    .file_head(namespace, committed_request.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                1
            );
            assert_eq!(
                reopened
                    .node(namespace, committed_request.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .attributes,
                Some(committed_attributes.clone())
            );
            assert_eq!(
                reopened
                    .physical_copy(
                        namespace,
                        committed_request.file_id,
                        committed_request.copy_id,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .volume_generation,
                observed_index.generation
            );
            let mut after = None;
            let mut volume_copies = Vec::new();
            loop {
                let page = reopened
                    .copies_on_volume(volume_id, after.as_deref(), 1)
                    .await
                    .unwrap();
                volume_copies.extend(page.items);
                let Some(next) = page.next else { break };
                after = Some(next);
            }
            assert_eq!(
                volume_copies
                    .iter()
                    .filter(|copy| copy.copy_id == committed_request.copy_id)
                    .count(),
                1
            );
            assert!(
                !reopened
                    .pending_requests(namespace, None, 100)
                    .await
                    .unwrap()
                    .items
                    .iter()
                    .any(|receipt| receipt.operation_id == committed_request.operation_id)
            );
            let mut intent_key = b"tape-rs/catalog/22/I/".to_vec();
            intent_key.extend_from_slice(namespace.as_bytes());
            intent_key.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            intent_key.extend_from_slice(committed_request.operation_id.as_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_intent = tr.get(&intent_key, false).await.unwrap().unwrap().to_vec();
            tr.set(&intent_key, b"damaged-intent");
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .resolve_active_path(&serial, &committed_path)
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .list_children(namespace, Uuid::nil(), None, 100)
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .copies_on_volume(volume_id, None, 10)
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&intent_key, &saved_intent);
            tr.commit().await.unwrap();
            let tr = direct.create_trx().unwrap();
            let mut bad_publication = publication.clone();
            bad_publication[49] ^= 1;
            tr.set(&publication_key, &bad_publication);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .register_active_request(&committed_request)
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .resolve_active_path(&serial, &committed_path)
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .list_children(namespace, Uuid::nil(), None, 100)
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.clear(&publication_key);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened
                    .resolve_active_path(&serial, &committed_path)
                    .await
                    .unwrap(),
                None
            );
            assert!(
                !reopened
                    .list_children(namespace, Uuid::nil(), None, 100)
                    .await
                    .unwrap()
                    .items
                    .iter()
                    .any(|child| child.name == committed_request.name)
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        11,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        Uuid::new_v4(),
                        0,
                    )
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        1,
                    )
                    .await
                    .is_err()
            );
            let mut recovery_page_key = b"tape-rs/catalog/22/p/g/".to_vec();
            recovery_page_key.extend_from_slice(volume_id.as_bytes());
            recovery_page_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            recovery_page_key.extend_from_slice(&0_u16.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_page = tr
                .get(&recovery_page_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            let mut corrupt_page = saved_page.clone();
            corrupt_page[26] ^= 1;
            tr.set(&recovery_page_key, &corrupt_page);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&recovery_page_key, &saved_page);
            tr.commit().await.unwrap();
            let tr = direct.create_trx().unwrap();
            let saved_locator = tr
                .get(&path_locator_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            let mut corrupt_locator = saved_locator.clone();
            corrupt_locator[49] ^= 1;
            tr.set(&path_locator_key, &corrupt_locator);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&path_locator_key, &saved_locator);
            tr.commit().await.unwrap();
            assert_eq!(
                catalog.cancel_request(&committed_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::GenerationConflict
            );
            assert_eq!(
                reopened
                    .pending_requests(namespace, None, 2)
                    .await
                    .unwrap()
                    .items,
                vec![committed_request.clone()]
            );
            assert_eq!(
                reopened.cancel_request(&committed_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::GenerationConflict
            );
            assert_eq!(
                catalog
                    .register_active_request(&committed_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::GenerationConflict
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        batch_id,
                        baseline_index,
                        observed_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::AlreadyRecorded
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        Uuid::new_v4(),
                        baseline_index,
                        observed_index
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::BatchConflict
            );
            let next_index = ActiveIndexIdentity {
                generation: observed_index.generation + 1,
                location: tape_rs::ltfs::index::IndexLocation {
                    start_block: observed_index.location.start_block + 2,
                    ..observed_index.location
                },
                sha256: Sha256::digest(b"synthetic-committed-index-2").into(),
                incremental: false,
                ..observed_index
            };
            let mut next_request = committed_request.clone();
            next_request.content_digest = ContentDigest::Unknown;
            next_request.operation_id = Uuid::new_v4();
            next_request.file_id = Uuid::new_v4();
            next_request.name = b"next-generation".to_vec();
            next_request.ltfs_file_uid += 1;
            next_request.volume_generation = next_index.generation;
            let attributes_for = |file_uid| {
                NodeAttributes::new(
                    NodeMeta {
                        file_uid,
                        ..Default::default()
                    },
                    Vec::new(),
                    None,
                )
                .unwrap()
            };
            next_request.metadata_digest =
                Some(attributes_for(next_request.ltfs_file_uid).digest());
            next_request.copy_id = derive_copy_id(&next_request);
            assert_eq!(
                catalog
                    .register_active_request(&next_request)
                    .await
                    .unwrap(),
                tape_rs::managed_catalog::PublishOutcome::Registered
            );
            let mut next_requests = vec![next_request.clone()];
            for number in 1..129_u64 {
                let mut request = next_request.clone();
                request.operation_id = Uuid::new_v4();
                request.file_id = Uuid::new_v4();
                request.name = format!("next-generation-{number}").into_bytes();
                request.ltfs_file_uid += number;
                request.metadata_digest = Some(attributes_for(request.ltfs_file_uid).digest());
                request.copy_id = derive_copy_id(&request);
                assert_eq!(
                    catalog.register_active_request(&request).await.unwrap(),
                    tape_rs::managed_catalog::PublishOutcome::Registered
                );
                next_requests.push(request);
            }
            next_requests.sort_by_key(|receipt| (receipt.request_epoch, receipt.operation_id));
            let next_batch_id = Uuid::new_v4();
            assert_eq!(
                catalog
                    .stage_active_batch(next_batch_id, &next_requests)
                    .await
                    .unwrap(),
                ActiveBatchOutcome::Staged
            );
            let incorrect_previous = ActiveIndexIdentity {
                sha256: [99; 32],
                ..observed_index
            };
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        next_batch_id,
                        incorrect_previous,
                        next_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::IndexConflict
            );
            assert!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        next_batch_id,
                        baseline_index,
                        next_index
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        next_batch_id,
                        observed_index,
                        next_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::Recorded
            );
            assert_eq!(
                catalog
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::PageIncomplete
            );
            assert_eq!(
                catalog
                    .seal_observed_active_preparation(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::PageIncomplete
            );
            assert!(
                catalog
                    .prepare_observed_active_file(
                        &serial,
                        12,
                        next_batch_id,
                        &next_request,
                        &committed_attributes,
                    )
                    .await
                    .is_err()
            );
            for (number, receipt) in next_requests.iter().enumerate() {
                assert_eq!(
                    catalog
                        .prepare_observed_active_file(
                            &serial,
                            12,
                            next_batch_id,
                            receipt,
                            &attributes_for(receipt.ltfs_file_uid),
                        )
                        .await
                        .unwrap(),
                    ActivePreparationOutcome::Prepared
                );
                if number == 127 {
                    assert_eq!(
                        reopened
                            .mark_observed_active_batch_page_prepared(
                                &serial,
                                12,
                                volume_id,
                                next_index.generation,
                                next_batch_id,
                                0,
                            )
                            .await
                            .unwrap(),
                        ActivePreparationOutcome::Prepared
                    );
                    assert_eq!(
                        reopened
                            .mark_observed_active_batch_page_prepared(
                                &serial,
                                12,
                                volume_id,
                                next_index.generation,
                                next_batch_id,
                                1,
                            )
                            .await
                            .unwrap(),
                        ActivePreparationOutcome::PageIncomplete
                    );
                    assert_eq!(
                        reopened
                            .seal_observed_active_preparation(
                                &serial,
                                12,
                                volume_id,
                                next_index.generation,
                                next_batch_id,
                            )
                            .await
                            .unwrap(),
                        ActivePreparationOutcome::PageIncomplete
                    );
                }
            }
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                        1,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::Prepared
            );
            assert_eq!(
                reopened
                    .seal_observed_active_preparation(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::Prepared
            );
            let expected_names: std::collections::HashSet<_> = next_requests
                .iter()
                .map(|receipt| receipt.name.clone())
                .collect();
            let expected_copies: std::collections::HashSet<_> = next_requests
                .iter()
                .map(|receipt| receipt.copy_id)
                .collect();
            let mut next_prepared = b"tape-rs/catalog/22/p/z/".to_vec();
            next_prepared.extend_from_slice(volume_id.as_bytes());
            next_prepared.extend_from_slice(&next_index.generation.to_be_bytes());
            let mut next_publication = b"tape-rs/catalog/22/p/c/".to_vec();
            next_publication.extend_from_slice(volume_id.as_bytes());
            next_publication.extend_from_slice(&next_index.generation.to_be_bytes());
            for visible in [false, true] {
                let mut cursor = None;
                let mut found = std::collections::HashSet::new();
                loop {
                    let page = reopened
                        .list_children(namespace, Uuid::nil(), cursor.as_deref(), 23)
                        .await
                        .unwrap();
                    for child in page.items {
                        if expected_names.contains(&child.name) {
                            assert!(found.insert(child.name));
                        }
                    }
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
                assert_eq!(found.len(), if visible { 129 } else { 0 });
                let mut cursor = None;
                let mut found_copies = std::collections::HashSet::new();
                loop {
                    let page = reopened
                        .copies_on_volume(volume_id, cursor.as_deref(), 23)
                        .await
                        .unwrap();
                    for copy in page.items {
                        if expected_copies.contains(&copy.copy_id) {
                            assert!(found_copies.insert(copy.copy_id));
                            assert_eq!(copy.version, 1);
                        }
                    }
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
                assert_eq!(found_copies.len(), if visible { 129 } else { 0 });
                if !visible {
                    assert_eq!(
                        reopened
                            .publish_observed_active_batch(
                                &serial,
                                12,
                                volume_id,
                                next_index.generation,
                                next_batch_id,
                            )
                            .await
                            .unwrap(),
                        ActivePublicationOutcome::Published
                    );
                    let tr = direct.create_trx().unwrap();
                    assert_eq!(
                        tr.get(&next_publication, false)
                            .await
                            .unwrap()
                            .unwrap()
                            .as_ref(),
                        tr.get(&next_prepared, false)
                            .await
                            .unwrap()
                            .unwrap()
                            .as_ref()
                    );
                }
            }
            let mut quota_before = std::collections::BTreeMap::<u16, Vec<u8>>::new();
            let mut permit_counts = std::collections::BTreeMap::<u16, u64>::new();
            let tr = direct.create_trx().unwrap();
            for receipt in &next_requests {
                let mut intent = b"tape-rs/catalog/22/I/".to_vec();
                intent.extend_from_slice(namespace.as_bytes());
                intent.extend_from_slice(&receipt.request_epoch.to_be_bytes());
                intent.extend_from_slice(receipt.operation_id.as_bytes());
                let value = tr.get(&intent, false).await.unwrap().unwrap();
                let parsed: serde_json::Value = serde_json::from_slice(&value[32..]).unwrap();
                let permit = parsed["permit"].as_u64().unwrap() as u16;
                *permit_counts.entry(permit).or_default() += 1;
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    quota_before.entry(permit)
                {
                    let mut quota = b"tape-rs/catalog/22/B/".to_vec();
                    quota.extend_from_slice(namespace.as_bytes());
                    quota.extend_from_slice(&0_u64.to_be_bytes());
                    quota.extend_from_slice(&permit.to_be_bytes());
                    entry.insert(tr.get(&quota, false).await.unwrap().unwrap().to_vec());
                }
            }
            for page in 0..2 {
                assert_eq!(
                    reopened
                        .finalize_published_active_batch_page(
                            &serial,
                            12,
                            volume_id,
                            next_index.generation,
                            next_batch_id,
                            page,
                        )
                        .await
                        .unwrap(),
                    ActiveFinalizationOutcome::Finalized
                );
                assert_eq!(
                    reopened
                        .finalize_published_active_batch_page(
                            &serial,
                            12,
                            volume_id,
                            next_index.generation,
                            next_batch_id,
                            page,
                        )
                        .await
                        .unwrap(),
                    ActiveFinalizationOutcome::AlreadyFinalized
                );
            }
            assert_eq!(
                reopened
                    .finish_materialized_active_batch_page(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::Incomplete
            );
            assert_eq!(
                reopened
                    .seal_materialized_active_batch(
                        &serial,
                        12,
                        volume_id,
                        next_index.generation,
                        next_batch_id,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::Incomplete
            );
            let tr = direct.create_trx().unwrap();
            for (permit, before) in &quota_before {
                let mut quota = b"tape-rs/catalog/22/B/".to_vec();
                quota.extend_from_slice(namespace.as_bytes());
                quota.extend_from_slice(&0_u64.to_be_bytes());
                quota.extend_from_slice(&permit.to_be_bytes());
                let old = u64::from_be_bytes(before.as_slice().try_into().unwrap());
                let after = tr.get(&quota, false).await.unwrap();
                let now = after
                    .as_deref()
                    .map_or(0, |value| u64::from_be_bytes(value.try_into().unwrap()));
                assert_eq!(now, old - permit_counts[permit]);
                tr.set(&quota, before);
            }
            for receipt in &next_requests {
                let mut terminal = b"tape-rs/catalog/22/P/".to_vec();
                terminal.extend_from_slice(namespace.as_bytes());
                terminal.extend_from_slice(&receipt.request_epoch.to_be_bytes());
                terminal.extend_from_slice(receipt.operation_id.as_bytes());
                assert!(tr.get(&terminal, false).await.unwrap().is_some());
                tr.clear(&terminal);
            }
            for page in 0..2_u16 {
                let mut finalized = b"tape-rs/catalog/22/p/t/".to_vec();
                finalized.extend_from_slice(volume_id.as_bytes());
                finalized.extend_from_slice(&next_index.generation.to_be_bytes());
                finalized.extend_from_slice(&page.to_be_bytes());
                tr.clear(&finalized);
            }
            tr.commit().await.unwrap();
            let tr = direct.create_trx().unwrap();
            tr.clear(&next_publication);
            tr.commit().await.unwrap();
            let first_page = reopened
                .recover_observed_active_batch_page(
                    &serial,
                    12,
                    volume_id,
                    next_index.generation,
                    next_batch_id,
                    0,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(first_page.receipts.len(), 128);
            assert_eq!(first_page.next_page, Some(1));
            let last_page = reopened
                .recover_observed_active_batch_page(
                    &serial,
                    12,
                    volume_id,
                    next_index.generation,
                    next_batch_id,
                    1,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(last_page.receipts.len(), 1);
            assert_eq!(last_page.next_page, None);
            let recovered: std::collections::HashSet<_> = first_page
                .receipts
                .into_iter()
                .chain(last_page.receipts)
                .map(|receipt| receipt.operation_id)
                .collect();
            assert_eq!(recovered.len(), 129);
            assert!(
                next_requests
                    .iter()
                    .all(|receipt| recovered.contains(&receipt.operation_id))
            );
            assert_eq!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        batch_id,
                        baseline_index,
                        observed_index,
                    )
                    .await
                    .unwrap(),
                ActiveIndexOutcome::IndexConflict
            );
            assert!(catalog.advance_owner_epoch(&serial, 13).await.unwrap());
            let inherited_page = reopened
                .recover_observed_active_batch_page(
                    &serial,
                    13,
                    volume_id,
                    observed_index.generation,
                    batch_id,
                    0,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(inherited_page.receipts, vec![committed_request.clone()]);
            assert_eq!(
                reopened
                    .recover_observed_active_prepared_file(
                        &serial,
                        13,
                        batch_id,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                Some(committed_attributes.clone())
            );
            assert_eq!(
                reopened
                    .prepare_observed_active_file(
                        &serial,
                        13,
                        batch_id,
                        &committed_request,
                        &committed_attributes,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            assert_eq!(
                reopened
                    .mark_observed_active_batch_page_prepared(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            assert_eq!(
                reopened
                    .seal_observed_active_preparation(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePreparationOutcome::AlreadyPrepared
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        12,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::StaleOwner
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::Published
            );
            assert_eq!(
                reopened
                    .materialize_published_active_file(
                        &serial,
                        13,
                        batch_id,
                        0,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                ActiveMaterializationOutcome::NotFinalized
            );
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::Finalized
            );
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::AlreadyFinalized
            );
            assert_eq!(
                reopened
                    .materialize_published_active_file(
                        &serial,
                        12,
                        batch_id,
                        0,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                ActiveMaterializationOutcome::StaleOwner
            );
            assert_eq!(
                reopened
                    .materialize_published_active_file(
                        &serial,
                        13,
                        batch_id,
                        0,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                ActiveMaterializationOutcome::Materialized
            );
            assert_eq!(
                reopened
                    .materialize_published_active_file(
                        &serial,
                        13,
                        batch_id,
                        0,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                ActiveMaterializationOutcome::AlreadyMaterialized
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .receipts
                    .is_empty()
            );
            let materialized = catalog
                .resolve_active_path(&serial, b"/active-admission-only")
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                materialized,
                tape_rs::managed_catalog::ResolvedPath::File { id, head, attributes: Some(attributes), .. }
                    if id == committed_request.file_id
                        && head.version == 1
                        && attributes == committed_attributes
            ));
            assert_eq!(
                catalog
                    .file_head(namespace, committed_request.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                1
            );
            assert_eq!(
                catalog
                    .physical_copy(
                        namespace,
                        committed_request.file_id,
                        committed_request.copy_id,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .size,
                committed_request.size
            );
            assert_eq!(
                reopened
                    .finish_materialized_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::Completed
            );
            assert_eq!(
                reopened
                    .finish_materialized_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::AlreadyCompleted
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .receipts
                    .is_empty()
            );
            assert_eq!(
                reopened
                    .seal_materialized_active_batch(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::Completed
            );
            assert_eq!(
                reopened
                    .seal_materialized_active_batch(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::AlreadyCompleted
            );
            let mut completed_key = b"tape-rs/catalog/22/p/d/".to_vec();
            completed_key.extend_from_slice(volume_id.as_bytes());
            completed_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_completed = tr.get(&completed_key, false).await.unwrap().unwrap();
            tr.set(&completed_key, &[1]);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .active_batch_status(&serial, volume_id, observed_index.generation)
                    .await
                    .is_err()
            );
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&completed_key, &saved_completed);
            tr.commit().await.unwrap();
            assert_eq!(
                reopened
                    .cleanup_materialized_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::Completed
            );
            assert_eq!(
                reopened
                    .cleanup_materialized_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveCompactionOutcome::AlreadyCompleted
            );
            let completed_status = reopened
                .active_batch_status(&serial, volume_id, observed_index.generation)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(completed_status.batch_id, batch_id);
            assert_eq!(completed_status.pages, 1);
            assert!(completed_status.completed);
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .receipts
                    .is_empty()
            );
            assert_eq!(
                reopened
                    .materialize_published_active_file(
                        &serial,
                        13,
                        batch_id,
                        0,
                        &committed_request,
                    )
                    .await
                    .unwrap(),
                ActiveMaterializationOutcome::AlreadyMaterialized
            );
            assert_eq!(
                reopened
                    .publish_observed_active_batch(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                    )
                    .await
                    .unwrap(),
                ActivePublicationOutcome::AlreadyPublished
            );
            assert_eq!(
                reopened
                    .finalize_published_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap(),
                ActiveFinalizationOutcome::AlreadyFinalized
            );
            let tr = direct.create_trx().unwrap();
            let mut committed_intent = b"tape-rs/catalog/22/I/".to_vec();
            committed_intent.extend_from_slice(namespace.as_bytes());
            committed_intent.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            committed_intent.extend_from_slice(committed_request.operation_id.as_bytes());
            let mut committed_claim = b"tape-rs/catalog/22/p/i/".to_vec();
            committed_claim.extend_from_slice(namespace.as_bytes());
            committed_claim.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            committed_claim.extend_from_slice(committed_request.operation_id.as_bytes());
            assert!(tr.get(&committed_claim, false).await.unwrap().is_none());
            assert!(tr.get(&committed_intent, false).await.unwrap().is_none());
            let tr = direct.create_trx().unwrap();
            tr.clear(&finalized_page_key);
            tr.clear(&publication_key);
            tr.commit().await.unwrap();
            assert!(
                reopened
                    .recover_observed_active_batch_page(
                        &serial,
                        13,
                        volume_id,
                        observed_index.generation,
                        batch_id,
                        0,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .receipts
                    .is_empty()
            );
            assert_eq!(
                catalog
                    .observed_active_volume_index(&serial, volume_id)
                    .await
                    .unwrap(),
                next_index
            );
            for index in [baseline_index, observed_index, next_index] {
                assert_eq!(
                    catalog
                        .observed_active_volume_generation(&serial, volume_id, index.generation)
                        .await
                        .unwrap(),
                    Some(index)
                );
            }
            assert_eq!(
                catalog
                    .observed_active_volume_generation(
                        &serial,
                        volume_id,
                        next_index.generation + 1
                    )
                    .await
                    .unwrap(),
                None
            );
            let mut history_key = b"tape-rs/catalog/22/r/".to_vec();
            history_key.extend_from_slice(volume_id.as_bytes());
            history_key.extend_from_slice(&observed_index.generation.to_be_bytes());
            let tr = direct.create_trx().unwrap();
            let saved_history = tr.get(&history_key, false).await.unwrap().unwrap().to_vec();
            tr.set(&history_key, &[1]);
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .observed_active_volume_generation(
                        &serial,
                        volume_id,
                        observed_index.generation,
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&history_key, &saved_history);
            tr.commit().await.unwrap();
            assert_eq!(
                catalog
                    .volume_source(volume_id, bindings[0].scan)
                    .await
                    .unwrap(),
                baseline_source
            );
            let active_path = format!("/{pool}{}", staged_entry.path.logical_path);
            assert!(matches!(
                catalog.resolve_path(namespace, active_path.as_bytes()).await.unwrap(),
                Some(tape_rs::managed_catalog::ResolvedPath::File { id, head, .. })
                    if id == plan.id && head == plan.head
            ));
            assert!(matches!(
                catalog
                    .resolve_active_path(&serial, active_path.as_bytes())
                    .await
                    .unwrap(),
                Some(tape_rs::managed_catalog::ResolvedPath::File { id, head, .. })
                    if id == plan.id && head == plan.head
            ));
            let wrong_route = tape_rs::managed_catalog::ActiveLibraryRoute {
                frozen_index: active.frozen_index + 1,
                ..active
            };
            assert!(
                catalog
                    .resolve_active_path_for_route(&serial, active_path.as_bytes(), wrong_route)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .list_active_children_for_route(&serial, b"/", None, 1, wrong_route)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .resolve_active_path("STAGE-ABSENT", active_path.as_bytes())
                    .await
                    .is_err()
            );
            assert_eq!(
                catalog
                    .create_directory(namespace, Uuid::nil(), b"late-active", Uuid::new_v4())
                    .await
                    .unwrap(),
                DirectoryOutcome::NamespaceNotReady
            );
            assert!(
                catalog
                    .create_pool(
                        namespace,
                        &PoolRecord {
                            library_serial: serial.clone(),
                            id: Uuid::new_v4(),
                            name: "lateactive".into(),
                            file_limit: 1,
                            policy: PoolPolicy::default(),
                            revision: 1,
                        }
                    )
                    .await
                    .is_err()
            );
            assert!(!catalog.advance_owner_epoch(&serial, 10).await.unwrap());
            assert_eq!(
                catalog
                    .begin_volume_reconcile(
                        &serial,
                        10,
                        bindings[0].placement.volume_id,
                        VolumeScan {
                            generation: bindings[0].scan.generation,
                            id: Uuid::new_v4(),
                        }
                    )
                    .await
                    .unwrap(),
                VolumeOutcome::StaleOwner
            );
            assert_eq!(
                catalog
                    .begin_volume_reconcile(
                        &serial,
                        12,
                        bindings[0].placement.volume_id,
                        VolumeScan {
                            generation: bindings[0].scan.generation,
                            id: Uuid::new_v4(),
                        }
                    )
                    .await
                    .unwrap(),
                VolumeOutcome::StaleOwner
            );
            assert_eq!(
                catalog.verify_namespace_page(namespace, 1).await.unwrap(),
                NamespaceVerificationOutcome::NotVerifying
            );
            assert_eq!(
                catalog.active_library_route(&serial).await.unwrap(),
                Some(active)
            );
            assert_eq!(
                catalog
                    .activate_prepared_cutover(&serial, certificate)
                    .await
                    .unwrap(),
                active
            );
            let daemon = std::env::var("LTFS_D_BIN")
                .expect("LTFS_D_BIN 指向本轮 FDB 特性构建的 ltfsd；启动检查不接触设备");
            let daemon_dir =
                std::env::temp_dir().join(format!("tape-rs-fdb-legacy-boot-{}", Uuid::new_v4()));
            let boot = std::process::Command::new(&daemon)
                .args([
                    "--id",
                    "71",
                    "--listen",
                    "127.0.0.1:0",
                    "--peer",
                    "71=127.0.0.1:1",
                    "--peer",
                    "72=127.0.0.1:2",
                    "--peer",
                    "73=127.0.0.1:3",
                    "--data-dir",
                    daemon_dir.to_str().unwrap(),
                    "--changer-serial",
                    &serial,
                    "--fdb-cluster-file",
                    &cluster,
                ])
                .output()
                .unwrap();
            assert_eq!(boot.status.code(), Some(2));
            assert!(
                String::from_utf8_lossy(&boot.stderr)
                    .contains("已切换到 FDB，禁止启动旧 Raft/SQLite 文件服务"),
                "{}",
                String::from_utf8_lossy(&boot.stderr)
            );
            assert!(!daemon_dir.join("spool").exists());
            assert!(!daemon_dir.join("directory.db").exists());
            fs::remove_dir_all(daemon_dir).unwrap();
            use base64::Engine as _;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let readonly_dir =
                std::env::temp_dir().join(format!("tape-rs-fdb-readonly-{}", Uuid::new_v4()));
            let bind = format!("127.0.0.1:{port}");
            let daemon = TestDaemon(
                std::process::Command::new(&daemon)
                    .args([
                        "--id",
                        "71",
                        "--listen",
                        "127.0.0.1:0",
                        "--peer",
                        "71=127.0.0.1:1",
                        "--peer",
                        "72=127.0.0.1:2",
                        "--peer",
                        "73=127.0.0.1:3",
                        "--data-dir",
                        readonly_dir.to_str().unwrap(),
                        "--changer-serial",
                        &serial,
                        "--fdb-cluster-file",
                        &cluster,
                        "--client-listen",
                        &bind,
                        "--fdb-metadata-readonly",
                    ])
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            for _ in 0..100 {
                if std::net::TcpStream::connect(&bind).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let ready = fdb_http_probe(port, "GET", "/readyz");
            assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");
            let encoded =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(active_path.as_bytes());
            let stat = fdb_http_probe(
                port,
                "GET",
                &format!("/v1/fdb/stat/{serial}?path={encoded}"),
            );
            assert!(stat.starts_with("HTTP/1.1 200"), "{stat}");
            assert!(stat.contains("\"kind\":\"file\""), "{stat}");
            let mut cursor = None;
            let mut found_pool = false;
            for _ in 0..100 {
                let suffix = cursor
                    .as_ref()
                    .map_or(String::new(), |value: &String| format!("&after={value}"));
                let root = fdb_http_probe(
                    port,
                    "GET",
                    &format!("/v1/fdb/children/{serial}?path=Lw&limit=1{suffix}"),
                );
                assert!(root.starts_with("HTTP/1.1 200"), "{root}");
                let body: serde_json::Value =
                    serde_json::from_str(root.split_once("\r\n\r\n").unwrap().1).unwrap();
                let items = body["items"].as_array().unwrap();
                assert!(items.len() <= 1);
                found_pool |= items.iter().any(|item| item["name"] == pool.to_string());
                let next = body["next"].as_str().map(str::to_owned);
                if next.is_some() {
                    assert_ne!(next, cursor, "目录分页游标未前进：{body}");
                }
                cursor = next;
                if cursor.is_none() || found_pool {
                    break;
                }
            }
            assert!(found_pool, "根目录分页未列出已知池 {pool}");
            let parent = active_path.rsplit_once('/').unwrap().0;
            let parent = if parent.is_empty() { "/" } else { parent };
            let parent = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(parent.as_bytes());
            let children = fdb_http_probe(
                port,
                "GET",
                &format!("/v1/fdb/children/{serial}?path={parent}&limit=100"),
            );
            assert!(children.starts_with("HTTP/1.1 200"), "{children}");
            assert!(children.contains(&plan.id.to_string()), "{children}");
            let invalid_cursor = fdb_http_probe(
                port,
                "GET",
                &format!("/v1/fdb/children/{serial}?path=Lw&after=AA&limit=1"),
            );
            assert!(
                invalid_cursor.starts_with("HTTP/1.1 400"),
                "{invalid_cursor}"
            );
            let write = fdb_http_probe(port, "PUT", "/files/unavailable");
            assert!(write.starts_with("HTTP/1.1 404"), "{write}");
            assert!(!readonly_dir.join("directory.db").exists());
            assert!(!readonly_dir.join("spool").exists());
            let mut certificate_key = b"tape-rs/catalog/22/x/".to_vec();
            certificate_key.extend_from_slice(serial.as_bytes());
            certificate_key.extend_from_slice(&[0, 0]);
            let tr = direct.create_trx().unwrap();
            let saved_certificate = tr
                .get(&certificate_key, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
            tr.set(&certificate_key, &[1]);
            tr.commit().await.unwrap();
            assert!(catalog.active_library_route(&serial).await.is_err());
            let unavailable = fdb_http_probe(port, "GET", "/readyz");
            assert!(unavailable.starts_with("HTTP/1.1 503"), "{unavailable}");
            assert!(
                catalog
                    .register_active_request(&takeover_request)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .resolve_active_path(&serial, active_path.as_bytes())
                    .await
                    .is_err()
            );
            drop(daemon);
            fs::remove_dir_all(readonly_dir).unwrap();
            assert!(
                catalog
                    .observed_active_volume_index(&serial, volume_id)
                    .await
                    .is_err()
            );
            let invalid_index = ActiveIndexIdentity {
                generation: next_index.generation + 1,
                location: tape_rs::ltfs::index::IndexLocation {
                    start_block: next_index.location.start_block + 2,
                    ..next_index.location
                },
                sha256: Sha256::digest(b"synthetic-unpublished-index").into(),
                ..next_index
            };
            assert!(
                catalog
                    .record_active_volume_index(
                        &serial,
                        12,
                        volume_id,
                        next_batch_id,
                        next_index,
                        invalid_index
                    )
                    .await
                    .is_err()
            );
            let tr = direct.create_trx().unwrap();
            tr.set(&certificate_key, &saved_certificate);
            tr.commit().await.unwrap();
            assert_eq!(
                catalog.active_library_route(&serial).await.unwrap(),
                Some(active)
            );
            assert!(
                catalog
                    .prepare_library_route(&serial, namespace, None)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .forget_prepared_library_route(&serial, 1)
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .certify_prepared_cutover(&serial, route, 71, evidence)
                    .await
                    .is_err()
            );
            let mut owner_key = b"tape-rs/catalog/22/O/".to_vec();
            owner_key.extend_from_slice(serial.as_bytes());
            owner_key.extend_from_slice(&[0, 0]);
            let mut active_key = b"tape-rs/catalog/22/A/".to_vec();
            active_key.extend_from_slice(serial.as_bytes());
            active_key.extend_from_slice(&[0, 0]);
            let mut state_key = b"tape-rs/catalog/22/S/".to_vec();
            state_key.extend_from_slice(namespace.as_bytes());
            let tr = direct.create_trx().unwrap();
            assert_eq!(
                tr.get(&owner_key, false).await.unwrap().unwrap().as_ref(),
                &13_u64.to_be_bytes()
            );
            assert_eq!(
                tr.get(&state_key, false).await.unwrap().unwrap().as_ref(),
                &[NamespaceState::Active as u8]
            );
            // The synthetic namespace is never routed to clients. Restore
            // its private FDB keys exactly so the existing discard test can
            // exercise the pre-activation cleanup path on a clean fixture.
            let mut observed_key = b"tape-rs/catalog/22/r/".to_vec();
            observed_key.extend_from_slice(volume_id.as_bytes());
            tr.clear(&observed_key);
            for generation in [observed_index.generation, next_index.generation] {
                let mut history_key = observed_key.clone();
                history_key.extend_from_slice(&generation.to_be_bytes());
                tr.clear(&history_key);
            }
            let mut batch_page_key = b"tape-rs/catalog/22/p/g/".to_vec();
            batch_page_key.extend_from_slice(volume_id.as_bytes());
            batch_page_key.extend_from_slice(&committed_request.volume_generation.to_be_bytes());
            batch_page_key.extend_from_slice(&0_u16.to_be_bytes());
            let mut claim_key = b"tape-rs/catalog/22/p/i/".to_vec();
            claim_key.extend_from_slice(namespace.as_bytes());
            claim_key.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            claim_key.extend_from_slice(committed_request.operation_id.as_bytes());
            tr.clear(&batch_key);
            tr.clear(&batch_page_key);
            let mut batch_digest_key = batch_page_key.clone();
            batch_digest_key[b"tape-rs/catalog/22/p/".len()] = b'h';
            tr.clear(&batch_digest_key);
            tr.clear(&claim_key);
            let mut materialized_key = b"tape-rs/catalog/22/p/m/".to_vec();
            materialized_key.extend_from_slice(namespace.as_bytes());
            materialized_key.extend_from_slice(&committed_request.request_epoch.to_be_bytes());
            materialized_key.extend_from_slice(committed_request.operation_id.as_bytes());
            tr.clear(&materialized_key);
            let mut compacted_batch_key = b"tape-rs/catalog/22/p/d/".to_vec();
            compacted_batch_key.extend_from_slice(volume_id.as_bytes());
            compacted_batch_key
                .extend_from_slice(&committed_request.volume_generation.to_be_bytes());
            tr.clear(&compacted_batch_key);
            let mut next_batch_key = b"tape-rs/catalog/22/p/b/".to_vec();
            next_batch_key.extend_from_slice(volume_id.as_bytes());
            next_batch_key.extend_from_slice(&next_request.volume_generation.to_be_bytes());
            let mut next_page_key = b"tape-rs/catalog/22/p/g/".to_vec();
            next_page_key.extend_from_slice(volume_id.as_bytes());
            next_page_key.extend_from_slice(&next_request.volume_generation.to_be_bytes());
            next_page_key.extend_from_slice(&0_u16.to_be_bytes());
            let mut next_claim_key = b"tape-rs/catalog/22/p/i/".to_vec();
            next_claim_key.extend_from_slice(namespace.as_bytes());
            next_claim_key.extend_from_slice(&next_request.request_epoch.to_be_bytes());
            next_claim_key.extend_from_slice(next_request.operation_id.as_bytes());
            tr.clear(&next_batch_key);
            tr.clear(&next_page_key);
            let mut next_digest_key = next_page_key.clone();
            next_digest_key[b"tape-rs/catalog/22/p/".len()] = b'h';
            tr.clear(&next_digest_key);
            let mut second_page_key = next_page_key.clone();
            second_page_key.truncate(second_page_key.len() - 2);
            second_page_key.extend_from_slice(&1_u16.to_be_bytes());
            tr.clear(&second_page_key);
            let mut second_digest_key = second_page_key.clone();
            second_digest_key[b"tape-rs/catalog/22/p/".len()] = b'h';
            tr.clear(&second_digest_key);
            for receipt in &next_requests {
                next_claim_key.truncate(next_claim_key.len() - 16);
                next_claim_key.extend_from_slice(receipt.operation_id.as_bytes());
                tr.clear(&next_claim_key);
            }
            let clear_target_claims = |receipt: &CommitReceipt| {
                let mut path = b"tape-rs/catalog/22/p/l/".to_vec();
                path.extend_from_slice(receipt.namespace.as_bytes());
                path.extend_from_slice(receipt.parent.as_bytes());
                for byte in &receipt.name {
                    if *byte == 0 {
                        path.extend_from_slice(&[0, 0xff]);
                    } else {
                        path.push(*byte);
                    }
                }
                path.extend_from_slice(&[0, 0]);
                tr.clear(&path);
                let mut file = b"tape-rs/catalog/22/p/n/".to_vec();
                file.extend_from_slice(receipt.namespace.as_bytes());
                file.extend_from_slice(receipt.file_id.as_bytes());
                tr.clear(&file);
                let mut reverse = b"tape-rs/catalog/22/p/o/".to_vec();
                reverse.extend_from_slice(receipt.volume_id.as_bytes());
                reverse.extend_from_slice(receipt.namespace.as_bytes());
                reverse.extend_from_slice(receipt.file_id.as_bytes());
                reverse.extend_from_slice(receipt.copy_id.as_bytes());
                tr.clear(&reverse);
            };
            clear_target_claims(&committed_request);
            for receipt in &next_requests {
                clear_target_claims(receipt);
            }
            let clear_prepared = |receipt: &CommitReceipt| {
                let mut suffix = volume_id.as_bytes().to_vec();
                suffix.extend_from_slice(&receipt.volume_generation.to_be_bytes());
                suffix.extend_from_slice(&receipt.request_epoch.to_be_bytes());
                suffix.extend_from_slice(receipt.operation_id.as_bytes());
                let mut item = b"tape-rs/catalog/22/p/v/".to_vec();
                item.extend_from_slice(&suffix);
                tr.clear(&item);
                for index in 0..if receipt.operation_id == committed_request.operation_id {
                    2
                } else {
                    1
                } {
                    let mut chunk = b"tape-rs/catalog/22/p/a/".to_vec();
                    chunk.extend_from_slice(&suffix);
                    chunk.extend_from_slice(&(index as u16).to_be_bytes());
                    tr.clear(&chunk);
                }
            };
            clear_prepared(&committed_request);
            for receipt in &next_requests {
                clear_prepared(receipt);
            }
            // This fixture deliberately restores its pre-activation import
            // namespace. Remove the one synthetic Active copy materialized
            // above before exercising the old import-only discard path.
            let mut child = b"tape-rs/catalog/22/C/".to_vec();
            child.extend_from_slice(namespace.as_bytes());
            child.extend_from_slice(Uuid::nil().as_bytes());
            child.extend_from_slice(&committed_request.name);
            child.extend_from_slice(&[0, 0]);
            tr.clear(&child);
            for prefix in *b"NH" {
                let mut row = b"tape-rs/catalog/22/".to_vec();
                row.push(prefix);
                row.push(b'/');
                row.extend_from_slice(namespace.as_bytes());
                row.extend_from_slice(committed_request.file_id.as_bytes());
                tr.clear(&row);
            }
            let mut copy = b"tape-rs/catalog/22/R/".to_vec();
            copy.extend_from_slice(namespace.as_bytes());
            copy.extend_from_slice(committed_request.file_id.as_bytes());
            copy.extend_from_slice(committed_request.copy_id.as_bytes());
            tr.clear(&copy);
            let mut reverse = b"tape-rs/catalog/22/V/".to_vec();
            reverse.extend_from_slice(committed_request.volume_id.as_bytes());
            reverse.extend_from_slice(committed_request.volume_scan_id.as_bytes());
            reverse.extend_from_slice(namespace.as_bytes());
            reverse.extend_from_slice(committed_request.file_id.as_bytes());
            reverse.extend_from_slice(committed_request.copy_id.as_bytes());
            tr.clear(&reverse);
            let mut attributes = b"tape-rs/catalog/22/m/".to_vec();
            attributes.extend_from_slice(namespace.as_bytes());
            attributes.extend_from_slice(committed_request.file_id.as_bytes());
            for chunk in 0..2_u16 {
                let mut row = attributes.clone();
                row.extend_from_slice(&chunk.to_be_bytes());
                tr.clear(&row);
            }
            for generation in [observed_index.generation, next_index.generation] {
                let mut batch_marker = b"tape-rs/catalog/22/p/z/".to_vec();
                batch_marker.extend_from_slice(volume_id.as_bytes());
                batch_marker.extend_from_slice(&generation.to_be_bytes());
                tr.clear(&batch_marker);
                for page in 0..if generation == next_index.generation {
                    2
                } else {
                    1
                } {
                    let mut marker = b"tape-rs/catalog/22/p/y/".to_vec();
                    marker.extend_from_slice(volume_id.as_bytes());
                    marker.extend_from_slice(&generation.to_be_bytes());
                    marker.extend_from_slice(&(page as u16).to_be_bytes());
                    tr.clear(&marker);
                }
            }
            // Restore the synthetic owner only after all observed batch rows
            // are removed, so the fixture can cancel its original requests.
            tr.set(&owner_key, &12_u64.to_be_bytes());
            tr.commit().await.unwrap();
            let tr = direct.create_trx().unwrap();
            assert!(tr.get(&observed_key, false).await.unwrap().is_none());
            for generation in [observed_index.generation, next_index.generation] {
                let mut history_key = observed_key.clone();
                history_key.extend_from_slice(&generation.to_be_bytes());
                assert!(tr.get(&history_key, false).await.unwrap().is_none());
            }
            assert_eq!(
                catalog.cancel_request(&committed_request).await.unwrap(),
                tape_rs::managed_catalog::PublishOutcome::AlreadyApplied(1)
            );
            for receipt in &next_requests {
                assert_eq!(
                    catalog.cancel_request(receipt).await.unwrap(),
                    tape_rs::managed_catalog::PublishOutcome::RequestCancelled
                );
            }
            let tr = direct.create_trx().unwrap();
            tr.clear(&active_key);
            tr.set(&owner_key, &10_u64.to_be_bytes());
            tr.set(&state_key, &[NamespaceState::Sealed as u8]);
            tr.commit().await.unwrap();
            assert_eq!(catalog.active_library_route(&serial).await.unwrap(), None);
            assert_eq!(
                reopened
                    .prepare_library_route(&serial, namespace, None)
                    .await
                    .unwrap(),
                PrepareRouteOutcome::Unchanged(route)
            );
            assert_eq!(
                reopened
                    .forget_prepared_library_route(&serial, 2)
                    .await
                    .unwrap(),
                ForgetRouteOutcome::RevisionConflict(route)
            );
            assert_eq!(
                catalog
                    .forget_prepared_library_route(&serial, 1)
                    .await
                    .unwrap(),
                ForgetRouteOutcome::Removed
            );
            assert_eq!(
                reopened.prepared_library_route(&serial).await.unwrap(),
                None
            );
            assert!(
                reopened
                    .certified_prepared_cutover(&serial)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                reopened
                    .prepare_library_route(&serial, namespace, Some(1))
                    .await
                    .unwrap(),
                PrepareRouteOutcome::Conflict(None)
            );
            let next_route = PreparedLibraryRoute {
                namespace,
                revision: 3,
            };
            assert_eq!(
                reopened
                    .prepare_library_route(&serial, namespace, None)
                    .await
                    .unwrap(),
                PrepareRouteOutcome::Prepared(next_route)
            );
            assert_eq!(
                catalog
                    .forget_prepared_library_route(&serial, 1)
                    .await
                    .unwrap(),
                ForgetRouteOutcome::RevisionConflict(next_route)
            );
            assert_eq!(
                catalog
                    .forget_prepared_library_route(&serial, 3)
                    .await
                    .unwrap(),
                ForgetRouteOutcome::Removed
            );
            assert_eq!(
                reopened.prepared_library_route(&serial).await.unwrap(),
                None
            );
            // This random serial exists only in this test. Remove its CAS
            // tombstone so repeated site runs do not grow global route keys.
            let mut route_key = b"tape-rs/catalog/22/z/".to_vec();
            route_key.extend_from_slice(serial.as_bytes());
            route_key.extend_from_slice(&[0, 0]);
            let mut certificate_key = b"tape-rs/catalog/22/x/".to_vec();
            certificate_key.extend_from_slice(serial.as_bytes());
            certificate_key.extend_from_slice(&[0, 0]);
            let tr = direct.create_trx().unwrap();
            tr.clear(&route_key);
            tr.clear(&certificate_key);
            tr.commit().await.unwrap();
            assert!(
                !ManagedCatalog::open(Some(&cluster))
                    .unwrap()
                    .seal_bound_staged_import(namespace)
                    .await
                    .unwrap()
            );
            assert_eq!(
                catalog.file_head(namespace, plan.id).await.unwrap(),
                Some(plan.head.clone())
            );
            assert!(
                catalog
                    .physical_copy(namespace, plan.id, plan.head.copy_id)
                    .await
                    .unwrap()
                    .is_some()
            );
            loop {
                if catalog
                    .purge_sealed_import_staging(namespace, 2)
                    .await
                    .unwrap()
                    .complete
                {
                    break;
                }
            }
            assert_eq!(
                catalog
                    .purge_sealed_import_staging(namespace, 2)
                    .await
                    .unwrap()
                    .removed,
                0
            );
            assert!(
                catalog
                    .staged_row_sha256(namespace, 0)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                catalog.file_head(namespace, plan.id).await.unwrap(),
                Some(plan.head)
            );
            eprintln!(
                "合成已发布候选 namespace={namespace} source_volumes={},{}",
                bindings[0].placement.volume_id, bindings[1].placement.volume_id
            );
            // This random test namespace is never routed to a client. Restore
            // its private state so the existing discard path can remove all
            // global placement and copy rows; this is test-only compensation.
            let mut state_key = b"tape-rs/catalog/22/S/".to_vec();
            state_key.extend_from_slice(namespace.as_bytes());
            let tr = direct.create_trx().unwrap();
            assert_eq!(
                tr.get(&state_key, false).await.unwrap().unwrap().as_ref(),
                &[NamespaceState::Sealed as u8]
            );
            tr.set(&state_key, &[NamespaceState::Verified as u8]);
            tr.commit().await.unwrap();
            for path in [node72, node73, bad_node] {
                fs::remove_file(path).unwrap();
            }
        }
        let mut progress_key = b"tape-rs/catalog/22/Z/".to_vec();
        progress_key.extend_from_slice(namespace.as_bytes());
        let tr = direct.create_trx().unwrap();
        let progress = tr
            .get(&progress_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut wrong_progress = progress.clone();
        wrong_progress[16] ^= 1;
        tr.set(&progress_key, &wrong_progress);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .inspect_bound_import_copy_fence(namespace)
                .await
                .is_err()
        );
        let tr = direct.create_trx().unwrap();
        tr.set(&progress_key, &progress);
        tr.commit().await.unwrap();
        let mut hold_key = b"tape-rs/catalog/22/I/".to_vec();
        hold_key.extend_from_slice(namespace.as_bytes());
        hold_key.extend_from_slice(Uuid::new_v4().as_bytes());
        let tr = direct.create_trx().unwrap();
        tr.set(&hold_key, b"pending");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .inspect_bound_import_copy_fence(namespace)
                .await
                .is_err()
        );
        let tr = direct.create_trx().unwrap();
        tr.clear(&hold_key);
        tr.commit().await.unwrap();
        catalog
            .inspect_bound_import_copy_fence(namespace)
            .await
            .unwrap();
        // A metadata-only reconcile changes the bound source after the
        // private proof was written, so a future seal must reject it.
        let next_scan = VolumeScan {
            generation: bindings[0].scan.generation,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, bindings[0].placement.volume_id, next_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let tr = revision_db.create_trx().unwrap();
        assert_eq!(
            tr.get(&revision_key, false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            5u64.to_be_bytes().as_slice()
        );
        assert!(
            catalog
                .inspect_bound_import_copy_fence(namespace)
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 10, bindings[0].placement.volume_id, next_scan)
                .await
                .unwrap(),
            VolumeOutcome::Cancelled
        );
        assert!(catalog.file_head(namespace, plan.id).await.is_err());
        assert!(catalog.seal_import(namespace).await.is_err());
        assert!(catalog.discard_import(namespace).await.unwrap());
        for binding in &bindings {
            assert_eq!(
                catalog
                    .retire_discarded_volume_scan(
                        namespace,
                        &serial,
                        10,
                        binding.placement.volume_id,
                        binding.scan,
                    )
                    .await
                    .unwrap(),
                VolumeOutcome::Retired
            );
        }
        loop {
            match catalog.purge_discarded_namespace(namespace, 2).await {
                Ok(progress) => assert!(!progress.complete),
                Err(tape_rs::managed_catalog::CatalogError::Invalid(_)) => break,
                Err(error) => panic!("清理导入副本失败: {error}"),
            }
        }
        for binding in &bindings {
            assert_eq!(
                catalog
                    .release_discarded_tape(
                        namespace,
                        pool,
                        &serial,
                        &binding.placement.barcode,
                        Some(binding.placement.volume_id),
                        10,
                    )
                    .await
                    .unwrap(),
                ControlOutcome::Unassigned
            );
        }
        loop {
            if catalog
                .purge_discarded_namespace(namespace, 2)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
        let tr = revision_db.create_trx().unwrap();
        assert!(tr.get(&revision_key, false).await.unwrap().is_none());
        let mut sealed_key = b"tape-rs/catalog/22/w/".to_vec();
        sealed_key.extend_from_slice(namespace.as_bytes());
        assert!(tr.get(&sealed_key, false).await.unwrap().is_none());
        drop(db);
        drop(spool);
        drop(older_spool);
        if seal {
            fs::remove_file(committed_cache).unwrap();
        }
        for path in [spool_path, older_path, new_cache, old_cache, snapshot] {
            fs::remove_file(path).unwrap();
        }
        return;
    }
    let replacement_scan = VolumeScan {
        generation: bindings[0].scan.generation,
        id: Uuid::new_v4(),
    };
    assert_eq!(
        catalog
            .begin_volume_reconcile(
                &serial,
                10,
                bindings[0].placement.volume_id,
                replacement_scan,
            )
            .await
            .unwrap(),
        VolumeOutcome::Reconciling
    );
    assert!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .is_err()
    );
    assert!(
        catalog
            .verify_staged_source_bindings(namespace)
            .await
            .is_err()
    );
    assert!(
        catalog
            .confirm_bound_staged_import(namespace)
            .await
            .is_err()
    );
    db.execute("UPDATE meta SET v=93 WHERE k='applied_index'", [])
        .unwrap();
    assert!(
        catalog
            .stage_audited_reconcile(namespace, &snapshot, &scratch_dir, volumes(), &bindings)
            .await
            .is_err()
    );
    let mut digest = NamespaceDigest::new(namespace).unwrap();
    digest
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
    let expected = digest.summary();
    assert_eq!(
        catalog.freeze_namespace(namespace, expected).await.unwrap(),
        NamespaceVerificationOutcome::Frozen
    );
    loop {
        match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
            NamespaceVerificationOutcome::Progress { .. } => {}
            NamespaceVerificationOutcome::Verified(_) => break,
            outcome => panic!("unexpected verification outcome: {outcome:?}"),
        }
    }
    assert!(catalog.seal_import(namespace).await.is_err());
    assert!(
        catalog
            .retire_discarded_volume_scan(
                namespace,
                &serial,
                10,
                bindings[0].placement.volume_id,
                bindings[0].scan,
            )
            .await
            .is_err()
    );
    assert!(catalog.discard_import(namespace).await.unwrap());
    assert_eq!(
        catalog
            .retire_discarded_volume_scan(
                namespace,
                &serial,
                10,
                bindings[0].placement.volume_id,
                bindings[0].scan,
            )
            .await
            .unwrap(),
        VolumeOutcome::ScanConflict
    );
    assert_eq!(
        catalog
            .cancel_volume_reconcile(
                &serial,
                10,
                bindings[0].placement.volume_id,
                replacement_scan,
            )
            .await
            .unwrap(),
        VolumeOutcome::Cancelled
    );
    for binding in &bindings {
        assert_eq!(
            catalog
                .retire_discarded_volume_scan(
                    namespace,
                    &serial,
                    9,
                    binding.placement.volume_id,
                    binding.scan,
                )
                .await
                .unwrap(),
            VolumeOutcome::StaleOwner
        );
        assert_eq!(
            catalog
                .retire_discarded_volume_scan(
                    namespace,
                    &serial,
                    10,
                    binding.placement.volume_id,
                    binding.scan,
                )
                .await
                .unwrap(),
            VolumeOutcome::Retired
        );
        assert_eq!(
            catalog
                .retire_discarded_volume_scan(
                    namespace,
                    &serial,
                    10,
                    binding.placement.volume_id,
                    binding.scan,
                )
                .await
                .unwrap(),
            VolumeOutcome::AlreadyRetired
        );
        assert_eq!(
            catalog
                .release_discarded_tape(
                    namespace,
                    pool,
                    &serial,
                    &binding.placement.barcode,
                    Some(binding.placement.volume_id),
                    10,
                )
                .await
                .unwrap(),
            ControlOutcome::Unassigned
        );
    }
    loop {
        if catalog
            .purge_discarded_namespace(namespace, 2)
            .await
            .unwrap()
            .complete
        {
            break;
        }
    }
    assert!(
        catalog
            .staged_import_summary(namespace)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .verify_staged_source_bindings(namespace)
            .await
            .is_err()
    );
    assert!(
        catalog
            .bound_staged_import_summary(namespace)
            .await
            .is_err()
    );
    assert!(
        catalog
            .staged_row_sha256(namespace, 0)
            .await
            .unwrap()
            .is_none()
    );
    drop(db);
    drop(spool);
    drop(older_spool);
    fs::remove_file(spool_path).unwrap();
    fs::remove_file(older_path).unwrap();
    fs::remove_file(new_cache).unwrap();
    fs::remove_file(old_cache).unwrap();
    fs::remove_file(snapshot).unwrap();
}
