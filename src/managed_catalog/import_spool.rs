//! Immutable, node-local source inventory for a confirmed complete LTFS index.
//! FDB imports may resume from a path cursor, but must compare the complete
//! source digest with the imported namespace before making it visible.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
use crate::error::{Result, TapeError};
use crate::ltfs::index::{IndexEntry, LtfsIndex};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::{IndexCandidate, RecoveryBudget, confirmed_index_reader};
use crate::ltfs::volume::{TOMBSTONE_DIR, XATTR_DELETED_PATH, XATTR_VERSION, tombstone_path};
use crate::tape::commands::TapeDrive;

use super::{ContentDigest, NodeAttributes, NodeKind};

const MAX_PAGE: usize = 1000;
const DOMAIN: &[u8] = b"tape-rs/ltfs-import-spool/1";
const CANDIDATE_DOMAIN: &[u8] = b"tape-rs/ltfs-import-candidates/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportCandidateStats {
    pub entries: u64,
    pub rows_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCandidate {
    pub logical_path: String,
    pub physical_path: String,
    pub kind: NodeKind,
    pub deleted: bool,
    pub version: (u64, u64),
    pub length: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct ImportCandidatePage {
    pub candidates: Vec<ImportCandidate>,
    pub next: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportSpoolStats {
    pub files: u64,
    pub directories: u64,
    pub max_attribute_bytes: u64,
    pub rows_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportNode {
    pub path: String,
    pub kind: NodeKind,
    pub length: Option<u64>,
    pub attributes: NodeAttributes,
    pub content_digest: Option<ContentDigest>,
}

/// The media file that actually owns the bytes for a logical LTFS file.
/// IBM EE user paths can be symlinks whose own fileuid and length describe
/// only the alias, not the data object in `.LTFSEE_DATA`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportDataSource {
    pub logical_file_uid: u64,
    pub data_file_uid: u64,
    pub data_path: String,
    pub length: u64,
    pub content_digest: ContentDigest,
}

#[derive(Debug)]
pub struct ImportPage {
    pub nodes: Vec<ImportNode>,
    pub next: Option<String>,
}

pub struct ImportSpool {
    conn: Connection,
    stats: ImportSpoolStats,
    candidates: ImportCandidateStats,
    source: ExpectedIndex,
}

struct StoredManifest {
    volume: String,
    generation: String,
    partition: String,
    block: String,
    source: Vec<u8>,
    files: String,
    directories: String,
    max_attributes: String,
    rows: Vec<u8>,
    candidate_count: String,
    candidate_rows: Vec<u8>,
}

impl ImportSpool {
    /// 从恢复协议确认的位置只读重读索引。此操作会 LOCATE 并改变驱动器读位置；
    /// 调用方须持有当前 owner/PR 和独占设备执行资格，不得与写会话并发。
    pub fn build_confirmed(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        candidate: &IndexCandidate,
        budget: &RecoveryBudget,
        path: &Path,
    ) -> Result<ImportSpoolStats> {
        let expected = ExpectedIndex {
            volume_uuid: candidate.index.volume_uuid,
            generation: candidate.index.generation,
            location: candidate.index.self_location,
            sha256: candidate.construct_sha256,
        };
        let mut reader = confirmed_index_reader(drive, label, candidate, budget)?;
        match Self::build(&mut reader, path, expected) {
            Ok(stats) => Ok(stats),
            Err(error) => Err(reader.take_device_error().unwrap_or(error)),
        }
    }

    /// Parse the entire XML into a private SQLite file. The destination appears
    /// only after the original bytes, LTFS identity, and all rows are verified.
    /// Rebuilding never replaces an existing source inventory.
    pub fn build<R: Read>(
        input: R,
        path: &Path,
        expected: ExpectedIndex,
    ) -> Result<ImportSpoolStats> {
        if path.exists() {
            return Err(TapeError::Catalog(format!(
                "LTFS 导入清单已存在: {}",
                path.display()
            )));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let name = path
            .file_name()
            .ok_or_else(|| TapeError::Catalog("LTFS 导入清单缺少文件名".into()))?;
        let temp = parent.join(format!(
            ".{}.{}.building",
            name.to_string_lossy(),
            Uuid::new_v4()
        ));
        let cleanup = RemoveOnDrop(temp.clone());
        let conn = Connection::open(&temp)?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA user_version=3;
             CREATE TABLE manifest (
               id INTEGER PRIMARY KEY CHECK(id=1),
               volume_uuid TEXT NOT NULL, generation TEXT NOT NULL,
               partition TEXT NOT NULL, start_block TEXT NOT NULL,
               source_sha256 BLOB NOT NULL, files TEXT NOT NULL,
               directories TEXT NOT NULL, max_attribute_bytes TEXT NOT NULL,
               rows_sha256 BLOB NOT NULL,
               candidate_count TEXT NOT NULL, candidate_sha256 BLOB NOT NULL
             );
             CREATE TABLE nodes (
               path TEXT PRIMARY KEY, uid TEXT NOT NULL UNIQUE,
               kind INTEGER NOT NULL, length TEXT,
               attributes BLOB NOT NULL
             );
             CREATE TABLE catalog_candidates (
               logical_path TEXT PRIMARY KEY, physical_path TEXT NOT NULL,
               kind INTEGER NOT NULL, deleted INTEGER NOT NULL,
               ver_round TEXT NOT NULL, ver_seq TEXT NOT NULL,
               length TEXT NOT NULL, sha256 TEXT NOT NULL
             );
             BEGIN IMMEDIATE;",
        )?;
        let mut stats = ImportSpoolStats {
            files: 0,
            directories: 0,
            max_attribute_bytes: 0,
            rows_sha256: [0; 32],
        };
        let mut hashed = HashingRead {
            inner: input,
            sha: Sha256::new(),
        };
        let mut ee_alias_seen = false;
        let mut ee_internal_seen = false;
        let mut ee_data_dir_seen = false;
        let index = LtfsIndex::scan_full_reader(BufReader::new(&mut hashed), |entry| {
            let (path, kind, length, attributes) = match entry {
                IndexEntry::File { path, node } => {
                    ContentDigest::from_ltfs_xattrs(&node.xattrs)
                        .map_err(|e| TapeError::Catalog(e.to_string()))?;
                    if path.rsplit('/').next() != Some(node.name.as_str()) {
                        return Err(TapeError::Catalog("LTFS 文件名称与路径不一致".into()));
                    }
                    let attributes = NodeAttributes::new(node.meta, node.xattrs, node.symlink)
                        .map_err(|e| TapeError::Catalog(e.to_string()))?;
                    (path, NodeKind::File, Some(node.length), attributes)
                }
                IndexEntry::Directory { path, node } => {
                    if path.is_empty() {
                        if !node.name.is_empty() {
                            return Err(TapeError::Catalog("LTFS 根目录名称无效".into()));
                        }
                        return Ok(());
                    }
                    if path.rsplit('/').next() != Some(node.name.as_str()) {
                        return Err(TapeError::Catalog("LTFS 目录名称与路径不一致".into()));
                    }
                    let attributes = NodeAttributes::new(node.meta, node.xattrs, None)
                        .map_err(|e| TapeError::Catalog(e.to_string()))?;
                    (path, NodeKind::Directory, None, attributes)
                }
            };
            validate_path(&path)?;
            if let Some(target) = attributes.symlink()
                && target.contains(".LTFSEE_DATA/")
            {
                resolve_ee_alias_target(&path, target)?;
                ee_alias_seen = true;
            }
            insert_node(&conn, &path, kind, length, &attributes)?;
            let ee_internal = path == ".LTFSEE_DATA" || path.starts_with(".LTFSEE_DATA/");
            ee_internal_seen |= ee_internal;
            ee_data_dir_seen |= path == ".LTFSEE_DATA" && kind == NodeKind::Directory;
            if !ee_internal
                && let Some(candidate) = import_candidate(&path, kind, length, &attributes)?
            {
                insert_candidate(&conn, &candidate)?;
            }
            match kind {
                NodeKind::File => {
                    stats.files = stats
                        .files
                        .checked_add(1)
                        .ok_or_else(|| TapeError::Catalog("文件数溢出".into()))?
                }
                NodeKind::Directory => {
                    stats.directories = stats
                        .directories
                        .checked_add(1)
                        .ok_or_else(|| TapeError::Catalog("目录数溢出".into()))?
                }
            }
            stats.max_attribute_bytes = stats
                .max_attribute_bytes
                .max(attributes.bytes().unwrap().len() as u64);
            if (stats.files + stats.directories).is_multiple_of(1000) {
                conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            }
            Ok(())
        })?;
        let source_sha256: [u8; 32] = hashed.sha.finalize().into();
        if index.volume_uuid != expected.volume_uuid
            || index.generation != expected.generation
            || index.self_location != expected.location
            || source_sha256 != expected.sha256
        {
            return Err(TapeError::Catalog(
                "LTFS 导入清单的索引身份或原文摘要不符".into(),
            ));
        }
        let root = NodeAttributes::new(index.root.meta, index.root.xattrs, None)
            .map_err(|e| TapeError::Catalog(e.to_string()))?;
        insert_node(&conn, "", NodeKind::Directory, None, &root)?;
        stats.directories = stats
            .directories
            .checked_add(1)
            .ok_or_else(|| TapeError::Catalog("目录数溢出".into()))?;
        stats.max_attribute_bytes = stats
            .max_attribute_bytes
            .max(root.bytes().unwrap().len() as u64);
        drop(root);
        if ee_internal_seen != ee_alias_seen || ee_alias_seen != ee_data_dir_seen {
            return Err(TapeError::Catalog(
                "无法确认 .LTFSEE_DATA 为 EE 内部数据目录".into(),
            ));
        }
        let checked = digest_rows(&conn)?;
        if (stats.files, stats.directories, stats.max_attribute_bytes)
            != (
                checked.files,
                checked.directories,
                checked.max_attribute_bytes,
            )
        {
            return Err(TapeError::Catalog("LTFS 导入节点统计不一致".into()));
        }
        stats.rows_sha256 = checked.rows_sha256;
        let candidates = digest_candidates(&conn)?;
        conn.execute(
            "INSERT INTO manifest(id,volume_uuid,generation,partition,start_block,source_sha256,files,directories,max_attribute_bytes,rows_sha256,candidate_count,candidate_sha256)
             VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                expected.volume_uuid.to_string(), expected.generation.to_string(),
                expected.location.partition.to_string(), expected.location.start_block.to_string(),
                &expected.sha256[..], stats.files.to_string(), stats.directories.to_string(),
                stats.max_attribute_bytes.to_string(), &stats.rows_sha256[..],
                candidates.entries.to_string(), &candidates.rows_sha256[..],
            ],
        )?;
        conn.execute_batch("COMMIT;")?;
        drop(conn);
        File::open(&temp)?.sync_all()?;
        fs::hard_link(&temp, path)?;
        File::open(parent)?.sync_all()?;
        drop(cleanup);
        Ok(stats)
    }

    pub fn open(path: &Path, expected: ExpectedIndex) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 3 {
            return Err(TapeError::Catalog(format!(
                "LTFS 导入清单版本 {version} 不受支持"
            )));
        }
        let row: Option<StoredManifest> = conn.query_row(
            "SELECT volume_uuid,generation,partition,start_block,source_sha256,files,directories,max_attribute_bytes,rows_sha256,candidate_count,candidate_sha256 FROM manifest WHERE id=1",
            [],
            |r| Ok(StoredManifest {
                volume: r.get(0)?, generation: r.get(1)?, partition: r.get(2)?,
                block: r.get(3)?, source: r.get(4)?, files: r.get(5)?,
                directories: r.get(6)?, max_attributes: r.get(7)?, rows: r.get(8)?,
                candidate_count: r.get(9)?, candidate_rows: r.get(10)?,
            }),
        ).optional()?;
        let Some(StoredManifest {
            volume,
            generation,
            partition,
            block,
            source,
            files,
            directories,
            max_attributes,
            rows,
            candidate_count,
            candidate_rows,
        }) = row
        else {
            return Err(TapeError::Catalog("LTFS 导入清单未完成".into()));
        };
        if volume != expected.volume_uuid.to_string()
            || generation != expected.generation.to_string()
            || partition != expected.location.partition.to_string()
            || block != expected.location.start_block.to_string()
            || source.as_slice() != expected.sha256
        {
            return Err(TapeError::Catalog("LTFS 导入清单来源不匹配".into()));
        }
        let rows_sha256: [u8; 32] = rows
            .try_into()
            .map_err(|_| TapeError::Catalog("LTFS 导入清单摘要长度损坏".into()))?;
        let candidate_sha256: [u8; 32] = candidate_rows
            .try_into()
            .map_err(|_| TapeError::Catalog("LTFS 导入候选摘要长度损坏".into()))?;
        let spool = Self {
            conn,
            stats: ImportSpoolStats {
                files: files
                    .parse()
                    .map_err(|_| TapeError::Catalog("LTFS 导入文件数损坏".into()))?,
                directories: directories
                    .parse()
                    .map_err(|_| TapeError::Catalog("LTFS 导入目录数损坏".into()))?,
                max_attribute_bytes: max_attributes
                    .parse()
                    .map_err(|_| TapeError::Catalog("LTFS 导入属性长度损坏".into()))?,
                rows_sha256,
            },
            candidates: ImportCandidateStats {
                entries: candidate_count
                    .parse()
                    .map_err(|_| TapeError::Catalog("LTFS 导入候选数量损坏".into()))?,
                rows_sha256: candidate_sha256,
            },
            source: expected,
        };
        spool.verify_all()?;
        Ok(spool)
    }

    pub fn stats(&self) -> ImportSpoolStats {
        self.stats
    }

    pub fn candidate_stats(&self) -> ImportCandidateStats {
        self.candidates
    }

    pub fn source(&self) -> ExpectedIndex {
        self.source
    }

    /// Prove that both independently built local projections describe exactly
    /// the same files before their index identity is recorded in FDB. The
    /// inventory contains directories too; the extent cache intentionally does not.
    pub fn verify_extent_cache(&self, cache: &VolumeExtentCache) -> Result<()> {
        // All three cursors use the existing SQLite TEXT UID indexes. Keep one
        // node/file at a time, and retain each cached file's extent proof check.
        let mut stmt = self
            .conn
            .prepare("SELECT path,uid,kind,length,attributes FROM nodes ORDER BY uid")?;
        let mut rows = stmt.query([])?;
        let count = cache.visit_files_by_uid(|uid, cached| {
            let node = loop {
                let row = rows
                    .next()?
                    .ok_or_else(|| TapeError::Catalog("LTFS 导入清单缺少缓存文件".into()))?;
                let node = decode_row(row)?;
                if node.kind == NodeKind::File {
                    break node;
                }
            };
            if node.attributes.meta().unwrap().file_uid != uid
                || cached.path != node.path
                || Some(cached.length) != node.length
                || cached.symlink.as_deref() != node.attributes.symlink()
            {
                return Err(TapeError::Catalog(format!(
                    "LTFS 导入清单与 extent 缓存 UID {uid} 不一致"
                )));
            }
            Ok(())
        })?;
        while let Some(row) = rows.next()? {
            if decode_row(row)?.kind == NodeKind::File {
                return Err(TapeError::Catalog("卷 extent 缓存缺少清单文件".into()));
            }
        }
        if count != self.stats.files {
            return Err(TapeError::Catalog(
                "LTFS 导入清单与 extent 缓存文件数不一致".into(),
            ));
        }
        Ok(())
    }

    /// Re-read all rows before treating this local file as an independent source.
    pub fn verify_all(&self) -> Result<()> {
        if digest_rows(&self.conn)? != self.stats {
            return Err(TapeError::Catalog("LTFS 导入清单内容摘要不符".into()));
        }
        if digest_candidates(&self.conn)? != self.candidates {
            return Err(TapeError::Catalog("LTFS 导入候选摘要不符".into()));
        }
        Ok(())
    }

    pub fn page(&self, after: Option<&str>, limit: usize) -> Result<ImportPage> {
        if limit == 0 || limit > MAX_PAGE {
            return Err(TapeError::Catalog(
                "LTFS 导入清单页大小必须为 1..=1000".into(),
            ));
        }
        let sql = if after.is_some() {
            "SELECT path,uid,kind,length,attributes FROM nodes WHERE path>?1 ORDER BY path LIMIT ?2"
        } else {
            "SELECT path,uid,kind,length,attributes FROM nodes WHERE path>=?1 ORDER BY path LIMIT ?2"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![after.unwrap_or(""), (limit + 1) as i64])?;
        let mut nodes = Vec::with_capacity(limit + 1);
        let mut attribute_bytes = 0usize;
        let mut byte_limited = false;
        while let Some(row) = rows.next()? {
            let bytes = row
                .get_ref(4)?
                .as_blob()
                .map_err(|error| TapeError::Catalog(format!("LTFS 导入属性类型: {error}")))?
                .len();
            // A single large node remains readable; do not gather many such
            // nodes just because the caller requested 1,000 rows.
            if !nodes.is_empty() && attribute_bytes.saturating_add(bytes) > 8 * 1024 * 1024 {
                byte_limited = true;
                break;
            }
            attribute_bytes = attribute_bytes.saturating_add(bytes);
            nodes.push(decode_row(row)?);
        }
        let next = if nodes.len() > limit {
            nodes.pop();
            nodes.last().map(|node| node.path.clone())
        } else if byte_limited {
            nodes.last().map(|node| node.path.clone())
        } else {
            None
        };
        Ok(ImportPage { nodes, next })
    }

    /// Compact logical candidates ordered by user path. Physical tombstone
    /// paths are remapped here, so multiple volumes can be merged by path
    /// without loading their complete indexes or large attributes into RAM.
    pub fn candidate_page(&self, after: Option<&str>, limit: usize) -> Result<ImportCandidatePage> {
        if limit == 0 || limit > MAX_PAGE {
            return Err(TapeError::Catalog("LTFS 候选页大小必须为 1..=1000".into()));
        }
        let sql = if after.is_some() {
            "SELECT logical_path,physical_path,kind,deleted,ver_round,ver_seq,length,sha256
             FROM catalog_candidates WHERE logical_path>?1 ORDER BY logical_path LIMIT ?2"
        } else {
            "SELECT logical_path,physical_path,kind,deleted,ver_round,ver_seq,length,sha256
             FROM catalog_candidates WHERE logical_path>=?1 ORDER BY logical_path LIMIT ?2"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![after.unwrap_or(""), (limit + 1) as i64])?;
        let mut candidates = Vec::with_capacity(limit + 1);
        while let Some(row) = rows.next()? {
            candidates.push(decode_candidate(row)?);
        }
        let next = if candidates.len() > limit {
            candidates.pop();
            candidates
                .last()
                .map(|candidate| candidate.logical_path.clone())
        } else {
            None
        };
        Ok(ImportCandidatePage { candidates, next })
    }

    pub fn node_by_path(&self, path: &str) -> Result<Option<ImportNode>> {
        if !path.is_empty() {
            validate_path(path)?;
        }
        let mut stmt = self
            .conn
            .prepare("SELECT path,uid,kind,length,attributes FROM nodes WHERE path=?1")?;
        let mut rows = stmt.query([path])?;
        rows.next()?.map(decode_row).transpose()
    }

    /// Resolve a single EE alias within this independently verified index.
    /// The returned UID is suitable for locating extents; the alias UID is not.
    /// Chained links, missing objects and paths escaping the volume are errors.
    pub fn resolve_file_data(&self, physical_path: &str) -> Result<Option<ImportDataSource>> {
        let Some(logical) = self.node_by_path(physical_path)? else {
            return Ok(None);
        };
        if logical.kind != NodeKind::File {
            return Err(TapeError::Catalog("LTFS 数据来源不是文件".into()));
        }
        let data = if let Some(target) = logical.attributes.symlink() {
            let path = resolve_ee_alias_target(&logical.path, target)?;
            let data = self
                .node_by_path(&path)?
                .ok_or_else(|| TapeError::Catalog("EE 别名目标文件缺失".into()))?;
            if data.kind != NodeKind::File || data.attributes.symlink().is_some() {
                return Err(TapeError::Catalog("EE 别名目标不是直接数据文件".into()));
            }
            data
        } else {
            logical.clone()
        };
        let logical_file_uid = logical
            .attributes
            .meta()
            .ok_or_else(|| TapeError::Catalog("LTFS 别名缺少 fileuid".into()))?
            .file_uid;
        let data_file_uid = data
            .attributes
            .meta()
            .ok_or_else(|| TapeError::Catalog("LTFS 数据文件缺少 fileuid".into()))?
            .file_uid;
        let length = data
            .length
            .ok_or_else(|| TapeError::Catalog("LTFS 数据文件缺少长度".into()))?;
        let content_digest = ContentDigest::from_ltfs_xattrs(
            data.attributes
                .xattrs()
                .ok_or_else(|| TapeError::Catalog("LTFS 数据文件属性未读取".into()))?,
        )
        .map_err(|error| TapeError::Catalog(error.to_string()))?;
        Ok(Some(ImportDataSource {
            logical_file_uid,
            data_file_uid,
            data_path: data.path,
            length,
            content_digest,
        }))
    }
}

pub(super) fn resolve_ee_alias_target(path: &str, target: &str) -> Result<String> {
    if target.starts_with('/') || target.is_empty() {
        return Err(TapeError::Catalog("EE 别名目标必须是卷内相对路径".into()));
    }
    let mut components = path.split('/').collect::<Vec<_>>();
    components.pop();
    for component in target.split('/') {
        match component {
            "" => return Err(TapeError::Catalog("EE 别名目标存在空路径分量".into())),
            "." => {}
            ".." => {
                components
                    .pop()
                    .ok_or_else(|| TapeError::Catalog("EE 别名目标越过卷根目录".into()))?;
            }
            name => components.push(name),
        }
    }
    let resolved = components.join("/");
    if !resolved.starts_with(".LTFSEE_DATA/") {
        return Err(TapeError::Catalog("EE 别名目标不在数据目录".into()));
    }
    validate_path(&resolved)?;
    Ok(resolved)
}

fn single_xattr<'a>(xattrs: &'a [crate::ltfs::index::Xattr], key: &str) -> Result<Option<&'a str>> {
    let mut values = xattrs.iter().filter(|xattr| xattr.key == key);
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if first.base64 || values.next().is_some() {
        return Err(TapeError::Catalog(format!(
            "LTFS 语义属性重复或编码无效: {key}"
        )));
    }
    Ok(Some(first.value.as_str()))
}

