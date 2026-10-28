# 真实带库序列号入口与 FUSE 现场验收（2026-10-21）

本轮将真实 changer 序列号作为唯一带库身份，覆盖配置、Raft 作用域、目录路径、HTTP 发现与路由、ltfsctl 和 FUSE。配置不接受自定义 id，l8/l9 URL 返回 404；按用户要求不做旧别名配置或状态迁移。配置和调用方式见[多带库说明](multi-library.md)。

## 软件验证

`cargo test`：457 通过、4 忽略；`cargo test --all-features`：459 通过、11 忽略。fmt 全量、check all-targets、clippy all-targets all-features 及 diff-check 均通过。新增测试覆盖序列号唯一身份、拒绝别名配置、相同文件路径的分库隔离、数据和管理转发、选库后提示地址不改变作用域、FUSE 远程选库与本地设备模式互斥。默认测试不访问物理设备。

日志及源码哈希：[软件证据](../.scratch/ltfs-ha/probes/results/library-serial-20261021/verification.json)。首次开发阶段测试因预期仍为旧别名而失败的日志保留在 cargo-test.log，最终日志为 cargo-test-final.log 和 cargo-test-default.log。

## 物理数据面

三台真实节点 lisa-1/2/3（10.131.7.207/208/209），固定 VIP 10.131.7.219:17821。每台运行一个 ltfsd Raft 节点，设备 owner 最初同在 207，随后分散到 207 和 209；客户端挂载始终位于 208 并访问同一 VIP。

| 唯一带库标识 | 实际写入介质 | FUSE 提交时索引代数 | 新文件 |
| --- | --- | --- | --- |
| 55L3A7802K19LL01 | RC0018L9 | 124 | /serial-fuse-20261021.bin，1 MiB，内容 A |
| 55L3A78011W9LL01 | AA0013L8 | 16 | /serial-fuse-20261021.bin，1 MiB，内容 B |

208 上分别以 `tape-fuse --library <序列号>` 建立两个真实内核 FUSE 挂载，客户端进程仅允许 /dev/fuse，禁止访问 SG。通过普通文件写入和显式 fsync 确认落带；HTTP stat 核对条码、长度、代数和 SHA256，随后从 HTTP 实际读取校验。两文件 SHA256 分别为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56` 和 `5ae9782017a68037004b2bf806c77d324db4d915ed3725d84eb3121b2ad16061`。

撤销 207 对第二库的访问后，第二库转由 209 持有，第一库仍由 207 持有。卸载后以新挂载会话重新读取两个文件并校验 SHA；分别执行 `ltfsctl --library <序列号> get/stat`，下载内容均一致。本轮实际写入经 FUSE 完成，未单独执行 ltfsctl put。

另通过序列号 HTTP 入口实读上轮 R3 的 6 个 4 MiB 文件，以及 AA0013L8 原 64 MiB perf64m.bin 和 48 字节 marker，SHA 均保持。这里只核验列出的 8 个既有文件，不代表全介质所有内容哈希审计。没有格式化或擦除；RC17、清洗带及 TD8 内未分配 8A0042 未作为数据介质装载写入。

## 隔离状态与证据边界

本轮使用全新独立测试目录 `/root/tape-library-serial-20261021`，旧 l8/l9 状态目录原样保留。离线验收夹具只初始化既有池 UUID/名称和两盘磁带 assignment；使用起始索引 1000000，确保测试写入版本与 PR 执行轮次高于此前验收。夹具初始不含历史文件目录，运行时实际装带恢复并通过生产路径对账重建目录。这是明确的测试初始化，不是产品提供的状态迁移工具，也不是旧配置升级验收。

新序列号模式后续接续使用三机 `/root/tape-library-serial-20261021/state`，不要将旧别名状态或离线 seed 覆盖它。该状态含本轮真实提交及磁带对账，控制元数据只包含本轮初始化范围，不声称迁移了旧状态的全部策略和任务历史。

候选构建为 gnu.2.34 / features=fuse；ltfsd SHA256 `84e7cbf2f9868ea83394d2469d2d1db5e875fc2aaf2b2e02a644e0295bb3bfd4`，其余二进制和三机核对记录见[物理证据](../.scratch/ltfs-ha/probes/results/library-serial-physical-20261021/binary-sha256.json)。源码哈希、控制脚本、离线 seed、原始命令结果、FUSE fsync、换 owner 后重新挂载读取均保留。没有正式替换 EE 或原 Rust 控制服务；本轮不代表物理断电、FC 物理断链或全部写入故障窗口验收。

## 清场

验收、清场及独立复核均完成：acceptance.complete、cleanup.complete、post-close.complete 已存在。三机新候选及历史临时服务 MainPID=0，无测试 VIP 和 FUSE 挂载；原 17421 控制集群仍可查询，executor=null。EE208 available、无活动任务；A 库 drive/changer 为原唯一 EE PR（type3，0x400000000a8307d0 / 0x500000000a8307d0），B 库三个设备均无 PR。RC18 槽7、RC17 槽8、AA13 槽1、AA00 槽2，8A0042 保留在 TD8，HH8 为空。两个新文件的条码、长度、SHA 在三份目录中一致。三条 SSH ControlMaster 已关闭，凭据未落盘。

守护进程 CDB 审计没有 FORMAT/ERASE/EXCHANGE；未分配 TD8 的 8A0042 路径只有 INQUIRY/PR，没有 LOAD/WRITE/FILEMARK。208 候选无 SG_IO。归槽等原生维护命令另有控制器原始记录，不能将守护进程计数当作全部现场命令。

首次独立复核错误使用 127.0.0.1 查询只绑定物理 IP 的正式 HTTP 服务，收到连接拒绝；只修正复核脚本到各节点的实际绑定地址后，全部复核通过，原失败日志保留。未放宽断言、修改正式服务或重跑有状态写入。
