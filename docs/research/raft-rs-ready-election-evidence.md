# raft-rs 单 Ready 周期与设备参选门控：限定依据

票据 25；2026-09-15。固定 raft-rs v0.7.0，提交 `10c6e9db6792b85c81784e44fc278f895d5f0ab0`。复用官方源码检出，并以 web 核对固定源码。未实现包装、未测试或访问设备；不比较其他版本，不批准 fork。

## 结论

**单未完成 Ready、可靠保存后 `advance_append`、按真实应用位置 `advance_apply_to` 是可研究的简化集成方向，但原描述还须补上两组输出、快照实际安装和 LightReady commit 的恢复责任。** 设备失效可以先关闭独立外部门控；不得在未完成 Ready 上并发修改 RawNode。

**公开接口/字段可表达候选适配，不等于已有完整禁选 API。** Candidate 退回路径存在；禁选 Follower 的时钟和 `MsgTimeoutNow` 则需复现部分固定版本推进语义。它们可以不修改库源码、成员或多数派投票规则，但仍属于版本耦合适配，必须独立回归。未证明无需核心扩展，也未证明全部 EG 已满足。

## 1. 单 Ready 周期应包含什么

源码事实：`ready()` 到 advance 系列之间禁止 step/propose/campaign 等内核变更。Ready 与随后 LightReady 均可能携带待应用条目和消息；`advance_append` 回报保存而不回报 apply，且可能产生新的 commit 位置。`advance_apply_to` 只回报应用进度，不执行应用。参见 [RawNode 源码][raw]。

以下为**待验证的项目顺序**，不是可直接执行的示例程序：

1. 串行推进者在 Ready 空闲切点先处理已知设备资格变化；随后取得一个 Ready，保留其身份及全部输出，不只取 entries。
2. 保存其 snapshot/entries/HardState，满足配套存储的一致性和可靠条件。普通 `messages` 可按协议早发，也可在此简化为晚发；`persisted_messages` 不得早于依赖保存完成。保存期间协议输入可排队，独立设备/管理门控可立即关闭，RawNode 本身不得跨线程突变。
3. 将 Ready 中待应用条目、ReadState、消息等分别交给对应处理路径，不能因随后消费 Ready 而丢失。快照必须实际安装为匹配的状态机视图，不能只保存正文或提升 applied 数字；条目在快照之后按日志顺序处理。
4. 保存确实完成且 Storage 能正确读取后调用 `advance_append(ready)`，完整接收 LightReady。处理其消息、commit 更新和额外 committed entries；应用序列须覆盖先前 Ready 的条目，不能倒序或漏批。不把“已取得 LightReady”当实际完成。
5. 只有实际快照安装/确定性应用连续完成至位置 A，才调用 `advance_apply_to(A)`。空条目、业务条件拒绝及重复处理也须正确推进；配置条目须按对应接口完成配置应用，不把它们当普通业务拒绝。
6. 继续处理上述过程产生的新 Ready 工作。恢复处理外来获票消息之前，优先落实现已失效 Candidate 的退回；不能以任意清空队列或无限 drain 输出令正常输入饥饿。

第 3—5 步允许细化安装/应用的具体时间，但不能弱化保存与实际完成边界。简化方案可将本周期实际应用也串行完成；这增加等待，并非吞吐最优承诺。持久化长期无结果不能被隐藏成无限“健康等待”，适用故障与调度策略仍需验证。

### LightReady commit 与快照容易漏掉的条件

- `advance_append` 若提交位置推进，会更新 `prev_hs.commit` 并通过 LightReady 返回；忽略它不能假设下一 Ready 必然再给同一 HardState 差异。LightReady 文档不要求该字段每次独立持久化，但官方集成说明特别警告：若持久 applied 超过恢复出的 commit，会造成恢复问题；建议 commit 与应用一同或先行保存，不应以启动时无条件取 max 掩盖日志丢失。[RawNode][raw]、[集成说明][guide]
- 因此项目须选择一致的恢复组合：如果把实际 applied/状态机另行持久化，保证对应 commit/日志依据可恢复；不能由“单 Ready 已 fsync”推断 advance 后的新 commit 已保存，也不要求每个 apply 独立 fsync。
- `next_entries_since` 受 committed 与 persisted 边界共同限制，`applied_to` 拒绝越界或倒退；这些检查不验证业务快照真的安装成功。接收快照、业务状态安装、配置关系及 applied 位置仍须一起验证。[RaftLog][log]
- `advance_apply_to → commit_apply` 还可能因 joint 配置自动离开而追加配置条目；它不是绝对无副作用的计数赋值，后续 Ready 不可漏处理。[Raft 核心][core]

## 2. 已知资格失效与获票的顺序

源码事实：`poll` 收齐票后直接进入 Leader；`become_follower(term, leader_id)` 是公开方法。其 `reset` 在 term 相同时保留已投 vote，在 term 改变时清 vote；退回还保留 `pending_request_snapshot`，但会重置竞选计时、读请求及相关跟踪，不能当作只改角色字段。[Raft 核心][core]

