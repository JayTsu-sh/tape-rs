# IBM EE 资源与状态模型的借鉴与状态归属

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: claude-ee-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 14, 17, 18, 36

## Question

用户目标是“模仿 IBM EE，做一个基于 Raft 的带库软件”。既有票据集中在控制面授权与卷提交，尚未回答 EE 产品层最基本的资源模型：带库、节点组、节点、驱动器、磁带池、磁带各自有哪些属性与状态码，它们之间的约束是什么，以及在本项目“三节点 Raft、Leader 唯一执行、不依赖 GPFS”的前提下，这些状态分别应归属于 Raft 复制的控制状态、Leader 本地可重建目录、磁带自描述内容，还是带库实时库存。

## Scope

只核对《IBM Spectrum Archive EE V1.3.2.2》指南 §1.3.1、§1.4、§6.6、§6.8–6.10、§6.17–6.20、§6.22–6.23、§7.15–7.20、§10.1.2–10.1.4。不核对 EE 源码或实验室实际配置；不移植 GPFS/DMAPI/stub、mmapplypolicy 或 SNMP 细节；不裁定 WORM、加密密钥服务器、3592 介质及双带库的支持范围。页码按“印刷页 / PDF 页”标注，偏移 24。

## Answer

限定研究完成，形成[资源与状态模型候选](../ee-resource-model.md)。以下为有原文页码支撑的事实与本项目处理。

### EE 的资源实体与硬约束

| 实体 | 原文事实 | 本项目处理 |
| --- | --- | --- |
| EE 节点 | 每个节点连接一个逻辑带库内的一组驱动器，一个节点不能连接多个逻辑带库；多节点访问同一带库时各节点驱动器集合不共享，且每个节点须有自己的控制路径（changer）设备（§1.3.1 印刷页 9 / PDF 33；§1.4 印刷页 15 / PDF 39） | 首版由 Leader 独占整组驱动器，节点—驱动器分配收缩为“受管设备组”的成员集合；三节点是否各自拥有 changer 控制路径是 09 部署前提，须现场核对 |
| 节点组 | 连接同一带库的节点集合；池只属一个节点组，节点组可访问多个池（§1.3.1 印刷页 10 / PDF 34） | 首版一个受管设备组即一个节点组；不预先实现多节点组 |
| 控制节点 / MMM | 每带库一个控制节点运行 MMM，维护全部资源状态、驱动器目录、磁带目录、每盘磁带的空闲空间估计并分配空间；MMM 停止则迁移/召回失败（§1.3.3 印刷页 13–14 / PDF 37–38） | 对应本项目 Leader 上的执行实例；资源目录为 Leader 本地可重建状态，不进入 Raft 日志 |
| 磁带池 | 同一逻辑带库内同类型（WORM/非 WORM、LTO/3592）磁带的集合；不跨带库；只属一个节点组（§1.3.1 印刷页 9 / PDF 33）。属性含 poolid、devtype、mediarestriction、format、worm、nodegroup、fillpolicy、owner、mountlimit、lowspacewarning*、nospacewarningenable、mode（§6.10 印刷页 164 / PDF 188） | 采用池作为上传目标与容量监控单位；属性集合按候选表裁剪，数值留空 |
| 池的容量监控 | 每 30 分钟检查低空间阈值（TiB），每 24 小时发一次 trap；无空间告警由迁移因池满失败触发（§6.10 印刷页 163 / PDF 187） | 借鉴“阈值告警”和“失败触发告警”两类事件；周期与阈值留空，不绑定 SNMP |
| 驱动器角色 | migrate(4)/recall(2)/generic(1) 位或组合；格式化、检查、对账、回收、校验属 generic；回收需同一节点两台 generic 驱动器；角色变更在当前任务完成后生效（§6.20 印刷页 207–208 / PDF 231–232；§6.8.5 印刷页 161 / PDF 185） | 保留三种角色位作为调度输入；“同一节点”约束在 Leader 独占模式下自动满足 |

### EE 的状态码

