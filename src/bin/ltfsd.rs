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
use tape_rs::daemon::files::{BatchPolicy, FileService};
use tape_rs::daemon::http::{self, HttpContext};
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
    /// 自动格式化空白带时用的块大小（字节）
    #[arg(long, default_value_t = 524_288)]
    block_size: u32,
    /// 为读请求装载的带空闲这么久后卸回槽位（毫秒）
    #[arg(long, default_value_t = 300_000)]
    read_idle_ms: u64,
    /// 自动收尾时打捞未索引数据（默认放弃）
    #[arg(long)]
    salvage: bool,
    /// 合批：队列累计字节数达到即提交（MiB）
    #[arg(long, default_value_t = 8192)]
    batch_mib: u64,
    /// 合批：队列累计文件数达到即提交
    #[arg(long, default_value_t = 20_000)]
    batch_files: usize,
    /// 合批：这么久没有新的上传完成即提交（毫秒）
    #[arg(long, default_value_t = 2000)]
    batch_idle_ms: u64,
    /// 合批：队列里最早的文件最多等这么久（毫秒）。带 wait 的上传的延迟上界
    #[arg(long, default_value_t = 60_000)]
    batch_max_wait_ms: u64,
    /// 客户端接口（HTTP）的监听地址。不给则不开
    #[arg(long)]
    client_listen: Option<String>,
    /// 各节点客户端接口的端口，用来在 503 里给出 Leader 地址（主机取自 --peer）
    #[arg(long, default_value_t = 7401)]
    client_port: u16,
    /// Raft 逻辑时钟周期（毫秒）
    #[arg(long, default_value_t = 100)]
    tick_ms: u64,
    /// 日志压缩：距上次快照累积这么多条目即做一次快照
    #[arg(long, default_value_t = 2000)]
    snapshot_entries: u64,
    /// 日志压缩：距上次快照累积这么多字节即做一次快照（MiB）
    #[arg(long, default_value_t = 256)]
    snapshot_mib: u64,
    /// 收到 SIGTERM/SIGINT 后最多等多久让执行线程落带、退带、释放预留（秒）
    #[arg(long, default_value_t = 300)]
    shutdown_grace_s: u64,
}

/// SIGTERM/SIGINT 只置一个标志——信号处理函数里能做的事很少，发通道不在其中。
/// 再来一次信号就直接退出，好让"按两次 Ctrl-C"可以放弃等待。
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_stop_signal(_: nix::libc::c_int) {
    if STOPPING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        // 已经在停了，第二次信号不再等待
        unsafe { nix::libc::_exit(1) };
    }
}

/// 装上信号处理，并起一个线程把它转成 `NodeInput::Shutdown`。
fn install_signal_handler(inbox: std::sync::mpsc::Sender<NodeInput>) {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(SigHandler::Handler(on_stop_signal), SaFlags::empty(), SigSet::empty());
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: 处理函数只对一个 AtomicBool 做 swap，失败路径只调 _exit，都是信号安全的
        if let Err(e) = unsafe { sigaction(sig, &action) } {
            eprintln!("装 {:?} 处理失败: {}", sig, e);
        }
    }
    thread::Builder::new()
        .name("ltfsd-signal".into())
        .spawn(move || {
            while !STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
            log::info!("收到停机信号，正在计划停机（再按一次直接退出）");
            let _ = inbox.send(NodeInput::Shutdown);
        })
        .expect("信号线程");
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
    let directory_file = args.data_dir.join("directory.db");
    let net = TcpNet::start(
        &args.listen,
        &others,
        inbox_tx.clone(),
        Some(tape_rs::daemon::directory::Directory::snapshot_path(&directory_file)),
    )
    .expect("启动节点间通信");
    let fetcher = net.fetcher();

    let (exec_tx, exec_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let provider = Box::new(SgProvider { changer_serial: args.changer_serial.clone(), drive_serials: args.drive_serials.clone() });
    let eopts = ExecOptions {
        node_id: args.id,
        interval: Duration::from_millis(args.interval_ms),
        demo_write: args.demo_write,
        salvage: args.salvage,
        block_size: args.block_size,
        read_idle: Duration::from_millis(args.read_idle_ms),
    };
    let policy = BatchPolicy {
        max_bytes: args.batch_mib << 20,
        max_files: args.batch_files,
        idle: Duration::from_millis(args.batch_idle_ms),
        max_wait: Duration::from_millis(args.batch_max_wait_ms),
    };
    let files = FileService::with_options(args.data_dir.join("spool"), policy, Some(directory_file.clone()))
        .expect("创建暂存区");
    let status = Arc::new(Mutex::new(NodeStatus::default()));
    let status_for_exec = status.clone();
    files.set_executor(exec_tx.clone());
    let files_for_exec = files.clone();
    thread::Builder::new()
        .name("ltfsd-exec".into())
        .spawn(move || executor::run(provider, eopts, exec_rx, ev_tx, files_for_exec, status_for_exec))
        .expect("执行线程");
    let inbox_for_events = inbox_tx.clone();
    thread::Builder::new()
        .name("ltfsd-exec-events".into())
        .spawn(move || {
            for ev in ev_rx {
                if inbox_for_events.send(NodeInput::Exec(ev)).is_err() {
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
        directory_file: Some(directory_file),
        cooldown: Duration::from_secs(20),
        shutdown_grace: Duration::from_secs(args.shutdown_grace_s),
        snapshot: tape_rs::daemon::store::SnapshotPolicy {
            entries: args.snapshot_entries,
            bytes: args.snapshot_mib << 20,
        },
    };
    if let Some(listen) = &args.client_listen {
        let client_addrs = peers
            .iter()
            .map(|(id, addr)| (*id, format!("{}:{}", addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr), args.client_port)))
            .collect();
        let ctx = Arc::new(HttpContext {
            files: files.clone(),
            status: status.clone(),
            exec: Mutex::new(exec_tx.clone()),
            node: Mutex::new(inbox_tx.clone()),
            client_addrs,
            wait_timeout: Duration::from_secs(600),
        });
        http::serve(listen, ctx).expect("启动客户端接口");
    }
    install_signal_handler(inbox_tx.clone());
    if let Err(e) =
        node::run_with(cfg, store, Box::new(net), Some(fetcher), Some(inbox_tx.clone()), inbox_rx, exec_tx, status)
    {
        eprintln!("ltfsd 退出: {}", e);
        std::process::exit(1);
    }
}
