//! Raft 状态的持久化：任期、投票、提交位置、日志条目和快照写穿到 SQLite，内存里用 raft-rs 的
//! `MemStorage` 做读缓存。任期与投票必须持久化，否则节点重启后可能在同一任期重复投票。
//!
//! 三件事在这里保证：
//!
//! - **日志回滚**。Raft 的日志可以被覆写：新 Leader 发来的条目与本地未提交的尾部冲突时，
//!   冲突点之后的条目全部作废。`append` 在同一个事务里先删后插；收到快照时整段日志作废
//!   （`install_snapshot`）。回滚绝不允许越过快照点，越过就是不变式被破坏，直接报错。
//! - **日志压缩**。`compact_to` 把快照点之前的条目从库里删掉，日志大小因此有界。
//! - **发快照**。raft-rs 要给落后的 Follower 发快照时会向 `Storage::snapshot()` 要一份。
//!   `MemStorage` 只会给出空数据的快照，所以这里套一层 `ControlStorage`，把带数据的那一份接管过来。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use protobuf::Message as _;
use raft::eraftpb::{ConfState, Entry, HardState, Snapshot};
use raft::storage::{MemStorage, Storage};
use raft::{GetEntriesContext, RaftState, StorageError};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Result, TapeError};

/// 何时做一次快照并压缩日志。两个条件任一满足即可。
#[derive(Debug, Clone, Copy)]
pub struct SnapshotPolicy {
    /// 快照点之后累积了这么多条目
    pub entries: u64,
    /// 快照点之后累积了这么多字节的条目数据
    pub bytes: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        // 目录条目每片约 1 MiB，所以字节数是更有效的那个闸门：256 MiB 约等于 256 片。
        Self { entries: 2_000, bytes: 256 << 20 }
    }
}

/// 交给 raft-rs 的存储句柄。日志部分直接转给 `MemStorage`（它由 `RaftStore` 写穿到 SQLite），
/// 只把 `snapshot()` 接管过来。
#[derive(Clone)]
pub struct ControlStorage {
    mem: MemStorage,
    latest: Arc<Mutex<Option<Snapshot>>>,
    /// raft-rs 要过、但我们当时给不出的快照索引。Raft 循环看到它就现做一份。
    wanted: Arc<AtomicU64>,
}

impl ControlStorage {
    fn new(mem: MemStorage) -> Self {
        Self { mem, latest: Arc::new(Mutex::new(None)), wanted: Arc::new(AtomicU64::new(0)) }
    }

    fn publish(&self, snap: Snapshot) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
    }

    /// 有节点在等快照吗？取走请求（只报一次，Raft 会重试）。
    pub fn wanted(&self) -> Option<u64> {
        let w = self.wanted.swap(0, Ordering::SeqCst);
        (w > 0).then_some(w)
    }
}

impl Storage for ControlStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        self.mem.initial_state()
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        context: GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        self.mem.entries(low, high, max_size, context)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        self.mem.term(idx)
    }

    fn first_index(&self) -> raft::Result<u64> {
        self.mem.first_index()
    }

    fn last_index(&self) -> raft::Result<u64> {
        self.mem.last_index()
    }

    fn snapshot(&self, request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        let latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        match latest.as_ref() {
            Some(s) if s.get_metadata().index >= request_index => Ok(s.clone()),
            _ => {
                // 记下请求，Raft 循环下一圈现做一份；这一次先让 raft-rs 稍后重试。
                self.wanted.fetch_max(request_index.max(1), Ordering::SeqCst);
                Err(raft::Error::Store(StorageError::SnapshotTemporarilyUnavailable))
            }
        }
    }
}

pub struct RaftStore {
    db: Connection,
    store: ControlStorage,
    voters: Vec<u64>,
    /// 快照覆盖到的日志索引。日志回滚绝不能越过它。
    snapshot_index: u64,
    /// 启动时从库里读出的快照（如果有）：控制状态要从它恢复，而不是从头重放日志
    loaded: Option<Snapshot>,
    /// 快照点之后追加的条目数与字节数（压缩触发用）
    since_entries: u64,
    since_bytes: u64,
}

fn db_err(e: rusqlite::Error) -> TapeError {
    TapeError::Ltfs(format!("控制存储: {}", e))
}

