# Holo：LTFS 2.5.1 增量提交与完整检查点验证

2026-09-24，分支 `ltfs-recovery-ibm-interop`。部署的是连接 Holo 的 ltfsd 节点及 tsclient 工具；未修改 Holo 服务。

## 范围与介质

- TAPERS 库，changer `IBMtaper2287_LL3`；drive `IBMtaper2287`，tsclient `/dev/sg6`。
- 当前测试卷 `TS1001L08`，UUID `295fe36b-1154-437c-a1e9-a3205030fb75`；rc 池 UUID `0ba1b026-9595-4b86-9056-c54c7858e744`。
- 已读库存确认：TS1001L08 在驱动器 1，原槽 6；TS1000L08 在槽 5，TR800x 介质未操作。
- 所有三节点先 SIGTERM 正常退出，确认驱动器预留/注册为空。旧 `/cluster` local 摘要 gen8 是历史自述；直接读带确认本次起点 gen21。
- 执行了追加、删除测试路径、Full checkpoint、驱动 LOAD/UNLOAD 和预留注册/释放。无格式化、擦除、回收或物理设备访问。删除不会回收已占带空间。

## 可重复的介质探针

`examples/hw_incremental.rs` 要求显式设备、驱动序列号、卷 UUID、池 UUID；拒绝已有预留，核对卷身份后才注册独占预留。追加随机独立目录，出错保留现场，成功清理目录并清洁卸载/释放。CHECK_ONLY 模式只核验清洁 Full。

原始结果：`probes/results/ltfs25-holo-incremental.log`。

| 提交 | DP 类型 | 连续增量 | DP XML 字节数 |
|---|---|---|---|
| gen22 | 增量 | 1 | 2503 |
| gen23–26 | 增量 | 2–5 | 各 2213 |
| gen27 | 全量 | 0 | 29697 |
| gen28 | 增量删除测试目录 | 1 | 未记录 |
| gen29 | 清洁卸载全量 | 0 | 未记录 |

每次提交独立读取实际 DP XML 并解析，再重新 mount 重放、读回所有已写测试文件；不是仅检查 writer 内存标记。最后核对 DP/IP 都是 gen29 Full，IP 回指 DP Full。原有 14 个文件条目（含既有墓碑）的 UID、extent、元数据保持一致，14 个文件内容哈希全部核对通过。

## 集群 HTTP 与计划停机

三节点统一运行新版后，原目录 gen21 自动与磁带 gen29 对账。

1. HTTP 单次上传 65536 字节到 `/rc/codex-20260924/incremental-http-1208.bin`，确认提交 gen30；下载 SHA256 与源一致。
2. 三节点同时 SIGTERM。检查点和 UNLOAD 完成后预留/注册为空；显式 LOAD 后只读检查得到 `CHECK clean_gen=31 dp_block=213 ip_block=5 files=15`，断言两分区同代 Full 及回指关系。
3. 重启后读回同一文件，SHA256 仍为 `1e62d8c8d0d4339888f01bc22d6cbda3626437ef5c2f991aa05b289a31f25e8a`。
4. HTTP 删除测试文件（gen32 增量墓碑），可见目录恢复为原有 9 个用户文件；集群保持运行，不要求运行中的 DP 尾部为 Full。

日志：`probes/results/ltfs25-holo-{http,shutdown,restart-cleanup}.log`。
首次 ltfsctl 命令误传 `http://` 地址，连接未到达服务器；查询未提交后终止该客户端，改为 `host:port` 成功，只上传一次。

## 部署与交接

- EE1/2/3（10.131.9.71/.72/.74）程序和启动脚本：`/home/rocky/tape-rs-ltfs25-20260924/{ltfsd,start.sh}`。使用 `bash start.sh <节点ID>`；原旧脚本仍指向旧程序，不能用它回退一个存在增量尾部的卷。
- tsclient（.73）：同目录下 `tape-rs`、`ltfsctl`、`hw_incremental`。本次未替换 tape-fuse。
- 三节点停机状态备份：`/home/rocky/ltfsd-backup-ltfs25-20260924`；旧程序保留。磁带已前进，备份不能直接视为最新目录。
- ltfsd SHA256：`1df362ff2b844bdb00e9e55e1265de7e431607d65514e7defadd6da7a2eba44c`，三节点一致。
- 最终探针 SHA256（含 CHECK_ONLY）：`54f0a4a2e8ea10f775468e4ca2a775b9fbb46d129d78e3d90fe5458c92814610`。
- 最终 leader/executor 节点 2，term7、round97，三节点一致。日志保存在各节点新版目录 `ltfsd.log`。

## 验证边界

完成 Holo 实际 SCSI/iSCSI 路径的增量 XML、重挂载重放、周期 Full、删除、清洁卸载、集群提交和重启读回。没有切断 Holo 电源/存储、未做写入中强杀，也没有用 LE 2.4.8 验证本次 2.5 Full 互操作；不把上述结果当成这些能力的证据。

本次新增 example，经 `cargo check --all-targets`、`cargo clippy --all-targets --all-features` 和 glibc2.34 兼容构建；clippy 保留既有告警。探针已 rustfmt，`git diff --check` 通过。未改变上一轮已通过本地回归的生产逻辑。
