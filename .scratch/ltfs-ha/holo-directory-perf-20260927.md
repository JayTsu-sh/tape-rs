# 大目录枚举实测与优化（2026-09-27）

## 固定实验

Holo 隔离 sr 池，100及1000个独立小文件，每目录另有2个目录及3个链接（正常、悬空、循环）；总1132条catalog。8并发HTTP暂存后等待全部committed，再测枚举。正式数据未用于负载。每规模重复5次，Python scandir验证全部名字、类型及数目。使用相同客户端、节点2及HTTP计数代理、相同数据；冷FUSE启动，目录窗口不读文件内容。脚本 probes/directory-perf-20260927.py 固定参数/环境门控。

| 文件数 | 优化前中位数 | 优化后中位数 | 每轮HTTP请求前→后 | FUSE采样最大RSS前→后 |
|---|---:|---:|---:|---:|
|100|105.54ms|12.34ms|113→10|7744→5828KiB|
|1000|843.12ms|17.57ms|1013→10|12352→10564KiB|

1000文件最大值856.50→19.67ms；FUSE CPU每轮9–10 ticks→0–1 ticks（时钟粒度限制，不代表零CPU）。结果在results/directory-perf-20260927-{before,after}.log及summary.json。约48倍是此合成目录枚举结果，不是物理磁带吞吐或业务SLA；不以5次样本声称P99。全池扫描仍存在，不宣称百万级目录可扩展性或长期RSS上界。

## 改动

原FUSE opendir为每个非目录补查stat以判断链接，形成N次网络往返。目录服务从既有metadata同时提取长度/目录/链接类型，新增可选is_symlink响应字段；客户端保留旧服务端缺字段时的stat回退及ENOENT/ESTALE处理。FUSE从单次目录快照构建类型并沿用稳定分页。不改变持久化格式、索引提交或SCSI行为，不引入类型缓存或新依赖。

新1000项单测验证无逐项stat及旧服务端成功/消失/I/O失败路径；集群链接测试验证提交前后接管的类型。fmt/check/严格Clippy通过，214测试通过7忽略。第一次全套与兼容构建并发时，persistent_directories_survive_fuse_remount_and_takeover在rmdir返回Indeterminate；单独复测及无并发构建的完整重跑通过。不能证明偶发超时根因，保留两份日志，不修改确认语义来掩盖它。

兼容构建ltfsd SHA256 11e5512ef8f98f56a096619c1eefdcbd1078857a01f926a1c8a258dab8fbf12a，FUSE a43950d6cfb63551806ef8b23963a944336adfe89cef2a3a34b67d0b6e52a206；部署目录/home/rocky/tape-rs-directory-perf-20260927。正常停机后更换隔离程序，原数据1132行相同，node2/term16/round203，SR1 Full24 Complete。介质备份/home/rocky/holo-directory-perf-20260927，两带SHA校验；各节点before-optimization.tgz保存关闭状态。正式程序保持原32bd5541…。

首次准备探针固定node1但当时node2为leader，503后改为从cluster读取leader重跑；未更改产品。首次全量内容检查的SSH包装58秒到期，未将其计为通过，改为持久化结果并独立执行完整冷缓存检查。

冷缓存1100文件逐字节通过（77.43秒，与目录窗口分开），原LE文件/六种链接/属性/mtime复验通过。正式恢复node1/term50/round444，53行逐行不变，原9文件冷缓存验证通过；EE三节点available无任务、f3原摘要不变、9发布ready。FUSE已卸载、隔离进程停止、SR1 Full24槽8/SR2 Full3槽9。最新关闭数据directory-perf/test-data；下一项5。

收尾首次逐行比较隔离catalog失败：正常停机完整索引及接管把1132行generation统一到24，除此字段外所有路径、内容摘要、版本和metadata逐项一致。正式catalog逐字段完全不变。这个预期索引代数变化明确记录，不把原始全字段比较描述为通过。
