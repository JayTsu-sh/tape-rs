# 文件客户端与 FUSE 挂载

当前能力与部署状态见 [剩余实施队列](../.scratch/ltfs-ha/remaining-implementation.md) 和 [交接入口](CLAUDE-HANDOFF.md)。本文带日期的实验记录保留当时验证边界；后续记录取代早期状态。

`ltfsctl` 与 `tape-fuse` 都通过 ltfsd 客户端接口寻找 Leader；节点地址可以重复指定。它们不直接打开磁带设备。

希望客户端固定访问一个地址时，可启用 [ltfsd 内置 HTTP VIP](http-vip.md)：首版支持同子网 IPv4，由有效设备 owner 持有，无需 Keepalived。

数据访问现在以池为命名空间：`/libraries/<真实带库序列号>/pools/<池名称或UUID>/files/...`，`stat`、`list`、`tasks`、`sync`、目录、属性和改名操作也使用同一池前缀。库内恰好一个池时可以省略池；存在多个池时省略会返回 `400 pool_required`，避免把文件写入任意池。管理接口继续使用库作用域的 `/admin/...`。

文件任务 ID 包含本次设备 owner 轮次，受理与终态随 Raft 日志复制到各节点目录。`GET /tasks/<id>` 在接管或进程重启后仍可查询；接管时尚未获多数派确认的任务会显示 `indeterminate`，此时应按路径查询内容与哈希。写入响应只有在任务记录已应用后才确认任务 ID；等待超时返回带任务 ID 的 504，不代表请求未执行。文件任务与 `/admin/tasks` 管理任务仍使用各自的查询路径。

已持久化的文件任务通过固定 HTTP 入口在本地复制目录查询；即使该库暂时没有设备 owner，`GET /tasks/<id>` 和带池前缀的查询仍可返回终态。新任务受理与文件读写仍需要有效的设备 owner。目录快照正在发布时查询可暂时返回 `catalog_pending`，稍后重试。

```bash
ltfsctl -e 10.131.7.219:17821 --library 55L3A78011W9LL01 --pool multilib-test put ./file.bin /file.bin
tape-fuse -e 10.131.7.219:17821 --library 55L3A78011W9LL01 --pool multilib-test \
  --cache-dir /var/cache/tape-client /mnt/tape
```

Rust 客户端使用 `Client::new(endpoints).with_library(serial)?.with_pool(pool)?`；重定向及 owner 变化不会改变调用方选定的库和池。一次 FUSE 挂载固定一个池。同一个池的不同驱动器共享路径预留、任务号和最新目录视图，每台驱动器独占自己的卷会话及容量预算；文件版本序号由库级执行者统一分配。跨卷的原生改名仍返回 `cross_device`。

并发上传按需申请额外写驱动。一台驱动的批次到期时，也启动其他驱动已收齐的队列；普通工作每台只提交一批，随后返回 owner 循环处理控制请求。`mountlimit` 限制同池的写入驱动租约，包含准备写入的装带阶段；只读装载、维护源带和闲置但没有写入租约的介质不占该配额。资源不足时，读请求或其他池可以借用已排空的写驱动；借用前先关闭新准入、再次核对空闲状态并生成卸载检查点。整库维护与计划停机要等待所有驱动写入作业归还句柄，才会移动介质或释放 PR。

选带先检查池限制、介质状态、节点组和 m/r/g 角色，再核对驱动的 REPORT DENSITY SUPPORT / WRTOK。优先复用已装载的合格介质，其余按最近有效 MAM 的物理已用比例从高到低排列，最后按条码确定顺序；这项次序是本项目的确定性策略，不声称复现 IBM 未公开的全部调度算法。装载后还会核对当前介质格式。无法确认兼容性的驱动或介质不会获得写入资格。

写入预算必须来自本次有效的 DP/IP MAM 读数，并保留索引收尾空间；不再用虚构的大容量替代缺失读数。容量刷新失败时，已确认的提交仍成功，已有文件可读，新写入暂停并等待有界重试；不会仅因容量未知将磁带标成 FULL 或 CHECK。显示仍保留同卷最近有效容量，并注明刷新失败；这个缓存不能代替新的写入准入证据。索引分区剩余空间不足也会停止新写入。

大目录的原子发布在后台执行，该库的数据入口暂时返回 `503 catalog_pending`；目录发布完成后可重试。控制查询和多数派许可继续工作。目录快照固定同一日志索引，制作快照本身不暂停数据请求。

HTTP 接入使用 axum / Hyper，保持现有端点、状态码、JSON 字段和每次请求后关闭连接的行为；客户端无需升级。同步文件服务和设备任务等待在有界阻塞任务中执行，上传和下载逐块传输，不把整个文件装入内存，也不套用缓冲型提取器的默认 2 MiB 限制。

每节点最多同时接入 256 条连接、处理 32 个业务请求；下载发送期间仍占业务额度。业务额度用尽时返回 `503 http_busy`，连接额度用尽时关闭新连接；`/readyz` 不占业务额度，但仍受连接总量限制。请求头总量最多 32 KiB，须在 30 秒内接收完整；请求体连续 60 秒未收到下一帧时中止接收。文件上传仍要求 `Content-Length`，不支持 chunked 上传。直连 owner 时，`Expect: 100-continue` 在准入成功、开始读取内容时才发送；经固定 VIP 转发时，入口的 `100 Continue` 只表示可以发送正文，最终准入结果以后续状态码为准。owner 提前拒绝转发请求时，最多用 5 秒排空 64 MiB 剩余正文后返回错误；正文超限或过慢、缺少长度以及业务额度已满时，客户端可能在发送过程中遇到断连而收不到原始错误。HTTP 断开或等待超时不代表已经提交的设备任务被取消，结果未定时仍须查询，不能自动重放有副作用的操作。

设备接管时，驱动器内仍有盒带不代表介质已经上带。新 owner 在设备隔离完成后显式执行 LOAD 并等待就绪，再识别和恢复卷；这覆盖计划停机已执行 UNLOAD、但没有把盒带移回槽位的情况。磁带恢复时间还包含机械定位和索引链检查，HTTP 就绪不等于池里已有可写磁带。

普通接管时，两分区的新鲜 VCI 与实际完整末索引一致、DP 为最新视图时，允许 IP 仍为旧检查点。Full 自包含；增量必须完整读取、核验并重放到其 Full 基线，才能省去基线之前的历史 Full 链审计，IP 维护债务保留。仍读取并核验两分区索引、卷身份、EOD、尾部和追加位置；增量链不完整会拒绝开放，过期或不符的 VCI、异常尾部及显式历史审计沿用原路径。依据为 [LTFS 2.5.1 §9.2、§10.3](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)，不改变卷提交屏障或设备隔离。


与 IBM EE 的最新物理对照、已确认差距及待验边界见 [EE GAP 现场审计（2026-10-21）](ee-gap-physical-audit-20261021.md)。

## 集群 HTTP 管理接口

管理命令直接接入 `ltfsd`，通过 Raft 复制配置及任务状态，由当前设备 owner 执行设备操作。无需另启 `tape-library` 控制器，也不导入或复用其独立 `--library-state` 数据库。同一逻辑库仍只允许一个有效设备 owner；节点组是这个设备组内的调度策略，不增加 Raft 成员或并行设备 owner。

