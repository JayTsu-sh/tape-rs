# EE 与 tape-rs 差距现场审计（2026-10-21）

本轮以现场 **IBM Storage Archive EE 1.3.4.0 (53233)** 为运行基准，以当前 `main / 64cbfb8` 加已有未提交实现为项目基准。EE 1.3.5 文档用于检查覆盖范围；新增版本能力不计作现场已确认差距。没有修改生产 Rust 源码。

**结论口径：7 项差距由本轮物理环境运行确认，另 1 项兼容性限制复用既有物理双方对照。其余能力按覆盖边界逐项记录，未验证项目不称为已确认 GAP，也不称为已经对等。没有完成 EE 全产品、所有介质和故障组合的穷举验收。**

## 证据与运行范围

- 物理主机 `lisa-1/2/3 = 10.131.7.207/208/209`，逐机 VPD 确认换带器 `55L3A7802K19LL01`、TD9 `11EB4A80F1`。当前逻辑库只有一台驱动器。
- Rust 使用原隔离状态 `/root/tape-ha-hw-20261016/state`，三节点 71/72/73、17820/17821、VIP `10.131.7.219`。先备份停机状态，再正常推进；没有回滚数据库、注入目录或替换正式服务。
- 仅向 RC0018L9 原卷追加新路径 `/comparison/ee-gap-r3-20261021/task-persistence.bin`，1 MiB；另保留 R2 已确认写入的同大小测试文件，本轮合计新增 2 MiB。卷 UUID `4d78dbe5-5d35-4bb1-a0f3-ca2621702349`。RC0017L9 和 CLNU03L1 不装载、不移动。没有 FORMAT/ERASE。
- EE 正常停服释放设备后，Rust 207 成为实际 owner，208 为控制 Leader；正常停止 207 后，由 208 接管。逐次实际读带核对新文件和原有 64 MiB 文件的 SHA。查询 EE 既有 GPFS 测试对象及任务历史不读取文件正文，不触发新召回。
- [本轮原始结果](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/)、[汇总与独立收尾核验](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/summary.json)。每条 GAP 以下链接到具体证据，源码和命令帮助只作补充解释。
- [官方 25 项验收基准](ee-primary-capabilities-20261021.md)由 [`research` skill](/root/.agents/skills/research/SKILL.md)背景核对 IBM 一手资料；该文件本身不是现场证据。

## 已由现场确认的差距

