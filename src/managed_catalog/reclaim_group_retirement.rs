//! Source history-pin exceptions derived from one original group transaction.
//! No token grants device I/O or formatting, and no builder calls coverage.
use super::*;

pub(in crate::managed_catalog) struct GroupRetirementPins<'tr> {
    transaction: &'tr Transaction,
    parent: ReclaimReservation,
    owner: u64,
    group: Uuid,
    attempt: Uuid,
    ordinal: u8,
    file: ReclaimFile,
    request: ActiveFileRequest,
    rv: Vec<u8>,
    pins: std::collections::BTreeSet<Vec<u8>>,
}
impl GroupRetirementPins<'_> {
    pub(in crate::managed_catalog) fn namespace(&self) -> Uuid {
        self.parent.namespace()
    }
    pub(in crate::managed_catalog) fn source(&self) -> Uuid {
        self.parent.spec().volume()
    }
    pub(in crate::managed_catalog) fn keys(&self) -> &std::collections::BTreeSet<Vec<u8>> {
        &self.pins
    }
    pub(in crate::managed_catalog) fn require_transaction(&self, tr: &Transaction) -> Result<()> {
        if !std::ptr::eq(self.transaction, tr) {
            return Err(CatalogError::Invalid("退休pin证明属于其他FDB事务"));
        }
        Ok(())
    }
    pub(in crate::managed_catalog) async fn require_binding(
        &self,
        tr: &Transaction,
        parent: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        self.require_transaction(tr)?;
        if self.parent.intent() != parent.intent()
            || self.owner != owner
            || self.file != *file
            || self.request != *request
        {
            return Err(CatalogError::Invalid(
                "退休pin证明改变原source/member/request/owner",
            ));
        }
        if record_at(tr, parent, owner).await?["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let group = evidence_group_at(tr, parent, self.group).await?;
        isolation::require_group_action_at(tr, parent, owner, &group).await?;
        let member = group
            .members
            .get(usize::from(self.ordinal))
            .ok_or(CatalogError::Incomplete)?;
        if parse_uuid(&group.header.batch)? != self.attempt
            || member.file != self.file
            || original_child_at(tr, parent, owner, member)
                .await?
                .request()
                != self.request
            || tr
                .get(&walk::verification_key(parent, file), false)
                .await?
                .as_deref()
                != Some(self.rv.as_slice())
        {
            return Err(CatalogError::Corrupt("退休pin证明原child/attempt/Rv改变"));
        }
        Ok(())
    }
}

