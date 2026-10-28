# B 库单驱动离线移除：三节点与实带验收（2026-10-22）

范围：将 B 库 `55L3A78011W9LL01` 的驱动 `117018605B` 从 ltfsd 配置中移除，保留 `117018606F`。这是**逻辑退役**：旧驱动仍在物理带库内，且内有原来的 `8A0042L8`；未做拔线、拆机或设备完全缺席测试。A 库 `55L3A7802K19LL01` 继续由 EE 管理，本次候选没有获准访问 A 的 SG 设备。正式 17421 服务未替换。

软件实现是 `ltfsd --remove-drive-offline SERIAL --libraries-config <新配置>`。它要求三节点停机、同一库仅移除一台非末台驱动，拒绝未提交日志、错误序列号、同时增删驱动和伪造记录。配置变更写入原有链式记录；旧快照及日志回放丢弃该驱动策略，将指向它的排队/运行管理任务标为 `indeterminate`，含旧驱动的库存观测失效。被移除的序列号不得在后续配置里复用。

软件检查：完整串行 `cargo test -- --test-threads=1` 为 500 passed、0 failed、4 ignored；`cargo fmt --all -- --check`、`cargo check --all-targets`、`cargo clippy --all-targets --all-features -- -D warnings` 和 `git diff --check` 均通过。完整测试日志为 `/work/tmp/tape-rs-test-drive-remove-20261022.log`。在源自 207 的历史数据目录**本地副本**上实际运行新二进制，变更链从两步增长至三步，提交索引 `1002771`；Raft、两库目录和目录快照的 SHA256 均保持不变。[本地副本结果](../.scratch/ltfs-ha/probes/results/drive-remove-local-20261022/probe-success.json)不是硬件验收。

现场预检核对了 207/208/209 上 B 换带器 `/dev/sg5` 和两台驱动的 VPD 序列号；208 上 B 的 PR 注册表及持有者均为空，`AA0013L8` 在槽 1，保留驱动为空，退役驱动中的 `8A0042L8` 未移动。原正式 17421 三节点 executor 均为空。

第一轮隔离候选在三节点从前轮 R7 状态的副本启动，候选 systemd 单元均为 `DevicePolicy=closed`。三节点逐一运行离线移除，变更前提交索引均为 `1005342`，原 `raft.db`、目录和目录快照哈希不变。固定 208 入口列出 B 库只剩 `117018606F`，原文件任务 `4307040449069057` 仍为 `committed`。候选已停止，未访问 SG 设备。[控制面结果](../.scratch/ltfs-ha/probes/results/drive-remove-control-r1-20261022/acceptance.json)、[收尾](../.scratch/ltfs-ha/probes/results/drive-remove-control-r1-20261022/cleanup.json)。

第二轮在三节点复制第一轮状态，起初仍禁止所有 SG 访问。旧 owner 撤销后，仅 207 的候选单元获准访问 B 换带器 `/dev/sg5` 与保留驱动 `/dev/sg6`；退役驱动 `/dev/sg4`、A 库设备以及 208/209 的 SG 均未开放。207 获得 B owner 后，将 `AA0013L8` 从槽 1 装入保留驱动；恢复日志显示 IP/DP 均为第 31 代、两个 VCI 命中、无 IP 维护债务。随后读取既有文件 `/gap-readback-20261022/133d28d484394cfe972c2d4db046f57c-copy.bin` 的 262144 字节，SHA256 为 `4339d49b90cb0ba34206bc9c0fd8d04ccebb7c8a76e925837b121432d9c175df`，与前次实带记录一致。未发起新的文件 PUT、格式化或擦除；正常停机仍按原 LTFS 逻辑处理索引检查点。[实带结果](../.scratch/ltfs-ha/probes/results/drive-remove-physical-r2-20261022/acceptance.json)及[207 原始日志](../.scratch/ltfs-ha/probes/results/drive-remove-physical-r2-20261022/node-207-journal.log)。

第二轮先停持有者，再停其他节点，并在需要时显式退带归槽。脚本清场及一次独立只读复核均确认：三节点候选 MainPID 为 0、正式 17421 executor 为空，`AA0013L8` 回槽 1、`L00284L8` 留槽 4、`8A0042L8` 仍在旧驱动，保留驱动为空，B 三个 SG 设备的 PR 注册表及持有者均为空。临时 SSH ControlMaster 已关闭。[清场](../.scratch/ltfs-ha/probes/results/drive-remove-physical-r2-20261022/cleanup.json)、[独立复核](../.scratch/ltfs-ha/probes/results/drive-remove-physical-r2-20261022/post-close-independent.json)。

此验收只证明逻辑移除后保留驱动能接管并读回已知测试文件，不证明拆除旧驱动后的发现行为、全库所有文件、跨库副本/迁池、在线配置变更或正式服务升级。
