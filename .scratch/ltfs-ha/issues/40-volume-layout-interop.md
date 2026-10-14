# 卷内布局与外来卷兼容

Type: research
Labels: wayfinder:research
Status: resolved
Assignee: claude
Parent: [高可用双接口 LTFS 客户端决策地图](../map.md)
Blocked by: 14, 37

## Question

EE 在磁带上用什么布局存文件，为什么？LTFS 规范已经标准化了哪些跨实现约定（池成员、哈希、跨卷文件、卷锁、增量索引）？本系统格式化的卷要满足什么才能被 IBM LTFS 读写，反过来对其他实现写过的卷应开放到什么程度？本系统自己的卷内布局应沿用 EE 的"数据对象 + 符号链接"，还是直接按路径存？

## Scope

证据限于 EE 指南 §6.13、§7.23、§10.2，LTFS Format 2.5.1 的 §5.2.3、§9.2、Annex F/H，参考实现 LinearTapeFileSystem/ltfs `v2.4.8.3-10521` 的源码，以及 2026-09-17 在实验室 TAPERS 上的双向校准。不核对 EE 闭源部分，不在 EE 集群的带上做任何写入。不裁定 xattr 的具体键名、`tape import` 的命令形态和 FUSE 适配细节。

## Answer

研究完成，形成[卷内布局与外来卷兼容：证据与候选](../volume-layout-interop.md)。

### 事实

- EE 的布局在指南 §10.2 有完整说明，实测逐项吻合：数据在 `.LTFSEE_DATA/CLID-FSID-IGEN-INO`，原 GPFS 路径上是相对符号链接，数据文件的 xattr 记原路径。原因是 GPFS 才是命名空间权威、身份是 inode。[EE 资源模型](../ee-resource-model.md)原先写"具体内容原文未给出"，已更正。
- 规范 Annex F 已标准化池成员（`ltfs.mediaPool.name/uuid`）、内容哈希（`ltfs.hash.*`）、跨卷文件（`-LTFSsegN` + 符号链接 + `ltfs.spannedFileOffset`）、权限。参考实现只把这几类 `ltfs.` 名字存进索引。
- 反向校准前 IBM LTFS 完全读不了 tape-rs 写的卷；五条格式要求与 VCI 的 8 字节 VCR 已修并有回归用例。现在两边可交替读写同一卷并互认 VCI。
- 2.5 的增量索引对只认 Full Index 的实现是向后兼容的：一致卷两端必有 Full Index。崩溃后的卷在 tape-rs 上会得到安全但保守的结论。

### 推荐（按自动接受授权记为已接受方向，用户可推翻）

1. **外来卷挂载分三级**：可读写 / 只读 / 不可用，由挂载时的事实判定（D02 放行、无未知元素、卷未锁、视图里没有 IP 数据），不靠 creator 字符串。可读写不等于自动入池，入池仍是 37 的显式 `tape import`。
2. **本系统的卷用直接路径布局，加身份属性**，不采用 EE 的"数据对象 + 符号链接"。EE 那层间接是为 GPFS 的 inode 身份服务的，本项目不依赖 GPFS，命名空间权威就是卷上已提交的索引。直接路径的卷对任何 LTFS 实现都完全自描述，目录重建只需读索引。
3. **池成员用规范 F.4 的标准键**写在卷根，取代 37 里打算自定义的属性；仍只作 import 提示与一致性检查，不作归属权威。
4. **内容哈希默认 `ltfs.hash.sha256sum`**，`md5sum` 为给 IBM 侧校验时的可选项。依据是实测吞吐：两种同时算的单线程速度低于 LTO-8 原生速率。
5. **EE 写的带首版只读支持**（已具备）。导入并继续写会造成一卷两种布局，记为后续扩展，经 37 的 replace/reclaim 类任务迁移。
6. **超过单卷容量的已知大小文件在 admit 阶段以确定的空间不足拒绝**；以后若做跨卷，采用规范 F.1，不自创格式。

### 对其他票据的影响

- 05：文件契约的"标准扩展属性读写"有了落点（键名不带 `user.` 前缀存入索引，FUSE 层转换）；新增"超单卷容量"的确定失败。replace 与改名的卷内表现已写明。
- 37：磁带自描述标记改用 `ltfs.mediaPool.*`；EE 布局的描述已更正。
- 04：D02 两条规则因校准而调整（IP 领先但回指 DP 末索引是一致卷；视图含 IP 数据则只读），已在 [实验室环境](../lab-environment.md)记录，[尾部协议](../recovery-tail-protocol.md)需同步。
- 07：FileService 的 `get_xattr / set_xattr` 直接映射到索引 xattr；AdminService 的 `tape import` 需区分"登记为只读外来卷"与"纳入池可写"。

### 未验证

VL06（2.5 增量索引样本带，暂无来源）、VL07（池成员键经 IBM 侧可读）、VL08（admit 阶段的超容量拒绝）。
