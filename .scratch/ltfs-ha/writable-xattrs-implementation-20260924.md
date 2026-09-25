# 可写 xattr 软件实现与验证（2026-09-24）

分支 ltfs-recovery-ibm-interop；本轮没有远程连接、部署、真实设备访问或故障注入。

Client::setxattr/removexattr、HTTP /xattrs、FileService 元数据准入、执行线程原生属性修改和 FUSE setxattr/removexattr 已接通。支持当前写入卷的文件及原生目录，CREATE/REPLACE/删除错误语义与此前 LE 实测对齐。二进制与空值用 Base64 持久化，单值上限 64 KiB；属性提交沿用现有 LTFS 增量索引及发布流程，不重写内容。

FUSE 先同步同一 inode 的脏写句柄，后续内容覆盖继承最新属性；元数据提交结果未知时封锁后续写回。客户端响应丢失不重放。属性变更不按文件原长度占用数据容量，路径父子互斥，提交失败不发布成功状态。

## 已完成验证

- cargo test --all-features：200 通过、0 失败、6 门控测试跳过。
- 独立 kernel_file_operations --ignored：1 通过；这是现有文件操作的本机内核回归，不是新增 xattr 的系统调用验收。
- cargo check --all-targets --all-features 通过。
- cargo clippy --all-targets --all-features 完成，与 /tmp/read-failover-final-clippy.log 比较没有新增告警类别或数量。
- git diff --check 通过；全仓 cargo fmt --all -- --check 仍有历史格式差异，仅整理本轮新增函数及路由代码。
- 三节点模拟集群覆盖二进制值、空值、64 KiB HTTP 请求、目录属性、错误码、文件 UID/extent/mtime/hash 不变、后续写入保留属性、Leader 接管读回。
- 准入测试覆盖零数据容量预留、冲突操作、非法名称/flags/大小、提交未定；客户端覆盖设置/删除响应丢失不重放，FUSE 覆盖未定后禁止继续上传。

日志：/tmp/xattr-full-tests.log、/tmp/xattr-check.log、/tmp/xattr-clippy.log、/tmp/xattr-fmt.log、/tmp/xattr-kernel-regression.log。

## 边界与下一步

其他卷上的对象 EXDEV；根目录不支持修改；user.tape.*、user.tapers.*、user.ltfs.* 受保护，拒绝 user.user.* 别名。未实现 user.ltfs.sync。不能据此宣称完整 LE 兼容。服务端需升级后才支持新接口；线上版本本轮未变、未重查。

下一步先验证真实 FUSE setxattr/removexattr 系统调用，然后在隔离 Holo 介质上验证 tape-fuse 写属性→LE 读/改/删→tape-fuse 读回，尤其确认 LE 降级回写后的二进制属性与删除状态。验收后再统一部署；所有工作树改动仍未提交。
