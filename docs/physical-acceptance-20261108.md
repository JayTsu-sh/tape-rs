# tape-rs 真实 B 库全功能验证报告

**本轮可安全执行的真实 B 库验证已完成，整体尚不能判定为“全部功能通过”。**

S3/FUSE/FDB数据链路、主要运维动作、通知、正常HA与FDB依赖故障场景通过。原验收发现的旧运维/文件入口、离线说明展示三项缺陷，已在后续 `8bd242a` 修复并实带复验通过，详见文末补验；下方原矩阵保留 `e0a7625` 的历史结果。完整原生reclaim尚未开放，受现场条件限制的项目单独列出。

本报告覆盖当前三节点独立FDB部署及安全可用的真实B库场景。它同时列出不支持、失败和未具备条件的入口，不代表所有构建模式、所有故障边界或全部硬件功能已经通过。Holo/Sim结果与真实TS4300结果分别记录。

## 版本、环境与操作边界

- 源码：`e0a7625b920cad93d884939f5981c2cafeb4b97f`；tree `2b6cd858d2c3dc9a5f1197e0c490afe2fffe22d4`。普通EL9/glibc2.34 release，207/208/209逐机部署ELF SHA核对；本轮没有通过提高生产栈、改硬件超时或故障专用版本制造通过。
- 主机：10.131.7.207、10.131.7.208、10.131.7.209。B库55L3A78011W9LL01；驱动117018605B、117018606F。A库保持释放。B changer6070别名禁止访问，原白名单中的HH8 drive6070仍允许。
- 介质：只写L00284L8、L00285L8，卷UUID分别643ea0be-8914-45f0-8afa-be339fcbb8f9、17f487c8-7a18-4b47-9d65-5bb500b62a40；两盘都有需保留的live/retained数据。未执行FORMAT、ERASE或绕过受管流程修改PR。
- FDB：独立三节点45153、double，原namespace c51a94dd-7e84-4167-bd25-7151880eb628、原state和unit保持；未操作45151/45152。元数据/异步任务/订阅进度保留于FDB，正文在磁带。
- 部署目录：`/opt/tape-rs/v1-20261108-e0a7625b920c`；服务 `tape-rs-v1-20261106.service`；Raft7430、管理HTTP7431、S37432。池native6：02004aa0-c2f6-40a5-9849-40f4fc78d196。
- 此三节点候选未配置HTTP VIP；物理IP入口的HA不能代替VIP漂移验收。项目进程挂载命名空间无GPFS；207/208宿主机有其他GPFS进程，未停用，也未将其视为项目依赖；本轮未运行EE互通实验。
- 测时采用请求控制端monotonic；SSH只读测得三台物理机相对控制端约+16.25秒（约0.3秒往返界），三机测得偏移相近。时间戳保留原时钟域，任务耗时不与控制端墙钟混算。
- 升级采用整组正常停启，原权威状态摘要保持、停服后队列及PR-IN空。不重建namespace、不清除原门闩，不重跑已消费的变更controller；未知结果按同一原身份结清。

| 产物 | SHA-256 |
| --- | --- |
| ltfsd | `ae4e658266e63de03e200e52ce42c32bdd19c6ea5784157568cad229b572d8c2` |
| ltfsctl | `cdbd154977f549ec75cf0bcd0d0cdc87b52150129e4c72395bfa17730c70f5eb` |
| tape-fuse | `28fa3dee2758666f6172aa96cf10820b4aafd6b2037dbcc491c6956851650675` |

## 功能矩阵

