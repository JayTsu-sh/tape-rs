# 达到并超过 LE 的性能：证据、优先级与验收

本轮为分析，不改运行行为、不访问硬件。依据源码93cd0fb及已归档RC18实机证据；所有收益预测都是待验证假设。

## 已知基线

- 16GiB：Rust63.583s、LE62.942s，Rust耗时多约1.0%，大流量已经接近。
- 256×4KiB：Rust25.820s、LE20.743s，耗时多24.5%。2500×64B：31.051s/20.745s，多49.7%。只是既有样本，不是统计显著差异。
- 新轨迹LE普通index阶段也约13s，与Rust13.37s接近；先前LE8.6s和Rust13.37s不能直接当作固定软件差额。
- 同卷最新诊断Rust cold mount124.232s；LE move后open按需mount18.461s；不是严格A/B，但工作量差异值得优先拆解。
- LE缓存读19.982s不是磁带速度，不能拿它对比Rust真实SCSI读88.731s。

证据：[最终实机报告](physical-rc18-final-validation-20260927.md)、[LE API/CDB报告](le-api-scsi-direct-io-research-20260927.md)。

## 成功标准

相同文件集/压缩率/块长、硬件、介质布局与起点、缓存状态、哈希与持久化边界。普通DP sync对普通DP sync；IP checkpoint/装卸单列；库接口与带Raft/catalog/spool的服务接口分开。不能用缓存返回、异步接收或推迟sync来替代已持久化完成。

采用交错A/B和B/A、多轮结果及离散度，不要求每次绝对更快。初步目标为大文件总时长在LE±5%内，小文件等同步频率下稳定不慢于LE；“超过”要求多轮收益超过噪声，且冷挂SHA、LE互读、故障恢复仍通过。这些是验收目标，不是当前成绩。

## P0：先做对照计量

给Rust加与LE相同的阶段/CDB统计：opcode、分区、字节、耗时、返回状态、FM IMMED、VCR/VCI、PR guard；单列CPU、源读取、哈希、XML、目录发布。探针开销两边相同，计时之外做数据SHA，另测带哈希的服务工作负载。重点测最终FM0、每文件READ POSITION和挂载LOCATE。

## P1：常驻卷会话，消除服务端逐批重新挂载

已确认 `src/daemon/executor.rs:1190` 在每一批process_uploads中创建LtfsVolume::mount；写带上的read路径1390以后也经read_file重新mount。第二读驱动器已有ReadView缓存，不能说所有读都重挂。基准examples/performance_compare已经跨批保留vol，所以基准未暴露服务端这部分代价。

推荐设备执行线程拥有活动卷会话，绑定drive serial、volume UUID、执行round/PR key、介质epoch；同介质同轮复用工作/已提交索引和追加状态。跨读取后更新真实位置，不能用旧write head假定当前物理位置。换带、接管、复位/UA、失权、任何不确定写入后销毁会话，重新完整恢复。保持失败冻结和提交后发布，不以缓存绕过资格检查。通过分离owned卷状态和短期设备借用避免不安全自引用/生命周期伪造。

验收：20连续批次只初次mount；中途读写切换、强制UA、失权、失败提交、同条码不同UUID均正确；真实带多批写/重装/互读。收益取决于当前每批mount实测，不承诺固定倍数。此项最优先。

## P2：正常挂载先用一致性信息，恢复扫描按需执行

`recovery::scan_partition` 先LOCATE EOD才读VCI提示索引；`recover_inner`总扫描DP和IP、验证追加位置。当前优化只省历史链，仍存在多次跨分区移动。

先做保守优化：一次收集MAM，合并同分区读取顺序，避免来回定位。进一步设计正常IP一致Full挂载：验证UUID、label、当前VCR与双VCI关系、索引自指针/代数/完整闭FM后建立视图；读模式不提前把磁头定位DP写入点，首次append才验证追加位置。异常、增量、IP欠账、信息不一致进入现有恢复扫描。

重要限制：目前EOD验证防护包括VCI之后还有新构造、Holo的VCR不随写入变化。不能只因VCR数值相同就删EOD检查；若无法证明正常快速路径同等安全，保留现有验证，仅减少重复移动。用户“不支持EOD不保留兼容回退”仍有效。这是恢复策略设计，不是删几条CDB。

