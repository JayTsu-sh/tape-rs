# tape-rs

[命令行参考手册](docs/cli-reference.md) · [管理模型](docs/management-model.md) · [FoundationDB 部署与接口](docs/foundationdb.md)

运行时独立于 GPFS 和 IBM EE：FDB 保存权威目录、介质归属、异步任务与通知，本项目通过自身 SCSI/LTFS 实现访问磁带，向客户端提供 S3 和 FUSE。导入 EE 磁带时直接解析带内索引，不要求 GPFS 挂载或 EE 服务。详见[运行时依赖约束](docs/adr/0002-gpfs-independent-runtime.md)。