| 功能 | 入口 | 结果 | 实际范围与限制 |
| --- | --- | --- | --- |
| library list/show、pool list/show、tape list/show | CLI、HTTP三节点 | 通过 | 共享FDB投影；nodes/active_node及真实库存 |
| pool create/set/delete | CLI、显式HTTP | 通过 | 空池CRUD、策略原请求重放、S3 bucket联动；测试空池已删除 |
| library rescan、task list/show | CLI、显式HTTP | 通过 | 真实库存；异步原任务终态，不把202当完成 |
| S3列举 | S3 | 通过 | 全局UTF-8字典序/1000+5分页/跨节点token/V1 marker/start-after/delimiter/encoding/非法token |
| S3普通对象读写 | S3 | 通过 | PUT/覆盖/空/中文/元数据，GET/HEAD/Range/ETag与时间条件优先级；错误MD5/SHA/CRC未发布 |
| RenameObject | S3 | 通过 | 目标条件、原ClientToken跨节点重放、改参数拒绝；该入口是本项目S3扩展 |
| 不支持的S3请求 | S3 | 通过 | Multipart明确405/XML；CopyObject、ACL、IfRange等明确拒绝，不能记为功能支持 |
| 通知订阅、扩类重连、显式ACK | CLI、HTTP SSE | 通过 | 原类型积压保留，新类型只收扩类后的事件；断线/跨节点保持同一pending，CLI watch不自动ACK；本组mock事件显式injected |
| 真实内核FUSE与S3互读 | tape-fuse、S3 | 通过 | 阶段14在最终e0独立新文件复验创建/读写/fsync/mmap/随机覆盖/append/truncate/多句柄及属性；阶段4在同e0完成文件/目录改名、unlink/recreate、symlink、hardlink拒绝；两阶段均空缓存新journal重挂 |
| FUSE未确认改名恢复 | 真实内核FUSE | 通过 | 真实EIO后root user.ltfs.sync可达，原intent/prepared身份保持，结清同一请求，无第二次rename |
| tape offline/online/set/validate/move | CLI、HTTP管理链路 | 通过 | L84策略还原，实际标签/索引校验、I/E往返原槽；validate不等于全部正文校验 |
| 已分配磁带再次import | L84 CLI、L85显式HTTP | 通过 | 刷新FDB索引、原池及卷UUID保持，已有S3/FUSE内容仍可读；首次导入的目录映射分支本轮未重验 |
| 分配/解除分配的引用保护 | CLI | 通过 | 已有引用unassign失败，已分配assign拒绝；不把拒绝用例算成正向初始化/解除分配 |
| import期间池间元数据隔离 | HTTP S3 | 通过 | native6 HEAD/List 503；ee6 List200。仅查元数据，不装载或读取受保护介质 |
| 磁带原生监控与真实事件 | 管理查询、notify | 通过 | 真实策略/机械位置变化产生非注入state/location事件；MAM容量/WP和TapeAlert正常采样，不调用EE |
| 同库管理互斥与正常HA | CLI、HTTP、真实服务 | 通过 | 原零子父占用跨owner保持，原请求排队/取消，旧owner被拒；原handle关闭后按原身份完成；未开放reclaim完整流程 |
| 未ACK通知跨HA | HTTP通知 | 通过 | 同consumer/generation/delivery/pending保留，接管后按原批次ACK |
| 原生FDB文件CLI | ltfsctl fdb | 通过 | prepare/upload/status/details/原回执重放；stat/list/children/get/不覆盖本地输出/attributes；rename/fs原请求run/status/终态cancel |
| S3 DeleteObject | S3 | 通过 | 正确IfMatch删除204、HEAD404、重复删除不存在对象204；目录/历史介质空间不据此立即回收 |
| 驱动策略 | ltfsctl fdb drive | 通过 | 两驱动查询，HH8 role mrg→mr→原值；enabled=false及硬件故障分支未覆盖 |
| FDB单节点故障 | 45153专用服务 | 通过 | 真实停止209专用FDB，剩余多数派仍可实际PUT/GET；恢复同unit/原数据 |
| FDB全体故障及恢复 | 45153专用服务 | 通过 | 停止三个专用unit，新数据请求不虚报成功；恢复后原被拒key404，旧正文及同一未ACK通知保持，14类占用空 |
| 大对象顺序S3 | S3 | 通过 | 3×64MiB随机正文（os.urandom生成同一64MiB文件，按三个对象写入；未用零块/稀疏文件，未测硬件压缩比），按持久实际写入序读取，独立SHA，另测反向1MiB Range |
| 官方可选顺序客户端工具 | tools/s3-sequential-io.py | 通过 | 另3×64MiB，持久intent/commit/footer清单，跨入口读取按原顺序，ETag和完整SHA核验，无自动重试 |
| 并发小对象与合批观测 | S3 | 通过 | 6×2KiB并发PUT并独立读回；是否同批提交按实际CDB观测单独判断 |
| CLI表格和监控连续观察 | CLI、HTTP | 通过 | 真实输出保留；额外旧入口只读观察与正常采样快照，帮助文本不算硬件功能通过 |
| 旧drive list/node list/cluster | 旧CLI入口 | 未通过 | drive list、node list为404；cluster退出0但输出null；原生fdb drive show不能替代这些入口通过 |
| 旧stat/list/get文件入口 | 旧CLI入口 | 未通过 | 已知存在的最终S3对象：stat退出0却打印不存在，list/get报不存在；失败get在新输出路径留下0B文件。当前fdb stat/get已独立通过 |
| offline -o说明的查询展示 | CLI、HTTP tape show | 未通过 | 离线任务回执含main1108 acceptance，但同轮tape show的offline_message为null；离线状态本身正确。需修复查询投影，不能宣称说明字段已展示 |
| 空闲读挂载自动退带 | 真实UNLOAD/MOVE、管理就绪探针 | 通过 | 默认read_idle_ms=300000，闲置后L84自动回原槽1004；机械活动期间readyz暂503，同owner恢复200，无重启/清锁。 |
| 完整原生reclaim | 公开CLI/HTTP | 未实现/封闭 | 内部零子父、组件软件回归和历史SQLite格式化回执不是完整迁移/源覆盖/接管恢复验收 |

