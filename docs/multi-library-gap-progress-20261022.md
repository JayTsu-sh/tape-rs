# 双带库剩余差距进展（2026-10-22）

本轮范围仅为：配置变更、跨库副本与迁池、文件任务与集群 CLI、正式现场验收。此前双库数据面和固定 VIP 的 [物理验收](multi-library-physical-acceptance-20261021.md) 不等于本轮功能已上线。

| 项目 | 本轮结果 | 尚未完成 |
| --- | --- | --- |
| 配置变更 | `ltfsd --expand-libraries-offline` 可在保留旧库与旧驱动的前提下增加新库或驱动；新增库为空。扩容前的控制/目录快照只在记录的 Raft 提交索引以内允许缺少新库，较新快照缺库会拒绝。连续扩容有链式记录。`--replace-drive-offline OLD=NEW` 仅允许同库单驱动显式更换，旧驱动策略随回放迁至新序列号。`--remove-drive-offline SERIAL` 可逐节点离线移除一台非末台驱动，旧策略和库存观测失效、指向旧驱动的排队任务转为结果未定。三类离线配置变更现在均拒绝本地未提交日志。三物理节点隔离候选完成 B 库逻辑移除和仅用剩余驱动读回 AA0013L8 的实带验收。 | 删除库、在线变更仍无安全事务；新增驱动、实体更换及真正拆除/缺席驱动尚缺对应物理验收。 |
| 跨库副本与迁池 | 新增显式普通文件复制：源版本稳定且本地 SHA-256 相符后，目标只创建、落带并实际 GET 读回核验；现代源记录的时间、只读标记和用户 xattr 随目标写带保存。条件删除仅在源版本未变时准入。双库分散 owner 模拟集群通过；B 库 AA0013L8 的同库复制已实带读回验证。A 库 RC0018L9 → B 库 AA0013L8 的真实跨库复制已分别在同节点 owner 和分散 owner（A/207、B/209）下通过，两次均在停机后的 B-only 冷挂中实带读回相同 SHA。现有目录仍按库和池隔离，尚无用户文件的全局身份或副本集合。 | 这些实带试验只证明显式内容复制，不是全局副本或迁池。不随改写同步；旧源记录缺失属性时仍只复制内容。仍需把 `library/pool/tape` 放置集合、故障读取、覆盖/删除/回收及物理出入仓纳入同一个提交合同；不能无条件删源当作迁池。 |
| 文件任务与 CLI | 文件任务受理/终态随 Raft 复制，接管前未获终态确认的任务保留原 ID 并变为 `indeterminate`；集群 CLI 覆盖 library、tape、drive、node、nodegroup、pool、管理任务的查询与具名管理命令。双库模拟集群通过；B 库实际写带后，在 207→209 owner 切换期间从固定 208 入口查询原文件任务为 `committed`。又修复无设备 owner 时固定入口错误转发已持久化任务查询的问题，三物理节点隔离候选经 `ltfsctl file-task` 查到原终态，`nodegroup create/show/delete` 及空池 `pool create/show/delete` 通过。B 库 owner 接管后，`library rescan`、保留驱动的幂等 `drive up`、角色 `mrg→mr→mrg`、磁带 `validate` 索引/完整读取、`offline→blacklist→clear-blacklist→online` 和临时空池 `pool set file_limit` 在三节点任务终态成功，旧文件实带读回 SHA 一致。固定 208 入口已在 A/207、B/209 分散 owner 时完成跨库复制。 | 其他管理写命令及故障中跨库文件任务的恢复合同，仍须物理验收。 |
| 正式验收 | B 库在三物理节点隔离端口完成新候选的单库写入、复制、任务接管与冷挂读回；A/B 由同节点以及分散节点持有时的显式跨库复制和 B-only 冷挂读回也已通过。正式 17421 服务没有替换，EE A 在写带试验后恢复。 | 跨库副本/迁池及配置退役完成后，仍须双库候选完整验收，再安排正式控制服务升级和故障矩阵。 |

