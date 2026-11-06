//! Owner-fenced refresh of an already assigned volume. The existing pool gate
//! hides bounded catalog changes; an undo journal restores exact prior bytes.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Refresh {
    version: u8,
    volume: String,
    pub(super) base: String,
    assignment: Vec<u8>,
    ready: Vec<u8>,
    index: Vec<u8>,
}

fn bad() -> CatalogError {
    CatalogError::Corrupt("已分配磁带刷新状态损坏")
}
fn record<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|_| bad())
}
fn parse<T: for<'a> Deserialize<'a> + Serialize>(raw: &[u8]) -> Result<T> {
    let v = serde_json::from_slice(raw).map_err(|_| bad())?;
    if record(&v)? != raw {
        return Err(bad());
    }
    Ok(v)
}
fn snapshot_key(import: &NativeMediaImport) -> Vec<u8> {
    let mut k = import.journal();
    k.push(b'r');
    k
}
pub(super) fn base_key(ns: Uuid, volume: Uuid) -> Vec<u8> {
    let mut k = management::prefix(ns, b'v');
    k.extend_from_slice(volume.as_bytes());
    k
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    version: u8,
    source_path: String,
    target: String,
}

pub(super) fn origin_prefix(ns: Uuid, volume: Uuid, scan: Uuid) -> Vec<u8> {
    let mut k = base_key(ns, volume);
    k.push(b'o');
    k.extend_from_slice(scan.as_bytes());
    k
}
pub(super) fn suppression_prefix(ns: Uuid, volume: Uuid, scan: Uuid) -> Vec<u8> {
    let mut key = base_key(ns, volume);
    key.push(b's');
    key.extend_from_slice(scan.as_bytes());
    key
}
fn suppression_key(import: &NativeMediaImport, uid: u64) -> Result<Vec<u8>> {
    let volume = import.refresh.as_ref().ok_or_else(bad)?.volume()?;
    let mut key = suppression_prefix(import.request.namespace(), volume, import.scan());
    key.extend_from_slice(&uid.to_be_bytes());
    Ok(key)
}

