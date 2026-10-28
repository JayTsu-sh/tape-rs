//! 可重建的单卷物理 extent 缓存；文件名与元数据的共享权威不在这里。
//! 只接受完整 LTFS 索引，按索引摘要与卷身份隔离代数。

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::ltfs::index::{Extent, IndexEntry, IndexLocation, LtfsIndex};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::{IndexCandidate, RecoveryBudget, confirmed_index_reader};
use crate::tape::commands::TapeDrive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedIndex {
    pub volume_uuid: Uuid,
    pub generation: u64,
    pub location: IndexLocation,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub path: String,
    pub length: u64,
    pub symlink: Option<String>,
    pub extents: Vec<Extent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub files: u64,
    pub extents: u64,
}

pub struct VolumeExtentCache {
    conn: Connection,
}

#[cfg(feature = "fdb")]
struct VerifiedBuild<'a> {
    incremental: bool,
    require_latest: bool,
    storage: Option<&'a mut crate::local_read_storage::CacheBuild>,
}

impl VolumeExtentCache {
    /// 在恢复协议已确认的物理索引位置重读原文；不修改介质，且 EOF/摘要必须一致。
    /// 会 LOCATE 并移动驱动器当前读取位置；调用方必须持有设备使用资格，
    /// 且不得与当前写入会话或其他驱动器操作并发。
    pub fn build_confirmed(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        candidate: &IndexCandidate,
        budget: &RecoveryBudget,
        path: &Path,
    ) -> Result<CacheStats> {
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

    /// Re-read one recovery-confirmed incremental construct from the medium.
    /// The cache contains only complete changed-file descriptions; it never
    /// reconstructs a full billion-row view from the delta XML.
    pub fn build_confirmed_incremental(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        candidate: &IndexCandidate,
        budget: &RecoveryBudget,
        path: &Path,
    ) -> Result<CacheStats> {
        if !candidate.index.incremental {
            return Err(TapeError::Catalog("卷索引不是增量构造".into()));
        }
        let expected = ExpectedIndex {
            volume_uuid: candidate.index.volume_uuid,
            generation: candidate.index.generation,
            location: candidate.index.self_location,
            sha256: candidate.construct_sha256,
        };
        let mut reader = confirmed_index_reader(drive, label, candidate, budget)?;
        match Self::build_incremental(&mut reader, path, expected) {
            Ok(stats) => Ok(stats),
            Err(error) => Err(reader.take_device_error().unwrap_or(error)),
        }
    }

    /// Build a disposable cache for an FDB-recorded DP construct, including a
    /// historical generation. This does not prove that it is the latest tape
    /// index; an owner must prove the latest head before serving reads.
    /// The final path appears only after the complete on-media construct,
    /// closing filemark, identity and SHA-256 have all been verified.
    /// The caller must hold the current library owner permission and drive PR,
    /// exclude concurrent tape operations, and recheck ownership before using
    /// the result as a read plan. This function moves the read head.
    #[cfg(feature = "fdb")]
    pub fn build_verified_from_media(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        budget: &RecoveryBudget,
        path: &Path,
        expected: ExpectedIndex,
        incremental: bool,
    ) -> Result<CacheStats> {
        Self::build_verified_from_media_inner(
            drive,
            label,
            budget,
            path,
            expected,
            VerifiedBuild {
                incremental,
                require_latest: false,
                storage: None,
            },
        )
    }

    /// Prove that this exact FDB index is also the current DP tail, then
    /// publish its cache. A later or unindexed on-media tail is rejected.
    #[cfg(feature = "fdb")]
    pub fn build_verified_latest_from_media(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        budget: &RecoveryBudget,
        path: &Path,
        expected: ExpectedIndex,
        incremental: bool,
    ) -> Result<CacheStats> {
        Self::build_verified_from_media_inner(
            drive,
            label,
            budget,
            path,
            expected,
            VerifiedBuild {
                incremental,
                require_latest: true,
                storage: None,
            },
        )
    }

    #[cfg(feature = "fdb")]
    pub(crate) fn build_managed_from_media(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        budget: &RecoveryBudget,
        path: &Path,
        expected: ExpectedIndex,
        incremental: bool,
        storage: &mut crate::local_read_storage::CacheBuild,
    ) -> Result<CacheStats> {
        Self::build_verified_from_media_inner(
            drive,
            label,
            budget,
            path,
            expected,
            VerifiedBuild {
                incremental,
                require_latest: false,
                storage: Some(storage),
            },
        )
    }

    #[cfg(feature = "fdb")]
    fn build_verified_from_media_inner(
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        budget: &RecoveryBudget,
        path: &Path,
        expected: ExpectedIndex,
        mut options: VerifiedBuild<'_>,
    ) -> Result<CacheStats> {
        if label.volume_uuid != expected.volume_uuid {
            return Err(TapeError::Catalog("磁带标签与 FDB 卷身份不一致".into()));
        }
        if path.exists() {
            return Err(TapeError::Catalog(format!(
                "卷 extent 缓存已存在: {}",
                path.display()
            )));
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let provisional = parent.join(format!(".extent-verifying-{}.db", Uuid::new_v4()));
        let cleanup = RemoveOnDrop(provisional.clone());
        let stats = crate::ltfs::recovery::with_verified_dp_construct(
            drive,
            label,
            expected.location,
            expected.sha256,
            budget,
            options.require_latest,
            |reader| {
                if let Some(storage) = options.storage.as_mut() {
                    Self::build_with_admission(
                        reader,
                        &provisional,
                        expected,
                        options.incremental,
                        true,
                        &mut |conn, path, bytes| storage.admit(conn, path, bytes),
                    )
                } else {
                    Self::build_with_mode(reader, &provisional, expected, options.incremental)
                }
            },
        )?;
        fs::hard_link(&provisional, path)?;
        File::open(parent)?.sync_all()?;
        drop(cleanup);
        Ok(stats)
    }

    /// 将完整索引建立成单文件 SQLite 缓存；只有全部解析、身份和摘要核对成功后
    /// 才在 `path` 原子出现。目标文件已存在时拒绝覆盖。
    pub fn build<R: Read>(input: R, path: &Path, expected: ExpectedIndex) -> Result<CacheStats> {
        Self::build_with_mode(input, path, expected, false)
    }

    /// Store only files whose UID, length and physical description are
    /// explicit in a verified incremental construct. Missing/partial entries
    /// cannot be used as read plans and must be recovered from another cache.
    pub fn build_incremental<R: Read>(
        input: R,
        path: &Path,
        expected: ExpectedIndex,
    ) -> Result<CacheStats> {
        Self::build_with_mode(input, path, expected, true)
    }

    fn build_with_mode<R: Read>(
        input: R,
        path: &Path,
        expected: ExpectedIndex,
        incremental: bool,
    ) -> Result<CacheStats> {
        Self::build_with_admission(input, path, expected, incremental, false, &mut |_, _, _| {
            Ok(())
        })
    }

    fn build_with_admission<R: Read>(
        input: R,
        path: &Path,
        expected: ExpectedIndex,
        incremental: bool,
        disposable: bool,
        admit: &mut dyn FnMut(&Connection, &Path, usize) -> Result<()>,
    ) -> Result<CacheStats> {
        if path.exists() {
            return Err(TapeError::Catalog(format!(
                "卷 extent 缓存已存在: {}",
                path.display()
            )));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(".extent-building-{}.db", Uuid::new_v4()));
        let cleanup = RemoveOnDrop(temp.clone());
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let conn = Connection::open(&temp)?;
        if disposable {
            conn.execute_batch(
                "PRAGMA page_size=4096; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
                PRAGMA temp_store=FILE; PRAGMA cache_size=-8192; PRAGMA mmap_size=0;",
            )?;
        } else {
            conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        }
        admit(&conn, &temp, 0)?;
        conn.execute_batch(&format!(
            "PRAGMA foreign_keys=ON;
             PRAGMA user_version={};
             CREATE TABLE manifest (
               id INTEGER PRIMARY KEY CHECK (id=1),
               volume_uuid TEXT NOT NULL, generation TEXT NOT NULL,
               partition TEXT NOT NULL, start_block TEXT NOT NULL,
               sha256 BLOB NOT NULL, files TEXT NOT NULL, extents TEXT NOT NULL
             );
             CREATE TABLE files (
               uid TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
               length TEXT NOT NULL, symlink TEXT, proof BLOB NOT NULL
             );
             CREATE TABLE extents (
               uid TEXT NOT NULL REFERENCES files(uid), seq INTEGER NOT NULL,
               partition TEXT NOT NULL, start_block TEXT NOT NULL,
               byte_offset TEXT NOT NULL, byte_count TEXT NOT NULL,
               file_offset TEXT NOT NULL,
               PRIMARY KEY (uid,seq)
             );
             BEGIN IMMEDIATE;",
            if incremental { 2 } else { 1 }
        ))?;

        if disposable && incremental {
            conn.execute_batch("CREATE TABLE seen(path BLOB PRIMARY KEY) WITHOUT ROWID;")?;
        }
        let mut stats = CacheStats {
            files: 0,
            extents: 0,
        };
        let mut hash_input = HashingRead {
            inner: input,
            sha: Sha256::new(),
        };
        let mut save_entry = |entry: IndexEntry| {
            if disposable && incremental {
                let path = match &entry {
                    IndexEntry::File { path, .. } | IndexEntry::Directory { path, .. } => path,
                };
                admit(&conn, &temp, path.len() + 64)?;
                conn.execute("INSERT INTO seen VALUES(?1)", [path.as_bytes()])?;
            }
            if let IndexEntry::File { path, node } = entry {
                if incremental
                    && node.delta_fields.as_ref().is_some_and(|fields| {
                        fields.contains("deleted")
                            || !fields.contains("fileuid")
                            || !fields.contains("length")
                            || !(fields.contains("extentinfo") || fields.contains("symlink"))
                    })
                {
                    return Ok(());
                }
                validate_path(&path)?;
                if node.meta.file_uid == 0 || node.name.is_empty() {
                    return Err(TapeError::Catalog("索引文件 UID 或名称无效".into()));
                }
                admit(
                    &conn,
                    &temp,
                    path.len() + node.symlink.as_ref().map_or(0, String::len) + 128,
                )?;
                let uid = node.meta.file_uid.to_string();
                let proof = file_proof(
                    &path,
                    node.meta.file_uid,
                    node.length,
                    node.symlink.as_deref(),
                    &node.extents,
                );
                conn.execute(
                    "INSERT INTO files(uid,path,length,symlink,proof) VALUES (?1,?2,?3,?4,?5)",
                    params![uid, path, node.length.to_string(), node.symlink, &proof[..]],
                )?;
                for (seq, ext) in node.extents.iter().enumerate() {
                    admit(&conn, &temp, 256)?;
                    conn.execute(
                        "INSERT INTO extents(uid,seq,partition,start_block,byte_offset,byte_count,file_offset)
                         VALUES (?1,?2,?3,?4,?5,?6,?7)",
                        params![
                            uid,
                            i64::try_from(seq).map_err(|_| TapeError::Catalog("extent 数量溢出".into()))?,
                            ext.partition.to_string(),
                            ext.start_block.to_string(),
                            ext.byte_offset.to_string(),
                            ext.byte_count.to_string(),
                            ext.file_offset.to_string(),
                        ],
                    )?;
                    stats.extents = stats
                        .extents
                        .checked_add(1)
                        .ok_or_else(|| TapeError::Catalog("extent 计数溢出".into()))?;
                }
                stats.files = stats
                    .files
                    .checked_add(1)
                    .ok_or_else(|| TapeError::Catalog("文件计数溢出".into()))?;
                if stats.files.is_multiple_of(1000) {
                    conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
                }
            }
            Ok(())
        };
        let index = if incremental {
            #[cfg(feature = "fdb")]
            {
                if disposable {
                    LtfsIndex::scan_incremental_reader_to_disk(
                        BufReader::new(&mut hash_input),
                        &mut save_entry,
                    )?
                } else {
                    LtfsIndex::scan_incremental_reader(
                        BufReader::new(&mut hash_input),
                        &mut save_entry,
                    )?
                }
            }
            #[cfg(not(feature = "fdb"))]
            LtfsIndex::scan_incremental_reader(BufReader::new(&mut hash_input), &mut save_entry)?
        } else {
            LtfsIndex::scan_full_reader(BufReader::new(&mut hash_input), &mut save_entry)?
        };
        let digest: [u8; 32] = hash_input.sha.finalize().into();
        if index.volume_uuid != expected.volume_uuid
            || index.generation != expected.generation
            || index.self_location != expected.location
            || index.incremental != incremental
            || digest != expected.sha256
        {
            return Err(TapeError::Catalog("卷索引身份或 SHA-256 与预期不符".into()));
        }
        if disposable && incremental {
            conn.execute_batch("DROP TABLE seen;")?;
        }
        admit(&conn, &temp, 512)?;
        conn.execute(
            "INSERT INTO manifest(id,volume_uuid,generation,partition,start_block,sha256,files,extents)
             VALUES (1,?1,?2,?3,?4,?5,?6,?7)",
            params![
                expected.volume_uuid.to_string(),
                expected.generation.to_string(),
                expected.location.partition.to_string(),
                expected.location.start_block.to_string(),
                &expected.sha256[..],
                stats.files.to_string(),
                stats.extents.to_string(),
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

    /// 打开时再次核对卷身份；不能将上一代或另一盘的物理位置当成当前索引。
    pub fn open(path: &Path, expected: ExpectedIndex) -> Result<Self> {
        Self::open_with_mode(path, expected, false)
    }

    pub fn open_incremental(path: &Path, expected: ExpectedIndex) -> Result<Self> {
        Self::open_with_mode(path, expected, true)
    }

    fn open_with_mode(path: &Path, expected: ExpectedIndex, incremental: bool) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != if incremental { 2 } else { 1 } {
            return Err(TapeError::Catalog(format!(
                "卷 extent 缓存版本 {version} 不受支持"
            )));
        }
        let row: Option<(String, String, String, String, Vec<u8>)> = conn
            .query_row(
                "SELECT volume_uuid,generation,partition,start_block,sha256 FROM manifest WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let Some((volume, generation, partition, block, digest)) = row else {
            return Err(TapeError::Catalog("卷 extent 缓存未完成".into()));
        };
        if volume != expected.volume_uuid.to_string()
            || generation != expected.generation.to_string()
            || partition != expected.location.partition.to_string()
            || block != expected.location.start_block.to_string()
            || digest != expected.sha256
        {
            return Err(TapeError::Catalog("卷 extent 缓存身份不匹配".into()));
        }
        Ok(Self { conn })
    }

    pub fn file_by_uid(&self, uid: u64) -> Result<Option<CachedFile>> {
        let uid = uid.to_string();
        let row: Option<(String, String, Option<String>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT path,length,symlink,proof FROM files WHERE uid=?1",
                [&uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((path, length, symlink, proof)) = row else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT partition,start_block,byte_offset,byte_count,file_offset
             FROM extents WHERE uid=?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([&uid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut extents = Vec::new();
        for row in rows {
            let (partition, block, offset, count, file_offset) = row?;
            let partition = partition
                .chars()
                .next()
                .filter(|_| partition.len() == 1)
                .ok_or_else(|| TapeError::Catalog("extent 分区字段损坏".into()))?;
            extents.push(Extent {
                partition,
                start_block: parse_u64(&block)?,
                byte_offset: parse_u64(&offset)?,
                byte_count: parse_u64(&count)?,
                file_offset: parse_u64(&file_offset)?,
            });
        }
        let length = parse_u64(&length)?;
        if proof
            != file_proof(
                &path,
                parse_u64(&uid)?,
                length,
                symlink.as_deref(),
                &extents,
            )
        {
            return Err(TapeError::Catalog("卷 extent 缓存文件证明不匹配".into()));
        }
        Ok(Some(CachedFile {
            path,
            length,
            symlink,
            extents,
        }))
    }

    /// Merge the two existing UID indexes without per-file SQL queries or an
    /// in-memory file map. UID order is SQLite TEXT order, not numeric order.
    /// Each callback sees one fully proof-checked file, including its extents;
    /// successful earlier callbacks remain provisional if a later row fails.
    #[cfg(feature = "fdb")]
    pub(crate) fn visit_files_by_uid(
        &self,
        mut visit: impl FnMut(u64, &CachedFile) -> Result<()>,
    ) -> Result<u64> {
        let mut files = self
            .conn
            .prepare("SELECT uid,path,length,symlink,proof FROM files ORDER BY uid")?;
        let mut extents = self.conn.prepare(
            "SELECT uid,partition,start_block,byte_offset,byte_count,file_offset
             FROM extents ORDER BY uid,seq",
        )?;
        let mut file_rows = files.query([])?;
        let mut extent_rows = extents.query([])?;
        let mut next_extent = extent_rows.next()?;
        let mut count = 0;
        while let Some(row) = file_rows.next()? {
            let uid_text: String = row.get(0)?;
            let uid = parse_u64(&uid_text)?;
            if uid == 0 || uid.to_string() != uid_text {
                return Err(TapeError::Catalog("卷 extent 缓存 UID 损坏".into()));
            }
            let mut file = CachedFile {
                path: row.get(1)?,
                length: parse_u64(&row.get::<_, String>(2)?)?,
                symlink: row.get(3)?,
                extents: Vec::new(),
            };
            while let Some(row) = next_extent {
                let extent_uid: String = row.get(0)?;
                match extent_uid.cmp(&uid_text) {
                    std::cmp::Ordering::Less => {
                        return Err(TapeError::Catalog("卷 extent 缓存存在孤立 extent".into()));
                    }
                    std::cmp::Ordering::Greater => break,
                    std::cmp::Ordering::Equal => {
                        let partition: String = row.get(1)?;
                        let partition =
                            partition
                                .chars()
                                .next()
                                .filter(|_| partition.len() == 1)
                                .ok_or_else(|| TapeError::Catalog("extent 分区字段损坏".into()))?;
                        file.extents.push(Extent {
                            partition,
                            start_block: parse_u64(&row.get::<_, String>(2)?)?,
                            byte_offset: parse_u64(&row.get::<_, String>(3)?)?,
                            byte_count: parse_u64(&row.get::<_, String>(4)?)?,
                            file_offset: parse_u64(&row.get::<_, String>(5)?)?,
                        });
                        next_extent = extent_rows.next()?;
                    }
                }
            }
            let proof: Vec<u8> = row.get(4)?;
            if proof
                != file_proof(
                    &file.path,
                    uid,
                    file.length,
                    file.symlink.as_deref(),
                    &file.extents,
                )
            {
                return Err(TapeError::Catalog("卷 extent 缓存文件证明不匹配".into()));
            }
            visit(uid, &file)?;
            count += 1;
        }
        if next_extent.is_some() {
            return Err(TapeError::Catalog("卷 extent 缓存存在孤立 extent".into()));
        }
        Ok(count)
    }

    /// Used when binding a full-index import inventory to this physical cache.
    /// A count check also rejects extra cached files absent from that inventory.
    pub fn file_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|_| TapeError::Catalog("卷 extent 缓存文件数损坏".into()))
    }

    /// 已由共享目录确认的文件必须在同代物理缓存中存在；缺失是缓存损坏，不能当 404。
    pub fn required_file_by_uid(&self, uid: u64) -> Result<CachedFile> {
        self.file_by_uid(uid)?
            .ok_or_else(|| TapeError::Catalog(format!("卷 extent 缓存缺少已确认 UID {uid}")))
    }

    /// Resolve a path through the SQLite path index, then verify the UID row's
    /// immutable file proof before returning any extents. This is a bounded
    /// lookup even when a tape contains millions of entries.
    pub fn file_by_path(&self, path: &str) -> Result<Option<(u64, CachedFile)>> {
        validate_path(path)?;
        let uid: Option<String> = self
            .conn
            .query_row("SELECT uid FROM files WHERE path=?1", [path], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(uid) = uid else {
            return Ok(None);
        };
        let uid = parse_u64(&uid)?;
        if uid == 0 {
            return Err(TapeError::Catalog("卷 extent 缓存 UID 无效".into()));
        }
        let file = self.required_file_by_uid(uid)?;
        if file.path != path {
            return Err(TapeError::Catalog("卷 extent 缓存路径索引不一致".into()));
        }
        Ok(Some((uid, file)))
    }

    /// Resolve a user path to the one on-tape data file it reads. IBM EE uses
    /// a symlink into `.LTFSEE_DATA`; following exactly one in-volume link
    /// matches the LTFS read path without accepting loops or root escapes.
    pub fn resolve_data_file(&self, path: &str) -> Result<(u64, CachedFile)> {
        let (uid, entry) = self
            .file_by_path(path)?
            .ok_or_else(|| TapeError::Catalog(format!("卷 extent 缓存缺少路径 {path}")))?;
        let (uid, file) = if let Some(target) = entry.symlink.as_deref() {
            let resolved = crate::ltfs::volume::resolve_symlink(path, target)
                .ok_or_else(|| TapeError::Catalog("卷内符号链接越出根目录".into()))?;
            self.file_by_path(&resolved)?
                .ok_or_else(|| TapeError::Catalog("卷内符号链接目标缺失".into()))?
        } else {
            (uid, entry)
        };
        if file.symlink.is_some() {
            return Err(TapeError::Catalog("卷内符号链接目标仍是符号链接".into()));
        }
        if file.length != 0 && file.extents.is_empty() {
            return Err(TapeError::Catalog("非空文件缺少物理 extent".into()));
        }
        Ok((uid, file))
    }

    /// Read one file from an already verified cache without constructing a
    /// volume directory tree. This checks label/cache UUID and extent bounds;
    /// it does not prove current media generation, owner authority or digest.
    /// Caller must prove those separately, serialize device use and enforce
    /// PR/execution permission on every command, then validate output before
    /// publishing it. One in-volume symlink is resolved as in normal LTFS reads.
    #[cfg(feature = "fdb")]
    pub fn read_file_to_writer<W: std::io::Write>(
        &self,
        device: &dyn crate::scsi::transport::TapeTransport,
        label: &LtfsLabel,
        path: &str,
        output: &mut W,
    ) -> Result<u64> {
        let volume: String =
            self.conn
                .query_row("SELECT volume_uuid FROM manifest WHERE id=1", [], |row| {
                    row.get(0)
                })?;
        if volume != label.volume_uuid.to_string() {
            return Err(TapeError::Catalog(
                "读带标签与 extent 缓存卷身份不符".into(),
            ));
        }
        let (_, file) = self.resolve_data_file(path)?;
        crate::ltfs::volume::read_cached_file_to_writer(device, label, &file, output)
    }
}

fn parse_u64(value: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| TapeError::Catalog("卷 extent 缓存数值损坏".into()))
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.len() > 255
                || part.contains('\0')
        })
    {
        return Err(TapeError::Catalog("卷索引文件路径不安全".into()));
    }
    Ok(())
}

fn file_proof(
    path: &str,
    uid: u64,
    length: u64,
    symlink: Option<&str>,
    extents: &[Extent],
) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(b"tape-rs/extent-cache-file/1");
    sha.update(uid.to_be_bytes());
    sha.update(length.to_be_bytes());
    proof_text(&mut sha, path);
    sha.update([u8::from(symlink.is_some())]);
    if let Some(target) = symlink {
        proof_text(&mut sha, target);
    }
    sha.update((extents.len() as u64).to_be_bytes());
    for ext in extents {
        sha.update((ext.partition as u32).to_be_bytes());
        for value in [
            ext.start_block,
            ext.byte_offset,
            ext.byte_count,
            ext.file_offset,
        ] {
            sha.update(value.to_be_bytes());
        }
    }
    sha.finalize().into()
}

fn proof_text(sha: &mut Sha256, value: &str) {
    sha.update((value.len() as u64).to_be_bytes());
    sha.update(value.as_bytes());
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
    use crate::ltfs::index::{FileNode, NodeMeta};
    use std::io::{BufWriter, Write};

    fn fixture() -> (LtfsIndex, ExpectedIndex) {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "cache-test".into(), 'b');
        index.root.files.push(FileNode {
            name: "海量.txt".into(),
            length: u64::MAX,
            meta: NodeMeta {
                file_uid: u64::MAX,
                ..Default::default()
            },
            extents: vec![Extent {
                partition: 'b',
                start_block: u64::MAX,
                byte_offset: 3,
                byte_count: 5,
                file_offset: 7,
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
        (index, expected)
    }

    fn cache_path() -> PathBuf {
        std::env::temp_dir().join(format!("tape-rs-volume-extents-{}.db", Uuid::new_v4()))
    }

    #[cfg(feature = "fdb")]
    #[test]
    fn managed_cache_budget_covers_build_and_delta_path_deduplication() {
        use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
        for incremental in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("tape-managed-cache-{}", Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            let path = directory.join("cache.db");
            let (mut index, _) = fixture();
            index.incremental = incremental;
            index.version = "2.5.0".into();
            index.generation = 2;
            index.self_location.start_block = 9;
            if incremental {
                index.previous_location = Some(IndexLocation {
                    partition: 'b',
                    start_block: 5,
                });
            }
            let xml = index.to_xml().unwrap();
            let expected = ExpectedIndex {
                volume_uuid: index.volume_uuid,
                generation: index.generation,
                location: index.self_location,
                sha256: Sha256::digest(&xml).into(),
            };
            let budget = ReadStorageBudget {
                cache_bytes: 1 << 20,
                headroom_bytes: 0,
                ..Default::default()
            };
            let mut storage = CacheBuild::new(&directory, budget).unwrap();
            VolumeExtentCache::build_with_admission(
                xml.as_ref(),
                &path,
                expected,
                incremental,
                true,
                &mut |conn, path, bytes| storage.admit(conn, path, bytes),
            )
            .unwrap();
            storage.finish().unwrap();
            let cache = VolumeExtentCache::open_with_mode(&path, expected, incremental).unwrap();
            assert!(cache.required_file_by_uid(u64::MAX).is_ok());
            let seen: bool = cache
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='seen')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!seen, "dedup state must not change the final schema");
            drop(cache);
            drop(storage);
            fs::remove_dir_all(directory).unwrap();
        }
        let directory = std::env::temp_dir().join(format!("tape-managed-cache-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("cache.db");
        let (mut index, _) = fixture();
        index.incremental = true;
        index.version = "2.5.0".into();
        index.generation = 2;
        index.self_location.start_block = 9;
        index.previous_location = Some(IndexLocation {
            partition: 'b',
            start_block: 5,
        });
        index.root.files[0].delta_fields = Some(["name".to_string()].into());
        index.root.files.push(index.root.files[0].clone());
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let mut storage = CacheBuild::new(
            &directory,
            ReadStorageBudget {
                headroom_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            VolumeExtentCache::build_with_admission(
                xml.as_ref(),
                &path,
                expected,
                true,
                true,
                &mut |conn, path, bytes| storage.admit(conn, path, bytes)
            )
            .is_err()
        );
        assert!(
            !path.exists(),
            "duplicate partial entries cannot publish a cache"
        );
        drop(storage);
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            1,
            "only directory lock remains"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(feature = "fdb")]
    #[test]
    fn managed_cache_over_budget_never_publishes_partial_file() {
        use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
        let directory = std::env::temp_dir().join(format!("tape-managed-cache-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("cache.db");
        let (mut index, _) = fixture();
        let template = index.root.files[0].clone();
        index.root.files.clear();
        for uid in 2..1002 {
            let mut node = template.clone();
            node.name = format!("file-{uid}");
            node.meta.file_uid = uid;
            index.root.files.push(node);
        }
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let mut storage = CacheBuild::new(
            &directory,
            ReadStorageBudget {
                cache_bytes: 64 << 10,
                headroom_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            VolumeExtentCache::build_with_admission(
                xml.as_ref(),
                &path,
                expected,
                false,
                true,
                &mut |conn, path, bytes| storage.admit(conn, path, bytes)
            )
            .is_err()
        );
        assert!(!path.exists());
        drop(storage);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn full_index_builds_verified_cache_without_losing_u64_values() {
        let (index, expected) = fixture();
        let xml = index.to_xml().unwrap();
        let path = cache_path();
        assert_eq!(
            VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap(),
            CacheStats {
                files: 1,
                extents: 1
            }
        );
        assert!(VolumeExtentCache::build(xml.as_ref(), &path, expected).is_err());
        let cache = VolumeExtentCache::open(&path, expected).unwrap();
        assert_eq!(cache.file_by_uid(1).unwrap(), None);
        assert!(cache.required_file_by_uid(1).is_err());
        assert_eq!(
            cache.required_file_by_uid(u64::MAX).unwrap(),
            CachedFile {
                path: "海量.txt".into(),
                length: u64::MAX,
                symlink: None,
                extents: index.root.files[0].extents.clone(),
            }
        );
        drop(cache);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn incremental_cache_uses_exact_delta_identity_and_only_explicit_extents() {
        let (mut base, _) = fixture();
        base.root.files[0].name = "old".into();
        base.root.files[0].meta.file_uid = 2;
        base.root.files[0].length = 5;
        base.root.files[0].extents[0].file_offset = 0;
        base.root.files[0].extents[0].byte_count = 5;
        base.highest_file_uid = 2;
        base.self_location.start_block = 5;
        let mut next = base.clone();
        next.generation = 2;
        next.self_location.start_block = 9;
        next.highest_file_uid = 3;
        next.root.files.push(FileNode {
            name: "new".into(),
            length: 7,
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            extents: vec![Extent {
                partition: 'b',
                start_block: 8,
                byte_offset: 0,
                byte_count: 7,
                file_offset: 0,
            }],
            ..Default::default()
        });
        let delta = next.incremental_since(&base).unwrap();
        let xml = delta.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: next.volume_uuid,
            generation: next.generation,
            location: next.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = cache_path();
        assert_eq!(
            VolumeExtentCache::build_incremental(xml.as_ref(), &path, expected).unwrap(),
            CacheStats {
                files: 1,
                extents: 1
            }
        );
        assert!(VolumeExtentCache::open(&path, expected).is_err());
        let cache = VolumeExtentCache::open_incremental(&path, expected).unwrap();
        assert_eq!(cache.file_by_uid(2).unwrap(), None);
        assert_eq!(cache.resolve_data_file("new").unwrap().0, 3);
        drop(cache);
        fs::remove_file(path).unwrap();

        let mut partial = delta.clone();
        partial.root.files[0].delta_fields = Some(
            ["name", "fileuid", "length"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        let partial_xml = partial.to_xml().unwrap();
        let partial_expected = ExpectedIndex {
            sha256: Sha256::digest(&partial_xml).into(),
            ..expected
        };
        let path = cache_path();
        assert_eq!(
            VolumeExtentCache::build_incremental(partial_xml.as_ref(), &path, partial_expected)
                .unwrap()
                .files,
            0
        );
        assert!(
            VolumeExtentCache::open_incremental(&path, partial_expected)
                .unwrap()
                .file_by_uid(3)
                .unwrap()
                .is_none()
        );
        fs::remove_file(path).unwrap();
        let bad_path = cache_path();
        assert!(
            VolumeExtentCache::build_incremental(xml.as_ref(), &bad_path, partial_expected)
                .is_err()
        );
        assert!(!bad_path.exists());
        assert!(VolumeExtentCache::build(xml.as_ref(), &bad_path, expected).is_err());
        assert!(!bad_path.exists());
    }

    #[test]
    fn wrong_digest_and_incomplete_xml_never_publish_cache() {
        let (index, mut expected) = fixture();
        let xml = index.to_xml().unwrap();
        let path = cache_path();
        expected.sha256[0] ^= 1;
        assert!(VolumeExtentCache::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
        expected.sha256[0] ^= 1;
        assert!(VolumeExtentCache::build(&xml[..xml.len() - 20], &path, expected).is_err());
        assert!(!path.exists());
        let mut wrong_generation = expected;
        wrong_generation.generation += 1;
        assert!(VolumeExtentCache::build(xml.as_ref(), &path, wrong_generation).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn duplicate_uid_and_unsafe_path_are_rejected_before_publication() {
        let (mut index, _) = fixture();
        index.root.files.push(FileNode {
            name: "different.txt".into(),
            meta: NodeMeta {
                file_uid: u64::MAX,
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
        let path = cache_path();
        assert!(VolumeExtentCache::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
        index.root.files.pop();
        index.root.files[0].name = "../escape".into();
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            sha256: Sha256::digest(&xml).into(),
            ..expected
        };
        assert!(VolumeExtentCache::build(xml.as_ref(), &path, expected).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn changed_extent_is_detected_before_read_planning() {
        let (index, expected) = fixture();
        let xml = index.to_xml().unwrap();
        let path = cache_path();
        VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE extents SET start_block='1'", [])
            .unwrap();
        drop(db);
        let cache = VolumeExtentCache::open(&path, expected).unwrap();
        assert!(cache.file_by_uid(u64::MAX).is_err());
        drop(cache);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn real_ee_index_alias_resolves_to_proven_data_uid() {
        let xml = include_bytes!("../../tests/fixtures/ee-rc17-generation12.schema");
        let index = LtfsIndex::parse(xml).unwrap();
        let mut alias = None;
        index.walk_files(|path, file| {
            if file.symlink.is_some() && alias.is_none() {
                alias = Some(path.to_owned());
            }
        });
        let alias = alias.expect("真实 EE 索引含有用户别名");
        let target = crate::ltfs::volume::resolve_symlink(
            &alias,
            index.find_file(&alias).unwrap().symlink.as_deref().unwrap(),
        )
        .unwrap();
        let target_file = index.find_file(&target).unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(xml).into(),
        };
        let path = cache_path();
        VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap();
        let cache = VolumeExtentCache::open(&path, expected).unwrap();
        let (uid, file) = cache.resolve_data_file(&alias).unwrap();
        assert_eq!(uid, target_file.meta.file_uid);
        assert_eq!(file.path, target);
        assert_eq!(file.length, target_file.length);
        assert_eq!(file.extents, target_file.extents);
        assert_eq!(cache.resolve_data_file(&target).unwrap().0, uid);
        drop(cache);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn path_lookup_rejects_escaping_or_missing_link_targets() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "cache-link-test".into(), 'b');
        index.root.files.push(FileNode {
            name: "link".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            symlink: Some("../outside".into()),
            ..Default::default()
        });
        index.root.files.push(FileNode {
            name: "dangling".into(),
            meta: NodeMeta {
                file_uid: 3,
                ..Default::default()
            },
            symlink: Some("missing".into()),
            ..Default::default()
        });
        index.root.files.push(FileNode {
            name: "chain".into(),
            meta: NodeMeta {
                file_uid: 4,
                ..Default::default()
            },
            symlink: Some("dangling".into()),
            ..Default::default()
        });
        index.root.files.push(FileNode {
            name: "no-extent".into(),
            length: 1,
            meta: NodeMeta {
                file_uid: 5,
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
        let path = cache_path();
        VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap();
        let cache = VolumeExtentCache::open(&path, expected).unwrap();
        assert!(cache.resolve_data_file("link").is_err());
        assert!(cache.resolve_data_file("dangling").is_err());
        assert!(cache.resolve_data_file("chain").is_err());
        assert!(cache.resolve_data_file("no-extent").is_err());
        assert!(cache.resolve_data_file("missing").is_err());
        assert!(cache.file_by_path("../outside").is_err());
        drop(cache);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_cache_format_is_rejected() {
        let (index, expected) = fixture();
        let xml = index.to_xml().unwrap();
        let path = cache_path();
        VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap();
        let db = Connection::open(&path).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        drop(db);
        assert!(VolumeExtentCache::open(&path, expected).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn batch_boundaries_keep_all_files_and_last_manifest() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "batch-test".into(), 'b');
        for number in 0..2001 {
            index.root.files.push(FileNode {
                name: format!("f{number}"),
                meta: NodeMeta {
                    file_uid: number + 2,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        index.highest_file_uid = 2002;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = cache_path();
        let stats = VolumeExtentCache::build(xml.as_ref(), &path, expected).unwrap();
        assert_eq!(
            stats,
            CacheStats {
                files: 2001,
                extents: 0
            }
        );
        let cache = VolumeExtentCache::open(&path, expected).unwrap();
        assert_eq!(cache.file_by_uid(2002).unwrap().unwrap().path, "f2000");
        drop(cache);
        fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "百万文件 SQLite 构建压力测试；需显式运行并提供足够磁盘空间"]
    fn million_file_cache_build_is_bounded() {
        million_cache_build(false);
    }

    #[cfg(feature = "fdb")]
    #[test]
    #[ignore = "百万增量文件的受预算缓存构建和磁盘去重压测；需足够临时空间"]
    fn million_managed_delta_cache_uses_disk_deduplication() {
        million_cache_build(true);
    }

    fn million_cache_build(managed: bool) {
        const COUNT: u64 = 1_000_000;
        let dir = std::env::var_os("TAPE_RS_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let nonce = Uuid::new_v4();
        let dir = if managed {
            let private = dir.join(format!("tape-managed-million-{nonce}"));
            fs::create_dir(&private).unwrap();
            private
        } else {
            dir
        };
        let xml_path = dir.join(format!("tape-rs-volume-extents-{nonce}.xml"));
        let db_path = dir.join(format!("tape-rs-volume-extents-{nonce}.db"));
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "cache-million".into(), 'b');
        index.self_location.start_block = 5;
        if managed {
            index.version = "2.5.0".into();
            index.generation = 2;
            index.incremental = true;
            index.previous_location = Some(index.self_location);
            index.self_location.start_block = 9;
        }
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
                    "<file><name>f{number}</name><length>0</length><fileuid>{}</fileuid>{}</file>",
                    number + 2,
                    if managed { "<extentinfo/>" } else { "" }
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
        #[cfg(feature = "fdb")]
        let stats = if managed {
            let mut storage = crate::local_read_storage::CacheBuild::new(
                &dir,
                crate::local_read_storage::ReadStorageBudget {
                    cache_bytes: 1 << 30,
                    headroom_bytes: 0,
                    ..Default::default()
                },
            )
            .unwrap();
            let stats = VolumeExtentCache::build_with_admission(
                File::open(&xml_path).unwrap(),
                &db_path,
                expected,
                true,
                true,
                &mut |conn, path, bytes| storage.admit(conn, path, bytes),
            )
            .unwrap();
            storage.finish().unwrap();
            stats
        } else {
            VolumeExtentCache::build(File::open(&xml_path).unwrap(), &db_path, expected).unwrap()
        };
        #[cfg(not(feature = "fdb"))]
        let stats =
            VolumeExtentCache::build(File::open(&xml_path).unwrap(), &db_path, expected).unwrap();
        assert_eq!(stats.files, COUNT);
        let cache = VolumeExtentCache::open_with_mode(&db_path, expected, managed).unwrap();
        assert_eq!(
            cache.required_file_by_uid(COUNT + 1).unwrap().path,
            format!("f{}", COUNT - 1)
        );
        drop(cache);
        println!(
            "million cache: managed={managed}, {:?}, xml={} db={}",
            started.elapsed(),
            fs::metadata(&xml_path).unwrap().len(),
            fs::metadata(&db_path).unwrap().len()
        );
        fs::remove_file(db_path).unwrap();
        fs::remove_file(xml_path).unwrap();
        if managed {
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
