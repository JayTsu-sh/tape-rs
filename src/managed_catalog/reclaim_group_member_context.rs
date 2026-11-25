//! Original target facts for one guarded byte read, never a byte-read receipt.
use super::*;

pub(crate) struct ReclaimGroupMemberReadContext {
    scope: ReclaimGroupObservation,
    owner: u64,
    group: Uuid,
    attempt: Uuid,
    batch: Uuid,
    ordinal: u8,
    facts: MemberReadFacts,
}

#[derive(PartialEq, Eq)]
struct MemberReadFacts {
    plan: [u8; 32],
    observation: [u8; 32],
    request: ActiveFileRequest,
    receipt: CommitReceipt,
    metadata: ReadMetadata,
    head: ActiveIndexIdentity,
    source_identity: IndexSourceIdentity,
    physical_path: String,
    donor: FileNode,
    source_md5: Option<[u8; 16]>,
}
impl ReclaimGroupMemberReadContext {
    pub(crate) fn scope(&self) -> &ReclaimGroupObservation {
        &self.scope
    }
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }
    pub(crate) fn group_id(&self) -> Uuid {
        self.group
    }
    pub(crate) fn attempt_id(&self) -> Uuid {
        self.attempt
    }
    pub(crate) fn batch_id(&self) -> Uuid {
        self.batch
    }
    pub(crate) fn ordinal(&self) -> u8 {
        self.ordinal
    }
    pub(crate) fn plan_digest(&self) -> [u8; 32] {
        self.facts.plan
    }
    pub(crate) fn observation_digest(&self) -> [u8; 32] {
        self.facts.observation
    }
    pub(crate) fn metadata(&self) -> &ReadMetadata {
        &self.facts.metadata
    }
    pub(crate) fn request(&self) -> &ActiveFileRequest {
        &self.facts.request
    }
    pub(crate) fn receipt(&self) -> &CommitReceipt {
        &self.facts.receipt
    }
    pub(crate) fn head(&self) -> ActiveIndexIdentity {
        self.facts.head
    }
    pub(crate) fn source_identity(&self) -> &IndexSourceIdentity {
        &self.facts.source_identity
    }
    pub(crate) fn physical_path(&self) -> &str {
        &self.facts.physical_path
    }
    pub(crate) fn donor(&self) -> &FileNode {
        &self.facts.donor
    }
    pub(crate) fn source_md5(&self) -> Option<[u8; 16]> {
        self.facts.source_md5
    }

    pub(crate) fn require_capture(&self, capture: &CapturedIndexSource) -> Result<()> {
        if capture.identity().map_err(cache)?.to_bytes() != self.source_identity().to_bytes()
            || capture
                .entry_by_path(self.physical_path().trim_start_matches('/'))
                .map_err(cache)?
                != Some(IndexEntry::File {
                    path: self.physical_path().trim_start_matches('/').into(),
                    node: self.donor().clone(),
                })
        {
            return Err(CatalogError::Invalid(
                "回收正文回读必须使用原共享最终捕获与实际donor",
            ));
        }
        Ok(())
    }
}

struct GroupReadFacts {
    batch: Uuid,
    plan: ReclaimIndexAugmentation,
    plan_digest: [u8; 32],
    observation_digest: [u8; 32],
    head: ActiveIndexIdentity,
    source_identity: IndexSourceIdentity,
}

