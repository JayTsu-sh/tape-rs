# 接续更新（2026-11-25，全工作树审查未通过）

- 用户要求审查全部未提交代码并在通过后合入 GitHub main。远端 API 一次返回 `085c5470b6c4f72c1eea86f1d815c4eb96b3d24a`，本地 main 在其上有四个提交。当前未提交工作包括 105 个已跟踪文件变更、98 个未跟踪文件；审查未通过，没有提交、合并或推送。
- 新发现交还重试竞态：`fdb_http_handoff::submit` 可在恢复执行器已经设置 `release_retry_used=true` 且原设备命令仍在执行时，将它设回 false。执行器随后按旧状态保存设备结果可能 CAS 失败，另一轮可能重发。须持久区分原尝试正在执行、已结清与结果未知，恢复时证明原在途命令结束后才能重试；不能单凭延时或清标志解决。
- 原混部问题仍在：未对所有执行节点的交还协议支持能力建立可验证门槛。旧版执行器可能忽略共享交还状态并抢占外部应用的设备。
- `src/daemon/s3.rs` 删除既有公开 `s3::serve`，`daemon::s3` 改为仅 fdb feature 可用，且删除旧 Sim S3 集成测试和 `tests/s3_clients.py`。这些是调用方兼容性和测试覆盖变化，须核对既有授权及迁移/替代回归后再判定是否合并。
- 本轮默认 `cargo test`、`cargo test --features fdb --lib`（872 通过、262 忽略）、`cargo check --all-targets`、全特性 Clippy、`git diff --check` 退出0；全树 fmt 108 处已有差异。未触及现场，独立审查代理工具不可用，两轴完整审查仍未完成。

# 接续更新（2026-11-25，交还代码审查未完成）

- 用户要求审查通过后合入 GitHub main。当前仓库有大量先前未提交改动，交还增量与其在 executor、files、managed_catalog、reservation 等文件交叠，无法整文件提交而不带入无关 WIP。已建只读分析用 detached worktree `/work/tmp/tape-handoff-review-20261125`，未提交或推送。
- 审查发现交还准入缺少所有执行节点具备交还协议能力的持久证明。混部/回滚时旧节点可能忽略 FDB 交还状态并抢占外部设备，须设计并验证节点版本/资格门槛后再开放交还。现有现场三节点版本一致，只支持已执行的限定实测，不能证明混部安全。
- 已修复健康检查把设备失权当作一般失败的问题，`prepare` 遇到预留冲突或所有权失效立即返回 Lost，执行器关闭原轮次。定向模拟回归 `health_preserves_reservation_conflict_for_executor_fencing` 通过，`cargo check --all-targets --features fdb` 通过。该修复尚未部署或实带验证。
- GitHub SSH 连接关闭，HTTPS Git 和 `gh api` 均报 TLS/EOF，无法确认远端 main 最新状态。未合入、未推送。四个本地提交领先旧 origin/main；GitHub 恢复后先查询真实远端，明确本次功能依赖并在隔离工作树审查和验证。

# 接续更新（2026-11-25，整库交还与重新加载实带验证）

- 用户授权结清原回收任务并部署验收。原 `8641dda1-79f0-4eee-90da-483194a76422` 在 207/878355 接续，8 个对象迁移后 `succeeded`，L00284L8 初始化一次，新卷 UUID `e333b41a-a750-44ec-8218-504c49827e43`。取消端点在恢复期间返回 owner 未就绪，未再提交回收。
- 三个隔离 benchmark unit 当前运行 `/opt/tape-rs/library-handoff-20261125-3727e766/bin/ltfsd`，SHA256 `3727e766f83f82f88c0bc98b87c4e49c46554e94662d141a2639335d27bf7f53`。三节点 ready，B 库 owner 207/886663；原业务 unit runtime mask 保留。
- 首次交还 `9c8997c8-c424-41b8-909f-92936ff96556` 失败，I/E 元素 0x65 的 CLNU01L8 返回清洗带 30/03。依据元素类型、full、清洗身份和 ASC/ASCQ 修正健康判定后部署 v2。交还 `cb0dbaf7-3541-4c22-a50a-20ffb2e5252f` 达 `released`。208 独立发起端为 B changer、两驱动取得并回读独占 PR，加载 `d9a2d5c1-95eb-4a00-8c58-389859cfbf11` 显示 blocked；只释放本轮外部 PR 后以同 UUID 重试，回执 `online`。
- 恢复后从 L00285L8 实读旧文件 `/reclaim-v4-13d33e5a-5bec-43c9-ab23-db11eaa6b150-f07.bin`，1048576 字节 SHA256 `11ea5ca0dc771edab4723ab4b858dce324b9106aeabc7ac4ccccbaea49911446` 与历史记录一致。证据 `/work/tmp/tape-handoff-field-20261125`。未验证故障注入、加载中断接管和全部数据读取；本次实带结果限定上述路径。

# 接续更新（2026-11-25，整库交还与重新加载已接入）

- main 4e8cc0c，保留原有未提交改动。FDB writer 支持按库提交交还与重新加载，SDK、`ltfsctl library unload/load/handoff-status` 和 HTTP 共用持久 UUID 回执。交还健康检查失败时保留原预留，驱动器内介质原位保留。部分释放由原发起节点按原键续接。交还期间阻止普通接管，加载使用 REGISTER/RESERVE 取得设备，遇到外部预留报告 `blocked`，可按原加载 UUID 重试。
- 加载发布完整库存后进入新执行轮次，保留 FDB 业务目录。旧卷首次写入或收尾前核对原索引和完整尾部。入口、状态及升级约束见 `docs/reference/library-handoff.md`，验收规格见 `.scratch/library-handoff/SPEC.md`。
- 私有 FDB + SimLibrary 的 HTTP/SDK/执行器联测通过，覆盖留带、拒绝不健康库、部分释放续接、交还后抑制普通接管、外部预留阻塞及同 UUID 重试、原管理任务忙拒绝。默认 796 项和 FDB 库 871 项通过。未部署，未运行 Holo 或真实带库验收。

# 接续更新（2026-11-19，v7 冷装载实带已验，已装载轮按用户指示停止）

- v7 在隔离 B 库三台部署，构建源 6786132800bf72e7eaf2ab1e0097dec3dadd23c10038b35eac0ec3d1c8c6d6b6，EL9 ltfsd 55f1017611b495004442de4f725cabb526d3775031976d65736c48435386735c。207/853461 接管恢复，原 v6 reclaim 终态字段保持，剩余 7 项独立 GET 实读和摘要核验完成，合计 48/48。
- v7 冷轮 e7f0350c-c9d4-4755-a14d-0fb6ed7ec6c2 双冷 8×1 MiB 成功，服务端 281.268s，较 v6 312.300s 少 31.032s/9.94%。目标 48→56，原文件身份保留，8 新文件独立实带读回通过。源空卷 UUID 7f38c3cf-892f-4eed-b312-afe004e52446，目标 UUID 保持。本地旧 FDB SDK 路径失效时用原摘要副本恢复控制端观察。360.086s 是首次重新读取终态，服务端时间用于比较。
- 已装载轮原任务 8641dda1-79f0-4eee-90da-483194a76422，停止前最后可读状态 running/reserved，children=0。目标 /dev/sg6 在历史实读的 slow LOCATE/READ 共 1079.206s，P1 block68 LOCATE 418.713s GOOD，同块冷轮约 15ms。所有已见 host 字段为 0。已装载轮未到目标预挂载、写入或源初始化，原任务及检查点保留。本轮管理取消回执未定，无已装载终态耗时。
- 用户要求停止后，原本地控制观察停止，208→209→207 的三个隔离单元正常停机。三台终态 inactive/dead、MainPID/ControlPID=0、指定设备 busy=0 且提交完成数相等。专属 SSH masters 已关闭并移除目录。FDB、磁带内容和原业务 unit 保留。后续只有获准时恢复原任务，先核对原状态，避免重提交或盲改权限。
- 证据 [实带报告](/root/jay/cicd_local/reclaim-v7-physical-7QBURWQq/RESULT.md) · [完整目标命令时间戳](/root/jay/cicd_local/reclaim-v7-physical-7QBURWQq/WARM-DEVICE-REPLAY.json)。本轮修改只含本地基准工具和交接记录，生产源码沿用冻结 v7。

# 接续更新（2026-11-19，reclaim v7 软件验证通过，真实带库待复测）

- main 4e8cc0c 与原 WIP 保留。实现与独立分析分别由不同子 agent 完成。冻结十九文件增量清单 SHA-256 `26359e77e300b2b166b5ce025405587c3760962071e8818a139838e43492b244`，软件源码 `/root/jay/cicd_local/reclaim-v7-software-mFk4yR/verified-source`，入口 [软件报告](/root/jay/cicd_local/reclaim-v7-software-mFk4yR/REPORT.md)。
- FDB 原终态 Rf 与留存组证据重算同一 v4 CloseSeal 根覆盖，未终态继续要求 Rm/Re/Rq。旧版新回归命中清理期间已完成批次失败，候选覆盖分页/完整清理、回执损坏拒绝和下一 owner 仅观察及读回，保留原任务和批次。
- 目标 fresh S4 后和源组尾 B 先核验 DP，再在一次 IP 停留中实读 P0 标签并核对，所有原物理核验和归档发布门槛保留。源空卷在一次实读中构造原终态证明，每分区仍核验实际 EOD 和位置。确定性 quick reset 以可信实时位置省两条同分区 LOCATE。
- 同一 reclaim 调用中把目标只读预挂载移入与源正文读取并行的阶段，正式写前核对原 FDB scope、PR、会话代次/token 和实际 P1 追加点。额外挂载每份索引受现有并行预算和 16 MiB 上限约束，已确认容量不足时复用原串行挂载路径。正常 WRITE 改变 token 时仍确认原回执，错误追加点零写入。新增成员结清、关闭、覆盖/历史和权限开销计时。
- 最终冻结输入 7 项隔离 FDB+Sim 定向回归、21 项 LTFS 历史 Sim、两套软件全量范围验证通过。默认 806 项、FDB 库 872 项均排除原有同断言偶发失败的 control-only 用例。check、全特性 Clippy、git diff check 退出 0，109→108 处已有全树格式差异。所有本轮私有 FDB 退出，先前独立实例保持。定向 libtest 使用 32 MiB 栈。
- 本轮未访问真实设备或部署。最后观测仍是下方 v6 现场状态。现场工作应先验证原 completed reclaim 后接管恢复，续验已存前 41 项之后的 7 个独立 GET，保留原结果及任务身份。获准复测时再分冷装载/已装载，用相近目标初始历史记录性能。MAM 合并写待实带兼容性验证，正文流式待办留待验收后。

# 接续更新（2026-11-19，reclaim v6 实带基准完成，独立后验待接管修复）

- run `982b193c-18e4-4abe-92af-ed542cc23350` 在隔离 B 库 `55L3A78011W9LL01` 从 1004/1006 双冷启动。原任务 `ad8dd285-f83a-4a64-bdd8-d28c42dd2cb5` 一次发令，207/776289 稳定至 succeeded，L00284L8 → L00285L8 迁移 8×1 MiB。源初始化一次成空卷 `6b6985f9-1bd4-4ea9-96ef-26ca2b66bc30`，目标 UUID `b1b34698-c984-46e3-bcef-b73d2890a258` 保持、40→48，成功后两盘 taskNone。控制端 312.571574458s，服务端 312.300s，较 v5 分别少 34.898357196s/10.04% 和 34.788s/10.02%。v5 目标起始 32 项，v6 起始 40 项。
- 组尾 B 46.217→24.214s，目标 LOCATE 13→6。B 完成至采用源格式化证明 25.082→8.259s，源 LOCATE/READ/SPACE 9/49/6→0/0/0。冷装载源初检与目标就绪等待实际重叠 13.622s，复用日志命中。301.842s 诊断触发，取样时仅源驱动忙，原 quick 已在服务端 294.687s 完成，正做空卷 P1 EOD 读回。完整结果及逐阶段证据 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v6/field-prep/PERFORMANCE.md`。
- 原 reclaim 内部实读并核验全部 8 份新正文和目标组尾。独立后验已保存 41/48 次 GET 的正文 SHA、精确 LTFS 读取记录与实际目标 READ，原 40 项 ID/版本保持。第 42 项独立 GET 在目标 READ 120.029s/host=0x3 后返回 FDB owner 暂不可用。207/208 自动交替接管，能读出目标 DP9/IP4，随后在 FDB 已完成批次核对报“目录索引未完成”，连续 270s 探测未恢复就绪。旧 closed group checkpoint 仍要求清理阶段删除的原源清单与覆盖记录，v5 冻结源码也存在该调用链。剩余 7 项未 GET，后验只能在库恢复后按已保存状态续读。没有新 reclaim、重新初始化、清库或手动抢占。该故障和独立 GET 耗时不计入 312.300s 的原 benchmark。
- 1812 输入的 EL9 包源清单 `c008989d9e19c3ce9f150615eadb3eb6b5e916431405f1ee8d4d53594c787519`，三个隔离单元实际 ltfsd `9e2261867497a956ca84b669a317e3d41c6e3400ec4ee04ee782921daf4f9914`，路径 `/opt/tape-rs/reclaim-bench-982b193c-c008989d9e19/bin/ltfsd`，PID 1728425/3550880/2587425。最终 active 但 B 未 ready，原业务单元仍 mask。专属三个 SSH master 已退出，`/run/trb-v6-982b193c` 已移除。未改生产源码，流式正文待办原样保留。

# 接续更新（2026-11-19，reclaim v6 三项小数据优化已通过软件验证）

- main 仍为 4e8cc0c。按用户顺序完成三个批次。目标 B 沿 A 的临时历史目录按 DP 物理块升序实读，仍核对逐代摘要、回指、文件标记、DP/IP 和第二次 EOD。预算不足时使用原核验。相同 Sim 夹具的 B 普通 DP LOCATE 从 7 次降到 2 次，两次 EOD 保留。
- `group-only` 新鲜组结清且原 FDB 枚举已返回 Done 时，使用同轮实读清单和原并行候选完成有界覆盖检查并保存尚未发令的初始化计划。每页继续验证原覆盖和引用。下一轮执行原写前完整实读，未知结果和接管仍续接原尝试。
- 双冷装载仅在两次原 MOVE 同轮确认后，将源初检与目标只读就绪等待并行。目标明确需要实际 LOAD 时，先收回源读句柄，再执行原串行 LOAD 并重读源。Sim 自动就绪路径只捕获源索引一次。
- 21 项 LTFS 历史 Sim、8 项隔离 FDB+Sim 定向通过，含原 MOVE 丢回执、许可关闭、目标读失败及 S4 接管。最终夹具调整后，两条冷装载用例重新编译并通过。默认 806 和 FDB 库 865 通过，均排除已在 v5 同断言复现的一条 control-only 基线失败。编译和全特性 Clippy 通过，Clippy 告警范围与 v5 一致。全树有与 v5 相同的 109 处格式差异。
- 最终源码 `/work/tmp/reclaim-small-benchmark-20261117/optimization-v6/final-source`。验证入口 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v6/software/REPORT.md`。本轮仅软件验证，未部署或操作实带。207–209 最近实带结果仍为 v5 的服务端 347.088 秒。正文流式待办保留在下文。

# 接续更新（2026-11-19，reclaim v5 双向只读并行实带已完成）

- 当前 main 保留原工作树未提交内容。已实现源组读取与目标只读预检重叠，以及目标正文和组尾核验与源格式化来源只读读取重叠。两台 LU 分别独占句柄，停手等待在途命令退出，原 FDB/PR、目标 Fresh 和源写前实带检查保留。冻结 1812 文件源码清单 SHA-256 `3a0dcec86c0f95f9cba4d3c293b05d799243a78fa4d18ed9daadcb516e7f812e`，目录 `/work/tmp/reclaim-small-benchmark-20261117/optimization-v5/frozen-source-v5`。用户追加的流式待办在冻结之后写入本交接。
- 隔离 B 库 run `d4a3c21b-e077-434a-9dcc-9ca675f8a24b`，一次提交原任务 `75c081ae-b312-4d4b-bbd0-b07fa145f7d4`，node207/round711735 全程。L00284L8 原 UUID `83fe28b0-7f9c-45c8-8022-aa8f0ab662e5` 的 8 × 1 MiB 迁至 L00285L8。目标 UUID `b1b34698-c984-46e3-bcef-b73d2890a258` 保持，32→40 个对象，原 32 ID/版本保持，40 个对象实际读带和正文 SHA 全过。源初始化 1 次，空卷 UUID `0b86061b-ddc3-4286-9a2c-dd10fec6d8a9`。两盘 taskNone。
- 双冷实际控制端 347.469931654 秒，服务端 347.088 秒，较 v4 控制端少 24.482229156 秒 / 6.58%。前段并行窗口 20.203 秒、后段 26.054 秒，真实动作或正文 SG_IO 重叠 15.419/9.210 秒，两份证据都实证复用。目标起始历史 32 vs v4 24。300.256 秒诊断时已进入源 quick，只有源 sg4 忙。后验 GET 602.340627101 秒单计。所有现场实证在 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v5/field-prep/RESULT.md`。
- 三台隔离服务均运行新 EL9 候选 `08b5c48e451859c6fd1094c47989be392e041160fd7c3be59e9ab28ac4c9ccab`，路径 `/opt/tape-rs/reclaim-bench-d4a3c21b-3a0dcec86c0f/bin/ltfsd`，PID 1652297/3466701/2518232。B owner1/711735、gateway2/711725。原业务单元 mask，专属 `/run/trb-v5-d4a3c21b` 三连接和目录已关闭、移除。其他资源按原状态保留。
- 私有 FDB+Sim 四个定向用例均过，19 个 LTFS 历史模拟场景通过，check/clippy 和 git diff check 通过。control-only 的一个全量用例在冻结旧版和候选同一断言失败，默认套件明确排除此基线用例后 806 通过。全树有 109 处原有格式差异。早段并行正例预算 256 MiB，目标每路至少三份 16 MiB，晚段辅助源预读 64 MiB 并为目标保留完整预算，实带现有配置 128 GiB。软件证据见 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v5/software/REPORT.md`。

# 待办：reclaim 流式优化（本轮双驱动并行验收后推进）

- 用户要求先完成当前双驱动只读并行验收，再实施流式优化。本轮验收使用有界暂存预算及原可核验的低预算串行路径。
- 大数据正文采用有界流水线。源驱动按 FDB 封存的 extent 顺读并计算摘要，目标驱动顺序追加已验证的上一段。以暂存字节预算背压，保留原写意图、阶段回执、目标落带和实读证明，并覆盖中途故障与 HA 接管。
- 索引 XML 已流式解析。下一步查明完整历史核验中可合并或逐页比较的临时结构，减少同时存在的暂存量。保留回指、摘要、文件标记、DP/IP/EOD 校验。FDB 持续作为目录、副本、extent 和任务的业务权威，临时 SQLite 按原约定使用。

# 接续更新（2026-11-19 04:03 UTC，reclaim v4 实带已完成，本段优先）

- 用户要求继续优化并保持分析/改动由不同子 agent 执行。v3 同节点时间显示提交至首 MOVE 2.575s、源/目标 MOVE 23.416/29.944s、两 MOVE 间隔 10.896s（含源就绪 2+4s），不存在客户端首见 reserved 暗示的 24s 调度等待。
- v4 group-only 源带从槽位搬入并确认原 MOVE 后立即顺序搬入目标带，再分别确认就绪。已有源带在驱动器、旧默认模式保留顺序。快路径初始化完成后按 IP/DP 两分区固定配方逐条实读、实际 BLANK、MAM、独立完整 DP/IP/EOD、原介质和 PR 核验，省去两次单独标签定位。写前与未知结果路径照旧。所有最小变更和验证收在 /root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v4。
- 私有 FDB+Sim 源 MOVE 成功回执丢失、原 pending 只观察与双驱动优先安排顺序均通过。v3 基线断言检测到 4 而非 3 次中间 TUR，以及格式化后的 2 次重复标签定位。普通 EL9 候选冻结清单 8f03a63a40a76ea600a0866875328e7287328a8b085565914df5a09b4dc101b4。main 驱动文件只有额外已记录的 rustfmt 等价差异。
- 真实隔离 B 库同池新 run 13d33e5a-5bec-43c9-ab23-db11eaa6b150。源 62de170a-ecf3-43f8-8a16-5d6a18c890bb 8×1MiB 已实读，目的卷 b1b34698-c984-46e3-bcef-b73d2890a258 原 24 个对象 ID/版本确认。为覆盖冷装载，原生归槽任务 c19c2221-169a-4180-a75f-c6c9c4012248 一次成功，两盘物理地址 1004/1006 均归槽。已私有 staging 至 /opt/tape-rs/reclaim-bench-13d33e5a-8f03a63a40a7。复测进展看 v4/field-prep/{DEPLOYMENT.json,run-13d33e5a-5bec-43c9-ab23-db11eaa6b150}，有原 issue 时只观察。207/208/209 已运行同一候选 95eb7ca9abca1bbcc75deab99110953c1a240780e9f040741b133daf109ac4b6（1582160/3388330/2454218）。原 reclaim 1e2df2d4-ecc5-421a-a44e-01543a9f793f 一次完成，207/round652009，客户端371.95216081s/服务器370.688s。v3/378.614652094s→v4少6.662491284s。源MOVE完成至目标MOVE发令10.895999→1.867234s，源就绪退避6→0s。quick完成至任务成功28.287056→17.623602s，冗余标签LOCATE 2→0。目标仅提交Full gen7，301.136s取证为源重初始化前核验，完成一次quick。

- FINAL RESULT /root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v4/field-prep/RESULT.md。源空UUID 83fe28b0-7f9c-45c8-8022-aa8f0ab662e5、0/0，L85 UUID不变、32个对象/32MiB，无活动任务。旧24 ID/版、迁移8个ID均验证，32个真实LTFS读取及SHA256复核通过，后验306.203485244s单列。目标读回后的尾段v4 LOCATE累计32.735s vs v3 17.818s，观察复制间隔173.774s vs163.148s。目标历史本次初始24 vs旧16。未到3分钟，无门槛。三节点稳定运行新版本，专属SSH已全关。
- 合并冻结版5精确 FDB+Sim（含实际源MOVE丢回执、S4丢回执、目标READ错误）和19模拟历史通过。默认807、FDB库864通过；check/clippy退出0，同v3告警。live fmt109条基线格式差异，新增块已定向格式化且与冻结rustfmt等价。工作区全部旧 WIP 保留，无commit/push。FDB 私有端口44247无活动测试。新源带格式后读失败与目标MOVE丢回执未做实带故障注入；证据在 v4/combined-software/REPORT.md。


# 接续更新（2026-11-19 02:52 UTC，目标端 v3 受限实带完成，本段优先）

- 依次实现三批：新 group-only 归档根与首组同一 Full DP 索引世代，旧任务仍用原协议，根 coverage 与 Closed 同一 FDB 事务；准备 capture 同一 guard 少读一次 DP，Fresh mount/R5保留；组尾实际完整历史与当前 DP/IP/EOD 改用同一证明和原文SHA/manifest核对，省一次头读和第二份临时树。精确证据 /root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v3/field-prep/RESULT.md。
- 原任务 7adfdc6b-3050-4599-8443-1ccac81055ed 一次提交 succeeded，run d23dfff3-b644-4092-95f7-04d66ca6ccbe，B库/namespace/pool原样。L84 b126161f…迁8×1MiB至L85 b1b34698…，本轮目标初始16，24项实际GET+SHA256均通过，旧16的ID/版本不变。源只quick初始化一次，新空UUID **62de170a-ecf3-43f8-8a16-5d6a18c890bb**，目标UUID保持、源0和目标24、两盘taskNone。后验211.739秒独立。
- 控制端378.614652094秒，server378.554秒，较旧540.166700875少161.552049秒/29.9%。首见copying127.513769、reformat290.661609、copystage163.147840、reformatstage87.953043。owner207/R612149稳定。目标gen5→仅gen6 Full提交，CDB复制近似窗口target ordinary31/EOD12（旧60/32），源组30.871s。条件不同，无3分钟门槛且未到3分钟。300s诊断实际301.388触发、302.10–302.56采齐3节点无截断日志。最终journal、终态JSON、24返回body证明在v3/field-prep。
- main/4e8cc0c全部旧WIP保留，冻结源 /work/tmp/reclaim-small-benchmark-20261117/optimization-v3-final-source-v1，1811files manifest72d8baa12a69ac03f66fb2ddaccdd5fae4e5da86a1813f1304fb866ba652667f。实际三台服务均active，MainPID 2071534311/2083336251/2092411770，ltfsd fee1e7256532edce1ce6fc1462288bdc105f684bb9dcf51e6b0dca27ca47c51b，路径 /opt/tape-rs/reclaim-bench-d23dfff3-72d8baa12a69/bin/ltfsd，包b06f5640…。原benchmark单位drop-in 80-benchmark-be7cd08b.conf仅替换ExecStart路径。旧单位mask、私有命名空间与原配置保留。FINAL-HEALTH与SSH-CLOSED已保存，专属SSH全关，不复用旧socket。
- 独立冻结版私有FDB 8/8、Sim19/19、默认807/FDB库864、check/clippy exit0、git diff check0/CodeGraph及时；fmt全树有历史差异，Clippy增4可定位项。两个去重Sim各3→2 DP头读/8→7 LOCATE/3→2 EOD。独立S4实际写完回执丢失零重写，真实Sim READ故障保留源的pin。Close4事务自身独立commit unknown与混部没有实际注入。44247独占租用归还，无活跃测试；现场仅受限小数据，非大数据/生产验收。

# 接续更新（2026-11-19 02:40 UTC，目标端优化 v3 实测进行中）

- 用户要求按顺序优化目标端。main 4e8cc0c 保留全部前序 WIP。v3 三批已合入，冻结源和证据入口 /root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v3/FINAL-FREEZE.json，1811 个文件，SHA 72d8baa12a69ac03f66fb2ddaccdd5fae4e5da86a1813f1304fb866ba652667f。首批新 group-only 原父意图记录版本，旧任务原样解码，根与首组同代，根覆盖与关闭同一 FDB 事务。第二批去准备捕获内重复 DP 读，Fresh mount继续核验。第三批目标读回后完整重读历史并比较规范原文SHA及范围，省重复 DP 读和第二份目录重放。后两批各少一次 DP READ/普通定位/EOD 是 Sim 指令测量。
- 合并版独立 private FDB READ 保源、新单代、S4接管零重写、目录后继均通过。完整默认 cargo test、FDB lib、19 项 Sim、check 和 clippy 退出0。fmt 仍有历史差异。其它独立回归继续运行，44247 lease 正在 target_reclaim_verify。Close事务独立丢ACK和混部无真实注入。
- 真正207–209已逐台部署普通EL9候选，ltfsd fee1e7256532edce1ce6fc1462288bdc105f684bb9dcf51e6b0dca27ca47c51b，release tar b06f5640…。实际MainPID 207=1534311/208=3336251/209=2411770，field-prep/DEPLOYMENT.json。旧业务服务仍 mask，bench单位路由、白名单、原存储保持，仅 runtime drop-in 的二进制路径替换。
- 原B库55L3A78011W9LL01、namespacecf42cc01-431d-425e-bc37-a9511e33e178、pool15a7d418…，L00284L8源 b126161f… 新8×1MiB，L00285L8目标 b1b34698…已有16。源8个已真实GET/hash，16旧身份锚定。新 run d23dfff3-b644-4092-95f7-04d66ca6ccbe，已仅一次提交任务 7adfdc6b-3050-4599-8443-1ccac81055ed，CLI回执、原request在v3/field-prep/run子目录。observer与300s自动只读诊断运行，不得重送、重seed、擦除其余卷、重启正在执行的原任务。
- SSH仅 /run/trb-v3-d23dfff3/{207,208,209}.sock 本轮自有，PTY16247/22046/64817；observer19643和watchdog43860，取真实终态后保存无行数上限journals并postverify 全部24 GET；最终关闭自有SSH，台上服务照常。前代540.1667秒仅对照，本次目标初始16不同，等真终态报性能。

# 接续更新（2026-11-18 11:10 UTC；v2 小数据优化与真实带库验收完成，本段优先）

- 已实现源Full>=3 managed group-only按FDB封存extent顺读、读级DP/IP/EOD A/B边界核验+逐file SHA/必要MD5、全部通过才返回proof/prepare；strict PR-UA仅新源组无重试；原Load未issue/no pending/loaded条件下current02/04/01原轮最多30s等待。原source_session None禁跨tick、source全库存/初始化、default/1/2/incremental/已admitted恢复和publication→readback→retire不变；无schema/设备timeout/栈/SQLite业务镜像变化。
- **真实新任务3f768dee-0024-48ed-9e21-6b0fde6db4a0一次成功**，runbe7cd08b-be94-4cb5-9db3-a12562df5b7b，namespacecf42…/B库55L3A78011W9LL01/pool15a7d418…，L84原320496c2…新8×1MiB→L85b1b34698…旧8保留。控制端单调 **540.166700875s**，server538.077s；准备111.069611、copying339.027190、reformat90.069900。源init1次新UUID **b126161f-fafa-411c-a3a6-6f746ab5fec1**，末态source0/0、target16/16MiB、两盘taskNone，owner1/R94564未变；后验**16次真实GET+长度+SHA全pass**，旧文件身份保留，149.6887s单列。RESULT.md及BENCHMARK_RESULT/TERMINAL原件在/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v2/field-retest-v1与run子目录。
- 与前次633.978654075s比较少93.811953s/14.80%，copying454.334→339.027；同口径copy窗口source LOCATE68→31，target80→92。新源组完整A/body/B **31.247s**，8正文首末完成间隔1.780s(旧152.301仅同间隔定义)，源A/B各3ordinary+2EOD/body1，实际field marker与原group/drive可证。目标非空8、历史/初始机械不同，不把全部总差归因代码；总时间仍约9min，未达原3min方向，本轮无性能硬门槛。剩余大头target重复元数据/索引核验，归档根合并需独立Intent/HA版本方案，未实现。
- 301.951s观察器发300s诊断，root实际采集365.29s且journal12000行上限，先前『静默』推断无效已向用户/报告更正。最终terminal-journals不设行数限，20726324行，所有局部/全阶段分析据完整日志；没有kill或重发任务。SOURCE-GROUP-FIELD-REVIEW.md/FINAL-PERFORMANCE-ANALYSIS.md/RESULT.md可复核。
- 实测源冻结**c439f589056dd2b8f6989c2641ee76135e56ecc29da5f7c7817f4346d3113548**1811file、tar f4ce350b…，EL9包4c16779d…，实际三机ltfsd **49a616aaf2ead863184fdfcd4695312710a487c27f560094e5aa6164fbcfd94e**、ctl183e6235…/SDK1be5…。新版三unit保持active PID207923842/2082655443/2091855900，实际exe `/opt/tape-rs/reclaim-bench-be7cd08b-c439f589056d/bin/ltfsd`。只以原benchmark unit的任务专属runtime dropin替换ExecStart bin，原namespace/datadir/SDK/权限/flags不变；最终process/maps见DEPLOYMENT.json与final-health。
- 独立软件：19相关exact PASS，FDBlib863pass/251ignored，完整默认cargo test含integration/doc0，checkalltargets0、clippyalltargetallfeatures0；原raw-body完整15在候选7.64s/旧40b7 7.69s均同目录索引Incomplete失败，根因未定、不算PASS。它此前MOVE丢回执不重发断言已执行。初SDKlink环境失败保留且非产品RED。fmt全树历史差异；最后仅main reclaim_copy_groups.rs新增长行拆行，FORMAT-ONLY.diff/json独立normalize相等，c439冻结/现场未改，不能冒称后格式SHA实测。clippy告警未全归因。
- 最终gate **FIELD-GATE-VERDICT.md签限定managed Full-head group-only小数据的软件与现场PASS**，非全生产/全故障/大数据PASS；FINALSTATIC/CROSS/DESIGN均有记录。所有真实证据在/root/jay/cicd_local。工作区main/4e8cc0c所有前序WIP保留，无commit/reset；本批FINAL-AB.diff14path加FORMAT-ONLY唯一换行，git diff --check0/CodeGraph662files uptodate。
- **root本轮专属SSH masters已全部关闭**，control退出命令0，PTY等待255(显式control退出后)，三个socket均实查absent且/run/trb-v2-be7cd08b已删，SSH-CLOSED.json记录。不能复用旧socket/会话。原旧业务unit mask与维护nft watcher保持，旧原e429…非终态/旧namespace archived不盲恢复，不做NoBusiness release或旧reset重放。私有44247 server为既有资源，不停止；runner已结束所有测试，归还lease后可新分配，先核同期身份。

# 接续更新（2026-11-18 10:17 UTC；v2 发布包已暂存，软件验收继续）

- EL9 v2包 B/optimized-field-release-v2/optimized-reclaim-field-c439f589056d-el9-x86_64.tar.gz SHA **4c16779dde05886bbde8cffc96c8c9d56e01c2897b639bc4a054225082f392b1**；ltfsd **49a616aaf2ead863184fdfcd4695312710a487c27f560094e5aa6164fbcfd94e**，ctl **183e6235fee03aa7e70745d87850f68583e1451a0f35ab1b9844875c21b19ba6**，tape-rs4289e370…，build239.450s、warnings61与v1文本同multiset、1811seal c439…前后不变。完整manifest本地optimized-field-release-v2/BUILD-MANIFEST.json SHA39504a81…。
- 根已实际上传/解包至207–209 `/opt/tape-rs/reclaim-bench-be7cd08b-c439f589056d`。三台实际loader及3help、group-only帮助全通过，无GPFS链接；远端安装记录 `/root/jay/reclaim-small-benchmark-20261117/optimization-v2-c439f589056d/STAGED.json`，根证据optimization-v2/field-retest-v1/stage-candidate。**仅staging，三机runtime依旧旧d75874eb… PID818069/2535574/1756202，未切服务/未发新reclaim。**
- root已准备仅替换ExecStart二进制路径的本地deployment-config/{207,208,209}.candidate-dropin.conf及PLAN.json；尚未写远端dropin。原unit片段/完整多行DeviceAllow已实捕 node-preflight/{node}.original-unit.service +unit-capture.stdout，207/208允许sg5/6/4、209允许sg7/6/4。不能用早先简化dict的DeviceAllow单值当完整白名单。后续软件gate通过才逐个滚动更新；原unit/nft/namespace/data dir/SDK/flags不改。专属SSH sockets仍活跃/run/trb-v2-be7cd08b，完成后关闭自有master。
- 独立runner初build缺-lfdb_c搜索路径exit101，首01case也环境link失败（无测试）。修正SDK链接环境后已有效build；后续改直接冻结ELF `/work/tmp/reclaim-small-benchmark-20261117/optimization-v2-ab-target-v1/debug/deps/tape_rs-6b20314001029640`，SHA由runner最终记录。01retry、02三file、03坏SHA、04尾label、05尾layout、06PR失权、07PR-UA、08Load预算、09Load许可、10strictguardpure、11holderpure均实际exact1pass。目录independent-run-v1，已见同期server前后与test maps/cmdline/stat；要求补listener socket inode（不能把cluster_inode替代）及完整argv/env来源，gate原始证据待审。
- generic lib test863pass/251ignored、check0、clippy0，fmt1历史差异；实际argv范围尚待runner明确，根已要求标准完整cargo test含集成/doc、check all-targets、clippy all-targets all-features。本批旧unknown/knownfull/twofile/2directory/actualMOVE回归、B3pure还未见独立日志，已明确要求runner补齐，不可因新11case通过就切服务。runner44247独占lease仍有效。
- FINAL-STATIC-GATE.md绑定c4391811seal无新源码阻断但证据不足；FINAL-CROSS-MODULE-REVIEW及FINAL-DESIGN-REVIEW已独立explorer/architect通过静态/设计边界。最终质量结论须runner完成后由gate确认。没有新实带reclaim计时结果，源新8+目标旧8已SEED_VERIFIED状态保持。

