# xattr 增量提交成功后的 Holo VM 断电验收

2026-09-25，分支 ltfs-recovery-ibm-interop。沿用用户明确的共享 Holo 断电授权，直接停止 PVE VM510；本轮通过，没有发现需要修改生产代码的问题。

## 故障窗口和结果

原集群正常停止，TS1001L08 归槽6。隔离三节点目录 `/home/rocky/tape-rs-xattr-powercut-20260925/test-data`，端口7500/7501，只归属 RT2502L8，使用正式 ltfsd f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。

node1/round220 从 Full gen73 挂载。HTTP API 对 `/rc/xattr-fault-20260925` 依次设置 `user.powercut.binary`（00 ff + powercut + 00）、设置空值 `user.powercut.empty`、删除原 `user.set`，均收到201/committed，分别为gen74、75、76。随后暂停三个测试进程，避免正常卸载和额外checkpoint。

UTC 00:04:36.450 执行 `qm stop 510`，00:04:38.246 完成；立即启动，00:04:41.595 启动命令完成。Holo 起机后、测试进程重启前保存原始介质副本：IP Full73，DP Incremental76（block261，前一增量block258，基底Full73/block252），空值为自闭合Base64元素。原始XML仅作辅助证据，恢复结论来自下面的实际挂载和读取。

旧测试进程全部SIGKILL，新进程node1/round231实际从磁带恢复gen76、DP/IP尾部Complete、可写，并重新对账目录。验证通过：

- 二进制及空属性原值保留，已删除的set没有复活。
- 版本仍220.3，与成功回执后的版本相同，没有重放。
- 文件内容、长度、mtime不变；完整list和list中的13个文件SHA256均与断电前一致。
- 三节点SQLite目标行完全一致。

清理两个临时属性并恢复原二进制set，三次操作均201，gen77/78/79。最终DP/IP均Full79，测试带归槽7。LE副本TS1000L8没有访问或改写。

## 备份与现场恢复

Holo `/home/rocky/holo-xattr-powercut-20260925` 保存卸载状态测试带before、after-crash、清理后after、配置、SQLite backup及media-state；runtime.json权限0600，含9个实际handler的参数、环境、身份、工作目录和程序哈希。恢复后逐一比对全部9个运行参数与环境一致。

PVE快照 `before-xattr-powercut-20260925` 创建成功，但QEMU客体文件系统冻结因pool1权限失败，所以只视为崩溃一致性备份；测试带另有上述独立备份。没有回滚快照或恢复旧介质覆盖测试结果。

启动默认handler后恢复现场参数，首次SIGTERM等待10秒仍未退出，脚本断言停止；补入仅针对已确认publication进程的SIGKILL后恢复9个handler。原始失败日志保留。测试进程SIGSTOP也暂停了sudo父进程；最后恢复父进程运行以回收已经死亡的子进程，清除了僵尸，无测试进程残留。

原集群恢复node1/term32/round266，TS1001L08仍gen72 Complete，TS1000L08仍gen2。三节点实际程序哈希不变，catalog31条及哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0一致；原9文件长度/SHA256及完整list一致。EE三节点available、无活动任务，GPFS f3哈希cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e不变；Holo 9 publications ready。LE本地驱动Available，远程驱动在本地LE视图中Unavailable与基线一致。

## 检查与边界

新增显式条码和7501端点门控的HTTP探针 `probes/xattr-powercut.py`；语法检查和git diff --check通过。无Rust生产代码改动，未重跑Rust软件全套、fmt、check、clippy；本worktree无CodeGraph目录。

这是Holo VM硬停止后的已确认提交持久性验证。PVE宿主及底层存储一直运行，不能扩大为物理存储断电保证。此次未测试EE活动迁移中断、未确认提交时掉电、所有barrier/VCI故障窗口，也没有重新执行LE回写互操作；这些与之前已有进程截断、响应丢失、目录发布前窗口及LE往返证据分别记录。

原始材料：`probes/results/xattr-powercut-*`，包含三次回执、VM时间线、原始XML、恢复stat、三节点目录、13文件基线、清理结果、三个节点日志及原环境复核。改动未提交。
