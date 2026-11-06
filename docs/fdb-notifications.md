# FDB 持久通知存储

通知事件日志、消费者、待确认批次与注入收据保存在 FDB。外部系统主动连接本项目的 HTTP/SSE 入口，无需配置外部回调地址。`ltfsctl notify` 提供相同操作；服务端需要启用 FDB，客户端默认构建即可。管理任务状态、周期带库库存和原生磁带巡检已接入同事务事件发布及管理展示。设备采集遵守当前 owner、PR 和库级维护互斥；查询展示已有观测，不为每次 list/show 装带扫描。测试注入带有独立标记，不能当成真实设备状态变化。

消费者以带库、活动 namespace 和稳定的消费者 ID 标识，创建时返回 generation。连接可在不同进程重新建立，只有显式 ACK 才推进进度；断线、租约到期、发送次数增加都不会丢弃积压。外部系统需要保存消费者 ID，并在重连时查询原 generation，不能每次创建新 ID 来代替恢复原订阅。

每个消费者同时只保留一个待确认批次。重连后获得相同 delivery ID 和不可变事件；待确认记录以 SHA-256 绑定原批次身份、范围及事件内容，处理完成后使用 generation、delivery ID 和 through 确认整个批次；不允许跳跃或部分确认。重复确认已经完成的位置不推进进度。投递语义为至少一次，消费者须按 event_id 幂等处理。删除消费者是显式放弃其未确认工作；同名重建会产生新 generation，旧连接和旧 ACK 无权操作新消费者。

事件类型可以在创建/重连或 `expand_notify_types` 时扩大，不能缩小；空类型列表表示全部类型，与显式列出所有类型等价。原类型继续补发积压，新类型仅接收扩展事务之后的事件；未确认批次保持原内容。池、条码资源过滤条件及初始位置不能借重连修改。首次创建可选择从日志开头或当前日志头之后开始。

连接状态通过独立 session 标识隔离。旧连接的关闭或心跳不能覆盖新连接；45 秒租约仅用于显示连接是否中断，不代表外部系统整体健康，也不控制事件保留。消费者状态区分 connected/disconnected、backlogged/waiting_ack/caught_up；没有连接且有积压时显示 interrupted。

支持 tape.state.changed、tape.location.changed、tape.assignment.changed、tape.alert.raised、library.monitor.changed、library.inventory.changed 和 task.status.changed。事件注入要求非零请求 UUID、已知类型及不超过 8 KiB 的 JSON 对象，同一 UUID 重试必须保持相同类型和内容；注入事件明确标注 injected，只写通知日志，不修改真实库存、策略、任务或设备。

单批投递最多 16 条日志、192 KiB；底层每次最多取 16 条记录后按字节预算截断；单事件上限 90,000 字节。过滤后没有可见事件的批次仍可能包含已扫描的位置，必须确认其完整批次才能继续。日志缺口或损坏必须报错，不能通过推进 ACK 跳过。FDB 不可用时操作失败，不回退到 SQLite 或内存事实源。事件写入失败会使所在事务回滚，防止业务状态已提交而通知丢失。

通知使用独立的持久键空间，连接超时和一般目录清理不删除事件；当前没有自动日志回收。HTTP 网络链路与隔离 FDB 故障测试不能代替真实带库和现场 HA 验收，后两项仍需随后续集成完成。


## HTTP 管理接口

接口挂在 FDB metadata/owner HTTP router 上，任意配置了该库路由的节点都可以访问同一份 FDB 状态，无需等待设备 owner；这些请求不打开设备。沿用现有管理端口的可信网络部署边界，没有新增外部系统密码、地址或健康探测配置。

每个请求通过 `?library=<真实 changer 序列号>` 选择带库；HTTP 单库时可以省略，CLI 始终要求 `--library`。消费者 ID 只接受 1–64 个字母、数字、`-`、`_`。UUID 使用规范的非零 UUID 字符串。请求 JSON 不接受重复或未知的封套字段。类型过滤规则与上文相同。

