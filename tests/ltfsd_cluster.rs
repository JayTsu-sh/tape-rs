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
use tape_rs::daemon::net::{AdminReply, DirectoryFetch, Network, NodeInput};
use tape_rs::daemon::state::Command;
use tape_rs::daemon::node::{self, NodeConfig, NodeStatus, SharedStatus};
use tape_rs::daemon::store::RaftStore;
use tape_rs::error::Result;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::scsi::reservation::{ReservationKey, fence, read_status, release_and_unregister};
use tape_rs::scsi::sim::{SimCartridge, SimLibrary};

const BARCODE: &str = "LTFSD1L8";
const POOL: &str = "00000000-0000-4000-8000-000000000001";

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

/// 进程内的"节点间通道"：直接复制对端的目录库快照文件。
struct FileFetch {
    dir: PathBuf,
    isolated: Arc<Mutex<HashSet<u64>>>,
    me: u64,
}

impl DirectoryFetch for FileFetch {
    fn fetch(&self, from: u64, min_index: u64, dest: &std::path::Path) -> Result<u64> {
        let err = |s: String| tape_rs::error::TapeError::Ltfs(s);
        {
            let iso = self.isolated.lock().unwrap();
            if iso.contains(&self.me) || iso.contains(&from) {
                return Err(err(format!("与节点 {} 不通", from)));
            }
        }
        let src = tape_rs::daemon::directory::Directory::snapshot_path(&self.dir.join(format!("directory-{}.db", from)));
        if !src.exists() {
            return Err(err(format!("节点 {} 还没有目录库快照", from)));
        }
        let index = tape_rs::daemon::directory::Directory::applied_index_of(&src)?;
        if index < min_index {
            return Err(err(format!("节点 {} 的目录库快照只到 {}", from, index)));
        }
        std::fs::copy(&src, dest).map_err(|e| err(e.to_string()))?;
        Ok(index)
    }
}

struct SimProvider {
    lib: SimLibrary,
    initiator: u32,
    drives: usize,
}

