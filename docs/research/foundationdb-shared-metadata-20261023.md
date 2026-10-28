# FoundationDB 作为共享元数据存储的可行性（2026-10-23）

本笔记只依据 FoundationDB 项目及 `foundationdb-rs` 项目的原始文档讨论数据库能力与限制；下述键布局和采用建议是推论，不代表 `tape-rs` 已实现它们。文档中 FoundationDB 核心行为主要以当前 8.0 官方文档为依据，而 Rust 客户端的发布状态单独标明。

## 结论

FoundationDB **可以**承载跨节点共享、需要事务一致性的元数据，例如逻辑库、池定义、磁带归属、文件身份与磁带副本位置，以及按路径或条码反查的索引。它提供有序键值空间、范围查询和默认严格可串行化的跨键 ACID 事务；应用需要自己设计键、编码、二级索引和生命周期状态。[数据建模](https://apple.github.io/foundationdb/data-modeling.html)、[开发者指南](https://apple.github.io/foundationdb/developer-guide.html)

这不是“把 SQLite 文件放在共享磁盘上”的替换。FoundationDB 自己是要部署、监控和备份的分布式数据库；其存储进程在磁盘上持久化并依配置复制事务。除数据库服务器外，客户端还需要 FoundationDB C 客户端库和集群连接文件。若现有一致性协议已经拥有这些元数据，应先判定是否确有无法满足的全局查询/事务需求，避免出现两个都自称权威的控制面。最后一句是架构推论。[构建集群](https://apple.github.io/foundationdb/building-cluster.html)、[配置](https://apple.github.io/foundationdb/configuration.html)、[Rust 客户端 README](https://github.com/foundationdb-rs/foundationdb-rs/blob/main/foundationdb/README.md)

## `tape-rs` 现状与真正缺口

当前 `ltfsd` **已经有集群共享的逻辑状态**，虽然每个节点把它持久化在自己的 SQLite 文件里：一个 Raft 日志承载多库控制命令，各库目录由已提交日志确定性应用并用应用索引支持重放/快照。[Raft 持久化](../../src/daemon/store.rs)、[多库应用](../../src/daemon/multilib.rs)、[目录库](../../src/daemon/directory.rs)。因此仅为“让三个节点都能知道池和文件位置”，不需要再引入 FoundationDB 或共享文件系统；节点须追上提交和目录应用索引后才能按最新状态回答。

| 当前信息 | 现有位置与能力 | 尚缺的语义 |
| --- | --- | --- |
| 池、磁带归属与状态 | 每库的 Raft `ControlState` 保存池 UUID、条码归属与状态；各节点 `directory.db` 有 `pools`、`tapes` 投影。[控制状态](../../src/daemon/state.rs)、[目录 schema](../../src/daemon/directory.rs) | 池属于指定物理库是目标约束，不是差距；跨库副本通过不同库的不同池承载（2026-10-26 用户确认）。 |
| 文件 → 磁带 | 每库目录的 `files` 以 `(pool_uuid, path)` 为主键，记录条码、索引代数、大小、摘要、版本和墓碑；`lookup` 是键查询，`files_by_tape` 可按条码反查。[目录 schema 与查询](../../src/daemon/directory.rs) | 文件只有库内路径身份，缺跨库稳定文件 ID、多个副本及其健康/版本关系。 |
| 库身份 | 多库配置用真实 `changer_serial` 作选择器，每库隔离目录文件。[多库配置](../../src/daemon/multilib.rs) | “逻辑库”若指一个面向用户、可覆盖多物理库的命名空间或放置策略，目前没有这种资源；物理序列号不可直接充当逻辑库 ID。 |
| 单机 `tape-library` catalog | 独立 `catalog.db` 存卷、文件、extent 和容量缓存，供本地同步/查询。[本地 catalog](../../src/catalog/db.rs) | 它不是 `ltfsd` 的 Raft 目录，不能让其本地扫描结果直接覆盖集群权威记录。 |

现有显式跨库复制已在实带上验证，但它创建的是另一库、另一池中的目标文件；当前没有统一文件身份、副本集合和原子迁池合同。[双库差距记录](../multi-library-gap-progress-20261022.md)。亿级目标需要**全局领域模型、共享索引和相应的权威迁移一起设计**。当前索引能查询一个已知库/池中的路径，但不提供跨库的稳定文件身份与副本集合。路径并非天然全局唯一，应以稳定文件 ID 加命名空间 ID 定义用户可见身份，明确同名、重命名、删除与副本保留规则。

## 目标数据归属（亿级文件）

用户已明确要求按亿级文件设计，不以现有目录的持久化方式作为约束。**FDB 将成为逻辑库、库内池/磁带政策、当前文件/墓碑、副本和文件任务的唯一共享元数据权威**；Raft 只负责设备 owner/PR fencing 及小型控制状态。本地 SQLite 保存执行日志、暂存、可丢弃的有界缓存和设备观察。逐表映射、键布局、发布协议及容量估算见[亿级目录目标设计](foundationdb-directory-100m-target-20261023.md)。

| 数据 | 唯一权威/目标位置 | 保留的安全语义 |
| --- | --- | --- |
| 设备 owner、轮次、PR fencing 和小型配置 | Raft + 节点本地 `raft.db` | 失去 owner 的执行器不能触带，也不能发布旧轮次的 FDB 结果。 |
| 池政策、磁带管理归属、卷身份 | FDB | 调度读写点统一切换；实时库存、槽位、MAM 容量是 owner 的本地观察，不由陈旧摘要驱动机械动作。 |
| 文件身份、父目录/名称、版本、墓碑、副本、按卷反向索引 | FDB | 只发布有 LTFS 提交证据的结果；路径与副本索引短事务更新，回收还须与卷索引对账。 |
| 文件任务意图/终态与批次发布标记 | FDB | 稳定幂等 ID、结果未定恢复；本机只存执行步骤与待发送记录。 |
| 原 `directory.db`、`directory.snap.db`、`cluster-directory.db` | 迁移完成后淘汰整库副本和快照 | FDB 独立备份、逐卷恢复对账；Raft 只传小控制快照。 |
| 离线 `catalog.db` 观察和完整 LTFS XML/extent | 本地扫描工作集或冷归档，必要的精简观察索引另放 FDB | 与 `managed` 运行目录隔离，不能凭同名路径升级为文件权威。 |

## 为什么不把现有目录只当成“可重建缓存”

当前 `directory.db` 除查询外还保存 Raft 应用水位，约束快照追赶，并为回收提供活引用和路径核对。[目录](../../src/daemon/directory.rs)、[文件服务](../../src/daemon/files.rs)、[多库快照](../../src/daemon/multilib.rs) 直接将它删除、同时让 Raft 保留同一份池/文件状态，会造成双权威与恢复缺口。目标设计是**真正迁移写入权威与调用路径**：文件级状态不再写入 Raft，FDB 对外回答共享目录；本地缓存只用于带版本的短期加速。磁带上的索引仍是物理提交证据，而 FDB 记录全局命名空间与副本关系，二者以操作 ID、代数和哈希对账。

亿级文件时，完整目录复制到每个节点和传输整库 SQLite 快照会放大存储与恢复时间；FDB 按需范围读取更合理，但单事务不能更新整盘百万级索引。初次导入与整卷对账要分片、校验、发布，查询在未完成时返回“暂不可判定”，不能返回假 404。[FDB 限制](https://apple.github.io/foundationdb/known-limitations.html)、[分段范围读取](https://apple.github.io/foundationdb/segmented-range-reads.html)

用户最新授权允许现场高文件量介质进行写入和迁移压测，但**禁止格式化**；压测须预检实时库存、卷状态和原文件基线，并在完成后核验原文件与迁移目标。[现场交接](../../.scratch/ltfs-ha/handoff-to-codex.md) 这仍是目标方案，尚未实现、部署或实测 FDB。磁带提交与 FDB 发布不是原子事务；已写带但发布未知时，以介质证据和稳定操作 ID 恢复，不能盲目重写或删除源副本。[FDB 未知结果处理](https://apple.github.io/foundationdb/developer-guide.html)

## 对所需能力的核查

| 能力 | 官方证据与应用含义 |
| --- | --- |
| 原子更新多个索引 | 默认事务具严格可串行化隔离；官方示例在同一事务里维护主记录和二级索引。这适合同时更改“路径 → 文件”“文件 → 副本”“条码 → 文件”，从而避免一次元数据提交内的悬挂索引。[开发者指南](https://apple.github.io/foundationdb/developer-guide.html)、[数据建模：索引](https://apple.github.io/foundationdb/data-modeling.html#indexes) |
| 按前缀快速查询 | 键按字节序排列，tuple 编码保留元素顺序；将共同查询前缀放在键左侧，可按范围读取逻辑库、池、磁带或目录中的条目。多种访问路径需要显式二级索引。[数据建模](https://apple.github.io/foundationdb/data-modeling.html) |
| 高可用和持久性 | 配置复制度后，提交成功前事务在相应数量的进程上持久化；协调器多数派所在网络分区保持可用，官方通常建议 3 或 5 个、跨故障域的奇数协调器。单机默认的 `single`/`memory` 不是生产 HA 配置；官方示例采用 `configure triple ssd`。这些性质取决于正确的拓扑和磁盘部署。[配置：存储和协调器](https://apple.github.io/foundationdb/configuration.html)、[构建集群](https://apple.github.io/foundationdb/building-cluster.html) |
| 变更提示 | key watch 可提示值发生变化，但不保存每次变化，也不保证之后读取到的是触发 watch 的那个值；变回原值可能不触发。默认每连接最多 10,000 个待触发 watch。适合缓存失效或配置刷新，不能单独作为完整审计/任务事件流。[C API：Watches](https://apple.github.io/foundationdb/api-c.html)、[开发者指南：Watches](https://apple.github.io/foundationdb/developer-guide.html) |
| 持续变更流 | 8.0 新增原生 CDC，但官方把它列为**实验性**；其 C API 要求 API 版本 800，8.0 同时删除旧实验性 ChangeFeed，二者并不兼容。不能把 CDC 当作现成稳定的 Rust 集成能力。[8.0 发布说明](https://apple.github.io/foundationdb/release-notes/release-notes-800.html)、[8.0 C API：CDC](https://apple.github.io/foundationdb/api-c.html) |
| 备份和灾备 | 官方 `fdbbackup` 可连续生成可恢复的一致时间点；`fdbrestore` 恢复期间要求阻止客户端访问，目标范围需事先清理。`fdbdr` 到另一集群是异步复制，灾难切换可能损失最近提交；备份本身仍是必须设计的独立措施。[备份、恢复与 DR](https://apple.github.io/foundationdb/backups.html) |

## 数据模型建议（推论）

使用稳定 ID 而非显示名称作为主键，使用 tuple/subspace 编码为不同访问方式设置独立前缀。例如 `('node', namespace_id, inode_id)` 保存文件/目录身份，`('child', namespace_id, parent_inode, name)` 解析目录项，`('head', inode_id)` 保存当前版本/墓碑，`('copy', file_id, copy_id)` 保存副本及卷代数，`('by_volume', volume_uuid, file_id, copy_id)` 支持按卷枚举。父 inode/名称避免目录重命名时改写全部后代的完整路径键；副本状态变化在同一短事务中维护主记录与反向索引。这是官方索引模式在磁带域上的应用，并非现有实现。[数据建模：tuple/subspace/索引](https://apple.github.io/foundationdb/data-modeling.html)

存储文件**位置元数据**和小型状态，不存文件数据、LTFS 整份索引 XML 或运行日志。官方限制为键 10,000 字节、值 100,000 字节、单事务影响数据 10,000,000 字节；官方还建议键尽量小、值通常低于 10 KB。大量条目导入需分页/分批事务，不能一笔事务覆盖整盘磁带。偏移很大的 key selector 是 O(offset)，分页应以上次返回的最后一个键继续范围读取。[已知限制](https://apple.github.io/foundationdb/known-limitations.html)、[开发者指南：大小与加载](https://apple.github.io/foundationdb/developer-guide.html)

磁带装卸、写入、索引 checkpoint 和跨库复制均不能放进数据库事务：FoundationDB 不支持超过约 5 秒的读写事务，也无法替外部设备提供原子提交。合理模式是短事务登记意图/版本 → 执行设备操作 → 校验结果 → 短事务发布完成状态；故障后靠幂等任务 ID、实际磁带索引和 reconciliation 恢复。这里的磁带流程是从事务限制推导的设计建议。[已知限制：长事务](https://apple.github.io/foundationdb/known-limitations.html)、[开发者指南：长事务与未知提交结果](https://apple.github.io/foundationdb/developer-guide.html)

事务必须处理冲突重试和 `commit_unknown_result`：发生该错误时提交可能已成功，盲目重试非幂等计数/副作用可重复执行。为任务及状态变更使用稳定幂等 ID，并在重试事务里检查其提交标记。不要依赖实验性的自动幂等功能作为生产前提。[开发者指南：未知结果](https://apple.github.io/foundationdb/developer-guide.html)、[自动幂等功能说明](https://apple.github.io/foundationdb/automatic-idempotency.html)

## 部署和 Rust 集成条件

- 先选择 FoundationDB 服务器与客户端的**具体兼容版本**。`foundationdb-rs` 当前发布的 `foundationdb` 0.11.0 是 FoundationDB C API 的异步包装，要求本机安装 FDB 客户端，Rust 至少 1.85.1；其公开 feature 列表止于 `fdb-7_4`，未列 `fdb-8_0`。因此选 8.0/CDC 之前需证明该绑定与 C API 可用，不能由核心数据库 8.0 文档直接推断 Rust 支持。[Rust crate 0.11.0 文档](https://docs.rs/foundationdb/0.11.0/foundationdb/)、[绑定项目 README](https://github.com/foundationdb-rs/foundationdb-rs/blob/main/foundationdb/README.md)、[8.0 发布说明](https://apple.github.io/foundationdb/release-notes/release-notes-800.html)
- FoundationDB 集群要求独立规划服务器进程、持久化数据目录、冗余模式、协调器、SSD 资源、备份代理和监控；不能把一份 SQLite 或 FDB 数据目录通过克隆虚拟机复制成新节点，官方明确禁止这种建群方式。[构建集群](https://apple.github.io/foundationdb/building-cluster.html)、[配置](https://apple.github.io/foundationdb/configuration.html)、[备份](https://apple.github.io/foundationdb/backups.html)
- 数据库不是租户权限边界：官方限制写明，能连入集群的客户端可以读写整个键空间，没有用户级 ACL；应在服务网关、网络和凭据层控制访问，并在应用层校验命名空间授权。[已知限制：安全边界](https://apple.github.io/foundationdb/known-limitations.html)
- FoundationDB 不自带断网节点的“本地继续写、以后自动合并”语义；若离线节点需要浏览缓存或继续工作，需另外定义本地 SQLite 只读缓存/日志及重连校验策略。此处为对官方离线操作限制的设计推论。[Anti-Features：Disconnected operation](https://apple.github.io/foundationdb/anti-features.html)

## 决策前建议验证

先以真实文件量估计：全局文件数、每文件副本数、平均路径长度、单次导入条目量、峰值查询 QPS、目录列举范围与目标恢复时间。然后对候选 Rust/FDB 版本做兼容构建，并在隔离集群用**合成亿级文件**验证：双向索引原子更新、超大目录分页、批量重建、冲突/未知提交结果重试、失去多数派后的行为、备份恢复与节点重加入。该验证清单由以上官方限制与部署模型推导，尚非实测结论。[已知限制](https://apple.github.io/foundationdb/known-limitations.html)、[配置](https://apple.github.io/foundationdb/configuration.html)、[备份](https://apple.github.io/foundationdb/backups.html)
