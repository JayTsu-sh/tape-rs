# 外部系统持久通知

外部系统主动连接 tape-rs 管理 HTTP 服务，无需在 tape-rs 中配置接收端 URL，也无需部署消息代理。状态更新事务把事件同时写入 FDB；消费者、过滤条件、待确认批次和 ACK 进度也保存在 FDB。SSE 连接可以落在任意配置了相同库路由的节点，不要求它是库的 active node。

## 订阅与接收

通知按带库隔离。多库系统为每个带库创建独立消费者；同一库内的不同外部系统使用不同消费者 ID。一个消费者只代表一条处理进度，新连接接替旧连接；交接时可能重复投递，不能当作自动分工的工作队列。

```sh
ltfsctl -e 10.131.7.219:7401 -l LIBRARY_SERIAL notify create ops-platform --event-type tape.state.changed
ltfsctl -e 10.131.7.219:7401 -l LIBRARY_SERIAL notify show ops-platform
ltfsctl -e 10.131.7.219:7401 -l LIBRARY_SERIAL notify watch ops-platform --generation GENERATION_UUID
```

创建结果返回 `generation`（本次订阅实例身份）。默认从创建事务读取到的日志尾部之后订阅；`--from-start` 回放本版本记录并保留的历史，不会补造功能启用以前的状态变化。需要当前完整状态时另查 `library show`、`tape list/show`。第一次全量同步应先创建订阅，再取当前快照，并按事件版本处理随后积压的变化。

`notify create` 可重复指定 `--barcode`、`--event-type`；全局 `-p` 在创建时解析为池 UUID 过滤条件。默认关注该库全部事件。相同 ID、相同过滤条件和起点的创建幂等，不重置进度。事件类型可以扩大：新集合必须包含原集合的全部类型，空集合表示全部；池、条码及起点不得改变。缩小类型或更换资源范围时使用新的 ID，原订阅的未确认历史继续保留。

`notify watch` 每行输出一个完整批次 JSON，通过 SSE 接收并在断线后跨 `-e` 地址重连。输出、TCP 写入或 SSE 的 `Last-Event-ID` 均不代表外部业务处理成功，客户端绝不自动 ACK。

## 批次和确认

```json
{
  "generation": "消费者实例 UUID",
  "delivery_id": "批次 UUID",
  "from": 100,
  "through": 103,
  "events": [
    {
      "version": 1,
      "event_id": "namespace:library:101",
      "event_type": "tape.state.changed",
      "sequence": 101,
      "namespace": "命名空间 UUID",
      "library": "真实带库序列号",
      "observed_at": "UTC RFC3339 时间",
      "data": {
        "barcode": "AA0017L8",
        "pool": "池 UUID 或 null",
        "previous": {"state": "appendable"},
        "current": {"state": "offline"},
        "reason": "policy_changed"
      }
    }
  ]
}
```

日志序号是同一命名空间、同一带库的提交顺序，跨 owner 继续递增；不同库之间没有全局顺序。`events` 只包含命中过滤条件的事件，所以序号可以不连续，也可能是空数组。`through` 是本批次完整扫描到的位置；空的过滤批次同样须 ACK，才能继续后面的通知。无新事件时 `delivery_id=null`，无需 ACK。

外部系统完整处理并可靠保存整个批次后执行：

```sh
ltfsctl -e 10.131.7.219:7401 -l LIBRARY_SERIAL notify ack ops-platform \
  --generation GENERATION_UUID --delivery-id DELIVERY_UUID --through 103
```

同一消费者只有一个待确认批次。过滤条件不变时，在确认前断线重连、切换节点、服务重启都重放相同 `delivery_id` 和相同事件。确认必须匹配当前实例、批次及完整结束位置，不能任意跳到日志尾。扩大事件类型不改变已有待确认批次。ACK 响应丢失可以原样重试，重复 ACK 不回退进度。外部系统按 `event_id` 去重；业务更新与去重记录应在外部系统自己的事务中一起提交。

这是至少一次交付。HA 可能造成短暂中断或重复，不承诺网络层恰好一次。可靠性依赖已提交 FDB 数据仍可恢复、订阅未被注销以及事件历史未被外部手动清理。FDB 不可用时暂停投递和确认，不能绕过持久化先发通知。

