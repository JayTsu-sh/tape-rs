# 硬链接支持边界与显式同步（2026-09-27）

## 标准与LE依据

[SNIA LTFS 2.5.1 §9.2.8](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf#page=48)要求索引内文件/目录fileuid唯一；格式没有标准化共享inode硬链接关系。本项目不引入私有持久化扩展或以复制冒充链接。[IBM LE虚拟属性](https://www.ibm.com/docs/en/storage-archive-le/2.4.7?topic=attributes-virtual-extended)将ltfs.sync定义为卷根只写控制，虚拟属性不列举。[同步操作](https://www.ibm.com/docs/en/storage-archive-le/2.4.8?topic=tips-sync-operation)区分数据flush与索引同步。

实际LE 2.4.8.3：在专用SR2501L08 /sr/sync-boundary-20260927测试，硬链接ENOSYS(38)、无目标；根同步空值/CREATE/REPLACE成功，文件和普通目录EACCES(13)，listxattr不列出。临时文件/目录删除后显式同步、正常卸载/取消分配。结果results/sync-boundary-20260927-le.json。该LE副本从Full8历史继续，已与TAPERS SR1的性能负载分叉，**不要再把LE副本覆盖SR1**。

## 实现和保证

保持既有硬链接EOPNOTSUPP(95)，显式拒绝无副作用，与LE能力一致但errno不强求相同。新增挂载根user.ltfs.sync控制：冻结本挂载已有句柄并提交；服务端POST /sync绑定接纳轮次，强制一个已完成上传批次并复用已有完整索引检查点/预留验证。批次上限防止新请求无限延长本次同步；尚在接收的HTTP请求不包含。关闭后的本地暂存仍按长度/哈希确认，换届丢失或他人替换不能误报成功。索引失败关闭本轮服务并报告失去资格，HTTP500/504或传输失败为结果未定，不自动重放。虚拟属性不入索引、只写不列举，根以外EACCES；fsync的既有更强归档保证保留。未改SCSI编码、超时、持久化格式或生产依赖。

软件验证：218测试通过7忽略；fmt/check/严格Clippy通过。覆盖open/closed/其他客户端写入、Full检查点、重复空闲同步、属性边界、暂存丢失/被替换、UA/介质错误、响应丢失不重发。候选ltfsd SHA256 5d2ff8eb0b39debadb95739213cfdf315cb444786f9540420053a78554ecaaa0，FUSE 62aa241430eb677fcbca9de807a631334816875e09694f0711ea66ea318996a2，ctl ec5ff8ddc0ae3c8d6fefa7d89eb68f8678fb07c0eb9d4cdc698d54fdb3130b01。兼容构建仍有Zig外部链接器废弃优化参数提示，未屏蔽。

## Holo实际验证

隔离目录/home/rocky/tape-rs-sync-boundary-20260927（.71/.72/.74），初始SR1 Full24/SR2 Full3、1132行。门控探针probes/sync-boundary-20260927.py覆盖open writer、已close暂存、独立HTTP上传、二进制属性、空值及CREATE/REPLACE、非根拒绝、硬链接拒绝、虚拟属性读/列举/删除边界。全部通过。

显式同步后、停止服务前，辅助只读探针按Holo DTV2数据块格式去除存储记录头，确认DP/IP末次Full均generation30、1125文件，无ltfs.sync实体属性；原始直接正则拼接因大索引跨块头失败，不把失败结果作为证明，探针根据本地Holo storage/data_path.rs格式修正并保留脚本。SCP写root备份目录被拒后改用/tmp，不改目录权限。

同步成功后SIGKILL node1/round208（无正常停机检查点），node3/round225接管仍Full30 Complete；全新FUSE缓存读回三个文件及二进制属性正确。原LE文件/六种链接/mtime/xattr再次通过；性能目录全部名字/类型及每目录首/中/尾样本内容通过。不是物理驱动断电测试。证据results/sync-boundary-20260927-*。

## 收尾

正式恢复node2/term51/round449，三节点原53行逐字段相同、原9文件冷缓存长度/SHA/mmap/条码复验通过。EE三节点available无任务、f3原摘要不变、9发布ready。隔离及FUSE进程停止，SR1 Full30归槽8、SR2 Full3槽9；介质前后SHA备份/home/rocky/holo-sync-boundary-20260927。LE SR2501L08 Full10已取消分配归槽1033。最新关闭隔离数据sync-boundary/test-data，node3/round225、1136行，两驱动配置。正式程序保持旧已部署版本；本队列新增修复/性能/同步程序仅隔离验证，未推送GitHub，物理TS4300验收暂缓。
