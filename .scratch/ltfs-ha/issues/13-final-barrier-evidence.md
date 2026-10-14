# 索引结束标记与最终介质屏障

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-barrier-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 10, 11

## Question

限定核对：数据分区索引结束的 WRITE FILEMARKS（IMMED=0，count=1）能否同时承担此前数据和索引的最终介质持久化屏障？什么条件下还需额外 count=0 同步？区分格式要求、设备完成语义、完整状态/延迟错误检查与现场未验证前提，不能把较少命令当成已验证优化。

使用 SNIA LTFS 与 IBM LTO 官方资料；复用既有研究线索但核对原文。只研究提交状态机 S3→S4，不重新讨论回执、合批或 HA 范围，不改源码、不访问设备。结果在独立 research 分支/worktree 留存，主地图只链接研究结果。

## Assets

- 研究分支：`research/ltfs-final-barrier`；提交：`4544f78`。
- 独立 worktree：`/tmp/tape-rs-final-barrier-research-rIlumF`。
- [研究报告](../../../docs/research/ltfs-final-barrier.md)。

## Answer

官方资料核对已完成，结论是有条件候选，不是减少命令已经获证：IBM §5.2.46 对 IMMED=0、count=0 明示全部缓冲数据与文件标记的介质同步；对非立即命令一般描述为完成后返回，但该段未展开 count=1 对此前全部缓冲的覆盖。不能由此直接证明可以省略 count=0，也不能反推 count=1 不同步。[IBM LTO SCSI Reference，印刷页 181 / PDF 页 210](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=210)

建议在证据未补齐时保留显式最终同步候选；索引结束标记兼任屏障须有适用协议/设备证据及验证。同步不能补齐残缺索引、修复短写或证明旧主历史请求；延迟错误、nexus、所有权、位置和完整结果仍须核验。LTFS 格式构造、介质同步与文件提交覆盖是不同条件，不因逻辑状态拆分就断言必需多发一条命令。

报告给出候选路径和缺失证据，已同步至[状态机 S3→S4](../commit-state-machine.md#待验证s3s4-最终屏障候选)。本研究票据 resolved 只表示限定文档核对完成；SSC 正文对应条款、现场固件/模式/HBA 行为、掉电与性能验证仍未完成，04 不关闭。未审计源码、访问设备或实施优化。
