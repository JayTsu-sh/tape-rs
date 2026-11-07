# 原生 FDB 带库扫描

`library rescan` 是持久异步管理任务，刷新设备库存和巡检结果。它使用当前带库 owner 的受管设备线程，不调用 EE，不搬带、不装卸、不格式化，也不导入 LTFS 文件索引。`library list/show` 和 `tape list/show` 查询扫描缓存；文件索引导入由 `tape import` 负责。

```bash
ltfsctl -e NODE:7411 --library SERIAL library rescan --request rescan.json
ltfsctl -e NODE:7411 --library SERIAL task show --request rescan.json
ltfsctl -e NODE:7411 --library SERIAL task cancel --request rescan.json
```

必须显式指定带库，不接受全局 `--pool`。首次提交前以独占创建、0600、文件和父目录 fsync 保存原请求。重试执行同一命令并使用同一文件，不要删除文件、换 operation 或重新生成 epoch。文件只保存请求身份，任务状态以 FDB 为准；池管理共用这一套检查点机制。

返回 queued 或 HTTP 202 只代表接收任务。用 `task show` 查询至 succeeded、failed 或 cancelled。queued 可以取消；running 不接受取消，不能把正在执行的设备命令当作已停止。返回 failed 时保留原设备错误和巡检时间，旧完整库存与 last_success 保留。

专用 `POST /libraries/SERIAL/admin/library/rescan` 对同一请求的终态重放仍返回 HTTP 202，正文返回原任务和原回执，不重新扫描。判断结果须读取正文的 `status` 或查询原任务，不能把 202 等同于 queued，也不能要求终态重放必须返回 200。

HTTP：先 GET `/libraries/SERIAL/admin/context` 获取 namespace 和管理窗口 accepting，再 POST `/libraries/SERIAL/admin/library/rescan`。POST 必须有且只有一个 `x-tapers-catalog-namespace` 头，正文严格为：

```json
{"version":1,"library":"SERIAL","namespace":"NAMESPACE_UUID","epoch":1,"operation":"OPERATION_UUID","change":{"kind":"library-rescan"}}
```

UUID 使用规范非零值，epoch 使用原 accepting；未知或重复字段拒绝。查询 GET `/libraries/SERIAL/admin/tasks/EPOCH/OPERATION`，取消 POST 同路径加 `/cancel`。非 owner 管理入口最多转发一次；元数据只读模式拒绝提交。HTTP 接收线程不访问设备。

执行顺序：持久 running 回执确认 → 新的完整库存采样 → 原样本库存确认 → 原样本巡检确认 → 根据已发布事实写任务终态。成功结果包含 captured_at、node、round、库存计数和语义指纹。周期采集与显式扫描共用一个发布器：先结清旧 pending，终态回执未知时暂停新采样，保留原时间和正文。

执行需要同一库、路由、冻结点和 owner 轮次，实际 Serving 多数派许可及同源设备守护。FDB 超时、失权或回执未知不等于未提交；保留原意图，按原身份核对。元数据等待预算为5秒并每25毫秒复核资格，失败退避并加入抖动。持久损坏停止当前轮次，保留诊断，不自动修复。

新 owner 可以接管原 queued/running 的 library-rescan 并重新采样，因为此任务只读取设备状态；已终结任务返回原回执，不重新扫描。这个接管规则不适用于写带、搬带、格式化或其他介质变更。

EE 命令文档说明 rescan 刷新带库资源，show 读取缓存。本实现复用现有完整 READ ELEMENT STATUS 采集，不声称复制 EE 内部尚未观测的完整 SCSI 调用序列。

## 真实 B 库验收（2026-11-06）

实际部署源码 `4e2e63431fd2591c8d7362c3f262ded961159e16`，三节点使用原 B-only 配置、新三节点 FDB 和既有 namespace。非 owner 208 入口的 CLI 与 HTTP 均完成异步扫描，得到提交后的 8 盘磁带、2 个驱动器新观测；三节点查询和原请求重放返回相同终态。完整新增事件范围为六条任务状态事件，没有新增库存或巡检事件。负 namespace、未知字段及错误任务种类在 208 入口被拒绝。

真实任务已 succeeded 后，受限代理才破坏202正文；CLI 报告结果未定，原文件字节不变，随后查询和原请求重放恢复同一终态。真实 queued 取消及重复取消在三节点返回相同 cancelled，原检查点保留。计划停止 owner 1 / epoch 207055 后，16个原请求全部成功，事先观察为 queued 的15个由 owner 2 / epoch 208225 完成；原服务正常恢复。这是计划交接，不是断电或进程崩溃测试。

在 CLI 请求文件已持久保存、POST 尚未送达后端的边界，实际停止新 FDB 208、209 的原服务，确认读取1031、真实管理 POST 返回503及 CLI 结果未定。故障期间三台设备的 PR generation、注册键和 holder 保持不变。恢复原 FDB 服务与数据后，原请求成功完成，此前已提交任务、56/57事件原字节及首个未确认通知批次保留。旧 FDB 的故障期间和恢复后健康检查均通过，属于离散核对。

专属任务订阅的21个原任务与62条状态事件逐身份、逐状态完全匹配，无重复或整组遗漏；原库存订阅仅收到空过滤批次。先保存完整批次再显式 ACK，两个自有消费者在三节点均确认至119、pending为空，事件和消费者仍保留。最终严格 SDK 与独立控制面/FDB/三设备 PR 审计一致：owner 2 / epoch 209476、key `0x4c02000000033244`，三节点就绪；三节点 HTTP 显式确认 pool lto8 revision 6、零磁带归属。

软件检查含393项全特性单元测试、完整默认测试、全目标全特性检查、严格Clippy与格式检查。隔离 FDB / 模拟设备另测扫描失败保留旧库存、running回执未知前不发CDB、原样本重试不再采样、终态回执未知阻止周期覆盖、传输失权与原running任务接管。物理测试没有构造介质错误、断电或故障恰逢设备命令执行的精确瞬间；这些结果不能互相替代。日志写盘失败不阻断恢复的边界通过实际 finally 及远程恢复代码的模拟 OS 测试验证。

本片未执行 MOVE、装卸、格式化或介质文件读写。A 库及 B 的备用6070预留域继续隔离。LTFS 索引导入、单盘健康采集与 S3/FUSE 写入仍需独立接线及实物验收。原始证据位于维护主机 `/work/tmp/tape-native-library-rescan-physical-20261106/`。
