//! 一个真实 Raft 循环/节点，两个模拟库。固定 HTTP 入口覆盖同节点持有和分散持有。
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use raft::eraftpb::{Message, MessageType};
use serde_json::json;
use tape_rs::daemon::{
    executor::{self, DeviceKind, DeviceProvider, ExecOptions, ManagedDevice},
    files::{BatchPolicy, FileService},
    gateway,
    http::HttpContext,
    multilib::{self, GATEWAY, LibrariesConfig, LibraryRuntime},
    net::{AdminReply, Network, NodeInput},
    node::{CoordinationConfig, NodeConfig, NodeStatus, SharedStatus},
    state::Command,
    store::{RaftStore, SnapshotPolicy},
};
use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::sim::{SimCartridge, SimLibrary};

struct Net(Arc<HashMap<u64, mpsc::Sender<NodeInput>>>);
impl Network for Net {
    fn send(&self, message: Message) {
        if let Some(tx) = self.0.get(&message.to) {
            let _ = tx.send(NodeInput::Raft(Box::new(message)));
        }
    }
}
struct Provider {
    lib: SimLibrary,
    id: u64,
    enabled: Arc<AtomicBool>,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(TapeError::NotReady("测试断开本库 FC".into()));
        }
        Ok(vec![
            ManagedDevice {
                name: "drive".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: Box::new(self.lib.drive_as(0, self.id as u32)),
            },
            ManagedDevice {
                name: "changer".into(),
                serial: "SIMLIB0000000001".into(),
                kind: DeviceKind::Changer,
                dev: Box::new(self.lib.changer_as(self.id as u32)),
            },
        ])
    }
}
fn wait(mut check: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !check() {
        assert!(Instant::now() < until, "多库状态未在期限内收敛");
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn exchange(address: &str, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: cluster\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let at = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8_lossy(&response[..at]);
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    let bytes = response[at + 4..].to_vec();
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut output = Vec::new();
        let mut rest = bytes.as_slice();
        loop {
            let end = rest.windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(
                std::str::from_utf8(&rest[..end])
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap(),
                16,
            )
            .unwrap();
            if n == 0 {
                break;
            }
            output.extend_from_slice(&rest[end + 2..end + 2 + n]);
            rest = &rest[end + 4 + n..];
        }
        (status, output)
    } else {
        (status, bytes)
    }
}

#[test]
fn one_cluster_two_owners_and_fixed_http_entry_preserve_library_isolation() {
    let root = std::env::temp_dir().join(format!("tape-multilib-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let config=LibrariesConfig::parse(&json!({"default_library":"55L3A7802K19LL01","libraries":[{"changer_serial":"55L3A7802K19LL01","drive_serials":["drive-a"]},{"changer_serial":"55L3A78011W9LL01","drive_serials":["drive-b"]}]})).unwrap();
    let mut senders = HashMap::new();
    let mut receivers = HashMap::new();
    for id in 1..=3 {
        let (tx, rx) = mpsc::channel();
        senders.insert(id, tx);
        receivers.insert(id, rx);
    }
    let peers = Arc::new(senders);
    let mut addresses = HashMap::new();
    for id in 1..=3 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        addresses.insert(id, listener.local_addr().unwrap().to_string());
    }
    let libs: BTreeMap<_, _> = ["55L3A7802K19LL01", "55L3A78011W9LL01"]
        .into_iter()
        .map(|id| {
            let lib = SimLibrary::new(1, 2, 0);
            lib.enable_changer_pr();
            lib.insert_cartridge(SimCartridge::blank("SAMEL8", 256 << 20), 0)
                .unwrap();
            (id, lib)
        })
        .collect();
    let s3_listeners: Vec<_> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let s3_addresses: HashMap<_, _> = s3_listeners
        .iter()
        .enumerate()
        .map(|(n, l)| (n as u64 + 1, l.local_addr().unwrap().to_string()))
        .collect();
    drop(s3_listeners);
    let mut statuses = HashMap::<(u64, String), SharedStatus>::new();
    let mut flags = HashMap::new();
    let mut joins = Vec::new();
    for id in 1..=3 {
        let dir = root.join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let mut runtimes = Vec::new();
        let mut contexts = BTreeMap::new();
        for name in ["55L3A7802K19LL01", "55L3A78011W9LL01", GATEWAY] {
            let status = Arc::new(Mutex::new(NodeStatus::default()));
            statuses.insert((id, name.into()), status.clone());
            let (exec, requests) = mpsc::channel();
            let (events, event_rx) = mpsc::channel();
            let directory = (name != GATEWAY).then(|| dir.join(format!("{name}.db")));
            if let Some(path) = &directory {
                drop(tape_rs::daemon::directory::Directory::open(path).unwrap());
            }
            let files = FileService::with_options(
                dir.join(format!("spool-{name}")),
                BatchPolicy {
                    max_bytes: 1 << 20,
                    max_files: 10,
                    idle: Duration::from_millis(25),
                    max_wait: Duration::from_millis(200),
                },
                directory.clone(),
            )
            .unwrap();
            files.set_executor(exec.clone());
            files.set_task_events(events.clone());
            if name == GATEWAY {
                let files = files.clone();
                std::thread::spawn(move || {
                    multilib::run_gateway(
                        Box::new(multilib::GatewayProvider),
                        requests,
                        events,
                        files,
                    )
                });
            } else {
                let enabled = Arc::new(AtomicBool::new(true));
                flags.insert((id, name), enabled.clone());
                let provider = Box::new(Provider {
                    lib: libs[name].clone(),
                    id,
                    enabled,
                });
                let (files, status) = (files.clone(), status.clone());
                std::thread::spawn(move || {
                    executor::run(
                        provider,
                        ExecOptions {
                            node_id: id as u8,
                            interval: Duration::from_millis(40),
                            demo_write: false,
                            salvage: false,
                            block_size: 65536,
                            read_idle: Duration::from_secs(10),
                        },
                        requests,
                        events,
                        files,
                        status,
                    )
                });
            }
            let scoped = if name == GATEWAY {
                peers[&id].clone()
            } else {
                multilib::scoped_sender(name.into(), peers[&id].clone())
            };
            let forward = scoped.clone();
            std::thread::spawn(move || {
                for event in event_rx {
                    if forward.send(NodeInput::Exec(event)).is_err() {
                        break;
                    }
                }
            });
            contexts.insert(
                name.to_owned(),
                Arc::new(HttpContext {
                    files,
                    status: status.clone(),
                    exec: Mutex::new(exec.clone()),
                    node: Mutex::new(scoped),
                    client_addrs: addresses.clone(),
                    wait_timeout: Duration::from_secs(10),
                }),
            );
            runtimes.push(LibraryRuntime {
                id: name.into(),
                group: format!("group-{name}"),
                drives: usize::from(name != GATEWAY),
                directory_file: directory,
                exec,
                status,
                files: contexts[name].files.clone(),
            });
        }
        let gateway = contexts.remove(GATEWAY).unwrap();
        tape_rs::daemon::s3::serve(
            &s3_addresses[&id],
            contexts.clone(),
            gateway.clone(),
            s3_addresses.clone(),
        )
        .unwrap();
        gateway::serve(
            &addresses[&id],
            config.clone(),
            contexts,
            gateway,
            addresses.clone(),
        )
        .unwrap();
        let cfg = NodeConfig {
            id,
            peers: vec![1, 2, 3],
            tick: Duration::from_millis(25),
            election_ticks: 40,
            heartbeat_ticks: 3,
            status_file: None,
            directory_file: Some(dir.join("bundle.db")),
            cooldown: Duration::from_millis(500),
            snapshot: SnapshotPolicy {
                entries: 48,
                bytes: 8 << 20,
            },
            shutdown_grace: Duration::from_secs(10),
            drives: 0,
            coordination: Some(CoordinationConfig {
                group: "cluster".into(),
                validity: Duration::from_secs(6),
            }),
        };
        let store = RaftStore::open(&dir.join("raft.db"), &[1, 2, 3]).unwrap();
        let (config, net, tx, rx) = (
            config.clone(),
            Box::new(Net(peers.clone())),
            peers[&id].clone(),
            receivers.remove(&id).unwrap(),
        );
        joins.push(std::thread::spawn(move || {
            multilib::run(cfg, config, store, net, None, tx, rx, runtimes)
        }));
    }
    wait(|| {
        statuses[&(3, "55L3A7802K19LL01".into())]
            .lock()
            .unwrap()
            .leader
            != 0
    });
    let leader = statuses[&(3, "55L3A7802K19LL01".into())]
        .lock()
        .unwrap()
        .leader;
    if leader != 3 {
        peers[&leader]
            .send(NodeInput::Raft(Box::new(Message {
                from: 3,
                to: leader,
                msg_type: MessageType::MsgTransferLeader,
                ..Default::default()
            })))
            .unwrap();
    }
    let owner = |name: &str| {
        let s = statuses[&(3, name.into())].lock().unwrap();
        s.executor
            .as_ref()
            .filter(|_| s.executor_ready)
            .map(|e| e.node)
    };
    wait(|| {
        owner("55L3A7802K19LL01") == Some(3)
            && owner("55L3A78011W9LL01") == Some(3)
            && owner(GATEWAY) == Some(3)
    });
    // ExecutorReady 先于后续 Serving ReadIndex 许可，等待实际可服务状态。
    wait(|| exchange(&s3_addresses[&3], "GET", "/", &[]).0 == 200);
    // 两库允许相同池名、UUID、条码和路径，目录与介质内容分别归属。
    let admin = |library: &str, cmd: Command| {
        let (reply, rx) = mpsc::channel();
        peers[&3]
            .send(NodeInput::Library {
                library: library.into(),
                event: Box::new(NodeInput::Admin { cmd, reply }),
            })
            .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            AdminReply::Ok(_)
        ));
    };
    for name in ["55L3A7802K19LL01", "55L3A78011W9LL01"] {
        admin(
            name,
            Command::PoolCreate {
                uuid: "same-uuid".into(),
                name: "same-name".into(),
                file_limit: 200_000,
            },
        );
        admin(
            name,
            Command::TapeAssign {
                barcode: "SAMEL8".into(),
                pool: "same-name".into(),
            },
        );
    }
    let endpoint = &addresses[&3];
    // 新建/分配池的控制日志先提交，实际装带与准入随后就绪。
    for library in ["55L3A7802K19LL01", "55L3A78011W9LL01"] {
        wait(|| {
            let (code, body) = exchange(
                endpoint,
                "GET",
                &format!("/libraries/{library}/cluster"),
                &[],
            );
            code == 200
                && serde_json::from_slice::<serde_json::Value>(&body)
                    .is_ok_and(|v| v["serving_round"].is_u64())
        });
    }
    let (code, body) = exchange(endpoint, "GET", "/admin/libraries", &[]);
    assert_eq!(code, 200);
    let public: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(public["default_library"], "55L3A7802K19LL01");
    assert_eq!(public["libraries"][0]["id"], "55L3A78011W9LL01");
    assert_eq!(public["libraries"][1]["id"], "55L3A7802K19LL01");
    let mut file_tasks = HashMap::new();
    for (name, data) in [
        ("55L3A7802K19LL01", b"library A".as_slice()),
        ("55L3A78011W9LL01", b"library B".as_slice()),
    ] {
        let mut task = None;
        wait(|| {
            let (code, body) = exchange(
                endpoint,
                "PUT",
                &format!("/libraries/{name}/files/same?wait=1"),
                data,
            );
            assert!(
                code == 201 || code == 503,
                "{code}: {}",
                String::from_utf8_lossy(&body)
            );
            if code == 201 {
                task = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|j| j["task"].as_u64());
            }
            code == 201
        });
        file_tasks.insert(name, task.expect("已提交响应包含文件任务号"));
    }
    for id in 1..=3 {
        for name in ["55L3A7802K19LL01", "55L3A78011W9LL01"] {
            wait(|| {
                tape_rs::daemon::directory::Directory::open_reader(
                    &root.join(id.to_string()).join(format!("{name}.db")),
                )
                .unwrap()
                .stat("same-uuid", "/same")
                .unwrap()
                .is_some()
            });
        }
    }
    for name in ["55L3A7802K19LL01", "55L3A78011W9LL01"] {
        flags[&(3, name)].store(false, Ordering::SeqCst);
    }
    flags[&(1, "55L3A78011W9LL01")].store(false, Ordering::SeqCst);
    flags[&(2, "55L3A7802K19LL01")].store(false, Ordering::SeqCst);
    wait(|| owner("55L3A7802K19LL01") == Some(1) && owner("55L3A78011W9LL01") == Some(2));
    // 同名、同 UUID 池投影为不同 bucket；固定节点 3 的 S3 入口分别转发到 owner 1/2。
    let mut s3_buckets = Vec::new();
    for (library, data) in [
        ("55L3A7802K19LL01", b"S3 library A".as_slice()),
        ("55L3A78011W9LL01", b"S3 library B".as_slice()),
    ] {
        let bucket = tape_rs::daemon::s3::bucket_name(library, "same-uuid");
        assert!(!s3_buckets.contains(&bucket));
        s3_buckets.push(bucket.clone());
        wait(|| {
            let put = exchange(
                &s3_addresses[&3],
                "PUT",
                &format!("/{bucket}/s3-only"),
                data,
            );
            assert!(
                put.0 == 200 || put.0 == 503,
                "{}: {}",
                put.0,
                String::from_utf8_lossy(&put.1)
            );
            put.0 == 200
        });
        assert_eq!(
            exchange(&s3_addresses[&3], "GET", &format!("/{bucket}/s3-only"), &[]),
            (200, data.to_vec())
        );
        assert_eq!(
            exchange(
                endpoint,
                "GET",
                &format!("/libraries/{library}/pools/same-uuid/files/s3-only"),
                &[]
            ),
            (200, data.to_vec())
        );
    }
    assert_eq!(
        exchange(
            &s3_addresses[&3],
            "DELETE",
            &format!("/{}/s3-only", s3_buckets[0]),
            &[]
        )
        .0,
        204
    );
    assert_eq!(
        exchange(
            &s3_addresses[&3],
            "GET",
            &format!("/{}/s3-only", s3_buckets[0]),
            &[]
        )
        .0,
        404
    );
    assert_eq!(
        exchange(
            &s3_addresses[&3],
            "GET",
            &format!("/{}/s3-only", s3_buckets[1]),
            &[]
        ),
        (200, b"S3 library B".to_vec())
    );
    let ctl =
        std::env::var_os("LTFSCTL_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_ltfsctl").into());
    let create = std::process::Command::new(&ctl)
        .args([
            "-e",
            endpoint,
            "--library",
            "55L3A78011W9LL01",
            "nodegroup",
            "create",
            "TESTG",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    assert_eq!(
        exchange(
            endpoint,
            "GET",
            "/libraries/55L3A78011W9LL01/admin/nodegroups/TESTG",
            &[]
        )
        .0,
        200
    );
    assert_eq!(
        exchange(
            endpoint,
            "GET",
            "/libraries/55L3A7802K19LL01/admin/nodegroups/TESTG",
            &[]
        )
        .0,
        404
    );
    let remove = std::process::Command::new(&ctl)
        .args([
            "-e",
            endpoint,
            "--library",
            "55L3A78011W9LL01",
            "nodegroup",
            "delete",
            "TESTG",
        ])
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    for (name, task) in &file_tasks {
        wait(|| {
            let (code, body) = exchange(
                endpoint,
                "GET",
                &format!("/libraries/{name}/pools/same-uuid/tasks/{task}"),
                &[],
            );
            code == 200
                && serde_json::from_slice::<serde_json::Value>(&body)
                    .is_ok_and(|j| j["status"] == "committed")
        });
    }
    // 终态存于每个节点的复制目录；B 暂时没有设备 owner 时仍能查询原任务。
    flags[&(2, "55L3A78011W9LL01")].store(false, Ordering::SeqCst);
    wait(|| owner("55L3A78011W9LL01").is_none());
    let (code, body) = exchange(
        endpoint,
        "GET",
        &format!(
            "/libraries/55L3A78011W9LL01/pools/same-uuid/tasks/{}",
            file_tasks["55L3A78011W9LL01"]
        ),
        &[],
    );
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let task: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(task["status"], "committed");
    flags[&(2, "55L3A78011W9LL01")].store(true, Ordering::SeqCst);
    wait(|| owner("55L3A78011W9LL01") == Some(2));
    // 同一入口仍为节点 3；数据由节点 1/2 处理，客户端未得到物理 owner 地址。
    for (name, data) in [
        ("55L3A7802K19LL01", b"library A".as_slice()),
        ("55L3A78011W9LL01", b"library B".as_slice()),
    ] {
        wait(|| {
            let (code, body) = exchange(
                endpoint,
                "GET",
                &format!("/libraries/{name}/files/same"),
                &[],
            );
            assert!(
                code == 200 || code == 503,
                "{code}: {}",
                String::from_utf8_lossy(&body)
            );
            if code == 200 {
                assert_eq!(body, data);
                true
            } else {
                false
            }
        });
    }
    // FUSE 与 ltfsctl 共用的 Client 在转发后仍按真实序列号隔离。
    for (name, data) in [
        ("55L3A7802K19LL01", b"client A".as_slice()),
        ("55L3A78011W9LL01", b"client B".as_slice()),
    ] {
        let mut client = tape_rs::client::Client::new([endpoint.clone()])
            .with_library(name)
            .unwrap();
        assert!(client.put("/client-same", data).unwrap().committed);
        assert_eq!(client.get("/client-same").unwrap(), data);
        assert_eq!(
            client.stat("/client-same").unwrap().unwrap().length,
            data.len() as u64
        );
    }
    let mut source = tape_rs::client::Client::new([endpoint.clone()])
        .with_library("55L3A7802K19LL01")
        .unwrap()
        .with_pool("same-uuid")
        .unwrap();
    let mut destination = tape_rs::client::Client::new([endpoint.clone()])
        .with_library("55L3A78011W9LL01")
        .unwrap()
        .with_pool("same-uuid")
        .unwrap();
    source
        .setxattr("/client-same", "user.copy-note", b"preserved", 0)
        .unwrap();
    let copied = source
        .copy_verified_to(&mut destination, "/client-same", "/copy-from-a", &root)
        .unwrap();
    assert_eq!(copied.source.sha256, copied.destination.sha256);
    assert_eq!(
        copied.source.metadata["node"],
        copied.destination.metadata["node"]
    );
    let user_attr = |metadata: &serde_json::Value| {
        metadata["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attr| attr["key"] == "copy-note")
            .cloned()
    };
    assert_eq!(
        user_attr(&copied.source.metadata),
        user_attr(&copied.destination.metadata)
    );
    assert_eq!(destination.get("/copy-from-a").unwrap(), b"client A");
    assert!(matches!(
        source.copy_verified_to(&mut destination, "/client-same", "/copy-from-a", &root),
        Err(tape_rs::client::ClientError::Exists(_))
    ));
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".ltfsctl-copy-")
    }));
    assert_eq!(destination.get("/client-same").unwrap(), b"client B");
    assert!(
        source
            .put("/client-same", b"client A newer")
            .unwrap()
            .committed
    );
    assert!(matches!(
        source.delete_if_version("/client-same", copied.source.version),
        Err(tape_rs::client::ClientError::Verification(_))
    ));
    assert_eq!(source.get("/client-same").unwrap(), b"client A newer");
    assert!(
        source
            .put("/cas-delete", b"remove this version")
            .unwrap()
            .committed
    );
    let version = source.stat("/cas-delete").unwrap().unwrap().version;
    source.delete_if_version("/cas-delete", version).unwrap();
    assert!(source.stat("/cas-delete").unwrap().is_none());
    // 旧别名拒绝；省略选择仍访问显式默认库。
    assert_eq!(
        exchange(endpoint, "GET", "/libraries/l8/files/same", &[]).0,
        404
    );
    assert_eq!(
        exchange(endpoint, "GET", "/files/same", &[]),
        (200, b"library A".to_vec())
    );
    let (code, body) = exchange(
        endpoint,
        "POST",
        "/libraries/55L3A78011W9LL01/admin/pools/extra",
        &[],
    );
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    wait(|| {
        statuses[&(2, "55L3A78011W9LL01".into())]
            .lock()
            .unwrap()
            .pools
            .len()
            == 2
    });
    assert_eq!(
        statuses[&(2, "55L3A7802K19LL01".into())]
            .lock()
            .unwrap()
            .pools
            .len(),
        1
    );
    // Follower 入口的管理写入也应在保留库作用域的情况下转到控制 Leader。
    let (code, body) = exchange(
        &addresses[&1],
        "POST",
        "/libraries/55L3A78011W9LL01/admin/pools/via-follower",
        &[],
    );
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    wait(|| {
        statuses[&(2, "55L3A78011W9LL01".into())]
            .lock()
            .unwrap()
            .pools
            .len()
            == 3
    });
    assert_eq!(
        statuses[&(2, "55L3A7802K19LL01".into())]
            .lock()
            .unwrap()
            .pools
            .len(),
        1
    );
    assert_eq!(
        exchange(
            endpoint,
            "GET",
            "/libraries/55L3A78011W9LL01/files/same",
            &[]
        )
        .0,
        400,
        "多个池必须显式选择，不能落到任意池"
    );
    // A 完全失去通路，B 的许可、数据和入口资格继续有效。
    flags[&(1, "55L3A7802K19LL01")].store(false, Ordering::SeqCst);
    wait(|| owner("55L3A7802K19LL01").is_none());
    assert_eq!(owner("55L3A78011W9LL01"), Some(2));
    assert_eq!(exchange(endpoint, "GET", "/readyz", &[]).0, 200);
    let (code, body) = exchange(
        endpoint,
        "PUT",
        "/libraries/55L3A78011W9LL01/pools/same-uuid/files/after-loss?wait=1",
        b"still B",
    );
    assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        exchange(
            endpoint,
            "GET",
            "/libraries/55L3A78011W9LL01/pools/same-uuid/files/after-loss",
            &[]
        ),
        (200, b"still B".to_vec())
    );
    // A 的 owner 已自然退出：经 HTTP 复制暂停屏障，再冻结旧目录；B 仍可服务。
    let a = "55L3A7802K19LL01";
    wait(|| statuses[&(3, a.into())].lock().unwrap().executor.is_none());
    let (code, body) = exchange(
        endpoint,
        "POST",
        &format!("/libraries/{a}/admin/catalog/current/quiesce"),
        br#"{"confirm":true}"#,
    );
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
    let quiesce_index =
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["quiesce_index"]
            .as_u64()
            .unwrap();
    wait(|| {
        (1..=3).all(|id| {
            statuses[&(id, a.into())]
                .lock()
                .unwrap()
                .catalog_quiesce_index
                == Some(quiesce_index)
        })
    });
    let (code, body) = exchange(
        endpoint,
        "GET",
        &format!("/libraries/{a}/admin/catalog/current"),
        &[],
    );
    assert_eq!(code, 200);
    let catalog: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(catalog["payload"]["authority"], "raft_sqlite");
    assert_eq!(catalog["payload"]["quiesce_index"], quiesce_index);
    assert_eq!(
        exchange(
            endpoint,
            "POST",
            &format!("/libraries/{a}/admin/pools/frozen"),
            &[],
        )
        .0,
        409
    );
    admin(a, Command::CatalogFreeze { quiesce_index });
    wait(|| {
        statuses[&(3, a.into())]
            .lock()
            .unwrap()
            .catalog_frozen_index
            .is_some()
    });
    let frozen_index = statuses[&(3, a.into())]
        .lock()
        .unwrap()
        .catalog_frozen_index
        .unwrap();
    wait(|| {
        (1..=3).all(|id| {
            let (code, body) = exchange(
                &addresses[&id],
                "GET",
                &format!("/libraries/{a}/admin/catalog/current/audit"),
                &[],
            );
            if code != 200 {
                return false;
            }
            let audit: serde_json::Value = serde_json::from_slice(&body).unwrap();
            audit["answered_by"] == id
                && audit["frozen_index"] == frozen_index
                && audit["projected_frozen_index"] == frozen_index
                && audit["frozen_projection_caught_up"] == true
                && audit["has_staging"] == false
                && audit["has_unresolved_file_tasks"] == false
        })
    });
    for id in 1..=3 {
        let (code, body) = exchange(
            &addresses[&id],
            "POST",
            &format!("/libraries/{a}/admin/catalog/current/export"),
            &[],
        );
        assert_eq!(code, 200, "节点 {id}: {}", String::from_utf8_lossy(&body));
        let exported = root.join(format!("node-{id}-frozen-export.db"));
        std::fs::write(&exported, &body).unwrap();
        let snapshot = tape_rs::daemon::directory::Directory::open_reader(&exported).unwrap();
        assert_eq!(
            snapshot.cutover_audit().unwrap().frozen_index,
            Some(frozen_index)
        );
        assert!(snapshot.stat("same-uuid", "/same").unwrap().is_some());
    }
    assert!(
        statuses[&(3, "55L3A78011W9LL01".into())]
            .lock()
            .unwrap()
            .catalog_frozen_index
            .is_none()
    );
    assert_eq!(
        exchange(
            endpoint,
            "GET",
            "/libraries/55L3A78011W9LL01/pools/same-uuid/files/after-loss",
            &[]
        ),
        (200, b"still B".to_vec())
    );
    assert_eq!(
        exchange(endpoint, "GET", "/libraries/missing/stat/same", &[]).0,
        404
    );
    // 每个节点只有一份日志；快照包包含两个隔离目录，索引与全局一致。
    wait(|| root.join("3/bundle.snap.db").exists());
    let bundle = rusqlite::Connection::open(root.join("3/bundle.snap.db")).unwrap();
    assert_eq!(
        bundle
            .query_row("SELECT count(*) FROM libraries", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
    for tx in peers.values() {
        let _ = tx.send(NodeInput::Shutdown);
    }
    for join in joins {
        join.join().unwrap().unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}
