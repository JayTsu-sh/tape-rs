# 卷内布局与外来卷兼容：证据与候选

来源票据：[40 卷内布局与外来卷兼容](issues/40-volume-layout-interop.md)。证据三类，分开标注：**[指南]** IBM Spectrum Archive EE V1.3.2.2 指南；**[规范]** SNIA LTFS Format 2.5.1；**[实测]** 2026-09-17 在实验室 TAPERS 上对 EE 克隆带与参考实现 LinearTapeFileSystem/ltfs `v2.4.8.3-10521` 的双向校准（过程见 [实验室环境](lab-environment.md)）；**[源码]** 同一参考实现的源码。不核对 EE 闭源部分。

## 1. EE 在磁带上到底写了什么

更正：[EE 资源模型](ee-resource-model.md)原先写"`.LTFSEE_DATA` 的具体内容原文未给出"，这是漏读。指南 §10.2 给了完整布局，实测逐项吻合。

| 事实 | 出处 |
| --- | --- |
| 迁移数据存为卷根下 `.LTFSEE_DATA/` 里的文件，文件名是 `CLID-FSID-IGEN-INO`（集群 ID、文件系统 ID、inode 代数、inode 号） | [指南] §10.2 印刷页 326 / PDF 350；[实测] 名字形如 `3943857032761222777-8982614630178882119-924865612-200724-0`，比指南多一个 `-0` 后缀 |
| 每个数据文件在卷上按原 GPFS 路径建一个符号链接，用相对路径指向数据文件，这样挂载点变了链接仍有效 | [指南] 同上；[实测] `gpfs/ee-holo-validation/x.bin -> ../../.LTFSEE_DATA/<id>`，length 0，readonly=true，带 `ltfs.vendor.IBM.prefixLength=0` |
| 数据文件带扩展属性记录原 GPFS 路径，用于 export→import 重建命名空间，也用于文件召回成 resident 后清理陈旧链接 | [指南] §10.2 印刷页 326–327；[实测] `ibm.ltfsee.gpfs.path`，另有 `ibm.ltfsee.gpfs.objtype=regfile`、`ibm.ltfsee.objgen`、`ltfs.hash.md5sum` |
| 同一文件的多个副本在各盘带上用同一个 UID 文件名 | [指南] §10.2 图 10-2（file1 在 tapeA 与 tapeB 同名） |
| 未 reconcile 时，GPFS 里的删除和改名不反映到磁带；从磁带只能恢复"近似"命名空间，import 未 reconcile 的带可能出现多代文件或已删文件 | [指南] §10.2 印刷页 327；§7.23 印刷页 261 |
| 空文件、空目录、符号链接不占数据，用 `eeadm save` 单独存到带上 | [指南] §6.13 印刷页 186 / PDF 210 |
| 每次迁移批次写一次索引，creator 串带 "Sync via adminchannel"；IP 索引可落后 DP 一代 | [实测] DP gen 3 @35，IP gen 2 @5 |

**为什么 EE 要绕这一层**：GPFS 才是命名空间权威，文件身份是 inode。用 inode 命名让 GPFS 里的改名、移动不需要碰磁带（带上的链接会陈旧，由 reconcile 事后修）；也让多副本跨带同名、召回时按 inode 定位。代价是磁带单独拿出来时命名空间只是近似的，且依赖符号链接。

## 2. 规范已经标准化的互通约定

这些约定都放在索引的扩展属性里，任何合规实现都会原样保留。[源码] `xattr.c::_xattr_is_stored_vea` 显示参考实现只把 `ltfs.spannedFileOffset`、`ltfs.mediaPool.name`、`ltfs.permissions.*`、`ltfs.hash.*` 这几类 `ltfs.` 名字存进索引，其余 `ltfs.*` 是运行时虚拟属性。

