use crate::error::{Result, TapeError};
#[cfg(test)]
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::scsi::device::ScsiResult;
use crate::scsi::transport::TapeTransport;
use std::sync::Mutex;

/// Only a complete current CHECK CONDITION is a confirmed device rejection.
/// Raw host/driver failures and deferred or truncated sense keep the result unknown.
pub(super) fn confirmed_device_failure(error: &TapeError) -> bool {
    matches!(error,TapeError::ScsiCommand{status:2,sense_key,asc,ascq,sense}
        if !matches!(sense_key,2|6|0x0b) && sense.as_deref().and_then(|raw|raw.current_condition())==Some((*sense_key,*asc,*ascq)))
}

fn confirmed_response(cdb: &[u8], error: &TapeError) -> bool {
    if crate::scsi::reservation::ownership_lost(error) {
        return false;
    }
    if confirmed_device_failure(error) {
        return true;
    }
    let TapeError::ScsiCommand {
        status: 2,
        sense_key,
        asc,
        ascq,
        sense,
    } = error
    else {
        return false;
    };
    if sense.as_deref().and_then(|raw| raw.current_condition()) != Some((*sense_key, *asc, *ascq)) {
        return false;
    }
    // TUR reports an actual not-ready state. Existing, bounded UA retries
    // remain valid only for position-independent commands; reset/preemption,
    // incomplete sense and sequential data commands never get this exception.
    (*sense_key == 2 && cdb.first() == Some(&0x00))
        || (*sense_key == 6
            && matches!(
                cdb.first(),
                Some(
                    0x00 | 0x01
                        | 0x04
                        | 0x07
                        | 0x12
                        | 0x15
                        | 0x1a
                        | 0x1b
                        | 0x2b
                        | 0x34
                        | 0x44
                        | 0x55
                        | 0x5a
                        | 0x8c
                        | 0x8d
                        | 0x92
                        | 0xa5
                        | 0xb8
                )
            ))
}

/// 每条 CDB 前后核实原任务；一次失权即锁定此实例，不能因依赖恢复自行续写。
pub(in crate::daemon) struct Guard<'a, F> {
    inner: &'a dyn TapeTransport,
    authority: F,
    lost: Mutex<Option<String>>,
}

impl<'a, F: Fn() -> Result<()>> Guard<'a, F> {
    pub(in crate::daemon) fn new(inner: &'a dyn TapeTransport, authority: F) -> Self {
        Self {
            inner,
            authority,
            lost: Mutex::new(None),
        }
    }

    fn check(&self) -> Result<()> {
        let mut lost = self.lost.lock().map_err(|_| TapeError::OwnershipLost {
            device: self.inner.identity().into(),
            detail: "设备命令权限状态损坏".into(),
        })?;
        if let Some(detail) = lost.as_ref() {
            return Err(TapeError::OwnershipLost {
                device: self.inner.identity().into(),
                detail: detail.clone(),
            });
        }
        if let Err(error) = (self.authority)() {
            let detail = format!("原设备任务执行权不可用: {error}");
            *lost = Some(detail.clone());
            return Err(TapeError::OwnershipLost {
                device: self.inner.identity().into(),
                detail,
            });
        }
        Ok(())
    }

    /// Check the latched result after a caller collected or wrapped an error.
    /// A final unknown CDB must not disappear behind a successful helper result.
    pub(in crate::daemon) fn verify_proof(&self) -> Result<()> {
        self.check()
    }

