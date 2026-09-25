# Holo VM 断电与 LE Full 交接验证

2026-09-24，用户明确允许共享 Holo 断电测试。对象为 PVE VM510/Holo 10.131.9.70；不是物理 PVE 主机或存储断电。

- 停止客户端写入并备份 Holo 配置、数据库、运行参数；PVE 系统/数据 ZFS 卷创建 `before-ltfs25-powercut-20260924` 快照，未回滚。
- TAPERS/TS1001L08 gen33 Full 起，`examples/hw_powercut.rs prepare` 顺序提交三个增量，ACK gen34、35、36。每次64 KiB数据，附空目录、相对符号链接及二进制 xattr。没有正常 unmount 或最后 Full 检查点。
- 12:16:04–12:16:07 UTC 执行 `qm stop 510` 并确认 stopped，随后 start；12:16:50 Holo可达、12:16:55 targets active。
- 原卷恢复 gen36，增量深度3；三份数据、元数据正确，全部原有15条目内容/元数据校验通过。再生成 Full gen37。
- 实际 EE1 LE 2.4.8.3 挂载 gen37 的关闭副本 TS1001L8，读到2.5.0；三个payload、只读共享mmap、空目录、相对链接、二进制xattr及原探针均通过。该副本随后正常卸载、归槽、删除。
- 原卷仅删除本次 `rc/powercut-37b5d411-43d5-4d48-b188-e2ad3276e31c` 前缀，增量gen38后生成Full gen39，原15条目再次校验。此后 LE 降级测试只使用副本，见 `holo-le-downgrade-client-20260924.md`。

限制：VM hard stop 并不切断宿主存储电源；结论是 Holo VM 突停后已确认增量恢复通过，不能宣称真实磁带驱动器掉电或宿主写缓存掉电安全。LE验证只覆盖Full检查点，不证明旧LE能重放2.5增量。

恢复时 Holo 默认配置未保留原手工 handler profile，按备份恢复既有7个运行参数；TAPERS product TS4300 不被该LE支持，临时测试3573后又遇许可证检查，均已恢复，没有绕过许可。正式LE测试改用已有授权的LISA4300路径。创建/删除Holo介质会禁用EE2/EE3手工发布的changer targets，已重新发布原参数并恢复ACL；最终9个ready targets，EE三节点available、原ltfsd三节点正常。没有格式化/擦除原带。

证据：`probes/results/ltfs25-powercut-{prepare,stop,start,recover,cleanup}.log`、`ltfs25-ee-le-verify2.log`、`ltfs25-ee-le-journal.log`、`ltfs25-ee-clone-cleanup.log`。原始manifest在tsclient `/home/rocky/tape-rs-ltfs25-20260924/powercut.json`；Holo配置备份 `/home/rocky/holo-powercut-20260924/` 和PVE快照保留，不应无条件回滚到旧状态。