验收：正常Full、IP欠账、增量链、陈旧/损坏VCI、截断index、提交后VCI失败、VCR异常均覆盖；首次open、首次read、首次append合计分别比较，不能将工作移到first read后宣称总成本消失。

## P3：减少小文件的逐文件控制开销

`append_blocks` 每文件检查资格并locate_if_needed（READ POSITION），随后才WRITE；read_index_file每文件分配块缓冲、每extent核对位置。2500个小文件把这些固定成本放大。

在P1会话所有权成立后，在设备执行层维护已知位置，成功READ/WRITE推进、LOCATE/SPACE更新，任何UA/reset/异常/外部访问失效；所有能改变位置的命令必须经过该层。PR不能阻止同initiator其他句柄，因此还需本地访问约束，不能直接删read_position或逐文件guard。先量化收益上限：N×每次查询耗时。

读缓冲跨文件复用；大量同目录文件的线性查找/去重可用内存辅助索引加速，保持持久化XML不变。小文件打包进共享record虽可能进一步减少CDB，但改变record布局且需extent byte_offset/覆盖语义/LE互读专项验证，不作为首轮默认方案。

## P4：证明并移除真正重复的同步工作

当前commit_inner闭FM非IMMED后仍有guard→FM0→VCR/VCI；LE普通DP轨迹没有最终FM0。先测FM0实际时间，只有证明闭FM已满足原S4屏障、资格检查/延迟错误/发布顺序仍正确，才合并。FM0可能几乎免费，不能预支5秒收益。

不删非IMMED闭FM，不跳过应更新的MAM，不将PR guard降级为任意时间缓存。LE index样例7.46s闭FM+约6.10s MAM，XML WRITE约5ms，所以零拷贝/压缩XML不能消除十几秒机械同步。

## P5：现有增量index与合批，针对大目录形成优势

现有incremental_since仍clone完整index再计算差分，提交后又clone，executor的catalog_of也遍历全目录。百万级文件应跟踪dirty路径/元数据，生成等价增量，减少全树克隆和全量目录扫描；保证原子发布、重命名/删除正确。当前连续增量上限5，达阈值commit会写DP/IP Full；不可盲目扩大，应联合评估恢复时间与checkpoint成本。

增量收益来自大目录XML生成、传输和恢复负担；只有约3155文件时XML发送很短，预期不会直接节省当前7–13秒屏障。LE2.4.8的Full和本项目2.5增量必须分开展示，定期Full/清洁卸载后检验LE兼容性。

已存在8GiB/20000文件/2秒idle/60秒max-wait合批，优先调优现有策略而非再造。并发请求可共享一次真实commit，所有成功仍等待该commit；严格逐个等待fsync完成的串行应用无法免费合批。两侧允许相同合批时再比较，另报单请求延迟。

## P6：持续供数和读调度；最后考虑SG direct

先通过有界双缓冲重叠源读取/哈希与单一顺序SCSI执行，保持同drive有状态CDB顺序，错误时不重复WRITE。buffer大小按测得供数抖动决定。大流量现已接近，不预期靠此翻倍。

多文件读按磁带/分区/块位置调度，在API允许的窗口内排序，结果仍对应原请求；先服务已装载带，避免单驱动器读写来回切换。缓存命中单列且按UUID/代数失效。增加第二驱动器后才讨论预装下一卷并行；当前单驱动器、RC17未重新验收，不能称跨盘换带已验证。

SG direct继续关闭。它只减少SG payload复制，不省FM/MAM/机械动作；此前EINVAL无成功A/B。只有CPU/复制已成为实测瓶颈且映射兼容性解决后，才值得另行评估；不视为本轮实施前提。

## 推荐顺序

P0计量 → P1常驻卷 → P2正常挂载 → P3小文件/位置 → P4同步屏障审计 → P5大目录增量/合批 → P6流水线/读调度。P4可以在P0发现高耗时时提前；没有独立证据时不更改安全步骤。

参考：[官方LTFS源码](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c)、[Linux FUSE缓存语义](https://docs.kernel.org/filesystems/fuse/fuse-io.html)。缓存返回速度与真实磁带吞吐必须分列。
