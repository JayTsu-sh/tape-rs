# LTFS recovery / LE interop GAP 补全与实机验收

日期：2026-10-14

分支：`ltfs-recovery-ibm-interop`（基线 `2f49b92`）

目标：补全 `rea-le-tape-rs-integrated-20261014.html` 识别的 P0/P1 gap，并同时提供直接 Rust library 与 FUSE 入口。

## 结论

生产实现、模拟测试、真实 FUSE、真实直接 Rust library、大于 64 MiB 索引，以及 IBM Spectrum Archive Library Edition（LE）只读互操作均已通过。没有执行 FORMAT 或 ERASE。AA0013L8 保留本轮 65 MiB+ xattr 大索引夹具，AA0000L8 保留独立内容文件。

LE 正常卸载后启动的最终 Rust 冷读遇到现场 FC 链路故障：`LOAD` 返回 SCSI host status `0x000d`，随后 host15/host16 均为 `Linkdown`，所有 tape/changer LUN 从 `lsscsi` 消失。该失败发生在索引读取前，不能归因于 LE/Rust 格式不兼容。链路恢复后仍需核实 AA0013L8 位置、PR，并完成最终 Rust 冷读与归槽。

## 实现

- `LtfsIndex::parse_reader` 复用原 XML 状态机，从 `BufRead` 流式解析；完整索引和增量索引的截断输入均拒绝。
- recovery 以单个磁带记录为缓冲，512 字节前缀允许跨短记录，不再保留整份 XML 原始字节；XML 错误后仍排空到构造边界，底层 TapeError 不被解析错误覆盖。
- 默认构造 I/O 上限由 64 MiB 调整为 8 GiB；显式小预算、反向步进和 chain 长度限制仍生效。文件树与单个 XML 事件仍占内存。
- 新增 `library::Library`、`LibraryBackend`、`tape-library` 与 `tape-fuse` 直接逻辑库模式。路径为 `/pool/barcode/relative-path`，支持显式 assignment、blacklist、`NEED_ASSIGN`、库存、失败状态、按需 MOVE/LOAD/ready/recovery、同卷 session 复用与正常归槽。
- changer 与 drive 均以 VPD/DVCID/element map 核验；所有设备资格通过 PR 管理。未知或未 assignment 的卷不会被驱逐，失去资格的实例不会重新 fence。
- FUSE 支持多卷文件读写、fsync、目录、metadata、二进制 xattr、symlink、同卷 rename；跨卷 rename 返回 `EXDEV`。原有 `--endpoint` 模式保持。
- FUSE 占锁期间，`tape-library status` 从 SQLite 的最近完整库存快照合并实时资源状态，返回 `source=cached_inventory`/`observed_at`；空闲时使用设备查询。
- IBM append-only WRITE MODE 1 下，IP checkpoint 在 PR guard 与精确当前位置校验后发送一次 action 1 `ALLOW OVERWRITE`。不发送 format 授权，不关闭 append-only，不吞 READ POSITION UA。
- 新增直接 Rust 实机验收和大索引夹具示例；所有真实动作均由独立 SQLite、显式设备序列号和单调 round 约束。

## 软件验证

