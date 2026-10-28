//! Read a logical path, current copy and physical placement at one FDB version.

use super::*;
use std::collections::HashMap;
use std::path::Path;

use crate::catalog::volume_extents::{CachedFile, ExpectedIndex, VolumeExtentCache};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedPath {
    Directory {
        id: Uuid,
        attributes: Option<NodeAttributes>,
    },
    File {
        id: Uuid,
        head: FileHead,
        copy: Box<PhysicalCopy>,
        placement: VolumePlacement,
        attributes: Option<NodeAttributes>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRead {
    pub id: Uuid,
    pub head: FileHead,
    pub copy: PhysicalCopy,
    pub placement: VolumePlacement,
    pub physical: CachedFile,
    pub attributes: Option<NodeAttributes>,
}

/// A file plan and the exact index construct that supplies its extents.
/// The caller must still verify the medium and owner/PR before tape I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedActiveRead {
    pub read: ResolvedRead,
    pub source: ActiveIndexIdentity,
}

/// A cache-independent file lookup. The path and published metadata remain
/// bound to the exact FDB version that supplied the source index identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveReadSource {
    resolved: ResolvedPath,
    path: Vec<u8>,
    pub source: ActiveIndexIdentity,
}

impl ActiveReadSource {
    pub fn placement(&self) -> &VolumePlacement {
        match &self.resolved {
            ResolvedPath::File { placement, .. } => placement,
            ResolvedPath::Directory { .. } => unreachable!("source lookup accepts files only"),
        }
    }

    pub fn plan_read(self, cache_path: &Path) -> Result<ResolvedActiveRead> {
        let expected = ExpectedIndex {
            volume_uuid: self.source.volume_uuid,
            generation: self.source.generation,
            location: self.source.location,
            sha256: self.source.sha256,
        };
        let cache = if self.source.incremental {
            VolumeExtentCache::open_incremental(cache_path, expected)
        } else {
            VolumeExtentCache::open(cache_path, expected)
        }
        .map_err(|error| CatalogError::Cache(error.to_string()))?;
        Ok(ResolvedActiveRead {
            read: plan_read(self.resolved, &self.path, &cache)?,
            source: self.source,
        })
    }
}

pub(super) fn path_components(path: &[u8]) -> Result<Vec<&[u8]>> {
    if path.first() != Some(&b'/') {
        return Err(CatalogError::Invalid("目录路径必须以 / 开头"));
    }
    if path == b"/" {
        return Ok(Vec::new());
    }
    let components: Vec<_> = path[1..].split(|byte| *byte == b'/').collect();
    if components.len() > usize::from(MAX_DEPTH) {
        return Err(CatalogError::Invalid("目录路径层数超过上限"));
    }
    for component in &components {
        validate_name(component)?;
    }
    Ok(components)
}

pub(super) async fn require_serving_copy_at(
    tr: &Transaction,
    namespace: Uuid,
    copy: &PhysicalCopy,
) -> Result<VolumePlacement> {
    let baseline = require_volume_ready(tr, copy.volume_id, None).await?;
    let placement = control::verified_placement(tr, copy.volume_id)
        .await?
        .ok_or(CatalogError::Corrupt("当前副本缺少卷放置"))?;
    if placement.namespace != namespace {
        return Err(CatalogError::Corrupt("当前副本与命名空间归属不一致"));
    }
    let ready = tr
        .get(&ready_volume_key(copy.volume_id), true)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let (_, serial) = decode_ready_volume(&ready)?;
    if serial != placement.library_serial {
        return Err(CatalogError::Corrupt("卷扫描带库序列号与放置不一致"));
    }
    if copy.volume_generation > baseline.generation
        && tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
    {
        if copy.volume_scan_id != baseline.id
            || routing::require_active_namespace(tr, &serial).await? != namespace
        {
            return Err(CatalogError::Corrupt("活动副本与扫描或带库路由不一致"));
        }
        let (_, index) =
            active_volume::observed_batch_at(tr, &serial, copy.volume_id, copy.volume_generation)
                .await?
                .ok_or(CatalogError::Incomplete)?;
        if index.volume_uuid != copy.volume_id || index.generation != copy.volume_generation {
            return Err(CatalogError::Corrupt("活动副本与已观察索引不一致"));
        }
    } else {
        require_volume_ready(
            tr,
            copy.volume_id,
            Some(VolumeScan {
                generation: copy.volume_generation,
                id: copy.volume_scan_id,
            }),
        )
        .await?;
    }
    Ok(placement)
}