    fn invoke(
        &self,
        cdb: &[u8],
        operation: impl FnOnce() -> Result<ScsiResult>,
    ) -> Result<ScsiResult> {
        self.check()?;
        let result = operation();
        if let Err(error) = &result
            && !confirmed_response(cdb, error)
            && let Ok(mut lost) = self.lost.lock()
        {
            *lost = Some(format!("原设备命令结果未确认: {error}"));
        }
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
        self.invoke(cdb, || self.inner.execute_no_data(cdb, timeout))
    }
    fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(cdb, || self.inner.execute_read(cdb, data, timeout))
    }
    fn execute_write(&self, cdb: &[u8], data: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(cdb, || self.inner.execute_write(cdb, data, timeout))
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

#[cfg(test)]
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

    #[test]
    fn readiness_and_safe_unit_attention_require_complete_current_sense() {
        struct Response {
            key: u8,
            asc: u8,
            ascq: u8,
            response: u8,
            length: usize,
        }
        impl TapeTransport for Response {
            fn identity(&self) -> &str {
                "response-fixture"
            }
            fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
                let mut raw = [0; 18];
                raw[0] = self.response;
                raw[2] = self.key;
                raw[7] = 10;
                raw[12] = self.asc;
                raw[13] = self.ascq;
                Err(TapeError::ScsiCommand {
                    status: 2,
                    sense_key: self.key,
                    asc: self.asc,
                    ascq: self.ascq,
                    sense: Some(Box::new(crate::scsi::sense::SenseInfo::from_bytes(
                        &raw[..self.length],
                    ))),
                })
            }
            fn execute_read(&self, cdb: &[u8], _: &mut [u8], timeout: u32) -> Result<ScsiResult> {
                self.execute_no_data(cdb, timeout)
            }
            fn execute_write(&self, cdb: &[u8], _: &[u8], timeout: u32) -> Result<ScsiResult> {
                self.execute_no_data(cdb, timeout)
            }
        }
        for (opcode, key, asc, ascq, response, length, known) in [
            (0x00, 2, 4, 1, 0x70, 18, true),
            (0x1b, 2, 4, 1, 0x70, 18, false),
            (0x1b, 6, 0x28, 0, 0x70, 18, true),
            (0xb8, 6, 0x28, 0, 0x70, 18, true),
            (0x8c, 6, 0x28, 0, 0x70, 18, true),
            (0x08, 6, 0x28, 0, 0x70, 18, false),
            (0x0a, 6, 0x28, 0, 0x70, 18, false),
            (0x10, 6, 0x28, 0, 0x70, 18, false),
            (0x11, 6, 0x28, 0, 0x70, 18, false),
            (0x19, 6, 0x28, 0, 0x70, 18, false),
            (0x00, 6, 0x29, 0, 0x70, 18, false),
            (0x00, 6, 0x2a, 3, 0x70, 18, false),
            (0x00, 2, 4, 1, 0x71, 18, false),
            (0x1b, 6, 0x28, 0, 0x71, 18, false),
            (0x00, 2, 4, 1, 0x70, 13, false),
            (0x1b, 6, 0x28, 0, 0x70, 17, false),
        ] {
            let device = Response {
                key,
                asc,
                ascq,
                response,
                length,
            };
            let checked = Guard::new(&device, || Ok(()));
            assert!(checked.execute_no_data(&[opcode], 100).is_err());
            assert_eq!(
                checked.verify_proof().is_ok(),
                known,
                "opcode={opcode:x}, key={key}, response={response:x}, length={length}"
            );
        }
    }

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
            let result = self.inner.execute_write(cdb, data, timeout);
            if cdb.first() == Some(&0x0a) && result.is_ok() {
                self.formats.fetch_add(1, Ordering::SeqCst);
                self.disconnected.store(true, Ordering::SeqCst);
            }
            result
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
    fn dependency_loss_after_first_data_write_stops_remaining_blocks_and_filemarks() {
        let lib = SimLibrary::new(1, 2, 1);
        lib.insert_cartridge(SimCartridge::blank("WRITE0L8", 64 << 20), 0)
            .unwrap();
        lib.load_into_drive("WRITE0L8", 0).unwrap();
        mkltfs(&lib.drive(0), &MkltfsOptions::default()).unwrap();
        let device = DisconnectAfterFormat {
            inner: lib.drive(0),
            disconnected: AtomicBool::new(false),
            formats: AtomicUsize::new(0),
            later_commands: AtomicUsize::new(0),
        };
        let guard = Guard::new(&device, || {
            if device.disconnected.load(Ordering::SeqCst) {
                Err(std::io::Error::other("注入首WRITE后FDB不可达").into())
            } else {
                Ok(())
            }
        });
        let mut volume = crate::ltfs::volume::LtfsVolume::mount(&guard).unwrap();
        let result = volume.append_file(
            "blocked.bin",
            &mut std::io::Cursor::new(vec![0x5a; 2 << 20]),
        );
        assert!(result.is_err());
        assert_eq!(device.formats.load(Ordering::SeqCst), 1);
        assert_eq!(
            device.later_commands.load(Ordering::SeqCst),
            0,
            "依赖失效后不能写后续块、标记或索引"
        );
        device.disconnected.store(false, Ordering::SeqCst);
        assert!(
            guard.execute_no_data(&[0x10, 0, 0, 0, 0, 0], 100).is_err(),
            "恢复依赖不能复活旧执行实例"
        );
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
