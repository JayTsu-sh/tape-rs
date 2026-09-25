# Holo：LE 回写降级、tape-fuse 与 Client 库实测

日期：2026-09-24。分支 `ltfs-recovery-ibm-interop`。

## 结论

实际 IBM LE 2.4.8.3 (10521) 可以读入 tape-rs 的 2.5.0 Full，新增文件并同步、正常卸载后写出 2.4.0 Full。tape-rs 恢复该索引后，真实 tape-fuse 挂载和 Rust `tape_rs::client::Client` 均能读回内容。Client 常用创建、覆盖、冲突和删除接口也通过。

本次用 Holo 虚拟介质，不是物理磁带。没有格式化或擦除原介质，也没有再执行断电。

## 介质、隔离及方法

- 原磁带 TAPERS/TS1001L08：UUID `295fe36b-1154-437c-a1e9-a3205030fb75`，起点/终点 gen39 Full，原集群目录完整保留。
- 正常停止原 ltfsd 集群，复制关闭状态下的介质数据到 LISA4300/TS1001L7，由 EE1 的真实 LE `/ltfs/TS1001L7` 挂载。使用此条码是为了满足 LE 的八字符条码和 ANSI 卷标 TS1001 一致性检查；它是虚拟 LTO7 副本，不能推断物理 LTO8 的固件兼容性。
- LE 写入 `rc/le-downgrade-20260924.bin`，65,569 字节，设置 `user.interop.binary` 为 `00 ff 4c 45 32 34`，同步索引，再正常卸载、unassign、归槽。
- LE 回写前 `user.ltfs.indexVersion=2.5.0`，回写后为 `2.4.0`。实际 DP/IP XML 均为 gen40、version2.4.0、creator IBM LTFS 2.4.8.3；不是只改变版本字符串。
- 再将关闭的 LE 副本完整复制到 TAPERS/LD2502L08；三节点隔离服务运行于 tsclient 的 127.0.0.2/3/4:7501，独立 Raft/catalog/spool，仅归属该副本。
- 磁带池 UUID 为 `0ba1b026-9595-4b86-9056-c54c7858e744`，LE 保留属性不允许 setxattr 修改。使用一次性测试入口通过现有 `NodeInput::Admin/PoolCreate` 创建同 UUID 的 rc 池；生产库和生产 HTTP/FUSE 路径不变。入口差异保存在 `probes/hw_interop_daemon.patch`，不作为生产配置功能。
- 初次尝试的 LD2501L8 因 ANSI 前缀不符被 LE 拒绝；修改保留池 UUID 的尝试返回 EACCES，发生于写文件之前。测试改为保留原 UUID，未绕过 LE 许可。
- tape-fuse 使用最新构建和全新空缓存、新挂载。先完成全部读验证，再调用 Client 写接口。因此读回结果没有经过 tape-rs 重新写 Full，也不是旧缓存。

## 实测结果

| 路径 | 检查 | 结果 |
|---|---|---|
| LE | 写入、fsync、索引 sync、正常卸载 | 2.5.0 → 2.4.0，gen40 |
| tape-fuse | 完整 read、pread | 字节逐一相等 |
| tape-fuse | MAP_SHARED + PROT_READ mmap | 字节逐一相等 |
| tape-fuse | readdir、二进制 xattr、mtime 纳秒 | 正确；mtime=1790253484419335445 |
| Client | stat/stat_path | committed、65569 字节、LD2502L08、gen40；xattr/mtime 元数据正确 |
| Client | get/get_to/get_range | 全量、偏移、跨 EOF 截断、零长度均正确 |
| Client | list/list_dir/pools | 文件、长度、池 UUID 正确 |
| Client | 不存在路径、越界 Range | stat=None、get=NotFound、range=416 |
| Client | put_new、重复 put_new | 首次落带；重复 Exists，原内容不变 |
| Client | put、put_file(wait=true) | 覆盖、65,569 字节流式上传均落带，读回相等 |
| Client | delete、重复 delete | 删除落带，stat=None/get=NotFound；再次删除 NotFound |
| Client | 写接口测试后的 LE 文件 | 仍逐字节相等 |

LE 文件 SHA-256：`ff32fddb1023267c72225486e878b94ffed9b7f39b4d28e18966bec699c619c9`。
LE 没有为该文件添加内容哈希 xattr，所以 Client stat.sha256 为空，这是带上元数据事实；本次使用独立预期内容逐字节校验和外部 SHA-256，没有把空哈希当作校验通过。无 tapers.version 的 LE 文件版本为 (0,0)。

探针最初对 list 返回路径漏考虑前导 `/` 而断言失败；服务端返回正确的 `/rc/...` 和长度。修正探针比较后重跑通过，未修改生产代码。

Client 写测试在副本追加 gen41–44，正常停机生成 Full gen45。该副本随后清理。此次未重测故障切换、异步 put_file(wait=false)、reclaim、全部管理接口；也未证明 LE 能重放 2.5 增量链。LE 互操作边界仍是交接前的 Full 检查点。

## 恢复与证据

- FUSE 正常卸载，隔离三节点 SIGTERM 正常关闭，检查点、UNLOAD、PR 释放有日志。LD2502L08 从第二驱动移回原槽7后删除。
- 专用副本 LD2501L8、TS1001L7、LD2502L08 全部删除；没有删除 TS1001L08/TS1000L08。
- Holo 创建/删除介质会禁用此前手工发布的 EE2/EE3 changer targets；完成所有介质变更后，按原参数重新发布并恢复 ACL。最终 9 个 ready publication。
- 原集群 node1 leader、term9、round109；TS1001L08 gen39、尾部 Complete、约48 MiB可用。
- 原 `rc/_probe.bin` SHA-256 `49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7`。
- EE 三节点 available、无活动任务；原 `/gpfs/tapers-del/f3` SHA-256 `cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e`。

可重复探针：`examples/hw_client_interop.rs`（显式 endpoint 环境变量，写操作还需显式副本条码），`probes/le-downgrade-write.py`、`probes/le-downgrade-fuse-read.py`。远程脚本和隔离数据库保留为证据，不再运行；副本已删除，重跑需要重新创建隔离介质。

日志在 `probes/results/`：`ltfs25-le-downgrade-write.log`、`ltfs25-le-index-evidence.log`、`ltfs25-fuse-downgrade-read.log`、`ltfs25-client-{read,write}.log`、`ltfs25-interop-daemon.log`、`ltfs25-interop-{restored,cluster-restored}.log`。EE 恢复日志末尾有一次在错误主机调用 ltfsctl 的路径错误，已在 tsclient 重跑并独立记录成功结果。

## 软件回归

本轮最终 `cargo test`：172 项通过，1 项忽略。`cargo check --all-targets` 与 `cargo clippy --all-targets --all-features` 成功，保留既有 clippy 告警。新增探针 rustfmt 检查与 `git diff --check` 通过；全仓 `cargo fmt --all -- --check` 仍因已有多文件格式差异失败，未重排无关代码。工作树无 `.codegraph/`，未初始化索引。
