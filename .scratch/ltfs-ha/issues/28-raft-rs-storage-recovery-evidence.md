# raft-rs 控制存储发布与启动恢复依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 22, 25

## Question

固定 raft-rs v0.7.0，如何把 HardState、日志、快照元数据、实际状态机和成员配置组成合法启动输入？特别核对 LightReady commit、Config.applied、Storage.initial_state/ConfState、快照安装与本地生成的区别；采用仅快照持久化业务状态、快照后重放日志的简化候选时，哪些启动顺序会重复配置变更或跳过已提交历史？

## Scope

只查固定官方源码及必要的第一方持久化说明，区分内存示例与生产可靠性。补齐可靠发布前不得回收唯一恢复依据的证据边界；不选存储引擎、不实现 WAL、不运行模型/硬件测试、不连接实验室。报告写入独立 research 分支/工作树，只提交一份研究文档。

## 验证判据

- 不用 max(applied, commit) 修补丢失历史，不从磁带恢复任期/投票。
- 说明实际应用与持久快照切点、启动 ConfState 的配套关系；不把当前配置和旧 applied 任意混装。
- 接收快照的安装不能回滚当前 term/vote；本机新快照不能删除未被可靠覆盖的后续日志。
- 库内存 Storage、文件 rename 或校验和均不单独证明可靠恢复及防整目录回滚。

## Assets

- [研究报告](/tmp/tape-rs-storage-recovery-Kv1hWo/docs/research/raft-rs-storage-recovery-evidence.md)。
- worktree：`/tmp/tape-rs-storage-recovery-Kv1hWo`；分支：`research/raft-rs-storage-recovery-evidence`；提交：`e0b0093531c59a0e2858c7e66e814f826cbac987`。

## Answer

限定研究完成，固定 v0.7.0 / `10c6e9db6792b85c81784e44fc278f895d5f0ab0`，事实与项目候选分开：

1. RawNode/Raft 启动直接使用 initial_state 的 ConfState 建立成员关系；Config.applied 只给出应用位置，不会重建业务/成员状态。因而“最新配置+旧快照/applied+重放旧配置”不能混拼。依据：[Raft 构造](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L325)、[Config.applied](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/config.rs#L43)。
2. 完整控制快照 S/ConfState_S 配套最新可靠 HardState/log，以 applied=S 构造并按序重放到启动可靠 commit，是有源码支持的集成候选。启动期间先不接收网络 step/tick/提案/竞选，防止旧配置参与协议；恢复后正常投票复制，不因设备禁选停控制。这是项目顺序及可用性成本，不是库现成的恢复入口。
3. LightReady 新 commit 更新内部 prev_hs，不能等下轮重复通知；项目选择保存新 commit 及配套依据后再实际应用，平时业务状态在 RAM，仅完整快照持久化。不持久化比正文更大的裸 applied、不 max 修补历史。依据：[advance_append](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L669)。
4. 接收 MsgSnapshot 有拒绝、日志匹配仅推进 commit、真正恢复三类；只有真正 Ready 快照才按安装路径处理，旧后缀不默认保留。本机生成快照不套用接收快照清空 entries 的行为；内存示例不含生产业务正文和可靠发布保证。依据：[restore](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2564)、[MemStorage](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/storage.rs#L373)。
5. 库不提供本项目所需的跨对象可靠发布、实际业务安装或整目录防回滚证明。完整但倒退的历史可能仍内部自洽；继续按 SG 区分可信重放和控制身份失信，不从磁带重建控制授权。报告未选择存储产品/屏障/格式，未实现或运行测试。

依自动推进授权，以上候选写入[控制存储恢复草案](../control-storage-recovery.md)。resolved 只表示限定依据核对完成，18 仍 claimed、09 仍阻塞；研究分支只提交报告，未操作实验室或真实控制目录。
