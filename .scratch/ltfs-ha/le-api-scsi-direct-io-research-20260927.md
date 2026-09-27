# LE sync / fsync / fdatasync、装卸、open 与 SG direct I/O

日期：2026-09-27。结论依据分为现场安装版轨迹、参考源码和官方语义，不能互相替代。

## 范围与复现条件

- 活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`；生产代码未修改。
- 主机 `10.131.4.143`；IBM LE 2.4.8.2 (10520)，私有 admin17600，`sync_type=unmount`。
- Drive IBM TD9 R3G0 / 11EB4A80F1；LE 使用 `/dev/sg4`，Rust 使用同 LU `/dev/sg3`；changer `/dev/sg5` / 55L3A7802K19LL01。
- RC0018L9，UUID `878f7b34-4910-4d09-9f1f-3e4725ede6d8`；新增10个独立4KiB文件，无格式化。RC17/.209 未动。
- `allow_dio=0` 全程保持。外层 strace 采集应用 syscall，另一个 strace 跟踪 LE 全线程 ioctl；以纳秒事件时间对齐，调用间保留0.3s观察窗口。
- 时间是本次带状态下的诊断样本，含 ptrace 开销，不作为 LE/Rust 严格性能 A/B。每次 dirty 调用后单独卷同步，为下一样例建立干净起点。
- probe：`probes/physical-le-api-trace-20260927.py`；分析器：`probes/analyze-le-api-trace-20260927.py`；原始证据：`probes/results/physical-le-api-20260927/`。
- 本次未做掉电注入；最终冷读是在完整卷同步和卸载之后，不能证明 fsync 单独调用后的崩溃持久性。

## 同步接口：现场结果

这里的 DP=数据分区1，IP=索引分区0。XML index 与文件数据都使用 WRITE(6)，必须结合阶段和块长度区分。

| 操作，均为新4KiB文件 | 本次核心 SCSI | 调用耗时 | 再次 clean 调用 |
|---|---|---:|---|
| write | 无；数据进入软件缓冲 | 约0.3–1.1ms | 不适用 |
| fdatasync | 1×WRITE(6)，4096字节 | 13.238 / 13.285s | 约0.7ms，无CDB |
| fsync | 1×WRITE(6)，4096字节 | 13.311 / 13.408s | 约0.8ms，无CDB |
| setxattr(user.ltfs.sync) | 数据WRITE → DP完整XML index/FM → VCR/VCI | 27.033 / 27.199s | 约5.6–5.9ms，仅健康/属性查询，无index重写 |
| leadm tape sync | 同上，另有管理层状态查询 | 27.345 / 27.405s | 约0.257s，仅状态查询，无index重写 |
| close，之前没有barrier | 1×WRITE(6)，4096字节 | 13.660s | 已fsync后close无CDB |
| syncfs(fd) | 无CDB | 0.323ms | 后续close才WRITE，13.650s |

`fsync` / `fdatasync` 的普通正常路径在这台 LE 上相同：它们完成文件队列刷新，**没有卷 index / FM / MAM 写入**。随后显式 `ltfs.sync` 才花12.75–13.33s写索引。不能将文件 flush 耗时与卷 index sync 耗时直接相减并归为实现效率差距。

参考源码中两者进入同一个 FUSE 回调，`isdatasync` 只记录、不分支；底层正常 flush 没有无条件追加 FM0。特殊跨分区保护或错误恢复可能另写 index，不能推广为任意场景绝不出现index。当前 `sync_type=unmount` 下 close 不自动卷同步；`sync_type=close` 会改变此行为。详见[固定提交源码研究](le-api-source-paths-20260927.md)。

**Linux sync 不是 LE sync。** IBM 明确说明 Linux sync 命令及 sync/fsync/fdatasync 系统调用不会自动触发 LE 卷同步；需要 `leadm tape sync` 或 LTFS 扩展属性。现场为了只同步目标挂载测了 `syncfs(fd)`，没有运行全主机 `sync()`，不能把它的精确CDB数量假装成全局sync实测。来源：[IBM LE Sync operation](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=tips-sync-operation)。

正常卷同步的代表性实测 `ltfs-sync-0:ltfs-sync`：

```text
WRITE(6) data 4096                         13.396620s
READ ATTRIBUTE 0x1623                      0.001009s（可选属性探测，05/24/00）
READ POSITION                             0.000567s
WRITE FILEMARKS count=1, IMMED=1           0.003817s
READ POSITION                             0.000733s
WRITE(6) XML: 3×524288 + 238894 bytes      合计0.005154s
WRITE FILEMARKS count=1, IMMED=0           7.461948s
READ POSITION                             0.000664s
READ ATTRIBUTE 0x0009 VCR                  2.830423s
WRITE ATTRIBUTE 0x080c VCI, partition0     1.638949s
WRITE ATTRIBUTE 0x080c VCI, partition1     1.628061s
TUR / LOG SENSE / optional-attribute probe
```

开FM CDB `100100000100`，闭FM `100000000100`。本例写入位置已在DP追加点，没有LOCATE，也没有额外FM0；目录baseline从读位置切回DP时有LOCATE。主要同步耗时在非IMMED闭FM、VCR/VCI，不在把约1.8MiB XML送进驱动器。该结论说明耗时分布，不授权删除这些持久化/一致性步骤。

轨迹包含非零SCSI状态，不能把整轮成功写成“所有CDB status=0”：新介质LOAD有06/28/00；未就绪TUR有02/04/02；正常filemark/短块读取有对应sense；baseline采用最大block定位得到08/00/05(EOD)后继续；可选属性0x1623查询49次05/24/00。原始sense均保留。这些是安装版LE的状态/探测处理，不据此给本项目新增EOD兼容回退。

干净 `ltfs.sync` 仍有两条 LOG SENSE 和一次可选 READ ATTRIBUTE，并非完全零CDB。某些短阶段中的 TUR 可能是同时间窗内后台查询；分段是时间相关性，不是每条命令的因果调用栈。

补充metadata样例：chmod(0600→0640)后 fsync/fdatasync/ltfs.sync 未写index；本次没有同时验证 LE 实际持久化了该权限变化，因此此样例不能证明一般metadata-only更新都不需要index。

## 打开、读取与关闭

| 阶段 | 实测耗时 | 轨迹 / 边界 |
|---|---:|---|
| leadm move到drive | 27.134s | TUR、LOG SENSE、MOVE MEDIUM；仅机械搬入，不等于LTFS已挂载 |
| 搬入后第一次open(已有文件) | 18.461s | 按需drive load、参数/容量/MAM、两分区label/index读取，建立卷视图 |
| 第一次read(4096) | 47.297s | LOCATE16(P1, object34829) → READ POSITION → READ6 |
| 已挂载后再次open | 0.249ms | 无CDB，查已有目录/index并分配句柄 |
| 只读close | 0.804ms | 无CDB |
| dirty写文件close | 13.660s | 刷新数据WRITE，当前配置不提交卷index |

首次 open 是完整 open(2)，包含路径查找，不能把按需挂载全部归到单一 FUSE_OPEN 回调。首次open内的READ是标签/索引；第一次文件内容READ在随后read阶段。首次load出现3条LOAD CDB：第一条返回Unit Attention `06/28/00`，后两条成功；不能把3次CDB等同3次物理装带。

参考源码的热open路径 `ltfs_fuse_open → ltfs_fsops_open → iosched_open/raw_open` 本身不预读文件内容；LE的带库按需挂载是安装版管理行为，以上以实测为准。[参考源码与条件](le-api-source-paths-20260927.md)。

## 卸载与重新装载

第一次卸载前，DP已sync，但最新完整索引仍需写到IP。实测 `leadm-unload-dirty-ip` 175.923s：

```text
TUR / 健康与MAM查询
LOCATE16 P0 object4               36.165s
ALLOW OVERWRITE P0 object4
FM1 IMMED1 → XML WRITE×4 → FM1 IMMED0（闭FM 6.172s）
READ POSITION → VCR → VCI P0/P1
健康统计 / MAM读取
ALLOW MEDIUM REMOVAL
REWIND                            3.160s
READ POSITION
UNLOAD                            94.028s
MODE SENSE/SELECT
MOVE MEDIUM drive→slot7          30.235s
```

LE重新搬入25.072s，open+读取校验18.568s。这个阶段发生按需挂载和index读取，但没有新文件extent的数据READ，说明内容校验可由缓存满足，**不能将此阶段称为冷磁带数据验证**。

随后没有新写入的 clean 卸载69.100s：健康/属性查询 → ALLOW → REWIND 4.389s → READ POSITION → UNLOAD 30.374s → MODE SENSE/SELECT → MOVE归槽34.025s，**没有LOCATE、WRITE、FM或MAM写入**，跳过IP checkpoint。

175.9s与69.1s的差额不能全部当成索引成本：两次UNLOAD本身分别94.0s与30.4s，卷绕位置/机械状态也不同。完整命令顺序、设备路径、CDB、原始行号和sense在 `le-api-decoded.json`。

## SG direct I/O 开与关

这里首先指先前调整的 **SG_IO direct I/O**，不是文件open的O_DIRECT，也不是驱动器buffered mode。

```text
SG indirect WRITE:
  用户缓冲 → CPU复制到sg内核缓冲 → HBA DMA → 驱动器缓冲 → 磁带
