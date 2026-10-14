//! 显式逻辑库资源管理与直接 Rust 库访问入口。
use clap::{Parser, Subcommand};
use tape_rs::library_cli::LibraryArgs;

#[derive(Parser)]
#[command(about = "管理多卷逻辑库；显式 assignment 后才允许按路径自动装带")]
struct Args {
    #[command(flatten)]
    library: LibraryArgs,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 查询库存、assignment、blacklist 和 drive 资源；不搬带
    Status,
    /// 将已知条码分配到逻辑池；不写介质
    Assign { pool: String, barcode: String },
    /// 取消已归槽卷的 assignment
    Unassign { barcode: String },
    /// 禁止卷访问并保存原因
    Blacklist { barcode: String, reason: String },
    /// 显式清除 blacklist
    ClearBlacklist { barcode: String },
    /// 按逻辑路径读取文件到本地；会自动装带，结束后归槽
    Read {
        path: String,
        output: std::path::PathBuf,
    },
    /// 列举逻辑卷中的文件；会自动装带，结束后归槽
    List { path: String },
}
fn main() {
    env_logger::init();
    let args = Args::parse();
    let result = (|| -> tape_rs::error::Result<()> {
        if matches!(&args.command, Command::Status)
            && let Some(state) = &args.library.library_state
            && let Some(snapshot) = tape_rs::library::cached_status_if_active(state)?
        {
            if snapshot["library_serial"].as_str() != args.library.library_changer_serial.as_deref()
            {
                return Err(tape_rs::error::TapeError::Refused {
                    reason: "快照changer序列号不匹配".into(),
                });
            }
            println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
            return Ok(());
        }
        let mut lib = args.library.open()?;
        let operation: tape_rs::error::Result<()> = (|| {
            match args.command {
                Command::Status => {
                    println!("{}", serde_json::to_string_pretty(&lib.status()?).unwrap())
                }
                Command::Assign { pool, barcode } => lib.assign(&pool, &barcode)?,
                Command::Unassign { barcode } => lib.unassign(&barcode)?,
                Command::Blacklist { barcode, reason } => lib.blacklist(&barcode, Some(&reason))?,
                Command::ClearBlacklist { barcode } => lib.blacklist(&barcode, None)?,
                Command::Read { path, output } => {
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(output)?;
                    lib.read_to(&path, &mut f)?;
                    f.sync_all()?;
                }
                Command::List { path } => lib.inspect_volume(&path, |v, p| {
                    for (name, len) in v.list() {
                        if p.is_empty()
                            || name.starts_with(&format!("{}/", p.trim_end_matches('/')))
                        {
                            println!("{len}\t{name}");
                        }
                    }
                    Ok(())
                })?,
            }
            Ok(())
        })();
        let cleanup = lib.close();
        operation?;
        cleanup
    })();
    if let Err(e) = result {
        eprintln!("逻辑库操作失败: {e}");
        std::process::exit(1);
    }
}
