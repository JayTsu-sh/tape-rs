# 原生 FDB 带库与磁带查询

`library list/show` 和 `tape list/show` 直接使用原生 FDB 管理查询。读取不调用 EE、不发设备命令、不刷新库存、不修改归属，也不回退 SQLite。设备库存由当前 owner 的周期采集提供；显式 rescan、导入及管理变更属于其他操作。

## 命令

```sh
ltfsctl -e 10.131.7.207:7411 library list
ltfsctl -e 10.131.7.207:7411 library list --json
ltfsctl -e 10.131.7.207:7411 library show 55L3A78011W9LL01
ltfsctl -e 10.131.7.207:7411 --library 55L3A78011W9LL01 library show
ltfsctl -e 10.131.7.207:7411 --library 55L3A78011W9LL01 tape list
ltfsctl -e 10.131.7.207:7411 --library 55L3A78011W9LL01 tape list --json
ltfsctl -e 10.131.7.207:7411 --library 55L3A78011W9LL01 --pool lto8 tape list
ltfsctl -e 10.131.7.207:7411 --library 55L3A78011W9LL01 tape show 8A0042L8
```

`library list` 列出本服务配置的活动带库，可用全局 `--library` 筛选。`library show` 必须给出序列号或全局 `--library`；同时给出时必须相同。带库查询不接受 `--pool`。磁带查询必须显式指定 `--library`，可按该库中的池名称或 UUID 过滤。

list 默认输出表格，`--json` 返回完整机器可读清单；show 输出 JSON。表头与 ASCII 标识、数字列按本次完整结果计算宽度，不截断长条码、UUID 或数值。池名位于最后一列；控制字符转义，中文名称不会推偏后面的列。未知值显示 `--`，JSON 保留 null。

## HTTP

| 操作 | GET 路径 | 查询参数 |
| --- | --- | --- |
| library list | `/admin/libraries` | 可选 `library=SERIAL` |
| library show | `/libraries/SERIAL/admin/library` | 无 |
| tape list | `/libraries/SERIAL/admin/tapes` | 可选 `pool`、`limit`、`after` |
| tape show | `/libraries/SERIAL/admin/tapes/BARCODE` | 可选 `pool` |

除 library list 外，每次请求必须带唯一的 `x-tapers-catalog-namespace: UUID`，可先读取同库的 `/libraries/SERIAL/admin/context` 获取。缺失、重复或不同的 namespace 返回 409；客户端不会通过重新选 namespace 掩盖改变的作用域。池过滤接受库内名称或规范 UUID，UUID 不会隐式回退为另一个池的名称；未知库、池或磁带返回 404。

Tape 管理分页默认及最大 100 条，按条码字节字典序排列；这是管理分页，与 S3 ListObjectsV2 的 1000 条上限无关。响应 `next` 可作为 `after`，游标绑定库、namespace 和池，不得跨范围使用。每页读取当前事务，不保留跨页历史快照；CLI 连续读页并拒绝重复游标或无序、重复的条码。

## 事实的含义

带库视图包含 namespace、owner_epoch、nodes、active_node、库存计数、原采样节点/轮次/时间、巡检记录及库存年龄。nodes 是本入口配置的控制节点，`reported_eligible`/`reported_drives` 来自已应用的候选信息，缺失保持 null，不代表实时探测结果。不暴露 group/nodegroup。

active_node 仅在已应用控制状态就绪、冻结点及 FDB epoch 一致时给出；本入口没有 owner 上下文或无法确认时为 null。它不是设备租约或实时 I/O 可用性的证明，`execution_readiness=unknown` 明确表示该查询未探测数据服务就绪。实际数据操作仍执行原有许可、PR 和 FDB 栅栏核验。

`library list` 的 inventory 是采样节点/轮次/时间摘要；`library show` 包含完整 volumes/drives。两者都有库存计数和年龄，库存缺失时计数与年龄为 null，不能将未知显示为空库。库存与巡检由不同事务依次发布，合法的中间记录对可以有不同采样时间。

Tape 清单合并已观测的物理条码与 FDB 分配记录，包含未分配实带（包括清洗带）及已分配但当前库存未观测到的盘。池过滤仅返回该库该池的管理归属。`presence` 区分 `unknown`、`observed`、`not_observed`；后者仅表示那次库存中未发现，不自动推断磁带已导出或丢失。

`assignment` 保留池、volume、online、blacklisted 及修订号，`location` 与 `observation` 保留物理观测及来源。online 是管理策略，不等于介质健康或当前可读；没有单盘采集事实时 health/capacity 为 null，不推断健康、零容量或可写。metadata volume 绑定也不单独证明文件索引已导入或 S3 内容可读。

查询验证活动路由、缓存/通知证明、所读取的归属索引、池和卷反向绑定。损坏返回 500 且不可重试，临时 FDB 不可用返回 503；不自动补字段、修复记录或从本地状态伪造成功。查询并发沿用有界管理预算。实际设备操作与异步任务仍须单独验收。
