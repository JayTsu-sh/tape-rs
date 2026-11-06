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
        Self {
            entries: 2_000,
            bytes: 256 << 20,
        }
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
        Self {
            mem,
            latest: Arc::new(Mutex::new(None)),
            wanted: Arc::new(AtomicU64::new(0)),
        }
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
                self.wanted
                    .fetch_max(request_index.max(1), Ordering::SeqCst);
                Err(raft::Error::Store(
                    StorageError::SnapshotTemporarilyUnavailable,
                ))
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

fn fixed_voters(conf: &ConfState) -> Result<Vec<u64>> {
    let mut members = conf.voters.clone();
    members.sort_unstable();
    if members.is_empty()
        || members[0] == 0
        || members.windows(2).any(|ids| ids[0] == ids[1])
        || !conf.voters_outgoing.is_empty()
        || !conf.learners.is_empty()
        || !conf.learners_next.is_empty()
        || conf.auto_leave
    {
        return Err(TapeError::Ltfs(
            "当前控制存储仅支持非空、唯一的固定投票成员".into(),
        ));
    }
    Ok(members)
}

impl RaftStore {
    /// 本机进程代数，不属于 Raft 日志；安装快照、日志回滚不能把它退回旧值。
    /// 复制/回滚整个 data-dir 不受此计数保护，部署必须保留独占目录和节点身份。
    pub fn next_instance(&mut self) -> Result<u64> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS local_instance (id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);"
        ).map_err(db_err)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let old: i64 = tx
            .query_row("SELECT value FROM local_instance WHERE id=1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_err)?
            .unwrap_or(0);
        let next = old
            .checked_add(1)
            .filter(|v| *v > 0)
            .ok_or_else(|| TapeError::Ltfs("本机执行实例计数溢出".into()))?;
        tx.execute("INSERT INTO local_instance(id,value) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value", [next]).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(next as u64)
    }

    /// 打开（或创建）控制存储，并把已有状态装入内存。`voters` 是固定的三节点成员。
    pub fn open(path: &Path, voters: &[u64]) -> Result<Self> {
        let expected = fixed_voters(&ConfState::from((voters.to_vec(), vec![])))?;
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS hardstate (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (idx INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS snapshot (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS membership (id INTEGER PRIMARY KEY CHECK (id = 1), data BLOB NOT NULL);",
        ).map_err(db_err)?;
        let snapshot: Option<Vec<u8>> = db
            .query_row("SELECT data FROM snapshot WHERE id=1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        let snapshot = snapshot
            .map(|bytes| {
                Snapshot::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的快照损坏: {e}")))
            })
            .transpose()?;
        let initial: Option<Vec<u8>> = db
            .query_row("SELECT data FROM membership WHERE id=1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        let initial = initial
            .map(|bytes| {
                ConfState::parse_from_bytes(&bytes)
                    .map_err(pb_err)
                    .and_then(|conf| fixed_voters(&conf))
            })
            .transpose()?;
        let voters = if let Some(snap) = &snapshot {
            let members = fixed_voters(snap.get_metadata().get_conf_state())?;
            if initial.as_ref().is_some_and(|initial| *initial != members) {
                return Err(TapeError::Ltfs("快照与初始持久成员不一致".into()));
            }
            members
        } else if let Some(initial) = initial {
            initial
        } else {
            let nonempty: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM hardstate) OR EXISTS(SELECT 1 FROM entries)",
                    [],
                    |r| r.get(0),
                )
                .map_err(db_err)?;
            if nonempty {
                return Err(TapeError::Ltfs(
                    "非空控制存储缺少初始成员或快照证据，拒绝猜测成员".into(),
                ));
            }
            let conf = ConfState::from((expected.clone(), vec![]));
            db.execute(
                "INSERT INTO membership(id,data) VALUES(1,?1)",
                [conf.write_to_bytes().map_err(pb_err)?],
            )
            .map_err(db_err)?;
            expected.clone()
        };
        if voters != expected {
            return Err(TapeError::Ltfs(
                "启动成员与持久成员不一致；不支持通过 CLI 重配成员".into(),
            ));
        }
        let mem = MemStorage::new_with_conf_state(ConfState::from((voters.clone(), vec![])));
        let store = ControlStorage::new(mem);
        let mut snapshot_index = 0;
        let mut loaded = None;
        if let Some(snap) = snapshot {
            snapshot_index = snap.get_metadata().index;
            store
                .mem
                .wl()
                .apply_snapshot(snap.clone())
                .map_err(|e| TapeError::Ltfs(format!("装入快照失败: {e}")))?;
            store.publish(snap.clone());
            loaded = Some(snap);
        }
        let mut entries = Vec::new();
        {
            let mut stmt = db
                .prepare("SELECT data FROM entries WHERE idx > ?1 ORDER BY idx")
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![snapshot_index as i64], |r| r.get::<_, Vec<u8>>(0))
                .map_err(db_err)?;
            for row in rows {
                let bytes = row.map_err(db_err)?;
                entries.push(
                    Entry::parse_from_bytes(&bytes)
                        .map_err(|e| TapeError::Ltfs(format!("控制存储里的日志条目损坏: {e}")))?,
                );
            }
        }
        let hs: Option<Vec<u8>> = db
            .query_row("SELECT data FROM hardstate WHERE id=1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        {
            let mut memory = store.mem.wl();
            if !entries.is_empty() {
                memory
                    .append(&entries)
                    .map_err(|e| TapeError::Ltfs(format!("装入日志失败: {e}")))?;
            }
            if let Some(bytes) = hs {
                let hs = HardState::parse_from_bytes(&bytes)
                    .map_err(|e| TapeError::Ltfs(format!("控制存储里的 HardState 损坏: {e}")))?;
                let last = entries.last().map_or(snapshot_index, |e| e.index);
                if hs.commit < snapshot_index || hs.commit > last {
                    return Err(TapeError::Ltfs(
                        "持久 HardState 提交位置超出日志/快照边界".into(),
                    ));
                }
                memory.set_hardstate(hs);
            }
        }
        let since_bytes = entries.iter().map(|e| e.data.len() as u64).sum();
        Ok(Self {
            db,
            store,
            voters,
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

    /// 一轮 Ready 必须整体落盘；失败时磁盘与内存都保持原像。
    pub fn persist_ready(
        &mut self,
        snapshot: Option<&Snapshot>,
        entries: &[Entry],
        hardstate: Option<&HardState>,
    ) -> Result<()> {
        let snapshot = snapshot.filter(|s| !s.is_empty());
        if snapshot.is_none() && entries.is_empty() && hardstate.is_none() {
            return Ok(());
        }
        let previous = self.store.mem.rl().hard_state().clone();
        let old_last = self
            .store
            .last_index()
            .map_err(|e| TapeError::Ltfs(e.to_string()))?;
        let boundary = snapshot.map_or(self.snapshot_index, |s| s.get_metadata().index);
        if let Some(snap) = snapshot
            && (boundary <= self.snapshot_index
                || boundary < previous.commit
                || fixed_voters(snap.get_metadata().get_conf_state())? != self.voters)
        {
            return Err(TapeError::Ltfs("快照边界或持久成员无效".into()));
        }
        if let Some(first) = entries.first() {
            let before = snapshot.map_or(old_last, |_| boundary);
            if first.index <= boundary
                || first.index <= previous.commit
                || first.index > before.saturating_add(1)
                || entries
                    .windows(2)
                    .any(|pair| pair[0].index.checked_add(1) != Some(pair[1].index))
                || entries.iter().any(|entry| entry.index > i64::MAX as u64)
            {
                return Err(TapeError::Ltfs(
                    "日志不连续或试图覆写已提交/快照历史".into(),
                ));
            }
        }
        let last = entries
            .last()
            .map_or(snapshot.map_or(old_last, |_| boundary), |e| e.index);
        let mut hs = hardstate.cloned().unwrap_or_else(|| previous.clone());
        if let Some(snap) = snapshot.filter(|_| hardstate.is_none()) {
            hs.commit = boundary;
            if hs.term < snap.get_metadata().term {
                hs.term = snap.get_metadata().term;
                hs.vote = 0;
            }
        }
        if hs.term < previous.term
            || hs.commit < previous.commit
            || hs.commit < boundary
            || hs.commit > last
            || (hs.term == previous.term && previous.vote != 0 && hs.vote != previous.vote)
        {
            return Err(TapeError::Ltfs("HardState 任期、投票或提交位置无效".into()));
        }
        let tx = self.db.transaction().map_err(db_err)?;
        if let Some(snap) = snapshot {
            tx.execute("DELETE FROM entries", []).map_err(db_err)?;
            tx.execute("INSERT INTO snapshot(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                [snap.write_to_bytes().map_err(pb_err)?]).map_err(db_err)?;
        }
        if let Some(first) = entries.first() {
            tx.execute("DELETE FROM entries WHERE idx>=?1", [first.index as i64])
                .map_err(db_err)?;
            for entry in entries {
                tx.execute(
                    "INSERT INTO entries(idx,data) VALUES(?1,?2)",
                    params![entry.index as i64, entry.write_to_bytes().map_err(pb_err)?],
                )
                .map_err(db_err)?;
            }
        }
        let write_hs = hardstate.is_some() || snapshot.is_some();
        if write_hs {
            tx.execute("INSERT INTO hardstate(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                [hs.write_to_bytes().map_err(pb_err)?]).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        {
            let mut memory = self.store.mem.wl();
            if let Some(snap) = snapshot {
                memory
                    .apply_snapshot(snap.clone())
                    .map_err(|e| TapeError::Ltfs(format!("装入快照失败: {e}")))?;
            }
            if !entries.is_empty() {
                memory
                    .append(entries)
                    .map_err(|e| TapeError::Ltfs(format!("追加日志失败: {e}")))?;
            }
            if write_hs {
                memory.set_hardstate(hs);
            }
        }
        if let Some(snap) = snapshot {
            self.finish_snapshot(snap, boundary);
        }
        self.since_entries += entries.len() as u64;
        self.since_bytes += entries.iter().map(|e| e.data.len() as u64).sum::<u64>();
        Ok(())
    }

    pub fn append(&mut self, entries: &[Entry]) -> Result<()> {
        self.persist_ready(None, entries, None)
    }

    pub fn set_hardstate(&mut self, hs: &HardState) -> Result<()> {
        self.persist_ready(None, &[], Some(hs))
    }

    pub fn set_commit(&mut self, commit: u64) -> Result<()> {
        let mut hs = self.store.mem.rl().hard_state().clone();
        hs.commit = commit;
        self.persist_ready(None, &[], Some(&hs))
    }

    /// 用当前状态做一份覆盖到 `index` 的快照。`data` 是状态机自己的内容。
    pub fn build_snapshot(&self, index: u64, data: Vec<u8>) -> Result<Snapshot> {
        let term = self
            .store
            .term(index)
            .map_err(|e| TapeError::Ltfs(format!("取索引 {} 的任期失败: {}", index, e)))?;
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
        tx.execute("DELETE FROM entries WHERE idx < ?1", params![index as i64])
            .map_err(db_err)?;
        tx.execute(
            "INSERT INTO snapshot (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![bytes],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        self.store
            .mem
            .wl()
            .compact(index)
            .map_err(|e| TapeError::Ltfs(format!("压缩日志失败: {}", e)))?;
        self.finish_snapshot(snap, index);
        Ok(())
    }

    /// 独立安装快照也必须把其恢复边界和 HardState 一起提交。
    pub fn install_snapshot(&mut self, snap: &Snapshot) -> Result<()> {
        self.persist_ready(Some(snap), &[], None)
    }

    fn finish_snapshot(&mut self, snap: &Snapshot, index: u64) {
        self.snapshot_index = index;
        self.since_entries = 0;
        self.since_bytes = 0;
        self.store.publish(snap.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u64, term: u64, data: &[u8]) -> Entry {
        Entry {
            index,
            term,
            data: data.to_vec().into(),
            ..Default::default()
        }
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
            s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")])
                .unwrap();
            let hs = HardState {
                term: 2,
                vote: 3,
                ..Default::default()
            };
            s.set_hardstate(&hs).unwrap();
            s.set_commit(2).unwrap();
            // 日志回滚：从索引 3 起被新任期的条目覆盖
            s.append(&[entry(3, 3, b"c2")]).unwrap();
        }
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let st = s.raft_storage();
        let init = st.initial_state().unwrap();
        assert_eq!(
            (
                init.hard_state.term,
                init.hard_state.vote,
                init.hard_state.commit
            ),
            (2, 3, 2)
        );
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
            s.set_commit(8).unwrap();
            assert!(s.due(&SnapshotPolicy {
                entries: 10,
                bytes: u64::MAX
            }));
            let snap = s.build_snapshot(8, b"control".to_vec()).unwrap();
            s.compact_to(&snap).unwrap();
            assert!(
                !s.due(&SnapshotPolicy {
                    entries: 10,
                    bytes: u64::MAX
                }),
                "计数在快照后归零"
            );
            let n: i64 =
                s.db.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
                    .unwrap();
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
        let n: i64 =
            s.db.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
                .unwrap();
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

    #[test]
    fn snapshot_hardstate_write_failure_preserves_the_previous_durable_image() {
        let path = scratch("snapshot-atomic");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b")]).unwrap();
        s.set_hardstate(&HardState {
            term: 7,
            vote: 2,
            commit: 2,
            ..Default::default()
        })
        .unwrap();
        s.db.execute_batch("CREATE TRIGGER fail_hardstate BEFORE INSERT ON hardstate BEGIN SELECT RAISE(ABORT, 'injected hardstate failure'); END;").unwrap();
        let mut snap = Snapshot::default();
        snap.mut_metadata().set_index(50);
        snap.mut_metadata().set_term(7);
        snap.mut_metadata()
            .set_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
        assert!(s.install_snapshot(&snap).is_err());
        let in_memory = s.raft_storage().initial_state().unwrap().hard_state.commit;
        drop(s);
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let restored = (
            s.snapshot_index(),
            s.raft_storage().last_index().unwrap(),
            s.raft_storage().initial_state().unwrap().hard_state.commit,
        );
        drop(s);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(in_memory, 2, "落盘失败不得提前改变内存状态");
        assert_eq!(restored, (0, 2, 2), "快照和 HardState 必须一起回滚");
    }

    #[test]
    fn initial_voters_cannot_be_changed_before_the_first_snapshot() {
        let path = scratch("initial-voters");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        drop(s);
        let changed = RaftStore::open(&path, &[1, 4, 5]);
        let rejected = changed.is_err();
        drop(changed);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(rejected, "重启不能用 CLI 静默替换持久成员");
    }

    #[test]
    fn snapshot_voters_must_match_the_startup_configuration() {
        let path = scratch("snapshot-voters");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        s.set_commit(1).unwrap();
        let snap = s.build_snapshot(1, b"control".to_vec()).unwrap();
        s.compact_to(&snap).unwrap();
        drop(s);
        let changed = RaftStore::open(&path, &[1, 4, 5]);
        let rejected = changed.is_err();
        drop(changed);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(rejected, "CLI 必须匹配快照中的真实 ConfState");
    }

    #[test]
    fn a_nonempty_store_without_membership_evidence_is_rejected() {
        let path = scratch("membership-evidence");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        s.db.execute_batch("DROP TABLE IF EXISTS membership;")
            .unwrap();
        drop(s);
        let reopened = RaftStore::open(&path, &[1, 2, 3]);
        let rejected = reopened.is_err();
        drop(reopened);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(rejected, "历史成员缺失时不得猜测当前 CLI 就是原成员");
    }
    #[test]
    fn a_snapshot_proves_membership_without_creating_an_initial_record() {
        let path = scratch("snapshot-evidence");
        let mut s = RaftStore::open(&path, &[3, 1, 2]).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        s.set_commit(1).unwrap();
        let snapshot = s.build_snapshot(1, b"control".to_vec()).unwrap();
        s.compact_to(&snapshot).unwrap();
        s.db.execute_batch("DROP TABLE membership;").unwrap();
        drop(s);
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        assert_eq!(
            s.raft_storage().initial_state().unwrap().conf_state.voters,
            vec![1, 2, 3]
        );
        let records: u64 =
            s.db.query_row("SELECT COUNT(*) FROM membership", [], |row| row.get(0))
                .unwrap();
        drop(s);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(records, 0, "已有快照直接作为证据，不自动补造初始成员");
    }

    #[test]
    fn hardstate_term_vote_and_commit_cannot_regress() {
        let path = scratch("hardstate-monotonic");
        let mut s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 7, b"c")])
            .unwrap();
        s.set_hardstate(&HardState {
            term: 7,
            vote: 2,
            commit: 2,
            ..Default::default()
        })
        .unwrap();
        for (term, vote, commit) in [(6, 2, 2), (7, 3, 2), (7, 0, 2), (7, 2, 1), (8, 1, 4)] {
            assert!(
                s.set_hardstate(&HardState {
                    term,
                    vote,
                    commit,
                    ..Default::default()
                })
                .is_err()
            );
        }
        let state = s.raft_storage().initial_state().unwrap().hard_state;
        assert_eq!((state.term, state.vote, state.commit), (7, 2, 2));
        s.set_hardstate(&HardState {
            term: 8,
            vote: 1,
            commit: 3,
            ..Default::default()
        })
        .unwrap();
        drop(s);
        let s = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let state = s.raft_storage().initial_state().unwrap().hard_state;
        drop(s);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!((state.term, state.vote, state.commit), (8, 1, 3));
    }
}