fn pb_err(e: protobuf::ProtobufError) -> TapeError {
    TapeError::Ltfs(e.to_string())
}

impl RaftStore {
    /// 打开（或创建）控制存储，并把已有状态装入内存。`voters` 是固定的三节点成员。
    pub fn open(path: &Path, voters: &[u64]) -> Result<Self> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS hardstate (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (idx INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS snapshot (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);",
        )
        .map_err(db_err)?;

        let mem = MemStorage::new_with_conf_state(ConfState::from((voters.to_vec(), vec![])));
        let store = ControlStorage::new(mem);

        // 顺序要紧：先装快照（它会清空内存里的日志并把提交位置推到快照点），
        // 再装快照之后的日志，最后用库里的 HardState 覆盖任期、投票和提交位置。
        let snap_bytes: Option<Vec<u8>> =
            db.query_row("SELECT data FROM snapshot WHERE id = 1", [], |r| r.get(0)).optional().map_err(db_err)?;
        let mut snapshot_index = 0;
        let mut loaded = None;
        if let Some(bytes) = snap_bytes {
            let snap = Snapshot::parse_from_bytes(&bytes)
                .map_err(|e| TapeError::Ltfs(format!("控制存储里的快照损坏: {}", e)))?;
            snapshot_index = snap.get_metadata().index;
            store
                .mem
                .wl()
                .apply_snapshot(snap.clone())
                .map_err(|e| TapeError::Ltfs(format!("装入快照失败: {}", e)))?;
            store.publish(snap.clone());
            loaded = Some(snap);
        }

        let mut entries = Vec::new();
        {
            let mut stmt = db.prepare("SELECT data FROM entries WHERE idx > ?1 ORDER BY idx").map_err(db_err)?;
            let rows = stmt.query_map(params![snapshot_index as i64], |r| r.get::<_, Vec<u8>>(0)).map_err(db_err)?;
            for row in rows {
                let bytes = row.map_err(db_err)?;
                let e = Entry::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的日志条目损坏: {}", e)))?;
                entries.push(e);
            }
        }
        let hs: Option<Vec<u8>> =
            db.query_row("SELECT data FROM hardstate WHERE id = 1", [], |r| r.get(0)).optional().map_err(db_err)?;
        {
            let mut w = store.mem.wl();
            if !entries.is_empty() {
                w.append(&entries).map_err(|e| TapeError::Ltfs(format!("装入日志失败: {}", e)))?;
            }
            if let Some(bytes) = hs {
                let hs = HardState::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的 HardState 损坏: {}", e)))?;
                w.set_hardstate(hs);
            }
        }
        let since_bytes = entries.iter().map(|e| e.data.len() as u64).sum();
        Ok(Self {
            db,
            store,
            voters: voters.to_vec(),
            snapshot_index,
            loaded,
            since_entries: entries.len() as u64,
            since_bytes,
        })
    }

    /// 交给 `RawNode` 的存储句柄（与本结构共享同一份内存状态）。
    pub fn raft_storage(&self) -> ControlStorage {
        self.store.clone()
    }

    /// 启动时读到的快照：控制状态要从它恢复。
    pub fn loaded_snapshot(&self) -> Option<&Snapshot> {
        self.loaded.as_ref()
    }

    pub fn snapshot_index(&self) -> u64 {
        self.snapshot_index
    }

    /// 该做一次快照了吗？
    pub fn due(&self, policy: &SnapshotPolicy) -> bool {
        self.since_entries >= policy.entries || self.since_bytes >= policy.bytes
    }

