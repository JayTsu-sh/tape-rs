# FDB 原生池管理

普通池管理与 `ltfsctl task` 使用 `/libraries/{真实changer序列号}/admin/...`，必须显式指定 `--library`。物理库启用逻辑带库后，`ltfsctl pool` 还须指定 `--logical-library UUID`，HTTP 池请求使用 `x-tapers-logical-library` 头。尚未启用逻辑带库时，下面的命令直接操作物理库下的池。池、任务和通知以 FDB 为事实源。

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

未提供磁带清单时，`create` 原子创建空池并返回池记录。`set` 和 `delete` 返回持久任务，需查询至 succeeded、failed 或 cancelled。删除要求目录为空、没有磁带归属或活动文件写入。取消仅适用于 queued。任务窗口轮换、历史清理和运行中设备任务取消尚未开放。

用 `ltfsctl -e NODE:7411 --library SERIAL pool create archive --preview --tape ABC001L8 --tape ABC002L8` 可以在创建前检查池名称和 1 到 100 个磁带条码。预检依赖当前 owner 的完整库存。结果中的 `eligible` 表示库存位置和归属符合条件，`blocked` 会列出相关条码及原因。预检只读 FDB，磁带格式仍须在后续设备检查中确认。

磁带清单非空时，创建先原子预约名称、所有条码及归位槽，再由带库 owner 处理介质。全部介质完成后，池及其归属同时可见。已有 LTFS 卷沿用原卷 UUID 和文件内容。使用 `--confirm-format` 明确授权初始化，且必须实读证明所有分区为空。目录按现有 LTFS 导入规则处理，已有介质的池标记必须与目标池一致或缺失。

```bash
ltfsctl -e NODE:7411 --library SERIAL pool create archive \
  --tape ABC001L8 --tape ABC002L8 --confirm-format --request create-with-tapes.json
ltfsctl -e NODE:7411 --library SERIAL pool create-status POOL_UUID --namespace NAMESPACE_UUID
```

创建或查询逻辑带库下的池时，两条命令都传入原 `--logical-library`。超时后用原请求文件和原参数继续，或按回执中的 UUID 与 namespace 查询。回执的 `active_tape` 包括原介质子任务及其检查点。位置或初始化结果待核对时，任务保留原条码、卷 UUID 和预约。

介质任务明确失败或取消且设备操作已结清时，建池返回 `failed` 或 `cancelled`。原回执仍可查询，已确认结清的预约会释放。已完成导入的卷保留归属，并在原回执中列出。建池失败时，部分准备的池不会发布给应用。

写命令在第一次提交前以独占创建、0600、文件和父目录 fsync 保存请求文件。重复执行同一命令和相同参数时使用同一文件，沿用原 namespace、池 UUID、修订号、任务 epoch/operation；不会重新解析池名或更新修订号。不同参数、损坏或不完整文件直接拒绝，不覆盖、不修复。文件只保存请求身份，不缓存任务状态。保留这些文件供超时、断网和接管后恢复；不要为了重试而删除文件或改用新文件。

策略字段是 `name`、`file_limit`、`media_restriction`、`mount_limit`、`low_space_warning_enabled`、`low_space_warning_threshold`、`no_space_warning_enabled`。开关使用 true/false；数值阈值为 0..32767；file_limit 必须大于零；介质限制沿用 L5/L6/L7/L8/L9/M8 和支持的 IBM 介质代码，none 表示不限制。表格的 UUID、修订号、文件上限列使用固定宽度；可变宽度中文名称放在末列，控制字符转义，避免换行破坏表格。

HTTP 路由以 `/libraries/SERIAL/admin` 为前缀：

| 方法和路径 | 含义 |
| --- | --- |
| GET `/context` | 当前 namespace、管理请求窗口 |
| GET `/pools?limit=100&after=CURSOR` | 当前库池分页 |
| GET `/pools/UUID_OR_NAME` | 池详情；UUID 不回退为名称 |
| POST `/pools` | 固定池 UUID。空清单同步创建，有选中磁带时返回持久任务 |
| POST `/pool-create/preview` | 只读检查名称和条码，正文包含 `version`、`name`、`file_limit`、`tapes` |
| GET `/pool-create/UUID` | 查询原带磁带的建池任务及进度 |
| PATCH `/pools/UUID` | 固定管理请求身份的策略任务 |
| DELETE `/pools/UUID` | 固定管理请求身份的删除任务 |
| GET `/tasks?limit=100&after=CURSOR` | 当前库持久任务分页 |
| GET `/tasks/EPOCH/OPERATION` | 查询原任务 |
| POST `/tasks/EPOCH/OPERATION/cancel` | 取消原 queued 任务 |

