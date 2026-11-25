//! One-shot, operation-scoped fault injection; absent from production builds.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Clone)]
pub(crate) struct SettlementBefore {
    pub(crate) rows: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub(crate) rv: Vec<u8>,
}
struct Armed {
    parent: String,
    ordinal: u8,
    fired: std::sync::atomic::AtomicBool,
    before: Mutex<Option<SettlementBefore>>,
    staged_rv: Mutex<Option<Vec<u8>>>,
}
fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
    static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(crate) struct ReclaimSettlementAfterRvFault(Arc<Armed>);
impl ReclaimSettlementAfterRvFault {
    pub(crate) fn fired(&self) -> bool {
        self.0.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn staged_rv(&self) -> Option<Vec<u8>> {
        self.0.staged_rv.lock().unwrap().clone()
    }
    pub(crate) fn before(&self) -> Option<SettlementBefore> {
        self.0.before.lock().unwrap().clone()
    }
}
impl Drop for ReclaimSettlementAfterRvFault {
    fn drop(&mut self) {
        arms()
            .lock()
            .unwrap()
            .retain(|entry| !Arc::ptr_eq(entry, &self.0));
    }
}
pub(super) fn arm(parent: &ReclaimReservation, ordinal: u8) -> ReclaimSettlementAfterRvFault {
    let entry = Arc::new(Armed {
        parent: parent.fingerprint(),
        ordinal,
        fired: std::sync::atomic::AtomicBool::new(false),
        before: Mutex::new(None),
        staged_rv: Mutex::new(None),
    });
    let mut all = arms().lock().unwrap();
    assert!(
        !all.iter()
            .any(|old| old.parent == entry.parent && old.ordinal == ordinal)
    );
    all.push(entry.clone());
    ReclaimSettlementAfterRvFault(entry)
}
fn matching(parent: &ReclaimReservation, ordinal: u8) -> Option<Arc<Armed>> {
    arms()
        .lock()
        .unwrap()
        .iter()
        .find(|entry| {
            entry.parent == parent.fingerprint()
                && entry.ordinal == ordinal
                && !entry.fired.load(std::sync::atomic::Ordering::SeqCst)
        })
        .cloned()
}
pub(super) async fn before_verification_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
    ordinal: u8,
) -> Result<()> {
    let Some(arm) = matching(parent, ordinal) else {
        return Ok(());
    };
    let mut keys = std::collections::BTreeSet::from([
        parent.journal_key(),
        walk::state_key(parent),
        key(parent, b'h', Some(group.id()), None),
    ]);
    for member in &group.members {
        let original = original_child_at(tr, parent, owner, member).await?;
        let rv = walk::verification_key(parent, &member.file);
        let mut rx = rv.clone();
        rx[domain(parent.namespace(), b'v').len() - 1] = b'x';
        keys.extend([
            rv,
            rx,
            children::history_pin_key(&original.request()),
            key(parent, b'm', Some(group.id()), Some(member.ordinal)),
            key(parent, b'r', Some(member.file.operation()), None),
            source::group_coverage_test_key(parent, member.body_uid),
        ]);
        let childkey =
            children::child_key(parent.namespace(), &parent.token(), original.operation_id)?;
        let raw = tr
            .get(&childkey, false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        keys.insert(childkey);
        let child = super::super::super::super::decode(&raw)?;
        let head = decode_head(
            &serde_json::from_value::<Vec<u8>>(child["source_head"].clone()).map_err(cache)?,
        )?;
        let source = copy_records::decode_copy(
            &serde_json::from_value::<Vec<u8>>(child["source_copy"].clone()).map_err(cache)?,
        )?;
        keys.insert(copy_key(
            parent.namespace(),
            member.file.file_id(),
            head.copy_id,
        ));
        keys.insert(volume_key(
            source.volume_id,
            source.volume_scan_id,
            parent.namespace(),
            member.file.file_id(),
            head.copy_id,
        ));
    }
    let mut rows = Vec::new();
    for key in keys {
        rows.push((key.clone(), tr.get(&key, false).await?.map(|v| v.to_vec())));
    }
    let member = group
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    *arm.before.lock().unwrap() = Some(SettlementBefore {
        rows,
        rv: walk::verification_key(parent, &member.file),
    });
    Ok(())
}
pub(super) async fn after_rv_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    ordinal: u8,
) -> Result<()> {
    if let Some(arm) = matching(parent, ordinal) {
        assert!(arm.before.lock().unwrap().is_some());
        let key = arm.before.lock().unwrap().as_ref().unwrap().rv.clone();
        *arm.staged_rv.lock().unwrap() = Some(
            tr.get(&key, false)
                .await?
                .ok_or(CatalogError::Incomplete)?
                .to_vec(),
        );
        arm.fired.store(true, std::sync::atomic::Ordering::SeqCst);
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