fn require_destination(
    destination: &ReclaimBodyDestination,
    receipt: &CommitReceipt,
    head: ActiveIndexIdentity,
) -> Result<()> {
    if destination.index() != head
        || destination.copy().volume_id != receipt.volume_id
        || destination.copy().volume_generation != receipt.volume_generation
        || destination.copy().volume_scan_id != receipt.volume_scan_id
        || destination.copy().ltfs_file_uid != receipt.ltfs_file_uid
        || destination.copy().size != receipt.size
        || destination.copy().content_digest != receipt.content_digest
        || destination.head().copy_id != receipt.copy_id
    {
        return Err(CatalogError::Corrupt(
            "退休pin证明目标不是原published receipt",
        ));
    }
    Ok(())
}
async fn require_member_target_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    member: &Member,
    receipt: &CommitReceipt,
) -> Result<()> {
    let path = pool_catalog_path(parent.spec().pool(), member.file.path().as_bytes())?;
    let Some(ResolvedPath::File {
        id,
        head,
        copy,
        attributes,
        placement,
        ..
    }) = lookup::resolve_active_path_at_mode(
        tr,
        parent.namespace(),
        lookup::path_components(&path)?,
        true,
    )
    .await?
    else {
        return Err(CatalogError::Incomplete);
    };
    if id != receipt.file_id
        || head.copy_id != receipt.copy_id
        || head.version
            != receipt
                .expected_version
                .and_then(|v| v.checked_add(1))
                .ok_or(CatalogError::Incomplete)?
        || head.size != receipt.size
        || copy.version != head.version
        || copy.volume_id != receipt.volume_id
        || copy.volume_generation != receipt.volume_generation
        || copy.volume_scan_id != receipt.volume_scan_id
        || copy.ltfs_file_uid != receipt.ltfs_file_uid
        || copy.size != receipt.size
        || copy.content_digest != receipt.content_digest
        || attributes.as_ref().map(|a| a.digest()) != receipt.metadata_digest
        || placement.namespace != parent.namespace()
        || placement.pool_id != parent.spec().pool()
        || placement.library_serial != parent.serial()
    {
        return Err(CatalogError::Corrupt(
            "退休pin证明原target metadata/head/copy改变",
        ));
    }
    Ok(())
}
async fn require_pending_source_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    member: &Member,
    original: &ActiveFileIntent,
) -> Result<()> {
    let raw = tr
        .get(
            &children::child_key(parent.namespace(), &parent.token(), original.operation_id)?,
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let child: Value = super::super::super::super::decode(&raw)?;
    let source_raw: Vec<u8> =
        serde_json::from_value(child["source_copy"].clone()).map_err(cache)?;
    let source_head = decode_head(
        &serde_json::from_value::<Vec<u8>>(child["source_head"].clone()).map_err(cache)?,
    )?;
    let source = copy_records::decode_copy(&source_raw)?;
    let (scan, _) = source_state_at(tr, parent).await?;
    if source_head.version != member.file.version()
        || source.version != source_head.version
        || source.volume_id != parent.spec().volume()
        || source.volume_scan_id != scan.id
        || source.size != member.file.size()
        || tr
            .get(
                &copy_key(
                    parent.namespace(),
                    member.file.file_id(),
                    source_head.copy_id,
                ),
                false,
            )
            .await?
            .as_deref()
            != Some(source_raw.as_slice())
    {
        return Err(CatalogError::Corrupt("未结清成员原source copy已改变"));
    }
    let reverse = tr
        .get(
            &volume_key(
                source.volume_id,
                source.volume_scan_id,
                parent.namespace(),
                member.file.file_id(),
                source_head.copy_id,
            ),
            false,
        )
        .await?;
    if !reverse
        .as_deref()
        .is_some_and(|v| v.len() == 40 && read_u64(v) == source.version)
    {
        return Err(CatalogError::Corrupt("未结清成员原source倒排已改变"));
    }
    Ok(())
}

/// Build from canonical facts, checking already-settled members without
/// recursively invoking GroupFile coverage or retired_destination wrappers.
async fn pins_at<'tr>(
    tr: &'tr Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
    plan: &ReclaimIndexAugmentation,
    observed: &Observed,
    current: u8,
) -> Result<GroupRetirementPins<'tr>> {
    if parent.intent()["version"] != 2
        || record_at(tr, parent, owner).await?["owner_epoch"] != owner
    {
        return Err(CatalogError::OwnerFenced);
    }
    require_parent_pool_at(tr, parent).await?;
    source_state_at(tr, parent).await?;
    let canonical = evidence_group_at(tr, parent, group.id()).await?;
    if canonical.header != group.header
        || canonical.members != group.members
        || group.header.owner > owner
        || !matches!(
            group.header.phase,
            Phase::Admitted | Phase::Settling | Phase::Closed
        )
    {
        return Err(CatalogError::Invalid("退休pin证明需要原已执行持久组"));
    }
    let current_member = group
        .members
        .get(usize::from(current))
        .ok_or(CatalogError::Incomplete)?;
    let current_request = original_child_at(tr, parent, owner, current_member)
        .await?
        .request();
    let rv = tr
        .get(&walk::verification_key(parent, &current_member.file), false)
        .await?
        .ok_or(CatalogError::Incomplete)?
        .to_vec();
    let identity = IndexSourceIdentity::from_bytes(&observed.source).map_err(cache)?;
    let head = ActiveIndexIdentity::from(identity.head());
    let mut pins = std::collections::BTreeSet::new();
    for member in &group.members {
        let reverse = [group.id().as_bytes().as_slice(), &[member.ordinal]].concat();
        if tr
            .get(
                &key(parent, b'r', Some(member.file.operation()), None),
                false,
            )
            .await?
            .as_deref()
            != Some(reverse.as_slice())
        {
            return Err(CatalogError::Corrupt("退休pin证明成员反向身份改变"));
        }
        let original = original_child_at(tr, parent, owner, member).await?;
        let prepared =
            original_member_receipt_at(tr, parent, owner, group, plan, member.ordinal).await?;
        require_publication_at(tr, group.batch_id(), &prepared.receipt, head).await?;
        require_member_target_at(tr, parent, member, &prepared.receipt).await?;
        let node = source::sealed_entry_node_at(
            tr,
            parent,
            &plan.intent.members[usize::from(member.ordinal)].source,
        )
        .await?;
        if node.uid() != member.body_uid {
            return Err(CatalogError::Corrupt("退休pin证明原source UID改变"));
        }
        let pin = children::history_pin_key(&original.request());
        if let Some(settlement) = member.settlement {
            if tr.get(&pin, false).await?.is_some() {
                return Err(CatalogError::Corrupt("已结清成员仍有历史pin"));
            }
            let reference = reference_at(group, plan, observed, member.ordinal, &prepared.receipt)?;
            if source::group_bundle_identity_at(tr, parent, owner, member.body_uid, &reference)
                .await?
                != settlement
            {
                return Err(CatalogError::Corrupt("已结清成员完整Rq摘要改变"));
            }
            let (destination, pending) = retirement::group_destination_identity_at(
                tr,
                parent,
                owner,
                &member.file,
                &original.request(),
            )
            .await?;
            if pending {
                return Err(CatalogError::Corrupt("已结清成员缺原退休marker/Rx"));
            }
            require_destination(&destination, &prepared.receipt, head)?;
        } else {
            if tr.get(&pin, false).await?.as_deref() != Some(&[][..]) || !pins.insert(pin) {
                return Err(CatalogError::Corrupt("未结清成员历史pin缺失、非空或重复"));
            }
            let proof = walk::verification_key(parent, &member.file);
            let mut retired = proof.clone();
            retired[domain(parent.namespace(), b'v').len() - 1] = b'x';
            source::require_group_absent_at(tr, parent, member.body_uid).await?;
            if member.ordinal == current {
                // The only unstored Rv admitted here was just generated by
                // the sealed-byte caller in this very transaction.
                if tr.get(&retired, false).await?.is_some() {
                    return Err(CatalogError::Corrupt("staged退休已出现半套Rx"));
                }
                let (destination, pending) = retirement::group_destination_identity_at(
                    tr,
                    parent,
                    owner,
                    &member.file,
                    &original.request(),
                )
                .await?;
                if !pending {
                    return Err(CatalogError::Corrupt("staged成员已缺原source"));
                }
                require_destination(&destination, &prepared.receipt, head)?;
            } else if tr.get(&proof, false).await?.is_some()
                || tr.get(&retired, false).await?.is_some()
            {
                return Err(CatalogError::Corrupt("其他未结清成员出现半套Rv/Rx"));
            }
            require_pending_source_at(tr, parent, member, &original).await?;
        }
    }
    Ok(GroupRetirementPins {
        transaction: tr,
        parent: parent.clone(),
        owner,
        group: group.id(),
        attempt: parse_uuid(&group.header.batch)?,
        ordinal: current,
        file: current_member.file.clone(),
        request: current_request,
        rv,
        pins,
    })
}