SG direct WRITE:
  固定/映射用户页 → HBA DMA直接取用户页 → 驱动器缓冲 → 磁带
READ为相反方向；两种模式都有DMA，direct省去的是中间payload复制。
```

| 维度 | 关闭 | 请求并真正采用direct |
|---|---|---|
| SG缓冲路径 | 内核预留/动态缓冲，加用户/内核复制 | 用户页pin/map，直接DMA，完成后解除映射 |
| CPU/内存带宽 | 多一次payload复制 | 可减少复制；增加每次pin/map成本 |
| 对齐/映射限制 | 使用内核缓冲路径 | 受对齐、HBA映射、散布列表等约束 |
| CDB、FM、分区、MAM | 按上层逻辑 | 不因为direct开关而改变 |
| 驱动器写缓存、index持久化 | 必须靠对应同步步骤保证 | 同样必须，direct不是sync |
| 性能 | 可能已达磁带流速 | 大块有机会节省CPU；不保证更快，机械等待不消失 |

Linux v5.14 `sg_start_req` 只有在 `sg_allow_dio`、请求 `SG_FLAG_DIRECT_IO`、方向、无用户iovec、队列对齐等条件满足时才选择直接映射；否则走间接缓冲。`allow_dio=1` 只是允许，不能强制所有请求direct。应用应检查成功返回后的 `info & SG_INFO_DIRECT_IO_MASK`：0=indirect、2=direct、4=mixed，不能只看提交的flags。来源：[Linux v5.14 sg.c](https://kernel.googlesource.com/pub/scm/linux/kernel/git/torvalds/linux/+/v5.14/drivers/scsi/sg.c)、[sg维护者HOWTO](https://tldp.org/HOWTO/SCSI-Generic-HOWTO/dio.html)。现场是vendor 5.14内核，该上游源码是机制依据，不是对vendor补丁的逐行验证。

老HOWTO称不满足条件可退回indirect，但现代代码中 `blk_rq_map_user` 失败可直接返回错误，不能保证任何direct失败都透明重试。该源码在成功映射时设置DIRECT标志，并返回映射错误；本机此前 `allow_dio=1` 的512KiB READ(6)实际得到 `EINVAL`，没有完成读。改为0后读取成功。现场根因尚未独立证明，不能只根据页数推定具体HBA限制；本轮没有重新开启，没有可用的成功direct吞吐A/B。既有证据：`probes/results/physical-rc18-reboot-direct-20260927.json`。

**文件系统direct I/O是另一层。** FUSE的FOPEN_DIRECT_IO绕过内核page cache、关闭预读；缓存模式可以直接从page cache返回而完全不读磁带。这不会自动使LE后端SG_IO采用direct。即使SG采用direct，上游FUSE/应用/调度器仍可能复制数据，不能称端到端零拷贝。来源：[Linux FUSE I/O modes](https://docs.kernel.org/filesystems/fuse/fuse-io.html)。

## 对本项目的实施含义

1. 对标时分开文件flush、DP卷sync、IP卸载checkpoint，保持相同dirty/clean和带位置；不能以LE fsync对比Rust卷sync。
2. 非IMMED闭FM与MAM占用真实时间；剩余优化要逐CDB测量，不能删除持久化屏障来“追平”。本轮LE index阶段约13s，旧基线约8s，说明先前约5s差额尚不能归因为Rust固定开销。
3. 热open应不触碰硬件；cold open/mount与first read分别计时，缓存命中读单列。
4. 保持SG direct关闭。若以后专项研究，需要独立验证映射限制、实际DIRECT返回标志和SHA，再做相同媒体状态的吞吐/CPU对比；当前无证据支持仅打开开关即可提速。

## 验收与最终状态

- 应用78个分段均完成；LE trace解码551条SG_IO，未解析/挂起记录0。读写87条均请求SG_FLAG_DIRECT_IO但返回indirect。
- LE正常退出后Rust从/dev/sg3冷挂载：generation28，124.232s；10个新文件/40960字节读取14.755s，全部SHA验证通过。Rust读21条indirect、0 direct/0 mixed。
- 此验证发生在完整卷sync和卸载之后，不验证单独fsync的断电恢复保证。
- probe和解析器Python语法检查通过；没有改生产Rust，因此本轮未重复运行Rust测试套件。
- 最终只读确认：RC18槽7、RC17槽8、drive空；LE退出、FUSE已卸载；PR generation20、注册表空/无持有者；allow_dio=0。`le-api-trace.complete`与`le-api-final-state.json`已保存。
