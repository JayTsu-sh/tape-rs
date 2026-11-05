# FDB 持久管理任务账本

本批提供 `admit_management`、`management_task`、`list_management_tasks`、`next_management`、`cancel_management` 和管理请求窗口查询。它只持久排队和查询请求，不应用池/磁带策略、不执行库存扫描或设备命令。设备 actor、执行中安全取消、管理 CLI/HTTP 接线仍需后续完成；queued 不能解释为命令已成功执行。

请求须先通过统一 `ManagementRequest::parse`，并固定带库、namespace、epoch、operation、完整 change 及指纹。接纳和取消要求精确活动路由与当前 FDB owner epoch；owner 接管后可用原身份查询、重试，旧 owner 不得继续修改任务。FDB owner 校验不代替 Raft/PR 设备执行许可。

任务和队列键按 namespace、带库、epoch、operation 组织；分页游标校验完整带库作用域。其他带库的队列及损坏记录不会混入本库列表和 next 查询。管理请求窗口仍使用 namespace 初始化的保留范围，当前没有窗口轮换或自动任务历史回收。

每库最多保留 100 个排队/执行中的任务索引；达到上限返回 `ManagementQueueFull`，调用方应退避重试。完全相同的已提交请求先返回原回执，即使当前队列已满；同一身份使用不同参数会拒绝。请求校验、容量检查、回执、队列索引及 task.status.changed 通知在一个 FDB 事务中完成。事务结果不明时继续使用原 operation 查询或重试，不能换新 ID 重新发送控制意图。

未开始的 queued 任务可原子变为 cancelled，并解除其排队索引；重复取消返回原终态。running 任务会拒绝此入口的直接取消，保留回执及索引，由后续对应设备 actor 处理实际副作用和恢复。终态回执仍保留，查询不会重启或重放任务。

通知记录明确由 tape-rs 生成，与测试注入区分。通知投影绑定原任务状态、原事件位置及 SHA-256；接纳、查询和转换会核对投影与不可变日志，孤立或提前写成目标状态的投影不能跳过通知。通知失败会回滚任务变更，防止“任务已入队但通知不存在”；查询发现本库任务/队列不一致时失败，不静默补写。临时 FDB 故障不切换 SQLite 或内存队列。

隔离 FDB 测试验证共享查询、owner fencing、幂等、队列背压及并发争用、带库作用域、queued 取消、running 拒绝清理和通知故障原子回滚。这里没有访问真实带库，不能替代设备执行和实带 HA 验收。