离线扩容在从 207 复制出的**真实双库数据目录副本**上运行，新增虚构测试序列号 `TESTNEW20261022`，记录扩容前 Raft 提交索引 `1002771`。原 `raft.db`、`directory.db` 和 `cluster-directory.snap.db` 与现场原件分别保持 SHA256 `c6e31ba4…`、`5ee436c8…`、`3b9b183e…` 一致；这验证命令没有修改旧数据文件。该试验只在本地副本执行，不代表真实节点重启后的扩容验收。

软件验证：`cargo test -- --test-threads=1` 全量通过；最后只增加“缺失 Raft 存储/HardState 时拒绝扩容”的失败路径，相关单元测试通过。`cargo fmt --all -- --check`、`git diff --check`、`cargo check --all-targets`、`cargo clippy --all-targets --all-features -- -D warnings` 通过；双库模拟集群的原文件任务号接管后查询通过。一次默认并行的 HA 管理测试在 owner 换届窗口返回 `indeterminate`，单独及串行重跑通过；这仍须在真实故障矩阵中观察。

现场只读预检：207/208/209 的原 17421 控制集群仍运行，Leader 为 208，三个节点均无设备 owner；208 的 EE MMM 仍在运行。207 可见两库 FC 路径。A 库 `/dev/sg3` 的库存显示驱动空、RC0018L9/RC0017L9 在槽 7/8、CLNU03L1 清洗带在 I/E 口 2。B 库 `/dev/sg5` 一次库存读取返回 `status=0x02, sense_key=0x06, asc=0x2a, ascq=0x05`；只读 PR 状态显示无注册/持有者。没有据此推断 B 库库存或重试机械动作。本轮未执行装载、移动、写带、格式化、擦除、PR 写入或候选部署。

IBM 的 [副本创建](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=migration-replica-redundant-copy-creation) 与 [迁移磁带池到另一带库](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=libraries-relocating-tape-pool-different-library) 是两类不同流程。实现前需维持现有库身份、Raft/PR 隔离和用户文件单一逻辑身份，明确外部实体磁带转移步骤与失败恢复；源卷删除或重新格式化只能发生在目标副本/归属得到持久确认之后。

2026-10-22 接续：新增 `Client::copy_verified_to`、`ltfsctl copy`、`Client::delete_if_version` 和 `ltfsctl delete --if-version`；源版本改变/链接类型/目标已存在的拒绝，以及跨库双 owner 下复制、原版本被覆盖后的条件删除拒绝、正确版本删除均通过模拟测试。具名管理 CLI 的 `nodegroup create/delete` 通过真实可执行文件经模拟集群固定入口验证，仅影响所选库。全量串行 `cargo test -- --test-threads=1` 在这些功能第一次完成后通过；最后又收紧源类型检查并补充测试、增加具名 CLI，相关定向测试、`cargo check --all-targets`、`cargo clippy --all-targets --all-features -- -D warnings`、格式与 diff 检查通过。207/208/209 的正式 17421 HTTP 只读查询仍显示 Leader 208、三节点无设备 executor；本次接续未部署、未执行 SG_IO 或写带。复制和条件删除是后续副本/迁池的基础，尚无跨库原子放置记录、自动同步或物理迁池。

随后把显式离线扩容扩至旧库的**纯追加驱动**，记录新增序列号集合并保留旧库、旧驱动和快照；删减/替换仍拒绝，旧版只含新增库字段的扩容链仍可读取。全量串行测试在这次配置变更后再次通过；最后追加的旧记录兼容单测单独通过，格式、diff、check、严格 Clippy 和 CodeGraph 同步通过。实验室当前配置没有可新增并验收的独立驱动，本路径仍未获得真实硬件通过结论。

后续收紧跨库复制的竞争边界：只创建上传丢失响应时，同内容目标不能证明由本次请求创建，返回结果未定；目标确认落带后再次查询源版本，源在落带期间被改写则报校验失败并保留目标。这两条分别有客户端回归测试，仍不能替代跨库原子放置事务。最后一次全量串行 `cargo test -- --test-threads=1`、格式、diff、`cargo check --all-targets`、严格 Clippy 和 CodeGraph 同步/影响分析通过；此次只做软件验证，未重新操作物理设备。