除 context 外必须提供且只提供一个 `x-tapers-catalog-namespace` 头。分页上限 100 是管理元数据规则；与 S3 ListObjectsV2 的上限 1000 无关。游标是原样传回的 Base64URL 字符串，不能跨库使用。

创建正文包含 version=1、id、name、file_limit、created_at_millis 和 policy，可以附加 `tapes` 字符串数组及 `confirm_format` 布尔值。提交有磁带的合法清单后，HTTP 202 表示已受理，状态为 `succeeded` 时池才可用。policy 包含全部五个策略字段（不包含 name/file_limit）。PATCH/DELETE 正文是统一 v1 管理信封：

```json
{"version":1,"library":"SERIAL","namespace":"NAMESPACE_UUID","epoch":1,"operation":"OPERATION_UUID","change":{"kind":"pool","id":"POOL_UUID","revision":1,"patch":{"media_restriction":"L8"}}}
```

删除 change 为 `{"kind":"pool-delete","id":"POOL_UUID","revision":2}`。原请求字段、UUID 和 epoch 不得在重试时改变。未知或重复字段、其他尚未接入执行器的任务类型会在入队前拒绝。完整格式参见 [持久任务账本](fdb-management-intents.md)。

元数据只读模式允许查询；修改要求目标库 owner Ready、已 fenced、冻结点匹配且 Serving 多数派许可有效，FDB 事务还校验当前 owner epoch。非 owner 入口最多转发一次。后台元数据执行不占 SCSI/Raft 线程，逐库隔离，限制队列和并发；依赖失败按库指数退避并加入抖动，最长约30秒，恢复后回到正常轮询。永久损坏返回500及原诊断，不伪装成临时503。FDB 故障、失权或回执未知时停止本次等待，保留原 UUID/任务，不回退 SQLite；按原请求查询或重试。已经提交的事务不能因 HTTP 超时而推断未执行。

本文件描述当前原生接口；历史物理验收报告中的旧数字任务 ID、旧池命令参数和端口不适用于该接口。设备管理命令与 S3/FUSE 写入仍需各自的原生接线和实际带库验收，不能由本片池元数据验证代替。


## 真实B库验收（2026-11-05）

实际部署源码为 `a13dffdd9abd23c8c6007611e1c2766c94d817c8`；后续文档修订不改变该版本的运行代码。三台物理节点连接独立的三节点FDB及B库主FC预留域，A库保持释放和隔离。基础CLI/HTTP CRUD、中文列表、原选择器与请求重放、同修订号竞争、退役UUID拒绝复活均通过。

计划停止实际owner后，原queued任务由更高owner epoch执行；全部16份原请求回放仍返回原终态，只产生一次池修订。真实后台成功后再由受限代理损坏202回执，CLI报告结果未知，未重复PATCH，原检查点逐字节不变。真实queued取消不改变池，三节点返回相同终态。

实际停止新FDB集群两个原进程后，原集群读取超时、后台管理请求返回依赖故障，CLI预算耗尽仍保留原身份。恢复原unit及数据后，原请求成功收敛且只增加一次修订；故障前已提交任务全文不变，新FDB恢复healthy/double/三进程，旧FDB在故障期间及恢复后的健康检查均通过。

软件检查包括387项全特性单元测试、完整默认测试、全目标全特性检查、严格Clippy及显式隔离FDB集成测试。实测不涉及格式化、MOVE或文件写带；计划停机、回执故障和FDB失去多数派的结果不代替断电、介质故障或S3/FUSE写入验收。详细阶段证据位于维护主机 `/work/tmp/tape-fdb-pool-api-physical-20261105/`。

## 多副本池

