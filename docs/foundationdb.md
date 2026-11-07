# FoundationDB 首版

本项目按首个版本实现 FDB。数据库、协议、客户端与服务端按当前模型成套使用；不维护原型数据、旧入口或旧格式的读取回退，不提供迁移适配。开发验证使用新建隔离 FDB 和重建的测试数据。真实磁带与正式运行数据的清理必须有明确目标，不能用开发数据库清理代替介质授权。

## 数据与权威

逻辑归属为 **Library → Pool → Tape**：一个带库包含多个池，每个池只属于一个带库，每盘数据带同时最多属于一个池。库中尚未分池的介质作为 `unassigned` 库存显示，清洗带单独标识；它们不是可直接写入的池成员。`tape import` 固定带库、池 UUID 和条码，发布成功时原子建立介质归属与文件索引。跨库池或重复分配请求必须拒绝。

FDB 是活动命名空间中的文件、池和物理副本目录权威。Raft 负责多数派执行许可，设备 PR 负责隔离，FDB owner epoch 绑定当前执行轮次。提供内容之前同时验证 Active 路由、冻结水位、owner、Serving 许可及介质证据。任一不确定状态返回失败，不退回 SQLite 读取，不把目录不可用转换成不存在或空列表。

节点主记录采用必需的 `N` 标记与版本1编码。目录持有非零的属性版本，普通文件的版本仍由 `FileHead.version` 唯一决定；目录 stat 返回 `version`，SDK 拒绝缺失、零值或非整数。目录属性版本不代替目录项范围读取、非空检查及路径/卷互斥。旧无版本节点不读取、不补1，开发库按本版重建。节点变更计划还强制绑定父目录属性版本，缺失字段的原型计划直接拒绝。

属性使用内联值或按摘要寻址的不可变 `M` 对象。大对象分短事务预上传；目录发布事务先核对对象完成标记，再切换节点引用。完成标记的并发变化会使发布事务冲突，旧对象保留至命名空间回收，供已经取得引用的读取使用。

物理副本使用带 `R1` 标记的当前编码，来源明确区分 `FileRequest`、`Imported` 和 `NodeLink`。文件收据保留真实请求窗口代次；整卷原生介质接入使用 `Imported`，不填造上传代次。`NodeLink` 保存管理请求代次和 operation，绑定真实原生链接创建的独立来源，不能作为上传收据。属性修改保留原来源及数据读取构造。回收标记使用 `X1` 并保留同一来源；收据窗口仅约束 `FileRequest`，其他来源的回收标记保守保留至 namespace 清理。当前历史副本回收仍仅适用于 Ready 命名空间，Active 与任务历史 GC 尚未提供入口。缺失标记、未知类型、旧无标记或旧回收编码一律拒绝，开发库按本版重建。

每个池有独立虚拟目录根。对外路径为池内规范绝对路径 `/dir/file`，对象键为 `dir/file`；库序列号和池 UUID 必须显式指定。内部目录可以用池 UUID 分区，但它不出现在 S3 键中，也不写成 LTFS 目录。不同池可以拥有同名路径。命名空间根目录里的原型文件不属于此模型。

导入依据可信 LTFS 索引、卷身份和目录核验建立副本，所有已注册池均生成池根，包括没有分配磁带的空池。激活前核验完整命名空间和副本来源；未通过核验的目录不可见。新增上传只使用已归属、扫描就绪并有可信来源的卷；不自动认领、格式化或擦除介质。

## 运行模式

服务端和客户端均不要求安装 GPFS 或 IBM EE。导入直接读取 LTFS 索引并将目录、范围和原始介质来源发布到 FDB；不生成或召回 GPFS stub。`tape import -P` 指定池内逻辑目录，FUSE 挂载的是本项目提供的目录视图，不是 GPFS。具体边界见[运行时依赖约束](adr/0002-gpfs-independent-runtime.md)。

构建服务端及原生初始化命令需要 `--features fdb` 和匹配的 FDB C 客户端库。Rust 客户端及 `ltfsctl` 的查询、下载和上传命令默认构建即可使用，不需要 FDB SDK。

- `--fdb-metadata-readonly`：仅 FDB 元数据查询，不启动设备执行器。
- `--fdb-owner-readonly`：Raft/PR owner 接任和按需内容读取，可能装卸磁带。
- `--fdb-owner-writable`：恢复并提交 FDB 上传，含未确认 S4 的处理。
- `--fdb-s3 --s3-listen 0.0.0.0:7402`：开放匿名 path-style S3；只读 owner 提供读取；可写 owner 支持 PutObject、DeleteObject 和 RenameObject。

owner 模式要求 FDB 集群文件、多库配置及客户端 HTTP 地址。S3 端口须与各节点 `--s3-port` 一致。FDB owner 模式可显式共用 `--http-vip` 与 `--http-vip-interface`。多库 VIP 沿用 Raft 入口 owner 的多数派许可和严格撤址确认，FDB 数据仍通过原 `/v1/fdb` 路由访问；共享 `/cluster` 控制发现及规范库、池、磁带管理入口，不开放旧 SQLite 文件或管理写入路由。owner-readonly 的文件内容只读，仍允许持久池管理以及显式 rescan、assign、move；import、unassign、validate、online 等介质生命周期任务和文件写入需要 owner-writable。metadata-readonly 不执行管理变更。S3 同时指定 `--fdb-s3`。这些开关不构成现场操作授权。

## 原生初始化

全新空带库先完成三节点 Raft 的 `catalog/current/quiesce` 与 `catalog/current/freeze` 控制屏障；冻结时必须没有设备 owner、未完成目录投影或活动任务。管理 POST 仍要求显式 `confirm=true`，freeze 还须提供已提交的 `quiesce_index`。这些操作须使用明确的带库作用域，等待全部节点应用冻结水位。

quiesce 关闭新目录变更，不会自动结束健康设备 owner。初始化时先正常停机完成设备释放，再运行仅控制阶段的三节点 Raft/HTTP，保持候选不可访问设备，完成 freeze 和屏障核验。本轮物理部署以临时 systemd `DevicePolicy=closed` 实现这一阶段；原生初始化后正常停止控制实例，以最终 FDB owner 单位启动并恢复设备权限。它不导出旧 SQLite 文件目录。

使用固定且非 nil 的 namespace UUID：

```sh
ltfsctl -e NODE1:7401 -e NODE2:7401 -e NODE3:7401 --library SERIAL fdb init \
  --cluster-file /path/to/fdb.cluster --namespace NAMESPACE_UUID
```

命令直接读取三个指定节点的 `GET /libraries/SERIAL/admin/catalog/current/native-barrier?challenge=CHALLENGE_UUID`，要求同一成员配置、term、leader、暂停/冻结水位和三个不同节点身份，各节点均已应用冻结记录、无 owner、无池与磁带目录。管理节点地址的认证沿用部署侧管理网络边界。命令只读取控制状态，不自动暂停服务或操作介质。

FDB 在一个事务中建立空根、物理带库、请求窗口、维护状态和 Active 路由。初始化要求未使用且没有孤立记录的 namespace，不降低已有 owner epoch。初始 epoch 为冻结水位加一；设备执行器随后仍须取得有效的新 Raft 轮次、PR 和 Serving 许可。无需 SQLite 目录导出；已删除原型 `fdb_empty_cutover` 辅助入口。

重试必须使用相同带库与 namespace。已有成功结果通过完整原生激活证明核对后返回，保留后来推进的 owner epoch；成功后无需再次读取已重启节点的冻结屏障。已有其他 namespace、导入来源、损坏证明或根记录均拒绝覆盖。当前激活证明明确区分原生与介质导入来源，不接受无来源前缀的原型编码。

受管 namespace 必须有布局版本2标记。磁带反向归属键固定包含 namespace、版本2、池 UUID、带库和条码，使空池检查可以按池进行有界查询。原生与导入初始化均只建立新布局；活动路由、owner 接任与归属操作拒绝缺失、旧版本1或未知版本，不自动回填。初始化也拒绝无 namespace 主记录却残留归属、写前预约或其他所属记录的状态。首版不读取旧反向键，原型开发数据须清除并重建，不提供迁移或兼容路径。

初始化后目录为空，可启动 owner、创建 Active 池，并通过原生介质接入任务登记已有干净 Full 或完整增量链的 LTFS 磁带。通过显式确认的 tape assign 初始化空白介质，其他运维命令见下文管理模型。

## Active 池

普通客户端和默认构建的 `ltfsctl` 可创建、查询和分页列举池：

```sh
ltfsctl -e NODE:7401 --library SERIAL pool create archive --request create-pool.json
ltfsctl -e NODE:7401 --library SERIAL pool show archive
ltfsctl -e NODE:7401 --library SERIAL pool list --json
```

CLI 只读捕获 namespace 并在原文件中固定池 UUID；重试使用同一 namespace、UUID、名称和上限。创建仅在 FDB owner 模式受理，入口最多转发一次到当前 owner；本机须有已恢复的 owner 与 Serving 许可。写事务重新核对 Active 路由和 FDB owner epoch，原子登记池主记录、名称索引、虚拟根 inode 和目录项。默认策略为无介质限制及默认告警策略。已有相同属性返回原记录；UUID 或名称冲突返回409，不更新已有池或创建时间。创建不分配磁带、不装卸、不写介质，也不创建旧 Raft/SQLite 池记录。

HTTP 使用 `POST /libraries/SERIAL/admin/pools`，携带 `x-tapers-catalog-namespace: NAMESPACE_UUID`，正文包含 `version=1`、固定 `id`、`name`、`file_limit`、`created_at_millis` 和完整 `policy`，最多4 KiB；创建返回201，原请求重试返回200。SDK 不确定响应时保留原 UUID 和创建时间；切换 namespace 后拒绝原请求。提交后执行许可失效可能返回504，应按原 UUID 查询。详情为 `GET /libraries/SERIAL/admin/pools/POOL_UUID`；列表为 `GET /libraries/SERIAL/admin/pools?limit=100&after=TOKEN`，默认及最多100条，按池 UUID 排序。游标绑定库和 namespace，跨页不保证同一快照。

当前持久池记录编码统一为 v4，必须含有效的 UTC 毫秒创建时间。该时间表示 FDB 池登记事件，不推断磁带格式化时间；重试与属性更新均不改变它。无时间字段的原型格式不读取、不补写，开发数据按当前格式重建。查询核对虚拟根和控制索引，缺失或冲突返回失败，不以空池或隐式修复代替损坏记录。池重命名、空池删除、策略修改使用下述持久化管理任务。

池虚拟根的五个初始 LTFS 时间统一来自池记录的 UTC 毫秒创建事件，保存为九位小数的 UTC 文本；初始 readonly=false，file_uid=1 仅为逻辑根元数据，不证明任何物理卷的根 UID。原生创建将池记录、索引、根目录项、inode 及不可变查询元数据投影原子写入。离线导入使用 `create_pool_root(namespace, pool)`，从已登记池记录生成同一根；仅允许 Building/Ready，不改变来源声明修订号。通用目录接口拒绝代建已登记的池根。已存在的根必须完整且与原创建记录一致，缺目录项、属性或投影均报错，不回填旧原型数据。导入预期摘要也使用该创建事件；后续物理卷格式化或导入时间不覆盖它。

删除只允许目录为空、没有任何磁带归属、介质接入屏障或活动写入目标的池：

```sh
ltfsctl -e NODE:7401 --library SERIAL pool delete POOL_UUID --request delete-pool.json
```

请求固定池 UUID 与预期 revision，保存及重试规则与其他管理任务相同。执行事务核对全部约束后，原子清除池主记录、名称索引及虚拟根，保存最终修订号的退役记录和成功回执。已删除 UUID 永久拒绝再次创建；旧创建或删除请求不能使其复活，也不能影响后来复用旧名称的新 UUID。排队任务可由新 owner 接续，已取消任务不执行。空的已绑定卷、待接入归属、离线或黑名单磁带同样阻止删除；损坏索引报错，不假定没有磁带。不提供强制删除，不隐式卸带、解除归属或擦除介质，非空池清理和介质释放须使用对应显式命令。

## 管理策略与持久化任务

池策略、驱动策略与磁带在线/黑名单策略的权威在 FDB。设备执行器在完整介质请求之间处理管理队列，再从同一 FDB 读版本读取本次调度所需的池、驱动和磁带策略；不从冻结的 Raft 策略补值。磁带策略仅点查已挂载与本次目标条码，最多65个，不扫描历史介质。驱动默认启用、角色 mrg；模型不包含节点分组。

```sh
ltfsctl -e NODE:7401 --library SERIAL fdb drive set DRIVE_SERIAL --patch '{"role":"mr","enabled":true}' --namespace NAMESPACE_UUID --request drive.json
ltfsctl -e NODE:7401 --library SERIAL pool set POOL_UUID mount_limit 1 --request pool.json
ltfsctl -e NODE:7401 --library SERIAL pool set POOL_UUID name archive --request rename.json
ltfsctl -e NODE:7401 --library SERIAL tape show BARCODE
ltfsctl -e NODE:7401 --library SERIAL --pool POOL tape offline BARCODE --request offline.json
ltfsctl -e NODE:7401 --library SERIAL --pool POOL tape set BARCODE --patch '{"blacklisted":false}' --request allow.json
ltfsctl -e NODE:7401 --library SERIAL --pool POOL tape online BARCODE --request online.json
ltfsctl -e NODE:7401 fdb control status pool.json
ltfsctl -e NODE:7401 fdb control run pool.json
ltfsctl -e NODE:7401 fdb control cancel pool.json
ltfsctl -e NODE:7401 --library SERIAL fdb control list --limit 100
```

客户端先只读准备并核对 namespace，再用0600、不覆盖、文件与父目录 fsync 保存稳定请求，策略管理提交后等待终态，磁带导入则异步返回任务号。文件已存在时直接失败，避免把原请求覆盖成新身份。响应丢失或进程重启后保留文件，通过 control run/status 使用原 UUID 与请求代次；不得重新生成请求来猜测首次结果。保存文件损坏、符号链接或非规范 JSON 均拒绝。默认客户端构建无需 FDB SDK。

策略与原生生命周期任务写入需要 writer 模式，池管理及显式 rescan、assign、move 也可在 owner-readonly 受理；非 owner 最多转发一次，登记与执行分别核对 Active 路由和 owner epoch；设备 actor 还须持有 Serving 许可才执行。FDB 先原子登记 queued 请求和队列索引，执行时把策略变更、成功/失败终态及队列移除放进同一事务。策略任务的取消与执行原子竞争；介质任务进入 running 后，取消会请求分批回滚，回滚完成才进入 cancelled。终态不可更改。业务修订冲突保存为 failed；损坏或权威变化停止本轮服务。queued/running/failed/cancelled 均不表示介质写入成功。执行器接任后自动继续未结束队列。

每个请求是最多4 KiB的规范 JSON，字段严格为 version=1、library、namespace、epoch、operation、change。池、驱动及磁带变更带预期 revision；CLI 从当前资源读取后填入，提交前资源再次变化时失败而不覆盖。

同池导入或维护屏障产生的 `PoolImporting` / `PoolBusy` 是等待条件。策略请求保留原 queued 身份，执行器轮转后继续处理其他池的可执行任务，保留 Library 的 Serving 和取消入口；屏障解除后再应用原策略。同步等待超时只表示结果未定，使用原请求查询或继续等待。设备错误、损坏状态及 owner 失效仍停止本轮服务。

池 patch 允许 name、file_limit、media_restriction、mount_limit、low_space_warning_enabled、low_space_warning_threshold、no_space_warning_enabled。名称沿用创建规则：非空、最多255字节，不含 NUL 或斜线，按原始文本区分，不自动改写。重命名在同一事务检查 revision、更新池主记录、释放旧名称并登记新名称；名称冲突不留下策略或名称索引的部分修改。损坏或孤立名称引用报错，不覆盖修补。UUID、虚拟根和创建时间保持原值，改名不影响按池 UUID 访问文件；原成功请求重试只返回原结果，不把后来改名的池恢复成旧名称。驱动 patch 允许 enabled、role；role 为 m/r/g 的无重复非空组合。文件数上限按卷物理索引中的文件条目计算，包含链接条目，不包含目录；已核验基线来源记录保存文件数，S4 索引观察与本代计数原子登记。来源记录强制保存文件数、真实原始索引种类及完整前驱链：版本1、文件数8字节和带版本的来源身份；Full 为77字节，最多64个前驱时为4301字节。不接受此前42/50字节原型记录，开发库需重建。选带跳过达到上限的卷，实际追加前再次检查，跨越上限的批次在任何预约/追加前拒绝。已提交请求的重试仍返回原成功结果，读取不受写入文件数限制。修改策略本身不装卸介质；介质限制、驱动角色与挂载上限在后续调度中生效。告警开关仅完成策略存储，新的 FDB 容量监控与告警执行仍待接入。

磁带 patch 支持 online=false、blacklisted 布尔值及 offline_message 文本，至少一个字段；online=true 必须通过 tape online 的实带核验任务。离线说明独立按 namespace/library/barcode/volume 存在 FDB，随原归属解除清理；不修改卷内容。任务还必须固定 barcode、pool、volume 和预期 revision；条码重新绑定、池/卷或修订号变化时拒绝旧变更。策略仅能针对已绑定卷，尚在接入保留阶段的条码不可修改；卷的扫描就绪状态不应阻止管理者将其禁用。CLI 的 `--pool` 可作额外归属断言，`tape import` 仍须提供目标池。条码归属使用必需的版本化44/60字节编码，含 revision 与两个策略标志；初始 revision=1、online=true、blacklisted=false。不接受此前33/49字节原型记录，也不回填冻结状态；开发库按本版重建。

离线或黑名单阻止后续内容读取、写入选带、实际追加及需要写索引的脏尾收尾；纯元数据完成已经物理提交的批次不受该策略阻断。读取前后均核对当前策略修订。策略禁用返回暂不可用并保留可信 owner 的 Serving 许可。已准备但未提交的上传保留原请求，重新允许后可重试同一请求；已经成功的上传与管理任务重试只返回原终态，不重复写带或应用策略。策略任务只改 FDB，不隐式 MOVE、写带或修改 MAM；禁用时已挂载介质可能留在驱动器，调度不会为腾出驱动器自动卸载它。显式机械管理使用 tape move；offline 本身不移动介质。

HTTP 前缀为 `/v1/fdb/control/SERIAL`：GET window、POST prepare 为只读；POST requests 登记任务（queued 返回202），GET requests 分页，GET/DELETE requests/EPOCH/OPERATION 查询/取消，GET policy?kind=drive|tape&target=BASE64URL 查询单资源。点查/取消必须给出唯一 x-tapers-catalog-namespace；列表默认/最多100条，游标绑定库、namespace和资源。管理请求使用独立代次窗口，不与上传窗口混用；历史回收尚未提供入口，当前不自动删除回执。


## 带库和磁带管理入口

管理命令参考仓库 `ee.md` 的 EE 1.4.0 资源语义。正式调研依据真实 IBM 带库上 EE 1.3.4 的运行与后台跟踪：查询使用缓存，rescan 刷新库存，import 读取索引并构建目录，validate 核验标签/索引。assign、unassign、独立 move 的 EE 行为当前仅有官方文档和实际帮助，不能称已实测，详见 [生命周期研究](research/ee-management-lifecycle.md)。Holo 不计正式调研或物理验收。

