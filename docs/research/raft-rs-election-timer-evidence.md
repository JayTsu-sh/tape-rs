# raft-rs 禁选 Follower 的计时与恢复合格依据

票据 26；2026-09-15。仅核对 raft-rs v0.7.0 / `10c6e9db6792b85c81784e44fc278f895d5f0ab0` 的官方源码，复用票据 25 的只读检出并经 web 打开固定来源。未实现、未运行测试、未修改库或操作设备。

## 结论

**限定验证候选：对设备不合格且确为 Follower 的实例，将逻辑年龄以溢出安全方式推进并饱和到当前随机选举阈值 R，不触发 Hup；恢复合格这一事件本身不清零年龄、不重新抽取 R，随后恢复原版 tick 路径。** 相比只饱和到基本选举阈值 E，保留 R 能同时保留 lease 与选举超时的两个阈值判断。

这是根据固定源码提出的应用适配推论，不是现成禁选 API 或已经证明的实现。它使用公开 elapsed 字段及阈值 getter，但复制了部分计时语义；全入口 EG、资格同步、Ready 顺序、TimeoutNow、设备 fencing 仍各须验证，不能据此宣布无需 fork。

## 1. 原版语义与适用前提

记 `e = election_elapsed`，`E = election_timeout()`，`R = randomized_election_timeout()`，`L/U` 为有效的 min/max election tick。

| 源码事实 | 对适配的含义 |
| --- | --- |
| 配置要求 heartbeat>0、E>heartbeat、L≥E、U>L；随机值来自 `[L,U)` | 对真正有效且计算无溢出的配置，`0<E≤R<U`；不是所有配置都使用默认 `[E,2E)` |
| Follower/Candidate 的 tick_election 先 `e += 1`，随后检查 `e≥R` 及 promotable；命中后先清 e 再发 MsgHup | 直接调用它会绕过外层单独的 campaign 开关 |
| 高 term Vote/PreVote 路径的旧 Leader lease 条件包含 check_quorum、leader_id 非空及 `e<E`，转移上下文等另有例外 | 禁选不应冻结 e 或反复清零，否则可能延长旧 lease、阻碍给其他节点投票 |
| elapsed 及阈值 getter 可访问；promotable 来自 voter 配置 | 不修改成员集合或把 promotable 当设备门控 |

依据：[配置实现][config]、[Raft 的字段/getter、tick_election、step 与随机超时实现][core]。

适用点仅是票据 25 的合法串行变更切点：无尚未完成 Ready 的非法变更；PreCandidate/Candidate 先按已定规则退回；Leader 不使用这条 Follower 计时替代。正常协议输入、投票与复制继续处理。定时算法不是 RQ 的真实时间证据，不能为 K/G 续期。

## 2. E 饱和与 R 饱和的区别

以下是**源码推论**，不改变 core 的 Vote、term 或 quorum 判断：

| 方案 | 能保留的结论 | 局限 |
| --- | --- | --- |
| 停 tick／反复清 e | 无可靠保证 | 缺少新 Leader 消息时旧 lease 仍可能无限保持年轻 |
| `e` 饱和至 E | 达到 E 后 `e<E` 为假，足以处理此 lease 老化条件 | 若 R>E，丢失是否已达到 R 的信息；长期禁选后恢复仍可能额外等待 R−E 个逻辑 tick 才尝试竞选 |
| `e` 饱和至当前 R | 同时保留 `e<E` 与 `e≥R` 两个判断 | 仍需正确串行化消息重置、配置/角色变化、Ready 等待及正常 tick；不是所有其他计时都可同样压缩 |

对年龄 x，取 `min(x,R)` 不改变这两个谓词，因为 E≤R。因而推荐研究 R 饱和，而非强制将已老化的 e 降回 E。此证明只覆盖本版禁选 Follower 上的相关阈值，不声称精确保留禁选期间原版会发生的竞选、term 变化或随机重抽。