impl ManagedCatalog {
    async fn hydrate_resolved(
        &self,
        namespace: Uuid,
        resolved: Option<ResolvedPath>,
    ) -> Result<Option<ResolvedPath>> {
        let Some(mut resolved) = resolved else {
            return Ok(None);
        };
        let attributes = match &mut resolved {
            ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. } => {
                attributes
            }
        };
        if let Some(reference) = attributes.take() {
            *attributes = Some(self.hydrate_attributes(namespace, reference).await?);
        }
        Ok(Some(resolved))
    }

    /// Build a read plan from the shared logical catalog and an exact local
    /// index cache. A ready scan can change after this returns; the executor
    /// must still hold owner/PR qualification and revalidate media identity
    /// before issuing tape I/O.
    pub async fn resolve_read(
        &self,
        namespace: Uuid,
        path: &[u8],
        cache_path: &Path,
    ) -> Result<Option<ResolvedRead>> {
        let Some(resolved) = self.resolve_path(namespace, path).await? else {
            return Ok(None);
        };
        let ResolvedPath::File { ref copy, .. } = resolved else {
            return Err(CatalogError::Invalid("目录不能作为普通文件读取"));
        };
        let scan = VolumeScan {
            generation: copy.volume_generation,
            id: copy.volume_scan_id,
        };
        let cache = self
            .open_volume_cache(copy.volume_id, scan, cache_path)
            .await?;
        plan_read(resolved, path, &cache).map(Some)
    }

    /// Resolve through the active physical-library route and bind the local
    /// extent cache to the same FDB version as the published file and index.
    /// Device owner/PR and mounted-medium identity are checked by the executor.
    pub async fn resolve_active_read(
        &self,
        serial: &str,
        path: &[u8],
        cache_path: &Path,
    ) -> Result<Option<ResolvedRead>> {
        Ok(self
            .resolve_active_read_at(serial, path, cache_path, None)
            .await?
            .map(|resolved| resolved.read))
    }

    /// Bind a content read plan to the exact active route and owner epoch in
    /// the same FDB read version as the file, copy and source index identity.
    /// This is a metadata plan only: the executor must recheck Raft/PR/owner
    /// and the current medium head before issuing tape I/O.
    pub async fn resolve_active_read_for_owner(
        &self,
        serial: &str,
        path: &[u8],
        cache_path: &Path,
        route: ActiveLibraryRoute,
        owner_epoch: u64,
    ) -> Result<Option<ResolvedActiveRead>> {
        self.resolve_active_read_at(serial, path, cache_path, Some((route, owner_epoch)))
            .await
    }

    async fn resolve_active_read_at(
        &self,
        serial: &str,
        path: &[u8],
        cache_path: &Path,
        owner: Option<(ActiveLibraryRoute, u64)>,
    ) -> Result<Option<ResolvedActiveRead>> {
        self.resolve_active_read_source_at(serial, path, owner)
            .await?
            .map(|source| source.plan_read(cache_path))
            .transpose()
    }

    /// Resolve the published file and exact index source before a local cache
    /// exists. Route, owner, copy and source share one FDB read version.
    pub async fn resolve_active_read_source_for_owner(
        &self,
        serial: &str,
        path: &[u8],
        route: ActiveLibraryRoute,
        owner_epoch: u64,
    ) -> Result<Option<ActiveReadSource>> {
        self.resolve_active_read_source_at(serial, path, Some((route, owner_epoch)))
            .await
    }

    async fn resolve_active_read_source_at(
        &self,
        serial: &str,
        path: &[u8],
        owner: Option<(ActiveLibraryRoute, u64)>,
    ) -> Result<Option<ActiveReadSource>> {
        control::validate_physical_serial(serial)?;
        let components = path_components(path)?;
        let tr = self.db.create_trx()?;
        let namespace = if let Some((route, epoch)) = owner {
            routing::require_runtime_active_owner(&tr, serial, route, epoch).await?
        } else {
            routing::require_active_namespace(&tr, serial).await?
        };
        let Some(resolved) = resolve_active_path_at(&tr, namespace, components).await? else {
            return Ok(None);
        };
        let ResolvedPath::File {
            ref copy,
            ref placement,
            ..
        } = resolved
        else {
            return Err(CatalogError::Invalid("目录不能作为普通文件读取"));
        };
        if copy
            .media
            .as_ref()
            .is_some_and(|media| media.native_symlink)
        {
            return Err(CatalogError::Invalid("原生符号链接不能作为普通文件读取"));
        }
        if placement.library_serial != serial {
            return Err(CatalogError::Corrupt("活动文件副本与带库路由不一致"));
        }
        let baseline = require_volume_ready(&tr, copy.volume_id, None).await?;
        if copy.volume_scan_id != baseline.id || copy.volume_generation < baseline.generation {
            return Err(CatalogError::Corrupt("活动文件副本与导入基线不一致"));
        }
        let (expected, incremental) = if copy.volume_generation == baseline.generation {
            (
                index_evidence::require_source(&tr, copy.volume_id, baseline).await?,
                false,
            )
        } else {
            let (_, index) = active_volume::observed_batch_at(
                &tr,
                serial,
                copy.volume_id,
                copy.volume_generation,
            )
            .await?
            .ok_or(CatalogError::Corrupt("活动文件副本缺少已观察索引"))?;
            (
                ExpectedIndex {
                    volume_uuid: index.volume_uuid,
                    generation: index.generation,
                    location: index.location,
                    sha256: index.sha256,
                },
                index.incremental,
            )
        };
        let source = ActiveIndexIdentity {
            volume_uuid: expected.volume_uuid,
            generation: expected.generation,
            location: expected.location,
            sha256: expected.sha256,
            incremental,
        };
        drop(tr);
        let resolved = self
            .hydrate_resolved(namespace, Some(resolved))
            .await?
            .expect("resolved path");
        Ok(Some(ActiveReadSource {
            resolved,
            path: path.to_vec(),
            source,
        }))
    }

    /// A readable managed namespace can answer `None` only after its import
    /// has been sealed. Pending scans and unavailable physical copies return an
    /// error, so callers cannot turn them into a false 404.
    pub async fn resolve_path(&self, namespace: Uuid, path: &[u8]) -> Result<Option<ResolvedPath>> {
        let components = path_components(path)?;
        let tr = self.db.create_trx()?;
        let resolved = if tr.get(&namespace_key(namespace), true).await?.as_deref()
            == Some(&[NamespaceState::Active as u8][..])
        {
            resolve_active_path_at(&tr, namespace, components).await?
        } else {
            resolve_path_at(&tr, namespace, components, true).await?
        };
        drop(tr);
        self.hydrate_resolved(namespace, resolved).await
    }

    /// Resolve a serving path through its active library route. Route proof,
    /// namespace state and inode/copy rows share one FDB read version.
    pub async fn resolve_active_path(
        &self,
        serial: &str,
        path: &[u8],
    ) -> Result<Option<ResolvedPath>> {
        self.resolve_active_path_with_route(serial, path, None)
            .await
    }

    /// Refuse a new Active route in a running reader until its configuration
    /// and cutover barrier have been revalidated by a fresh process.
    pub async fn resolve_active_path_for_route(
        &self,
        serial: &str,
        path: &[u8],
        expected: ActiveLibraryRoute,
    ) -> Result<Option<ResolvedPath>> {
        self.resolve_active_path_with_route(serial, path, Some(expected))
            .await
    }

    /// Query core catalog rows without hydrating opaque xattrs. Used by stat
    /// and directory enumeration; full-attribute queries remain explicit.
    pub async fn resolve_active_metadata_for_route(
        &self,
        serial: &str,
        path: &[u8],
        expected: ActiveLibraryRoute,
    ) -> Result<Option<ResolvedPath>> {
        control::validate_physical_serial(serial)?;
        let components = path_components(path)?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(expected)).await?;
        resolve_active_path_at_mode(&tr, namespace, components, false).await
    }

    async fn resolve_active_path_with_route(
        &self,
        serial: &str,
        path: &[u8],
        expected: Option<ActiveLibraryRoute>,
    ) -> Result<Option<ResolvedPath>> {
        control::validate_physical_serial(serial)?;
        let components = path_components(path)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_expected_active_namespace(&tr, serial, expected).await?;
        let resolved = resolve_active_path_at(&tr, namespace, components).await?;
        drop(tr);
        self.hydrate_resolved(namespace, resolved).await
    }

    /// Resolve the directory and its bounded page under the same active-route
    /// read version, so a route change cannot expose an old namespace page.
    pub async fn list_active_children(
        &self,
        serial: &str,
        path: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Option<Page<Child>>> {
        self.list_active_children_with_route(serial, path, after, limit, None)
            .await
    }

    pub async fn list_active_children_for_route(
        &self,
        serial: &str,
        path: &[u8],
        after: Option<&[u8]>,
        limit: usize,
        expected: ActiveLibraryRoute,
    ) -> Result<Option<Page<Child>>> {
        self.list_active_children_with_route(serial, path, after, limit, Some(expected))
            .await
    }

    async fn list_active_children_with_route(
        &self,
        serial: &str,
        path: &[u8],
        after: Option<&[u8]>,
        limit: usize,
        expected: Option<ActiveLibraryRoute>,
    ) -> Result<Option<Page<Child>>> {
        control::validate_physical_serial(serial)?;
        let components = path_components(path)?;
        if !(1..=100).contains(&limit) {
            return Err(CatalogError::Invalid("活动目录分页大小必须在 1..=100"));
        }
        let tr = self.db.create_trx()?;
        let namespace = routing::require_expected_active_namespace(&tr, serial, expected).await?;
        let parent = match resolve_active_path_at_mode(&tr, namespace, components, false).await? {
            Some(ResolvedPath::Directory { id, .. }) => id,
            Some(ResolvedPath::File { .. }) => {
                return Err(CatalogError::Invalid("文件不能作为目录列出"));
            }
            None => return Ok(None),
        };
        let prefix = child_prefix(namespace, parent);
        if let Some(cursor) = after
            && (!cursor.starts_with(&prefix)
                || decode_name(&cursor[prefix.len()..]).is_err()
                || cursor.len() > prefix.len() + 2 * MAX_NAME + 2)
        {
            return Err(CatalogError::Invalid("目录分页游标无效"));
        }
        Ok(Some(
            list_active_children_at(&tr, namespace, parent, after, limit).await?,
        ))
    }
}

