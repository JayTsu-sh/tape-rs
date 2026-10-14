# 读取故障切换修复：原集群统一部署

2026-09-24，分支 `ltfs-recovery-ibm-interop`。已将通过隔离Holo验收的ltfsd统一部署原三节点。本轮只做发布与验收，没有生产代码变更。

## 发布和备份

- 正式目录：三节点 `.71/.72/.74` 的 `/home/rocky/tape-rs-read-prod-20260924`。
- 沿用隔离验收的同一二进制，三个实际 `/proc/<pid>/exe` SHA256均为 `e1a8c4fde9bffdb9fcee1108d4cd4c5bbbaa537b974d13d3838b02e3d302b410`。
- 原 `/home/rocky/tape-rs-ltfs25-20260924/start.sh N` 已切到新版；新目录另存start.sh。原数据目录 `/home/rocky/ltfsd-data`、7400/7401端口及所有参数保持不变。
- 同时SIGTERM正常停机，确认设备卸载及预留释放后，各节点保存权限0600的 `pre-upgrade.tgz`，包含关闭的数据目录、旧rename版ltfsd和旧启动脚本。备份是离线恢复材料，不应直接覆盖已推进的Raft状态。
- 没有新旧服务端混跑；测试副本未归属原集群，隔离测试服务未启动。
- Client及FUSE无需更新，继续使用原rename发布版本：tsclient常用 `/home/rocky/ltfsctl`，FUSE路径 `/home/rocky/tape-rs-rename-prod-20260924/tape-fuse`。

## 实际验收

升级前node1/term23/round197、TS1001L08 gen58 Complete。原9文件经HTTP逐一校验长度/SHA256，全部一致；归属只有TS1000L08、TS1001L08；EE无活动任务。

升级后三节点均运行新版，原Client/FUSE通过新建空缓存的真实内核挂载读取原9文件：全部长度、SHA256及只读MAP_SHARED mmap哈希一致。最终HTTP list与原9文件清单完全相同。仅只读验收，没有新增文件、改写或清理磁带路径；验收FUSE已正常卸载。

最终node1/term24/round202，TS1001L08仍gen58 Complete、39MiB可用。三节点applied205、catalog29条，排序序列化哈希均 `3bf72f0e246b39c695afa54c1dc85fa83f4c046a58eecd23482c8f50df40e7c0`，与升级前一致。TS1000L08仍gen2。

EE三节点available、无活动任务，f3 SHA256仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`；9个Holo publications均ready。

## 检查与证据

- `probes/read-upgrade-fuse.py`显式门控唯一验收挂载点，只读原文件及mmap。
- `probes/results/read-upgrade-*`保存升级前清单、停机PR释放、FUSE结果、最终节点/目录/程序一致性、EE及targets状态。
- Python语法与git diff --check通过。二进制未重新构建；沿用此前197测试通过/1忽略、check/clippy与隔离Holo故障验收，本轮未重跑软件全套。全仓fmt历史差异仍在。
- 本轮接触Holo虚拟带库的正常停机/卸载、PR释放和接管读取，没有故障注入、SIGKILL、断电、格式化或物理TS4300操作。真实抢占时503/get/get_to/get_range结果见 `holo-read-failover-20260924.md`，本轮未在原数据带上重复注入。

读取丢失设备资格的修复已上线；普通500不会被盲目重试，部分流式输出后连接中断仍需调用方丢弃输出后重试。跨带rename、可写xattr和符号链接创建等剩余LE能力仍待实施。全部源码、探针和记录未提交。