**候选步骤的语义，不是已实现代码：** 每次获准处理逻辑 tick 时，在同一串行视图读取角色、资格、e 与 R；若符合禁选 Follower 前提，则在 e<R 时安全加一，否则停在 R，不调用会生成 Hup 的 tick_election。若初始 e 已超过 R，最多归一至 R，保留上述两个谓词；这不是把旧 lease 变年轻的清零。禁止先普通 `e+1` 再 min，因为前一加法可能已经溢出。

恢复合格事件只更改设备参选门控，不调用 reset/become_follower 来“重置计时”，不清 e、不重抽 R。下一次正常 tick 仍需检查当前角色和资格；若已饱和至 R，正常 tick 可触发原版选举检查，但不承诺一定成为 Candidate 或赢得选举。

## 3. 原版重置必须继续生效

源码中，Follower 对经前置 term 处理后的 Append、Heartbeat、Snapshot 会清 e 并记录 leader；批准真正的 Vote 也会清 e，不能把它误写成仅心跳能重置。reset/角色转换会重置年龄及随机阈值；消息处理还可能改变配置与 promotable。Snapshot 消息路径的计时重置不证明业务快照安装成功，不能因此跳过 Ready/实际安装要求。[消息、投票与 reset 路径][core]

**项目适配约束：** 原消息/角色路径发生重置后，后续禁选 tick 应读取新的 e/R，不能用维护的旧最大年龄覆盖它。不能因设备仍不合格就忽略真实 Leader 消息，反过来也不能因设备恢复消息就伪造一次 Leader 活动。重复资格通知没有刷新 lease 的权力。

**重要限定：不清零仅指资格恢复事件本身。** 原版 tick 超时会先清 e 再调用 hup；hup 若发现未应用配置条目可拒绝竞选，节点仍可能留在 Follower。此时如旧 leader_id 保留，之后短期内可能再次满足 `e<E`。因此不能宣称适配后所有后续路径都不会出现新的 lease 年龄；保留原协议行为，并验证配置应用能继续推进，不能靠丢配置或自行旁路 hup 来保证进展。[tick_election 与 hup][core]

Ready 等待期间到来的 tick、真实协议消息和资格变化须按已定义因果顺序处理；不能批量补加墙钟差后覆盖先到的心跳重置。暂停/时钟不可信及 RQ 到期另按既有政策，不把补 tick 当多数派证明。

## 4. 整数与配置边界

配置 `max_election_tick=0` 时，`max_election_tick()` 直接计算 `2 * election_tick`；`validate()` 自己调用该函数，没有 checked multiplication。故“先调用 validate 看是否报错”不是完整溢出防护。构造/验证配置前须确保默认倍增可表示，再验证 E、L、U 的关系；显式 U 则按真实区间核验，不擅自假设它等于 2E。[配置实现][config]

禁选增量采用检查分支或等价溢出安全算术；初值异常不能用回绕掩盖。对上述有效区间，R≤U−1≤usize::MAX−1，因此规范化到 R 后恢复正常 tick 的一次加一可表示；这只证明该次加法，不证明 term/index 或其他计数器永不溢出。阈值变更/实例更换后须重核不变量，不跨实例复用年龄。

## 5. 最小验证义务

ET01：无新 Leader 消息时禁选 Follower 到达 E 后不因旧 lease 永久拒票，同时不发本节点 Hup。ET02：R=E 与 R>E、禁选前已过阈值分别覆盖，恢复事件不清零，正常 tick 能按原版前提推进。ET03：Heartbeat/Append/Snapshot、真实 Vote 批准、term/角色及配置变化与 tick 交错，保留合法重置且不恢复旧外部权限。ET04：Ready 等待/重复资格通知/队列积压不丢因果顺序，不将逻辑时钟当 RQ。ET05：默认倍增溢出、显式 min/max、边界年龄和长时间饱和不回绕，正常 tick 衔接可表示。ET06：hup 被未应用配置阻止、无效候选退回、重新合格及其他节点复制投票均有可进展正例，不以永久拒绝算通过。

**ET01—ET06 均未实现/执行。** 推荐 R 饱和是限定设计候选；调度公平性、全输入门控、资格发布的线性化、实际性能与现场隔离仍未证明。只编辑并提交本报告，diff 检查不替代测试。

[core]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs
[config]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/config.rs
