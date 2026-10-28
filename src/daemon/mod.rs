//! ltfsd：三个持久 Raft 投票副本，控制 Leader 与设备 owner 分离。
//! 无 FC 的节点也可担任控制 Leader；完整设备组通过发现后才成为执行候选。
//!
//! 线程划分：一个 Raft 循环线程（`node`），一个设备执行线程（`executor`），网络收发线程（`net`）。
//! 设备操作可能耗时数分钟，绝不在 Raft 循环里做。
//!
//! 新协议：提交 `AssignExecutor`（日志索引为轮次）→ ReadIndex 核验隔离许可 →
//! 隔离全部设备并回读 → 提交实例/轮次/序号绑定的 `Fenced` → ReadIndex 执行许可 →
//! 恢复卷 → 提交 `ExecutorReady` → ReadIndex 服务许可。每条 CDB 均有本地时效门控。
//! 许可超时不能代替设备隔离，不会取消已经进入内核的命令，也不授权重放硬件操作。
//!
//! `ltfsd` 默认使用新协议；`NodeConfig::coordination=None` 仅保留旧公开调用者的兼容行为。
//! 不支持旧协议节点与新协议节点混合运行。

mod coordinator;
pub mod directory;
pub mod executor;
#[cfg(feature = "fdb")]
pub mod fdb_http;
#[cfg(feature = "fdb")]
pub mod fdb_media;
#[cfg(feature = "fdb")]
pub mod fdb_read;
pub mod files;
pub mod gateway;
pub mod http;
pub mod management;
pub mod multilib;
pub mod net;
pub mod node;
pub mod permission;
pub mod s3;
pub mod state;
pub mod store;
pub mod vip;

mod scheduling;
