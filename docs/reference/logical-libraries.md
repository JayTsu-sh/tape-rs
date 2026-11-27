# 逻辑带库资源登记

`ltfsctl logical-library` 在指定物理带库中登记、列出和查看逻辑带库。资源清单使用驱动器序列号、存储元素地址和 I/E 元素地址。列表的 `unallocated_resources` 给出尚未登记归属的资源，`inventory.observation` 给出观测时间和介质位置。`inventory.status` 为 `current` 时，本轮完整库存可用于分配。缺失、未完整或过期时，先通过物理库 `library rescan` 刷新库存并确认任务成功。

```sh
ltfsctl -e HOST:7401 -l LIBRARY logical-library list --namespace NAMESPACE_UUID
ltfsctl -e HOST:7401 -l LIBRARY logical-library create archive-a \
  --namespace NAMESPACE_UUID --id REQUEST_UUID \
  --drive DRIVE_SERIAL --storage-slot 1001 --ie-slot 101
ltfsctl -e HOST:7401 -l LIBRARY logical-library show archive-a --namespace NAMESPACE_UUID
```

物理库必须先完成[离线启用](#首次启用已有物理带库)，才能在线新增逻辑库。创建前生成并保存 UUID 与选择的全部资源。新增或移除资源需要当前完整库存证明资源为空，且没有未结清动作或 home 引用。可以重复使用 `--drive`、`--storage-slot` 和 `--ie-slot` 指定同类资源。创建响应丢失时，按原 UUID 查询，或用相同 UUID 和完整参数重试。重复分配和篡改原请求都会返回冲突。创建只登记共享目录中的资源归属。

HTTP 使用 `/libraries/{serial}/admin/logical-libraries` 的 `GET` 和 `POST`。详情使用 `GET /libraries/{serial}/admin/logical-libraries/{selector}`，选择器为名称或 UUID。所有请求携带 `x-tapers-catalog-namespace`。`POST` 的 JSON 包含 `version`、`id`、`name` 和 `resources`，资源对象包含 `drives`、`storage_slots` 和 `ie_slots` 三个数组。首次成功返回 201，重试成功返回 200。Rust 客户端提供 `FdbClient::create_logical_library`、`list_logical_libraries` 和 `logical_library`。

## 修改、删除及查询回执

用 `logical-library show` 查询当前修订号。`set` 接受完整的目标名称和资源清单。`delete` 要求逻辑库没有 pool，并在完整库存中证实待释放资源为空且没有未结清的动作或 home 引用。保存原操作 UUID 和参数，结果未定时查询原回执。

```sh
ltfsctl -e HOST:7401 -l LIBRARY logical-library set LOGICAL_UUID \
  --namespace NAMESPACE_UUID --revision 1 --operation OPERATION_UUID \
  --name archive-b --drive DRIVE_SERIAL --storage-slot 1001 --ie-slot 101
ltfsctl -e HOST:7401 -l LIBRARY logical-library delete LOGICAL_UUID \
  --namespace NAMESPACE_UUID --revision 2 --operation DELETE_UUID
ltfsctl -e HOST:7401 -l LIBRARY logical-library change-status LOGICAL_UUID \
  --namespace NAMESPACE_UUID --operation DELETE_UUID
```

HTTP `PATCH` 和 `DELETE` 使用逻辑库 UUID。请求包含 `version`、`operation`、`revision` 和 `replacement`，其中 `replacement` 是 `name` 和 `resources` 对象，删除时为 `null`。回执地址为 `GET /libraries/{serial}/admin/logical-libraries/{id}/changes/{operation}`，删除后仍可按原 UUID 查询。

## 在逻辑带库下管理 pool

`ltfsctl pool create/list/show/set/delete` 使用 `--logical-library LOGICAL_UUID`。HTTP 的 `/libraries/{serial}/admin/pools` 及池详情使用 `x-tapers-logical-library` 请求头，修改和删除还在原 `change.logical_library` 中固定同一 UUID。池分页游标只对原物理库、namespace 和逻辑带库有效。创建会一起提交 pool、逻辑带库归属和原创建回执。

```sh
ltfsctl -e HOST:7401 -l LIBRARY pool create --logical-library LOGICAL_UUID archive
ltfsctl -e HOST:7401 -l LIBRARY pool list --logical-library LOGICAL_UUID --json
```

## 搬带、分池和介质维护

在逻辑带库内执行 `tape move` 或 `tape assign` 时使用 `--logical-library LOGICAL_UUID`。分池须明确指定所属 pool。移动已分池介质时同样指定该 pool。未分池介质可直接指定逻辑带库和条码。

```sh
ltfsctl -e HOST:7401 -l LIBRARY tape move --logical-library LOGICAL_UUID -L ieslot BARCODE
ltfsctl -e HOST:7401 -l LIBRARY tape assign -p archive --logical-library LOGICAL_UUID --yes BARCODE
```

HTTP 和 Rust SDK 将 UUID 固定在原管理请求的 `change.logical_library`。设备命令前核对源位置、目标和 home 的归属。没有归属当前逻辑库且可访问的空 I/E 槽或可用驱动器时，任务记录拒绝原因。结果未知时保留原检查点与请求文件。

`tape import`、`validate`、`online`、`offline`、`set`、`unassign` 和 `reclaim` 也使用 `--logical-library LOGICAL_UUID` 及所属 pool。索引导入使用原归位槽。回收在源带和目标带就位前固定两台所属驱动器和各自的归位槽。驱动器不足时，原回收任务保留等待状态和原因。

## 文件读写

原生文件入口使用物理带库、逻辑带库 UUID 和 pool UUID。HTTP 请求携带 `x-tapers-logical-library`。Rust SDK 使用 `FdbClient::with_logical_library` 设置范围。原生上传日志会保存逻辑带库 UUID，续传与回执查询使用原 UUID。

```sh
ltfsctl -e HOST:7401 -l LIBRARY -p POOL_UUID fdb --logical-library LOGICAL_UUID stat /archive.tar
tape-fuse -e HOST:7401 --library LIBRARY --pool POOL_UUID \
  --fdb-namespace NAMESPACE_UUID --logical-library LOGICAL_UUID --cache-dir CACHE MOUNTPOINT
```

S3 通过 `ListBuckets` 查找逻辑带库下的 pool 名称。应用也可用 Rust 函数 `s3::logical_bucket_name` 根据物理带库序列号、逻辑带库 UUID 和 pool UUID 计算 bucket 名称。旧的物理库 bucket 名称无法访问已绑定逻辑带库的 pool。

## 整库交还和重新加载

重新加载取得设备后，执行器先采集完整库存，再把原加载操作推进至 `online`。库存缺失或设备身份不符时，保留原加载操作及原因。介质跨逻辑库或 home 不符时，相关逻辑库的读写与介质操作暂停。通过 `logical-library show` 或 `list` 的 `reconciliation` 字段查看相关条码、原因和原加载 UUID。原 pool 归属及整库交还回执继续可查。库存恢复一致并经完整观测核对后，相关业务可继续。[整库交还手册](library-handoff.md)

## 首次启用已有物理带库

未启用逻辑库时，普通 pool、文件和介质业务使用物理库范围。首次切换前，在原版本结清已有的业务请求及未知设备动作，然后进入维护窗口。启用后的 pool、文件及介质业务都使用显式逻辑库范围。

在维护窗口停发新的业务，确认全部任务结清。完成整库交还并保存回执 UUID。停止所有能够访问该物理库的执行器，确认此前发出的设备命令已经结清。候选版本要在所有节点准备完毕。离线命令要求原设备组的 PR 注册与持有者均已清空，并加载当前完整库存。

在任一停机成员上，用其原 `--id`、`--listen`、`--peer`、`--data-dir`、`--libraries-config` 和 `--fdb-cluster-file` 参数运行以下命令，保留生成的启用 UUID 和预览的 `plan_sha256`。

```sh
ltfsd <原节点参数> --activate-logical-library-offline LIBRARY \
  --logical-library-namespace NAMESPACE_UUID \
  --logical-library-release RELEASE_UUID --logical-library-operation ACTIVATION_UUID
```

核对预览中的 pool 与资源后，追加 `--commit-logical-library-plan PLAN_SHA256`。命令重新采集库存并核对摘要，然后用同一 FDB 事务发布 `default`、资源和既有 pool 的归属、启用状态及回执。`default` 使用启用 UUID。原有 pool、卷和文件保持原身份，物理资源先归入 `default`。之后可释放当前为空闲且无 home 引用的资源，并划分给新的逻辑库。

执行节点仍在线、设备访问无法核实或原任务未结清时，预览仍列出 pool、阻塞原因和可用的历史库存。`inventory_provenance` 标明库存来源，此时 `plan_sha256` 为空。排除阻塞后按原 UUID 重新预览，提交时重新核验设备、库存、任务和计划。

首次配置或启用时保存槽位起始地址、数量及驱动器地址和序列号的对应关系。重新取得设备后，受布局变化影响的逻辑库暂停业务，即使旧地址仍在当前范围内。恢复原布局并核对完整库存后恢复业务。已经启用、但尚未保存原始布局的物理库，在升级前先重扫库存并整库交还。升级后重新取得设备时，系统用原启用回执核验交还前的库存，再以该布局核对当前设备。

同一 UUID 和计划可查询原结果。HTTP 使用 `GET /libraries/{serial}/admin/logical-libraries/activation/{operation}`，携带 `x-tapers-catalog-namespace`。启用后使用支持逻辑库的版本启动全部成员，使用原交还 UUID 重新加载，并用 `default` 范围读回原文件核对摘要。新的路由版本要求显式逻辑库范围，也使旧执行器无法通过原路由准入。[逻辑带库 ADR](../adr/0003-logical-library-resource-boundary.md)
