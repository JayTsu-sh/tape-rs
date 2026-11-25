use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use nix::fcntl::{FcntlArg, fcntl};
use tape_rs::scsi::reservation::{CaptureOutcome, CaptureRequest, capture_b_pr_evidence};

const MAX_REQUEST: u64 = 4096;

#[derive(Parser)]
#[command(about = "采集固定 B 库 FC 路径和持久预留的只读诊断原件")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 从继承的普通文件 FD 读取采集请求。
    Capture {
        /// 请求 JSON 所在 FD，须大于 2。
        #[arg(long)]
        request_fd: RawFd,
        /// 明确启用只读设备采集。
        #[arg(long)]
        read_only: bool,
    },
}

fn read_request(fd: RawFd) -> Result<CaptureRequest, String> {
    if fd <= 2 {
        return Err("需要独立传入的请求 FD（大于 2）".into());
    }
    let copy = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(3)).map_err(|error| error.to_string())?;
    // SAFETY: F_DUPFD_CLOEXEC 成功分配新 fd，此 File 独占关闭权。
    let file = unsafe { File::from_raw_fd(copy) };
    let before = file.metadata().map_err(|error| error.to_string())?;
    if !before.is_file() || !(1..=MAX_REQUEST).contains(&before.len()) {
        return Err(format!("请求 FD 需要 1 至 {MAX_REQUEST} 字节的普通文件"));
    }
    let mut bytes = vec![0; before.len() as usize];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|error| error.to_string())?;
    let after = file.metadata().map_err(|error| error.to_string())?;
    if before.len() != after.len()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err("读取请求期间文件发生变化".into());
    }
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if raw.to_string().as_bytes() != bytes {
        return Err("请求需要规范 JSON".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn run(args: Args, mut output: impl Write) -> io::Result<u8> {
    let Some(Command::Capture {
        request_fd,
        read_only,
    }) = args.command
    else {
        writeln!(
            output,
            "{{\"mode\":\"plan\",\"profile\":\"b-fc-v1\",\"schema\":\"fresh.stopped.sg-pr.capture.v1\",\"version\":1}}"
        )?;
        return Ok(0);
    };
    let outcome = if read_only {
        match read_request(request_fd) {
            Ok(request) => capture_b_pr_evidence(request),
            Err(error) => CaptureOutcome::invalid_request(error),
        }
    } else {
        CaptureOutcome::invalid_request("设备采集需要显式指定 --read-only")
    };
    if let Some(detail) = outcome.diagnostic() {
        eprintln!("只读采集未完成：{detail}");
    }
    let json = serde_json::to_value(&outcome)?;
    writeln!(output, "{json}")?;
    Ok(outcome.exit_code())
}

fn main() -> ExitCode {
    match run(Args::parse(), io::stdout().lock()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("只读采集输出失败：{error}");
            ExitCode::from(4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_explicit_read_only_is_plan_or_invalid_before_reading_fd() {
        let mut bytes = Vec::new();
        assert_eq!(
            run(Args::try_parse_from(["capture"]).unwrap(), &mut bytes).unwrap(),
            0
        );
        let plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan["mode"], "plan");
        bytes.clear();
        let args = Args::try_parse_from(["capture", "capture", "--request-fd", "999999"]).unwrap();
        assert_eq!(run(args, &mut bytes).unwrap(), 2);
        let rejected: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rejected["classification"], "invalid");
        assert_eq!(rejected["devices"], serde_json::json!([]));
        assert!(
            rejected["failure"]["detail"]
                .as_str()
                .unwrap()
                .contains("--read-only")
        );
    }

    #[test]
    fn rejects_device_cdb_and_service_action_switches() {
        for flag in ["--device", "--cdb", "--service-action"] {
            assert!(
                Args::try_parse_from([
                    "capture",
                    "capture",
                    "--request-fd",
                    "3",
                    "--read-only",
                    flag,
                    "arbitrary"
                ])
                .is_err()
            );
        }
    }
}
