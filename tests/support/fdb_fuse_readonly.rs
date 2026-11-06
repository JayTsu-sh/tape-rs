//! Isolated FDB, SimLibrary and an explicitly supplied local FUSE binary only.
use super::*;
use tape_rs::client::fdb::FdbClient;
use tape_rs::tapefs::{Backend, FdbBackend, ROOT_INO, TapeFs};

fn change(
    client: &FdbClient,
    f: &Fixture,
    path: &str,
    version: Option<u64>,
    mutation: serde_json::Value,
) {
    let saved = f.root.join(format!("fuse-node-{}.json", Uuid::new_v4()));
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

pub(super) struct Mount {
    pub(super) child: std::process::Child,
    pub(super) path: std::path::PathBuf,
}
impl Mount {
    pub(super) fn mounted(&self) -> bool {
        fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .lines()
            .any(|line| line.split_whitespace().nth(4) == self.path.to_str())
    }
    pub(super) fn finish(&mut self) {
        if self.mounted() {
            assert!(
                std::process::Command::new("fusermount3")
                    .args(["-u", "--"])
                    .arg(&self.path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "FUSE child did not stop after its mount was removed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Mount {
    fn drop(&mut self) {
        if self.mounted() {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z", "--"])
                .arg(&self.path)
                .status();
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER、TAPE_RS_FDB_FUSE_BIN 与本机FUSE；仅SimLibrary，不接受真实设备"]
fn default_fuse_mount_reads_original_fdb_nodes_and_rejects_writes() {
    let binary =
        std::env::var("TAPE_RS_FDB_FUSE_BIN").expect("explicit default FUSE binary required");
    assert!(std::path::Path::new(&binary).is_file());
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_fuse_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let original = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    change(
        &client,
        &f,
        "/目录",
        None,
        json!({"kind":"create-directory"}),
    );
    change(
        &client,
        &f,
        "/目录",
        Some(1),
        json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"原始属性","base64":false},"mode":"create"}),
    );
    change(
        &client,
        &f,
        "/链接",
        None,
        json!({"kind":"create-symlink","target":"file-0"}),
    );
    change(
        &client,
        &f,
        "/file-0",
        original["version"].as_u64(),
        json!({"kind":"set-xattr","attribute":{"key":"user.binary","value":"AP8=","base64":true},"mode":"create"}),
    );
    change(
        &client,
        &f,
        "/file-0",
        Some(original["version"].as_u64().unwrap() + 1),
        json!({"kind":"change-metadata","readonly":true,"access_time":"2026-11-01T12:34:55.000000000Z","modify_time":"2026-11-01T12:34:56.000000000Z"}),
    );
    let current = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    assert_eq!(current["generation"], original["generation"]);
    assert_eq!(
        current["version"],
        original["version"].as_u64().unwrap() + 2
    );
    assert_ne!(current["generation"], current["version"]);
    let retained = client
        .node_attributes(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let readonly = reader(&f);
    drop(server);
    owner.permission.close(12);
    let trace = owner.trace.clone();
    drop(owner);
    let commands = trace.lock().unwrap().len();
    let backend = FdbBackend::new(
        FdbClient::new([&readonly.address]).unwrap(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
    )
    .unwrap();
    let core = TapeFs::new(backend, f.root.join("metadata-cache")).unwrap();
    let directory = core.lookup(ROOT_INO, "目录").unwrap();
    assert!(directory.is_dir);
    assert_eq!(
        core.getxattr(directory.ino, "user.note").unwrap(),
        "原始属性".as_bytes()
    );
    assert_eq!(
        core.getxattr(directory.ino, "user.tape.generation"),
        Err(nix::libc::ENODATA)
    );
    let link = core.lookup(ROOT_INO, "链接").unwrap();
    assert!(link.is_symlink);
    assert_eq!(core.readlink(link.ino).unwrap(), b"file-0");
    let file = core.lookup(ROOT_INO, "file-0").unwrap();
    assert!(file.readonly);
    assert_eq!(core.getxattr(file.ino, "user.binary").unwrap(), [0, 255]);
    let expected =
        chrono::DateTime::parse_from_rfc3339(retained["metadata"]["modify_time"].as_str().unwrap())
            .unwrap();
    assert_eq!(file.mtime, std::time::SystemTime::from(expected));
    assert!(core.backend().stat("/missing").unwrap().is_none());
    assert!(
        FdbBackend::new(
            FdbClient::new([&readonly.address]).unwrap(),
            f.serial.clone(),
            Uuid::new_v4(),
            f.pools[0]
        )
        .is_err()
    );
    assert_eq!(
        trace.lock().unwrap().len(),
        commands,
        "metadata reads must issue no device commands without an owner"
    );
    drop(core);
    drop(readonly);
    let owner = Owner::start(&f, 13, json!({}));
    let server = writer(&f, &owner, 13);
    let backend = FdbBackend::new(
        FdbClient::new([&server.address]).unwrap(),
        f.serial.clone(),
        f.route.route.namespace,
        f.pools[0],
    )
    .unwrap();
    assert_eq!(backend.mkdir("/forbidden"), Err(nix::libc::EROFS));
    assert_eq!(backend.delete("/file-0"), Err(nix::libc::EROFS));
    let core = TapeFs::new(backend, f.root.join("body-cache")).unwrap();
    let file = core.lookup(ROOT_INO, "file-0").unwrap();
    let handle = core.open(file.ino, nix::libc::O_RDONLY).unwrap();
    assert_eq!(core.read(handle, 0, 100).unwrap(), b"content-0");
    core.release(handle);
    drop(core);
    let mountpoint = f.root.join("mount");
    fs::create_dir(&mountpoint).unwrap();
    let log = fs::File::create(f.root.join("fuse-child.log")).unwrap();
    let child = std::process::Command::new(&binary)
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "--fdb-namespace",
            &f.route.route.namespace.to_string(),
            "--cache-dir",
        ])
        .arg(f.root.join("mounted-cache"))
        .arg(&mountpoint)
        .stdout(std::process::Stdio::from(log.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .unwrap();
    let mut mounted = Mount {
        child,
        path: mountpoint,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !mounted.mounted() {
        assert!(
            mounted.child.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(f.root.join("fuse-child.log")).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "{}",
            fs::read_to_string(f.root.join("fuse-child.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let output=std::process::Command::new("python3").args(["-c",r#"
import os,sys,mmap,errno,stat,datetime
p=sys.argv[1]
assert os.statvfs(p).f_flag & os.ST_RDONLY
assert {'file-0','目录','链接'}.issubset(set(os.listdir(p)))
assert open(p+'/file-0','rb').read()==b'content-0'
assert os.readlink(p+'/链接')=='file-0'
assert next(x for x in os.scandir(p) if x.name=='链接').is_symlink()
assert open(p+'/链接','rb').read()==b'content-0'
assert os.getxattr(p+'/file-0','user.binary')==b'\x00\xff'
assert os.getxattr(p+'/目录','user.note')=='原始属性'.encode()
assert not os.stat(p+'/file-0').st_mode & 0o222
expected=int(datetime.datetime(2026,11,1,12,34,56,tzinfo=datetime.timezone.utc).timestamp())*10**9
assert os.stat(p+'/file-0').st_mtime_ns==expected
with open(p+'/file-0','rb') as f:
    with mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ) as m: assert m[:]==b'content-0'
for change in [lambda:os.open(p+'/new',os.O_WRONLY|os.O_CREAT,0o600),lambda:os.open(p+'/file-0',os.O_WRONLY|os.O_TRUNC),lambda:os.mkdir(p+'/newdir'),lambda:os.unlink(p+'/file-0'),lambda:os.rename(p+'/file-0',p+'/renamed'),lambda:os.symlink('/file-0',p+'/newlink'),lambda:os.setxattr(p+'/file-0','user.forbidden',b'no')]:
    try: change()
    except OSError as ex: assert ex.errno==errno.EROFS,ex
    else: raise AssertionError('readonly mount accepted mutation')
assert open(p+'/file-0','rb').read()==b'content-0'
assert not os.path.exists(p+'/new') and not os.path.exists(p+'/renamed')
"#]).arg(&mounted.path).output().unwrap();
    mounted.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(mounted);
    let client = FdbClient::new([&server.address]).unwrap();
    assert_eq!(
        client
            .node_attributes(&f.serial, f.pools[0], "/file-0")
            .unwrap()
            .unwrap(),
        retained
    );
    owner.expect(0);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

struct Probe(std::process::Child);
impl Drop for Probe {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn probe_stage(
    reader: &mut std::io::BufReader<std::process::ChildStdout>,
    expected: &str,
    log: &std::path::Path,
) {
    let mut line = String::new();
    assert!(
        std::io::BufRead::read_line(reader, &mut line).unwrap() > 0,
        "{}",
        fs::read_to_string(log).unwrap()
    );
    assert_eq!(
        line.trim(),
        expected,
        "{}",
        fs::read_to_string(log).unwrap()
    );
}

#[test]
#[ignore = "需要隔离FDB、TAPE_RS_FDB_FUSE_BIN及本机FUSE；外部变更只在SimLibrary，挂载始终只读"]
fn default_fuse_keeps_inode_snapshots_during_remote_overwrite_recreation_and_type_changes() {
    let binary = std::env::var("TAPE_RS_FDB_FUSE_BIN").unwrap();
    assert!(std::path::Path::new(&binary).is_file());
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("fdb_fuse_inode_workspace={}", f.root.display());
    // Only the external test client modifies simulated media. The mounted client stays RO.
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    change(
        &client,
        &f,
        "/file-0",
        Some(1),
        json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"old-data","base64":false},"mode":"create"}),
    );
    let initial = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let initial_version = format!(
        "fdb:{}/{}@{}",
        f.route.route.namespace,
        initial["id"].as_str().unwrap(),
        initial["version"].as_u64().unwrap()
    );
    let mountpoint = f.root.join("inode-mount");
    fs::create_dir(&mountpoint).unwrap();
    let log = fs::File::create(f.root.join("inode-fuse-child.log")).unwrap();
    let child = std::process::Command::new(&binary)
        .args([
            "-e",
            &server.address,
            "--library",
            &f.serial,
            "--pool",
            &f.pools[0].to_string(),
            "--fdb-namespace",
            &f.route.route.namespace.to_string(),
            "--cache-dir",
        ])
        .arg(f.root.join("inode-cache"))
        .arg(&mountpoint)
        .stdout(std::process::Stdio::from(log.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .unwrap();
    let mut mounted = Mount {
        child,
        path: mountpoint,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while !mounted.mounted() {
        assert!(
            mounted.child.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(f.root.join("inode-fuse-child.log")).unwrap()
        );
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let probe_log = f.root.join("inode-probe.log");
    let errors = fs::File::create(&probe_log).unwrap();
    let child = std::process::Command::new("python3")
        .args([
            "-u",
            "-c",
            r#"
import os,sys,mmap,errno,stat,signal,json
signal.alarm(30)
p=sys.argv[1]+'/file-0'
a=open(p,'rb'); ai=os.fstat(a.fileno()).st_ino
av=os.getxattr(a.fileno(),'user.tape.version')
assert av==sys.argv[2].encode()
assert os.getxattr(a.fileno(),'user.note')==b'old-data'
old=mmap.mmap(a.fileno(),0,access=mmap.ACCESS_READ)
assert old[:]==b'content-0'
print('ready',flush=True)
assert json.loads(sys.stdin.readline())['stage']=='overwritten'
assert os.pread(a.fileno(),100,0)==b'content-0' and old[:]==b'content-0'
assert os.getxattr(a.fileno(),'user.tape.version')==av
assert os.getxattr(a.fileno(),'user.note')==b'old-data'
try: open(p,'rb')
except OSError as ex: assert ex.errno==errno.ESTALE,ex
else: raise AssertionError('new revision entered the pinned inode cache')
print('overwritten-ok',flush=True)
next=json.loads(sys.stdin.readline());assert next['stage']=='recreated'
b=open(p,'rb'); bi=os.fstat(b.fileno()).st_ino
assert bi!=ai
assert b.read()==b'new-node-body'
bv=os.getxattr(b.fileno(),'user.tape.version')
assert bv==next['version'].encode() and bv!=av
assert os.getxattr(b.fileno(),'user.note')==b'new-node'
assert os.pread(a.fileno(),100,0)==b'content-0' and old[:]==b'content-0'
assert os.getxattr(a.fileno(),'user.tape.version')==av
assert os.getxattr(a.fileno(),'user.note')==b'old-data'
print('recreated-ok',flush=True)
assert json.loads(sys.stdin.readline())['stage']=='directory'
assert os.path.isdir(p) and os.stat(p).st_ino not in (ai,bi)
assert os.listdir(p)==[]
assert stat.S_ISREG(os.fstat(a.fileno()).st_mode) and stat.S_ISREG(os.fstat(b.fileno()).st_mode)
assert os.pread(a.fileno(),100,0)==b'content-0'
assert os.pread(b.fileno(),100,0)==b'new-node-body'
assert os.getxattr(a.fileno(),'user.tape.version')==av
assert os.getxattr(b.fileno(),'user.tape.version')==bv
assert os.getxattr(b.fileno(),'user.note')==b'new-node'
directory=os.open(p,os.O_RDONLY|os.O_DIRECTORY)
di=os.fstat(directory).st_ino
print('directory-ok',flush=True)
assert json.loads(sys.stdin.readline())['stage']=='directory-recreated'
try: os.stat('child',dir_fd=directory,follow_symlinks=False)
except OSError as ex: assert ex.errno==errno.ESTALE,ex
else: raise AssertionError('old directory fd looked up a child in the recreated directory')
assert os.stat(p).st_ino!=di
assert os.readlink(p+'/child')=='../file-1'
assert open(p+'/child','rb').read()==b'content-1'
os.close(directory)
print('directory-recreated-ok',flush=True)
assert json.loads(sys.stdin.readline())['stage']=='symlink'
assert os.path.islink(p) and os.readlink(p)=='file-1'
assert open(p,'rb').read()==b'content-1'
assert os.pread(a.fileno(),100,0)==b'content-0' and old[:]==b'content-0'
assert os.pread(b.fileno(),100,0)==b'new-node-body'
assert os.getxattr(a.fileno(),'user.tape.version')==av
assert os.getxattr(b.fileno(),'user.tape.version')==bv
old.close();a.close();b.close()
print('done',flush=True)
"#,
        ])
        .arg(&mounted.path)
        .arg(&initial_version)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(errors))
        .spawn()
        .unwrap();
    let mut probe = Probe(child);
    let mut input = probe.0.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(probe.0.stdout.take().unwrap());
    probe_stage(&mut output, "ready", &probe_log);
    let body = f.root.join("external-overwrite");
    fs::write(&body, b"changed body while old reader is open").unwrap();
    let saved = f.root.join("external-overwrite.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/file-0",
            &body,
            upload_replacement(
                &client
                    .stat(&f.serial, f.pools[0], "/file-0")
                    .unwrap()
                    .unwrap(),
            ),
            None,
            &saved,
        )
        .unwrap();
    assert_upload_committed(client.upload_saved(&saved).unwrap(), &saved, 3);
    change(
        &client,
        &f,
        "/file-0",
        Some(3),
        json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"changed-data","base64":false},"mode":"replace"}),
    );
    writeln!(input, "{}", json!({"stage":"overwritten"})).unwrap();
    probe_stage(&mut output, "overwritten-ok", &probe_log);
    change(&client, &f, "/file-0", Some(4), json!({"kind":"unlink"}));
    let body = f.root.join("external-recreate");
    fs::write(&body, b"new-node-body").unwrap();
    let saved = f.root.join("external-recreate.json");
    client
        .prepare_file(
            &f.serial,
            f.pools[0],
            "/file-0",
            &body,
            UploadCondition::Create {
                namespace: f.route.route.namespace,
                parent: f.pool_roots[0],
            },
            None,
            &saved,
        )
        .unwrap();
    assert_upload_committed(client.upload_saved(&saved).unwrap(), &saved, 1);
    change(
        &client,
        &f,
        "/file-0",
        Some(1),
        json!({"kind":"set-xattr","attribute":{"key":"user.note","value":"new-node","base64":false},"mode":"create"}),
    );
    let current = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    assert_ne!(current["id"], initial["id"]);
    let version = format!(
        "fdb:{}/{}@{}",
        f.route.route.namespace,
        current["id"].as_str().unwrap(),
        current["version"].as_u64().unwrap()
    );
    writeln!(input, "{}", json!({"stage":"recreated","version":version})).unwrap();
    probe_stage(&mut output, "recreated-ok", &probe_log);
    change(&client, &f, "/file-0", Some(2), json!({"kind":"unlink"}));
    change(
        &client,
        &f,
        "/file-0",
        None,
        json!({"kind":"create-directory"}),
    );
    writeln!(input, "{}", json!({"stage":"directory"})).unwrap();
    probe_stage(&mut output, "directory-ok", &probe_log);
    change(
        &client,
        &f,
        "/file-0",
        Some(1),
        json!({"kind":"remove-directory"}),
    );
    change(
        &client,
        &f,
        "/file-0",
        None,
        json!({"kind":"create-directory"}),
    );
    change(
        &client,
        &f,
        "/file-0/child",
        None,
        json!({"kind":"create-symlink","target":"../file-1"}),
    );
    writeln!(input, "{}", json!({"stage":"directory-recreated"})).unwrap();
    probe_stage(&mut output, "directory-recreated-ok", &probe_log);
    change(
        &client,
        &f,
        "/file-0/child",
        Some(1),
        json!({"kind":"unlink"}),
    );
    let directory_version = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap()["version"]
        .as_u64()
        .unwrap();
    change(
        &client,
        &f,
        "/file-0",
        Some(directory_version),
        json!({"kind":"remove-directory"}),
    );
    change(
        &client,
        &f,
        "/file-0",
        None,
        json!({"kind":"create-symlink","target":"file-1"}),
    );
    writeln!(input, "{}", json!({"stage":"symlink"})).unwrap();
    probe_stage(&mut output, "done", &probe_log);
    assert!(
        probe.0.wait().unwrap().success(),
        "{}",
        fs::read_to_string(&probe_log).unwrap()
    );
    drop(input);
    drop(output);
    drop(probe);
    mounted.finish();
    drop(mounted);
    drop(server);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