// The scope was just checked in this transaction. Hold its common readback
// facts only until the current transaction ends.
async fn group_read_at(
    tr: &Transaction,
    scope: &ReclaimGroupObservation,
    attempt: Uuid,
) -> Result<GroupReadFacts> {
    let parent = scope.parent();
    let group = scope.group();
    // The first admitted physical attempt uses the original batch. Linked
    // successor attempts require the explicit durable attempt resolver.
    let batch = parse_uuid(&group.header.batch)?;
    if attempt != batch {
        return Err(CatalogError::Invalid("正文回读不能替换原physical attempt"));
    }
    let binding_read = tr.get(&key(parent, b'b', Some(group.id()), None), false);
    let observed_read = tr.get(&key(parent, b'p', Some(group.id()), None), false);
    let plan = require_actual_at(tr, parent, group).await?;
    let binding: BatchBinding =
        native_decode(&binding_read.await?.ok_or(CatalogError::Incomplete)?)?;
    let observed: Observed = native_decode(&observed_read.await?.ok_or(CatalogError::Incomplete)?)?;
    observed.require_full_dp(&plan)?;
    let source_identity = IndexSourceIdentity::from_bytes(&observed.source).map_err(cache)?;
    let head = ActiveIndexIdentity::from(source_identity.head());
    require_checkpoint_at(tr, parent.namespace(), batch, binding.manifest, head, None).await?;
    Ok(GroupReadFacts {
        batch,
        plan_digest: plan.fingerprint()?,
        observation_digest: digest(b"tape-rs/reclaim-index-observed/1", &observed)?,
        head,
        source_identity,
        plan,
    })
}