## 正常监控观测

最终候选阶段12的三次独立查询中，库存观测与装载L84的MAM/TapeAlert样本均继续前进；最后两次库存样本时间差30.131秒、L84样本差30.131秒。样本WP=false、活动alerts为空。L84的alert_history含49属于历史记录，不能当作当前活动告警。未装载的L85继续保留上次13:27:34Z样本，不冒称持续读取盒内MAM；长import期间样本可能变旧，需按observed_at/last_success判断新鲜度。此项证明正常采样及真实策略/位置事件，不证明坏带/写保护等现场故障正例。

## 空闲自动退带与就绪探针

首次收尾快照为三节点ready；随后复查捕获暂时503。复核确认是已存在的自动退带行为：当前运行进程未覆盖`--read-idle-ms`，绑定源码默认300000毫秒。L84空闲后，owner2在14:03:44Z完成UNLOAD（24.237秒）、14:04:13Z完成MOVE MEDIUM（28.593秒），回到原storage element1004；同一owner round1501886保持，无WARN/ERROR。原96项管理任务均终态，14类门闩为空，没有重启、清锁或人工再发move。

机械活动期间owner入口返回“池或带库正在执行互斥操作”，另外两个入口将其转成“owner未确认可服务”；完成后三个入口自行恢复200。这是会改变机械位置的后台行为，不能仅因客户端读取已结束就认定设备已完全空闲。报告分别保留首次健康快照、503窗口及恢复观测，不把503解释成丢数据或正常HA。

退带后再连续观察70秒、三轮查询，三个入口均200；同owner、14类门闩为空，FDB仍available/healthy。原健康快照与退带后稳定证据均保留。

后续优化：探针应区分机械维护暂忙和owner失权，并保留转发owner的具体原因；空闲阈值需按连续批量读取的间隔与驱动占用成本调优。不能通过跳过设备活动屏障或强制切换owner消除503。

## 顺序读写与可优化项

### 同一原始12KiB诊断样本

连续性由FDB原正文物理extent与实际READ POSITION判断，不按对象名、请求时间或List排序猜测。同卷会话还绑定owner/PR、Permission/epoch、有效VCR及当前head；短前向间隔在64逻辑对象/32MiB预算内顺读，倒退/大间隔或失效回原核验/定位路径。同一pool也可能跨带，不能据pool相同直接复用位置。

