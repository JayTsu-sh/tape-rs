# 模块 seam 候选：ltfsd 独占状态、双前端 Interface 与传输适配

关联：[票据 07](issues/07-core-seams.md) · [文件契约](file-contract.md) · [资源与状态模型](ee-resource-model.md) · [任务与调度](ee-task-scheduling.md) · [故障分类](ee-fault-recovery.md) · [卷提交状态机](commit-state-machine.md) · [容量账本](capacity-reservations.md) · [SQLite 控制库](control-store-sqlite.md) · [raft-rs 绑定](raft-rs-binding.md) · [性能验收](performance-acceptance.md)

状态：依自动接受授权采用的首版候选，票据 07 保持 claimed。**实施进度（2026-09-17）**：`TapeTransport` trait（`src/scsi/transport.rs`）与 `SimTransport`/`SimLibrary`（`src/scsi/sim.rs`）已实现；`ScsiDevice` 实现该 trait，`TapeDrive`/`MediumChanger`/`Mam`/`LtfsVolume`/`mkltfs`/`standard_inquiry`/`sync_from_device` 改为借用 `&dyn TapeTransport`，CLI 调用点零改动；状态分类逻辑抽为 `device::finish_command` 供两者共用。`tests/sim_ltfs.rs` 在模拟器上跑通 mkltfs→mount→append→commit→掉电→重挂、故障注入、换带器库存/搬运；本机 `cargo test` 29 项通过，新代码 clippy 无告警。未触及硬件。只定义模块职责、独占状态、接口面和测试面；不定义 crate 名称之外的类型签名、线程/channel 模型、IPC 编码或依赖选型。基于仓库当前 `src/` 结构提出迁移方向，不是已完成的重构。

## 顶层架构与模块对应

```
Python Client API
   └─ tape-sdk (Rust LIB)          tape-fuse (low-level FUSE)
            └────────────┬──────────────┘
                    ltfsd-api（Interface：FileService + AdminService + Events）
                         │  进程内直接调用 或 本机 IPC
                    ltfsd-core（引擎：独占全部运行状态）
        ┌────────────────┼────────────────┐
   control-plane     ltfs-format      tape-scsi
 (Raft/许可/控制库)   (纯编解码)     (CDB + TapeTransport)
                                          │
                                   SgIoTransport ／ SimTransport
                                          │
                                        带库
```

四个正交关注点各归一个模块，不得跨层混用：

| 关注点 | 模块 | 不承担 |
| --- | --- | --- |
| 协议编码与传输 | tape-scsi：CDB 构造、sense 解析、`TapeTransport` seam、changer 地址映射 | 不知道文件、池、任务、许可 |
| 文件语义 | ltfsd-api 定义，ltfsd-core 实现 | 不出现 CDB、sg 路径、errno |
| 设备位置与资源 | ltfsd-core 的资源模型（37）与库存比对 | 不出现在适配器 |
| HA 控制 | control-plane：Raft 绑定、运行许可、控制库 | 不执行任何设备命令、不读写文件数据 |

## 现有代码的归属

| 现有路径 | 目标模块 | 变化 |
| --- | --- | --- |
| `src/scsi/{sg_io,cdb,sense,inquiry}.rs` | tape-scsi | 保持；`ScsiDevice::execute*` 提升为 `TapeTransport` trait 的 `SgIoTransport` 实现 |
| `src/scsi/device.rs` | tape-scsi | `ScsiDevice` 成为 `SgIoTransport`；`TapeError::{ScsiCommand,Busy,ReservationConflict,ScsiHost,ScsiDriver,InvalidResponse}` 留在本层 |
| `src/changer/*`、`src/tape/commands.rs` | tape-scsi | `MediumChanger`/`TapeDrive` 改为借用 `&dyn TapeTransport`（或泛型），行为不变；地址映射规则不变 |
| `src/ltfs/{index,label}.rs`、`mam.rs` 的编解码部分 | ltfs-format | 纯函数编解码，无设备依赖 |
| `src/ltfs/{volume,mkltfs}.rs`、`mam.rs` 的读写部分 | ltfsd-core 的 VolumeSession / 维护任务 | mount/append/commit 拆成 S0—S6 状态机步骤，由 DeviceExecutor 串行派发 |
| `src/catalog/*` | ltfsd-core 的本地发布视图 | 明确为可重建缓存；`sync_from_volume` 成为目录重建的一步 |
| `src/cli/*`、`src/main.rs` | 拆为 `tape-rs` 管理/诊断 CLI（AdminService 客户端）与显式维护模式（直连 tape-scsi，需显式确认标志） | 诊断直连不得与运行中的 ltfsd 争用同一设备组，靠 36 的锁文件互斥 |
| `src/error.rs` | 各层各自错误类型 | `TapeError` 不再穿透到 ltfsd-api；core 映射为文件契约的错误类别 |

