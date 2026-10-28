# 海量文件场景的 SQLite / FoundationDB 数据归属分析（2026-10-23）

本报告逐一检查仓库内生产代码创建或访问的 SQLite 数据。**目录的目标规格已提高到集群亿级文件**；当前表的存续不作为设计前提，目标架构详见[亿级目录设计](foundationdb-directory-100m-target-20261023.md)。结论基于源码、现场交接中已记录的约 108 万和约 209 万目录记录的带、以及 SQLite 和 FoundationDB 官方文档；**没有对现场数据库或 FDB 集群做吞吐/容量实测**。用户最新授权允许高文件量介质参加写入和迁移压测，但明确禁止格式化；现场执行前仍须核对介质、现有内容、设备 owner 和预期写入范围，避开会自动格式化的路径。[现场交接](../../.scratch/ltfs-ha/handoff-to-codex.md)

## 先区分三种事实

1. **设备控制事实**：谁有带库 owner/PR、某条磁带属于哪个库内池、是否可写、哪些设备可操作。错误会导致错误装带或写带。
2. **已确认的物理卷事实**：某个 `volume_uuid`/generation 的索引含哪些文件与 extent，某个库内路径的最新已提交版本在哪盘带上。回收/删除的安全检查依赖它。
3. **全局逻辑事实与观察**：逻辑库、用户文件身份、副本集合和跨库任务属于全局权威；独立扫描得到的文件清单、容量和完整 XML 则只是带时间戳的观察，可能落后于磁带。

