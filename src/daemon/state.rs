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
    /// 管理命令的结论。`Err` 表示被状态机拒绝（所有节点结论相同），状态未变。
    Admin(std::result::Result<String, String>),
    Nothing,
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
            Command::TapeUnassign { barcode } => Applied::Admin(match self.tapes.remove(barcode) {
                Some(_) => Ok(format!("{} 已解除归属", barcode)),
                None => Err(format!("{} 未归属任何池", barcode)),
            }),
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
}
