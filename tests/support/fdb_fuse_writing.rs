//! Writable filesystem contract on isolated FDB and simulated media only.
use super::*;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, AtomicUsize};
use tape_rs::client::fdb::{FdbClient, UploadCondition, UploadJournal};
use tape_rs::tapefs::{FdbBackend, ROOT_INO, TapeFs};

pub(super) struct UploadProxy {
    pub(super) address: String,
    pub(super) mode: Arc<AtomicU8>,
    puts: Arc<AtomicUsize>,
    nodes: Arc<AtomicUsize>,
    pub(super) renames: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl UploadProxy {
    pub(super) fn new(target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let mode = Arc::new(AtomicU8::new(0));
        let puts = Arc::new(AtomicUsize::new(0));
        let nodes = Arc::new(AtomicUsize::new(0));
        let worker_nodes = nodes.clone();
        let renames = Arc::new(AtomicUsize::new(0));
        let worker_renames = renames.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_mode, worker_puts, worker_stop) = (mode.clone(), puts.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            let mut node_faulted = false;
            let mut rename_faulted = false;
            'connections: loop {
                let (mut downstream, _) = listener.accept().unwrap();
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                downstream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut input = BufReader::new(downstream.try_clone().unwrap());
                let mut first = String::new();
                match input.read_line(&mut first) {
                    Ok(0) => continue,
                    Ok(_) => {}
                    Err(error) if proxy_client_gone(&error) => continue,
                    Err(error) => panic!("读取代理请求行: {error}"),
                }
                if !first.ends_with("\r\n") {
                    continue;
                }
                let mut header = first.as_bytes().to_vec();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    match input.read_line(&mut line) {
                        Ok(0) => continue 'connections,
                        Ok(_) => {}
                        Err(error) if proxy_client_gone(&error) => continue 'connections,
                        Err(error) => panic!("读取代理请求头: {error}"),
                    }
                    if !line.ends_with("\r\n") {
                        continue 'connections;
                    }
                    header.extend_from_slice(line.as_bytes());
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                if let Err(error) = input.read_exact(&mut body) {
                    assert!(proxy_client_gone(&error), "读取代理请求体: {error}");
                    continue;
                }
                let mode = worker_mode.load(Ordering::SeqCst);
                if mode != 4 {
                    node_faulted = false;
                }
                if mode != 6 {
                    rename_faulted = false;
                }
                let rename = first.starts_with("POST /v1/fdb/renames/")
                    && first
                        .split_whitespace()
                        .nth(1)
                        .is_some_and(|p| p.ends_with("/requests"));
                if mode == 6
                    && rename_faulted
                    && first.starts_with("GET /v1/fdb/renames/")
                    && first.contains("/requests/")
                {
                    proxy_reply(&mut downstream, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                let put = first.starts_with("PUT ");
                let node = first.starts_with("POST /v1/fdb/nodes/")
                    && first
                        .split_whitespace()
                        .nth(1)
                        .is_some_and(|p| p.ends_with("/requests"));
                if (mode == 4
                    && first.starts_with("GET /v1/fdb/nodes/")
                    && first.contains("/requests/")
                    && node_faulted)
                    || (mode == 5 && node)
                {
                    proxy_reply(&mut downstream, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                if mode == 3 && first.starts_with("GET /v1/fdb/upload-receipts/") {
                    let body = r#"{"status":"unconfirmed"}"#;
                    proxy_reply(&mut downstream, format!("HTTP/1.1 202 Accepted\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
                    continue;
                }
                if (mode == 1 && first.starts_with("GET /v1/fdb/upload-receipts/"))
                    || (mode == 2 && put)
                {
                    proxy_reply(&mut downstream, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    continue;
                }
                let mut upstream = TcpStream::connect(&target).unwrap();
                upstream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                upstream.write_all(&header).unwrap();
                upstream.write_all(&body).unwrap();
                let answer = read_proxy_response(&mut upstream);
                if rename {
                    worker_renames.fetch_add(1, Ordering::SeqCst);
                }
                if rename && mode == 6 {
                    assert!(
                        answer.starts_with(b"HTTP/1.1 200") || answer.starts_with(b"HTTP/1.1 202")
                    );
                    rename_faulted = true;
                    continue;
                }
                if put {
                    worker_puts.fetch_add(1, Ordering::SeqCst);
                }
                if node {
                    worker_nodes.fetch_add(1, Ordering::SeqCst);
                    if mode == 4 {
                        node_faulted = true;
                        continue;
                    }
                }
                if (mode == 1 || mode == 3) && put {
                    assert!(answer.starts_with(b"HTTP/1.1 201"));
                    // S4 completed on the actual simulated medium; lose its reply.
                    continue;
                }
                proxy_reply(&mut downstream, &answer);
            }
        });
        Self {
            address,
            mode,
            puts,
            nodes,
            renames,
            stop,
            worker: Some(worker),
        }
    }
    fn client(&self) -> FdbClient {
        FdbClient::new([&self.address])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_secs(10))
    }
}
impl Drop for UploadProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn proxy_client_gone(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}
fn proxy_reply(connection: &mut TcpStream, answer: &[u8]) {
    if let Err(error) = connection.write_all(answer) {
        assert!(proxy_client_gone(&error), "写入代理响应: {error}");
    }
}

fn original_record(directory: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(directory.join("request.json")).unwrap()).unwrap()
}
fn published_journal(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let id = name.strip_prefix("node-").unwrap_or(name);
            Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
        })
}
fn upload_directory(root: &std::path::Path, path: &str) -> std::path::PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|directory| published_journal(directory))
        .find(|directory| original_record(directory)["original"]["path"] == path)
        .unwrap()
}
fn prepared_node_directory(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|directory| {
            published_journal(directory)
                && directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("node-"))
        })
        .find(|directory| {
            // Published directories are retained; the producer has already
            // fsynced their header before the atomic directory rename.
            let record = original_record(directory);
            record["original"]["path"] == path && !record["prepared"].is_null()
        })
}