随后把普通文件复制的原生元数据通过单次只创建上传带入目标，服务端在写带前校验来源格式、时间和 Base64 用户 xattr，过滤卷内版本/摘要等私有属性；双库模拟集群核对目标时间与用户 xattr，损坏属性单元测试通过。此项后的全量串行测试、格式、diff、check、严格 Clippy 与 CodeGraph 均通过；没有新物理验收。

后续把目标落带校验加强为实际 `GET` 读回全文件 SHA-256 和长度、前后版本/元数据稳定检查；不能只信目标 `stat` 中的摘要。模拟客户端新增损坏目标读回的拒绝测试，完整串行 `cargo test -- --test-threads=1`、格式、`cargo check --all-targets`、严格 Clippy 和 diff 检查通过。B 库真实 AA0013L8 上用隔离三节点候选追加源文件和复制目标，各 256 KiB；`ltfsctl copy` 对目标实际读回成功，SHA-256 均为 `4339d49b90cb0ba34206bc9c0fd8d04ccebb7c8a76e925837b121432d9c175df`。B owner 从 207 切至 209 后，经固定 208 入口查询文件任务 `4307040449069057` 为 `committed`，目标 GET 仍匹配；新 CLI 的 `file-task`、`library list`、`tape list` 通过。证据位于 `.scratch/ltfs-ha/probes/results/readback-b-physical-r6-20261022/`，其中 `acceptance.complete` 与 `cleanup.complete` 均存在。此试验只使用 B 库 `/dev/sg4/5/6`；EE 持有的 A 库未触及。

R6 清场时先停了两个非 owner，最后停 B owner，导致失去多数派许可后的 IP 检查点失败；没有把此失败掩盖成成功。独立冷挂 AA0013L8 确认 DP 第 31 代、IP 第 28 代、维护债务为 true，但两文件按 LTFS 索引分别实读全长和 SHA-256 正确。R7 保持 208/209 控制多数派，先停止 owner 207；脚本错误地要求出现新代提交日志，故主断言失败，但独立只读冷挂复核显示 **IP/DP 均为第 31 代、VCI 双命中、维护债务为 false**，两个文件仍在。这证明既有同代 IP 检查点已落带；它不生成新代提交日志。R7 的候选和维护 PR 已清理，AA0013L8 回槽 1、HH8 空、B 库 PR 无持有者，正式 17421 executor 仍为 null。证据位于 `.scratch/ltfs-ha/probes/results/readback-b-checkpoint-r7-20261022/` 和 `.scratch/ltfs-ha/probes/results/readback-b-checkpoint-verify-r7-20261022/`；后者有 `verification.complete`、`cleanup.complete`。R1/R2/R3/R4/R5 失败及安全清场记录也保留，不计入通过次数。

R8 纯控制面三节点复验（候选 `DevicePolicy=closed`、无 SG 许可）发现旧固定入口把 `GET /pools/<池>/tasks/<id>` 错误地按数据请求路由到设备 owner：无 owner 时 CLI 重试后报 `not_serving`，尽管复制目录已有 committed 记录。修复 `src/daemon/gateway.rs` 后，合法只读文件任务查询改为入口本地读取，写请求仍按原 owner 路由；双库模拟集成测试在 B 无 owner 时验证旧任务可查，完整串行测试、fmt、check、严格 Clippy、diff 均通过。R9 已证明任务查询和节点组命令成功，R10 已证明删除完成；两轮最终断言分别误期待删除响应回显名称、引用不存在的探针常量，均已清场。R11 用新的隔离状态完成全套：固定 208 入口的 `ltfsctl file-task 4307040449069057` 返回 `committed`、generation 29，`nodegroup create/show/delete` 成功，删除后 A 库未见该组；三机候选 MainPID=0，B 库 PR 为空、AA13 仍在槽 1、正式 17421 executor=null。R11 `acceptance.complete`/`cleanup.complete` 与原始 CLI 输出见 `.scratch/ltfs-ha/probes/results/readback-b-cli-control-r11-20261022/`。R8–R11 均未给候选任何设备访问权限，不能把它们当作新的写带验收。