| ID / 优先级 | 差距及用户影响 | EE 现场证据 | 当前项目实际结果 | 确认边界 |
|---|---|---|---|---|
| G01 / P0 | **GPFS HSM 文件生命周期未接入**：缺少与 resident / premigrated / migrated / saved 对应的迁移、驻留与磁盘释放合同。 | 查询既有 1 MiB 文件返回 `premigrated`、1 份带上副本；空对象为 `resident (saved)`；历史 migrate 1013 成功，原物理实验已核对 inode、磁盘块变化和透明召回 SHA。 | 新文件真实归档后 `/stat` 为 `committed`；现场带有效 `-e` 的 `ltfsctl migrate/premigrate/recall/save` 均不识别，相关 HTTP 管理请求为 404。 | 这是 GPFS HSM 合同缺口。项目已有 FUSE 按需读带，不把 FUSE 说成缺失；`committed` 是提交状态，不能当作文件驻留状态。 |
| G02 / P0 | **缺少用户可查询、管理的磁带副本集合**：不能从文件状态获取 EE 式各份 `tape@pool@library` 及其健康/放置策略。 | 同一实际文件 `file state` 返回 `Replicas: 1` 和 `RC0017L9@REA_P0@EE_REA`。 | 实际 `/stat` 仅给单一 barcode、版本和摘要，副本管理查询不存在。 | 本轮确认单副本视图/管理接口差距；没有把 Raft 的三份目录副本算成三份带上数据副本。2–3 份数据副本及故障 fallback 未完成对照，见后表。 |
| G03 / P0 | **文件任务记录不跨节点、进程持久保存，且与管理任务分为两套合同**。客户端不能始终凭原任务 ID 查询结果。 | 本轮仍可查询 10 月 16 日 migrate 1013、save 1025、export 1043、import 1048 的成功终态；原物理 MMM 重启后历史保留。 | 本轮 PUT 返回 201、task=1，207 查 `/tasks/1` 为 committed；208 查为 404。正常接管和原进程重启后，仍为 `unknown_task`。同一实验的 rescan 管理任务成功记录保留。 | 仅文件任务存在此差距；不能再把全部管理任务说成易失，也不能把任务不可查询误报为已提交数据丢失。新文件和原 64 MiB 均实际读回。 |
| G04 / P1 | **EE 对象空间计量未实现**：`active_space`、`reclaimable_space`、可回收比例无法直接用于运维决策。 | EE 已入池 RC17 实际返回 active=337696240 B、reclaimable=56568336 B；池也有对应非零统计。 | 已装载并写入的 RC18 主容量、DP/IP 正常，但上述对象字段仍为 null。 | 确认指标缺失，不直接比较不同磁带的 Usable 数值，不以 DP 的 max−remaining 冒充对象活跃/垃圾空间；IBM 私有预留算法等价性未验证。 |
| G05 / P1 | **EE 原生卷 import/export 的 GPFS 事务入口未实现**：不是简单搬带、分配或导出普通文件。 | 本轮查询真实 export1043/import1048 均 succeeded；原物理记录包含源对象 purge、新前缀 stub/目录/链接重建及冷读 SHA。 | 实际 `/admin/tapes/RC0018L9/import`、`export` 返回 404；现有 assign/unassign/move/restore 不提供这套 GPFS 命名空间与副本事务。 | 普通 EE 保存对象识别及恢复已经有物理成功记录，不能重列为缺失。为核查差距没有重跑会删除 GPFS 测试源对象的 export。 |
| G06 / P1 | **EE/GPFS 外部策略调用协议未兼容**。现有文件上传接口不能作为 EE 的外部池命令直接接入。 | 现场 `eeadm TEST` 返回 0；历史成功的 migrate/save 使用候选列表，reconcile1036 已实际调用 GPFS snapshot/mmapplypolicy 并完成。 | 现场 `ltfsctl TEST` 返回 2、unknown subcommand；没有策略管理资源，迁移等外部命令入口也缺失。 | 本轮确认协议/接口差距。完整策略选取、转义、重复项和部分失败矩阵未跑，不以一次 TEST 成功宣称 EE 所有策略行为通过。 |
| G07 / P1 | **活跃增量索引卷不能直接交给当前现场 EE 的 LE 挂载**，须先做 Full/checkpoint 交接。 | 既有同物理介质对照中，EE 1.3.4 的 LE 拒绝 generation15 的 `ltfsincrementalindex`，mount rc=3，`LTFSI5011E Unexpected top tag`；随后 Full16 挂载成功且文件 SHA 一致。 | 项目能读取并重放原生增量；普通活动增量提交不等同 EE 可直接挂载的 Full 交接状态。 | **复用 10 月 16 日双方实测**，本轮确认 EE 版本未变，未声称重新制造过该失败。它是当前 EE 版本的互通限制，不是项目不支持 LTFS 增量。[原失败及清理](../.scratch/ltfs-ha/probes/results/ee-basic-completion-20261016/ee-oracle-incremental.json)。 |
| G08 / P2 | **集群 CLI 尚未覆盖已提供的 HTTP 管理能力**。运维不能通过统一客户端完成 EE 式资源查询。 | EE 的 tape/drive/node/nodegroup/pool/task 查询实际运行成功。 | 现场带有效 `-e` 的 `ltfsctl tape list`、`drive list`、`node list`、`nodegroup list`、`task list`、`pool show` 六个实际调用均返回2、unrecognized subcommand；对应资源已有 HTTP GET。 | 确认 CLI 覆盖缺口，不把它误报为整个集群没有这些能力；独立 tape-library CLI 不是 Raft 集群客户端。[六次实际返回](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/native-cluster-cli-gaps.json)。 |

