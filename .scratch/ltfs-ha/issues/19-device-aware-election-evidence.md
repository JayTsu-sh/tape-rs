# 设备感知的 Raft 参选资格与安全边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 16, 17

## Question

三节点均连接带库、Leader 直接执行设备时，如何把节点设备能力作为参选因素，同时保留 Raft 的日志完整性、多数派及旧执行者隔离？核对本地抑制 campaign、普通投票/日志复制、PreVote/leadership transfer 与自定义候选健康过滤的区别，不把选举扩展当作标准协议已有保证。

明确设备访问条件无法等同于已完成磁带恢复；参选前探测不能改变正在由旧 Leader 使用的驱动器位置。关注只有落后日志的节点设备可用、就绪信息过期、候选设备局部故障、全部候选不具备条件以及额外健康规则导致无法选举的情况。只读一手资料，不选库或运行代码/设备命令。

## Assets

- [研究报告](../../../docs/research/device-aware-raft-election.md)。
- 独立 worktree：`/tmp/tape-rs-device-aware-election-l6m3S0`；分支：`research/device-aware-raft-election`；提交：`9c61d50493ef60b7b001b0e2a0c910cced22d38f`。

## Answer

限定一手资料核对完成：

1. Raft 保留成员多数派和候选日志新旧判断；比较最后日志 term，再比较 index。设备合格但日志落后的节点不能被强行选出，不能只在健康候选集合里重新计算多数派。[原论文 Figure 2、§5.4.1](https://raft.github.io/raft.pdf)
2. 官方实现具有定时、显式 Campaign、领导权转移等竞选入口，PreVote 不检查设备；仅禁止一个 API 或启用 PreVote 不能证明覆盖全部竞选路径。源码以本次读取内容为准，最终实现须绑定选定版本复核。[etcd-io/raft](https://github.com/etcd-io/raft/blob/main/raft.go)
3. 本地抑制竞选、正常投票/复制和当选后执行门控可以作为不同职责设计；健康条件加入远端拒票规则是额外扩展，不能与本地门控混称标准 Raft 能力。控制健康但设备访问故障，不自动取消其投票成员身份。
4. 项目推论：严格候选过滤可能使多数控制节点正常却无 Leader，尤其唯一设备合格节点日志落后时；无 Leader 不能保证标准复制自动把它追平。完整隔离/卷恢复不适合作为当前接管流程中的竞选前提，选举前只采用安全可观察的本机条件，后置执行核验仍需保留。

具体证据、项目推论与 DE01—DE07 候选用例见报告。研究 resolved 本身不代表用户批准策略或库集成；用户随后在 [18](18-raft-execution-contract.md#已裁定接管条件前置与执行就绪复核)接受前置参选条件与无合格候选时暂时无 Leader 的取舍，具体门槛及实现仍待完成。未修改代码、测试设备或运行模型。
