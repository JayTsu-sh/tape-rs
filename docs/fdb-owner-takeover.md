# FDB owner 接管前置检查

原生 FDB owner 收到 `Takeover` 后，先核对本轮 Fencing 多数派许可、本地冻结点、停机和 catalog pending 状态，再通过严格 SDK 读取活动路由及 owner epoch。路由必须与启动时绑定一致，owner epoch 必须有效且不高于本轮。任一检查失败均不打开接管设备、不调用 `reservation::fence`，关闭该轮许可并返回带库、节点、轮次和原错误的 `FenceFailed`。

这修复了共享 FDB 已不可达时，节点仍先注册/抢占设备 PR、随后才在 Recover 阶段失败的顺序。可达性核验最多等待 5 秒，每 25 毫秒检查许可和本地状态；等待中失权会终止本次检查。预算只针对 FDB 元数据等待，不改变 SCSI 或机械超时。后台能力发现仍允许进行既有只读设备探测。

前置读取不会推进 FDB owner，也不授予设备执行权。通过后仍须逐台设备执行原有 fencing、读回核对、提交 Fenced 控制记录，再由 Recover 核对路由/PR并推进 owner epoch及恢复介质。若 FDB 在前检成功后才故障，已提交的 CDB 无法撤回；后续检查继续决定是否能完成接管，不把一次读取当成跨存储与设备的事务锁。

该顺序只改变原生 FDB owner；旧 SQLite actor、持久格式、用户接口、PR 协议和设备超时保持原行为。首次版本直接采用此顺序，不保留旧顺序兼容开关。

`tests/fdb_owner_takeover.rs` 的显式隔离 FDB 测试运行真实 owner 消息入口，使用模拟设备覆盖不可达、等待中失权、错路由/冻结点、停机、pending、缺许可、FDB 更高 epoch，以及正常 fencing 不提前推进 owner。真实设备验证另行记录，模拟结果不代替现场 PR generation 核对。


## 真实 B 库验证（2026-11-06）

源码 `9003d1eec74addb1338b83d9bdff075d1a96f245` 已在 207–209 三节点的原隔离 unit、namespace 和新 45152 FDB 集群部署。正常接管及严格 SDK 读取通过，库存仍为 8 盒磁带、2 个驱动器。

停止新 FDB 的 208/209 两个原 server，确认真实 1031 读取超时与就绪接口 503。三个候选节点均记录共享事实源前检失败。故障前、全部候选失败后以及再观察 35 秒后的三次 PR IN 核对结果相同：changer generation 119280、TD8 generation 299、HH8 generation 453，注册键与持有者均为原 `0x4c0200000002555c`。故障窗口未再出现前一版本的反复 fencing。

恢复原 FDB unit/data/cluster 后，三节点就绪、完整新采样和严格 SDK 读取恢复；原通知 56/57 的事件字节及消费者 ACK 57 保持不变。最终 FDB、控制状态及三设备 PR 独立核对一致：owner node 2 / epoch 154042，key `0x4c020000000259ba`。新集群恢复 healthy/double/3 processes，旧 45151 集群在故障期间和恢复后分别通过健康检查。

本次无 MOVE、格式化或介质文件读写，A 库与备用 changer 域继续隔离。没有模拟网络故障瞬间撤回已提交 CDB，也不扩大为断电或单盘损坏验证。证据在 `/work/tmp/tape-fdb-owner-preflight-physical-20261106/` 的 `QUORUM-COMPLETE.json`、`quorum-pr-stable-window.json`、`SDK-recovery.json` 和 `OWNER-AUDIT-final.json`。