使用 `pool replicated`。创建请求逐个指定底层池的物理库、FDB 命名空间、池 UUID 与名称。已启用逻辑带库的成员填写 `logical_library`，其余填 null。参与的物理库须在同一 FDB 里，并配置在接收请求的管理服务上。一次可提交 2 至 16 个底层池，共最多 100 盘磁带。省略某个成员的 `tapes` 可创建空底层池。

第一次提交前将完整请求保存到文件。下面是同一逻辑带库内的两个底层池。请将 UUID、带库序列号、时间及条码替换为实际的请求标识和设备范围。

```json
{
  "version": 1,
  "id": "a88dc978-9f9e-47f9-80bc-33483264418c",
  "name": "archive",
  "created_at_millis": 1795680000000,
  "members": [
    {
      "library_serial": "SERIAL",
      "namespace": "71077f97-c7ce-411d-9c58-38cee2c8c30a",
      "logical_library": "aea064af-fefa-4207-81a1-718288755d62",
      "pool_id": "2d5910f6-8e68-475b-9040-a5313b0f1c89",
      "name": "archive-a",
      "tapes": ["ABC001L8"]
    },
    {
      "library_serial": "SERIAL",
      "namespace": "71077f97-c7ce-411d-9c58-38cee2c8c30a",
      "logical_library": "aea064af-fefa-4207-81a1-718288755d62",
      "pool_id": "29382d7b-c6bc-41e4-8a3a-758eb0fcae7d",
      "name": "archive-b",
      "tapes": ["ABC002L8"],
      "confirm_format": true
    }
  ]
}
```

```bash
ltfsctl -e NODE:7411 pool replicated create --request replica.json
ltfsctl -e NODE:7411 pool replicated show POOL_UUID
ltfsctl -e NODE:7411 pool replicated files POOL_UUID --limit 100
ltfsctl -e NODE:7411 pool replicated resolve POOL_UUID --path /报告.pdf --revision REVISION --volume VOLUME_UUID
```

创建受理后保留同一请求文件和 UUID。底层池及磁带在全部准备完成后共同发布，管理详情按物理库列出故障范围。成员有库存或 owner 待核对时显示 `waiting_library`，已发出的初始化待确认时显示 `result_unknown`。成员的旧请求范围保存在 `original_logical_library`，查询中的 `logical_library` 反映底层池当前的带库范围。

任一成员明确失败或取消时停止发起后续介质任务，各成员结清原任务后释放已确认安全的预约。父池详情保留原成员回执及已完成卷的身份。

已有 LTFS 文件逐个读回验证，卷和文件内容保留。文件清单分页给出正文 SHA-256 和按底层池计数的 `replica_count`，创建期间不会补写历史副本。指定 `confirm_format: true` 的成员只可初始化经实读核实的空白磁带。已有 LTFS 卷上的池标记须缺失或与对应底层池相符。

同路径内容冲突时，详情的 `conflict` 给出路径、修订号和两盘来源。管理员选定保留卷后提交该路径和原修订号，原任务继续。另一盘的原文件仍保存在原卷。目录与文件占用同一路径时，保留目录及其中的文件。底层池的独立文件写入和删除受保护。应用通过多副本池的文件接口访问已发布版本。

HTTP 使用 `POST /admin/replicated-pools` 提交请求，`GET /admin/replicated-pools/{id}` 查询原任务。已发布文件使用 `GET /admin/replicated-pools/{id}/files?limit=100`，下一页的 `after` 是上一页 `next` 路径的无填充 Base64URL。冲突通过 `POST /admin/replicated-pools/{id}/resolve` 提交 `version`、`path`、`revision` 和 `volume_uuid`。

### 多副本池的原生文件接口

每次归档先保存一份原请求 JSON。`operation` 是新操作的 UUID。新文件的 `previous` 为 null，覆盖时填写当前已发布版本。`size` 是正文的字节数，`sha256` 是 64 位十六进制摘要。路径使用池内绝对路径，根目录和已有的导入目录可承载新文件。

```json
{
  "operation": "eb81c5b8-2417-48d8-bb03-3cc0534648e2",
  "path": "/report.txt",
  "previous": null,
  "size": 5,
  "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
}
```

