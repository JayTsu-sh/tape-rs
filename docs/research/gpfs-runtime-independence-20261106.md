# GPFS 独立运行核对（2026-11-06）

用户明确要求本项目与 GPFS 解耦。约束见 [ADR 0002](../adr/0002-gpfs-independent-runtime.md)，适用于当前历史 WIP 集成候选和后续部署。

## 运行边界

| 能力 | 本项目使用的事实源与执行链路 |
|---|---|
| library / pool / tape 管理 | FDB 持久任务与归属，本项目库存扫描和 SCSI 执行器 |
| 首次导入、已分配卷索引刷新 | 磁带 LTFS 索引和完整来源证明，流式暂存及 FDB 原子发布 |
| S3 / FUSE 访问 | FDB 逻辑目录、本项目的介质定位与 LTFS 数据读写 |
| 状态监控与通知 | 自身 SCSI / TapeAlert / MAM 采样，FDB 状态、事件及消费确认 |

生产环境不要求 GPFS 文件系统、挂载点、DMAPI、HSM stub、EE 缓存、EE 服务或 `eeadm` / `mmapplypolicy`。设备所有权仍需要 Raft 多数派许可、FDB owner epoch 及 SCSI PR。客户端 FUSE 还需要 Linux FUSE 支持及相应挂载工具；这与 GPFS 无关。

`-P /archive` 表示池内逻辑目录，不在宿主机或 GPFS 中创建 `/archive`。索引中的 `/ee-gpfs/...` 是原保存路径文本，不代表导入时要访问该路径。带内 `ibm.ltfsee.gpfs.*` 属性与 `.LTFSEE_DATA` 数据对象关联必须保留解析，避免破坏 EE 格式互通。

## 核对方法与证据

核对范围包含生产源码、Cargo 依赖和仓库提供的部署文件，未发现 GPFS/EE SDK、动态库、子进程或服务调用依赖。源码中的相关引用是带内格式解析、明确边界说明及测试样本。没有为了消除字符串而删除格式解析器，也没有新增替代外部服务。

本机未安装 `/usr/lpp/mmfs`、`/opt/ibm/ltfsee`，没有 GPFS 挂载，也无 `eeadm`、`leadm`、`mmgetstate`、`mmapplypolicy`、`mmfsadm` 命令。使用真实 B 库 EE 导入任务1017留下的 Full Gen5 XML，验证两条独立路径：

- `tests/ee_objects.rs::real_b_ee_index_maps_saved_objects_without_gpfs_runtime`：识别三条原保存路径、文件长度、EE 别名及 `.LTFSEE_DATA` 数据范围，保持原始索引属性。
- `tests/fdb_import_spool.rs::real_b_ee_index_builds_streaming_sources_without_gpfs`：在私有本地文件中流式构建导入清单与 extent 缓存，完整核验二者，按单条候选续页，解析八个目录与三条文件别名到真实数据 UID 和长度。该测试不连接 FDB 或访问设备。

首轮新测试把目录候选也当作文件，断言失败；已按导入 API 同时返回目录和文件的实际契约修正断言，保留首轮日志。它没有暴露生产代码调用 GPFS 的问题。

两组相关测试合计15项通过、0失败，另6项需要显式环境的测试保持忽略。流式清单测试的 `strace` 显示仅执行测试程序自身一次，网络 `connect` 为0，GPFS/EE 安装目录及原保存挂载路径访问为0；这只描述该离线测试的实际行为。Rust 格式检查及 `git diff --check` 通过。

`cargo clippy --all-targets --all-features` 执行成功；仍有集成代码的未使用项、参数数量及测试模块位置等告警，本次新增测试无告警。全量集成审查需继续处理这些事项，不能将命令退出0当作整个 WIP 已审查通过。

原始检查结果、源码摘要和测试日志保存在 `/work/tmp/tape-wip-integration-evidence-20261106` 的 `GPFS-*`、`test-gpfs-*` 文件中。版本固定为 main `e37ba633` 与 WIP `82ec54c6` 的未提交集成候选；本文件不代表已合入 GitHub。

## 实物验收边界

上述测试使用真实索引样本，属于纯软件验证，不是本次集成候选的物理正文读回。真实 EE 参考调查的 assign、premigrate 和 import 成功，GPFS recall1019失败（errno13）；失败独立保留，不作为项目链路成功或运行前置条件。

当前集成候选仍需完成机械执行互斥和分配后索引来源等语义集成，随后在 B 库验证无 GPFS 挂载、无 EE 服务依赖的 CLI/HTTP、导入刷新、S3/FUSE 数据链路与故障恢复。A 库已借出，不访问；不通过全局停用研究用 GPFS 来影响其他使用者。所有已消费的 EE 实验启动器均不得重跑。
