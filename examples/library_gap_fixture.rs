//! 受控实机夹具：只向指定已分配测试卷追加独立目录/文件与标准 root xattr。
//! 不格式化或删除原文件；--write-test-data 为显式写入门槛。
use clap::Parser;
use tape_rs::error::{Result, TapeError};
use tape_rs::library_cli::LibraryArgs;
#[derive(Parser)]
#[command(about = "建立 gap 测试夹具：追加独立文件和可选 65MiB 索引，不格式化")]
struct Args {
    #[command(flatten)]
    library: LibraryArgs,
    /// 已分配卷根路径，格式 /池/条码
    #[arg(long)]
    volume: String,
    /// 独立的新建测试目录名（拒绝覆盖）
    #[arg(long)]
    directory: String,
    /// 在根索引中新增1040个64KiB标准文本xattr，使构造超过64MiB
    #[arg(long)]
    large_index: bool,
    /// 显式允许向测试带写入目录/文件/索引
    #[arg(long)]
    write_test_data: bool,
}
fn run() -> Result<()> {
    let args = Args::parse();
    if !args.write_test_data {
        return Err(TapeError::Refused {
            reason: "测试夹具会写带，必须指定 --write-test-data".into(),
        });
    }
    if args.directory.is_empty()
        || args.directory.contains('/')
        || args.directory == "."
        || args.directory == ".."
    {
        return Err(TapeError::Refused {
            reason: "目录名必须是单个新路径分量".into(),
        });
    }
    let mut lib = args.library.open()?;
    let (_, relative) = lib.resolve(&args.volume)?;
    if !relative.is_empty() {
        return Err(TapeError::Refused {
            reason: "volume 必须是卷根".into(),
        });
    }
    let result = lib.with_volume(&args.volume, |vol, _| {
        vol.create_directory(&args.directory)?;
        let path = format!("{}/marker.bin", args.directory);
        vol.append_bytes(&path, b"tape-rs logical library gap acceptance 20261014\n")?;
        if args.large_index {
            let value = "x".repeat(65536);
            for i in 0..1040 {
                vol.set_root_xattr(&format!("tapers.gap.20261014.{i:04}"), &value)?;
            }
        }
        println!("新增测试路径: {}/{}", args.volume, path);
        Ok(())
    });
    let closed = lib.close();
    result?;
    closed
}
fn main() {
    env_logger::init();
    if let Err(e) = run() {
        eprintln!("夹具创建失败: {e}");
        std::process::exit(1);
    }
}
