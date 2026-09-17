//! ltfsd：三节点 Raft + 设备层隔离的守护进程骨架。
//!
//! 例：
//!   ltfsd --id 1 --listen 10.10.2.71:7400 \
//!         --peer 1=10.10.2.71:7400 --peer 2=10.10.2.72:7400 --peer 3=10.10.2.74:7400 \
//!         --data-dir /var/lib/ltfsd --changer-serial IBMtaper2287_LL3 \
//!         --drive-serial IBMtaper2287 --drive-serial IBMtaper3D5E --demo-write

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;
use tape_rs::daemon::executor::{self, ExecOptions, SgProvider};
use tape_rs::daemon::net::{NodeInput, TcpNet};
use tape_rs::daemon::node::{self, NodeConfig, NodeStatus};
use tape_rs::daemon::store::RaftStore;

#[derive(Parser)]
#[command(name = "ltfsd", about = "磁带库高可用守护进程（骨架）：Raft 选主 + 设备层隔离 + 自动接管")]
struct Args {
    /// 本节点编号（1..=255，同时写进预留键）
    #[arg(long)]
    id: u8,
    /// 节点间通信的监听地址
    #[arg(long)]
    listen: String,
    /// 全部成员，形如 1=10.10.2.71:7400，包含本节点，重复给出
    #[arg(long = "peer", value_name = "ID=ADDR")]
    peers: Vec<String>,
    /// 控制存储与状态文件所在目录
    #[arg(long)]
    data_dir: PathBuf,
    /// 受管逻辑库换带器的序列号（VPD 0x80）
    #[arg(long)]
    changer_serial: Option<String>,
    /// 受管驱动器的序列号，重复给出
    #[arg(long = "drive-serial")]
    drive_serials: Vec<String>,
    /// 服务期间的工作周期（毫秒）：持有者自检，以及可选的演示写入
    #[arg(long, default_value_t = 3000)]
    interval_ms: u64,
    /// 周期性向卷里追加一个小文件，用于故障演练
    #[arg(long)]
    demo_write: bool,
    /// 自动收尾时打捞未索引数据（默认放弃）
    #[arg(long)]
    salvage: bool,
    /// Raft 逻辑时钟周期（毫秒）
    #[arg(long, default_value_t = 100)]
    tick_ms: u64,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let mut peers: HashMap<u64, String> = HashMap::new();
    for p in &args.peers {
        let Some((id, addr)) = p.split_once('=') else {
            eprintln!("--peer 需要 ID=ADDR 形式: {}", p);
            std::process::exit(2);
        };
        peers.insert(id.parse().expect("节点编号"), addr.to_string());
    }
    let me = args.id as u64;
    if !peers.contains_key(&me) || peers.len() < 3 {
        eprintln!("--peer 必须列出包含本节点在内的至少三个成员");
        std::process::exit(2);
    }
    let mut ids: Vec<u64> = peers.keys().copied().collect();
    ids.sort();
    std::fs::create_dir_all(&args.data_dir).expect("创建数据目录");

    let (inbox_tx, inbox_rx) = channel::<NodeInput>();
    let others: HashMap<u64, String> = peers.iter().filter(|(id, _)| **id != me).map(|(i, a)| (*i, a.clone())).collect();
    let net = TcpNet::start(&args.listen, &others, inbox_tx.clone()).expect("启动节点间通信");

    let (exec_tx, exec_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let provider = Box::new(SgProvider { changer_serial: args.changer_serial.clone(), drive_serials: args.drive_serials.clone() });
    let eopts = ExecOptions {
        node_id: args.id,
        interval: Duration::from_millis(args.interval_ms),
        demo_write: args.demo_write,
        salvage: args.salvage,
    };
    thread::Builder::new().name("ltfsd-exec".into()).spawn(move || executor::run(provider, eopts, exec_rx, ev_tx)).expect("执行线程");
    thread::Builder::new()
        .name("ltfsd-exec-events".into())
        .spawn(move || {
            for ev in ev_rx {
                if inbox_tx.send(NodeInput::Exec(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("事件转发线程");

    let store = RaftStore::open(&args.data_dir.join("raft.db"), &ids).expect("打开控制存储");
    let cfg = NodeConfig {
        id: me,
        peers: ids,
        tick: Duration::from_millis(args.tick_ms),
        election_ticks: 10,
        heartbeat_ticks: 3,
        status_file: Some(args.data_dir.join("status.json")),
        cooldown: Duration::from_secs(20),
    };
    let status = Arc::new(Mutex::new(NodeStatus::default()));
    if let Err(e) = node::run(cfg, store, Box::new(net), inbox_rx, exec_tx, status) {
        eprintln!("ltfsd 退出: {}", e);
        std::process::exit(1);
    }
}
