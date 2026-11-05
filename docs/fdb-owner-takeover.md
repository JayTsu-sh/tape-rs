# FDB owner 接管前置检查

原生 FDB owner 收到 `Takeover` 后，先核对本轮 Fencing 多数派许可、本地冻结点、停机和 catalog pending 状态，再通过严格 SDK 读取活动路由及 owner epoch。路由必须与启动时绑定一致，owner epoch 必须有效且不高于本轮。任一检查失败均不打开接管设备、不调用 `reservation::fence`，关闭该轮许可并返回带库、节点、轮次和原错误的 `FenceFailed`。

这修复了共享 FDB 已不可达时，节点仍先注册/抢占设备 PR、随后才在 Recover 阶段失败的顺序。可达性核验最多等待 5 秒，每 25 毫秒检查许可和本地状态；等待中失权会终止本次检查。预算只针对 FDB 元数据等待，不改变 SCSI 或机械超时。后台能力发现仍允许进行既有只读设备探测。

前置读取不会推进 FDB owner，也不授予设备执行权。通过后仍须逐台设备执行原有 fencing、读回核对、提交 Fenced 控制记录，再由 Recover 核对路由/PR并推进 owner epoch及恢复介质。若 FDB 在前检成功后才故障，已提交的 CDB 无法撤回；后续检查继续决定是否能完成接管，不把一次读取当成跨存储与设备的事务锁。

该顺序只改变原生 FDB owner；旧 SQLite actor、持久格式、用户接口、PR 协议和设备超时保持原行为。首次版本直接采用此顺序，不保留旧顺序兼容开关。

`tests/fdb_owner_takeover.rs` 的显式隔离 FDB 测试运行真实 owner 消息入口，使用模拟设备覆盖不可达、等待中失权、错路由/冻结点、停机、pending、缺许可、FDB 更高 epoch，以及正常 fencing 不提前推进 owner。真实设备验证另行记录，模拟结果不代替现场 PR generation 核对。
