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
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let v = match self {
            Command::Takeover { node, term } => json!({"op": "takeover", "node": node, "term": term}),
            Command::Fenced { node, round } => json!({"op": "fenced", "node": node, "round": round}),
            Command::Abandoned { node, round, reason } => {
                json!({"op": "abandoned", "node": node, "round": round, "reason": reason})
            }
        };
        v.to_string().into_bytes()
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(data).ok()?;
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
    Nothing,
}

#[derive(Debug, Clone, Default)]
pub struct ControlState {
    pub executor: Option<Executor>,
    pub applied_index: u64,
    /// 历次接管的简要记录：(轮次, 节点)
    pub history: Vec<(u64, u64)>,
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
        }
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
}
