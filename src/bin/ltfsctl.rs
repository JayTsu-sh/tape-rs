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
}

#[derive(Subcommand)]
enum TapeCmd {
    /// 把磁带（条码）归入池（名称或 UUID）
    Assign { barcode: String, pool: String },
    /// 解除归属
    Unassign { barcode: String },
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
            Cmd::Pool { cmd: PoolCmd::Create { name, file_limit } } => {
                println!("已创建池 {}  UUID {}", name, c.pool_create(&name, file_limit)?);
            }
            Cmd::Pool { cmd: PoolCmd::List { all_nodes } } => {
                let targets: Vec<Option<String>> =
                    if all_nodes { args.endpoints.iter().cloned().map(Some).collect() } else { vec![None] };
                for t in targets {
                    if let Some(a) = &t {
                        println!("== {}", a);
                    }
                    match c.pools(t.as_deref()) {
                        Ok(pools) if pools.is_empty() => println!("  (没有池)"),
                        Ok(pools) => {
                            for p in pools {
                                println!("  {}  {}  文件数上限 {}  磁带 [{}]", p.name, p.uuid, p.file_limit, p.tapes.join(", "));
                            }
                        }
                        Err(e) => println!("  不可达: {}", e),
                    }
                }
            }
            Cmd::Tape { cmd: TapeCmd::Assign { barcode, pool } } => println!("{}", c.tape_assign(&barcode, &pool)?),
            Cmd::Tape { cmd: TapeCmd::Unassign { barcode } } => println!("{}", c.tape_unassign(&barcode)?),
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
