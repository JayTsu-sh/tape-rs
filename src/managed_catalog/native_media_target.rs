//! Durable virtual destinations, independent of immutable on-tape evidence.
use super::*;
use serde::{Deserialize, Serialize};

const PROBES: u64 = 16;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    request: String,
    source: String,
    base: String,
    created_at_millis: u64,
    prefix_depth: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Mapping {
    version: u8,
    source_path: String,
    fingerprint: String,
    pub(super) target: String,
    #[serde(with = "uuid_text")]
    pub(super) id: Uuid,
    #[serde(with = "uuid_text")]
    pub(super) parent: Uuid,
    pub(super) name: String,
    pub(super) existing_directory: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    version: u8,
    source_path: String,
    fingerprint: String,
    next_suffix: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    version: u8,
    path: String,
    #[serde(with = "uuid_text")]
    id: Uuid,
}

mod uuid_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(
        value: &Uuid,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        Uuid::parse_str(&text)
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == text)
            .ok_or_else(|| serde::de::Error::custom("目标 UUID 不规范"))
    }
}

fn corrupt() -> CatalogError {
    CatalogError::Corrupt("介质导入目标映射损坏")
}

pub(super) fn prefix(import: &NativeMediaImport) -> Vec<u8> {
    let mut out = management::prefix(import.request.namespace(), b'm');
    out.extend_from_slice(import.pool().as_bytes());
    out.extend_from_slice(import.request.operation().as_bytes());
    out
}

fn row(import: &NativeMediaImport, kind: u8, path: &str) -> Vec<u8> {
    let mut out = prefix(import);
    out.push(kind);
    out.extend_from_slice(&Sha256::digest(path.as_bytes()));
    out
}

pub(super) fn header_key(import: &NativeMediaImport) -> Vec<u8> {
    let mut out = prefix(import);
    out.push(b'h');
    out
}

fn encode_record<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(encode(&serde_json::to_value(value).map_err(|_| corrupt())?))
}

fn record<T: for<'de> Deserialize<'de> + Serialize>(raw: &[u8]) -> Result<T> {
    let value = serde_json::from_value(decode(raw)?).map_err(|_| corrupt())?;
    if encode_record(&value)? != raw {
        return Err(corrupt());
    }
    Ok(value)
}

pub(super) fn base(import: &NativeMediaImport) -> Result<String> {
    if let Some(refresh) = &import.refresh {
        return Ok(refresh.base.clone());
    }
    let change = &import.request.value()["change"];
    if change["kind"] == "media-assign" {
        return Ok("/".into());
    }
    let path = change["path"].as_str().ok_or_else(corrupt)?;
    pool_catalog_path(import.pool(), path.as_bytes())?;
    let merged = change["merge_tapes"].as_bool().ok_or_else(corrupt)?;
    let base = if merged {
        path.to_owned()
    } else {
        format!("{}/{}", path.trim_end_matches('/'), import.barcode())
    };
    pool_catalog_path(import.pool(), base.as_bytes())?;
    Ok(base)
}

fn request_fingerprint(import: &NativeMediaImport) -> String {
    format!("{:x}", Sha256::digest(encode(import.request.value())))
}

