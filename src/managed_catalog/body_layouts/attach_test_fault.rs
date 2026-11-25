//! Test-only, namespace/token-scoped faults around the real final submission.
use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) enum AttachTestPoint {
    BeforeSubmit,
    AfterCommitted,
}
struct Armed {
    token: Uuid,
    point: AttachTestPoint,
    fired: bool,
}
static ARMED: Mutex<BTreeMap<Uuid, Armed>> = Mutex::new(BTreeMap::new());

pub(in crate::managed_catalog) struct AttachTestGuard {
    namespace: Uuid,
    token: Uuid,
}
impl AttachTestGuard {
    pub(in crate::managed_catalog) fn fired(&self) -> bool {
        ARMED
            .lock()
            .unwrap()
            .get(&self.namespace)
            .is_some_and(|state| state.token == self.token && state.fired)
    }
}
impl Drop for AttachTestGuard {
    fn drop(&mut self) {
        let mut armed = ARMED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if armed
            .get(&self.namespace)
            .is_some_and(|state| state.token == self.token)
        {
            armed.remove(&self.namespace);
        }
    }
}

pub(in crate::managed_catalog) fn arm_attach_test_fault(
    namespace: Uuid,
    point: AttachTestPoint,
) -> AttachTestGuard {
    assert!(!namespace.is_nil());
    let token = Uuid::new_v4();
    let mut armed = ARMED.lock().unwrap();
    if armed.contains_key(&namespace) {
        drop(armed);
        panic!("an attach test fault already owns this namespace");
    }
    armed.insert(
        namespace,
        Armed {
            token,
            point,
            fired: false,
        },
    );
    AttachTestGuard { namespace, token }
}

pub(super) fn fire(namespace: Uuid, point: AttachTestPoint) -> Result<()> {
    let mut armed = ARMED.lock().unwrap();
    let Some(state) = armed.get_mut(&namespace) else {
        return Ok(());
    };
    if state.point != point || state.fired {
        return Ok(());
    }
    state.fired = true;
    drop(armed);
    match point {
        // This is a pre-submit injected failure, not an actual unknown commit.
        AttachTestPoint::BeforeSubmit => Err(CatalogError::Incomplete),
        // db.run already returned success. Only the test-layer receipt is lost;
        // no claim is made about client-library retry or a real network fault.
        AttachTestPoint::AfterCommitted => Err(CatalogError::Fdb(
            foundationdb::FdbError::from_code(1021).into(),
        )),
    }
}

#[test]
fn namespace_token_fault_is_one_shot_and_drop_does_not_clear_another_token() {
    let namespace = Uuid::new_v4();
    let guard = arm_attach_test_fault(namespace, AttachTestPoint::BeforeSubmit);
    fire(Uuid::new_v4(), AttachTestPoint::BeforeSubmit).unwrap();
    fire(namespace, AttachTestPoint::AfterCommitted).unwrap();
    assert!(!guard.fired());
    assert!(fire(namespace, AttachTestPoint::BeforeSubmit).is_err());
    assert!(guard.fired());
    fire(namespace, AttachTestPoint::BeforeSubmit).unwrap();
    let other_token = Uuid::new_v4();
    ARMED.lock().unwrap().get_mut(&namespace).unwrap().token = other_token;
    drop(guard);
    assert_eq!(
        ARMED.lock().unwrap().get(&namespace).unwrap().token,
        other_token
    );
    ARMED.lock().unwrap().remove(&namespace);
}