fn plan_read(
    resolved: ResolvedPath,
    path: &[u8],
    cache: &VolumeExtentCache,
) -> Result<ResolvedRead> {
    let ResolvedPath::File {
        id,
        head,
        copy,
        placement,
        attributes,
    } = resolved
    else {
        return Err(CatalogError::Invalid("目录不能作为普通文件读取"));
    };
    if copy
        .media
        .as_ref()
        .is_some_and(|media| media.native_symlink)
    {
        return Err(CatalogError::Invalid("原生符号链接不能作为普通文件读取"));
    }
    let logical = std::str::from_utf8(path)
        .map_err(|_| CatalogError::Invalid("LTFS 路径必须为 UTF-8"))?
        .strip_prefix('/')
        .ok_or(CatalogError::Invalid("目录路径必须以 / 开头"))?;
    let (uid, physical) = if let Some(media) = &copy.media {
        // An imported logical path includes its pool namespace, while the
        // immutable provenance names the actual data object on this tape.
        // EE aliases point directly to their verified .LTFSEE_DATA target.
        if attributes
            .as_ref()
            .and_then(NodeAttributes::meta)
            .is_some_and(|meta| meta.file_uid != media.logical_file_uid)
        {
            return Err(CatalogError::Corrupt("逻辑属性与副本来源 UID 不一致"));
        }
        let indexed = cache
            .required_file_by_uid(media.logical_file_uid)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if media.ee_alias != indexed.symlink.is_some() {
            return Err(CatalogError::Corrupt("副本来源与索引别名类型不一致"));
        }
        let (uid, physical) = cache
            .resolve_data_file(&indexed.path)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        if physical.path != media.physical_path || physical.symlink.is_some() {
            return Err(CatalogError::Corrupt("物理缓存与已确认副本数据路径不一致"));
        }
        (uid, physical)
    } else {
        cache
            .resolve_data_file(logical)
            .map_err(|error| CatalogError::Cache(error.to_string()))?
    };
    if uid != copy.ltfs_file_uid || physical.length != copy.size {
        return Err(CatalogError::Corrupt(
            "逻辑目录与已确认 LTFS 路径的 UID 或长度不一致",
        ));
    }
    Ok(ResolvedRead {
        id,
        head,
        copy: *copy,
        placement,
        physical,
        attributes,
    })
}

