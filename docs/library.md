# 多卷逻辑库（Rust 库与 FUSE）

`library::Library` 提供显式 assignment、blacklist、pool/volume 路径解析、库存状态和按需装带。`LibraryBackend` 将同一实现接入 `TapeFs`；`tape-fuse --endpoint` 原有集群模式继续可用。

新模式是独占设备的单进程逻辑库，资源 SQLite 与现有 ltfsd 的 Raft/catalog 分开。它不是第二个 HA 控制面；不得与 LE、ltfsd 或其他实例同时控制同一设备。资源文件有进程锁，CLI 资源修改在 FUSE 卸载后进行。Rust 调用方可以持有 `LibraryBackend` 的共享锁，在同一实例内查询/管理资源。没有新增生产依赖或改变已有持久化格式。

## 路径与状态

FUSE 路径为 `/池名/条码/卷内路径`。根目录列池，池目录列显式分配卷，不会因列根目录而自动装所有磁带。首次访问卷内路径才触发调度。`Library::resolve`、`pools`、`volumes`、`status`、`read_to`、`write_from` 是对应 Rust 接口；只读检查使用 `inspect_volume`（不可变卷引用），目录/xattr/元数据更新使用 `with_volume`，完成提交后才返回成功。

`tape-library` 提供 `status`、`assign POOL BARCODE`、`unassign BARCODE`、`blacklist BARCODE REASON`、`clear-blacklist BARCODE`、`read PATH OUTPUT`、`list PATH`。资源模型保留 pool、home storage 地址、blacklist 原因、调度阶段和失败前阶段/原始错误。`NEED_ASSIGN` 卷不允许访问，`BLACKLISTED` 卷不允许装载，无法识别的卷记录 `NON_SUPPORTED_TAPE`。它提供本项目管理面，不实现 leadm 的线协议。

FUSE 持有资源锁时，另一个 `tape-library status` 不打开设备，也不争抢控制权；它在一个只读事务内把最近一次完整库存快照与实时 assignment/blacklist/阶段/失败合并，输出 `source=cached_inventory` 和 `observed_at`。资源锁空闲时状态来自当前设备查询，输出 `source=device_query`。缓存状态必须通过已绑定的 changer 序列号核验，不能用于机械决策。

配置的 changer 由 VPD 序列号核验。驱动器参数用 `SERIAL=/dev/sgN`；DTE 从 element map 和库存 DVCID 推导，再与 drive VPD 核对，不能按旧 sg 编号或固定 element 地址猜测。资源库绑定 changer 序列号。

## 调度与持久化

按顺序执行：核验 assignment/home slot → 确认 changer 与 drive 的 PR → 必要时将本库已分配介质归槽 → 重新查库存 → MOVE → 核对目标条码 → LOAD → 保留长超时的 TUR ready polling → LTFS 恢复 → 操作。优先沿用已有目标卷，其次选空驱动器，最后才驱逐本库分配卷；未知/未分配介质不搬动。

changer 不支持 PR 时安全拒绝自动调度，不把文件锁冒充跨主机隔离。模拟器的默认 changer 不支持 PR，因此新测试使用独立模拟 PR transport；这不能证明 TS4300 的 changer PR 行为。

连续同卷访问沿用已验证的 `VolumeSession`；每次仍检查 PR、TUR 和设备 epoch，换带/UA/复位后失效。失去资格后本实例停止，不能重新 fence。写操作完成完整双分区 commit 后才返回成功，直接 FUSE 的 close/upload 因而比原 HTTP 暂存模式更强。正常 FUSE 卸载和 CLI 结束会归槽已访问卷并释放本轮 PR；Rust 调用方必须检查 `close()` 结果。Drop 不执行机械操作，异常终止可能留下装载介质/PR，需要核实现场后重新接续。

