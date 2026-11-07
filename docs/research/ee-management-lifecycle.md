# EE 库库存与磁带生命周期：rescan、move、assign、unassign、validate

调查日期：2026-11-04。本报告服务于本项目的 Library → Pool → Tape 管理实现，范围为五项操作及其与 import/export 的边界。先整理冻结证据/官方文档，后经root书面交接，在真实A库AA0017L8补做原生生命周期实验。设备已正常停止释放并交回项目；当前仅整理本地归档。

## 结论与证据等级

- `library rescan` 刷新物理资源视图；`tape move` 改变机械位置；两者不等于把磁带文件导入命名空间。
- EE 的 `tape assign` 具有检查/格式化并加入池的语义，不能复刻成单次数据库 pool 字段更新。已有文件默认拒绝，保留文件的入口是 `tape import`。
- `tape unassign` 须确认没有仍依赖该卷的文件，不能用它代替会处理 GPFS 文件引用的 `export`。
- `tape validate` 装带检查并更新状态；本次真实执行读取了 LTFS index，但没有新增用户文件，也没有验证每个文件的完整 payload。以上契约分别由下文原生 help、物理 trace 或官方页面支撑。

采用四种标记：**物理实测**＝真实 IBM FC 库执行及后台证据；**原生 help/静态**＝本次安装产物的帮助或离线分析；**文档**＝版本明确的 IBM 页面或用户提供的 ee.md；**建议**＝本项目拟实现的工程选择，不冒充 EE 内部实现。

| 操作 | 本轮既有物理 EE 证据 | 未覆盖范围 |
|---|---|---|
| library rescan | EE1.3.4.0 成功，完整后台 trace，18条真实 A changer CDB | `--task`、多库、故障中的 rescan |
| tape move | AA17独立EEADM IE1035/home1036成功，实际CDB1001→101→1001 | 未测原home被占/其他目的选择；IE中的LE home_slot实际为0 |
| tape assign | AA17普通assign1029真实拒非空；显式force1031成功、实际新UUID/空index/入池 | 未测所有有效空LTFS不带force、WORM、LTO9优化分支 |
| tape unassign | AA17默认empty-check=tape的1039成功；保留UUID/Gen3/空index | 非空拒绝、disk/compare/safe及故障介质仍未实测 |
| tape validate | AA0018L8 task1027 成功，index读取、namespace/文件SHA前后对照 | 错误介质、所有异常状态、全盘 payload 校验、自动修复 |

真实环境是208 EL8 chroot 中 EE **1.3.4.0-53233**、LE2.4.6.0-10507，207/208 GPFS5.2.3-6，库 A serial **55L3A7802K19LL01**、drive serial **11EB4A80F1**；Holo 结果不计上述实测。[物理最终报告](../../../../tmp/tape-fdb-ee-management-20261103-4r8xnbqz/rea/physical/PHYSICAL-EE-REPORT.md)的实际位置为 `/work/tmp/tape-fdb-ee-management-20261103-4r8xnbqz/rea/physical/PHYSICAL-EE-REPORT.md`，以下以 `EVIDENCE` 指该目录。

## 文档覆盖与版本差异

