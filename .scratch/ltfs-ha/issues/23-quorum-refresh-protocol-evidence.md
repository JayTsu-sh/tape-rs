# 多数派确认刷新与本地时效协议依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 20, 21, 22

## Question

核对周期性 safe ReadIndex 类多数派确认能否作为 18 本地证据时效的候选刷新来源，以及应满足的边界：本任期提交条件、请求上下文关联、当前 Leader 发起、返回 index 与本地已应用进度、配置/轮次变化及迟到返回。与 CheckQuorum 活跃标记、lease-based 读取、每周期写入 no-op 日志的区别是什么？

研究项目候选“从本次请求本地发起时刻而非结果到达时刻计算保守期限，旧期限已过期则不续用旧轮次”；不能将一次读确认解释为未来独占设备的跨节点租约，也不替代 fencing。明确时钟/暂停、请求合并/重排/无响应、实际派发竞争与实现特定接口仍待证明的部分。

## Scope

只查 Raft 原论文与官方参考实现的固定版本源码/文档；不选 Rust 库或直接使用某产品 API，不设计自定义 Raft 投票扩展。独立 research 分支/工作树提交一份报告；不改业务代码、构造集群、执行设备操作或做计时实验。

## Context

[18](18-raft-execution-contract.md)已接受低频控制共识、新授权提交/应用后才恢复设备、有效证据到期则终止本轮资格、旧轮次不复活。用户要求普通文件/磁带 I/O 不逐条共识，未知数值留空。此研究用于缩小持续资格刷新协议缺口，不把周期性刷新当作重新提交执行授权或承诺故障瞬间可知。

## Assets

- [研究报告](../../../docs/research/quorum-refresh-protocol-evidence.md)。
- 独立 worktree：`/tmp/tape-rs-quorum-refresh-QBbS4E`；分支：`research/quorum-refresh-protocol-evidence`；提交：`bdf8a1aa105f8fc4e4fc2aff7d92f2a1833fee58`。

## Answer

限定研究完成，参考实现固定为 etcd-io/raft `3cbf6a74be3fa392edd8b64253fcd11c3ce5649b`，不代表 Rust 库选型。

1. safe ReadIndex 类请求可通过多数派确认提供控制状态的读位置，而不为每次请求追加日志；新 Leader 须先满足本任期提交条件。CheckQuorum 活跃标记和 lease-based 读取不等同于请求相关的 safe 确认。依据：[Raft §8](https://raft.github.io/raft.pdf)、[固定 raft.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/raft.go)。
2. ReadState 只有 Index 与 RequestCtx，不自带执行轮次、term 或成员配置代际；该实现可合并确认，不能假定每个应用 ctx 原样作为独立心跳 token。集成层须正确关联上下文，且等待本机状态机实际应用到返回位置；API 返回不是确认成功，请求可能无通知丢失。依据：[固定 read_only.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/read_only.go)、[固定 node.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/node.go)。
3. 项目候选从进入协议队列前的原始 t0 计算 `t0 + T`，让网络、排队及 apply 等待消耗窗口；先检查原期限，迟到结果不复活已过期授权。这是本地证据年龄政策及推论，不是 ReadIndex 提供的未来设备租约，不替代 fencing 或实际派发竞争的处理。
4. QR01—QR09 为研究候选验证，均未实现/执行。首次启用、单待处理请求等项目取舍在[刷新协议草案](../authority-refresh.md)另行定义，用户已在 18 接受该方向及代价；库接口、原子观察、时钟/暂停及设备隔离仍待证明。

resolved 仅表示本项事实核对和独立报告完成；用户后续接受方向与研究完成分别记录，不表示 18 的授权协议已完成。未运行集群、业务代码、计时实验或设备操作。
