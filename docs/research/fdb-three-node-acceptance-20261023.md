# 三节点 FDB 隔离验收（2026-10-23）

## 范围

在真实节点 lisa-1/2/3（10.131.7.207/208/209）运行 FoundationDB 7.3.78，但只写合成目录元数据。每节点使用独立目录 `/root/tape-fdb-lab-20261023-550ae75a`、端口 `45150` 和临时 systemd 单元 `tape-fdb-lab-550ae75a.service`；服务设置 `DevicePolicy=closed`，未访问磁带设备，也未修改正式 `ltfsd` 的数据目录、端口或进程。

## 结果

| 检查 | 结果 |
| --- | --- |
| 初始集群 | 三个协调者、`double` 复制、三台机器；`status details` 显示复制 Healthy、可容忍一台机器故障。 |
| 内存配置 | 逐台从 512 MiB 调为进程 4 GiB、systemd `MemoryMax=5G`，每台恢复 Healthy 后再重启下一台；最终最小可用进程内存为 4.0 GB。 |
| 单节点退出 | seed 阶段写入受管命名空间、池、物理带库、卷放置、已验证空卷扫描和未决请求；停止 207 后，在 208/209 上 verify 通过，旧 owner 被拒、新 owner 发布一次、重试幂等。 |
| 三节点目录原语 | `managed_catalog::tests::isolated_fdb_publish_and_page` 在集群上通过（53.50 秒）。 |
| 备份与独立恢复 | `fdbbackup` 生成可恢复文件备份（版本 1197361954）；`fdbrestore` 恢复到本机独立 `127.0.0.1:45151` 实例，`tests/fdb_restart.rs` 的 audit 阶段校验终态通过。 |
| 清场 | 三台测试单元均 `inactive`/`MainPID=0`，端口 45150 无监听；本机备份代理和恢复实例已停止。测试数据、日志和备份保留在隔离目录。 |

本机证据目录：`/work/tmp/tape-rs-fdb-3node-20261023-550ae75a`。其中 `fdb.cluster` 是已停止测试集群的连接文件，`failover.state` 是本次合成命名空间 ID，`backup/` 保存文件备份。复跑须使用新的测试命名空间和独立服务进程，不得将这些目录或连接文件指向正式 `ltfsd`。

后续池更新验收：`ManagedCatalog::update_pool` 对池名和单带文件数上限做 FDB 事务性比较更新。三节点目录测试通过；新建的 `pool-failover.state` 在 seed 阶段创建并更新池，停 207 后 verify 阶段在 208/209 读到新值，并完成旧 owner 拒绝与新 owner 发布。207 恢复后三机复制再次 Healthy、Fault Tolerance 1，随后三台测试单元均已停止。此操作仍只发生在隔离 FDB 命名空间，没有进入正式 `ltfsd` 管理路径。

池 revision 续验：原型键前缀从 `catalog/11/` 升为 `catalog/12/`，池记录新增从 1 开始的单调 revision，以阻止“改走又改回”后的旧管理请求通过内容比较。隔离本机 FDB 的目录测试和编解码测试通过；三节点新命名空间 `pool-revision-failover.state` 的 seed 与停 207 后 verify 各 1/1 通过，读回 revision 2 的新池值和文件终态。207 恢复后三机复制 Healthy、Fault Tolerance 1；再停三台测试单元，确认 `MainPID=0`、45150 无监听，正式 17420/17421 仍监听。旧实验键保留、不迁移，也不作为生产数据。

