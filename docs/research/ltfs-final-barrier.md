# LTFS 索引结束标记与最终持久化屏障

日期：2026-09-14。范围：提交状态机 S3→S4；仅规范研究，未审计当前代码、未访问设备。

## 结论

**结束标记兼任最终屏障是有条件候选，不能仅凭这两份资料就宣布已证明可省略 count=0。** IBM 对 count=0 明确给出全部缓冲数据及文件标记的介质同步保证；对 IMMED=0 的一般说明是等待命令完成，未在该段单独展开 count=1 对此前全部缓冲的覆盖语义。这个证据缺口不是“count=1 不会同步”的反证，但目前不足以把减少命令列为已验证优化。[IBM §5.2.46，印刷页 181 / PDF 页 210](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=210)

## 已核实事实

- LTFS 索引构造为文件标记、索引 XML 块、文件标记。格式要求不等于指定一套 SCSI 同步命令。支持 ltfs.sync 时，sync 须留下足以无损恢复的一致状态所需介质数据，但不要求当时两分区已一致。[SNIA LTFS 2.5.1 §5.2.3，页 25](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=25)、[Annex C.4，页 74](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=74)
- IBM 的 IMMED=0 等待完成，IMMED=1 只等待验证命令；IMMED=0、count=0 可同步全部缓冲数据及文件标记。空缓冲 count=0 甚至可在某些不可写或被篡改的 WORM 情形返回 GOOD，故其 GOOD 不证明历史提交或可追加性。[IBM §5.2.46，页 181](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=210)
- 缓冲写先前的 GOOD 仍可产生延迟错误。WRITE FILEMARKS 同时支持延迟错误报告及错误归属转移；须处理 I_T nexus 关联和所有待报告错误。设备使用 autosense，不要求每次成功后再固定发送 REQUEST SENSE。[IBM §4.19.2–3，页 65–66](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=94)、[表 31，页 78](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf#page=107)

## 项目推论与候选路径（尚未实现或验证）

| 条件 | S3→S4 的处理 |
|---|---|
| 已取得适用设备/协议证据，证明有序执行的非立即结束标记覆盖此前数据、整个索引及结束标记；完整结果可信 | 可让该 count=1 同时承担最终屏障，不因逻辑上分成两个状态就强制增加 count=0 |
| 结束标记用了 IMMED=1，或当前证据不能证明其完整持久化覆盖 | 保留显式 IMMED=0、count=0 路径；开始该命令前仍须满足所有权、命令顺序与设备安全条件 |
| 屏障后又写入新的数据或索引，并要确认这些新变化 | 需要覆盖新变化的屏障；不必机械限定为 count=0，也不能复用旧屏障 |
| 短写、结束标记缺失、延迟错误、超时/响应丢失、重置或位置不明 | 不把追加一次 count=0 当作修复或成功证明；转入既定错误分类与介质恢复 |

无论哪条路径，都必须保留完整传输结果、SCSI/host/driver 状态、原始 sense 与延迟错误上下文的检查；“某条命令 GOOD”不能替代本批数据完整性、索引合法性、覆盖关系及可恢复性的证据。count=0 不产生缺失的结束标记，也不补齐缺失数据。B 新建 nexus 后的空同步不证明 A 的历史上传成功。本研究不新增回执或持久化请求历史。

## 尚缺证据与验证边界

省略 count=0 前，补齐适用 SSC 条款或 IBM 明确语义，确认非立即 count=1 的完整缓冲覆盖；本次未取得 SSC 正文，不能声称完成该条款核验。现场仍须固定固件、缓冲模式、加密、HBA 与传输路径，验证命令顺序、完整状态处理，以及结束标记前后断电、响应丢失和跨节点介质恢复。纯软件模型不能证明真实落带，额外 count=0 也不替代这些验证。

来源均为官方原文。IBM 网页抓取后续检索超时，已从同一官方 URL 下载 PDF 并提取文本逐段核对；SNIA 在线正文已核对。仅新增此研究文档，未知设备参数和性能收益不填默认值。