# 接续更新（2026-11-18 09:52 UTC；v2 源码已冻结，独立验证/EL9构建中）

- 最终A+B冻结 `/work/tmp/reclaim-small-benchmark-20261117/optimization-v2-ab-source-v3`，1811file，manifest optimization-v2/FINAL-SOURCE-MANIFEST.json rawSHA **c439f589056dd2b8f6989c2641ee76135e56ecc29da5f7c7817f4346d3113548**，tar sibling optimization-v2-ab-source-v3-inputs.tar.gz SHA **f4ce350b3c65c28b29a53019254e2269989829947bdfcf7af6a0ab0427a4e14c**。根逐1811重核一致；FINAL-AB.diff为初始BASELINE-SOURCE锚定14path真实本批diff，非HEAD全WIP。coder停写/归还44247。禁止再改冻结版；若修复须新freeze并重验证。
- 最终A是Full已封存组>=3的read级DP/IP/hash/EOD/活动前驱 A/B包围顺序源正文，**无额外旧Fullhistory审计**；原组外完整source inventory与format资格不变。严格Guard还覆盖raw PR IN的UA：新不重试holder check仅sourcegroup用，原caller保持原路径。最终code4file selftest150.74s、TailLayout故障24.01s、strictPR-UA真实23.51s/0child0write及pure通过；这些尚非独立PASS，更非实带时长。full端点中间对照153.67s不部署。源3+1+3 ordinaryLOCATE/A-body-B，body零分区切换；wholecopytick含外层共16ordinary+8EOD，需现场比较。
- 独立 `/root/reclaim_v2_runner` 已正式拥有私有44247唯一lease，按VALIDATION-PLAN从冻结版独立build精确tests+通用cargo，输出optimization-v2/independent-run-v1，运行同期必须捕serverPID/boot/start/listener/SDK。A清单A-EXACT-CASES.json包含PR-UA真实case和pure严格PR。当前尚无gate终判。
- `/root/physical_candidate_build` 已按同seal正式开始EL9普通release featuresfdb,fuse三bins，独立target，输出B/optimized-field-release-v2。不连接SDK服务/现场；需软件gate后root部署，不能把编译/本地loader冒称实际EL9现场通过。
- 现场新8源已SEED_VERIFIED，run be7cd08b-be94-4cb5-9db3-a12562df5b7b，L84当前8files、L85旧8files；没有新reclaim原request、没有新部署。SSH自有`/run/trb-v2-be7cd08b/{207,208,209}.sock`活跃PTY30150/32661/56272，三台仍旧d75874eb…；旧unit/nft保留。后续按冻结包滚动部署前重核无活动写和新旧身份，完整三机deployment record再harness提交一次。300秒诊断不kill，目标非空差异需报告。
- 用户新建议源顺读→目标顺写→index→两端顺读；已说明采用源首读同步SHA、常规末尾目标顺读，保留逐file回执/检查点。用户明确“具体方案最合理你来把控”。现协议S4后FDB **active业务head**已切目标，再目标读回，源旧copy/pin保留到settle，不能称GET自动fallback或所有publication晚于readback；本批顺序不改。归档根合并需单独Intent版本/覆盖/HA设计，SOURCE-TARGET-SEQUENTIAL-PLAN.md已给最小后续设计，尚未实现。是否下一批依据新实带余下瓶颈，勿直接删root节点事务。

# 接续更新（2026-11-18 09:31 UTC；v2 实带数据已准备，候选未部署）

- 根已实际运行新 harness seed+verify-seed，run **be7cd08b-be94-4cb5-9db3-a12562df5b7b**，脚本SHA742dd10c…，目录 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v2/field-retest-v1/run-be7cd08b-be94-4cb5-9db3-a12562df5b7b`。8原上传request/receipt全部齐，8个新source实带GET与SHA通过，SEED_VERIFIED.json已落盘；旧L85八身份保持。现在L84 UUID320496c2…有新8files8MiB，L85 UUIDb1b34698…旧8files8MiB，pool共16逻辑files。**没有新reclaim请求/部署/格式化**；不得再次seed，不复用旧成功task。下步新版软件验收后reclaim stage必须pin新ctl/release manifest/三机实际运行ELF deployment record，CLI原request先落盘；300s只诊断。root现有专属SSH masters `/run/trb-v2-be7cd08b/{207,208,209}.sock`，PTY session30150/32661/56272（2026-11-18本轮新建，先前长socket路径失败连接已退出）；node-preflight证三台实际旧ELF d75874eb… active/PID818069/2535574/1756202，未部署。后续操作前重新核身份，完成后关闭这三条自有连接。
- A真实RED已101.35s命中4file tokenNone actual source labelLOCATE5 vs期望3，准确计数失败不是setup。源码source-red留TEST-INPUT与日志，原路径ELF后被build覆盖，不能声称旧ELF尚可直接运行。coder当前fullcapture A/B已编译并GREEN跑中，但性能复核source-cost-review.md证明fullcapture每次历史H+1 LOCATE，单标签下降不证明总收益。根已叫停部署，architect正在裁决改为read级verify_media_head/前驱A+B包围实际SHA正文，保留组外driver全量inventory及format前完整检查，严禁新sourcegroup证据直接授权初始化。未有最终A软件PASS。
- B源码已冻结not-ready-v1，仅mechanics生产修改，30s总退避4额外观察；3pure实际GREEN，2实际executeSDK新case仅编译/list未跑，gate初审无阻断但证据不足。gate已把原真实MOVE回执丢失机械actor纳回归计划。B coder结束，DB44247仍A独占，runner待明确移交。

# 接续更新（2026-11-18 09:04 UTC；性能优化 v2 实施中，本段优先）

- 用户“按照你的分析进行优化 / continue”。main/4e8cc0c 全部前序WIP保留；本批基线证据 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-v2/BASELINE-SOURCE.json`，计划 PLAN.md，独立验证计划 VALIDATION-PLAN.md。尚无本批软件PASS或新实带结果。
- 分析 physical_reclaim_reserved / reclaim_timings 已完成文档 source-group-session-analysis.md、mechanics-root-analysis.md。architect reclaim_v2_architect 已确认：>=3个已封存SHA/完整FDB layout的 group-only，在同原guard/transport fresh完整A→顺序正文pending→fresh完整B及最终FDB验证成功后才prepare/admit；None跨tick source_session缓存禁用不变，不宣称能排除同initiator介质ABA，所读字节以预先可信SHA/版本保证。
- coder reclaim_v2_coder 唯一负责源组A（fdb_read/新group reader/executor_group_copy等），独占私有44247 SDK；physical_candidate_build coder独立负责B（fdb_reclaim_mechanics/executor_fdb_reclaim_driver及同文件tests），禁止其并用SDK，必要时根串行交接。B严格未issue无pending且原位置匹配current02/04/01有界等待，已issued/unknown保留原observe。gate reclaim_v2_gate 已确认计划，等待冻结后审查+独立runner。设备timeout/默认栈不改。
- 目标归档根合并涉及Intent3/完整树/根coverage与HA兼容，architect将其列后续独立版本化批，本批先做前两项。不能直接删node(root)或伪目录回执，也不能把省一次目标提交计入本批收益。
- 本批现场只读preflight确认三节点同owner3/R18624 fenced ready；旧原reclaim ec2e6c05…仍succeeded，L84新UUID320496c2…0files，L85 b1b34698…8files8MiB，完整list8。证据optimization-v2/field-readonly-preflight。无新部署/写带/初始化，本轮尚未建立SSH master。旧units mask/nft保护不撤。
- 私有FDB44247已由根本轮核验PID1878008/start50473440/boot b52d0d6f…/clusterSHAfff02be8…/serverSHA1ddd719d…，证据PRIVATE-FDB-PREFLIGHT.json；coder归还后须独立runner同期捕获身份再执行，无真实45151/52/53权限。

# 接续更新（2026-11-18 08:30 UTC；新实带基准成功完成，本段优先）

- 新实际原任务 ec2e6c05-a5fc-470f-8567-82b926ea4778（epoch1，group-only，pool15a7d418…）一次提交成功。8×1MiB seed 原8 upload/前实读校验齐，旧脚本因 tape list counters null失败后仅复用原receipt做只读校验；无重复上传/原reclaim重发。单调时钟ISSUED→首见succeeded **633.978654075s**（server created→complete632.839s）。当前结果见 /root/jay/cicd_local/reclaim-small-benchmark-20261117/field-start-20261118.v2Gvay/RESULT.md 及 seed-reclaim-run-v1/BENCHMARK_RESULT.json。
- 原任务实际诊断触发301.611s；精确首见reserved25.379s、copying91.797s、reformatting546.131s、succeeded633.977s。初期真实目标TUR 02/04/01→GOOD且换轮2028→18439→18466→18624，原任务自行继续；无逐TUR原报错映射。209重复源验证分区往返，f00到f07完成约152.3s，目标八文件P1连续block10…24首末写完3.33s，目标组读回首末4.51s。软件/物理指标不可混算。本轮仅基准，无性能通过阈值。
- 终态真实8files/8388608B，源初始化1次新UUID320496c2-d7bd-4326-97a0-b6f299422c49，回执0files/paths、实时统计0files/bytes，两个带task_id None。目标保持b1b34698…，8files/8MiB。postverify使用三节点一致且fenced/ready的真实末owner3/R18624，8个目标实带GET+SHA256全部通过（82.180s单列）。新版owner三机继续运行，旧unit依旧mask及旧状态保留；nft watcher未撤。

# 接续更新（2026-11-18 07:58 UTC；现场准备过程）

- 用户明确「启动真实环境的测试」。本轮已在 207–209 真实执行，并非旧 task 重放。证据根 /root/jay/cicd_local/reclaim-small-benchmark-20261117/field-start-20261118.v2Gvay；新 run abd9d909-e370-4f42-be5a-089904348b30、新 namespace cf42cc01-431d-425e-bc37-a9511e33e178、B pool 15a7d418-99ea-470c-9d7c-ac0fabb9b899。准备及 benchmark 无性能门槛；reclaim 从原提交开始计时，300秒未终态做只读诊断，不自动停止。
- 优化冻结源码40b7fb96735d…、EL9正式 ltfsd d75874eb…/ltfsctl3442df42…/tarb0fa9227…；三台实际EL9加载与group-only CLI通过。目录独立7SDK+2codec均实际一通过，详 independent-directory-run-v4/GATE-VERDICT.md（该证据目录缺运行同期服务端捕获；正确报告范围）。
- 旧207–209 service 通过专属持久 generator 有效 mask，实际片段来自 generator.early，原 /etc 单元文件身份及内容相等。旧B+gateway在临时 DevicePolicy=closed 的 ExistingRoute 三副本实际多数派；current/audit/barrier 503为真实 gateway Rebalance。三者正常停，官方离线 replay exact旧前缀+term3 commit73161证 B/gateway 最终None、4个过渡轮未fence、2个新round Abandoned；控制期间三机全部B/alias队列计数不变。没有伪造旧round字面Abandoned或旧 task终态。旧app同RV导出437row/222377B，四子上传原P回执AlreadyApplied，最新RZ指向committed，欠后续验证。旧状态树原路径保持，取证3份原件/完整旧目录hash保留。
- changer VPD88已实际证明 target605d→RTPI2。旧207原FC同nexus、主动按各LU一次原意图、CLI `pr-release`，两轮独立完整空表，三机随后PR IN也全部为空。该动作是计划RELEASE/注销，未发CLEAR/PREEMPT。三个旧keepers mask无活动。
- 真正测试45153 app22 reset2be96c63-491a-4da2-97fa-420cc9839cea 单次事务 APPLIED，独立同RV严格核旧V1/新HEAD/新receipt/空app范围，保45151/52。每台新root `/mnt/meta-data/tape-rs/v1-20261106/state.benchmark-abd9d909-e370-4f42-be5a-089904348b30`；新版无设备控制启动、quiesce590/freeze947、3同challenge屏障200、CLI fdb init成功initial948。再停无设备controls，实际 `tape-rs-reclaim-bench-abd9d909-owner-{1,2,3}.service` 只有各机三个正确B设备白名单Startup，owner node3=209/round2028真实fenced且ready。
- 真实 rescan succeeded，L84/L85在605B/606F原address1/2，用户许可的两盘 serial assign `--yes --force-format` 都 succeeded 与native读取空files/paths，source d35a4c50-6f20-4712-a113-aa1afeb76317 rev1，target b1b34698-c984-46e3-bcef-b73d2890a258 rev1。只此池两卷且0 objects/bytes/list。即将seed八个SHA固定文件、upstream readback、发一次 `--group-only`。在未见本段后续真实操作已提交证据前，不能报reclaim已开始或跑完。旧/新维护网络nft watcher仍保留，不调用历史cleanup。实际硬件、DB有状态动作以各原唯一receipt为准。

# 接续更新（2026-11-18 06:17 UTC；本段优先）

- 用户最后问旧 reclaim 可否停止、候选 ready 后直接实带复测。已答旧测试已停止、原非终态任务与检查点保留，继续新候选软件验证后复测。旧业务三节点 inactive/dead、PID/Job/cgroup 空；未把停止标为任务成功。原 op e42985d8-13ec-42ef-bae3-7acc23786fb4、namespace f37a34e5-415d-4302-9bd1-f5132ff732c3 不得盲重放。新实带 benchmark 尚未启动。
- 真实只读证据 E=/root/jay/cicd_local/reclaim-small-benchmark-20261117/readonly-branch-20261118T043542Z-9e93f468。READONLY-AUDIT.json：三机原5状态文件 identity/SHA 未变、SG/tape FD 扫描无占用、队列 drained；旧 unit 仅 disabled **尚未有效 masked**。此 FD 扫描不含 bsg，不宣称完整排他。三轮 SSH 自有连接均已关闭，最新 SSH-CLOSED-v3.json；不复用旧 socket。
- strict native capture 已在三机真实 paths 执行，均因 changer VPD83 缺 target-port/RTPI designator 返回 Unsupported，未到 PR，不制造授权。独立 sg_persist 只读诊断在207执行18次 PR IN，两轮三 LU 都是唯一 4c01000000008cab/EA type3，FC initiator100070b7e4262f06、RTPI2/2/1、gen1d292/1c7/257 稳定。原始同次 ioctl CDB/response/resid/status 全在 E/207-raw；**无 PR OUT、无带读写/定位/格式化**，尚无 changer 授权级端口映射。
- E/route-current 实际 FIELD45153 三点只读事务成功 exit0/ROUTE_OBSERVED，RV1018320249762、routeReadSHA be87293ba305c5c32a84e6583e5ec191f013951928fbce04e6386bc87e23a916。现 owner36011、route rev1/Frozen2745/initial2746、原 N1 cert2f27dbcf…db7 对上；activation_authorized=false。固定 v3 generator 及 adapter 已独立 gate 软件 PASS，原可变依赖问题已隔离到 freeze-v3；尚无真实 control clone 启动/Abandoned/新 reset/init。
- B v13 相同实际 ELF b3db6f22… 的2pure+5SDK全部 PASS（坏MAM58.610s、Fresh public2 101.086s、KnownFull恢复148.065s、groupActualNone55.242s、rootMetadataWriting8.68s）。Unknown三阶段 taskCDB0、原行/冷读保护；KnownFull MAM先读及原组闭合验证真实通过。默认栈溢出已独立定位为有限大 future frame，拆 Fresh/Observe 后 execute frame287720→50680，未增栈/超时。B最终格式源另冻，不能把旧 ELF 运行冒称新格式 SHA。
- 当前 main coder /root/reclaim_batch_implementation 唯一持有 PRIVATE44247；正在修目录后继缺陷。真实 RED 118.209s/default stack：原生 mkdir uid4/gen4，公开 group-only/S4/7→8 到 Ready(root+z-empty)、group Closed/0x85 Observed、delta2、Rq目录缺；第二步被原 Fresh 谓词误拒。精确 Ready 非root Directory 的 metadata successor 谓词放行后，实际首 GREEN 223.954s/default stack/ELF7dc1976c…：delta3、terminal/source format、两archive冷正文2MiB通过。**这只是模拟设备软件测试，不是新实带时长。**
- 目录第二真实 RED 同ELF109.794s/101：原Closed后目录Writing回执丢失，分类前直接reaper得到Ok(0)，rejected=false，证明normalMutation早return错误准入，**不声称已删除数据**。故障ns fb5ac759…与原日志保留。main正仅把require_finished_at的return改为await?后继续原live metadata检查，随后最终ELF跑目录健康+Writing/12负例/5mutator/7/7/8/cold两个case再冻结。目录gate已先确认计划，最终需独立同ELF旧5SDK回归；尚未最终PASS。允许旧 protected_rows 仅 pub(super) 复用，无正文/断言变动。
- 后续9a2282fe…新ELF/input8f0852be… build77.50s0；fault实际12负例、分类前5mutators、54rows不变/0CDB通过，但旧通用fixture期望source copies=2失败，**不是产品RED/不是whole-case PASS**。原groupClosed后source files已正常退休，此阶段copies=0；批准test-only提取core(expected_source_copies)，旧7case原wrapper固定2语义不变，新目录wrapper0，并先证明Writing前已0、完整media/claim/cold保护保留。需避免大future双层内联栈倒退。gate已预审8f三生产无已证新阻断，但最终仍证据不足；要求同Tr便宜补已有合法coverage/object xattrs/错误volume/inventory，notClosed旧负例只命中codec不夸phase证明。main继续新snapshot两case并最终freeze，SDK44247仍main独占，尚未移交runner。
- O=/root/jay/cicd_local/reclaim-small-benchmark-20261117/optimization-batch-v1。SMALL-BENCHMARK-BOOTSTRAP-IMPLEMENTATION-v1.md、SMALL-BENCHMARK-BOOTSTRAP-C1-READONLY-v1.md 是设计材料，**尚未实现、未分派新bootstrap coder**；本批先收敛性能/目录修复，不扩通用来源或恢复工厂。C1建议独立只读核验工具，不发init/PR/owner许可，固定root安装manifest还未安装；是否需要独立工具应与既有reset只读检查复用后再判断。后续不放弃正式 Takeover→Fenced→Recover→claim。原 BUSINESS_ISSUED 维护保护持续，旧非终态禁止 NoBusiness release/rearm。
- 当前工作树仍 main/4e8cc0c，全部已有WIP保留。根会话仅编排/证据/交接，生产源码由 coder；分析与改码不同agent。真实8×1MiB仅benchmark、无性能门槛，>300秒诊断，软件600秒cap/默认栈/设备超时不改。清业务FDB/SQLite与重初始化L84/L85已有用户授权，不重复询问。

# 接续更新（2026-11-18 04:22 UTC；本段优先）

- B v6两个真实matrix已PASS：rootMetadataWriting无组8.338s、groupActualNone52.267s，3阶段7/7/8 taskCDB0，protected18/40rows同bytes、每阶段5mutators拒不改、0x85 subtype/RecoverFDBbarrier0IO、cachelost及source冷读2MiB均实跑。v5 KnownFull147.15s已PASS。**v8新增6prodguards+2test后坏VCI exact40.55s/-6 default stackoverflow，尚未到坏VCI predicate，不能继承v5/v6PASS**。源码/ELF/raw保留、未加栈/超时。
- maincoder已停止写与DB，SDK44247归root临时持有无人run。`reclaim_b_stack_explorer`只读窄调查：O/group-only-B-stack-v8-v1/MANIFEST,V6-V8.diff,V6-INPUT,V8-INPUT；v8src W/group-only-B-core-source-v8/ELF W/group-only-B-core-v8-artifacts/tape_rs-lib-tests68d992…9943/tar99881d17…138a，v6src ...source-v6/ELF3a824e6d…4e8ce。L/group-only-B-bad-mam-runtime-v1 stderr/START/MAPS/result与身份齐；core_pattern apport/无core。先排新增guard→metadata discovery/claimed_task_at回环(仅待证假设)，再pollframe/testbranch；不能盲Box掩盖无限递归。root派独立分析，main不得先改Box。
- control generator桥最终冻：B/control-existingroute-tools-v1/evidence/bridge-v1 tar6ff50c8b…ac4127/patcha9a0ff86…fa805/manifest4dd74a4d…8e695，1801inputs/ELFcad6293…1601e71/3正14负+1既有审计PASS；Python236原封。gate静态test-only5paths通过，runner`reclaim_diag_validation`已获独立执行计划，新source/newtarget/Python31+5CLI/bridge exact3+14和旧fixture1，禁DB/boot/remoteSG。记录Raft0WAL+32768SHM允许控制侧副作用，Directory0；真实SDK/fieldAbandoned未验。
- facts3 semantic首批四file冻：/work/tmp/facts3-semantic-integration-IcF9TT8D/IMPLEMENTATION-SELFTEST.md，tar dfa359793470da9ba738953eb0a110fbd554ab0e8409d11f85410866e92afbe9/deltac97389ff2998f82b9492a2a4af1d1424060f5fbef45aee000530c1926befb7df，58collected57PASS1optionalSKIP，4pycompile0，actual6离线childtrace noNET/设备/SDK/意外写+输入前后同。旧同metadata FD内容漂移preeditRED已修FD分块字节recheck。无mint、PRIVATE/FIELD UNREADY：真实Sim FULLSTATUS/VPD/private stop/maintenance/rootrelease/SDK exporterOwner/current gateway custody缺，未独立gate/runner。coder已结束不再写。
- architect正做`SMALL-BENCHMARK-MINIMAL-PATH-AUDIT-v1.md`，初判普通clear→原init不可直接走（init新challenge在clear后才生成，ControlOnly AbsentAfterReset拒）；普通fence会截断PR表且可preempt未证明foreign，不能跳strict安全。正在收敛到实际必要最小bootstrap/首轮guard/持久异常停止，而非为bench先建全部恢复与历史key算法；未结论/无现场。
- 当前3child：stack_explorer只读/noDB；architect最小路径审计/noDB；runner控制桥/noDB。main暂停thread等分析(不是用户暂停目标)。真实旧test/服务保持停止，原任务仍非terminal，未新SSH/fieldFDB/PR/format/benchmark。

# 接续更新（2026-11-18 04:13 UTC；本段优先）

- B有效首GREEN：v5 source4a2d9e18…/bin9b4f95a9…，early-green-runtime-v2实际53.581s0，cachelost/repeattick owner7、同owner重建Actor、HAowner8均taskCDB0、原40protected rows不变、独立冷读源2MiB真实保；之前893→4CDB两次失败原raw保。v5正常public KnownFull实际147.152sPASS：原S4 issuer7→live8，同batch恢复publish/readback/settle/close/sourceformat terminal，targetwrites0/delta2/冷读断言。全部Sim/隔离FDB，非实带。
- 现main唯一SDKrun是v6根MetadataTask Writing未知+5mutator/Recoverbarrier测试，未出结局；其余catalog机械/metadata scope/claim/retry/checkpoint/discard5窄guards已批准保preedit落，尚未编译不继承v5结果。rootMetadata testhook只cfgtest精确parent+RAII，实际Writing提交后返回Err，不造持久状态/不改媒体算法。KnownFull MAM合同`GROUP-ONLY-B-KNOWNFULL-READONLY-CONTRACT-v1.md` SHAb5cab766…8795f：Actual/Header3先于S4，原ACK不证明；MAM-first VCR→两VCI→VCR，同guard原probe后纯比较，各VCI对应各自partition真实head，完整旧IP债保，缺/错Blocked不修写。
- control generator真实Rust桥run-v3 3正+14负全pass，原Python输出未修，实际N1/active73/Raft/Directory/FD/state构造，FakeKnownAuthority每node2读不是SDK。5mainFD metadata/SHA相同/Directory无sidecar；drop(seedwriter)后原线上READ_ONLY currentRaft reader仍实际创建0B WAL+32768B SHM，ownedfixture清单保留，不删/不判业务镜像。场前需完整一致view，不凭旧无WAL声明；这是controlSQLite侧不涉业务。Bridge正封testonlyoverlay/全source，当前未独立gate/runner。
- facts3_semantic_impl在最小软件接线/whole-tree/joins/负例末段，暂无root approvedjob/真实PRIVATEproducer，execute仍UNREADY。held source FD同inode/相同stat但bytes改动的旧facts3 pin漏洞preedit真实18case16PASS1FAIL1SKIP复现，已最窄FD分块内容recheck修并新基础7PASS；新实际helperfixture/invalid/PRplaninvalid/resetplan有rawtrace，finalfreeze尚未完成。root信任anchor选B/approved-semantic-jobs/root-owned0700只读安装，不/etc或编译SHA=None钉死、不改旧wire1/2/SDK/CAS/ledger。任何unimplemented source保持缺证，不能假设authority。
- 当前root+mainB(44247唯一)+controlgenerator桥封档+semanticimpl，max3子agent；其余完成可复用。用户真实旧service/任务保持停止/未终态，本轮无任何newSSH/DBfield/SG/PR/format/实带benchmark。

# 接续更新（2026-11-18 03:40 UTC；本段优先）

- 当前无新现场操作；真实旧reclaim/服务停机状态保留，源任务未终态。active agents: mainB coder（44247唯一许可）/control_existingroute_generator（离线Python生成工具，禁实际DB/SSH）/native_successor_design（只读KnownFull MAM/IP接受合同）。strictPRcapture coder/runner/gate已全部结束且限定软件PASS。后续需接真实只读capture但不能把plan/invalidCLI追踪当合法field已验。
- 控制generator没有安全现成readonlyrouteCLI，fdb_empty_cutover例子有写不可用；允许它在新工具显式collect-route分支复用已封session/clusterpin只读精确B active/certificate/必要owner点查一Tx/RV（源expected namespacef37a…/fieldclusterSHA5e…/3×45153），本轮仅fakeSDKtests、不真正连接。默认plan不import/bootSDK或联网，不改原reset模块/keys/receipts。
- mainB询问KnownFull append_ok是否错误覆盖IP缺尾及原S4是否证明VCI/MAM；architect正CodeGraph核实际RecoveryReport/原checkpoint，root要求保合法旧代但tail完整IP债、truncated/missing明确Blocked，无新增MAM写/autoRepair、无猜字段。ActualNone group/rootMetadataTask早隔离仍由main先做，不被此核查阻塞。

# 接续更新（2026-11-18 03:36 UTC；本段优先）

- strictPRcapture已获独立限定软件PASS：`/work/tmp/facts3-pr-capture-Z29ZqXia/GATE-VERDICT-v1.md`；E=`facts3-pr-capture-independent-v1`，tar3fa70be718633e231003da24fc0b4d7c377d1eb4f526e52fa3d42a428c58c400/1802beforeafter一致/newbin3d5a2e3afc75475cfca55f0a92ee0909fa319c30eaa27be068b6fd171a24dd54。33scsi+2bin+11ordinarySim实PASS，4CLI defaultplan0/缺readonly2/FD2拒2/继承FD3但actorhost错2；fullargv/env/FD/inputbeforeafter/strace无SG/sys/net，未执行合法fieldcapture。作者finalcheck/clippy/4filefmt复用，全fmt110diff同WIPbase；真实kernel/firmware/v2reader/factory/HA未验。
- ExistingRoute设计`CONTROL-ONLY-EXISTINGROUTE-FIELD-PLAN-v1.md` SHA40be9754…e7a93a已完成。旧private-gpfs-exec.py已读原文/三机同SHA51d7b466…：仅私有mountns摘除外部GPFS挂载后exec绝对路径，无GPFS SDK/daemon/业务/LD_PRELOAD，数据state本地ZFS，保隔离不引入GPFS依赖。HTTP仅library executor，gateway Abandoned必须3机stop后officialcommitted replay证明。尚无field启动。
- 新coder`control_existingroute_generator`仅外部新`control-existingroute-tools-v1`Python软件实现binding+临时unit/runplan/defaultUNREADY，禁现场；需去原ownerwritable/S3flags，binding准确route revision/certSHA需真实45153 readonly点查。没有安全独立现成CLI（fdb_empty_cutover有写副作用禁用）；允许新explicit只读pointcollector复用封SDKsession/pin，fakeSDK测试，本轮不实际connect。原路由73B严按codec，native revision1不补默认。
- architect`native_successor_design`正只读裁定B KnownFull dp/ip.tail/合法IP旧代完整债与MAM/VCI原S4证据，只发最窄合同，不写修复。mainB仍源码实现/44247独占，尚无GREEN；所有其余agent禁DB。真实207–209原服务仍处旧停止状态，未清FDB/PR/格式化/新reclaim。

# 接续更新（2026-11-18 03:25 UTC；本段优先）

- B已取得有效产品RED62.053s/101：正常公开group-only原admit回执丢后ActualNone/NoHeader，cache删除/repeat advance phase0 owner7虽然Err但发893任务CDB（目标应0），40protected R/C/V/F/sourceproof行未变。仅此边界已实跑，sameowner/HA/coldsource后续尚未到；不是实带故障/不等同旧5095s全部根因。source21e5b817…/bin1756e376…，ns1a8994c3…保留，Wlogs group-only-B-early-red-runtime-v2 stdout/stderr164行/START/MAPS/result/身份齐。main正最小生产B接线，独占44247。
- ExistingRoute field plan仍architect设计中，发现旧ExecStart的unshare→python→private-gpfs-exec wrapper以及--fdb-owner-writable/--fdb-s3/S3listen flags。不能原样加controlOnly两个flags；须去业务flags并核GPFS wrapper实际用途，用户要求native组件无GPFS依赖，root已要求不无依据保legacyshim/LD_PRELOAD。本轮未取freshhost/未启任何unit；不因计划讨论视为现场部署。

# 接续更新（2026-11-18 03:23 UTC；本段优先）

- `facts3_native_gate`已最终限定软件component PASS，`/work/tmp/facts3-native-verifier/GATE-FINAL.md`：独立native补证`facts3-native-independent-v1/evidence-supplement-v1`两正例完整输入beforeafter76/76及request37/source17完全相同，5CLI actualargv/env/FD3FD4/requests/trace/0,0,2,3,4核实；先前只事后清单的缺口由新实际执行补证，旧raw保留。noWAL仅输入声明/claimedRV不是SDK来源、无resetgrant、现场/HA/整体factory未验；fullfmt旧差异/clippy未全归因保留。
- B首运行52.08s/101只因fixture range快照漏分页，虽已到原admit回执丢/ActualNone/rootdelta1，但尚未调用被测repeat advance，不计产品RED。main仅test bounded next_range修复/新ns复跑，生产未修，原ns/source/bin/raw保留。main仍44247唯一许可。计划`GROUP-ONLY-B-CODER-HANDOFF-v1.md`含group+rootMetadataTask subtype/Recover+perTxguard；前文A已限定闭合，B尚未GREEN。
- strictPRcapture4files软件33SCSI+2bin+Sim fence已自测PASS、4ownedfmt通过，alltarget/check/clippy/CLI trace仍进行；RO内核PRIN可能EPERM的条件保留errno、不自动RW回退，现场尚未取证。user授权B only旧服务停、未清FDB/PR/格式化，别把软件fixture当实带。
- architect当前仅设计`CONTROL-ONLY-EXISTINGROUTE-FIELD-PLAN-v1.md`：用已限定PASS和已EL9 loader验证的旧controlv2组件做B ExistingRoute→原链Abandoned→正常停止，0device，不是启动新reclaim；无generator/运行脚本/现场操作已执行，后续root仍要fresh边界再串行实际操作。原BUSINESS_ISSUED维护不能NoBusiness release/rearm。

# 接续更新（2026-11-18 03:06 UTC；本段优先）

- A收尾已获`GROUP-ONLY-A-FIX-GATE-VERDICT-v3.md`限定通过：7pure+7DB与活child SDKmaps由旧冻结 source964088a2…/newindepbinaryc5493b…实际执行；newformat-only source91e9bd6d…/tar3be8a1d4…仅2文件空白，整file normalized等同+newcheck0，没有newruntime，别借新SHA。原文件整fmt旧WIP差异仍存在。本批不涵盖B/CLI8/field。
- nativecomponent独立验证已完成，最终gate`facts3_native_gate`正复裁：`facts3-native-independent-v1/RESULT.md`，1800/1800beforeafter/newbin990fe4d97d1164aa5513fefad7052d134de26a76428cab36b565b2f2c7464853，build0/control22PASS3ignored/native3PASS2ignored/2offlineignored各1PASS、2CLI正/3negative 2/3/4stdout空、无libfdb NEEDED/fdb_*dynamic、trace无netSG。首fixture因预创建输出目录失败原raw留存，换全新目录pass，非产品RED。组件仍只有conditional source/noWAL声明/claimedRV，不授reset权限。
- 新strictPRcapture四文件coder`facts3_pr_capture_impl`正软件实现，contract`FACTS3-STRICT-PR-CAPTURE-IMPLEMENTATION-v1.md` SHAea8002f0…9dddc；device单原SGIO optionaltap signedresid+O_RDONLYopen，不改普通路径/timeout/UA/ABI；新FC VPD83 NAA子型避LUN8猜测。首隔离check0，rawfixtures/Sim/边界test仍在做；禁真实SG/SSH/DB。
- main`reclaim_batch_implementation`正B Unknown gate实现，privateprefix0x85，已授权managed_catalog reclaim pub(crate) reexport最小hunk，保nativehelper重导出。main当前44247唯一许可；全体其他agent禁DB。现场仍没有新的启动/清库/PR变更/格式化/benchmark。

# 接续更新（2026-11-18 02:51 UTC；本段优先）

