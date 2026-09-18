# 索引候选发现、增量链重放与安全尾部依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-recovery-chain-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 10, 11, 13, 14

## Question

为 D02 核对 SNIA LTFS 2.5.1 的索引候选发现、有效性与最新性、Full/增量依赖链重放及搜索停止条件。区分规范要求与资料性恢复算法；解释完整尾部、缺少标记、残缺索引、读错、EOD 无法验证时哪些只能恢复读取、哪些涉及介质修复，不能凭最大 generation 或 EOD 自动许可追加。

本项目不依赖 GPFS，B 在外部 fencing 后从磁带恢复已提交状态；不自动截断、补标记、覆写、丢弃未验证尾部或重放旧上传。研究需明确这些约束与参考算法中写入修复步骤的差异，以及仍需设备协议或现场证明的条件。只读官方资料，不修改源码或访问设备。

## Assets

- [研究报告](/tmp/tape-rs-recovery-chain-Y50XuQ/docs/research/ltfs-recovery-chain.md)。
- 独立 worktree：`/tmp/tape-rs-recovery-chain-Y50XuQ`；分支：`research/ltfs-recovery-chain`；提交：`e850a8e`。
- 主文档：[D02 协议细化](../recovery-evidence.md#d02-细化条件式快速定位与独立验证)。

## Answer

限定官方资料核对完成，得到三个具体约束：

1. §10.3 的快速定位要求全部分区 VCI 与当前有效介质 VCR 匹配；过期、缺失或无效不能被当作全卷最新性的证明。该优化不新增每批 ACK 必须更新 MAM 的要求。
2. `previousgenerationlocation` 与 `previousincrementallocation` 分别承担 Full 和增量回指，不能混成单一“上一代”；目标增量需从对应 Full 正序应用全部依赖。generation 可合法跳跃，不能仅凭数字差距判断缺链。
3. Annex D 是卷示例、Annex H 是资料性增量处理，不是覆盖任意异常尾部的完整恢复算法。IBM 修复工具可能修改介质或使历史数据不可恢复，不能成为本项目自动接管步骤。

来源及章节见研究报告：SNIA LTFS 2.5.1 §5.4、§9.2.4、§10.3、Annex H，以及 IBM SDE 官方恢复命令资料。候选发现/停止的充分性、完整语义校验器、目标设备尾部安全追加协议仍未完成；不能以研究 resolved 代表恢复协议获证。

研究结果已用于 D02 的条件式快速定位、指针/重放顺序、失败出口和 RE06—RE10 验证要求。04 继续 claimed；不依赖 GPFS、无回执、无暂存复制、不自动修复或恢复旧上传的约束不变。未改源码、执行修复或访问硬件。
