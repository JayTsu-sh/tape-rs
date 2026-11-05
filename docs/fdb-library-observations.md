# FDB 带库库存与巡检事实

`ManagedCatalog::record_active_library_inventory` 和 `record_library_monitor` 将完整库存、巡检结果及对应通知在 FDB 同一事务中保存；`active_library_inventory` 和 `active_library_monitor` 只读这些记录。查询不打开设备、不发 SCSI，也不调用 EE。这里提供存储与事件源，周期设备采集、rescan actor、library/tape CLI/HTTP 展示及磁带健康/MAM 状态仍需后续接线，不代表后台监控已经启用或现场验收通过。

写入要求精确活动路由和当前 FDB owner epoch，库存 round 必须等于该 epoch。设备调用方还须在采集前后核对自己的 Raft/PR 执行许可；FDB 校验不代替设备隔离。旧 owner 在接管后不能覆盖事实或生成事件。读取返回原采集节点、轮次和时间，之后发生接管也不会将历史观察改成新 owner 的实时库存；派发机械操作必须另行核验实际设备。

## 库存格式与边界

库存输入精确包含以下字段，空值也须显式给出，拒绝未知或缺失字段：

```json
{
  "library_serial": "LIBRARY_SERIAL",
  "owner": 1,
  "round": 100,
  "observed_at": "2026-11-05T00:00:00Z",
  "volumes": [
    {
      "barcode": "A001L8", "address": 1001, "element_type": 2,
      "medium_type": 1, "source_address": null, "home_slot": 1
    }
  ],
  "drives": [
    {"serial": "DRIVE_SERIAL", "address": 256, "barcode": null}
  ]
}
```

地址为 u16，元素类型沿用项目的 transport/storage/I-E/drive 值 1–4；medium_type 是原始 u8 或 null，不在这里推断介质健康。source_address 为 u16 或 null，home_slot 为正 u32 或 null，只有 storage 元素可以有 home_slot。地址来自采集方的实际 element address map，存储层不计算或假定真实设备地址。

现有设备执行器的 `observe` 用同一次 `READ ELEMENT STATUS` 返回的元素生成 volumes 和 drives，避免两次读取之间位置变化时拼出混合快照。它仍是带时间的观测，返回后设备可能已变化，不代替执行机械操作前的身份与位置核验；原生owner的周期采集已接入，行为见下节；library/tape list/show管理查询入口仍待下一片，当前通过SDK和既有CLI/HTTP通知接口验证。

真实采集验证用例 `real_inventory_observation_uses_one_element_status_reply` 默认忽略，直接调用该生产采集方法并计数实际 CDB。现场协调器须先确认当前 owner、空闲设备队列、原 PR 与目标隔离，再通过 `TAPE_RS_LIBRARY_OBSERVATION_CONFIG` 提供 library、node、round、devices（每项含 path、serial、kind=changer|drive）的 JSON。用例在打开前逐项校验 sysfs 序列号，只允许 INQUIRY、PR IN、MODE SENSE、READ ELEMENT STATUS；验证一次库存响应、卷与驱动投影一致、前后 PR 不变。它不写 FDB，也不代替管理 API、后台巡检和通知链的实带验收。

条码和地址不能在 volumes 内重复，驱动序列号和地址不能在 drives 内重复；列出的驱动装载条码须与同地址的 drive 元素一致。完整 changer 库存可以包含未配置驱动中的介质，不要求所有 drive 元素都在受管 drives 集合中。库存内驱动序列号最多 255 字节，拒绝空白、控制符和 `/`；库序列号沿用当前物理库标识规则。

验证标量形状与边界后才复制输入，最多 4096 个 volumes 和 4096 个 drives，最终完整记录最多 90,000 字节；超限拒绝整次发布，不截断或保存半份库存。当前是有界单记录缓存，大型库存的分片发布尚未实现。数组按元素地址规范排序，读取校验版本、namespace、库、轮次、类型、范围及规范编码，损坏记录不会被查询或后续写入自动修复。

