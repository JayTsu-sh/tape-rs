//! Canonical desired physical rows: private desired physical rows, never an I/O permit.
use super::physical_source::{Body, encode_body, row_hash};
use super::*;
use crate::ltfs::index::{DirectoryNode, FileNode, NodeMeta, Xattr};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameVolumeProjectionStats {
    pub source: ActiveIndexIdentity,
    pub before: [u8; 32],
    pub after: [u8; 32],
    pub highest_uid: u64,
    pub files: u64,
    pub directories: u64,
    pub source_files: u64,
    pub source_directories: u64,
    pub database_bytes: u64,
}
pub struct NodeRenameVolumeProjection {
    conn: Connection,
    request: NodeRenameRequest,
    stats: NodeRenameVolumeProjectionStats,
    binding: live_guard::Binding,
    guard_sha256: [u8; 32],
    manifest: [u8; 32],
    highest_before: u64,
    before_files: u64,
    recovery_abandoned: Option<volume_plan::RecoveryAttribute>,
    recovery_marker: Option<volume_plan::RecoveryAttribute>,
}
pub(super) struct Builder<'a> {
    view: NodeRenameVolumeProjection,
    source: &'a mut NodeRenamePhysicalSource,
    guard: &'a mut NodeRenameVolumeGuard,
    max_bytes: u64,
}
fn inside(prefix: &str, path: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|p| p.starts_with('/'))
}
fn parent(path: &str) -> &str {
    let p = path.rsplit_once('/').expect("validated path").0;
    if p.is_empty() { "/" } else { p }
}
fn invalid_source() -> CatalogError {
    CatalogError::Corrupt("rename 按卷物理投影与原清单或来源不符")
}
impl<'a> Builder<'a> {
    pub(super) fn new(
        request: NodeRenameRequest,
        source: &'a mut NodeRenamePhysicalSource,
        guard: &'a mut NodeRenameVolumeGuard,
        highest: u64,
        max_bytes: u64,
    ) -> Result<Self> {
        source.verify()?;
        source.require_original(&request, guard.original_projection_binding().1, guard)?;
        if highest != source.highest_uid() {
            return Err(invalid_source());
        }
        let (_, root_attrs) = source.directory("/")?.ok_or_else(invalid_source)?;
        let recovery_abandoned = volume_plan::RecoveryAttribute::capture(
            &root_attrs,
            crate::ltfs::volume::XATTR_ABANDONED,
        )?;
        let recovery_marker = volume_plan::RecoveryAttribute::capture(
            &root_attrs,
            crate::ltfs::volume::XATTR_EMPTY_RECOVERY,
        )?;
        drop(root_attrs);
        let conn = projection::temporary_database(max_bytes)?;
        conn.execute_batch("CREATE TABLE nodes(path TEXT PRIMARY KEY COLLATE BINARY,tree_key BLOB NOT NULL UNIQUE,uid BLOB NOT NULL UNIQUE CHECK(length(uid)=8),kind INTEGER NOT NULL CHECK(kind IN(0,1)),parent TEXT REFERENCES nodes(path),sha BLOB NOT NULL CHECK(length(sha)=32),body BLOB NOT NULL CHECK(length(body)<=8388608)) WITHOUT ROWID;").map_err(projection::sql)?;
        let from = request.spec().from.clone();
        let to = request.spec().to.clone();
        source.visit(|path, uid, body| {
            if !inside(&from, path) && !inside(&to, path) {
                insert(&conn, path, uid, body)?;
            }
            Ok(())
        })?;
        Ok(Self {
            view: NodeRenameVolumeProjection {
                conn,
                request,
                binding: guard.original_projection_binding().2,
                guard_sha256: guard.stats().sha256,
                manifest: guard.original_projection_binding().1,
                highest_before: highest,
                before_files: source.stats().files,
                recovery_abandoned,
                recovery_marker,
                stats: NodeRenameVolumeProjectionStats {
                    source: source.source_index(),
                    before: source.stats().sha256,
                    after: [0; 32],
                    highest_uid: highest,
                    source_files: 0,
                    source_directories: 0,
                    files: 0,
                    directories: 0,
                    database_bytes: 0,
                },
            },
            source,
            guard,
            max_bytes,
        })
    }
    fn allocate(&mut self) -> Result<u64> {
        self.view.stats.highest_uid = self
            .view
            .stats
            .highest_uid
            .checked_add(1)
            .ok_or(invalid("rename 物理目录 fileuid 已耗尽"))?;
        Ok(self.view.stats.highest_uid)
    }
    pub(super) fn ancestor(
        &mut self,
        node: &NodeMutationAncestor,
        changed_parent: bool,
    ) -> Result<()> {
        let attributes = node.attributes.clone();
        let existing = self.source.directory(&node.path)?;
        let (mut meta, xattrs) = if node.path == "/" {
            let (meta, mut xattrs) = existing.ok_or_else(invalid_source)?;
            xattrs.retain(|a| a.key != super::RENAME_MARKER);
            (meta, xattrs)
        } else {
            let uid = if let Some((meta, _)) = &existing {
                meta.file_uid
            } else {
                // A physical stale file may occupy a canonical directory name.
                // Never overwrite a current metadata or referenced data UID.
                if let Some((uid, _)) = self.source.file_at(&node.path)? {
                    if self.guard.metadata(uid)?.is_some() || self.guard.data(uid)?.is_some() {
                        return Err(invalid_source());
                    }
                    self.view
                        .conn
                        .execute("DELETE FROM nodes WHERE path=?", [&node.path])
                        .map_err(projection::sql)?;
                }
                self.allocate()?
            };
            let mut meta = attributes.meta().ok_or_else(invalid_source)?.clone();
            meta.file_uid = uid;
            (
                meta,
                attributes.xattrs().ok_or_else(invalid_source)?.to_vec(),
            )
        };
        if changed_parent {
            meta.change_time = self.view.request.time().into();
            meta.modify_time = self.view.request.time().into();
        }
        // Root's physical/private xattrs and intrinsic metadata stay physical.
        // Non-root canonical ancestors replace their logical user attributes.
        upsert_directory(&self.view.conn, &node.path, &meta, &xattrs)?;
        Ok(())
    }
    pub(super) fn directory(
        &mut self,
        row: &NodeRenameSubtreeEntry,
        attributes: NodeAttributes,
    ) -> Result<()> {
        let ResolvedPath::Directory {
            attributes: original,
            ..
        } = &row.node
        else {
            return Err(invalid_source());
        };
        if original.as_ref().ok_or_else(invalid_source)?.digest() != attributes.digest() {
            return Err(invalid_source());
        }
        let uid = if let Some((meta, _)) = self.source.directory(&row.path)? {
            meta.file_uid
        } else {
            self.allocate()?
        };
        let mut meta = attributes.meta().ok_or_else(invalid_source)?.clone();
        meta.file_uid = uid;
        if row.path == self.view.request.spec().from {
            meta.change_time = self.view.request.time().into();
        }
        upsert_directory(
            &self.view.conn,
            &row.destination,
            &meta,
            attributes.xattrs().ok_or_else(invalid_source)?,
        )?;
        self.view.stats.source_directories += 1;
        Ok(())
    }
    pub(super) fn file(
        &mut self,
        row: &NodeRenameSubtreeEntry,
        attributes: NodeAttributes,
    ) -> Result<()> {
        let ResolvedPath::File {
            copy,
            attributes: original,
            ..
        } = &row.node
        else {
            return Err(invalid_source());
        };
        if original.as_ref().ok_or_else(invalid_source)?.digest() != attributes.digest() {
            return Err(invalid_source());
        }
        let uid = copy
            .media
            .as_ref()
            .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
        let (path, mut file) = self.source.file(uid)?.ok_or_else(invalid_source)?;
        if path != row.path {
            return Err(invalid_source());
        }
        // Canonical EE text may retain the original metadata depth; native
        // links still require the exact original literal target.
        if !node_mutation::logical_file_link_matches(
            &file,
            attributes.xattrs().ok_or_else(invalid_source)?,
            attributes.symlink(),
            copy.media
                .as_ref()
                .filter(|m| m.ee_alias)
                .map(|m| m.physical_path.as_str()),
        )? {
            return Err(invalid_source());
        }
        file.name = row
            .destination
            .rsplit_once('/')
            .ok_or_else(invalid_source)?
            .1
            .into();
        file.meta = attributes.meta().ok_or_else(invalid_source)?.clone();
        file.meta.file_uid = uid;
        file.xattrs = attributes.xattrs().ok_or_else(invalid_source)?.to_vec();
        if row.path == self.view.request.spec().from {
            file.meta.change_time = self.view.request.time().into();
        }
        if let Some(media) = &copy.media
            && media.ee_alias
        {
            let depth = parent(&row.destination)
                .split('/')
                .filter(|p| !p.is_empty())
                .count();
            file.symlink = Some(format!("{}{}", "../".repeat(depth), media.physical_path));
            stamp_data_directory(&self.view.conn)?;
        }
        insert(
            &self.view.conn,
            &row.destination,
            uid,
            &Body::File { node: file },
        )?;
        self.view.stats.source_files += 1;
        Ok(())
    }
    pub(super) fn finish(
        mut self,
        expected_files: u64,
        expected_directories: u64,
    ) -> Result<NodeRenameVolumeProjection> {
        if self.view.stats.source_files != expected_files
            || self.view.stats.source_directories != expected_directories
        {
            return Err(invalid_source());
        }
        verify_references(self.source, self.guard, &self.view)?;
        let (after, files, directories) = self.view.hash_rows()?;
        let orphan:bool=self.view.conn.query_row("SELECT EXISTS(SELECT 1 FROM nodes child LEFT JOIN nodes parent ON parent.path=child.parent WHERE child.parent IS NOT NULL AND (parent.path IS NULL OR parent.kind!=0))",[],|r|r.get(0)).map_err(projection::sql)?;
        let check: String = self
            .view
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(projection::sql)?;
        if orphan || check != "ok" || projection::database_bytes(&self.view.conn)? > self.max_bytes
        {
            return Err(invalid_source());
        }
        self.view.stats.after = after;
        self.view.stats.files = files;
        self.view.stats.directories = directories;
        self.view.stats.database_bytes = projection::database_bytes(&self.view.conn)?;
        self.view
            .conn
            .pragma_update(None, "query_only", true)
            .map_err(projection::sql)?;
        Ok(self.view)
    }
}

