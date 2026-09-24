# Raft 控制面与磁带设备隔离的适用边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 01, 02, 03

## Question

用户希望把原主备 HA 更换为 Raft 模式。核对 Raft 的多数派与节点数、复制状态范围、Leader 与外部设备执行权的关系，以及能否保持磁带权威、暂存不复制、不自动重放和整组单执行者。区分普通投票成员与原 qdevice 见证；Raft 控制日志提交不等于磁带数据提交，也不自动替代外部 fencing 或服务管理。

## Scope

仅依据 Raft 原论文、官方实现文档或第一方资料进行限定研究，不选定 Rust 库、修改代码、部署集群或访问设备。报告遵循独立 research 分支/工作树惯例；结论供 [HA 方案变更](17-raft-ha-direction.md)裁定，不自动替代 03 的已定方案。

## Assets

- 后续裁定：[17](17-raft-ha-direction.md#answer)选择三个节点均接带库、Leader 直接承担设备执行。报告中的 C 仅控制及 Leader/owner 分离是当时的候选建议，未被采用；多数派、持久复制与隔离边界的事实核对仍适用。
- [研究报告](../../../docs/research/raft-ha-evidence.md)。
- 独立 worktree：`/tmp/tape-rs-raft-ha-research-NGSlAw`；分支：`research/raft-ha-evidence`；提交：`ca7a81a73be9ab297459e4154b0975f5096b92f8`。

## Answer

限定研究完成，事实与项目推论分开记录：

1. 普通 Raft 两投票成员需要两票，不能在一个成员故障后继续提交控制变更；三个投票成员多数派为两票，可容忍一个成员故障。learner 不投票，不能替代第三个投票副本。依据：[etcd FAQ](https://etcd.io/docs/v3.6/faq/#what-is-failure-tolerance)、[learner 设计](https://etcd.io/docs/v3.6/learning/design-learner/)。
2. Raft 管理复制状态机的日志与选举，持久状态须可靠保存；各节点不一定同时得知新 term。它不自动完成共享外部设备隔离，也不提供控制日志与磁带写入的原子事务。项目仍须独立核验 fencing、设备状态及磁带持久化，不能让 follower 或日志恢复重放原始 CDB。依据及推论边界见报告、[Raft 原论文](https://raft.github.io/raft.pdf)与[外部隔离说明](https://clusterlabs.org/projects/pacemaker/doc/3.0/Pacemaker_Explained/html/fencing.html)。
3. 可评审 A/B/C 三个完整控制副本、A/B 两个设备执行候选、整组一个有效 owner；C 可以任控制 Leader 而不操作磁带。控制状态复制不要求复制上传正文或目录，不能据此取消磁带权威、隔离或引入旧上传重放。
4. 更换协调算法还需明确服务监管、控制/数据路由、隔离适配、成员管理及故障状态转换；不能把 Pacemaker 名称换成 Raft 就视为完成 HA。控制共识也有网络和持久化成本，不保证更快接管或零吞吐影响。

研究 resolved 只代表限定事实核对完成，不是用户接受拓扑/实现、完整 HA 协议完成或部署验证通过。方向裁定交给 17；失多数派的设备任务边界、授权/隔离切换协议和现场能力仍待细化。未改业务源码或访问硬件。
