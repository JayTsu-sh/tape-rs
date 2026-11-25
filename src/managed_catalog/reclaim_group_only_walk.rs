//! A complete bounded namespace snapshot. No cursor, root, or request is written.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog::control::reclaim) struct GroupOnlyLogicalFile {
    pub file: ReclaimFile,
    pub head: Vec<u8>,
    pub copy: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog::control::reclaim) struct GroupOnlyLogicalManifest {
    pub files: Vec<GroupOnlyLogicalFile>,
    pub directories: Vec<(String, String, u64)>,
}

pub(in crate::managed_catalog::control::reclaim) async fn snapshot_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<GroupOnlyLogicalManifest> {
    let root = active_pools::root_id(parent.namespace(), parent.spec().pool());
    let full = pool_catalog_path(parent.spec().pool(), b"/")?;
    let Some(ResolvedPath::Directory { id, version, .. }) = lookup::resolve_active_path_at_mode(
        tr,
        parent.namespace(),
        lookup::path_components(&full)?,
        false,
    )
    .await?
    else {
        return Err(CatalogError::Invalid("group-only逻辑根目录不存在"));
    };
    if id != root {
        return Err(CatalogError::Corrupt("group-only逻辑根目录身份不同"));
    }
    let mut directories = vec![(String::from("/"), id.to_string(), version)];
    let mut files = Vec::new();
    let mut stack = vec![Frame {
        directory: root.to_string(),
        name: String::new(),
        after: None,
    }];
    // Match the ordinary walk's depth-first, one-child merged-page order,
    // including empty tombstone pages. Only this local stack is advanced.
    for _ in 0..512 {
        let Some(frame) = stack.last() else {
            return Ok(GroupOnlyLogicalManifest { files, directories });
        };
        let directory = uuid(&frame.directory)?;
        let after = frame
            .after
            .as_ref()
            .map(|name| child_key(parent.namespace(), directory, name.as_bytes()));
        let page =
            lookup::list_active_children_at(tr, parent.namespace(), directory, after.as_deref(), 1)
                .await?;
        if page.items.is_empty() {
            if let Some(next) = page.next {
                let prefix = child_prefix(parent.namespace(), directory);
                let name = decode_name(&next[prefix.len()..])?;
                stack.last_mut().unwrap().after = Some(
                    String::from_utf8(name)
                        .map_err(|_| CatalogError::Invalid("group-only游标必须UTF-8"))?,
                );
            } else {
                stack.pop();
            }
            continue;
        }
        let child = &page.items[0];
        let name = std::str::from_utf8(&child.name)
            .map_err(|_| CatalogError::Invalid("group-only文件名必须UTF-8"))?;
        let current = path(&stack);
        let relative = if current == "/" {
            format!("/{name}")
        } else {
            format!("{current}/{name}")
        };
        let full = pool_catalog_path(parent.spec().pool(), relative.as_bytes())?;
        let resolved = lookup::resolve_active_path_at_mode(
            tr,
            parent.namespace(),
            lookup::path_components(&full)?,
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("group-only分页目标丢失"))?;
        stack.last_mut().unwrap().after = Some(name.to_owned());
        match resolved {
            ResolvedPath::Directory { id, version, .. } if id == child.id => {
                if directories.len() >= 32 || stack.len() >= 32 {
                    return Err(CatalogError::Invalid(
                        "group-only目录数量或深度超出固定预算",
                    ));
                }
                directories.push((relative, id.to_string(), version));
                stack.push(Frame {
                    directory: id.to_string(),
                    name: name.to_owned(),
                    after: None,
                });
            }
            ResolvedPath::File { id, head, copy, .. } if id == child.id => {
                if copy.volume_id == parent.spec().volume() {
                    if files.len() >= 32 {
                        return Err(CatalogError::Invalid("group-only完整输入超过32成员"));
                    }
                    let hash = Sha256::new()
                        .chain_update(b"tape-rs/group-only-file-operation/1")
                        .chain_update(parent.fingerprint().as_bytes())
                        .chain_update(relative.as_bytes())
                        .chain_update(id.as_bytes())
                        .chain_update(head.version.to_be_bytes())
                        .finalize();
                    let operation = Uuid::from_bytes(hash[..16].try_into().expect("SHA256"));
                    let file = ReclaimFile {
                        path: relative,
                        file: id.to_string(),
                        version: head.version,
                        size: copy.size,
                        operation: operation.to_string(),
                    };
                    file.validate()?;
                    files.push(GroupOnlyLogicalFile {
                        file,
                        head: encode_head(&head),
                        copy: copy_records::encode_physical_copy(&copy)?,
                    });
                }
            }
            _ => return Err(CatalogError::Corrupt("group-only逻辑分页inode不同")),
        }
    }
    Err(CatalogError::Invalid("group-only完整逻辑枚举超出固定预算"))
}