impl DeviceProvider for SimProvider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let mut v: Vec<ManagedDevice> = (0..self.drives)
            .map(|i| ManagedDevice { name: format!("drive{}", i), serial: format!("SIMDRV{:04}", i), kind: DeviceKind::Drive, dev: Box::new(self.lib.drive_as(i, self.initiator)) })
            .collect();
        v.push(ManagedDevice { name: "changer".into(), serial: "SIMLIB0000000001".into(), kind: DeviceKind::Changer, dev: Box::new(self.lib.changer()) });
        Ok(v)
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
        Self::start_lib(name, demo_write, lib, 200_000, &[BARCODE])
    }

    /// 用准备好的模拟库启动三节点集群，建一个池并把 `assign` 里的条码归进去。
    fn start_lib(name: &str, demo_write: bool, lib: SimLibrary, file_limit: u64, assign: &[&str]) -> Self {
        Self::start_lib_with(name, demo_write, lib, file_limit, assign, None)
    }

    /// `claimed_drives` 是节点配置里声称的驱动器数（默认与模拟库一致）。故意报多，
    /// 可以让 Leader 的受理检查放行、执行线程在设备上才发现不够。
    fn start_lib_with(name: &str, demo_write: bool, lib: SimLibrary, file_limit: u64, assign: &[&str], claimed_drives: Option<usize>) -> Self {

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
            let provider = Box::new(SimProvider { lib: lib.clone(), initiator: id as u32, drives: lib.drive_count() });
            let eopts = ExecOptions { node_id: id as u8, interval: Duration::from_millis(40), demo_write, salvage: false, block_size: 64 * 1024, read_idle: Duration::from_millis(250) };
            let policy = tape_rs::daemon::files::BatchPolicy { max_bytes: 4 << 20, max_files: 50, idle: Duration::from_millis(25), max_wait: Duration::from_millis(400) };
            let files = tape_rs::daemon::files::FileService::with_options(dir.join(format!("spool-{}", id)), policy, Some(dir.join(format!("directory-{}.db", id)))).unwrap();
            let status_for_exec = shared.clone();
            files.set_executor(exec_tx.clone());
            services.insert(id, files.clone());
            execs.insert(id, exec_tx.clone());
            thread::spawn(move || executor::run(provider, eopts, exec_rx, ev_tx, files, status_for_exec));
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
                // 选举超时 510–1020 ms（raft-rs 在 [n, 2n) 里随机），已经接近生产的 1 秒。
                // 压到 300 ms 时，并行跑十几份、每份二十个用例的调度抖动足以让 Leader 误判失去多数派
                election_ticks: 34,
                heartbeat_ticks: 3,
                status_file: None,
                directory_file: Some(dir.join(format!("directory-{}.db", id))),
                cooldown: Duration::from_millis(1500),
                shutdown_grace: Duration::from_secs(20),
                drives: claimed_drives.unwrap_or(lib.drive_count()),
                // 压得很紧：每个用例都会走一遍快照与日志压缩
                snapshot: tape_rs::daemon::store::SnapshotPolicy { entries: 32, bytes: 8 << 20 },
            };
            let store = RaftStore::open(&dir.join(format!("raft-{}.db", id)), &ids).unwrap();
            let net = Box::new(ChannelNet { me: id, peers: inboxes.clone(), isolated: isolated.clone() });
            let fetch = Arc::new(FileFetch { dir: dir.clone(), isolated: isolated.clone(), me: id });
            let self_tx = inboxes[&id].clone();
            let rx = rxs.remove(&id).unwrap();
            thread::spawn(move || {
                node::run_with(cfg, store, net, Some(fetch), Some(self_tx), rx, exec_tx, shared).unwrap()
            });
        }
        let c = Cluster { lib, status, services, execs, inboxes, isolated, dir };
        // 未归属的磁带系统绝不触碰：先建池并把模拟库里的这盘带归进去
        c.admin(Command::PoolCreate { uuid: POOL.into(), name: "pool".into(), file_limit });
        for b in assign {
            c.admin(Command::TapeAssign { barcode: (*b).into(), pool: POOL.into() });
        }
        c
    }

    /// 向当前 Leader 提交一条管理命令并等它应用。
    fn admin(&self, cmd: Command) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for id in 1..=3u64 {
                let (tx, rx) = channel();
                let _ = self.inboxes[&id].send(NodeInput::Admin { cmd: cmd.clone(), reply: tx });
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(AdminReply::Ok(_)) => return,
                    // 上一次尝试其实已经成功、只是回复来晚了：重发会被状态机以"已存在/已归属"拒绝
                    Ok(AdminReply::Rejected(why)) if why.contains("已存在") || why.contains("已归属") => return,
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "管理命令没有被受理: {:?}", cmd);
            thread::sleep(Duration::from_millis(30));
        }
    }

    /// 向当前 Leader 提交一条管理命令，返回状态机（或 Leader 受理阶段）的结论。
    fn admin_result(&self, cmd: Command) -> std::result::Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for id in 1..=3u64 {
                let (tx, rx) = channel();
                let _ = self.inboxes[&id].send(NodeInput::Admin { cmd: cmd.clone(), reply: tx });
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(AdminReply::Ok(s)) => return Ok(s),
                    Ok(AdminReply::Rejected(why)) => return Err(why),
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "管理命令没有得到结论: {:?}", cmd);
            thread::sleep(Duration::from_millis(30));
        }
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
        // 停机现在要落带、退带、放预留，别在它做完之前把数据目录删掉
        thread::sleep(Duration::from_millis(400));
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
    /// 读一个文件；"稍后重试"（唯一的驱动器正忙）就重试，直到超时。
    fn read_via(&self, node: u64, path: &str) -> std::result::Result<Vec<u8>, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (tx, rx) = channel();
            self.execs[&node].send(ExecRequest::Read { path: path.to_string(), reply: tx }).unwrap();
            match rx.recv_timeout(Duration::from_secs(15)).map_err(|e| e.to_string())? {
                Ok(d) => return Ok(d),
                Err(tape_rs::daemon::executor::ReadError::Busy(why)) if Instant::now() < deadline => {
                    let _ = why;
                    thread::sleep(Duration::from_millis(30));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
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

    // PN06：旧 Leader 被隔开期间落带的文件，它们的目录记录没能进日志。新 Leader 接管时按磁带重写
    // 该带的目录；网络恢复后三个节点的目录库都应包含新 Leader 上可见的全部文件。
    c.isolated.lock().unwrap().clear();
    let visible: Vec<String> = raced.iter().map(|(i, _)| format!("/race/{}.bin", i)).filter(|p| svc_new.stat(p).unwrap().is_some()).collect();
    for id in 1..=3u64 {
        let db = c.dir.join(format!("directory-{}.db", id));
        c.wait("目录按磁带对账补齐", || {
            let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
            d.stat(POOL, "/f1.bin").unwrap().is_some() && visible.iter().all(|p| d.stat(POOL, p).unwrap().is_some())
        });
    }

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
    let opts = ExecOptions { node_id: 1, interval: Duration::from_secs(3600), demo_write: false, salvage: false, block_size: 64 * 1024, read_idle: Duration::from_millis(250) };
    let status: SharedStatus = Arc::new(Mutex::new(NodeStatus::default()));
    thread::spawn(move || executor::run(Box::new(SimProvider { lib, initiator: 1, drives: 1 }), opts, rx, ev_tx, files, status));
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
    ///
    /// 取端口的办法是 bind 0、记下端口、放开、再 bind，中间有个窗口；并行跑十几份时
    /// 内核会把同一个临时端口发给两份，于是第二份 `AddrInUse`。拿不到就换一组重来。
    fn start_http(&self) -> Vec<String> {
        for _ in 0..20 {
            let ports: Vec<u16> = (0..3)
                .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port())
                .collect();
            let addrs: HashMap<u64, String> =
                (1..=3u64).map(|id| (id, format!("127.0.0.1:{}", ports[id as usize - 1]))).collect();
            let mut bound = true;
            for id in 1..=3u64 {
                let ctx = Arc::new(HttpContext {
                    files: self.services[&id].clone(),
                    status: self.status[&id].clone(),
                    exec: Mutex::new(self.execs[&id].clone()),
                    node: Mutex::new(self.inboxes[&id].clone()),
                    client_addrs: addrs.clone(),
                    wait_timeout: Duration::from_secs(15),
                });
                if http::serve(&addrs[&id], ctx).is_err() {
                    bound = false;
                    break;
                }
            }
            if bound {
                return (1..=3u64).map(|id| addrs[&id].clone()).collect();
            }
        }
        panic!("找不到三个可用的本机端口");
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

// ---------- 池与磁带归属（P1）----------

/// 管理命令经 Raft 提交：只有 Leader 受理（客户端库自动找到它），被状态机拒绝的命令不改变状态，
/// 三个节点从各自已应用的日志给出相同的答案。
#[test]
fn pools_and_tape_assignments_are_replicated_to_every_node() {
    let c = Cluster::start_with("pools", false);
    c.wait_serving(None, 0);
    let endpoints = c.start_http();
    // 故意把一个 Follower 放在最前面：库要靠 503 里的提示找到 Leader
    let leader = (1..=3u64).find(|i| c.st(*i).role == "Leader").unwrap();
    let mut order = endpoints.clone();
    order.rotate_left(leader as usize % 3);
    let mut cl = Client::new(order);

    let uuid = cl.pool_create("archive", Some(50_000)).unwrap();
    assert_eq!(uuid.len(), 36);
    let dup = cl.pool_create("archive", None).unwrap_err();
    assert!(matches!(dup, tape_rs::client::ClientError::Rejected { status: 409, .. }), "{dup}");
    cl.pool_create("scratch", None).unwrap();
    cl.tape_assign("TR8000L08", "archive").unwrap();
    cl.tape_assign("TR8001L08", &uuid).unwrap();
    assert!(cl.tape_assign("TR8000L08", "scratch").is_err(), "已归属的带不能再归入别的池");
    assert!(cl.tape_assign("TR8002L08", "no-such-pool").is_err());
    cl.tape_assign("TR8002L08", "scratch").unwrap();
    cl.tape_unassign("TR8002L08").unwrap();
    assert!(cl.tape_unassign("TR8002L08").is_err());

    let want = |pools: &[tape_rs::client::PoolInfo]| {
        pools.len() == 3
            && pools.iter().any(|p| p.name == "archive" && p.uuid == uuid && p.file_limit == 50_000 && p.tapes == ["TR8000L08", "TR8001L08"])
            && pools.iter().any(|p| p.name == "scratch" && p.file_limit == 200_000 && p.tapes.is_empty())
    };
    assert!(want(&cl.pools(None).unwrap()));
    for e in &endpoints {
        let e = e.clone();
        c.wait("每个节点都应用到同样的池状态", || Client::new([e.clone()]).pools(Some(&e)).map(|p| want(&p)).unwrap_or(false));
    }
    // 每个节点的目录库与它的状态机一致
    for id in 1..=3u64 {
        let d = tape_rs::daemon::directory::Directory::open(&c.dir.join(format!("directory-{}.db", id))).unwrap();
        let rows = d.pools().unwrap();
        assert_eq!(rows.iter().find(|p| p.name == "archive").unwrap().tapes, ["TR8000L08", "TR8001L08"]);
        assert!(d.applied_index() > 0);
    }
    c.shutdown();
}

// ---------- 合批与目录（P2）----------

/// PN01：并发上传被合成少数几次卷提交；目录记录带着写带时算出的
/// sha256 复制到每个节点；带上有文件时不能解除归属。
#[test]
fn uploads_are_batched_and_catalogued_on_every_node() {
    use sha2::{Digest, Sha256};
    let c = Cluster::start_with("batching", false);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = &c.services[&leader];
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let base = svc.stat("/nothing").map(|_| ()).is_ok();
    assert!(base);

    // 测试用的策略：每批最多 50 个文件、空闲 25 ms、最长等待 400 ms
    let n = 130usize;
    let gens: Vec<u64> = thread::scope(|sc| {
        (0..n)
            .map(|i| sc.spawn(move || match upload(svc, &format!("/b/{:03}.bin", i), &body(3000 + i, i as u8)).unwrap() {
                TaskStatus::Committed { generation } => generation,
                other => panic!("{other:?}"),
            }))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    let mut per_gen: HashMap<u64, usize> = HashMap::new();
    for g in &gens {
        *per_gen.entry(*g).or_default() += 1;
    }
    println!("{} 个并发上传 -> {} 次卷提交，每批 {:?}", n, per_gen.len(), { let mut v: Vec<_> = per_gen.values().copied().collect(); v.sort(); v });
    assert!(per_gen.len() <= n / 5, "应当合批，而不是每个文件提交一次: {}", per_gen.len());
    // 文件数与字节数是**触发阈值**而不是硬上限：队列达到阈值就提交，取队列时把已到齐的全部带走
    // （VolumeState 的冻结一次覆盖所有已完成的上传）。所以一批可以超过阈值，但阈值保证了不会无限攒着。
    assert!(per_gen.values().any(|&k| k >= 50), "突发上传应当凑出达到阈值的批: {per_gen:?}");

    // 一个慢速上传者：每个文件都在空闲触发之内到达，靠"最长等待"封顶
    let t0 = Instant::now();
    let slow: Vec<u64> = thread::scope(|sc| {
        let hs: Vec<_> = (0..40usize)
            .map(|i| {
                let h = sc.spawn(move || match upload(svc, &format!("/slow/{:02}.bin", i), &body(500, i as u8)).unwrap() {
                    TaskStatus::Committed { generation } => generation,
                    other => panic!("{other:?}"),
                });
                thread::sleep(Duration::from_millis(12));
                h
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let distinct: std::collections::BTreeSet<u64> = slow.iter().copied().collect();
    println!("慢速上传 40 个，历时 {:?} -> {} 次卷提交", t0.elapsed(), distinct.len());
    assert!(distinct.len() >= 2, "最长等待到期应当切出一批，而不是无限等下去");
    assert!(distinct.len() <= 8);

    // 目录记录（路径、长度、sha256、条码、代数）经 Raft 到达每个节点
    let want = Sha256::digest(body(3000 + 7, 7)).iter().map(|b| format!("{:02x}", b)).collect::<String>();
    for id in 1..=3u64 {
        let db = c.dir.join(format!("directory-{}.db", id));
        c.wait("目录复制到每个节点", || {
            let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
            d.list(POOL).map(|l| l.len()).unwrap_or(0) == n + 40
        });
        let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
        let row = d.stat(POOL, "/b/007.bin").unwrap().unwrap();
        assert_eq!((row.barcode.as_str(), row.length, row.sha256.as_str()), (BARCODE, 3007, want.as_str()));
        assert!(d.tape_generation(BARCODE).unwrap() >= *gens.iter().max().unwrap());
    }
    assert_eq!(svc.stat("/b/007.bin").unwrap().unwrap().sha256, want);

    // 带上有文件：不能解除归属
    let (tx, rx) = channel();
    c.inboxes[&leader].send(NodeInput::Admin { cmd: Command::TapeUnassign { barcode: BARCODE.into() }, reply: tx }).unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), AdminReply::Rejected(_)));
    c.shutdown();
}

// ---------- 选带、装载、格式化、换带（P3）----------

const MIB: usize = 1 << 20;

/// 三盘很小的空白带都在槽位里，第四盘不归属。
fn small_tape_library(tape_mib: u64) -> SimLibrary {
    let lib = SimLibrary::new(1, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8", "PA0003L8", "ZZ0009L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, tape_mib << 20), i).unwrap();
    }
    lib
}

fn put_until<F: Fn(&std::result::Result<TaskStatus, ServiceError>) -> bool>(svc: &FileService, prefix: &str, size: usize, max: usize, stop: F) -> Vec<(String, std::result::Result<TaskStatus, ServiceError>)> {
    let mut out = Vec::new();
    for i in 0..max {
        let path = format!("/{}/{:03}.bin", prefix, i);
        // 换带期间服务端答"稍后重试"，这里就重试（客户端库也是这么做的）
        let deadline = Instant::now() + Duration::from_secs(20);
        let r = loop {
            match upload(svc, &path, &body(size, i as u8)) {
                Err(ServiceError::SwitchingTape(_)) | Err(ServiceError::NotServing(_)) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                other => break other,
            }
        };
        let done = stop(&r);
        out.push((path, r));
        if done {
            break;
        }
    }
    out
}

/// PN03、PN04：空白的已归属磁带被自动装载、格式化并写上池标记；一盘写满就卸回槽位换下一盘；
/// 三盘都满之后上传以确定的"没有可写的磁带"拒绝；未归属的那盘始终没被碰过。
#[test]
fn blank_tapes_are_formatted_on_first_use_and_full_tapes_are_switched() {
    let lib = small_tape_library(12);
    let c = Cluster::start_lib("switch", false, lib, 200_000, &["PA0001L8", "PA0002L8", "PA0003L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    assert_eq!(svc.tape().unwrap().barcode, "PA0001L8", "按条码顺序取第一盘空白带");

    // 比单盘还大的文件：确定的拒绝，不触发换带
    let huge = svc.begin("/huge.bin", 64 << 20).map(|h| h.task);
    assert!(matches!(huge, Err(ServiceError::TooLarge { .. })), "{huge:?}；{}", c.st(leader).local);
    assert_eq!(svc.tape().unwrap().barcode, "PA0001L8");

    let results = put_until(&svc, "data", MIB, 60, |r| matches!(r, Err(ServiceError::NoTape(_))));
    let committed: Vec<&String> = results.iter().filter(|(_, r)| matches!(r, Ok(TaskStatus::Committed { .. }))).map(|(p, _)| p).collect();
    let (last_path, last) = results.last().unwrap();
    assert!(matches!(last, Err(ServiceError::NoTape(_))), "{last_path}: {last:?}");
    assert_eq!(committed.len(), results.len() - 1, "除最后一个外全部提交: {:?}", results.iter().map(|(_, r)| r.is_ok()).collect::<Vec<_>>());

    // 文件分布在三盘带上，目录知道每个文件在哪一盘
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录追上", || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap().list(POOL).map(|l| l.len()).unwrap_or(0) == committed.len());
    let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
    let mut per_tape: HashMap<String, usize> = HashMap::new();
    for p in &committed {
        *per_tape.entry(d.stat(POOL, p).unwrap().unwrap().barcode).or_default() += 1;
    }
    println!("{} 个 1 MiB 文件分布: {:?}", committed.len(), per_tape);
    assert_eq!(per_tape.len(), 3);
    assert!(per_tape.values().all(|&n| n >= 3));

    // 三盘都标为已满；未归属的那盘仍是空白、仍在原槽位
    c.wait("三盘都标为 full", || {
        let st = c.st(leader);
        ["PA0001L8", "PA0002L8", "PA0003L8"].iter().all(|b| st.tapes.get(*b).is_some_and(|t| t.state == "full"))
    });
    let untouched = c.lib.cartridge("ZZ0009L8").unwrap();
    assert_eq!(untouched.partitions.len(), 1);
    assert!(untouched.partitions[0].objects.is_empty(), "未归属的带不得被格式化或写入");
    // 每盘用过的带都带着池标记
    for b in ["PA0001L8", "PA0002L8", "PA0003L8"] {
        let cart = c.lib.cartridge(b).unwrap();
        let xml = cart.partitions[1].objects.iter().rev().find_map(|o| match o {
            tape_rs::scsi::sim::LogicalObject::Record(r) if r.windows(10).any(|w| w == b"<ltfsindex") => Some(String::from_utf8_lossy(r).to_string()),
            _ => None,
        }).unwrap();
        assert!(xml.contains("ltfs.mediaPool.uuid") && xml.contains(POOL), "{b} 缺池标记");
    }
    // 池满了只是写不了：三盘带上的文件 stat 和 list 仍然答得出（来自目录）
    for p in [committed[0], committed[committed.len() / 2], committed[committed.len() - 1]] {
        assert!(svc.stat(p).unwrap().is_some(), "{p}");
    }
    assert_eq!(svc.list().unwrap().len(), committed.len());
    c.shutdown();
}

/// PN02：文件数到上限的带转 data_full，即使还有空间；新上传落到下一盘。
#[test]
fn file_count_limit_switches_to_the_next_tape() {
    let lib = small_tape_library(64);
    let c = Cluster::start_lib("filelimit", false, lib, 5, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let results = put_until(&svc, "small", 2000, 8, |_| false);
    assert!(results.iter().all(|(_, r)| matches!(r, Ok(TaskStatus::Committed { .. }))), "{:?}", results);
    c.wait("第一盘转 data_full", || c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "data_full"));
    assert_eq!(svc.tape().unwrap().barcode, "PA0002L8");
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录追上", || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap().list(POOL).map(|l| l.len()).unwrap_or(0) == 8);
    let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
    let on_first = results.iter().filter(|(p, _)| d.stat(POOL, p).unwrap().unwrap().barcode == "PA0001L8").count();
    assert_eq!(on_first, 5, "第一盘正好放到上限");
    // 第一盘已卸回槽位，驱动器里是第二盘
    assert_eq!(c.st(leader).tapes["PA0002L8"].state, "appendable");
    // 两盘带上的文件 stat 都答得出：第二盘来自当前索引，第一盘来自目录
    assert!(svc.stat(&results[0].0).unwrap().is_some());
    assert!(svc.stat(&results[7].0).unwrap().is_some());
    assert_eq!(svc.stat(&results[0].0).unwrap().unwrap().barcode, "PA0001L8");
    c.shutdown();
}

/// 带上已有别的数据（不是 LTFS）：不自动覆盖，标为 label_mismatch 后跳过，用下一盘。
#[test]
fn a_tape_holding_foreign_data_is_never_formatted() {
    let lib = small_tape_library(16);
    lib.with_cartridge_mut("PA0001L8", |c| {
        let p = &mut c.partitions[0];
        p.objects.push(tape_rs::scsi::sim::LogicalObject::Record(b"TAR ARCHIVE written by another product".to_vec()));
        p.objects.push(tape_rs::scsi::sim::LogicalObject::Filemark);
        p.flushed = p.objects.len();
    });
    let c = Cluster::start_lib("foreign", false, lib, 200_000, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    assert_eq!(svc.tape().unwrap().barcode, "PA0002L8");
    c.wait("第一盘标为 label_mismatch", || c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "label_mismatch"));
    let foreign = c.lib.cartridge("PA0001L8").unwrap();
    assert_eq!(foreign.partitions.len(), 1, "没有被重新分区");
    assert!(matches!(&foreign.partitions[0].objects[0], tape_rs::scsi::sim::LogicalObject::Record(r) if r.starts_with(b"TAR ARCHIVE")));
    assert!(matches!(upload(&svc, "/ok.bin", &body(1000, 1)).unwrap(), TaskStatus::Committed { .. }));
    c.shutdown();
}

/// 被拒绝的大上传必须让客户端看到那条错误，而不是连接重置：用了 Expect 的客户端不必发内容，
/// 没用的客户端由服务端把内容读掉再回应。
#[test]
fn rejected_large_uploads_get_a_clean_answer_over_http() {
    use std::io::{Read, Write};
    let lib = small_tape_library(12);
    let c = Cluster::start_lib("reject-http", false, lib, 200_000, &["PA0001L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || c.services[&leader].serving_round() == Some(round));
    let endpoints = c.start_http();

    // 客户端库（大内容走 Expect: 100-continue）
    let mut cl = Client::new(endpoints.clone());
    let big = vec![7u8; 40 << 20];
    match cl.put("/too-big.bin", &big) {
        Err(tape_rs::client::ClientError::Rejected { status: 507, body }) => assert!(body.contains("too_large") || !body.is_empty(), "{body}"),
        other => panic!("{other:?}"),
    }
    // 不用 Expect、闷头发内容的客户端
    let addr = &endpoints[leader as usize - 1];
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    write!(s, "PUT /files/too-big-2.bin HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", big.len()).unwrap();
    s.write_all(&big).expect("服务端应当把内容读掉，而不是重置连接");
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 507"), "{}", &resp[..resp.len().min(120)]);
    assert!(resp.contains("too_large"));
    // 正常大小的照常成功
    assert_eq!(cl.put("/ok.bin", &vec![1u8; 300_000]).unwrap().attempts, 1);
    c.shutdown();
}

// ---------- 文件契约 F1（file-contract.md）----------

/// 发一个不带内容的请求，返回整个响应文本。
fn raw_http(addr: &str, req: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// FC01 只创建、FC02 替换在途时旧版本可读、FC09 不在服务的节点不答 404、FC10 Range、FC11 按目录列举，
/// 以及在途状态经 stat 可见、只在途的路径读取答 not_committed。
#[test]
fn file_contract_create_only_in_flight_state_range_and_directories() {
    use tape_rs::client::ClientError;
    let c = Cluster::start_with("f1", false);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = &c.services[&leader];
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());

    // FC01：只创建
    let v1 = body(90_000, 1);
    cl.put("/f1/a.bin", &v1).unwrap();
    let before = cl.stat("/f1/a.bin").unwrap().unwrap();
    match cl.put_new("/f1/a.bin", &body(10, 9)) {
        Err(ClientError::Exists(p)) => assert_eq!(p, "/f1/a.bin"),
        other => panic!("{other:?}"),
    }
    assert_eq!(cl.stat("/f1/a.bin").unwrap().unwrap(), before, "被拒的只创建不产生新版本");
    assert_eq!(cl.put_new("/f1/new.bin", &body(5_000, 2)).unwrap().attempts, 1);

    // FC02：新版本在途时，stat 报告在途状态，读到的仍是旧版本
    let v2 = body(120_000, 7);
    let h = svc.begin("/f1/a.bin", v2.len() as u64).unwrap();
    let p = cl.stat_path("/f1/a.bin").unwrap().unwrap();
    assert_eq!(p.state, "uploading");
    assert_eq!(p.current.as_ref().map(|s| s.length), Some(v1.len() as u64));
    assert_eq!(cl.get("/f1/a.bin").unwrap(), v1);
    assert!(matches!(cl.put_new("/f1/a.bin", &body(10, 3)), Err(ClientError::Exists(_))));

    // 只在途、没有已提交版本的路径
    let h2 = svc.begin("/f1/fresh.bin", 10).unwrap();
    let p = cl.stat_path("/f1/fresh.bin").unwrap().unwrap();
    assert_eq!((p.state.as_str(), p.current), ("uploading", None));
    assert_eq!(cl.stat("/f1/fresh.bin").unwrap(), None);
    match cl.get("/f1/fresh.bin") {
        Err(ClientError::Rejected { status: 409, body }) => assert!(body.contains("not_committed"), "{body}"),
        other => panic!("{other:?}"),
    }
    match cl.put_new("/f1/fresh.bin", &body(10, 4)) {
        Err(ClientError::Rejected { status: 409, body }) => assert!(body.contains("path_busy"), "{body}"),
        other => panic!("{other:?}"),
    }

    // FC11：按目录列举
    let names = |items: &[tape_rs::client::DirItem]| items.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    let committed_only = cl.list_dir("/f1", false).unwrap();
    assert_eq!(names(&committed_only), vec!["a.bin", "new.bin"]);
    let a = committed_only.iter().find(|e| e.name == "a.bin").unwrap();
    assert_eq!((a.state.as_str(), a.committed_length), ("uploading", Some(v1.len() as u64)), "已提交文件上有新版本在途");
    let with_pending = cl.list_dir("/f1", true).unwrap();
    assert_eq!(names(&with_pending), vec!["a.bin", "fresh.bin", "new.bin"]);
    let root = cl.list_dir("/", false).unwrap();
    assert!(root.iter().any(|e| e.name == "f1" && e.is_dir), "{root:?}");
    assert!(matches!(cl.list_dir("/nope", false), Err(ClientError::NotFound)));

    // 在途版本落带后成为当前版本；放弃的在途路径不留痕迹
    std::fs::write(&h.spool, &v2).unwrap();
    svc.ingest(&h, v2.len() as u64).unwrap();
    svc.finish(h, v2.len() as u64).unwrap();
    svc.abort(h2);
    c.wait("新版本落带", || svc.stat_full("/f1/a.bin").is_ok_and(|p| p.in_flight.is_none() && p.committed.is_some_and(|s| s.len == v2.len() as u64)));
    assert_eq!(cl.get("/f1/a.bin").unwrap(), v2);
    assert_eq!(cl.stat_path("/f1/fresh.bin").unwrap(), None);

    // FC10：Range
    assert_eq!(cl.get_range("/f1/a.bin", 100, 50).unwrap(), v2[100..150]);
    let n = v2.len() as u64;
    assert_eq!(cl.get_range("/f1/a.bin", n - 10, 100).unwrap(), v2[v2.len() - 10..], "越过末尾的部分截掉");
    assert!(matches!(cl.get_range("/f1/a.bin", n, 1), Err(ClientError::Rejected { status: 416, .. })));
    let addr = &endpoints[leader as usize - 1];
    let resp = raw_http(addr, "GET /files/f1/a.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=-5\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 206"), "{}", &resp[..resp.len().min(120)]);
    assert!(resp.contains(&format!("Content-Range: bytes {}-{}/{}", n - 5, n - 1, n)), "{}", &resp[..resp.len().min(200)]);

    // FC09：不在服务的节点答 503，不答 404
    let follower = (1..=3).find(|i| *i != leader).unwrap();
    let resp = raw_http(&endpoints[follower as usize - 1], "GET /stat/f1/a.bin HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 503"), "{}", &resp[..resp.len().min(120)]);
    c.shutdown();
}

// ---------- 跨磁带读取（P4）----------

/// FC06/FC07：删除必须落带才确认；在途路径拒绝，换届后墓碑仍生效，重新上传可以复活。
#[test]
fn file_deletion_is_committed_and_survives_takeover() {
    use tape_rs::client::ClientError;
    let c = Cluster::start_with("f2-delete", false);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = &c.services[&leader];
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints);

    assert!(matches!(cl.delete("/missing"), Err(ClientError::NotFound)));
    let pending = svc.begin("/new", 5).unwrap();
    assert!(matches!(cl.delete("/new"), Err(ClientError::Rejected { status: 409, .. })));
    svc.abort(pending);

    cl.put("/deleted", b"old content").unwrap();
    let pending = svc.begin("/deleted", 5).unwrap();
    assert!(matches!(cl.delete("/deleted"), Err(ClientError::Rejected { status: 409, .. })));
    svc.abort(pending);
    cl.delete("/deleted").unwrap();
    assert_eq!(cl.stat("/deleted").unwrap(), None);
    assert!(matches!(cl.get("/deleted"), Err(ClientError::NotFound)));
    assert!(!cl.list().unwrap().iter().any(|(p, _)| p == "/deleted"));
    assert!(matches!(cl.delete("/deleted"), Err(ClientError::NotFound)));
    assert!(svc.lookup("/deleted").unwrap().unwrap().deleted, "墓碑必须已提交");

    c.isolated.lock().unwrap().insert(leader);
    let (new, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("新执行者开放文件服务", || c.services[&new].serving_round() == Some(next_round));
    assert_eq!(cl.stat("/deleted").unwrap(), None);
    cl.put_new("/deleted", b"new content").unwrap();
    assert_eq!(cl.get("/deleted").unwrap(), b"new content");
    c.shutdown();
}

/// 只有一个驱动器：读不在驱动器里的带要临时换带，读完换回，服务继续。
#[test]
fn reading_from_another_tape_with_a_single_drive_swaps_and_swaps_back() {
    let lib = small_tape_library(64);
    let c = Cluster::start_lib("read-swap", false, lib, 4, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let results = put_until(&svc, "r", 4000, 6, |_| false);
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    c.wait("第一盘转 data_full", || c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "data_full"));
    assert_eq!(svc.tape().unwrap().barcode, "PA0002L8");
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录追上", || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap().list(POOL).map(|l| l.len()).unwrap_or(0) == 6);

    // 写入侧忙着（有已准入未完成的上传）时，读别的带得到"稍后重试"，写入不被打断
    let (p0, _) = &results[0];
    let h = svc.begin("/inflight.bin", 10).unwrap();
    let (tx, rx) = channel();
    c.execs[&leader].send(ExecRequest::Read { path: p0.clone(), reply: tx }).unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Err(tape_rs::daemon::executor::ReadError::Busy(_))));
    svc.abort(h);
    assert_eq!(svc.tape().unwrap().barcode, "PA0002L8", "写入带没被换掉");

    // 写入侧空闲后：目录说它在 PA0001L8；读它要临时换带
    assert_eq!(svc.stat(p0).unwrap().unwrap().barcode, "PA0001L8");
    assert_eq!(c.read_via(leader, p0).unwrap(), body(4000, 0));
    // 读完自动换回写入带，服务继续
    c.wait("换回写入带", || svc.serving_round() == Some(round) && svc.tape().is_some_and(|t| t.barcode == "PA0002L8"));
    assert!(matches!(upload(&svc, "/after-read.bin", &body(100, 9)).unwrap(), TaskStatus::Committed { .. }));
    // 当前带上的文件照常读
    assert_eq!(c.read_via(leader, &results[5].0).unwrap(), body(4000, 5));
    c.shutdown();
}

/// 有两个驱动器：读带装进第二个驱动器并留在那里，再次读不用重新装载；空闲后卸回槽位。
#[test]
fn a_second_drive_serves_reads_and_idle_read_tapes_are_unloaded() {
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i).unwrap();
    }
    let c = Cluster::start_lib("read-drive2", false, lib, 3, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let results = put_until(&svc, "r", 3000, 5, |_| false);
    c.wait("第一盘转 data_full", || c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "data_full"));
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录追上", || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap().list(POOL).map(|l| l.len()).unwrap_or(0) == 5);

    let drives = |lib: &SimLibrary| -> Vec<Option<String>> { (0..2).map(|i| lib.loaded_barcode(i)).collect() };
    let before = drives(&c.lib);
    assert_eq!(c.read_via(leader, &results[0].0).unwrap(), body(3000, 0));
    let during = drives(&c.lib);
    assert!(during.iter().any(|b| b.as_deref() == Some("PA0001L8")), "读带装进了一个驱动器: {during:?}");
    assert!(during.iter().any(|b| b.as_deref() == Some("PA0002L8")), "写入带没被动: {during:?}");
    assert_ne!(before, during);
    // 写入不受影响，再次读同一盘不重新装载
    assert!(matches!(upload(&svc, "/mid.bin", &body(100, 7)).unwrap(), TaskStatus::Committed { .. }));
    assert_eq!(c.read_via(leader, &results[1].0).unwrap(), body(3000, 1));
    assert_eq!(drives(&c.lib), during);
    // 空闲 250 ms 后读带被卸回
    c.wait("读带空闲卸回", || !drives(&c.lib).iter().any(|b| b.as_deref() == Some("PA0001L8")));
    assert!(drives(&c.lib).iter().any(|b| b.as_deref() == Some("PA0002L8")));
    c.shutdown();
}

/// 回收：把一盘带上还活着的文件搬到同池的另一盘带上，然后重新格式化源带。
///
/// 这是整个系统里唯一一条会主动销毁数据的路径，所以断言分两部分：搬走的必须一个不少
/// （内容逐字节对得上，被重写过的路径拿到的是新版本），源带必须真的被格式化干净。
#[test]
fn reclaiming_a_tape_moves_the_live_files_away_and_reformats_it() {
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i).unwrap();
    }
    let c = Cluster::start_lib("reclaim", false, lib, 10, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    assert_eq!(svc.tape().unwrap().barcode, "PA0001L8");

    // 三个文件都落在第一盘上，其中 /r/b.bin 再写一次：旧的那一份成了这盘带上的死字节
    for (i, p) in ["/r/a.bin", "/r/b.bin", "/r/c.bin"].iter().enumerate() {
        assert!(matches!(upload(&svc, p, &body(3000, i as u8)).unwrap(), TaskStatus::Committed { .. }));
    }
    assert!(matches!(upload(&svc, "/r/b.bin", &body(5000, 9)).unwrap(), TaskStatus::Committed { .. }));

    let db = c.dir.join(format!("directory-{}.db", leader));
    let dir = || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
    c.wait("目录追上", || dir().live_on("PA0001L8").unwrap_or((0, 0)) == (4, 3000 + 5000 + 3000));
    let (_, live_before) = dir().live_on("PA0001L8").unwrap();
    let written_before = c.st(leader).tapes["PA0001L8"].bytes_written;
    assert!(
        written_before > live_before,
        "带上实际写掉的字节多于活着的字节，差额就是被重写掉的那一份：写掉 {} 活着 {}",
        written_before,
        live_before
    );

    // 源带正好是当前写入带：回收要先把写入带换掉，再来搬它
    c.admin(Command::TapeReclaim { barcode: "PA0001L8".into() });
    c.wait("回收完成", || {
        c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "appendable" && t.files == 0)
    });

    // 回收途中可以换届：状态在 Raft 里，新执行者会接着搬。所以读之前重新问一次谁在服务
    let (leader, _) = c.wait_serving(None, 0);
    let db = c.dir.join(format!("directory-{}.db", leader));
    let dir = || tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();

    // 三个文件都还在，而且 /r/b.bin 拿到的是重写之后的那一版
    for (i, p) in ["/r/a.bin", "/r/c.bin"].iter().enumerate() {
        let want = body(3000, if i == 0 { 0 } else { 2 });
        assert_eq!(c.read_via(leader, p).unwrap(), want, "{} 的内容", p);
    }
    assert_eq!(c.read_via(leader, "/r/b.bin").unwrap(), body(5000, 9), "被重写过的路径拿到的必须是新版本");

    // 目录里再没有任何一条记录指向源带；三个文件都记在第二盘上
    c.wait("目录清干净", || dir().live_on("PA0001L8").unwrap_or((1, 1)) == (0, 0));
    let d = dir();
    for p in ["/r/a.bin", "/r/b.bin", "/r/c.bin"] {
        assert_eq!(d.stat(POOL, p).unwrap().unwrap().barcode, "PA0002L8", "{}", p);
    }
    // 源带真的被重新格式化了：带上一个文件都没有
    assert!(tape_files(&c.lib, "PA0001L8").is_empty(), "源带应当是空的");
    assert_eq!(tape_files(&c.lib, "PA0002L8").len(), 3);
    c.shutdown();
}