fn insert(conn: &Connection, path: &str, uid: u64, body: &Body) -> Result<()> {
    let bytes = encode_body(body)?;
    let kind = matches!(body, Body::File { .. }) as i64;
    conn.execute(
        "INSERT INTO nodes(path,tree_key,uid,kind,parent,sha,body) VALUES(?,?,?,?,?,?,?)",
        params![
            path,
            tree_key(path),
            uid.to_be_bytes().as_slice(),
            kind,
            if path == "/" {
                None
            } else {
                Some(parent(path))
            },
            row_hash(path, &bytes).as_slice(),
            bytes
        ],
    )
    .map_err(projection::sql)?;
    Ok(())
}
fn upsert_directory(
    conn: &Connection,
    path: &str,
    meta: &NodeMeta,
    xattrs: &[Xattr],
) -> Result<()> {
    let body = Body::Directory {
        meta: meta.clone(),
        xattrs: xattrs.to_vec(),
    };
    let bytes = encode_body(&body)?;
    let changed=conn.execute("INSERT INTO nodes(path,tree_key,uid,kind,parent,sha,body) VALUES(?,?,?,0,?,?,?) ON CONFLICT(path) DO UPDATE SET uid=excluded.uid,kind=0,sha=excluded.sha,body=excluded.body WHERE nodes.kind=0",
        params![path,tree_key(path),meta.file_uid.to_be_bytes().as_slice(),if path=="/"{None}else{Some(parent(path))},row_hash(path,&bytes).as_slice(),bytes]).map_err(projection::sql)?;
    if changed != 1 {
        return Err(invalid_source());
    }
    Ok(())
}

