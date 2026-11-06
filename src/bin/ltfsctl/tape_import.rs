use clap::Args;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tape_rs::client::{
    Client,
    fdb::{FdbClient, PreparedManagement},
};
use uuid::Uuid;

#[derive(Args)]
pub struct Command {
    /// 要导入的条码；使用 --request 恢复原请求时省略
    barcodes: Vec<String>,
    /// 单盘请求文件或多盘请求目录；省略时在本机状态目录自动保存
    #[arg(long)]
    request: Option<PathBuf>,
    /// 首次导入的池内目录，默认 /IMPORT；已分配卷刷新沿用原路径映射
    #[arg(short = 'P', long)]
    path: Option<String>,
    /// 首次导入直接合并到目录，不追加条码；刷新不另建导入目录
    #[arg(short = 'R', long)]
    merge_tapes: bool,
}

fn result(client: &FdbClient, path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let task = client.submit_management_saved(path)?;
    Ok(
        json!({"task_id":tape_rs::management_task_id::ManagementTaskId::from_receipt(&task)?.to_string(),"request_file":path,"task":task}),
    )
}

pub fn run(
    client: &FdbClient,
    control: &mut Client,
    library: Option<&str>,
    pool: Option<&str>,
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    if command.barcodes.is_empty() {
        if command.path.is_some() || command.merge_tapes {
            return Err("恢复任务只能使用原请求，不得覆盖 --path 或 --merge-tapes".into());
        }
        let path = command
            .request
            .ok_or("导入须指定条码；恢复时使用 --request 原请求文件")?;
        let request = PreparedManagement::load(&path)?;
        if request.value()["change"]["kind"] != "media-import" {
            return Err("该文件不是磁带导入请求".into());
        }
        if library.is_some_and(|s| request.value()["library"] != s) {
            return Err("所选带库与原请求不符".into());
        }
        if let Some(pool) = pool
            && request.value()["change"]["pool"] != pool
        {
            return Err("恢复请求时 --pool 必须与原池 UUID 一致".into());
        }
        let value = result(client, &path)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        if matches!(
            value["task"]["status"].as_str(),
            Some("failed" | "cancelled")
        ) {
            return Err("原导入任务已失败或取消，未创建新任务".into());
        }
        return Ok(());
    }
    let unique: std::collections::BTreeSet<_> = command.barcodes.iter().collect();
    if unique.len() != command.barcodes.len() {
        return Err("同一批导入不得重复指定条码".into());
    }
    let library = match library {
        Some(s) => s.to_owned(),
        None => {
            let value = control.libraries()?;
            let libraries = value["items"].as_array().ok_or("带库清单无效")?;
            if libraries.len() != 1 {
                return Err("多个带库时必须指定 --library".into());
            }
            libraries[0]["library_serial"]
                .as_str()
                .ok_or("带库缺少序列号")?
                .to_owned()
        }
    };
    let selector = pool.ok_or("导入必须指定 --pool 池名称或 UUID")?;
    let id = Uuid::parse_str(selector).ok();
    let mut after = None;
    let mut scope = None;
    let mut target = None;
    loop {
        let page = client.list_pools(&library, 100, after.as_deref())?;
        let namespace = page["namespace"]
            .as_str()
            .ok_or("池清单缺少 namespace")?
            .parse::<Uuid>()?;
        if scope.is_some_and(|old| old != namespace) {
            return Err("查找池期间目录已切换，请重新检查".into());
        }
        scope = Some(namespace);
        for p in page["items"].as_array().ok_or("池清单无效")? {
            if id.map_or(p["name"] == selector, |id| p["id"] == id.to_string()) {
                target = Some(p["id"].as_str().ok_or("池缺少 UUID")?.parse::<Uuid>()?);
            }
        }
        if target.is_some() {
            break;
        }
        after = page["next"].as_str().map(str::to_owned);
        if after.is_none() {
            break;
        }
    }
    let pool = target.ok_or("指定池不存在")?;
    let namespace = scope.ok_or("池清单缺少 namespace")?;
    let default_root = || -> Result<PathBuf, Box<dyn std::error::Error>> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
            .ok_or("无法确定本机状态目录，请显式指定 --request")?;
        Ok(base
            .join("tape-rs/imports")
            .join(Uuid::new_v4().to_string()))
    };
    let (directory, single) = match command.request {
        Some(path) if command.barcodes.len() == 1 => (
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            Some(path),
        ),
        Some(path) => (path, None),
        None => (default_root()?, None),
    };
    use std::os::unix::fs::DirBuilderExt;
    if single.is_none() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
    }
    let mut prepared = Vec::new();
    for barcode in command.barcodes {
        let request = client.prepare_control(
            &library,
            namespace,
            json!({"kind":"media-import","pool":pool.to_string(),"barcode":barcode,"volume":null,"path":command.path.as_deref().unwrap_or("/IMPORT"),"merge_tapes":command.merge_tapes}),
        )?;
        let path = single.clone().unwrap_or_else(|| {
            directory.join(format!(
                "{}.json",
                request.value()["operation"]
                    .as_str()
                    .expect("prepared operation")
            ))
        });
        request.save_new(&path)?;
        eprintln!("磁带 {barcode} 的原请求已保存至 {}", path.display());
        prepared.push(path);
    }
    let mut rows = Vec::new();
    let mut failed = false;
    for path in prepared {
        match result(client, &path) {
            Ok(value) => {
                failed |= matches!(
                    value["task"]["status"].as_str(),
                    Some("failed" | "cancelled")
                );
                rows.push(value);
            }
            Err(error) => {
                failed = true;
                rows.push(json!({"request_file":path,"error":error.to_string()}));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"requests":rows}))?
    );
    if failed {
        return Err(
            "部分导入未成功；保留原请求文件，以 tape import --request FILE 查询或继续原任务".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumption_refuses_path_changes_before_network_or_file_access() {
        for (path, merge_tapes) in [(Some("/elsewhere".into()), false), (None, true)] {
            let command = Command {
                barcodes: vec![],
                request: Some("missing-original.json".into()),
                path,
                merge_tapes,
            };
            let client = FdbClient::new(["127.0.0.1:1"]).unwrap();
            let mut control = Client::new(["127.0.0.1:1"]);
            let error = run(&client, &mut control, None, None, command).unwrap_err();
            assert_eq!(
                error.to_string(),
                "恢复任务只能使用原请求，不得覆盖 --path 或 --merge-tapes"
            );
        }
    }
}
