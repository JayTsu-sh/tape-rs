//! Bounded, read-only merge of independently confirmed volume inventories.
//! No result may be published until every assigned volume has been supplied
//! and the merged view has been checked against a stable directory snapshot.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::Read;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{CatalogError, ImportCandidate, ImportNode, ImportSpool, NodeKind, Result};

const PAGE: usize = 8;
const AUDIT_DOMAIN: &[u8] = b"tape-rs/import-directory-audit/2";

#[derive(Clone)]
pub struct ImportVolume<'a> {
    pub pool_id: Uuid,
    pub barcode: String,
    pub volume_id: Uuid,
    pub generation: u64,
    pub spool: &'a ImportSpool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCandidate {
    pub barcode: String,
    pub volume_id: Uuid,
    pub generation: u64,
    pub candidate: ImportCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciledPath {
    pub pool_id: Uuid,
    pub logical_path: String,
    pub sources: Vec<SourceCandidate>,
    pub winner: usize,
}

/// Explicit provenance of the imported view; included in the sealed manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportOrigin {
    LegacySnapshot,
    ExternalMedia { library_sha256: [u8; 32] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryAudit {
    pub origin: ImportOrigin,
    pub applied_index: u64,
    pub frozen_index: u64,
    pub paths: u64,
    pub tombstones: u64,
    pub rows_sha256: [u8; 32],
    pub snapshot_sha256: [u8; 32],
}

pub(super) fn snapshot_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| CatalogError::Cache(format!("目录快照文件读取失败: {error}")))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let bytes = file
            .read(&mut buffer)
            .map_err(|error| CatalogError::Cache(format!("目录快照文件读取失败: {error}")))?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
    }
    Ok(digest.finalize().into())
}

impl ReconciledPath {
    pub fn current(&self) -> &SourceCandidate {
        &self.sources[self.winner]
    }
}

struct SourceState<'a> {
    source: ImportVolume<'a>,
    after: Option<String>,
    ready: VecDeque<ImportCandidate>,
    exhausted: bool,
}

impl SourceState<'_> {
    fn refill(&mut self) -> Result<()> {
        if !self.ready.is_empty() || self.exhausted {
            return Ok(());
        }
        let page = self
            .source
            .spool
            .candidate_page(self.after.as_deref(), PAGE)
            .map_err(|error| CatalogError::Cache(error.to_string()))?;
        self.exhausted = page.next.is_none();
        self.after = page.next;
        self.ready = page.candidates.into();
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct CursorKey {
    pool_id: Uuid,
    path: String,
    source: usize,
}

pub struct ImportReconciler<'a> {
    sources: Vec<SourceState<'a>>,
    next: BinaryHeap<Reverse<CursorKey>>,
    by_barcode: HashMap<String, usize>,
}

impl<'a> ImportReconciler<'a> {
    pub fn new(volumes: Vec<ImportVolume<'a>>) -> Result<Self> {
        let mut by_barcode = HashMap::new();
        let mut sources = Vec::with_capacity(volumes.len());
        for source in volumes {
            let evidence = source.spool.source();
            if source.pool_id.is_nil()
                || source.barcode.is_empty()
                || source.barcode.len() > 32
                || !source.barcode.is_ascii()
                || source.volume_id.is_nil()
                || source.generation == 0
                || evidence.volume_uuid != source.volume_id
                || evidence.generation != source.generation
                || by_barcode
                    .insert(source.barcode.clone(), sources.len())
                    .is_some()
            {
                return Err(CatalogError::Invalid("导入卷身份、池或条码无效/重复"));
            }
            sources.push(SourceState {
                source,
                after: None,
                ready: VecDeque::new(),
                exhausted: false,
            });
        }
        let mut merged = Self {
            sources,
            next: BinaryHeap::new(),
            by_barcode,
        };
        for index in 0..merged.sources.len() {
            merged.push_head(index)?;
        }
        Ok(merged)
    }

    fn push_head(&mut self, index: usize) -> Result<()> {
        let source = &mut self.sources[index];
        source.refill()?;
        if let Some(candidate) = source.ready.front() {
            self.next.push(Reverse(CursorKey {
                pool_id: source.source.pool_id,
                path: candidate.logical_path.clone(),
                source: index,
            }));
        }
        Ok(())
    }

