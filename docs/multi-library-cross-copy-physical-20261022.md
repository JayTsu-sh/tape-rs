# A→B 跨物理带库显式文件复制验收（2026-10-22）

在 207/208/209 三台真实节点上，以既有双库状态的独立副本启动当前源码构建的 `ltfsd` 和 `ltfsctl`。A 库 `55L3A7802K19LL01` 的 RC0018L9 只作为客户端复制的源，B 库 `55L3A78011W9LL01` 的 AA0013L8 作为目标；目标路径为 `/gap-cross-library-20261022/a-serial-fuse-r2.bin`。没有向 A 提交用户写入，也没有格式化或擦除。候选初始全部 `DevicePolicy=closed`；EE 无活动任务、正常停止并释放 A 的 PR 后，仅向 207 开放 A 的 `/dev/sg2/3` 和 B 的 `/dev/sg5/6`。B 逻辑退役驱动 `/dev/sg4` 始终不在候选许可中，208/209 不获 SG 权限。正式 `17421` 服务未替换。

从固定 208 入口运行 `ltfsctl --library 55L3A7802K19LL01 --pool 24c72d49-ab09-470e-aab4-7e8f48464faf copy /serial-fuse-20261021.bin /gap-cross-library-20261022/a-serial-fuse-r2.bin --to-library 55L3A78011W9LL01 --to-pool 10a0f22c-d50b-4c92-b057-90b35b1b7e1b`。提交前确认 B 目标路径不存在。复制后，A 源仍在 RC0018L9、版本 `[1000135,1]`；B 目标在 AA0013L8、版本 `[1005440,1]`。两者均为 1,048,576 字节，SHA-256 均为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。客户端完成源版本复核、目标落带及目标读回；控制脚本又独立 GET 两端并核对长度和哈希。

正常停机并恢复 EE 后，RC0018L9 在 A 槽 7、AA0013L8 在 B 槽 1，B 保留驱动为空，8A0042L8 仍留在未授权的旧驱动内。B `/dev/sg4/5/6` 的 PR 无注册或持有者；A `/dev/sg2/3` 恢复原 EE type-3 持有键。EE 208 节点为 available，三台候选 MainPID 为 0，正式服务 executor 为 null。R2 的输入、两端 stat、CLI 输出、哈希及清场证据见 `.scratch/ltfs-ha/probes/results/cross-library-copy-physical-r2-20261022/`，其中 `acceptance.complete` 与 `cleanup.complete` 均存在。

停机后又从 R2 状态独立克隆一个三节点 B-only 候选，仅允许 207 访问 B `/dev/sg5/6`，不停止 EE 或开放 A 设备。冷挂 AA0013L8 后从实带再次读回目标，长度和 SHA-256 不变；清场后 AA0013L8 回槽 1、B PR 清空、EE A 的 PR 保持原状。证据及双完成标记位于 `.scratch/ltfs-ha/probes/results/cross-library-copy-cold-read-r1-20261022/`。

第一次 R1 探针因 Python `http` 名称遮蔽而在候选控制查询阶段退出；EE 未停止、SG 权限从未开放，候选已清场。R2 使用全新目录完成实带验收。软件侧 `multilib_cluster` 定向测试通过，兼容构建的 `ltfsd`/`ltfsctl` 最高 GLIBC 需求为 2.34，SHA-256 分别为 `971d3e661f481ceca7216109bd791635bcb8e846cbdd70f54f1353b57c02d4f2` 和 `652d05c127f248f0d58e1ec65f20e7a81f262e610f912feb0691696c07e562e0`；探针 Python 语法检查通过。本轮没有修改 Rust 运行时代码。

这证明**显式普通文件内容复制**能跨两台物理库完成并在重启后从目标带读回。现有目录仍按库和池隔离，未建立用户文件的全局身份、副本集合或自动同步；源覆盖/删除、目标故障读取与物理迁池尚不具备同一个 Raft 提交合同。目标文件作为验收证据保留在隔离候选状态和 AA0013L8 上，不把本次复制称为 EE 式副本管理或迁池。