```sh
ltfsctl -e NODE:7401 library list --jsonpp
ltfsctl -e NODE:7401 library show SERIAL -a serial
ltfsctl -e NODE:7401 tape list -l SERIAL --unit binary
ltfsctl -e NODE:7401 tape list -l SERIAL --json
ltfsctl -e NODE:7401 tape show BARCODE -l SERIAL -a poolname
```

`list` 默认输出表格，支持 `--no-header`、`--json`、`--jsonpp`；磁带列表的容量显示可选 `--unit decimal|binary`，不能与 JSON 同用。`show -a` 只显示指定属性。库标识使用真实 changer serial，多库时 `show` 和导入必须显式选库；磁带全局列表按库标识、条码排序。CLI 自动取尽磁带分页；HTTP 每页默认100、最多100，`after` 绑定所选库集合和各 Active namespace。

只读 HTTP 入口：

- `GET /admin/libraries`：所有配置带库。
- `GET /libraries/SERIAL/admin/library`：一库属性。
- `GET /libraries/SERIAL/admin/tapes?limit=100&after=TOKEN`：该库磁带分页。
- `GET /libraries/SERIAL/admin/tapes/BARCODE`：一盘磁带。
- `GET /admin/tapes`：跨库磁带分页；详情使用带库作用域入口。

查询只读取 FDB 归属与已保存的设备观测，不打开 SG 设备、不装卸带、不扫描索引。观测由当前 owner 在恢复和介质接入结束后通过既有设备层采集，带库序列号、owner epoch 和 Active route 在发布事务中核对；查询返回原观测时间和 owner 匹配信息。`host_device_name` / `host_scsi_address` 属于 `host_device_node` 所指观测节点，不应作为其他节点的本地设备路径使用。没有可信容量、可回收比例或只读策略证据时返回 null，CLI 显示 `-`；不按 LTO 型号猜容量，也不将缺失当成零。已分配但当前不在库存中的条码仍可查询；仅当库存与当前 owner 匹配时才把已绑定卷的缺席显示为 missing。清洗带独立标记，不作为数据带导入。

### 顶层磁带导入与异步任务

```sh
ltfsctl -e NODE:7401 tape import TAPE01L8 TAPE02L8 -l SERIAL -p POOL
ltfsctl -e NODE:7401 tape import TAPE01L8 -l SERIAL -p POOL -P /incoming -R --request intake.json
ltfsctl -e NODE:7401 tape import --request intake.json
ltfsctl -e NODE:7401 task show NAMESPACE:LIBRARY_SERIAL:EPOCH:OPERATION -l SERIAL
ltfsctl -e NODE:7401 task cancel NAMESPACE:LIBRARY_SERIAL:EPOCH:OPERATION -l SERIAL
```

池可用名称或 UUID。CLI 从服务捕获并固定当前库、namespace、池 UUID 与请求窗口；不要求用户操作 FoundationDB。默认在 `XDG_STATE_HOME/tape-rs/imports`（未设置时使用 `~/.local/state/tape-rs/imports`）的独立0700目录保存0600请求。`--request` 在单盘时指定新文件，多盘时指定保存各请求的目录。先保存原请求再登记任务；断线或等待超时后，只使用对应原文件恢复，不能重新生成请求来推断旧结果。多盘是独立任务，队列按设备许可串行执行；批次返回各盘的原文件和任务号，部分失败整体返回非零。

导入目录使用池内规范绝对路径。默认 `-P /IMPORT`，每卷放入 `/IMPORT/BARCODE`；指定 `-P /incoming` 时放入 `/incoming/BARCODE`；加 `-R` / `--merge-tapes` 时直接合并到指定目录，不再追加条码层。EE 导出卷保留原 GPFS 路径作为相对子树，所以原 `/ee-gpfs/dir/file` 导入 `-P /incoming -R` 后位于 `/incoming/ee-gpfs/dir/file`。已有目录合并并保留原 inode/属性；普通文件重名时在完整文件名后寻找 `_1`、`_2` 等空位，例如 `name.txt_1`，不覆盖原文件。文件与目录类型冲突明确失败。目标路径映射与任务进度持久化，新 owner 继续原映射；取消/失败只撤销本任务创建的节点，不删除已有合并目录。恢复 `--request FILE` 不允许另传路径或合并参数。

`tape import` 始终异步受理并立即返回任务号，不提供同步等待模式。命令成功只表示任务已受理，不代表磁带已导入；通过 `task show TASK_ID` / `task list` 或对应 HTTP GET 查询阶段、计数与终态。受理响应丢失时保留原文件，通过 `tape import --request FILE` 提交或查看同一任务；已经 failed/cancelled 的原请求只返回原终态，不偷偷生成新任务。公开任务号包含 namespace，切换目录后查询或取消旧任务不会误指新目录里的同号任务。 任务权威记录、阶段、已提交进度和终态保存在 FDB，与原请求身份绑定；SQLite 仅存本地可重建的索引暂存/重放缓存。SDK 使用 `submit_management_saved` 和 `management_status`；同步 `run_management_saved` 拒绝介质导入，避免客户端长期占用连接等待设备。

HTTP 先使用带库管理 context/requests/prepare 固定完整请求，再以规范 JSON 提交到 `POST /libraries/SERIAL/admin/pools/POOL_UUID/tapes/BARCODE/import`。请求中的 library、barcode、kind、namespace、epoch、operation 都与原准备结果一致；该接口复用同一持久管理任务，不另造第二套任务。`GET /libraries/SERIAL/admin/tasks/EPOCH/OPERATION` 查询原结果，`POST /libraries/SERIAL/admin/tasks/EPOCH/OPERATION/cancel` 请求取消，`GET /libraries/SERIAL/admin/tasks` 分页列任务。带库作用域路径和 `x-tapers-catalog-namespace` 头均为必需。只读服务拒绝导入和取消。

本项目恢复的是池内 FDB 目录及磁带归属，正文保留在源磁带，不构建 GPFS stub、不做 EE 的介质格式转换。默认 IMPORT/BARCODE、-P/-R 和自动冲突 _i 使用上文规则；已有源池标记不匹配时拒绝，不偷偷重写。空白带初始化使用另一个显式确认的 assign 任务。

### 流式处理与进度

索引物理块缓冲在分配前验证大小，范围为 1..=0xFF_FFFF 字节，与 `TapeDrive` 的 READ(6) 24-bit 限制一致；异常标签返回 `LocalCapacity`，不发送数据 READ 命令。

流式 XML 扫描在读取事件前限制事件缓冲，单事件和文本字段各有 64 MiB 预算；当前未闭合节点及祖先保留的元数据输入共享 128 MiB 预算。发出节点后归还该节点的预算，不按累计文件数保留目录树。超限返回 `LocalCapacity`，不截断属性或形成可发布的来源证明；完整目录与增量重放继续使用受容量和剩余空间约束的磁盘暂存。原始索引摘要、两分区构造和必需前驱链仍须完整核验，流式处理不代表只读头部。

索引读取、Full/Delta 重放和候选列举使用有界内存及受预算限制的磁盘暂存，不把所有文件或整份 XML 装进一个 Vec，也不把文件正文复制回磁盘。先核对固定来源，再以小页预检、分批事务物化；本任务的临时目录受池屏障保护，只有完整来源再次核验和最终发布成功后才对外可见。不能因某批元数据已写入便声称整盘成功。

任务查询的 `progress` 是与任务同一 FDB 读版本的派生视图：`source_pending` 表示尚未固定完整来源（可能等待介质或正在读索引）；随后为 `preflight`、`materialize`、`rollback` 或终态。来源确定后返回 `physical_files`、`total_paths`，物化/回滚阶段返回实际 `processed_paths`；未知计数为 null，不估算虚假百分比。取消正在执行的 CDB 不会立即中断设备命令；现有索引捕获及重放阶段的响应时间也受其分段检查边界约束，大索引的取消延迟与吞吐仍须现场验证。

## 原生介质接入

显式选择现有 LTFS 磁带与目标池：

```sh
ltfsctl -e NODE:7401 --library SERIAL --pool POOL tape import BARCODE --request intake.json
ltfsctl -e NODE:7401 tape import --request intake.json
```

HTTP/SDK 使用严格的 `media-import` change：`kind`、`pool`、`barcode`、`volume`（规范 UUID 或 null）、`path`（如 `/IMPORT`）、`merge_tapes`（布尔值），六个字段均必需。`volume` 提供时必须与实带标签 UUID 相同；执行器从实际介质读取并绑定卷身份。原 `fdb tape import` 入口已删除，原四字段请求也不再接受。首版持久化模型变更先完整归档核验受影响应用元数据，再在停机后清理并重建；不读取旧请求或节点分组格式，不清理 FDB 系统数据或磁带内容。

入口仅在 writer 模式注册。此命令明确授权对指定条码装带和只读核验；不格式化空白介质、不修尾、不写文件标记或 MAM。真实设备操作仍须遵守部署的带库与介质授权范围。本项目导入 FDB 对象目录，不进行 IBM EE 的 GPFS stub 转换，不因 EE import 会写磁带而隐式修改介质布局。

设备访问前先原子登记条码保留、running 任务和池屏障；首次导入保留未分配条码；已分配条码须与目标池及原卷一致，并固定原刷新快照。两者均须符合池介质限制。写前文件预约、文件收据和批次占用均阻止接入开始；反向的文件预约及直接收据登记也在同一事务检查池屏障，不能让早先 prepared 的请求绕过接入。上传在查询已知终态后、装带前检查池屏障，池忙时返回 Busy 并保留原请求和健康 Serving；接入结束后可重试同一请求，已知成功或取消结果不受池忙影响。调度沿用 Serving、PR、库存、密度、驱动读取角色、启用与挂载上限约束。缺介质或合格驱动时等待，队列轮转使后续策略任务仍可执行。核对标签后，通过有界流式恢复流程捕获干净 Full 或完整增量链，核验 DP/IP 构造、有限历史及完整节点重放。任务首次绑定即固定原始 head 和每个必需前驱的卷、代数、位置、种类及原始字节 SHA-256，再交叉核对本机暂存清单和 extent 缓存；不将合成 Full 的摘要当成原始增量摘要。暂存使用私有文件、物理空间预留与配置上限；索引和候选分批处理，不把整份对象目录放进 FDB 单事务。

接入先预检全部逻辑路径，再分条物化目录、文件、原生链接、EE 别名与删除记录。来源的 files 是包含 EE 数据对象与链接的物理文件数，paths 是公开逻辑候选数，二者独立；隐藏的数据对象可能使前者更大。文件上限使用物理计数，接入进度使用逻辑候选计数。已有同名目录保留 inode 和属性，普通文件冲突追加 `_i`，类型与删除冲突拒绝覆盖；未知属性保持在独立不可变属性对象中。池屏障期间，该池的公开路径查询、目录分页、S3 对象分页和新上传均返回暂不可用，其他池继续使用。物化记录附带本任务回滚清单；取消或确定失败后，逐页核对记录摘要再撤销本任务创建的数据。目标被改动时保留屏障并报错，不猜测删除。失败或取消不会反向 MOVE 补偿，介质可能留在驱动器；后续显式机械管理使用 tape move；offline 本身不移动介质。遗留的不可变属性对象待后续 GC，不影响原有文件。

发布前再次核验标签、实带索引和整个固定前驱链；同一事务发布扫描来源、就绪指针、卷放置、条码归属与成功回执，并解除池屏障。中断后的新 owner 接续 running 任务与回滚进度，旧 owner 无权继续。接入没有 SQLite 目录导出或 namespace 根文件兼容路径。成功后正常读取和上传使用已发布的实带索引证据；上传的增量 S4 发布后不再隐式追加未登记的 Full checkpoint。

完整重放基线缓存与原始 delta 子集缓存严格区分，缓存身份包含整个来源链。导入后写入新代或重启，冷读可按已登记来源重建旧增量基线，不要求旧基线仍在 DP 尾部，也不要求 IP 仍停留在旧代；暖缓存仍核对物理构造摘要和当前 head/owner/PR。来源或前驱不符则拒绝返回文件，并由现有执行器撤销不可信的服务许可。暂存清单和缓存采用本版必需字段，不读取旧原型 schema。

可写 owner 启动发现已分配磁带的干净 Full 与 FDB 旧头不符且没有本项目在途写入占用时，保留旧基线和管理 Serving，将该带留作待显式导入；普通读写、自动选带及 LRU 不使用它。原 running 刷新先核验原请求、池屏障和快照，再由新 owner 接续，成功发布后才登记可调度挂载。UUID 错误、异常尾、未知增量及已拥有 S4 仍按严格恢复处理。磁带无需先归槽，真实启动、异步刷新和队列取消证据见[物理报告](../.scratch/ltfs-ha/reports/loaded-media-refresh-physical-20261104.md)。

完整 13 项运维命令与 HTTP 层级、安全限制见 [管理模型](management-model.md)。模拟器和本地 FDB 通过不等同于真实带库验收；现场状态及证据单独记录。

## 上传

```sh
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb prepare source.bin /dir/file --request upload-state
ltfsctl -e NODE:7401 fdb upload upload-state
ltfsctl -e NODE:7401 fdb status upload-state
ltfsctl -e NODE:7401 fdb status upload-state --details
```

父目录须已存在。默认只创建；覆盖必须显式指定 `--expected-version`。FDB 逻辑版本从1开始，覆盖版本必须大于0且可递增；0被拒绝，不能代替未指定的只创建条件。服务端从池中选择合格卷，条码仅可作为诊断约束，不是普通上传的必填项。准备只查询，不 MOVE、不写带、不预留路径或配额；实际写入重新验证资源、卷代数、路径和当前版本。 SDK `prepare_file` 使用必填 `UploadCondition`：创建传 `Create { namespace, parent }`，parent 是原直接父目录的 inode，覆盖传 `Replace(FileVersion::Fdb { namespace, inode, version })`。只含数字版本的 SDK 参数已删除；跨域版本、空 UUID、零或溢出版本在读取源文件和发 RPC 前拒绝。准备应答必须匹配原 namespace、创建父目录或覆盖 inode 和版本；同名删除重建即使版本号相同也失败，不把条件改绑到新 inode。CLI 沿用现有 `--expected-version`，只读捕获真实目标身份并核对用户版本后提交完整条件；创建只读捕获目标直接父目录的 namespace 与 inode（不绑定会因兄弟项变更而递增的目录版本），不新增命令或标志。

内容覆盖保留原逻辑文件的文本及二进制自定义 xattr 和 readonly，重新生成时间、卷内 UID、extent 与内容摘要，并清除源属性中的 `tapers.version`。同卷与跨卷覆盖均从 FDB 原逻辑 inode/版本读取完整属性；捕获后，预约事务再次核对原身份与版本条件。服务端沿用索引字节预算作为整个批次覆盖属性的编码字节上限，在任何文件追加前完成属性读取；属性缺失、损坏或超出预算时拒绝追加。

SDK `FdbClient::prepare_file(library, pool, path, source, condition, barcode, directory)` 在发出 prepare RPC **之前**，先保存原 operation UUID、完整逻辑条件、长度、SHA-256 与独立正文。CLI 先只读捕获目标身份，再进入此工作流。目录0700权限、`request.json` 和 `payload` 为0600；目标必须不存在。正文独立 inode 流式复制，核对长度和 SHA-256，文件及目录持久化后原子发布，不使用活动源文件的硬链接。原意图一经保存便固定，源文件后续修改、截断或删除不影响恢复。

本地必需状态文件采用当前格式 version=2，含 version=2 的原逻辑意图、可空的服务端准备结果及整体校验摘要。原意图的 parent 字段必需：创建必须是非空原父目录 UUID，覆盖必须为 null，并保留原文件 inode/版本。尚未准备的 Draft 不填造文件 inode、卷、扫描或代数。父目录删除重建后，即使路径相同，原 Draft 仍拒绝新的 parent，保留原 UUID 与正文，不自动改绑或发 PUT。纯 prepare RPC 成功后，核对原条件与原操作，先 fsync 临时状态文件，再在独占目录锁下原子替换整个状态并持久化；只有完整准备结果落盘后才允许 PUT。文件缺失、截断、摘要不符或字段不完整均报错，不解释成新的 Draft，不从当前路径修复。不接受旧物理头单 JSON 或旧本地头格式，没有兼容 reader。服务端准备头和上传身份采用当前 version=2：仅有 namespace、parent、file、previous、operation、epoch、size、sha256、可选 barcode，库、池、路径来自外层及 URL；不存在物理占位字段。

准备失败时已发布的原请求目录和正文保留。`FdbClient::status_saved(directory)` 返回 `NotPrepared` 表示尚未完成准备，此时不发物理结果查询；`upload_saved(directory)` 使用保存的同一 operation、条件和正文继续纯 prepare。不能重新生成 operation 或改绑目标。逻辑准备身份保存后，使用 `PreparedUpload::load(directory)` 和原身份状态查询。CLI 沿用 `prepare/upload/status` 入口，失败后保留目录并运行 `upload`，不重新 prepare。准备结果只固定原 namespace、父目录、路径、inode、覆盖条件、operation、请求代次、正文及可选条码约束，不绑定物理卷、扫描或下一次索引代数。同卷其他提交推进索引后，尚未准入的原逻辑请求仍可提交；执行线程在首次准入时选择当前合格目标。已有写前预约、已升级 I 收据或 S4 未确认的请求必须复用或恢复原物理尝试，不改写客户端准备记录、不生成新 operation，也不重新追加未知结果的内容。

`FdbClient::upload_saved(directory)` 返回 `CommittedUpload`，不再返回单个数字。成功必须取得并验证原操作的完整回执；`version()` 返回真实 `FileVersion::Fdb { namespace, inode, version }`，`operation()` 返回原 UUID，`details()` 保留原物理字段及完整属性。PUT 201 或轻量状态中的 committed 数字仅触发原回执查询，不能单独确认完成。 原完整回执验证后，SDK 将其以 `receipt.json` 保存到原请求目录：当前 envelope version=1，绑定必需原请求头的摘要，并以独立 domain SHA-256 校验全部字段；0600 临时文件 fsync 后以 NOREPLACE 原子发布，持久化目录。随后才校验并删除本目录中长度、SHA-256 与 dev/inode 均一致的独立 `payload`；源文件不删除。已保存的原请求头与回执继续保留。尚无本地完整回执且服务端完整回执缺失、损坏或属于其他操作时返回错误，保持原请求目录和正文，不从当前路径补证、不重新准备。原操作已提交且完整证据有效时，即使保存的正文已被用户移除，也能只读返回首次回执。CLI 沿用 `fdb upload`，输出原操作 UUID 与完整 FDB 版本。 已有本地完整回执时，重启后的 `upload_saved`、`status_saved` 和 `saved_receipt` 均可在无服务端、无正文的情况下返回原结果，不受服务端保留窗口清理影响。若回执已落盘但正文尚未删除，只有显式 `upload_saved` 会完成清理；查询不修改目录。`saved_receipt(directory)` 优先读本地完整回执，没有本地记录时只查询服务端原结果而不保存。CLI `fdb status --details` 使用该只读流程。损坏、截断、超预算、非普通或不属于原请求的本地回执均报错，不回退网络修复；缺少必需原请求头也不能从回执重建。

