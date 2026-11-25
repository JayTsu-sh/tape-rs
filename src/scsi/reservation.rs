//! SCSI 持久预留（PERSISTENT RESERVE IN/OUT）与设备层隔离流程。
//!
//! 用途：Raft 决定谁有资格执行，但驱动器不认识任期。新执行者用本模块对每个受管设备
//! 抢占预留并**回读确认**，此后旧执行者的介质命令由设备自己拒绝。
//!
//! 纪律（与 IBM LTFS 相反，它遇冲突会抢回）：持有者一旦看到 [`ownership_lost`] 为真的错误，
//! 必须停止，不得重新注册或抢占。抢占只发生在 [`fence`] 里，由上层在取得执行资格后调用。

use log::{info, warn};

use crate::error::{Result, TapeError};
use crate::scsi::cdb;
use crate::scsi::transport::TapeTransport;

mod successor_capture;
pub use successor_capture::{CaptureOutcome, CaptureRequest, capture_b_pr_evidence};

/// Exclusive Access：非持有者的介质命令全部被拒绝（Write Exclusive 仍允许读和定位）。
pub const PR_TYPE_EXCLUSIVE_ACCESS: u8 = 0x03;

const SA_IN_READ_KEYS: u8 = 0x00;
const SA_IN_READ_RESERVATION: u8 = 0x01;
const SA_OUT_REGISTER: u8 = 0x00;
const SA_OUT_RESERVE: u8 = 0x01;
const SA_OUT_RELEASE: u8 = 0x02;
const SA_OUT_PREEMPT: u8 = 0x04;
const SA_OUT_PREEMPT_ABORT: u8 = 0x05;
const SA_OUT_REGISTER_IGNORE: u8 = 0x06;

const PR_TIMEOUT_MS: u32 = 30_000;

/// PR 命令自己的 UNIT ATTENTION 处理：全部越过，包括 2A/03—05。
/// 这些信号在这里是历史（上一次被抢占时留下的），而本模块接下来读到的注册表才是现状；
/// `verify_holder` 读到的现状不对就会报失去资格，所以越过它们不会掩盖问题。
fn through_unit_attention<T>(
    what: &str,
    dev: &dyn TapeTransport,
    mut f: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut seen = 0;
    loop {
        match f() {
            Err(TapeError::ScsiCommand {
                sense_key: 0x06,
                asc,
                ascq,
                ..
            }) if seen < 8 => {
                seen += 1;
                info!(
                    "{}: {} 前越过 UNIT ATTENTION {:02x}/{:02x}",
                    dev.identity(),
                    what,
                    asc,
                    ascq
                );
            }
            other => return other,
        }
    }
}
/// 键的首字节。区别于 IBM LTFS 的 0x10/0x40/0x60，现场一眼能认出是谁的键。
pub const KEY_PREFIX: u8 = 0x4C;

/// 8 字节预留键：前缀 + 节点号 + 48 位执行轮次。同一节点新一轮当选得到新键，
/// 所以旧轮次、旧实例的键天然陈旧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationKey(pub u64);

impl ReservationKey {
    pub fn new(node: u8, round: u64) -> Self {
        Self(((KEY_PREFIX as u64) << 56) | ((node as u64) << 48) | (round & 0xFFFF_FFFF_FFFF))
    }
    pub fn node(self) -> u8 {
        (self.0 >> 48) as u8
    }
    pub fn round(self) -> u64 {
        self.0 & 0xFFFF_FFFF_FFFF
    }
    pub fn is_ours(self) -> bool {
        (self.0 >> 56) as u8 == KEY_PREFIX
    }
}

/// 设备自己报告的注册与预留现状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub generation: u32,
    pub keys: Vec<u64>,
    /// 持有者的键与预留类型。
    pub holder: Option<(u64, u8)>,
}

fn pr_in(dev: &dyn TapeTransport, service_action: u8) -> Result<Vec<u8>> {
    through_unit_attention("PERSISTENT RESERVE IN", dev, || {
        pr_in_once(dev, service_action)
    })
}

fn pr_in_once(dev: &dyn TapeTransport, service_action: u8) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 1024];
    let c = cdb::persistent_reserve_in(service_action, buf.len() as u16);
    let r = dev.execute_read(&c, &mut buf, PR_TIMEOUT_MS)?;
    buf.truncate(r.transferred);
    Ok(buf)
}