pub(super) fn import_candidate(
    path: &str,
    kind: NodeKind,
    length: Option<u64>,
    attrs: &NodeAttributes,
) -> Result<Option<ImportCandidate>> {
    if path.is_empty() {
        return Ok(None);
    }
    let xattrs = attrs
        .xattrs()
        .ok_or_else(|| TapeError::Catalog("LTFS 导入属性未读取".into()))?;
    let version = match single_xattr(xattrs, XATTR_VERSION)? {
        Some(raw) => {
            let (round, seq) = raw
                .split_once('.')
                .ok_or_else(|| TapeError::Catalog("LTFS 版本属性格式无效".into()))?;
            (
                round
                    .parse()
                    .map_err(|_| TapeError::Catalog("LTFS 版本轮次无效".into()))?,
                seq.parse()
                    .map_err(|_| TapeError::Catalog("LTFS 版本序号无效".into()))?,
            )
        }
        None => (0, 0),
    };
    if path
        .strip_prefix(TOMBSTONE_DIR)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        if kind != NodeKind::File || length != Some(0) || attrs.symlink().is_some() {
            return Err(TapeError::Catalog("LTFS 墓碑节点无效".into()));
        }
        let target = single_xattr(xattrs, XATTR_DELETED_PATH)?
            .ok_or_else(|| TapeError::Catalog("LTFS 墓碑缺少被删路径".into()))?;
        validate_logical_path(target)?;
        if tombstone_path(target) != path {
            return Err(TapeError::Catalog("LTFS 墓碑路径与被删路径不符".into()));
        }
        let kind = match single_xattr(xattrs, "tapers.deletedKind")? {
            Some("directory") => NodeKind::Directory,
            Some("file") | None => NodeKind::File,
            Some(_) => return Err(TapeError::Catalog("LTFS 墓碑类型无效".into())),
        };
        return Ok(Some(ImportCandidate {
            logical_path: target.to_string(),
            physical_path: path.to_string(),
            kind,
            deleted: true,
            version,
            length: 0,
            sha256: String::new(),
        }));
    }
    if path == ".tapers" || path.starts_with(".tapers/") {
        return Ok(None);
    }
    let logical_path = format!("/{path}");
    validate_logical_path(&logical_path)?;
    let sha256 = if kind == NodeKind::File {
        single_xattr(xattrs, crate::ltfs::index::XATTR_SHA256)?
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };
    Ok(Some(ImportCandidate {
        logical_path,
        physical_path: path.to_string(),
        kind,
        deleted: false,
        version,
        length: length.unwrap_or(0),
        sha256,
    }))
}

