# 原生 FDB 异步搬带

`tape move` 是运维机械操作：将指定条码移至原存储槽 `homeslot` 或空 I/E 槽 `ieslot`。它与 `library rescan`、池管理共用 FDB 任务及 CLI 请求检查点。显式搬带允许未分配磁带；文件读取仍须有当前归属和已验证卷身份。操作不导入文件索引，不格式化、不擦除、不发 LTFS 数据写入或 MAM 写入命令。

```bash
ltfsctl -e NODE:7411 --library SERIAL tape move BARCODE homeslot --request move-home.json
ltfsctl -e NODE:7411 --library SERIAL tape move BARCODE ieslot --request move-ie.json
ltfsctl -e NODE:7411 --library SERIAL task show --request move-home.json
ltfsctl -e NODE:7411 --library SERIAL task cancel --request move-home.json
```

可选全局 `--pool NAME_OR_UUID` 限定磁带当前归属。未分配磁带不能指定池。首次提交前查询当前归属，将池 UUID、卷 UUID 和修订号固定在原请求；尚未绑定卷身份的已分配带不接受准备。重试同一命令和文件，不重新解析归属，不覆盖文件、不换 operation。请求文件以独占创建、0600、文件和父目录 fsync 持久保存；任务状态保存在 FDB。`task show/cancel` 不接受全局 `--pool`。

HTTP：POST `/libraries/SERIAL/admin/tapes/BARCODE/move`，使用 `x-tapers-catalog-namespace`，正文如下。`namespace` 和 `epoch` 从 GET `/libraries/SERIAL/admin/context` 获取。外部调用者在首次 POST 前持久保存完整原正文。

```json
{
  "version": 1,
  "library": "SERIAL",
  "namespace": "NAMESPACE_UUID",
  "epoch": 1,
  "operation": "OPERATION_UUID",
  "change": {
    "kind": "media-move",
    "barcode": "BARCODE",
    "destination": "homeslot",
    "pool": null,
    "volume": null,
    "revision": null
  }
}
```

未分配时 `pool/volume/revision` 三字段必须显式为 null；已分配时分别填写当前池 UUID、卷 UUID、正整数修订号。重复、未知、缺失字段及路径条码不符会被拒绝。HTTP 接收线程只提交任务，不访问设备；非 owner 入口按既有规则最多转发一次。HTTP 202 或 queued 只表示已接收；running 不接受取消，不能将正在执行或结果未知的机械操作当作已停止。通过 GET `/libraries/SERIAL/admin/tasks/EPOCH/OPERATION` 查询，POST 同路径加 `/cancel` 取消 queued 任务。

设备线程先读取完整库存和地址 map，唯一匹配条码、配置驱动序列号及目标槽，拒绝清洗带、异常或禁用元素、不可访问目标和已占用目标。驱动已穿带时 Access=0 可以准备卸带，但不能直接发 MOVE。`homeslot` 只使用当前存储位置、驱动 SVALID 的存储地址，或关联原任务的持久 home 证明。I/E 中没有可信 home 时拒绝归槽，不任意挑槽。已在目标位置可确认成功而不发机械命令。

执行前原子保存原计划与带库互斥检查点。任务 `execution` 包含原 map、采样时间、源/目的地址、驱动身份、home、阶段及诊断。阶段是 `prepared -> unloading -> moving -> confirmed`；存储或 I/E 源无需卸带，走 `prepared -> moving -> confirmed`。每个阶段回执确认后才发相应命令，UNLOAD 与 MOVE 分两次轮询并重新核对许可、PR 和真实库存，保留原硬件长超时。

超时或进程退出不证明命令未执行。恢复保留原条码、目标和计划，先观察完整库存：原条码已在目标且原源为空则确认，不再 MOVE；仍在原源且原目标空则由当前已隔离 owner 继续原阶段；其他位置、冲突、不可观测或地址 map 改变则保留 running 和诊断，禁止重选目标、隐式回滚或清除互斥检查点。UNLOAD 回执丢失后，如新观测 Access=1，则继续核对 MOVE，避免盲目重发 UNLOAD。Access=0 不证明旧 UNLOAD 已结束；本执行器已尝试过命令，或接管时只拿到原 `unloading` 检查点，则保留 running、互斥检查点和诊断，继续观察，禁止重发 UNLOAD。换 owner 轮次本身也不等于旧命令已中止。同一执行器仍能证明尚未调用过 UNLOAD 时，可以在原阶段回执确认后首次发送。若进程在保存卸带意图后、发送命令前退出，接管节点无法区别“未发送”与“仍在执行”；任务可能持续等待，需要先核实设备与原命令的完成或中止状态，不能靠重提请求或清除检查点解除。准备阶段确认尚未开始的错误才可 failed，并标记 `effects=not_started`。

