# 双带库磁带管理 CLI 现场验收（2026-10-22）

`ltfsctl tape` 新增 `offline`、`online`、`blacklist`、`clear-blacklist` 与 `validate [--full-read]` 具名命令，复用现有按库作用域的 HTTP/Raft 管理任务；返回任务号后仍须查询终态。未修改服务端任务语义、SCSI 操作或磁带格式。兼容构建的 `ltfsctl` 最高 GLIBC 依赖为 2.34。

在 207/208/209 三台物理节点的新隔离候选上，只有 209 获得 B 库 `55L3A78011W9LL01` 的保留驱动 `/dev/sg4`（序列号 `117018606F`）及换带器 `/dev/sg5` 权限。A 库、B 的逻辑退役驱动和其余候选节点均无 SG 权限，EE A 保持运行，正式 17421 服务未替换。

R1 从固定 208 入口刷新库存后发现，B 库仅 AA0013L8 已分配，且有 23 个文件；其余实带均未分配。预设的策略验收条件要求另有已分配、已格式化、归槽且无文件的磁带，因此 **没有提交任何 blacklist、offline、clear-blacklist 或 online 任务**。R1 正常停机、AA13 归槽、B PR 清空。库存与清场证据在 `.scratch/ltfs-ha/probes/results/tape-policy-cli-physical-r1-20261022/`。不能把命令帮助或服务端既有模拟测试算作四项策略的实机通过。

