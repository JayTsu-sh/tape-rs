//! Verified import evidence, kept separate from the ordinary pool's LTFS names.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::managed_catalog::control) enum Kind {
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog::control) struct Content {
    pub kind: Kind,
    pub sha256: [u8; 32],
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog::control) struct Source {
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool_id: Uuid,
    pub barcode: String,
    pub volume_uuid: Uuid,
    pub generation: u64,
    pub index_sha256: [u8; 32],
    pub operation: Uuid,
    pub file_uid: u64,
    pub physical_path: String,
    pub tape_version: (u64, u64),
    pub content: Content,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Head {
    source: Source,
    resolution_revision: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Conflict {
    revision: u64,
    sources: [Source; 2],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Version {
    content: Content,
    pool_ids: BTreeSet<Uuid>,
}

fn path_key(id: Uuid, kind: u8, path: &str) -> Vec<u8> {
    let mut key = task_key(id, kind);
    key.extend_from_slice(path.as_bytes());
    key
}

fn keyed_source(id: Uuid, path: &str, volume: Uuid) -> Vec<u8> {
    let mut key = task_key(id, b's');
    key.extend_from_slice(&Sha256::digest(path.as_bytes()));
    key.extend_from_slice(volume.as_bytes());
    key
}

fn version_key(id: Uuid, path: &str, content: &Content) -> Vec<u8> {
    let mut key = task_key(id, b'v');
    key.extend_from_slice(&Sha256::digest(path.as_bytes()));
    key.push(match content.kind {
        Kind::File => b'f',
        Kind::Directory => b'd',
        Kind::Symlink => b'l',
    });
    key.extend_from_slice(&content.length.to_be_bytes());
    key.extend_from_slice(&content.sha256);
    key
}

fn resolution_key(id: Uuid, path: &str, revision: u64) -> Vec<u8> {
    let mut key = task_key(id, b'r');
    key.extend_from_slice(&Sha256::digest(path.as_bytes()));
    key.extend_from_slice(&revision.to_be_bytes());
    key
}

pub(in crate::managed_catalog::control) async fn record_at(
    tr: &Transaction,
    id: Uuid,
    path: &str,
    source: Source,
) -> Result<()> {
    let creation = read_at(tr, id)
        .await?
        .ok_or(CatalogError::Corrupt("缺少多副本导入原任务"))?;
    if creation.published
        || !creation.input.members.iter().any(|member| {
            member.library_serial == source.library_serial
                && member.namespace == source.namespace
                && member.pool_id == source.pool_id
                && member.tapes.contains(&source.barcode)
        })
    {
        return Err(CatalogError::Corrupt("多副本来源不在原导入范围"));
    }
    let source_key = keyed_source(id, path, source.volume_uuid);
    if let Some(raw) = tr.get(&source_key, false).await? {
        if decode::<(String, Source)>(&raw)? != (path.to_owned(), source) {
            return Err(CatalogError::Corrupt("同一导入来源的正文证明发生变化"));
        }
        return Ok(());
    }
    let version_key = version_key(id, path, &source.content);
    let original_version = tr.get(&version_key, false).await?;
    let head_key = path_key(id, b'h', path);
    if let Some(raw) = tr.get(&head_key, false).await? {
        let head: Head = decode(&raw)?;
        if head.source.content != source.content && original_version.is_none() {
            let conflict_key = path_key(id, b'c', path);
            match tr.get(&conflict_key, false).await? {
                Some(raw) if source.content.kind == Kind::Directory => {
                    let mut conflict: Conflict = decode(&raw)?;
                    if conflict
                        .sources
                        .iter()
                        .all(|copy| copy.content.kind != Kind::Directory)
                    {
                        conflict.sources[1] = source.clone();
                        tr.set(&conflict_key, &encode(&conflict)?);
                    }
                }
                Some(_) => {}
                None => {
                    let revision = head
                        .resolution_revision
                        .checked_add(1)
                        .ok_or(CatalogError::Corrupt("多副本冲突修订号耗尽"))?;
                    tr.set(
                        &conflict_key,
                        &encode(&Conflict {
                            revision,
                            sources: [head.source, source.clone()],
                        })?,
                    );
                }
            }
        }
    } else {
        tr.set(
            &head_key,
            &encode(&Head {
                source: source.clone(),
                resolution_revision: 0,
            })?,
        );
    }
    let mut version: Version = match original_version {
        Some(raw) => decode(&raw)?,
        None => Version {
            content: source.content.clone(),
            pool_ids: BTreeSet::new(),
        },
    };
    if version.content != source.content {
        return Err(CatalogError::Corrupt("多副本来源摘要与证明键不符"));
    }
    version.pool_ids.insert(source.pool_id);
    tr.set(&version_key, &encode(&version)?);
    tr.set(&source_key, &encode(&(path, source))?);
    Ok(())
}

pub(super) async fn conflict_at(tr: &Transaction, id: Uuid) -> Result<Option<Value>> {
    let prefix = task_key(id, b'c');
    let page = read_page(tr, &prefix, None, 1, false).await?;
    page.items
        .first()
        .map(|(key, raw)| {
            let path = path_suffix(&prefix, key)?;
            let conflict: Conflict = decode(raw)?;
            Ok(json!({"path":path,"revision":conflict.revision,
                "sources":conflict.sources.iter().map(source_view).collect::<Vec<_>>()}))
        })
        .transpose()
}

fn path_suffix<'a>(prefix: &[u8], key: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(&key[prefix.len()..])
        .map_err(|_| CatalogError::Corrupt("多副本导入路径不是 UTF-8"))
}

fn source_view(source: &Source) -> Value {
    json!({
        "library_serial":source.library_serial, "namespace":source.namespace,
        "pool_id":source.pool_id,"barcode":source.barcode,"volume_uuid":source.volume_uuid,
        "generation":source.generation,"kind":source.content.kind,"size":source.content.length,
        "sha256":hex(&source.content.sha256),
    })
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) async fn resolve_at(
    tr: &Transaction,
    id: Uuid,
    choice: &ResolveReplicatedPoolConflict,
) -> Result<()> {
    let path = choice.path.as_str();
    pool_catalog_path(id, path.as_bytes())?;
    let receipt = resolution_key(id, path, choice.revision);
    if let Some(raw) = tr.get(&receipt, false).await? {
        return if decode::<ResolveReplicatedPoolConflict>(&raw)? == *choice {
            Ok(())
        } else {
            Err(CatalogError::IdempotencyMismatch)
        };
    }
    let key = path_key(id, b'h', path);
    let raw = tr
        .get(&key, false)
        .await?
        .ok_or(CatalogError::Invalid("导入路径不存在"))?;
    let head: Head = decode(&raw)?;
    let conflict = path_key(id, b'c', path);
    let raw = tr
        .get(&conflict, false)
        .await?
        .ok_or(CatalogError::Invalid("导入路径没有待处理冲突"))?;
    let original: Conflict = decode(&raw)?;
    if original.revision != choice.revision
        || head.resolution_revision.checked_add(1) != Some(choice.revision)
    {
        return Err(CatalogError::IdempotencyMismatch);
    }
    let raw = tr
        .get(&keyed_source(id, path, choice.volume_uuid), false)
        .await?
        .ok_or(CatalogError::Invalid("指定卷缺少该路径的已验证来源"))?;
    let (original_path, source): (String, Source) = decode(&raw)?;
    if original_path != path || source.volume_uuid != choice.volume_uuid {
        return Err(CatalogError::Corrupt("多副本冲突来源与原路径或卷不符"));
    }
    if source.content.kind != Kind::Directory {
        let descendants = path_key(id, b'h', &format!("{path}/"));
        if original
            .sources
            .iter()
            .any(|copy| copy.content.kind == Kind::Directory)
            || requests::prefix_has_rows(tr, &descendants).await?
        {
            return Err(CatalogError::Invalid(
                "此路径包含目录来源，请保留目录及其文件",
            ));
        }
    }
    tr.set(
        &key,
        &encode(&Head {
            source,
            resolution_revision: choice.revision,
        })?,
    );
    tr.set(&receipt, &encode(choice)?);
    tr.clear(&conflict);
    Ok(())
}

pub(super) async fn page(
    tr: &Transaction,
    id: Uuid,
    after: Option<&str>,
    limit: usize,
) -> Result<Value> {
    if !(1..=100).contains(&limit) {
        return Err(CatalogError::Invalid("导入文件分页大小须在 1..=100"));
    }
    if let Some(path) = after {
        pool_catalog_path(id, path.as_bytes())?;
    }
    let prefix = task_key(id, b'h');
    let after = after.map(|path| path_key(id, b'h', path));
    let page = read_page(tr, &prefix, after.as_deref(), limit, false).await?;
    let mut items = Vec::with_capacity(page.items.len());
    for (key, raw) in page.items {
        let path = path_suffix(&prefix, &key)?;
        let head: Head = decode(&raw)?;
        let version = tr
            .get(&version_key(id, path, &head.source.content), false)
            .await?
            .ok_or(CatalogError::Corrupt("导入路径缺少已验证副本记录"))?;
        let version: Version = decode(&version)?;
        if version.content != head.source.content
            || !version.pool_ids.contains(&head.source.pool_id)
        {
            return Err(CatalogError::Corrupt("导入路径的版本与来源不符"));
        }
        items.push(json!({
            "path":path,"kind":version.content.kind,"size":version.content.length,
            "sha256":hex(&version.content.sha256),"replica_count":version.pool_ids.len(),
            "pool_ids":version.pool_ids,
        }));
    }
    Ok(json!({
        "version":1,"id":id,"items":items,
        "next":page.next.as_deref().map(|next| path_suffix(&prefix, next)).transpose()?,
    }))
}