impl NodeRenameVolumeProjection {
    pub(super) fn original_request(&self) -> &NodeRenameRequest {
        &self.request
    }
    pub(super) fn original_binding(&self) -> &live_guard::Binding {
        &self.binding
    }
    pub(super) fn guard_sha256(&self) -> [u8; 32] {
        self.guard_sha256
    }
    pub(super) fn manifest_sha256(&self) -> [u8; 32] {
        self.manifest
    }
    pub(super) fn highest_before(&self) -> u64 {
        self.highest_before
    }
    fn hash_rows(&self) -> Result<([u8; 32], u64, u64)> {
        let mut hash = Sha256::new()
            .chain_update(b"tape-rs/node-rename-volume-projection/1")
            .chain_update(self.request.fingerprint())
            .chain_update(self.stats.before)
            .chain_update(self.highest_before.to_be_bytes())
            .chain_update(self.stats.highest_uid.to_be_bytes());
        let mut files = 0;
        let mut directories = 0;
        self.visit(|path, uid, body| {
            if uid > self.stats.highest_uid {
                return Err(invalid_source());
            }
            match body {
                Body::File { .. } => files += 1,
                Body::Directory { .. } => directories += 1,
            };
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            hash.update(uid.to_be_bytes());
            hash.update(row_hash(path, &encode_body(body)?));
            Ok(())
        })?;
        Ok((hash.finalize().into(), files, directories))
    }
    pub(super) fn verify(&self) -> Result<()> {
        let (sha, files, dirs) = self.hash_rows()?;
        if sha != self.stats.after
            || files != self.stats.files
            || dirs != self.stats.directories
            || self.stats.highest_uid < self.highest_before
            || self.stats.source != self.binding.source
        {
            return Err(invalid_source());
        }
        Ok(())
    }
    pub fn stats(&self) -> &NodeRenameVolumeProjectionStats {
        &self.stats
    }
    fn row(&self, uid: u64) -> Result<Option<(String, Body)>> {
        let value:Option<(String,Vec<u8>,Vec<u8>)>=self.conn.query_row("SELECT path,sha,CASE WHEN length(body)<=8388608 THEN body END FROM nodes WHERE uid=?",[uid.to_be_bytes().as_slice()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(projection::sql)?;
        value
            .map(|(p, s, b)| Ok((p.clone(), physical_source::decode_row(uid, &p, &s, &b)?)))
            .transpose()
    }
    pub fn file(&self, uid: u64) -> Result<Option<(String, FileNode)>> {
        if uid == 0 {
            return Err(invalid("rename 投影 fileuid 为零"));
        }
        match self.row(uid)? {
            None => Ok(None),
            Some((path, Body::File { node })) => Ok(Some((path, node))),
            Some(_) => Err(invalid("rename 投影 fileuid 指向目录")),
        }
    }
    fn row_at(&self, path: &str) -> Result<Option<(u64, Body)>> {
        let uid: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT uid FROM nodes WHERE path=?", [path], |r| r.get(0))
            .optional()
            .map_err(projection::sql)?;
        let Some(raw) = uid else {
            return Ok(None);
        };
        let uid = u64::from_be_bytes(raw.as_slice().try_into().map_err(|_| invalid_source())?);
        let (actual, body) = self.row(uid)?.ok_or_else(invalid_source)?;
        if actual != path {
            return Err(invalid_source());
        }
        Ok(Some((uid, body)))
    }
    pub fn directory(&self, path: &str) -> Result<Option<NodeAttributes>> {
        match self.row_at(path)? {
            None => Ok(None),
            Some((_, Body::Directory { meta, xattrs })) => {
                Ok(Some(NodeAttributes::new(meta, xattrs, None)?))
            }
            Some(_) => Err(invalid("rename 投影路径是文件")),
        }
    }
    fn visit<F: FnMut(&str, u64, &Body) -> Result<()>>(&self, visit: F) -> Result<()> {
        self.visit_rows(false, visit)
    }
    fn visit_rows<F: FnMut(&str, u64, &Body) -> Result<()>>(
        &self,
        tree_order: bool,
        mut visit: F,
    ) -> Result<()> {
        let query = if tree_order {
            "SELECT path,uid,kind,parent,sha,CASE WHEN length(body)<=8388608 THEN body END,tree_key FROM nodes ORDER BY tree_key"
        } else {
            "SELECT path,uid,kind,parent,sha,CASE WHEN length(body)<=8388608 THEN body END,tree_key FROM nodes ORDER BY path COLLATE BINARY"
        };
        let mut stmt = self.conn.prepare(query).map_err(projection::sql)?;
        let mut rows = stmt.query([]).map_err(projection::sql)?;
        while let Some(row) = rows.next().map_err(projection::sql)? {
            let p: String = row.get(0).map_err(projection::sql)?;
            let raw: Vec<u8> = row.get(1).map_err(projection::sql)?;
            let uid = u64::from_be_bytes(raw.as_slice().try_into().map_err(|_| invalid_source())?);
            let kind: i64 = row.get(2).map_err(projection::sql)?;
            let saved_parent: Option<String> = row.get(3).map_err(projection::sql)?;
            let sha: Vec<u8> = row.get(4).map_err(projection::sql)?;
            let bytes: Vec<u8> = row.get(5).map_err(projection::sql)?;
            let body = physical_source::decode_row(uid, &p, &sha, &bytes)?;
            let saved_key: Vec<u8> = row.get(6).map_err(projection::sql)?;
            if saved_key != tree_key(&p)
                || kind != i64::from(matches!(&body, Body::File { .. }))
                || saved_parent.as_deref() != if p == "/" { None } else { Some(parent(&p)) }
            {
                return Err(invalid_source());
            }
            visit(&p, uid, &body)?;
        }
        Ok(())
    }
}
fn stamp_data_directory(conn: &Connection) -> Result<()> {
    let row:Option<(Vec<u8>,Vec<u8>,Vec<u8>)>=conn.query_row("SELECT uid,sha,CASE WHEN length(body)<=8388608 THEN body END FROM nodes WHERE path='/.LTFSEE_DATA'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(projection::sql)?;
    let (raw, sha, bytes) = row.ok_or_else(invalid_source)?;
    let uid = u64::from_be_bytes(raw.as_slice().try_into().map_err(|_| invalid_source())?);
    let Body::Directory { meta, xattrs } =
        physical_source::decode_row(uid, "/.LTFSEE_DATA", &sha, &bytes)?
    else {
        return Err(invalid_source());
    };
    upsert_directory(conn, "/.LTFSEE_DATA", &meta, &ee_internal::stamp(&xattrs)?)
}
fn verify_references(
    source: &NodeRenamePhysicalSource,
    guard: &NodeRenameVolumeGuard,
    view: &NodeRenameVolumeProjection,
) -> Result<()> {
    let spec = view.request.spec();
    source.visit(|path, uid, body| {
        let Body::File { node: before } = body else {
            return Ok(());
        };
        if let Some(file) = guard.metadata(uid)? {
            match file.role {
                NodeRenameReferenceRole::Other => {
                    let Some((after_path, after)) = view.file(uid)? else {
                        return Err(invalid_source());
                    };
                    if after_path != path || after != *before {
                        return Err(invalid_source());
                    }
                }
                NodeRenameReferenceRole::Target => {
                    if view.row(uid)?.is_some() {
                        return Err(invalid_source());
                    }
                }
                NodeRenameReferenceRole::Source => {
                    let Some((after_path, after)) = view.file(uid)? else {
                        return Err(invalid_source());
                    };
                    let suffix = path.strip_prefix(&spec.from).ok_or_else(invalid_source)?;
                    if after_path != format!("{}{}", spec.to, suffix)
                        || before.length != after.length
                        || before.extents != after.extents
                    {
                        return Err(invalid_source());
                    }
                    let kind = link_kind::classify(&before.xattrs, before.symlink.as_deref())?;
                    match kind {
                        link_kind::LinkKind::None => {
                            if after.symlink.is_some()
                                || ContentDigest::from_ltfs_xattrs(&before.xattrs)?
                                    != ContentDigest::from_ltfs_xattrs(&after.xattrs)?
                            {
                                return Err(invalid_source());
                            }
                        }
                        link_kind::LinkKind::Native => {
                            if after.symlink != before.symlink {
                                return Err(invalid_source());
                            }
                        }
                        link_kind::LinkKind::EeAlias => {
                            let original = import_spool::resolve_ee_alias_target(
                                path.trim_start_matches('/'),
                                before.symlink.as_deref().ok_or_else(invalid_source)?,
                            )
                            .map_err(|_| invalid_source())?;
                            let projected = import_spool::resolve_ee_alias_target(
                                after_path.trim_start_matches('/'),
                                after.symlink.as_deref().ok_or_else(invalid_source)?,
                            )
                            .map_err(|_| invalid_source())?;
                            if original != projected {
                                return Err(invalid_source());
                            }
                        }
                    }
                }
            }
        }
        if let Some(refs) = guard.data(uid)?
            && (refs.source > 0 || refs.other > 0)
        {
            let Some((after_path, after)) = view.file(uid)? else {
                return Err(invalid_source());
            };
            if before.symlink.is_some()
                || after.symlink.is_some()
                || before.length != after.length
                || before.extents != after.extents
                || ContentDigest::from_ltfs_xattrs(&before.xattrs)?
                    != ContentDigest::from_ltfs_xattrs(&after.xattrs)?
            {
                return Err(invalid_source());
            }
            if (refs.other > 0 || guard.metadata(uid)?.is_none())
                && (after_path != path || after != *before)
            {
                return Err(invalid_source());
            }
        }
        Ok(())
    })
}
impl Builder<'_> {
    pub(super) fn target(&mut self, baseline: &NodeRenameBaseline) -> Result<()> {
        if let Some(ResolvedPath::File { copy, .. }) = &baseline.target
            && copy.volume_id == self.source.source_index().volume_uuid
            && copy.media.as_ref().is_some_and(|m| m.ee_alias)
        {
            stamp_data_directory(&self.view.conn)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "volume_projection_tests.rs"]
pub(super) mod tests;

/// Consume verified rows in component-wise DFS order (derived BLOB tree_key),
/// building one owned result tree. Caller drops old Working.root first and
/// freezes the volume if any read fails. Committed/published tree stays intact.
pub(super) fn tree_from_rows(
    projection: &NodeRenameVolumeProjection,
    root_name: String,
) -> Result<DirectoryNode> {
    struct Frame {
        node: DirectoryNode,
        path_len: usize,
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut current = String::new();
    projection.visit_rows(true, |path, _, body| {
        if stack.is_empty() {
            let Body::Directory { meta, xattrs } = body else {
                return Err(invalid_source());
            };
            if path != "/" {
                return Err(invalid_source());
            }
            stack.push(Frame {
                node: DirectoryNode {
                    name: root_name.clone(),
                    meta: meta.clone(),
                    xattrs: xattrs.clone(),
                    ..Default::default()
                },
                path_len: 1,
            });
            current.push('/');
            return Ok(());
        }
        let expected_parent = parent(path);
        while current != expected_parent {
            if stack.len() <= 1 {
                return Err(invalid_source());
            }
            let done = stack.pop().expect("child");
            let parent = stack.last_mut().expect("root");
            parent.node.subdirs.push(done.node);
            current.truncate(parent.path_len);
        }
        match body {
            Body::File { node } => stack
                .last_mut()
                .expect("parent")
                .node
                .files
                .push(node.clone()),
            Body::Directory { meta, xattrs } => {
                let name = path
                    .rsplit_once('/')
                    .ok_or_else(invalid_source)?
                    .1
                    .to_string();
                if current != "/" {
                    current.push('/');
                }
                current.push_str(&name);
                stack.push(Frame {
                    node: DirectoryNode {
                        name,
                        meta: meta.clone(),
                        xattrs: xattrs.clone(),
                        ..Default::default()
                    },
                    path_len: current.len(),
                });
            }
        }
        Ok(())
    })?;
    while stack.len() > 1 {
        let done = stack.pop().expect("child");
        stack
            .last_mut()
            .expect("parent")
            .node
            .subdirs
            .push(done.node);
    }
    stack.pop().map(|f| f.node).ok_or_else(invalid_source)
}
fn tree_key(path: &str) -> Vec<u8> {
    path.trim_start_matches('/')
        .bytes()
        .map(|b| if b == b'/' { 0 } else { b })
        .collect()
}

impl NodeRenameVolumeProjection {
    pub(super) fn before_files(&self) -> u64 {
        self.before_files
    }
    pub(super) fn recovery_abandoned(&self) -> &Option<volume_plan::RecoveryAttribute> {
        &self.recovery_abandoned
    }
    pub(super) fn recovery_marker(&self) -> &Option<volume_plan::RecoveryAttribute> {
        &self.recovery_marker
    }
}