G01/G02/G03 的本轮证据：[带有效 endpoint 的五个 CLI 实际拒绝](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/native-file-cli-gaps.json)、[状态/任务/新写入与接管结果](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/gap-measurements.json)、[EE 文件状态](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/ee-file-placement.txt)、[EE 保存对象状态](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/ee-saved-placement.txt)。quick 文件状态会明确标记 `check tape state`，不拿它证明介质健康；原状态转换行为证据另见 [10 月 16 日报告](../.scratch/ltfs-ha/reports/rea-ee-gap-report-20261015.md)。副本的官方定义见 [IBM replica creation](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=migration-replica-redundant-copy-creation)。

G04 的证据：[本轮 EE tape JSON](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/ee-tapes.txt)、[Rust 实际状态](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/native-final.json)。

G05/G06 的证据：[已执行请求及真实 404](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/native-interface-absence.json)、[EE import 终态](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/ee-task-import.txt)、[EE export 终态](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/ee-task-export.txt)、[完整命令及返回码/stdout/stderr](../.scratch/ltfs-ha/probes/results/ee-gap-physical-r3-20261021/controller.jsonl)。原 [EE reconcile 成功记录](../.scratch/ltfs-ha/probes/results/ee-full-20261016/reconcile-after-m4-result.log)保留失败后补依赖的过程。未知 URL 的 404 仅证明该管理接口没有实现；已结合现场 CLI 不识别和当前公开接口清单核对，不把一个随意构造的 URL 当作全部后端能力不存在的唯一证据。

## 25 项覆盖矩阵

G08 是 CLI 覆盖差距；下表“已有”按受支持接口判断，不能据此认定 CLI 已全部补齐。

“已有/部分覆盖”表示有实际项目实现和明确物理场景证据，不表示 EE 全部语义相同；“待验”不计入上面的已确认 GAP 数量。

