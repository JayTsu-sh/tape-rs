# 符号链接创建：原集群部署验收

2026-09-25，ltfs-recovery-ibm-interop。使用隔离Holo/LE验收过的同一二进制，升级原三节点与tsclient；本轮未修改生产源码。

## 部署

正式目录 /home/rocky/tape-rs-symlink-create-prod-20260925：

- .71/.72/.74 ltfsd SHA256：45ca052da5867aafcf5e77f15ad7a3b728041ed79c08e0b392c25f87cada405d。
- .73 tape-fuse SHA256：cc8b3c3013819d9122e3345e96bfd266ce207af90e8057c304ae21dc3494c1bb。

实际/proc/PID/exe校验一致。启动入口仍 /home/rocky/tape-rs-ltfs25-20260924/start.sh N，仅替换程序路径，bash语法检查通过。端口7400/7401、原ltfsd-data、设备serial、批次/快照参数保持。原ltfsctl cluster调用正常。

三节点同时SIGTERM并确认退出后备份，再统一切换。各正式目录pre-upgrade.tgz权限0600，包含关闭的原ltfsd-data、旧读取版ltfsd和启动入口，已验证归档可列举；另有start.before.sh。客户端保留tape-fuse.before。不能用旧Raft/catalog备份覆盖已推进的在线状态。

## 验收

升级前node1/term37/round291、TS1001L08 gen72 Complete，原9文件长度/SHA与基线相同。停机库存确认原带在drive0，RT2502L8在槽7；本轮不移动介质，不访问LE副本。

新程序启动后node1/round296，三节点catalog仍31条、a6c01d…一致。正式FUSE在/rc/symlink-prod-20260925创建新target和6类链接：relative、dangling、directory、loop、中文、chain。验证readlink/lstat/scandir类型、链式读取、目录解析、ENOENT/ELOOP、只读mmap、重复EEXIST，通过链接覆盖新target并fsync，以及临时链接rename/unlink。

卸载FUSE后正常停止node1，node3接管至term39/round327，gen86 Complete；重启node1重新加入。新缓存挂载全部链接检查通过，再验证并清理专用目录。末次读取原文件时实际mount日志为gen95、DP/IP Complete、writable=true。/cluster.local仍显示接管时gen86，是旧描述，最终代数以catalog及mount日志为准。

原9文件全部真实FUSE读取、长度/SHA256、mmap及barcode验证通过。测试目录已不存在，FUSE正常卸载，无残留进程。三节点实际程序一致，catalog均42条，排序JSON SHA256为7d5660c28eb3023b91fc7f835ad41028c81530fe562bdfc1563437df065dbcd4。与停机备份比较原30条非/rc记录（排除随索引变化的generation列），内容、版本、删除标记、属性保持相同。/rc因增删测试子目录更新元数据；新增目录记录为测试删除后的墓碑，不宣称整个catalog未变。测试数据块占用保留在磁带上。

EE三节点available、无活动任务，/gpfs/tapers-del/f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e。本轮没有VM断电、Holo重启、格式化或物理磁带操作。

## 证据与范围

probes/symlink-prod.py 固定正式测试目录并要求TAPE_RS_SYMLINK_CREATE=TS1001L08-production-test；创建不覆盖已有目录，清理先核对内容和精确条目集合。证据probes/results/symlink-create-upgrade-*含升级前/运行/最终状态、创建/重挂/清理、原文件FUSE校验、EE及最终挂载日志。实际程序/catalog辅助脚本 /tmp/symlink-create-upgrade-catalog.py；通用当前程序校验 /tmp/symlink-create-prod-node-verify.py。旧/tmp/symlink-prod-node-verify.py固定旧程序哈希，不能再用来验证当前版本。

本轮部署的是上一轮205项软件测试、6项门控内核测试及Holo/LE通过版本，没有重复跑Rust全套或修改源码。启动语法、备份、实际程序哈希、现场验收和git diff --check通过。全仓历史格式差异未处理。

符号链接创建已正式部署，取代前轮“尚未部署”状态。硬链接、链接跨带回收迁移和大目录性能仍未验收，不能称完整LE能力全部对齐。工作树未提交。
