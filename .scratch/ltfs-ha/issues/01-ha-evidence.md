# 跨节点接管与 LTFS 持久化的事实依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-ha-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by:

## Question

从 LTFS/SCSI/Linux/FUSE 官方规范中，哪些机制与限制决定故障接管的可行性？明确独占访问与真正 fencing 的区别、PR 的硬件依赖、双节点仲裁限制、同步暂存故障域、LTFS 提交与恢复的必要证据、FUSE 重连与重新挂载的区别。核查现有对话将 P0/P1/MAM 全部更新视为统一完成条件是否有充分规范依据。提供一份带来源的研究和部署前待核实清单；不得假定当前设备支持 PR 或自动恢复。研究只给事实和推论，不替用户选择架构。

## Assets

- 研究分支：`research/ltfs-ha-evidence`；独立 worktree：`/tmp/tape-rs-ha-research-rD1Cvk`。
- [研究报告](../../../docs/research/ltfs-ha-evidence.md)：研究分支的 `docs/research/ltfs-ha-evidence.md`，提交 `8b83cccc443dcb0af2913df9f56e3791d35d4ba4`。worktree 移除后可用 `git show research/ltfs-ha-evidence:docs/research/ltfs-ha-evidence.md` 读取。

## Answer

研究已完成，证据与原始来源见报告。

- 仲裁决定行动资格，fencing 阻止旧所有者继续操作；本地锁或过期 lease 不等于跨节点隔离。PR 的支持、访问路径和抢占行为须在目标 drive/changer 上分别核实。
- LTFS sync 的规范要求是可无损恢复，不要求当时已达到 consistent state；VCI 为可选机制。先前建议中“P0/P1/MAM 全部完成”的条件仅可作为待选择的更严格产品契约。VCR 不可当作应用自增事务号。
- 同步暂存确认需覆盖另一故障域上的持久化与元数据/日志顺序；共享存储和复制方案仍待产品决策。
- 同节点 FUSE 连接恢复机制不能迁移故障主机的内核挂载；客户端存活但后端切换、客户端自身失效是两种验收场景。
- 60 秒/5 分钟目标尚未经硬件验证，必须定义计时范围和每类故障的恢复终点。

本票据解决机制与规范事实疑问，不代表部署拓扑、硬件能力或产品提交协议已定。后续分别由恢复承诺、设备隔离、持久化和文件语义票据裁定。
