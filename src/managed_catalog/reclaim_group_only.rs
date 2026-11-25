//! Explicit, immutable single-group qualification. This is metadata authority,
//! never a source-byte proof or permission to append, repair, or format.
use super::*;
use crate::managed_catalog::body_layouts::CopyLayoutBinding;
use crate::managed_catalog::control::reclaim::copy_groups::{
    ReclaimCopyGroup, ReclaimCopyGroupLimits,
};

const MAX_PLAN: usize = 90_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    file: ReclaimFile,
    head: Vec<u8>,
    copy: [u8; 32],
    uid: u64,
    node: [u8; 32],
    sha256: [u8; 32],
    physical: [u8; 32],
    block_size: u32,
    first: Option<(char, u64, u32)>,
    end: Option<(char, u64, u32)>,
    layout: [u8; 32],
}
impl Member {
    fn capture(candidate: &ReclaimGroupCandidate, layout: &CopyLayoutBinding) -> Result<Self> {
        Ok(Self {
            file: candidate.file().clone(),
            head: candidate.head().to_vec(),
            copy: candidate.copy_digest(),
            uid: candidate.body_uid(),
            node: candidate.node_digest(),
            sha256: candidate.known_sha256(),
            physical: candidate.physical_digest(),
            block_size: candidate.block_size(),
            first: candidate.range().0,
            end: candidate.range().1,
            layout: layout.fingerprint()?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    group: String,
    logical: walk::GroupOnlyLogicalManifest,
    directories: Vec<Object>,
    members: Vec<Member>,
    limits: ReclaimCopyGroupLimits,
    index_bytes: u64,
    destination: ReclaimDriveBinding,
    target_index: Vec<u8>,
    target_scan: String,
    target_files: u64,
    pool_revision: u64,
    file_limit: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum Decision {
    Ready { plan: Ready },
    UnsupportedGroupOnly { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    version: u8,
    parent: String,
    inventory: Summary,
    decision: Decision,
}

pub(crate) enum ReclaimGroupOnlyQualification {
    NotRequested,
    Ready,
    Unsupported(String),
    NeedsCandidates(ReclaimGroupOnlyDraft),
}
pub(crate) struct ReclaimGroupOnlyDraft {
    row: Row,
}
impl ReclaimGroupOnlyDraft {
    pub(crate) fn files(&self) -> impl Iterator<Item = &ReclaimFile> {
        let Decision::Ready { plan } = &self.row.decision else {
            unreachable!()
        };
        plan.logical.files.iter().map(|file| &file.file)
    }
}
fn key(parent: &ReclaimReservation) -> Vec<u8> {
    let mut key = domain(
        parent.namespace(),
        crate::managed_catalog::RECLAIM_GROUP_ONLY_DOMAIN,
    );
    key.extend_from_slice(
        &parent.intent()["epoch"]
            .as_u64()
            .expect("validated epoch")
            .to_be_bytes(),
    );
    key.extend_from_slice(
        Uuid::parse_str(parent.intent()["operation"].as_str().expect("validated op"))
            .expect("validated op")
            .as_bytes(),
    );
    key
}
fn require_geometry(members: &[Member]) -> Result<()> {
    let mut sizes = std::collections::BTreeSet::new();
    let mut partitions = std::collections::BTreeSet::new();
    for member in members {
        sizes.insert(member.block_size);
        let valid_range = match (member.first, member.end) {
            (None, None) => member.file.size() == 0,
            (Some(first), Some(end)) => {
                partitions.insert(first.0);
                member.file.size() != 0 && first < end && first.0 == end.0
            }
            _ => false,
        };
        if member.block_size == 0 || !valid_range {
            return Err(CatalogError::Invalid("group-only原物理范围不受支持"));
        }
    }
    if sizes.len() != 1 || partitions.len() > 1 {
        return Err(CatalogError::Invalid(
            "group-only不同块尺寸或跨分区正文不受支持",
        ));
    }
    copy_groups::physical_order(
        members
            .iter()
            .enumerate()
            .map(|(n, m)| (n as u8, m.first, m.end)),
    )?;
    Ok(())
}
fn encode(row: &Row) -> Result<Vec<u8>> {
    let mut raw = serde_json::to_vec(row).map_err(cache)?;
    if row.version != 1 || row.parent.len() != 64 || raw.len() > MAX_PLAN {
        return Err(CatalogError::Invalid(
            "group-only资格记录身份或大小不受支持",
        ));
    }
    if let Decision::Ready { plan } = &row.decision {
        let group = Uuid::parse_str(&plan.group).map_err(cache)?;
        let scan = Uuid::parse_str(&plan.target_scan).map_err(cache)?;
        let n = plan.logical.files.len();
        if ReclaimCopyGroupLimits::bounded(plan.limits.members(), plan.limits.bytes())?
            != plan.limits
        {
            return Err(CatalogError::Corrupt("group-only原固定预算编码无效"));
        }
        if group.is_nil()
            || group.to_string() != plan.group
            || scan.is_nil()
            || scan.to_string() != plan.target_scan
            || n == 0
            || n > 32
            || n != plan.members.len()
            || n > usize::from(plan.limits.members())
            || plan.index_bytes == 0
            || plan.directories.is_empty()
            || plan.directories.len() > 32
            || row.inventory.counts.files != n as u64
            || row.inventory.counts.directories != plan.directories.len() as u64
            || row.inventory.counts.symlinks != 0
            || plan.logical.directories.is_empty()
            || plan.logical.directories.len() > 32
        {
            return Err(CatalogError::Corrupt(
                "group-only完整资格成员或目录边界无效",
            ));
        }
        active_volume::decode_index(plan.destination.volume, &plan.target_index)?;
        let mut identities = std::collections::BTreeSet::new();
        let mut uids = std::collections::BTreeSet::new();
        let mut bytes = 0u64;
        for (logical, member) in plan.logical.files.iter().zip(&plan.members) {
            logical.file.validate()?;
            if logical.file != member.file
                || logical.head != member.head
                || Sha256::digest(&logical.copy).as_slice() != member.copy
                || !identities.insert(member.file.operation())
                || !uids.insert(member.uid)
            {
                return Err(CatalogError::Corrupt("group-only原资格成员身份重复或改变"));
            }
            bytes = bytes
                .checked_add(member.file.size())
                .ok_or(CatalogError::Corrupt("group-only资格字节溢出"))?;
        }
        require_geometry(&plan.members)?;
        if bytes > plan.limits.bytes() || bytes != row.inventory.counts.data_bytes {
            return Err(CatalogError::Corrupt("group-only资格字节预算不符"));
        }
        let mut roots = 0;
        for directory in &plan.directories {
            directory.encode()?;
            if directory.kind != ReclaimSourceNodeKind::Directory || !uids.insert(directory.uid) {
                return Err(CatalogError::Corrupt("group-only资格目录UID或类型无效"));
            }
            roots += usize::from(directory.path.is_empty());
        }
        if roots != 1 {
            return Err(CatalogError::Corrupt("group-only资格缺原唯一root"));
        }
    }
    if let Decision::UnsupportedGroupOnly { reason } = &row.decision {
        if reason.is_empty() || reason.len() > 1024 {
            return Err(CatalogError::Corrupt("group-only拒绝原因无效"));
        }
    }
    let hash = Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-only/1")
        .chain_update(&raw)
        .finalize();
    raw.extend_from_slice(&hash);
    Ok(raw)
}
async fn row_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<Option<Row>> {
    let Some(raw) = tr.get(&key(parent), false).await? else {
        return Ok(None);
    };
    if raw.len() < 32 || raw.len() > MAX_PLAN + 32 {
        return Err(CatalogError::Corrupt("group-only资格记录长度"));
    }
    let row: Row = serde_json::from_slice(&raw[..raw.len() - 32]).map_err(cache)?;
    if encode(&row)?.as_slice() != raw.as_ref()
        || row.parent != parent.fingerprint()
        || !parent.spec().group_only()
        || parent.intent()["version"] != 2
    {
        return Err(CatalogError::Corrupt("group-only原资格身份或规范编码改变"));
    }
    Ok(Some(row))
}
fn ready(row: &Row) -> Result<&Ready> {
    match &row.decision {
        Decision::Ready { plan } => Ok(plan),
        Decision::UnsupportedGroupOnly { .. } => Err(CatalogError::Invalid(
            "UnsupportedGroupOnly：未执行迁移，原请求保留",
        )),
    }
}
fn physical_directories(
    inventory: &ReclaimSourceInventory<'_>,
    max_attributes: u64,
) -> Result<Vec<Object>> {
    if inventory.files() == 0
        || inventory.files() > 32
        || inventory.directories() > 32
        || inventory.symlinks() != 0
    {
        return Err(CatalogError::Invalid(
            "group-only源必须完整包含1到32普通文件及有界目录，不能含链接",
        ));
    }
    let mut directories = Vec::new();
    let mut attributes = 0u64;
    let mut failure = None;
    let read = inventory.source.for_each_entry(|entry| {
        let captured: Result<Option<Object>> = (|| {
            if !matches!(&entry, IndexEntry::Directory { .. }) {
                return Ok(None);
            }
            let node = node_metadata::attributes_of(&entry)?;
            attributes = attributes
                .checked_add(
                    node.bytes()
                        .ok_or(CatalogError::Invalid("group-only目录属性不完整"))?
                        .len() as u64,
                )
                .ok_or(CatalogError::Invalid("group-only目录属性预算溢出"))?;
            if attributes > max_attributes {
                return Err(CatalogError::Invalid("group-only目录属性超过固定预算"));
            }
            Ok(Some(Object::capture(entry)?))
        })();
        match captured {
            Ok(Some(node)) => {
                directories.push(node);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error) => {
                failure = Some(error);
                Err(crate::error::TapeError::Catalog(
                    "group-only目录资格不受支持".into(),
                ))
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    read.map_err(cache)?;
    Ok(directories)
}
async fn initial_ready_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    inventory: &ReclaimSourceInventory<'_>,
    limits: ReclaimCopyGroupLimits,
    index_bytes: u64,
) -> Result<Ready> {
    let manifest = ledger::require_complete_at(tr, parent).await?;
    if manifest.summary != inventory.summary {
        return Err(CatalogError::Invalid("group-only必须使用原完整实读库存"));
    }
    let directories = physical_directories(inventory, index_bytes)?;
    let logical = walk::group_only_snapshot_at(tr, parent).await?;
    if logical.files.len() as u64 != inventory.files()
        || logical.files.len() > usize::from(limits.members())
        || inventory.data_bytes() > limits.bytes()
    {
        return Err(CatalogError::Invalid(
            "group-only完整物理文件与逻辑源集合或预算不符",
        ));
    }
    let layout = layout::at(tr, parent)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if copy_groups::remaining_slots_at(tr, parent, layout.destination.volume).await?
        < logical.files.len() as u64
    {
        return Err(CatalogError::Invalid(
            "group-only目标不足以容纳全部donor与archive两项",
        ));
    }
    let (scan, index) =
        active_volume::observed_at(tr, parent.serial(), layout.destination.volume).await?;
    let count =
        active_volume::file_count_at(tr, parent.serial(), layout.destination.volume).await?;
    let pool_raw = tr
        .get(&pool_key(parent.namespace(), parent.spec().pool()), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let pool = decode_pool(parent.spec().pool(), &pool_raw)?;
    let hash = Sha256::new()
        .chain_update(b"tape-rs/group-only-group/1")
        .chain_update(parent.fingerprint().as_bytes())
        .finalize();
    Ok(Ready {
        group: Uuid::from_bytes(hash[..16].try_into().expect("SHA256")).to_string(),
        logical,
        directories,
        members: Vec::new(),
        limits,
        index_bytes,
        destination: layout.destination,
        target_index: index_bytes_of(index),
        target_scan: scan.id.to_string(),
        target_files: count,
        pool_revision: pool.revision,
        file_limit: pool.file_limit,
    })
}
fn index_bytes_of(index: ActiveIndexIdentity) -> Vec<u8> {
    let mut bytes = Vec::new();
    active_volume::encode_index(&mut bytes, index);
    bytes
}
async fn require_identity_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    row: &Row,
) -> Result<()> {
    if record_at(tr, parent, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    source_state_at(tr, parent).await?;
    if ledger::require_complete_at(tr, parent).await?.summary != row.inventory {
        return Err(CatalogError::Invalid("group-only原封存库存改变"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Safe before layout/load: a persistent Unsupported decision never retries
    /// source capture, root metadata, obsolete retirement or formatting.
    pub(crate) async fn reclaim_group_only_status(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<String>> {
        if !parent.spec().group_only() {
            return Ok(None);
        }
        let tr = self.db.create_trx()?;
        if record_at(&tr, parent, owner).await?["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(match row_at(&tr, parent).await?.map(|r| r.decision) {
            Some(Decision::UnsupportedGroupOnly { reason }) => Some(reason),
            _ => None,
        })
    }
    pub(crate) async fn prepare_reclaim_group_only(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        inventory: &ReclaimSourceInventory<'_>,
        limits: ReclaimCopyGroupLimits,
        index_bytes: u64,
    ) -> Result<ReclaimGroupOnlyQualification> {
        if !parent.spec().group_only() {
            return Ok(ReclaimGroupOnlyQualification::NotRequested);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            if record_at(&tr, parent, owner).await?["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            source_state_at(&tr, parent).await?;
                            if let Some(row) = row_at(&tr, parent).await? {
                                return Ok(match row.decision {
                                    Decision::Ready { .. } => ReclaimGroupOnlyQualification::Ready,
                                    Decision::UnsupportedGroupOnly { reason } => {
                                        ReclaimGroupOnlyQualification::Unsupported(reason)
                                    }
                                });
                            }
                            let seal = ledger::require_complete_at(&tr, parent).await?;
                            if seal.summary != inventory.summary {
                                return Err(CatalogError::Invalid("group-only原库存改变"));
                            }
                            let row =
                                match initial_ready_at(&tr, parent, inventory, limits, index_bytes)
                                    .await
                                {
                                    Ok(plan) => Row {
                                        version: 1,
                                        parent: parent.fingerprint(),
                                        inventory: seal.summary,
                                        decision: Decision::Ready { plan },
                                    },
                                    Err(error)
                                        if matches!(error.domain(), CatalogError::Invalid(_)) =>
                                    {
                                        let reason =
                                            error.to_string().chars().take(240).collect::<String>();
                                        let row = Row {
                                            version: 1,
                                            parent: parent.fingerprint(),
                                            inventory: seal.summary,
                                            decision: Decision::UnsupportedGroupOnly {
                                                reason: reason.clone(),
                                            },
                                        };
                                        tr.set(&key(parent), &encode(&row)?);
                                        return Ok(ReclaimGroupOnlyQualification::Unsupported(
                                            reason,
                                        ));
                                    }
                                    Err(error) => return Err(error),
                                };
                            Ok(ReclaimGroupOnlyQualification::NeedsCandidates(
                                ReclaimGroupOnlyDraft { row },
                            ))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub(crate) async fn reject_reclaim_group_only(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        draft: &ReclaimGroupOnlyDraft,
        reason: &str,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            require_identity_at(&tr, parent, owner, &draft.row).await?;
                            if row_at(&tr, parent).await?.is_some() {
                                return Err(CatalogError::Invalid(
                                    "group-only资格已固定，不能替换",
                                ));
                            }
                            let mut row = draft.row.clone();
                            row.decision = Decision::UnsupportedGroupOnly {
                                reason: reason.chars().take(240).collect(),
                            };
                            tr.set(&key(parent), &encode(&row)?);
                            Ok(())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub(crate) async fn seal_reclaim_group_only(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        draft: &ReclaimGroupOnlyDraft,
        candidates: &[(ReclaimGroupCandidate, CopyLayoutBinding)],
    ) -> Result<ReclaimGroupOnlyQualification> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            require_identity_at(&tr, parent, owner, &draft.row).await?;
                            let saved = row_at(&tr, parent).await?;
                            if let Some(Row {
                                decision: Decision::UnsupportedGroupOnly { reason },
                                ..
                            }) = &saved
                            {
                                return Ok(ReclaimGroupOnlyQualification::Unsupported(
                                    reason.clone(),
                                ));
                            }
                            let prepared: Result<Row> = async {
                                let mut row = draft.row.clone();
                                let Decision::Ready { plan } = &mut row.decision else {
                                    return Err(CatalogError::Invalid(
                                        "group-only草案不是Ready候选",
                                    ));
                                };
                                if walk::group_only_snapshot_at(&tr, parent).await? != plan.logical
                                    || candidates.len() != plan.logical.files.len()
                                {
                                    return Err(CatalogError::Invalid(
                                        "group-only最终完整逻辑集合改变",
                                    ));
                                }
                                let mut uids = std::collections::BTreeSet::new();
                                for (logical, (candidate, binding)) in
                                    plan.logical.files.iter().zip(candidates)
                                {
                                    if &logical.file != candidate.file()
                                        || !uids.insert(candidate.body_uid())
                                    {
                                        return Err(CatalogError::Invalid(
                                            "group-only成员遗漏重复或顺序改变",
                                        ));
                                    }
                                    candidate.require_at(&tr, parent, owner).await?;
                                    let head = decode_head(&logical.head)?;
                                    if body_layouts::binding_at(
                                        &tr,
                                        parent.namespace(),
                                        logical.file.file_id(),
                                        head.copy_id,
                                    )
                                    .await?
                                    .as_ref()
                                        != Some(binding)
                                    {
                                        return Err(CatalogError::Invalid(
                                            "group-only正文布局绑定改变",
                                        ));
                                    }
                                    plan.members.push(Member::capture(candidate, binding)?);
                                }
                                let physical = read_page(
                                    &tr,
                                    &ledger::own_key(parent, b'e'),
                                    None,
                                    128,
                                    false,
                                )
                                .await?;
                                if physical.next.is_some() {
                                    return Err(CatalogError::Invalid(
                                        "group-only完整物理库存超过预算",
                                    ));
                                }
                                let mut physical_files = std::collections::BTreeSet::new();
                                for (_, raw) in physical.items {
                                    let object: Object =
                                        serde_json::from_slice(&raw).map_err(cache)?;
                                    if object.kind == ReclaimSourceNodeKind::File {
                                        physical_files.insert(object.uid);
                                    }
                                }
                                if physical_files != uids {
                                    return Err(CatalogError::Invalid(
                                        "group-only有未映射原始正文或额外物理文件",
                                    ));
                                }
                                require_target_at(&tr, parent, owner, plan).await?;
                                encode(&row)?;
                                Ok(row)
                            }
                            .await;
                            let row = match prepared {
                                Ok(row) => {
                                    if let Some(saved) = &saved {
                                        if encode(saved)? != encode(&row)? {
                                            return Err(CatalogError::Invalid(
                                                "group-only重放改变原资格计划",
                                            ));
                                        }
                                        return Ok(ReclaimGroupOnlyQualification::Ready);
                                    }
                                    row
                                }
                                Err(error)
                                    if saved.is_none()
                                        && matches!(error.domain(), CatalogError::Invalid(_)) =>
                                {
                                    let mut row = draft.row.clone();
                                    row.decision = Decision::UnsupportedGroupOnly {
                                        reason: error.to_string().chars().take(240).collect(),
                                    };
                                    row
                                }
                                Err(error) => return Err(error),
                            };
                            // No write occurs until all Ready checks, or the exact
                            // Unsupported disposition, have completed in this transaction.
                            tr.set(&key(parent), &encode(&row)?);
                            Ok(match row.decision {
                                Decision::Ready { .. } => ReclaimGroupOnlyQualification::Ready,
                                Decision::UnsupportedGroupOnly { reason } => {
                                    ReclaimGroupOnlyQualification::Unsupported(reason)
                                }
                            })
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub(crate) async fn reclaim_group_only_precopy_ready(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<bool> {
        if !parent.spec().group_only() {
            return Ok(true);
        }
        let tr = self.db.create_trx()?;
        ready_exists_at(&tr, parent, owner).await
    }
    pub(crate) async fn require_reclaim_group_only_ready(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        body_bytes: u64,
        index_bytes: u64,
    ) -> Result<()> {
        if !parent.spec().group_only() {
            return Ok(());
        }
        let tr = self.db.create_trx()?;
        require_ready_at(&tr, parent, owner, Some((body_bytes, index_bytes))).await
    }
}

/// Mandatory immutable qualification presence for already-admitted I/O scopes.
/// Current owner/source/claims are still checked by the enclosing scope.
pub(in crate::managed_catalog::control::reclaim) fn require_seal_at<'a>(
    tr: &'a Transaction,
    parent: &'a ReclaimReservation,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(require_seal_at_in(tr, parent))
}
async fn require_seal_at_in(tr: &Transaction, parent: &ReclaimReservation) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    ready(&row)?;
    if ledger::require_complete_at(tr, parent).await?.summary != row.inventory {
        return Err(CatalogError::Invalid("group-only原资格清单改变"));
    }
    Ok(())
}

pub(in crate::managed_catalog::control::reclaim) fn require_ready_at<'a>(
    tr: &'a Transaction,
    parent: &'a ReclaimReservation,
    owner: u64,
    budget: Option<(u64, u64)>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(require_ready_at_in(tr, parent, owner, budget))
}
async fn require_ready_at_in(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    budget: Option<(u64, u64)>,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Invalid(
        "group-only完整资格尚未Ready，禁止复制或格式化",
    ))?;
    require_identity_at(tr, parent, owner, &row).await?;
    let plan = ready(&row)?;
    if plan.members.len() != plan.logical.files.len()
        || plan.members.is_empty()
        || budget
            .is_some_and(|(bytes, index)| bytes < plan.limits.bytes() || index < plan.index_bytes)
    {
        return Err(CatalogError::Invalid(
            "group-only当前执行预算不足原固定计划",
        ));
    }
    if require_target_at(tr, parent, owner, plan).await? == TargetProgress::BeforeGroup {
        if walk::group_only_snapshot_at(tr, parent).await? != plan.logical {
            return Err(CatalogError::Invalid(
                "group-only原资格后完整逻辑集合已改变",
            ));
        }
        for member in &plan.members {
            let head = decode_head(&member.head)?;
            let binding = body_layouts::binding_at(
                tr,
                parent.namespace(),
                member.file.file_id(),
                head.copy_id,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            if binding.fingerprint()? != member.layout {
                return Err(CatalogError::Invalid("group-only原资格后正文布局已改变"));
            }
        }
    }
    Ok(())
}
pub(in crate::managed_catalog::control::reclaim) async fn ready_exists_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
) -> Result<bool> {
    if !parent.spec().group_only() {
        return Ok(true);
    }
    let Some(row) = row_at(tr, parent).await? else {
        return Ok(false);
    };
    ready(&row)?;
    require_ready_at(tr, parent, owner, None).await?;
    Ok(true)
}
/// The catalog owns this guard: a caller cannot turn a Ready plan into Single
/// or a prefix by changing the current selector budget/candidate.
pub(in crate::managed_catalog::control::reclaim) async fn require_selector_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    limits: ReclaimCopyGroupLimits,
    choice: &crate::managed_catalog::control::reclaim::copy_groups::ReclaimCopyGroupChoice<'_>,
) -> Result<()> {
    use crate::managed_catalog::control::reclaim::copy_groups::ReclaimCopyGroupChoice;
    if !parent.spec().group_only() {
        return Ok(());
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    if limits.members() < plan.limits.members()
        || limits.bytes() < plan.limits.bytes()
        || matches!(choice, ReclaimCopyGroupChoice::Single(_))
    {
        return Err(CatalogError::Invalid(
            "group-only禁止Single或缩减原固定组预算",
        ));
    }
    if let ReclaimCopyGroupChoice::Candidate(candidate) = choice {
        let member = plan
            .members
            .iter()
            .find(|m| &m.file == candidate.file())
            .ok_or(CatalogError::Invalid("group-only候选不属于原完整计划"))?;
        if member.head != candidate.head()
            || member.copy != candidate.copy_digest()
            || member.uid != candidate.body_uid()
            || member.node != candidate.node_digest()
            || member.sha256 != candidate.known_sha256()
            || member.physical != candidate.physical_digest()
            || member.block_size != candidate.block_size()
            || (member.first, member.end) != candidate.range()
        {
            return Err(CatalogError::Invalid("group-only原候选或extent几何改变"));
        }
    }
    Ok(())
}
/// All selected/closed-prefix paths share the exact original complete group.
pub(in crate::managed_catalog::control::reclaim) async fn require_complete_selection_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    if group.id().to_string() != plan.group
        || group.files().cloned().collect::<Vec<_>>()
            != plan
                .logical
                .files
                .iter()
                .map(|m| m.file.clone())
                .collect::<Vec<_>>()
    {
        return Err(CatalogError::Invalid("group-only禁止提前封存不完整原组"));
    }
    Ok(())
}
pub(in crate::managed_catalog::control::reclaim) async fn planned_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<Option<Uuid>> {
    if !parent.spec().group_only() {
        return Ok(None);
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    Ok(Some(Uuid::parse_str(&ready(&row)?.group).map_err(cache)?))
}

pub(in crate::managed_catalog::control::reclaim) async fn planned_preflight_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    layout: &ReclaimLayout,
) -> Result<Option<(Uuid, Uuid, ActiveIndexIdentity)>> {
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    if plan.members.len() < 3 {
        return Ok(None);
    }
    if plan.destination != layout.destination {
        return Err(CatalogError::Invalid("回收目标预检固定分配改变"));
    }
    Ok(Some((
        Uuid::parse_str(&plan.group).map_err(cache)?,
        Uuid::parse_str(&plan.target_scan).map_err(cache)?,
        active_volume::decode_index(plan.destination.volume, &plan.target_index)?,
    )))
}

pub(in crate::managed_catalog::control::reclaim) async fn require_group_root_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: Uuid,
    root: &ReclaimSourceNode,
) -> Result<()> {
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    if !parent.groups_archive_root()
        || plan.group != group.to_string()
        || !root.path().is_empty()
        || !plan.directories.contains(&root.0)
    {
        return Err(CatalogError::Invalid(
            "回收组归档根不属于原Ready完整目录清单",
        ));
    }
    Ok(())
}
pub(in crate::managed_catalog::control::reclaim) async fn planned_operation_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    path: &str,
    id: Uuid,
    head: &FileHead,
    copy: &PhysicalCopy,
) -> Result<Option<Uuid>> {
    if !parent.spec().group_only() {
        return Ok(None);
    }
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    let member = plan
        .members
        .iter()
        .find(|m| m.file.path() == path && m.file.file_id() == id)
        .ok_or(CatalogError::Invalid("group-only出现计划外逻辑源文件"))?;
    let copy_digest: [u8; 32] = Sha256::digest(copy_records::encode_physical_copy(copy)?).into();
    if member.head != encode_head(head) || member.copy != copy_digest {
        return Err(CatalogError::Invalid("group-only原逻辑版本或copy改变"));
    }
    Ok(Some(member.file.operation()))
}
pub(in crate::managed_catalog::control::reclaim) fn require_admission_at<'a>(
    tr: &'a Transaction,
    parent: &'a ReclaimReservation,
    owner: u64,
    group: &'a ReclaimCopyGroup,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(require_admission_at_in(tr, parent, owner, group))
}
async fn require_admission_at_in(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    if !parent.spec().group_only() {
        return Ok(());
    }
    require_ready_at(tr, parent, owner, None).await?;
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    if group.id().to_string() != plan.group
        || group.files().cloned().collect::<Vec<_>>()
            != plan
                .logical
                .files
                .iter()
                .map(|m| m.file.clone())
                .collect::<Vec<_>>()
    {
        return Err(CatalogError::Invalid("group-only必须一次准入完整原单组"));
    }
    if !group.requires_observation() {
        if walk::group_only_snapshot_at(tr, parent).await? != plan.logical {
            return Err(CatalogError::Invalid("group-only准入前完整集合已改变"));
        }
        for member in &plan.members {
            let head = decode_head(&member.head)?;
            let binding = body_layouts::binding_at(
                tr,
                parent.namespace(),
                member.file.file_id(),
                head.copy_id,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            if binding.fingerprint()? != member.layout {
                return Err(CatalogError::Invalid("group-only准入前布局已改变"));
            }
        }
    }
    Ok(())
}

/// Metadata-only continuation of the one immutable group qualification.
/// The caller separately validates the exact Closed group/disposition at this RV.
pub(in crate::managed_catalog::control::reclaim) async fn require_directory_successor_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: Uuid,
    metadata: &ReclaimMetadataPlan,
) -> Result<()> {
    require_ready_at(tr, parent, owner, None).await?;
    let row = row_at(tr, parent).await?.ok_or(CatalogError::Incomplete)?;
    let plan = ready(&row)?;
    let original = &metadata.0.source;
    let archive = format!(
        "_reclaim_{}/node-{}",
        parent.intent()["operation"]
            .as_str()
            .ok_or(CatalogError::Incomplete)?,
        original.uid
    );
    if plan.group != group.to_string()
        || original.path.is_empty()
        || original.kind != ReclaimSourceNodeKind::Directory
        || !plan.directories.contains(original)
        || !metadata.is_node_metadata()
        || metadata.is_raw_body()
        || metadata.added_files() != 0
        || metadata.path() != archive
        || metadata.0.parent != parent.fingerprint()
        || metadata.0.inventory != row.inventory.digest
        || metadata.volume() != plan.destination.volume
    {
        return Err(CatalogError::Invalid(
            "group-only目录后继偏离原Ready源节点或路径",
        ));
    }
    coverage::require_uncovered_uid_at(tr, parent, owner, &ReclaimSourceNode(original.clone()))
        .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetProgress {
    BeforeGroup,
    GroupCheckpoint,
}

async fn require_target_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    plan: &Ready,
) -> Result<TargetProgress> {
    let layout = layout::at(tr, parent)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if layout.destination != plan.destination {
        return Err(CatalogError::Invalid("group-only固定目标布局改变"));
    }
    let assignment =
        ManagedCatalog::verified_assignment(tr, parent.serial(), &plan.destination.barcode)
            .await?
            .ok_or(CatalogError::Incomplete)?;
    if assignment.volume_id != Some(plan.destination.volume)
        || assignment.revision != plan.destination.revision
        || !assignment.allows_io()
    {
        return Err(CatalogError::Invalid("group-only原目标身份或策略改变"));
    }
    let pool = decode_pool(
        parent.spec().pool(),
        &tr.get(&pool_key(parent.namespace(), parent.spec().pool()), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    if pool.revision != plan.pool_revision || pool.file_limit != plan.file_limit {
        return Err(CatalogError::Invalid("group-only原池策略改变"));
    }
    let (scan, current) =
        active_volume::observed_at(tr, parent.serial(), plan.destination.volume).await?;
    if scan.id.to_string() != plan.target_scan {
        return Err(CatalogError::Invalid("group-only原目标scan改变"));
    }
    let initial = active_volume::decode_index(plan.destination.volume, &plan.target_index)?;
    let grouped_root = parent.groups_archive_root();
    if current.generation < initial.generation
        || current.generation - initial.generation
            > plan.directories.len() as u64 + u64::from(!grouped_root)
    {
        return Err(CatalogError::Invalid("group-only目标历史超出原有界计划"));
    }
    let mut previous = initial;
    let mut count = plan.target_files;
    let mut seen = std::collections::BTreeSet::new();
    let mut batch_seen = false;
    for generation in initial.generation + 1..=current.generation {
        let (kind, index) =
            active_volume::history_at(tr, parent.serial(), plan.destination.volume, generation)
                .await?;
        match kind {
            active_volume::IndexKind::ReclaimMetadata(attempt) => {
                if grouped_root && !batch_seen {
                    return Err(CatalogError::Invalid("归档根随组的回收禁止先提交独立目录"));
                }
                let link = metadata_write::require_directory_history_link_at(
                    tr, parent, owner, attempt, index,
                )
                .await?;
                let original = plan
                    .directories
                    .iter()
                    .find(|node| {
                        node.uid == link.source().uid()
                            && node.path == link.source().path()
                            && node.sha256 == link.source().fingerprint()
                    })
                    .ok_or(CatalogError::Invalid("group-only出现计划外目录metadata"))?;
                let archive = format!(
                    "_reclaim_{}",
                    parent.intent()["operation"].as_str().expect("valid parent")
                );
                let path = if original.path.is_empty() {
                    archive
                } else {
                    format!("{archive}/node-{}", original.uid)
                };
                if grouped_root && original.path.is_empty()
                    || !seen.insert(original.uid)
                    || link.destination_path() != path
                    || link.previous() != previous
                    || link.current() != index
                    || link.physical_files() != count
                {
                    return Err(CatalogError::Invalid(
                        "group-only目录metadata前驱、路径或计数不同",
                    ));
                }
            }
            active_volume::IndexKind::FileBatch(batch) => {
                let group = Uuid::parse_str(&plan.group).map_err(cache)?;
                if batch_seen || batch != group {
                    return Err(CatalogError::Invalid("group-only目标包含额外批次"));
                }
                let (base, files, root) =
                    copy_groups::group_only_history_link_at(tr, parent, group, index).await?;
                if base != previous
                    || count.checked_add(2 * plan.members.len() as u64) != Some(files)
                {
                    return Err(CatalogError::Invalid(
                        "group-only原组历史前驱或物理文件计数不同",
                    ));
                }
                if let Some(root) = root {
                    if !grouped_root
                        || previous != initial
                        || !root.path().is_empty()
                        || !plan.directories.contains(&root.0)
                        || !seen.insert(root.uid())
                    {
                        return Err(CatalogError::Invalid("归档根未随原组首代提交"));
                    }
                } else if grouped_root {
                    return Err(CatalogError::Invalid("原组首代缺实读归档根证明"));
                }
                count = files;
                batch_seen = true;
            }
            _ => {
                return Err(CatalogError::Invalid(
                    "group-only目标历史包含不受支持的写入或恢复",
                ));
            }
        }
        previous = index;
    }
    if previous != current
        || active_volume::file_count_at(tr, parent.serial(), plan.destination.volume).await?
            != count
    {
        return Err(CatalogError::Invalid("group-only目标完整历史终点不同"));
    }
    Ok(if batch_seen {
        TargetProgress::GroupCheckpoint
    } else {
        TargetProgress::BeforeGroup
    })
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    fn member(
        size: u64,
        block_size: u32,
        first: Option<(char, u64, u32)>,
        end: Option<(char, u64, u32)>,
    ) -> Member {
        Member {
            file:serde_json::from_value(json!({"path":"/file","file":Uuid::from_u128(1).to_string(),"version":1,"size":size,"operation":Uuid::from_u128(2).to_string()})).unwrap(),
            head:vec![],copy:[0;32],uid:1,node:[0;32],sha256:[0;32],physical:[0;32],block_size,first,end,layout:[0;32],
        }
    }
    #[test]
    fn qualification_rejects_missing_block_size_partial_ranges_and_cross_partition() {
        let a = member(1, 4096, Some(('b', 7, 0)), Some(('b', 8, 0)));
        assert!(require_geometry(&[a.clone(), member(0, 4096, None, None)]).is_ok());
        for invalid in [
            member(1, 0, Some(('b', 8, 0)), Some(('b', 9, 0))),
            member(1, 8192, Some(('b', 8, 0)), Some(('b', 9, 0))),
            member(1, 4096, Some(('a', 8, 0)), Some(('a', 9, 0))),
            member(1, 4096, Some(('b', 8, 0)), Some(('c', 9, 0))),
            member(1, 4096, None, None),
            member(0, 4096, None, Some(('b', 9, 0))),
        ] {
            assert!(require_geometry(&[a.clone(), invalid]).is_err());
        }
        assert!(
            require_geometry(&[a, member(1, 4096, Some(('b', 7, 1)), Some(('b', 9, 0)))]).is_err()
        );
    }
    #[test]
    fn original_physical_order_keeps_empty_and_adjacent_ranges() {
        assert_eq!(
            copy_groups::physical_order([
                (0, Some(('b', 9, 0)), Some(('b', 10, 0))),
                (1, None, None),
                (2, Some(('b', 7, 0)), Some(('b', 9, 0))),
            ])
            .unwrap(),
            vec![1, 2, 0]
        );
    }
    #[test]
    fn original_physical_order_rejects_overlapping_and_interleaved_ranges() {
        for end in [('b', 9, 1), ('b', 11, 0)] {
            assert!(
                copy_groups::physical_order([
                    (0, Some(('b', 7, 0)), Some(end)),
                    (1, Some(('b', 9, 0)), Some(('b', 10, 0))),
                ])
                .is_err()
            );
        }
    }
}

#[cfg(test)]
#[path = "reclaim_group_directory_tests.rs"]
mod directory_tests;
