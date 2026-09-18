# raft-rs 禁选计时与恢复合格的边界依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 25

## Question

固定 raft-rs v0.7.0，禁选 Follower 的 election_elapsed 如何在不触发本节点 Hup 的前提下继续老化？恢复合格时如何防止重置旧 Leader lease 或因数值溢出改变行为？

## Scope

仅核对 25 已指出的计时适配缺口，不扩展库选型。用官方固定源码确认 election_timeout、randomized_election_timeout、elapsed 的比较/加法/重置以及 PreVote/CheckQuorum 的关系；比较“按必要阈值饱和并在重入时规范化”与“重新清零计时”的风险。不得把 Raft 逻辑 tick 当 RQ 的真实截止时钟。

研究在独立分支/工作树提交一份报告，不实现包装、不运行测试、不修改内核、不填数值或探测设备。同步机制的项目设计由 18 基于既有 25 独立推进；计时子项等待本研究，不把未完成事实作为通过结论。

## Assets

- [研究报告](/tmp/tape-rs-election-timer-HxpokL/docs/research/raft-rs-election-timer-evidence.md)。
- worktree：`/tmp/tape-rs-election-timer-HxpokL`；分支：`research/raft-rs-election-timer-evidence`；提交：`d05635d2fe05f2ad2e73ce215a643d16110ccbac`。

## Answer

限定研究完成，固定官方源码依据见报告：

1. 对经溢出检查及配置校验的区间，基本阈值 E、当前随机阈值 R 和最大值 U 满足 `0 < E <= R < U`。禁选 Follower 安全饱和至 R 可同时保留旧 lease 的 `e < E` 与选举阈值 `e >= R` 两个判断；只饱和到 E 会丢失是否已达 R 的信息。此结论是限定源码推论，不是现成 API 保证。
2. 候选每 tick 在 e<R 时安全加一，否则归一至 R，不先加法再 min；恢复资格事件自身不清 e、不重抽 R。正常消息、批准实际 Vote、角色转换和 reset 的合法重置继续生效，不用旧最大年龄覆盖它们。
3. 正常 tick 超时仍会先清 e 再 hup；若未应用配置阻止 hup，节点可仍为 Follower 并短期再出现旧 lease 条件。因此不承诺所有后续路径永不重置或恢复资格即当选，配置应用与其他节点投票/复制的正例仍须验证。
4. 固定版默认最大阈值直接计算 `2 * election_tick`，validate 自身也会调用；须在该计算前防溢出，再校验真实 min/max 区间。逻辑 tick 不是 RQ 的真实时效证据，不从补 tick 获得多数派许可。

依据：[Raft 固定源码](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs)、[Config 固定源码](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/config.rs)。ET01—ET06 均未实现/执行；resolved 仅代表事实核对和报告完成，不表示完整 EG 或同步机制已证明。研究代理未连接实验室；主代理随后获授权的 EE3 只读观察另见环境记录，不混作此研究证据。
