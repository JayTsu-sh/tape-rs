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
