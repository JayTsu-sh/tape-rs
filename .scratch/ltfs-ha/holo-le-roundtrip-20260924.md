# Holo LE → tape-fuse → LE 往返验证

2026-09-24，分支 `ltfs-recovery-ibm-interop`。结论：**内容与 Full 索引交接通过，但被 FUSE 修改的文件丢失原 xattr，整体兼容验收失败。** 本轮为实测与定位，没有修改生产逻辑；下一优先事项是修复覆盖写元数据继承。

## 实测路径

1. 原 ltfsd 三节点正常停止。原 TS1001L08 保持 gen39；另取已关闭、已格式化的 TS1000L08 副本为 LISA4300/TS1000L8，原卷不写入。副本起点实际 XML 为2.4.0 gen2，池 UUID为既有rc，卷UUID `26aefb84-fbc8-4c4a-a500-c85264ba2d29`。
2. 实际 IBM LE 2.4.8.3 (10521) 挂载副本，创建 rc/keep.bin、rc/edit.bin（各65536字节），设置二进制 `user.roundtrip.binary=00ff4c452d726f756e6474726970`、mtime=1790200000123456789；另建空目录 rc/empty、相对符号链接 rc/link→keep.bin。同步并正常卸载，LE写出2.4.0 gen3。
3. 将关闭副本复制到 TAPERS/RT2502L8，在独立三节点catalog/Raft目录接纳。原池UUID通过上轮的一次性测试入口登记，生产HTTP/FUSE实现不变。
4. 全新 tape-fuse 挂载和缓存：先确认 edit.bin 原内容和xattr；O_RDWR偏移4093写16字节 `FUSE-ROUNDTRIP!!`，扩大truncate到70000后fsync，再追加12字节并fsync；另创建26字节new.bin并fsync。对应gen4–6标准增量。
5. FUSE正常卸载，测试服务SIGTERM，生成DP/IP 2.5.0 Full gen7并UNLOAD、释放PR。
6. 关闭副本回拷到未挂载/已unassign的TS1000L8，LE重新assign -r并挂载，实际报告indexVersion2.5.0；用独立期望内容、xattr、mtime、目录和链接逐项核对。

上一轮TS1001L8/TS1001L7条码已经被Holo退役，不能重建。尝试TS1001M8被Holo命名校验400拒绝，没有创建；本轮改用TS1000L8，未改写ANSI标签，也没有格式化/擦除任何介质。

## 结果

| 对象 | LE 回读结果 |
|---|---|
| keep.bin | 内容、二进制xattr、原mtime均保留 |
| edit.bin | 70012字节内容正确，mtime与FUSE发布的一致；**原二进制xattr丢失** |
| new.bin | 26字节内容正确，mtime与FUSE发布的一致 |
| rc/empty | 保留，仍为空目录 |
| rc/link | 保留，目标仍为keep.bin |
| DP/IP索引 | 2.5.0 Full gen7，LE成功读取 |

SHA-256：
- keep.bin：`7daca2095d0438260fa849183dfc67faa459fdf4936e1bc91eec6b281b27e4c2`
- edit.bin：`3c2f1b0ed0090d476885163291f3fddecd4ac4d3d73453175c960e160a7015b2`
- new.bin：`75ad67ecf8c747a39fa87f1c2449a2cd06c2af93248e40b02f56805491560096`

第一版验证脚本用15字节切片替换16字节标记，导致期望数据被插入1字节，首次内容断言失败；修正为按标记实际长度构造期望后，三个文件均逐字节相等。LE读取SHA与FUSE读取SHA及索引内容摘要一致，未发现内容损坏。最终脚本仍因真实xattr丢失返回失败，此失败不可忽略。

## 缺陷定位

- `src/tapefs.rs` 的 `ClientBackend::upload` 调用 `Client::put_file`，只提交内容。
- HTTP PUT使用 `FileService::finish`，将 `metadata=None` 入队。
- executor普通上传走 `LtfsVolume::append_file_with_xattrs`，只附新的版本号。
- `append_file_inner` 删除同名旧条目并创建新节点；没有preserved metadata时，xattrs从空列表构建，然后只添加版本号/内容哈希。
- 因此被覆盖文件的原自定义xattr丢失。带上增量与Full XML都没有该属性，LE亦返回缺失；不是FUSE缓存或LE解析问题。未修改文件保留原属性。

