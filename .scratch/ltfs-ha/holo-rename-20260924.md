# 当前写入卷 rename：Holo / LE 往返与响应丢失验收

2026-09-24，分支 `ltfs-recovery-ibm-interop`。完成隔离验证，并修复 LE 改回旧名被私有墓碑遮蔽的问题。原集群已恢复，尚未升级 rename 版本。

## 范围与隔离

- Holo 虚拟设备；没有访问物理 TS4300，没有 VM 断电、格式化或介质创建/删除。
- 原三节点同时正常停止，原 TS1001L08 从 drive0 归槽6。测试节点使用独立 `/home/rocky/tape-rs-rename-20260924/test-data` 和7500/7501端口，仅归属 RT2502L8。
- RT2502L8（TAPERS槽7）与 TS1000L8（LE槽1032）是保留副本，卷 UUID 与原 TS1000L08 相同，不能归属原集群。复制只在卸载、LE unassigned 后进行。
- Holo `/home/rocky/holo-rename-20260924/{rt-before,le-before,native-full,le-result}` 保留前后证据。起点 gen24，原生 Full gen45，LE 结果 gen46。

## 结果

| 验证 | 结果 |
|---|---|
| Client 文件改名、目录树改名 | 通过 |
| FUSE 源 inode 保持、打开 writer 改名后继续写 | 通过 |
| NOREPLACE 冲突返回 EEXIST | 通过 |
| 替换目标的旧打开 writer 后续写入不覆盖新目标 | 通过，另做独立重复验证 |
| committed 响应丢失、Leader SIGKILL、接管 | 通过，Client Indeterminate，不重放，新 Leader 读取完整新树 |
| LE 读取原生 Full | 通过，UID、extent、mtime、二进制 xattr 保留 |
| LE 将文件与目录改回旧路径，回写2.4.0 | 通过，gen46 |
| tape-fuse 读回 LE 结果 | 初次失败；修复后通过，含空目录、内容、mtime、xattr、只读 MAP_SHARED mmap |
| 恢复原集群与 EE | 通过，原9文件长度/SHA256全部相同，EE3节点可用、无活动任务 |

故障代理只截获一次 POST /rename：201 committed、gen44、task20。旧 node2 退出后 node3/round104 接管并读回，新旧源目录未出现部分切换。此测试故障点在提交成功之后，不声称本轮测试了真实断电或截断索引；上一轮纯软件测试覆盖索引写入故障与模拟断电。

## LE 改回旧名的缺陷与修复

`rename-fuse.py verify-le` 首次对 `/rc/edit.bin` 返回 ENOENT，HTTP 同样显示未提交。LE gen46 原生 XML 中该文件实际存在（UID12、version59.8），同时保留 `.tapers/deleted` 中同路径的 version59.9 墓碑。catalog_of 按版本取最大值，错误地将原生文件视为删除；空目录存在同类问题。

最小回归测试先复现失败，再修改 catalog_of：同一恢复索引中原生文件/目录优先于私有墓碑；没有原生节点的删除仍然有效。保留原生节点自己的 tapers.version，不借用墓碑版本抬高优先级；其他磁带之间仍使用原版本比较规则。没有新依赖、SQLite结构或磁带格式变更。

只将修复后的 ltfsd 同步到隔离三节点，重新挂载原 LE gen46 索引验证成功，无须重写或人工清理墓碑。FUSE 与 Client 均通过读回。测试覆盖两种文件遍历顺序、无版本空目录、真实删除和服务层可见性。

## 验证与证据

- `cargo test`：194通过、1忽略。`cargo check --all-targets`、`cargo clippy --all-targets --all-features`完成，保留既有警告。
- `cargo fmt --all -- --check`仍报告历史基线差异；新增 example/test 已单独格式检查，`git diff --check`通过。无 `.codegraph/`，按仓库规则未创建索引。
- 软件日志：`/tmp/rename-le-{tests,check,clippy,fmt}.log`、`/tmp/rename-le-regression-{red,green}.log`。
- 实测日志：`probes/results/rename-*`，含旧失败、修复后通过、LE XML、代理回执与原环境校验。
- 新探针：`examples/hw_rename_interop.rs`、`probes/rename-fuse.py`、`probes/rename-le.py`，均显式限定目标介质/挂载路径。

探针自身首轮将10字节读取误与11字节字串比较，导致 prepare 提前退出；已修正并独立验证旧目标 writer 隔离、随后完整读回。prepare 尾部的非空 rmdir 断言当轮未执行，不能据此称其通过。另一处原始 XML extent 字符串比较受字段顺序/空白影响，改为语义字段比较，UID12文件和UID26目录三阶段校验均通过。

## 最终环境与边界

- 原集群 node2/term19/round155，TS1001L08 gen45 Complete、45MiB可用。原9个文件逐一长度/SHA256一致。
- 三节点实际运行的原目录版本 SHA256：`2b5bd74cec73809b5b818947a1ddebec79527043286a054d6148497fb4912491`。原数据目录、启动入口未改变。
- 测试服务/代理停止、FUSE卸载。RT2502L8回槽7，TS1000L8回LE槽1032并unassigned，均保留LE2.4.0 gen46。
- EE3节点available，无活动任务，f3 SHA256仍为 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`；9个Holo publications均ready。
- rename仍仅当前写入卷内原子操作；跨带EXDEV、符号链接创建、可写xattr等尚未完成。此前发现的Client缓存旧Leader读失败问题未在本轮修复；故障后验证显式使用新Leader。

下一步可统一部署已验收的 rename/LE 修复版本；应保留离线备份与现有目录对账流程。全部工作树改动尚未提交。
