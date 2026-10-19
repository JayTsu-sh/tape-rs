# 多卷逻辑库（Rust 库与 FUSE）

`library::Library` 提供显式 assignment、blacklist、pool/volume 路径解析、库存状态和按需装带。`LibraryBackend` 将同一实现接入 `TapeFs`；`tape-fuse --endpoint` 原有集群模式继续可用。

新模式是独占设备的单进程逻辑库，资源 SQLite 与现有 ltfsd 的 Raft/catalog 分开。它不是第二个 HA 控制面；不得与 LE、ltfsd 或其他实例同时控制同一设备。资源文件有进程锁，CLI 资源修改在 FUSE 卸载后进行。Rust 调用方可以持有 `LibraryBackend` 的共享锁，在同一实例内查询/管理资源。没有新增生产依赖；基础管理扩展采用新增表，保留旧 assignment 数据。升级前保存资源库，关闭旧进程后再用新版打开；旧版不执行新增池/离线/驱动约束，不支持运行中混用新旧控制器。

## 路径与状态

FUSE 路径为 `/池名/条码/卷内路径`。根目录列池，池目录列显式分配卷，不会因列根目录而自动装所有磁带。首次访问卷内路径才触发调度。`Library::resolve`、`pools`、`volumes`、`status`、`read_to`、`write_from` 是对应 Rust 接口；只读检查使用 `inspect_volume`（不可变卷引用），目录/xattr/元数据更新使用 `with_volume`，完成提交后才返回成功。

`tape-library` 提供 `status`、`assign POOL BARCODE`、`unassign BARCODE`、`blacklist BARCODE REASON`、`clear-blacklist BARCODE`、`read PATH OUTPUT`、`list PATH`。资源模型保留 pool、home storage 地址、blacklist 原因、调度阶段和失败前阶段/原始错误。`NEED_ASSIGN` 卷不允许访问，`BLACKLISTED` 卷不允许装载，无法识别的卷记录 `NON_SUPPORTED_TAPE`。它提供本项目管理面，不实现 leadm 的线协议。

## 基础管理扩展（不含 HA / HSM）

沿用 `tape-library`，不安装或替换 IBM 的 `eeadm`。以下命令仍需同一组显式设备/资源库参数。

| 命令 | 行为与边界 |
|---|---|
| `library list` / `library show` / `library rescan` | 查询唯一绑定库；rescan重新读取SCSI库存，不发送INITIALIZE ELEMENT STATUS，不扫描所有卷内文件 |
| `pool create NAME` / `pool delete NAME` | 显式空池实体、稳定UUID；重复创建、删除非空池、未知池均拒绝，不写介质 |
| `pool list` / `pool show NAME` | 列出显式池与旧assignment隐式池，真实统计分配卷数；不伪造容量 |
| `pool set NAME poolname NEW` | 事务改名并更新assignment，UUID不变；同名冲突不合并 |
| `pool set NAME mediarestriction L9` | 后续assignment校验六字符前缀+介质后缀；已有卷不匹配则拒绝修改。none不限制旧条码；不从条码猜驱动物理密度 |
| `pool set NAME mountlimit N` | 0..32767，0不限；新挂载超限时先归槽本实例已持有的本池旧卷，不驱逐外来实例卷 |
| `tape list` / `tape show BARCODE` | 库存与分配/离线/失败状态，不装带 |
| `tape offline BARCODE MESSAGE` | 保留assignment，安全归槽本实例卷后禁止访问；消息1..63 ASCII字母数字、空格、下划线、横线，不写EE/GPFS属性 |
| `tape online BARCODE` | 实际装带、恢复解析LTFS索引后才清离线标记；验证失败继续离线。不是全带内容校验 |
| `drive list` / `drive show SERIAL` | 显示配置、占用及调度资格 |
| `drive down SERIAL` / `drive up SERIAL` | 持久启停调度资格，不是断电/复位；down只归槽本实例持有卷，拒绝隐式抢占 |
| `write PATH INPUT [--overwrite]` | 追加普通LTFS文件并完整commit，默认拒绝覆盖；不格式化。拒绝写`.LTFSEE_DATA`、EE属性对象或EE对象别名 |

