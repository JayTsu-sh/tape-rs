# FDB 原生池管理

`ltfsctl pool` 和 `ltfsctl task` 使用 `/libraries/{真实changer序列号}/admin/...`，必须显式指定 `--library`，不接受全局 `--pool`。池、任务和通知以 FDB 为事实源。文件内容仍属于阶段性的 owner 只读服务；池创建不分配、不格式化磁带。

```bash
ltfsctl -e NODE:7411 --library SERIAL pool create archive --request create.json
ltfsctl -e NODE:7411 --library SERIAL pool list
ltfsctl -e NODE:7411 --library SERIAL pool list --json
ltfsctl -e NODE:7411 --library SERIAL pool show archive
ltfsctl -e NODE:7411 --library SERIAL pool set archive media_restriction L8 --request set.json
ltfsctl -e NODE:7411 --library SERIAL task show --request set.json
ltfsctl -e NODE:7411 --library SERIAL task list
ltfsctl -e NODE:7411 --library SERIAL task cancel --request set.json
ltfsctl -e NODE:7411 --library SERIAL pool delete archive --request delete.json
ltfsctl -e NODE:7411 --library SERIAL task show --request delete.json
```

`create` 是原子创建，返回池记录。`set`、`delete` 返回持久任务；202 或 queued 不代表成功，须查询至 succeeded、failed 或 cancelled。删除要求目录为空、没有磁带归属或活动文件写入。取消仅适用于 queued。任务窗口轮换、历史清理和运行中设备任务取消尚未开放。

写命令在第一次提交前以独占创建、0600、文件和父目录 fsync 保存请求文件。重复执行同一命令和相同参数时使用同一文件，沿用原 namespace、池 UUID、修订号、任务 epoch/operation；不会重新解析池名或更新修订号。不同参数、损坏或不完整文件直接拒绝，不覆盖、不修复。文件只保存请求身份，不缓存任务状态。保留这些文件供超时、断网和接管后恢复；不要为了重试而删除文件或改用新文件。

策略字段是 `name`、`file_limit`、`media_restriction`、`mount_limit`、`low_space_warning_enabled`、`low_space_warning_threshold`、`no_space_warning_enabled`。开关使用 true/false；数值阈值为 0..32767；file_limit 必须大于零；介质限制沿用 L5/L6/L7/L8/L9/M8 和支持的 IBM 介质代码，none 表示不限制。表格的 UUID、修订号、文件上限列使用固定宽度；可变宽度中文名称放在末列，控制字符转义，避免换行破坏表格。

HTTP 路由以 `/libraries/SERIAL/admin` 为前缀：

| 方法和路径 | 含义 |
| --- | --- |
| GET `/context` | 当前 namespace、管理请求窗口 |
| GET `/pools?limit=100&after=CURSOR` | 当前库池分页 |
| GET `/pools/UUID_OR_NAME` | 池详情；UUID 不回退为名称 |
| POST `/pools` | 固定池 UUID 的原子创建 |
| PATCH `/pools/UUID` | 固定管理请求身份的策略任务 |
| DELETE `/pools/UUID` | 固定管理请求身份的删除任务 |
| GET `/tasks?limit=100&after=CURSOR` | 当前库持久任务分页 |
| GET `/tasks/EPOCH/OPERATION` | 查询原任务 |
| POST `/tasks/EPOCH/OPERATION/cancel` | 取消原 queued 任务 |

除 context 外必须提供且只提供一个 `x-tapers-catalog-namespace` 头。分页上限 100 是管理元数据规则；与 S3 ListObjectsV2 的上限 1000 无关。游标是原样传回的 Base64URL 字符串，不能跨库使用。

创建正文严格包含 version=1、id、name、file_limit、created_at_millis 和 policy。policy 包含全部五个策略字段（不包含 name/file_limit）。PATCH/DELETE 正文是统一 v1 管理信封：

```json
{"version":1,"library":"SERIAL","namespace":"NAMESPACE_UUID","epoch":1,"operation":"OPERATION_UUID","change":{"kind":"pool","id":"POOL_UUID","revision":1,"patch":{"media_restriction":"L8"}}}
```

删除 change 为 `{"kind":"pool-delete","id":"POOL_UUID","revision":2}`。原请求字段、UUID 和 epoch 不得在重试时改变。未知或重复字段、其他尚未接入执行器的任务类型会在入队前拒绝。完整格式参见 [持久任务账本](fdb-management-intents.md)。

元数据只读模式允许查询；修改要求目标库 owner Ready、已 fenced、冻结点匹配且 Serving 多数派许可有效，FDB 事务还校验当前 owner epoch。非 owner 入口最多转发一次。后台元数据执行不占 SCSI/Raft 线程，逐库隔离，限制队列和并发。FDB 故障、失权或回执未知时停止本次等待，保留原 UUID/任务，不回退 SQLite；按原请求查询或重试。已经提交的事务不能因 HTTP 超时而推断未执行。

本文件描述当前原生接口；历史物理验收报告中的旧数字任务 ID、旧池命令参数和端口不适用于该接口。设备管理命令与 S3/FUSE 写入仍需各自的原生接线和实际带库验收，不能由本片池元数据验证代替。
