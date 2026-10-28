//! 本地执行许可。截止时间从发起多数派确认时计算，不从迟到的回复计算。
//! 关闭或过期的轮次不可复活；新轮次必须重新隔离设备。

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use crate::error::{Result, TapeError};
use crate::scsi::device::ScsiResult;
use crate::scsi::transport::TapeTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Fencing,
    Execution,
    Serving,
}

#[derive(Debug, Default)]
pub struct Permission {
    state: Mutex<Option<Grant>>,
}

#[derive(Debug)]
struct Grant {
    round: u64,
    phase: Phase,
    deadline: Instant,
    closed: bool,
}

impl Permission {
    pub fn renew(&self, round: u64, phase: Phase, deadline: Instant) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if round == 0 || deadline <= now {
            return false;
        }
        if let Some(old) = state.as_ref() {
            if round < old.round
                || (round == old.round && (old.closed || old.deadline <= now || phase < old.phase))
            {
                return false;
            }
            if round == old.round && deadline <= old.deadline && phase == old.phase {
                return true;
            }
        }
        *state = Some(Grant {
            round,
            phase,
            deadline,
            closed: false,
        });
        true
    }

    pub fn allows(&self, round: u64, phase: Phase) -> bool {
        self.deadline(round, phase).is_some()
    }

    /// 返回仍有效的截止时间，供内核 VIP 地址寿命使用；不得据此延长许可。
    pub fn deadline(&self, round: u64, phase: Phase) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|g| {
                g.round == round && !g.closed && g.phase == phase && g.deadline > Instant::now()
            })
            .map(|g| g.deadline)
    }

    pub fn close(&self, round: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.as_ref().is_none_or(|g| g.round < round) {
            *state = Some(Grant {
                round,
                phase: Phase::Fencing,
                deadline: Instant::now(),
                closed: true,
            });
        } else if let Some(g) = state.as_mut().filter(|g| g.round == round) {
            g.closed = true;
        }
    }
}

/// 在每条 CDB 提交前、返回后核验许可。已在内核执行的命令不能由时钟撤销；
/// 新 owner 仍必须执行设备级 PREEMPT AND ABORT 和回读，不能把许可超时当作隔离成功。
pub struct GuardedTransport {
    inner: Box<dyn TapeTransport + Send>,
    permission: Arc<Permission>,
    round: u64,
}

impl GuardedTransport {
    pub fn new(
        inner: Box<dyn TapeTransport + Send>,
        permission: Arc<Permission>,
        round: u64,
    ) -> Self {
        Self {
            inner,
            permission,
            round,
        }
    }

    fn check(&self, cdb: &[u8]) -> Result<()> {
        let pr = matches!(cdb.first(), Some(0x5e | 0x5f | 0x12));
        // Execution 阶段只能释放/注销自己的 PR，不可重新注册或抢回。
        let changes_pr = cdb.first() == Some(&0x5f)
            && !matches!(cdb.get(1).map(|v| v & 0x1f), Some(0x00 | 0x02 | 0x06));
        if (pr && self.permission.allows(self.round, Phase::Fencing))
            || (!changes_pr
                && (self.permission.allows(self.round, Phase::Execution)
                    || self.permission.allows(self.round, Phase::Serving)))
        {
            Ok(())
        } else {
            Err(TapeError::OwnershipLost {
                device: self.identity().to_string(),
                detail: format!("轮次 {} 的多数派执行许可已失效或阶段不符", self.round),
            })
        }
    }
}

impl TapeTransport for GuardedTransport {
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        let result = self.inner.execute_no_data(cdb, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        let result = self.inner.execute_read(cdb, buf, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        if cdb.first() == Some(&0x5f)
            && matches!(cdb.get(1).map(|v| v & 0x1f), Some(0x00 | 0x06))
            && !self.permission.allows(self.round, Phase::Fencing)
            && buf.get(8..16) != Some(&[0u8; 8][..])
        {
            return Err(TapeError::OwnershipLost {
                device: self.identity().into(),
                detail: "持有期间禁止重新注册 PR".into(),
            });
        }
        let result = self.inner.execute_write(cdb, buf, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        Some((self.round, Arc::as_ptr(&self.permission) as usize))
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}
