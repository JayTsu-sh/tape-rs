# 符号链接跨带回收修复与隔离验收

2026-09-25，ltfs-recovery-ibm-interop。软件和两盘新建Holo虚拟带验收通过；修复尚未正式部署。

## 复现与修复

按diagnosing-bugs流程建立三节点模拟回收测试，缩小到一个dangling→missing链接，稳定返回abandoned，detail为“符号链接目标不可读”。命令cargo test --test ltfsd_cluster reclaim_preserves_symlinks -- --nocapture。原copy_one把链接送入普通文件read_file_to_writer流程，读取了目标。目录/循环/悬空链接不应要求目标可读，迁移也不能将链接变成目标内容的普通副本。

executor先按索引节点类型分流：链接经FileService::relocate_symlink预留路径并加入元数据队列，没有spool和数据读取。与目录迁移一致处理已被替代、路径占用、换带与停止服务；元数据准入拒绝目录冲突，不覆盖当前写带已发布的新版本。volume::add_symlink_with_metadata复制目标及FileMetadata，保留全部时间/readonly/xattrs，分配目标卷UID，不复制extent，再设置新的catalog version。普通文件仍走原内容哈希校验；源带格式化前的两道检查保持。

预防同类问题的关键是让回收测试包含各种节点类型和不可解析目标，而不只验证普通文件内容。没有新增依赖、持久化格式或公共API。

## 软件验证

最小复现修复后通过，再恢复6类矩阵：普通、悬空、循环、绝对路径、链式和目录链接，含二进制属性及readonly。回收done、目录条码迁移、接管后TapeFs readlink/类型及目标内容通过。通过独立模拟库重新mount目标带的实际介质副本，确认全部NodeMeta字段（UID按目标卷分配）、xattrs、无extent及UID唯一性。

扩展测试首次尝试直接解析IP最后Full，遇到旧Full视图缺节点；改为mount恢复Full+Incremental后检查，没有为了通过测试改提交策略。

cargo test --all-features：206通过、7忽略。cargo check --all-targets与cargo clippy --all-targets --all-features完成，保留既有告警。全仓fmt check仍历史差异，本轮方法/新增测试已局部格式化；git diff --check通过。无.codegraph目录。没有修改FUSE源码，未重复独立内核套件；真实FUSE路径由下述Holo验收覆盖。

## Holo隔离验证

新建SR2501L8、SR2502L8，各512MiB，TAPERS槽8/9，避免格式化前几轮证据带。新池sr UUID 51cfd06c-bf79-4bbf-ac85-886608a0ab4b，仅归属独立test-data。测试目录/home/rocky/tape-rs-symlink-reclaim-20260925，7500/7501，ltfsd SHA256 b8496c5bf8de2abb26ffecef81159e5db948437169fa716d5b6e03b31c253025。FUSE使用现有正式创建版cc8b3c…。

原三节点正常停止，TS1001L08归槽6后才使用测试集群。先仅归属SR2501L8并由既有空白带流程格式化，创建/sr/seed及/sr/links测试目录。FUSE创建6类链接并通过relative写入新target、fsync，HTTP给6个链接添加binary=00ff属性。正常停机卸载后源带备份Full23。

随后归属SR2502L8并显式执行ltfsctl tape reclaim SR2501L8。结果done：“搬走11个文件，已重新格式化”。源带DP/IP Full2，目标带DP/IP Full4。全新FUSE缓存验证目标条码SR2502L8，readlink/lstat/scandir、链式和中文读取、目录链接、悬空ENOENT、循环ELOOP及只读mmap全部通过；HTTP比对链接类型、零长度、目标、mtime及原xattrs保持。比较源备份Full23与目标Full4的XML，6个链接目标、length、readonly、全部时间字段一致且无extent。

正常停止node1，node2在round62接管目标带gen4 Complete，再用全新缓存复验全部通过。源/目标测试带已分别归槽8/9，测试三节点和FUSE停止，保留测试data及介质证据。Holo备份/home/rocky/holo-symlink-reclaim-20260925/{created.json,source-before,source-after,destination-after}。

过程中的一次SSH启动包装将sed与后台命令放在同一异步列表，使本地等待第一个SSH退出，后两节点未启动；终止本地包装后分开执行，三节点正常启动。这是测试编排问题，没有改集群算法。首次归属后在设备周期前PUT返回no_tape，确认进入服务后首次写入成功；未盲目重放未知提交。

## 原环境恢复

原正式程序仍tape-rs-symlink-create-prod-20260925、45ca052…，并未部署回收修复。原正常停机触发Full checkpoint，TS1001L08从95到96；恢复node2/term40/round350、96 Complete、19MiB可用。不能声称原带代数和catalog哈希完全未变。

原9文件长度/SHA全部一致。与正式升级前备份相比，原30条非/rc记录的除generation外字段一致；三节点catalog均42条，SHA256 89691e412db63fe43f18bd3b2ad33ff2f9f12337748f8ca92f5885282c6211c8。旧7d5660…总哈希断言因checkpoint失败，查日志确认95→96并改为字段核对和三副本一致性验证，未修改目录数据。

EE3节点available、无活动任务，f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e；9个Holo publications ready。RT2502L8、LE副本未访问。新SR两带仅归属隔离集群，不属于正式池；保留作后续测试。

证据probes/results/symlink-reclaim-*，固定挂载和环境变量门控探针probes/symlink-reclaim.py。无VM断电、无物理磁带；确实格式化了新建测试源/目标，并由回收再次格式化源带。本轮没有LE回读，不能宣称LE回收互操作实测通过。

下一步可部署已验收回收修复。正式旧版仍缺这项修复；大目录性能、其他LE能力和回收中断场景不是本轮验收范围。工作树全部改动未提交。
