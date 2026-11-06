use crate::error::{Result, TapeError};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::scsi::device::ScsiResult;
use crate::scsi::transport::TapeTransport;
use std::sync::Mutex;

/// 每条 CDB 前后核实原任务；一次失权即锁定此实例，不能因依赖恢复自行续写。
pub(super) struct Guard<'a, F> {
    inner: &'a dyn TapeTransport,
    authority: F,
    lost: Mutex<Option<String>>,
}

impl<'a, F: Fn() -> Result<()>> Guard<'a, F> {
    pub(super) fn new(inner: &'a dyn TapeTransport, authority: F) -> Self {
        Self {
            inner,
            authority,
            lost: Mutex::new(None),
        }
    }

    fn check(&self) -> Result<()> {
        let mut lost = self.lost.lock().map_err(|_| TapeError::OwnershipLost {
            device: self.inner.identity().into(),
            detail: "格式化命令权限状态损坏".into(),
        })?;
        if let Some(detail) = lost.as_ref() {
            return Err(TapeError::OwnershipLost {
                device: self.inner.identity().into(),
                detail: detail.clone(),
            });
        }
        if let Err(error) = (self.authority)() {
            let detail = format!("原格式化任务执行权不可用: {error}");
            *lost = Some(detail.clone());
            return Err(TapeError::OwnershipLost {
                device: self.inner.identity().into(),
                detail,
            });
        }
        Ok(())
    }

    fn invoke(&self, operation: impl FnOnce() -> Result<ScsiResult>) -> Result<ScsiResult> {
        self.check()?;
        let result = operation();
        let checked = self.check();
        // 保留设备本身返回的诊断；权限失效仍已锁定，后续重试不能发出 CDB。
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                checked?;
                Ok(value)
            }
        }
    }
}

impl<F: Fn() -> Result<()>> TapeTransport for Guard<'_, F> {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(|| self.inner.execute_no_data(cdb, timeout))
    }
    fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(|| self.inner.execute_read(cdb, data, timeout))
    }
    fn execute_write(&self, cdb: &[u8], data: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(|| self.inner.execute_write(cdb, data, timeout))
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}

pub(super) fn format(
    device: &dyn TapeTransport,
    options: &MkltfsOptions,
    authority: impl Fn() -> Result<()>,
) -> Result<uuid::Uuid> {
    mkltfs(&Guard::new(device, authority), options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TapeError;
    use crate::scsi::device::ScsiResult;
    use crate::scsi::sim::{SimCartridge, SimLibrary, SimTransport};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct DisconnectAfterFormat {
        inner: SimTransport,
        disconnected: AtomicBool,
        formats: AtomicUsize,
        later_commands: AtomicUsize,
    }

    impl DisconnectAfterFormat {
        fn count(&self) {
            if self.disconnected.load(Ordering::SeqCst) {
                self.later_commands.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl TapeTransport for DisconnectAfterFormat {
        fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
            self.count();
            let result = self.inner.execute_no_data(cdb, timeout);
            if cdb.first() == Some(&0x04) && result.is_ok() {
                self.formats.fetch_add(1, Ordering::SeqCst);
                self.disconnected.store(true, Ordering::SeqCst);
            }
            result
        }
        fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
            self.count();
            self.inner.execute_read(cdb, data, timeout)
        }
        fn execute_write(&self, cdb: &[u8], data: &[u8], timeout: u32) -> Result<ScsiResult> {
            self.count();
            self.inner.execute_write(cdb, data, timeout)
        }
        fn identity(&self) -> &str {
            self.inner.identity()
        }
    }

    #[test]
    fn fdb_loss_after_format_prevents_every_following_cdb() {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("GUARD0L8", 64 << 20), 0)
            .unwrap();
        lib.load_into_drive("GUARD0L8", 0).unwrap();
        let device = DisconnectAfterFormat {
            inner: lib.drive(0),
            disconnected: AtomicBool::new(false),
            formats: AtomicUsize::new(0),
            later_commands: AtomicUsize::new(0),
        };
        let result = format(&device, &MkltfsOptions::default(), || {
            if device.disconnected.load(Ordering::SeqCst) {
                Err(TapeError::OwnershipLost {
                    device: device.identity().into(),
                    detail: "注入 FDB 不可达".into(),
                })
            } else {
                Ok(())
            }
        });
        assert!(result.is_err(), "FDB 失联后不能继续完成格式化");
        assert_eq!(device.formats.load(Ordering::SeqCst), 1);
        assert_eq!(device.later_commands.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn authority_recovery_does_not_reactivate_the_same_guard() {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("LATCH0L8", 64 << 20), 0)
            .unwrap();
        lib.load_into_drive("LATCH0L8", 0).unwrap();
        let device = DisconnectAfterFormat {
            inner: lib.drive(0),
            disconnected: AtomicBool::new(true),
            formats: AtomicUsize::new(0),
            later_commands: AtomicUsize::new(0),
        };
        let guard = Guard::new(&device, || {
            if device.disconnected.load(Ordering::SeqCst) {
                Err(std::io::Error::other("FDB 不可达").into())
            } else {
                Ok(())
            }
        });
        assert!(guard.execute_no_data(&[0; 6], 100).is_err());
        device.disconnected.store(false, Ordering::SeqCst);
        assert!(guard.execute_no_data(&[0; 6], 100).is_err());
        assert_eq!(device.later_commands.load(Ordering::SeqCst), 0);
        assert_eq!(device.formats.load(Ordering::SeqCst), 0);
    }
}
