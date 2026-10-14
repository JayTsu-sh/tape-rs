# LE FUSE ↔ tape-rs 真实轮流挂载证据

日期：2026-10-14（Asia/Shanghai）

节点：10.131.7.209 / lisa-3

真实库：TS4300，changer `/dev/sg5`，测试 tape drive `/dev/sg4`
测试介质：`AA0013L8`，原始 slot 1，目标 drive 0

## 初始状态

- `.209` 无 FUSE 挂载、无 LE 进程、无 `/dev/sg*` holder。
- `node-scheduler` 为 `inactive`。
- tape-rs inventory 成功识别 1 robot、2 drives、35 storage、5 I/E。

## LE 第一次挂载

命令使用临时挂载点 `/mnt/rea-le-roundtrip-20261014`、临时 work directory `/tmp/rea-le-work-20261014`、管理端口 `17600`。

- FUSE 挂载成功。
- `leadm node show`：`Available`，LE `2.4.8.2 (10520)`。
- `leadm library show`：`3573-TL`，2 drives、35 slots、5 I/E、1 robot。
- 根目录真实出现 7 个卷目录：`8A0042L8`、`8A0043L8`、`8A0047L8`、`AA0000L8`、`AA0013L8`、`L00284L8`、`L00285L8`。
- `AA0013L8/b1` 读取成功：33516 bytes。
- SHA-256：`6fdc3ec5848d9368fe35d6270e0fecb66d276d0d03b8df45e0dbce74979c02af`。

## tape-rs 接管

LE 卸载后，库存显示 `AA0013L8` 回到 slot 1，drive 为空。直接访问 `/dev/sg4` 首次返回真实 `NOT READY / ASC 3A00`，说明磁带已被 LE 卸载。

随后执行：

```text
tape-rs load --device /dev/sg5 --slot 1 --drive 0
```

MOVE MEDIUM 成功，约 25 秒；tape-rs status 为 Ready，position 为 Block 0/BOT。

随后执行 `ltfs-list --device /dev/sg4` 和 `ltfs-read --device /dev/sg4 --name b1`：

- 两者都成功读到 LTFS label：version `2.4.0`、blocksize `524288`、compression `true`。
- 两者都在构造 LTFS 索引时返回：`Recovery restricted: 构造超过预算 67108864 字节`。
- 两者返回码均为 1。

## LE 第二次挂载

用临时挂载点 `/mnt/rea-le-roundtrip-20261014-2`、work directory `/tmp/rea-le-work-20261014-2`、端口 `17601` 再次启动 LE：

- FUSE 挂载成功。
- `leadm node show` 为 `Available`。
- 根目录再次出现同样的 7 个逻辑卷目录。
- 再次读取 `AA0013L8/b1` 得到 33516 bytes。
- SHA-256 与第一次 LE 读取完全一致：`6fdc3ec5848d9368fe35d6270e0fecb66d276d0d03b8df45e0dbce74979c02af`。

## 清理

- LE FUSE 正常卸载返回 0。
- 停止残留 LE 进程。
- tape-rs 执行 `unload --device /dev/sg5 --drive 0 --slot 1`，将 `AA0013L8` 返回原 slot 1。
- 最终无挂载、无 LE/tape-rs 进程、无设备 holder；`node-scheduler` 保持 `inactive`。

## 结论

轮流挂载闭环成立：`LE mount/read → unmount → tape-rs load/read → unload/LE mount/read`。LE 两次读回内容一致。tape-rs 的真实失败点不是磁带不可读，而是已读到 LTFS label 后，默认 64 MiB 索引恢复预算耗尽；该 GAP 已在真实同一卷上重复复现。

## 追加：大文件、FORMAT 与 CDB 闭环

- 用户授权后，测试带 `AA0013L8` 在第二台真实 drive `/dev/sg2` 完成 tape-rs 2-partition FORMAT，生成 UUID `97b7990c-9e7a-4532-a765-ed718eaddf6c`。
- tape-rs 写入 `perf64m.bin`，大小 67108864 bytes；源 SHA-256：`2009f414fa6abf46885f6701a5f454b3f5af29afc1ff47b99bd4f42719d0d8c3`；耗时 35.44 秒。
- 释放 tape-rs PR 后，LE 接管同一卷读取 64 MiB，耗时 16.75 秒，SHA 一致。
- LE 卸载后，tape-rs 接管读取同一文件，耗时 18.55 秒，返回码 0，SHA 一致。
- LE 内核 SCSI trace 与 tape-rs `SG_IO` 日志均记录 `READ(6) 08 00 08 00 00 00`、请求 524288 bytes；READ CDB 对照已闭环。
- drive 0 的 FORMAT 曾返回 `sense_key=0x07 ASC/ASCQ=0x5A/0x02`；换用 drive 1 后 FORMAT 成功，该失败作为 drive-specific 异常留证。

## 追加：MAM、density、压缩与诊断

- `sg_read_attr /dev/sg2` 真实读取完整 MAM：medium type `0x0`、medium density `0x5e`、TapeAlert 0、当前卷 UUID/VCI、容量和加密位置字段均有输出。
- `report-density` 真实返回 LTO-7、LTO-8 Type M、LTO-8；当前介质使用 LTO-8 `0x5e`。
- 当前 LTFS label 的 `compression=true`，且 LE/tape-rs 对 64 MiB 文件双侧读取 SHA 一致；压缩配置路径已验证。
- 使用真实 `WRITE ATTRIBUTE (0x8D)` 将 MAM `0x0801` 写为 `REA-MAM-20261014`，SCSI status Good；随后 `sg_read_attr` 回读值完全一致。MAM 修改后 LE 重新挂载并识别 `AA0013L8` 成功。
- tape-rs 后台 SEND DIAGNOSTIC 成功提交；前台模式第一次因装带后的 UNIT ATTENTION `28/00`，清除后返回 `24/00`，说明该 drive 不接受当前前台自检模式；不能把后台成功扩大解释为前台成功。
- 当前介质 MAM 的加密位置为 unknown/`ff`、medium type `0x0`，证明本样本未启用加密；普通 LTO 介质不能转换成 WORM，WORM/硬件加密能力仍需对应特殊介质或 TS4300 密钥策略才能做正向验证。
