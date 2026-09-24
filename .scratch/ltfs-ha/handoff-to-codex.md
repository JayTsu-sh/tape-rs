# 交接给 Codex（2026-09-24）

此前在 Claude Code 里开发，之后改用 Codex。本文件把只存在于 Claude 本地记忆里的约定搬进仓库，并记录交接时的施工状态。

## 工作约定（原 Claude 记忆）

- **没有真实带库。** 截至 2026-09-17，CLAUDE.md 里 10.128.54.118 的 TS4300 不可用。唯一设备是 PVE 实验室里的 Holo-VTL（入口 PVE 10.131.9.20；VM：holo-vtl .70、ee1 .71、ee2 .72、tsclient .73、ee3 .74；tape-rs 在 tsclient 上通过 iSCSI 访问自己的逻辑库 TAPERS）。不要规划依赖真实驱动固件的工作，不要问 FC/SAS 拓扑问题。旧笔记里的"硬件验证"都按"Holo 验证"理解；依赖固件语义的内容记到 `real-hardware-checklist.md` 的延后清单，不报告为已验证。Holo 的偏差一经发现就在其源码（10.131.9.10:/work/jay/env/Holo）修掉。IBM Spectrum Archive EE 集群与 tsclient 上的参考 LTFS 2.4.8.3（`/opt/ltfs-ref`）是检验 Holo 和 tape-rs 的依据。
- **验证顺序。** 用户原话："GPFS + EE -> 推动校准 HOLO VTL（因为没有物理带库）；GPFS + EE + 校准 HOLO VTL >> 推动自研软件研发"。每个切片：先在实验室观察 EE 的行为 → 检查/修正 Holo 的偏差 → tape-rs 实现 + 模拟器测试 → Holo 实验室演练；每一步记在 `.scratch/ltfs-ha/`。
- **自主推进。** 用户原话（2026-09-23）："接下来，你参照EE一步一步的实现定义好的功能，我不再给你输入了，如果要决策的都按照默认推荐走"。已定义的功能（`issues/` 里的票据，如 05 文件契约）不停下来问；遇到设计选择取推荐项，把决定和理由写进票据/笔记，逐片推进。测试与演练结果照实报告。

## 交接时的施工状态

分支 `ltfs-recovery-ibm-interop`。最后一个完整切片是 `d6b59dd`（F1：create-only 上传、在途状态、Range 读、目录列举），全部测试通过。

其后的提交是**未完成**的 05 文件契约下一片，原样推送，**不能编译**：

- 删除（墓碑）：`ltfs::volume::{TOMBSTONE_DIR, tombstone_path, XATTR_DELETED_PATH}`、`LtfsVolume::remove_file`、`FileRec.deleted`（日志里旧条目解为文件）、`Directory` 的墓碑行与 `lookup`、`VolumeState::admit_removal`、`FileService::{delete, lookup, relocate_tombstone}`、回收搬迁墓碑（`executor.rs::copy_tombstone`）。
- 客户端：`Client::{put_file, get_to}`（流式，不整个读进内存）、`sha256_file`。
- FUSE 适配器：`src/tapefs.rs`（逻辑层，不依赖 fuser，可脱离内核测试）；`src/bin/tape-fuse.rs` 还是空的 `main`，需 `--features fuse`（`fuser` 0.18，不用 libfuse）。
- 已知编译错误：`src/daemon/http.rs:141` 的 `match` 缺 `ServiceError::NotFound(_)` 分支（HTTP 删除接口还没接上）。

下一步：接上 `DELETE /files/<path>` 与客户端/ltfsctl 的删除，补齐 tape-fuse 的挂载层，然后按上面的验证顺序做 EE 观察与 Holo 演练。
