# 可写 xattr 与空值 XML 修复：原集群统一部署

2026-09-24，ltfs-recovery-ibm-interop。本轮完成正式部署和原带专用目录验收，无生产源码变更。

## 发布

三节点 .71/.72/.74 的正式目录 /home/rocky/tape-rs-xattr-prod-20260924。ltfsd 使用隔离验收的同一文件，实际 /proc/PID/exe SHA256 全为 f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。

原 /home/rocky/tape-rs-ltfs25-20260924/start.sh 已更新二进制路径，仍用 bash 调用，保留全部设备serial、7400/7401端口、数据目录和运行参数。三节点同时正常停止后备份，未混跑新旧服务端。各新目录权限0600的pre-upgrade.tgz包含关闭的数据目录、旧读取修复版ltfsd及旧启动入口；不能把该备份直接覆盖已经推进的Raft状态。

tsclient使用同名正式目录中的tape-fuse，SHA fac4a6ae370f978c39b5b15bf24985f03899c2a63954f5505ca657238fdcad86（与隔离验收相同）。ltfsctl按当前源码zigbuild glibc2.34重新构建，SHA 3688d36c31fd57f71d2e7a89eee02020647c2260b759e206af34b9a43e881d3a；同步到常用/home/rocky/ltfsctl，旧文件保存为正式目录中的ltfsctl.before。Client的cluster命令实际执行通过。

## 验收与清理

升级前 node1/term25/round207、TS1001L08 gen58 Complete、39MiB可用。原9文件逐个长度/SHA256全部匹配。仅原池TS1000L08和TS1001L08归属；隔离副本未归属，未启动隔离服务。

新版本node2/round212接管。门控Python探针经真实FUSE仅写/rc/xattr-prod-20260924：文件内容、二进制属性、空值、CREATE冲突EEXIST、REPLACE、删除属性、目录属性及listxattr、mtime不变、只读共享mmap均通过。文件实际barcode确认TS1001L08。

正常卸载FUSE、全停节点后形成Full gen67；先启动node1与node3，node1/round233接管，随后恢复node2。全新FUSE挂载读回内容、属性、空值及删除状态成功，然后删除空值/目录属性、文件及专用目录。原9文件通过FUSE长度/SHA256再次校验一致。

清理后再正常全停、卸载和释放PR，重启后node3/term28/round246服务，TS1001L08 gen72 Complete、32MiB可用，TS1000L08仍gen2。HTTP最终原9文件长度/SHA256和完整list与升级前一致，/rc直接子目录列表确认专用目录已消失。删除保留墓碑并消耗索引空间，未回收、格式化或声称空间恢复。

三节点applied249，catalog31条，排序JSON SHA256全为a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0。增加两条测试对象墓碑是预期结果。测试FUSE已卸载，无遗留tape-fuse进程。

EE3节点available、无活动任务，/gpfs/tapers-del/f3 SHA256仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e；Holo9个publications均ready。没有断电、SIGKILL、格式化、介质克隆或物理TS4300操作；本轮涉及Holo原带元数据/少量文件写入、计划停机及正常接管。

## 验证范围

本轮只增加门控探针和文档；Python语法、git diff --check通过。服务端/FUSE沿用已通过201软件测试及内核/Holo/LE验收的二进制；未重复运行整套Rust测试、check或clippy。ltfsctl兼容release构建成功。全仓历史格式差异保持。

证据：probes/xattr-prod-fuse.py 与 probes/results/xattr-prod-*。包含升级前原文件、集群状态，属性准备、接管读回/清理、原文件FUSE验证、最终HTTP文件/目录列表、三节点程序与catalog哈希、EE与publication状态。

可写xattr及空值IBM兼容修复已部署。现有范围仍为当前写入卷用户属性，跨带EXDEV，根目录LE虚拟控制属性未开放。下一步优先补充隔离Holo的属性提交中断/响应丢失验收；不要在原数据带注入故障。跨带rename、符号链接创建等继续作为独立能力。所有工作树改动未提交。
