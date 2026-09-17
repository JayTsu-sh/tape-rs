//! 经 Raft 复制的控制状态。这里只有"谁是当前执行者、轮次多少、是否已确认隔离"。
//! 所有节点按相同顺序应用相同条目，得到相同结论；应用过程不做任何设备 I/O。

use serde_json::{Value, json};

/// 写入 Raft 日志的命令。用 JSON 编码，便于排查和以后扩展。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 新 Leader 声明接管。它被提交时的日志索引就是该执行者的轮次。
    Takeover { node: u64, term: u64 },
    /// 执行者报告：本轮已对全部设备完成隔离并回读确认。
    Fenced { node: u64, round: u64 },
    /// 执行者放弃本轮（隔离失败、失去预留、计划移交）。
    Abandoned { node: u64, round: u64, reason: String },
    /// 新建磁带池。UUID 是身份，名称只是标签（LTFS 2.5.1 F.4）。
    PoolCreate { uuid: String, name: String, file_limit: u64 },
    /// 把一盘带（按条码）归入某个池。
    TapeAssign { barcode: String, pool: String },
    /// 解除归属。
    TapeUnassign { barcode: String },
    /// 磁带的追加资格或健康发生变化（执行者在写满、到文件数上限、池标记不符等时上报）。
    TapeState { barcode: String, state: String },
    /// 一次卷提交的目录记录的一片。先进暂存，等同一批的 `TapeCommitted` 到达才可见。
    CatalogPart { barcode: String, generation: u64, part: u32, files: Vec<FileRec> },
    /// 一次卷提交的摘要。应用它时，若 `parts` 片齐全，该批目录记录原子地并入目录。
    /// `full` 为真表示这批是该带的完整列表（接管时对账用），并入前先清掉该带的旧记录。
    TapeCommitted { barcode: String, volume_uuid: String, generation: u64, files: u64, bytes_used: u64, parts: u32, full: bool },
}

/// 目录里的一条文件记录。它只是"该带第 G 代索引里有这个文件"的缓存，磁带才是权威。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRec {
    pub path: String,
    pub length: u64,
    /// 十六进制小写；没有哈希属性的文件为空串
    pub sha256: String,
}

/// 目录条目单片的大小上限（字节，按 JSON 编码后估算）。
pub const CATALOG_PART_BYTES: usize = 1 << 20;

