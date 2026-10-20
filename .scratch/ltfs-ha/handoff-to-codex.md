# 交接给 Codex（2026-09-24）

最新接续摘要见 [docs/CLAUDE-HANDOFF.md](../../docs/CLAUDE-HANDOFF.md)。2026-09-27 已部署符号链接回收修复并通过 Holo 正式集群回归；下列旧版本和“尚未部署”描述按末尾最新记录更新。

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


## Codex 接续记录（2026-09-24）

已在现有未提交修改上接续 F2/F3：接通真实 FUSE 入口，修复打开读句柄在替换/删除后失效，增加删除响应丢失不重放、墓碑回收接管测试和显式内核挂载测试。使用指南与剩余限制见 `docs/file-client.md`；本记录不把 F3 标为完整验收。

本轮实验室仅经 SSH 读取 tsclient 主机名和 EE `node list`，三节点均 available；未部署、写带或改变服务状态。没有重新执行 EE 删除/覆盖观察与 Holo 演练。当前 worktree 无 `.codegraph/`，按约定跳过索引，不新建索引。

验证结果：`cargo test` 145 项通过；`cargo check --all-targets` 通过；`cargo clippy --all-targets --all-features` 完成，保留既有告警，新增挂载入口无告警；显式执行 `kernel_file_operations --ignored` 通过，内存后端在本机内核 FUSE 上完成创建/fsync/替换/删除后旧句柄读取/目录/EXDEV 验证。`cargo fmt --all -- --check` 仍有大量基线差异，新挂载入口的 rustfmt 检查通过；未批量格式化原有文件。`git diff --check` 通过。改动未提交。

### 后续：大文件下载（2026-09-24）

已完成 HTTP 下载移除 256 MiB 上限：`ReadTo` 把磁带内容分块写入暂存目录下创建后立即 unlink 的私有文件，HTTP 在线程外接手句柄流式发送。客户端慢读不占设备线程；内存不随下载大小增长，临时磁盘仍需容纳整个文件，Range 仍读全文件后从磁盘切片。内部兼容用 `Read` 仍返回 Vec，HTTP 不再走它。

257 MiB 测试发现并修复等待落带返回 202 被客户端当作拒绝的问题：现在继续查询，只上传一次。底层读取提前遇到文件标记或长度不符会失败，HTTP 读取失败返回 500 而非 404。

验证：全量 `cargo test` 148 项通过，包含 257 MiB 上传/下载逐字节校验、202 后查询不重放、提前文件标记拒绝短读；`cargo check --all-targets` 与 `cargo clippy --all-targets --all-features` 完成，告警数量与上一轮一致；`git diff --check` 通过。全量 fmt 仍存在历史差异，未批量重排原有代码。当前工作树仍无 CodeGraph 索引，未初始化。本轮没有远程访问、部署或设备操作，改动仍未提交。

下一步仍是 F3 索引时间/version/任意索引 xattr 透传与 EE/Holo 联合验收；按范围定位读取的性能优化尚未做。

### 后续：文件元数据透传（2026-09-24）

索引修改时间和原始 xattr 已通过 CatalogPart → SQLite files/staging.metadata → FileService → HTTP stat → Client → FUSE 贯通；原有版本号也暴露为 user.tape.version。新增 base64 0.22 用于解码二进制 xattr，损坏返回 EIO，内置属性优先。目录日志分片计入 metadata 大小。旧日志和旧缓存 metadata 默认为 null，版本列保留；不主动装带补齐历史未知值。建议统一升级，新旧混跑/回退不保证新增元数据完整性。

验证：`cargo test` 151 项通过，新增旧库迁移、mtime/文本/二进制/损坏 xattr、目录发布及接管一致性测试；内核 FUSE 显式挂载测试通过；`cargo check --all-targets`、全特性 Clippy 完成（已清理本轮新增告警，既有告警保留）；全量 fmt 仍有基线差异，未批量格式化。没有 CodeGraph 索引，未初始化；未远程访问、部署或写带，改动未提交。

下一步明确缺口：回收复制路径仍重建文件属性，需要保留源文件的修改时间和自定义 xattr；之后继续实际 mv/mmap 与 EE/Holo 联合验收。旧库迁移不自动补齐历史 xattr，未知时间仍回退 epoch。

### 后续：回收保留源文件属性（2026-09-24）

已修复上轮发现的缺口：内部回收任务携带源 NodeMeta 和原始 xattr，卷写入保留源时间/只读标记/属性编码，重建新卷 UID、extent、版本号，并按实际内容重算已有哈希。普通上传路径不接受内部回收属性；墓碑流程和源带格式化前的双重检查不变。

验证：先用集群测试复现 mtime 在回收时变化，再修复；全量 `cargo test` 153 项通过，包括回收后属性/内容/版本、源带清空与接管一致性，以及卷级空文件/二进制属性/只读/UID/哈希验证。`cargo check --all-targets`、`cargo clippy --all-targets --all-features` 完成，告警数量与前轮一致；全量 fmt 仍有基线差异，未批量格式化；`git diff --check` 通过。未访问远程主机、部署或写真实设备；改动未提交。

下一步：实际 mv/mmap 的 FUSE 兼容性与 EE/Holo 联合验收；Range 直接读取所需磁带块仍待优化。

### 后续：实际 mv/mmap 与实验室基线（2026-09-24）

本机测试先复现只读共享 mmap 返回 ENODEV，再按内核 FUSE 文档补充可选 FUSE_DIRECT_IO_ALLOW_MMAP 协商；普通读写不因旧内核缺少该能力被禁用。显式内核测试验证 mmap 内容、拒绝读写混合打开、GNU mv 的复制删除回退，以及目标暂存 ENOSPC 时保留源文件。测试不把 close/mv 成功当作归档成功。

验证：默认 `cargo test` 153 项通过；两个显式 FUSE 内核测试通过；`cargo check --all-targets`、全特性 Clippy 完成，既有告警保留；新挂载入口 rustfmt 检查和 git diff --check 通过，全仓库仍有历史格式差异。

实验室仅只读查询：EE 三节点 available、无活动 EE 任务；tsclient 有 /dev/fuse 和 fusermount，无 fusermount3，非交互 PATH 没有 cargo/rustc；内核 5.14、glibc 2.34。EE1 仍有旧 ltfsd 在运行，status.json 自述管理池 rc 的 TS1000L08/TS1001L08，不把该文件当实时库存。未部署、停服或接触磁带。下一步是准备兼容实验室 glibc 的构建、核对既有 ltfsd 与受管介质，再完成 Holo 联合演练；所有改动仍未提交。

### 后续：兼容构建与 Holo 状态解析（2026-09-24）

用户提供 Holo 源码位置 `/work/jay/env/Holo`。该仓库已有多项未提交修改，本轮保留原改动，仅修改控制面 resources_handler.go 及其测试：按数据面的首行约定读取 cartridge，忽略后续 load_seq。新增用例先稳定复现错误，再修复；覆盖旧格式、带 load_seq、空带和 CRLF，并通过 REST reconcile 用例验证。`go test ./...`（control-plane）全通过，`git diff --check` 通过。修复尚未部署。

发现实验室 Holo 的 drives 接口曾返回 `TS1001L08\nload_seq=1790047505480547633` 为 mountedCartridgeId。注意该 GET 会调用 reconcileMediaState 并可能写入控制面数据库，因此库存 GET 不应称为严格无持久化副作用的只读检查。未执行磁带写入、机械操作、服务替换或停服。

已完成兼容构建：`CFLAGS_x86_64_unknown_linux_gnu='-mcpu=baseline' cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 --features fuse --bins`。使用环境覆盖避免仓库 CFLAGS 的 mtune=generic 与 Zig 不兼容，未改构建配置。产物位于 `/work/cargo-target/x86_64-unknown-linux-gnu/release/`；ltfsd、ltfsctl、tape-fuse、tape-rs 的 readelf 最高 GLIBC 依赖均为 2.34。SHA256 清单保存在 `/tmp/tape-rs-lab-build-manifest.json`，尚未完成远程执行验收。

实验室旧 ltfsd 三节点仍运行，node1 为 leader/executor，term=3、round=35、各节点 applied_index=46；池 rc 管理 TS1000L08/TS1001L08，目录包含 _probe.bin、after_reclaim.bin、f0.bin 至 f5.bin。不得把旧状态或 Holo 缓存容量计数视为新鲜 SCSI 库存。下一步仍为受控升级与 EE/Holo 联合演练；Range 按磁带块优化仍未做。所有改动未提交。

### 后续：Holo 部署与 FUSE 现场验收（2026-09-24）

已统一升级实验室三个 ltfsd 节点并保留升级前状态/程序备份。新客户端在 tsclient glibc 2.34 运行；Holo 上完成 fsync 落带、覆盖/删除后旧句柄读取、元数据、GNU mv 回退、HTTP Range 和计划接管验收。原有 8 个文件全部读回并核对索引 SHA256。节点 2 为 leader/executor（term5 round61），节点 1/3 正常跟随；FUSE 已卸载。确实执行了 Holo 写带、UNLOAD 和预留操作；无物理设备。

现场限制：Rocky 5.14 的只读共享 mmap 返回 ENODEV；mv 保留权限/时间有告警。EE 删除/覆盖现场对照、跨带回收现场验证、写入中故障和 Range 块级优化仍待完成。Holo 控制面解析修复尚未部署。详细证据和路径见 [Holo 文件客户端现场验收](holo-file-client-20260924.md)。

### EE 页缓存调研；暂停 tape-fuse 改造（2026-09-24）

用户最新指示为“先不要改 tape-fuse，先做 EE 调研”，所以尚未实施内核页缓存改造，不应自动继续之前的代码修改请求。本轮只新增调研笔记，Rust 代码和远程服务未改。

现场 /gpfs 是 GPFS，EE 内部 /ltfs 是 LE FUSE。已安装 LE 2.4.8.3 的 open/create 回调经静态反汇编确认 direct_io=0、keep_cache=1，与参考源码一致；运行参数没有 direct_io。此证据不是实际 OPEN 报文追踪或 mmap 实测，也不能把 keep_cache 当作 writeback-cache。详见 [现场核对](ee-fuse-cache-lab.md) 与 [官方资料](ee-fuse-cache-sources.md)。

### 现场 LE mmap 已验证（2026-09-24）

按用户明确要求，在 ee1 /ltfs 的 EE8005L8 上读取已有 4 MiB f3 副本。MAP_SHARED/PROT_READ、MAP_PRIVATE/PROT_READ、与 pread 逐字节比较、关闭 fd 后映射读取全部通过，GPFS 副本 SHA256 一致。内核同为 Rocky 5.14。

收尾 LE unassign 删除卷缓存，曾使 EE 报 file state=missing；使用 EE tape validate（任务1114成功）恢复正常 premigrated。最终介质原槽、驱动空闲、无任务，保留 EE validate 建立的 LE ASSIGNED/缓存状态。详见 ee-fuse-cache-lab.md；未修改 tape-fuse，仍按用户要求暂停改造。LOGSENSE/TapeAlert兼容告警另待调查。

### LE 可见性实测（2026-09-24）

按用户要求继续调研，使用 EE8005L8 独立临时目录小量写入：fsync/close 前另开读 fd 即可见数据；同 inode 覆盖及 O_TRUNC 影响旧 fd/映射；unlink 后旧 fd 保留内容，同名重建分配新 inode。证明现场 LE 不是覆盖时每句柄快照语义，支持先前建议的共享文件对象方向，但尚未授权恢复 tape-fuse 改造。

测试目录已删除，介质正常卸回原槽，不再 unassign；原 EE f3 哈希和 premigrated 状态正常。记录见 ee-fuse-cache-lab.md。此次有虚拟带写入和索引更新，total_blocks65→73，valid_blocks8；未做回收。fsync 的完整磁带提交边界、远端更新和多节点一致性仍未现场证明。

### 根据 LE 语义完成页缓存实现（2026-09-24）

用户已明确恢复实施。tape-fuse 改为内核页缓存 + write-through，不设置 FOPEN_DIRECT_IO、不依赖 DIRECT_IO_ALLOW_MMAP、不启用 writeback-cache，暂不设置 KEEP_CACHE。逻辑层同一 inode 共享本地文件；写入在本挂载 fsync/close 前可见，覆盖/截断影响旧句柄；unlink 保留旧引用，同名重建新 inode。仅暂存的文件额外保留本地副本，避免 close 后重开退回旧版本；后续打开确认已提交/替代/消失时解除保留，无后续访问则可能留到卸载。user.tape.state=local 区分尚未上传的本地数据。写句柄 fsync 等待磁带提交的保证保留。

远端一致性采用保守边界：已有缓存引用时检测到不同的已提交内容，新 open 返回 ESTALE；已有引用保持当前内容，全部关闭/解除映射后重开刷新。首次 stat/GET 的长度/已有 SHA256 不一致返回 EAGAIN，无 SHA256 的旧记录额外核对前后 stat。没有分布式实时缓存一致性、共享可写 mmap 或跨挂载缓存复用。顺序写、O_WRONLY 和整体替换限制保留。

验证：默认 cargo test 157 项通过；3 个显式本机内核 FUSE 测试通过，覆盖 mmap、截断/覆盖、删除重建、fd关闭后映射、mv和上传失败。cargo check --all-targets、cargo clippy --all-targets --all-features 完成，既有告警数量保留，src/tapefs.rs 和新入口无告警。全量 cargo fmt --check 仍报历史格式差异，未批量格式化；入口 rustfmt --check 和 git diff --check 通过。工作树没有 .codegraph，未初始化。

现场只部署 tsclient 的新 tape-fuse，三节点 ltfsd 未重启。最终客户端 `/home/rocky/tape-rs-codex-20260924/tape-fuse` SHA256 为 `4acb0a8e7629de82b0dfd6127ec5bf47148058d79f403e36690e4b67bdf0f7ac`，旧 direct I/O 程序备份为 tape-fuse.direct-io。Rocky 5.14 + Holo 实测通过共享只读 mmap、fsync前可见、截断、删除重建及关闭fd后继续映射，旧 ENODEV 已消除。外部 ltfsctl 覆盖验证了 ESTALE/映射内容一致/全部释放后刷新；暂存副本被替代后的释放补充由单元测试覆盖。

现场测试在 rc 池独立目录 codex-cached-20260924 和 codex-cached-final-20260924，已删除测试文件/目录并卸载 mount-cached，cache-cached 为空；删除会留下墓碑、并非回收物理空间。原 _probe.bin 哈希仍一致；节点2 term5 round61继续服务。本轮有Holo写带，无物理带库操作。所有源码改动仍未提交。

### LOOKUP/OPEN 长度变化保护（2026-09-24，实验性实现，已由下一节撤销）

补充页缓存边界：此前 LOOKUP 返回旧长度后，外部客户端若改变长度，首次 OPEN 会下载新内容并成功，但 OPEN 回复不包含新属性。现在记录 LOOKUP/GETATTR 已返回的长度，首次无本地引用的只读 OPEN 发现长度变化时返回 ESTALE，要求刷新属性再打开。本挂载写句柄释放时记录最终长度，正常覆盖后的重开不误报。此保护不提供跨客户端实时一致性，也不把属性回复与远端修改变成原子事务。

测试先复现旧代码错误（预期 ESTALE、实际 Ok），修复后增长/缩短、刷新后恢复、本地覆盖直接重开均通过。默认 cargo test 共159项通过，3项显式本机 FUSE 内核测试通过；cargo check --all-targets 与 cargo clippy --all-targets --all-features 完成，tapefs/tape-fuse 无告警，其他既有告警仍在。cargo fmt --all -- --check 仍因仓库历史格式差异失败；git diff --check 通过。日志 /tmp/tapefs-size-{red,green,tests,kernel,check,clippy,fmt}.log。工作树无 .codegraph，未初始化。

本次仅本地源码/文档与软件验证，没有远程部署或设备操作；现场仍运行上一节记录的客户端二进制，尚未包含本次长度保护。源码改动未提交。


### 实际内核验证及长度保护修正（2026-09-24）

