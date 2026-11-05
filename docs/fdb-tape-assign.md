# 原生 FDB 磁带分配

`assign` 对未分配数据带执行 LTFS 格式化，再原子登记 library -> pool -> tape。依照提供的 EE 1.4.0 命令文档 section1，有文件的磁带须显式强制格式化，I/E 中的磁带先回 home，保留数据则使用 import。

```sh
ltfsctl -e 10.131.7.207:7411 --library SERIAL tape assign BARCODE POOL --confirm-format --request /secure/assign.json
# 有文件或原格式不可读且确实要清除：再加 --force-format
ltfsctl -e 10.131.7.207:7411 --library SERIAL task show --request /secure/assign.json
```

`--confirm-format` 与请求文件必填，池参数是名称或 UUID。首次提交前保存原任务 UUID、新卷 UUID、namespace 与 pool UUID；重复执行使用同一请求文件，不重新解析归属或生成卷身份。请求文件是客户端恢复凭证，任务状态由 FDB 保存。HTTP `POST /libraries/SERIAL/admin/tapes/BARCODE/assign` 要求 namespace header 与原 media-assign envelope，返回202仅代表已受理；task show/cancel 沿用管理任务协议。

执行计划固定完整地址 map、原源/home、目标驱动器序列号和池策略修订号；原机械线程与搬带、rescan、文件读挂载共享库级互锁。每条 FORMAT 后续写入及探测命令前，再确认 FDB 原任务/owner/池策略、PR和本地许可。格式化超时不缩短。空白介质须检查全部已定义分区；分区0空白不代表其他分区可覆盖。分区数依据 IBM GA32-0928-08 Table348，ready后读 MODE SENSE11h byte3+1。

阶段为 prepared、moving、threading、inspecting、formatting、confirmed。FORMAT 可能已执行或仍执行时保留原请求、新卷 UUID、阶段、错误和互锁；恢复只读取并验证原卷，不再次调用格式化。完整初代空卷经恢复协议及原驱动器库存确认后才登记成功。新epoch自身不证明旧硬件命令已经被中止。阶段已持久但进程在发命令前退出，与命令仍在执行可能不可区分；若 LOAD 一直未完成或格式化只完成分区未写完标签，任务会保留为 running+诊断，需核对实际设备与原任务，不能靠清锁/新ID解决。当前尚无自动修复部分初始化的入口。

成功事务同时登记磁带归属、卷放置及反向索引，保存不可变任务终态、产生通知并释放原队列/库互锁。新的卷 UUID 在格式化前跨库互斥保留，成功后保留该身份，避免复用历史卷身份。分配成功的 `index_imported=false`，本操作不发布文件 heads 或伪造 imported baseline；尚未导入的卷不可服务文件读写，且不会阻止其他磁带的 owner 恢复。queued 可取消，running 拒绝取消；非强制检查拒绝后可能已装入驱动器，结果明确标记 loaded_not_formatted。

目前未部署本片代码，也未在真实 B 库执行 assign；纯软件检查与真实硬件结果分别记录，不能将 Sim 格式化测试当实物验证。真实 B 使用范围、A及备用路径禁用要求见最新本地交接记录。
