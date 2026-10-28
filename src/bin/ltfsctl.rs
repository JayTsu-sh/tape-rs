//! ltfsctl：基于 Rust 客户端库的命令行。给出全部节点地址即可，库自己找 Leader。

use clap::{Parser, Subcommand, ValueEnum};
use tape_rs::client::Client;

#[derive(Parser)]
#[command(name = "ltfsctl", about = "ltfsd 客户端")]
struct Args {
    /// 节点的客户端接口地址，重复给出
    #[arg(short = 'e', long = "endpoint", required = true)]
    endpoints: Vec<String>,
    /// 目标带库的真实 changer 序列号；省略时访问默认库
    #[arg(long)]
    library: Option<String>,
    /// 文件命名空间的池名称或 UUID；库内有多个池时必填
    #[arg(long)]
    pool: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 上传本地文件并等到落带（默认替换已有版本）
    Put {
        file: String,
        path: String,
        /// 只创建：路径已有已提交版本时失败
        #[arg(long)]
        new: bool,
    },
    /// 把已提交普通文件的内容校验后复制到另一库/池的空路径；后续改写不会自动同步
    Copy {
        source_path: String,
        destination_path: String,
        #[arg(long)]
        to_library: String,
        #[arg(long)]
        to_pool: String,
        /// 本地临时文件目录；需容纳整个源文件
        #[arg(long)]
        spool_dir: Option<std::path::PathBuf>,
    },
    /// 读出已提交的文件
    Get { path: String, output: String },
    /// 删除文件并等墓碑落带（不立即回收介质空间）
    Delete {
        path: String,
        /// 仅当当前文件仍是指定的 round.sequence 版本时删除
        #[arg(long)]
        if_version: Option<String>,
    },
    /// 查询路径状态：已提交 / 上传中 / 已暂存
    Stat { path: String },
    /// 列出已提交的全部文件；给出 --dir 时只列该目录的直接子项
    List {
        #[arg(long)]
        dir: Option<String>,
        /// 同时列出尚未落带的在途文件（需要 --dir）
        #[arg(long)]
        pending: bool,
    },
    /// 各节点的自述
    Cluster,
    /// 带库管理状态
    Library {
        #[command(subcommand)]
        cmd: LibraryCmd,
    },
    /// 驱动器管理状态
    Drive {
        #[command(subcommand)]
        cmd: DriveCmd,
    },
    /// 节点管理状态
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// 节点组管理状态
    Nodegroup {
        #[command(subcommand)]
        cmd: NodegroupCmd,
    },
    /// 集群管理任务
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// 查询文件操作任务
    FileTask { id: u64 },
    /// 调用指定带库的 HTTP 管理写入接口；设备任务返回 ID 后须查询终态
    Admin {
        #[arg(value_enum)]
        method: AdminMethod,
        /// `/admin/...` 路径；库由 --library 选择
        route: String,
        /// JSON 请求体；省略时发送空对象
        #[arg(long)]
        body: Option<String>,
    },
    /// 池管理
    Pool {
        #[command(subcommand)]
        cmd: PoolCmd,
    },
    /// 磁带归属
    Tape {
        #[command(subcommand)]
        cmd: TapeCmd,
    },
}

#[derive(Subcommand)]
enum PoolCmd {
    /// 新建池
    Create {
        name: String,
        /// 每盘带的文件数软上限（默认 200000）
        #[arg(long)]
        file_limit: Option<u64>,
    },
    /// 列出池与各池的磁带。加 --all-nodes 时逐个节点询问，用来核对三个节点是否一致
    List {
        #[arg(long)]
        all_nodes: bool,
    },
    /// 查询池详情
    Show { name: String },
    /// 修改池属性；服务端执行任务并返回任务号
    Set {
        name: String,
        attribute: String,
        value: String,
    },
    /// 删除空池
    Delete { name: String },
}

#[derive(Subcommand)]
enum LibraryCmd {
    /// 列出集群内所有带库
    List,
    /// 查询一台带库；省略时使用 --library 所选库或默认库
    Show { serial: Option<String> },
    /// 显式刷新当前库的设备库存
    Rescan,
}

#[derive(Subcommand)]
enum DriveCmd {
    List,
    Show { id: String },
    Up { id: String },
    Down { id: String },
    Role { id: String, role: String },
    Assign { id: String, group: String },
}

#[derive(Subcommand)]
enum NodeCmd {
    List,
    Show { id: String },
    Assign { id: String, group: String },
}

