# B 库 owner 管理 CLI 现场验收（2026-10-22）

在 207/208/209 三台物理节点上，使用已有真实状态目录的独立副本启动双库候选；正式 `17421` 服务未替换，EE 持有的 A 库未授权给候选。候选初始均为 `DevicePolicy=closed`，确认旧 B owner 撤销后，仅给 207 的 B 库换带器 `/dev/sg5` 和保留驱动 `/dev/sg6` 授权。逻辑退役驱动 `/dev/sg4` 与 A 库设备始终不在候选许可中。B 库序列号为 `55L3A78011W9LL01`，保留驱动序列号为 `117018606F`。

R3 从固定 208 入口以 `ltfsctl --library 55L3A78011W9LL01 library rescan` 提交任务 `1006065`，终态 `success`，库存观测的 owner 为 71、round 为 `1005387`，配置内驱动仅 `117018606F`。原 262144 字节文件先实带读回，SHA-256 为 `4339d49b90cb0ba34206bc9c0fd8d04ccebb7c8a76e925837b121432d9c175df`。随后对原本已启用、角色 `mrg` 的保留驱动执行幂等 `ltfsctl drive up 117018606F`，任务 `1006107` 终态 `success`，策略仍为启用和 `mrg`。任务执行期间卷按现有管理流程卸载归槽；执行器重新选带就绪后，相同文件再次从磁带读回，长度和 SHA-256 不变。两个任务在三节点查询均为成功。

正常停机后，AA0013L8 在槽 1、保留驱动为空、原 8A0042L8 仍在逻辑退役驱动内；B 库 `/dev/sg4/5/6` 的 PR 注册与持有者均为空，三台候选 MainPID 为 0，正式 `17421` 服务没有设备 owner。R3 原始任务、库存、哈希与清场证据在 `.scratch/ltfs-ha/probes/results/drive-management-physical-r3-20261022/`，`acceptance.complete` 和 `cleanup.complete` 均存在；所用探针为 `.scratch/ltfs-ha/probes/drive-management-physical-r3-20261022.py`。候选沿用此前构建的二进制；本轮没有新运行时代码编译或部署。

R1 探针误将 URL 传给只接受 `host:port` 的 CLI，管理命令未提交。R2 已完成两项任务，但在 `drive up` 终态出现后立即 GET，执行器尚未重新就绪，HTTP 暂返 503；R3 改为等待就绪后验证。R1/R2 均已单独清场，不能计作完整读回通过。这个窗口说明管理任务成功只表示策略操作完成，不保证数据面即时可读；用户调用方应按服务就绪或可重试的 503 处理。

2026-10-23 又从上一轮已停机候选状态独立克隆 B-only 三节点候选，209 仅获 B `/dev/sg4/5` 权限，固定 208 入口。显式 `library rescan` 任务 `1023953` 后，对真实保留驱动 `117018606F` 执行 `drive down` 任务 `1023999` 与 `drive up` 任务 `1024703`，三项终态均为 `success`，策略由 `enabled=true` 变为 `false` 再恢复 `true`，角色仍为 `mrg`。`down` 执行器先将 AA0013L8 归槽，`up` 后原 1 MiB 文件重新实带读回，SHA-256 为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。高文件数 AA0000L8、L00284L8 始终在原槽。`acceptance.complete`/`cleanup.complete`、任务状态、前后策略、哈希和独立清场记录见 `.scratch/ltfs-ha/probes/results/drive-down-up-physical-20261023/`。停机后 AA13 槽 1、保留驱动空、B 三设备 PR 空、候选 MainPID 为 0、正式 17421 无 owner；EE A 保持运行。这关闭了保留驱动 `down → up` 的现场路径，没有验证对其他驱动的操作或正式服务部署。

本验收没有改变驱动策略，也没有执行新文件 PUT、格式化或擦除。其他磁带管理写命令、非幂等驱动策略、两库同时拥有真实设备、跨库副本/迁池和正式服务升级仍未通过现场矩阵。
