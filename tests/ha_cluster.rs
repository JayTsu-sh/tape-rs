//! 通过真实 Raft 循环、执行线程与公开状态观测控制/执行分离；设备为内存模拟器。
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use raft::eraftpb::Message;
use tape_rs::daemon::executor::{DeviceKind, DeviceProvider, ExecOptions, ManagedDevice};
use tape_rs::daemon::{
    executor,
    files::FileService,
    net::{Network, NodeInput},
    node::{self, CoordinationConfig, NodeConfig, NodeStatus, SharedStatus},
    store::{RaftStore, SnapshotPolicy},
};
use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::sim::SimLibrary;

struct Net {
    me: u64,
    peers: Arc<HashMap<u64, mpsc::Sender<NodeInput>>>,
    isolated: Arc<Mutex<HashSet<u64>>>,
}
impl Network for Net {
    fn send(&self, msg: Message) {
        let isolated = self.isolated.lock().unwrap();
        if isolated.contains(&self.me) || isolated.contains(&msg.to) {
            return;
        }
        if let Some(tx) = self.peers.get(&msg.to) {
            let _ = tx.send(NodeInput::Raft(Box::new(msg)));
        }
    }
}
struct Provider {
    lib: SimLibrary,
    id: u64,
    fc: Arc<AtomicBool>,
    slow_checkpoint: Arc<AtomicBool>,
    removed_path: Arc<AtomicBool>,
}

struct SlowCheckpoint {
    inner: tape_rs::scsi::sim::SimTransport,
    delay: Arc<AtomicBool>,
    removed_path: Arc<AtomicBool>,
    opened_before_removal: bool,
}

