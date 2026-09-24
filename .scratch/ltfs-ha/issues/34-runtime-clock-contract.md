# Linux 运行许可时钟与暂停恢复的能力边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 31, 32, 33

## Question

为许可原 t0、P 后采样和当前派发检查选择 Linux 时钟基线：CLOCK_BOOTTIME、MONOTONIC 与 Rust Instant 在线程暂停、系统挂起及虚拟机暂停/迁移中的实际保证是什么？哪些条件能作为部署前提，哪些必须保持未知且不能靠运行时比较两个时钟证明？

## Scope

只用 Linux、Rust、KVM/QEMU 等原始官方资料，给出可实施计时/错误出口及正反例。不得推断现有 PVE/EE3 已满足能力，不访问远程环境，不配置/运行模型。独立 research 分支只提交报告，周期和性能数值留空。

## Answer

限定研究完成，见[报告](../../../docs/research/runtime-clock-contract.md)。独立分支 `research/runtime-clock-contract`，提交 `6645c8ce41144e2165db13c582c1a08af70b81e1`，仅报告提交。

依自动接受授权采用[BOOTTIME 计时契约](../runtime-clock.md)：原 t0/W/当前检查用同一受支持时间域、检查算术与错误；定时器只提示，不能延长原期限。计入 Linux 系统挂起不等于证明所有 VM 生命周期行为，单调也不保证真实秒数速率误差有界。

明确首版不承诺活动许可跨未资格化 VM 暂停、迁移、快照恢复继续有效；受控停权/旧效果约束之后操作，恢复后建立新上下文/许可。该支持边界不是禁止用户运维，不授权本轮执行维护；guest 双钟或内存标签不能全覆盖检测共同冻结/回滚。

TC01—TC06 未实现/执行，时钟适配与内存序组合、真实环境资格化及外部派发仍待验证。18 保持 claimed，09 依赖不解除，未访问 PVE/EE3 或设备。
