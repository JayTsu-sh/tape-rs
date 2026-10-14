# 回收在源带格式化前中断：接管与链接保真

2026-09-27，`ltfs-recovery-ibm-interop`。本轮完成模拟测试和 Holo 现场验收，仅新增测试与探针，没有改变生产算法或重新发布二进制。使用上一任务部署的 ltfsd `32bd5541c0123559b590e9dd94c935753a30dacdb660a3e8efcc2ee3e89f8092`。

## 成功标准与软件结果

在目标副本已经提交、源 FORMAT 尚未送达设备时暂停旧执行者；新节点接管后仍能完成回收，旧命令不得破坏新状态。六类链接的目标、类型、全部时间、readonly、xattrs 和目标内容必须保真；目标卷分配 UID、不复制 extent。

`tests/ltfsd_cluster.rs` 新增 `reclaim_takeover_before_source_format_preserves_symlinks_and_fences_old_format`，与已有完整回收测试共享数据矩阵。暂停点仅在测试 transport 内，预先格式化目标防止误截目标创建。检查源副本仍有链接、目录已经指向目标；隔离旧节点，等待新执行者 done，再释放旧 FORMAT，断言错误属于 `ownership_lost` 且两分区对象完全不变。旧节点重新加入后核对复制的 applied index/磁带状态；独立重挂目标介质及后续接管验证类型和数据。

测试编写中纠正了两项错误假设：`last_reclaim` 是节点本地展示状态，不会复制给旧节点；PREEMPT 可以返回 06/2A/03..05 的失权 UNIT ATTENTION，生产代码特意不重试它，不能只接受 status 0x18。最终断言使用既有 `ownership_lost` 分类，没有为通过测试改变生产逻辑。

最终 `cargo test --all-features`：207 通过、7 忽略；`cargo check --all-targets`、`cargo clippy --all-targets --all-features` 完成，保留既有告警。新增测试区域局部 rustfmt，完整 fmt check 仍有历史差异。`git diff --check` 通过。目标工作树无 `.codegraph/`，未建索引。

## Holo 故障场景

先正常停止正式三节点，将 TS1001L08 归槽6。新隔离目录 `/home/rocky/tape-rs-reclaim-cut-20260927`，端口7500/7501；各节点复制上一轮**关闭的隔离** test-data，保留 `isolated-before.tgz`，未覆盖正式 `/home/rocky/ltfsd-data`。池仍为 sr，UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b`，只有 SR2501L8/SR2502L8。

反向回收 SR2502L8 → SR2501L8。源 Full4 含六个链接及普通文件，目标是上一轮回收后的空 Full2。Holo 两带备份及逐文件 SHA 校验位于 `/home/rocky/holo-reclaim-cut-20260927/{sr2501l8,sr2502l8}-{before,after}`。

node2/round67 的 `ltfsd-exec` 线程通过 strace 将 ioctl 入口延迟500ms。门控探针看到源带“内容已全部有着落…重新格式化”日志后，于01:30:58 UTC终止该隔离进程。截断前及进程退出后的 trace 均无 FORMAT MEDIUM CDB；关闭目录全部13条已指向 SR2501L8。截断点是在格式化流程开始、首个 FORMAT 前，**并非 FORMAT 执行中的故障**。

node3/round78 自动接管，重新验证13个路径已有别处副本，01:31:05 UTC报告回收 done（无需重新搬数据）。旧节点正常重启并追上后三份目录一致。全新缓存 FUSE 验证六种链接的 lstat/readlink/scandir、链式/中文读、目录链接、悬空 ENOENT、循环 ELOOP、mmap、目标条码 SR2501L8 和 seed 哈希均通过。13条目录记录除条码、代数、迁移版本外，类型/目标/mtime/xattrs/length/hash/删除标记不变。

正常停机后直接比较已关闭介质的 Full XML：六个链接除重新分配的 UID 与 tapers.version 外全部字段相同、没有 extent；源 SR2502L8 两分区 Full2、零文件；目标 SR2501L8 两分区 Full4、六链接/10个非删除文件。两带分别归槽9/8。测试节点、strace 和 FUSE 已停止，缓存清空。

模拟测试覆盖“延迟旧 FORMAT 恢复后被拒绝”；Holo 覆盖“旧进程死亡后新节点继续回收”。没有将后者描述成真实设备上恢复旧 FORMAT 的证明。

## 正式环境恢复

恢复原启动入口与正式程序，当前 node1/term43/round409，三节点 applied412、53条目录一致。TS1001L08 正常停机 Full checkpoint 从119到120；mount 日志确认 DP/IP Complete、可写、可用7MiB。TS1000L08仍2。53条原记录除 generation 外全部字段不变；当前排序目录 SHA256 `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`。原9文件长度/SHA256/mmap/条码通过冷缓存 FUSE 复验并已卸载。

EE三节点 available，无活动任务，`/gpfs/tapers-del/f3` SHA仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`；9个Holo publication均ready。TS1001L08恢复第一驱动器，RT2502L8/SR2501L8/SR2502L8分别槽7/8/9。

收尾编排曾因墓碑metadata为null、系统工具名为fusermount而非fusermount3、跨主机sg编号不同和publication API路径写错而失败，修正脚本后完成检查。曾在非持有者做只读inventory遇失权UA；另一次把node2的sg8用于node1，收到05/24/00。最终用node1 VPD序列号确认TAPERS changer为sg9后检查库存；这些失败没有执行机械移动或写带。后续跨主机必须重新核对序列号，不能沿用sg编号。

## 证据与剩余范围

探针：`probes/reclaim-cut-20260927.py`、`probes/reclaim-cut-read-20260927.py`，固定目录/池/环境变量门控。证据：`probes/results/reclaim-cut-20260927-*`，含软件日志、中断trace、三副本目录、介质XML比较、正式恢复和EE状态。

确实在 Holo 上执行了专用测试带搬迁、源带格式化与SIGKILL。未访问物理TS4300、未重启EE/Holo/VM、未进行LE往返。物理固件、源FORMAT成功后到TapeReclaimed之间的窗口、硬链接、大目录性能以及既有间歇namespace/takeover HTTP500仍未由本轮验证覆盖。下一步优先补“源格式化成功、完成记录未复制”窗口，继续使用SR隔离池并重新核对当前状态。
