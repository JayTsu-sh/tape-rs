//! Shared simulator-only fixture; no device paths or hardware access.
use super::super::{node::NodeStatus, state::Executor};
use super::*;
use crate::scsi::{cdb::opcode, device::ScsiResult, sim::SimTransport};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct Counter {
    inner: Arc<SimTransport>,
    log: Arc<Mutex<Vec<u8>>>,
    revoke: Arc<AtomicBool>,
    permission: Arc<Permission>,
    round: u64,
}
impl TapeTransport for Counter {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.log.lock().unwrap().push(cdb[0]);
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.log.lock().unwrap().push(cdb[0]);
        let result = self.inner.execute_read(cdb, buf, timeout);
        if cdb[0] == opcode::READ_ELEMENT_STATUS && self.revoke.swap(false, Ordering::SeqCst) {
            self.permission.close(self.round);
        }
        result
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.log.lock().unwrap().push(cdb[0]);
        self.inner.execute_write(cdb, buf, timeout)
    }
}
pub(super) struct Provider {
    pub(super) serial: String,
    pub(super) changer: Arc<SimTransport>,
    pub(super) drive: Arc<SimTransport>,
    pub(super) log: Arc<Mutex<Vec<u8>>>,
    pub(super) revoke: Arc<AtomicBool>,
    pub(super) permission: Arc<Permission>,
    pub(super) round: u64,
}
impl DeviceProvider for Provider {
    fn open_all(&self) -> Result<Vec<ManagedDevice>> {
        let wrap = |inner: Arc<SimTransport>| {
            Box::new(Counter {
                inner,
                log: self.log.clone(),
                revoke: self.revoke.clone(),
                permission: self.permission.clone(),
                round: self.round,
            }) as Box<dyn TapeTransport + Send>
        };
        Ok(vec![
            ManagedDevice {
                name: "changer".into(),
                serial: self.serial.clone(),
                kind: DeviceKind::Changer,
                dev: wrap(self.changer.clone()),
            },
            ManagedDevice {
                name: "drive".into(),
                serial: "SIMDRV0000".into(),
                kind: DeviceKind::Drive,
                dev: wrap(self.drive.clone()),
            },
        ])
    }
}
pub(super) fn options(node: u8) -> ExecOptions {
    ExecOptions {
        node_id: node,
        interval: Duration::from_secs(1),
        demo_write: false,
        salvage: false,
        block_size: 64 * 1024,
        read_idle: Duration::from_secs(30),
    }
}
pub(super) fn make_status(node: u8, round: u64, frozen: u64) -> SharedStatus {
    Arc::new(Mutex::new(NodeStatus {
        id: u64::from(node),
        catalog_frozen_index: Some(frozen),
        executor: Some(Executor {
            node: u64::from(node),
            round,
            term: 7,
            fenced: true,
        }),
        executor_ready: true,
        ..Default::default()
    }))
}
