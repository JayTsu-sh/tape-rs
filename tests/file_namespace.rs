use tape_rs::daemon::files::{BatchPolicy, FileService, ServiceError, TapeIdent, TapeLimits};
use tape_rs::ltfs::index::{FileNode, LtfsIndex};

struct Fixture {
    files: std::sync::Arc<FileService>,
    dir: std::path::PathBuf,
}

impl Fixture {
    fn new(index: &LtfsIndex) -> Self {
        let dir = std::env::temp_dir().join(format!("file-namespace-{}", uuid::Uuid::new_v4()));
        let files = FileService::with_options(dir.clone(), BatchPolicy::default(), None).unwrap();
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
            index,
            1024,
            true,
        );
        Self { files, dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.files.close("test completed");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn empty() -> LtfsIndex {
    LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b')
}

#[test]
fn pending_ancestors_and_descendants_conflict_but_siblings_do_not() {
    let f = Fixture::new(&empty());
    let parent = f.files.begin("/a", 1).unwrap();
    assert!(matches!(
        f.files.begin("/a/b", 1),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.delete("/a/b"),
        Err(ServiceError::PathBusy(_))
    ));
    let unrelated = f.files.begin("/ab", 1).unwrap();
    f.files.abort(unrelated);
    f.files.abort(parent);

    let child = f.files.begin("/a/b", 1).unwrap();
    assert!(matches!(
        f.files.begin("/a", 1),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.delete("/a"),
        Err(ServiceError::PathBusy(_))
    ));
    let sibling = f.files.begin("/a/c", 1).unwrap();
    f.files.abort(child);
    f.files.abort(sibling);
    let retry = f.files.begin("/a", 1).unwrap();
    f.files.abort(retry);
}

#[test]
fn file_ancestors_and_native_empty_directories_cannot_be_overwritten() {
    let mut index = empty();
    index.root.files.push(FileNode {
        name: "file".into(),
        ..Default::default()
    });
    index.create_directory("/empty").unwrap();
    let f = Fixture::new(&index);
    assert!(matches!(
        f.files.begin("/file/child", 1),
        Err(ServiceError::NotDirectory(_))
    ));
    assert!(matches!(
        f.files.begin("/empty", 1),
        Err(ServiceError::IsDirectory(_))
    ));
    assert!(matches!(
        f.files.delete("/empty"),
        Err(ServiceError::IsDirectory(_))
    ));
    let child = f.files.begin("/empty/child", 1).unwrap();
    f.files.abort(child);
    assert!(f.files.take_batch_now(1).is_none());
}

#[test]
fn nul_is_rejected_before_any_reservation() {
    let f = Fixture::new(&empty());
    assert!(matches!(
        f.files.begin("/a\0b", 1),
        Err(ServiceError::BadPath(_))
    ));
    assert!(f.files.take_batch_now(1).is_none());
}

#[test]
fn simultaneous_parent_and_child_admission_has_exactly_one_winner() {
    let f = Fixture::new(&empty());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = ["/race", "/race/child"]
        .into_iter()
        .map(|path| {
            let files = f.files.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                files.begin(path, 1)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(ServiceError::PathBusy(_))))
            .count(),
        1
    );
    for handle in results.into_iter().flatten() {
        f.files.abort(handle);
    }
}

#[test]
fn catalog_checks_use_component_boundaries_and_recent_tombstones() {
    let dir = std::env::temp_dir().join(format!("namespace-catalog-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("catalog.db");
    drop(tape_rs::daemon::directory::Directory::open(&db).unwrap());
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (path, deleted) in [
        ("/remote/file", false),
        ("/blocking", false),
        ("/deleted", true),
        ("/abc/file", false),
        ("/dXX/file", false),
    ] {
        conn.execute("INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,deleted) VALUES('pool',?1,'OTHER',1,0,'',?2)", rusqlite::params![path, deleted]).unwrap();
    }
    let files =
        FileService::with_options(dir.join("spool"), BatchPolicy::default(), Some(db)).unwrap();
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
        &empty(),
        1024,
        true,
    );
    assert!(matches!(
        files.begin("/remote", 1),
        Err(ServiceError::IsDirectory(_))
    ));
    assert!(matches!(
        files.begin("/blocking/child", 1),
        Err(ServiceError::NotDirectory(_))
    ));
    for path in ["/deleted/child", "/ab", "/d_%"] {
        let h = files.begin(path, 1).unwrap();
        files.abort(h);
    }
    assert!(matches!(
        files.rename("/blocking", "/renamed", false),
        Err(ServiceError::CrossDevice(_))
    ));
    // 磁带已提交删除，但 Raft 的目录库还没追上；recent 墓碑应遮住数据库里的旧祖先。
    files.delete("/blocking").unwrap();
    let (batch, uploads) = files.take_batch_now(1).unwrap();
    assert!(matches!(
        files.begin("/blocking/child", 1),
        Err(ServiceError::PathBusy(_))
    ));
    files.batch_done(1, &batch, &uploads, Ok((2, Default::default())));
    let h = files.begin("/blocking/child", 1).unwrap();
    files.abort(h);
    files.close("test completed");
    drop(files);
    drop(conn);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_tasks_publish_only_after_commit_and_failed_batches_stay_unknown() {
    use tape_rs::daemon::files::{TaskStatus, catalog_of};
    let f = Fixture::new(&empty());
    let task = f.files.mkdir("/empty").unwrap();
    assert_eq!(f.files.task(task), Some(TaskStatus::Staged));
    assert!(f.files.stat_full("/empty").unwrap().in_flight.is_none());
    assert!(f.files.list_dir("/", true).unwrap().unwrap().is_empty());
    assert!(matches!(
        f.files.begin("/empty/child", 1),
        Err(ServiceError::PathBusy(_))
    ));
    let (batch, uploads) = f.files.take_batch_now(1).unwrap();
    let mut committed = empty();
    committed.create_directory("/empty").unwrap();
    let records = catalog_of(&committed)
        .0
        .into_iter()
        .map(|r| (r.path.clone(), r))
        .collect();
    f.files.batch_done(1, &batch, &uploads, Ok((2, records)));
    assert_eq!(
        f.files.task(task),
        Some(TaskStatus::Committed { generation: 2 })
    );
    assert!(
        f.files
            .list_dir("/empty", false)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    let remove = f.files.rmdir("/empty").unwrap();
    let (batch, uploads) = f.files.take_batch_now(1).unwrap();
    f.files
        .batch_done(1, &batch, &uploads, Err("barrier failed".into()));
    assert!(matches!(
        f.files.task(remove),
        Some(TaskStatus::Indeterminate { .. })
    ));
    assert!(f.files.stat("/empty").unwrap().is_some());
    f.files.close("failure requires recovery");
}

#[test]
fn pending_directory_creation_is_not_a_durable_success_on_close() {
    use tape_rs::daemon::files::TaskStatus;
    let f = Fixture::new(&empty());
    let task = f.files.mkdir("/not-committed").unwrap();
    f.files.close("takeover");
    assert!(matches!(
        f.files.task(task),
        Some(TaskStatus::Failed { .. })
    ));
}

#[test]
fn rename_reserves_both_subtrees_and_rejects_invalid_targets() {
    let mut index = empty();
    index.create_directory("/source").unwrap();
    index.create_directory("/source/empty").unwrap();
    index.create_directory("/target").unwrap();
    let f = Fixture::new(&index);
    assert!(matches!(
        f.files.rename("/source", "/source/child", false),
        Err(ServiceError::BadPath(_))
    ));
    assert!(matches!(
        f.files.rename("/source", "/target", true),
        Err(ServiceError::Exists(_))
    ));
    let task = f.files.rename("/source", "/target", false).unwrap();
    assert!(f.files.stat("/source/empty").unwrap().is_some());
    assert!(f.files.stat("/target/empty").unwrap().is_none());
    assert!(matches!(
        f.files.begin("/source/new", 0),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.mkdir("/target/new"),
        Err(ServiceError::PathBusy(_))
    ));
    let (batch, uploads) = f.files.take_batch_now(1).unwrap();
    assert_eq!(uploads.len(), 4);
    f.files
        .batch_done(1, &batch, &uploads, Err("索引提交未确认".into()));
    assert!(matches!(
        f.files.task(task),
        Some(tape_rs::daemon::files::TaskStatus::Indeterminate { .. })
    ));
}

#[test]
fn foreign_rename_back_keeps_native_nodes_visible_over_private_tombstones() {
    use tape_rs::daemon::files::catalog_of;
    use tape_rs::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION};
    let mut index = empty();
    index.create_directory("/empty").unwrap();
    let mut live = FileNode {
        name: "file".into(),
        length: 7,
        ..Default::default()
    };
    live.set_xattr(XATTR_VERSION, "59.8");
    index.root.subdirs[0].files.push(live);
    index.create_directory("/.tapers").unwrap();
    index.create_directory("/.tapers/deleted").unwrap();
    let deleted = &mut index
        .root
        .subdirs
        .iter_mut()
        .find(|d| d.name == ".tapers")
        .unwrap()
        .subdirs[0];
    for (name, path) in [("f", "/empty/file"), ("d", "/empty"), ("gone", "/gone")] {
        let mut tomb = FileNode {
            name: name.into(),
            ..Default::default()
        };
        tomb.set_xattr(XATTR_VERSION, "59.9");
        tomb.set_xattr(XATTR_DELETED_PATH, path);
        deleted.files.push(tomb);
    }
    // LE preserves tapers.version and private tombstones when restoring old names.
    // Exercise either traversal order, including an unversioned native directory.
    for _ in 0..2 {
        let (records, bytes) = catalog_of(&index);
        assert_eq!(bytes, 7);
        let file = records.iter().find(|r| r.path == "/empty/file").unwrap();
        assert!(!file.deleted);
        assert_eq!(file.version, (59, 8)); // Never promote a foreign edit over another tape.
        assert!(!records.iter().find(|r| r.path == "/empty").unwrap().deleted);
        assert!(records.iter().find(|r| r.path == "/gone").unwrap().deleted);
        let f = Fixture::new(&index);
        assert!(
            f.files
                .stat_full("/empty/file")
                .unwrap()
                .committed
                .is_some()
        );
        assert!(f.files.list_dir("/empty", false).unwrap().is_some());
        index.root.subdirs.reverse();
    }
}

#[test]
fn xattr_changes_reserve_metadata_without_data_capacity_and_fail_closed() {
    use tape_rs::daemon::files::TaskStatus;
    let mut index = empty();
    index.root.files.push(FileNode {
        name: "file".into(),
        length: 2048,
        ..Default::default()
    });
    index.create_directory("/dir").unwrap();
    let f = Fixture::new(&index);
    assert!(matches!(
        f.files.change_xattr("/file", "user.missing", None, 0),
        Err(ServiceError::NoAttribute(_))
    ));
    assert!(matches!(
        f.files.change_xattr("/file", "user.missing", Some(b"x"), 2),
        Err(ServiceError::NoAttribute(_))
    ));
    for name in [
        "user.tapers.version",
        "user.ltfs.hash.sha256sum",
        "user.tape.state",
        "security.foo",
    ] {
        assert!(matches!(
            f.files.change_xattr("/file", name, Some(b"x"), 0),
            Err(ServiceError::ProtectedAttribute(_))
        ));
    }
    assert!(matches!(
        f.files.change_xattr("/file", "user.note", Some(b"x"), 3),
        Err(ServiceError::BadPath(_))
    ));
    assert!(matches!(
        f.files
            .change_xattr("/file", "user.note", Some(&vec![0; 65537]), 0),
        Err(ServiceError::AttributeTooLarge)
    ));
    let task = f
        .files
        .change_xattr("/file", "user.note", Some(b"\0\xff"), 1)
        .unwrap();
    assert!(matches!(
        f.files.delete("/file"),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.rename("/file", "/other", false),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.change_xattr("/file", "user.other", Some(b"x"), 0),
        Err(ServiceError::PathBusy(_))
    ));
    assert_eq!(f.files.stat("/file").unwrap().unwrap().len, 2048);
    let (batch, uploads) = f.files.take_batch_now(1).unwrap();
    f.files
        .batch_done(1, &batch, &uploads, Err("索引提交未确认".into()));
    assert!(matches!(
        f.files.task(task),
        Some(TaskStatus::Indeterminate { .. })
    ));
    assert_eq!(f.files.stat("/file").unwrap().unwrap().len, 2048);
}

#[test]
fn catalog_preserves_symlink_identity_without_changing_target_file_metadata() {
    let mut index = empty();
    index.root.files.push(FileNode {
        name: "target".into(),
        length: 42,
        ..Default::default()
    });
    index.root.files.push(FileNode {
        name: "link".into(),
        symlink: Some("target".into()),
        ..Default::default()
    });
    let (records, bytes) = tape_rs::daemon::files::catalog_of(&index);
    assert_eq!(bytes, 42);
    let link = records.iter().find(|r| r.path == "/link").unwrap();
    assert_eq!(link.length, 0);
    assert_eq!(link.metadata["kind"], "symlink");
    assert_eq!(link.metadata["symlink"], "target");
    let target = records.iter().find(|r| r.path == "/target").unwrap();
    assert!(target.metadata.get("kind").is_none());
    assert!(target.metadata.get("symlink").is_none());
}

#[test]
fn symlink_creation_validates_and_reserves_only_the_link_path() {
    let mut index = empty();
    index.root.files.push(FileNode {
        name: "target".into(),
        length: 42,
        ..Default::default()
    });
    let f = Fixture::new(&index);
    for target in ["", "bad\0target", "bad\u{1}target", "bad\u{fffe}target"] {
        assert!(matches!(
            f.files.symlink(target, "/bad"),
            Err(ServiceError::BadPath(_))
        ));
    }
    assert!(matches!(
        f.files.symlink("missing", "/target"),
        Err(ServiceError::Exists(_))
    ));
    assert!(matches!(
        f.files.symlink("missing", "/target/child"),
        Err(ServiceError::NotDirectory(_))
    ));
    assert!(matches!(
        f.files.symlink("missing", "/absent/link"),
        Err(ServiceError::NotFound(_))
    ));
    f.files.symlink("../missing & 中文", "/link").unwrap();
    assert!(matches!(
        f.files.mkdir("/link"),
        Err(ServiceError::PathBusy(_))
    ));
    assert!(matches!(
        f.files.begin("/link/child", 0),
        Err(ServiceError::PathBusy(_))
    ));
    assert_eq!(f.files.stat("/target").unwrap().unwrap().len, 42);
    let (_, uploads) = f.files.take_batch_now(1).unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].path, "/link");
    assert_eq!(uploads[0].len, 0);
    assert!(uploads[0].spool.as_os_str().is_empty());
}
