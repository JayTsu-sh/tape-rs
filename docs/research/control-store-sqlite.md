# SQLite 本机控制存储：事务与可靠完成基线

票据：36；核对日期：2026-09-16。
范围：SQLite 官方文档、当前缺陷公告与本项目控制存储契约；不重新设计 Raft。
本报告只给候选及验证条件，未添加依赖、创建数据库、运行模型或访问实验室/设备。

## 1. 推荐结论

首版建议采用独立本地控制 DB、单数据库工作线程/连接、`journal_mode=DELETE`、`synchronous=EXTRA`。
Raft 日志、HardState、完整控制快照、快照安全依赖及配套发布元数据都在该 DB 内；不复用 catalog，也不使用外置快照文件。
按已定操作边界使用显式事务，不把一个 Ready 与其后才出现的 LightReady 更新强行合成预先已知的单事务。
Raft 同步读取由匹配已提交修订的受保护 RAM 视图提供，SQLite 不在 S 临界区中执行。
这些是项目候选，不是 SQLite 自动生成的 Raft Storage 实现。

SQLite 的原子事务可承载本项目要求的“日志/协议状态/快照发布完整发生或完整不发生”，前提是 SQL 更新正确、事务覆盖全部必要对象、底层存储满足其同步及锁假设。[原子提交][atomic]
它不自动证明 Raft 后缀范围正确、真实 apply 完成、旧成员不并存或全目录未回滚。

## 2. 日志模式对照

| 方案 | 官方保证/代价 | 本项目判断 |
| --- | --- | --- |
| DELETE＋EXTRA | FULL 同步之外，还同步提交时删除 rollback journal 所在目录 | 推荐首个低频控制事务基线，接受额外同步/写放大 |
| DELETE＋FULL | 掉电后某些文件系统可能丢最近已提交事务，虽未损坏数据库 | 不足以作为本项目统一可靠完成基线 |
| WAL＋FULL | 每次提交同步 WAL；已提交内容可在 checkpoint 前可靠恢复 | 合法替代候选，但引入 WAL/检查点及读者管理，不据“WAL 更快”直接选定 |
| WAL＋NORMAL | 可保持一致性，但 OS 崩溃/掉电后可能回滚已提交事务 | 不满足必要 term/vote/log 的可靠确认 |
| OFF/MEMORY journal 或 synchronous=OFF | 不具备本项目要求的机器故障恢复组合 | 不允许静默降级 |

同步依据：[synchronous 官方定义及矩阵][sync]。
EXTRA 在 WAL 模式下不比 FULL 增加保证；Linux 上也不把名称 `fullfsync` 当额外硬件可靠性认证。
推荐 DELETE 的理由是首版只有一个 DB 连接，业务读者使用 RAM，尚不需要 WAL 的 DB 读写并行能力；不是宣称 DELETE 吞吐优于 WAL。

WAL 仍只有一个写者；读事务可长期固定旧 end mark，阻碍 checkpoint 完整推进并使 WAL 增长。[WAL 并发与回收][wal]
未来若改多连接/WAL，必须另外定义短读事务、checkpoint 所有权、空间预算和 BUSY 处理；不能仅替换一条 PRAGMA。
SQLite checkpoint 是页从 WAL 迁回 DB，Raft 控制快照是状态机切点；二者不可混称或相互代替。

## 3. 2026 官方缺陷及版本准入

官方公告确认 WAL-reset 数据竞争缺陷，可能导致损坏；可能影响 3.7.0 至 3.51.2。
修复见 3.51.3 及以后，也有 3.44.6、3.50.7 回补；触发涉及同一文件的多个线程/进程连接同时写入或 checkpoint。[WAL-reset 公告][walbug]
因此“应用只有一个业务写者”不足以排除另一个连接做 checkpoint；synchronous=FULL 也不是代码缺陷补丁。
本报告不把已修复缺陷描述成所有 WAL 都不可靠，也不因首版单连接而推荐旧漏洞版本。

