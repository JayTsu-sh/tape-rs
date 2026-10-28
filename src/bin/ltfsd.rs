//! ltfsd：三节点 Raft + 设备层隔离的守护进程骨架。
//!
//! 例：
//!   ltfsd --id 1 --listen 10.10.2.71:7400 \
//!         --peer 1=10.10.2.71:7400 --peer 2=10.10.2.72:7400 --peer 3=10.10.2.74:7400 \
//!         --data-dir /var/lib/ltfsd --changer-serial IBMtaper2287_LL3 \
//!         --drive-serial IBMtaper2287 --drive-serial IBMtaper3D5E --demo-write

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;
use sha2::{Digest, Sha256};
use tape_rs::daemon::executor::{self, ExecOptions, SgProvider};
use tape_rs::daemon::files::{BatchPolicy, FileService};
use tape_rs::daemon::http::{self, HttpContext};
use tape_rs::daemon::net::{NodeInput, TcpNet};
use tape_rs::daemon::node::{self, CoordinationConfig, NodeConfig, NodeStatus};
use tape_rs::daemon::store::RaftStore;
use tape_rs::daemon::vip::{HttpVip, VipConfig};

#[derive(Parser)]
#[command(
    name = "ltfsd",
    about = "磁带库高可用守护进程（骨架）：Raft 选主 + 设备层隔离 + 自动接管"
)]
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
    /// FDB 集群连接文件；用于权威预检和显式 FDB 运行模式
    #[cfg(feature = "fdb")]
    #[arg(long, value_name = "PATH")]
    fdb_cluster_file: Option<PathBuf>,
    /// 只读 FDB 目录元数据入口；不启动旧 Raft/SQLite 文件服务或设备执行器
    #[cfg(feature = "fdb")]
    #[arg(
        long,
        requires_all = ["fdb_cluster_file", "client_listen"],
        conflicts_with_all = [
            "http_vip",
            "demo_write",
            "expand_libraries_offline",
            "replace_drive_offline",
            "remove_drive_offline"
        ]
    )]
    fdb_metadata_readonly: bool,
    /// FDB Raft/PR owner 只读接任与 HTTP 读取（会按需装卸磁带）；不启用旧目录或写入
    #[cfg(feature = "fdb")]
    #[arg(
        long,
        requires_all = ["fdb_cluster_file", "client_listen", "libraries_config"],
        conflicts_with_all = [
            "fdb_metadata_readonly",
            "http_vip",
            "demo_write",
            "salvage",
            "expand_libraries_offline",
            "replace_drive_offline",
            "remove_drive_offline"
        ]
    )]
    fdb_owner_readonly: bool,
    /// FDB owner 每库索引核验临时库上限（MiB；默认 131072）
    #[cfg(feature = "fdb")]
    #[arg(long, requires = "fdb_owner_readonly", value_parser = clap::value_parser!(u64).range(1..=8796093022207))]
    fdb_read_index_mib: Option<u64>,
    /// FDB owner 每库 extent 缓存总预算（MiB；默认 131072）
    #[cfg(feature = "fdb")]
    #[arg(long, requires = "fdb_owner_readonly", value_parser = clap::value_parser!(u64).range(1..=8796093022207))]
    fdb_read_cache_mib: Option<u64>,
    /// FDB owner 单文件下载暂存上限（MiB；默认 131072）
    #[cfg(feature = "fdb")]
    #[arg(long, requires = "fdb_owner_readonly", value_parser = clap::value_parser!(u64).range(1..=8796093022207))]
    fdb_read_spool_mib: Option<u64>,
    /// FDB owner 本地读取预留文件系统余量（MiB；默认 1024，可为 0）
    #[cfg(feature = "fdb")]
    #[arg(long, requires = "fdb_owner_readonly", value_parser = clap::value_parser!(u64).range(..=8796093022207))]
    fdb_read_headroom_mib: Option<u64>,
    /// 多库 JSON 配置（default_library、libraries）；与单库设备参数互斥
    #[arg(long, conflicts_with_all = ["changer_serial", "drive_serials"])]
    libraries_config: Option<PathBuf>,
    /// 全集群停机后逐节点执行，仅新增带库或驱动；写入显式扩容记录并退出
    #[arg(long, alias = "add-libraries-offline", requires = "libraries_config")]
    expand_libraries_offline: bool,
    /// 全集群停机后逐节点执行，格式 OLD=NEW；仅更换同一库的一台驱动
    #[arg(
        long,
        value_name = "OLD=NEW",
        requires = "libraries_config",
        conflicts_with_all = ["expand_libraries_offline", "remove_drive_offline"]
    )]
    replace_drive_offline: Option<String>,
    /// 全集群停机后逐节点执行；从指定库移除一台非末台驱动（真实 VPD 序列号）
    #[arg(
        long,
        value_name = "SERIAL",
        requires = "libraries_config",
        conflicts_with = "expand_libraries_offline"
    )]
    remove_drive_offline: Option<String>,
    /// 受管逻辑库换带器的序列号（VPD 0x80）
    #[arg(long)]
    changer_serial: Option<String>,
    /// 换带器的 FC 目标端口 WWPN；固定各节点使用同一设备预留域
    #[arg(long, requires = "changer_serial", conflicts_with = "libraries_config")]
    changer_target_port: Option<String>,
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
    /// 同子网 HTTP IPv4 VIP：单库跟随设备 owner，多库跟随统一入口；不给则不管理地址
    #[arg(long, requires_all = ["http_vip_interface", "client_listen"])]
    http_vip: Option<Ipv4Addr>,
    /// HTTP VIP 所在以太网接口，各节点可不同（最多 13 字节）
    #[arg(long, requires = "http_vip")]
    http_vip_interface: Option<String>,
    /// 各节点客户端接口的端口，用来在 503 里给出 Leader 地址（主机取自 --peer）
    #[arg(long, default_value_t = 7401)]
    client_port: u16,
    /// Raft 逻辑时钟周期（毫秒）
    #[arg(long, default_value_t = 100)]
    tick_ms: u64,
    /// 多数派执行许可时限（毫秒），不改变磁带命令超时
    #[arg(long, default_value_t = 6000, value_parser = clap::value_parser!(u64).range(1000..))]
    execution_permit_ms: u64,
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
    let action = SigAction::new(
        SigHandler::Handler(on_stop_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
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
    let mut http_vip = args
        .http_vip
        .filter(|_| {
            !args.expand_libraries_offline
                && args.replace_drive_offline.is_none()
                && args.remove_drive_offline.is_none()
        })
        .map(|address| {
            let config = VipConfig {
                address,
                interface: args.http_vip_interface.clone().expect("VIP 接口"),
            };
            config
                .validate(
                    args.client_listen.as_deref().expect("HTTP 监听地址"),
                    Duration::from_millis(args.execution_permit_ms),
                )
                .and_then(|()| {
                    HttpVip::prepare(config, Duration::from_millis(args.execution_permit_ms))
                })
                .unwrap_or_else(|e| {
                    eprintln!("HTTP VIP 配置失败：{e}");
                    std::process::exit(2);
                })
        });
    let mut ids: Vec<u64> = peers.keys().copied().collect();
    ids.sort();
    std::fs::create_dir_all(&args.data_dir).expect("创建数据目录");
    // 必须在打开 SQLite、清理 spool、启动网络和设备线程之前取得锁。
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(args.data_dir.join("ltfsd.lock"))
        .expect("打开进程锁");
    let _instance_lock =
        nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .unwrap_or_else(|(_, e)| {
                eprintln!("该数据目录已有 ltfsd 实例：{e}");
                std::process::exit(2)
            });
    if args.expand_libraries_offline
        || args.replace_drive_offline.is_some()
        || args.remove_drive_offline.is_some()
    {
        let result = (|| -> tape_rs::error::Result<u64> {
            use std::net::{TcpStream, ToSocketAddrs};
            for (&id, address) in &peers {
                if id == me {
                    continue;
                }
                for socket in address.to_socket_addrs()? {
                    if TcpStream::connect_timeout(&socket, Duration::from_millis(500)).is_ok() {
                        return Err(tape_rs::error::TapeError::Ltfs(format!(
                            "节点 {id} 的 Raft 端口仍可连接；先停掉全部成员"
                        )));
                    }
                }
            }
            let value =
                serde_json::from_slice(&std::fs::read(args.libraries_config.as_ref().unwrap())?)
                    .map_err(|e| {
                        tape_rs::error::TapeError::Ltfs(format!("多库配置 JSON 无效：{e}"))
                    })?;
            let config = tape_rs::daemon::multilib::LibrariesConfig::parse(&value)?;
            if let Some(spec) = &args.replace_drive_offline {
                let (old, new) = spec.split_once('=').ok_or_else(|| {
                    tape_rs::error::TapeError::Ltfs("驱动更换格式应为 OLD=NEW".into())
                })?;
                if old.is_empty() || new.is_empty() || new.contains('=') {
                    return Err(tape_rs::error::TapeError::Ltfs(
                        "驱动更换格式应为非空的 OLD=NEW".into(),
                    ));
                }
                config.replace_drive_offline(&args.data_dir, old, new)
            } else if let Some(serial) = &args.remove_drive_offline {
                config.remove_drive_offline(&args.data_dir, serial)
            } else {
                config.add_libraries_offline(&args.data_dir)
            }
        })();
        match result {
            Ok(index) => println!(
                "已记录离线配置变更（本节点变更前 Raft 提交索引 {index}）；核对全部节点索引一致后再启动集群"
            ),
            Err(e) => {
                eprintln!("离线配置变更失败：{e}");
                std::process::exit(2);
            }
        }
        return;
    }
    // FDB 已激活的库不能再由旧 Raft/SQLite 文件服务接管。此检查必须
    // 早于设备锁、网络、执行线程和 FileService 的暂存区清理。
    #[cfg(feature = "fdb")]
    #[cfg(feature = "fdb")]
    let mut fdb_routes = None;
    #[cfg(feature = "fdb")]
    let _fdb_network = if let Some(cluster_file) = args.fdb_cluster_file.as_ref() {
        let network = unsafe { foundationdb::boot() };
        match preflight_catalog(&args, cluster_file) {
            Ok(routes) => fdb_routes = Some(routes),
            Err(error) => {
                eprintln!("FDB 目录权威预检失败：{error}");
                std::process::exit(2);
            }
        }
        Some(network)
    } else {
        None
    };
    #[cfg(feature = "fdb")]
    if args.fdb_metadata_readonly {
        let cluster = args
            .fdb_cluster_file
            .as_ref()
            .and_then(|path| path.to_str())
            .expect("clap 已检查 FDB 集群文件路径");
        let catalog =
            tape_rs::managed_catalog::ManagedCatalog::open(Some(cluster)).unwrap_or_else(|error| {
                eprintln!("FDB 目录连接失败：{error}");
                std::process::exit(2);
            });
        let listen = args
            .client_listen
            .as_deref()
            .expect("clap 已检查 HTTP 地址");
        if let Err(error) = tape_rs::daemon::fdb_http::serve(
            listen,
            catalog,
            fdb_routes.expect("FDB 目录权威预检已经通过"),
        ) {
            eprintln!("FDB 元数据 HTTP 监听失败：{error}");
            std::process::exit(2);
        }
        let (signal_tx, _signal_rx) = channel();
        install_signal_handler(signal_tx);
        while !STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        return;
    }
    #[cfg(feature = "fdb")]
    let fdb_owner = if args.fdb_owner_readonly {
        let storage = fdb_read_budget(&args).unwrap_or_else(|error| {
            eprintln!("FDB 本地读取预算无效：{error}");
            std::process::exit(2);
        });
        let cluster = args
            .fdb_cluster_file
            .as_ref()
            .and_then(|path| path.to_str())
            .expect("clap 已检查 FDB 集群文件路径");
        let catalog =
            tape_rs::managed_catalog::ManagedCatalog::open(Some(cluster)).unwrap_or_else(|error| {
                eprintln!("FDB owner 连接失败：{error}");
                std::process::exit(2);
            });
        Some(FdbOwnerMode {
            storage,
            catalog: Arc::new(catalog),
            routes: fdb_routes.take().expect("FDB 活动路由预检已通过"),
        })
    } else {
        None
    };
    if args.libraries_config.is_some() {
        #[cfg(feature = "fdb")]
        let result = run_libraries(args, peers, http_vip, fdb_owner);
        #[cfg(not(feature = "fdb"))]
        let result = run_libraries(args, peers, http_vip);
        if let Err(e) = result {
            eprintln!("多库 ltfsd 退出：{e}");
            std::process::exit(1);
        }
        return;
    }
    if args.data_dir.join("libraries.json").exists() {
        eprintln!("该数据目录已经启用多库，必须提供 --libraries-config，禁止按单库重放多库日志");
        std::process::exit(2);
    }
    let mut devices = args.drive_serials.clone();
    devices.sort();
    if devices.windows(2).any(|w| w[0] == w[1]) || devices.iter().any(String::is_empty) {
        eprintln!("驱动器序列号不得重复或为空");
        std::process::exit(2);
    }
    let group_devices = if let Some(port) = &args.changer_target_port {
        let port = tape_rs::scsi::inquiry::normalize_fc_target_port(port).unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(2);
        });
        serde_json::to_vec(&(args.changer_serial.as_ref(), &devices, port))
    } else {
        serde_json::to_vec(&(args.changer_serial.as_ref(), &devices))
    }
    .expect("设备组编码");
    let group = format!("{:x}", Sha256::digest(group_devices));
    // 同一主机不同 data-dir 也不能同时操作同一设备组（同 I_T nexus 的 PR 不能区分进程）。
    let lock_dir = std::path::Path::new("/run/lock/tape-rs");
    std::fs::create_dir_all(lock_dir).expect("创建设备组锁目录");
    if let Some(changer) = &args.changer_serial {
        devices.push(changer.clone());
    }
    devices.sort();
    devices.dedup();
    let mut _device_locks = Vec::new();
    for serial in devices {
        let name = format!("ltfsd-{:x}.lock", Sha256::digest(serial.as_bytes()));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_dir.join(name))
            .expect("打开设备锁");
        _device_locks.push(
            nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
                .unwrap_or_else(|(_, e)| {
                    eprintln!("本机已有实例管理设备 {serial}：{e}");
                    std::process::exit(2)
                }),
        );
    }

    let (inbox_tx, inbox_rx) = channel::<NodeInput>();
    let others: HashMap<u64, String> = peers
        .iter()
        .filter(|(id, _)| **id != me)
        .map(|(i, a)| (*i, a.clone()))
        .collect();
    let directory_file = args.data_dir.join("directory.db");
    let net = TcpNet::start(
        &args.listen,
        &others,
        inbox_tx.clone(),
        Some(tape_rs::daemon::directory::Directory::snapshot_path(
            &directory_file,
        )),
    )
    .expect("启动节点间通信");
    let fetcher = net.fetcher();

    let (exec_tx, exec_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let provider = Box::new(SgProvider {
        changer_serial: args.changer_serial.clone(),
        changer_target_port: args.changer_target_port.clone(),
        drive_serials: args.drive_serials.clone(),
    });
    let provider = match http_vip.as_ref() {
        Some(vip) => vip.guard_provider(provider),
        None => provider,
    };
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
    let files = FileService::with_options(
        args.data_dir.join("spool"),
        policy,
        Some(directory_file.clone()),
    )
    .expect("创建暂存区");
    let status = Arc::new(Mutex::new(NodeStatus::default()));
    let status_for_exec = status.clone();
    files.set_executor(exec_tx.clone());
    files.set_task_events(ev_tx.clone());
    let files_for_exec = files.clone();
    thread::Builder::new()
        .name("ltfsd-exec".into())
        .spawn(move || {
            executor::run(
                provider,
                eopts,
                exec_rx,
                ev_tx,
                files_for_exec,
                status_for_exec,
            )
        })
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
        coordination: Some(CoordinationConfig {
            group,
            validity: Duration::from_millis(args.execution_permit_ms),
        }),
        id: me,
        peers: ids,
        tick: Duration::from_millis(args.tick_ms),
        election_ticks: 10,
        heartbeat_ticks: 3,
        status_file: Some(args.data_dir.join("status.json")),
        directory_file: Some(directory_file),
        cooldown: Duration::from_secs(20),
        shutdown_grace: Duration::from_secs(args.shutdown_grace_s),
        drives: args.drive_serials.len(),
        snapshot: tape_rs::daemon::store::SnapshotPolicy {
            entries: args.snapshot_entries,
            bytes: args.snapshot_mib << 20,
        },
    };
    if let Some(listen) = &args.client_listen {
        let client_addrs = peers
            .iter()
            .map(|(id, addr)| {
                (
                    *id,
                    format!(
                        "{}:{}",
                        addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
                        args.client_port
                    ),
                )
            })
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
        if let Some(vip) = http_vip.as_mut() {
            vip.start(status.clone(), files.clone(), inbox_tx.clone())
                .expect("启动 HTTP VIP");
        }
    }
    install_signal_handler(inbox_tx.clone());
    if let Err(e) = node::run_with_catalog(
        cfg,
        store,
        Box::new(net),
        Some(fetcher),
        Some(inbox_tx.clone()),
        inbox_rx,
        exec_tx,
        status,
        Some(files.clone()),
    ) {
        eprintln!("ltfsd 退出: {}", e);
        drop(http_vip);
        std::process::exit(1);
    }
}