## HTTP 接口

所有接口使用管理服务现有网络访问边界；`library=SERIAL` 为查询参数，多库必须指定。接口不接受回调 URL，不直接连接外部服务。

| 方法与路径 | 作用 |
|---|---|
| `PUT /admin/notify/consumers/{id}?library=SERIAL` | 创建；JSON 为 `{"filter":{},"from_start":false}` |
| `GET /admin/notify/consumers?library=SERIAL&limit=100` | 分页列表；下一页使用返回的 `next` 作为 `after` |
| `GET /admin/notify/consumers/{id}?library=SERIAL` | 查询实例、日志尾、确认位置和待确认批次 |
| `GET /admin/notify/consumers/{id}/stream?library=SERIAL&generation=UUID` | SSE 长连接 |
| `GET /admin/notify/consumers/{id}/events?library=SERIAL&generation=UUID` | 查询/领取当前批次，供不支持 SSE 的客户端使用 |
| `POST /admin/notify/consumers/{id}/ack?library=SERIAL` | JSON 为 `generation`、`delivery_id`、`through` |
| `DELETE /admin/notify/consumers/{id}?library=SERIAL` | JSON 为 `generation` 和 `abandon_unacknowledged:true` |

过滤条件包含可选的 `pool`（规范 UUID 字符串）、`barcodes` 和 `event_types` 数组。池移动事件同时匹配原池和新池。`unacknowledged_positions` 是尚未确认的日志位置数，包含被过滤掉的事件，不应解释为过滤后的告警数量。

连接 `/stream` 时可指定 `event_types=tape.state.changed,task.status.changed`；省略时沿用持久订阅，`event_types=` 扩大到全部。新集合必须包含原集合，否则请求被拒绝。扩大范围时，在同一 FDB 事务中为每种新增类型记录当前日志尾部 `type_starts`。原有类型从最后 ACK 之后继续补发积压；新增类型只发送扩展生效后产生的事件，不发送此前历史。ACK 位置及已有待确认批次均保持不变，原批次 ACK 继续有效。之后再次重连沿用这些类型起点，不会把新增类型的起点反复移到最新位置。需要新增类型更早的历史时建立独立回放消费者。

```sh
ltfsctl -e HOST:7401 -l LIBRARY_SERIAL notify watch ops-platform --generation GENERATION_UUID \
  --event-type tape.state.changed --event-type task.status.changed
# 或扩大到全部类型
ltfsctl -e HOST:7401 -l LIBRARY_SERIAL notify watch ops-platform --generation GENERATION_UUID --all-event-types
```

SSE 事件名为 `notify`，`data` 是完整批次 JSON，`id` 为批次身份。服务端约每十秒发送注释心跳，等待 ACK 时不会反复刷同一批次。连接异常后重新连接并继续使用原 `generation`；不能仅凭浏览器已经收到的 Last-Event-ID 跳过尚未处理的批次。服务端错误可能以 `notify-error` 结束连接；重连时仍由 FDB ACK 位置决定重放起点。

Rust 客户端 `FdbClient` 提供对应 create/list/show/batch/ack/delete 方法和 `watch_notifications` 回调。回调返回 false 停止，回调错误直接返回；只有连接类故障才自动重连。回调应先完成可靠处理，再显式调用 `acknowledge_notify`。

## 连接、重试和消费者状态

tape-rs 不配置或主动探测外部系统地址。Rust 客户端和 CLI 在通知连接异常时轮换节点并自动重连；SSE 的 `retry: 1000` 提示接收端重连。服务端写入超时会结束旧连接，未确认批次仍保留；断开的 TCP 连接不能继续重试写入，恢复后通过新连接补发。

`notify show/list` 与对应 HTTP 查询显示：