需要跨调用保留原目录身份时，使用 `UploadJournal::create(library, pool, path, source, condition, barcode, directory)`：它仅在本地保存原意图和独立正文，不发 RPC；`UploadJournal::open(directory)` 只独占打开现有请求，不创建新请求。句柄不可克隆，提供已验证的 library、namespace、pool、path、operation、condition、size、sha256 和 prepared 只读访问。将同一句柄传给 `FdbClient::prepare_journal` 与 `upload_journal`；失败后继续持有并重试，不按路径重新选择操作。目录被移动、原路径放入另一份合法请求时，句柄仍只操作原目录、原 UUID 和原正文；原请求头被替换或修改则在 RPC 前拒绝。独占租约随句柄跨线程移动，其他写入者无法同时打开；释放句柄只关闭描述符，保留磁盘记录。`prepare_file` 和 `upload_saved` 便捷入口共用这一实现。

一次 upload 调用仅发送一次可能到达服务端的 PUT。响应不确定时查询原 operation；不按路径内容推断归属，不新建 operation 或换卷重写。再次执行同一命令先查询原身份；已有成功结果可直接返回，未确认时核验并持久化保存的原正文后发送。未确认且保存正文损坏或缺失时保留目录并报错，不重新准备请求。该约束用于写入正确性，避免首次版本中的重复追加，不是历史兼容功能。

上传结果 GET 在元数据只读入口同样开放，不需要启用 writer、当前 owner 就绪或当前执行轮次。客户端继续提交保存的原身份与 namespace；服务端在同一 FDB 读版本核对 Active 路由、请求窗口及保留回执的原指纹，不读取当前路径，也不访问设备。原请求提交后，后续覆盖、删除、同内容重建及 owner 接管不会改变它的首次结果。没有终态的原请求与不同 operation 仍返回未确认；请求指纹不符返回冲突，namespace 不符拒绝。已完成的 upload 重试可以先从只读入口取得原回执并直接返回；尚未确认的请求不能在该入口执行 PUT。

目录查询以原逻辑请求指纹识别首次结果，指纹覆盖逻辑条件和条码约束，不包含执行 owner、物理卷、扫描或索引代数。物理写前预约采用当前格式 version=2，保留实际卷、代数、扫描、owner 和逻辑条码约束；预约事务在同一读版本核对条码约束与实际不可变卷放置；不匹配返回 PlacementMismatch，且不写预约、占用或配额。未指定条码仍可自动选带。同一已准入操作的物理身份仍须完全一致，只有执行围栏可按已验证的接管协议更新。终态同时保留实际物理收据指纹和原逻辑指纹，原始详细回执返回真实提交时的物理字段。

详细回执使用 `GET /v1/fdb/upload-receipts/SERIAL?pool=UUID&path=BASE64URL`，携带与上传状态相同的原身份头。SDK `FdbClient::upload_receipt(&prepared)` 和现有 CLI `fdb status DIRECTORY --details` 返回原提交的 namespace、文件 inode、逻辑版本、copy、卷/代数/扫描、真实 LTFS fileuid、条码、长度、SHA-256 及完整原属性；未确认返回202/None。该查询不读取当前同名路径、节点 head、当前放置或 owner。后续属性修改、覆盖、删除、同名重建均不改变原回执。

原属性在 S4 已观测后的准备事务中与真实收据原子登记，终结前核对其身份与属性引用。小属性另存 namespace-owned `@u` 子键，大属性复用不可变 M 对象，正常物化和批次整理不删除；随 namespace 清理。原请求窗口仍限制可查询期；缺失、截断或不匹配的已提交证据报错，不补造或从当前路径修复。详细属性读取预算8 MiB、JSON响应预算64 MiB；轻量状态和上传确认不依赖详细属性预算。该回执不伪造 Raft round 或物理写入轮次；FDB 文件写入使用同一完整原回执和本地日志，见下方挂载说明。


逻辑身份与执行许可分别校验：未绑定执行者的身份可以用于纯路径校验和结果查询，但不能获得写前预约或物理发布许可。准备与正文 PUT 仍要求当前 writer、Serving、路由、Raft/PR 守护及原资源条件；只读结果查询不放宽这些写入检查。

同卷原生写入组的每次设备命令校验使用一个新的 FDB 只读事务，在同一读版本核对活动路由、owner、机械任务屏障、池归属、回收子任务证明及每个原物理预约或升级收据。公共权限不再按组内对象重复开启事务；校验结果不跨命令缓存。曾观察到的原预约消失、任一物理身份改变或依赖不可核实时，设备守护立即锁定，后续命令不能继续。逐 CDB 的多数派许可、PR 校验和提交屏障保持不变；这项查询优化不合并严格串行请求的独立磁带提交。

设备线程在追加前检查大小、SHA-256、配额、池/驱动策略、Serving、PR及FDB路由。只有索引S4证据、目录发布和必要终结完成才确认提交。接任重放已观察/封存批次，不重新追加已经确认的数据。T1/T2 收尾遵守已有显式恢复策略；不自动salvage、不截断或覆盖已确认文件。

## 查询与下载

```sh
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb stat /dir/file
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb children /dir --limit 100
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb list --prefix dir/ --max-keys 1000
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb list --delimiter / --continuation-token TOKEN
ltfsctl -e NODE:7401 --library SERIAL --pool POOL_UUID fdb get /dir/file output.bin
```

`stat`、`children`、`get`、`list` 都以显式池为作用域。children是目录直接子项，每页最多100；对象list默认/最多1000，支持0，按完整UTF-8键的字节字典序排序后截页。`delimiter=/` 将后代合并为CommonPrefixes，每个公共前缀占一个名额。支持prefix、start-after和绑定库/池/namespace/查询条件的续页token。跨页不保证同一快照。

对象列举合并导入目录与已发布Active文件；未确认意图不出现。内部目录分页有界，单页使用一个FDB读版本，不全量收集命名空间排序；巨大或稀疏查询触发资源/事务上限时返回失败。百万/亿级性能仍待验收。

下载要求普通文件及记录中的SHA-256或MD5。客户端流式写0600同目录暂存，限制长度并校验摘要，复查对象身份/版本/位置后才不覆盖发布。传输/校验失败清理暂存；不覆盖已有文件、符号链接或发布时并发创建的输出。发布后的目录fsync失败可能留下已经校验的完整输出，应检查本地结果。断线不跨连接续写半个文件。

## FDB 挂载

```sh
tape-fuse -e NODE:7401 --library SERIAL --pool POOL_UUID \
  --fdb-namespace NAMESPACE_UUID --cache-dir /path/to/private-cache /existing/mountpoint
```

`--fdb-namespace` 显式选择 FDB 模式，要求真实带库序列号及池 UUID，与本地设备模式互斥。挂载固定 namespace，不接受读取过程中切换到另一命名空间。省略 `--fdb-journal-dir` 时强制内核只读，支持目录、普通文件、原始时间/只读属性、二进制 xattr、原生符号链接和 mmap；所有修改均拒绝。默认 `--features fuse` 客户端只通过 HTTP 访问服务，无需链接 FDB SDK。读取正文仍需要具备读取许可的服务 owner，元数据查询可以没有 owner。

文件属性先获取轻量 stat 与有界原属性，只有 namespace、池、inode、逻辑版本及类型相同才组合；变化时返回 EAGAIN，不拼接另一个节点的属性。文件 stat 的 `generation` 是当前副本的原内容来源代数，元数据更新不会把它改成逻辑版本或最新 S4 代数。目录没有物理卷代数，`FileStat.generation` 为未知；缺少的代数、条码和 SHA-256 不作为虚拟 xattr 暴露，自定义同名属性不能伪造这些字段。MD5 文件按 MD5 校验，不伪装成 SHA-256。

目录读取按100条分页，再对每个子项点查身份与类型，并核对读取前后的父目录身份和版本；目前会在内存收集本次目录项，不承诺百万级目录性能。该目录接口与最多1000条的 S3 ListObjectsV2 分开。跨页排序、身份冲突、非 UTF-8 名称或不完整属性返回错误，不静默隐藏或补造内容。

