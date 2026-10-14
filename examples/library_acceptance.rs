//! 显式实机验收入口：只读文件，但按 assignment 自动搬带/LOAD/UNLOAD/PR。
//! 不格式化、不写文件、不更改 assignment；目标及预期 SHA 由操作员给出。
use clap::Parser;
use sha2::{Digest, Sha256};
use tape_rs::library_cli::LibraryArgs;

#[derive(Parser)]
#[command(about = "逻辑库实机读回验收：逐卷两轮SHA校验，正常结束归槽；会改变机械位置和PR")]
struct Args {
    #[command(flatten)]
    library: LibraryArgs,
    /// 已分配逻辑路径及预期 SHA256，格式 /池/条码/文件=SHA，可重复
    #[arg(long, required = true)]
    expect: Vec<String>,
    /// 新建本地 JSON 证据文件（拒绝覆盖）
    #[arg(long)]
    report: std::path::PathBuf,
    /// 读回均验证后为目标卷补齐IP检查点；允许写索引，不重放文件上传
    #[arg(long)]
    checkpoint: bool,
}
#[derive(Default)]
struct HashOutput {
    hash: Sha256,
    bytes: u64,
}
impl std::io::Write for HashOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.hash.update(data);
        self.bytes += data.len() as u64;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn run() -> tape_rs::error::Result<()> {
    let args = Args::parse();
    let expected = args
        .expect
        .iter()
        .map(|s| {
            let (p, h) = s
                .rsplit_once('=')
                .ok_or_else(|| tape_rs::error::TapeError::Refused {
                    reason: "expect 格式非法".into(),
                })?;
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(tape_rs::error::TapeError::Refused {
                    reason: "SHA256 必须为64位十六进制".into(),
                });
            }
            Ok((p, h.to_ascii_lowercase()))
        })
        .collect::<tape_rs::error::Result<Vec<_>>>()?;
    let mut report = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(args.report)?;
    let mut lib = args.library.open()?;
    let initial = lib.status()?;
    let mut rows = Vec::new();
    let result = (|| {
        for round in 1..=2 {
            for (path, sha) in &expected {
                let start = std::time::Instant::now();
                let mut output = HashOutput::default();
                let bytes = lib.read_to(path, &mut output)?;
                let actual = format!("{:x}", output.hash.finalize());
                rows.push(serde_json::json!({"round":round,"path":path,"bytes":bytes,"seconds":start.elapsed().as_secs_f64(),"sha256":actual,"expected":sha,"passed":&actual==sha && bytes==output.bytes}));
                if &actual != sha || bytes != output.bytes {
                    return Err(tape_rs::error::TapeError::InconsistentState(format!(
                        "{path} 读回SHA/长度不符"
                    )));
                }
            }
        }
        if args.checkpoint {
            let roots: std::collections::BTreeSet<String> = expected
                .iter()
                .map(|(p, _)| {
                    format!(
                        "/{}",
                        p.trim_start_matches('/')
                            .split('/')
                            .take(2)
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                })
                .collect();
            for root in roots {
                lib.with_volume(&root, |_, _| Ok(()))?;
            }
        }
        Ok(())
    })();
    let cleanup = lib.close();
    let end = lib.status().map_err(|e| e.to_string());
    let json = serde_json::json!({"initial":initial,"reads":rows,"operation_error":result.as_ref().err().map(ToString::to_string),
        "cleanup_error":cleanup.as_ref().err().map(ToString::to_string),"final":end});
    serde_json::to_writer_pretty(&mut report, &json)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    report.sync_all()?;
    result?;
    cleanup
}
fn main() {
    env_logger::init();
    if let Err(e) = run() {
        eprintln!("验收失败: {e}");
        std::process::exit(1);
    }
}
