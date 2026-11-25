//! Read/settlement authority for an original admitted physical checkpoint.
//! This type cannot be passed to the Fresh reserve/complete/stage interfaces.
use super::*;

pub(crate) struct ReclaimGroupObservation {
    parent: ReclaimReservation,
    group: ReclaimCopyGroup,
    intent: ReclaimIndexIntent,
    owner: u64,
}

impl ReclaimGroupObservation {
    pub(crate) fn batch_id(&self) -> Uuid {
        self.group.batch_id()
    }
    pub(crate) fn parent(&self) -> &ReclaimReservation {
        &self.parent
    }
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }
    pub(crate) fn destination_drive_serial(&self) -> &str {
        &self.group.header.destination.drive_serial
    }
    pub(crate) fn group(&self) -> &ReclaimCopyGroup {
        &self.group
    }
    pub(crate) fn intent(&self) -> &ReclaimIndexIntent {
        &self.intent
    }

    pub(super) async fn require_at(&self, tr: &Transaction) -> Result<()> {
        routing::require_runtime_active_owner(
            tr,
            self.parent.serial(),
            self.parent.route,
            self.owner,
        )
        .await?;
        parent_at(tr, &self.parent, self.owner).await?;
        require_parent_pool_at(tr, &self.parent).await?;
        source_state_at(tr, &self.parent).await?;
        let current = evidence_group_at(tr, &self.parent, self.group.id()).await?;
        isolation::require_group_action_at(tr, &self.parent, self.owner, &current).await?;
        if current.header != self.group.header
            || current.members != self.group.members
            || current.header.owner != self.owner
            || current.header.version != 3
            || !matches!(current.header.phase, Phase::Admitted | Phase::Settling)
            || tr
                .get(&key(&self.parent, b'a', None, None), false)
                .await?
                .as_deref()
                != Some(self.group.id().as_bytes())
        {
            return Err(CatalogError::OwnerFenced);
        }
        let sealed = require_intent_at(tr, &self.parent, &current).await?;
        if sealed.intent != self.intent.intent || sealed.members != self.intent.members {
            return Err(CatalogError::Corrupt("回收观察期原生元数据封存改变"));
        }
        // Original child edges survive unknown writes and may never be
        // replaced by a newly generated request during observation.
        for member in &current.members {
            original_child_at(tr, &self.parent, self.owner, member).await?;
        }
        let layout = layout::at(tr, &self.parent)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if layout.destination != current.header.destination
            || digest(b"tape-rs/reclaim-group-layout/1", &layout)? != current.header.layout
        {
            return Err(CatalogError::Invalid("回收观察期原布局改变"));
        }
        let assignment = ManagedCatalog::verified_assignment(
            tr,
            self.parent.serial(),
            &current.header.destination.barcode,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
        if assignment.namespace != self.parent.namespace()
            || assignment.pool_id != self.parent.spec().pool()
            || assignment.volume_id != Some(current.header.destination.volume)
            || assignment.revision != current.header.destination.revision
            || !assignment.allows_io()
        {
            return Err(CatalogError::Invalid("回收观察期目标分配或策略改变"));
        }
        let (scan, index) =
            active_volume::observed_at(tr, self.parent.serial(), current.header.destination.volume)
                .await?;
        let mut encoded = Vec::new();
        active_volume::encode_index(&mut encoded, index);
        if scan.id.to_string() != current.header.scan {
            return Err(CatalogError::Invalid("回收观察期目标scan改变"));
        }
        if encoded != current.header.previous {
            let plan = require_actual_at(tr, &self.parent, &current).await?;
            let raw = tr
                .get(&key(&self.parent, b'p', Some(current.id()), None), false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let observed: Observed = native_decode(&raw)?;
            observed.require_full_dp(&plan)?;
            if observed.version != 1
                || observed.plan != plan.fingerprint()?
                || observed.index != encoded
                || observed.dp_eod != observed.construct_end
                || index.generation != plan.actual.generation
                || index.location.start_block != plan.actual.index_start
            {
                return Err(CatalogError::Invalid(
                    "回收观察期目标不是原base或原checkpoint",
                ));
            }
        }
        source::require_metadata_idle_at(tr, &self.parent).await?;
        mechanics::require_parent_idle_at(tr, &self.parent).await?;
        Ok(())
    }
}

pub(super) async fn at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    id: Uuid,
) -> Result<ReclaimGroupObservation> {
    let group = evidence_group_at(tr, parent, id).await?;
    let intent = require_intent_at(tr, parent, &group).await?;
    let scope = ReclaimGroupObservation {
        parent: parent.clone(),
        group,
        intent,
        owner,
    };
    scope.require_at(tr).await?;
    Ok(scope)
}

impl ManagedCatalog {
    /// Only adopt the original admitted identity. A missing proof is expected
    /// before capture and is never used to authorize a new append.
    pub(crate) async fn open_reclaim_group_observation(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
    ) -> Result<ReclaimGroupObservation> {
        let tr = self.db.create_trx()?;
        at(&tr, parent, owner, id).await
    }

    pub(crate) async fn check_reclaim_group_observation(
        &self,
        scope: &ReclaimGroupObservation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        scope.require_at(&tr).await
    }
}
