# 逻辑带库资源登记

`ltfsctl logical-library` 在指定物理带库中登记、列出和查看逻辑带库。资源清单使用驱动器序列号、存储元素地址和 I/E 元素地址。列表的 `unallocated_resources` 给出可选资源，`inventory.observation` 给出观测时间和介质位置。`inventory.status` 为 `current` 时，本轮完整库存可用于分配。缺失、未完整或过期时，先通过物理库 `library rescan` 刷新库存并确认任务成功。

```sh
ltfsctl -e HOST:7401 -l LIBRARY logical-library list --namespace NAMESPACE_UUID
ltfsctl -e HOST:7401 -l LIBRARY logical-library create archive-a \
  --namespace NAMESPACE_UUID --id REQUEST_UUID \
  --drive DRIVE_SERIAL --storage-slot 1001 --ie-slot 101
ltfsctl -e HOST:7401 -l LIBRARY logical-library show archive-a --namespace NAMESPACE_UUID
```

首次创建前生成并保存 UUID 与选择的全部资源。可以重复使用 `--drive`、`--storage-slot` 和 `--ie-slot` 指定同类资源。创建响应丢失时，按原 UUID 查询，或用相同 UUID 和完整参数重试。重复分配和篡改原请求都会返回冲突。创建只登记共享目录中的资源归属。

HTTP 使用 `/libraries/{serial}/admin/logical-libraries` 的 `GET` 和 `POST`。详情使用 `GET /libraries/{serial}/admin/logical-libraries/{selector}`，选择器为名称或 UUID。所有请求携带 `x-tapers-catalog-namespace`。`POST` 的 JSON 包含 `version`、`id`、`name` 和 `resources`，资源对象包含 `drives`、`storage_slots` 和 `ie_slots` 三个数组。首次成功返回 201，重试成功返回 200。Rust 客户端提供 `FdbClient::create_logical_library`、`list_logical_libraries` 和 `logical_library`。

本阶段提供资源登记。pool 归属、业务设备隔离和维护窗口启用将在后续交付。[逻辑带库 ADR](../adr/0003-logical-library-resource-boundary.md)