    /// 追加日志：先落盘再进内存。新条目的起点之后的旧条目被覆盖，这就是 Raft 的日志回滚。
    pub fn append(&mut self, ents: &[Entry]) -> Result<()> {
        let Some(first) = ents.first() else {
            return Ok(());
        };
        if first.index <= self.snapshot_index {
            // 回滚只会发生在未提交的尾部，而快照点不会超过已应用的位置，所以这不可能发生。
            // 真发生了说明有更严重的问题，宁可停下也不要把快照点之前的历史改掉。
            return Err(TapeError::Ltfs(format!(
                "日志回滚越过快照点：要从索引 {} 起覆写，但快照已覆盖到 {}",
                first.index, self.snapshot_index
            )));
        }
        let tx = self.db.transaction().map_err(db_err)?;
        let rolled = tx
            .execute("DELETE FROM entries WHERE idx >= ?1", params![first.index as i64])
            .map_err(db_err)?;
        for e in ents {
            let bytes = e.write_to_bytes().map_err(pb_err)?;
            tx.execute("INSERT INTO entries (idx, data) VALUES (?1, ?2)", params![e.index as i64, bytes])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        if rolled > 0 {
            log::info!("日志回滚：索引 {} 起的 {} 条被覆写", first.index, rolled);
        }
        self.store.mem.wl().append(ents).map_err(|e| TapeError::Ltfs(format!("追加日志失败: {}", e)))?;
        self.since_entries += ents.len() as u64;
        self.since_bytes += ents.iter().map(|e| e.data.len() as u64).sum::<u64>();
        Ok(())
    }

    pub fn set_hardstate(&mut self, hs: &HardState) -> Result<()> {
        self.persist_hardstate(hs)?;
        self.store.mem.wl().set_hardstate(hs.clone());
        Ok(())
    }

    pub fn set_commit(&mut self, commit: u64) -> Result<()> {
        let mut hs = self.store.mem.rl().hard_state().clone();
        hs.set_commit(commit);
        self.persist_hardstate(&hs)?;
        self.store.mem.wl().mut_hard_state().set_commit(commit);
        Ok(())
    }

    /// 用当前状态做一份覆盖到 `index` 的快照。`data` 是状态机自己的内容。
    pub fn build_snapshot(&self, index: u64, data: Vec<u8>) -> Result<Snapshot> {
        let term = self.store.term(index).map_err(|e| TapeError::Ltfs(format!("取索引 {} 的任期失败: {}", index, e)))?;
        let mut snap = Snapshot::default();
        snap.set_data(data.into());
        let meta = snap.mut_metadata();
        meta.index = index;
        meta.term = term;
        meta.set_conf_state(ConfState::from((self.voters.clone(), vec![])));
        Ok(snap)
    }

    /// 本节点自己做的快照：落盘、丢掉快照点之前的日志、并把它作为要发给落后节点的那一份。
    pub fn compact_to(&mut self, snap: &Snapshot) -> Result<()> {
        let index = snap.get_metadata().index;
        if index <= self.snapshot_index {
            return Ok(());
        }
        let bytes = snap.write_to_bytes().map_err(pb_err)?;
        let tx = self.db.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM entries WHERE idx < ?1", params![index as i64]).map_err(db_err)?;
        tx.execute(
            "INSERT INTO snapshot (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![bytes],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        self.store.mem.wl().compact(index).map_err(|e| TapeError::Ltfs(format!("压缩日志失败: {}", e)))?;
        self.finish_snapshot(snap, index);
        Ok(())
    }

    /// 收到别的节点发来的快照：整段日志作废（Raft 的日志回滚在快照这条路上的形态），
    /// 状态从快照重建。
    pub fn install_snapshot(&mut self, snap: &Snapshot) -> Result<()> {
        let index = snap.get_metadata().index;
        let bytes = snap.write_to_bytes().map_err(pb_err)?;
        let tx = self.db.transaction().map_err(db_err)?;
        let dropped = tx.execute("DELETE FROM entries", []).map_err(db_err)?;
        tx.execute(
            "INSERT INTO snapshot (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![bytes],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        log::info!("装入索引 {} 的快照，本地 {} 条日志作废", index, dropped);
        self.store
            .mem
            .wl()
            .apply_snapshot(snap.clone())
            .map_err(|e| TapeError::Ltfs(format!("装入快照失败: {}", e)))?;
        let hs = self.store.mem.rl().hard_state().clone();
        self.persist_hardstate(&hs)?;
        self.finish_snapshot(snap, index);
        Ok(())
    }

    fn finish_snapshot(&mut self, snap: &Snapshot, index: u64) {
        self.snapshot_index = index;
        self.since_entries = 0;
        self.since_bytes = 0;
        self.store.publish(snap.clone());
    }

    fn persist_hardstate(&self, hs: &HardState) -> Result<()> {
        let bytes = hs.write_to_bytes().map_err(pb_err)?;
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

    fn entry(index: u64, term: u64, data: &[u8]) -> Entry {
        let mut e = Entry::default();
        e.index = index;
        e.term = term;
        e.data = data.to_vec().into();
        e
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ltfsd-store-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("raft.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        path
    }

    #[test]
    fn term_vote_and_log_survive_a_restart() {
        let path = scratch("restart");
        {
            let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
            s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")]).unwrap();
            let mut hs = HardState::default();
            hs.term = 2;
            hs.vote = 3;
            s.set_hardstate(&hs).unwrap();
            s.set_commit(2).unwrap();
            // 日志回滚：从索引 3 起被新任期的条目覆盖
            s.append(&[entry(3, 3, b"c2")]).unwrap();
        }
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let st = s.raft_storage();
        let init = st.initial_state().unwrap();
        assert_eq!((init.hard_state.term, init.hard_state.vote, init.hard_state.commit), (2, 3, 2));
        assert_eq!(init.conf_state.voters, vec![1, 2, 3]);
        assert_eq!(st.last_index().unwrap(), 3);
        assert_eq!(st.term(3).unwrap(), 3);
        assert!(s.loaded_snapshot().is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 压缩之后：库里只剩快照点及其之后的条目，重启从快照恢复，回滚不得越过快照点。
    #[test]
    fn compaction_bounds_the_log_and_survives_a_restart() {
        let path = scratch("compact");
        {
            let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
            let ents: Vec<Entry> = (1..=10).map(|i| entry(i, 1, b"x")).collect();
            s.append(&ents).unwrap();
            s.set_commit(10).unwrap();
            assert!(s.due(&SnapshotPolicy { entries: 10, bytes: u64::MAX }));
            let snap = s.build_snapshot(8, b"control".to_vec()).unwrap();
            s.compact_to(&snap).unwrap();
            assert!(!s.due(&SnapshotPolicy { entries: 10, bytes: u64::MAX }), "计数在快照后归零");
            let n: i64 = s.db.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 3, "只剩索引 8、9、10");
            // 快照点之前的位置不允许被覆写
            assert!(s.append(&[entry(7, 2, b"y")]).is_err());
            // 快照点之后的尾部照常回滚
            s.append(&[entry(9, 2, b"y")]).unwrap();
            assert_eq!(s.raft_storage().last_index().unwrap(), 9);
        }
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        assert_eq!(s.snapshot_index(), 8);
        assert_eq!(s.loaded_snapshot().unwrap().get_data(), b"control");
        let st = s.raft_storage();
        assert_eq!(st.first_index().unwrap(), 9);
        assert_eq!(st.last_index().unwrap(), 9);
        assert_eq!(st.term(8).unwrap(), 1, "快照点的任期留着给匹配用");
        // 有快照时 raft-rs 要得到带数据的那一份
        assert_eq!(st.snapshot(0, 2).unwrap().get_data(), b"control");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 收到快照：本地日志整段作废，状态从快照点重建。
    #[test]
    fn installing_a_snapshot_rolls_the_whole_log_back() {
        let path = scratch("install");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b")]).unwrap();
        s.set_commit(2).unwrap();
        let mut snap = Snapshot::default();
        snap.set_data(b"from-leader".to_vec().into());
        let meta = snap.mut_metadata();
        meta.index = 50;
        meta.term = 7;
        meta.set_conf_state(ConfState::from((vec![1u64, 2, 3], vec![])));
        s.install_snapshot(&snap).unwrap();
        let n: i64 = s.db.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        assert_eq!(s.snapshot_index(), 50);
        let st = s.raft_storage();
        assert_eq!(st.first_index().unwrap(), 51);
        assert_eq!(st.last_index().unwrap(), 50);
        assert_eq!(st.initial_state().unwrap().hard_state.commit, 50);
        // 快照点之后接着追加
        s.append(&[entry(51, 7, b"c")]).unwrap();
        drop(s);
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        assert_eq!(s.snapshot_index(), 50);
        assert_eq!(s.raft_storage().last_index().unwrap(), 51);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 给不出快照时要记下请求，好让 Raft 循环现做一份。
    #[test]
    fn an_unavailable_snapshot_is_recorded_as_a_request() {
        let path = scratch("wanted");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        let st = s.raft_storage();
        assert!(st.snapshot(1, 2).is_err());
        assert_eq!(st.wanted(), Some(1));
        assert_eq!(st.wanted(), None, "请求只报一次");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
