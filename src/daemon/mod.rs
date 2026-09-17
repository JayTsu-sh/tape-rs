//! ltfsd 最小骨架：三节点 Raft 选出 Leader，Leader 取得执行轮次、隔离逻辑库的全部设备、
//! 经多数派确认后恢复卷并开始服务；失去资格则冻结并让出领导权。
//!
//! 线程划分：一个 Raft 循环线程（`node`），一个设备执行线程（`executor`），网络收发线程（`net`）。
//! 设备操作可能耗时数分钟，绝不在 Raft 循环里做。
//!
//! 接管序列（研究笔记 authority-after-device-fencing.md）：
//! 当选 → 提交 `Takeover`，它的日志索引就是执行轮次 → 隔离全部设备并回读 →
//! 提交 `Fenced`（应用时若已有更新的 `Takeover` 则本轮作废）→ 恢复、收尾 → 服务。

pub mod executor;
pub mod net;
pub mod node;
pub mod state;
pub mod store;
