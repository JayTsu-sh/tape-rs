# raft-rs Ready 周期与参选门控的推进依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 24

## Question

固定 raft-rs v0.7.0，如何在不违反 Ready 契约的前提下串行处理可靠保存、实际 apply 和全部竞选入口？设备资格在保存等待或竞选中失效时，哪些公开路径可以保留投票/复制而抑制本节点竞选，哪些仍有未证缺口？

## Scope

只补 24 已定位的 RawNode/Raft 推进顺序，不再比较库或查询新版本。核对 ready/advance_append/advance_apply_to、Ready 与 LightReady 输出、tick/tick_election、campaign/MsgHup、MsgTimeoutNow、投票响应及 become_follower；区分同 term 退回与高 term 消息必须处理的规则。使用官方固定源码及已有报告，在独立 research 分支提交一份限定报告；不实现包装、不运行候选代码/集群、不修改核心或访问设备。

## 验证候选的独立判据

- 可靠保存未完成不能谎报持久化；apply 入队不能谎报实际应用；结果通知迟到不改变真实完成顺序。
- 本地资格失效可先关闭设备门控，不要求跨线程非法修改尚有未完成 Ready 的 RawNode。
- 不以过滤全部协议消息、停投票或删成员实现禁选；高任期消息不能因属于选举相关类型而被不加分析地丢弃。
- 资格失效与即将赢票的响应按明确顺序处理；不能当选后仅关 G 就宣称已满足严格 EG，也不要求感知尚未知晓的物理故障。
- 单个 Ready 的保存等待不是把健康不合格节点永久停在不处理协议的状态；需区分有限等待、存储故障和调度饥饿。
- 软件串行化及公开接口只证明候选机制可研究，不能替代外部 fencing、完整活性证明或性能测量。

### 与既有故障轨迹对齐

以下是研究答案的检查场景，不是已经运行的测试，也不预先规定尚待源码核对的 API 调用：

| 交错 | 按既有约束应观察的结果 | 必须查明的绑定 |
| --- | --- | --- |
| 已取 Ready、保存未完成时获知设备资格失效 | 设备新派发先受限；不伪报保存成功，不让并发退位破坏 Ready 契约；恢复合法推进后不能忽略失效继续赢票 | 允许恢复内核处理的位置，以及失效标记与后续输入的序列 |
| 日志已提交，状态机尚未实际应用，safe 结果先到 | RQ 不启用/刷新，历史授权不能被回调直接用作 G；apply 完成后仍复核原 t0 和当前上下文 | commit、实际 apply、ReadState 的各自位置与通知 |
| Candidate 的获胜响应已排队，随后先识别资格失效 | 不因响应排队较早而在失效已知后继续当选；正常其他候选的投票和复制不被关闭 | 消息消费前的资格复核、合法退出竞选以及迟到响应处理 |
| 设备不合格的健康 Follower 收到更高 term 消息 | 保持协议要求的 term/vote/日志处理，不以类型过滤抹去必要状态推进；不能由转移消息绕过资格竞选 | 消息预处理、副作用发生点与公开抑制路径 |

同一物理故障发生时刻、监测方获知时刻、门控生效时刻、RawNode 允许变更时刻分别观察。凡候选方法无法保证这些区别，报告其缺口，不把模型私有真值或瞬时抢占假设作为实现条件。

## Assets

- [研究报告](../../../docs/research/raft-rs-ready-election-evidence.md)。
- worktree：`/tmp/tape-rs-ready-election-e8ZZMA`；分支：`research/raft-rs-ready-election-evidence`；提交：`c3c5917514a068775a7b46a52a77a90be87c4660`。

## Answer

限定研究完成，固定版本源码支持以下事实及候选边界：

1. Ready 未完成期间不能并发改变 RawNode；可靠保存后的 `advance_append` 与实际应用后的 `advance_apply_to` 分别确认不同工作。Ready/LightReady 两组输出都须处理，快照必须真正安装。`advance_apply_to` 还可能触发 joint 配置自动退出并产生新工作，不能当纯计数赋值。[RawNode](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs)、[Raft 核心](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs)。
2. LightReady 的新增 commit 不保证在下一 Ready 重报；持久化 applied/状态机时，恢复出的 commit 和日志依据须与它一致，不能靠启动时取 max 补造依据。[集成说明](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/lib.rs)。
3. 同 term Candidate→Follower 的公开路径保留已投 vote，但重置其他跟踪状态，必须核对 pending snapshot、迟到响应和旧输出。已知失效在 Ready 中发生时可先关闭受影响外部门控，在允许内核推进的切点先退回再处理获票消息；K/G 各按自身条件核验，不将 G 条件不足自动等同 HK 协调禁止。
4. 单纯停 tick 会冻结 Follower 的 election_elapsed，可能令旧 Leader lease 持续妨碍正常投票。禁选时仅抑制本节点 Hup、保留必要计时的公开字段适配可研究，但复制了部分固定版核心语义；溢出、恢复合格、随机超时和重置竞争仍未证明。[Raft 核心](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs)。
5. 不合格节点的 TimeoutNow 既不能直接完整 step 导致竞选，也不能无分析丢掉高 term 处理；可以研究经验证输入下复现固定版必要 term 前缀、抑制 Hup 的限定适配。不能推广为所有消息取 max(term)，PreVote 等有不同语义。此适配不改库源码或多数派规则，但其正确性及无需扩展仍未证明。

RE01—RE06 为研究候选场景，均未实现/执行。resolved 仅表示本项事实核对完成；具体集成规格及固定版本维护风险由 18 继续收敛，不宣称已满足 EG。未添加依赖、修改内核或访问设备。
