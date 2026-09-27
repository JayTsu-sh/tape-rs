# LE I/O 语义与 SCSI 拷贝优化（进行中）

用户追加授权：读写行为对齐 LE，保留既有增量 index；性能必须使用 LE 同样的提交口径；检查零拷贝和命令层优化；写请求必须请求 SG direct I/O。不得仅改测试标签来宣称行为已经一致。

前序队列1–5已完成，提交6ac9bc2。正式源af6bfad，三节点tape-rs-rollout-20260927，node1 term56/round499，58行，TS1000 Full13/TS1001 Full120；详见rollout-20260927.md。此次新改动尚未部署，不得把旧验证归为新改动通过。

## 新证据

.72 LE attach strace 实测专用PF2701L08/io-boundary-20260927/data（8MiB）：WRITE16条，每条512KiB；fdatasync等待未完成WRITE，随后fsync无新命令；普通sync没有LOCATE IP，有2个FM、4个索引WRITE和MAM操作；卸载才LOCATE IP。两次刚写文件read均无READ命令，缓存命中。记录在probes/results/le-io-boundary-*。LE WRITE请求SG_FLAG_DIRECT_IO，但info=0；主机/sys/module/sg/parameters/allow_dio=0，未实际direct。源/结果数据为诊断轮，不用于性能计时。PF1已再次归槽/unassign，新增诊断文件保留；其最新内容比le-final备份更新，恢复空卷前必须另存le-diagnostic备份。

