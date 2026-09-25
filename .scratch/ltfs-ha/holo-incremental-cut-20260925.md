# 独立 Incremental 索引截断：Holo 验收

2026-09-25 Asia/Shanghai，远程日志为09-24 UTC。ltfs-recovery-ibm-interop。补齐上一轮只命中周期Full的缺口；没有修改生产代码或线上配置。

## 场景与证据

原集群正常停机、TS1001L08归槽6。独立/home/rocky/tape-rs-incremental-cut-20260925、7500/7501，仅归属RT2502L8，正式版ltfsd SHA f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。测试带起点是上一轮正常停机的Full gen67；复用原专用文件，不额外prepare写入，确保下一次修改产生Incremental。

开始先用Client验证cut=old、set二进制值正确、delete属性不存在、文件内容正确。node3/round196执行对cut的REPLACE。新探针incremental-cut.py在唯一隔离Leader的ltfsd-exec线程附着strace，每次ioctl进入前延迟200ms；只有成功WRITE(6)的内容明确含ltfsincrementalindex才允许SIGKILL，否则解除跟踪并报未命中。

本轮一次命中：WRITE(6) CDB 0a0000091500，2325字节，status=0/resid=0，数据前缀明确为XML ltfsincrementalindex version2.5.0。紧接的WRITE FILEMARKS(1)处于延迟进入、unfinished状态时终止node3进程；没有执行其结束文件标记。

Client返回Indeterminate。新node1/round201恢复发现DP TruncatedIndex、IP Complete、chain_ok=true，以Full gen67为已提交视图，放弃块234..235，在EOD236追加Full收尾为gen68。没有应用未完成增量中的新cut值。

Client接管后验证：cut仍old，set二进制值保持，delete不复活，文件内容一致；版本仍(184,1)。另通过HTTP设置临时user.recovered二进制值再删除，分别201 committed/gen69、gen70，确认恢复后可以继续提交；再次Client验证原属性和内容通过。正常停机后两分区Full gen71。

## 收尾

隔离节点全停，strace退出，RT2502L8归槽7保留gen71。Holo /home/rocky/holo-incremental-cut-20260925/{before,after}保留介质证据。LE副本本轮未访问，仍为此前gen58；没有介质格式化、VM断电、Holo服务重启或物理TS4300操作。这是进程崩溃，不是存储断电。

原三节点恢复正式可写xattr版本，实际exe SHA均f8c238…，node1/term30/round256，TS1001L08仍gen72 Complete，TS1000L08 gen2。原9文件长度/SHA和完整list一致。三节点catalog31条，哈希均a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0。EE3节点available无任务，f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e；9publications ready。

## 检查与边界

新增门控Python探针的语法检查、git diff --check通过。复用上一轮已构建的Client example及正式服务端，本轮没有Rust语义变更，未重跑全套201软件测试/check/clippy。未发现需要生产修复的问题。

证据probes/results/incremental-cut-*包含真实SCSI调用、kill事件、Client中断/恢复/再次写入验证、各节点日志、最终XML、原环境恢复状态、EE与publication状态。探针probes/incremental-cut.py要求隔离目录、Leader、唯一RT2502L8归属和Incremental内容门控；测试目录内单独复制strace，未安装或修改系统软件。

现在Full和独立Incremental在“数据写入成功、结束文件标记前”的进程中断均有现场证据，设置/删除在201响应丢失后也有证据。仍不代表索引提交后但目录发布前、所有barrier/VCI故障点或存储掉电均已覆盖。全部工作树改动未提交。
