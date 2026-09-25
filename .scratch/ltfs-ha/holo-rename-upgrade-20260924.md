# rename / LE 兼容修复版本统一部署

2026-09-24，分支 `ltfs-recovery-ibm-interop`。原 Holo 三节点已统一升级，并完成原集群上的改名、重启接管、清理与完整性验收。没有新增生产逻辑。

## 部署

- 三节点10.131.9.71/.72/.74，原数据目录 `/home/rocky/ltfsd-data`、7400/7401端口不变。
- 新正式目录 `/home/rocky/tape-rs-rename-prod-20260924`。原入口 `/home/rocky/tape-rs-ltfs25-20260924/start.sh N` 已指向新版；新目录另存start.sh。
- 同时SIGTERM正常停机，确认驱动卸载、PR释放后，各节点保存 `pre-upgrade.tgz`，含关闭的原数据目录、旧程序及旧启动脚本，权限0600。不得用此旧数据库备份覆盖已经产生新提交的现状。
- 新ltfsd实际/proc/exe三节点SHA256均 `dc1e70ac7080e69e5104f0d8bd709d2f6230719993c61b433f0e542db9ded0ac`。
- ltfsctl SHA256 `e70a6189647215f70edcd2e75fd77dec100dd7294875fb053c48a8a090e1c7ee`；tape-fuse SHA256 `7134549765506d1ab4a3db5520177711120bba7736fbc29e8717a4c84cfbf54a`。三节点及tsclient新目录均校验一致。
- tsclient常用 `/home/rocky/ltfsctl` 更新，旧版保存在新目录 `ltfsctl.before`；FUSE从新目录运行，验收挂载已卸载。
- 全停全启，没有新旧版本混跑。隔离副本RT2502L8/TS1000L8未归属原集群，隔离服务未启动。

## 验收

升级前node2/term19/round155、TS1001L08 gen45 Complete，原9文件逐一长度与SHA256验证通过。当前全部归属仍仅TS1000L08(gen2)与TS1001L08；此前目录升级已完成两卷对账，本轮不必为无变更的空卷额外换带。

1. 新版接管node1/round160，同代全量目录对账。原探针的barcode确认TS1001L08。
2. 显式门控探针 `probes/rename-upgrade-fuse.py prepare` 在 `/rc/rename-upgrade-20260924` 创建目录树及13字节文件。目录改名后，已打开writer继续写入并fsync成功；再改文件名，inode保持；NOREPLACE对已存在目标返回EEXIST；只读共享mmap内容正确。
3. FUSE卸载、整群正常停机，DP/IP落下2.5.0 Full gen53。先启动node2/node3，新Leader node3/term21/round179从介质恢复；之后补回node1。新FUSE挂载 verify 通过：旧路径不存在，新目录/空目录/文件内容与mmap均正确。
4. cleanup 删除专用文件及目录，正常卸载FUSE、全停落下Full gen58，再恢复三节点服务。
5. 原9文件逐一下载，长度/SHA256全部一致；最终list与升级前9文件清单完全一致。三节点catalog均29条（包含目录/墓碑），排序序列化SHA256 `3bf72f0e246b39c695afa54c1dc85fa83f4c046a58eecd23482c8f50df40e7c0`，applied195。

最终node1/term22/round192，TS1001L08 gen58 Complete、39MiB可用。验收数据和删除记录占用带上空间，不因清理路径而物理回收。EE三节点available、无任务，f3哈希仍 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`，9个Holo publication均ready。

## 检查与边界

- 本轮重新构建release全bins（glibc2.34、fuse），Python探针语法和git diff --check通过。
- 生产逻辑沿用上一轮194测试通过/1忽略、all-targets check与all-features clippy结果，本轮没有重跑全套；全仓fmt基线差异仍在。
- 实测证据位于 `probes/results/rename-upgrade-*`，含升级前清单、FUSE三个阶段、Full索引、最终节点/目录/程序一致性及EE状态。
- 本轮只有Holo虚拟介质写入及正常停机接管，无SIGKILL/VM断电、格式化、介质创建删除或物理TS4300操作。故障响应丢失和LE2.4回写修复沿用上一轮隔离验收结果，未在原带上重复LE往返。
- rename仍限当前写入卷内；跨带EXDEV、可写xattr/符号链接等未完成。此前发现的Client缓存旧Leader后读取失败尚未修复，建议下一步单独处理安全的Leader发现与应用层读取重试，不重试位置相关SCSI命令。

所有代码与记录仍未提交。
