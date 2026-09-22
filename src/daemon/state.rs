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
    /// 开始回收一盘带：把它上面还活着的文件搬到同池的其他带上，然后重新格式化它。
    /// 只改状态；真正的搬迁由执行者做，中途换届也能从状态里接着干。
    TapeReclaim { barcode: String },
    /// 回收完成：源带已重新格式化。这是唯一允许摘要代数回退的地方（带真的被重写了）。
    TapeReclaimed { barcode: String, volume_uuid: String },
    /// 一次卷提交的目录记录的一片。先进暂存，等同一批的 `TapeCommitted` 到达才可见。
    CatalogPart { barcode: String, generation: u64, part: u32, files: Vec<FileRec> },
    /// 一次卷提交的摘要。应用它时，若 `parts` 片齐全，该批目录记录原子地并入目录。
    /// `full` 为真表示这批是该带的完整列表（接管时对账用），并入前先清掉该带的旧记录。
    TapeCommitted {
        barcode: String,
        volume_uuid: String,
        generation: u64,
        files: u64,
        bytes_used: u64,
        /// 带上实际写掉的字节；读不到容量时为 0
        bytes_written: u64,
        parts: u32,
        full: bool,
    },
}

/// 目录里的一条文件记录。它只是"该带第 G 代索引里有这个文件"的缓存，磁带才是权威。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRec {
    pub path: String,
    pub length: u64,
    /// 十六进制小写；没有哈希属性的文件为空串
    pub sha256: String,
    /// 这一份内容的版本 (轮次, 序号)，取自带上的 `tapers.version`。见 `ltfs::volume::XATTR_VERSION`。
    /// 目录按它取大者：同一路径的多份物理副本（重写、回收搬迁）里只有最新的一份算数。
    pub version: (u64, u64),
}

/// 目录条目单片的大小上限（字节，按 JSON 编码后估算）。
pub const CATALOG_PART_BYTES: usize = 1 << 20;

