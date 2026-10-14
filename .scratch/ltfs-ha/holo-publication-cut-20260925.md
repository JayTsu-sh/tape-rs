# 索引已提交、目录未发布：隔离 Holo 验收

2026-09-25（Asia/Shanghai；设备日志09-24 UTC）。ltfs-recovery-ibm-interop。已验证该窗口，不需要生产修复。

## 明确故障点

executor::process_uploads依次执行commit_incremental、catalog_of、bytes_written_on的MAM读取，再调用files.batch_done并发送ExecEvent::Committed；Raft收到事件后才提议CatalogPart/TapeCommitted。因此可利用提交后的MAM读取，在不改生产代码的情况下阻止本地发布和目录事件。

隔离node2/round210从RT2502L8 Full gen71开始。publication-cut.py仅接受唯一测试Leader、独立程序路径和RT2502L8归属，strace使ioctl进入前延迟200ms。脚本观测到卷提交日志gen72，确认尚无“执行线程: 已提交”日志，再读取只读SQLite：目标记录generation=71、cut仍old，随后SIGKILL。

cut.trace中提交后的MAM READ ATTRIBUTE（8c、partition1、attr0000）unfinished；cut.event保存commit日志、旧目录行及最后六条SCSI调用。磁带增量索引、屏障和VCI流程已完成，FileService batch_done及CatalogPart/TapeCommitted事件尚未发出。Client返回Indeterminate，没有收到成功回执。

## 恢复结果

node1/round215恢复磁带DP gen72 Complete（IP仍Full gen71）。明确日志：“目录停在第71代，磁带是第72代：以磁带为准重写该带的目录”。读取恢复后的cut为新值uncommitted（这是探针写入的字面内容，实际已提交），set二进制属性保留，delete仍不存在；文件内容、SHA256、长度、mtime与旧目录记录一致。

恢复属性版本为(210,1)，即原Leader的提交版本，没有重放。重新启动node2后，三节点SQLite均核对为generation72、version210.1及同一新属性。正常停机写Full gen73。

## 隔离与收尾

原三节点正常停机、TS1001L08归槽6；独立/home/rocky/tape-rs-publication-cut-20260925/test-data和7500/7501，仅归属RT2502L8。服务端沿用正式版f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。Holo备份/home/rocky/holo-publication-cut-20260925/{before,after}。node2缺系统strace，复制工具到测试目录后才附着；没有系统安装或修改。

测试节点、strace退出，RT2502L8归槽7保留gen73；LE副本未访问。原集群恢复node3/term31/round261，TS1001L08仍gen72 Complete，TS1000L08 gen2。三节点实际exe均正式f8c238…，原9文件长度/SHA和完整list不变，catalog31条及哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0不变。EE3节点available无任务、f3哈希不变、9publications ready。

本轮仅隔离ltfsd进程SIGKILL，没有VM/存储断电、Holo重启、格式化、物理TS4300或原带文件写入。不能把此结果扩大到存储掉电、所有复制日志阶段及所有barrier/VCI故障点。

## 检查与材料

新增Python探针语法及git diff --check通过；复用已验收程序和Client example，无Rust语义改动，未重跑201项软件全套/check/clippy。探针整理时将日志起始偏移统一为字符计数，避免Unicode日志的字节/字符偏移混用；本次实际命中由事件中完整提交日志和旧目录行确认。

probes/publication-cut.py；probes/results/publication-cut-*包括Client Indeterminate、提交/旧目录事件、SCSI跟踪、恢复stat、三节点目录记录、各节点日志、最终XML和原环境校验。全部工作树改动未提交。