- A最终functional限定PASS：7pure+7DB，5短第二次supplement优先活maps/cmdline后hash，均5段实际libfdb映射+exactargv/ELF；前一轮空maps真实原因是先hash大型debugELF，采样时已zombie，绝不是未用SDK。旧raw保留，错误解释已纠正。gate v2仍要求新增hunk最窄fmt。main已完成`group-only-A-format-only-v1`：仅2文件空白delta、old/new整file rustfmtstdout完全相同，newcheck8.807s0；newsource91e9bd6d…0bcde2/newtar3be8a1d4…985a2无runtime，不把旧14PASS归新SHA。待gate短复裁。
- main开始B持久早隔离，尚无已验收B结果；新privateprefix协调为0x85（0x83资格，0x84预留native successor）。main已获44247唯一selftest许可，其他agent禁DB。
- facts3纯native组件9file已冻1800inputs，tar1afed6…77f41b/ELF84dc72…9895；selftests22control(3defaultignored中2offline另pass)、3nativecodec(2liveignored)、真实历史15与codecfixture无网络/设备/输入变化，finalcheck48.37s0/clippy51.93s0/ownedfmt0/fullfmt历史WIP1。gate静态无已证阻断/计划齐，runner`reclaim_diag_validation`在新source/target独立软件验证（禁DB/boot/SG）；`no_wal`和claimedRV只是输入声明，不等于可信来源，不发resetgrant。
- Python只读collector3新增file已冻`retest-tools-v2/evidence/facts3-readonly-collectors-v1`，24fake测试/pycompile/defaultplan通过且无网络，FIELD连接关闭/private原SDK未运行，whole-tree/PR/外部writer/semantic authority仍未覆盖。资料3 execute仍UNREADY。
- architect`native_successor_design`正写最小strictSG只读capture实现计划，禁止现场、PRout、清PR。真实旧服务/原任务/PR/两盘仍保持旧停止状态，新真实benchmark尚未启动。

# 接续更新（2026-11-18 02:35 UTC；本段优先）

- 独立A newsource1786/1786前后一致，build145s0/bin c5493b7e62877bc4e3b00915b5da671c47e5e136f4e8e272110a38a54cbbc936，7pure+7DB各实际1PASS（public104.20s/fault63.11s/0–4均CDB0/FMT0/Ready不变）。**尚未最终gate**：runner首wrapper抓外层shell maps，actualtestchild maps未证，旧raw留存不倒填；root已要求查根因→shell=False directPopen且exeSHA/cmdline/start确认→只7DB新ns补证，不重build/pure。原26warnings未归因，计划CLIcheck/scopedfmt的raw也待核/补。44247仍runner独占，未归还main。证据`group-only-A-fix-independent-v1/RESULT.md`。
- facts3_native_verifier首真实历史15副本组件1.99s exit0，strace无网络/设备ioctl/写打开/sidecar，全部sourceinventory不变；仅历史control-only，export=null，testfixture/负例和最终freeze仍在进行，不能作现场authority。facts3_readonly_collectors首22软件tests通过，正在封fakeSDK wrapper和source；FIELD连接关闭、PRIVATE允许未来受控44247但本批未接DB，事实3 execute仍UNREADY。
- 原旧实带服务继续保持停止，原任务/检查点/PR/磁带未改。新实带benchmark未启动。Unknown B/公共CLI8/严格nativefactory/现场来源与接管仍未完成。

# 接续更新（2026-11-18 02:26 UTC；本段优先）

- A修复正式冻`group-only-fixed-plan-freeze-v1`（MANIFEST a322064a…757c7/SEAL0e7222fd…4bd13），最终v2 source964088a2…61ea2/bin75dfc4f8…81ca59；新增第五项同kind错误metadata attempt实际PASS61.013s。前三个真实RED均已修，normal public2file104.007s是v1相同生产字节的Sim自测，最终v2统一独立跑仍在进行。`GROUP-ONLY-A-FIX-GATE-REVIEW-v1.md`静态无新增阻断，计划7pure+7DB required；runner`reclaim_diag_validation`获唯一44247许可，main已释放并结束。不允许其他agent接DB。
- 当前其余活动：facts3_native_verifier只离线Rust组件（含directory只读SQL提取/内存SQLite，原native codecs；禁SDK/SSH/SG）；facts3_readonly_collectors只新增Python exporter/stopped probe及fake SDK tests（禁真实DB/SSH/SG）。旧实体服务、FDB45153和PR/磁带本轮未变。
- 语义合同末节已澄清历史原PID的argv/boot缺项可如实null，不能用predeploy PID683309冒充；须另有完整原actor/owner/实际注册归属/当前全停排他证明。新facts3语义分支后续才改，旧freeze/receipt不回填。READFULLSTATUS仅复用原CDB16位builder，新strict capture须同次ioctl真实resid（现device.rs负resid夹0不可倒推），实际三LU支持未知。源报告`PR-FULL-STATUS-STANDARD-v1.md`与seams SHA54bf60af…92dd58。

# 接续更新（2026-11-18 02:14 UTC；本段优先于下方旧状态）

- 真实旧reclaim e42985d8已安全停止，原任务仍非终态，5095.823s仍是未完成样本；207–209原服务全停，没有清FDB/PR或格式化。控制组件已仅上传独立目录并做三机EL9 loader/早拒共12命令通过；`control-component-loadcheck-20261118/RESULT.md`与独立GATE记录证明原unit/状态5文件未变、无SDK/SG/网络操作。所有本轮SSHmaster已关闭。**尚未运行新真实reclaim**。
- group-only A原独立gate发现Ready可退Single/半组选取/缩预算；main已取得3项真实软件RED并最小修，5短case现GREEN，ordinary内部v2 Single仍成功。新公开mode=group-only两文件真实driver正例实际104.01s（reclaim98.264s，Sim，非实带/非CLI8），root+唯一group后targetdelta2、源format终态/冷读/属性/回执通过；实际lineage故障读门禁正串行运行。待freeze→独立gate/runner，不能以coder自测声称交付。44247仅main独占；Unknown早隔离B与公共CLI8 C尚未做。
- B1b/controlOnly SDK/strict PR core均已获各自限定独立PASS，详见各GATE-VERDICT；不能合成完整HA/现场权限。control SDK新实测30.48s证明sealed cluster FD/同catalog只读调用，不是现场ExistingRoute成功。strict PR core未注册共享parent、无生产factory；EL9 control构件不等于最终候选。
- facts3 wire部分已冻`retest-tools-v2/evidence/retained-control-facts3-v1`：110 PASS+1 optional SKIP，seal1abb9571a023cc5fd4f93779868c7304146fb8b4f6dbc2cc5c0492d401f9da7b；完整语义helper仍缺，execute3在SDK/transaction前拒，UNREADY。不得将wire golden或自报Passed当真实来源。
- 新设计`FACTS3-SEMANTIC-VERIFIER-SEAMS-v1.md`（SHA0b34dd569ef967803fb39fa73c9adf92858ba632a52a92469ed3752a54e5c2fc）给原件矩阵/纯Rust控制与native归档join/同RV末空页exporter/停止host采集；EMPTY或同旧committed Fenced actor及FDB owner的singleton才支持，singleton需实际TransportID登记证据。全EMPTY也需新archive.v2 stop_evidence承诺完整acquisition来源，不能仅依赖缺失origin witness；不改原reset CAS/wire，不回填旧APPLIED。
- 当前三个子agent：`reclaim_batch_implementation`写A并独占44247；`pr_full_status_research`仅标准研究（无DB/SSH/SG）；`facts3_native_verifier`只做离线Rust控制/FDB join组件（无SDK/设备/网络），分析已由architect完成。root只协调/证据/交接，不改生产。后两批不构成最终reset/nativefactory/现场条件。

# 当前最短接续状态（2026-11-18；本段优先）

- **更晚优先：本轮明确采用公开显式group-only小候选，不先建设完整Single/本地持久cache**。`GROUP-ONLY-BENCHMARK-IMPLEMENTATION-v1.md`与`GROUP-ONLY-TARGET-LINEAGE-SEAM-v1.md`覆盖旧最小门禁的默认v2/Single前置：CLI/RPC显式--group-only、mode进原保存请求指纹，普通/旧请求保持v1原字节；一组≤32文件/固定bytes/源物理inventory与逻辑walk全集同事务资格seal，任何目标root/组准入前重核Ready，更早obsolete/format无Ready也禁止。Unsupported持久阻下tick重load；目标metadata须是原父/名单目录sourceUID/path的已证明逐代后继，仅唯一计划group/batch允许FileBatch。主coder已实施A此批，尚未compile/不部署；B持久Unknown早隔离（含Recover/reaper/cleanup/format）与C真实public8/fault矩阵随后，未完成。
- **持久正文边界已复核**：现匿名unlink FD仍只是cache，不宣称持久。已有source_head/source_copy/source_read、原inventory/index/entry/extents与真实SHA是原payload持久来源；受限group-only可不新增retainedlocalbody。Unknown必须保原source介质/claim/未结清C/V，禁退休/format/reaper/cleanup解除保护、不自动重读source/重写target/新attempt；已结清成员只可用原已验证目标证明，不能部分结清释放整源。失cache/restart/HA需实际测试。完整Single/持久暂存的大方案保留`SINGLE-UNKNOWN-PUBLIC-IMPLEMENTATION-v1.md`后续，本次不假报完整架构实现。
- **B1b已获独立限定PASS且selector误判已正式撤**：`KNOWNFULL-B1B-GATE-VERDICT-v1.md`核独立1779inputs/newtargetbuild/bin dd8c33ada014d772b175b06c974e1828ef518b22d9dfee7e16f98ad013af3ae7，4core+7→8FullS4实际95.26s/targetwrites0/delta2/原receipts保、100→101错candidate实际.77s/19rows不变/CDB0/精确Invalid。原selector993–994早有事务内atomic_error，未生产修/未造RED，gate已纠正旧报告。元证据缺口明确：首env缺失败raw尚未找到，不能说原日志已保存或此次停在preDB已证；源码env unwrap早于DB；preflight没记录listener，FullS4 maps有path/dev/inode但无时点SHA，短case没maps，不能回填。成功case限定通过，不是完整fault/HA/public/field验收。
- **control-only revision2已冻并独立验证中**：`control-only-revision-v2/OWNED-OVERLAY-MANIFEST.json`/15filetar/finaloverlay/diffs，原chain装饰/合法restart和sourceclusterpath TOCTOU的3RED→17定向PASS+原档案显式1PASS+真实startup1PASS/源hash不变，pure multilib replay桥/store fixed_voters可见性，currentraft READ_ONLY含WAL、基线prefix/后继原apply及压缩缺桥拒、sealed memfd四seal+同Arc catalog/FD终生。`CONTROL-ONLY-GATE-REVIEW-v2.md`静态无新已证block；runner正用固定1779base+15overlay构composite新source/target，不copy main公开A新hunks，无实际SDK连接。原coder现只加**独立cfgtest SDKsmoke**（`CONTROL-ONLY-PINNED-SDK-SMOKE-PLAN-v1.md`），生产bytes不变/默认ignored/禁止自己连接DB；SDK后续只读random serial `active_library_route`返None证明sealed连接，不seed/不充ExistingRoute授权正例，后者现场零设备controlOnly另验。
- **PR组件也独立验证中**：`successor-fence-core-v1/freeze-v1/MANIFEST.json` SHAdd8ab5470f20cb99b313725f65c3523c537c59f6445f95c25b60f48b724628f9，独立4e8base+privatechild注册1723inputs，coder19core+ordinary11+两E0603隐私负例与fmt通过。新gate `/root/successor_fence_gate`已静态无block/确认计划；runner与control软件分source/target独立并行。main sharedreservation字节127076ab…没改/没注册，生产nativefactory/FDB逐device intent/pendingmarker/HA未实现，绝不能用于field。
- 当前active调度：main `/root/reclaim_batch_implementation`做A（DB44247必要自测独占、现代码阶段未run）；runner `/root/reclaim_diag_validation`只软件control/PR双组件（无DB）；`/root/control_only_implementation`只test-only SDKsmoke接缝（禁真实SDK/DB/SSH/SG，待root移交）。其余gate/architect/explorer已完成可复用，max3子agent。root实际三机仅只读取证且短SSHmaster已全关闭；207–209真实仍停止、FDB45153/PR/两盘未在本阶段变更。现场最终还缺Unknown早隔离、facts3/strictfactory/nativeinit/harness/EL9候选，不因组件软件通过而跳过。

- 唯一worktree/main/HEAD4e8cc0c；用户和原WIP保留。真实旧run/e42985d8已正常stop，任务未终态；5095.823秒是未完成样本。207–209仍未新部署/启动/清FDB或PR/格式化。本轮root仅fresh只读SSH状态与控制文件取证，三新master均正常关闭；`control-readonly-20261118/RESULT.md`有原始证据和权限边界。新方案尚不具备实带复测条件，不把Sim时长当实带成绩。
- **B1b首次完整软件GREEN已冻**：`optimization-batch-v1/knownfull-B1b-freeze-v1/`，MANIFEST330521ef…0c73b，source`phaseB1b-knownfull-source-v3`1778inputs/b1f76dc3…24506，tar`phaseB1b-knownfull-build-v3-inputs.tar.gz`50a5e52e…284b5，binaryd37178d2ddcdfe85557f783f3c5a7082c165d30a22ef7455ed39b57b9aaa5892。actual96.998s/reclaim89.390565、原Full S4后生产takeover7→8、oldscope0CDB、原batch/issuer/全部receipts保持、targetwrite0、delta2、sourceformat/task成功及donor/archive冷读属性；4media core PASS、normalcheck0。18src scoped/60raw；NONSCOPED1144历史scratch/docs/test差异，**不是deployment白名单**。部分发布/acklost/txn故障/Unknown/default还未验，尚未独立通过。
- **B1b selector审查候选须先核，不强造RED**：gate `KNOWNFULL-B1B-GATE-REVIEW-v1.md`说Selecting helper先写owner后Invalid会半提交；main实际冻结v3 selector993–994已发现`await.map_err(atomic_error)`，报告可能读错源码/closure。main现在仅补Selecting old7→new8/错Candidate真实字节不变回归，先保存完整frozen函数SHA和wrap上下文；若已GREEN则不改生产/不删abort造RED，root待其完成释放slot再让gate复核纠正。DB44247仅main此回归，其他agent不连。原成功/失败namespace不修不重跑。
- **control-only在修gate两个实际binding缺口**：`control-only-implementation-v1`原11file/9default+2fdbCLIstartup/真实两库gateway provider0启停重启/真实3nodeCandidate→Abandoned→原barrier已自测PASS，非独立验收。gate `CONTROL-ONLY-GATE-REVIEW-v1.md`指出chainSHA只语法和cluster path校验后SDK再打开TOCTOU，且真Abandoned后samebinding重启未验。原coder `/root/control_only_implementation` revision-v2已取真实chainfake SHA接受RED与重启整raftSHA变化拒绝RED。现实现immutable evidence manifest/实际官方codec baseline+currentREAD_ONLY含WAL连续后继验证、samebinding3node重启；再sealed memfd+共用Arc<ManagedCatalog>/FD终生fixedcluster。新增授权仅control_only/lineage.rs、multilib pure replay bridge、store fixed_voters可见性（加原11files/directory immutable方法）；不改node/state/coordinator/executor/reservation/catalog。压缩无archive桥可明确拒绝，不能忽略WAL/只比Frozen/原mutable整库SHA。
- **PR strict core并行未接线**：`/root/reclaim_benchmark_fixture` own新scsi/reservation/successor.rs+private tests，parent尚未注册；17Sim/fault测试已写，尚待实际编译/执行。契约`SUCCESSOR-FENCE-CORE-CONTRACT-v1.md`，生产authority无实现，only cfgtest sealedfake；单步≤1PR OUT，原issue先持久/每步live+exactstate、unknown不重发/清PR，observeOriginal零OUT；ordinary fence/CDB/timeout/UA逐字保持。首批非0x4c proof也拒，4c仍必须exact归属。只隔离gitarchive4e8+child/private注册组件build，不碰mainsharedparent直到root协调，无DB/SSH/硬件。
- **下一benchmark最小范围已裁决**：`SMALL-BENCHMARK-MINIMUM-GATE-v1.md`允许自动TailRepair/NoCommit-successor后续，不等全大方案；但必须实做early持久Unresolved隔离（layout/load/source session前、跨tick/重启/接管零该任务后续IO，不能只末端Lost）、production入口pre-admission eligible→v2/其余v1 Single兼容、一旦v2准入不得降级。还需strict successor factory/namespace pending marker/facts3 retainedControl/harness原native请求身份接线。这些尚未实现，不能拿internal正例直接做实带benchmark。用户实带8×1MiB无性能gate，超过300s诊断不自动杀；软件600s原预算不变。
- root/三coder目前：main `/root/reclaim_batch_implementation`（B1b与selector回归，DB独占）、`/root/control_only_implementation`（上项binding修复，禁DB）、`/root/reclaim_benchmark_fixture`（PRcore，禁DB）；gate已完成待root复用。architect两正式契约已落，不需重复调查。分析与实现不同agent，冻结/测试期间只用不可变copy；后续新完整source/target由gate计划→runner→gate裁决。

# 2026-11-18 接续：旧实带仍未重启，优先闭合新方案两文件真实生产入口模拟流程

本段优先于下面2026-11-17的逐轮历史。唯一main仍4e8cc0c，原WIP保留；未新部署、未操作实带/清FDB45153或PR。本轮新增仅只读SSH核三服务仍inactive/dead/0PID、ZFS与控制文件副本，未start/stop/打开SG设备；三个新自有SSH master均正常关闭，证据见下。旧e42985d8实带测试已正常停止，原任务未完成/原状态保留，源目标两测试带未在本阶段重格式化；新candidate未ready，不能报告实带性能或完整实现完成。

- **最新有效B1b产品RED与生产实现**：main `phaseB1b-original-s4-red-source-v3` / binarydc4c032e…b387 实际44.967s/101：原group Full S4已真gen3、FDB仍rootgen2，通过生产takeover新GuardedTransport/实际PRfence issuer7→owner8，旧actorprobe0CDB，正常driver step1在原Lost“已准入，先观察原physical attempt”失败。前轮复用旧guard7被正确拒、编译参数漏传均不算产品RED。main现实现KnownFull恢复及10动作PublicationAuthority typedview，每sameTr校验（含Already*/cleanup），root已更正授权仅 `attribute_objects.rs` upload私有core/guarded wrapper（非attributes.rs codec）。secondary媒体文件先冻结，待新view契约再接。DB44247仍main独占。
- **最小control-only新批已实际分派**：真实只读报告证明不能直接用旧模式：FDBowner directory_file=None→barrier503；legacy with_options会删spool，Frozen非全业务读取gate。architect `CONTROL-ONLY-IMPLEMENTATION-CONTRACT-v1.md`已批准显式CLI/control-binding、inert FileService、独立只读BoundDirectoryAudit、deny actor无provider且永远不eligible、HTTP/direct业务全拒、Runtime/Net directory bundle=None复用FDBsnapshot语义；不改原Raft协议、不假清旧task。`/root/control_only_implementation` dev_coder已接binltfsd/files/gateway/newcontrol_only/mod必要hunk与tests，只ExistingRoute、AbsentAfterReset明确拒，暂不碰executor/node/state/coordinator/reservation/managed_catalog/multilib；任何扩文件先root。无DB/SSH/SG权限，用injected reader/无FDB软件集群，默认栈，原spool/audit源hash不变。main reclaim文件不重叠。STRICT successor fence/pendingmarker/facts3/harness未实现，仍不能现场bootstrap/benchmark。
- **官方oldcodec离线decoder已封存与独立判读**：`control-readonly-20261118/decoder/MANIFEST.json` SHA24224fdb486db593903b258338dec686a28b6326d051b335bdc6dd8ce959d04d；old4e8 baseline/helperdiff1722inputs，build56.15s0/run.574s0/strace、15inputs inode/mode0400/SHA前后一致。真实调用原apply_entry_for_replay(emptyRunning,false,false)，无Running/Directory.apply/node.start/FDB/SSH/设备。explorer已独立核3节点committed状态一致（Frozen2745/quiesce2058/executor node1-term2-round36011-ready/历史busy，pools/tapes/management空；SQL applied7133五业务表0）；不能把历史busy当当前物理IO，也不能拿已停状态当barrier通过。root fresh SSH只读已正常closed。

- **真实control只读取证已完成（最新）**：`reclaim-small-benchmark-20261117/control-readonly-20261118/`存三节点fresh *.status.*（MainPID=ControlPID0、inactive/dead、Job/cgroup空）、*.state-layout.*（实际state /mnt/meta-data/tape-rs/v1-20261106/state 在ZFS meta-data）、launcher SHA51d7b466…d57e（只在私有mount namespace隔离GPFS，业务data不依赖GPFS）。root沿既有用户凭据显式认证新E/ssh短会话，未将凭据放argv/env/文件。READONLY-COPY-MANIFEST逐源dev/inode/size/mtime/ctime/SHA/无WALSHM；O_RDONLY|O_NOFOLLOW|O_NOATIME只读复制每host5file约685KB到E/{207,208,209}0400。原三服务始终未启动/变更，无FDB/SG访问。SSH-IDENTITIES和SSH-CLOSED证明3master原pid/start/argv/socketinode核验→正常-Oexit0、pidfd退出/socket缺；不要复用已关闭这些socket。
- **control实际已证与下一步**：三directory.db pools/tapes/files/staging/file_tasks均0，applied7133/frozen2745。真实proto解码三HardState commit72616、snapshot71998；snapshot blob不同（source1/2/3等差异），committed tail原entrySHA1101192a…相同；各B库pools/tapes空、quiesce2058/frozen2745，但仍持久executor node1/term2/round36011/fenced/ready=true。不能当native barrier通过。explorer正核完整committed状态与启动control-only是否正常清旧actor；secondary coder在隔离old4e8基线实现纯离线decoder，不能猜proto或拿status当权威、不能改变apply语义。architect `RETEST-CONTROL-PR-DESIGN-v1.md`采条件式：能复用原Frozen空态就不加Raftmode；保留同链防新round复位，仅facts3 retainedControl+strict successor fence是必要增量；无field准入。目录applied7133<snapshot.directory_index71998的multilib投影语义须明确，不能声称已追commit。
- **B1b后端gate与main扩文件（最新）**：`ORIGINAL-CHECKPOINT-MEDIA-GATE-REVIEW-v1.md`允许未来联合freeze独立4pure，但完整publisher有sameTr权限阻断：普通prepare/publish/finalize/materialize/finish/cleanup不带Ctx，入口检查不足，upload_attributes前置写也必须保护。main已受派统一PublicationAuthority::Ordinary|Original(&Ctx)+10方法view，publicOrdinary不变，域Err原子；根代码还未验。upload实际在 `attribute_objects.rs`，root已授权仅其私有core/guarded wrapper最小hunk并保存WIP/hash（不是attributes.rs codec）。secondary媒体2file冻结，等main新view签名后再改，root将4pure独立合并稳定后联合freeze，不单独重复构建即将改的旧joint源码。main现DB44247独占，真实S4中断/owner7→8旧无恢复分支RED已开始（runtime session78410），build0/127s；编译参数漏接的前次失败不是逻辑RED。

- **reset v2已最终限定PASS，纠正采样范围**：`retest-tools-v2/evidence/reset-v2-private-sdk-v1/GATE-VERDICT-v1.md`核93offline/4实际SDK/底层0create拒绝/acklost原query/race两commit一胜/prefix/SDKmaps；**八file两份完整SHA采样均在SDKruntime之前，仅四实际运行file在runtime内自核前后**，不能将下条runner汇总解读为完整八file运行后采样。本轮未重建Rustgolden，现场未验。DB已归main。explorer `/root/reclaim_candidate_dependencies`已只读受派查nativeCLI预绑定原ID与fresh bootstrap正式接管retained owned PR机制；不执行现场。
- **B1b媒体core首build与夹具修正**：secondary联合build实际0/182.15s/source稳定，4core实际2PASS2FAIL/101；正例guard8/receipt7/zeroWRITE和epoch分离通过，两个负例coldmount fixture漏reservation_guard导致被正确拒，非产品逻辑RED。旧source/bin/raw保留，normalcheck原snapshot继续；仅修自己testfixture后另GREENfreeze/build/test，不改普通算法。B1b完整driver恢复尚未验证。

- **22:34UTC外部独立验证已回，DB已归还main**：`retest-tools-v2/evidence/reset-v2-private-sdk-v1/independent-v1/RESULT.md`：93offline实际PASS/0skip、7pycompile、无SDK/网络/输出defaultplan、8file前后SHA一致；真实44247四namedcase各PASS（一次apply、7前事务拒绝/原facts、丢ack UNKNOWN→原IDquery、两ledger race一APPLIED一UNKNOWN/两commit），不重试不cleanup。原始runtime `/work/tmp/reclaim-small-benchmark-20261118/reset-v2-private-sdk-independent-v1-runtime-output/reset-v2-private-ym92t1b9/`。gate正在核原始证据，未最终裁决前不称组件验收完成；现场仍未验。runner明确释放44247，root已立即归还main B1b必要runtime，secondary联合source/emptytarget仅编译/Sim不接DB。后续可复用explorer独立核native CLI预绑定请求能力与fresh bootstrap如何安全接管原retained PR，不执行设备。

- **22:29UTC最新调度**：main已实际落B1b Context/type/getter/constructor + Header3原issuer/mandatory route同事务 + typed body-layout authority，尚无可编译/运行PASS；secondary新媒体模块preview SHA5ee253a742c5c37d0faf8e087a526d80c58a8630d49325ccc401ae8964e7a25b，2typed入口+4Sim待jointcompile，原writer/read/probe不改。具体契约 `optimization-batch-v1/ORIGINAL-CHECKPOINT-B1b-HANDOFF-v1.md`。main无DB进程并明确临时释放44247；root已移交独立runner，允许下述offline后4SDK新case，结束立刻归还main B1b。无软件测试访问现场。
- **新reset v2私库工具冻结/静态通过，独立验证中**：`retest-tools-v2/evidence/reset-v2-private-sdk-v1/SOURCE-SEAL-v1.json` SHA5348ce06d2c024cd5fcb6087fd9d8269340ed2ca1b2c192bf294ed9db6f61c5e，tar c4cea6aa98c6368b5eafe088490b4b33ac1484bd58c438852eaddc9f0d8224e7，8file（原六仅README差异）。`GATE-REVIEW-v1.md`静态无已证阻断，root已复用runner `/root/reclaim_diag_validation` 独立93offline/pycompile/defaultplan后按PRIVATE-RUN-PLAN执行4实际SDK（v2成功、wrongfacts/前事务负例、acklost原query、v2两ledger CAS），直接create/commit计数。未回证前不称通过；旧12不覆盖，原fa5297 freeze/ledger全保留。新4case无真实PR/SSH/设备，无field4515x。

- **最新配额独立有限通过与B1b分派**：`optimization-batch-v1/QUOTA-PAIR-GATE-VERDICT-v1.md`核独立633source/emptytarget/build135s0/newbinarya2f43cd9…5fd42，quota exact4.88s和原双member wrapper1.28s均实际1PASS/0ignored；98files/99拒/100和101可用一pair、准备无CDB。26build warnings未单独归因，未整体验证。main旧wrapper自测3.462s0缺actualcwd的旧证据如实保留，不回填、不重复跑，独立新run有完整绑定。DB44247独立runner已释放，root现分给main B1b必要runtime。secondary `/root/reclaim_benchmark_fixture`已followup，只own新 `fdb_reclaim_original_checkpoint.rs` + fdb_media最小注册，resume只cold mount/verifiedCommitted+精确Current恢复原receipt/path（不sync/append），publish显式liveowner，原issuer不重绑；main负责opaque OriginalCheckpointContext、catalog/body_layouts/active_batch same-tr typed分派及executor。普通接口与默认v1保留；B1b新代码尚未验。
- **reset facts.v2静态复审通过，私库新fixture在实现**：`optimization-batch-v1/RESET-OPERATOR-STOPPED-GATE-REVIEW-v2.md`确认原两block已修，无新已证阻断，尚未独立83/offline或新SDK通过。`RESET-V2-PRIVATE-SDK-VALIDATION-PLAN-v1.md`要求4类最小SDK场景和create_transaction直接计数；外部coder正新增独立 `reset_v2_private_validation.py` / `test_reset_v2_private_validation.py`（+必要README），不改旧core/SDK/旧runner/test，默认plan、完整新seal后再gate/分配私库。新freeze应含原六file SHA差异清单，合并一次83+新offline+4SDK验证，不重复旧12。此轮外部coder无DB/SSH/设备权限；原fa5297 freeze与所有旧ledger保留，真实仍UNREADY。

- **本次接续核对（覆盖下方旧分派状态）**：唯一worktree/main/HEAD4e8cc0c不变，已有WIP保留。停止证据 `reclaim-small-benchmark-20261117/stop-current-run-v1/RESULT.md` 已核：三节点原stop均exit0、MainPID/ControlPID0、inactive/dead、无job/cgroup，停止时两轮设备queue quiet；这些是历史事实，下次部署必须重新核验。旧任务未终态，最后5095.823s是未完成样本，不是成功耗时。继续新方案软件工作，未重新操作现场。
- **FullDP+postseal已独立有限通过**：`optimization-batch-v1/FULL-POLICY-POSTSEAL-GATE-VERDICT-v1.md`，632input/新target构建0/newbinary7c2b8207…6c4；9后端+2策略pure和3私有FDB/Sim实际通过。clean封存/返回CDB2747相同、delta2；owner-fenced不返回proof/源R/V不变；NoHeader24rows原子/probe416只读。此前4PR-IN RED的epoch未变，不能称已注入UA。quota、B1b/Repair/NoCommit/successor/default/Single/实带均不在此通过范围。
- **配额窄修待独立审查**：`optimization-batch-v1/quota-pair-freeze-v1/SELFTEST-RESULT.md`，生产仅删remaining pair slots重复乘2，actual98files/limit100一member RED4.965s/101→GREEN7.494s/0（100/101正例、99拒绝且intent/member缺、准备阶段CDB不变）；3file冻结。root已给main独占44247短窗口跑同已冻binary的existing `second_member_retains_only_exact_group_history_pins` wrapper回归，未收到结果前不称通过。gate已复用确认quota独立计划；main B1b仅只读准备，不改该冻结。
- **reset facts.v2两个阻断修订已自测，仍待独立复审**：`retest-tools-v2/evidence/reset-operator-stopped-v2/RESULT-v2.md` / `SOURCE-SEAL-v2.json` SHAfa5297c17b59dd523ca350a49a5d1f77d5daa8fe9a9d6b7c0e4543e39ae5067b。原ISSUED exact facts SHA与真实management key/raw12字段已actual2RED→全83/旧70/新13 PASS；私有fixture原facts持久并query原文件，原8subcase失败保留。SDKTx/adapter/diag3未改，无新SDK/FDB/SSH/设备。gate现审这4file，新的facts2私有SDK和现场均未验；不升级任何旧UNKNOWN ledger/source。

- **当前独立A5已闭合，B1正式开工（更晚优先）**：`ORIGINAL-ATTEMPT-A-GATE-VERDICT-v2.md`限A：newsource630/emptytargetbuild135s0/binary861f269c...、3codec各1PASS、NoHeader57.53s/no_postseal_io/416readonly0write/24row atomic/issuer7→owner8旧scope拒、cleanhappy78.87s(reclaim73.519563s)/delta2。真实test_argv/cwd/manifest+serverstarttime/bootid/SHA/argv/cluster/lib身份齐。startup自动dispatcher、四branch、Repair/NoCommit/successor/default/Single/真实接管仍未实现，不能deploy。main `/root/reclaim_batch_implementation`已followup启动B1a私有Intent2固定FullDP策略+observed/DPonly/encodedproof拒Delta，再B1b opaque OriginalCheckpoint（原issuer receipt/manifest保持、liveowner独立）。已授权body_layouts/active_batch最小private分派hunk，原WIP preedit档在original-checkpoint-B1-pre-edit；不碰别的origin/import WIP。DB44247目前main独占（所有独立A/Probe已结束），后续软件run需先preflight且串行。
- **secondary FullDP后端批在A5immutablebase构建**：仅owned batch_media新helper+Sim，base tar619321e5de97687ac87c544c51abbb37941359858afeb93032dab1dc449acdd4/manifest37be8c34526bee1207e9507682b64e730096b14ea27ec465fd35045e4d1ccd15，630inputs核齐。更旧probe-v2-base编译自身PG已中止-15/104s留abort不算证据。第1commit_inner flag是incremental、不是force；sync(false,false,false)与sync_batch(true,false,true)无dirty均no-op，group archive staging无条件dirty+外层generation+1约束保护零byte，后端将证明DP/FM0/VCI/IP债/finish/getter不弱化。旧v1/Single sync策略不动；main仍要Intent2/真实actor整合。Repair必须pure fixedbytes prepare→持久→guarded execute，secondary未获write权限。architect `/root/reclaim_hotpath_design`现只读查新successor harness/旧maintenance BusinessIssued过渡，避免candidate就绪后工具阻塞；无新实带操作。

- **最新持久A5待独立验收/B设计（覆盖下方A3/A4）**：A4 actual真实driver NoHeader/atomic/opaqueProbe59.524sPASS（24rows事务abort不变、Header3/Actual缺失仍418readonlyCDB、issuer7→currentowner8CAS/旧scope拒、historyhash不变）、normalcheck31.24s0、fullfresh78.972s/reclaim71.525s/delta2PASS；但gate发现checkpoint wrapper在media finalepoch后额外io.verify(CDB)。已真实opaque RED59.489s/101，returned3407 vs mediaSeal3405(2PRIN)，非epoch注入。A5仅删尾io.verify，保media权限+Someepoch gate，之后pure require_scope返证；实际GREEN59.481s returned=sealed3405/epochSome347/probe416readonly、24rows/missingrow严格拒/currentownerCAS保留，新oldactor重新probe0CDB拒也实际执行。freeze`original-attempt-A-freeze-v2/MANIFEST.json` b65e6e92... source`phaseA-original-attempt-return-seal-green-source-v1` fullmanifest37be8c34.../tar619321e5.../binary1954a58f...，normalcheck7.515s0/inputsame。`ORIGINAL-ATTEMPT-A-GATE-REVIEW-v2.md`静态认可并要求独立3codec+NoHeader+cleanpositive，runner `/root/reclaim_diag_validation`现独占44247新源空target执行；main暂仅B1只读准备，无DB/新B生产hunks。
- **B接口与FullDP决策**：`ORIGINAL-ATTEMPT-REPAIR-DESIGN-v1.md`含具体B1 OriginalCheckpointScope(原issuer验证Receipt/Header3/manifest，不改原receipt；liveowner/PR另验)、B2两Repair模式与pure prepare→persistintent→guardedexecute→reobserve。volume close_tail当前commit_inner取now，不能仅存time后继续旧API；需真正使用固定preparedXML。Delta损坏IP不准NoCommit，缺同树g→g+1桥时须Unknown-repair，禁止提前default。实际源码group `sync_batch()`可Delta，归档stage不强制Full。root采推荐：仅PhaseA group FullDP，私有Intent2持久policy/Actual和normal/DPonly/encodedproof拒Delta，保原DP/FM/barrier/VCI/IP债/committed getter合同；普通v1/Single不变，代价大目标metadata字节增加，无新运行期开关。secondary `/root/reclaim_benchmark_fixture`已获group batch_media Fullpolicy+pureSim小批，main负责Intent2/catalog/OriginalCheckpoint+volume必要hunk，body_layouts narrow originalcheckpoint API已授权（原WIP preedit归档，不碰import_source/object_refs/staged_import）。secondary仍不得启writeRepair直到typedIntent/recipe落，actualRecovery四分支/NoCommit/successor/default/Single未做。