显式池名为1..16个ASCII字母、数字、下划线。旧隐式池仍按assignment存在，最后unassign后消失；对它成功set才转为显式池并获得UUID。新增表为`library_pools`、`library_tape_options`、`library_drive_options`，不修改ltfsd的catalog/状态机。Rust接口包括`pool_create/delete/list/show/set`、`tape_offline/online`、`drive_set_enabled`。

上述查询在新版本FUSE持锁时复用只读缓存入口；库存可能过时，池/离线/资格从同一SQLite事务读取。rescan要求取得资源锁，不在活跃FUSE旁路查询设备。查询输出标明source与observed_at。

这是本项目的直接管理入口，不等于完整 EE。下面的后续扩展补充了角色、节点组、任务日志和本地维护；需要操作 GPFS stub/副本引用的 EE import/export/datamigrate/replace 仍不在此非 HSM 管理器的实现范围。底层机械命令不能直接当成这些事务。离线/上线只实现本项目卷访问资格，不生成 IBM EE 的 GPFS/HSM 元数据。

LTFS 2.5.1 实现与 EE 互通必须分开验收：普通 LTFS 字节/索引互读、未修改 EE 对象保真、新建 EE 对象的语义恢复是三个层次。当前普通写入不维护 EE 对象 ID/objgen/GPFS 映射；不能把通用 xattr 保真或 LE 能读取普通文件当成完整 EE 对象写入已支持。`write_from` 和 `LibraryBackend` 的删除、改名、元数据/xattr 等普通入口拒绝改变 EE 对象、别名以及包含它们的目录；新建 `ibm.ltfsee.*` 属性也被拒绝。显式 `with_volume` 和原始 `LtfsVolume` 仍是低层接口，调用方必须负责协议语义，不应用于普通 EE 对象改写。

## 持久任务、策略与维护扩展

`tape-library` 的设备命令继续显式指定库/驱动/新轮次。非只读命令在资源库中持久登记，任务 ID 输出到 stderr，不污染 JSON 结果。任务是本地前台执行日志，不是 HA 队列，也没有后台派发或崩溃自动续跑。任务记录成功只表示该命令和正常设备收尾均成功；写带还记录完整检查点的 generation/UUID。失败或 interrupted 不代表没有副作用，更不代表可以直接重试。

仅以下任务命令不需要设备参数，且控制器持锁时也能运行：

```sh
tape-library --library-state /isolated/library.sqlite task list
tape-library --library-state /isolated/library.sqlite task show 12
tape-library --library-state /isolated/library.sqlite task cancel 12
tape-library --library-state /isolated/library.sqlite task clearhistory --yes
```

取消是请求，不打断 SCSI 命令。在调度、搬带、挂载结束、文件读取之间的安全点停止；文件追加已经开始后会完成提交阶段，不在 append 与 commit 之间响应取消。晚到的取消请求不能抹掉已确认提交。任务恢复必须取得同一资源库的控制器排他锁；前实例留下的 running 改为 interrupted，保留未知结果而不重放。`clearhistory` 仅删除终态，运行中及等待取消的任务保留，已用任务 ID 不复用。清掉的日志不可恢复，需要留证时先保存任务结果。

