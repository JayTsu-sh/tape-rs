//! Provisional disk-backed replay of streamed index fields. Only per-field
//! SHA-256 values are retained for verification. Source capture can also keep
//! child-free node payloads in the same budgeted database; no tree is retained.

use std::{cell::Cell, fmt::Debug};

use rusqlite::{OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use crate::error::{Result, TapeError};

use super::index::{DirectoryNode, Extent, FileNode, IndexEntry, NodeMeta};

const FIELDS: [&str; 13] = [
    "name",
    "readonly",
    "creationtime",
    "changetime",
    "modifytime",
    "accesstime",
    "backuptime",
    "fileuid",
    "extendedattributes",
    "length",
    "openforwrite",
    "extentinfo",
    "symlink",
];
const UID: usize = 7;
const EXTENTS: usize = 11;
const SYMLINK: usize = 12;
const BODY_LEN: usize = FIELDS.len() * 32;

fn bad(reason: &str) -> TapeError {
    TapeError::Ltfs(format!("索引磁盘重放: {reason}"))
}

fn hash(value: &impl Debug) -> [u8; 32] {
    Sha256::digest(format!("{value:?}").as_bytes()).into()
}

struct Entry {
    path: String,
    kind: i64,
    uid: u64,
    fields: u16,
    deleted: bool,
    symlink: bool,
    body: Vec<u8>,
    payload: Option<Vec<u8>>,
}

impl Entry {
    fn new(entry: IndexEntry) -> Result<Self> {
        let mut body = vec![0; BODY_LEN];
        let mut put = |field: usize, value: [u8; 32]| {
            body[field * 32..(field + 1) * 32].copy_from_slice(&value);
        };
        let (path, kind, name, meta, xattrs, fields, symlink) = match entry {
            IndexEntry::File { path, node } => {
                // Exhaustive destructuring makes new node fields a compile
                // error until their replay/comparison semantics are defined.
                let FileNode {
                    delta_fields,
                    open_for_write,
                    name,
                    length,
                    meta,
                    extents,
                    xattrs,
                    symlink,
                } = node;
                put(9, hash(&length));
                put(10, hash(&open_for_write));
                put(EXTENTS, hash(&extents));
                put(SYMLINK, hash(&symlink));
                (path, 1, name, meta, xattrs, delta_fields, symlink.is_some())
            }
            IndexEntry::Directory { path, node } => {
                let DirectoryNode {
                    delta_fields,
                    name,
                    meta,
                    files,
                    subdirs,
                    xattrs,
                } = node;
                if !files.is_empty() || !subdirs.is_empty() {
                    return Err(bad("只接受不保留子节点的逐条扫描结果"));
                }
                (path, 0, name, meta, xattrs, delta_fields, false)
            }
        };
        if (path.is_empty() && kind != 0)
            || name.contains(['/', '\0'])
            || name == "."
            || name == ".."
            || (!path.is_empty() && (name.is_empty() || path.rsplit('/').next() != Some(&name)))
        {
            return Err(bad("节点名称或路径非法"));
        }
        put(0, hash(&name));
        let NodeMeta {
            readonly,
            creation_time,
            change_time,
            modify_time,
            access_time,
            backup_time,
            file_uid,
        } = meta;
        put(1, hash(&readonly));
        for (field, value) in [
            creation_time,
            change_time,
            modify_time,
            access_time,
            backup_time,
        ]
        .iter()
        .enumerate()
        {
            put(field + 2, hash(value));
        }
        put(UID, hash(&file_uid));
        put(8, hash(&xattrs));
        Ok(Self {
            path,
            kind,
            uid: file_uid,
            fields: FIELDS.iter().enumerate().fold(0, |mask, (i, field)| {
                mask | if fields.as_ref().is_none_or(|fields| fields.contains(*field)) {
                    1 << i
                } else {
                    0
                }
            }),
            deleted: fields
                .as_ref()
                .is_some_and(|fields| fields.contains("deleted")),
            symlink,
            body,
            payload: None,
        })
    }

    fn row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let uid: Vec<u8> = row.get(2)?;
        let uid = u64::from_be_bytes(uid.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?);
        let body: Vec<u8> = row.get(6)?;
        if body.len() != BODY_LEN {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(Self {
            path: row.get(0)?,
            kind: row.get(1)?,
            uid,
            fields: row.get(3)?,
            deleted: row.get(4)?,
            symlink: row.get(5)?,
            body,
            payload: row.get(7)?,
        })
    }

    fn decode_payload_body(&self) -> Result<IndexEntry> {
        decode_payload(
            &self.path,
            self.kind,
            self.uid,
            &self.body,
            self.payload.as_deref(),
        )
    }

    fn decode_payload(&self) -> Result<IndexEntry> {
        let value = self.decode_payload_body()?;
        let decoded = Self::new(value.clone())?;
        if decoded.fields != self.fields
            || decoded.deleted != self.deleted
            || decoded.symlink != self.symlink
        {
            return Err(bad("节点 payload 字段声明或类型不符"));
        }
        Ok(value)
    }

    fn has(&self, field: usize) -> bool {
        self.fields & (1 << field) != 0
    }

    fn require_complete(&self) -> Result<()> {
        for field in [0, 1, 2, 3, 4, UID] {
            if !self.has(field) {
                return Err(bad("新对象缺少完整属性"));
            }
        }
        if self.kind == 1 && !self.has(9) {
            return Err(bad("新文件缺少 length"));
        }
        Ok(())
    }
}

use super::index_scratch::{IndexScratchBudget, ScratchDatabase};

type Current = (i64, u64, Vec<u8>, Option<Vec<u8>>);

pub(super) struct IndexReplay {
    db: ScratchDatabase,
    payloads: bool,
    index_partition: char,
    referenced_index_partition: Cell<bool>,
}

impl IndexReplay {
    pub(super) fn empty_dp(&self) -> Result<bool> {
        Ok(!self
            .db
            .query_row("SELECT EXISTS(SELECT 1 FROM dp WHERE path!=X'')", [], |r| {
                r.get::<_, bool>(0)
            })?)
    }
    pub(super) fn reserved_bytes(&self) -> u64 {
        self.db.reserved_bytes()
    }

    pub(super) fn new(budget: &IndexScratchBudget, index_partition: char) -> Result<Self> {
        Self::create(budget, false, index_partition)
    }

    pub(super) fn capture(budget: &IndexScratchBudget, index_partition: char) -> Result<Self> {
        Self::create(budget, true, index_partition)
    }

    fn create(budget: &IndexScratchBudget, payloads: bool, index_partition: char) -> Result<Self> {
        let db = ScratchDatabase::new(budget)?;
        db.execute_batch(
            "CREATE TABLE raw(construct INTEGER NOT NULL, path BLOB NOT NULL,
                 kind INTEGER NOT NULL, uid BLOB NOT NULL, fields INTEGER NOT NULL,
                 deleted INTEGER NOT NULL, symlink INTEGER NOT NULL, body BLOB NOT NULL, payload BLOB,
                 PRIMARY KEY(construct,path)) WITHOUT ROWID;
             CREATE TABLE dp(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                 uid BLOB NOT NULL, body BLOB NOT NULL, payload BLOB) WITHOUT ROWID;
             CREATE INDEX dp_uid ON dp(uid);
             CREATE TABLE ip(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                 uid BLOB NOT NULL, body BLOB NOT NULL) WITHOUT ROWID;
             CREATE INDEX ip_uid ON ip(uid);
             BEGIN;",
        )?;
        Ok(Self {
            db,
            payloads,
            index_partition,
            referenced_index_partition: Cell::new(false),
        })
    }

    /// Conservatively include every required Full/delta source. A deleted
    /// historical IP extent still prevents accepting an absent IP index.
    pub(super) fn references_index_partition(&self) -> bool {
        self.referenced_index_partition.get()
    }

    pub(super) fn stage(&self, construct: u32, entry: IndexEntry) -> Result<()> {
        if let IndexEntry::File { node, .. } = &entry
            && node
                .extents
                .iter()
                .any(|extent| extent.partition == self.index_partition)
        {
            self.referenced_index_partition.set(true);
        }
        let payload = if self.payloads {
            Some(serde_json::to_vec(&entry).map_err(|e| bad(&e.to_string()))?)
        } else {
            None
        };
        let mut entry = Entry::new(entry)?;
        entry.payload = payload;
        self.db.ensure_room(
            entry.path.len() + BODY_LEN + entry.payload.as_ref().map_or(0, Vec::len) + 64,
        )?;
        self.db
            .prepare_cached("INSERT INTO raw VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?
            .execute(params![
                construct,
                entry.path.as_bytes(),
                entry.kind,
                entry.uid.to_be_bytes().as_slice(),
                entry.fields,
                entry.deleted,
                entry.symlink,
                entry.body,
                entry.payload,
            ])
            .map_err(super::index_scratch::index_row_error)?;
        Ok(())
    }

    pub(super) fn record_ip(&self, entry: IndexEntry) -> Result<()> {
        let entry = Entry::new(entry)?;
        self.db.ensure_room(entry.path.len() + BODY_LEN + 64)?;
        self.db
            .prepare_cached("INSERT INTO ip VALUES(?1,?2,?3,?4)")?
            .execute(params![
                entry.path.as_bytes(),
                entry.kind,
                entry.uid.to_be_bytes().as_slice(),
                entry.body
            ])
            .map_err(super::index_scratch::index_row_error)?;
        Ok(())
    }

    pub(super) fn load_full(&self, construct: u32, highest: u64) -> Result<()> {
        // Copy a row at a time so a large baseline extends its physical
        // reservation incrementally rather than bypassing the page budget.
        let mut rows = self
            .db
            .prepare("SELECT path,kind,uid,body,payload FROM raw WHERE construct=?1")?;
        let mut cursor = rows.query([construct])?;
        while let Some(row) = cursor.next()? {
            let path: Vec<u8> = row.get(0)?;
            let kind: i64 = row.get(1)?;
            let uid: Vec<u8> = row.get(2)?;
            let body: Vec<u8> = row.get(3)?;
            let payload: Option<Vec<u8>> = row.get(4)?;
            self.db
                .ensure_room(path.len() + body.len() + payload.as_ref().map_or(0, Vec::len) + 64)?;
            self.db
                .prepare_cached("INSERT INTO dp VALUES(?1,?2,?3,?4,?5)")?
                .execute(params![path, kind, uid, body, payload])?;
        }
        drop(cursor);
        drop(rows);
        self.discard(construct)?;
        self.validate(false, highest)
    }

    fn discard(&self, construct: u32) -> Result<()> {
        self.db
            .execute("DELETE FROM raw WHERE construct=?1", [construct])?;
        Ok(())
    }

    fn current(&self, path: &str) -> Result<Option<Current>> {
        self.db
            .prepare_cached("SELECT kind,uid,body,payload FROM dp WHERE path=?1")?
            .query_row([path.as_bytes()], |row| {
                let uid: Vec<u8> = row.get(1)?;
                Ok((
                    row.get(0)?,
                    u64::from_be_bytes(uid.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?),
                    row.get(2)?,
                    row.get(3)?,
                ))
            })
            .optional()
            .map_err(Into::into)
    }

    fn remove_tree(&self, path: &str) -> Result<()> {
        if path.is_empty() {
            return Err(bad("不能删除根目录"));
        }
        let mut lower = path.as_bytes().to_vec();
        lower.push(b'/');
        let mut upper = lower.clone();
        *upper.last_mut().unwrap() = b'0';
        self.db.execute(
            "DELETE FROM dp WHERE path=?1 OR (path>=?2 AND path<?3)",
            params![path.as_bytes(), lower, upper],
        )?;
        Ok(())
    }

    pub(super) fn apply_delta(&self, construct: u32, highest: u64) -> Result<()> {
        let mut statement = self.db.prepare(
            "SELECT CAST(path AS TEXT),kind,uid,fields,deleted,symlink,body,payload FROM raw
             WHERE construct=?1 AND deleted=?2 ORDER BY path",
        )?;
        // Deletions precede additions, allowing a UID to move between paths.
        for row in statement.query_map(params![construct, true], Entry::row)? {
            let patch = row?;
            if let Some((kind, _, _, _)) = self.current(&patch.path)?
                && kind == patch.kind
            {
                self.remove_tree(&patch.path)?;
            }
        }
        // Ancestors precede descendants, so a replaced directory discards its
        // old subtree before any new children are installed.
        for row in statement.query_map(params![construct, false], Entry::row)? {
            let mut patch = row?;
            if !patch.path.is_empty() {
                let parent = patch.path.rsplit_once('/').map_or("", |(parent, _)| parent);
                if !self
                    .current(parent)?
                    .is_some_and(|(kind, _, _, _)| kind == 0)
                {
                    return Err(bad("增量对象缺少目录父节点"));
                }
            }
            let prior = self.current(&patch.path)?;
            let matches = prior.as_ref().is_some_and(|(kind, uid, _, _)| {
                *kind == patch.kind && (*uid == patch.uid || (patch.kind == 0 && !patch.has(UID)))
            });
            if patch.path.is_empty() && !matches {
                return Err(bad("根目录 UID 或类型变化"));
            }
            if matches {
                let (kind, uid, mut body, payload) = prior.unwrap();
                if self.payloads {
                    let previous =
                        decode_payload(&patch.path, kind, uid, &body, payload.as_deref())?;
                    let delta = patch.decode_payload()?;
                    patch.payload = Some(merge_payload(previous, delta)?);
                }
                if body.len() != BODY_LEN {
                    return Err(bad("节点字段摘要长度错误"));
                }
                for field in 0..if patch.kind == 0 { 9 } else { FIELDS.len() } {
                    if patch.has(field) && field != SYMLINK {
                        body[field * 32..(field + 1) * 32]
                            .copy_from_slice(&patch.body[field * 32..(field + 1) * 32]);
                    }
                }
                if patch.kind == 1 {
                    if patch.has(EXTENTS) {
                        body[SYMLINK * 32..(SYMLINK + 1) * 32]
                            .copy_from_slice(&hash(&None::<String>));
                    }
                    if patch.has(SYMLINK) && patch.symlink {
                        body[SYMLINK * 32..(SYMLINK + 1) * 32]
                            .copy_from_slice(&patch.body[SYMLINK * 32..(SYMLINK + 1) * 32]);
                        body[EXTENTS * 32..(EXTENTS + 1) * 32]
                            .copy_from_slice(&hash(&Vec::<Extent>::new()));
                    }
                }
                if !patch.has(UID) {
                    patch.uid = uid;
                }
                patch.body = body;
            } else {
                patch.require_complete()?;
                if self.payloads {
                    patch.payload = Some(normalize_payload(patch.decode_payload()?)?);
                }
                self.remove_tree(&patch.path)?;
            }
            if self.payloads {
                patch.decode_payload_body()?;
            }
            self.db.ensure_room(
                patch.path.len() + BODY_LEN + patch.payload.as_ref().map_or(0, Vec::len) + 64,
            )?;
            self.db.prepare_cached("INSERT INTO dp VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(path) DO UPDATE SET kind=excluded.kind,uid=excluded.uid,body=excluded.body,payload=excluded.payload")?
                .execute(params![patch.path.as_bytes(), patch.kind, patch.uid.to_be_bytes().as_slice(), patch.body,patch.payload])?;
        }
        drop(statement);
        self.discard(construct)?;
        self.validate(false, highest)
    }

    pub(super) fn validate(&self, ip: bool, highest: u64) -> Result<()> {
        let invalid: bool = self.db.query_row(
            if ip {
                "SELECT NOT EXISTS(SELECT 1 FROM ip WHERE path=X'' AND kind=0)
                OR EXISTS(SELECT 1 FROM ip WHERE uid=zeroblob(8) OR (?1!=zeroblob(8) AND uid>?1))
                OR EXISTS(SELECT 1 FROM ip GROUP BY uid HAVING count(*)>1)"
            } else {
                "SELECT NOT EXISTS(SELECT 1 FROM dp WHERE path=X'' AND kind=0)
                OR EXISTS(SELECT 1 FROM dp WHERE uid=zeroblob(8) OR (?1!=zeroblob(8) AND uid>?1))
                OR EXISTS(SELECT 1 FROM dp GROUP BY uid HAVING count(*)>1)"
            },
            [highest.to_be_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if invalid {
            return Err(bad("根目录缺失、UID 为零、重复或超过 highestfileuid"));
        }
        Ok(())
    }

    pub(super) fn for_each_payload(
        &self,
        mut emit: impl FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        if !self.payloads {
            return Err(bad("当前重放未捕获完整节点"));
        }
        let mut query = self
            .db
            .prepare("SELECT CAST(path AS TEXT),kind,uid,body,payload FROM dp ORDER BY path")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            emit(complete_payload(row)?)?;
        }
        Ok(())
    }

    /// Seek by the original BLOB path index; an empty path is the root, so
    /// None and Some("") deliberately use different comparison operators.
    pub(super) fn for_each_payload_page(
        &self,
        after: Option<&str>,
        limit: usize,
        mut emit: impl FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        if !self.payloads || !(1..=32).contains(&limit) {
            return Err(bad("节点分页需要完整捕获且每页为 1–32 项"));
        }
        let sql = if after.is_some() {
            "SELECT CAST(path AS TEXT),kind,uid,body,payload FROM dp
             WHERE path>?1 ORDER BY path LIMIT ?2"
        } else {
            "SELECT CAST(path AS TEXT),kind,uid,body,payload FROM dp
             WHERE path>=?1 ORDER BY path LIMIT ?2"
        };
        let mut query = self.db.prepare(sql)?;
        let mut rows = query.query(params![after.unwrap_or("").as_bytes(), limit as u32])?;
        while let Some(row) = rows.next()? {
            emit(complete_payload(row)?)?;
        }
        Ok(())
    }

    /// Resolve one raw node through dp_uid, retaining the full payload proof.
    pub(super) fn payload_by_uid(&self, uid: u64) -> Result<Option<IndexEntry>> {
        if !self.payloads || uid == 0 {
            return Err(bad("UID 查询需要完整节点捕获且 UID 非零"));
        }
        let mut query = self.db.prepare(
            "SELECT CAST(path AS TEXT),kind,uid,body,payload FROM dp WHERE uid=?1 LIMIT 2",
        )?;
        let mut rows = query.query([uid.to_be_bytes().as_slice()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let entry = complete_payload(row)?;
        if rows.next()?.is_some() {
            return Err(bad("完整视图 UID 重复"));
        }
        Ok(Some(entry))
    }

    pub(super) fn require_equal(&self) -> Result<()> {
        let different: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM dp LEFT JOIN ip USING(path)
                WHERE ip.path IS NULL OR dp.kind!=ip.kind OR dp.uid!=ip.uid OR dp.body!=ip.body)
                OR EXISTS(SELECT 1 FROM ip LEFT JOIN dp USING(path) WHERE dp.path IS NULL)",
            [],
            |row| row.get(0),
        )?;
        if different {
            return Err(TapeError::RecoveryRestricted {
                reason: "同代 DP 增量重放与 IP Full 节点内容不一致".into(),
            });
        }
        Ok(())
    }
}

fn complete_payload(row: &Row<'_>) -> Result<IndexEntry> {
    let path: String = row.get(0)?;
    let kind: i64 = row.get(1)?;
    let uid: Vec<u8> = row.get(2)?;
    let body: Vec<u8> = row.get(3)?;
    let payload: Option<Vec<u8>> = row.get(4)?;
    let uid = u64::from_be_bytes(uid.try_into().map_err(|_| bad("节点 UID 长度"))?);
    let entry = decode_payload(&path, kind, uid, &body, payload.as_deref())?;
    let partial = match &entry {
        IndexEntry::File { node, .. } => node.delta_fields.is_some(),
        IndexEntry::Directory { node, .. } => node.delta_fields.is_some(),
    };
    if partial {
        return Err(bad("完整视图残留增量字段声明"));
    }
    Ok(entry)
}

fn decode_payload(
    path: &str,
    kind: i64,
    uid: u64,
    body: &[u8],
    payload: Option<&[u8]>,
) -> Result<IndexEntry> {
    let entry: IndexEntry =
        serde_json::from_slice(payload.ok_or_else(|| bad("节点 payload 缺失"))?)
            .map_err(|e| bad(&format!("节点 payload 损坏: {e}")))?;
    let check = Entry::new(entry.clone())?;
    if check.path != path || check.kind != kind || check.uid != uid || check.body != body {
        return Err(bad("节点 payload 与独立字段摘要不符"));
    }
    Ok(entry)
}

fn normalize_payload(mut entry: IndexEntry) -> Result<Vec<u8>> {
    match &mut entry {
        IndexEntry::File { node, .. } => node.delta_fields = None,
        IndexEntry::Directory { node, .. } => node.delta_fields = None,
    }
    serde_json::to_vec(&entry).map_err(|e| bad(&e.to_string()))
}

fn merge_payload(mut current: IndexEntry, delta: IndexEntry) -> Result<Vec<u8>> {
    match (&mut current, delta) {
        (IndexEntry::File { node, .. }, IndexEntry::File { node: patch, .. }) => {
            super::incremental::merge_file_metadata(node, &patch)
        }
        (IndexEntry::Directory { path, node }, IndexEntry::Directory { node: patch, .. }) => {
            super::incremental::merge_directory_metadata(node, &patch, path.is_empty())?
        }
        _ => return Err(bad("节点 payload 合并类型冲突")),
    }
    normalize_payload(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{DirectoryNode, FileNode, IndexLocation, LtfsIndex, Xattr};
    use std::collections::BTreeSet;

    fn base() -> LtfsIndex {
        let mut index = LtfsIndex::empty(uuid::Uuid::new_v4(), "replay-test".into(), 'b');
        index.self_location.start_block = 5;
        index.highest_file_uid = 10;
        let mut file = FileNode {
            name: "a".into(),
            length: 10,
            meta: NodeMeta {
                file_uid: 2,
                ..index.root.meta.clone()
            },
            extents: vec![Extent {
                partition: 'b',
                start_block: 99,
                byte_offset: 1,
                byte_count: 10,
                file_offset: 0,
            }],
            xattrs: vec![Xattr {
                key: "old".into(),
                value: "value".into(),
                base64: false,
            }],
            ..Default::default()
        };
        index.root.files.push(file.clone());
        file.name = "d0".into();
        file.meta.file_uid = 3;
        index.root.files.push(file.clone());
        file.name = "child".into();
        file.meta.file_uid = 5;
        index.root.subdirs.push(DirectoryNode {
            name: "d".into(),
            meta: NodeMeta {
                file_uid: 4,
                ..index.root.meta.clone()
            },
            files: vec![file],
            ..Default::default()
        });
        index
    }

    fn next(base: &LtfsIndex) -> LtfsIndex {
        let mut next = base.clone();
        next.generation += 1;
        next.self_location = IndexLocation {
            partition: 'b',
            start_block: base.self_location.start_block + 100,
        };
        next.highest_file_uid = 100;
        next
    }

    #[test]
    fn captured_payload_pages_seek_after_root_and_preserve_bytewise_order() {
        let base = base();
        let replay = IndexReplay::capture(&IndexScratchBudget::default(), 'a').unwrap();
        LtfsIndex::scan_full_reader(&base.to_xml().unwrap()[..], |entry| replay.stage(0, entry))
            .unwrap();
        replay.load_full(0, base.highest_file_uid).unwrap();
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut page = Vec::new();
            replay
                .for_each_payload_page(after.as_deref(), 1, |entry| {
                    page.push(match entry {
                        IndexEntry::File { path, .. } | IndexEntry::Directory { path, .. } => path,
                    });
                    Ok(())
                })
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert_eq!(page.len(), 1);
            after = page.last().cloned();
            all.extend(page);
        }
        assert_eq!(all, ["", "a", "d", "d/child", "d0"]);
        assert!(replay.payload_by_uid(0).is_err());
        assert!(replay.payload_by_uid(u64::MAX).unwrap().is_none());
        assert!(
            matches!(replay.payload_by_uid(1).unwrap(), Some(IndexEntry::Directory {path,..}) if path.is_empty())
        );
        assert!(
            matches!(replay.payload_by_uid(2).unwrap(), Some(IndexEntry::File {path,..}) if path=="a")
        );
        let plan: String = replay
            .db
            .query_row(
                "EXPLAIN QUERY PLAN SELECT path FROM dp WHERE uid=?1 LIMIT 2",
                [2u64.to_be_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("dp_uid"), "UID 查询必须使用已有索引: {plan}");
        for limit in [0, 33] {
            assert!(
                replay
                    .for_each_payload_page(None, limit, |_| Ok(()))
                    .is_err()
            );
        }
        replay
            .db
            .execute("UPDATE dp SET payload=X'00' WHERE path=X'61'", [])
            .unwrap();
        assert!(
            replay
                .for_each_payload_page(Some(""), 1, |_| Ok(()))
                .is_err(),
            "分页必须同样检查捕获节点的独立摘要"
        );
        assert!(
            replay.payload_by_uid(2).is_err(),
            "UID 查询也必须拒绝损坏 payload"
        );
        replay
            .db
            .execute(
                "INSERT INTO dp SELECT X'7a',kind,uid,body,payload FROM dp WHERE path=X'6430'",
                [],
            )
            .unwrap();
        assert!(
            replay.payload_by_uid(3).is_err(),
            "不能把重复 UID 当成唯一来源"
        );
    }

    fn check(base: &LtfsIndex, delta: &LtfsIndex) {
        for payloads in [false, true] {
            let replay = if payloads {
                IndexReplay::capture(&IndexScratchBudget::default(), 'a')
            } else {
                IndexReplay::new(&IndexScratchBudget::default(), 'a')
            }
            .unwrap();
            let base_xml = base.to_xml().unwrap();
            LtfsIndex::scan_full_reader(&base_xml[..], |entry| replay.stage(0, entry)).unwrap();
            replay.load_full(0, base.highest_file_uid).unwrap();
            let delta_xml = delta.to_xml().unwrap();
            let parsed = LtfsIndex::parse(&delta_xml).unwrap();
            LtfsIndex::scan_incremental_reader_to_disk(&delta_xml[..], |entry| {
                replay.stage(1, entry)
            })
            .unwrap();
            let expected = base.apply_incremental(&parsed);
            let actual = replay.apply_delta(1, parsed.highest_file_uid);
            if let Ok(mut expected) = expected {
                actual.unwrap();
                expected.incremental = false;
                let xml = expected.to_xml().unwrap();
                LtfsIndex::scan_full_reader(&xml[..], |entry| replay.record_ip(entry)).unwrap();
                replay.validate(true, expected.highest_file_uid).unwrap();
                replay.require_equal().unwrap();
                if payloads {
                    let mut wanted = std::collections::BTreeMap::new();
                    LtfsIndex::scan_full_reader(&xml[..], |entry| {
                        let path = match &entry {
                            IndexEntry::File { path, .. } | IndexEntry::Directory { path, .. } => {
                                path.clone()
                            }
                        };
                        wanted.insert(path, entry);
                        Ok(())
                    })
                    .unwrap();
                    let mut actual = std::collections::BTreeMap::new();
                    replay
                        .for_each_payload(|entry| {
                            let path = match &entry {
                                IndexEntry::File { path, .. }
                                | IndexEntry::Directory { path, .. } => path.clone(),
                            };
                            actual.insert(path, entry);
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(actual, wanted, "完整 payload 必须与独立的内存重放模型一致");
                }
            } else {
                assert!(actual.is_err(), "磁盘重放不能接受内存重放拒绝的更新");
            }
        }
    }

    #[test]
    fn captured_payload_corruption_is_rejected_before_nodes_are_released() {
        let base = base();
        let xml = base.to_xml().unwrap();
        let replay = IndexReplay::capture(&IndexScratchBudget::default(), 'a').unwrap();
        LtfsIndex::scan_full_reader(&xml[..], |entry| replay.stage(0, entry)).unwrap();
        replay.load_full(0, base.highest_file_uid).unwrap();
        replay.for_each_payload(|_| Ok(())).unwrap();
        replay.db.execute(r#"UPDATE dp SET payload=CAST(replace(CAST(payload AS TEXT),'"value":"value"','"value":"damaged"') AS BLOB) WHERE path=X'61'"#,[]).unwrap();
        let error = replay.for_each_payload(|_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("摘要"), "{error}");
    }

    #[test]
    fn small_disk_budget_fails_closed_and_removes_private_database() {
        let directory =
            std::env::temp_dir().join(format!("tape-proof-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let budget = IndexScratchBudget {
            directory: directory.clone(),
            max_bytes: 64 << 10,
            headroom_bytes: 0,
        };
        let comparison = IndexReplay::new(&budget, 'a').unwrap();
        let mut denied = false;
        for id in 2..10_002 {
            if comparison
                .stage(
                    0,
                    IndexEntry::File {
                        path: format!("file-{id}"),
                        node: FileNode {
                            name: format!("file-{id}"),
                            meta: NodeMeta {
                                file_uid: id,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    },
                )
                .is_err()
            {
                denied = true;
                break;
            }
        }
        assert!(denied, "a partial proof cannot exceed the admitted budget");
        drop(comparison);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn replay_matches_tree_replay_for_rename_delete_replacement_and_symlinks() {
        let base = base();
        for case in 0..7 {
            let mut next = next(&base);
            match case {
                0 => next.root.files[0].name = "renamed".into(),
                1 => {
                    next.root.subdirs.clear();
                }
                2 => {
                    next.root.subdirs[0].meta.file_uid = 11;
                    next.root.subdirs[0].files[0].meta.file_uid = 12;
                    next.root.subdirs[0].files[0].name = "new".into();
                }
                3 => {
                    let mut file = next.root.files[0].clone();
                    file.name = "d".into();
                    file.meta.file_uid = 11;
                    next.root.subdirs.clear();
                    next.root.files.push(file);
                }
                4 => {
                    next.root.files.remove(0);
                    let mut directory = next.root.subdirs[0].clone();
                    directory.name = "a".into();
                    directory.meta.file_uid = 11;
                    directory.files[0].meta.file_uid = 12;
                    next.root.subdirs.push(directory);
                }
                5 => {
                    next.root.files[0].extents.clear();
                    next.root.files[0].symlink = Some("d/child".into());
                }
                6 => {
                    next.root.subdirs[0].name = "moved".into();
                    next.root.subdirs[0].meta.readonly = true;
                }
                _ => unreachable!(),
            }
            check(&base, &next.incremental_since(&base).unwrap());
        }
    }

    #[test]
    fn partial_field_updates_match_tree_replay_and_preserve_omitted_values() {
        let base = base();
        let names = [
            "readonly",
            "creationtime",
            "changetime",
            "modifytime",
            "accesstime",
            "backuptime",
            "length",
            "openforwrite",
            "extendedattributes",
        ];
        for mask in 0..512 {
            let mut delta = next(&base).incremental_since(&base).unwrap();
            let mut file = base.root.files[0].clone();
            file.delta_fields = Some(
                ["name", "fileuid"]
                    .into_iter()
                    .map(String::from)
                    .chain(
                        names
                            .iter()
                            .enumerate()
                            .filter(|(id, _)| mask & (1 << id) != 0)
                            .map(|(_, name)| (*name).into()),
                    )
                    .collect::<BTreeSet<_>>(),
            );
            file.meta.readonly = true;
            file.meta.creation_time = "changed creation".into();
            file.meta.change_time = "changed change".into();
            file.meta.modify_time = "changed modify".into();
            file.meta.access_time = "changed access".into();
            file.meta.backup_time = "changed backup".into();
            file.length = 20;
            file.open_for_write = Some(false);
            file.xattrs.clear();
            delta.root.files.push(file);
            check(&base, &delta);
        }
    }

    #[test]
    fn duplicate_uid_partial_creation_and_duplicate_delta_paths_fail_closed() {
        let base = base();
        let mut next = next(&base);
        next.root.files[0].meta.file_uid = 3;
        check(&base, &next.incremental_since(&base).unwrap());
        let mut delta = next.incremental_since(&base).unwrap();
        delta.root.files[0].name = "new-partial".into();
        delta.root.files[0].delta_fields =
            Some(["name", "fileuid"].into_iter().map(String::from).collect());
        check(&base, &delta);
        let replay = IndexReplay::new(&IndexScratchBudget::default(), 'a').unwrap();
        let mut delta = next.incremental_since(&base).unwrap();
        delta.root.files.push(delta.root.files[0].clone());
        let xml = delta.to_xml().unwrap();
        assert!(
            LtfsIndex::scan_incremental_reader_to_disk(&xml[..], |entry| replay.stage(1, entry))
                .is_err()
        );
    }

    #[test]
    #[ignore = "显式百万基线和百万增量节点落盘重放压测，不属于默认测试"]
    fn million_entry_delta_replay_spills_to_disk() {
        fn file(uid: u64, partial: bool) -> IndexEntry {
            IndexEntry::File {
                path: format!("file-{uid}"),
                node: FileNode {
                    name: format!("file-{uid}"),
                    meta: NodeMeta {
                        file_uid: uid,
                        readonly: partial,
                        ..Default::default()
                    },
                    delta_fields: partial.then(|| {
                        ["name", "fileuid", "readonly"]
                            .into_iter()
                            .map(String::from)
                            .collect()
                    }),
                    ..Default::default()
                },
            }
        }
        fn root(partial: bool) -> IndexEntry {
            IndexEntry::Directory {
                path: String::new(),
                node: DirectoryNode {
                    meta: NodeMeta {
                        file_uid: if partial { 0 } else { 1 },
                        ..Default::default()
                    },
                    delta_fields: partial
                        .then(|| ["name", "contents"].into_iter().map(String::from).collect()),
                    ..Default::default()
                },
            }
        }
        let replay = IndexReplay::new(&IndexScratchBudget::default(), 'a').unwrap();
        replay.stage(0, root(false)).unwrap();
        for uid in 2..1_000_002 {
            replay.stage(0, file(uid, false)).unwrap();
        }
        replay.load_full(0, 1_000_001).unwrap();
        replay.stage(1, root(true)).unwrap();
        for uid in 2..1_000_002 {
            replay.stage(1, file(uid, true)).unwrap();
        }
        replay.apply_delta(1, 1_000_001).unwrap();
        replay.record_ip(root(false)).unwrap();
        for uid in 2..1_000_002 {
            let IndexEntry::File { path, mut node } = file(uid, true) else {
                unreachable!()
            };
            node.delta_fields = None;
            replay.record_ip(IndexEntry::File { path, node }).unwrap();
        }
        replay.validate(true, 1_000_001).unwrap();
        replay.require_equal().unwrap();
        let (pages, size): (u64, u64) = replay.db.query_row(
            "SELECT (SELECT page_count FROM pragma_page_count), (SELECT page_size FROM pragma_page_size)",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        println!(
            "1000000 baseline + 1000000 partial delta entries + 1000000 IP entries; {} temporary bytes",
            pages * size
        );
    }
}