- **19:xxUTC后最新（覆盖下方早期进度）**：stage-binding修复已`STAGE-BINDING-GATE-VERDICT-v1.md`独立有限通过：newsource628/emptytarget134s0/newbinarye541b212...；4exact（global/parent冲突、page缺binding严格拒、validbinding保BatchConflict）各1pass0ignored，18rows不变；cleanhappy74.03s/reclaim68.800259s/delta2。AlreadySealed缺route/其他partial/noCDB与oldv1runtime仍缺覆盖。server startticks/bootid/exe/lib等已采，**独立test完整argv transcript仍未采**（不是server argv），不可扩称已核；future须实际cmd记录，旧时点不补。
- **Probe v2已独立限定通过**：`ATTEMPT-PROBE-GATE-VERDICT-v2.md`。v1None==None能放行已真实RED11pass1fail；v2 privatefinalizer强制Some→固定io.verify→Some相等，12actualpure独立通过（630输入/newsource/target），wrong/消失/verifyErr均拒，actualopaque factory未整合。独立type首raw意外0由runner错cwd检查main导致，原证据B/optimization-batch-v1/attempt-probe-independent-v2保留不计；corrected2negative在**B/attempt-probe-independent-v2/corrected-negatives-v1/**，actualpwd+metadata+absoluteManifest核copy，E0308/E0451正确/最终630恢复check0。另3type仅v1历史。secondary新file+注册冻结`attempt-probe-freeze-v2/MANIFEST.json` e29ed347...，未Repair/未DB/未HA；secondary线程已完成，可按payload复用。
- **main持久A当前在编译/尚无runtime正例**：newprivate RG3/GroupHeader3/PhysicalAttempt1(current edge/admit原子创建、64KiB bounded row+完整basechain/ordered seals、noHeader discovery、currentownerCAS/原issuer不变)与opaqueProbe factory已落。A1compile0/116s+2purepass、A2compile0/71s；历史hash修正只含不可变身份，live scope仍bind完整currentrow。catalog-only case2.93s因fixture缺真实archive-root被prepare拒，未到admit，不是协议RED；旧source/log保留、仅移除自己新unready live test/include。改用真实lifecycle根metadata lane：admit后tr.set fault比较原Header/current/attempt+0I/F/Q/quota/2/4/p（不含driver外层diagnostic变更），ACK-lost后同Prepared真admit，再actualOpaqueProbe零写、missingAttempt不补、owner100→101/旧scope拒。A3compile4处接口可见性/借用错误已保留、正A4修正快照；**新A尚未实际通过，default/dispatcher/Repair/successor/Single均未启**。main现独占44247可run，Probe独立纯测试已结束无DB；不得部署。

- **最新独立裁决/当前分派**：`INITIAL-HAPPY-GATE-VERDICT-v1.md`限initial internal-v2两文件软件正例及2codec通过：独立空source/targetbuild136s0、626inputs一致、newbinary9d03cc464e8971eb02547f6643ecc0e61f9206757dddd95d7aad4967329ad29d、happy75.02s/reclaim69.739s/delta2。首次漏ignored的0pass1ignored留存不计；actual fullargvfile及serverELF/starttime未采，不能扩称已核。source/privatecluster其余已记录身份一致；尚无default/HA/field结论。main无DB进程；44247已释放给main新A/冲突RED，未来run须完整argv+PIDstarttime/ELFSHA/argv/listener/cluster/lib前后。secondary已唤醒按`ORIGINAL-ATTEMPT-IO-HANDOFF-v1.md`实现新`fdb_reclaim_attempt_media.rs`+fdb_media最小注册，只读Probe（私有CapturedAttemptEvidence/report+recordedbase+optionalcurrent）；catalog/opaqueIo factory+reexports main负责，原4file归secondary不变。拟RG layout3/groupHeader3/privatePhysicalAttempt1，public parent2/IntentReceipt/ActiveHeader3不变，旧未部署layout2拒迁移；Probe不要求Actual/Header/session，原issuer epoch与现owner分开，Repair payload未定前不启写。新scope代码尚未验证。
- **新增静态原子边界候选**：`GROUP-ATOMIC-BOUNDARIES-AUDIT-v1.md`唯一明确候选是active_batch group bind：原binding缺而global/parentroute冲突时，先tr.set binding再Err经retry_result可提交半套；page还可能写后Ok(RequestConflict/BatchConflict)，仅atomic_error不够。需既有public outcome不变、Page/Seal只读严格verify已有binding或在Staged最终分支写全套，并实际冲突RED/keys不变证据；未跑故障，不把静态候选称已复现。其余close/audit未发现有效输入可触同类错误，分页正常checkpoint不误报。

- **当前最新：initial internal v2两文件完整流程首次实际GREEN**：`phaseA-format-result-codec-green-source-v1`/tar`ce7c6ff6075f28ee21af44e3b479d75460e15a50f5d8f906bd6acf85facd8cf3`，`phaseA-small-happy-freeze-v1/MANIFEST.json`+CODEC-DELTA，binaryd8934a94...，runtime`phaseA-group-codec-happy-runtime-v1`（以manifest确切prefix为准）实际76.44s/exit0、reclaim68.788s、target generation delta精确2。ns4a4fe625-d685-480b-abb7-3fe71278f764/op0ae796a7-df8e-4562-82c9-1d7e3c1fcd6a。实际断言Rf结果、source assignment→fixedemptyoutput、两个donor/archive冷全文+共享extents、archive完整FileNode属性（仅name/newUID/extents变化）；gate初审明确**无直接donor全属性/FDB属性witness、format前全coverage/源引用清空、逐file精确一次计数断言**，日志copied2/2097152与files/paths0不替代独立断言。故仅internal合法v2/Sim positive，不是default入口/HA/实带/完整验收。gate正审freeze与独立plan、runner新源新target重编准备（未接DB），main无DB进程并转OriginalAttempt稳定接口。
- **该GREEN前两项修复闭环**：monitor每组完整B核验+io.verify后复用原`index_state(current,false)`再verify，未增逐filecapture；旧monitor runtime已推进reformatting后complete receipt Corrupt。独立报告`MONITOR-HAPPY-FINISH-CORRUPT-v1.md`确认result validate硬编码parent1与真实parent2矛盾，原goal issued1保留，formatter空卷Committed证据当时仅内存、无durable最终回执，不许重放旧goal。codec实际pureRED→最小`validate_parent_intent`→2pureGREEN，旧v1编码goldenSHA8ea4d3f3...不变，unknown/wrong-wire/identity/coverage篡改拒绝。初始fullGREEN所用freeze还未加PhysicalAttempt协议。
- **独立组件/后续设计**：`GROUP-RETIREMENT-FAULT-GATE-VERDICT-v1.md`限AfterRv域错误原子性+dirtyStored通过，独立58.11s/21keys不变/Rv缺、旧dirty1203B拒不补。`PHASEA-RECOVERY-ENABLEMENT-v1.md`安排A持久attempt/只读发现→B四分支→C唯一linked successor/接管→D默认v2，main catalog/dispatcher/Single/factory，secondary四media/read文件归属不变。`V2-SINGLE-COMPATIBILITY-v1.md`具体旧路径复用限制：Rs/current原子结清、v2 guard/register/stage专属typed分流，不能只改版本判定；`ORIGINAL-ATTEMPT-MEDIA-SEAMS-v1.md`列readonlyrecover/DPcapture和既知checkpoint修复seam，generic tail取消原请求禁用。default仍v1、Admitted恢复stub/linkedPhysicalAttempt未实现；当前main准备Probe/Repair稳定接口给secondary。

- **最新实际两文件debug进度（覆盖下方“未driver调用”）**：合法内部v2 management→production driver已执行，普通默认入口仍v1。runtime-v1/v2在prepare ordinal1返回PoolBusy；独立同RV只读证据确认member0原empty F pin被generic namespace quiescence误拦，非epoch sealing/配额。主coder增加Selecting/Prepared精确本组pin授权，保留外来/缺失/非空pin及0/I严格拒绝。新binary82d348bd... runtime-v3实际58.97s，children2/target generation delta2，已到共同body/archive S4，但后续step0仍Busy，**尚无terminal、冷读或sourceformat通过**。新Busy交独立explorer，禁止以清pin/放宽通用池屏障掩盖。日志`/work/tmp/reclaim-small-benchmark-20261117/optimization-batch-v1-logs/phaseA-group-happy-runtime-v3*`；typed-pin矩阵首次仅fixture缺complete source hint，未到被测门槛，不能称RED。linked PhysicalAttempt四分支仍未完成。
- **其后进展与事实更正**：quiescence v3 binary b3d19768...，typed-pin矩阵coder实际3.40s PASS、独立runner同binary1.28s PASS（`group-pin-independent-v1/`）；gate静态无已证阻断，node/claim/identity/跨事务仍需补，独立新target重编中。driver runtime-v4实际59.44s，仍Busy且未命中任何checkpoint错误标签，未terminal。explorer报告`GROUP-HAPPY-RUNTIME-V3-ROOT-CAUSE.md`最强候选为settle→retire_at和coverage→retired_destination均经source通用quiescence被两empty F自阻；p/c、p/z缺失不能判未publish，compact会清，补查p/d已证实际published+compacted。新`GROUP-RETIREMENT-NARROW-DESIGN-v1.md`限定sameTxn私有token，同时修写/只读重入，已settled集合完整bundle非递归校验，普通v1不变。**新发现atomic缺口**：group_settlement db.run缺notifications::atomic_error，普通PoolBusy可能提交已staged Rv，不能假定之前全回滚；须实际同RV核并修原子边界/拒绝后无半套提交。未修stage binary a9380049.../tar4368445d...保留，当前单一私库运行`phaseA-group-settle-red-runtime-v1`先确认直接settle ordinal，retirement/atomic源码尚未改。固定可写`phaseA-dev-src`只供增量构建，每轮独立immutable snapshot/tar+前后hash；旧frozen目录不覆。
- **直接退休RED/原子缺口已确认**：未修stage运行实际58.95s/exit101，明确`settle ordinal=0` Busy，ns0243d818-0fd9-4564-8101-9df3de712821/opfa898c81-7b40-48fe-98bb-f0781d12a0aa。explorer单RV136839923200确认ordinal0 Rv1219B已提交、Rx/Rq缺、settlement=None、F仍在，source R/V原copy/版本1保留、复制计数0；ordinal1 Rv/Rx/Rq均缺。原namespace只读取证不重跑；`GROUP-SETTLE-ATOMIC-RED-v1.md`纠正此前回滚假设。main正实施私有Stored/Staged(&sameTxn token)退休写/只读双入口与atomic_error，持久Stored不得凭脏Rv/Rx补成功。typed-pin独立空target重建0/137s、623inputs一致/newbinary274ea14... exact1pass1.28s，证据`group-pin-independent-v1/rebuild-v1/RESULT.md`，最终gate原始证据裁决待回派。
- **退休修复实际推进但尚未terminal**：typed-pin已`GROUP-PIN-GATE-VERDICT-v1.md`独立组件通过，遗漏边界保留。retirement freeze-v1 /binary813c659d...：step0 Ok(true)/53.42s，step1 Incomplete，总64.94s exit101。独立同RV确认两member Rv/Rx/Rq+settlement均在、F全释放、source R退休/V缺、walk copied2/2097152B、group Closed/issued=closed1/audit complete18；报告`RETIREMENT-HAPPY-STEP1-INCOMPLETE-v1.md`。明确缺目标LAYOUTL8 tape-monitor，原普通writer发布后调index_state，新group checkpoint漏，须真实proof接线不能fakehealth/跳formatgate。main先fault-source-v2/binary91406539... build0/96.97s，当前串行AfterRv+dirtyStored实际case，随后未修monitor stage直接RED；未format/未完整冷读。architect正在细化已有四分支PhysicalAttempt及默认v2接线（默认仍v1）。
- **reset官方SDK adapter已有限软件通过（覆盖下方UNIMPLEMENTED历史）**：外部工具coder新6file freeze `retest-tools-v2/evidence/reset-sdk-adapter-v1/SOURCE-SEAL-v1.json` SHA1e6ee760...，原v3/diag3file不变。独立70offline+pycompile/defaultplan/CLI缺参数拒2、真实owned44247官方SDK12cases/31s全部通过；`independent-v1/RESULT.md`及`GATE-VERDICT-v1.md`核源码/cluster/API730/actualmapsdevinodehash/每case物理P trace与commit。CAS同RV一APPLIED一UNKNOWN，丢ack后原ID只读query/no再clear，所有12随机P保留。固定field45153+22范围与private factory隔离、SDK-BINDING/ISSUEDdurable、一次commit/init恒false。ctypes仅预load已pinFD库无CAPI调用，无任意fallback。该组件尚未访问现场，用户已有现场授权仍需root实际维护/归档/停净/freshbarriers；组件通过不等于新候选/初始化就绪。
- **组件独立验证最新**：writer/read v4已独立clean rebuild并通过15实际测试+7编译负例，`writer-freeze-v4/GATE-VERDICT-v1.md`限组件软件通过，非main Layout2/部署候选。Reset successor纯协议已升v3/seal8a596143...，45实际FakeStore测试、pycompile、默认plan与execute/query拒绝均由独立runner复验；`retest-tools-v2/evidence/reset-successor-v1/independent-v1/RESULT.md`及同级`GATE-VERDICT-v1.md`独立限离线协议通过。生产SDK adapter仍UNIMPLEMENTED，init_allowed始终false，绝不据此现场reset；architect现单独设计最小官方SDK adapter边界，尚未实现。

- **A0公开门禁v3组件已独立软件通过**：`optimization-batch-v1/phaseA-public-gates-v3/GATE-VERDICT-v2.md`；runner从冻结tar独立新源/新target重编，612inputs一致、build135s0、新binary b5316f3e...，5exact+Header2/nativepayload/缺route回归通过，真实私有44247 PID1878008/exe/argv/cluster前后核对。普通NotFinalized兼容已恢复，原v2未过历史不覆盖此结论；这不是完整group/HA/性能通过。
- **writer/read v4组件静态复审通过已知绑定与cohort缺口**：`writer-freeze-v4/GATE-REVIEW-v1.md`，manifest71db9555...，binarybb2f9364...。新source是批准composite（旧v3完整618source+仅actor、cohort helper/提取、原Header.order getter、caller order四delta），不含main新Layout2，**不能部署**。FreshIo私有factory、逐CDB权限+完整members+seen latch、reserve确认后首WRITE前收紧；ObserveIo固定session/group/device/round，禁写；一组真实正文SHA/原MD5读完，再一次尾complete capture B对照原S4 identity/EOD/closing，最后单tr完整cohort/finalauthority/epoch检查才seal。总2次完整capture/组(A发布前+B读后)，不逐file重mount/MAM/capture。15helper/fake/body及v3七type负例有证据，但真实typedActor/S4/FDBfault尚未跑。
- **当前独立runner `/root/reclaim_diag_validation` 正测writer/read v4**：按gate计划新源/新target重编15pure和7compiler反例，尚待返回；这轮不连接任何DB/硬件，source与恢复后hash须核。其它四file由secondary冻结不改。
- **生产主coder `/root/reclaim_batch_implementation`**：catalog/volume/executor/driver/settlement/audit/PhysicalAttempt；另已确认executor.rs/executor_fdb_reclaim.rs最小reexport及physical.rs的prepare_file_node最小可见性hunk，所有原WIP各有pre-edit。GroupFile/Rq2+同RV Rv原格式→原retire→Rq/settlement/首次计数+清本childpin已libcheck0，不清walk.current。Ry2 proof digest与v1字节兼容pure通过。新private layout2 current/original/issued/close已check，分页audit producer首版刚libcheck0/40.70s：逐q深验closed/member链，再8rows/page审整个RG域，末页F+progress并Rf绑定L/z/Ry，CDB时常数核seal。**尚未driver调用/typedCloseAudit运行/默认group入口、linked PhysicalAttempt四分支未实现**。主coder下一优先实际2file完整terminal，再故障矩阵；不能把unused函数/libcheck当功能通过。
- **secondary `/root/reclaim_benchmark_fixture`**：独占src/daemon/fdb_media.rs、fdb_reclaim_batch_media.rs、fdb_read.rs、fdb_reclaim_group_read.rs（旧WIP保留，父file仅mod小hunk）及main/v4 batch测试+两helper最小include；目前4productionfiles冻结供v4review/runner，线程已结束可followup。主coder不改这4file。测试1/2/8要求精确target delta2+donor/archive冷读，13pure实际过，production3ignored未跑。FormatProbe只是sameRV原bytes/identity见证不宣称完整target validator，待GroupFile layout2适配+产品stepcoverage消费前篡proof零破坏CDB故障测试。
- 新设计参考：`GROUP-COVERAGE-DESIGN-v1.md`、`GROUP-SETTLEMENT-DESIGN-v1.md`、`GROUP-READBACK-HEAD-DESIGN-v1.md`、`GROUP-CLOSE-LAYOUT-DESIGN-v1.md`。Canonical closed历史保留；private layout2明确拒旧未部署prototype不迁移，v1不变；issued/closed+完整分页audit防a/u缺失或orphan误idle，不删proof伪结清。Group3旧binary preHeader不安全，必须全3停机统一新版本，v2在途禁混跑/回滚旧binary。
- **SQLite实路径只读审计**：`FDB-SQLITE-AUTHORITY-AUDIT.md`，已查fresh FDBdaemon GET/reclaim无持久SQL mirror/fallback调用；正常写verified S4后stage_observed_body_layout入FDB（新8files需逐copy实际证明）。intake/proof SQLite正常Workspace Drop删除；crash/delete-fail残留未验。旧lookup public SQL plan API仍存在但无daemon调用，不能称项目所有旧能力已删除。main/v4旧readsession/intake/lookup一致，不因过期N1交接盲转大重构。
- **诊断工具小批完成软件验收**：`retest-tools-v2/evidence/GATE-VERDICT-v1.md`，seal-v4 963c87c5...，32独立PASS/pycompile/defaultplan/hashes；300s自动只读诊断未接新harness/实带。**reset successor仅纯协议fake**：28FakeStore/FakeTx自测PASS、seal `.../reset-successor-v1/PURE-PROTOCOL-SOURCE-SEAL-v1.json` b966d305...；生产SDK/CLI execute/query仍UNIMPLEMENTED拒绝，未独立gate/runner，不能现场用。原diag三file保持不变。

部署/复测仍需最终source依赖白名单、整树软件/故障检查、EL9普通build、fresh三机/介质/PR/维护状态与原状态归档、新reset successor CAS及SDK adapter、两测试带干净baseline。原v1 marker、旧sentinel/ledger不可重放或改写；原任务被人工停止不等于terminal，旧NoBusiness清理永久无效。旧root observer24129已关闭，不wait/重起。下一实带仅8×1MiB且无180/600性能门槛，5分钟必须程序化诊断，非自动kill。

# 2026-11-17 当前优先：旧实带测试已人工正常停止，实施FDB先行优化后复测（历史，以上段为准）

**再续最新（优先于下段早期阻断）**：A0已真public RED→GREEN：body-only合法Header2 SDK/Sim流程到compacted后升Header3缺route，旧5公开入口Already*/cleanup成功并删58B页；case2正常未物化已有lookup间接gate，本来GREEN、inode未变（先前gate过述已书面更正）。新六入口同RV guard的3 exact tests通过，`phaseA-public-gates-v2/`保存4file snapshot/delta/manifest、隔离源码tar与binary f6489837...。但独立预审发现materialize无Header/无terminal的普通路径旧NotFinalized变Incomplete，**v2仍未通过**：主coder正做v3真实回归再最小移动guard；首次compat测试因自身path与intent不符被prepare拒绝，非目标RED，未修生产兼容分支前重做。privatecluster .contains也在换exactbytes/sealedfd。A0最初LocalCapacity是fixture同目录replay预留16MiB被cache计入再申请16MiB，不是磁盘/headroom；独立import-cache目录修，预算/栈/时限没加。build-v2→source-v1 SHA6eebdf...、build-v3→source-v2 SHA38526b...各正确匹配，初查误比对已更正文档，未见旧snapshot覆盖。

secondary一次S4 writer两文件冻结 `writer-freeze-v1/MANIFEST.json` SHA1536cafdd12f97e15527abdd838698d663059a76e9bae9e4e6c2ad574e38aabc，newmodule ec03b037...、fdb_media584d6769...仅7line注册hunk。libcompile0/42.62s；path祖先冲突实际3PASS1FAIL→4PASS，**真实writer尚未执行**。主coder已另落`executor_fdb_reclaim_checkpoint.rs`完整caller草稿：onceS4→Observe guard→完整capture→plan.observe→persistproof→indexCAS→publish，但未check/driver selector/default启用。catalog Observe scope已libcheck0/39.88s，允许原base无proof/已新head需原planObserved，非Fresh不授append。PhysicalAttempt/startup/coverage/close仍未闭合。两writer文件归secondary，其他production归主coder，不能并写。

Group覆盖设计已由architect落 `GROUP-COVERAGE-DESIGN-v1.md`：Rq v2 GroupFile绑group/attempt/batch/member+plan/Observed/receipt，Ry新coverage_digest累积canonicalRq，Rf绑定；Node原本无W、RawFile才依W。FormatProbe只sameRV原bytes/identity见证，不宣称独立完整validator；产品step_coverage消费前坏proof→零破坏性CDB故障测试必须补。sealed后immutable/cleanup约束要实现，任意root裸改FDB后不全量扫描为明示边界。

diag v4已**独立软件通过**：32 tests/pycompile/defaultplan/source前后seal全部核过，`retest-tools-v2/evidence/GATE-VERDICT-v1.md`；harness/field未接。另reset_successor三个新file仅plan-only UNREADY checkpoint（17FakeTx草稿，事务/ledger/SDK未实现、所有execute/query拒绝），seal `evidence/reset-successor-v1/UNREADY-SOURCE-SEAL.json` 862ef25a...，不能用于现场或说reset工具已完成。旧run仍全停，所有这阶段无SSH/FDB45153变更/PR/写带。

**此前分工/阻断（以上段最新状态为准）**：A0协议冻结 `optimization-batch-v1/phaseA-protocol-freeze-v1/`（manifest b8ee3dc8...，coder-only diff e87ae5d5...）独立gate未通过：多个Already*、真实file物化和清页仍漏requiredproof，同目录GATE-REVIEW-v1.md。原freeze保留，12文件已解除冻结由`reclaim_batch_implementation`新增真实FDB反例并修。此agent继续catalog/volume/executor/driver/read/recovery/coverage；**现生产已按文件拆为两coder**：`reclaim_benchmark_fixture`另正式独占`src/daemon/fdb_media.rs`与新增`src/daemon/fdb_reclaim_batch_media.rs`（主coder不再改两文件），实现固定原batch顺序append/ActualPlan持久/typedHeader3/一次S4→PendingCheckpoint，不record/publish；Observe/最终proof/恢复/coverage仍主coder负责。公共API交接由两者协调，不能并改同文件。

fixture冻结 `optimization-batch-v1/fixture-freeze-v1/`（manifest8793ceaf...、batch源4cbd88ca...；两树独立重编0、各5pure通过/3生产ignored）独立gate亦未过：cluster contains护栏、proof.is_object假覆盖、未断VOL1实际载荷、未严格断源1→目标2。同目录GATE-REVIEW-v1.md；同fixture coder仍独占两树batch模块+最小include，先补真反例修复再做writer。未运行1/2/8完整privateFDB benchmark。

诊断工具3阻断已原coder修并留下3类实际assert RED→27PASS/pycompile0，`retest-tools-v2/evidence/SOURCE-SEAL-v3.json` SHA b9c951b40f6aaa9066615e534facecf72daf64a71d860791d9806cc5459d268b；仍UNREADY待独立复审/runner/新harness集成。`reclaim_phase_a_gate`当前只复审该diag v3，未运行测试/现场。上一版17PASS/seal-v2和未通过review保留，不可把自测当正式通过。所有现场仍全停，未新部署/DB重置/PR清理/写带。

续接核对：唯一main仍4e8cc0c且原WIP保留。PhaseA check v7 lib编译exit0（35.09s，79warnings）；protocol test build v2 exit0/122.95s，随后同binary4399a9de...默认栈实际通过2Header+2native payload纯测试和1private44247缺route负例，仅协议自测，writer尚未接通，不能称功能通过。独立gate正式执行前计划已存 `optimization-batch-v1/GATE-VALIDATION-PLAN-v1.md`：先冻结后检查/纯负例，再2file完整终态+archive/冷读+精确generation增量2；原RED阈值≤3不能当最终验收，1/8files及恢复/旧v1/格式化gate均部署前必验。独立explorer前置候选依赖报告 `optimization-batch-v1/CANDIDATE-DEPENDENCIES-PRELIMINARY.md` 已交coder；main的reclaim注册/children/management/format v2 gate需要逐项移植，额外body_layouts WIP不能整main部署。两份报告均不代表最终diff审查或实际正式测试。architect只读续行方案已存 `optimization-batch-v1/RETEST-TRANSITION-PLAN.md`：保v1 resetmarker和旧sentinel，新v2/head+immutable receipt exact predecessor CAS，不盲清/重放；实时PR/maintenance/归档seal/候选SHA待查。禁止重放旧reset/init/reclaim或旧NoBusiness清理。

旧binary兼容实查缺口 `optimization-batch-v1/old-binary-preheader-compatibility.json`：旧无Header ordinary recovery可能先修尾，旧parentdecoder接受1|2，Header3前不能证明旧binary failclosed。因此全3停机统一新版本后开业务，v2在途禁混跑/回滚旧binary；“新binary恢复旧v1”不等于“旧binary可恢复新v2”。未做旧binary实际重放。

并行文件归属：生产仍唯一`reclaim_batch_implementation`；`reclaim_benchmark_fixture`独占两树batch测试及各helper最小include（main scheduling，v4 production），原交接 `optimization-batch-v1/benchmark-test-handoff.json`，不碰生产。`reclaim_benchmark_diagnostics`仅独立`retest-tools-v2/`三文件，17自测/pycompile0但独立gate **未通过**，报告 `retest-tools-v2/evidence/GATE-REVIEW-v1.md`：迟到terminal吞300DUE、capture future异常静默、关键probe字段缺失误COMPLETE。原seal-v2与17PASS保留，待原coder新增失败复现修正，尚未集成/现场。`reclaim_phase_a_gate`已结束诊断review，等待生产协议freeze正式回派；slot优先生产审查，不把aux PASS当ready。

用户明确要求停止当前不能完成的测试，等新方案ready再真实带库测试。已正常stop三机原tape-rs-v1-20261106.service（nonowner207→209→activeowner208），各MainPID/ControlPID0、inactive/dead、nojob/cgroup空；B各device_busy0/iorequest_cnt=iodone_cnt两次1s稳定采样。未kill/改timeout/清FDB或PR/format，原task仍真实running未完成，保所有检查点。证据 `/root/jay/cicd_local/reclaim-small-benchmark-20261117/stop-current-run-v1/`。唯一root GET observer PID3642815按pidfd核身份后SIGINT退出130，execsession24129已关闭不能再wait/重起。原Ledger追加OPERATOR_STOPPED_BENCHMARK而非TASK_TERMINAL，最后copying/children4/owner36011，实际lastpoll截至5095.823s；无完整reclaim终态耗时。维护watchers/自有nft表仍保留，不能伪旧NoBusiness/rearm。新候选检查通过、旧I/O停净事实再核后，按已授权两测试带建立干净baseline复测；不直接覆盖/重发旧未知操作。

用户已明确实施方向：FDB完整权威metadata先持久/顺序writebody/适时LTFS index checkpoint/HA；按fileversion+replica独立op/extent/status/receipt，而group仅调度和共享checkpoint。Pending/Writing/DataDurable/Indexed/Unknown从真实原receipts/watermarks推导，不任意bool；sourceformat仍allrequiredIndexed+完整coverage。A先保原S4语义，完整view与真正DataDurable→Indexed分离在B/C/D，不能A就称全部完成。

唯一生产coder `/root/reclaim_batch_implementation` 已获完整PhaseA ownership，在main保所有原WIP逐hunk修改，并whitelist移植 `/work/tmp/reclaim-small-benchmark-20261117/candidate-v4-opt-src`（从冻结v3源copy1746files，不能整个main WIP部署）。design角色 `/root/reclaim_hotpath_design` 正式方案 `optimization-batch-v1/ARCHITECTURE-PLAN.json`。A是FDB先freeze组source/nativeattrs+archiveintent→append后原workingdonor生成ActualPlan并持久→body+archive同一finalindex/S4→整组物理proof+每file发布/coverage；不再bodyS4后第二metadata commit。旧v1/公共Intent和Receipt codec保留；oldwriter拒v2，专用typedcontext每CDBfresh检查，需要required augmentation没有plan/proof failclosed。A从两files5commit降目录1+filegroup1=2；rootdir合并可A2再降1，不跳rootcoverage。

coder已建managed8file+NoneToken真实production fixture baseline，600s到期children4未到计数oracle，仅证据不足保namespace；2file基线277.19s触发targethead1→6 delta5>bound3的稳定性能RED，未做终局coldread/功能完成，全部日志/计数保 `optimization-batch-v1` & `/work/tmp/.../optimization-batch-v1-logs`。生产新Intent/ActualPlan/pendingdonor TypedStage/内部Group gates正在接线，部分cargo check和codec2 tests已过，整体未冻结/未正式独立测试/未部署。main没有frozen旧production fixture helper，RED先v4独立test+include，main后独立module复用main Fixture，严禁复制旧helper覆盖已有scheduling WIP。后续稳定后gatekeeper先review/validationplan→test_runner→architect一致性→普通EL9build→完整physicalretst，5min自动采诊断非强制cancel。

旧v1收敛问题诊断：同RV原bytes证据 `reclaim-diagnostic-stall-child4/fdb-readonly-{snapshot,child-f03,verification-f03}-v1/` 已核Rj/Rs/RZ/RW checksum、lastmetadata f02 committed，f03原child5acac942...P73bytes成功version2、0/Iabsent、F存在、Rv absent、Rs currentf03 copied3，目标gen8Full→9Full，无Full→Delta证据。137组(3DP+1IP)EOD/TUR反复但当前执行PC未定位；不能猜具体self-lock或清pin。explorer建议诊断-only每stage begin/end/error日志，尚未修改。此known未明缺陷不可把group绕过说成已修复，现场服务现已停不再采实时perf（perf207本来不可用）。

# 2026-11-17 优先级切换：先优化实测瓶颈再复测（本条优先）

用户要求当前耗时无法忍受，先优化当前瓶颈再复测。原fresh reclaim e42985d8...仍COPYING且已提交child，原生cancel按reclaim_management明确拒绝已迁移任务；没有发cancel/kill/重启/清FDB/换op。原root watch24129仅GET原task继续，后台自然结清；新二进制须等原task真正settled及核验后才部署。离线分析fresh_runtime_receipts、设计dev_architect reclaim_hotpath_design（不改码），随后另一个coder执行最小批次，不再只等待旧benchmark完成。

5分钟排查是新用户诊断触发点，非性能fail/cancel；实际root到620.7s才采集，已向用户坦诚且status记录late。实测日志证明目标端重复定位/元数据核验耗时：209606F约11分钟60次LOCATE EOD218.36s、103次blockLocate164.68s；两drive READ6累计17.057s含索引非纯body，PR-IN5738次4.259s非主耗时。相关candidate热链 fdb_reclaim_media320-360多次capture→完整mount→commit→aftercapture，fdb_read327-425覆盖再次capture；copy每node/file准备target完整capture。不要凭统计删除guard/FM/PR/MAM屏障，不把跨并行CDBwall相加当E2E。确定事实+原SHA+源码refs已在fresh-benchmark-reset/reclaim-performance-analysis/physical-diagnostic-current.json及PHYSICAL-DIAGNOSTIC-CURRENT.md。原首8000行各节点截断仅到10:17；root补采20910:17–10:28完整filtered无截断位于reclaim-diagnostic-5min/supplement-after-1017-v1/。先设计同actor/session原真实owner/head证明下消除重复恢复；若必须C2提交组明确最小依赖，真实Scsi没有可信allhandle内容epoch，不能把旧A1仅Sim缓存当实带优化。

# 2026-11-17T10:14Z 唯一小数据实带 reclaim 已运行（本条优先）

同一fresh benchmark25c79c58-7876-47a2-862f-c4cb09108b2b、namespacef37a34e5-415d-4302-9bd1-f5132ff732c3、pool6fdb44b9-9f56-4280-b5c0-86a158ae8b02。三机已普通B-only owner运行candidate06ab/ELFb446，临时99/zz已精确移除。两盘真实初始化成功：L84→d4973b30-47c2-4dd8-af2d-a1c5338981c1 rev1，L85→c3863d02-4c70-493c-865d-17fb0f0eff51 rev1；原terminal files/paths0。初始化观测117.668s/221.929s仅准备。8prepare/8upload/8beforeGET SHA+size全过，stat都源卷，完整root children/list仅8普通files，root独立重验真实正文hash；各PID实际ELF b446已采证。

**唯一reclaim operation e42985d8-13ec-42ef-bae3-7acc23786fb4已经ISSUED并running，严禁再次提交。** 原request `fresh-benchmark-reset/harness/runs/25c79c58-7876-47a2-862f-c4cb09108b2b/requests/reclaim.json`，唯一root watch exec session24129持Ledger锁，观测只查原task，无180/600s gate。root currentfacts/raw `fresh-benchmark-reset/reclaim-field-v1-retry1-preflight/`。第一次root preflight发现operator旧normalized209把两驱动编号写反，在RPC前拒绝，未发新task；现root实际sysfs逐行校验209 sg7=changerB/605d、sg6=drive605B/605d、sg4=drive606F/6070，原真实DeviceAllow集合始终正确，未改服务/重复seed；原错误证据保留。后续facts用root修正版本，不能复用旧get-before7错映射。