/// FC08：回收搬迁墓碑后，旧版本仍不可见，接管后也不能复活。
#[test]
fn reclaim_preserves_tombstones() {
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PD0001L8", "PD0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i).unwrap();
    }
    let c = Cluster::start_lib("reclaim-delete", false, lib, 100, &["PD0001L8", "PD0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = &c.services[&leader];
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let mut cl = Client::new(c.start_http());
    cl.put("/removed", b"old").unwrap();
    cl.put("/kept", b"live").unwrap();
    cl.delete("/removed").unwrap();
    c.wait("墓碑目录追上", || c.st(leader).tapes["PD0001L8"].generation >= svc.lookup("/removed").unwrap().unwrap().generation);
    c.admin(Command::TapeReclaim { barcode: "PD0001L8".into() });
    c.wait("回收完成", || c.st(leader).tapes["PD0001L8"].files == 0);
    assert_eq!(cl.stat("/removed").unwrap(), None);
    assert_eq!(cl.get("/kept").unwrap(), b"live");
    let tombstone = svc.lookup("/removed").unwrap().unwrap();
    assert!(tombstone.deleted);
    assert_eq!(tombstone.barcode, "PD0002L8");
    assert!(tape_files(&c.lib, "PD0001L8").is_empty());
    c.isolated.lock().unwrap().insert(leader);
    let (new, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("新执行者开放文件服务", || c.services[&new].serving_round() == Some(next_round));
    assert_eq!(cl.stat("/removed").unwrap(), None);
    cl.put_new("/removed", b"revived").unwrap();
    assert_eq!(cl.get("/removed").unwrap(), b"revived");
    c.shutdown();
}

/// 回收需要两台驱动器。只有一台的节点在受理阶段就同步拒绝（EE 的 GLESL154E），
/// 磁带状态不动，也不进日志；服务照常。
#[test]
fn reclaim_is_refused_synchronously_when_the_node_has_one_drive() {
    let lib = SimLibrary::new(1, 4, 1);
    for (i, b) in ["PB0001L8", "PB0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i).unwrap();
    }
    let c = Cluster::start_lib("reclaim1", false, lib, 10, &["PB0001L8", "PB0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    assert!(matches!(upload(&svc, "/s/a.bin", &body(2000, 1)).unwrap(), TaskStatus::Committed { .. }));
    // 上传之后目录条目还在应用：等日志停下来再记基线
    let applied_before = {
        let mut last = c.st(leader).applied_index;
        loop {
            thread::sleep(Duration::from_millis(300));
            let now = c.st(leader).applied_index;
            if now == last {
                break now;
            }
            last = now;
        }
    };

    let why = c.admin_result(Command::TapeReclaim { barcode: "PB0001L8".into() }).unwrap_err();
    assert!(why.contains("两台驱动器"), "{}", why);
    let s = c.st(leader);
    assert_eq!(s.tapes["PB0001L8"].state, "appendable");
    assert_eq!(s.applied_index, applied_before, "被拒绝的命令不占日志");
    assert_eq!(s.drives, 1);
    // 写入没有被换带打断
    assert!(matches!(upload(&svc, "/s/b.bin", &body(2000, 2)).unwrap(), TaskStatus::Committed { .. }));
    assert_eq!(svc.tape().unwrap().barcode, "PB0001L8");
    c.shutdown();
}

/// Leader 放行了、执行线程在设备上才发现干不了：带退回可写并记下原因，
/// 不能永远停在 reclaiming 上（那个状态没有别的出口，而且带还会被继续写）。
/// 同池没有别的可写带时状态机本身也拒绝（EE 的 GLESR211E）。
#[test]
fn a_reclaim_that_cannot_start_falls_back_to_appendable_with_a_reason() {
    let lib = SimLibrary::new(1, 4, 1);
    for (i, b) in ["PC0001L8", "PC0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i).unwrap();
    }
    // 节点声称有两台驱动器，模拟库里只有一台
    let c = Cluster::start_lib_with("reclaim2", false, lib, 10, &["PC0001L8"], Some(2));
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    assert!(matches!(upload(&svc, "/t/a.bin", &body(2000, 1)).unwrap(), TaskStatus::Committed { .. }));

    // 池里只有这一盘带：没有目标，状态机拒绝
    let why = c.admin_result(Command::TapeReclaim { barcode: "PC0001L8".into() }).unwrap_err();
    assert!(why.contains("目标"), "{}", why);
    assert_eq!(c.st(leader).tapes["PC0001L8"].state, "appendable");

    // 有了目标带，受理；执行线程发现只有一台驱动器，放弃并退回可写
    c.admin(Command::TapeAssign { barcode: "PC0002L8".into(), pool: POOL.into() });
    c.admin_result(Command::TapeReclaim { barcode: "PC0001L8".into() }).unwrap();
    c.wait("回收放弃后退回可写", || {
        let s = c.st(leader);
        s.tapes["PC0001L8"].state == "appendable" && s.last_reclaim.as_ref().is_some_and(|r| r.outcome == "abandoned")
    });
    let r = c.st(leader).last_reclaim.unwrap();
    assert_eq!(r.barcode, "PC0001L8");
    assert!(r.detail.contains("两台驱动器"), "{}", r.detail);
    // 内容还在，服务照常
    assert_eq!(c.read_via(leader, "/t/a.bin").unwrap(), body(2000, 1));
    assert!(matches!(upload(&svc, "/t/b.bin", &body(2000, 2)).unwrap(), TaskStatus::Committed { .. }));
    assert!(tape_files(&c.lib, "PC0001L8").contains(&"t/a.bin".to_string()) || !tape_files(&c.lib, "PC0001L8").is_empty(), "源带没有被格式化");
    c.shutdown();
}

/// 对介质副本执行真实恢复，兼容 Full + Incremental，不扰动执行线程的磁头。
fn tape_files(lib: &SimLibrary, barcode: &str) -> Vec<String> {
    let copy = SimLibrary::new(1, 1, 1);
    copy.insert_cartridge(lib.cartridge(barcode).unwrap(), 0).unwrap();
    copy.load_into_drive(barcode, 0).unwrap();
    let dev = copy.drive(0);
    let vol = tape_rs::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
    let mut v = Vec::new();
    vol.index().walk_files(|p, _| v.push(p.to_string()));
    v.sort();
    v
}

/// PN07：目录比磁带还新（不应出现）时，装载后不改目录，把带标为 check 等人核验。
#[test]
fn a_directory_ahead_of_the_tape_marks_the_tape_for_checking() {
    let lib = small_tape_library(64);
    let c = Cluster::start_lib("dir-ahead", false, lib, 3, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let results = put_until(&svc, "r", 3000, 4, |_| false);
    // 代数也要等：换带时执行线程会先往状态里塞一条占位记录，代数是 0
    c.wait("第一盘转 data_full", || {
        c.st(leader).tapes.get("PA0001L8").is_some_and(|t| t.state == "data_full" && t.generation > 0)
    });
    let real_gen = c.st(leader).tapes["PA0001L8"].generation;
    // 伪造一条比磁带更新的目录批次
    use tape_rs::daemon::state::FileRec;
    c.admin(Command::CatalogPart { barcode: "PA0001L8".into(), generation: real_gen + 5, part: 0, files: vec![FileRec { metadata: serde_json::Value::Null, path: "/ghost.bin".into(), length: 1, sha256: String::new(), version: (0, 0), deleted: false }] });
    c.admin(Command::TapeCommitted { barcode: "PA0001L8".into(), volume_uuid: "x".into(), generation: real_gen + 5, files: 4, bytes_used: 1, bytes_written: 1, parts: 1, full: false });
    c.wait("伪造批次已应用", || c.st(leader).tapes["PA0001L8"].generation == real_gen + 5);
    assert!(svc.stat("/ghost.bin").unwrap().is_some());
    // 读第一盘上的真实文件会装载它：发现目录比磁带新
    assert_eq!(c.read_via(leader, &results[0].0).unwrap(), body(3000, 0));
    c.wait("标为 check", || c.st(leader).tapes["PA0001L8"].state == "check");
    assert!(svc.stat("/ghost.bin").unwrap().is_some(), "不自动改目录，等人核验");
    c.shutdown();
}

// ---------- 快照与日志压缩（P5）----------

/// 控制存储里还剩多少条日志、最小的那条是哪个索引。
fn log_range(dir: &std::path::Path, id: u64) -> (u64, u64, u64) {
    let db = rusqlite::Connection::open_with_flags(
        dir.join(format!("raft-{}.db", id)),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = db.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
    let min: i64 = db.query_row("SELECT COALESCE(MIN(idx), 0) FROM entries", [], |r| r.get(0)).unwrap();
    let max: i64 = db.query_row("SELECT COALESCE(MAX(idx), 0) FROM entries", [], |r| r.get(0)).unwrap();
    (count as u64, min as u64, max as u64)
}

/// P5：一个节点掉线期间 Leader 做了快照并压缩掉它还需要的日志；它回来后靠快照追赶，
/// 目录库经节点间通道整份拉回来，之后 stat/list 与 Leader 一致。
#[test]
fn a_lagging_node_catches_up_from_a_snapshot_and_pulls_the_directory() {
    let lib = small_tape_library(64);
    let c = Cluster::start_lib("snapshot", false, lib, 200_000, &["PA0001L8", "PA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let lagging = (1..=3u64).find(|id| *id != leader).unwrap();

    // 先上传几个，确认落后节点此刻是跟上的
    let before = put_until(&svc, "before", 2000, 3, |_| false);
    c.wait("落后节点先跟上", || c.st(lagging).applied_index >= c.st(leader).applied_index);
    let stalled_at = c.st(lagging).applied_index;

    // 断开它，再上传足够多的文件把 Leader 的日志推过快照阈值（32 条）
    c.isolated.lock().unwrap().insert(lagging);
    let after = put_until(&svc, "after", 2000, 30, |_| false);
    assert!(after.iter().all(|(_, r)| matches!(r, Ok(TaskStatus::Committed { .. }))), "{:?}", after);
    c.wait("Leader 压缩掉落后节点还需要的日志", || log_range(&c.dir, leader).1 > stalled_at + 2);
    let (kept, first, last) = log_range(&c.dir, leader);
    let stalled = c.st(lagging).applied_index;
    println!("Leader 日志：{} 条，索引 {}..={}；落后节点停在 {}", kept, first, last, stalled);
    assert!(kept < last, "日志有界：{} 条 < 总索引 {}", kept, last);
    // 它要的下一条已经不在 Leader 的日志里了：接不上，只能靠快照
    assert!(stalled + 1 < first, "落后节点停在 {}，Leader 的日志从 {} 起", stalled, first);

    // 放回来：日志已经接不上了，只能靠快照
    c.isolated.lock().unwrap().clear();
    let all: Vec<&String> = before.iter().chain(after.iter()).map(|(p, _)| p).collect();
    let db = c.dir.join(format!("directory-{}.db", lagging));
    c.wait("落后节点靠快照追上", || c.st(lagging).applied_index >= last);
    c.wait("目录库拉回来了", || {
        tape_rs::daemon::directory::Directory::open_reader(&db).unwrap().list(POOL).map(|l| l.len()).unwrap_or(0) == all.len()
    });

    // 它的目录与 Leader 的逐条一致，而这些记录对应的日志条目它从来没收到过
    let mine = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
    let theirs =
        tape_rs::daemon::directory::Directory::open_reader(&c.dir.join(format!("directory-{}.db", leader))).unwrap();
    for p in &all {
        assert_eq!(mine.stat(POOL, p).unwrap(), theirs.stat(POOL, p).unwrap(), "{p}");
    }
    assert_eq!(mine.list(POOL).unwrap(), theirs.list(POOL).unwrap());
    // 池与磁带归属也从快照恢复了
    let st = c.st(lagging);
    assert_eq!(st.pools.len(), 1);
    assert_eq!(st.tapes.len(), 2);
    assert_eq!(st.tapes["PA0001L8"].generation, c.st(leader).tapes["PA0001L8"].generation, "磁带代数一致");
    // 它自己的日志也已经压缩过：重启不必从头重放
    assert!(log_range(&c.dir, lagging).1 > 1, "落后节点的日志也从快照点开始");
    c.shutdown();
}

/// 计划停机：把队列落带、退带，最后释放全部设备的预留。不释放也是安全的（下一个执行者会抢占），
/// 但设备上会留着一个已经不存在的持有者。
#[test]
fn a_planned_shutdown_commits_the_queue_and_releases_every_reservation() {
    let c = Cluster::start_with("graceful", false);
    let (leader, round) = c.wait_serving(None, 0);
    let svc = c.services[&leader].clone();
    c.wait("文件服务开放", || svc.serving_round() == Some(round));
    let files = put_until(&svc, "g", 4000, 3, |_| false);
    assert!(files.iter().all(|(_, r)| matches!(r, Ok(TaskStatus::Committed { .. }))), "{:?}", files);

    // 普通上传留下标准 DP 增量，IP 保留此前完整索引。
    let live = c.lib.drive_as(0, leader as u32);
    let vol = tape_rs::ltfs::volume::LtfsVolume::mount(&live).unwrap();
    assert!(vol.index().incremental);
    assert!(vol.recovery().ip_debt);
    drop(vol);

    // 停机之前：执行者持有全部设备
    let key = ReservationKey::new(leader as u8, round);
    let drives = c.lib.drive_count();
    for i in 0..drives {
        assert_eq!(read_status(&c.lib.drive_as(i, 99)).unwrap().holder.map(|h| h.0), Some(key.0));
    }
    // 模拟器的换带器不支持 PR（fence 对它返回 Unsupported），所以这里只看驱动器

    for tx in c.inboxes.values() {
        let _ = tx.send(NodeInput::Shutdown);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let clear = || {
        let drives = (0..drives).all(|i| {
            let st = read_status(&c.lib.drive_as(i, 99)).unwrap();
            st.holder.is_none() && st.keys.is_empty()
        });
        drives
    };
    while !clear() {
        assert!(Instant::now() < deadline, "停机后设备上仍有预留或注册");
        thread::sleep(Duration::from_millis(20));
    }

    // 卷仍是完整的：停机前落的带读得回来，下一次接管不必收尾。
    // 停机给驱动器发过 UNLOAD，介质已退出，要先 LOAD 回去才能读
    let dev = c.lib.drive_as(0, 99);
    tape_rs::tape::commands::TapeDrive::new(&dev).load().unwrap();
    let vol = tape_rs::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable(), "停机后卷应当是完整尾部: {:?}", vol.recovery().notes);
    assert!(!vol.index().incremental, "正常停机必须生成 DP 全量检查点");
    assert!(!vol.recovery().ip_debt);
    let ip = &vol.recovery().ip.last_index.as_ref().unwrap().index;
    assert!(!ip.incremental);
    assert_eq!(ip.generation, vol.index().generation);
    assert_eq!(ip.previous_location, Some(vol.index().self_location));

    let on_tape: Vec<String> = vol.list().into_iter().map(|f| f.0).collect();
    for (p, _) in &files {
        assert!(on_tape.iter().any(|t| t.trim_start_matches('/') == p.trim_start_matches('/')), "{p} 不在带上: {on_tape:?}");
    }
    let _ = std::fs::remove_dir_all(&c.dir);
}

/// 下载经过磁带、匿名暂存文件和 HTTP；超过原来的 256 MiB 上限，客户端不缓存全量。
#[test]
fn streaming_download_above_256_mib() {
    use std::io::Write;
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank("LG0001L8", 768 << 20), 0).unwrap();
    let c = Cluster::start_lib("large-download", false, lib, 100, &["LG0001L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || c.services[&leader].serving_round() == Some(round));
    let path = c.dir.join("source.bin");
    let block = body(1 << 20, 37);
    let mut file = std::fs::File::create(&path).unwrap();
    for _ in 0..257 { file.write_all(&block).unwrap(); }
    drop(file);
    let mut cl = Client::new(c.start_http());
    cl.retry_for = Duration::from_secs(180);
    let uploaded = cl.put_file("/large", &path, false, true).unwrap();
    assert_eq!(uploaded.attempts, 1, "等待提交不能重复上传");
    struct CheckedSink { pos: u64, block: Vec<u8> }
    impl Write for CheckedSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            for (i, byte) in data.iter().enumerate() {
                assert_eq!(*byte, self.block[(self.pos as usize + i) % self.block.len()]);
            }
            self.pos += data.len() as u64;
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut sink = CheckedSink { pos: 0, block };
    assert_eq!(cl.get_to("/large", &mut sink).unwrap(), 257 << 20);
    assert_eq!(sink.pos, 257 << 20);
    c.shutdown();
}

#[test]
fn file_metadata_survives_catalog_publication_and_takeover() {
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0).unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(&dev, &MkltfsOptions { volume_id: "META".into(), block_size: 64 * 1024, ..Default::default() }).unwrap();
    let expected_time = {
        let mut vol = tape_rs::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
        vol.append_file_with_xattrs("/imported", &mut std::io::Cursor::new(b"content"), &[("note", "原始属性"), ("tapers.version", "1.5")]).unwrap();
        vol.commit().unwrap();
        vol.index().find_file("imported").unwrap().meta.modify_time.clone()
    };
    let c = Cluster::start_lib("metadata", false, lib, 100, &[BARCODE]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || c.services[&leader].serving_round() == Some(round));
    let mut cl = Client::new(c.start_http());
    let imported = cl.stat("/imported").unwrap().unwrap();
    assert_eq!(imported.version, (1, 5));
    assert_eq!(imported.metadata["modify_time"], expected_time);
    assert!(imported.metadata["xattrs"].as_array().unwrap().iter().any(|x| x["key"] == "note" && x["value"] == "原始属性"));
    cl.put("/new", b"new data").unwrap();
    let written = cl.stat("/new").unwrap().unwrap();
    assert_eq!(written.version.0, round);
    assert!(!written.metadata["modify_time"].as_str().unwrap().is_empty());
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录保存元数据", || {
        let d = tape_rs::daemon::directory::Directory::open_reader(&db).unwrap();
        d.stat(POOL, "/new").unwrap().is_some_and(|r| r.version == written.version && r.metadata == written.metadata)
    });
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后文件服务开放", || c.services[&next].serving_round() == Some(next_round));
    let recovered = cl.stat("/new").unwrap().unwrap();
    assert_eq!(recovered.version, written.version);
    assert_eq!(recovered.metadata, written.metadata);
    assert_eq!(cl.stat("/imported").unwrap().unwrap().metadata, imported.metadata);
    c.shutdown();
}

#[test]
fn reclaim_preserves_file_metadata_with_a_new_version() {
    let lib = SimLibrary::new(2, 6, 1);
    for (slot, barcode) in ["MD0001L8", "MD0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(barcode, 64 << 20), slot).unwrap();
    }
    lib.load_into_drive("MD0001L8", 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(&dev, &MkltfsOptions { volume_id: "META".into(), block_size: 64 * 1024, ..Default::default() }).unwrap();
    {
        let mut vol = tape_rs::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
        vol.append_file_with_xattrs("/keep", &mut std::io::Cursor::new(b"content"), &[("note", "回收前的属性"), ("tapers.version", "1.1")]).unwrap();
        vol.commit().unwrap();
    }
    let c = Cluster::start_lib("reclaim-metadata", false, lib, 100, &["MD0001L8", "MD0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || c.services[&leader].serving_round() == Some(round));
    let mut cl = Client::new(c.start_http());
    let before = cl.stat("/keep").unwrap().unwrap();
    c.admin(Command::TapeReclaim { barcode: "MD0001L8".into() });
    c.wait("元数据回收完成", || c.st(leader).tapes["MD0001L8"].files == 0);
    let after = cl.stat("/keep").unwrap().unwrap();
    assert_eq!(after.barcode, "MD0002L8");
    assert!(after.version > before.version);
    assert_eq!(after.metadata["modify_time"], before.metadata["modify_time"]);
    assert!(after.metadata["xattrs"].as_array().unwrap().iter().any(|x| x["key"] == "note" && x["value"] == "回收前的属性"));
    assert_eq!(after.sha256, before.sha256);
    assert_eq!(cl.get("/keep").unwrap(), b"content");
    assert!(tape_files(&c.lib, "MD0001L8").is_empty());
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后文件服务开放", || c.services[&next].serving_round() == Some(next_round));
    let recovered = cl.stat("/keep").unwrap().unwrap();
    assert_eq!(recovered.metadata, after.metadata);
    assert_eq!(recovered.version, after.version);
    c.shutdown();
}

#[test]
fn overwrites_preserve_xattrs_on_same_and_different_tapes() {
    use tape_rs::ltfs::index::Xattr;
    let lib = SimLibrary::new(2, 6, 1);
    for (slot, barcode) in ["XA0001L8", "XA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(barcode, 64 << 20), slot)
            .unwrap();
    }
    lib.load_into_drive("XA0001L8", 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "XATTR".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    {
        let mut vol = tape_rs::ltfs::volume::LtfsVolume::mount(&dev).unwrap();
        vol.set_hash_policy(tape_rs::ltfs::volume::HashPolicy {
            md5: true,
            sha256: true,
        });
        vol.append_file_with_xattrs(
            "/edit",
            &mut std::io::Cursor::new(b"old"),
            &[("note", "原始属性")],
        )
        .unwrap();
        vol.set_node_xattr(
            "/edit",
            Xattr {
                key: "binary".into(),
                value: "AP8=".into(),
                base64: true,
            },
            false,
            false,
        )
        .unwrap();
        vol.commit().unwrap();
    }
    let c = Cluster::start_lib("overwrite-xattrs", false, lib, 3, &["XA0001L8", "XA0002L8"]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let mut cl = Client::new(c.start_http());
    let before = cl.stat("/edit").unwrap().unwrap();
    let held = c.services[&leader].begin("/edit", 1).unwrap();
    assert!(matches!(
        cl.put("/edit", b"conflict"),
        Err(tape_rs::client::ClientError::Rejected { status: 409, .. })
    ));
    c.services[&leader].abort(held);
    for (data, barcode) in [
        (b"same tape".as_slice(), "XA0001L8"),
        (b"other tape".as_slice(), "XA0002L8"),
    ] {
        assert!(cl.put("/edit", data).unwrap().committed);
        let after = cl.stat("/edit").unwrap().unwrap();
        assert_eq!(after.barcode, barcode);
        let attrs = after.metadata["xattrs"].as_array().unwrap();
        assert!(
            attrs
                .iter()
                .any(|x| x["key"] == "note" && x["value"] == "原始属性")
        );
        assert!(
            attrs
                .iter()
                .any(|x| x["key"] == "binary" && x["value"] == "AP8=" && x["base64"] == true)
        );
        use sha2::Digest;
        assert_eq!(after.sha256, format!("{:x}", sha2::Sha256::digest(data)));
        assert!(attrs.iter().any(|x| x["key"] == "ltfs.hash.md5sum"
            && x["value"] == format!("{:x}", md5::Md5::digest(data))));
        assert_ne!(after.sha256, before.sha256);
        assert_ne!(
            after.metadata["modify_time"],
            before.metadata["modify_time"]
        );
        assert_eq!(cl.get("/edit").unwrap(), data);
        if barcode == "XA0001L8" {
            cl.put("/filler", b"switch tape").unwrap();
            cl.put("/filler2", b"switch tape").unwrap();
        }
    }
    c.shutdown();
}

/// 文件准入必须跨磁带检查类型，接管后仍给客户端明确的拒绝结果。
#[test]
fn namespace_conflicts_across_tapes_and_takeover() {
    use tape_rs::client::ClientError;
    let c = Cluster::start_lib(
        "namespace-conflicts",
        false,
        small_tape_library(64),
        3,
        &["PA0001L8", "PA0002L8"],
    );
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("文件服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let mut cl = Client::new(c.start_http());
    for p in ["/d_%/file", "/d_ax/file", "/filler", "/second"] {
        cl.put(p, b"preserve").unwrap();
    }
    assert_eq!(cl.stat("/d_%/file").unwrap().unwrap().barcode, "PA0001L8");
    assert_eq!(cl.stat("/second").unwrap().unwrap().barcode, "PA0002L8");
    let check = |cl: &mut Client| {
        for (path, error) in [
            ("/d_%", "is_directory"),
            ("/d_%/file/child", "not_directory"),
        ] {
            match cl.put(path, b"rejected") {
                Err(ClientError::Rejected { status: 409, body }) => {
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
                        error
                    );
                }
                other => panic!("路径 {path} 应被拒绝: {other:?}"),
            }
        }
        assert!(matches!(
            cl.delete("/d_%"),
            Err(ClientError::Rejected { status: 409, .. })
        ));
        assert_eq!(cl.get("/d_%/file").unwrap(), b"preserve");
    };
    check(&mut cl);
    // /d 只是文本前缀，不是 /d_% 的父路径。
    cl.put("/d", b"independent").unwrap();
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后文件服务开放", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    check(&mut cl);
    assert_eq!(cl.get("/d").unwrap(), b"independent");
    c.shutdown();
}

#[test]
fn persistent_directories_survive_fuse_remount_and_takeover() {
    use tape_rs::client::ClientError;
    use tape_rs::tapefs::{ClientBackend, ROOT_INO, TapeFs};
    let c = Cluster::start_with("persistent-directories", false);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("目录服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());
    cl.mkdir("/empty").unwrap();
    let before = cl.stat("/empty").unwrap().unwrap();
    assert_eq!(before.metadata["kind"], "directory");
    assert!(cl.list_dir("/empty", false).unwrap().is_empty());
    assert!(matches!(
        cl.get("/empty"),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    assert!(matches!(cl.mkdir("/empty"), Err(ClientError::Exists(_))));
    assert!(matches!(
        cl.mkdir("/missing/child"),
        Err(ClientError::NotFound)
    ));
    cl.mkdir("/empty/child").unwrap();
    assert!(matches!(
        cl.rmdir("/empty"),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    let held = c.services[&leader].begin("/empty/child/file", 1).unwrap();
    assert!(matches!(
        cl.rmdir("/empty/child"),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    c.services[&leader].abort(held);
    cl.rmdir("/empty/child").unwrap();
    // The same path may safely change kind after rmdir.
    cl.put("/empty/child", b"file after directory").unwrap();
    assert!(matches!(
        cl.rmdir("/empty/child"),
        Err(ClientError::Rejected { status: 409, .. })
    ));
    cl.delete("/empty/child").unwrap();
    assert!(cl.list_dir("/empty", false).unwrap().is_empty());
    let fs = TapeFs::new(
        ClientBackend::new(Client::new(endpoints.clone())),
        c.dir.join("fuse-cache"),
    )
    .unwrap();
    let made = fs.mkdir(ROOT_INO, "fused").unwrap();
    assert!(made.is_dir);
    drop(fs);
    let fs = TapeFs::new(
        ClientBackend::new(Client::new(endpoints.clone())),
        c.dir.join("fuse-cache"),
    )
    .unwrap();
    assert!(fs.lookup(ROOT_INO, "fused").unwrap().is_dir);
    fs.rmdir(ROOT_INO, "fused").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "fused").err(), Some(nix::libc::ENOENT));
    drop(fs);
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后目录服务开放", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    assert!(cl.list_dir("/empty", false).unwrap().is_empty());
    assert_eq!(
        cl.stat("/empty").unwrap().unwrap().metadata["node"]["creation_time"],
        before.metadata["node"]["creation_time"]
    );
    assert!(cl.stat("/fused").unwrap().is_none());
    assert!(cl.stat("/empty/child").unwrap().is_none());
    cl.rmdir("/empty").unwrap();
    cl.mkdir("/empty").unwrap();
    assert!(cl.list_dir("/empty", false).unwrap().is_empty());
    c.shutdown();
}

#[test]
fn reclaim_preserves_native_empty_directories_and_their_metadata() {
    use tape_rs::ltfs::{index::Xattr, volume::LtfsVolume};
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i)
            .unwrap();
    }
    lib.load_into_drive("PA0001L8", 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "DIRS".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.create_directory("/native").unwrap();
        vol.create_directory("/native/empty").unwrap();
        vol.set_node_xattr(
            "/native/empty",
            Xattr {
                key: "binary".into(),
                value: "AP8=".into(),
                base64: true,
            },
            false,
            false,
        )
        .unwrap();
        vol.commit().unwrap();
    }
    let c = Cluster::start_lib(
        "reclaim-directories",
        false,
        lib,
        100,
        &["PA0001L8", "PA0002L8"],
    );
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("目录服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let mut cl = Client::new(c.start_http());
    let before = cl.stat("/native/empty").unwrap().unwrap();
    cl.mkdir("/removed").unwrap();
    cl.rmdir("/removed").unwrap();
    c.admin(Command::TapeReclaim {
        barcode: "PA0001L8".into(),
    });
    c.wait("含空目录的磁带回收完成", || {
        c.st(leader)
            .last_reclaim
            .as_ref()
            .is_some_and(|r| r.barcode == "PA0001L8" && r.outcome == "done")
    });
    let after = cl.stat("/native/empty").unwrap().unwrap();
    assert_eq!(after.barcode, "PA0002L8");
    assert_eq!(after.metadata["node"], before.metadata["node"]);
    assert!(
        after.metadata["xattrs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["key"] == "binary" && x["value"] == "AP8=" && x["base64"] == true)
    );
    assert!(cl.list_dir("/native/empty", false).unwrap().is_empty());
    assert!(cl.stat("/removed").unwrap().is_none());
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管完成", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    assert!(cl.list_dir("/native/empty", false).unwrap().is_empty());
    assert!(cl.stat("/removed").unwrap().is_none());
    c.shutdown();
}

#[test]
fn directory_tombstones_survive_reconciling_an_old_tape() {
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i)
            .unwrap();
    }
    let c = Cluster::start_lib(
        "directory-old-tape",
        false,
        lib,
        2,
        &["PA0001L8", "PA0002L8"],
    );
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("目录服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let mut cl = Client::new(c.start_http());
    cl.mkdir("/gone").unwrap();
    cl.put("/first", b"old tape").unwrap();
    cl.put("/filler", b"fill first tape").unwrap();
    cl.put("/second", b"new tape").unwrap();
    assert_eq!(cl.stat("/second").unwrap().unwrap().barcode, "PA0002L8");
    cl.rmdir("/gone").unwrap();
    // Loading the old tape publishes its still-existing native directory again.
    assert_eq!(cl.get("/first").unwrap(), b"old tape");
    assert!(cl.stat("/gone").unwrap().is_none());
    let db = c.dir.join(format!("directory-{}.db", leader));
    c.wait("目录删除墓碑进入 Raft", || {
        tape_rs::daemon::directory::Directory::open_reader(&db)
            .unwrap()
            .lookup(POOL, "/gone")
            .unwrap()
            .is_some_and(|r| r.deleted && r.barcode == "PA0002L8")
    });
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后仍保留墓碑", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    assert!(cl.stat("/gone").unwrap().is_none());
    cl.mkdir("/gone").unwrap();
    assert!(cl.list_dir("/gone", false).unwrap().is_empty());
    c.shutdown();
}

#[test]
fn atomic_rename_preserves_native_nodes_and_survives_takeover() {
    use tape_rs::ltfs::volume::LtfsVolume;
    let lib = SimLibrary::new(1, 4, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "RENAME".into(),
            block_size: 65536,
            ..Default::default()
        },
    )
    .unwrap();
    let original = {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.create_directory("/source").unwrap();
        vol.create_directory("/source/empty").unwrap();
        vol.append_file_with_xattrs(
            "/source/file",
            &mut std::io::Cursor::new(b"renamed content"),
            &[("note", "保留属性")],
        )
        .unwrap();
        vol.commit().unwrap();
        vol.index().find_file("source/file").unwrap().clone()
    };
    let c = Cluster::start_lib("atomic-rename", false, lib, 100, &[BARCODE]);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());
    cl.mkdir("/target").unwrap();
    cl.rename("/source", "/target", false).unwrap();
    assert!(cl.stat("/source").unwrap().is_none());
    assert!(cl.stat("/source/file").unwrap().is_none());
    assert_eq!(cl.get("/target/file").unwrap(), b"renamed content");
    assert!(cl.list_dir("/target/empty", false).unwrap().is_empty());
    cl.rename("/target/file", "/target/% ?文件", true).unwrap();
    cl.put("/replace", b"old target").unwrap();
    cl.rename("/target/% ?文件", "/replace", false).unwrap();
    c.isolated.lock().unwrap().insert(leader);
    let (next, nr) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管开放", || {
        c.services[&next].serving_round() == Some(nr)
    });
    // 保留旧 Leader 缓存，读取应在设备隔离后切换到新执行者。
    assert_eq!(cl.get("/replace").unwrap(), b"renamed content");
    assert!(cl.stat("/source/empty").unwrap().is_none());
    assert!(cl.stat("/target/% ?文件").unwrap().is_none());
    assert_eq!(cl.get("/replace").unwrap(), b"renamed content");
    let copy = SimLibrary::new(1, 1, 1);
    copy.insert_cartridge(c.lib.cartridge(BARCODE).unwrap(), 0)
        .unwrap();
    copy.load_into_drive(BARCODE, 0).unwrap();
    let dev = copy.drive(0);
    let vol = LtfsVolume::mount(&dev).unwrap();
    let renamed = vol.index().find_file("replace").unwrap();
    assert_eq!(renamed.meta.file_uid, original.meta.file_uid);
    assert_eq!(renamed.extents, original.extents);
    assert_eq!(renamed.meta.modify_time, original.meta.modify_time);
    assert_eq!(renamed.xattr("note"), Some("保留属性"));
    assert!(vol.index().find_directory("source").is_none());
    c.shutdown();
}

#[test]
fn client_read_retries_after_executor_reports_preemption() {
    use tape_rs::scsi::reservation::{ReservationKey, fence};
    let c = Cluster::start_with("read-preempt", false);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let mut cl = Client::new(c.start_http());
    cl.put("/read-preempt", b"complete response only").unwrap();
    assert_eq!(cl.get("/read-preempt").unwrap(), b"complete response only");
    fence(&c.lib.drive_as(0, 99), ReservationKey::new(99, round)).unwrap();
    let mut out = Vec::new();
    assert_eq!(cl.get_to("/read-preempt", &mut out).unwrap(), 22);
    assert_eq!(out, b"complete response only");
    let (next, _) = c.wait_serving(Some(leader), round + 1);
    assert_ne!(next, leader);
}

#[test]
fn user_xattrs_commit_without_rewriting_data_and_survive_takeover() {
    use nix::libc;
    use tape_rs::ltfs::volume::LtfsVolume;
    use tape_rs::tapefs::{ClientBackend, TapeFs};
    let c = Cluster::start_with("user-xattrs", false);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());
    cl.put("/file", b"unchanged content").unwrap();
    cl.mkdir("/dir").unwrap();
    let native = || {
        let copy = SimLibrary::new(1, 1, 1);
        copy.insert_cartridge(c.lib.cartridge(BARCODE).unwrap(), 0)
            .unwrap();
        copy.load_into_drive(BARCODE, 0).unwrap();
        let dev = copy.drive(0);
        let vol = LtfsVolume::mount(&dev).unwrap();
        vol.index().find_file("file").unwrap().clone()
    };
    let original = native();
    cl.setxattr("/file", "user.binary", &[0, 255, 0xe4, 0xb8, 0xad], 1)
        .unwrap();
    assert!(matches!(
        cl.setxattr("/file", "user.binary", b"other", 1),
        Err(tape_rs::client::ClientError::Exists(_))
    ));
    let missing = cl.setxattr("/file", "user.missing", b"x", 2).unwrap_err();
    assert_eq!(tape_rs::tapefs::errno_of(&missing), libc::ENODATA);
    cl.setxattr("/file", "user.binary", &[0, 255, 42], 2)
        .unwrap();
    cl.setxattr("/file", "user.empty", b"", 0).unwrap();
    cl.setxattr("/dir", "user.directory", b"directory\0\xff", 0)
        .unwrap();
    let large = vec![0xff; 65536];
    cl.setxattr("/file", "user.large", &large, 0).unwrap();
    let current = native();
    assert_eq!(current.extents, original.extents);
    assert_eq!(current.meta.file_uid, original.meta.file_uid);
    assert_eq!(current.meta.modify_time, original.meta.modify_time);
    assert_eq!(
        current.xattr("ltfs.hash.sha256sum"),
        original.xattr("ltfs.hash.sha256sum")
    );
    let t = TapeFs::new(ClientBackend::new(cl.clone()), c.dir.join("xattr-cache")).unwrap();
    let file = t.lookup(1, "file").unwrap();
    let directory = t.lookup(1, "dir").unwrap();
    assert_eq!(t.getxattr(file.ino, "user.binary").unwrap(), [0, 255, 42]);
    assert_eq!(t.getxattr(file.ino, "user.large").unwrap(), large);
    assert!(
        t.listxattr(directory.ino)
            .unwrap()
            .split(|b| *b == 0)
            .any(|n| n == b"user.directory")
    );
    // 打开writer的脏内容先提交；后续内容覆盖仍保留属性。
    let fh = t.open(file.ino, libc::O_RDWR).unwrap();
    t.write(fh, 0, b"UPDATED").unwrap();
    t.change_xattr(file.ino, "user.writer", Some(b"writer\0"), 0)
        .unwrap();
    t.write(fh, 0, b"REWRITE").unwrap();
    t.fsync(fh).unwrap();
    t.release(fh);
    assert_eq!(t.getxattr(file.ino, "user.writer").unwrap(), b"writer\0");
    t.change_xattr(directory.ino, "user.directory", None, 0)
        .unwrap();
    assert_eq!(
        t.getxattr(directory.ino, "user.directory"),
        Err(libc::ENODATA)
    );
    cl.removexattr("/file", "user.binary").unwrap();
    assert_eq!(t.getxattr(file.ino, "user.binary"), Err(libc::ENODATA));
    c.isolated.lock().unwrap().insert(leader);
    let (next, nr) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管开放", || {
        c.services[&next].serving_round() == Some(nr)
    });
    let t = TapeFs::new(
        ClientBackend::new(Client::new(endpoints)),
        c.dir.join("xattr-remount"),
    )
    .unwrap();
    let file = t.lookup(1, "file").unwrap();
    assert_eq!(t.getxattr(file.ino, "user.writer").unwrap(), b"writer\0");
    assert_eq!(t.getxattr(file.ino, "user.empty").unwrap(), b"");
    assert_eq!(t.getxattr(file.ino, "user.binary"), Err(libc::ENODATA));
}

#[test]
fn symlinks_commit_and_survive_remount_and_takeover() {
    use tape_rs::client::ClientError;
    use tape_rs::tapefs::{ClientBackend, ROOT_INO, TapeFs};
    let c = Cluster::start_with("symlink-create", false);
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("符号链接服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());
    cl.put("/target", b"target unchanged").unwrap();
    let before = cl.stat("/target").unwrap().unwrap();
    for (path, target) in [
        ("/link", "target"),
        ("/dangling", "../missing & 中文%?#"),
        ("/absolute", "/target"),
        ("/loop", "loop"),
    ] {
        cl.symlink(target, path).unwrap();
        let st = cl.stat(path).unwrap().unwrap();
        assert_eq!(st.length, 0);
        assert_eq!(st.metadata["kind"], "symlink");
        assert_eq!(st.metadata["symlink"], target);
        assert!(matches!(
            cl.symlink("different", path),
            Err(ClientError::Exists(_))
        ));
    }
    let fs = TapeFs::new(
        ClientBackend::new(Client::new(endpoints.clone())),
        c.dir.join("link-cache"),
    )
    .unwrap();
    let made = fs.symlink(ROOT_INO, "fused", "target").unwrap();
    assert!(made.is_symlink);
    assert_eq!(made.size, 6);
    assert_eq!(fs.readlink(made.ino).unwrap(), b"target");
    drop(fs);
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("接管后链接服务开放", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    let fs = TapeFs::new(
        ClientBackend::new(Client::new(endpoints)),
        c.dir.join("link-cache-new"),
    )
    .unwrap();
    let link = fs.lookup(ROOT_INO, "fused").unwrap();
    assert!(link.is_symlink);
    assert_eq!(fs.readlink(link.ino).unwrap(), b"target");
    fs.rename(ROOT_INO, "fused", ROOT_INO, "renamed", false)
        .unwrap();
    assert_eq!(
        fs.readlink(fs.lookup(ROOT_INO, "renamed").unwrap().ino)
            .unwrap(),
        b"target"
    );
    fs.unlink(ROOT_INO, "renamed").unwrap();
    assert_eq!(
        fs.lookup(ROOT_INO, "renamed").err(),
        Some(nix::libc::ENOENT)
    );
    fs.symlink(ROOT_INO, "renamed", "dangling").unwrap();
    assert_eq!(
        cl.stat("/dangling").unwrap().unwrap().metadata["symlink"],
        "../missing & 中文%?#"
    );
    let after = cl.stat("/target").unwrap().unwrap();
    assert_eq!(before.version, after.version);
    assert_eq!(before.metadata, after.metadata);
    assert_eq!(cl.get("/target").unwrap(), b"target unchanged");
    c.shutdown();
}

#[test]
fn reclaim_preserves_symlinks_without_resolving_targets() {
    use tape_rs::ltfs::{index::Xattr, volume::LtfsVolume};
    use tape_rs::tapefs::{ClientBackend, ROOT_INO, TapeFs};
    let lib = SimLibrary::new(2, 6, 1);
    for (i, b) in ["PA0001L8", "PA0002L8"].iter().enumerate() {
        lib.insert_cartridge(SimCartridge::blank(b, 64 << 20), i)
            .unwrap();
    }
    lib.load_into_drive("PA0001L8", 0).unwrap();
    let dev = lib.drive_as(0, 50);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "LINKS".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let links = [
        ("link", "target"),
        ("dangling", "missing"),
        ("loop", "loop"),
        ("absolute", "/target"),
        ("chain", "link"),
        ("directory", "dir"),
    ];
    let original;
    {
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_file("/target", &mut &b"target content"[..])
            .unwrap();
        vol.create_directory("/dir").unwrap();
        for (name, target) in links {
            vol.add_symlink(&format!("/{name}"), target).unwrap();
            vol.set_node_xattr(
                &format!("/{name}"),
                Xattr {
                    key: "binary".into(),
                    value: "AP8=".into(),
                    base64: true,
                },
                false,
                false,
            )
            .unwrap();
        }
        vol.set_node_readonly("/dangling", true).unwrap();
        vol.commit().unwrap();
        original = vol.index().clone();
    }
    let c = Cluster::start_lib(
        "reclaim-symlinks",
        false,
        lib,
        100,
        &["PA0001L8", "PA0002L8"],
    );
    let (leader, round) = c.wait_serving(None, 0);
    c.wait("链接服务开放", || {
        c.services[&leader].serving_round() == Some(round)
    });
    let endpoints = c.start_http();
    let mut cl = Client::new(endpoints.clone());
    c.admin(Command::TapeReclaim {
        barcode: "PA0001L8".into(),
    });
    c.wait("链接回收结束", || c.st(leader).last_reclaim.is_some());
    let outcome = c.st(leader).last_reclaim.unwrap();
    assert_eq!(outcome.outcome, "done", "{outcome:?}");
    for (name, target) in links {
        let st = cl.stat(&format!("/{name}")).unwrap().unwrap();
        assert_eq!(st.barcode, "PA0002L8");
        assert_eq!(st.length, 0);
        assert_eq!(st.metadata["kind"], "symlink");
        assert_eq!(st.metadata["symlink"], target);
        assert_eq!(
            st.metadata["modify_time"],
            original.find_file(name).unwrap().meta.modify_time
        );
        assert!(
            st.metadata["xattrs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["key"] == "binary" && x["value"] == "AP8=" && x["base64"] == true)
        );
    }
    // Inspect the actual destination index: all timestamps/readonly/xattrs survive,
    // link nodes have no data extents and newly allocated UIDs are unique.
    let cart = c.lib.cartridge("PA0002L8").unwrap();
    let detached = SimLibrary::new(1, 2, 0);
    detached.insert_cartridge(cart, 0).unwrap();
    detached.load_into_drive("PA0002L8", 0).unwrap();
    let detached_drive = detached.drive(0);
    let recovered = LtfsVolume::mount(&detached_drive).unwrap();
    let moved = recovered.index();
    let mut uids = std::collections::HashSet::new();
    moved.walk_files(|_, f| {
        assert!(uids.insert(f.meta.file_uid));
    });
    moved.walk_directories(|_, d| {
        assert!(uids.insert(d.meta.file_uid));
    });
    for (name, _) in links {
        let old = original.find_file(name).unwrap();
        let node = moved.find_file(name).unwrap();
        let mut expected = old.meta.clone();
        expected.file_uid = node.meta.file_uid;
        assert_eq!(node.meta, expected);
        assert!(node.extents.is_empty());
        assert_eq!(node.symlink, old.symlink);
        for attr in &old.xattrs {
            assert!(node.xattrs.contains(attr));
        }
    }
    c.isolated.lock().unwrap().insert(leader);
    let (next, next_round) = c.wait_serving(Some(leader), round + 1);
    c.wait("链接回收后接管", || {
        c.services[&next].serving_round() == Some(next_round)
    });
    let fs = TapeFs::new(
        ClientBackend::new(Client::new(endpoints)),
        c.dir.join("link-reclaim-cache"),
    )
    .unwrap();
    for (name, target) in links {
        let attr = fs.lookup(ROOT_INO, name).unwrap();
        assert!(attr.is_symlink);
        assert_eq!(fs.readlink(attr.ino).unwrap(), target.as_bytes());
    }
    assert_eq!(cl.get("/target").unwrap(), b"target content");
    c.shutdown();
}
