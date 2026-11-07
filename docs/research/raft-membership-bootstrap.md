# raft-rs 0.7.0：初始化与成员替换核对

日期：2026-09-24。仅核对第一方源码，固定 commit `10c6e9db6792b85c81784e44fc278f895d5f0ab0`（本地 `git describe --tags --exact-match` 为 `v0.7.0`）。
此报告不表示 tape-rs 已实现以下协议；调查因任务转向既有实现而提前收束，没有运行库测试或接触硬件。

## 已确认事实

1. `RawNode::new(config, storage, logger)` 存在；该文件没有 `bootstrap` 方法。初始化读取 Storage 的 `initial_state()`，恢复 ConfState 和 HardState，再采用 `config.applied`。因此不能把其他语言库的 bootstrap API 当作本版本接口。[RawNode][raw-new]、[Raft 初始化][raft-new]
2. 官方支持用相同 ConfState 初始化全部创始节点：`MemStorage::new_with_conf_state` / `initialize_with_conf_state`。其注释明确要求相同起始状态，或让新节点先通过快照追赶；生产 Storage 需自行持久化相同含义的数据，不能把 MemStorage 当持久存储。[Storage 初始化][storage-init]
3. 合法的三 voter 创始状态可采用三个节点都持有 `voters=[A,B,C]`，其余 ConfState 集合为空，配套一致的空应用状态、初始日志和 HardState。另一官方示例使用非零 snapshot index/term（均为 1）建立起点，强制空白新节点先取快照；示例最初只有 voter 1，三 voter 推广属于项目方案而非示例原文。[Storage 初始化][storage-init]、[示例起点][example-init]
4. 新节点允许以空 Storage 创建 RawNode。示例根据首次消息创建，但这不是认证机制；`RawNode::step` 会接受来自未知 peer 的非响应消息，不能依赖它阻断未知发送者。[示例入场][example-join]、[step][raw-step]
5. `AddLearnerNode(D)` 后，`AddNode(D)` 会把 learner 提升为 voter。包含 `RemoveNode(C)` 与 `AddNode(D)` 的一个 ConfChangeV2 会进入联合配置；`Explicit` 禁止自动离开，`Auto` 多变更和 `Implicit` 设置自动离开。[变更选择][cc-transition]、[提升][changer-voter]
6. 联合状态为 incoming={A,B,D}、outgoing={A,B,C}。离开联合状态另需空的、transition=Auto 的 ConfChangeV2，应用后才移除 outgoing 与 C 的 progress。不能把“一个联合进入条目”描述为“单条日志完成全部替换”。[进入/离开][changer-joint]、[空变更判断][cc-transition]
7. 联合提交和选举都要求两组多数派。C 不可用时 A+B 可满足两组多数派；只有一个可信旧 voter 时，新增 D 不能补出旧多数派。[双多数派][joint-quorum]
8. 同时只允许一个尚未应用的配置变更；联合状态只能提出离开联合状态。拒绝的配置提案可能被改成普通空条目，因此 `propose_conf_change` 返回成功不证明变更生效。[提案检查][pending]
9. 自动离开有 leader 在应用进入条目前退位的已知 TODO，且 RawNode 文档要求调用方负责再次提出空变更以保证离开。不能将 auto_leave 当作完成保证。[自动离开][auto-leave]、[接口说明][raw-conf]

## 建议的合法操作顺序（项目方案）