本次接续补上显式离线单驱动更换：三节点停机后逐节点用 `--replace-drive-offline OLD=NEW` 和新配置记录同库更换，旧驱动的启停、角色和节点组策略在旧快照及日志回放时迁至新序列号；旧库存观测不会冒充新设备。命令拒绝未提交的本地 Raft 日志、跨库或批量更换，以及伪造的更换记录。定向单测覆盖连续更换、策略合并、库存失效和拒绝路径。用此前来自 207 的双库数据目录**本地副本**运行真实 `ltfsd` 二进制，更换副本中虚构测试库的驱动 `TESTDRIVE20261022` → `TESTDRIVE_NEW20261022`：退出码 0、原提交索引 `1002771`、链式记录增加一步，原 `raft.db`、两库目录及集群目录快照 SHA-256 不变。证据见 `.scratch/ltfs-ha/probes/results/drive-replace-local-20261022/probe-success.json`。先前故意删除 `before_index` 的故障夹具被命令拒绝，记录在同目录的 `probe.json`。当前没有可替换的实体驱动；此软件验收不代表真实硬件或正式三节点升级通过。

同时收紧驱动管理命令：`up/down/role/assign` 必须以当前 owner 轮次的显式库存观测为依据，未知或旧序列号不能在 Raft 管理状态里留下虚构策略；换届或更换驱动后须重新 `library rescan`。这条边界已有纯单元测试，实体驱动流程仍待现场验证。

本次再补上 `ltfsd --remove-drive-offline SERIAL`，用于保留带库的单驱动逻辑退役。它拒绝本地未提交日志、批量/跨库或最后一台驱动的移除，按旧快照与日志回放消除退役驱动策略、库存及未完成任务。完整串行测试 500 通过/4 忽略，fmt/check/严格 Clippy/diff 检查通过。来自 207 的数据目录本地副本验证三步配置变更链和数据文件哈希不变；随后 207/208/209 隔离候选在相同提交索引 `1005342` 上逐节点变更，SG 全禁止的控制面启动及旧文件任务查询通过。第二轮只允许 B 的换带器和保留驱动，实带 `AA0013L8` 既有 256 KiB 文件读回 SHA 一致；正常停机后归槽、B PR 为空、三节点候选停机、正式服务无设备 owner。退役驱动仍实体存在，且内有原来的 `8A0042L8`，所以未验证真正拔除驱动。详见[驱动移除现场验收](multi-library-drive-removal-physical-20261022.md)。其余库退役、Raft 全局副本/物理迁池、剩余管理 CLI 实机覆盖和正式升级矩阵仍未闭环。

本次接续的全量串行 `cargo test -- --test-threads=1` 退出码 0，另有格式、diff、`cargo check --all-targets`、严格 Clippy 与 CodeGraph 检查通过；测试日志 `/work/tmp/tape-rs-test-drive-replace-final-20261022.log`。正式 207/208/209 的 17421 `/cluster` 只读核查见 `.scratch/ltfs-ha/probes/results/drive-replace-local-20261022/formal-http-readonly.json`：208 Leader，三个节点均无设备 owner。未部署新代码，未访问 `/dev/sg*` 或执行机械/写带命令。

后续发现离线扩库/扩驱只读取 Raft 提交索引，没有像离线更换/移除驱动一样拒绝未提交日志尾部。现三类离线配置变更共用同一检查：若 `entries` 最后索引高于 HardState 的 `commit`，在写配置标记及 manifest 前拒绝。新库和旧库新增驱动两条测试都覆盖了拒绝、配置不变及尾部清除后的成功。该保护不改变已提交日志和目录文件；它也不支持在线变更或库退役。

用真实三节点状态的本地衍生副本运行当前 `ltfsd`：在提交索引 `1002771` 后人为加入第 `1002772` 条未提交夹具时，离线扩驱退出码 2，原 manifest 与配置变更标记未动；删除夹具后相同命令成功，配置链增至四步，而 `raft.db`、两库目录和集群目录快照的 SHA-256 均不变。证据见 `.scratch/ltfs-ha/probes/results/config-floor-local-r2-20261022/`。这是本地副本验收，不是三物理节点正式停机变更。

