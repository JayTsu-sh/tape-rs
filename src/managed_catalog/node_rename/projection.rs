//! Disposable, private disk-backed indexes over a verified original manifest.
//! The FDB manifest remains authoritative; this connection grants no I/O permit.
use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const ROW_BYTES: usize = 8 << 20;
const PAGE_SIZE: u64 = 4096;
const SELECT: &str = "SELECT path,destination,inode,kind,volume,metadata_uid,sha,CASE WHEN length(body)<=8388608 THEN body END FROM nodes";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameProjectionStats {
    pub nodes: u64,
    pub files: u64,
    pub directories: u64,
    pub encoded_bytes: u64,
    pub database_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameProjectionVolume {
    pub volume: Uuid,
    pub files: u64,
}

pub struct NodeRenameProjectionSpool {
    conn: Connection,
    request: NodeRenameRequest,
    expected: NodeRenameSubtreeSummary,
    stats: NodeRenameProjectionStats,
    max_bytes: u64,
    sealed: bool,
}

pub(super) fn sql(error: rusqlite::Error) -> CatalogError {
    CatalogError::Cache(format!("rename 外存清单: {error}"))
}
fn corrupt() -> CatalogError {
    CatalogError::Corrupt("rename 外存清单与原规范节点不一致")
}
fn digest(body: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-projection-row/1")
        .chain_update(body)
        .finalize()
        .into()
}
pub(super) fn database_bytes(conn: &Connection) -> Result<u64> {
    let pages: u64 = conn
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .map_err(sql)?;
    let size: u64 = conn
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(sql)?;
    pages.checked_mul(size).ok_or_else(corrupt)
}

struct Stored {
    path: String,
    destination: String,
    id: Vec<u8>,
    kind: i64,
    volume: Option<Vec<u8>>,
    uid: Option<Vec<u8>>,
    sha: Vec<u8>,
    body: Vec<u8>,
}
fn stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok(Stored {
        path: row.get(0)?,
        destination: row.get(1)?,
        id: row.get(2)?,
        kind: row.get(3)?,
        volume: row.get(4)?,
        uid: row.get(5)?,
        sha: row.get(6)?,
        body: row.get(7)?,
    })
}
fn checked(value: Stored) -> Result<NodeRenameSubtreeEntry> {
    if value.body.len() > ROW_BYTES || value.sha != digest(&value.body) {
        return Err(corrupt());
    }
    let row = NodeRenameSubtreeEntry::decode(&value.body)?;
    let (id, kind, volume, uid) = columns(&row);
    if row.path != value.path
        || row.destination != value.destination
        || value.id != id.as_bytes()
        || value.kind != kind
        || value.volume != volume.map(|v| v.as_bytes().to_vec())
        || value.uid != uid.map(|v| v.to_be_bytes().to_vec())
    {
        return Err(corrupt());
    }
    Ok(row)
}
fn columns(row: &NodeRenameSubtreeEntry) -> (Uuid, i64, Option<Uuid>, Option<u64>) {
    match &row.node {
        ResolvedPath::Directory { id, .. } => (*id, 0, None, None),
        ResolvedPath::File { id, copy, .. } => (
            *id,
            1,
            Some(copy.volume_id),
            Some(
                copy.media
                    .as_ref()
                    .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid),
            ),
        ),
    }
}