fn validate_logical_path(path: &str) -> Result<()> {
    let relative = path
        .strip_prefix('/')
        .ok_or_else(|| TapeError::Catalog("LTFS 逻辑路径必须以 / 开头".into()))?;
    validate_path(relative)
}

fn insert_candidate(conn: &Connection, candidate: &ImportCandidate) -> Result<()> {
    let written = conn.execute(
        "INSERT OR IGNORE INTO catalog_candidates(logical_path,physical_path,kind,deleted,ver_round,ver_seq,length,sha256)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            candidate.logical_path,
            candidate.physical_path,
            i64::from(candidate.kind == NodeKind::File),
            i64::from(candidate.deleted),
            candidate.version.0.to_string(),
            candidate.version.1.to_string(),
            candidate.length.to_string(),
            candidate.sha256,
        ],
    )?;
    if written != 0 {
        return Ok(());
    }
    let old_deleted: i64 = conn.query_row(
        "SELECT deleted FROM catalog_candidates WHERE logical_path=?1",
        [&candidate.logical_path],
        |row| row.get(0),
    )?;
    match (old_deleted, candidate.deleted) {
        (0, true) => Ok(()),
        (1, false) => {
            conn.execute(
                "UPDATE catalog_candidates SET physical_path=?2,kind=?3,deleted=0,
                 ver_round=?4,ver_seq=?5,length=?6,sha256=?7 WHERE logical_path=?1",
                params![
                    candidate.logical_path,
                    candidate.physical_path,
                    i64::from(candidate.kind == NodeKind::File),
                    candidate.version.0.to_string(),
                    candidate.version.1.to_string(),
                    candidate.length.to_string(),
                    candidate.sha256,
                ],
            )?;
            Ok(())
        }
        _ => Err(TapeError::Catalog(
            "LTFS 同一卷的逻辑路径出现重复候选".into(),
        )),
    }
}

