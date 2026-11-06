//! Bound cancellation polling to the blocking, read-only preformat stage.
//! The FORMAT path deliberately uses its existing durable write checkpoints.
use crate::error::{Result, TapeError};
use crate::managed_catalog::CatalogError;
use crate::scsi::device::ScsiResult;
use crate::scsi::transport::TapeTransport;
use std::sync::Mutex;
use std::time::{Duration, Instant};

enum State {
    Active(Option<Instant>),
    Cancelled,
    Lost(String),
}

pub(super) struct PreformatRead<'a, F> {
    inner: &'a dyn TapeTransport,
    probe: F,
    state: Mutex<State>,
    interval: Duration,
}

impl<'a, F: Fn() -> std::result::Result<bool, CatalogError>> PreformatRead<'a, F> {
    pub(super) fn new(inner: &'a dyn TapeTransport, probe: F) -> Self {
        Self {
            inner,
            probe,
            state: Mutex::new(State::Active(None)),
            interval: Duration::from_millis(500),
        }
    }

    fn lost(&self, detail: String) -> TapeError {
        TapeError::OwnershipLost {
            device: self.identity().into(),
            detail,
        }
    }

    fn cancelled() -> TapeError {
        // Interrupted would let std::io readers silently retry the same read.
        std::io::Error::other("原维护任务已取消，停止只读预检查").into()
    }

    fn check(&self, cdb: &[u8]) -> Result<()> {
        if matches!(cdb.first(), Some(0x04 | 0x0a | 0x10 | 0x19 | 0x8d)) {
            return Err(TapeError::Refused {
                reason: "只读预检查不能发送写介质命令".into(),
            });
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| self.lost("原维护任务的取消检查状态损坏".into()))?;
        match &*state {
            State::Cancelled => return Err(Self::cancelled()),
            State::Lost(detail) => return Err(self.lost(detail.clone())),
            State::Active(Some(last)) if last.elapsed() < self.interval => return Ok(()),
            State::Active(_) => {}
        }
        let started = Instant::now();
        match (self.probe)() {
            Ok(false) => {
                *state = State::Active(Some(started));
                Ok(())
            }
            Ok(true) => {
                *state = State::Cancelled;
                Err(Self::cancelled())
            }
            Err(error) => {
                let detail = format!("原维护任务的 FDB 执行资格不可用: {error}");
                *state = State::Lost(detail.clone());
                Err(self.lost(detail))
            }
        }
    }
}

impl<F: Fn() -> std::result::Result<bool, CatalogError>> TapeTransport for PreformatRead<'_, F> {
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        let result = self.inner.execute_no_data(cdb, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        let result = self.inner.execute_read(cdb, data, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn execute_write(&self, cdb: &[u8], data: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.check(cdb)?;
        let result = self.inner.execute_write(cdb, data, timeout_ms)?;
        self.check(cdb)?;
        Ok(result)
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::sense::SenseInfo;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Fake {
        calls: AtomicUsize,
        cancelled: AtomicBool,
        cancel_during_io: bool,
        fail: bool,
    }
    impl Fake {
        fn result(&self) -> Result<ScsiResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.cancel_during_io {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            if self.fail {
                return Err(TapeError::ScsiCommand {
                    status: 2,
                    sense_key: 3,
                    asc: 0x11,
                    ascq: 0,
                });
            }
            Ok(ScsiResult {
                status: 0,
                sense: SenseInfo::from_bytes(&[]),
                transferred: 0,
                duration_ms: 0,
            })
        }
    }
    impl TapeTransport for Fake {
        fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
            self.result()
        }
        fn execute_read(&self, _: &[u8], _: &mut [u8], _: u32) -> Result<ScsiResult> {
            self.result()
        }
        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
            self.result()
        }
        fn identity(&self) -> &str {
            "cancel-test"
        }
        fn session_epoch(&self) -> Option<u64> {
            Some(42)
        }
        fn execution_guard(&self) -> Option<(u64, usize)> {
            Some((7, 9))
        }
    }

    #[test]
    fn cancellation_before_io_never_reaches_the_device() {
        let fake = Fake::default();
        fake.cancelled.store(true, Ordering::SeqCst);
        let guard = PreformatRead::new(&fake, || Ok(fake.cancelled.load(Ordering::SeqCst)));
        assert!(guard.execute_no_data(&[0x92], 100).is_err());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancellation_after_io_is_latched_and_not_an_interrupted_retry() {
        let fake = Fake {
            cancel_during_io: true,
            ..Fake::default()
        };
        let mut guard = PreformatRead::new(&fake, || Ok(fake.cancelled.load(Ordering::SeqCst)));
        guard.interval = Duration::ZERO;
        assert!(matches!(guard.execute_read(&[0x08], &mut [0;8], 100),
            Err(TapeError::Io(error)) if error.kind()==std::io::ErrorKind::Other));
        fake.cancelled.store(false, Ordering::SeqCst);
        assert!(guard.execute_no_data(&[0x92], 100).is_err());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn media_writes_are_refused_but_drive_configuration_and_guard_identity_survive() {
        let fake = Fake::default();
        let guard = PreformatRead::new(&fake, || Ok(false));
        for opcode in [0x04, 0x0a, 0x10, 0x19, 0x8d] {
            assert!(guard.execute_write(&[opcode], &[], 100).is_err());
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        guard.execute_write(&[0x55], &[], 100).unwrap();
        assert_eq!(guard.session_epoch(), Some(42));
        assert_eq!(guard.execution_guard(), Some((7, 9)));
        assert_eq!(guard.identity(), fake.identity());
    }

    #[test]
    fn authority_loss_is_latched_without_more_device_commands() {
        let fake = Fake::default();
        let revoked = AtomicBool::new(true);
        let guard = PreformatRead::new(&fake, || {
            if revoked.load(Ordering::SeqCst) {
                Err(CatalogError::OwnerFenced)
            } else {
                Ok(false)
            }
        });
        assert!(matches!(
            guard.execute_no_data(&[0x92], 100),
            Err(TapeError::OwnershipLost { .. })
        ));
        revoked.store(false, Ordering::SeqCst);
        assert!(guard.execute_no_data(&[0x92], 100).is_err());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn physical_scsi_failure_keeps_its_original_diagnostics() {
        let fake = Fake {
            cancel_during_io: true,
            fail: true,
            ..Fake::default()
        };
        let guard = PreformatRead::new(&fake, || Ok(false));
        assert!(matches!(
            guard.execute_read(&[0x08], &mut [0; 8], 100),
            Err(TapeError::ScsiCommand {
                status: 2,
                sense_key: 3,
                asc: 0x11,
                ascq: 0
            })
        ));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }
}