fn pr_out(dev: &dyn TapeTransport, sa: u8, pr_type: u8, key: u64, sa_key: u64) -> Result<()> {
    let mut param = [0u8; 24];
    param[0..8].copy_from_slice(&key.to_be_bytes());
    param[8..16].copy_from_slice(&sa_key.to_be_bytes());
    let c = cdb::persistent_reserve_out(sa, pr_type, param.len() as u32);
    through_unit_attention("PERSISTENT RESERVE OUT", dev, || {
        dev.execute_write(&c, &param, PR_TIMEOUT_MS)
    })?;
    Ok(())
}

/// 读注册表与预留持有者。只读，任何发起方都可以执行。
pub fn read_status(dev: &dyn TapeTransport) -> Result<PrStatus> {
    read_status_with(|action| pr_in(dev, action))
}

fn read_status_with(pr_in: impl Fn(u8) -> Result<Vec<u8>>) -> Result<PrStatus> {
    let k = pr_in(SA_IN_READ_KEYS)?;
    if k.len() < 8 {
        return Err(TapeError::InvalidResponse {
            expected: 8,
            actual: k.len(),
        });
    }
    let generation = u32::from_be_bytes([k[0], k[1], k[2], k[3]]);
    let add = u32::from_be_bytes([k[4], k[5], k[6], k[7]]) as usize;
    let end = (8 + add).min(k.len());
    let keys = k[8..end]
        .as_chunks::<8>()
        .0
        .iter()
        .copied()
        .map(u64::from_be_bytes)
        .collect();

    let r = pr_in(SA_IN_READ_RESERVATION)?;
    if r.len() < 8 {
        return Err(TapeError::InvalidResponse {
            expected: 8,
            actual: r.len(),
        });
    }
    let add = u32::from_be_bytes([r[4], r[5], r[6], r[7]]) as usize;
    let holder = if add >= 16 && r.len() >= 24 {
        let key = u64::from_be_bytes(r[8..16].try_into().expect("8 bytes"));
        Some((key, r[21] & 0x0F))
    } else {
        None
    };
    Ok(PrStatus {
        generation,
        keys,
        holder,
    })
}

/// 隔离一个设备的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceOutcome {
    /// 已抢占（或取得）预留，并经回读确认：注册表只含本轮键，持有者是本轮键。
    Fenced { before: PrStatus, after: PrStatus },
    /// 设备不支持持久预留（ILLEGAL REQUEST）。不算"无法确认"，由上层按设备类型决定。
    Unsupported,
    /// 回读与预期不符。不得据此接管。
    Unconfirmed {
        reason: String,
        status: Option<PrStatus>,
    },
    /// 持有者是本系统的键且轮次比本轮高：本轮已被取代，什么也没改。
    /// 轮次来自 Raft，单调递增，所以迟到的旧一轮隔离不会把预留从新一轮手里抢走。
    Superseded { holder: ReservationKey },
}

fn is_unsupported(e: &TapeError) -> bool {
    matches!(
        e,
        TapeError::ScsiCommand {
            sense_key: 0x05,
            asc: 0x20 | 0x24,
            ..
        }
    )
}

fn newer_round_holder(st: &PrStatus, key: ReservationKey) -> Option<ReservationKey> {
    let (h, _) = st.holder?;
    let h = ReservationKey(h);
    (h.is_ours() && h.round() > key.round()).then_some(h)
}

