//! Durable operational device requests. Submission never waits for tape work.
use clap::Args;
use serde_json::{Value, json};
use std::path::PathBuf;
use tape_rs::client::{
    Client,
    fdb::{FdbClient, PreparedManagement},
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Args)]
pub struct Targets {
    /// 目标条码；恢复时省略并指定原请求文件
    barcodes: Vec<String>,
    /// 单盘原请求文件，或多盘新请求目录
    #[arg(long)]
    request: Option<PathBuf>,
}
#[derive(Args)]
pub struct Assign {
    #[command(flatten)]
    targets: Targets,
    /// 确认允许初始化/格式化指定介质
    #[arg(long)]
    yes: bool,
    /// 允许格式化已有文件的介质；必须同时 --yes
    #[arg(short = 'f', long)]
    force_format: bool,
}
#[derive(Args)]
pub struct Move {
    #[command(flatten)]
    targets: Targets,
    /// 目的位置；已分池卷出库前须 offline
    #[arg(short='L',long,value_parser=["homeslot","ieslot"])]
    location: Option<String>,
}
#[derive(Args)]
pub struct Offline {
    #[command(flatten)]
    targets: Targets,
    /// 离线说明，1–63 个英文字母、数字、空格、下划线或连字符；默认当天日期
    #[arg(short = 'o', long = "offline-msg")]
    message: Option<String>,
}
#[derive(Args)]
pub struct Set {
    #[command(flatten)]
    targets: Targets,
    /// JSON 属性：blacklisted、online=false、offline_message
    #[arg(long)]
    patch: Option<String>,
}
#[derive(Args)]
pub struct Rescan {
    /// 原请求文件；多库新扫描时为各库请求的目录
    #[arg(long)]
    request: Option<PathBuf>,
}
enum Action {
    Assign { yes: bool, force: bool },
    Move { location: Option<String> },
    Unassign,
    Validate,
    Online,
    Policy { patch: Option<Value> },
}
impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Self::Assign { .. } => "media-assign",
            Self::Move { .. } => "media-move",
            Self::Unassign => "media-unassign",
            Self::Validate => "media-validate",
            Self::Online => "media-online",
            Self::Policy { .. } => "tape",
        }
    }
    fn changes_on_resume(&self) -> bool {
        matches!(
            self,
            Self::Assign { yes: true, .. }
                | Self::Assign { force: true, .. }
                | Self::Move { location: Some(_) }
                | Self::Policy { patch: Some(_) }
        )
    }
}
fn submitted(client: &FdbClient, path: &std::path::Path) -> Result<Value> {
    let task = client.submit_management_saved(path)?;
    Ok(
        json!({"task_id":format!("{}:{}:{}",task["namespace"].as_str().ok_or("任务 namespace 缺失")?,task["epoch"],task["operation"].as_str().ok_or("任务 operation 缺失")?),"request_file":path,"task":task}),
    )
}
fn paths(request: Option<PathBuf>, count: usize) -> Result<(PathBuf, bool)> {
    let supplied = request.is_some();
    let path = super::pool::request_path(request)?;
    if count == 1 {
        return Ok((path, false));
    }
    use std::os::unix::fs::DirBuilderExt;
    let path = if supplied {
        path
    } else {
        path.with_extension("")
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)?;
    Ok((path, true))
}
fn submit_all(client: &FdbClient, requests: Vec<PathBuf>) -> Result<()> {
    let mut failed = false;
    let mut results = Vec::new();
    for path in requests {
        match submitted(client, &path) {
            Ok(result) => {
                failed |= matches!(
                    result["task"]["status"].as_str(),
                    Some("failed" | "cancelled")
                );
                results.push(result);
            }
            Err(error) => {
                failed = true;
                results.push(json!({"request_file":path,"error":error.to_string()}));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"requests":results}))?
    );
    if failed {
        return Err(
            "部分任务失败或受理结果未定；保留原请求并查询 task show，不生成新请求代替".into(),
        );
    }
    Ok(())
}
fn run(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    targets: Targets,
    action: Action,
) -> Result<()> {
    if targets.barcodes.is_empty() {
        if action.changes_on_resume() {
            return Err("恢复任务不得覆盖原操作参数".into());
        }
        let path = targets
            .request
            .ok_or("须指定条码；恢复时使用 --request 原文件")?;
        let original = PreparedManagement::load(&path)?;
        let v = original.value();
        if v["change"]["kind"] != action.kind()
            || library.is_some_and(|s| v["library"] != s)
            || pool.is_some_and(|p| v["change"]["pool"] != p)
        {
            return Err("原任务类型、带库或池与选择不符".into());
        }
        return submit_all(client, vec![path]);
    }
    if targets
        .barcodes
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != targets.barcodes.len()
    {
        return Err("同一批任务不得重复条码".into());
    }
    match &action {
        Action::Assign { yes: false, .. } => {
            return Err(
                "assign 可能初始化并格式化介质，必须显式 --yes；保留已有文件请使用 tape import"
                    .into(),
            );
        }
        Action::Move { location: None } => {
            return Err("tape move 必须指定 -L homeslot|ieslot".into());
        }
        _ => {}
    }
    let library = super::pool::library(control, library)?;
    let (namespace, pool) = if let Some(selector) = pool {
        let (ns, p) = super::pool::selected(client, &library, selector)?;
        (
            ns,
            Some(p["id"].as_str().ok_or("池 UUID 缺失")?.parse::<Uuid>()?),
        )
    } else {
        if !matches!(action, Action::Move { .. }) {
            return Err("该操作必须指定 --pool 池名称或 UUID".into());
        }
        let page = client.list_pools(&library, 100, None)?;
        (
            page["namespace"]
                .as_str()
                .ok_or("namespace 缺失")?
                .parse::<Uuid>()?,
            None,
        )
    };
    let (path, directory) = paths(targets.request, targets.barcodes.len())?;
    let mut saved = Vec::new();
    for barcode in targets.barcodes {
        let current = client.tape_inventory_entry(&library, None, &barcode)?;
        let tape = &current["payload"];
        if tape["cleaning"] == true {
            return Err("清洗带不是数据介质，不能执行该命令".into());
        }
        let assignment = &tape["assignment"];
        let change = match &action {
            Action::Assign { force, .. } => {
                if !assignment.is_null() {
                    return Err("磁带已有池归属，不能 assign".into());
                }
                json!({"kind":action.kind(),"pool":pool.unwrap().to_string(),"barcode":barcode,"volume":Uuid::new_v4().to_string(),"confirm_format":true,"force_format":force})
            }
            Action::Move { location } => {
                if assignment.is_null() {
                    if pool.is_some() {
                        return Err("未分池介质移动时不得指定 --pool".into());
                    }
                    json!({"kind":action.kind(),"pool":null,"barcode":barcode,"volume":null,"revision":null,"destination":location.as_deref().unwrap()})
                } else {
                    if pool.map(|id| id.to_string()).as_deref() != assignment["pool"].as_str() {
                        return Err("已分池磁带须显式选择其所属 --pool".into());
                    }
                    json!({"kind":action.kind(),"pool":assignment["pool"],"barcode":barcode,"volume":assignment["volume"],"revision":assignment["revision"],"destination":location.as_deref().unwrap()})
                }
            }
            Action::Unassign | Action::Validate | Action::Online | Action::Policy { .. } => {
                if assignment.is_null()
                    || assignment["pool"] != pool.unwrap().to_string()
                    || assignment["volume"].is_null()
                {
                    return Err("磁带尚未绑定已核验卷或不属于所选池".into());
                }
                let mut value = json!({"kind":action.kind(),"pool":assignment["pool"],"barcode":barcode,"volume":assignment["volume"],"revision":assignment["revision"]});
                if let Action::Policy { patch } = &action {
                    value["patch"] = patch.clone().ok_or("tape set 必须指定 --patch")?;
                }
                value
            }
        };
        let original = client.prepare_management(&library, namespace, change)?;
        let file = if directory {
            path.join(format!(
                "{}.json",
                original.value()["operation"].as_str().unwrap()
            ))
        } else {
            path.clone()
        };
        original.save_new(&file)?;
        eprintln!("磁带 {barcode} 的原请求已保存至 {}", file.display());
        saved.push(file);
    }
    submit_all(client, saved)
}
pub fn online(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    targets: Targets,
) -> Result<()> {
    run(client, control, library, pool, targets, Action::Online)
}
pub fn offline(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Offline,
) -> Result<()> {
    let patch = if command.targets.barcodes.is_empty() {
        command
            .message
            .map(|message| json!({"online":false,"offline_message":message}))
    } else {
        Some(
            json!({"online":false,"offline_message":command.message.unwrap_or_else(|| chrono::Utc::now().format("%Y%m%d").to_string())}),
        )
    };
    run(
        client,
        control,
        library,
        pool,
        command.targets,
        Action::Policy { patch },
    )
}
pub fn set(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Set,
) -> Result<()> {
    let patch = command
        .patch
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    run(
        client,
        control,
        library,
        pool,
        command.targets,
        Action::Policy { patch },
    )
}
pub fn assign(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Assign,
) -> Result<()> {
    run(
        client,
        control,
        library,
        pool,
        command.targets,
        Action::Assign {
            yes: command.yes,
            force: command.force_format,
        },
    )
}
pub fn move_tape(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Move,
) -> Result<()> {
    run(
        client,
        control,
        library,
        pool,
        command.targets,
        Action::Move {
            location: command.location,
        },
    )
}
pub fn unassign(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Targets,
) -> Result<()> {
    run(client, control, library, pool, command, Action::Unassign)
}
pub fn validate(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Targets,
) -> Result<()> {
    run(client, control, library, pool, command, Action::Validate)
}
pub fn rescan(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    command: Rescan,
) -> Result<()> {
    if let Some(path) = command.request.as_ref().filter(|path| path.is_file()) {
        let original = PreparedManagement::load(path)?;
        if original.value()["change"]["kind"] != "library-rescan"
            || library.is_some_and(|s| original.value()["library"] != s)
        {
            return Err("原扫描请求与所选库或操作不符".into());
        }
        return submit_all(client, vec![path.clone()]);
    }
    let libraries = if let Some(library) = library {
        vec![library.to_owned()]
    } else {
        let value = control.libraries()?;
        value["libraries"]
            .as_array()
            .ok_or("带库列表无效")?
            .iter()
            .map(|row| {
                row["changer_serial"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("库序列号缺失".into())
            })
            .collect::<Result<Vec<_>>>()?
    };
    if libraries.is_empty() {
        return Err("没有配置可扫描的带库".into());
    }
    let (path, directory) = paths(command.request, libraries.len())?;
    let mut saved = Vec::new();
    for library in libraries {
        let page = client.list_pools(&library, 100, None)?;
        let namespace = page["namespace"]
            .as_str()
            .ok_or("namespace 缺失")?
            .parse::<Uuid>()?;
        let original =
            client.prepare_management(&library, namespace, json!({"kind":"library-rescan"}))?;
        let file = if directory {
            path.join(format!(
                "{}.json",
                original.value()["operation"].as_str().unwrap()
            ))
        } else {
            path.clone()
        };
        original.save_new(&file)?;
        eprintln!("带库 {library} 的扫描原请求已保存至 {}", file.display());
        saved.push(file);
    }
    submit_all(client, saved)
}
