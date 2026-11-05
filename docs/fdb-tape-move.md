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

超时或进程退出不证明命令未执行。恢复保留原条码、目标和计划，先观察完整库存：原条码已在目标且原源为空则确认，不再 MOVE；仍在原源且原目标空则由当前已隔离 owner 继续原阶段；其他位置、冲突、不可观测或地址 map 改变则保留 running 和诊断，禁止重选目标、隐式回滚或清除互斥检查点。UNLOAD 回执丢失后，如新观测 Access=1，则继续核对 MOVE，避免盲目重发 UNLOAD。准备阶段确认尚未开始的错误才可 failed，并标记 `effects=not_started`。

机械检查点未结清时，新设备读和自动卸带返回忙，`/readyz` 反映设备暂不可读；元数据查询和任务检查仍可用。新 owner 优先恢复原机械任务，普通启动恢复不得先 LOAD 介质。周期库存和显式扫描共用同一设备线程发布器；目标结果的原库存及巡检样本确认后才写成功终态、清除本任务的互斥检查点，终态回执未知时不覆盖原样本。

FDB 状态变更及 `task.status.changed` 通知在同一事务提交，机械阶段和诊断进入任务通知投影。沿用通知消费者的持久游标和显式 ACK，不以连接断开删除事件。

状态位依据 [IBM TS4300 SCSI Reference SC27-4641-00](https://www.ibm.com/support/pages/system/files/inline-files/%24FILE/SC27-4641-00_1.pdf) printed50–58。命令形式参考 EE 运维语义；不声称已观测到 EE 内部的相同 SCSI 时序。

## 验证状态

当前实现正在软件故障测试阶段。真实 B 库搬带尚未执行；验证后补充实际部署版本、物理位置变化、HA/FDB 故障结果及未测边界。模拟设备通过不代表真实带库验收通过。
