# raft-rs 控制存储与启动恢复证据

日期：2026-09-16；票据 28。固定官方 raft-rs v0.7.0，commit `10c6e9db6792b85c81784e44fc278f895d5f0ab0`。仅阅读该提交源码并浏览官方固定链接；未实现存储、运行测试或访问实验室。

## 结论

“完整控制快照持久化、平时 RAM 确定性应用、可靠日志重放”可作为集成验证候选，不要求每条业务应用单独持久化。但快照、配置、应用位置与可靠历史必须配套；库不替应用发布这个恢复集合，也不防整目录回滚。最重要的补充是：从旧快照构造 RawNode 后，恢复屏障不能只禁止竞选和设备动作，还要暂缓协议参与，直到启动时已知提交历史中的配置重放完成。

## 1. 启动输入的源码事实

- `Raft::new` 先读取 `Storage.initial_state()` 的 HardState/ConfState，直接由 ConfState 恢复成员跟踪器，再加载 HardState，最后处理 `Config.applied`。它不会扫描日志替应用重建最新成员配置。[构造顺序][new]
- `Config.applied` 指定不再交付的历史上界；它不是状态机数据，也不是配置恢复函数。`RawNode` 还以该值初始化 committed entries 交付游标。[Config 文档][config]、[RawNode 构造][raw-new]
- 日志初始 committed/applied 为 `first_index - 1`，persisted 为 Storage 的 last_index。加载 HardState 要求 commit 不低于该截断边界且不高于 last_index；推进 applied 要求不倒退且不高于 committed/persisted。返回的待应用区间受这些边界约束。[日志初始化及应用][log]、[HardState 加载][load]
- 配置条目需要应用层调用 `apply_conf_change`，该调用针对当前成员前态转换，并非通过 index 自动去重。业务上拒绝的配置变更不得调用它。[配置应用入口][conf]

因此，“最新 ConfState + 旧 applied/业务快照 + 再重放配置条目”不能任意拼装；联合配置进入/退出尤其不能假设重复调用无害。这是由上述调用次序得出的项目风险，不是已运行的故障结论。

## 2. 候选恢复集合与顺序（项目推论）

记完整可靠快照切点为 S，启动时可靠 HardState.commit 为 C，可靠日志尾为 L。使用快照业务状态、`ConfState_S`、快照 index/term，以及最新可信 HardState 与匹配日志，要求 `S ≤ C ≤ L`；截断边界不能超过 S，S 后至 C 的重放区间不得有缺口。快照切点的 entry term 与当前选举 term 是不同概念。

1. 先验证并选择一个可恢复集合。恢复快照正文到 RAM，但不恢复任何旧实例的可执行 K/G 许可、旧期限或待发送设备命令。
2. Storage 向启动暴露 `ConfState_S` 与最新可靠 HardState/log；令 `Config.applied = S` 构造 RawNode。即使运行时缓存了更新配置，也不能将其误作本次旧快照重放的启动配置。
3. 暂缓网络 `step`、tick、campaign、提案及其他外部协议入口；通过正常 Ready/实际 apply/advance 边界，按序重放 `(S, C]`。控制业务、空条目、配置变更和确定性拒绝结果都须推进真实连续应用位置；不是重新提交历史命令。
4. 配置与控制状态恢复到 C、相关 Ready 处理完成后，才解除有限启动屏障，进入正常协议追赶。此后无设备资格的健康节点仍可投票和复制；它与故障设备导致的禁选不是同一屏障。
5. 设备/隔离动作恢复仍走当前实例的新授权与现有 EG/RQ/SG 门槛；重放仅恢复控制事实，不派发原始 CDB 或管理副作用。

该启动屏障避免用旧成员表参与协议；这是候选顺序的额外可用性代价——恢复到本地可靠 C 的时间计入节点重新参与服务的时间。若只禁止 campaign，旧配置仍可能影响投票/消息处理，尚不足以支撑该方案。

## 3. LightReady commit 与 RAM 应用