1. 新集群创建只能由显式创建流程执行：分配 cluster ID 和永不复用的成员 ID，三个创始节点持久化同一 genesis 身份与三 voter ConfState；重启不得重新执行创建逻辑。
2. C 丢失、回滚或无法证明持久状态完整时隔离 C，保留其身份墓碑；A+B 仍可信且可用才继续成员替换。
3. 分配全新 D 身份。先建立受限的、绑定 cluster ID / D / 对端身份的控制网络入场许可；D 以空 Storage 入场而不自行创建 `{D}` voter 集群。上述许可和身份墓碑是应用责任，库接口不提供认证保证。
4. leader 提出 `AddLearnerNode(D)`，待提交、应用并持久化 ConfState 后允许 D 获取本集群快照和后续日志。采用非零 genesis 快照的方案可避免空节点从缺少创始配置的日志前缀开始。
5. D 完成快照安装、日志持久化及应用后，再建立本次提升的追赶证据。库 `make_voter` 没有替应用检查追赶阈值；应自行定义并核验目标 commit/applied index，不能只检查“连接成功”。
6. 等前一变更已应用，提出 `Explicit` ConfChangeV2，包含 `RemoveNode(C)` 和 `AddNode(D)`。等待提交、应用，核对完整 incoming/outgoing/learner 状态。
7. 没有 pending 配置变更后，提出默认空 ConfChangeV2。提交、应用并持久化后核对 sole voters={A,B,D}、outgoing 为空、C 不再是成员；这才是替换完成条件。
8. 成员资格不自动恢复设备执行权；控制面入场与硬件执行资格必须分开设计。

以上 4–7 的 Raft 合法性依据是配置变更源码；网络许可、身份不复用、执行隔离与提升门槛均为项目建议，尚未核对当前实现。

## Ready 与崩溃恢复边界

- 获取 Ready 后，处理完成并交回 advance 系列之前，不调用 step/propose/campaign。先完成其快照、日志与 HardState 的可靠持久化，再发 `persisted_messages`；按 committed entry 顺序应用配置，通过 `apply_conf_change` 获取并持久化 ConfState。[Ready 约束][ready-contract]、[持久消息][persist-msg]、[应用接口][raw-conf]
- `advance(ready)` 要求 Ready 全部完成；返回 LightReady 后还要处理 commit index、消息和 committed entries，再 `advance_apply`。恢复时 Config.applied 必须对应已持久应用位置；应用状态、ConfState 与 applied index 的事务一致性由项目实现。[advance][advance]、[初始化][raft-new]
- 处理 committed entry 必须先看 EntryType。空 `EntryConfChangeV2` 是离开联合状态，不能用通用的 `entry.data.is_empty()` 分支跳过；五节点旧示例仅演示 v1，不能原样复制为 v2 handler。[空变更与自动退出][auto-leave]、[示例 handler][example-ready]
- 提案响应丢失：以操作 ID、已提交应用结果及 ConfState 查状态；不要仅凭 RPC 重试产生下一变更。发现已进入 joint 时完成 leave；发现最终配置则返回既有结果。这是应用恢复方案，不是库提供的 exactly-once 保证。
- ConfState 或 applied index 持久化失败后，不继续驱动已被内存 `apply_conf_change` 修改的 RawNode；停止该实例，从一致持久状态重建并重放未应用提交条目。能否做到原子应用、如何避免 snapshot 和 HardState 撕裂仍待 Storage 实现验证。
- 本地历史完整的正常重启与历史不可信的重新入场必须分流；不能清空旧 C 并继续以 C 的身份投票。该身份规则是项目安全约束，库没有持久身份墓碑服务。
- 失去旧配置多数派时停止自动替换，不能以单节点本地修改 ConfState 或复制旧数据库冒充多数派。灾难恢复若另行定义，须更换集群身份并单独处理旧设备权限。

## 未完成核对

尚未验证当前 tape-rs 的 Storage/Ready 实现、SQLite 事务边界、快照数据绑定、加入许可协议、追赶阈值与请求去重；未执行故障注入。上述方案不能直接作为“已实现、已验收”的结论。

[raw-new]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L299-L321
[raft-new]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L326-L389
[storage-init]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs#L375-L422
[example-init]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/examples/five_mem_node/main.rs#L175-L189
[example-join]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/examples/five_mem_node/main.rs#L210-L234
[raw-step]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L401-L412
[cc-transition]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/proto/src/confchange.rs#L120-L150
[changer-voter]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/confchange/changer.rs#L159-L197
[changer-joint]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/confchange/changer.rs#L50-L128
[joint-quorum]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/quorum/joint.rs#L42-L66
[pending]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2043-L2090
[auto-leave]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L950-L982
[raw-conf]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L372-L398
[ready-contract]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L470-L478
[persist-msg]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L197-L215
[advance]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L645-L711
[example-ready]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/examples/five_mem_node/main.rs#L281-L345
