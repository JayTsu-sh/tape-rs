//! Exact-parent test-only wrapper markers; never an I/O or proof factory.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[derive(Clone, Copy)]
pub(in crate::daemon::executor) enum ReadbackSealMode {
    Clean,
    MonitorOwnerFenced,
    DestinationBodyReadFails,
    ParallelFormatPreparation,
}
struct Armed {
    parent: serde_json::Value,
    commands: Arc<Mutex<Vec<(usize, Vec<u8>)>>>,
    mode: ReadbackSealMode,
    sealed: Mutex<Option<(usize, Option<u64>)>>,
    returned: Mutex<Option<(usize, Option<u64>)>>,
    target_read: Mutex<Option<(usize, bool)>>,
    parallel_reads: (Mutex<u8>, Condvar),
    prepared_source_used: AtomicBool,
}
fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
    static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(in crate::daemon::executor) struct ReadbackSealMarker(Arc<Armed>);
impl ReadbackSealMarker {
    pub(in crate::daemon::executor) fn sealed(&self) -> Option<(usize, Option<u64>)> {
        *self.0.sealed.lock().unwrap()
    }
    pub(in crate::daemon::executor) fn returned(&self) -> Option<(usize, Option<u64>)> {
        *self.0.returned.lock().unwrap()
    }
    pub(in crate::daemon::executor) fn destination_read_failed(&self) -> bool {
        self.0
            .target_read
            .lock()
            .unwrap()
            .is_some_and(|(_, fired)| fired)
    }
    pub(in crate::daemon::executor) fn checked_parallel_source(&self) -> bool {
        self.reads_overlapped() && self.0.prepared_source_used.load(Ordering::Acquire)
    }
    pub(in crate::daemon::executor) fn reads_overlapped(&self) -> bool {
        *self.0.parallel_reads.0.lock().unwrap() == 3
    }
}
impl Drop for ReadbackSealMarker {
    fn drop(&mut self) {
        arms()
            .lock()
            .unwrap()
            .retain(|old| !Arc::ptr_eq(old, &self.0));
    }
}
pub(in crate::daemon::executor) fn arm_readback_seal_for_test(
    parent: &ReclaimReservation,
    commands: Arc<Mutex<Vec<(usize, Vec<u8>)>>>,
    mode: ReadbackSealMode,
) -> ReadbackSealMarker {
    let entry = Arc::new(Armed {
        parent: parent.intent().clone(),
        commands,
        mode,
        sealed: Mutex::new(None),
        returned: Mutex::new(None),
        target_read: Mutex::new(None),
        parallel_reads: (Mutex::new(0), Condvar::new()),
        prepared_source_used: AtomicBool::new(false),
    });
    let mut all = arms().lock().unwrap();
    assert!(!all.iter().any(|old| old.parent == entry.parent));
    all.push(entry.clone());
    ReadbackSealMarker(entry)
}

pub(super) fn before_destination_read(parent: &ReclaimReservation, drive: usize) {
    if let Some(entry) = matching(parent)
        && matches!(
            entry.mode,
            ReadbackSealMode::DestinationBodyReadFails
                | ReadbackSealMode::ParallelFormatPreparation
        )
    {
        assert!(
            entry
                .target_read
                .lock()
                .unwrap()
                .replace((drive, false))
                .is_none()
        );
    }
}

pub(in crate::daemon::executor) fn observe_group_read_for_test(
    commands: &Arc<Mutex<Vec<(usize, Vec<u8>)>>>,
    drive: usize,
    cdb: &[u8],
) -> bool {
    if cdb.first() != Some(&crate::scsi::cdb::opcode::READ_6) {
        return false;
    }
    let Some(entry) = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|entry| Arc::ptr_eq(&entry.commands, commands))
        .cloned()
    else {
        return false;
    };
    let mut target_read = entry.target_read.lock().unwrap();
    let Some((target, failed)) = *target_read else {
        return false;
    };
    let fail = matches!(entry.mode, ReadbackSealMode::DestinationBodyReadFails)
        && target == drive
        && !failed;
    if fail {
        *target_read = Some((drive, true));
    }
    drop(target_read);
    if matches!(
        entry.mode,
        ReadbackSealMode::ParallelFormatPreparation | ReadbackSealMode::DestinationBodyReadFails
    ) {
        // Each drive waits at its first READ until the other drive reaches
        // its own READ. A serial implementation cannot pass this rendezvous.
        let (reads, both) = &entry.parallel_reads;
        let mut seen = reads.lock().unwrap();
        *seen |= if target == drive { 1 } else { 2 };
        both.notify_all();
        let (_seen, timed) = both
            .wait_timeout_while(seen, std::time::Duration::from_secs(10), |seen| *seen != 3)
            .unwrap();
        assert!(!timed.timed_out(), "源和目标设备的 READ 未并行到达");
    }
    fail
}

pub(super) fn record_prepared_source_used(parent: &ReclaimReservation) {
    if let Some(entry) = matching(parent) {
        entry.prepared_source_used.store(true, Ordering::Release);
    }
}
fn matching(parent: &ReclaimReservation) -> Option<Arc<Armed>> {
    arms()
        .lock()
        .unwrap()
        .iter()
        .find(|entry| entry.parent == *parent.intent())
        .cloned()
}
pub(super) fn record(
    parent: &ReclaimReservation,
    device: &dyn crate::scsi::transport::TapeTransport,
    returned: bool,
) {
    if let Some(entry) = matching(parent) {
        let sample = (entry.commands.lock().unwrap().len(), device.session_epoch());
        let slot = if returned {
            &entry.returned
        } else {
            &entry.sealed
        };
        let mut saved = slot.lock().unwrap();
        assert!(
            saved.is_none(),
            "original readback wrapper marker is one-shot"
        );
        *saved = Some(sample);
    }
}
pub(super) fn before_monitor(
    parent: &ReclaimReservation,
    owner: &Active,
    control: &FdbOwnerControl,
) {
    if let Some(entry) = matching(parent) {
        if matches!(entry.mode, ReadbackSealMode::MonitorOwnerFenced) {
            assert!(entry.sealed.lock().unwrap().is_some());
            futures_executor::block_on(Box::pin(
                control
                    .catalog
                    .claim_active_owner_epoch(&control.serial, owner.round.checked_add(1).unwrap()),
            ))
            .unwrap();
        }
    }
}
