# 显式无业务控制副本

`ltfsd --fdb-control-only --control-binding FILE` 要求 `fdb` 构建特性、原 `--fdb-cluster-file`、原 `--libraries-config`、原绝对 `--data-dir` 和完整三节点 `--peer`。`--listen` 必须等于本节点原成员地址。此模式与 FDB owner、metadata readonly、S3、VIP、demo write、salvage 及离线配置变更互斥；未提供绑定或原来源缺失时拒绝启动。

该模式继续持久化原 Raft 控制链，所有库和统一入口只报告 `eligible=false, drives=0`，不构造设备 provider，不创建/清理 spool。旧 executor 由现有 Coordinator 提交和应用原 `Abandoned` 后清除；这不改变旧 FDB 任务的结果，也不释放或接管设备 PR。控制副本能够被观察不表示业务就绪，`/readyz` 始终返回 503。

绑定 JSON 采用版本 1，拒绝未知字段；进程读取后固定，运行中改绑定文件不会切换模式或 namespace。字段如下：

| 字段 | 内容 |
| --- | --- |
| `version`, `node`, `members` | `1`、本节点编号、全部三节点原成员地址映射 |
| `cluster_sha256` | 实际 cluster 文件原始字节的 SHA-256 |
| `libraries_sha256`, `group` | `LibrariesConfig::value()` 的 JSON 编码 SHA-256，两者相同 |
| `data_root`, `data_root_dev`, `data_root_inode` | 原数据根的绝对规范路径和身份 |
| `original_chain_manifest`, `original_chain_sha256` | 已封存证据清单的绝对路径及其实际文件字节 SHA-256 |
| `raft`, `manifest` | 本节点原 `raft.db` 和 `libraries.json` 的来源 `dev`、`inode`、原始 `sha256`；Raft 当前字节可合法变化，必须通过原前缀与后继回放核验 |
| `libraries` | 以真实 changer serial 为键的逐库绑定 |

每库绑定必须包含原 `directory` 文件 seal、`quiesce_index`、`frozen_index` 和 `catalog`。目录路径仅从原 data root 与现有 library_dir 规则派生，不能指定任意外来空库。目录须无 WAL/SHM/journal，通过 immutable 只读连接审计；路径、inode、字节 hash 及权威路由在每次审计前后重核。审计不抬高目录 applied 水位，不创建业务目录连接、snapshot bundle 或 SQLite 业务镜像。

本批只支持：

```json
{"kind":"ExistingRoute","route":{"namespace":"原 UUID","revision":1,"frozen_index":2745,"certificate_sha256":"64位小写十六进制","initial_owner_epoch":1}}
```

所有路由字段必须与权威 FDB 完全一致；缺路由、路由变化、FDB 不可用或损坏均拒绝审计。`AbsentAfterReset` 尚无审定回执 decoder，本批明确拒绝。此模式不执行 reset、init、格式化或设备操作。

HTTP 仅允许本地 `GET /cluster`、`GET /admin/libraries` 和 exact 配置库的 `GET /libraries/SERIAL/admin/catalog/current`、`audit`、`native-barrier?challenge=UUID`。`current` 明确返回 `mode=control_only`、绑定路由及审计源。原 native-barrier 的空目录、稳定三节点、无 executor 等谓词保持不变。所有业务读取、上传、任务正文、mutation、export、S3、编码别名和未知路径均拒绝；不代理或转发。

仅完成本模式的软件检查不表示新的 namespace、PR 接管或实带基准验收完成。生产启动须由主会话核对原链、实时 FDB 路由、冻结来源和候选证据后统一调度。


证据清单字段为 `version=1`、`node`、`host`、`copy_root`，以及四个 `{path, sha256}` 制品引用：`copy_manifest`（原 READONLY-COPY-MANIFEST）、`baseline_raft`、`baseline_libraries`、`original_replay`（原正式 codec 的 REPLAY JSON）。`host` 选择 copy manifest 中该节点的来源记录，副本布局为 `copy_root/HOST/原相对文件名`。程序重读清单引用的所有本地归档文件，核实际 SHA/长度与无 WAL/SHM/journal；再交叉核对本节点来源路径、dev/inode、原目录/manifest、实际 protobuf snapshot/HardState/固定成员及完整 committed tail。正式 `ClusterState::restore` 和 `apply_entry_for_replay` 在空 Running map 中回放，`write=false`、`live_gate=false`；重算的完整控制结果必须与原报告相同，不能用清单里的自报成功布尔值替代。

证据由部署者或受信生成器封存并部署到服务不更新的只读制品目录；运行进程不生成、更新或自动重签该清单。此处信任部署者提供的原取证来源。程序不能抵御可以同时替换程序、全部配置及全部证据的 root。

当前 Raft 通过 READ_ONLY SQLite 一致事务读取真实 WAL。原 snapshot 字节、固定成员及原 commit 前每条原始 Entry 均须保持；后继仅允许零驱动且不可执行的正确作用域 Candidate、原 node/round 的 Abandoned，以及精确原 instance/round 的 ExecutorReport 内包 Abandoned 和空 Entry。HardState 与 local_instance 必须保持合法单调关系。后继由正式 codec/apply 重放，库集合、原 quiesce/Frozen 保持；审计还要求本地 runtime status 的任期、逐库 applied、候选和 actor 与当前持久回放结果一致。同一绑定可在正常 Candidate/Abandoned 提交后重启，无须改原 SHA 或伪造 None。若当前压缩后的 snapshot 已越过原档案而无完整归档桥，本批明确拒绝审计及重启；不会关闭压缩或更改快照协议来绕过。

cluster 文件在网络启动前用受控 O_NOFOLLOW 打开一次，核稳定身份、完整长度和实际字节 SHA 后复制到 memfd，添加 WRITE/GROW/SHRINK/SEAL 四项 seal 并回读字节与 flags。SDK 只接收该 `/proc/self/fd/N`；同一个私有 catalog 与 `Arc<PinnedCluster>` 在预检和所有 authority reader 中共同存活。原路径后续只用于漂移检测。封存失败或源漂移均失败关闭；路径校验后换入再换回不能使 SDK 读到另一组 cluster bytes。模拟 factory 测试验证这一输入绑定，不代替真实 SDK 连接验收。
