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

list 默认输出表格，`--json` 返回完整机器可读清单；show 输出 JSON。表头与 ASCII 标识、数字列按本次完整结果计算宽度，不截断长条码、UUID 或数值。池名位于最后一列；控制字符转义，中文名称不会推偏后面的列。未知值显示 `--`，JSON 保留 null。尚无库存观测时，tape list 的默认表格会明确提示“库存尚无观测”，仍显示已有 FDB 归属行；不会与已知空库混淆。

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

## 真实 B 库验收（2026-11-06）

源码 `954098a36306ff8cab46ac1f28ae0b425f35f3b0` 已部署到 207–209 的原隔离服务，沿用 45152 FDB 集群与 namespace。非 owner 的 207 实际完成四类 CLI 查询；三节点 HTTP 也分别完成 list/show、limit=3 的条码有序分页、namespace 缺失/错误/重复拒绝、跨池游标拒绝及未知条码/库查询。CLI 的默认表格列对齐和 JSON 均通过。

实际库存为 8 盒磁带（含清洗带）、2 个驱动器，nodes 为 1/2/3，active_node 与当时 FDB/control owner 一致。8 盘均未分配，逐盘 show 的 health/capacity 保持 null，online/blacklisted 未推定为默认值；lto8 池过滤返回空清单。非 owner 查询前后均未持有原始设备句柄，原 pool revision 6、通知 ACK 57 和三节点 PID 保持不变。已分配/离线但未观测到的盘及损坏引用，由明确合成记录的隔离 FDB 测试覆盖，未冒称已在本次 0-assignment 实物环境复现。

实际停止新 FDB 的两个原 server，使读取出现 1031 超时。四个 HTTP 查询分别返回可重试依赖错误，CLI library list 重试预算耗尽后非零退出且没有成功正文，没有返回伪造空库。故障窗口内三设备 PR generation、注册键及 holder 保持不变。恢复原 unit/data/cluster 后，四种 HTTP 查询及完整新采样恢复；原池、事件 56/57 字节和消费者 ACK 57 均保留。新旧 FDB 分别通过故障期间或恢复后的离散健康核对，不代表连续监测。

最终严格 SDK 及独立 FDB/control/三设备 PR 审计一致：owner node 3 / epoch 179631，key `0x4c0300000002bdaf`，三节点就绪。A 库和备用 changer 域继续隔离；本次未分配、装卸、MOVE、格式化或读写介质文件。S3/FUSE 写入、rescan/import actor 和单盘健康采集仍待后续完成。

证据在 `/work/tmp/tape-native-library-tape-queries-physical-20261106/` 的 `BASIC-COMPLETE.json`、`SDK-basic.json`、`QUORUM-COMPLETE.json`、`SDK-recovery.json`、`OWNER-AUDIT-final.json` 及各 CLI/HTTP 原始记录。软件和实物故障结果分别保留；生产者的周期采样可以更新观测时间，不将正常时间变化误判为查询修改事实。
