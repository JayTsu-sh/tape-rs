//! Native reclaim executor continuation. Temporary completion ledgers have a
//! durable cleanup queue and are drained before ordinary takeover recovery.
use super::*;
#[path = "executor_fdb_reclaim_driver.rs"]
mod driver;
pub(super) use driver::PreparedReclaimFormatSource;
pub(super) use driver::advance;
#[cfg(test)]
pub(super) use driver::arm_original_probe_seal_for_test;
#[cfg(test)]
pub(super) use driver::probe_original_for_test;
pub(in crate::daemon) use driver::{
    FreshReclaimGroupIo, ObserveReclaimGroupIo, OriginalAttemptProbeIo,
};
#[cfg(test)]
pub(super) use driver::{OriginalS4StopFacts, arm_original_checkpoint_s4_stop_for_test};
#[cfg(test)]
pub(super) use driver::{
    ReadbackSealMode, arm_readback_seal_for_test, observe_group_read_for_test,
};

/// Claim and classify every parent using FDB only. A protected original task
/// keeps generic recovery away from its media, even if it is not the first
/// parent in the bounded queue page.
pub(super) async fn recovery_barrier(
    owner: &Active,
    control: &FdbOwnerControl,
) -> std::result::Result<bool, ReadError> {
    let mut after = None;
    let mut protected = false;
    loop {
        let page = control
            .catalog
            .claim_reclaim_reservations_page(
                &control.serial,
                control.route,
                owner.round,
                after.as_deref(),
                16,
            )
            .await
            .map_err(|e| ReadError::Lost(format!("回收恢复隔离接任: {e}")))?;
        for parent in &page.items {
            protected |= !matches!(
                control
                    .catalog
                    .classify_reclaim_execution(parent, owner.round)
                    .await
                    .map_err(|e| ReadError::Lost(format!("回收恢复隔离: {e}")))?,
                crate::managed_catalog::ReclaimExecutionGate::Fresh
            );
        }
        let Some(next) = page.next else {
            return Ok(protected);
        };
        after = Some(next);
    }
}

pub(super) async fn cleanup(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    drain: bool,
) -> std::result::Result<(), ReadError> {
    let check = || {
        if permit.allows(owner.round, super::super::permission::Phase::Execution)
            || permit.allows(owner.round, super::super::permission::Phase::Serving)
        {
            Ok(())
        } else {
            Err(ReadError::Lost("回收完成清理期间多数派许可失效".into()))
        }
    };
    let mut after = None;
    loop {
        check()?;
        let page = control
            .catalog
            .reclaim_cleanup_page(
                &control.serial,
                control.route,
                owner.round,
                after.as_deref(),
                16,
            )
            .await
            .map_err(|e| ReadError::Lost(format!("回收完成清理队列: {e}")))?;
        for parent in page.items {
            loop {
                check()?;
                let complete = control
                    .catalog
                    .step_reclaim_cleanup(&parent, owner.round)
                    .await
                    .map_err(|e| ReadError::Lost(format!("回收完成清理: {e}")))?;
                if !drain {
                    return Ok(());
                }
                if complete {
                    break;
                }
            }
        }
        let Some(next) = page.next else {
            return Ok(());
        };
        after = Some(next);
    }
}
