//! Closing releases only the original pending pointers. Canonical S4/native
//! evidence remains addressable while later groups execute.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseSeal {
    version: u8,
    parent: String,
    group: String,
    sequence: u64,
    header: [u8; 32],
    members: Vec<[u8; 32]>,
    intent: [u8; 32],
    plan: [u8; 32],
    observation: [u8; 32],
    binding: [u8; 32],
    attempt: [u8; 32],
    previous_close: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_coverage: Option<[u8; 32]>,
}
async fn seal_value(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    previous_close: [u8; 32],
    closing: bool,
) -> Result<CloseSeal> {
    let plan = require_actual_at(tr, parent, group).await?;
    let observed: Observed = native_decode(
        &tr.get(&key(parent, b'p', Some(group.id()), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    observed.require_full_dp(&plan)?;
    let binding: BatchBinding = native_decode(
        &tr.get(&key(parent, b'b', Some(group.id()), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let members = group
        .members
        .iter()
        .map(|m| digest(b"tape-rs/reclaim-closed-member/2", m))
        .collect::<Result<Vec<_>>>()?;
    let root_coverage = if let Some((entry, reference)) = plan.root_reference(group, &observed)? {
        use source::GroupRootEvidence;
        let evidence = if closing {
            GroupRootEvidence::Closing
        } else if let Some(completed) = format::result_at(tr, parent).await? {
            let inventory = completed.source_inventory();
            if group.members.is_empty()
                || group
                    .members
                    .iter()
                    .any(|member| member.inventory != inventory)
            {
                return Err(CatalogError::Corrupt(
                    "原关闭组与回收完成回执的源清单不一致",
                ));
            }
            GroupRootEvidence::Completed(inventory)
        } else {
            GroupRootEvidence::Closed
        };
        Some(source::group_root_bundle_at(tr, parent, entry, &reference, evidence).await?)
    } else {
        None
    };
    Ok(CloseSeal {
        version: if root_coverage.is_some() { 4 } else { 3 },
        parent: parent.fingerprint(),
        group: group.header.group.clone(),
        sequence: group.header.sequence,
        header: digest(b"tape-rs/reclaim-closed-header/2", &group.header)?,
        members,
        intent: plan.intent.fingerprint()?,
        plan: plan.fingerprint()?,
        observation: digest(b"tape-rs/reclaim-index-observed/1", &observed)?,
        binding: digest(b"tape-rs/reclaim-closed-binding/2", &binding)?,
        attempt: group_attempt::fingerprint_at(tr,parent,group).await?,
        previous_close,
        root_coverage,
    })
}

/// Close validated the original canonical rows before atomic sealing. A
/// terminal format receipt independently binds the same original inventory
/// after cleanup has removed its temporary coverage ledger.
pub(super) async fn require_closed_evidence_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    let raw = tr
        .get(
            &group_layout::canonical_key(parent, group.id(), b'c', None),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let seal: CloseSeal = native_decode(&raw)?;
    if seal_value(tr, parent, group, seal.previous_close, false).await? != seal {
        return Err(CatalogError::Corrupt(
            "原closed组canonical seal与完整原证据不同",
        ));
    }
    group_layout::require_closed_sequence_at(tr, parent, group, &raw, seal.previous_close).await
}

impl ManagedCatalog {
    pub(crate) async fn close_reclaim_copy_group(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
    ) -> Result<ReclaimCopyGroup> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, parent, owner).await?;
                            let mut group = evidence_group_at(&tr, parent, id).await?;
                            isolation::require_group_action_at(&tr, parent, owner, &group).await?;
                            if group.header.phase == Phase::Closed {
                                // load_original validates c/q/z; never touch a later a/u.
                                return Ok(group);
                            }
                            if group.header.owner != owner || group.header.phase != Phase::Settling
                            {
                                return Err(CatalogError::Invalid(
                                    "只能关闭原owner已验证的Settling组",
                                ));
                            }
                            let current = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if current.id() != id {
                                return Err(CatalogError::Invalid("关闭不能替换原active组"));
                            }
                            let plan = require_actual_at(&tr, parent, &group).await?;
                            let observed: Observed = native_decode(
                                &tr.get(&key(parent, b'p', Some(id), None), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?,
                            )?;
                            observed.require_full_dp(&plan)?;
                            for member in &group.members {
                                let expected = member.settlement.ok_or(CatalogError::Incomplete)?;
                                let prepared = original_member_receipt_at(
                                    &tr,
                                    parent,
                                    owner,
                                    &group,
                                    &plan,
                                    member.ordinal,
                                )
                                .await?;
                                let reference = group_settlement::reference_at(
                                    &group,
                                    &plan,
                                    &observed,
                                    member.ordinal,
                                    &prepared.receipt,
                                )?;
                                if source::group_bundle_at(
                                    &tr,
                                    parent,
                                    owner,
                                    member.body_uid,
                                    &reference,
                                )
                                .await?
                                    != expected
                                {
                                    return Err(CatalogError::Corrupt(
                                        "关闭缺少原逐成员Rv/退休/Rq bundle",
                                    ));
                                }
                            }
                            let previous =
                                group_layout::previous_close_digest_at(&tr, parent).await?;
                            group.header.phase = Phase::Closed;
                            let seal = seal_value(&tr, parent, &group, previous, true).await?;
                            let raw = native_bytes(&seal)?;
                            group_layout::record_close_at(&tr, parent, &group, &raw, previous)
                                .await?;
                            Ok(group)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

/// Compact immutable manifests used during the complete raw-domain audit.
/// The issued phase separately validates all source/retirement/publication
/// semantics; this pass detects extra, missing and misrouted physical rows.
async fn compact_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    id: Uuid,
) -> Result<(ReclaimCopyGroup, CloseSeal)> {
    let raw = tr
        .get(&group_layout::canonical_key(parent, id, b'h', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let header: Header = decode(&raw)?;
    header.validate(parent)?;
    let group = ReclaimCopyGroup {
        header,
        members: Vec::new(),
    };
    let raw = tr
        .get(&group_layout::canonical_key(parent, id, b'c', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let seal: CloseSeal = native_decode(&raw)?;
    if seal.version != if parent.groups_archive_root() { 4 } else { 3 }
        || seal.root_coverage.is_some() != parent.groups_archive_root()
        || seal.parent != parent.fingerprint()
        || seal.group != id.to_string()
        || seal.sequence != group.header.sequence
        || seal.members.len() != usize::from(group.header.count)
        || seal.header != digest(b"tape-rs/reclaim-closed-header/2", &group.header)?
    {
        return Err(CatalogError::Corrupt(
            "audit canonical header/close manifest不同",
        ));
    }
    group_layout::require_identity_at(tr, parent, &group).await?;
    group_layout::require_closed_sequence_at(tr, parent, &group, &raw, seal.previous_close).await?;
    Ok((group, seal))
}
pub(super) async fn audit_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    id: Uuid,
) -> Result<(u64, Vec<u8>, [u8; 32])> {
    let group = evidence_group_at(tr, parent, id).await?;
    if group.header.phase != Phase::Closed || group.header.owner > owner {
        return Err(CatalogError::Invalid("audit原issued组未完整关闭"));
    }
    let plan = require_actual_at(tr, parent, &group).await?;
    let observed: Observed = native_decode(
        &tr.get(&key(parent, b'p', Some(id), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    observed.require_full_dp(&plan)?;
    for member in &group.members {
        let expected = member.settlement.ok_or(CatalogError::Incomplete)?;
        let prepared =
            original_member_receipt_at(tr, parent, owner, &group, &plan, member.ordinal).await?;
        let reference = group_settlement::reference_at(
            &group,
            &plan,
            &observed,
            member.ordinal,
            &prepared.receipt,
        )?;
        if source::group_bundle_at(tr, parent, owner, member.body_uid, &reference).await?
            != expected
        {
            return Err(CatalogError::Corrupt("audit缺原成员完整结清bundle"));
        }
    }
    let raw = tr
        .get(&group_layout::canonical_key(parent, id, b'c', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let seal: CloseSeal = native_decode(&raw)?;
    Ok((group.header.sequence, raw.to_vec(), seal.previous_close))
}
pub(super) async fn audit_row_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    tail: &[u8],
    raw: &[u8],
) -> Result<()> {
    let Some(kind) = tail.first().copied() else {
        return Err(CatalogError::Corrupt("空audit路由key"));
    };
    match kind {
        b'g' => {
            if tail.len() < 19 || tail[17] != b'/' {
                return Err(CatalogError::Corrupt("canonical group key长度"));
            }
            let id = Uuid::from_slice(&tail[1..17]).map_err(|_| CatalogError::Incomplete)?;
            let (group, seal) = compact_at(tr, parent, id).await?;
            let rest = &tail[18..];
            let (attempt, node) = if rest.first() == Some(&b't') {
                if rest.len() < 19 || rest[17] != b'/' {
                    return Err(CatalogError::Corrupt("canonical attempt key长度"));
                }
                let attempt =
                    Uuid::from_slice(&rest[1..17]).map_err(|_| CatalogError::Incomplete)?;
                if attempt != group.batch_id() {
                    return Err(CatalogError::Corrupt("audit出现未登记physical attempt"));
                }
                (true, &rest[18..])
            } else {
                (false, rest)
            };
            match (attempt, node) {
                (false, [b'h']) => {
                    if digest(b"tape-rs/reclaim-closed-header/2", &decode::<Header>(raw)?)?
                        != seal.header
                    {
                        return Err(CatalogError::Corrupt("audit header摘要"));
                    }
                }
                (false, [b'm', ordinal]) => {
                    let row: Member = decode(raw)?;
                    row.validate(parent)?;
                    if row.ordinal != *ordinal
                        || row.group != group.header.group
                        || seal.members.get(usize::from(*ordinal))
                            != Some(&digest(b"tape-rs/reclaim-closed-member/2", &row)?)
                    {
                        return Err(CatalogError::Corrupt("audit member摘要或ordinal"));
                    }
                }
                (false, [b'i']) => {
                    if digest(
                        b"tape-rs/reclaim-index-intent/1",
                        &decode_index_intent(raw)?,
                    )? != seal.intent
                    {
                        return Err(CatalogError::Corrupt("audit native intent摘要"));
                    }
                }
                (false, [b'n', ordinal]) => {
                    let intent: Intent = decode_index_intent(
                        &tr.get(&key(parent, b'i', Some(id), None), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?,
                    )?;
                    let row: NativeMember = native_decode(raw)?;
                    if row.ordinal != *ordinal
                        || intent.payloads.get(usize::from(*ordinal))
                            != Some(&payload_digest(&row)?)
                    {
                        return Err(CatalogError::Corrupt("audit native payload摘要或ordinal"));
                    }
                }
                (false, [b'c']) => {
                    let close: CloseSeal = native_decode(raw)?;
                    if close != seal {
                        return Err(CatalogError::Corrupt("audit close seal改变"));
                    }
                }
                (true, [b'w']) => {
                    if group_attempt::raw_fingerprint(raw)?!=seal.attempt {
                        return Err(CatalogError::Corrupt("audit physical attempt摘要"));
                    }
                }
                (true, [b'a']) => {
                    if digest(
                        b"tape-rs/reclaim-index-actual/1",
                        &native_decode::<Actual>(raw)?,
                    )? != seal.plan
                    {
                        return Err(CatalogError::Corrupt("audit Actual摘要"));
                    }
                }
                (true, [b'd', ordinal]) => {
                    let actual: Actual = native_decode(
                        &tr.get(&key(parent, b'a', Some(id), None), false)
                            .await?
                            .ok_or(CatalogError::Incomplete)?,
                    )?;
                    let row: ActualMember = native_decode(raw)?;
                    if row.ordinal != *ordinal
                        || actual.members.get(usize::from(*ordinal))
                            != Some(&digest(b"tape-rs/reclaim-index-actual-member/1", &row)?)
                    {
                        return Err(CatalogError::Corrupt("audit donor摘要或ordinal"));
                    }
                }
                (true, [b'p']) => {
                    native_decode::<Observed>(raw)?.require_raw_full_dp()?;
                    if digest(
                        b"tape-rs/reclaim-index-observed/1",
                        &native_decode::<Observed>(raw)?,
                    )? != seal.observation
                    {
                        return Err(CatalogError::Corrupt("audit Observed摘要"));
                    }
                }
                (true, [b'b']) => {
                    if digest(
                        b"tape-rs/reclaim-closed-binding/2",
                        &native_decode::<BatchBinding>(raw)?,
                    )? != seal.binding
                    {
                        return Err(CatalogError::Corrupt("audit BatchBinding摘要"));
                    }
                }
                _ => {
                    return Err(CatalogError::Corrupt(
                        "audit canonical域出现未知kind/ordinal",
                    ));
                }
            }
        }
        b'r' => {
            if tail.len() != 17 || raw.len() != 17 {
                return Err(CatalogError::Corrupt("audit reverse route长度"));
            }
            let operation = Uuid::from_slice(&tail[1..]).map_err(|_| CatalogError::Incomplete)?;
            let id = Uuid::from_slice(&raw[..16]).map_err(|_| CatalogError::Incomplete)?;
            let ordinal = raw[16];
            let (group, seal) = compact_at(tr, parent, id).await?;
            let row: Member = decode(
                &tr.get(&key(parent, b'm', Some(id), Some(ordinal)), false)
                    .await?
                    .ok_or(CatalogError::Incomplete)?,
            )?;
            if row.file.operation() != operation
                || row.ordinal != ordinal
                || row.group != group.header.group
                || seal.members.get(usize::from(ordinal))
                    != Some(&digest(b"tape-rs/reclaim-closed-member/2", &row)?)
            {
                return Err(CatalogError::Corrupt("audit orphan reverse route"));
            }
        }
        b'b' => {
            if tail.len() != 17 {
                return Err(CatalogError::Corrupt("audit batch route长度"));
            }
            let batch = Uuid::from_slice(&tail[1..]).map_err(|_| CatalogError::Incomplete)?;
            let route: BatchRoute = native_decode(raw)?;
            let id = parse_uuid(&route.group)?;
            let (group, seal) = compact_at(tr, parent, id).await?;
            let binding: BatchBinding = native_decode(
                &tr.get(&key(parent, b'b', Some(id), None), false)
                    .await?
                    .ok_or(CatalogError::Incomplete)?,
            )?;
            if route.version != 2
                || route.parent != *parent.intent()
                || route.batch != batch.to_string()
                || route.physical_attempt != group.header.batch
                || batch != group.batch_id()
                || route.plan != seal.plan
                || route.manifest != binding.manifest
                || tr
                    .get(&batch_route_key(parent.namespace(), batch), false)
                    .await?
                    .as_deref()
                    != Some(raw)
            {
                return Err(CatalogError::Corrupt("audit orphan batch route"));
            }
        }
        _ => return Err(CatalogError::Corrupt("audit出现未知路由域")),
    }
    Ok(())
}
