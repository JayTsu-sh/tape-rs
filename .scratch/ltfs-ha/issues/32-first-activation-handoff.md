# 首次许可启用与 K→G 交接的时间及权限边界

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: raft_ha_research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 31

## Question

在 31 的条件接受对象基础上，分别审查首次 K/G 启用和 K→G：没有旧可用期限时如何避免提前放行，交接如何同时封闭 K 与建立 G，暂停跨原 K 截止或新证据期限如何处理？不得直接把稳态续期正例当作交接证明。

## Scope

以 18 原 HK/RQ、31 研究及原始线性化/官方时钟资料为依据。区分逻辑接受、证明可见、当前执行权和外部旧管理动作收束；给出所有读者、关闭竞争及正常进展的条件历史。限定报告，不选生产容器，不实现模型或访问设备；独立 research 分支只提交报告。

## Answer

限定研究完成，见[报告](/tmp/tape-rs-first-handoff-zwBaje/docs/research/first-activation-handoff.md)。独立分支 `research/first-activation-handoff`，提交 `66939dccfd78eb3bbcd254055936ea782fcfd4d8`，仅提交报告。

首次 Pending 没有旧执行权；交接 Pending 两侧副作用均拒绝。成功分支以证书证明及时的 P 接受，C 只公开证据；G 截止仅来自自身 Eg，不能取 max(Dk,Eg)。交接失败必须保留 K 的封闭/退出效果，不能照搬稳态失败无续期效果的映射；真正终止不复活。

P 前外部安全前提须持续有效，不能只凭队列空快照。准备期封闭新副作用与 K 终止不同，已终止 K 不可交接。依本轮自动接受授权采用[主文候选](../activation-handoff.md)，并明确接受首版固定准备期 Dk、不再刷新 K 的额外等待预算取舍；这不是 HK 强制要求。

研究 FA01—FA07 与主文 AH01—AH06 为不同分组，均未实现/执行；形式化历史、实际同步/生命周期/时钟与外部封闭证据未完成。18 保持 claimed，09 不解除依赖，未操作设备或运行模型。
