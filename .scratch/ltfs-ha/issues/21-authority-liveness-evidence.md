# 持续多数派活性与执行资格时效的证据边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 16, 19, 20

## Question

为 18 的持续执行资格核对限定事实：官方 Raft 实现的 quorum 活性检查、心跳响应与逻辑 tick 能证明什么，不能证明什么；控制循环/进程暂停及恢复后继续使用旧许可的风险；本地证据有效期与跨节点租约/外部 fencing 的区别。

核对怎样表达“期限内未获得有效多数派证据则停发”的项目候选，避免单个心跳、旧响应、单纯历史授权或补跑 tick 被视为永久执行许可。说明正常无业务写入与真正失去控制进展的区别；不将每次有效期刷新变成逐文件/逐 CDB 共识。

## Scope

仅查询 Raft 原论文、官方实现文档/源码和必要的第一方计时说明。具体 Rust 库、租约算法、检查周期/期限、线程与进程布局不选定。现场 fencing、SG_IO 在途命令和设备访问继续由既有安全契约约束，不操作硬件。报告在独立 research 分支/工作树中单独提交，不修改主工作树或业务源码。

## Context

[18](18-raft-execution-contract.md)已接受：三节点均接带库且 Leader 为唯一执行者；新授权提交并应用后仍须现实隔离与恢复门控；资格失效后停发未下发命令，不等撤权记录多数派提交；旧轮次不复活，隔离事实只可经新轮次复核后沿用。用户接受低频控制共识，日常 I/O 本地检查资格，未知数值留空。此次研究补持续时效的事实依据，不将候选取舍视为已接受。

## Assets

- [研究报告](/tmp/tape-rs-authority-liveness-aJxliM/docs/research/authority-liveness-evidence.md)。
- 独立 worktree：`/tmp/tape-rs-authority-liveness-aJxliM`；分支：`research/authority-liveness-evidence`；提交：`24ea8c2e758f1127218f4e6dc0c57c22d76b4551`。
- 官方参考源码固定在 etcd-io/raft commit `3cbf6a74be3fa392edd8b64253fcd11c3ce5649b`，仅用于事实核对，不作为本项目库选型。

## Answer

限定研究完成，来源事实与项目推论分开记录：

1. 官方参考实现的 CheckQuorum 按逻辑检查周期判断近期活跃多数派，RecentActive 是布尔标记，不直接承载本项目需要的真实时间许可。Node.Tick 是逻辑推进，内部通道繁忙时可遗漏 tick；不能直接将缓存 Leader 状态、补 tick 或一次活跃检查换成 `now + T` 的设备许可。依据：[固定 raft.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/raft.go)、[progress.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/tracker/progress.go)、[node.go](https://github.com/etcd-io/raft/blob/3cbf6a74be3fa392edd8b64253fcd11c3ce5649b/node.go)。
2. 通过 term 等协议检查不等于消息刚产生；本项目若以证据时效门控执行，仍须关联当前成员配置、领导/执行上下文及检查轮次，证明新鲜性和保守截止点。Leader 也可能尚不知道自己已被替代，本地证据期限不承诺其他节点在此期间不会选出 Leader，不是跨节点独占租约。依据：[Raft §5.6、§8](https://raft.github.io/raft.pdf)与报告中的项目推论。
3. 单写“单调时钟”不足以规定系统 suspend 或 VM 暂停/恢复行为；Linux 不同计时来源的 suspend 语义不同。具体计时与目标环境能力须验证，不能以应用暂停期间没有处理定时回调推断期限未过。依据：[Linux timekeeping](https://docs.kernel.org/core-api/timekeeping.html)。本次未做计时或 VM 实验。
4. 控制循环卡住但数据路径运行、进程暂停恢复、最后一次本地检查后才暂停再迟发，均为必须覆盖的反例。每条命令本地检查仍不能单独证明外部访问独占，可靠 fencing 与在途效果核验继续必需。依据及推论见报告和[ClusterLabs fencing](https://clusterlabs.org/projects/pacemaker/doc/3.0/Pacemaker_Explained/html/fencing.html)。
5. “时效内有合格证据则继续、过期终止本轮资格且不自动复活”可作为项目候选政策；不能因单个 Follower 失联或无业务写入就直接判定多数派失效。证据刷新协议、期限、失效与交付竞争、库集成及性能仍须细化验证。AL01—AL08 仅为候选验证，未实现/执行。

resolved 只代表这项限定事实核对完成，不证明完整执行授权协议。用户后续已在 [18](18-raft-execution-contract.md#已裁定持续资格的本地时效与过期不复活)接受时效与过期终止政策，产品裁定与本研究完成分别记录。独立研究分支仅提交报告，未改业务源码或操作设备。
