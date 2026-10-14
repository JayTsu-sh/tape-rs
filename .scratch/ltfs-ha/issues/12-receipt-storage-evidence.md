# LTFS 回执承载与历史保留的格式依据

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: codex-receipt-research
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 11

## Question

依据 LTFS 2.5.1 官方格式，比较文件/目录扩展属性和标准卷内文件承载应用回执的约束。核实扩展属性命名空间、编码、增量索引中属性集合更新规则、自定义 XML 标签限制，以及删除/覆盖业务文件和生成完整检查点后如何保留历史证据。区分标准文件互操作与应用回执协议互操作，不将系统目录隐藏或只读标记当作跨客户端安全保护，不替用户决定方案。

## Assets

- 分支 `research/ltfs-receipt-storage`，worktree `/tmp/tape-rs-receipt-research-fFWbcY`。
- [研究报告](../../../docs/research/ltfs-receipt-storage.md)：该分支的 `docs/research/ltfs-receipt-storage.md`，提交 `e81074277ee3c830ff39770a7ec26f3b5b4f7ca2`。worktree 移除后可用 `git show research/ltfs-receipt-storage:docs/research/ltfs-receipt-storage.md` 读取。

## Answer

文档研究完成。LTFS 文件/目录扩展属性与普通文件均可承载应用信息，具体条款与三种方案对比见报告；未选择产品方案、访问设备或修改业务源码。

- 增量索引中若变更 extendedattributes，必须写完整属性集合；将全部回执集中在一个目录属性集合会产生重复写入。extentinfo 亦须完整记录，单个无限追加回执文件也有 extent 列表增长代价。
- 标准禁止自行增加索引 XML 标签；应用回执文件内容可以定义项目协议，这不等于修改 LTFS 格式。属性名称须遵守保留命名空间规则。
- 单靠业务文件属性不能保证业务文件删除/覆盖后的历史查询；保留期内的回执须在认可的当前恢复状态中持续可达，不能只依赖旧索引的残存条目。
- 普通回执文件使证据与业务文件寿命分离，但恢复索引后仍可能需要额外定位读取。外部客户端能读取文件不等于理解回执协议。
- readonly 对合规 writer 有实际约束，但不能取代身份授权、设备独占或防篡改；隐藏目录不是安全边界。缺失或受外部修改影响的证据不得直接解释为从未提交。

回执布局、记录格式与保留策略仍由[提交契约](04-durability.md)裁定；性能与故障行为未验证。
