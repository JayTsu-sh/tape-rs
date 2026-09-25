# 持久化目录 Holo / IBM LE 往返验收（2026-09-24）

结论：本轮目录验收通过。Client/FUSE mkdir、rmdir 写入 LTFS 标准目录节点，经增量提交、正常停机 Full、IBM LE 读取及降级回写后，新版 Client/FUSE 能正确读取目录变化及目录属性。本轮未修改生产逻辑，只新增显式门控探针和验收记录；不代表全部 LE 能力已对齐。

## 环境与隔离

- 分支 `ltfs-recovery-ibm-interop`，全部工作树改动未提交。
- 隔离三节点 .71/.72/.74，7500/7501，目录 `/home/rocky/tape-rs-directories-20260924`；从此前已关闭的 HA 测试数据复制独立 catalog/Raft 数据，仅归属 TAPERS/RT2502L8。
- Client/FUSE 在 tsclient .73；LE 为 ee1 的 IBM 2.4.8.3-10521、LISA4300/TS1000L8、驱动 IBMlisa41D6C。
- 两盘副本 UUID 均为 `26aefb84-fbc8-4c4a-a500-c85264ba2d29`，与原 TS1000L08 相同，**不得归属原集群**。
- 原三节点并发 SIGTERM、确认正常检查点/卸载与 PR 释放后，才启动隔离节点。测试前后盘内容备份在 Holo `/home/rocky/holo-directories-20260924/{rt-before,le-before,native-full,le-result}`。
- 新 ltfsd SHA256 `2b5bd74cec73809b5b818947a1ddebec79527043286a054d6148497fb4912491`；tape-fuse `68306acc5f22de45a9f52761dabc403b9404aeb9263eb1f434af6235799f9a19`。glibc 2.34 release 构建。

## 实测结果

| 阶段 | 结果 |
| --- | --- |
| 起点 | RT2502L8 DP/IP 2.5.0 Full gen13；加载时旧 catalog gen12 对账 |
| Rust Client | 在 `rc/dir-interop-20260924` 创建根目录和空目录；非空 rmdir 返回 409；空目录删除成功；stat/list_dir 正确 |
| 增量证据 | Client 操作后 DP `ltfsincrementalindex` gen17，IP Full gen13；目录写入没有强制每次 Full |
| 实际 FUSE | 创建空目录和 nested/child；删除空目录；非空 rmdir 返回 ENOTEMPTY；正常卸载、全新挂载后空目录保留、删除项不存在 |
| 正常停机 | DP/IP 2.5.0 Full gen23，正常卸载、PR 释放；随后回槽7 |
| LE 读取 | 关闭副本复制到未分配 TS1000L8；assign/mount 成功，原生空目录和嵌套目录正确，已删除目录不存在 |
| LE 回写 | 删除 client-empty，创建 le-empty；设置二进制 xattr `00ff6469726563746f7279`、mtime `1790200000123456789` ns；创建 le-file、索引同步后卸载 |
| 降级证据 | LE 写出 DP/IP 2.4.0 Full gen24，随后回槽1032并 unassign |
| Client 回读 | 关闭副本复制回 RT2502L8，重启隔离集群；stat/list 正确区分目录；client-empty 未复活；le-file 内容正确 |
| FUSE 回读 | le-empty 空目录、二进制 xattr、纳秒 mtime 均准确；le-file 的 MAP_SHARED/PROT_READ mmap 内容准确 |

原 `rc/keep.bin` SHA256 始终为 `7daca2095d0438260fa849183dfc67faa459fdf4936e1bc91eec6b281b27e4c2`，原二进制 `user.roundtrip.binary` 保留。

`directories-index.py` 是原始段文件的 XML 证据扫描，不替代恢复器的链和追加位置校验；服务日志同时证明 Full/Complete 可写挂载及对账成功。本轮未将 LE 用作增量重放 oracle，交换前已生成 Full。

## 恢复现场

- 隔离节点全部停止，FUSE 已卸载，RT2502L8 回槽7，TS1000L8 位于槽1032、NEED_ASSIGN。保留 gen24 结果用于后续工作。
- 原三节点运行原 xattr 修复二进制，实际 `/proc/<pid>/exe` SHA256 均为 `59faf2c49915f3ce22772efd9b5ce8fff028c2c9b697a91be7bbb969e5a8ec69`；未升级原集群目录版本。
- 原集群 node2 / term14 / round124，TS1001L08 gen39、Complete，15 catalog 项，48 MiB 可用。
- 原 `rc/_probe.bin` SHA256 `49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7`；EE `/gpfs/tapers-del/f3` SHA256 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`，均与测试前一致。
- EE 三节点 available、无活动任务；9 个 Holo publications 全部 ready。
- 仅 Holo 虚拟介质写入/移动，无格式化、擦除、介质创建删除、断电或物理带库操作。

## 探针与证据

- `examples/hw_directory_interop.rs`：要求 `TAPE_RS_DIRECTORY_BARCODE=RT2502L8` 和显式 endpoints，检查 keep.bin 所在条码再写；模式 prepare / verify-le。prepare 要求新测试路径不存在，不能盲目重跑。
- `probes/directories-{start-test.sh,fuse.py,le.py,index.py}`；LE probe 也有写操作，只用于上述已检查的专用副本，不进入默认测试。
- `probes/results/dirs-*.log`：Client、FUSE、LE、各阶段 XML、三个节点及恢复现场证据。
- 恢复日志保留了一次向 follower 直接 curl 导致的 503（管道空输入哈希不能当作文件哈希）；随后向实际 leader 下载到文件并确认正确哈希。还保留旧 ltfsee task 命令不支持的结果，随后 eeadm task list 确认无活动任务。

下一步可统一部署目录版本并对账相关旧介质，再接入 rename；旧版会误解目录 catalog，不能混用版本。本轮没有验证真实 LTO 固件、目录跨带回收的现场路径或目录操作期间断电；相应软件测试与此前故障验证不能替代这些现场场景。

软件验证：本轮 cargo test 187 通过、1 忽略；cargo check --all-targets 和 cargo clippy --all-targets --all-features 完成（既有告警）。新增 Rust probe rustfmt、Python 编译、shell 语法及 git diff --check 通过。全局 cargo fmt 仍因既有格式差异失败。日志 `/tmp/dirs-interop-{tests,check,clippy,fmt}.log`。本轮没有改生产源码；上一轮本机 FUSE 测试不冒充本轮重新执行，本轮实际远程 FUSE 验证另见上述证据。
