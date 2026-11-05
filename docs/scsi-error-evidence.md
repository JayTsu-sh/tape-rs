# SCSI 错误证据

`TapeError::ScsiCommand` 保留原有 status、sense_key、ASC/ASCQ，并携带可选的完整 `SenseInfo`。真实 SG_IO 的错误会保存原始 sense、response_code、实际接收长度与有效范围；`sense_info()` 可越过 `CommitFailed` 包装读取原证据。没有设备回复的合成错误使用 None，不伪造原始数据。错误文本也包含诊断，current 和 deferred 不再被压成相同字符串。

SG_IO sense 缓冲从 64 扩为 255 字节，即现有 u8 长度字段的完整预算。IBM LTO 文档说明设备可生成 96 字节 sense，读 ILI 情况可能只返回 18 字节；64 字节缓冲不足以保留完整诊断。ioctl 号、C ABI、CDB、数据缓冲方向、超时与重试没有改变。边界检查仍先验证内核返回长度，再解析。

`SenseInfo` 最多保留 255 字节原始数据，同时记录原始输入长度；有效范围取实际捕获长度与 `8 + additional sense length` 的较小值。未知格式也保留原始字节和 response_code。既有 filemark、EOM、ILI、INFORMATION、sense key 解析及成功/失败分类沿用原规则。

`current_condition()` 从原始字节取得完整 current 条件，只接受 0x70/0x72、完整声明长度与足够的字段。deferred 0x71/0x73、截断、未知格式及超出 SG 长度预算的输入均返回 None；短包不会因缺少 ASCQ 而被补成一个可归因的 00。此方法只给出 key/ASC/ASCQ，不证明错误属于当前命令、当前磁带或其 payload；上层仍须结合实际失败 CDB、阶段、卷身份与 owner。它不授予重试、写入或清除故障的权限。

模拟器的 CheckCondition 注入也经相同 finish_command 生成显式模拟 sense。默认测试只做软件验证；真实设备测试 `scsi_error_evidence_physical` 保留 ignore 门控，必须由现场协调器确认目标序列号、当前 owner、无并发设备任务和原 PR key，再提供以下环境变量运行：

- TAPE_RS_SCSI_EVIDENCE_DEVICE：已确认设备路径。
- TAPE_RS_SCSI_EVIDENCE_SERIAL：目标驱动器序列号，打开前还会通过 sysfs 校验。
- TAPE_RS_SCSI_EVIDENCE_OWNER_KEY：当前 owner 的十六进制 PR key；只读取，不注册/抢占/释放。

实带用例仅允许 INQUIRY 和 PR IN，先读取支持的 VPD 页，再请求一个未列出的非厂商页以获得真实 CHECK CONDITION，验证原始诊断与提交错误包装中的保留效果，并比较前后 PR。不会装卸、定位、读写正文、采 TapeAlert/MAM 或变更池归属。它只验证该诊断路径，不等同真实介质损坏、deferred 故障构造或监控状态机验收。

依据：[IBM LTO SCSI Reference GA32-0928-08，§5.2.31 REQUEST SENSE 与 §4.19 deferred errors](https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference%20GA32-0928-08%20%28EXTERNAL%29.pdf)。实际当前型号返回格式和长度以本轮现场日志为准。
