# 许可续期的发布后时间证明与暂停边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 23, 29

## Question

能否用“先发布不可使用的待核验续期描述、再取得发布后的时间证据、最后条件确认”排除 PU05，而不假设线程不会暂停、不允许旧许可复活？明确续期逻辑生效点、证明可见点、读者保守拒绝及并发终止的关系。若该候选仍违反原 RQ，必须给出反例而非直接放行。

## Scope

研究线性化的原始定义及官方时钟/原子资料，严格区分文献事实与项目提出的协议。只分析本地续期内存协议，不声称跨节点设备租约；不选实现容器、时钟 API、不改代码、不运行模型或设备测试。独立 research 分支/工作树只提交一份报告。

## 验证判据

- 单纯 now 检查后 CAS 的反例仍为否决样本。
- 待核验状态不能提前提供更晚期限；发布后采样若已过旧 D 必须终止，不能把早先检查时间当证明。
- 明确确认若迟于 D 时是否构成复活：生效点与确认点不能含糊，已发生的终止不能被覆盖。
- 所有输入/读者对待核验描述的解释一致；K→G、首次启用是否可直接复用单独列出。
- 无模型运行不宣称验证通过；无法证明就保留缺口，不弱化既有 RQ/LG。

## Assets

- [研究报告](/tmp/tape-rs-renewal-linearization-vZMLn5/docs/research/renewal-linearization-evidence.md)。
- worktree：`/tmp/tape-rs-renewal-linearization-vZMLn5`；分支：`research/renewal-linearization-evidence`；提交：`aeca36a5650974e53360ea16b56765be2d52fce0`。

## Answer

限定研究完成；项目候选与文献事实分开，未证明完整算法：

1. 线性化要求并发历史对应对象规格允许的顺序历史；不能只用“在调用与返回之间选一点”就改写项目到期语义。依据：[Herlihy/Wing 原论文](https://cs.brown.edu/~mph/HerlihyW90/p463-herlihy.pdf)。论文不是本项目 Pending/Confirmed 算法的来源。
2. 在同一可信非递减时钟、实际发布 P 先于后取样 W 等前提下，W<D 可以证明 P 发生于旧 D 前；不能用发布前样本或模型私有真值代替。此为项目条件推论，不是已运行的故障证据。
3. P/W<D 但确认 C>D 的无读者分支是关键：若 P 仍为未接受的 Pending，不能因为无人写 Terminal 就跨截止确认；若想以 P 为逻辑接受点，须完整定义并证明其与原 RQ、所有读者和不可逆终止的关系，不能自动放行。已成功终止的许可不能被任何证书恢复。
4. Rust Instant 不统一承诺系统挂起计时或匀速；Linux MONOTONIC 与 BOOTTIME 对挂起计时有不同语义，不能据此推断 VM 暂停完全符合本项目模型。依据：[Instant](https://doc.rust-lang.org/std/time/struct.Instant.html)、[Linux clock_gettime](https://man7.org/linux/man-pages/man2/clock_gettime.2.html)。本轮未选择时钟或添加实时运行假设。
5. CAS/SeqCst 仍不能把取样与确认或外部派发合成原子操作；首次启用和 K→G 也不能直接套用稳态续期。下一步需收敛原契约下的时间/同步适配，不以增加一次 now 检查宣称已解决。

依自动推进授权，保留原 RQ 的严格边界，形成[续期时间证据分析](../renewal-time-evidence.md)；不采用当前未经证明的跨截止追溯确认。RT01—RT06 未执行，18 仍 claimed、09 阻塞，地图尚未全部收敛。报告仅提交文档，未实现、测试或操作实验室。