| 字段 | 含义 |
|---|---|
| `connection_status` | `never_connected`、`connected`、`disconnected` |
| `delivery_status` | `caught_up`、`backlogged`、`waiting_ack` |
| `notification_status` | 无连接且存在未确认位置时为 `interrupted`，否则为 `normal` |
| `last_connected_at_millis` / `last_disconnected_at_millis` | 最近连接和已观察到的断开时间 |
| `last_sent_at_millis` / `last_ack_at_millis` | 最近一次完成本地发送和可靠确认时间 |
| `delivery_attempts` | 完成本地写入的批次投递次数，不等于外部成功消费次数 |
| `waiting_ack_millis` | 当前批次等待 ACK 的时间 |

连接会话每约 10 秒刷新一次 FDB 中的观测租期；节点突然故障时，约 45 秒无续期会显示断开。它依赖节点时钟同步，只用于运维观测，绝不用于删除事件、推进 ACK 或判定业务处理成功。新会话 ID 会阻止旧连接的心跳/关闭覆盖新连接状态。

这些字段表示通知连接或消费链路异常，不能仅凭断线断言外部系统宕机；一个系统也可能维护、重启或主动暂停。连接正常但长期不 ACK 时，显示待确认时间与积压，由运维结合业务处理时间判断。消费者状态变化本身不再生成通知，避免通知失败递归制造通知。

## 事件与保留

当前事件类型为 `tape.state.changed`、`tape.assignment.changed`、`tape.location.changed`、`tape.alert.raised`、`library.inventory.changed`、`library.monitor.changed` 和 `task.status.changed`。覆盖监控状态、管理策略、物理库存、分配关系以及管理异步任务的状态变化。逐文件导入进度不逐行发送，可查询原任务详情。

磁带事件与 `tape list/show` 共享状态优先级，永久介质故障与 TapeAlert 历史继续保留。告警附带原始编号及 scope，驱动器、环境和上一盘介质的告警不能直接解释为当前磁带损坏。监控时间戳、普通容量字节变化及 owner epoch 变化不会单独刷出重复状态事件。物理发现仍受采样周期和设备忙碌影响。

首版不自动删除事件，不设置未确认事件 TTL。确认只推进消费者进度，避免慢消费者因超时而静默丢失历史；运维应关注 FDB 空间和消费者积压。事件自动回收需要另行实现跨消费者安全水位，不能通过直接清 FDB 前缀处理。

删除订阅明确放弃它的未确认进度，CLI 强制要求：

```sh
ltfsctl -e 10.131.7.219:7401 -l LIBRARY_SERIAL notify delete ops-platform \
  --generation GENERATION_UUID --abandon-unacknowledged
```

同名重建产生新的 `generation`，旧连接和旧 ACK 被拒绝。通知事务、SSE 和外部消费均不新增 SCSI 命令，不依赖 IBM EE/LE。


## 测试事件注入

使用显式管理接口 `POST /admin/notify/events/inject?library=SERIAL`，请求为：

```json
{"operation":"固定的请求UUID","event_type":"tape.state.changed","data":{"barcode":"MOCKL8","current":{"state":"need_replace"}}}
```

CLI 对应：

```sh
ltfsctl -e HOST:7401 -l LIBRARY_SERIAL notify inject \
  --operation REQUEST_UUID --event-type tape.state.changed \
  --data '{"barcode":"MOCKL8","current":{"state":"need_replace"}}'
```

事件类型使用上文已支持的类型，`data` 必须是最大 8KiB 的 JSON 对象。相同带库、相同 operation 和相同内容的重试只返回原事件；复用 operation 更改类型或内容会拒绝。响应及 SSE 事件顶层固定包含 `injected:true`、`origin:"injected"` 和 `injection_id`，调用方不能覆盖这些来源字段。

注入只写通知日志与注入去重回执，不更新真实磁带监控、库存、策略或任务，不发送 SCSI 命令。测试事件同样持久化，遵守类型起点、资源过滤、SSE、重连和 ACK 规则；实际订阅者应识别注入标记。建议使用专用消费者和 `MOCKL8` 等测试条码过滤，避免把模拟内容当作真实硬件故障。

测试扩大订阅的流程：先订阅类型 A，分别注入旧 A 和旧 B；断线后扩大为 A+B，旧 A 应继续补发而旧 B 不发送；再注入新 B，应正常接收。全过程保留同一消费者身份，最后确认完整批次。
