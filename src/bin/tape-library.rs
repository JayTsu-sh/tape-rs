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
#[derive(Subcommand, Debug)]
enum Command {
    /// 查询/取消本地持久任务；只需 --library-state，不打开设备
    Task {
        #[command(subcommand)]
        action: TaskCommand,
    },
    /// 查询库存、assignment、blacklist 和 drive 资源；不搬带
    Status,
    /// 查询单逻辑库与重新读取库存（不初始化机械手）
    Library {
        #[command(subcommand)]
        action: LibraryCommand,
    },
    /// 管理独立池实体；不格式化或写磁带
    Pool {
        #[command(subcommand)]
        action: PoolCommand,
    },
    /// 查询磁带、离线/上线；上线会装带验证 LTFS 索引
    Tape {
        #[command(subcommand)]
        action: TapeCommand,
    },
    /// 查询和启停本实例驱动调度资格（不是硬件断电）
    Drive {
        #[command(subcommand)]
        action: DriveCommand,
    },
    /// 单进程本地节点组与驱动归属；不是 HA 节点注册
    Nodegroup {
        #[command(subcommand)]
        action: NodegroupCommand,
    },
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
    /// 追加文件并完整提交；默认拒绝覆盖，不格式化磁带
    Write {
        path: String,
        input: std::path::PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCommand {
    List,
    Show {
        id: i64,
    },
    Cancel {
        id: i64,
    },
    /// 只清理终态历史，保留运行中任务；不可恢复
    Clearhistory {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum LibraryCommand {
    List,
    Show,
    Rescan,
}
#[derive(Subcommand, Debug)]
enum PoolCommand {
    Create {
        name: String,
    },
    Delete {
        name: String,
    },
    List,
    Show {
        name: String,
    },
    Set {
        name: String,
        attribute: String,
        value: String,
    },
}
#[derive(Subcommand, Debug)]
enum TapeCommand {
    List,
    Show {
        barcode: String,
    },
    Offline {
        barcode: String,
        message: String,
    },
    Online {
        barcode: String,
    },
    /// 机械搬至 homeslot/ieslot；出 I/E 前必须先 offline
    Move {
        barcode: String,
        location: String,
    },
    /// 挂载检查，--full-read 逐文件读取并检查已有哈希
    Validate {
        barcode: String,
        #[arg(long)]
        full_read: bool,
    },
    /// 从磁带刷新本地索引缓存，不做 GPFS/HSM 对账
    Reconcile {
        barcode: String,
    },
    /// 解释 EE 保存对象的原类型、路径和链接
    Objects {
        barcode: String,
    },
    /// 从 EE 保存对象恢复普通文件/目录/链接到全新目录，不创建 HSM stub
    Restore {
        barcode: String,
        destination: std::path::PathBuf,
    },
}
#[derive(Subcommand, Debug)]
enum DriveCommand {
    List,
    Show {
        serial: String,
    },
    Up {
        serial: String,
    },
    Down {
        serial: String,
    },
    Role {
        serial: String,
        role: String,
    },
    /// 分配本地驱动到节点组，- 表示解除分配
    Assign {
        serial: String,
        group: String,
    },
}

#[derive(Subcommand, Debug)]
enum NodegroupCommand {
    Create {
        name: String,
    },
    Delete {
        name: String,
    },
    List,
    Show {
        name: String,
    },
    /// 指定此本地控制器所属组，- 表示停用本地调度
    Local {
        name: String,
    },
}

fn query(
    command: &Command,
    status: &serde_json::Value,
) -> tape_rs::error::Result<Option<serde_json::Value>> {
    use serde_json::json;
    let refuse = || tape_rs::error::TapeError::Refused {
        reason: "未知资源".into(),
    };
    let payload = match command {
        Command::Status => return Ok(Some(status.clone())),
        Command::Library {
            action: LibraryCommand::List | LibraryCommand::Show | LibraryCommand::Rescan,
        } => status.clone(),
        Command::Tape {
            action: TapeCommand::List,
        } => status["volumes"].clone(),
        Command::Tape {
            action: TapeCommand::Show { barcode },
        } => status["volumes"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["barcode"].as_str() == Some(barcode))
            })
            .cloned()
            .ok_or_else(refuse)?,
        Command::Drive {
            action: DriveCommand::List,
        } => status["drives"].clone(),
        Command::Drive {
            action: DriveCommand::Show { serial },
        } => status["drives"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item["serial"].as_str().is_some_and(|id| {
                        id == serial || id.split_ascii_whitespace().any(|part| part == serial)
                    })
                })
            })
            .cloned()
            .ok_or_else(refuse)?,
        Command::Pool {
            action: PoolCommand::List,
        } => status["pools"].clone(),
        Command::Pool {
            action: PoolCommand::Show { name },
        } => status["pools"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["name"].as_str() == Some(name))
            })
            .cloned()
            .ok_or_else(refuse)?,
        Command::Nodegroup {
            action: NodegroupCommand::List,
        } => status["nodegroups"].clone(),
        Command::Nodegroup {
            action: NodegroupCommand::Show { name },
        } => status["nodegroups"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["name"].as_str() == Some(name))
            })
            .cloned()
            .ok_or_else(refuse)?,
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"source":status["source"],"observed_at":status["observed_at"],"payload":payload}),
    ))
}