写入3×4096B，顺序z-first→a-second→m-third，刻意不同于字典序；读取严格使用原持久写入清单，ListObjects仍按全局字典序。原d639的连续PUT后两次16.410/15.644秒，0 LOCATE/LOAD/MOVE，已是同卷连续append。

| 对象 | 原d639 GET秒 | 最终e0 GET秒 | 原LOCATE/IP定位 | 最终LOCATE/IP定位 |
| --- | ---: | ---: | --- | --- |
| seq-z-first.bin | 44.232 | 56.719 | 12 / 6 | 9 / 4 |
| seq-a-second.bin | 43.192 | 5.375 | 12 / 6 | 0 / 0 |
| seq-m-third.bin | 43.354 | 5.161 | 12 / 6 | 0 / 0 |

后两次暖读平均43.273→5.268秒，约8.21倍改善，正文SHA均正确。首读含真实装带/首次证明，不能按相同条件比较。该12KiB样本用于定位诊断，不能作为LTO流式带宽。

### 64MiB随机对象与并发小对象

同一os.urandom生成的64MiB随机文件写成三个独立对象（未测硬件压缩比），串行写入并按实际提交序读取：192MiB写、192MiB读；另向第一个对象发反向1MiB Range。全量SHA正确，SDK自动重试关闭。

| 操作 | 序号 | 秒 | 端到端MiB/s | LOCATE / IP定位 | READ(6)次数 |
| --- | ---: | ---: | ---: | --- | ---: |
| put | 0 | 56.238 | 1.138 | 12 / 6 | 10 |
| put | 1 | 19.058 | 3.358 | 0 / 0 | 0 |
| put | 2 | 19.327 | 3.311 | 0 / 0 | 0 |
| get | 0 | 26.899 | 2.379 | 8 / 3 | 135 |
| get | 1 | 7.734 | 8.275 | 0 / 0 | 131 |
| get | 2 | 9.760 | 6.557 | 0 / 0 | 131 |
| range-backward | 0 | 6.573 | 0.152 | 1 / 0 | 128 |

此速率包括S3入站/暂存、索引、flush/MAM和确认，不能称裸驱动器带宽；1MiB Range的速率只以返回字节计算。CDB来自同一owner的请求日志窗口，包含并行的后台监控/资格检查，不能将所有PR/INQUIRY都归为前台操作独占开销。

可选顺序客户端另完成192MiB写/192MiB读；写102.220秒，读52.902秒。两个大对象组总计384MiB写、384MiB全量读，不含Range的内部额外读取。工具按持久清单保持z/a/m顺序，并以原ETag与完整SHA校验。

6×2KiB并发PUT共12KiB，全部200并独立读回SHA正确，总耗时36.275秒；实际WRITE(6)/FILEMARK/WRITE ATTRIBUTE CDB数量为7/3/2。按CDB形态观察到一次共享提交：True。HTTP完成次序没有被当作物理写入次序。

