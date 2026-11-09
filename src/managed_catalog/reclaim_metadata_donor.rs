//! Fresh donor evidence after a settled recovery. Original body/retirement
//! records are immutable; a later head gets its own independently read proof.
use super::*;

/// A same-version FDB read context, not permission to access a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataDonorContext {
    task: ReclaimMetadataWrite,
    input: ReclaimMetadataInput,
    current: ActiveIndexIdentity,
    scan: Uuid,
    files: u64,
}
impl ReclaimMetadataDonorContext {
    pub fn input(&self) -> &ReclaimMetadataInput {
        &self.input
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub(crate) fn task(&self) -> &ReclaimMetadataWrite {
        &self.task
    }
    pub(crate) fn donor_uid(&self) -> u64 {
        self.task.0.plan.donor.uid
    }
    pub(crate) fn donor_path(&self) -> &str {
        &self.task.0.plan.donor.path
    }
    pub(crate) fn require_donor(
        &self,
        source: &CapturedIndexSource,
    ) -> Result<crate::ltfs::index::FileNode> {
        current_proof(source.scope(), source.header())?;
        if ActiveIndexIdentity::from(source.construct()) != self.current {
            return Err(CatalogError::Invalid("donor 实读偏离当前 FDB 卷头"));
        }
        let entry = source
            .entry_by_uid(self.donor_uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if Object::capture(entry.clone())? != self.task.0.plan.donor {
            return Err(CatalogError::Invalid("修复后的原 donor 节点已改变"));
        }
        let IndexEntry::File { node, .. } = entry else {
            return Err(CatalogError::Incomplete);
        };
        if node.symlink.is_some()
            || node.length != self.input.destination().copy().size
            || node
                .extents
                .iter()
                .any(|extent| extent.partition != self.current.location.partition)
            || ContentDigest::from_ltfs_xattrs(&node.xattrs)?
                != ContentDigest::Sha256(self.input.source().byte_proof().sha256())
        {
            return Err(CatalogError::Invalid("donor 不是原已核验的 DP 正文"));
        }
        Ok(node)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fact {
    pub owner: u64,
    pub attempt: String,
    pub recovery_epoch: u64,
    pub input: [u8; 32],
    pub index: Vec<u8>,
    pub identity: [u8; 32],
    pub dp_eod: u64,
    pub donor: [u8; 32],
    pub length: u64,
    pub sha256: [u8; 32],
    pub md5: Option<[u8; 16]>,
}
impl Fact {
    pub(super) fn require_plan(&self, plan: &ReclaimMetadataPlan) -> Result<()> {
        node_mutation::id(&self.attempt)?;
        if self.owner == 0
            || self.owner > 0xFFFF_FFFF_FFFF
            || self.recovery_epoch == 0
            || self.input != plan.0.input
            || self.index != plan.0.previous
            || self.identity != plan.previous_source()?.fingerprint()
            || self.dp_eod != plan.previous_dp_eod()
            || self.donor != <[u8; 32]>::from(Sha256::digest(plan.0.donor.encode()?))
            || Some(self.length) != plan.0.donor.length
        {
            return Err(CatalogError::Corrupt("后继计划与独立 donor 实读事实不同"));
        }
        Ok(())
    }
}

/// Produced only after uncached body I/O under the original owner/task guard.
/// Retained facts never restore this live capability.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimMetadataDonorEvidence =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataDonorEvidence {
    context: ReclaimMetadataDonorContext,
    pub(super) fact: Fact,
}
impl ReclaimMetadataDonorEvidence {
    pub(crate) fn from_read(
        context: &ReclaimMetadataDonorContext,
        owner: u64,
        source: &CapturedIndexSource,
        length: u64,
        sha256: [u8; 32],
        md5: Option<[u8; 16]>,
    ) -> Result<Self> {
        let node = context.require_donor(source)?;
        if !context.task.resolved()
            || context.task.owner() != owner
            || length != node.length
            || sha256 != context.input.source().byte_proof().sha256()
        {
            return Err(CatalogError::Invalid(
                "新 donor 实读缺少原任务、owner 或完整正文",
            ));
        }
        if let Some(attr) = node
            .xattrs
            .iter()
            .find(|attr| attr.key == crate::ltfs::index::XATTR_MD5)
            && Some(parse_ltfs_hex::<16>(attr)?) != md5
        {
            return Err(CatalogError::Invalid("新 donor 实读 MD5 不符"));
        }
        let (dp_eod, ip, _) = current_proof(source.scope(), source.header())?;
        if !ip.is_some_and(|ip| (1..=context.current.generation).contains(&ip))
            || source.construct_end() != dp_eod
        {
            return Err(CatalogError::Invalid(
                "新 donor 要求完整当前 DP/IP 与精确末边界",
            ));
        }
        let mut index = Vec::new();
        active_volume::encode_index(&mut index, context.current);
        let fact = Fact {
            owner,
            attempt: context.task.id().to_string(),
            recovery_epoch: context.task.recovery_epoch(),
            input: context.input.fingerprint()?,
            index,
            identity: source.identity().map_err(cache)?.fingerprint(),
            dp_eod,
            donor: Sha256::digest(context.task.0.plan.donor.encode()?).into(),
            length,
            sha256,
            md5,
        };
        Ok(Self {
            context: context.clone(),
            fact,
        })
    }
    pub(super) fn require(
        &self,
        task: &ReclaimMetadataWrite,
        owner: u64,
        input: &ReclaimMetadataInput,
    ) -> Result<()> {
        if self.context.task != *task
            || task.owner() != owner
            || self.fact.owner != owner
            || self.fact.input != input.fingerprint()?
            || self.context.input != *input
            || !task.resolved()
            || self.fact.attempt != task.id().to_string()
            || self.fact.recovery_epoch != task.recovery_epoch()
        {
            return Err(CatalogError::Invalid(
                "新 donor 证明改变原任务、正文或恢复序号",
            ));
        }
        Ok(())
    }
    pub(super) fn require_context(
        &self,
        context: &ReclaimMetadataDonorContext,
        plan: &ReclaimMetadataPlan,
    ) -> Result<()> {
        if self.context != *context || plan.0.physical_files != context.files {
            return Err(CatalogError::Invalid(
                "后继登记改变 donor 实读的原上下文或文件数",
            ));
        }
        self.fact.require_plan(plan)
    }
    pub(crate) fn require_capture(
        &self,
        input: &ReclaimMetadataInput,
        source: &CapturedIndexSource,
    ) -> Result<()> {
        self.require(&self.context.task, self.fact.owner, input)?;
        self.context.require_donor(source)?;
        if self.fact.identity != source.identity().map_err(cache)?.fingerprint()
            || self.fact.dp_eod != source.construct_end()
        {
            return Err(CatalogError::Invalid("新计划不是原 donor 实读的精确构造"));
        }
        Ok(())
    }
}

pub(super) async fn original_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    original: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataWrite> {
    let task = context::claimed_task_at(tr, reservation, owner, original).await?;
    if task != *original || !task.resolved() {
        return Err(CatalogError::Invalid("重新规划要求已领取的原恢复终态"));
    }
    registry::require_current_at(tr, reservation, &task).await?;
    let resolution = resolution::require_task_at(tr, reservation, &task).await?;
    if resolution.kind() == ReclaimMetadataRecoveryKind::Committed {
        return Err(CatalogError::Invalid(
            "原元数据已提交，不能重新创建元数据写入",
        ));
    }
    Ok(task)
}

pub(super) async fn context_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
    original: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataDonorContext> {
    request.validate()?;
    let task = original_at(tr, reservation, owner, original).await?;
    task.0.context.require(reservation, file, request)?;
    let input = input_at(tr, reservation, owner, file, request, &task.plan()).await?;
    let (scan, current) =
        active_volume::observed_at(tr, reservation.serial(), task.plan().volume()).await?;
    let resolution = resolution::require_task_at(tr, reservation, &task).await?;
    if scan.id.to_string() != task.0.scan
        || current.generation < resolution.current().generation
        || current.generation == resolution.current().generation && current != resolution.current()
    {
        return Err(CatalogError::Corrupt(
            "新 donor 上下文改变原扫描或恢复后的历史",
        ));
    }
    native_device::require_volume_quiescent_at(tr, reservation.namespace(), current.volume_uuid)
        .await?;
    let files = active_volume::file_count_at(tr, reservation.serial(), current.volume_uuid).await?;
    require_parent_pool_at(tr, reservation).await?;
    target_placement_policy_at(tr, reservation, &input, files, false).await?;
    Ok(ReclaimMetadataDonorContext {
        task,
        input,
        current,
        scan: scan.id,
        files,
    })
}

impl ManagedCatalog {
    pub fn reclaim_metadata_replan_context<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<(ReclaimFile, ActiveFileRequest)>> + 'a>,
    > {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            let task = original_at(&tr, reservation, owner, original).await?;
            context::original_context_at(&tr, reservation, owner, &task).await
        })
    }

    pub fn reclaim_metadata_donor_context<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<ReclaimMetadataDonorContext>> + 'a>,
    > {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            context_at(&tr, reservation, owner, file, request, original).await
        })
    }
}
