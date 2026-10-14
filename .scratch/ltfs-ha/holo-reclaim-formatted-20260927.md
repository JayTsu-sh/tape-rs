# 源带 mkltfs 完成、回收完成记录之前中断

2026-09-27，`ltfs-recovery-ibm-interop`。用户再次确认无人值守继续，本轮选择交接记录中的下一项并完成模拟与 Holo 验收。仅扩展测试、探针和证据；无生产源码、持久化格式、依赖或二进制变更。

## 成功标准与实现

目标内容已迁移，源带已成为完整空 LTFS 卷，但池标记与 `TapeReclaimed` 尚未提交。接管应从仍为 `reclaiming` 的复制状态继续，得到可写空源带和完整目标副本；恢复旧执行者不能破坏新状态。

在既有测试 transport 中增加 `ReclaimCut::AfterMkltfs`：记录源 FORMAT 的 GOOD 返回，在此后第一次 READ(6)（finish_reclaim 重新 mount 源带）前暂停。mkltfs 自身不做 READ(6)，因此该点已完成两分区空索引、barrier、VCI 和倒带。仍保留原 `BeforeFormat` 用例，不向生产代码加入故障开关。

新增 `reclaim_takeover_after_mkltfs_before_completion_preserves_symlinks`：暂停时独立挂载源介质快照，确认新 UUID、Full1、可写、无文件/池属性，而复制状态仍为 reclaiming、目录链接已全部指向目标。隔离旧节点后新节点回收 done，再独立挂载源带确认可写 Full2、零文件和正确池 UUID。恢复旧操作必须报告 ownership_lost，前后源带分区对象不变。复用六链接的全部 NodeMeta/xattrs/无extent/UID唯一性检查、目标数据和后续接管读回。

最终软件验证：`cargo test --all-features` **208通过、7忽略**；`cargo check --all-targets`、`cargo clippy --all-targets --all-features` 完成，既有告警保留。新增区域 rustfmt、探针 Python 编译及证据复核通过；全仓 fmt check 仍有历史差异，保存摘要与完整输出 SHA。`git diff --check` 通过，工作树无 CodeGraph，未新建索引。

## Holo 实际中断与接管

正式服务正常停止、TS1001L08归槽6后启动隔离集群：`.71/.72/.74:/home/rocky/tape-rs-reclaim-formatted-20260927`，7500/7501，复制上一轮关闭的 reclaim-cut test-data。各节点 `isolated-before.tgz` 保存原隔离数据；未覆盖正式数据。池 sr UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b`，只含 SR2501L8/SR2502L8。使用正式已验收 ltfsd SHA256 `32bd5541c0123559b590e9dd94c935753a30dacdb660a3e8efcc2ee3e89f8092`。

本轮 SR2501L8 Full4 → SR2502L8 空 Full2。node1/round84 执行回收，strace只给其 `ltfsd-exec` 线程 ioctl 入口增加500ms延迟。02:34:53 UTC，源 mkltfs 完成（UUID `a7f691bb-80bf-44ac-aca6-172188e82a6b`）后 SIGKILL。trace证明 FORMAT GOOD，随后没有完成重新挂载 READ(6)；截断状态仍reclaiming、last_reclaim为空，13条目录均指向目标。源节点关闭数据另存 `.71:at-cut.tgz`（0600），截断事件及完整trace已保存。

node2/round95 接管：02:34:56 UTC实际挂载同一 UUID、DP/IP Complete Full1、零文件；从复制的回收状态再次格式化，生成新 UUID `f5483a52-3587-4069-bfe1-8ed56a8f0868`，写入池标记，02:34:59 UTC报告 done，搬移0文件。旧节点重启后三份目录一致。

新缓存 FUSE验证六类链接的lstat/readlink/scandir、目标及链式/中文读取、目录链接、悬空ENOENT、循环ELOOP、mmap、SR2502L8条码和seed哈希全部通过。13条目录的类型/目标/mtime/xattrs/length/hash/墓碑字段保持，允许条码、generation和tapers.version迁移。

正常停止隔离集群后，比较已关闭介质的Full XML：六链接除UID与tapers.version外全部字段相同、无extent。SR2501L8最终两分区Full2、零文件、正确池UUID；SR2502L8最终两分区Full4、六链接/10个非删除文件。两带分别归槽8/9。备份及逐文件SHA校验在 `holo:/home/rocky/holo-reclaim-formatted-20260927`。本轮已覆盖mkltfs完成后的进程死亡；没有覆盖原始FORMAT刚返回而标签尚未写完整的更早窗口，也不是VM断电测试。

## 正式环境恢复与接续

正式三节点运行原部署路径，实际exe哈希一致。最终node1/term44/round414，三份目录53条/applied417；TS1001L08仍Full120 Complete、可写、可用7MiB，TS1000L08仍2。原53条catalog及池摘要**逐字段完全不变**，目录SHA仍 `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`。原9文件length/SHA/mmap/条码经新缓存FUSE复验通过，随后卸载、缓存清空。

EE三节点available、无活动任务；f3 SHA仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`；9个Holo publications ready。原TS1001L08在第一TAPERS驱动器；RT/SR1/SR2位于槽7/8/9。隔离节点、strace和FUSE均停止；无物理设备、EE/Holo/VM重启或LE往返。

最新关闭的SR数据是 `/home/rocky/tape-rs-reclaim-formatted-20260927/test-data`（node2/round95）；不要重启更早目录的数据。下一项推荐补原始FORMAT成功、LTFS标签/索引构造尚未完成时的回收恢复。硬链接、大目录性能、既有namespace/takeover间歇HTTP500及物理固件验证仍开放。

证据前缀 `probes/results/reclaim-formatted-20260927-`；固定门控故障探针 `probes/reclaim-formatted-20260927.py`，FUSE只读探针 `probes/reclaim-formatted-read-20260927.py`，可重复的本地证据核对 `probes/reclaim-formatted-verify-20260927.py`。故障探针在现场运行后补入了相同的“无完成READ(6)”后置断言，已用保存的真实trace复核，无需再次故障注入。
