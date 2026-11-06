use super::tests::{fixture, start_target, target_entries};
use super::*;
use crate::ltfs::index::{DirectoryNode, Extent, FileNode, LtfsIndex};

fn source_at(
    root: &std::path::Path,
    volume: Uuid,
    generation: u64,
    files: &[(&str, u64, u64)],
) -> (ImportSpool, NativeMediaSource) {
    let mut index = LtfsIndex::empty(volume, "refresh-test".into(), 'b');
    index.generation = generation;
    index.self_location.start_block = generation * 20;
    let mut dir = DirectoryNode {
        name: "docs".into(),
        meta: index.root.meta.clone(),
        ..Default::default()
    };
    dir.meta.file_uid = 2;
    for (name, uid, length) in files {
        let mut f = FileNode {
            name: (*name).into(),
            length: *length,
            meta: index.root.meta.clone(),
            ..Default::default()
        };
        f.meta.file_uid = *uid;
        if *length > 0 {
            f.extents.push(Extent {
                partition: 'b',
                start_block: 100 + uid,
                byte_offset: 0,
                byte_count: *length,
                file_offset: 0,
            });
        }
        dir.files.push(f);
        index.highest_file_uid = index.highest_file_uid.max(*uid);
    }
    index.highest_file_uid = index.highest_file_uid.max(2);
    index.root.subdirs.push(dir);
    let xml = index.to_xml().unwrap();
    let expected = ExpectedIndex {
        volume_uuid: volume,
        generation,
        location: index.self_location,
        sha256: Sha256::digest(&xml).into(),
    };
    let path = root.join(format!("{}-{}.sqlite", generation, Uuid::new_v4()));
    ImportSpool::build(&xml[..], &path, expected).unwrap();
    let spool = ImportSpool::open(&path, expected).unwrap();
    let proof = NativeMediaSource {
        identity: spool.identity().clone(),
        files: files.len() as u64,
        paths: spool.candidate_stats().entries,
    };
    (spool, proof)
}
async fn project(
    c: &ManagedCatalog,
    i: &NativeMediaImport,
    owner: u64,
    s: &ImportSpool,
    p: &NativeMediaSource,
) {
    c.bind_native_media_source(i, owner, p).await.unwrap();
    for e in target_entries(i, s) {
        c.project_native_media_entry(i, owner, &e, false)
            .await
            .unwrap();
    }
    c.begin_native_media_projection(i, owner).await.unwrap();
    for e in target_entries(i, s) {
        c.project_native_media_entry(i, owner, &e, true)
            .await
            .unwrap();
    }
}
async fn file(
    c: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    pool: Uuid,
    path: &str,
) -> Option<ResolvedPath> {
    c.resolve_active_path_for_route(
        serial,
        &pool_catalog_path(pool, path.as_bytes()).unwrap(),
        route,
    )
    .await
    .unwrap()
}
#[tokio::test]
#[ignore = "需要隔离FDB；外部新索引更新/新增/删除、原请求重放和同卷归属，不触硬件"]
async fn assigned_refresh_changes_source_without_duplicate_paths_or_rebinding() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("refresh-diff-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let volume = Uuid::new_v4();
    let (s, p) = source_at(&root, volume, 1, &[("a", 3, 3), ("removed", 4, 2)]);
    let i = start_target(
        &c,
        &serial,
        route,
        pool.id,
        100,
        "REFRESH1L8",
        ("/custom", true),
    )
    .await;
    project(&c, &i, 100, &s, &p).await;
    c.finish_native_media(&i, 100, &p).await.unwrap();
    let before = file(&c, &serial, route, pool.id, "/custom/docs/a")
        .await
        .unwrap();
    let assigned = c.tape_assignment(&serial, "REFRESH1L8").await.unwrap();
    let (s2, p2) = source_at(&root, volume, 2, &[("a", 3, 7), ("新文件 +%", 5, 9)]);
    let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"REFRESH1L8","volume":volume.to_string(),"path":"/","merge_tapes":true}})).unwrap();
    c.admit_management(&serial, route, 100, &request)
        .await
        .unwrap();
    let r = c
        .start_native_media(&serial, route, 100, request.epoch(), request.operation())
        .await
        .unwrap()
        .unwrap();
    project(&c, &r, 100, &s2, &p2).await;
    assert!(matches!(
        c.resolve_active_path_for_route(
            &serial,
            &pool_catalog_path(pool.id, b"/custom/docs/a").unwrap(),
            route
        )
        .await,
        Err(CatalogError::PoolImporting)
    ));
    let done = c.finish_native_media(&r, 100, &p2).await.unwrap();
    assert_eq!(done["result"]["refreshed"], true);
    assert_eq!(c.finish_native_media(&r, 100, &p2).await.unwrap(), done);
    assert!(
        c.start_native_media(
            &serial,
            route,
            100,
            r.request.epoch(),
            r.request.operation()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
        c.tape_assignment(&serial, "REFRESH1L8").await.unwrap(),
        assigned
    );
    let after = file(&c, &serial, route, pool.id, "/custom/docs/a")
        .await
        .unwrap();
    match (before, after) {
        (
            ResolvedPath::File {
                id: a, head: old, ..
            },
            ResolvedPath::File {
                id: b,
                head: new,
                copy,
                ..
            },
        ) => {
            assert_eq!(a, b);
            assert_eq!(new.version, old.version + 1);
            assert_eq!(new.size, 7);
            assert_eq!(copy.volume_scan_id, r.scan());
            assert_eq!(copy.volume_generation, 2);
        }
        _ => panic!("files"),
    }
    assert!(
        file(&c, &serial, route, pool.id, "/custom/docs/新文件 +%")
            .await
            .is_some()
    );
    assert!(
        file(&c, &serial, route, pool.id, "/custom/docs/removed")
            .await
            .is_none()
    );
    assert!(
        file(&c, &serial, route, pool.id, "/custom/docs/a_1")
            .await
            .is_none()
    );
    assert!(
        file(&c, &serial, route, pool.id, "/IMPORT/REFRESH1L8")
            .await
            .is_none()
    );
    let observed = c
        .observed_active_volume_index(&serial, volume)
        .await
        .unwrap();
    assert_eq!(observed.generation, 2);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
#[ignore = "需要隔离FDB；刷新部分发布/退休后的取消、重开目录和owner接任回滚，不触硬件"]
async fn assigned_refresh_rollback_restores_exact_old_files_after_owner_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("refresh-undo-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let volume = Uuid::new_v4();
    let (s, p) = source_at(&root, volume, 1, &[("a", 3, 3), ("removed", 4, 2)]);
    let i = start_target(&c, &serial, route, pool.id, 100, "REFRESH2L8", ("/", true)).await;
    project(&c, &i, 100, &s, &p).await;
    c.finish_native_media(&i, 100, &p).await.unwrap();
    let old_a = file(&c, &serial, route, pool.id, "/docs/a").await.unwrap();
    let old_removed = file(&c, &serial, route, pool.id, "/docs/removed")
        .await
        .unwrap();
    let old_assignment = c.tape_assignment(&serial, "REFRESH2L8").await.unwrap();
    let (s2, p2) = source_at(&root, volume, 2, &[("a", 3, 7), ("new", 5, 9)]);
    let r = start_target(
        &c,
        &serial,
        route,
        pool.id,
        100,
        "REFRESH2L8",
        ("/IMPORT", false),
    )
    .await;
    project(&c, &r, 100, &s2, &p2).await;
    c.db.run(|tr, _| {
        let r = &r;
        async move {
            requests::retry_result(
                async {
                    let value = state(&tr, r, 100).await?;
                    refresh::sweep(&tr, r, &value).await
                }
                .await,
            )
        }
    })
    .await
    .unwrap()
    .unwrap();
    drop(c);
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    c.claim_active_owner_epoch(&serial, 101).await.unwrap();
    assert!(matches!(
        c.abort_native_media(&r, 100, None).await,
        Err(CatalogError::OwnerFenced)
    ));
    let resumed = c
        .start_native_media(
            &serial,
            route,
            101,
            r.request.epoch(),
            r.request.operation(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.refresh, r.refresh);
    c.abort_native_media(&resumed, 101, None).await.unwrap();
    while !c.rollback_native_media_page(&resumed, 101).await.unwrap() {}
    assert_eq!(
        file(&c, &serial, route, pool.id, "/docs/a").await.unwrap(),
        old_a
    );
    assert_eq!(
        file(&c, &serial, route, pool.id, "/docs/removed")
            .await
            .unwrap(),
        old_removed
    );
    assert!(
        file(&c, &serial, route, pool.id, "/docs/new")
            .await
            .is_none()
    );
    assert_eq!(
        c.tape_assignment(&serial, "REFRESH2L8").await.unwrap(),
        old_assignment
    );
    assert_eq!(
        c.observed_active_volume_index(&serial, volume)
            .await
            .unwrap()
            .generation,
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
#[ignore = "需要隔离FDB；刷新拒换卷、旧代数和同代异源，不触硬件"]
async fn assigned_refresh_rejects_foreign_or_older_source_and_retains_old_catalog() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("refresh-reject-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let volume = Uuid::new_v4();
    let (s, p) = source_at(&root, volume, 5, &[("a", 3, 3)]);
    let i = start_target(&c, &serial, route, pool.id, 100, "REFRESH3L8", ("/", true)).await;
    project(&c, &i, 100, &s, &p).await;
    c.finish_native_media(&i, 100, &p).await.unwrap();
    let original = file(&c, &serial, route, pool.id, "/docs/a").await;
    for (id, generation, size) in [(Uuid::new_v4(), 6, 3), (volume, 4, 3), (volume, 5, 7)] {
        let (_, bad) = source_at(&root, id, generation, &[("a", 3, size)]);
        let r = start_target(&c, &serial, route, pool.id, 100, "REFRESH3L8", ("/", true)).await;
        assert!(matches!(
            c.bind_native_media_source(&r, 100, &bad).await,
            Err(CatalogError::Invalid(_))
        ));
        c.abort_native_media(&r, 100, Some("invalid source"))
            .await
            .unwrap();
        while !c.rollback_native_media_page(&r, 100).await.unwrap() {}
        assert_eq!(file(&c, &serial, route, pool.id, "/docs/a").await, original);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "需要隔离FDB；多页刷新和中途owner接任，流式进度不重建命名空间，不触硬件"]
async fn assigned_refresh_multiple_pages_resume_under_original_source_and_request() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let (serial, route, pool) = fixture(&c).await;
    let root = std::env::temp_dir().join(format!("refresh-pages-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let volume = Uuid::new_v4();
    let names = (0..37).map(|n| format!("file-{n:03}")).collect::<Vec<_>>();
    let files = names
        .iter()
        .enumerate()
        .map(|(n, name)| (name.as_str(), n as u64 + 3, 3))
        .collect::<Vec<_>>();
    let (s, p) = source_at(&root, volume, 1, &files);
    let i = start_target(
        &c,
        &serial,
        route,
        pool.id,
        100,
        "REFRESH4L8",
        ("/paged", true),
    )
    .await;
    project(&c, &i, 100, &s, &p).await;
    c.finish_native_media(&i, 100, &p).await.unwrap();
    let mut newer = names
        .iter()
        .enumerate()
        .filter(|(n, _)| n % 2 == 0)
        .map(|(n, name)| (name.as_str(), n as u64 + 3, 7))
        .collect::<Vec<_>>();
    newer.push(("extra", 40, 9));
    let (s2, p2) = source_at(&root, volume, 2, &newer);
    let r = start_target(
        &c,
        &serial,
        route,
        pool.id,
        100,
        "REFRESH4L8",
        ("/IMPORT", false),
    )
    .await;
    c.bind_native_media_source(&r, 100, &p2).await.unwrap();
    let entries = target_entries(&r, &s2);
    for e in &entries {
        c.project_native_media_entry(&r, 100, e, false)
            .await
            .unwrap();
    }
    c.begin_native_media_projection(&r, 100).await.unwrap();
    for e in entries.iter().take(10) {
        c.project_native_media_entry(&r, 100, e, true)
            .await
            .unwrap();
    }
    assert_eq!(c.native_media_status(&r, 100).await.unwrap()["rows"], 10);
    drop(c);
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    c.claim_active_owner_epoch(&serial, 101).await.unwrap();
    let resumed = c
        .start_native_media(
            &serial,
            route,
            101,
            r.request.epoch(),
            r.request.operation(),
        )
        .await
        .unwrap()
        .unwrap();
    c.bind_native_media_source(&resumed, 101, &p2)
        .await
        .unwrap();
    // Completed pages replay exactly; the remaining original entries continue.
    for e in &entries {
        c.project_native_media_entry(&resumed, 101, e, true)
            .await
            .unwrap();
    }
    assert_eq!(
        c.native_media_status(&resumed, 101).await.unwrap()["rows"],
        p2.paths
    );
    c.finish_native_media(&resumed, 101, &p2).await.unwrap();
    for (n, name) in names.iter().enumerate() {
        let found = file(&c, &serial, route, pool.id, &format!("/paged/docs/{name}")).await;
        if n % 2 == 0 {
            assert!(matches!(found,Some(ResolvedPath::File{head,..}) if head.size==7));
        } else {
            assert!(found.is_none());
        }
    }
    assert!(
        file(&c, &serial, route, pool.id, "/paged/docs/extra")
            .await
            .is_some()
    );
    assert!(
        file(&c, &serial, route, pool.id, "/IMPORT/REFRESH4L8")
            .await
            .is_none()
    );
    std::fs::remove_dir_all(root).unwrap();
}