完整核对了 [ee.md](../../ee.md)：它声明1.4.0，包含 [tape move](../../ee.md#eeadm-tape-move)、[validate](../../ee.md#eeadm-tape-validate)、[import](../../ee.md#eeadm-tape-import) 和 [export](../../ee.md#eeadm-tape-export) 完整章节。**没有独立的 library rescan、tape assign、tape unassign 章节**；rescan 只在 library show 的缓存提示中被引用。缺失部分以下用原生 help 和官方文档补足，不冒称 ee.md 已给出全部契约。

IBM1.4.0对应网页本轮读取失败；没有据此推断命令不存在。可读取的官方资料包括1.3.4/1.3.5/1.3.6，各段注明版本。当前 IBM1.3.4网页列出 rescan `--task`，但本次 **1.3.4.0原生 help 没有该选项**，应视为补丁/网页版本差异，不能替换已安装程序证据。官方1.3.6 rescan另列互斥任务：Cluster Restore/Save、Library Rescan、Tape Cleanup/Export/Reclaim/Reconcile/Replace/Set/Unassign；未据此声称当前1.3.4.0动态验证了同一清单。[IBM rescan1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=library-eeadm-rescan)、[IBM rescan1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=library-eeadm-rescan)；`EVIDENCE/native-library-help-208.json`。

## library rescan

**参数与前提。** 本版语法为 `eeadm library rescan [<library>]`：给出名称只刷新该库；省略则全部已配置库；支持 `-h/--help`。官方网页的 `--task` 创建父 `library_rescan` 任务及每库子任务，详情通过 task show `-v` 查询；它不是已证实可用于当前1.3.4.0的 `--async` 别名。CLI同步 exit0 表示完成，非0表示失败。[IBM rescan1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=library-eeadm-rescan)；`EVIDENCE/native-library-help-208.json`。

**状态与副作用。** 语义是向库查询物理资源并更新 EE inventory。物理实测 `rescan EE_REA_A` 成功：15 TUR、1 MODE SENSE、2 READ ELEMENT STATUS，均到 A changer；未装带读取 LTFS index，未建立 GPFS stubs。另16次 ioctl 只到被屏蔽的 `/dev/null` 返回 ENOTTY，不能计作 B 库 SCSI。历史文本汇总把16条日志也匹配进去产生50；准确机器账本是 `EVIDENCE/rescan-physical-normalized.json` 的 **18**。[实际命令与时间窗](../../../../tmp/tape-fdb-ee-management-20261103-4r8xnbqz/rea/physical/run-physical-rescan-208.json)。

**缓存边界。** 本次成功 library list/show、tape list/show 前台及 MMM/MD/LE 全线程窗口物理 SG_IO=0，前后各15秒空闲对照；显式 rescan 才触发上述 changer CDB。不能仅凭前台无 SG 推断后台无 SG，也不能由此宣称所有版本/周期任务永不刷新硬件。`EVIDENCE/summarize-aa18-successful-queries-208.json`、`query-front-IPC-observations.json`、`rescan-physical-normalized.json`；[ee.md library show](../../ee.md#eeadm-library-show)。

**最小实现建议。** 将 inventory 当作带时间戳的观测，list/show仅读 FDB。rescan通过该库当前执行节点排队、核对设备serial/当前owner，完整读取再发布新观测。查询失败保留上一份成功观测并标本次失败，不把“没读到”变成空库，不清除Pool/Tape归属或目录。多库父任务保留每库结果；部分成功不覆盖成全部成功。无需 nodegroup。

## tape move：位置改变，非文件导入

**参数。** `eeadm tape move <tapes...> -L homeslot|ieslot [-p <pool>] [-l <library>] [--async]`。磁带已分配时给所属池；**未分配时必须省略 `-p`**。单库可省 `-l`；多库须明确库以避免同条码歧义。[ee.md move](../../ee.md#eeadm-tape-move)、[IBM move1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-move)。

**前提与变化。** homeslot 表示从驱动器或 I/E 回本卷归属存储槽，不是任意整数地址。去 ieslot 允许未分配卷、已分配且offline卷，或处于pool relocation流程中的卷。成功只改变真实位置与对应库存，不能凭目的为homeslot便自动加入池、导入索引或上线。`--async` 的0只表示任务受理；同步0须所有指定卷完成，部分失败也非0。[ee.md move](../../ee.md#eeadm-tape-move)。

**机械 I/E 与逻辑 import。** 物理介质进入 I/O站 → 库存发现/rescan → move homeslot，是入库链路；其后才选择 assign或import。物理出库是先完成offline或逻辑export等前提，再move ieslot，由人/库管理系统取走介质。export不会等价于“已经从机器人里拿走了磁带”。官方加减介质流程给出上述分步操作；该页面一处“有数据用assign”的泛述与命令参考及后续import段冲突，保留数据应以专用import契约及实际help为准。[IBM 加减介质1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=drives-adding-removing-cartridges)、[IBM assign1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=tape-eeadm-assign)。

**写入边界。** move不是格式化指令；然而从已挂载drive退出可能涉及正常卸载、flush、关闭index/MAM更新，不能给出“整个move流程绝不写介质”的承诺。AA17补测的独立IE/home两任务窗分别3/5真实SG，仅TUR+MOVE，未捕获数据/MAM写；但先行offline和随后online各自真实写index/MAM。当前结果不覆盖所有从已挂载drive移动的路径，详下方补测。

**最小实现建议。** 显式library/barcode/location；所有槽位从address map计算，并再次查询条码、source、home及空闲目的位置。没有可用IE槽/有效home、磁带busy、外部reservation或当前所有权失效时拒绝，不能抢占另一控制器完成动作。驱动器里的本实例介质先走现有正常停写/卸载，再MOVE，再读库存核验。失败后保留实际位置/未知状态；不按旧source盲重放MOVE，不缩短机械超时。未实现pool relocation时明确不接受该例外，不借此放宽online卷出库。

## tape assign：具有格式化语义的入池

**参数与前提。** 本次原生help：`eeadm tape assign <tapes...> -p <pool> [-l <library>] [--async] [-f|--force-format] [--force-re-optimize]`。限未分配卷；IE槽不能assign，必须先homeslot。已有file object默认拒绝；要保留文件应使用import。`--force-re-optimize`用于LTO9重新优化、耗时更长，不能静默当作普通format。[IBM assign1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=tape-eeadm-assign)；**原生证据** `EVIDENCE/query-result-and-help-208.json` 中 `args=[tape,assign,-h]`。

**默认不等于永远重新格式化，也不等于只改DB。** 专用命令help描述为格式化并入池；官方池指南进一步说明：新带/空带可初始化，已有文件须force；已具有有效LTFS格式时并非所有条件都默认重格式化。可确定的公共契约是“具有介质检查/初始化/破坏性格式化能力”，实际分支需要看介质和flag。不能把未知mount失败解释为空带后直接格式化。[IBM 入池/格式化指南1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=cdtsp-add-tapes-pools-format-tapes-remove-tapes-from-pools)。

**状态与写入。** 操作成功后卷属于指定池；格式化分支会建立LTFS格式/标签/index，force可使原文件不可达，可能改变volume UUID等身份。AA17补测已经真实观察force分支的format、标签/index及12条WRITE ATTRIBUTE，并成功入池；每种空带分支及所有MAM字段语义仍未覆盖。旧AA18独立 `leadm tape format` 继续作为LE fixture证据，不冒充这次原生 `eeadm tape assign` 成功。`EVIDENCE/write-aa18-flat-fixture-208.json`、`final-AA0018L8/manifest.json`。

**失败与最小实现建议。** CLI和HTTP都显式区分普通检查/有条件初始化与force format；保留项目已有格式化确认门槛。受理前校验Pool属于Library、卷未分配、介质/drive兼容、位置与所有权；执行时再次核对。先证明已有数据/是否可格式化，再调用现有受保护格式化路径，验证两分区标签/index/MAM观测后发布入池结果。对失败记录最后确认的UUID/generation、已完成阶段及介质可能已改变，不能回滚DB就宣称介质未写。所有删除旧数据的授权不应被代码转化为“每次assign隐式force”的默认值。

## tape unassign：没有活动依赖后解除池关系

**参数。** `eeadm tape unassign <tapes...> -p <pool> [-l <library>] [--async] [--empty-check=tape|disk|compare] [--safe-unassign]`。默认允许 appendable、append_fenced、full、data_full、recall_only；safe-unassign还可用于need_replace/require_replace；`empty-check=disk`可用于其他状态，但卷不能在drive中。上述完整状态/选项集合来自本版help及官方1.3.4；本次只成功实测AA17空卷默认分支，不能替代其他状态/选项的动态验证。[IBM unassign1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=tape-eeadm-unassign)；`EVIDENCE/native-library-help-208.json` 中 `args=[tape,unassign,-h]`。

检查方式须区分：默认tape检查缓存的带内容；disk进行GPFS policy扫描确认没有对应stub；compare从带文件查找对应stub，本版help已称该逻辑逐步废弃。safe-unassign试图把剩余依赖该卷的stub恢复为resident，成功后才解除关系，并强制采用disk检查。官方网页提及旧empty-remove/safe-remove别名；本项目无需兼容旧别名。[IBM unassign1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=tape-eeadm-unassign)。

**与export不同。** unassign要求不再有对应GPFS依赖，safe分支以先恢复resident为目标；export的目标则是离开系统并按规则清理指向该卷的GPFS文件/最后副本stub。不能把“FDB里删Tape.pool”当作这些条件已经满足；也不能因为unassign成功便假定磁带文件已擦除，旧文件可能仍留带上。重新使用可能仍需显式force-format。[IBM 入池/移出池指南1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=cdtsp-add-tapes-pools-format-tapes-remove-tapes-from-pools)、[ee.md export](../../ee.md#eeadm-tape-export)。

**MAM/index与失败边界。** 命令契约不是格式化/擦除；AA17默认分支已真实成功，任务窗只有1TUR、无装卸/READ(6)/WRITE(6)/WRITE ATTRIBUTE，Gen3空index保留。仍不能据此保证compare/safe或故障路径完全不装带、不写MAM/index或部分recall失败无副作用。safe分支必须按每卷真实完成结果记录；未证明所有依赖已解除就保留池关系。不能从“没有GPFS”简化成“本项目必无引用”。

**最小实现建议。** 本项目应在自己的FDB模型上证明没有已发布对象、历史有效副本、写入批次或在途任务依赖，并以事务/任务gate阻止证明后新引用进入，成功才解除池关系；不要仅数当前某个Pool前缀下的路径。对safe-unassign，若项目尚无可证明的resident恢复/持久化契约，明确拒绝该选项，不能仅下载到临时spool后声称已安全移除。`empty-check`的GPFS策略语义不能空壳照搬；选择实现能给出的依赖证明并在帮助中说明。

## tape validate：检查介质条件并更新状态

**参数与前提。** `eeadm tape validate <tapes...> -p <pool> [-l <library>] [--async]`。典型require_validate，也允许appendable、append_fenced、full、data_full、recall_only、need_replace、require_replace、check_key_server、check_hba、check_tape_library。卷须属于池且online；这是装带状态检查，不是import或自动ltfsck。[ee.md validate](../../ee.md#eeadm-tape-validate)、[IBM validate1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-validate)、[IBM 检查介质1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=troubleshooting-checking-validating-reconciling-medium-in-library)；`EVIDENCE/native-index-entrypoint-help-208.json` 的对应原生help。

**真实成功与实际读取。** AA18命令 `tape validate AA0018L8 -p REA_AA18 -l EE_REA_A` exit0，创建task1027；15:52:22UTC受理、15:53:57原生报告成功。完整窗口150条真实A SG、另8条屏蔽ENOTTY；10条READ(6)响应实际包含LTFS label和ltfsindex XML。观察到LOAD/UNLOAD、MOVE、MODE SELECT等状态操作，没有捕获WRITE(6)或WRITE ATTRIBUTE。用户namespace一直4文件，inode/SHA不变，9512B cache schema SHA保持 `8421f195c18f7ded1c454a4d9bcd3e053711f85a01637e7a4cabd1190c1da02e`。`EVIDENCE/run-aa18-reconcile-validate-208.json`、`summarize-entrypoint-SG-208.json`。

这只证明本次健康卷路径读取index并检查状态，不能说validate从不写带、检查了每个payload、恢复了缺失inode，或所有故障状态都会变appendable。**不应宣称EE提供 `validate --full-read`**：本版help和ee.md没有该参数；本项目若提供全量内容校验，须明确属于自己的扩展。

**最小实现建议。** 通过当前owner独占装带，读标签/UUID、index/recovery一致性与必要MAM/容量/介质状态，再将有证据的状态写回FDB；有未知/限制条件时保留诊断状态，而不是将非传输错误一律清为healthy。默认不format、不自动修复、不导入用户命名空间。保留原始status/sense/ASC/ASCQ及阶段；失去所有权立即终止。后台task完成与前台等待使用同一持久结果，不能在只排队时返回“已验证”。

## assign/import、unassign/export 与文件可见性

| 命令 | 池关系 | 用户文件命名空间 | 格式/介质内容 |
|---|---|---|---|
| assign | 成功后加入池 | 不负责把原文件恢复为用户stubs | 有条件初始化/format，已有文件默认拒绝，force可破坏旧数据 |
| import | 未分配卷成功后加入池 | 读取源index、建立stubs/目录，使带上文件可访问 | **不是只读保证**；本次真实EE改变对象路径/xattr并写index/MAM |
| unassign | 无依赖证明成立后解除 | 不以删除活动文件为正常前提；safe先resident | AA17默认分支无捕获介质写；其他分支仍未知 |
| export --remove | 成功后移出池 | reconcile并处理引用，最后副本的GPFS对象可被删除 | 文件payload留带，index/导出标记可更新；不等于机械出库 |
| move | 不应隐式改池关系 | 不创建/删除用户文件 | 改机械位置，drive卸载可能伴随正常介质更新 |
| rescan | 更新“看到在哪里” | 不等于导入目录 | 本次仅changer观测 |
| validate | 检查已有成员卷，更新有证据的状态 | 本次不创建新文件 | 装带、index读取；健康路径无捕获的数据/MAM写 |

import/export的文档依据是[ee.md import](../../ee.md#eeadm-tape-import)与[export](../../ee.md#eeadm-tape-export)。物理依据：AA18 import1002、P1013、R1020及export1008/1015成功，最终三文件recall SHA一致；RC18 import1000失败前已写介质，正常退出写Gen134；不能把失败导入描述成“无副作用”。详 `EVIDENCE/PHYSICAL-EE-REPORT.md`、`native-import-task-details-208.json`、`verify-and-recall-aa18-final-R-208.json`。

## 统一任务与失败契约建议

EE move/assign/unassign/validate的help约定：同步0＝所有指定介质成功；部分或全部失败＝非0。`--async`的0仅表示请求受理且任务建立。task list/show应观察运行中、逐卷结果与终态，不能仅依据前台退出码判断介质任务成功。rescan使用其独立契约，不把未验证的flag强套过去。[ee.md move](../../ee.md#eeadm-tape-move)、[validate](../../ee.md#eeadm-tape-validate)；`EVIDENCE/query-result-and-help-208.json`、`native-library-help-208.json`。

本项目CLI和HTTP建议共用同一请求/任务：显式Library、Pool（需要时）、Tape列表和动作参数；持久父任务→每卷结果，显示waiting/running/completed及succeeded/failed、当前phase、完成卷数、错误与最后确认的物理状态。受理、SCSI完成、校验完成、FDB发布是不同阶段。重连应查询原task，不以HTTP超时为由自动重发format/MOVE等有状态请求。跨磁带没有物理原子回滚，部分成功须逐卷保留；不因父任务失败抹去已发生的index/MAM/位置变化。

只有实测支持的进度才填数字。AA18导入父任务1/1 tapes、子任务3/3 files及user.importexport.progress已实际存在；这不能证明assign/validate有同样文件计数。1.3.4 task cancel help只支持reclaim/datamigrate，不能承诺上述任务都可立即取消。`EVIDENCE/native-import-task-details-208.json`、`native-task-cancel-help-208.json`。

## 冻结 REA 依据与不可推断内容

本次物理EEADM SHA256为 `b39403378192eadbcf5b50a2bf66eb8ccabd7b3d454a9d95fd938374fc5ae29f`；MMM为 `c90c45514a5b31b101dd1351dedf497ebbb9416ba46e1b19922df22d541403ef`。原程序REA使用Ghidra12.1.4，不沿用Holo版本地址。`EVIDENCE/physical-artifact-manifest.json`。

冻结证据：EEADM查询搜索 `ev_5f490374ffe142f5a5cf71a08db49f2029691d5d074aebcb979234e60f8a1ace`，TapeListCmd包装层 `ev_d0bff31bd08394848bf05130cae25698cc960ff142469488e5cb1b3b25c3d192`，MMM资源搜索 `ev_665212052844cebf2b74953d8949a7a0e6f2fbc1c969f816d88d2b812b16eae9`，GetAllTapeList反编译 `ev_ea21204a691cf01c502ddf09563c4b50a63b3222dde42ccbcb240c213473ba08`。它们支持“查询包装→MMM资源对象”的静态链路；部分Ghidra结果有no-return误标截断，**不能据此证明所有生命周期后端均无SCSI/无介质写入**。真正查询SG0、rescan阳性和validate读取XML结论依赖前述真实后台trace。

离线符号清单阶段只读取冻结 `.gnu_debugdata`，没有执行本地取回的ELF；远端原生执行结果见本节前文及下方补测：LibraryRescanCmd::NoTaskMain `0x1d0382`，TapeAssignCmd::ParseOption `0x1aced8`，TapeUnassignCmd::ParseOption `0x1ae654`，TapeMoveCmd::ParseOption `0x1b9862`，TapeValidateCmd::ParseOption `0x1c0622`；对应TaskParam::Validate也存在。mini ELF SHA `92cdc07a8e343febef3c4f09e71c431f75ae098250a11a7fe5632e4604a9c744`。它只是入口/版本定位，未解析执行路径，不能把“符号存在”冒充操作成功或特定MAM字段被写。记录在 `EVIDENCE/management-lifecycle-static-symbols.json`、`eeadm-mini-debug-provenance.json`。

完整物理原始证据包 `EVIDENCE/physical-native-evidence-20261103.tar.gz`，88,604,981B，SHA256 `1de118145913781456f42798ad5b0216984ed4d35a84d9b8a2c7e14bb3e239cb`，远端逐成员及本地整包SHA已核验。报告的本地路径和archive是可复查证据，不表示可以重启旧实验控制器。物理EE已正常停止并交回项目，后续真实验收由主代理安排。


## 2026-11-04真实AA17补测：原先三个未实测项已补齐限定路径

root书面交接后，仅A AA0017L8做原生EE1.3.4实验；空池create/list/show/delete均成功，完整前后台加空闲余量93.37秒0SG。普通assign1029/1030拒绝55,400文件的旧Gen199卷，GLESL138E非空，219SG但无捕获format/WRITE(6)/WRITE ATTRIBUTE；随后保留原生--force-format，1031/1032真实格式化并成功入池。没有假加本版不存在的--yes，没有把LE工具成功套到EE。

| 原生任务 | 终态 | 真实A SG | WRITE(6) / WRITE ATTRIBUTE | 要点 |
|---|---|---:|---|---|
| 1029普通assign | failed，按预期拒非空 | 219 | 0 / 0 | 原失败完整保留，不自动force |
| 1031 force assign | succeeded | 207 | 6 / 12 | opcode04一次；新UUID、Gen1、0file、Pool成员 |
| 1033 offline | succeeded | 150 | 2 / 4 | 实际装卸及介质元数据更新 |
| 1035 move ieslot | succeeded | 3 | 0 / 0 | MOVE1001→101 |
| 1036 move homeslot | succeeded | 5 | 0 / 0 | MOVE101→1001 |
| 1037 online | succeeded | 148 | 2 / 4 | 实际装卸及介质元数据更新 |
| 1039默认unassign | succeeded | 1 | 0 / 0 | 仅TUR；前后各15秒空闲窗0SG |

这些是完整任务时间窗的ioctl请求数，非协议固定数量，且不把masked ENOTTY算成B硬件。各操作原始状态/sense与CDB均留存。对非空unassign、disk/compare/safe、原home被占等路径，仍只有文档/软件证据，未扩称现场通过。

**home来源的新事实：** 搬出前真实MODE SENSE与LE库存给home1001；IE101内刷新后LE home_slot=0、assignment=UNASSIGNED，而EE仍offline/原Pool。因此LE assignment不能替代EE池关系，IE中的0不能当原home。回库前使用持久保存的真实1001并确认空位，实际CDB确实101→1001；本例不能证明EE内部总保留原home或已知home冲突时会如何选择。项目必须按自己的规范持久化可信home并在冲突时拒绝。

**项目边界：** EE offline/online确实装卸且写index/MAM；本项目FDB offline/online策略只改变管理元数据，不宣称复制vendor这些物理写入。不得把本报告误写成“EE offline/online也是纯DB开关”，也不必为仿照名称引入无需求的介质写入。

AA17最终UUID **b5e8eccf-0ff5-4e6f-9c2a-0fcf7931afbf**、Gen3、0文件/2目录（root与空.LTFSEE_DATA），unassigned并回原slot1。目录/index占用不等于有用户payload，不能用used_space非零代替有效引用判断。AA18 Gen7样本、RC17/RC18及B保持保护。官方cluster stop0、全部EE/LE/FUSE/tracer退出，本轮SSH/mask恢复，A canonical PRkeys/holder空和FD空后已交root；root已进入新项目bootstrap，本报告不代表当前设备仍空闲。

新增证据根 `EVIDENCE/aa17-lifecycle-20261104/`：[补测报告](../../../../tmp/tape-fdb-ee-management-20261103-4r8xnbqz/rea/physical/aa17-lifecycle-20261104/EE-AA17-REPORT.md)、`final-operation-CDB-ledger-208.json`、`offline-cdb-context.json`、`final-release-208.json`。完整raw包41,245,441B，SHA **dc01264e9bb9f446cc06f61edfc81738d9389aa4227aa46527bbee3210e97bff**，9,932成员远端逐SHA读回、本地整包SHA一致。EE成功仍不等于本项目13项CLI/HTTP完成，项目现场验收由root另记。


## 2026-11-04 补充：EE 与 LTFS 增量索引边界

现场 EE 1.3.4.0-53233 / bundled LE 2.4.6.0-10507 使用 LTFS 2.4 Full 索引；不能把 tape-rs 对 LTFS 2.5 增量的支持算作 EE 能力。EE 实际从 AA0017L8 读取并追加正文后，原始捕获的 DP195 是第37代 Recovery Full（schema2.4.0），DP201/IP8 是第38代 Unmount Full，均无 ltfsincrementalindex 根。该现场观察与 IBM 的2.4格式说明一致；不外推所有未来 EE 版本。

SNIA LTFS2.5 §9.2.13 规定增量保存前一索引之后的变化，正常卸载应有 Full；旧实现沿 Full 回指跳过增量，不保证从异常增量末尾恢复。项目→EE 的流程必须在独占移交前正常完成 Full 检查点、卸载并核验设备释放；项目崩溃留下增量末尾时先由项目恢复。EE→项目则导入 EE 的 Full，必要时检验其同代重写副本的逻辑等价，不能仅按 schema、锁字段有无或 Base64/文本表示判断冲突。

来源：[IBM2.4.6格式说明](https://www.ibm.com/docs/en/SSAMTG_2.4.6/pdf/storage-archive-sde-2.4.6-documentation.pdf)、[SNIA LTFS2.5 §9.2.13](https://www.snia.org/sites/default/files/2025-03/SNIA-LTFS-Format-v2.5-Technical-Position.pdf)。现场正文验证及限制见[本轮实带报告](../../.scratch/ltfs-ha/reports/fuse-s3-ee-physical-20261104.md)。
