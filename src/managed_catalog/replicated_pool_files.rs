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
pub(in crate::managed_catalog) struct Source {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
}

impl Source {
    fn matches_symlink(&self, target: &str) -> bool {
        self.content.kind == Kind::Symlink
            && self.content.length == target.len() as u64
            && self.content.sha256 == <[u8; 32]>::from(Sha256::digest(target.as_bytes()))
    }

    async fn legacy_symlink_at(&self, tr: &Transaction, path: &str) -> Result<Option<String>> {
        let projected = match native_media::replica_origin_target_at(tr, self, path).await {
            Ok(path) => path,
            Err(CatalogError::MediaUnavailable) => return Ok(None),
            Err(error) => return Err(error),
        };
        let path = pool_catalog_path(self.pool_id, projected.as_bytes())?;
        let resolved =
            lookup::resolve_active_path_at(tr, self.namespace, lookup::path_components(&path)?)
                .await?;
        match resolved {
            Some(ResolvedPath::File {
                id,
                attributes: Some(attributes),
                copy,
                placement,
                ..
            }) if id
                == import_materialize::import_inode(self.namespace, self.pool_id, &projected)
                && placement.pool_id == self.pool_id
                && copy
                    .media
                    .as_ref()
                    .is_some_and(|media| media.native_symlink) =>
            {
                Ok(attributes
                    .symlink()
                    .filter(|target| self.matches_symlink(target))
                    .map(str::to_owned))
            }
            _ => Ok(None),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportedHead {
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
        let (saved_path, mut saved): (String, Source) = decode(&raw)?;
        let backfill = saved.symlink.is_none() && source.symlink.is_some();
        if backfill {
            saved.symlink.clone_from(&source.symlink);
        }
        if saved_path != path || saved != source {
            return Err(CatalogError::Corrupt("同一导入来源的正文证明发生变化"));
        }
        if backfill {
            tr.set(&source_key, &encode(&(&saved_path, &saved))?);
        }
        return Ok(());
    }
    let version_key = version_key(id, path, &source.content);
    let original_version = tr.get(&version_key, false).await?;
    let head_key = path_key(id, b'h', path);
    if let Some(raw) = tr.get(&head_key, false).await? {
        let head: ImportedHead = decode(&raw)?;
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
            &encode(&ImportedHead {
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

pub(super) fn hex(digest: &[u8; 32]) -> String {
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
    let head: ImportedHead = decode(&raw)?;
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
        &encode(&ImportedHead {
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
        let head = match head {
            Head::Archived { archive } => {
                items.push(archive_info(path, &archive));
                continue;
            }
            Head::Imported(head) => head,
        };
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
            "path":path,"kind":version.content.kind,"size":version.content.length,"file_version":1,
            "sha256":hex(&version.content.sha256),"replica_count":version.pool_ids.len(),
            "pool_ids":version.pool_ids,
        }));
    }
    Ok(json!({
        "version":1,"id":id,"items":items,
        "next":page.next.as_deref().map(|next| path_suffix(&prefix, next)).transpose()?,
    }))
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Head {
    Imported(ImportedHead),
    Archived { archive: archive::Published },
}

async fn head(tr: &Transaction, id: Uuid, path: &str) -> Result<Option<Head>> {
    tr.get(&path_key(id, b'h', path), false)
        .await?
        .map(|raw| decode(&raw))
        .transpose()
}

pub(super) async fn require_write_path(
    tr: &Transaction,
    id: Uuid,
    path: &str,
    previous: Option<u64>,
) -> Result<()> {
    let current = match head(tr, id, path).await? {
        None => None,
        Some(Head::Imported(record)) if record.source.content.kind == Kind::File => Some(1),
        Some(Head::Archived { archive }) => Some(archive.version),
        _ => return Err(CatalogError::Invalid("多副本归档目标不是普通文件")),
    };
    if current != previous {
        return Err(CatalogError::Invalid("多副本池路径已存在或版本不匹配"));
    }
    // Parents are imported directories or the implicit pool root.
    let (parent, _) = path
        .rsplit_once('/')
        .ok_or(CatalogError::Invalid("归档路径无效"))?;
    if !parent.is_empty()
        && !matches!(head(tr, id, parent).await?, Some(Head::Imported(record))
            if record.source.content.kind == Kind::Directory)
    {
        return Err(CatalogError::Invalid("多副本归档父目录不存在"));
    }
    Ok(())
}

pub(super) async fn publish_archive(
    tr: &Transaction,
    id: Uuid,
    input: &crate::replicated_pool::ReplicatedArchive,
    archive: archive::Published,
) -> Result<()> {
    require_write_path(tr, id, &input.path, input.previous).await?;
    tr.set(
        &path_key(id, b'h', &input.path),
        &encode(&Head::Archived { archive })?,
    );
    Ok(())
}

fn archive_info(path: &str, archive: &archive::Published) -> Value {
    json!({"path":path,"kind":"file","file_version":archive.version,"size":archive.size,
        "sha256":hex(&archive.sha256),"replica_count":archive.members.len(),
        "object_headers":archive.object.as_ref().map(|object| &object.headers),"modified":archive.modified,
        "pool_ids":archive.members.iter().map(|member| member.pool_id).collect::<Vec<_>>()})
}

#[derive(Clone, Debug)]
pub struct ReplicaRead {
    pub(crate) serial: String,
    pub(crate) pool: Uuid,
    ns: Uuid,
    owner_path: String,
    candidate: ReplicaCopy,
    pub(crate) size: u64,
    pub(crate) digest: [u8; 32],
}

#[derive(Clone, Debug)]
enum ReplicaCopy {
    Imported(Box<Source>),
    Archived(archive::CommittedMember),
}

pub(crate) struct ReplicaFile {
    pub info: Value,
    pub copies: Vec<ReplicaRead>,
}

fn archived_file(
    creation: &Creation,
    path: &str,
    archive: archive::Published,
) -> Result<ReplicaFile> {
    let info = archive_info(path, &archive);
    let mut copies = Vec::with_capacity(archive.members.len());
    for copy in archive.members {
        let member = super::archive::member_in(creation, copy.pool_id)?;
        copies.push(ReplicaRead {
            serial: member.library_serial.clone(),
            pool: member.pool_id,
            ns: member.namespace,
            owner_path: super::archive::member_path(creation.input.id, archive.operation),
            candidate: ReplicaCopy::Archived(copy),
            size: archive.size,
            digest: archive.sha256,
        });
    }
    Ok(ReplicaFile { info, copies })
}

impl ManagedCatalog {
    pub(crate) async fn replica_file(
        &self,
        parent: Uuid,
        path: &str,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<ReplicaFile>> {
        pool_catalog_path(parent, path.as_bytes())?;
        let tr = self.db.create_trx()?;
        let creation = archive::require_published(&tr, parent, routes).await?;
        if path == "/" {
            return Ok(Some(ReplicaFile {
                info: json!({"path":"/","kind":"directory","file_version":1,"size":0}),
                copies: Vec::new(),
            }));
        }
        let Some(head) = head(&tr, parent, path).await? else {
            return Ok(None);
        };
        let head = match head {
            Head::Archived { archive } => return archived_file(&creation, path, archive).map(Some),
            Head::Imported(head) => head,
        };
        let content = &head.source.content;
        let mut prefix = keyed_source(parent, path, Uuid::nil());
        prefix.truncate(prefix.len() - 16);
        let page = read_page(&tr, &prefix, None, super::MAX_TAPES, false).await?;
        let mut copies = Vec::new();
        let mut pools = BTreeSet::new();
        let mut symlink = None;
        let mut legacy_symlinks = Vec::new();
        if page.next.is_some() {
            return Err(CatalogError::Corrupt("导入副本集合超过原磁带清单上限"));
        }
        for (_, raw) in page.items {
            let (saved_path, source): (String, Source) = decode(&raw)?;
            if saved_path != path {
                return Err(CatalogError::Corrupt("导入副本来源路径不一致"));
            }
            if source.content == *content {
                pools.insert(source.pool_id);
                if content.kind == Kind::File {
                    copies.push(ReplicaRead {
                        serial: source.library_serial.clone(),
                        pool: source.pool_id,
                        ns: source.namespace,
                        owner_path: path.to_owned(),
                        candidate: ReplicaCopy::Imported(Box::new(source)),
                        size: content.length,
                        digest: content.sha256,
                    });
                } else if content.kind == Kind::Symlink {
                    match &source.symlink {
                        Some(target) if source.matches_symlink(target) => {
                            symlink.get_or_insert_with(|| target.clone());
                        }
                        Some(_) => {
                            return Err(CatalogError::Corrupt("导入符号链接目标与原证明不符"));
                        }
                        None => legacy_symlinks.push(source),
                    }
                }
            }
        }
        let mut info = json!({"path":path,"kind":content.kind,"file_version":1,"size":content.length,
            "sha256":hex(&content.sha256),"replica_count":pools.len(),"pool_ids":pools});
        if content.kind == Kind::Symlink {
            for source in legacy_symlinks {
                if symlink.is_some() {
                    break;
                }
                symlink = source.legacy_symlink_at(&tr, path).await?;
            }
            info["symlink"] = json!(symlink.ok_or(CatalogError::MediaUnavailable)?);
        }
        Ok(Some(ReplicaFile { info, copies }))
    }

    pub(crate) async fn replica_recovery_file(
        &self,
        parent: Uuid,
        operation: Uuid,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<ReplicaFile>> {
        let tr = self.db.create_trx()?;
        let creation = archive::require_published(&tr, parent, routes).await?;
        archive::recovery_version(&tr, parent, operation)
            .await?
            .map(|archive| archived_file(&creation, "/", archive))
            .transpose()
    }

    pub(crate) async fn resolve_replica_read(
        &self,
        selection: &ReplicaRead,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<ActiveReadSource> {
        let tr = self.db.create_trx()?;
        let ns =
            routing::require_runtime_active_owner(&tr, &selection.serial, route, owner).await?;
        if ns != selection.ns || parent_at(&tr, ns, selection.pool).await?.is_none() {
            return Err(CatalogError::OwnerFenced);
        }
        logical_libraries::require_pool_inventory_ready_at(
            &tr,
            ns,
            &selection.serial,
            selection.pool,
        )
        .await?;
        native_media::require_pool_readable(&tr, ns, selection.pool).await?;
        let projected = match &selection.candidate {
            ReplicaCopy::Archived(_) => selection.owner_path.clone(),
            ReplicaCopy::Imported(source) => {
                native_media::replica_origin_target_at(&tr, source, &selection.owner_path).await?
            }
        };
        let path = pool_catalog_path(selection.pool, projected.as_bytes())?;
        let resolved = lookup::resolve_active_path_at(&tr, ns, lookup::path_components(&path)?)
            .await?
            .ok_or(CatalogError::MediaUnavailable)?;
        let ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            ..
        } = &resolved
        else {
            return Err(CatalogError::MediaUnavailable);
        };
        if placement.pool_id != selection.pool || copy.size != selection.size {
            return Err(CatalogError::Corrupt("读取副本的归属或长度失配"));
        }
        let digest_matches = copy.content_digest == ContentDigest::Sha256(selection.digest);
        let is_original = match &selection.candidate {
            ReplicaCopy::Archived(original) => {
                *id == original.file_id && head.copy_id == original.copy_id
            }
            ReplicaCopy::Imported(original) => {
                copy.volume_id == original.volume_uuid
                    && copy.volume_generation == original.generation
                    && copy.ltfs_file_uid == original.file_uid
                    && copy.media.as_ref().is_some_and(|media| {
                        media.physical_path == original.physical_path
                            && media.version == original.tape_version
                    })
            }
        };
        let member_inode = match &selection.candidate {
            ReplicaCopy::Archived(original) => original.file_id,
            ReplicaCopy::Imported(_) => {
                import_materialize::import_inode(ns, selection.pool, &projected)
            }
        };
        if *id != member_inode
            || !is_original && !digest_matches
            || matches!(selection.candidate, ReplicaCopy::Archived(_)) && !digest_matches
        {
            return Err(CatalogError::Corrupt("当前副本与原归档或导入来源不符"));
        }
        let source = lookup::read_source_at(&tr, &selection.serial, &path, resolved).await?;
        if let ReplicaCopy::Imported(original) = &selection.candidate
            && is_original
            && source.source.sha256 != original.index_sha256
        {
            return Err(CatalogError::Corrupt("导入副本原索引摘要不符"));
        }
        Ok(source)
    }
}

impl ManagedCatalog {
    pub(crate) async fn replica_children(
        &self,
        pool: Uuid,
        path: &str,
        after: Option<&str>,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<Value>> {
        pool_catalog_path(pool, path.as_bytes())?;
        if let Some(name) = after {
            validate_name(name.as_bytes())?;
        }
        let tr = self.db.create_trx()?;
        archive::require_published(&tr, pool, routes).await?;
        if path != "/" {
            match head(&tr, pool, path).await? {
                None => return Ok(None),
                Some(Head::Imported(row)) if row.source.content.kind == Kind::Directory => {}
                _ => return Err(CatalogError::Invalid("多副本池路径不是目录")),
            }
        }
        let base = task_key(pool, b'h');
        let prefix = path_key(pool, b'h', &format!("{}/", path.trim_end_matches('/')));
        let mut items = Vec::new();
        let mut start = after.map(|name| [prefix.as_slice(), name.as_bytes()].concat());
        let mut inclusive = false;
        while items.len() <= 100 {
            let rows = path_rows(&tr, &prefix, start.as_deref(), inclusive).await?;
            if rows.is_empty() {
                break;
            }
            for (key, row) in rows {
                let name = path_suffix(&prefix, &key)?;
                if let Some((child, _)) = name.split_once('/') {
                    start = Some(prefix_end(
                        &[prefix.as_slice(), child.as_bytes(), b"/"].concat(),
                    ));
                    inclusive = true;
                    break;
                }
                if !name.is_empty() {
                    items.push(head_info(path_suffix(&base, &key)?, decode(&row)?));
                }
                start = Some(key);
                inclusive = false;
                if items.len() > 100 {
                    break;
                }
            }
        }
        let next = if items.len() > 100 {
            items.truncate(100);
            items
                .last()
                .and_then(|v| v["path"].as_str())
                .and_then(|p| p.rsplit('/').next())
        } else {
            None
        };
        Ok(Some(json!({"id":pool,"items":items,"next":next})))
    }

    pub(crate) async fn list_replica_objects(
        &self,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
        query: ObjectListRequest<'_>,
    ) -> Result<Page<ObjectEntry>> {
        let tr = self.db.create_trx()?;
        archive::require_published(&tr, query.pool, routes).await?;
        let base = path_key(query.pool, b'h', "/");
        let prefix = [base.as_slice(), query.prefix.as_bytes()].concat();
        let mut start = Some([base.as_slice(), query.start_after.as_bytes()].concat());
        let mut inclusive = false;
        let mut items = Vec::new();
        while query.max_keys > 0 && items.len() <= query.max_keys {
            let rows = path_rows(&tr, &prefix, start.as_deref(), inclusive).await?;
            if rows.is_empty() {
                break;
            }
            for (key, raw) in rows {
                let object_key = path_suffix(&base, &key)?;
                let suffix = path_suffix(&prefix, &key)?;
                let info = head_info(&format!("/{object_key}"), decode(&raw)?);
                let is_file = info["kind"] == "file";
                let grouping = if query.delimiter == "/" {
                    suffix
                        .find('/')
                        .map(|i| &object_key[..query.prefix.len() + i + 1])
                } else {
                    None
                };
                if is_file && let Some(group) = grouping {
                    if group > query.start_after {
                        items.push(ObjectEntry {
                            key: group.into(),
                            file: None,
                            modified: None,
                        });
                    }
                    start = Some(prefix_end(&[base.as_slice(), group.as_bytes()].concat()));
                    inclusive = true;
                    break;
                }
                if is_file {
                    let inode =
                        crate::replicated_pool::inode(query.pool, &format!("/{object_key}"));
                    items.push(ObjectEntry {
                        key: object_key.into(),
                        file: Some((
                            inode,
                            FileHead {
                                version: info["file_version"].as_u64().expect("catalog version"),
                                size: info["size"].as_u64().expect("catalog size"),
                                copy_id: inode,
                            },
                        )),
                        modified: info["modified"].as_i64(),
                    });
                }
                start = Some(key);
                inclusive = false;
                if items.len() > query.max_keys {
                    break;
                }
            }
        }
        let next = if items.len() > query.max_keys {
            items.truncate(query.max_keys);
            items.last().map(|row| row.key.as_bytes().to_vec())
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

fn head_info(path: &str, head: Head) -> Value {
    match head {
        Head::Imported(row) => json!({"path":path,"kind":row.source.content.kind,"file_version":1,
            "size":row.source.content.length,"sha256":hex(&row.source.content.sha256)}),
        Head::Archived { archive } => archive_info(path, &archive),
    }
}

async fn path_rows(
    tr: &Transaction,
    prefix: &[u8],
    start: Option<&[u8]>,
    inclusive: bool,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix);
    if start.is_some_and(|start| start >= end.as_slice()) {
        return Ok(Vec::new());
    }
    let begin = match start {
        Some(key) if key >= prefix && !inclusive => KeySelector::first_greater_than(key.to_vec()),
        Some(key) if key >= prefix => KeySelector::first_greater_or_equal(key.to_vec()),
        _ => KeySelector::first_greater_or_equal(prefix.to_vec()),
    };
    let range = RangeOption {
        begin,
        end: KeySelector::first_greater_or_equal(end),
        limit: Some(64),
        ..RangeOption::default()
    };
    Ok(tr
        .get_range(&range, 1, false)
        .await?
        .iter()
        .map(|row| (row.key().to_vec(), row.value().to_vec()))
        .collect())
}
