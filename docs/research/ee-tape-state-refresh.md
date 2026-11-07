# EE 磁带 Status / State 的刷新机制

调查日期：2026-11-04。问题：EE 是否有常驻组件刷新磁带状态，`tape list` 是否现场检查磁带，以及库存刷新、介质健康检查分别能证明什么。本次只查 IBM 第一方资料与仓库已有物理实验报告，没有启动 EE、远程登录、发送 SCSI 或修改运行数据。

## 结论

**EE 有常驻管理与监控组件，也有自动更新 library inventory 的机制；但不能把这解释为定时把所有磁带装入驱动器、完整读盘并刷新全部健康状态。** 磁带列表综合了资源观测、管理配置、操作结果及错误证据。库存发现、读写时发现故障、显式 `tape validate` 是不同的更新来源。[IBM 组件说明 1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=overview-storage-archive-enterprise-edition-components)、[IBM Redbook 1.3.2.2 §1.4、§6.8.1](https://www.redbooks.ibm.com/redbooks/pdfs/sg248333.pdf#page=40)、[IBM 状态表 1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)。

目前可以回答“有常驻刷新机制”，不能回答“现场 EE1.3.4.0 每隔多少秒刷新所有磁带健康状态”。旧版文档明确说物理资源获取会周期自动执行；较新版文档说库存自动维护。已有物理查询窗口没有测出周期，也未观察到全库逐带健康巡检。**周期数值、触发条件和是否存在额外后台介质检查，仍缺少针对当前二进制的直接证据。** [IBM Redbook 1.2.6 p320，旧 `ltfsee retrieve`](https://www.redbooks.ibm.com/redbooks/pdfs/sg248445.pdf#page=342)、[IBM Redbook 1.3.2.2 p154](https://www.redbooks.ibm.com/redbooks/pdfs/sg248333.pdf#page=178)。

## 常驻组件及更新来源

| 来源 | 已有依据 | 能更新或发现的内容 | 不能据此推断的内容 |
|---|---|---|---|
| MMM | 每库 active control node 上的服务，维护资源状态、驱动器/磁带目录与容量估计，并调度任务 | 管理资源视图及执行结果 | MMM 常驻不等于持续读取所有磁带正文 |
| MD | 每节点常驻，监控本地/远端 MD、MMM、LE、Scale、网络及系统服务；MD 之间周期通信 | 节点/组件不可用与部分软件自动恢复 | MD 不是全盘介质校验器；IBM 明确硬件故障由管理员处理 |
| LE | 每节点随 cluster start/stop 启停，通过文件系统接口访问磁带数据及元数据 | 装带、挂载、读写期间可获得的介质观测与故障 | 仅服务存活不证明某卷已重新读取 |

以上为 IBM1.3.6 组件文档的职责说明；同类 MMM/MD 职责在 IBM1.3.4 页面也有说明。此处没有把 MD 心跳周期当作 inventory 周期。[IBM 组件说明 1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=overview-storage-archive-enterprise-edition-components)、[IBM 组件说明 1.3.4](https://www.ibm.com/docs/en/storage-archive-ee/1.3.4?topic=overview-storage-archive-enterprise-edition-components)、[IBM 启停说明 1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=ee-how-start-stop-storage-archive)。

**自动 inventory / 显式 rescan。** Redbook1.3.2.2 说明 inventory 保存在 MMM 所在节点并自动保持更新；新插入介质若尚未出现在列表，应显式 rescan。`eeadm library rescan` 的公开契约是向库获取物理资源并更新 EE 资源记录。因此适合处理位置、缺带、重复条码、不可达槽位等资源观测；该契约没有承诺读取每盘 LTFS index 或全部 payload。[Redbook1.3.2.2 p16、154](https://www.redbooks.ibm.com/redbooks/pdfs/sg248333.pdf#page=40)、[IBM rescan1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=library-eeadm-rescan)。

**操作中发现的错误。** IBM 状态表把 `need_replace` 对应到已检测的永久读错误，把 `require_replace` 对应到永久写错误；mount 错误、元数据不一致也各有状态。结合 LE 的 I/O 职责，可推断正常挂载/读写是产生这些错误证据的入口之一；这不是本轮通过真实故障注入证明的内部完整调用链。[IBM 状态表1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)。

**显式 validate。** `eeadm tape validate` 明确装带检查当前条件并更新 tape state，通常用于 `require_validate`，也接受若干健康/错误状态。它属于有机械动作的介质任务；文档没有声称默认完整读取所有文件，也没有 `--full-read` 选项。状态恢复须按错误类别处理，不能把 rescan 成功或一次短读成功当作清除既有永久错误的依据。[IBM validate1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-validate)。

## `Status`、`State` 与恢复条件

`Status` 是摘要等级，`State` 才是具体条件；例如 IBM1.3.5 将 `need_replace` 列为 `info`，但其描述仍是永久读错误，不能把所有 `info` 当作健康。`require_replace` 是 `degraded`；`require_validate` 是 `error`。资源缺失与槽位问题按 rescan 链路处理，挂载/元数据问题常要求先修原因再 validate。读写保护、offline、Pool 格式限制等还包含管理策略，不能全部解释为驱动器 sense。[IBM 状态表1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)。

IBM 明确说明：`tape replace` 成功后，原 `need_replace` / `require_replace` 卷仍保持原状态，只是离开池、可移出库。因此这些状态不应因库存刷新或数据搬迁成功自动变成 `appendable`。这里的“永久”是 IBM 对已检测错误的分类；不能进一步断言在所有物理移除/重新分配/格式化流程后标志永不改变。`validate` 接受此类状态也不代表保证清掉它们。[IBM 状态表1.3.5](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)、[IBM validate1.3.6](https://www.ibm.com/docs/en/storage-archive-ee/1.3.6?topic=tape-eeadm-validate)。

版本须保留：IBM 状态页注明独立的逻辑 `recall_only` 与物理 `write_protected` 区分从1.3.4.3开始；现场安装版为 **1.3.4.0-53233 / LE2.4.6.0-10507**，不能按新文档假设它已有相同属性集合。[IBM 状态表版本注](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=list-tape-status-state-information)、[IBM1.3.4变更记录](https://www.ibm.com/docs/en/storage-archive-ee/1.3.5?topic=new-version-134)。

## 已有真实 TS4300 证据及其限度

下表来自已保存的[生命周期研究报告](ee-management-lifecycle.md)与[索引/导入报告](../../.scratch/ltfs-ha/reports/ee-index-import-model-20261103.md)，不是本轮重新执行的实验。环境为真实 FC IBM 库 A **55L3A7802K19LL01**、drive **11EB4A80F1**、上述 EE/LE 版本；Holo 结果不纳入。

| 已有实验报告的观察 | 支持的结论 | 观察边界 |
|---|---|---|
| library list/show、tape list/show 成功；前台及 MMM/MD/LE 全线程查询窗口，前后各15秒空闲对照，物理 SG_IO 均0 | 本次查询返回已维护的资源视图，没有由查询触发物理检查 | 不能推出后台永无周期刷新，也不能量出自动 inventory 间隔 |
| 显式 `library rescan EE_REA_A` 捕获18条真实 A changer CDB：15 TUR、1 MODE SENSE、2 READ ELEMENT STATUS | rescan确实访问物理库存，给零 SG 查询提供阳性对照 | 未见装带读 index；数量只是本次窗口，不是固定协议配方 |
| AA18 validate task1027 成功；150条真实A SG，10条READ(6)响应含LTFS label/index；用户4文件inode/SHA、schema SHA不变 | validate可以装带读取索引并检查状态，没有重建用户namespace | 这次健康路径未捕获WRITE(6)/WRITE ATTRIBUTE；不能外推所有故障路径不写，也不等于验证每个payload |

原始证据的历史根是 `/work/tmp/tape-fdb-ee-management-20261103-4r8xnbqz/rea/physical/`，关键文件名为：`summarize-aa18-successful-queries-208.json`、`query-front-IPC-observations.json`、`rescan-physical-normalized.json`、`run-aa18-reconcile-validate-208.json`、`summarize-entrypoint-SG-208.json`。**本轮该旧目录已不存在，未重新打开原始 trace 或重新校验 archive**；上述数值按仓库报告记录引用。报告登记原始包 `physical-native-evidence-20261103.tar.gz` 为88,604,981B，SHA256 `1de118145913781456f42798ad5b0216984ed4d35a84d9b8a2c7e14bb3e239cb`，不能把本轮复核文档写成已复核此包。

同一[生命周期报告的冻结 REA 节](ee-management-lifecycle.md#冻结-rea-依据与不可推断内容)记录了 EEADM `TapeListCmd` 包装层到 MMM `GetAllTapeList` 资源对象的静态链路：EEADM SHA `b39403378192eadbcf5b50a2bf66eb8ccabd7b3d454a9d95fd938374fc5ae29f`，MMM SHA `c90c45514a5b31b101dd1351dedf497ebbb9416ba46e1b19922df22d541403ef`；包装层证据ID `ev_d0bff31bd08394848bf05130cae25698cc960ff142469488e5cb1b3b25c3d192`、GetAllTapeList `ev_ea21204a691cf01c502ddf09563c4b50a63b3222dde42ccbcb240c213473ba08`。部分反编译存在no-return截断；它只支持查询架构解释，不证明所有后端无SCSI，更不提供周期数值。本轮未重新运行REA。

## 仍未证实

- 当前 EE1.3.4.0 自动 inventory 的精确间隔、配置入口及完整触发条件；MD周期通信不能替代该证据。
- 当前版本是否还运行独立介质健康巡检、范围多大；已有文档和短窗口不足以证明存在或不存在。
- 永久读/写错误、key server/HBA错误等真实异常的完整状态转换和持久化细节；健康卷validate成功不能覆盖这些路径。
- 本轮没有新增物理实验。当前项目 healthy、EE stopped 的现场状态以最新交接为准；研究报告不授权重新启动旧实验或接管设备。

本次资料校验：IBM官方HTML页面直接打开部分返回403，使用搜索工具返回的IBM原页正文，并直接读取两个官方Redbook PDF定位页码；没有依赖第三方文章。只新增本文，未运行Rust测试或硬件验证。
