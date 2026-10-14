# 目录版本正式集群部署（2026-09-24）

本轮已将前一轮通过 Holo/LE 往返验收的目录版本统一部署到原三节点，并完成全部已归属介质的对账。无生产源码变更；没有继续实现 rename。

## 部署与备份

- 三节点 `.71/.72/.74`，原数据目录 `/home/rocky/ltfsd-data`、端口7400/7401保持不变。
- 正式发布目录 `/home/rocky/tape-rs-directories-prod-20260924`，与此前隔离测试目录分开。
- 各节点原启动入口 `/home/rocky/tape-rs-ltfs25-20260924/start.sh N` 已更新到上述新 ltfsd；新发布目录另存同内容 start.sh。
- 三节点实际 `/proc/<pid>/exe` SHA256 均为 `2b5bd74cec73809b5b818947a1ddebec79527043286a054d6148497fb4912491`，与上一轮验收二进制一致。
- ltfsctl SHA256 `bcf87133e55351045efa8192c73a853860eceec8a53d9703c3bfe9ecf01defba`；tape-fuse SHA256 `68306acc5f22de45a9f52761dabc403b9404aeb9263eb1f434af6235799f9a19`。发布目录三节点和tsclient均有这两个程序，tsclient常用 `/home/rocky/ltfsctl` 已更新，旧文件存为新目录 `ltfsctl.before`。
- tsclient FUSE 正式使用新发布目录的 tape-fuse；本轮验收挂载 mnt 已正常卸载，没有留常驻挂载。历史测试目录内旧二进制仅作历史留档，不应再用于当前目录 catalog。
- 全停全启避免新旧节点混用。停机确认 PR 释放后，每节点在新目录保存 `pre-upgrade.tgz`，包含关闭的原数据目录、旧 ltfsd 和旧启动脚本，权限0600。升级后已产生新带上提交，不能用旧数据库备份直接覆盖当前 Raft/catalog，也不能仅切回不认识目录记录的旧程序。

## 介质对账与验收

已归属的只有 TS1000L08、TS1001L08，隔离副本 RT2502L8 未归属原集群。

1. 升级前读取原9个可见文件，逐一验证长度与既有SHA256，保存 `probes/results/dirs-upgrade-before-files.json`。
2. 原三节点并发SIGTERM正常停机。TS1001L08卸回槽6，TS1000L08从槽5装载；新版启动后将其旧catalog gen1更新为介质实际gen2，确认原生索引没有文件或子目录，没有格式化或写入该卷。
3. 再正常停机，TS1000L08回槽5，TS1001L08装载并启动。即使磁带与catalog同为gen39，仍完成全量对账，补齐 `/rc` 与三个原生子目录记录。
4. 新版实际FUSE创建 `/rc/directory-upgrade-20260924/empty`，确认非空rmdir返回ENOTEMPTY；读取原9个文件，SHA256全部一致。
5. 正常卸载FUSE、停止全群，DP/IP生成同代Full gen42。重启全群和FUSE后空目录仍存在，原9个文件再次通过SHA256。
6. 删除验收子目录和父目录，确认不可见，再次验证原文件；FUSE卸载，正常停机生成Full gen45，重启恢复服务。

最终三节点applied_index153、catalog21条，按全部字段排序序列化后的SHA256均为 `081d28507122a1e663e4e7b9cc7890e64e4a740c74328166e9ee2b62860e7462`。21条包含文件、目录及墓碑，不能解读为21个可见文件；实际list仍为原9个文件。验收清理留下两个目录墓碑，没有回收带上物理空间。

最终node1/term18/round150，TS1001L08 gen45、Complete、可用45MiB；TS1000L08 gen2。原probe SHA256 `49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7`，EE f3 SHA256 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`，均不变。EE三节点available、无活动任务。

## 验证范围

本轮新增 `probes/directories-upgrade-fuse.py`，由显式 `TAPE_RS_UPGRADE_MOUNT` 门控，限制唯一挂载点和验收目录；Python语法检查及git diff --check通过。程序沿用上一轮已完成的187项测试、all-targets check和all-features clippy构建结果，本轮没有重跑软件全套。全局fmt历史差异仍在。

实际远程验证见 `probes/results/dirs-upgrade-*`：升级前文件清单、FUSE创建/重启/清理、Full索引、三节点日志/catalog一致性和EE状态。

本轮有Holo虚拟介质移动及索引写入，无物理带库、格式化、擦除、断电或介质创建删除。全部源码和证据仍未提交。rename仍EXDEV，后续继续接入文件/目录rename及对应持久化、并发和互操作验收；本轮不代表全部LE能力已对齐。