/// 切换后拒绝旧文件服务重启；FDB 不可达或活动证明损坏均失败关闭。
/// 本函数不授予 FDB 读写权限，正式切换仍需独立的运行时实现。
#[cfg(feature = "fdb")]
fn preflight_catalog(
    args: &Args,
    cluster_file: &std::path::Path,
) -> tape_rs::error::Result<
    std::collections::BTreeMap<String, tape_rs::managed_catalog::ActiveLibraryRoute>,
> {
    use tape_rs::daemon::multilib::LibrariesConfig;
    use tape_rs::error::TapeError;
    use tape_rs::managed_catalog::ManagedCatalog;

    let serials = if let Some(path) = args.libraries_config.as_ref() {
        let value = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|error| TapeError::Ltfs(format!("多库配置 JSON 无效：{error}")))?;
        let config = LibrariesConfig::parse(&value)?;
        config.check_manifest(&args.data_dir)?;
        config
            .load_additions(&args.data_dir)?
            .libraries
            .into_iter()
            .map(|library| library.changer_serial)
            .collect::<Vec<_>>()
    } else {
        vec![
            args.changer_serial
                .clone()
                .ok_or_else(|| TapeError::Ltfs("FDB 权威预检需要带库真实序列号".into()))?,
        ]
    };
    let cluster = cluster_file
        .to_str()
        .ok_or_else(|| TapeError::Ltfs("FDB 集群文件路径不是 UTF-8".into()))?;
    let catalog = ManagedCatalog::open(Some(cluster))
        .map_err(|error| TapeError::Ltfs(format!("连接 FDB 失败：{error}")))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut active_routes = std::collections::BTreeMap::new();
    for serial in &serials {
        let active = runtime
            .block_on(catalog.active_library_route(serial))
            .map_err(|error| TapeError::Ltfs(format!("带库 {serial} 的 FDB 路由无效：{error}")))?;
        if args.fdb_metadata_readonly || args.fdb_owner_readonly {
            let route = active.ok_or_else(|| {
                TapeError::Ltfs(format!(
                    "带库 {serial} 尚未激活 FDB 路由，禁止启动只读元数据入口"
                ))
            })?;
            active_routes.insert(serial.clone(), route);
        } else if active.is_some() {
            return Err(TapeError::Ltfs(format!(
                "带库 {serial} 已切换到 FDB，禁止启动旧 Raft/SQLite 文件服务"
            )));
        }
    }
    Ok(active_routes)
}

