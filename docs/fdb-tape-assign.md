# 原生 FDB 磁带分配

`assign` 对未分配数据带执行 LTFS 格式化，再原子登记 library -> pool -> tape。依照提供的 EE 1.4.0 命令文档 section1，有文件的磁带须显式强制格式化，I/E 中的磁带先回 home，保留数据则使用 import。

```sh
ltfsctl -e 10.131.7.207:7411 --library SERIAL tape assign BARCODE POOL --confirm-format --request /secure/assign.json
# 有文件或原格式不可读且确实要清除：再加 --force-format
ltfsctl -e 10.131.7.207:7411 --library SERIAL task show --request /secure/assign.json
```

`--confirm-format` 与请求文件必填，池参数是名称或 UUID。首次提交前保存原任务 UUID、新卷 UUID、namespace 与 pool UUID；重复执行使用同一请求文件，不重新解析归属或生成卷身份。请求文件是客户端恢复凭证，任务状态由 FDB 保存。HTTP `POST /libraries/SERIAL/admin/tapes/BARCODE/assign` 要求 namespace header 与原 media-assign envelope，返回202仅代表已受理；task show/cancel 沿用管理任务协议。

执行计划固定完整地址 map、原源/home、目标驱动器序列号和池策略修订号；原机械线程与搬带、rescan、文件读挂载共享库级互锁。每条 MOVE、FORMAT 后续写入及探测命令前，再确认 FDB 原任务/owner/池策略、PR和本地许可。格式化超时不缩短。空白介质须检查全部已定义分区；分区0空白不代表其他分区可覆盖。分区数依据 IBM GA32-0928-08 Table348，ready后读 MODE SENSE11h byte3+1。

阶段为 prepared、moving、threading、inspecting、formatting、confirmed。FORMAT 可能已执行或仍执行时保留原请求、新卷 UUID、阶段、错误和互锁；恢复只读取并验证原卷，不再次调用格式化。完整初代空卷须重读两分区 VOL1/LTFS 标签及其闭合文件标记，核对原 UUID、格式参数和标签公共字段，再经恢复协议及原驱动器库存确认后登记成功。新epoch自身不证明旧硬件命令已经被中止。阶段已持久但进程在发命令前退出，与命令仍在执行可能不可区分；若 LOAD 一直未完成或格式化只完成分区未写完标签，任务会保留为 running+诊断，需核对实际设备与原任务，不能靠清锁/新ID解决。当前尚无自动修复部分初始化的入口。

成功事务同时登记磁带归属、卷放置及反向索引，保存不可变任务终态、产生通知并释放原队列/库互锁。新的卷 UUID 在格式化前跨库互斥保留，成功后保留该身份，避免复用历史卷身份。分配成功的 `index_imported=false`，本操作不发布文件 heads 或伪造 imported baseline；尚未导入的卷不可服务文件读写，且不会阻止其他磁带的 owner 恢复。queued 可取消，running 拒绝取消；非强制检查拒绝后可能已装入驱动器，结果明确标记 loaded_not_formatted。

已在真实 B 库以显式 force 格式化并分配8A0042L8，验证原请求在 FDB 故障后恢复、完成后计划切换 owner 及已分配带归槽；未导入文件索引。01ee927 的非强制实物补测因大索引整树挂载触及 4GiB cgroup 内存上限而失败。128e2c3 已在原4GiB限制下恢复同一AA43请求，发现内容后结束为 loaded_not_formatted，并完成三节点原回执重放及归槽；三个新进程峰值RSS约33–36MiB。旧失败及暂停证据保留，当前服务已恢复。纯软件检查与真实硬件结果分别记录，不能将 Sim 故障注入当实物故障。真实 B 使用范围、A及备用路径禁用要求见最新本地交接记录。

本片软件反例已复现并修复 MOVE 前 FDB 核验遗漏、数据分区标签未核对和永久诊断未上达控制器。私有 FDB + Sim 覆盖完整格式化回执丢失、部分初始化、P1 UUID 错误/截断、旧 FORMAT 仍在后台执行时同 initiator 新 epoch 接管，保持原 UUID/gate 且不重复 FORMAT；FDB TCP 黑洞在 Moving stage 已确认后及 FORMAT 前阻止新设备命令。这些不是物理设备故障注入结果。

非强制检查中，已完成读取后确认的标签/XML/恢复限制会结束为 failed.loaded_not_formatted；保留原错误，释放本操作的库互锁和未使用新卷 UUID。FDB、许可、PR 或传输结果未知则保留 running/原检查点；传输错误即使被索引恢复的回退路径捕获，也不会被误当作“确认坏卷”，同次探测停止后续命令。

非强制检查不再构造完整写入挂载及其目录树副本。当前 Full 构造发现首个文件或非根目录即可拒绝；这不宣称整份索引有效。空卷须完整核对当前 DP 构造边界和身份、同代 DP/IP、未锁定及两分区标签；空 delta 需先在私有临时 SQLite 中重放为最终对象集合，再判断为空。临时 SQLite 是可丢弃证明数据，不保存权威任务状态。

此操作限制每构造16MiB、增量前驱最多8份，XML解析已有512层深度限制；单事件/属性也受构造字节上限约束。临时盘沿用本库 spool 和原有物理预留限额。内存/深度/链预算、本地容量、磁盘IO不足会保留 running、原请求与库互锁，不作为坏介质清理；原传输或失权证明仍优先。当前 Full 已发现内容时可提前拒绝，无需读完巨型历史索引。真实大索引修复已在原4GiB限制内验证，未提高上限。

默认测试复现回收完成事件已发出、Raft 尚未应用时重复 FORMAT 的窗口；执行器保留 completed 工作状态，等共享状态离开 reformatting 才释放。2026-11-06 在真实 B 库 AA0000L8 上，直接运行生产 finish_reclaim/drive_reclaim：一次 FORMAT 取得 GOOD，留置原完成回执后连续推进50次，零新增CDB、FORMAT仍为一次；应用原 TapeReclaimed 后释放 completed，磁带归home1002、预留释放。此次使用独立 operator/SQLite 已reformatting阶段夹具，不验证此前文件迁移、真实Raft共识或多机HA；不能把它扩展成整条回收命令全部通过。硬件测试保持 ignore，须提供 TAPE_RS_RECLAIM_ACCEPT_ROOT 下显式 confirm_format/allow_data_loss 计划及原设备owner撤权证明。源码b9a66cb，测试ELF SHA256 0bc0a63c252670810539e3dc47f4f332d96f65becd3266f47e8a9bfedb082607，原始证据 /work/tmp/tape-legacy-reclaim-evidence-20261106。

两既有通知消费者在完整原始事件及批次落盘后，从ACK194推进到210；旧40个终态及历史事件字节保留。最后B主路径三节点就绪、PR与FDB owner一致，AA42仍assigned且未导入、位于home1007，AA43未分配且位于home1003。2026-11-06 正式128/main2b70的实际CLI/HTTP另完成AA47非强制空LTFS分配正例：force=false/confirm=true，任务e7141fd1-0290-4cc2-92fa-7c0325d5bf3f，卷cadebf8a-71fb-4df1-ab96-391440073b44，归lto8后以原homeslot任务归1005，仍index_imported=false。当前共有69个终态管理任务，原67条字节及AA42归属不变；两原消费者ACK仍210，新增事件保留未盲ACK。随后独立回收实验仅清空未分配AA0000L8并归1002，AA47/42归属保持。证据 /work/tmp/tape-format-safety-evidence-20261106/main-empty-positive/COMPLETE.json。其余场景和边界见[实物验收矩阵](physical-acceptance-20261106.md)。