IBM append-only 扩展模式下，普通 DP 追加不受影响，但 IP checkpoint 的 opening filemark 可能返回 DATA PROTECT `5A/02`。卷提交在已持有且复核 PR 后读取精确 partition/block，以 `ALLOW OVERWRITE` action 1 仅授权当前位置一次写入；不发送 format 授权，也不关闭 append-only 模式。READ POSITION 的 UA、位置不符、LOLU/PERR、未知模式页和截断响应都安全拒绝。设备不支持该 mode subpage（`24/00`）时沿用标准 LTFS 路径。

跨卷 rename 返回 `EXDEV`。符号链接目标保持 LTFS 索引原文，FUSE 采用 POSIX 路径语义；直接 Rust 读取沿用卷库的单层、卷内相对链接限制。

## 使用

先核实真实节点、介质与设备 holder。以下为参数模板，设备节点与序列号必须替换为本次库存核验值；命令的访问可能执行 PR、MOVE、LOAD 和 UNLOAD，但不会自动格式化。

```sh
cargo build --release --features fuse
# 每条命令都携带同一份显式设备参数，执行轮次由运维单调分配。
ARGS=(--library-changer /dev/sgN --library-changer-serial CHANGER_SERIAL
      --library-drive DRIVE_SERIAL=/dev/sgM --library-state /isolated/library.sqlite
      --library-node 1 --library-round NEW_ROUND)
tape-library "${ARGS[@]}" status
tape-library "${ARGS[@]}" assign test TEST_BARCODE
tape-fuse "${ARGS[@]}" --cache-dir /isolated/cache /isolated/mount
```

Rust 接口使用 `Library::open(changer, Vec<LibraryDrive<D>>, state_path, key)`，transport 可以是 `ScsiDevice` 或测试 transport；没有 FUSE feature 的编译也支持逻辑库。读取示例为 `library.read_to("/test/TEST_BARCODE/file", &mut output)`；写入为 `library.write_from(path, &mut input, create_only)`，结束时检查 `library.close()`。`with_volume` 更新回调失败后不重放写入，失败诊断保留在资源库。

`examples/library_acceptance.rs` 是独立 Rust 库实机验收入口。它对显式给定路径做两轮流式 SHA256 校验，并保存初始/最终库存、耗时、结果及清理错误；不格式化、不写文件、不自动分配介质。两卷路径交替输入、只配置一台 drive 才能验证真实换带，重复读同卷不能替代此项。

`--checkpoint` 只在所有预期文件两轮校验通过后对涉及的卷提交一次 checkpoint，用于明确修复已证明的 IP debt；它不重放文件追加。没有已知 debt 时不要使用。

```sh
cargo run --release --example library_acceptance -- "${ARGS[@]}" \
  --expect /test/TEST_BARCODE/file=EXPECTED_SHA256 \
  --expect /test/SECOND_BARCODE/file=EXPECTED_SHA256 \
  --report /isolated/acceptance.json
```

## 大索引恢复

`LtfsIndex::parse_reader(BufRead)` 共享原有 XML 状态机。恢复只缓存一个磁带记录和最多 512 字节前缀，不再缓存整份原始 XML；前缀与 XML 事件可以跨记录。设备错误保留原类型，解析失败后仍建立构造边界，截断 Full/增量均拒绝发布。

`RecoveryBudget::max_index_bytes` 保留为整个构造的 I/O 上限，默认由 64 MiB 调整为 8 GiB，显式较小预算仍会返回受限；反向步进与历史深度预算不变。文件树、单个 XML 事件及增量重放仍使用内存，内存随文件/属性数量增长；不声称百万文件挂载具有固定内存占用。65 MiB 构造回归与分段解析已覆盖，但历史百万文件卷已重格式化，实机验收必须重新建立大索引夹具。

软件基线中的显式百万文件流式解析测试会生成并解析一百万个文件元素，核对末尾节点、UID、名称和时间；它不访问硬件，也不等同于百万文件实机性能。真实介质验收应分别记录索引字节数、文件/属性数量、旧预算失败和新实现读回结果。
