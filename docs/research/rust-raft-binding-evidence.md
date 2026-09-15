# Rust Raft 与控制事件绑定：限定版本依据

状态：票据 24 的一手资料核对，未实现、未执行集群或故障测试。核对日期：2026-09-15。仅比较下列固定发布，不声称是最新版本，不选存储引擎、不增加依赖。

| 候选 | 本次固定来源 |
| --- | --- |
| raft-rs v0.7.0 | 官方 tag 对应提交 [`10c6e9db6792b85c81784e44fc278f895d5f0ab0`](https://github.com/tikv/raft-rs/tree/10c6e9db6792b85c81784e44fc278f895d5f0ab0) |
| OpenRaft v0.9.21 | 官方 tag 对应提交 [`fc315d5a21cd6480fd715c03c322b9cc7268e6ee`](https://github.com/databendlabs/openraft/tree/fc315d5a21cd6480fd715c03c322b9cc7268e6ee)；存储对照采用该版 `storage-v2` 接口，不混用后续版本 API |

以下“源码事实”可由固定引用复核；“项目判断”不等于库提供的保证。关联主工作树票据 18/24、`control-interface-events.md` 的 proposal/persist/commit/apply/snapshot/quorum 事件，以及 EG/RQ/SG。主工作树规划未复制到本研究分支。

## 结论与候选顺序

**没有一个候选凭现成配置即可完整满足本项目。** 两者都提供 Raft 控制能力，但不负责磁带 fencing、维护交接、旧授权防复用或设备命令门控。尤其“禁止发起竞选”不等于“竞选途中不合格也不能当选”。后文给出具体绕行路径。

**项目判断：可以优先将 raft-rs 固定版列为集成验证候选。** 理由是 RawNode 由应用驱动，持久化、实际 apply 和消息推进边界较直接，并存在公开的退回 Follower 方法；相应代价是项目承担更多网络、存储、调度及故障恢复工作。OpenRaft 提供较完整的运行循环和状态机工作线程，但本次版本缺少已核实的公开完整设备候选门控。这不是性能排名、最终锁库，也不证明 raft-rs 无需 fork；若受控包装无法覆盖全部路径，须另评估扩展，不默认批准改核心投票规则。

## 1. 持久化、提交与实际应用

| 边界 | raft-rs v0.7.0 源码事实 | OpenRaft v0.9.21 源码事实 |
| --- | --- | --- |
| 提案 | `RawNode::propose` 将消息交给内核，返回值不是业务提交或应用回执。[R1] | `client_write_ff` 发送请求并返回接收端；`client_write` 等待状态机应用结果，客户端丢响应仍可能重试，业务去重由应用实现。[O4] |
| 本地可靠保存 | `Ready` 提供 entries、HardState、snapshot；`persisted_messages` 须在其依赖可靠保存后发送。`advance_append_async` 仅允许先进入可读缓存，之后按 Ready 编号顺序调用 `on_persist_ready`；通知较大编号包含之前 Ready 已持久化的断言。`must_sync` 有明确条件，不是任意省略可靠保存的开关。[R1] | `save_vote` 返回前要求落盘；`append` 返回只要求可读，真正落盘后调用 `LogFlushed::log_io_completed`，回调可早于或晚于 append 返回。vote/log 写入须串行且日志无空洞；失败回调可携带错误。[O1][O10] |
| commit 与 apply | Ready/LightReady 给出已提交条目及 commit 位置；应用自行执行状态机。`advance_append` 不确认 apply，`advance_apply_to` 才回报应用进度；`advance` 的契约包含先处理已提交条目，不能把仅入队当处理完。[R1] | core 的 `Command::Commit` 先调用 `save_committed` 再安排 apply；状态机 worker 等 `RaftStateMachine::apply` 返回才形成应用完成结果。其内部完成、通知和客户端回调是不同切点，不能合成一个事件。[O2][O8] |

**项目判断。** `control.persisted` 应绑定真实存储完成，不绑定 Ready 取得或 append 入队；`control.committed` 与指定实例 `control.applied` 分别观察。OpenRaft 的公开读状态/metrics 可用于查询，但不替代故障注入所需的实际完成切点；精确 commit 观察须结合 core/storage 边界确定。条件拒绝须作为确定性业务结果正常推进应用位置，不能冒充存储错误。两者的应用代码均须禁止 apply 发 CDB/外部动作，提交后仍检查本轮资格。

## 2. 快照与恢复责任

- raft-rs 的 `Storage` 提供初始 HardState/ConfState、日志和快照读取；Ready 提供待持久化的接收快照。真实磁盘保存、业务状态切点、发布及日志回收由集成实现；示例 MemStorage 不能充当可靠存储证明。[R3][R1]
- OpenRaft 的 `RaftStateMachine` 明确区分持久状态机与持久快照两种责任模型；`get_snapshot_builder`/worker 要求一致视图或排他保护。`install_snapshot` 返回前须替换状态并保存当前快照，`get_current_snapshot` 须提供匹配的应用位置和成员信息。接口契约不是具体 fsync/原子发布实现；不能先删除唯一恢复依据再依赖方法名证明安全。[O1][O8]
- OpenRaft `save_committed` 是可选接口，默认不保存，并明确提示恢复状态倒退的风险；不能把默认实现直接套入本项目 SG。`trigger().purge_log` 只触发回收，可能延后，触发返回不是实际回收完成。[O1][O5]

**项目判断。** 两者都仍须绑定 `snapshot.captured/durable/published`、`history.reclaim_allowed/reclaimed`，证明同一切点、必要协议状态及安全证据闭包。压缩不删除未知动作；历史授权不恢复 K/G。不能从磁带 index 重建控制投票或维护授权。

## 3. RQ safe 确认绑定

| 项目 | 可核实能力 | 本项目仍需补足 |
| --- | --- | --- |
| raft-rs | `ReadOnlyOption::Safe` 通过 quorum；`read_index(ctx)` 后在 Ready 取得 `ReadState { index, request_ctx }`。唯一 ctx 由调用方保证；同 ctx 已在途不会重新建请求，完成可累计推进排队请求。[R4][R1] | 保存原始 t0、请求所属实例/term/配置/轮次；等待本机实际 applied 覆盖返回位置及授权位置，不将 ReadState 到达等同 usable |
| raft-rs 首次/无响应 | Leader 尚未提交当前 term 条目时，`MsgReadIndex` 路径直接返回而不产出结果；Safe 分支登记请求并广播带 ctx 的心跳。[R2] | 明确先有当前 term 的已提交记录再发起；丢失、超期无结果不能等待旧授权无限工作，也不能当成成功 |
| OpenRaft | `get_read_log_id` 以本次调用的响应通道关联结果，core 向当时有效成员发送独立空 AppendEntries 并收集多数派；不是仅读取 lease/健康布尔值。高 vote 或多数派失败返回错误。[O2] | 公共返回值不是 RQ 的完整上下文证明；需应用关联唯一请求/t0、原始配置与领导上下文，拒绝变化期间的陈旧结果；不把网络实现返回的任意缓存响应当本轮应答 |
| OpenRaft 应用等待 | `read_log_id` 取 committed 与本 Leader 初始空条目位置的较大者；`ensure_linearizable` 在确认后等待实际应用进度达到它。单独 get 调用不含这一步；等待并非项目的 T 期限。[O7][O8] | 库可先确认再等待初始空条目应用，不应误称它先等待当前 term commit 再发探测。本项目须先满足自己的 CG/当前 term 条目前提，再调用；结果到达时仍过期优先检查 |

两者都没有替项目实现 `t0 + T`、旧期限已过不复活、每执行上下文最多一条在途及配置变化失效策略。OpenRaft 分离运行任务、应用等待和返回，需要验证取消/超时后迟到完成仍不能更新旧许可；raft-rs 需验证 ReadState 累计出队和丢失场景。safe 确认不是未来租约或物理排他证明，不逐文件增加共识。这些是 RQ 的既有集成要求，不是新库功能声明。

## 4. 严格 EG：逐入口对照

| 路径 | raft-rs v0.7.0 | OpenRaft v0.9.21 |
| --- | --- | --- |
| 自动发起 | `tick_election → MsgHup → hup/campaign`；`promotable` 来自 voter 配置，不是公开设备资格开关。[R2] | `runtime_config().elect(false)` 控制超时选举；不等于停止投票/复制。[O6][O2] |
| 显式发起 | `RawNode::campaign` 直接进入 `MsgHup`，不能只过滤 tick。[R1][R2] | `trigger().elect` 文档明确不受 enable_elect(false) 影响。[O5] |
| 领导转移 | 有 `transfer_leader`；接收端 `MsgTimeoutNow → hup(true)` 直接竞选并绕过 PreVote，必须核验目标节点自己的门控。[R1][R2] | 本版公开 Raft/Trigger/message 中未找到完整领导转移 API；内部 vote handler 提及代选/转移机制，不应把它当已支持的公开转移协议。能力缺口仅限此版本，不推断其他版本。[O4][O5][O9] |
| 竞选途中条件变化 | `poll` 得到多数票即 `become_leader`；没有再次检查应用设备资格。priority 参与对端投票比较，不是全入口禁选方案。[R2] | `Notify::VoteResponse` 经 vote 关联检查后交给 `handle_vote_resp`，多数票直接建立 Leader，不再查询 enable_elect。[O2][O3] |
| 启动/初始化 | 正常恢复仍须在允许本地推进前建立 EG 门控，不能拿旧业务 ready 作为许可。此项是项目要求 | `initialize` 直接 elect；`startup` 可根据持久 vote 恢复内部 Leader，不由自动选举开关统一覆盖。内部角色恢复不等于项目 G 恢复，但严格 EG 仍须覆盖该路径。[O3] |

**raft-rs 有公开退回路径，但整体正确性待证。** `RawNode.raft` 公开；`Raft::become_follower(term, leader_id)` 公开，同 term 的 `reset` 不清掉已投 vote。因此 Candidate 主动退回 Follower 的源码路径确实存在，不必假称不存在接口。但应用能否在资格变化与获胜响应之间正确排序、保留必要 term/vote、处理输出和全部推进路径，尚未验证。特别是 Ready 取出后到 advance 系列处理前，文档禁止调用 step/propose/campaign 等改变内核状态；不能随手插入退位回调违反这一约束。[R1][R2]

**OpenRaft 的明确不足。** 该版本 core/engine 候选推进为 crate 私有；已检查的公开运行配置不提供“撤销在途竞选但继续健康投票复制”的完整钩子。网络侧屏蔽返回也不能撤回已经排入内部队列的投票响应；停整个 Raft 或删成员不满足 EG。是否需要上游新增钩子或受审计的核心扩展须单独验证，不能保证外层包装足够。[O2][O3][O6]

**共同限制。** 资格变化必须进入可证明的序列化边界；不以尚未获知的物理故障要求瞬时拒选，但一旦已知失效，不能仍在后续投票处理里当选，再仅用 G 关闭来冒充 EG。保留正确投票、日志新旧和多数派规则，不以 priority 改写用户已接受的协议。此处没有批准 fork 或给出可运行实现。

## 5. 存储故障与验证门槛

raft-rs `Storage` 文档将故障恢复交给应用，并区分临时日志/快照不可用路径；不能把所有可重试读取都当控制历史损坏，也不能对失败的写入伪报持久成功。[R3] OpenRaft 将存储错误纳入 Fatal，真实存储实现仍承担契约；Fatal 不会自动隔离设备或替项目完成受控重入。[O11]

**项目判断：SG 仍由产品实现。** 正常可信重启/追赶与回滚、旧身份空目录重入分别处理；不宣称上述库能检测全部静默损坏。健康多数派按原配置工作，不自动缩群；无 Leader 的 MR 例外不变，恢复不会重放维护动作。两候选都不能由类型接口直接证明持久化/设备安全。

本报告建议进入下列限定集成验证，而不是宣布候选已通过；RB01—RB06 均未实现/执行：

1. **RB01：** Ready/append 可读但未可靠保存时，不伪报 persisted、不提前发送受持久化约束的响应；保存失败与迟到通知分开。
2. **RB02：** commit、实际 apply、应用通知分别延迟；条件拒绝仍推进位置，旧授权迟到不复活。
3. **RB03：** 自动/显式/转移/启动路径及在途投票响应分别注入设备资格变化；不可只证明选后 G 关闭。合法其他节点仍能获票并复制。
4. **RB04：** RQ 当前 term 前提、请求丢失/排队/迟到、apply 落后、term/配置变化及旧期限到期；正例不永久等待。
5. **RB05：** 快照同切点、保存/发布/回收崩溃窗口、可信旧快照加日志与失信回滚；拒绝依据和未决动作都不丢。
6. **RB06：** 固定身份/轮次条件下新授权能正常推进，同时设备门控和外部 fencing 仍独立；不因选库新增逐文件共识或上传回执。

## 来源与检查范围

使用 web 打开官方固定提交源码，并以官方 tag 的本地只读检出核对方法与调用路径；未运行候选代码、构建集群或访问设备。docs.rs/GitHub 部分页面抓取失败后改用官方 raw 源码，不用二手文章补结论。只检查文档 diff、引用与固定版本；没有 Rust 测试、吞吐/RTO 测量或完整协议证明。

[R1]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs
[R2]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs
[R3]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs
[R4]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/read_only.rs
[O1]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/storage/v2.rs
[O2]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/core/raft_core.rs
[O3]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/engine/engine_impl.rs
[O4]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/raft/mod.rs
[O5]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/raft/trigger.rs
[O6]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/raft/runtime_config_handle.rs
[O7]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/docs/protocol/read.md
[O8]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/core/sm/worker.rs
[O9]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/engine/handler/vote_handler/mod.rs
[O10]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/storage/callback.rs
[O11]: https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/error.rs
