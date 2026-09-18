# 参选失效与 Raft 内核调用重叠的细化审计

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 25, 26, 27, 33, 34

## Question

将原 18 EG 约束与后续 S/RawNode 草案区分，审查一次内核调用已经开始时设备资格失效的完整轨迹。哪些边界可由固定 raft-rs 的受控适配满足，哪些必须改库或明确调整可观察契约？不能仅以不启用 G 代替参选门控，也不能声称能在源端获知瞬间撤回所有内部变化。

## Scope

核对原 EG、固定 raft-rs v0.7.0 源码及必要的并发原始资料。明确内存角色、输出、HardState/日志保存和后续退位的关系，覆盖自动/显式/转移/赢票入口，不能回滚 term/vote 或漏掉合法协议响应。只提交独立 research 报告，不修改库或运行模型/设备。

## Answer

限定研究完成，见[报告](/tmp/tape-rs-election-overlap-mwWmAF/docs/research/election-invalidation-overlap.md)。独立分支 `research/election-invalidation-overlap`，提交 `7fb2d12f592c2b4496b95081a32927a56854f375`，仅报告提交；固定核对 raft-rs v0.7.0。

依自动接受授权，明确采用[可发布领导资格与调用产物隔离](../election-overlap.md)：并发失效期间库内角色可能短暂变化，但不对外发布/服务不合格的新 Leader，不建立 K/G；合法退回仍保存 term/vote/log 及必要 apply，正常投票/复制响应不随旧竞选整批删除。这是本轮选择的可观察契约，不倒写成用户原先要求的内部瞬时保证。

调用前后核对不能单独证明源端获知至失效发布、接受资格至实际网络交付的窗口；输出来源标注、同步和全入口仍需落实。研究 EO01—EO06 与主文 EA01—EA06 均未实现/执行。18 claimed，09 依赖不解除，未改库、运行模型或操作设备。