核对时官方稳定发布页为 **3.53.4，2026-07-24**；可作为首版验证的固定上游候选。[3.53.4 发布页][release]
其 `SQLITE_SOURCE_ID` 为 `2026-07-24 19:02:57 bf7c7f30031888f4e796e429ab3978879485813aaca6f641c7b33e4e09459bcc`。
这不是“版本号大于某值就自动安全”的规则；实际构建须锁定源及补丁，升级重新核对公告和回归。
3.52.0 曾因部分新特性兼容性被撤回，不能把它列作无条件推荐的稳定中间版本。[官方新闻][news]

启动记录并验证实际链接库版本、source ID、编译选项和 VFS；Rust 绑定版本/系统 CLI 版本不能代替进程实际 SQLite 版本。[运行时版本 API][version]
不能采用关闭 journaling、同步或文件锁的特殊构建/VFS，也不默认发行版补丁与上游同号二进制完全一致。
本报告不选择 Rust 绑定 crate，不修改依赖或声称现有 catalog 的 SQLite 构建已通过准入。

## 4. 连接和配置必须核验

所有 DB 修改、恢复读取及回收通过唯一工作线程的连接串行执行；运行期间不开放绕过该入口的后台写/管理连接。
采用明确的本地文件路径和正常锁定 VFS，不使用 `nolock`、把活动 DB 标成 immutable 或把文件当网络共享控制权威。
单连接减少竞争，不等于其文件锁会阻止另一进程在空闲时成为第二个合法写者；运行实例排他仍须由现有写者接续契约保证。

建议在打开 DB/进行可能触发恢复的读取之前，对固定独立锁文件取得 `flock(LOCK_EX|LOCK_NB)`，并持有至全部 DB 调用和对象收束后最后释放。
这是同机本地文件系统、所有参与者共同遵守的 advisory 协议；SQLite 事务锁不与它自动等价，也不能阻止绕过者直接访问。[Linux flock][flock]
锁关联 open file description；dup/fork 可共享它，任一相关描述符显式解锁或全部关闭会影响持锁，exec 也有继承语义，须限制泄漏和提前释放。
锁文件不 unlink/recreate，不允许不同锁路径对应同一活动 DB、活动目录替换或另开维护连接；这些是保证固定锁对象的部署前提，不是 flock 自动检查的事实。
取得锁只证明当时没有相冲突的合规持锁者，不独自证明旧 I/O 全部可靠或其所有可观察效果已结束；仍由 SQLite 恢复和 SCM 的写者收束条件共同判定。
跨主机同身份克隆、整机回滚或不同文件副本各自取得本地锁均不被该锁排除，仍需 SG/部署约束，不宣称它是设备 fencing。

在许可开放前设定并读回 `journal_mode=DELETE`、`synchronous=EXTRA`；模式不符或调用失败则受限，不继续靠默认值。
PRAGMA 拼写错误可以被静默忽略，故“执行没有异常”不是设置成功证据。[PRAGMA 规则][pragma]
新连接不能继承此前连接已经核验的同步假设；若以后增加连接，每个连接都需独立核验适用设置。
已有控制文件处在不同模式时，先报告格式/运行配置不匹配；不能在未知写者或未收束恢复条件下自动迁移模式。

若模式/约束设计使用外键，明确启用并读回其 enforcement，不能依赖默认；表约束、类型检查与应用解码共同保护格式。
禁止共享缓存脏读；不在运行期执行任意外来 SQL、加载扩展或修改安全 PRAGMA。防御性配置是补充，不替代固定 SQL 与格式验证。[隔离][isolation]
`BEGIN IMMEDIATE` 可用于显式批次，尽早取得写事务；所有语句/COMMIT 结果均检查。[事务][transaction]
不允许把多个必要语句各自隐式提交，也不以内部 savepoint RELEASE 代替最外层可靠 COMMIT。

## 5. 同库事务能承载的操作

以下仅说明事务边界，不取代主线程的字段/对象映射设计。