下一：同原watch到true terminal→8get-after/bodySHA+stat目标c386、原source身份d497绑定（新source需当前tapeshow实际获取）→已冻结/独立13pure PASS post-sdk原函数同RV证明原Task/Rf、newemptyhead、globalVold空、old/new/target occupancy+pool/libraryidle→harness report JSON/CSV→gate分析实际证据→精确自有清理。维护tool旧binding固定oldnamespace且仍UNBOUND，不可伪造NoBusiness或换nonce。gate已批准有真实postcomplete后fresh ledger CLEANUP_ISSUED，adminlock下pidfd核原watch身份只SIGINT一次、真实退出/子进程/锁证明，所有watch停后精确核oldm.objects并仅删owned inet trb_1bc0892f66243c97，原rules对照；不是旧release正常退出，不改watch源或构造假receipt。任一未知保留表，不升级信号。最终SSH仅清本任务renewed masters。真实benchmark仍未完成，不能只报软件通过。

# 2026-11-17T09:00Z fresh 实带小数据 benchmark 接续（本条优先）
最新现场续：v2独立24纯测试通过，实际3host hostproof绑定原GUID/newinode，own zz覆盖后3机control已active且zeroDeviceAllow/noSG。原B quiesce2058、freeze2745，首次init在原生barrier409 prepublication被拒，官方SDK再查app prefixempty。随后同challenge三物理node barrier200；gate确认可同原intent/namespace一次有界恢复，已exit0成功：namespace f37.../initial_owner_epoch2746，证据fresh-benchmark-reset/init-recovery-preflight-retry1/original-init-recovery.stdout。Ledger保存原失败及CONFIRMED_PREPUBLICATION_REJECT/RESUMED_ORIGINAL_INIT/真实child exit0，禁止再init或reset。当前operator正在normalstop全control→按hash移own zz（ea401fb0...）→reload→原setupv1 restorefinal按原journal移own99→全3reload核原B-only finalowner→normalstart。尚无assign/format/upload/新reclaim。原probe失败字面\n dropin错误保留；实际生效zz文件为真实newlines，root确认后的SHA为ea401fb0...。原/libraries/B是operator误用别名造成404，正确完整serial路径已经root200核过，不能称库未注册。


用户新增明确授权：207–209本应用FDB45153前缀和对应SQL状态可清理；B库L00284L8/L00285L8两盘可重新初始化。只测8×1MiB，无180/600s性能通过门槛。未完成的main WIP保留，实测候选仍source06ab75df/daemon b4464a5f。

- 证据根 `/root/jay/cicd_local/reclaim-small-benchmark-20261117`，最新 `fresh-benchmark-reset/EXECUTION-IDENTITY-MAP.json`：benchmark run25c79c58-7876-47a2-862f-c4cb09108b2b；setup operation556e8ab6-303c-4e9e-bf32-4c8b17940afa；新namespace f37a34e5-415d-4302-9bd1-f5132ff732c3。不同operation ID是明确绑定，无须改冻结工具。旧1097保留控制器已被新fresh目标替代，不得再运行。
- 原activation首次因未挂ZFS在ExecPre前失败，已正常停全3并恢复各原GUID ZFS；原数据未变化。新candidate90在盘。三机当前实际inactive/dead/MainPID=ControlPID=0/cgroup空，服务User/Group空=root；真实tree和配置 `setup-field-observe/IP.stdout`。setup独立13、harness独立16纯测试通过；原始失败保留。ready plan canonicalSHA b4c817495b539d15d4a6c8a39b33d71ddb3855b9fa5be25bf94778ff7e098538经gate核对。
- ROOT已durable issue-state-archive到唯一benchmark ledger，**fresh BUSINESS_ISSUED，旧NoBusiness释放/换nonce/rearm从此禁止**。三机已成功归档whole state为同父forensic-only-556（不复制回活动），各原journal ARCHIVED_EMPTY_ACTIVE，完整forensic SHA与原批准树相同。shared SDK reset已一次APPLIED，原marker匹配、app prefixempty，fresh-benchmark-reset/reset-field-v1；永不再次clear。三机99 control已安装，首次启动因新precheck错误在ExecPre失败（实际合法systemd子bind被字面字符串拒，service零设备也拒zfs get），原自动重试已正常stop。真实probe control-field/207.mount-probe.stdout。正在独立验证control-precheck-v2，新的own zz只替ExecPre到新bundle，设备权限继续zero；最终全停后移exact zz再originalsetup restorefinal。尚无nativeinit/format/reclaim。后续状态以原ledger+节点journal/新字段事实为准，勿沿旧部署START_ISSUED重放。
- 当前maintenance原session仍VALID：runid reclaim-small-deploy-20261117-06ab75df、nonce d1933e9688989b7ab72da60a5dd401020、table trb_1bc0892f66243c97；真实remoteACT `/root/jay/reclaim-small-benchmark-20261117/activation-v3-20261117T060337Z`，watchers在ACT/session。仅隔离事实继续有效，旧业务binding还是UNBOUND/旧namespace，不能当fresh业务清理证明。结束需原owned watcher正常退出+精确owned表清理，在fresh原账本下核已结清。
- SSH master原3600s过期，root依原用户SSH授权显式重建（不换key/auth配置）。同socket paths，`ssh-control/MASTERS-RENEWED.json`新PID身份；后续只显式复用，禁止隐式fallback。
- 唯一shared FDB reset仅45153 `tape-rs/catalog/22/`，官方SDK原marker+clear同事务，不动45151/52/系统key。ordinary ltfsctl8f1a实际支持fdbinit，无需不存在的ltfsctl-init。先archive全部完成→reset一次→同unit自有99零设备control→fresh quorum/quiesce/freeze/init原namespace→全部停control/移自己99/恢复原B-onlyowner并正常启动→rescan/pool/serialassign forceformat这两盘→8source upload+beforeSHA→单原reclaim+true terminal+afterSHA与原Rf/globalV/occupancySDKproof。真实性能尚未产生，不能结束为软件通过。

# 2026-11-17T05:48:22.116428+00:00 真实小数据 benchmark 接续（本条优先）

用户最新选择：新优化候选经构建/安全检查后，在207–209真实B库做小数据 reclaim benchmark；**本轮没有180s/600s性能通过阈值**。单软件进程600s安全预算与实际硬件性能门槛不同。分析与改码使用不同子agent。完整FDB-only迁移/C2/all-origin仍未完成，本轮候选只含冻结已测优化，不部署当前main的未完成WIP。

- 唯一main HEAD4e8cc0c，现有大量未提交改动保留。最终候选source06ab75df、ltfsd SHA b4464a5f...；其独立软件限定范围通过、3机EL9 loader/help通过。真实三机新目录 `/opt/tape-rs/reclaim-small-deploy-20261117-06ab75df-d1933e96` 已纯文件INSTALLED，**运行daemon仍原4e8，未stop/switch/start/seed/新reclaim/新设备CDB**。
- 本轮证据根 `/root/jay/cicd_local/reclaim-small-benchmark-20261117`；动态状态 BENCHMARK-STATUS.json。fresh045102：B owner208/2310433、leader209；仅B605d/changer55L3A78011W9LL01，两drive605B/606F；source L84 UUID8335894d... rev13/空，target L85 UUID17f487c8... rev3。
- 真实只读v2 authority/gates COMPLETE（gates capture427ms），full最初Tr超时。新v3同RV八并发实际完整导出1097对象，但输入误要求total1096而PENDING；**实际1096 regular+1 native symlink**，不是root。见ACTUAL-BASELINE-SCOPE.json；保护全部1097，预期加8后1105（1104regular+1link），禁止丢link。v4typedcount+phase诊断/core来源绑定正在实现，v3冻结证据不覆盖。
- 维护backend-v3独立48测试/隔离nft通过，真实三机120s TCP校准各465/464/464 VALID，临时owned表均显式正常RELEASED/ABSENT、原rules/services未改。209负例与IPv6路径未验证。该旧session已释放，不能当新业务持续lease。新业务须新明确session，原SETUP后不得换nonce重建。
- 独立部署工具18测试通过，实际补齐facts计划9a74...；仅文件安装已完成。服务phase仍待持续维护/最新actual SDK gates/发行者事实与root独占/三机fresh cohort，所有旧controller/旧5271 consumed流程不得重跑。
- SSH task masters `/root/jay/cicd_local/reclaim-small-benchmark-20261117/ssh-control/%h`（完整IP）仍保留，MASTERS.json和各identity记录原PID/start/socketinode。后续仅使用已有master+BatchMode/ProxyCommand=false，不隐式重认证；任务结清后仅精确清理自有master。
- issuer helper初版独立13测试通过，但现场278ms实际3host proc partial，fd3枚举竞态本机复现，旧现场未记录self PID故历史归因UNKNOWN。backend已完成v2 held-scandir修复，issuer SHA6e1aeab2.../probe72155fd4...，18自测通过，正在独立复审/正式runner；未现场重跑。native_inflight始终另需SDK证明，不靠proc宣称空闲。
- core-v3冻结36自测但独立审查未通过：eligibility事实引用旧bound、基线/后验未充分绑本次actual collector产物。coder在v4修正，保留全部旧失败证据，不能跑未审core。仅有两带成员不证明L85可用；用fresh唯一非源成员事实+原运行时guards，失败如实报告。
- 计划8x1MiB新flatroot文件，source必须原L84受约束native prepare/upload，所有旧1097元数据+固定8普通文件before/after正文SHA保护。SETUP在任何旧GET/prepare/RPC前持久三机ACK（GET可materialize FDB）；旧8GET后重导出最终基线再pin，全部actual8上传UUID BUSINESS ACK后才传正文；只发一个原reclaim operation，未知只查询不重发。尚无本轮新源初始化显式确认，旧5271授权不能复用；工具/计划全部具体后绑定新op+source8335再询一次。

真实benchmark **未完成**，无新reclaim时长；不要将Sim、audit毫秒或预放置误称实带性能验收。下一继续provider-v2正式验证/现场只读→collector/core-v4冻结审查→有持续维护和fresh证据的正常3机切换→精确源重置确认→seed+单原reclaim+实际终态/全后验/报告。

# 2026-11-16 reclaim 性能方案实施中（本条优先）

最新接续（覆盖下方各阶段早期描述）：用户允许核验临时 SQLite，禁止持久业务镜像和本地回退服务。小数据 reclaim 阶段超过180秒即性能失败；单项小软件测试包含fixture硬上限600秒，超时保留日志、原namespace和任务，不清共享FDB。当前只有>8MiB extent/1600Full元数据压力，尚无本轮真实大容量吞吐验收。entry-v2长历史功能通过但advance236.084121秒，性能失败。

最新不可变候选 `extent-draft/lb2-retained-live-v3-tests` SHA256 `5c1112503c43f957a128af07b47be30aef6ae7a871c74bfbea12e27ed1d2edf3`、503构建输入前后一致。新实际RetainedLive授权/scoped LB2/FDBplan exact通过0.69秒，仅Live空文件非真实退休/完整父流程。原Default exact默认2MiB栈仍40.505秒SIGABRT（600秒未触发），没有reclaim阶段完成时长；不得称栈或180秒达标。此前resolved-only私有phase减少poll264280→170920B仍不足，须独立explorer定位新实际栈，再由coder修改。新marker-only升级源码不在该binary。最新状态与日志在 `/root/jay/cicd_local/reclaim-implementation-20261116/implementation-status.json`；无新部署或真实硬件操作。

用户已授权按完整方案实现：FDB保存独立完整extent目录，SQLite可重建；同会话核验复用、有界spool与批次提交、冷路径完整历史实读调度，目标小数据端到端180s。分批实施，不把局部软件通过称整项完成。唯一main工作树仍4e8cc0c；初始9路径未提交改动保留，完整baseline位于 `/root/jay/cicd_local/reclaim-implementation-20261116/baseline.{json,diff}`。

- 真实现场只读新证据已保存 `/root/jay/cicd_local/tape-real-20261116T025754Z`；三台仍4e8普通版，B owner208/2310433、leader209。原5271已succeeded（11-12），回执27/402709395B、总63.5742h；不能沿用下方“仍卡死”的旧状态。旧日志把0x5e称MAM错误，实际PR-IN；4e8两个独立窗口目标LOCATE605.578/612.610s，READ约10s。没有新部署或设备命令。
- A1 coder `/root/reclaim_implementation` 已加真实父流程RED测试（70Full源/目标+32目录分页），编译成功、单项测试正在执行，生产尚未改。A1为严格同epoch源capture复用和真实进度诊断；不能在整个step后盲目续epoch，失权/UA/介质或head变化必须drop。A2批量节点观察、B extent全发布读侧/迁移、C持久批次、D冷核验和E实带验收均待完成。
- 本任务owned私有FDB7.3.78：127.0.0.1:44247，cluster `/work/tmp/reclaim-implementation-20261116/private-fdb-i869w_ef/fdb.cluster`，当前PID1878008（使用前核/proc真实argv）。仅single/ssd模拟测试，healthy；身份记录 `/root/jay/cicd_local/reclaim-implementation-20261116/PRIVATE_FDB.json`。旧43843没有监听、旧cluster不存在，不启动或清理旧实例。真实45151/52/53禁止作为软件测试库。
- 统一CARGO_TARGET_DIR `/work/tmp/reclaim-implementation-20261116/target`，TMPDIR同根tmp，FDB_CLIENT_LIB_PATH/LD_LIBRARY_PATH `/work/tmp/tape-s3-physical-20261104-8otpe85o/fdb-tools`，FDB_TEST_CLUSTER显式新cluster。日志同根logs；RED source manifest/diff已保存。禁止扩大生产/测试线程栈掩盖问题，禁止批量执行所有ignored（含硬件）。
- gatekeeper `/root/reclaim_plan_review` 已确认A验证计划，须实际RED/GREEN、故障失效/原未知结果和FORMAT门槛，稳定冻结后独立review→dev_test_runner正式运行。当前尚无软件验收结论或三分钟实带结果。B1具体设计由 `/root/reclaim_perf_design` 完成，source `/root/reclaim_perf_source` 已提供全部R生产者及overlay发布接缝；源码仅一个coder修改。

- A1当前实际状态（后续覆盖上方早期尚未改生产描述）：初版strict-cache已编译并实际GREEN（1pass194.02s，source命令2555→354、最老history读1→0、PR2268→312仍保留），但独立gatekeeper拒绝验收：P1不支持VCR明确05响应的epoch变化误阻断fresh capture；P2诊断clear+递增reason写放大、progress观察范围不完整。coder正补反例修正，尚未正式独立测试/部署。初版源码和测试结果冻结在logs/a1-green-source.json、a1-green.diff、a1-green-test.log；RED二进制保留artifacts/a1-red-tests及SHA。

- A1 最新冻结 v5：13 项 coder 单测均各执行 1 项通过；gatekeeper 已核原始 log/exit。70Full 分页最老历史读取 0、源命令 354/PR 312，190.19s 包含造带，不是三分钟实带验收。真实 ScsiDevice 无可信 all-handle 内容 token，保留原完整核验；不得声称实带缓存收益。已修两处实际 fresh token RED，临时 Active failure ticket 已撤回，恢复基线依赖清理点并加完整快照 CAS。最后 token 返回后至 copy step 的外改交错仍待架构/基线差分审查，A1 未验收。证据 logs/a1-v5-handoff.md、a1-freeze-v5-source.json。B1 新模块未注册编译、接线 patch 未应用；B/C/D/E 都未完成。

- 用户最新收敛：FDB管理模式的全部业务元数据（目录、文件、版本、属性、链接、副本、extent、reclaim意图与回执）统一FDB权威，SQLite不保留目录/文件/extent业务镜像；SQLite仅Raft等本地控制，LTFS核验中间数据改有界临时文件。需先独立架构审计持久缓存与临时payload，分批核验/回填/切换后移除旧fallback；不能盲删安全路径造成旧文件不可读。B当前fallback仅过渡，最终不以8MiB超限退SQLite作为完成。A v5后新增4组反例尚未写入/运行，A无cargo运行，释放slot给架构。

- **用户最终明确范围（覆盖上方严格临时file迁移描述）**：允许核验使用临时 SQLite，但禁止持久业务镜像和本地回退服务。FDB为全部受管业务元数据权威；保留IndexReplay/FullIndexComparison/import/rename任务期SQL核验，预算/guard/生命周期不变，不作离线服务/跨owner恢复事实源。撤销N2/N3全临时file大重构。继续N1分页FDB extent+真实LB2旧copy回填、全origin发布、移除持久VolumeExtentCache和FDB模式SQLfallback；Raft本地控制SQL不受影响。

- 当前新增阻断：C1计数fixture尚无实际2→1RED，默认2MiB栈在当前组合原Unassign也溢出；旧v5同case实际PASS89.55s。独立dev_explorer已查真实guard命中，上层debugpoll合计2088144B、相对旧63帧+33872B，serde单帧不变。证据stack-investigation/FINDINGS.md；B在fdb_media尝试私有非asyncboxed恢复阶段（3tail入口）减栈，禁止增栈或仅改fixture遮盖。B N1组件owner失权/原batch续传和>8MiBfdplan已实际过，但生产读取seam/LB2/全origin/去持久fallback未完成；D1/D2a有实际计数/短tx证据，独立gate仍证据不足，D2b/目的热点未做。C2持久freshgroup共享提交由architect继续设计，未实施。软件/现场整项均未完成。

完整接续状态 `/root/jay/cicd_local/reclaim-implementation-20261116/implementation-status.json`。保留所有旧controller、原请求/结果和证据，绝不重跑已消费现场脚本；B605d/A禁用等原安全边界不变。以下为历史记录。

---

# 最新优先：4e8cc0c已部署，原任务与候选HA/后验V2继续（2026-11-10本地）

**本条覆盖下方旧停服/旧59/旧后验V1状态。** 唯一main工作树；产品候选 `4e8cc0c44828204033638d1f76ffab2f006256f2` 已普通EL9部署207–209，root `/opt/tape-rs/v1-20261109-4e8cc0c44828`，ltfsd SHA `75ca23d6a0dd286fd39334e76e590cd04924b0fe197ce87e87ca5a1b7dfa658c`。未push。source既有incoming相对link保护已修，5相关测试、最终六checks-v5、15matrix-v6及产品两轴全部通过；完整真实B验收仍在进行，不能以软件/部署完成结束。

新证据 `history-fix-v1/incoming-link-fix-v1`（IF）；IF/upgrade-running-v1.py **已消费且成功**，安装前后原完整R*恢复账本（含Z/U/V）/原P/布局/datafacts全等，真实unknownmetadata保留，由普通新daemon fence旧PR。原epoch1/task5271不变，16:02UTC owner2299538领取，copying已到child2；首metadata后继W3已同id/sameplan观察Committed、Rq2/35，第一源ref已退休（V61→60）、原P1保留。第二文件18B已有P2，W4创建/未结清；源未初始化。设备/源/目的约束仍B605d、L84→L85，A禁止。所有旧submit/withdraw/upgrade/HA均不可重跑。

- root/watch-native-v1与history/watch-coverage-v1仍只读。新候选HA IF/ha-candidate-v1/accept-candidate-ha-v1.py（session95272）已观察child2原I并于16:14UTC触发普通stop207/node1/2299538，正在接管/rejoin；先查本目录HA-ISSUED/COMPLETE/controller-v1.log，**不要重复启动**。旧59 HA已完成，备用v3禁止启动。
- 新后验history/run-post-acceptance-v2.py（session92543）已启动，只读等原任务最终succeeded和本次4e8候选HA证明，再自动跑history/post-native-v1.py及HTTP-empty-u。看POST-PIPELINE-V2-*和post-acceptance-controller-v2.log；**不要手动并发post/u或重跑V2**。旧V1已被动SIGINT退出且无stageissue，不能复跑。
- 39正文scope始终全部26源+13旧目标样本，另1096文件元数据/1070旧目标N/H/R/1 native链接；不能声称全部1070旧目标正文读过。HTTP-u仅在前述全过后新空L84 UUID/revision上发新请求，不能改原5271输入。
- docs/reference已从4e8实际六发布产物生成（普通ltfsctl客户端fuse，其余服务工具fdb,fuse；初始化另ltfsctl-init）；当前未提交为文档。后续仍须真实完整回收/复读/HTTP-u、报告（重复历史定位性能/600s宽限耗尽的真实HA边界写清）、最终review/GitHubmain合入。无新worktree/代码归档。

详细新P2红绿、完整恢复注册表/谱系与控制器消耗状态见local/handoff-history最新段。以下为历史，不覆盖本条。

---

# 优先接续更新：原任务冻结，修复 incoming 相对链接P2（2026-11-09 15:20UTC左右）

**本条优先于下方进行中的部署/调度状态。** 追加Spec review发现目标普通文件被改名后，原有相对链接仍指旧路径，会破坏当前LTFS视图。原B任务已通过新正常withdraw冻结，207–209应用全停、B队列quiet/所有sg及st/nst别名FD0，FDB45153不变。证据 `history-fix-v1/incoming-link-fix-v1/RUNNING-WITHDRAW-COMPLETE.json`。原5271父仍copying/child1、原布局/35对象封存/原Rc与P保留，无sourceRF/Rf，未开始源初始化；metadata后继W3结果未知仍保留。旧59候选不能直接重启续行。

生产保护已补齐：planner、封存计划执行（position I/O前）、Committed/检查点观察拒绝破坏既有相对链接的displacement；真实Unchanged仍可结清NoWrite。冷读实际RED0.04s、NoWrite RED0.05s均已绿；5项相关Full/delta/旧计划测试通过。六项最终检查v5全部0（默认771、FDB+FUSE1147通过），15父流程matrix-v6收尾中；产品两轴review-v1均0。冻结466源码在 `history-fix-v1/incoming-link-fix-v1/checked-sources.json`，本地提交以git log/CANDIDATE.json为准，EL9新发布仍待构建/部署。详细状态见local/handoff-history最新段。无实现agent，只有code-review技能明确要求的两个只读reviewer。

旧post sequencer已仅SIGINT停止，确认没有DATA-AUDIT/HTTP-u阶段issue；POST-PIPELINE-PASSIVE-WITHDRAW-COMPLETE及UNRESOLVED保留。**禁止重跑旧sequencer/post/u/withdraw/upgrade/submit**；rootwatch及coveragewatch仍只读。后续需新候选通过软件/两轴/EL9，再以新控制器保留原copying/未知metadata/原PR进行正常部署和恢复，同原5271，最后新后验调度/报告review/GitHub合入。A禁止/B605d/sourceL84→destL85限制保持。

---

# 最新接续：完整 native reclaim 真实 B 验收进行中（2026-11-09，本条优先）

唯一工作树 `/work/jay/tape-rs`，`main`；产品候选 `59bb7f9f52a93ea32cc1350fd4264fa8163e84b3`（含 `9dac53d` 完整父流程、`20a54bd` 实机 DVCID 修复和长 Full 历史/IP 恢复修复），未推 GitHub。当前未提交内容为发布文档。整项须完成真实回收、数据复读、HTTP `-u`、报告及最终 review 后合 GitHub main；不能以软件通过结束，也不要请求用户反复继续。

- 证据根 `/work/tmp/tape-native-reclaim-completion-20261109-2d357wak`，最新发布/部署子目录 `history-fix-v1`。最终六检查 `/work/tmp/native-reclaim-final-checks-v4/results.json` 全0，默认771/FDB+FUSE1145通过；私有 FDB43843+Sim 的15项完整父流程 `/work/tmp/native-reclaim-public-matrix-v5/results.json` 全0，包含70代Full、T2及IP失败。普通默认栈，未增加硬件超时。产品两轴v2均0，原P2恢复挂载64代截止已闭合，证据不等同实带。
- 三节点207–209已正常部署59bb7f9，路径 `/opt/tape-rs/v1-20261109-59bb7f9f52a9`；ltfsd SHA `95ce7c68cdabbc0793a74ee696d8107520150a712bc074f390d8647c4d9e6b8c`。普通EL9，无fault/minstack/GPFS。原45153、c51 namespace/state/unit保持。**仅 B605d 可用，A禁止；仅L84源→L85目的允许本轮写入。**
- **原任务绝不能重建/替换**：epoch1/operation `5271c82b-f857-4e4c-9112-d3eb660345de`，原请求根下`physical-reclaim-request.json`；source旧UUID643ea0be.../revision13，destination17f487c8.../revision3，pool native6/02004aa0...。原`--yes`、unassign=false已封存。当前真实progress已进入copying/children0，root元数据RW Writing；当前owner2254359（实时查）。源35物理对象/26可见文件402709395B已封存，初始化尚未验收。不得手工删除V、清锁、PR_OUT或盲重放机械命令。
- `history-fix-v1/withdraw-only-v1.py`、`upgrade-history-v1.py` **均已消费完成，禁止重跑**；停服时PR实际保留node2/2251142 key/type3，由新daemon正常fence接管，未人工清PR。全部原目录/副本/布局数据到START保持。原9dac/20a submit及upgrade也不可重跑。
- 根`watch-native-v1.py`仍只读运行。旧dvcid及history-v1 HA观察器均在未触发时仅SIGINT本地passive进程，UNRESOLVED保留，绝不重跑。**当前新HA观察器** `history-fix-v1/ha-v2/accept-parent-ha-v2.py`（session73269）已实际完成HA-ISSUED→原父/Rc接管→旧207正常rejoin，ha-v2/HA-COMPLETE记录2254359→2288565（14:59UTC），不能重跑；原I已有P终态，metadataW2 Writing仍待新owner观察结清，整回收未完成。备用v3虽已审查，**v2已issue，绝不能启动v3**。两轴0；该实验只声明原父/Rc身份接管，精确写中Q/I边界仍仅Sim。
- **后验已有后台顺序调度** `history-fix-v1/run-post-acceptance-v1.py`（session35905，只读等待已启动，两轴0），先查POST-PIPELINE-*和post-acceptance-controller-v1.log，**勿手动并发运行post或HTTP-u、勿重跑调度器**。原任务succeeded且ha-v2/HA-COMPLETE后，调度器才运行`history-fix-v1/post-native-v1.py`：1096文件元数据/1070旧目的N/H/R保持、全部26源正文+原目的13样本/1 native链接实读。原body scope为根`physical-baseline/BODY-BASELINE-SCOPE.json`，**没有/不会有BODY-BASELINE-COMPLETE**；不能声称全部1070旧目的正文读过。
- 最后`history-fix-v1/accept-empty-http-unassign-v1.py`由同一后台调度器执行（接续以HTTP各ISSUED/COMPLETE实际状态为准），仅当前任务+POST-COMPLETE全通过后，对实时新空L84 UUID/revision发新namedHTTP reclaim-u，禁止改原请求；两轴v3均0且HTTP-UNASSIGN-REVIEW.json绑定。等待本次完成后的库存时间，库存/观察/executor/FDB owner轮次节点一致，再证明1004归位与解绑。
- `docs/reference`已从新实际EL9产物生成；普通ltfsctl为fuse客户端，另提供fdb,fuse的ltfsctl-init。新增CLI帮助不代表实带已验收。历史20261108报告后续须追加本轮真实结果与性能限制。长历史重复核验/逐对象提交定位成本仍需如实计入报告，不能删证明或缩timeout提速。

现场最新细节及已消费控制器见 `.scratch/ltfs-ha/local/handoff-history.md`；以下均为较早历史，部署/未完成状态不覆盖本条。

---

# 最新优先：软件验收与最终审查通过，准备本地候选提交/EL9真实B验收（2026-11-09）

以下记录于本地候选提交前，实际提交SHA以git log及本轮artifact.json为准；GitHub未push、真实集群仍3bc8cf2未部署。本轮整项仍须完成普通EL9的真实B完整回收/HA/冷读、验证报告和GitHub主分支合入，不能以软件通过结束。

- **最终冻结软件已全过** `/work/tmp/native-reclaim-final-checks-v1/results.json`：fmt0；cargo test默认44suites770pass0fail12ignored；cargo test --features fdb,fuse 45suites1140pass0fail298ignored；check --all-targets默认和fdb,fuse全0；clippy --all-targets --all-features全0，仅已有src/client/fdb.rs prepare_file8args告警，所有本次新增告警已清（RawContext的ReadFact改Box只改内存，Serde JSON透明不变；NR双if等价合并）。CodeGraph sync及affected0（affected工具说无独立test文件；实际inline tests已运行，不据此减少测试）。
- **相同冻结源码+普通fdb,fuse默认2MiB** binaryfa0c20b46064ddf8的最新 `/work/tmp/native-reclaim-public-matrix-v2.log` 全11项0：default57.049、EE73.638、Native缺父117.604、原childHA62.911、raw/unassign100.202、NR64.121、旧版本58.608、mount-limit3.473、stale-queued3.572、cancelHA63.012、monitor终态HA62.563。软件私有43843+Sim，非真实带库。
- 最终review v7双轴1P1（queued stale反复击落owner），实际RED3.48s→v8 GREEN3.78s并保护原Rj/Rf注入反例；v8及最后Clippy v9两轴均0，报告 `/work/tmp/native-reclaim-parent-review-20261109-v9/review-{standards,spec}.md`，v8整体报告0也保留。v6旧link displacementP2已File-only/zero-CDB负例闭合。
- 冻结466个Rust/Cargo源SHA位于证据根 `checked-sources.json`，其SHA9b3a57ee6b65826f30daf58f246348982b5795e857702f494bb52140ab01f104；提交前已核完全相同。`build-native-v1.py`在该证据根，尚未运行，按glibc2.34正常fdb,fuse server / fuse client，无hardware-fault-tests。生成docs/reference帮助附录仍待新产物，不提前把旧帮助SHA当新产物。
- 最新真实**只读**额外audit `/work/tmp/native-reclaim-physical-idle-preflight-20261109-v1.json`：M98(91succ/6fail/1cancel)、Node49completed、S3metadata1082/uploads2228、RW/RT/Rj0、3assignments/2pools；原14类无占用+新Kg/Kr/RM/RY/RF5类空，两原源/目的全局Qv claim均空。SCSI0/FDBwrites0。Python最初缺LD和误用FDB Value.wait isNone的失败仅审计脚本问题，已改present()实际验证为空，不是现场占用。池API用正确namespace header：native6 file_limit200000/mount_limit0/mediarestrictionnone。
- 现场207–209仍原3bc8cf2；仅B55L3A78011W9LL01/B605d、源L84目的L85可写，A及Bchanger6070别名禁止，HH8 drive6070仍按原whitelist允许。两带旧live/NR/历史必须按全部源覆盖+合法GC+globalzeroV+原显式确认处理，禁止手工清真实V或直接擦源。

下一：本地commit固定候选（不push）→运行证据根build-native-v1.py→新B-only normal部署控制器（不能重跑旧upgrade/accept），原45153/c51 namespace/state/unit保持，所有原数据实读/索引/设备预检→原生reclaim L84→L85、原任务HA和源初始化/冷读/解绑边界实测→报告+帮助附录→最终审查/合GitHub main。仅一个worktree，无需新归档。

---

# 最新优先接续：完整 native reclaim 收尾中（2026-11-09，本条优先于下方旧状态）

唯一worktree `/work/jay/tape-rs` / main / HEAD085c547不变。用户整项目标仍是补全完整native reclaim、全部软件检查、普通EL9真实B验收、最终review和合GitHub main；**不能以组件完成结束，也不要等待用户反复继续**。当前约105条未提交变化，无commit/push/新worktree；现场仍原3bc8cf2部署，未发送新的真实SCSI/FDB写。

- 最近公开候选：CLI `Reclaim(lifecycle::Reclaim)`接原生异步、--yes明确source重新初始化/-u实际归槽解绑/原request文件恢复；client PreparedManagement允许media-reclaim，完整字段/同意/async回执校验；namedHTTP `/libraries/{serial}/admin/pools/{pool}/tapes/{barcode}/reclaim`已接，同generic body已解除旧501，但**未发布/未部署/未真实B验收**。文档新增`docs/native-reclaim.md`、cli-reference及management-model/task-exclusion已按实现更新，生成帮助附录仍待新产物。
- NR故障测试先因夹具用C而实际副本键R失败、次因错误词过窄失败，修正后`native-reclaim-deleted-green-fdb4.log`59.58s PASS（只损坏私有NR、Lost、0写、精确C/V不动、恢复本次字节后原parent完成）。v5 frozen7文件Std/Spec均0。
- 真实复现 logical EE alias无MD5而实际body有SHA+MD5导致archive拒绝：`ee-alias-red-fdb1.log`22.76s；`reclaim_upload_read_source`从原child+sealed sourceproof读MD5，正常media writer保留/计算该MD5并计属性预算，原metadata binder核实际body已知MD5，不能接受客户端假证明。之后两次默认2MiB溢出，GDB `native-reclaim-alias-stack-gdb1/2.log`分别定位共用reconcile+metadata/child大future叠加；`reconcile_active_media_on_takeover`、metadata::step、Copy.child改非async入口boxed阶段（Copy.execute直接返回boxed step），**无栈/timeout/PR/unsafe改变**。`ee-alias-green-fdb3`72.61s PASS。
- Native同名目的普通文件保留：Plan首版v7 displaced原/目标Object，原UID/extents/attrs不变，仅固定root归档名；stage前核，Committed观察按原路径顺序恢复旧tree hash，全量旧树必须一致。Full/delta纯test实际RED后GREEN。v6 Spec指出移旧相对链接会改语义P2，已限制**仅普通File**，旧目录/链接同名明确拒绝，加zeroCDB负例；v7已确认该P2闭合。
- driver所有父错误尝试保存RD诊断（UTF8<=1800字节，保存失败也报未知），保持原任务。format gate已有layout时，目的media_fault/import_required/requires_validation禁止源init；full/WP不代表已提交数据丢失。Default私有注入目的validation latch验证Busy、RD、0写、无RF且不自动清故障，恢复仅本次记录后完成；`destination-fault-green-fdb1`56.59s PASS。
- `/work/tmp/native-reclaim-public-matrix-v1.log` **9完整父用例全0**（独立FDB43843+Sim，不是真带）：default58.552s、EEalias79.481、missing-parent native128.017、原childHA68.884、raw/unassign111.561、NR66.062、旧版本60.848、cancel65.959、monitor终态HA66.560。完整`native-reclaim-public-cargo-test-fdb1.log` **44 suites/1137pass/0fail/289ignored**。这批在最后资源筛选与queued修复之前，不冒充最终冻结检查。
- 资源片：initial layout遵守pool mediarestriction、monitor.data_write_allowed以及实际同池占用+source/dest新增挂载<=mountlimit，0无限制；固定layout不得改选。新增`native_reclaim_layout_waits_for_pool_mount_limit_before_loading`实际`native-reclaim-mount-limit-green-fdb1.log`3.64s PASS：上限1等待、0 LOAD/MOVE/WRITE、无layout，RD原因、原Reserved任务可cancel。
- **目前P1修复待绿**：v7最终整体双轴均发现1项queued stale请求会反复击落owner（admit只存queued，startInvalid→executorLost、原q保留）；没有降低要求合入。真实RED `native-reclaim-stale-queue-red-fdb1.log`3.48s。`reserve_at`缺assignment现在为明确Invalid；start在queued且Invalid/MediaUnavailable时，同读版本证明原Rj/Rf/E/markers/Rp/Kg均无（若其他库任务占用则继续Busy），才写原failed/source_unchanged/clear本q；running/已有parent/数据库未知/Corrupt绝不如此结束。测试扩展stale revision、错误volume、已无assignment，夹具还逐次注入本次Rj/Rf字节，必须保留queued+证据不改，再只还原注入后结束原请求。**当前正在编译** `/work/tmp/native-reclaim-stale-queue-green-compile1.log`，session32568；先读日志，成功后跑新stale case、mount-limit、default/HA/native/EE（默认2MiB），补v8双轴关闭P1。不要盲重启消耗的旧RED脚本。
- 最后新host只读预检 `/work/tmp/native-reclaim-physical-preflight-hosts-20261109-v1.json`：207lisa1 PID1564240持B sg4/5/6、208PID873064/209PID1176874，无sg，三台原3bc8cf2正常，原cluster/state在。**SCSI0/FDBwrites0**。密码仍仅0600私有tape.md，绝不输出。A永久禁用，仅B605d、L84/L85授权，EE/GPFS其他应用不动。
- 普通EL9脚本已备于证据根`/work/tmp/tape-native-reclaim-completion-20261109-2d357wak/build-native-v1.py`，未运行；需要本轮`checked-sources.json`，cargo zigbuild glibc2.34，server fdb,fuse ltfsd+ltfsctl-init，client fuse ltfsctl，无hardware-fault-tests。新deploy controller应从旧脚本静态提取已审查primitives，**不可执行旧upgrade.py/accept.py**；原45153/c51 namespace/state/unit保持，正常停启，权威摘要/PR-IN预检，所有原数据读前后核。