R2 从 R1 停机状态克隆，用更新后的具名 CLI 执行 `ltfsctl tape validate AA0013L8`，不带 `--full-read`。显式 `library rescan` 任务 `1016249` 和索引校验任务 `1016294` 均在 owner 73、round `1015568` 下成功。校验确认 LTFS 卷代数 34、索引内 23 个文件；磁带始终归属原池，状态 online，卷 UUID/代数/文件数未变。之后独立 GET 已有跨库复制文件 `/gap-cross-library-20261022/a-serial-fuse-split-r2.bin`，实读 1,048,576 字节，SHA-256 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`，与此前一致。三节点查询结果一致。正常停机后 AA13 在槽 1、保留驱动为空、B `/dev/sg4/5/6` PR 均无注册或持有者、三个候选 MainPID 为 0、正式 17421 无设备 owner。`acceptance.complete`、`cleanup.complete` 和原始结果在 `.scratch/ltfs-ha/probes/results/tape-validate-cli-physical-r2-20261022/`。

R3 再从 R2 停机状态克隆，使用 `ltfsctl tape validate AA0013L8 --full-read`。显式库存刷新任务 `1020056` 和完整校验任务 `1020089` 均成功；卷代数仍为 34，索引有 23 个条目，其中 22 个文件的已有哈希匹配，`read_without_hash=0`。再独立实读原 1 MiB 目标文件，SHA-256 不变。正常停机后 AA13 槽 1、保留驱动为空、B PR 空、三个候选 MainPID 为 0、正式 17421 无设备 owner。双通过标记和原始证据在 `.scratch/ltfs-ha/probes/results/tape-validate-cli-full-r3-20261022/`。

用户随后明确：有大量文件的磁带不得触动其数据，其余磁带可作测试带。现场因此将已知约 108 万条目录记录的 `AA0000L8` 排除；旧驱动内的 `8A0042L8` 也未参与。R4 拟试 `L00284L8`，但脚本误将 `tape assign` 的文本回显按 JSON 解析，在装载前退出并完整清场。R5 修正后仅做该带 LTFS 标签及索引读取：读到第 662 代，数据分区索引跨块 5–5018，明显超出低文件量试验预期；未等文件数返回即主动停止，未提交四项策略任务，也未写文件。停机后该带归槽 4、AA00 仍在槽 2、B PR 清空。**索引跨度不能当作已确认的文件数**，但足以将 `L00284L8` 保守地列入保护范围。R4/R5 的失败和清场证据分别在 `.scratch/ltfs-ha/probes/results/tape-policy-cli-safe-r4-20261022/` 和 `tape-policy-cli-safe-r5-20261022/`。

R6 改用已知索引只有 23 个文件的 `AA0013L8`，只切换可逆策略并在结束后读取既有文件。固定 208 入口提交的 `offline` 任务 `1020079`、`blacklist` 任务 `1020806`、`clear-blacklist` 任务 `1020849`、`online` 任务 `1020894` 均在 B owner 73 下为 `success`。每步查询分别显示 offline、blacklisted、offline、online；最终三节点 online/原池一致，卷 UUID 不变、文件数仍 23。`online` 实装读取第 34 代 LTFS 索引后归槽。原 `/gap-cross-library-20261022/a-serial-fuse-split-r2.bin` 再实读 1 MiB，SHA-256 仍为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。正常停机后 AA13 槽 1、AA00 槽 2、L00284 槽 4，B 保留驱动空，三候选 MainPID 为 0、B PR 清空、正式 17421 无 owner；EE A 保持运行。R6 的 `acceptance.complete`/`cleanup.complete` 和任务原始结果在 `.scratch/ltfs-ha/probes/results/tape-policy-cli-known-r6-20261022/`。四项策略的具名 CLI 实机路径现已通过；这不代表库退役、全局副本/物理迁池或正式双库升级矩阵完成。

探针位于 `.scratch/ltfs-ha/probes/tape-policy-cli-physical-r1-20261022.py`、`.scratch/ltfs-ha/probes/tape-validate-cli-physical-r2-20261022.py`、`.scratch/ltfs-ha/probes/tape-validate-cli-full-r3-20261022.py` 和 `tape-policy-cli-known-r6-20261022.py`。`validate` 索引/完整读取及四项可逆策略均有实机证据。R4/R5 探针保留为保护带筛查和失败清场证据。

软件检查：`cargo test -- --test-threads=1` 全量通过，`cargo fmt --all -- --check`、`cargo check --all-targets`、`cargo clippy --all-targets --all-features -- -D warnings`、Python 语法检查、`git diff --check` 与 CodeGraph 同步/影响检查通过。命令帮助确认此前五个子命令及 `validate --full-read` 参数；R2 使用的 CLI SHA-256 记录在验收 JSON 中。

2026-10-23 又为原有按库 HTTP/Raft 管理任务补 `ltfsctl tape reconcile <barcode>` 与 `ltfsctl tape objects <barcode> [--offset N] [--limit N]` 具名入口。`reconcile` 从实带索引更新**集群目录**，不等同 GPFS HSM 对账；`objects` 只列 EE 保存对象视图，不是普通 LTFS 文件清单。两者返回任务号，必须再查询终态。CLI 限制对象页大小为 1–500。服务端任务和磁带格式未改。

独立 B-only 候选从上一轮停机状态克隆，209 只获 B `/dev/sg4/5`，固定 208 入口。`library rescan` 任务 `1026402`、`tape reconcile AA0013L8` 任务 `1026446`、`tape objects AA0013L8 --offset 0 --limit 10` 任务 `1028593` 均成功；后者返回 scope=`ee_saved_object_view`、total=0、objects=[]，这盘带上的 23 个普通索引条目不会被误报为 EE 保存对象。对账前后卷第 34 代、23 条目、池 UUID `10a0f22c-d50b-4c92-b057-90b35b1b7e1b`、卷 UUID `97b7990c-9e7a-4532-a765-ed718eaddf6c` 一致。原 1 MiB 文件实带读回 SHA-256 仍为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。

`acceptance.complete`/`cleanup.complete`、任务和读回原始记录在 `.scratch/ltfs-ha/probes/results/tape-reconcile-objects-physical-20261023/`；探针为同目录上级的 `tape-reconcile-objects-physical-20261023.py`。清场后 AA13 槽 1、AA00 槽 2、L00284 槽 4，保留驱动空，B `/dev/sg4/5/6` PR 空，候选 MainPID 均为 0，正式 17421 executor=null；EE A 原有 PR 与服务保持。兼容 CLI SHA-256 `c57cfad5a2de78ad2ea924a920bd5df0008a9f1c2dbfcd8c8e2fad513040eb31`，最高 GLIBC 2.34。此次全量串行测试 500 通过/4 忽略，fmt/check/严格 Clippy/diff、Python 语法和 CodeGraph sync/affected 均通过。只验收了 23 条目的 AA13 上的空 EE 视图；没有证明非空 EE 对象解析和分页结果，也没有正式服务部署。