| 约定 | 内容 | 出处 | 与本项目的关系 |
| --- | --- | --- | --- |
| 介质池成员 | 根目录 xattr `ltfs.mediaPool.name` 与 `ltfs.mediaPool.uuid`；名字可改，以 UUID 为准；名字还可写入 MAM 0808h | [规范] F.4 | 37 的"磁带自描述标记"候选原先打算自定义属性，应改用这两个标准键 |
| 内容哈希 | `ltfs.hash.{crc32sum,md5sum,sha1sum,sha256sum,sha512sum}`，同一文件可带多种；目录与符号链接不带 | [规范] F.3 | 已实现：默认 sha256sum，md5sum 可选；[实测] 与 IBM 双向互认 |
| 跨卷文件 | 段文件名加后缀 `-LTFSsegN`，各卷同路径，前后段用符号链接互指（目标 `/卷名/路径`），每段带 `ltfs.spannedFileOffset` | [规范] F.1 | 首版已裁定不自动跨卷续传；若以后做，应采用此约定而非自创 |
| 权限 | `ltfs.permissions.*` | [规范] F.2 | 首版文件契约不含权限模型，保留透传即可 |
| 卷锁 | `<volumelockstate>` 取值 `unlocked / locked / permlocked` | [源码] `xml_reader_libltfs.c:1628`；[实测] EE 带写 `unlocked` | 已实现：非 unlocked 一律只读 |
| 符号链接 | `<symlink>` 与 `<extentinfo>` 互斥；IBM 另加 `ltfs.vendor.IBM.prefixLength`（[源码] `LTFS_LIVELINK_EA_NAME`，挂载点前缀长度） | [规范] §9.2.9；[源码] `ltfs.h:172` | 已实现解析、跟随一层、原样回写 |
| 增量索引 | 2.5 新增 `<ltfsincrementalindex>`；一致卷必须在 IP 和 DP 末尾各有一份当前 Full Index；回指链只串 Full Index，旧实现会跳过增量索引 | [规范] Annex H.2 | tape-rs 只认 `<ltfsindex`。正常卸载的 2.5 卷可读；崩溃后 DP 末尾若是增量索引，会被判成 T1 未索引尾部而只读、视图停在上一份 Full。结论安全（不冒充最新、不放行写入），但读不到增量里的文件 |
| xattr 键名 | 索引里存不带 `user.` 前缀的名字，Linux 挂载时由实现加上 | [实测] tape-rs 写 `origin`，IBM 侧为 `user.origin`；EE 写 `ibm.ltfsee.*` | FUSE 适配层应做同样的前缀转换 |

## 3. tape-rs 写的卷要满足的格式要求

反向校准前 IBM 完全读不了 tape-rs 写的带。五条要求现在由 `on_tape_layout_matches_what_ibm_ltfs_requires` 钉住，细节见[实验室环境](lab-environment.md)的反向校准一节：9 位小数时间戳；两份卷标 formattime 相同；Index Construct 以文件标记开头（首个索引在块 5）；fileuid 不为 0；IP 索引回指同代 DP 索引。另有 VCI 里 VCR 为 8 字节。

## 4. 候选：外来卷的挂载分级

"外来卷"指不是本系统格式化、或被其他 LTFS 实现写过的卷。分级由挂载时的事实决定，不靠 creator 字符串猜测。

| 级别 | 判定条件（全部由 tape-rs 现有代码给出） | 允许的操作 |
| --- | --- | --- |
| 可读写 | D02 恢复放行追加；索引无未知元素；`volumelockstate` 缺省或为 unlocked；视图里没有任何 extent 位于 IP | 读、追加、提交。提交会整份重写索引，已知元素（xattr、符号链接、锁状态）原样保留 |
| 只读 | 上面任一条不满足，但存在可信视图 | 读、校验、目录重建；`restricted_reason` 说明原因 |
| 不可用 | 无可信视图（D02 的受限情形） | 仅诊断 |

可读写不等于"应该写"。把外来卷纳入本系统的池，是 37 的 `tape import` 运维动作，需要显式指定目标池；库存里出现未登记磁带仍按 37 的裁定标为 unassigned，不自动入池。

## 5. 候选：本系统自己的卷内布局