依据：[IBM LE sync](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=tips-sync-operation)、[Linux sg.h](https://github.com/torvalds/linux/blob/master/include/scsi/sg.h)。请求direct不等于已实现direct，更不等于HTTP/FUSE/iSCSI端到端零拷贝。

## 当前未提交改动

- ScsiDevice非空READ(6)/WRITE(6)请求SG_FLAG_DIRECT_IO；记录direct/mixed/indirect实际计数，绝不因fallback重发命令。检查CDB/32bit长度、空缓冲指针、sense返回长度。SG_IO=0x2285、repr(C)、超时和数据CDB保持不变。
- TransferBuffer安全Vec切片对齐4096，用于文件流读写。
- append_bytes直接借用源切片；传输层指针回归确认零中间复制；流式append仍适用通用Read，不谎称源文件到设备端到端零拷贝。写失败/短写冻结卷，防止继续追加。
- LtfsVolume::sync只写DP Full；commit保留显式双分区检查点，DP Full已完成则只复制同代IP。unmount补IP。needs_checkpoint用于卸载判断。
- 按用户例外保留增量及原5份增量后的DP/IP Full策略。普通daemon Sync不额外强制IP checkpoint，卸载/关闭仍保留checkpoint。
- 直接库性能示例默认使用sync、关闭额外SHA256索引属性（读校验不变）；增加checkpoint与borrowed入口及actual direct统计。

## 尚须完成，不能宣称整项完成

1. 新逻辑完整软件检查，特别是sync失败/IP失败、未确认版本、数据短写与指针边界；已有第一次full test的2项失败源于旧“普通sync强制Full/IP”断言，已更新契约回归，第二轮进行中。新4项LE sync/API测试、11项增量回归通过；Clippy新告警items_after_test_module已实际移动模块修复，未屏蔽。
2. 重跑同口径Holo/LE性能，含sync、卸载检查点、换带、读、资源与direct成功/fallback。使用同主机/介质/初始状态，追踪轮与计时轮分开。当前正式环境不变；LEnode2仍available。
3. FUSE fsync仍走强归档确认，尚未对齐LE数据刷新。需要把“设备已接收数据但索引未提交”与“已归档”分开，不能用HTTP spool确认冒充数据到设备。连续fsync/同文件改写、读取在途内容、sync/卸载、失败和接管均要有明确状态/测试。
4. 连续LOCATE优化需要可靠所有权/位置失效机制；不能在共享裸设备API上盲目缓存位置。读缓存的实际效果与LE对齐需分缓存命中和真实设备读，保留HA fencing。
5. 完整Holo/FUSE/LE验证后再部署这轮改动，更新报告与交接，物理仍暂缓，无push。


## 2026-09-27 追加检查点：允许实机格式化

用户已明确授权实机性能测试，必要时可格式化。固定测试主机10.131.4.143，介质RC0018L9（已在驱动，来源槽7）与RC0017L9（原I/E3，计划导入槽8）。驱动11EB4A80F1，规范路径/dev/sg3；/dev/sg5为同一LU的另一FC路径，绝非第二驱动。换带器55L3A7802K19LL01，规范路径/dev/sg4。10.131.7.209已有LE实例保持不动。凭据不保存到仓库。

实机读写尚未验收：.143 LE版本2.4.8.2 (10520)，初始无ltfs进程、无设备打开者；PR持有者0x400000000a83048f，类型3；sg allow_dio初值0。准备独立LE目录/root/tape-rs-io-20260927/le-{mount,work}、管理端口17600，显式sync_type=unmount，使用手动sync计时。格式化/写入前必须再次核验介质与库存，保留显式破坏性门控。

Holo direct I/O中断诊断已实测：probes/results/le-io-direct-interrupt.trace。唯一WRITE(6)请求SG_FLAG_DIRECT_IO后被SIGUSR1中断，SG_GET_REQUEST_TABLE显示orphan=1、req_state=1（约2/14/26ms），直到约38ms请求表归零才返回错误；句柄冻结，无WRITE重发。该诊断证明中断后的等待路径，不把未返回的WRITE计入actual-direct成功次数。诊断前aligned-native-final备份和诊断后direct-interrupt备份均保存到/home/rocky/holo-performance-compare-20260927；两盘PF已恢复共同empty基线并归槽。allow_dio恢复0；EE节点2已恢复，三节点available、无任务、f3摘要不变、九项Holo发布ready。

本轮最新cargo fmt --check、全目标全特性Clippy -D warnings及cargo test --all-features通过（235通过、7忽略；随后增加的2项sync故障测试另行通过）。这些结果覆盖新增DMA等待和read/write分离统计；尚不代表物理驱动测试通过。遇到ioctl错误后无法查询在途请求时，当前实现采取进程abort避免释放可能仍受DMA访问的内存，这项故障行为须在交付中明确。


实机准备进度：RC0017L9 已经由 map 地址0x0067移到0x03f0（I/E偏移2→存储槽8），耗时约17秒。独立LE已在端口17600挂载成功，日志确认原PR来自本机且当前路径可预留。leadm显式blocksize参数触发其Python序列化错误，未执行格式化；省略该参数使用默认524288后开始RC0018L9格式化，目前等待前置UNLOAD完成。首次本地格式化工作脚本另有语法错误（执行任何设备命令前失败），两次失败日志均保留。不要同时再发一次格式化或强杀LE；查询/root/tape-rs-io-20260927/format-baseline-r3.log和le-start.log判断进展。两盘格式化成功才生成format-baseline.complete。

源数据完成：3145个RC18文件、2个RC17文件，主连续文件16GiB，另外4×256MiB、256×4KiB、2500×64B、12×32×4KiB、2GiB+256MiB；源文件随机不可压缩，双方读取同一份，不在计时内生成。清单volume_uuid暂为null，须在格式化后取真实UUID再填，禁止绕过校验。实机候选performance_compare已增加--physical固定驱动/介质门控并编译；未跑实机读写。physical-le/native脚本已准备，尚待核验基线并启用同一allow_dio设置。FUSE fsync改造仍未完成。

## 新同口径 Holo 样本（不能替代实机结果）

双方普通同步均持久化 DP，卸载补 IP；同主机、相同空卷基线、同一源数据，均不额外写 SHA256 索引属性，读取完整校验 SHA256。两轮计时都临时启用 sg.allow_dio=1，结束恢复0。Rust这轮实际返回的统计为direct（旧示例合计读写）；LE计时轮没有attach tracer，不能仅从内核开关宣称每次IO均为direct。计时文件为probes/results/le-io-{native,le}-direct-measurements.jsonl。

| 工作负载 | Rust 数据+普通sync秒 | LE 数据+普通sync秒 |
| --- | ---: | ---: |
| 4×8MiB | 0.706 | 0.894 |
| 256×4KiB | 0.601 | 0.607 |
| 2500×64B | 2.201 | 2.213 |
| 单文件128MiB | 3.047 | 1.519 |

6次换带后首读8MiB：Rust中位1.301秒（1.257–1.356），LE中位1.464秒（1.361–2.441）。这是各一轮的观测，128MiB Rust普通sync本轮2.247秒，方差仍大；不能断言语言或direct IO的速度优劣。Native每批新进程重新挂载，LE常驻，mount单列但卸载检查点包含重新挂载成本。旧native日志的unthread_seconds包含checkpoint；探针已拆成checkpoint_seconds和纯unthread_seconds，禁止重解释旧字段。

Holo当前PF最终为本轮LE数据，均归槽且取消分配；aligned-le-final备份与SHA校验已完成。正式三节点未部署候选。

补充前瞻检查：参考LTFS的[tape.c](https://raw.githubusercontent.com/LinearTapeFileSystem/ltfs/master/src/libltfs/tape.c)正常关闭路径通常会关闭append-only模式并释放预留。实机切换到Rust前必须正常退出LE并核验设备状态；该源码不是已安装LE二进制行为的替代证据。