| 命令 | 行为与边界 |
|---|---|
| `drive role SERIAL mrg` | 设置 m 写、r 读、g 维护；真实候选筛选执行约束，缺合适驱动时在搬带前拒绝 |
| `nodegroup create/list/show/delete` | 管理本地分组，默认 G0；删除被驱动、池或本地成员引用的组会拒绝 |
| `nodegroup local NAME` | 指定本地控制器所属组；`-` 停用本地组调度，不创建远端 EE 节点 |
| `drive assign SERIAL GROUP` | 分配本地驱动；`-` 解除分配；池、驱动、本地成员三者的组必须匹配才可调度 |
| `pool set NAME nodegroup GROUP` | 约束这个池可用的本地驱动组；池改名保留组关系 |
| `pool set NAME lowspacewarningenable yes/no` / `nospacewarningenable yes/no` | 控制基于已观测 DP 剩余字节的本地告警条件；不是 EE 后台告警服务 |
| `pool set NAME lowspacewarningthreshold N` | 0..32767 TiB，0 禁用低空间阈值；所有卷都有有效容量快照才计算，否则返回 null |
| `tape move BARCODE homeslot/ieslot` | 基于当前 element map 和条码核验搬带；到 I/E 必须先 offline；保留 assignment，不等于 EE export |
| `tape validate BARCODE [--full-read]` | g 角色装带检查；full-read 读取所有普通文件，有哈希则比对，无哈希明确报告未验证 checksum |
| `tape reconcile BARCODE` | 从实际磁带刷新本地完整索引快照；失败不替换旧缓存，含未知 XML 拒绝有损保存；不做 GPFS 对账或写带修正 |
| `tape objects BARCODE` | 解释 `.LTFSEE_DATA` 的 `gpfs.objtype/path/target` 为原来的文件、目录、符号链接，保留对象 ID/objgen 不变 |
| `tape restore BARCODE DESTINATION` | 把 EE 保存对象流式恢复为普通文件/目录/原文链接；目标必须是空目录或全新目录，祖先不能为符号链接，拒绝穿越/覆盖/路径冲突 |

恢复不执行保存的链接目标，不读取它们指向的宿主路径。它保留原始保存路径层级和链接文本，不承诺还原权限、时间、GPFS inode、私有 xattr、stub 或 HSM 副本登记。中途磁带/磁盘失败可能留下部分恢复文件，保留作诊断，重试应选择新的空目录。原始语义、别名和 xattr 可由 `ltfs::ee::EeManifest` 查询，原始索引不会因此修改。

新增持久表与原表增量共存，包括 `library_tasks`、角色/节点组关系和 `library_volume_indexes`。缓存是带读取结果，不是修改带的授权；重新分配或换介质后必须重新对账。不能把本地节点组当成跨主机分配、服务接管或 HA 成果。

容量在实际挂载和完整写提交后采样 DP MAM 0x0000/0x0001，必须两个属性都存在且数值有效；缺失、读失败或算术溢出不伪造零。查询标明 `capacity_source=observed_dp_mam` 和观测时间，并不是每次查询都装带。失败访问/下次恢复前使旧观测失效；没有全池观测时，容量和告警条件都明确 unknown/null。它不估算 EE 的 reclaimable/non-appendable 空间，不能代替现场容量规划。

现场 EE 1.3.4 / 自带 LE 2.4.6 已实证接受 DP-only Full sync（IP13/DP14），但直接 mount `ltfsincrementalindex` 返回 `LTFSI5011E Unexpected top tag`。原生增量功能仍保留；交给该 EE 前必须完成双分区 Full 检查点并由目标 LE 验收，不能只把增量卷归槽就称为互通完成。

EE Recovery 还可能追加同一 generation 的 Full 恢复副本。回指链只对所有已建模逻辑字段完全相同的同代 Full 放行，creator/物理位置/回指可不同；仍要求物理向后，不接受同代不同内容、未知元素或增量同代。实际 DP54/57/62 的固定夹具与故障记录已纳入 `ee_recovery_chain` 回归，避免“干净 Full 快速挂载成功、下一次增量恢复却失败”的情况。

FUSE 持有资源锁时，另一个 `tape-library status` 不打开设备，也不争抢控制权；它在一个只读事务内把最近一次完整库存快照与实时 assignment/blacklist/阶段/失败合并，输出 `source=cached_inventory` 和 `observed_at`。资源锁空闲时状态来自当前设备查询，输出 `source=device_query`。缓存状态必须通过已绑定的 changer 序列号核验，不能用于机械决策。

