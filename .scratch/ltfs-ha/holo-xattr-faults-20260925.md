# 属性提交中断及响应丢失：隔离 Holo 验收

2026-09-25（Asia/Shanghai；远程日志为09-24 UTC）。分支 ltfs-recovery-ibm-interop。三个场景通过，没有修改生产逻辑或替换线上程序。

## 隔离与故障边界

原三节点正常全停、原TS1001L08归槽6。独立 /home/rocky/tape-rs-xattr-ha-20260925/test-data，从已关闭的上一轮隔离数据复制；7500/7501端口，仅归属RT2502L8。服务端使用正式版f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。Holo备份 /home/rocky/holo-xattr-ha-20260925/{before,after}。

测试起点LE2.4.0 gen58，专用文件/rc/xattr-fault-20260925内容固定，prepare创建cut=old、delete=old。只操作保留副本；没有VM/Holo断电、格式化或物理TS4300操作。三个故障均为隔离ltfsd进程SIGKILL；Holo自身持续运行，因此不能称为存储掉电验证。

## 结果

| 场景 | 实际故障点 | Client与恢复结果 |
|---|---|---|
| 属性提交中断 | node1/round162：DP索引WRITE(6) 47788字节成功返回，后续WRITE FILEMARKS(1)延迟进入且未完成时SIGKILL | Indeterminate；node3/round177识别TruncatedIndex，以gen63为已提交视图，放弃块220..221，追加收尾gen64；cut仍old，文件内容不变 |
| 设置属性响应丢失 | node3提交gen65，代理实际收到201 committed/task1后拦截；SIGKILL旧Leader再关闭客户端连接 | Indeterminate；新node1/round184读回00ff前缀的完整属性值；版本仍(177,1)，没有重放 |
| 删除属性响应丢失 | node1提交gen66，代理实际收到201 committed/task1后拦截；SIGKILL旧Leader再关闭连接 | Indeterminate；新执行者读回delete属性不存在，set属性保持正确；版本仍(184,1)，没有重放 |

验证每个阶段都重新读取文件内容。最终正常停机写Full gen67，测试带保留故障后状态。当前命中的中断索引为周期性Full（XML根ltfsindex），不是半条Incremental；不能将其扩大成所有索引类型和所有提交阶段都已现场覆盖。

## 故障点证据与首轮未命中

xattr-cut.py只接受RT2502L8门控，核对唯一ltfsd的exe位于独立测试目录、Leader及唯一测试带归属，再附着ltfsd-exec线程。strace只延迟ioctl，不更改SCSI返回码或数据。每次延迟500ms，脚本观察首个成功WRITE(6)，确认没有后续完成的WRITE FILEMARKS，立即SIGKILL。cut.trace明确显示XML索引数据已写入，下一条文件标记调用unfinished；新Leader日志独立证实TruncatedIndex及自动收尾。

首轮每次延迟2秒、90秒观察窗口，实际耗在重新挂载及历史索引链扫描，观察超时后自动解除跟踪，未杀进程，属性随后正常提交gen62。Client探针的Indeterminate断言因此失败，该轮记为未命中。先重新提交cut=old到gen63，再将延迟改为500ms/观察180秒，第二轮实际命中。没有把首轮普通提交算作故障通过。

响应代理仅监听tsclient 127.0.0.1:7593/7594，针对/xattrs的POST或DELETE各一次。保留服务端真实201内容，不伪造错误。两个日志均只有一次目标修改请求；结合接管后的原提交版本确认没有重放。设置/删除未在SCSI层注入中断，分别覆盖提交后、回应前的窗口。

## 恢复与校验

隔离全部节点、两个代理、strace控制进程退出，RT2502L8回槽7，保留gen67；LE的TS1000L8本轮未访问，仍为前轮gen58。原集群恢复正式可写xattr版，三节点实际exe SHA f8c238…一致，node3/term29/round251服务；原TS1001L08仍gen72 Complete，原TS1000L08仍gen2。

原9文件升级前后逐一长度/SHA256与最终完整list完全一致。三节点catalog31条，哈希均a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0，与开始一致。EE3节点available、无任务，f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e；9个Holo publications ready。没有更改原启动配置或原文件。

## 软件检查与材料

新增examples/hw_xattr_fault.rs，显式条码/端点门控，使用真实Client接口验证Indeterminate、属性值、删除、数据和版本。兼容glibc2.34的zigbuild成功；cargo check --all-targets通过，针对新example的clippy通过（库25条既有告警）；rustfmt example、Python语法和git diff --check通过。本轮无生产逻辑改动，未重跑全套201软件测试，沿用既有结果。

probes/xattr-cut.py、复用ha-fault-proxy.py；probes/results/xattr-ha-*保存Client结果、真实提交回执、代理请求、SCSI跟踪、控制事件、各节点日志、最终原生XML与原环境校验。软件日志/tmp/xattr-fault-build.log、/tmp/xattr-ha-{check,clippy}.log。

后续可补独立Incremental索引的截断/提交后目录发布前故障，或继续剩余LE功能；当前跨带属性EXDEV、根目录虚拟控制属性等边界未改变。全部工作树改动未提交。
