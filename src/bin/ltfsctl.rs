//! ltfsctl：基于 Rust 客户端库的命令行。给出全部节点地址即可，库自己找 Leader。

use clap::{Parser, Subcommand, ValueEnum};
use tape_rs::client::Client;

#[path = "ltfsctl/fdb_rename.rs"]
mod fdb_rename;
#[path = "ltfsctl/inventory.rs"]
mod inventory;
#[path = "ltfsctl/lifecycle.rs"]
mod lifecycle;
#[path = "ltfsctl/notify.rs"]
mod notify;
#[path = "ltfsctl/pool.rs"]
mod pool;
#[path = "ltfsctl/table.rs"]
mod table;
#[path = "ltfsctl/tape_import.rs"]
mod tape_import;

#[derive(Parser)]
#[command(name = "ltfsctl", about = "ltfsd 客户端")]
struct Args {
    /// 节点的客户端接口地址，重复给出
    #[arg(short = 'e', long = "endpoint", required = true)]
    endpoints: Vec<String>,
    /// 目标带库的真实 changer 序列号；省略时访问默认库
    #[arg(short = 'l', long, global = true)]
    library: Option<String>,
    /// 文件命名空间的池名称或 UUID；库内有多个池时必填
    #[arg(short = 'p', long, global = true)]
    pool: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 外部系统通知：持久订阅、SSE 接收和显式确认
    Notify {
        #[command(subcommand)]
        cmd: notify::Command,
    },
    /// FDB 服务：查询、校验下载和持久上传
    Fdb {
        #[command(subcommand)]
        cmd: FdbCmd,
    },
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
        cmd: pool::Command,
    },
    /// 磁带归属
    Tape {
        #[command(subcommand)]
        cmd: TapeCmd,
    },
}

#[derive(Subcommand)]
enum FdbCmd {
    /// 原双路径重命名；持久准备、运行、查询或取消
    Rename {
        #[command(subcommand)]
        cmd: fdb_rename::Command,
    },
    /// 文件、目录和链接变更；先保存逻辑请求，再提交或查询原请求
    Fs {
        #[command(subcommand)]
        cmd: FdbFilesystemCmd,
    },
    /// FDB 持久化管理任务：查询、重试及取消原请求
    Control {
        #[command(subcommand)]
        cmd: FdbControlCmd,
    },
    /// FDB 驱动器策略
    Drive {
        #[command(subcommand)]
        cmd: FdbDriveCmd,
    },
    /// 从三节点冻结屏障初始化全新空目录；需 fdb 特性，不导入旧目录、不操作磁带
    #[cfg(feature = "fdb")]
    Init {
        #[arg(long)]
        cluster_file: std::path::PathBuf,
        /// 稳定初始化身份；重复执行必须使用同一 UUID
        #[arg(long)]
        namespace: uuid::Uuid,
    },
    /// 查询库级命名空间的路径元数据（不读磁带）
    Stat { path: String },
    /// 按 S3 ListObjectsV2 规则列举池内对象；完整对象键字典序
    List {
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long, default_value = "")]
        delimiter: String,
        #[arg(long, default_value_t = 1000)]
        max_keys: usize,
        #[arg(long)]
        continuation_token: Option<String>,
        #[arg(long)]
        start_after: Option<String>,
    },
    /// 库级目录的一页直接子项；内部诊断用途
    Children {
        path: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        after: Option<String>,
    },
    /// 按显式池读取普通文件，校验后发布本地文件；不覆盖已有输出
    Get {
        path: String,
        output: std::path::PathBuf,
    },
    /// 准备上传并保存原请求与正文目录；默认只创建，不写磁带、不预留路径
    Prepare {
        file: std::path::PathBuf,
        path: String,
        /// 保存原请求与正文的目录，目标必须不存在
        #[arg(long)]
        request: std::path::PathBuf,
        /// 覆盖时必须给出当前 FDB 文件版本
        #[arg(long)]
        expected_version: Option<u64>,
        /// 可选诊断约束；省略时由服务端从所选池自动选带
        #[arg(long)]
        barcode: Option<String>,
    },
    /// 使用请求目录内的原正文上传；响应不确定时只查询原身份
    Upload { request: std::path::PathBuf },
    /// 查询已保存请求的持久化结果
    Status {
        request: std::path::PathBuf,
        /// 查询原提交的完整属性与物理身份，无需当前路径或 owner
        #[arg(long)]
        details: bool,
    },
}