| 官方基准 | 本轮核查结论 | 物理覆盖或尚缺条件 |
|---|---|---|
| EE01 系统健康 | 部分覆盖，完整诊断等价待验 | 两侧实时节点/驱动状态可查；组件故障、恢复和外部告警链路未对照。 |
| EE02 集群及多库 | 单集群多库与固定 VIP 已实现并物理通过；EE 双节点对等待验 | 后续两库同节点/分散持有、单库不可用、VIP 迁移写读均通过，见[多库验收](multi-library-physical-acceptance-20261021.md)。EE207仍为原 disabled，不能据此验证 EE 双节点完整 HA。 |
| EE03 驱动策略 | 已有，部分物理覆盖 | 单机角色/节点组拒绝路径已有物理证据，HA 字段和管理入口已接入。多驱动并行、公平性未跑。 |
| EE04 池属性 | 已有常用属性，其余待验 | 介质限制/阈值/节点组/mountlimit 已实现；多驱动限额效果、3592 格式及其他设备类型缺少现场条件。 |
| EE05 库存与移动 | 已有，清洗带缺口关闭 | 本轮新代码经真实 owner rescan，三节点 HTTP 均识别 CLNU03L1 type2；已有物理 I/E 往返记录。未装载清洗带。 |
| EE06 分配及格式化保护 | 已有，部分物理覆盖 | 既有真实非空 unassign 拒绝；新式清洗带分配拒绝有软件回归。没有为对比而再次格式化任何带；empty-remove/stub 语义仍属 HSM 范围。 |
| EE07 全状态模型 | 正常/offline 已验，其余待验 | 当前健康数据带不能证明 data_full/full/硬件写保护/全部 sense 异常自动映射。没有写满或制造介质错误。 |
| EE08 容量 | G04；主容量/DP/IP已有 | 本轮实际状态和原 MAM 独立对照均保留；对象计量未实现，不能称容量完全与 EE 相同。 |
| EE09 HSM 迁移 | G01 | 本轮查询状态与接口拒绝；EE 原状态转换有物理旧证据。 |
| EE10 外部策略 | G06 | TEST 协议实测；复杂策略边界未验。 |
| EE11 透明读取及副本容错 | 按需读取已有；HSM 为 G01，冗余容错待验 | HTTP/FUSE 现有按需读带不可抹去；EE DMAPI 透明召回有既有物理证据，多副本 fallback 尚未做。 |
| EE12 召回优先级/重排 | 待验 | 配置 `recall_use_rao=auto` 不是实际执行 RAO 的证据；需要真实批量召回和资源竞争。 |
| EE13 文件状态与副本查询 | G01/G02 | 对实际成功归档文件和实际 GPFS 文件分别读取用户可见状态。 |
| EE14 2–3份带上副本 | G02 的模型差距已验，行为待验 | 当前 EE 只有一个有带成员的池；需要额外可用介质、目标池和安全 fallback 实验，不能以多条 FC 路径或 Raft 代替。 |
| EE15 save 对象 | GPFS save入口属 G01；对象恢复缺口已关闭 | 既有真实1目录/2文件/2链接恢复及 SHA 对照；不把一般 LTFS 保真等同完整 GPFS save 生命周期。 |
| EE16 import/export | G05 | EE 实际成功历史及本轮缺少入口；普通恢复能力已区分。 |
| EE17 GPFS reconcile | 待补双方同场景验收 | EE1036实际扫描成功；项目现有 reconcile 明确为 LTFS→本项目 catalog，GPFS 快照/对象引用对账没有按当前 HA 代码补跑。不能把同名当语义相同。 |
| EE18 reclaim | 项目已有，双方全流程待验 | 原 Holo 两带及 reformatting 恢复通过；当前物理逻辑库只有一台驱动，不具备通常的双 g 驱动 EE 回收前提，不能补造格式化源带实验。 |
| EE19 datamigrate | 待验 | 本轮项目请求404已记录；EE 数据作业未实际跑，不能仅凭 EE help 把该项升格为双方已确认功能 GAP。 |
| EE20 replace/validate | validate已有，故障替换待验 | 项目真实 full-read/SHA 校验已有证据；replace 的 EE 作业及故障带条件没有完成，404 本身不证明 EE 替换行为已测。 |
| EE21 任务 | G03；管理任务持久性已实现 | 本轮同一接管中管理任务保留、文件任务丢失。取消需分作业类型，官方只对特定任务保证 cancel，不能泛化。 |
| EE22 完整性 | 两侧基础已有，错误矩阵待验 | 新旧文件实读 SHA、EE历史迁移/召回 SHA 均有证据；未故意篡改介质以验所有校验失败路径，算法差异本身不是 GAP。 |
| EE23 SNMP/运维通知 | 待验 | 尚未核对实际 trap 接收端并触发可恢复告警；有日志或配置不等于通知闭环。 |
| EE24 REST认证/权限 | 版本基准差异，未确认 | 现场1.3.4.0；1.3.5 的 PAM/token/CSRF/RBAC 必须在相应版本环境验证，不能从新版文档直接列为现场 GAP。 |
| EE25 备份/整系统恢复 | 待验 | Raft快照/目录恢复已有；不等于 EE SOBAR 或 GPFS stub 系统迁移。未破坏系统数据库进行恢复实验。 |

另外，物理 WORM、加密/KMIP、跨库、物理 FC 断链/设备 reset、整机断电与写入各阶段组合故障缺少本轮双方等价实验。已有旧物理网络分区/进程故障与 Holo 故障覆盖按原记录保留；它们不能替代未跑的硬件场景，也不应倒退为“本项目没有 HA”。

## 已关闭或不应继续列入 GAP 的项目

1. 集群 HTTP 管理已接入 Raft/owner；管理任务、配置可持久复制，不能沿用旧报告“管理全是单机 SQLite”的结论。
2. 节点组、驱动 m/r/g 角色、池常用属性、offline/online/blacklist、库存及受管移动已有实现和限定物理证据。
3. 清洗带识别本轮完成真正三物理节点 owner→Raft→HTTP 验证，补上前轮仅 CLI/共享投影验收的边界。
4. EE 保存对象目录、空文件、符号链接的解析与恢复已物理成功；其与完整 GPFS HSM 导入事务是两件事。
5. LTFS读写、内容摘要、元数据保真、增量恢复和按需读带已有，不把 EE 同名操作自动视为缺失。
6. 历史“约330秒恢复”已被后续有界恢复优化替代，原相近物理场景降至约29–31秒；本轮不提供新的 EE 同口径 RTO 比较，不将历史数值当当前 GAP。

