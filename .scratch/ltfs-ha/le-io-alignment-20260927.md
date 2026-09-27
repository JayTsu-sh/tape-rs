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

## 实机新增发现：LTO-9 首次 LOAD 超时

2026-09-27 15:49:26 CST，/proc/scsi/sg/debug显示sg3独占打开，只有一条op=0x1b、timeout=7200000ms、elapsed=426990ms的在途命令；LE日志正在LOAD RC0017L9，尚未写该盘标签。仅观察kernel和LE日志，没有发额外TUR/LOAD，也没有中断。无法仅凭耗时断定一定在介质优化；但[IBM TS4300文档](https://www.ibm.com/docs/en/ts4300-tape-library?topic=features-media-optimization)明确LTO-9首次装带介质优化可能两小时，建议不中断。

因此将TapeDrive::load的SG_IO超时从300000延长至7200000ms，与观察到的LE请求一致；保持单次LOAD及既有UA处理。兼容影响：无响应LOAD最长等待增加；不改变介质内容或CDB位域。新增传输层回归验证调用一次、CDB保持1B0000000100、超时覆盖两小时；9项le_sync测试及严格Clippy通过，兼容构建完成。候选控制CLI另存实机tape-rs-candidate，未覆盖旧验证二进制。新LOAD路径的实机执行仍待当前LE作业完成。


### 实机自动作业交接（15:58 CST）

format-second.py正在RC0017L9的LOAD，内核elapsed约955s，仍只有单条LOAD在途；不要重发或中断。physical-le-runner pid1107426等待format-baseline.complete；physical-native-runner pid1129509等待physical-le.complete。后者会保留LE证据、按顺序重建两盘RC空卷、正常卸载私有LE并核验PR无持有者，才临时开启allow_dio并运行Rust。任意阶段失败均停止。日志均在/root/tape-rs-io-20260927/{format-second,physical-le-runner,physical-native-runner}.log。专用LE pid1085305、admin17600。实机测试尚未开始，不能报告吞吐通过。

LE计时完成后另运行physical-direct-diagnostic-20260927.py，单独追踪8MiB写+fdatasync/fsync/普通sync，不将诊断时间混入JSONL；成功后才发布LE完成标记。Rust下一轮重置会清除这次诊断数据。Native门控固定序列号+清单UUID，控制程序使用tape-rs-candidate（含2小时LOAD），保留旧tape-rs。

候选二进制SHA256：

- tape-rs-candidate: `6f8c525209f5a525597d33b588f16d96aea54b3c7b32ed56ce05bf9b94f0eef5`
- performance_compare: `7f9f6b858554a152b72c5ff2a385a1eba164e13af852ea716812aad2b0b189dc`


## 16:16CST 用户改为RC0018L9

用户要求换成RC0018L9。实查RC17的LOAD仍在途（2031776ms，超时7200000ms）。没有强制中断、复位或机械移动。已核验两个性能supervisor均尚未产生子进程/计时文件，仅停止这两个等待进程1107426、1129509。保留正在执行的LE/format-second作业。新supervisor1170428等待format-baseline.complete后核验两盘均在槽位，正常move/mount RC18；结果写switch-to-RC0018L9.log/.complete。当前未确认换带完成，allow_dio=0。不得恢复旧双带测试与此流程竞争。


## 16:18CST 显式强制中断授权

用户要求强制中断驱动器换带。已停止所有本轮格式化/自动切换控制器，先SIGSTOP私有LE防止复位后继续格式化，再对同一LU naa.5000e111eb4a80f1执行device-only/no-escalation复位（sg5及sg3，均返回0），SIGKILL私有LE并正常卸载死亡的FUSE。未做总线/HBA复位。原LOAD依然在kernel请求表；TUR先返回复位UA，后返回NOT READY/becoming ready。已通过candidate发出UNLOAD（300s），正在等待，不能视为退带成功。库存RC17仍在drive1，RC18slot7。PR仍为原LEkey/type3，没有抢占。若主机退带失败，需要带库管理地址/登录方式，已询问用户。此前两个性能supervisor及自动换RC18等待任务均已停止。


16:25CST结果：主机退带失败，SCSI host error0x0003（超时），实际约326秒。原LE LOAD仍留在内核；没有成功换带，也未向机械手发移动命令。LE及所有自动控制器均停止，私有FUSE已卸载，allow_dio0。等待带库管理IP/登录方式执行驱动器硬复位/强制退带。证据见probes/results/physical-forced-switch-20260927.json。

## 16:59 实机 RC18 重启后验证

RC0018L9 装带已由库存、MAM 条码和 LTFS UUID 三重核实。原配置 allow_dio=0 下，Rust 两次读取空卷 Full1、双分区 Complete 成功。allow_dio=1 时 LE 和 Rust 均无法读取索引；Rust strace 记录 512KiB READ(6) 请求 SG_FLAG_DIRECT_IO，SG_IO 立即返回 EINVAL。尚未证明具体 HBA/页分段根因，不能宣称 direct I/O 在实机验证通过。性能脚本未完成任何写入组；重新读取仍为空卷。LE 正常退出并释放 PR，allow_dio 恢复0，RC18 留在驱动器、RC17 在槽8。证据见 probes/results/physical-rc18-reboot-direct-20260927.json。

## 17:20 用户要求关闭 SG direct I/O

已在 .143 显式设置并回读 allow_dio=0；实机 LE/Rust runner 及 RC18 runner 已改为保持0，不再自动启用，禁用时跳过 direct 专用诊断。后续对照统一使用 indirect I/O。未执行读写或机械命令，未修改启动配置。脚本 Python 语法检查通过。证据：probes/results/physical-sg-direct-disabled-20260927.json。

## 2026-09-27 最终优化轮已通过

源提交344e9b1；259软件测试通过、7硬件测试忽略，严格Clippy/格式/兼容构建通过。RC18原生格式化及3145文件16批写入、卸载重载Rust两轮SHA、LE两轮SHA均成功，完成标志physical-io-final.complete。最终drive空、RC18槽7/RC17槽8、LE退出、PR无持有者、allow_dio0。详细性能、失败及修复证据见[最终验收](physical-rc18-final-validation-20260927.md)。EOD无兼容回退。保留剩余小批sync差距和跨两盘未验收边界，不宣称全面追平LE。