| 项目操作 | 同一事务的必要范围 | 后续独立门槛 |
| --- | --- | --- |
| 普通协议批次 | 前态修订核验、合法日志追加/后缀替换、必要 HS 及配套元数据 | RAM Storage 视图匹配、Ready 原身份仍有效 |
| LightReady commit | 新 Cd 与已有日志/快照依据核验，保留当前 term/vote | 超过旧 Cd 的条目此后才能实际 apply |
| 本机快照发布 | 完整 BLOB/闭包、S/任期/ConfState、当前基线与保留边界 | 不回退当前 A 或覆盖构建时之后的新 HS |
| 真正接收快照 | 正文/闭包、基线及内核认可的日志/HS 组合 | 实际状态机安装后才满足既有 P2 |
| 回收 | 仅删除已被可靠替代且无必要引用的精确对象/范围 | 不等于磁盘空间已返还 OS |

前态条件、修订和内容关联应与更新在同一事务核验；冲突不允许改写 expected 后复用旧输入。
接收快照不能默认保留旧后缀，本机生成快照不能借提交清掉之后的新日志；SQLite 不认识这些 Raft 约束。
若用同库分阶段准备对象，未发布对象与权威引用必须显式区分；首版可先在 RAM 完整准备，再在一个发布事务插入正文与元数据。
同库不要求“整个历史 DB 每次重写”，也不要求逐文件或逐 CDB 存储事务。

## 6. 可靠、可见、被观察和收束分别表达

建议顺序：在 S 外完成不可变输入和 RAM 候选准备；DB 线程执行完整事务；COMMIT 成功后才发布匹配修订的 Storage 视图；最后回报该操作完成。
同一 SQLite 连接能读取自己的未提交修改，因此事务内 SELECT 可构建候选，但不能提前把其内容暴露给 RawNode。[同连接可见性][isolation]
多条恢复 SELECT 必须来自同一个受保护读取区间；不能让其他事务插入其中后拼接不同修订。

在合格版本/设置/存储假设下，成功 COMMIT 是本次 DB 事务的可靠完成依据；语句 SQLITE_DONE、事务已开始或数据写进 page cache 都不是。
COMMIT 成功而 RAM 发布失败：DB 事实可用于恢复，但本次 P2 不成功，不能先 advance 后补 RAM。
回调丢失或观察超时：不知结果不是未提交；查询原操作关联或在旧写者收束后恢复完整组合，不盲重跑旧事务。
同事务中的操作关联/修订可协助核对，但不要求无限保留每次历史结果；查不到旧结果仍不能推出“未发生”。
正常运行可只在内存实际 apply，并从完整快照加可靠日志重启重放；这不要求新增逐条业务状态表或独立持久 applied。
SQLite 提交不意味着多数派 commit，DB 中的 commit 字段也不是 SQLite 能自行验证的 Raft 证明。

## 7. 事务错误与取消出口

| 观察 | SQLite 事实/项目处理 |
| --- | --- |
| 前态检查拒绝 | 在同一事务确认无修改并收束后，才能报条件不成立；必要批次不假 advance |
| COMMIT 返回 BUSY | 事务可能仍活动；库允许稍后重试 COMMIT，但不能重新提交整批 SQL 当新操作 |
| FULL/IOERR/NOMEM/INTERRUPT | 可能只撤销当前语句，或回滚整个事务；检查实际事务状态，不能按统一“完全没发生”处理 |
| COMMIT 成功、通知未到 | 核对已提交事实，原失败/停止实例不因此复活 |
| 损坏/错身份/不可验证恢复组合 | 限制旧控制身份，按 SG 核对，不自动新建空库或采用旧备份投票 |

事务结果依据：[事务错误规则][transaction]、[autocommit 状态 API][autocommit]。
autocommit 恢复仅说明事务已结束，不单独区分成功提交与回滚；须连同原调用结果/当前可靠关联解读。
BUSY 的原事务续等与“必要保存已经失败后的重放”不同；首版仍按既有 FD 一旦必要保存明确失败先停发、核对并收束，不引入无限自动重试。
保留 SQLite 主/扩展错误码、操作身份、当前事务状态、失败阶段及是否仍可能修改；不能只返回字符串“数据库忙/失败”。