本次扩展控制日志和快照字段；旧快照可由新版读取，但所有集群节点必须统一升级，不能混跑旧版或在新增管理状态后回退。独立 `tape-library` 的单机模式保留。

| 能力 | 方法及路径 | JSON 请求体 |
| --- | --- | --- |
| 管理状态 | `GET /admin/status` | 无 |
| 带库查询 | `GET /admin/libraries`、`GET /admin/libraries/current` | 无 |
| 重新读取库存 | `POST /admin/libraries/current/rescan` | `{}`；不执行 INITIALIZE，不装带或搬带 |
| 池列表/详情 | `GET /admin/pools`、`GET /admin/pools/{pool}` | 无；pool 可用名称或 UUID |
| 建池 | `POST /admin/pools/{name}` | 无；沿用 `?file_limit=200000` |
| 删除空池 | `DELETE /admin/pools/{pool}` | 无 |
| 修改池属性 | `PATCH /admin/pools/{pool}` | `{"attribute":"mountlimit","value":"1"}` |
| 磁带列表/详情 | `GET /admin/tapes`、`GET /admin/tapes/{barcode}` | 无 |
| 核对库存后分配 | `POST /admin/tapes/{barcode}/assign` | `{"pool":"poolname"}` |
| 核对归槽后取消分配 | `POST /admin/tapes/{barcode}/unassign` | `{}`；拒绝仍有已知目录内容的磁带 |
| 离线/上线 | `POST /admin/tapes/{barcode}/offline`、`…/online` | offline：`{"message":"maintenance"}`；online：`{}` |
| 黑名单/清除 | `POST /admin/tapes/{barcode}/blacklist`、`…/clear-blacklist` | blacklist：`{"reason":"inspection"}`；清除：`{}` |
| 机械搬带 | `POST /admin/tapes/{barcode}/move` | `{"location":"homeslot"}` 或 `{"location":"ieslot"}`；移至 I/E 前必须 offline |
| 校验 | `POST /admin/tapes/{barcode}/validate` | `{"full_read":true}`；默认只验证索引，完整读取核对已有哈希，无哈希单独计数 |
| 对账 | `POST /admin/tapes/{barcode}/reconcile` | `{}`；通过已有分片流程更新集群 catalog，不做 GPFS/HSM 对账 |
| EE 保存对象 | `POST /admin/tapes/{barcode}/objects` | `{"offset":0,"limit":100}`；limit 为 1–500，按 `next_offset` 继续，跨页须核对卷代数 |
| 恢复 EE 保存对象 | `POST /admin/tapes/{barcode}/restore` | `{"destination":"/absolute/new-directory","confirm":true}`；目的地在执行 owner 本机，只恢复普通文件/目录/链接，不创建 HSM stub |
| 空间回收 | `POST /admin/tapes/{barcode}/reclaim` | 沿用既有回收协议、两驱动器要求及格式化前核验 |
| 驱动列表/详情 | `GET /admin/drives`、`GET /admin/drives/{serial}` | 无 |
| 驱动调度资格 | `POST /admin/drives/{serial}/up`、`…/down`、`…/role`、`…/assign` | up/down：`{}`；role：`{"role":"mrg"}`；assign：`{"group":"G0"}`，`-` 解除分配 |
| 节点与节点组查询 | `GET /admin/nodes`、`GET /admin/nodes/{id}`、`GET /admin/nodegroups`、`GET /admin/nodegroups/{name}` | 无 |
| 创建/删除节点组 | `POST /admin/nodegroups/{name}`、`DELETE /admin/nodegroups/{name}` | 无；拒绝删除默认组或仍被使用的组 |
| 指定节点调度组 | `POST /admin/nodes/{id}/assign` | `{"group":"G0"}`；`-` 停用该节点的设备调度，不改变 Raft 成员资格 |
| 管理任务列表/详情 | `GET /admin/tasks`、`GET /admin/tasks/{id}` | 无；与文件操作的 `/tasks/{id}` 分开 |
| 请求取消 | `POST /admin/tasks/{id}/cancel` | `{}`；协作取消，不中断在途 CDB，不回滚已完成操作 |
| 清理终态任务历史 | `POST /admin/tasks/clearhistory` | 必须 `{"confirm":true}`；保留 queued/running，删除的历史不能恢复 |

`ltfsctl tape offline <条码> <说明>`、`tape online <条码>`、`tape blacklist <条码> <原因>`、`tape clear-blacklist <条码>` 对应上述可逆策略接口；`tape validate <条码> [--full-read]` 校验 LTFS 索引，指定选项时还读取文件核对已有哈希。命令返回管理任务号；须用 `ltfsctl task show <任务号>` 核对终态。操作特定带库时加 `--library <真实 changer 序列号>`。

`ltfsctl` 现在可直接查询集群管理资源，例如 `library list`、`library show <序列号>`、`tape list/show`、`drive list/show`、`node list/show`、`nodegroup list/show`、`pool show` 和 `task list/show`。除全局 `library list` 外，`--library <真实序列号>` 始终限定目标库。输出为管理接口的完整 JSON，容量与状态字段不经 CLI 二次计算。文件任务单独用 `file-task <id>` 查询。

尚未有专用 CLI 语法的管理写入可用 `ltfsctl --library <序列号> admin post|patch|delete /admin/... --body '<JSON>'` 显式调用上述接口。例如 `admin post /admin/libraries/current/rescan --body '{}'`；设备任务返回 `task` 后用 `task show <id>` 查询终态。连接在写请求可能送达后断开时，客户端返回结果未定，不会自动重放请求。

池属性支持 `poolname`、`file_limit`、`mediarestriction`、`mountlimit`、`nodegroup`、`lowspacewarningenable`、`lowspacewarningthreshold`、`nospacewarningenable`。数字及 yes/no 属性沿用字符串值；mountlimit 为 0 时不限制池的写入驱动租约数，容量告警阈值单位为 TiB。容量采用最近一次有效 DP MAM 缓存：未装载时沿用旧值，装载及写入提交后由 owner 刷新并通过 Raft 复制；刷新失败保留同卷旧值并报告失败。未观测或已知卷身份失效时返回 null，不从文件大小推算剩余容量，也不冒充 EE 后台告警事件。m/r/g 分别控制写入、读取和维护；驱动、池及节点分组同时参与设备调度。

所有变更由控制 Leader 受理；访问 Follower 返回 `503 not_leader` 及已知 Leader 地址。查询可访问任意节点，返回 `applied_index` 和 `answered_by`，属于已应用状态视图，不承诺线性一致读取。库存查询读取带时间、owner 和轮次的最近观测；首次使用或要求新鲜库存时显式 rescan。驱动序列号必须与配置设备精确匹配。

需要 owner 核验或操作的请求返回 `202 {"task":123,"status":"queued"}`，表示任务已登记；必须查询 `/admin/tasks/123` 的终态。任务开始先经多数派提交，再在现有隔离和执行许可下操作设备。任务状态为 queued/running/success/failed/cancelled/indeterminate，带执行 owner、round、进度和结果；索引代数及结果不会因换节点丢失。无设备 owner 时可查询、建删空池及管理节点组/取消任务，新的设备任务会拒绝受理。