事实：`advance_append` 可能产生更新后的 LightReady.commit_index，并立即更新内部 `prev_hs.commit`；不能期待下一 Ready 重复提供该提交变化。库对 LightReady 的文档不要求该 commit 立即落盘，但整体指南明确提醒：实际应用可能先于 commit 持久化，重启应用游标大于 commit 可导致错误。[advance_append][advance]、[LightReady 文档][light]、[应用持久化说明][guide]

项目候选：沿用单个未完成 Ready，先可靠保存并使 Storage 可读，再 advance_append；得到更大的 commit 时，将它与当前一致的 term/vote、已有可靠日志关联并可靠保存，然后应用相应条目。不可用提交位置对应条目的 term 覆写当前选举 term；不可为了保存 commit 重置 vote。正常 Ready 的 HardState 也遵循先保存后依赖动作的约束。

RAM 应用丢失可由快照与已提交日志重放恢复；仅持久化一个比快照更大的 applied 数字却没有对应状态正文，会跳过应重放历史。本项目采用先持久化 commit 的保守顺序，不采用无证据 `max(applied, commit)` 修补；更不以磁带内容推导 Raft 的 term/vote/授权日志。

## 4. 接收快照与本机生成快照不同

事实：接收 MsgSnapshot 不必然发生完整安装。`restore` 可能拒绝过旧快照，也可能因匹配本地日志而只快进 commit；真正恢复时才替换 Raft 日志/成员状态并形成待处理快照。应用应跟随 Ready，而不是收到报文就绕过 Raft 安装其正文。[接收恢复][restore]

事实：MemStorage 是主要供测试的不完整内存实现；其返回快照没有业务正文，生成快照还假设 commit 已全部 apply。`apply_snapshot` 会清空 entries、替换配置并更新 HardState 部分字段，不能把该内存辅助函数当作生产原子持久化协议。[MemStorage 限制][memory]、[内存快照操作][storage-snap]

项目候选：接收后真正安装的非空 Ready 快照，应可靠保存并实际安装与其切点匹配的完整状态/配置，在既有保守 P2 边界完成后再 advance_append；同周期后续日志按序应用。保存时保留当前正确的 term/vote，并协调快照、日志和 HardState 更新，不能用快照元数据替换整个 HardState。

本机生成快照则从同一个真实已应用切点 S 捕获状态正文、ConfState、index/term 及旧授权拒绝/未决动作闭包。新快照可靠发布前保留旧快照与必要日志；发布后也不得删除 S 后未被其覆盖的历史。不能把本机生成当成接收安装，套用清空所有 entries 的行为。Storage 还须保留截断点 term；它的 compact 条件本身不证明应用恢复资料已可靠覆盖。[Storage 读取与压缩契约][storage]

## 5. 尚未证明及验证要求

- 本报告没有设计跨文件原子发布、断电恢复、目录持久化或故障磁盘协议；单个 rename、校验和或“文件存在”不是本研究已经验证的可靠恢复证据。
- 覆盖发布前/后崩溃、LightReady commit 保存前/后崩溃、快照后配置条目跨多批重放、联合配置进入/退出和被移除节点重启；正例须能重建到 C 并恢复合法协议参与。
- 区分过旧/匹配日志/真正安装三条接收快照分支，检查当前 term/vote 不回退、快照正文实际安装而非只推进 applied。
- 持久化失败、历史缺口或不可信控制目录走 SG 受控修复。完整但被整体回滚的旧目录可能仍内部一致；本方案不能据自洽性证明它没有回滚，不解决旧身份重复使用或失信恢复。
- 未给出快照频率、存储引擎、可用性时限或性能数字；上述用例均未实现/执行。

[new]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L325
[config]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/config.rs#L43
[raw-new]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L297
[log]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft_log.rs#L74
[load]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2784
[conf]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L393
[advance]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L669
[light]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L249
[guide]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/lib.rs#L304
[restore]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2564
[memory]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs#L373
[storage-snap]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs#L243
[storage]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs#L106
