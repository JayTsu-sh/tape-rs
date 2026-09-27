# 符号链接回收修复：三节点正式部署与现场验收

2026-09-27，分支 `ltfs-recovery-ibm-interop`，生产源码来自 `cda359d`。本轮完成已有修复的部署，没有修改生产 Rust 代码。用户明确要求无人值守、按推荐方案逐步推进，并在现场验证每项任务；此约定按已定义功能和实验室范围执行。

## 部署产物与备份

统一目录：`/home/rocky/tape-rs-symlink-reclaim-prod-20260927`。

| 产物 | 主机 | SHA256 |
| --- | --- | --- |
| ltfsd | 10.131.9.71 / .72 / .74 | `32bd5541c0123559b590e9dd94c935753a30dacdb660a3e8efcc2ee3e89f8092` |
| tape-fuse | 10.131.9.73 | `ef237d2ea43bfaf182f24d34a3d4a2a3bf9e509e8ba21ead321112e7c87c85d9` |
| ltfsctl | 10.131.9.73，新目录内 | `90f7d3caee4c16261779b5f5d3608e927dd899657144e3d1c02d4db4ab5ce933` |

重新构建命令：`CFLAGS_x86_64_unknown_linux_gnu=-mcpu=baseline cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 --features fuse --bins`。三种产物的最高 GLIBC 依赖均为 2.34，远端 `--help` 均成功；本轮构建哈希与此前隔离验收产物不同，以上为实际本轮产物。

升级前核对三节点实际 `/proc/PID/exe`、原数据目录及设备序列号，均为旧创建版 `45ca052…`，node2/term40/round350 服务，目录42条/applied353一致。现场库存确认 TS1001L08 在驱动器1，TS1000L08 槽5，RT2502L8/SR2501L8/SR2502L8 分别槽7/8/9；EE无活动任务。先正常同时 SIGTERM 三节点，逐节点确认退出，再创建权限0600的 `pre-upgrade.tgz`，包含关闭后的 `ltfsd-data`、旧 ltfsd 和原启动脚本，归档可列举且已记录哈希。

启动入口仍为 `/home/rocky/tape-rs-ltfs25-20260924/start.sh N`，仅替换程序目录；端口7400/7401、原数据目录、批次/快照设置和三个设备序列号保持。新目录另存 `start.before.sh`。旧程序与客户端目录保留。任何回退只另行评估程序和入口，不将旧 Raft/catalog 数据覆盖已推进的在线状态。

## 软件与现场结果

- `cargo test --all-features`：206通过、0失败、7忽略；`cargo check --all-targets` 通过；全特性 Clippy 完成并保留既有告警。全仓 fmt check 仍失败于历史差异，未批量格式化。本轮没有重复执行 ignored 内核套件，真实 FUSE 验证见下。
- 新三节点启动后 node3/term41/round355，目录42条/applied358一致；实际程序哈希均为本轮构建。新 FUSE 挂载后，原9文件长度、SHA256、只读共享 mmap 和条码均通过。
- 使用固定专用目录 `/rc/reclaim-upgrade-20260927`：创建6类链接，验证相对/链式/中文目标、目录链接、悬空 ENOENT、循环 ELOOP、lstat/readlink/scandir、mmap、重复创建 EEXIST，以及通过链接覆盖目标并 fsync、临时链接 rename/unlink。
- 卸载 FUSE，正常停止当时的 Leader node3；node2/term42/round386 接管，从 generation110、完整尾部恢复服务。随后重启 node3，三副本目录一致。新 FUSE 使用已清空的缓存重新挂载，全部临时链接及内容再次验证通过。
- 清理前复核精确目录条目及链接目标，删除本轮创建的内容。再次读回原9文件并通过长度/SHA256/mmap/条码校验；FUSE已卸载、缓存为空、无残留 tape-fuse 进程。

最终三节点实际程序一致，node2/term42/round386，目录53条/applied407完全一致。最终磁带与目录为 generation119；最后原文件读取触发的实际 mount 日志确认 `dp_tail=Complete, ip_tail=Complete, writable=true`。`/cluster.local` 仍显示接管时110与12MiB，是历史描述，不当作最终磁带代数或实时容量。

目录从42到53是测试删除后的墓碑及父目录变化。对升级前除 `/rc` 本身外的41条记录，排除随索引更新的 generation 后逐字段一致；原9文件实际内容也一致。排序目录JSON（`sort_keys=True`）SHA256为 `1a76875af7cac4e057e5fe80ab66d654348800f22b9c07ca5681f83a32762634`。TS1000L08仍generation2，池归属未变，隔离三盘仍在原槽位。此次小量数据与索引写入消耗磁带空间，删除不等于物理回收。

最终EE三节点available、无活动任务，`/gpfs/tapers-del/f3` SHA256仍为 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`；Holo的9个publication均ready。

## 证据与边界

探针：[reclaim-upgrade-20260927.py](probes/reclaim-upgrade-20260927.py)，固定挂载点/测试目录，要求 `TAPE_RS_RECLAIM_UPGRADE=TS1001L08-production-test`。证据前缀 `probes/results/reclaim-upgrade-20260927-*`，包含构建清单、软件检查、备份摘要、三节点实际程序/目录、创建/接管/清理/原文件、库存和EE结果。

本轮准备阶段的临时采集脚本曾因嵌套Python中的NUL转义失败，随后又因将各节点不同的 `answered_by` 当成池差异而失败；两次均在停服前，修正为比较池正文后核验通过。`cargo zigbuild --version` 不受该工具支持，实际构建命令成功；不把这些准备失败算作验收通过。

确实执行了Holo磁带写入、正常停服的UNLOAD/预留释放及接管预留操作；没有格式化介质、执行真实物理带库操作、重启Holo/EE或VM断电。回收算法的跨带搬迁/源带格式化已有[上一轮隔离验收](holo-symlink-reclaim-20260925.md)，本轮部署回归没有在原数据带重跑回收，也没有新增LE回收往返证明。

下一步优先在隔离SR测试池补回收中断/接管的保真验收；原正式数据带不做故障注入。硬链接、大目录性能及物理驱动固件验证仍未完成。
