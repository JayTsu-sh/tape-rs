# IBM Storage Archive EE 官方能力验收基准（2026-10-21）

这是本轮现场 GAP 核查的**文档基准**，不是现场验收结果，也不判断 tape-rs 是否缺少相应功能。仅采用 IBM 官方 EE 文档；LE/SDE 单独具备的功能不算 EE 依据。以下只读方法用于确认现场对象、配置和历史结果，不能替代写入、召回、故障切换等行为验收。

## 版本与证据口径

- 产品目标基准为 EE 1.3.5，现场必须另存 `eeadm --version` 和已安装 RPM 的实际版本。1.3.5 新 REST 提供 PAM/SSH 认证和 token 授权；不能把该版本的文档能力直接记为现场较旧版本已提供的能力。[1.3.5 变更记录](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=new-version-135)
- `recall_only` 属性及其与物理写保护的明确区分从 1.3.4.3 起提供；`library rescan --task`、优化 rescan 是 1.3.4.4 的变更。必须记录 fix pack，不能只写“1.3.4”。[1.3.4 变更记录](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=new-version-134)
- 下表明确标记补充引用的 1.3.4 文档；搜索器对若干 list 页面返回 1.3.6，本清单没有拿 1.3.6 新行为补作 1.3.5 已有能力。
- “命令存在/帮助可用”“实际配置可读”“历史成功记录”“本轮执行成功”分别记录。只有后两类能证明发生过操作，且历史结果必须标明时间、版本、介质、对象与输入输出；无法复现的场景保持待验证。

## 25 项有限验收基准

所有“官方能力”均为文档声称，**本文件未连接现场主机、未执行设备命令**。只读命令采用明确对象，文件查询先确认路径属于允许检查的测试目录。普通文件内容读取可能触发透明召回，不属于无副作用查询。

