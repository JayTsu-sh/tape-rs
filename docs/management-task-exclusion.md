# 管理命令互斥

本项目以物理 Library 为管理任务的互斥范围：同一库内的冲突任务不能同时执行，不同库可以并行。该范围由用户于 2026-11-07 明确确认；不将 EE 文档未说明的跨库范围当作已实测事实。

## EE 依据

本地参考 `eeadm-tape-commands-1.4.0.md` 的 export、reclaim、replace、unassign 章节均列出双向限制：这些任务运行时不能启动 Cluster Restore、Cluster Save、Library Rescan、Tape Cleanup、Tape Export、Tape Reclaim、Tape Reconcile、Tape Replace、Tape Set、Tape Unassign；反方向同样成立。

本项目不依赖 GPFS，不实现用户已排除的 reconcile。未实现的 export、replace、cluster save/restore、cleanup 不能因为出现在参考清单中就显示为可执行。

## 本项目约束

| 操作 | 同库约束 | 当前实现边界 |
|---|---|---|
| library list/show、pool list/show、tape list/show、task list/show | 可读取已提交状态 | 查询不取得任务互斥权，不以查询返回授予设备操作权限 |
| library rescan | 与同库 reclaim、unassign、其他 rescan 和 tape set 互斥 | 运行前持久化原任务的库占用；原任务完成后释放 |
| tape reclaim | 与同库 rescan、unassign、其他 reclaim 和 tape set 互斥 | 原父任务持有占用，复制子任务不另建竞争任务；完整原生 reclaim 尚未开放 |
| tape unassign | 与同库 rescan、reclaim、其他 unassign 和 tape set 互斥 | 保留既有零引用、卷静默和机械任务约束 |
| tape set / offline | 与同库正在运行的上述任务互斥 | 纯目录修改在单个 FDB 事务中检查占用；多个纯目录提交由事务线性化 |

互斥检查、运行状态与占用写入必须在同一 FDB 事务内完成。CLI 和 HTTP 使用同一后端，不能只在命令行进程内加锁。允许保存排队请求，但排队不代表已获执行权；冲突请求不得变为 running 或执行设备命令。

占用绑定原 namespace、Library、操作类型、请求代次、operation 和指纹。HA 接管继续同一身份；旧 owner 被原执行权检查拒绝。缺失、损坏或他人占用不得自动修复、清除或当作空闲。超时、客户端断线和取消请求均不能直接释放可能仍在执行的任务。只有已确认终态或确认未执行的安全关闭路径可以在同一事务中清除精确自有占用。终态重试先返回原回执，不清除后续任务的占用。

这些规则补充已有池、卷、路径、驱动器和机械手约束，不放松介质身份、PR fencing、逐 CDB 权限或格式化确认。

## 参数互斥

EE 的 datamigrate/reclaim 规定 `-L` 与 `-S` 互斥，二者也分别与 `--use-readonly-drive` 互斥；节流选项只适用于单盘显式磁带请求。这些属于请求合法性检查，不能用任务互斥锁代替。当前原生 reclaim 尚未开放，不增加未实现选项或兼容分支；以后开放参数时，CLI 与 HTTP 必须共同校验。

## 验收要求

逐对验证 A 阻止 B 和 B 阻止 A；同一任务重复请求保持原身份；同库跨池仍互斥，异库不因本互斥槽被阻塞。覆盖 FDB 故障、提交成功但回执丢失、任务未结束的取消、HA 接管、占用缺失及损坏。只读查询继续可用。软件注入与真实 B 库验证分别记录，新增规则在实物验证前不得标记已验收。

### 已验证的现场路径（2026-11-07）

三节点普通 release `fa7a2d732fd6ce60d0eb68e5770d234aefdb7d0e` 在 B605d 与独立 FDB 45153 上完成以下限定路径验证。测试使用 L00284L8、L00285L8；原 namespace 和数据保留，没有格式化或擦除。

- 内部 reclaim 原父任务在零子任务状态持有库/池占用时，CLI rescan、tape set、unassign 及 HTTP rescan 均保持 queued；管理查询及 S3 已提交正文读取继续可用。
- 普通 S3 写入返回 503 与明确的“池或带库正在执行互斥操作”，原始 XML 同样包含该原因；被拒写对象最终 HEAD 为 404。已知池占用时不访问设备的调用顺序由隔离 FDB/Sim 回归验证。
- 按原 handle 安全关闭后，两个 rescan 和无策略变化的 set 成功；unassign 被保留副本引用拒绝。使用最新修订号另查该引用保护，归属保持不变。CLI 与 HTTP 重放返回原终态回执；4097 字节新正文与 41 字节旧正文校验成功，14 类占用为空，三节点就绪。

原验收脚本因误要求专用 rescan 终态重放返回 200 而退出 1；现有路由及既有测试规定返回 202，现场实际正文与原成功回执完全一致。保留该失败记录，再以只读补验核对原响应、任务、数据和占用，没有重发管理操作或 PUT。证据为本地 `/work/tmp/tape-reclaim-resume-20261107/EXCLUSION-UPLOAD-ACCEPTANCE-SETTLEMENT-v1.json`。

不同 library 并行、同库跨池及占用缺失/损坏的证明来自软件与隔离 FDB 测试；正常 HA 的零子任务路径另有下述实测。本轮没有逐对运行全部物理互斥组合，也未操作已释放的 A 库。完整原生 reclaim 的复制与重初始化流程仍未开放，零子任务占用验证不代表该流程实带验收。

### 正常 HA 的零子任务路径（2026-11-07）

普通部署源码仍为上述 `fa7a2d7`。207（node1、round 701278）持有原父占用、三个冲突请求排队时正常停机；208（node2、round 727643）接管 B 库。新节点的 executor 在 Serving 前通过既有 `claim_reclaim_reservations_page` 领取原父任务。库占用、原任务索引和池屏障字节均保持，父记录仅更新 owner；原意图、Reserved 阶段及零子任务不变，三个原请求仍逐字 queued。HA 恢复不能要求原父 owner 保持旧值直到外部程序手工领取。

旧 owner 的状态查询与释放均被拒绝，拒绝后占用仍在。续接只枚举已存在的原父任务，不调用创建预留接口；新 owner 上的 S3 已提交数据可读，普通写入仍返回明确 PoolBusy。按原 handle 安全关闭后，原 CLI/HTTP rescan 成功、unassign 被引用保护拒绝，三个终态均属于新 owner。随后 207 正常加入，三节点就绪，原 CLI 和 HTTP 202 回执重放不变，4097/41 字节正文完整，被拒写对象 HEAD 为 404，14 类占用为空；两个私有无设备测试 helper 均已退出。

原控制器误要求 HA 后父记录仍归旧 owner，因此退出 1，失败记录保留。经原状态及源码核对后，原操作续接控制器退出 0，没有替换父任务或重新提交管理请求。证据位于本地 `/work/tmp/tape-reclaim-resume-20261107/ha-exclusion-20261107-v1/recovery-v2/COMPLETE.json`。本轮未部署新产品代码、未格式化或擦除、未在受管流程外修改 PR；它不覆盖复制中或元数据准备任务的 HA、断电、内核冻结及跨库物理并行。
