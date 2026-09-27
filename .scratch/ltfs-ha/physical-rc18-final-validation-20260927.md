# RC0018L9 最终优化实机验收（通过）

活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`。仅10.131.4.143的TD9 11EB4A80F1（sg3，LE sg4）及RC0018L9；changer sg5。SG allow_dio=0，未触碰RC17和.209。结果目录 `probes/results/physical-rc18-20260927/`。

## 已证实的修复

- 挂载直接LOCATE EOD，按用户要求无不支持设备的兼容回退，亦无后续SPACE EOD。旧卷gen24实际挂载126.059s，LE写入3个诊断文件全SHA通过；没有证明它在本次挂载额外提速。
- 原生分区参数错误：模式页只读字段被清零、P0为零、单位指数为零。实机逐字段sense指针10→13→16证明，改读MODE SENSE保留字段、P0=1GB向上取整、P1=剩余、单位9。
- FORMAT位置错误：REWIND只回当前分区起点，失败时实际P1/block0，05/3B/0C；改显式LOCATE P0/block0。
- FORMAT命令已真实完成约24s，卷标/初始索引完成，新UUID `878f7b34-4910-4d09-9f1f-3e4725ede6d8`。Buffered mode1/speed0、硬件压缩保持原值的回读通过。
- 现场LE的3次普通sync均开FM IMMED1、闭FM IMMED0，证据 `le-fm-immed-evidence.json`。最终Rust也采用此开FM策略，仍保留资格检查和最终FM0屏障。

## 写入结果

同一基线3145文件/18256392448字节，16组单挂载。计时均分数据/普通sync；IP checkpoint单列。测量含真实机械行为，单次试验不是统计意义上的提速保证。

| 工作负载 | LE 数据/sync/总计(s) | 上轮Rust 数据/sync/总计(s) | 最终Rust 数据/sync/总计(s) |
|---|---:|---:|---:|
| 16GiB 连续写 | 55.905 / 7.038 / 62.942 | 64.228 / 10.627 / 74.855 | 56.315 / 7.268 / 63.583 |
| 256×4KiB | 12.137 / 8.606 / 20.743 | 26.258 / 19.929 / 46.187 | 12.982 / 12.839 / 25.820 |
| 2500×64B | 12.541 / 8.204 / 20.745 | 18.328 / 16.208 / 34.536 | 17.848 / 13.203 / 31.051 |

旧版逐文件LOCATE的256×4KiB总计569.988s，早期重复定位问题确实已消除。当前保留实时READ POSITION保障位置正确，微小文件及sync仍有性能差距，不能宣称已完全追平LE。未实测归因的候选不凭猜测删减命令。最终IP checkpoint47.758s，写流程峰值RSS22496KiB；实际38039次写传输均indirect，零actual direct。

## 验收状态

全量软件259 passed/7 ignored、fmt/check/Clippy -D warnings/兼容release通过；失败回归及实机失败原始日志单独保留。全16组写入完成，卸载重载Rust双SHA已通过（3145文件，90.440s/88.731s，mount112.011s）；LE双SHA也已通过（3145文件，首轮70.849s，缓存重读19.982s）；LE核验新UUID、正常卸载退出均完成。运行日志 `physical-io-final-resume2-run.log`，完成标志 `physical-io-final.complete`。

LE热缓存读将单列，不能作为磁带吞吐；RC18同盘重载不能代表RC17/RC18跨盘换带验收。

原生模式回读raw=`001e98100000000860000000000000000f0ec080000000ff000000ff00000000`，buffered=1、speed=0、DCE=true；procfs确实观测SG fd bufflen1048576。写入后卸载（含checkpoint）175.949s，同盘重载31.309s。

## 最终状态与边界

`physical-io-final.complete`已生成，`native-final-state.json`记录：RC18槽7、RC17槽8、drive空、LE不运行，PR generation18/注册表空/无持有者，allow_dio0。源码和测试提交344e9b1，未push。最终CLI及bench哈希见 `perf-final-format-position-hashes.json`。

连续12批：数据中位12.888s、普通sync中位13.373s、总计中位26.325s。剩余约5s/batch相对LE的差距仍待独立命令计时归因，不宣称已消除。LE首轮70.849s含自动挂载及真实读，第二轮19.982s SCSI计数完全不变，明确是缓存。Rust两次90.440/88.731s都是真实SCSI读，不能拿LE缓存速度冒充带速比较。

原生FORMAT参数与起点两个真实缺陷已经修复；第一次MODE SELECT失败、第二次FORMAT拒绝、第三次完整成功分别保留于physical-io-final-run.log、physical-io-final-resume-run.log、physical-io-final-resume2-run.log。失败尝试UUID未写入成功清单。最终benchmark仍使用原3145文件源及新UUID，旧清单/归档未覆盖。

本地归档的可解码文本 `.log` 仅清除行尾空白及多余末尾空行；完整原始输出仍保留于远端测试目录或本机 `/tmp/tape-rs-*.log`。二进制日志保持原样，诊断字段和测量数据未改动。
