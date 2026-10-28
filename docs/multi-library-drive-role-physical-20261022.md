# 双带库驱动角色策略现场验收（2026-10-22）

在 207/208/209 三台物理节点上，克隆已停机的双库候选状态，运行独立端口 17862/17863。只有 209 获得 B 库 `55L3A78011W9LL01` 的保留驱动 `/dev/sg4`（序列号 `117018606F`）及换带器 `/dev/sg5` 权限；A 库、B 的逻辑退役驱动及其余候选节点均无 SG 权限。正式 17421 控制服务未替换，EE A 保持运行。

从固定 208 入口执行 `ltfsctl library rescan`（任务 `1011500`），再把保留驱动角色从 `mrg` 改为 `mr`（任务 `1011533`）并恢复为 `mrg`（任务 `1012260`）。三个任务均在 B 库 owner 73、round `1010819` 下达到 `success`。角色缩减后，管理库存显示驱动已归槽为空；恢复后三个节点查询的策略都是 `mrg`，驱动始终 enabled=true。该任务路径先收束写入并卸载本实例持有的介质，随后再改变调度策略。

恢复后从 B 库 AA0013L8 真实磁带读取 `/gap-cross-library-20261022/a-serial-fuse-split-r2.bin`，得到 1,048,576 字节，SHA-256 为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`，与原跨库复制结果一致。正常停机后 AA0013L8 回槽 1、保留驱动为空、B `/dev/sg4/5/6` PR 无注册或持有者、三节点候选 MainPID 均为 0，正式 17421 仍无设备 owner。通过标记和原始状态见 `.scratch/ltfs-ha/probes/results/drive-role-physical-r2-20261022/`。

R1 探针在任务提交前误要求冷接管后的驱动为空；实际上 owner 已自动装载 AA0013L8。该轮按断言退出并完成清场；R2 删除过早断言，验证管理任务自身的收束与卸载流程。探针分别位于 `.scratch/ltfs-ha/probes/drive-role-physical-20261022.py` 和 `.scratch/ltfs-ha/probes/drive-role-physical-r2-20261022.py`。Python 语法检查、`cargo test --test multilib_cluster -- --test-threads=1`、`cargo fmt --all -- --check`、`cargo check --all-targets`、严格 Clippy 和 `git diff --check` 通过。本轮没有更改 Rust 运行时代码，也没有部署正式双库服务。

此验收关闭保留驱动 `role` 策略的实体 owner CLI 路径。库退役、Raft 全局副本及物理迁池、其余管理写命令与正式双库升级故障矩阵仍待完成。
