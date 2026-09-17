//! ltfsd 骨架的三节点集成测试：节点在同一进程里用内存通道通信，共享一个模拟带库，
//! 每个节点以不同的发起方身份访问设备。对应研究笔记的 AF02/AF03 与接管序列。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use raft::eraftpb::Message;
use tape_rs::daemon::executor::{self, DeviceKind, DeviceProvider, ExecOptions, ManagedDevice};
use tape_rs::daemon::net::{Network, NodeInput};
use tape_rs::daemon::node::{self, NodeConfig, NodeStatus, SharedStatus};
use tape_rs::daemon::store::RaftStore;
use tape_rs::error::Result;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::scsi::reservation::{ReservationKey, fence, read_status, release_and_unregister};
use tape_rs::scsi::sim::{SimCartridge, SimLibrary};

const BARCODE: &str = "LTFSD1L8";

#[derive(Clone)]
struct ChannelNet {
    me: u64,
    peers: Arc<HashMap<u64, Sender<NodeInput>>>,
    /// 被隔离的节点：收发都丢弃
    isolated: Arc<Mutex<HashSet<u64>>>,
}

impl Network for ChannelNet {
    fn send(&self, msg: Message) {
        let iso = self.isolated.lock().unwrap();
        if iso.contains(&self.me) || iso.contains(&msg.to) {
            return;
        }
        if let Some(tx) = self.peers.get(&msg.to) {
            let _ = tx.send(NodeInput::Raft(Box::new(msg)));
        }
    }
}

struct SimProvider {
    lib: SimLibrary,
    initiator: u32,
}

impl DeviceProvider for SimProvider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        Ok(vec![
            ManagedDevice { name: "drive0".into(), kind: DeviceKind::Drive, dev: Box::new(self.lib.drive_as(0, self.initiator)) },
            ManagedDevice { name: "changer".into(), kind: DeviceKind::Changer, dev: Box::new(self.lib.changer()) },
        ])
    }
}

struct Cluster {
    lib: SimLibrary,
    status: HashMap<u64, SharedStatus>,
    inboxes: Arc<HashMap<u64, Sender<NodeInput>>>,
    isolated: Arc<Mutex<HashSet<u64>>>,
    dir: PathBuf,
}

