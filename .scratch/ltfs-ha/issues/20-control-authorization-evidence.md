# 控制授权提交与外部效果的证据边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 16, 17, 19

## Question

为 18 下一步控制日志与授权提交点核对限定事实：Raft 提案、可靠持久化、已提交与已应用的区别；本轮授权已提交为何不证明持续领导资格、外部隔离或设备效果；快照/重放恢复控制状态与触发外部动作的边界。核对不让失效执行者等待撤权日志多数派提交才停发的安全要求，以及避免逐条 CDB 共识与持续资格验证仍待定义的区别。

## Scope

只查 Raft 原论文、官方实现说明及第一方隔离资料。报告区分来源事实和项目推论，不选 Raft 库、租约参数、控制日志字段格式或 fencing 实现，不声称已证明完整授权协议。遵循独立 research 分支/工作树惯例，仅提交报告，不改业务代码或操作硬件。

## Context

供 [18](18-raft-execution-contract.md)使用。三投票节点均接带库、当前 Leader 是唯一执行者；设备感知参选、失效后停发单条新命令、重建接管轮次并复核旧隔离事实已定。本研究不重新裁定这些取舍，不引入上传暂存复制、回执、非 Leader 设备执行或 GPFS。

## Assets

- [研究报告](../../../docs/research/control-authorization-evidence.md)。
- 独立 worktree：`/tmp/tape-rs-control-authorization-hVqdcT`；分支：`research/control-authorization-evidence`；提交：`886844748e67964900764f33250e7f55377c4b72`。

## Answer

限定研究完成，事实与项目候选分开记录：

1. 提案返回、单节点持久化、Raft 提交与本机状态机应用是不同边界；应用本轮授权不要求所有副本同时应用，但不能跳过此前相关控制转换。协议提交也不能简化为任意旧任期条目“有两份副本”。依据：[官方实现集成说明](https://github.com/etcd-io/raft#usage)、[Raft Figure 2、§5.3–5.4.2](https://raft.github.io/raft.pdf)。
2. 已提交/应用的授权是控制历史，不证明执行者此刻仍有领导资格，也不证明外部隔离或磁带效果。即便每条 CDB 前都共识，仍需处理提交后暂停再恢复派发的旧执行者；不能据此替代外部隔离。依据及项目推论见报告、[Raft §8](https://raft.github.io/raft.pdf)与[ClusterLabs fencing](https://clusterlabs.org/projects/pacemaker/doc/3.0/Pacemaker_Explained/html/fencing.html)。
3. “本轮授权提交并在当前实例应用，再允许受限恢复 I/O”可作为项目候选启用门槛，不是 Raft 内建业务授权协议。隔离动作意图、实际效果和结果记录不原子；控制日志去重不能独自保证外部动作幂等或只执行一次，仍需当前资格、适配协议和效果核验。
4. 多数派不可达可能阻止新控制写入，因此已知资格失效后的本地停发不能等待撤权记录提交；迟到授权应用或回调也不能复活失效轮次。状态机正常重建控制历史与运行门控保持关闭须分开，不通过篡改已提交记录维持停发。依据：[etcd 故障模式](https://etcd.io/docs/v3.6/op-guide/failures/)及报告中的项目推论。
5. Follower 应用、日志重放及快照恢复只重建控制状态，不直接重放原始 CDB 或自动重跑外部隔离。控制提案结果未知或重复须核对前态/轮次/实例，不能由控制重试推导自动重传文件。报告列出 CA01—CA07，均未实现/执行。

resolved 仅代表限定事实核对结束，不等于完整安全证明或目标硬件通过验证。用户后续已在 [18](18-raft-execution-contract.md#已裁定最小控制状态与授权提交点)接受控制状态范围与低频授权提交门槛，该产品裁定与本研究完成分别记录。仅独立研究分支提交报告，未改业务代码、部署或操作设备。