`sqlite3_interrupt` 只是请求尽早中断，临近完成的操作可能仍成功，不能当作同步取消或写者退出证明。[interrupt][interrupt]
封闭接纳后须跟踪当前 DB 调用、排队修改、准备任务及回收，防止随后又进入连接。
关闭前完成/销毁 statement、BLOB handle 和 backup 对象；`close` 可因遗留对象返回 BUSY；`close_v2` 的 OK 可只是 zombie 延迟释放。[连接关闭][close]
因此 Rust drop/关闭通道/取消 future 不能单独证明底层写入已结束；待具体绑定证明其所有权和错误传播。
慢 SQLite 调用不持 S 或逐命令锁，运行许可的快速失效路径不等待它返回。

## 8. 完整 BLOB 的容量与内存成本

同库 BLOB 消除外置文件“正文 fsync 成功、引用提交失败/错配”的跨资源发布责任，但不消除大小和复制成本。
官方默认单字符串/BLOB 上限为 1,000,000,000 字节，当前实现硬上限为 2^31−3；同一行的编码大小也受该限制约束。[SQLite limits][limits]
这些是库上限，不是项目可安全接收的快照额度；项目额度须更早检查并包含正文、元数据及安全闭包。
绑定大值使用参数，不把 BLOB 转成巨型 SQL 字面量；运行时实际 limit 必须查询/核验。[limits][limits]

内存预算至少包含旧运行视图、新快照构建/编码、不可变待写对象、SQLite page cache/绑定复制、新 RAM 视图及仍被发送任务持有的旧对象。
SQLITE_TRANSIENT 要求 SQLite 复制绑定数据；SQLITE_STATIC 则要求调用者维持数据生命周期，不能凭减少复制理由悬挂引用。[BLOB binding][binding]
查询返回的 BLOB 指针受 statement/转换/reset/finalize 生命周期限制；交给跨任务读者前复制到安全所有对象，不保存裸 SQLite 指针。[column lifetime][column]
一次大 BLOB 事务、journal 及新旧快照共存会放大瞬时磁盘需求；单 DB 线程也会被大事务占用。
因此不能用“控制面低频”推断无限快照或可靠更新延迟不会影响 RQ/接管；真实预算和测量仍待定。

快照超限时不截断未决动作/拒绝依据、不增加隐式外置文件；未发布本机优化可以放弃，必要接收/保存能力不足按相应受限出口处理。
如长期安全闭包不能进入首版预算，须调整正式支持容量/格式，而不是删未知事实让测试通过。
SQLite 原生支持 incremental BLOB I/O 并不自动解决完整解码/RAM Storage 预算，本报告不据此新增分块协议。

## 9. u64 与持久格式

SQLite INTEGER 是有符号整数，内存接口通常为 64 位；REAL 是浮点数，不可无损承载全部 Rust u64。[数据类型][datatype]
建议需要完整 u64 域的 term/index/node ID/修订采用统一 **8 字节大端 BLOB** 编码；需要排序时保持同一 BLOB 类型和固定长度，并使用显式 ORDER BY。
该编码是项目候选：固定宽大端字节比较可保持无符号序；不使用 SQL 数值加减或自动类型转换处理其进位。
另一方案是明确限制到 i64 范围并检查越界，但这会引入协议/格式限制，不能通过 `as i64` 静默实施。
STRICT 表可限制存储类型，却不创造 unsigned INTEGER；结合 NOT NULL、长度/类型检查与解码验证。[STRICT tables][strict]
未知 schema/编码版本拒绝启动写入，不通过 `CREATE IF NOT EXISTS` 把丢失或错格式控制库修成空状态。
配置、快照边界、日志连续性及内容关联仍由项目校验；数据库结构完整不等于控制历史正确。

## 10. 读者保护、回收与重启

首版没有跨网络操作持有的 DB 游标；完整读取后形成受保护 RAM 对象，结束 statement/事务，再交给读者或发送任务。
只复制对象 ID 不等于取得正文所有权；若任务仍需晚些读取 DB 正文，必须登记保留引用并由同一写入口核对后才删除。
同一连接上游标未结束时穿插修改，其后扫描结果不可当作固定恢复视图。[隔离文档][isolation]
DB 删除对象后，已完整拥有正文的 RAM 读者可继续使用；没有 RAM 副本的读者不能因 DB 曾经查询成功而视作受保护。
回收事务保留当前/替代恢复组合、必要日志、未决安全事实及拒绝边界；不根据队列为空删除全部历史。
默认非 auto-vacuum 时 DELETE 释放的页主要供 DB 内复用，不保证文件缩小；首版不把全库 VACUUM 插入关键保存路径。[auto_vacuum][autovacuum]

