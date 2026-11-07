use super::*;
use nix::libc;
use tape_rs::tapefs::{FdbBackend, ROOT_INO, TapeFs};

#[test]
#[ignore = "需要隔离 FDB；已删除文件不得阻断同卷其他文件改名，仅模拟磁带"]
fn fuse_file_rename_after_unlink_preserves_retired_history() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[1]:{"online":false},BARCODES[3]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let backend = FdbBackend::new_writable(
        FdbClient::new([&server.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(5)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &f.root.join("unlink-rename-journals"),
    )
    .unwrap();
    let fs = TapeFs::new(backend, f.root.join("unlink-rename-cache")).unwrap();
    let (_, writer) = fs.create(ROOT_INO, "delete-me", true).unwrap();
    fs.write(writer, 0, b"temporary").unwrap();
    fs.fsync(writer).unwrap();
    fs.release(writer);
    fs.unlink(ROOT_INO, "delete-me").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "delete-me"), Err(libc::ENOENT));
    let source = fs.lookup(ROOT_INO, "file-0").unwrap();
    fs.rename(ROOT_INO, "file-0", ROOT_INO, "renamed", false)
        .expect("同卷已删除文件不得使后续 rename 放弃 owner");
    assert_eq!(fs.lookup(ROOT_INO, "renamed").unwrap().ino, source.ino);
    assert_eq!(fs.lookup(ROOT_INO, "file-0"), Err(libc::ENOENT));
    assert_eq!(fs.lookup(ROOT_INO, "delete-me"), Err(libc::ENOENT));
    drop(fs);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；FUSE core 改名、原 writer/reader 与目录子树，仅模拟磁带"]
fn fuse_rename_flushes_source_and_rebinds_only_original_writers_and_descendants() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(15), Duration::from_secs(10));
    let backend = FdbBackend::new_writable(
        FdbClient::new([&server.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(15), Duration::from_secs(10)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &f.root.join("fuse-rename-journals"),
    )
    .unwrap();
    let fs = TapeFs::new(backend, f.root.join("fuse-rename-cache")).unwrap();
    let source = fs.lookup(ROOT_INO, "file-0").unwrap();
    let target = fs.lookup(ROOT_INO, "file-1").unwrap();
    let old_reader = fs.open(target.ino, libc::O_RDONLY).unwrap();
    let old_writer = fs.open(target.ino, libc::O_RDWR).unwrap();
    fs.write(old_writer, 0, b"orphan123").unwrap();
    let source_writer = fs.open(source.ino, libc::O_RDWR).unwrap();
    fs.write(source_writer, 0, b"renamed00").unwrap();
    fs.rename(ROOT_INO, "file-0", ROOT_INO, "file-1", false)
        .unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "file-1").unwrap().ino, source.ino);
    assert_eq!(fs.lookup(ROOT_INO, "file-0"), Err(libc::ENOENT));
    assert_eq!(fs.read(old_reader, 0, 9).unwrap(), b"orphan123");
    fs.write(source_writer, 0, b"after-ren").unwrap();
    fs.fsync(source_writer).unwrap();
    fs.fsync(old_writer).unwrap();
    let output = f.root.join("file-rename-readback");
    client
        .download_new(&f.serial, f.pools[0], "/file-1", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"after-ren");
    assert_eq!(client.stat(&f.serial, f.pools[0], "/file-0").unwrap(), None);
    fs.release(source_writer);
    fs.release(old_writer);
    fs.release(old_reader);
    let directory = fs.mkdir(ROOT_INO, "dir").unwrap();
    let count = fs::read_dir(f.root.join("fuse-rename-journals"))
        .unwrap()
        .count();
    assert_eq!(
        fs.rename(ROOT_INO, "file-1", ROOT_INO, "dir", true),
        Err(libc::EEXIST)
    );
    assert_eq!(
        fs.rename(ROOT_INO, "file-1", ROOT_INO, "dir", false),
        Err(libc::EISDIR)
    );
    assert_eq!(
        fs.rename(ROOT_INO, "dir", ROOT_INO, "file-1", false),
        Err(libc::ENOTDIR)
    );
    assert_eq!(
        fs.rename(ROOT_INO, "file-1", ROOT_INO, "bad/name", false),
        Err(libc::EINVAL)
    );
    assert_eq!(
        fs::read_dir(f.root.join("fuse-rename-journals"))
            .unwrap()
            .count(),
        count
    );
    let (child, child_writer) = fs.create(directory.ino, "child", true).unwrap();
    fs.write(child_writer, 0, b"subtree").unwrap();
    fs.rename(ROOT_INO, "dir", ROOT_INO, "moved", true).unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "moved").unwrap().ino, directory.ino);
    assert_eq!(fs.lookup(directory.ino, "child").unwrap().ino, child.ino);
    fs.write(child_writer, 0, b"nextsub").unwrap();
    fs.fsync(child_writer).unwrap();
    let output = f.root.join("directory-rename-readback");
    client
        .download_new(&f.serial, f.pools[0], "/moved/child", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"nextsub");
    fs.release(child_writer);
    let closed = fs.lookup(ROOT_INO, "file-3").unwrap();
    let first = fs.open(closed.ino, libc::O_RDWR).unwrap();
    fs.write(first, 0, b"closed000").unwrap();
    fs.release(first);
    let reopened = fs.open(closed.ino, libc::O_RDWR).unwrap();
    fs.rename(ROOT_INO, "file-3", ROOT_INO, "closed", true)
        .unwrap();
    assert_eq!(
        client
            .stat(&f.serial, f.pools[0], "/closed")
            .unwrap()
            .unwrap()["version"],
        3
    );
    assert_eq!(fs.read(reopened, 0, 9).unwrap(), b"closed000");
    fs.release(reopened);
    drop(fs);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；丢失改名响应后根 sync 继续原操作，仅模拟磁带"]