新增显式 ignored FUSE 挂载测试 `kernel_remote_resize_between_lookup_and_open`：内存后端在返回第一次旧 stat 后立即替换内容，覆盖 8192→16384 与 8192→1024；应用不调用 fstat/read_to_end，直接逐块 read。临时移除上节长度保护后，测试仍通过，本机内核没有复现上节推测的截断/补零。上节单元测试只验证“必须报 ESTALE”的人为规则，不能证明内核会读错。因此撤销新增 sizes 表及首次 OPEN 长度限制；保留下载内容校验和已有引用冲突的 ESTALE 规则。单位测试改为验证无本地引用时下载新版本、getattr 返回新长度。不能把本机结果外推为所有内核/并发时序的完整证明。

新增 `kernel_remote_replace_preserves_mapping_until_release`：Python 建立只读共享映射后，由测试进程直接替换内存后端；新 open 必须 ESTALE，旧映射内容不变，关闭 fd 后映射仍有效，解除映射后重新打开得到完整新内容。测试同步使用管道，不操作磁带。

最终验证：默认 cargo test 159项通过，显式本机 FUSE 测试5项通过；cargo check --all-targets 完成；最终 clippy --all-targets --all-features 完成，tapefs/tape-fuse 无告警，保留其他既有告警。入口 rustfmt --check 和 git diff --check 通过；全量 fmt 仍有历史差异。日志 /tmp/tapefs-kernel-followup-*，反向验证 /tmp/tapefs-resize-kernel-without-guard.log。没有远程部署、设备操作或提交；现场二进制仍是页缓存实现章节记录的版本。

### LE 全部声明能力摸测与写入扩展（2026-09-24）

用户先明确要求“没有对齐的功能要对齐”，随后追加要求先实际验证 LE 只读 mmap，并在 Holo 摸测其声明支持的能力。原始完整对齐目标仍有效，不能把本节局部实现称为已完成全部对齐。

使用 research 技能，后台 agent 仅查主来源/参考源码，产出 le-alignment-details.md。重要结论：官方 mmap 范围只有 PROT_READ；rename 必须保持 inode 并支持目录持久化；fsync 数据flush与索引提交分离，不能用HTTP staging冒充数据落带。

现场在 ee1 /ltfs、LISA4300/EETEST/EE8005L8、IBMlisa41D6C 测试，完整记录 le-capabilities-holo-20260924.md，可重复脚本 probes/le-capabilities.py 和原始结果 probes/results/le-20260924-*。只读共享/私有 mmap、随机/向量I/O、dup、>4GiB稀疏定位、truncate、多个writer和两个进程128条并发追加、文件/目录rename、打开文件unlink、符号链接、xattr、统计等通过；正常重挂载验证内容/空目录/链接/xattr持久化。chmod0400后root仍能写是实际观测，初版测试误设“root也必须拒绝”的FAIL原样保留，随后完整补测权限。chmod0777最终0600，chown被忽略；statfs容量字段为0xffffffff，不证明准确容量。fdatasync/fsync/sync不增加索引代数（6），user.ltfs.sync增加为7，重挂载8。umount/umount2只在独立递归private挂载命名空间对子进程副本测试成功，宿主EE挂载保持不变；未测试销毁主LE FUSE会话或断电恢复。

已删除测试目录并正常卸载，不unassign；最终槽1030，ASSIGNED/UNMOUNTED/WRITABLE、无活动任务，原f3为premigrated。LE和GPFS原文件SHA256仍 cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e。total_blocks73→102、valid_blocks仍8。本轮有Holo虚拟写带/索引更新，没有物理磁带操作。

本地代码增量：TapeFs支持O_RDWR、非截断写、任意偏移、扩大/缩小truncate；多个写句柄引用同一个Handle状态，避免独立dirty/upload状态互相覆盖。O_APPEND偏移由内核write-through处理。release与open通过inode slot串行更新canonical writer句柄。路径truncate无writer时临时打开、上传。适配器create解除O_WRONLY限制，内核回归新增多writer/追加/随机写/truncate断言。fsync仍是此前较强的归档保证，KEEP_CACHE、rename、权限/xattr/符号链接及空目录持久化尚未对齐；后续必须继续，不能只更新文档了事。

验证：默认cargo test159项通过；最后release锁调整后16项tapefs单测和5项真实本机FUSE测试复跑通过。cargo check --all-targets通过；最终clippy全targets/features完成，tapefs/入口无新增告警，既有lib25/libtest29等仍在。入口rustfmt和diff-check通过；全量fmt仍有历史差异。日志 /tmp/le-alignment-*.log。无.codegraph，未初始化。客户端/服务端均未部署，本轮所有改动未提交。

### 2026-09-24：LTFS 2.5.1 增量与检查点策略已接入

- 普通 ltfsd 上传/删除批次调用 `commit_incremental`：DP 标准 `ltfsincrementalindex`（XML version 2.5.0），IP 保留此前 Full。
- 连续最多 5 份增量，第 6 次有修改的提交写 DP/IP Full；若 mount 的 `max_chain_len` 更小则采用该值，0 禁用增量。计数从恢复链取得，重新挂载不会清零；无变更提交不消耗次数。
- 正常停机、写入带切换、单驱动器临时换出写入带，先以当前预留持有者写 Full，再允许 UNLOAD/MOVE。只对当前接纳的写入卷执行检查点，先核对库存条码，不对拒绝候选或外来介质执行写入。停机/检查点提交失败保留恢复路径，不能当作成功卸载；读请求触发的检查点失败也放弃本轮。
- 计划停机先 drain：拒绝新上传、删除及晚到的 finish，保留已经完成的队列，批次提交后再生成检查点并 close。原 close 在 flush 前丢弃队列的问题已修正。
- 模拟介质新增覆盖：跨挂载累计上限、小恢复预算、零预算、周期 Full 的 IP 写失败冻结、drain 保留已完成上传。集群正常停机断言 DP/IP 同代 Full；测试介质检查辅助函数使用副本恢复，不能再只找最后 Full。
- 尚未部署到 Holo；现场 LE 2.4.8 不是增量恢复 oracle。FUSE 命名空间操作接线等之前列出的 LE 差异仍待完成，本条不代表全面对齐。

本次验证：`cargo test` 全仓通过（171 项，另 1 项忽略），随后新增 drain 回归测试并运行 `cargo test --test sim_incremental`，9/9 通过；最终 `cargo check --all-targets`、`cargo clippy --all-targets --all-features` 均通过，clippy 保留既有告警。`git diff --check` 通过。`cargo fmt --all -- --check` 因现有多文件格式差异未通过，新增测试模块已 rustfmt，未整仓重排。当前 worktree 无 `.codegraph/`，未初始化索引。未接触真实硬件、未部署到 Holo。

### 2026-09-24：Holo 实测与新版集群部署完成

见 `holo-ltfs25-incremental-20260924.md`。TAPERS/TS1001L08 起点实际 gen21，5 次 DP 增量 gen22–26，第 6 次自动 Full gen27；删除目录增量 gen28，正常卸载 DP/IP Full gen29。逐次直接解析带上 XML 并重挂载读回，原有14条目与哈希全部验证。新版三节点上传 gen30，正常停机产生同代 DP/IP Full gen31，重启读回哈希一致；HTTP 清理产生 gen32 增量，集群正常运行（node2 term7 round97）。无格式化/擦除。新增门控 example `hw_incremental`，日志已入 probes/results。此轮只升级 ltfsd 和 tsclient工具，没有替换 tape-fuse；前述剩余 LE/FUSE 对齐项继续有效。新启动脚本在各节点 `/home/rocky/tape-rs-ltfs25-20260924/start.sh`，旧脚本仍指旧程序。备份在 `/home/rocky/ltfsd-backup-ltfs25-20260924`，不可当作最新磁带状态直接回滚。

### 2026-09-24：共享 Holo VM 断电、LE 降级回写及 Client/FUSE 实测完成

详见 `holo-powercut-20260924.md` 和 `holo-le-downgrade-client-20260924.md`。VM510 hard stop 后，TS1001L08 恢复 ACK gen36 的3份增量，数据/元数据及原15条目全部通过；清理后原卷gen39 Full。不是物理宿主或磁带断电。

实际LE在关闭副本TS1001L7上回写，2.5.0→2.4.0 gen40，DP/IP XML creator均为IBM LE。再复制到TAPERS/LD2502L08，隔离三节点接纳；全新tape-fuse缓存实际read/pread/MAP_SHARED只读mmap/xattr/mtime/readdir全部通过。Rust Client stat、全量/流式/范围读取、列表、404/416、创建/冲突/覆盖/流式上传/删除通过；未重测故障切换/异步上传/reclaim。LE文件无哈希xattr，stat.sha256为空，实际按预期字节和外部SHA256校验。

本轮仅新增门控example和证据，没有生产接口改动；一次性daemon入口只通过现有Admin命令登记原池UUID，patch保存在probes，避免修改LE保留xattr。原集群数据库未更改，测试服务现已停止，临时副本已删除。创建/删除介质导致EE手工target禁用的Holo副作用已恢复，9个ready；EE三节点available无活动任务。原ltfsd节点1 leader/term9/round109，原卷gen39，原探针和EE f3哈希均一致。软件回归172通过/1忽略，all-targets check和all-features clippy成功（既有告警），全局fmt仍有基线差异，新增探针fmt/diff-check通过。全部改动未提交；之前FUSE功能差距列表仍有效。

### 2026-09-24：LE → FUSE → LE 往返发现覆盖写丢 xattr

详见 `holo-le-roundtrip-20260924.md`。实际LE写2.4.0 gen3；FUSE随机写/truncate/追加/新建+fsync产生增量gen4–6，停机Full gen7；LE成功读取2.5.0，三个文件内容/mtime、空目录和符号链接正确，但edit.bin原二进制xattr丢失，整体兼容验收失败。根因：FUSE put_file→HTTP finish(metadata=None)→普通append新建节点，未继承旧xattrs。未修改生产代码；下一优先项应修复FUSE覆盖的元数据继承，兼顾跨带覆盖及哈希重算，不要将本轮结果标为全通过。

测试副本TS1000L8（LISA槽1032，LE unassigned）和RT2502L8（TAPERS槽1030，原集群未归属）保留供复测；LE原始gen3数据备份Holo `/home/rocky/holo-powercut-20260924/roundtrip-le-before`。隔离服务目录tsclient `/home/rocky/tape-rs-roundtrip-20260924`，已停止，FUSE已卸载。原集群恢复node3/term10/round112，TS1001L08 gen39；EE三节点available、9个ready targets、原探针与f3哈希一致。切勿在原集群中归属测试副本（其卷UUID与原TS1000L08相同）。本轮无格式化、无断电、无物理磁带操作。

### 2026-09-24：覆盖写 xattr 丢失已修复并通过 LE 往返复验

`FileService::finish` 在路径预留期间从当前目录版本继承xattrs，覆盖新时间/版本/内容哈希重算，通过既有FileMetadata落带；跨带覆盖同样支持。stat不再吞掉目录读取错误，未知/损坏xattrs拒绝有损覆盖并abort释放预留。普通Client PUT同样保留xattrs，无协议/持久化字段变化；旧metadata=null的catalog记录需先对账。尚未补齐之前列出的其他FUSE功能差距。

软件测试174通过/1忽略，check all-targets与clippy all-features成功（既有告警）。新增覆盖同带/跨带文本及二进制属性、SHA/MD5重算、并发预留、目录故障清理。Holo用原LE gen3副本和修复测试服务重跑，DP/IP gen7 Full与LE回读均保留edit.bin二进制xattr，内容/mtime/空目录/链接正确，详见roundtrip报告末尾及xattr-fixed日志。原集群仍是旧二进制，已恢复node3/term11/round115、TS1001L08 gen39；EE三节点available、原校验文件不变。两盘副本仍回槽保留，测试服务目录 `/home/rocky/tape-rs-roundtrip-fixed-20260924` 已停机；不能直接启动它与原集群争抢设备。全部代码未提交。

### 2026-09-24：原三节点已统一升级，跨主机故障切换验证通过

详见 `holo-xattr-upgrade-ha-20260924.md`。新ltfsd SHA256 `59faf2c49915f3ce22772efd9b5ce8fff028c2c9b697a91be7bbb969e5a8ec69`，原三节点实际/proc/exe均验证一致。发布目录 `/home/rocky/tape-rs-xattr-ha-20260924/`；原 `tape-rs-ltfs25-20260924/start.sh` 已指向新版，原data-dir不变，离线旧数据/二进制/脚本备份在各节点新目录pre-upgrade.tgz。

使用.71/.72/.74各自发起方、7500/7501及独立test-data，仅接纳RT2502L8副本；实际正式二进制运行测试。SIGKILL Leader三次：4MiB上传转发64KiB后中断，Client两次尝试成功；FUSE fsync gen10提交响应丢失，接管后查询确认成功且未重复提交，独立下载+xattr正确；DELETE gen11响应丢失，返回Indeterminate不重放，随后同名新版本安全。三个节点追平applied29/gen12，停机Full gen13、释放PR、副本归槽7。代理/FUSE/测试服务均停止；测试数据保留。

原集群现运行新版node3/term13/round121，TS1001L08仍gen39，原探针与EE f3哈希不变，EE三节点available无活动任务。最终软件174通过/1忽略、check/clippy成功（既有告警）、全局fmt基线差异仍在。本轮没有Holo断电/格式化/介质创建删除。旧条目“原集群尚未升级”已被本条取代。LE/FUSE剩余命名空间功能尚未实施；全部代码未提交。

### 2026-09-24：目录接入前先修复父子路径准入

本轮没有实现持久化 mkdir/rmdir 或 rename。检查服务端到 FUSE 链路时发现父子路径可并发准入，先修复此前置问题：files.rs 对上传/删除/回收墓碑做分量级在途互斥，结合 Directory::namespace_files 的跨带相关路径与当前卷 recent/原生目录节点阻止文件和目录类型冲突，NUL 在准入前拒绝。目录查询失败拒绝写入；查询完成后核对 VolumeState 指针，覆盖同 round 换带的情况。新增 409 is_directory/not_directory，FUSE 映射 EISDIR/ENOTDIR。无持久化格式变更。

新增 tests/file_namespace.rs 5 项，加三节点跨磁带/接管 HTTP 集成测试；旧覆盖元数据故障测试调整为准入后让目录库失效，仍验证 finish 失败清理。cargo test 180 通过/1 忽略；check/clippy 成功（原有告警），全局 fmt 历史差异未清理。日志 /tmp/namespace-{all-tests,check,clippy,fmt}.log。没有部署、没有 Holo 操作，线上仍为上一条记录的 xattr 修复版。当前工作树继续保留全部未提交改动。

下一步：目录必须成为版本化 catalog 记录并进入提交/接管/回收流程，再开放持久化 mkdir/rmdir 和 rename；不能只把内存目录搬到本地文件，也不能把目录记录直接当零长度普通文件加入回收。原生 LtfsVolume 的 create_directory/remove_directory/rename_path 已存在。已提交视图的 DirNode 保留目录节点，但当前 catalog_of 仅遍历文件，池内跨带空目录仍不可见。

### 2026-09-24：持久化目录已接通，尚未部署

本轮接通 Client::mkdir/rmdir、HTTP POST/DELETE /directories/<path> 和 FUSE mkdir/rmdir，移除挂载内 mem_dirs。201 才是 LTFS 索引提交确认；202/响应丢失/提交未知返回 Indeterminate，不重放，只有明确 failed（未落带）才重试。目录进入标准 LTFS 原生节点，metadata.kind=directory 标记 catalog 类型，并保存原生时间/readonly/xattrs；普通 list 仍只列文件，list_dir/stat 支持空目录。Core 的 publish_namespace 原子发布真实 DirNode，目录不是零长度 FileVersion。

目录与文件共用准入、冻结和增量提交路径，父子路径冲突受同一把服务锁保护。文件写入会在当前带复制或建立必要父目录、保留已知目录属性并发布版本；目录项创建/删除更新直接父目录时间。目录墓碑含 tapers.deletedKind，回收按原生目录搬迁并把目录及墓碑纳入两道格式化证明。并发回收若遇到当前写带已发布的替代记录会跳过旧副本。打开旧带时 recent 不再无条件盖住别带较新或同版本记录；lookup 目录失败也不再伪装成不存在。