正常崩溃后的 hot journal 由 SQLite 处理；它是恢复依据，不能启动时当临时垃圾删除。
“一个 DB”是一个事务权威，不是声称运行中永远只有一个物理文件；journal/WAL 必须与 DB 配套保留。[损坏风险说明][corrupt]
不复制活动主 DB 文件就当完整备份，也不移动/替换活动文件或任意清理旁文件。
恢复后先验证应用身份、版本、完整引用/日志/HS 组合，再构建 RAM 视图及进行既有受限重放；不把 integrity_check 当 Raft 安全审计。
一致旧备份或 VM 全盘回滚可以同时回滚本地校验/修订，SQLite 无法据自洽内容排除；SG/部署生命周期前提仍必要。

## 11. 真实存储前提与下一步验收

SQLite 依赖 OS/VFS 文件锁、正确的写入/删除行为和有效 flush；底层设备谎报落盘可破坏持久性。[SQLite 硬件前提][atomic]、[同步失败][corrupt]
正式支持需覆盖本地文件系统、挂载方式、虚拟磁盘、宿主缓存/存储控制器直到断电后仍保留数据的链路；EXTRA 不是绕过这条链路的魔法。
应用崩溃、guest 重启、host 掉电、存储故障、整机回滚/克隆分别记录，不能用 kill 进程测试代替整机掉电保证。
新建 DB/目录、路径接续和删除旧设施还须满足命名空间可靠性及独占条件；不把同库事务外的部署步骤自动涵盖。

待验证且本轮未执行：
- SQ01：协议批次/后缀替换与 HS 的事务各切点崩溃，只恢复完整合法组合。
- SQ02：LightReady commit 可靠后才发布超出旧 Cd 的 apply；通知丢失不盲重做。
- SQ03：本机/接收快照的正文、引用、日志与最新 HS 配套，安装失败不假 P2。
- SQ04：COMMIT BUSY、FULL/IOERR、中断/关闭未收束，旧写者不能继续越界修改。
- SQ05：BLOB/整行/峰值 RAM/磁盘预算边界，不删安全事实、不默认吞吐达标。
- SQ06：旧 RAM/DB 读者及回收交错，释放后正常回收并继续工作。
- SQ07：运行版本/source ID/PRAGMA/VFS 不符时拒绝开放，不能静默回退同步模式。
- SQ08：可信正常重启能重放和重新授权；损坏/全盘回滚/克隆不伪称自动识别或修复。

研究支持推进上述具体基线；实际 SQL schema、Rust 绑定、RAM 发布原语及存储环境验收仍由后续规格/实现完成。

[atomic]: https://sqlite.org/atomiccommit.html
[sync]: https://sqlite.org/pragma.html#pragma_synchronous
[wal]: https://sqlite.org/wal.html
[walbug]: https://sqlite.org/wal.html#the_wal_reset_bug
[release]: https://www.sqlite.org/releaselog/3_53_4.html
[news]: https://www.sqlite.org/news.html
[version]: https://sqlite.org/c3ref/libversion.html
[pragma]: https://sqlite.org/pragma.html
[transaction]: https://sqlite.org/lang_transaction.html
[isolation]: https://sqlite.org/isolation.html
[autocommit]: https://sqlite.org/c3ref/get_autocommit.html
[interrupt]: https://sqlite.org/c3ref/interrupt.html
[close]: https://sqlite.org/c3ref/close.html
[limits]: https://sqlite.org/limits.html
[binding]: https://sqlite.org/c3ref/bind_blob.html
[column]: https://sqlite.org/c3ref/column_blob.html
[datatype]: https://sqlite.org/datatype3.html
[strict]: https://sqlite.org/stricttables.html
[autovacuum]: https://sqlite.org/pragma.html#pragma_auto_vacuum
[corrupt]: https://sqlite.org/howtocorrupt.html
[flock]: https://man7.org/linux/man-pages/man2/flock.2.html
