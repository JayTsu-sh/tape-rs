//! Compare frozen offline directory projections from different Raft nodes.
//! This is source evidence, not an FDB runtime authority switch.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use super::{CatalogError, Result, import_reconcile};

const DOMAIN: &[u8] = b"tape-rs/frozen-directory-content/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrozenSnapshotIdentity {
    pub applied_index: u64,
    pub frozen_index: u64,
    pub catalog_sha256: [u8; 32],
    pub snapshot_sha256: [u8; 32],
    /// pools, tapes, files, file_tasks in that order.
    pub row_counts: [u64; 4],
}

/// A downloaded node export whose HTTP headers match the offline file.
/// The node ID still relies on the authenticated management connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedNodeExport {
    pub node_id: u64,
    pub(super) identity: FrozenSnapshotIdentity,
}

impl VerifiedNodeExport {
    pub fn identity(&self) -> FrozenSnapshotIdentity {
        self.identity
    }
}

pub fn verify_node_export(
    path: &Path,
    node_id: u64,
    header_applied_index: u64,
    header_frozen_index: u64,
    header_sha256: [u8; 32],
) -> Result<VerifiedNodeExport> {
    if node_id == 0 {
        return Err(CatalogError::Invalid("目录快照来源节点 ID 无效"));
    }
    let identity = inspect_frozen_snapshot(path)?;
    if identity.applied_index != header_applied_index
        || identity.frozen_index != header_frozen_index
        || identity.snapshot_sha256 != header_sha256
    {
        return Err(CatalogError::Invalid("目录快照内容与节点导出头不一致"));
    }
    Ok(VerifiedNodeExport { node_id, identity })
}

fn sqlite_error(error: rusqlite::Error) -> CatalogError {
    CatalogError::Cache(format!("冻结目录快照读取失败: {error}"))
}

fn hash_bytes(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn hash_value(digest: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => digest.update([0]),
        ValueRef::Integer(value) => {
            digest.update([1]);
            digest.update(value.to_be_bytes());
        }
        ValueRef::Real(value) => {
            digest.update([2]);
            digest.update(value.to_bits().to_be_bytes());
        }
        ValueRef::Text(value) => {
            digest.update([3]);
            hash_bytes(digest, value);
        }
        ValueRef::Blob(value) => {
            digest.update([4]);
            hash_bytes(digest, value);
        }
    }
}

