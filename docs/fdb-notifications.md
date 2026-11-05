# FDB 持久通知存储

本模块提供通知事件日志、消费者、待确认批次和注入请求的 FDB 存储 API。当前入口是 `ManagedCatalog` 的 `*_notify_*`、`notify_batch`、`acknowledge_notify` 和 `inject_notification` 方法；本批尚未开放 HTTP/SSE、CLI，也未接入真实库存、磁带监控或任务状态生产者，不能据此认定外部推送已完成。

消费者以带库、活动 namespace 和稳定的消费者 ID 标识，创建时返回 generation。连接可在不同进程重新建立，只有显式 ACK 才推进进度；断线、租约到期、发送次数增加都不会丢弃积压。外部系统需要保存消费者 ID，并在重连时查询原 generation，不能每次创建新 ID 来代替恢复原订阅。

每个消费者同时只保留一个待确认批次。重连后获得相同 delivery ID 和不可变事件；待确认记录以 SHA-256 绑定原批次身份、范围及事件内容，处理完成后使用 generation、delivery ID 和 through 确认整个批次；不允许跳跃或部分确认。重复确认已经完成的位置不推进进度。投递语义为至少一次，消费者须按 event_id 幂等处理。删除消费者是显式放弃其未确认工作；同名重建会产生新 generation，旧连接和旧 ACK 无权操作新消费者。

事件类型可以在创建/重连或 `expand_notify_types` 时扩大，不能缩小；空类型列表表示全部类型，与显式列出所有类型等价。原类型继续补发积压，新类型仅接收扩展事务之后的事件；未确认批次保持原内容。池、条码资源过滤条件及初始位置不能借重连修改。首次创建可选择从日志开头或当前日志头之后开始。

连接状态通过独立 session 标识隔离。旧连接的关闭或心跳不能覆盖新连接；45 秒租约仅用于显示连接是否中断，不代表外部系统整体健康，也不控制事件保留。消费者状态区分 connected/disconnected、backlogged/waiting_ack/caught_up；没有连接且有积压时显示 interrupted。

支持 tape.state.changed、tape.location.changed、tape.assignment.changed、tape.alert.raised、library.monitor.changed、library.inventory.changed 和 task.status.changed。事件注入要求非零请求 UUID、已知类型及不超过 8 KiB 的 JSON 对象，同一 UUID 重试必须保持相同类型和内容；注入事件明确标注 injected，只写通知日志，不修改真实库存、策略、任务或设备。

单批投递最多 16 条日志、192 KiB；底层每次最多取 16 条记录后按字节预算截断；单事件上限 90,000 字节。过滤后没有可见事件的批次仍可能包含已扫描的位置，必须确认其完整批次才能继续。日志缺口或损坏必须报错，不能通过推进 ACK 跳过。FDB 不可用时操作失败，不回退到 SQLite 或内存事实源。事件写入失败会使所在事务回滚，防止业务状态已提交而通知丢失。

通知使用独立的持久键空间，连接超时和一般目录清理不删除事件；当前没有自动日志回收。网络发送重试、真实状态生产者与现场 HA 验收仍需随后续集成验证。