兼容性：无新依赖/SQLite 表，metadata 增加目录类型和 node 字段，但旧二进制会误读目录记录，必须统一升级，不能混跑。旧带重新装载时即使 generation 相同也提交全量 catalog，以补齐旧缓存遗漏的目录；未装载旧带的空目录仍未知，部署后目录写操作前需要对账相关介质。文件数上限保持只计文件/文件墓碑，目录与目录墓碑不计入；保留索引空间的估算包含全部原生目录。跨带删除仍依赖池内墓碑，单独导出旧带不能代表全池当前视图。

验证：cargo test 187通过/1忽略；cargo check --all-targets、cargo clippy --all-targets --all-features 成功，告警种类与此前一致。本机实际 FUSE kernel_file_operations 显式运行成功，新增空目录卸载/重挂载断言（内存后端）。三节点模拟测试覆盖 Client/FUSE 对象重建、接管、原生空目录+二进制属性回收、目录删除后读取旧带触发对账、删除后同名改作文件。纯测试覆盖目录发布前不可见、失败提交结果未知、停止时暂存失败、响应丢失不重放、Core 旧读者快照不变。全局 cargo fmt 仍有历史差异，仅格式化本轮触及区域。日志 /tmp/dirs-final-{tests,check,clippy,fmt}.log、/tmp/dirs-kernel-remount.log。

本轮没有远程连接、没有 Holo/LE 写入或部署；线上仍运行 xattr 修复版本。rename 仍 EXDEV，符号链接创建、可写 xattr 等 LE 差距仍未接通。下一步先用保留测试介质做目录增量/Full 检查点及 LE 往返验收，再考虑部署目录版本、继续实现 rename；注意旧副本卷 UUID 与原带相同，不能归属原集群。工作树全部改动未提交。

### 2026-09-24：目录 Holo/LE 往返验收通过，原集群已恢复

详见 `holo-directories-20260924.md`。隔离三节点新目录版本仅接纳保留副本 RT2502L8；Client mkdir/rmdir、非空拒绝、FUSE 空目录/嵌套目录、卸载重挂载均通过。DP gen17 标准增量/IP gen13；正常停机 DP/IP gen23 Full。复制关闭副本到 LE/TS1000L8，LE 成功读取目录并删除 client-empty、新建 le-empty（二进制 xattr、纳秒 mtime）及 le-file，回写2.4.0 gen24。复制回 RT 后新版 Client/FUSE 读回成功，删除未复活，目录属性与文件只读共享 mmap 正确。原 keep.bin 内容/xattr 保留。本轮只新增门控 example/probes/记录，没有进一步修改生产逻辑。

测试目录各节点 `/home/rocky/tape-rs-directories-20260924`，现已全部停止，tsclient FUSE 已卸载；RT2502L8 回槽7、TS1000L8 回槽1032且unassigned，均保留gen24结果。Holo `/home/rocky/holo-directories-20260924/` 保留前后副本。相同UUID副本绝不能归属原集群；不可直接启动测试集群与原集群争抢设备。原三节点仍统一运行 xattr 修复版 SHA59faf2…，尚未升级目录版本；现 node2/term14/round124、TS1001L08 gen39 Complete。原探针与EE f3哈希不变，EE三节点available、无任务、9个targets ready。无断电、格式化、介质创建删除或物理带库操作。

本轮软件187通过/1忽略，check/clippy完成（既有告警）；新增probe格式/语法/diff-check通过，全局fmt历史差异仍在。下一步可统一部署目录版本并对账相关旧介质，再继续rename。rename仍EXDEV，符号链接创建和可写xattr等剩余LE差异仍有效，不能将目录验收称为全部能力对齐。所有改动仍未提交。

### 2026-09-24：目录版本已统一部署到原集群，全部归属介质对账完成

详见 `holo-directories-upgrade-20260924.md`。新正式目录各节点 `/home/rocky/tape-rs-directories-prod-20260924`，原start.sh入口指向新版；三节点实际exe SHA256均2b5bd74…，沿用上轮验证二进制。各节点pre-upgrade.tgz保存离线旧data/程序/脚本，升级后不能直接回滚旧数据库或只降级二进制。tsclient常用/home/rocky/ltfsctl已更新；新版FUSE在正式发布目录，验收挂载已卸载。

全部已归属介质只有TS1000L08、TS1001L08。前者实际装载后catalog从gen1对账到gen2，原生空卷、未写入；后者gen39同代全量对账补齐rc及三个原生子目录。实际FUSE创建empty、非空rmdir拒绝；全群停机Full gen42、重启后目录仍在；清理后再正常停机Full gen45并恢复服务。原9个文件逐一长度/SHA256验证通过；两条新目录墓碑保留，未回收空间。三节点catalog21条/哈希相同、applied153，实际list仍9文件。

当前node1/term18/round150、TS1001L08 gen45 Complete、45MiB可用；TS1000L08回槽5 gen2。原probe和EE f3哈希不变，EE3节点available且无任务。隔离副本未归属，测试集群仍停机。只有Holo虚拟介质索引写入/移动，无断电/格式化/物理带库。本轮无生产代码修改，仅探针/记录；Python语法及diff-check通过，软件187测试/check/clippy沿用上轮，未重新运行。下一步实现rename，之前符号链接创建/可写xattr等剩余差距仍有效。所有改动未提交。

### 2026-09-24：当前写入卷内原子 rename 已实现，尚未部署

接通 `Client::rename(from,to,no_replace)`、HTTP `POST /rename?from=<encoded>&to=<encoded>&noreplace=0|1` 和 FUSE rename。只有当前写入卷内源子树及已有目标可原子改名；源位于其他读带、跨带源子项或跨带目标明确返回409 cross_device/FUSE EXDEV。本阶段没有实现池内任意磁带的原子rename，不能宣称全部LE能力已对齐。RENAME_EXCHANGE/WHITEOUT仍不支持。

FileService同一锁内预留源、目标全部路径，一起complete/freeze，同批标准增量提交及目录发布；源旧路径写版本化墓碑。executor使用native rename保留UID/extent/mtime/xattrs，再更新catalog版本，不复制数据。执行前核对原生源子树与准入计划一致，不把其他带已替代的旧子项静默带到新目录。Core stage_reference发布既有长度而不重复消耗容量；修复同批父/子删除时已删祖先可能被子删除重新创建的问题。未改变SQLite表/索引格式，Upload多一个内存rename计划字段。

FUSE namespace读写锁协调改名与路径操作；源及子inode保持，源打开写句柄先同步脏数据、成功后改用新路径，被替换目标写句柄解绑名字，后续flush/fsync不能覆盖新目标。相关数据句柄锁冻结写入，已有读句柄保留本地内容。close后未确认的暂存上传可返回EBUSY。rename若EIO/结果未定，挂载停止后续路径访问及写入，只允许已有句柄读缓存和释放，需独立Client核对后重挂载，避免旧路径重新上传；Client响应丢失不重放。

验证：最终cargo test 193通过/1忽略，全部5项本机ignored FUSE测试显式运行通过（包含真实内核rename inode保持、打开writer改名后继续写、空目录改名后重挂载、跨卷EXDEV回退上传失败保留源）。三节点模拟验证rename后接管、替换、特殊字符、UID/extent/mtime/xattr保留；源/目标在途互斥、跨带拒绝、结果未定FUSE禁止旧路径写回、Core零容量引用与无残留祖先、索引filemark故障后模拟断电均有覆盖。check all-targets和clippy all-targets/all-features完成，未保留新增clippy警告；fmt全仓仍有历史差异，新函数/已规范文件单独格式化，diff-check通过。日志 `/tmp/rename-final-{tests,check,clippy,fmt}.log`、`/tmp/rename-kernel-all.log`、`/tmp/rename-faults.log`。

首轮并发全套的接管测试向缓存的旧隔离Leader读取，遇到既有read_failed 500/UA2A03；最终恢复测试显式访问新Leader以验证rename恢复，不将它当作“Client任意读取自动故障切换已通过”。后续应单独审视读失败后的Leader发现/安全重试，不能自动重试位置相关SCSI。格式化过程中曾生成重叠片段导致语法错误，已修正并完整重跑，以上是最终结果。

本轮无远程连接、无部署、无Holo/物理带库操作；线上仍是上一节目录版本，node1/term18/round150、TS1001L08 gen45的状态只来自上一轮，本轮没有重新核对。下一步先在保留隔离副本上做rename的Holo/LE往返和失败恢复验收，再部署；跨带原子rename、符号链接创建、可写xattr等仍待继续。全部改动未提交。

### 2026-09-24：rename Holo/LE 验收完成，修复 LE 改回旧名的墓碑遮蔽

详见 `holo-rename-20260924.md`。隔离三节点 RT2502L8 验证 Client/FUSE rename、NOREPLACE、源打开writer及被替换目标writer隔离；截获gen44 committed响应并SIGKILL旧Leader，Client返回Indeterminate、不重放，新Leader恢复完整新树。正常停机gen45 Full交给LE；LE读回后将imported.bin→rc/edit.bin、imported-empty→原le-empty、moved→source，写出2.4.0 gen46。

首次FUSE读回ENOENT，确认原生edit.bin version59.8被私有源墓碑59.9遮蔽。catalog_of已改为同索引原生节点优先于墓碑，保留节点原版本，跨带版本规则不变。回归先失败再通过，隔离三节点部署修复后直接读取LE gen46成功，内容/UID/extents/mtime/xattrs/空目录/只读共享mmap验证通过。194软件测试通过/1忽略，check/clippy完成（基线警告），全仓fmt仍基线差异。未测试VM断电，只有隔离Leader SIGKILL。

原集群已恢复node2/term19/round155，TS1001L08仍gen45 Complete，原9文件逐个长度/SHA一致。仍运行目录版本2b5bd74…，没有部署rename到原集群。EE3节点available、无任务，f3哈希不变，9个publication ready。测试FUSE/代理/服务全停，RT2502L8槽7、TS1000L8槽1032且unassigned，均保留gen46；Holo备份目录holo-rename-20260924。不要将同UUID测试副本归属原集群。

下一步统一部署验收后的rename及LE墓碑修复版本；跨带原子rename、可写xattr/符号链接及Client缓存旧Leader后的读取失败仍是独立后续项。所有改动未提交。探针首轮有10/11字节断言笔误，已独立复测通过，详情见验收报告，不能把prepare末尾未执行的断言当成通过。

### 2026-09-24：rename 与 LE 墓碑兼容修复已统一部署原集群

详见 `holo-rename-upgrade-20260924.md`。正式目录 `/home/rocky/tape-rs-rename-prod-20260924`；原start.sh已切换，三节点实际exe SHA256均dc1e70ac7080e69e5104f0d8bd709d2f6230719993c61b433f0e542db9ded0ac。全停备份pre-upgrade.tgz包含旧data/程序/入口。tsclient常用ltfsctl同步更新，FUSE使用新目录，验收挂载已卸载。隔离副本未归属原集群。

原带专用路径验证目录/文件rename、打开writer继续写、inode保持、NOREPLACE EEXIST、只读共享mmap。正常停机Full gen53后node3接管重挂载读回成功；清理后再次Full gen58并恢复。当前node1/term22/round192，TS1001L08 gen58 Complete、39MiB可用，TS1000L08仍gen2。原9文件长度/SHA256与最终list全部一致，三节点catalog29条/applied195/哈希3bf72f…一致。EE3节点available无任务，f3哈希不变，9 publications ready。

本轮没有生产源码变更；release全bins重新构建、门控Python探针语法与diff-check通过。软件全套沿用上一轮194通过/1忽略、check/clippy结果。无断电、SIGKILL、格式化、物理带库操作。原集群“尚未升级rename”的历史状态被本条取代，全部工作树未提交。

下一步优先修复已有Client缓存旧Leader后读取返回read_failed的问题，做应用层安全重试/重新发现Leader的回归及隔离故障验证；不要自动重试READ/WRITE/SPACE等位置相关SCSI。跨带原子rename、可写xattr/符号链接继续作为后续独立能力。

### 2026-09-24：旧 Leader 读取失败的软件修复完成，尚未部署

详见 `client-read-failover-20260924.md`。三节点模拟PR抢占稳定复现get_to返回500 read_failed/sense06/2A03。根因在executor::read_any丢失ownership_lost分类，不是Client应当盲目重试500。新增ReadError::Lost与device分类，所有读取/库存/装卸错误保留原始诊断；丢失所有权后关闭服务、终止本轮并通知Raft，映射既有Busy→HTTP503，旧Client自动换节点。失去资格后不再换带清理；恢复写带之前的卸带错误不再被忽略。READ/WRITE/SPACE重试和PR抢占策略未改。

回归覆盖模拟PR抢占后get_to完整输出、新执行者接管；rename测试去除重建Client绕过；普通介质错误分类、HTTP内容截断不重放/不拼接、普通500不重试。197软件测试通过/1忽略，check/clippy完成，原有警告与全局fmt基线差异保留。无远程连接/部署/硬件访问，线上仍上一条rename发布版dc1e70…（本轮未重查）。下一步隔离Holo旧Leader网络隔离与设备转移的读取验收，再部署ltfsd。工作树全部未提交。

### 2026-09-24：读取预留丢失修复通过隔离 Holo 实测，原集群已恢复

详见 `holo-read-failover-20260924.md`。测试目录 `/home/rocky/tape-rs-read-ha-20260924`，独立test-data，仅RT2502L8/gen46。正式修复ltfsd SHA e1a8c4fde9bffdb9fcee1108d4cd4c5bbbaa537b974d13d3838b02e3d302b410。三个Client预读缓存node3/round119；strace仅延迟旧Raft下一次futex20秒及执行线程下一次ioctl12秒，新node2/round124实际接管PR后，旧LOCATE返回06/2A05。HTTP503 drive_busy保留sense，旧FileService关闭、轮次Lost；trace未再执行设备调用。get_to完整4MiB无拼接，get/get_range同样保持旧缓存后切换通过。透明代理只记录，不伪造故障。

首轮node3缺strace没有实际故障，仅作基线；复制node1工具到测试目录并确认TracerPid后第二轮才命中。没有系统网络规则、错误码注入、SIGKILL、VM断电、格式化或文件写入。新example与代理门控；check all-targets、新example Clippy/格式、Python语法/diff-check通过，197全套软件结果沿用上轮。

测试节点/代理/strace全部停止，RT2502L8回槽7，隔离目录保留。原集群恢复旧rename发布版dc1e70…，node1/term23/round197，TS1001L08仍gen58 Complete、39MiB；原9文件和list完全一致，EE3节点正常无任务、f3哈希不变、9 publications ready。读取修复仍未部署原集群。下一步统一部署验收后的ltfsd，Client重试策略不需改动。全部改动未提交。

### 2026-09-24：读取故障切换修复已统一部署原集群

详见 `holo-read-upgrade-20260924.md`。正式目录 `/home/rocky/tape-rs-read-prod-20260924`，原start.sh已切换；三节点实际exe SHA均e1a8c4fde9bffdb9fcee1108d4cd4c5bbbaa537b974d13d3838b02e3d302b410，与隔离验收相同。全停后pre-upgrade.tgz保存原data/旧rename程序/入口。Client和FUSE未更新，继续rename发布版，已用其真实挂载和全新缓存验证原9文件长度/SHA256/只读共享mmap；挂载已卸载。

当前node1/term24/round202，TS1001L08仍gen58 Complete、39MiB可用，TS1000L08 gen2。三节点applied205/catalog29条/哈希3bf72f…完全一致，最终list仍原9文件。EE3节点正常无任务、f3哈希不变、9 publications ready。隔离副本未归属原集群，测试服务保持停止。

无生产源码变更，本轮只读验收、没有故障注入/写文件/格式化/断电/物理带库操作。沿用已验收二进制和197软件测试/check/clippy结果，未重跑全套；新增门控Python语法和diff-check通过。历史“读取修复尚未部署”被本条取代。普通500重试与部分流式输出的边界保持原契约。后续回到剩余LE功能：跨带rename、可写xattr、符号链接等；全部工作树未提交。


