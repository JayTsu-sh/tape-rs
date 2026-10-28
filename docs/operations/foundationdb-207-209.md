# 207–209 FoundationDB 元数据集群（2026-10-23）

此集群是 `tape-rs` 的独立 FDB 7.3.78 部署；它尚未成为生产 `ltfsd` 的元数据权威。生产 `ltfsd` 仍从 Raft/SQLite 读写池、磁带、文件与任务，禁止把 FDB 的健康状态当作切换完成。

- 三台主机：`10.131.7.207`、`.208`、`.209`；各一台 `fdbserver`，端口 `45151`，三协调者，`double` 冗余与 `ssd-2` 存储/日志引擎。服务名 `tape-rs-fdb@<本机IP>.service`，已经设为开机启动。
- 数据 `/var/lib/tape-rs-fdb/data`，日志 `/var/log/tape-rs-fdb`，集群连接文件 `/etc/tape-rs-fdb/fdb.cluster`；进程用户 `tape-fdb`。不要把连接文件写入仓库或日志。管理机的同一连接文件在 `/work/tmp/tape-rs-fdb-prod-20261023/fdb.cluster`，权限 `0600`。
- 单进程 FDB 内存预算 `16GiB`，其中缓存 `8GiB`，事务限制 `2GiB`；systemd `MemoryMax=20G`。这只规定上限，不等于已测出亿级文件的吞吐量。节点有约 256GB RAM，现场启动时实际占用约 0.1GB/进程。
- 部署来源是 `deploy/foundationdb/tape-rs-fdb@.service` 和 `install-node.sh`。安装包的 `fdbserver` 与 `libfdb_c.so` SHA-256 固定在脚本中。脚本要求将二进制、库、unit 和相同的集群文件预先放入仅 root 可访问的 staging 目录。配置文件已存在时只接受字节相同者；二进制已存在时只接受字节相同者；unit 可更新，**更新后必须滚动重启并检查集群健康**。
- FDB 本身没有此部署的 TLS/账户隔离；systemd IP 规则只允许本机、三个集群地址与管理机 `10.131.9.10`。扩展客户端节点前应明确增加精确源地址并重新验证隔离。不能把 `45151` 对公共网络开放。

只读健康检查：

```sh
LD_LIBRARY_PATH=/usr/local/libexec/tape-rs-fdb \
  /root/tape-fdb-stage-20261023/fdbcli \
  -C /etc/tape-rs-fdb/fdb.cluster --exec 'status details'
systemctl status tape-rs-fdb@10.131.7.207.service
```

第一条在对应节点执行；在 208/209 上替换第二条的实例地址。`status json` 的 `client.database_status.available`、`healthy` 和 `cluster.data.state.name` 应分别为 `true`、`true`、`healthy`；三协调者可达、三个 zone/机器、`Fault Tolerance 1 machines`。服务故障时先查 `journalctl -u tape-rs-fdb@<IP>.service` 和 `/var/log/tape-rs-fdb/trace*`，不要直接清空数据目录、重新 `configure new` 或把旧实验集群文件复制过来。

本轮验收：三个节点均用本机客户端库返回健康；从管理机写入并读回合成键，停 209 时在其余两节点可继续读写，恢复后读回更新值并清理测试键；三机最终恢复 `healthy`。正式 `ltfsd` 端口 17420/17421 未改，未访问 `/dev/sg*` 或磁带。最初服务因 `RestrictSUIDSGID=true` 拒绝 FDB 的 `processId` 特殊文件模式而循环重启；移除冲突限制后验证稳定。不得重新加上这项限制。

2026-10-23 备份恢复演练：管理机临时启动一个 `backup_agent`，以 `file:///work/tmp/tape-rs-fdb-prod-20261023/backup-acceptance` 为目标执行一次完成即停止的备份。`fdbbackup describe` 对实际生成的 `backup-2026-10-23-21-45-28.201489` 返回 `Restorable: true`。恢复目标是另建的 **本机** `127.0.0.1:45117` 单节点空集群；恢复前验证其连接文件仅含这一地址、测试键不存在。`fdbrestore` 到版本 `946780320` 报 `State: completed`，恢复后的测试键值与源一致。源集群测试键已清除，两个临时 backup agent 和本机 FDB 服务均退出，45117 无监听，源集群仍 `healthy`。备份文件及隔离恢复数据保留在上述 `/work/tmp` 目录供复核；这只验证机制，不构成持续生产备份。

**切换前阻断项：** 尚无持久、自动化且容量足够的生产备份目标与持续恢复演练；尚无现场真实目录迁移、唯一权威切换/回退、生产 `ltfsd` 接线、亿级文件压力与物理带库端到端验收。管理机的集群文件不是备份。FDB 官方要求由 `backup_agent` 执行备份，所有代理须可访问同一备份目标；在确定共享或对象存储目标前，不能把各节点同名的本地路径当作一个可恢复的备份。[官方备份说明](https://apple.github.io/foundationdb/backups.html)

2026-10-24 备份目标现场预检：207/208 均挂载同一 `/gpfs`（约 3.5 TB 可用），209 无该挂载；`mmlsfs gpfsfs` 显示默认数据与元数据副本数均为 1，`mmlsdisk` 显示两个 NSD 均在 failure group `-1`，NSD server 分别为 207/208。因此不能直接把当前 `/gpfs` 当作已具备独立冗余的唯一生产备份目标，也不能在 209 上启动指向该路径的 backup agent。管理机 `/work` 当时约有 434 GB 可用，尚无亿级目录容量测量证明其足够。上述均为只读预检，未创建备份任务或改动 GPFS 配置；下一步需测量实际 FDB 数据增长、明确独立持久目标与保留期，再建立连续备份及定期异机恢复演练。