另以三台**物理节点上的隔离控制面候选**完成 B 库空池 `pool create/show/delete`、跨节点一致性、A 库隔离、磁带/库/旧文件任务只读 CLI 查询验收；所有候选 SG 权限关闭。R12 因错误地要求 `library show` 输出库序列号而失败并清场；R13 用新目录完整通过，清场后候选 MainPID=0、B PR 为空、AA13 在槽 1、正式服务无设备 owner。证据与边界见[池管理 CLI 现场报告](multi-library-pool-cli-physical-20261022.md)。这不覆盖需设备 owner 的池属性更新或磁带/驱动管理命令，也不验证新离线代码在物理节点运行。

随后以 B 库单 owner 的隔离候选完成 `library rescan` 与保留驱动幂等 `drive up`：三节点任务均为 `success`，显式库存只列配置内驱动，原 256 KiB 文件在驱动管理任务前后读回 SHA 一致；R3 停机归槽、PR 释放、正式服务无 owner。R1 CLI 端点格式错误未提交任务，R2 把任务终态后的短暂 503 误判为最终读回失败，两轮均已清场。详见[驱动管理 CLI 现场报告](multi-library-drive-cli-physical-20261022.md)。任务成功与数据面重新就绪之间存在可重试的 503 窗口；本次没有验证非幂等驱动策略、磁带管理写命令或正式部署。

另以新的 B 库 owner 候选验收空池 `file_limit` 更新：临时池上限从 37 改为 41，任务 `1006048` 成功且三节点一致，删除后临时池消失，A 库未受影响。AA0013L8 原文件再次实带读回 SHA 一致；候选停机后归槽、B PR 清空、正式服务仍无 owner。R1 探针误把 `pool create` 的成功文本当 JSON 而失败并清场；R2 完整通过，证据见[池管理 CLI 现场报告](multi-library-pool-cli-physical-20261022.md)和 `.scratch/ltfs-ha/probes/results/pool-set-physical-r2-20261022/`。仍缺磁带分配、其他池策略及非幂等驱动/磁带管理的真实环境覆盖。

此后用当前源码构建的兼容二进制在 207/208/209 的独立候选上完成 **A→B 真实跨物理带库显式复制**：A 的 RC0018L9 上既有 1 MiB 文件作源，B 的 AA0013L8 新路径作目标；源版本不变，目标落带、客户端读回及独立 HTTP GET 的 SHA 均一致。停机后恢复 EE，A/B 原带归槽、A 恢复 EE 原有 PR、B PR 清空。再从停机状态独立冷启 B-only 候选，实带读回新目标同一 SHA 并清场。R1 探针命名错误只启动 SG 全禁的控制候选，已清场；R2 与冷挂 R1 的双完成标记见[跨库复制现场报告](multi-library-cross-copy-physical-20261022.md)。这仍是两套库内目录间的显式内容复制，不是全局副本放置、自动同步或物理迁池；正式服务没有升级。

再以 A/207、B/209 **分散 owner** 复验同一显式复制路径：固定 208 入口的 `ltfsctl copy` 把 A 的 1 MiB 文件写入 B 的新路径，源版本 `[1000135,1]` 不变，B 目标 AA0013L8 的实读 SHA-256 为 `4e29ad18ab9f42d7c233500771a39d7c852b200baf328fd00fbbe3fecea1eb56`。只读 A owner 正常停机；B owner 在控制多数派下完成第 34 代 IP 检查点。EE 和 PR 恢复后，又在仅给 209 B 设备权限的新候选中冷挂目标并读回相同 SHA。清场确认两带归槽、候选停机、正式服务无 owner。首轮 SSH 复用拒绝导致预检即退出，没有开放 SG 或停 EE；成功 R2 与冷挂 R1 的证据见[分散 owner 现场报告](multi-library-split-owner-copy-physical-20261022.md)。这补齐分散 owner 的显式内容复制实带验收，但没有全局副本/迁池合同或正式升级。