    fn take(&mut self, key: CursorKey) -> Result<SourceCandidate> {
        let source = &mut self.sources[key.source];
        let candidate = source
            .ready
            .pop_front()
            .ok_or(CatalogError::Corrupt("导入卷候选游标缺项"))?;
        if source.source.pool_id != key.pool_id || candidate.logical_path != key.path {
            return Err(CatalogError::Corrupt("导入卷候选游标错位"));
        }
        let result = SourceCandidate {
            barcode: source.source.barcode.clone(),
            volume_id: source.source.volume_id,
            generation: source.source.generation,
            candidate,
        };
        self.push_head(key.source)?;
        Ok(result)
    }

    /// One logical path at a time. Equal versions on different volumes are
    /// ambiguous without an independently verified replica relationship.
    pub fn next_path(&mut self) -> Result<Option<ReconciledPath>> {
        let Some(Reverse(first)) = self.next.pop() else {
            return Ok(None);
        };
        let pool_id = first.pool_id;
        let logical_path = first.path.clone();
        let mut sources = vec![self.take(first)?];
        while self
            .next
            .peek()
            .is_some_and(|Reverse(key)| key.pool_id == pool_id && key.path == logical_path)
        {
            let Reverse(key) = self.next.pop().expect("peeked candidate");
            sources.push(self.take(key)?);
        }
        let latest = sources
            .iter()
            .map(|source| source.candidate.version)
            .max()
            .expect("at least one candidate");
        let mut winners = sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.candidate.version == latest)
            .map(|(index, _)| index);
        let winner = winners.next().expect("latest version exists");
        if winners.next().is_some() {
            return Err(CatalogError::Invalid(
                "跨卷同一路径出现并列版本，不能自动选主",
            ));
        }
        Ok(Some(ReconciledPath {
            pool_id,
            logical_path,
            sources,
            winner,
        }))
    }

    /// Compare every on-tape winner with a stable, offline SQLite snapshot.
    /// A live WAL database is rejected because a multi-hour audit would pin
    /// its WAL and still race catalog changes after the audit finishes.
    pub fn audit_directory_snapshot(&mut self, path: &Path) -> Result<DirectoryAudit> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite_error)?;
        let journal: String = db
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(sqlite_error)?;
        if journal.eq_ignore_ascii_case("wal") {
            return Err(CatalogError::Invalid("目录审计必须使用离线一致快照"));
        }
        db.execute_batch("BEGIN").map_err(sqlite_error)?;
        let (applied, frozen): (Option<i64>, Option<i64>) = db
            .query_row(
                "SELECT (SELECT v FROM meta WHERE k='applied_index'),
                        (SELECT v FROM meta WHERE k='catalog_frozen_index')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(sqlite_error)?;
        let applied_index = applied
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or(CatalogError::Invalid("目录快照缺少有效应用水位"))?;
        let frozen_index = frozen
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0 && *value <= applied_index)
            .ok_or(CatalogError::Invalid("目录快照缺少有效冻结索引"))?;
        let (staging, unresolved_tasks): (i64, i64) = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM staging LIMIT 1),
                        EXISTS(SELECT 1 FROM file_tasks
                               WHERE status NOT IN ('committed','failed') LIMIT 1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(sqlite_error)?;
        if staging != 0 {
            return Err(CatalogError::Invalid("目录快照仍有未合并索引片"));
        }
        if unresolved_tasks != 0 {
            return Err(CatalogError::Invalid("目录快照仍有未决文件任务"));
        }
        let sources_sha256 = self.check_tape_coverage(&db)?;
        let mut stmt = db
            .prepare(
                "SELECT pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata
                 FROM files ORDER BY pool_uuid,path",
            )
            .map_err(sqlite_error)?;
        let mut rows = stmt.query([]).map_err(sqlite_error)?;
        let mut digest = Sha256::new();
        digest.update(AUDIT_DOMAIN);
        digest.update(applied_index.to_be_bytes());
        digest.update(frozen_index.to_be_bytes());
        digest.update(sources_sha256);
        let mut paths = 0u64;
        let mut tombstones = 0u64;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let saved = DirectoryRow::decode(row).map_err(sqlite_error)?;
            let pool_id = Uuid::parse_str(&saved.pool)
                .map_err(|_| CatalogError::Invalid("目录快照池 UUID 无效"))?;
            let current = self
                .next_path()?
                .ok_or_else(|| CatalogError::Cache(format!("目录快照多出路径 {}", saved.path)))?;
            if current.pool_id != pool_id || current.logical_path != saved.path {
                return Err(CatalogError::Cache(format!(
                    "目录快照与介质候选路径不一致: 目录 {} / 介质 {}",
                    saved.path, current.logical_path
                )));
            }
            let winner = current.current();
            let candidate = &winner.candidate;
            let generation = nonnegative(saved.generation, "目录卷代数")?;
            let length = nonnegative(saved.length, "目录文件长度")?;
            let round = nonnegative(saved.round, "目录版本轮次")?;
            let seq = nonnegative(saved.seq, "目录版本序号")?;
            let deleted = match saved.deleted {
                0 => false,
                1 => true,
                _ => return Err(CatalogError::Corrupt("目录删除位")),
            };
            if saved.barcode != winner.barcode
                || generation != winner.generation
                || length != candidate.length
                || saved.sha256 != candidate.sha256
                || (round, seq) != candidate.version
                || deleted != candidate.deleted
            {
                return Err(CatalogError::Cache(format!(
                    "目录快照与介质候选内容不一致: {}",
                    saved.path
                )));
            }
            let source = self
                .by_barcode
                .get(&winner.barcode)
                .and_then(|index| self.sources.get(*index))
                .ok_or(CatalogError::Corrupt("导入卷条码索引缺失"))?;
            let node = source
                .source
                .spool
                .node_by_path(&candidate.physical_path)
                .map_err(|error| CatalogError::Cache(error.to_string()))?
                .ok_or(CatalogError::Corrupt("导入候选缺少物理节点"))?;
            let projected = project_metadata(candidate, &node)?;
            let metadata: serde_json::Value = serde_json::from_str(&saved.metadata)
                .map_err(|_| CatalogError::Corrupt("目录快照元数据 JSON"))?;
            if metadata != projected {
                return Err(CatalogError::Cache(format!(
                    "目录快照与 LTFS 属性不一致: {}",
                    saved.path
                )));
            }
            digest.update(pool_id.as_bytes());
            hash_text(&mut digest, &saved.path);
            hash_text(&mut digest, &winner.barcode);
            digest.update(winner.volume_id.as_bytes());
            digest.update(winner.generation.to_be_bytes());
            digest.update(round.to_be_bytes());
            digest.update(seq.to_be_bytes());
            digest.update([u8::from(deleted)]);
            digest.update(length.to_be_bytes());
            digest.update(node.attributes.digest());
            paths = paths
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("目录审计路径数溢出"))?;
            tombstones += u64::from(deleted);
        }
        if let Some(extra) = self.next_path()? {
            return Err(CatalogError::Cache(format!(
                "目录快照缺少介质路径 {}",
                extra.logical_path
            )));
        }
        digest.update(paths.to_be_bytes());
        digest.update(tombstones.to_be_bytes());
        Ok(DirectoryAudit {
            origin: ImportOrigin::LegacySnapshot,
            applied_index,
            frozen_index,
            paths,
            tombstones,
            rows_sha256: digest.finalize().into(),
            snapshot_sha256: snapshot_sha256(path)?,
        })
    }

    /// Audit external media separately from a legacy catalog migration. The
    /// old authority must be a frozen, entirely empty offline snapshot. The
    /// caller still binds every source to a ready managed FDB volume and later
    /// certifies three actual empty node exports before route activation.
    pub fn audit_external_sources(
        &mut self,
        path: &Path,
        library_serial: &str,
    ) -> Result<DirectoryAudit> {
        super::control::validate_physical_serial(library_serial)?;
        let library_sha256: [u8; 32] = Sha256::digest(library_serial.as_bytes()).into();
        let identity = super::inspect_frozen_snapshot(path)?;
        if identity.row_counts != [0; 4] {
            return Err(CatalogError::Invalid(
                "外部介质导入要求旧目录无池、磁带、文件或任务",
            ));
        }
        if self.sources.is_empty() {
            return Err(CatalogError::Invalid("外部介质导入缺少已核验来源卷"));
        }
        let mut digest = Sha256::new();
        digest.update(b"tape-rs/external-media-import-audit/1");
        digest.update(library_sha256);
        digest.update(identity.applied_index.to_be_bytes());
        digest.update(identity.frozen_index.to_be_bytes());
        digest.update(identity.snapshot_sha256);
        // Bind even empty volumes, independent of caller order. The full source
        // identity also commits to attributes/extents outside logical candidates.
        let mut sources: Vec<_> = self.sources.iter().map(|state| &state.source).collect();
        sources.sort_by(|a, b| (a.pool_id, &a.barcode).cmp(&(b.pool_id, &b.barcode)));
        digest.update((sources.len() as u64).to_be_bytes());
        for source in sources {
            let index = source.spool.source();
            digest.update(source.pool_id.as_bytes());
            hash_text(&mut digest, &source.barcode);
            digest.update(source.volume_id.as_bytes());
            digest.update(source.generation.to_be_bytes());
            digest.update((index.location.partition as u32).to_be_bytes());
            digest.update(index.location.start_block.to_be_bytes());
            digest.update(index.sha256);
        }
        let mut paths = 0u64;
        let mut tombstones = 0u64;
        while let Some(current) = self.next_path()? {
            let winner = current.current();
            let candidate = &winner.candidate;
            let source = &self.sources[*self
                .by_barcode
                .get(&winner.barcode)
                .ok_or(CatalogError::Corrupt("外部导入来源缺失"))?]
            .source;
            let node = source
                .spool
                .node_by_path(&candidate.physical_path)
                .map_err(|error| CatalogError::Cache(error.to_string()))?
                .ok_or(CatalogError::Corrupt("外部导入缺少物理节点"))?;
            digest.update(current.pool_id.as_bytes());
            hash_text(&mut digest, &current.logical_path);
            hash_text(&mut digest, &winner.barcode);
            digest.update(winner.volume_id.as_bytes());
            digest.update(winner.generation.to_be_bytes());
            digest.update(candidate.version.0.to_be_bytes());
            digest.update(candidate.version.1.to_be_bytes());
            digest.update([u8::from(candidate.deleted)]);
            digest.update(candidate.length.to_be_bytes());
            digest.update(node.attributes.digest());
            paths = paths
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("外部导入路径数溢出"))?;
            tombstones += u64::from(candidate.deleted);
        }
        if super::inspect_frozen_snapshot(path)? != identity {
            return Err(CatalogError::Invalid("外部导入期间冻结目录发生变化"));
        }
        digest.update(paths.to_be_bytes());
        digest.update(tombstones.to_be_bytes());
        Ok(DirectoryAudit {
            origin: ImportOrigin::ExternalMedia { library_sha256 },
            applied_index: identity.applied_index,
            frozen_index: identity.frozen_index,
            paths,
            tombstones,
            rows_sha256: digest.finalize().into(),
            snapshot_sha256: identity.snapshot_sha256,
        })
    }

    fn check_tape_coverage(&self, db: &Connection) -> Result<[u8; 32]> {
        let mut seen = vec![false; self.sources.len()];
        let mut digest = Sha256::new();
        digest.update(b"tape-rs/import-directory-sources/1");
        let mut stmt = db
            .prepare("SELECT barcode,pool_uuid,volume_uuid,generation FROM tapes ORDER BY barcode")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sqlite_error)?;
        for row in rows {
            let (barcode, pool, volume, generation) = row.map_err(sqlite_error)?;
            hash_text(&mut digest, &barcode);
            hash_text(&mut digest, &pool);
            hash_text(&mut digest, &volume);
            digest.update(generation.to_be_bytes());
            let source = self.by_barcode.get(&barcode).copied();
            if volume.is_empty() && generation == 0 {
                if source.is_some() {
                    return Err(CatalogError::Invalid("空白磁带不能有索引清单"));
                }
                digest.update([0]);
                continue;
            }
            let index = source.ok_or(CatalogError::Incomplete)?;
            let source = &self.sources[index].source;
            if source.pool_id.to_string() != pool
                || source.volume_id.to_string() != volume
                || i64::try_from(source.generation).ok() != Some(generation)
            {
                return Err(CatalogError::Invalid("导入卷与目录快照磁带归属不符"));
            }
            digest.update([1]);
            digest.update(source.spool.source().sha256);
            digest.update(source.spool.stats().rows_sha256);
            let candidate_stats = source.spool.candidate_stats();
            digest.update(candidate_stats.entries.to_be_bytes());
            digest.update(candidate_stats.rows_sha256);
            seen[index] = true;
        }
        if seen.iter().any(|present| !present) {
            return Err(CatalogError::Invalid("导入来源包含快照未登记的磁带"));
        }
        digest.update((self.sources.len() as u64).to_be_bytes());
        Ok(digest.finalize().into())
    }
}

