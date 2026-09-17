//! Raft 状态的持久化：任期、投票、提交位置和日志条目写穿到 SQLite，内存里用 raft-rs 的
//! `MemStorage` 做读缓存。任期与投票必须持久化，否则节点重启后可能在同一任期重复投票。
//!
//! 骨架不做快照与日志压缩：日志只在换届和接管时增长。

use std::path::Path;

use protobuf::Message as _;
use raft::eraftpb::{ConfState, Entry, HardState};
use raft::storage::MemStorage;
use rusqlite::{Connection, params};

use crate::error::{Result, TapeError};

pub struct RaftStore {
    db: Connection,
    mem: MemStorage,
}

fn db_err(e: rusqlite::Error) -> TapeError {
    TapeError::Ltfs(format!("控制存储: {}", e))
}

impl RaftStore {
    /// 打开（或创建）控制存储，并把已有状态装入内存。`voters` 是固定的三节点成员。
    pub fn open(path: &Path, voters: &[u64]) -> Result<Self> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS hardstate (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (idx INTEGER PRIMARY KEY, data BLOB NOT NULL);",
        )
        .map_err(db_err)?;

        let mem = MemStorage::new_with_conf_state(ConfState::from((voters.to_vec(), vec![])));
        let mut entries = Vec::new();
        {
            let mut stmt = db.prepare("SELECT data FROM entries ORDER BY idx").map_err(db_err)?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).map_err(db_err)?;
            for row in rows {
                let bytes = row.map_err(db_err)?;
                let e = Entry::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的日志条目损坏: {}", e)))?;
                entries.push(e);
            }
        }
        let hs: Option<Vec<u8>> = db
            .query_row("SELECT data FROM hardstate WHERE id = 1", [], |r| r.get(0))
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })
            .map_err(db_err)?;
        {
            let mut w = mem.wl();
            if !entries.is_empty() {
                w.append(&entries).map_err(|e| TapeError::Ltfs(format!("装入日志失败: {}", e)))?;
            }
            if let Some(bytes) = hs {
                let hs = HardState::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的 HardState 损坏: {}", e)))?;
                w.set_hardstate(hs);
            }
        }
        Ok(Self { db, mem })
    }

    /// 交给 `RawNode` 的存储句柄（与本结构共享同一份内存状态）。
    pub fn raft_storage(&self) -> MemStorage {
        self.mem.clone()
    }

    /// 追加日志：先落盘再进内存。新条目的起点之后的旧条目被覆盖（Raft 的日志截断）。
    pub fn append(&mut self, ents: &[Entry]) -> Result<()> {
        let Some(first) = ents.first() else {
            return Ok(());
        };
        let tx = self.db.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM entries WHERE idx >= ?1", params![first.index as i64]).map_err(db_err)?;
        for e in ents {
            let bytes = e.write_to_bytes().map_err(|e| TapeError::Ltfs(e.to_string()))?;
            tx.execute("INSERT INTO entries (idx, data) VALUES (?1, ?2)", params![e.index as i64, bytes])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        self.mem.wl().append(ents).map_err(|e| TapeError::Ltfs(format!("追加日志失败: {}", e)))
    }

    pub fn set_hardstate(&mut self, hs: &HardState) -> Result<()> {
        self.persist_hardstate(hs)?;
        self.mem.wl().set_hardstate(hs.clone());
        Ok(())
    }

    pub fn set_commit(&mut self, commit: u64) -> Result<()> {
        let mut hs = self.mem.rl().hard_state().clone();
        hs.set_commit(commit);
        self.persist_hardstate(&hs)?;
        self.mem.wl().mut_hard_state().set_commit(commit);
        Ok(())
    }

    fn persist_hardstate(&self, hs: &HardState) -> Result<()> {
        let bytes = hs.write_to_bytes().map_err(|e| TapeError::Ltfs(e.to_string()))?;
        self.db
            .execute(
                "INSERT INTO hardstate (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
                params![bytes],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raft::Storage;

    fn entry(index: u64, term: u64, data: &[u8]) -> Entry {
        let mut e = Entry::default();
        e.index = index;
        e.term = term;
        e.data = data.to_vec().into();
        e
    }

    #[test]
    fn term_vote_and_log_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("ltfsd-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("raft.db");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
            s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")]).unwrap();
            let mut hs = HardState::default();
            hs.term = 2;
            hs.vote = 3;
            s.set_hardstate(&hs).unwrap();
            s.set_commit(2).unwrap();
            // 日志截断：从索引 3 起被新任期的条目覆盖
            s.append(&[entry(3, 3, b"c2")]).unwrap();
        }
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let st = s.raft_storage();
        let init = st.initial_state().unwrap();
        assert_eq!((init.hard_state.term, init.hard_state.vote, init.hard_state.commit), (2, 3, 2));
        assert_eq!(init.conf_state.voters, vec![1, 2, 3]);
        assert_eq!(st.last_index().unwrap(), 3);
        assert_eq!(st.term(3).unwrap(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
