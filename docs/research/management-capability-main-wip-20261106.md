# main 与历史 WIP：分配、导入及数据链路

比较对象固定为 GitHub main `e37ba63361d89e4800c85ae04c7c00cbc4ff8425` 与已修格式化安全问题的 WIP `82ec54c664b2c92689e04dab35af10d139c97809`，不是尚未完成的集成工作区。

用户要求已明确：保留完整历史 WIP，解决与 main 的冲突；已分配磁带再次 import、刷新 FDB 索引是必须支持的项目扩展。完整集成候选须在当前可用的真实 B 库验证，通过后再合入。

| 能力 | main | WIP |
|---|---|---|
| EE 式空带分配 | CLI/HTTP、异步任务、真实 B 验证已有；完成时建立归属与 W 不可用标记，返回 `index_imported=false` | 原生生命周期执行器分配后继续捕获空卷，建立 ready/scan/source 基线 |
| 空带分配后进入 FDB 写入链 | 缺少后续基线与完整写入链 | 有完整链路，仍须对最终集成候选重新验收 |
| 未分配带的文件索引导入 | `ManagementRequest` 有 media-import 协议解析；CLI 与设备 actor 未闭合，不能称已实现功能 | CLI→HTTP→FDB 队列→设备 actor→流式捕获与元数据发布已实现 |
| 已分配卷刷新 | 缺少原生 import 执行链 | 同库、同池、同 UUID，保留原路径投影；拒绝更旧来源及同代不等价来源 |
| FDB S3、FUSE | 当前 FDB 模式拒绝 S3 启动，FUSE 无 FDB 后端 | 已接入 FDB 读写、持久请求、目录/节点变更与重命名等执行链 |
| 后续安全修复 | 包含库级机械互斥、严格任务/通知证明、缓存验证、原始 sense、接管前检及原任务恢复等修复 | 必须在集成时保留 main 新修复；不能整份使用旧 WIP 文件覆盖 |

## 关键代码证据

main `src/managed_catalog/management_request.rs` 接受 media-import，只证明协议可解析。`src/daemon/executor_fdb_rescan.rs` 的任务选择只执行 media-move、media-assign、library-rescan。`src/managed_catalog/management_assign.rs::complete_media_assign` 明确不伪造 imported heads，写入 W 并返回 `index_imported=false`。

WIP `src/bin/ltfsctl/tape_import.rs` 为实际异步 CLI；`src/daemon/executor.rs` 派发到 `executor_fdb_intake`，`src/managed_catalog/native_media.rs` 发布来源/扫描/就绪卷。`native_media_refresh.rs::prepare` 验证原归属、原池、UUID、可用状态和旧 baseline；`publish` 负责完成切换。WIP 自身的 assign 会继续捕获并发布空卷 baseline，因此不能丢弃该行为再称功能等价。

## 集成必须补齐的边界

1. **分配后的首份基线**：main assign 产生“已分配但未导入”的卷，WIP refresh 假定已有 ready/index。直接拼接会在首次 import 前失败。须显式支持该初始状态，保原归属修订、UUID、永久格式占用与 W；完整发布后再建立基线并解除 W。空带 assign→写入的整体行为也必须保留，不能让用户为内部缺口多执行一步。
2. **再次导入是项目扩展**：EE 安装版帮助限定 import 对象未分配；本项目明确允许已分配同池卷刷新。失败/取消保留原可用索引及原归属；发布不确定时沿原任务恢复。
3. **执行器与机械互斥唯一**：不能让两个 actor 执行同一个原任务；新导入/维护任务须接入 main 库级屏障，普通读写、巡检及接管不能绕过。
4. **持久接口统一**：任务原身份、serial/namespace、请求文件、通知证明与游标共用同一契约。首版不保留多套旧接口作兼容。

## 验证证据的适用范围

WIP 的历史能力不是仅有文档：本机既有 `loaded-media-refresh-physical-20261104.md` 记录已分配 AA17 经异步 import 刷新至 Gen39、归属保持，随后 S3 写至 Gen40，并通过实际内核 FUSE/S3 共 41 项检查；另有 S3/FUSE 物理报告。它们属于当时 A 库及旧冻结候选，不替代当前 82ec 或集成候选的 B 库验收。A 库目前借出，禁止访问。

82ec 的两项格式化安全修复有真实 B 独立证据；main 的 assign/move/rescan 等也有独立 B 证据。不同版本、不同测试的通过，不能合并推导为整个集成候选已经通过。

本报告形成时，集成工作树已解决文本冲突并通过 `cargo check --lib --all-features`，完整检查、语义接缝修复、最终双轴 review 及组合 B 验证仍未完成，尚未提交或合入 GitHub。

## EE 本轮验证范围

新证据目录：`/work/tmp/tape-ee-assign-import-physical-20261106`。初始核验：原三节点就绪、69 个终态任务、队列及 Kl 为空、AA0000 未分配/home1002、TD8 空、HH8 保持 AA13，原通知 ACK210 不变。安装版实际帮助重新获取，确认 import 的未分配限制与异步参数；历史完成任务查询必须使用 `task list -c`。

计划仅以 AA0000 验证 EE assign→实际 GPFS premigrate、已分配 import 拒绝、EE export 卷的异步回导和逐路径内容回读，并保留带内 fixture 给随后集成候选使用。该计划不能在未执行前写为通过，也不能扩称通用第三方 LTFS 或已证明导入从磁带而非缓存读取 index。

## 用户补充：必须与 GPFS 解耦

本项目无 GPFS/EE 运行依赖；仅参考 EE 行为和互通带内格式。真实 EE 实验中安装 GPFS 是 EE 自身的前提，不能转化为 tape-rs 的启动或导入条件。以 [独立运行时 ADR](../adr/0002-gpfs-independent-runtime.md) 为集成约束。后续项目 B 实测的运行环境不应暴露 GPFS 挂载、EE 服务或其命令工具。


## 当前集成候选进度（2026-11-06，尚未提交）

上述表格仍比较两个固定历史版本。当前集成工作区已采用 main Observer 作为唯一 assign/move/rescan 执行器，分配时捕获真实空卷来源并原子发布 baseline；保留原生 import、同池已分配刷新和完整 FDB 数据链。旧 WIP 分配/补池标记 actor 及专属辅助函数已删除。原任务标识统一四段 `namespace:library_serial:epoch:operation`，CLI、SDK、HTTP 使用规范带库作用域和 namespace 头。

最新单元测试659通过、0失败；5项隔离FDB目录/回滚/互斥回归全部通过。另两项使用真实隔离FDB和内存Sim设备：完整生命周期及回收组合回归通过；最后一条MAM命令未知时不发布成功采样、不推进原任务的回归通过。它们不替代真实带库验收。剩余未守护的库存/机械调用、规范接口测试与文档对齐、最终整diff审查及真实B独立S3/FUSE内容验收仍未完成，尚未合入GitHub。

EE参考实验已完成assign1004、premigrate1006、export1012和异步import1017；对已分配卷的import1008明确拒绝。recall1019失败0/3（实际errno13），没有内容回读成功结论。AA0000保留真实FullGen5三文件样本供项目独立验证；EE实验已正常收尾并恢复原三节点服务，不能重放旧实验脚本。