## 原生owner采集

原生FDB owner进入Ready、fenced且有Serving多数派许可后，在该库现有设备执行线程中首次采集，正常周期30秒。采集不另起设备线程，不打断正在读取的带；设备线程忙时轮询可延后。换带器序列号、节点/轮次、冻结点和每台设备的执行守护必须匹配。占用元素没有条码、重复身份或配置驱动映射不完整时整次失败，不发布空库存或部分库存。

成功后先确认完整inventory，再记录monitor成功。普通设备采样失败只更新monitor错误并保留原完整inventory和上次成功时间；丢失执行权时不发布。错误日志保留库、节点/轮次、原采样时间、阶段及诊断，持久错误文本限制2048字节且保持UTF-8。

FDB请求有独立5秒预算，每25毫秒检查Serving资格；这不缩短任何SCSI或机械超时。超时/回执未知保留原payload及observed_at，结清前不采新快照覆盖；元数据重试采用有上限的退避和抖动。设备失败也退避，正常成功后恢复30秒周期。持久记录损坏或无效事实会停止当前轮次发布，需修复事实源并显式重启或接管后重新评估；不自动补字段或修复通知证明。

旧轮次不能覆盖新轮次；owner变化后重新采集，但物理语义相同时仍不重复通知。这里的library monitor描述完整库存采样与发布结果，不表示单盘TapeAlert/MAM或介质内容验证成功。

## 变化通知

每个库、namespace 保存独立缓存与通知证明。库存的语义投影包含介质/驱动数量及规范化 volumes+drives 的 SHA-256；`library.inventory.changed` 使用这个紧凑投影。条码、位置、装载状态及其他物理字段变化会改变指纹，即使磁带数量不变也产生通知；输入数组顺序、轮询时间或 owner 变化不改变同一物理库存的指纹。每次合法采集仍更新缓存中的时间与来源，不重复追加相同事件。

巡检记录包含 version、namespace、library_serial、owner_epoch、interval_seconds、last_attempt、last_success 和 error。interval_seconds=30 描述预期采集周期，存储方法本身不启动定时任务。调用方在采样结束时固定captured_at并显式交给record_library_monitor，事务和回执重试不重新取当前时间。成功采集的 last_success 必须与该次 last_attempt 和原采样时间相同；缺失或不一致视为记录损坏，不由下一次心跳补齐。失败保留上次成功时间；同一 error/status 不因心跳、时间或 owner 更新重复通知。成功/故障或诊断内容变化生成 `library.monitor.changed`。错误最多 2048 字节，持久记录上限 16 KiB，JSON 转义后的长度同样受限。

这些是带时间的历史事实，不能仅凭 error=null 显示“实时健康”；展示层须结合 last_attempt/last_success 的新鲜度与当前 owner。库存读取与巡检故障独立：失败不会删除上一次完整库存，也不会把未知状态补成空库或健康。

两种通知都带库级作用域，不携带 pool/barcode。订阅设置了池或条码过滤时不会收到这类全库事件；需要全库事件时创建不带资源过滤的相应类型订阅。单盘位置、归属和健康事件的生产者仍待后续集成。

查询和写入都核对原缓存所对应的通知投影及不可变事件证明，包括缓存缺失而投影仍在的情况。原事件、投影、缓存缺失或内容不一致时保留现场并报错。通知追加失败会使整次事实更新回滚；FDB 不可用时不回退 SQLite 或内存事实源。事件和旧 namespace 记录没有自动回收，一般目录清理不应绕开通知证明删除它们。

隔离 FDB 测试覆盖槽位间同数移动、装载变化、排序及轮询去重、owner 接管、上次成功保留、带库隔离、超限拒绝、缓存/证明/原事件损坏不修复，以及通知追加失败的完整回滚。这些合成记录测试不等同真实带库监控或物理 HA 验收。