fn fuse_rename_root_sync_resolves_original_lost_result_without_rebinding() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = super::super::fuse_writing::UploadProxy::new(server.address.clone());
    let journals = f.root.join("lost-rename-journals");
    let backend = FdbBackend::new_writable(
        FdbClient::new([&proxy.address])
            .unwrap()
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(3)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &journals,
    )
    .unwrap();
    let fs = TapeFs::new(backend, f.root.join("lost-rename-cache")).unwrap();
    let source = fs.lookup(ROOT_INO, "file-0").unwrap();
    let handle = fs.open(source.ino, libc::O_RDWR).unwrap();
    proxy.mode.store(6, Ordering::SeqCst);
    assert_eq!(
        fs.rename(ROOT_INO, "file-0", ROOT_INO, "renamed", true),
        Err(libc::EIO)
    );
    assert_eq!(fs.lookup(ROOT_INO, "file-0"), Err(libc::EIO));
    assert_eq!(fs.write(handle, 0, b"blocked"), Err(libc::EIO));
    assert_eq!(fs.read(handle, 0, 9).unwrap(), b"content-0");
    let saved: Vec<_> = fs::read_dir(&journals)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("rename-")
        })
        .collect();
    assert_eq!(saved.len(), 1);
    let posts = proxy.renames.load(Ordering::SeqCst);
    assert!(posts > 0);
    let original = fs::read(saved[0].join("request.json")).unwrap();
    assert!(!saved[0].join("receipt.json").exists());
    proxy.mode.store(0, Ordering::SeqCst);
    fs.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
        .unwrap();
    assert_eq!(fs::read(saved[0].join("request.json")).unwrap(), original);
    assert_eq!(proxy.renames.load(Ordering::SeqCst), posts);
    assert!(saved[0].join("receipt.json").exists());
    assert_eq!(fs.lookup(ROOT_INO, "renamed").unwrap().ino, source.ino);
    assert_eq!(fs.lookup(ROOT_INO, "file-0"), Err(libc::ENOENT));
    fs.write(handle, 0, b"continued").unwrap();
    fs.fsync(handle).unwrap();
    let writes = owner
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c[0] == 0x0a)
        .count();
    fs.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
        .unwrap();
    assert_eq!(
        owner
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x0a)
            .count(),
        writes
    );
    let output = f.root.join("lost-rename-readback");
    FdbClient::new([&server.address])
        .unwrap()
        .download_new(&f.serial, f.pools[0], "/renamed", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"continued");
    fs.release(handle);
    drop(fs);
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB、默认 FUSE 二进制和本机 FUSE；改名系统调用及进程崩溃恢复，仅模拟磁带"]
fn default_fuse_renames_open_files_subtrees_and_recovers_lost_result_after_crash() {
    use super::super::fuse_writing::{UploadProxy, mount_writable};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let mut mount = mount_writable(&f, &proxy);
    let result = std::process::Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import os,sys,ctypes,errno
p=sys.argv[1]; a=p+'/file-0'; b=p+'/file-1'
s=os.open(a,os.O_RDWR);t=os.open(b,os.O_RDWR)
ino=os.fstat(s).st_ino
os.pwrite(s,b'renamed00',0);os.pwrite(t,b'orphan123',0)
os.rename(a,b)
assert os.stat(b).st_ino==ino and not os.path.exists(a)
assert os.pread(t,9,0)==b'orphan123'
os.pwrite(s,b'after-ren',0);os.fsync(s);os.fsync(t)
assert open(b,'rb').read()==b'after-ren'
os.close(t);os.close(s)
d=p+'/dir';os.mkdir(d);c=os.open(d+'/child',os.O_CREAT|os.O_RDWR,0o644)
ino=os.fstat(c).st_ino;os.write(c,b'subtree')
os.rename(d,p+'/moved')
assert os.stat(p+'/moved/child').st_ino==ino
os.pwrite(c,b'nextsub',0);os.fsync(c);os.close(c)
assert open(p+'/moved/child','rb').read()==b'nextsub'
libc=ctypes.CDLL(None,use_errno=True)
libc.renameat2.argtypes=[ctypes.c_int,ctypes.c_char_p,ctypes.c_int,ctypes.c_char_p,ctypes.c_uint]
assert libc.renameat2(-100,os.fsencode(b),-100,os.fsencode(p+'/moved'),1)==-1
assert ctypes.get_errno()==errno.EEXIST
assert open(b,'rb').read()==b'after-ren'
"#,
        )
        .arg(&mount.path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let posts = proxy.renames.load(Ordering::SeqCst);
    proxy.mode.store(6, Ordering::SeqCst);
    let from = mount.path.join("file-1");
    let to = mount.path.join("crash-renamed");
    let worker = std::thread::spawn(move || fs::rename(from, to));
    let journal_root = f.root.join("kernel-journals");
    let deadline = Instant::now() + Duration::from_secs(15);
    let directory = loop {
        if proxy.renames.load(Ordering::SeqCst) > posts {
            let paths: Vec<_> = fs::read_dir(&journal_root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    if !p
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with("rename-")
                    {
                        return false;
                    }
                    let v: serde_json::Value =
                        serde_json::from_slice(&fs::read(p.join("request.json")).unwrap()).unwrap();
                    v["original"]["spec"]["to"] == "/crash-renamed"
                })
                .collect();
            assert_eq!(paths.len(), 1);
            break paths.into_iter().next().unwrap();
        }
        assert!(Instant::now() < deadline, "未观察到原改名登记");
        std::thread::sleep(Duration::from_millis(20));
    };
    let header = fs::read(directory.join("request.json")).unwrap();
    assert!(!directory.join("receipt.json").exists());
    mount.child.kill().unwrap();
    mount.child.wait().unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(
        std::process::Command::new("fusermount3")
            .args(["-u", "--"])
            .arg(&mount.path)
            .status()
            .unwrap()
            .success()
    );
    drop(mount);
    let admitted_posts = proxy.renames.load(Ordering::SeqCst);
    proxy.mode.store(0, Ordering::SeqCst);
    let mut mount = mount_writable(&f, &proxy);
    assert_eq!(
        fs::read(mount.path.join("crash-renamed")).unwrap(),
        b"after-ren"
    );
    assert!(!mount.path.join("file-1").exists());
    assert_eq!(fs::read(directory.join("request.json")).unwrap(), header);
    let original: serde_json::Value = serde_json::from_slice(&header).unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["receipt"]["operation"],
        original["original"]["operation"]
    );
    assert_eq!(proxy.renames.load(Ordering::SeqCst), admitted_posts);
    mount.finish();
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}
