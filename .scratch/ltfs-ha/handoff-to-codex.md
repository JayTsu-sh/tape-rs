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