pub(super) async fn from_readback_at<'tr>(
    tr: &'tr Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
    plan: &ReclaimIndexAugmentation,
    observed: &Observed,
    readback: &crate::daemon::fdb_read::ReclaimGroupMemberReadback,
) -> Result<GroupRetirementPins<'tr>> {
    let context = readback.context();
    require_readback_binding(
        parent,
        owner,
        group.id(),
        parse_uuid(&group.header.batch)?,
        context.ordinal(),
        readback,
    )?;
    member_context::require_immutable_at(tr, context).await?;
    if group
        .members
        .get(usize::from(context.ordinal()))
        .ok_or(CatalogError::Incomplete)?
        .settlement
        .is_some()
        || reference_at(group, plan, observed, context.ordinal(), context.receipt())?.plan
            != context.plan_digest()
    {
        return Err(CatalogError::Invalid(
            "退休写token必须消费原未结清sealed字节证明",
        ));
    }
    pins_at(tr, parent, owner, group, plan, observed, context.ordinal()).await
}
pub(super) async fn from_stored_at<'tr>(
    tr: &'tr Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
    plan: &ReclaimIndexAugmentation,
    observed: &Observed,
    ordinal: u8,
) -> Result<GroupRetirementPins<'tr>> {
    if group
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?
        .settlement
        .is_none()
    {
        return Err(CatalogError::Corrupt(
            "持久分组覆盖缺少完整结清bundle，不能补半套",
        ));
    }
    pins_at(tr, parent, owner, group, plan, observed, ordinal).await
}