| 优先级 | 优化项/缺口 | 本轮依据与建议 |
| --- | --- | --- |
| 已修复 | 连续读重复定位 | 每个4KiB原需12 LOCATE/6 IP定位；最终后续暖读0/0。会话仍绑定设备、owner/Permission/PR/epoch/VCR/head，发生失效走原核验路径。 |
| 已修复 | 当前读误用完整历史审计 | 中间465候选长Full历史冷读超过10分钟；70独立Full软件owner红测85 LOCATE。最终专用当前读类型保留最新DP完整构造/EOD/SHA、DP/IP及必要全部增量链；完整历史审计保持独立。最终三份历史正文GET92.300/5.687/5.868秒、SHA全对。 |
| 已修复 | FUSE根恢复入口被自身EIO屏障挡住 | 真实根GETATTR先被拒导致SETXATTR不可达。仅放行根GETATTR并重新核根身份；最终再次真实EIO后同journal恢复成功，其他写入屏障保留。 |
| 高 | 小对象逐次提交的固定成本 | 已有FIFO约50ms/最多128对象合批；串行等待每个PUT落带会使队列无法积累。使用本轮并发CDB观测评估现有合批或应用聚合，不删除持久屏障、不提前报告落带成功。 |
| 中 | FUSE小文件更新延迟 | 最终候选补验中，相邻随机覆盖/追加、缩小/扩大截断、多句柄fsync发出检查点约60秒；包含前后准备/校验，不当成单独fsync精确耗时。应拆分客户端读改写、服务器任务与索引提交，评估安全合并，不能省略落带确认。 |
| 高 | 顺序大对象仍有软件链路成本 | 64MiB暖PUT约19秒/3.3MiB/s，已0 LOCATE；请求日志窗口含129 WRITE(6)、约912–954 PR-IN。暖GET约6.56–8.27MiB/s，131 READ(6)。应按RPC/FDB、PR、SG_IO、校验及暂存阶段分解，减少可证明重复的查询/调度，保留原执行权和提交核验。 |
| 高 | Range整对象读取放大 | 当前先完整读取校验再截取。本轮64MiB对象返回1MiB Range；结合实际READ(6)次数与耗时评估范围读取证明/缓存，不能仅因206正确就认为已优化。 |
| 中 | 导入别名/无SHA/历史UID正文不同的保守路径 | 当前快路径不覆盖这些来源，仍可能触发额外索引/机械读取；后续应独立建立有界来源证明，不能按文件名或最新路径替换原来源。 |
| 中 | 暖读剩余资格与协议开销 | 4KiB暖读仍约5秒，保留PR/FDB等检查；应按逐CDB及阶段计时找重复工作，不能将安全检查直接删除。 |
| 中 | 引用/空间统计收敛延迟 | L84 import任务成功后约80秒统计仍在calculating，之后才恢复estimated与live对象数。建议按namespace修订增量维护/继续有界扫描，并给出扫描进度和观测年龄；不能把null当0。 |
| 高 | 长历史导入与可观测性 | L85真实import为688.322秒/1072文件，任务窗口146次LOCATE合计582.833秒（约84.7%），另有10774次PR-IN；同池HEAD/List在接入期间503，任务只显示running而无细粒度扫描进度。优化历史核验的流式/有界执行与进度输出，保留来源、完整性及发布屏障。 |
| 中 | 长设备任务期间库存/健康样本的新鲜度 | import运行时CLI状态为importing，但位置仍为cached_inventory的旧homeslot；原快照库存观测13:13:35Z、MAM last_success13:06:55Z。名义30秒不等于任务期间每30秒完成；建议在表格中展示观测年龄/忙状态，只有安全的设备调度边界才采样。 |
| 高 | 旧运维/文件入口收敛 | 已确认旧drive/node查询404、cluster null；额外旧文件入口stat/list/get对已知FDB对象错误报不存在，stat仍exit0，get失败留下0B新文件。采用现行FDB入口并清理/接通未实现入口与手册，避免退出0/null误导运维。 |
| 低 | CLI参数/帮助约束一致 | fdb control list --limit 10本地拒绝“管理分页大小须为100”，改用100通过。帮助应明确固定页大小或在解析时约束，避免传入表面可选却无法使用的值。 |
| 后续 | pool空间告警投影 | 本轮只验策略持久化与实际容量查询；low/no-space告警输出与满池触发没有实物正例，不将配置存在当作告警端到端通过。 |

## 原始失败与恢复

1. 原d639顺序性能断言失败（数据SHA正确），以及中间465长历史冷读回退，均保留原失败日志。中间只读控制器被精确取消时没有新的写入意图；未把它改记为通过。修复后按最终同一版本重验。
2. 原FUSE文件改名1/c8b4b1dc-2580-490a-8a34-4b12e0e21d15已在服务端完成，但客户端未确认；原挂载正常卸载，按原journal结清同一回执，未重复rename。最终候选继续原场景，并在新目录改名真实EIO后成功通过根同步恢复；阶段14再用新文件于同一最终候选完整重验mmap/多句柄/属性等基础子集，未借旧版本结果冒充最终候选。
3. 初始测试脚本的旧SDK不支持新参数、Multipart误期待501（实际405）、已自动回收transient unit再次stop等，均作为测试环境/脚本错误保留。磁带黑名单请求的非规范JSON被400拒绝，原任务查询404后才使用相同语义/operation的规范JSON；没有换新ID重放未知副作用。磁带原槽按实际drive source_address及返回地址验证，不把loaded时null的home_slot拿来比较。
4. 管理动作后的就绪与容量/引用统计具有异步刷新窗口；不把暂时calculating/null当作空带，也不清门闩制造就绪。原始控制器失败时仅接续未发出的步骤及明确未受理的原请求。
5. 补充只读脚本曾误用管理分页10（实际仅支持100），收尾脚本曾误期望已删除消费者查询为404（实际为400及明确“消费者不存在”、retryable=false）。原失败保留，后续仅继续未执行步骤，没有重复已成功的ACK或删除。

