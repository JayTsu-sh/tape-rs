# 任务与调度候选（借鉴 IBM EE MMM）

关联：[研究 38](issues/38-ee-task-scheduling-evidence.md) · [资源与状态模型](ee-resource-model.md) · [驱动器串行执行草案](issues/04-durability.md#驱动器串行执行与提交状态草案) · [计划切换排空边界](issues/18-raft-execution-contract.md#已裁定计划切换的排空与失败边界) · [失去资格后的命令边界](issues/18-raft-execution-contract.md#已裁定失去执行资格后的命令边界) · [性能验收](performance-acceptance.md)

状态：依自动接受授权采用、经 [38 限定研究](issues/38-ee-task-scheduling-evidence.md#answer)核对的首版候选。仅规划；不选定队列实现、线程/channel 模型或任何数值。调度器只在持有当前有效运行许可的 Leader 执行实例内运行，每次派发前仍须通过 18 的本地门控。

## 任务分类

| 类别 | 任务类型候选 | 优先级 | 所需驱动器角色 | EE 对应 |
| --- | --- | --- | --- | --- |
| 读取 | 文件读取/召回（客户端触发）、批量预取 | H | r | transparent / selective recall |
| 写入 | 上传批次的卷提交（含 time/size 触发）、显式 flush | M | m | migrate / premigrate / save |
| 磁带维护 | assign（含格式化）、unassign、validate、reconcile、reclaim、replace、export、offline/online、import、move | L | g（reclaim 需两台） | eeadm tape * |
| 驱动器维护 | drive assign/unassign/up/down、角色变更 | L | — | eeadm drive * |
| 目录重建 | 接管后的库存重扫、按卷索引读取 | 由恢复流程驱动，不进普通队列 | r/g | MMM 启动时重建库存 |

客户端触发的读取在本项目由原生接口或 FUSE 适配器直接提交为 H 任务，没有 EE 的 HSM 拦截层。

## 任务身份与状态

- **ID**：`<接管轮次标识>-<本轮序号>`。轮次标识来自 18 的接管轮次记录（Raft 中），序号为 Leader 本地单调递增。新 Leader 对旧轮次 ID 的回答固定为“已终止轮次，结果未知”。
- **状态**：waiting → running ⇄ interrupted → succeeded | failed | aborted。aborted 只由执行实例终止（换 Leader、资格失效、强制停止）产生，不由任务自身错误产生。
- **子任务**：写入任务按目标池拆子任务，子任务按驱动器写队列拆 tasklet；每 tasklet 的完成与 04 的卷提交切点对齐，不在切点之间上报“部分成功”。
- **逐文件结果**：succeeded / failed(code) / duplicate / wrong_pool / not_found / too_small / too_early 保留为结果分类；“too_early”对应上传尚未满足 time/size 触发。
- **限流**：按类别设接受上限，超限立即拒绝且不分配 ID；上限数值留空。读取类与其他类分开计数。
- **历史**：Leader 本地保留已完成任务及逐文件结果；保留期与“最近 N 个失败任务始终保留”的 N 留空。

## 队列排序与抢占

排序：running 先于 waiting/interrupted；同状态按优先级 H > M > L；同优先级按开始时间。

抢占规则：

1. H 任务需要的磁带正被 L 任务（回收/对账/校验）占用时，L 任务进入 interrupted，在最近的安全切点释放驱动器，H 完成后自动恢复。安全切点由 04 的驱动器串行执行草案定义：不在索引提交序列中间释放，不在 tasklet 未达提交切点时释放。
2. H 任务不抢占 M 任务正在进行的索引提交；M 任务的 tasklet 边界是可抢占点。
3. L 任务之间不互相抢占。
4. 抢占次数、最长等待和饥饿保护参数留空，由 06 的性能验收补测。

互斥：对同一磁带，reconcile/reclaim/replace/export/validate 互斥且与写入互斥；reconcile 开始前等待该池在途写入完成，开始后拒绝该池新写入直到完成。同一时间的 reconcile 数量上限留空（EE 为 1）。

## 磁带与驱动器选择

写入任务选择目标磁带时按顺序过滤：

1. 池内磁带，管理归属 assigned，健康 ok，追加资格 appendable，位置非 missing/ieslot。
2. 池指定代次时，磁带代次与候选驱动器支持代次都匹配。
3. 当前向该池写入的驱动器数未达 mountlimit。
4. 容量账本可满足本批预算（04 的准入）。
5. 多候选时优先已装载在可用驱动器上的磁带；其次按 fillpolicy（首版：可用空间最大）。

驱动器选择：受管组内、管理意图 enabled、运行态 not_mounted 或已装载目标磁带、具备任务所需角色位；多候选时优先已装载目标磁带者，其次空闲最久者。

读取任务：同一磁带的多个读取请求合并为一个批次，按起始块排序执行；批次大小上限留空。RAO（SSC-5 Recommend Access Order）作为可选优化保留，首版不实现。多副本时的副本选择规则见 38，首版单副本。

## 换 Leader 时的任务处理

| 情形 | 处理 |
| --- | --- |
| 计划切换 | 按 18 的排空边界：停止接受新任务，等待 running 任务到安全切点或完成，排空成功后转移；到期不强制取消 |
| 资格失效 / 崩溃 | 旧实例按 LG 停发；新 Leader 不读取旧队列，所有旧轮次任务对外视为 aborted |
| 新 Leader 上任 | 先完成 H0–H3 与目录重建，再开放队列；重建期间提交的任务排队但不派发 |
| 客户端查询旧 ID | 返回“已终止轮次，结果未知”；读取任务由客户端直接重提，上传任务按 05 的结果未定规则核对后重提 |
| 运维任务 aborted | 不自动重提；磁带侧若已部分完成（如 reclaim 已复制部分文件），由目录重建时的磁带状态反映，重提时按当前状态继续 |

## 候选验证 TS01—TS08

- TS01：超过类别上限的任务被立即拒绝且不占用 ID；未超限任务得到含轮次前缀的 ID。
- TS02：回收进行中到达同带读取请求，回收在安全切点释放驱动器、读取完成后回收自动恢复，且回收的中间结果不丢失。
- TS03：读取请求到达时目标磁带正处于索引提交序列，读取等待到提交切点后才获得驱动器。
- TS04：mountlimit=1 的池有两批并发写入时，第二批等待而不是装载第二盘磁带。
- TS05：两盘候选磁带中一盘已装载，写入任务选择已装载者。
- TS06：换 Leader 后，旧 ID 查询返回“已终止轮次”，新任务 ID 使用新轮次前缀，旧队列不被派发。
- TS07：reconcile 开始后该池新写入被拒绝，在途写入完成后 reconcile 才开始。
- TS08：同带十个读取请求被合并为按块序执行的一个批次，装载次数为一次。

均未实现/执行。参数留空：各类上限、历史保留期、抢占等待上限、批次大小、reconcile 并发数。