磁带分配续验：隔离原型前缀升至 `catalog/13/`。`assign_tape` 按真实带库序列号、条码和 pool 预留磁带，不需要预先知道卷 UUID；owner 核验后 `register_volume_placement` 以 epoch 栅栏将 UUID 原子写进分配记录、卷主记录和条码反向索引。未绑定磁带可解绑，已绑定磁带即使反向索引丢失也不能直接解绑；文件准入同时核对绑定记录。编解码截断、幂等、跨命名空间冲突、并发卷争夺、反向索引损坏保护和旧 owner 拒绝在隔离本机 FDB 目录测试通过；本机服务重启后的 seed/verify/audit 各 1/1 通过。三节点 `assignment-v13-failover.state` 在 Healthy/Fault Tolerance 1 下 seed 1/1；停 207 后 verify 1/1，未绑定分配可读、旧 owner 绑定拒绝、新 owner 绑定及文件发布通过；恢复 207 后 audit 1/1，三机重新 Healthy/Fault Tolerance 1。三节点目录原语测试 1/1 通过（53.40 秒）。测试结束三台临时单元均 inactive/MainPID0、45150 无监听，正式 17420/17421 仍监听；本机恢复实例也已停止。默认测试 34 套件 500 通过、4 忽略、0 失败；格式、两种 all-targets check、严格全特性 Clippy 和 diff 检查通过。该续验仍只写合成元数据，没有真实磁带/生产进程接线；旧 `catalog/12/` 数据不迁移。

受管导入续验：原型前缀升至 `catalog/14/`。`begin_managed_import` 建立不可见候选，Building 阶段可登记带库/池/磁带及 owner 栅栏下的卷放置，并完成文件与命名空间独立摘要核验后 seal；sealing 前控制查询也返回未就绪。新命名空间磁带索引支持有界废弃清理，已登记的条码必须在当前 owner 下显式释放；释放前检查候选副本、卷反向索引、就绪指针和全部扫描退役。三节点目录原语测试覆盖一个带文件的成功导入，以及另一个带文件的废弃候选：扫描未退役或副本未清空时拒绝释放，清理后释放并续清候选（最终运行 1/1，通过用时 53.83 秒）。本机实际 FDB 服务重启的 seed/verify/audit 各 1/1 通过；三节点新命名空间 `managed-import-v14-failover.state` 在三机 Healthy/Fault Tolerance 1 下 seed 1/1，停 207 后 verify 1/1（Building 仍不可读、验证后 seal），恢复 207 后 audit 1/1，三机再次 Healthy/Fault Tolerance 1。最终三台临时单元 inactive/MainPID0、45150 无监听，正式 17420/17421 仍监听。默认测试 34 套件 500 通过、4 忽略、0 失败；格式、两种 all-targets check、严格全特性 Clippy 和 diff 检查通过。旧 `catalog/13/` 实验键未迁移；只写合成 FDB 元数据，未连接正式 `ltfsd` 或真实磁带。

## 生产切换判定

**不能切换。** 当前正式 `ltfsd` 仍以 Raft 控制状态和每节点 SQLite `directory.db` 管理池、磁带、文件与任务；本次 FDB 集群没有接入运行时、HTTP/FUSE 或真实介质提交。208 上的正式目录库只读检查为 3 个池、0 盘已归属磁带、0 条文件、0 条暂存记录；这只说明该正式实例当前目录规模，不能代表所有隔离候选和现场介质。

切换前必须完成唯一权威迁移、真实库存和 LTFS UID/卷索引核对、未决任务和 owner fencing 恢复、从 FDB 备份恢复后的介质对账，以及亿级数据与故障压测。当前测试没有验证网络分区、TLS/访问控制、备份自动化或真实带读写。现场高文件量磁带可用于获准写入/迁移压测，**不得格式化**。

### 已确认索引证据续验（catalog/17）

`ManagedCatalog` 的受管卷扫描现在要求一个 FDB 中不可变的索引来源记录，将卷 UUID、扫描 ID、索引分区/起始块及原始索引 SHA-256 绑定到节点本地已完成的 extent 缓存 manifest。未登记或损坏的来源记录不能把 Building 扫描封口为 Ready，也不能通过 Ready 读门槛；同一扫描的相同来源可重试，位置或摘要变化会冲突。本地缓存仍为可重建数据，FDB 记录不包含逐文件 extent。`tests/fdb_restart.rs` 使用合成 LTFS XML 验证高位 fileuid 的路径、长度和 extent 在重新打开缓存后可读；不是物理磁带扫描证据。

