//! Public file queries share the native FDB client and its verified downloads.
use std::error::Error;
use tape_rs::client::fdb::{FdbClient, ListObjectsOptions};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub(super) fn scope<'a>(library: Option<&'a str>, pool: Option<&str>) -> Result<(&'a str, Uuid)> {
    let library = library.ok_or("文件查询必须指定 --library")?;
    let pool = pool
        .ok_or("文件查询必须指定 --pool UUID")?
        .parse::<Uuid>()?;
    if pool.is_nil() {
        return Err("文件查询必须指定非零 pool UUID".into());
    }
    Ok((library, pool))
}

pub(super) fn list(
    client: &FdbClient,
    library: &str,
    pool: Uuid,
    directory: Option<&str>,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut after = None;
    loop {
        let next = if let Some(directory) = directory {
            let directory = if directory == "/" {
                "/"
            } else {
                directory.trim_end_matches('/')
            };
            let directory = if directory.starts_with('/') {
                directory.to_owned()
            } else {
                format!("/{directory}")
            };
            let page = client.list_children(library, pool, &directory, 100, after.as_deref())?;
            for item in page["items"].as_array().ok_or("目录分页缺少 items")? {
                let name = item["name"].as_str().ok_or("目录名称不是 UTF-8")?;
                let path = format!("{}/{name}", directory.trim_end_matches('/'));
                let node = client
                    .stat(library, pool, &path)?
                    .ok_or("列举期间节点已改变")?;
                if node["id"] != item["id"] {
                    return Err("列举期间节点身份已改变，请重新查询".into());
                }
                if node["kind"] == "directory" {
                    println!("{:>12}  {name}/", "");
                } else {
                    println!(
                        "{:>12}  {name}",
                        node["size"].as_u64().ok_or("文件大小缺失")?
                    );
                }
            }
            page["next"].as_str().map(str::to_owned)
        } else {
            let page = client.list_objects_v2(
                library,
                pool,
                &ListObjectsOptions {
                    continuation_token: after.clone(),
                    ..Default::default()
                },
            )?;
            for item in page["contents"].as_array().ok_or("对象分页缺少 contents")? {
                println!(
                    "{:>12}  /{}",
                    item["size"].as_u64().ok_or("文件大小缺失")?,
                    item["key"].as_str().ok_or("对象路径缺失")?
                );
            }
            page["next_continuation_token"].as_str().map(str::to_owned)
        };
        let Some(next) = next else {
            return Ok(());
        };
        if !seen.insert(next.clone()) {
            return Err("列举游标重复，停止查询".into());
        }
        after = Some(next);
    }
}
