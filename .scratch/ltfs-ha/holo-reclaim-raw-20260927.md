# FORMAT 后标签未完成的回收恢复（2026-09-27）

基于 f1d9993。确定性复现命令 `cargo test --test ltfsd_cluster reclaim_takeover_after_format_before_labels -- --nocapture`：旧实现接管后 BLANK CHECK 08/00/05，回收 abandoned，错误退回 appendable，无法完成源带初始化。失败输出保存在 probes/results/reclaim-raw-20260927-red.log。

修复：迁移完成后仍执行源索引逐路径与目录零引用两个核验；目录无法读取时拒绝格式化。将核验结果通过既有 TapeState 日志持久化为 reformatting，必须观察应用完成才发 FORMAT。接管在此阶段跳过可能已擦除的源索引，重新核验目录零引用及池归属，完成 mkltfs/池标记后回到 appendable。该阶段错误终止执行轮次，保留恢复阶段，不退回普通可写。普通 unreadable reclaiming 没有自动擦除授权。保留旧 TapeReclaimed 日志重放兼容；新版须整组升级，不支持初始化阶段中混跑或回退旧二进制。未修改 SCSI 命令、ABI、超时或依赖。

新增 FORMAT 后无标签、只有 VOL1 的部分标签两个接管用例，加原 FORMAT 前/mkltfs 后两个窗口，均验证旧操作恢复后被 fencing 拒绝、源带不再被改变、六种链接保真。状态测试覆盖非法阶段跳转、快照恢复、重复回收拒绝、完成后迟到提案拒绝。软件：211通过、7忽略；全仓 fmt/check、严格 Clippy -D warnings 通过。兼容构建通过，仍有外部 Zig 废弃优化参数提示，未添加任何告警屏蔽。

现场候选 /home/rocky/tape-rs-reclaim-raw-20260927，ltfsd SHA256 f618de55a803ac52d5c287e1d18cd1b08b1563a20bf0f3f4d0e0ce8e7c70b114。关闭数据复制自 cleanup/test-data；正式数据未用于注入。SR2502L8 Full14 → SR2501L8 空Full2，17条目录记录。node1 round127执行回收；03:38:34 UTC，strace证明 FORMAT GOOD 且其后没有完成 WRITE(6)，在持久 reformatting 阶段 SIGKILL。node3 round141自动接管、报告 done。旧节点重启追平，三份目录一致；所有17条记录除条码/代数/版本外内容及元数据保持。冷缓存 FUSE六链接/链式读取/悬空/循环/目录链接/mmap/seed哈希均通过。

关闭介质 XML 核验：SR2501L8两分区Full5，14个文件元素、六链接，链接字段除fileuid/tapers.version外一致、无数据extent；SR2502L8两分区Full2、零文件、池UUID正确。闭合前后备份 /home/rocky/holo-reclaim-raw-20260927，经SHA逐文件校验。最新关闭数据 /home/rocky/tape-rs-reclaim-raw-20260927/test-data，node3 round141。两带归槽8/9。node1 at-cut.tgz仅为故障证据，不能替代最新状态。

正式集群已恢复node1 term46/round424；53行目录逐字段不变，TS1001L08 Full120、TS1000L08 gen2，原9文件长度/哈希/mmap/条码通过。正式二进制仍32bd5541…，本修复只部署隔离候选。EE三节点available、无活动任务、f3哈希未变；Holo九个publication ready。隔离进程/strace均停止，FUSE卸载、缓存清空。未触及物理设备，物理固件验收暂缓。

脚本 reclaim-raw-20260927.py、reclaim-raw-read-20260927.py、reclaim-raw-verify-20260927.py 和 results/reclaim-raw-20260927-* 保存可复核证据。下一项：namespace/takeover 间歇HTTP500。