/// Read an offline VACUUM snapshot in one SQLite transaction, excluding only
/// the Raft applied index from the canonical catalog hash. Rejected commands
/// may advance that index after freeze without changing the catalog content.
pub fn inspect_frozen_snapshot(path: &Path) -> Result<FrozenSnapshotIdentity> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sqlite_error)?;
    let journal: String = db
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(sqlite_error)?;
    if journal.eq_ignore_ascii_case("wal") {
        return Err(CatalogError::Invalid("冻结目录比较必须使用离线一致快照"));
    }
    db.execute_batch("BEGIN").map_err(sqlite_error)?;
    let (applied, frozen, staging, tasks): (Option<i64>, Option<i64>, i64, i64) = db
        .query_row(
            "SELECT (SELECT v FROM meta WHERE k='applied_index'),
                    (SELECT v FROM meta WHERE k='catalog_frozen_index'),
                    EXISTS(SELECT 1 FROM staging LIMIT 1),
                    EXISTS(SELECT 1 FROM file_tasks
                           WHERE status NOT IN ('committed','failed') LIMIT 1)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(sqlite_error)?;
    let applied_index = applied
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(CatalogError::Invalid("冻结目录缺少有效应用水位"))?;
    let frozen_index = frozen
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0 && *value <= applied_index)
        .ok_or(CatalogError::Invalid("冻结目录缺少有效冻结索引"))?;
    if staging != 0 || tasks != 0 {
        return Err(CatalogError::Invalid("冻结目录仍有暂存或未决文件任务"));
    }

    let tables = [
        (b'p', "SELECT uuid,name,file_limit FROM pools ORDER BY uuid"),
        (
            b't',
            "SELECT barcode,pool_uuid,volume_uuid,generation,files,bytes_used,bytes_written FROM tapes ORDER BY barcode",
        ),
        (
            b'f',
            "SELECT pool_uuid,path,barcode,generation,length,sha256,ver_round,ver_seq,deleted,metadata FROM files ORDER BY pool_uuid,path",
        ),
        (
            b'j',
            "SELECT pool_uuid,task,round,path,status,generation,reason FROM file_tasks ORDER BY pool_uuid,task",
        ),
    ];
    let mut digest = Sha256::new();
    digest.update(DOMAIN);
    digest.update(frozen_index.to_be_bytes());
    let mut row_counts = [0u64; 4];
    for (index, (tag, query)) in tables.into_iter().enumerate() {
        digest.update([tag]);
        let mut statement = db.prepare(query).map_err(sqlite_error)?;
        let mut rows = statement.query([]).map_err(sqlite_error)?;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            digest.update([1]);
            for column in 0..row.as_ref().column_count() {
                hash_value(&mut digest, row.get_ref(column).map_err(sqlite_error)?);
            }
            row_counts[index] = row_counts[index]
                .checked_add(1)
                .ok_or(CatalogError::Corrupt("冻结目录记录数溢出"))?;
        }
        digest.update([0]);
        digest.update(row_counts[index].to_be_bytes());
    }
    Ok(FrozenSnapshotIdentity {
        applied_index,
        frozen_index,
        catalog_sha256: digest.finalize().into(),
        snapshot_sha256: import_reconcile::snapshot_sha256(path)?,
        row_counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::directory::Directory;
    use crate::daemon::state::{Applied, Command, ControlState};

    #[test]
    fn content_digest_ignores_post_freeze_raft_watermark_but_detects_catalog_changes() {
        let root = std::env::temp_dir().join(format!("frozen-content-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut first = Directory::open(&root.join("first.db")).unwrap();
        let mut second = Directory::open(&root.join("second.db")).unwrap();
        let mut control = ControlState::default();
        for (index, command) in [
            (
                1,
                Command::PoolCreate {
                    uuid: "00000000-0000-0000-0000-000000000001".into(),
                    name: "archive".into(),
                    file_limit: 10,
                },
            ),
            (2, Command::CatalogQuiesce),
            (3, Command::CatalogFreeze { quiesce_index: 2 }),
        ] {
            let outcome = control.apply(index, &command);
            assert!(matches!(outcome, Applied::Admin(Ok(_))));
            first.apply(index, &command, &outcome).unwrap();
            second.apply(index, &command, &outcome).unwrap();
        }
        let rejected = Command::PoolCreate {
            uuid: "00000000-0000-0000-0000-000000000002".into(),
            name: "late".into(),
            file_limit: 10,
        };
        let outcome = control.apply(4, &rejected);
        assert!(matches!(outcome, Applied::CatalogRejected));
        second.apply(4, &rejected, &outcome).unwrap();
        let a = root.join("a.snap.db");
        let b = root.join("b.snap.db");
        first.write_snapshot(&a).unwrap();
        second.write_snapshot(&b).unwrap();
        let left = inspect_frozen_snapshot(&a).unwrap();
        let right = inspect_frozen_snapshot(&b).unwrap();
        assert_eq!((left.applied_index, right.applied_index), (3, 4));
        assert_eq!(left.frozen_index, 3);
        assert_eq!(left.row_counts, [1, 0, 0, 0]);
        assert_eq!(left.catalog_sha256, right.catalog_sha256);
        assert_ne!(left.snapshot_sha256, right.snapshot_sha256);

        let db = Connection::open(&b).unwrap();
        db.execute("UPDATE pools SET file_limit=11", []).unwrap();
        drop(db);
        assert_ne!(
            left.catalog_sha256,
            inspect_frozen_snapshot(&b).unwrap().catalog_sha256
        );
        let db = Connection::open(&b).unwrap();
        db.execute("INSERT INTO staging(barcode,generation,part,path,length,sha256) VALUES('T1',1,0,'/f',1,'x')", []).unwrap();
        drop(db);
        assert!(inspect_frozen_snapshot(&b).is_err());
        assert!(inspect_frozen_snapshot(&root.join("first.db")).is_err());
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }
}