随后在独立 B-only 候选上验收保留驱动的实际角色策略变更：从固定 208 入口提交 `library rescan`、`drive role 117018606F mr`、`drive role 117018606F mrg`，三个 Raft 管理任务均成功，三节点最终角色一致。管理执行器先 checkpoint 并归槽，角色恢复后从 AA0013L8 实带读回上述 1 MiB 文件，SHA 不变；正常停机归槽、B PR 清空、正式 17421 无 owner。R1 探针的空驱动预断言早于管理任务内部卸载而失败，已清场；R2 完整通过。见[驱动角色现场报告](multi-library-drive-role-physical-20261022.md)。这只关闭驱动 `role` CLI 的现场路径，其余缺口保持未完成。

再补 `ltfsctl tape offline/online/blacklist/clear-blacklist/validate` 具名命令。B-only 隔离候选实查只有 AA0013L8 已分配且有 23 个文件，故 R1 在提交四项策略任务前停止并完整清场。R2 使用新 CLI 对 AA13 执行默认索引校验，任务 `1016294` 在 owner 73 下成功；R3 的 `validate --full-read` 任务 `1020089` 成功，22 个文件哈希匹配。用户明确保护大量文件后，R4/R5 对 `L00284L8` 做候选筛查，R5 读到第 662 代和约 5014 块的大索引即停止并清场，将该带也排除。R6 改在已知仅 23 文件的 AA13 上执行四项可逆策略，任务 `1020079/1020806/1020849/1020894` 均成功，最终状态 online、原池/卷 UUID/文件数不变；原目标 1 MiB 实读 SHA 不变。AA13/AA00/L00284 均归原槽、保留驱动空、B PR 清空、正式无 owner。证据见[磁带 CLI 现场报告](multi-library-tape-cli-physical-20261022.md)。

2026-10-23 继续以隔离 B-only 候选对保留驱动执行实际 `down → up`，管理任务 `1023999/1024703` 均成功，策略 `enabled=true → false → true`；AA13 先归槽，恢复后旧 1 MiB 文件实带读回 SHA 不变。AA00/L00284 未装带，EE A 保持运行；停机后 AA13 归槽、B PR 空、三候选停止、正式服务无 owner。见[驱动管理 CLI 现场报告](multi-library-drive-cli-physical-20261022.md)及 `.scratch/ltfs-ha/probes/results/drive-down-up-physical-20261023/`。这关闭保留驱动的启停路径，尚不覆盖物理缺席驱动或正式升级。

同日新增 `ltfsctl tape reconcile/objects` 具名入口，复用已有按库 Raft 管理任务。B-only 隔离候选在固定 208 入口完成 rescan `1026402`、AA13 索引对账 `1026446` 和 EE 保存对象查询 `1028593`，三任务均成功；对象结果为空，与普通文件不属于 EE 保存对象视图的语义一致。卷代数 34、23 条目、池/卷 UUID 前后不变，旧 1 MiB 文件实读 SHA 不变。高文件数带未动，正常清场及软件 500 通过/4 忽略见[磁带管理 CLI 现场报告](multi-library-tape-cli-physical-20261022.md)。非空 EE 对象视图、其他管理任务、库退役、全局副本/真正物理迁池和正式升级矩阵仍未闭环。

随后在新 B-only 隔离候选验证节点组与归属路径：临时组 `BTEST20261023` 创建/删除成功，非 owner 节点 71 的组别 `G0→BTEST20261023→G0` 两个任务 `1033020/1033736` 成功，保留驱动显式 `assign G0` 任务 `1035082` 成功，三节点最终策略一致。AA13 旧 1 MiB 文件实带读回 SHA 不变，保护带未动，停机归槽/PR 清空/正式无 owner。见[节点组现场报告](multi-library-group-policy-physical-20261023.md)。这只关闭节点跨组与驱动幂等归属路径；唯一驱动的跨组迁移若与现有池组不同时切换，会使调度失去可用驱动，尚无联合事务与物理验收，不能宣称该差距完成。
