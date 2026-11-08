# 当前交接：继续原生 reclaim 开发（2026-11-08）

本地工作树清理已完成。用户随后明确要求“继续开发”，现恢复原生 reclaim 的剩余实现、故障验证与真实 B 库验收；沿用此前管理模型和设备安全边界。

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
