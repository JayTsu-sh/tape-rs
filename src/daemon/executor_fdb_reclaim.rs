//! Native reclaim executor continuation. Temporary completion ledgers have a
//! durable cleanup queue and are drained before ordinary takeover recovery.
use super::*;
#[path = "executor_fdb_reclaim_driver.rs"]
mod driver;
pub(super) use driver::advance;

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
