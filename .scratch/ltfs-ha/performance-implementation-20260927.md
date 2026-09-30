# LE 对照优化实施与验收

活动工作树 `/work/jay/tape-rs-ltfs-recovery-ibm-interop`。本轮保留普通sync持久化边界、PR隔离、实时位置验证、EOD严格支持要求；SG direct保持关闭。真实介质仅RC0018L9，同UUID `878f7b34-4910-4d09-9f1f-3e4725ede6d8`，未格式化。

## 实现

1. **逐CDB计量**：ScsiDevice DEBUG记录设备、原始CDB、字节、wall_us、内核duration、status/host/driver/resid/info/raw sense。错误路径先等待在途I/O结束，不提前访问可能仍被内核使用的sense；不重发命令。
2. **服务端常驻卷**：VolumeSession仅保存owned状态，保存/恢复不克隆目录、不延长设备借用。executor在同一Active生命周期内复用写卷，包含连续批次、写带读取及卸载检查点；失败、选带、换带和demo旁路写入清除缓存。resume验证原设备身份、PR轮次、TUR和session_epoch。
3. **设备事件不被重试掩盖**：transport在异常/UA和状态修改命令返回时递增epoch，位于重试层之下。后台PR查询即便消费了UA，旧会话仍拒绝复用；自定义transport未提供epoch时自动不缓存。资格与位置检查未删除。
4. **挂载顺序**：先扫IP，再扫DP；扫描结束后直接核验DP追加位置。保留两分区EOD、VCI、构造完整性、历史/增量恢复与异常只读逻辑，减少DP→IP→DP往返，不以跳过安全检查换性能。
5. **读缓冲与Full编码**：同卷/ReadView复用对齐读缓冲；Full XML编码直接借用工作索引，消除一次整树克隆。
6. **大目录增量计算**：只复制索引头和真正输出的目录/文件；每层用名称映射/集合代替反复线性查找，保留UID区分目录替换，输出顺序不变。重放不再先复制待丢弃的旧root。仍扫描目录，不宣称已实现完全O(变更路径)的dirty journal。
7. **周期Full停留DP**：达到增量链上限时使用已有sync路径生成DP Full，重置增量深度；不再每6批切到IP再切回。IP债务保留，显式commit/清洁卸载仍补齐双分区。IP来源的特殊视图仍走原完整checkpoint。

## 没有凭猜测改变的部分

- FM0已实测约1.75ms中位，并非秒级瓶颈，保留最终屏障。
- 每文件READ POSITION和PR guard保留；当前允许同initiator其他句柄，不能凭内存位置直接删掉。
- 不增加无实测依据的并发SCSI、预取线程或新的合批延迟策略；大流量此前仅差LE约1%，尚未证明供数/复制瓶颈。已有合批能力继续使用。
- SG direct不重新打开；不把缓存命中与异步接收计为磁带持久化速度。

## 软件验收

- 最终行为版 `cargo test --all-features`：262 passed，7 ignored，0 failed。
- 默认cargo test、all-targets check、strict Clippy `-D warnings`、fmt检查均通过。最终日志调整后另跑device单测。
- 新测试：20独立批次之间零标签/索引READ；写带读取仅一次数据READ；读后追加；dirty状态不缓存；旧round/reset拒绝；后台PR消费UA后仍拒绝会话；周期DP Full跨掉电重挂保持链界限、最后显式checkpoint清除IP债务；周期Full失败不发布。
- 兼容release通过，未加依赖、未用allow属性关闭告警。软件日志归档于 `probes/results/physical-session-20260927/`。
- 初次集群计数测试读了另一个模拟句柄的日志，导致数据READ计数为0；已改统计执行线程实际调用，单测和全量通过。周期Full测试修改前先红，再改生产路径后绿；原日志保留。

## 纯内存性能

`examples/index_delta_perf.rs`，每次修改1文件，验证XML解析后重放等于完整视图；3次独立样本：

| 文件数 | 旧差分中位 | 新差分中位 | 验证 |
|---|---:|---:|---|
| 10,000 | 606.18ms | 3.965ms | 每次1变更、1311字节XML、重放相等 |
| 100,000 | 未测 | 52.30ms | 每次1变更、1313字节XML、重放相等 |

10,000文件的差分CPU时间约153倍改善，**不是整体磁带写入153倍**。证据 `probes/results/index-delta-perf-20260927.json`；临时旧二进制用于before，数据创建/完整视图克隆不计入差分计时。

## 实机分阶段结果

`.143` 的3节点同机隔离集群，端口17700/17710/17720，HTTP17701/17711/17721。这是物理设备I/O与服务生命周期验收，不是跨主机HA验收。只分配RC18到pool `rc18-session`（UUID98310642-518f-4db9-872a-3ba69933011e），池标记为本轮新增持久化元数据。

第一候选已包含常驻卷/读缓冲/计量，未包含新的挂载顺序、增量差分算法和周期DP Full。8次4KiB提交：48.796、18.332、18.489、13.733、18.601、63.302、48.654、18.519秒，合计248.425秒。第一次及第7次从IP回DP；第6次仍是旧周期DP/IP Full。中途两次真实读校验4.773/3.595秒。整个daemon生命周期只mount一次。FM0中位1.7525ms。

第一候选正常停机完成后进行Rust冷读与归槽；最终候选自动接续，由独立目录文件名标识：`session2-*`。最终结果尚待完成后补充，不能把第一候选结果当作最终全部改动的实机验收。

候选哈希：
- 第一候选ltfsd-session-candidate：5dc253b9e337a75bd3eec32e76a010fe5398386e7a13d85bbe0ceff62dbfdc8f。
- 最终ltfsd-perf-final：a6a17e27991afd95f577f0a296bf53b24047223079a99167e2515587aff83853。
- 最终performance_compare-perf-final：317b09ce195156163641dfa0699a4714881b1dd8fce5619740a3ea26d0824738。

## 对比限制

两轮小批测的是实际ltfsd含SHA/spool/Raft/catalog与增量提交，不能把其数值直接对比LE Full sync来宣称同算法提速。有效改进证据是重复mount消失、周期Full避免IP机械往返，以及独立测得的索引CPU成本。最终判定还需冷挂全量SHA和LE互读。跨两盘换带仍未覆盖，RC17没有重新初始化。
