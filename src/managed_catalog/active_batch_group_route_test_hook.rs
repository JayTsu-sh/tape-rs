//! Exact original-group header-stage corruption tests; no production switch.
use super::*;
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Clone, Copy)]
pub(crate) enum GroupStageRouteConflict {
    Global,
    Parent,
    PageMissingBinding,
    PageConflict,
}
struct Armed {
    parent: Value,
    which: GroupStageRouteConflict,
    before: Mutex<Option<Vec<(Vec<u8>, Option<Vec<u8>>)>>>,
    outcome: Mutex<Option<ActiveBatchOutcome>>,
    fired: std::sync::atomic::AtomicBool,
}
fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
    static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(crate) struct GroupStageRouteFault(Arc<Armed>);
impl GroupStageRouteFault {
    pub(crate) fn fired(&self) -> bool {
        self.0.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn outcome(&self) -> Option<ActiveBatchOutcome> {
        *self.0.outcome.lock().unwrap()
    }
    pub(crate) fn expects_business_conflict(&self) -> bool {
        matches!(self.0.which, GroupStageRouteConflict::PageConflict)
    }
    pub(crate) fn before(&self) -> Option<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        self.0.before.lock().unwrap().clone()
    }
}
impl Drop for GroupStageRouteFault {
    fn drop(&mut self) {
        arms()
            .lock()
            .unwrap()
            .retain(|old| !Arc::ptr_eq(old, &self.0));
    }
}
pub(super) fn arm(
    parent: &crate::managed_catalog::ReclaimReservation,
    which: GroupStageRouteConflict,
) -> GroupStageRouteFault {
    let value = Arc::new(Armed {
        parent: parent.intent().clone(),
        which,
        before: Mutex::new(None),
        outcome: Mutex::new(None),
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let mut all = arms().lock().unwrap();
    assert!(!all.iter().any(|old| old.parent == value.parent));
    all.push(value.clone());
    GroupStageRouteFault(value)
}
pub(super) async fn before_header(
    catalog: &ManagedCatalog,
    scope: &ReclaimGroupExecution,
    receipts: &[CommitReceipt],
) -> Result<()> {
    let arm = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|arm| {
            arm.parent == *scope.parent().intent()
                && matches!(
                    arm.which,
                    GroupStageRouteConflict::Global | GroupStageRouteConflict::Parent
                )
                && !arm.fired.load(std::sync::atomic::Ordering::SeqCst)
        })
        .cloned();
    let Some(arm) = arm else {
        return Ok(());
    };
    let routekeys = scope.stage_binding_keys_for_test();
    let tr = catalog.db.create_trx()?;
    for key in &routekeys {
        if tr.get(key, false).await?.is_some() {
            return Err(CatalogError::Corrupt("stage故障fixture必须原三binding全缺"));
        }
    }
    let corrupt = match arm.which {
        GroupStageRouteConflict::Global => &routekeys[1],
        GroupStageRouteConflict::Parent => &routekeys[2],
        _ => unreachable!(),
    };
    tr.set(corrupt, b"original-group-stage-conflicting-route");
    tr.commit()
        .await
        .map_err(|error| CatalogError::Cache(format!("测试原route冲突注入提交未知: {error:?}")))?;
    snapshot(catalog, scope, receipts, &arm).await?;
    arm.fired.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

async fn snapshot(
    catalog: &ManagedCatalog,
    scope: &ReclaimGroupExecution,
    receipts: &[CommitReceipt],
    arm: &Armed,
) -> Result<()> {
    let first = receipts.first().ok_or(CatalogError::Incomplete)?;
    let routekeys = scope.stage_binding_keys_for_test();
    let mut keys = std::collections::BTreeSet::from(routekeys);
    keys.insert(batch_key(first.volume_id, first.volume_generation));
    for page in 0..receipts.chunks(PAGE_ITEMS).len() {
        keys.insert(page_key(
            first.volume_id,
            first.volume_generation,
            page as u16,
        ));
        keys.insert(page_digest_key(
            first.volume_id,
            first.volume_generation,
            page as u16,
        ));
    }
    for receipt in receipts {
        keys.extend([
            claim_key_for(
                receipt.namespace,
                receipt.request_epoch,
                receipt.operation_id,
            ),
            reverse_claim_key(receipt),
            path_claim_key(receipt),
            file_claim_key(receipt),
            requests::intent_key(receipt),
        ]);
        let mut pending = requests::epoch_prefix(b'0', receipt.namespace, receipt.request_epoch);
        pending.extend_from_slice(receipt.operation_id.as_bytes());
        keys.insert(pending);
    }
    let tr = catalog.db.create_trx()?;
    let mut rows = Vec::new();
    for key in keys {
        rows.push((key.clone(), tr.get(&key, false).await?.map(|v| v.to_vec())));
    }
    *arm.before.lock().unwrap() = Some(rows);
    Ok(())
}
/// Runs after the legitimate Header3 transaction, before any page mutation.
pub(super) async fn before_page(
    catalog: &ManagedCatalog,
    scope: &ReclaimGroupExecution,
    receipts: &[CommitReceipt],
    number: usize,
) -> Result<()> {
    if number != 0 {
        return Ok(());
    }
    let arm = arms()
        .lock()
        .unwrap()
        .iter()
        .find(|arm| {
            arm.parent == *scope.parent().intent()
                && matches!(
                    arm.which,
                    GroupStageRouteConflict::PageMissingBinding
                        | GroupStageRouteConflict::PageConflict
                )
                && !arm.fired.load(std::sync::atomic::Ordering::SeqCst)
        })
        .cloned();
    let Some(arm) = arm else {
        return Ok(());
    };
    let first = receipts.first().ok_or(CatalogError::Incomplete)?;
    let keys = scope.stage_binding_keys_for_test();
    let tr = catalog.db.create_trx()?;
    for key in &keys {
        if tr.get(key, false).await?.is_none() {
            return Err(CatalogError::Incomplete);
        }
    }
    let header = Header::decode(
        &tr.get(&batch_key(first.volume_id, first.volume_generation), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    if header.protocol != Protocol::ReclaimGroup
        || header.phase != ActiveBatchPhase::Open
        || header.batch_id != scope.batch_id()
    {
        return Err(CatalogError::Incomplete);
    }
    let page = page_key(first.volume_id, first.volume_generation, 0);
    let digest = page_digest_key(first.volume_id, first.volume_generation, 0);
    if tr.get(&page, false).await?.is_some() || tr.get(&digest, false).await?.is_some() {
        return Err(CatalogError::Incomplete);
    }
    if matches!(arm.which, GroupStageRouteConflict::PageMissingBinding) {
        tr.clear(&keys[0]);
    }
    tr.set(&page, b"original-conflicting-page");
    tr.set(&digest, &[0x39; 32]);
    tr.commit()
        .await
        .map_err(|error| CatalogError::Cache(format!("测试原page冲突注入提交未知: {error:?}")))?;
    snapshot(catalog, scope, receipts, &arm).await?;
    arm.fired.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}
pub(super) fn after_page(scope: &ReclaimGroupExecution, outcome: ActiveBatchOutcome) {
    if let Some(arm) = arms().lock().unwrap().iter().find(|arm| {
        arm.parent == *scope.parent().intent()
            && arm.fired.load(std::sync::atomic::Ordering::SeqCst)
    }) {
        *arm.outcome.lock().unwrap() = Some(outcome);
    }
}