#[cfg(feature = "fdb")]
fn fdb_read_budget(args: &Args) -> Result<tape_rs::daemon::fdb_read::ReadStorageBudget, String> {
    let bytes = |value: Option<u64>, default, name: &str, zero: bool| {
        let value = value.unwrap_or(default);
        if !zero && value == 0 {
            return Err(format!("{name} 必须大于 0"));
        }
        value
            .checked_mul(1 << 20)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| format!("{name} 超过本地文件长度上限"))
    };
    Ok(tape_rs::daemon::fdb_read::ReadStorageBudget {
        index_bytes: bytes(args.fdb_read_index_mib, 131072, "索引临时库", false)?,
        cache_bytes: bytes(args.fdb_read_cache_mib, 131072, "extent 缓存", false)?,
        download_bytes: bytes(args.fdb_read_spool_mib, 131072, "下载暂存", false)?,
        headroom_bytes: bytes(args.fdb_read_headroom_mib, 1024, "保留余量", true)?,
    })
}

#[cfg(feature = "fdb")]
struct FdbOwnerMode {
    storage: tape_rs::daemon::fdb_read::ReadStorageBudget,
    catalog: Arc<tape_rs::managed_catalog::ManagedCatalog>,
    routes: std::collections::BTreeMap<String, tape_rs::managed_catalog::ActiveLibraryRoute>,
}

