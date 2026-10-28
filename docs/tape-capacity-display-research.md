# 磁带容量的用户展示建议

调研日期：2026-10-20。范围：LTFS 磁带、IBM Storage Archive LE/EE 与 tape-rs HTTP。本文是展示口径建议，不代表源码已全部实现；初次调研未访问真实设备，后续经授权只读获取了 EE 实机列表，见末节。

## 结论与厂商依据

**主视图采用数据存储可用空间，列出可用总容量、已占用、剩余；IP 和 DP+IP 放在详情。** EE 的 `eeadm tape list` 展示 Usable、Used、Available、Reclaimable%、状态和位置，不把 IP 当作另一块用户文件空间。[IBM EE 命令说明](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-list)

IBM LE 区分介质原始容量与格式化后的可用容量；例如 LTO-9 为 18 TB 与 17.55 TB。压缩收益取决于内容，JPEG/MPEG 等通常收益很小。建议默认十进制 TB，允许切换 TiB；API 始终返回整数 bytes，名义容量和宣传压缩容量仅作介质规格，不能据此预测还能写多少文件。[IBM LE 容量表](https://www.ibm.com/docs/en/storage-archive-le/2.4.8?topic=planning-supported-tape-drives-media)

**“剩余物理空间”和“允许追加空间”必须分开。** EE REST 定义 `capacity = used_space + free_space + non_appendable`，不可追加时 `free_space` 为零，余下不可写空间另列；`used_space = active_space + reclaimable_space`。这不是单纯照搬 MAM 三项。[IBM EE REST](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=information-rest-api-tape)

## tape-rs 推荐字段

| 层级 | 用户名称 / 字段 | 口径 |
| --- | --- | --- |
| 主视图 | 数据可用总容量 `capacity` | DP 面向用户的容量；如扣除保留区，应注明规则 |
| 主视图 | 已占用 `used_space` | 与总量、剩余采用相同物理口径，不等同当前文件大小 |
| 主视图 | 可用空间 `free_space` | 按 EE 形式分配：可追加带的 DP 余量；已知不可追加带为 0，其余量计入 non_appendable_space |
| 主视图 | 可否追加 `appendable`、原因 | 综合写保护、介质健康、策略和索引提交状态；未知也须可表达 |
| 详情 | 当前文件逻辑字节数 | catalog 的活跃文件大小，单独展示 |
| 详情 | 可回收估算 | 不能拿物理已占用减逻辑文件字节数直接求得 |
| 技术详情 | `partitions.data/index` | 分别显示物理 total/used/remaining，注明设备分区号和角色 |
| 技术详情 | `volume_capacity` | 仅为两分区观测合计，标注“含索引，非用户文件可用容量” |
| 共用 | 观测时间、缓存/失败标记 | 跟随数值展示；UUID、代数、来源供详情/API 使用 |

用户随后明确选择 EE 表现形式：HTTP 主字段改用 EE 的可用/不可追加空间划分，保留 non_appendable_space 与 non_appendable 同值别名，并在接口文档显式说明 free_space 的语义变化；原物理剩余改由 partitions.data.free_space 提供。上游 LTFS 用户容量还会扣除保留空间，并按 NO_SPACE 状态归零；这是该实现的预留策略，不是 LTFS 标准强制规则。因此原始 DP MAM 应标明“设备容量观测”，不能声称已经完全复现 LE/EE 的用户可写口径。[上游容量计算，固定提交 b31172c](https://github.com/LinearTapeFileSystem/ltfs/blob/b31172cd4a85d95c718707d76992cca4d4103a97/src/libltfs/ltfs.c#L683-L745)

示意（虚构数值）：`TAPE001｜数据容量 17.55 TB｜已占用 6.20 TB｜剩余 11.35 TB｜可追加｜槽位｜上次观测 10:20`。详情展开 IP、整卷合计及当前文件逻辑大小；离线时保留数值并显示“历史观测”，不能把有剩余显示成当前可写。

## 索引空间与写入状态

IP 不是像 DP 一样不断消耗的追加日志。IBM 的技术说明表明 IP 使用覆盖模式、DP 使用追加模式并保存历史索引。因此 **IP 的 MAM 剩余为零，本身不足以推断 DP 无法写入**；还要判断新完整索引能否覆盖写入、实际提交是否成功。[IBM/SNIA 技术演讲第 17 页](https://www.snia.org/sites/default/files/SDC/2019/presentations/File_Systems/Ishimoto_Takeshi_Pease_David_Developments_in_LTO_Tape_Hardware_and_Software.pdf#page=17)

SNIA 规范区分正常同步与最终一致状态，IP 只能存完整索引。结合项目现有 DP 提交、卸载时 IP checkpoint 的流程，建议分别展示“数据空间不足”和“索引检查点失败”。若索引确实放不下，应停止宣称可正常追加并提示处理；不能因为 DP 余量大而展示健康，也不能仅凭 IP 剩余百分比触发“整带满”。这是产品建议，不是厂商规定的统一阈值。[SNIA LTFS 2.5.1 第 4.1.4、9.2 节及附录 C](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)

## 缓存、告警与实现边界

未装带沿用同一 UUID 的成功缓存，装载、写入提交及卸载检查点后由 owner 刷新，再经 Raft 传播，是合理方案；“在 drive”仍不等于实时值。刷新失败保留旧数值并显示失败时间，格式化/UUID 改变则作废；未知为 `null`。LE 自身也缓存每分区 MAM 和容量，并在卸载时写缓存。[IBM LE 缓存说明](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=management-index-cache)

池汇总须显示已知/未知磁带数量；有未知成员时不能把部分和标成完整容量。告警应分为 DP 低空间、DP 无空间、索引提交异常、观测过期/失败，未知不等于满。LE 的 `df` 返回固定文件系统容量及零已用，不可作为单带容量源。[IBM LE 文件系统状态说明](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=tips-remaining-capacity-storage-archive)

另：IBM 提供独立 DP/IP total/available 扩展属性，原单位为 1,048,576 bytes；API 应换算后明确单位，不能仅凭字段名猜测 MB 或 bytes。[IBM 分区容量属性](https://www.ibm.com/docs/en/storage-archive-sde/2.4.6?topic=attributes-virtual-extended)

项目当前边界（主线程提供，未在本研究重复源码审计）：用户选择 EE 表现形式后，主字段按磁带/池策略划分可用与不可追加空间；capacity/used 的基础仍为 DP MAM，未复现 IBM 私有活跃/可回收对象计量和预留区规则，相关对象计量为 null。分区详情和整卷合计保留原始物理读数。文档和客户端应区分这套容量展示与设备实际写入准入，不将 free_space 当成保证可写的逻辑文件字节数。

## EE 实机列表核对（2026-10-20）

用户限定现场只运行 `eeadm tape list`。在 lisa-2 / 10.131.7.208 执行 `/opt/ibm/ltfsee/bin/eeadm tape list`，返回码 0；客户端采集时间为 09:33:30–09:33:35 UTC（北京时间 17:33:30–17:33:35）。仅执行这一条远程查询，没有使用额外参数、查询其他资源或进行磁带操作。原始 stdout、stderr 和客户端执行元数据保存在本地 `.scratch/ltfs-ha/probes/results/ee-tape-list-readonly-20261020T093330530595Z/`。此前一次连接认证失败，远程命令未执行，失败证据另存。

原始表头：`Tape ID / Status / State / Usable(GB) / Used(GB) / Available(GB) / Reclaimable% / Pool / Library / Location / Task ID`。

| 磁带 | Status | State | Usable(GB) | Used(GB) | Available(GB) | Reclaimable% | Pool | Library | Location | Task ID |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- | --- | --- |
| AA0014L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| AA0015L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| AA0016L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| AA0017L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| AA0018L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| CLNU03L1 | info | non_supported | 0 | 0 | 0 | 0% | - | EE_REA | ieslot | - |
| L00283L8 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |
| RC0017L9 | ok | appendable | 17549 | 0 | 17549 | 0% | REA_P0 | EE_REA | homeslot | - |
| RC0018L9 | ok | unassigned | 0 | 0 | 0 | 0% | - | EE_REA | homeslot | - |

对展示实现的约束：

- `Status` 是严重程度，`State` 是业务状态；`unassigned` 不应因此显示为异常。实机正常数据带都是 `ok`，不能用 `unknown` 代替已知的正常分级。
- 主列表采用 Usable、Used、Available、Reclaimable%，IP/DP 仍作为详情。Usable 指格式化后的可用容量；表头为十进制 GB。列表显示的整数 `Used=0` 不能证明没有数据，`Reclaimable=0%` 不能证明精确可回收字节数为零。[IBM 命令字段说明](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-list)
- 本次所有未入池磁带的列表容量都是 0；这只是 EE 列表表现，不能据此将设备容量认作零，或清除本地已有的容量缓存。RC0018L9 在 EE 中为 unassigned，其显示不代表 Rust 管理的介质内容为空。
- RC0017L9 位于 homeslot 仍显示容量，证明列表可在介质不在 drive 时提供容量；本次只查询列表，不能据此确认缓存刷新机制、时间或原始分区读数。
- 清洗带实机为 `info / non_supported`，而用户提供的 1.3.5 状态表将 `non_supported` 列在 `error` 分组。既有交接记载该环境为 EE 1.3.4.0，本次没有运行版本查询；这项差异保留为观测，不推断唯一原因。状态映射应明确采用的版本/策略。[IBM 1.3.5 状态与严重程度说明](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)

本轮未修改生产代码、未部署 tape-rs，也未验证新 HTTP 管理流程；以上是 EE 参考端的真实只读输出。

## 后续实施：EE 主容量与物理详情分开

用户要求按上述建议实施后，管理 HTTP 投影作了以下调整；本节描述后续源码改动，不改变前节实机查询的证据范围：

- 未入池主容量 capacity/used_space/free_space/non_appendable_space 均为 0，capacity_source=unassigned、capacity_cached=false，主容量观测时间/代数/刷新结果为 null。DP/IP 及整卷物理观测不删除，重新入池继续采用有效缓存。已入池但没有有效容量观测时仍为 null。
- 严重程度采用明确的 EE 1.3.5 映射及本项目状态扩展；正常状态为 ok，full 为 info，check/label_mismatch 为 error。离线或未入池不降低已知介质健康告警；未知状态不猜测为健康。清洗带单独采用现场 EE 的 info/non_supported，设备识别规则见后续补充；其他 non_supported 保持文档 error 映射。
- 增加 reclaimable_percent=null，与尚未实现的活跃/可回收空间计量一致；不复制实机整数显示的 0% 冒充已计算结果。
- 池及其分区汇总增加 capacity_complete、capacity_known_tapes、capacity_unknown_tapes。任一成员容量未知或求和溢出则相关汇总保持 null；主汇总还要求追加资格已知。完整性不等于实时性，仍须查看成员的观测时间和刷新结果。
- HTTP 继续返回整数 bytes，没有新增文本渲染层。界面的十进制/二进制单位、小于 1 GB 非零值及未知值的显示约定写入 docs/file-client.md，避免从舍入后的 GB 数字反推字节。

保持原有 owner 采样及 Raft 复制流程：GET 不访问设备、不装带，不新增轮询、容量预留或写入准入策略。IP 剩余为零不单独判定 data_full；相关状态与索引检查点失败仍使用现有设备执行路径。容量基础仍为 DP MAM，尚未实现 IBM 私有对象空间计量及预留算法。

## 物理专项验收结果

2026-10-20 在三台 lisa 物理节点、IBM TD9 11EB4A80F1 / 换带器 55L3A7802K19LL01 完成容量专项验收，使用现有 RC0018L9 追加数据；详细证据和范围见[现场报告](../.scratch/ltfs-ha/reports/admin-capacity-physical-20261020.html)。

DP/IP 的 HTTP 数值与独立 `sg_read_attr --readonly` 原始 MAM 响应一致，三节点经 Raft 收敛一致。48 次管理 GET 未增加搬带、装卸、MAM 读写或写带命令；离线归槽后保留物理缓存，主 Available 归零、余量转入 non_appendable，池汇总一致；上线和 owner 转换后继续刷新。实写后已用容量增长，正常检查点自身也消耗 DP 空间，再次说明 Used 不等同当前用户文件大小。

容量报告与目录摘要分别提交，现场曾观察到容量 gen108、目录 gen107 的短暂窗口，`capacity_generation_matches=false` 正确反映该差异。比较一致性时应等待收敛，不能把 false 一概解释为旧缓存或采样失败。管理任务终态刚提交时，执行线程也可能短暂返回可重试的 drive_busy；首两轮探针因未等待这些窗口而中止，其失败与收尾证据保留，最终 R3 按实际异步协议等待后完成。

三份 16 MiB 验收文件共新增 48 MiB，最终实读 SHA 均通过；原有 64 MiB 文件前后 SHA 一致。RC0017L9 未装载或读写。测试候选已停止，无残留 VIP，EE 及原设备预留恢复，RC0018L9/RC0017L9 分别在槽 7/8；独立复核通过。

该轮识别边界：当时清洗带 CLNU03L1 在 HTTP 库存中为 unassigned/ok，尚未自动识别 EE 的 non_supported；后续识别改动见下节，原验收证据保留。成功解除分配后的缓存保留由软件回归覆盖，本轮因 RC0018L9 有文件，只验证非空解除分配被拒绝。未进行写满、格式化、UUID 变化、MAM 读取故障、掉电或内核冻结测试，也未实现 IBM 私有预留和对象空间计量。

### 2026-10-21 清洗介质识别

IBM TS4300 SCSI Reference 的 READ ELEMENT STATUS 定义 descriptor byte 9 的低 3 位为 Medium Type，2 表示 Cleaning；不依赖装入 drive。[IBM 官方 SCSI 手册，Storage/I/E/DTE 状态页](https://www.ibm.com/support/pages/system/files/inline-files/%24FILE/SC27-4641-00_1.pdf)

新增库存 medium_type、cleaning、cleaning_source，优先设备类型 2，兼容标准 CLN 条码；普通 DATA01L1 不会仅因 L1 后缀被判断为清洗带。HTTP 采用现场 EE 的 non_supported/info，容量业务零、appendable=no，分区物理容量未知；不把清洗带作为普通未分配数据带，不启动清洗。分配和 LTFS 数据任务拒绝已识别清洗带，实际装载前由新鲜库存再次防护，包含旧分配记录。新字段无需数据库迁移；旧缓存使用条码兜底，重新 rescan 后获得设备类型。

现场只读验证通过：lisa-2 的新鲜 SCSI 响应确认 CLNU03L1 位于 I/E 口 2、medium_type=2；新 CLI 标注清洗带，生产 HTTP 共享投影返回 non_supported/info、appendable=no。未启动物理 HA HTTP 服务；在线端点流程由三节点模拟设备集成测试覆盖，正式服务尚未升级。[验收证据](../.scratch/ltfs-ha/probes/results/cleaning-identification-20261021/summary.json)