三节点隔离集群再次以 207/208/209、端口 45150 启动，`status` 为三机器、三 zone、`double`、复制 Healthy、Fault Tolerance 1。新命名空间 `source-v17-3node.state` 的 seed 阶段通过；停止 207 后，208/209 的 verify 通过，含旧 owner 拒绝、新 owner 单次发布和 FDB 来源记录加本地缓存读取；恢复 207 后 audit 通过，三副本回到 Healthy/Fault Tolerance 1。三节点综合目录事务测试再通过 1/1（54.38 秒），日志 `/work/tmp/tape-rs-fdb-source-v17-3node-integrated-20261023.log`。本机隔离 FDB 的真实服务停服/重启也通过同一 seed/verify。最后三台临时单元均 `inactive`/`MainPID=0`、45150 无监听，正式 17420/17421 仍监听；本机 45117 临时 FDB 已停止。

这一轮只使用合成索引和隔离元数据。生产执行器尚未将真实 `build_confirmed` 缓存登记到 FDB，逐文件目录记录也尚未与索引 fileuid、路径和长度独立对账；正式 `ltfsd` 继续使用 Raft/SQLite，不能切到 FDB。没有访问 `/dev/sg*` 或操作现场介质。

### 逐文件 UID/长度续验（catalog/18）

R 物理副本值增加 8 字节文件长度，SHA-256/MD5/Unknown 三种值分别为 105/89/73 字节。受管卷的 `verify_volume_scan_page_with_cache` 在每页 FDB 事务推进进度前，按副本 fileuid 查询由同一来源索引构建的节点本地缓存并核对长度；不提供缓存的原分页接口返回 `CacheRequired`，不把只核对 FDB 内部摘要当作介质核验。命名空间封口还核对当前 head 的版本和长度与当前物理副本一致。缺 UID、长度不符、缓存缺失或索引身份不符均拒绝。旧 `catalog/17/` 只是隔离实验数据，新前缀不迁移它。

本机 FDB 7.3.78 真停服/重启后的 `tests/fdb_restart.rs` seed/verify 各 1/1 通过，包含源缓存、旧 owner 拒绝、新 owner 发布和 R 长度读回。207–209 三机 Healthy/Fault Tolerance 1 下新 `length-v18-3node.state` seed 1/1；停止 207 后在 208/209 上 verify 1/1；恢复 207 后 audit 1/1，三机再次 Healthy/Fault Tolerance 1。三节点综合目录事务测试 1/1（54.01 秒），日志 `/work/tmp/tape-rs-fdb-length-v18-3node-integrated-20261023.log`。默认软件测试 34 套件 510 通过/6 忽略/0 失败；启用 FDB 特性的完整测试 34 套件 532 通过/8 忽略/0 失败，日志分别为 `/work/tmp/tape-rs-fdb-length-v18-default-tests-20261023.log` 与 `/work/tmp/tape-rs-fdb-length-v18-feature-tests-20261023.log`。格式、两种 all-targets 编译、严格全特性 Clippy、diff 和 CodeGraph sync/affected 通过。

验收后 207/208/209 临时单元均 inactive/MainPID0，45150 无监听；正式 17420/17421 仍监听。本机 45117 临时 FDB 已停止。测试没有打开 `/dev/sg*`、写磁带或格式化。**生产仍不能切换**：正式执行器没有调用真实索引缓存构建/登记，Ready 后新增提交也尚未由介质证据在 FDB 层逐项约束；EE 用户路径到 `.LTFSEE_DATA` 物理路径的映射、完整 LTFS 属性/增量索引、唯一权威迁移和亿级端到端性能尚未完成。高文件量磁带不得格式化。

## 2026-10-23：受管路径查询与卷不可用屏障（catalog/19）

`resolve_path` 在一个 FDB 读事务中定位目录项、当前副本、卷扫描来源和物理放置。卷放置登记即写入 `W/<namespace>/<volume>`；重扫取消保留该屏障，扫描 Ready 原子清除。屏障存在时未知路径返回 `Incomplete`，不误报 404；已就绪的其他卷文件仍可读取。旧 `catalog/18/` 隔离数据未迁移。

本机 FDB 7.3.78 的综合目录测试 1/1 通过。207–209 原有隔离目录和 45150 端口重新启动后，三节点 `double`、复制 Healthy、Fault Tolerance 1；新代码综合测试 1/1（53.83 秒）。停 207 临时进程后，208/209 再次通过 1/1（53.77 秒）；恢复 207 后复制重新 Healthy、Fault Tolerance 1。随后三台临时单元均停止为 inactive/MainPID0，45150 无监听；正式 17420/17421 仍监听。本机 45117 临时实例也已停止。未访问磁带设备、写带或格式化，生产 `ltfsd` 未改。