/// 多库启动仍只有一份 RaftStore/TcpNet；每个库使用独立执行线程。
fn run_libraries(
    args: Args,
    peers: HashMap<u64, String>,
    mut vip: Option<HttpVip>,
    #[cfg(feature = "fdb")] fdb_owner: Option<FdbOwnerMode>,
) -> tape_rs::error::Result<()> {
    use std::collections::BTreeMap;
    use tape_rs::daemon::{
        gateway,
        multilib::{self, GATEWAY, LibrariesConfig, LibraryRuntime},
    };
    let value = serde_json::from_slice(&std::fs::read(args.libraries_config.as_ref().unwrap())?)
        .map_err(|e| tape_rs::error::TapeError::Ltfs(format!("多库配置 JSON 无效：{e}")))?;
    let config = LibrariesConfig::parse(&value)?;
    config.check_manifest(&args.data_dir)?;
    let config = config.load_additions(&args.data_dir)?;
    #[cfg(feature = "fdb")]
    let fdb_active = fdb_owner.is_some();
    #[cfg(not(feature = "fdb"))]
    let fdb_active = false;
    let lock_dir = std::path::Path::new("/run/lock/tape-rs");
    std::fs::create_dir_all(lock_dir)?;
    let mut locks = Vec::new();
    for l in &config.libraries {
        for serial in std::iter::once(&l.changer_serial).chain(&l.drive_serials) {
            let path = lock_dir.join(format!(
                "ltfsd-{:x}.lock",
                Sha256::digest(serial.as_bytes())
            ));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)?;
            locks.push(
                nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
                    .map_err(|(_, e)| {
                        tape_rs::error::TapeError::Ltfs(format!(
                            "本机已有实例管理设备 {serial}：{e}"
                        ))
                    })?,
            );
        }
    }
    let (tx, rx) = channel();
    let others = peers
        .iter()
        .filter(|(id, _)| **id != args.id as u64)
        .map(|(id, a)| (*id, a.clone()))
        .collect();
    let bundle = args.data_dir.join("cluster-directory.db");
    let net = TcpNet::start(
        &args.listen,
        &others,
        tx.clone(),
        (!fdb_active).then(|| tape_rs::daemon::directory::Directory::snapshot_path(&bundle)),
    )?;
    let fetcher = net.fetcher();
    let validity = Duration::from_millis(args.execution_permit_ms);
    let group = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&config.value()).unwrap())
    );
    let mut contexts = BTreeMap::new();
    let mut runtimes = Vec::new();
    let client_addrs: HashMap<u64, String> = peers
        .iter()
        .map(|(id, addr)| {
            (
                *id,
                format!(
                    "{}:{}",
                    addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
                    args.client_port
                ),
            )
        })
        .collect();
    // 客户端的重试提示仍指向固定 VIP，内部路由单独使用物理节点地址。
    let hints = args
        .http_vip
        .map(|address| {
            peers
                .keys()
                .map(|id| (*id, format!("{address}:{}", args.client_port)))
                .collect()
        })
        .unwrap_or_else(|| client_addrs.clone());
    let policy = BatchPolicy {
        max_bytes: args.batch_mib << 20,
        max_files: args.batch_files,
        idle: Duration::from_millis(args.batch_idle_ms),
        max_wait: Duration::from_millis(args.batch_max_wait_ms),
    };
    for library in &config.libraries {
        let dir = config.library_dir(&args.data_dir, &library.changer_serial);
        std::fs::create_dir_all(&dir)?;
        let directory_file = (!fdb_active).then(|| dir.join("directory.db"));
        if let Some(path) = directory_file.as_ref() {
            // 旧目录模式先创建目录，HTTP reader 可在协调线程之前打开。
            drop(tape_rs::daemon::directory::Directory::open(path)?);
        }
        let files = FileService::with_options(
            dir.join(if fdb_active {
                "fdb-owner-spool"
            } else {
                "spool"
            }),
            policy,
            directory_file.clone(),
        )?;
        let status = Arc::new(Mutex::new(NodeStatus::default()));
        let (exec, requests) = channel();
        let (events, event_rx) = channel();
        files.set_executor(exec.clone());
        files.set_task_events(events.clone());
        let provider = Box::new(SgProvider {
            changer_serial: Some(library.changer_serial.clone()),
            changer_target_port: library.changer_target_port.clone(),
            drive_serials: library.drive_serials.clone(),
        });
        let opts = ExecOptions {
            node_id: args.id,
            interval: Duration::from_millis(args.interval_ms),
            demo_write: args.demo_write,
            salvage: args.salvage,
            block_size: args.block_size,
            read_idle: Duration::from_millis(args.read_idle_ms),
        };
        let (f, s) = (files.clone(), status.clone());
        #[cfg(feature = "fdb")]
        let fdb_control =
            fdb_owner
                .as_ref()
                .map(|mode| tape_rs::daemon::executor::FdbOwnerControl {
                    storage: mode.storage,
                    catalog: mode.catalog.clone(),
                    serial: library.changer_serial.clone(),
                    route: *mode
                        .routes
                        .get(&library.changer_serial)
                        .expect("已预检全部带库活动路由"),
                });
        thread::Builder::new()
            .name(format!("ltfsd-{}", library.changer_serial))
            .spawn(move || {
                #[cfg(feature = "fdb")]
                if let Some(control) = fdb_control {
                    executor::run_fdb_owner(provider, opts, requests, events, f, s, control);
                    return;
                }
                executor::run(provider, opts, requests, events, f, s);
            })?;
        let scoped = multilib::scoped_sender(library.changer_serial.clone(), tx.clone());
        let forward = scoped.clone();
        thread::spawn(move || {
            for event in event_rx {
                if forward.send(NodeInput::Exec(event)).is_err() {
                    break;
                }
            }
        });
        let ctx = Arc::new(HttpContext {
            files,
            status: status.clone(),
            exec: Mutex::new(exec.clone()),
            node: Mutex::new(scoped),
            client_addrs: hints
                .iter()
                .map(|(id, a)| (*id, format!("{a}{}", library.http_prefix())))
                .collect(),
            wait_timeout: Duration::from_secs(600),
        });
        contexts.insert(library.changer_serial.clone(), ctx.clone());
        runtimes.push(LibraryRuntime {
            id: library.changer_serial.clone(),
            group: format!("{group}:{}", library.changer_serial),
            drives: library.drive_serials.len(),
            directory_file,
            exec,
            status,
            files: ctx.files.clone(),
        });
    }
    let gateway_files = FileService::with_options(
        args.data_dir.join(if fdb_active {
            "fdb-gateway-spool"
        } else {
            "gateway-spool"
        }),
        policy,
        None,
    )?;
    let gateway_status = Arc::new(Mutex::new(NodeStatus::default()));
    let (exec, requests) = channel();
    let (events, event_rx) = channel();
    let provider: Box<dyn tape_rs::daemon::executor::DeviceProvider> =
        Box::new(multilib::GatewayProvider);
    let provider = match vip.as_ref() {
        Some(v) => v.guard_provider(provider),
        None => provider,
    };
    let files = gateway_files.clone();
    thread::spawn(move || multilib::run_gateway(provider, requests, events, files));
    let forward = tx.clone();
    thread::spawn(move || {
        for event in event_rx {
            if forward.send(NodeInput::Exec(event)).is_err() {
                break;
            }
        }
    });
    runtimes.push(LibraryRuntime {
        id: GATEWAY.into(),
        group: format!("{group}:gateway"),
        drives: 0,
        directory_file: None,
        exec: exec.clone(),
        status: gateway_status.clone(),
        files: gateway_files.clone(),
    });
    let gateway = Arc::new(HttpContext {
        files: gateway_files.clone(),
        status: gateway_status.clone(),
        exec: Mutex::new(exec),
        node: Mutex::new(tx.clone()),
        client_addrs: hints,
        wait_timeout: Duration::from_secs(600),
    });
    if let Some(listen) = &args.client_listen {
        #[cfg(feature = "fdb")]
        if let Some(mode) = fdb_owner.as_ref() {
            let cluster = args
                .fdb_cluster_file
                .as_ref()
                .and_then(|path| path.to_str())
                .expect("clap 已检查 FDB 集群文件路径");
            let catalog = tape_rs::managed_catalog::ManagedCatalog::open(Some(cluster))
                .map_err(|error| tape_rs::error::TapeError::Catalog(error.to_string()))?;
            tape_rs::daemon::fdb_http::serve_owner(
                listen,
                Arc::new(catalog),
                mode.routes.clone(),
                contexts,
                client_addrs,
            )?;
        } else {
            gateway::serve(listen, config.clone(), contexts, gateway, client_addrs)?;
            if let Some(vip) = vip.as_mut() {
                vip.start(gateway_status, gateway_files, tx.clone())?;
            }
        }
        #[cfg(not(feature = "fdb"))]
        {
            gateway::serve(listen, config.clone(), contexts, gateway, client_addrs)?;
            if let Some(vip) = vip.as_mut() {
                vip.start(gateway_status, gateway_files, tx.clone())?;
            }
        }
    }
    let mut ids: Vec<_> = peers.keys().copied().collect();
    ids.sort();
    let store = RaftStore::open(&args.data_dir.join("raft.db"), &ids)?;
    let cfg = NodeConfig {
        id: args.id as u64,
        peers: ids,
        tick: Duration::from_millis(args.tick_ms),
        election_ticks: 10,
        heartbeat_ticks: 3,
        status_file: Some(args.data_dir.join("status.json")),
        directory_file: (!fdb_active).then_some(bundle),
        cooldown: Duration::from_secs(20),
        snapshot: tape_rs::daemon::store::SnapshotPolicy {
            entries: args.snapshot_entries,
            bytes: args.snapshot_mib << 20,
        },
        shutdown_grace: Duration::from_secs(args.shutdown_grace_s),
        drives: 0,
        coordination: Some(CoordinationConfig { group, validity }),
    };
    install_signal_handler(tx.clone());
    let result = multilib::run(
        cfg,
        config,
        store,
        Box::new(net),
        Some(fetcher),
        tx,
        rx,
        runtimes,
    );
    drop(vip);
    drop(locks);
    result
}

