# 本机 Raft 控制存储的 SQLite 事务基线

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 28, 29, 35

## Question

以 SQLite 单节点本地数据库承载 Raft 必要日志、HardState 和完整控制快照，能否具体满足现有逻辑原子批次、可靠完成、读者保护及崩溃恢复契约？选择可核验的日志/同步基线，明确版本与已知限制，不将本地数据库当作共享权威或磁带 index。

## Scope

只研究 SQLite 官方文档/公告及必要 Linux 原始资料。比较适合首版的一库事务与外置快照方案；说明日志后缀替换、LightReady commit、本地/接收快照、连接/读者/检查点、I/O 错误及整机掉电的边界。研究分支仅报告，不添加依赖、创建数据库、改生产代码或操作设备；不以现有 catalog 用了 SQLite 作为正确性证据。

## Answer

限定研究完成，见[报告](/tmp/tape-rs-control-sqlite-d4DHnP/docs/research/control-store-sqlite.md)。独立分支 `research/control-store-sqlite`，提交 `19304a0f8669e041e109505022284d1c9dd9374b`，仅报告提交。

依自动接受授权采用[独立单库对象及事务映射](../control-store-transactions.md)与 [SQLite 运行基线](../control-store-sqlite.md)：DELETE＋EXTRA、单工作入口/连接、同库完整快照、可靠提交后才发布 RAM Storage 视图，SQLite 3.53.4 作为固定上游验证候选。事务锁之外使用终身本地 flock，仍受真实收束、存储同步及部署防克隆前提约束。

接受控制事务额外同步、完整快照预算和必要错误后保守重建的成本；不声称吞吐已达标，不新增每文件/CDB 事务，不将 catalog 或磁带作为控制历史恢复依据。研究 SQ01—SQ08、主文 DB01—DB08/SB01—SB06 均未实现/执行，真实环境能力未核验；18 claimed、09 依赖不解除。
