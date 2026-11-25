# 逻辑带库资源登记

`ltfsctl logical-library` 在指定物理带库中登记、列出和查看逻辑带库。资源清单使用驱动器序列号、存储元素地址和 I/E 元素地址。列表的 `unallocated_resources` 给出尚未登记归属的资源，`inventory.observation` 给出观测时间和介质位置。`inventory.status` 为 `current` 时，本轮完整库存可用于分配。缺失、未完整或过期时，先通过物理库 `library rescan` 刷新库存并确认任务成功。

```sh
ltfsctl -e HOST:7401 -l LIBRARY logical-library list --namespace NAMESPACE_UUID
ltfsctl -e HOST:7401 -l LIBRARY logical-library create archive-a \
  --namespace NAMESPACE_UUID --id REQUEST_UUID \
  --drive DRIVE_SERIAL --storage-slot 1001 --ie-slot 101
ltfsctl -e HOST:7401 -l LIBRARY logical-library show archive-a --namespace NAMESPACE_UUID
```

首次创建前生成并保存 UUID 与选择的全部资源。新增或移除资源需要当前完整库存证明资源为空，且没有未结清动作或 home 引用。可以重复使用 `--drive`、`--storage-slot` 和 `--ie-slot` 指定同类资源。创建响应丢失时，按原 UUID 查询，或用相同 UUID 和完整参数重试。重复分配和篡改原请求都会返回冲突。创建只登记共享目录中的资源归属。

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

业务启用和完整的设备、文件入口隔离需要完成后续切换及验收。[逻辑带库 ADR](../adr/0003-logical-library-resource-boundary.md)