pub(super) async fn resolve_active_path_at(
    tr: &Transaction,
    namespace: Uuid,
    components: Vec<&[u8]>,
) -> Result<Option<ResolvedPath>> {
    resolve_active_path_at_mode(tr, namespace, components, true).await
}

async fn resolve_active_path_at_mode(
    tr: &Transaction,
    namespace: Uuid,
    components: Vec<&[u8]>,
    load_attributes: bool,
) -> Result<Option<ResolvedPath>> {
    if let Some((&name, parents)) = components.split_last() {
        let parent = resolve_path_at_mode(tr, namespace, parents.to_vec(), true, false).await?;
        let parent = match parent {
            Some(ResolvedPath::Directory { id, .. }) => id,
            Some(ResolvedPath::File { .. }) => {
                return Err(CatalogError::Invalid("路径中间分量不是目录"));
            }
            None => return Ok(None),
        };
        if let Some(candidate) =
            published_target_at(tr, namespace, parent, name, load_attributes).await?
        {
            return Ok(Some(candidate));
        }
    }
    resolve_path_at_mode(tr, namespace, components, true, load_attributes).await
}

pub(super) async fn published_receipt_at(
    tr: &Transaction,
    namespace: Uuid,
    locator: active_batch::TargetLocator,
) -> Result<Option<(CommitReceipt, VolumePlacement)>> {
    if !active_batch::published_at(tr, locator).await? {
        return Ok(None);
    }
    let mut intent_key = requests::epoch_prefix(b'I', namespace, locator.request_epoch);
    intent_key.extend_from_slice(locator.operation.as_bytes());
    let intent = tr
        .get(&intent_key, true)
        .await?
        .ok_or(CatalogError::Corrupt("已发布活动目标缺少准入意图"))?;
    let receipt =
        requests::decode_intent(&intent, namespace, locator.request_epoch, locator.operation)?;
    if receipt.file_id != locator.file_id
        || receipt.volume_id != locator.volume
        || receipt.volume_generation != locator.generation
        || routing::require_active_namespace(tr, &receipt.library_serial).await? != namespace
        || !active_batch::target_claims_match(tr, &receipt, locator.batch_id).await?
        || !active_batch::sealed_for_index(
            tr,
            namespace,
            locator.volume,
            locator.generation,
            locator.batch_id,
            receipt.owner_epoch,
            receipt.volume_scan_id,
        )
        .await?
    {
        return Err(CatalogError::Corrupt("已发布活动目标与批次或路由不一致"));
    }
    let (observed_batch, index) = active_volume::observed_batch_at(
        tr,
        &receipt.library_serial,
        locator.volume,
        locator.generation,
    )
    .await?
    .ok_or(CatalogError::Corrupt("已发布活动目标缺少物理索引证据"))?;
    let proof = tr
        .get(
            &active_batch::prepared_batch_key(locator.volume, locator.generation),
            true,
        )
        .await?
        .ok_or(CatalogError::Corrupt("已发布活动目标缺少准备证明"))?;
    if observed_batch != locator.batch_id || proof[81..] != index.sha256 {
        return Err(CatalogError::Corrupt("已发布活动目标索引摘要不一致"));
    }
    let placement = control::verified_placement(tr, locator.volume)
        .await?
        .ok_or(CatalogError::Corrupt("已发布活动目标缺少卷放置"))?;
    if placement.namespace != namespace || placement.library_serial != receipt.library_serial {
        return Err(CatalogError::Corrupt("已发布活动目标卷放置不一致"));
    }
    Ok(Some((receipt, placement)))
}