默认 `cargo test --quiet` 与 FDB 特性的 `cargo test --features fdb --quiet` 均通过；`cargo fmt --all -- --check`、两种 all-targets `cargo check`、严格全特性 Clippy、`git diff --check` 通过。仍未验收亿级规模或真实文件读写，正式服务仍以 Raft/SQLite 为权威。

## 2026-10-23：逻辑路径到 LTFS 物理读取计划

本地 extent 缓存新增按路径的有界 SQLite 点查，按原始索引文件证明复核 UID、长度和 extent；一层 EE 用户符号链接解析到 `.LTFSEE_DATA`，越根、断链、链式链接及非空无 extent 均拒绝。`ManagedCatalog::resolve_read` 结合 FDB 当前逻辑副本与同一扫描来源缓存，拒绝 UID/长度不一致；真实 EE gen12 XML 夹具和合成受管导入/错误 UID 测试通过。缓存仍是可重建的节点本地物理位置，不是共享目录权威；返回读取计划不授予设备 owner/PR 资格。

本机隔离 FDB 7.3.78 综合测试 1/1。207–209 隔离 45150 集群三节点复制 Healthy/Fault Tolerance 1 时综合测试 1/1（54.18 秒）；停 207 后 208/209 再次 1/1（54.14 秒）；恢复 207 后复制重新 Healthy。最终三台临时单元 inactive/MainPID0、45150 无监听，正式 17420/17421 仍监听，本机 45117 临时实例关闭。没有部署/修改正式 `ltfsd`，也没有访问磁带设备、写入或格式化。

默认测试 34 套件 512 通过/6 忽略；FDB 串行测试 34 套件 535 通过/8 忽略。第一次并行全特性测试在 HA 接管窗口的既有管理测试失败：预期“设备 owner 尚未就绪”，实际收到“驱动库存已过期”。该 HA 套件单独串行 13/13 通过，之后全特性串行全量通过；原失败日志 `/work/tmp/tape-rs-fdb-read-v19-feature-tests-20261023.log` 保留，不能据此断言时序根因已解决。fmt、两种 all-targets check、严格全特性 Clippy、diff 检查通过。生产目录、任务和管理权威仍是 Raft/SQLite，真实介质/亿级端到端验收仍缺失。

## 2026-10-23：大 LTFS 属性分块（catalog/22）

207 历史 `directory.db` 只读检查发现单个文件的 `metadata` 达 590,725 字节，含九个约 64 KiB 的 xattr；这超过最初试验的单 FDB 值阈值。`NodeAttributes` 因此改为 64,000 字节以内内联，较大属性按 64,000 字节拆块，与 inode/文件发布同一事务写入。清单保存块数、长度及 SHA-256，读回和命名空间验证逐块核对；当前编码属性上限 4,000,000 字节。隔离本机 FDB 综合测试通过约 590 KiB 属性的写入、更新、查找和故意缺块的损坏检测；服务实际停服/重启后的 seed、verify、audit 也通过。

207–209 独立 `45150` 集群三进程 Healthy、Fault Tolerance 1 时，大属性综合测试 1/1 通过（59.30 秒）。新命名空间 `catalog22-chunk-failover.state` 的 seed 1/1；停止 207 测试进程后 208/209 verify 1/1；恢复 207 并等三进程重回 Healthy 后 audit 1/1。随后三台测试单元均 inactive/MainPID0、45150 无监听，正式 FDB `45151` 仍为三进程 Healthy/Fault Tolerance 1，正式 `ltfsd` 的 17420/17421 仍监听。现场只使用合成元数据；未访问 `/dev/sg*`、读写或格式化磁带。

默认串行测试 34 套件 512 通过/6 忽略/0 失败；最终源码的 FDB 全特性串行测试 34 套件 536 通过/8 忽略/0 失败。fmt、两种 all-targets check、严格全特性 Clippy、diff 检查通过，CodeGraph sync/affected 已执行。真实高文件量索引尚未全量扫描，上限仍待验证；生产 `ltfsd` 没有接入 FDB，正式切换条件未满足。