下一：P1绿色/保护反例→最终fmt/default cargo test/FDB全tests/check alltargets/clippy allfeatures/CodeGraph sync affected→完整最终review（v7已frozen/full.diff25428行，报告都在v7；待v8关闭）→EL9release→全新B-only部署/实际完整reclaim/HA/冷读+报告→review通过合GitHub main。native6/L84目前有61 V(27live+2NR、其余历史)，L85有2163 V(1072live+2NR)，不得手动清真实V或直接擦带。完整源覆盖+全局零V+显式原授权缺一不可。当前单盘固定目的/双drive、目录或旧link同名、空间不足等限制如实写文档，不用清锁制造成功。无新功能扩展计划。

---

# 当前接续：完整原生 reclaim 开发中（2026-11-09）

用户明确要求“补全完整原生 reclaim”，本轮目标包含完整父任务、来源覆盖、显式双门槛格式化和真实 B 库验收；不能以恢复组件完成作为整项收尾。唯一工作树仍 `/work/jay/tape-rs` / main，起点 `085c5470b6c4f72c1eea86f1d815c4eb96b3d24a`（PR43）。目前有本轮未提交源码，未部署、未提交、未 review/推送。公开 native reclaim 仍封闭。

- **最新接续补充（以此优先）**：v3 两轴审查 Standards 确认1项已知V摘要误判、0新；Spec新增1项 Rz 缺/from会重置预算。已受控真实 RED `retry-role-red-fdb1.log`6.78s（原后继已取消、仅删除/from后错误建新attempt1），新独立 **Rr** 谱系（parent/file SHA/root UUID/attempt）随首次walk选定文件/state原子保存；successor时与双向边/state原子保存，倒溯必须连续到独立root，child/scan/native完成均检查；Reserved0child只清正确root，completed有界cleanup含r。`retry-role-green-fdb1`6.47s过，v4 frozen增量8文件两轴均0，P2明确关闭，快照 `/work/tmp/native-reclaim-parent-review-20261109-v4`。仍只是组件review，不是full/public/hardware通过。
- GC V误判已修（现有V后32字节是来源相关摘要：active_batch fold为receipt fp，import/node为C hash；不是历史兼容逻辑）：原键/原40字节/版本+精确C/H/scan/primary V/history/placement核验，RQ回执保留原V摘要。`obsolete-parent-fdb1`/`native-link-fdb1`/`default-fdb1`各3.6~4.7s因Reserved阶段被新增GC误阻断真实失败，GC Reserved现在返回None；`obsolete-parent-fdb2`43.46s暴露错误hash假设；fix后`obsolete-parent-fdb3`60.27s、`retry-role-obsolete-fdb1`54.36s均过（真实普通writer先提交第二版，GC后旧copy None且原P可重放）。默认`obsolete-default-fdb2`52.49s过。NativeLinks `native-link-fdb2`夹具过早套用引用等待断言而失败，限定仅Default/Obsolete场景；`native-link-fdb3`48.90s实际发现先普通read-source再识别Native会被拒绝，copy.walk改直接resolve metadata再分支，后`retry-role-native-link-fdb1`60.34s暴露夹具Serving向Execution降级续租被拒（旧代码忽略bool），现在按当前阶段60秒续租且assert，不复活过期许可。`native-link-parent-fdb2`118.94s实际过：补两层父目录、全源覆盖、初始化、FDB link attrs+真实LTFS冷读相对链接正文。`retry-role-finish-parent-fdb1`70.16s、`retry-role-cancel-parent-fdb1`67.56s过。
- 只读真实B FDB节点分类 `/work/tmp/native-reclaim-physical-readonly-nodes-20261109-v1.json`：45153，L84 V61/29 files=27live+2NR，L85 V2163/1074 files=1072live+2NR；**SCSI0/FDBwrites0**，不能当GC授权。为补真环境已删除 inode 引用，新增DeletedObject父夹具走项目真实Node Unlink发布（不伪造NR）。red compile1错用getter已修；compile2过但red-fdb1默认栈溢出，GDB deleted-stack-gdb1指明实际在父copy→共用media owner恢复链，非Unlink；pub establish_active_media_owner改非async入口返回boxed Future（独立volume/borrow生命周期，原检查/顺序不变），red-compile3过，**deleted-red-fdb2**45.88s实际因NR永远阻断父任务而RED。共同阶段box后lib`media-owner-box-libtest1`729/0/168ignored、HA finish64.23s过。
- **NR回收已首绿，尚待全检查/review/硬件**：`retired_nodes::deleted_copy_at` 要求Active + NR原namespace/inode + retained H精确等于NR.previous_head + 删除版本严格高于copy，N/H不修改；GC同时核同pool、原独立NodeDeletion结果完整匹配NR及保留窗口（保留期内缺结果拒绝、过期后NR永久身份保留）、匹配原primary完整PhysicalCopy及同版大小，scan/source/holds/全源覆盖仍核。C保留CopyOrigin墓碑，sourceV释放；physical_copy对删除inode的历史H识别，仍拒绝live current marker。初绿`deleted-green-fdb1`58.79s，验证NR bytes原样+原NodeDeletion结果完全可查询+file_head None+physical_copy None+父成功/目标冷读。之后强化了原primary/同版本C匹配并新增故障注入：完整coverage后只损坏本夹具NR checksum，advance必须Lost且0 WRITE/FM/MAM/FORMAT，原C/V及坏证据不改，RF无，再只恢复本次坏字节并同原parent完成；编译`deleted-green-compile2`54.06s过。**当前正在跑** `deleted-green-fdb2`会话本轮tool返回、`deleted-obsolete-fdb1`、`deleted-retry-role-fdb1`、`deleted-libtest1`（4进程，固定private43843，各fresh namespace）；先读这些日志不重复启动。
- 全局状态仍 main/085c547，单worktree，约96项本轮未提交变更，无部署/现场SCSI/commit/push；public native仍封闭。privateFDB数据盘/dev/sdb20G已用约1G/余19G安全；Root曾降至601MiB，是其他缓存增长，未删除其他项目缓存，后自行回到3.4GiB；所有新增build/test TMPDIR指定work目录。临时误起的全/work/tmp递归rg PID2809767已只停止本进程，避免扫其他巨量构建目录。
- 下一：等最新NR矩阵结果、补两轴NR及media-owner boxed增量审查；核对原生link目的已有节点/已覆盖后canonical投影、多目的/容量故障边界（不能盲改原layout，至少明确安全行为）、logical EE alias sourceMD5已知而dest不记MD5的保守阻断是否仍在；完整故障矩阵、CLI/HTTP入口+规范/手册、所有软件检查/CodeGraph、普通EL9 release(fdb,fuse、无故障feature)真实B验收、最终双轴审查/merge GitHub main依次做，**不能以组件收尾结束用户整项**。
- 本轮最新进展（仍未 full native/未现场）：取消后的原 Q/I 无正文→HA→原取消+新请求父完成 `cancel-parent-fdb1.log`62.64s PASS；预算扩展 `cancel-budget-fdb1.log`6.44s PASS，包括恰三次唯一后继/再试拒绝/原回执不变/原目录不变。`reclaim_upload_retry.rs` 的 Rz/child/next 与 Rz/new/from 保留并发唯一原边，原取消历史 GC 后仍可重放，后继必须重新 source read/prepare。
- 未映射 EE 别名正文片已实现：原 raw proof v2 分别固定原 alias Object 与实际 body Object，Plan 首版 v6 RawBody 固定 body，alias.length=0 不作正文长度；只跟随本卷相对 EE 数据路径一层，Native/chain/已知 SHA/MD5 冲突拒绝。数据及 alias 属性实读物化为普通文件，source coverage 接独立原 raw commit，spool 按真实 body.length 预留。补捕获 SQLite path 主键点读及完整性检查，不全表寻找每个 alias。`raw-alias-unit1.log` 实际 RED：改 body UID 的配方先发2条校验 CDB再拒绝；stage 纯来源配方验证移至设备验证之前，`raw-alias-unit2.log`2项 PASS（普通原始正文矩阵+未映射EE alias attrs/len/SHA/delta/断电冷读/zero-CDB负例）。
- 原生链接缺失父目录片：Plan v6 增 ancestors（原目录Object+固定目的Object），新UID连续、已有树全量hash不变，辅助父目录逐项来源/attrs/UID/路径实读核，Unchanged/修尾原最高UID核 first_uid，Committed核最终UID及每个新增目录，不借目录覆写既有节点；source目录优先沿真实源路径、合成前缀用原源root属性，EE ownership marker沿既有规则处理。`scaffold-unit1.log`3passed/1ignored（包含原 node Full/delta×root/dir/link×有无缺父目录24场景）；旧测试“缺父目录必须失败”改为文件占用父路径负例。完整新链接父用例首次 `scaffold-parent-fdb1.log`因夹具create_directory不递归而缺 `/links` 父目录失败，已修夹具，尚未动态通过。
- RY F pin 校验再加强：pending/begin/complete既有书签同样须原F存在，普通未收尾cancel seal拒绝已在精确P2父测试加断言。`scaffold-finish-fdb1`又默认栈溢出，GDB `scaffold-stack-gdb1`确认旧 writer group135K/batch367K与新 walk/step poll叠加；生产私有 write_group 改返回 boxed future，执行顺序不变，不改栈。之后 `scaffold-finish-fdb2`61.74s PASS、`scaffold-cancel-fdb1`60.23s PASS。
- 当前最新未验 GC 片：新 `reclaim_obsolete_copies.rs`，完整物理源 coverage+walk完成、原owner/layout/无RM/RY/namespace holds后每事务最多16条全局源V；仅本namespace plain N/H与精确C/V、已发布新版本primary+V+历史、原scan一致的旧版本转保留CopyOrigin墓碑，独立 RQ 回执/游标；未知/foreign/同版本replica/已删除N保留为源format blockers，任何页错误不写半页。**已删除节点的合法GC还未实现，不能宣称所有旧历史均可处理。** driver在已完整coverage后先跑FDB分页，避免每16条都重新实读源带；全部删引用后仍实际源proof+原双format门槛。RQ进完成后有界cleanup。compile1因completed_at可见性失败保留；compile2通过。`obsolete-native-link-fdb1`（会话53095）、`obsolete-parent-fdb1`（57890）、`obsolete-default-fdb1`（70479）正在跑，不重复；其中 OldVersions 夹具真走普通 writer 提交第二版生成真实C/V，再由父reclaim处理，检查旧物理副本不再可见+原上传结果可重放。**启动这三项后二次增加了primary placement/history校验，仅源未编译，须新编译后复验，不能复用当前binary为最新。** test帮助入参已改私有 NativeParentCase enum。
- Root磁盘余量降至2.7GiB（其他并行工作也在用），后续build及test TMPDIR均用 `/work/tmp/native-reclaim-build-tmp`，不清其他项目缓存。完整 all checks/两轴复审/真实B/CLIHTTP开放/文档/主分支合入仍待。公开native仍拒绝，现场未动，A禁止。
- 最新 P2 精确 RED/GREEN 已完成（仍不是 full native 验收）：受控脚本 `/work/tmp/native-reclaim-finish-red-v2.py` 只换回审查 v2 的旧上传 recovery 模块，其余相同断点测试与当前父调度不变，compile0/test101，`native-reclaim-finish-red-fdb2.log` 27.50s 在第二次接管 step2 因 monitor validation 锁存返回 Busy；finally 已恢复候选 SHA a226831b… 。`finish-red-fdb1` 两次默认栈溢出均保留，不算逻辑 RED；GDB `finish-stack-gdb1` 指明 walk/step 和旧 writer recovery 链大 poll 叠加，copy::walk 改返回 boxed future，不改默认栈/断言。候选 `finish-green-compile1`0，`finish-green-fdb1`62.79s 精确两次 HA+原终态→监控结清→父成功/冷读通过。RY 创建新增要求原 Q/I 完整匹配与 F pin；verify/cancel seal 在 RY 尚存时不得清 F，取消 seal 改在 monitor+RY 结清后执行。
- 当前继续取消后唯一后继片：新增 `reclaim_upload_retry.rs`，独立取消 Rz 回执不变，在同父 Rz/child/next 与 Rz/newchild/from 原子保存唯一 successor+替换未验证 walk.current，最多3次自动重试、每次重新读源/申请新请求，不复用取消的 P/Q/I；对取消前辈重放只查原边且不再消耗 quota。新前 Q 恢复书签拒绝、取消/GC/HA 后并发唯一新请求与损坏边拒绝已在 `cancel-successor-fdb1.log`6.17s 通过，编译1因 serde导入/FdbSlice比较/test私有方法失败保留、compile2通过。尚未全套检查/审查；新实际 CDB 前正文失败→HA→cancel→新尝试父完成测试 `cancel-parent-compile1` 当前编译，先检查会话95314与日志，不重复启动。新的更强预算/RY负例待补。未部署/现场操作。
- 父流程第一轮端到端已实际通过，仍非整项完成：`native-reclaim-parent-fdb3.log` 51.13s，默认分配→原异步 succeeded→全局 V 原始范围确实空→临时账本清理→目的冷读。最初 `parent-fdb1` 默认栈溢出；GDB `parent-stack-gdb2` 指明新增 advance/step/walk poll 合计约 800KiB 叠加旧 writer recovery 链，并非守卫递归；生产单对象 write 改为返回 boxed future。`parent-fdb2` 已父 succeeded 后测试错误地调用“活动卷查询”查询旧 retired 卷而 Incomplete，改为真实全局 V 范围检查，未弱化门槛。`parent-ha-fdb1.log` 50.73s 默认回归过；`parent-ha-fdb3.log` 88.48s 包括 HA 旧 owner 零 CDB、未映射原始正文实读、初始化/归槽解绑/冷读过；`parent-ha-fdb2` 因无 EE alias/归属 marker 的不合法 `.LTFSEE_DATA` 测试介质被严格导入拒绝，夹具加正确私有 ownership marker 后才通过。默认等待引用两轮零 WRITE/FM/MAM 与恢复通过 `parent-reference-fdb1.log` 54.06s。完整 `native-reclaim-parent-fulltest1.log` exit0，44 suites / 1134 passed / 0 failed / 282 ignored；这是修下述 P1 之前的候选，不可复用为当前冻结检查。Clippy `parent-clippy1` 仅旧 prepare_file8参数告警，亦早于后续修复。
- code-review 技能已用于中间审查，并依技能显式要求启动两个只读审查 agent（不能据此再启动实现 agent）。冻结快照 `/work/tmp/native-reclaim-parent-review-20261109`：Standards 0 hard +1重复分支启发，Spec 1 P1：未结清原 Q/I HA 直接进新 owner writer 被旧预约比较拒绝/监控 require_validate 阻塞。新增 `executor_fdb_reclaim_upload_recovery.rs` 和 `require_reclaim_upload_recovery_lane`：只恢复原 parent/child/第一物理输入，Q/I→精确 terminal 的动态只读授权，普通 write_group 仍严格完整比较；私有枚举 RecoverOriginal 使用已有 recovery_write_allowed，仍禁止 WP/permanent fault；挂载/establish_active_media_owner 后实际索引验证、监控结清，无 fresh Q。`child-ha-red-fdb1.log` 20.06s 复现首个 require_validate 阻断；`child-ha-green-fdb1` 又默认栈溢出，GDB `child-ha-stack-gdb1` 指向旧 writer 链+新父 poll，copying::step 改为返回 boxed phase future，`child-ha-green-fdb2.log` 57.04s 真实 barrier 已执行但回执未知→HA→原父 completed/冷读通过。
- 增量快照 `/work/tmp/native-reclaim-parent-review-20261109-v2` 的两轴报告：Standards 0/0，重复 read/retire/seal 已合并；Spec 原 P1 修复，另 1 P2：原 terminal 已提交、单独 index_state 尚未保存时中断，下轮跳过已有 terminal 令 require_validate 永久保留。已新增 `managed_catalog/reclaim_upload_recovery.rs`，原 `RY` 每 child 恢复收尾书签，begin 先于 establish、finish 核实际已登记 index+精确 terminal+0 Q/I+监控验证代次，格式化门槛拒绝待收尾 RY；daemon 有 RY 即使 terminal 已有也恢复原封存目标。`native-reclaim-upload-finish-check1.log` compile PASS；**P2 精确断点尚未动态测**，必须接着完成（不能宣称该缺口验收过）。
- 当前最新测试函数 `native_reclaim_parent_case` 加了严格普通守卫不能接纳旧 Q、改 recovery 请求零 CDB 等负断言；重跑 `child-ha-regression-fdb1/2/3` 三项均默认栈溢出，保留原结果，不算通过。现 production `driver::advance` 也改为返回 boxed future，避免其约 171KiB 构造临时值进入调用方 poll；`native-reclaim-parent-entry-compile1.log` 编译成功，三项 `/work/tmp/native-reclaim-parent-entry-fdb{1,2,3}.log` 正在新候选实际重跑。分别是默认+引用等待、HA+raw+unassign、原 child barrier 后 HA。不扩大栈，不改生产超时；若仍溢出，按默认栈 GDB 定位并拆 phase，不弱化用例。当前新模块还包括 `executor_fdb_reclaim_tests.rs`，在 lifecycle 根以 cfg(test) 引入，硬件隔离默认 ignored。原 red-compile1 是 inline tests 相对 path 错误，green-compile1 是 volume 漏 mut，均非产品断言红测。
- 现场仅新做了 FDB **只读**预检，0 SCSI/0 FDB写：`/work/tmp/native-reclaim-physical-preflight-20261109-v2.json`，45153 原 c51a94dd...、B55L3A78011W9LL01，原审计门闩为空、管理 91 succeeded/6 failed/1 cancelled，node49 completed。L84 全局 V61（当前普通 H29、较旧版本32）；L85 V2163（当前普通 H1072、较旧1091），均当前 namespace，分别跨两个 scan。普通 H 对比**不是 GC 授权**，不得直接清 V。v1 误将键中的 scan 字段命名 namespace，已在 v2 明确改为 V/volume/scan/namespace/file/copy；v1 原文件保留。当前 `compact_superseded_copies` 只支持 NamespaceState::Ready + retired scan，原生 Active namespace 的合法旧副本 GC 尚未接通，完整实带 reclaim 不能绕过这些历史引用；需开发 owner/parent-fenced 有界合法回收或诚实等待依赖。无部署/提交/推送。
- 最新原数据通路接续：新增 RO 固定双驱动/home 与库级 Kg，普通访问闭合，仅精确原 parent/child 能使用读写 lane。`require_parent_pool_at` 保留其他导入/节点/机械门槛且核对原 RO，避免通用 library-available 拒绝自己的占用；普通无原 parent 仍走旧门槛。`fdb_read.rs` 的原源、封存源、目的正文及节点实读已加 activity + 原 parent/owner/Permission/PR 逐 CDB 守卫。`executor_fdb_loading/writing/monitor` 已接精确原请求：目的必须已装入固定驱动且 map/SMC/DVCID/VPD/标签/密度匹配，不执行普通自动选带；监控仍采 TapeAlert/MAM。原上传 terminal 查询先于设备 lane，重放不重新开介质。`native-reclaim-upload-lane-fdb1.log` (旧 source reader MaintenanceBusy)、`native-reclaim-read-lane-fdb2.log` (旧池门槛 PoolBusy) 是原 RED；`native-reclaim-upload-lane-fdb3.log` 6.33s PASS、`native-reclaim-upload-body-fdb1.log` 13.15s PASS，后者两驱动/HA/原请求源读取→实际目的写入→SHA/元数据实读→验证/精确退休均通过，混入子请求/改版本/旧 owner 拒绝。最初 read-lane-fdb1 是错误过滤器 0 tests，不能算通过。
- 新增 `executor_fdb_reclaim_driver.rs` / `_copy.rs` / `_metadata.rs`，**父调度正在端到端排障，尚未完成**：原异步父接续、固定布局、原 RM 观察、正常 File 子请求/物理节点覆盖、原 RW/recovery/replan 和 RF 初始化接续骨架已接 Serving 调度。公开 generic HTTP 仍显式拒绝 media-reclaim；命名 CLI/HTTP 入口仍未开放。新增 catalog 原 child→ReclaimFile 恢复、覆盖查询/缺项工作分页、独立原任务发现/领取等只读恢复接口。首个端到端 ignored 测试 `native_reclaim_parent_drives_copy_coverage_and_initialization` 编译通过，运行 `native-reclaim-parent-fdb1.log` **默认栈溢出**，不是验收成功。未扩大任何栈，正在依 diagnosing-bugs skill 查 GDB；`native-reclaim-parent-stack-gdb1.log` 仅18帧不够，第二份 `/work/tmp/native-reclaim-parent-stack-gdb2.log` 正在记录100帧及 rsp 差值。不得将单组件通过表述为完整父流程完成。`native-reclaim-parent-check1/2/3` 先后为 reexport visibility、错误枚举名编译失败，最新 parent-compile1 修正后成功。
- 后续还必须补齐：默认栈端到端、startup 原 reclaim 恢复优先级、原 terminal/元数据证明重放时序、取消后原 child 新尝试、原生链接 canonical 父目录/旧目标替换、未映射 EE alias 覆盖、剩余 retained/global V 的合法等待和可判断异步进度、策略/容量/挂载上限、父故障矩阵、CLI/HTTP/文档、全检查、真实 B 完整验收、review/合入。新的父 `_copy` 当前最多只实现正在验证的普通文件路径，未知分支保守拒绝；不能据此开放公开入口。没有提交/部署/真实设备操作/推送。
- 2026-11-09 最新续接：`reclaim_format.rs` / `reclaim_format_source.rs` / `fdb_reclaim_format.rs` 已补制造商 0400(8 ASCII)+只读介质序列号 0401(32 ASCII) 的原介质证明、两分区及双标签核对、固定 UUID/时间戳的 quick reset、原独立覆盖+全局零 V/all-holds 双门槛、最多三次原尝试/ABA 防护，以及完成结果只读识别。保留原两分区；不执行 FORMAT(04)/MODE SELECT(55)，不是物理安全擦除。断网/超时不授权重放，只有原介质/原配方的真实 prefix 结果才可 rearm。`native-quick-reset-unit1.log` 八种写/FM/MAM 注入通过；`native-formatter-fdb1.log` 39.40s 通过实际 FDB+Sim 接管/丢准入/半写/断电/重试/丢结清及原子默认分配发布。源原始恢复最新 `native-raw-recovery-fdb3.log` 103.17s PASS，之前 60s 测试许可失效未改生产超时，夹具只在正常阶段边界续多数派许可。
- 新增 `reclaim_format_publication.rs`，当前未发布 schema **v2**，独立 `Rf` 完成回执，原子退休旧 scan/当前 cursor、绑定新初始空卷或执行已证明的 unassign、原管理任务 succeeded、通知和精确 E/Rp/Qv/% 释放，永久输出 UUID `key(5)` 保留。`reclaim_format_home.rs` 的非序列化归位能力必须同时绑定当前 owner 的实际空卷证明、原 MOVE 确认和实际 home 库存；仅空卷或仅 UNLOAD 均不能解绑。新增 `reclaim_cleanup.rs`：独立 RC 完成清理队列，逐事务至多 16 原账本行及精确 Ri；保留 Rf/RH/RJ 和永久 UUID，不清 V。`executor_fdb_reclaim.rs` 当前仅含 cleanup，已接 Serving 每轮一个清理步骤及 Execution 接任前 drain；**完整迁移父调度尚未实现**。
- 新增 `reclaim_mechanics.rs` / `fdb_reclaim_mechanics.rs`，RM 原命令、RN 最近确认、库级 Kr，分别持久化 LOAD/UNLOAD/MOVE；原 parent/owner/Permission 指针/PR 每 CDB 核实，原 map/条码/驱动序列号/home/完整当前 TUR 观察，未知 issue 只认实际 postcondition，不换计划或重放。Kr 阻止普通机械与设备读准入、初始化双门槛及未开写取消；精确原机械事实仍可观察。真实 Sim RED `native-reclaim-mechanics-fdb1.log` 5.19s：移入后自动穿带令 ACCESS=0，被旧 postcondition 误拒；现只在发 MOVE 前要求 ACCESS，GREEN `native-reclaim-mechanics-fdb2.log` 26.04s，包括 MOVE 已成功却 HostError、保持原 Kr/异步占用、禁止换新计划、恢复零额外 MOVE、再装载正文实读和错误 Permission 零 CDB。两项纯机械门槛单测 `native-reclaim-mechanics-unit1.log` PASS；Clippy `native-reclaim-mechanics-clippy1.log` 仅旧 prepare_file 参数数告警。
- 异步父任务 start 与 E/Rp 原子绑定；新增原 **reserved / 0 child / metadata idle / mechanical idle** 取消，与原管理回执一起结清，不单独清父门闩。迁移后仍保守拒绝“未开写取消”。`native-reclaim-cancel-fdb2.log` 24.33s、默认已分配完成+清理 `native-reclaim-cleanup-fdb2.log` 40.65s、lib `native-reclaim-cleanup-libunit1.log` 724 passed/158 ignored 通过；早期 `native-reclaim-cancel-fdb1.log` 是错误过滤器 **0 tests**，不算验证。首次 formatter 编译缺 reexport / private result_at、机械比较 FdbSlice、测试误把 inject 用在 SimLibrary 都已修复，失败日志保留。
- 最新新增 **管理任务** 的默认完成及 `--unassign`：`native-reclaim-home-assigned-fdb1.log` 41.71s PASS、`native-reclaim-home-fdb1.log` 42.93s PASS；两者实际 source/node/native-link 迁移→双门槛→HA→部分初始化/断电→原任务 succeeded→清理→目的索引仍可查，unassign 另做真实 Sim UNLOAD/MOVE home 与原子解绑。最后编译 `native-reclaim-home-compile2.log` 成功，包含更严格重放参数（归位回执也必须相同）；需在最终冻结候选重跑，不能沿用阶段结果声称整套通过。当前没有代码提交/部署/真实设备操作/推送。公开原生 reclaim 仍关闭，剩余 source/target 固定机械布局、父阶段接续/恢复优先级、完整 File/EE alias/原始节点调度、保留历史 V 的合法处理、CLI/HTTP/手册、全检查/真实 B 验收/双轴 review/合入全部仍在目标内，不得以此片完成收尾。
- 新证据入口 `/work/tmp/tape-native-reclaim-completion-20261109-2d357wak/RECOVERY-COMPONENT-STATUS.json`，后续设计 `REPLAN-NEXT-DESIGN.json`。当前 main 代码增加原任务的 Discarding/Checkpointing→Resolved、独立 RJ 回执和 IndexKind8；结清回执与旧 Writing token 分离，原 RA/Qv 仅原任务原子结清时释放。`reclaim_metadata_resolution.rs`、`reclaim_metadata_checkpoint.rs`、`fdb_reclaim_metadata_recovery.rs` 为新增模块。
- 恢复 actor 每条 CDB 检查原 task/owner、Permission 指针及 PR。重入先识别原已提交、未写、已修尾，再决定是否修复；回执丢失后不重复追加。空 IP、opening-FM-only、截断 IP 有专门 DP 前提；不伪造 Recorded→Current。SNIA LTFS2.5.1 §§5.4.3/H.1 要求增量 DP 的一致卷先生成 Full DP，不能仅补 IP；该分支保持原树/计数/UID且结清为 Checkpointed，不能当成已迁移文件。
- 未发布的 metadata Record 当前为 v7，Plan 为 v5；recovery_epoch 防止阶段 ABA，plan_origin 保证 Closed 重试仍验证最初 RV。File donor 实读和请求绑定新 Plan/RV 已实现；实际破坏 donor 数据会拒绝，旧正文/退休事实不改。原 Plan/RU 重试保留，不添加兼容器。第一次“同计划重试仍须核实最初独立 donor 证明”的真实 RED 在 `/work/tmp/native-donor-origin-red2.log`，修复 GREEN 和四项回归均通过。
- 新增 `reclaim_source_coverage.rs`：物理源全节点逐页核实独立目的证明，普通文件需要原始元数据提交、EE alias 需要原字节/退休及属性证明、目录/原生链接需要实际索引观察。目的卷根会被修尾修改，不能当原源根的归档；须建立独立非根目录。待写元数据任务阻止覆盖结清，覆盖齐全仍不替代全局零引用门槛。
- 目录/原生链接已接入同一 metadata RW/RT/RZ/RA/Qv 和 RH/RJ 状态机，Context 区分 File 与 Node，不虚构文件子请求。后续执行、发布、修尾等 API 从原任务取输入，已移除重复传 file/request 的内部接口。Node 任务准备后父阶段为 copying，允许零文件子任务；不能再按只读预留释放。`reclaim_metadata_node_replan.rs` 的独立 RV 以实际当前目标索引证明为依据，不伪造 donor。FDB+Sim 节点正常提交、发布幂等、完整源覆盖、HA，以及 T2→Discarded→实读新 Plan→唯一 RV→Closed/RU→发布/HA 均通过（`native-task-api2-node.log`、`native-node-replan2-node_replan.log`）。File donor 和空 IP 原目标回归也通过。
- `reclaim_raw_source_read.rs` / `fdb_reclaim_raw_read.rs` 已实现无逻辑路径的物理 UID 流式源读取，逐 CDB 核实 owner/原清单/PR，校验实际长度、SHA 和已有 MD5；不补建用户路径。Raw 源私有 FDB+Sim 测试已通过（`native-raw-read-fdb3.log`，重跑 `native-raw-batch1-raw_source.log` 3.98s）。早期 import 夹具失败保留，没有弱化产品校验。
- Plan v5 明确 raw_body（实际标签块大小、SHA/原 MD5），Context::Raw 持久化原实读事实，目的父目录为索引锚点。数据块数决定 DP 索引位置，0 数据节点和正文引用保持原几何。`append_reclaimed_body` 有界校验输入、固定 UID/位置/源属性，错误冻结；提交/恢复 Committed 的能力必须追加实际无缓存目的正文读回，XML 正确不能单独产生证明。四项纯 raw-body 测试通过（`native-raw-plan-unit2.log`）。FDB+Sim 未映射正文追加、实际破坏目的正文、发布、覆盖、owner 接管通过（`native-raw-batch1-raw_commit.log` 23.84s）。
- 原 NodeReplan 未发布接口已直接改为 SourceReplan（`reclaim_metadata_source_replan.rs`），目录/链接及 RawBody 共用实际当前目标索引证明和唯一 RV，不伪造 donor；原 File donor 保留。原节点恢复重规划回归已通过（`native-raw-batch1-source_replan.log` 41.21s）。RawBody 恢复测试第一轮错误注入落到 IP 而非 closing-FM，是夹具计数已修正；第二轮在 60s 多数派测试许可到期时正确停止，正在补正常阶段续约后重跑（`native-link-batch1-raw_recovery.log`），不得提前称通过。
- `reclaim_native_link.rs` 已补原生链接的逻辑迁移：精确原当前 FileHead/C/V + 原链接实际提交结果，原子新副本/版本、旧副本退休、原扫描推进及 checksummed 回执；保持用户属性、链接目标和 CopyOrigin，不虚构空上传。私有 FDB+Sim 并发幂等、源全局零引用、原回执和 HA 均通过（`native-link-batch1-native_link.log` 19.78s）。最终全软件/故障套件与现场仍未运行；当前 lib Clippy 新增 tuple 复杂度已通过别名消除，须重跑确认（原 prepare_file 告警保留）。
- 软件结果：4 项恢复 FDB+Sim（T1二次修复失败、T2、缓冲尾丢失、原正常提交已完成），5 项检查点 FDB+Sim（空/起始FM/截断/原DP成功但IP失败/再次检查点失败）通过；均含 owner 接任、原回执、未知结果只读重入、索引/计数/占用和历史独立性。4 个纯 Sim 测试包含 Full/Delta×3 IP 形态与拒绝边界通过，历史 codec 通过。产品源码的完整 fdb lib 非现场套件 710 passed/152 ignored；旧普通发布、registry、Closed重试、IP失败观察、普通commit各通过。
- 测试夹具两次触及默认2MiB栈上限：gdb 证明大 setup/metadata/execution Future 帧叠加，并非循环递归。现将拥有数据的阶段排队，前阶段返回后才 poll 下一阶段；不增大线程栈。后继尝试旧回归已重跑通过（`/work/tmp/native-phases-attempt-history1.log`）。其余场景及710套件须在最终夹具树重跑。保留所有失败和gdb日志，不将 RED 改写成通过。
- `executor_fdb_lifecycle.rs` 仅测试阶段重排；metadata phase 的 let 语句保留既有夹具布局，避免 cargo fmt 改出六千行无关差异。恢复前后 Rust token 流校验相同（加工具属性前），fmt/check-diff 已通过。最终默认/全targets/全features/Clippy/完整E2E尚待完成，不宣称全部检查通过。
- 尚未访问真实硬件；207–209 仍上一批 3bc8cf2 部署、45153/c51a94dd…、原 state/unit。A 库保持释放，后续现场只使用 B605d 的 L84/L85；两带均有 live/retained 数据，必须迁移与完整覆盖后才可在原明确确认/持久意图下格式化。后续动作前刷新服务、库存、owner和介质，不重跑已消费 controller。