pub(super) async fn published_file_receipt_at(
    tr: &Transaction,
    namespace: Uuid,
    file_id: Uuid,
) -> Result<Option<(active_batch::TargetLocator, CommitReceipt, VolumePlacement)>> {
    let Some(raw) = tr
        .get(&active_batch::file_claim_key_for(namespace, file_id), true)
        .await?
    else {
        return Ok(None);
    };
    let locator = active_batch::TargetLocator::decode(&raw)?;
    if locator.file_id != file_id {
        return Err(CatalogError::Corrupt("活动 inode 定位记录不一致"));
    }
    Ok(published_receipt_at(tr, namespace, locator)
        .await?
        .map(|(receipt, placement)| (locator, receipt, placement)))
}

pub(super) fn candidate_head(receipt: &CommitReceipt) -> Result<FileHead> {
    let version = receipt
        .expected_version
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("已发布活动目标版本溢出"))?;
    Ok(FileHead {
        version,
        size: receipt.size,
        copy_id: receipt.copy_id,
    })
}

pub(super) fn candidate_copy(receipt: &CommitReceipt, version: u64) -> PhysicalCopy {
    PhysicalCopy {
        version,
        request_epoch: receipt.request_epoch,
        volume_id: receipt.volume_id,
        volume_generation: receipt.volume_generation,
        volume_scan_id: receipt.volume_scan_id,
        ltfs_file_uid: receipt.ltfs_file_uid,
        size: receipt.size,
        content_digest: receipt.content_digest,
        media: None,
    }
}

