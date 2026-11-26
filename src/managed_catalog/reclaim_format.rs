//! Original explicit erase goal, admitted only after both independent gates.
//! These catalog snapshots never grant a device command by themselves.
use super::*;
use serde::{Deserialize, Serialize};

#[path = "reclaim_format_home.rs"]
mod home;
#[path = "reclaim_format_publication.rs"]
mod publication;
pub use home::ReclaimFormatHomeEvidence;
pub use publication::ReclaimFormatResult;
pub(super) use publication::result_at;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    version: u8,
    parent: String,
    source: String,
    output: String,
    scan: String,
    coverage: [u8; 32],
    issued: bool,
    attempt: u32,
    last_observation: Option<ResetObservation>,
    formatted_at: String,
    source_evidence: source::FormatSourceFact,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetObservation {
    attempt: u32,
    kind: String,
    sha256: [u8; 32],
}

impl Goal {
    fn encode(&self) -> Result<Vec<u8>> {
        let source = node_mutation::id(&self.source)?;
        let output = node_mutation::id(&self.output)?;
        node_mutation::id(&self.scan)?;
        self.source_evidence.require_origin(source, &self.parent)?;
        crate::ltfs::index::MetadataChange {
            modify_time: Some(self.formatted_at.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|e| CatalogError::Cache(e.to_string()))?;
        if self.version != 2
            || self.issued && self.attempt == 0
            || !self.issued
                && self.attempt > 0
                && self
                    .last_observation
                    .as_ref()
                    .is_none_or(|o| o.attempt != self.attempt)
            || self.last_observation.as_ref().is_some_and(|o| {
                !matches!(o.kind.as_str(), "original" | "partial")
                    || o.attempt == 0
                    || (self.issued && o.attempt.checked_add(1) != Some(self.attempt))
                    || (!self.issued && o.attempt != self.attempt)
            })
            || source == output
            || self.parent.len() != 64
            || !self
                .parent
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CatalogError::Corrupt("原回收格式化目标身份无效"));
        }
        let mut raw = serde_json::to_vec(self).map_err(|e| CatalogError::Cache(e.to_string()))?;
        if raw.len() > 48_000 {
            return Err(CatalogError::Invalid("格式化目标来源链超出持久化预算"));
        }
        let sum = Sha256::new()
            .chain_update(b"tape-rs/reclaim-format/1")
            .chain_update(&raw)
            .finalize();
        raw.extend_from_slice(&sum);
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=48_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("回收格式化目标长度无效"));
        }
        let value: Self = serde_json::from_slice(&raw[..raw.len() - 32])
            .map_err(|_| CatalogError::Corrupt("回收格式化目标 JSON 无效"))?;
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("回收格式化目标完整性校验失败"));
        }
        Ok(value)
    }
    fn claim(&self, reservation: &ReclaimReservation) -> Result<Vec<u8>> {
        let mut fixed = self.clone();
        fixed.issued = false;
        fixed.attempt = 0;
        fixed.last_observation = None;
        node_claims::encode(&node_claims::NodeVolumeClaim::ReclaimFormat(Box::new(
            node_claims::ReclaimFormatVolumeClaim {
                namespace: reservation.namespace().to_string(),
                volume: self.source.clone(),
                output: self.output.clone(),
                task: key(reservation),
                fingerprint: Sha256::digest(fixed.encode()?).into(),
            },
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimFormatPlan {
    owner: u64,
    goal: Goal,
}
impl ReclaimFormatPlan {
    pub fn output(&self) -> Uuid {
        Uuid::parse_str(&self.goal.output).expect("validated format output")
    }
    pub fn scan(&self) -> Uuid {
        Uuid::parse_str(&self.goal.scan).expect("validated format scan")
    }
    pub fn owner(&self) -> u64 {
        self.owner
    }
    pub fn formatted_at(&self) -> &str {
        &self.goal.formatted_at
    }
    pub(crate) fn cartridge(&self) -> &crate::ltfs::mam::MediumIdentity {
        &self.goal.source_evidence.cartridge
    }
    pub(crate) fn source_identity(&self) -> Result<crate::ltfs::recovery::IndexSourceIdentity> {
        self.goal.source_evidence.identity()
    }
    pub(crate) fn source_label(&self) -> Result<crate::ltfs::label::LtfsLabel> {
        self.goal.source_evidence.old_label()
    }
    pub(crate) fn source_dp_eod(&self) -> u64 {
        self.goal.source_evidence.dp_eod()
    }
    pub(crate) fn require_source_capture(
        &self,
        actual: &crate::ltfs::recovery::CapturedIndexSource,
    ) -> Result<()> {
        self.goal.source_evidence.require_capture(actual)
    }
    pub fn attempt(&self) -> u32 {
        self.goal.attempt
    }
    pub fn issued(&self) -> bool {
        self.goal.issued
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimFormatAdmission {
    plan: ReclaimFormatPlan,
    needs_observation: bool,
}
impl ReclaimFormatAdmission {
    pub fn plan(&self) -> &ReclaimFormatPlan {
        &self.plan
    }
    pub fn needs_observation(&self) -> bool {
        self.needs_observation
    }
}
/// Actual read of the original cartridge's recoverable initialization state.
/// It cannot be deserialized from a saved plan or an error message.
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimFormatRetryEvidence {
    plan: ReclaimFormatPlan,
    observation: ResetObservation,
}
impl ReclaimFormatRetryEvidence {
    pub(crate) fn from_observation(
        plan: &ReclaimFormatPlan,
        owner: u64,
        medium: &crate::ltfs::mam::MediumIdentity,
        original: bool,
        sha256: [u8; 32],
    ) -> Result<Self> {
        if !plan.issued() || plan.owner() != owner || medium != plan.cartridge() {
            return Err(CatalogError::Invalid(
                "初始化结果实读不属于原介质、尝试或 owner",
            ));
        }
        Ok(Self {
            plan: plan.clone(),
            observation: ResetObservation {
                attempt: plan.attempt(),
                kind: if original { "original" } else { "partial" }.into(),
                sha256,
            },
        })
    }
}

/// Only a guarded, exact empty-recipe read mints this publication capability.
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimFormatCommitEvidence {
    plan: ReclaimFormatPlan,
    source: NativeMediaSource,
}
impl ReclaimFormatCommitEvidence {
    pub fn source(&self) -> &NativeMediaSource {
        &self.source
    }
    pub(crate) fn from_current(
        plan: &ReclaimFormatPlan,
        owner: u64,
        actual: &crate::ltfs::recovery::CapturedIndexSource,
    ) -> Result<Self> {
        use crate::ltfs::recovery::IndexSourceScope;
        if actual.construct_end() != crate::ltfs::volume::FIRST_INDEX_BLOCK + 2
            || !matches!(actual.scope(),IndexSourceScope::Current {dp_eod,ip_generation:Some(1),..} if dp_eod==actual.construct_end())
            || !actual.predecessors().is_empty()
        {
            return Err(CatalogError::Invalid(
                "初始化结果不是原固定空索引及完整两分区检查点",
            ));
        }
        Self::from_empty_head(plan, owner, actual.construct())
    }
    pub(crate) fn from_verified_reset(
        plan: &ReclaimFormatPlan,
        owner: u64,
        actual: crate::daemon::fdb_reclaim_format::VerifiedEmptyReset,
    ) -> Result<Self> {
        Self::from_empty_head(plan, owner, actual.head())
    }
    fn from_empty_head(
        plan: &ReclaimFormatPlan,
        owner: u64,
        index: crate::ltfs::volume::CommittedIndexConstruct,
    ) -> Result<Self> {
        let options = crate::ltfs::mkltfs::MkltfsOptions {
            volume_uuid: Some(plan.output()),
            quick: true,
            ..Default::default()
        };
        let records =
            crate::ltfs::mkltfs::reset_partition_records(&options, plan.formatted_at(), 1)
                .map_err(|e| CatalogError::Cache(e.to_string()))?;
        let expected = records[5].as_ref().expect("fixed empty index");
        if !plan.issued()
            || plan.owner() != owner
            || index.volume_uuid != plan.output()
            || index.generation != 1
            || index.incremental
            || index.location.partition != crate::ltfs::label::PART_DATA
            || index.location.start_block != crate::ltfs::volume::FIRST_INDEX_BLOCK
            || index.sha256 != <[u8; 32]>::from(Sha256::digest(expected))
        {
            return Err(CatalogError::Invalid(
                "初始化结果不是原固定空索引及完整两分区检查点",
            ));
        }
        let source = NativeMediaSource {
            identity: crate::ltfs::recovery::IndexSourceIdentity::new(index, Vec::new())
                .map_err(|e| CatalogError::Cache(e.to_string()))?,
            files: 0,
            paths: 0,
        };
        Ok(Self {
            plan: plan.clone(),
            source,
        })
    }
    pub(super) fn require(&self, plan: &ReclaimFormatPlan, owner: u64) -> Result<()> {
        if self.plan != *plan || plan.owner() != owner {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(())
    }
}

fn key(reservation: &ReclaimReservation) -> Vec<u8> {
    source::own_format_key(reservation)
}
fn output_reservation(output: Uuid) -> Vec<u8> {
    let mut key = super::super::key(b'5');
    key.extend_from_slice(output.as_bytes());
    key
}
async fn output_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    goal: &Goal,
    owned: bool,
) -> Result<()> {
    let output = node_mutation::id(&goal.output)?;
    let claim = goal.claim(reservation)?;
    // These non-snapshot reads use one transaction and do not depend on each
    // other's values. Start the point reads together before awaiting results.
    let claims = [
        output_reservation(output),
        native_media::volume_claim_key(output),
    ]
    .map(|key| tr.get(&key, false));
    let occupied = [
        placement_key(output),
        ready_volume_key(output),
        reconcile_volume_key(output),
        node_attempts::volume_key(output),
    ]
    .map(|key| tr.get(&key, false));
    for claim_read in claims {
        let raw = claim_read.await?;
        if owned && raw.as_deref() != Some(claim.as_slice()) {
            return Err(CatalogError::Corrupt("回收原输出 UUID 占用丢失或冲突"));
        }
        if !owned && raw.is_some() {
            return Err(CatalogError::Invalid("回收输出 UUID 已被其他请求保留"));
        }
    }
    let mut scans = super::super::key(b'G');
    scans.extend_from_slice(output.as_bytes());
    if requests::prefix_has_rows(tr, &scans).await? {
        return Err(CatalogError::Invalid(
            "回收输出 UUID 已有历史扫描，不能复用",
        ));
    }
    for occupancy_read in occupied {
        if occupancy_read.await?.is_some() {
            return Err(CatalogError::Invalid("回收输出 UUID 已被使用"));
        }
    }
    Ok(())
}
async fn at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<Option<ReclaimFormatPlan>> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    let raw = tr.get(&key(reservation), false).await?;
    let Some(raw) = raw else {
        if parent["phase"] == "reformatting" {
            return Err(CatalogError::Corrupt("回收格式化阶段缺少原目标"));
        }
        return Ok(None);
    };
    let goal = Goal::decode(&raw)?;
    if parent["phase"] != "reformatting"
        || goal.parent != reservation.fingerprint()
        || goal.source != reservation.spec().volume().to_string()
    {
        return Err(CatalogError::Corrupt("回收格式化目标与原父任务不同"));
    }
    output_at(tr, reservation, &goal, true).await?;
    Ok(Some(ReclaimFormatPlan { owner, goal }))
}
async fn gates_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    goal: Option<&Goal>,
) -> Result<[u8; 32]> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    gates_after_parent_at(tr, reservation, owner, goal).await
}

// The caller has checked the live parent and owner in this transaction.
async fn gates_after_parent_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    goal: Option<&Goal>,
) -> Result<[u8; 32]> {
    isolation::require_normal_mutation_at(tr, reservation, owner).await?;
    source::group_only::require_ready_at(tr, reservation, owner, None).await?;
    mechanics::require_parent_idle_at(tr, reservation).await?;
    if let Some(layout) = layout::at(tr, reservation).await? {
        layout.require_resource_scope_at(tr, reservation).await?;
        let destination = tape_monitor::read_at(
            tr,
            reservation.namespace(),
            reservation.serial(),
            &layout.destination.barcode,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
        // A full/write-protected destination may still retain verified data.
        // Known damage or an unverified head cannot authorize source reset.
        if destination.volume != layout.destination.volume
            || destination.media_fault.is_some()
            || destination.import_required
            || destination.requires_validation
        {
            return Err(CatalogError::MediaUnavailable);
        }
    } else if reservation.spec().logical_library().is_some() {
        return Err(CatalogError::Invalid("逻辑带库回收缺少原驱动器布局"));
    }
    upload_recovery::require_idle_at(tr, reservation).await?;
    copy_groups::require_no_unsettled_at(tr, reservation).await?;
    source_state_at(tr, reservation).await?;
    if !walk::current_state_at(tr, reservation).await?.complete() {
        return Err(CatalogError::Incomplete);
    }
    let coverage = source::require_complete_coverage_at(tr, reservation, owner).await?;
    let coverage = copy_groups::require_format_audit_at(tr, reservation, coverage).await?;
    let volume = reservation.spec().volume();
    let mut prefix = super::super::key(b'V');
    prefix.extend_from_slice(volume.as_bytes());
    if requests::prefix_has_rows(tr, &prefix).await? {
        return Err(CatalogError::Invalid(
            "源卷仍有其他版本、扫描或命名空间的保留引用，不能格式化",
        ));
    }
    let claim = goal
        .filter(|goal| goal.attempt > 0)
        .map(|goal| goal.claim(reservation))
        .transpose()?;
    native_device::require_volume_quiescent_with_claim_at(
        tr,
        reservation.namespace(),
        volume,
        claim.as_deref(),
    )
    .await?;
    Ok(coverage)
}
impl ManagedCatalog {
    /// The output UUID is explicit and retained; changing it on retry is an
    /// error. No gate, claim or format record changes on a failed precondition.
    pub async fn prepare_reclaim_format(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        output: Uuid,
        evidence: &ReclaimFormatSourceEvidence,
    ) -> Result<ReclaimFormatPlan> {
        if evidence.owner != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let formatted_at = crate::ltfs::label::ltfs_time_now();
        if output.is_nil() || output == reservation.spec().volume() {
            return Err(CatalogError::Invalid("回收格式化须固定不同的非空输出 UUID"));
        }
        let scan = Uuid::new_v4();
        self.db
            .run(|tr, _| {
                let formatted_at = &formatted_at;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            evidence.fact.require_at(&tr, reservation).await?;
                            if let Some(saved) = at(&tr, reservation, owner).await? {
                                if saved.output() != output
                                    || saved.goal.source_evidence != evidence.fact
                                {
                                    return Err(CatalogError::Invalid(
                                        "回收格式化重试改变原输出 UUID",
                                    ));
                                }
                                if gates_at(&tr, reservation, owner, Some(&saved.goal)).await?
                                    != saved.goal.coverage
                                {
                                    return Err(CatalogError::Corrupt("回收格式化原覆盖门槛改变"));
                                }
                                return Ok(saved);
                            }
                            let coverage = gates_at(&tr, reservation, owner, None).await?;
                            if tr.get(&placement_key(output), false).await?.is_some()
                                || tr.get(&ready_volume_key(output), false).await?.is_some()
                            {
                                return Err(CatalogError::Invalid("回收输出 UUID 已被使用"));
                            }
                            let goal = Goal {
                                version: 2,
                                parent: reservation.fingerprint(),
                                source: reservation.spec().volume().to_string(),
                                output: output.to_string(),
                                scan: scan.to_string(),
                                coverage,
                                issued: false,
                                attempt: 0,
                                last_observation: None,
                                formatted_at: formatted_at.clone(),
                                source_evidence: evidence.fact.clone(),
                            };
                            let raw = goal.encode()?;
                            output_at(&tr, reservation, &goal, false).await?;
                            let output_claim = goal.claim(reservation)?;
                            let mut parent = record_at(&tr, reservation, owner).await?;
                            parent["phase"] = json!("reformatting");
                            tr.set(&key(reservation), &raw);
                            tr.set(&output_reservation(output), &output_claim);
                            tr.set(&native_media::volume_claim_key(output), &output_claim);
                            tr.set(&reservation.journal_key(), parent.to_string().as_bytes());
                            Ok(ReclaimFormatPlan { owner, goal })
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    pub async fn reclaim_format_plan(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimFormatPlan>> {
        at(&self.db.create_trx()?, reservation, owner).await
    }
    /// Only a newly persisted issue can reach the first physical attempt.
    /// Lost replies and HA return observation-only, retaining the same goal.
    pub async fn admit_reclaim_format(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimFormatPlan,
    ) -> Result<ReclaimFormatAdmission> {
        if original.owner != owner {
            return Err(CatalogError::OwnerFenced);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let mut saved = at(&tr, reservation, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let mut before = saved.goal.clone();
                            before.issued = original.goal.issued;
                            before.attempt = original.goal.attempt;
                            before.last_observation = original.goal.last_observation.clone();
                            if before != original.goal
                                || original.goal.attempt > saved.goal.attempt
                                || !saved.goal.issued
                                    && (original.goal.issued
                                        || original.goal.attempt != saved.goal.attempt)
                            {
                                return Err(CatalogError::Invalid(
                                    "回收格式化准入改变原目标或尝试",
                                ));
                            }
                            if gates_at(&tr, reservation, owner, Some(&saved.goal)).await?
                                != saved.goal.coverage
                            {
                                return Err(CatalogError::Corrupt("回收格式化原覆盖门槛改变"));
                            }
                            let needs_observation = saved.goal.issued;
                            if !needs_observation {
                                saved.goal.attempt = saved
                                    .goal
                                    .attempt
                                    .checked_add(1)
                                    .filter(|n| *n <= 3)
                                    .ok_or(CatalogError::Invalid(
                                        "原初始化已达重试上限，保留原介质目标等待修复",
                                    ))?;
                                saved.goal.issued = true;
                                let raw = saved.goal.encode()?;
                                let claim = saved.goal.claim(reservation)?;
                                tr.set(&key(reservation), &raw);
                                tr.set(
                                    &node_attempts::volume_key(reservation.spec().volume()),
                                    &claim,
                                );
                            }
                            Ok(ReclaimFormatAdmission {
                                plan: saved,
                                needs_observation,
                            })
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    /// A fresh actual observation settles one unknown reset attempt. The old
    /// issue is never replayed; the next attempt needs its own persisted issue.
    pub async fn rearm_reclaim_format(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimFormatPlan,
        proof: &ReclaimFormatRetryEvidence,
    ) -> Result<ReclaimFormatPlan> {
        if original.owner() != owner || proof.plan != *original {
            return Err(CatalogError::OwnerFenced);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let mut saved = at(&tr, reservation, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let mut expected = original.clone();
                            expected.goal.issued = false;
                            expected.goal.last_observation = Some(proof.observation.clone());
                            if saved == expected {
                                return Ok(saved);
                            }
                            if saved != *original || !saved.issued() || saved.attempt() >= 3 {
                                return Err(CatalogError::Invalid(
                                    "原初始化快照已改变或已达有界恢复次数，保留原目标",
                                ));
                            }
                            if gates_at(&tr, reservation, owner, Some(&saved.goal)).await?
                                != saved.goal.coverage
                            {
                                return Err(CatalogError::Corrupt("初始化重试的双门槛改变"));
                            }
                            saved.goal.issued = false;
                            saved.goal.last_observation = Some(proof.observation.clone());
                            let raw = saved.goal.encode()?;
                            tr.set(&key(reservation), &raw);
                            Ok(saved)
                        }
                        .await,
                    )
                })
            })
            .await?
    }

    /// Fresh per-CDB recheck of the exact issue and both independent gates.
    pub async fn verify_reclaim_format(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimFormatPlan,
    ) -> Result<()> {
        if original.owner != owner || !original.issued() {
            return Err(CatalogError::OwnerFenced);
        }
        let tr = self.db.create_trx()?;
        let saved = at(&tr, reservation, owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if saved != *original
            || gates_after_parent_at(&tr, reservation, owner, Some(&saved.goal)).await?
                != saved.goal.coverage
        {
            return Err(CatalogError::Invalid("回收格式化原目标或双门槛改变"));
        }
        Ok(())
    }

    pub(crate) async fn verify_reclaim_format_for_route(
        &self,
        reservation: &ReclaimReservation,
        route: ActiveLibraryRoute,
        owner: u64,
        original: &ReclaimFormatPlan,
    ) -> Result<()> {
        if route != reservation.route {
            return Err(CatalogError::OwnerFenced);
        }
        // The parent check verifies this exact route and owner with the plan
        // and gates at one fresh read version.
        self.verify_reclaim_format(reservation, owner, original)
            .await
    }
}
