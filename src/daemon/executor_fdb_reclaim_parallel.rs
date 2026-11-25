//! Reclaim reads on the other drive while the owner uses its current drive.
use super::*;
use crate::scsi::{device::ScsiResult, transport::TapeTransport};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{SyncSender, sync_channel},
};
use std::thread::{Builder, JoinHandle};

type Drive = Box<dyn TapeTransport + Send>;
type Outcome<T> = std::result::Result<T, ReadError>;

pub(super) struct StartReadWorker(SyncSender<bool>);

impl StartReadWorker {
    pub(super) fn start(self) {
        let _ = self.0.try_send(true);
    }
}

pub(super) struct DriveWorker<'a, T> {
    owner: &'a mut Active,
    slot: usize,
    drive: Arc<Mutex<Drive>>,
    stop: Arc<AtomicBool>,
    start: SyncSender<bool>,
    thread: Option<JoinHandle<Outcome<T>>>,
}

impl<'a, T> DriveWorker<'a, T> {
    pub(super) fn owner(&mut self) -> &mut Active {
        self.owner
    }

    fn reclaim_drive(&mut self) -> Outcome<T> {
        let _ = self.start.try_send(false);
        let result = self.thread.take().expect("one drive worker").join();
        let mut drive = self.drive.lock().unwrap_or_else(|error| error.into_inner());
        std::mem::swap(&mut *drive, &mut self.owner.devices[self.slot].dev);
        result.map_err(|_| lost("回收只读驱动任务异常退出"))?
    }

    pub(super) fn finish(mut self) -> Outcome<T> {
        self.reclaim_drive()
    }
}

impl<'a, T: Send + 'static> DriveWorker<'a, T> {
    pub(super) fn start(
        owner: &'a mut Active,
        slot: usize,
        read: impl FnOnce(&dyn TapeTransport) -> Outcome<T> + Send + 'static,
    ) -> Outcome<Self> {
        let (worker, start) = Self::start_paused(owner, slot, read)?;
        start.start();
        Ok(worker)
    }

    pub(super) fn start_paused(
        owner: &'a mut Active,
        slot: usize,
        read: impl FnOnce(&dyn TapeTransport) -> Outcome<T> + Send + 'static,
    ) -> Outcome<(Self, StartReadWorker)> {
        if owner.devices[slot].kind != DeviceKind::Drive {
            return Err(lost("回收并行只读任务必须使用磁带驱动器"));
        }
        let drive = Arc::new(Mutex::new(std::mem::replace(
            &mut owner.devices[slot].dev,
            Box::new(ClosedTransport),
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let owned_drive = drive.clone();
        let stopped = stop.clone();
        let (start, ready) = sync_channel(1);
        let thread = Builder::new().name("reclaim-read".into()).spawn(move || {
            if ready.recv() != Ok(true) || stopped.load(Ordering::Acquire) {
                return Err(lost("回收只读驱动任务已停止"));
            }
            let drive = owned_drive
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            read(&ReadOnlyDrive {
                inner: drive.as_ref(),
                stop: &stopped,
            })
        });
        match thread {
            Ok(thread) => Ok((
                Self {
                    owner,
                    slot,
                    drive,
                    stop,
                    start: start.clone(),
                    thread: Some(thread),
                },
                StartReadWorker(start),
            )),
            Err(error) => {
                let mut drive = drive.lock().unwrap_or_else(|error| error.into_inner());
                std::mem::swap(&mut *drive, &mut owner.devices[slot].dev);
                Err(lost(format!("回收只读工作线程启动失败: {error}")))
            }
        }
    }
}

impl<T> Drop for DriveWorker<'_, T> {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop.store(true, Ordering::Release);
            let _ = self.reclaim_drive();
        }
    }
}

struct ReadOnlyDrive<'a> {
    inner: &'a dyn TapeTransport,
    stop: &'a AtomicBool,
}

impl ReadOnlyDrive<'_> {
    fn call(&self, command: impl FnOnce() -> Result<ScsiResult>) -> Result<ScsiResult> {
        let check = || {
            if self.stop.load(Ordering::Acquire) {
                return Err(TapeError::OwnershipLost {
                    device: self.inner.identity().into(),
                    detail: "回收只读驱动任务已停止".into(),
                });
            }
            Ok(())
        };
        check()?;
        let result = command()?;
        check()?;
        Ok(result)
    }
}

impl TapeTransport for ReadOnlyDrive<'_> {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        if !matches!(cdb.first(), Some(0x00 | 0x01 | 0x11 | 0x2b | 0x92)) {
            return Err(TapeError::Refused {
                reason: "回收只读驱动任务仅允许读取和定位".into(),
            });
        }
        self.call(|| self.inner.execute_no_data(cdb, timeout))
    }

    fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        if !matches!(
            cdb.first(),
            Some(0x05 | 0x08 | 0x12 | 0x1a | 0x34 | 0x4d | 0x5a | 0x5e | 0x8c | 0xa3)
        ) {
            return Err(TapeError::Refused {
                reason: "回收只读驱动任务拒绝非读取 CDB".into(),
            });
        }
        self.call(|| self.inner.execute_read(cdb, data, timeout))
    }

    fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
        Err(TapeError::Refused {
            reason: "回收只读驱动任务禁止写入".into(),
        })
    }

    fn identity(&self) -> &str {
        self.inner.identity()
    }

    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.inner.medium_change_token()
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_read_worker_refuses_commands_and_always_rejects_writes() {
        let stop = AtomicBool::new(false);
        let drive = ReadOnlyDrive {
            inner: &ClosedTransport,
            stop: &stop,
        };
        assert!(matches!(
            drive.execute_no_data(&[0x1b], 0),
            Err(TapeError::Refused { .. })
        ));
        assert!(matches!(
            drive.execute_write(&[0x8d], &[], 0),
            Err(TapeError::Refused { .. })
        ));
        assert!(matches!(
            drive.execute_read(&[0x08], &mut [], 0),
            Err(TapeError::NotReady(_))
        ));
        stop.store(true, Ordering::Release);
        assert!(matches!(
            drive.execute_no_data(&[0x92], 0),
            Err(TapeError::OwnershipLost { .. })
        ));
        assert!(matches!(
            drive.execute_read(&[0x08], &mut [], 0),
            Err(TapeError::OwnershipLost { .. })
        ));
    }
}