impl tape_rs::scsi::transport::TapeTransport for SlowCheckpoint {
    fn execute_no_data(
        &self,
        cdb: &[u8],
        timeout: u32,
    ) -> Result<tape_rs::scsi::device::ScsiResult> {
        if cdb.first() == Some(&0x92) && self.delay.swap(false, Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1500));
        }
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        buf: &mut [u8],
        timeout: u32,
    ) -> Result<tape_rs::scsi::device::ScsiResult> {
        if self.opened_before_removal && self.removed_path.load(Ordering::SeqCst) {
            return Err(nix::errno::Errno::ENODEV.into());
        }
        self.inner.execute_read(cdb, buf, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        buf: &[u8],
        timeout: u32,
    ) -> Result<tape_rs::scsi::device::ScsiResult> {
        self.inner.execute_write(cdb, buf, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        if !self.fc.load(Ordering::SeqCst) {
            return Err(TapeError::NotReady("测试节点无 FC".into()));
        }
        Ok(vec![
            ManagedDevice {
                name: "测试驱动".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: Box::new(SlowCheckpoint {
                    inner: self.lib.drive_as(0, self.id as u32),
                    delay: self.slow_checkpoint.clone(),
                    opened_before_removal: !self.removed_path.load(Ordering::SeqCst),
                    removed_path: self.removed_path.clone(),
                }),
            },
            ManagedDevice {
                name: "测试机械手".into(),
                serial: "SIMLIB0000000001".into(),
                kind: DeviceKind::Changer,
                dev: Box::new(self.lib.changer_as(self.id as u32)),
            },
        ])
    }
}

#[test]
fn control_only_c_leads_while_a_or_b_owns_devices() {
    scenario(0);
}

#[test]
fn a_control_leader_with_new_fc_access_takes_over_from_a_healthy_owner() {
    scenario(1);
}

#[test]
fn partitioned_owner_loses_permission_and_the_other_fc_node_takes_over() {
    scenario(2);
}

#[test]
fn losing_the_control_leader_aligns_the_owner_with_the_new_fc_leader() {
    scenario(3);
}

#[test]
fn committed_data_survives_follower_owner_failover() {
    scenario(4);
}

#[test]
fn partial_fencing_never_publishes_an_executor_when_changer_pr_is_unsupported() {
    scenario(5);
}

#[test]
fn planned_shutdown_renews_permission_until_a_slow_checkpoint_and_release_finish() {
    scenario(6);
}

#[test]
fn planned_shutdown_cannot_renew_permission_after_losing_the_majority() {
    scenario(7);
}

#[test]
fn a_control_leader_owner_can_finish_a_slow_checkpoint_during_shutdown() {
    scenario(8);
}

#[test]
fn an_unusable_active_path_yields_even_when_a_fresh_probe_finds_an_alternate_path() {
    scenario(9);
}

#[test]
fn strict_vip_waits_for_old_follower_owner_withdrawal_across_a_raft_partition() {
    scenario(10);
}

#[test]
fn transferring_control_to_an_fc_follower_moves_ownership_and_preserves_committed_data() {
    scenario(11);
}

fn scenario(case: u8) {
    let dir = std::env::temp_dir().join(format!("ha-coordinator-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let ids = [1, 2, 3];
    let mut txs = HashMap::new();
    let mut rxs = HashMap::new();
    for id in ids {
        let (tx, rx) = mpsc::channel();
        txs.insert(id, tx);
        rxs.insert(id, rx);
    }
    let peers = Arc::new(txs);
    let lib = SimLibrary::new(1, 2, 0);
    if case != 5 {
        lib.enable_changer_pr();
    }
    if case == 1 || case == 4 || case >= 6 {
        lib.insert_cartridge(
            tape_rs::scsi::sim::SimCartridge::blank("HATESTL8", 256 << 20),
            0,
        )
        .unwrap();
    }
    let fc: HashMap<_, _> = ids
        .into_iter()
        .map(|id| {
            (
                id,
                Arc::new(AtomicBool::new(if case == 8 {
                    id == 3
                } else {
                    id != 3 && (case < 6 || id == 1)
                })),
            )
        })
        .collect();
    let slow_checkpoint = Arc::new(AtomicBool::new(false));
    let removed_paths: HashMap<_, _> = ids
        .into_iter()
        .map(|id| (id, Arc::new(AtomicBool::new(false))))
        .collect();
    let isolated = Arc::new(Mutex::new(HashSet::new()));
    let mut statuses: HashMap<u64, SharedStatus> = HashMap::new();
    let mut services = HashMap::new();
    let mut execs = HashMap::new();
    let mut joins = Vec::new();
    for id in ids {
        let status = Arc::new(Mutex::new(NodeStatus::default()));
        statuses.insert(id, status.clone());
        let (exec_tx, exec_rx) = mpsc::channel();
        execs.insert(id, exec_tx.clone());
        let (ev_tx, ev_rx) = mpsc::channel();
        let provider = Box::new(Provider {
            lib: lib.clone(),
            id,
            fc: fc[&id].clone(),
            slow_checkpoint: slow_checkpoint.clone(),
            removed_path: removed_paths[&id].clone(),
        });
        let directory = dir.join(format!("directory-{id}.db"));
        let files = FileService::with_options(
            dir.join(format!("spool-{id}")),
            tape_rs::daemon::files::BatchPolicy {
                max_bytes: 1 << 20,
                max_files: 10,
                idle: Duration::from_millis(25),
                max_wait: Duration::from_millis(200),
            },
            Some(directory.clone()),
        )
        .unwrap();
        files.set_executor(exec_tx.clone());
        services.insert(id, files.clone());
        let ex_status = status.clone();
        let exec_join = std::thread::spawn(move || {
            executor::run(
                provider,
                ExecOptions {
                    node_id: id as u8,
                    interval: Duration::from_millis(40),
                    demo_write: false,
                    salvage: false,
                    block_size: 65536,
                    read_idle: Duration::from_secs(1),
                },
                exec_rx,
                ev_tx,
                files,
                ex_status,
            )
        });
        let tx = peers[&id].clone();
        let events = std::thread::spawn(move || {
            for ev in ev_rx {
                if tx.send(NodeInput::Exec(ev)).is_err() {
                    break;
                }
            }
        });
        let cfg = NodeConfig {
            id,
            peers: ids.to_vec(),
            tick: Duration::from_millis(20),
            election_ticks: if id == 3 { 10 } else { 40 },
            heartbeat_ticks: 3,
            status_file: None,
            directory_file: Some(directory),
            cooldown: Duration::from_millis(500),
            snapshot: SnapshotPolicy::default(),
            shutdown_grace: Duration::from_secs(if case >= 6 { 4 } else { 2 }),
            drives: 1,
            coordination: Some(CoordinationConfig {
                group: "lab".into(),
                validity: if case >= 6 {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(2)
                },
            }),
        };
        let store = RaftStore::open(&dir.join(format!("raft-{id}.db")), &ids).unwrap();
        let net = Box::new(Net {
            me: id,
            peers: peers.clone(),
            isolated: isolated.clone(),
        });
        let inbox = rxs.remove(&id).unwrap();
        let node_join =
            std::thread::spawn(move || node::run(cfg, store, net, inbox, exec_tx, status));
        joins.push((node_join, exec_join, events));
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut success = false;
    while Instant::now() < deadline {
        let c = statuses[&3].lock().unwrap().clone();
        if case == 5 {
            assert!(!c.executor_ready, "只隔离了驱动器，不得对外发布");
            if statuses
                .values()
                .any(|s| s.lock().unwrap().local.contains("不支持持久预留"))
            {
                success = true;
                break;
            }
        }
        if c.leader == 3
            && c.executor
                .as_ref()
                .is_some_and(|e| (e.node != 3 || case == 8) && e.fenced)
            && c.executor_ready
        {
            success = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = statuses[&3].lock().unwrap().executor.clone();
    if success && case == 0 {
        use std::io::{Read, Write};
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap().to_string();
        drop(socket);
        let owner = first.as_ref().unwrap().node;
        let ctx = Arc::new(tape_rs::daemon::http::HttpContext {
            files: services[&3].clone(),
            status: statuses[&3].clone(),
            exec: Mutex::new(execs[&3].clone()),
            node: Mutex::new(peers[&3].clone()),
            client_addrs: HashMap::from([(owner, "127.0.0.1:18002".into()), (3, addr.clone())]),
            wait_timeout: Duration::from_secs(2),
        });
        tape_rs::daemon::http::serve(&addr, ctx).unwrap();
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"GET /stat/ha.bin HTTP/1.1\r\nHost: test\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
        assert_eq!(body["leader"], 3);
        assert_eq!(body["owner"], owner);
        assert_eq!(body["leader_url"], "http://127.0.0.1:18002");
        fn ready(addr: &str) -> bool {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .write_all(b"GET /readyz HTTP/1.1\r\nHost: test\r\n\r\n")
                .unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply.starts_with("HTTP/1.1 200")
        }
        assert!(!ready(&addr), "无设备的控制 Leader 不具备 VIP 资格");
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let owner_addr = socket.local_addr().unwrap().to_string();
        drop(socket);
        tape_rs::daemon::http::serve(
            &owner_addr,
            Arc::new(tape_rs::daemon::http::HttpContext {
                files: services[&owner].clone(),
                status: statuses[&owner].clone(),
                exec: Mutex::new(execs[&owner].clone()),
                node: Mutex::new(peers[&owner].clone()),
                client_addrs: HashMap::new(),
                wait_timeout: Duration::from_secs(2),
            }),
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while !ready(&owner_addr) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ready(&owner_addr),
            "真实 Raft 许可发布后 follower owner 应具备 VIP 资格"
        );
        assert!(
            services[&owner].serving_round().is_none(),
            "尚未分配磁带也必须保留 HTTP 入口"
        );
    }
    if success && case == 10 {
        use tape_rs::daemon::state::Command;
        let old = first.as_ref().unwrap();
        let address = "192.0.2.100".parse().unwrap();
        let wait_vip = |node: u64, round: u64| {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let record = statuses[&3].lock().unwrap().vips.get(&address).cloned();
                if let Some(record) = record
                    && record
                        .holder
                        .as_ref()
                        .is_some_and(|h| h.node == node && h.round == round)
                {
                    return record.token;
                }
                assert!(Instant::now() < until, "VIP 预约必须经过真实 Raft 提交");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        peers[&old.node]
            .send(NodeInput::Vip(Command::VipAcquire {
                address,
                node: old.node,
                round: old.round,
                expected_token: 0,
            }))
            .unwrap();
        let token = wait_vip(old.node, old.round);
        let other = if old.node == 1 { 2 } else { 1 };
        fc[&other].store(true, Ordering::SeqCst);
        isolated.lock().unwrap().insert(old.node);
        let until = Instant::now() + Duration::from_secs(8);
        let new = loop {
            let state = statuses[&3].lock().unwrap().clone();
            if state.executor_ready
                && let Some(new) = state
                    .executor
                    .filter(|e| e.node == other && e.round > old.round)
            {
                break new;
            }
            assert!(Instant::now() < until, "其他候选应完成设备接管");
            std::thread::sleep(Duration::from_millis(20));
        };
        peers[&new.node]
            .send(NodeInput::Vip(Command::VipAcquire {
                address,
                node: new.node,
                round: new.round,
                expected_token: token,
            }))
            .unwrap();
        // 新节点不允许替旧节点伪造撤址确认，即使两者已知同一 token。
        peers[&new.node]
            .send(NodeInput::Vip(Command::VipRelease {
                address,
                node: old.node,
                round: old.round,
                token,
            }))
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            wait_vip(old.node, old.round),
            token,
            "失联/时间/新 owner ready 都不能清除旧占用"
        );
        isolated.lock().unwrap().remove(&old.node);
        peers[&old.node]
            .send(NodeInput::Vip(Command::VipRelease {
                address,
                node: old.node,
                round: old.round,
                token,
            }))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut next_retry = Instant::now();
        while statuses[&3].lock().unwrap().vips[&address].holder.is_some() {
            assert!(Instant::now() < until);
            if Instant::now() >= next_retry {
                peers[&old.node]
                    .send(NodeInput::Vip(Command::VipRelease {
                        address,
                        node: old.node,
                        round: old.round,
                        token,
                    }))
                    .unwrap();
                next_retry = Instant::now() + Duration::from_millis(200);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        peers[&new.node]
            .send(NodeInput::Vip(Command::VipAcquire {
                address,
                node: new.node,
                round: new.round,
                expected_token: token,
            }))
            .unwrap();
        let new_token = wait_vip(new.node, new.round);
        peers[&old.node]
            .send(NodeInput::Vip(Command::VipRelease {
                address,
                node: old.node,
                round: old.round,
                token,
            }))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            wait_vip(new.node, new.round),
            new_token,
            "迟到释放不能撤销新授权"
        );
        peers[&new.node].send(NodeInput::Shutdown).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !joins[new.node as usize - 1].0.is_finished(),
            "计划停机须继续驱动 Raft，等 VIP 撤址确认提交再退出"
        );
        peers[&new.node]
            .send(NodeInput::Vip(Command::VipRelease {
                address,
                node: new.node,
                round: new.round,
                token: new_token,
            }))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while statuses[&new.node]
            .lock()
            .unwrap()
            .vips
            .get(&address)
            .is_none_or(|r| r.holder.is_some())
        {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if success && ((1..=4).contains(&case) || case >= 6) && case != 10 {
        let old = first.as_ref().unwrap();
        if case == 1 || case == 4 || case >= 6 {
            use tape_rs::daemon::state::Command;
            let pool = "00000000-0000-4000-8000-000000000001".to_string();
            for cmd in [
                Command::PoolCreate {
                    uuid: pool.clone(),
                    name: "HA".into(),
                    file_limit: 1000,
                },
                Command::TapeAssign {
                    barcode: "HATESTL8".into(),
                    pool,
                },
            ] {
                let (reply, rx) = mpsc::channel();
                peers[&3].send(NodeInput::Admin { cmd, reply }).unwrap();
                assert!(matches!(
                    rx.recv_timeout(Duration::from_secs(3)),
                    Ok(tape_rs::daemon::net::AdminReply::Ok(_))
                ));
            }
            let svc = &services[&old.node];
            let until = Instant::now() + Duration::from_secs(4);
            while svc.serving_round().is_none() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            let data = vec![0x5a; 700_000];
            let h = svc.begin("/ha.bin", data.len() as u64).unwrap();
            std::fs::write(&h.spool, &data).unwrap();
            svc.ingest(&h, data.len() as u64).unwrap();
            let task = svc.finish(h, data.len() as u64).unwrap();
            assert!(matches!(
                svc.wait_task(task, Duration::from_secs(5)),
                Some(tape_rs::daemon::files::TaskStatus::Committed { .. })
            ));
        }
        if case == 9 {
            let other = if old.node == 1 { 2 } else { 1 };
            fc[&other].store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(2);
            while !statuses[&3]
                .lock()
                .unwrap()
                .candidates
                .get(&other)
                .is_some_and(|candidate| candidate.eligible)
                && Instant::now() < until
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            // Existing active handle now returns the kernel's ENODEV. Fresh
            // opens still work, as they do through the surviving FC alias.
            removed_paths[&old.node].store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(3);
            let mut changed = false;
            while Instant::now() < until {
                let state = statuses[&3].lock().unwrap().clone();
                if state.executor.as_ref().is_some_and(|owner| {
                    owner.node == other && owner.round > old.round && owner.fenced
                }) && services[&other].serving_round().is_some()
                {
                    changed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            success &= changed && services[&old.node].serving_round().is_none();
            if changed {
                let (reply, rx) = mpsc::channel();
                execs[&other]
                    .send(executor::ExecRequest::Read {
                        path: "/ha.bin".into(),
                        reply,
                    })
                    .unwrap();
                success &= rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap()
                    == vec![0x5a; 700_000];
            }
        } else if case == 11 {
            // 建池和提交可能跨过一次正常重接管，抢占观察必须使用准备完成后的轮次。
            let until = Instant::now() + Duration::from_secs(4);
            let before = loop {
                let s = statuses[&3].lock().unwrap().clone();
                if s.leader == 3
                    && s.executor_ready
                    && s.executor.as_ref().is_some_and(|e| {
                        e.node == old.node
                            && e.fenced
                            && services[&e.node].serving_round() == Some(e.round)
                    })
                {
                    break s;
                }
                assert!(Instant::now() < until, "准备后 owner 未恢复就绪: {s:?}");
                std::thread::sleep(Duration::from_millis(20));
            };
            let old = before.executor.unwrap();
            let instance = before.candidates[&old.node].instance;
            let other = if old.node == 1 { 2 } else { 1 };
            fc[&other].store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(4);
            let mut eligible_since = None;
            loop {
                let s = statuses[&3].lock().unwrap().clone();
                // 连续观察若干分配周期：资格恢复须成功，且 follower 不能抢占。
                assert_eq!(s.leader, 3);
                assert_eq!(
                    s.executor.as_ref().map(|e| (e.node, e.round)),
                    Some((old.node, old.round))
                );
                assert_eq!(s.candidates[&old.node].instance, instance);
                if s.candidates.get(&other).is_some_and(|c| c.eligible) {
                    if eligible_since.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_millis(500)
                    {
                        break;
                    }
                } else {
                    eligible_since = None;
                }
                assert!(Instant::now() < until, "FC follower 未稳定取得资格: {s:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
            // 控制权转给已合格 follower 后，才应迁移 owner。
            peers[&3]
                .send(NodeInput::Raft(Box::new(Message {
                    msg_type: raft::eraftpb::MessageType::MsgTransferLeader,
                    from: other,
                    to: 3,
                    ..Default::default()
                })))
                .unwrap();
            let until = Instant::now() + Duration::from_secs(6);
            let mut moved = false;
            while Instant::now() < until {
                let s = statuses[&other].lock().unwrap().clone();
                if s.leader == other
                    && s.executor_ready
                    && s.executor
                        .as_ref()
                        .is_some_and(|e| e.node == other && e.round > old.round && e.fenced)
                    && services[&other].serving_round().is_some()
                {
                    moved = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            success &= moved && services[&old.node].serving_round().is_none();
            if moved {
                let (reply, rx) = mpsc::channel();
                execs[&other]
                    .send(executor::ExecRequest::Read {
                        path: "/ha.bin".into(),
                        reply,
                    })
                    .unwrap();
                success &= rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap()
                    == vec![0x5a; 700_000];
            }
        } else if case >= 6 {
            slow_checkpoint.store(true, Ordering::SeqCst);
            peers[&old.node].send(NodeInput::Shutdown).unwrap();
            if case == 7 {
                let until = Instant::now() + Duration::from_secs(2);
                while slow_checkpoint.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                isolated.lock().unwrap().insert(old.node);
            }
            let deadline = Instant::now() + Duration::from_secs(6);
            while !joins[old.node as usize - 1].0.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let drive = tape_rs::scsi::reservation::read_status(&lib.drive_as(0, 99)).unwrap();
            let changer = tape_rs::scsi::reservation::read_status(&lib.changer_as(99)).unwrap();
            success &= !slow_checkpoint.load(Ordering::SeqCst);
            if case == 7 {
                let expected =
                    tape_rs::scsi::reservation::ReservationKey::new(old.node as u8, old.round).0;
                success &= drive.holder.is_some_and(|(key, _)| key == expected)
                    && changer.holder.is_some_and(|(key, _)| key == expected)
                    && services[&old.node].serving_round().is_none();
            } else {
                success &= drive.holder.is_none()
                    && drive.keys.is_empty()
                    && changer.holder.is_none()
                    && changer.keys.is_empty();
            }
        } else {
            if case == 1 {
                fc[&3].store(true, Ordering::SeqCst);
            } else if case == 2 || case == 4 {
                isolated.lock().unwrap().insert(old.node);
            } else {
                peers[&3].send(NodeInput::Shutdown).unwrap();
            }
            let until = Instant::now() + Duration::from_secs(10);
            let mut transitioned = false;
            while success && Instant::now() < until {
                let id = if case == 3 { old.node } else { 3 };
                let s = statuses[&id].lock().unwrap().clone();
                let changed = s.executor.as_ref().is_some_and(|e| {
                    e.fenced
                        && s.executor_ready
                        && if case == 3 {
                            e.node == s.leader
                                && s.leader != 3
                                && s.leader != 0
                                && if e.node == old.node {
                                    e.round == old.round
                                } else {
                                    e.round > old.round
                                }
                        } else {
                            e.node != old.node && (case != 1 || e.node == 3)
                        }
                });
                if changed {
                    transitioned = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            success &= transitioned;
            if case == 1 || case == 2 || case == 4 {
                success &= services[&old.node].serving_round().is_none();
            }
            if success && (case == 1 || case == 4) {
                let owner = statuses[&3].lock().unwrap().executor.as_ref().unwrap().node;
                let until = Instant::now() + Duration::from_secs(3);
                while services[&owner].serving_round().is_none() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let (reply, rx) = mpsc::channel();
                execs[&owner]
                    .send(executor::ExecRequest::Read {
                        path: "/ha.bin".into(),
                        reply,
                    })
                    .unwrap();
                assert_eq!(
                    rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
                    vec![0x5a; 700_000]
                );
            }
        }
    }
    if success && case == 1 {
        let round = statuses[&3]
            .lock()
            .unwrap()
            .executor
            .as_ref()
            .unwrap()
            .round;
        std::thread::sleep(Duration::from_millis(500));
        let s = statuses[&3].lock().unwrap();
        success &= s.leader == 3
            && s.executor
                .as_ref()
                .is_some_and(|e| e.node == 3 && e.round == round);
    }
    let observed: Vec<_> = statuses
        .values()
        .map(|s| s.lock().unwrap().clone())
        .collect();
    for tx in peers.values() {
        let _ = tx.send(NodeInput::Shutdown);
    }
    for (node, exec, events) in joins {
        node.join().unwrap().unwrap();
        exec.join().unwrap();
        events.join().unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
    assert!(success, "控制/执行分离场景 {case} 失败；实际 {observed:#?}");
}
