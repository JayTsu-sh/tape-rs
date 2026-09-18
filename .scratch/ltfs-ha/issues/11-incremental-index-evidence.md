# LTFS 数据分区增量索引与完整检查点约束

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-incremental-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 10

## Question

核实 LTFS 2.5.1 对数据分区增量索引的支持、完整索引与增量索引的布局及版本约束、恢复基线与链接规则、清洁卸载/一致卷条件，以及旧实现的互操作风险。明确标准增量索引与“追加一份完整索引”的区别，为可恢复后确认政策提供必要边界；不设计私有索引格式、不操作设备。

## Assets

- 研究分支：`research/ltfs-incremental-index`，worktree `/tmp/tape-rs-incremental-research-EeXGG3`。
- [研究报告](/tmp/tape-rs-incremental-research-EeXGG3/docs/research/ltfs-incremental-index.md)：该分支的 `docs/research/ltfs-incremental-index.md`，提交 `74f03eb`。worktree 移除后可用 `git show research/ltfs-incremental-index:docs/research/ltfs-incremental-index.md` 读取。

## Answer

LTFS 2.5.1 支持该方向。§9.2.4 限制增量索引只写数据分区；§9.2.13 定义其为相对前一完整或增量索引的变化，不是追加完整 XML。索引分区写完整索引。

恢复需要完整基线与其后必要的有序增量；完整索引回指针与增量回指针不能混用。清洁卸载要求以完整索引收束，数据分区末尾也必须为完整索引，不能仅补索引分区。具体条款、旧实现恢复风险和项目验证建议见报告。

周期完整检查点有助于限制恢复成本，标准提供的次数仅为建议示例，不据此设定项目默认值。本研究未验证硬件或当前代码，也未选择检查点阈值、回执格式或合批政策。
