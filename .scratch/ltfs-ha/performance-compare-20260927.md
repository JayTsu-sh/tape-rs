# LE 与直接 Rust 库性能对照（2026-09-27）

两边各完成两轮，3210 个文件逐一核对内容 SHA256，并完成每轮六次交替换带后的 8 MiB 首读。环境为 Holo VTL，经 iSCSI 的同一主机 .72、同一 LISA 驱动器 IBMlisa42299、相同 512 MiB 专用介质 PF2701L08/PF2702L08。每轮从相同 UUID 的已格式化空卷备份恢复。没有访问物理 TS4300。

## 计时口径修正

之前探针注释和进度消息将 LE sync 与 Rust Full commit 都称作 Full 提交，不准确。LE 使用 user.ltfs.sync；IBM 文档明确普通 sync 将索引写到 DP，卸载时再更新 IP。Rust commit() 每次写 DP + IP 两份 Full 索引，并执行最终屏障及 MAM VCI 更新。因此下表是两种默认接口策略的端到端比较，**不是相同 SCSI 工作量或相同提交终点的性能比**。Full 描述索引内容完整性，不能据此推导 IP 已更新。

依据：[IBM LE Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.6?topic=tips-sync-operation)。尚未采集两边逐 CDB 轨迹，LE 实际版本的每条命令数量仍待核实。

Rust 写入默认计算并保存 SHA256；LE 未配置同等摘要写入。读时两边均由测试端流式计算 SHA256。Rust 每组使用新进程并 mount，写入计时单独排除 mount/source 预校验；换带总计时包含进程、mount 和校验路径。LE 保持常驻进程，包含 FUSE 开销。目录操作分别是驻内存索引与 FUSE scandir。首次读取在卸载/加载后，LE 另执行 unassign/assign；未全局清理操作系统/设备缓存，不能叫物理冷读。

## 两轮区间（秒）

| 负载 | Rust | LE |
| --- | ---: | ---: |
| 4 × 8 MiB 写 + 提交 | 1.159–1.404 | 0.655–1.585 |
| 256 × 4 KiB 写 + 提交 | 1.032–1.679 | 0.610–0.619 |
| 2500 × 64 B 写 + 提交 | 2.279–2.707 | 2.080–2.389 |
| 12 × 8 MiB 读与 64 × 32 KiB 写交替 + 提交 | 2.222–2.897 | 0.985–1.371 |
| 128 MiB 写 + 提交 | 2.551–2.636 | 1.513–1.616 |
| PF1 全量首次读（3208 文件） | 2.671–2.740 | 1.610–1.617 |
| PF1 全量再次读（3208 文件） | 2.518–2.578 | 0.593–0.608 |
| PF2 全量首次读（136 MiB） | 1.619–1.690 | 0.489–0.517 |
| PF2 全量再次读（136 MiB） | 1.616–1.643 | 0.493–0.525 |
| 换到 3208 文件卷 + 首读 8 MiB（各六次） | 1.405–2.061 | 1.457–2.462 |
| 换到 2 文件卷 + 首读 8 MiB（各六次） | 0.179–0.698 | 1.364–2.330 |

LE 并非所有负载都快；大文件批提交第二轮存在 1.372 s 提交波动，2500 小文件两边接近，换入小索引卷 Rust 更快。不能从两轮得到稳定 p99 或物理机械性能。

## 从 SCSI 路径看差异与优化顺序

1. **先补命令级证据并对齐提交边界。** 统计 LOCATE(16)、READ/WRITE(6)、WRITE FILEMARKS、READ/WRITE ATTRIBUTE、LOAD/UNLOAD、MOVE MEDIUM 的数量、字节、分区转换和服务耗时。性能轮关闭详细跟踪，另设诊断轮。LE 普通 sync 对比 Rust DP 持久化策略，双分区终点比较则应计入 LE 卸载更新 IP 的成本；不可直接删除 Rust 屏障伪造同等耐久性。
2. **连续 I/O 的位置跟踪。** volume.rs 当前每次 append_file 都 LOCATE(P1,write_head)，每个读 extent 都 LOCATE(start)。单 extent 的 2500 个 64 B 新文件，数据路径静态推导为 2500 LOCATE + 2500 WRITE，尚不含提交/mount。若确认位置已在目标块，可省定位；介质更换、分区变化、错误/残差、UA、预留丢失或其他使用者操作须使位置失效。先做独占执行器内的连续追加，再做按物理顺序读取。
3. **提交策略。** Rust 每次 Full：定位 DP → opening FM → XML WRITE → closing FM → 定位 IP → opening FM → XML WRITE → closing FM → 零计数 FM 屏障 → VCR/VCI MAM。下一批又返回 DP。第二轮 256×4KiB 总 1.032s 中提交 0.851s，占 82%；LE 0.610s 中 sync 0.411s。可先批量合并提交、使用既有增量能力；进一步的 DP-only 持久化 Full 与 IP checkpoint 策略需要恢复/互操作设计和断电故障测试，不能直接改变默认确认语义。增量索引与 DP-only Full 是不同维度。
4. **读缓存及挂载恢复。** 直接库再次读取仍向设备发 READ；LE 的重复读可能命中 FUSE/OS/内部缓存，因此重复读结果不等于驱动器吞吐。先复用已验证索引与缓冲区，按卷 UUID/代数/所有权失效，再评估有限数据块缓存与物理顺序预取。既有第二驱动只读 ReadView 优化已完成，写卷挂载路径仍有空间；不能为速度跳过异常尾部恢复或 fencing。
5. **小文件块合并与数据流水线。** 当前一个 64 B 文件至少一个记录；将多个小文件合并到一个 LTFS record、用 extent byte_offset 描述，可能减少 READ/WRITE 命令，但比跳过多余 LOCATE 风险大，需 IBM 双向兼容与跨块/崩溃验证。大文件已按 512 KiB 记录写，128MiB 为 256 个数据 WRITE；优先复用缓冲、将输入读取/哈希与单线程设备 I/O 流水化。不要直接扩大变长 READ 的长度并误以为会返回多个记录，也不要并发提交会改变磁头位置的命令。
6. **换带调度。** 按卷合并读请求，卷内按 extent 排序，利用已有第二驱动减少读写竞争；设置公平性避免饿死冷卷。换带分解为 flush、卸载、机械移动、装载、索引验证、首读。LE 常驻索引缓存有优势空间，但本轮换到小卷 Rust 已更快，不能将 LE 描述为换带全面领先。

没有证据表明语言运行时本身决定差距，也尚不能声称 LE 通过小文件打包、异步 SCSI 或更大记录获得优势；这些需要实际 CDB 轨迹证实。物理设备上的寻道、分区切换、流带速度与 Holo 不同，物理验收待环境恢复。

## 证据与现场

probes/results/performance-compare-* 保留两轮输出、资源记录、清单和失败记录。native-first/native-final/le-first/le-final 介质树保存在 .70 /home/rocky/holo-performance-compare-20260927，空卷基线同目录。两张 PF 已卸载归槽且取消 LE 分配，最终 LE 数据保留。源数据由 performance-compare-data-20260927.py 重现；examples/performance_compare.rs 是直接库入口。未更改默认 SCSI/提交行为，未关闭任何告警。

初次在 node1 格式化 Holo 小容量空卷失败，未使用其结果；改在 node2 初始化专用卷。初次 LE 移带后缺少显式 mount 导致 EBUSY，未写入数据；修正探针后两轮全部成功。历史失败不计入成功样本。
