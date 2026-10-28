//! Provisional disk-backed replay of streamed index fields. Only per-field
//! SHA-256 values are retained: no complete nodes, XML or directory trees.

use std::fmt::Debug;

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
        })
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

pub(super) struct IndexReplay {
    db: ScratchDatabase,
}

impl IndexReplay {
    pub(super) fn new(budget: &IndexScratchBudget) -> Result<Self> {
        let db = ScratchDatabase::new(budget)?;
        db.execute_batch(
            "CREATE TABLE raw(construct INTEGER NOT NULL, path BLOB NOT NULL,
                 kind INTEGER NOT NULL, uid BLOB NOT NULL, fields INTEGER NOT NULL,
                 deleted INTEGER NOT NULL, symlink INTEGER NOT NULL, body BLOB NOT NULL,
                 PRIMARY KEY(construct,path)) WITHOUT ROWID;
             CREATE TABLE dp(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                 uid BLOB NOT NULL, body BLOB NOT NULL) WITHOUT ROWID;
             CREATE INDEX dp_uid ON dp(uid);
             CREATE TABLE ip(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                 uid BLOB NOT NULL, body BLOB NOT NULL) WITHOUT ROWID;
             CREATE INDEX ip_uid ON ip(uid);
             BEGIN;",
        )?;
        Ok(Self { db })
    }

    pub(super) fn stage(&self, construct: u32, entry: IndexEntry) -> Result<()> {
        let entry = Entry::new(entry)?;
        self.db.ensure_room(entry.path.len() + BODY_LEN + 64)?;
        self.db
            .prepare_cached("INSERT INTO raw VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?
            .execute(params![
                construct,
                entry.path.as_bytes(),
                entry.kind,
                entry.uid.to_be_bytes().as_slice(),
                entry.fields,
                entry.deleted,
                entry.symlink,
                entry.body
            ])
            .map_err(|error| {
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) {
                    TapeError::LocalCapacity {
                        reason: format!("索引磁盘重放路径重复或暂存失败: {error}"),
                    }
                } else {
                    TapeError::Ltfs(format!("索引磁盘重放路径重复或暂存失败: {error}"))
                }
            })?;
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
            ])?;
        Ok(())
    }

    pub(super) fn load_full(&self, construct: u32, highest: u64) -> Result<()> {
        // Copy a row at a time so a large baseline extends its physical
        // reservation incrementally rather than bypassing the page budget.
        let mut rows = self
            .db
            .prepare("SELECT path,kind,uid,body FROM raw WHERE construct=?1")?;
        let mut cursor = rows.query([construct])?;
        while let Some(row) = cursor.next()? {
            let path: Vec<u8> = row.get(0)?;
            let kind: i64 = row.get(1)?;
            let uid: Vec<u8> = row.get(2)?;
            let body: Vec<u8> = row.get(3)?;
            self.db.ensure_room(path.len() + body.len() + 64)?;
            self.db
                .prepare_cached("INSERT INTO dp VALUES(?1,?2,?3,?4)")?
                .execute(params![path, kind, uid, body])?;
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

    fn current(&self, path: &str) -> Result<Option<(i64, u64, Vec<u8>)>> {
        self.db
            .prepare_cached("SELECT kind,uid,body FROM dp WHERE path=?1")?
            .query_row([path.as_bytes()], |row| {
                let uid: Vec<u8> = row.get(1)?;
                Ok((
                    row.get(0)?,
                    u64::from_be_bytes(uid.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?),
                    row.get(2)?,
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
            "SELECT CAST(path AS TEXT),kind,uid,fields,deleted,symlink,body FROM raw
             WHERE construct=?1 AND deleted=?2 ORDER BY path",
        )?;
        // Deletions precede additions, allowing a UID to move between paths.
        for row in statement.query_map(params![construct, true], Entry::row)? {
            let patch = row?;
            if let Some((kind, _, _)) = self.current(&patch.path)?
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
                if !self.current(parent)?.is_some_and(|(kind, _, _)| kind == 0) {
                    return Err(bad("增量对象缺少目录父节点"));
                }
            }
            let prior = self.current(&patch.path)?;
            let matches = prior.as_ref().is_some_and(|(kind, uid, _)| {
                *kind == patch.kind && (*uid == patch.uid || (patch.kind == 0 && !patch.has(UID)))
            });
            if patch.path.is_empty() && !matches {
                return Err(bad("根目录 UID 或类型变化"));
            }
            if matches {
                let (_, uid, mut body) = prior.unwrap();
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
                self.remove_tree(&patch.path)?;
            }
            self.db.ensure_room(patch.path.len() + BODY_LEN + 64)?;
            self.db.prepare_cached("INSERT INTO dp VALUES(?1,?2,?3,?4)
                ON CONFLICT(path) DO UPDATE SET kind=excluded.kind,uid=excluded.uid,body=excluded.body")?
                .execute(params![patch.path.as_bytes(), patch.kind, patch.uid.to_be_bytes().as_slice(), patch.body])?;
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

    fn check(base: &LtfsIndex, delta: &LtfsIndex) {
        let replay = IndexReplay::new(&IndexScratchBudget::default()).unwrap();
        let base_xml = base.to_xml().unwrap();
        LtfsIndex::scan_full_reader(&base_xml[..], |entry| replay.stage(0, entry)).unwrap();
        replay.load_full(0, base.highest_file_uid).unwrap();
        let delta_xml = delta.to_xml().unwrap();
        let parsed = LtfsIndex::parse(&delta_xml).unwrap();
        LtfsIndex::scan_incremental_reader_to_disk(&delta_xml[..], |entry| replay.stage(1, entry))
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
        } else {
            assert!(actual.is_err(), "磁盘重放不能接受内存重放拒绝的更新");
        }
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
        let comparison = IndexReplay::new(&budget).unwrap();
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
        let replay = IndexReplay::new(&IndexScratchBudget::default()).unwrap();
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
        let replay = IndexReplay::new(&IndexScratchBudget::default()).unwrap();
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