需要借用磁带的维护以及池属性修改会先暂停新上传、等待已准入上传完成、提交并收束当前写入带、将本实例使用的带归槽；维护结束再按最新策略选带。驱动策略变更要求目标驱动器为空，当前 owner 的节点组变更要求受管驱动器为空。offline 保留 assignment，上线须实际装带验证 LTFS 索引；校验和对账允许检查 offline 带，但不绕过黑名单。搬到 I/E 不等于 EE export，回到 home slot 不等于 HSM import。

分配磁带后，既有调度器可能自动装带，并在首次选择空白带写入时按既有规则格式化；foreign 介质仍拒绝覆盖。恢复操作的文件输出留在任务记载的 owner 本机，不随 HA 自动搬迁。

连接断开、`504 indeterminate` 或任务换届并不取消设备操作。未开始的 queued 任务可由新 owner 领取；running 任务在执行轮次结束后标为 indeterminate，不自动重放，迟到的旧轮次报告不能覆盖结果。已进入恢复过程的取消请求不能抹去已写入文件或已完成提交；失败也不代表副作用已回滚。响应丢失时先查询任务及资源状态，不能盲目重新提交。

管理请求体上限 16 KiB，单任务结果上限 256 KiB，任务历史最多 1024 条且管理状态总量上限 2 MiB；新设备任务会预留结果空间。达到上限后显式清理终态历史。大型索引通过原有 catalog 分片复制，不放进管理结果或额外复制整份 XML。同一设备组一次只受理一个管理任务，既有 reclaim 期间拒绝其他设备任务。

例如，在控制 Leader 的客户端端口提交校验，再查询结果：

```bash
curl -X POST http://127.0.0.1:7401/admin/tapes/TEST01L8/validate \
  -H 'Content-Type: application/json' -d '{"full_read":true}'
curl http://127.0.0.1:7401/admin/tasks/123
```

这组接口已有模拟器和三节点 Raft 回归验证。容量/状态、装载与实写刷新、离线缓存、重新上线和 owner 切换已完成物理带库专项验收，见[现场报告](../.scratch/ltfs-ha/reports/admin-capacity-physical-20261020.html)。本轮使用隔离候选和 RC0018L9，其他管理操作以各自验证记录为准。

### 容量与状态输出对标

容量主字段采用 EE 的空间划分：`capacity = used_space + free_space + non_appendable_space`（各项已知时），统一使用整数字节。对已入池带，`capacity` 是格式化后数据分区的容量观测，`used_space` 是已占用空间；可追加时余量计入 `free_space`，已知不可追加时 free_space 为 0，余量计入 non_appendable_space。未入池带的上述四项按 EE 列表口径均为 0，表示尚未纳入池管理，不能推断为空带或物理容量为零。物理读数保留在 partitions.data/index 和 volume_capacity 技术详情，不使用两分区合计冒充用户数据容量。参照 [IBM EE REST 定义](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=information-rest-api-tape) 和 [eeadm tape list 的 Usable/Used/Available 说明](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=tape-eeadm-list)。

命名同时参考本地保存的 EE 1.3.4 REST 与 LE tape show 现场输出。EE 返回字节；LE total_dp/remaining_dp 为 MiB，例如 LE total_dp=16736984 对应 EE capacity=17549999734784 字节。本次更改新 HTTP 管理接口顶层 free_space 的语义：它不再等同物理余量；原始值须读取 partitions.data.free_space。没有更改容量采样、介质格式、目录持久化或设备操作策略。

磁带列表、详情及 `/admin/status` 的 tapes 使用同一套字段；池列表、详情及汇总也使用同一容量计算：

| EE 参照字段 | 本接口含义 |
| --- | --- |
| `capacity`、`used_space`、`free_space` | 格式化后数据容量、已占用、允许追加的剩余空间；capacity/used 由有效 DP MAM 最大及最大减物理余量得到，free 按可追加状态划分；`capacity_unit=bytes`、`capacity_partition=DP` |
| `non_appendable_space`、`non_appendable` | 已知不可追加时的未用空间，两个名字同值（现场 EE JSON 使用前者，部分版本文档使用后者）；可追加时为 0，未知为 null |
| `partitions.data`、`partitions.index` | DP/IP 各自的总量、已用、剩余及物理 partition_number、成功观测时间、代数、刷新结果；两分区独立保存缓存 |
| `volume_capacity` | IP+DP 物理容量合计，标明 capacity_partition=IP+DP；任一分区未知或合计溢出则三项均为 null。它不是用户文件可写空间 |
| `capacity_source`、`capacity_observed_at`、`capacity_cached` | 本项目扩展：来源 observed_dp_mam/unknown、成功观测时间，以及是否为缓存值；GET 始终读缓存，未知时 cached 为 null。未入池主容量 source=unassigned、cached=false、时间/代数/刷新结果为 null；历史观测在 partitions 中。空池 source=empty_pool |
| `capacity_generation`、`capacity_generation_matches`、`capacity_refresh` | 磁带容量观测代数、是否与目录代数一致，以及最近刷新结果（status/time/error/owner/round）；同 UUID 的跨代观测可保留。容量报告与目录摘要分别提交，观测也可能短暂领先目录，matches=false 不等同于采样失败；无可比较目录代数时为 null |
| `active_space`、`reclaimable_space`、`reclaimable_percent` | 尚未实现 EE 活跃/失效对象空间计量，空间及可回收比例明确为 null（界面显示“—”）；不使用物理占用减文件字节数伪造可回收空间，也不补成 0% |
| 磁带 `state`、`volume_state` | state 优先报告 blacklisted/offline，再报告 unassigned 或已知介质状态；volume_state 保留底层 appendable/data_full/full/check 等状态 |
| 磁带 `status`、`appendable`、`appendable_reason` | status 是严重程度，state 是业务状态；分级规则见下文。可追加资格综合卷状态、离线/黑名单、池介质限制及文件数上限；no 附中文原因，未分配或状态未确认时 appendable 为 null。它不表示此刻已装载或 owner 正在服务 |
| `pool_id`、`pool_name`、`offline_msg` | 当前分配池及离线说明；无对应值为 null |
| `location_type`、`address`、`drive_id` | 最近库存中的 homeslot/ieslot/drive、绝对 SMC element 地址（整数）及驱动序列号；未知为 null，address 不是 CLI 槽位序号 |
| 池 `id`、`num_of_tapes`、`media_restriction`、`mount_limit`、`nodegroup_name` | UUID、分配带数及有效策略；保留旧 uuid/name/tapes/options 及池列表外层结构 |
| 池 `capacity_complete`、`capacity_known_tapes`、`capacity_unknown_tapes` | 主容量各项是否完整、有效 DP 观测成员数、缺少有效 DP 观测成员数；完整性同时要求可追加状态已知且求和无溢出。partitions 内分别统计 DP/IP 完整性和数量；未知汇总保持 null，不返回部分和冒充总量 |
| 驱动 `enabled`、`role`、`nodegroup_name` | 平铺有效调度策略，保留 policy；enabled 不代表驱动已经就绪 |

LE 的 NEED_ASSIGN 对应 state=unassigned，WRITABLE 由本项目 volume_state=appendable 表达；盒带在驱动器里不能直接等同于 LE handling=MOUNTED。容量移出 policy，原始策略对象不再夹带未经校验的历史容量。

