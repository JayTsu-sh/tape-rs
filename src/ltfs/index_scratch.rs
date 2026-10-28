//! Private, disposable storage for streamed index proofs. Space is physically
//! reserved in the database inode before SQLite may grow into it.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use nix::fcntl::{FallocateFlags, fallocate};
use nix::sys::statvfs::fstatvfs;
use rusqlite::Connection;

use crate::error::{Result, TapeError};

const PAGE_SIZE: u64 = 4096;
const CHUNK: u64 = 16 << 20;

/// Node-local limits, independent of tape size and catalog authority. The
/// ceiling is not allocated up front; reservations grow in 16 MiB chunks.
#[derive(Debug, Clone)]
pub struct IndexScratchBudget {
    pub directory: PathBuf,
    pub max_bytes: u64,
    pub headroom_bytes: u64,
}

impl Default for IndexScratchBudget {
    fn default() -> Self {
        Self {
            directory: std::env::temp_dir(),
            max_bytes: 128 << 30,
            headroom_bytes: 1 << 30,
        }
    }
}

fn refused(detail: impl std::fmt::Display) -> TapeError {
    TapeError::LocalCapacity {
        reason: format!("索引核验临时盘准入失败: {detail}"),
    }
}

struct Storage {
    file: File,
    path: PathBuf,
    allocated: Cell<u64>,
    limit: u64,
    headroom: u64,
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Only our create_new inode, never another proof/cache/database.
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) struct ScratchDatabase {
    // Declaration order closes SQLite before removing its backing file.
    db: Connection,
    storage: Storage,
}

impl Deref for ScratchDatabase {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.db
    }
}