/// 接管时对单个设备执行：读现状 → 注册本轮键 → 抢占或预留 → 回读确认。
/// 命令本身失败（传输错误、超时）以 `Err` 返回，调用方按"无法确认"处理。
pub fn fence(dev: &dyn TapeTransport, key: ReservationKey) -> Result<FenceOutcome> {
    let before = match read_status(dev) {
        Ok(s) => s,
        Err(e) if is_unsupported(&e) => return Ok(FenceOutcome::Unsupported),
        Err(e) => return Err(e),
    };
    info!(
        "{}: 预留现状 generation={} keys={:x?} holder={:x?}",
        dev.identity(),
        before.generation,
        before.keys,
        before.holder
    );

    if let Some(newer) = newer_round_holder(&before, key) {
        warn!(
            "{}: 持有者轮次 {} 高于本轮 {}，放弃隔离",
            dev.identity(),
            newer.round(),
            key.round()
        );
        return Ok(FenceOutcome::Superseded { holder: newer });
    }

    pr_out(dev, SA_OUT_REGISTER_IGNORE, 0, 0, key.0)?;
    // 注册之后重新读：若持有者就是本发起方（同节点新一轮），预留已跟随新键，不需要也不能抢占自己。
    let mid = read_status(dev)?;
    if let Some(newer) = newer_round_holder(&mid, key) {
        // 两次读之间被更新的轮次抢先：撤回刚才的注册，不抢占
        pr_out(dev, SA_OUT_REGISTER_IGNORE, 0, 0, 0)?;
        return Ok(FenceOutcome::Superseded { holder: newer });
    }
    match mid.holder {
        Some((h, _)) if h != key.0 => {
            pr_out(
                dev,
                SA_OUT_PREEMPT_ABORT,
                PR_TYPE_EXCLUSIVE_ACCESS,
                key.0,
                h,
            )?;
        }
        Some((_, t)) if t != PR_TYPE_EXCLUSIVE_ACCESS => {
            pr_out(dev, SA_OUT_RELEASE, t, key.0, 0)?;
            pr_out(dev, SA_OUT_RESERVE, PR_TYPE_EXCLUSIVE_ACCESS, key.0, 0)?;
        }
        Some(_) => {}
        None => pr_out(dev, SA_OUT_RESERVE, PR_TYPE_EXCLUSIVE_ACCESS, key.0, 0)?,
    }
    // 其余陈旧注册逐个移除（PREEMPT 对非持有者的键只删除注册）。抢占持有者时同键的注册已一并移除。
    let held = mid.holder.map(|(h, _)| h);
    let mut stale: Vec<u64> = mid
        .keys
        .iter()
        .copied()
        .filter(|&k| k != key.0 && Some(k) != held)
        .collect();
    stale.dedup();
    for k in stale {
        pr_out(dev, SA_OUT_PREEMPT, PR_TYPE_EXCLUSIVE_ACCESS, key.0, k)?;
    }

    let after = read_status(dev)?;
    let reason = if after.keys != [key.0] {
        Some(format!("注册表不是只含本轮键: {:x?}", after.keys))
    } else if after.holder != Some((key.0, PR_TYPE_EXCLUSIVE_ACCESS)) {
        Some(format!("持有者不是本轮键: {:x?}", after.holder))
    } else if after.generation == before.generation {
        Some("generation 没有变化".to_string())
    } else {
        None
    };
    Ok(match reason {
        Some(reason) => {
            warn!("{}: 隔离未能确认: {}", dev.identity(), reason);
            FenceOutcome::Unconfirmed {
                reason,
                status: Some(after),
            }
        }
        None => FenceOutcome::Fenced { before, after },
    })
}

/// 持有者自检：设备报告的持有者是不是 `key`。用于每次卷提交的屏障之前。
pub fn verify_holder(dev: &dyn TapeTransport, key: ReservationKey) -> Result<()> {
    verify_reported_holder(dev, key, read_status(dev)?)
}

/// For a bracketed read, Unit Attention invalidates the previous observation
/// even if the current PR holder would still match after a retry.
#[cfg(any(feature = "fdb", test))]
pub(crate) fn verify_holder_without_ua_retry(
    dev: &dyn TapeTransport,
    key: ReservationKey,
) -> Result<()> {
    verify_reported_holder(
        dev,
        key,
        read_status_with(|action| pr_in_once(dev, action))?,
    )
}

fn verify_reported_holder(
    dev: &dyn TapeTransport,
    key: ReservationKey,
    st: PrStatus,
) -> Result<()> {
    if st.holder == Some((key.0, PR_TYPE_EXCLUSIVE_ACCESS)) && st.keys.contains(&key.0) {
        return Ok(());
    }
    Err(TapeError::OwnershipLost {
        device: dev.identity().to_string(),
        detail: format!(
            "预留持有者 {:x?}，注册表 {:x?}，本轮键 {:x}",
            st.holder, st.keys, key.0
        ),
    })
}