严重程度以 [IBM EE 1.3.5 状态表](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)为基准：appendable、append_fenced、data_full、exported、offline、recall_only、unassigned、write_protected 为 ok；full、need_replace 为 info；require_replace 为 degraded；check_tape_library 为 warning；require_validate、check_key_server、check_hba、inaccessible、non_supported、duplicated、missing、disconnected、unformatted、label_mismatch、need_unlock 为 error。本项目扩展 check=error、blacklisted=warning、reclaiming/reformatting=info；未识别状态为 unknown。展示状态与底层 volume_state 取较高严重程度，避免离线或未入池掩盖已知介质错误。data_full 表示不能再写文件内容但仍可更新元数据，full 表示所有更新均被拒绝；两者都不允许新增文件。现场旧 EE 清洗带曾返回 info/non_supported，与 1.3.5 文档有差异；本接口采用上述明确规则，不按条码猜测介质健康或复制特定旧版本分级。

分级映射只作用于已知状态，不代表已实现全部 EE 状态的自动检测。清洗带例外按现场 EE 输出展示为 state=non_supported、status=info；已知健康错误或黑名单告警仍保留较高分级。库存增加 medium_type（设备原始介质类型，2=cleaning，0=未指定，缺失为 null）、cleaning 和 cleaning_source（element_status/barcode/null）。优先使用 READ ELEMENT STATUS 的类型；标准 CLN 六位卷标及其 L1 后缀作为旧缓存/设备的保守兜底，不单凭 L1 后缀识别清洗带。数据池分配及 LTFS 数据操作会拒绝清洗带，执行装载前另查实际库存，保护历史错误分配。清洗带主容量/已用/可用/不可追加为业务零，capacity_source=cleaning、cached=false、appendable=no；DP/IP 和整卷物理容量为 null，不读取或沿用数据带容量，也不伪造可回收百分比。不会为识别而装载或启动清洗。Rust 和 EE 的池分配分别维护，不能直接将两个系统的分配状态当成同一目录比较。

未装载磁带不因查询而搬带，继续返回上次容量与观测时间；未入池时仅分区详情保留历史读数。普通写入带装载、读带装载、写入提交及卸载检查点后读取 DP 与 IP MAM，按已挂载卷的恢复报告选择实际分区号；维护装带和回收同样采样。每个分区的最大/剩余属性必须同时有效，缺失、长度异常、超出范围或换算溢出均记为该分区刷新失败。失败保留同 UUID 的该分区最近成功值，不阻止另一个分区更新；格式化前或确认 UUID 变化时两分区旧缓存失效；分配关系变化不抹掉该条码最近一次成功容量观测，重新入池可沿用有效缓存。无成功观测的物理容量为 null。旧快照若缺少容量 UUID，仅兼容同代观测；未保存 IP 的旧快照将其报告为未知，等待下一次装带补齐。

池主容量按每盘分配带的 EE 空间划分相加，包括未装载带的有效缓存；free_space 不包含离线、黑名单、已满、须检查、正在维护、达到文件数上限或不满足介质限制的带的余量。这些余量归 non_appendable_space。时间取最早成功观测；任一带 DP 未知则池主容量和告警条件为 null，已知容量但可追加状态未知时 available/non_appendable 和告警未知。空池没有分配空间，返回总/已用/可用/不可追加均 0、capacity_source=empty_pool，而非未知。IP 未知仅使池 IP 和整卷物理汇总未知，不抹掉已知 DP。

capacity_complete 不代表实时或无刷新错误：它仅说明汇总所需值已知且可安全相加。历史观测是否适用、最近是否刷新失败，须结合每盘容量代数、时间及 capacity_refresh 查看；在 drive 中也仍是最近成功观测。池汇总可能 capacity_known_tapes 等于成员总数，但因追加资格未知或求和溢出而 capacity_complete=false；空池 complete=true、已知/未知成员数均为 0。

池低空间/无空间条件按 EE free_space 判断，warning_scope=available_space；“无可追加空间”不等于所有介质物理已满。物理分区合计始终保留在 partitions，不受离线策略影响。旧池列表 tape_details 中的 bytes_used/live_bytes/reclaimable_bytes 继续沿用索引及目录统计口径，不等同于 EE used_space/active_space/reclaimable_space。

用户主视图采用 EE 的数据容量、已用、可用、可追加状态；DP/IP 明细与整卷物理合计用于高级详情。当前 capacity/used 的基础仍为设备 DP MAM，不等于当前文件大小，也未复现 IBM 私有活跃/可回收计量或预留区算法。DP 还容纳索引和历史数据；free_space 不是承诺还能写入同样多的逻辑文件字节，不代表待提交队列已预留的预算。IP 检查点在固定位置覆盖重写，IP 剩余量不能直接判断新的完整索引能否容纳；需要 IP 检查点时若写入失败仍会冻结写入并报告错误。容量展示不改变设备准入或磁带写入策略。

例如，同一盘已观测总量 1000、已占用 600、物理余量 400 的磁带，在线且可追加时返回 `free_space=400, non_appendable_space=0, appendable=yes`；离线后返回 `free_space=0, non_appendable_space=400, appendable=no`，而 `partitions.data.free_space` 仍为 400。解除分配后主容量四项均为 0，source=unassigned，分区详情仍保留 1000/600/400；重新入池后恢复使用这份有效观测。已入池但未观测过容量时容量为 null，不借用 0 表示未知。

本接口提供 JSON，没有容量文本渲染层。客户端主列表建议按 EE 顺序展示 Usable、Used、Available、Reclaimable%、Status、State；默认十进制 GB/TB（1 GB=10^9 bytes），切换二进制单位时必须标为 GiB/TiB。保留原始整数字节，不把 GB 舍入值用于汇总或反算；小于 1 GB 的非零值显示“<1 GB”或更小单位，避免显示 0 使用户误认为空带。分区与 IP+DP 仅在详情展示。

每盘磁带、每台驱动的列表和详情均返回 `inventory_observation={source,observed_at,owner,round,owner_matches_current}`。source 为 cached_inventory 或 unknown；owner_matches_current 仅比较执行轮次，不保证库存尚未变化。归槽后的库存观测可能早于服务恢复重新装带，跨 owner 也保留历史观测。外层 applied_index 是 Raft 应用进度，不是设备观测时间。要求新鲜库存时显式 rescan；rescan 不读取槽内磁带容量，GET 不查询设备。当前容量刷新由装载和提交触发，不以 HTTP 请求或空闲定时轮询触发。

## 文件客户端使用

```bash
cargo build --bin ltfsctl --bin tape-fuse --features fuse
ltfsctl -e 127.0.0.1:7401 put ./archive.bin /archive.bin
ltfsctl -e 127.0.0.1:7401 get /archive.bin ./download.bin
ltfsctl -e 127.0.0.1:7401 delete /archive.bin
mkdir -p /tmp/tape-mount /tmp/tape-cache
tape-fuse -e 127.0.0.1:7401 --cache-dir /tmp/tape-cache /tmp/tape-mount
```

挂载进程在前台运行，需要 Linux FUSE 与挂载权限。另一个终端执行 `fusermount3 -u /tmp/tape-mount` 卸载。默认只向挂载用户开放，属主权限由内核检查。