impl Cluster {
    fn start(name: &str) -> Self {
        let lib = SimLibrary::new(1, 4, 1);
        lib.insert_cartridge(SimCartridge::blank(BARCODE, 256 << 20), 0).unwrap();
        lib.load_into_drive(BARCODE, 0).unwrap();
        let opts = MkltfsOptions { volume_id: "LTFSD1".into(), block_size: 64 * 1024, ..Default::default() };
        mkltfs(&lib.drive_as(0, 50), &opts).unwrap();

        let dir = std::env::temp_dir().join(format!("ltfsd-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ids = [1u64, 2, 3];
        let mut txs = HashMap::new();
        let mut rxs = HashMap::new();
        for id in ids {
            let (tx, rx) = channel();
            txs.insert(id, tx);
            rxs.insert(id, rx);
        }
        let inboxes = Arc::new(txs);
        let isolated = Arc::new(Mutex::new(HashSet::new()));
        let mut status = HashMap::new();
        for id in ids {
            let shared: SharedStatus = Arc::new(Mutex::new(NodeStatus::default()));
            status.insert(id, shared.clone());
            let (exec_tx, exec_rx) = channel();
            let (ev_tx, ev_rx) = channel();
            let provider = Box::new(SimProvider { lib: lib.clone(), initiator: id as u32 });
            let eopts = ExecOptions { node_id: id as u8, interval: Duration::from_millis(40), demo_write: true, salvage: false };
            thread::spawn(move || executor::run(provider, eopts, exec_rx, ev_tx));
            let inbox_tx = inboxes[&id].clone();
            thread::spawn(move || {
                for ev in ev_rx {
                    if inbox_tx.send(NodeInput::Exec(ev)).is_err() {
                        break;
                    }
                }
            });
            let cfg = NodeConfig {
                id,
                peers: ids.to_vec(),
                tick: Duration::from_millis(15),
                election_ticks: 10,
                heartbeat_ticks: 2,
                status_file: None,
                cooldown: Duration::from_millis(1500),
            };
            let store = RaftStore::open(&dir.join(format!("raft-{}.db", id)), &ids).unwrap();
            let net = Box::new(ChannelNet { me: id, peers: inboxes.clone(), isolated: isolated.clone() });
            let rx = rxs.remove(&id).unwrap();
            thread::spawn(move || node::run(cfg, store, net, rx, exec_tx, shared).unwrap());
        }
        Cluster { lib, status, inboxes, isolated, dir }
    }

    fn st(&self, id: u64) -> NodeStatus {
        self.status[&id].lock().unwrap().clone()
    }

    /// 等到某个节点在服务，返回 (节点, 轮次)。`not` 用来排除旧执行者。
    fn wait_serving(&self, not: Option<u64>, min_round: u64) -> (u64, u64) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for id in [1u64, 2, 3] {
                if Some(id) == not {
                    continue;
                }
                let s = self.st(id);
                if let Some(e) = &s.executor {
                    if e.node == id && e.fenced && e.round >= min_round && s.local.contains("服务中") {
                        return (id, e.round);
                    }
                }
            }
            assert!(Instant::now() < deadline, "没有节点进入服务: {:?}", [self.st(1), self.st(2), self.st(3)]);
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait<F: Fn() -> bool>(&self, what: &str, f: F) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < deadline, "等待超时: {} {:?}", what, [self.st(1), self.st(2), self.st(3)]);
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn demo_files(&self) -> Vec<String> {
        // 用一个独立的发起方只读查看介质内容会被预留拒绝，所以直接看模拟介质上的末索引
        let c = self.lib.cartridge(BARCODE).unwrap();
        let xml = c.partitions[1]
            .objects
            .iter()
            .rev()
            .find_map(|o| match o {
                tape_rs::scsi::sim::LogicalObject::Record(d) if d.windows(10).any(|w| w == b"<ltfsindex") => Some(d.clone()),
                _ => None,
            })
            .unwrap();
        let idx = tape_rs::ltfs::index::LtfsIndex::parse(&xml).unwrap();
        let mut v = Vec::new();
        idx.walk_files(|p, _| v.push(p.to_string()));
        v.sort();
        v
    }

    fn shutdown(self) {
        for tx in self.inboxes.values() {
            let _ = tx.send(NodeInput::Shutdown);
        }
        thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn leader_takes_over_the_library_and_serves() {
    let c = Cluster::start("basic");
    let (leader, round) = c.wait_serving(None, 0);
    // 设备上的持有者就是 (leader, round)
    let key = ReservationKey::new(leader as u8, round);
    let st = read_status(&c.lib.drive_as(0, 99)).unwrap();
    assert_eq!(st.holder.map(|h| h.0), Some(key.0));
    assert_eq!(st.keys, vec![key.0]);
    // 三个节点对"谁是执行者"的结论一致
    c.wait("状态收敛", || (1..=3).all(|i| c.st(i).executor.as_ref().map(|e| (e.node, e.round, e.fenced)) == Some((leader, round, true))));
    c.wait("演示写入", || c.demo_files().iter().filter(|f| f.contains(&format!("round{:06}", round))).count() >= 2);
    c.shutdown();
}

/// Leader 被网络隔开后仍在写带；另外两个节点选出新 Leader 并接管。
#[test]
fn partitioned_leader_is_fenced_by_the_new_leader() {
    let c = Cluster::start("partition");
    let (old, r1) = c.wait_serving(None, 0);
    c.wait("旧 Leader 先写几个文件", || c.demo_files().len() >= 2);
    c.isolated.lock().unwrap().insert(old);

    let (new, r2) = c.wait_serving(Some(old), r1 + 1);
    assert_ne!(new, old);
    assert!(r2 > r1, "新一轮的轮次更大");
    let key = ReservationKey::new(new as u8, r2);
    assert_eq!(read_status(&c.lib.drive_as(0, 99)).unwrap().keys, vec![key.0], "旧执行者没有重新注册");
    c.wait("新 Leader 写入", || c.demo_files().iter().any(|f| f.contains(&format!("round{:06}", r2))));
    c.wait("旧执行者停手", || !c.st(old).local.contains("服务中"));

    // 网络恢复：旧节点回到集群，承认新执行者，不再碰设备
    c.isolated.lock().unwrap().clear();
    c.wait("旧节点追上日志", || c.st(old).executor.as_ref().map(|e| e.round) >= Some(r2));
    thread::sleep(Duration::from_millis(300));
    let cur = c.st(new).executor.unwrap();
    assert_eq!(read_status(&c.lib.drive_as(0, 99)).unwrap().holder.map(|h| h.0), Some(ReservationKey::new(cur.node as u8, cur.round).0));

    // 卷始终一致：两轮的文件都在，没有残留的不完整尾部挡住写入
    let files = c.demo_files();
    assert!(files.iter().any(|f| f.contains(&format!("round{:06}", r1))), "{files:?}");
    assert!(files.iter().any(|f| f.contains(&format!("round{:06}", r2))), "{files:?}");
    c.shutdown();
}

/// 预留被外力夺走（例如运维误操作或别的软件）：执行者停手、放弃本轮、让出领导权，
/// 由另一个节点以更高轮次接管。原 Leader 不自己抢回。
#[test]
fn leader_that_loses_its_reservation_yields_and_another_node_takes_over() {
    let c = Cluster::start("lost");
    let (old, r1) = c.wait_serving(None, 0);
    let intruder = c.lib.drive_as(0, 77);
    let foreign = ReservationKey(0x4000_0000_0a83_0948);
    fence(&intruder, foreign).unwrap();

    c.wait("原执行者发现并放弃", || c.st(old).local.contains("已放弃"));
    // 外来的键必须让开，新执行者才能取得设备（它不受轮次规则保护，会被抢占）
    let (new, r2) = c.wait_serving(Some(old), r1 + 1);
    assert_ne!(new, old);
    assert!(r2 > r1);
    let st = read_status(&c.lib.drive_as(0, 99)).unwrap();
    assert_eq!(st.keys, vec![ReservationKey::new(new as u8, r2).0]);

    c.wait("新执行者写入", || c.demo_files().iter().any(|f| f.contains(&format!("round{:06}", r2))));
    // 新执行者自己挂载：尾部完整、可写
    let _ = release_and_unregister(&intruder, foreign);
    c.shutdown();
}
