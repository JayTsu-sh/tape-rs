//! Actual complete reads scheduled by task-owned untrusted location hints.
//! These helpers neither admit writes nor reuse a historical media proof.
use super::*;

impl ReclaimMetadataWriter<'_> {
    pub(super) fn capture_target_history(
        &self,
        reservation: &ReclaimReservation,
        task: &ReclaimMetadataWrite,
        checked: &dyn TapeTransport,
        label: &crate::ltfs::label::LtfsLabel,
    ) -> Result<CapturedIndexSource> {
        // FoundationDB completes these futures on its existing network thread,
        // as do the original per-CDB catalog checks. No actor/timer is awaited.
        let context =
            futures_executor::block_on(self.catalog.reclaim_attempt_destination_history_context(
                reservation,
                self.key.round(),
                task,
            ))
            .map_err(catalog)?;
        let hint = futures_executor::block_on(self.catalog.load_reclaim_destination_history_hint(
            reservation,
            self.key.round(),
            &context,
            &self.budget.index_scratch,
        ))
        .map_err(catalog)?;
        let scope_checked = Guard::new(checked, || {
            futures_executor::block_on(self.catalog.require_reclaim_destination_history_context(
                reservation,
                self.key.round(),
                &context,
            ))
            .map_err(catalog)
        });
        let actual = crate::ltfs::recovery::capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&scope_checked),
            label,
            self.budget,
            hint,
        )?;
        scope_checked.verify_proof()?;
        Ok(actual)
    }

    // Call only after the existing task's actual plan/readback recognizer and
    // original checked.verify_proof(). The output is still scheduling data;
    // business publication retains every original independent proof gate.
    pub(super) fn publish_target_history(
        &self,
        reservation: &ReclaimReservation,
        task: &ReclaimMetadataWrite,
        actual: &CapturedIndexSource,
    ) -> Result<()> {
        let context =
            futures_executor::block_on(self.catalog.reclaim_attempt_destination_history_context(
                reservation,
                self.key.round(),
                task,
            ))
            .map_err(catalog)?;
        if futures_executor::block_on(self.catalog.publish_reclaim_destination_history_hint(
            reservation,
            self.key.round(),
            &context,
            actual,
        ))
        .map_err(catalog)?
        {
            futures_executor::block_on(self.catalog.collect_reclaim_destination_history(
                reservation,
                self.key.round(),
                &context,
            ))
            .map_err(catalog)?;
        }
        Ok(())
    }
}