**推荐：直接路径布局，加身份属性。不采用 EE 的"数据对象 + 符号链接"。**

- 文件按应用给的逻辑路径直接存成普通 LTFS 文件。
- 每个文件带两个 xattr：稳定的文件身份（UUID，跨副本、跨改名不变）和版本号。键名用项目自己的前缀，不占用 `ltfs.` 保留名字空间；具体键名留给 07 的实施。
- 卷根带 `ltfs.mediaPool.uuid` 与 `ltfs.mediaPool.name`；是否同时写 MAM 0808h 留待实施核对。
- 内容哈希 `ltfs.hash.sha256sum`。

理由：

1. EE 需要那层间接，是因为 GPFS 是命名空间权威、身份是 inode。本项目已裁定不依赖 GPFS，[文件契约](file-contract.md)里命名空间的权威就是卷上已提交的索引，改名本来就要经卷提交发布。间接层解决的问题在这里不存在。
2. 直接路径的卷是完全自描述的：任何 LTFS 实现挂上去看到的就是应用的目录树，不需要本系统在场，也没有"未 reconcile 则近似"的问题。这比 EE 的离线可读性更强。
3. 目录重建（37 的能力、接管后的 RS02）只需要读索引，不需要解析链接再回查。
4. 副本：同一文件在不同卷上路径相同、身份属性相同，可由索引直接对上，不需要同名对象文件。

代价与边界：

- 改名、删除必须经卷提交才生效；对已离线或已导出的卷无法生效，需要 37 的 reconcile 在卷回来后补做。这与 EE 的限制是同一类，不是新问题。
- 同一路径的新版本替换旧版本（05 的 replace）在带上是"新 extent + 新索引"，旧数据成为可回收空间，与 EE 的 reclaim 语义一致。

**读 EE 写的带**：已具备只读能力（跟随链接、读 `ibm.ltfsee.gpfs.path`、用 `ltfs.hash.md5sum` 校验）。把 EE 的带导入本系统的池并继续写，会得到两种布局混在一卷的结果；首版不做，记为后续扩展：导入时只读登记，数据迁移到本系统格式的卷由 37 的 replace/reclaim 类任务完成。

## 6. 超过单卷容量的文件

首版已裁定不自动跨卷续传。补一条明确的失败语义：文件大小已知且超过目标池里任何单卷的可用容量时，在 admit 阶段以确定的"空间不足"拒绝，不进入结果未定。大小未知的流式上传沿用 04 的"保留可靠提交前缀"政策。以后若支持跨卷，采用规范 F.1 的约定。

## 候选验证 VL01—VL08

- VL01（已通过，实机）：tape-rs 写的卷，IBM LTFS 2.4.8.3 `ltfsck` 报 consistent，挂载读出内容哈希一致，接受 tape-rs 的 VCI 走快速挂载。
- VL02（已通过，实机）：IBM 写的卷（EE 克隆带），tape-rs 读出内容与索引 md5、GPFS 原文件三方一致，用 IBM 的 VCI 命中快速路径。
- VL03（已通过，实机）：两边交替追加同一卷，回指链连续，双方都判一致。
- VL04（已通过，实机 + 模拟器）：IBM 写的文本、二进制、目录 xattr 经 tape-rs 重写索引后原样保留。
- VL05（已通过，模拟器）：未知元素、锁定卷、IP 上有文件数据，三种情形分别只读并给出原因。
- VL06（未做）：2.5 增量索引卷。正常卸载的可读；崩溃后末尾为增量索引的判为只读且不冒充最新。需要一份 2.5 实现写的样本带，实验室的参考实现是 2.4，暂无来源。
- VL07（已通过，Holo，2026-09-17）：ltfsd 自动格式化并写入池标记的卷，经 IBM LTFS 2.4.8.3 挂载后 `getfattr user.ltfs.mediaPool.name` 与 `.uuid` 读到的值与池一致。
- VL08（未做）：admit 阶段对超过单卷容量的已知大小文件返回确定的空间不足。
