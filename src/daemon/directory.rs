//! 每个节点的本地目录库（SQLite）。只由 Raft 应用线程写，内容完全由已提交的日志决定，
//! 所以三个节点上的库是一致的。P1 只有池与磁带归属；文件目录在 P2 加入。
//!
//! 节点重启后 raft-rs 会从头重放已提交的条目。库里记着自己应用到了哪个索引，
//! 重放时跳过已经落库的条目，所以重放是幂等的。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use super::state::{Applied, Command, ControlState};
use crate::error::{Result, TapeError};

pub struct Directory {
    db: Connection,
    applied: u64,
}

fn db_err(e: rusqlite::Error) -> TapeError {
    TapeError::Ltfs(format!("目录库: {}", e))
}

/// 目录里的一个文件：在哪盘带、哪一代索引。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub barcode: String,
    pub generation: u64,
    pub length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    pub uuid: String,
    pub name: String,
    pub file_limit: u64,
    pub tapes: Vec<String>,
}

impl Directory {
    /// 只读连接，给 HTTP 线程查询用。WAL 模式下读不阻塞 Raft 应用线程的写。
    pub fn open_reader(path: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(db_err)?;
        Ok(Self { db, applied: 0 })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS pools (uuid TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, file_limit INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tapes (barcode TEXT PRIMARY KEY, pool_uuid TEXT NOT NULL REFERENCES pools(uuid),
                 volume_uuid TEXT NOT NULL DEFAULT '', generation INTEGER NOT NULL DEFAULT 0,
                 files INTEGER NOT NULL DEFAULT 0, bytes_used INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS files (pool_uuid TEXT NOT NULL, path TEXT NOT NULL, barcode TEXT NOT NULL,
                 generation INTEGER NOT NULL, length INTEGER NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY (pool_uuid, path));
             CREATE INDEX IF NOT EXISTS files_by_tape ON files (barcode);
             CREATE TABLE IF NOT EXISTS staging (barcode TEXT NOT NULL, generation INTEGER NOT NULL, part INTEGER NOT NULL,
                 path TEXT NOT NULL, length INTEGER NOT NULL, sha256 TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS staging_by_batch ON staging (barcode, generation, part);",
        )
        .map_err(db_err)?;
        let applied: Option<i64> =
            db.query_row("SELECT v FROM meta WHERE k = 'applied_index'", [], |r| r.get(0)).optional().map_err(db_err)?;
        Ok(Self { db, applied: applied.unwrap_or(0) as u64 })
    }

    /// 已经落库的最后一个日志索引。
    pub fn applied_index(&self) -> u64 {
        self.applied
    }

    /// 把一条已应用的命令落库。`outcome` 是状态机给出的结论：被拒绝的命令不改变任何东西。
    /// 索引不大于 `applied_index` 的条目（重放）直接跳过。
    pub fn apply(&mut self, index: u64, cmd: &Command, outcome: &Applied) -> Result<()> {
        if index <= self.applied {
            return Ok(());
        }
        let tx = self.db.transaction().map_err(db_err)?;
        if let Applied::Admin(Ok(_)) = outcome {
            match cmd {
                Command::PoolCreate { uuid, name, file_limit } => {
                    tx.execute(
                        "INSERT OR REPLACE INTO pools (uuid, name, file_limit) VALUES (?1, ?2, ?3)",
                        params![uuid, name, *file_limit as i64],
                    )
                    .map_err(db_err)?;
                }
                Command::TapeAssign { barcode, pool } => {
                    // 命令里可以是池名；落库用 UUID
                    let uuid: String = tx
                        .query_row("SELECT uuid FROM pools WHERE uuid = ?1 OR name = ?1", params![pool], |r| r.get(0))
                        .map_err(db_err)?;
                    tx.execute("INSERT OR REPLACE INTO tapes (barcode, pool_uuid) VALUES (?1, ?2)", params![barcode, uuid])
                        .map_err(db_err)?;
                }
                Command::TapeUnassign { barcode } => {
                    tx.execute("DELETE FROM tapes WHERE barcode = ?1", params![barcode]).map_err(db_err)?;
                }
                _ => {}
            }
        }
        match (cmd, outcome) {
            (Command::CatalogPart { barcode, generation, part, files }, _) => {
                let mut ins = tx
                    .prepare("INSERT INTO staging (barcode, generation, part, path, length, sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                    .map_err(db_err)?;
                for f in files {
                    ins.execute(params![barcode, *generation as i64, *part as i64, f.path, f.length as i64, f.sha256]).map_err(db_err)?;
                }
                if files.is_empty() {
                    // 空片也要留痕，片数才对得上
                    ins.execute(params![barcode, *generation as i64, *part as i64, "", -1i64, ""]).map_err(db_err)?;
                }
            }
            (Command::TapeCommitted { barcode, volume_uuid, generation, files, bytes_used, parts, full }, Applied::CatalogCommitted) => {
                let have: i64 = tx
                    .query_row(
                        "SELECT COUNT(DISTINCT part) FROM staging WHERE barcode = ?1 AND generation = ?2",
                        params![barcode, *generation as i64],
                        |r| r.get(0),
                    )
                    .map_err(db_err)?;
                // 片齐全才并入：一批目录记录原子出现。片不齐（Leader 中途失败）则整批丢弃，
                // 该带的目录停在旧代数，等下次装载时按磁带对账补齐。
                if have == *parts as i64 {
                    if *full {
                        tx.execute("DELETE FROM files WHERE barcode = ?1", params![barcode]).map_err(db_err)?;
                    }
                    tx.execute(
                        "INSERT OR REPLACE INTO files (pool_uuid, path, barcode, generation, length, sha256)
                         SELECT t.pool_uuid, s.path, s.barcode, s.generation, s.length, s.sha256
                         FROM staging s JOIN tapes t ON t.barcode = s.barcode
                         WHERE s.barcode = ?1 AND s.generation = ?2 AND s.length >= 0",
                        params![barcode, *generation as i64],
                    )
                    .map_err(db_err)?;
                    tx.execute(
                        "UPDATE tapes SET volume_uuid = ?2, generation = ?3, files = ?4, bytes_used = ?5 WHERE barcode = ?1",
                        params![barcode, volume_uuid, *generation as i64, *files as i64, *bytes_used as i64],
                    )
                    .map_err(db_err)?;
                }
                tx.execute("DELETE FROM staging WHERE barcode = ?1 AND generation <= ?2", params![barcode, *generation as i64])
                    .map_err(db_err)?;
            }
            _ => {}
        }
        tx.execute(
            "INSERT INTO meta (k, v) VALUES ('applied_index', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![index as i64],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        self.applied = index;
        Ok(())
    }

    pub fn pools(&self) -> Result<Vec<PoolRow>> {
        let mut out = Vec::new();
        let mut stmt = self.db.prepare("SELECT uuid, name, file_limit FROM pools ORDER BY name").map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
            .map_err(db_err)?;
        for row in rows {
            let (uuid, name, file_limit) = row.map_err(db_err)?;
            let mut t = self.db.prepare("SELECT barcode FROM tapes WHERE pool_uuid = ?1 ORDER BY barcode").map_err(db_err)?;
            let tapes = t.query_map(params![uuid], |r| r.get::<_, String>(0)).map_err(db_err)?.collect::<std::result::Result<_, _>>().map_err(db_err)?;
            out.push(PoolRow { uuid, name, file_limit: file_limit as u64, tapes });
        }
        Ok(out)
    }

    /// 目录里该带的索引代数；0 表示还没有任何目录记录。
    pub fn tape_generation(&self, barcode: &str) -> Result<u64> {
        let g: Option<i64> = self
            .db
            .query_row("SELECT generation FROM tapes WHERE barcode = ?1", params![barcode], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        Ok(g.unwrap_or(0) as u64)
    }

    pub fn stat(&self, pool_uuid: &str, path: &str) -> Result<Option<FileRow>> {
        self.db
            .query_row(
                "SELECT barcode, generation, length, sha256 FROM files WHERE pool_uuid = ?1 AND path = ?2",
                params![pool_uuid, path],
                |r| Ok(FileRow { barcode: r.get(0)?, generation: r.get::<_, i64>(1)? as u64, length: r.get::<_, i64>(2)? as u64, sha256: r.get(3)? }),
            )
            .optional()
            .map_err(db_err)
    }

    pub fn list(&self, pool_uuid: &str) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.db.prepare("SELECT path, length FROM files WHERE pool_uuid = ?1 ORDER BY path").map_err(db_err)?;
        let rows = stmt.query_map(params![pool_uuid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))).map_err(db_err)?;
        rows.collect::<std::result::Result<_, _>>().map_err(db_err)
    }

    /// 库里的内容是否与内存里的控制状态一致（自检用）。
    pub fn matches(&self, ctl: &ControlState) -> Result<bool> {
        let rows = self.pools()?;
        if rows.len() != ctl.pools.len() {
            return Ok(false);
        }
        for r in &rows {
            let Some(p) = ctl.pools.get(&r.uuid) else { return Ok(false) };
            let mut want: Vec<&String> = ctl.tapes.iter().filter(|(_, u)| **u == r.uuid).map(|(b, _)| b).collect();
            want.sort();
            if p.name != r.name || p.file_limit != r.file_limit || want != r.tapes.iter().collect::<Vec<_>>() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> Vec<Command> {
        vec![
            Command::PoolCreate { uuid: "u-1".into(), name: "archive".into(), file_limit: 100 },
            Command::PoolCreate { uuid: "u-2".into(), name: "archive".into(), file_limit: 100 }, // 被拒
            Command::TapeAssign { barcode: "T1".into(), pool: "archive".into() },
            Command::TapeAssign { barcode: "T2".into(), pool: "u-1".into() },
            Command::Takeover { node: 1, term: 1 },
            Command::TapeUnassign { barcode: "T1".into() },
        ]
    }

    #[test]
    fn replay_after_restart_is_idempotent_and_matches_the_state_machine() {
        let dir = std::env::temp_dir().join(format!("ltfsd-directory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.db");
        let _ = std::fs::remove_file(&path);

        let mut ctl = ControlState::default();
        {
            let mut d = Directory::open(&path).unwrap();
            for (i, c) in script().iter().enumerate().take(4) {
                let out = ctl.apply(i as u64 + 1, c);
                d.apply(i as u64 + 1, c, &out).unwrap();
            }
            assert_eq!(d.applied_index(), 4);
        }
        // 重启：状态机从头重放，库跳过已落库的前 4 条，只应用后面的
        let mut ctl2 = ControlState::default();
        let mut d = Directory::open(&path).unwrap();
        assert_eq!(d.applied_index(), 4);
        for (i, c) in script().iter().enumerate() {
            let out = ctl2.apply(i as u64 + 1, c);
            d.apply(i as u64 + 1, c, &out).unwrap();
        }
        assert_eq!(d.applied_index(), 6);
        assert!(d.matches(&ctl2).unwrap());
        assert_eq!(
            d.pools().unwrap(),
            vec![PoolRow { uuid: "u-1".into(), name: "archive".into(), file_limit: 100, tapes: vec!["T2".into()] }]
        );
        let _ = (ctl, std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn a_batch_becomes_visible_atomically_and_incomplete_batches_are_dropped() {
        use super::super::state::FileRec;
        let dir = std::env::temp_dir().join(format!("ltfsd-directory-cat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.db");
        let _ = std::fs::remove_file(&path);
        let mut ctl = ControlState::default();
        let mut d = Directory::open(&path).unwrap();
        let mut idx = 0u64;
        let mut run = |ctl: &mut ControlState, d: &mut Directory, c: Command| {
            idx += 1;
            let out = ctl.apply(idx, &c);
            d.apply(idx, &c, &out).unwrap();
            out
        };
        let rec = |p: &str, n: u64| FileRec { path: p.into(), length: n, sha256: format!("{:064x}", n) };
        run(&mut ctl, &mut d, Command::PoolCreate { uuid: "u".into(), name: "p".into(), file_limit: 10 });
        run(&mut ctl, &mut d, Command::TapeAssign { barcode: "T1".into(), pool: "p".into() });
        let commit = |g: u64, parts: u32, files: u64, full: bool| Command::TapeCommitted {
            barcode: "T1".into(), volume_uuid: "v".into(), generation: g, files, bytes_used: 100, parts, full,
        };

        // 两片的批次：第二片到达、摘要应用之前，一条都不可见
        run(&mut ctl, &mut d, Command::CatalogPart { barcode: "T1".into(), generation: 2, part: 0, files: vec![rec("/a", 1)] });
        assert_eq!(d.stat("u", "/a").unwrap(), None);
        run(&mut ctl, &mut d, Command::CatalogPart { barcode: "T1".into(), generation: 2, part: 1, files: vec![rec("/b", 2)] });
        assert_eq!(run(&mut ctl, &mut d, commit(2, 2, 2, false)), Applied::CatalogCommitted);
        assert_eq!(d.stat("u", "/a").unwrap().map(|f| (f.barcode, f.generation, f.length)), Some(("T1".into(), 2, 1)));
        assert_eq!(d.tape_generation("T1").unwrap(), 2);

        // 片不齐的批次整批丢弃，目录停在旧代数
        run(&mut ctl, &mut d, Command::CatalogPart { barcode: "T1".into(), generation: 3, part: 0, files: vec![rec("/c", 3)] });
        run(&mut ctl, &mut d, commit(3, 2, 3, false));
        assert_eq!(d.stat("u", "/c").unwrap(), None);
        assert_eq!(d.tape_generation("T1").unwrap(), 2);

        // 完整列表（对账）：替换该带的全部记录；同一路径再次上传覆盖旧记录
        run(&mut ctl, &mut d, Command::CatalogPart { barcode: "T1".into(), generation: 4, part: 0, files: vec![rec("/a", 9), rec("/c", 3)] });
        run(&mut ctl, &mut d, commit(4, 1, 2, true));
        assert_eq!(d.list("u").unwrap(), vec![("/a".to_string(), 9), ("/c".to_string(), 3)]);
        assert_eq!(d.tape_generation("T1").unwrap(), 4);
        // 只读连接看到同样的内容
        let r = Directory::open_reader(&path).unwrap();
        assert_eq!(r.stat("u", "/c").unwrap().unwrap().sha256, format!("{:064x}", 3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