fn change_node(
    client: &FdbClient,
    f: &Fixture,
    path: &str,
    version: Option<u64>,
    mutation: serde_json::Value,
) {
    let saved = f.root.join(format!("write-node-{}", Uuid::new_v4()));
    client
        .prepare_node_mutation(
            &f.serial,
            f.pools[0],
            path,
            original_condition(
                client,
                &f.serial,
                f.route.route.namespace,
                f.pools[0],
                path,
                version,
            ),
            mutation,
            &saved,
        )
        .unwrap();
    assert_eq!(
        client.run_node_mutation_saved(&saved).unwrap()["status"],
        "completed"
    );
}

#[test]
#[ignore = "需要隔离FDB；实际节点修改、原身份与SimLibrary，不接触真实设备"]
fn writable_core_nodes_keep_original_identity_and_resume_lost_results() {
    use tape_rs::ltfs::index::MetadataChange;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_nodes_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let journals = f.root.join("node-mount-journals");
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
    let core = TapeFs::new(backend, f.root.join("node-cache")).unwrap();
    let dir = core.mkdir(ROOT_INO, "nodes").unwrap();
    let link = core.symlink(dir.ino, "link", "../file-0").unwrap();
    assert!(link.is_symlink);
    assert_eq!(core.readlink(link.ino).unwrap(), b"../file-0");
    assert_eq!(core.rmdir(ROOT_INO, "nodes"), Err(nix::libc::ENOTEMPTY));
    core.change_xattr(link.ino, "user.binary", Some(&[0, 255]), 1)
        .unwrap();
    assert_eq!(
        core.getxattr(link.ino, "user.binary").unwrap(),
        vec![0, 255]
    );
    assert_eq!(
        core.change_xattr(link.ino, "user.binary", Some(b"other"), 1),
        Err(nix::libc::EEXIST)
    );
    core.change_xattr(link.ino, "user.binary", None, 0).unwrap();
    assert_eq!(
        core.change_xattr(link.ino, "user.binary", None, 0),
        Err(nix::libc::ENODATA)
    );
    let (file, writer_handle) = core.create(dir.ino, "data", true).unwrap();
    core.write(writer_handle, 0, b"first").unwrap();
    core.change_metadata(
        file.ino,
        &MetadataChange {
            readonly: Some(true),
            access_time: None,
            modify_time: Some("2026-11-01T12:34:56.000000000Z".into()),
        },
    )
    .unwrap();
    core.write(writer_handle, 0, b"after").unwrap();
    core.fsync(writer_handle).unwrap();
    let current = proxy
        .client()
        .stat(&f.serial, f.pools[0], "/nodes/data")
        .unwrap()
        .unwrap();
    assert_eq!(current["version"], 3);
    core.release(writer_handle);
    let reader_handle = core.open(file.ino, nix::libc::O_RDONLY).unwrap();
    core.unlink(dir.ino, "data").unwrap();
    assert_eq!(core.read(reader_handle, 0, 10).unwrap(), b"after");
    core.release(reader_handle);
    core.unlink(dir.ino, "link").unwrap();
    core.rmdir(ROOT_INO, "nodes").unwrap();

    // Completed remote operation, lost registration reply and status queries.
    proxy.mode.store(4, Ordering::SeqCst);
    assert!(core.mkdir(ROOT_INO, "lost-node").is_err());
    let saved = upload_directory(&journals, "/lost-node");
    let header = fs::read(saved.join("request.json")).unwrap();
    let original = original_record(&saved);
    assert!(!saved.join("receipt.json").exists());
    assert!(core.lookup(ROOT_INO, "lost-node").is_err());
    let deadline = Instant::now() + Duration::from_secs(5);
    let direct = FdbClient::new([&server.address]).unwrap();
    while direct
        .stat(&f.serial, f.pools[0], "/lost-node")
        .unwrap()
        .is_none()
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let posts = proxy.nodes.load(Ordering::SeqCst);
    let held = f.root.join("held-node-journals");
    fs::rename(&journals, &held).unwrap();
    fs::create_dir(&journals).unwrap();
    fs::write(journals.join("foreign"), b"untouched").unwrap();
    proxy.mode.store(0, Ordering::SeqCst);
    core.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
        .unwrap();
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    let moved = held.join(saved.file_name().unwrap());
    assert_eq!(fs::read(moved.join("request.json")).unwrap(), header);
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(moved.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["receipt"]["operation"],
        original["original"]["operation"]
    );
    assert!(core.lookup(ROOT_INO, "lost-node").unwrap().is_dir);
    assert_eq!(fs::read(journals.join("foreign")).unwrap(), b"untouched");
    let (deleted, handle) = core.create(ROOT_INO, "lost-delete", true).unwrap();
    core.write(handle, 0, b"original deleted bytes").unwrap();
    core.fsync(handle).unwrap();
    core.release(handle);
    let reader_handle = core.open(deleted.ino, nix::libc::O_RDONLY).unwrap();
    proxy.mode.store(4, Ordering::SeqCst);
    assert!(core.unlink(ROOT_INO, "lost-delete").is_err());
    let deadline = Instant::now() + Duration::from_secs(5);
    while direct
        .stat(&f.serial, f.pools[0], "/lost-delete")
        .unwrap()
        .is_some()
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let replacement = f.root.join("replacement-node-body");
    fs::write(&replacement, b"new identity").unwrap();
    let ready = f.root.join("replacement-node-upload");
    let parent = direct.stat(&f.serial, f.pools[0], "/").unwrap().unwrap();
    direct
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/lost-delete",
            &replacement,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: Uuid::parse_str(parent["id"].as_str().unwrap()).unwrap(),
            },
            None,
            &ready,
        )
        .unwrap();
    direct.upload_saved(&ready).unwrap();
    let posts = proxy.nodes.load(Ordering::SeqCst);
    proxy.mode.store(0, Ordering::SeqCst);
    core.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
        .unwrap();
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    assert_eq!(
        core.read(reader_handle, 0, 100).unwrap(),
        b"original deleted bytes"
    );
    let replacement = core.lookup(ROOT_INO, "lost-delete").unwrap();
    assert_ne!(replacement.ino, deleted.ino);
    let handle = core.open(replacement.ino, nix::libc::O_RDONLY).unwrap();
    assert_eq!(core.read(handle, 0, 100).unwrap(), b"new identity");
    core.release(handle);
    core.release(reader_handle);
    drop(core);
    // Both file and node journals recover through the same held store.
    let restarted = FdbBackend::new_writable(
        direct,
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &held,
    )
    .unwrap();
    drop(restarted);
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    let condition = original_condition(
        &FdbClient::new([&server.address]).unwrap(),
        &f.serial,
        f.route.route.namespace,
        f.pools[0],
        "/never-admit",
        None,
    );
    let foreign = held.join(format!("node-{}", Uuid::new_v4()));
    let saved = tape_rs::client::fdb::NodeJournal::create(
        "OTHER",
        f.pools[0],
        "/foreign",
        condition,
        json!({"kind":"create-directory"}),
        &foreign,
    )
    .unwrap();
    drop(saved);
    assert!(
        FdbBackend::new_writable(
            proxy.client(),
            f.serial.clone(),
            f.route.route.namespace,
            f.pools[0],
            &held
        )
        .is_err()
    );
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    fs::remove_dir_all(&foreign).unwrap();
    let draft = held.join(format!("node-{}", Uuid::new_v4()));
    let saved = tape_rs::client::fdb::NodeJournal::create(
        &f.serial,
        f.pools[0],
        "/never-admit",
        condition,
        json!({"kind":"create-directory"}),
        &draft,
    )
    .unwrap();
    drop(saved);
    let receipt = moved.join("receipt.json");
    let original = fs::read(&receipt).unwrap();
    fs::write(&receipt, b"corrupt").unwrap();
    assert!(
        FdbBackend::new_writable(
            proxy.client(),
            f.serial.clone(),
            f.route.route.namespace,
            f.pools[0],
            &held
        )
        .is_err()
    );
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    assert!(
        proxy
            .client()
            .stat(&f.serial, f.pools[0], "/never-admit")
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(&receipt).unwrap(), b"corrupt");
    fs::write(&receipt, original).unwrap();
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB、默认FUSE二进制和本机FUSE；节点系统调用与崩溃恢复，不接触真实设备"]
fn default_fuse_mutates_nodes_and_recovers_original_node_after_process_crash() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_node_kernel_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let mut mount = mount_writable(&f, &proxy);
    let result=std::process::Command::new("python3").arg("-c").arg(r#"
import os,sys,errno
p=sys.argv[1]; d=p+'/nodes';os.mkdir(d)
f=d+'/data'
with open(f,'wb') as stream:stream.write(b'original');stream.flush();os.fsync(stream.fileno())
os.setxattr(f,'user.binary',b'\0\xff',os.XATTR_CREATE)
assert os.getxattr(f,'user.binary')==b'\0\xff'
for call,code in [(lambda:os.setxattr(f,'user.binary',b'bad',os.XATTR_CREATE),errno.EEXIST),(lambda:os.removexattr(f,'user.missing'),errno.ENODATA),(lambda:os.rmdir(d),errno.ENOTEMPTY)]:
 try:call()
 except OSError as error:assert error.errno==code,(error,code)
 else:raise AssertionError('missing error')
os.removexattr(f,'user.binary');os.chmod(f,0o444);assert os.stat(f).st_mode&0o222==0
os.chmod(f,0o644);os.utime(f,ns=(1735776000000000000,1735776000000000000))
assert os.stat(f).st_mtime_ns==1735776000000000000
os.symlink('data',d+'/link');assert os.readlink(d+'/link')=='data'
with open(f,'rb') as stream:
 os.unlink(f);assert stream.read()==b'original'
os.unlink(d+'/link');os.rmdir(d)
"#).arg(&mount.path).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    proxy.mode.store(5, Ordering::SeqCst);
    let path = mount.path.join("crash-node");
    let worker = std::thread::spawn(move || fs::create_dir(path));
    let journal_root = f.root.join("kernel-journals");
    let deadline = Instant::now() + Duration::from_secs(10);
    let directory = loop {
        if let Some(directory) = prepared_node_directory(&journal_root, "/crash-node") {
            break directory;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    let header = fs::read(directory.join("request.json")).unwrap();
    let original = original_record(&directory);
    let posts = proxy.nodes.load(Ordering::SeqCst);
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
    proxy.mode.store(0, Ordering::SeqCst);
    let mut mount = mount_writable(&f, &proxy);
    assert!(mount.path.join("crash-node").is_dir());
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts + 1);
    assert_eq!(fs::read(directory.join("request.json")).unwrap(), header);
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["receipt"]["operation"],
        original["original"]["operation"]
    );
    mount.finish();
    let posts = proxy.nodes.load(Ordering::SeqCst);
    let mut mount = mount_writable(&f, &proxy);
    assert!(mount.path.join("crash-node").is_dir());
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    mount.finish();
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；旧节点版本/父身份条件，不接触真实设备"]
fn writable_core_node_conditions_refuse_changed_versions_and_recreated_parents() {
    use tape_rs::ltfs::index::MetadataChange;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!(
        "fdb_writable_node_conditions_workspace={}",
        f.root.display()
    );
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let direct = FdbClient::new([&server.address]).unwrap();
    let journals = f.root.join("node-condition-journals");
    let backend = FdbBackend::new_writable(
        proxy
            .client()
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(3)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &journals,
    )
    .unwrap();
    let core = TapeFs::new(backend, f.root.join("node-condition-cache")).unwrap();
    let file = core.lookup(ROOT_INO, "file-0").unwrap();
    let read = core.open(file.ino, nix::libc::O_RDONLY).unwrap();
    change_node(
        &direct,
        &f,
        "/file-0",
        Some(1),
        json!({"kind":"change-metadata","readonly":true,"access_time":null,"modify_time":null}),
    );
    let current = direct
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let posts = proxy.nodes.load(Ordering::SeqCst);
    assert!(
        core.change_metadata(
            file.ino,
            &MetadataChange {
                readonly: Some(false),
                access_time: None,
                modify_time: None
            }
        )
        .is_err()
    );
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    assert_eq!(
        direct
            .stat(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
        current
    );
    let saved = upload_directory(&journals, "/file-0");
    let record = original_record(&saved);
    assert_eq!(record["original"]["condition"]["expected_version"], 1);
    let header = fs::read(saved.join("request.json")).unwrap();
    assert!(
        core.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
            .is_err()
    );
    assert_eq!(fs::read(saved.join("request.json")).unwrap(), header);
    core.release(read);
    drop(core);

    let journals = f.root.join("node-parent-journals");
    let backend = FdbBackend::new_writable(
        proxy
            .client()
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(3)),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &journals,
    )
    .unwrap();
    let core = TapeFs::new(backend, f.root.join("node-parent-cache")).unwrap();
    let parent = core.mkdir(ROOT_INO, "parent").unwrap();
    change_node(
        &direct,
        &f,
        "/parent",
        Some(1),
        json!({"kind":"remove-directory"}),
    );
    change_node(
        &direct,
        &f,
        "/parent",
        None,
        json!({"kind":"create-directory"}),
    );
    let posts = proxy.nodes.load(Ordering::SeqCst);
    let count = fs::read_dir(&journals).unwrap().count();
    let replacement = core.lookup(ROOT_INO, "parent").unwrap();
    assert_ne!(replacement.ino, parent.ino);
    assert_eq!(
        core.rename(parent.ino, "child", ROOT_INO, "old-parent", true),
        Err(nix::libc::ESTALE)
    );
    assert_eq!(
        core.rename(ROOT_INO, "file-0", parent.ino, "child", true),
        Err(nix::libc::ESTALE)
    );
    assert_eq!(
        core.mkdir(parent.ino, "child").map(|_| ()),
        Err(nix::libc::ESTALE)
    );
    assert_eq!(
        core.symlink(parent.ino, "link", "target").map(|_| ()),
        Err(nix::libc::ESTALE)
    );
    assert_eq!(core.unlink(parent.ino, "child"), Err(nix::libc::ESTALE));
    assert_eq!(core.rmdir(parent.ino, "child"), Err(nix::libc::ESTALE));
    assert_eq!(
        core.create(parent.ino, "file", true).map(|_| ()),
        Err(nix::libc::ESTALE)
    );
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), posts);
    assert_eq!(fs::read_dir(&journals).unwrap().count(), count);
    assert!(
        direct
            .stat(&f.serial, f.pools[0], "/parent/child")
            .unwrap()
            .is_none()
    );
    drop(core);
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；实际TapeFs/HTTP/SimLibrary上传与丢回执恢复，不接触真实设备"]
fn writable_core_retains_original_journal_across_reply_loss_close_root_rename_and_restart() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_core_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let journal_root = f.root.join("mount-journals");
    let cache_root = f.root.join("mount-cache");
    let backend = FdbBackend::new_writable(
        proxy.client(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &journal_root,
    )
    .unwrap();
    assert!(
        FdbBackend::new_writable(
            proxy.client(),
            f.serial.clone(),
            f.route.route.namespace,
            f.pools[0],
            &journal_root
        )
        .is_err(),
        "the stable root must be exclusively leased"
    );
    let core = TapeFs::new(backend, cache_root.clone()).unwrap();
    let (attr, writer_handle) = core.create(ROOT_INO, "lost-reply", true).unwrap();
    core.write(writer_handle, 0, b"original bytes").unwrap();
    proxy.mode.store(1, Ordering::SeqCst);
    assert_eq!(core.fsync(writer_handle), Err(nix::libc::EBUSY));
    let directory = upload_directory(&journal_root, "/lost-reply");
    let original = original_record(&directory);
    let header = fs::read(directory.join("request.json")).unwrap();
    assert_eq!(
        fs::read(directory.join("payload")).unwrap(),
        b"original bytes"
    );
    assert!(!directory.join("receipt.json").exists());
    assert_eq!(proxy.puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        core.write(writer_handle, 0, b"other contents"),
        Err(nix::libc::EBUSY)
    );
    assert_eq!(core.truncate(attr.ino, 0), Err(nix::libc::EBUSY));
    core.release(writer_handle);
    let reopened = core.open(attr.ino, nix::libc::O_WRONLY).unwrap();
    assert_eq!(core.write(reopened, 0, b"changed"), Err(nix::libc::EBUSY));
    core.release(reopened);

    // A same-name recreation with identical bytes is a different inode. The
    // pending mount must still confirm its original operation, without I/O.
    let direct = FdbClient::new([&server.address]).unwrap();
    let before = direct
        .stat(&f.serial, f.pools[0], "/lost-reply")
        .unwrap()
        .unwrap();
    change_node(
        &direct,
        &f,
        "/lost-reply",
        Some(1),
        json!({"kind":"unlink"}),
    );
    let replacement = f.root.join("replacement-source");
    fs::write(&replacement, b"original bytes").unwrap();
    let replacement_request = f.root.join("replacement-upload");
    direct
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/lost-reply",
            &replacement,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &replacement_request,
        )
        .unwrap();
    let replacement_receipt = direct.upload_saved(&replacement_request).unwrap();
    assert_ne!(replacement_receipt.details()["id"], before["id"]);

    // Move the held root, then put a different, valid request at the old path.
    let held_root = f.root.join("held-journals");
    fs::rename(&journal_root, &held_root).unwrap();
    fs::create_dir(&journal_root).unwrap();
    let source = f.root.join("foreign-source");
    fs::write(&source, b"foreign").unwrap();
    let foreign = journal_root.join(Uuid::new_v4().to_string());
    let foreign_handle = UploadJournal::create(
        &f.serial,
        f.pools[0],
        "/foreign",
        &source,
        UploadCondition::Create {
            namespace: f.route.route.namespace,
            parent: f.pool_roots[0],
        },
        None,
        &foreign,
    )
    .unwrap();
    drop(foreign_handle);
    let foreign_header = fs::read(foreign.join("request.json")).unwrap();
    proxy.mode.store(0, Ordering::SeqCst);
    let commands = owner.trace.lock().unwrap().len();
    core.change_xattr(ROOT_INO, "user.ltfs.sync", Some(b"1"), 0)
        .unwrap();
    assert!(
        owner.trace.lock().unwrap()[commands..].iter().all(|cdb| {
            !matches!(
                cdb[0],
                0x01 | 0x04 | 0x08 | 0x0a | 0x10 | 0x11 | 0x19 | 0x1b | 0x2b | 0x8d | 0xa5
            )
        }),
        "root sync may coexist with background fencing/health reads, but must not replay media I/O"
    );
    assert_eq!(
        proxy.puts.load(Ordering::SeqCst),
        1,
        "root fsync must query the original operation without a second append"
    );
    let directory = upload_directory(&held_root, "/lost-reply");
    assert_eq!(fs::read(directory.join("request.json")).unwrap(), header);
    let ack: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(
        ack["receipt"]["operation"],
        original["original"]["operation"]
    );
    assert!(!directory.join("payload").exists());
    assert_ne!(core.lookup(ROOT_INO, "lost-reply").unwrap().ino, attr.ino);
    let (_, handle) = core.create(ROOT_INO, "new-after-move", true).unwrap();
    core.write(handle, 0, b"new bytes").unwrap();
    core.fsync(handle).unwrap();
    core.write(handle, 0, b"overwritten").unwrap();
    core.fsync(handle).unwrap();
    assert_eq!(
        direct
            .stat(&f.serial, f.pools[0], "/new-after-move")
            .unwrap()
            .unwrap()["version"],
        2
    );
    core.release(handle);
    assert!(
        upload_directory(&held_root, "/new-after-move")
            .join("receipt.json")
            .is_file()
    );
    assert_eq!(
        fs::read(foreign.join("request.json")).unwrap(),
        foreign_header
    );
    assert_eq!(fs::read(foreign.join("payload")).unwrap(), b"foreign");
    assert_eq!(fs::read_dir(&journal_root).unwrap().count(), 1);

    // An unadmitted original survives loss of the cache and mount instance.
    let (_, handle) = core.create(ROOT_INO, "restart-original", true).unwrap();
    core.write(handle, 0, b"restart bytes").unwrap();
    proxy.mode.store(2, Ordering::SeqCst);
    assert_eq!(core.fsync(handle), Err(nix::libc::EIO));
    let restart = upload_directory(&held_root, "/restart-original");
    let restart_header = fs::read(restart.join("request.json")).unwrap();
    core.release(handle);
    drop(core);
    fs::remove_dir_all(&cache_root).unwrap();
    proxy.mode.store(0, Ordering::SeqCst);
    let puts = proxy.puts.load(Ordering::SeqCst);
    let backend = FdbBackend::new_writable(
        proxy.client(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &held_root,
    )
    .unwrap();
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts + 1);
    assert_eq!(
        fs::read(restart.join("request.json")).unwrap(),
        restart_header
    );
    assert!(!restart.join("payload").exists());
    let core = TapeFs::new(backend, cache_root).unwrap();
    let file = core.lookup(ROOT_INO, "restart-original").unwrap();
    let handle = core.open(file.ino, nix::libc::O_RDONLY).unwrap();
    assert_eq!(core.read(handle, 0, 100).unwrap(), b"restart bytes");
    core.release(handle);
    drop(core);
    let puts = proxy.puts.load(Ordering::SeqCst);
    drop(
        FdbBackend::new_writable(
            proxy.client(),
            f.serial.clone(),
            f.route.route.namespace,
            f.pools[0],
            &held_root,
        )
        .unwrap(),
    );
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts);

    // A valid foreign namespace cannot be replayed by this mount.
    let invalid = held_root.join(Uuid::new_v4().to_string());
    drop(
        UploadJournal::create(
            &f.serial,
            f.pools[0],
            "/wrong-scope",
            &source,
            UploadCondition::Create {
                namespace: Uuid::new_v4(),
                parent: f.pool_roots[0],
            },
            None,
            &invalid,
        )
        .unwrap(),
    );
    assert!(
        FdbBackend::new_writable(
            proxy.client(),
            f.serial.clone(),
            f.route.route.namespace,
            f.pools[0],
            &held_root
        )
        .is_err()
    );
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts);
    assert_eq!(fs::read(invalid.join("payload")).unwrap(), b"foreign");
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB；TapeFs原版本/父目录条件与模拟磁带，不接触真实设备"]
fn writable_core_rejects_changed_original_file_and_recreated_parent_without_put() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_conditions_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let client = proxy.client();
    let management = FdbClient::new([&proxy.address]).unwrap();
    let backend = FdbBackend::new_writable(
        proxy.client(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
        &f.root.join("conditions-journals"),
    )
    .unwrap();
    let core = TapeFs::new(backend, f.root.join("conditions-cache")).unwrap();
    let old = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let attr = core.lookup(ROOT_INO, "file-0").unwrap();
    let handle = core
        .open(attr.ino, nix::libc::O_WRONLY | nix::libc::O_TRUNC)
        .unwrap();
    core.write(handle, 0, b"local body").unwrap();
    let remote_source = f.root.join("remote-source");
    fs::write(&remote_source, b"remote replacement").unwrap();
    let saved = f.root.join("remote-upload");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/file-0",
            &remote_source,
            UploadCondition::Replace(tape_rs::client::FileVersion::Fdb {
                namespace: f.route.route.namespace,
                inode: Uuid::parse_str(old["id"].as_str().unwrap()).unwrap(),
                version: old["version"].as_u64().unwrap(),
            }),
            None,
            &saved,
        )
        .unwrap();
    client.upload_saved(&saved).unwrap();
    let puts = proxy.puts.load(Ordering::SeqCst);
    assert!(core.fsync(handle).is_err());
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts);
    assert_eq!(core.read(handle, 0, 100).unwrap(), b"local body");
    let record = upload_directory(&f.root.join("conditions-journals"), "/file-0");
    assert_eq!(fs::read(record.join("payload")).unwrap(), b"local body");
    assert!(!record.join("receipt.json").exists());
    core.release(handle);

    change_node(
        &management,
        &f,
        "/parent",
        None,
        json!({"kind":"create-directory"}),
    );
    let parent = core.lookup(ROOT_INO, "parent").unwrap();
    let (_, handle) = core.create(parent.ino, "child", true).unwrap();
    core.write(handle, 0, b"original parent bytes").unwrap();
    change_node(
        &management,
        &f,
        "/parent",
        Some(1),
        json!({"kind":"remove-directory"}),
    );
    change_node(
        &management,
        &f,
        "/parent",
        None,
        json!({"kind":"create-directory"}),
    );
    assert!(core.fsync(handle).is_err());
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts);
    let record = upload_directory(&f.root.join("conditions-journals"), "/parent/child");
    assert_eq!(
        fs::read(record.join("payload")).unwrap(),
        b"original parent bytes"
    );
    assert!(!record.join("receipt.json").exists());
    assert!(
        client
            .stat(&f.serial, f.pools[0], "/parent/child")
            .unwrap()
            .is_none()
    );
    core.release(handle);
    drop(core);
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

