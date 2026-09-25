# 故障恢复后 Full84 → LE 回写 → Client/FUSE 读回

2026-09-25，ltfs-recovery-ibm-interop。普通文件及属性往返通过；完整FUSE扫描发现既有符号链接读取缺口，未修复，不能将本轮写成全部兼容通过。

## 交接及 LE 实测

RT2502L8在前轮barrier/VCI中断恢复、继续提交、正常收尾后为DP/IP Full84（version2.5.0）。确认LE TS1000L8为SLOT/NEED_ASSIGN、驱动IBMlisa41D6C空闲、EE无任务后，备份并将卸载状态副本复制到既有LE测试带，没有创建/删除Holo介质或格式化。

LE实际assign/mount成功，读回原list中13个路径（包括跟随rc/link）的SHA256；故障测试文件的cut/set正确，删除的barrier/vci/delete/powercut属性没有复活。新增`rc/post-recovery-le-20260925.bin`，65566字节，SHA256=eebfc1dd2c3c1d6e1145006b70af850a01bfde380f16df2bc8137321c4d65f16；新文件属性le.binary为00 ff + LE-after-recovery、le.empty为空；在原`rc/xattr-fault-20260925`添加le.after-recovery属性。LE上pread及MAP_SHARED/PROT_READ mmap逐字节校验通过。

user.ltfs.sync后报告2.5.0→2.4.0。正常unmount/unassign后原始DP/IP XML均为2.4.0 gen85，creator为IBM LE。原26个文件的UID、长度、mtime、链接目标及extent字段与Full84一致，只新增上述文件；字段比较忽略XML子元素顺序，因为LE将extent内fileoffset移到首位。首次按元素顺序比较报错，确认仅排序差异后改按字段比较，没有修改介质。

## 独立 Client 与 FUSE 读回

正常停止原集群，TS1001L08归槽6。将卸载后的LE副本复制回RT2502L8；独立`/home/rocky/tape-rs-recovery-le-20260925/test-data`和7500/7501，仅归属此带。沿用正式ltfsd f8c238…与tape-fuse fac4a6…，未修改生产代码。

node1/round261从磁带实际恢复gen85 Complete，将旧目录83对账到85。Client example验证stat/stat_path、get/get_to、普通/跨EOF/空range、416、list/list_dir、二进制/空属性及原故障文件内容。LE新增文件version=(0,0)、sha256字段为空，是LE未写这些tape-rs属性的事实；验证用独立预期字节，不把空哈希当校验成功。

FUSE使用全新缓存和实际内核挂载，读回12个原普通文件及新文件，校验属性、mtime纳秒（新文件1790295955085558477、原故障文件1790293009141803429）、pread及只读共享mmap。全程只读，正常停止后原始DP/IP仍为LE的2.4.0 Full85，没有先由tape-rs重写索引掩盖互操作问题。

## 未通过：既有符号链接 rc/link

严格FUSE探针读取`rc/link`失败，连续3次最小重现均errno11/EAGAIN。带上XML为`link → keep.bin`、length0；HTTP stat也是length0且没有链接类型/目标，HTTP GET跟随链接返回65536字节。直接FUSE读取keep.bin成功，排除目标数据损坏或设备忙。

根因在已有接口缺口：`daemon::files::catalog_of`没有把symlink目标传入元数据，FUSE只建普通文件/目录；`TapeFs::open_inner`下载后将实际65536与stat的0比较，按现有一致性防护返回EAGAIN。不能取消长度校验规避，否则会破坏并发替换防护。现有docs/file-client.md已将链接列为未支持能力，但本轮第一次把这条具体LE遗留链接的读失败明确记录下来。

`probes/recovery-le.py fuse`保留严格失败；显式`fuse-regular`模式单独报告该GAP，仅把普通文件子集列为通过，没有静默忽略失败。使用diagnosing-bugs流程完成了最小重现和定位，本轮没有进行符号链接功能实施。下一步应补全链接类型/目标的catalog→Client→FUSE传递及readlink，再用同一Full85和全新缓存重跑严格探针；链接创建、写入、rename/unlink语义需分别验证。

## 恢复与材料

RT2502L8归槽7、LE TS1000L8归槽且unassigned，双方保留Full85及本轮新增测试文件/属性。Holo备份`/home/rocky/holo-recovery-le-20260925/{native-full,le-before,le-result,rt-before-return,rt-after}`，原Full84和旧LE58仍可审阅，不能未经核对回滚正在使用的副本。

FUSE与测试节点停止；卸载工具为fusermount，初次尝试fusermount3不存在后改用已安装工具，实际卸载成功。原集群恢复node1/term34/round276，TS1001L08仍gen72 Complete、TS1000L08仍gen2；3节点正式程序哈希、catalog31条及哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0不变，原9文件长度/SHA与完整list一致。EE3节点available、无任务，f3哈希cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e不变，9publications ready。

新增`examples/hw_recovery_le.rs`与`probes/recovery-le.py`，均有显式目标门控。example兼容构建、cargo check/clippy --example hw_recovery_le、example rustfmt、Python语法、git diff --check完成；clippy库既有告警。无生产代码改动，未重跑上一轮202项软件全套或全仓fmt/check/clippy。无CodeGraph目录。全部工作树改动未提交。

证据`probes/results/recovery-le-*`保存LE写入与卸载、原始三个XML、字段比较、Client结果、严格FUSE失败及普通文件子集成功、节点日志、原环境复核。本轮无VM断电、Holo重启或物理TS4300测试；交接为Full，不证明LE能直接重放2.5增量链。
