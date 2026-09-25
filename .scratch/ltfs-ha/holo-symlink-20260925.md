# LE 已有符号链接读取修复

2026-09-25，ltfs-recovery-ibm-interop。修复前rc/link稳定EAGAIN；修复后原严格LE往返探针及链接专用检查通过。仅部署到隔离测试环境，原集群未升级。

## 根因和实现

前轮已确认带上link→keep.bin、length0；catalog丢失链接类型和目标，FUSE把它当普通零长度文件，HTTP GET返回目标64KiB，触发下载长度防护。

`daemon/files.rs::catalog_of`仅对链接增加metadata.kind=symlink和metadata.symlink，保持普通文件JSON、磁带XML及SQLite表结构不变。Client通用metadata透传，无需新协议端点。`tapefs.rs`增加链接属性及readlink（目标缺失、空或含NUL返回EIO），直接逻辑open链接返回ELOOP，内核先解析目标再打开普通文件；保留长度/版本校验。`tape-fuse.rs`报告Symlink、目标字符串字节数和readlink响应；目录打开对非目录项补查属性，快照报告正确d_type，已消失条目跳过。

兼容要求：需要升级服务端、重新挂载介质对账，再用新FUSE挂载；旧catalog没有目标字段。该新增字段不修改普通文件目录记录。目录补查会增加元数据请求，大目录性能尚未实测。链接创建、硬链接、完整链接写/改名/删除能力没有在本轮宣称对齐。

## 回归与现场

新内核测试直接使用实际catalog_of生成元数据，再经Backend→TapeFs→FUSE内核读取。修复前islink断言失败，修复后相对链接、链接链、目录链接、中文名称、readlink/lstat、scandir类型、悬空ENOENT、循环ELOOP、只读mmap通过。新增默认catalog测试确认链接字段及普通文件元数据不变。

正常停原集群，TS1001L08归槽6；独立`/home/rocky/tape-rs-symlink-20260925/test-data`，7500/7501，只归属RT2502L8（LE2.4.0 Full85）。程序：

- ltfsd：418339d1f60b277c07759b42c775342557c36c84fc4df3a84a23b6ed13f88ae8。
- tape-fuse：a8ccc02cc7a25f88d6f4b50ea3d36ba00c1837dd19f172bf2abc8bd4a05f4d25。

使用全新symlink/cache，复用已卸载的recovery-le/mnt路径。前轮严格`recovery-le.py fuse`现在通过：13个原路径（包含链接）及LE新文件内容、二进制/空属性、mtime、pread/mmap正确。独立断言rc/link为符号链接、readlink=keep.bin、lstat.size=8、scandir类型及链接上的mmap正确。HTTP stat实际包含kind/symlink。只读后正常卸载，DP/IP仍LE2.4.0 Full85，未先写带修饰测试结果。

## 验证与恢复

cargo test --all-features最终203通过、7忽略；6项本机门控FUSE内核测试另行全部通过。首次全套有namespace_conflicts_across_tapes_and_takeover失败，原因是读检查点时执行资格已失去/HTTP500；单独重跑及完整复跑通过，保留失败日志，不把首轮记成通过。本轮没有修改该测试或接管生产逻辑，不能据此宣布该间歇性竞态已修复。

cargo check --all-targets和clippy --all-targets --all-features完成；清理本轮测试新增clippy告警后无本轮新增告警。全仓fmt仍既有差异，仅格式化新增测试区域及触及片段，未格式化无关代码；git diff --check通过。无CodeGraph目录。兼容构建成功，最后仅测试格式及文档变化，生产逻辑与现场版本相同。

测试FUSE和三节点已停止，RT归槽7；Holo备份`/home/rocky/holo-symlink-20260925/{before,after}`。LE副本未访问，仍Full85/unassigned。原集群恢复node1/term35/round281、旧正式ltfsd f8c238…；TS1001L08仍72 Complete，TS1000L08仍2。原3程序哈希、catalog31条/a6c01d…、9文件长度/SHA及完整list不变。EE3节点available无任务、f3哈希不变、9publications ready。

无VM断电/Holo重启/格式化/物理磁带操作。证据`probes/results/symlink-*`含首次红、修复绿、软件全套失败与最终通过、内核套件、现场严格读回、链接检查、最终XML及原环境复核。全部工作树未提交。
