# 双带库节点组策略现场验收（2026-10-23）

在 207/208/209 三节点的独立 B-only 候选上，从已停机的 `/root/tape-reconcile-objects-20261023/state` 克隆状态；仅 209 获 B 库保留驱动 `/dev/sg4` 和换带器 `/dev/sg5` 权限。A 库与逻辑退役驱动不给候选 SG 权限，EE A 保持运行。预检确认 B 库序列号 `55L3A78011W9LL01`、保留驱动 `117018606F`、AA0013L8 槽 1、AA0000L8 槽 2、L00284L8 槽 4、保留驱动空、B PR 空、正式 17421 无设备 owner。

固定 208 入口的 `library rescan` 任务 `1032964` 成功。创建临时节点组 `BTEST20261023` 后，非 owner 节点 71 的 `node assign` 到测试组任务 `1033020` 成功，再恢复 G0 的任务 `1033736` 成功；期间 B owner 仍为 73。保留驱动的 `drive assign 117018606F G0` 任务 `1035082` 成功。删除测试组后，三节点查询均显示节点 71 与保留驱动归属 G0。AA0013L8 上既有 1 MiB 文件实带读回 SHA-256 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`，卷代数 34、23 个索引条目及卷 UUID 均未变。

探针 `.scratch/ltfs-ha/probes/group-policy-physical-20261023.py` 的结果在同级 `results/group-policy-physical-20261023/`，`acceptance.complete` 与 `cleanup.complete` 均存在。清场后 AA13、AA00、L00284 在原槽、B 保留驱动空、B `/dev/sg4/5/6` PR 无持有者、三候选 MainPID 为 0、正式 17421 executor=null；EE A 的原 PR 未改变。探针 Python 语法检查、`group_role_and_tape_policies_have_real_scheduling_effect` 定向 Rust 测试和 diff 检查通过；本轮未修改 Rust 运行时代码或部署正式服务。前一轮全量串行测试的 500 通过/4 忽略不能算本轮重跑。

本实验验证了节点实际跨组与恢复、驱动幂等归属的管理任务路径。**没有验证保留驱动跨组迁移**：它是 B 库唯一配置内驱动，真实池仍在 G0；单独把驱动改到其他组会暂时失去该池的可用驱动，现有管理接口没有保证池、驱动和 owner 同时迁移的事务。此能力及正式升级矩阵仍未闭环。
