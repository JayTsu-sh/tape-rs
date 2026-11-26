use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;
use tape_rs::client::Client;
use tape_rs::client::fdb::{
    FdbClient,
    management::{ManagementIntent, ManagementRequestFile},
};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum Command {
    /// 创建、查看多副本池及处理导入冲突
    Replicated {
        #[command(subcommand)]
        cmd: ReplicatedCommand,
    },
    /// 创建空池或指定磁带的普通池
    Create {
        name: String,
        #[arg(long, default_value_t = 200_000)]
        file_limit: u64,
        /// 只读检查池名称和磁带清单
        #[arg(long, requires = "tapes", conflicts_with = "request")]
        preview: bool,
        /// 指定磁带条码，可重复指定
        #[arg(long = "tape")]
        tapes: Vec<String>,
        /// 允许初始化经验证的空白磁带
        #[arg(long, requires = "tapes", conflicts_with = "preview")]
        confirm_format: bool,
        /// 首次调用前保存请求身份；重试须用相同文件和参数
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// 按原 UUID 查询指定磁带的建池任务
    CreateStatus {
        id: Uuid,
        #[arg(long)]
        namespace: Uuid,
    },
    /// 列出当前带库的全部FDB池
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        selector: String,
    },
    /// 提交异步策略修改；属性使用HTTP字段名，例如media_restriction L8
    Set {
        selector: String,
        attribute: String,
        value: String,
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// 提交删除空池任务；目录或磁带归属非空时失败
    Delete {
        selector: String,
        #[arg(long)]
        request: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum ReplicatedCommand {
    /// 从包含原 UUID、底层池范围和条码的 JSON 文件提交创建
    Create {
        #[arg(long)]
        request: PathBuf,
    },
    /// 按 UUID 查看原任务、底层池和物理故障范围
    Show { id: Uuid },
    /// 分页查看已导入路径及各文件的实际副本数
    Files {
        id: Uuid,
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 处理同路径内容冲突，选定原卷的版本
    Resolve {
        id: Uuid,
        #[arg(long)]
        path: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        volume: Uuid,
    },
}

fn run_replicated(
    client: &FdbClient,
    command: ReplicatedCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use tape_rs::replicated_pool::{CreateReplicatedPool, ResolveReplicatedPoolConflict};

    let value = match command {
        ReplicatedCommand::Create { request } => {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&request)?;
            if !file.metadata()?.is_file() {
                return Err("创建多副本池的请求须保存在普通文件中".into());
            }
            file.sync_all()?;
            std::fs::File::open(
                request
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new(".")),
            )?
            .sync_all()?;
            let mut bytes = Vec::new();
            file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 16 * 1024 {
                return Err("多副本池创建请求超过 16 KiB".into());
            }
            let input: CreateReplicatedPool = serde_json::from_slice(&bytes)?;
            client.create_replicated_pool(&input)?
        }
        ReplicatedCommand::Show { id } => client.replicated_pool_status(id)?,
        ReplicatedCommand::Files { id, after, limit } => {
            client.replicated_pool_files(id, after.as_deref(), limit)?
        }
        ReplicatedCommand::Resolve {
            id,
            path,
            revision,
            volume,
        } => client.resolve_replicated_pool_conflict(
            id,
            &ResolveReplicatedPoolConflict {
                version: 1,
                path,
                revision,
                volume_uuid: volume,
            },
        )?,
    };
    print(value)
}

pub(super) fn library<'a>(
    library: Option<&'a str>,
    pool: Option<&str>,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    if pool.is_some() {
        return Err("池管理与管理任务接口不接受全局 --pool".into());
    }
    library.ok_or_else(|| "FDB管理接口必须通过 --library 指定带库序列号".into())
}
pub(super) fn submit(
    client: &FdbClient,
    library: &str,
    logical: Option<Uuid>,
    input: ManagementIntent,
    path: PathBuf,
) -> Result<Value, Box<dyn std::error::Error>> {
    match ManagementRequestFile::load(&path) {
        Ok(saved) => {
            if !saved.matches_pool_scope(library, logical, &input) {
                return Err("请求文件的物理库、逻辑库或参数不同；禁止覆盖原请求".into());
            }
        }
        Err(tape_rs::client::ClientError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            client
                .prepare_pool_management(library, logical, input)?
                .save_new(&path)?;
        }
        Err(e) => return Err(e.into()),
    }
    eprintln!("原管理请求已保存至 {}", path.display());
    let mut result = client.submit_management(&path)?;
    result["request_file"] = json!(path);
    Ok(result)
}
fn items(
    client: &FdbClient,
    library: &str,
    logical: Option<Uuid>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let context = client.management_context(library)?;
    let ns = context["namespace"].as_str().ok_or("缺少目录命名空间")?;
    let mut after = None;
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    loop {
        let page = client.pool_page_in_scope(library, ns, logical, after.as_deref())?;
        items.extend(page["items"].as_array().ok_or("分页清单无效")?.clone());
        after = page["next"].as_str().map(str::to_owned);
        if let Some(cursor) = &after {
            if !seen.insert(cursor.clone()) {
                return Err("服务端重复分页游标".into());
            }
        } else {
            break;
        }
    }
    Ok(json!({"library_serial":library,"logical_library":logical,"namespace":ns,"items":items}))
}
fn print(value: Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
fn display_name(name: &str) -> String {
    name.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub fn run(
    client: &FdbClient,
    serial: Option<&str>,
    pool: Option<&str>,
    logical: Option<Uuid>,
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Command::Replicated { cmd } = command {
        if serial.is_some() || pool.is_some() || logical.is_some() {
            return Err("多副本池使用请求中的各底层池范围，请移除全局带库或池选择".into());
        }
        return run_replicated(client, cmd);
    }
    let library = library(serial, pool)?;
    if logical.is_some_and(|id| id.is_nil()) {
        return Err("逻辑带库 UUID 不能为零".into());
    }
    let result = match command {
        Command::Replicated { .. } => unreachable!("多副本池已分派"),
        Command::Create {
            name,
            file_limit,
            preview: true,
            tapes,
            ..
        } => {
            let context = client.management_context(library)?;
            let namespace = context["namespace"].as_str().ok_or("缺少目录命名空间")?;
            client.preview_pool_create(library, namespace, logical, &name, file_limit, &tapes)?
        }
        Command::Create {
            name,
            file_limit,
            tapes,
            confirm_format,
            request,
            ..
        } if !tapes.is_empty() => submit(
            client,
            library,
            logical,
            ManagementIntent::CreateSelected {
                name,
                file_limit,
                tapes,
                confirm_format,
            },
            request_path(request)?,
        )?,
        Command::Create {
            name,
            file_limit,
            request,
            ..
        } => submit(
            client,
            library,
            logical,
            ManagementIntent::Create { name, file_limit },
            request_path(request)?,
        )?,
        Command::CreateStatus { id, namespace } => {
            client.selected_pool_create_status(library, namespace, logical, id)?
        }
        Command::Set {
            selector,
            attribute,
            value,
            request,
        } => submit(
            client,
            library,
            logical,
            ManagementIntent::Set {
                selector,
                attribute,
                value,
            },
            request_path(request)?,
        )?,
        Command::Delete { selector, request } => submit(
            client,
            library,
            logical,
            ManagementIntent::Delete { selector },
            request_path(request)?,
        )?,
        Command::Show { selector } => {
            let context = client.management_context(library)?;
            client.pool_in_scope(
                library,
                context["namespace"].as_str().ok_or("缺少命名空间")?,
                logical,
                &selector,
            )?
        }
        Command::List { json } => {
            let result = items(client, library, logical)?;
            if json {
                return print(result);
            }
            println!(
                "{:<36}  {:>20}  {:>20}  NAME",
                "UUID", "REVISION", "FILE_LIMIT"
            );
            for pool in result["items"].as_array().ok_or("池清单无效")? {
                let id = pool["id"].as_str().ok_or("池UUID缺失")?;
                let revision = pool["revision"].as_u64().ok_or("池修订号缺失")?;
                let limit = pool["file_limit"].as_u64().ok_or("池文件上限缺失")?;
                let name = pool["name"].as_str().ok_or("池名称缺失")?;
                println!(
                    "{id:<36}  {revision:>20}  {limit:>20}  {}",
                    display_name(name)
                );
            }
            return Ok(());
        }
    };
    print(result)
}

pub(super) fn request_path(path: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = path {
        return Ok(path);
    }
    use std::os::unix::fs::DirBuilderExt;
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".local/state")))
        .ok_or("无法确定本机状态目录，请指定 --request")?
        .join("tape-rs/management");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    Ok(root.join(format!("{}.json", Uuid::new_v4())))
}