pub(super) fn temporary_database(max_bytes: u64) -> Result<Connection> {
    if max_bytes < PAGE_SIZE * 16 {
        return Err(invalid("rename 外存清单至少需要 64 KiB 数据库页预算"));
    }
    // Empty filename is a private temporary on-disk database, not :memory:.
    // SQLite owns its unique name and deletes it on connection close.
    let conn = Connection::open("").map_err(sql)?;
    conn.execute_batch(
        "PRAGMA page_size=4096; PRAGMA journal_mode=DELETE;
        PRAGMA temp_store=FILE; PRAGMA cache_size=-2048; PRAGMA mmap_size=0;
        PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;",
    )
    .map_err(sql)?;
    let forced_memory: bool = conn
        .query_row(
            "SELECT sqlite_compileoption_used('TEMP_STORE=3')",
            [],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if forced_memory {
        return Err(invalid(
            "SQLite 构建强制内存临时库，不能创建 rename 外存清单",
        ));
    }
    conn.pragma_update(
        None,
        "max_page_count",
        (max_bytes / PAGE_SIZE).min(i32::MAX as u64) as i64,
    )
    .map_err(sql)?;
    Ok(conn)
}

impl NodeRenameProjectionSpool {
    pub(super) fn new(
        request: &NodeRenameRequest,
        expected: &NodeRenameSubtreeSummary,
        max_bytes: u64,
    ) -> Result<Self> {
        if !expected.complete || max_bytes < PAGE_SIZE * 16 {
            return Err(invalid("rename 外存清单至少需要 64 KiB 数据库页预算"));
        }
        let conn = temporary_database(max_bytes)?;
        conn.execute_batch("CREATE TABLE volumes(id BLOB PRIMARY KEY NOT NULL CHECK(length(id)=16),files INTEGER NOT NULL CHECK(files>0)) WITHOUT ROWID;
            CREATE TABLE nodes(path TEXT PRIMARY KEY NOT NULL CHECK(length(CAST(path AS BLOB))<=2048),destination TEXT NOT NULL UNIQUE CHECK(length(CAST(destination AS BLOB))<=2048),
                inode BLOB NOT NULL UNIQUE CHECK(length(inode)=16),kind INTEGER NOT NULL CHECK(kind IN(0,1)),
                parent TEXT REFERENCES nodes(path),volume BLOB REFERENCES volumes(id),metadata_uid BLOB,
                sha BLOB NOT NULL CHECK(length(sha)=32),body BLOB NOT NULL CHECK(length(body)<=8388608),
                CHECK((kind=0 AND volume IS NULL AND metadata_uid IS NULL) OR (kind=1 AND volume IS NOT NULL AND metadata_uid IS NOT NULL AND length(volume)=16 AND length(metadata_uid)=8))) WITHOUT ROWID;
            CREATE INDEX files_by_volume ON nodes(volume,path) WHERE kind=1;
            CREATE UNIQUE INDEX metadata_uids ON nodes(volume,metadata_uid) WHERE kind=1;
            CREATE INDEX directories ON nodes(path) WHERE kind=0;").map_err(sql)?;
        let size = database_bytes(&conn)?;
        if size > max_bytes {
            return Err(invalid("rename 外存清单超过数据库页预算"));
        }
        Ok(Self {
            conn,
            request: request.clone(),
            expected: expected.clone(),
            stats: NodeRenameProjectionStats {
                nodes: 0,
                files: 0,
                directories: 0,
                encoded_bytes: 0,
                database_bytes: size,
            },
            max_bytes,
            sealed: false,
        })
    }

    pub(super) fn append(&mut self, rows: &[NodeRenameSubtreeEntry]) -> Result<()> {
        if self.sealed {
            return Err(invalid("rename 外存清单已封口"));
        }
        let mut stats = self.stats.clone();
        let tr = self.conn.transaction().map_err(sql)?;
        for row in rows {
            let body = row.encode()?;
            let encoded_len = body.len() as u64 + 8;
            let decoded = entry::decode_entries(
                &{
                    let mut frame = (body.len() as u64).to_be_bytes().to_vec();
                    frame.extend_from_slice(&body);
                    frame
                },
                1,
                &self.request,
            )?;
            if decoded.first() != Some(row) {
                return Err(corrupt());
            }
            let (id, kind, volume, uid) = columns(row);
            let parent = if row.path == self.request.spec().from {
                if stats.nodes != 0
                    || revision(self.request.spec().source.namespace, &row.node)
                        != self.request.spec().source
                {
                    return Err(corrupt());
                }
                None
            } else {
                let path = row.path.rsplit_once('/').ok_or_else(corrupt)?.0;
                let parent_kind: Option<i64> = tr
                    .query_row("SELECT kind FROM nodes WHERE path=?", [path], |r| r.get(0))
                    .optional()
                    .map_err(sql)?;
                if parent_kind != Some(0) {
                    return Err(corrupt());
                }
                Some(path)
            };
            if let Some(volume) = volume {
                tr.execute("INSERT INTO volumes(id,files) VALUES(?,1) ON CONFLICT(id) DO UPDATE SET files=files+1",[volume.as_bytes().as_slice()]).map_err(sql)?;
            }
            tr.execute("INSERT INTO nodes(path,destination,inode,kind,parent,volume,metadata_uid,sha,body) VALUES(?,?,?,?,?,?,?,?,?)",
                params![row.path,row.destination,id.as_bytes().as_slice(),kind,parent,
                    volume.map(|v|v.as_bytes().to_vec()),uid.map(|v|v.to_be_bytes().to_vec()),digest(&body).as_slice(),body]).map_err(sql)?;
            stats.nodes = stats.nodes.checked_add(1).ok_or_else(corrupt)?;
            if kind == 0 {
                stats.directories += 1;
            } else {
                stats.files += 1;
            }
            stats.encoded_bytes = stats
                .encoded_bytes
                .checked_add(encoded_len)
                .ok_or_else(corrupt)?;
            if stats.nodes > self.expected.entries {
                return Err(corrupt());
            }
        }
        stats.database_bytes = database_bytes(&tr)?;
        if stats.database_bytes > self.max_bytes {
            return Err(invalid("rename 外存清单超过数据库页预算"));
        }
        tr.commit().map_err(sql)?;
        self.stats = stats;
        Ok(())
    }

    pub(super) fn seal(&mut self) -> Result<()> {
        if self.stats.nodes != self.expected.entries
            || self.stats.files != self.expected.files
            || self.stats.directories != self.expected.directories
            || self.stats.encoded_bytes != self.expected.encoded_bytes
        {
            return Err(corrupt());
        }
        let counts:(u64,u64,u64,u64)=self.conn.query_row("SELECT count(*),coalesce(sum(kind=1),0),coalesce(sum(kind=0),0),coalesce(sum(length(body)+8),0) FROM nodes",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(sql)?;
        let groups: u64 = self
            .conn
            .query_row("SELECT coalesce(sum(files),0) FROM volumes", [], |r| {
                r.get(0)
            })
            .map_err(sql)?;
        if counts
            != (
                self.stats.nodes,
                self.stats.files,
                self.stats.directories,
                self.stats.encoded_bytes,
            )
            || groups != self.stats.files
        {
            return Err(corrupt());
        }
        let check: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(sql)?;
        if check != "ok" {
            return Err(corrupt());
        }
        self.conn
            .pragma_update(None, "query_only", true)
            .map_err(sql)?;
        self.sealed = true;
        Ok(())
    }

    pub(super) fn require_manifest(
        &self,
        request: &NodeRenameRequest,
        summary: &NodeRenameSubtreeSummary,
    ) -> Result<()> {
        self.readable()?;
        if &self.request != request || &self.expected != summary {
            return Err(invalid("rename 外存视图不属于原清单"));
        }
        Ok(())
    }
    pub(super) fn source_file(&self, file: Uuid) -> Result<Option<NodeRenameSubtreeEntry>> {
        self.readable()?;
        let mut statement = self
            .conn
            .prepare(&format!("{SELECT} WHERE inode=? AND kind=1"))
            .map_err(sql)?;
        statement
            .query_row([file.as_bytes().as_slice()], stored)
            .optional()
            .map_err(sql)?
            .map(checked)
            .transpose()
    }
    pub(super) fn require_guard(
        &self,
        request: &NodeRenameRequest,
        manifest: [u8; 32],
    ) -> Result<()> {
        self.require_manifest(request, &self.expected)?;
        if self.expected.sha256 != manifest {
            return Err(invalid("rename 物理来源与原规范清单不符"));
        }
        Ok(())
    }
    pub(super) fn volume_file_count(&self, volume: Uuid) -> Result<u64> {
        self.readable()?;
        Ok(self
            .conn
            .query_row(
                "SELECT files FROM volumes WHERE id=?",
                [volume.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?
            .unwrap_or(0))
    }
    pub fn stats(&self) -> &NodeRenameProjectionStats {
        &self.stats
    }
    fn readable(&self) -> Result<()> {
        if self.sealed {
            Ok(())
        } else {
            Err(CatalogError::Incomplete)
        }
    }
    fn cursor_prefix(&self, kind: u8, volume: Option<Uuid>) -> Vec<u8> {
        let mut p = b"rename-projection/1".to_vec();
        p.extend_from_slice(&self.request.fingerprint());
        p.extend_from_slice(&self.expected.sha256);
        p.push(kind);
        if let Some(volume) = volume {
            p.extend_from_slice(volume.as_bytes());
        }
        p
    }
    fn cursor(
        &self,
        kind: u8,
        volume: Option<Uuid>,
        after: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>> {
        let prefix = self.cursor_prefix(kind, volume);
        after
            .map(|raw| {
                raw.strip_prefix(prefix.as_slice())
                    .filter(|s| !s.is_empty())
                    .map(<[u8]>::to_vec)
                    .ok_or(invalid("rename 外存游标不属于原清单或查询"))
            })
            .transpose()
    }
    fn limit(limit: usize) -> Result<()> {
        if (1..=MAX_PAGE).contains(&limit) {
            Ok(())
        } else {
            Err(invalid("rename 外存分页条数必须为 1..=1000"))
        }
    }
    fn volume(&self, id: Uuid) -> Result<()> {
        if id.is_nil()
            || !self
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM volumes WHERE id=?)",
                    [id.as_bytes().as_slice()],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(sql)?
        {
            return Err(invalid("卷不属于原 rename 清单"));
        }
        Ok(())
    }
    pub fn volumes_page(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<NodeRenameProjectionVolume>> {
        self.readable()?;
        Self::limit(limit)?;
        let after = self.cursor(b'v', None, after)?;
        if after.as_ref().is_some_and(|v| v.len() != 16) {
            return Err(invalid("rename 卷游标长度无效"));
        }
        if let Some(raw) = &after {
            self.volume(Uuid::from_slice(raw).map_err(|_| corrupt())?)?;
        }
        let mut statement = self
            .conn
            .prepare(
                "SELECT id,files FROM volumes WHERE (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map(params![after, limit as i64 + 1], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?))
            })
            .map_err(sql)?;
        let mut items = Vec::new();
        let mut more = false;
        for row in rows {
            let (id, files) = row.map_err(sql)?;
            let volume = Uuid::from_slice(&id).map_err(|_| corrupt())?;
            if volume.is_nil() || files == 0 {
                return Err(corrupt());
            }
            if items.len() == limit {
                more = true;
                break;
            }
            items.push(NodeRenameProjectionVolume { volume, files });
        }
        let next = if more {
            let mut p = self.cursor_prefix(b'v', None);
            p.extend_from_slice(items.last().ok_or_else(corrupt)?.volume.as_bytes());
            Some(p)
        } else {
            None
        };
        Ok(Page { items, next })
    }

    fn rows_page(
        &self,
        volume: Option<Uuid>,
        after: Option<&[u8]>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Page<NodeRenameSubtreeEntry>> {
        self.readable()?;
        Self::limit(limit)?;
        if !(1..=ROW_BYTES).contains(&max_bytes) {
            return Err(invalid("rename 外存页字节预算无效"));
        }
        if let Some(volume) = volume {
            self.volume(volume)?;
        }
        let kind = if volume.is_some() { b'f' } else { b'd' };
        let after = self
            .cursor(kind, volume, after)?
            .map(|v| String::from_utf8(v).map_err(|_| invalid("rename 路径游标不是 UTF-8")))
            .transpose()?;
        if let Some(path) = &after {
            self.require_path(path)?;
            let exists: bool = if let Some(volume) = volume {
                self.conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM nodes WHERE kind=1 AND volume=? AND path=?)",
                        params![volume.as_bytes().as_slice(), path],
                        |r| r.get(0),
                    )
                    .map_err(sql)?
            } else {
                self.conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM nodes WHERE kind=0 AND path=?)",
                        [path],
                        |r| r.get(0),
                    )
                    .map_err(sql)?
            };
            if !exists {
                return Err(invalid("rename 路径游标不属于本次查询"));
            }
        }
        let filter = if volume.is_some() {
            " WHERE kind=1 AND volume=?1 AND (?2 IS NULL OR path>?2) ORDER BY path COLLATE BINARY LIMIT ?3"
        } else {
            " WHERE kind=0 AND (?2 IS NULL OR path>?2) ORDER BY path COLLATE BINARY LIMIT ?3"
        };
        let mut statement = self
            .conn
            .prepare(&format!("{SELECT}{filter}"))
            .map_err(sql)?;
        let mut cursor = statement
            .query(params![
                volume.map(|v| v.as_bytes().to_vec()),
                after,
                limit as i64 + 1
            ])
            .map_err(sql)?;
        let mut items = Vec::new();
        let mut used = 0usize;
        let mut more = false;
        while let Some(raw) = cursor.next().map_err(sql)? {
            if items.len() == limit {
                more = true;
                break;
            }
            let saved = stored(raw).map_err(sql)?;
            let charge = saved.body.len().checked_add(8).ok_or_else(corrupt)?;
            if used.checked_add(charge).is_none_or(|n| n > max_bytes) {
                if items.is_empty() {
                    return Err(invalid("rename 外存单项超过页字节预算"));
                }
                more = true;
                break;
            }
            let row = checked(saved)?;
            if volume.is_some_and(|v| columns(&row).2 != Some(v))
                || (volume.is_none() && columns(&row).1 != 0)
            {
                return Err(corrupt());
            }
            used += charge;
            items.push(row);
        }
        let next = if more {
            let mut p = self.cursor_prefix(kind, volume);
            p.extend_from_slice(items.last().ok_or_else(corrupt)?.path.as_bytes());
            Some(p)
        } else {
            None
        };
        Ok(Page { items, next })
    }
    pub fn directories_page(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Page<NodeRenameSubtreeEntry>> {
        self.rows_page(None, after, limit, max_bytes)
    }
    pub fn volume_files_page(
        &self,
        volume: Uuid,
        after: Option<&[u8]>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Page<NodeRenameSubtreeEntry>> {
        self.rows_page(Some(volume), after, limit, max_bytes)
    }
    fn require_path(&self, path: &str) -> Result<()> {
        pool_catalog_path(self.request.spec().pool, path.as_bytes())?;
        let from = &self.request.spec().from;
        if path != from && !path.strip_prefix(from).is_some_and(|s| s.starts_with('/')) {
            return Err(invalid("路径不属于原 rename 子树"));
        }
        Ok(())
    }
    pub fn directory(&self, path: &str) -> Result<Option<NodeRenameSubtreeEntry>> {
        self.readable()?;
        self.require_path(path)?;
        let mut statement = self
            .conn
            .prepare(&format!("{SELECT} WHERE path=? AND kind=0"))
            .map_err(sql)?;
        statement
            .query_row([path], stored)
            .optional()
            .map_err(sql)?
            .map(checked)
            .transpose()
    }
    pub fn metadata_node(&self, volume: Uuid, uid: u64) -> Result<Option<NodeRenameSubtreeEntry>> {
        self.readable()?;
        self.volume(volume)?;
        if uid == 0 {
            return Err(invalid("rename 原 metadata UID 为零"));
        }
        let mut statement = self
            .conn
            .prepare(&format!(
                "{SELECT} WHERE kind=1 AND volume=? AND metadata_uid=?"
            ))
            .map_err(sql)?;
        statement
            .query_row(
                params![volume.as_bytes().as_slice(), uid.to_be_bytes().as_slice()],
                stored,
            )
            .optional()
            .map_err(sql)?
            .map(checked)
            .transpose()
    }
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