历史证据：[基础能力与对象恢复](../.scratch/ltfs-ha/reports/ee-basic-completion-20261016.html)、[容量/离线/owner专项](../.scratch/ltfs-ha/reports/admin-capacity-physical-20261020.html)、[有界恢复对照](../.scratch/ltfs-ha/reports/recovery-bounded-physical-20261020.html)。每份记录的版本、时点和介质以正文为准。

## 建议推进顺序

先统一文件任务的持久 ID/查询/终态，再建立文件驻留状态与磁带副本集合，保持它们与现有提交状态、Raft目录副本分离。随后接 GPFS HSM、策略和原生 import/export/reconcile；对象活跃/可回收计量应从该引用模型派生，不能只补 JSON 数字。已有 HTTP 能力可另补齐 ltfsctl 集群 CLI，避免维护两个设备控制实例。与当前 EE 交接时使用 Full/checkpoint，并将活动增量卷的限制写入操作流程。

待验项目需要先具备对应环境，再各自验证输入、执行终态、实际内容、失败恢复与收尾。当前只能交付上述有现场依据的差距清单，不能承诺这就是 EE 全产品所有 GAP 的最终数量。

## 收尾与记录质量

R3 测量和独立收尾均通过。新文件及 R2 保留文件 SHA 均为 `5a545e259344d7a19dfa15abaf346795f005de5c77acae475b941ef453665b99`；原64MiB文件 SHA 保持 `281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`。控制缓存最终 gen117/files83/appendable，不把缓存代数称为另外完成的冷挂末代审计。三节点 VIP holder=null/token34363；测试和观察服务均 inactive/MainPID0，原17421控制集群仍可答且 executor=null。EE208 available/enabled、207保持 disabled；双设备恢复原 EE 唯一注册及 type3 holder，驱动空、RC18槽7、RC17槽8、清洗带I/E2。临时 SSH sockets 已关闭，凭据未落盘。

守护进程 CDB 日志无 FORMAT/ERASE/EXCHANGE，MOVE 仅1007→1；维护 CLI 的卸载和回槽另外记录在控制器日志，不能把守护进程计数说成全部现场命令。原生 CLI 只将 RC18 归槽。正式服务未替换，原始 Raft/catalog 没有回滚，2MiB测试内容保留在带上。

R1 在只读预检时未处理 eeadm 将版本写入 stderr 而中止，没有设备接管；R2 已归档并正常接管，但探针在旧进程 HTTP 尚未监听时过早查询中止，已安全收尾。R3 只修正探针重连等待并用新标识复验，没有放宽产品超时或改生产实现；R2 文件在 R3 再次实际读回。三个已消费 requested 均禁止原样重跑。

新候选已按 gnu.2.34、features=fuse 交叉构建并逐机核对哈希。Rust 源码与上轮447通过/4忽略、fmt/check/clippy通过的版本一致，本轮没有重跑整套 Rust 测试，也不将旧结果写成新执行；新增探针通过 Python 语法检查，文档链接和diff检查通过。第一次构建漏给可选 fuse 特性而拒绝构建，补齐特性后的完整候选构建成功，原日志保留。

### 后续设备清单补充：主机可见资源与本轮测试范围

同日用户追问后，补查 lisa-2 全部 SCSI generic 设备：按 VPD 序列号/WWID 去重实际可见 **2 个带库、3 台独立驱动**。EE_REA 与本轮 Rust 验收仍只使用库55L3A7802K19LL01及TD9；当时另一库55L3A78011W9LL01和另外两台LTO8驱动尚未纳入测试，库存、预留和现有使用者未核查。本文“一库一驱”描述的是验收/EE配置范围，不能解释为整台主机物理上仅有这些资源。多驱/跨库验证应先评估既有另一库的可用性，不应直接断言必须采购新设备。[只读设备清单](../.scratch/ltfs-ha/probes/results/library-count-20261021/node-208.json)、[WWID去重证据](../.scratch/ltfs-ha/probes/results/library-count-20261021/wwids-node-208.json)。

后续用户授权后，两库已纳入同一 ltfsd Raft 集群并完成真实硬件与 VIP 验收，更新范围及证据见[多库验收](multi-library-physical-acceptance-20261021.md)。此前仅盘点时的未知项不再代表当前多库实现状态；其他 GAP 结论不变。
