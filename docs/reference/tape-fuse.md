# tape-fuse 完整参数参考

FUSE 文件系统客户端。用法说明见[命令行参考手册](../cli-reference.md)。

本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。可选命令随构建特性变化，下面的帮助为本次构建内容。

构建产物 SHA256：`60d4ccc59c88f2f84a043eefff8e90838d718fe634554428d063e1f8afe11853`。共 1 个帮助入口。

## 目录

- [tape-fuse](#tape-fuse)

## tape-fuse

```text
挂载 ltfsd 文件服务；fsync 确认落带

Usage: tape-fuse [OPTIONS] --cache-dir <CACHE_DIR> <MOUNTPOINT>

Arguments:
  <MOUNTPOINT>  已存在的挂载目录

Options:
  -e, --endpoint <ENDPOINTS>
          节点的客户端接口地址，可重复指定
      --library <LIBRARY_SERIAL>
          目标带库的真实 changer 序列号；省略时访问默认库
      --pool <POOL>
          文件命名空间的池名称或 UUID；库内有多个池时必填
      --fdb-namespace <FDB_NAMESPACE>
          固定 FDB 命名空间；须指定 --library 和 UUID 格式的 --pool，默认只读
      --fdb-journal-dir <FDB_JOURNAL_DIR>
          FDB 持久化请求日志目录（0700）；指定后恢复原操作并启用文件及节点修改
      --library-changer <LIBRARY_CHANGER>
          逻辑库 changer 设备（必须与 LE/ltfsd 互斥使用）
      --library-changer-serial <LIBRARY_CHANGER_SERIAL>
          预期 changer VPD 序列号
      --library-drive <LIBRARY_DRIVE>
          允许调度的 drive，格式 SERIAL=/dev/sgN，可重复
      --library-state <LIBRARY_STATE>
          资源状态 SQLite 路径；禁止复用 ltfsd catalog
      --library-node <LIBRARY_NODE>
          独占执行节点号 [default: 1]
      --library-round <LIBRARY_ROUND>
          运维分配的单调递增执行轮次（1..2^48），不得复用旧轮次
      --cache-dir <CACHE_DIR>
          本地缓存目录，每次挂载创建独占子目录
  -h, --help
          Print help
```