### 2026-09-24：可写 xattr 软件实现完成，尚未部署

详见 writable-xattrs-implementation-20260924.md。Client/HTTP/FileService/executor/FUSE 支持当前写入卷文件及目录用户属性的设置、替换、删除，原生 LTFS 增量索引持久化，不重写数据。二进制值/空值/64 KiB、CREATE/REPLACE/ENODATA、UID/extent/mtime/hash 保留、后续覆盖保留属性及接管读回均通过模拟集群验证。元数据容量按零预留；响应丢失不重放，FUSE 未定后阻止继续写回。

全功能软件测试200通过/6门控跳过，独立现有本机kernel_file_operations通过；check通过，clippy无新增告警，全仓fmt仍历史基线差异，diff-check通过。本轮没有远程访问或部署；线上仍上一条读取修复版，未重查。跨带EXDEV，根目录及ltfs虚拟控制属性未实现，内部属性受保护；具体边界见docs/file-client.md。

下一步：新增xattr实际内核调用验收，然后隔离Holo/LE双向属性回写（包括2.4.0降级、二进制值和删除）及提交中断恢复。通过后再部署。全部改动未提交。


### 2026-09-24：可写 xattr 通过本机内核及隔离 Holo/LE 验收

详见 holo-writable-xattr-20260924.md。新增本机内核 set/get/list/remove 属性验证（含空值/64KiB/CREATE/REPLACE/目录/脏writer）。Holo准备gen55时LE报Unexpected xattr size；离线IBM解析器用最小XML确认空Base64显式闭合元素=-5030、自闭合元素=0。write_xattrs改用Event::Empty，新增回归先失败后成功。修复后原测试带REPLACE提交gen56，停机Full gen57；LE挂载读回、修改后2.4.0 gen58，由新FUSE挂载读回全部通过。UID/extent/mtime/内容哈希一致。

201软件测试通过/6门控跳过，独立kernel_file_operations通过，check通过、clippy无新增告警；全局fmt历史基线差异、diff-check通过。固定隔离ltfsd SHA f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d，FUSE fac4a6ae370f978c39b5b15bf24985f03899c2a63954f5505ca657238fdcad86。兼容Rocky使用CFLAGS_x86_64_unknown_linux_gnu=-mcpu=baseline cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.34 --features fuse。

原三节点通过bash调用原start.sh恢复（脚本本身无执行权限），node1/term25/round207，原TS1001L08仍gen58。实际exe均旧读取修复e1a8c4…，原9文件长度/SHA全部一致；EE3节点available无任务、f3哈希不变、9publications ready。隔离服务/FUSE停，RT2502L8槽7、TS1000L8槽1032且unassigned，均保留LE gen58；备份 /home/rocky/holo-xattr-20260924。无断电/SIGKILL/格式化/物理设备操作。

下一步统一部署验收后的可写xattr与空值XML兼容修复，在原集群专用目录验证并清理。属性提交中断的新增Holo故障点仍未测；跨带属性修改EXDEV、根目录虚拟控制属性未开放。全部改动未提交。


### 2026-09-24：可写 xattr 与空值 XML 修复已统一部署原集群

详见 holo-writable-xattr-upgrade-20260924.md。正式目录 /home/rocky/tape-rs-xattr-prod-20260924；原start.sh已切换，需bash调用。三节点实际exe均验收版f8c2383146c4b595d42993838bbbfd353efef064d0f57296a17758d66f2d2e8d。各目录pre-upgrade.tgz为离线data/旧读取修复程序/入口备份。tsclient新FUSE fac4a6ae…，常用ltfsctl更新为3688d36c…；旧Client保存ltfsctl.before。

原带专用/rc/xattr-prod-20260924验证二进制/空值/CREATE冲突/REPLACE/删除/目录属性/mtime/只读共享mmap。node2写入，停机Full gen67后node1接管、全新挂载读回并清理；再停机Full gen72后node3/term28/round246恢复。原9文件长度/SHA和最终list一致，专用目录消失；三节点applied249/catalog31条，哈希a6c01dada1bf202c64da6d0e4e4a2b84e9d69cdcf5ff10e79bf6d5eb1dfdddb0一致（两个新墓碑）。TS1001L08 gen72 Complete/32MiB，TS1000L08 gen2。

EE3节点available无任务，f3哈希不变，9publications ready。测试FUSE卸载无残留进程；隔离副本未归属、测试服务未启动。本轮未改生产源码，沿用201软件测试及内核/LE结果；Client兼容release构建、Python语法/diff-check通过。未做断电/SIGKILL/格式化/克隆/物理操作。历史“可写xattr未部署”由本条取代，全部改动未提交。

下一步优先隔离Holo补测属性提交中断与响应丢失的恢复边界；原数据带不做故障注入。跨带属性EXDEV、根目录虚拟控制属性/跨带rename/符号链接创建仍待实现。


### 2026-09-25：属性提交中断与响应丢失隔离验收通过

详见holo-xattr-faults-20260925.md。独立xattr-ha-20260925目录/7500/7501，仅RT2502L8，正式ltfsd f8c238…未修改。Client探针hw_xattr_fault新增显式条码/端点门控。

第二轮strace延迟500ms每ioctl，WRITE(6)成功47788字节后、结束FM延迟进入时SIGKILL node1。实际周期Full索引（非Incremental）截断；node3/round177从gen63恢复TruncatedIndex，放弃220..221并收尾gen64，旧cut属性与内容保留，Client Indeterminate。首轮2秒延迟/90秒窗口超时，未杀进程，正常提交gen62，明确不算通过；重设old到63后再测。

设置201/gen65与删除201/gen66分别被单次代理拦截，SIGKILL旧Leader后释放连接，Client均Indeterminate。接管读回设置值正确、删除不复活；版本保持(177,1)/(184,1)，没有重放。最终隔离正常停机Full gen67，RT2502L8归槽7；备份holo:/home/rocky/holo-xattr-ha-20260925。未访问LE副本，TS1000L8仍前轮gen58。

原集群恢复node3/term29/round251，TS1001L08仍gen72 Complete；实际exe均f8c238…，原9文件长度/SHA与list一致，三节点catalog31条/哈希a6c01d…一致。EE3节点available无任务、f3不变、9publications ready。隔离节点/代理/strace均退出。无VM断电/Holo重启/格式化/物理操作。

新example兼容构建、check all-targets、example clippy（库既有警告）、example格式/Python语法/diff-check通过；未重跑201软件全套。生产代码无改动，全部工作树未提交。后续独立Incremental截断与目录发布前故障尚未现场测，可继续补齐；剩余LE能力边界不变。


### 2026-09-25：独立 Incremental 截断隔离验收通过

详见holo-incremental-cut-20260925.md。独立incremental-cut目录/7500/7501，RT2502L8从Full gen67开始，无额外prepare。node3/round196修改cut属性；strace延迟200ms，确认WRITE(6)2325字节XML根ltfsincrementalindex后、结束FM尚未执行时SIGKILL。Client Indeterminate；node1/round201恢复gen67/TruncatedIndex，放弃234..235并收尾gen68，旧cut/set二进制/delete不存在/内容全部保持。恢复后临时属性设置删除分别gen69/70成功，最终Full gen71。

原集群恢复node1/term30/round256，原TS1001L08仍gen72 Complete；三节点实际exe f8c238…，catalog31条/哈希a6c01d…一致，原9文件长度/SHA/list一致。EE正常无任务/f3不变/9pub ready。隔离节点和strace停，RT2502L8槽7保留71，LE副本仍58。Holo备份/home/rocky/holo-incremental-cut-20260925。无存储断电/格式化/物理操作。

新增Python探针语法、diff-check通过；复用正式程序和Client例程，未改Rust/未重跑201软件全套。Full和Incremental截断及设置/删除响应丢失均已现场验证；后续索引已提交但目录发布前窗口、存储断电尚未覆盖。全部工作树未提交。


### 2026-09-25：索引已提交、目录未发布窗口验收通过

详见holo-publication-cut-20260925.md。隔离publication-cut目录、RT2502L8 Full71起点，node2/round210提交Incremental72后，在bytes_written_on的MAM读取前延迟。控制器确认卷commit72日志而SQLite目标仍71/旧cut，尚无执行线程已提交日志，才SIGKILL。Client Indeterminate。node1/round215恢复Complete72、目录71，自动对账补齐；属性新值/原version210.1保留、无重放，内容/mtime/SHA不变。node2重启后3节点SQLite均72/210.1。最终Full73。

原集群恢复node3/term31/round261，原TS1001L08仍gen72；实际exe均f8c238…、原9文件/list和catalog31条/a6c01d…一致。EE正常无任务、f3不变、9pub ready；隔离服务/strace退出，RT2502L8槽7保留73，LE副本仍58。备份/home/rocky/holo-publication-cut-20260925。无存储断电/Holo重启/格式化/物理操作。

新Python探针语法/diff-check通过；无生产代码改动，未重跑Rust全套。Full/Incremental截断、响应丢失和提交后目录发布前均有现场证据；存储断电以及其他barrier/VCI/复制发布细分阶段未全覆盖。全部改动未提交。

### 2026-09-25：xattr 已确认增量提交的 VM 断电验收通过

详见holo-xattr-powercut-20260925.md。原集群停机/原带归槽后，隔离xattr-powercut目录、7500/7501、RT2502L8 Full73起点，node1/round220三次HTTP201提交二进制/空值/删除属性至gen74/75/76；暂停测试进程，直接qm stop510并重启，无正常卸载或Full checkpoint。恢复9个现场handler参数（首次TERM未退出，补KILL并保留失败日志），新node1/round231从磁带恢复Complete76。属性、版本220.3、mtime、完整list及13文件SHA一致，三节点目录一致。清理至gen79，最终DP/IP Full79、RT槽7，LE副本未触及。

原集群恢复node1/term32/round266、TS1001L08仍gen72 Complete；三节点实际exe f8c238…、catalog31条/a6c01d…及原9文件/list不变。EE三节点available无任务、f3不变、9pub ready。测试进程/被暂停的sudo父进程和僵尸均清理。Holo备份/home/rocky/holo-xattr-powercut-20260925含before/after-crash/after和私密runtime.json。PVE快照before-xattr-powercut-20260925生成但guest冻结失败，只当崩溃一致性备份；未回滚。

新增HTTP探针probes/xattr-powercut.py，语法/diff-check通过；无生产Rust改动，未重跑全套。覆盖的是Holo VM硬停止后已确认提交持久性，不是PVE宿主/物理存储断电；其他barrier/VCI细分窗口仍未全覆盖。原始材料probes/results/xattr-powercut-*，全部改动未提交。

### 2026-09-25：Incremental barrier / VCI 错误与中断验收通过

详见holo-barrier-vci-20260925.md。新增sim_incremental的12组矩阵：barrier/IP VCI/DP VCI错误 × VCR正常/不变 × 是否模拟断电。旧视图不发布/卷冻结/重挂恢复新增量/继续提交及Full收尾均通过；全套202通过6忽略，check/clippy完成（既有告警），修改文件fmt和Python语法/diff-check通过，全仓fmt仍基线差异。

隔离barrier-vci目录、7500/7501、RT2502L8 Full79。node2/round242写barrier属性，确认增量XML和结束FM成功、尚未执行barrier后SIGKILL（trace在屏障前PR检查）；Client Indeterminate，node1/round247恢复Complete80并补目录79。随后node1/round247写vci属性，在IP VCI成功而DP VCI进入延迟时SIGKILL；Client Indeterminate，node2/round252恢复Complete81、补目录80。版本保持242.1/247.1，属性/mtime/list/13文件SHA一致，重启旧节点后三目录一致。首次Client端点误带http://导致未连接，改host:port后才执行测试，失败日志保留。清理gen82/83，最终Full84、RT槽7。

原集群恢复node1/term33/round271，TS1001L08仍72 Complete，实际exe f8c238…、catalog31条/a6c01d…、原9文件/list不变。EE正常无任务/f3不变/9pub ready；测试进程和strace退出。备份holo:/home/rocky/holo-barrier-vci-20260925。无VM断电、Holo重启、LE访问或生产代码改动。下一项：故障恢复后Full84的LE往返及降级读回；LE副本TS1000L8仍gen58，未更新。材料probes/results/{barrier-cut*,vci-cut*,barrier-vci-*}。本轮开发、报告、探针和筛选后的验收证据已在当前分支整理，最终提交状态以 Git 为准。

### 2026-09-25：故障恢复后 LE 往返完成，发现 FUSE 符号链接读取缺口

详见holo-recovery-le-20260925.md。RT Full84复制到卸载且unassigned的TS1000L8，LE挂载读13路径哈希/原故障属性成功，新增rc/post-recovery-le-20260925.bin（65566B，eebfc1dd…）及二进制/空属性，原故障文件新增le.after-recovery。同步卸载后DP/IP实际2.4.0 Full85，26个原文件UID/length/mtime/symlink/extent按字段比较不变。复制回RT后隔离recovery-le目录7500/7501，node1/round261恢复85、目录83→85；Client探针接口通过，FUSE全新缓存的12个原普通文件+新文件、属性/mtime/pread/mmap通过。无tape-rs写入，最后仍2.4.0/85。

严格FUSE全扫描未通过：rc/link带上symlink→keep.bin、length0；catalog_of未传链接类型/目标，FUSE将其当零长度普通文件，HTTP GET实际跟随返回64KiB，open_inner长度防护稳定EAGAIN（三次重现）。目标keep.bin直接读取正确。根因定位完成，未修复；不要宣称完整FUSE兼容，也不要移除长度防护。probes/recovery-le.py fuse保留严格失败；fuse-regular显式报告GAP，仅普通文件子集通过。下一步优先补链接类型/目标端到端及readlink，再用现存Full85冷缓存重跑；创建/rename/unlink等需独立验证。

双方测试带保留85及新文件/属性，RT槽7、LE槽位且unassigned；备份holo:/home/rocky/holo-recovery-le-20260925含native-full84和le-before58。原集群恢复node1/term34/round276、TS1001L08仍72 Complete，正式3exe f8c238…、catalog31/a6c01d…、原9文件/list不变，EE正常无任务/f3不变/9pub ready。FUSE和测试节点停止。无VM断电/Holo重启/生产代码变更。新增只读Client example hw_recovery_le与门控Python；兼容构建、example check/clippy/fmt、Python语法/diff-check通过，未重跑全套。证据probes/results/recovery-le-*；全部未提交。

### 2026-09-25：已有 LE 符号链接读取修复并隔离验收

见holo-symlink-20260925.md。catalog仅对链接增加metadata.kind=symlink/metadata.symlink；TapeFs链接属性/readlink及Adapter S_IFLNK/正确d_type，内核负责目标解析，长度校验保留。非目录项opendir补查属性会增加元数据请求，大目录性能未测。新内核回归先红后绿，覆盖链/目录/中文/悬空/循环/mmap；默认catalog回归保护普通文件元数据。

隔离symlink目录7500/7501、RT LE Full85；ltfsd418339d1…、FUSEa8ccc02c…，冷缓存复跑前轮严格recovery-le.py fuse通过13原路径+新LE文件，lstat/readlink/scandir/mmap link→keep.bin通过。最终仍2.4.0 Full85、RT槽7；LE副本未动。原集群恢复node1/term35/round281、仍旧正式f8c238…，原TS1001L08 72 Complete、原9文件/catalog31/a6c01d…一致，EE无任务/f3不变/9pub ready。备份holo:/home/rocky/holo-symlink-20260925。

最终203通过7忽略，6项本机FUSE门控测试另行通过；首轮接管namespace测试因执行资格失去HTTP500失败，单项和全套复跑通过，未修复该间歇竞态，日志保留。check/clippy完成无本轮新增警告，fmt全仓历史差异、diff-check通过。新增测试区域格式化，无生产逻辑后改。修复版尚未部署原集群；下一步可统一升级服务端/FUSE并重挂对账，随后验收原环境；链接创建/硬链接与完整链接写语义仍待实施。全部未提交。

### 2026-09-25：符号链接读取修复已正式升级

