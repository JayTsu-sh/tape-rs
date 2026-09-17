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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    pub uuid: String,
    pub name: String,
    pub file_limit: u64,
    pub tapes: Vec<String>,
}

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS pools (uuid TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, file_limit INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tapes (barcode TEXT PRIMARY KEY, pool_uuid TEXT NOT NULL REFERENCES pools(uuid));",
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
}