## ltfsd-core 独占的状态

以下状态只存在于 ltfsd-core 进程内，适配器不得持有权威副本；适配器缓存必须标为非权威且可被 `RecoveryInProgress` 作废。

| 状态 | 子模块 | 权威性 | 参考 |
| --- | --- | --- | --- |
| 运行许可与接管轮次 | PermitGate（读 control-plane 发布的许可） | 控制面权威在 Raft，本地只读副本 | 18、33 |
| 资源目录：驱动器运行态、磁带健康/追加资格/容量/位置、库存比对结果 | ResourceModel | 可重建 | 37 |
| 任务队列、子任务、tasklet、任务历史 | Scheduler | 本地 | 38 |
| 上传会话、有界接收缓冲、会话期限 | SessionRegistry | 本地 | 04 |
| 容量账本：预留、滚动额度、收尾预算、校准切点 | Ledger | 本地，校准自设备 | capacity-reservations |
| 每卷提交状态 S0—S6、冻结快照、dirty 集合 | VolumeCommit | 本地；持久化事实在磁带 | commit-state-machine |
| 每驱动器串行执行队列与安全切点 | DeviceExecutor | 本地 | 04 驱动器串行执行草案 |
| 本地发布视图（路径→版本→extent、文件状态） | PublishedView（现 catalog） | 可重建缓存 | 37、05 |
| 恢复进度与可信视图标记 | Recovery | 本地 | recovery-evidence |
| 健康状态与事件流 | Health | 本地 | 39 |

池定义、磁带归属、驱动器分配/角色、受管组配置由 control-plane 持有（Raft 状态机应用后发布给 core 只读）。

## Interface：ltfsd-api

三组接口，进程内与 IPC 两种绑定必须语义一致：

- **FileService**（文件契约的全部操作）：`put / write / complete / commit / wait / cancel / status / stat / list / read / read_partial / rename / unlink / mkdir / rmdir / get_xattr / set_xattr / commit_namespace`。错误为文件契约的 12 个类别；任务 ID 含接管轮次前缀。
- **AdminService**（EE 风格运维）：`pool {create,delete,set,list,show}`、`tape {assign,unassign,validate,reconcile,reclaim,replace,export,offline,online,import,move,list}`、`drive {assign,unassign,up,down,set,list}`、`task {list,show,cancel,clearhistory}`、`library rescan`、`node list`、`cluster {failover,drain,status}`。运维任务默认异步返回任务 ID。
- **Events**：订阅健康与任务事件（39 的事件类别），适配器用于作废缓存和向应用报告降级。

约束：
- 接口以路径 + 版本描述文件，不暴露卷/extent/块地址；`stat` 返回的 `location` 仅供运维展示。
- 每个请求携带调用方看到的接管轮次；core 对旧轮次请求返回 `TerminatedRound`。
- 数据传输：进程内绑定按流（Reader/Writer）；IPC 绑定的数据通道与控制通道分离，避免 04 的“数据队列饱和阻断控制请求”。具体编码留空。
- Python Client API 只封装 tape-sdk，不直接绑定 ltfsd-api。

## 适配器职责

| | tape-sdk | tape-fuse |
| --- | --- | --- |
| 持有 | 连接、任务 ID、未确认任务清单由应用持有 | 句柄表（fh→任务/读上下文）、目录项缓存（非权威） |
| 映射 | 错误类别→Rust 枚举/Python 异常 | 错误类别→errno（文件契约表） |
| 不做 | 重试、去重、猜测结果 | 把 RecoveryInProgress 翻译成 ENOENT；缓存跨 ESTALE 复用 |
| 事件 | 可选订阅 | 必须订阅，用于作废缓存与句柄 |