#[derive(Subcommand)]
enum NodegroupCmd {
    List,
    Show { id: String },
    Create { name: String },
    Delete { name: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    List,
    Show {
        id: String,
    },
    Cancel {
        id: String,
    },
    /// 只清理终态任务历史，需显式 --yes
    Clearhistory {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum AdminMethod {
    Post,
    Patch,
    Delete,
}

#[derive(Subcommand)]
enum TapeCmd {
    /// 列出磁带状态和容量
    List,
    /// 查询磁带详情
    Show { barcode: String },
    /// 把磁带（条码）归入池（名称或 UUID）
    Assign { barcode: String, pool: String },
    /// 解除归属
    Unassign { barcode: String },
    /// 将已分配磁带设为离线；返回管理任务号
    Offline { barcode: String, message: String },
    /// 实际装带验证后解除离线；返回管理任务号
    Online { barcode: String },
    /// 将已分配磁带列入黑名单；返回管理任务号
    Blacklist { barcode: String, reason: String },
    /// 从黑名单移除；返回管理任务号
    ClearBlacklist { barcode: String },
    /// 校验 LTFS 索引；可选完整读取文件并核对已有哈希
    Validate {
        barcode: String,
        #[arg(long)]
        full_read: bool,
    },
    /// 以实带 LTFS 索引重新对账集群目录；返回管理任务号
    Reconcile { barcode: String },
    /// 从实带索引列出 EE 对象；返回管理任务号
    Objects {
        barcode: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=500))]
        limit: u64,
    },
    /// 回收：把带上还活着的文件搬到同池的其他带上，然后重新格式化这盘带。
    /// 需要两台驱动器；命令立即返回，搬迁在后台进行，进度看 `pool list`
    Reclaim { barcode: String },
}

