# 三节点升级与 Client/FUSE 故障切换实测

日期：2026-09-24。分支 `ltfs-recovery-ibm-interop`。结果：统一升级完成，三项限定故障场景全部通过。

## 升级与测试隔离

- 正常停止原三节点后，各节点备份 `/home/rocky/ltfsd-data`、旧ltfsd和启动脚本至 `/home/rocky/tape-rs-xattr-ha-20260924/pre-upgrade.tgz`（约2.8 MiB）。备份是恢复材料，不应在后续状态推进后直接回滚Raft目录。
- 发布目录为 `/home/rocky/tape-rs-xattr-ha-20260924/`，三节点 `/proc/<pid>/exe` 实测均指向其中ltfsd，SHA-256均为 `59faf2c49915f3ce22772efd9b5ce8fff028c2c9b697a91be7bbb969e5a8ec69`。
- 原启动入口 `/home/rocky/tape-rs-ltfs25-20260924/start.sh <1|2|3>` 已改为新二进制，原data-dir不变。tsclient同目录提供新版ltfsctl、tape-fuse。
- 升级后先启动原集群验证TS1001L08仍gen39且原探针哈希不变，再正常停止原服务开始隔离测试。
- 三台真实实验VM .71/.72/.74，各有自己的iSCSI发起方。测试用7500/7501端口、独立 `test-data`，仅归属TAPERS/RT2502L8专用副本。原TS1001L08未归属测试集群、不写入；不将与TS1000L08同UUID的测试副本归属原集群。
- 一次性bootstrap入口只用于通过已有Admin命令登记原rc池UUID，然后全部停止，改用正式发布的ltfsd运行全部测试。没有在生产路径加入故障钩子。
- 本轮无格式化、无介质创建/删除、无Holo VM断电。故障是明确目标ltfsd进程的SIGKILL，不是物理主机或磁带断电。

## 故障注入与结果

使用tsclient上的单次HTTP代理，对指定方法/路径截断或扣住响应。代理写event文件后，先SIGKILL目标Leader，再允许代理关闭连接。其余请求透明转发；Client还配置三节点真实端点。事件与代理日志记录精确窗口，不靠随机sleep碰撞故障。

| 场景 | 注入点 | 接管 | 结果 |
|---|---|---|---|
| Client 4 MiB上传 | PUT仅转发65536字节，剩4128768字节时停止Leader | node3/round6 → node1/round14 | Client查询后重传，attempts=2、committed=true；4 MiB逐字节相等 |
| FUSE fsync成功响应丢失 | 服务端已返回201 committed gen10，但尚未转给客户端；停止Leader并丢弃响应 | node1/round14 → node2/round21 | fsync成功返回；新Leader查询确认哈希，索引仍gen10，无重复写入；独立Client下载与预期相等，二进制xattr保留 |
| Client删除成功响应丢失 | 服务端已返回200 committed gen11，但尚未转给客户端；停止Leader并丢弃响应 | node2/round21 → node3/round26 | 返回Indeterminate，不自动重放DELETE；确认路径已删除后创建同名新版本，新版本保持完整 |

每次故障后重启故障节点并确认跟随当前Leader，再开始下一次故障。接管日志确认三个设备读取旧PR key并完成新轮次隔离，不是同主机共用一个iSCSI发起方的模拟切换。

FUSE用真实内核挂载和全新缓存，对已有 `rc/edit.bin` 偏移100写入 `HA-FSYNC-RESPONSE-LOST`，fsync后的独立下载SHA-256为 `84597e955e5de74231acdcc939fd74098ef372c0d2356268e25e6c58598ae8fd`，`roundtrip.binary`仍为 `00ff4c452d726f756e6474726970`。
fsync日志约18秒包括人工保持代理窗口的时间，**不是自动故障恢复延迟指标**。

全部故障后再次验证最早确认成功的上传、重建后的删除路径、修改文件xattr。三节点最后applied_index均29、term5，测试带gen12。正常停止测试服务后生成Full gen13、UNLOAD、释放PR，再将RT2502L8由第二驱动归槽7（地址1030）。初次库存读取早于计划停机释放PR，得到reservation conflict；等待日志确认释放后重试成功，没有自行抢占预留。

## 最终运行状态

- 原集群已恢复并持续运行**新版**：node3 Leader，term13，round121，TS1001L08 gen39/Complete、15条目、约48 MiB可用。
- 原探针 `rc/_probe.bin` SHA-256仍为 `49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7`。
- EE三节点available、无活动任务，原 `/gpfs/tapers-del/f3` SHA-256仍为 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`。
- 测试FUSE已卸载、代理已停止、测试服务已停止。RT2502L8回槽保留测试数据；LISA副本TS1000L8未触碰。隔离数据库和日志保留，**不能将测试服务与原服务同时启动争抢同一逻辑库**。
- 本次新增门控 `examples/hw_client_failover.rs` 及探针/证据文档，无新的生产逻辑修改。没有提交代码。

## 验证及范围

最终 `cargo test` 174通过、1忽略；`cargo check --all-targets`、`cargo clippy --all-targets --all-features` 成功，保留既有告警。新example格式和Python探针语法检查通过，diff-check通过；全仓fmt仍有历史差异。

本轮验证明确覆盖上传体中断、已提交fsync响应丢失、已提交删除响应丢失及跨主机PR接管。不代表任意SCSI写入阶段崩溃、网络脑裂、物理介质掉电或全部并发竞争均已现场验证；此前LE/FUSE rename等剩余功能仍待实施。

可重复材料：`examples/hw_client_failover.rs`、`probes/ha-fault-proxy.py`、`probes/ha-fsync.py`、`probes/ha-run-client.sh`、`probes/ha-start-test.sh`。它们含实验环境显式目标，重跑前必须确认原集群已正常停止、测试副本和路径状态；已有测试文件不能盲目再次create-only。

日志在 `probes/results/`：
- `ha-upgraded-original.log`、`ha-restored-original.log`、`ha-restored-ee.log`
- `ha-{upload,fsync,delete}.event` 与 `ha-{upload,fsync,delete}-proxy.log`
- `ha-{upload,fsync,delete}-client.log`、`ha-fsync-cold-read.log`、`ha-final-verify.log`
- `ha-node1.log`、`ha-node2.log`、`ha-node3.log`
