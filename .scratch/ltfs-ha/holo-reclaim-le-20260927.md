# 回收介质 IBM LE 往返（2026-09-27）

任务3通过。隔离候选 cc4f390，ltfsd SHA256 1040995424f01e4f7060cf617806f48179af51598bd2b8f0a55db39b53049442。未改生产代码、正式程序或物理设备。

## 过程与结果

SR2501L8 回收后的 Full7（六种链接）离线复制到 LISA 新建专用 SR2501L08，ANSI SR2501 保持不变。IBM LE 2.4.8.3 在 ee1 /ltfs/SR2501L08 挂载第7代成功；原文件、相对/悬空/目录/循环/中文/链式链接均通过。LE 创建65,557字节文件，二进制/空值xattr，修改既有文件属性，user.ltfs.sync成功；正常卸载后DP/IP均Full8。

备份后离线复制回 SR2501L8，三节点候选 node1/term14/round163 接管，识别第8代 Complete；目录从18增至19行。冷缓存FUSE验证全部链接、原seed、新文件SHA256、二进制及空xattr、pread/mmap、纳秒mtime均与LE一致。新文件SHA256 dba4ff0e067063e9713866943bacdc7c603d0024f2cc9e65fd526db5cab227dc。证据：probes/reclaim-le-20260927.py、results/reclaim-le-20260927-{le-pass,fuse-pass}.log及isolated-{before,after}.json。

## 准备阶段失败与恢复

- 最初把SR副本置于TS1000L8，LE正确拒绝ANSI条码不符（LTFS11530E），未进入写入探针；原TS1000L8 Full99已从校验备份恢复，LE recover成功，正常卸载/取消分配。跨文件系统rename初次EXDEV后未中止后续命令，曾挂载原Full99；未写探针文件，后续准备改为同文件系统交换并严格中止失败。
- 独立TAPERS LE实例先拒绝TS4300型号；临时既有3573兼容profile后遇VPD FB许可校验，不做绕过。已恢复TAPERS原profile/序列号与正式服务。
- 六字符条码不符合Holo命名规则，HTTP400无创建；改为合法SR2501L08。创建触发Holo自动停用两条额外EE changer发布，按保存配置重新发布，9条ready。额外发布ID可能变化，按IQN/角色/配置核对，非旧ID假定。
- 新副本copytree未保留属主，首次fsync报EIO。Holo日志明确PermissionDenied（UID992读取root文件），不是介质协议问题。恢复副本与先前恢复的TS1000L8为holo属主；LE unlock、正常卸载、recover后干净重跑成功。失败探针及日志保留，不计为通过。

源/返回/原LE备份在Holo /home/rocky/holo-reclaim-le-20260927/{native-full,native-before-return,le-before,le-return}，复制前后SHA校验；不把备份覆盖运行中介质。新LE副本保留Full8、取消分配，slot1033；原LE TS1000L8取消分配。TAPERS SR1归槽8 Full8、SR2槽9 Full3。

## 收尾

最新关闭数据 /home/rocky/tape-rs-reclaim-le-20260927/test-data（.71/.72/.74），两驱动配置；勿回退namespace旧数据。所有隔离进程/FUSE停止、缓存清空。正式恢复node1/term49/round439，TS1001L08仍Full120；三节点53行与开始逐行一致，原9文件冷缓存长度/SHA/mmap/条码通过。EE三节点available无任务，f3 SHA仍cd5c2a3513cfef51485baeddfeda2009269e2bb53944aa469512f4dc2ca2779e。物理TS4300未验证。
