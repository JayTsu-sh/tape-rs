# 目录接管偶发结果未定：根因与验收

原始失败：persistent_directories_survive_fuse_remount_and_takeover 在接管后的 rmdir(/empty) 返回 Indeterminate。8 并发共 80 次复跑出现 2 次相同失败；启用 error 级诊断后 160 次出现 6 次，均为旧轮次 2 的 06/2A/03。info 级日志改变时序，80 次未出现，不能据此否认问题。

## 根因和修正

测试只隔离 Raft 通信，没有关闭旧 HTTP 服务；Client 保留旧 Leader。新节点已完成 fencing 不表示旧节点的周期自检已执行。删除可在旧节点先进入批次，随后挂载遇到抢占 UA，服务端按未确认批次返回 Indeterminate。这是符合契约的结果；原测试错误地要求这次修改无条件成功。

持久化验收现在明确使用接管节点。另加确定性集成测试在目录删除批次注入同类失权，断言返回未定且不自动重放，新节点仍保留原版本，显式删除才能移除。此注入在 WRITE 阶段，覆盖未确认提交处理；历史随机失败的实际失权发生在批次挂载中。

客户端 namespace_change 原先丢弃服务端 detail，现保留底层 sense/设备上下文。没有调整超时、错误分类、重放策略或 SCSI 命令。新增测试先因丢失 sense 详情失败，修复后通过。修正后的原持久化测试 8 并发 160 次全部通过；219 项软件测试通过、7 忽略，fmt/check/严格 Clippy 通过。兼容构建仍保留外部 Zig 链接器废弃参数告警。

## Holo 验收

隔离目录 /home/rocky/tape-rs-directory-outcome-20260927，从 sync-boundary 的关闭 SR 数据接续。专用路径 /sr/directory-outcome-20260927。在批次挂载 READ(6) 后 SIGSTOP 旧执行者，新节点抢占并恢复后 SIGCONT；旧响应实际 HTTP500/status=indeterminate，包含预留冲突设备上下文。新节点目录存在，显式删除、重建和删除均成功。

首次探针已经得到预期未定响应，但随后错用 /stat?path 路由中断；改为 /stat/<path> 后重新执行完整验收。第二次 node1 round237 → node2 round246，通过。实际 FUSE 冷缓存读回六种链接、seed、LE 文件/属性/mmap，并创建删除空目录，通过。脚本为 probes/directory-outcome-fault.py，证据 results/directory-outcome-*。

关闭后 SR1 Full41、SR2 Full3，1139 条目录记录。隔离进程及 strace 停止，FUSE 卸载缓存空；介质在槽8/9。前后介质备份 /home/rocky/holo-directory-outcome-20260927 逐文件 SHA 一致。直接从 segment 正则抽取大索引 XML 会跨记录边界而解析失败，未将其当作通过；Full41 依据正常关闭提交日志，内容依据 FUSE 校验。

正式程序保持原版本，恢复 node1 term52/round454；53 条目录逐字段不变，原9文件 length/SHA/mmap/barcode 通过。未操作物理设备。