配置的 changer 由 VPD 序列号核验。驱动器参数用 `SERIAL=/dev/sgN`；DTE 从 element map 和库存 DVCID 推导，再与 drive VPD 核对，不能按旧 sg 编号或固定 element 地址猜测。资源库绑定 changer 序列号。

## 调度与持久化

按顺序执行：核验 assignment/home slot → 确认 changer 与 drive 的 PR → 必要时将本库已分配介质归槽 → 重新查库存 → MOVE → 核对目标条码 → LOAD → 保留长超时的 TUR ready polling → LTFS 恢复 → 操作。优先沿用已有目标卷，其次选空驱动器，最后才驱逐本库分配卷；未知/未分配介质不搬动。

changer 不支持 PR 时安全拒绝自动调度，不把文件锁冒充跨主机隔离。模拟器的默认 changer 不支持 PR，因此新测试使用独立模拟 PR transport；这不能证明 TS4300 的 changer PR 行为。

连续同卷访问沿用已验证的 `VolumeSession`；每次仍检查 PR、TUR 和设备 epoch，换带/UA/复位后失效。失去资格后本实例停止，不能重新 fence。写操作完成完整双分区 commit 后才返回成功，直接 FUSE 的 close/upload 因而比原 HTTP 暂存模式更强。正常 FUSE 卸载和 CLI 结束会归槽已访问卷并释放本轮 PR；Rust 调用方必须检查 `close()` 结果。Drop 不执行机械操作，异常终止可能留下装载介质/PR，需要核实现场后重新接续。

IBM append-only 扩展模式下，普通 DP 追加不受影响，但 IP checkpoint 的 opening filemark 可能返回 DATA PROTECT `5A/02`。卷提交在已持有且复核 PR 后读取精确 partition/block，以 `ALLOW OVERWRITE` action 1 仅授权当前位置一次写入；不发送 format 授权，也不关闭 append-only 模式。READ POSITION 的 UA、位置不符、LOLU/PERR、未知模式页和截断响应都安全拒绝。设备不支持该 mode subpage（`24/00`）时沿用标准 LTFS 路径。

跨卷 rename 返回 `EXDEV`。符号链接目标保持 LTFS 索引原文，FUSE 采用 POSIX 路径语义；直接 Rust 读取沿用卷库的单层、卷内相对链接限制。

## 使用

先核实真实节点、介质与设备 holder。以下为参数模板，设备节点与序列号必须替换为本次库存核验值；命令的访问可能执行 PR、MOVE、LOAD 和 UNLOAD，但不会自动格式化。

```sh
cargo build --release --features fuse
# 每条命令都携带同一份显式设备参数，执行轮次由运维单调分配。
ARGS=(--library-changer /dev/sgN --library-changer-serial CHANGER_SERIAL
      --library-drive DRIVE_SERIAL=/dev/sgM --library-state /isolated/library.sqlite
      --library-node 1 --library-round NEW_ROUND)
tape-library "${ARGS[@]}" status
tape-library "${ARGS[@]}" assign test TEST_BARCODE
tape-fuse "${ARGS[@]}" --cache-dir /isolated/cache /isolated/mount
```

Rust 接口使用 `Library::open(changer, Vec<LibraryDrive<D>>, state_path, key)`，transport 可以是 `ScsiDevice` 或测试 transport；没有 FUSE feature 的编译也支持逻辑库。读取示例为 `library.read_to("/test/TEST_BARCODE/file", &mut output)`；写入为 `library.write_from(path, &mut input, create_only)`，结束时检查 `library.close()`。`with_volume` 更新回调失败后不重放写入，失败诊断保留在资源库。