#[derive(clap::Args)]
struct FdbChangeArgs {
    /// 预期的 Active namespace，防止切换后误改另一目录
    #[arg(long)]
    namespace: uuid::Uuid,
    /// 原请求的保存位置；目标必须不存在，文件系统变更使用目录
    #[arg(long)]
    request: std::path::PathBuf,
}

#[derive(Subcommand)]
enum FdbFilesystemCmd {
    /// 查询已提交节点的原属性、扩展属性及原生链接目标；不访问磁带
    Attributes { path: String },
    /// 先保存原意图目录再只读预检；尚不登记任务或写入磁带
    Prepare {
        path: String,
        /// 修改或删除时要求的原逻辑版本；创建时省略
        #[arg(long)]
        expected_version: Option<u64>,
        /// 类型化逻辑变更 JSON，例如 {"kind":"create-directory"}
        #[arg(long)]
        change: String,
        #[command(flatten)]
        save: FdbChangeArgs,
    },
    /// 提交或重试保存的原请求，等待持久化终态
    Run { request: std::path::PathBuf },
    /// 查询原请求；null 表示尚未登记
    Status { request: std::path::PathBuf },
    /// 取消尚未写入的原请求；已结束时返回首次终态
    Cancel { request: std::path::PathBuf },
}

#[derive(Subcommand)]
enum FdbControlCmd {
    /// 提交或重试保存的原请求，等待持久化终态
    Run { request: std::path::PathBuf },
    /// 查询保存的原请求；null 表示尚未登记
    Status { request: std::path::PathBuf },
    /// 原子取消尚未执行的请求；已结束时返回原终态
    Cancel { request: std::path::PathBuf },
    List {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        after: Option<String>,
    },
}

#[derive(Subcommand)]
enum FdbDriveCmd {
    Show {
        serial: String,
    },
    /// 规范 JSON patch：enabled、role(m/r/g)
    Set {
        serial: String,
        #[arg(long)]
        patch: String,
        #[command(flatten)]
        change: FdbChangeArgs,
    },
}

fn fdb_change(
    client: &tape_rs::client::fdb::FdbClient,
    library: &str,
    args: FdbChangeArgs,
    change: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let prepared = client.prepare_control(library, args.namespace, change)?;
    prepared.save_new(&args.request)?;
    eprintln!(
        "管理请求已保存至 {}；结果未定时保留此文件重试",
        args.request.display()
    );
    print_json(client.run_management_saved(&args.request)?);
    Ok(())
}

fn fdb_scope(
    value: &serde_json::Value,
    namespace: uuid::Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    if value["namespace"] != namespace.to_string() {
        return Err("Active namespace 与命令不符".into());
    }
    Ok(())
}

#[derive(Subcommand)]
enum LibraryCmd {
    /// 从单一物理节点读取已冻结空目录屏障，不写FDB或磁带
    NativeBarrier {
        #[arg(long)]
        challenge: uuid::Uuid,
    },
    /// 列出集群内所有带库
    List {
        #[command(flatten)]
        output: inventory::ListOutput,
    },
    /// 查询一台带库；单库可省略，多库须指定此参数或 --library
    Show {
        serial: Option<String>,
        #[command(flatten)]
        output: inventory::ShowOutput,
    },
    /// 显式刷新当前库的设备库存
    Rescan(lifecycle::Rescan),
}

#[derive(Subcommand)]
enum DriveCmd {
    List,
    Show { id: String },
    Up { id: String },
    Down { id: String },
    Role { id: String, role: String },
}

