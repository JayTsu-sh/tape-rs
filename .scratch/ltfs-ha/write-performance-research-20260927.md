# 写入性能调研（2026-09-27）

范围：活动工作树 `ltfs-recovery-ibm-interop`，基线 `2f46b06` 加当前未提交的追加定位优化。只调研、写本文；未操作远程设备、未改代码。实机数据由主执行任务提供，完整读回验收仍在进行，不能把本表当成已验收报告。行号对应调研时工作树，后续修改可能移动。

结论：先减少不必要的定位与同步等待，再评估 indirect I/O 的缓冲区复用。现有最强证据是小文件间重复 LOCATE；大文件数据阶段此前已接近 LE，当前没有证据证明内存复制是主要瓶颈。保持用户关闭的 `sg.allow_dio=0`，不以重新启用 direct I/O 为任何方案前提。

## 口径与实测边界

普通同步写 DP 索引，卸载补 IP；Linux `fsync`/`fdatasync` 不是 LE 索引同步命令，应使用相同的显式 LTFS 同步边界。[IBM LE 同步说明](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=tips-sync-operation)。本项目对应 [volume.rs:1102](../../src/ltfs/volume.rs#L1102) 的 `sync` 与 `commit`。除已有增量索引外，不以减少承诺的同步次数或改变文件布局制造优势。

| RC0018L9 负载 | LE 数据 / 普通同步（秒） | Rust 旧版 | Rust 追加定位修复版 |
|---|---:|---:|---:|
| 16 GiB 顺序写 | 55.90 / 7.04 | 56.18 / 10.50 | 64.23 / 10.63 |
| 256 × 4 KiB | 12.14 / 8.61 | 554.89 / 15.10 | 26.26 / 19.93 |
| 2500 × 64 B | 12.54 / 8.20 | 本文不引用 | 18.33 / 16.21 |
| 连续每批 32 × 4 KiB | 约 12.5 / 8.6 | 本文不引用 | 数据约 13–27 / 同步约 12–16 |

这是不同轮次的观测，不是重复试验置信区间。旧版 strace 显示每文件 LOCATE 约 1.9 秒、WRITE 约 0.06 秒；追加优化后该组数据阶段下降约 21 倍，但仍不能解释全部差距。16 GiB 的修复版变慢也说明不能把单轮变化都归因于代码。小文件阶段存在缓冲和机械等待，必须报告 `数据 + sync` 总时间，不能将缓存接收速率命名为磁带持续吞吐。

同样每批 32 文件，LE 约 50 条设备命令、Rust 约 77 条；Rust 的逐文件 READ POSITION 可能解释约 32 条差额，但这只是待 trace 的假设。命令少不自动意味着快，须看累计耗时、长尾及命令间空隙。既有诊断入口和历史证据见 [le-io-alignment-20260927.md](le-io-alignment-20260927.md)。

## 按顺序实施

| 顺序 | 工作 | 收益依据 | 风险与验收 |
|---|---|---|---|
| P0 | 完成本轮全量 SHA、卸载重载恢复；另开诊断轮记录 MODE SENSE、CDB 次数/时间 | 先区分驱动模式、定位、flush、MAM 与 CPU | 只读模式核验放计时之外；固定驱动序列号、条码、UUID、固件、HBA、512 KiB 记录及随机源；同口径至少多轮 |
| P1 | 将已有精确实时位置检查用于普通 sync 的 DP LOCATE | `commit_inner` 仍无条件定位，一次 sync 一次；已存在安全辅助函数 | 目标一致才跳过；未知位置、LOLU/PERR、超 32 位目标继续 LOCATE；保留资格检查、错误冻结；模拟器及实机交叉读写 |
| P2 | 对齐索引开头 FM 的 IMMED 时序；随后独立评估重复屏障 | 参考 LTFS 开 FM immediate，普通同步末 FM 非 immediate；项目两次都是非 immediate，后面还有 FM(0) | 先证实安装 LE 的轨迹；先只改开 FM，保留最终屏障；末 FM 或屏障失败仍不得发布已提交视图 |
| P3 | 每设备 fd 设置并读回足够大的 sg reserved buffer | 参考 sg 后端申请 1 MiB；项目未配置，512 KiB indirect 请求可能超过默认预留 | 保持 `allow_dio=0`；首次命令前设置；失败/实际较小记录并安全回退；不改全机模块参数；收益待测 |
| P4 | 流缓冲区复用，必要时有界预读/计算流水线 | 每文件新建 512 KiB 对齐缓冲；小文件有分配清零浪费，大文件读源与写带目前串行 | 先测用户态耗时；保持记录边界、顺序、同步与错误传播；每驱动仍只有一个执行器 |
| P5 | 在独占执行上下文中维护可靠位置状态，减少逐文件 READ POSITION | 参考 LTFS 可用设备状态跳过定位；当前每文件都向设备查询 | 必须先建立状态失效模型；不能把共享裸设备上的推断当真值；预留丢失、reset、UA、读/seek、失败、换带均须失效 |

### P0：buffered mode 与压缩必须读实况

源码事实：挂载 [volume.rs:234](../../src/ltfs/volume.rs#L234) 读取 `label.compression`，没有由此设置或验证驱动器 DCE；`compression=true` 不能证明驱动实际启用压缩。另 [mkltfs.rs:251](../../src/ltfs/mkltfs.rs#L251) 分区 MODE SELECT 的头 byte 3 为零，包括 Buffered Mode 位。这是应核验的模式风险，不能直接断言当前 RC18 无缓冲：该带由 LE 格式化，当前运行状态尚须 MODE SENSE 证据。

读取当前值、可变掩码、块描述符、压缩页，并保存原始响应；对比 LE 初始化前后和 Rust 初始化后的变化。在明确证据前不改模式。IBM 的磁带 SCSI 架构师说明，无缓冲写会等数据落带并造成频繁 backhitch：[IBM 一方技术答复](https://community.ibm.com/community/user/discussion/lto-5-and-ts3200)。这是机理佐证，不能替代当前 TD9 的测量。压缩只对可压缩输入有意义；随机源对照不应靠零填充获得“提速”。

### P1/P5：位置策略

当前 [volume.rs:979](../../src/ltfs/volume.rs#L979) 已用 `locate_if_needed`；[commands.rs:130](../../src/tape/commands.rs#L130) 对位置不可靠和超 32 位范围保留 LOCATE。普通 sync 的 [volume.rs:1198](../../src/ltfs/volume.rs#L1198) 尚未复用它，这是优先级最高的小改动候选。不要扩散到恢复搜索或 IP 位置切换，后者有不同语义。

参考实现的 `tape_seek` 在目标与已维护位置一致时跳过后端 LOCATE，仍有位置结果核验：[固定提交 tape.c:999](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/tape.c#L999)。本项目的 transport 可由外部调用者操作，不能照搬缓存假设。P5 必须有单一所有者和完整失效路径，收益通过 READ POSITION 累计耗时证明；不因“每文件一条命令”就贸然删掉安全检查。

### P2：FM 时序与同步屏障

本项目 [cdb.rs:190](../../src/scsi/cdb.rs#L190) 的 WRITE FILEMARKS byte 1 恒为零；DP 时序为 LOCATE → FM(1) → XML WRITE → FM(1) → 资格检查 → FM(0) → 读 VCR/写 VCI。对应 [volume.rs:1196](../../src/ltfs/volume.rs#L1196)。

参考 LTFS 的索引开 FM 使用 `immed=true`：[ltfs.c:2657](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L2657)。末 FM 普通同步为 `false`，仅格式化用 immediate；成功后才发布索引偏移缓存：[xml_writer_libltfs.c:1336](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/xml_writer_libltfs.c#L1336)。这是开源参考源码事实，不代表现场闭源 LE 2.4.8.2 必然完全一致。

建议候选顺序为：数据 → 开 FM(IMMED=1) → XML → 闭 FM(IMMED=0) → 既有屏障 → VCR/VCI → 发布。其目的只是不在索引前增加一次落盘等待，最终仍等待数据、索引、关闭 FM 持久化。IMMED 的提前成功不是 durability；延迟写错误可能在后续命令暴露，任何阶段失败都继续冻结、不重放有副作用的 WRITE/FM。先验证缓冲模式支持、固定 CDB 位域，再测开/末 FM 延迟错误、介质满、UA/预留丢失、关闭 FM 前后崩溃恢复及 LE 冷挂全 SHA。

FM(0) 是否冗余应是第二个独立改动：即使闭 FM 已承担落盘，其后的资格检查和错误分类仍是现有安全协议的一部分。不得只删除命令而不重新定义资格检查时点、发布门槛及故障测试。规范依据入口为 [IBM LTO SCSI Reference GA32-0928-05，WRITE FILEMARKS 与 Mode parameter header](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference_GA32-0928-05%20%28EXTERNAL%29_0.pdf)；本轮抓取完整正文不稳定，实施时须按项目使用的 GA32-0928-08 再核对原文。SSC-5 r06、SPC 的正式草案访问有成员限制，本报告未声称逐条阅读付费原文：[T10 官方草案目录](https://t10.t10.org/drafts.htm)。

VCI 暂不优化。当前 [volume.rs:366](../../src/ltfs/volume.rs#L366) 在屏障后读 VCR 并为两分区各写最新索引提示；DP 普通 sync 不把 IP 冒充同代。参考 `ltfs_update_cart_coherency` 同样取当前 VCR，再按完整分区状态更新 coherency：[ltfs_internal.c:1358](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs_internal.c#L1358)。只有 trace 证明它占主要时间、且规范/安装 LE 支持相同延后边界，才考虑减少更新；不能把尚未落盘的索引写成有效提示。

### P3/P4：indirect 缓冲和源读取

参考 sg 后端在打开时申请 1 MiB 并查询实际保留大小：[sg_tape.c:801](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/tape_drivers/linux/sg/sg_tape.c#L801)。[Linux sg 官方维护者文档](https://tldp.org/HOWTO/SCSI-Generic-HOWTO/gs_rs_size.html) 说明预留区按 fd 管理，申请后实际大小可能不同。固定请求尺寸可减少动态分配压力，但这不是零拷贝，也没有已测性能收益。以现场实际保留值为准，不能将 32 KiB 历史默认当成已测事实。

流路径 [volume.rs:950](../../src/ltfs/volume.rs#L950) 每文件新建缓冲，[buffer.rs:13](../../src/scsi/buffer.rs#L13) 分配并清零；[volume.rs:1333](../../src/ltfs/volume.rs#L1333) 填满一记录再执行同步 WRITE。可以先复用缓冲，再测有界双缓冲让源读/哈希与一个在途 WRITE 重叠。保持 partial read 聚合、最后短记录、短写冻结、哈希含义和文件发布时点。`append_bytes` 已可借用内存切片，但 sg indirect 仍复制，不能宣称端到端零拷贝。

不建议直接上多条在途 WRITE、去掉 fd Mutex 或改 sg 异步 read/write 接口。当前 ioctl 中断后还有在途 DMA 等待协议；并发要求每请求独立生命周期、完成顺序、错误后的排空/停止、资格检查和提交边界，不能释放尚在传输的 buffer。[Linux v5.14 sg.c](https://github.com/torvalds/linux/blob/v5.14/drivers/scsi/sg.c) 是评估接口生命周期的源码依据，现场发行版补丁仍须另核对。优先让“源读取并行、磁带命令串行”，风险小得多。

## 暂不采用

- 跨文件 block packing：当前每文件独立记录、extent `byte_offset=0`（[volume.rs:1051](../../src/ltfs/volume.rs#L1051)）。打包会改变布局、缓存和部分记录提交语义；即使格式能够表达，也不符合当前“与 LE 相同 I/O 行为”的默认范围，须先证明安装 LE 的实际布局。
- 为提速把 512 KiB 改为更大记录、固定块模式，或少执行用户请求的 sync：都会改变比较条件；现有卷应遵守 label blocksize，不能仅按 CDB 24 位上限扩大。调块大小是独立新卷实验，不是这轮优化。
- 将快速 WRITE 返回、内核缓存命中、可压缩零数据、减少 durability 直接称为磁带带宽提升。硬件吞吐受主机、块大小、压缩及接口共同影响：[IBM TS4300 性能说明](https://www.ibm.com/docs/tr/STAKKZ/pdf/ts4300-tape-library-documentation.pdf)。
- 因性能问题格式化当前有效卷；改变介质不会修复冗余定位/屏障或主机请求问题。

## 交付状态

已核对项目写入、同步、VCI、模式设置及 sg 路径；引用第一方规范入口、IBM 文档和固定提交源码。本轮未改生产代码，未运行软件测试，未触及真实硬件。以上 P1–P5 均为后续候选，追加定位修复的完整现场验收由主任务单独记录。

## 续查：正常挂载与历史审计边界

固定提交 `ltfs.c` 的 `ltfs_mount` 在两分区 coherency VCR 与当前 VCR 相等时按代数选择最新索引，不逐代遍历所有历史 Full；提示无效则转 full consistency check。[源码](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/ltfs.c#L1763)。这不等同于 `ltfsck` 历史回滚点扫描。

候选实现比参考正常路径更保守：仍扫描双分区 EOD、读取末 Full、核验真实自指针/结束 FM；要求双 VCI 新鲜且代数等于实际索引、两边同代、IP 回指 DP、无增量依赖，才跳过历史 Full。报告新增 `history_checked`，不把未扫描历史写为已验证。`mount_with_budget` 与 `recovery::recover` 保留完整历史审计，增量重放与异常尾部检查不变。此举会改变默认 `mount` 的历史损坏发现范围：独立最新 Full 完整时，旧历史损坏由显式审计发现，不再阻断正常读取；损坏检测没有从审计接口删除。

2026-09-27 实机 MODE SENSE：RC18 在 LE 正常退出、Rust 装载后，模式头 byte3=0x10（Buffered Mode1），压缩页0x0f的byte2=0xc0（DCE/DCC均1）。因此这轮写慢不是无缓冲模式。格式化代码 header 清零仍单列修复，不能混为已测因果。

## 构建告警修复

Zig0.16 把 Rust 传入的 GNU ld 参数 `-Wl,-O1` 作为已弃用的无效参数报告。新增 `probes/zig-linker-compat.py` 只从 `zig cc/c++` 的参数列表移除此项，保留 C 编译优化 `-O3` 等参数、其他 linker 参数、stderr 和退出码；未关闭 `linker_messages` 或 Clippy 告警。用独立模拟 zig 验证了参数和诊断透传，兼容 release 五个入口重新链接无告警，最高 GLIBC 均为2.34。原带告警构建日志仍保存。

后续兼容构建：

```bash
CARGO_ZIGBUILD_ZIG_PATH="$PWD/.scratch/ltfs-ha/probes/zig-linker-compat.py" \
CFLAGS_x86_64_unknown_linux_gnu=-mcpu=baseline \
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 --features fuse --bins --example performance_compare
```

这个环境变量由 [cargo-zigbuild0.22.3自身支持](https://github.com/rust-cross/cargo-zigbuild/blob/v0.22.3/src/zig.rs#L2018)，不修改全局 Zig 或 Cargo 安装。

## 实机追加发现：原生分区参数无效

最终轮在FORMAT之前的MODE SELECT失败（05/26/00），与EOD无关。最小复现sg_raw同CDB/24字节参数返回sense字段指针byte10；将最大分区数从0恢复为MODE SENSE返回的3后指向byte13，将格式识别从0恢复为3后指向byte16；P0由0改1后返回01/37/00参数取整。旧代码还误将PSUM=11视作GB而没有填PARTITION UNITS=9。全部修正后同样接受取整，回读P0=128GB，符合LE当前布局。保留全部失败/成功探针，未将Recovered Error误报为普通GOOD。既有finish_command认可Recovered Error，无需扩大sense忽略范围。

依据：[IBM GA32-0928-08 §6.6.13](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf) 要求不可更改字段保留、IDP的P0非零、单位为10^PARTITION_UNITS；[参考LTFS tape_format](https://github.com/LinearTapeFileSystem/ltfs/blob/4c0e6faae661e0bfd50bc9707f59ecd88c90b7eb/src/libltfs/tape.c#L1491) 先MODE SENSE并将P0申请1GB、P1=FFFF。软件修复采用同样方式；省略block descriptor，缓冲模式1/速度0。

第二个实机缺陷：MODE SELECT修复后FORMAT返回05/3B/0C，READ POSITION证据为partition1/block0。IBM §5.2.3要求分区0起点；旧mkltfs仅REWIND，不能切换当前分区。改为LOCATE(16) partition0/block0/CP1，并在模拟器校验FORMAT位置及缓冲持久化；从DP起点重新格式化的测试先红（同sense）再验绿。没有缩短超时、重试FORMAT或吞掉错误。