pub(super) async fn published_child_id_at(
    tr: &Transaction,
    namespace: Uuid,
    parent: Uuid,
    name: &[u8],
) -> Result<Option<Uuid>> {
    let Some(raw) = tr
        .get(
            &active_batch::path_claim_key_for(namespace, parent, name),
            true,
        )
        .await?
    else {
        return Ok(None);
    };
    let locator = active_batch::TargetLocator::decode(&raw)?;
    let Some((receipt, _)) = published_receipt_at(tr, namespace, locator).await? else {
        return Ok(None);
    };
    if receipt.parent != parent || receipt.name != name {
        return Err(CatalogError::Corrupt("已发布路径与请求准入意图不一致"));
    }
    Ok(Some(locator.file_id))
}

async fn published_target_at(
    tr: &Transaction,
    namespace: Uuid,
    parent: Uuid,
    name: &[u8],
    load_attributes: bool,
) -> Result<Option<ResolvedPath>> {
    let Some(raw) = tr
        .get(
            &active_batch::path_claim_key_for(namespace, parent, name),
            true,
        )
        .await?
    else {
        return Ok(None);
    };
    let locator = active_batch::TargetLocator::decode(&raw)?;
    let Some((receipt, placement)) = published_receipt_at(tr, namespace, locator).await? else {
        return Ok(None);
    };
    if receipt.parent != parent || receipt.name != name {
        return Err(CatalogError::Corrupt("已发布路径与请求准入意图不一致"));
    }
    let attributes = if load_attributes {
        Some(
            active_batch::prepared_attributes_at(tr, locator.batch_id, &receipt)
                .await?
                .ok_or(CatalogError::Corrupt("已发布活动目标缺少文件属性"))?,
        )
    } else {
        None
    };
    let head = candidate_head(&receipt)?;
    Ok(Some(ResolvedPath::File {
        id: receipt.file_id,
        copy: Box::new(candidate_copy(&receipt, head.version)),
        head,
        placement,
        attributes,
    }))
}

/// Merge at most one bounded page from the imported directory and the Active
/// target index. The cursor is always a C-key, even when the last name came
/// from a target, so retries use the same ordering across both streams.
pub(super) async fn list_active_children_at(
    tr: &Transaction,
    namespace: Uuid,
    parent: Uuid,
    after: Option<&[u8]>,
    limit: usize,
) -> Result<Page<Child>> {
    let base_prefix = child_prefix(namespace, parent);
    let target_prefix = active_batch::path_claim_prefix_for(namespace, parent);
    let target_after = after.map(|cursor| {
        let mut key = target_prefix.clone();
        key.extend_from_slice(&cursor[base_prefix.len()..]);
        key
    });
    let base = read_page(tr, &base_prefix, after, limit + 1, true).await?;
    let targets = read_page(tr, &target_prefix, target_after.as_deref(), limit + 1, true).await?;
    let mut base_pos = 0;
    let mut target_pos = 0;
    let mut processed = 0;
    let mut items = Vec::with_capacity(limit);
    let mut last_suffix = None;
    let mut published = HashMap::new();
    while (base_pos < base.items.len() || target_pos < targets.items.len()) && processed < limit {
        let base_suffix = base
            .items
            .get(base_pos)
            .map(|(key, _)| &key[base_prefix.len()..]);
        let target_suffix = targets
            .items
            .get(target_pos)
            .map(|(key, _)| &key[target_prefix.len()..]);
        let suffix = match (base_suffix, target_suffix) {
            (Some(base), Some(target)) => base.min(target),
            (Some(base), None) => base,
            (None, Some(target)) => target,
            (None, None) => unreachable!(),
        };
        let base_id = if base_suffix == Some(suffix) {
            let id = uuid_from(&base.items[base_pos].1)?;
            base_pos += 1;
            Some(id)
        } else {
            None
        };
        let target = if target_suffix == Some(suffix) {
            let locator = active_batch::TargetLocator::decode(&targets.items[target_pos].1)?;
            target_pos += 1;
            Some(locator)
        } else {
            None
        };
        let mut id = base_id;
        if let Some(locator) = target {
            let key = (locator.volume, locator.generation, locator.batch_id);
            let committed = if let Some(committed) = published.get(&key) {
                *committed
            } else {
                let committed = active_batch::published_at(tr, locator).await?;
                published.insert(key, committed);
                committed
            };
            if committed {
                let (receipt, _) = published_receipt_at(tr, namespace, locator)
                    .await?
                    .ok_or(CatalogError::Corrupt("已发布目录项缺少批次证明"))?;
                if receipt.parent != parent || receipt.name != decode_name(suffix)? {
                    return Err(CatalogError::Corrupt("已发布目录项与请求准入意图不一致"));
                }
                let inode = tr
                    .get(
                        &active_batch::file_claim_key_for(namespace, locator.file_id),
                        true,
                    )
                    .await?
                    .ok_or(CatalogError::Corrupt("已发布目录项缺少 inode 定位记录"))?;
                if active_batch::TargetLocator::decode(&inode)? != locator {
                    return Err(CatalogError::Corrupt("已发布目录项定位记录不一致"));
                }
                id = Some(locator.file_id);
            }
        }
        if let Some(id) = id {
            items.push(Child {
                name: decode_name(suffix)?,
                id,
            });
        }
        last_suffix = Some(suffix.to_vec());
        processed += 1;
    }
    let next = if base_pos < base.items.len()
        || target_pos < targets.items.len()
        || base.next.is_some()
        || targets.next.is_some()
    {
        last_suffix.map(|suffix| {
            let mut key = base_prefix;
            key.extend_from_slice(&suffix);
            key
        })
    } else {
        None
    };
    Ok(Page { items, next })
}