/// 把一批文件记录切成若干片，每片编码后不超过约 1 MiB。
pub fn split_catalog(files: &[FileRec]) -> Vec<Vec<FileRec>> {
    let mut out = vec![Vec::new()];
    let mut size = 0usize;
    for f in files {
        let cost = f.path.len() + f.sha256.len() + 72;
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
            Command::TapeReclaim { barcode } => json!({"op": "tape_reclaim", "barcode": barcode}),
            Command::TapeReclaimed { barcode, volume_uuid } => {
                json!({"op": "tape_reclaimed", "barcode": barcode, "volume_uuid": volume_uuid})
            }
            Command::CatalogPart { barcode, generation, part, files } => json!({
                "op": "catalog_part", "barcode": barcode, "generation": generation, "part": part,
                "files": files.iter().map(|f| json!([f.path, f.length, f.sha256, f.version.0, f.version.1])).collect::<Vec<_>>(),
            }),
            Command::TapeCommitted { barcode, volume_uuid, generation, files, bytes_used, bytes_written, parts, full } => json!({
                "op": "tape_committed", "barcode": barcode, "volume_uuid": volume_uuid, "generation": generation,
                "files": files, "bytes_used": bytes_used, "bytes_written": bytes_written, "parts": parts, "full": full,
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
            "tape_reclaim" => return Some(Command::TapeReclaim { barcode: text("barcode")? }),
            "tape_reclaimed" => {
                return Some(Command::TapeReclaimed { barcode: text("barcode")?, volume_uuid: text("volume_uuid")? });
            }
            "catalog_part" => {
                let files = v
                    .get("files")?
                    .as_array()?
                    .iter()
                    .map(|f| {
                        Some(FileRec {
                            path: f.get(0)?.as_str()?.to_string(),
                            length: f.get(1)?.as_u64()?,
                            sha256: f.get(2)?.as_str()?.to_string(),
                            version: (f.get(3).and_then(Value::as_u64).unwrap_or(0), f.get(4).and_then(Value::as_u64).unwrap_or(0)),
                        })
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
                    bytes_written: v.get("bytes_written").and_then(Value::as_u64).unwrap_or(0),
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

/// 快照里的内容：整份控制状态，加上"目录库到哪里去拉"。
///
/// 目录库（文件记录）可以到几百 MB，塞不进 Raft 消息，所以快照里只放一个指针：
/// 生成这份快照的节点编号，以及它的目录库快照覆盖到的日志索引。接收方经节点间通道
/// 另行拉取那个文件（`net::DirectoryFetch`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlSnapshot {
    pub control: ControlState,
    /// 生成这份快照的节点：目录库要从它那里拉
    pub source: u64,
    /// 它的目录库快照覆盖到哪个日志索引；0 表示该节点没有目录库
    pub directory_index: u64,
}

impl ControlSnapshot {
    pub fn encode(&self) -> Vec<u8> {
        let c = &self.control;
        json!({
            "source": self.source,
            "directory_index": self.directory_index,
            "applied_index": c.applied_index,
            "executor": c.executor.as_ref().map(|e| json!([e.node, e.term, e.round, e.fenced])),
            "history": c.history.iter().map(|(r, n)| json!([r, n])).collect::<Vec<_>>(),
            "pools": c.pools.iter().map(|(u, p)| json!([u, p.name, p.file_limit])).collect::<Vec<_>>(),
            "tapes": c.tapes.iter().map(|(b, u)| json!([b, u])).collect::<Vec<_>>(),
            "tape_state": c.tape_state.iter().map(|(b, s)| json!([b, s])).collect::<Vec<_>>(),
            "tape_summary": c
                .tape_summary
                .iter()
                .map(|(b, s)| json!([b, s.volume_uuid, s.generation, s.files, s.bytes_used, s.bytes_written]))
                .collect::<Vec<_>>(),
        })
        .to_string()
        .into_bytes()
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(data).ok()?;
        let rows = |k: &str| -> Option<&Vec<Value>> { v.get(k)?.as_array() };
        let s = |x: Option<&Value>| -> Option<String> { Some(x?.as_str()?.to_string()) };
        let n = |x: Option<&Value>| -> Option<u64> { x?.as_u64() };
        let mut c = ControlState { applied_index: n(v.get("applied_index"))?, ..Default::default() };
        if let Some(e) = v.get("executor").filter(|e| !e.is_null()) {
            c.executor = Some(Executor {
                node: n(e.get(0))?,
                term: n(e.get(1))?,
                round: n(e.get(2))?,
                fenced: e.get(3)?.as_bool()?,
            });
        }
        for r in rows("history")? {
            c.history.push((n(r.get(0))?, n(r.get(1))?));
        }
        for r in rows("pools")? {
            c.pools.insert(s(r.get(0))?, Pool { name: s(r.get(1))?, file_limit: n(r.get(2))? });
        }
        for r in rows("tapes")? {
            c.tapes.insert(s(r.get(0))?, s(r.get(1))?);
        }
        for r in rows("tape_state")? {
            c.tape_state.insert(s(r.get(0))?, s(r.get(1))?);
        }
        for r in rows("tape_summary")? {
            c.tape_summary.insert(
                s(r.get(0))?,
                TapeSummary {
                    volume_uuid: s(r.get(1))?,
                    generation: n(r.get(2))?,
                    files: n(r.get(3))?,
                    bytes_used: n(r.get(4))?,
                    bytes_written: n(r.get(5)).unwrap_or(0),
                },
            );
        }
        Some(Self { control: c, source: n(v.get("source"))?, directory_index: n(v.get("directory_index"))? })
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
    /// 一盘带回收完毕、已重新格式化：目录库应当清掉它名下的全部记录。
    Reclaimed,
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
    /// 正在回收：不再作为写入带候选，带上还活着的文件正在搬到同池的其他带上
    pub const RECLAIMING: &str = "reclaiming";
    pub const ALL: &[&str] = &[APPENDABLE, DATA_FULL, FULL, LABEL_MISMATCH, CHECK, RECLAIMING];
}

/// 默认的每盘带文件数软上限。推导见研究笔记 pooling-slice-plan.md。
pub const DEFAULT_FILE_LIMIT: u64 = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    pub file_limit: u64,
}

/// 接管历史在内存里只留最近这么多条。它只是给人看的，留全份会随运行时间无界增长，
/// 也会让每份快照变大。
pub const HISTORY_KEPT: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlState {
    pub executor: Option<Executor>,
    pub applied_index: u64,
    /// 历次接管的简要记录：(轮次, 节点)，只留最近 `HISTORY_KEPT` 条
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
    /// 索引里这些文件的字节数（只算活着的）
    pub bytes_used: u64,
    /// 带上实际写掉的字节（容量读数）。与 `bytes_used` 的差额是被重写的旧副本和放弃的块
    pub bytes_written: u64,
}

impl ControlState {
    pub fn apply(&mut self, index: u64, cmd: &Command) -> Applied {
        self.applied_index = index;
        match cmd {
            Command::Takeover { node, term } => {
                let e = Executor { node: *node, term: *term, round: index, fenced: false };
                self.history.push((index, *node));
                if self.history.len() > HISTORY_KEPT {
                    let drop = self.history.len() - HISTORY_KEPT;
                    self.history.drain(..drop);
                }
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
            Command::TapeReclaim { barcode } => Applied::Admin(self.tape_reclaim(barcode)),
            Command::TapeReclaimed { barcode, volume_uuid } => {
                // 只认"确实在回收中"的带：迟到的重复上报不能把一盘已经重新写入的带清空
                if self.tape_state.get(barcode).is_some_and(|s| s == tape_state::RECLAIMING) {
                    // 带被重新格式化了，摘要必须跟着回到第 1 代——这是唯一允许代数回退的地方
                    self.tape_summary.insert(
                        barcode.clone(),
                        TapeSummary { volume_uuid: volume_uuid.clone(), generation: 1, files: 0, bytes_used: 0, bytes_written: 0 },
                    );
                    self.tape_state.insert(barcode.clone(), tape_state::APPENDABLE.to_string());
                    Applied::Reclaimed
                } else {
                    Applied::Nothing
                }
            }
            // 文件记录进目录库的暂存表，这里不处理
            Command::CatalogPart { .. } => Applied::Nothing,
            Command::TapeCommitted { barcode, volume_uuid, generation, files, bytes_used, bytes_written, .. } => {
                // 只接受已归属的带，且代数不回退：迟到的旧批次不能把摘要改回去
                let newer = self.tape_summary.get(barcode).is_none_or(|s| *generation >= s.generation);
                if self.tapes.contains_key(barcode) && newer {
                    self.tape_summary.insert(
                        barcode.clone(),
                        TapeSummary {
                            volume_uuid: volume_uuid.clone(),
                            generation: *generation,
                            files: *files,
                            bytes_used: *bytes_used,
                            bytes_written: *bytes_written,
                        },
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

    /// 开始回收。只改状态：执行者按状态自己去干，中途换届后新执行者接着干。
    fn tape_reclaim(&mut self, barcode: &str) -> std::result::Result<String, String> {
        if !self.tapes.contains_key(barcode) {
            return Err(format!("{} 未归属任何池", barcode));
        }
        let state = self.tape_state.get(barcode).map(String::as_str).unwrap_or(tape_state::APPENDABLE);
        match state {
            tape_state::RECLAIMING => return Err(format!("{} 已在回收中", barcode)),
            // 这两种状态说明带本身有问题：回收要读它的全部内容再格式化，先人工弄清楚
            tape_state::LABEL_MISMATCH | tape_state::CHECK => {
                return Err(format!("{} 当前状态是 {}，先人工处理再回收", barcode, state));
            }
            _ => {}
        }
        // 与 EE 一致（GLESR211E）：搬出来的文件要有地方放。同池里没有别的可写带，
        // 受理了也只会让这盘带停在 reclaiming 上没有出口
        let pool = &self.tapes[barcode];
        let has_target = self.tapes.iter().any(|(b, u)| {
            u == pool && b != barcode && self.tape_state.get(b).map(String::as_str).unwrap_or(tape_state::APPENDABLE) == tape_state::APPENDABLE
        });
        if !has_target {
            return Err(format!("{} 所在的池里没有别的可写带作为回收目标", barcode));
        }
        self.tape_state.insert(barcode.to_string(), tape_state::RECLAIMING.to_string());
        Ok(format!("{} 开始回收", barcode))
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
            .map(|i| FileRec { path: format!("/dir/sub/object-{:08}.bin", i), length: i, sha256: "ab".repeat(32), version: (7, i) })
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
        let t = Command::TapeCommitted { barcode: "T1".into(), volume_uuid: "v".into(), generation: 7, files: 3, bytes_used: 9, bytes_written: 12, parts: 2, full: true };
        assert_eq!(Command::decode(&t.encode()), Some(t));
    }

    /// 快照要能原样装回来：压缩之后节点就是从它恢复的，丢什么都等于丢状态。
    #[test]
    fn a_snapshot_restores_the_whole_control_state() {
        let mut s = ControlState::default();
        s.apply(1, &Command::PoolCreate { uuid: "u-1".into(), name: "archive".into(), file_limit: 100 });
        s.apply(2, &Command::TapeAssign { barcode: "T1".into(), pool: "archive".into() });
        s.apply(3, &Command::TapeAssign { barcode: "T2".into(), pool: "archive".into() });
        s.apply(4, &Command::Takeover { node: 2, term: 3 });
        s.apply(5, &Command::Fenced { node: 2, round: 4 });
        s.apply(6, &Command::TapeState { barcode: "T2".into(), state: tape_state::FULL.into() });
        s.apply(
            7,
            &Command::TapeCommitted {
                barcode: "T1".into(), volume_uuid: "v-9".into(), generation: 12, files: 40, bytes_used: 900, bytes_written: 1200, parts: 1, full: false,
            },
        );
        let snap = ControlSnapshot { control: s.clone(), source: 2, directory_index: 7 };
        let back = ControlSnapshot::decode(&snap.encode()).expect("能解回来");
        assert_eq!(back, snap);
        assert_eq!(back.control.executor.unwrap(), Executor { node: 2, term: 3, round: 4, fenced: true });
        assert_eq!(back.control.tape_summary["T1"].generation, 12);
        assert_eq!(back.control.tape_state["T2"], tape_state::FULL);
        assert_eq!(ControlSnapshot::decode(b"{}"), None);
        // 从快照恢复之后接着应用，结论与一路应用下来的一致
        let mut restored = ControlSnapshot::decode(&snap.encode()).unwrap().control;
        let next = Command::TapeUnassign { barcode: "T2".into() };
        assert_eq!(restored.apply(8, &next), s.apply(8, &next));
        assert_eq!(restored, s);
    }

    /// 回收会格式化源带，所以入口的校验要严：只对已归属、且状态说明带本身没问题的带受理，
    /// 而且同池里得有别的可写带能接收搬出来的文件（EE 的 GLESR211E）。
    #[test]
    fn reclaim_is_only_accepted_for_an_assigned_healthy_tape() {
        let mut s = ControlState::default();
        s.apply(1, &Command::PoolCreate { uuid: "u".into(), name: "p".into(), file_limit: 10 });
        let reclaim = |b: &str| Command::TapeReclaim { barcode: b.into() };
        assert!(matches!(s.apply(2, &reclaim("T1")), Applied::Admin(Err(_))), "未归属的带不受理");
        s.apply(3, &Command::TapeAssign { barcode: "T1".into(), pool: "p".into() });
        assert!(matches!(s.apply(4, &reclaim("T1")), Applied::Admin(Err(e)) if e.contains("目标")), "池里只有这一盘带，没有目标");
        assert_eq!(s.tape_state.get("T1"), None, "被拒绝的命令不改状态");
        s.apply(5, &Command::TapeAssign { barcode: "T2".into(), pool: "p".into() });
        s.apply(6, &Command::TapeState { barcode: "T2".into(), state: tape_state::FULL.into() });
        assert!(matches!(s.apply(7, &reclaim("T1")), Applied::Admin(Err(_))), "写满的带不算目标");
        s.apply(8, &Command::TapeState { barcode: "T2".into(), state: tape_state::APPENDABLE.into() });
        assert!(matches!(s.apply(4, &reclaim("T1")), Applied::Admin(Ok(_))));
        assert_eq!(s.tape_state["T1"], tape_state::RECLAIMING);
        assert!(matches!(s.apply(5, &reclaim("T1")), Applied::Admin(Err(_))), "重复下发不受理");
        assert!(matches!(s.apply(5, &reclaim("T2")), Applied::Admin(Err(_))), "T1 在回收中，不能再当 T2 的目标");

        // 带本身有问题的，先人工处理
        s.apply(6, &Command::TapeState { barcode: "T1".into(), state: tape_state::CHECK.into() });
        assert!(matches!(s.apply(7, &reclaim("T1")), Applied::Admin(Err(_))));

        // 完成：摘要回到第 1 代（唯一允许代数回退的地方），状态恢复可写
        s.apply(8, &Command::TapeState { barcode: "T1".into(), state: tape_state::FULL.into() });
        s.apply(
            9,
            &Command::TapeCommitted {
                barcode: "T1".into(), volume_uuid: "old".into(), generation: 40, files: 7, bytes_used: 90,
                bytes_written: 100, parts: 1, full: false,
            },
        );
        let done = Command::TapeReclaimed { barcode: "T1".into(), volume_uuid: "new".into() };
        assert_eq!(s.apply(10, &done), Applied::Nothing, "不在回收中的带不接受完成上报");
        assert_eq!(s.tape_summary["T1"].generation, 40);
        assert!(matches!(s.apply(11, &reclaim("T1")), Applied::Admin(Ok(_))));
        assert_eq!(s.apply(12, &done), Applied::Reclaimed);
        assert_eq!(s.tape_summary["T1"], TapeSummary { volume_uuid: "new".into(), generation: 1, files: 0, bytes_used: 0, bytes_written: 0 });
        assert_eq!(s.tape_state["T1"], tape_state::APPENDABLE);
        // 迟到的重复上报不能把一盘已经重新写入的带清空
        s.apply(13, &Command::TapeCommitted {
            barcode: "T1".into(), volume_uuid: "new".into(), generation: 2, files: 3, bytes_used: 30,
            bytes_written: 30, parts: 1, full: false,
        });
        assert_eq!(s.apply(14, &done), Applied::Nothing);
        assert_eq!(s.tape_summary["T1"].files, 3);
        assert_eq!(Command::decode(&reclaim("T1").encode()), Some(reclaim("T1")));
        assert_eq!(Command::decode(&done.encode()), Some(done));
    }

    /// 接管历史不能无界增长：它每换一次届就加一条，还要进每一份快照。
    #[test]
    fn takeover_history_is_bounded() {
        let mut s = ControlState::default();
        for i in 1..=(HISTORY_KEPT as u64 * 3) {
            s.apply(i, &Command::Takeover { node: 1, term: i });
        }
        assert_eq!(s.history.len(), HISTORY_KEPT);
        assert_eq!(s.history[0].0, HISTORY_KEPT as u64 * 2 + 1, "留的是最近的");
        assert_eq!(s.executor.unwrap().round, HISTORY_KEPT as u64 * 3);
    }

    #[test]
    fn summaries_only_move_forward_and_block_unassign() {
        let mut s = ControlState::default();
        s.apply(1, &Command::PoolCreate { uuid: "u".into(), name: "p".into(), file_limit: 10 });
        let commit = |g: u64, files: u64| Command::TapeCommitted { barcode: "T1".into(), volume_uuid: "v".into(), generation: g, files, bytes_used: 1, bytes_written: 1, parts: 1, full: false };
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