## 未具备条件、未实现或未运行的范围

- 正向assign/初始化、无引用unassign、未分配move、首次import的-P/-R映射、强制格式化/擦除：本轮两盘可写scratch均已有live/retained引用；没有安全的空白未分配scratch。已有引用拒绝通过不能替代正向路径。
- 完整FDB原生reclaim仍未开放；源格式化、全部来源覆盖、实际迁移、T1/T2修尾/新donor与执行中HA边界不能据组件或零子父实验宣称通过。
- A库已借出，双库并行/互斥隔离、active_node跨库均衡、跨库copy及单库多驱动吞吐未重新实测。
- 现场无人拨动物理写保护开关；真实满带、永久坏带、需清洁/TapeAlert故障正例和设备路径断链未构造。注入事件只验证通知链路，不验证真实传感器识别。
- 仅正常owner停启接管；写入中断电/强杀、WRITE/FORMAT/UNLOAD精确回执丢失、控制网络分区、VIP漂移/内核冻结未覆盖。
- 当前FDB部署的EE实时双向互通、灾备大规模恢复、离线扩库/换驱动、重新初始化、独占tape-library/直接tape-rs全部低层设备命令及SQLite模式未重跑；旧版本独立报告只证明其各自版本。
- 不运行会删除历史证据的clearhistory/purge/清库命令来制造整洁状态。软件故障注入与协议单元测试不能代替这些实带边界。

## 软件检查、审查与证据

最终software-v8-results.json共11项exit0：fmt、4组有针对性的私有FDB/Sim回归、cargo test、test --all-features、check --all-targets、clippy --all-targets --all-features、8项Python工具测试、diff检查。Clippy保留既有prepare_file参数数量告警。三片修复分别经过规范/需求两轴独立审查，最终均0个未解决发现。真实硬件是否通过仅以本报告矩阵为准。

原始总目录：`/work/tmp/tape-main-physical-20261108-h9vn4yhb`；最终统一版本目录：`/work/tmp/tape-main-physical-20261108-h9vn4yhb/physical-final-e0a7625`。每阶段JSON、请求、回执、CDB日志、失败控制器、源码/ELF SHA均保留原位；报告附带摘要索引以便校核。私有登录信息仅保存在/root/jay/cicd/tape.md，不进入报告。

## 收尾状态

三台物理节点运行的ltfsd ELF均与本报告最终产物SHA一致，项目进程挂载命名空间无GPFS；三台项目服务与45153专用FDB服务均active，三个管理入口ready。FDB status JSON确认available/healthy，冗余模式double。14类权威占用为空；其后曾发生正常空闲退带的503窗口，已自行恢复（见上节），不能将首次快照当作持续就绪保证。测试FUSE均正常卸载、进程退出；L84/L85同卷同池、online且非黑名单，驱动策略还原，原两个池保留。

原d639测试消费者的原未ACK批次在多次正常升级、HA与全体FDB停服后逐字保持，已显式ACK；仅删除两个精确匹配的自有MOCKL8测试订阅，未清除全局outbox、其他消费者或任务历史。两份最终S3/FUSE正文再次独立读回校验成功，测试对象保留供复验。A库保持释放，未执行格式化/擦除或手工清锁/改PR。

本轮未向GitHub推送源码修复及报告；实际部署来源仍固定为e0a7625，不用文档提交号冒充运行版本。