impl ScratchDatabase {
    pub(super) fn new(budget: &IndexScratchBudget) -> Result<Self> {
        let limit = (budget.max_bytes / PAGE_SIZE * PAGE_SIZE).min(i64::MAX as u64);
        if limit < PAGE_SIZE * 16 {
            return Err(refused("单次预算至少需要 64 KiB"));
        }
        // Caller supplies an existing, private directory. Never create or
        // clean a shared directory implicitly in the low-level proof helper.
        let path = budget
            .directory
            .join(format!("index-proof-{}.db", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| refused(format!("{}: {error}", budget.directory.display())))?;
        let storage = Storage {
            file,
            path,
            allocated: Cell::new(0),
            limit,
            headroom: budget.headroom_bytes,
        };
        let db = Connection::open(&storage.path)?;
        db.execute_batch(
            "PRAGMA page_size=4096; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             PRAGMA temp_store=FILE; PRAGMA cache_size=-8192; PRAGMA mmap_size=0;
             PRAGMA auto_vacuum=NONE;",
        )?;
        let scratch = Self { db, storage };
        scratch.ensure_room(0)?;
        Ok(scratch)
    }

    /// Reserve enough for one bounded row operation, including B-tree splits.
    /// SQLite's own page ceiling remains a hard backstop on every statement.
    pub(super) fn ensure_room(&self, row_bytes: usize) -> Result<()> {
        let pages: u64 = self
            .db
            .pragma_query_value(None, "page_count", |r| r.get(0))?;
        let required = pages
            .checked_mul(PAGE_SIZE)
            .and_then(|n| n.checked_add((row_bytes as u64).saturating_mul(4)))
            .and_then(|n| n.checked_add(8 * PAGE_SIZE))
            .ok_or_else(|| refused("节点或页数溢出"))?;
        if required > self.storage.limit {
            return Err(refused(format!(
                "超过单次上限 {} bytes",
                self.storage.limit
            )));
        }
        if required <= self.storage.allocated.get() {
            return Ok(());
        }
        let target = required
            .div_ceil(CHUNK)
            .saturating_mul(CHUNK)
            .min(self.storage.limit);
        let extra = target - self.storage.allocated.get();
        let stat = fstatvfs(&self.storage.file)?;
        let available = stat.blocks_available().saturating_mul(stat.fragment_size());
        if available.saturating_sub(self.storage.headroom) < extra {
            return Err(TapeError::LocalSpace {
                reason: format!(
                    "索引核验临时盘准入失败: 目录 {} 可用 {available} bytes，需新增 {extra} bytes，保留 {} bytes",
                    self.storage.path.parent().unwrap().display(),
                    self.storage.headroom
                ),
            });
        }
        // KEEP_SIZE leaves the logical DB length intact. Unlike sparse
        // set_len(), this competes for real blocks across libraries/processes.
        // Other writers can still consume headroom; allocation failure denies
        // this proof, and the old SQLite ceiling is left unchanged.
        fallocate(
            self.storage.file.as_raw_fd(),
            FallocateFlags::FALLOC_FL_KEEP_SIZE,
            self.storage.allocated.get() as i64,
            extra as i64,
        )
        .map_err(|error| {
            if matches!(error, nix::errno::Errno::ENOSPC | nix::errno::Errno::EDQUOT) {
                TapeError::LocalSpace {
                    reason: format!("索引核验临时盘准入失败: 物理空间预留失败: {error}"),
                }
            } else {
                refused(format!("物理空间预留失败: {error}"))
            }
        })?;
        let actual: u64 = self.db.query_row(
            &format!("PRAGMA max_page_count={}", target / PAGE_SIZE),
            [],
            |r| r.get(0),
        )?;
        if actual != target / PAGE_SIZE {
            return Err(refused("SQLite 页数上限设置失败"));
        }
        self.storage.allocated.set(target);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn explicit_directory_physical_reservation_page_cap_and_cleanup() {
        let budget = IndexScratchBudget {
            max_bytes: 1 << 20,
            headroom_bytes: 0,
            ..Default::default()
        };
        let db = ScratchDatabase::new(&budget).unwrap();
        let path = db.storage.path.clone();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(path.parent(), Some(budget.directory.as_path()));
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert!(meta.blocks() * 512 >= budget.max_bytes);
        db.execute_batch("CREATE TABLE t(x); BEGIN;").unwrap();
        let err = db
            .execute("INSERT INTO t VALUES(zeroblob(2097152))", [])
            .unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::DiskFull));
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() <= budget.max_bytes);
        assert!(
            meta.blocks() * 512 >= budget.max_bytes,
            "SQLite retained the reserved blocks"
        );
        drop(db);
        assert!(!path.exists());
    }

    #[test]
    fn reservations_grow_in_chunks_and_clamp_the_final_chunk() {
        let budget = IndexScratchBudget {
            max_bytes: 40 << 20,
            headroom_bytes: 0,
            ..Default::default()
        };
        let db = ScratchDatabase::new(&budget).unwrap();
        db.execute_batch("CREATE TABLE t(x); BEGIN;").unwrap();
        let length = db.storage.file.metadata().unwrap().len();
        for (row_bytes, reserved) in [(5 << 20, 32 << 20), (9 << 20, 40 << 20)] {
            db.ensure_room(row_bytes).unwrap();
            let meta = db.storage.file.metadata().unwrap();
            assert_eq!(
                meta.len(),
                length,
                "KEEP_SIZE preserves logical database length"
            );
            assert!(meta.blocks() * 512 >= reserved);
            let pages: u64 = db
                .pragma_query_value(None, "max_page_count", |r| r.get(0))
                .unwrap();
            assert_eq!(pages * PAGE_SIZE, reserved);
        }
        assert!(db.ensure_room(11 << 20).is_err());
    }

    #[test]
    fn denied_admission_and_growth_release_only_their_own_files() {
        let directory =
            std::env::temp_dir().join(format!("tape-scratch-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let marker = directory.join("existing-cache");
        std::fs::write(&marker, b"keep").unwrap();
        let mut budget = IndexScratchBudget {
            directory: directory.clone(),
            max_bytes: 1 << 20,
            headroom_bytes: u64::MAX,
        };
        assert!(
            ScratchDatabase::new(&budget)
                .err()
                .unwrap()
                .to_string()
                .contains("临时盘准入")
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        budget.headroom_bytes = 0;
        let first = ScratchDatabase::new(&budget).unwrap();
        let second = ScratchDatabase::new(&budget).unwrap();
        let first_path = first.storage.path.clone();
        let second_path = second.storage.path.clone();
        assert_ne!(first_path, second_path);
        assert!(
            first
                .ensure_room(1 << 20)
                .unwrap_err()
                .to_string()
                .contains("单次上限")
        );
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
        std::fs::remove_file(marker).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    #[ignore = "需要 TAPE_RS_SCRATCH_TEST_DIR 指向独占 24 MiB 临时文件系统；不接触磁带"]
    fn constrained_filesystem_denies_overcommit_and_releases_reservation() {
        let directory = PathBuf::from(
            std::env::var_os("TAPE_RS_SCRATCH_TEST_DIR").expect("explicit scratch filesystem"),
        );
        let stat = nix::sys::statvfs::statvfs(&directory).unwrap();
        assert_eq!(stat.blocks() * stat.fragment_size(), 24 << 20);
        let budget = IndexScratchBudget {
            directory,
            max_bytes: 64 << 20,
            headroom_bytes: 0,
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let (tx, rx) = std::sync::mpsc::channel();
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let budget = budget.clone();
                let barrier = barrier.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let admitted = ScratchDatabase::new(&budget);
                    if let Err(error) = &admitted {
                        eprintln!("concurrent admission denied: {error}");
                        assert!(error.to_string().contains("临时盘准入"), "{error}");
                    }
                    tx.send(admitted.is_ok()).unwrap();
                    // Keep the winner's blocks reserved until both attempts end.
                    barrier.wait();
                    drop(admitted);
                })
            })
            .collect();
        barrier.wait();
        let admitted = usize::from(rx.recv().unwrap()) + usize::from(rx.recv().unwrap());
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(admitted <= 1, "actual allocation prevents overcommit");
        let replacement = ScratchDatabase::new(&budget).unwrap();
        drop(replacement);
        assert_eq!(std::fs::read_dir(&budget.directory).unwrap().count(), 0);
    }
}