见holo-symlink-upgrade-20260925.md。新正式目录/home/rocky/tape-rs-symlink-prod-20260925：三节点ltfsd实际exe SHA418339d1f60b277c07759b42c775342557c36c84fc4df3a84a23b6ed13f88ae8；tsclient FUSE实际exe SHAa8ccc02cc7a25f88d6f4b50ea3d36ba00c1837dd19f172bf2abc8bd4a05f4d25。原start.sh仅换目录，其余参数/data-dir保留，仍bash调用。全停后各节点pre-upgrade.tgz(0600)保存关闭data/旧bin/旧入口，另存start.before.sh；客户端保存tape-fuse.before。ltfsctl不变，cluster实际成功。

原集群当前node1/term36/round286、applied289，TS1001L08仍72 Complete、32MiB，TS1000L08仍2。新FUSE冷缓存原9文件length/SHA/mmap/barcode和rc列举通过，已卸载；HTTP list/9文件及3节点catalog31条/a6c01d…不变。EE正常无任务/f3不变/9pub ready。RT与LE隔离副本未触及，仍85。本轮无写文件/属性、断电、复制或格式化，仅部署和只读验证，无新源码、不重跑全套，脚本语法/hash/diff-check通过。

后续核验当前程序使用/tmp/symlink-prod-node-verify.py；旧/tmp/xattr-prod-node-verify.py固化f8c238旧哈希。证据probes/results/symlink-upgrade-*。链接创建/硬链接/完整写语义、大目录性能、既有接管间歇500仍待独立工作。全部改动未提交。

## 2026-09-25 符号链接创建完成隔离验收

新增 Client::symlink(target,path)、POST /symlinks、FileService/Upload/executor 原生链接提交与 FUSE 创建回调。新增准入/集群接管/丢响应不重放/实际内核测试。软件205通过7忽略，门控内核6通过；check/clippy完成，fmt历史差异保留。报告holo-symlink-create-20260925.md。

隔离目录tape-rs-symlink-create-20260925，ltfsd 45ca052…、FUSE cc8b3c…；RT2502L8从85写至99 Complete，6类链接及通过链接写新target、rename/unlink、重启全新缓存、LE副本读取全部通过，原13路径哈希不变。最新关闭test-data在此目录（node1 round302）；RT槽7，LE TS1000L8已卸载unassign，副本含新目录（只读验证，未将LE副本回拷RT）。Holo备份holo-symlink-create-20260925/{before,after,le-before}。

原集群恢复node1 term37 round291，TS1001L08仍72 Complete。当前正式程序仍tape-rs-symlink-prod-20260925的418339…读取版；三节点实际程序/catalog31条a6c01d…和原9文件哈希复核不变。创建版尚未正式部署。下一步可部署创建版并验收，先读取本报告与当前状态。禁止将测试data覆盖原ltfsd-data。所有改动未提交。

## 2026-09-25 符号链接创建正式发布

当前正式目录变为/home/rocky/tape-rs-symlink-create-prod-20260925；ltfsd 45ca052…，FUSE cc8b3c…。原入口tape-rs-ltfs25-20260924/start.sh已改路径，端口/数据/设备参数不变。各节点pre-upgrade.tgz含关闭的旧数据/程序/入口，勿恢复旧Raft数据覆盖在线状态。

原node1正常停止后node3接管；当前node3 term39 round327，3节点均已启动。TS1001L08从72经创建86到清理95；最终原文件读回mount日志确认95 DP/IP Complete，cluster.local仍显示接管时86不代表最终磁带代数。原9文件read/hash/mmap/barcode一致；与备份比较原30条非/rc记录的除generation外全部字段一致。/rc父目录元数据和测试删除墓碑正常变化，当前catalog42条，三节点SHA 7d5660c28eb3023b91fc7f835ad41028c81530fe562bdfc1563437df065dbcd4。

正式/rc/symlink-prod-20260925已创建验收后清理，FUSE已卸载；后续用新正式目录FUSE。RT2502L8和LE副本未访问，保持上一轮gen99测试状态。EE三节点available、无任务、f3哈希一致。报告holo-symlink-create-upgrade-20260925.md，证据symlink-create-upgrade-*。当前程序核验/tmp/symlink-create-prod-node-verify.py；旧symlink-prod脚本固定418339…会失败。下一项可针对链接跨带回收迁移保真做隔离验证，硬链接与大目录性能仍未覆盖。工作树保留全部未提交改动。

## 2026-09-25 符号链接跨带回收修复与Holo隔离验收

copy_one原读取链接目标，最小dangling→missing稳定abandoned。已通过relocate_symlink/add_symlink_with_metadata走元数据迁移，保留目标/时间/readonly/xattrs，目标卷UID，无extent，维持路径互斥和回收格式化双闸门。软件206通过7忽略，check/clippy完成，fmt历史差异。新增tests::reclaim_preserves_symlinks_without_resolving_targets，证据symlink-reclaim-*，报告holo-symlink-reclaim-20260925.md。

新测试集群/home/rocky/tape-rs-symlink-reclaim-20260925，7500/7501，独立全新test-data，池sr=51cfd06c-bf79-4bbf-ac85-886608a0ab4b；仅新建SR2501L8/SR2502L8，各512MiB。ltfsd SHA b8496c5bf8de2abb26ffecef81159e5db948437169fa716d5b6e03b31c253025。源Full23→回收后Full2，目标Full4；6类链接及全部XML时间/readonly、HTTP binary xattr、mmap、新缓存、node2 round62接管均通过。测试停止，源槽8、目标槽9，保留备份/holo-symlink-reclaim-20260925及关闭data。不得将此data覆盖正式data。创建/回收确实格式化新带，没有动RT或LE。

原正式程序保持45ca052…（创建版，回收修复尚未发布）。已恢复node2 term40 round350，原TS1001L08正常停机checkpoint由95→96 Complete，19MiB可用。三节点catalog42条，新SHA89691e412db63fe43f18bd3b2ad33ff2f9f12337748f8ca92f5885282c6211c8；旧7d5660…哈希不再适用。原9文件SHA不变，原30条非/rc记录除generation外与升级前备份一致。EE正常/f3一致/9 publications ready。当前程序核验/tmp/symlink-create-prod-node-verify.py仍适用（不固定catalog哈希）；下一步可部署回收修复。本轮开发、报告、探针和筛选后的验收证据已在当前分支整理，最终提交状态以 Git 为准。

## 2026-09-27 回收修复正式升级与无人值守接续

