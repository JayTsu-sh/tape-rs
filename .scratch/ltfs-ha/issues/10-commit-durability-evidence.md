# 磁带提交屏障与中断恢复证据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-commit-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 01

## Question

从 LTFS、SSC 和目标驱动器的官方资料核实：使用 SG_IO 的变长 WRITE 与非立即 WRITE FILEMARKS，哪些返回条件能证明数据及索引真正写入介质？缓冲模式、IMMED、延迟错误、短写、EOM、电源中断和跨节点重新打开分别有哪些限制？数据分区已有完整索引而索引分区落后时，恢复需要什么证据，哪些条件不足以提前确认成功？

提供能用于提交状态机和故障测试的事实、引用、候选顺序与尚待硬件验证的条件。不要将规范允许可恢复状态等同于现有实现已经达到该状态，不替用户决定合批策略或保留期限。限定为研究，不访问真实设备，不修改源码。

## Assets

- 分支 `research/ltfs-commit-durability`，worktree `/tmp/tape-rs-commit-research-QIDBx0`。
- [研究报告](../../../docs/research/ltfs-commit-durability.md)：该分支的 `docs/research/ltfs-commit-durability.md`，提交 `0073163`。worktree 移除后可用 `git show research/ltfs-commit-durability:docs/research/ltfs-commit-durability.md` 读取。

## Answer

文档研究完成，原始来源、候选顺序和六类失败验证案例见报告；未访问设备或修改业务源码。

- IBM LTO SCSI Reference GA32-0928-08 §5.2.46 明确 IMMED=0 的 WRITE FILEMARKS 等待完成，count=0 可同步缓冲数据及文件标记。普通缓冲 WRITE 返回成功与 SG_IO resid 均不能单独证明介质持久化。
- 完整结果判断须覆盖 SCSI/host/driver 状态、sense、残差、early warning、实际 EOM 及延迟错误；跨节点 I_T nexus 变化可能改变错误报告归属。B 重新打开设备或执行同步成功，不足以证明 A 的历史写入成功。
- LTFS 允许 sync 后可恢复但暂未两分区一致。IP 落后时必须恢复有效 DP 索引和必要索引链，并验证卷身份及指针关系，不能只依据可解析 XML、较大 generation 或旧 IP 快照。
- 候选顺序为写数据及回执、完整 DP 索引与结束标记、明确的非立即介质同步及结果检查，之后才确认；具体布局、冗余屏障消除和中断状态机仍须设计与验证。
- SSC-5 正文受访问表单限制，本次未完成逐条核实。IBM 文档覆盖 LTO-8，但现场固件、缓冲模式、HBA 残差、加密、掉电恢复和 RTO 未实测，不能宣称目标设备验收通过。

本票据仅解决文档事实研究，不决定合批或保留期限，也不替代[提交契约](04-durability.md)与后续硬件验收。
