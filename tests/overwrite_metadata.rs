use tape_rs::daemon::files::{BatchPolicy, FileService, ServiceError, TapeIdent, TapeLimits};
use tape_rs::ltfs::index::LtfsIndex;

#[test]
fn unavailable_catalog_aborts_upload_instead_of_dropping_metadata() {
    let dir = std::env::temp_dir().join(format!("overwrite-metadata-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    drop(tape_rs::daemon::directory::Directory::open(&dir.join("missing.db")).unwrap());
    let files = FileService::with_options(
        dir.join("spool"),
        BatchPolicy::default(),
        Some(dir.join("missing.db")),
    )
    .unwrap();
    files.open(
        1,
        TapeIdent {
            barcode: "TEST".into(),
            pool_uuid: "pool".into(),
        },
        TapeLimits {
            file_limit: 100,
            usable_capacity: 1024,
        },
        &LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b'),
        1024,
        true,
    );
    let h = files.begin("/possibly-on-another-tape", 1).unwrap();
    std::fs::remove_file(dir.join("missing.db")).unwrap();
    std::fs::write(&h.spool, b"x").unwrap();
    let spool = h.spool.clone();
    files.ingest(&h, 1).unwrap();
    assert!(matches!(files.finish(h, 1), Err(ServiceError::Io(_))));
    assert!(!spool.exists());
    assert!(files.take_batch_now(1).is_none());
    // 错误路径必须释放预留，不能永久卡住同名文件。
    assert!(matches!(
        files.begin("/possibly-on-another-tape", 1),
        Err(ServiceError::Io(_))
    ));
    drop(tape_rs::daemon::directory::Directory::open(&dir.join("missing.db")).unwrap());
    let retry = files.begin("/possibly-on-another-tape", 1).unwrap();
    files.abort(retry);
    files.close("test completed");
    drop(files);
    std::fs::remove_dir_all(dir).unwrap();
}
