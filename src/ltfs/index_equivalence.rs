//! Disposable disk-backed comparison of streamed Full index entries.
//! No catalog authority or persistent cache format is introduced here.

use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::error::{Result, TapeError};

use super::index::IndexEntry;
use super::index_scratch::{IndexScratchBudget, ScratchDatabase};

pub(super) struct FullIndexComparison {
    db: ScratchDatabase,
}

impl FullIndexComparison {
    pub(super) fn new(budget: &IndexScratchBudget) -> Result<Self> {
        let db = ScratchDatabase::new(budget)?;
        db.execute_batch(
            "CREATE TABLE dp(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                             uid BLOB NOT NULL UNIQUE, digest BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE ip(path BLOB PRIMARY KEY, kind INTEGER NOT NULL,
                             uid BLOB NOT NULL UNIQUE, digest BLOB NOT NULL) WITHOUT ROWID;
             BEGIN;",
        )?;
        Ok(Self { db })
    }

    pub(super) fn record(&self, ip: bool, entry: IndexEntry) -> Result<()> {
        let (path, kind, name, uid, digest) = match entry {
            IndexEntry::File { path, node } => {
                let digest: [u8; 32] = Sha256::digest(format!("{node:?}").as_bytes()).into();
                (path, 1, node.name, node.meta.file_uid, digest)
            }
            IndexEntry::Directory { path, node } => {
                let digest: [u8; 32] = Sha256::digest(format!("{node:?}").as_bytes()).into();
                (path, 0, node.name, node.meta.file_uid, digest)
            }
        };
        // Debug covers all equality fields of a single child-free node.
        // Hashes are only compared inside this invocation, never persisted.
        if uid == 0
            || (path.is_empty() && kind != 0)
            || name.contains(['/', '\0'])
            || name == "."
            || name == ".."
            || (!path.is_empty() && (name.is_empty() || path.rsplit('/').next() != Some(&name)))
        {
            return Err(TapeError::Ltfs("DP/IP Full 节点名称或 UID 非法".into()));
        }
        self.db.ensure_room(path.len() + 64)?;
        let sql = if ip {
            "INSERT INTO ip(path,kind,uid,digest) VALUES (?1,?2,?3,?4)"
        } else {
            "INSERT INTO dp(path,kind,uid,digest) VALUES (?1,?2,?3,?4)"
        };
        self.db
            .prepare_cached(sql)?
            .execute(params![
                path.as_bytes(),
                kind,
                uid.to_be_bytes().as_slice(),
                digest.as_slice(),
            ])
            .map_err(|error| {
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) {
                    TapeError::LocalCapacity {
                        reason: format!("DP/IP Full 节点重复或暂存失败: {error}"),
                    }
                } else {
                    TapeError::Ltfs(format!("DP/IP Full 节点重复或暂存失败: {error}"))
                }
            })?;
        Ok(())
    }

    pub(super) fn validate_highest_uid(&self, ip: bool, highest: u64) -> Result<()> {
        if highest != 0 {
            let invalid: bool = self.db.query_row(
                if ip {
                    "SELECT EXISTS(SELECT 1 FROM ip WHERE uid > ?1)"
                } else {
                    "SELECT EXISTS(SELECT 1 FROM dp WHERE uid > ?1)"
                },
                [highest.to_be_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if invalid {
                return Err(TapeError::Ltfs(
                    "DP/IP Full 节点 UID 超过 highestfileuid".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn require_equal(&self) -> Result<()> {
        // Indexed joins stream the two sets; node order in XML is irrelevant.
        let different: bool = self.db.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM dp LEFT JOIN ip USING(path)
                 WHERE ip.path IS NULL OR dp.kind != ip.kind OR dp.uid != ip.uid
                    OR dp.digest != ip.digest
             ) OR EXISTS(
                 SELECT 1 FROM ip LEFT JOIN dp USING(path) WHERE dp.path IS NULL
             )",
            [],
            |row| row.get(0),
        )?;
        if different {
            return Err(TapeError::RecoveryRestricted {
                reason: "同代 DP/IP Full 索引节点内容不一致".into(),
            });
        }
        Ok(())
    }

    /// Keep the just-verified predecessor as the next left-hand view without
    /// retaining either directory tree or copying all rows into memory.
    pub(super) fn advance_history(&self) -> Result<()> {
        self.db.execute_batch(
            "ALTER TABLE dp RENAME TO retired;
             ALTER TABLE ip RENAME TO dp;
             ALTER TABLE retired RENAME TO ip;
             DELETE FROM ip;",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{FileNode, NodeMeta};

    fn file(path: &str, uid: u64) -> IndexEntry {
        IndexEntry::File {
            path: path.into(),
            node: FileNode {
                name: path.into(),
                meta: NodeMeta {
                    file_uid: uid,
                    ..Default::default()
                },
                ..Default::default()
            },
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
        let comparison = FullIndexComparison::new(&budget).unwrap();
        let mut denied = false;
        for id in 2..10_002 {
            if comparison
                .record(false, file(&format!("file-{id}"), id))
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
    fn disk_comparison_is_independent_of_node_order_and_detects_omissions() {
        let comparison = FullIndexComparison::new(&IndexScratchBudget::default()).unwrap();
        for id in 2..10_002 {
            comparison
                .record(false, file(&format!("file-{id}"), id))
                .unwrap();
        }
        for id in (3..10_002).rev() {
            comparison
                .record(true, file(&format!("file-{id}"), id))
                .unwrap();
        }
        assert!(comparison.require_equal().is_err());
        comparison.record(true, file("file-2", 2)).unwrap();
        comparison.require_equal().unwrap();
        comparison.record(true, file("extra", 10_002)).unwrap();
        assert!(comparison.require_equal().is_err());
    }

    #[test]
    fn full_views_reject_duplicate_paths_uids_and_invalid_names() {
        let comparison = FullIndexComparison::new(&IndexScratchBudget::default()).unwrap();
        comparison.record(false, file("one", 2)).unwrap();
        assert!(comparison.record(false, file("one", 3)).is_err());
        assert!(comparison.record(false, file("two", 2)).is_err());
        assert!(comparison.record(false, file("zero", 0)).is_err());
        assert!(comparison.record(false, file("a/b", 4)).is_err());
        assert!(comparison.validate_highest_uid(false, 1).is_err());
        comparison.validate_highest_uid(false, 2).unwrap();
    }

    #[test]
    #[ignore = "显式百万条节点落盘对比压测，不属于默认测试"]
    fn million_entry_comparison_spills_to_disk() {
        let comparison = FullIndexComparison::new(&IndexScratchBudget::default()).unwrap();
        for id in 2..1_000_002 {
            comparison
                .record(false, file(&format!("file-{id}"), id))
                .unwrap();
        }
        for id in (2..1_000_002).rev() {
            comparison
                .record(true, file(&format!("file-{id}"), id))
                .unwrap();
        }
        comparison.require_equal().unwrap();
        let (pages, size): (u64, u64) = comparison.db.query_row(
            "SELECT (SELECT page_count FROM pragma_page_count), (SELECT page_size FROM pragma_page_size)",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        println!(
            "DP/IP comparison: 1000000 nodes per partition, {} temporary bytes",
            pages * size
        );
    }
}
