//! Private disk-backed inventory of one complete physical source view.
//! It proves source facts only, never permission to alter a medium.
use super::*;
use crate::ltfs::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta, Xattr};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ROW_BYTES: usize = 8 << 20;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenamePhysicalSourceStats {
    pub files: u64,
    pub directories: u64,
    pub source_files: u64,
    pub target_files: u64,
    pub other_files: u64,
    pub referenced_data_files: u64,
    pub sha256: [u8; 32],
    pub database_bytes: u64,
}
pub struct NodeRenamePhysicalSource {
    conn: Connection,
    source: ActiveIndexIdentity,
    stats: NodeRenamePhysicalSourceStats,
    original: ([u8; 32], [u8; 32], live_guard::Binding),
    guard_sha256: [u8; 32],
    highest_uid: u64,
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(super) enum Ref<'a> {
    File {
        node: &'a FileNode,
    },
    Directory {
        meta: &'a NodeMeta,
        xattrs: &'a [Xattr],
    },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Body {
    File { node: FileNode },
    Directory { meta: NodeMeta, xattrs: Vec<Xattr> },
}
fn bad() -> CatalogError {
    CatalogError::Corrupt("rename 完整物理来源与有效 UID 引用不符")
}
fn subtree(root: &str, path: &str) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|p| p.starts_with('/'))
}
pub(super) fn row_hash(path: &str, body: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-physical-row/1")
        .chain_update((path.len() as u64).to_be_bytes())
        .chain_update(path.as_bytes())
        .chain_update(body)
        .finalize()
        .into()
}
struct Buffer(Vec<u8>);
impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > ROW_BYTES)
        {
            return Err(std::io::Error::other("rename 物理节点超过8MiB预算"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode(body: &Ref<'_>) -> Result<Vec<u8>> {
    let mut bytes = Buffer(Vec::new());
    serde_json::to_writer(&mut bytes, body)
        .map_err(|_| invalid("rename 物理节点编码失败或超过预算"))?;
    Ok(bytes.0)
}
pub(super) fn attributes(
    meta: &NodeMeta,
    xattrs: &[Xattr],
    symlink: Option<&str>,
    highest: u64,
) -> Result<()> {
    if meta.file_uid == 0 || meta.file_uid > highest {
        return Err(bad());
    }
    let mut bytes = symlink.map_or(0, str::len);
    for attr in xattrs {
        bytes = bytes
            .checked_add(attr.key.len())
            .and_then(|n| n.checked_add(attr.value.len()))
            .filter(|n| *n <= ROW_BYTES)
            .ok_or_else(|| invalid("rename 物理节点属性超过8MiB预算"))?;
    }
    let attrs = NodeAttributes::new(meta.clone(), xattrs.to_vec(), symlink.map(str::to_owned))?;
    let mut keys = std::collections::BTreeSet::new();
    if attrs
        .xattrs()
        .ok_or_else(bad)?
        .iter()
        .any(|a| !keys.insert(&a.key))
    {
        return Err(bad());
    }
    Ok(())
}
impl NodeRenamePhysicalSource {
    pub fn source_index(&self) -> ActiveIndexIdentity {
        self.source
    }
    pub fn stats(&self) -> &NodeRenamePhysicalSourceStats {
        &self.stats
    }
    fn row(&self, uid: u64) -> Result<Option<(String, Body)>> {
        let row:Option<(String,Vec<u8>,Vec<u8>)>=self.conn.query_row(
            "SELECT path,sha,CASE WHEN length(body)<=8388608 THEN body END FROM nodes WHERE uid=?",
            [uid.to_be_bytes().as_slice()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .optional().map_err(projection::sql)?;
        row.map(|(path, sha, bytes)| Ok((path.clone(), decode_row(uid, &path, &sha, &bytes)?)))
            .transpose()
    }
    pub(super) fn visit<F: FnMut(&str, u64, &Body) -> Result<()>>(
        &self,
        mut visit: F,
    ) -> Result<()> {
        let mut statement=self.conn.prepare("SELECT path,uid,sha,CASE WHEN length(body)<=8388608 THEN body END FROM nodes ORDER BY path COLLATE BINARY").map_err(projection::sql)?;
        let mut rows = statement.query([]).map_err(projection::sql)?;
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let path: String = row.get(0).map_err(projection::sql)?;
            let raw: Vec<u8> = row.get(1).map_err(projection::sql)?;
            let uid = u64::from_be_bytes(raw.as_slice().try_into().map_err(|_| bad())?);
            let sha: Vec<u8> = row.get(2).map_err(projection::sql)?;
            let bytes: Vec<u8> = row.get(3).map_err(projection::sql)?;
            let body = decode_row(uid, &path, &sha, &bytes)?;
            visit(&path, uid, &body)?;
        }
        Ok(())
    }
    pub(super) fn verify(&self) -> Result<()> {
        let mut hash = Sha256::new()
            .chain_update(b"tape-rs/node-rename-physical-source/1")
            .chain_update(self.guard_sha256)
            .chain_update(self.highest_uid.to_be_bytes());
        let mut files = 0;
        let mut directories = 0;
        self.visit(|path, uid, body| {
            match body {
                Body::File { .. } => files += 1,
                Body::Directory { .. } => directories += 1,
            };
            hash.update(uid.to_be_bytes());
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            hash.update(row_hash(path, &encode_body(body)?));
            Ok(())
        })?;
        if files != self.stats.files
            || directories != self.stats.directories
            || <[u8; 32]>::from(hash.finalize()) != self.stats.sha256
        {
            return Err(bad());
        }
        Ok(())
    }
    fn uid_at(&self, path: &str) -> Result<Option<u64>> {
        let raw: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT uid FROM nodes WHERE path=?", [path], |r| r.get(0))
            .optional()
            .map_err(projection::sql)?;
        raw.map(|v| {
            Ok(u64::from_be_bytes(
                v.as_slice().try_into().map_err(|_| bad())?,
            ))
        })
        .transpose()
    }
    pub(super) fn directory(&self, path: &str) -> Result<Option<(NodeMeta, Vec<Xattr>)>> {
        let Some(uid) = self.uid_at(path)? else {
            return Ok(None);
        };
        match self.row(uid)?.ok_or_else(bad)? {
            (actual, Body::Directory { meta, xattrs }) if actual == path => {
                Ok(Some((meta, xattrs)))
            }
            (_, Body::File { .. }) => Ok(None),
            _ => Err(bad()),
        }
    }
    pub(super) fn file_at(&self, path: &str) -> Result<Option<(u64, FileNode)>> {
        let Some(uid) = self.uid_at(path)? else {
            return Ok(None);
        };
        match self.row(uid)?.ok_or_else(bad)? {
            (actual, Body::File { node }) if actual == path => Ok(Some((uid, node))),
            (_, Body::Directory { .. }) => Ok(None),
            _ => Err(bad()),
        }
    }
    pub(super) fn highest_uid(&self) -> u64 {
        self.highest_uid
    }
    pub(super) fn require_original(
        &self,
        request: &NodeRenameRequest,
        manifest: [u8; 32],
        guard: &NodeRenameVolumeGuard,
    ) -> Result<()> {
        let original = guard.original_projection_binding();
        if original != self.original
            || original.0 != request.fingerprint()
            || original.1 != manifest
            || guard.stats().sha256 != self.guard_sha256
        {
            return Err(bad());
        }
        Ok(())
    }

    pub fn file(&self, uid: u64) -> Result<Option<(String, FileNode)>> {
        if uid == 0 {
            return Err(invalid("物理 fileuid 为零"));
        }
        match self.row(uid)? {
            Some((path, Body::File { node })) => Ok(Some((path, node))),
            Some(_) => Err(invalid("物理 fileuid 指向目录")),
            None => Ok(None),
        }
    }
    fn insert(&self, path: &str, uid: u64, body: Ref<'_>) -> Result<()> {
        let body = encode(&body)?;
        self.conn
            .execute(
                "INSERT INTO nodes(uid,path,sha,body) VALUES(?,?,?,?)",
                params![
                    uid.to_be_bytes().as_slice(),
                    path,
                    row_hash(path, &body).as_slice(),
                    body
                ],
            )
            .map_err(projection::sql)?;
        Ok(())
    }
}

impl NodeRenameVolumeGuard {
    /// Audit a complete recovered view. The caller still needs source-construct,
    /// current task/owner, original global participant and PR checks to write.
    pub fn physical_source(
        &mut self,
        baseline: &NodeRenameBaseline,
        projection: &mut NodeRenameProjectionSpool,
        index: &LtfsIndex,
        max_database_bytes: u64,
    ) -> Result<NodeRenamePhysicalSource> {
        self.require_physical_inputs(baseline, projection)?;
        let source = self.source_index();
        if index.volume_uuid != source.volume_uuid
            || index.generation != source.generation
            || index.self_location != source.location
            || index.incremental != source.incremental
            || (index.incremental && !index.materialized)
            || !index.unknown_elements.is_empty()
            || index
                .volume_lock_state
                .as_deref()
                .is_some_and(|s| s != "unlocked")
        {
            return Err(invalid("rename 物理来源不是原完整、可重写索引"));
        }
        let conn = projection::temporary_database(max_database_bytes)?;
        conn.execute_batch("CREATE TABLE nodes(uid BLOB PRIMARY KEY NOT NULL CHECK(length(uid)=8),path TEXT NOT NULL UNIQUE COLLATE BINARY,
            sha BLOB NOT NULL CHECK(length(sha)=32),body BLOB NOT NULL CHECK(length(body)<=8388608)) WITHOUT ROWID;")
            .map_err(projection::sql)?;
        let mut inventory = NodeRenamePhysicalSource {
            conn,
            source,
            original: self.original_projection_binding(),
            guard_sha256: self.stats().sha256,
            highest_uid: index.highest_file_uid,
            stats: NodeRenamePhysicalSourceStats {
                files: 0,
                directories: 0,
                source_files: 0,
                target_files: 0,
                other_files: 0,
                referenced_data_files: 0,
                sha256: [0; 32],
                database_bytes: 0,
            },
        };
        visit_index(index, &index.root.xattrs, |path, row| {
            match row {
                Ref::Directory { meta, xattrs } => {
                    let owned = ee_internal::declared(xattrs)?;
                    if owned
                        && (subtree(path, &baseline.spec.from) || subtree(path, &baseline.spec.to))
                    {
                        return Err(invalid("rename 不能移动或覆盖卷内 EE 数据目录"));
                    }
                    if self.metadata(meta.file_uid)?.is_some()
                        || self.data(meta.file_uid)?.is_some()
                    {
                        return Err(bad());
                    }
                    inventory.insert(path, meta.file_uid, Ref::Directory { meta, xattrs })?;
                    inventory.stats.directories += 1;
                }
                Ref::File { node } => {
                    inventory.insert(path, node.meta.file_uid, Ref::File { node })?;
                    inventory.stats.files += 1;
                    if let Some(file) = self.metadata(node.meta.file_uid)? {
                        match file.role {
                            NodeRenameReferenceRole::Source => {
                                let expected = projection
                                    .metadata_node(source.volume_uuid, node.meta.file_uid)?
                                    .ok_or_else(bad)?;
                                if expected.path != path {
                                    return Err(bad());
                                }
                                inventory.stats.source_files += 1;
                            }
                            NodeRenameReferenceRole::Target => {
                                if baseline.spec.to != path {
                                    return Err(bad());
                                }
                                inventory.stats.target_files += 1;
                            }
                            NodeRenameReferenceRole::Other => {
                                if subtree(&baseline.spec.from, path)
                                    || subtree(&baseline.spec.to, path)
                                {
                                    return Err(bad());
                                }
                                inventory.stats.other_files += 1;
                            }
                        }
                    }
                    if let Some(refs) = self.data(node.meta.file_uid)? {
                        if node.symlink.is_some() {
                            return Err(bad());
                        }
                        if subtree(&baseline.spec.from, path) && (refs.target > 0 || refs.other > 0)
                            || subtree(&baseline.spec.to, path)
                                && (refs.source > 0 || refs.other > 0)
                        {
                            return Err(invalid("rename 物理路径仍被范围外数据引用保护"));
                        }
                        inventory.stats.referenced_data_files += 1;
                    }
                }
            }
            Ok(())
        })?;
        let actual = &inventory.stats;
        let expected = self.stats();
        if (
            actual.source_files,
            actual.target_files,
            actual.other_files,
            actual.referenced_data_files,
        ) != (
            expected.source_files,
            expected.target_files,
            expected.other_files,
            expected.data_uids,
        ) {
            return Err(bad());
        }
        // Validate canonical source copies after the complete UID index exists;
        // EE data lookup is logarithmic, never a repeated linear tree search.
        let mut after = None;
        if expected.source_files > 0 {
            loop {
                let page = projection.volume_files_page(
                    source.volume_uuid,
                    after.as_deref(),
                    128,
                    ROW_BYTES,
                )?;
                for row in page.items {
                    let ResolvedPath::File { copy, .. } = &row.node else {
                        unreachable!()
                    };
                    inventory.require_copy(copy, &row.path)?;
                }
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        if let Some(ResolvedPath::File { copy, .. }) = &baseline.target
            && copy.volume_id == source.volume_uuid
        {
            inventory.require_copy(copy, &baseline.spec.to)?;
        }
        let mut hash = Sha256::new()
            .chain_update(b"tape-rs/node-rename-physical-source/1")
            .chain_update(expected.sha256)
            .chain_update(index.highest_file_uid.to_be_bytes());
        let mut statement = inventory
            .conn
            .prepare("SELECT uid,path,sha FROM nodes ORDER BY path COLLATE BINARY")
            .map_err(projection::sql)?;
        let mut rows = statement.query([]).map_err(projection::sql)?;
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let uid: Vec<u8> = row.get(0).map_err(projection::sql)?;
            let path: String = row.get(1).map_err(projection::sql)?;
            let sha: Vec<u8> = row.get(2).map_err(projection::sql)?;
            hash.update(uid);
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            hash.update(sha);
        }
        drop(rows);
        drop(statement);
        inventory.stats.sha256 = hash.finalize().into();
        let check: String = inventory
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(projection::sql)?;
        if check != "ok" {
            return Err(bad());
        }
        inventory.stats.database_bytes = projection::database_bytes(&inventory.conn)?;
        if inventory.stats.database_bytes > max_database_bytes {
            return Err(invalid("rename 物理来源超过数据库页预算"));
        }
        inventory
            .conn
            .pragma_update(None, "query_only", true)
            .map_err(projection::sql)?;
        Ok(inventory)
    }
}

impl NodeRenamePhysicalSource {
    fn require_copy(&self, copy: &PhysicalCopy, path: &str) -> Result<()> {
        let uid = copy
            .media
            .as_ref()
            .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
        let (actual, node) = self.file(uid)?.ok_or_else(bad)?;
        if actual != path {
            return Err(bad());
        }
        let data = if copy.media.as_ref().is_some_and(|m| m.ee_alias) {
            let (path, node) = self.file(copy.ltfs_file_uid)?.ok_or_else(bad)?;
            Some((path, node))
        } else {
            None
        };
        // Shared with Node7: keep original data identity separate from metadata.
        let kind = link_kind::classify(&node.xattrs, node.symlink.as_deref())?;
        let expected = copy.media.as_ref().map_or(link_kind::LinkKind::None, |m| {
            if m.ee_alias {
                link_kind::LinkKind::EeAlias
            } else if m.native_symlink {
                link_kind::LinkKind::Native
            } else {
                link_kind::LinkKind::None
            }
        });
        if kind != expected {
            return Err(bad());
        }
        if let Some((data_path, _)) = &data {
            let resolved = import_spool::resolve_ee_alias_target(
                path.trim_start_matches('/'),
                node.symlink.as_deref().ok_or_else(bad)?,
            )
            .map_err(|_| bad())?;
            if resolved != data_path.trim_start_matches('/') {
                return Err(bad());
            }
        }
        let data = data
            .as_ref()
            .map(|(p, n)| -> Result<_> {
                Ok(node_mutation::PhysicalDataIdentity {
                    path: p.trim_start_matches('/'),
                    uid: n.meta.file_uid,
                    length: n.length,
                    checksum: ContentDigest::from_ltfs_xattrs(&n.xattrs)?,
                })
            })
            .transpose()?;
        node_mutation::validate_physical_file_copy(
            copy,
            &node,
            self.source.volume_uuid,
            self.source.generation,
            data,
        )
    }
}

#[cfg(test)]
#[path = "physical_source_tests.rs"]
pub(super) mod tests;

pub(super) fn encode_body(body: &Body) -> Result<Vec<u8>> {
    match body {
        Body::File { node } => encode(&Ref::File { node }),
        Body::Directory { meta, xattrs } => encode(&Ref::Directory { meta, xattrs }),
    }
}
pub(super) fn decode_row(uid: u64, path: &str, sha: &[u8], bytes: &[u8]) -> Result<Body> {
    if bytes.len() > ROW_BYTES || sha != row_hash(path, bytes) {
        return Err(bad());
    }
    let body: Body = serde_json::from_slice(bytes).map_err(|_| bad())?;
    if encode_body(&body)? != bytes {
        return Err(bad());
    }
    let meta = match &body {
        Body::File { node } => {
            if path.rsplit_once('/').map(|v| v.1) != Some(node.name.as_str()) {
                return Err(bad());
            }
            &node.meta
        }
        Body::Directory { meta, .. } => meta,
    };
    if uid == 0 || meta.file_uid != uid {
        return Err(bad());
    }
    Ok(body)
}

/// Visit a complete physical tree with a single path buffer. The callback can
/// encode borrowed rows into a private bounded SQLite index; no tree is cloned.
pub(super) fn visit_index<'a, F>(
    index: &'a LtfsIndex,
    root_xattrs: &'a [Xattr],
    mut emit: F,
) -> Result<()>
where
    F: FnMut(&str, Ref<'a>) -> Result<()>,
{
    if index.incremental && !index.materialized
        || !index.unknown_elements.is_empty()
        || index
            .volume_lock_state
            .as_deref()
            .is_some_and(|s| s != "unlocked")
    {
        return Err(bad());
    }
    struct Frame<'a> {
        dir: &'a DirectoryNode,
        child: usize,
        entered: bool,
        original_len: usize,
    }
    let mut stack = vec![Frame {
        dir: &index.root,
        child: 0,
        entered: false,
        original_len: 0,
    }];
    let mut path = String::new();
    while let Some(frame) = stack.last_mut() {
        let dir = frame.dir;
        if !frame.entered {
            frame.entered = true;
            let at = if path.is_empty() { "/" } else { &path };
            if dir.delta_fields.is_some() {
                return Err(bad());
            }
            let attrs = if path.is_empty() {
                root_xattrs
            } else {
                &dir.xattrs
            };
            attributes(&dir.meta, attrs, None, index.highest_file_uid)?;
            ee_internal::require_location(attrs, at, NodeKind::Directory)?;
            emit(
                at,
                Ref::Directory {
                    meta: &dir.meta,
                    xattrs: attrs,
                },
            )?;
            for node in &dir.files {
                validate_name(node.name.as_bytes())?;
                let original = path.len();
                path.push('/');
                path.push_str(&node.name);
                if node.delta_fields.is_some()
                    || node.open_for_write.unwrap_or(false)
                    || node.symlink.is_some() && (node.length != 0 || !node.extents.is_empty())
                {
                    return Err(bad());
                }
                attributes(
                    &node.meta,
                    &node.xattrs,
                    node.symlink.as_deref(),
                    index.highest_file_uid,
                )?;
                ee_internal::require_location(&node.xattrs, &path, NodeKind::File)?;
                emit(&path, Ref::File { node })?;
                path.truncate(original);
            }
        }
        if frame.child < dir.subdirs.len() {
            let child = &dir.subdirs[frame.child];
            frame.child += 1;
            validate_name(child.name.as_bytes())?;
            let original_len = path.len();
            path.push('/');
            path.push_str(&child.name);
            if stack.len() >= usize::from(MAX_DEPTH) {
                return Err(invalid("rename 物理目录超过深度上限"));
            }
            stack.push(Frame {
                dir: child,
                child: 0,
                entered: false,
                original_len,
            });
        } else {
            let old = stack.pop().expect("nonempty");
            path.truncate(old.original_len);
        }
    }
    Ok(())
}