## 完成语义

- `ltfsctl put` 等待文件落带；`delete` 等待删除墓碑落带，不立即回收介质空间。
- FUSE `close` 仅确认上传到服务端暂存区。需要归档确认的应用必须在关闭前调用 `fsync`；`fsync` 成功才表示该内容已落带。
- 删除响应丢失会返回结果未定，不自动按路径重发，避免删除随后创建的新版本。
- 写入支持 `O_RDWR`、非截断打开、任意偏移覆盖、`O_APPEND` 和任意长度截断；多个写句柄共享内容及上传状态。非截断写首次需要完整下载旧内容。`O_EXCL` 提供只创建保护。
- 服务端读取只选择已提交文件；只有服务端在途版本且没有本地文件时返回 `EBUSY`。首次读打开下载整个已提交文件，因此可能等待磁带读取。同一 inode 的本地句柄共享内容，本挂载写入在 fsync/close 前可读；覆盖和截断对旧句柄及映射可见。删除后已有句柄继续引用旧文件，同名重建使用新 inode。
- `mkdir`/`rmdir` 等待 LTFS 索引提交；空目录跨挂载和节点接管保留。`mkdir` 要求父目录存在，`rmdir` 拒绝非空目录及在途子路径。目录流在打开时取快照。
- `rename` 支持当前写入卷内的文件和目录原子改名、同类型目标替换，以及 `RENAME_NOREPLACE`；目录替换要求目标为空。保留源 inode、UID、extent、mtime 和 xattr。跨带源/目标或不在当前写入卷的源仍返回 `EXDEV`；复制加删除回退不具备原子性。权限/时间修改的原生映射见下文；符号链接支持创建及读取，已有符号链接可通过 FUSE 的 lstat/readlink 和内核路径解析读取，读取链接目标时仍校验目标文件版本。可写扩展属性的范围见下文。

`user.tape.state=local` 表示本地内容尚未上传，不能据此认为已归档。其他 user.tape 属性描述服务端已提交版本。

缓存使用每次挂载独占、权限为 0700 的子目录，正常退出清理本次目录，保留缓存根目录中的其他内容。缓存空间需要容纳打开的文件（同一 inode 的句柄共享一个文件）；异常终止后的旧子目录不会自动删除。

## 当前限制与验证范围

当前提供 `user.tape.state`、`user.tape.generation`、`user.tape.version`（轮次.序号）、`user.tape.barcode`、`user.tape.sha256`，并透传已提交文件的索引修改时间与文件 xattr。索引 xattr 名称放在 `user.` 命名空间，已有该前缀则不重复添加；内置 `user.tape.*` 属性优先。文本按 UTF-8 返回，Base64 值解码为字节，损坏编码返回 EIO。原生目录的修改时间已透传；合成根目录、旧缓存推导的目录及缺失或非法时间回退为 Unix epoch。HTTP 下载已去掉 256 MiB 上限：设备线程分块写入服务端暂存目录下的匿名临时文件，完成后由 HTTP 异步任务流式发送。临时文件在最后一个句柄关闭时自动回收；服务端需要容纳并发下载的临时磁盘空间。Range 仍先读完整文件到临时磁盘再切片，尚未优化成只读所需磁带块。采用内核页缓存和默认 write-through，不启用异步 writeback-cache，也不再依赖 `FUSE_DIRECT_IO_ALLOW_MMAP`。每次 open 不设置 KEEP_CACHE，避免无条件复用旧页；支持读写混合打开；共享可写映射不属于当前承诺或 LE 官方只读 mmap 支持范围。

纯软件测试覆盖删除提交、接管、墓碑回收、删除响应丢失不重放，257 MiB 文件上传/下载逐字节校验、提交等待超时后的查询、提前文件标记拒绝短读，以及适配器的关闭/同步、随机写、多写句柄、追加、截断、目录和错误映射。可显式运行使用内存后端的内核挂载测试，不接触磁带：

```bash
cargo test --features fuse --bin tape-fuse -- --ignored
```

2026-10-20 已在物理节点显式运行四项默认忽略用例：内核 VIP 地址生命周期、VIP 进程暂停/恢复/退出/强杀、增量 XML 导出及官方 XSD 校验、百万文件索引解析，均通过。VIP 使用隔离网络中的真实内核、HTTP 和 ARP，控制日志提交按用例模拟；本轮不访问磁带。结果、资源测量、清理核验及复用步骤见[验收摘要](../.scratch/ltfs-ha/reports/ignored-four-physical-20261020-summary.html)。默认测试的环境门控保持不变。

Holo 端到端实测已覆盖 fsync、覆盖、删除、旧句柄读取、mv、Range 和计划接管；EE 删除/覆盖现场对照与物理带库验证仍未完成。详见[现场记录](../.scratch/ltfs-ha/holo-file-client-20260924.md)。


## 元数据兼容性

目录日志条目增加可选 metadata 字段，SQLite 的 files/staging 表按需增加同名 TEXT 列。新版本可以解码旧日志、迁移旧库，已有版本号保留；原来未缓存的修改时间和 xattr 保持未知，直到从磁带索引重新形成目录记录。迁移不会主动装带、改写磁带或强制刷新全部旧记录。

建议整组节点统一升级后使用这些字段，不承诺新旧二进制混跑或回退时的元数据完整性。Rust 的 FileStat/FileRec 增加字段，直接构造这些结构的调用方需要更新。传输与缓存保留原索引属性；回收复制已保留源文件的时间、只读标记和原始自定义 xattr（包括 Base64 标记）；新副本分配新的卷内 UID 和版本号，重新计算内容哈希。普通上传替换仍生成新时间，并从当前目录版本继承原 xattr（包括二进制属性），内容哈希和版本号重新生成；跨带覆盖同样适用。目录不可读或旧记录缺少 xattr 元数据时拒绝覆盖，需先从磁带重新对账，不能把未知属性当成空属性。不能通过客户端注入回收元数据。


## 工具兼容性早期实验（后续页缓存版本已取代）

本机 Linux 6.8.0-124-generic、GNU coreutils mv 9.4 下，显式内核测试已验证：只读共享 mmap 读取正确；读写混合打开返回 EOPNOTSUPP；mv 经 EXDEV 回退后目标内容一致、源路径删除；目标暂存返回 ENOSPC 时 mv 失败并保留源文件。测试使用内存后端，不能代替集群或磁带验证。

mv 的复制和删除不是原子操作，目标 close 成功仍只表示暂存。测试显式模拟后台提交后再检查目标内容；需要归档保证的应用应使用带 fsync/提交确认的工作流。

