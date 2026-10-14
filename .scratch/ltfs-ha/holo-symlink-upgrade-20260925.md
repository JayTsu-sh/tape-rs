# 符号链接读取修复：原集群升级

2026-09-25，ltfs-recovery-ibm-interop。已统一升级原三节点服务端及tsclient FUSE，正常运行，无新增源码修改。

## 发布与备份

正式目录均为`/home/rocky/tape-rs-symlink-prod-20260925`。

- .71/.72/.74 ltfsd：418339d1f60b277c07759b42c775342557c36c84fc4df3a84a23b6ed13f88ae8。
- .73 tape-fuse：a8ccc02cc7a25f88d6f4b50ea3d36ba00c1837dd19f172bf2abc8bd4a05f4d25。

使用隔离验收的同一二进制，实际/proc/PID/exe哈希均验证一致。原启动入口`/home/rocky/tape-rs-ltfs25-20260924/start.sh N`仅替换程序目录，保留设备serial、7400/7401端口、数据目录、批次及快照参数，bash语法检查通过。客户端从新正式目录启动FUSE；ltfsctl及Client接口本轮无变化，原/home/rocky/ltfsctl的cluster命令对新服务执行成功。旧发布目录保留作回退材料，后续应使用新FUSE路径。

三个节点同时正常停止并确认无ltfsd后才备份和切换，未混跑新旧服务端。各新目录`pre-upgrade.tgz`权限0600，保存关闭的ltfsd-data、旧正式ltfsd及旧入口；另存start.before.sh。客户端保存tape-fuse.before。不能直接把旧Raft/catalog备份覆盖已推进的在线状态。

## 原环境验证

升级前后原9文件长度与SHA256相同。新节点重新挂载原带并对账，node1/term36/round286服务，三节点applied289。TS1001L08仍gen72 Complete、32MiB可用，TS1000L08仍gen2。

全新FUSE缓存、真实内核挂载验证全部9文件read、长度/SHA256、MAP_SHARED/PROT_READ mmap及user.tape.barcode；rc目录列举成功。测试挂载随后正常卸载，无遗留FUSE会话。HTTP完整list、9文件读回，以及三节点catalog31条、排序JSON哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0全部不变。

本轮未在原带创建测试链接；已有LE链接readlink/lstat/scandir/mmap与原严格往返探针的验收见前轮holo-symlink-20260925.md，不能把前轮隔离介质证据写作本轮原带链接实测。

EE三节点available、无活动任务；GPFS f3哈希cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e不变。Holo 9 publications ready。隔离RT2502L8和LE TS1000L8未访问，仍保持LE2.4.0 Full85及原归槽状态。

## 范围与后续

本轮只有计划停机、备份、部署、重挂对账和读取；无用户文件/属性写入、VM断电、Holo重启、介质复制、格式化或物理磁带操作。新增链接元数据格式已由前轮软件和隔离现场验证，本轮未重复运行203项Rust全套、内核套件或check/clippy。启动脚本语法、备份可列举、程序哈希及git diff --check通过；全仓既有格式差异保持。

符号链接读取已正式发布；链接创建、硬链接、完整链接写入语义尚未完成。目录补查带来的大目录性能，以及前轮接管测试出现过的间歇性资格丢失HTTP500，仍应独立验证。

证据`probes/results/symlink-upgrade-*`：升级前list、原文件FUSE校验、实际FUSE程序、三节点实际程序/catalog、HTTP原文件、Client cluster、EE及publication。当前程序核验辅助脚本为本地`/tmp/symlink-prod-node-verify.py`（旧xattr脚本固定旧哈希，不能用于新版本）。所有工作树改动未提交。