/// 计划移交：释放预留并注销本轮键。不使用 CLEAR，以免清掉别人的注册。
pub fn release_and_unregister(dev: &dyn TapeTransport, key: ReservationKey) -> Result<()> {
    pr_out(dev, SA_OUT_RELEASE, PR_TYPE_EXCLUSIVE_ACCESS, key.0, 0)?;
    // 同一发起方可能已注册后继轮次。让设备按原键注销，保留后继注册。
    pr_out(dev, SA_OUT_REGISTER, 0, key.0, 0)
}

/// 交还和加载须拒绝截断的注册表。普通 HA 接管继续使用原读回策略。
#[cfg(any(feature = "fdb", test))]
pub(crate) fn read_handoff_status(dev: &dyn TapeTransport) -> Result<PrStatus> {
    let keys = pr_in(dev, SA_IN_READ_KEYS)?;
    let reservation = pr_in(dev, SA_IN_READ_RESERVATION)?;
    for (raw, descriptor) in [
        (&keys, SA_IN_READ_KEYS),
        (&reservation, SA_IN_READ_RESERVATION),
    ] {
        let header = raw.get(..8).ok_or(TapeError::InvalidResponse {
            expected: 8,
            actual: raw.len(),
        })?;
        let length = u32::from_be_bytes(header[4..8].try_into().expect("PR 响应头"));
        if if descriptor == SA_IN_READ_KEYS {
            length % 8 != 0
        } else {
            !matches!(length, 0 | 16)
        } {
            return Err(TapeError::NotReady("PR 描述符长度不正确".into()));
        }
        if raw.len() - 8 < length as usize {
            return Err(TapeError::InvalidResponse {
                expected: 8 + length as usize,
                actual: raw.len(),
            });
        }
    }
    if keys[..4] != reservation[..4] {
        return Err(TapeError::NotReady("PR 注册表在读取期间变化".into()));
    }
    let status = read_status_with(|action| {
        Ok(if action == SA_IN_READ_KEYS {
            keys.clone()
        } else {
            reservation.clone()
        })
    })?;
    if status
        .holder
        .is_some_and(|(key, _)| key != 0 && !status.keys.contains(&key))
    {
        return Err(TapeError::NotReady(
            "PR 持有者和注册表在读取期间不一致".into(),
        ));
    }
    Ok(status)
}

/// 外部应用交回后，使用设备约束的 REGISTER 和 RESERVE 获取预留。
#[cfg(any(feature = "fdb", test))]
pub(crate) fn reserve_for_reload(dev: &dyn TapeTransport, key: ReservationKey) -> Result<()> {
    let before = read_handoff_status(dev)?;
    if before.holder.is_some() || !before.keys.is_empty() {
        return Err(TapeError::NotReady(format!(
            "{} 仍有设备预留或注册",
            dev.identity()
        )));
    }
    pr_out(dev, SA_OUT_REGISTER, 0, 0, key.0)?;
    pr_out(dev, SA_OUT_RESERVE, PR_TYPE_EXCLUSIVE_ACCESS, key.0, 0)?;
    let after = read_handoff_status(dev)?;
    if after.keys != [key.0] || after.holder != Some((key.0, PR_TYPE_EXCLUSIVE_ACCESS)) {
        return Err(TapeError::NotReady(format!(
            "{} 重新加载预留未确认",
            dev.identity()
        )));
    }
    Ok(())
}

#[cfg(feature = "fdb")]
pub(crate) fn clear_reload_registration(
    dev: &dyn TapeTransport,
    key: ReservationKey,
) -> Result<()> {
    let before = read_handoff_status(dev)?;
    if before.keys.contains(&key.0) {
        if before.holder == Some((key.0, PR_TYPE_EXCLUSIVE_ACCESS)) {
            release_and_unregister(dev, key)?;
        } else {
            pr_out(dev, SA_OUT_REGISTER, 0, key.0, 0)?;
        }
        let after = read_handoff_status(dev)?;
        if after.keys.contains(&key.0) || after.holder.is_some_and(|(owner, _)| owner == key.0) {
            return Err(TapeError::NotReady(format!(
                "{} 本轮注册仍未清除",
                dev.identity()
            )));
        }
    }
    Ok(())
}

