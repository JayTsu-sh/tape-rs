//! One original known S4 interruption, without forging persistent authority.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Clone)]
pub(in crate::daemon::executor) struct OriginalS4StopFacts {
    pub(in crate::daemon::executor) group: Uuid,
    pub(in crate::daemon::executor) batch: Uuid,
    pub(in crate::daemon::executor) previous: crate::managed_catalog::ActiveIndexIdentity,
    pub(in crate::daemon::executor) current: crate::managed_catalog::ActiveIndexIdentity,
    pub(in crate::daemon::executor) drive: usize,
    pub(in crate::daemon::executor) files: Vec<PreparedActiveFile>,
}
struct Armed {
    parent: serde_json::Value,
    facts: Mutex<Option<OriginalS4StopFacts>>,
    resumed: std::sync::atomic::AtomicBool,
}
fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
    static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(in crate::daemon::executor) struct OriginalS4Stop(Arc<Armed>);
impl OriginalS4Stop {
    pub(in crate::daemon::executor) fn facts(&self) -> Option<OriginalS4StopFacts> {
        self.0.facts.lock().unwrap().clone()
    }
    pub(in crate::daemon::executor) fn resumed(&self) -> bool {
        self.0.resumed.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for OriginalS4Stop {
    fn drop(&mut self) {
        arms()
            .lock()
            .unwrap()
            .retain(|old| !Arc::ptr_eq(old, &self.0));
    }
}
pub(in crate::daemon::executor) fn arm_original_checkpoint_s4_stop_for_test(
    parent: &ReclaimReservation,
) -> OriginalS4Stop {
    let entry = Arc::new(Armed {
        parent: parent.intent().clone(),
        facts: Mutex::new(None),
        resumed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut all = arms().lock().unwrap();
    assert!(!all.iter().any(|old| old.parent == entry.parent));
    all.push(entry.clone());
    OriginalS4Stop(entry)
}
pub(super) fn after_s4(
    parent: &ReclaimReservation,
    group: Uuid,
    batch: Uuid,
    previous: crate::managed_catalog::ActiveIndexIdentity,
    current: crate::managed_catalog::ActiveIndexIdentity,
    drive: usize,
    files: &[PreparedActiveFile],
) -> bool {
    let entry = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|entry| entry.parent == *parent.intent())
        .cloned();
    let Some(entry) = entry else { return false };
    let mut slot = entry.facts.lock().unwrap();
    if slot.is_some() {
        return false;
    }
    let mut files = files.to_vec();
    files.sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
    *slot = Some(OriginalS4StopFacts {
        group,
        batch,
        previous,
        current,
        drive,
        files,
    });
    true
}
pub(super) fn resumed(parent: &ReclaimReservation, batch: Uuid, files: &[PreparedActiveFile]) {
    let entry = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|entry| entry.parent == *parent.intent())
        .cloned();
    if let Some(entry) = entry {
        let slot = entry.facts.lock().unwrap();
        let old = slot
            .as_ref()
            .expect("original successful S4 must be recorded");
        assert_eq!(batch, old.batch);
        let mut files = files.to_vec();
        files.sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
        assert_eq!(files.len(), old.files.len());
        for (a, b) in files.iter().zip(&old.files) {
            assert_eq!(a.receipt, b.receipt);
            assert_eq!(a.attributes, b.attributes);
        }
        entry
            .resumed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
