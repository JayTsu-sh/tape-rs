# 符号链接创建验收

2026-09-25，ltfs-recovery-ibm-interop。新增 Client / HTTP / 元数据队列 / FUSE 创建链路。只更新工作树和隔离测试程序，原正式集群仍为 symlink-prod 读取版。

## 实现

Client::symlink(target,path) 使用 POST /symlinks，沿用 namespace_change 的提交确认及未知结果不重放。FileService 将目录准入辅助函数扩展为节点元数据准入；创建链接必须有父目录，拒绝同名条目及文件祖先，保留 pending 路径互斥和文件数限制。目标不解析，拒绝空值和 XML 非法字符。Upload 增加可选目标，executor 清理旧墓碑后调用原有 add_symlink、写 catalog version，再提交增量索引。没有新磁带格式、SQLite schema 或依赖。

FUSE 回调经 TapeFs/ClientBackend 创建，返回已提交的链接属性。内核负责目标解析。硬链接仍不支持。本轮未测大目录性能、跨带链接或链接回收迁移，不能据此宣称所有 LE 能力已对齐。

## 软件验证

新准入测试先因缺失 API 失败，实现后通过；覆盖目标非法字符、父目录、冲突和 pending 路径占用。模拟三节点测试验证 Client 创建、特殊字符目标、重复拒绝、接管重挂后读回、链接改名删除再创建及目标内容/元数据不变。Client 丢响应测试增加 symlink，确认不重放。实际本机内核测试增加创建、readlink、目录链接、中文和 mmap。

cargo test --all-features：205通过、7忽略。另运行6项门控 FUSE 内核测试全部通过；第一次内核创建测试暴露回调仍返回 EOPNOTSUPP，补齐回调后通过。cargo check --all-targets 和 cargo clippy --all-targets --all-features 完成，clippy保留既有告警。全仓 fmt check 仍有历史差异；只格式化本轮新增方法/测试片段，git diff --check通过。无 .codegraph 目录。

## 隔离 Holo 与 LE

三节点隔离目录 /home/rocky/tape-rs-symlink-create-20260925，独立test-data，7500/7501，只归属 RT2502L8。原 TS1001L08 正常卸载归槽6，RT 从槽7使用。现场二进制：

- ltfsd SHA256：45ca052da5867aafcf5e77f15ad7a3b728041ed79c08e0b392c25f87cada405d
- tape-fuse SHA256：cc8b3c3013819d9122e3345e96bfd266ce207af90e8057c304ae21dc3494c1bb

RT起点LE2.4.0 Full85。FUSE在 /rc/symlink-create-20260925 创建relative、dangling、directory、loop、中文、chain，另验证临时链接改名及删除、重复EEXIST、通过relative覆盖新测试target后fsync。内容SHA256 dc233f09e28bbab5f69d0013ea971969d12ae0eb212959075ec4354cd25ef665。只修改新测试目录。

正常关闭、卸载后，备份并复制到LE未分配的TS1000L8；LE挂载检查所有6链接目标/lstat、目录链接、悬空ENOENT、循环ELOOP、链式读取和只读mmap全部通过。首轮探针漏写LE路径中的rc，纠正探针后通过；未为此改带。LE已unmount/unassign。没有LE回写再复制到RT，本轮是tape-fuse创建→LE读取验收。

隔离集群重启至node1/round302，以全新FUSE缓存读回全部通过；RT gen99 Complete，卸载后DP/IP均2.5.0 Full99。原有13路径SHA不变。备份位于Holo /home/rocky/holo-symlink-create-20260925/{before,after,le-before}。未断电、未格式化、未访问物理磁带。

## 恢复与证据

测试FUSE/三节点停止，RT归槽7；原集群恢复 symlink-prod 程序，正式部署未改。恢复核验结果见 probes/results/symlink-create-prod-restored.json；原3程序SHA/catalog及9文件内容核对不变，EE节点/任务/f3见symlink-create-ee-restored.log。测试脚本 probes/symlink-create.py 有显式环境变量及固定挂载路径门控。其余证据均以 probes/results/symlink-create- 为前缀。

下一步：将已验收创建版部署到原集群并做独立测试目录验收；继续前先核实原集群当前状态。工作树保留既有未提交改动。