/// 这个错误是否意味着"已失去执行资格或无法再证明它"。为真时调用方必须停止，
/// 不得重试，也不得重新注册或抢占。
pub fn ownership_lost(e: &TapeError) -> bool {
    match e {
        TapeError::CommitFailed { source, .. } => ownership_lost(source),
        TapeError::ReservationConflict { .. } | TapeError::OwnershipLost { .. } => true,
        // 2A/03 注册被抢占，2A/04 预留被释放，2A/05 预留被抢占；29/xx 上电或复位（预留可能已丢）
        TapeError::ScsiCommand {
            sense_key: 0x06,
            asc: 0x2A,
            ascq: 0x03..=0x05,
            ..
        } => true,
        TapeError::ScsiCommand {
            sense_key: 0x06,
            asc: 0x29,
            ..
        } => true,
        _ => false,
    }
}

#[cfg(test)]
mod bracketed_read_tests {
    use super::*;
    use crate::scsi::sim::{Fault, FaultAction, SimLibrary};

    #[test]
    fn reload_loses_a_reserve_race_without_preempting() {
        use crate::scsi::device::ScsiResult;

        struct ForeignReservation<'a> {
            local: &'a dyn TapeTransport,
            foreign: &'a dyn TapeTransport,
            key: ReservationKey,
        }

        impl TapeTransport for ForeignReservation<'_> {
            fn identity(&self) -> &str {
                self.local.identity()
            }

            fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
                self.local.execute_no_data(cdb, timeout)
            }

            fn execute_read(
                &self,
                cdb: &[u8],
                data: &mut [u8],
                timeout: u32,
            ) -> Result<ScsiResult> {
                self.local.execute_read(cdb, data, timeout)
            }

            fn execute_write(&self, cdb: &[u8], data: &[u8], timeout: u32) -> Result<ScsiResult> {
                let result = self.local.execute_write(cdb, data, timeout)?;
                if cdb[0] == crate::scsi::cdb::opcode::PERSISTENT_RESERVE_OUT
                    && cdb[1] & 0x1f == SA_OUT_REGISTER
                {
                    pr_out(self.foreign, SA_OUT_REGISTER, 0, 0, self.key.0)?;
                    pr_out(
                        self.foreign,
                        SA_OUT_RESERVE,
                        PR_TYPE_EXCLUSIVE_ACCESS,
                        self.key.0,
                        0,
                    )?;
                }
                Ok(result)
            }
        }

        let lib = SimLibrary::new(1, 1, 1);
        let local = lib.drive_as(0, 1);
        let foreign = lib.drive_as(0, 2);
        let key = ReservationKey(0x2300_0000_0000_0001);
        let racing = ForeignReservation {
            local: &local,
            foreign: &foreign,
            key,
        };
        assert!(matches!(
            reserve_for_reload(&racing, ReservationKey::new(1, 2)),
            Err(TapeError::ReservationConflict { .. })
        ));
        verify_holder(&foreign, key).unwrap();
    }

    #[test]
    fn holder_check_without_ua_retry_preserves_live_reservation() {
        let library = SimLibrary::new(1, 1, 1);
        let drive = library.drive(0);
        let key = ReservationKey::new(1, 7);
        assert!(matches!(
            fence(&drive, key).unwrap(),
            FenceOutcome::Fenced { .. }
        ));
        let before = drive.command_log().len();
        drive.inject(Fault {
            opcode: cdb::opcode::PERSISTENT_RESERVE_IN,
            action: FaultAction::CheckCondition {
                sense_key: 6,
                asc: 0x28,
                ascq: 0,
            },
            after: 0,
            remaining: 1,
        });
        assert!(matches!(
            verify_holder_without_ua_retry(&drive, key),
            Err(TapeError::ScsiCommand {
                sense_key: 6,
                asc: 0x28,
                ascq: 0,
                ..
            })
        ));
        assert_eq!(
            drive.command_log().len(),
            before + 1,
            "UA must not retry PR IN"
        );
        verify_holder_without_ua_retry(&drive, key).unwrap();
        assert!(matches!(
            verify_holder_without_ua_retry(&drive, ReservationKey::new(1, 8)),
            Err(TapeError::OwnershipLost { .. }),
        ));
    }
}