fn decode_candidate(row: &rusqlite::Row<'_>) -> Result<ImportCandidate> {
    let logical_path: String = row.get(0)?;
    let physical_path: String = row.get(1)?;
    validate_logical_path(&logical_path)?;
    validate_path(&physical_path)?;
    let kind = match row.get::<_, i64>(2)? {
        0 => NodeKind::Directory,
        1 => NodeKind::File,
        _ => return Err(TapeError::Catalog("LTFS 候选类型损坏".into())),
    };
    let deleted = match row.get::<_, i64>(3)? {
        0 => false,
        1 => true,
        _ => return Err(TapeError::Catalog("LTFS 候选删除位损坏".into())),
    };
    let round: String = row.get(4)?;
    let seq: String = row.get(5)?;
    let length: String = row.get(6)?;
    let sha256: String = row.get(7)?;
    let candidate = ImportCandidate {
        logical_path,
        physical_path,
        kind,
        deleted,
        version: (
            round
                .parse()
                .map_err(|_| TapeError::Catalog("LTFS 候选版本轮次损坏".into()))?,
            seq.parse()
                .map_err(|_| TapeError::Catalog("LTFS 候选版本序号损坏".into()))?,
        ),
        length: length
            .parse()
            .map_err(|_| TapeError::Catalog("LTFS 候选长度损坏".into()))?,
        sha256,
    };
    if candidate.deleted {
        if candidate.length != 0
            || !candidate.sha256.is_empty()
            || tombstone_path(&candidate.logical_path) != candidate.physical_path
        {
            return Err(TapeError::Catalog("LTFS 墓碑候选损坏".into()));
        }
    } else if candidate.logical_path != format!("/{}", candidate.physical_path)
        || (candidate.kind == NodeKind::Directory
            && (candidate.length != 0 || !candidate.sha256.is_empty()))
    {
        return Err(TapeError::Catalog("LTFS 普通候选损坏".into()));
    }
    Ok(candidate)
}

