//! Control-only runtimes must catch up without a legacy SQLite directory bundle.
use raft::eraftpb::{Message, MessageType};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tape_rs::daemon::{
    files::{BatchPolicy, FileService},
    multilib::{self, GATEWAY, GatewayProvider, LibrariesConfig, LibraryRuntime},
    net::{AdminReply, Network, NodeInput},
    node::{CoordinationConfig, NodeConfig, NodeStatus, SharedStatus},
    state::Command,
    store::{RaftStore, SnapshotPolicy},
};
const LIB: &str = "55L3A78011W9LL01";
struct Net {
    peers: Arc<HashMap<u64, mpsc::Sender<NodeInput>>>,
    blocked: Arc<AtomicBool>,
    snapshots: Arc<AtomicU64>,
}
impl Network for Net {
    fn send(&self, message: Message) {
        if self.blocked.load(Ordering::SeqCst) && (message.from == 3 || message.to == 3) {
            return;
        }
        if message.to == 3 && message.msg_type == MessageType::MsgSnapshot {
            self.snapshots.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(tx) = self.peers.get(&message.to) {
            let _ = tx.send(NodeInput::Raft(Box::new(message)));
        }
    }
}
fn wait(check: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(12);
    while !check() {
        assert!(
            Instant::now() < until,
            "control-only snapshot catchup failed to restore owner permission"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn control_only_snapshot_catchup_restores_owner_permission_without_sqlite_bundle() {
    let root = std::env::temp_dir().join(format!("control-snapshot-{}", uuid::Uuid::new_v4()));
    let config = LibrariesConfig::parse(&json!({"default_library":LIB,"libraries":[{"changer_serial":LIB,"drive_serials":["TESTDRIVE"]}]})).unwrap();
    let mut senders = HashMap::new();
    let mut receivers = HashMap::new();
    for id in 1..=3 {
        let (tx, rx) = mpsc::channel();
        senders.insert(id, tx);
        receivers.insert(id, rx);
    }
    let peers = Arc::new(senders);
    let blocked = Arc::new(AtomicBool::new(true));
    let snapshots = Arc::new(AtomicU64::new(0));
    let mut statuses = HashMap::<(u64, String), SharedStatus>::new();
    let mut joins = Vec::new();
    for id in 1..=3 {
        let dir = root.join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let mut runtimes = Vec::new();
        for name in [LIB, GATEWAY] {
            let status = Arc::new(Mutex::new(NodeStatus::default()));
            statuses.insert((id, name.into()), status.clone());
            let (exec, requests) = mpsc::channel();
            let (events, event_rx) = mpsc::channel();
            let files = FileService::with_options(
                dir.join(format!("spool-{name}")),
                BatchPolicy::default(),
                None,
            )
            .unwrap();
            let forward = multilib::scoped_sender(name.into(), peers[&id].clone());
            std::thread::spawn(move || {
                for event in event_rx {
                    if forward.send(NodeInput::Exec(event)).is_err() {
                        break;
                    }
                }
            });
            let worker_files = files.clone();
            std::thread::spawn(move || {
                multilib::run_gateway(Box::new(GatewayProvider), requests, events, worker_files)
            });
            runtimes.push(LibraryRuntime {
                id: name.into(),
                group: format!("test-{name}"),
                drives: 0,
                directory_file: None,
                exec,
                files,
                status,
            });
        }
        let cfg = NodeConfig {
            id,
            peers: vec![1, 2, 3],
            tick: Duration::from_millis(20),
            election_ticks: 20,
            heartbeat_ticks: 2,
            status_file: None,
            directory_file: None,
            cooldown: Duration::from_millis(100),
            snapshot: SnapshotPolicy {
                entries: 8,
                bytes: 8 << 20,
            },
            shutdown_grace: Duration::from_secs(2),
            drives: 0,
            coordination: Some(CoordinationConfig {
                group: "cluster".into(),
                validity: Duration::from_secs(2),
            }),
        };
        let store = RaftStore::open(&dir.join("raft.db"), &[1, 2, 3]).unwrap();
        let tx = peers[&id].clone();
        let rx = receivers.remove(&id).unwrap();
        let net = Box::new(Net {
            peers: peers.clone(),
            blocked: blocked.clone(),
            snapshots: snapshots.clone(),
        });
        let config = config.clone();
        joins.push(std::thread::spawn(move || {
            multilib::run(cfg, config, store, net, None, tx, rx, runtimes)
        }));
    }
    wait(|| statuses[&(1, LIB.into())].lock().unwrap().executor_ready);
    let leader = statuses[&(1, LIB.into())].lock().unwrap().leader;
    for i in 0..32 {
        let (reply, answer) = mpsc::channel();
        peers[&leader]
            .send(NodeInput::Library {
                library: LIB.into(),
                event: Box::new(NodeInput::Admin {
                    cmd: Command::PoolCreate {
                        uuid: format!("pool-{i}"),
                        name: format!("pool-{i}"),
                        file_limit: 1000,
                    },
                    reply,
                }),
            })
            .unwrap();
        assert!(matches!(
            answer.recv_timeout(Duration::from_secs(3)).unwrap(),
            AdminReply::Ok(_)
        ));
    }
    wait(|| {
        let db = rusqlite::Connection::open_with_flags(
            root.join(leader.to_string()).join("raft.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row("SELECT min(idx) FROM entries", [], |r| r.get::<_, u64>(0))
            .is_ok_and(|i| i > 8)
    });
    blocked.store(false, Ordering::SeqCst);
    wait(|| snapshots.load(Ordering::SeqCst) > 0);
    wait(|| statuses[&(3, LIB.into())].lock().unwrap().pools.len() == 32);
    peers[&leader]
        .send(NodeInput::Raft(Box::new(Message {
            from: 3,
            to: leader,
            msg_type: MessageType::MsgTransferLeader,
            ..Default::default()
        })))
        .unwrap();
    wait(|| {
        let s = statuses[&(3, LIB.into())].lock().unwrap();
        s.leader == 3
            && s.executor_ready
            && s.executor.as_ref().is_some_and(|e| e.node == 3)
            && s.local.contains("服务中")
    });
    for id in 1..=3 {
        peers[&id].send(NodeInput::Shutdown).unwrap();
    }
    for join in joins {
        join.join().unwrap().unwrap();
    }
    assert!(!root.join("3/bundle.db").exists());
    std::fs::remove_dir_all(root).unwrap();
}
