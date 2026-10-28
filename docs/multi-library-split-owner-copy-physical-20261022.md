# 分散 owner 跨库显式复制现场验收（2026-10-22）

本轮在 207/208/209 三台物理节点上，从已停机的双库候选状态克隆独立目录，使用当前源码构建的 `ltfsd`/`ltfsctl`。固定 208 HTTP 入口接收复制命令，A 库 `55L3A7802K19LL01` 由 207 持有，B 库 `55L3A78011W9LL01` 由 209 持有。207 只获 A 驱动 `/dev/sg2` 与 changer `/dev/sg3` 权限；209 只获 B 保留驱动 `/dev/sg4` 与 changer `/dev/sg5` 权限；208 和 B 逻辑退役驱动均不向候选开放 SG。运行前逐节点核对正式 `17421` 无设备 owner、VPD 序列号、库存、PR、端口、EE 无任务和候选目录。EE 停止并释放 A 的 PR 后才开放候选设备权限；未替换正式服务。

源文件为 A 库 RC0018L9 的 `/serial-fuse-20261021.bin`，目标是 B 库 AA0013L8 的新路径 `/gap-cross-library-20261022/a-serial-fuse-split-r2.bin`。命令从 208 运行 `ltfsctl --library 55L3A7802K19LL01 --pool 24c72d49-ab09-470e-aab4-7e8f48464faf copy ... --to-library 55L3A78011W9LL01 --to-pool 10a0f22c-d50b-4c92-b057-90b35b1b7e1b`。复制前目标不存在；复制后 A 源版本 `[1000135,1]` 不变，B 目标版本 `[1006966,1]`。源与目标各 1,048,576 字节，客户端读回及独立 HTTP GET 的 SHA-256 均为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。验收时集群明确报告 A owner 71（207）、B owner 73（209）。

停机时先让只读 A owner 207 在多数派下正常退出（挂载时 IP/DP 均为第 125 代、无维护债务）；再以 SG 全关闭的 207 控制进程维持多数派，让 B owner 209 停机。209 日志明确记录 B 的第 34 代 `IP checkpoint=true`，随后卸载并释放 PR。清场确认 RC0018L9 回 A 槽 7、AA0013L8 回 B 槽 1、B 保留驱动空、8A0042L8 仍在退役驱动内；B PR 无注册/持有者，EE 在 208 恢复 available，A 恢复原 type-3 PR，正式 `17421` 仍无 executor。R2 原始证据、两节点停机日志和双完成标记位于 `.scratch/ltfs-ha/probes/results/cross-library-copy-split-r2-20261022/`。

随后从 R2 停机状态再次克隆一个 B-only 三节点候选，仅允许 209 访问 B 保留驱动与 changer，EE A 不停机、A 设备不开放。冷挂 AA0013L8 后从实带 GET 目标，长度与 SHA-256 相同；候选停机、AA0013L8 回槽 1、B PR 清空，正式服务状态未变。证据及双完成标记在 `.scratch/ltfs-ha/probes/results/cross-library-copy-split-cold-r1-20261022/`。

最初 R1 因 SSH 复用连接拒绝，在只读预检第一步退出；没有启动候选、停 EE 或开放 SG，清场标记见 `.scratch/ltfs-ha/probes/results/cross-library-copy-split-r1-20261022/`。R2 改用逐次认证后通过。探针 Python 语法检查、`multilib_cluster` 定向集成测试通过；本轮未改 Rust 运行时代码。没有格式化或擦除，也没有向 A 提交用户写入。

本次关闭的是**分散 owner 下显式内容复制的物理验收缺口**。它没有建立全局文件身份、副本集合、覆盖/删除同步、故障读取或源带回收事务；不能据此宣称 EE 式跨库副本管理或迁池已经实现。
