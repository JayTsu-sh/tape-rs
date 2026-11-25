//! Test-only real wrapper boundary. No scope, callback, or I/O authority.
use crate::managed_catalog::ReclaimReservation;
use crate::scsi::transport::TapeTransport;
use std::sync::{Arc, Mutex, OnceLock};
struct Armed {
    parent: serde_json::Value,
    commands: Arc<Mutex<Vec<(usize, Vec<u8>)>>>,
    sealed: Mutex<Option<(usize, Option<u64>)>>,
}
fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
    static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(in crate::daemon::executor) struct OriginalProbeSealMarker(Arc<Armed>);
impl OriginalProbeSealMarker {
    pub(in crate::daemon::executor) fn sealed(&self) -> (usize, Option<u64>) {
        self.0
            .sealed
            .lock()
            .unwrap()
            .expect("real media probe must seal first")
    }
}
impl Drop for OriginalProbeSealMarker {
    fn drop(&mut self) {
        arms()
            .lock()
            .unwrap()
            .retain(|old| !Arc::ptr_eq(old, &self.0));
    }
}
pub(in crate::daemon::executor) fn arm_original_probe_seal_for_test(
    parent: &ReclaimReservation,
    commands: Arc<Mutex<Vec<(usize, Vec<u8>)>>>,
) -> OriginalProbeSealMarker {
    let armed = Arc::new(Armed {
        parent: parent.intent().clone(),
        commands,
        sealed: Mutex::new(None),
    });
    let mut all = arms().lock().unwrap();
    assert!(!all.iter().any(|old| old.parent == armed.parent));
    all.push(armed.clone());
    OriginalProbeSealMarker(armed)
}
pub(super) fn record(parent: &ReclaimReservation, device: &dyn TapeTransport) {
    let armed = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|armed| armed.parent == *parent.intent())
        .cloned();
    if let Some(armed) = armed {
        let epoch = device.session_epoch();
        let count = armed.commands.lock().unwrap().len();
        let mut sealed = armed.sealed.lock().unwrap();
        assert!(sealed.is_none(), "same original probe marker is one-shot");
        *sealed = Some((count, epoch));
    }
}
