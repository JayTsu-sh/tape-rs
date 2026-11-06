//! Operational pool commands use the FDB library/pool identity domain.
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tape_rs::client::{
    Client,
    fdb::{FdbClient, PreparedManagement},
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum Command {
    /// 创建带库内的池；先持久保存池 UUID，响应丢失后用 --request 原文件恢复
    Create {
        name: Option<String>,
        #[arg(long)]
        id: Option<Uuid>,
        #[arg(long)]
        file_limit: Option<u64>,
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// 列出所选带库的全部池
    List {
        #[command(flatten)]
        output: super::inventory::ListOutput,
    },
    /// 按池名称或 UUID 查看详情
    Show { selector: String },
    /// 仅删除空池；不卸带、不擦带，先保存原请求
    Delete {
        selector: Option<String>,
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// 以原修订号修改池策略；先保存原请求
    Set {
        selector: Option<String>,
        #[arg(long)]
        patch: Option<String>,
        #[arg(long)]
        request: Option<PathBuf>,
    },
}

pub(super) fn request_path(path: Option<PathBuf>) -> Result<PathBuf> {
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
fn save(value: &Value, path: &Path) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(format!("{value}\n").as_bytes())?;
    file.sync_all()?;
    std::fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}
fn load(path: &Path) -> Result<Value> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 4096 {
        return Err("池创建请求文件无效或过大".into());
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(4097)
        .read_to_end(&mut bytes)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    if bytes.strip_suffix(b"\n").unwrap_or(&bytes) != value.to_string().as_bytes() {
        return Err("池创建请求须为规范 JSON".into());
    }
    let fields = [
        "version",
        "library",
        "namespace",
        "pool",
        "name",
        "file_limit",
    ];
    if value["version"] != 1
        || !value.as_object().is_some_and(|o| {
            o.len() == fields.len() && o.keys().all(|k| fields.contains(&k.as_str()))
        })
    {
        return Err("池创建请求字段无效".into());
    }
    Ok(value)
}
pub(super) fn library(control: &mut Client, serial: Option<&str>) -> Result<String> {
    if let Some(serial) = serial {
        return Ok(serial.into());
    }
    let value = control.libraries()?;
    let libraries = value["libraries"].as_array().ok_or("带库清单无效")?;
    if libraries.len() != 1 {
        return Err("多个带库时必须指定 --library".into());
    }
    Ok(libraries[0]["changer_serial"]
        .as_str()
        .ok_or("带库序列号缺失")?
        .into())
}
fn pools(client: &FdbClient, serial: &str) -> Result<Value> {
    let mut value = client.list_pools(serial, 100, None)?;
    let mut items = value["items"].as_array().ok_or("池清单无效")?.clone();
    let mut next = value["next"].as_str().map(str::to_owned);
    while let Some(cursor) = next {
        let page = client.list_pools(serial, 100, Some(&cursor))?;
        if page["namespace"] != value["namespace"] {
            return Err("查询期间命名空间改变，请重查".into());
        }
        items.extend(
            page["items"]
                .as_array()
                .ok_or("池清单无效")?
                .iter()
                .cloned(),
        );
        next = page["next"].as_str().map(str::to_owned);
        if next.as_deref() == Some(&cursor) {
            return Err("池游标未推进".into());
        }
    }
    value["items"] = json!(items);
    value["next"] = Value::Null;
    Ok(value)
}
pub(super) fn selected(client: &FdbClient, serial: &str, selector: &str) -> Result<(Uuid, Value)> {
    let page = pools(client, serial)?;
    let id = Uuid::parse_str(selector).ok();
    let found: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| id.map_or(p["name"] == selector, |id| p["id"] == id.to_string()))
        .collect();
    if found.len() != 1 {
        return Err("所选带库内没有唯一匹配的池".into());
    }
    Ok((
        page["namespace"]
            .as_str()
            .ok_or("namespace 缺失")?
            .parse()?,
        found[0].clone(),
    ))
}
fn resume(
    client: &FdbClient,
    serial: Option<&str>,
    pool: Option<&str>,
    request: Option<PathBuf>,
    kind: &str,
) -> Result<()> {
    let path = request.ok_or("恢复时必须指定 --request 原文件")?;
    let original = PreparedManagement::load(&path)?;
    let v = original.value();
    if v["change"]["kind"] != kind
        || serial.is_some_and(|s| v["library"] != s)
        || pool.is_some_and(|p| v["change"]["id"] != p)
    {
        return Err("原池请求的类型、带库或池不符".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&client.run_management_saved(&path)?)?
    );
    Ok(())
}
pub fn run(
    client: &FdbClient,
    control: &mut Client,
    serial: Option<&str>,
    pool: Option<&str>,
    command: Command,
) -> Result<()> {
    match command {
        Command::Create {
            name: None,
            id,
            file_limit,
            request,
        } => {
            if id.is_some() || file_limit.is_some() {
                return Err("恢复池创建不得覆盖原参数".into());
            }
            let path = request.ok_or("创建池须指定名称；恢复时指定 --request 原文件")?;
            let v = load(&path)?;
            let library = v["library"].as_str().ok_or("原请求缺少带库")?;
            if serial.is_some_and(|s| s != library) || pool.is_some_and(|p| v["pool"] != p) {
                return Err("原池创建请求与作用域不符".into());
            }
            let result = client.create_pool(
                library,
                v["namespace"].as_str().ok_or("namespace 缺失")?.parse()?,
                v["pool"].as_str().ok_or("池 UUID 缺失")?.parse()?,
                v["name"].as_str().ok_or("池名缺失")?,
                v["file_limit"].as_u64().ok_or("文件数上限缺失")?,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"request_file":path,"result":result}))?
            );
            Ok(())
        }
        Command::Delete {
            selector: None,
            request,
        } => resume(client, serial, pool, request, "pool-delete"),
        Command::Set {
            selector: None,
            patch,
            request,
        } => {
            if patch.is_some() {
                return Err("恢复池修改不得覆盖原 patch".into());
            }
            resume(client, serial, pool, request, "pool")
        }
        command => {
            let serial = library(control, serial)?;
            match command {
                Command::Create {
                    name: Some(name),
                    id,
                    file_limit,
                    request,
                } => {
                    if pool.is_some() {
                        return Err("创建池用 --id 固定 UUID，不接受 --pool".into());
                    }
                    let page = pools(client, &serial)?;
                    let namespace: Uuid = page["namespace"]
                        .as_str()
                        .ok_or("namespace 缺失")?
                        .parse()?;
                    let id = id.unwrap_or_else(Uuid::new_v4);
                    let file_limit = file_limit.unwrap_or(200_000);
                    let path = request_path(request)?;
                    let value = json!({"version":1,"library":serial,"namespace":namespace.to_string(),"pool":id.to_string(),"name":name,"file_limit":file_limit});
                    save(&value, &path)?;
                    eprintln!("池创建原请求已保存至 {}", path.display());
                    let result = client.create_pool(&serial, namespace, id, &name, file_limit)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"request_file":path,"result":result})
                        )?
                    );
                    Ok(())
                }
                Command::List { output } => {
                    if pool.is_some() {
                        return Err("pool list 不接受 --pool；使用 pool show 查看单池".into());
                    }
                    super::inventory::pools(pools(client, &serial)?, output);
                    Ok(())
                }
                Command::Show { selector } => {
                    let (namespace, p) = selected(client, &serial, &selector)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"library_serial":serial,"namespace":namespace.to_string(),"pool":p})
                        )?
                    );
                    Ok(())
                }
                Command::Delete {
                    selector: Some(selector),
                    request,
                } => {
                    let (namespace, p) = selected(client, &serial, &selector)?;
                    if pool.is_some_and(|pool| p["id"] != pool) {
                        return Err("选择的池与 --pool 不一致".into());
                    }
                    let prepared = client.prepare_management(
                        &serial,
                        namespace,
                        json!({"kind":"pool-delete","id":p["id"],"revision":p["revision"]}),
                    )?;
                    let path = request_path(request)?;
                    prepared.save_new(&path)?;
                    eprintln!("池删除原请求已保存至 {}", path.display());
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.run_management_saved(&path)?)?
                    );
                    Ok(())
                }
                Command::Set {
                    selector: Some(_),
                    patch: None,
                    ..
                } => Err("pool set 必须指定 --patch".into()),
                Command::Set {
                    selector: Some(selector),
                    patch: Some(patch),
                    request,
                } => {
                    let (namespace, p) = selected(client, &serial, &selector)?;
                    if pool.is_some_and(|pool| p["id"] != pool) {
                        return Err("选择的池与 --pool 不一致".into());
                    }
                    let patch: Value = serde_json::from_str(&patch)?;
                    let prepared = client.prepare_management(
                        &serial,
                        namespace,
                        json!({"kind":"pool","id":p["id"],"revision":p["revision"],"patch":patch}),
                    )?;
                    let path = request_path(request)?;
                    prepared.save_new(&path)?;
                    eprintln!("池修改原请求已保存至 {}", path.display());
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.run_management_saved(&path)?)?
                    );
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
    }
}