/// Merge the immutable imported reverse index with Active target claims at
/// one read version. The returned cursor uses the existing V-key shape.
pub(super) async fn list_active_volume_copies_at(
    tr: &Transaction,
    volume: Uuid,
    scan: VolumeScan,
    after: Option<&[u8]>,
    limit: usize,
) -> Result<Page<VolumeCopy>> {
    let base_prefix = volume_prefix(volume, scan.id);
    let target_prefix = active_batch::reverse_claim_prefix_for(volume);
    let target_after = after.map(|cursor| {
        let mut key = target_prefix.clone();
        key.extend_from_slice(&cursor[base_prefix.len()..]);
        key
    });
    let base = read_page(tr, &base_prefix, after, limit + 1, true).await?;
    let targets = read_page(tr, &target_prefix, target_after.as_deref(), limit + 1, true).await?;
    let mut base_pos = 0;
    let mut target_pos = 0;
    let mut processed = 0;
    let mut items = Vec::with_capacity(limit);
    let mut last_suffix = None;
    while (base_pos < base.items.len() || target_pos < targets.items.len()) && processed < limit {
        let base_suffix = base
            .items
            .get(base_pos)
            .map(|(key, _)| &key[base_prefix.len()..]);
        let target_suffix = targets
            .items
            .get(target_pos)
            .map(|(key, _)| &key[target_prefix.len()..]);
        let suffix = match (base_suffix, target_suffix) {
            (Some(base), Some(target)) => base.min(target),
            (Some(base), None) => base,
            (None, Some(target)) => target,
            (None, None) => unreachable!(),
        };
        if suffix.len() != 48 {
            return Err(CatalogError::Corrupt("活动卷反向索引键长度"));
        }
        let namespace = uuid_from(&suffix[..16])?;
        let file_id = uuid_from(&suffix[16..32])?;
        let copy_id = uuid_from(&suffix[32..])?;
        if base_suffix == Some(suffix) {
            let value = &base.items[base_pos].1;
            if value.len() != 40 || read_u64(value) == 0 {
                return Err(CatalogError::Corrupt("卷反向索引版本或长度"));
            }
            items.push(VolumeCopy {
                namespace,
                file_id,
                copy_id,
                version: read_u64(value),
            });
            base_pos += 1;
        }
        if target_suffix == Some(suffix) {
            let locator = active_batch::TargetLocator::decode_reverse(
                &targets.items[target_pos].1,
                volume,
                file_id,
            )?;
            if let Some((receipt, _)) = published_receipt_at(tr, namespace, locator).await? {
                if receipt.copy_id != copy_id || receipt.namespace != namespace {
                    return Err(CatalogError::Corrupt("已发布卷反向索引与请求不一致"));
                }
                if base_suffix == Some(suffix) {
                    return Err(CatalogError::Corrupt("已发布卷副本与导入基线重复"));
                }
                items.push(VolumeCopy {
                    namespace,
                    file_id,
                    copy_id,
                    version: candidate_head(&receipt)?.version,
                });
            }
            target_pos += 1;
        }
        last_suffix = Some(suffix.to_vec());
        processed += 1;
    }
    let next = if base_pos < base.items.len()
        || target_pos < targets.items.len()
        || base.next.is_some()
        || targets.next.is_some()
    {
        last_suffix.map(|suffix| {
            let mut key = base_prefix;
            key.extend_from_slice(&suffix);
            key
        })
    } else {
        None
    };
    Ok(Page { items, next })
}

pub(super) async fn resolve_path_at(
    tr: &Transaction,
    namespace: Uuid,
    components: Vec<&[u8]>,
    snapshot: bool,
) -> Result<Option<ResolvedPath>> {
    resolve_path_at_mode(tr, namespace, components, snapshot, true).await
}