机械检查点未结清时，新设备读和自动卸带返回忙，`/readyz` 反映设备暂不可读；元数据查询和任务检查仍可用。新 owner 优先恢复原机械任务，普通启动恢复不得先 LOAD 介质。周期库存和显式扫描共用同一设备线程发布器；目标结果的原库存及巡检样本确认后才写成功终态、清除本任务的互斥检查点，终态回执未知时不覆盖原样本。

FDB 状态变更及 `task.status.changed` 通知在同一事务提交，机械阶段和诊断进入任务通知投影。沿用通知消费者的持久游标和显式 ACK，不以连接断开删除事件。

永久损坏会停止当前轮次的管理执行器；即使原事实随后被修复，也不会在该轮次自动继续机械操作。日志保留原 operation、条码、目标、已确认阶段和原始错误；诊断回执失败同样不视为未执行。

READ ELEMENT STATUS 保持 CurData=0；IBM 文档说明此时带库忙会返回 check condition，正常结果必须是有效库存，不改用可能返回不完整状态的 CurData=1。

状态位依据 [IBM TS4300 SCSI Reference SC27-4641-00](https://www.ibm.com/support/pages/system/files/inline-files/%24FILE/SC27-4641-00_1.pdf) printed50–58。命令形式参考 EE 运维语义；不声称已观测到 EE 内部的相同 SCSI 时序。

## 真实 B 库验收（2026-11-06）

实际部署源码 `b3432f5d993626b374b03f4612480709eeef59b2`，三节点使用既有 B-only 配置和三节点 FDB，保留原 namespace。代码及每项有状态现场脚本经两路 review，完整软件检查、显式私有 FDB 回归均通过；默认测试不接触物理设备。

| 验证 | 现场结果 |
| --- | --- |
| CLI 驱动归槽 | `8A0042L8` 从 TD8 地址1卸带并归到1007，约40.183秒；新观测曾为 Access=0，保留检查点等待后才 MOVE。单位日志为一次 UNLOAD 方法调用及一次 MOVE 方法调用。 |
| CLI / HTTP 往返 | `8A0043L8` 从1003移到空 I/E 102，再由原 HTTP 意图归1003，约17.472 / 16.484秒；三节点查询及原请求重放返回相同终态。 |
| 幂等和准备拒绝 | 已在 home 的任务成功且源等于目标；不存在条码的 CLI 在保存请求前拒绝，明确持久化的 HTTP 意图与清洗带 CLI 任务均 failed，`effects=not_started`。 |
| HA 与取消 | 观察到原任务 moving 后计划停止 owner；另一节点以原任务和原计划完成到102，排队 rescan 随后完成。running 取消拒绝，queued 取消真实成功，三节点查询、原 move 重放及重复取消一致。原服务已恢复。 |
| FDB 故障 | CLI 原检查点已持久、POST 尚未转发时暂停两个原45152进程，实际读超时1031、19个HTTP503，CLI报告结果未定且原正文不变。原服务和数据恢复后，同检查点归槽1003成功。不可用区间 PR 的代次、注册与持有者不变，单位日志无装卸/搬带方法进入。 |
| 通知与历史 | 两个原待确认批次跨 HA/FDB 故障全文相同；保存全部原始事件及每个完整批次后，两个自有消费者显式 ACK122→161，三节点 caught_up。新增10个原任务、34条任务事件及5条库存事件；原22个终态任务和56/57事件字节保留。 |

最终三节点 ready，SDK、FDB、控制状态及三个设备 PR 一致。TD8 留空，`8A0042L8` 在1007，`8A0043L8` 在1003；其余六盘的库存投影与准备前一致。池 `lto8` 保持修订号6、200000文件上限、零磁带归属。旧45151服务及其数据保留，A 库和 B 备用端口不执行设备命令。

初次滚动在207已换新二进制并启动后，现场脚本的立即进程哈希检查过早失败；随后只读确认207已运行正确 daemon，保留原失败和脚本，使用另一个经审查的接续脚本仅更新208/209。启动检查改为有界等待目标 daemon 哈希及就绪接口，无强杀、无额外进程、无回滚磁带任务。

本轮实物范围是未分配磁带的运维机械操作。已分配卷的真实数据读取互锁、原生 assign/import 及 S3/FUSE 写入互通仍须后续验收。命令完成后丢回执、仍在执行的 UNLOAD、阶段回执未知和永久损坏停止本轮等精确边界采用 Sim/private FDB 验证；不声称本轮做了物理掉电、进程崩溃或硬件精确丢弃 UNLOAD 回执。日志统计是方法调用，不能作为总线 CDB 数量。
