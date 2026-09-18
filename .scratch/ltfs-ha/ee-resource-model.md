# 资源与状态模型候选（借鉴 IBM EE）

关联：[研究 37](issues/37-ee-resource-state-model.md) · [EE 架构边界](issues/14-ibm-ee-architecture.md) · [最小控制状态](issues/18-raft-execution-contract.md#已接受的业务控制状态范围) · [SQLite 控制库](control-store-sqlite.md) · [容量账本](capacity-reservations.md) · [任务与调度候选](ee-task-scheduling.md)

状态：依自动接受授权采用、经 [37 限定研究](issues/37-ee-resource-state-model.md#answer)核对的首版模型候选。仅规划；不定义 SQL DDL、Rust 类型或 CLI 参数，不操作设备。EE 的页码引用见 37，本文不重复标注。

## 实体

本项目首版的资源实体及其与 EE 概念的对应：

| 本项目实体 | 对应 EE 概念 | 身份 | 说明 |
| --- | --- | --- | --- |
| 受管设备组 | 节点组 + 逻辑带库 | 带库序列号 + 逻辑库标识 | 一个接管范围；首版只有一个组，Leader 独占其全部驱动器与 changer |
| 控制节点 | EE 节点 / 控制节点 | Raft 成员 ID | 三个投票节点；EE 的“节点拥有各自驱动器集合”在首版收缩为“当前 Leader 拥有整组” |
| 驱动器 | Tape drive | 驱动器序列号 | 设备节点路径不是身份；属性：类型/代次、角色位 m/r/g、管理意图（assigned/disabled/unassigned） |
| 磁带池 | Tape pool | 池 UUID（名称可重命名） | 上传目标、容量监控和导出/回收的管理单位 |
| 磁带 | Tape cartridge | 条码 + LTFS 卷 UUID | 条码来自库存，卷 UUID 来自 LTFS 标签；两者不一致即 label_mismatch |
| 卷 | LTFS volume | 卷 UUID | 见 [CONTEXT](../../CONTEXT.md)；一盘磁带承载至多一个卷 |

**不采纳的实体**：GPFS 文件系统、stub 文件、DMAPI 属性、迁移策略规则。文件层面沿用 04 的上传任务/已提交状态，而不是 resident/premigrated/migrated。

## 池属性候选

| 属性 | EE 有无 | 首版处理 |
| --- | --- | --- |
| 名称、UUID | 有 | 采用；UUID 为身份 |
| 介质类型/代次限制（devtype、format、mediarestriction） | 有 | 采用为准入守卫：池内磁带类型一致，驱动器须支持该代次 |
| worm | 有 | 保留字段，首版不支持 WORM 操作语义 |
| 所属节点组 | 有 | 首版恒为唯一受管组 |
| mountlimit | 有（默认 0 无限） | 采用为调度输入：同时用于向该池写入的驱动器上限 |
| fillpolicy | 有（Default） | 保留字段；首版只支持“优先已装载磁带，否则按可用空间”一种策略 |
| lowspacewarning*/nospacewarning* | 有 | 采用两类告警事件；阈值、检查周期留空，不绑定 SNMP |
| mode | 有（normal） | 保留；用于表达池级“禁止追加”（对应 append_fenced 的来源） |

## 磁带状态：四个正交维度

EE 把归属、健康、追加资格和临时故障混在一个 21 值枚举里，并用 Table 10-5 列出每种状态允许的命令。本项目改为四个维度，命令准入表达为各维度上的守卫条件：

| 维度 | 取值候选 | 主要来源 | 归属 |
| --- | --- | --- | --- |
| 管理归属 | unassigned、assigned(pool)、exported、offline(note) | 管理命令 | Raft 控制状态 |
| 介质健康 | ok、require_validate、need_replace（读错误）、require_replace（写错误）、check_library（装卸失败）、non_supported、label_mismatch | 设备/LTFS 事件 | Leader 本地目录 |
| 追加资格 | unformatted、appendable、append_fenced（池级禁止）、data_full（只接受元数据）、full、recall_only（写保护） | 卷提交状态机 + 容量账本 + 池 mode | Leader 本地目录 |
| 位置 | homeslot、ieslot、drive(serial)、missing、duplicated、inaccessible | 带库库存 | 派生（库存 vs 归属意图） |

对应关系示例：EE 的 `missing` = 归属 assigned 且库存中不存在；`duplicated` = 库存出现两个同条码；`disconnected`（LE 实例未运行）在本项目对应“无当前执行实例”，不作为磁带状态。EE 的 `need_unlock` 是处理永久写错误时的中间态，本项目对应 require_replace 的进入过程，不单列。

**命令守卫示例**（替代 Table 10-5 的矩阵）：

- 接受上传：归属 assigned，健康 ok，追加资格 appendable，位置非 missing/ieslot，且通过 04 的容量准入。
- 读取/召回：归属 assigned 或 offline 之外的任一，健康非 non_supported/label_mismatch，位置可达；need_replace 允许读但触发替换任务。
- assign：归属 unassigned，位置 homeslot，健康非 non_supported；未格式化则先格式化（沿用 EE 的四种可格式化条件，否则须显式强制）。
- unassign：池内无被引用文件（沿用 EE 的“有效空”检查，命中第一个活动文件即拒绝）。
- export/offline：归属 assigned；先预留、对账，再转换归属；导出后位置可为 ieslot。
- move 到 ieslot：归属为 exported/offline/unassigned 之一。

## 驱动器状态

| 维度 | 取值候选 | 归属 |
| --- | --- | --- |
| 管理意图 | unassigned、assigned+enabled、assigned+disabled；角色位 m/r/g | Raft 控制状态 |
| 运行态 | not_mounted、mounting、mounted、in_use(task)、unmounting、standby | Leader 本地 |
| 健康 | ok、check_drive（硬件错误）、not_installed、missing（意图有、库存无）、check_path（路径失效） | 派生/本地 |

角色变更在当前任务完成后生效，与 EE 一致；禁用驱动器时若有装载磁带，任务完成后自动卸回 homeslot。

## 状态归属

| 层 | 内容 | 权威性 | 变更频率 | 恢复方式 |
| --- | --- | --- | --- | --- |
| Raft 控制状态 | 受管组成员与配置版本；池定义与属性；磁带归属意图（含 offline 备注、export 记录）；驱动器分配与角色；加上 18 已定的接管轮次、隔离意图、执行授权 | 管理意图的权威 | 低频（运维命令） | Raft 日志/快照，见 [控制存储恢复](control-storage-recovery.md) |
| Leader 本地目录 | 磁带健康、追加资格、容量估计（usable/used/available/reclaimable）、位置缓存；驱动器运行态；文件→副本→磁带放置索引；任务队列与历史 | 可重建缓存，不是权威 | 高频 | 接管后由库存重扫 + 按需装载读取索引重建；重建完成前对应卷不开放，见 04 的 R1—R5 |
| 磁带 | LTFS 标签/卷 UUID、索引、文件内容、扩展属性；MAM | 已提交文件状态的权威 | 每次卷提交 | 见 [卷提交状态机](commit-state-machine.md) |
| 带库实时库存 | 元素状态、条码、驱动器存在性 | 物理事实 | 每次查询 | READ ELEMENT STATUS；与归属意图比对得出位置维度 |

EE 在“池增删、磁带/驱动器 assign/unassign、节点增删、控制节点变更”后自动备份数据库，这组操作与本表第一层完全重合，是把它们放入 Raft 的直接依据。EE 的磁带目录和空间估计由 MMM 在控制节点上维护并在故障切换时重建，对应本表第二层。

**修订（2026-09-17，来自 [42](issues/42-pool-namespace-scaling.md)）**：文件→磁带的放置目录、每盘带的代数与用量改为随每次卷提交写入 Raft 日志，使新 Leader 不必重装磁带即可回答查询。目录条目永远带 (条码, 索引代数)，只是磁带事实的缓存，不一致时以磁带为准，因此不构成第二份权威。下面这段是修订前的理由，保留作背景；其中"磁带健康"仍留在 Leader 本地，"容量"随提交摘要进入 Raft。

**不进入 Raft 的原因**：磁带健康与容量随每次写入变化，逐次共识违反 18 的“普通 I/O 不逐条共识”；放置索引可由磁带索引重建，进入 Raft 会让共识日志成为第二份权威。

**Raft 状态与物理事实冲突时**：以物理为准生成派生状态并告警，不自动修改意图。例如 assigned 磁带库存缺失 → missing，等待运维决定 unassign 或找回；库存中出现未登记磁带 → unassigned 待处理，不自动入池。

## 磁带上的自描述标记

EE 在磁带上的布局见指南 §10.2，并经实验室实测核对：数据存为 `.LTFSEE_DATA/CLID-FSID-IGEN-INO`，原 GPFS 路径上是相对符号链接，数据文件的扩展属性记原路径（详见[卷内布局与外来卷兼容](volume-layout-interop.md)；此前本段写"具体内容原文未给出"是漏读，已更正）。本项目候选：卷根用 LTFS 规范 F.4 的标准键 `ltfs.mediaPool.uuid` 与 `ltfs.mediaPool.name` 记录池成员，不自定义属性；仅作为 import 时的提示和 label_mismatch 之外的一致性检查，不作为归属权威；导入未登记磁带时以运维显式指定为准。是否同时写 MAM 0808h 留待实施核对（MAM 写入是有状态操作，见 AGENTS.md）。

## 候选验证 RS01—RS06

- RS01：三个节点上 Raft 应用同一序列的池/归属/角色变更后，各自派生的资源视图一致；Leader 本地目录不同步给 Follower。
- RS02：新 Leader 从 Raft 状态 + 库存重扫重建目录，重建期间对应卷不接受上传，重建后的归属与旧 Leader 一致，健康/容量允许暂时为 unknown。
- RS03：assigned 磁带从库存消失/重复出现/卡住时，分别得到 missing/duplicated/inaccessible，且不改变 Raft 归属。
- RS04：含有被引用文件的磁带拒绝 unassign 与换池；空磁带 unassign→assign 到另一池要求重新格式化。
- RS05：写错误后磁带进入 require_replace，只允许读取与替换任务；替换完成后源带脱离池。
- RS06：池 mode 切换为禁止追加后，池内磁带追加资格变为 append_fenced，在途提交按 04 的排空边界收尾。

均未实现/执行。参数留空：池属性默认值、告警阈值与周期、库存重扫触发条件、目录重建的时间目标（归 08）。