| 方法 | 路径（均可带 library 查询参数） | 请求 / 行为 |
| --- | --- | --- |
| PUT | `/admin/notify/consumers/{id}` | `{"filter":{"pool":null,"barcodes":[],"event_types":["tape.state.changed"]},"from_start":false}`；创建或幂等恢复原订阅，只能扩大类型 |
| GET | `/admin/notify/consumers` | 可带 `after=<id>&limit=100`；返回 consumers 与 next，单页 1–100 |
| GET | `/admin/notify/consumers/{id}` | 查询 generation、确认位置、积压与连接诊断 |
| GET | `/admin/notify/consumers/{id}/events` | 必须带 `generation=<UUID>`；取一个持久待确认批次，不自动 ACK |
| GET | `/admin/notify/consumers/{id}/stream` | 必须带 generation；可加 `event_types=tape.state.changed,task.status.changed` 扩大类型；空值表示全部类型 |
| POST | `/admin/notify/consumers/{id}/ack` | `{"generation":"UUID","delivery_id":"UUID","through":123}`；完整批次处理并持久保存后确认 |
| DELETE | `/admin/notify/consumers/{id}` | `{"generation":"UUID","abandon_unacknowledged":true}`；明确放弃积压，不能用来恢复断线 |
| POST | `/admin/notify/events/inject` | `{"operation":"UUID","event_type":"tape.state.changed","data":{"test":true}}`；同一 operation 重试必须保留原内容 |

SSE `event: notify` 的 `data` 为完整批次，`id` 为 delivery_id。连接每 10 秒发送注释心跳，不因已发送而确认；同一连接等待外部 ACK 后才继续下一批。发送失败只结束该连接，保留原待确认批次；外部系统重连后继续处理。SSE `notify-error` 包含 error 与 retryable：暂时 FDB 故障可以重连，消费者失效或日志损坏应停止并检查。HTTP 参数无效返回 400，暂时依赖/容量故障返回 503，记录损坏返回 500，慢请求体超时返回 408。

每个 HTTP router 最多 64 个通知管理请求和 64 个 SSE 流，超额立即返回 503；SSE 有独立名额。请求体上限 16 KiB、读取限时 10 秒，流发送等待最多 10 秒，SDK 单响应上限 2 MiB，SSE 单帧上限 256 KiB。FDB 操作沿用目录事务超时和重试限制。SDK 单次管理 RPC 总预算默认 60 秒，各端点尝试也受剩余预算约束，临时故障按有上限的退避与抖动切换节点；持续 watch 则直到调用方停止或出现永久错误，连接、响应头、无帧等待分别限时 3/10/30 秒。回调返回错误会直接向调用方返回，不自动重试或 ACK。

## CLI

```sh
cargo build --release --bin ltfsctl
# 默认 target 下生成 target/release/ltfsctl；设置 CARGO_TARGET_DIR 时在其 release 目录。
ltfsctl -e 192.0.2.11:7401 -e 192.0.2.12:7401 --library LIBRARY_SERIAL notify create external --event-type tape.state.changed
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify show external
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify list
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify read external --generation GENERATION_UUID
ltfsctl -e 192.0.2.11:7401 -e 192.0.2.12:7401 --library LIBRARY_SERIAL notify watch external --generation GENERATION_UUID --event-type tape.state.changed --event-type task.status.changed
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify ack external --generation GENERATION_UUID --delivery-id DELIVERY_UUID --through 123
# 仅在确实要放弃积压时删除：
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify delete external --generation GENERATION_UUID --abandon-unacknowledged
ltfsctl -e 192.0.2.11:7401 --library LIBRARY_SERIAL notify inject --operation OPERATION_UUID --event-type tape.state.changed --data '{"test":true}'
```

`create --from-start` 才回放已有历史，否则从创建时的日志头之后开始；重复 create 的资源过滤和 from-start 必须与首次相同。`create --barcode` 可重复；全局 `--pool` 用于 create 的资源过滤时须为规范的非零池 UUID。`watch --all-event-types` 扩大到全部类型；不传类型选项沿用原集合，不能缩小。普通命令输出 JSON，watch 每批输出并刷新一行 JSON；打印与进程正常退出都不是 ACK。接收程序须完整、可靠地保存或处理整个批次后再 ACK，使用 event_id 幂等，不能对每条事件部分确认。

SDK 是同步接口；在异步应用中应通过 `spawn_blocking` 调用。watch 在网络轮询的运行时之外执行用户回调，回调内可以在处理完成后显式调用 `acknowledge_notify`，然后返回 true 继续接收下一批。

带库库存与巡检事件源、缓存新鲜度和输入边界见 [FDB 带库观测](fdb-library-observations.md)。