隔离 FDB、SimLibrary 与实际本机内核挂载已验证目录、文件、链接、时间、二进制 xattr、mmap 和修改拒绝。此结果不代表真实磁带验收。指定专用持久化日志目录可启用下述文件、节点修改及[持久改名](#fuse-rename-持久改名)。

启用文件与节点修改时显式指定日志目录：

```sh
tape-fuse -e NODE:7401 --library SERIAL --pool POOL_UUID \
  --fdb-namespace NAMESPACE_UUID --fdb-journal-dir /path/to/fdb-journals \
  --cache-dir /path/to/private-cache /existing/mountpoint
```

日志根为专用 0700 目录；不存在时仅创建末级，父目录须存在。挂载在根目录上持有独占租约，拒绝并发复用、符号链接、开放给其他用户的目录、未知文件或不属于该库/池/namespace 的记录。所有本地记录先校验作用域，再逐个恢复原操作，内存与同时打开的描述符不随记录数增长；恢复失败时不启动挂载，保留日志和未确认正文。目录内当前保留已确认头与回执，启动成本随历史请求数增长，回执归档与历史 GC 仍待完成。

共享 `UploadState` 持有原 `UploadJournal`，跨 RPC 失败、关闭、重开与根目录同步沿用同一个 operation。首次准备前持久保存原条件和独立正文；创建绑定原父目录，覆盖绑定打开时完整原版本。请求尚未确认时，写入和截断返回 EBUSY，不改写该原操作正文。上传和摘要均读取已打开的缓存文件描述符，缓存路径被替换也不会上传外来文件。日志根或请求目录改名后仍按原描述符访问；不按旧路径重新选择合法但不同的请求。必需原记录缺失或损坏时失败关闭，不生成替代操作。

FDB 文件 `flush` 同步完成 S4 并取得完整原回执；`fsync` 成功同样表示本次内容已确认。回执持久化并校验后才释放日志中的独立正文，随后允许以已确认版本开始下一次覆盖。挂载根 `user.ltfs.sync` 确认已有写句柄及已关闭 pending 的原上传，不按当前路径哈希推断结果，也不额外追加 Full 检查点。启动恢复只使用保存的原请求；缓存丢失、进程崩溃、原路径同内容重建及回执丢失后均不重放已确认的追加。

已用隔离 FDB、模拟磁带和默认构建的真实内核 FUSE 验证创建、覆盖、随机写、截断为空、重新挂载、原版本与父目录冲突、丢回执、SIGKILL 测试挂载进程后的原操作恢复。此入口还支持 mkdir/rmdir、unlink、链接、时间/只读属性、xattr 和持久 rename，并按保存的原操作恢复。上述故障注入来自隔离 FDB/模拟介质；正常物理带库读写及客户端重挂结果见[FUSE 实测报告](../.scratch/ltfs-ha/reports/fuse-fdb-physical-20261104.md)。

## S3 对象接口

Bucket ID为 `bucket_name(真实带库序列号, 池UUID)` 的稳定哈希。池清单从FDB Active记录读取，不依赖冻结的旧管理状态。

已实现基础读取：ListBuckets、HeadBucket、GetBucketLocation、ListObjects V1/V2、HEAD、GET、单字节Range、ETag和修改时间条件。S3键直接对应池内路径，不带内部池UUID前缀，不查询命名空间根目录原型数据。ETag标识namespace/inode/逻辑版本，不伪装MD5。完整 HEAD/GET 在 checksum-mode=ENABLED 时，优先返回上传时验证且与当前 SHA-256 内容摘要匹配的 CRC32；没有有效 CRC32 时返回已知 SHA-256。Range 不返回全对象 checksum。

HEAD/GET 返回真实 LTFS 修改时间的 Last-Modified；对象列举返回 LastModified。支持 If-Modified-Since 和 If-Unmodified-Since，按 HTTP 秒精度比较：If-Match 优先于 If-Unmodified-Since，If-None-Match 优先于 If-Modified-Since。无效日期忽略、重复条件头拒绝；条件结果在 Range 和读带之前判断。304 不携带正文长度，保留 ETag、已知修改时间和保存的缓存策略。未知修改时间不伪造，需要该时间作条件判断时返回503。

读取 `tapers.s3.headers` 中保存的 Content-Type、Content-Encoding、Content-Language、Content-Disposition、Cache-Control、Expires 和 x-amz-meta-*。投影最多128个响应头、编码后最多8 KiB，拒绝非法字段、头注入、大小写重复或超限；正文长度和 ETag 不能由该属性覆盖。CRC32 与其正文 SHA-256 绑定可保存在投影中，格式须为标准 Base64 的四字节和32字节；回读只在绑定匹配当前内容时使用 CRC32，不能用旧属性替代当前内容摘要。投影在属性发布前生成，以 namespace 和完整属性摘要索引，导入封口时与完整属性核验。它与属性对象一同保留至命名空间回收；HEAD/list只读取小投影，不加载大属性正文。S3成功响应的Cache-Control仅来自已保存对象投影，不透传内部下载暂存接口的默认no-store，HEAD/GET保持一致。投影缺失或损坏返回失败，不从旧格式或大属性正文恢复。开发数据库按当前模型重建。

非owner仅向配置的S3节点端口转发一次。内容经真实owner完整读取和暂存核验，回执后复核目录及Serving再发布；回执前失去许可时不返回内容。HEAD、列举、条件失败、错池与非法范围不读带。只读 owner 拒绝修改；可写 owner 支持普通 PutObject、DeleteObject 及下述 RenameObject，全部使用 FDB 与受隔离的设备执行器，不进入旧 SQLite 写入队列。ListBuckets 的 CreationDate 使用池记录中的真实创建时间，以 UTC 毫秒精度返回，不使用占位日期。PutObject 支持创建/覆盖并替换对象元数据；DeleteObject 复用原版本条件的单节点 Unlink 持久任务，等待 LTFS 索引提交与 FDB 发布后返回204。PutObject 校验定长正文、MD5/SHA-256/CRC32 后调用既有 FDB 上传，磁带 S4 和目录发布完成才返回200。原 S3 替换属性在追加前以 QU 子域记录绑定 namespace、epoch、operation 和原逻辑请求指纹，带独立完整性校验，损坏不回退重建；原始 NodeMeta、MD5 和 S3 响应头随实际 LTFS 属性及提交回执发布。与现有属性共同保留至 namespace 清理。所有写入受实际上传暂存预算限制，HTTP断开后已开始的原任务继续持有资源至结束。multipart、CopyObject、批量删除等其他高级操作仍明确拒绝，支持范围见 [S3接口](s3.md)。

协议依据：[ListObjectsV2](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html)、[GetObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)、[HeadObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html)。


### S3 RenameObject

`--fdb-s3` 取代仅表示只读的旧开关，不保留别名。`--fdb-owner-writable` 下支持 `PUT /<bucket>/<key>?renameObject`，返回成功时为 `200` 空正文；只读 owner 返回405。现有 pool bucket、匿名和 path-style 规则保持，不要求 AWS 目录桶名称。这是本项目扩展，AWS 仅对 S3 Express One Zone 目录桶提供该操作。[AWS API](https://docs.aws.amazon.com/AmazonS3/latest/API/API_RenameObject.html)、[请求示例](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-buckets-objects-rename.html)。

`x-amz-rename-source` 接受 URL 编码的源 key 或同 bucket 的 `bucket/key`，只解码一次；源与目标 key 最长1024个UTF-8字节。源与替换目标必须是普通对象，两端父目录须已存在；不改名目录、符号链接或跨 bucket 对象。支持源 `x-amz-rename-source-if-match/if-none-match/if-modified-since/if-unmodified-since` 和目标对应的四个 HTTP 条件；不满足返回412。ETag 条件优先于对应时间条件。重复头、无效日期、非空正文和未支持的重命名条件拒绝，不忽略后执行。

`x-amz-client-token` 为1至64个非空格可打印ASCII字符。首次核对源、目标与两端父目录完整身份后，FDB `@/namespace/QJ` 记录在准入前原子绑定 token、规范参数和不可变 rename 请求。并发首次请求只选出一个原请求；相同 token 更换参数返回 `400 IdempotencyParameterMismatch`。记录使用严格版本1编码；损坏时报错，不修补或另选操作。省略 token 时每次调用生成新身份；需要断线恢复的客户端必须保存并重用显式 token。

同 token 重试先读取原任务与独立两路径回执，不重新定位当前源路径、不重做条件判断。完成回执在没有当前 owner、原源路径已重建或目标后来改名时仍返回首次成功；首次记录中的 epoch、operation 与时间不变。超时返回504，不撤销原任务；使用同一 token 和参数继续。只有全部参与卷的 S4 与 FDB 命名空间发布均完成才返回200，不把排队或局部提交当成成功。

改名保留源数据 extent、摘要、用户元数据和 Last-Modified，不复制文件正文。FDB 的 opaque ETag 仍表示 inode 与逻辑版本，改名后会变化。跨卷可能需要装带和改写多个 LTFS 索引，不能承诺 AWS S3 Express 的毫秒级延迟。token 及终态保留随后续生命周期与 GC 策略处理，目前不自动删除。本功能的软件验证与真实带库实测分开记录。

## 当前交付边界

上述 FDB 入口统一采用首版池内模型，检查结果以 `.scratch/ltfs-ha/handoff-to-codex.md` 最新记录和对应证据目录为准。

底层 Rust API `ltfs::recovery::capture_index_source` 已支持在受预算约束的临时磁盘上保留 Full 或增量链重放后的完整节点（属性、链接、extent），并分别返回真实原始构造身份、原始 SHA-256 与前驱证明。返回的 header 不含目录子项，完整内容须逐条枚举，不能把重放视图序列化后的摘要当成原始介质摘要。该 API 仅提供只读来源证据，调用方仍负责 owner/PR 与发布前复核；CLI、FDB 持久来源和历史基线冷读均使用此完整链身份。`capture_recorded_index_source` 只捕获已登记的历史来源，返回 Recorded 作用域；它不声称历史构造仍是最新 DP/IP head，当前 head 由读取流程另行证明。

节点修改的介质组件已提供 `NodeMutationPlan`：类型化表示建目录、建原生符号链接、xattr/只读与时间属性修改、unlink 和空目录删除。规范计划固定库、namespace、池、请求/owner 代次、逻辑 inode/version、扫描来源与原始索引身份，并保存确定时间和单节点前后状态。暂存只修改工作索引；PR 下的干净 S4、持久请求指纹标记及完整结果树摘要一致时，才生成不可变介质证明。内容长度、extent、UID 和未修改的属性均纳入检查，删除仍保留数据块。完整树摘要不依赖 XML 兄弟节点顺序，计划只保留受影响节点的前后属性，计算时按目录排序引用；仍会遍历当前内存索引，尚不是外存 writer 或规模验收。

计划存储使用带库、namespace、内容指纹和声明长度隔离的不可变对象。每块最多64,000字节，逐事务续传；只有完整长度、EOF 和域摘要都核对后才能封口。重复上传逐块比较，不覆盖已存内容；中断或错误摘要留下的未封口对象不能读取，可显式清除后重传，重置令牌使旧上传者失效。已封口对象不能通过该接口删除。读取有处理预算，核对分块长度、摘要、封口令牌及最后一次 owner/路由；失败时私有输出可能已有部分内容，调用方只能在成功后使用。首版 `J1` 对象引用为必需格式，不读取原型引用。所有对象归 namespace 清理范围；这套存储不等于产品备份。

物理节点计划采用规范版本5，必须显式提供完整逻辑祖先链、既有目标类型与属性及 source_data 字段；EE 原目标绑定直接数据对象，其他目标的该字段为 null。非当前版本或缺字段的计划直接拒绝。计划同时绑定原卷的真实祖先元数据、完整源树摘要和逻辑规范属性，构造计划不先改写源树。非根祖先与目标使用逻辑属性并保留每卷实际 UID、文件长度、内容 extent 和链接目标；不把逻辑 UID 写入物理索引。池虚拟根并非卷根别名，物理卷根保留其格式化创建信息和私有/未知属性，逻辑根另行保存在计划内；父目录应更新的 mtime/ctime 按原请求时间合并。

新建路径所在卷缺少已存在的逻辑祖先时，计划显式记录缺失前缀，按该卷最高 UID 顺序分配并物化所需目录，再分配新目标 UID。文件/链接占据祖先路径、缺失顺序不符或 UID 耗尽时，在暂存前拒绝。原源树摘要仍对应没有这些目录的真实视图；投影摘要、staging 和 S4 完整树校验必须一致。S4 证据分别返回逻辑属性和物理属性，并提供祖先逻辑 inode/原属性版本与实际 UID、是否新物化的映射，不能把物理父目录元数据直接写回虚拟根。

`validate_prepared_inputs` 将新物理尝试的逻辑作用域、当前执行 owner、原请求引用/指纹/时间/操作、祖先和目标属性与捕获的 Prepared 输入核对。它不检查驱动 PR、当前物理索引、介质选择策略或持久尝试状态，因此不是写入许可；恢复既有尝试必须读取已保存的原计划，不用新 owner 重造原计划。对新建目标，逻辑 UID 尚只是该卷候选，后续发布必须固定主副本选择。

`upload_node_mutation_plan` 与 `load_node_mutation_plan` 在上述对象层验证规范计划及带库/namespace 绑定；接任者可以读取旧 owner 的原计划，不能据此开始写入。`admit_node_mutation_attempt` 将封口的原计划关联到持久 Writing 尝试及卷占用；对象层本身不登记占用。节点 S4、未发生关闭及目录逻辑发布已有独立接口，并已接入可写 owner 的持久节点队列；HTTP/CLI/Client 已通过下述持久节点入口提供这些操作，FUSE 节点修改也已接入同一持久入口，FUSE rename 已接入同一持久改名入口。封口对象、计划和介质证明本身不授予 FDB 发布权限。

`prepare_node_mutation_baseline` 提供节点修改的逻辑预检：在同一 FDB 读版本核对活动路由与 owner、池归属、规范路径、父目录与目标版本、目标类型和空目录约束。路径链、inode/head/copy 与相关反向记录按带冲突检测的方式读取，空目录检查还覆盖待写子路径；实际写入占用事务仍须重新核对并持久占用目标。新建 inode 固定于 namespace、管理请求代次及 operation，接任不更换身份，已占用的身份不能用于另一条路径。管理窗口独立于文件上传窗口。属性保留该读版本的原始值或不可变大对象引用；池虚拟根必须有登记时保存的完整属性，缺失时报错，不能以磁带格式化时间补值。操作语法检查与物理计划共用；耗尽的目标版本，以及需要更新父属性时已耗尽的父版本，均在介质暂存前拒绝。结果仅为预检快照；自动选带、写入占用及恢复发布仍待接线。

`NodeMutationRequest` 固定库、namespace、池、管理代次、operation、路径、父/目标身份与版本、原始 LTFS 纳秒时间及操作内容，并以独立域摘要绑定逻辑前提。前提包含未知属性摘要和当前文件 head/copy/归属；owner 不属于逻辑请求身份。规范 JSON 严格拒绝缺失、额外、旧版或非规范字段。`prepare_node_mutation_request` 创建新请求，`check_node_mutation_request` 只验证保存的原请求；后者始终使用其保留代次，当前 accepting 推进不会让旧请求改用新 inode。过期代次或已改变的逻辑前提拒绝通过。

逻辑请求和物理计划共用分块对象层，typed 上传/加载分别核对内容类型与作用域，大操作内容不会放入单个 FDB value。逻辑请求指纹与对象摘要使用不同的域，不能混用；两种 typed 加载也不互相接受。接任者可加载原始请求，保留原时间、UUID、前提和完整字节；对象存储仍可保存已过时的请求，实际写入占用前必须重新验证。封口对象和验证结果均不授予介质写入权限。

节点任务组件提供持久排队、准备占用、点查、分页、加载原请求及取消。`queue_node_mutation_request` 先核对原身份，必要时续传并封口原始内容，再在同一事务写入最多4 KiB的任务头、带库内待处理索引及 namespace 级代次反向索引。任务头固定请求引用、逻辑指纹和原始登记 owner/时间；同一身份换内容会冲突，原身份重试返回原记录。已知终态在上传或当前池检查之前返回，池后来删除也不会被旧请求复活。索引缺失、键值冲突或非规范记录报错，不自动修补。

节点任务记录采用规范 JSON 版本6，强制包含 SingleNode/Rename 请求类型、准备 owner/时间、active_attempt、publication_digest 和 rejection_reason 字段；未准备的 owner/时间为 null，仅 Writing 的 active_attempt 为非空 UUID，仅 Completed 的 publication_digest 为非空摘要，仅 Rejected 的 rejection_reason 为非空原因。原因最多500个 UTF-8 字节，不含控制字符；其余状态该字段必须为 null。仅接受当前规范格式，开发任务数据按当前格式重建。

`reject_node_mutation_task` 记录尚未写入任务的永久拒绝，保留原原因、首次完成 owner/时间及原登记和准备信息。Queued、Prepared 可以转为 Rejected；任务头、待处理索引、代次反向索引与本任务的池占用在同一事务收尾。Writing 必须先用原物理尝试和不可伪造证明恢复，不能直接取消或拒绝。已知 Completed、Cancelled、Rejected 重试始终返回原终态，不覆盖首次原因，也不释放后续任务的占用。拒绝不创建节点、head、磁带写入或 Qr/Qd；Rejected 若存在发布结果则报损坏。可写 owner 对失效的原逻辑条件或可识别的写入前拒绝保存终态；池忙、索引未就绪、介质或驱动暂不可用则保留任务。Writing 的物理或权威错误保留原尝试并交给接管恢复。

`prepare_node_mutation_task` 在同一事务重验保存的原请求、保留代次、当前逻辑前提和封口对象，再原子转为 `prepared` 并取得池写入屏障。准备不创建逻辑节点，不选择卷或赋予物理写入权限。同池新预约、直接文件收据及上传准备均被屏障拒绝；已知成功或取消请求先返回原终态。同池其他节点准备、池删除和新介质接入也被阻止，已提交数据的查询继续可用。现有文件占用只有 namespace 级发现范围，因此准备前保守要求整个 namespace 无 0/I/F 文件占用；取得屏障后其他池仍可开始文件写入或节点准备。介质接入沿用已有的 namespace 占用检查。准备重试保存首次准备 owner/时间，新 owner 可原样发现和加载原请求。

`capture_prepared_node_inputs` 只读取仍处于原 Prepared 状态的任务，重验保存的请求与当前 owner/路由，再捕获从池虚拟根 `/` 到父目录的完整祖先链。返回逻辑 inode、目录版本和原属性，同时加载既有目标的完整属性。读取不包含全局 namespace 根，不借用磁带格式化时间；逻辑属性中的 file_uid 不能当作物理卷 UID。祖先身份、目录项、类型、版本和直接父前提必须一致，缺失属性、重复 inode 或目录属性为链接均报错。

输入字节预算包含原请求、祖先路径/身份及属性编码；大属性按清单长度在加载前计入预算，超限不解除 Prepared 屏障。大对象在短事务中读取，读取期间及返回前重新核对 owner 和同一个 Prepared 任务；取消或接任会使不再获准的捕获失败。输入捕获不更改目录、任务或物理索引，也不构成写带证明。物理计划将这些逻辑属性与每个卷的真实祖先 UID/source tree_before 绑定；后续 Writing 转换仍须重新核对。

属性完整加载后，捕获操作使用同一个读取事务重新校验原请求、当前 owner、任务及池屏障、封口对象和完整祖先 C/N 链的身份、顺序、类型、属性版本与摘要。`check_prepared_node_inputs` 提供同样的只读快照检查；它不安装持久占用，也不授予物理写入权限。Writing 准入在安装尝试记录的同一个事务中复用这些非 snapshot 读取，与祖先并发变化产生冲突；不能先检查再另开事务写入。

`queued` 仅表示请求已登记；它不锁定路径或介质，排队期间逻辑前提仍可能变化。接任者从 FDB 原样发现任务并加载内容，旧 owner 无法登记或取消。取消已登记的 queued/prepared 任务时，终态、两个待处理索引及本任务的池屏障原子更新，重试保留原准备信息、终态及完成时间；找不到任务不表示取消成功。Writing 拒绝普通取消，不能清除可能已有物理结果的尝试。分页每页1–100条，包含 queued/prepared/writing，按管理代次及 operation 排序，游标绑定带库和 namespace。代次反向索引检测未完成任务超出保留窗口的损坏状态；窗口推进及任务历史 GC 仍待提供入口。

`admit_node_mutation_attempt` 重核原 Prepared 输入、当前 owner/路由、完整祖先链、封口计划、目标卷归属与在线/黑名单策略、扫描及最新源索引，再原子登记 Writing、独立尝试 UUID、原计划引用和该卷的待恢复占用。目标下一代已有文件预约、批次或通用修尾时拒绝准入；失败不释放原 Prepared 屏障。尝试记录保留原 owner/时间、池与磁带策略修订号、源索引以及现有文件的原始副本/媒体来源，不用别名的零长度覆盖真实数据长度。当前现有文件仅准入 head 选中的主副本所在卷；导入模型不支持同版本多个来源，不将历史 R 行视作当前副本。

普通文件的实际 UID、长度和索引摘要必须匹配固定副本，规范投影也不能改变其内容身份；链接 UID 与 EE 数据对象 UID 分别保留。EE 计划还保存实际完整来源中直接数据对象的路径、UID、长度、摘要及语义条目指纹；准入核对固定副本，暂存前及 S4 证明生成时重新从实际完整索引核对该绑定。缺失、链式或越界目标、错误数据 UID/路径/长度/摘要均拒绝，属性投影不能改变原链接目标。`pending_node_mutation_attempt` 按卷发现原始待恢复尝试，`load_node_mutation_attempt_plan` 加载原计划；接任不重写计划 owner、请求时间或内容。重复准入原尝试返回原记录，另一 UUID/计划不能覆盖它。任务、池屏障、尝试主记录和卷占用缺失或不一致时报错，不自动修补。文件批次及通用索引观察/修尾拒绝节点卷占用，节点恢复必须先处理这些记录。

Writing 准入仅完成持久元数据互斥，不检查设备 PR，也不表示已经提交 S4 或逻辑变更。新建链接还须在该事务确认源卷文件数未达到池上限；unlink 的目标为已有实际文件或链接。执行器仍须核对当前运行许可、PR、策略修订和真实介质状态，区分未开始、T1/T2、已提交但未确认的 S4；不能把加载原计划当成无条件重放许可。

`observe_node_mutation_attempt` 只接受原不可变尝试、原物理计划和 PR 下取得的干净节点 S4 证明。证明在完整结果树校验的同一次遍历中计算实际物理文件数，并与源计数及操作的 `+1/-1/0` 变化核对。观察事务重核当前 owner/路由、Writing 任务与卷占用、扫描及前代，原子保存专用观察、活动卷游标、历史身份和本代文件数；不修改 C/N/head，不解除 Writing 或池屏障。观察已发生的提交不重新套用新写入的在线策略。接任者加载原计划，重新挂载并取得当前 PR 的证明后，可以登记未确认 S4；重复观察保留首次观察 owner 和时间。

`node_mutation_observation` 用于重放前发现已保存结果。观察记录使用严格规范 JSON 版本1，位于 namespace 的 `Qs + volume + generation`，自包含原尝试头、提交身份和计数；历史读取证明不依赖仍在运行的任务或计划分块。Active 历史明确区分文件批次、通用空收尾、节点提交与节点专用修尾，节点历史还须核对观察或关闭记录、卷归属、扫描及摘要绑定的文件数。文件批次证明仍只认可文件批次；读取副本来源另外接受有专用证明的节点 S4，不把任何修尾代数当作新文件来源。缺失、损坏或交叉关联不符均报错，不进行读修复。

`node_mutation_absence_evidence` 在当前 PR 下区分两种未发生证明：`Unchanged` 要求干净索引仍与原来源完全一致；`Discarded` 要求确定的下一代 Full、原来源的正确 Full/Incremental 回指、精确的空收尾标记，以及原标记、最高 UID 和完整文件树保持不变。只归一化已经核对的 `tapers.abandonedBlocks` 与 `tapers.emptyRecovery`，其他根属性、子节点和文件内容均不得变化。语义 no-op 只要已经写入本次节点标记，就不能认作未发生。

`discard_node_mutation_tail` 是显式的有状态接口：先核对原来源、完整树与 PR，再复用现有 T1/T2 的 Discard 追加规则，不打捞文件、不覆盖 DP 旧块。未知新 S4、其他限制状态和错误来源在写入前拒绝；成功后返回同一种不透明未发生证明。设备执行器仍须保证当前 owner、策略和独占执行，从核验到 FDB 关闭之间不得插入其他介质变更；证明不授予盲目重放权限。

`close_node_mutation_attempt` 用该证明原子保存关闭结果、将原 Writing 任务退回 Prepared 并解除本卷占用，保留原逻辑请求、准备 owner/时间、队列和池屏障。Discard 同时推进游标、专用历史及不变的文件数；Unchanged 不推进物理代数。关闭记录采用严格规范 JSON 版本1，位于 `Qc + volume + source_generation + attempt`，自包含原尝试头、结果、首次关闭 owner/时间和计数。`node_mutation_attempt_closure` 可在重放前发现它；原尝试头及计划不因关闭而改写。接任者重新捕获同一逻辑请求，以当前源和新尝试 UUID 准入；旧关闭重试返回首次结果，不清除后来尝试的占用。已关闭的原尝试不能复用，Prepared 仍可按原取消规则终止。Unchanged 后的新尝试可以提交同一下一代，旧尝试的观察查询仍返回未发生，不把新结果冒认为旧结果。

`publish_node_mutation_attempt` 处理建目录、创建原生符号链接和既有目录、普通文件、原生链接及 EE 别名的属性、时间及 xattr 修改。发布要求保存的原请求、原计划、Writing 尝试、节点 S4 观察和当前 owner；在同一事务重新检查完整逻辑祖先链、目标与父版本及属性摘要。目录不得带有文件 head 或孤立副本。大属性先分块上传，发布前核对完成标记；目标 C/N、需要变更的父属性与版本、Completed 终态、不可变结果，以及待处理索引和池/卷占用的释放一次提交。创建目录或链接增加父属性版本；仅改目标属性保留父版本和属性。文件属性修改原子切换 N/head 与新的不可变 R/V 描述，增加逻辑版本，原副本和反向记录保留。新的属性 S4 由发布结果单独绑定；副本的 volume_generation 保持原始已确认数据来源，避免把仅包含别名属性变化的增量索引冒充完整数据来源。读取使用原始数据构造，并另行核对卷当前提交状态。数据 UID、长度、摘要、原始来源及原介质内容版本与路径保持不变，管理请求不冒充上传，不创建文件收据或批次。逻辑虚拟根的身份与属性不被物理卷根覆盖。

新建链接发布真实的零数据 LTFS 节点，逻辑版本从1开始；副本使用创建 S4 的真实代次、scan 与分配的 fileuid，来源为 `NodeLink { management_epoch, operation }`。不创建上传请求、文件收据或批次，也不占用文件请求窗口。后续属性修改保留该创建来源和原构造。链接写入受保护的 `tapers.fdb.nativeSymlink=1` 文本属性；目标可为不存在的路径或含 `.LTFSEE_DATA/` 的任意链接文本，重新导入仍保持原生链接类型。无该标记的外部 LTFS 链接继续按 EE 介质约定识别。重复、未知值、二进制标记或非链接节点携带该标记均拒绝；普通 xattr 修改不能设置或移除它。原生链接可查询属性及目标，数据读取接口拒绝为其提供普通文件载荷。

`node_mutation_publication` 查询保留的原结果。结果位于 `Qr + library + management_epoch + operation`，采用严格规范 JSON 版本1，并由 Completed 任务中的独立域摘要绑定。接任、后续修改目标或介质暂不可用后，原身份重试仍返回首次版本、属性摘要、文件副本描述和完成 owner；不重新应用变更。历史结果不依赖仍在 Writing 的尝试头、计划分块或当前介质状态，也不作为当前内容读取证明。发布结果必须与 Completed 任务及其摘要一一对应；任务缺失，或仍为 Queued、Prepared、Writing、Cancelled 时存在结果，查询、排队、取消和发布均拒绝处理，不把孤立结果视为完成，也不自动补写任务。缺失或损坏结果报错，不自动修补。已完成任务的排队、准备和取消重试均返回原终态。

`publish_node_mutation_deletion` 处理 unlink 和空目录删除，要求原请求、规范计划、Writing 尝试及实际节点 S4 观察。最终事务先完整验证祖先链、父与目标版本和属性、C/N、原 head/副本与介质来源，再原子清除目录边 C、将原 inode 的 N 替换为严格 `NR1` 删除记录、更新父属性与版本、保存 `Qd` 删除结果和永久路径屏障，并完成任务及释放池/卷占用。原 H/R/V/M 保留为历史；按 inode 的 node/head 查询返回不存在，历史副本仍可按原身份验证。删除不创建新文件 head、空文件、上传收据或批次，不清除磁带数据块。目录边若错误指向删除记录则报损坏，不能把历史 inode 当作活节点。

`node_mutation_deletion` 返回原删除身份、事件版本、父版本、原属性摘要、副本来源和首次完成 owner。Completed 任务必须恰好拥有一个 Qr 或 Qd，任务摘要与该结果匹配；两个结果并存、缺失或损坏均拒绝，不读时修补。不同类型的结果查询返回 None。 真实 FileRequest 上传的已完成批次物化重试只返回 `AlreadyMaterialized`，不重新挂接已退役 inode；同名新上传保留新 inode、head 和内容，原删除重试也不清除它。原请求重试不依赖已清除的尝试头、当前介质就绪状态或后续同名节点；显式创建使用新 inode，旧创建或删除重试不重新挂接旧 inode，也不清除新任务的占用。

删除屏障属于 `@ + namespace + Qb + pool + path-domain-hash` 管理子空间，引用该路径最近一次删除的 NR1 身份和结果摘要。每个旧 inode 的删除记录仍保留；路径屏障不随请求窗口或显式同名重建消失。原生介质导入在预检和物化时、读取已有物化回执前，检查目标及所有祖先的屏障，拒绝把旧介质路径重新引入当前池；其他 namespace/池不受影响。屏障纳入 namespace 清理范围。该规则不冒充磁带内 tombstone 或内容版本。

EE unlink 同时在实际卷根 `.LTFSEE_DATA` 目录写入受保护的 `tapers.fdb.eeInternalDirectory=1` 文本归属标记。目录 UID、时间、其他属性及全部数据对象保持不变；多个别名逐一删除时标记幂等。标记变化纳入完整来源树与结果树摘要，实际 S4 缺少标记时，即使目标已删除且请求指纹一致，也不能生成有效介质证明。节点规范计划采用版本5，不读取旧计划投影。普通 xattr 修改不能设置或删除归属标记；重复、未知、二进制或位于卷根、其他目录及文件上的标记均拒绝。

原生导入允许具有该显式归属的内部数据目录在零 EE 别名时继续存在，实际 Full/增量链重放后，导入清单与 extent cache 仍核对全部物理数据对象，而目录候选继续隐藏内部路径。无标记且无 EE 别名的保留目录仍拒绝导入。归属标记不清理数据块、不重新发布已删除的用户路径，也不解除当前 namespace/池的永久删除屏障。

规范目标必须同时提供目录/文件类型和完整属性，并与 Prepared 输入的类型、属性摘要及原版本条件一致。`NodeMutationPlan::prepare` 的目标参数使用 `Option<(NodeKind, &NodeAttributes)>`，不从空的 symlink 字段猜测节点类型。计划中的目标类型是必需的规范字段，缺失、未知或与物理来源不符均拒绝。

逻辑目录在所选卷缺失时，属性修改按规范祖先链物化目录，并在该卷分配真实的新 UID；逻辑 inode、逻辑属性 UID 及父逻辑版本保持原语义。卷内 UID 耗尽则在暂存前失败。空逻辑目录在该卷没有物理目标时，仍以真实节点 S4 记录删除；只按需物化父目录，不虚构目标节点或消耗目标 UID。已有物理目录非空时仍拒绝 rmdir。缺失的普通文件和链接不能据此变成空文件或目录。提交及目录发布继续重验完整来源和当前逻辑前提；更新只作用于所选卷，多副本统一修改仍待完成。

`daemon::fdb_node_media::execute_node_mutation` 接收原持久任务、当前路由/PR 键和已挂载卷，统一执行准备、规范输入捕获、物理计划准入、暂存、S4 观察及类型化发布。目录内部核对实际扫描来源，现有文件必须使用当前主副本卷；写入前再次核对 owner、原卷占用和介质策略。调用方负责 GuardedTransport 许可、驱动串行化及介质选择；函数不执行接管、移带、格式化或 salvage。错误消耗传入卷，调用方必须重新挂载后恢复，不能继续使用脏视图。

Writing 恢复加载原请求和原计划。实际已提交 S4 无论是否已观察，均发布原尝试而不重复追加；未发生或可安全丢弃的 T1/T2 尾部使用节点专属证明关闭原尝试，再保留原逻辑请求重新准备一次。未知较高 S4、介质错误或许可丢失均保留待恢复记录，不盲目重试。接管后必须以新许可重新挂载，不能给旧缓存视图换键后当成恢复证据。Completed 重试返回唯一原结果，不访问传入设备，也不重新挂接已删除节点；结果中的历史 S4 不代表当前卷头。

`establish_active_media_owner` 在现有多数派 Execution/Serving 许可、同一 GuardedTransport 身份及 PR 核对后，先推进当前 owner，再发现本卷原节点尝试并恢复，最后进入通用文件尾部恢复。已观察和未观察的节点 S4 都先完成原逻辑发布；未发生或不完整的尝试可关闭后按原请求重建。未知 S4 仍拒绝，通用文件恢复不单独接管节点任务。节点恢复后再次核对许可及 PR，错误保留持久恢复状态。原调用接口增加必需的节点字节预算，所有调用方直接更新；运行时使用已配置索引预算，不添加旧接口或无界读取。该路径仍使用内存中的计划和索引，不代表外存 writer 或规模验收。

可写 owner 的 Serving 循环已自动消费持久节点队列，和管理任务共用串行执行线程；只读 owner 保留节点任务。每次最多处理一项，分页游标在等待时仍轮转，旧代次游标失效时从队首重新读取。无任务或暂时不可用时等待一秒再检查，管理队列仍能执行介质接入和策略调整。

文件和链接修改选择当前主副本卷，目录操作在同池已登记介质中自动选择。候选沿用在线、黑名单、密度、节点组、驱动角色及挂载上限约束，新链接在新准入前检查物理文件数；原 Writing 恢复不因后来的新文件限制跳过原卷。读取实际标签后再次核对目标 UUID，再由规范计划验证完整来源。明显不符的物理节点类型、非空 rmdir 来源和 UID 耗尽候选在准备占用前跳过；候选筛选不是介质证明。

新任务没有可用介质或驱动时保持 Queued，不先占池，因此同池后续原生接入仍能推进；已有 Prepared/Writing 保留原记录和占用等待恢复。干净来源先原子取得 Prepared 与 namespace 文件静默条件，再执行通用清理，避免取消已接受的文件预约；已有 Prepared 不重复要求其他池静默。需要恢复的来源先完成原协议恢复，随后重新检查原逻辑条件。原条件失效时保存首次 Rejected 原因；Writing 的物理或权威错误保留原尝试、停止当前 owner，并用重新挂载的实际介质恢复。成功后保留已发布的准确卷头和挂载记录，结束卷对象不额外追加卸载 checkpoint。

节点 HTTP 前缀为 `/v1/fdb/nodes/SERIAL`，所有请求必须携带唯一的 `x-tapers-catalog-namespace`，不接受查询参数：

- `POST prepare`：只做逻辑预检。规范 JSON 固定 `version:1`、`pool`、`operation`、`path`、`expected_version`、`time` 和类型化 `mutation`。创建的 expected_version 为 null，修改/删除为原正整数版本；UUID 必须使用规范形式。返回规范 `request` 与 fingerprint。此步不登记任务、不占用池、不选带。
- `POST requests`：正文直接使用准备响应中的完整规范 request；排队返回202，已知终态返回200。先持久保存原 request，再提交；响应丢失时查询或原样重发，禁止重新生成时间、UUID 或当前路径前提来猜测结果。
- `GET requests/EPOCH/OPERATION`：返回200及 queued/prepared/writing/completed/cancelled/rejected 状态，尚无记录返回404。点查不要求当前 owner，不读取介质或原请求对象。任务、索引和保留的结果在同一读版本校验；completed 的 result 固定原 node/version/parent_version/deleted，路径后来修改、删除或重建不会改写原结果。
- `DELETE requests/EPOCH/OPERATION`：显式取消 Queued/Prepared；Writing 必须按原尝试恢复，不能普通取消。已知终态保持首次结果及原因。写入、准备和取消要求 writer、当前 owner 与有效 Serving；非 owner 只向配置节点转发一次，只读入口返回405。

规范请求正文限制为 8 MiB，且不超过配置的索引字节预算；这与磁盘索引容量分开限制，避免按大磁盘预算无界缓冲 JSON。物理计划、S4 证明与条码不作为此接口输入。文件/链接使用原主副本卷，目录由 owner 自动选同池合格介质。

默认客户端使用 `NodeCondition` 捕获原父目录完整 FDB 版本，以及修改/删除目标的完整 FDB 版本；创建时 node 为 None。拒绝 Raft/LTFS 身份、跨 namespace、nil UUID、零或不可递增版本。纯准备应答还须逐项匹配原 parent/inode/version，不能把同路径、同数字版本的新节点当成原目标。创建 inode 必须由 namespace、请求代次和原 operation 按当前协议派生，单独重算请求指纹不能替换创建身份。

`NodeJournal::create(library, pool, path, condition, change, directory)` 只在本地持久保存原 UUID、规范时间、变更与条件，不发 RPC。目录 0700、必需 `request.json` 为 0600；独占租约和目录描述符随句柄存活。将同一句柄传入 `prepare_node_journal` 和 `run_node_journal`，目录移动或旧路径放入另一合法请求后仍只继续原操作；头被替换或修改则在 RPC 前拒绝。`prepare_node_mutation(library, pool, path, condition, change, directory)` 共用此实现，首次准备 RPC 前已经落盘。失败后保留目录，使用 run 继续，不重新生成 operation、时间或目标。

节点本地必需头采用 envelope version=1，包含 original、可空 prepared 和 domain SHA-256 摘要。Original 明确保存原父目录和可选目标完整身份；缺头、缺字段、截断、重复键、超预算、非普通文件、未知版本或摘要不符均失败，不降级为新 Draft，不做网络修复。准备结果原子替换并 fsync 后才可登记任务。原型单 JSON 请求文件和旧 `PreparedNodeMutation::save_new/load` 接口已删除，没有兼容 reader。

`run_node_journal` 先查询原身份，再按保存的规范请求登记或重试，返回原 completed/rejected/cancelled 终态。完成并不依赖当前路径、owner 或当前节点属性。经完整验证的终态以 `receipt.json` 持久保存，绑定必需请求头摘要，并以独立 domain 校验全部字段；临时文件 fsync 后 NOREPLACE 发布，已有终态不可覆盖。重复调用和新进程可在无服务端时返回原终态；坏本地回执不回退网络。`run_node_mutation_saved` 只将 completed 当作成功，rejected/cancelled 返回原拒绝。`cancel_node_mutation_saved` 不重新准备 Draft，也不撤销已完成任务；已知终态直接返回首次结果。`node_mutation_status_saved` 只读查询，优先本地终态；没有完整准备结果或尚未登记时返回 None，必需头损坏仍报错。`node_mutation_status(&prepared)` 保留为原身份在线诊断。

CLI 沿用文件系统变更组 `fdb fs`，请求位置现在是目录：

```sh
ltfsctl -e HOST:PORT --library SERIAL --pool POOL_UUID fdb fs prepare /dir \
  --namespace NAMESPACE_UUID --request mkdir-request --change '{"kind":"create-directory"}'
ltfsctl -e HOST:PORT fdb fs run mkdir-request
ltfsctl -e HOST:PORT fdb fs status mkdir-request
ltfsctl -e HOST:PORT fdb fs cancel mkdir-request
```

`prepare` 先查询并捕获当前父目录和目标完整身份，再持久保存原意图并只读预检，不登记任务或写带；已有请求目录不覆盖。修改/删除给出 `--expected-version` 原正整数版本，创建时省略；数字条件须与本次捕获的目标一致，完整 inode 及父目录条件进入保存的原请求。`--change` 使用节点 HTTP 的七种逻辑 mutation。`run/status/cancel` 的库、池和 namespace 均来自原目录，拒绝额外 `--library/--pool` 覆盖。默认构建无需 FDB SDK；文件系统挂载的节点修改使用同一条件与日志，rename 尚待实现。

`GET /v1/fdb/attributes/SERIAL?pool=UUID&path=BASE64URL` 提供显式完整属性查询。默认 SDK 使用 `node_attributes(library, pool, path)`，CLI 使用 `ltfsctl -e HOST:PORT --library SERIAL --pool POOL_UUID fdb fs attributes /dir`。响应包含库、namespace、池、路径、inode/version、类型、原属性摘要、readonly、五个原时间字段、xattr 文本/编码标志及逻辑链接目标。原有未知时间字符串保留原值；原属性摘要保留规范记录身份，客户端核对响应作用域及字段。EE 数据别名按普通文件返回，逻辑链接目标为 null；原生链接返回保存的目标，包括内容类似 `.LTFSEE_DATA` 的用户目标。

属性查询在同一 FDB 读版本捕获节点、版本与不可变属性引用，再核验并读取该引用。规范属性读取预算为8 MiB，编码 JSON 响应上限64 MiB；超出预算或属性损坏/缺失时返回错误，路径不存在返回404。大属性在加载前检查长度；查询可以在只读入口、无当前 owner 时使用，读取已提交状态。文件操作发生在途 S4 时，查询仍返回已发布的原属性，待原尝试恢复并发布后才显示新版本。属性查询与原有轻量 stat/目录分页分别按需使用。

FUSE 共用的客户端文件身份已区分 Raft、LTFS 与 FDB，FDB 身份包含 namespace、inode 和逻辑版本，原生 LTFS 身份包含卷 UUID 与 fileuid；无执行轮次的查询明确保留未知值。共用缓存已补上 FDB 身份校验，即使内容摘要相同，也拒绝跨 namespace、inode 或逻辑版本复用，并检查首次下载期间的身份变化。只读桥接已提供显式入口：本地 inode 按 FDB namespace/inode 绑定，目录列举保留父子身份，旧读句柄的内容与属性保持同一快照。同名重建的新 inode 可独立打开，原 inode 的版本变化仍须释放旧引用后刷新。普通文件写入、目录/链接/删除/属性修改及原日志恢复已接线，rename 尚待实现。共用缓存层在上传拒绝、fsync 失败、未 flush 或仅暂存时保留本地字节，释放句柄和卸载均不再递归删除未确认数据；上传前先保留 pending 引用。确认归档或正常只读缓存仍会清理。清理经原目录描述符并核对文件 inode，只移除自己仍为空的原目录，路径被替换或出现外来文件时不递归删除。保留路径会写入警告日志，可能继续占用磁盘；FDB 文件写入还通过独立持久化日志提供跨重启原操作恢复；未曾上传的本地缓存保留不等同于已有完整可重放日志。

多副本管理和 FUSE rename 已接入同一持久改名入口。

仍需逐项完成：FUSE rename、非空池及介质生命周期任务、空白介质初始化、任务历史回收、完整S3/VIP接线、持续备份/恢复、FDB唯一权威启动和故障恢复、百万/亿级端到端、writer外存索引和属性对象GC。原生初始化、Active 池创建、重命名与空池删除已提供入口，后续管理须直接使用首版 FDB 模型。真实磁带的写入/恢复验收与纯软件结果分开记录，不用SimLibrary或Holo结果代替物理验收。


## 文件系统写入与验收顺序

`tape-fuse` 的 Backend 以不可克隆的共享 `UploadState` 持有原目标及日志；上传目标显式区分 `UploadTarget::Native` 与 `UploadTarget::Fdb`。FDB 创建在本地文件建立时固定 namespace 和直接父目录 UUID，覆盖在打开时固定完整原 FileVersion；截断前先核对已有缓存的来源版本。根 inode 保存挂载构造时的真实池根身份，根或父目录删除重建后返回 ESTALE，目录属性版本正常推进不更换目录身份。未确认缓存关闭后重开仍保留原目标，不用当前路径重新选择覆盖条件。

返回类型区分原生暂存结果与完整 `CommittedUpload`。FDB 不接受原生布尔确认，不按当前路径的相同摘要确认成功；完整回执还须匹配原条件、路径及本次字节，成功后保留真实 FDB 版本、LTFS 代数、条码和属性，并将下一次写入条件推进到已确认版本。显式指定 `--fdb-journal-dir` 时启用文件内容与节点修改及启动恢复；省略时继续强制只读。

按用户 2026-11-02 的要求，先补齐既定范围内所有依赖 FDB 的功能并完成软件验收，然后才分阶段上真实带库；不提前以 CLI 小文件写入启动物理验收。剩余工作按以下依赖顺序推进，每项以实际调用链和测试关闭，不能把底层 API 已有实现等同于对外功能完成：

1. 文件系统持久化上传与原操作恢复，以及创建、覆盖、flush/fsync 的完整确认（已通过本地软件与内核验收）。
2. 文件系统目录、属性、链接、删除的 FDB 接线与原身份条件（已实现）；rename 的原身份预检、持久占用、多卷 owner 执行和原子发布已实现，HTTP 提交已接入；SDK 与 CLI 原日志已接通，FUSE rename 接线仍待完成。
3. S3 写入、覆盖、删除及分页规则；FDB 模式的统一 VIP 路由已接通。
4. 介质初始化、分配/释放、维护与回收等既定生命周期功能在 FDB 模式下的闭环。
5. Active 历史副本与任务回收、备份恢复及对应运维入口。
6. 完整软件回归、本地 FDB/模拟磁带故障恢复与内核 FUSE 验证，整理逐项验收记录后进入物理阶段。

物理阶段再按目录与读取、正常读写、失败恢复、owner 切换、生命周期与规模验证推进。此前本地真实 FDB、模拟磁带、内核 FUSE 和 Holo 的结果分别记录，均不代替物理带库结果。


### FDB 挂载的节点修改

可写挂载支持 mkdir/rmdir、unlink、symlink、只读标记与访问/修改时间、用户 xattr 创建/替换/删除。节点修改前先完成本挂载同 inode 的未确认内容上传；从原 inode 或共享缓存的来源版本捕获完整条件，再读取并核对原父目录身份。父目录版本可正常推进，同名重建不能替换原父 inode。旧读句柄的版本不会被当前路径查询悄悄替换。

节点与文件请求共用独占的 0700 日志根。节点目录使用 `node-UUID`，未发布草稿使用 `.fdb-node-UUID`，文件使用 UUID 或 `.fdb-upload-UUID`；按明确名称选择各自 codec，不探测或降级兼容。启动先流式核对全部记录的 scope、必需头与本地回执，再按各自原操作恢复，保持内存与句柄使用有界。历史终态继续保留，回收尚待完成。

节点操作跨 RPC 持有 `NodeJournal`。日志位置在创建前固定，发布后 fsync 失败也不会生成新请求。结果未知时保留原请求并暂停后续命名空间操作；通过根目录 `user.ltfs.sync` 显式继续原操作，或卸载后从持久日志恢复。仍不满足原版本的草稿保留并拒绝恢复，不按当前路径重新规划。已确认拒绝/取消返回失败；成功后删除仅移除原 inode 的路径绑定，旧读句柄保留原内容。创建/属性结果刷新必须严格匹配原回执的完整 inode/version；当前同名新对象不能提供旧操作的属性，无法取得对应快照时返回 ESTALE。rename 使用下述[持久改名入口](#fuse-rename-持久改名)，保留双路径原条件、逐卷计划和原子发布。


### rename 原请求预检

Catalog 提供 `NodeRenameRevision`、`NodeRenameSpec`、`NodeRenameBaseline` 与不可变 `NodeRenameRequest`，并提供 `prepare_node_rename_baseline`、`prepare_node_rename_request`、`check_node_rename_request`、`upload_node_rename_request`、`load_node_rename_request`。这是 rename 的准备组件；owner 与独立 HTTP 入口已能执行持久任务，SDK 与 CLI 提交见后文，FUSE rename 已接入同一持久改名入口。

调用方明确提供源与两端父目录的 namespace/inode/version，目标为原完整版本或原缺失条件；`no_replace` 不接受已有目标。源、父目录和替换目标须有可递增版本，拒绝根、相同路径、祖先关系、跨 namespace、无效路径及重复冲突身份。同父目录的两份条件必须一致。预检在同一 FDB 读版本核对当前路由/owner、池与物理带库归属、原两端父目录、源和目标完整身份、路径与 inode 占用、替换类型，以及目标目录无已提交或待写子项；同名、同数字版本的新节点不能替换原条件。非空源目录可以进入预检，不在此阶段枚举或锁住完整子树。

规范请求保存固定 operation、原管理代次、时间和两端条件；基线摘要覆盖两端父属性、原 source/target 节点、文件 head/copy/placement、属性摘要和 S3 元数据。指纹与基线分别使用独立 domain，执行 owner 不进入原请求。接任后的复核使用原代次、原条件与原指纹，不重造请求。请求采用严格规范 JSON version=1、8 MiB 字节预算，拒绝未知/缺失/重复字段、非规范 UUID/时间、截断与旧节点请求格式。原对象复用有界不可变分块存储，库与 namespace 作用域必须匹配；保存的原请求在当前路径改变后仍可读取，但其旧前提不再成立时复核失败。

prepare/check 均为纯读，不登记执行任务、不取得池占用、不移动目录边、不进行机械操作或写带。保存对象也不授权介质操作。后续准入必须在同一事务重新复核并取得持久占用，再保存涉及各卷的原物理计划与 S4 恢复状态，最终原子发布两端目录、替换退役与旧路径屏障。不能把预检完成表述为已支持 rename。


### rename 持久任务与池占用

`queue_node_rename_request`、`node_rename_task_request`、`prepare_node_rename_task`、`cancel_node_rename_task`、`reject_node_rename_task` 使用同一节点任务账本、排序队列、保留窗口与池占用。任务记录为严格版本6，kind 明确区分 `single-node` 和 `rename`；池占用记录为版本2，并绑定该 kind。首版直接使用当前格式，不读取旧任务或旧占用格式。

登记与重复登记绑定原 operation、代次、原请求对象和指纹；同身份的另一种协议或另一份内容均拒绝。Queued 不持占用；Prepared 在一个事务中核对全部原记录、当前 owner、封口对象、原两端条件与既有文件占用，然后同时保存任务和池占用，所有验证与编码先于首次修改。rename 与普通节点、文件上传共用现有互斥点，不使用旁路占用。读路径仍返回已提交状态，其他池的独立节点操作不被该池占用阻塞。

接任和重复准备读取原请求，不更换准备 owner/时间或原身份。未写任务可原子取消或拒绝，保留首次终态；此后原路径或父目录改变，原身份重试仍返回该终态，不释放后来其他任务的占用。调用错误协议的加载/取消接口拒绝处理。缺失、损坏或不一致的任务、队列和占用均失败，不自动补写。普通 owner 队列按明确 kind 分派并自动执行 rename。暂时缺少合格介质或驱动器的任务保留原身份，公平游标继续前进；已知文件源和替换目标的原卷不可用时不取得 Prepared hold。

Prepared 取得逻辑互斥；全部原始逐卷意图和共享占用验证通过后，可整组原子进入 Writing，协议及原记录恢复接口见后文。原 S4 接纳和最终原子发布见后文；owner 自动执行可以完成 Completed，HTTP 提交见后文；SDK 与 CLI 提交见后文，FUSE rename 提交仍待接线。


### 改名后的原数据与当前元数据节点

数据读取固定原卷、原索引构造和 data fileuid。Active 副本的读取计划按该 fileuid 从原 extent 缓存取数据，仍核对池路径作用域、UID、长度，并拒绝普通 Active 数据副本指向链接；正文摘要验证继续沿用原读取链路。当前逻辑名称既可已不在原索引内，也可在原索引里属于另一条被替换的旧文件，均不能改变该数据来源。导入副本仍按保存的原 logical UID 与物理数据路径验证原索引，不改写历史来源。

属性修改按当前逻辑路径定位当前物理节点，再核对原逻辑/数据 UID、卷和代数关系、类型、长度及内容摘要。导入普通文件和原生链接保存的 `MediaProvenance.physical_path` 是原数据构造中的来源路径，不能强制等于当前节点名称；EE 别名仍必须核对其固定内部数据对象路径及完整来源证明，链接目标也不得在普通属性修改中变化。`MediaProvenance.version` 保留真实媒体版本对，不是格式版本，不随逻辑改名伪造新值。

这些是完整 rename 的读取与属性前置组件；不移动目录边、不登记 rename 物理尝试，不授予写带权限。R1 副本格式与现有 on-medium 数据保持不变，实际多卷原计划、S4 恢复、原子发布及 rename 对外执行入口仍待完成。


### rename Prepared 规范输入

`capture_prepared_node_rename_inputs` 返回原任务、原请求、原 source/target 及两端从池虚拟根到父目录的完整规范祖先链。捕获只接受持有原池占用的 Prepared rename；Queued、终态、错误 kind 或旧 owner 均拒绝。接任 owner 可读取同一原操作，原准备 owner、操作 UUID 与时间不改变。

每条祖先链核对规范路径、C/N 边、inode、目录版本、类型和属性摘要；两端共享的祖先仍须匹配同一规范节点。外部属性先按声明长度计入总预算，再只读取捕获的不可变引用，最后在同一事务重新核对两条链、原节点、原请求和池占用。调用者可用 `check_prepared_node_rename_inputs` 再次复核；篡改祖先、取消或 owner 失效均失败，不自动重建请求。

此输入本身不包含源目录完整子树或各卷投影，也不授予写带许可。完整 owner 执行另外要求后续清单、逐卷计划、原 S4 和原子命名空间发布屏障；HTTP 提交见后文；SDK 和 CLI 原日志见后文，FUSE 接线仍待完成。


### rename 有界子树捕获

`begin_node_rename_subtree_capture` 接受已复核的 Prepared 输入并创建私有游标；`next_node_rename_subtree_page` 从原 source 根开始，按各目录规范子键顺序深度优先读取，每页最多1000条，并受显式字节预算约束。该遍历用于 rename 清单，S3 ListObjectsV2 仍使用独立的全局字典序查询。

每项包含原路径、目标路径和当前规范目录/文件快照；文件绑定原 head、当前主副本、数据 UID、源索引扫描、卷放置及对象元数据。内联属性计入页预算，外部属性保留已封口的不可变引用，不加载整棵树或全部属性正文。原源目录中的已删除历史条目不进入清单。目录残留预约或 Active claim、孤立 head/副本、错边、缺节点和目录环均失败，不把损坏当成空目录。

每项使用短事务核对原 owner、任务、封口对象和池占用，页结束再复核两端规范输入。预算不足或任何核对失败都不推进调用者游标。摘要绑定原请求、两条祖先链及逐项长度与规范内容，与页大小无关；新 owner 须重新捕获同一原请求，旧游标不能继承新的 owner 身份。取消后即使游标已经完成也拒绝继续读取。

游标仅保存在当前进程中；完成计数和摘要不是持久清单封口或写带许可。跨进程可信清单见下节；按卷投影、配方封存及介质 S4 组件见后文；全局介质准入、最终原子发布与 rename 对外入口仍待完成。


### rename 可信持久清单

`resume_node_rename_manifest_capture` 创建或恢复服务端捕获器；`next_node_rename_manifest_page` 从规范子树生成下一页，保存封口的不可变页对象，再在同一事务中核对原 Prepared 占用并登记页索引和进度。调用者不能提交页内容、对象引用或摘要来创建规范关联。通用对象上传保持 inert，上传成功并不构成规范清单证据。

清单记录使用 namespace 所有的 `@/Q/M` 子空间及严格版本1，绑定原任务对象、请求指纹、准备 owner/时间、首次捕获 owner、初始规范输入摘要、逐页摘要链和完成计数。页最多8MiB，保存原请求页条数及实际条数；末页与 complete 状态原子登记，不修改 namespace 的 C/N/H/R。首版直接使用此格式，无旧格式 reader。

原页提交结果未知时，只接受完全相同的已提交后态；错误保留原游标。重开进程或接任 owner 时，按原页大小重放规范树并核对所有已保存页及完整分块，沿用原操作、原页和首次捕获 owner，不生成替代操作。恢复读取随已保存历史量增长；这是进程重开时的开销，正常推进保留有界游标。缺头但存在页、缺页、额外页、坏对象、原条件变化或错误 owner 均拒绝，不修补损坏记录。

`finish_node_rename_manifest_capture` 只为完成的清单返回不透明引用。`open_node_rename_manifest_reader` 与 `next_node_rename_manifest_entries` 按序验证页对象的全部字节、长度边界、页链及最终总头；失败不推进 reader，取消后也拒绝继续。只有读完所有页才标记 reader 完成；部分返回节点不能作为已验证完整清单使用。记录与对象保留到后续显式生命周期/回收流程处理。

清单封口和 reader 完成都不授予介质操作权限。按卷规范投影和未武装原尝试捕获见后文；原 S4 接纳见后文；完整物理恢复、原子发布、旧路径屏障和 owner 自动执行见后文；SDK 和 CLI 原日志见后文，FUSE rename 提交仍待接线。


### rename 清单的类型化读取

`NodeRenameSubtreeEntry::decode` 严格读取现有规范 JSON1，逐字段恢复原目录/文件身份、head、R1 主副本、原数据 UID/源扫描、介质来源、卷放置、对象元数据及属性引用。使用当前字段和现有 R1/属性约束，不增加旧格式兼容分支，也不按当前路径查询或替换原记录。

解码拒绝截断、重复/未知/缺失字段、非规范 UUID、版本或摘要不符、错误副本来源、非法路径和属性引用，以及超过8MiB的输入。外部属性只恢复不可变引用，不读取全部正文。介质来源的 version 仍是实际 round/seq，与清单 schema version 无关。

公开 reader 使用 `next_node_rename_manifest_entries` 返回类型化节点，并额外核对原 from/to 路径映射、首项原 source 身份和文件 namespace/pool/library。整个页先在私有 reader 副本上读完并解码，最后再核对 owner 和原清单头；任一失败均不推进调用者 reader。原公开的原始字节分页接口已删除，原始对象体读取仅供内部解码使用。完成 typed reader 仍不是写带许可，按卷计划与原子发布另有独立入口。


### rename 按卷外存视图

`build_node_rename_projection_spool` 只从已封口的原清单及完整 typed reader 构建私有 SQLite 视图。接口不接受调用者提供的 SQLite 文件或替代页对象；源路径、inode、目标路径唯一，父目录必须先存在且是目录。构建完成核对全部计数和规范编码字节数，并进行数据库检查、设为 query-only，最后重新核对 owner、原任务和清单头。失败不会发布部分视图或修改 FDB 原任务。

视图提供 `volumes_page`、`volume_files_page`、`directories_page`、`directory` 和 `metadata_node`。文件按原卷分组，并以原 metadata UID 定位规范节点；EE 的不同别名可以共享同一 data UID，不能用 data UID 代替别名 UID。UID 索引采用无符号64位大端字节，保留大于有符号整数范围的原 UID。每行保留原规范节点编码和摘要，查询会重新校验编码、路径、身份、卷和 UID。

分页最多1000项，并受输出字节预算约束。目录及同卷文件按完整 UTF-8 路径字节序排序；游标绑定原请求、清单摘要、查询种类及卷，拒绝跨视图或查询使用。预算不足、重复身份/metadata UID、缺父目录、错误归属或损坏行均失败，写入事务回滚后不留下部分计数或分组。

SQLite 使用空文件名创建每连接私有的临时数据库，连接关闭后自动清理；它可以将页写入临时文件，持久证据仍来自 FDB 原清单，重启后重建此视图即可。[SQLite 官方临时数据库说明](https://www.sqlite.org/inmemorydb.html) 内部设置 FILE 临时存储、页缓存目标2MiB和禁用 mmap，并拒绝强制纯内存临时库的 SQLite 构建。调用者必须给出至少64KiB的数据库页预算；此预算约束数据库页，不包括短期 rollback journal 和有界输入/输出页缓冲。

清单解码与规范池内路径统一使用2048字节上限，边界按UTF-8字节计算。池 UUID 仅用于 FDB 虚拟根路由，不写成物理 LTFS 目录分量。此视图保存规范输入，不授予写带权限；按卷投影、原多卷尝试与 S4 接口见后文，原子发布与旧路径屏障见后文。

### rename 卷内有效 UID 引用防护

`build_node_rename_volume_guard` 从原 Prepared 任务、已封口清单和私有按卷视图捕获指定卷的当前主副本引用。卷必须属于同一 namespace/pool/library，并通过放置、assignment、池索引、源索引及 ready scan 校验；每页及结束时重新核对 owner、原任务、清单和卷绑定。每页最多32个卷索引记录，结果保存在带数据库页预算的私有临时 SQLite 中，不加载整个卷的引用集合到内存。

守卫按原 FID/head/copy 将当前文件分为 Source、Target 和 Other，检查原源文件及替换目标在该卷的完整覆盖。查询 `metadata(uid)` 返回当前 metadata UID 的原文件身份及角色；`data(uid)` 返回共享该数据 UID 的三类引用数量。EE 别名的 metadata UID 与 data UID 分开，原生符号链接不产生数据对象引用。相同旧索引记录不能重复计数；metadata UID 冲突、缺当前 head/R/V、错误版本、卷或扫描、损坏行及预算不足均失败，不修复 FDB 记录。

遍历使用现有卷索引及已发布但尚未物化的 Active overlay。遇到历史副本索引时仍核对当前 head 选中的主副本，防止残留旧索引掩盖当前索引缺失。V 值末尾32字节保存发布来源的指纹，不作为文件内容摘要比较。该遍历以现有卷索引完整性为前提，不能证明整个命名空间不存在完全丢失的索引记录；后续物理计划还须逐项核对实际完整索引。

结果封口后只读，每行校验原编码和摘要，数据引用数量还与原 metadata 行重新核对。结果摘要绑定原请求、规范清单、源索引/扫描及池和 assignment 修订，不包含接任 owner，因此同一事实在 owner 接任后保持同一摘要。取消或失效任务不能再构建守卫。守卫只保存引用事实，不授予介质操作或数据回收权限；完整参与卷封口、按卷过滤投影及介质证明组件见后文；原多卷尝试与 S4 账本见后文；原子发布见后文，rename HTTP 提交见后文；SDK 和 CLI 原日志见后文，FUSE 接线仍待完成。

### rename 完整物理来源核对

`NodeRenameVolumeGuard::physical_source` 将恢复后的完整 `LtfsIndex` 与原规范 baseline、清单视图及该卷守卫核对，返回私有只读 `NodeRenamePhysicalSource`。输入树保持原样；接口不接收设备句柄，也不联网。卷、代数、位置及增量标志必须匹配原源索引，拒绝未重放的 delta、未知 XML 元素、锁定卷、未关闭文件、重复路径/UID、错误类型和非法属性。目录遍历使用显式栈及单一路径缓冲，不递归复制子树。

全部物理节点进入带页预算的私有临时 SQLite，UID 采用无符号64位大端字节。单个节点编码及属性受8MiB预算限制，每行保存路径和原节点编码的摘要。封口前检查数据库、覆盖计数和总页数；封口后按 UID 查询文件时重新检查摘要、编码和 UID。结果摘要绑定原卷守卫、最高 UID 及按完整路径字节序排列的行，物理兄弟节点排列顺序不改变摘要。

原 Source 和 Target metadata UID 必须出现在各自原路径，Other metadata UID 不能落入待移动或覆盖的范围。全部当前 metadata UID 和 data UID 都必须在实际完整索引中出现。规范源文件及替换目标还须通过与既有节点修改共用的副本校验，保持数据 UID、长度、内容摘要、链接类型及原 EE 数据路径；EE 别名的数据节点通过 SQLite UID 索引定位，避免为每个别名线性搜索整个数据目录。仍被范围外引用的数据文件禁止随子树移动或被替换，拥有归属标记的根 EE 内部数据目录也禁止移动或覆盖。

该结果是物理来源事实，不是写带许可，也不能代替原索引构造的原始字节摘要证明。实际执行仍须核对原任务、owner、全局参与卷、原物理计划及 PR。按卷过滤投影及介质证明组件见后文；整组 Writing、全局多卷执行和原子发布见后文，对外提交入口仍待接线。

### EE 别名移动后的属性修改

目录移动到不同深度时，EE 别名的物理相对目标会变化，子节点的原规范属性仍可能保存旧深度的表示。节点属性计划分别核对链接类型和数据身份：只有同为 EE 别名、且规范目标归约后的数据路径与原物理数据对象完全一致时，才允许相对深度不同。计划保留实际索引中的相对目标、metadata UID 和原数据对象，属性操作的逻辑结果仍保留原逻辑 UID。显式修改成功后，逻辑属性结果保存该卷已验证的相对表示。

原始数据副本的路径、UID、长度、内容摘要和完整数据条目证明继续校验。不同数据对象、绝对路径、空路径分量和 EE/原生链接类型不符均拒绝。原生符号链接始终逐字匹配目标；即使其字面值包含 `.LTFSEE_DATA/`，显式原生标记仍阻止按 EE 目标处理。此规则不重写整个目录的子节点，也不执行后台属性修复。

### rename 按卷规范物理投影

`build_node_rename_volume_projection` 将原清单、私有规范视图与已核对的完整物理来源组合为 `NodeRenameVolumeProjection`。构建时重新捕获原强身份对应的 Prepared 输入，核对原祖先摘要、当前 owner、任务、清单头及卷绑定；重建的当前 UID 守卫也必须与原物理来源一致。读取外部属性时逐个验证不可变对象，每页最多32项，最终再次核对原输入。取消、接任前的旧 owner、来源改变和无关卷均拒绝。无文件的源目录可以形成候选锚卷投影，最终参与卷仍须由后续全局计划确定。

投影使用带页预算的私有 SQLite。它保留旧源范围和目标范围以外的节点；源范围内只带入原清单的当前文件及规范目录，已删除或被替换的物理旧条目不随目录移动。各卷均包含清单中的空目录，文件仅出现在原主副本所在卷；替换目标所在卷删除原目标 metadata。两个原父目录更新时间，相同父目录只产生同一组时间；源根更新 change time。虚拟池根保留物理根的 UID、固有属性和私有 xattrs，其他目录使用规范属性及各卷自己的 UID。原物理目录 UID 可保留，缺失目录从该卷最高 UID 后依次分配，耗尽时拒绝。

EE 别名按新父目录深度生成相对目标，保持原数据路径、UID、长度和 extent；原生符号链接保持原字面值。涉及 EE 别名的移动或删除时，为根数据目录保留或补充归属标记。封口前重新核对范围外当前 metadata 及共享数据、全部行编码/摘要、父目录关系、数据库完整性和页预算，再设为 query-only。原物理来源的全部计数和摘要也会重算，防止缺失缓存行导致范围外节点被遗漏。原索引树、规范视图和 FDB 命名空间均不修改。

`stats` 提供来源、前后摘要、最高 UID、节点计数及数据库页数；`file(uid)` 和 `directory(path)` 只读查询已验证的期望节点。摘要不包含接任 owner，同一原事实在接任后保持相同投影。结果本身是可重建的临时视图，原配方封存及介质 S4 组件见后文；它不授予写带权限。全局参与卷封口见后文；介质崩溃恢复与最终原子发布见后文，外部 rename 提交入口仍待完成。

### rename 原按卷计划封存

`save_node_rename_volume_plan` 将已验证投影的原始配方封存为 `NodeRenameVolumePlan`。描述符绑定原请求及任务、规范清单和祖先摘要、准备 owner/时间、卷原始索引构造与扫描、放置及池和 assignment 修订、UID 守卫摘要、前后语义摘要、前后最高 UID 及节点计数。原清单和不可变属性对象已经持久化；描述符与它们共同确定投影，描述符本身不包含另一份完整卷索引。执行前须重建并匹配同一摘要。

记录位于原可信清单的 namespace/任务域，使用独立按卷分支。调用者只能提交不透明的已验证投影；保存前重算整个缓存的行、计数及摘要，检查原 Prepared hold、owner、清单头与卷绑定，然后才关联记录。相同事实重复保存会返回首次记录；接任保留原创建者和同一计划指纹。不同事实、作用域或损坏记录均拒绝，不覆盖原记录。记录使用严格当前 JSON1、规范编码和独立校验和，拒绝未知/重复字段、错误 UUID、引用、计数和非规范字节，不接受旧格式。

`node_rename_volume_plan` 只读取已关联的原记录，验证原任务、清单、owner 和卷身份。尚未生成且未被引用的记录返回 `None`；已被原参与页引用后缺失、或存在损坏记录时返回错误，均不重建或修复。普通对象上传或计划指纹不授予写带权限。原配方保存在 Prepared 阶段；持有原任务时可在 Prepared 或 Writing 读取同一原记录；全局参与卷封口见后文；未武装原尝试捕获见后文；整组 Writing 元数据准入与原记录读取见后文；原 S4 接纳见后文；完整介质执行与最终原子发布见后文，rename HTTP 提交见后文；SDK 和 CLI 原日志见后文，FUSE 接线仍待完成。


### rename 介质改写与原尝试 S4 组件

`NodeRenameMediaPlan::new` 将已封存的按卷配方、原物理尝试 UUID 和原 owner 绑定为不可变物理指纹。配方指纹仍独立于接任 owner；物理标记则区分每次原始尝试。构造此数据对象不登记尝试、不取得介质许可；设备 actor 仍须先完成持久准入、原全局参与卷校验及当前策略和 PR 校验。

`LtfsVolume::stage_node_rename` 要求干净可写、PR 已确认的原始索引构造，核对原始字节身份、完整来源语义摘要以及重建投影与原配方的一致性。临时 SQLite 增加由路径派生的组件顺序键，以显式目录栈重建工作树；排序会将 `/a/child` 与 `/a` 保持相邻，不受 `/a-` 的完整路径字节序影响。语义摘要继续使用完整路径字节序。构建前释放旧工作根，保持已有两棵树的内存规模，已发布树保持原样；重建过程中读取失败会冻结卷，禁止提交部分工作树。提交采用完整索引。

根属性 `tapers.fdb.nodeRename` 保存 `1:` 加原物理指纹。用户属性修改不能设置或删除该键；旧标记若重复、base64 或格式错误则拒绝来源。来源摘要保留旧标记，结果摘要排除本次标记并独立验证其精确值，避免自引用。

`node_rename_evidence` 只通过当前 PR 下干净的实际已提交构造生成不透明 `NodeRenameS4Evidence`。它要求同卷、原代数加一、DP 位置前进、完整非增量结果、最高 UID 和全部节点计数与摘要一致。整树遍历共用物理来源的节点校验，带真实 SQLite 页预算，核对共享数据及无关节点，不接受仅有同名目标、相同内容或内存树作为提交证明。失败实例不能生成证据；结果未定时，接任者须重新挂载，再以同一原尝试验证实际 S4。

该组件仅在 Sim 磁带上验证。测试覆盖源/目标两卷、原始 UID 和数据 extent 保留、工作树与已发布视图隔离、重新挂载和 PR 接任，以及 locate、opening_fm、index_records、closing_fm、index_partition、barrier、vci 故障；源卷已提交或两卷均提交均不会改变 FDB 命名空间。原全局参与卷封口见下节；未武装原尝试捕获见后文；原 S4 原子接纳见后文；完整介质执行、整组 Writing 和最终命名空间发布见后文；SDK 和 CLI 原日志见后文，FUSE rename 提交仍待完成。


### rename 原全局参与卷封口

`begin_node_rename_participants` 只从原可信清单及其私有规范视图推导参与卷：全部当前源主副本所在卷，加上原替换文件所在卷；同一卷合并一次。无源文件且无替换文件的目录任务必须明确选择一个合格锚卷。每个参与卷都必须已有同一原请求的封存配方，源文件数与规范视图精确一致，并在新增页时重新核对放置、assignment、ready scan 及原活动索引；调用者不能添加其他卷或上传自定义参与页。

捕获器每次最多保存32项，只保留一页源卷和不透明游标。参与卷页位于原可信清单的独立分支，严格绑定原请求、规范清单、页号、前页链和各原配方指纹，使用当前规范 JSON1 及独立记录校验和。相同原页重试保留第一次创建 owner；页链不包含接任 owner。缺少源或目标配方、错误卷、计数、作用域或损坏记录时失败，捕获器不前进，原页不被覆盖。

`finish_node_rename_participants` 要求全部候选已消费，再逐页重新核对页链、顺序、源文件总数、目标/锚卷计数和原配方，最终在原 Prepared hold、当前 owner 和两端原强身份校验下写入封口头。`node_rename_global_plan` 只返回完整验证后的不透明 `NodeRenameGlobalPlan`；尚未封口且没有原尝试状态时，头缺失返回 `None`；已有原尝试状态而头缺失、或头存在而页或原配方缺失、损坏时返回错误，均不修复。全局清单指纹和首次创建者在接任后保持不变。

`resume_node_rename_participants` 重放原页。空目录的锚卷从原首个参与页恢复，因此首个页已提交、封口头尚未写入时中断，也不会按最新库存重新选择锚卷；原首个页缺失则拒绝恢复。非空源卷仍由原不可变规范清单推导。

这是全局参与集合封口，不授予设备 I/O 权限。整组 Writing 元数据准入与原记录接任读取见后文；原 S4 接纳见后文；完整 PR 执行流程与最终命名空间事务见后文；SDK 原日志见后文，CLI 和 FUSE 提交入口仍待接线。验证使用隔离 FDB 与 Sim，仅核对元数据，保持原命名空间和已挂载索引不变。


### rename 未武装原尝试与共享卷占用

`begin_node_rename_attempt_preparation` 将已封口的全局参与卷集合关联到一个原物理准备组，保存原组 UUID、owner、时间、作用域及稳定指纹。当前组指针与原组头在同一事务登记，组指针缺失而历史组仍存在时视为损坏，不能另选 UUID。新组只允许在原组已明确关闭后创建，已关闭 UUID 不能复用；同一组重试返回原记录，不改写原 owner 或时间。

`next_node_rename_attempt_preparation_page` 每页最多32卷。它重新核对原 Prepared hold、全局封口、原逻辑条件、卷计划、当前来源和策略修订，并检查恢复、文件预留及下一代批次。每个原意图绑定原配方、独立逐卷 UUID、owner/时间、物理计划指纹及策略修订。原意图、逐卷指针、共享卷占用、原准备页和组进度在同一事务保存，全部可失败检查及编码先于首次写入。响应丢失时重取同一原页和原意图 UUID，不跳到下一页。完成后组状态为 Ready，但逻辑任务仍为 Prepared。

Q/v 是单节点修改和 rename 共用的卷互斥键，统一使用当前版本及明确类型封装。单节点原尝试主记录 Qa 保持其独立编码；卷占用不再直接等于裸主记录。解析只接受当前封装，拒绝裸记录、未知类型和非规范内容。`pending_node_mutation_attempt` 遇到有效 rename 占用会验证原组、原意图、准备页和逐卷指针并返回 PoolBusy，不能将它解释为单节点计划。Q/v 缺失时仍核对持有的 rename 组；即使逐卷指针也丢失，原准备页的有界 UUID 二分查找仍能识别必须存在的占用，报错且不修复。

`node_rename_prepared_attempt` 和 `node_rename_prepared_volume_intents` 可以在接任后读取原记录，保留原物理 owner。新 owner 不能继续武装旧 owner 的准备组。`close_prepared_node_rename_attempt` 只针对从未进入 Writing 的组，按原准备页逐批核对并释放精确属于它的共享占用和逐卷指针，保存关闭进度及首次关闭 owner/时间，保留原组、意图与准备页。另一卷的有效占用、缺失或损坏记录均使该批失败，不清除其他占用、不改写计划，也不执行设备命令。关闭后可用当前 owner 和新物理 UUID 创建新组，原逻辑操作、时间、规范清单、全局集合及配方保持不变。

取消或拒绝 Prepared rename 会先关闭其未武装组，再在终止事务核对没有开放组并释放逻辑 hold；非法拒绝原因不会提前关闭原组。该数据准备层尚不授予 PR 或介质写入权限，整组 Writing 元数据准入见后文；原 S4 接纳、Completed、最终原子发布与对外执行见后文。

目录快照摘要保持原1MiB流式读取大小，缓冲区改为有界堆分配，避免占用工作线程栈。固定摘要测试在256KiB线程栈上通过；这不改变快照字节、摘要规则或持久化格式。


### rename 整组 Writing 准入与原记录恢复

`admit_node_rename_attempt` 在有界读取事务中验证全部原准备页、页链、原配方、逐卷意图、共享 Q/v 和逐卷指针；最终事务重新核对同一组快照、原池 hold、当前 owner、全局封口以及源、目标和两个父目录的原强身份，再同时保存 Writing 任务、池 hold 和 Writing 组。所有可失败检查和编码先于首次写入。未准备完整、占用缺失或原条件变化时均拒绝，不重建记录；响应丢失后返回同一原组。

Writing 任务的 active_attempt 必须等于原组 UUID，且组状态必须也是 Writing。池占用验证按明确任务 kind 分派；单节点接口拒绝 rename。Prepared 与 Writing 的错误混合状态、缺失原头或组指针均报错。Writing 不能走未写取消、拒绝或 Prepared 关闭路径。

`node_rename_held_manifest`、`node_rename_held_global_plan`、`node_rename_attempt` 和 `node_rename_volume_intents` 从持有的原任务恢复完整原记录，保留原 owner、时间、配方、意图 UUID 和指纹。接任读取不按最新路径、库存或策略重新规划。原 Prepared 专用接口继续只接受 Prepared。

`check_node_rename_write_intent` 逐卷检查当前 owner 必须等于原物理 owner，且原意图、共享占用、当前来源和策略修订完全一致。接任者可以读取旧 Writing 组，但不能把旧计划换成新 owner 后重放。该检查仅验证 FDB 元数据；调用方仍须独立确认实际设备 PR 和原物理来源。

以上元数据准入本身不会移动介质、执行 CDB、提交 LTFS 索引或发布命名空间；owner actor 随后执行原任务的独立物理与发布流程。原 S4 接纳、未发生关闭、新物理尝试及全卷发布屏障见下文；最终原子发布见下文；owner actor 自动执行见后文，SDK 和 CLI 原日志见后文，FUSE 提交仍待接入。验证使用隔离 FDB，真实带库实测按全部 FDB 功能完成后的分阶段计划进行。


### rename 原 S4 原子观察与独立历史

`observe_node_rename_intent` 只接受实际 `LtfsVolume` 在干净、PR 校验通过的卷上构造的不透明 `NodeRenameS4Evidence`。证据必须对应原逐卷意图及完整配方。观察事务核对当前 owner、原 Writing 任务与组、原全局封口、精确共享占用及逐卷指针、卷归属和原就绪扫描，再同时保存原 S4 观察、已观察卷头、明确类型的历史索引和准确文件数。事务首次修改后没有可失败操作，不发布命名空间或解除池、卷占用。

每条观察位于 namespace 的 `QR + volume + generation`，保存完整原配方、原意图、实际索引位置与原始字节摘要、文件数以及首次观察 owner/时间。严格版本1规范编码与校验拒绝未知字段、重复字段、非规范内容和跨任务作用域。Writing 准入前预留最坏编码长度，避免介质改变后才发现历史记录放不下。新观察使用显式历史类型 `NodeRename`，与文件批次、单节点修改和修尾结果分别验证。

`node_rename_observation` 在重放前读取原结果。重复观察保留首次审计字段；接任后可接纳原物理 owner 已提交的 S4，不能因此重放旧计划。后来的 offline、blacklist 或策略修订不遮蔽已提交结果。历史验证使用自包含记录，不要求活动任务、规范清单页或原请求对象块仍存在；观察、历史或文件数缺失、损坏、类型不符时均拒绝，不补写或猜测。

隔离 FDB 与 Sim 验证覆盖源卷先观察、目标卷提交在 index_partition 报错后重新挂载识别原 S4、接任后以新 PR 读取同一原结果、停写策略、首次审计保留、共享占用缺失时观察不推进，以及清除原清单和请求对象块后历史仍可验证。全部参与卷观察后，逻辑任务仍为 Writing，原源/目标路径不变。原尝试未发生关闭见下节；物理重试与最终命名空间事务见后文，HTTP 提交见后文；SDK 和 CLI 原日志见后文，FUSE 接线仍待完成，真实带库验证尚未开始。


### rename 未发生证明与逐卷关闭

按卷配方保存原物理文件总数，以及原卷根 `tapers.abandonedBlocks`、empty-recovery 属性的值和位置。它们与原完整来源摘要一起封存；重复属性、base64、非法 XML 或超出64000字节配方预算均拒绝。首版直接使用必需的新字段，不读取缺少这些字段的旧配方或占用记录。

`LtfsVolume::node_rename_absence_evidence` 只在干净且 PR 校验通过的实际卷上返回不透明 `NodeRenameAbsenceEvidence`。Unchanged 要求仍是原构造、完整原树、原最高 UID 和文件数。Discarded 要求原下一代完整索引、正确回指、确定性修尾标记及准确放弃范围；仅恢复两个已核对属性的原位置后，完整树摘要必须与原来源一致。成功原 S4、未知较高构造或其他树变化均不能解释为未发生。

`discard_node_rename_tail` 是显式 T1/T2 Discard 操作，先验证完整原来源并预留修尾后卷根的编码预算，再追加收尾索引。它保留原数据与放弃范围，不发布命名空间；失败卷保持冻结，须重新挂载判定结果。当前 FDB 权威和执行许可仍由调用方另外核对。

`close_node_rename_intent` 在当前 owner 下用该原证明保存不可变逐卷关闭结果，同时把共享 Q/v 标为已关闭并绑定关闭记录摘要。Discarded 同事务推进明确的 `NodeRenameDiscard` 历史、卷头和不变的原文件数；Unchanged 保留原卷头。原组、意图、逐卷指针、池和卷占用全部保留，原任务仍为 Writing。Writing 准入前同时预留 S4 观察和关闭记录的最坏编码预算。

`node_rename_intent_closure` 返回原结果及首次关闭 owner/时间。重复关闭或接任读取保留首次结果；同一原 owner 也不能重放已关闭意图。关闭标记和关闭记录任一侧缺失、不一致时均报错，不重建或恢复为开放占用。关闭及历史证明自包含原意图与配方，历史验证不要求原清单仍存在。

隔离 FDB 与 Sim 覆盖 T0 原卷关闭、closing_fm 失败后的重新挂载与 Discard、同 owner 重放拒绝、记录/占用丢失拒绝、接任重取首次关闭结果，以及清除原清单后 Discard 历史仍可验证。关闭一个卷不会撤销其他卷结果，也不取消整组 Writing。基于原关闭结果的新物理尝试准入见下节；最终命名空间事务已由全卷封口与原子发布入口提供。owner 自动调度见后文，HTTP 提交见后文；SDK 和 CLI 原日志见后文，FUSE 接线仍待完成，未访问真实硬件。


### rename 原闭卷上的新物理尝试

`admit_node_rename_rebased_intent` 只推进原 Writing 组中的一个已关闭卷。它核对精确的原关闭记录、已关闭共享占用、当前指针、原参与卷、当前来源与策略修订，以及源、目标和两个父目录的原强身份，再原子保存新配方、意图、指针和开放占用。新意图使用当前 owner 和新逐卷 UUID；原全局组 UUID、owner/时间、逻辑操作与时间、规范清单、参与集合和原 `QM2` 配方全部保留。

新配方从原封存清单和当前确定来源重建，保留 Discard 的放弃范围。只读清单、引用守卫及临时投影可在原 Prepared 或 Writing hold 下构建；公开 Prepared 捕获和清单/原配方生产接口仍严格要求 Prepared。恢复过程不重新选择同名节点或参与卷。

原意图声明 Original；重试意图声明 Rebased，并绑定原准备页中的根引用及直接父关闭记录摘要。新配方位于独立 `QM9` 键。当前意图的读、写前检查、S4 接纳和后续关闭都验证这条原关系；历史 S4 和关闭记录仍自包含，不依赖活动父记录。当前 owner 只可执行属于自己的当前物理意图，旧物理上下文保留为恢复事实。

`QI + volume + intent` 保存不可变 nonce 与意图摘要关联，随 namespace 保留。它阻止已使用 UUID 在旧计划行清除后被复用；缺失或不一致时不补写。原关闭记录、当前配方、指针、共享占用或 nonce 缺失均拒绝恢复准入。重复同一已准入 UUID 返回首次意图，其他 UUID 不能覆盖已经推进的指针。

隔离 FDB 与 Sim 验证 T0 和 Discard 来源上的重试、多次 T0 关闭与新 UUID、保留另一卷成功 S4、清除较早计划行后的 UUID 复用拒绝，以及缺父记录/配方/指针/nonce 不修复。本步骤保持原全局配方和命名空间不变；最终命名空间事务见后续原子发布部分。owner 自动调度见后文；SDK 和 CLI 原日志见后文，FUSE rename 提交仍待接入，真实带库验证尚未开始。

### rename 全卷 S4 发布屏障

`seal_node_rename_s4s` 按原准备页核对每个原参与卷的当前物理意图、完整谱系、共享占用、原 S4 观察、历史类型、当前卷头与扫描身份。缺少任一卷的 S4，或卷已关闭但尚未成功重试，均不能封口；不补写或猜测证据。

原组的必需进度版本在首次 S4 接纳、逐卷关闭和新物理尝试时同事务递增；重复请求保持原版本。遍历全过程核对同一组快照，最终常数规模 CAS 再次核对当前 owner、原全局封口和两端强身份，保存全部 S4 的链式摘要并进入 Publishing。变化发生在扫描期间时拒绝封口，调用方须重读同一原组。进度版本、封口摘要和阶段均不改变原组身份。

Publishing 禁止新介质写入、S4 接纳、逐卷关闭和重试，保留全部卷占用、池 hold 与原目录；同一封口可以在响应丢失或 owner 接任后重取。此屏障不执行最终命名空间切换或产生 Completed 回执；两者由后续独立发布接口完成。记录采用首版严格格式，不读取或补齐旧缺字段记录。


### rename 最终原子发布与两路径回执

全卷封口后，`release_node_rename_publication_page` 每次只核对并释放一个原准备页中的卷占用，最多 32 卷，同时推进原组的释放页数与 S4 链。缺失、外来或被恢复的旧占用均拒绝，不补写、不误清；最后一页的摘要必须等于原全卷封口。池 hold 在整个释放过程中保持，已释放卷在通用节点入口仍被判为忙。

`publish_node_rename_attempt` 只在全部原参与卷完成释放后运行。它先检查原终态结果，再读取原 held 输入并封存更新后的属性对象；最终事务重新核对当前 owner、原组封口、释放进度、源、目标、两个父目录和规范祖先链。它原子切换两个目录边、移动源节点并推进其逻辑版本、推进两个父目录版本与 mtime/ctime、独立删除原替换目标，同时保存严格两路径回执、Completed 任务并清除队列与池 hold。同父目录只推进一次；目录子树不逐节点重写。

源 FID 保留。文件只新增元数据副本与 head：原 content origin、扫描、数据代数、数据 UID、介质版本及物理路径全部保留，历史副本不删除。源节点始终保持活动 N 记录，替换目标单独保存 NR1。旧源前缀写入自包含 B2 迁离屏障，原生导入不能把历史路径或子路径带回；屏障不要求源节点被删除，也不依赖其后来是否仍然活动。删除屏障 B1 与迁离屏障 B2 分别验证，未知格式拒绝。

`NodeRenamePublication` 保存原请求、原组及全卷封口、源和两端父目录的完整版本、替换身份、属性摘要与首次完成审计。`node_rename_publication(serial, route, epoch, operation)` 不要求存活 owner，在同一读事务中校验保留窗口、原任务与唯一 typed 结果；不按当前路径重新定位。临时清单和原请求对象块清理后仍可重取，结果缺失或损坏时拒绝，不修复或重放。Rename 完成不伪装成 SingleNode 的单父目录 ACK；两类完成入口和结果验证各自限定任务类型。

上述功能使用隔离 FDB、默认线程栈及模拟介质验证；跨父空目录、同父文件替换、接任、缺证据和响应丢失重试分别覆盖。owner actor 自动执行见后文，SDK 和 CLI 原日志见后文，FUSE 的 rename 接线仍待完成，真实带库测试继续按全部 FDB 软件功能补齐后的分阶段安排执行。

### rename 逐卷执行与接任恢复组件

`daemon::fdb_rename_media::NodeRenameExecution` 串行处理一个原 Writing 组的当前逐卷意图。它要求当前 FDB owner、完整 held 记录，以及设备守护中与调用方完全一致的多数派 Permission 对象和代次。该组件不抢占设备、不移动介质，不格式化或 salvage；装卸和 fencing 仍由 owner 调度层负责。

执行先重取当前原组和意图，再验证实际卷身份、PR 和恢复构造。已有原 S4 或关闭记录须与实际干净介质相符；发现已提交但尚未记录的原 S4 时只接纳，不重写。旧 owner 的未发生意图关闭后必须用新物理 UUID 重试；T1/T2 仅在当前追加策略允许且原来源严格匹配时显式修尾，并保存关闭结果。未知较高构造拒绝，不自动导入、丢弃或更换原请求。

当前 owner 的原 T0 意图在缺少投影时返回 `NeedsProjection`，不推进元数据或追加介质；提供完整原投影后，写入门控、暂存、同步、实际 S4 证明和原子观察依次执行。Permission 在元数据与介质边界重复核对，设备守护覆盖每个 CDB。错误消耗挂载对象，冻结或结果未定的卷不能被调用方继续使用。

隔离 FDB 与模拟介质覆盖 Permission 身份不符、缺投影、正常提交、S4 重试、旧 owner T0、真实 closing-FM 中断、IP 失败但 DP 已有原 S4、未知较高索引；修尾接任还覆盖 Serving 前的 Execution 许可阶段。owner actor 自动任务循环见后文，SDK 和 CLI 原日志见后文，FUSE rename 已接入同一持久改名入口，组件通过不代表真实带库验证完成。


### rename 原准备记录恢复与参与卷预览

`node_rename_next_participant_volumes` 从原规范投影预览下一页最多 32 个参与卷 UUID，不推进捕获游标，不创建记录，也不授予介质写入许可。重复预览与随后封存页的卷集合一致；空目录仍使用首次参与页中保存的原锚卷。

准备阶段区分尚未生成的配方与已被原参与页引用后缺失的配方。后者读取和保存均报错，接任 owner 不得重新生成描述符或替换首次创建者。已封存全局清单后不能增加新配方。参与页按原有序卷区间进行有界二分查询，缺页和作用域错误保留原状。

页生成器在全局头、后续参与页或原尝试记录仍存在时，拒绝重建缺失页；非首页还须核对前页链。全局头和首页同时缺失时，任一后续页或 `QM5` 至 `QM9` 原尝试状态都会阻止把任务解释为从未开始。仅完全未开始的空源返回 Incomplete，供自动调度选择首个合格锚卷；这些检查不新增持久化格式或兼容路径。

隔离 FDB 与模拟带库验证接管后删除已引用配方、缺少全局头、缺少首页、仅留后续页或各类尝试状态时的拒绝和不修复行为；原记录恢复后仍保留首次 owner 与字节内容。此准备接口由 owner 自动调度使用；准备接口单独通过不代表真实带库测试已完成。


### owner 自动执行原多卷 rename

可写 owner 的公平持久队列按请求 kind 分派。Queued 请求先核对原双路径条件和池 gate，再确认介质与驱动器可调度；已有文件源或替换目标按原卷选择，不能由同池无关磁带代替。尚无合格介质时保留 Queued，其他池任务仍可前进。Prepared 保留原 hold 后，按有界规范页、最多 32 卷的预览和原配方建立全局参与集合，再记录完整原尝试并整组准入 Writing。

执行逐卷使用现有受许可装卸和 PR 守护。已有原 S4 观察的卷保留成功结果；未发生的旧 owner 意图必须关闭并用新 UUID 基于当前已验证来源重试。T1/T2 仅执行明确的 Discard，未知较高构造仍拒绝。全部原 S4 封口后，逐页释放共享占用，再一次性发布命名空间与原两路径回执。Publishing 重试不重新写带，Writing 错误不会被转为未写拒绝。

启动 Execution 阶段先识别已装载卷的原 Rename hold，恢复该卷的原意图，再执行正常的精确 DP/IP 与前驱证明并登记可调度挂载。完成初始挂载核验后再串行恢复剩余原任务，最终核对多数派许可和 FDB owner 才进入 Serving；通用 SingleNode/文件恢复不能接管 Rename 的共享占用。

DP 原构造仍须通过原始 SHA、卷/代数/位置、完整 closing FM、最新 EOD 和必要增量链校验。IP 在重写 opening FM 后、首条 XML 前中断时，只接受精确的标签结束标记、opening FM 和 EOD 空构造，结果 `ip_generation=None` 明示待维护。Full 的当前文件引用和增量链的全部必要来源均不得引用 IP 数据；部分 XML、其他尾部形状、IP 领先及同代不一致仍拒绝。该校验不补写 IP、不产生新 DP 代数，也不重放原物理意图。`verify_dp_ip_head`、`verify_ip_not_ahead` 和捕获来源的 IP 代数直接使用可空结果，不保留旧返回类型。

隔离 FDB、真实 owner 线程和 Sim 验证跨卷文件替换、跨父多卷目录子树及空目录、原 parent 版本与后代身份、原内容 SDK 读取、ownerless 首次回执、接任重取、T0/T2/DP-S4 启动恢复，以及无介质队列公平性。纯模拟负例另验证空 IP 与部分 XML、IP 数据引用的区别。HTTP 双路径提交见后文；SDK 和 CLI 的持久原日志见后文，FUSE rename 已接入同一持久改名入口；真实带库实测仍在全部 FDB 软件功能补齐后分阶段执行。


### rename 文件系统 HTTP 协议

现有 FDB HTTP 服务提供独立的双路径请求：

| 方法与路径 | 行为 |
| --- | --- |
| `POST /v1/fdb/renames/{serial}/prepare` | 只读核对原条件，返回规范请求，不登记任务 |
| `POST /v1/fdb/renames/{serial}/requests` | 登记或重试保存的规范原请求，待执行返回 202，已知终态返回 200 |
| `GET /v1/fdb/renames/{serial}/requests/{epoch}/{operation}` | 查询原请求，未知身份返回 404；不要求存活 owner |
| `DELETE /v1/fdb/renames/{serial}/requests/{epoch}/{operation}` | 取消原未写任务；Writing 拒绝直接取消，已知终态保持首次结果 |

准备正文为完整的规范 JSON，包含 `version=1`、`pool`、`operation`、`from`、`to`、`source`、`source_parent`、`destination_parent`、`target`、`no_replace`、`time`。每个原身份为 namespace/inode/version 三元组；`target` 必须明确为原三元组或 null。路径、UUID、版本、类型、替换和时间均严格验证，缺字段、额外字段、重复字段及非规范编码拒绝。所有接口固定 `x-tapers-catalog-namespace`，不接受查询参数。准备与登记沿用现有 Serving owner 转发和逻辑正文预算；接口不接收物理计划、卷意图或提交证明。

调用方须在首次 RPC 前保存完整原意图与 operation/time，并在登记前持久保存返回的规范原请求。返回 `kind=rename` 和原请求指纹，不能作为 SingleNode 请求使用。终态结果独立保存源与两端父目录的完成后完整版本、被替换目标的退休版本、目录标记、属性摘要及原组/发布摘要；同父目录只推进一次。未知结果按原 epoch/operation 查询，不按当前路径创建替代请求。

GET 读取原任务，Completed 进一步从独立 `NodeRenamePublication` 取得原结果，并核对原作用域、请求指纹、发布摘要与首次完成审计。临时清单和请求对象块清理后仍可查询；已完成结果缺失或损坏返回不可用，不伪装成 404，也不补写。新 owner 的重试和对已完成任务的取消保持首次回执，原路径后来同名重建也不会改变结果。

这些路径供原生文件客户端和 FUSE 的文件系统语义使用，复用现有 HTTP 服务。普通 S3 对象改名通常由复制后删除实现；AWS 的 RenameObject 仅支持 S3 Express One Zone 目录桶，不能据此代替这里的目录子树及原双路径条件协议。[AWS 复制与改名说明](https://docs.aws.amazon.com/AmazonS3/latest/userguide/copy-object.html)，[RenameObject 范围](https://docs.aws.amazon.com/AmazonS3/latest/API/API_RenameObject.html)。SDK 和 CLI 的双路径持久日志见下节，FUSE 使用[持久改名入口](#fuse-rename-持久改名)；S3 RenameObject 适配见 S3 对象接口；它复用相同执行器，文件系统 RPC 继续承载目录子树语义。

### rename SDK 与双路径持久日志

默认构建可使用 `client::fdb::{RenameSpec, RenameCondition, RenameJournal}`，不需要链接 FoundationDB 客户端库。`RenameCondition` 保存源、源父目录、目标父目录的完整 `FileVersion::Fdb`，并以 `target: None` 明确固定原目标不存在；已有目标必须保存其完整身份。所有条件属于同一 namespace，`RenameSpec` 同时固定池 UUID、两端路径和 `no_replace`。

`RenameJournal::create(library, &spec, directory)` 在首次 RPC 前保存原条件、operation UUID 与时间；`FdbClient::prepare_rename_journal` 只读准备并持久保存规范请求。也可通过 `prepare_rename(library, &spec, directory)` 完成这两步。准备应答丢失后重开原目录重试，保留原意图；准备完成后始终使用保存的 epoch、operation 和请求正文，不按最新路径重新获取条件。

`run_rename_journal` 或 `run_rename_saved` 先查询原身份，未知时才登记原请求；应答丢失后继续查询原操作。未确认结果返回 `Indeterminate` 并保留原目录。`rename_status_saved` 只读查询，尚未准备的日志返回 `None`；`cancel_rename_saved` 先返回已知终态，再对原未写任务请求取消，不为取消而新建或准备任务。`run_rename_saved` 对 Cancelled/Rejected 返回拒绝；日志仍保存首次终态供查询。

日志独占租约、目录与头文件身份、相对目录 FD、原子发布和 fsync 防止并发复用或路径替换；已持有目录改名后仍操作原目录，不写入旧路径上的替代目录。首次终态绑定原头摘要并单独保存，后续可离线查询、运行或取消而返回同一结果。头丢失、替换或回执损坏均报错，不转为网络重试；同名路径重建或新 owner 不会替换首次终态。

软件验证使用隔离 FDB、真实 HTTP/owner 和模拟磁带，覆盖准备与登记响应丢失、原目录移动与替换、无 owner 重取已完成结果、同版本父目录重建拒绝、无介质排队及取消、离线首次终态读取。CLI 命令见下节，FUSE rename 调用已接入同一持久改名入口；未访问真实带库。

### rename CLI

默认构建的 `ltfsctl fdb rename` 使用上述 SDK，不需要 FoundationDB 客户端库。`prepare` 要求显式库、池 UUID、namespace 和源逻辑版本；查询并保存源、源父目录、目标父目录的完整 FDB 身份。同父目录复用同一条件。替换目标必须给出 `--expected-target-version`；省略即固定原目标不存在，不能覆盖稍后出现的同名对象。`--no-replace` 与目标版本参数互斥。

```bash
ltfsctl -e <节点:7401> --library <库序列号> --pool <池UUID> fdb rename prepare \
    /old /new --namespace <namespaceUUID> --expected-version <源版本> \
    --no-replace --request /var/lib/tape-client/rename-request
ltfsctl -e <节点:7401> fdb rename run /var/lib/tape-client/rename-request
ltfsctl -e <节点:7401> fdb rename status /var/lib/tape-client/rename-request
ltfsctl -e <节点:7401> fdb rename cancel /var/lib/tape-client/rename-request
```

准备只保存原意图并只读预检，不登记任务或写带。源版本、namespace 或原目标条件不符时拒绝，不留下新的请求目录。目录已存在时使用 `run/status/cancel`，不能重新准备覆盖原请求。后三个命令从保存目录读取原库、namespace、池、两端路径与身份，拒绝另给 `--library/--pool`，不按当前路径重选原操作。

已缓存的首次终态可离线查询；已完成后的 run/cancel 同样返回首次结果。原请求取消后 run 返回非零，status/cancel 仍可重取原 Cancelled 结果；日志或回执损坏时报错并保留现场。FUSE 改名沿用下述同一原日志协议；软件验证不表示真实带库验收。

### FUSE rename 持久改名

`tapefs::RenameState` 保存源与目标路径、完整两端条件以及同一原日志租约；`FdbBackend::mutate_rename` 返回包含源、两端父目录、原替换目标完整版本与目录标记的 `RenameOutcome`。规范条件先纯校验，再在已持有的日志根描述符下创建 `rename-UUID`；准备或登记响应不确定时保留原目录和独占租约，不根据旧路径重选请求。

挂载恢复将上传、单节点和 rename 视为独立日志域。先逐项验证全部本地记录的库、namespace、池和原头/回执，再逐项恢复；任何错误作用域或损坏日志均阻止整次挂载，不先执行其他有效草稿。恢复沿用原 epoch、operation、时间与两端条件；源名字后来重建时仍只重取已完成原结果。日志根改名后继续使用原描述符，旧路径上的替代目录保持原样。

SDK 的 `RenameJournal::create_in/open_in` 仅供 crate 内部使用，沿用现有原日志编码、相对 FD、原子发布及 fsync；没有新增兼容 reader 或服务端口。已有父目录与目标条件的规则仍由规范 rename 协议约束。FUSE 的文件、符号链接及目录子树改名使用此类型化入口，支持同类型目标替换和 `RENAME_NOREPLACE`；目录替换要求目标为空。


改名先冻结相关数据句柄，将源及已打开子文件的脏数据提交到各自原上传，再固定源、两端父目录和原目标的完整 FDB 身份与版本。同一父目录只采用一个版本；同名父目录被删除重建后拒绝复用原 inode。原目标若存在尚未确认的上传，改名前返回 `EBUSY`，先通过根 sync 确认该上传。

完成回执经核验后再调整本挂载的路径映射。源及已知子 inode 保持不变，打开 writer 改用新路径和准确的已提交版本；目录下文件的版本不会因祖先改名而增加。被替换目标的打开句柄保留原缓存，后续 fsync 不会向新目标上传。目标退休与当前 head 清除属于同一发布事务，历史副本和退休证明继续保留。

准备或登记响应丢失时不猜测结果，当前挂载停止路径操作及写入，已有句柄仍可读取缓存。根 `user.ltfs.sync` 重新查询同一原日志，取得原完成结果后恢复路径及 writer；进程退出后的重新挂载同样恢复原操作，不为当前同名对象创建替代请求。完成结果与当前路径已经发生其他变更而无法安全核对时，保持屏障并报告错误。

## 停机宽限与 VIP 收尾

`ltfsd --shutdown-grace-s` 默认 300 秒，当前物理部署显式设置 600 秒。这是等待在途设备任务、目录后台任务和 VIP 收尾的总上限；正常条件满足后会提前退出，与各条 SCSI 命令的超时分别配置。

有 Leader 时，节点仍等待 VIP 释放记录提交。无 Leader 时，只有设备执行线程全部结束、目录任务完成，且本机网络线程已删除 VIP 并回读确认不存在，才允许提前退出；确认必须匹配当前节点、执行轮次和 token。没有确认、旧 token 或其他节点的确认均不能放行。此路径保留未提交的 Raft 占用记录，不会伪造共识释放或允许其他节点凭超时接管；恢复集群时原持有节点会重新报告撤址。

单节点及多库实际 Raft 循环已覆盖无多数派退出与未确认状态保留；新版物理停机耗时仍待现场验收，不能将软件回归耗时当成带库实测。

### IBM EE 初始化与其他恢复方式

真实 IBM 带库上 EE 1.3.4 已完成四查询、普通 LTFS 导入、`-P`、`-R`、重名 `_1`、正文 SHA 读回、默认 reconcile 与 validate 的调用跟踪；用户提供的 `ee.md` 是 1.4 文档，两种版本的差异独立记录。四查询成功窗口内前台及 MMM/MD/LE 全线程未发物理 SG_IO；显式 rescan 发出库存查询，不能据此视作已读取每卷 index。

初始化 `CLUSTER` / `ADD_CTRL_NODE` 建立配置和资源关系。库存发现、LE 读取 index/cache、EE 建立 GPFS stubs 是三个步骤；初始化与 rescan 不会自动重建用户文件目录。`tape import` 是本次已实测的外部 LTFS 目录导入入口，普通 LTFS 转为 EE 布局时会写 index/MAM。SOBAR 可从事先保存的 GPFS image 和 EE 配置恢复 stubs；`tape online`、迁池仅恢复已有映射的访问，reconcile/validate 不重建丢失的 stubs。SOBAR、online 与迁池未在本轮实测。

详细文档来源、真实命令结果及限制见 [EE index 导入研究](../.scratch/ltfs-ha/reports/ee-index-import-model-20261103.md)。Holo 结果不作为真实带库验收。


## 已分配磁带的索引刷新

`tape import` 与规范 HTTP import 管理端点同时支持首次接入和原卷刷新。已分配条码只能在原 Library/Pool 内刷新，持久核对原 UUID、assignment、ready/source 与活动索引；同代异源和旧代数拒绝。目录前缀是首次路径投影，不是磁带写入路由。刷新保留 UID/源路径映射，兼顾首次导入前缀、后续本项目追加和 EE 外部写入，避免重复前缀。

池屏障内逐页捕获旧引用和历史 UID，逐项保存精确旧值以便取消/错误回滚。该卷当前文件更新、新文件加入、缺失当前文件退休，已有共享目录保留；另一卷较新 head 及已删除文件的旧 UID 不复活。被取代 UID 的抑制标记属于新扫描，后续重复刷新仍有效。最终事务发布新来源/扫描基线，保留 assignment 与不可变历史，清理任务暂存；旧任务重放不触设备。新扫描成为后续正常写入的活动基线。

软件验证包含真实隔离 FDB 的多页接任/回滚/换卷与回退拒绝，以及真实 HTTP/CLI + Sim 的外部写入、原卷刷新、继续追加、跨磁带覆盖后重复刷新与删除后刷新。真实 EE/FUSE/S3 磁带内容验收结果见[现场报告](../.scratch/ltfs-ha/reports/fuse-s3-ee-physical-20261104.md)，进行中的项目不视为已通过。


FUSE 私有缓存读取允许已导入 EE 普通文件的 digest 为 null：仍要求固定命名空间、完整传输长度、读取前后同一 inode/version/placement，已登记摘要存在时严格核验，不把损坏或未知算法当成缺失摘要。无带内摘要时没有正文摘要证明，保留 null；显式校验导出 fdb get 仍拒绝无摘要，不伪造 SHA。真实 EE 新文件通过独立数据源 SHA 对照的验收见[本轮报告](../.scratch/ltfs-ha/reports/fuse-s3-ee-physical-20261104.md)。

磁带后台监控由当前 Library owner 串行执行，约 30 秒更新库存，采样已核验的装载介质；监控快照、告警历史和永久 I/O 故障也保存到 FDB，不落本地 SQLite。记录按 namespace/library/barcode 隔离，写入要求当前 owner epoch，CLI/HTTP 共用投影。库存失败不覆盖旧快照，索引验证不会清除永久故障；详细采集边界、状态及容量单位见 [磁带监控](management-model.md#磁带监控)。

监控运行时不依赖 IBM EE/LE：不调用其 CLI、HTTP/REST 或私有接口，也不读取其缓存作为监控来源。设备数据由 tape-rs 自身 SCSI/SG_IO 采集后写入 FDB；EE 仅供调研参考。
