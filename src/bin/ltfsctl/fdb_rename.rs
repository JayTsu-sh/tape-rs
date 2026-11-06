//! CLI adapter for the SDK's original two-path rename journal.
use super::{FdbChangeArgs, print_json};
use clap::Subcommand;
use std::path::PathBuf;
use tape_rs::client::{
    FileVersion,
    fdb::{FdbClient, RenameCondition, RenameSpec},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub(super) enum Command {
    /// 保存两端原身份并只读准备，不登记任务或写带
    Prepare {
        from: String,
        to: String,
        /// 必须匹配的原源节点逻辑版本
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..u64::MAX))]
        expected_version: u64,
        /// 替换目标时必须指定其原逻辑版本；省略表示原目标不存在
        #[arg(long, conflicts_with = "no_replace", value_parser = clap::value_parser!(u64).range(1..u64::MAX))]
        expected_target_version: Option<u64>,
        /// 明确要求目标不存在
        #[arg(long)]
        no_replace: bool,
        #[command(flatten)]
        save: FdbChangeArgs,
    },
    /// 提交或恢复保存的原重命名，等待持久终态
    Run { request: PathBuf },
    /// 查询原重命名；null 表示尚未准备或登记
    Status { request: PathBuf },
    /// 取消原未写任务；已结束时返回首次终态
    Cancel { request: PathBuf },
}

fn parent(path: &str) -> Result<&str> {
    if !path.starts_with('/') || path == "/" {
        return Err("重命名路径必须是池内非根绝对路径".into());
    }
    let (parent, _) = path.rsplit_once('/').ok_or("重命名路径缺少父目录")?;
    Ok(if parent.is_empty() { "/" } else { parent })
}
fn version(value: &serde_json::Value, namespace: Uuid) -> Result<FileVersion> {
    if value["namespace"] != namespace.to_string() {
        return Err("原节点 namespace 与 --namespace 不一致".into());
    }
    let inode: Uuid = value["id"].as_str().ok_or("原节点缺少 inode")?.parse()?;
    let version = value["version"]
        .as_u64()
        .filter(|v| *v > 0 && *v < u64::MAX)
        .ok_or("原节点版本无效或不可递增")?;
    if inode.is_nil() {
        return Err("原节点 inode 不能为空".into());
    }
    Ok(FileVersion::Fdb {
        namespace,
        inode,
        version,
    })
}

pub(super) fn run(
    client: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    command: Command,
) -> Result<()> {
    if let Command::Prepare {
        from,
        to,
        expected_version,
        expected_target_version,
        no_replace,
        save,
    } = command
    {
        let library = library.ok_or("FDB rename prepare 必须指定 --library")?;
        let pool: Uuid = pool
            .ok_or("FDB rename prepare 必须指定 --pool UUID")?
            .parse()?;
        if save.request.try_exists()? {
            return Err("原请求目录已存在；请使用 run/status/cancel，不重新准备".into());
        }
        let source_parent_path = parent(&from)?;
        let destination_parent_path = parent(&to)?;
        let source = client.stat(library, pool, &from)?.ok_or("原源节点不存在")?;
        if source["version"].as_u64() != Some(expected_version) {
            return Err("原源节点版本与 --expected-version 不一致".into());
        }
        let source = version(&source, save.namespace)?;
        let source_parent = client
            .stat(library, pool, source_parent_path)?
            .ok_or("源父目录不存在")?;
        if source_parent["kind"] != "directory" {
            return Err("源父节点不是目录".into());
        }
        let source_parent = version(&source_parent, save.namespace)?;
        let destination_parent = if source_parent_path == destination_parent_path {
            source_parent
        } else {
            let parent = client
                .stat(library, pool, destination_parent_path)?
                .ok_or("目标父目录不存在")?;
            if parent["kind"] != "directory" {
                return Err("目标父节点不是目录".into());
            }
            version(&parent, save.namespace)?
        };
        let target = match (client.stat(library, pool, &to)?, expected_target_version) {
            (None, None) => None,
            (Some(target), Some(expected)) if target["version"].as_u64() == Some(expected) => {
                Some(version(&target, save.namespace)?)
            }
            _ => return Err("原目标存在性或版本与 --expected-target-version 不一致".into()),
        };
        let spec = RenameSpec {
            pool,
            from,
            to,
            no_replace,
            condition: RenameCondition {
                source,
                source_parent,
                destination_parent,
                target,
            },
        };
        client.prepare_rename(library, &spec, &save.request)?;
        println!(
            "原重命名已保存至 {}；尚未登记任务或写带",
            save.request.display()
        );
    } else {
        if library.is_some() || pool.is_some() {
            return Err("重命名作用域来自原请求目录，请省略 --library/--pool".into());
        }
        match command {
            Command::Run { request } => print_json(client.run_rename_saved(&request)?),
            Command::Status { request } => print_json(
                client
                    .rename_status_saved(&request)?
                    .unwrap_or(serde_json::Value::Null),
            ),
            Command::Cancel { request } => print_json(client.cancel_rename_saved(&request)?),
            Command::Prepare { .. } => unreachable!(),
        }
    }
    Ok(())
}