修复应明确区分文件系统覆盖与普通对象替换：FUSE覆盖必须继承原属性，更新时间和内容哈希；不能照搬旧摘要，也不能仅依赖当前写带上的同名条目（跨带覆盖仍需保留）。应覆盖普通、二进制xattr、连续fsync、跨带覆盖、并发版本变化及哈希重算，再用本轮保留副本复测。此报告不宣称上述修复已经完成。

## 恢复及保留材料

- FUSE已卸载，隔离测试服务已正常停止。TS1000L8已由LE正常卸载、unassign并回LISA4300槽1032；RT2502L8已由第二驱动回TAPERS槽1030（逻辑槽7）。**两盘测试副本保留供修复复测，不属于原生产池分配。** 原TS1000L08/TS1001L08未修改。
- LE写入后的原始gen3副本另备份在Holo `/home/rocky/holo-powercut-20260924/roundtrip-le-before`。RT2502L8保留native gen7结果；TS1000L8在LE验收后已正常卸载，可能有LE收尾更新，重新使用前核对索引。
- Holo创建介质导致EE2/EE3手工changer publication失效，已按原参数恢复并配置ACL，最终9个ready。
- 原ltfsd三节点恢复：node3 leader、term10、round112，TS1001L08仍gen39/Complete。原探针SHA仍 `49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7`。
- EE三节点available，无活动任务，原f3 SHA仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`。

探针：`probes/roundtrip-le.py`、`probes/roundtrip-fuse.py`。它们只供上述专用副本使用，prepare和write会修改内容，不能不核对状态直接重复运行。
日志：`probes/results/roundtrip-{le-prepare,fuse-write,native-index,le-verify,daemon,cluster-restored,ee-restored}.log`。
本轮只新增实测脚本/日志/文档，Python语法与diff-check通过，未重跑Rust测试；上轮172通过的结果不能覆盖这个新发现的现场缺陷。未执行本轮断电，未触及物理磁带。

## 2026-09-24 修复与复验：通过

已修复 `FileService::finish`：路径已被上传预留后，从当前目录版本继承原xattrs（含Base64标记）；通过已有逻辑属性传递路径落带，新时间、新版本、UID及内容摘要仍重新生成。读取的是池内当前版本，因此跨带覆盖也保留属性。普通Client覆盖与FUSE覆盖一致；回收的原时间保留逻辑不变。目录不可读/查询失败不再当作文件不存在；已有记录缺少或损坏xattrs时拒绝覆盖并释放预留，避免有损写入。旧catalog中metadata未知的记录必须先从带上对账后才能覆盖，这是一项明确的兼容性限制。没有新增接口、依赖或持久化字段。

软件回归先复现原缺陷（`xattr-red.log`），修复后覆盖同带/跨带、文本/二进制xattr、旧MD5/SHA256重算、修改时间更新、路径预留冲突；另测目录不可读拒绝上传、清理spool并释放预留。最终 `cargo test` 174通过、1忽略，`cargo check --all-targets`、`cargo clippy --all-targets --all-features` 成功（既有告警）。全局fmt仍有既有差异，只格式化本轮新增/修改代码段及新测试文件；diff-check通过。

现场使用原LE gen3备份重新初始化**关闭的RT2502L8副本**，独立全新catalog/缓存，修复后的测试服务重新执行同一随机写、扩展truncate、追加、新建及fsync。正常停机生成DP/IP 2.5.0 Full gen7，再交给实际LE读取：keep/edit/new内容、原keep时间、修改文件时间、空目录/符号链接均正确；**edit.bin的 `roundtrip.binary` 属性在DP/IP XML及LE中全部保留**。已用独立哈希核对内容。证据 `probes/results/xattr-fixed-{fuse,index,le,daemon}.log`。

测试过程中一次初始池创建命令发给了Follower，被明确拒绝；改为各测试节点在启动后提交一次，只有Leader成功创建池。第一次FUSE访问发生于磁带目录对账之前，返回ENOENT；确认gen3目录就绪后重跑。两者均发生在写入之前，不计作修复验证通过。

恢复完成：隔离服务已停止、FUSE已卸载、两盘副本均回槽，仍保留供复测。原ltfsd恢复node3 leader/term11/round115，TS1001L08仍gen39；EE三节点available无活动任务，原探针和f3哈希不变。日志 `xattr-restored-{cluster,ee}.log`。本轮没有创建/删除介质、格式化或断电。原三节点生产二进制未替换，修复版只部署于隔离测试服务；后续升级原集群需整组一致更新。
