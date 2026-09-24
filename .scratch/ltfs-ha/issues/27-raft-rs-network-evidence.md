# raft-rs 网络输入与旧竞选输出的分类依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 25, 26

## Question

固定 raft-rs v0.7.0，哪些 MessageType 是本地事件、哪些是网络协议消息？RawNode::step 实际拒绝哪些输入、还缺哪些网络校验？设备不合格或竞选中断后，如何区分本节点旧 Vote/PreVote 请求与给其他节点的投票、复制及 safe 确认响应，避免泛化丢弃或改写 term？

## Scope

只查固定官方源码的消息分类、RawNode::step、send、Ready 输出与必要 transport 反馈。核对 MsgTimeoutNow、MsgTransferLeader、MsgReadIndex/Resp、MsgUnreachable/SnapStatus 等易混项；不选 TLS/RPC 产品、不修改 wire format、不实现网络或运行测试，不访问实验室。

本轮报告必须区分库事实与项目候选：网络身份/集群绑定不由 protobuf from 自动证明；业务竞选失效代次不能写进 Raft term 或篡改 ReadIndex ctx；未知发送结果不等于未交付。保留健康禁选节点正常投票/复制，不保证外部 exactly-once。

## 验证判据

- 未认证身份、错集群/目标、本地事件从网络注入，不能直接进入内核；实际允许集及字段校验依据固定版，不只靠“枚举可解析”。
- 旧竞选请求可按其生成关联抑制，但 term/vote 必需持久化不能丢；给其他候选的回复及复制响应不能按本机失效代次一概删除。
- 已排队/已交付、transport 重试与内核重发分别观察，未知交付不制造新的本地动作或修改原消息内容。
- 网络输入不能仅因不同于本地 G/接管轮次被丢弃，Raft term/PreVote 例外仍交原协议处理。
- 队列及消息大小预算须表达，但数值不猜；拥塞不能阻塞本地停发，也不能通过回报虚假成功“消除”积压。

## Assets

- [研究报告](../../../docs/research/raft-rs-network-evidence.md)。
- worktree：`/tmp/tape-rs-network-evidence-8tgeS1`；分支：`research/raft-rs-network-evidence`；提交：`9b368e7d8aa10c5c31bb4f36b8d7724a85fd1288`。

## Answer

限定研究完成，固定 v0.7.0 / `10c6e9db6792b85c81784e44fc278f895d5f0ab0`：

1. 全部 19 种消息可按本项目候选划分为 5 个本地事件、4 个首版不启用的网络转发类型、10 个直接共识类型。RawNode::step 只拒绝特定本地类型和未知 peer 的特定响应；ReadIndexResp 不在该响应集合，不能据此视作完整身份/目标认证。依据：[消息定义](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/proto/proto/eraftpb.proto#L49)、[RawNode::step](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L402)。
2. send 仅在 from=0 时补本机 ID；Follower 转发提案/读确认/转移请求可能保留非零 from，transfer_leader(target) 本身也以目标作为 from。故连接对端==from 不是原库所有消息的通用规则。依据：[send](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L611)。
3. 首版将控制提案、RQ 和计划转移限定为当前 Leader 本机 API、不启用四类网络转发，与已定 RQ/PS 未见直接冲突；保留合法 TimeoutNow 和 safe Heartbeat/HeartbeatResponse。对剩余十个无代理直连类型可建立实际 peer/from 绑定，但仍须认证、集群/目标及字段检查。此项是项目推论，不是库自动保证；路由与调用之间角色变化必须另核验。
4. 不以 term 与当前值不等就通用丢弃消息；PreVote、高/低 term 及正常转发的零 term 语义有差别。本节点旧 Vote/PreVote 请求与为别人生成的响应、复制和 safe 确认分开处理，不按失效 G/B 整批清空 Ready。
5. report_unreachable/report_snapshot 是本地传输反馈入口，不能由远端同名消息伪造。反馈关联原任务和实例，不等于控制提交、快照实际安装或设备隔离；Ready/LightReady 输出的持久化约束仍有效。依据：[本地方法](https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L726)。

研究仅补消息分类事实及候选边界；[首版网络草案](../raft-network-boundary.md)依自动推进授权采用更窄入口。认证产品、完整字段校验、队列预算和发送/反馈并发仍未实现或验证。resolved 不代表 18 完成，不代表网络/设备试验通过；独立分支只提交报告，未连接实验室。