下一步先结清 RawBody 修尾/重新规划故障测试，补原 EE 内部目录位置归属标记的受限规范化；再接完整父状态机/原异步管理任务与 CLI/HTTP、Serving 前恢复、源格式化双门槛及中断恢复。保留其他扫描/命名空间的保留引用，不可手工清 V/门闩推进。Raw 数据计划已实现，但完整父任务、格式化和公开入口尚未接通。全部软件、普通 EL9 真实 B 验收及 code review 通过后才发布；不要提前开放源带擦除入口。

---

# 发布接续：报告缺陷修复 PR #43（2026-11-08）

用户此前授权审查通过后合入GitHub主分支，本轮修复按此执行。基线e2380b5，产品8bd242a和3bc8cf2已分别完成真实B补验，当前现场三节点仍3bc8cf2。9c42490之前的5提交全部规范/需求两轴终审0，原两个P3文档问题已修正；发布追踪增量仅文档，不更改产品/测试或扩大验收结论。

PR：https://github.com/JayTsu-sh/tape-rs/pull/43 。本条记录于合入前，最终是否MERGED、主分支SHA/树、临时分支清理，以GitHub实际状态及 `/work/tmp/tape-readiness-gap-20261108-l6b6og6u/MERGED-CLOSEOUT.json` 和本地handoff-history为准，不提前宣称成功。所有源/测试字节仍与已检查/部署候选相同；不重跑任何已消费controller、不额外部署或碰A。完整native reclaim及其余性能/现场限制仍按报告保留。

---

# 当前接续：机械暂忙诊断补验通过，四项报告缺陷已收口（2026-11-08）

产品提交 `3bc8cf26ac5775596ce2e9eac752d56faa8590aa` 在前批8bd242a三项修复之上，保留owner的原503，机械暂忙返回reason=library_busy、ready=false及原owner/round；不跳过任何就绪屏障、不改超时或read_idle阈值。11项软件检查全0，真实B默认300000ms观察已通过；报告新增就绪诊断补验，全部未实现/受限项保持。

- 当前207–209目录 `/opt/tape-rs/v1-20261108-3bc8cf26ac57`，同新ltfsd SHA，原45153/c51a94dd.../state/unit不变，普通EL9无故障特性；未改低层/FUSE原产物。源3bc8cf2；246d02f仅修正生成手册argv0及JSON歧义，产品字节不变。两轴初审Standards0/Spec2P3文档，修后复审均0，原报告保留。
- 证据 `/work/tmp/tape-readiness-gap-20261108-l6b6og6u`，入口 `UPGRADE-COMPLETE.json`、`physical/COMPLETE.json`。L84原4389B正文SHA正确；读后307.072s出现真实默认退带，三入口503正文完全相同，owner1/round1568310保持，53.551s自然恢复200。SG_IO退带24.294s/MOVE28.837s均GOOD，回原1004；原装带UA记录保留。三进程及FDB healthy/double、14类占用空，原权威摘要逐项相同。A未动，无FORMAT/ERASE/手工PR或清锁，L84/L85策略归属保持。
- 新旧所有controller均已消费，禁止重跑；本批未推GitHub。源码/检查/实带与两轴证据SHA在报告JSON两个补验区，原123份历史及前批84份证据继续保留。

完整native reclaim依然封闭，actual tail actor/独立回执、IP空/截断恢复、新donor/replan、完整源覆盖/父流程/Serving恢复及全流程实带未完成；Range/导入/顺序链路性能和现场受限项仍按报告保留，不能将已修复四项当作全部功能实现。

---

# 当前接续：验收报告三项缺陷已修复并实带通过（2026-11-08）

最新用户要求修复 `docs/physical-acceptance-20261108.md` 指明的缺口。产品提交 `8bd242ac8eb8fb890be4f54102edfaeadabb23d6` 已完成旧drive/node/cluster查询、顶层stat/list/get及offline_message投影三项缺陷修复，附管理分页固定100的解析/帮助手册约束。10项软件检查通过，Clippy仅既有prepare_file参数数量告警；最终diff自查0，不宣称独立双轴审查或已合GitHub。

- 207–209已正常部署8bd242a的ltfsd/ltfsctl，目录 `/opt/tape-rs/v1-20261108-8bd242ac8eb8`；未改动的低层工具和FUSE继续使用e0a7625原产物，SHA逐项记录。原45153/namespace/state/unit保持，正常停启前后权威摘要一致，无强杀/清锁/手工PR操作。A库继续释放。
- 新实带证据 `/work/tmp/tape-acceptance-gap-fixes-20261108-64uu7mpq`：`UPGRADE-COMPLETE.json`、`physical/COMPLETE.json`、`final-integrity.json`。三节点HTTP与CLI管理查询及拒绝边界通过；顶层list自动跨页1096项全局有序，目录含中文；4389B原S3对象CLI/S3 SHA一致；下载失败不建输出、不覆盖文件或符号链接。
- 原L84新增offline/online两个成功任务，三节点list/show和CLI显示离线说明，上线后清空；L84同卷643ea0be...、原native6池02004aa0...、online/nonblacklisted，正文复读正确。14类占用空、三入口ready；S3元数据/上传/节点回执/池/回收摘要未变。没有FORMAT/ERASE。
- 原accept.py在属性筛选JSON对象与标量的脚本断言处停止，正确返回已保留。只读核对原offline任务后，resume-online-v1.py仅完成原计划尚未发出的online与收尾；均已消费，不可重跑。两次部署预检错误未到副作用点；build原缺链接路径失败、成功v2均留存。
- 报告原e0矩阵及123份证据保留，补验与84份新文件SHA追加至JSON的gap_repairs_v1；软件源SHA见checked-sources.json，最终发行来源见artifact.json。GitHub尚未发布本批修复。

剩余范围继续按报告和既有设计处理：完整native reclaim仍封闭，实际tail actor/独立结清回执、IP空/截断恢复、新donor/replan、完整源覆盖/父流程/Serving前恢复尚未完成；不能根据三项查询缺陷收口开放源格式化。顺序/Range/导入等性能建议及现场条件受限项没有借此宣称完成。后续新动作前刷新实际服务/库存/原任务，保留已有live/retained数据，不重跑历史controller。

---

# 最新接续：PR #42 顺序读与 FUSE 修复合入批次（2026-11-08）

最新用户要求 review 代码，通过后合入 GitHub 主分支。本批基线为 GitHub/main `d63932586fa4d57d7ab0be5f320003a2eee5ee11`，已对候选 `a4a0e6d12a0cadf59f9214309fdb22e984853615` 的5提交/17文件进行完整规范与需求两轴审查，均0项发现；13份产品源码、123份实带证据和11项成功软件日志SHA已逐项核对，本次复查fmt和Python8项通过。下方报告中的“本轮未推GitHub”属于前一验收轮的历史状态，不能用于判断此次发布结果。

PR：https://github.com/JayTsu-sh/tape-rs/pull/42 。本条是合入前的交接记录；最终是否MERGED、主分支提交/树及临时分支清理，应以GitHub PR状态与本地 `/work/tmp/tape-sequential-merge-review-20261108-99tdzs_r/MERGED-CLOSEOUT-v1.json`、`.scratch/ltfs-ha/local/handoff-history.md` 的收尾证据为准，不凭本条预先宣称合入成功。审查原报告为 `/work/tmp/tape-main-physical-20261108-h9vn4yhb/merge-review-{standards,spec}-final-v1.md`。

本批可执行源码仍与真实207–209已验收的 `e0a7625` 相同；本交接新增内容只记录发布状态，不重跑已消费的实带controller，不重新部署或触碰A库。既有旧CLI/离线说明、完整native reclaim及未具备现场条件的边界继续按实带报告保留，不借review扩展新功能。日常目录/分支仍是唯一工作树 `/work/jay/tape-rs` 的main。

---

# 最新接续：真实 B 库验收及报告已完成（2026-11-08）

用户要求“在真实带库环境验证所有的功能，并给出验证报告，可优化项也要体现”。本轮已完成可安全执行的现场矩阵及收尾，结论是 **completed_with_gaps，不能称全部功能通过**。正式报告 [docs/physical-acceptance-20261108.md](../../docs/physical-acceptance-20261108.md)，可机读摘要 `docs/physical-acceptance-20261108.json`；31组矩阵中27组通过（含明确拒绝行为验证）、3组入口/字段缺口、1组完整原生reclaim封闭，另列现场受限范围。不要按下面旧“尚未部署/阶段进行中”记录重跑。

- 实际207–209均为普通EL9源码 `e0a7625b920cad93d884939f5981c2cafeb4b97f`，产品tree `2b6cd858d2c3dc9a5f1197e0c490afe2fffe22d4`；ltfsd SHA `ae4e658266e63de03e200e52ce42c32bdd19c6ea5784157568cad229b572d8c2`。独立45153、原namespace/state/unit保持；三服务/FDB available+healthy/double，三个入口ready、14类占用空，所有本轮FUSE正常卸载并退出。应用挂载命名空间无GPFS，宿主其他GPFS未动；本轮未推GitHub。
- 原始证据 `/work/tmp/tape-main-physical-20261108-h9vn4yhb`；最终候选 `physical-final-e0a7625/physical-COMPLETE-v3.json`、`acceptance/phase13-COMPLETE.json`、`phase14-PASS.json`。报告索引绑定123份本地证据SHA，失败控制器与实际缺口均保留。`run-all`、`run-remaining-final-v1`、`run-final-stages-v2`及其接续都已消费；不可重跑变更脚本。阶段5规范JSON与源槽断言、阶段12固定100分页、收尾缺消费者返回400而非404等脚本问题已按原身份结清，没有替代操作或重复ACK/删除。
- 实带通过：CLI/HTTP池管理与异步rescan；S3 1000+5全局字典序分页、读写/条件/校验/扩展RenameObject/Delete；内核FUSE完整读写/属性/链接/目录及冷重挂；L84离线/黑名单/I-E往返/上线/validate；L84 CLI与L85 HTTP已分配import同卷刷新；真实非注入监控通知、扩类重连/显式ACK；同库互斥与正常HA原任务/回执/未ACK保留；原生FDB文件CLI/驱动策略还原；45153单节点及全体停服故障恢复。
- 三片本地修复为 `a6f987f`（受保护的顺序读会话与持久顺序客户端）、`46515ac`（FUSE根GETATTR恢复入口）、`e0a7625`（当前读证明与独立Full历史审计分开）。最终11项软件检查全0，三片规范/需求两轴审查最终0。原12KiB对象暖读43.273→5.268秒、0 LOCATE/IP定位；两组各连续3×64MiB，总384MiB写/384MiB全量读，实际顺序工具与SHA/ETag通过；6×2KiB并发观察到7 WRITE/3 FILEMARK/2 VCI的共享提交。L85 import1072文件/1080路径688.322秒，146 LOCATE占582.833秒；报告记录Range读放大、FUSE小更新约60秒检查点间隔、空间统计约81秒收敛和后台样本陈旧等优化项，不将端到端速率称裸带宽。
- 明确缺口：旧drive/node list404、cluster null；旧stat对已知FDB对象exit0打印不存在、list/get报不存在且失败get留下0B新文件；offline -o任务回执有说明而tape show offline_message为null。新fdb stat/get等正常不能替代旧入口通过。完整原生reclaim仍未开放，不能将零子父互斥HA当迁移/格式化全过程。
- A库保持释放；只写L84/L85且同卷/同池/在线/非黑名单，未FORMAT/ERASE、未手工清门闩/改PR。保护介质未装载/写入，仅另池ee6只读列举元数据验证隔离。无空白未分配scratch，正向assign/unassign、首次import映射及格式化未重跑；双库、现场WP/坏带/满带、断电写中HA、VIP、EE实时互通等未验收。只有两个精确自有MOCK订阅被清理，原旧批次显式ACK后删除；全局outbox/其他消费者/历史未清，测试正文保留供复验。

- **补充空闲退带观测**：文档提交98ed09f后复查曾出现readyz503；不是新owner或残留任务，实际为默认300000ms的空闲读挂载自动退带。owner2/round1501886保持，14:03:44Z UNLOAD GOOD、14:04:13Z MOVE GOOD，L84回原槽1004且同卷/池/策略保持。14类占用空、96管理任务终态，三入口随后自行200，无重启/清锁/人工move。报告新增通过组及探针误导/空闲阈值优化项；原健康、503、恢复证据分别保留，不可将机械暂忙当失权。退带后再三轮/70秒持续观测，三个入口200、同owner、14类占用空、FDB healthy（POST-REPORT-STABILITY-COMPLETE-v1.json），未重启/清锁。

---

# 当前交接：真实 B 库全功能验证与顺序读写优化（2026-11-08）

用户在 PR #41 合入后明确要求“在真实带库环境验证所有的功能”，随后要求按磁带顺序读写特性优化同 pool 的 S3 访问，并明确**读取顺序由客户端保证与实际写入顺序一致**。本轮继续完成验收及该优化，不因下方历史 review 收尾记录而停止；不开发无关功能。S3 ListObjectsV2 保持完整键字典序，实际写/读顺序用客户端独立持久清单保存。省掉重复机械定位不意味着省掉 owner/PR、提交屏障或失权后的重新核验。

- 固定起点为 GitHub/main `d63932586fa4d57d7ab0be5f320003a2eee5ee11`。已用当前树构建正常 EL9/glibc2.34 二进制，真实 207–209 全部升级完成，仍沿用原 FDB45153、namespace、state、unit 和 B-only 设备策略。升级前后原目录/上传/管理回执摘要相同、14类占用为空。实际部署 root `/opt/tape-rs/v1-20261108-d63932586fa4`；SHA 与普通配置逐机证据在新目录 `upgrade-COMPLETE.json`。
- 新现场总证据目录 `/work/tmp/tape-main-physical-20261108-h9vn4yhb`，接续先读 `CURRENT.json`、各 controller 日志和独立 stage 结果，不能重跑已消费的有状态脚本。新 Python SDK 在该目录 `python/`；运行测试使用相应 PYTHONPATH，FDB 只读审计另需既有 fdb-tools 的 LD_LIBRARY_PATH。
- 已验证：四类管理查询 CLI/HTTP（三节点）、pool CLI/HTTP CRUD/策略/原请求重放及 S3 bucket 联动、两入口异步 library rescan。发现旧 `drive list`/`node list` 返回404、`cluster`输出null；原生 `fdb drive show` 两驱动均正常，这些旧入口的缺口已分别保留，不冒称通过。
- S3 当前版本已真实完成旧三份正文冷读及摘要、1000+5字典序分页/跨节点token、V1/start-after/分隔符/编码、PUT/覆盖/空/中文、HEAD/GET/Range/条件、MD5/CRC/错误SHA拒绝、RenameObject原token重放与改参数拒绝。`phase2-PASS.json` 是按原操作结清的结果；第一轮本机旧SDK在发请求前拒IfNoneMatch，隔离安装boto3 1.40.76后续接；末尾multipart实际明确返回405/XML，原脚本误要求501失败，结清脚本保留原失败并验证目标未出现，无重发PUT或改产品迎合脚本。
- 通知 CLI/HTTP SSE/显式ACK、跨节点断线重连、扩类保留旧批次/旧类型积压且不发新增类型旧事件通过。测试消费者 `phase3-PASS.json` 的原未确认批次特意保留，后续 HA 必须按同 consumer/generation/delivery 验证，不能新建替代。
- 唯一 `sequential-baseline.py` 已消费，实际3×4KiB按持久原序读写且 SHA 全对；首 PUT120.468s，后两次16.410/15.644s均0 LOCATE/LOAD/MOVE。三个GET44.232/43.192/43.354s，每次12 LOCATE/6 IP，目标性能断言exit101，原红测保留，禁止重跑PUT。随后 exact owner+private FDB/Sim红测left12/right0，修复后顺序/后退/短间隔及9类失效回归全0（sequential-v3-results.json）。新read_session绑定原Permission/PR/epoch/有效VCR/当前head，导入/无SHA/当前同UID正文不同回退旧路径，未削弱逐CDB/FDB及终核；尚未部署新优化。
- `phase4-fuse.py`、`phase5-tape.py`、`phase6-ha.py` 已准备但**尚未执行**；新无设备 HA helper 已按当前main构建并保存来源，允许原executor在Serving前自动领原零子父，并验证精确release重放。执行HA前须重新采集当前profile/authority、先结清普通数据请求，不能沿用旧fa7轮次/修订号。
- L84和L85当前均有live/retained数据，不是历史空带；仅这两盘授权scratch可写，不重跑历史格式化/空带脚本。A库保持释放；B changer6070别名不碰，原白名单内两台B drive仍允许。完整native reclaim公开父流程和源格式化继续关闭，相关未接通内核不能算整功能实带验收。双库并行/均衡、现场写保护开关、满带或永久坏带等条件未具备，必须保留未验证边界。

- 用户再次明确要求所有功能真实带库验证并出具含优化项的报告。当前 `phase4-fuse.py` 原 exec60597 正在真实209内核FUSE执行；只读查询和原journal证明已完成初始S3读、目录/文件创建，正在随机写/追加/截断。`run-remaining-physical-v1.py` 原exec45864等待其PASS后按序执行phase5/6/7/8/9，均为one-shot，失败后只结清原身份，不重跑。新HA profiles已采集实际d639三节点；45153专用unit已与45151/45152隔离，double配置可容忍1zone。后续应核各原日志/receipt，不能同时再开相同controller。
- 本地顺序候选完整7检查software-v4-results.json均0（fmt/default/allfeatures/check/clippy/Python/diff），Clippy只有旧prepare_file8args。独立规范/需求审查v1发现可选客户端新建清单父目录落盘P2，按最小方案改为预先存在的持久父目录；新增实际RED8项1失败后GREEN8项全过，两轴v2均0，Rust未变。工具不新增服务依赖，docs/s3.md说明独立写入顺序及安全恢复；当前硬件仍d639，不能将新优化软件绿色算实带通过。接续需完成物理报告，包含旧入口404/null、原生reclaim关闭及无空scratch/跨库/现场物理状态等未验条件。

- 顺序候选现按本地提交固定来源以构建EL9普通release；尚未推送GitHub或部署。提交前最终Rust三项定向回归v5均0，生产Rust与双轴冻结v1逐字一致，Python仅已复核P2及对应8项GREEN，新增文档只说明已有功能。后续统一升级必须等当前d639原现场流水线结清、重新采集真实profile和14类空占用；不能把本地HEAD前进误作实带版本已变化。

- **最新现场结清与阻断修复**：本地顺序候选已提交 `a6f987f8a47e16380fd5ef9bf9407796faa7b2e1`，其EL9六产物已构建但**未部署**。d639实带FUSE在属性操作后原rename返回EIO（原controller exit1），服务器原操作 `1/c8b4b1dc-2580-490a-8a34-4b12e0e21d15` 三节点均Completed，客户端未保存回执且namespace_uncertain。真实/dev/fuse抓包证实内核先发root GETATTR(op3,ino1)被-5拒绝，恢复SETXATTR根本不可达；不是根据服务端Completed冒称客户端成功。已最小放行root getattr，仍新鲜验证根namespace/inode/type，其他路径及写屏障不变；实际lostreply回归RED101→GREEN0，uncertain根重建仍ESTALE。software-v6七检查均0，两轴fuse-root-review-v1均0；该修复还未实带GREEN。
- 旧FUSE已**正常umount退出**，原PID804628消失、transient unit自动回收、mountinfo无挂载；首次结清脚本对已回收unit再stop返回5，是harness生命周期误判，保留原日志。v2只确认已退出，按原journal `.../main1108-fuse-85cf1af9/journal-warm/rename-f13ad87f-70dc-4cb3-acf5-5d6cae90c2c2` 查询/结清同一Completed回执，未新建/重发rename，`acceptance/fuse-original-normal-settled-PASS.json`。旧phase4及remaining-physical-v1均已失败消费，**不能重跑**；phase5/6/7/8/9尚未开始。后续必须正常升级经过完整检查的新候选，按原日志接续FUSE剩余场景，再在同一候选上完成管理/S3/通知/HA/FDB故障/大对象顺序测量并出报告；不要以旧流水线仍运行作判断。原报告需求继续有效，A禁止，L84/L85有数据不格式化。

- **最新统一465现场及冷读修复**：46515ac普通EL9已正常整组升级207–209，`physical-final-46515ac/final-upgrade-COMPLETE.json`证明原FDB全部stable摘要保持/14类占用空、停服后PR-IN空。原12KiB清单同3对象实读SHA全对，暖GET5.287/5.138s且0 LOCATE/0 IP（旧43.192/43.354s、各12/6）；首读56.824s含MOVE/LOAD，不能作同条件加速比较。该候选phase1控制/池CRUD/两入口rescan重跑通过；phase2在首个历史GET发现首次cache捕获误用完整Full历史审计，实际逐代LOCATE十余分钟。已仅SIGTERM精确本地**只读**phase2 controller（无PUT/rename/delete ISSUED），未信号应用/设备；原all-controller已消费退出，不能重跑。新candidate的phase3–10/FUSE修复接续仍未执行。当前现场465而非d639，旧originalFUSE已正常卸载、原rename同身份结清。
- 冷读修复新增仅用于当前读的 `CapturedReadIndex`（完整DP/EOD/原SHA、DP/IP、全部必要Delta重放），保留公开 `CapturedIndexSource`完整历史审计和在用UID接口，原历史来源/回收/写入许可不变。70个独立Full后的实际owner私有FDB/Sim旧代码RED-v2 LOCATE85，修后GREEN-v2和最终software-v8私有long-full均0；原RED-v1是fixture60s许可过期（用同轮次正常续租纠正），GREEN-v1是误删在用方法编译失败（已逐字恢复），分别保留，不冒称产品红测。最终v8=7软件检查+4逐进程定向回归全部0，两轴long-full-review-v2均0；Clippy仅既有prepare_file8args。新修复待提交/EL9构建/从实际465升级并统一重验。不要部署已知冷读回退的旧a6或465产物当作最终通过。
- 现场最新预检发现207/208宿主有mmfsd，但应用进程mount namespace无GPFS，EE/ltfs进程0；不停止其他用户GPFS。进度报告 `/work/tmp/tape-main-physical-20261108-h9vn4yhb/validation-report-progress.md`明确进行中，最终报告须含优化/失败/未实现及物理条件不足项目。当前源/controller状态以同目录CURRENT.json、final-target.json及每stage原receipt为准。

# 此前交接：代码审查与 GitHub main 集成（2026-11-08）

用户最新要求为“review代码，通过后合入github主分支”。本轮只审查、修复阻断问题和集成已有代码，不继续开发新的 reclaim 能力。工作目录仍为 `/work/jay/tape-rs` 的 `main`；GitHub 是否已同步须按接续要求核对远端，不以本文内历史“未推送”记录判断。

## 本轮集成审查

- 固定审查起点为 GitHub main `0f2b465abb63e59b76919db9b1dd03be611825c7`，原候选 `184d0f59fa0901a293d3c0bfa38006ce61849b16`，共 29 个本地提交、62 个路径。规范与需求两轴独立全量审查；新增 P2 为父 journal/池 gate 缺失但 E 库占用残留时，`release_reserved_reclaim` 误报成功。
- 已通过真实隔离 FDB 负例复现，再增加只读原占用释放校验；原 slot/marker 任一残留拒绝且不清除或补回，完整合法后继允许旧释放重放并保留后继占用。修复再次独立审查，两轴均零剩余发现。证据及测试终态在 `/work/tmp/tape-main-review-20261108-l8sw3rht`；失败红测原日志保留。修复后 3 项相关隔离 FDB 回归及六项软件检查均通过，逐项退出码与日志 SHA 见 `fix-results-v1.json`；Clippy 仅既有 `prepare_file` 参数数量告警。
- 本批次同时含已在真实 B 库验证的同库任务互斥，以及尚未接入公开入口的原生 reclaim 内核。现场仍为 `fa7a2d7`，既有实带证据只覆盖其明确记录的互斥/读写/HA 场景；新 metadata 内核未部署、未完成实带验收。公开原生 reclaim 和源格式化继续封闭。实际修尾 actor、独立结清回执、空/损坏 IP 恢复、新 donor/replan、完整源覆盖与父流程仍未完成，合入开发主分支不代表这些功能已可用。
- 下文为此前逐片开发历史；保留原始提交、失败及验证范围，后续只在用户要求恢复开发时继续未完能力。A/B6070 已释放，不得占用；真实验证仅限授权 B605d 与指定 scratch 介质，须重新 preflight。

## 开发入口与待合内容

- 开发目录为 `/work/jay/tape-rs`，交付时使用 `main`。按 `AGENTS.md` 的接续步骤确认实际分支、提交和工作树状态。
- PR #39 已合入 GitHub main，合并提交 `591469170428c49ddfa4f93f9d7f969e78ac9aae`；其代码树与实带验证的 `f42c775292cdaee65221b1bc64aeeb0fb13e80d0` 相同。
- 原目录的大量差异是旧 WIP。额外 10 份源码和 4 份测试已复核，没有需要独立合入的正确修复；旧实现会削弱 main 的故障守卫、回执或测试覆盖。已直接采用 main，未归档旧源码。
- 已合入的旧 worktree 已删除。现仍只使用本目录的 `main` 工作树；保留分支 `integrate/historical-wip-20261106` 的 9 个提交已无冲突接回本地，尚未向 GitHub main 发布。保留原分支 `8caca7e2511a052c54745f90d787c7acb45f7896` 作为现有开发来源，不新建代码归档。
- 这 9 个提交属于未完成原生 reclaim 的依赖链，只有软件、Sim/private FDB 证据；完整父流程、逐对象覆盖和实带验收尚未闭合。同一 FDB 读版本输入已绑定原源实读、目标实读/退休回执和 b/d/g 账本；查询不创建或修复记录，也不授予 I/O 权限。
- 元数据固定计划从实际源/目标捕获流式核对路径和物理文件数，固定新 UID、路径、目标卷头及原属性/目标 extent 指纹。FDB 的 `RA`/`RW`/`RT` 记录和现有节点卷占用在同一事务取得；重复准备、查询、显式 HA 领取与未开写关闭均保留原计划，缺失占用不自动修复。后续新增 Prepared→Writing 持久准入：首次转换重核原证明、目标卷头、物理文件计数/配额及介质状态，重复或结果未确认后的重试仅返回原记录并要求观察；Writing 不得走准备关闭，接任仍保留原 Writing/计划/占用，原回执发现阶段冲突时拒绝。六项软件检查、准入与原生命周期两项隔离回归及双轴审查均通过，证据 `METADATA-ADMIT-*`；它没有设备执行或结果观测接线，不授 I/O 或格式化许可，未部署或通过实带验证。新增大测试分支在默认调试栈溢出后已用独立 boxed future 隔开，未改生产栈或 `RUST_MIN_STACK`；编译错误与栈失败原日志保留。首次准备内核的旧回执清理回归曾发现准备占用误挡历史验证，已拆开只读身份检查和写入准入，证据保留。
- 该准备内核已本地提交 `9b3ac375322b52d35d72ad4a46371a9f09972cf3`，六项软件检查及两项隔离 FDB/Sim 回归全部通过，规范与需求审查均为零发现；随后随互斥版本部署，未向 GitHub 发布。
- Writing 准入内核已本地提交 `ec476d4812e3663c66e1909d561791c9f118ee8a`，未部署。后续已增加计划 v2 的完整目标树摘要/计数及只读当前介质判定；复用磁盘索引路径字节序视图，逐对象流式核对。计划固定原构造和完整 delta 链，Full 判定须额外取得 Recorded 历史实读的完整身份，Delta/Unchanged 同样绑定完整链。原“只比较 Full 前驱位置”的 P2 经实际 Sim 反例复现（原进程 15799 退出 101），修复后三种 Full/Delta 组合通过；实际 Sim 正常提交与无关根属性变化拒绝、原准入及原生命周期四项隔离回归全部通过，六项软件检查及两轴审查零发现，证据 `METADATA-OBSERVE-*-v2.json`。原 Writing 与占用均不因判定而完成或释放。计划 v1 不加兼容分支，现场该类记录为空。该只读观察片已本地提交 `17670b0`，未部署。后续已给原 Writing 任务保存观测快照，允许 Unchanged→Committed 且拒绝回退；HA 保留原快照和占用，任务记录首版直接改为 v2，不设旧格式兼容。私有 124 字节编码绑定原计划并校验完整载荷 SHA-256；原缺校验的 P2 经真实 Sim 单字节损坏反例复现后修复，逐字节损坏、缺原记录、损坏持久行、旧 owner 及 HA 保留均已验证。六软件检查（`METADATA-CHECKPOINT-CHECKS-v3.json`，复用同候选 v2 的 fmt/default，另外四项重跑）、四隔离 FDB/Sim 回归及两轴审查通过。原 v1 两 controller 因修 P2 主动中断均退出 130；v2 全特性一轮因既有 HA 用例原 owner 主动让出而失败，原任务为 Indeterminate，具体是目录就绪还是多数派许可失效无法二分；原三节点目录与 Raft 日志已保留，定向、13 项并行 HA 及全套复跑均通过，未改变测试条件或生产逻辑。它仍不发布活动卷头、不结清任务、不释放占用，也未接设备 actor 或授权源带格式化。持久快照不能替代未来在新 owner/原任务守卫和 PR 下重新实读当前介质。
- 后续独立元数据设备执行原语已完成软件验证：由已选中/装入目标的串行执行线程调用；只有首次准入可写，重入仅观察原介质。逐 CDB 核对原 Writing/owner/route/Permission/PR；首次执行额外重核原输入/卷头/计数/配额/监控，源节点和目标树/固定引用均从实际捕获核对。无换带、fencing、尾部修复、活动卷发布、任务结清、释放或格式化，尚未接完整父任务或公开入口。审查 P1（最内层许可未强绑定）已由原始设备真实 Sim 反例复现（73396 exit101），修复后 37533 exit0；严格 execution_guard 绑定、wrapper 透明转发、raw/异许可实例零 I/O 拒绝及最后 PR-IN 后撤权均已验证。挂载使用传入 RecoveryBudget 完整审计，仍物化目标树，不宣称全流程恒定内存。新6项执行场景及4项原回归共10项隔离 FDB/Sim、六项软件检查和两轴审查均通过，原管线 47066 exit0，证据 `METADATA-ACTOR-*-v3.json`。初检缺私有 getter 编译错、v1 default 三项既有 HA 失败、v1 private 为修 P1 中断 130、v2 三个单元夹具缺 PR 守卫失败均保留；夹具实际 fence/设置卷守卫后定向与全套通过，原 HA 同一 ELF 并行13/串行13亦均通过，具体失效原因未唯一确认，未放宽超时或断言。该原语未部署或实带验收；下一片是新活动卷头/计数/历史与原任务终态、原占用的同事务发布。
- 元数据原子发布片已完成软件验证：稳定尝试/扫描身份、不能反序列化的本轮实读证明、独立历史回执与活动卷头/历史/计数/Committed 终态/精确自有占用释放同事务；并发、异常结果槽、回执损坏、后续其他占用、旧原任务/清单暂缺时历史独立读取、正文与新引用读回及 HA 原结果查询均通过。改变请求的终态重放漏洞由实际 FDB+Sim 反例 72310 exit101 复现，修复后 32815 exit0。11 项隔离回归、六软件检查和双轴审查均通过，证据 METADATA-PUBLICATION-PIPELINE-FINAL-v2.json；默认检查复用同生产 v1，其余五项 v2，私有11 v1，v2 仅把旧非法 tag7 测试改为8，原全特性失败和定向修复证据保留。Clippy 仅既有 prepare_file 参数数量告警。该片不代表全源覆盖、父任务完成或格式化授权；尚未部署或真实 B 库验收，GitHub 未推送。下一步须完成 Writing 未提交结果的本轮实读关闭/受控尾部恢复、保留旧尝试的新尝试、接任发现及完整父流程；Current 的 Unchanged 不能直接当关闭证据，尤其须排除 IP ahead。真实部署仍为 fa7a2d7，禁止沿用其旧 HA 结果声称新 metadata 验收。
- Writing 未写关闭片已完成软件验证：独立不可反序列化的本轮实读证明，仅在干净 DP、原完整树/卷头未变且 IP/DP 同代时生成；原请求/任务/owner/尝试/扫描、精确 RA/Qv、卷头/计数、无恢复及下一代三结果槽均核对后，Closed 与精确释放同事务，不推进卷头或源覆盖。并发/旧回执重放不清后续占用，旧 owner 证明不可重绑。IP 超前由当前捕获直接 RecoveryRestricted，普通 IP 债务保留 Unchanged 但无关闭证明；正向夹具在准备前单独检查点且 DP 原头保持。初次正向错误地沿用 IP 债务夹具未获证明；新增同步 checkpoint 嵌在大 async poll 后触发默认栈溢出，GDB 定位首次普通 write 的深恢复链、外层 poll 916656 字节。嵌套 Box 和单次 write Box 均未单独解决；最终将同步夹具移到独立函数并撤回无效 write Box，默认栈与断言不变，原失败/GDB/真正绿色证据保留。IP 超前测试的原成功预期也已修正为严格错误/原状态/零写检查，未放宽代码。13 项隔离及六软件与双轴0通过（完整 v7 原62150exit0；v8 仅等价 let-chain 清理，原30392exit0，重跑关闭/IP超前/发布3项及五软件，其他10项与default复用v7并逐行保留来源）；Clippy 仅既有 prepare_file 告警。证据 METADATA-UNWRITTEN-PIPELINE-FINAL-v8.json。该片未部署/实带验收/推GitHub，不能认为典型IP债务或T1/T2已结清；下一先保留旧尝试的新尝试身份、有界丢索引检查与接任发现，再完成同代IP/尾部恢复、实际新donor证明、全源覆盖及完整父流程。设计草案在本地 METADATA-NEXT-ATTEMPT-DESIGN-v1.md 与 METADATA-RECOVERY-NEXT-BOUNDARIES-v1.md，尚未实现；原生公开入口继续拒绝，源格式化仍封闭。
- 尝试历史与有界活跃检查点片已完成软件验证：RW 按 parent/源UID/attempt UUID 保留，RT 存16字节尝试ID，Record 首版v4固定 first/previous；首次 prepare 重放读取首次原记录，显式 retry 仅从 Closed 在原目标仍不变时创建唯一后继并保存 RU 边。RZ 用父上下文校验和及完整任务摘要绑定 last，点读发现缺失 RA/RT/Qv/RZ/RW 和阶段/观察/owner 回退，不扫描整个源清单；历史终态按原身份查询/领取不替换当前检查点，actor 仅核对原尝试。原 RA-only 发现反例64327exit101保留；多次默认栈失败及GDB定位后，将巨大夹具按正文/元数据分段并收束查询future，19584exit0，默认栈/断言未放宽。扩展 warmup 原11127exit0；冻结候选15项隔离 FDB/Sim及六软件检查全部通过，原管线36833exit0，规范与需求两轴审查均0；Clippy仅既有 prepare_file 参数数量告警。证据 METADATA-ATTEMPT-PIPELINE-FINAL-v1.json / METADATA-ATTEMPT-REVIEWS-v1.json。尚未部署/实带验收/推GitHub；下一片先固化原文件及已封存子请求的恢复定位与校验，再完成同代IP/尾部恢复、新donor、全源覆盖和Serving前完整父流程。设计 METADATA-EXECUTION-CONTEXT-PLAN-v1.md，公开原生入口与源格式化仍封闭。
- 原尝试执行上下文恢复片已完成软件验证：Record 首版v5固定原 ReclaimFile及sealed child的epoch/fingerprint，后继原样继承、RZ摘要与独立回执共同覆盖；点读恢复要求先领取原父/原尝试，同一读版本核验原child与完整input账本，不依赖walk current、不执行CDB。缺child/正文/目标/退休证明、匹配外层摘要后的改请求、旧owner/终态均拒绝且不补记录；后继与HA恢复原上下文已验证。15项隔离 FDB/Sim、1项新增纯单测、3次S3定向及六软件检查通过，两轴0；证据 METADATA-CONTEXT-PIPELINE-FINAL-v5.json，2项SDK及codec来源v4、13项SDK来源v3、3次S3及六软件v5，逐行保留来源，复用仅跨未变代码块的位置调整及独立S3测试变更。v1/v2历史回归默认栈溢出及三份GDB保留，单次writer Box只减少5344字节无效已撤回；最终在测试专用FnOnce函数中构造原七段async阶段，消除父poll为完整future临时值预留栈，不改生产栈、断言或顺序，原46962exit0且后续回归均过。v3 Clippy测试模块位置告警已按未变代码块挪序消除，无抑制；v4全特性S3桶管理因旧owner并非Leader收到明确not_leader失败，原日志/目录保留；测试创建池现沿用其DELETE的当前Leader/仅明确未受理重试策略，未知结果不重放，3次定向及完整集群55项均过。最终原30011exit0，Clippy仅既有prepare_file告警。未部署、未实带验收、未推GitHub；下一先核对IP落后是否真的需要额外checkpoint，草案 METADATA-IP-DEBT-CLOSURE-REVIEW-DRAFT-v1.md 尚未采纳；T1/T2 repair/replan、实际新donor、全覆盖及Serving前完整父流程仍待实现，设计 METADATA-REPAIR-REPLAN-NEXT-v1.md。当前同代IP关闭规则未改，公开原生入口与源格式化仍封闭。
- 正常IP债务的未写关闭片已完成软件验证：本轮guarded Current实读确认原DP构造/SHA、完整链、整树/UID/计数精确未变且完整IP有效不超前时，允许NoWrite证明保留旧IP债务，不额外写checkpoint、不代表IP同步或源覆盖。capture仍拒绝IP超前/冲突/截断，同代IP核整树/回指；metadata提交先DP后IP。实际普通写留下IP<DP的原59672exit101复现旧门槛阻塞，最小调整后同场景通过并发、原请求/结果槽/精确占用、重放与HA拒绝；实际Sim IP/DP对象及flushed、MAM/VCR、FDB计数/覆盖检查点不变。空IP仍无token并保留Writing/占用，actor只返回证明而不自动关闭。17项隔离 FDB/Sim和六软件、两轴独立安全审查均0通过，原73009exit0；Clippy仅既有prepare_file告警。证据 METADATA-IP-DEBT-PIPELINE-FINAL-v1.json / METADATA-IP-DEBT-REVIEWS-v1.json，原红测保留，选择依据 METADATA-IP-DEBT-DECISION-v1.md（验收以final pipeline为准）。未部署/实带验收/推GitHub；下一补只凭原parent发现Closed并恢复retry原参数，不能偷用fixture内scanned/request或假设walk.current，见 METADATA-CLOSED-RETRY-CONTEXT-GAP-v2.md。T1/T2修复/新donor/replan、全源覆盖及Serving前完整父流程仍未实现，公开原生入口和源格式化封闭。
- Closed后的parent-only接续片已完成软件验证：新增含终态的有界checkpoint事实查询，pending保留active-only；共享原claimed-task/context输入校验，专用Closed retry context要求显式领取并同读版本核原child/input，Prepared/Writing/Committed不能当Closed参数，执行context仍拒绝终态。无新格式/配置/设备权限。SDK在fresh NoWrite关闭及HA后，用新catalog和只接parent/owner的helper发现原Closed并还原UID/id/file/request，验证缺证明不补记录、并发唯一Prepared后继、历史Ctx不替换新RA/Qv/RZ、旧owner/已发布拒绝及零CDB/介质不变。18项隔离 FDB/Sim和六软件、两轴审查均0通过，原79745exit0；Clippy仅既有prepare_file告警。证据 METADATA-CLOSED-RESUME-PIPELINE-FINAL-v1.json / METADATA-CLOSED-RESUME-REVIEWS-v1.json。初次编辑脚本因两个同形claim片段断言退出，未写测试文件，随后限定NoWrite分支完成，非运行红测。未部署/实带验收/推GitHub。下一T1/T2：查得通用close_tail实际写两个根marker（abandonedBlocks及emptyRecovery），当前Plan/Observation未保存原target EOD，不能只相信修复marker自述范围；优先复用已验node deterministic discard的原source/PR/范围与树验证，但须先把metadata恢复意图和结果原子回执补足，设计 METADATA-DISCARD-REUSE-DESIGN-v1.md 尚未实现。新donor/replan、全源覆盖及Serving前父流程仍待完成，公开原生入口/源格式化封闭。
- 用户随后要求参考 EE 命令互斥，并明确“同一 library 内互斥，不同 library 可并行”。`767aea9c86df6d6c222913a988cb2b4067396d87` 已补库级持久占用及原任务索引，接 rescan、reclaim 父任务、unassign 与原子 tape set/offline；缺失或损坏时拒绝，不自动补锁。该片六项软件及八项隔离回归通过，两轴审查零发现。首次真实 B 库发现 S3 写入占用原因被误报；修复 `fa7a2d732fd6ce60d0eb68e5770d234aefdb7d0e` 在设备访问前检查原池占用，并保留实际准入重核。真实 RED/GREEN、六项软件及四项隔离回归、两轴审查均通过，限定现场路径与未覆盖项见 `docs/management-task-exclusion.md`。完整原生 reclaim 与未实现命令仍未开放，未向 GitHub 发布。
- 仍需接入设备执行、逐 CDB 原任务守卫、未知提交观察/恢复、关闭后的显式新尝试、完整属性与全源对象覆盖及真实 B 库验收。目标新代次须重新实读核验，不能仅用旧摘要代替；源 MD5 存在而正文副本未记 MD5 的情况仍保守拒绝。公共 FDB 原生 reclaim 入口继续拒绝执行，不能将当前准入内核视为已验收功能。
- 旧文档分支的 PDF 已在 main；旧 AGENTS/CONTEXT 已由现行管理及故障规范替代，不整体合回旧分支。
- 核对 Raft 初始化或成员替换方案时，读 [固定 raft-rs 0.7.0 源码研究](../../docs/research/raft-membership-bootstrap.md)。该文档区分源码事实与项目建议，不代表项目已实现成员变更。