pub(super) fn mount_writable(f: &Fixture, proxy: &UploadProxy) -> super::fuse_readonly::Mount {
    let binary =
        std::env::var("TAPE_RS_FDB_FUSE_BIN").expect("explicit default FUSE binary required");
    let mountpoint = f.root.join("write-mount");
    if !mountpoint.exists() {
        fs::create_dir(&mountpoint).unwrap();
    }
    let log_path = f.root.join(format!("write-fuse-{}.log", Uuid::new_v4()));
    let log = fs::File::create(&log_path).unwrap();
    let child = std::process::Command::new(binary)
        .args(["-e", &proxy.address, "--library", &f.serial, "--pool"])
        .arg(f.pools[0].to_string())
        .arg("--fdb-namespace")
        .arg(f.route.route.namespace.to_string())
        .arg("--fdb-journal-dir")
        .arg(f.root.join("kernel-journals"))
        .arg("--cache-dir")
        .arg(f.root.join("kernel-cache"))
        .arg(&mountpoint)
        .stdout(std::process::Stdio::from(log.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .unwrap();
    let mut mount = super::fuse_readonly::Mount {
        child,
        path: mountpoint,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !mount.mounted() {
        assert!(
            mount.child.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&log_path).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "{}",
            fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    mount
}

#[test]
#[ignore = "需要隔离FDB、TAPE_RS_FDB_FUSE_BIN和本机FUSE；模拟磁带，不接触真实设备"]
fn default_fuse_writes_creates_overwrites_truncates_and_reopens_durable_versions() {
    use std::io::{Seek, SeekFrom};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_kernel_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let mut mount = mount_writable(&f, &proxy);
    let path = mount.path.join("written");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(b"first-body").unwrap();
    file.sync_all().unwrap();
    file.seek(SeekFrom::Start(5)).unwrap();
    file.write_all(b"changed").unwrap();
    file.set_len(4).unwrap();
    file.sync_all().unwrap();
    assert_eq!(
        proxy
            .client()
            .stat(&f.serial, f.pools[0], "/written")
            .unwrap()
            .unwrap()["version"],
        2
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"firs");
    file.set_len(0).unwrap();
    file.sync_all().unwrap();
    let committed = proxy
        .client()
        .stat(&f.serial, f.pools[0], "/written")
        .unwrap()
        .unwrap();
    assert_eq!(committed["version"], 3);
    assert_eq!(committed["size"], 0);
    drop(file);
    mount.finish();
    let puts = proxy.puts.load(Ordering::SeqCst);
    let mut remounted = mount_writable(&f, &proxy);
    assert_eq!(proxy.puts.load(Ordering::SeqCst), puts);
    assert_eq!(fs::read(remounted.path.join("written")).unwrap(), b"");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(remounted.path.join("written"))
        .unwrap();
    file.write_all(b"after remount").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let latest = proxy
        .client()
        .stat(&f.serial, f.pools[0], "/written")
        .unwrap()
        .unwrap();
    assert_eq!(latest["version"], 4);
    assert_eq!(latest["id"], committed["id"]);
    remounted.finish();
    drop(remounted);
    drop(mount);
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
#[ignore = "需要隔离FDB、TAPE_RS_FDB_FUSE_BIN和本机FUSE；只SIGKILL测试FUSE子进程，模拟磁带"]
fn default_fuse_recovers_the_same_committed_operation_after_process_crash_and_lost_reply() {
    use std::os::unix::process::ExitStatusExt;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_writable_crash_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let proxy = UploadProxy::new(server.address.clone());
    let mut mount = mount_writable(&f, &proxy);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(mount.path.join("crash-original"))
        .unwrap();
    file.write_all(b"durable original bytes").unwrap();
    proxy.mode.store(3, Ordering::SeqCst);
    assert_eq!(
        file.sync_all().unwrap_err().raw_os_error(),
        Some(nix::libc::EIO)
    );
    assert_eq!(
        file.write_all(b"changed").unwrap_err().raw_os_error(),
        Some(nix::libc::EBUSY)
    );
    let directory = upload_directory(&f.root.join("kernel-journals"), "/crash-original");
    let header = fs::read(directory.join("request.json")).unwrap();
    let original = original_record(&directory);
    assert_eq!(
        fs::read(directory.join("payload")).unwrap(),
        b"durable original bytes"
    );
    assert_eq!(proxy.puts.load(Ordering::SeqCst), 1);
    mount.child.kill().unwrap();
    assert_eq!(mount.child.wait().unwrap().signal(), Some(9));
    drop(file);
    assert!(
        std::process::Command::new("fusermount3")
            .args(["-u", "--"])
            .arg(&mount.path)
            .status()
            .unwrap()
            .success()
    );
    fs::remove_dir_all(f.root.join("kernel-cache")).unwrap();
    proxy.mode.store(0, Ordering::SeqCst);
    let mut resumed = mount_writable(&f, &proxy);
    assert_eq!(
        proxy.puts.load(Ordering::SeqCst),
        1,
        "restart must not append an already committed original again"
    );
    assert_eq!(
        fs::read(resumed.path.join("crash-original")).unwrap(),
        b"durable original bytes"
    );
    assert_eq!(fs::read(directory.join("request.json")).unwrap(), header);
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["receipt"]["operation"],
        original["original"]["operation"]
    );
    assert!(!directory.join("payload").exists());
    resumed.finish();
    drop(resumed);
    drop(mount);
    drop(proxy);
    drop(server);
    drop(owner);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn journal_discovery_ignores_unpublished_drafts_and_reads_only_complete_original_headers() {
    let root =
        std::env::temp_dir().join(format!("tape-rs-fdb-journal-discovery-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    eprintln!("fdb_journal_discovery_workspace={}", root.display());
    for prefix in [".fdb-node-", ".fdb-upload-"] {
        let draft = root.join(format!("{prefix}{}", Uuid::new_v4()));
        fs::create_dir(&draft).unwrap();
        // The producer may still be writing this header, or may atomically
        // rename this entire directory immediately after a polling stat.
        fs::write(draft.join("request.json"), b"incomplete").unwrap();
    }
    assert!(prepared_node_directory(&root, "/pending-node").is_none());
    let published = root.join(Uuid::new_v4().to_string());
    fs::create_dir(&published).unwrap();
    fs::write(
        published.join("request.json"),
        br#"{"original":{"path":"/published"},"prepared":null}"#,
    )
    .unwrap();
    assert_eq!(upload_directory(&root, "/published"), published);
    let node = root.join(format!("node-{}", Uuid::new_v4()));
    fs::create_dir(&node).unwrap();
    fs::write(
        node.join("request.json"),
        br#"{"original":{"path":"/pending-node"},"prepared":null}"#,
    )
    .unwrap();
    assert!(prepared_node_directory(&root, "/pending-node").is_none());
    fs::write(
        node.join("request.json"),
        br#"{"original":{"path":"/pending-node"},"prepared":{"original":"fixed"}}"#,
    )
    .unwrap();
    assert_eq!(prepared_node_directory(&root, "/pending-node"), Some(node));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn upload_proxy_keeps_serving_after_a_client_exits_before_request_completion() {
    let proxy = UploadProxy::new("127.0.0.1:9".into());
    proxy.mode.store(5, Ordering::SeqCst);
    for partial in [
        b"".as_slice(),
        b"POST /v1/fdb/nodes/test/requests HTTP/1".as_slice(),
        b"POST /v1/fdb/nodes/test/requests HTTP/1.1\r\nContent-Length: ".as_slice(),
        b"POST /v1/fdb/nodes/test/requests HTTP/1.1\r\nContent-Len".as_slice(),
        b"POST /v1/fdb/nodes/test/requests HTTP/1.1\r\nContent-Length: 3\r\n\r\nx".as_slice(),
    ] {
        let mut connection = TcpStream::connect(&proxy.address).unwrap();
        connection.write_all(partial).unwrap();
        connection.shutdown(std::net::Shutdown::Write).unwrap();
    }
    let mut connection = TcpStream::connect(&proxy.address).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    connection.write_all(b"POST /v1/fdb/nodes/test/requests HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut answer = Vec::new();
    let result = connection.read_to_end(&mut answer);
    assert!(
        result.is_ok() && answer.starts_with(b"HTTP/1.1 503"),
        "proxy stopped after an aborted request: {result:?} {answer:?}"
    );
    assert_eq!(proxy.nodes.load(Ordering::SeqCst), 0);
    assert_eq!(proxy.puts.load(Ordering::SeqCst), 0);
}
