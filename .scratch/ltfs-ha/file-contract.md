# 文件接口契约：原生接口与 FUSE 的共同语义

关联：[票据 05](issues/05-file-contract.md) · [提交契约 04](issues/04-durability.md#answer) · [池化切片](pooling-slice-plan.md) · [任务与调度](ee-task-scheduling.md) · [资源与状态模型](ee-resource-model.md)

状态：**已裁定（2026-09-23，票据 05 resolved）**。2026-09-17 的首版候选写在 ltfsd 实现之前，按"先整文件接收、再批量落带"的写入模型设计了流式写、partial 状态和默认 create_only；这些与实现不符的部分已按用户裁定改写，旧版见 git 历史（`a1d163c` 及更早）。本文只定语义、线上表示和错误映射，数值（合批时长、重试窗口、暂存区额度）沿用配置项，不在此固定。

## 写入模型（已实现，契约以此为准）

- 一次上传 = 一个完整对象。服务端先把整个内容接收到执行者节点的暂存区，校验长度后标记完成，再由合批提交整批写带（`files.rs`：准入 → 接收 → 完成 → 冻结 → 落带 → 发布）。上传前必须知道长度（HTTP `Content-Length`）。
- 索引只引用完整文件，所以**单个文件不会处于"部分已提交"**。04 裁定的"保留已可靠提交的部分文件"在这个模型下只剩一种来源：接管收尾时 `TailPolicy::Salvage` 把未索引尾部登记成 `_ltfs_lostandfound/` 下的只读文件。文件状态里不再有 `partial`。
- 暂存不复制。执行者更换时：仅暂存的任务失败（需重传），已进入提交批次的任务结果未定（查询该路径判定）。
- 不提供按任务 ID 的持久查询、服务端幂等键或去重：`/tasks/<id>` 只在受理它的节点、同一进程内有效。结果未定由客户端按"路径 + 长度 + sha256"判定（见"结果未定与安全重试"）。

## 文件状态

| 状态 | 含义 | 读 | 线上表示 |
| --- | --- | --- | --- |
| committed | 当前版本已随卷提交落带并发布 | 可读 | `stat` 200，`state: committed` |
| uploading | 已准入，内容还在接收 | 读已提交旧版本（如有），否则 `not_committed` | `stat` 200，`state: uploading` |
| staged | 已完整暂存，等待合批落带 | 同上 | `stat` 200，`state: staged` |
| 不存在 | 视图可信且没有当前版本，也没有在途任务；包括被墓碑覆盖 | — | `stat` 404 |
| 恢复中 | 本节点不在服务（换届、接管、恢复） | — | 503；客户端库等待并跟随 Leader |

- `stat` 对 uploading/staged 在顶层给出在途状态与已接收长度，已提交的当前版本放在字段 `current`（没有则省略），调用方能看到"新版本在途、旧版本仍是当前"。已提交且无在途时字段在顶层，与旧格式兼容。
- `list` 默认只列已提交；`pending=1` 时附上在途路径及其状态。
- 恢复中绝不回答 404：视图不可信时只能答 503。

## 创建与替换

- `PUT /files/<path>` 默认**替换**：新版本在它所在的卷提交发布时原子成为当前版本；之前旧版本一直可读。替换不回收旧版本占用的磁带空间，由回收（reclaim）处理。
- **只创建**：请求带 `If-None-Match: *`。路径有已提交的当前版本 → 412 `exists`；路径有在途任务 → 409 `path_busy`（与普通上传相同）。客户端库 `Client::put_new`，FUSE 的 `O_CREAT|O_EXCL`。
  - 判定依据是准入时的已提交视图（本轮刚落带的 + 目录库）。已知边界：旧执行者已落带、但 `TapeCommitted` 尚未进 Raft 就失效，而新执行者又选了别的写入带时，这个文件要等那盘带下次被打开并对账后才进目录；这段时间里只创建与 `stat` 都看不到它。`ensure_write_tape` 优先已装载的带，通常新执行者会打开同一盘带。
- 同一路径同一时刻只能有一个在途任务（`VolumeState` 的路径预留）。
- 全集群的版本顺序由 `tapers.version = 轮次.序号` 给出（见 CLAUDE.md "File versions"），替换与删除都按它比较。

## 删除（墓碑）

EE 的做法（[资源模型](ee-resource-model.md)、[39](issues/39-ee-fault-recovery-evidence.md)）：删除 GPFS 里的文件并不动磁带，要等 `reconcile` 装上那盘带、改写它的索引才从带上去掉，reconcile 与写入互斥；没对账过的带导出再导入时，已删除的文件会重新出现。tape-rs 没有 GPFS，磁带是权威、目录只是缓存，而且被删文件可能在任何一盘带上，所以删除写**墓碑**而不改旧带：

- `DELETE /files/<path>`：路径没有已提交的当前版本 → 404；有在途任务 → 409 `path_busy`；否则受理，202 = 已暂存，`?wait=1` 等到落带后 200。
- 墓碑是当前写入带上的一个零长度文件 `.tapers/deleted/<sha256(路径)>`，带 `tapers.version` 与 `tapers.deletedPath = <路径>`（`ltfs::volume::tombstone_path`；用哈希做文件名，是因为被删路径的层次不能在墓碑目录里重建，`/a` 与 `/a/b` 会冲突）。它和普通上传走同一条合批落带路径，同一批次里的墓碑与上传一起发布。IBM LTFS 能读这盘带，只是把墓碑当普通空文件。
- 目录按版本取大者：墓碑版本更高则该路径不存在；之后重新上传得到更大的版本，路径复活。
- 回收时墓碑按活对象搬到目标带（很小，不做逐条判断是否仍有旧副本）。墓碑的最终清理留到后续（需要"池里不再有更旧副本"的证明）。
- 与 EE 相同的局限：单独拿出一盘旧带（不带墓碑所在的带）重建目录时，被删文件会重新出现。

## 重命名

不支持。FUSE 的 `rename` 返回 `EXDEV`；原生接口没有 rename。

选 `EXDEV` 而不是 `EOPNOTSUPP` 的原因：`mv` 遇到 `EXDEV` 会自动退回"复制 + 删除"，这正是裁定要求应用自己做的事，于是 `mv` 能直接用。代价是 rsync 默认的"写临时文件再改名"会失败（rsync 不做这个回退），需用 `--inplace` 写新文件。用户可推翻为 `EOPNOTSUPP`。

## 读取

- 只服务已提交的当前版本。路径只有在途任务、没有已提交版本时答 409 `not_committed`。
- `GET /files/<path>` 支持单段 `Range: bytes=a-b`（206）。性能按顺序读优化，随机读可能触发重新定位。
- 读可以服务池里任何一盘带（`read_any`）；单驱动器正在写时答 503 `drive_busy`，客户端稍后重试。

## 目录

隐式目录：有文件（已提交或在途）就有目录。服务端不存目录记录。

- `GET /list?dir=<d>` 返回该目录的直接子项：文件（含长度、状态）和子目录名。不带 `dir` 时保持现有的全量列举（ltfsctl 用）。
- FUSE 的 `mkdir` 只在该挂载的内存里创建空目录，直到有文件写进去；挂载重启或别的挂载看不到它。`rmdir` 只对没有子项（已提交、在途、内存目录都没有）的目录成功。
- `.tapers/` 与 `_ltfs_lostandfound/` 不出现在 FUSE 的根目录列举里；原生 `list` 照常可见。

## 完成与归档确认

| 入口 | 已暂存 | 已归档（卷提交） |
| --- | --- | --- |
| HTTP | `PUT` 202 | `PUT ?wait=1` 201，或 `GET /tasks/<id>?wait=1` |
| 客户端库 | —（库不暴露"仅暂存"） | `Client::put` / `put_new` 返回即已落带（现有行为） |
| FUSE | `close` 返回 0 | `fsync` 返回 0 |

- FUSE `close`（`flush`）把本地缓存的内容上传到暂存区后返回；被拒绝时 `close` 返回对应 errno。**`close` 成功不代表已归档**：换届时暂存会作废，文件随之消失（或回到旧版本）。需要确认的应用必须在 `close` 前 `fsync`。
- FUSE `fsync` 在写句柄上：上传截至此刻的内容并等到落带（最长约为合批的 `max_wait`，默认 60 s）；返回 0 即该内容已归档。之后继续写入的内容在下次 `close`/`fsync` 时作为新版本整体上传（替换）。`fsync` 在目录上：等本挂载发出的删除都落带。
- 结果未定时客户端库先按路径判定（下节）；仍无法判定时 `fsync` 返回 `EIO`，应用用 `getxattr user.tape.state` 核对。

## 结果未定与安全重试

应用（或 FUSE 适配器）持有源数据。上传途中连接断开、服务端答 `indeterminate`，或换届后查不到任务时：

1. 在当前 Leader 上 `stat(path)`：committed 且长度与 sha256 与本次上传一致 → 已完成，不重传（`Client::put` 已实现，`resolved_by_query`）。
2. committed 但内容不一致、或 404 → 按原模式重传（替换，或只创建）。
3. staged/uploading → 等它结束再判定；503 → 等待。
4. 可能已到达服务端的请求不会被库悄悄发到另一个节点重放。

## 接管与句柄

- 换届或执行者重启：服务端所有在途任务终止（仅暂存 → failed，已入批 → indeterminate）；读在新执行者恢复后继续。
- FUSE 是独立进程，经客户端库连接集群，可装在任何主机上；它的打开句柄不绑定某个 ltfsd 节点。换届期间的调用阻塞到客户端库的 `retry_for`，超时返回 `EBUSY`。
- FUSE 进程自身重启：打开的句柄全部失效（`ESTALE`），本地缓存里未 `close` 的内容丢弃。
- 不承诺内核 fd 迁移。

## FUSE 操作子集

| 操作 | 行为 |
| --- | --- |
| `lookup`/`getattr` | 已提交：长度、索引的修改时间；在途且无已提交版本：长度为暂存长度。文件 0644、目录 0755，属主为挂载用户 |
| `readdir` | `list?dir=&pending=1` + 内存目录 |
| `open` 只读 | 已提交可读；否则 `EBUSY` |
| `create` / `open(O_WRONLY\|O_TRUNC)` | 本地缓存文件；`O_EXCL` → 只创建 |
| `open` 已存在文件写但无 `O_TRUNC`，或 `O_RDWR`、`O_APPEND` | `EOPNOTSUPP` |
| `write` | offset 必须等于当前长度，否则 `EINVAL` |
| `flush`（close） | 上传到暂存区（见上） |
| `fsync` | 上传并等落带（见上） |
| `unlink` | `DELETE`；在途 `EBUSY`，不存在 `ENOENT` |
| `rename` | `EXDEV` |
| `mkdir`/`rmdir` | 见"目录" |
| `truncate` | 只允许截到 0 且文件正以写方式打开；其他 `EOPNOTSUPP` |
| `chmod`/`chown`/`utimens`/`setxattr` | `EOPNOTSUPP`（`cp -p` 会告警但继续） |
| `getxattr` | 只读：`user.tape.state`、`user.tape.version`、`user.tape.barcode`、`user.tape.sha256`，以及索引里文件自带的 xattr |
| `link`/`symlink`/可写 `mmap` | `EOPNOTSUPP`（只读 `mmap` 由内核经 `read` 支持） |

## 错误映射

| HTTP | 原因 | 客户端库 | FUSE errno |
| --- | --- | --- | --- |
| 404 | 视图可信且不存在 | `NotFound` | `ENOENT` |
| 409 `path_busy` | 路径有在途任务 | `Rejected` | `EBUSY` |
| 409 `not_committed` | 读在途文件 | `Rejected` | `EBUSY` |
| 412 `exists` | 只创建冲突 | `Exists` | `EEXIST` |
| 400 `bad_path` | 路径非法 | `Rejected` | `EINVAL` |
| 503 `not_serving`/`switching_tape`/`drive_busy` | 恢复中、换带、驱动器忙 | 库内等待重试；`retry_for` 到期 `NoLeader` | `EBUSY` |
| 503 `read_only` | 卷受限只读 | `Rejected` | `EROFS` |
| 507 `too_large` | 比一盘带还大 | `Rejected` | `EFBIG` |
| 507 `no_tape` / `insufficient_capacity` | 池里没有可写带 | `Rejected` | `ENOSPC` |
| 500 `indeterminate` | 结果未定且按路径无法判定 | `Indeterminate` | `EIO` |
| 500 `failed`/`spool_io` | 明确失败 | `Rejected` | `EIO` |

恢复中与未定状态不得映射为 `ENOENT`。

## 实施状态

- **F1（2026-09-23）已实现**：`FileService::begin_with`（只创建）、`stat_full`、`list_dir`；HTTP `If-None-Match: *` → 412、`/stat` 在途状态、`/list?dir=&pending=1`、单段 `Range`（206/416）、只在途路径读取 409 `not_committed`、不存在 404；客户端库 `put_new`、`stat_path`、`get_range`、`list_dir`，结果未定时遇到在途状态先等它结束再判定；`ltfsctl put --new`、`stat` 显示状态、`list --dir [--pending]`。FC01/02/09/10/11 由 `ltfsd_cluster.rs::file_contract_create_only_in_flight_state_range_and_directories` 覆盖。已知的简化：`Range` 仍是整文件读出后切片（`read_file` 256 MiB 上限不变），`list_dir` 取全量再按前缀过滤；两处都标了 TODO，F3 FUSE 做大文件读与大目录时要换成按块读和目录库前缀查询。
- F2 删除：进行中（另一会话）。F3 FUSE：未开始。

## 验证 FC01—FC12

| 编号 | 内容 | 层 |
| --- | --- | --- |
| FC01 | 只创建遇到已提交路径 412，不产生新版本；遇到在途路径 409 | 模拟器集群 |
| FC02 | 替换在途时旧版本一直可读，落带后切到新版本 | 模拟器集群 |
| FC03 | FUSE 非顺序写 `EINVAL`，已存在文件无 `O_TRUNC` 写 `EOPNOTSUPP` | FUSE 适配器（模拟后端） |
| FC04 | `close` 后 `stat` 为 staged、读 `EBUSY`；`fsync` 返回 0 后为 committed 且可读 | 两层 |
| FC05 | `fsync` 期间换届：返回 0（经路径判定）或 `EIO`；之后 `stat` 为 committed 或不存在，不会"磁带有数据但被隐藏" | 模拟器集群 + Holo |
| FC06 | 删除后 `stat` 404；装入被删文件所在的旧带并对账后仍 404；重新上传后路径复活 | 模拟器集群 + Holo |
| FC07 | 删除在途路径 409；删除不存在路径 404 | 模拟器集群 |
| FC08 | 回收一盘带着墓碑的带后，被删路径仍不存在 | 模拟器集群 |
| FC09 | 恢复中 `stat` 503，不回答 404 | 模拟器集群 |
| FC10 | `Range` 读返回正确片段 | 模拟器集群 + Holo |
| FC11 | `list?dir=` 只返回直接子项；FUSE `mkdir`/`rmdir` 规则 | 两层 |
| FC12 | FUSE `mv` 经 `EXDEV` 退回复制 + 删除，结果路径内容一致、源路径不存在 | FUSE + Holo |

## 与 EE 的对照（切片实施时先在实验室观察）

- 删除：EE 删除 GPFS 文件后 `eeadm file state` 与磁带索引的变化、`reconcile` 何时去掉带上副本、未对账带导入的表现。
- 覆盖写：EE 覆盖一个 migrated 文件后旧副本在带上的状态（预期为"已失效、待 reclaim"），对照 tape-rs 的版本 + 回收。
- 只读访问 migrated 文件时的召回与 `EBUSY`/阻塞行为，对照 FUSE 读在途文件的 `EBUSY`。