fn read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Status
            | Command::Library {
                action: LibraryCommand::List | LibraryCommand::Show
            }
            | Command::Tape {
                action: TapeCommand::List | TapeCommand::Show { .. }
            }
            | Command::Drive {
                action: DriveCommand::List | DriveCommand::Show { .. }
            }
            | Command::Pool {
                action: PoolCommand::List | PoolCommand::Show { .. }
            }
            | Command::Nodegroup {
                action: NodegroupCommand::List | NodegroupCommand::Show { .. }
            }
    )
}
fn main() {
    env_logger::init();
    let args = Args::parse();
    let result = (|| -> tape_rs::error::Result<()> {
        if let Command::Task { action } = &args.command {
            let path = args.library.library_state.as_deref().ok_or_else(|| {
                tape_rs::error::TapeError::Refused {
                    reason: "任务命令必须指定 --library-state".into(),
                }
            })?;
            if !path.is_file() {
                return Err(tape_rs::error::TapeError::Refused {
                    reason: "资源库不存在".into(),
                });
            }
            let tasks = tape_rs::library::tasks::TaskStore::open(path)?;
            let value = match action {
                TaskCommand::List => {
                    serde_json::Value::Array(tasks.list()?.iter().map(|t| t.to_json()).collect())
                }
                TaskCommand::Show { id } => tasks.show(*id)?.to_json(),
                TaskCommand::Cancel { id } => {
                    serde_json::json!({"id":id,"cancel_requested":tasks.cancel(*id)?})
                }
                TaskCommand::Clearhistory { yes } => {
                    if !yes {
                        return Err(tape_rs::error::TapeError::Refused {
                            reason: "清理不可恢复的任务历史须显式 --yes".into(),
                        });
                    }
                    serde_json::json!({"cleared":tasks.clear_history()?})
                }
            };
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            return Ok(());
        }
        if read_only(&args.command)
            && let Some(state) = &args.library.library_state
            && let Some(snapshot) = tape_rs::library::cached_status_if_active(state)?
        {
            if snapshot["library_serial"].as_str() != args.library.library_changer_serial.as_deref()
            {
                return Err(tape_rs::error::TapeError::Refused {
                    reason: "快照changer序列号不匹配".into(),
                });
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&query(&args.command, &snapshot)?.unwrap()).unwrap()
            );
            return Ok(());
        }
        let mut lib = args.library.open()?;
        if !read_only(&args.command) {
            let id = lib.begin_task(
                &format!("{:?}", args.command),
                args.library.library_changer_serial.as_deref().unwrap_or(""),
            )?;
            eprintln!("任务 {id} 已持久登记");
        }
        let operation: tape_rs::error::Result<()> = (|| {
            lib.check_task(0, "命令开始前检查")?;
            if read_only(&args.command)
                || matches!(
                    &args.command,
                    Command::Library {
                        action: LibraryCommand::Rescan
                    }
                )
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&query(&args.command, &lib.status()?)?.unwrap())
                        .unwrap()
                );
                return Ok(());
            }
            match args.command {
                Command::Task { .. } => unreachable!(),
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
                Command::Write {
                    path,
                    input,
                    overwrite,
                } => {
                    let mut input = std::fs::File::open(input)?;
                    lib.write_from(&path, &mut input, !overwrite)?;
                }
                Command::Pool { action } => match action {
                    PoolCommand::Create { name } => lib.pool_create(&name)?,
                    PoolCommand::Delete { name } => lib.pool_delete(&name)?,
                    PoolCommand::Set {
                        name,
                        attribute,
                        value,
                    } => lib.pool_set(&name, &attribute, &value)?,
                    PoolCommand::List | PoolCommand::Show { .. } => unreachable!(),
                },
                Command::Tape { action } => match action {
                    TapeCommand::Offline { barcode, message } => {
                        lib.tape_offline(&barcode, &message)?
                    }
                    TapeCommand::Online { barcode } => lib.tape_online(&barcode)?,
                    TapeCommand::Move { barcode, location } => {
                        println!("{}", lib.tape_move(&barcode, &location)?);
                    }
                    TapeCommand::Validate { barcode, full_read } => {
                        println!("{}", lib.tape_validate(&barcode, full_read)?);
                    }
                    TapeCommand::Reconcile { barcode } => {
                        println!("{}", lib.tape_reconcile(&barcode)?);
                    }
                    TapeCommand::Objects { barcode } => {
                        let path = volume_path(&lib, &barcode)?;
                        lib.inspect_volume(&path, |volume, _| {
                            let manifest = tape_rs::ltfs::ee::EeManifest::from_index(volume.index())?;
                            let objects: Vec<_> = manifest.objects().iter().map(|o| {
                                let (kind, target) = match &o.kind {
                                    tape_rs::ltfs::ee::EeObjectKind::Directory => ("directory", None),
                                    tape_rs::ltfs::ee::EeObjectKind::RegularFile => ("file", None),
                                    tape_rs::ltfs::ee::EeObjectKind::Symlink { target } => ("symlink", Some(target)),
                                };
                                serde_json::json!({"object_path":o.object_path,"saved_path":o.saved_path,"aliases":o.aliases,"kind":kind,"target":target,"length":o.length})
                            }).collect();
                            println!("{}", serde_json::json!({"scope":"ee_saved_object_view","objects":objects}));
                            Ok(())
                        })?;
                    }
                    TapeCommand::Restore {
                        barcode,
                        destination,
                    } => {
                        let path = volume_path(&lib, &barcode)?;
                        lib.inspect_volume(&path, |volume, _| {
                            let report = tape_rs::ltfs::ee::restore(volume, &destination)?;
                            println!("{}", serde_json::json!({"scope":"ordinary_filesystem_not_hsm","directories":report.directories,"regular_files":report.regular_files,"symlinks":report.symlinks,"bytes_written":report.bytes_written}));
                            Ok(())
                        })?;
                    }
                    TapeCommand::List | TapeCommand::Show { .. } => unreachable!(),
                },
                Command::Drive { action } => match action {
                    DriveCommand::Up { serial } => lib.drive_set_enabled(&serial, true)?,
                    DriveCommand::Down { serial } => lib.drive_set_enabled(&serial, false)?,
                    DriveCommand::Role { serial, role } => lib.drive_set_role(&serial, &role)?,
                    DriveCommand::Assign { serial, group } => {
                        lib.drive_set_nodegroup(&serial, (group != "-").then_some(group.as_str()))?
                    }
                    DriveCommand::List | DriveCommand::Show { .. } => unreachable!(),
                },
                Command::Nodegroup { action } => match action {
                    NodegroupCommand::Create { name } => lib.nodegroup_create(&name)?,
                    NodegroupCommand::Delete { name } => lib.nodegroup_delete(&name)?,
                    NodegroupCommand::Local { name } => {
                        lib.nodegroup_set_local((name != "-").then_some(name.as_str()))?
                    }
                    NodegroupCommand::List | NodegroupCommand::Show { .. } => unreachable!(),
                },
                Command::Library { .. } => unreachable!(),
            }
            Ok(())
        })();
        let cleanup = lib.close();
        if let Err(error) = &cleanup {
            eprintln!("设备收尾失败，须核实介质位置及预留: {error}");
        }
        let cleanup_message = cleanup.as_ref().err().map(ToString::to_string);
        let outcome = operation.and(cleanup);
        let mut message = outcome
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_else(|| "命令完成且设备已正常收尾".into());
        if let Some(error) = cleanup_message {
            message.push_str(&format!("；设备收尾失败: {error}"));
        }
        let recorded = lib.finish_task(outcome.is_ok(), &message);
        if let Err(error) = &recorded {
            eprintln!("保存任务终态失败: {error}");
        }
        outcome?;
        recorded.map(|_| ())
    })();
    if let Err(e) = result {
        eprintln!("逻辑库操作失败: {e}");
        std::process::exit(1);
    }
}

fn volume_path<
    C: tape_rs::scsi::transport::TapeTransport,
    D: tape_rs::scsi::transport::TapeTransport,
>(
    library: &tape_rs::library::Library<C, D>,
    barcode: &str,
) -> tape_rs::error::Result<String> {
    for pool in library.pools()? {
        if library.volumes(&pool)?.iter().any(|b| b == barcode) {
            return Ok(format!("/{pool}/{barcode}"));
        }
    }
    Err(tape_rs::error::TapeError::Refused {
        reason: "NEED_ASSIGN".into(),
    })
}
