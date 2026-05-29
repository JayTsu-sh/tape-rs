use clap::{Parser, Subcommand};
use log::error;

use tape_rs::error::Result;

mod cli;

use cli::catalog_cli;
use cli::changer;
use cli::inquiry;
use cli::ltfs_cli;
use cli::tape;

#[derive(Parser)]
#[command(name = "tape-rs", about = "IBM TS4300 Tape Library SCSI Control Tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 查询设备信息（INQUIRY）：自动扫描 /dev/sg*，按 LU NAA 合并控制路径
    Inquiry {
        /// 以 JSON 格式输出（便于程序解析）
        #[arg(long)]
        json: bool,
    },
    /// 显示带库库存状态
    Inventory {
        /// 换带器设备路径 (如 /dev/sg2)
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// catalog 路径（覆盖 $TAPE_RS_CATALOG / XDG 默认）；
        /// 没有 catalog 或查不到时不影响 inventory，仅跳过缓存容量。
        #[arg(long)]
        catalog: Option<String>,
        /// 跳过扫描 /dev/sg* 自动读 drive 实时容量。跳过后仅回落到 catalog。
        #[arg(long)]
        no_drive_scan: bool,
    },
    /// 移动磁带
    Move {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// 源 slot 编号 (1-based)
        #[arg(long)]
        from_slot: u16,
        /// 目标 slot 编号 (1-based) 或 drive 编号
        #[arg(long)]
        to_slot: Option<u16>,
        /// 目标 drive 编号 (0-based)
        #[arg(long)]
        to_drive: Option<u16>,
    },
    /// 装载磁带到驱动器
    Load {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// 源 slot 编号 (1-based)
        #[arg(long)]
        slot: u16,
        /// 目标 drive 编号 (0-based)
        #[arg(long, default_value = "0")]
        drive: u16,
    },
    /// 从驱动器卸载磁带
    Unload {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// 驱动器编号 (0-based)
        #[arg(long, default_value = "0")]
        drive: u16,
        /// 目标 slot 编号 (1-based)
        #[arg(long)]
        slot: u16,
    },
    /// 磁带倒带
    Rewind {
        /// 磁带机 sg 设备路径 (如 /dev/sg1)
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// 读取磁带位置
    Position {
        /// 磁带机 sg 设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// 写入文件到磁带
    Write {
        /// 磁带机 sg 设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 要写入的文件路径
        #[arg(short, long)]
        file: String,
        /// 块大小（字节，默认 512KB）
        #[arg(long, default_value = "524288")]
        block_size: usize,
    },
    /// 从磁带读取文件
    Read {
        /// 磁带机 sg 设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 输出文件路径
        #[arg(short, long)]
        output: String,
        /// 块大小（字节，默认 512KB）
        #[arg(long, default_value = "524288")]
        block_size: usize,
        /// 最大读取字节数，0 表示不设上限（由 filemark / BLANK CHECK 终止）
        #[arg(long, default_value = "0")]
        max_size: u64,
    },
    /// 检查磁带机状态
    Status {
        /// 磁带机 sg 设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// 初始化换带器 element 状态（重新扫描全库）
    Init {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
    },
    /// 三方交换介质（A→B，原 B→C）
    Exchange {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// 源绝对地址（十六进制或十进制）
        #[arg(long)]
        source: String,
        /// 目标1绝对地址
        #[arg(long)]
        dest1: String,
        /// 目标2绝对地址
        #[arg(long)]
        dest2: String,
    },
    /// 锁定/解锁介质移除
    PreventRemoval {
        /// 换带器或磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// true=禁止, false=允许
        #[arg(long, action = clap::ArgAction::Set)]
        prevent: bool,
    },
    /// 通过 I/E 口导入磁带到存储槽
    Import {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// I/E 偏移（0-based）
        #[arg(long)]
        ie: u16,
        /// 目标 slot (1-based)
        #[arg(long)]
        slot: u16,
    },
    /// 从存储槽导出磁带到 I/E 口
    Export {
        /// 换带器设备路径
        #[arg(short, long, default_value = "/dev/sg2")]
        device: String,
        /// 源 slot (1-based)
        #[arg(long)]
        slot: u16,
        /// I/E 偏移（0-based）
        #[arg(long)]
        ie: u16,
    },
    /// 磁带机内部装载（LOAD）
    DriveLoad {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// 磁带机内部弹出（UNLOAD）
    DriveUnload {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// SPACE：跳过 block / filemark / 到 EOD
    Space {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 模式：block | filemark | eod
        #[arg(long, default_value = "filemark")]
        mode: String,
        /// 数量（filemark / block 模式下生效，可为负数表示反向）
        #[arg(long, default_value = "1", allow_hyphen_values = true)]
        count: i32,
    },
    /// LOCATE：快速定位到指定 block
    Locate {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 目标 partition
        #[arg(long, default_value = "0")]
        partition: u8,
        /// 目标 block 号
        #[arg(long)]
        block: u64,
        /// 是否切换 partition
        #[arg(long)]
        change_partition: bool,
    },
    /// ERASE：抹带
    Erase {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 整带长 erase（非常耗时）
        #[arg(long)]
        long: bool,
    },
    /// FORMAT MEDIUM：格式化介质
    Format {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// format 类型：0=默认（单 partition），1=按默认 partition 参数
        #[arg(long, default_value = "0")]
        mode: u8,
        /// 格式化后校验
        #[arg(long)]
        verify: bool,
        /// 确认会擦除磁带；不带此 flag 时拒绝执行
        #[arg(long)]
        yes_destroy: bool,
    },
    /// LOG SENSE：读取 log page（默认读 TapeAlert 0x2E）
    LogSense {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// Log page code（十六进制，如 2E）
        #[arg(long, default_value = "2E")]
        page: String,
        /// 以 hex 打印原始数据
        #[arg(long)]
        raw: bool,
    },
    /// REPORT DENSITY SUPPORT：显示支持的 density
    ReportDensity {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 仅返回当前介质支持的 density
        #[arg(long)]
        media_only: bool,
    },
    /// SEND DIAGNOSTIC：触发自检并读取结果
    Diagnostic {
        /// 磁带机设备路径
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 前台自检（阻塞至完成）
        #[arg(long)]
        foreground: bool,
    },
    /// mkltfs：把磁带初始化成 LTFS 格式（破坏性操作）
    Mkltfs {
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// Volume Identifier（6 字符以内，通常 = barcode）
        #[arg(long)]
        volume_id: String,
        /// Owner Identifier（14 字符以内）
        #[arg(long, default_value = "")]
        owner: String,
        /// 块大小（字节，默认 512KB）
        #[arg(long, default_value = "524288")]
        block_size: u32,
        /// LTFS label 里的 compression 标记
        #[arg(long)]
        compression: bool,
        /// 必须显式加这个参数才会真正执行（避免误操作）
        #[arg(long)]
        yes_destroy: bool,
        /// 快速模式：跳过 MODE SELECT + FORMAT MEDIUM，只重写 labels/index/MAM。
        /// 仅适用于已经是 LTFS 2-partition 格式的磁带；裸带必须先跑普通 mkltfs。
        #[arg(long)]
        quick: bool,
    },
    /// ltfs-list：列出 LTFS 卷的所有文件
    LtfsList {
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
    },
    /// ltfs-read：从 LTFS 卷读取文件到本地
    LtfsRead {
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 磁带上的文件路径（'/' 分隔，不以 '/' 开头）
        #[arg(long)]
        name: String,
        /// 输出本地文件路径
        #[arg(short, long)]
        output: String,
    },
    /// ltfs-write：把本地文件追加写入 LTFS 卷
    LtfsWrite {
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 本地文件路径
        #[arg(short, long)]
        file: String,
        /// 磁带上的目标路径（'/' 分隔）
        #[arg(long)]
        name: String,
    },
    /// 跨盘 catalog（SQLite）：sync / list / find / show
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCmd,
    },
}

#[derive(Subcommand)]
enum CatalogCmd {
    /// 把当前 sg 设备上 mount 的 LTFS 卷灌进 catalog。
    Sync {
        #[arg(short, long, default_value = "/dev/sg1")]
        device: String,
        /// 覆盖 catalog 文件路径（优先级：该参数 > $TAPE_RS_CATALOG > XDG 默认）。
        #[arg(long)]
        catalog: Option<String>,
        /// 手工指定 barcode，跳过 MAM 0x0806 读取。
        #[arg(long)]
        barcode: Option<String>,
    },
    /// 列出 catalog 里所有卷。
    List {
        #[arg(long)]
        catalog: Option<String>,
    },
    /// 按路径模糊搜索文件（支持 SQL LIKE 通配符 `%`、`_`）。
    Find {
        /// 搜索串；不含 `%`/`_` 时按 "包含" 匹配。
        pattern: String,
        #[arg(long)]
        catalog: Option<String>,
        /// 结果上限
        #[arg(long, default_value = "200")]
        limit: usize,
    },
    /// 列出某卷的所有文件（uuid 或 barcode 均可）。
    Show {
        /// 卷的 UUID 或 barcode
        key: String,
        #[arg(long)]
        catalog: Option<String>,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        error!("错误: {}", e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Inquiry { json } => inquiry::cmd_inquiry(json),
        Commands::Inventory { device, catalog, no_drive_scan } => {
            changer::cmd_inventory(&device, catalog.as_deref(), no_drive_scan)
        }
        Commands::Load { device, slot, drive } => changer::cmd_load(&device, slot, drive),
        Commands::Unload { device, drive, slot } => changer::cmd_unload(&device, drive, slot),
        Commands::Move { device, from_slot, to_slot, to_drive } => {
            changer::cmd_move(&device, from_slot, to_slot, to_drive)
        }
        Commands::Init { device } => changer::cmd_init(&device),
        Commands::Exchange { device, source, dest1, dest2 } => {
            changer::cmd_exchange(&device, &source, &dest1, &dest2)
        }
        Commands::PreventRemoval { device, prevent } => changer::cmd_prevent_removal(&device, prevent),
        Commands::Import { device, ie, slot } => changer::cmd_import(&device, ie, slot),
        Commands::Export { device, slot, ie } => changer::cmd_export(&device, slot, ie),
        Commands::Rewind { device } => tape::cmd_rewind(&device),
        Commands::Position { device } => tape::cmd_position(&device),
        Commands::Write { device, file, block_size } => tape::cmd_write(&device, &file, block_size),
        Commands::Read { device, output, block_size, max_size } => {
            tape::cmd_read(&device, &output, block_size, max_size)
        }
        Commands::Status { device } => tape::cmd_status(&device),
        Commands::DriveLoad { device } => tape::cmd_drive_load(&device),
        Commands::DriveUnload { device } => tape::cmd_drive_unload(&device),
        Commands::Space { device, mode, count } => tape::cmd_space(&device, &mode, count),
        Commands::Locate { device, partition, block, change_partition } => {
            tape::cmd_locate(&device, partition, block, change_partition)
        }
        Commands::Erase { device, long } => tape::cmd_erase(&device, long),
        Commands::Format { device, mode, verify, yes_destroy } => {
            tape::cmd_format(&device, mode, verify, yes_destroy)
        }
        Commands::LogSense { device, page, raw } => tape::cmd_log_sense(&device, &page, raw),
        Commands::ReportDensity { device, media_only } => tape::cmd_report_density(&device, media_only),
        Commands::Diagnostic { device, foreground } => tape::cmd_diagnostic(&device, foreground),
        Commands::Mkltfs { device, volume_id, owner, block_size, compression, yes_destroy, quick } => {
            ltfs_cli::cmd_mkltfs(&device, &volume_id, &owner, block_size, compression, yes_destroy, quick)
        }
        Commands::LtfsList { device } => ltfs_cli::cmd_ltfs_list(&device),
        Commands::LtfsRead { device, name, output } => ltfs_cli::cmd_ltfs_read(&device, &name, &output),
        Commands::LtfsWrite { device, file, name } => ltfs_cli::cmd_ltfs_write(&device, &file, &name),
        Commands::Catalog { cmd } => run_catalog(cmd),
    }
}

fn run_catalog(cmd: CatalogCmd) -> Result<()> {
    match cmd {
        CatalogCmd::Sync { device, catalog, barcode } => {
            catalog_cli::cmd_catalog_sync(&device, catalog.as_deref(), barcode.as_deref())
        }
        CatalogCmd::List { catalog } => catalog_cli::cmd_catalog_list(catalog.as_deref()),
        CatalogCmd::Find { pattern, catalog, limit } => {
            catalog_cli::cmd_catalog_find(&pattern, catalog.as_deref(), limit)
        }
        CatalogCmd::Show { key, catalog } => catalog_cli::cmd_catalog_show(&key, catalog.as_deref()),
    }
}
