//! Test-only, one original parent's committed Writing/admission reply loss.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};
struct State {
    fired: std::sync::atomic::AtomicBool,
}
static ARMED: OnceLock<Mutex<std::collections::HashMap<Uuid, Arc<State>>>> = OnceLock::new();
fn operation(parent: &ReclaimReservation) -> Uuid {
    Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap()
}
pub(crate) struct Fault {
    operation: Uuid,
    state: Arc<State>,
}
impl Fault {
    pub(crate) fn fired(&self) -> bool {
        self.state.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        ARMED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&self.operation);
    }
}
pub(super) fn after_committed(parent: &ReclaimReservation) -> Result<()> {
    if let Some(state) = ARMED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&operation(parent))
    {
        if !state.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(CatalogError::Invalid(
                "test-only原MetadataTask Writing准入回执丢失",
            ));
        }
    }
    Ok(())
}
impl ManagedCatalog {
    pub(crate) fn lose_reclaim_metadata_admission_once(
        &self,
        parent: &ReclaimReservation,
    ) -> Fault {
        let operation = operation(parent);
        let state = Arc::new(State {
            fired: std::sync::atomic::AtomicBool::new(false),
        });
        assert!(
            ARMED
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(operation, state.clone())
                .is_none()
        );
        Fault { operation, state }
    }
}