## 真实环境与安全边界

现场操作前读取私有 `/root/jay/cicd/tape.md`，确认登录、实际设备、库存和当前服务状态。该文件含认证信息，保持本地，不写入仓库。

- 只允许 B605d 库参与已有授权实验；A/B6070 已释放给其他人，禁止操作。
- 可写实验介质为 `L00284L8`、`L00285L8`。保护 AA0000、8A0043、正式 8A0042/8A0047、AA0013 和清洁带。
- 最近已核实的三节点部署源码为 `fa7a2d732fd6ce60d0eb68e5770d234aefdb7d0e`，普通 release 配置，无 GPFS、测试故障环境或 `RUST_MIN_STACK`。
- 部署目录 `/opt/tape-rs/v1-20261107-fa7a2d732fd6`，`ltfsd` SHA256 为 `943e8ed8934ce72eaf0a52a23d36e3a0db5f90558b15ca77e63b8f8aa9e6059b`。
- 独立 FDB 集群使用 45153；保留原 namespace、state、服务单元和端口。不得碰 45151/45152，不清除原任务门闩或设备预约来制造就绪。
- 已消费的实测 controller 不重跑。未知副作用保留原请求及检查点，按项目恢复流程核对；格式化、擦除沿用显式确认门槛。
- 两轮占用父任务均已按原 handle 在零子任务状态安全关闭，原排队请求均终态。首次 `accept-exclusion-v1.py` 因占用原因被掩盖退出 1，证据 `EXCLUSION-V1-ORIGINAL-SETTLEMENT-v1.json`。修复后 `accept-exclusion-upload-v1.py` 的严格 PoolBusy、引用保护和原任务回执检查通过，但脚本误要求专用 rescan 终态重放为 200 而退出 1；现有路由/测试要求 202，原响应正文与原终态一致。只读补验 `settle-exclusion-upload-v1.py` 原进程退出 0，核实新旧正文、被拒写对象 404、原回执未变及 14 类占用为空。总证据 `EXCLUSION-UPLOAD-ACCEPTANCE-SETTLEMENT-v1.json`。L85 同池同卷、在线且非黑名单，最新修订号为 3。没有重新提交管理操作或 PUT，没有 FORMAT/ERASE；不把原失败进程改记为成功。后续不得重跑这些已消费脚本。
- 后续正常 HA 实测仍使用普通 `fa7a2d7`：207/node1/round701278 正常停止，208/node2/round727643 接管。executor 在 Serving 前已按原身份领取父任务；E/Rp/原索引字节保持，父记录仅更新 owner，三个原请求仍 queued。旧 owner 查询/释放被拒绝，原零子任务按原 handle 关闭后，两个 rescan 成功、unassign 被引用保护拒绝；207 正常加入，三节点就绪，新旧正文及原回执不变，14 类占用为空。两个私有测试 helper 均已退出。原 `accept-v2.py` 误要求新节点尚未领取原父而退出 1；`resume-original-v2.py` 按原操作续接退出 0，没有新建父任务或管理请求。证据 `ha-exclusion-20261107-v1/recovery-v2/COMPLETE.json`，详细边界见互斥规范；不重跑这些已消费脚本，不将其当作复制中或元数据准备任务的 HA 验收。

## 验证证据与已知限制

此前清理只整理源码和文档，没有部署或设备动作。恢复开发后的新结果须另行记录，不能沿用旧验收宣称新代码通过。

- PR #39：完整与增量审查问题已关闭，六项软件检查及相关真实 B 库 S3/FUSE rename 回归通过，FUSE 正常卸载，最终 12 类任务门闩为空；该轮没有 FORMAT/ERASE。
- READ/WRITE 后取消的精确故障边界由 SimLibrary 加隔离 FDB 验证；不把它描述为已在实带注入同一故障。
- 历史物理矩阵 24 项通过，各自绑定实际源码版本；不能称为最终候选统一重跑，完整原生 reclaim 未验收。
- 已有 owner 定向用例在默认 debug 小栈下溢出，基线 `6dbbd808` 同条件也复现。8 MiB 调试回归通过；正常 release 实带验证未扩大生产栈。
- 恢复开发的本地集成提交为 `a07768f`，保留了 PR #39 的 rename 原任务守卫。旧本地测试 FDB 35127 连续超时，原失败日志保留，未重启或清理旧库。后建的隔离 FDB 43843 又因所在文件系统余量低于 5% 触发 Ratekeeper Reason7，普通事务超时；仅该实例正常停止、冷迁移原数据到余量充足的本地磁盘后，同 cluster 文件/端口的最小读及原 HA 回归通过，未改保护阈值或真实集群。旧数据和失败证据保留，当前 PID、argv、数据目录见 `/work/tmp/tape-reclaim-resume-20261107/ha-exclusion-20261107-v1/PRIVATE-FDB-RELOCATED-v1.json`；原 `PRIVATE-FDB-v1.json` 只记录迁移前实例。不要沿用旧 PID 或清理其他项目的数据。
- 新查询的源卷头篡改用例已实际先红后绿；初次测试代码编译错误与真实断言失败分别保留。最新源码、审查和检查结果查本地 `/work/tmp/tape-reclaim-resume-20261107`；后续互斥部署与部分实测不代表完整原生 reclaim 通过。
- 原始记录在本地 `/work/tmp/tape-wip-integration-evidence-20261106`、`/work/tmp/tape-wip-independent-b-20261106`，核心入口为前者 `PR39-MERGED-v1.json`、`PR39-MERGE-ACCEPTANCE-v1.json`。现场脚本和失败日志保留原位，不作为待合入产品源码。

查证历史或恢复现场上下文时，再读本地 `.scratch/ltfs-ha/local/handoff-history.md`。本机 `.git/info/exclude` 排除了现场证据、CodeGraph 索引和 EE 参考输入；它不隐藏 `src/` 或 `tests/` 的产品改动。

## 2026-11-08 原目标 DP EOD 边界片（软件验证完成）

Plan v3 固定原实际 DP EOD 并纳入摘要/边界校验，Unchanged/首次新挂载暂存要求精确原边界，Committed 必须原 EOD+1 起写。实际 Sim 同 XML/SHA/全链但记录重新分段的反例已复现，原 2829 测试 exit101；初次 61738 编译缺 PartialEq 另记，非逻辑红测。候选未部署、未实带验收或推送；T1/T2 修尾、新 donor/replan、全源覆盖和完整父流程仍待完成。19 项隔离 FDB/Sim、11 项观察编码测试及六软件检查全部通过，原管线 46571 exit0；双轴复审0。完整结果及日志 SHA 见 METADATA-EOD-PIPELINE-FINAL-v3.json；Clippy 仅既有 prepare_file 参数数量告警。

本片两轴初审 Standards0 / Spec1：持久观察解码未同步原 EOD+1，正确 checksum/错误位置的三种 Full/delta 红测实际10504 exit101；现同步 codec，原全套 v3 已重验通过。v1管线13324为测试 Vec/整数比较编译错；v2新物理分段回归0后为修P2精确中断自有闭后恢复测试，SDK子进程-2、controller1，原日志保留。codec红测首次56524是Cargo过滤器参数错误，不算逻辑红测。以上不涉及现场。

本片最终只更新本交接的检查事实，生产/测试/管理规范与实际 v3 执行候选逐字相同；最终源码清单 METADATA-EOD-SOURCE-v4.json。下一明确恢复 phase/guard 生命周期、两个 marker/原闭合边界核对和独立恢复回执设计见本地 METADATA-TAIL-IMPLEMENTATION-BOUNDARIES-v2.json（设计未实现），完整父流程和真实 B 验收仍未完成，禁止据此开放原生 reclaim 或格式化。

## 2026-11-08 修尾结果只读识别片（软件验证完成）

8be83c2 后新增独立 Discard fact：完整 Current 新Full +原 Recorded/完整链及实际构造结束块，原EOD到新Full起点-2的范围、原前缀和emptyRecovery双标记、原树/UID/count/donor/关键header均核对。原根属性与单对象流式核对，不发CDB、不修改持久任务、不授IO，不自动close_tail或发布/释放；显式Recovering/Recovered、独立回执及完整父流程仍待实现。历史捕获从原SHA验证读取额外返回exclusive construct end，共享底层读取不增加CDB、不改变原value-only调用方语义。实际历史索引重分段+已放弃记录删除保持新/旧XML身份和新Full位置不变，原33451测试子进程三测中边界负例exit101，修后4666测试子进程三测exit0；首轮34820夹具使用绝对链接被既有卷根约束拒绝，已改为相对链接，不算逻辑红测。实际执行的 19 项 SDK、14 项观察单测和六软件检查全部通过，双轴静态审查0；原48449管线0与77828补测controller0。管线v1过滤器误将dp_eod改为dp_discard，原exit0/0tests行明确排除，补跑正确framing一次0；METADATA-DISCARD-PIPELINE-FINAL-v2.json逐项绑定26份成功日志，原错误行另列。Clippy仅旧prepare_file参数数量告警。未部署/实带/推送，现场仍normalfa7，A禁止。

本片最终仅本交接检查事实晚于实际v1执行，生产/测试/管理规范逐字相同，源码清单METADATA-DISCARD-SOURCE-v3.json；读取核心字节保持证明READ-CORE-PROOF-v1。下一片显式原T1/T2前提与恢复阶段/actor/独立回执，另有IP空/截断的受控checkpoint边界必须补齐，详见本地METADATA-TAIL-IMPLEMENTATION-BOUNDARIES-v4.json（设计未实现）。严格Current证明不放宽，完整native/源覆盖/父流程/真实B验收仍未完成。

## 2026-11-08 显式 Discarding 前提与意图（历史预验记录，见下方正式结果）

HEAD ffe86417327a07c3dd0f7cae66fb9dddeb1aafcf。九个Rust路径未提交：共享原Recorded/end/chain/tree核对、fresh PR/full-budget T1/T2前提Token、Discarding phase以及原RW/RZ原子意图；RA/Qv保持，不执行close_tail，不授写入或释放。普通metadata准入/观察/准备关闭/重试拒绝Discarding，当前持有者可恢复原context；cleanup不再占新file quota但原目标head/count/scan/media/pool/input仍检查。生产check0；新单测及T2/T1预验原75718正在跑，先wait，禁止重复启动或改其冻结Rust路径。详情 METADATA-DISCARD-INTENT-WORKING-STATE-v1.json；尚未全检查/审查/部署/实带/push。原56981编译String模式错误与9892的T2定位结清旧buffer引起flushed11->13原失败保留；v3明确稳定持久T1/T2夹具，定位可让已在设备中的缓冲落盘，非新append；不隐瞒该潜在副作用。下一全套21SDK/14观察/六软件与双轴后才提交；actual repair actor/独立回执、IP空/截断checkpoint和fullparent仍未实现。

上述75718预验最终unit3/T2/T1均0，尚未全套。新增真实旧NoWrite token跨Discarding拒绝回归后，当前17967在跑v4四项预验（old-token/unit3/T2/T1）；先wait别重复，冻结Rust源见PRECHECK-SOURCE-v4。下一完整矩阵为22SDK/14观察单测/六软件，状态见METADATA-DISCARD-INTENT-WORKING-STATE-v2.json。代码还未提交/部署/实带，actual修尾actor/回执仍待。

## 2026-11-08 Discarding 正式候选（软件验证完成）

前述17967 v4预验 old-token/unit3/T2/T1 四子进程全0。已冻结正式11路径候选 METADATA-DISCARD-INTENT-CANDIDATE-v1.json，基线ffe8641；原4652管线实际22SDK、14观察单测与六软件全部exit0，29份成功日志与源码SHA绑定见METADATA-DISCARD-INTENT-PIPELINE-FINAL-v1.json；无零用例或复用旧候选。新增phase不变更Task字段或增兼容分支；token仅授权保存意图，非IO许可；同phase RW/RZ原子更新/原RAQv保持，oldNoWrite token跨phase释放被拒绝。原编译错误与T2读定位结清旧buffer的flushed差异保留，稳定持久tail夹具不是生产修复，也非新逻辑红测。双轴静态审查0；最终复核与提交结果查METADATA-DISCARD-INTENT-REVIEWS-v2.json及COMMITTED-v2.json；尚未部署/实带/push。actual修尾actor、结果回执与正常结果恢复(不能放宽旧token用途)、IP空/截断checkpoint、fullparent仍缺，设计见METADATA-TAIL-IMPLEMENTATION-BOUNDARIES-v5.json。

本片仅此交接事实收尾晚于实际执行候选，全部Rust/测试/管理规范逐字相同，最终源码清单METADATA-DISCARD-INTENT-SOURCE-v2.json。全部预验与正式controller均已退出，不重复启动。Clippy仅旧prepare_file参数数量告警；正常target策略原body保持证明TARGET-POLICY-PROOF-v1。原编译/稳定夹具失败日志保留，不冒称产品红测；旧未写证明跨phase的真实拒绝已在22SDK矩阵中验证。下一修尾执行器、用途/phase绑定的新鲜结果证明与独立恢复回执，必须先辨实际正常Commit/NoWrite/Discard结果再决定IO；IP空/损坏checkpoint及fullparent仍待，不能放宽旧NoWrite token的关闭门槛。设计METADATA-TAIL-IMPLEMENTATION-BOUNDARIES-v5.json未实现，public native/源format继续关闭，真实B验收未完成。

## 2026-11-25 带库交还 review 修复进行中

工作树 main HEAD 4e8cc0c，包含大量既有未提交改动；未提交、未推送。恢复原公共 S3 接口及其集成测试；交还重试开始时清除旧诊断，避免并发重放将正在执行的重试重新开放；交还准入事务将活动路由升级为 v2，使未预先读取路由的旧版节点拒绝访问，v2 保留到重新加载后。该路由变更不可逆兼容旧版，见 docs/reference/library-handoff.md。

软件检查：默认 cargo test、cargo check --all-targets --all-features、cargo clippy --all-targets --all-features 通过；FDB lib 878 通过/263 忽略，两个隔离 FDB 测试分别通过，S3 两个集群测试通过。fmt 全仓检查仍有历史差异；git diff --check 通过。日志 /work/tmp/handoff-fix-{default,check,clippy,fmt}.log 和 /work/tmp/handoff-fix-*.log。私有测试 FDB 已停止；没有部署本轮修改或重新访问真实带库。

review 未通过：executor_library_handoff::reconcile 在其他节点可能仅根据预留缺失就报告 Released，尚无原执行者及未知结果的副作用已结束的证明；旧版节点在 v2 原子切换前完成预检后延迟执行的窗口也没有充分证明，且升级前已存在的交还状态未必具有 v2 路由。必须先设计并验证跨节点执行终止证明、崩溃和未知结果恢复及混版切换，再开展完整工作树 review 和现场验收，不得合并。FDB 测试仅证明新读者可识别 v2、旧解码拒绝 v2，不证明消除预检竞态。

### 后续修复（仍未完成）

隔离 FDB/Sim 回归分别实际复现：另一个节点在原交还执行中看到空预留就记录 Released（/work/tmp/handoff-reconcile-red2.log）；以及交还结果未知后同一节点接管自动重发并记录 Released（/work/tmp/handoff-retry-red.log）。已禁止这两种自动推进及 HTTP 重放重置重试标志；原交还成功且回执保存失败现在会停留在 Releasing，不能自动恢复。新回归在模拟夹具中手动完成交还仅为了继续测试后续加载，不是生产恢复机制。补上结果未知后的可靠执行终止证明和安全恢复入口前，功能不能用于真实交还。旧节点预检竞态及历史 v1 交还仍未解决，完整工作树未 review、未提交/部署/推送。

最终软件验证：包含重复 HTTP 请求断言的隔离 FDB/Sim 回归通过（/work/tmp/handoff-retry-final.log）；FDB lib 878 通过、263 忽略（/work/tmp/handoff-retry-lib.log），check all targets/all features 与 clippy 通过，后者 105 warnings；全仓 fmt 仍有历史差异，本轮新触及位置按 fmt 修正；git diff --check 通过。测试实例 FDB 已关闭，本轮未接触真实设备。测试、check、clippy 后只有本轮两处空白格式调整与此交接记录发生变化。

### 回执保存重试、旧路由保护及加载结果未知处理

原交还执行器结束设备操作并释放句柄后保留结果，最多尝试六次保存同一 FDB 回执。持久化失败期间不再访问设备。FDB 准入或执行权无法确认时关闭本地执行权，回查使用原 UUID。补写 Released 时即使当前状态已进入重新加载也可核实原回执。已有 v1 路由的历史交还仍可查询，新版拒绝据此推进加载。文档规定首次交还前在维护窗口停止旧版执行器并确认提交的设备命令结束，路由版本只阻止之后发起的旧版预检。

加载时传输或许可异常保留本次 attempt_key，自动接管与重送不再续接未结清的尝试。明确未执行或已核实清理时仍可重送同一加载 UUID。模拟第二条 PR OUT 报 host error 后首条注册仍存在，隔离 FDB 验证保留 attempt_key 的 Blocked 回执保持原样。原执行成功但 FDB 写入超时的测试用独立不可达 FDB 地址，重写相同回执成功且设备命令计数不增加。

最终隔离 FDB 两项（SDK/Sim 与旧路由）及 FDB lib 879 通过、263 忽略，日志 /work/tmp/handoff-reload-final-{sdk,route,lib}.log；默认 cargo test 通过（/work/tmp/handoff-receipt-final-default.log）；全功能 check 与 clippy 通过（/work/tmp/handoff-reload-final-{check,clippy}.log），Clippy 仍有既有告警。全仓 fmt 108 处既有差异，触及的独立交还文件格式检查及 git diff --check 通过。私有测试 FDB 已停止。

尚未通过完整工作树 review，未提交、未部署、未访问真实带库或推送。原执行进程退出且没有持久回执时没有可信的自动恢复入口。首次交还的停机/排空旧版命令前提没有本次现场验证。GitHub API 读到远程 main 085c5470b6c4f72c1eea86f1d815c4eb96b3d24a，SSH 22/443 连接关闭、HTTPS Git TLS 握手失败。工作树仍含大量既有回收及迁移改动，按用户要求合入之前须整体 review。

### 离线启用、接管互斥和回执重启恢复

首次交还须先停机和排空旧设备命令，再用 ltfsd 的 `--enable-library-handoff-offline` 和 `--handoff-namespace` 启用指定 FDB 路由。入口持有本地数据目录锁，Raft 端口可连接、检查超时或地址不明确时拒绝。启用动作保留 namespace、路由身份和目录，v1 路由不再通过普通交还升级。旧进程及已提交命令排空仍须现场核实。已有 v1 未完成交还只保留原回执查询。升级后的普通接管在 PR 命令前持久化代次及 UUID 互斥记录，明确结束后结清，交还和未结清接管不能并存；未知接管可在现场停机排空后用原离线启用命令清理。

已确认的交还在 FDB 回执前原子同步写入原节点的库数据目录，重启可用原操作 CAS 补写，过程不访问设备。进程在证明落盘前退出时，可在现场停机与排空后加 `--verify-release-operation` 只读核对原设备及预留，缺少匹配的原意图或仍有本集群注册则拒绝。正常命令、状态和两种离线操作见 docs/reference/library-handoff.md。

最终软件验证默认 cargo test、check all-targets/all-features、clippy all-targets/all-features、FDB lib 879 通过/263 忽略、两项隔离 FDB/Sim 和 CLI 离线参数测试通过。最终日志 /work/tmp/handoff-offline-verify-{fdb,check,clippy,fmt}.log 及 /work/tmp/handoff-offline-final-default.log。全仓 fmt 既有 108 处差异，本轮改动的最后一处测试换行已单独格式化，git diff --check 通过。测试 FDB 已停止。

完整工作树 review 仍被既有 raw 回收用例阻塞。默认调试栈下溢出，32 MiB 测试线程栈首次重跑因模拟许可过期失败。该不换主的夹具已复用现有心跳，生产许可和性能阈值没有调整。随后两次完成回收但 advance 阶段分别 188.158643 和 192.807368 秒，超过 180 秒限制（/work/tmp/handoff-review-raw-{fixture,final}.log）。完整工作树未提交或合入。GitHub main API 再次核为 085c5470b6c4f72c1eea86f1d815c4eb96b3d24a，SSH ls-remote 一次通过、后续偶发关闭。仅对 B 库 207–209 读就绪状态（均 200）及 207 库存（可用、8 项），未部署本轮候选或发送真实设备命令。

### 性能改为告警及交还并发复核

按用户要求，raw 回收用例的 180 秒目标暂时只告警，耗时仍记录，其他断言原样保留。隔离 FDB/Sim 重跑通过，advance 阶段 196.075896 秒，告警正常，主读 1 次、物理读 5 次（4 次审计跨距），目标冷读、覆盖和 FORMAT 校验通过。日志 `/work/tmp/handoff-review-raw-advisory.log`。上方因性能阻塞的结论已解除。

同时关闭成功隔离后清除接管记录至 FDB owner epoch 更新的间隙。只有已确认隔离成功才在结清原接管记录的同一事务内推进代次，明确失败保持原代次。隔离 FDB 回归覆盖两种结果且验证旧代次交还被拒绝。两项隔离回归、FDB lib 879 通过/263 忽略、全功能 check 与 clippy 通过（`/work/tmp/handoff-owner-atomic-{fdb,check,clippy}.log`），全仓 fmt 仍有 108 处既有差异，git diff --check 通过。私有 FDB 已关闭，本轮未操作或部署真实带库。远端主分支仍读到 085c5470b6c4f72c1eea86f1d815c4eb96b3d24a，完整工作树复审与当前候选实带验收仍待完成，未提交或合入。

### B 库现场验收与可复用部署技能

本地安装 `$tape-rs-b-field`（`/root/.codex/skills/tape-rs-b-field/SKILL.md`），包含 EL9 源码冻结与构建脚本、限定 207–209 的 SSH 助手和现场部署步骤。原候选由技能重建后源码清单及三份二进制摘要一致。技能格式检查和三台主机登录通过。技能与仓库未写入提供的凭据。

最终候选源码清单 `2443f08cbdd9d91be32ddea508555a82289d18e538b0917ebcf9a7f1e77b344c`，精确源码及证据 `/work/tmp/tape-b-field-fds7_2vg`，部署路径 `/opt/tape-rs/library-handoff-final-2443f08cbdd9/bin/ltfsd`，摘要 `4419ee1f6dfc8b8b5bd57cf0ef5541d04ac65770d49dc786192c20f7f3c1fa70`。B 库三节点正常停机、设备请求与完成计数相等且稳定，再切换同构版本。库序列号 55L3A78011W9LL01，namespace、设备配置及原数据目录沿用。首次切换的离线启用成功证据见 `/work/tmp/tape-handoff-merge-_xh_au8i/offline-enable.log`。中间一次即时 MainPID 可执行文件断言在包装进程尚未 exec 时失败，稍后逐节点确认候选路径及就绪，未重复启动。

最终候选使用 CLI 交还、HTTP 原操作查询与加载、CLI 同 UUID 重试。驱动器 117018605B 的 L00285L8 原位保留，三台 B 设备的预留均释放。外部测试进程取得三个预留，加载停在 blocked，外部键保持完整。注销后原加载 online，新 owner epoch 982011，三节点 readyz 200，64 个文件清单与交还前一致，原 1 MiB 文件前后逐字节一致，SHA256 `1084b67ee9743ae18c2523b389fd049a58259c8054e215bd8f665dd337e38e4b`。无介质初始化或写入测试。首次验收直接 inventory 的默认扫描额外探测了 /dev/sg0、/dev/sg1，发现后改用 `--no-drive-scan`，最终验收只向已验证的 B 路径直接发送命令。离线核验代码另加 sysfs VPD/FC 端口筛选，附截断 VPD 回归。

软件：880 项 FDB lib 通过，263 忽略；两项交还隔离回归、全功能 check 与 Clippy 通过。隔离 pinned FDB 的 group-only 准入、普通模式兼容、两文件完整 lineage/FORMAT 三项通过。默认并行 HA 在构建并行时有 7 项许可过期失败，单独串行重跑 13 项全过；全套串行日志 `/work/tmp/tape-b-field-fds7_2vg/default-serial.log`。全树 fmt 仍有原差异。独立 Git 索引冻结全范围，Git diff --check 通过。SSH GitHub 和 HTTPS 偶发连接失败，合入前核对远端真实 head 与完整验证结果。

`cargo test -- --test-threads=1` 完整通过，包含 HA、S3、多库和文档测试。复审按仓库规范与用户需求两条线进行，现有工具没有独立子代理。重点核对 PR/机械命令边界、FDB 原意图及代次互斥、SDK/CLI/HTTP 回执、旧路由兼容和回收上线门槛。当前已核对项无新增合入阻断。此段交接晚于现场源码冻结，产品源码及测试与已验收候选一致。

### release/acquire 接口改名

CLI 使用 `library release` 和 `library acquire`，SDK 使用 `release_library` 和 `acquire_library`。HTTP handoff PUT 请求体必须使用 `{"action":"release"}` 或 `{"action":"acquire","release":"<UUID>"}`。按用户要求不保留旧 CLI、SDK 和 HTTP 请求兼容。FDB 内持久化回执结构不变，新旧客户端与服务端须同步更新。

隔离 FDB/Sim 的 SDK、HTTP 交还及外部预留阻塞/取得端到端回归通过，独立 FDB 已停止。CLI 参数和 HTTP 格式边界测试通过，全功能 check 和 Clippy 通过，既有告警与格式差异仍存在。

EL9 候选 `/work/tmp/tape-b-field-zldckprw`，源码清单摘要 `eb41dddd1997135e4f86fbbd95a29e77096c8249d049746eb1b5973e0f2f82a5`，三节点当前程序 `/opt/tape-rs/library-handoff-actions-eb41dddd1997/bin/ltfsd`，摘要 `07854878872ef1ded1c19ca80f16e3f6626e657f6eeb4040fc6a573f029f339d`。服务切换前停止三节点，B 设备提交与完成计数两轮相等且未变化，重启后三节点就绪。实带新 CLI 交还 `114adbc0-1f90-4d24-a816-8696c05c425b` 到 released，取得 `ae827d6b-fa23-4570-908c-a0b10429636d` 到 online，代次 1007537。64 文件清单不变，1 MiB 原文件复读 SHA256 `1084b67ee9743ae18c2523b389fd049a58259c8054e215bd8f665dd337e38e4b`。本次交还前两驱均为空，已载带交还仍以此前候选验收及本次 Sim 回归为准。

`cargo test -- --test-threads=1` 全套通过，日志 `/tmp/tape-release-acquire-test.log`。