文件量大并不使 SQLite 的索引查找自动失效；它使**全量重扫、整表物化、索引快照复制、WAL/网络写入量和恢复时间**成为主要问题。SQLite 官方允许远超百万行的数据；FDB 也能存大量小键值，但它的事务不能覆盖整盘百万级索引。[SQLite 限制](https://www.sqlite.org/limits.html)、[FDB 限制](https://apple.github.io/foundationdb/known-limitations.html)

## 全部 SQLite 数据清单与建议

以下“现有数据”是迁移源清单；“建议归属”按亿级目标判断，不以少改当前代码为原则。不能把同一字段同时交给 Raft/SQLite 和 FDB 写入。

| 当前文件与表 | 内容、读写用途 | 建议归属及海量文件理由 |
| --- | --- | --- |
| `raft.db`: `hardstate`、`entries`、`snapshot`、`local_instance` | 任期/投票/提交位置、复制日志、控制快照、本机进程代数。[源码](../../src/daemon/store.rs) | **保留节点 SQLite，但缩小日志内容。** Raft 只管设备 owner/PR fencing 和小型控制配置；文件、池/磁带政策及文件任务迁走后不再把亿级目录分片放入日志和快照。 |
| 各库 `directory.db`: `meta` | 已应用 Raft 索引；重放幂等、快照水位与服务屏障。[源码](../../src/daemon/directory.rs) | **拆分并淘汰此目录表。** Raft 保留控制水位，FDB 管目录 schema/发布水位，本地有界缓存记录自己的失效水位；目录不再靠逐节点 Raft 重放恢复。 |
| `directory.snap.db`、`directory.incoming.db` | 当前目录的本地快照和接收中的快照；供 Raft 落后节点追赶，完成后安装。[源码](../../src/daemon/directory.rs) | **完成迁移后淘汰整库快照。** FDB 以自身复制、备份和逐卷对账恢复，Raft 只传小控制快照；不再搬运亿级 SQLite 文件。 |
| 各库 `directory.db`: `pools`、`tapes` | 库内池 UUID/名称/文件上限、条码归属、卷 UUID、代数与汇总；由 Raft 控制状态投影。[源码](../../src/daemon/state.rs)、[目录](../../src/daemon/directory.rs) | **政策和归属迁到 FDB 唯一权威。** 瞬时库存/驱动位置及 MAM 容量读数留 owner 本地，必要时发布带来源/时间的摘要；设备 owner/PR 仍在 Raft。迁移调度和安全检查的全部读写点，不能留下另一份可写归属。 |
| 各库 `directory.db`: `files`、`files_by_tape` | `(pool_uuid,path)` 当前物理记录、条码、generation、版本、墓碑、大小、摘要与 metadata；查文件、列目录、回收前活引用核对。[源码](../../src/daemon/directory.rs)、[文件服务](../../src/daemon/files.rs) | **迁到 FDB 唯一共享目录。** 用稳定 inode + 父目录/名称支持重命名和分页，文件版本/墓碑与副本、按卷反向索引短事务更新；本地只可有受控大小且带版本的缓存。回收检查仍要核对真实卷索引，不能只信缓存。 |
| 各库 `directory.db`: `staging`、`staging_by_batch` | Raft `CatalogPart` 的文件片段，等 `TapeCommitted` 及分片齐全后才合并到 `files`，保证整批可见。[源码](../../src/daemon/directory.rs) | **本地 SQLite/分段文件存执行暂存，FDB 存全局意图、凭据和发布标记。** 大批次分片写不可见候选，校验后短事务发布；不把亿级全量结果放入一笔事务或 Raft。 |
| 各库 `directory.db`: `file_tasks` | 按池的已受理/终态任务，随 Raft owner 变化转为结果未定。[源码](../../src/daemon/directory.rs) | **迁到 FDB 唯一共享任务账本。** 本地仅保存执行步骤和 outbox；幂等 ID、结果未定恢复及保留/清理策略必须明确。 |
| `cluster-directory.db`: `meta`、`libraries` | 把各库目录快照分块封装供追赶；它不是在线查询库。[源码](../../src/daemon/multilib.rs) | **完成迁移后淘汰。** 用 FDB 恢复/备份及必要的介质对账取代多库 SQLite 快照包；不能把快照包当作 FDB 大 value。 |
| 独立 `catalog.db`: `volumes` | 人工/离线 `catalog sync` 观察到的卷 UUID、条码、LTFS 代数、blocksize、同步时间及容量快照。[源码](../../src/catalog/db.rs)、[同步](../../src/catalog/sync.rs) | **本地 SQLite 作按卷扫描工作集；FDB 保存全局已发布观察 manifest**（卷身份、代数、来源、时间、计数/摘要、容量观测）。观察不等于当前可写状态；容量必须带时间戳和来源。 |
| 独立 `catalog.db`: `files`、`files_by_path`、`files_by_size` | 离线扫描的每卷 file UID、路径、大小、时间；支持 `find`。[源码](../../src/catalog/db.rs)、[查询](../../src/catalog/query.rs) | **本地 SQLite 只保存受控范围的扫描明细/暂存，不默认在单节点长期积累全部亿级文件。** 已受管卷直接查 FDB `managed` 目录；未受管或脱机卷确需跨节点搜索时，才按已完成代数向 `observed` 发布精简索引，避免无条件复制第二份亿级目录。原始明细可按卷冷归档或重扫；观察不能自动覆盖管理目录。 |
| 独立 `catalog.db`: `extents`、`extents_by_block` | 每文件每段物理分区/块位置，`show` 当前只取首个 extent。[源码](../../src/catalog/db.rs)、[查询](../../src/catalog/query.rs) | **留在本地 SQLite 或冷归档分段文件**；全局普通找文件不需要把所有 extent 复制进 FDB。只有明确的远程脱机读规划需求且测过存储放大后，才按卷/文件 UID 做 FDB 按需索引。extent 数可高于文件数。 |
| 独立 `--library-state` SQLite: `library_identity`、`library_observations` | 单机带库身份和最近库存/状态观察。[源码](../../src/library.rs) | **保留单机 SQLite。** 设备身份、原始库存属于当前节点/扫描时点；全局展示可发布带时间戳摘要，不得以旧观察决定机械动作。 |
| 独立 `--library-state`: `library_volumes` | 单机条码→池/home、phase、blacklist 与失败诊断。[源码](../../src/library.rs) | **现有单机模式保留。** 若合并到 HA，将管理归属/blacklist 政策迁到 FDB 的唯一控制权威；单机工具不得与 `ltfsd` 同时管理同一实库。这些行数随带数增长。 |
| 独立 `--library-state`: `library_pools`、`library_pool_groups`、`library_pool_warnings` | 本地池、介质限制、mountlimit、节点组和空间告警策略。[源码](../../src/library/pools.rs) | **单机保留；集群统一管理时把全局池政策迁到 FDB**，并显式切换调度读写路径。当前 standalone 状态不供 `ltfsd` 复用。[客户端边界](../file-client.md) |
| 独立 `--library-state`: `library_tape_capacity` | 带的 MAM 容量快照，assignment/phase/home 改变时用 trigger 清除。[源码](../../src/library/pools.rs) | **保留节点 SQLite 的带来源观察。** FDB 若用于全局容量展示，只存归一化值、采集时间/设备/代数与有效性；写入准入仍核验当前装载介质容量。 |
| 独立 `--library-state`: `library_tape_options`、`library_drive_options`、`library_drive_roles`、`library_drive_groups`、`library_nodegroups`、`library_local_node` | 单机 offline、驱动启停/角色/组和本节点组。[源码](../../src/library/admin.rs) | **单机保留**；集群管理政策迁到 FDB 唯一权威，本节点身份、owner/PR 与实时设备观察仍本地/Raft。规模与设备/组数有关，不受海量文件影响。 |
| 独立 `--library-state`: `library_volume_indexes` | 每盘带一整份已对账 LTFS XML 文本及观察时间。[源码](../../src/library/maintenance.rs) | **不放 FDB value。** FDB 单 value 上限 100 KB，百万文件索引远超它；大索引也不宜作为 SQLite 单行频繁返给 HTTP/CLI。目标应是本地或对象/文件归档中的分段/压缩索引，FDB 只存 `(卷,代数,摘要,位置,观察时间)` manifest。旧实现保留到新读取路径验证完成。 |
| 独立 `--library-state`: `library_tasks` | 单机前台任务及进度/取消/结果；不自动重放。[源码](../../src/library/tasks.rs) | **保留本地 SQLite 执行日志。** 若将跨节点任务统一管理，FDB 只放全局任务 ID、意图和发布状态；详细本地进度仍在执行节点，不能把单机终态误当磁带提交凭据。 |

`tape-library --library-state` 是**另一套非 HA 控制器**，不是 `ltfsd` 的第四个副本；不能直接把它的表同步到集群控制状态。[源码](../../src/library/admin.rs)、[现有边界](../file-client.md)

## 亿级目标下，数据库迁移不能替代的工作

- `catalog sync` 当前对同一卷执行一次事务的**全量删除、级联删除 extent、全量插入**；源码注释以 10 万文件/30 万行为常态，已低于现场百万级前提。[同步](../../src/catalog/sync.rs)。SQLite 允许大事务，但写入期间 WAL 不能重置，可能显著增大磁盘峰值和查询成本；需要测真实快照大小、重扫时间、WAL 峰值、失败回滚与重试。[SQLite WAL](https://www.sqlite.org/wal.html)
- 将同样的百万级全量替换直接搬到 FDB **不成立**：单事务影响数据上限 10 MB、持续约 5 秒。应按 `volume_uuid + generation + scan_id` 分批写不可见快照，逐批校验条数/摘要，再用一个短事务切换该卷的 `published_generation`。查询 cursor 固定代数；旧代数须在 cursor 有效期结束且无读取引用后清理。重跑以 `scan_id` 幂等覆盖，不对在途部分结果提供正常查询。[FDB 限制](https://apple.github.io/foundationdb/known-limitations.html)、[FDB 加载建议](https://apple.github.io/foundationdb/developer-guide.html)
- `catalog find` 默认把输入变成 `%pattern%`，普通 `files_by_path` B-tree 索引不能按前缀缩小范围；FDB 的有序键也不能自动做任意子串搜索。[当前查询](../../src/catalog/query.rs)、[SQLite LIKE 优化条件](https://www.sqlite.org/optoverview.html)、[FDB 数据模型](https://apple.github.io/foundationdb/data-modeling.html)。若用户主要按精确路径/目录前缀查，做相应索引即可；若必须保持包含搜索，要单独评估 SQLite FTS5 trigram 或搜索索引的磁盘放大。少于 3 字符的 trigram 查询仍可能全表扫描。[SQLite FTS5](https://www.sqlite.org/fts5.html)
- `catalog show` 将整卷文件收集成 `Vec` 再输出；`directory.list_nodes_scoped`、回收 `paths_on` 和单机 `tape_validate` 也会收集整卷/整池记录。百万级时应先改成有界分页/流式、取消检查和限额输出，FDB 不能替这些 API 自动节省内存。[catalog 查询](../../src/catalog/query.rs)、[目录](../../src/daemon/directory.rs)、[单机校验](../../src/library/maintenance.rs)
- `LtfsVolume::mount` 恢复并持有完整 `LtfsIndex` 树；`catalog sync` 随后遍历它。FDB 可减少重复扫描后分发数据，但不减少第一次从磁带读取大索引、解析和内存物化的成本。[卷挂载](../../src/ltfs/volume.rs)、[索引模型](../../src/ltfs/index.rs)

容量规划不能只数文件行：若一盘有 `N` 个文件、每文件平均 `e` 个 extent，离线本地明细至少有约 `N + N×e` 条主体记录，另有 B-tree 索引。FDB 若发布每文件主键再维护路径和条码两个访问路径，单盘至少约 `3N` 个逻辑键；以 `N=200 万` 为量级示例，就是约 600 万个键，尚未计 extent、旧代数、键名/值字节及 FDB 复制开销。这只是键数量级推算，不是现场大小或吞吐测量；决定是否复制某类明细前，要采样真实平均路径、metadata、extent 数与版本保留量。[FDB 数据建模](https://apple.github.io/foundationdb/data-modeling.html)

## 建议的落地次序与验收门槛

在“确定引入 FDB、设计集群亿级目录”的前提下，目标是让 FDB 成为**池/磁带政策、全局文件/副本目录和文件任务的唯一共享权威**；Raft 留设备 owner/PR，小型本地 SQLite 留执行暂存与缓存。离线扫描的已发布搜索观察独立放 `observed/*`，不得靠路径相同提升为 `managed/*` 运行记录。FDB 不可用时停止新的全局变更；旧本地目录仅在迁移核对期保留，目标态不复制每节点亿级文件。迁移阶段的双读核对、写入权威切换与恢复门槛见[亿级目录设计](foundationdb-directory-100m-target-20261023.md)。

验收至少覆盖：用**合成亿级文件数据**测批量导入、重扫、深目录/超大目录查询分页、按卷枚举、磁盘/RSS/FDB 空间峰值；模拟重复/中断导入与 `commit_unknown_result`、FDB 不可用、owner 接管、FDB 备份恢复和磁带较新提交的对账；核对同名跨库文件、旧版本/墓碑和回收前引用。现场高文件量介质可以用于追加写入和迁移压测，但每轮先确认库存、卷状态、owner、可追加空间及现有文件基线，完成后验证原有文件与迁移目标；不得调用格式化、擦除或会进入自动格式化的回收/空带初始化路径。以上是待实施的验收合同，不是已通过的结果。
