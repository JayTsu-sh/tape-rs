# LTFS 数据分区增量索引核实

日期：2026-09-14。仅核实格式规范，未检查业务实现或访问设备。

## 规范结论

- **支持标准增量，不等于追加完整 XML。** 增量记录相对上一份索引（完整或增量）的变化。v2.5.0 起要求实现能识别、处理增量，但不强制写增量。[§9.2.13，印刷页 53](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=53)
- **增量只在 DP，IP 只写完整索引。** `previousgenerationlocation` 指向前一个完整索引；`previousincrementallocation` 指向最近完整索引之后的前一份增量。有该前驱时须记录增量指针，不能将完整索引指针改成通用前驱指针。[§5.4.3、§9.2.4，页 27、45–46](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=45)
- **卸载不能仅补 IP。** 卸载处理必须以完整索引收束增量；一致卷的两分区完整，IP 最后索引回指 DP 最后完整索引。[§4.1.4、§5.4.1、§9.2.13；资料性 Annex H.1–H.2 明确 DP 末尾亦为完整索引](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=98)
- **恢复需要完整基线加有序增量链。** 旧实现可能跳过增量而恢复失败或丢失最新状态；清洁一致卷的兼容性不能证明异常卷的恢复兼容性。[§9.2.13；资料性 Annex H.2–H.5，页 98–102](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=99)
- **周期完整索引是建议，数字不是强制。** 标准建议可设增量间隔及上限，5–10 只是示例，不构成本项目默认值。[§9.2.13，页 53](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=53)

## 对当前地图的设计推论（不是标准额外要求）

日常提交可以选择 DP 标准增量，保留已接受的“可恢复后确认”政策；但确认条件必须覆盖可靠落带的数据、回执及所需完整基线和增量链，不能只看到最新 XML 写成功就 ACK。具体设备持久性屏障仍由独立提交协议研究及断电测试验证，本报告不替代它们。

“索引分区稍后补齐”应拆成两个事件：DP 完整检查点限制重放成本；清洁卸载/导出时完成 DP/IP 一致卷收束。检查点频率、时间或字节上限均待决策。B 与恢复工具必须支持所用增量格式；不允许把旧工具跳回完整索引后的结果误报为“所有已确认上传均已恢复”。

建议验收覆盖：基线加多份增量重放、增量尾部截断、链缺失、响应丢失、完整检查点中断、清洁卸载再挂载；并分别检验当前文件状态和历史回执证据。这些是项目测试建议，不是声称标准定义了上传回执或应用事务。
