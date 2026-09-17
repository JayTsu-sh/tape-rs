//! ltfsctl：基于 Rust 客户端库的命令行。给出全部节点地址即可，库自己找 Leader。

use clap::{Parser, Subcommand};
use tape_rs::client::Client;

#[derive(Parser)]
#[command(name = "ltfsctl", about = "ltfsd 客户端")]
struct Args {
    /// 节点的客户端接口地址，重复给出
    #[arg(short = 'e', long = "endpoint", required = true)]
    endpoints: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 上传本地文件并等到落带
    Put { file: String, path: String },
    /// 读出已提交的文件
    Get { path: String, output: String },
    /// 查询文件是否已提交
    Stat { path: String },
    /// 列出已提交的全部文件
    List,
    /// 各节点的自述
    Cluster,
}

fn main() {
    let args = Args::parse();
    let mut c = Client::new(args.endpoints.clone());
    let r: Result<(), Box<dyn std::error::Error>> = (|| {
        match args.cmd {
            Cmd::Put { file, path } => {
                let data = std::fs::read(&file)?;
                let o = c.put(&path, &data)?;
                println!(
                    "已提交 {} ({} 字节) 索引代数 {}  请求次数 {}{}",
                    path,
                    data.len(),
                    o.generation,
                    o.attempts,
                    if o.resolved_by_query { "  (经查询判定)" } else { "" }
                );
            }
            Cmd::Get { path, output } => {
                let d = c.get(&path)?;
                std::fs::write(&output, &d)?;
                println!("{} -> {} ({} 字节)", path, output, d.len());
            }
            Cmd::Stat { path } => match c.stat(&path)? {
                Some(s) => println!("已提交  {} 字节  索引代数 {}  执行轮次 {}", s.length, s.generation, s.round),
                None => println!("未提交"),
            },
            Cmd::List => {
                for (p, n) in c.list()? {
                    println!("{:>12}  {}", n, p);
                }
            }
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
