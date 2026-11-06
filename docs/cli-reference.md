# tape-rs 命令行参考手册

适用版本：首版开发版本，2026-11-04。以当前源码及全特性构建产物为依据。主要面向使用 FDB 集群管理多个真实带库的运维人员。

模型为 **Library → Pool → Tape**：一个池只属于一个带库，数据带最多属于一个池；未分池带和清洗带属于带库库存。每个带库显示参与节点 `nodes` 和唯一就绪执行节点 `active_node`，多个库的执行节点尽量分散。完整语义见[管理模型](management-model.md)。

## 目录

- [工具与运行环境](#工具与运行环境)
- [连接、作用域与输出](#连接作用域与输出)
- [命令速查](#命令速查)
- [带库管理](#带库管理)
- [池管理](#池管理)
- [磁带管理](#磁带管理)
- [外部系统通知](#外部系统通知)
- [异步任务与原请求恢复](#异步任务与原请求恢复)
- [FDB 策略与节点查询](#fdb-策略与节点查询)
- [FDB 文件与对象操作](#fdb-文件与对象操作)
- [FUSE 与服务进程](#fuse-与服务进程)
- [常用操作流程](#常用操作流程)
- [错误处理与退出状态](#错误处理与退出状态)
- [完整参数附录与更新方式](#完整参数附录与更新方式)

## 工具与运行环境

| 工具 | 用途 | 完整参数 |
|---|---|---|
| `ltfsctl` | 通过 HTTP 访问集群，管理库、池、磁带、任务和文件 | [ltfsctl](reference/ltfsctl.md) |
| `ltfsd` | 运行集群服务，承担 Raft、设备隔离和后台执行 | [ltfsd](reference/ltfsd.md) |
| `tape-fuse` | 把指定池挂载为文件系统 | [tape-fuse](reference/tape-fuse.md) |
| `tape-library` | 独占设备的单机逻辑库工具，维护独立 SQLite 状态 | [tape-library](reference/tape-library.md) |
| `tape-rs` | 直接执行 SCSI、机械操作、LTFS 和 PR 诊断 | [tape-rs](reference/tape-rs.md) |

日常集群运维使用 `ltfsctl`，客户端不直接打开磁带设备。`tape-library`、直接设备模式的 `tape-fuse` 和 `tape-rs` 需要独占明确的设备，不能与正在管理同一设备的 `ltfsd` 或 IBM EE/LE 同时操作。同名命令的语义随工具而异：例如 `tape-rs import` 是机械入库，`ltfsctl tape import` 是导入文件索引。

客户端可单独构建：

```sh
cargo build --release --bin ltfsctl
```

普通 `ltfsctl` 构建通过 HTTP 使用 FDB 功能，不需要直接链接 FDB 客户端库。`fdb init` 要求启用 `fdb` 构建特性；全特性构建还包含 FUSE。启用 `fdb` 后，运行环境须能加载 `libfdb_c.so`。S3 服务统一通过 FDB owner 提供，启动要求 `--fdb-s3`，不再回退到 SQLite S3 后端。

本文示例中的 `LIBRARY_SERIAL`、`POOL_UUID`、`NAMESPACE_UUID` 和条码需替换为实际查询结果；`/var/lib/tape-client` 为客户端保存原请求的位置，不是服务端目录。示例中的格式化命令会改变介质内容，应单独选择需要初始化的介质执行。

## 连接、作用域与输出

### 连接集群

```sh
ltfsctl -e 10.131.7.207:7401 -e 10.131.7.208:7401 -e 10.131.7.209:7401 library list
ltfsctl -e 10.131.7.219:7401 library list --jsonpp
```

`-e` 使用管理 HTTP 端口，不是 Raft 7400 或 S3 7402。可重复给出节点地址；当前实验室的统一管理 VIP 为 `10.131.7.219:7401`。地址来自现场配置，不是程序内置默认值。

为缩短后续示例，可在 Bash 中定义：

```sh
ctl() {
  ltfsctl -e 10.131.7.207:7401 -e 10.131.7.208:7401 -e 10.131.7.209:7401 "$@"
}
install -d -m 700 /var/lib/tape-client
```

后文 `ctl` 是这个 shell 函数，不是额外安装的可执行文件。

### 公共选项

| 选项 | 含义 |
|---|---|
| `-e, --endpoint HOST:PORT` | 节点 HTTP 地址；至少一个，在顶层命令前指定 |
| `-l, --library SERIAL` | 目标带库真实 changer 序列号；可放在子命令后 |
| `-p, --pool SELECTOR` | 池名称或 UUID，按具体命令解释；FDB 文件操作要求 UUID |
| `-h, --help` | 当前命令的帮助；不执行业务操作 |

多库下执行池和介质管理时明确指定 `-l`。`library rescan` 省略 `-l` 会为全部已配置库各建一个任务；它不接受 `-p`。`pool create` 使用位置参数指定池名、可用 `--id` 固定 UUID，不接受 `-p`；`pool list` 也不接受 `-p`。

### 输出选项

| 命令 | 输出方式 |
|---|---|
| `library list`、`pool list`、`tape list` | 默认表格；`--no-header` 去表头；`--json` 紧凑 JSON；`--jsonpp` 缩进 JSON |
| `library show`、`tape show` | 默认属性列表；`-a FIELD` 只选顶层属性；支持 `--json`、`--jsonpp` |
| `pool show` | 直接输出缩进 JSON，不接受 `--json`、`-a` |
| 设备任务受理、策略变更、FDB 查询 | 直接输出对应 JSON 或命令定义的结果，不统一接受 `--json` |

`--json` 与 `--jsonpp` 互斥。`tape list --unit decimal|binary` 只影响表格容量显示，不能与 JSON 输出组合。未知或没有可信观测的字段可为 `null`，表格显示 `-`；不能把未知容量当作零或按型号估算。

表格按本次完整结果和表头的终端显示宽度动态计算列宽，列间至少两个空格，不使用制表符。文本左对齐，容量、数量和百分比右对齐；中文和组合字符按显示宽度计算。名称、条码和任务 ID 完整保留，不按固定宽度截断；结果较宽时可使用宽终端。脚本读取使用 JSON 输出，避免依赖随内容变化的列位置。

## 命令速查

以下均省略前面的 `ctl`，参数方括号表示可选，`BARCODE...` 表示可指定多盘。

| 命令 | 作用 |
|---|---|
| `library list [--jsonpp]` | 查看全部带库 |
| `library show -l SERIAL [--jsonpp]` | 查看库身份、nodes、active_node 和库存观测 |
| `library rescan -l SERIAL [--request FILE]` | 异步刷新指定库库存 |
| `pool create NAME -l SERIAL [--request FILE]` | 创建库内逻辑池 |
| `pool delete SELECTOR -l SERIAL [--request FILE]` | 删除无引用空池 |
| `pool list -l SERIAL [--jsonpp]` | 列出库内全部池，自动续页 |
| `pool show SELECTOR -l SERIAL` | 查看池详情 |
| `tape list -l SERIAL [-p POOL] [--jsonpp]` | 查询整库库存或池成员 |
| `tape import BARCODE... -l SERIAL -p POOL [--request PATH]` | 异步导入已有文件索引 |
| `tape move BARCODE... -l SERIAL [-p POOL] -L homeslot\|ieslot` | 异步改变真实位置 |
| `tape assign BARCODE... -l SERIAL -p POOL --yes` | 异步检查/初始化介质并入池 |
| `tape unassign BARCODE... -l SERIAL -p POOL` | 异步证明无引用后解除归属 |
| `tape validate BARCODE... -l SERIAL -p POOL` | 异步核验标签和索引结构 |
| `tape offline BARCODE... -l SERIAL -p POOL [-o MESSAGE]` | 持久设置离线及说明 |
| `tape online BARCODE... -l SERIAL -p POOL` | 实带标签/索引核验后上线 |
| `tape set BARCODE... -l SERIAL -p POOL --patch JSON` | 黑名单和离线属性 |

辅助入口包括 `tape show`、`pool set`、`task list/show/cancel`、`fdb control`、`fdb drive/tape`、`node`、`cluster` 及 FDB 文件命令。完整命令树见附录。

## 带库管理

### library list

```sh
ctl library list
ctl library list --jsonpp
ctl library list --no-header
```

列出集群已配置带库。读取已有观测，不逐卷读取磁带 index。新接入介质或需要明确刷新位置时运行 `library rescan`。

### library show

```sh
ctl library show -l LIBRARY_SERIAL --jsonpp
ctl library show LIBRARY_SERIAL -a active_node --json
```

两种选库形式择一使用。关键字段：`serial`、`model`、`num_of_drives`、`num_of_slots`、`num_of_ieslots`、`nodes`、`active_node`、`inventory_observation`。

`active_node` 表示已经完成接管并就绪的设备执行节点；没有就绪节点时为 `null`。`host_device_name` 属于 `host_device_node` 指定的主机，不能把它当作其他主机的 `/dev/sgN`。库存时间和 owner 轮次用于判断观测是否属于当前执行者。

### library rescan

```sh
ctl library rescan -l LIBRARY_SERIAL --request /var/lib/tape-client/rescan.json
ctl library rescan --request /var/lib/tape-client/all-library-rescans
```

第一条为指定库建任务；第二条为全部库各建任务，请求路径为目录。rescan 重新读取真实设备库存并发布观测，不执行格式化，不等同于 SCSI `INITIALIZE ELEMENT STATUS`，也不导入文件命名空间。

返回任务号后按[异步任务](#异步任务与原请求恢复)查询。读取失败保留诊断，不把失败解释为空库。

## 池管理

### pool create

```sh
ctl pool create archive -l LIBRARY_SERIAL --request /var/lib/tape-client/create-pool.json
ctl pool create archive -l LIBRARY_SERIAL --id POOL_UUID --file-limit 200000 \
  --request /var/lib/tape-client/create-pool-fixed.json
```

`NAME` 是库内唯一名称。省略 `--id` 自动生成 UUID；`--file-limit` 默认 200000。创建前固定并持久保存 UUID、namespace 和参数，成功创建逻辑池不会自动导入或格式化任何磁带。

### pool list / show

```sh
ctl pool list -l LIBRARY_SERIAL --jsonpp
ctl pool show archive -l LIBRARY_SERIAL
ctl pool show POOL_UUID -l LIBRARY_SERIAL
```

list 自动读取全部分页。show 的名称或 UUID 只在指定库内解析，并输出池、库及 namespace 信息。

### pool delete

```sh
ctl pool delete empty-pool -l LIBRARY_SERIAL --request /var/lib/tape-client/delete-pool.json
```

要求池内无文件目录依赖、无磁带归属、无接入屏障和在途写入。失败不会自动解绑磁带或删除文件；成功只删除逻辑池，不擦带、不卸带。请求固定原池 UUID 和 revision，过期修订不能覆盖后续修改。

### pool set

```sh
ctl pool set archive -l LIBRARY_SERIAL --patch '{"mount_limit":1,"file_limit":200000}' \
  --request /var/lib/tape-client/pool-policy.json
ctl pool set POOL_UUID -l LIBRARY_SERIAL --patch '{"name":"archive-renamed"}' \
  --request /var/lib/tape-client/pool-name.json
```

使用当前池 revision 准备规范 JSON patch。池 UUID 不随改名改变；不支持节点组属性。创建、删除和修改响应丢失时，使用原文件恢复，示例见任务章节。

## 磁带管理

### tape list / show

```sh
ctl tape list -l LIBRARY_SERIAL --jsonpp
ctl tape list -l LIBRARY_SERIAL -p archive --no-header
ctl tape list -l LIBRARY_SERIAL --unit binary
ctl tape show TAPE001L8 -l LIBRARY_SERIAL --jsonpp
ctl tape show TAPE001L8 -l LIBRARY_SERIAL -p archive -a poolname
```

不指定池显示库库存，包括 unassigned 和清洗带；指定池仅显示该池成员。列表自动续页。查询不为每盘装带读取 index；条码是库内介质选择条件，LTFS volume UUID 是介质文件系统身份，两者不能互换。

### tape import

```sh
ctl tape import TAPE001L8 -l LIBRARY_SERIAL -p archive \
  --request /var/lib/tape-client/import-one.json
ctl tape import TAPE001L8 TAPE002L8 -l LIBRARY_SERIAL -p archive \
  --request /var/lib/tape-client/import-batch
ctl tape import TAPE003L8 -l LIBRARY_SERIAL -p archive -P /incoming \
  --request /var/lib/tape-client/import-prefix.json
ctl tape import TAPE004L8 TAPE005L8 -l LIBRARY_SERIAL -p archive -P /incoming -R \
  --request /var/lib/tape-client/import-merged
```

| 参数 | 行为 |
|---|---|
| `BARCODE...` | 新任务的库内数据带条码，不得重复；清洗带拒绝 |
| `-p POOL` | 指定目标池名称或 UUID，必须属于所选库 |
| `-P, --path PATH` | 池内导入前缀，默认 `/IMPORT` |
| `-R, --merge-tapes` | 直接合并到前缀，不追加每盘条码目录 |
| `--request PATH` | 单盘原请求文件，或多盘原请求目录；目标文件不能已存在 |

首次导入的默认文件路径为 `/IMPORT/BARCODE/<源路径>`；`-P /incoming` 为 `/incoming/BARCODE/<源路径>`；再加 `-R` 则为 `/incoming/<源路径>`。同名文件保留原文件并给新文件追加 `_i` 后缀，文件/目录类型冲突失败。EE 内部数据对象和 shadow link 归并为用户文件，并保留实际物理来源。

这是已有 LTFS 文件索引导入：有界流式捕获、分批物化、核验完成后发布到 FDB，文件正文仍留在带上。命令始终异步，没有 `--async` 或同步等待参数。受理成功不代表目录已发布；以任务 `succeeded` 为准。

条码已经分配到所选池时，同一命令刷新该卷的 FDB 索引。保留原 Library、Pool、卷 UUID 和路径映射；不会再次追加条码目录或把已有文件改名为 `_1`。`-P`、`-R` 仅决定首次导入的位置，刷新沿用原映射。新请求重新读带，旧 `--request` 只恢复原任务或返回原终态。

刷新有界处理新索引与既有引用：更新该卷的当前文件、导入新增文件、撤销源中已消失的当前文件；保留已有共享目录。另一盘带上的较新逻辑版本以及已删除 inode 的历史副本不能因重扫旧带复活。失败或取消在池屏障内恢复原字节；全部核验后才发布新扫描基线，成功结果含 `refreshed=true`。换卷 UUID、索引代数回退或无法证明的路径冲突拒绝，不能用重新格式化来解决。

外部 EE/LE 改写磁带后，先正常生成 Full 并卸载，停止外部工具且确认设备独占释放，再启动项目及执行刷新。干净 Full 的磁带可以留在驱动器：可写 owner 保留旧 FDB、暂停该带普通读写并保持管理接口可用，显式异步 tape import 成功后才解除。原刷新已 running 时按原请求和快照接续；本项目在途写入或异常尾部仍走严格恢复，不自动采信带外索引。该启动边界及队列屏障已在[真实 TS4300](../.scratch/ltfs-ha/reports/loaded-media-refresh-physical-20261104.md)验证，维护设备操作仍须遵守设备独占。

### tape assign

```sh
ctl tape assign EMPTY001L8 -l LIBRARY_SERIAL -p archive --yes \
  --request /var/lib/tape-client/assign-empty.json
```

此操作具有初始化/格式化能力，`--yes` 必填。有文件的介质默认拒绝；明确要覆盖旧文件时才同时给出 `-f, --force-format`：

```sh
ctl tape assign ERASE001L8 -l LIBRARY_SERIAL -p archive --yes --force-format \
  --request /var/lib/tape-client/assign-reformat.json
```

目标须未分池且满足位置、驱动兼容与设备所有权条件。未知格式或读取错误不能当作空带。原请求固定新卷 UUID；格式化开始后的失败可能已经改变介质，必须保留任务诊断并接续原请求。需要保留已有文件时使用 `tape import`。

### tape unassign

```sh
ctl tape unassign EMPTY001L8 -l LIBRARY_SERIAL -p archive \
  --request /var/lib/tape-client/unassign.json
```

后台在 FDB 证明不存在有效文件副本、历史有效引用和在途任务等依赖后解除池归属；不能仅根据当前目录显示为空判定。必要时正常退带归槽，保留介质内容。成功不等于擦除，也不等于已经机械出库。

### tape move

```sh
ctl tape move TAPE001L8 -l LIBRARY_SERIAL -L homeslot \
  --request /var/lib/tape-client/move-home.json
ctl tape move TAPE001L8 -l LIBRARY_SERIAL -L ieslot \
  --request /var/lib/tape-client/move-ie.json
```

上述为未分池带；已分池带必须附加所属 `-p POOL`。去 I/E 前已分池带须先设为 offline，策略操作见下一节。

`homeslot` 回到可信原存储槽，`ieslot` 移到可用 I/E 槽，不接受任意整数槽地址。执行前核验条码、实际位置、address map、home、目的槽空闲及当前所有权；已知 home 被占、无效或来源冲突时拒绝。move 不隐式分池或导入目录；从驱动器移出可能先执行正常关闭和退带。

未分配磁带位于驱动器时，也可直接提交 move；若其原 home 未知，则从实际库存选择空存储槽并固定在原任务中。存在已管理挂载时拒绝此路径。退带不读取或重写 LTFS 索引，但 UNLOAD 可能刷出驱动器已有缓冲，因此任务会记录潜在介质影响。已分配磁带仍要求可信 home 和本轮核验的挂载。

### tape validate

```sh
ctl tape validate TAPE001L8 -l LIBRARY_SERIAL -p archive \
  --request /var/lib/tape-client/validate.json
```

异步核验标签、volume UUID、LTFS 索引及结构一致性，保存真实诊断。结果中的 `scope=label_and_index`、`payload_verified=false` 表示没有读取全部文件正文。没有 `--full-read`；该命令不自动格式化、修复尾部或重建用户命名空间。

### tape offline / online / set

```sh
ctl tape offline TAPE001L8 -l LIBRARY_SERIAL -p archive -o 'drive maintenance'
ctl tape show TAPE001L8 -l LIBRARY_SERIAL -p archive -a offline_message
ctl tape online TAPE001L8 -l LIBRARY_SERIAL -p archive --request /var/lib/tape-client/online.json
ctl tape set TAPE001L8 -l LIBRARY_SERIAL -p archive --patch '{"blacklisted":true}'
ctl tape set TAPE001L8 -l LIBRARY_SERIAL -p archive --patch '{"blacklisted":false}'
ctl tape import TAPE001L8 -l LIBRARY_SERIAL -p archive --request /var/lib/tape-client/refresh.json
ctl tape import --request /var/lib/tape-client/refresh.json
```

上述命令均提交 FDB 持久任务，立即返回 task_id；查询和取消使用 `task show/cancel`。每盘有原请求文件，多盘使用请求目录。离线说明仅支持 1–63 个 ASCII 字母、数字、空格、下划线、连字符，默认当天日期。`set` 支持 `blacklisted`、`online=false`、`offline_message`；禁止用策略 patch 设置 `online=true`。上线必须实际读回标签和索引，失败保持离线；它不验证全部正文。已分配卷刷新统一使用 `tape import`，沿用原卷归属及路径映射，支持 Full/增量来源。对应 HTTP 路径及完整请求字段见[管理模型](management-model.md#磁带策略与索引刷新)。

## 外部系统通知

通知使用 FDB 持久订阅和 SSE，无需配置外部接收 URL。CLI 输出不自动确认；外部系统可靠处理完整批次后显式 ACK。跨节点重连沿用同一消费者实例，未确认批次原样重放。

```sh
ctl -l LIBRARY_SERIAL notify create ops-platform
ctl -l LIBRARY_SERIAL notify inject --operation REQUEST_UUID --event-type tape.state.changed --data '{"barcode":"MOCKL8","current":{"state":"need_replace"}}'
ctl -l LIBRARY_SERIAL notify list
ctl -l LIBRARY_SERIAL notify show ops-platform
ctl -l LIBRARY_SERIAL notify read ops-platform --generation GENERATION_UUID
ctl -l LIBRARY_SERIAL notify watch ops-platform --generation GENERATION_UUID
ctl -l LIBRARY_SERIAL notify ack ops-platform --generation GENERATION_UUID --delivery-id DELIVERY_UUID --through SEQUENCE
ctl -l LIBRARY_SERIAL notify delete ops-platform --generation GENERATION_UUID --abandon-unacknowledged
```

创建时可以用 `-p` 选择池，重复 `--barcode`、`--event-type` 选择范围；默认从创建时的日志尾部订阅，`--from-start` 回放本功能保留的历史。重连 `notify watch` 可重复 `--event-type` 扩大到包含原类型的集合，或用 `--all-event-types` 扩大到全部；不允许缩小。扩大类型时原类型继续补发积压，新增类型从扩展生效后的新事件开始；ACK 和原待确认批次保持不变，原批次 ACK 继续有效。`notify show/list` 显示连接、待确认与积压状态。删除明确放弃未确认通知。完整 HTTP 接口、批次协议、去重及保留规则见[外部系统持久通知](fdb-notifications.md)。

## 异步任务与原请求恢复

### 受理与完成

rescan、import、assign、move、unassign、validate 先保存原请求，再持久登记任务，返回后后台执行。典型输出身份为：

```text
task_id: NAMESPACE_UUID:EPOCH:OPERATION_UUID
request_file: /var/lib/tape-client/import-one.json
task.status: queued
```

这是字段示意，不是完整 JSON。新建批量请求返回 `requests` 数组；import 的 request-only 恢复返回单个 `task_id/request_file/task` 对象，其他设备命令可返回单元素 `requests` 数组。脚本应按命令输出读取每个原任务身份，不能假定所有命令的外层结构相同。

```sh
ctl task list -l LIBRARY_SERIAL
ctl task show NAMESPACE_UUID:EPOCH:OPERATION_UUID -l LIBRARY_SERIAL
ctl task cancel NAMESPACE_UUID:EPOCH:OPERATION_UUID -l LIBRARY_SERIAL
```

| 状态 | 含义 |
|---|---|
| `queued` | 已登记，等待执行 |
| `running` | 执行中，查看 phase、已提交进度和诊断 |
| `succeeded` | 该原任务成功完成 |
| `failed` | 失败，需读取原错误及可能已发生的介质变化 |
| `cancelled` | 已在安全边界完成取消 |

任务与终态保存在 FDB；客户端请求 JSON 用于恢复原操作，SQLite 不是这些管理任务的权威。取消请求不能即时中断正在执行的 CDB，也不保证物理回滚。批量按盘保存结果，部分成功不代表整批可撤销。

assign 的旧索引只读预检查在设备命令边界检查原任务的取消状态；正在执行的命令返回后停止后续读取，无需停止服务。FORMAT 开始前仍须通过持久化检查点，不能把取消解释为已撤销完成的格式化。

### 恢复同一个请求

响应丢失、客户端退出或需要继续查询时，保留原文件并省略新建参数：

```sh
ctl tape import --request /var/lib/tape-client/import-one.json
ctl tape assign --request /var/lib/tape-client/assign-empty.json
ctl tape move --request /var/lib/tape-client/move-home.json
ctl tape unassign --request /var/lib/tape-client/unassign.json
ctl tape validate --request /var/lib/tape-client/validate.json
ctl library rescan --request /var/lib/tape-client/rescan.json
ctl pool create --request /var/lib/tape-client/create-pool.json
ctl pool delete --request /var/lib/tape-client/delete-pool.json
ctl pool set --request /var/lib/tape-client/pool-policy.json
```

多盘请求逐个使用返回的子请求文件恢复，不把整个目录当作单个原请求。恢复时不再给条码、路径、目的位置或格式化选项。若附加作用域断言，必须与原请求一致；池断言使用保存的 UUID，不能用后来改动的名称。

原文件存在时重复执行“带条码/池名的新建命令”会失败，防止覆盖原身份。`--request` 省略时也会自动保存：import 使用 `$XDG_STATE_HOME/tape-rs/imports/` 或 `$HOME/.local/state/tape-rs/imports/`，其他顶层池/设备管理使用相应的 `tape-rs/management/`。以输出的实际文件路径为准，文件权限 0600、创建的状态目录 0700。

### 按保存文件查询管理任务

```sh
ctl fdb control status /var/lib/tape-client/validate.json
ctl fdb control list -l LIBRARY_SERIAL --limit 100
ctl fdb control list -l LIBRARY_SERIAL --limit 100 --after CURSOR
ctl fdb control cancel /var/lib/tape-client/pool-policy.json
```

`control status` 返回 `null` 表示该身份尚未登记，不是任务成功。策略请求可用 `fdb control run FILE` 等待原终态；设备任务优先使用对应的 request-only 命令受理，再查询 `task show`。池创建使用独立创建请求格式，不能把 `create-pool.json` 交给 `fdb control`。

`task clearhistory --yes` 虽出现在完整帮助中，但当前 FDB 管理回执尚无该清理入口，不将它作为 FDB 历史回收操作。文件上传结果另用 `fdb status REQUEST_DIR` 查询，不与管理任务混用。

## FDB 策略与节点查询

常用运维使用上面的 `tape offline/online/set`。底层 FDB 策略请求仍可直接保存和查询：

```sh
ctl fdb tape show TAPE001L8 -l LIBRARY_SERIAL
ctl fdb tape set TAPE001L8 -l LIBRARY_SERIAL --namespace NAMESPACE_UUID \
  --patch '{"online":false}' --request /var/lib/tape-client/offline.json
ctl fdb tape set TAPE001L8 -l LIBRARY_SERIAL --namespace NAMESPACE_UUID \
  --patch '{"blacklisted":false}' --request /var/lib/tape-client/allow.json
ctl fdb drive show DRIVE_SERIAL -l LIBRARY_SERIAL
ctl fdb drive set DRIVE_SERIAL -l LIBRARY_SERIAL --namespace NAMESPACE_UUID \
  --patch '{"enabled":true,"role":"mr"}' --request /var/lib/tape-client/drive.json
```

策略命令只改管理状态，不隐式装卸或写 MAM；offline 后介质可能仍在驱动器，需要显式 move。`role` 使用 `m/r/g` 角色组合。namespace 从所选库/池查询结果取得，防止目录重建后误改另一份状态。

```sh
ctl node list -l LIBRARY_SERIAL
ctl node show 1 -l LIBRARY_SERIAL
ctl drive list -l LIBRARY_SERIAL
ctl cluster
```

`cluster` 展示各节点自述，执行者状态以当前库的就绪观测为准。磁带运维入口共用 FDB 管理任务；旧顶层 blacklist/clear-blacklist/objects 入口已移除，黑名单使用 `tape set`。`reclaim` 仍待 FDB 原生实现，不能因帮助存在就按已完成处理。`admin` 需要对应接口规定的完整正文，不能用空对象替代持久请求。模式边界见 [FoundationDB](foundationdb.md)。

## FDB 文件与对象操作

### 查询、S3 规则分页与校验下载

```sh
ctl fdb stat /dir/file.bin -l LIBRARY_SERIAL -p POOL_UUID
ctl fdb list -l LIBRARY_SERIAL -p POOL_UUID --prefix dir/ --max-keys 1000
ctl fdb list -l LIBRARY_SERIAL -p POOL_UUID --prefix dir/ --delimiter / --max-keys 1000
ctl fdb list -l LIBRARY_SERIAL -p POOL_UUID --prefix dir/ --continuation-token TOKEN
ctl fdb children /dir -l LIBRARY_SERIAL -p POOL_UUID --limit 100
ctl fdb get /dir/file.bin ./download.bin -l LIBRARY_SERIAL -p POOL_UUID
```

文件命令的池必须使用 UUID。`fdb list` 按完整 UTF-8 对象键的字节字典序排列后分页，默认/最多 1000 条；公共前缀也占名额。使用返回的 continuation token 时保持 prefix/delimiter 等条件一致；跨页不保证同一快照。`children` 则是目录直接子项查询，不等同于对象列表。

`fdb get` 需要普通文件记录带 SHA-256 或 MD5，校验后才发布本地输出，且不覆盖已有文件。仅从 index 导入但没有摘要的卷，不能据此保证校验下载可用；应读取具体错误。

### 持久上传

```sh
ctl fdb prepare ./source.bin /source.bin -l LIBRARY_SERIAL -p POOL_UUID \
  --request /var/lib/tape-client/upload-one
ctl fdb upload /var/lib/tape-client/upload-one
ctl fdb status /var/lib/tape-client/upload-one
ctl fdb status /var/lib/tape-client/upload-one --details
```

prepare 保存独立正文和原身份，默认只创建；覆盖须在 prepare 加 `--expected-version CURRENT_VERSION`。请求路径是新目录。upload 使用原正文和原 UUID，成功返回完整 FDB 版本/提交证据；响应未定时保留目录查询或继续原 upload，不重新 prepare。准备阶段不会写带。

### 节点变更与改名

```sh
ctl fdb fs attributes /dir -l LIBRARY_SERIAL -p POOL_UUID
ctl fdb fs prepare /newdir -l LIBRARY_SERIAL -p POOL_UUID --namespace NAMESPACE_UUID \
  --change '{"kind":"create-directory"}' --request /var/lib/tape-client/mkdir-one
ctl fdb fs run /var/lib/tape-client/mkdir-one
ctl fdb fs status /var/lib/tape-client/mkdir-one
ctl fdb rename prepare /old /new -l LIBRARY_SERIAL -p POOL_UUID \
  --namespace NAMESPACE_UUID --expected-version SOURCE_VERSION --no-replace \
  --request /var/lib/tape-client/rename-one
ctl fdb rename run /var/lib/tape-client/rename-one
ctl fdb rename status /var/lib/tape-client/rename-one
```

修改/删除节点须提供原逻辑版本；创建要求父目录存在。rename 准备阶段固定两端身份，替换目标须用其原 `--expected-target-version`，与 `--no-replace` 互斥。rename 的 run/status/cancel 从原目录读取作用域，不再附加 `-l/-p`。

S3 客户端也访问同一 FDB 目录：List/HEAD/GET、PutObject 创建/覆盖、DeleteObject 和 RenameObject 统一走此权威。S3 PUT/DELETE 等待磁带提交及 FDB 发布，不把受理当作成功；具体请求和 SDK 配置见 [S3接口](s3.md)。

完整文件系统变更 JSON 契约、取消与物理提交边界见 [FoundationDB](foundationdb.md)。顶层 `put/get/delete/copy/list/stat/file-task` 的参数也列在附录中；本节为当前 FDB 部署的推荐持久请求工作流。

## FUSE 与服务进程

### FDB 池挂载

```sh
mkdir -p /mnt/tape /var/cache/tape-client
tape-fuse -e 10.131.7.207:7401 -e 10.131.7.208:7401 -e 10.131.7.209:7401 \
  --library LIBRARY_SERIAL --pool POOL_UUID --fdb-namespace NAMESPACE_UUID \
  --cache-dir /var/cache/tape-client /mnt/tape
```

FDB namespace 模式默认只读；显式提供 `--fdb-journal-dir` 才启用持久文件/节点修改与原操作恢复。挂载在前台运行，需要 Linux FUSE 和挂载权限。`fsync` 成功才确认内容落带，`close` 不能代替归档确认；卸载使用 `fusermount3 -u /mnt/tape`。详情见[文件客户端](file-client.md)。

### ltfsd

主要参数为 `--id`、`--listen`、重复的 `--peer`、`--data-dir`、`--libraries-config`、`--client-listen`、`--fdb-cluster-file`、`--fdb-owner-writable`，需要 S3 时增加 `--fdb-s3` 和监听参数。完整默认值见 [ltfsd 参数](reference/ltfsd.md)。

生产配置按节点固定，带库按真实设备序列号绑定。`--fdb-metadata-readonly`、`--fdb-owner-readonly` 与 `--fdb-owner-writable` 分别对应不同服务能力。正常停止使用部署单位的 `systemctl stop tape-rs-physical.service`，由服务完成提交、退带和释放预留；停机宽限不是机械 CDB 超时。

`ltfsctl fdb init` 是新空 namespace 的初始化工具，要求冻结屏障、固定初始化 UUID 和直接 FDB 连接，不是现有磁带导入入口。已有正常工作的集群不需为了使用管理命令再次 init。部署步骤见 [FoundationDB](foundationdb.md)。

## 常用操作流程

### 导入现有磁带目录

1. `library list/show` 确定库与就绪执行节点。
2. 必要时 `library rescan`，查询任务成功后 `tape list/show` 确认条码和位置。
3. `pool list/show` 选择池，或 `pool create` 创建池。
4. `tape import BARCODE -l SERIAL -p POOL --request FILE` 提交原请求。
5. `task show TASK_ID -l SERIAL` 查询至终态；成功后用 `fdb list/stat` 检查目标目录。

### 新介质入池

1. 查询库存，确认目标未分池、位于可用存储槽且可初始化。
2. `tape assign ... --yes --request FILE`；只在明确覆盖已有文件时加 `--force-format`。
3. 查询原任务成功，再用 `tape show` 确认池归属和卷身份。

### 空卷解绑并移至 I/E

1. 对目标卷运行 `tape unassign`，等待无依赖证明和任务成功。
2. 再执行不带 `-p` 的 `tape move ... -L ieslot`，等待任务成功。
3. 查询实际位置；I/E 到存储槽使用 `-L homeslot`，保留原请求。

保留池归属的卷移至 I/E 时，先用 FDB 策略请求设为 offline，再以所属 `-p POOL` 提交 move。上述流程是不同目标的示例，不能把“解绑”当作绕过有效文件依赖的方法。

## 错误处理与退出状态

| 现象 | 处理 |
|---|---|
| CLI 返回 0 且任务 queued/running | 仅表示受理；继续查询原 task，不能报告任务完成 |
| CLI 业务错误非零 | 阅读 stderr 和已输出的每盘结果，保留原请求 |
| 网络断开或等待超时 | 原任务可能已执行，先查原身份；使用 request-only 恢复 |
| 请求文件已存在 | 使用原文件恢复，另建文件会生成另一个操作 |
| 多库未指定 library / 找不到唯一池 | 先 list/show，再指定准确库与池 |
| namespace/revision 不符 | 查询当前权威及原任务；不要编辑旧请求改绑到新状态 |
| 没有可信 home、原槽被占或位置冲突 | 保留实际观测，解除冲突后处理原任务，不盲重发 MOVE |
| validate 成功但 payload_verified=false | 已完成标签/index 检查，不能当作文件正文校验 |

`ltfsctl` 的业务错误退出码为 1；clap 参数解析错误通常为 2，帮助输出为 0。各工具不承诺统一的细分业务退出码。`cluster` 会逐节点输出不可达信息，脚本应检查输出，不能只看其进程退出码。

批量请求可能部分成功。failed 不意味着介质未改变；运行中的取消也不能保证没有副作用。错误报告中的设备、status、sense key、ASC/ASCQ 和阶段应一并保留。

## 完整参数附录与更新方式

- [ltfsctl：全部子命令、位置参数和选项](reference/ltfsctl.md)
- [ltfsd：服务端启动和配置选项](reference/ltfsd.md)
- [tape-fuse：集群与直接设备挂载选项](reference/tape-fuse.md)
- [tape-library：独占单机库命令](reference/tape-library.md)
- [tape-rs：底层 SCSI/LTFS/PR 命令](reference/tape-rs.md)

附录由本轮实际部署的五个全特性 binary 采集，记录各自 SHA256。帮助是参数事实，服务模式和操作语义以本手册及链接的功能文档为准；本轮物理验收范围见[管理模型现场报告](../.scratch/ltfs-ha/reports/management-model-physical-20261104.md)。未把附录中全部底层命令作为本轮实带测试项目。

修改 CLI 后，用相应版本的构建产物更新附录：

```sh
cargo build --release --all-features --bins
python3 tools/generate-cli-reference.py --bin-dir target/release
```

如设置了 `CARGO_TARGET_DIR`，将 `--bin-dir` 改为实际 release 目录。启用 fdb 的 binary 需要系统动态库路径已包含 `libfdb_c.so`，或设置正确的 `LD_LIBRARY_PATH`。生成器只递归执行 `--help`，不连接集群、不访问磁带，也不执行示例中的业务命令。

### 磁带监控字段

`library show` 的 `monitor` 显示 active node 的后台库存采集周期、最近尝试/成功时间及错误。`tape list/show` 与对应 HTTP 接口读取同一份 FDB 监控记录，不在查询时装带或读取每盘索引。

```bash
ltfsctl -e HOST:7401 library show -l LIBRARY
ltfsctl -e HOST:7401 tape list -l LIBRARY -p POOL
ltfsctl -e HOST:7401 tape show BARCODE -l LIBRARY -p POOL
```

磁带详情的 `monitor` 包含 sample、last_success、alert_history、media_fault、last_error、verified_generation、write_allowed、data_write_allowed；sample 包含实际 drive_serial、采样时间、写保护、数据/索引分区容量、告警编号及采样错误。`available/used/usable` 的单位为 bytes，capacity_source=mam 表示有真实 MAM 观测；未知为 null。槽位磁带不自动装带采样，已有采样是历史值。完整状态含义及写入准入规则见 [管理模型：磁带监控](management-model.md#磁带监控)。

永久故障的依据在 `monitor.media_fault_evidence`，与最新错误 `monitor.last_error` 分开保存；后续设备或传输错误不会抹掉原始永久故障证据。告警编号的解释和作用域见 `sample.alert_details` 及 `alert_history_details`。

监控运行时不调用 EE/LE 命令、HTTP/REST 或内部接口，也不从 EE 缓存取数；所有设备观测由 tape-rs 自己的 SG_IO/SCSI 链路采集。无需安装或启动 EE。


### 容量与可回收空间

`tape list` 的 `Reclaimable%` 是占可用容量的估计百分比。`tape show <barcode> --jsonpp` 同时提供 `reclaimable_bytes`、`live_bytes`、`live_data_objects` 和 `statistics`；可用 `tape show <barcode> -a statistics --jsonpp` 查看统计来源、修订、完成时间、采样时间和未知原因，沿用全局带库、池选择参数。HTTP 磁带列表和详情返回相同字段。

后台按池分页扫描并将检查点保存在 FDB，按实际数据 UID 去重 EE 别名的共享正文。扫描未完成或目录发布使旧结果失效时显示 `calculating`；MAM 样本缺失、与当前卷头不符或容量粒度/压缩使估算不足时显示 `unknown`，表格用 `-`、JSON 用 `null`。当前可见数据量与磁带历史写入量不同；该估计值不能用来直接决定擦除或格式化。
