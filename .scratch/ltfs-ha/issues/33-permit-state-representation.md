# 许可状态表示、对象生命期与独立撤销

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 29, 31, 32

## Question

给 31/32 的逻辑许可选择可审查的 Rust 表示，分开大描述的安全读取/回收与小门控的不可逆撤销。能否使用标准库受保护描述、单发布者及原子控制字，令撤销不分配、不取描述锁、不等待 I/O，同时不会被旧发布覆盖？必须覆盖描述与控制字不同步、暂停、毒化、旧计时事件、K→G 及健康 ABA。

## Scope

以 Rust 官方文档及必要固定版本源码核对真实能力，比较保守标准库候选与原子指针候选，不把无锁当实时保证。不实现模型、引擎或硬件操作。独立 research 分支只提交报告；具体吞吐与时间数值留空。

## Answer

限定研究完成，见[报告](../../../docs/research/permit-state-representation.md)。独立分支 `research/permit-state-representation`，提交 `09daaacd1dc978020a6e51e4e9433c590f912720`，仅报告提交。

依自动接受授权采用[标准库表示基线](../permit-representation.md)：稳定 Arc 传递对象，Mutex 保护不可变描述，整数原子控制字与精确前态竞争；读者 try_lock 忙则暂拒。发布 CAS 失败须在锁内恢复旧描述，不能回写旧 token 或清 STOP。整轮故障与旧 K 阶段/到期关闭分别处理，不能把后者退化为无条件整轮 STOP。

接受短锁竞争导致的暂拒以及引用计数/回收成本，须按 06 测量；不新增 ArcSwap 依赖，不声称 wait-free 或固定实时上界。报告 PR01—PR06 与主文 RP01—RP06 为不同分组，均未实现/执行。

报告明确跨轮故障绑定、多来源 scope 注册和实际同步组合仍需协议，不能凭一个 latch 宣布完成。后续[稳定故障域代次候选](../permit-fault-scopes.md)专门细化这项缺口；它是项目设计，不是标准库提供的功能。18 保持 claimed、09 阻塞，未运行模型或访问设备。
