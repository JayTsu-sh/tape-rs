use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;
use tape_rs::client::fdb::{
    FdbClient,
    management::{ManagementIntent, ManagementRequestFile},
};

#[derive(Subcommand)]
pub enum Command {
    /// 原子创建池及其根目录；不分配或格式化磁带
    Create {
        name: String,
        #[arg(long, default_value_t = 200_000)]
        file_limit: u64,
        /// 首次调用前保存请求身份；重试须用相同文件和参数
        #[arg(long)]
        request: PathBuf,
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
        request: PathBuf,
    },
    /// 提交删除空池任务；目录或磁带归属非空时失败
    Delete {
        selector: String,
        #[arg(long)]
        request: PathBuf,
    },
}
#[derive(Subcommand)]
pub enum TaskCommand {
    List,
    /// 根据保存的原始请求查询FDB任务；创建请求查询池UUID
    Show {
        #[arg(long)]
        request: PathBuf,
    },
    /// 仅取消尚未执行的FDB任务
    Cancel {
        #[arg(long)]
        request: PathBuf,
    },
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
    input: ManagementIntent,
    path: PathBuf,
) -> Result<Value, Box<dyn std::error::Error>> {
    match ManagementRequestFile::load(&path) {
        Ok(saved) => {
            if !saved.matches(library, &input) {
                return Err("请求文件的带库或参数不同；禁止覆盖原请求".into());
            }
        }
        Err(tape_rs::client::ClientError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            client.prepare_management(library, input)?.save_new(&path)?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(client.submit_management(&path)?)
}
fn items(
    client: &FdbClient,
    library: &str,
    tasks: bool,
) -> Result<Value, Box<dyn std::error::Error>> {
    let context = client.management_context(library)?;
    let ns = context["namespace"].as_str().ok_or("缺少目录命名空间")?;
    let mut after = None;
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    loop {
        let page = if tasks {
            client.management_page(library, ns, after.as_deref())?
        } else {
            client.pool_page(library, ns, after.as_deref())?
        };
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
    Ok(json!({"library_serial":library,"namespace":ns,"items":items}))
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
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = library(serial, pool)?;
    let result = match command {
        Command::Create {
            name,
            file_limit,
            request,
        } => submit(
            client,
            library,
            ManagementIntent::Create { name, file_limit },
            request,
        )?,
        Command::Set {
            selector,
            attribute,
            value,
            request,
        } => submit(
            client,
            library,
            ManagementIntent::Set {
                selector,
                attribute,
                value,
            },
            request,
        )?,
        Command::Delete { selector, request } => submit(
            client,
            library,
            ManagementIntent::Delete { selector },
            request,
        )?,
        Command::Show { selector } => {
            let context = client.management_context(library)?;
            client.pool(
                library,
                context["namespace"].as_str().ok_or("缺少命名空间")?,
                &selector,
            )?
        }
        Command::List { json } => {
            let result = items(client, library, false)?;
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
pub fn task(
    client: &FdbClient,
    serial: Option<&str>,
    pool: Option<&str>,
    command: TaskCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = library(serial, pool)?;
    let cancel = matches!(command, TaskCommand::Cancel { .. });
    match command {
        TaskCommand::List => print(items(client, library, true)?),
        TaskCommand::Show { request } | TaskCommand::Cancel { request } => {
            let saved = ManagementRequestFile::load(&request)?;
            if !saved.is_library(library) {
                return Err("请求文件不属于指定带库".into());
            }
            print(client.management_request_status(&request, cancel)?)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_cannot_break_table_rows_and_unicode_name_is_last() {
        assert_eq!(display_name("中文\n\t\u{1b}"), "中文\\n\\t\\u{1b}");
    }
}