async fn member_read_at(
    tr: &Transaction,
    scope: &ReclaimGroupObservation,
    shared: &GroupReadFacts,
    ordinal: u8,
) -> Result<MemberReadFacts> {
    let parent = scope.parent();
    let owner = scope.owner();
    let group = scope.group();
    let plan = &shared.plan;
    let prepared = original_member_receipt_at(tr, parent, owner, group, plan, ordinal).await?;
    let member = group
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let actual = plan
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let native = plan
        .intent
        .members
        .get(usize::from(ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let path = pool_catalog_path(parent.spec().pool(), member.file.path().as_bytes())?;
    let receipt = &prepared.receipt;
    let ((), original, resolved, (scan, current)) = tokio::try_join!(
        require_publication_at(tr, shared.batch, receipt, shared.head),
        original_child_at(tr, parent, owner, member),
        lookup::resolve_active_path_at_mode(
            tr,
            parent.namespace(),
            lookup::path_components(&path)?,
            true,
        ),
        active_volume::observed_at(tr, parent.serial(), receipt.volume_id),
    )?;
    let Some(ResolvedPath::File {
        id: target_id,
        head: target_head,
        copy,
        placement,
        attributes,
        ..
    }) = resolved
    else {
        return Err(CatalogError::Incomplete);
    };
    if target_id != receipt.file_id
        || target_head.version
            != receipt
                .expected_version
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || target_head.copy_id != receipt.copy_id
        || target_head.size != receipt.size
        || copy.volume_id != receipt.volume_id
        || copy.volume_generation != receipt.volume_generation
        || copy.volume_scan_id != receipt.volume_scan_id
        || copy.ltfs_file_uid != receipt.ltfs_file_uid
        || copy.content_digest != receipt.content_digest
        || copy.size != receipt.size
        || attributes.as_ref().map(|a| a.digest()) != receipt.metadata_digest
    {
        return Err(CatalogError::Invalid(
            "原回收组正文回读目标不是原已发布成员副本",
        ));
    }
    if scan.id != receipt.volume_scan_id || current != shared.head {
        return Err(CatalogError::Invalid("原回收组正文回读目标卷头改变"));
    }
    let IndexEntry::File { node, .. } = &native.source else {
        return Err(CatalogError::Incomplete);
    };
    Ok(MemberReadFacts {
        plan: shared.plan_digest,
        observation: shared.observation_digest,
        request: original.request(),
        receipt: prepared.receipt,
        metadata: ReadMetadata {
            id: target_id,
            head: target_head,
            copy: *copy,
            placement,
            attributes,
        },
        head: shared.head,
        source_identity: shared.source_identity.clone(),
        physical_path: actual.donor_path.clone(),
        donor: actual.donor.clone(),
        source_md5: source::native_file_md5(node)?,
    })
}

async fn at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: Uuid,
    attempt: Uuid,
    ordinal: u8,
) -> Result<ReclaimGroupMemberReadContext> {
    let scope = group_observation::at(tr, parent, owner, group).await?;
    let shared = group_read_at(tr, &scope, attempt).await?;
    let facts = member_read_at(tr, &scope, &shared, ordinal).await?;
    Ok(ReclaimGroupMemberReadContext {
        scope,
        owner,
        group,
        attempt,
        batch: shared.batch,
        ordinal,
        facts,
    })
}
impl ManagedCatalog {
    pub(crate) async fn reclaim_group_member_read_context(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        group: Uuid,
        attempt: Uuid,
        ordinal: u8,
    ) -> Result<ReclaimGroupMemberReadContext> {
        let tr = self.db.create_trx()?;
        at(&tr, parent, owner, group, attempt, ordinal).await
    }
    /// Original source/target/plan/owner facts share this fresh read version.
    pub(crate) async fn check_reclaim_group_member_read_context(
        &self,
        context: &ReclaimGroupMemberReadContext,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        context.scope.require_at(&tr).await?;
        let shared = group_read_at(&tr, &context.scope, context.attempt).await?;
        require_member_read_at(&tr, &context.scope, &shared, context).await
    }

    /// Final byte-token sealing checks the full original cohort at one RV.
    pub(crate) async fn check_reclaim_group_member_read_contexts(
        &self,
        contexts: &[ReclaimGroupMemberReadContext],
    ) -> Result<()> {
        let first = contexts.first().ok_or(CatalogError::Incomplete)?;
        if contexts.len() != first.scope.group().members.len() {
            return Err(CatalogError::Invalid("最终正文证明必须覆盖完整原成员集合"));
        }
        let mut ordinals = std::collections::BTreeSet::new();
        let tr = self.db.create_trx()?;
        for (position, context) in contexts.iter().enumerate() {
            // Checking the exact common scope once is equivalent to
            // checking every member's scope at this same read version.
            if context.scope.parent().intent() != first.scope.parent().intent()
                || context.scope.parent().route != first.scope.parent().route
                || context.scope.group().header != first.scope.group().header
                || context.scope.group().members != first.scope.group().members
                || context.scope.intent().intent != first.scope.intent().intent
                || context.scope.intent().members != first.scope.intent().members
                || first.scope.group().header.order.get(position) != Some(&context.ordinal)
                || context.owner != first.owner
                || context.group != first.group
                || context.attempt != first.attempt
                || context.batch != first.batch
                || context.plan_digest() != first.plan_digest()
                || context.observation_digest() != first.observation_digest()
                || context.head() != first.head()
                || !ordinals.insert(context.ordinal)
            {
                return Err(CatalogError::Invalid(
                    "最终正文证明改变原组/attempt/顺序集合",
                ));
            }
        }
        if ordinals
            != (0..u8::try_from(contexts.len()).map_err(|_| CatalogError::Incomplete)?).collect()
        {
            return Err(CatalogError::Invalid("最终正文证明成员ordinal不完整"));
        }
        first.scope.require_at(&tr).await?;
        let shared = group_read_at(&tr, &first.scope, first.attempt).await?;
        for context in contexts {
            require_member_read_at(&tr, &first.scope, &shared, context).await?;
        }
        Ok(())
    }
}

async fn require_member_read_at(
    tr: &Transaction,
    scope: &ReclaimGroupObservation,
    shared: &GroupReadFacts,
    context: &ReclaimGroupMemberReadContext,
) -> Result<()> {
    if shared.batch != context.batch
        || member_read_at(tr, scope, shared, context.ordinal).await? != context.facts
    {
        return Err(CatalogError::Invalid("原回收组正文回读依据改变"));
    }
    Ok(())
}

/// Settlement reopens current authority and compares immutable read bindings.
/// Prior members may legitimately have new settlement hashes; their old live
/// Observe snapshot is not reused as authority for this transaction.
pub(super) async fn require_immutable_at(
    tr: &Transaction,
    context: &ReclaimGroupMemberReadContext,
) -> Result<()> {
    let current = at(
        tr,
        context.scope.parent(),
        context.owner,
        context.group,
        context.attempt,
        context.ordinal,
    )
    .await?;
    if current.owner != context.owner
        || current.group != context.group
        || current.attempt != context.attempt
        || current.batch != context.batch
        || current.ordinal != context.ordinal
        || current.facts != context.facts
    {
        return Err(CatalogError::Invalid("原回收组正文回读依据改变"));
    }
    Ok(())
}
