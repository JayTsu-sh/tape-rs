# 历史格式与 Clippy 清理验收（2026-09-27）

在活动工作树 ltfs-recovery-ibm-interop 完成全仓 rustfmt 和既有 Clippy 修复，没有新增 allow/expect 或全局屏蔽。实际修复包括条件合并、客户端私有方法命名、缓存类型别名、测试初始化、文档段落、十六进制大小写；SgIoHdr 的 Default 复用原 new，PR key 解析使用定长数组切片保持原截断语义。未修改 ioctl 号、ABI、CDB、超时、依赖或 CLI。

软件验收：cargo fmt --all -- --check、cargo check --all-targets、cargo clippy --all-targets --all-features -- -D warnings 通过；cargo test --all-features 共208通过、7忽略。兼容glibc 2.34的fuse release构建成功；Zig保留一条外部链接器废弃优化参数提示，不以屏蔽处理。格式前的实际lint改动保存在 probes/results/cleanup-20260927-lint.patch。

隔离候选 ltfsd SHA256 f4680ed765b542048a4d3c69eed84b9f20ee9e9869621020d23e75a1b172d948。Holo隔离SR池执行3200字节文件写入/fsync、二进制扩展属性、改名、符号链接，正常停止leader后由node3接管，冷缓存复验通过。原六链接及源文件保真；临时目录删除后13行变17行（删除记录保留），原数据除根目录元数据/索引代数外不变。SR1 Full2，SR2 Full14，分别卸载至slot8/9；关闭数据 /home/rocky/tape-rs-cleanup-20260927/test-data，node3 round116。介质前后备份 /home/rocky/holo-cleanup-20260927 已校验。

正式三节点恢复node1 term45/round419/applied422，53行catalog完全一致，排序摘要3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3。TS1001L08 Full120、TS1000L08 generation2不变；原9文件长度/SHA256/mmap/barcode复验通过。EE三节点available、无活动任务、f3基线摘要未变，Holo九个publication ready。正式二进制保持32bd5541…；隔离进程全部停止，客户端卸载且缓存清空。

证据为 probes/results/cleanup-20260927-*，可执行回归脚本 probes/cleanup-20260927.py。未触及物理磁带设备；物理固件验收仍暂缓。接续任务1：FORMAT成功后、标签形成前的中断恢复；本项不声称覆盖该窗口。