fn main() {
    let args = Args::parse();
    let mut c = Client::new(args.endpoints.clone());
    let r: Result<(), Box<dyn std::error::Error>> = (|| {
        if let Some(serial) = &args.library {
            c = c.with_library(serial.clone())?;
        }
        if let Some(pool) = &args.pool {
            c = c.with_pool(pool.clone())?;
        }
        match args.cmd {
            Cmd::Put { file, path, new } => {
                let local = std::path::Path::new(&file);
                let len = std::fs::metadata(local)?.len();
                let o = c.put_file(&path, local, new, true)?;
                println!(
                    "已提交 {} ({} 字节) 索引代数 {}  请求次数 {}{}",
                    path,
                    len,
                    o.generation,
                    o.attempts,
                    if o.resolved_by_query {
                        "  (经查询判定)"
                    } else {
                        ""
                    }
                );
            }
            Cmd::Copy {
                source_path,
                destination_path,
                to_library,
                to_pool,
                spool_dir,
            } => {
                if args.library.is_none() || args.pool.is_none() {
                    return Err("跨库复制必须用 --library 和 --pool 指定源库及源池".into());
                }
                let mut target = Client::new(args.endpoints.clone())
                    .with_library(to_library)?
                    .with_pool(to_pool)?;
                let scratch = spool_dir.unwrap_or_else(std::env::temp_dir);
                let outcome =
                    c.copy_verified_to(&mut target, &source_path, &destination_path, &scratch)?;
                println!(
                    "已复制并校验 {} -> {} ({} 字节，SHA-256 {}，源版本 {}.{})；后续改写不会自动同步",
                    source_path,
                    destination_path,
                    outcome.destination.length,
                    outcome.destination.sha256,
                    outcome.source.version.0,
                    outcome.source.version.1
                );
            }
            Cmd::Get { path, output } => {
                let mut out = std::fs::File::create(&output)?;
                let len = c.get_to(&path, &mut out)?;
                println!("{} -> {} ({} 字节)", path, output, len);
            }
            Cmd::Delete { path, if_version } => {
                if let Some(version) = if_version {
                    let (round, sequence) =
                        version.split_once('.').ok_or("版本应为 round.sequence")?;
                    c.delete_if_version(&path, (round.parse()?, sequence.parse()?))?;
                } else {
                    c.delete(&path)?;
                }
                println!("已删除 {}（墓碑已落带）", path);
            }
            Cmd::Stat { path } => match c.stat_path(&path)? {
                None => println!("不存在"),
                Some(p) => {
                    if p.state != "committed" {
                        println!(
                            "{}  已接收 {} 字节",
                            if p.state == "staged" {
                                "已暂存，等待落带"
                            } else {
                                "上传中"
                            },
                            p.length
                        );
                    }
                    match p.current {
                        Some(s) => println!(
                            "{}  {} 字节  索引代数 {}  版本 {}.{}  磁带 {}  sha256 {}",
                            if p.state == "committed" {
                                "已提交"
                            } else {
                                "当前版本"
                            },
                            s.length,
                            s.generation,
                            s.version.0,
                            s.version.1,
                            s.barcode,
                            s.sha256
                        ),
                        None => println!("没有已提交的版本"),
                    }
                }
            },
            Cmd::List { dir: None, .. } => {
                for (p, n) in c.list()? {
                    println!("{:>12}  {}", n, p);
                }
            }
            Cmd::List {
                dir: Some(d),
                pending,
            } => {
                for e in c.list_dir(&d, pending)? {
                    if e.is_dir {
                        println!("{:>12}  {}/", "", e.name);
                    } else {
                        let n = e.committed_length.or(e.staged_length).unwrap_or(0);
                        let st = if e.state == "committed" {
                            String::new()
                        } else {
                            format!("  ({})", e.state)
                        };
                        println!("{:>12}  {}{}", n, e.name, st);
                    }
                }
            }
            Cmd::Pool {
                cmd: PoolCmd::Create { name, file_limit },
            } => {
                println!(
                    "已创建池 {}  UUID {}",
                    name,
                    c.pool_create(&name, file_limit)?
                );
            }
            Cmd::Pool {
                cmd: PoolCmd::List { all_nodes },
            } => {
                let targets: Vec<Option<String>> = if all_nodes {
                    args.endpoints.iter().cloned().map(Some).collect()
                } else {
                    vec![None]
                };
                for t in targets {
                    if let Some(a) = &t {
                        println!("== {}", a);
                    }
                    match c.pools(t.as_deref()) {
                        Ok(pools) if pools.is_empty() => println!("  (没有池)"),
                        Ok(pools) => {
                            for p in pools {
                                println!(
                                    "  {}  {}  文件数上限 {}  磁带 [{}]",
                                    p.name,
                                    p.uuid,
                                    p.file_limit,
                                    p.tapes.join(", ")
                                );
                                for t in &p.tape_details {
                                    println!(
                                        "    {:<10} {:<14} gen={:<5} 文件 {}/{}  已用 {} MiB  可回收 {} MiB",
                                        t.barcode,
                                        t.state,
                                        t.generation,
                                        t.live_files,
                                        t.files,
                                        t.bytes_used >> 20,
                                        t.reclaimable >> 20
                                    );
                                }
                            }
                        }
                        Err(e) => println!("  不可达: {}", e),
                    }
                }
            }
            Cmd::Pool {
                cmd: PoolCmd::Show { name },
            } => print_json(c.admin_get(&format!("/admin/pools/{name}"))?),
            Cmd::Pool {
                cmd:
                    PoolCmd::Set {
                        name,
                        attribute,
                        value,
                    },
            } => print_json(c.admin_request(
                "PATCH",
                &format!("/admin/pools/{name}"),
                serde_json::json!({"attribute":attribute,"value":value}),
            )?),
            Cmd::Pool {
                cmd: PoolCmd::Delete { name },
            } => print_json(c.admin_request(
                "DELETE",
                &format!("/admin/pools/{name}"),
                serde_json::json!({}),
            )?),
            Cmd::Library {
                cmd: LibraryCmd::List,
            } => print_json(c.libraries()?),
            Cmd::Library {
                cmd: LibraryCmd::Show { serial },
            } => {
                if let Some(serial) = serial {
                    c = c.with_library(serial)?;
                }
                print_json(c.admin_get("/admin/libraries/current")?);
            }
            Cmd::Library {
                cmd: LibraryCmd::Rescan,
            } => print_json(c.admin_request(
                "POST",
                "/admin/libraries/current/rescan",
                serde_json::json!({}),
            )?),
            Cmd::Drive { cmd } => match cmd {
                DriveCmd::List => print_json(c.admin_get("/admin/drives")?),
                DriveCmd::Show { id } => print_json(c.admin_get(&format!("/admin/drives/{id}"))?),
                DriveCmd::Up { id } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/drives/{id}/up"),
                    serde_json::json!({}),
                )?),
                DriveCmd::Down { id } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/drives/{id}/down"),
                    serde_json::json!({}),
                )?),
                DriveCmd::Role { id, role } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/drives/{id}/role"),
                    serde_json::json!({"role":role}),
                )?),
                DriveCmd::Assign { id, group } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/drives/{id}/assign"),
                    serde_json::json!({"group":group}),
                )?),
            },
            Cmd::Node { cmd } => match cmd {
                NodeCmd::List => print_json(c.admin_get("/admin/nodes")?),
                NodeCmd::Show { id } => print_json(c.admin_get(&format!("/admin/nodes/{id}"))?),
                NodeCmd::Assign { id, group } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/nodes/{id}/assign"),
                    serde_json::json!({"group":group}),
                )?),
            },
            Cmd::Nodegroup { cmd } => match cmd {
                NodegroupCmd::List => print_json(c.admin_get("/admin/nodegroups")?),
                NodegroupCmd::Show { id } => {
                    print_json(c.admin_get(&format!("/admin/nodegroups/{id}"))?)
                }
                NodegroupCmd::Create { name } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/nodegroups/{name}"),
                    serde_json::json!({}),
                )?),
                NodegroupCmd::Delete { name } => print_json(c.admin_request(
                    "DELETE",
                    &format!("/admin/nodegroups/{name}"),
                    serde_json::json!({}),
                )?),
            },
            Cmd::Task { cmd } => match cmd {
                TaskCmd::List => print_json(c.admin_get("/admin/tasks")?),
                TaskCmd::Show { id } => print_json(c.admin_get(&format!("/admin/tasks/{id}"))?),
                TaskCmd::Cancel { id } => print_json(c.admin_request(
                    "POST",
                    &format!("/admin/tasks/{id}/cancel"),
                    serde_json::json!({}),
                )?),
                TaskCmd::Clearhistory { yes } => {
                    if !yes {
                        return Err("清理任务历史须显式 --yes".into());
                    }
                    print_json(c.admin_request(
                        "POST",
                        "/admin/tasks/clearhistory",
                        serde_json::json!({"confirm":true}),
                    )?);
                }
            },
            Cmd::FileTask { id } => print_json(c.file_task(id)?),
            Cmd::Admin {
                method,
                route,
                body,
            } => {
                let method = match method {
                    AdminMethod::Post => "POST",
                    AdminMethod::Patch => "PATCH",
                    AdminMethod::Delete => "DELETE",
                };
                let body = body
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or_else(|| serde_json::json!({}));
                print_json(c.admin_request(method, &route, body)?);
            }
            Cmd::Tape { cmd: TapeCmd::List } => print_json(c.admin_get("/admin/tapes")?),
            Cmd::Tape {
                cmd: TapeCmd::Show { barcode },
            } => print_json(c.admin_get(&format!("/admin/tapes/{barcode}"))?),
            Cmd::Tape {
                cmd: TapeCmd::Assign { barcode, pool },
            } => println!("{}", c.tape_assign(&barcode, &pool)?),
            Cmd::Tape {
                cmd: TapeCmd::Unassign { barcode },
            } => println!("{}", c.tape_unassign(&barcode)?),
            Cmd::Tape {
                cmd: TapeCmd::Offline { barcode, message },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/offline"),
                serde_json::json!({"message":message}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::Online { barcode },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/online"),
                serde_json::json!({}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::Blacklist { barcode, reason },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/blacklist"),
                serde_json::json!({"reason":reason}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::ClearBlacklist { barcode },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/clear-blacklist"),
                serde_json::json!({}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::Validate { barcode, full_read },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/validate"),
                serde_json::json!({"full_read":full_read}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::Reconcile { barcode },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/reconcile"),
                serde_json::json!({}),
            )?),
            Cmd::Tape {
                cmd:
                    TapeCmd::Objects {
                        barcode,
                        offset,
                        limit,
                    },
            } => print_json(c.admin_request(
                "POST",
                &format!("/admin/tapes/{barcode}/objects"),
                serde_json::json!({"offset":offset,"limit":limit}),
            )?),
            Cmd::Tape {
                cmd: TapeCmd::Reclaim { barcode },
            } => println!("{}", c.tape_reclaim(&barcode)?),
            Cmd::Cluster => {
                for e in &args.endpoints {
                    match c.node_status(e) {
                        Ok(j) => println!("{}  {}", e, j),
                        Err(err) => println!("{}  不可达: {}", e, err),
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("错误: {}", e);
        std::process::exit(1);
    }
}

fn print_json(value: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
