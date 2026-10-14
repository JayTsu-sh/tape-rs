# Holo 文件客户端现场验收（2026-09-24）

分支 ltfs-recovery-ibm-interop，未提交工作树构建。按交接中的自主实验室授权执行；本次访问并写入 Holo 虚拟设备，没有物理带库验证。

## 部署与边界

- EE1/2/3（10.131.9.71/.72/.74）统一升级 ltfsd；tsclient（.73）部署 ltfsctl、tape-fuse、tape-rs。四个程序在 glibc 2.34 上运行，SHA256 与本地构建一致。
- 新程序目录 `/home/rocky/tape-rs-codex-20260924`。三节点原启动脚本使用的 `/home/rocky/tape-rs/ltfsd` 也已更新，避免下次启动退回旧版本。
- 三节点同时发 SIGTERM 并正常退出，无强杀。停止后备份完整数据目录为 `/home/rocky/ltfsd-backup-20260924`，旧程序保存在新目录的 `ltfsd.previous`。这些是升级前备份；测试已新增落带内容，不能直接回滚备份并假定目录仍代表磁带现状。
- 设备按 VPD 序列号确认：changer IBMtaper2287_LL3、drives IBMtaper2287/IBMtaper3D5E。停止期间从 tsclient 的 /dev/sg5 读取库存（--no-drive-scan）：TS1001L08 在第一驱动器，TS1000L08 在槽 5。其他 TR800x 介质未操作。
- 只在已有 rc 池的 `/rc/codex-20260924/` 创建测试文件。没有触发 reclaim、显式格式化或擦除。停机执行了驱动 UNLOAD 和预留释放；启动执行了 fencing 和恢复。

## 结果

| 项目 | 结果 |
| --- | --- |
| 旧库迁移和恢复 | 新节点恢复 gen3 Complete；原目录 8 个文件保留 |
| FUSE 创建、fsync | 成功，服务端已提交后继续验证 |
| 整文件替换 | 新打开读到新内容，原读句柄仍读到旧内容 |
| 删除及旧句柄 | 路径消失，已打开句柄仍可读；接管后不复活 |
| 元数据 | 修改时间、user.tape.state/version/sha256 可读；SHA256 与新内容一致 |
| GNU mv 8.32 | EXDEV 复制删除回退成功；提示权限/时间保存不支持，退出码 0；另行确认目标 committed 后校验内容 |
| HTTP Range | bytes=7-1030 返回 206，内容逐字节一致 |
| 只读共享 mmap | **未通过**：Rocky 5.14.0-427.20.1.el9_4 返回 ENODEV。普通读写正常；不能用本机 6.8 的通过结果覆盖此限制 |
| 计划接管 | 停止节点 1，节点 2 成为 leader/executor，term5 round61；目标哈希一致，两条删除路径仍不存在 |
| 原有文件完整性 | _probe.bin、after_reclaim.bin、f0.bin 至 f5.bin 全部读回，长度及 SHA256 与索引一致 |
| 收尾 | 节点 1 已恢复，三节点池状态一致；FUSE 已卸载、私有缓存清理；EE 三节点 available、无活动 EE 任务 |

保留验收文件 `/rc/codex-20260924/move-target.bin`（69632 字节），SHA256 `d064c6fd6064551fe0aa780315acab0d16d5747fa04bf470ab6a2b3d421795e4`。另有 snapshot.bin 与 move-source.bin 的删除墓碑；介质 gen8 的文件计数 11 包含墓碑，不等于用户目录可见文件数。

最终节点 2 服务，TS1001L08 gen8 Complete；TS1000L08 保持 gen1。节点 2/3 日志为 `/home/rocky/ltfsd-codex-20260924.log`，节点 1 恢复后使用原脚本的 `/home/rocky/ltfsd.log`。客户端实验脚本和下载样本留在新程序目录。

## 验证局限

本次是计划停机接管，不是写入中故障或断电试验。未重跑 EE 的删除/覆盖迁移对照，也未做跨带回收现场测试。Holo 控制面 load_seq 解析修复只在源码中通过测试，尚未部署，库存判断采用 SCSI 结果。Range 仍整文件读出后切片。共享 mmap 在实验室内核不可用。

实验脚本首次因 mmap 不可用中止；核验已创建文件内容后续跑。后续脚本曾误用 `/stat?path=...` 导致 404，修正为现有 `/stat/<path>` 后完成验证；不是服务端丢失文件。未重新执行无关 Rust 测试，本次未改 Rust 代码。

## 后续页缓存版本验收

用户根据 LE 实测批准改造后，tape-fuse 已启用页缓存与 write-through，改变覆盖时每句柄快照语义。最终版本 SHA256 `4acb0a8e7629de82b0dfd6127ec5bf47148058d79f403e36690e4b67bdf0f7ac` 在 tsclient 相同 Rocky 5.14 内核上共享只读 mmap 通过，ENODEV 限制已解除。同 inode 的 fsync 前可见性、O_TRUNC、旧读句柄与映射跟随写入，以及 unlink/recreate 隔离、close fd 后继续映射全部通过。远端覆盖通过 ESTALE 保守拒绝混用，释放全部引用后重开刷新。

此版本仅更新客户端，未重启集群。独立测试文件已删除（有落带墓碑）、挂载卸载、私有缓存清理；旧 direct I/O 版本已备份。实现、157项默认测试及3项内核测试结果详见 handoff-to-codex.md 最新记录。上文 ENODEV 是旧版本历史结果，不再描述当前实现。