FUSE 使用 low-level 接口，inode 由 core 的路径版本派生，不在适配器分配持久 inode。

## 传输 seam：TapeTransport

```
trait TapeTransport {
    fn execute(&self, cdb: &[u8], dir: Direction, buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult>;
    fn identity(&self) -> &DeviceIdentity;   // 序列号、类型，不是 /dev/sg 路径
}
```

- `SgIoTransport`：现 `ScsiDevice`，保持 `0x2285` 与 `ioctl_readwrite_bad!`，不变。
- `SimTransport`：脚本化设备模型，支持库存、装卸、写/读块、文件标记、EOD/EOM、sense 注入、短传输、延迟错误、命令挂起与掉电快照。用于 DV/TS/RS/FR/FC 全部无硬件用例。
- 上层只依赖 trait；DeviceExecutor 按驱动器持有一个 transport，changer 单独一个。
- 现场验证与模拟验证分开记录（AGENTS.md 要求）；SimTransport 通过不等于设备协议已验证。

## 进程内模式

同一 ltfsd-core 可被 tape-sdk 直接链接（无 IPC、无 Raft），用于单机专用部署与测试。互斥条件：

1. 与守护进程共用 36 的控制目录锁文件；锁被占用即拒绝启动，不得换目录绕过。
2. 进程内模式使用 `StaticPermit` 控制面（许可恒有效、单轮次），仅当受管组配置声明 `single-node` 时允许；配置含 Raft 成员时拒绝进程内模式。
3. 进程内模式的 FUSE 适配器同样经 ltfsd-api 调用，不另开捷径。
4. 测试用例以进程内模式 + SimTransport 运行；同一用例集在守护进程模式下通过 IPC 复跑作为绑定一致性检查。

## 控制面 seam

- control-plane 向 core 发布只读的 `Permit{round, instance, scope, deadline}` 与配置快照（池/归属/角色）；core 每次派发前读 PermitGate（18 的本地门控），不调用 Raft。
- core 向 control-plane 提交的只有低频控制提案（接管登记、隔离意图、授权终止请求、配置变更）和可用性报告；不提交任务、文件或容量变化。
- control-plane 不持有 transport；隔离动作的设备侧执行由 core 在 H1 阶段按控制面意图完成。

## 测试面

| 模块 | 纯单元 | 模拟（SimTransport + StaticPermit） | 现场 |
| --- | --- | --- | --- |
| tape-scsi | CDB 编码、sense 解析、地址换算、截断输入 | 传输错误分类 | INQUIRY/库存冒烟 |
| ltfs-format | 标签/索引/MAM 编解码往返、增量链解析 | — | 读真实卷索引 |
| DeviceExecutor | 切点判定 | TS02/TS03、LG01—LG06 | — |
| VolumeCommit | 状态转换表 | DV01—DV08、PV01—PV05、U01—U05、RV01—RV05 | D01 屏障、掉电 |
| Ledger | 账本不变量 | K01—K06、C01—C06 | 校准取数 |
| Scheduler | 排序/过滤 | TS01—TS08 | — |
| ResourceModel | 四维状态守卫 | RS01—RS06 | 库存比对 |
| Recovery | 候选发现规则 | SC/AP、R1—R5 | 异常尾部 |
| Health/Events | 事件编码 | FR01—FR07、FT01—FT04 | 09 隔离验证 |
| ltfsd-api + 两适配器 | 错误映射表 | FC01—FC10 在 sdk 与 fuse 各跑一遍 | — |
| control-plane | 状态机应用 | CG/QG/RQ/SG 等 | 实验室三节点 |

## 仍待上游收敛

- 04 的 D02/D03 未收敛前，VolumeCommit 与 Recovery 的具体步骤只到状态与切点粒度。
- 08 未定 RTO 起终点前，目录重建与恢复开放的时间预算不进入接口承诺。
- IPC 编码、数据通道实现、FUSE 库与 PyO3 选型属于实施，不在本候选内。