完整可机读结果、原始文件SHA与路径见[证据摘要](physical-acceptance-20261108.json)。

## 后续缺陷修复与真实 B 库补验（8bd242a）

产品提交 `8bd242ac8eb8fb890be4f54102edfaeadabb23d6` 修复原矩阵的三项明确失败，并修正管理分页参数的帮助和解析约束。这不是其余未实现项或性能建议全部完成的声明。

| 原缺陷 | 修复 | 新版本真实验证 |
| --- | --- | --- |
| drive/node 查询404、cluster退出0输出null | CLI/HTTP驱动器与节点查询读取同库FDB事实及策略，校验namespace，保留观测时间；cluster直接查询物理节点，非200或无效JSON非零退出 | 三节点HTTP及CLI list/show通过；2驱动、3参与节点；缺失/错误namespace返回409，未知目标404；原复现脚本转绿 |
| 顶层stat/list/get误读旧目录，失败get留下空文件 | 直接复用现有FDB查询及原子校验下载，显式库/池UUID；stat不存在非零退出；移除未支持的--pending，不回退旧目录 | 同一S3原对象stat成功；自动跨页列举1,096对象，完整UTF-8字节字典序；直接子目录和中文目录正常；4389B正文CLI/S3独立SHA一致；缺失路径不创建输出，已有文件及符号链接不覆盖 |
| offline说明在tape show中为null | 管理查询沿用同一FDB事务已包含逻辑属性的assignment，不用基础归属覆盖它 | 原L84异步offline任务完成后，三节点HTTP list/show、CLI show及属性筛选均显示`acceptance gap repair 1108`；原任务上线成功后同名字段清空，原卷/池保持，正文再次校验正确 |

`fdb control list --limit` 当前固定100条，其他值在clap解析时拒绝；帮助及完整参数手册同步更新。文件下载仍须原记录有SHA-256或MD5，不降低现有校验要求。

软件检查共10项成功：fmt、CLI回归、3组隔离FDB/Sim管理回归、默认及全特性cargo test、全目标check、全目标/全特性Clippy和diff检查。回归先在原代码复现，再确认修复；隔离查询追踪未新增设备I/O。Clippy仅原有`prepare_file`参数数量告警。最终diff进行自查，未发现未解决问题；本批未宣称经过独立双轴审查或已推GitHub。

207–209的ltfsd及ltfsctl已正常升级至上述产品提交，目录`/opt/tape-rs/v1-20261108-8bd242ac8eb8`。未改动的tape-rs/tape-library/tape-fuse保留原e0a7625已验收产物，逐项SHA记录在升级证据中。原45153集群、namespace、state和服务单元保持；正常停启前后原权威摘要一致，未强杀、清锁或手工更改PR。L84离线/上线只新增两项成功管理任务，S3元数据、上传与节点回执、池及回收事实摘要保持。最终14类占用空、三入口就绪，原L84卷UUID、池归属及数据保留，A库未操作，无FORMAT/ERASE。

保留原失败：首次发行构建因交叉链接器缺FDB库搜索路径失败，使用依赖本身支持的`FDB_CLIENT_LIB_PATH`后普通EL9构建成功；两个部署脚本预检错误发生在任何停服或设备动作之前。实带控制器将属性筛选的JSON对象误按标量断言而退出，原正确返回及离线任务均保留；新接续只核对原任务并执行尚未发出的上线步骤，没有重复离线或重跑旧controller。上述脚本问题不改记为产品通过或产品失败。

新证据目录：`/work/tmp/tape-acceptance-gap-fixes-20261108-64uu7mpq`；入口为`UPGRADE-COMPLETE.json`、`physical/COMPLETE.json`、`final-integrity.json`、`checks-results.json`及`artifact.json`。原报告与123份历史证据不覆盖、不删除；本批结果及SHA另列于JSON摘要的`gap_repairs_v1`。原生reclaim、Range读取放大、长历史导入、顺序链路固定成本和其他原报告限制继续保留，不能把三项缺陷收口当作全部功能验收。
