# LTFS 回执承载与历史保留

日期：2026-09-14。范围：格式依据与设计取舍；不决定布局、编码协议或保留期限，不验证设备。

## 标准事实

- 文件与目录（含根目录）可有扩展属性；键唯一，遵循 LTFS 名称规则；值用 text 或 base64。大小写不敏感的 `ltfs` 前缀保留，厂商扩展使用 `ltfs.vendor.X.Y`。不能随意发明 `ltfs.receipt`。[§7.3–7.4、§9.2.7、§9.2.9–10、Annex C](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=35)
- 增量中变更 `extendedattributes` 必须记录完整集合，不是逐键 append；`extentinfo` 同理。完整索引描述当前卷状态，删除标记不进入完整索引。[§9.2.11、§9.2.13、Annex H.4](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=51)
- 禁止添加标准之外的索引 XML 标签；要求兼容保留未知标签不构成扩展许可。[§9.2.13](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=53)
- 普通文件以标准条目和 extent 描述。readonly 要求合规 writer 不修改；权限扩展属性的执行则是可选的。[§9.2.6、§9.2.8–9、Annex F.2](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=48)

## 对本项目的设计推论（不是标准要求）

| 承载候选 | 收益 | 代价与失效边界 |
|---|---|---|
| 业务文件 xattr | 当前对象和上传身份紧密关联，恢复索引即可查询 | 删除或覆盖对象后，当前命名空间不再承载旧回执；独立历史保留需要另一个承载位置 |
| 根目录或专用目录 xattr | 与业务文件寿命解耦，可随索引恢复 | 把全部历史集中到一个属性集合，会使每次新增回执重写整个集合；增量索引并不会消除此增长 |
| 卷内普通回执文件 | 历史证据可独立于业务文件保留；可设计不可变、分段文件 | 恢复索引只得到位置，读取内容仍可能需要磁带定位；单个无限追加文件也会使 extent 列表增长 |

应用回执若放在普通文件里，其文件格式可以是项目协议；这不同于修改 LTFS 索引 XML。外部 LTFS 客户端能够访问普通文件或属性，不代表能够理解上传状态、幂等键和历史保留规则。

未过期回执必须在项目认可的当前恢复状态中仍可达：可以是仍保留的属性，也可以是仍被索引引用的回执文件。不能只依赖旧索引中偶然残存的被删对象，再宣称完成了当前查询恢复。回执历史存在也不能证明原业务文件当前仍在。

专用目录以点开头只能作为命名和展示约定；readonly 是格式层协作约束，不能替代客户端身份授权、设备独占或防篡改措施。外部写入、回滚或重格式化超出受控协议时，应重新核实证据与覆盖范围；缺记录不直接等于从未提交。

## 尚待决策与验证

最终布局、schema/version、摘要、编码、分页/分段、保留期和外部写入兼容策略仍未裁定。需评估目录属性集合大小、回执文件读取定位次数、完整检查点体积；验证删除/覆盖后查询、检查点后历史可达、增量属性集合替换以及损坏或缺失证据的未知结果。不得将任何候选直接当成已实现协议。
