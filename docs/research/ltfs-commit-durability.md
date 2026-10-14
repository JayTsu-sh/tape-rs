# LTFS 提交屏障与中断恢复证据

日期：2026-09-14。关联决策：暂存确认、卷提交与故障恢复契约；研究票据：磁带提交屏障与中断恢复证据。仅文档研究，未访问硬件、未验证现有实现。

## 已核实的边界

1. **WRITE 成功不必等于落带。**缓冲写可以在数据进入驱动器缓冲区后确认；非缓冲写才等待介质写入。IBM 的说明也指出这会影响性能及掉电时的数据完整性。这里说明的是驱动器行为，不应把 AIX 配置接口直接搬入 SG_IO。[IBM Device Buffers](https://www.ibm.com/docs/en/aix/7.2.0?topic=drives-tape-drive-attributes)

2. **有明确的介质同步命令。**IBM LTO SCSI Reference GA32-0928-08（2026-02-16）§5.2.46、印刷页 181：WRITE FILEMARKS 的 IMMED=0 等待完成；count=0 可同步所有缓冲数据及文件标记。IMMED=1 仅等待命令验证。§4.19.2–3、页 65–66：先前 GOOD 仍可能产生延迟错误，其报告与 I_T nexus 的错误归属有关。§4.12.2 表 17、页 51：变长写的 early warning 可已写块，而物理 EOM 可未写；仅看 sense key=0 不足以确认完整成功。[IBM LTO SCSI Reference](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf)

3. **SG_IO 是命令通道，不是事务回执。**内核接口分别返回 SCSI、host、driver 状态、实际 sense 长度及 resid；resid 表示请求传输量减实际传输量，不证明磁带持久化。实现推论：必须综合检查这些字段；WRITE 非零或异常残差不得被当作完整写成功，亦不能按普通文件短写自动补写剩余字节。[Linux sg_io_hdr 定义](https://github.com/torvalds/linux/blob/master/include/scsi/sg.h)

4. **可恢复与一致卷是不同保证。**LTFS 2.5.1 Annex C.4、页 73–74 允许 sync 后暂不一致，但必须能无损恢复。§5.4、页 26–28 用卷 UUID、自指针、generation 和回指针识别索引；可解析 XML 或较大 generation 单独不足以证明有效提交。§9.2.13 与 Annex H.2 警示：不理解增量索引的恢复程序可能丢失新状态。推论：IP 落后时必须发现、验证 DP 上的有效索引及必要索引链，不可只读旧 IP；数据、上传凭据与映射必须属于同一可恢复提交。[SNIA LTFS 2.5.1](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf)

## 候选顺序（实现推论，非已通过验证的协议）

在已取得独占设备所有权后，串行写入文件数据与完成记录，依 LTFS 布局写入 DP 索引及结束标记，再执行明确的非立即同步并检查完整结果；只有该过程的恢复保证已验证，才能确认提交。后续 IP 检查点仍需单独同步并维护一致卷要求。上述顺序不决定合批等待时间、回执格式或保留期限。

响应丢失、超时或节点死亡时，将结果记为未知；B 在隔离旧主后重新识别介质并从介质证据裁定。B 新开设备、获得 GOOD 的空缓冲同步，均不能证明 A 的某次历史上传成功。不能继承 A 的内存磁带位置，也不能盲目重放具有位置副作用的 WRITE。

## 最小失败验证矩阵（建议，尚未执行）

| 注入点 | 必须观察到的结果 |
|---|---|
| 缓冲 WRITE 后、索引前断电 | 不误报完整文件提交；保留此前提交 |
| 索引中途截断、结束标记前断电 | 不接受残缺索引；有歧义则 UNKNOWN |
| 屏障前后断电、成功响应丢失 | B 独立读取介质；已确认提交全部可恢复 |
| DP 已同步、IP/MAM 更新失败 | 能发现 DP 新状态；不得回退后误判未提交 |
| resid 异常、early warning、EOM、延迟错误 | 不因 NO SENSE 或 ioctl 成功跳过分类 |
| A FC 断链、B 新 I_T nexus、驱动器重启 | 验证错误归属、UNIT ATTENTION、定位与恢复路径 |

先用可控磁带模拟器检验状态机，再对获授权的测试介质做真实掉电、断链与跨节点验证。模拟器不能证明驱动器持久化。

## 仍需证据

SSC-5 正文经 [T10 官方入口](https://www.t10.org/cgi-bin/ac.pl?t=f&f=ssc5r05.pdf) 进入访问表单，本次没有拿到可逐条核对的正文；不能宣称完成 SSC-5 一致性审计。IBM 新版参考覆盖 LTO-8 系列，但现场固件、缓冲模式、HBA 残差可靠性、加密配置、掉电后 EOD 可恢复性和接管耗时均未实测。进入实现前须固定设备与规范版本、记录适用模式页，并补齐上述硬件验证；规范许可本身不是当前代码已实现可靠提交的证据。
