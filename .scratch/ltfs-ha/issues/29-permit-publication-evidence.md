# Rust 运行许可发布与撤销的同步依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 25, 28

## Question

在单启用/刷新者、多失效来源、多派发读者的场景下，如何避免旧健康/刷新覆盖撤销、读取 torn 状态和指针复用？核对 Rust 原子、对象生命周期与进展保证，给出可验证的最小候选，而非把一个布尔值或 SeqCst 当成整个协议。

## Scope

仅官方 Rust 文档及必要的一手实现文档，报告区分内存安全、逻辑同步与物理交付。对比独立原子字段、短锁、整体条件替换等候选；不写代码、不选依赖、不弱化快速停发/EG/LG，不运行模型或设备测试。报告在独立 research 分支/工作树，只提交一份 Markdown。

## 验证判据

- 当前失效覆盖准备中或尚未发布的许可；后来健康不能复活旧许可。
- K 阶段结束不直接撤销已独立生效的 G；局部故障不一概扩大范围。
- 不在慢调用上持锁；无锁不等于无等待上界，CAS 重试及对象回收成本单列。
- 内存次序不能关闭最后检查至系统/外部交付的暂停窗口；不靠提前标记已发出来放宽 LG。

## Assets

- [研究报告](../../../docs/research/permit-publication-evidence.md)。
- worktree：`/tmp/tape-rs-permit-publication-m7bhRM`；分支：`research/permit-publication-evidence`；提交：`709feceb88ff68102d71962a12197bd247a0a419`。

## Answer

限定研究完成，依据 Rust 官方文档及固定 Rust 1.85.0 原子源码说明；引用版本不作为项目工具链选择。

1. CAS 只对匹配前态替换，多字段独立原子不提供完整状态快照。推荐按故障域使用整体版本化条件转换作为验证模型；无活动许可时也记录失效，阻止准备中的旧许可迟到发布。此为项目推论，不是现成库接口。依据：[Ordering](https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html)、[固定原子源码](https://github.com/rust-lang/rust/blob/1.85.0/library/core/src/sync/atomic.rs)。
2. 更新失败后不能只换 expected 复用旧健康/续期证据；撤销失败也不能漏掉仍适用的故障。K 的阶段终止和共同安全风险须分开，局部故障不泛化全组。这些是候选必须满足的协议约束。
3. Arc 不同步内部可变状态；裸指针读取后补引用需要对象持续存活的保证，不能忽略回收/ABA。标准原子 lock-free 不保证 wait-free，重试、分配、析构和调度须纳入快速停发证明。依据：[Arc](https://doc.rust-lang.org/std/sync/struct.Arc.html#thread-safety)、[原子进展保证](https://doc.rust-lang.org/std/sync/atomic/index.html)。
4. 项目反例：截止前检查 now，暂停跨过截止，到期回调未执行，原内存前态仍可匹配，CAS 可能发布更晚期限。仅“检查后 CAS”不能证明 RQ 过期不复活；事后撤销也未排除读者中间使用。未选择解决算法，不放宽原期限要求。
5. SeqCst 不覆盖最后内存检查至外部设备交付的暂停窗口；继续要求实际派发点及 fencing，不能提前将软件准备标记为已发出。

依自动推进授权形成[整体许可发布候选](../permit-publication.md)，接受额外版本、生命周期和竞争证明成本。时间生效切点和快速撤销仍是明确缺口；报告 PP 场景及规格 PU 场景均未执行。resolved 只指研究完成，不表示 18 许可协议完成；无实现、测试、依赖添加或实验室操作。
