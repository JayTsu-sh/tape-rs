# Rust Raft 与控制存储绑定的候选依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 19, 20, 22, 23

## Question

在不改变已接受选举、安全门控和数据热路径约束的前提下，raft-rs 与 OpenRaft 的具体版本能够为[接口事件契约](../control-interface-events.md)提供哪些直接绑定？哪些需要应用集成，哪些缺少充分扩展入口而不能承诺？

## Scope

限定两个 Rust 候选，查官方固定版本源码/文档，不做性能排名或泛化生态调查。核对提案、日志/协议持久化通知、commit/实际 apply、快照保存/安装/发布责任、safe 确认及应用等待；另查自动竞选、显式竞选、领导转移及竞选中资格变化的入口，不能靠停投票/删成员/冻结整个协议绕过设备参选要求。

必须区分库能力与项目需实现的部分：库的 storage trait 不等于可靠磁盘实现，某个禁用自动选举配置不等于全部入口已门控。磁带文件缓存中的 SQLite 不因此成为控制存储选型；不增加生产依赖、修改业务代码、构造集群或访问硬件。

研究在独立 research 分支/工作树形成一份有固定源码引用的报告。研究完成仅表示候选证据已整理，不表示 18 完成或候选已被用户选定。

## 核对清单

| 必须回答的问题 | 证据合格条件 |
| --- | --- |
| 本地持久化完成如何向 Raft 回报 | 对应 API、所覆盖状态、允许消息与后继处理的顺序；不能把入队当完成 |
| commit 与实际 apply 怎样区分 | 具体位置/结果通道及应用责任；条件拒绝仍正确推进，重放不触发外部效果 |
| 快照如何绑定同一切点并安全回收 | 库管理与应用存储各自责任；可靠发布、协议状态及恢复组合不能只靠接口名字推定 |
| RQ 的 safe 确认能否接入 | 请求关联、返回位置、等待实际应用、首次确认及无响应行为；不以 lease 或 CheckQuorum 冒充 |
| 所有竞选入口能否被设备资格约束 | 逐项列自动、显式、转移与竞选中条件变化，保留健康节点投票/复制；需要侵入核心则显式报告 |
| 失败与恢复责任是什么 | 持久化错误/失信与正常追赶区分，不空目录复用旧身份、不恢复旧运行许可 |

## Assets

- [研究报告](/tmp/tape-rs-rust-raft-binding-xbvYzk/docs/research/rust-raft-binding-evidence.md)。
- 独立 worktree：`/tmp/tape-rs-rust-raft-binding-xbvYzk`；分支：`research/rust-raft-binding-evidence`；提交：`5aef9aac2330d77b8d3cf25a789b2d0523556d5e`。
- 固定来源：raft-rs v0.7.0 / `10c6e9db6792b85c81784e44fc278f895d5f0ab0`；OpenRaft v0.9.21 / `fc315d5a21cd6480fd715c03c322b9cc7268e6ee`。仅代表研究版本，不声称最新或最终生产版本。

## Answer

限定研究完成，库源码事实与项目建议分开：

1. raft-rs 的 Ready/LightReady、持久化通知和实际应用推进提供可绑定边界；应用负责可靠保存、状态机执行与顺序，不能把提案成功、可读缓存或 apply 入队当成相应完成。OpenRaft storage-v2 区分 append 返回与真正落盘回调，apply 工作线程另有完成边界。依据：[raft-rs RawNode](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs)、[OpenRaft storage-v2](https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/storage/v2.rs)。
2. 两者均有可研究的 safe 确认路径，但 RQ 的原 t0、执行上下文、实际应用等待、期限和迟到结果处理仍需项目集成；不将名称相似 API、CheckQuorum 或读结果直接当作设备许可。固定版具体路径与首次请求前提见报告，不混用两库的处理顺序。
3. 严格 EG 不是单个配置项。raft-rs 的自动竞选、显式 campaign、MsgTimeoutNow 和竞选获票各有推进路径；OpenRaft 禁用自动选举不覆盖显式 elect、在途获票及全部启动/初始化路径。两者均不能凭现成配置认定满足全部门控。依据：[raft-rs 内核](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs)、[OpenRaft Trigger](https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/raft/trigger.rs)、[OpenRaft Engine](https://github.com/databendlabs/openraft/blob/fc315d5a21cd6480fd715c03c322b9cc7268e6ee/openraft/src/engine/engine_impl.rs)。
4. raft-rs 确有公开 Candidate→Follower 路径，同 term 重置不清已投 vote；但应用必须证明全部推进入口、Ready 处理顺序及已知资格变化的序列化，不能随意并发修改内核。OpenRaft 本次版尚无已核实的完整公开设备门控钩子。不能用选后关闭 G 替代参选要求，也不承诺 raft-rs 必然无需核心扩展。
5. 两库接口均不直接提供项目控制存储的可靠发布算法、未知动作保留、SG 失信恢复或设备 fencing。报告 RB01—RB06 为限定集成验证候选，均未实现/执行，库能力不等于目标存储/硬件保证。

**项目方向已由用户接受：** 优先以 raft-rs 研究版本形成集成验证规格，接受更多网络、存储及调度集成工作；不等于最终锁库，不默认批准 fork 或修改核心投票规则。若公开接口无法满足已接受约束，明确返回该缺口再决策，不悄悄放宽 EG。产品裁定记录在 [18](18-raft-execution-contract.md#已接受raft-rs-优先验证候选)，与本研究完成分别记录。

resolved 仅表示本项事实核对完成。研究分支只提交报告；未添加依赖、运行候选代码/集群、实施存储恢复或访问设备，18 和 09 不因此完成。