**候选约束：** 对已失效的 PreCandidate/Candidate，在 Ready 可以推进的合法切点，用当前真实 term 退回 Follower，再处理可能赢票的旧响应。未知 Leader 时使用 `INVALID_ID`，不凭旧缓存或 TimeoutNow 发送者伪造已知 Leader；同 term 退回不能撤销已经投给自己的票。正常 Append/Heartbeat/Snapshot 仍交给原协议建立其合法 leader/term 关系。

设备失效在 Ready 保存中发生时，先关闭 K/G 等外部新派发并标记待处理，不抢改内核。失效事件与下一次推进须有明确线性化边界；若事件已经被当前推进者获知，不允许先处理赢票后仅关闭 G 来宣称满足 EG。未知物理故障不要求软件预知。

退回之后仍处理正常 Vote/PreVote 请求、复制、心跳、快照及必要响应，不更改 quorum 或成员。已有 Ready 中本节点旧竞选请求与给其他节点的投票/复制响应须区分；旧竞选输出的抑制及已发送输出的迟到处理待验证，不能一概丢弃控制消息。已有合法 Leader 的失效按 LG 等规则处理，不能把 Candidate 的退回策略无条件套给所有角色或每次 Follower 不健康。

## 3. 禁选 Follower 的 tick：不能冻结旧 Leader lease

源码事实：`tick_election` 增加 `election_elapsed`，超时且 promotable 时内部发 `MsgHup`；该方法没有设备门控参数。`election_elapsed` 是公开字段；`promotable` 由 voter 配置决定。收到较高 term 的 Vote/PreVote 时，`check_quorum`、已知 leader 及 `election_elapsed < election_timeout` 会参与原协议的 lease 判断。[Raft 核心][core]

**反例。** 仅因设备不合格永久停 tick，若旧 leader_id 留存，旧 lease 的逻辑年龄可能不再增长，导致该节点持续拒绝其他节点的竞选，违背保持正常投票活性。每轮强行清零 elapsed 再 tick 也不是解决方案。

**有限候选。** 仅针对已退回的设备不合格 Follower，在原本 tick 的时刻维护非倒退的 elapsed 年龄，但不执行该方法内部触发本节点 Hup 的分支；其余 Vote/Append/Heartbeat/Snapshot 处理仍调用原路径，让正常消息按协议重置计时。可用公开字段表达，但这复制了核心计时的一部分，必须证明溢出/饱和、重新合格时的规范化、随机超时和消息重置交错，不预选具体算法。不能对 Leader 禁 heartbeat/check-quorum，也不能在 Ready 未完成期间写该字段。

显式 `campaign`/内部直达 `hup` 的入口须另被封装；仅处理 tick 不完整。字段公开不是稳定设备禁选接口，不应将未来升级视为无需回归。

## 4. TimeoutNow：拒绝竞选不等于丢掉高 term

源码事实：普通 `Raft::step` 先处理 term，较高 term 的 TimeoutNow 会先退回 `Follower(m.term, INVALID_ID)`，再进入 Follower 的 TimeoutNow 分支并 `hup(true)`；转移竞选不走 PreVote。因此完整直接 step 仍可能让设备不合格节点竞选，直接丢掉消息又漏掉 term 处理。[Raft 核心][core]

**有限候选。** 只对已验证来源/类型的 TimeoutNow、且本节点设备不合格的分支，复现其固定版必要 term 前缀：高 term 经公开 `become_follower` 更新到该 term/未知 Leader；同 term 不触发 Hup，低 term 不触发竞选。产生的 HardState 仍走 Ready 保存，不直接赋 term/vote，不伪造 Heartbeat，不把发送者当已确认 Leader。

这不是给所有选举消息应用通用 `max(term)`：原版对 PreVote、赞成的 PreVoteResponse、lease 和拒绝响应有不同分支。正常投票响应在先完成 Candidate 退回后仍走原协议，保留高 term 处理；未知来源、term=0 或异常报文须先遵循网络/RawNode 的验证契约，不由此特例扩展信任。封装无法完整证明这些边界时，保留缺口，不宣布仅公开 API 已满足 EG。

## 5. 未证明项与验证入口

RE01：持久化前后、Ready/LightReady 两组条目、commit 与 durable applied 的崩溃组合；RE02：快照实际安装/配置/apply 位置不同步时拒绝假完成；RE03：保存等待中失效、Ready 收束后先退回再处理赢票响应；RE04：禁选期间旧 lease 能正常老化且其他合格节点能获票，重新合格可竞选；RE05：TimeoutNow 高/同/低 term、PreVote 特例及所有输入验证均不混同；RE06：退回的 pending snapshot、旧输出、成员应用和后续 Ready 不丢失，正常复制及正例进展不被永久冻结。**均未实现/执行。**

此轮只建立限定可研究路径。公开字段适配仍需固定版回归和维护成本评估，未决定升级兼容方案、存储引擎、线程布局或性能参数；不替代 RQ 时效、实际命令交付和外部 fencing。主工作树无改动，仅报告提交与 diff 检查。

[raw]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs
[core]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs
[guide]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/lib.rs
[log]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft_log.rs