/// 把一批文件记录切成若干片，每片编码后不超过约 1 MiB。
pub fn split_catalog(files: &[FileRec]) -> Vec<Vec<FileRec>> {
    let mut out = vec![Vec::new()];
    let mut size = 0usize;
    for f in files {
        let cost = f.path.len() + f.sha256.len() + 48;
        if size + cost > CATALOG_PART_BYTES && !out.last().is_some_and(Vec::is_empty) {
            out.push(Vec::new());
            size = 0;
        }
        size += cost;
        out.last_mut().expect("非空").push(f.clone());
    }
    out
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let v = match self {
            Command::Takeover { node, term } => json!({"op": "takeover", "node": node, "term": term}),
            Command::Fenced { node, round } => json!({"op": "fenced", "node": node, "round": round}),
            Command::Abandoned { node, round, reason } => {
                json!({"op": "abandoned", "node": node, "round": round, "reason": reason})
            }
            Command::PoolCreate { uuid, name, file_limit } => {
                json!({"op": "pool_create", "uuid": uuid, "name": name, "file_limit": file_limit})
            }
            Command::TapeAssign { barcode, pool } => json!({"op": "tape_assign", "barcode": barcode, "pool": pool}),
            Command::TapeUnassign { barcode } => json!({"op": "tape_unassign", "barcode": barcode}),
            Command::TapeState { barcode, state } => json!({"op": "tape_state", "barcode": barcode, "state": state}),
            Command::CatalogPart { barcode, generation, part, files } => json!({
                "op": "catalog_part", "barcode": barcode, "generation": generation, "part": part,
                "files": files.iter().map(|f| json!([f.path, f.length, f.sha256])).collect::<Vec<_>>(),
            }),
            Command::TapeCommitted { barcode, volume_uuid, generation, files, bytes_used, parts, full } => json!({
                "op": "tape_committed", "barcode": barcode, "volume_uuid": volume_uuid, "generation": generation,
                "files": files, "bytes_used": bytes_used, "parts": parts, "full": full,
            }),
        };
        v.to_string().into_bytes()
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(data).ok()?;
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match v.get("op")?.as_str()? {
            "pool_create" => {
                return Some(Command::PoolCreate {
                    uuid: text("uuid")?,
                    name: text("name")?,
                    file_limit: v.get("file_limit")?.as_u64()?,
                });
            }
            "tape_assign" => return Some(Command::TapeAssign { barcode: text("barcode")?, pool: text("pool")? }),
            "tape_unassign" => return Some(Command::TapeUnassign { barcode: text("barcode")? }),
            "tape_state" => return Some(Command::TapeState { barcode: text("barcode")?, state: text("state")? }),
            "catalog_part" => {
                let files = v
                    .get("files")?
                    .as_array()?
                    .iter()
                    .map(|f| {
                        Some(FileRec { path: f.get(0)?.as_str()?.to_string(), length: f.get(1)?.as_u64()?, sha256: f.get(2)?.as_str()?.to_string() })
                    })
                    .collect::<Option<Vec<_>>>()?;
                return Some(Command::CatalogPart {
                    barcode: text("barcode")?,
                    generation: v.get("generation")?.as_u64()?,
                    part: v.get("part")?.as_u64()? as u32,
                    files,
                });
            }
            "tape_committed" => {
                return Some(Command::TapeCommitted {
                    barcode: text("barcode")?,
                    volume_uuid: text("volume_uuid")?,
                    generation: v.get("generation")?.as_u64()?,
                    files: v.get("files")?.as_u64()?,
                    bytes_used: v.get("bytes_used")?.as_u64()?,
                    parts: v.get("parts")?.as_u64()? as u32,
                    full: v.get("full")?.as_bool()?,
                });
            }
            _ => {}
        }
        let node = v.get("node")?.as_u64()?;
        match v.get("op")?.as_str()? {
            "takeover" => Some(Command::Takeover { node, term: v.get("term")?.as_u64()? }),
            "fenced" => Some(Command::Fenced { node, round: v.get("round")?.as_u64()? }),
            "abandoned" => Some(Command::Abandoned {
                node,
                round: v.get("round")?.as_u64()?,
                reason: v.get("reason")?.as_str()?.to_string(),
            }),
            _ => None,
        }
    }
}

/// 当前执行者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executor {
    pub node: u64,
    pub term: u64,
    /// 执行轮次 = `Takeover` 条目的日志索引。全局唯一、单调，写进预留键。
    pub round: u64,
    /// `Fenced` 已提交，且提交时它仍是最新的执行者。只有这时才允许恢复卷。
    pub fenced: bool,
}

/// 应用一条命令后，本地需要知道的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// 出现了新的执行者（可能是自己，也可能是别人）。
    NewExecutor(Executor),
    /// 某一轮的隔离得到确认，可以进入恢复。
    FenceConfirmed(Executor),
    /// `Fenced` 到达时该轮已被更新的 `Takeover` 取代，作废。
    FenceSuperseded { node: u64, round: u64 },
    /// 当前执行者放弃。
    ExecutorGone { node: u64, round: u64 },
    /// 一次卷提交的摘要被接受：目录库应当把对应的暂存记录并入目录。
    CatalogCommitted,
    /// 管理命令的结论。`Err` 表示被状态机拒绝（所有节点结论相同），状态未变。
    Admin(std::result::Result<String, String>),
    Nothing,
}

/// 磁带状态的取值。追加资格与健康合在一个字段里，首版够用（37 的四个维度里的两个）。
pub mod tape_state {
    /// 可以继续写
    pub const APPENDABLE: &str = "appendable";
    /// 文件数到上限：还有空间，但不再接收新文件
    pub const DATA_FULL: &str = "data_full";
    /// 空间用尽（扣除保留空间）
    pub const FULL: &str = "full";
    /// 卷根的池标记与归属不符，或带上是别的数据：不使用，等人处理
    pub const LABEL_MISMATCH: &str = "label_mismatch";
    /// 装载、挂载或格式化失败
    pub const CHECK: &str = "check";
    pub const ALL: &[&str] = &[APPENDABLE, DATA_FULL, FULL, LABEL_MISMATCH, CHECK];
}