async fn capture_suppressed(tr: &Transaction, import: &NativeMediaImport) -> Result<bool> {
    let r = import.refresh.as_ref().ok_or_else(bad)?;
    let cursor = captured_key(import, b'l', &[]);
    let saved = tr.get(&cursor, false).await?;
    if saved.as_deref() == Some(&[]) {
        return Ok(true);
    }
    let prefix = suppression_prefix(import.request.namespace(), r.volume()?, r.scan()?.id);
    let page = read_page(tr, &prefix, saved.as_deref(), 8, false).await?;
    for (key, value) in &page.items {
        if key.len() != prefix.len() + 8 || !value.is_empty() {
            return Err(bad());
        }
        let uid = &key[prefix.len()..];
        if read_u64(uid) == 0 {
            return Err(bad());
        }
        tr.set(&captured_key(import, b'z', uid), &[]);
    }
    tr.set(&cursor, page.next.as_deref().unwrap_or(&[]));
    Ok(page.next.is_none())
}
async fn superseded(
    tr: &Transaction,
    import: &NativeMediaImport,
    entry: &StagedImportEntry,
) -> Result<bool> {
    let candidate = &entry.path.current().candidate;
    if candidate.deleted || candidate.kind != NodeKind::File {
        return Ok(false);
    }
    let uid = entry.source_file_uids[0].to_be_bytes();
    Ok(tr
        .get(&captured_key(import, b'z', &uid), false)
        .await?
        .is_some()
        && tr
            .get(&captured_key(import, b'c', &uid), false)
            .await?
            .is_none())
}
fn origin_key(import: &NativeMediaImport, volume: Uuid, uid: u64) -> Vec<u8> {
    let mut k = origin_prefix(import.request.namespace(), volume, import.scan());
    k.extend_from_slice(&uid.to_be_bytes());
    k
}
pub(super) fn origin_write(
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
    uid: u64,
    path: &str,
    target: &str,
) -> Result<(Vec<u8>, Vec<u8>)> {
    Ok((
        origin_key(import, proof.index().volume_uuid, uid),
        record(&Origin {
            version: 1,
            source_path: path.into(),
            target: target.into(),
        })?,
    ))
}
fn capture_cursor(import: &NativeMediaImport) -> Vec<u8> {
    let mut k = target::prefix(import);
    k.push(b'k');
    k
}
fn captured_key(import: &NativeMediaImport, kind: u8, value: &[u8]) -> Vec<u8> {
    let mut k = target::prefix(import);
    k.push(kind);
    k.extend_from_slice(value);
    k
}
fn directory_key(import: &NativeMediaImport, path: &str) -> Vec<u8> {
    captured_key(import, b'a', &Sha256::digest(path.as_bytes()))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Captured {
    version: u8,
    uid: u64,
    file: String,
    source_path: String,
    target: String,
}
async fn directory_mapping(
    tr: &Transaction,
    import: &NativeMediaImport,
    source: &str,
    target: &str,
) -> Result<()> {
    if source == "/" || source.is_empty() {
        return Ok(());
    }
    let k = directory_key(import, source);
    let bytes = record(&Origin {
        version: 1,
        source_path: source.into(),
        target: target.into(),
    })?;
    if let Some(saved) = tr.get(&k, false).await? {
        if saved.as_ref() != bytes {
            return Err(CatalogError::Invalid("刷新目录存在不唯一的原路径投影"));
        }
    } else {
        tr.set(&k, &bytes);
    }
    Ok(())
}
async fn ancestors(
    tr: &Transaction,
    import: &NativeMediaImport,
    source: &str,
    target: &str,
) -> Result<()> {
    let mut a = source;
    let mut b = target;
    while let (Some((pa, _)), Some((pb, _))) = (a.rsplit_once('/'), b.rsplit_once('/')) {
        if pa.is_empty() {
            break;
        }
        if pb.is_empty() {
            return Err(bad());
        }
        directory_mapping(tr, import, pa, pb).await?;
        a = pa;
        b = pb;
    }
    Ok(())
}
// Capture each current primary once. UID/path mappings are trusted FDB output,
// kept with the original task and read in bounded pages across owner changes.
pub(super) async fn capture(tr: &Transaction, import: &NativeMediaImport) -> Result<bool> {
    let r = import.refresh.as_ref().ok_or_else(bad)?;
    let saved = tr.get(&capture_cursor(import), false).await?;
    if saved.as_deref() == Some(&[]) {
        return capture_suppressed(tr, import).await;
    }
    let page =
        lookup::list_active_volume_copies_at(tr, r.volume()?, r.scan()?, saved.as_deref(), 4)
            .await?;
    let ns = import.request.namespace();
    for row in &page.items {
        if row.namespace != ns {
            return Err(bad());
        }
        let Some((_, _, copy)) = current(tr, ns, row).await? else {
            // A retained old copy of a newer or retired logical inode is not
            // authority to make that old tape entry current again.
            let raw = tr
                .get(&copy_key(ns, row.file_id, row.copy_id), false)
                .await?
                .ok_or_else(bad)?;
            if let StoredCopy::Present(copy) = decode_stored_copy(&raw)? {
                if copy.volume_id != r.volume()?
                    || copy.volume_scan_id != r.scan()?.id
                    || copy.version != row.version
                {
                    return Err(bad());
                }
                let uid = copy
                    .media
                    .as_ref()
                    .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
                tr.set(&captured_key(import, b'z', &uid.to_be_bytes()), &[]);
            }
            continue;
        };
        if copy.volume_id != r.volume()? || copy.volume_scan_id != r.scan()?.id {
            return Err(bad());
        }
        let target = logical_path(tr, ns, row.file_id, import.pool()).await?;
        let uid = copy
            .media
            .as_ref()
            .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
        let mut old = origin_prefix(ns, r.volume()?, r.scan()?.id);
        old.extend_from_slice(&uid.to_be_bytes());
        let source_path = if let Some(raw) = tr.get(&old, false).await? {
            let origin: Origin = parse(&raw)?;
            if origin.version != 1 {
                return Err(bad());
            }
            origin.source_path
        } else if matches!(
            copy.origin,
            CopyOrigin::FileRequest { .. } | CopyOrigin::NodeLink { .. }
        ) {
            target.clone()
        } else if let Some(media) = copy.media.as_ref().filter(|m| !m.ee_alias) {
            format!("/{}", media.physical_path.trim_start_matches('/'))
        } else {
            let relative = target
                .strip_prefix(r.base.trim_end_matches('/'))
                .ok_or_else(bad)?;
            if !relative.starts_with('/') {
                return Err(bad());
            }
            relative.into()
        };
        let captured = Captured {
            version: 1,
            uid,
            file: row.file_id.to_string(),
            source_path: source_path.clone(),
            target: target.clone(),
        };
        let bytes = record(&captured)?;
        for key in [
            captured_key(import, b'c', &uid.to_be_bytes()),
            captured_key(import, b'f', &Sha256::digest(source_path.as_bytes())),
        ] {
            if let Some(old) = tr.get(&key, false).await? {
                if old.as_ref() != bytes {
                    return Err(bad());
                }
            } else {
                tr.set(&key, &bytes);
            }
        }
        ancestors(tr, import, &source_path, &target).await?;
    }
    tr.set(&capture_cursor(import), page.next.as_deref().unwrap_or(&[]));
    Ok(false)
}
async fn captured(
    tr: &Transaction,
    import: &NativeMediaImport,
    uid: u64,
    path: &str,
) -> Result<Option<Captured>> {
    let by_uid = tr
        .get(&captured_key(import, b'c', &uid.to_be_bytes()), false)
        .await?;
    let raw = match by_uid {
        Some(raw) => Some(raw),
        None => {
            tr.get(
                &captured_key(import, b'f', &Sha256::digest(path.as_bytes())),
                false,
            )
            .await?
        }
    };
    raw.as_deref().map(parse).transpose()
}
async fn destination(
    tr: &Transaction,
    import: &NativeMediaImport,
    entry: &StagedImportEntry,
) -> Result<String> {
    let path = &entry.path.logical_path;
    let candidate = &entry.path.current().candidate;
    if candidate.kind == NodeKind::File
        && !candidate.deleted
        && let Some(old) = captured(tr, import, entry.source_file_uids[0], path).await?
    {
        let target = if old.source_path == *path || old.target == *path {
            old.target
        } else {
            format!(
                "{}{}",
                old.target.strip_suffix(&old.source_path).unwrap_or(
                    import
                        .refresh
                        .as_ref()
                        .ok_or_else(bad)?
                        .base
                        .trim_end_matches('/')
                ),
                path
            )
        };
        return Ok(target);
    }
    if candidate.kind == NodeKind::Directory {
        if let Some(raw) = tr.get(&directory_key(import, path), false).await? {
            let mapping: Origin = parse(&raw)?;
            if mapping.version != 1 || mapping.source_path != *path {
                return Err(bad());
            }
            return Ok(mapping.target);
        }
        // Native empty directories have no data-copy reverse row.
        let literal = pool_catalog_path(import.pool(), path.as_bytes())?;
        if let Some(ResolvedPath::Directory {
            attributes: Some(attrs),
            ..
        }) = lookup::resolve_active_path_at_mode(
            tr,
            import.request.namespace(),
            lookup::path_components(&literal)?,
            false,
        )
        .await?
            && attrs.meta().map(|m| m.file_uid)
                == entry.current_attributes.meta().map(|m| m.file_uid)
        {
            return Ok(path.clone());
        }
    }
    let (parent, name) = path.rsplit_once('/').ok_or_else(bad)?;
    if !parent.is_empty()
        && let Some(raw) = tr.get(&directory_key(import, parent), false).await?
    {
        let mapping: Origin = parse(&raw)?;
        if mapping.version != 1 || mapping.source_path != parent {
            return Err(bad());
        }
        return Ok(format!("{}/{}", mapping.target.trim_end_matches('/'), name));
    }
    Ok(format!(
        "{}{}",
        target::base(import)?.trim_end_matches('/'),
        path
    ))
}
fn undo_prefix(import: &NativeMediaImport) -> Vec<u8> {
    let mut k = target::prefix(import);
    k.push(b'u');
    k
}
fn undo_key(import: &NativeMediaImport, kind: u8, path: &str) -> Vec<u8> {
    let mut k = undo_prefix(import);
    k.push(kind);
    k.extend_from_slice(&Sha256::digest(path.as_bytes()));
    k
}
fn sweep_key(import: &NativeMediaImport) -> Vec<u8> {
    let mut k = target::prefix(import);
    k.push(b's');
    k
}

impl Refresh {
    pub(super) fn volume(&self) -> Result<Uuid> {
        Uuid::parse_str(&self.volume).map_err(|_| bad())
    }
    fn scan(&self) -> Result<VolumeScan> {
        Ok(decode_ready_volume(&self.ready)?.0)
    }
    fn index(&self) -> Result<ActiveIndexIdentity> {
        active_volume::decode_index(self.volume()?, &self.index)
    }
}

pub(super) async fn load(tr: &Transaction, import: &NativeMediaImport) -> Result<Option<Refresh>> {
    tr.get(&snapshot_key(import), false)
        .await?
        .as_deref()
        .map(parse)
        .transpose()
}

pub(super) async fn prepare(
    tr: &Transaction,
    import: &NativeMediaImport,
) -> Result<Option<Refresh>> {
    let Some(raw) = tr
        .get(&assignment_key(import.serial(), import.barcode()), false)
        .await?
    else {
        return Ok(None);
    };
    if !matches!(
        import.request.value()["change"]["kind"].as_str(),
        Some("media-import")
    ) {
        return Err(CatalogError::Invalid("已分配磁带不能再次 assign"));
    }
    let assigned = decode_assignment(import.serial(), import.barcode(), &raw)?;
    let volume = assigned
        .volume_id
        .ok_or(CatalogError::Invalid("条码的既有分配尚未绑定卷"))?;
    if assigned.namespace != import.request.namespace() || assigned.pool_id != import.pool() {
        return Err(CatalogError::Invalid("已分配磁带只能在原带库和原池刷新"));
    }
    if import.request.value()["change"]["volume"]
        .as_str()
        .is_some_and(|s| s != volume.to_string())
    {
        return Err(CatalogError::Invalid("刷新请求的卷 UUID 与既有分配不符"));
    }
    if !assigned.allows_io() {
        return Err(CatalogError::Invalid("离线或黑名单磁带不能刷新"));
    }
    let placement = verified_placement(tr, volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.library_serial != import.serial()
        || placement.barcode != import.barcode()
        || placement.namespace != assigned.namespace
        || placement.pool_id != assigned.pool_id
    {
        return Err(bad());
    }
    let (_, index) = active_volume::observed_at(tr, import.serial(), volume).await?;
    if active_volume::recovery_active(tr, volume).await? {
        return Err(CatalogError::Incomplete);
    }
    let mut index_bytes = Vec::new();
    active_volume::encode_index(&mut index_bytes, index);
    let base = match tr.get(&base_key(assigned.namespace, volume), false).await? {
        Some(v) => String::from_utf8(v.to_vec()).map_err(|_| bad())?,
        None => {
            let (scan, _) = active_volume::observed_at(tr, import.serial(), volume).await?;
            if index_evidence::require_source_file_count(tr, volume, scan).await? == 0 {
                "/".into()
            } else {
                target::base(import)?
            }
        }
    };
    let projected = pool_catalog_path(import.pool(), base.as_bytes())?;
    if !matches!(
        lookup::resolve_active_path_at_mode(
            tr,
            assigned.namespace,
            lookup::path_components(&projected)?,
            false
        )
        .await?,
        Some(ResolvedPath::Directory { .. })
    ) {
        return Err(CatalogError::Invalid(
            "刷新必须使用原有投影目录，不能另建导入副本",
        ));
    }
    let value = Refresh {
        version: 1,
        volume: volume.to_string(),
        base,
        assignment: raw.to_vec(),
        ready: tr
            .get(&ready_volume_key(volume), false)
            .await?
            .ok_or(CatalogError::Incomplete)?
            .to_vec(),
        index: index_bytes,
    };
    tr.set(&snapshot_key(import), &record(&value)?);
    Ok(Some(value))
}

pub(super) async fn require(tr: &Transaction, import: &NativeMediaImport) -> Result<()> {
    let r = import.refresh.as_ref().ok_or_else(bad)?;
    if r.version != 1
        || load(tr, import).await?.as_ref() != Some(r)
        || tr
            .get(&assignment_key(import.serial(), import.barcode()), false)
            .await?
            .as_deref()
            != Some(r.assignment.as_slice())
        || tr
            .get(&ready_volume_key(r.volume()?), false)
            .await?
            .as_deref()
            != Some(r.ready.as_slice())
    {
        return Err(bad());
    }
    if active_volume::observed_at(tr, import.serial(), r.volume()?)
        .await?
        .1
        != r.index()?
    {
        return Err(bad());
    }
    Ok(())
}

pub(super) async fn bind(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
) -> Result<()> {
    require(tr, import).await?;
    let old = import.refresh.as_ref().ok_or_else(bad)?;
    let index = proof.identity.head();
    if index.volume_uuid != old.volume()? {
        return Err(CatalogError::Invalid("刷新介质 UUID 与既有分配不符"));
    }
    let expected = old.index()?;
    let actual = ActiveIndexIdentity {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.location,
        sha256: index.sha256,
        incremental: index.incremental,
    };
    if actual.generation < expected.generation
        || (actual.generation == expected.generation && actual != expected)
    {
        return Err(CatalogError::Invalid("刷新索引早于或偏离 FDB 当前卷索引"));
    }
    let claim = volume_claim_key(old.volume()?);
    if let Some(raw) = tr.get(&claim, false).await?
        && raw.as_ref() != import.token()
    {
        return Err(CatalogError::Invalid("卷正在被另一任务接入"));
    }
    tr.set(&claim, &import.token());
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Undo {
    version: u8,
    path: String,
    fingerprint: String,
    changes: Vec<Change>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    key: Vec<u8>,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}
async fn apply(
    tr: &Transaction,
    import: &NativeMediaImport,
    kind: u8,
    path: &str,
    fingerprint: &str,
    writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
) -> Result<()> {
    let key = undo_key(import, kind, path);
    if tr.get(&key, false).await?.is_some() {
        return Err(bad());
    }
    let mut changes = Vec::new();
    for (key, after) in writes {
        changes.push(Change {
            before: tr.get(&key, false).await?.map(|v| v.to_vec()),
            key,
            after,
        });
    }
    let undo = Undo {
        version: 1,
        path: path.into(),
        fingerprint: fingerprint.into(),
        changes,
    };
    let bytes = record(&undo)?;
    if bytes.len() > 90_000 {
        return Err(CatalogError::Invalid("单条索引刷新撤销记录超过预算"));
    }
    tr.set(&key, &bytes);
    for change in undo.changes {
        match change.after {
            Some(v) => tr.set(&change.key, &v),
            None => tr.clear(&change.key),
        }
    }
    Ok(())
}
fn checked_undo(
    import: &NativeMediaImport,
    key: &[u8],
    raw: &[u8],
    proof: &NativeMediaSource,
) -> Result<Undo> {
    let undo: Undo = parse(raw)?;
    if undo.version != 1
        || undo.changes.len() > 8
        || undo.path.len() > 2048
        || lookup::path_components(undo.path.as_bytes()).is_err()
        || undo.path == "/"
        || undo.fingerprint.len() != 64
        || !undo
            .fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || !b"ep"
            .iter()
            .any(|kind| undo_key(import, *kind, &undo.path) == key)
        || undo
            .changes
            .iter()
            .map(|c| &c.key)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != undo.changes.len()
    {
        return Err(bad());
    }
    for c in &undo.changes {
        validate_undo_key(import, proof, &c.key)?;
    }
    Ok(undo)
}
async fn replay(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
    fingerprint: &str,
) -> Result<bool> {
    let Some(raw) = tr.get(&undo_key(import, b'e', path), false).await? else {
        return Ok(false);
    };
    let value = tr.get(&import.gate(), false).await?.ok_or_else(bad)?;
    let proof = source(&decode(&value)?["source"])?.ok_or(CatalogError::Incomplete)?;
    let undo = checked_undo(import, &undo_key(import, b'e', path), &raw, &proof)?;
    if undo.path != path || undo.fingerprint != fingerprint {
        return Err(bad());
    }
    for c in undo.changes {
        if tr.get(&c.key, false).await?.as_deref() != c.after.as_deref() {
            return Err(bad());
        }
    }
    Ok(true)
}

pub(super) async fn project(
    tr: &Transaction,
    import: &NativeMediaImport,
    entry: &StagedImportEntry,
    write: bool,
    value: Value,
    proof: &NativeMediaSource,
) -> Result<()> {
    let ns = import.request.namespace();
    let path = &entry.path.logical_path;
    let fingerprint = entry_fingerprint(entry, proof);
    if replay(tr, import, path, &fingerprint).await? {
        return Ok(());
    }
    if superseded(tr, import, entry).await? {
        if !write {
            return Ok(());
        }
        return save_entry(
            tr,
            import,
            entry,
            proof,
            value,
            vec![(
                suppression_key(import, entry.source_file_uids[0])?,
                Some(Vec::new()),
            )],
        )
        .await;
    }
    let target = destination(tr, import, entry).await?;
    if !write {
        if entry.path.current().candidate.kind == NodeKind::File
            && !entry.path.current().candidate.deleted
        {
            ancestors(tr, import, path, &target).await?;
        }
        return Ok(());
    }
    pool_catalog_path(import.pool(), target.as_bytes())?;
    let (parent, name) = target_parent(tr, import, &target).await?;
    let existing = target::child_at(tr, import, parent, &name).await?;
    let mut old = None;
    if let Some(id) = existing {
        let raw = tr.get(&node_key(ns, id), false).await?.ok_or_else(bad)?;
        let node = decode_node(&raw)?;
        if node.parent != parent || node.name != name {
            return Err(bad());
        }
        old = Some((id, node));
    }
    let candidate = &entry.path.current().candidate;
    let mut writes = Vec::new();
    if !candidate.deleted {
        let id = old
            .as_ref()
            .map(|(id, _)| *id)
            .unwrap_or_else(|| import_materialize::import_inode(ns, import.scan(), &target));
        if old.as_ref().is_some_and(|(_, n)| n.kind != candidate.kind) {
            return Err(CatalogError::Invalid("刷新目标的文件与目录类型冲突"));
        }
        if candidate.kind == NodeKind::Directory {
            directory_mapping(tr, import, path, &target).await?;
            if old.is_none() {
                writes.extend([
                    (child_key(ns, parent, &name), Some(id.as_bytes().to_vec())),
                    (
                        node_key(ns, id),
                        Some(encode_node(&Node {
                            kind: NodeKind::Directory,
                            directory_version: Some(1),
                            parent,
                            name,
                            attributes: Some(entry.current_attributes.clone()),
                        })),
                    ),
                ]);
            }
        } else {
            let old_head = if old.is_some() {
                let head = decode_head(&tr.get(&head_key(ns, id), false).await?.ok_or_else(bad)?)?;
                let copy = decode_copy(
                    &tr.get(&copy_key(ns, id, head.copy_id), false)
                        .await?
                        .ok_or_else(bad)?,
                )?;
                if copy.volume_id != proof.index().volume_uuid
                    || copy.version != head.version
                    || copy.size != head.size
                {
                    return Err(CatalogError::Invalid(
                        "刷新路径的当前文件来自其他磁带或副本不一致",
                    ));
                }
                Some(head)
            } else {
                None
            };
            let binding = ImportSourceBinding {
                placement: VolumePlacement {
                    volume_id: proof.index().volume_uuid,
                    namespace: ns,
                    pool_id: import.pool(),
                    library_serial: import.serial().into(),
                    barcode: import.barcode().into(),
                },
                scan: VolumeScan {
                    generation: proof.index().generation,
                    id: import.scan(),
                },
                source: proof.index(),
            };
            let mut plan = import_materialize::plan_data_file(ns, entry, &[binding])?
                .ok_or(CatalogError::Incomplete)?;
            let version = old_head.map_or(Ok(1), |h| h.version.checked_add(1).ok_or_else(bad))?;
            plan.head.version = version;
            plan.copies[0].copy.version = version;
            let copy = &plan.copies[0];
            let bytes = encode_physical_copy(&copy.copy)?;
            let mut reverse = version.to_be_bytes().to_vec();
            reverse.extend_from_slice(&Sha256::digest(&bytes));
            if old.is_none() && tr.get(&node_key(ns, id), false).await?.is_some() {
                return Err(CatalogError::Invalid("刷新新增路径与历史 inode 冲突"));
            }
            writes.extend([
                (child_key(ns, parent, &name), Some(id.as_bytes().to_vec())),
                (
                    node_key(ns, id),
                    Some(encode_node(&Node {
                        kind: NodeKind::File,
                        directory_version: None,
                        parent,
                        name,
                        attributes: Some(plan.attributes),
                    })),
                ),
                (head_key(ns, id), Some(encode_head(&plan.head))),
                (copy_key(ns, id, copy.id), Some(bytes)),
                (
                    volume_key(
                        copy.copy.volume_id,
                        copy.copy.volume_scan_id,
                        ns,
                        id,
                        copy.id,
                    ),
                    Some(reverse),
                ),
            ]);
        }
    }
    if !candidate.deleted {
        let (key, bytes) = origin_write(import, proof, entry.source_file_uids[0], path, &target)?;
        writes.push((key, Some(bytes)));
    }
    if !candidate.deleted {
        let raw = tr
            .get(&node_key(ns, parent), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if decode_node(&raw)?.kind != NodeKind::Directory {
            return Err(bad());
        }
        attribute_objects::require_attributes(tr, ns, Some(&entry.current_attributes)).await?;
    }
    save_entry(tr, import, entry, proof, value, writes).await
}

async fn save_entry(
    tr: &Transaction,
    import: &NativeMediaImport,
    entry: &StagedImportEntry,
    proof: &NativeMediaSource,
    mut value: Value,
    writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
) -> Result<()> {
    let path = &entry.path.logical_path;
    let fingerprint = entry_fingerprint(entry, proof);
    if value["phase"] != "materialize"
        || value["after"].as_str().is_some_and(|s| s >= path.as_str())
    {
        return Err(bad());
    }
    target::require_ready(tr, import, proof).await?;
    apply(tr, import, b'e', path, &fingerprint, writes).await?;
    value["rows"] = json!(
        value["rows"]
            .as_u64()
            .ok_or_else(bad)?
            .checked_add(1)
            .ok_or_else(bad)?
    );
    if value["rows"].as_u64().ok_or_else(bad)? > proof.paths {
        return Err(bad());
    }
    value["after"] = json!(path);
    tr.set(&import.gate(), &encode(&value));
    Ok(())
}

async fn current(
    tr: &Transaction,
    ns: Uuid,
    row: &VolumeCopy,
) -> Result<Option<(Node, FileHead, PhysicalCopy)>> {
    let Some(raw) = tr.get(&node_key(ns, row.file_id), false).await? else {
        return Err(bad());
    };
    if let Some(retired) = retired_nodes::from_node_record(ns, row.file_id, &raw)? {
        let historical = tr.get(&head_key(ns, row.file_id), false).await?;
        let historical = historical.as_deref().map(decode_head).transpose()?;
        if historical.is_some() && historical != retired.previous_head {
            return Err(bad());
        }
        return Ok(None);
    }
    let node = decode_node(&raw)?;
    if node.kind != NodeKind::File {
        return Err(bad());
    }
    let head = decode_head(
        &tr.get(&head_key(ns, row.file_id), false)
            .await?
            .ok_or_else(bad)?,
    )?;
    if head.copy_id != row.copy_id || head.version != row.version {
        return Ok(None);
    }
    let copy = decode_copy(
        &tr.get(&copy_key(ns, row.file_id, row.copy_id), false)
            .await?
            .ok_or_else(bad)?,
    )?;
    let reverse = tr
        .get(
            &volume_key(
                copy.volume_id,
                copy.volume_scan_id,
                ns,
                row.file_id,
                row.copy_id,
            ),
            false,
        )
        .await?
        .ok_or_else(bad)?;
    if copy.version != head.version
        || copy.size != head.size
        || reverse.len() != 40
        || reverse[..8] != head.version.to_be_bytes()
    {
        return Err(bad());
    }
    Ok(Some((node, head, copy)))
}
async fn logical_path(tr: &Transaction, ns: Uuid, mut id: Uuid, pool: Uuid) -> Result<String> {
    let root = import_materialize::import_inode(ns, pool, "/");
    let mut names = Vec::new();
    for _ in 0..MAX_DEPTH {
        if id == root {
            names.reverse();
            return Ok(format!("/{}", names.join("/")));
        }
        let node = decode_node(&tr.get(&node_key(ns, id), false).await?.ok_or_else(bad)?)?;
        if tr
            .get(&child_key(ns, node.parent, &node.name), false)
            .await?
            .as_deref()
            != Some(id.as_bytes())
        {
            return Err(bad());
        }
        names.push(String::from_utf8(node.name).map_err(|_| bad())?);
        id = node.parent;
    }
    Err(bad())
}

pub(super) async fn sweep(
    tr: &Transaction,
    import: &NativeMediaImport,
    value: &Value,
) -> Result<bool> {
    let r = import.refresh.as_ref().ok_or_else(bad)?;
    let proof = source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
    if value["phase"] != "materialize" || value["rows"] != proof.paths {
        return Err(CatalogError::Incomplete);
    }
    let saved = tr.get(&sweep_key(import), false).await?;
    if saved.as_deref() == Some(&[]) {
        return Ok(true);
    }
    let after = saved.as_deref();
    let page = lookup::list_active_volume_copies_at(tr, r.volume()?, r.scan()?, after, 8).await?;
    let ns = import.request.namespace();
    for row in &page.items {
        if row.namespace != ns {
            return Err(bad());
        }
        let Some((node, head, copy)) = current(tr, ns, row).await? else {
            continue;
        };
        if copy.volume_id != r.volume()? || copy.volume_scan_id != r.scan()?.id {
            return Err(bad());
        }
        let path = logical_path(tr, ns, row.file_id, import.pool()).await?;
        // A head still pointing at the previous scan was absent or deleted in
        // the complete new source. Other-volume heads are never retired here.
        let retirement = retired_nodes::NodeRetirement {
            namespace: ns,
            pool: import.pool(),
            node: row.file_id,
            path: path.clone(),
            kind: NodeKind::File,
            deleted_version: head.version.checked_add(1).ok_or_else(bad)?,
            previous_attributes: node.attributes.as_ref().ok_or_else(bad)?.digest(),
            previous_head: Some(head),
            management_epoch: import.request.epoch(),
            operation: import.request.operation(),
            result_digest: proof.identity.fingerprint(),
        };
        apply(
            tr,
            import,
            b'p',
            &path,
            &format!("{:x}", Sha256::digest(record(r)?)),
            vec![
                (child_key(ns, node.parent, &node.name), None),
                (
                    node_key(ns, row.file_id),
                    Some(retired_nodes::encode(&retirement)?),
                ),
            ],
        )
        .await?;
    }
    tr.set(&sweep_key(import), page.next.as_deref().unwrap_or(&[]));
    Ok(page.next.is_none())
}

pub(super) async fn publish(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
) -> Result<Value> {
    require(tr, import).await?;
    if tr.get(&sweep_key(import), false).await?.as_deref() != Some(&[]) {
        return Err(CatalogError::Incomplete);
    }
    let ns = import.request.namespace();
    let volume = proof.index().volume_uuid;
    let scan = VolumeScan {
        generation: proof.index().generation,
        id: import.scan(),
    };
    if tr.get(&volume_claim_key(volume), false).await?.as_deref() != Some(import.token().as_slice())
    {
        return Err(bad());
    }
    if tr.get(&scan_key(volume, scan.id), false).await?.is_some()
        || tr.get(&source_key(volume, scan.id), false).await?.is_some()
    {
        return Err(bad());
    }
    let revision = source_revision(tr, ns)
        .await?
        .checked_add(1)
        .ok_or_else(bad)?;
    tr.set(
        &scan_key(volume, scan.id),
        &encode_scan_manifest(scan, import.serial(), ScanState::Ready),
    );
    tr.set(
        &source_key(volume, scan.id),
        &index_evidence::encode_source(&proof.identity, proof.files),
    );
    tr.set(
        &ready_volume_key(volume),
        &encode_ready_volume(scan, import.serial()),
    );
    tr.set(&source_revision_key(ns), &revision.to_be_bytes());
    tr.set(&base_key(ns, volume), target::base(import)?.as_bytes());
    active_volume::clear_unassigned_cursor_at(tr, volume);
    let mut task = management::read_task(tr, import.serial(), &import.request).await?;
    task["status"] = json!("succeeded");
    task["result"] = json!({"pool":import.pool().to_string(),"barcode":import.barcode(),"source":source_value(proof),"refreshed":true});
    task["completed_at_millis"] =
        json!(management::now().max(task["created_at_millis"].as_u64().ok_or_else(bad)?));
    management::write_task(tr, &import.request, &task).await?;
    management::clear_queued(tr, &import.request);
    tr.clear(&volume_claim_key(volume));
    tr.clear(&import.gate());
    tr.clear_range(
        &target::prefix(import),
        &prefix_end(&target::prefix(import)),
    );
    tr.clear_range(&import.journal(), &prefix_end(&import.journal()));
    notifications::tape_changed_at(
        tr,
        import.request.namespace(),
        import.serial(),
        import.barcode(),
        "import_finished",
    )
    .await?;
    Ok(task)
}

pub(super) async fn rollback(
    tr: &Transaction,
    import: &NativeMediaImport,
    value: &Value,
) -> Result<bool> {
    require(tr, import).await?;
    let page = read_page(tr, &undo_prefix(import), None, 8, false).await?;
    if !page.items.is_empty() {
        for (key, raw) in page.items {
            let proof = source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
            let undo = checked_undo(import, &key, &raw, &proof)?;
            for c in undo.changes {
                if tr.get(&c.key, false).await?.as_deref() != c.after.as_deref() {
                    return Err(bad());
                }
                match c.before {
                    Some(v) => tr.set(&c.key, &v),
                    None => tr.clear(&c.key),
                }
            }
            tr.clear(&key);
        }
        return Ok(false);
    }
    let volume = import.refresh.as_ref().ok_or_else(bad)?.volume()?;
    if let Some(raw) = tr.get(&volume_claim_key(volume), false).await? {
        if raw.as_ref() != import.token() {
            return Err(bad());
        }
        tr.clear(&volume_claim_key(volume));
    }
    let mut task = management::read_task(tr, import.serial(), &import.request).await?;
    task["status"] = json!(if value["error"].is_null() {
        "cancelled"
    } else {
        "failed"
    });
    task["result"] = if value["error"].is_null() {
        json!({"cancelled":true,"refreshed":false})
    } else {
        json!({"error":value["error"],"refreshed":false})
    };
    task["completed_at_millis"] =
        json!(management::now().max(task["created_at_millis"].as_u64().ok_or_else(bad)?));
    management::write_task(tr, &import.request, &task).await?;
    management::clear_queued(tr, &import.request);
    tr.clear(&import.gate());
    tr.clear_range(
        &target::prefix(import),
        &prefix_end(&target::prefix(import)),
    );
    tr.clear_range(&import.journal(), &prefix_end(&import.journal()));
    notifications::tape_changed_at(
        tr,
        import.request.namespace(),
        import.serial(),
        import.barcode(),
        "import_finished",
    )
    .await?;
    Ok(true)
}
