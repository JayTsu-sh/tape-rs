use clap::Parser;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    about = "离线验证原生 Raft 控制与归档 FDB 的组件证据",
    disable_version_flag = true
)]
struct Args {
    #[arg(long)]
    request_fd: i32,
    #[arg(long)]
    evidence_dir_fd: i32,
}

fn main() -> ExitCode {
    run(Args::parse())
}

#[cfg(feature = "fdb")]
fn run(args: Args) -> ExitCode {
    use std::io::Write;
    use tape_rs::daemon::control_only::archive_verify::verify;
    match verify(args.request_fd, args.evidence_dir_fd) {
        Ok(summary) => match writeln!(std::io::stdout().lock(), "{summary}") {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("组件摘要输出失败：{error}");
                ExitCode::from(4)
            }
        },
        Err(error) => {
            eprintln!("组件验证失败：{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(not(feature = "fdb"))]
fn run(_: Args) -> ExitCode {
    eprintln!("离线组件需要以 fdb feature 构建");
    ExitCode::from(3)
}
