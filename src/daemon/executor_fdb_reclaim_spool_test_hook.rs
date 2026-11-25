//! Software-only injection of download_file's admission budget, never its result.
use super::DeviceKind;
use crate::local_read_storage::ReadStorageBudget;
use crate::managed_catalog::{
    ManagedCatalog, ReclaimCoverageProgress, ReclaimReservation, ReclaimSourceInventory, VolumeCopy,
};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use uuid::Uuid;

type Key = (Uuid, String, u64);
type Commands = Arc<Mutex<Vec<(DeviceKind, u8)>>>;
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    coverage: ReclaimCoverageProgress,
    copies: Vec<VolumeCopy>,
    writes: usize,
    primary: u64,
}
struct Injection {
    commands: Commands,
    primary: Arc<AtomicU64>,
    before: Mutex<Option<Snapshot>>,
}
static HOOKS: OnceLock<Mutex<BTreeMap<Key, Arc<Injection>>>> = OnceLock::new();
fn key(parent: &ReclaimReservation, uid: u64) -> Key {
    (
        parent.namespace(),
        parent.intent()["operation"].as_str().unwrap().to_owned(),
        uid,
    )
}
impl Injection {
    async fn snapshot(
        &self,
        catalog: &ManagedCatalog,
        parent: &ReclaimReservation,
        owner: u64,
        uid: u64,
        inventory: &ReclaimSourceInventory<'_>,
    ) -> Snapshot {
        assert!(
            catalog
                .discover_reclaim_metadata_write(parent, owner, uid)
                .await
                .unwrap()
                .is_none(),
            "raw UID must not yet have a metadata attempt"
        );
        assert!(
            !catalog
                .reclaim_source_node_covered(parent, owner, inventory, uid)
                .await
                .unwrap(),
            "raw UID must remain uncovered"
        );
        let coverage = catalog
            .reclaim_source_coverage_progress(parent, owner)
            .await
            .unwrap();
        let page = catalog
            .copies_on_volume(parent.spec().volume(), None, 100)
            .await
            .unwrap();
        assert!(
            page.next.is_none(),
            "fixture reference snapshot must be complete"
        );
        let writes = self
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, op)| *kind == DeviceKind::Drive && *op == 0x0a)
            .count();
        Snapshot {
            coverage,
            copies: page.items,
            writes,
            primary: self.primary.load(Ordering::SeqCst),
        }
    }
}
pub(super) struct BudgetHook {
    key: Key,
    injection: Arc<Injection>,
}
impl Drop for BudgetHook {
    fn drop(&mut self) {
        HOOKS.get().unwrap().lock().unwrap().remove(&self.key);
    }
}
impl BudgetHook {
    pub(super) fn fired(&self) -> bool {
        self.injection.before.lock().unwrap().is_some()
    }
    pub(super) async fn assert_failed(
        &self,
        catalog: &ManagedCatalog,
        parent: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
    ) {
        assert_eq!(key(parent, self.key.2), self.key);
        let before = self.injection.before.lock().unwrap().clone().unwrap();
        let after = self
            .injection
            .snapshot(catalog, parent, owner, self.key.2, inventory)
            .await;
        assert_eq!(before.primary, 0);
        assert_eq!(
            after.primary, 0,
            "budget admission must fail before a main raw READ"
        );
        assert_eq!(
            after, before,
            "raw task/coverage/references/WRITE count must not advance on failed download admission"
        );
        eprintln!(
            "C1 admission-budget fault verified: namespace={} parent={} uid={} primary=0 newWRITE=0 metadataTask=None coverage/references unchanged; injection is budget, not a full disk",
            self.key.0, self.key.1, self.key.2
        );
    }
}
pub(super) fn install(
    parent: &ReclaimReservation,
    uid: u64,
    commands: Commands,
    primary: Arc<AtomicU64>,
) -> BudgetHook {
    let key = key(parent, uid);
    let injection = Arc::new(Injection {
        commands,
        primary,
        before: Mutex::new(None),
    });
    assert!(
        HOOKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(key.clone(), injection.clone())
            .is_none()
    );
    BudgetHook { key, injection }
}
pub(super) async fn budget(
    catalog: &ManagedCatalog,
    parent: &ReclaimReservation,
    owner: u64,
    uid: u64,
    inventory: &ReclaimSourceInventory<'_>,
    mut budget: ReadStorageBudget,
) -> ReadStorageBudget {
    // Clone and release the hook lock before any FDB await.
    let injection = HOOKS
        .get()
        .and_then(|hooks| hooks.lock().unwrap().get(&key(parent, uid)).cloned());
    if let Some(injection) = injection {
        let length = inventory.raw_body_length(uid).unwrap();
        assert!(length > 0 && budget.download_bytes >= length);
        let snapshot = injection
            .snapshot(catalog, parent, owner, uid, inventory)
            .await;
        assert_eq!(snapshot.primary, 0);
        assert!(
            injection.before.lock().unwrap().replace(snapshot).is_none(),
            "inject exactly once on the original fresh raw UID"
        );
        budget.download_bytes = length - 1;
    }
    budget
}