用户明确进入无人值守模式：按推荐决策逐步推进，每项任务完成后须现场验证通过。沿用Holo＋EE/LE现场能力边界，不把它称为物理带库验证。实际实现工作树为 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`；旧feature/docs设计不代表当前实现。

已完成[正式部署与验收](holo-reclaim-upgrade-20260927.md)：目录 `/home/rocky/tape-rs-symlink-reclaim-prod-20260927`，三节点ltfsd SHA `32bd5541…`，tsclient FUSE `ef237d2e…`。原启动入口仅换程序路径，关闭数据备份在各新目录 `pre-upgrade.tgz`（0600）。软件206通过7忽略，check/clippy完成，历史fmt差异保留。

新FUSE创建/链接写入/rename/unlink、node3正常停止后node2接管、冷缓存读回和清理均通过；原9文件长度/SHA/mmap/条码一致。最终node2/term42/round386，三节点catalog53条/applied407一致，TS1001L08实际mount generation119、DP/IP Complete；/cluster.local的110是接管时旧描述。原41条非根记录除generation外保持，目录SHA（本轮排序算法）`1a76875a…`。EE三节点available无任务、f3哈希保持、9 publications ready。FUSE已卸载，隔离带仍槽7/8/9；无格式化/VM断电/物理设备操作。

下一步：隔离SR池的回收中断与接管保真验收，不在原数据带故障注入。本轮没有重复原带回收或新增LE回收往返验证。进度与证据已写入当前分支，最终提交状态以Git为准。

## 2026-09-27 回收在源FORMAT前中断：模拟与Holo接管验收完成

见[holo-reclaim-cut-20260927.md](holo-reclaim-cut-20260927.md)。新增测试transport屏障与回收保真接管用例，无生产算法改动。目标提交后暂停旧FORMAT，新节点接管完成回收，再恢复旧命令，必须报告ownership_lost且介质对象不变；独立重挂完整元数据与后续接管读回通过。最终207通过7忽略，check/clippy完成、fmt历史差异保留。

Holo新隔离目录 `/home/rocky/tape-rs-reclaim-cut-20260927`，从关闭SR数据接续，7500/7501，正式部署版32bd5541…。SR2502→SR2501反向回收，node2 round67完成目标提交后、源FORMAT尚未发出时SIGKILL；node3 round78完成回收，旧节点重启追平。六类链接冷缓存FUSE/内容/mmap/条码/seed通过，13目录记录除条码代数版本外一致；关闭介质Full XML六链接全部元数据保真、无extent。最新SR1槽8 Full4含数据，SR2槽9 Full2空；测试进程/strace/FUSE停止，缓存空。最新关闭test-data在新目录，不能从20260925旧data接续。备份holo:/home/rocky/holo-reclaim-cut-20260927。

正式恢复node1 term43 round409/applied412，TS1001L08正常checkpoint由119→120 DP/IP Complete，可用7MiB。53条原catalog除generation外保持，三副本一致、SHA3339567f…；原9文件length/SHA/mmap/barcode通过。EE三节点available无任务、f3哈希不变、9publications ready。原TS1001第一驱动器、RT/SR1/SR2槽7/8/9。跨主机设备号不同（.71changer sg9，.72sg8），必须按VPD序列号重新核对。

现场覆盖Holo死亡接管，不是物理设备，也没有覆盖FORMAT已成功但完成记录未复制窗口；下一项优先补后者。详细测试编排修正和证据见报告及probes/results/reclaim-cut-20260927-*。按无人值守约定记录推荐决策，未推送远端。

## 2026-09-27 源mkltfs完成、回收完成记录前中断验收完成

用户再次确认无人值守授权，自动执行上一交接下一项。见[holo-reclaim-formatted-20260927.md](holo-reclaim-formatted-20260927.md)。测试transport新增AfterMkltfs：源Full1空/可写、池标记未写、复制状态reclaiming时隔离旧执行者，新节点重做回收得到Full2空/可写且有池UUID，恢复旧操作失权且介质不变。六链接保真和后续接管读回通过。最终208通过7忽略，check/clippy完成，历史fmt差异保留，无生产源码/二进制变更。

新隔离目录 `/home/rocky/tape-rs-reclaim-formatted-20260927`，7500/7501，SR1→SR2回收。node1 round84源mkltfs完成后SIGKILL，trace FORMAT GOOD且无完成的重挂READ，关闭catalog13条均在SR2，仍reclaiming。node2 round95实际挂载新UUID Full1空源带，重做并done。六链接冷缓存FUSE/内容/mmap/seed/条码通过，13目录元数据保真，关闭Full XML六链接除UID/version外一致。最终SR1槽8 Full2空且池属性完整，SR2槽9 Full4有数据；测试/strace/FUSE停止、缓存空。最新关闭test-data在新目录，旧目录不可接续。Holo前后备份 `/home/rocky/holo-reclaim-formatted-20260927`；.71新目录at-cut.tgz为截断证据。

正式恢复node1 term44 round414/applied417，TS1001L08仍120 Complete/7MiB，53条原catalog及池摘要逐字段不变、SHA3339567f…，原9文件length/SHA/mmap/barcode通过。EE available无任务/f3保持/9pub ready。未触及物理带库、未断电、未做LE往返。下一项推荐原始FORMAT成功但LTFS标签/索引未构造完的窗口；本轮不声称覆盖它。证据probes/results/reclaim-formatted-20260927-*，未推送。


# 历史格式与 Clippy 清理验收（2026-09-27）

在活动工作树 ltfs-recovery-ibm-interop 完成全仓 rustfmt 和既有 Clippy 修复，没有新增 allow/expect 或全局屏蔽。实际修复包括条件合并、客户端私有方法命名、缓存类型别名、测试初始化、文档段落、十六进制大小写；SgIoHdr 的 Default 复用原 new，PR key 解析使用定长数组切片保持原截断语义。未修改 ioctl 号、ABI、CDB、超时、依赖或 CLI。

软件验收：cargo fmt --all -- --check、cargo check --all-targets、cargo clippy --all-targets --all-features -- -D warnings 通过；cargo test --all-features 共208通过、7忽略。兼容glibc 2.34的fuse release构建成功；Zig保留一条外部链接器废弃优化参数提示，不以屏蔽处理。格式前的实际lint改动保存在 probes/results/cleanup-20260927-lint.patch。

隔离候选 ltfsd SHA256 f4680ed765b542048a4d3c69eed84b9f20ee9e9869621020d23e75a1b172d948。Holo隔离SR池执行3200字节文件写入/fsync、二进制扩展属性、改名、符号链接，正常停止leader后由node3接管，冷缓存复验通过。原六链接及源文件保真；临时目录删除后13行变17行（删除记录保留），原数据除根目录元数据/索引代数外不变。SR1 Full2，SR2 Full14，分别卸载至slot8/9；关闭数据 /home/rocky/tape-rs-cleanup-20260927/test-data，node3 round116。介质前后备份 /home/rocky/holo-cleanup-20260927 已校验。

正式三节点恢复node1 term45/round419/applied422，53行catalog完全一致，排序摘要3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3。TS1001L08 Full120、TS1000L08 generation2不变；原9文件长度/SHA256/mmap/barcode复验通过。EE三节点available、无活动任务、f3基线摘要未变，Holo九个publication ready。正式二进制保持32bd5541…；隔离进程全部停止，客户端卸载且缓存清空。

证据为 probes/results/cleanup-20260927-*，可执行回归脚本 probes/cleanup-20260927.py。未触及物理磁带设备；物理固件验收仍暂缓。接续任务1：FORMAT成功后、标签形成前的中断恢复；本项不声称覆盖该窗口。


# FORMAT 后标签未完成的回收恢复（2026-09-27）

基于 f1d9993。确定性复现命令 `cargo test --test ltfsd_cluster reclaim_takeover_after_format_before_labels -- --nocapture`：旧实现接管后 BLANK CHECK 08/00/05，回收 abandoned，错误退回 appendable，无法完成源带初始化。失败输出保存在 probes/results/reclaim-raw-20260927-red.log。

修复：迁移完成后仍执行源索引逐路径与目录零引用两个核验；目录无法读取时拒绝格式化。将核验结果通过既有 TapeState 日志持久化为 reformatting，必须观察应用完成才发 FORMAT。接管在此阶段跳过可能已擦除的源索引，重新核验目录零引用及池归属，完成 mkltfs/池标记后回到 appendable。该阶段错误终止执行轮次，保留恢复阶段，不退回普通可写。普通 unreadable reclaiming 没有自动擦除授权。保留旧 TapeReclaimed 日志重放兼容；新版须整组升级，不支持初始化阶段中混跑或回退旧二进制。未修改 SCSI 命令、ABI、超时或依赖。

新增 FORMAT 后无标签、只有 VOL1 的部分标签两个接管用例，加原 FORMAT 前/mkltfs 后两个窗口，均验证旧操作恢复后被 fencing 拒绝、源带不再被改变、六种链接保真。状态测试覆盖非法阶段跳转、快照恢复、重复回收拒绝、完成后迟到提案拒绝。软件：211通过、7忽略；全仓 fmt/check、严格 Clippy -D warnings 通过。兼容构建通过，仍有外部 Zig 废弃优化参数提示，未添加任何告警屏蔽。

现场候选 /home/rocky/tape-rs-reclaim-raw-20260927，ltfsd SHA256 f618de55a803ac52d5c287e1d18cd1b08b1563a20bf0f3f4d0e0ce8e7c70b114。关闭数据复制自 cleanup/test-data；正式数据未用于注入。SR2502L8 Full14 → SR2501L8 空Full2，17条目录记录。node1 round127执行回收；03:38:34 UTC，strace证明 FORMAT GOOD 且其后没有完成 WRITE(6)，在持久 reformatting 阶段 SIGKILL。node3 round141自动接管、报告 done。旧节点重启追平，三份目录一致；所有17条记录除条码/代数/版本外内容及元数据保持。冷缓存 FUSE六链接/链式读取/悬空/循环/目录链接/mmap/seed哈希均通过。

关闭介质 XML 核验：SR2501L8两分区Full5，14个文件元素、六链接，链接字段除fileuid/tapers.version外一致、无数据extent；SR2502L8两分区Full2、零文件、池UUID正确。闭合前后备份 /home/rocky/holo-reclaim-raw-20260927，经SHA逐文件校验。最新关闭数据 /home/rocky/tape-rs-reclaim-raw-20260927/test-data，node3 round141。两带归槽8/9。node1 at-cut.tgz仅为故障证据，不能替代最新状态。

正式集群已恢复node1 term46/round424；53行目录逐字段不变，TS1001L08 Full120、TS1000L08 gen2，原9文件长度/哈希/mmap/条码通过。正式二进制仍32bd5541…，本修复只部署隔离候选。EE三节点available、无活动任务、f3哈希未变；Holo九个publication ready。隔离进程/strace均停止，FUSE卸载、缓存清空。未触及物理设备，物理固件验收暂缓。

脚本 reclaim-raw-20260927.py、reclaim-raw-read-20260927.py、reclaim-raw-verify-20260927.py 和 results/reclaim-raw-20260927-* 保存可复核证据。下一项：namespace/takeover 间歇HTTP500。


# 跨带读检查点失去资格的 HTTP500（2026-09-27）

基于941f974。历史 namespace_conflicts_across_tapes_and_takeover 失败日志明确为读旧带前的写带检查点失去预留，却返回read_failed/HTTP500。原用例修改前30次均通过，不能靠概率复跑证明修复。新增确定性集成测试在单驱动器跨带读取的检查点WRITE注入预留抢占UA，1.10秒复现相同500；另加介质错误对照。

根因：检查点错误分支统一返回Failed；CommitFailed又把底层错误转成String，使资格分类丢失。修复保留Box<TapeError> source链，ownership_lost递归分类；检查点已经关闭服务、上报Lost，资格错误返回Busy/HTTP503，让尚未向客户端发送数据的读取换节点重试。介质错误仍为500，不重放部分输出。日志文本与提交阶段保持，Rust CommitFailed构造字段由reason:String变为source:Box<TapeError>，外部直接构造/匹配字段的调用者需适配；无持久格式、协议或依赖变更。source属性用于错误链，不是告警屏蔽。

软件：213通过7忽略，fmt/check/严格Clippy零告警。两个新测试分别验证自动换届读回与介质错误不隐藏；原namespace用例修复后30次通过。兼容构建成功，外部Zig废弃优化参数提示保留。

Holo隔离目录 /home/rocky/tape-rs-namespace-20260927，ltfsd SHA256 1040995424f01e4f7060cf617806f48179af51598bd2b8f0a55db39b53049442。从reclaim-raw关闭数据复制；仅本隔离启动脚本配置一台TAPERS驱动器。SR2502L8新增24B测试文件并保持载入，原数据在SR2501L8，确保读取seed必须经过换带检查点。node3 round147处理读取，strace确认第二次库存读取完成时SIGSTOP；node2 round152隔离设备并服务，再恢复旧节点。旧HTTP实际503/drive_busy，保留执行资格丢失、新旧PR键信息。新缓存FUSE六链接/内容/mmap/seed/条码复验通过。

删除测试文件后18目录记录（原17条除代数外不变，加一条墓碑）。正常停止后SR1 Full7（15个文件元素、六链接），SR2 Full3（一个已被目录墓碑取代的旧副本）；槽8/9。关闭数据namespace/test-data、node2 round152；下次恢复双驱动器配置再做维护。前后介质备份 /home/rocky/holo-namespace-20260927 已逐文件SHA验证。暂停过程中sudo监督进程也停止，清理时发现已退出子进程为zombie；校验父子PID/命令后SIGCONT监督进程完成回收，探针补入同样清理步骤。首次删除误用/sr/checkpoint-current被拒不存在，改用实际目录路径/checkpoint-current成功，没有删除其他数据。

正式服务恢复node1 term47/round429，53行目录及池摘要完全不变，TS1001L08 Full120/TS1000L08 gen2，原9文件复验通过；正式二进制仍32bd5541…。EE三节点available、无任务、f3哈希不变、九publication ready。隔离进程与strace停止，FUSE卸载缓存清空。无物理设备验收。接续任务3：回收介质IBM LE往返。

可复核材料：probes/namespace-checkpoint-20260927.py、namespace-read-20260927.py 及 results/namespace-20260927-*。生产源码只改错误链与返回分类，测试故障注入仅在测试transport。

## 2026-09-27 回收介质 LE 往返通过

见 holo-reclaim-le-20260927.md。最新关闭reclaim-le/test-data，SR1 Full8/SR2 Full3；正式term49/round439，53行/9文件一致。新LE SR2501L08 Full8取消分配；原TS1000L8恢复。下一项大目录性能。

## 2026-09-27 大目录优化通过

见holo-directory-perf-20260927.md；最新关闭directory-perf/test-data，SR1 Full24/SR2 Full3、1132行。正式term50/round444，53行/9文件不变。214测试通过7忽略，严格Clippy通过。下一项5。

## 2026-09-27 无人值守队列0–5完成

见holo-sync-boundary-20260927.md；218软件测试通过7忽略，严格Clippy无告警且无新增屏蔽。同步后Full30、SIGKILL与node3接管冷读通过。最新关闭sync-boundary/test-data，1136行、SR1 Full30/SR2 Full3；LE副本SR2501L08 Full10已与SR1分叉，禁止覆盖。正式恢复node2/term51/round449，53行及原9文件不变；新程序仅隔离验证，未正式升级/推送，物理设备验收暂缓。

## 2026-09-27 新剩余队列第1–2项完成

用户授权按 remaining-implementation.md 顺序完成文档、目录未定根因、LE权限时间、性能、正式发布。文档 da34019。目录根因见 directory-outcome-20260927.md：旧Leader HTTP窗口的未定是正确语义；持久化测试绑定新节点，另加未定不重放用例，客户端保留服务端原因。219测试通过7忽略、严格Clippy/fmt/check通过；160次复测和实际Holo故障/冷FUSE通过。

最新关闭 /home/rocky/tape-rs-directory-outcome-20260927/test-data，node2/round246，1139行，SR1 Full41/SR2 Full3，slot8/9，备份 holo-directory-outcome-20260927。正式恢复node1 term52/round454，53行逐字段不变，原9文件复验通过；正式二进制未升级。隔离/strace/FUSE已停止缓存空。下一项3：LE权限时间语义。物理设备仍暂缓。

## 2026-09-27 新剩余队列第3项完成

见metadata-20260927.md：原生readonly/atime/mtime、固定挂载所有权；FUSE/Client/HTTP贯通，LE双向回写及接管冷FUSE通过。全套221通过7忽略，新增边界1项通过；严格Clippy/fmt/check通过。最新关闭metadata/test-data，node2 round295、1143行，SR1 Full52/SR2 Full3槽8/9；LE SR2501L08Full52未分配。介质备份holo-metadata-20260927。正式恢复node1 term53/round459，原53行及9文件一致，正式未升级。下一项4：性能基线与按证据优化，然后第5项正式发布。

## 2026-09-27 性能修复与新增LE对照任务

详见performance-20260927.md；代码af6bfad，226项通过7忽略，严格Clippy/fmt/check通过。范围查询、旧根回收、只读卷索引复用及接管识别已装载读带修复，3273文件两轮冷挂/接管验证通过。最新关闭performance/test-data 6993行，SR1Full94/SR2Full18归槽8/9；正式旧版已恢复node1term54round464，53行及9文件不变。用户新增LE与直接Rust库性能/换带对照，先完成再执行正式升级；rollout候选已备份暂存未启用。新PF2701L08/PF2702L08槽1034/1035；LE空白格式化失败并已归槽，尚无对照数据。不得覆盖原LE两带。

## 2026-09-27 性能对照与正式升级完成

剩余队列1–5完成。LE/直接库各两轮3210文件全量SHA和每轮6次换带通过，见performance-compare-20260927.md。更正：LE普通sync只保证DP索引，不能直接称与Rust双分区commit同等Full终点；SCSI建议尚未实施、未采命令轨迹。PF两卷已归槽/unassign，LE末态保留。正式af6bfad部署至tape-rs-rollout-20260927，node1 term56/round499，58行三节点一致；新8MiB促使切至TS1000 Full13，原9文件及元数据冷挂/接管通过，详见rollout-20260927.md。FUSE卸载缓存空，隔离performance/test-data是唯一最新关闭数据。物理验收暂缓，无push。

## 2026-09-30 P1 gen13 recovery simulator characterization

新增 `tests/sim_recovery.rs::p1_full_generation_13_direct_eod_matches_legacy_scan`：SimTransport 上 gen13 双分区卷的 LOCATE EOD 与旧 SPACE EOD 位置一致，恢复仍判 DP/IP Complete；focused test 通过，结果见 `/work/tmp/codex-flow-r8ub2k_3/result.json`。这没有复现 `Unknown`：SimTransport 将两种 EOD 定位建模为等价，不能据此证明 IBM 固件语义；未改生产代码。`.209` 现场复现仍受阻：已核实的 LTFS 进程持有 `/dev/sg4`、`/dev/sg7`、`/dev/sg8`，故未执行 SCSI 或介质操作。后续需等 LTFS 工作负载安全释放并确认准确测试介质/设备组，再采每分区 LOCATE EOD completion/sense、READ POSITION 及 recovery notes、VCI 命中和候选索引数据。

## 2026-09-30 `.209` LTFS unmount 状态未确认

用户授权对 `/mnt/smartiq/LTFS_55L3A78011W9LL01` 做一次正常卸载。Hermes A2A execute 任务约 300 秒后 `TASK_STATE_FAILED`，返回 `[agent did not reply in time]`，没有可用工具事件或最终命令证据；因此远程挂载状态及是否已卸载均为 **UNKNOWN**，不得盲目重试。随后只读 Hermes CLI 核查无法认证：BatchMode 被拒，五个候选 key 的 hostname 探针也全部被拒；没有取得远程 mount/PID/users 检查结果或已验证卸载，也没有可确认的 SCSI、介质或服务操作。证据包：[A2A 卸载任务](probes/results/hermes-209-normal-unmount-a2a-20260930/) 与 [CLI 只读状态核查](probes/results/hermes-209-unmount-state-20260930/)。后续须先具备可靠的非交互授权 SSH/A2A，再做只读状态核查；核实现场状态后才可决定是否需要任何后续卸载。

本机随后只执行一次 `ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 root@10.131.7.209 hostname`（外层超时 15 秒），exit 255：`Permission denied (publickey,gssapi-keyex,gssapi-with-mic,password)`；未尝试其他认证方式。结果见 `probes/results/hermes-209-ssh-auth-check-20260930/codex-flow-ks_huzrr/result.json`。
用户随后已授权使用一次交互密码安装专用受限 root key。`probes/setup-ssh-209-auth.sh` 在单次 SSH 会话中先确认 `hostname=lisa-3`，再成功安装；随后 BatchMode、专用身份及严格主机密钥检查下的 hostname 验证也通过。详见 [SSH 修复与只读状态核查记录](probes/results/hermes-209-ssh-auth-repair-20260930/README.md)。

之后 Hermes A2A 只读挂载/owner 核查任务 `task-620dba6e02524626` 约 300 秒后以 `[agent did not reply in time]` 失败，无 terminal events 或远端命令记录；A2A 结果本身不能确定挂载状态。随后 Codex 使用已验证的受限 key 直接 SSH fallback：21:52:44 CST 只读检查在 lisa-3 确认目标 FUSE mount 存在，`fuser` 无 PID；21:53:44 CST 执行唯一一次普通 `umount /mnt/smartiq/LTFS_55L3A78011W9LL01`，exit 0。之后 `mountpoint` 确认目标已卸载，`findmnt` 解析至根文件系统 `/dev/sda7`。当前目标已确认卸载。此 fallback 未执行任何 SCSI、磁带、驱动器或 changer 操作。保留此前对一次正常卸载的授权；没有额外卸载。证据：[只读挂载检查日志](probes/results/hermes-209-ssh-auth-repair-20260930/codex-readonly-mount-check.log) 和 [正常卸载日志](probes/results/hermes-209-ssh-auth-repair-20260930/codex-normal-unmount.log)。

## 2026-09-30 `.209` A2A 只读 journal 能力核查被本地 consent gate 阻止

Hermes A2A transport 任务 `task-22e82e9712c94178` 已完成并收到 push notification，但本地 consent gate 因 1 分钟内未收到确认，在 SSH session 打开前阻止了 hostname/journalctl 命令；无远端事件或输出，两个命令均未执行。按 gate 要求未重试。证据：[structured-output.json](probes/results/hermes-209-a2a-journal-check-20260930/hermes-worker-s774jpd3/structured-output.json) 及同目录结果文件。

## 2026-10-14 GAP 补全实机验收进行中（不要重跑阶段）

依据 reports/rea-le-tape-rs-integrated-20261014.html，在当前分支 2f49b92 上新增流式索引恢复、library::Library/LibraryBackend、tape-library 管理 CLI、tape-fuse 直接逻辑库模式，以及 library_acceptance/library_gap_fixture 两个 Rust 示例。用户明确要求真实验证并确认库内所有磁带均为测试带；现场用凭据恢复连接（凭据不写入文件）。保留此前 sim_recovery.rs 和本交接已有修改；不修改旧 feature/docs 工作树。

软件：cargo test --all-features 273 passed/8 ignored；cargo test 272 passed/1 ignored；check/all-targets、strict Clippy -D warnings、全仓 fmt 和 diff-check 通过；本机显式 kernel_multi_volume_library 通过（SimTransport，非物理介质）。新夹具 example 随兼容构建和 Clippy 检查通过。Zig 0.16/0.17 不接受 cc-rs generic CPU flags，compat wrapper 将 x86-64/generic 组合映射为明确 -mcpu=baseline；使用 /snap/zig/16117 Zig0.16 完成 glibc2.34 release 构建，失败尝试日志保留。

实机 lisa-3 / 10.131.7.209：changer55L3A78011W9LL01 / sg5（LE另一路sg3），TD8 117018605B / sg4/offset0、HH8 117018606F / sg2/offset1。初始两 drive 空、无 holder/PR/registered keys，node-scheduler inactive；TS4300 changer PR capabilities支持 EA。隔离候选与状态目录 /root/tape-rs-gap-20261014，bin/ 下有 tape-library/tape-fuse/tape-rs/library_acceptance/library_gap_fixture，资源库 library.sqlite，节点250。实际控制轮次2026101400xx。不部署正式 Holo。

活动脚本 gap-real-20261014.py basic（经 SSH 启动，Codex exec session2297），结果 results/basic-run.log、events.jsonl、*.started/*.complete/*.failed，CLI 每命令 stdout/stderr 独立保存。首次 list /test/AA0013L8 已成功（117.696s，含归槽）；现在写 AA0013L8 gap-20261014-basic/marker.bin。所有7数据带已分配到test池，cleaning带不分配。**先查进程/阶段文件/SQLite只读状态，禁止重复启动 basic 或覆盖运行二进制。** 本机SSH复用 socket /tmp/tape-rs-gap-209-20261014.sock；会话过期需可靠认证，不能把认证成功假定为设备状态。

余下顺序：basic完整native两轮SHA → gap-real... fuse（真实FUSE目录/读写/fsync/xattr/rename/link/EXDEV）→ management（blacklist/NEED_ASSIGN/再assign）→ large（追加1040×64KiB root xattr +独立marker，旧二进制64MiB受限对照，新Rust两轮SHA +FUSE大索引）→ gap-le-large-20261014.py（独立LE只读互操作、正常卸载）→新Rust接管再次读回→最终库存/PR/holder复核与取回证据。

脚本无FORMAT/ERASE，不删除原文件。large阶段追加65MiB+标准root xattr，介质最终需明确记录仍保留此夹具；不得以旧百万文件卷（已格式化）的历史结果声称本轮百万文件性能验收。LE脚本只在large.complete门控后启动。每阶段 exclusive *.started 门控防止重复写入；遇到失败须先核实现场，再修复并用新的阶段/证据文件接续，不能删除门控后盲目重跑。

当前没有完成实机验收；docs/library.md说明单进程独占资源库与原HA catalog分离、changer PR不支持时安全拒绝、直接Rust单层卷内相对链接限制。真实证据本机 probes/results/gap-implementation-20261014，后续必须刷新远端results；第一轮试验失败log也保留，不冒充通过。

### 2026-10-14 实机发现 append-only IP 写入缺口，basic 已停止

basic阶段 v1 首次list通过，但fixture-AA0013L8在IP opening FM报DATA PROTECT 07/5A/02；DP Full已写，结果未定，程序正确失败并归槽释放PR。当前basic.failed/started保留，不得重放原脚本。两台drive空，AA0013L8槽1，原perf64m数据未主动修改；新增marker是否已发布要按恢复视图校验。原日志本机results/gap-implementation-20261014/physical-first/，远端results原样保留。

只读sg_modes确定HH8 MP10h/01h byte5=0x12（WRITE MODE1 append-only），TD8=0x02（WRITE MODE0）。IBM GA32-0928-08 §4.2/5.2.1/6.6.12说明AO在非append点返回5A02；不能凭字面把它当整卷物理写保护。新增82h action1、partition0/block4位置绑定一次授权：不提供Format授权、不关闭AO；PR guard必需，READ POSITION不吞UA且检查LOLU/PERR/精确位置；授权后仅第一IP opening FM消耗许可，其后沿EOD写。mode子页不支持24/00沿用旧路径，截断或未知mode拒绝。模拟器先红（同07/5A02）后绿，另覆盖无PR、位置不符、复位UA不重试；新增mode响应边界/CDB端序测试。归槽会清current failure，因此资源库新增last_failure迁移列，保留最近失败阶段/原始错误，close后保留测试通过。

v2兼容构建进行中（exec85753）；下一步部署到独立bin-v2，不覆盖bin/v1，先用新Rust library_acceptance --checkpoint校验已提交marker与原perf64m，再仅补IP，不重放append。gap-real脚本新增resume_basic阶段（一次性.started门控），后续fuse/management/large轮次提升为20261014008x/09x/10x/11x。未开始resume_basic，须待build-v2完成、确认无holder、保存v2 SHA后执行。LE脚本仍在本机，等large.complete之后再部署运行。

### 2026-10-14 GAP 补全实机主体通过，最终清场受 FC Linkdown 阻塞

最终实现与证据汇总见 `reports/gap-implementation-validation-20261014.md`。bin-v3 SHA 保存在远端 `results/v3-sha256.txt`。最终软件 `cargo test --all-features` 为278通过、9忽略、0失败；严格Clippy/fmt/diff-check通过；百万文件显式纯软件测试19.201秒通过。docs/library.md已补缓存状态、append-only和checkpoint语义。

实机：resume_basic通过；真实FUSE基础模式通过7卷namespace、双卷冷读、64MiB SHA、写/fsync/binary xattr/rename/symlink/EXDEV及两卷独有内容；management通过blacklist/NEED_ASSIGN/reassign。大索引夹具在AA0013L8写入1040×64KiB root xattr和marker，旧版按67108864预算失败，新版list、直接Rust两轮四路径SHA（613.763s）、真实FUSE大索引/64KiB xattr均通过。IBM LE列7卷，marker/64MiB SHA/64KiB xattr通过，普通卸载rc0、进程exit0，LE日志明确clean/self pointer IP而跳过index写入。

硬件事件1：大索引DP commit后sg5路径lpfc devloss；用同一TS4300备用sg3、原round/key完成两轮读、checkpoint、归槽和PR释放，未重放65MiB append。硬件事件2：LE成功后post-LE Rust冷读在LOAD上收到host status0x000d，随后HH8/changer ENXIO并从lsscsi消失；WWPN50:00:e1:11:70:18:60:70 devloss。当前host15/host16均Linkdown，无设备/FUSE/LE进程。AA0013物理位置和PR须等链路恢复后核实，不能猜测或盲重试。

链路恢复后：按VPD重新解析sg节点；先库存/PR核查，用失败round202610140130接续清场或确认无旧key后新round；再跑post-LE两轮四文件SHA；最后确认drive空、AA13/AA00槽1/2、两设备无PR、mode状态。远端失败证据 `post-le-native.stderr` 与kernel journal必须保留。


## 2026-10-19 — 主分支交付：HA、LE互通与自适应合批索引

本次发布从 GitHub origin/main 的 0cc1042 合并历史接续，整合当前已验证的源码、回归测试与正式文档。实现包括库管理及任务调度、控制/设备所有者协调与执行许可、EE同代恢复索引互通、挂载会话复用，以及根据实际索引块数选择 Full/增量的 sync_batch。保持现有持久化屏障、VCI 与设备预留保护；显式增量接口保留，链上限不放宽。

发布前在仅含已提交文件的干净工作树复核：cargo fmt、cargo test（390通过、2忽略）、example探针测试（3通过）、cargo check --all-targets、cargo clippy --all-targets --all-features。原始EE XML夹具按采集字节保留，其中DP57末尾空行未作格式归一化。正式行为及测量边界见 docs/library.md 与 docs/research/raft-ha-evidence.md。

隔离实机验证了自适应Full和增量两条库路径，64个文件经存储哈希或外部SHA-256实读核验，介质已归槽并恢复EE实际双PR。新ltfsd构建未替换正式服务；此前完整HA故障矩阵对应原e9fd构建，不能当作本次新构建的完整HA复验。测试日志、临时脚本和原始现场产物保留在本地验证工作树，不作为源码编译依赖。

本机接续仍使用 /work/jay/tape-rs-ltfs-recovery-ibm-interop 的 ltfs-recovery-ibm-interop 分支，提交以 git log -1 为准；GitHub交付目标为main，未改动另一个feature/docs工作树。现场无在途任务或新增临时SSH密钥。后续按新任务推进，不重跑已消费的现场one-shot标记。


## 2026-10-19 — 严格 HTTP VIP 通过 review，接续迁移到 main

用户要求 review 通过后合入 GitHub main，并删除开发 worktree。日常入口改为 /work/jay/tape-rs 的 main；旧 feature/docs 内容、未提交入口修改和开发目录全部内容备份到 /work/jay/tape-rs-local-archive-20261019，详细本地交接保留在 .scratch/ltfs-ha/local/handoff-history.md。主分支代码之外的现场原始证据保留本地，不作为源码依赖。

本次 VIP 不依赖 Keepalived，只支持同子网 IPv4。已提交预约与本地 Serving 许可共同控制挂址；旧节点删除 ACK、回读为空及精确 token 释放提交之前，不允许新 owner 挂址。失联或无多数派时宁可暂停，不能通过等待、清理共识数据或强制释放跳过。升级须整组统一版本，详见 docs/http-vip.md。

Standards / Spec 两轴 review 均无可操作发现；完整软件检查和发布 SHA 以本地 probes/results/http-vip-review-20261019 的最终记录为准。此前候选真实三节点测试观察到正常停机约15.16秒恢复VIP，暂停/强杀后旧地址约1.82/1.99秒消失，旧节点恢复/重启后约.49/.55秒恢复入口；新owner正常ready后约.13秒挂址。无双VIP或持续地址残留；暂停恢复曾观察到短暂旧地址重挂后立即删除，严格确认仍在新地址发布前完成。无磁带分配、装带或数据写入，未覆盖写入中恢复或掉电/内核冻结。

现场已收尾：三台测试服务和观察器停止，物理VIP及Raft holder均释放，token墓碑和共识目录保留；EE恢复可用、无活动任务并回读原双设备唯一Exclusive Access预留。不要重跑已消费的现场one-shot标记。详细测量与证据见本地reports/http-vip-physical-20261019.html及probes/results/http-vip-physical-r2-20261019。


## 2026-10-20 — HTTP 接入迁移至 axum（本地软件验证完成，未部署）

在 /work/jay/tape-rs 的 main（基于 1c2ed45）完成迁移，改动尚未提交。删除手写请求行/请求头解析和每连接创建线程的处理方式；axum Router 承接 HTTP 请求，Hyper HTTP/1 连接显式设置请求头总量 32 KiB、头部接收期限 30 秒，并保留每请求后关闭连接及原有响应头大小写形式。新增 axum / Tokio / Hyper 及必要流式适配依赖，未改动 SCSI、设备执行器、Raft、持久化格式或客户端协议字段。

固定内部上限为每节点 256 条连接、32 个业务请求；业务额度用尽返回 503 http_busy，连接额度用尽关闭新连接。阻塞任务和下载响应体持有业务额度直到完成/释放；/readyz 独立于业务额度，继续共用 ready_owner 和实时 Serving 许可判断。同步业务在 spawn_blocking 中运行，不对已受理设备操作增加整体 HTTP 取消期限。上传逐帧读取并沿用 60 秒无下一帧期限、准入后 100 Continue、abort/finish；下载从匿名暂存文件异步流式发送，保留 Range，并明确报告暂存内容截断。接口及限制见 docs/file-client.md，线程示意同步更新 docs/architecture.html。

验证：cargo fmt --all -- --check、cargo test（413 通过、4 忽略）、cargo check --all-targets、cargo clippy --all-targets --all-features、git diff --check 均通过，Clippy 无告警。最后移除 JSON 响应额外复制后重新运行 HTTP 七项测试、check、Clippy 和 fmt，均通过。覆盖冲突 Content-Length、超大请求头、准入拒绝时不发送 100、业务满载时 readiness 可用、响应体存活期间保留额度、取消 HTTP 等待后已受理命令仍能接收结果、下载暂存截断报错；新增模拟集群实 HTTP 上传断流后重新上传/实读通过。现有大文件、Range、客户端和 HA 集群回归通过。构建使用 /tmp/tape-rs-axum-target 和 /tmp/tape-rs-axum-check-target，避免竞争全局共享构建目录；日志 /tmp/tape-axum-{tests-final,http-final,check-final,clippy-final,fmt}.log。

CodeGraph 已 sync，并通过 affected 与 explore 检查 HTTP 相关路径；affected 本轮未列出测试文件，实际测试覆盖以上述执行结果为准。未连接远端、未部署、未访问真实设备，Holo/物理带库与现场 VIP 回归均未执行，仍待现场验证；未做性能对照测量，不承诺吞吐提升。现有现场状态沿用上一条记录，本轮未重新核查。


## 2026-10-20 — axum 实机验收完成；修复接管时未重新 LOAD 的旧缺陷

用户明确要求真实环境验证，并提供临时 SSH 认证。工作树仍 /work/jay/tape-rs / main，基于 1c2ed45，源码、测试和本交接改动未提交。三台实际物理节点 lisa-1/2/3（10.131.7.207/208/209），目标 changer 55L3A7802K19LL01 / sg3、TD9 11EB4A80F1 / sg2，逐机 VPD 核对。真实测试带仅 RC0018L9（槽7），RC0017L9（槽8/EE REA_P0）未读写或移动。测试使用原隔离 /root/tape-ha-hw-20261016/state、节点71/72/73和17820/17821，保留 Raft/目录状态并事先离线备份；候选在独立 /root/tape-axum-20261020 和 /root/tape-axum-r2-20261020。正式 Rust 构建未替换。

R1：新 HTTP 协议、32业务额度、64MiB上传/下载SHA、Range、只创建、断流释放、删除以及三种故障后的读取通过，但计划停机后 RC18 被标记 check，新写准入507。真实轨迹表明旧 owner UNLOAD 后盒带仍留驱动器，新 owner 首条 LOCATE(16) 返回02/04/02（需初始化），原 open_candidate 未先 LOAD。此路径在HTTP迁移前即存在。新增模拟回归稳定先红（NoTape/CHECK）、修复后绿；仅在已隔离后的 open_candidate 增加显式 LOAD 和等待就绪，保持原SCSI超时/编码/预留/持久化屏障。没有改控制 Leader/设备 owner 的选择策略：分配时优先合格 Leader，健康 owner 保持，不随控制选举主动迁移。

R2 gnu.2.34 release ltfsd SHA256 ac16bec88787f4bbd061ac197b97239fafe7bcecc2103ebdf9d48f0280fc4d86，三机部署逐一核对。先独立隔离/装带、确认原卷UUID4d78dbe5-5d35-4bb1-a0f3-ca2621702349且无只读限制，ltfs-verify实读56个已有存储哈希全部Match，另10条NoHash未计作通过。再UNLOAD但保留盒带于驱动器，以复现R1起点；设备访问全部被cgroup禁止时，通过现有可信Raft提案帧提交唯一TapeState(RC18,appendable)，三机回读提交后才恢复SG访问。此操作有物理校验证据，不清库/回滚数据库，不改投票、成员或VIP授权与ACK。

R2：32业务请求占满时503 http_busy，/readyz仍200；冲突Content-Length400、超大头431、chunked上传411。64MiB文件实际写入/读回SHA一致，Range206、只创建412且不要求正文、断流路径与容量释放后可重新上传、DELETE提交200均通过。计划停机、SIGSTOP/CONT、SIGKILL/重启各自之后均读回64MiB、再新写入及读回4KiB；最后独立再次读回全部三个已确认的小文件，哈希均一致；原64MiB文件故障前后SHA一致，R2起点已有65个活路径长度保留。新增测试路径 comparison/http-axum-20261020 与 comparison/http-axum-r2-20261020 留在带上，含两轮64MiB文件、第二轮三份4KiB标记及删除墓碑；不回收/格式化。

测量：正常207→208旧IP0.231s消失，新owner112.011s ready、112.193s VIP HTTP，113.421s完成64MiB读回、129.914s完成新写读回。暂停208→207旧IP1.867s消失，新owner340.500s ready、355.540s VIP HTTP、380.490s完成新写读回；强杀207→208旧IP1.845s消失、新owner340.115s ready、355.206s VIP HTTP、380.198s完成新写读回。两异常场景人为旧节点未确认观察门槛至少5秒，含不可达HTTP请求等待的实际观察约14.2秒；恢复旧节点后约0.770/0.821秒新IP发布。705条独立Netlink地址事件无重叠、无观察器错误，本轮没有旧节点瞬时重挂事件。跨主机地址时间保守40ms误差，HTTP100ms采样；控制机与现场约33秒偏差已单独校准。约340秒恢复就绪主要耗在实际索引历史回溯，不把它解释为HTTP框架开销或承诺固定RTO。

额外HTTP-only进程位于同一物理208、17920/17921，SG访问完全禁止、独立状态目录：256条连接占满时第257条关闭，头部30.001s超时、正文59.999s超时，之后正常停止。第一次探针漏配CLI要求的至少三个成员，启动参数校验拒绝、未触及设备，原失败日志保留，修正探针后通过。R2进程每5秒资源采样峰值RSS约27.4/32.4/21.7MiB，线程峰值46/18/15；没有旧HTTP同口径A/B，不能声称已测吞吐提升。守护进程SG_IO仅sg2/sg3，1128次READ、146次WRITE、4次LOAD、2次UNLOAD；无FORMAT/ERASE。维护CLI另有记录，不能把守护进程计数当成全部现场命令计数。

软件：最终cargo test 414通过/4忽略，fmt、check all-targets、clippy all-targets/all-features无告警、diff-check通过；新回归含真实调用序列的模拟卸带后接管和实HTTP读写。CodeGraph sync并explore open_candidate路径完成。源码额外改动为executor四行和回归，HTTP迁移其余不变。

现场已安全收尾并独立复核：R1/R2所有测试及观察进程MainPID0，无VIP，三机Raft holder=null、token26239墓碑保留，原ens9f0地址集合保留。控制面磁带摘要为gen98、files=71（含内部墓碑）、appendable；不将缓存代数当成另行冷挂验证的物理末代。驱动器空，RC18槽7、RC17槽8；EE208available/enabled、207disabled、无活动任务，恢复drive0x400000000a8307d0/changer0x500000000a8307d0的各唯一登记及Exclusive Access(type3)。无BMC、FC断链、掉电、内核冻结或写入中的故障注入；这不是完整HA故障矩阵，也未做Holo回归。

报告 reports/http-axum-physical-20261020.html；R1与R2证据分别在probes/results/http-axum-physical[-r2]-20261020，R1失败和CHECK证据保留。R2 summary.json、committed-markers-final.json、post-close-proof.json、scsi-opcodes.json、validated-*、clock-calibration.json、各node tar和软件/构建日志均已保留本地。one-shot requested/files.requested已消费，不得重跑。后续如继续部署，使用上述已验收候选并先重新核对现场；不要恢复pretest-state.tgz覆盖已推进的状态。

本轮临时SSH ControlMaster已全部关闭，无新增认证key或凭据文件；现场读写与清场脚本最终exit0。

### 2026-10-20 — Leader/owner 对齐策略（已验收）

用户明确调整策略：控制 Leader 具备目标设备组有效访问资格时兼任 owner；只有不具备资格时才使用其他节点。coordinator 在健康 owner 为 follower 时也会向合格 Leader 提议 AssignExecutor，继续使用 expected_round、实例号、ReadIndex 许可、PR 隔离/卷恢复与严格 VIP 撤址确认。资格沿用连续探测、设备组匹配、新鲜度与冷却，非仅看有无 FC 卡。恢复的普通 follower 不抢占；控制权迁移带来设备接管和服务中断，收敛期间允许临时不同。

新增控制权转移后迁移并保留已提交数据的模拟用例；原“Leader 恢复 FC 不抢健康 owner”用例改为不撤销旧 owner 资格也须接管并读回数据；原控制换届用例改为与新合格 Leader 对齐。12 项 HA 测试通过。完整软件并行首跑在原 500ms 许可场景6/9失败（Leader3无设备、新分支未触发），独立重跑12项通过；保留 /tmp/leader-owner-{tests,ha-repeat}.log，完整串行回归已415通过/4忽略，fmt、check、clippy均通过。并行首跑失败原因尚未定论，不以重跑通过掩盖，未修改短时限；最后默认并行完整回归也已415通过/4忽略。

实机探针 probes/leader-owner-physical-20261020.py，唯一结果目录 probes/results/leader-owner-physical-20261020；requested 已消费，禁止重跑。三机候选 /root/tape-owner-20261020、unit tape-owner-20261020.service、observer tape-owner-observe-20261020.service。复用既有 /root/tape-ha-hw-20261016/state，未清除/回退状态，均离线备份。设备序列及 sg2/sg3 同前，RC18原槽7、RC17槽8，EE无任务后已正常暂停。所有SG先禁止；208控制Leader，逐步仅207允许设备访问，后恢复209和208，再转移控制权至209；新增文件仅 comparison/leader-owner-20261020 的三份4KiB标记。自动 finally 沿用已验证的归槽/释放/恢复EE/确认VIP空流程。现场已全部通过并收尾，详细结果如下。


Leader/owner 实机结果：gnu.2.34 候选 ltfsd SHA256 cbaef10a82e93529d8aba21713d93bdbdc9a3253f0d1e10126973001173cd6ba；三机分别核对。208 禁止 SG 访问时保持控制 Leader，207 成为 owner；恢复普通 follower209 的设备访问不抢占；恢复 Leader208 的访问后自动从健康207接管；正常 Raft MsgTransferLeader 转给209后，owner从健康208迁到209。每个稳定状态观察12秒，两次主动接管从触发至VIP可用330.591/330.286秒。cgroup仅模拟无有效设备通路，未物理拔卡/断链，未进行写入中故障/断电/内核冻结。

三阶段各新写/读回4KiB标记，最后在209重新读回全部三份并核对SHA c8f5d0341d54d951a71b136e6e2afcb14d11ed8489a7ae126a8fee0df6ecf193；此前R2 64MiB也实读SHA281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6一致。新增路径仅comparison/leader-owner-20261020/{control-only-leader,leader-regains-devices,control-transfer}.bin，保留于RC18，无格式化/擦除。572条独立Netlink地址事件未观察到重叠或观察器错误。正式Rust服务未替换。

用户追问330秒拆分，已按每条SG_IO wall_us统计：208/209分别LOCATE(16)54/55次，307.804/309.168秒（总时长93.1%/93.6%）；LOAD8.592/9.044秒；READ(6)107/109次共8.951/8.960秒；剩余资格/控制/其他IO/循环等待和发布合计5.245/3.114秒。设备双PR隔离约12.3/24.7毫秒；最后恢复设备IO至VIP发布1.724/1.397秒。计时为kernel/HBA/device wall wait，非CPU；VPD探测并发，不叠加为串行耗时。日志逐次回溯至早期DP索引块；代码不满足clean_full_pair时执行verify_chain沿previousgenerationlocation核验历史，不应把全部回溯称为增量重放。现场未记录快路径每一条件，哪项未满足尚不能单凭日志断言。优化重点应为安全减少历史随机定位，不改超时或削弱隔离校验；本轮未修改恢复策略。

软件最终完整串行和默认并行均415通过/4忽略，fmt/check all-targets/clippy all-targets all-features/diff-check通过，CodeGraph sync及affected/explore完成。首轮并行两项短许可HA失败已保留，后独立HA和完整复核通过；首次失败原因未定，没有修改测试超时。证据probes/results/leader-owner-physical-20261020，summary.json、timing-breakdown.json、software-verification.json及software-*.log，报告reports/leader-owner-physical-20261020.html。requested/measurements.complete/cleanup.complete已消费，禁止重跑原one-shot。

独立收尾复核通过：三机测试/观察进程MainPID0，无VIP，holder=null、token27964墓碑保留。控制缓存RC18 gen102/files74/appendable（未额外冷挂宣称物理末代）。驱动器空，RC18槽7/RC17槽8，EE208available/enabled、207disabled、无任务，drive0x400000000a8307d0/changer0x500000000a8307d0各唯一注册+Exclusive Access(type3)。无清库/回滚/强制VIP释放。临时SSH已关闭，无新增凭据文件，现场脚本exit0。

### 2026-10-20 — 减少接管历史随机定位（已验收）

用户要求研究安全减少历史索引定位。已使用 diagnosing-bugs 快速回归复现：tests/sim_recovery.rs::committed_dp_full_with_older_ip_has_bounded_mount_locates，经守护进程同款 sync_batch 连续40次小文件提交，均为DP Full，IP滞后、VCI双新鲜；旧代码挂载46次LOCATE/历史深度40，先红。保守扩展clean_full_view：实际两分区末构造完整、VCI均新鲜且代数匹配、DP为非materialized的最新Full，IP同代回指须精确匹配DP或旧代回指须指向DP更早物理位置时，普通挂载跳过历史Full核验，保留IP债务。所有EOD/UUID/自指针/尾部/追加位置、PR和屏障保留，显式recover/mount_with_budget继续历史审计，增量/异常/失效提示仍原路径。未改CDB/超时/格式/配置或依赖。依据SNIA LTFS2.5.1 §9.2 Full自包含与§10.3 VCI，已核对官方PDF。

现快回归6次LOCATE/history_checked=false/depth0，模拟掉电后40文件均完整实读；新增失效/代数不符VCI双分区矩阵、旧历史损坏仍由显式审计拒绝、IP异常前向回指保留历史核验。针对恢复/增量/提交故障及全量419通过/4忽略，fmt/check/clippy/diff-check通过。源码src/ltfs/recovery.rs另加一次恢复INFO日志以记录IP/DP代数、增量、VCI命中、历史深度、IP债务和可追加结论，现场开启该命名空间。

实机新one-shot probes/recovery-fast-full-physical-20261020.py 已启动，原历史one-shot不重跑。结果probes/results/recovery-fast-full-physical-20261020；候选/root/tape-recovery-fast-20261020，unit tape-recovery-fast-20261020.service、observer tape-recovery-fast-observe-20261020.service，仍用原/root/tape-ha-hw-20261016/state节点71/72/73。仅RC18追加comparison/recovery-fast-full-20261020三份4KiB，复现“208无设备owner207→208恢复访问主动接管→转控制权至209并接管”。原64MiB和本轮三文件最终实读SHA，独立Netlink检查。相同已验收preflight/finally归槽释放恢复EE流程；当前尚在运行，不能宣称实机性能通过。


恢复优化第二阶段：R1仅放宽Full条件，实机当前Full已跨多个块、sync_batch实际写入增量，未覆盖本轮330秒瓶颈；控制器SIGINT后finally正常清场（exit130），结果标记incomplete.json，不能计性能通过。收尾时未就绪owner已持VIP预约，先停其他节点后欠多数派ACK；临时207控制节点明确禁止SG访问恢复多数派，随后以常规控制集群完成精确释放，保留墓碑。自动fallback曾与临时控制节点同机启动，受到本机VIP实例锁EAGAIN拒绝，未进入设备操作；停止临时holder并恢复标准peer后释放成功。未force release、清库或回滚。R1独立库存/EE/PR/MainPID/VIP复核已通过，临时SSH为下一轮继续使用。R1只新增control-only-leader.bin一个4KiB标记。

R2 fresh_current_index在增量重放前保存新鲜当前头条件；只有完整重放到Full成功后，普通挂载才跳过该基线以前Full审计。重放失败仍原受限路径，显式recover/mount_with_budget仍全历史，IP债务不清除。新增实际sync_batch增量回归40份Full+3份增量：50定位先红→9定位、3份全重放后绿（含模拟掉电全文件实读）；必要Full损坏拒绝、非依赖旧Full损坏普通视图可用但显式审计失败、错代VCI仍审计通过。更新原增量测试以断言必要增量重放2份而非强制历史审计。全量串行及最后默认并行422通过/4忽略；fmt/check/clippy均通过。并行首次HA转移用例因建池/写入后round176与初始round9对比失败，未改时限或断言，原日志保留。

R2实机probes/recovery-bounded-physical-20261020.py进行中；候选/root/tape-recovery-bounded-20261020，ltfsd SHA bd015e0f29ee7ab1889596e3a1b9dc074f9aa35b6e4c27391f9e8e9c08c5e04d；unit tape-recovery-bounded-20261020.service，observer tape-recovery-bounded-observe-20261020.service。已消费requested，禁止重跑。复用相同原状态、节点、17820/17821和RC18，源码/测试完整检查后部署。初始实机冷挂确认IP102/DP103增量，VCI命中true/true、重放1份、history_checked=false/depth1、IP债务true且可追加。即将测208接回、209控制转移；只追加comparison/recovery-bounded-20261020三份4KiB，末尾还复核R1确认标记及原64MiB。报告/结果仍待本轮现场完成。


### 2026-10-20 — 有界恢复最终实机验收通过

最终实现fresh_current_index与必要增量重放成功共同决定是否跳过更早Full审计：Full直接取当前快照，增量全部验证/重放到最近Full后停止。IP允许旧检查点但保留债务；双VCI新鲜、实际代数匹配、两分区末构造完整、自指针/UUID/回指方向/EOD/追加位置及原PR/执行许可均保留。错/旧提示、异常尾部、必要基线损坏或增量不完整不走省审计路径；显式recover/mount_with_budget按原预算审计历史。无新格式、依赖、CLI或配置，SCSI编码/超时及写入屏障不变。新增恢复INFO日志便于核对实际分支。

R2相同三物理节点、原RC18和原隔离Raft状态：208恢复FC资格接回，触发至VIP从330.591s降至30.535s（-90.76%），LOCATE54→9；转控制权到209后接管从330.286s降至28.883s（-91.26%），LOCATE55→10。日志分别确认IP102/DP104增量、重放2份、历史核验false/depth2，以及IP102/DP105增量、重放3份、历史核验false/depth3，均VCI命中true/true、IP债务true、可追加true。必要增量没有省略。对比为同设备/带/场景，但介质按追加写推进，不是假装同一份字节镜像；不承诺所有磁带固定30秒。

R2三份4KiB新路径comparison/recovery-bounded-20261020/{control-only-leader,leader-regains-devices,control-transfer}.bin分别写/读后，最终209再逐份实读SHA c8f5d0341d54d951a71b136e6e2afcb14d11ed8489a7ae126a8fee0df6ecf193一致；原64MiB SHA281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6及R1确认标记也实读一致。578条独立Netlink事件未观察到重叠/观察器错误；无FORMAT/ERASE，仅sg2/sg3。R1不完整状态和控制面清场记录保留，不能计作性能通过；R2 measurements.complete/extra-validation.complete/cleanup.complete均已生成、现场exit0，禁止重跑one-shot。

最终完整串行及默认并行cargo test均422通过/4忽略；fmt、check all-targets、clippy all-targets/all-features、diff-check和CodeGraph sync/affected/explore完成。新增回归覆盖Full历史40份46→6定位、40Full+3必要增量50→9定位（均模拟掉电后全文件实读），失效/错代VCI、旧IP异常回指、基线损坏拒绝、非依赖旧Full损坏只在显式审计失败。首轮并行旧HA测试round176与setup前round9断言失败保留，最终复核通过，未放宽断言/时限。报告reports/recovery-bounded-physical-20261020.html，结果probes/results/recovery-bounded-physical-20261020含summary、逐CDB、recovery日志、controller、原始node归档、software-verification、源码diff和clock-calibration；所有报告本地链接已核验。

独立最终核验：R1/R2/临时control-cleanup所有测试和观察unit MainPID0，三机无VIP且holder=null/token30564墓碑保留；RC18槽7、RC17槽8、驱动空；EE208available/enabled、207disabled、无任务，双设备各原唯一注册+holder(type3)，drive0x400000000a8307d0/changer0x500000000a8307d0。控制缓存gen107/files78/appendable（含内部墓碑，未另行冷挂宣称物理末代）。R2候选/root/tape-recovery-bounded-20261020/ltfsd SHA bd015e0f29ee7ab1889596e3a1b9dc074f9aa35b6e4c27391f9e8e9c08c5e04d，正式服务未替换。未进行本轮写入中故障、实机掉电或内核冻结；默认测试不接触真实设备。全部临时SSH会话及socket已关闭，无新增认证key/凭据文件。源码和文档尚未提交。

### 2026-10-20 — GitHub main 提交前 review 与最终复核

用户要求 review 后提交 GitHub main。本次以 1c2ed455103a2b7ba29e7fd0bee89c599e7d7241 为固定基线，审查全部已跟踪 WIP：axum HTTP、显式 LOAD、合格控制 Leader 兼任 owner、有界恢复、对应测试和文档。按 code-review 技能并行执行 Standards / Spec 两轴审查；Standards 无明确问题，Spec 发现一项 P2 测试问题，修复后复核无剩余问题。未发现生产实现阻断项。

修复 tests/ha_cluster.rs case11：原测试在建池和提交文件后仍比较准备前的 first.round，会将准备期间正常许可失效/重接管误判为 follower 抢占。现在准备完成后重新取得 fenced/ready 且 serving_round 精确匹配的 owner，启用 follower 后连续 500ms 核对已取得资格且控制 Leader、owner 节点/轮次/实例不变；保留原 4s 等待上限、控制权转移后的迁移及已提交数据实读。此前 round9→176 失败记录仍保留，不以重跑抹去。

最终代码完整 cargo test 422 通过 / 4 忽略；cargo fmt --all -- --check、cargo check --all-targets、cargo clippy --all-targets --all-features、git diff --check 均通过，check/clippy 无告警。review 修复前的首轮完整检查也通过。日志 /tmp/tape-review-20261020，最终轮 final/verification.json 和各检查日志。CodeGraph sync、affected 和相关调用路径检查完成；affected 未推导出测试，不替代实际全量测试。

已逐文件比较当前生产源码与 R2 实机 source.patch：Cargo.toml/Cargo.lock 及 http/coordinator/executor/recovery 均完全一致，故前述 R2 物理验收适用于本次生产实现。review 后只修正测试，本轮未访问设备、未重新部署或执行硬件动作；实机掉电、写入中故障、内核冻结仍未验证。正式服务保持原部署。本次提交限于已跟踪源码、测试、依赖及文档；CodeGraph、本地历史、探针原始日志、认证材料与其他未跟踪文件不纳入。交接记录随代码提交，远端推送结果以 main/origin/main 核对为准。
