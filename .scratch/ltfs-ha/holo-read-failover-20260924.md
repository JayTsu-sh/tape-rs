# 读取失去设备所有权：Holo 隔离验收

2026-09-24，分支 `ltfs-recovery-ibm-interop`。服务端修复在隔离三节点验收通过；原集群恢复原发布版本，尚未升级此修复。

## 环境和范围

原三节点 .71/.72/.74 正常全停，TS1001L08归槽6。独立 `/home/rocky/tape-rs-read-ha-20260924/test-data` 从已关闭的rename测试数据复制，7500/7501端口，仅归属RT2502L8副本。测试带仍为LE回写的2.4.0 gen46；同UUID副本绝不能归属原集群。

三节点测试ltfsd SHA256均 `e1a8c4fde9bffdb9fcee1108d4cd4c5bbbaa537b974d13d3838b02e3d302b410`。测试配置interval60秒，避免例行检查干扰精确的读故障窗口；未修改生产源代码或生产启动入口。本轮没有写入文件、格式化、断电、SIGKILL或物理带库操作。

## 精确故障窗口

1. 三个Client先各自读取 `rc/ha-upload.bin`，缓存旧node3/round119。4MiB内容是每字节 `offset % 251`，逐字节校验。
2. 只读透明代理在tsclient回环7592转发至旧node3，并记录真实HTTP状态；不伪造状态或错误。Client还配置三节点直接地址。
3. 使用strace附着旧Leader主线程和独立ltfsd-exec线程：主线程下一次futex进入前延迟20秒，执行线程下一次ioctl进入前延迟12秒。确认两线程TracerPid有效后才允许Client继续。只延迟系统调用，不注入错误码或更改CDB；不安装防火墙或修改网络规则。
4. 主线程暂停造成Raft通信中断，新node2/term13/round124完成设备PR隔离并从gen46恢复。旧读取的LOCATE(16)随后执行，设备实际返回status0x02、sense06/2A05。trace显示该调用DELAYED约12秒。
5. 旧服务端返回503 drive_busy，detail保留原始status/sense；关闭FileService并上报轮次119丢失资格。旧执行线程trace中仅该次ioctl，没有失败后的卸带或抢回预留。旧Raft恢复后转为Follower。
6. 保留旧缓存的get_to自动换节点，输出恰好4194304字节、逐字节正确；get全量与get_range(12345,65536)也通过。后两次旧节点请求返回503 not_serving，随后切换。没有手动重建Client指向新Leader。

延迟时间用于制造确定窗口，不能解读为正常故障恢复延迟指标；这是线程停顿下的设备接管场景，不代表任意网络分区、所有SCSI位置或物理介质故障均已覆盖。

首轮选出的node3没有系统strace，延迟工具启动失败，Client仅完成普通读取；该轮只算基线，不作为故障通过证据。随后从node1复制strace到node3的测试目录，校验可运行并确认TracerPid后重新执行；上述结果均来自第二轮。原系统软件包未修改。

## 证据和软件检查

- `examples/hw_read_failover.rs`：显式RT2502L8/测试门控路径，3个独立Client、完整字节断言。
- `probes/read-ha-proxy.py`：限制GET、限定三节点地址、只监听127.0.0.1。
- `probes/results/read-ha-client.log`：get_to/get/get_range通过。
- `read-ha-proxy.log`：真实503与06/2A05。
- `read-ha-read-delay.log`、`read-ha-raft-delay.log`、`read-ha-node74.log`：调用窗口、设备返回和Lost/Follower状态。
- `read-ha-after-cluster.json`、`read-ha-shutdown.log`：新执行者服务、旧轮次放弃及正常卸载/释放PR。
- release ltfsd/example构建成功，cargo check --all-targets成功，新example Clippy完成（库既有警告），example rustfmt、Python语法及git diff --check通过。全套197通过/1忽略与all-features Clippy沿用上一轮，本轮无生产逻辑更改，未重跑全套；全仓fmt历史差异仍在。

## 恢复结果

测试三节点正常停止，全部PR释放，RT2502L8归槽7。代理、两项strace附着均结束，未留下测试FUSE或网络规则；隔离数据库和日志保留。

原集群恢复node1/term23/round197、TS1001L08 gen58 Complete、39MiB可用。三个实际运行exe SHA256均原rename发布版 `dc1e70ac7080e69e5104f0d8bd709d2f6230719993c61b433f0e542db9ded0ac`。原9文件长度/SHA256及最终list与升级前清单完全相同。EE三节点available、无任务，f3 SHA256仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`，9个Holo publications均ready。

下一步统一部署已验收的服务端修复。现有Client无需增加通用500重试；部分输出后的IO错误仍需要调用方丢弃输出再重试。全部改动和证据未提交。