pub(super) fn library_from_client(
    control: &mut Client,
    serial: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(serial) = serial {
        return Ok(serial.into());
    }
    let value = control.libraries()?;
    let libraries = value["items"].as_array().ok_or("带库清单无效")?;
    if libraries.len() != 1 {
        return Err("多个带库时必须指定 --library".into());
    }
    Ok(libraries[0]["library_serial"]
        .as_str()
        .ok_or("带库序列号缺失")?
        .into())
}

pub(super) fn selected(
    client: &FdbClient,
    serial: &str,
    selector: &str,
) -> Result<(uuid::Uuid, Value), Box<dyn std::error::Error>> {
    let context = client.management_context(serial)?;
    let ns = context["namespace"].as_str().ok_or("namespace缺失")?;
    let value = client.pool(serial, ns, selector)?;
    Ok((ns.parse()?, value["pool"].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn selected_creation_and_preview_flags_are_distinct() {
        let parse = |extra: &[&str]| {
            let args = [
                "ltfsctl",
                "-e",
                "localhost:1",
                "pool",
                "--library",
                "LIB",
                "create",
                "archive",
            ];
            crate::Args::try_parse_from(args.into_iter().chain(extra.iter().copied()))
        };
        assert!(parse(&[]).is_ok());
        assert!(parse(&["--preview", "--tape", "NEW001L8", "--tape", "NEW002L8"]).is_ok());
        assert!(parse(&["--tape", "NEW001L8"]).is_ok());
        assert!(parse(&["--tape", "NEW001L8", "--confirm-format"]).is_ok());
        assert!(parse(&["--confirm-format"]).is_err());
        assert!(parse(&["--preview", "--tape", "NEW001L8", "--confirm-format"]).is_err());
        assert!(parse(&["--preview", "--tape", "NEW001L8", "--request", "plan.json"]).is_err());
    }

    #[test]
    fn names_cannot_break_table_rows_and_unicode_name_is_last() {
        assert_eq!(display_name("中文\n\t\u{1b}"), "中文\\n\\t\\u{1b}");
    }
}