| 对象 | 原文事实 | 本项目处理 |
| --- | --- | --- |
| 磁带 | `eeadm tape list` 同时显示 Status（ok/info/error）与 State；State 共 21 种：appendable、append_fenced、check_hba、check_key_server、check_tape_library、data_full、disconnected、duplicated、exported、full、inaccessible、label_mismatch、missing、need_replace、need_unlock、non_supported、offline、recall_only、require_replace、require_validate、unassigned、unformatted（§10.1.4 印刷页 320 / PDF 344）；每种状态允许的下一步命令见 Table 10-5（印刷页 321 / PDF 345） | 不照抄扁平枚举；拆成归属、健康、追加资格、位置四个正交维度，见候选文档；命令准入矩阵改为按维度的守卫条件 |
| 驱动器 | Status ok：mounted、standby、mounting、in_use、not_mounted、unmounting；info：locked、check_node、unassigned、disabled；error：check_tape_drive、not_installed、missing、check_drive_path（§10.1.2 印刷页 317–318 / PDF 341–342） | 运行态由 Leader 本地维护；assigned/disabled/unassigned 属管理意图 |
| 节点 | Available、License Expired、Unknown、Disconnected、Not configured、Error（§10.1.3 印刷页 319 / PDF 343） | 节点健康由控制面探测与 18 的资格门控承担，不另建平行状态机；License 状态不适用 |
| 文件 | resident、migrated、premigrated、saved、offline、missing（§10.1.3 印刷页 319 / PDF 343）；`eeadm file state` 列出每份副本所在磁带及磁带状态（§6.22 印刷页 213–214 / PDF 237–238） | 本项目没有 GPFS 驻留态与 stub，文件状态改为按上传/提交状态机；保留“文件→副本→磁带→磁带状态”的定位查询形态 |

### 磁带生命周期操作的约束事实

- 未格式化磁带不能加入库；`tape assign` 自动格式化，仅在未格式化、已格式化但无数据、标签无效或两分区标签不一致时才允许格式化，否则须 `-f` 强制（§6.8.1、§6.8.3 印刷页 154–158 / PDF 178–182）。
- 含有迁移/保存文件的磁带不能换池；空磁带可 unassign 后再 assign；建议换池前先回收（§6.8.2 印刷页 156–157 / PDF 180–181；§7.19 印刷页 254 / PDF 278）。
- 在线且属于池的磁带不能移到 I/E 槽；I/E 槽内磁带不能加入池（§6.8.2 印刷页 157–158 / PDF 181–182）。
- `tape unassign -E` 通过读取卷缓存并核对每个条目是否仍被引用来判断“有效空”，命中第一个活动文件即中止（§6.8.3 印刷页 159 / PDF 183）。
- 对账默认只更新内部元数据以刷新 Reclaimable%，`--commit-to-tape` 才写回磁带；同一时间只允许一个对账任务，对账期间禁止新迁移、等待在途迁移完成，索引更新的短暂窗口内不可召回（§6.16 印刷页 197–198 / PDF 221–222）。
- 回收把被索引引用的文件复制到另一盘磁带后重新格式化源带；召回优先于回收，回收被中断后自动恢复（§6.17 印刷页 200–201 / PDF 224–225）。
- 导出先预留磁带、自动对账、再从命名空间删除该带文件并强制脱离池；离线则保留命名空间条目但标记不可访问；多副本文件只有全部副本导出/离线才整体不可用（§6.19 印刷页 202–206 / PDF 226–230）。
- need_replace（读错误）/require_replace（写错误）用 `tape replace` 迁走数据；check_tape_library/require_validate/check_key_server 用 `tape validate` 恢复（§6.18 印刷页 201–202 / PDF 225–226）。
- 未经导出即物理取走磁带会导致访问失败且无位置信息；取出后修改再插回行为不可预测（§6.19.2 印刷页 204 / PDF 228）。
- EE 在池创建/删除、磁带 assign/unassign、驱动器 assign/unassign、节点增删及控制节点变更后自动备份数据库（§6.6 印刷页 152 / PDF 176）——这组操作正是 EE 视为“配置级”的状态变化。

### 状态归属的裁定候选

依自动接受授权，采用[四层归属](../ee-resource-model.md#状态归属)：池定义、磁带归属意图、驱动器分配与角色、受管组成员进入 Raft 复制的低频控制状态；磁带健康/追加资格/容量估计/位置、驱动器运行态、文件放置索引为 Leader 本地可重建目录；文件内容、索引与卷身份仍以磁带为权威；库存事实来自带库实时查询并与意图比对得出 missing/duplicated/inaccessible 等派生状态。该划分与 [18 的最小控制状态](18-raft-execution-contract.md#已接受的业务控制状态范围)及 [36 的不逐文件入库](36-control-store-sqlite-evidence.md#answer)一致，不新增逐文件共识。

不采纳：GPFS stub/DMAPI 属性、文件级 resident/premigrated 语义、mmapplypolicy 策略引擎、SNMP MIB；License 节点状态按用户明确要求不引入。留空：池属性默认值、容量阈值、库存重扫周期、状态转换的具体错误码。

验证：本轮只核对 PDF 文本（本地提取，SHA-256 与 14 一致），未访问实验室、未运行任何命令。RS01—RS06 见候选文档，均未实现/执行。18/04 状态不变，09 依赖不解除。