内核能力依据：[Linux FUSE I/O 文档](https://docs.kernel.org/filesystems/fuse/fuse-io.html)。旧内核或发行版回移情况需实际协商确认，不能仅按版本号推定。

实验室 Rocky Linux 5.14.0-427.20.1.el9_4、GNU mv 8.32 下：普通 FUSE 读写和 mv 回退通过，但只读共享 mmap 返回 ENODEV；mv 会提示无法保留权限和时间。不能将本机 Linux 6.8 的 mmap 通过结果视为所有目标系统可用。实验室使用 `fusermount -u` 卸载。

## 页缓存版本的可见性边界

2026-09-24 的改造对齐现场 LE 的同挂载行为，取代此前每读句柄独立快照的设计。写入和截断共享本地文件；unlink 后文件由已有句柄/映射保持，最后引用释放后删除本地缓存；仅暂存的上传额外保留本地副本，直到后续打开确认已提交、被替代或消失，或挂载结束。没有后续访问时，该副本可能占用磁盘直到卸载。顺序写和 O_WRONLY 限制已在后续 LE 对齐中解除；共享可写 mmap 不在支持承诺内。仅 write-through 的普通写路径开放，归档确认仍通过写句柄 fsync 等待服务端提交，比 LE 的普通 fsync 提供更强的明确保证。

已有本地引用时，新读 open 会检查服务端已提交内容；如与当前缓存不符，返回 ESTALE，避免在同一 inode 混入不同内容。关闭该文件所有句柄并解除映射后重新打开，才重新下载。对于仅有服务端在途版本的本地文件，本挂载可继续读暂存内容。跨客户端没有实时缓存一致性或自动刷新承诺；已打开句柄不会后台查询远端变化。没有 SHA256 的旧记录用完整 stat 前后核对，内容证明能力弱于具有哈希的记录。

首次下载有哈希的内容必须匹配打开前的长度和 SHA256；不匹配返回 EAGAIN。内核页缓存之外仍保留本地磁盘文件作为缺页/预读来源，当前不提供跨挂载缓存复用或常驻缓存淘汰服务。既有 direct I/O 版本在 Rocky 5.14 上的 ENODEV 记录保留为历史证据，新版本须单独验收。

页缓存版本已在 Rocky Linux 5.14.0-427.20.1.el9_4 上通过 Holo 实测：共享只读 mmap、写入在 fsync 前可见、截断影响旧句柄/映射、删除重建隔离、关闭 fd 后映射读取均通过。外部 ltfsctl 覆盖时新 open 返回 ESTALE，原映射保持一致；关闭全部引用后重开读到新内容。旧版 ENODEV 不再适用于此版本。


本机内核另有两项显式挂载回归：远端在 LOOKUP 与 OPEN 之间变长/变短后，逐块读取仍得到完整新内容；已有只读映射时外部替换触发新 open 的 ESTALE，旧映射保留内容，全部释放后重开刷新。未保留实验性的“首次 OPEN 与上次属性长度不同即拒绝”限制，因为移除该限制的实际挂载测试同样通过；这不代表提供跨客户端实时一致性。两项测试使用内存后端，尚未在 Rocky 5.14 重跑。


LE 后续能力实测及实现进度见 [LE/Holo 能力矩阵](../.scratch/ltfs-ha/le-capabilities-holo-20260924.md)。随机写、原生目录、同写入卷 rename、可写 xattr 和符号链接已实现并部署。LE 权限/时间语义已接通，现场验收状态见交接入口；user.ltfs.sync 已隔离验收，普通 fsync 保留更强归档确认，不能称为完全相同的 LE 同步语义。

### LTFS 索引持久化（2.5.1）

服务端上传和删除批次使用标准 DP 增量索引（XML 格式版本 2.5.0）。连续最多 5 份增量后，下一次有变更的提交生成 DP/IP 全量检查点；更小的恢复链预算会进一步缩短此间隔。正常停机和换出写入带前也生成全量检查点。全量只重写索引，不复制文件数据。

提交成功仍要求磁带持久化屏障及 VCI 更新完成；202 仍仅表示节点暂存。恢复从 Full 顺序应用依赖增量，缺链不得当作最新完整视图。旧版 LTFS 工具对异常增量尾部的恢复不在兼容性保证内；清洁 Full 与现场 LE 的互操作仍需独立验证。


## 持久化目录接口

Rust `Client::mkdir(path)` / `Client::rmdir(path)` 和 FUSE 的目录操作使用 `POST` / `DELETE /directories/<path>`。201 表示索引已提交；202 表示仍在暂存，客户端返回 `Indeterminate`。请求或提交响应丢失时不自动重放；只有服务端明确答复任务未落带时才允许重试。调用方需查询当前状态后决定如何处理结果未定，不能把“目录存在”直接等同于自己的创建成功。

目录仍是标准 LTFS 目录节点，使用 `tapers.version` 排序，删除通过带 `tapers.deletedKind=directory` 的版本化墓碑发布；普通文件列表不混入目录，`list_dir` 和 `stat` 可读目录，后者通过 `metadata.kind=directory` 区分。回收会搬迁空目录、时间、只读标记和原始 xattr，并将目录纳入格式化前的两道检查；目录不会作为零长度普通文件复制。

目录记录复用 SQLite 的 metadata 字段，不新增表或生产依赖；旧程序不能正确解释目录类型，必须统一升级所有 ltfsd 节点和客户端。旧介质在重新装载并对账后才补入原生目录记录，即使索引代数相同也会刷新。升级后应先完成相关磁带对账，再执行目录写操作；未装载的旧磁带空目录不会凭空出现在缓存中。目录删除与文件删除一样依赖池内版本/墓碑：单独导出旧带不能反映其他带上的删除。

本次目录功能通过三节点模拟集群的创建/删除、跨带旧索引对账、接管和空目录回收测试，以及本机真实 FUSE 卸载/重挂载测试（内存后端）。后续 Holo/LE 往返和正式部署已完成，见下文 rename 验收记录。

## 同写入卷 rename

`Client::rename(from, to, no_replace)` 对应 `POST /rename?from=<编码路径>&to=<编码路径>&noreplace=0|1`。201 committed 才确认源删除和目标出现已由同一索引提交；202、504或响应丢失返回 Indeterminate，不能盲目重放。早期仅支持目录的服务端没有此接口；现正式版本已包含。本阶段不支持跨带原子 rename、RENAME_EXCHANGE 或 WHITEOUT。

源子树与目标同时预留，拒绝相关在途上传、删除和回收；允许替换文件或空目录，禁止类型不一致、目标父目录缺失及源目标互为祖先。旧源路径逐项写入版本化墓碑，目标保留原生节点，沿用增量提交和正常卸载 Full 检查点。引用已有数据不消耗第二份文件容量。

FUSE 改名保持源 inode 和目录下已知子 inode；打开的源写句柄先提交脏数据，改名成功后继续写入新路径。已打开的被替换目标仍可访问缓存，后续 flush/fsync 不会覆盖新目标。路径操作在改名期间串行，已有数据句柄通过句柄锁协调；close后尚未确认的暂存上传可使改名返回 EBUSY。

若改名返回 EIO（包括提交结果未定），当前挂载停止后续路径访问及写入，已有句柄仍可读缓存；使用独立 Client 核对后重新挂载。这样不会在结果未定后向旧名字继续上传。本机 FUSE 和三节点模拟验证不代表已经完成 Holo/LE rename 互操作。

LE 改回曾删除或改名的路径时可能保留 tape-rs 私有墓碑。恢复时同一索引内的原生文件/目录优先于旧墓碑，保留节点自己的版本号；跨磁带版本比较规则不变。此行为已通过 [Holo/LE rename 往返验收](../.scratch/ltfs-ha/holo-rename-20260924.md)，原集群尚未升级 rename 版本。

2026-09-24 后续部署：原 Holo 三节点已统一升级上述 rename/LE 修复版本，实际验证目录与文件改名、打开 writer、正常重启接管和 mmap 读回。正式程序位于 `/home/rocky/tape-rs-rename-prod-20260924`，详见 [部署验收记录](../.scratch/ltfs-ha/holo-rename-upgrade-20260924.md)。跨带 rename 仍返回 EXDEV。

读取故障切换修复（尚未部署）：服务端读到设备预留丢失时关闭旧执行者，并在任何成功内容发送前返回503；现有Client可自动换节点重读。普通500读取错误仍返回失败；`get_to`若已写入部分内容后发生网络错误，不自动拼接第二次响应，调用方须丢弃部分输出再重试。详见 [诊断与验证记录](../.scratch/ltfs-ha/client-read-failover-20260924.md)。

后续 [隔离 Holo 验收](../.scratch/ltfs-ha/holo-read-failover-20260924.md) 已实际触发旧执行者的06/2A05及HTTP503，保留旧Leader缓存的get/get_to/get_range均成功自动切换。修复仍待部署原集群。

2026-09-24 部署更新：读取故障切换修复已统一部署原Holo三节点，服务端目录 `/home/rocky/tape-rs-read-prod-20260924`；现有Client和FUSE保持原版本，原9文件读取与mmap验收通过。详见 [上线记录](../.scratch/ltfs-ha/holo-read-upgrade-20260924.md)。


### 可写用户扩展属性（已部署并通过原集群验收）

Client::setxattr(path, name, value, flags) 与 removexattr(path, name) 已接通 HTTP 和 FUSE。仅支持当前写入卷上的文件与原生目录；其他卷返回 EXDEV。flags=0 表示创建或替换，1（CREATE）遇到已有属性返回 EEXIST，2（REPLACE）以及删除不存在的属性返回 ENODATA。支持空值和二进制值，单值最多 65536 字节，属性名最多 255 字节。

属性名必须属于 user. 命名空间；user.tape.*、user.tapers.*、user.ltfs.* 保留，不能作为普通属性写入或删除。唯一控制例外是挂载根上的 user.ltfs.sync（见下）。拒绝 user.user.* 别名；合成根目录不支持普通属性修改。索引值使用 Base64，元数据通过现有 LTFS 增量索引提交，不重写文件内容，保留 UID、extent、mtime 和内容哈希。正常停机的完整索引策略不变。

HTTP POST /xattrs?path=…&name=…&flags=… 的请求体是原始字节，DELETE 同一路径删除属性。201 才代表提交成功；响应丢失或提交未定返回 Indeterminate，不自动重放。FUSE 修改前同步同一文件的脏句柄，提交结果不确定后阻止继续写回。后续覆盖文件内容继续保留已提交的用户属性。新增接口需要服务端同步升级，旧服务端不支持。

模拟集群已验证二进制/空值/64 KiB、目录属性、CREATE/REPLACE/删除、原生数据布局不变、后续内容覆盖及 Leader 接管；新增设置/删除已通过实际内核调用及隔离 Holo/LE 双向回写，包含 LE 2.4.0 降级后的替换、创建、删除和空值读回。空 Base64 值必须输出自闭合 XML 元素，已修复并由 IBM 解析器及实际挂载验证；详见 [验收报告](../.scratch/ltfs-ha/holo-writable-xattr-20260924.md)。

原三节点已统一部署可写 xattr 与空值 XML 修复，实际 FUSE 专用目录写入、计划停机接管、全新挂载读回及清理均通过；原文件校验一致，详见[部署记录](../.scratch/ltfs-ha/holo-writable-xattr-upgrade-20260924.md)。

隔离 Holo 已验证属性提交中断以及设置/删除已提交但响应丢失：Client 返回 Indeterminate，恢复后分别保留旧值或已提交的新值/删除状态，没有重放。实际中断点为周期性 Full 索引数据写入后、结束文件标记前；不代表全部 Incremental 阶段或存储断电已覆盖。[故障验收记录](../.scratch/ltfs-ha/holo-xattr-faults-20260925.md)。

独立 Incremental 索引也已补测：确认增量 XML 数据写入后、结束文件标记前终止隔离 Leader，恢复排除未完成增量并保留旧属性，收尾后可继续提交。[增量截断验收](../.scratch/ltfs-ha/holo-incremental-cut-20260925.md)。这仍是进程崩溃测试，不是存储断电。

“索引已提交、目录未发布”窗口已通过隔离Holo验证：磁带gen72/目录gen71时中断，Client返回Indeterminate；新Leader按磁带对账补齐三节点目录，保留原提交版本及新属性，没有重放。[目录发布前故障验收](../.scratch/ltfs-ha/holo-publication-cut-20260925.md)。


已有 LTFS 符号链接读取（2026-09-25）：服务端对链接记录增加 `metadata.kind="symlink"` 和 `metadata.symlink`，普通文件元数据不变，HTTP stat 的 length仍是LTFS链接记录长度；FUSE lstat的size为目标字符串字节数。升级服务端并重挂介质对账后，新FUSE才能识别链接；旧服务端没有这些字段时不能提供这项能力。相对路径、链接链、目录链接、悬空和循环由内核解析。链接创建、硬链接及链接写操作的完整LE对齐仍不在本次验收范围。目录打开为非目录项补查属性以得到准确d_type，因此大目录会增加元数据请求；本轮未测大目录性能。修复版已在隔离Holo的LE Full85验证，并于2026-09-25统一部署原集群；正式服务端及客户端目录为/home/rocky/tape-rs-symlink-prod-20260925。

符号链接创建（2026-09-25，隔离验收完成，尚未正式部署）：`Client::symlink(target, path)` 对应 `POST /symlinks?path=...&target=...`，FUSE 支持 `symlink(2)`。父目录必须存在，已有路径返回 EEXIST；目标原样保存，允许相对、绝对、悬空和循环目标，空目标及 XML 非法字符返回 EINVAL。成功表示原生 LTFS 索引已提交；响应丢失返回 Indeterminate，不自动重放。链接改名、删除及通过链接写入目标已在隔离 Holo 验证，LE 能读取新建链接；硬链接仍不支持。新客户端需配套新服务端；旧正式服务端没有创建端点。详见[创建验收](../.scratch/ltfs-ha/holo-symlink-create-20260925.md)。

2026-09-25部署更新：符号链接创建版已统一发布到原三节点及tsclient，正式程序目录 `/home/rocky/tape-rs-symlink-create-prod-20260925`。实际FUSE创建、链接写入、改名删除、计划接管后全新缓存读回及清理通过，原9文件校验一致；取代上文“尚未正式部署”。见[部署验收](../.scratch/ltfs-ha/holo-symlink-create-upgrade-20260925.md)。

符号链接跨带回收修复（2026-09-25，尚未正式部署）：回收直接迁移链接节点，不解析或读取目标，保留链接目标、时间、只读标记及属性，并在目标卷分配UID。悬空、循环和目录链接不再因目标不可读而中止回收。模拟集群及两盘专用Holo虚拟带完成回收、源带重新格式化、全新FUSE缓存和接管读回；详见[验收记录](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md)。

2026-09-27 部署更新：回收修复已统一部署三节点，配套客户端位于 `/home/rocky/tape-rs-symlink-reclaim-prod-20260927`，取代上文“尚未正式部署”。实际FUSE链接操作、计划接管后冷缓存读回、清理及原文件校验通过；本轮未在原数据带执行回收。见[部署验收](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md)。

目录枚举使用服务器返回的子项类型快照，避免为每个文件单独查询属性；符号链接按链接自身类型返回。旧服务器没有 `is_symlink` 字段时客户端回退到逐项查询。该字段只扩展目录响应，不改变磁带索引或 catalog 持久化格式；目录打开后仍按该次快照分页，后续 lookup/getattr 查询当前状态。


显式同步：向 **tape-fuse 挂载根**（例如 `/mnt/tape`，不是其下普通目录 `/mnt/tape/sr`）的 `user.ltfs.sync` 写入任意值或空值，成功表示本挂载已有写句柄的内容、已关闭暂存内容得到确认，并完成当前服务卷的完整索引检查点。后端还覆盖同步执行时已完成接收的上传批次；其他客户端尚未完成的上传、随后到来的写入及尚未同步到服务端的本地缓存不属于此次保证。切换走的旧写入卷已按既有流程做检查点；不扫描或改写库中所有介质。

```python
os.setxattr('/mnt/tape', 'user.ltfs.sync', b'1')
```

该控制属性只写、不列举、不存入索引；读取返回 ENODATA，删除返回 EPERM，写在文件或子目录上返回 EACCES。CREATE/REPLACE 均允许；空闲且索引已完整时不会增加代数。调用期间本挂载已有writer被串行化；失败或响应丢失可能已有部分写入完成，调用者需核对，不自动跨节点重放或声称成功。Rust客户端对应 `Client::sync()`；HTTP `POST /sync` 同样只同步服务端已完成接收的写入。旧服务器不支持该操作，应先升级服务器。

现有 fsync 仍提供文件内容与索引归档确认，比 IBM LE 仅刷新数据的 fsync 保证更强；这里没有把暂存成功改称持久化成功。

硬链接不支持：LTFS 2.5.1 没有标准化的共享inode硬链接表示，tape-fuse保留EOPNOTSUPP，目标不会被创建。IBM LE 2.4.8.3实测返回ENOSYS；不以复制或符号链接替代硬链接。符号链接保持现有支持。


## 原生权限与时间修改

FUSE chmod/fchmod 将任意写位映射为 LTFS readonly=false；没有写位则为 true。文件基准模式保持0644，目录0755；readonly清除全部写位，符号链接仍报告0777。其他 mode 位不写入索引。chown/fchown/lchown 成功但不改变挂载属主 uid/gid，与 LE 的挂载所有权模型一致。创建时的 mode 仍不作为逐文件 POSIX 权限保存；需要只读标志时显式 chmod。合成挂载根不支持原生属性修改。

utime/utimes/utimensat/futimens 的显式 atime/mtime 使用标准 LTFS 纳秒 UTC 字段持久化，未指定字段保留；change_time由服务端更新，creation_time/backup_time/UID/extent/哈希保持。不会因读操作自动提交 atime。FUSE只读标志由内核 default_permissions 检查，root及已打开写句柄仍按Linux权限语义处理；HTTP/Rust接口不提供逐用户POSIX权限检查。

Rust Client::change_metadata(path, MetadataChange) 对应 POST /metadata?path=...，JSON仅接受readonly、access_time、modify_time，可省略或null。时间必须为四位年份、九位小数的UTC规范格式；空修改、未知字段、错误类型和非法时间拒绝。只支持当前写入卷原生文件/目录/链接；跨卷EXDEV，缺失路径ENOENT，在途冲突EBUSY。FUSE修改前同步已有writer；201才表示索引提交，结果未定或响应丢失不重放。

文件catalog metadata新增node对象，沿用已有JSON列与日志字段，没有新表、私有磁带格式或依赖。旧记录在装带对账后补齐；新接口需服务端与客户端统一升级。Attr新增只读与时间字段，直接构造该Rust结构的调用方需适配。内容覆盖保留已知readonly标志，时间仍遵循内容写入规则。

### 多带库的固定入口

`ltfsd --libraries-config` 启用同一 Raft 集群的多库管理。客户端访问固定 VIP，使用 `/libraries/<serial>/files/...`、`/libraries/<serial>/admin/...` 等路径使用真实 changer 序列号选择库；入口将请求转到对应 owner 或控制 Leader。原路径仍访问显式指定的默认库。两库可以由同一节点或不同节点持有，调用方无需跟踪节点。配置、兼容迁移、查询及故障返回语义见 [多带库说明](multi-library.md)。

FUSE 和 `ltfsctl` 可通过 `--library <真实 changer 序列号>` 选择目标库；每个挂载固定一个库。带库不再使用自定义 l8/l9 别名，新配置无需填写 id；多库旧别名状态不兼容。两个挂载的完整示例及配置见[多带库说明](multi-library.md#fuse-与命令行选库)。

### 跨库普通文件内容复制

`ltfsctl copy` 可从显式源库/池读取一个已提交普通文件，并将内容只创建到显式目标库/池的空路径。例如：

```bash
ltfsctl -e 10.131.7.219:17821 --library 55L3A7802K19LL01 --pool P1 \
  copy /archive.bin /archive.bin --to-library 55L3A78011W9LL01 --to-pool P2 \
  --spool-dir /var/tmp/tape-copy
```

临时目录必须容纳整个文件；临时文件权限为 0600，命令结束后清除。客户端在写目标前校验下载字节的长度、SHA-256 和源版本未变；目标落带后核对目录记录中的长度、SHA-256 和可读原生属性，再从目标流式读回全部字节计算 SHA-256，并复核两端版本。读回失败或不符时保留两端文件并报错，不能把目录哈希相等当作介质读回成功。现代目录记录中的创建/修改/访问/备份时间、只读标记和用户扩展属性随一次目标写带保存；目标生成自己的卷内 UID、版本和内容摘要。旧记录缺少完整属性时只复制内容；属性超过 12 KiB 或格式损坏时拒绝写目标。目标已存在时失败，不覆盖，也不删除源。只创建上传的响应若丢失，即使查询到同内容目标，也返回结果未定：相同哈希不能证明该目标是本次请求创建的。目标已落带而源版本随后变化时返回校验失败，保留目标供人工核对。此命令不建立后续改写自动同步、故障读取或全局副本身份；因此不能代替 EE 式副本或安全迁池。

元数据传输使用新的内部 `X-Tapers-Copy-Metadata` 请求头。所有数据 owner 升级前不要用新客户端执行跨库复制：旧服务可能忽略该头并写下缺失属性的目标，新客户端会在落带后的核验阶段拒绝把它报告为成功；此时源仍保留，目标须人工核对。

源记录必须包含可判定普通文件类型的元数据和 SHA-256；旧目录记录缺这些字段时须先装带对账。

`Client::delete_if_version` 和 `ltfsctl delete --if-version round.sequence` 只在路径当前版本仍与参数一致时提交删除墓碑；版本可从 `ltfsctl stat` 读取，版本变化返回 412，源内容不变。`If-Match` 请求头必须是双引号括起的 `round.sequence`。条件检查与路径预留在同一次写入准入中完成；结果未定时仍需查询路径和任务，不自动重发。它是后续迁池删除源的必要闸门，目前没有自动迁池命令。
