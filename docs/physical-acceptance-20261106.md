# 2026-11-06 真实带库验收矩阵

本表是版本化证据索引，不表示所有功能全部通过。核查基点为GitHub main `2b70b883`，实际三物理节点部署源码 `128e2c38` 与其产品源码、测试及依赖逐字一致；本片只增加默认忽略的硬件测试与文档。早期采用squash合入的现场SHA，已对照对应主线落点的src/tests/examples/Cargo确认，不能仅凭SHA不是祖先否定或确认现场结果。

所有后续已合入功能都需真实带库验证。软件、物理主机上的Sim、真实SCSI、FDB真实故障、EE调研分别记录，不相互替代。结果只覆盖指定版本和场景；后续改动需重验受影响路径。

现场范围仅B库 `55L3A78011W9LL01`、主FC域 `0x5000e1117018605d`。A库已借出、B备用6070域禁止访问。登录凭据仅保存在受限环境记录，不写入本表。

## 已有实物证据

下列证据目录均位于 `/work/tmp/`，FINAL是索引，须连同引用的原请求、回执、日志及哈希使用。

| 能力 | 现场源码 / 对应主线落点 | 已证明范围及证据 |
| --- | --- | --- |
| 三节点原生初始化屏障 | d72d53d5 / PR28 | 三个物理端点证明、新45152及namespace初始化；采集阶段无设备权。tape-native-barrier-physical-20261105/FINAL.json |
| 持久通知与重连/扩大类型 | d72d53d5 / PR28 | 真实连接、计划owner停止、旧积压与新类型future-only、原pending/generation/ACK保护；此批明确使用注入事件。tape-native-notify-ha-20261105/FINAL.json |
| pool及异步管理任务CLI/HTTP | a13dffdd / b8df921 | pool CRUD/set、排队与取消、坏202后同请求恢复、跨owner、实际FDB多数派故障。tape-fdb-pool-api-physical-20261105/FINAL.json |
| 完整库存及真实事件生产 | 9b7d4a54 / f185ddd | B八盒/两驱动，周期去重、非注入首次事件、SSE及FDB故障后的保留。tape-native-library-observations-physical-20261106/FINAL.json |
| owner接管前置FDB核验 | 9003d1ee / 3806a4f | 实际停止两个FDB server，前检失败且三设备PR在故障窗口保持不变。tape-fdb-owner-preflight-physical-20261106/FINAL.json |
| library/tape list/show | 954098a3 / 2f7e768 | 真实库存、nodes/active_node、CLI表格/JSON及HTTP分页排序；FDB故障不伪报成功。assigned/offline/corrupt合成行不算物理故障。tape-native-library-tape-queries-physical-20261106/FINAL.json |
| 异步rescan | 4e2e6343 / 62c20cc | 完整库存、CLI/HTTP同原任务、取消、坏回执、跨owner及FDB恢复。tape-native-library-rescan-physical-20261106/FINAL.json |
| 部分库存拒绝 | 9bb17897 / 15acf27 | 真实GOOD回复的截断/缺元素拒绝，2344字节43元素完整通过；新版周期采样/rescan。tape-complete-changer-status-physical-20261106/FINAL.json |
| 异步tape move | b3432f5d / 4555b85 | storage/I-E往返、已分配带归槽、原计划跨owner、queued取消、真实FDB故障及历史回执保护。tape-native-media-move-physical-20261106/FINAL.json |
| force assign及大索引nonforce拒绝 | bf555de8、128e2c38 / PR37 | AA42强制格式化/分配、提交前FDB故障恢复及完成后切换；AA43大索引同原任务从实际OOM恢复并安全拒绝，原4GiB限额未提高。不是执行中FORMAT接管。tape-native-media-assign-bounded-physical-20261106/FINAL.json |
| **nonforce空LTFS正例（本轮补齐）** | 128e2c38 / 2b70b883产品路径 | 实际CLI/HTTP以force=false分配AA47，三节点原终态/归属一致，归1005且未导入索引；原67任务字节保持，总69。tape-format-safety-evidence-20261106/main-empty-positive/COMPLETE.json |
| **回收completed回执窗口（本轮补齐）** | b9a66cb测试 / 2b70b883生产函数 | 真实finish_reclaim一次FORMAT GOOD；原回执留置时50次drive_reclaim零新CDB，随后应用原完成命令并释放状态、归槽。tape-legacy-reclaim-evidence-20261106/PHYSICAL-RESULT.json及POST-LEGACY.json。独立operator/SQLite阶段夹具，不证明迁移全过程或Raft HA。 |

本轮回收结束时原三服务已恢复；AA0000未分配/home1002，新空卷72041139-dc2f-40e4-b7db-cf16612cfb22；AA47归lto8/home1005、卷cadebf8a-71fb-4df1-ab96-391440073b44；AA42原卷4981a448-7866-4c86-80ab-12350bc1a0e8/rev1/home1007保持，AA43和HH8中的AA0013保持。原69个任务及AA42/47归属原始字节不变，queue/gate为空；两原消费者ACK210保持，事件head227，新增事件保留。独立核对owner1/518242及三设备PR/FDB一致。此状态是该次采样，后续现场操作须重新核实。

## 继续保留的验收项

- 历史SQLite、VIP、S3、FUSE、多库调度的全部命令尚未逐一重建当前主线的现场版本清单。旧报告只证明旧版本；需要核对仍保留的入口及受影响路径，不能宣布全部main已逐功能通过。
- 多库分散owner与跨库行为须待A重新可用；不得为补测操作借出的A。B单库结果不能替代多库验收。
- 已分配并导入真实文件后的读/运维互锁、当前严格模型/codec的非空FDB内容链路需重新验证。AA42/47的index_imported=false，不代表可用S3/FUSE读写；旧2026-10-27候选非空读结果不能自动用于新模型。
- FORMAT/UNLOAD精确回执丢失、阶段提交未知、进程崩溃、断电及物理介质错误，按实际覆盖逐项验收。计划停机、POST发送前FDB故障或上述operator回执窗口不能覆盖这些场景。
- 写保护拨片、满带及永久坏带不能仅凭软件注入称实测。无人现场配合时保留未完成，禁止损坏真实介质制造结果。

## 不属于已交付主线功能的候选

原生active import/unassign、更完整EE式动作、统一FDB的S3/FUSE writer、单盘TapeAlert/MAM健康生产者、完整初始化写管理入口及大规模灾备仍需按对应实现片验收。现存底层原语、旧接口、事件注入或EEADM实验，不代表新原生CLI/HTTP链路已完成。

本轮另在较早WIP快照上修复全分区空白证明、逐CDB FDB资格检查及未知结果分类，候选源码82ec54c已通过真实B实验，证据tape-format-safety-evidence-20261106/PHYSICAL-RESULT.json。该候选包含尚未集成的工作，不能把其operator/私有FDB实验当作当前主线新功能已发布；历史308文件WIP和44处集成冲突仍未由本测试片放行。
