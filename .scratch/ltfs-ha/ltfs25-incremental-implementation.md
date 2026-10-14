# LTFS 2.5 增量索引实施核对

日期：2026-09-24。仅核对规范与公开源码，不访问设备。用户要求的元数据持久化是标准 LTFS 增量索引，不能用仅存 SQLite、私有 sidecar 或重复追加 Full 代替。

主要依据：[SNIA LTFS Format 2.5.1（2020-08-18）](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)。以下页码为 PDF 印刷页，恰与 PDF 页码一致。已有决策见 `docs/research/ltfs-incremental-index.md`、`recovery-evidence.md`、`issues/11-incremental-index-evidence.md`。Annex B 是规范性 XSD；Annex H 是资料性处理说明，不把示意流程当完整异常介质恢复算法。

## 编码入口与公开样本

规范 §9.2.3（44 页）明确本版描述的 Index XML 格式版本仍为 **2.5.0**。根为 `ltfsincrementalindex`，不是在 `ltfsindex` 上加自定义 incremental 属性。Full 仍为 `ltfsindex`。新增格式标签不能自行发明，应用元数据应走标准 xattr。

公开 2.5 增量 XSD 就在 [Annex B.2，68–71 页](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=68)；官方 XML 样本在 §9.2.2（44 页）、§9.2.12（52–53 页）、H.4（100–101 页）。本次检索未找到可信、独立发布的官方 `.xsd` 下载文件，不声称不存在。公开 [LinearTapeFileSystem/ltfs README](https://github.com/LinearTapeFileSystem/ltfs/blob/main/README.md) 明确目标仍是格式 2.4，不能拿其 parser 当 2.5 参考实现。规范第 2 页对代码片段提供 BSD 3-Clause 许可；将 XSD 提取进仓库时须保留许可。下方 XML 是本项目自构造测试输入，不是官方样本原文。

## Parser / writer 数据模型

[§9.2.3–9.2.4 与 B.2](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=44) 对应的字段如下；未特别标可选者必需：

| 对象 | 元素及条件 |
| --- | --- |
| 增量根 | version 属性；creator、volumeuuid、generationnumber、updatetime、location、previousgenerationlocation、highestfileuid、一个 directory |
| 根可选 | comment、volumelockstate、previousincrementallocation（是否必需取决于真实前驱） |
| 根禁止 | allowpolicyupdate、dataplacementpolicy；两者只属于 Full |
| 位置 | partition + startblock；partition 是卷标映射的逻辑 ID，不固定认为 b 对应物理分区 1 |
| 删除项 | file 或 directory 内**只有** name 与空 deleted；`<deleted/>` 或 `<deleted></deleted>`，不是 `<deleted>true</deleted>` |
| 导航目录 | 只有 name、contents，无 fileuid；它不改变目录属性 |
| 修改目录 | name、fileuid、contents，加所有变化的字段；未变字段可带可不带 |
| 修改文件 | name、fileuid，加所有变化字段；不是缺省值覆盖现有属性 |
| 新建文件/目录 | 满足 Full 对该对象的全部字段规则；不能把不完整 patch 当创建 |

目录/文件共有 name、fileuid、readonly、creationtime、changetime、modifytime、accesstime、backuptime；目录有 contents，文件有 length。正文对 accesstime/backuptime 使用 may，但 B.1 Full XSD 把二者列为必需；writer 全部写出可避免歧义。不要仅依赖 B.2 验证：它将很多字段标可选，**不能表达删除/导航/新建/修改互斥约束**，必须补语义校验。[§9.2.6–9.2.11、B.2 前言](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=48)

实现建议：增量节点显式保留 `Option<T>` 和删除/导航状态，避免将“不出现”“空列表”“false/0”混成一个状态。字段 `extentinfo`、`extendedattributes` 若出现就是**完整替换集合**，不是 append/逐项 merge；清空使用空元素。symlink 与 extentinfo 互斥。Full 不接受 deleted。

## 可执行测试样例

假定 Full generation 3 在 DP b:100，已有 `/note` UID 7、`/gone` UID 8；增量 generation 5 在 b:200（合法跳代），改变 note 的 xattr 并删除 gone。下面全部 XML 可直接作为 parser 输入；它没有新建对象，因此无需完整文件元数据。父目录的真实时间变化应由调用层填入，本样例只演示格式和重放，不模拟真实 unlink 的全部时间更新。

```xml
<?xml version="1.0" encoding="UTF-8"?>
<ltfsincrementalindex version="2.5.0">
  <creator>tape-rs 0.1 - Linux - test</creator>
  <volumeuuid>c065209a-0388-4259-93c1-482564d41b12</volumeuuid>
  <generationnumber>5</generationnumber>
  <updatetime>2026-09-24T01:00:00.000000000Z</updatetime>
  <location><partition>b</partition><startblock>200</startblock></location>
  <previousgenerationlocation><partition>b</partition><startblock>100</startblock></previousgenerationlocation>
  <highestfileuid>8</highestfileuid>
  <directory>
    <name>test-volume</name>
    <contents>
      <file>
        <name>note</name><fileuid>7</fileuid>
        <changetime>2026-09-24T01:00:00.000000000Z</changetime>
        <extendedattributes><xattr><key>user.test</key><value type="base64">AP8=</value></xattr></extendedattributes>
      </file>
      <file><name>gone</name><deleted/></file>
    </contents>
  </directory>
</ltfsincrementalindex>
```

本例最高已分配 UID 保持 8，不因删除而回收；下一代创建从 9 分配是项目简单策略。零 highestfileuid 表示连续 UID 空间耗尽，不能按普通计数器加一溢出。根 UID 固定 1，普通文件 UID 至少 2，整个恢复后命名空间 UID 唯一。[§9.2.3、9.2.6、9.2.8](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=45)

## Diff 和 replay 的实现约束

[§9.2.11 与 H.4–H.5、图 H.1](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=51) 给出的语义映射：

| 输入与原视图 | 操作 |
| --- | --- |
| deleted，原路径不存在 | no-op 合法；同代创建又删除可能产生此项 |
| deleted，原路径存在 | 删除；目录同时移除子树 |
| name+contents 导航目录 | 进入已有目录，不更新属性 |
| 相同路径且相同 fileuid | 更新出现的字段，保留其他字段；递归 contents |
| 相同路径但不同 fileuid | 替换为完整新对象，不能继承旧 metadata/子树 |
| 原路径不存在 | 必须是完整新对象；缺必要字段报错 |
| rename/move 文件 | 旧名 deleted，新名完整插入，保留原 UID |
| rename/move 目录 | 旧目录 deleted，新目录完整插入**所有深度的子树**，UID 保留 |
| 删除后同名重建 | 只输出新 UID 的完整项，不同时输出同名 deleted |

实现建议：对比最近已提交快照生成 delta，按命名空间而非“操作日志条数”归并；同一路径不得重复。先在隔离候选树重放完整一代，再校验全树 UID/路径唯一后发布。否则合法 rename 的“新路径插入先于旧路径删除”会被中途 UID 检查误拒绝。应覆盖跨目录交换/覆盖 rename，以及父目录删除后移出的子节点。无 fileuid 导航到不存在目录不能凭空创建；它不具备完整创建字段，这是本项目保守拒绝策略。

## 指针、generation 与完整检查点

[§5.4、9.2.4、H.3](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=26)：

- `previousgenerationlocation` **总是 Full 指针**。DP 上指前 Full；增量必须有此基线。
- 自前 Full 后存在增量时，当前索引须带 `previousincrementallocation`，指最近增量；**当前索引可以是 Full**。没有此种增量则省略，不能误连上一个 Full 之前的增量。
- 沿完整历史链回溯，优先走增量指针，否则走 Full 指针；恢复目标增量时从相应 Full 正序应用所有依赖增量。
- generation 同分区按位置非递减；允许跳号。相同 generation 要满足规范限定的等价内容关系，不能只比数字。自指针必须匹配卷 UUID、实际分区与起始块；索引样式的普通用户数据不能误认成索引。

自构造链例：Full F1(b:100,g=1) → Delta D2(b:200,g=2,full=100) → Delta D4(b:300,g=4,full=100,incr=200) → Full F5(b:400,g=5,full=100,incr=300) → IP Full I5(a:20,g=5,full=b:400)。F5 后第一份增量只回指 F5，没有 incr=300；但 F5 自己保留 incr=300 供历史遍历。位置仅作模型值，真实 Index Construct 还需满足块与文件标记布局。

IP 只写 Full；清洁卸载时 DP 末尾也必须是 Full，并让最终 IP Full 回指最终 DP Full，两分区完整。不能“每次 DP 增量、卸载只补 IP”。Index Construct 是前后文件标记包围 XML；XML 后不能夹带私有字节。[§4.1.4、5.2.3、9.2、H.1–H.2](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=42)

周期 Full 的 5–10 是建议示例，不是固定强制值；现有提交路径若仍每批生成 Full，只是 Full checkpoint，并未实现用户要求的增量持久化。清洁卸载 Full 与日常 delta 的 durability barrier、元数据原子提交、重启重放必须分别验收。

## VCI 与故障处理边界

[§10.1–10.3](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=57) 的 MAM/VCI 是快速定位优化，不是标准增量格式的强制依赖。所有分区 VCI 的有效 VCR 都匹配当前 cartridge VCR 时才能使用全卷快速路径；全零/全一 VCR 不可信。一分区过期则需索引核验，不能把另一份 VCI 当全卷一致证据。推荐顺序是完整写 Index Construct、flush 所有 pending write、立即读 VCR、为所有 complete 分区更新 VCI。不能先更新 VCI 再据此 ACK 数据。

项目已定恢复策略继续适用：缺 Full、缺中间增量、环/反向指针、跨卷/错误位置、非法 UID、部分 XML、无法排除更晚提交时，拒绝发布“完整最新”视图；不能静默丢弃 delta 回退 Full 并声称成功。合法 generation 跳号不是缺链证据。Full 恢复当前快照不依赖所有历史 delta 能否重放，但不能因此证明它后面的尾部安全。候选发现覆盖、索引依赖验证、追加安全各自成立；不自动修复或写文件标记。

## 最低测试矩阵与 LE 验收边界

实施建议（非标准额外条款）：

1. parser：官方形式空增量、导航祖先、metadata patch、二进制/清空 xattr、extent 完整替换；拒绝 deleted=true、deleted+fileuid、重复字段/路径、IP 增量、policy 增量。
2. replay：创建/修改/删除、缺对象删除、同名新 UID 替换、文件/整树 rename、交换、稀疏 extent、symlink、零文件、readonly/times；增量恢复树等于原最终 Full。
3. 链：Full→多 delta→Full，合法跳代、截断 XML、缺依赖、循环、自指位置错、Full 同时带两类回指；重启不得从 SQLite 伪造成功。
4. Holo：元数据变更落 DP delta、重新启动从带重放、只读读回；再清洁 checkpoint、用 LE 挂载核对 Full。

**现场 LE 2.4.8 的成功挂载只能证明清洁 Full checkpoint 的互操作，不能证明 LE 能读取/恢复 LTFS 2.5 增量。** 异常尾部应由本项目 2.5 reader 验证，不能用旧版 LE 的 rollback/ltfsck 作为增量恢复 oracle。LE 的 mmap/POSIX 行为研究与磁带格式 2.5 的符合性是两条独立证据链。