| 验证 | 结果 |
|---|---|
| `cargo test` | 277 passed，2 ignored，0 failed |
| `cargo test --all-features` | 278 passed，9 ignored，0 failed |
| `cargo clippy --all-targets --all-features -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `git diff --check` | 通过 |
| 兼容构建（x86_64 glibc 2.34，FUSE/bins/examples） | 通过 |
| 内核 FUSE 多卷测试（SimTransport） | 通过 |
| 百万文件流式解析显式测试 | 通过，1,000,000 文件，19.201 s；仅软件测试 |

测试覆盖：65 MiB 有效索引在显式 64 MiB 预算下拒绝、默认预算下成功；7 字节短记录；截断 Full；append-only 一次授权、无 PR、错误位置、reset UA 不重试；assignment/blacklist/锁；未 assignment 介质不驱逐；ownership loss 不重新 fence；失败阶段持久化；两卷 CRUD/metadata/symlink/EXDEV。

## 实机环境

- 主机：`lisa-3` / `10.131.7.209`
- TS4300：`55L3A78011W9LL01`
- 验收 drive：HH8 `117018606F`
- 备用 TD8：`117018605B`
- 资源库：`/root/tape-rs-gap-20261014/library.sqlite`，node 250
- 数据卷 assignment：AA0013L8、AA0000L8、8A0043L8、L00284L8、8A0047L8、L00285L8、8A0042L8；CLNU01L8 未 assignment
- 原文件：AA0013L8 `/perf64m.bin`，67,108,864 bytes，SHA-256 `2009f414fa6abf46885f6701a5f454b3f5af29afc1ff47b99bd4f42719d0d8c3`

## 实机结果

| 场景 | 结果与证据 |
|---|---|
| 基础直接 Rust library | AA0013L8/AA0000L8 两卷、单 drive、两轮 SHA；通过，正常归槽 |
| append-only IP 修复 | 原 v1 在 `07/5A/02` 停止；v2 精确 `ALLOW OVERWRITE` CDB `[82,00,01,00,00,00,00,00,00,00,00,04,00,00,00,00]` GOOD；只补 IP，未重放 append |
| 真实 FUSE | 7 卷 namespace；AA13→AA00→AA13 冷读；64 MiB SHA；写/fsync、binary xattr、rename、relative symlink、跨卷 `EXDEV`；通过 |
| FUSE 运行中状态 | 两次均约 2 ms 返回 `cached_inventory`，序列号和时间戳正确，不打开设备 |
| 管理状态 | blacklist 拒绝、clear、unassign 后 `NEED_ASSIGN`、reassign；通过且不装带 |
| 65 MiB+ 实际索引 | AA0013L8 追加 1040 × 65,536-byte root xattr 与 marker；旧版在 67,108,864-byte budget 明确失败；新版 `list` 72.918 s 成功 |
| 大索引直接 Rust | 两卷四个不同文件，两轮 SHA，613.763 s；通过 |
| 大索引 FUSE | marker、65,536-byte root xattr、两卷冷读和 64 MiB 文件；通过，正常卸载 |
| IBM LE | 列出 7 卷；AA0013L8 大索引 marker 与 64 MiB SHA 正确；65,536-byte xattr 正确；普通卸载 rc=0、进程 exit=0；LE 记录卷未 dirty/self pointer 位于 IP，跳过索引写入 |
| LE 后 Rust 冷读 | 未完成：首次 LOAD 时 FC devloss，尚未进入索引读取 |

## 现场故障与恢复

大索引 DP commit（gen 8 @ DP 157）后，原 changer 路径 `/dev/sg5` 因 lpfc devloss 消失。备用 `/dev/sg3` 的序列号仍为同一 TS4300；使用原 round/key 只读取大索引 marker 与原 64 MiB 文件两轮，然后 checkpoint、归槽、释放 PR，结果通过。之后所有大索引 Rust/FUSE/LE 验收均在备用路径完成。

LE 完成后，Rust 最终冷读的初始库存显示 AA0013L8 位于 DTE offset 1；后续 LOAD 在约 40 秒后返回 host status `0x000d`。HH8 和 changer 随后同时 ENXIO，内核对 WWPN `50:00:e1:11:70:18:60:70` 记录 devloss。到 16:35，host15、host16 仍均为 `Linkdown`，没有 tape/changer LUN。没有进程或 FUSE 挂载。由于设备不可见，当前不能确认 LOAD 的物理结果或 PR，也不能执行归槽。

## 链路恢复后的唯一剩余步骤

1. 只读确认 FC host 为 Online、重新按 VPD 序列号解析 sg 节点，不复用旧编号假设。
2. 查询库存和 HH8/changer PR；用失败 round `202610140130` 接续并归槽 AA0013L8，或在无旧注册/holder 时使用新 round。
3. 新 round 运行 `library_acceptance`，两轮校验大索引 marker、AA0013L8 FUSE 文件、原 64 MiB 文件、AA0000L8 独有文件。
4. 确认两 drive 空、AA0013L8/AA0000L8 回 home slot 1/2、无 PR key/holder、无 FUSE/LE 进程，并记录 append-only mode。

证据位于 `.scratch/ltfs-ha/probes/results/gap-implementation-20261014/` 以及远端 `/root/tape-rs-gap-20261014/results/`。失败日志与成功接续均保留，阶段门控没有删除或重放。
