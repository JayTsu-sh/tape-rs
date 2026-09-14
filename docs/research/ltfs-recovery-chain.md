# LTFS 索引候选、依赖链与异常尾部：限定证据核对

日期：2026-09-14。对应主工作区研究票据 15；独立分支 `research/ltfs-recovery-chain`。仅研究资料，未改源码、未访问设备。页码均为 PDF 阅读器页码，与所引 SNIA 文件印刷页码一致。

## 结论与证据等级

**格式约束已有依据；异常卷的候选搜索、停止条件和无修复安全追加算法仍未完成。** 特别纠正：Annex D 是完整卷示例，不是恢复算法；Annex H 是资料性增量处理说明，H.1 流程图属于原型的高层流程，不是设备恢复命令协议。[SNIA 目录，页 12–13；H.5，页 102](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=12)

| 核对点 | 官方依据摘要 | 本项目采用边界 |
| --- | --- | --- |
| 候选合法性 | 自指针错误的“索引”按 Data Extent 处理；索引构造须完整 | 不能把解析成功的 XML 当成已提交索引 |
| generation | 同分区位置越后，代数不得下降；相邻代数不必加一 | 不用“差一代”判链缺失；不凭未验证的最大值选当前视图 |
| Full 指针 | `previousgenerationlocation` 指向 Full；IP 有同代 DP Full 等特定规则 | 不是通用上一条指针 |
| Incremental 指针 | `previousincrementallocation` 指向最近 Full 之后最近增量；该前驱存在时必须记录 | 历史遍历优先它；不得跳过依赖增量 |
| 重放 | 从前置 Full 出发，按次序应用目标前全部增量 | 先收集并核验依赖，再正序构建候选视图 |

表中格式规则依据：[§5.4，页 26–28](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=26)、[§9.1，页 42](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=42)、[§9.2.4，页 45–46](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=45)。重放说明依据：[资料性 H.3/H.5，页 99、102–103](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=99)。上述摘要不替代各节完整约束或 XML/语义校验器。

## VCI 快速路径：限定前提，不是新提交义务

§10.3 允许在**全部分区 VCI 的 VCR 都等于当前有效 cartridge VCR**时，以最高代数 VCI 定位当前索引；任一过期便不能只凭 VCI 判断整卷一致。全零/全一 VCR 不可用。推荐维护顺序是：完整写索引 → 刷完挂起写 → 无新写地立即读 VCR → 更新所有 complete 分区 VCI。[§10.1–10.3，页 57–59](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=57)

由此提出的项目检查点，而非新增标准文字：

- 所有权、fencing、卷身份已确认，采样期间没有其他写入者；校验 ACSI 标识/版本/UUID、字段长度与位置。不能拿旧节点本地缓存的 VCR 冒充当前介质读值。
- “发现候选”和“完成候选视图”分开。即使位置已可靠确定，最新候选是 Incremental 时仍需验证 Full 基线及依赖链；不能把 §10.3 的快速定位扩大为“一份增量 XML 描述全卷”。
- 单个分区 VCI 新鲜，不足以跳过另一分区；VCI 缺失、过期、损坏或不支持，进入待设计的介质搜索路径，而非宣判卷无数据。
- 这项优化不把 MAM 写入加入每批 ACK 的必要条件，也不引入 GPFS、共享数据库或回执。是否实现、维护频率、设备支持均待后续设计与验证。

## 建议的只读恢复骨架（设计建议，未实现）

