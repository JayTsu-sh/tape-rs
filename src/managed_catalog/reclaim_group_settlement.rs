//! One transaction preserves original per-file Rv/retirement and binds its
//! GroupFile coverage to the shared, actually observed body/archive S4.
use super::*;
#[path = "reclaim_group_retirement.rs"]
mod group_retirement;
pub(in crate::managed_catalog) use group_retirement::GroupRetirementPins;
#[cfg(test)]
#[path = "reclaim_group_settlement_test_hook.rs"]
mod test_hook;
#[cfg(test)]
pub(crate) use test_hook::ReclaimSettlementAfterRvFault;

pub(in crate::managed_catalog) enum GroupCoverageMode<'a> {
    Stored,
    Staged(&'a GroupRetirementPins<'a>),
}
fn require_readback_binding(
    parent: &ReclaimReservation,
    owner: u64,
    group_id: Uuid,
    attempt: Uuid,
    ordinal: u8,
    readback: &crate::daemon::fdb_read::ReclaimGroupMemberReadback,
) -> Result<()> {
    let context = readback.context();
    if context.owner() != owner
        || context.scope().parent().intent() != parent.intent()
        || context.group_id() != group_id
        || context.attempt_id() != attempt
        || context.ordinal() != ordinal
        || readback.bytes() != context.receipt().size
        || context.receipt().content_digest != ContentDigest::Sha256(readback.sha256())
        || context
            .source_md5()
            .is_some_and(|md5| readback.md5() != Some(md5))
    {
        return Err(CatalogError::Invalid(
            "分组结清必须消费原owner/成员的真实sealed字节回读",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) struct GroupFileReference {
    pub(in crate::managed_catalog) group: String,
    pub(in crate::managed_catalog) physical_attempt: String,
    pub(in crate::managed_catalog) batch: String,
    pub(in crate::managed_catalog) ordinal: u8,
    pub(in crate::managed_catalog) plan: [u8; 32],
    pub(in crate::managed_catalog) observation: [u8; 32],
    pub(in crate::managed_catalog) receipt: [u8; 32],
}
fn receipt_reference(
    group: &ReclaimCopyGroup,
    member: &Member,
    receipt: &CommitReceipt,
) -> Result<[u8; 32]> {
    digest(
        b"tape-rs/reclaim-group-receipt/2",
        &(
            receipt_fingerprint(receipt),
            member.child.as_ref().ok_or(CatalogError::Incomplete)?,
            group
                .header
                .execution_epoch
                .ok_or(CatalogError::Incomplete)?,
        ),
    )
}
pub(super) fn reference_at(
    group: &ReclaimCopyGroup,
    plan: &ReclaimIndexAugmentation,
    observed: &Observed,
    ordinal: u8,
    receipt: &CommitReceipt,
) -> Result<GroupFileReference> {
    let member = group
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    Ok(GroupFileReference {
        group: group.header.group.clone(),
        physical_attempt: group.header.batch.clone(),
        batch: group.header.batch.clone(),
        ordinal,
        plan: plan.fingerprint()?,
        observation: digest(b"tape-rs/reclaim-index-observed/1", observed)?,
        receipt: receipt_reference(group, member, receipt)?,
    })
}

/// Same-RV physical/member and retained-byte proof validation. It does not
/// require Rq/settlement already to exist, so the first atomic seal is acyclic.
pub(in crate::managed_catalog) async fn group_member_coverage_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    inventory: [u8; 32],
    uid: u64,
    reference: &GroupFileReference,
) -> Result<source::ReclaimSourceNode> {
    Box::pin(group_member_coverage_in_at(
        tr,
        parent,
        owner,
        inventory,
        uid,
        reference,
        GroupCoverageMode::Stored,
    ))
    .await
}
pub(in crate::managed_catalog) async fn group_member_coverage_in_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    inventory: [u8; 32],
    uid: u64,
    reference: &GroupFileReference,
    mode: GroupCoverageMode<'_>,
) -> Result<source::ReclaimSourceNode> {
    if parent.intent()["version"] != 2
        || record_at(tr, parent, owner).await?["owner_epoch"] != owner
    {
        return Err(CatalogError::OwnerFenced);
    }
    require_parent_pool_at(tr, parent).await?;
    source_state_at(tr, parent).await?;
    let id = parse_uuid(&reference.group)?;
    let group = evidence_group_at(tr, parent, id).await?;
    let batch = parse_uuid(&reference.batch)?;
    if group.header.version != 3
        || group.header.owner > owner
        || !matches!(
            group.header.phase,
            Phase::Admitted | Phase::Settling | Phase::Closed
        )
        || batch != group.batch_id()
        || parse_uuid(&reference.physical_attempt)? != batch
    {
        return Err(CatalogError::Invalid(
            "覆盖引用不是原已执行physical attempt",
        ));
    }
    let plan = require_actual_at(tr, parent, &group).await?;
    let observed: Observed = native_decode(
        &tr.get(&key(parent, b'p', Some(id), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    observed.require_full_dp(&plan)?;
    let binding: BatchBinding = native_decode(
        &tr.get(&key(parent, b'b', Some(id), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let identity = IndexSourceIdentity::from_bytes(&observed.source).map_err(cache)?;
    let head = ActiveIndexIdentity::from(identity.head());
    require_checkpoint_at(tr, parent.namespace(), batch, binding.manifest, head, None).await?;
    let member = group
        .members
        .get(usize::from(reference.ordinal))
        .ok_or(CatalogError::Incomplete)?;
    if member.body_uid != uid || member.inventory != inventory {
        return Err(CatalogError::Invalid("分组覆盖改变原source UID/inventory"));
    }
    let prepared =
        original_member_receipt_at(tr, parent, owner, &group, &plan, reference.ordinal).await?;
    if reference_at(
        &group,
        &plan,
        &observed,
        reference.ordinal,
        &prepared.receipt,
    )? != *reference
    {
        return Err(CatalogError::Corrupt(
            "分组覆盖原plan/Observed/receipt引用不匹配",
        ));
    }
    require_publication_at(tr, batch, &prepared.receipt, head).await?;
    let original = original_child_at(tr, parent, owner, member).await?;
    let stored;
    let pins = match mode {
        GroupCoverageMode::Stored => {
            stored = group_retirement::from_stored_at(
                tr,
                parent,
                owner,
                &group,
                &plan,
                &observed,
                reference.ordinal,
            )
            .await?;
            &stored
        }
        GroupCoverageMode::Staged(pins) => pins,
    };
    let destination = retirement::retired_group_destination_at(
        tr,
        parent,
        owner,
        &member.file,
        &original.request(),
        pins,
    )
    .await?;
    if destination.index() != head
        || destination.copy().volume_id != prepared.receipt.volume_id
        || destination.copy().ltfs_file_uid != prepared.receipt.ltfs_file_uid
        || destination.copy().content_digest != prepared.receipt.content_digest
        || destination.copy().size != prepared.receipt.size
        || destination.head().copy_id != prepared.receipt.copy_id
    {
        return Err(CatalogError::Invalid("分组覆盖缺少原Rv/精确退休目标事实"));
    }
    source::sealed_entry_node_at(
        tr,
        parent,
        &plan.intent.members[usize::from(reference.ordinal)].source,
    )
    .await
}

impl ManagedCatalog {
    #[cfg(test)]
    pub(crate) fn fail_reclaim_settlement_after_rv_once(
        &self,
        parent: &ReclaimReservation,
        ordinal: u8,
    ) -> ReclaimSettlementAfterRvFault {
        test_hook::arm(parent, ordinal)
    }
    #[cfg(test)]
    pub(crate) async fn check_stored_reclaim_group_coverage_for_test(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        ordinal: u8,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let group = evidence_group_at(&tr, parent, id).await?;
        let plan = require_actual_at(&tr, parent, &group).await?;
        let observed: Observed = native_decode(
            &tr.get(&key(parent, b'p', Some(id), None), false)
                .await?
                .ok_or(CatalogError::Incomplete)?,
        )?;
        observed.require_full_dp(&plan)?;
        let prepared =
            original_member_receipt_at(&tr, parent, owner, &group, &plan, ordinal).await?;
        let reference = reference_at(&group, &plan, &observed, ordinal, &prepared.receipt)?;
        let member = group
            .members
            .get(usize::from(ordinal))
            .ok_or(CatalogError::Incomplete)?;
        group_member_coverage_at(
            &tr,
            parent,
            owner,
            member.inventory,
            member.body_uid,
            &reference,
        )
        .await?;
        Ok(())
    }
    pub(crate) async fn settle_reclaim_group_member(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        group_id: Uuid,
        attempt: Uuid,
        ordinal: u8,
        readback: &crate::daemon::fdb_read::ReclaimGroupMemberReadback,
    ) -> Result<walk::ReclaimWalk> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let context = readback.context();
                            require_readback_binding(
                                parent, owner, group_id, attempt, ordinal, readback,
                            )?;
                            parent_at(&tr, parent, owner).await?;
                            let mut group = evidence_group_at(&tr, parent, group_id).await?;
                            isolation::require_group_action_at(&tr, parent, owner, &group).await?;
                            let plan = require_actual_at(&tr, parent, &group).await?;
                            let observed: Observed = native_decode(
                                &tr.get(&key(parent, b'p', Some(group_id), None), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?,
                            )?;
                            observed.require_full_dp(&plan)?;
                            let reference =
                                reference_at(&group, &plan, &observed, ordinal, context.receipt())?;
                            if reference.physical_attempt != attempt.to_string()
                                || reference.batch != context.batch_id().to_string()
                                || reference.plan != context.plan_digest()
                                || reference.observation != context.observation_digest()
                            {
                                return Err(CatalogError::Invalid(
                                    "分组结清改变原attempt/plan/Observed",
                                ));
                            }
                            let member = group
                                .members
                                .get(usize::from(ordinal))
                                .ok_or(CatalogError::Incomplete)?
                                .clone();
                            if let Some(settlement) = member.settlement {
                                let digest = source::group_bundle_at(
                                    &tr,
                                    parent,
                                    owner,
                                    member.body_uid,
                                    &reference,
                                )
                                .await?;
                                if digest != settlement {
                                    return Err(CatalogError::Corrupt("原成员结清bundle摘要不同"));
                                }
                                return walk::current_state_at(&tr, parent).await;
                            }
                            if !matches!(group.header.phase, Phase::Admitted | Phase::Settling) {
                                return Err(CatalogError::Invalid("只能结清原已准入成员"));
                            }
                            member_context::require_immutable_at(&tr, context).await?;
                            let original = original_child_at(&tr, parent, owner, &member).await?;
                            let proof_key = walk::verification_key(parent, &member.file);
                            let mut retired_key = proof_key.clone();
                            retired_key[domain(parent.namespace(), b'v').len() - 1] = b'x';
                            if tr.get(&proof_key, false).await?.is_some()
                                || tr.get(&retired_key, false).await?.is_some()
                            {
                                return Err(CatalogError::Corrupt("未结清成员出现半套分组bundle"));
                            }
                            source::require_group_absent_at(&tr, parent, member.body_uid).await?;
                            #[cfg(test)]
                            test_hook::before_verification_at(&tr, parent, owner, &group, ordinal)
                                .await?;
                            walk::record_group_verification_at(
                                &tr,
                                parent,
                                owner,
                                &member.file,
                                &original.request(),
                                context.metadata(),
                                context.head(),
                            )
                            .await?;
                            #[cfg(test)]
                            test_hook::after_rv_at(&tr, parent, ordinal).await?;
                            let pins = group_retirement::from_readback_at(
                                &tr, parent, owner, &group, &plan, &observed, readback,
                            )
                            .await?;
                            retirement::retire_group_member_at(
                                &tr,
                                parent,
                                owner,
                                &member.file,
                                &original.request(),
                                &pins,
                            )
                            .await?;
                            let node = source::sealed_entry_node_at(
                                &tr,
                                parent,
                                &plan.intent.members[usize::from(ordinal)].source,
                            )
                            .await?;
                            let settlement =
                                source::put_group_at(&tr, parent, owner, node, &reference, &pins)
                                    .await?;
                            group.members[usize::from(ordinal)].settlement = Some(settlement);
                            group.header.phase = Phase::Settling;
                            group.header.commit = Some(digest(
                                b"tape-rs/reclaim-group-observed-commit/2",
                                &observed,
                            )?);
                            save_header(&tr, parent, &group)?;
                            tr.set(
                                &key(parent, b'm', Some(group_id), Some(ordinal)),
                                &encode(&group.members[usize::from(ordinal)])?,
                            );
                            let progress =
                                walk::count_group_member_at(&tr, parent, &member.file).await?;
                            tr.clear(&children::history_pin_key(&original.request()));
                            Ok(progress)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}
