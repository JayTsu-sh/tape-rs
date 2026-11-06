# ltfsd 完整参数参考

集群服务进程。用法说明见[命令行参考手册](../cli-reference.md)。

本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。可选命令随构建特性变化，下面的帮助为本次构建内容。

构建产物 SHA256：`0286c0055eaa8874de9fc12b46f6dca7a5a78b4d41c1bbed389e85e0e6747b1f`。共 1 个帮助入口。

## 目录

- [ltfsd](#ltfsd)

## ltfsd

```text
磁带库高可用守护进程（骨架）：Raft 选主 + 设备层隔离 + 自动接管

Usage: ltfsd [OPTIONS] --id <ID> --listen <LISTEN> --data-dir <DATA_DIR>

Options:
      --id <ID>
          本节点编号（1..=255，同时写进预留键）
      --listen <LISTEN>
          节点间通信的监听地址
      --peer <ID=ADDR>
          全部成员，形如 1=10.10.2.71:7400，包含本节点，重复给出
      --data-dir <DATA_DIR>
          控制存储与状态文件所在目录
      --fdb-cluster-file <PATH>
          FDB 集群连接文件；用于权威预检和显式 FDB 运行模式
      --fdb-metadata-readonly
          只读 FDB 目录元数据入口；不启动旧 Raft/SQLite 文件服务或设备执行器
      --fdb-owner-readonly
          FDB Raft/PR owner 只读文件服务（会按需装卸磁带）及池运维；不启用旧目录或文件写入
      --fdb-owner-writable
          FDB 可写 owner；仅向显式已归属卷提交上传，并在接任时恢复未确认批次
      --fdb-s3
          在独立 S3 端口提供 FDB 对象访问；写操作要求可写 owner，可共用受管理 VIP
      --fdb-read-index-mib <FDB_READ_INDEX_MIB>
          FDB owner 每库索引核验临时库上限（MiB；默认 131072）
      --fdb-read-cache-mib <FDB_READ_CACHE_MIB>
          FDB owner 每库 extent 缓存总预算（MiB；默认 131072）
      --fdb-read-spool-mib <FDB_READ_SPOOL_MIB>
          FDB owner 单文件上传/下载暂存上限（MiB；默认 131072）
      --fdb-read-headroom-mib <FDB_READ_HEADROOM_MIB>
          FDB owner 本地读取预留文件系统余量（MiB；默认 1024，可为 0）
      --libraries-config <LIBRARIES_CONFIG>
          多库 JSON 配置（default_library、libraries）；与单库设备参数互斥
      --expand-libraries-offline
          全集群停机后逐节点执行，仅新增带库或驱动；写入显式扩容记录并退出
      --replace-drive-offline <OLD=NEW>
          全集群停机后逐节点执行，格式 OLD=NEW；仅更换同一库的一台驱动
      --remove-drive-offline <SERIAL>
          全集群停机后逐节点执行；从指定库移除一台非末台驱动（真实 VPD 序列号）
      --changer-serial <CHANGER_SERIAL>
          受管逻辑库换带器的序列号（VPD 0x80）
      --changer-target-port <CHANGER_TARGET_PORT>
          换带器的 FC 目标端口 WWPN；固定各节点使用同一设备预留域
      --drive-serial <DRIVE_SERIALS>
          受管驱动器的序列号，重复给出
      --interval-ms <INTERVAL_MS>
          服务期间的工作周期（毫秒）：持有者自检，以及可选的演示写入 [default: 3000]
      --demo-write
          周期性向卷里追加一个小文件，用于故障演练
      --block-size <BLOCK_SIZE>
          自动格式化空白带时用的块大小（字节） [default: 524288]
      --read-idle-ms <READ_IDLE_MS>
          为读请求装载的带空闲这么久后卸回槽位（毫秒） [default: 300000]
      --salvage
          自动收尾时打捞未索引数据（默认放弃）
      --batch-mib <BATCH_MIB>
          合批：队列累计字节数达到即提交（MiB） [default: 8192]
      --batch-files <BATCH_FILES>
          合批：队列累计文件数达到即提交 [default: 20000]
      --batch-idle-ms <BATCH_IDLE_MS>
          合批：这么久没有新的上传完成即提交（毫秒） [default: 2000]
      --batch-max-wait-ms <BATCH_MAX_WAIT_MS>
          合批：队列里最早的文件最多等这么久（毫秒）。带 wait 的上传的延迟上界 [default: 60000]
      --client-listen <CLIENT_LISTEN>
          客户端接口（HTTP）的监听地址。不给则不开
      --s3-listen <S3_LISTEN>
          匿名 S3 HTTP 监听地址（path-style）；须启用 FDB owner 与 --fdb-s3
      --s3-port <S3_PORT>
          各节点 S3 端口，主机取自 --peer；须与各节点 --s3-listen 的端口一致 [default: 7402]
      --http-vip <HTTP_VIP>
          同子网 HTTP IPv4 VIP：单库跟随设备 owner，多库跟随统一入口；不给则不管理地址
      --http-vip-interface <HTTP_VIP_INTERFACE>
          HTTP VIP 所在以太网接口，各节点可不同（最多 13 字节）
      --client-port <CLIENT_PORT>
          各节点客户端接口的端口，用来在 503 里给出 Leader 地址（主机取自 --peer） [default: 7401]
      --tick-ms <TICK_MS>
          Raft 逻辑时钟周期（毫秒） [default: 100]
      --execution-permit-ms <EXECUTION_PERMIT_MS>
          多数派执行许可时限（毫秒），不改变磁带命令超时 [default: 6000]
      --snapshot-entries <SNAPSHOT_ENTRIES>
          日志压缩：距上次快照累积这么多条目即做一次快照 [default: 2000]
      --snapshot-mib <SNAPSHOT_MIB>
          日志压缩：距上次快照累积这么多字节即做一次快照（MiB） [default: 256]
      --shutdown-grace-s <SHUTDOWN_GRACE_S>
          收到 SIGTERM/SIGINT 后最多等多久让执行线程落带、退带、释放预留（秒） [default: 300]
  -h, --help
          Print help
```