async fn resolve_path_at_mode(
    tr: &Transaction,
    namespace: Uuid,
    components: Vec<&[u8]>,
    snapshot: bool,
    load_attributes: bool,
) -> Result<Option<ResolvedPath>> {
    require_namespace_readable(tr, namespace).await?;
    if tr
        .get(&control::managed_key(namespace), true)
        .await?
        .as_deref()
        != Some(&[][..])
    {
        return Err(CatalogError::Invalid("路径查询要求受管命名空间"));
    }
    let root = tr
        .get(&node_key(namespace, Uuid::nil()), snapshot)
        .await?
        .ok_or(CatalogError::Corrupt("命名空间根节点缺失"))?;
    let mut node = decode_node(&root)?;
    if node.kind != NodeKind::Directory || !node.parent.is_nil() || !node.name.is_empty() {
        return Err(CatalogError::Corrupt("命名空间根节点损坏"));
    }
    let mut id = Uuid::nil();
    for component in components {
        if node.kind != NodeKind::Directory {
            return Err(CatalogError::Invalid("路径中间分量不是目录"));
        }
        let Some(child) = tr
            .get(&child_key(namespace, id, component), snapshot)
            .await?
        else {
            if requests::prefix_has_rows(tr, &unavailable_volume_prefix(namespace)).await? {
                return Err(CatalogError::Incomplete);
            }
            return Ok(None);
        };
        let child_id = uuid_from(&child)?;
        if child_id.is_nil() {
            return Err(CatalogError::Corrupt("目录项指向根节点"));
        }
        let raw = tr
            .get(&node_key(namespace, child_id), snapshot)
            .await?
            .ok_or(CatalogError::Corrupt("目录项指向缺失 inode"))?;
        let child_node = decode_node(&raw)?;
        if child_node.parent != id || child_node.name != component {
            return Err(CatalogError::Corrupt("目录项与 inode 不一致"));
        }
        id = child_id;
        node = child_node;
    }
    let _ = load_attributes;
    if node.kind == NodeKind::Directory {
        if tr.get(&head_key(namespace, id), true).await?.is_some() {
            return Err(CatalogError::Corrupt("目录存在文件 head"));
        }
        return Ok(Some(ResolvedPath::Directory {
            id,
            attributes: node.attributes,
        }));
    }
    let head = tr
        .get(&head_key(namespace, id), true)
        .await?
        .ok_or(CatalogError::Corrupt("文件 head 缺失"))?;
    let head = decode_head(&head)?;
    let copy = tr
        .get(&copy_key(namespace, id, head.copy_id), true)
        .await?
        .ok_or(CatalogError::Corrupt("当前物理副本缺失"))?;
    let copy = match decode_stored_copy(&copy)? {
        StoredCopy::Present(copy) => copy,
        StoredCopy::Reclaimed { .. } => {
            return Err(CatalogError::Corrupt("当前物理副本已被回收"));
        }
    };
    if copy.version != head.version || copy.size != head.size {
        return Err(CatalogError::Corrupt(
            "文件 head 与物理副本版本或长度不一致",
        ));
    }
    let placement = require_serving_copy_at(tr, namespace, &copy).await?;
    let scan = VolumeScan {
        generation: copy.volume_generation,
        id: copy.volume_scan_id,
    };
    let reverse = tr
        .get(
            &volume_key(copy.volume_id, scan.id, namespace, id, head.copy_id),
            true,
        )
        .await?
        .ok_or(CatalogError::Corrupt("当前副本缺少卷反向索引"))?;
    if reverse.len() != 40 || read_u64(&reverse) != head.version {
        return Err(CatalogError::Corrupt("当前副本卷反向索引损坏"));
    }
    Ok(Some(ResolvedPath::File {
        id,
        head,
        copy: Box::new(copy),
        placement,
        attributes: node.attributes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_validation_rejects_ambiguous_or_unbounded_paths() {
        assert!(path_components(b"/").unwrap().is_empty());
        assert_eq!(
            path_components(b"/a/b").unwrap(),
            vec![&b"a"[..], &b"b"[..]]
        );
        for path in [
            &b""[..],
            &b"a"[..],
            &b"/a//b"[..],
            &b"/a/"[..],
            &b"/./a"[..],
            &b"/../a"[..],
            &b"/a\0b"[..],
        ] {
            assert!(path_components(path).is_err());
        }
        let deep = format!("/{}", "a/".repeat(usize::from(MAX_DEPTH)));
        assert!(path_components(deep.as_bytes()).is_err());
    }
}