fn digest_candidates(conn: &Connection) -> Result<ImportCandidateStats> {
    let mut sha = Sha256::new();
    sha.update(CANDIDATE_DOMAIN);
    let mut entries = 0u64;
    let mut stmt = conn.prepare(
        "SELECT logical_path,physical_path,kind,deleted,ver_round,ver_seq,length,sha256
         FROM catalog_candidates ORDER BY logical_path",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let candidate = decode_candidate(row)?;
        for value in [
            candidate.logical_path.as_bytes(),
            candidate.physical_path.as_bytes(),
            candidate.sha256.as_bytes(),
        ] {
            sha.update((value.len() as u32).to_be_bytes());
            sha.update(value);
        }
        sha.update([u8::from(candidate.kind == NodeKind::File)]);
        sha.update([u8::from(candidate.deleted)]);
        sha.update(candidate.version.0.to_be_bytes());
        sha.update(candidate.version.1.to_be_bytes());
        sha.update(candidate.length.to_be_bytes());
        entries = entries
            .checked_add(1)
            .ok_or_else(|| TapeError::Catalog("LTFS 候选数量溢出".into()))?;
    }
    sha.update(entries.to_be_bytes());
    Ok(ImportCandidateStats {
        entries,
        rows_sha256: sha.finalize().into(),
    })
}

fn insert_node(
    conn: &Connection,
    path: &str,
    kind: NodeKind,
    length: Option<u64>,
    attrs: &NodeAttributes,
) -> Result<()> {
    let uid = attrs
        .meta()
        .ok_or_else(|| TapeError::Catalog("LTFS 属性未读取".into()))?
        .file_uid;
    conn.execute(
        "INSERT INTO nodes(path,uid,kind,length,attributes) VALUES (?1,?2,?3,?4,?5)",
        params![
            path,
            uid.to_string(),
            match kind {
                NodeKind::Directory => 0,
                NodeKind::File => 1,
            },
            length.map(|n| n.to_string()),
            attrs.bytes().unwrap()
        ],
    )
    .map_err(|e| TapeError::Catalog(format!("LTFS 路径或 UID 重复/无效: {e}")))?;
    Ok(())
}

fn decode_row(row: &rusqlite::Row<'_>) -> Result<ImportNode> {
    let path: String = row.get(0)?;
    let uid: String = row.get(1)?;
    let kind: i64 = row.get(2)?;
    let length: Option<String> = row.get(3)?;
    let bytes: Vec<u8> = row.get(4)?;
    if !path.is_empty() {
        validate_path(&path)?;
    }
    let attributes =
        NodeAttributes::decode_owned(bytes).map_err(|e| TapeError::Catalog(e.to_string()))?;
    if uid != attributes.meta().unwrap().file_uid.to_string() {
        return Err(TapeError::Catalog("LTFS 导入清单 UID 与属性不符".into()));
    }
    let (kind, length, content_digest) = match (kind, length) {
        (0, None) => (NodeKind::Directory, None, None),
        (1, Some(length)) => {
            let digest = ContentDigest::from_ltfs_xattrs(attributes.xattrs().unwrap())
                .map_err(|e| TapeError::Catalog(e.to_string()))?;
            (
                NodeKind::File,
                Some(
                    length
                        .parse()
                        .map_err(|_| TapeError::Catalog("LTFS 导入长度损坏".into()))?,
                ),
                Some(digest),
            )
        }
        _ => return Err(TapeError::Catalog("LTFS 导入节点种类或长度损坏".into())),
    };
    if path.is_empty() && kind != NodeKind::Directory {
        return Err(TapeError::Catalog("LTFS 导入根节点种类损坏".into()));
    }
    Ok(ImportNode {
        path,
        kind,
        length,
        attributes,
        content_digest,
    })
}

fn digest_rows(conn: &Connection) -> Result<ImportSpoolStats> {
    let mut sha = Sha256::new();
    sha.update(DOMAIN);
    let mut stats = ImportSpoolStats {
        files: 0,
        directories: 0,
        max_attribute_bytes: 0,
        rows_sha256: [0; 32],
    };
    let mut stmt =
        conn.prepare("SELECT path,uid,kind,length,attributes FROM nodes ORDER BY path")?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let node = decode_row(row)?;
        sha.update((node.path.len() as u32).to_be_bytes());
        sha.update(node.path.as_bytes());
        sha.update([match node.kind {
            NodeKind::Directory => 0,
            NodeKind::File => 1,
        }]);
        sha.update(node.attributes.meta().unwrap().file_uid.to_be_bytes());
        sha.update(node.length.unwrap_or(0).to_be_bytes());
        sha.update(node.attributes.digest());
        match node.kind {
            NodeKind::Directory => stats.directories += 1,
            NodeKind::File => stats.files += 1,
        }
        stats.max_attribute_bytes = stats
            .max_attribute_bytes
            .max(node.attributes.bytes().unwrap().len() as u64);
        count = count
            .checked_add(1)
            .ok_or_else(|| TapeError::Catalog("LTFS 导入节点数溢出".into()))?;
    }
    sha.update(count.to_be_bytes());
    stats.rows_sha256 = sha.finalize().into();
    Ok(stats)
}

pub(super) fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.split('/').count() > 1024
        || path.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.len() > 255
                || part.contains('\0')
        })
    {
        return Err(TapeError::Catalog("LTFS 导入路径不安全".into()));
    }
    Ok(())
}

struct HashingRead<R> {
    inner: R,
    sha: Sha256,
}

