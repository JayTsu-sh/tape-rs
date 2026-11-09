//! Return-to-home proof is distinct from empty-medium proof. Unassignment
//! requires both while the original format claim still prevents new writes.
use super::*;
use crate::changer::element::{ElementAddressMap, ElementStatus};

#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimFormatHomeEvidence {
    owner: u64,
    plan: ReclaimFormatPlan,
    source: NativeMediaSource,
    command: ReclaimMediaCommand,
}
pub(super) fn validate_receipt(
    intent: &Value,
    owner: u64,
    raw: &[u8],
) -> Result<ReclaimMediaCommand> {
    let command = ReclaimMediaCommand::decode(raw)?;
    let spec = ReclaimSpec::parse(&intent["change"])?;
    let plan = command.plan();
    if command.intent() != intent
        || command.owner() != owner
        || !spec.unassign()
        || plan.action != ReclaimMediaAction::Move
        || plan.barcode != spec.barcode()
        || plan.volume != spec.volume()
        || plan.destination != plan.home
        || plan.source < plan.map.dt_start
        || plan.source - plan.map.dt_start >= plan.map.dt_count
    {
        return Err(CatalogError::Invalid(
            "回收归位证明改变原初始化、条码、home 或驱动位置",
        ));
    }
    Ok(command)
}
impl ReclaimFormatHomeEvidence {
    pub(super) fn receipt(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimFormatPlan,
        proof: &ReclaimFormatCommitEvidence,
    ) -> Result<Vec<u8>> {
        if self.owner != owner || self.plan != *plan || self.source != *proof.source() {
            return Err(CatalogError::OwnerFenced);
        }
        let raw = self.command.encode()?;
        validate_receipt(reservation.intent(), owner, &raw)?;
        Ok(raw)
    }
    pub(crate) fn from_read(
        reservation: &ReclaimReservation,
        plan: &ReclaimFormatPlan,
        proof: &ReclaimFormatCommitEvidence,
        command: &ReclaimMediaCommand,
        map: &ElementAddressMap,
        rows: &[ElementStatus],
    ) -> Result<Self> {
        proof.require(plan, plan.owner())?;
        validate_receipt(reservation.intent(), plan.owner(), &command.encode()?)?;
        if !command.plan().condition(map, rows, Some(false), true)? {
            return Err(CatalogError::Incomplete);
        }
        Ok(Self {
            owner: plan.owner(),
            plan: plan.clone(),
            source: proof.source().clone(),
            command: command.clone(),
        })
    }
    pub(super) async fn require_at(
        &self,
        tr: &Transaction,
        reservation: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimFormatPlan,
        proof: &ReclaimFormatCommitEvidence,
    ) -> Result<Vec<u8>> {
        let raw = self.receipt(reservation, owner, plan, proof)?;
        mechanics::require_confirmed_at(tr, reservation, owner, &self.command).await?;
        Ok(raw)
    }
}
