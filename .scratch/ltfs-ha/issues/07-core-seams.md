# 共享引擎、双前端与磁带执行的模块 seam

Type: grilling
Labels: wayfinder:grilling
Status: claimed
Assignee: claude
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 03, 04, 05, 06, 08

## Question

按已确定的可靠性与文件契约，哪些状态由 ltfsd 独占，Native/FUSE 两个 adapter 通过何种 Interface 访问？定义嵌入模式互斥部署条件，真实 SG_IO 与故障注入 adapter 的 TapeTransport seam，区分协议编码、文件语义、设备位置与 HA 控制。给出最小模块契约与测试表面，而不是先按文件数量拆实现任务。

## Context

[提交票据的驱动器执行草案](04-durability.md#驱动器串行执行与提交状态草案)提出每驱动器统一排序，集中维护位置、索引提交隔离和错误门控；接收缓冲与下一批修改可并行，不引入全库数据写锁。后续模块设计须承接经确认的提交契约；本段只传递上游候选设计，不提前认领或解决本票据，也未选定线程、锁或 channel 实现。

[索引提交期间的有界接收与背压](04-durability.md#已裁定索引提交期间的有界接收与背压)已获用户接受。执行模块的 Interface 需要覆盖有界接收、按快照范围确认、数据队列满时必要控制请求仍可推进，以及所有权失效后的停止接收/写入行为；模拟器验收应从同一 Interface 注入队列饱和与提交失败。具体额度、等待与回收策略及并发原语仍待细化，不因该确认解除本票据其他依赖。

用户于 2026-09-17 给出顶层架构：Python Client API → Rust LIB API ∥ FUSE 客户端 → ltfsd（挂载、池化管理、IO 读写）→ 带库。本票据后续须把 [资源与状态模型](../ee-resource-model.md) 的池管理和 [任务调度](../ee-task-scheduling.md) 的队列明确划入 ltfsd 独占状态，并定义 Rust LIB 与 FUSE 共用的 Interface；Python 层只封装 Rust LIB，不单独定义 seam。不因此提前认领或解除本票据依赖。

## Comments

### 依自动接受授权采用：模块 seam 首版候选（2026-09-17）

05 的接口语义、37/38 的独占状态与队列、39 的健康事件已可支撑模块边界。依自动接受授权采用[模块 seam 候选](../module-seams.md)，要点：

- 五个模块：tape-scsi（CDB/sense/`TapeTransport`）、ltfs-format（纯编解码）、ltfsd-core（引擎，独占全部运行状态）、control-plane（Raft/许可/控制库，不碰设备）、ltfsd-api（FileService + AdminService + Events）；两个适配器 tape-sdk（Python 为其绑定）与 tape-fuse 只经 ltfsd-api。
- 四个关注点分层不混用：协议编码、文件语义、设备位置、HA 控制；`TapeError` 不穿透到 API，core 映射为文件契约错误类别，适配器再映射为 errno/异常。
- 现有代码归属明确：`ScsiDevice` 升为 `SgIoTransport`，`MediumChanger`/`TapeDrive` 借用 trait；`volume.rs` 拆入 S0—S6；`catalog/` 成为可重建发布视图；CLI 拆为管理客户端与显式维护模式。
- ltfsd-core 独占十类状态表；池/归属/角色由 control-plane 持有并只读发布。
- `TapeTransport` seam 有 `SgIoTransport` 与 `SimTransport` 两个实现；模拟通过不等于设备协议已验证。
- 进程内模式：共用控制目录锁、`StaticPermit`、仅 single-node 配置允许；同一用例集在进程内与 IPC 两种绑定复跑。
- 测试面按模块列出纯单元/模拟/现场三栏，串起既有 DV/PV/U/RV/K/C/TS/RS/FR/FT/FC/CG 用例编号。

**仍待上游**：04 的 D02/D03 收敛后细化 VolumeCommit/Recovery 步骤；08 的 RTO 起终点定后再给恢复时间预算；IPC 编码、数据通道、FUSE 库与 PyO3 选型属实施。本票据保持 claimed，03/04/05/06/08 依赖不解除。
