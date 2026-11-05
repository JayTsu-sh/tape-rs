# 原生 FDB 带库扫描

`library rescan` 是持久异步管理任务，刷新设备库存和巡检结果。它使用当前带库 owner 的受管设备线程，不调用 EE，不搬带、不装卸、不格式化，也不导入 LTFS 文件索引。`library list/show` 和 `tape list/show` 查询扫描缓存；文件索引导入由 `tape import` 负责。

```bash
ltfsctl -e NODE:7411 --library SERIAL library rescan --request rescan.json
ltfsctl -e NODE:7411 --library SERIAL task show --request rescan.json
ltfsctl -e NODE:7411 --library SERIAL task cancel --request rescan.json
```

必须显式指定带库，不接受全局 `--pool`。首次提交前以独占创建、0600、文件和父目录 fsync 保存原请求。重试执行同一命令并使用同一文件，不要删除文件、换 operation 或重新生成 epoch。文件只保存请求身份，任务状态以 FDB 为准；池管理共用这一套检查点机制。

返回 queued 或 HTTP 202 只代表接收任务。用 `task show` 查询至 succeeded、failed 或 cancelled。queued 可以取消；running 不接受取消，不能把正在执行的设备命令当作已停止。返回 failed 时保留原设备错误和巡检时间，旧完整库存与 last_success 保留。

HTTP：先 GET `/libraries/SERIAL/admin/context` 获取 namespace 和管理窗口 accepting，再 POST `/libraries/SERIAL/admin/library/rescan`。POST 必须有且只有一个 `x-tapers-catalog-namespace` 头，正文严格为：

```json
{"version":1,"library":"SERIAL","namespace":"NAMESPACE_UUID","epoch":1,"operation":"OPERATION_UUID","change":{"kind":"library-rescan"}}
```

UUID 使用规范非零值，epoch 使用原 accepting；未知或重复字段拒绝。查询 GET `/libraries/SERIAL/admin/tasks/EPOCH/OPERATION`，取消 POST 同路径加 `/cancel`。非 owner 管理入口最多转发一次；元数据只读模式拒绝提交。HTTP 接收线程不访问设备。

执行顺序：持久 running 回执确认 → 新的完整库存采样 → 原样本库存确认 → 原样本巡检确认 → 根据已发布事实写任务终态。成功结果包含 captured_at、node、round、库存计数和语义指纹。周期采集与显式扫描共用一个发布器：先结清旧 pending，终态回执未知时暂停新采样，保留原时间和正文。

执行需要同一库、路由、冻结点和 owner 轮次，实际 Serving 多数派许可及同源设备守护。FDB 超时、失权或回执未知不等于未提交；保留原意图，按原身份核对。元数据等待预算为5秒并每25毫秒复核资格，失败退避并加入抖动。持久损坏停止当前轮次，保留诊断，不自动修复。

新 owner 可以接管原 queued/running 的 library-rescan 并重新采样，因为此任务只读取设备状态；已终结任务返回原回执，不重新扫描。这个接管规则不适用于写带、搬带、格式化或其他介质变更。

EE 命令文档说明 rescan 刷新带库资源，show 读取缓存。本实现复用现有完整 READ ELEMENT STATUS 采集，不声称复制 EE 内部尚未观测的完整 SCSI 调用序列。

软件与真实 B 库验收分开记录；本片的真实 B 验收仍待执行，不能以模拟设备测试代替。