#[derive(Subcommand)]
enum NodeCmd {
    List,
    Show { id: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    List,
    Show { id: String },
    Cancel { id: String },
}

#[derive(Clone, ValueEnum)]
enum AdminMethod {
    Post,
    Patch,
    Delete,
}

#[derive(Subcommand)]
enum TapeCmd {
    /// 导入已有 LTFS 索引或刷新已分配卷的 FDB 目录；不格式化或擦除
    Import(tape_import::Command),
    /// 列出磁带状态和容量
    List {
        #[command(flatten)]
        output: inventory::ListOutput,
        #[arg(long,value_parser=["decimal","binary"])]
        unit: Option<String>,
    },
    /// 查询磁带详情
    Show {
        barcode: String,
        #[command(flatten)]
        output: inventory::ShowOutput,
    },
    /// 异步检查/初始化磁带并归入池；格式化须 --yes
    Assign(lifecycle::Assign),
    /// 异步证明无引用后解除池归属，不擦除介质
    Unassign(lifecycle::Targets),
    /// 异步移动介质到 homeslot 或 ieslot
    Move(lifecycle::Move),
    /// 将已分配磁带设为离线；返回管理任务号
    Offline(lifecycle::Offline),
    /// 实际装带验证后解除离线；返回管理任务号
    Online(lifecycle::Targets),
    /// 设置黑名单及离线属性；返回管理任务号
    Set(lifecycle::Set),
    /// 异步核验标签、LTFS 索引和结构；不读取全部文件正文
    Validate(lifecycle::Targets),
    /// 回收：把带上还活着的文件搬到同池的其他带上，然后重新格式化这盘带。
    /// 需要两台驱动器；命令立即返回，搬迁在后台进行，进度看 `pool list`
    Reclaim { barcode: String },
}

fn main() {
    let args = Args::parse();
    let mut c = Client::new(args.endpoints.clone());
    let r: Result<(), Box<dyn std::error::Error>> = (|| {
        if args.pool.is_some() && matches!(&args.cmd, Cmd::Library { .. } | Cmd::Task { .. }) {
            return Err("library 与 task 管理命令不接受 --pool".into());
        }
        if let Some(serial) = &args.library {
            c = c.with_library(serial.clone())?;
        }
        if let Some(pool) = &args.pool {
            c = c.with_pool(pool.clone())?;
        }
        match args.cmd {
            Cmd::Fdb { cmd } => {
                use tape_rs::client::fdb::{
                    FdbClient, NodeCondition, PreparedManagement, UploadStatus,
                };
                let client = FdbClient::new(args.endpoints.clone())?;
                match cmd {
                    FdbCmd::Rename { cmd } => fdb_rename::run(
                        &client,
                        args.library.as_deref(),
                        args.pool.as_deref(),
                        cmd,
                    )?,
                    FdbCmd::Fs { cmd } => {
                        if let FdbFilesystemCmd::Attributes { path } = cmd {
                            let library = args
                                .library
                                .as_deref()
                                .ok_or("FDB fs attributes 必须指定 --library")?;
                            let pool = args
                                .pool
                                .as_deref()
                                .ok_or("FDB fs attributes 必须指定 --pool UUID")?
                                .parse::<uuid::Uuid>()?;
                            print_json(
                                client
                                    .node_attributes(library, pool, &path)?
                                    .unwrap_or(serde_json::Value::Null),
                            );
                        } else if let FdbFilesystemCmd::Prepare {
                            path,
                            expected_version,
                            change,
                            save,
                        } = cmd
                        {
                            let library = args
                                .library
                                .as_deref()
                                .ok_or("FDB fs prepare 必须指定 --library")?;
                            let pool = args
                                .pool
                                .as_deref()
                                .ok_or("FDB fs prepare 必须指定 --pool UUID")?
                                .parse::<uuid::Uuid>()?;
                            use tape_rs::client::FileVersion;
                            if expected_version
                                .is_some_and(|version| version == 0 || version == u64::MAX)
                            {
                                return Err("节点原版本必须大于零且可递增".into());
                            }
                            let parent_path = path
                                .rsplit_once('/')
                                .map(|(p, _)| if p.is_empty() { "/" } else { p })
                                .ok_or("节点路径必须包含父目录")?;
                            let parent = client
                                .stat(library, pool, parent_path)?
                                .ok_or("节点父目录不存在")?;
                            if parent["kind"] != "directory"
                                || parent["namespace"] != save.namespace.to_string()
                            {
                                return Err("节点父目录类型或 namespace 与条件不一致".into());
                            }
                            let parent = FileVersion::Fdb {
                                namespace: save.namespace,
                                inode: parent["id"].as_str().ok_or("父目录缺少 inode")?.parse()?,
                                version: parent["version"].as_u64().ok_or("父目录缺少原版本")?,
                            };
                            let node = if let Some(version) = expected_version {
                                let node =
                                    client.stat(library, pool, &path)?.ok_or("原节点不存在")?;
                                if node["namespace"] != save.namespace.to_string()
                                    || node["version"].as_u64() != Some(version)
                                {
                                    return Err("原节点 namespace 或版本与条件不一致".into());
                                }
                                Some(FileVersion::Fdb {
                                    namespace: save.namespace,
                                    inode: node["id"]
                                        .as_str()
                                        .ok_or("原节点缺少 inode")?
                                        .parse()?,
                                    version,
                                })
                            } else {
                                None
                            };
                            client.prepare_node_mutation(
                                library,
                                pool,
                                &path,
                                NodeCondition { parent, node },
                                serde_json::from_str(&change)?,
                                &save.request,
                            )?;
                            println!(
                                "原节点变更已保存至 {}；尚未登记任务或写带",
                                save.request.display()
                            );
                        } else {
                            if args.library.is_some() || args.pool.is_some() {
                                return Err(
                                    "节点变更作用域来自请求文件，请省略 --library/--pool".into()
                                );
                            }
                            match cmd {
                                FdbFilesystemCmd::Run { request } => {
                                    print_json(client.run_node_mutation_saved(&request)?)
                                }
                                FdbFilesystemCmd::Status { request } => print_json(
                                    client
                                        .node_mutation_status_saved(&request)?
                                        .unwrap_or(serde_json::Value::Null),
                                ),
                                FdbFilesystemCmd::Cancel { request } => {
                                    print_json(client.cancel_node_mutation_saved(&request)?)
                                }
                                FdbFilesystemCmd::Prepare { .. }
                                | FdbFilesystemCmd::Attributes { .. } => unreachable!(),
                            }
                        }
                    }
                    FdbCmd::Control { cmd } => {
                        if let FdbControlCmd::List { limit, after } = cmd {
                            let library = args
                                .library
                                .as_deref()
                                .ok_or("FDB control list 必须指定 --library")?;
                            if args.pool.is_some() {
                                return Err("FDB control 不接受 --pool".into());
                            }
                            print_json(client.list_management(library, limit, after.as_deref())?);
                        } else {
                            if args.library.is_some() || args.pool.is_some() {
                                return Err(
                                    "任务作用域来自请求文件，请省略 --library/--pool".into()
                                );
                            }
                            match cmd {
                                FdbControlCmd::Run { request } => {
                                    print_json(client.run_management_saved(&request)?)
                                }
                                FdbControlCmd::Status { request } => print_json(
                                    client
                                        .management_status(&PreparedManagement::load(&request)?)?
                                        .unwrap_or(serde_json::Value::Null),
                                ),
                                FdbControlCmd::Cancel { request } => {
                                    print_json(client.cancel_management_saved(&request)?)
                                }
                                FdbControlCmd::List { .. } => unreachable!(),
                            }
                        }
                    }
                    FdbCmd::Drive { cmd } => {
                        let library = args
                            .library
                            .as_deref()
                            .ok_or("FDB drive 必须指定 --library")?;
                        if args.pool.is_some() {
                            return Err("FDB drive 不接受 --pool".into());
                        }
                        match cmd {
                            FdbDriveCmd::Show { serial } => {
                                print_json(client.management_policy(library, "drive", &serial)?)
                            }
                            FdbDriveCmd::Set {
                                serial,
                                patch,
                                change,
                            } => {
                                let patch: serde_json::Value = serde_json::from_str(&patch)?;
                                let value = client.management_policy(library, "drive", &serial)?;
                                fdb_scope(&value, change.namespace)?;
                                fdb_change(
                                    &client,
                                    library,
                                    change,
                                    serde_json::json!({"kind":"drive","serial":serial,"patch":patch,"revision":value["policy"]["revision"]}),
                                )?;
                            }
                        }
                    }
                    #[cfg(feature = "fdb")]
                    FdbCmd::Init {
                        cluster_file,
                        namespace,
                    } => {
                        if args.pool.is_some() {
                            return Err("FDB init 不接受 --pool；初始目录没有池".into());
                        }
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB init 必须指定 --library")?;
                        if args.endpoints.len() != 3
                            || args
                                .endpoints
                                .iter()
                                .collect::<std::collections::BTreeSet<_>>()
                                .len()
                                != 3
                        {
                            return Err("FDB init 必须给出三个不同节点的 --endpoint".into());
                        }
                        let _network = unsafe { foundationdb::boot() };
                        let catalog = tape_rs::managed_catalog::ManagedCatalog::open(Some(
                            cluster_file.to_str().ok_or("集群文件路径必须是 UTF-8")?,
                        ))?;
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        let active = if let Some(active) = runtime
                            .block_on(catalog.native_initialization_status(serial, namespace))?
                        {
                            active
                        } else {
                            let challenge = uuid::Uuid::new_v4();
                            let replies = args
                                .endpoints
                                .iter()
                                .map(|endpoint| {
                                    Client::new([endpoint])
                                        .with_library(serial)?
                                        .native_library_barrier(challenge)
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            let proof = tape_rs::managed_catalog::verify_native_barriers(
                                serial, challenge, &replies,
                            )?;
                            runtime
                                .block_on(catalog.initialize_native_library(namespace, &proof))?
                        };
                        print_json(
                            serde_json::json!({"library_serial":serial,"namespace":active.route.namespace.to_string(),"frozen_index":active.frozen_index,"initial_owner_epoch":active.initial_owner_epoch}),
                        );
                    }
                    FdbCmd::Stat { path } => {
                        let pool = args
                            .pool
                            .as_deref()
                            .ok_or("FDB stat 必须指定 --pool UUID")?
                            .parse::<uuid::Uuid>()?;
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB stat 必须指定 --library")?;
                        print_json(client.stat(serial, pool, &path)?.ok_or("路径不存在")?);
                    }
                    FdbCmd::List {
                        prefix,
                        delimiter,
                        max_keys,
                        continuation_token,
                        start_after,
                    } => {
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB list 必须指定 --library")?;
                        let pool = args
                            .pool
                            .as_deref()
                            .ok_or("FDB list 必须指定 --pool UUID")?
                            .parse::<uuid::Uuid>()?;
                        let options = tape_rs::client::fdb::ListObjectsOptions {
                            prefix,
                            delimiter,
                            max_keys,
                            continuation_token,
                            start_after,
                        };
                        print_json(client.list_objects_v2(serial, pool, &options)?);
                    }
                    FdbCmd::Children { path, limit, after } => {
                        let pool = args
                            .pool
                            .as_deref()
                            .ok_or("FDB children 必须指定 --pool UUID")?
                            .parse::<uuid::Uuid>()?;
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB children 必须指定 --library")?;
                        print_json(client.list_children(
                            serial,
                            pool,
                            &path,
                            limit,
                            after.as_deref(),
                        )?);
                    }
                    FdbCmd::Get { path, output } => {
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB get 必须指定 --library")?;
                        let pool = args
                            .pool
                            .as_deref()
                            .ok_or("FDB get 必须指定 --pool UUID")?
                            .parse::<uuid::Uuid>()?;
                        let size = client.download_new(serial, pool, &path, &output)?;
                        println!("已校验下载 {size} 字节至 {}", output.display());
                    }
                    FdbCmd::Prepare {
                        file,
                        path,
                        request,
                        expected_version,
                        barcode,
                    } => {
                        let serial = args
                            .library
                            .as_deref()
                            .ok_or("FDB 准备上传必须显式指定 --library")?;
                        let pool = args
                            .pool
                            .as_deref()
                            .ok_or("FDB 准备上传必须显式指定 --pool UUID")?
                            .parse::<uuid::Uuid>()?;
                        use tape_rs::client::{FileVersion, fdb::UploadCondition};
                        if expected_version
                            .is_some_and(|version| version == 0 || version == u64::MAX)
                        {
                            return Err("覆盖版本必须大于零且可递增".into());
                        }
                        let parent_path = path
                            .rsplit_once('/')
                            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                            .ok_or("上传路径必须包含父目录")?;
                        let baseline = client
                            .stat(
                                serial,
                                pool,
                                if expected_version.is_some() {
                                    &path
                                } else {
                                    parent_path
                                },
                            )?
                            .ok_or("准备上传的父目录或覆盖目标不存在")?;
                        let namespace = baseline["namespace"]
                            .as_str()
                            .ok_or("准备目标缺少 namespace")?
                            .parse::<uuid::Uuid>()?;
                        let condition = if let Some(version) = expected_version {
                            if baseline["kind"] != "file"
                                || baseline["version"].as_u64() != Some(version)
                            {
                                return Err("覆盖目标类型或当前版本与条件不一致".into());
                            }
                            UploadCondition::Replace(FileVersion::Fdb {
                                namespace,
                                inode: baseline["id"]
                                    .as_str()
                                    .ok_or("覆盖目标缺少 inode")?
                                    .parse()?,
                                version,
                            })
                        } else {
                            if baseline["kind"] != "directory" {
                                return Err("上传父节点不是目录".into());
                            }
                            UploadCondition::Create {
                                namespace,
                                parent: baseline["id"]
                                    .as_str()
                                    .ok_or("上传父目录缺少 inode")?
                                    .parse()?,
                            }
                        };
                        client
                            .prepare_file(
                                serial,
                                pool,
                                &path,
                                &file,
                                condition,
                                barcode.as_deref(),
                                &request,
                            )
                            .map_err(|error| {
                                if request.is_dir() {
                                    format!(
                                        "准备失败；请保留原请求目录 {} 并使用 upload 重试：{error}",
                                        request.display()
                                    )
                                } else {
                                    error.to_string()
                                }
                            })?;
                        println!("原上传请求与正文已保存至 {}；尚未写带", request.display());
                    }
                    FdbCmd::Upload { request } => {
                        if args.library.is_some() || args.pool.is_some() {
                            return Err(
                                "FDB 重试的库和池来自请求目录，请省略 --library/--pool".into()
                            );
                        }
                        let committed = client.upload_saved(&request)?;
                        println!(
                            "原上传操作 {} 已提交，FDB 版本 {}",
                            committed.operation(),
                            committed.version()
                        );
                    }
                    FdbCmd::Status { request, details } => {
                        if args.library.is_some() || args.pool.is_some() {
                            return Err(
                                "FDB 结果查询的库和池来自请求目录，请省略 --library/--pool".into(),
                            );
                        }
                        let status = client.status_saved(&request)?;
                        if status == UploadStatus::NotPrepared {
                            println!(
                                "原意图与正文已保存，尚未完成 prepare；请保留目录并使用 upload 重试"
                            );
                        } else if details {
                            match client.saved_receipt(&request)? {
                                Some(receipt) => print_json(receipt.details().clone()),
                                None => println!(
                                    "原请求尚无可确认终态；请保留请求目录，不要重新 prepare"
                                ),
                            }
                        } else {
                            match status {
                                UploadStatus::Committed(version) => {
                                    println!("原上传请求已提交，FDB 版本 {version}")
                                }
                                UploadStatus::Unconfirmed => println!(
                                    "原请求尚无可确认终态；请保留请求目录，不要重新 prepare"
                                ),
                                UploadStatus::NotPrepared => unreachable!("handled local draft"),
                            }
                        }
                    }
                }
            }
            Cmd::Notify { cmd } => notify::run(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                args.library.as_deref(),
                args.pool.as_deref(),
                cmd,
            )?,
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
                    "已复制并校验 {} -> {} ({} 字节，SHA-256 {}，源版本 {})；后续改写不会自动同步",
                    source_path,
                    destination_path,
                    outcome.destination.length,
                    outcome.destination.sha256,
                    outcome.source.version
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
                    c.delete_if_version(
                        &path,
                        tape_rs::client::FileVersion::Raft {
                            round: round.parse()?,
                            sequence: sequence.parse()?,
                        },
                    )?;
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
                            "{}  {} 字节  索引代数 {}  版本 {}  磁带 {}  sha256 {}",
                            if p.state == "committed" {
                                "已提交"
                            } else {
                                "当前版本"
                            },
                            s.length,
                            s.generation
                                .map(|g| g.to_string())
                                .unwrap_or_else(|| "未知".into()),
                            s.version,
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
            Cmd::Pool { cmd } => pool::run(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                args.library.as_deref(),
                args.pool.as_deref(),
                cmd,
            )?,
            Cmd::Library {
                cmd: LibraryCmd::NativeBarrier { challenge },
            } => print_json(c.native_library_barrier(challenge)?),
            Cmd::Library {
                cmd: LibraryCmd::List { output },
            } => inventory::libraries(c.libraries()?, output)?,
            Cmd::Library {
                cmd: LibraryCmd::Show { serial, output },
            } => {
                let serial = pool::library_from_client(
                    &mut c,
                    serial.as_deref().or(args.library.as_deref()),
                )?;
                let client = tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?;
                inventory::show(
                    client.library_inventory(&serial)?["library"].clone(),
                    output,
                )?;
            }
            Cmd::Library {
                cmd: LibraryCmd::Rescan(command),
            } => {
                if args.pool.is_some() {
                    return Err("library rescan 不接受 --pool".into());
                }
                lifecycle::rescan(
                    &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                    &mut c,
                    args.library.as_deref(),
                    command,
                )?;
            }
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
            },
            Cmd::Node { cmd } => match cmd {
                NodeCmd::List => print_json(c.admin_get("/admin/nodes")?),
                NodeCmd::Show { id } => print_json(c.admin_get(&format!("/admin/nodes/{id}"))?),
            },
            Cmd::Task { cmd } => {
                let client = tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?;
                match cmd {
                    TaskCmd::List => {
                        let libraries = if let Some(serial) = args.library.as_deref() {
                            vec![serial.to_owned()]
                        } else {
                            c.libraries()?["items"]
                                .as_array()
                                .ok_or("带库列表无效")?
                                .iter()
                                .map(|row| {
                                    row["library_serial"]
                                        .as_str()
                                        .map(str::to_owned)
                                        .ok_or("带库序列号缺失")
                                })
                                .collect::<std::result::Result<Vec<_>, _>>()?
                        };
                        let mut items = Vec::new();
                        for serial in libraries {
                            let context = client.management_context(&serial)?;
                            let namespace = context["namespace"].as_str().ok_or("namespace缺失")?;
                            let mut after = None;
                            loop {
                                let page =
                                    client.management_page(&serial, namespace, after.as_deref())?;
                                for record in page["items"].as_array().ok_or("任务清单无效")?
                                {
                                    let mut row = record.clone();
                                    row["task_id"]=serde_json::json!(tape_rs::management_task_id::ManagementTaskId::from_receipt(record)?.to_string());
                                    items.push(row);
                                }
                                after = page["next"].as_str().map(str::to_owned);
                                if after.is_none() {
                                    break;
                                }
                            }
                        }
                        print_json(serde_json::json!({"version":1,"items":items}));
                    }
                    command => {
                        let (text, cancel) = match command {
                            TaskCmd::Show { id } => (id, false),
                            TaskCmd::Cancel { id } => (id, true),
                            TaskCmd::List => unreachable!(),
                        };
                        let id = text.parse::<tape_rs::management_task_id::ManagementTaskId>()?;
                        if args
                            .library
                            .as_ref()
                            .is_some_and(|serial| serial != &id.library)
                        {
                            return Err("任务号与显式带库不符".into());
                        }
                        let result = if cancel {
                            client.cancel_management_task_by_id(&id)?
                        } else {
                            client.management_task_by_id(&id)?
                        };
                        print_json(result.ok_or("原管理任务不存在或已超过保留窗口")?);
                    }
                }
            }
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
            Cmd::Tape {
                cmd: TapeCmd::Import(command),
            } => tape_import::run(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::List { output, unit },
            } => inventory::tapes(
                &mut c,
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                args.library.as_deref(),
                args.pool.as_deref(),
                output,
                unit,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Show { barcode, output },
            } => inventory::tape(
                &mut c,
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                args.library.as_deref(),
                args.pool.as_deref(),
                &barcode,
                output,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Assign(command),
            } => lifecycle::assign(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Unassign(command),
            } => lifecycle::unassign(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Move(command),
            } => lifecycle::move_tape(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Offline(command),
            } => lifecycle::offline(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Online(command),
            } => lifecycle::online(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Set(command),
            } => lifecycle::set(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
            Cmd::Tape {
                cmd: TapeCmd::Validate(command),
            } => lifecycle::validate(
                &tape_rs::client::fdb::FdbClient::new(args.endpoints.clone())?,
                &mut c,
                args.library.as_deref(),
                args.pool.as_deref(),
                command,
            )?,
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

#[cfg(test)]
mod inventory_cli_tests {
    use super::*;

    #[test]
    fn management_cli_has_one_hierarchy_without_nodegroups_or_sync_import() {
        for old in [
            vec!["nodegroup", "list"],
            vec!["fdb", "nodegroup", "list"],
            vec!["fdb", "pool", "list"],
            vec!["fdb", "tape", "import", "TAPE00L8"],
            vec!["tape", "import", "TAPE00L8", "--async"],
            vec!["tape", "validate", "TAPE00L8", "--full-read"],
            vec!["tape", "objects", "TAPE00L8"],
            vec!["tape", "reconcile", "TAPE00L8"],
            vec!["tape", "blacklist", "TAPE00L8", "reason"],
        ] {
            let mut args = vec!["ltfsctl", "-e", "127.0.0.1:7401"];
            args.extend(old);
            assert!(Args::try_parse_from(args).is_err());
        }
        for command in [
            vec!["library", "rescan", "-l", "LIB"],
            vec![
                "tape", "assign", "TAPE00L8", "-l", "LIB", "-p", "archive", "--yes",
            ],
            vec!["tape", "validate", "TAPE00L8", "-l", "LIB", "-p", "archive"],
            vec!["tape", "unassign", "TAPE00L8", "-l", "LIB", "-p", "archive"],
            vec!["tape", "move", "TAPE00L8", "-l", "LIB", "-L", "ieslot"],
            vec!["tape", "move", "--request", "original.json"],
            vec![
                "tape",
                "offline",
                "TAPE00L8",
                "TAPE01L8",
                "-o",
                "maintenance",
            ],
            vec!["tape", "offline", "--request", "original.json"],
            vec!["tape", "online", "TAPE00L8", "-l", "LIB", "-p", "archive"],
            vec![
                "tape",
                "set",
                "TAPE00L8",
                "--patch",
                "{\"blacklisted\":true}",
            ],
            vec!["pool", "create", "archive", "-l", "LIB"],
            vec!["pool", "list", "-l", "LIB", "--json"],
            vec!["pool", "show", "archive", "-l", "LIB"],
            vec!["pool", "delete", "archive", "-l", "LIB"],
        ] {
            let mut args = vec!["ltfsctl", "-e", "127.0.0.1:7401"];
            args.extend(command);
            assert!(Args::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn ee_inventory_options_accept_library_after_the_action() {
        for command in [
            vec!["library", "list", "--json"],
            vec!["library", "show", "SERIAL", "-a", "serial"],
            vec![
                "tape",
                "list",
                "-l",
                "SERIAL",
                "--unit",
                "binary",
                "--no-header",
            ],
            vec![
                "tape", "show", "TAPE00L8", "-l", "SERIAL", "-a", "poolname", "--jsonpp",
            ],
            vec![
                "tape",
                "import",
                "TAPE00L8",
                "TAPE01L8",
                "-l",
                "SERIAL",
                "-p",
                "POOL",
                "-P",
                "/incoming",
                "-R",
            ],
        ] {
            let mut args = vec!["ltfsctl", "-e", "127.0.0.1:7401"];
            args.extend(command);
            assert!(Args::try_parse_from(args).is_ok());
        }
    }
}