1. 在已接受的执行资格和卷身份检查后，收集候选位置及来源，区分“有效快速定位依据”和“仅搜索提示”。
2. 对每个相关候选验证构造边界、完整读取、XML/版本、UUID/实际位置、指针目标类型/范围、代数约束及对象/extent 语义。记录读取失败位置和原始设备错误。部分验证不能发布为“完整有效”。
3. 对目标增量，按真实指针收集到相应 Full，不跨缺失链拼接不同基线；检测环、重复位置和方向异常。允许合法代数跳跃。按 Full → 最早依赖增量 → … → 目标增量正序重放到隔离的候选视图。
4. 独立证明“该视图有效”与“没有未排除的更新候选”。有未读区域、无法分类的更晚内容或冲突证据时，停止操作可以是安全退让，但不能等同于成功完成最新性搜索。
5. 只有当前性和读取安全都成立才发布普通查询/读取。安全追加位置、尾部状态、预算和所有权仍另行验证；不存在由本报告推导出的 EOD-only 写许可。

增量重放实现应另外覆盖同名对象替换、删除、导航目录、重命名子树、属性/extent 的替换语义；不能用通用 XML 合并代替文件系统语义。这是后续校验与测试工作项，不是本轮已经完成的解析器。

**搜索停止的真实缺口：**本次核对的标准章节未给出涵盖读错、残缺标记/索引、未知 EOD 和所有设备位置语义的完整搜索算法。尚不能承诺“从 EOD 后退找到第一份合法 XML 就停止”，也不能承诺“扫完某一分区就保证最新”。后续需写出覆盖证明和失败状态，再用设备模型与实际介质验证；扫描时限和性能目标留空。

## 异常尾部：读证据不等于自动修复

IBM SDE 的 `ltfsck --full-recovery` 会把未索引块恢复到介质上的 `_ltfs_lostandfound`；`--erase-history` 会使较新数据无法恢复；`--deep-recovery` 针对缺少 EOD，带有固件条件和数据可能不可恢复的警告。[IBM SDE 2.4.6：Checking media by using the ltfsck command](https://www.ibm.com/docs/en/storage-archive-sde/2.4.6?topic=ltfsck-checking-media-by-using-command)

IBM 的 `force_mount_no_eod` 跳过 EOD 检查后只读开放；`rollback_mount` 也是旧代只读挂载。这只是产品的受限访问能力，**不证明找到的是最新已提交状态**，也不证明本项目可以安全追加。[IBM SDE 2.4.6：ltfs](https://www.ibm.com/docs/en/storage-archive-sde/2.4.6?topic=commands-ltfs)

| 观测结果 | 本项目当前边界（项目政策，非宣称标准给出完整算法） |
| --- | --- |
| 有 EOD，但尾部有未索引数据 | 不从数据猜路径/上传完成，不自动生成 lost-and-found 或索引 |
| 索引结束标记缺失、XML 残缺或标记组合异常 | 不把残缺构造提升为提交；不自动补标记、覆盖或截断 |
| 定位/读取错误，EOD 无法确认 | 保留错误及不确定区域；不把 I/O 错误转成正常搜索终点 |
| 旧 Full 可读但更晚区域未排除 | 不作为普通当前目录开放，不返回误导重传的确定不存在 |
| 当前视图已另有充分证据，追加仍不确定 | 仅按已接受的读安全门开放；写保持关闭 |

IBM 命令行为不能直接变成本项目的自动接管步骤；本轮没有运行这些命令。资料中的“恢复”必须区分只读验证、内存重放、介质写入修复。任何后续修复能力都需要单独授权、显式目标和数据损失边界。

## 验证与未完成项

- 已下载并读取 SNIA 官方 PDF 相关正文，另以本地渲染查看页 103 的 Figure H.1；它是增量应用流程，而非磁带搜索或修复流程。
- IBM 网页正文通过搜索工具返回；直接打开部分 IBM 页返回 403，未把访问失败当作不存在。来源提供产品边界，不用于补造底层算法。
- 文档格式与链接形态检查；无 Rust 测试、无实机实验。本报告不证明外部 fencing、生效屏障、MAM 更新原子性、读错/EOD sense 分类或具体安全追加命令序列。
- 报告不改变整组 A/B、无暂存复制、无回执、不自动续旧上传、无 GPFS 依赖等既定约束。研究票据完成只表示证据核对结束，不表示恢复协议实现或验收完成。