`examples/library_acceptance.rs` 是独立 Rust 库实机验收入口。它对显式给定路径做两轮流式 SHA256 校验，并保存初始/最终库存、耗时、结果及清理错误；不格式化、不写文件、不自动分配介质。两卷路径交替输入、只配置一台 drive 才能验证真实换带，重复读同卷不能替代此项。

`--checkpoint` 只在所有预期文件两轮校验通过后对涉及的卷提交一次 checkpoint，用于明确修复已证明的 IP debt；它不重放文件追加。没有已知 debt 时不要使用。

```sh
cargo run --release --example library_acceptance -- "${ARGS[@]}" \
  --expect /test/TEST_BARCODE/file=EXPECTED_SHA256 \
  --expect /test/SECOND_BARCODE/file=EXPECTED_SHA256 \
  --report /isolated/acceptance.json
```

## 大索引恢复

`LtfsIndex::parse_reader(BufRead)` 共享原有 XML 状态机。恢复只缓存一个磁带记录和最多 512 字节前缀，不再缓存整份原始 XML；前缀与 XML 事件可以跨记录。设备错误保留原类型，解析失败后仍建立构造边界，截断 Full/增量均拒绝发布。

`RecoveryBudget::max_index_bytes` 保留为整个构造的 I/O 上限，默认由 64 MiB 调整为 8 GiB，显式较小预算仍会返回受限；反向步进与历史深度预算不变。文件树、单个 XML 事件及增量重放仍使用内存，内存随文件/属性数量增长；不声称百万文件挂载具有固定内存占用。65 MiB 构造回归与分段解析已覆盖，但历史百万文件卷已重格式化，实机验收必须重新建立大索引夹具。

软件基线中的显式百万文件流式解析测试会生成并解析一百万个文件元素，核对末尾节点、UID、名称和时间；它不访问硬件，也不等同于百万文件实机性能。真实介质验收应分别记录索引字节数、文件/属性数量、旧预算失败和新实现读回结果。

### 合批同步与增量索引选择

守护进程复用已挂载的写卷会话，同卷已就绪上传共享一次 `LtfsVolume::sync_batch()`。增量索引确实少占至少一个磁带索引块时使用增量；Full 占用块数不多于增量时直接写 DP Full，避免小索引产生无收益的恢复链及卸载前额外 DP Full。比较采用相同 XML 编码器的有界流式计数，不分配整份 Full XML；Full 超过增量块预算后立即停止比较。

增量链仍最多 5 份，并受更小的恢复预算约束；达到上限写 DP Full。已有 DP 基线的普通批次只更新 DP，清洁卸载、换带或计划停机才补齐 IP；从 IP 视图恢复且尚无可用 DP 基线时，沿用原有完整提交路径。`commit_incremental()` 保留显式增量语义，`sync()` 保留显式 DP Full 语义；本次不修改持久化格式或现有客户端接口。文件标记、最终刷新、VCR/双分区 VCI 与 PR 核验不变，文件只在提交成功后被确认为 committed。

合批字节数、文件数、空闲和最长等待条件沿用现有参数，不因选择增量而逐文件强制提交。顺序等待归档成功的客户端只能跨会话节省挂载成本；需要合批时应并发上传或先暂存多个文件再显式同步，暂存完成不能当作磁带持久化完成。

2026-10-19 验证：完整软件回归 390 项通过、2 项忽略；探针测试 3 项通过。单台 TD9 的隔离实测中，小索引的三份 64 MiB 合批选择 72,807 字节 DP Full，写入加同步 10.94 秒，之后只补同代 IP；通过新测试文件属性构造的大索引上，追加 64 MiB 选择 2,181 字节增量，同步 8.94 秒。全卷 64 文件通过存储哈希或外部 SHA-256 实读核验并恢复 EE。两组不是相同起点的倍速对照；大索引夹具不代表百万文件负载，新守护进程构建尚未替换正式服务，也未重新验收完整物理 HA 矩阵。