impl<R: Read> Read for HashingRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.inner.read(buf)?;
        self.sha.update(&buf[..size]);
        Ok(size)
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{DirectoryNode, FileNode, NodeMeta, XATTR_MD5, Xattr};
    use std::io::{BufWriter, Write};

    fn fixture() -> (Vec<u8>, ExpectedIndex) {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "import-spool-test".into(), 'b');
        index.highest_file_uid = 4;
        index.root.subdirs.push(DirectoryNode {
            name: "目录".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            files: vec![FileNode {
                name: "大属性".into(),
                length: u64::MAX,
                meta: NodeMeta {
                    file_uid: 3,
                    ..Default::default()
                },
                xattrs: (0..9)
                    .map(|i| Xattr {
                        key: format!("perf_padding_{i}"),
                        value: "x".repeat(65_536),
                        base64: false,
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        });
        index.root.files.push(FileNode {
            name: "root.txt".into(),
            length: 17,
            meta: NodeMeta {
                file_uid: 4,
                ..Default::default()
            },
            xattrs: vec![Xattr {
                key: XATTR_MD5.into(),
                value: "00000000000000000000000000000000".into(),
                base64: false,
            }],
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        (xml.to_vec(), expected)
    }

    #[test]
    fn byte_budget_pages_keep_oversized_nodes_without_skipping_rows() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "page-byte-budget".into(), 'b');
        index.highest_file_uid = 3;
        index.root.xattrs.push(Xattr {
            key: "root-large".into(),
            value: "x".repeat(9_000_000),
            base64: false,
        });
        for uid in 2..=3 {
            index.root.files.push(FileNode {
                name: format!("file-{uid}"),
                meta: NodeMeta {
                    file_uid: uid,
                    ..Default::default()
                },
                xattrs: vec![Xattr {
                    key: "padding".into(),
                    value: "y".repeat(5_000_000),
                    base64: false,
                }],
                ..Default::default()
            });
        }
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        drop(index);
        let path = std::env::temp_dir().join(format!("tape-rs-page-budget-{}.db", Uuid::new_v4()));
        ImportSpool::build(xml.as_ref(), &path, expected).unwrap();
        let spool = ImportSpool::open(&path, expected).unwrap();
        let first = spool.page(None, 1000).unwrap();
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(first.nodes[0].path, "");
        assert_eq!(first.next.as_deref(), Some(""));
        drop(first);
        let second = spool.page(Some(""), 1000).unwrap();
        assert_eq!(second.nodes.len(), 1);
        assert_eq!(second.nodes[0].path, "file-2");
        assert_eq!(second.next.as_deref(), Some("file-2"));
        let last = spool.page(second.next.as_deref(), 1000).unwrap();
        assert_eq!(last.nodes.len(), 1);
        assert_eq!(last.nodes[0].path, "file-3");
        assert!(last.next.is_none());
        drop(spool);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn confirmed_index_spool_preserves_large_attributes_and_pages_parent_first() {
        let (xml, expected) = fixture();
        let path = std::env::temp_dir().join(format!("tape-rs-import-spool-{}.db", Uuid::new_v4()));
        let stats = ImportSpool::build(xml.as_slice(), &path, expected).unwrap();
        assert_eq!((stats.files, stats.directories), (2, 2));
        assert!(stats.max_attribute_bytes > 590_000);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let spool = ImportSpool::open(&path, expected).unwrap();
        assert_eq!(spool.stats(), stats);
        let mut paths = Vec::new();
        let mut after = None;
        loop {
            let page = spool.page(after.as_deref(), 1).unwrap();
            for node in &page.nodes {
                paths.push(node.path.clone());
                if node.path == "目录/大属性" {
                    assert_eq!(node.length, Some(u64::MAX));
                    assert_eq!(node.attributes.xattrs().unwrap().len(), 9);
                    assert_eq!(node.content_digest, Some(ContentDigest::Unknown));
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(paths, ["", "root.txt", "目录", "目录/大属性"]);
        assert!(spool.page(None, 0).is_err());
        assert!(ImportSpool::build(xml.as_slice(), &path, expected).is_err());
        drop(spool);
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE nodes SET length='18' WHERE path='root.txt'", [])
            .unwrap();
        drop(conn);
        assert!(ImportSpool::open(&path, expected).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_inventory_must_match_entire_extent_cache() {
        let (xml, expected) = fixture();
        let base = std::env::temp_dir().join(format!("tape-rs-source-pair-{}", Uuid::new_v4()));
        let spool_path = base.with_extension("spool.db");
        let cache_path = base.with_extension("extent.db");
        ImportSpool::build(xml.as_slice(), &spool_path, expected).unwrap();
        VolumeExtentCache::build(xml.as_slice(), &cache_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        drop(cache);
        let conn = Connection::open(&cache_path).unwrap();
        conn.execute("DELETE FROM files WHERE path='root.txt'", [])
            .unwrap();
        drop(conn);
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        assert!(spool.verify_extent_cache(&cache).is_err());
        drop(cache);
        drop(spool);
        fs::remove_file(spool_path).unwrap();
        fs::remove_file(cache_path).unwrap();
    }

    #[test]
    fn ordered_source_verification_handles_text_uids_extents_and_corruption() {
        use crate::ltfs::index::{DirectoryNode, Extent};
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "ordered-source".into(), 'b');
        index.highest_file_uid = u64::MAX;
        index.root.files = vec![
            FileNode {
                name: "z-file".into(),
                length: 5,
                meta: NodeMeta {
                    file_uid: 2,
                    ..Default::default()
                },
                extents: vec![
                    Extent {
                        partition: 'b',
                        start_block: 25,
                        byte_offset: 0,
                        byte_count: 3,
                        file_offset: 0,
                    },
                    Extent {
                        partition: 'b',
                        start_block: 80,
                        byte_offset: 7,
                        byte_count: 2,
                        file_offset: 3,
                    },
                ],
                ..Default::default()
            },
            FileNode {
                name: "a-link".into(),
                meta: NodeMeta {
                    file_uid: 10,
                    ..Default::default()
                },
                symlink: Some("z-file".into()),
                ..Default::default()
            },
            FileNode {
                name: "m-empty".into(),
                meta: NodeMeta {
                    file_uid: u64::MAX,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        index.root.subdirs.push(DirectoryNode {
            name: "empty-dir".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let base = std::env::temp_dir().join(format!("ordered-source-{}", Uuid::new_v4()));
        fs::create_dir(&base).unwrap();
        let spool_path = base.join("import.db");
        let cache_path = base.join("extent.db");
        ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
        VolumeExtentCache::build(xml.as_ref(), &cache_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let mut uids = Vec::new();
        assert_eq!(
            cache
                .visit_files_by_uid(|uid, file| {
                    assert_eq!(file, &cache.required_file_by_uid(uid)?);
                    uids.push(uid);
                    Ok(())
                })
                .unwrap(),
            3
        );
        assert_eq!(
            uids,
            [10, u64::MAX, 2],
            "UIDs use the existing TEXT indexes"
        );
        let error = cache
            .visit_files_by_uid(|_, _| Err(TapeError::Catalog("callback stopped".into())))
            .unwrap_err();
        assert!(error.to_string().contains("callback stopped"));
        drop(cache);
        for mutation in [
            "UPDATE files SET path='changed' WHERE uid='2'",
            "UPDATE files SET proof=zeroblob(32) WHERE uid='2'",
            "UPDATE files SET length='bad' WHERE uid='2'",
            "UPDATE extents SET start_block='900' WHERE uid='2' AND seq=1",
            "UPDATE extents SET partition='bb' WHERE uid='2' AND seq=1",
            "DELETE FROM extents WHERE uid='2' AND seq=1",
            "DELETE FROM files WHERE uid='18446744073709551615'",
            "UPDATE files SET uid='018446744073709551615' WHERE uid='18446744073709551615'",
            "INSERT INTO extents VALUES('11',0,'b','1','0','1','0')",
            "INSERT INTO extents VALUES('9',0,'b','1','0','1','0')",
        ] {
            let changed = base.join("changed.db");
            fs::copy(&cache_path, &changed).unwrap();
            let conn = Connection::open(&changed).unwrap();
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute_batch(mutation).unwrap();
            drop(conn);
            let cache = VolumeExtentCache::open(&changed, expected).unwrap();
            assert!(
                spool.verify_extent_cache(&cache).is_err(),
                "accepted {mutation}"
            );
            drop(cache);
            fs::remove_file(changed).unwrap();
        }
        drop(spool);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn ordered_source_verification_accepts_no_files_and_checks_trailing_directories() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "empty-source".into(), 'b');
        index.highest_file_uid = 1;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let base = std::env::temp_dir().join(format!("empty-source-{}", Uuid::new_v4()));
        fs::create_dir(&base).unwrap();
        let spool_path = base.join("import.db");
        let cache_path = base.join("extent.db");
        ImportSpool::build(xml.as_ref(), &spool_path, expected).unwrap();
        VolumeExtentCache::build(xml.as_ref(), &cache_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let conn = Connection::open(&spool_path).unwrap();
        conn.execute("UPDATE nodes SET uid='999' WHERE path=''", [])
            .unwrap();
        drop(conn);
        assert!(spool.verify_extent_cache(&cache).is_err());
        drop(cache);
        drop(spool);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn mismatched_or_truncated_index_does_not_publish_a_spool() {
        let (xml, mut expected) = fixture();
        let path =
            std::env::temp_dir().join(format!("tape-rs-import-spool-bad-{}.db", Uuid::new_v4()));
        expected.sha256[0] ^= 1;
        assert!(ImportSpool::build(xml.as_slice(), &path, expected).is_err());
        assert!(!path.exists());
        expected.sha256[0] ^= 1;
        assert!(ImportSpool::build(&xml[..xml.len() / 2], &path, expected).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn logical_candidates_sort_tombstones_by_target_and_reject_tampering() {
        use crate::ltfs::index::DirectoryNode;
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "candidate-test".into(), 'b');
        index.self_location.start_block = 5;
        index.root.files.push(FileNode {
            name: "alive".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            xattrs: vec![Xattr {
                key: XATTR_VERSION.into(),
                value: "9.4".into(),
                base64: false,
            }],
            ..Default::default()
        });
        let tombstone = |target: &str, uid, version: &str| FileNode {
            name: tombstone_path(target).rsplit('/').next().unwrap().into(),
            meta: NodeMeta {
                file_uid: uid,
                ..Default::default()
            },
            xattrs: vec![
                Xattr {
                    key: XATTR_VERSION.into(),
                    value: version.into(),
                    base64: false,
                },
                Xattr {
                    key: XATTR_DELETED_PATH.into(),
                    value: target.into(),
                    base64: false,
                },
                Xattr {
                    key: "tapers.deletedKind".into(),
                    value: "file".into(),
                    base64: false,
                },
            ],
            ..Default::default()
        };
        index.root.subdirs.push(DirectoryNode {
            name: ".tapers".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            subdirs: vec![DirectoryNode {
                name: "deleted".into(),
                meta: NodeMeta {
                    file_uid: 4,
                    ..Default::default()
                },
                files: vec![
                    tombstone("/gone", 5, "10.2"),
                    tombstone("/alive", 6, "11.1"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        });
        index.highest_file_uid = 6;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = std::env::temp_dir().join(format!("tape-rs-candidates-{}.db", Uuid::new_v4()));
        ImportSpool::build(xml.as_ref(), &path, expected).unwrap();
        let spool = ImportSpool::open(&path, expected).unwrap();
        assert_eq!(spool.candidate_stats().entries, 2);
        let first = spool.candidate_page(None, 1).unwrap();
        assert_eq!(first.candidates[0].logical_path, "/alive");
        assert!(!first.candidates[0].deleted);
        assert_eq!(first.candidates[0].version, (9, 4));
        let second = spool.candidate_page(first.next.as_deref(), 1).unwrap();
        assert_eq!(second.candidates[0].logical_path, "/gone");
        assert!(second.candidates[0].deleted);
        assert_eq!(second.candidates[0].version, (10, 2));
        assert_eq!(second.next, None);
        assert!(
            spool
                .node_by_path(&second.candidates[0].physical_path)
                .unwrap()
                .is_some()
        );
        drop(spool);
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE catalog_candidates SET ver_seq='100' WHERE logical_path='/alive'",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(ImportSpool::open(&path, expected).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_tombstone_or_version_never_publishes_candidates() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "candidate-bad".into(), 'b');
        index.self_location.start_block = 5;
        index.root.files.push(FileNode {
            name: "bad".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            xattrs: vec![Xattr {
                key: XATTR_VERSION.into(),
                value: "not-a-version".into(),
                base64: false,
            }],
            ..Default::default()
        });
        let path =
            std::env::temp_dir().join(format!("tape-rs-bad-candidate-{}.db", Uuid::new_v4()));
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        assert!(ImportSpool::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
        index.root.files[0].xattrs.clear();
        index.root.files[0].name = tombstone_path("/gone").rsplit('/').next().unwrap().into();
        let bad_tombstone = index.root.files.remove(0);
        index.root.subdirs.push(crate::ltfs::index::DirectoryNode {
            name: ".tapers".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            subdirs: vec![crate::ltfs::index::DirectoryNode {
                name: "deleted".into(),
                meta: NodeMeta {
                    file_uid: 4,
                    ..Default::default()
                },
                files: vec![bad_tombstone],
                ..Default::default()
            }],
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            sha256: Sha256::digest(&xml).into(),
            ..expected
        };
        assert!(ImportSpool::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn ee_full_index_spools_user_aliases_and_native_directories() {
        let xml = include_bytes!("../../tests/fixtures/ee-recovery-dp-54.xml");
        let index = LtfsIndex::parse(xml).unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(xml).into(),
        };
        let path =
            std::env::temp_dir().join(format!("tape-rs-import-spool-ee-{}.db", Uuid::new_v4()));
        let cache_path = path.with_extension("extent.db");
        let stats = ImportSpool::build(xml.as_slice(), &path, expected).unwrap();
        VolumeExtentCache::build(xml.as_slice(), &cache_path, expected).unwrap();
        assert!(stats.files > 0 && stats.directories > 1);
        let spool = ImportSpool::open(&path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let mut after = None;
        let mut alias = false;
        loop {
            let page = spool.page(after.as_deref(), 7).unwrap();
            for node in &page.nodes {
                if node.attributes.symlink().is_some() {
                    alias = true;
                    assert_eq!(node.kind, NodeKind::File);
                    let data = spool.resolve_file_data(&node.path).unwrap().unwrap();
                    assert_eq!(
                        data.logical_file_uid,
                        node.attributes.meta().unwrap().file_uid
                    );
                    assert_ne!(data.data_file_uid, data.logical_file_uid);
                    assert!(data.data_path.starts_with(".LTFSEE_DATA/"));
                    let target = spool.node_by_path(&data.data_path).unwrap().unwrap();
                    assert_eq!(
                        data.data_file_uid,
                        target.attributes.meta().unwrap().file_uid
                    );
                    assert_eq!(data.length, target.length.unwrap());
                    let direct = spool.resolve_file_data(&data.data_path).unwrap().unwrap();
                    assert_eq!(direct.logical_file_uid, direct.data_file_uid);
                    assert_eq!(direct.data_file_uid, data.data_file_uid);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        assert!(alias);
        let mut cursor = None;
        let mut user_aliases = 0;
        loop {
            let page = spool.candidate_page(cursor.as_deref(), 7).unwrap();
            for candidate in &page.candidates {
                assert!(!candidate.logical_path.starts_with("/.LTFSEE_DATA"));
                let node = spool
                    .node_by_path(&candidate.physical_path)
                    .unwrap()
                    .unwrap();
                user_aliases += usize::from(node.attributes.symlink().is_some());
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert!(user_aliases > 0);
        drop(cache);
        drop(spool);
        fs::remove_file(path).unwrap();
        fs::remove_file(cache_path).unwrap();
    }

    #[test]
    fn ee_alias_target_cannot_escape_or_use_another_directory() {
        let path = "a/b/link";
        assert_eq!(
            resolve_ee_alias_target(path, "../../.LTFSEE_DATA/object").unwrap(),
            ".LTFSEE_DATA/object"
        );
        for target in [
            "/.LTFSEE_DATA/object",
            "../../../.LTFSEE_DATA/object",
            "../../other/object",
            "../../.LTFSEE_DATA//object",
            "../../.LTFSEE_DATA/../other/object",
        ] {
            assert!(resolve_ee_alias_target(path, target).is_err(), "{target}");
        }
    }

    #[test]
    fn reserved_ee_data_directory_without_alias_is_not_silently_omitted() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "reserved-data-test".into(), 'b');
        index.highest_file_uid = 2;
        index.root.subdirs.push(DirectoryNode {
            name: ".LTFSEE_DATA".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = std::env::temp_dir().join(format!("tape-rs-ambiguous-ee-{}.db", Uuid::new_v4()));
        assert!(ImportSpool::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
        index.root.subdirs.clear();
        index.root.files.push(FileNode {
            name: ".LTFSEE_DATA".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            ..Default::default()
        });
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            sha256: Sha256::digest(&xml).into(),
            ..expected
        };
        assert!(ImportSpool::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
    }

    #[test]
    #[ignore = "百万文件导入清单压力测试；需显式运行并提供足够磁盘空间"]
    fn million_file_spool_is_bounded_and_reopenable() {
        const COUNT: u64 = 1_000_000;
        let dir = std::env::var_os("TAPE_RS_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let nonce = Uuid::new_v4();
        let xml_path = dir.join(format!("tape-rs-import-spool-{nonce}.xml"));
        let db_path = dir.join(format!("tape-rs-import-spool-{nonce}.db"));
        let cache_path = dir.join(format!("tape-rs-import-spool-{nonce}.extent.db"));
        let _xml_cleanup = RemoveOnDrop(xml_path.clone());
        let _db_cleanup = RemoveOnDrop(db_path.clone());
        let _cache_cleanup = RemoveOnDrop(cache_path.clone());
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "import-million".into(), 'b');
        index.self_location.start_block = 5;
        index.highest_file_uid = COUNT + 1;
        let xml = index.to_xml().unwrap();
        let marker = b"<contents>";
        let split = xml.windows(marker.len()).position(|w| w == marker).unwrap() + marker.len();
        {
            let mut out = BufWriter::new(File::create(&xml_path).unwrap());
            out.write_all(&xml[..split]).unwrap();
            for number in 0..COUNT {
                write!(
                    out,
                    "<file><name>f{number}</name><length>0</length><fileuid>{}</fileuid></file>",
                    number + 2
                )
                .unwrap();
            }
            out.write_all(&xml[split..]).unwrap();
            out.flush().unwrap();
        }
        let mut input = File::open(&xml_path).unwrap();
        let mut sha = Sha256::new();
        let mut buf = [0; 65536];
        loop {
            let n = input.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
        }
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: sha.finalize().into(),
        };
        let started = std::time::Instant::now();
        let stats = ImportSpool::build(File::open(&xml_path).unwrap(), &db_path, expected).unwrap();
        assert_eq!((stats.files, stats.directories), (COUNT, 1));
        let spool = ImportSpool::open(&db_path, expected).unwrap();
        let spool_elapsed = started.elapsed();
        VolumeExtentCache::build(File::open(&xml_path).unwrap(), &cache_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let first = spool.page(None, 1).unwrap();
        assert_eq!(first.nodes[0].path, "");
        let last = spool.page(Some(&format!("f{}", COUNT - 2)), 1).unwrap();
        assert_eq!(last.nodes[0].path, format!("f{}", COUNT - 1));
        let merge_started = std::time::Instant::now();
        let mut reconciler = crate::managed_catalog::ImportReconciler::new(vec![
            crate::managed_catalog::ImportVolume {
                pool_id: Uuid::new_v4(),
                barcode: "BENCHL8".into(),
                volume_id: expected.volume_uuid,
                generation: expected.generation,
                spool: &spool,
            },
        ])
        .unwrap();
        let mut merged = 0u64;
        while let Some(path) = reconciler.next_path().unwrap() {
            assert!(!path.current().candidate.deleted);
            merged += 1;
        }
        assert_eq!(merged, COUNT);
        println!(
            "million import source pair: spool={spool_elapsed:?}, merge={:?}, total={:?}, xml={} spool={} cache={}",
            merge_started.elapsed(),
            started.elapsed(),
            fs::metadata(&xml_path).unwrap().len(),
            fs::metadata(&db_path).unwrap().len(),
            fs::metadata(&cache_path).unwrap().len()
        );
    }
}