fn source_fingerprint(proof: &NativeMediaSource) -> String {
    proof
        .identity
        .fingerprint()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn header(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
) -> Result<Header> {
    let value: Header = record(
        &tr.get(&header_key(import), false)
            .await?
            .ok_or_else(corrupt)?,
    )?;
    if value.version != 1
        || value.request != request_fingerprint(import)
        || value.source != source_fingerprint(proof)
        || value.base != base(import)?
        || value.created_at_millis == 0
        || value.prefix_depth > lookup::path_components(value.base.as_bytes())?.len()
    {
        return Err(corrupt());
    }
    Ok(value)
}

pub(super) async fn bind(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
    fresh: bool,
) -> Result<()> {
    if !fresh {
        header(tr, import, proof).await?;
        return Ok(());
    }
    if requests::prefix_has_rows(tr, &prefix(import)).await? {
        return Err(corrupt());
    }
    let task = management::read_task(tr, import.serial(), &import.request).await?;
    let value = Header {
        version: 1,
        request: request_fingerprint(import),
        source: source_fingerprint(proof),
        base: base(import)?,
        created_at_millis: task["created_at_millis"].as_u64().ok_or_else(corrupt)?,
        prefix_depth: 0,
    };
    tr.set(&header_key(import), &encode_record(&value)?);
    Ok(())
}

pub(super) async fn attributes(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
) -> Result<NodeAttributes> {
    let value = header(tr, import, proof).await?;
    let time = chrono::DateTime::from_timestamp_millis(value.created_at_millis as i64)
        .ok_or_else(corrupt)?
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    NodeAttributes::new(
        crate::ltfs::index::NodeMeta {
            // Virtual metadata only; it is never an on-tape fileuid.
            file_uid: 1,
            readonly: false,
            creation_time: time.clone(),
            change_time: time.clone(),
            modify_time: time.clone(),
            access_time: time.clone(),
            backup_time: time,
        },
        vec![],
        None,
    )
}

pub(super) async fn require_directory(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
    id: Uuid,
) -> Result<()> {
    if let Some(raw) = tr.get(&row(import, b'd', path), false).await? {
        let saved: Directory = record(&raw)?;
        if saved.version != 1 || saved.path != path || saved.id != id {
            return Err(corrupt());
        }
    }
    Ok(())
}

async fn remember_directory(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
    id: Uuid,
) -> Result<()> {
    require_directory(tr, import, path, id).await?;
    tr.set(
        &row(import, b'd', path),
        &encode_record(&Directory {
            version: 1,
            path: path.into(),
            id,
        })?,
    );
    Ok(())
}

/// One prefix component per transaction; synthetic directories do not advance
/// the original source cursor or its verified path count.
pub(super) async fn prefix_step(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
    attributes: &NodeAttributes,
) -> Result<bool> {
    let mut value = header(tr, import, proof).await?;
    let components = lookup::path_components(value.base.as_bytes())?;
    if value.prefix_depth == components.len() {
        return Ok(true);
    }
    let path = format!(
        "/{}",
        components[..=value.prefix_depth]
            .iter()
            .map(|v| std::str::from_utf8(v).map_err(|_| corrupt()))
            .collect::<Result<Vec<_>>>()?
            .join("/")
    );
    retired_nodes::require_native_path_allowed(
        tr,
        import.request.namespace(),
        import.pool(),
        &path,
    )
    .await?;
    let (parent, name) = target_parent(tr, import, &path).await?;
    let child = child_key(import.request.namespace(), parent, &name);
    let existing = child_at(tr, import, parent, &name).await?;
    let id = if let Some(id) = existing {
        require_kind(tr, import, &path, id, NodeKind::Directory).await?;
        id
    } else {
        let id = import_materialize::import_inode(import.request.namespace(), import.pool(), &path);
        let writes = vec![
            (child, id.as_bytes().to_vec()),
            (
                node_key(import.request.namespace(), id),
                encode_node(&Node {
                    kind: NodeKind::Directory,
                    directory_version: Some(1),
                    parent,
                    name,
                    attributes: Some(attributes.clone()),
                }),
            ),
        ];
        for (key, _) in &writes {
            if tr.get(key, false).await?.is_some() {
                return Err(corrupt());
            }
        }
        attribute_objects::require_attributes(tr, import.request.namespace(), Some(attributes))
            .await?;
        write_undo(tr, import, b'd', &path, &value.request, &writes)?;
        for (key, bytes) in writes {
            tr.set(&key, &bytes);
        }
        id
    };
    remember_directory(tr, import, &path, id).await?;
    value.prefix_depth += 1;
    let complete = value.prefix_depth == components.len();
    tr.set(&header_key(import), &encode_record(&value)?);
    Ok(complete)
}

pub(super) async fn require_ready(
    tr: &Transaction,
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
) -> Result<()> {
    let value = header(tr, import, proof).await?;
    if value.prefix_depth != lookup::path_components(value.base.as_bytes())?.len() {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

pub(super) async fn child_at(
    tr: &Transaction,
    import: &NativeMediaImport,
    parent: Uuid,
    name: &[u8],
) -> Result<Option<Uuid>> {
    if let Some(id) =
        lookup::published_child_id_at(tr, import.request.namespace(), parent, name).await?
    {
        return Ok(Some(id));
    }
    match tr
        .get(&child_key(import.request.namespace(), parent, name), false)
        .await?
    {
        Some(raw) => Ok(Some(uuid_from(&raw)?)),
        None => Ok(None),
    }
}

async fn require_kind(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
    id: Uuid,
    kind: NodeKind,
) -> Result<()> {
    let catalog_path = pool_catalog_path(import.pool(), path.as_bytes())?;
    let resolved = lookup::resolve_active_path_at_mode(
        tr,
        import.request.namespace(),
        lookup::path_components(&catalog_path)?,
        false,
    )
    .await?;
    let (actual_id, actual_kind) = match resolved {
        Some(ResolvedPath::Directory { id, .. }) => (id, NodeKind::Directory),
        Some(ResolvedPath::File { id, .. }) => (id, NodeKind::File),
        None => return Err(corrupt()),
    };
    if actual_id != id {
        return Err(corrupt());
    }
    if actual_kind != kind {
        return Err(CatalogError::Invalid("导入目标的文件与目录类型冲突"));
    }
    Ok(())
}

pub(super) async fn saved(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
    fingerprint: &str,
) -> Result<Option<Mapping>> {
    let Some(raw) = tr.get(&row(import, b'e', path), false).await? else {
        return Ok(None);
    };
    let value: Mapping = record(&raw)?;
    let expected_base = base(import)?;
    let original = format!("{}{}", expected_base.trim_end_matches('/'), path);
    let suffix = value.target.strip_prefix(&original).ok_or_else(corrupt)?;
    let valid_suffix = suffix.is_empty()
        || suffix
            .strip_prefix('_')
            .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == s));
    if value.version != 1
        || value.source_path != path
        || value.fingerprint != fingerprint
        || value.id.is_nil()
        || value.parent.is_nil()
        || value.target.rsplit('/').next() != Some(value.name.as_str())
        || !valid_suffix
        || (!value.existing_directory
            && value.id
                != import_materialize::import_inode(
                    import.request.namespace(),
                    import.pool(),
                    &value.target,
                ))
    {
        return Err(corrupt());
    }
    pool_catalog_path(import.pool(), value.target.as_bytes())?;
    let (parent, name) = target_parent(tr, import, &value.target).await?;
    if parent != value.parent || name != value.name.as_bytes() {
        return Err(corrupt());
    }
    match child_at(tr, import, parent, &name).await? {
        Some(id) if id == value.id => {}
        None if !value.existing_directory => {
            let tombstone = import_materialize::tombstone_key(
                import.request.namespace(),
                import.pool(),
                &value.target,
            );
            if tr.get(&tombstone, false).await?.is_none() {
                return Err(corrupt());
            }
        }
        _ => return Err(corrupt()),
    }
    Ok(Some(value))
}

/// None means a bounded suffix search advanced its durable cursor. The caller
/// commits this transaction and repeats with the same immutable source entry.
pub(super) async fn choose(
    tr: &Transaction,
    import: &NativeMediaImport,
    entry: &StagedImportEntry,
    fingerprint: &str,
) -> Result<Option<Mapping>> {
    let source_path = &entry.path.logical_path;
    let original = format!("{}{}", base(import)?.trim_end_matches('/'), source_path);
    pool_catalog_path(import.pool(), original.as_bytes())?;
    let probe_key = row(import, b'p', source_path);
    let mut suffix = match tr.get(&probe_key, false).await? {
        Some(raw) => {
            let probe: Probe = record(&raw)?;
            if probe.version != 1
                || probe.source_path != *source_path
                || probe.fingerprint != fingerprint
            {
                return Err(corrupt());
            }
            probe.next_suffix
        }
        None => 0,
    };
    let candidate = &entry.path.current().candidate;
    for _ in 0..PROBES {
        let target = if suffix == 0 {
            original.clone()
        } else {
            format!("{original}_{suffix}")
        };
        pool_catalog_path(import.pool(), target.as_bytes())?;
        retired_nodes::require_native_path_allowed(
            tr,
            import.request.namespace(),
            import.pool(),
            &target,
        )
        .await?;
        let (parent, name) = target_parent(tr, import, &target).await?;
        let existing = child_at(tr, import, parent, &name).await?;
        if tr
            .get(
                &import_materialize::tombstone_key(
                    import.request.namespace(),
                    import.pool(),
                    &target,
                ),
                false,
            )
            .await?
            .is_some()
        {
            return Err(CatalogError::Invalid("导入目标已有删除记录"));
        }
        if candidate.deleted && existing.is_some() {
            return Err(CatalogError::Invalid("导入删除记录与已有路径冲突"));
        }
        if let Some(id) = existing {
            require_kind(tr, import, &target, id, candidate.kind).await?;
            if candidate.kind == NodeKind::File {
                suffix = suffix
                    .checked_add(1)
                    .ok_or(CatalogError::Invalid("导入冲突后缀溢出"))?;
                continue;
            }
        }
        return Ok(Some(Mapping {
            version: 1,
            source_path: source_path.clone(),
            fingerprint: fingerprint.into(),
            id: existing.unwrap_or_else(|| {
                import_materialize::import_inode(import.request.namespace(), import.pool(), &target)
            }),
            parent,
            name: String::from_utf8(name).map_err(|_| corrupt())?,
            target,
            existing_directory: existing.is_some(),
        }));
    }
    tr.set(
        &probe_key,
        &encode_record(&Probe {
            version: 1,
            source_path: source_path.clone(),
            fingerprint: fingerprint.into(),
            next_suffix: suffix,
        })?,
    );
    Ok(None)
}

pub(super) async fn commit(
    tr: &Transaction,
    import: &NativeMediaImport,
    mapping: &Mapping,
    directory: bool,
) -> Result<()> {
    if tr
        .get(&row(import, b'e', &mapping.source_path), false)
        .await?
        .is_some()
    {
        return Err(corrupt());
    }
    if directory {
        remember_directory(tr, import, &mapping.target, mapping.id).await?;
    }
    tr.set(
        &row(import, b'e', &mapping.source_path),
        &encode_record(mapping)?,
    );
    tr.clear(&row(import, b'p', &mapping.source_path));
    Ok(())
}