| ID | 能力与官方依据 | 现场优先采用的只读确认 | 仍需行为验收的内容 |
|---|---|---|---|
| EE01 | 集群/节点健康：可观察 MMM、LE、MD、GPFS、SSHD、rpcbind、rsyslog。[系统健康](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=status-getting-system-health) | `eeadm node list`、指定节点 `node show`；核对实际进程与 GPFS 状态。 | 故障检测、恢复后状态收敛；不能用健康列表证明自动恢复成功。 |
| EE02 | 控制节点冗余、节点扩展及多带库控制转发；同一带库同时只有一个活动 MMM。[集群配置](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=overview-introduction-storage-archive-enterprise-edition-cluster-configuration) | `library list/show`、`node list/show`、`nodegroup list`；保存配置及已有切换日志。 | 手动/故障切换、跨节点并行吞吐；一个可用节点不能验证双节点接管。 |
| EE03 | 驱动分配、解除分配、up/down、属性设置与展示。[drive 命令](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-eeadm-drive) | `drive list`、指定序列号 `drive show`，核对角色、节点、挂载介质和任务。 | 调度是否遵守角色；维护时是否排空并恢复。解除驱动分配可能自动卸带，不能当只读探针。[维护说明](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=ttl-taking-drive-offline-run-diagnostic-operations-drive-cartridge) |
| EE04 | 池的设备类型、介质限制、3592 格式、并发 mountlimit、低容量告警阈值和运行模式。[pool show](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=pool-eeadm-show) | `pool list`、逐池 `pool show`，核对真实值而非只看帮助。 | 各限制是否真正影响分带/并发和告警；3592 专有字段不强加给仅 LTO 的现场。 |
| EE05 | 新增介质库存同步、homeslot/I/E 移动、必要时 rescan。[介质加入移除](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=drives-adding-removing-cartridges) | `tape list/show` 与带库实际只读库存比对；特别核对清洗带、未分配带、I/E 位置。 | 实际进出仓后的自动发现和 rescan 收敛；rescan 会更新 EE 库存，不能算纯查询。 |
| EE06 | 分配/取消分配和格式化防护；强制格式化须确认数据可擦除；empty-remove 检查活动 stub。[加入池与格式化](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=cdtsp-add-tapes-pools-format-tapes-remove-tapes-from-pools) | 查询池成员、目标 tape show、已有任务日志。 | 重复分配、非空带保护、取消分配条件和格式化；不得为证明支持而格式化现有数据带。 |
| EE07 | tape state 区分 appendable、append_fenced、data_full、full、offline、recall_only、写保护及故障状态；data_full 仍可更新元数据，full 拒绝更新。[状态表](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information) | 同时保存 `tape list` 与 `tape show`，核对显示状态、逻辑访问策略和物理保护。 | 离线拒绝、只召回拒写、分区耗尽和故障映射；健康带不能证明所有错误状态正确。 |
| EE08 | 容量对标需保留格式化可用量、已用、可用、可回收比例及池汇总；1.3.4 官方池示例区分 TB/TiB。[池容量示例（1.3.4）](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=list-eeadm-pool-command-output) | 运行现场 `tape list`、`pool list` 的 decimal/binary/可用 JSON 输出，逐字段保存，并与 tape show、实际已用测试文件核对。 | 本次不能仅凭页面摘录确定现场 tape JSON 字段名、分区口径及非 appendable 的 available 语义；必须以现场输出和写入前后增量确认。 |
| EE09 | migrate 回收磁盘空间，premigrate 保留磁盘副本；resident/premigrated/migrated 为不同放置状态。[状态转换](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=managing-archiving-retrieving-files-from-tape) | 查询已知测试文件 `file state` 及已完成 migration/premigration 任务。 | 对同一测试内容迁移，比较文件逻辑大小、占用块、stub 属性与带上内容。上传一个对象不能直接证明 GPFS HSM 迁移。 |
| EE10 | mmapplypolicy LIST 可调用 migrate/premigrate/recall/save；EE 的 PURGE 不支持，TEST 始终返回零。[LIST 接入](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-eeadm-list)、[命令语法](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-eeadm-command-syntax) | 读现行策略和调度配置、已完成策略执行记录；不运行会触发迁移的策略。 | 真实策略筛选与调用；不能将 TEST 退出码 0 当作端到端功能通过。 |
| EE11 | 透明召回在普通 GPFS 访问时先取回完整文件再服务读取；主副本不可用时可选冗余副本。[透明召回（1.3.4）](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=recall-transparent) | 查询已知 migrated 文件状态、现行 HSM 设置和历史透明召回任务。 | 对受控 migrated 文件执行普通 read，确认无需专门 API 即可读取且校验一致；read 会触发实际召回。 |
| EE12 | 选择性召回、透明召回优先级以及按带上位置重排召回。[手工召回（1.3.4）](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=recall-manual-eeadm-command)、[召回调度（1.3.4）](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=tape-managing-file-recall) | 查询已完成 selective/transparent recall 任务和顺序日志。 | 同时提交多个受控请求，确认合并、顺序和优先级；单文件读成功不能证明调度优化。 |
| EE13 | 文件状态包含 resident/premigrated/migrated/saved，显示文件 ID、副本数及每份 tape@pool@library。[file state](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=file-eeadm-state) | 只对已知测试路径运行 `file state`，记录完整及 short 形式；避免广泛 glob。 | rename/delete/overwrite 后跟踪引用一致性；HTTP file info 不能默认拥有 CLI 的全部字段。 |
| EE14 | 最多三份带上副本，位于不同池，可跨带库；GPFS inode 记录主/冗余位置。[副本创建](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=migration-replica-redundant-copy-creation) | 查询有副本的已知文件 `file state` 和对应池/介质，核对是否确有两份以上。 | 真正多副本写入和主副本不可用时召回；Raft 的元数据副本不是磁带数据副本。 |
| EE15 | save 存储空文件、空目录、符号链接的名称/对象信息。[命令语法](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-eeadm-command-syntax) | 查询已知 saved 对象的 `file state` 和任务历史。 | 保存并恢复三类对象，核对类型与链接目标；普通非空文件写带不覆盖此项。 |
| EE16 | 导出与导入涉及 GPFS 命名空间移除/重建，磁带文件保留，可供其他 LTFS 产品读取。[管理功能汇总](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-summary-commands-by-management-function) | 仅查询既有 import/export 历史和其对应对象。 | 受控跨系统导入/导出、路径冲突及 LTFS 互操作；机械手进出 I/E 不能代替逻辑 export/import。 |
| EE17 | reconcile 检查 GPFS 与 LTFS 对象一致性，确定未引用块及可回收容量，并有操作互斥和召回协调。[文件协调](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=pools-managing-file-reconciliation) | 查询既有 reconcile 任务、容量与文件状态。 | 删除/覆盖测试文件后的引用修正、容量估算和并发隔离；reconcile 会更新索引与状态。 |
| EE18 | reclaim 复制有效内容、更新 inode 位置，再格式化源带；--unassign 可保留源带格式并移出池。通常要求同节点至少两台 g 角色驱动。[回收](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=pools-managing-reclamation) | 查询驱动数与角色、源目标容量及历史任务；检查是否具备真实前置条件。 | 有效文件重定位、召回优先、取消后协调和源带后处理；单驱动现场不能记为完成了 EE 回收验收。 |
| EE19 | datamigrate 跨池复制并更新 GPFS inode，完成后移出源池而不格式化；源目标池同节点组要求一节点至少两台 g 驱动。[跨池迁移](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=technologies-managing-data-moved-different-pool) | 查询池/节点组/驱动与既有任务。 | 真正跨池移动后读回、断点及召回干扰；仅修改池成员不能代表 datamigrate。 |
| EE20 | replace 处理 require_replace/need_replace，成功后移出池；validate 用于状态恢复。[replace](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=tape-eeadm-replace)、[状态恢复动作](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information) | 查询历史故障、失败文件清单与恢复任务。 | 真实失败恢复需专用故障介质/隔离条件；不能损坏现有带来造证据。无故障条件则保留未验证。 |
| EE21 | task list/show、历史清理；cancel 官方范围为活动 reclaim/datamigrate。[task 命令](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-eeadm-task)、[功能汇总](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=reference-summary-commands-by-management-function) | `task list`，对明确任务 ID `task show`，保存进度、结果、子任务/逐文件信息。 | 接受请求与执行成功区分、可取消任务安全结束；不要将 cancel 扩张为所有任务通用能力。 |
| EE22 | 迁移计算 MD5，同时存于 DMAPI 和 LTFS 属性，召回比较；默认启用，适用于 1.3.3 后新迁移文件。[数据完整性](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=tape-managing-data-integrity) | `cluster show` 读取 filehash 配置；对已知文件读元数据属性与历史校验结果。 | 写入/读回哈希比较、校验失败是否阻止成功；仅计算客户端哈希不能证明服务端强制校验。 |
| EE23 | MD/日志与 SNMP trap 可报告组件故障、恢复、启停及池低容量；官方 MIB 已定义。[SNMP 事件](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=status-get-notified-events-by-using-snmp-traps) | 读日志、服务状态、现有 SNMP 配置及接收端历史；不触发伪告警。 | 告警实际到达、去重/恢复通知和容量阈值触发；有 metrics/日志不等同已有外部告警链路。 |
| EE24 | 1.3.5 REST 登录/token/CSRF、GET 与管理员写权限区分；资源管理异步返回 task_id。[认证](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=edition-rest-api-authentication)、[异步管理](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=edition-how-use-rest-api-manage-resources) | 查询安装包、监听和 TLS 配置；在已有授权凭据下读取 GET 和现有任务。用无凭据 GET 确认拒绝，不提交状态变更请求。 | 普通用户拒写、CSRF/token 到期及 HTTPS 实际行为；1.3.4 没装该 API 时只能记为版本基准差异。[安全建议](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=requirements-security-considerations) |
| EE25 | 配置备份涉及池/数据库；特定 SOBAR 系统迁移可在不召回全部 stub 的前提下转移系统，必须遵循 IBM 审核和序列。[配置备份（1.3.4）](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=managing-backing-up-storage-archive-ee-configuration)、[SOBAR 系统迁移](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=hardware-system-migration-by-using-sobar) | 查已有备份文件的时间、范围及恢复演练记录；配置内容先脱敏。 | 隔离环境恢复后确能定位并读回；备份文件存在不能证明可恢复，SOBAR 也不是任意在线备份都受支持。 |

## 不应误记为项目 GAP 的边界

EE 本身存在限制：单文件不能跨多带分片，非零常规文件与 save 对象的处理不同，硬链接有使用限制，稀疏文件召回会扩展。磁带加密依赖带库和密钥管理配置，不能把“GPFS 加密”推为“磁带自动保留同一加密形态”；这些也应核对现场而非默认 EE 无限制。[EE 限制](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=requirements-limitations)

本清单将“未提供命令/端点”“命令存在但后端未执行”“数据语义不同”“本轮设备条件不足”作为不同待核查问题。最终 GAP 报告应对每一条结论链接**同版本 EE 现场输出 + 本项目现场输出 + 执行对象/时间/版本**；只有文档依据的条目继续标记“文档基准，现场待验证”。
