# 原生空目录初始化屏障

该只读查询供FDB原生初始化采集来源证据。它不会初始化FDB、格式化磁带、移动介质或授予设备执行权。

分别连接三个配置的物理节点，使用同一个新的非nil challenge；每条回复包含节点自身的 `node_id`、目标 `library_serial`、term、角色、成员物理地址及冻结水位。查询不会转发给Leader，也不使用VIP作为成员地址。

```sh
ltfsctl --endpoint 10.131.7.207:7411 --library 55L3A78011W9LL01 \
  library native-barrier --challenge <UUID>
```

对应HTTP：

```http
GET /libraries/55L3A78011W9LL01/admin/catalog/current/native-barrier?challenge=<UUID>
```

CLI要求一个物理IP:port端点、显式带库且不选pool，直接请求一次；断线或服务端失败时调用方保留challenge，重新检查原冻结点。查询本身没有副作用，三份证明必须重新一起校验，不能拼接不同term或水位。CLI核对回复的库、challenge及来源节点在成员表中的地址，绝不跟随Leader提示。

前提是该库已通过现有 `catalog/current/quiesce` 和 `catalog/current/freeze` 管理操作排空并冻结。节点必须有三成员物理地址配置、有效Leader/Follower状态、无owner、无pending或停机状态；本地SQLite同一个读快照必须赶上相同冻结点，且没有任何池、磁带、文件（含墓碑）、暂存或文件任务历史。目录读取前后再次核对控制状态。冻结查询期间term或水位改变将拒绝本轮证明。

缺失/重复/额外查询参数、nil challenge或请求体返回400；非GET返回405；非空、未冻结或变化中的事实返回409；无法读取目录返回503；未知库404。读取在受并发限制的blocking线程执行，不阻塞Raft或执行设备命令。

三份真实回复交由现有 `verify_native_barriers` 校验，然后才能调用 `initialize_native_library` 创建全新的namespace。旧回复缺少 `library_serial` 必须退役，禁止补字段或放宽校验。完整初始化CLI/HTTP管理写入入口尚未实现；本片只完成证明采集。