fn sqlite_error(error: rusqlite::Error) -> CatalogError {
    CatalogError::Cache(format!("离线目录快照读取失败: {error}"))
}

fn nonnegative(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| CatalogError::Corrupt(field))
}

fn hash_text(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u32).to_be_bytes());
    hash.update(value.as_bytes());
}

struct DirectoryRow {
    pool: String,
    path: String,
    barcode: String,
    generation: i64,
    length: i64,
    sha256: String,
    round: i64,
    seq: i64,
    deleted: i64,
    metadata: String,
}

impl DirectoryRow {
    fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            pool: row.get(0)?,
            path: row.get(1)?,
            barcode: row.get(2)?,
            generation: row.get(3)?,
            length: row.get(4)?,
            sha256: row.get(5)?,
            round: row.get(6)?,
            seq: row.get(7)?,
            deleted: row.get(8)?,
            metadata: row.get(9)?,
        })
    }
}

fn project_metadata(candidate: &ImportCandidate, node: &ImportNode) -> Result<serde_json::Value> {
    let meta = node
        .attributes
        .meta()
        .ok_or(CatalogError::Corrupt("导入节点缺少 LTFS 属性"))?;
    let xattrs = node
        .attributes
        .xattrs()
        .ok_or(CatalogError::Corrupt("导入节点缺少 xattr"))?;
    if candidate.deleted {
        if node.kind != NodeKind::File || node.length != Some(0) {
            return Err(CatalogError::Corrupt("墓碑候选物理节点不符"));
        }
        return Ok(if candidate.kind == NodeKind::Directory {
            serde_json::json!({"kind":"directory"})
        } else {
            serde_json::Value::Null
        });
    }
    if candidate.kind != node.kind || candidate.length != node.length.unwrap_or(0) {
        return Err(CatalogError::Corrupt("普通候选物理节点不符"));
    }
    let mut value = serde_json::json!({
        "modify_time": meta.modify_time,
        "node": {
            "readonly": meta.readonly,
            "creation_time": meta.creation_time,
            "change_time": meta.change_time,
            "modify_time": meta.modify_time,
            "access_time": meta.access_time,
            "backup_time": meta.backup_time,
        },
        "xattrs": xattrs.iter().map(|xattr| serde_json::json!({
            "key": xattr.key, "value": xattr.value, "base64": xattr.base64,
        })).collect::<Vec<_>>(),
    });
    if candidate.kind == NodeKind::Directory {
        value["kind"] = serde_json::json!("directory");
    } else if let Some(target) = node.attributes.symlink() {
        value["kind"] = serde_json::json!("symlink");
        value["symlink"] = serde_json::json!(target);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::volume_extents::ExpectedIndex;
    use crate::ltfs::index::{DirectoryNode, FileNode, LtfsIndex, NodeMeta, Xattr};
    use crate::ltfs::volume::{XATTR_DELETED_PATH, XATTR_VERSION, tombstone_path};
    use sha2::{Digest, Sha256};

    fn file(name: &str, uid: u64, version: &str) -> FileNode {
        FileNode {
            name: name.into(),
            meta: NodeMeta {
                file_uid: uid,
                ..Default::default()
            },
            xattrs: vec![Xattr {
                key: XATTR_VERSION.into(),
                value: version.into(),
                base64: false,
            }],
            ..Default::default()
        }
    }

    fn spool(index: &LtfsIndex) -> (std::path::PathBuf, ImportSpool) {
        let xml = index.to_xml().unwrap();
        let source = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = std::env::temp_dir().join(format!("tape-rs-merge-{}.db", Uuid::new_v4()));
        ImportSpool::build(xml.as_ref(), &path, source).unwrap();
        let opened = ImportSpool::open(&path, source).unwrap();
        (path, opened)
    }

    #[test]
    fn external_import_requires_empty_frozen_authority_and_keeps_legacy_audit_strict() {
        use crate::daemon::directory::Directory;
        use crate::daemon::state::{Applied, Command, ControlState};
        let root = std::env::temp_dir().join(format!("external-audit-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut directory = Directory::open(&root.join("live.db")).unwrap();
        let mut control = ControlState::default();
        for (index, command) in [
            (1, Command::CatalogQuiesce),
            (2, Command::CatalogFreeze { quiesce_index: 1 }),
        ] {
            let outcome = control.apply(index, &command);
            assert!(matches!(outcome, Applied::Admin(Ok(_))));
            directory.apply(index, &command, &outcome).unwrap();
        }
        let snapshot = root.join("empty.snap.db");
        directory.write_snapshot(&snapshot).unwrap();
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "external".into(), 'b');
        index.root.files.push(file("external-file", 2, "1.1"));
        index.highest_file_uid = 2;
        let (path, spool) = spool(&index);
        let pool = Uuid::new_v4();
        let sources = || {
            vec![ImportVolume {
                pool_id: pool,
                barcode: "EXTERNL8".into(),
                volume_id: index.volume_uuid,
                generation: index.generation,
                spool: &spool,
            }]
        };
        assert!(
            ImportReconciler::new(sources())
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        let audit = ImportReconciler::new(sources())
            .unwrap()
            .audit_external_sources(&snapshot, "EXTERNAL01")
            .unwrap();
        assert!(matches!(audit.origin, ImportOrigin::ExternalMedia { .. }));
        assert_eq!(
            (audit.paths, audit.tombstones, audit.frozen_index),
            (1, 0, 2)
        );
        assert_eq!(
            audit,
            ImportReconciler::new(sources())
                .unwrap()
                .audit_external_sources(&snapshot, "EXTERNAL01")
                .unwrap()
        );
        assert!(
            ImportReconciler::new(Vec::new())
                .unwrap()
                .audit_external_sources(&snapshot, "EXTERNAL01")
                .is_err()
        );
        assert!(
            ImportReconciler::new(sources())
                .unwrap()
                .audit_external_sources(&root.join("live.db"), "EXTERNAL01")
                .is_err()
        );
        let changed = root.join("changed.snap.db");
        for sql in [
            "INSERT INTO pools(uuid,name,file_limit) VALUES('p','legacy',1)",
            "UPDATE meta SET v=0 WHERE k='catalog_frozen_index'",
            "INSERT INTO staging(barcode,generation,part,path,length,sha256) VALUES('T',1,0,'/f',1,'x')",
        ] {
            std::fs::copy(&snapshot, &changed).unwrap();
            let db = Connection::open(&changed).unwrap();
            db.execute_batch(sql).unwrap();
            drop(db);
            assert!(
                ImportReconciler::new(sources())
                    .unwrap()
                    .audit_external_sources(&changed, "EXTERNAL01")
                    .is_err(),
                "accepted {sql}"
            );
            std::fs::remove_file(&changed).unwrap();
        }
        drop(spool);
        drop(directory);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newer_copy_and_tombstone_win_across_volumes() {
        let mut first = LtfsIndex::empty(Uuid::new_v4(), "merge-1".into(), 'b');
        first.self_location.start_block = 5;
        first.root.files = vec![file("a", 2, "1.1"), file("old", 3, "1.2")];
        first.highest_file_uid = 3;
        let mut second = LtfsIndex::empty(Uuid::new_v4(), "merge-2".into(), 'b');
        second.self_location.start_block = 5;
        second.root.files.push(file("a", 2, "2.1"));
        let mut tomb = file(tombstone_path("/old").rsplit('/').next().unwrap(), 5, "2.2");
        tomb.xattrs.push(Xattr {
            key: XATTR_DELETED_PATH.into(),
            value: "/old".into(),
            base64: false,
        });
        second.root.subdirs.push(DirectoryNode {
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
                files: vec![tomb],
                ..Default::default()
            }],
            ..Default::default()
        });
        second.highest_file_uid = 5;
        let (p1, s1) = spool(&first);
        let (p2, s2) = spool(&second);
        let pool = Uuid::new_v4();
        let mut merge = ImportReconciler::new(vec![
            ImportVolume {
                pool_id: pool,
                barcode: "FIRSTL8".into(),
                volume_id: first.volume_uuid,
                generation: first.generation,
                spool: &s1,
            },
            ImportVolume {
                pool_id: pool,
                barcode: "SECONDL8".into(),
                volume_id: second.volume_uuid,
                generation: second.generation,
                spool: &s2,
            },
        ])
        .unwrap();
        let a = merge.next_path().unwrap().unwrap();
        assert_eq!(a.logical_path, "/a");
        assert_eq!(a.sources.len(), 2);
        assert_eq!(a.current().barcode, "SECONDL8");
        assert!(!a.current().candidate.deleted);
        let old = merge.next_path().unwrap().unwrap();
        assert_eq!(old.logical_path, "/old");
        assert_eq!(old.current().barcode, "SECONDL8");
        assert!(old.current().candidate.deleted);
        assert!(merge.next_path().unwrap().is_none());
        drop(merge);
        let snapshot =
            std::env::temp_dir().join(format!("tape-rs-merge-audit-{}.db", Uuid::new_v4()));
        let db = Connection::open(&snapshot).unwrap();
        db.execute_batch(
            "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
             CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
               volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL);
             CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
               generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
               ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
               metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
             CREATE TABLE staging(barcode TEXT NOT NULL);
             CREATE TABLE file_tasks(status TEXT NOT NULL);
             INSERT INTO meta(k,v) VALUES('applied_index',73),('catalog_frozen_index',70);",
        )
        .unwrap();
        for (index, barcode) in [(&first, "FIRSTL8"), (&second, "SECONDL8")] {
            db.execute(
                "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    barcode,
                    pool.to_string(),
                    index.volume_uuid.to_string(),
                    index.generation as i64,
                ],
            )
            .unwrap();
        }
        let mut winners = std::collections::BTreeMap::new();
        for (index, barcode) in [(&first, "FIRSTL8"), (&second, "SECONDL8")] {
            for record in crate::daemon::files::catalog_of(index).0 {
                if winners.get(&record.path).is_none_or(
                    |(_, old): &(&str, crate::daemon::state::FileRec)| old.version < record.version,
                ) {
                    winners.insert(record.path.clone(), (barcode, record));
                }
            }
        }
        for (_, (barcode, record)) in winners {
            db.execute(
                "INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![
                    pool.to_string(), record.path, barcode, first.generation as i64,
                    record.length as i64, record.sha256, record.version.0 as i64,
                    record.version.1 as i64, i64::from(record.deleted),
                    record.metadata.to_string(),
                ],
            )
            .unwrap();
        }
        let audit = ImportReconciler::new(vec![
            ImportVolume {
                pool_id: pool,
                barcode: "FIRSTL8".into(),
                volume_id: first.volume_uuid,
                generation: first.generation,
                spool: &s1,
            },
            ImportVolume {
                pool_id: pool,
                barcode: "SECONDL8".into(),
                volume_id: second.volume_uuid,
                generation: second.generation,
                spool: &s2,
            },
        ])
        .unwrap()
        .audit_directory_snapshot(&snapshot)
        .unwrap();
        assert_eq!((audit.paths, audit.tombstones), (2, 1));
        drop(db);
        drop(s1);
        drop(s2);
        std::fs::remove_file(p1).unwrap();
        std::fs::remove_file(p2).unwrap();
        std::fs::remove_file(snapshot).unwrap();
    }

    #[test]
    fn tied_current_versions_fail_closed_but_tied_old_versions_do_not() {
        let indexes: Vec<_> = (0..3)
            .map(|n| {
                let mut index = LtfsIndex::empty(Uuid::new_v4(), "merge-tie".into(), 'b');
                index.self_location.start_block = 5;
                index
                    .root
                    .files
                    .push(file("same", 2, if n == 2 { "2.1" } else { "1.1" }));
                index.highest_file_uid = 2;
                index
            })
            .collect();
        let pairs: Vec<_> = indexes.iter().map(spool).collect();
        let pool = Uuid::new_v4();
        {
            let make = |count: usize| {
                ImportReconciler::new(
                    pairs[..count]
                        .iter()
                        .enumerate()
                        .map(|(n, (_, spool))| ImportVolume {
                            pool_id: pool,
                            barcode: format!("TIE{n}L8"),
                            volume_id: indexes[n].volume_uuid,
                            generation: indexes[n].generation,
                            spool,
                        })
                        .collect(),
                )
                .unwrap()
            };
            assert!(make(2).next_path().is_err());
            let current = make(3).next_path().unwrap().unwrap();
            assert_eq!(current.current().barcode, "TIE2L8");
        }
        let paths: Vec<_> = pairs.iter().map(|(path, _)| path.clone()).collect();
        drop(pairs);
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn offline_directory_snapshot_must_match_media_and_cover_every_tape() {
        use rusqlite::params;
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "audit-source".into(), 'b');
        index.self_location.start_block = 5;
        index.root.files.push(file("a", 2, "4.1"));
        index.root.subdirs.push(DirectoryNode {
            name: "d".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            xattrs: vec![Xattr {
                key: XATTR_VERSION.into(),
                value: "4.2".into(),
                base64: false,
            }],
            ..Default::default()
        });
        index.highest_file_uid = 3;
        let (spool_path, spool) = spool(&index);
        let snapshot =
            std::env::temp_dir().join(format!("tape-rs-audit-snapshot-{}.db", Uuid::new_v4()));
        let pool = Uuid::new_v4();
        let barcode = "AUDIT01L8";
        let db = Connection::open(&snapshot).unwrap();
        db.execute_batch(
            "CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
             CREATE TABLE tapes(barcode TEXT PRIMARY KEY,pool_uuid TEXT NOT NULL,
               volume_uuid TEXT NOT NULL,generation INTEGER NOT NULL);
             CREATE TABLE files(pool_uuid TEXT NOT NULL,path TEXT NOT NULL,barcode TEXT NOT NULL,
               generation INTEGER NOT NULL,length INTEGER NOT NULL,sha256 TEXT NOT NULL,
               ver_round INTEGER NOT NULL,ver_seq INTEGER NOT NULL,deleted INTEGER NOT NULL,
               metadata TEXT NOT NULL,PRIMARY KEY(pool_uuid,path));
             CREATE TABLE staging(barcode TEXT NOT NULL);
             CREATE TABLE file_tasks(status TEXT NOT NULL);
             INSERT INTO meta(k,v) VALUES('applied_index',42),('catalog_frozen_index',40);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO tapes(barcode,pool_uuid,volume_uuid,generation) VALUES(?1,?2,?3,?4)",
            params![
                barcode,
                pool.to_string(),
                index.volume_uuid.to_string(),
                index.generation as i64
            ],
        )
        .unwrap();
        let (records, _) = crate::daemon::files::catalog_of(&index);
        assert_eq!(records.len(), 2);
        for record in &records {
            db.execute(
                "INSERT INTO files(pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    pool.to_string(), record.path, barcode, index.generation as i64,
                    record.length as i64, record.sha256, record.version.0 as i64,
                    record.version.1 as i64, i64::from(record.deleted),
                    record.metadata.to_string(),
                ],
            )
            .unwrap();
        }
        let source = || ImportVolume {
            pool_id: pool,
            barcode: barcode.into(),
            volume_id: index.volume_uuid,
            generation: index.generation,
            spool: &spool,
        };
        let audit = ImportReconciler::new(vec![source()])
            .unwrap()
            .audit_directory_snapshot(&snapshot)
            .unwrap();
        assert_eq!(
            (
                audit.applied_index,
                audit.frozen_index,
                audit.paths,
                audit.tombstones,
            ),
            (42, 40, 2, 0)
        );
        assert_ne!(audit.rows_sha256, [0; 32]);
        assert_ne!(audit.snapshot_sha256, [0; 32]);
        for frozen in [-1, 0, 43] {
            db.execute(
                "UPDATE meta SET v=?1 WHERE k='catalog_frozen_index'",
                [frozen],
            )
            .unwrap();
            assert!(
                ImportReconciler::new(vec![source()])
                    .unwrap()
                    .audit_directory_snapshot(&snapshot)
                    .is_err()
            );
        }
        db.execute("DELETE FROM meta WHERE k='catalog_frozen_index'", [])
            .unwrap();
        assert!(
            ImportReconciler::new(vec![source()])
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        db.execute(
            "INSERT INTO meta(k,v) VALUES('catalog_frozen_index',40)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO staging(barcode) VALUES(?1)", [barcode])
            .unwrap();
        assert!(
            ImportReconciler::new(vec![source()])
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        db.execute("DELETE FROM staging", []).unwrap();
        db.execute("INSERT INTO file_tasks(status) VALUES('staged')", [])
            .unwrap();
        for status in ["staged", "indeterminate"] {
            db.execute("UPDATE file_tasks SET status=?1", [status])
                .unwrap();
            assert!(
                ImportReconciler::new(vec![source()])
                    .unwrap()
                    .audit_directory_snapshot(&snapshot)
                    .is_err()
            );
        }
        db.execute("UPDATE file_tasks SET status='committed'", [])
            .unwrap();
        let settled = ImportReconciler::new(vec![source()])
            .unwrap()
            .audit_directory_snapshot(&snapshot)
            .unwrap();
        assert_eq!(settled.rows_sha256, audit.rows_sha256);
        assert_ne!(settled.snapshot_sha256, audit.snapshot_sha256);
        db.execute("DROP TABLE staging", []).unwrap();
        assert!(
            ImportReconciler::new(vec![source()])
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        db.execute("CREATE TABLE staging(barcode TEXT NOT NULL)", [])
            .unwrap();
        let mut changed_root = index.clone();
        changed_root.root.meta.access_time = "2026-10-24T00:00:00.000000000Z".into();
        let (changed_path, changed_spool) = self::spool(&changed_root);
        let changed_source = ImportVolume {
            pool_id: pool,
            barcode: barcode.into(),
            volume_id: index.volume_uuid,
            generation: index.generation,
            spool: &changed_spool,
        };
        let changed_audit = ImportReconciler::new(vec![changed_source])
            .unwrap()
            .audit_directory_snapshot(&snapshot)
            .unwrap();
        assert_ne!(audit.rows_sha256, changed_audit.rows_sha256);
        drop(changed_spool);
        std::fs::remove_file(changed_path).unwrap();
        db.execute("UPDATE files SET metadata='null' WHERE path='/a'", [])
            .unwrap();
        assert!(
            ImportReconciler::new(vec![source()])
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        db.execute("DELETE FROM tapes WHERE barcode=?1", [barcode])
            .unwrap();
        assert!(
            ImportReconciler::new(vec![source()])
                .unwrap()
                .audit_directory_snapshot(&snapshot)
                .is_err()
        );
        drop(db);
        drop(spool);
        std::fs::remove_file(spool_path).unwrap();
        std::fs::remove_file(snapshot).unwrap();
    }
}