#[cfg(all(test, feature = "fdb"))]
mod fdb_owner_cli_tests {
    use super::*;

    #[test]
    fn local_storage_flags_validate_units_limits_and_owner_scope() {
        let base = [
            "ltfsd",
            "--id",
            "1",
            "--listen",
            "127.0.0.1:7400",
            "--peer",
            "1=127.0.0.1:7400",
            "--data-dir",
            "/tmp/fdb-cli",
            "--libraries-config",
            "/tmp/libs.json",
            "--client-listen",
            "127.0.0.1:7500",
            "--fdb-cluster-file",
            "/tmp/fdb.cluster",
            "--fdb-owner-readonly",
        ];
        let defaults = fdb_read_budget(&Args::try_parse_from(base).unwrap()).unwrap();
        assert_eq!(defaults.cache_bytes, 128 << 30);
        assert_eq!(defaults.headroom_bytes, 1 << 30);
        let mut configured = base.to_vec();
        configured.extend([
            "--fdb-read-index-mib",
            "96",
            "--fdb-read-cache-mib",
            "64",
            "--fdb-read-spool-mib",
            "32",
            "--fdb-read-headroom-mib",
            "0",
        ]);
        let budget = fdb_read_budget(&Args::try_parse_from(configured).unwrap()).unwrap();
        assert_eq!(
            (
                budget.index_bytes,
                budget.cache_bytes,
                budget.download_bytes,
                budget.headroom_bytes
            ),
            (96 << 20, 64 << 20, 32 << 20, 0)
        );
        for flag in [
            "--fdb-read-index-mib",
            "--fdb-read-cache-mib",
            "--fdb-read-spool-mib",
        ] {
            for value in ["0", "18446744073709551615"] {
                let mut invalid = base.to_vec();
                invalid.extend([flag, value]);
                assert!(Args::try_parse_from(invalid).is_err());
            }
        }
        let mut outside_owner = base[..base.len() - 1].to_vec();
        outside_owner.extend(["--fdb-read-cache-mib", "64"]);
        assert!(Args::try_parse_from(outside_owner).is_err());
    }

    #[test]
    fn owner_readonly_requires_fdb_and_rejects_legacy_write_options() {
        let base = [
            "ltfsd",
            "--id",
            "1",
            "--listen",
            "127.0.0.1:7400",
            "--peer",
            "1=127.0.0.1:7400",
            "--data-dir",
            "/tmp/ltfsd-owner-cli",
            "--libraries-config",
            "/tmp/libraries.json",
            "--client-listen",
            "127.0.0.1:7500",
        ];
        let mut allowed = base.to_vec();
        allowed.extend([
            "--fdb-cluster-file",
            "/tmp/fdb.cluster",
            "--fdb-owner-readonly",
        ]);
        assert!(Args::try_parse_from(&allowed).is_ok());
        let mut missing = base.to_vec();
        missing.push("--fdb-owner-readonly");
        assert!(Args::try_parse_from(&missing).is_err());
        let mut legacy_write = allowed;
        legacy_write.push("--demo-write");
        assert!(Args::try_parse_from(&legacy_write).is_err());
    }
}