```sh
ltfsctl -e NODE:7411 pool replicated archive POOL_UUID --request archive.json --source report.txt
ltfsctl -e NODE:7411 pool replicated archive-status POOL_UUID OPERATION_UUID
ltfsctl -e NODE:7411 pool replicated stat POOL_UUID /report.txt
ltfsctl -e NODE:7411 pool replicated get POOL_UUID /report.txt saved.txt
ltfsctl -e NODE:7411 pool replicated get POOL_UUID /report.txt from-member.txt --member MEMBER_UUID
ltfsctl -e NODE:7411 pool replicated archive-cancel POOL_UUID OPERATION_UUID
```

`committed` 确认每个成员已持久提交完整版本。覆盖期间旧版本持续可读。如某个成员或其正文暂存不可用，请保留请求 JSON 和原正文，用同一请求恢复。任一成员已有可读提交时，可省略 `--source`，从该成员读回并校验正文后继续其余成员。取消会等待结果未定的设备写入结清，保留已确认的成员回执。已发布版本使用相同版本的健康副本读回。下载校验正文长度和 SHA-256 后才发布本地文件。原归档文件的底层路径按操作隔离，覆盖前和已取消的条目继续占用底层卷空间。

HTTP 使用 `POST /v1/fdb/replicated-pools/{id}/archives` 受理 JSON。每个底层池通过 `PUT /v1/fdb/replicated-pools/{id}/archives/{operation}/members/{member}` 上传完整正文和明确的 `Content-Length`。查询 `GET /v1/fdb/replicated-pools/{id}/archives/{operation}` 可恢复原成员和父提交回执。对同一路径发 `DELETE` 请求取消。已确认成员的恢复正文用 `GET` 请求相应的成员上传地址。

读文件使用 `GET /v1/fdb/replicated-pools/{id}/file?path=BASE64URL`。可传 `version` 固定发布版本，传 `member` 固定验收成员。`GET /v1/fdb/replicated-pools/{id}/stat?path=BASE64URL` 返回 `file_version`、长度、摘要和副本数。`path` 使用 UTF-8 字节的无填充 Base64URL。Rust SDK 在 `FdbClient` 暴露同样的归档、查询、取消和下载操作。

已导入的符号链接在 `stat` 的 `symlink` 字段返回校验后的原目标，FUSE 可读取该链接。

### S3 和 FUSE

启用 `ltfsd --fdb-s3 --s3-listen 0.0.0.0:PORT --s3-port PORT` 后，`ListBuckets` 会列出已发布的多副本池。bucket 名为 `replica-POOL_UUID`，使用父池 UUID。S3 支持列举、HEAD、GET 和单次 PUT。可在池根目录或已导入的目录内创建文件。

```sh
curl --fail-with-body --upload-file report.txt \
  -H 'X-Tapers-Operation: OPERATION_UUID' -H 'If-None-Match: *' \
  http://NODE:7402/replica-POOL_UUID/report.txt
curl --fail-with-body http://NODE:7402/replica-POOL_UUID/report.txt -o saved.txt
tape-fuse -e NODE:7411 --replicated-pool POOL_UUID --cache-dir CACHE MOUNTPOINT
```

S3 覆盖可用 `If-Match` 固定原对象的 ETag。PUT 前生成并保存 `X-Tapers-Operation` UUID，保留原路径、正文、条件头及对象元数据。收到 503、504 或连接中断时，可用原 UUID 查询 `ltfsctl pool replicated archive-status`，或提交完全相同的 PUT 继续。源正文丢失时，可通过原生归档接口从已验证的成员恢复。

FUSE 默认只读。添加 `--fdb-journal-dir JOURNAL` 启用常规文件创建和覆盖。日志目录由单次挂载独占，权限须为 0700。文件关闭或 `fsync` 成功表示所有底层池均已确认。挂载中断后用同一日志目录恢复原 UUID 和正文。归档等待期间，其他客户端仍可从可用副本读取上一个已发布版本。多副本入口当前支持归档和读取，删除、重命名及目录修改返回不支持。

已确认取消的 FUSE 原归档会在日志目录中记录 `cancelled`，保留原请求文件与正文。后续重新挂载时跳过这条原归档。
