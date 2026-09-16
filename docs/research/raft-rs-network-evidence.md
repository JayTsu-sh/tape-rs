# raft-rs 固定版本网络入口证据

日期：2026-09-16。范围：票据 27；仅核对官方 raft-rs v0.7.0，commit `10c6e9db6792b85c81784e44fc278f895d5f0ab0`。复用该提交的本地源码并浏览固定官方链接；未实现网络、运行测试或访问设备。

## 结论

`RawNode::step` 不是认证边界，也不是完整网络白名单。首版可以将控制提案、RQ 与计划转移限定为当前 Leader 本机 API，禁用库支持的网络转发类型，从而缩小网络协议面；这是项目候选限制，不是库要求。不能把禁选、旧 G 失效或设备不合格等同于停止投票与复制。

## 1. 固定版全部 19 个类型

下表覆盖该版本枚举全部值；“首版”列是项目候选，不代表库的统一分类。[消息定义][proto]

| 类型 | 首版入口边界 |
| --- | --- |
| `MsgHup`、`MsgBeat`、`MsgUnreachable`、`MsgSnapStatus`、`MsgCheckQuorum` | 5 个本地类型，网络禁止注入 |
| `MsgPropose`、`MsgReadIndex`、`MsgReadIndexResp`、`MsgTransferLeader` | 4 个首版网络禁用类型；相关业务路由至 Leader 本机 API |
| `MsgRequestVote`、`MsgRequestPreVote`、`MsgRequestVoteResponse`、`MsgRequestPreVoteResponse` | 4 个直接选举协议消息 |
| `MsgAppend`、`MsgAppendResponse`、`MsgHeartbeat`、`MsgHeartbeatResponse`、`MsgSnapshot`、`MsgTimeoutNow` | 6 个直接复制、确认或转移触发消息 |

未知值、解析失败或异常枚举不能默认为 `MsgHup`。这是网络解码适配要求，尚未核对具体序列化后端的异常值处理。

## 2. 库实际检查什么

事实：`RawNode::step` 先拒绝上述 5 个本地类型；随后只对 `is_response_msg` 命中的类型检查 `prs().get(from)`。该集合为 AppendResponse、RequestVoteResponse、HeartbeatResponse、Unreachable、RequestPreVoteResponse；其中 Unreachable 已被前一关拒绝。ReadIndexResp 不在该集合，非响应类型也不因此检查发送者是否已注册。入口没有集群身份、真实传输对端或目标绑定检查。[RawNode 入口][raw-step]

事实：`send` 只在 `from == 0` 时补本机 ID；Follower 转发 Proposal、ReadIndex、TransferLeader 时保留非零 `from`。`transfer_leader(target)` 特意将 `from` 设为转移目标。因此不能对原库所有消息要求“连接对端等于 from”，也不能相信任意对端声明的 from。转发路径存在不代表任意多跳、任意 term 组合都有效。[发送规则][send]、[本地方法][raw-methods]、[Follower 分派][follower]

项目推论：在上表首版限定、无代理转发且网络输出只来自受控 RawNode 的前提下，10 个直接协议类型可以绑定“已认证直连 peer == from”，并校验目标为当前实例对应节点；仍需额外关联集群与有效成员身份。不能从消息中的 ID 自证认证，也不能仅凭 `prs` 存在判定对端可信。

## 3. term、context 与选举不能粗暴裁剪

事实：正常转发的 Proposal、ReadIndex 不附加 term；`step` 对 term 为零跳过任期比较。高 term PreVote 请求及获准的 PreVoteResponse 有不推进本地 term 的例外；较低 term Append/Heartbeat 或 PreVote 也可能产生协议回复。不得在适配层用通用“term 不等于当前 term 就丢弃”替代这些规则。[发送规则][send]、[任期分派][term]

事实：TransferLeader 是请求转移的入口；Leader 最终向目标发送 TimeoutNow，Follower 收到后可能直接发起跳过 PreVote 的竞选。两者不是同一个网络权限。Leader 本地 ReadIndex 的 safe 路径使用 Heartbeat/HeartbeatResponse 的 context 进行确认，结果可以直接产生本地 ReadState，不需要 ReadIndexResp 网络返回。[Leader 分派][leader]、[Follower 分派][follower]、[TimeoutNow 与读结果][read-result]

项目推论：Leader 本机提案、RQ 和计划转移与上述网络禁用组合未见直接矛盾；需在实际调用前重查角色，不能仅依赖路由时的 Leader 判断。若角色已变导致本机 API 生成被禁用的转发输出，应按路由失效/未获确认处理，而非开启例外转发或报告提交成功。

项目推论：旧的本机 RequestVote/PreVote 输出可按竞选代际识别并抑制；它们不同于为别人生成的投票响应、复制响应或 safe 确认。不能按旧设备授权轮次清空所有 Ready 网络输出，也不能用丢获票消息代替已规定的失格 Candidate 退出与高 term 处理。Raft context 不自动包含项目执行轮次，RQ 关联仍由项目维护。

## 4. Ready 与本地完成通知

事实：Ready 区分普通消息与依赖 HardState、entries、snapshot 持久化之后才能发送的 persisted_messages；LightReady 也可能提供消息。`report_unreachable`、`report_snapshot` 通过内部 step 构造本地反馈，而不是接受远端同名消息。异步 entries 获取回调携带 Storage 提供的上下文，并检查相应 Leader/term/目标是否仍适用。[Ready 消息契约][raw-ready]、[本地方法][raw-methods]、[异步回调][raw-step]

项目推论：发送任务、快照任务与完成通知需关联原实例、目标和具体任务，不能由裸节点 ID 或任意远端报告构造可信本地完成。入队、实际发送、对端接收、Raft 复制确认和快照实际安装不是同一事件；不能为了消除积压伪造成功。报告发送不可达不证明对端已隔离，也不是撤销旧设备命令。

## 5. 留待集成验证

- 认证、集群/目标绑定、成员转换期间的准入，以及未知枚举处理尚未实现；本报告不是逐字段 wire 校验规格。
- 验证 19 型分类、4 型拒绝与合法 TimeoutNow；覆盖 Leader 路由后变 Follower、旧竞选输出与仍需发送的投票响应并存。
- 验证持久化依赖完成前不发送相关消息、重复/迟到发送反馈不归属新任务，RQ 旧 ctx 不更新新轮次时效。
- 队列容量、消息大小、退避与重试预算数值未定；网络背压不能阻塞本地失权停发。运输重试不能改写 Raft 消息内容或冒充 Raft 新一轮重发。

以上均为待验证断言；没有据此证明完整 EG、RQ 集成或外部 fencing。

[proto]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/proto/proto/eraftpb.proto#L49
[raw-step]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L402
[raw-methods]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L726
[raw-ready]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raw_node.rs#L176
[send]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L611
[term]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L1324
[leader]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2108
[follower]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2311
[read-result]: https://github.com/tikv/raft-rs/blob/10c6e9db6792b85c81784e44fc278f895d5f0ab0/src/raft.rs#L2832
