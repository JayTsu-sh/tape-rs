# Incremental barrier / VCI 故障验收

2026-09-25，ltfs-recovery-ibm-interop。本轮补齐增量提交的软件错误矩阵，并在隔离 Holo 环境命中两个进程中断窗口。没有发现生产代码缺陷。

## 软件错误矩阵

`tests/sim_incremental.rs::incremental_barrier_and_partial_vci_failures_recover_metadata_and_allow_next_commit`：3个故障点 × VCR正常/不变 × 重挂前是否模拟断电，共12组。故障点是Incremental最终WRITE FILEMARKS(0)、IP VCI写入、DP VCI写入；注入一次MEDIUM ERROR（03/0C/00）。

基线Full2、已确认Incremental3；第4代改变二进制/空属性并删除旧属性。所有组均验证CommitFailed阶段准确、旧发布视图不变、卷冻结、后续修改和提交被拒绝。新实例恢复gen4、深度2、DP Complete；旧DP提示不被采信，IP提示是否可用符合VCR及首次VCI写入完成状态。属性、原数据块位置及mtime正确；继续提交gen5、Full收尾并重挂后目录和空属性保留。

## Holo：结束FM之后、barrier之前

原三节点正常停机，TS1001L08归槽6。测试目录`/home/rocky/tape-rs-barrier-vci-20260925`、7500/7501，仅归属RT2502L8，从Full79开始；使用正式ltfsd SHA f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。

node2/round242设置`user.barrier`为00 ff + barrier。strace仅附着测试执行线程，ioctl进入延迟200ms。探针确认WRITE(6)的2463字节XML根是ltfsincrementalindex，结束FM成功，并确认没有最终barrier或VCI完成，才SIGKILL。最终trace停在屏障前PR检查（5e/00）的延迟调用；这不是“barrier已返回错误”的现场证据，而是“结束FM完成、barrier尚未执行”的中断证据。

Client库返回Indeterminate。node1/round247接管，从磁带恢复DP gen80 Complete，目录原为79，自动对账。属性正确、版本仍242.1；mtime、完整list及13个文件SHA256与基线一致。

第一次Client启动误传带http://的端点，返回NoLeader/DNS格式错误，未连接服务、未写带。改为Client要求的host:port后执行上述实际测试；原始失败日志单独保留，没有计为验收通过。

## Holo：IP VCI成功、DP VCI之前

恢复node2并确认跟上目录后，node1/round247设置`user.vci`为00 ff + vci。探针观测Incremental81的索引和结束FM、最终barrier成功，随后IP WRITE ATTRIBUTE（8d、partition0、属性080c）成功。IP VCI指向Full79/block5并持有新VCR；仅此一次VCI写入完成后SIGKILL。最终trace中partition1的WRITE ATTRIBUTE（计划gen81/block276）仍处于进入前延迟、unfinished。

Client库返回Indeterminate。node2/round252恢复DP gen81 Complete，将目录从80补齐；恢复后的文件版本仍247.1。此前barrier属性、本次vci属性、原二进制set都正确，mtime、完整list及13文件SHA256不变。重新启动node1后，三节点SQLite目标记录完全相同。

这证明该现场的旧DP VCI没有掩盖新提交的增量；软件矩阵另行直接断言hint_used行为，现场日志没有输出hint_used，不能把软件的提示分支断言当作现场日志。

## 收尾及检查

删除两个临时属性均201，分别提交gen82/83，原用户属性恢复；正常停止产生DP/IP Full84。RT2502L8归槽7，before/after保存在Holo `/home/rocky/holo-barrier-vci-20260925`。隔离ltfsd及strace退出。

原集群恢复node1/term33/round271，TS1001L08仍gen72 Complete、TS1000L08仍gen2。三节点实际exe哈希不变、catalog31条及哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0一致。原9文件长度/SHA256及完整list一致，EE三节点available无任务，GPFS f3哈希cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e不变，9个publications ready。

- cargo test --all-features：202通过、6忽略（新增矩阵计为1项测试，内部12组）。
- cargo check --all-targets、cargo clippy --all-targets --all-features完成；既有告警，本次修改文件无新增告警。
- 两个修改Rust测试/探针文件的rustfmt检查、Python语法、git diff --check通过。
- cargo fmt --all -- --check仍失败于既有格式差异，未格式化无关文件；无CodeGraph目录。
- Client探针兼容构建及Rocky实际运行通过。本轮只修改测试、探针及记录，未部署新生产二进制，全部未提交。

材料：`probes/barrier-vci-cut.py`、`examples/hw_xattr_fault.rs`新增barrier-cut/vci-cut模式；`probes/results/{barrier-cut*,vci-cut*,barrier-vci-*}`包含事件、完整SCSI跟踪、Client结果、恢复stat、三节点目录、节点日志、最终XML及验证日志。

本轮没有VM断电、Holo重启、物理TS4300或LE回写。现场中断、软件命令错误、上一轮VM掉电分别成立，不能合称所有物理设备故障已覆盖。下一项是用恢复后Full84的隔离副本补做LE读写、降级索引后Client/tape-fuse读回；原LE副本TS1000L8仍保持此前gen58，尚未更新。