/// 默认的每盘带文件数软上限。推导见研究笔记 pooling-slice-plan.md。
pub const DEFAULT_FILE_LIMIT: u64 = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    pub file_limit: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ControlState {
    pub executor: Option<Executor>,
    pub applied_index: u64,
    /// 历次接管的简要记录：(轮次, 节点)
    pub history: Vec<(u64, u64)>,
    /// 池 UUID → 池
    pub pools: std::collections::BTreeMap<String, Pool>,
    /// 条码 → 所属池的 UUID。不在这里的磁带即未归属，系统绝不触碰。
    pub tapes: std::collections::BTreeMap<String, String>,
    /// 条码 → 磁带状态。缺省即 appendable。取值见 `tape_state` 模块。
    pub tape_state: std::collections::BTreeMap<String, String>,
    /// 条码 → 最近一次卷提交的摘要。文件记录本身只在目录库里，不放内存。
    pub tape_summary: std::collections::BTreeMap<String, TapeSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TapeSummary {
    pub volume_uuid: String,
    pub generation: u64,
    pub files: u64,
    pub bytes_used: u64,
}

impl ControlState {
    pub fn apply(&mut self, index: u64, cmd: &Command) -> Applied {
        self.applied_index = index;
        match cmd {
            Command::Takeover { node, term } => {
                let e = Executor { node: *node, term: *term, round: index, fenced: false };
                self.history.push((index, *node));
                self.executor = Some(e.clone());
                Applied::NewExecutor(e)
            }
            Command::Fenced { node, round } => match self.executor.as_mut() {
                Some(e) if e.round == *round && e.node == *node => {
                    e.fenced = true;
                    Applied::FenceConfirmed(e.clone())
                }
                _ => Applied::FenceSuperseded { node: *node, round: *round },
            },
            Command::Abandoned { node, round, .. } => match &self.executor {
                Some(e) if e.round == *round && e.node == *node => {
                    self.executor = None;
                    Applied::ExecutorGone { node: *node, round: *round }
                }
                _ => Applied::Nothing,
            },
            Command::PoolCreate { uuid, name, file_limit } => Applied::Admin(self.pool_create(uuid, name, *file_limit)),
            Command::TapeAssign { barcode, pool } => Applied::Admin(self.tape_assign(barcode, pool)),
            Command::TapeUnassign { barcode } => Applied::Admin(self.tape_unassign(barcode)),
            Command::TapeState { barcode, state } => {
                if self.tapes.contains_key(barcode) && tape_state::ALL.contains(&state.as_str()) {
                    self.tape_state.insert(barcode.clone(), state.clone());
                }
                Applied::Nothing
            }
            // 文件记录进目录库的暂存表，这里不处理
            Command::CatalogPart { .. } => Applied::Nothing,
            Command::TapeCommitted { barcode, volume_uuid, generation, files, bytes_used, .. } => {
                // 只接受已归属的带，且代数不回退：迟到的旧批次不能把摘要改回去
                let newer = self.tape_summary.get(barcode).is_none_or(|s| *generation >= s.generation);
                if self.tapes.contains_key(barcode) && newer {
                    self.tape_summary.insert(
                        barcode.clone(),
                        TapeSummary { volume_uuid: volume_uuid.clone(), generation: *generation, files: *files, bytes_used: *bytes_used },
                    );
                    Applied::CatalogCommitted
                } else {
                    Applied::Nothing
                }
            }
        }
    }
}

impl ControlState {
    fn pool_create(&mut self, uuid: &str, name: &str, file_limit: u64) -> std::result::Result<String, String> {
        if name.is_empty() || name.contains('/') {
            return Err(format!("池名非法: {:?}", name));
        }
        if self.pools.contains_key(uuid) {
            return Err(format!("池 UUID 已存在: {}", uuid));
        }
        if self.pools.values().any(|p| p.name == name) {
            return Err(format!("池名已存在: {}", name));
        }
        if file_limit == 0 {
            return Err("文件数上限必须大于 0".to_string());
        }
        self.pools.insert(uuid.to_string(), Pool { name: name.to_string(), file_limit });
        Ok(uuid.to_string())
    }

    fn tape_unassign(&mut self, barcode: &str) -> std::result::Result<String, String> {
        if !self.tapes.contains_key(barcode) {
            return Err(format!("{} 未归属任何池", barcode));
        }
        // 37 的前提：带上还有目录记录的文件时不能解除归属（否则这些文件从池里消失）
        if let Some(s) = self.tape_summary.get(barcode).filter(|s| s.files > 0) {
            return Err(format!("{} 上还有 {} 个文件，不能解除归属", barcode, s.files));
        }
        self.tapes.remove(barcode);
        self.tape_summary.remove(barcode);
        self.tape_state.remove(barcode);
        Ok(format!("{} 已解除归属", barcode))
    }

    /// `pool` 可以是池名或 UUID。
    pub fn resolve_pool(&self, pool: &str) -> Option<String> {
        if self.pools.contains_key(pool) {
            return Some(pool.to_string());
        }
        self.pools.iter().find(|(_, p)| p.name == pool).map(|(u, _)| u.clone())
    }

    fn tape_assign(&mut self, barcode: &str, pool: &str) -> std::result::Result<String, String> {
        if barcode.is_empty() {
            return Err("条码为空".to_string());
        }
        let uuid = self.resolve_pool(pool).ok_or_else(|| format!("没有这个池: {}", pool))?;
        if let Some(cur) = self.tapes.get(barcode) {
            let cur_name = self.pools.get(cur).map(|p| p.name.as_str()).unwrap_or("?");
            return Err(format!("{} 已归属池 {}", barcode, cur_name));
        }
        self.tapes.insert(barcode.to_string(), uuid);
        Ok(format!("{} 已归入池 {}", barcode, self.pools[&self.tapes[barcode]].name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_roundtrip() {
        for c in [
            Command::Takeover { node: 2, term: 7 },
            Command::Fenced { node: 2, round: 41 },
            Command::Abandoned { node: 2, round: 41, reason: "预留被抢占".into() },
        ] {
            assert_eq!(Command::decode(&c.encode()), Some(c));
        }
        assert_eq!(Command::decode(b"{}"), None);
    }

    #[test]
    fn round_is_the_log_index_of_the_takeover() {
        let mut s = ControlState::default();
        let a = s.apply(5, &Command::Takeover { node: 1, term: 2 });
        assert_eq!(a, Applied::NewExecutor(Executor { node: 1, term: 2, round: 5, fenced: false }));
        assert!(matches!(s.apply(6, &Command::Fenced { node: 1, round: 5 }), Applied::FenceConfirmed(e) if e.fenced));
    }

    /// 隔离途中换届：B 的 Fenced 在 C 的 Takeover 之后才被应用，必须作废。
    #[test]
    fn fenced_after_a_newer_takeover_is_superseded() {
        let mut s = ControlState::default();
        s.apply(5, &Command::Takeover { node: 2, term: 2 });
        s.apply(6, &Command::Takeover { node: 3, term: 3 });
        assert_eq!(
            s.apply(7, &Command::Fenced { node: 2, round: 5 }),
            Applied::FenceSuperseded { node: 2, round: 5 }
        );
        assert_eq!(s.executor.as_ref().map(|e| (e.node, e.round, e.fenced)), Some((3, 6, false)));
        // 迟到的放弃也不影响新执行者
        assert_eq!(s.apply(8, &Command::Abandoned { node: 2, round: 5, reason: String::new() }), Applied::Nothing);
        assert!(s.executor.is_some());
    }

    #[test]
    fn admin_commands_roundtrip_and_are_validated_deterministically() {
        for c in [
            Command::PoolCreate { uuid: "u-1".into(), name: "archive".into(), file_limit: 200_000 },
            Command::TapeAssign { barcode: "TR8000L08".into(), pool: "archive".into() },
            Command::TapeUnassign { barcode: "TR8000L08".into() },
        ] {
            assert_eq!(Command::decode(&c.encode()), Some(c));
        }
        let mut s = ControlState::default();
        let mk = |u: &str, n: &str| Command::PoolCreate { uuid: u.into(), name: n.into(), file_limit: 10 };
        assert_eq!(s.apply(1, &mk("u-1", "archive")), Applied::Admin(Ok("u-1".into())));
        assert!(matches!(s.apply(2, &mk("u-2", "archive")), Applied::Admin(Err(_))), "重名");
        assert!(matches!(s.apply(3, &mk("u-1", "other")), Applied::Admin(Err(_))), "UUID 重复");
        assert!(matches!(s.apply(4, &Command::TapeAssign { barcode: "T1".into(), pool: "nope".into() }), Applied::Admin(Err(_))));
        // 池名或 UUID 都可以
        assert!(matches!(s.apply(5, &Command::TapeAssign { barcode: "T1".into(), pool: "archive".into() }), Applied::Admin(Ok(_))));
        assert!(matches!(s.apply(6, &Command::TapeAssign { barcode: "T2".into(), pool: "u-1".into() }), Applied::Admin(Ok(_))));
        assert!(matches!(s.apply(7, &Command::TapeAssign { barcode: "T1".into(), pool: "archive".into() }), Applied::Admin(Err(_))), "已归属");
        assert_eq!(s.tapes.len(), 2);
        assert!(matches!(s.apply(8, &Command::TapeUnassign { barcode: "T1".into() }), Applied::Admin(Ok(_))));
        assert!(matches!(s.apply(9, &Command::TapeUnassign { barcode: "T1".into() }), Applied::Admin(Err(_))));
        assert_eq!(s.tapes.keys().collect::<Vec<_>>(), vec!["T2"]);
        // 被拒绝的命令不改变状态
        assert_eq!(s.pools.len(), 1);
    }

    #[test]
    fn catalog_commands_roundtrip_and_split() {
        let files: Vec<FileRec> = (0..10_000)
            .map(|i| FileRec { path: format!("/dir/sub/object-{:08}.bin", i), length: i, sha256: "ab".repeat(32) })
            .collect();
        let parts = split_catalog(&files);
        assert!(parts.len() >= 2, "一万条记录超过 1 MiB，应当分片");
        assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), files.len());
        for (i, p) in parts.iter().enumerate() {
            let c = Command::CatalogPart { barcode: "T1".into(), generation: 7, part: i as u32, files: p.clone() };
            let enc = c.encode();
            assert!(enc.len() < 2 * CATALOG_PART_BYTES, "{}", enc.len());
            assert_eq!(Command::decode(&enc), Some(c));
        }
        assert_eq!(split_catalog(&[]), vec![Vec::<FileRec>::new()]);
        let t = Command::TapeCommitted { barcode: "T1".into(), volume_uuid: "v".into(), generation: 7, files: 3, bytes_used: 9, parts: 2, full: true };
        assert_eq!(Command::decode(&t.encode()), Some(t));
    }

    #[test]
    fn summaries_only_move_forward_and_block_unassign() {
        let mut s = ControlState::default();
        s.apply(1, &Command::PoolCreate { uuid: "u".into(), name: "p".into(), file_limit: 10 });
        let commit = |g: u64, files: u64| Command::TapeCommitted { barcode: "T1".into(), volume_uuid: "v".into(), generation: g, files, bytes_used: 1, parts: 1, full: false };
        assert_eq!(s.apply(2, &commit(5, 1)), Applied::Nothing, "未归属的带不接受");
        s.apply(3, &Command::TapeAssign { barcode: "T1".into(), pool: "p".into() });
        assert_eq!(s.apply(4, &commit(5, 2)), Applied::CatalogCommitted);
        assert_eq!(s.apply(5, &commit(4, 1)), Applied::Nothing, "代数回退的迟到批次被忽略");
        assert_eq!(s.tape_summary["T1"].generation, 5);
        assert!(matches!(s.apply(6, &Command::TapeUnassign { barcode: "T1".into() }), Applied::Admin(Err(_))));
        assert_eq!(s.apply(7, &commit(6, 0)), Applied::CatalogCommitted);
        assert!(matches!(s.apply(8, &Command::TapeUnassign { barcode: "T1".into() }), Applied::Admin(Ok(_))));
    }
}
