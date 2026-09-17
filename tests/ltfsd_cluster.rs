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
    services: HashMap<u64, Arc<tape_rs::daemon::files::FileService>>,
    execs: HashMap<u64, Sender<tape_rs::daemon::executor::ExecRequest>>,
    inboxes: Arc<HashMap<u64, Sender<NodeInput>>>,
    isolated: Arc<Mutex<HashSet<u64>>>,
    dir: PathBuf,
}

impl Cluster {
    fn start(name: &str) -> Self {
        Self::start_with(name, true)
    }

    fn start_with(name: &str, demo_write: bool) -> Self {
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
        let mut services = HashMap::new();
        let mut execs = HashMap::new();
        for id in ids {
            let shared: SharedStatus = Arc::new(Mutex::new(NodeStatus::default()));
            status.insert(id, shared.clone());
            let (exec_tx, exec_rx) = channel();
            let (ev_tx, ev_rx) = channel();
            let provider = Box::new(SimProvider { lib: lib.clone(), initiator: id as u32 });
            let eopts = ExecOptions { node_id: id as u8, interval: Duration::from_millis(40), demo_write, salvage: false };
            let files = tape_rs::daemon::files::FileService::new(dir.join(format!("spool-{}", id))).unwrap();
            files.set_executor(exec_tx.clone());
            services.insert(id, files.clone());
            execs.insert(id, exec_tx.clone());
            thread::spawn(move || executor::run(provider, eopts, exec_rx, ev_tx, files));
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
        Cluster { lib, status, services, execs, inboxes, isolated, dir }
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

// ---------- 文件服务 ----------

use tape_rs::daemon::executor::ExecRequest;
use tape_rs::daemon::files::{FileService, ServiceError, TaskStatus};

fn body(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(17).wrapping_add(seed)).collect()
}

/// 走与 HTTP 处理相同的调用序列：准入 → 写暂存并 ingest → finish → 等结论。
fn upload(svc: &FileService, path: &str, data: &[u8]) -> std::result::Result<TaskStatus, ServiceError> {
    let h = svc.begin(path, data.len() as u64)?;
    std::fs::write(&h.spool, data).unwrap();
    svc.ingest(&h, data.len() as u64)?;
    let task = svc.finish(h, data.len() as u64)?;
    Ok(svc.wait_task(task, Duration::from_secs(15)).unwrap())
}

impl Cluster {
    fn read_via(&self, node: u64, path: &str) -> std::result::Result<Vec<u8>, String> {
        let (tx, rx) = channel();
        self.execs[&node].send(ExecRequest::Read { path: path.to_string(), reply: tx }).unwrap();
        rx.recv_timeout(Duration::from_secs(15)).map_err(|e| e.to_string())?
    }
}

#[test]
fn upload_is_committed_to_tape_and_only_the_executor_serves() {
    let c = Cluster::start_with("files", false);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = &c.services[&leader];
    c.wait("文件服务开放", || svc.serving_round() == Some(round));

    let data = body(300_000, 5);
    assert!(svc.stat("/docs/a.bin").unwrap().is_none(), "提交前不可见");
    match upload(svc, "/docs/a.bin", &data).unwrap() {
        TaskStatus::Committed { generation } => assert!(generation >= 2),
        other => panic!("{other:?}"),
    }
    let st = svc.stat("/docs/a.bin").unwrap().unwrap();
    assert_eq!((st.len, st.round), (data.len() as u64, round));
    assert_eq!(c.read_via(leader, "/docs/a.bin").unwrap(), data, "读回的内容来自磁带");
    assert_eq!(svc.list().unwrap().get("/docs/a.bin"), Some(&(data.len() as u64)));

    // 其他节点不服务
    let follower = (1..=3).find(|i| *i != leader).unwrap();
    assert!(matches!(c.services[&follower].begin("/x", 1), Err(ServiceError::NotServing(_))));
    assert!(matches!(c.services[&follower].stat("/docs/a.bin"), Err(ServiceError::NotServing(_))));

    // 并发上传合批落带
    let results: Vec<_> = thread::scope(|sc| {
        (0..6)
            .map(|i| sc.spawn(move || upload(svc, &format!("/batch/f{}.bin", i), &body(20_000 + i, i as u8)).unwrap()))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    assert!(results.iter().all(|r| matches!(r, TaskStatus::Committed { .. })), "{results:?}");
    for i in 0..6 {
        assert_eq!(c.read_via(leader, &format!("/batch/f{}.bin", i)).unwrap(), body(20_000 + i, i as u8));
    }
    // 同一路径正在上传时再次准入被拒
    let h = svc.begin("/busy.bin", 10).unwrap();
    assert!(matches!(svc.begin("/busy.bin", 10), Err(ServiceError::PathBusy(_))));
    svc.abort(h);
    c.shutdown();
}

/// 跨故障切换的不变量：对客户端报告过"已提交"的文件，新 Leader 上一定可见且内容正确；
/// 没报告成功的上传可以丢，但不能以损坏的形式出现。
#[test]
fn committed_uploads_survive_failover_and_unconfirmed_ones_never_corrupt() {
    let c = Cluster::start_with("files-failover", false);
    let (old, r1) = c.wait_serving(None, 0);
    let svc_old = c.services[&old].clone();
    c.wait("文件服务开放", || svc_old.serving_round() == Some(r1));
    let f1 = body(120_000, 1);
    assert!(matches!(upload(&svc_old, "/f1.bin", &f1).unwrap(), TaskStatus::Committed { .. }));

    // 隔开旧 Leader，同时持续向它上传，直到它不再接受：保证有请求撞上隔离的那一刻
    c.isolated.lock().unwrap().insert(old);
    let svc2 = svc_old.clone();
    let racing = thread::spawn(move || {
        let mut out = Vec::new();
        for i in 0..400usize {
            let r = upload(&svc2, &format!("/race/{}.bin", i), &body(50_000, (i % 200) as u8));
            let stop = matches!(r, Err(ServiceError::NotServing(_)));
            out.push((i, r));
            if stop {
                break;
            }
        }
        out
    });

    let (new, r2) = c.wait_serving(Some(old), r1 + 1);
    let svc_new = c.services[&new].clone();
    c.wait("新 Leader 的文件服务开放", || svc_new.serving_round() == Some(r2));
    let raced = racing.join().unwrap();

    assert_eq!(svc_new.stat("/f1.bin").unwrap().map(|s| s.len), Some(f1.len() as u64));
    assert_eq!(c.read_via(new, "/f1.bin").unwrap(), f1);
    let (mut confirmed, mut visible_n, mut unconfirmed) = (0, 0, Vec::new());
    for (i, outcome) in &raced {
        let path = format!("/race/{}.bin", i);
        let visible = svc_new.stat(&path).unwrap();
        match outcome {
            Ok(TaskStatus::Committed { .. }) => {
                confirmed += 1;
                assert!(visible.is_some(), "{path} 报告过已提交，新 Leader 上必须可见");
            }
            Ok(TaskStatus::Failed { .. }) => unconfirmed.push(format!("{i}:failed")),
            Ok(TaskStatus::Indeterminate { .. }) => unconfirmed.push(format!("{i}:indeterminate")),
            Ok(TaskStatus::Staged) => unconfirmed.push(format!("{i}:staged")),
            Err(_) => unconfirmed.push(format!("{i}:rejected")),
        }
        if visible.is_some() {
            visible_n += 1;
            assert_eq!(c.read_via(new, &path).unwrap(), body(50_000, (*i % 200) as u8), "{path} 可见则内容必须完整正确");
        }
    }
    println!("竞争上传 {} 个：确认提交 {}，新 Leader 上可见 {}，未确认 {:?}", raced.len(), confirmed, visible_n, unconfirmed);
    assert!(!unconfirmed.is_empty(), "至少最后一个请求应当撞上停止服务");

    // 旧 Leader 最终停止服务，新 Leader 正常接受上传
    c.wait("旧 Leader 停止服务", || svc_old.serving_round().is_none());
    assert!(matches!(upload(&svc_new, "/after.bin", &body(1000, 9)).unwrap(), TaskStatus::Committed { .. }));
    c.shutdown();
}

// ---------- 执行线程的消息次序 ----------

use tape_rs::daemon::executor::ExecEvent;

fn lone_executor(name: &str) -> (Sender<ExecRequest>, std::sync::mpsc::Receiver<ExecEvent>) {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    mkltfs(&lib.drive_as(0, 50), &MkltfsOptions { volume_id: "LTFSD1".into(), block_size: 64 * 1024, ..Default::default() }).unwrap();
    let (tx, rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let dir = std::env::temp_dir().join(format!("ltfsd-exec-{}-{}", name, std::process::id()));
    let files = FileService::new(dir).unwrap();
    let opts = ExecOptions { node_id: 1, interval: Duration::from_secs(3600), demo_write: false, salvage: false };
    thread::spawn(move || executor::run(Box::new(SimProvider { lib, initiator: 1 }), opts, rx, ev_tx, files));
    (tx, ev_rx)
}

/// 压力下复现过的竞态：同一节点先后有轮次 2、3；针对轮次 2 的停手请求在轮次 3 的接管之后才到。
/// 它不能把轮次 3 的状态清掉，否则随后的恢复请求无人响应，节点永远停在"正在恢复卷"。
#[test]
fn a_late_stop_for_an_older_round_does_not_cancel_the_newer_round() {
    let (tx, ev) = lone_executor("late-stop");
    tx.send(ExecRequest::Takeover { round: 2 }).unwrap();
    tx.send(ExecRequest::Takeover { round: 3 }).unwrap();
    tx.send(ExecRequest::Stop { round: Some(2), reason: "过时".into() }).unwrap();
    tx.send(ExecRequest::Recover { round: 3 }).unwrap();
    let got: Vec<ExecEvent> = (0..3).map(|_| ev.recv_timeout(Duration::from_secs(10)).unwrap()).collect();
    assert_eq!(got[0], ExecEvent::Fenced { round: 2 });
    assert_eq!(got[1], ExecEvent::Fenced { round: 3 });
    assert!(matches!(&got[2], ExecEvent::Serving { round: 3, .. }), "{:?}", got[2]);
}

/// 执行线程对没有对应状态的恢复请求必须给出结论，不能悄悄忽略。
#[test]
fn recover_for_an_unknown_round_is_reported_not_ignored() {
    let (tx, ev) = lone_executor("unknown-round");
    tx.send(ExecRequest::Recover { round: 9 }).unwrap();
    assert!(matches!(ev.recv_timeout(Duration::from_secs(10)).unwrap(), ExecEvent::Lost { round: 9, .. }));
    // 无条件停手之后同样如此
    tx.send(ExecRequest::Takeover { round: 4 }).unwrap();
    assert_eq!(ev.recv_timeout(Duration::from_secs(10)).unwrap(), ExecEvent::Fenced { round: 4 });
    tx.send(ExecRequest::Stop { round: None, reason: "停".into() }).unwrap();
    tx.send(ExecRequest::Recover { round: 4 }).unwrap();
    assert!(matches!(ev.recv_timeout(Duration::from_secs(10)).unwrap(), ExecEvent::Lost { round: 4, .. }));
}

// ---------- 经客户端库（真实 HTTP）----------

use tape_rs::client::Client;
use tape_rs::daemon::http::{self, HttpContext};

impl Cluster {
    /// 给三个节点各开一个本机 HTTP 接口，返回地址列表。
    fn start_http(&self) -> Vec<String> {
        let ports: Vec<u16> = (0..3)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port())
            .collect();
        let addrs: HashMap<u64, String> = (1..=3u64).map(|id| (id, format!("127.0.0.1:{}", ports[id as usize - 1]))).collect();
        for id in 1..=3u64 {
            let ctx = Arc::new(HttpContext {
                files: self.services[&id].clone(),
                status: self.status[&id].clone(),
                exec: Mutex::new(self.execs[&id].clone()),
                client_addrs: addrs.clone(),
                wait_timeout: Duration::from_secs(15),
            });
            http::serve(&addrs[&id], ctx).unwrap();
        }
        (1..=3u64).map(|id| addrs[&id].clone()).collect()
    }
}

/// 应用只管连续 put。中途 Leader 被隔开：客户端库自己等待、找新 Leader、判定结果未定并重传，
/// 应用一次错误都不该看到；事后每个文件都已提交且内容正确。
#[test]
fn client_library_hides_failover_from_the_application() {
    let c = Cluster::start_with("client", false);
    let (old, r1) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || c.services[&old].serving_round() == Some(r1));
    let endpoints = c.start_http();

    let mut cl = Client::new(endpoints.clone());
    cl.retry_for = Duration::from_secs(30);
    let first = cl.put("/app/first.bin", &body(70_000, 3)).unwrap();
    assert_eq!((first.attempts, first.resolved_by_query), (1, false));
    assert_eq!(cl.get("/app/first.bin").unwrap(), body(70_000, 3));
    assert_eq!(cl.stat("/app/none.bin").unwrap(), None);

    let isolated = c.isolated.clone();
    let cutter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        isolated.lock().unwrap().insert(old);
    });
    let mut outcomes = Vec::new();
    let started = Instant::now();
    let mut i = 0usize;
    // 至少持续到新 Leader 接管之后再多传几个
    while i < 12 || started.elapsed() < Duration::from_secs(3) {
        let o = cl.put(&format!("/app/{:03}.bin", i), &body(40_000 + i, i as u8)).unwrap_or_else(|e| panic!("put {i}: {e}"));
        outcomes.push(o);
        i += 1;
    }
    cutter.join().unwrap();

    let (new, r2) = c.wait_serving(Some(old), r1 + 1);
    assert_ne!(new, old);
    let retried = outcomes.iter().filter(|o| o.attempts > 1).count();
    let by_query = outcomes.iter().filter(|o| o.resolved_by_query).count();
    println!("经客户端库上传 {} 个：重传过 {} 个，经查询判定 {} 个；执行轮次 {} -> {}", outcomes.len(), retried, by_query, r1, r2);

    let mut fresh = Client::new(endpoints);
    for k in 0..outcomes.len() {
        let st = fresh.stat(&format!("/app/{:03}.bin", k)).unwrap().unwrap_or_else(|| panic!("{k} 未提交"));
        assert_eq!(st.length, (40_000 + k) as u64);
    }
    for k in [0, outcomes.len() / 2, outcomes.len() - 1] {
        assert_eq!(fresh.get(&format!("/app/{:03}.bin", k)).unwrap(), body(40_000 + k, k as u8));
    }
    assert_eq!(fresh.get("/app/first.bin").unwrap(), body(70_000, 3));
    assert!(fresh.list().unwrap().len() >= outcomes.len() + 1);
    c.shutdown();
}
