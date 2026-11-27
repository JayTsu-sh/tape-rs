//! Durable, bounded orchestration only. active_batch remains the sole S4.
use super::*;
pub(super) use index_augmentation::group_only_history_link_at;
pub(super) use index_augmentation::{GroupRootReference, require_group_root_coverage_at};
use serde::{Deserialize, Serialize};

#[path = "reclaim_group_attempt.rs"]
mod group_attempt;
pub(crate) use group_attempt::OriginalAttemptObservation;

#[path = "reclaim_index_augmentation.rs"]
mod index_augmentation;
pub(in crate::managed_catalog) use index_augmentation::isolation;
pub(in crate::managed_catalog) use index_augmentation::require_checkpoint_at as require_index_checkpoint_at;
pub(in crate::managed_catalog) use index_augmentation::require_publication_at as require_index_publication_at;
pub(crate) use index_augmentation::{
    OriginalCheckpointContext, OriginalCheckpointPublication, OriginalObservationPermit,
    ReclaimExecutionGate, ReclaimGroupExecution, ReclaimGroupMemberReadContext,
    ReclaimGroupObservation, ReclaimIndexAugmentation, ReclaimIndexIntent,
};

pub(in crate::managed_catalog) use index_augmentation::{
    GroupCoverageMode, GroupFileReference, GroupRetirementPins, group_member_coverage_at,
    group_member_coverage_in_at,
};

#[path = "reclaim_group_layout.rs"]
mod group_layout;
pub(super) async fn initialize_layout_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<()> {
    group_layout::initialize_at(tr, parent).await
}
pub(super) async fn require_format_audit_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    coverage: [u8; 32],
) -> Result<[u8; 32]> {
    if parent.intent()["version"] == 1 {
        return Ok(coverage);
    }
    group_layout::require_format_audit_at(tr, parent, coverage).await
}
pub(super) async fn group_layout_marker_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<()> {
    group_layout::require_at(tr, parent).await.map(|_| ())
}

const MAX_MEMBERS: u8 = 32;
const MAX_BYTES: u64 = 512 << 20;
const MAX_CONTROL_BYTES: usize = 16 << 10;

fn cache(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Cache(error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReclaimCopyGroupLimits {
    members: u8,
    bytes: u64,
}
impl ReclaimCopyGroupLimits {
    pub(crate) fn bounded(members: u8, bytes: u64) -> Result<Self> {
        if members == 0 || bytes == 0 {
            return Err(CatalogError::Invalid("回收组预算为空"));
        }
        Ok(Self {
            members: members.min(MAX_MEMBERS),
            bytes: bytes.min(MAX_BYTES),
        })
    }
    pub(crate) fn members(self) -> u8 {
        self.members
    }
    pub(crate) fn bytes(self) -> u64 {
        self.bytes
    }
    fn validate(self) -> Result<()> {
        if self.members == 0
            || self.members > MAX_MEMBERS
            || self.bytes == 0
            || self.bytes > MAX_BYTES
        {
            return Err(CatalogError::Corrupt("回收组原预算无效"));
        }
        Ok(())
    }
    fn fits(self, count: u8, bytes: u64, next: u64) -> bool {
        count < self.members && bytes.checked_add(next).is_some_and(|sum| sum <= self.bytes)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Selecting,
    Prepared,
    Admitted,
    Settling,
    Closed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildRef {
    operation: String,
    epoch: u64,
    request: [u8; 32],
    source_read: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    version: u8,
    parent: String,
    group: String,
    ordinal: u8,
    file: ReclaimFile,
    source_index: Vec<u8>,
    source_head: Vec<u8>,
    source_copy: [u8; 32],
    inventory: [u8; 32],
    node: [u8; 32],
    body_uid: u64,
    known_sha256: [u8; 32],
    physical: [u8; 32],
    block_size: u32,
    first: Option<(char, u64, u32)>,
    end: Option<(char, u64, u32)>,
    child: Option<ChildRef>,
    // Minted only by future actual per-member postproof settlement, never by
    // Selecting/Prepared/Admitted or by a caller-provided success boolean.
    settlement: Option<[u8; 32]>,
}
impl Member {
    fn validate(&self, parent: &ReclaimReservation) -> Result<()> {
        self.file.validate()?;
        active_volume::decode_index(parent.spec().volume(), &self.source_index)?;
        let head = decode_head(&self.source_head)?;
        if self.version != 1
            || self.parent != parent.fingerprint()
            || parse_uuid(&self.group)?.is_nil()
            || self.ordinal >= MAX_MEMBERS
            || self.body_uid == 0
            || self.block_size == 0
            || head.version != self.file.version()
            || head.size != self.file.size()
            || self.source_index
                != serde_json::from_value::<Vec<u8>>(parent.intent()["index"].clone())
                    .map_err(cache)?
            || (self.first.is_none() || self.end.is_none()) != (self.file.size() == 0)
            || self
                .first
                .zip(self.end)
                .is_some_and(|(a, b)| a >= b || a.0 != b.0)
        {
            return Err(CatalogError::Corrupt("回收组成员身份/物理能力不同"));
        }
        if let Some(child) = &self.child
            && (child.epoch == 0 || parse_uuid(&child.operation)? != self.file.operation())
        {
            return Err(CatalogError::Corrupt("回收组子请求不是原成员"));
        }
        Ok(())
    }
    fn digest(&self) -> Result<[u8; 32]> {
        let mut selected = self.clone();
        selected.child = None;
        selected.settlement = None;
        digest(b"tape-rs/reclaim-copy-group-member/1", &selected)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    parent: String,
    group: String,
    sequence: u64,
    batch: String,
    owner: u64,
    phase: Phase,
    limits: ReclaimCopyGroupLimits,
    selected: bool,
    count: u8,
    bytes: u64,
    members: [u8; 32],
    order: Vec<u8>,
    destination: ReclaimDriveBinding,
    layout: [u8; 32],
    scan: String,
    previous: Vec<u8>,
    execution_epoch: Option<u64>,
    current_attempt: Option<String>,
    commit: Option<[u8; 32]>,
}
impl Header {
    fn validate(&self, parent: &ReclaimReservation) -> Result<()> {
        self.limits.validate()?;
        let id = parse_uuid(&self.group)?;
        let batch = parse_uuid(&self.batch)?;
        parse_uuid(&self.scan)?;
        active_volume::decode_index(self.destination.volume, &self.previous)?;
        if self.version != 3
            || self.parent != parent.fingerprint()
            || (self.current_attempt.is_none() && batch != id)
            || self.sequence == 0
            || self.owner == 0
            || self.count == 0
            || self.count > self.limits.members
            || self.bytes > self.limits.bytes
            || self.destination.volume == parent.spec().volume()
            || self.destination.barcode == parent.spec().barcode()
            || self.destination.revision == 0
            || self.destination.drive_serial.is_empty()
            || self.phase != Phase::Selecting && !self.selected
            || self.order.len() != usize::from(self.count)
            || self
                .order
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                != (0..self.count).collect()
        {
            return Err(CatalogError::Corrupt("回收组固定header无效"));
        }
        match (self.phase, self.execution_epoch) {
            (Phase::Selecting | Phase::Prepared, None) if self.current_attempt.is_none() => {}
            (Phase::Admitted | Phase::Settling | Phase::Closed, Some(epoch))
                if epoch > 0
                    && epoch <= self.owner
                    && self
                        .current_attempt
                        .as_deref()
                        .is_some_and(|id| parse_uuid(id).is_ok()) => {}
            _ => return Err(CatalogError::Corrupt("回收组执行代次或未知阶段无效")),
        }
        if matches!(self.phase, Phase::Settling | Phase::Closed) && self.commit.is_none() {
            return Err(CatalogError::Corrupt("结清组缺少原batch提交引用"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub(crate) struct ReclaimCopyGroup {
    header: Header,
    members: Vec<Member>,
}
impl ReclaimCopyGroup {
    pub(crate) fn id(&self) -> Uuid {
        parse_uuid(&self.header.group).expect("checked group")
    }
    pub(crate) fn batch_id(&self) -> Uuid {
        parse_uuid(&self.header.batch).expect("checked original batch")
    }
    pub(crate) fn files(&self) -> impl Iterator<Item = &ReclaimFile> {
        self.members.iter().map(|member| &member.file)
    }
    pub(crate) fn ordered_ordinals(&self) -> impl Iterator<Item = u8> + '_ {
        self.header.order.iter().copied()
    }
    pub(crate) fn preparation_owner(&self) -> u64 {
        self.header.owner
    }
    pub(crate) fn destination_drive_serial(&self) -> &str {
        &self.header.destination.drive_serial
    }
    pub(crate) fn requires_observation(&self) -> bool {
        matches!(self.header.phase, Phase::Admitted | Phase::Settling)
    }
    pub(crate) fn source_ready(&self) -> bool {
        self.header.selected && self.header.phase == Phase::Selecting
    }
    pub(crate) fn prepared(&self) -> bool {
        self.header.phase == Phase::Prepared
    }
    pub(crate) fn require_source_candidate(
        &self,
        ordinal: u8,
        candidate: &source::ReclaimGroupCandidate,
    ) -> Result<()> {
        let member = self
            .members
            .get(usize::from(ordinal))
            .ok_or(CatalogError::Incomplete)?;
        if &member.file != candidate.file()
            || member.source_index != candidate.index()
            || member.source_head != candidate.head()
            || member.source_copy != candidate.copy_digest()
            || member.inventory != candidate.inventory()
            || member.node != candidate.node_digest()
            || member.body_uid != candidate.body_uid()
            || member.known_sha256 != candidate.known_sha256()
            || member.physical != candidate.physical_digest()
            || member.block_size != candidate.block_size()
            || (member.first, member.end) != candidate.range()
        {
            return Err(CatalogError::Invalid("回收源正文布局与原封存组成员不同"));
        }
        Ok(())
    }
    fn require_members(&self, parent: &ReclaimReservation) -> Result<()> {
        self.header.validate(parent)?;
        if self.members.len() != usize::from(self.header.count) {
            return Err(CatalogError::Incomplete);
        }
        let mut bytes = 0u64;
        let mut ids = std::collections::BTreeSet::new();
        let mut files = std::collections::BTreeSet::new();
        for (ordinal, member) in self.members.iter().enumerate() {
            member.validate(parent)?;
            if member.group != self.header.group
                || usize::from(member.ordinal) != ordinal
                || !ids.insert(member.file.operation())
                || !files.insert(member.file.file_id())
                || (self.header.phase != Phase::Selecting && member.child.is_none())
                || (self.header.phase == Phase::Closed && member.settlement.is_none())
            {
                return Err(CatalogError::Corrupt("回收组原成员缺失/重复/未结清"));
            }
            bytes = bytes
                .checked_add(member.file.size())
                .ok_or(CatalogError::Corrupt("回收组字节计数溢出"))?;
        }
        let (digest, order) = membership(&self.members)?;
        if bytes != self.header.bytes || digest != self.header.members || order != self.header.order
        {
            return Err(CatalogError::Corrupt("回收组成员集合或执行顺序改变"));
        }
        Ok(())
    }
}
pub(crate) enum ReclaimCopyGroupChoice<'a> {
    Advance,
    Candidate(&'a source::ReclaimGroupCandidate),
    Single(&'a ReclaimFile),
}
pub(crate) enum ReclaimCopyGroupSelection {
    Scanning,
    NeedsCandidate(ReclaimFile),
    Group(ReclaimCopyGroup),
    Single(ReclaimFile),
    Done,
}
/// A persisted admission checkpoint, not a CDB permit or S4 proof.
pub(crate) struct ReclaimCopyGroupAdmission {
    group: ReclaimCopyGroup,
    needs_observation: bool,
}
impl ReclaimCopyGroupAdmission {
    pub(crate) fn group(&self) -> &ReclaimCopyGroup {
        &self.group
    }
    pub(crate) fn needs_observation(&self) -> bool {
        self.needs_observation
    }
}

fn prefix(parent: &ReclaimReservation) -> Vec<u8> {
    let mut prefix = domain(
        parent.namespace(),
        crate::managed_catalog::RECLAIM_COPY_GROUP_DOMAIN,
    );
    prefix.extend_from_slice(
        &parent.intent()["epoch"]
            .as_u64()
            .expect("valid epoch")
            .to_be_bytes(),
    );
    prefix.extend_from_slice(
        parse_uuid(parent.intent()["operation"].as_str().expect("valid op"))
            .expect("valid op")
            .as_bytes(),
    );
    prefix
}
fn key(parent: &ReclaimReservation, kind: u8, id: Option<Uuid>, ordinal: Option<u8>) -> Vec<u8> {
    match (kind, id) {
        (b'h' | b'm' | b'i' | b'n', Some(group)) => {
            group_layout::canonical_key(parent, group, kind, ordinal)
        }
        // Current initial-attempt callers pass its original group identity;
        // linked-attempt APIs use attempt_key with both explicit identities.
        (b'a' | b'd' | b'p' | b'b', Some(group)) => {
            group_layout::attempt_key(parent, group, group, kind, ordinal)
        }
        _ => group_layout::control_key(parent, kind, id),
    }
}

fn header_prefix(parent: &ReclaimReservation) -> Vec<u8> {
    key(parent, b'h', None, None)
}
fn member_prefix(parent: &ReclaimReservation, id: Uuid) -> Vec<u8> {
    key(parent, b'm', Some(id), None)
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(value).map_err(cache)?;
    if raw.len() > MAX_CONTROL_BYTES {
        return Err(CatalogError::Invalid("回收组单行控制记录超限"));
    }
    Ok(raw)
}
fn decode<T: for<'a> Deserialize<'a> + Serialize>(raw: &[u8]) -> Result<T> {
    if raw.len() > MAX_CONTROL_BYTES {
        return Err(CatalogError::Corrupt("回收组单行控制记录超限"));
    }
    let value: T = serde_json::from_slice(raw).map_err(cache)?;
    if encode(&value)? != raw {
        return Err(CatalogError::Corrupt("回收组记录非规范编码"));
    }
    Ok(value)
}
fn parse_uuid(value: &str) -> Result<Uuid> {
    node_mutation::id(value)
}
fn digest<T: Serialize>(domain: &[u8], value: &T) -> Result<[u8; 32]> {
    Ok(Sha256::new()
        .chain_update(domain)
        .chain_update(encode(value)?)
        .finalize()
        .into())
}
fn membership(rows: &[Member]) -> Result<([u8; 32], Vec<u8>)> {
    let mut hash = Sha256::new().chain_update(b"tape-rs/reclaim-copy-group-members/1");
    for member in rows {
        hash.update(member.digest()?);
    }
    Ok((
        hash.finalize().into(),
        physical_order(
            rows.iter()
                .map(|member| (member.ordinal, member.first, member.end)),
        )?,
    ))
}

/// Shared pure geometry check; metadata qualification cannot defer a Single
/// boundary until after root metadata has already been written.
pub(super) fn physical_order(
    rows: impl IntoIterator<Item = (u8, Option<(char, u64, u32)>, Option<(char, u64, u32)>)>,
) -> Result<Vec<u8>> {
    let mut ordered = rows.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(ordinal, first, _)| (*first, *ordinal));
    let mut end = None;
    for (_, first, next) in &ordered {
        if let Some(first) = first {
            if end.is_some_and(|previous| previous > *first) {
                return Err(CatalogError::Invalid(
                    "不同文件extent交错，须在封组前走原单文件路径",
                ));
            }
            end = *next;
        }
    }
    Ok(ordered.into_iter().map(|(ordinal, _, _)| ordinal).collect())
}

async fn parent_at(tr: &Transaction, parent: &ReclaimReservation, owner: u64) -> Result<()> {
    let row = record_at(tr, parent, owner).await?;
    if parent.intent()["version"] != 2 {
        return Err(CatalogError::Invalid("原v1父任务不迁入组协议"));
    }
    group_layout::require_at(tr, parent).await?;
    if row["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if !matches!(row["phase"].as_str(), Some("reserved" | "copying")) {
        return Err(CatalogError::Invalid("当前父阶段不能编排复制组"));
    }
    Ok(())
}
async fn load_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
) -> Result<Option<ReclaimCopyGroup>> {
    parent_at(tr, parent, owner).await?;
    let pointer = tr.get(&key(parent, b'a', None, None), false).await?;
    let pending = read_page(tr, &key(parent, b'u', None, None), None, 2, false).await?;
    let progress = group_layout::require_at(tr, parent).await?;
    let Some(pointer) = pointer else {
        if !pending.items.is_empty() || !progress.idle() {
            return Err(CatalogError::Corrupt("原a/u指针缺失但issued组尚未关闭"));
        }
        let rows = read_page(tr, &key(parent, b's', None, None), None, 2, false).await?;
        if !rows.items.is_empty() {
            let walk = walk::current_state_at(tr, parent).await?;
            if rows.next.is_some()
                || rows.items.len() != 1
                || !walk.current().is_some_and(|file| {
                    rows.items[0].0 == key(parent, b's', Some(file.operation()), None)
                        && encode(file).is_ok_and(|raw| raw == rows.items[0].1)
                })
            {
                return Err(CatalogError::Corrupt("原Single控制记录不一致"));
            }
        }
        return Ok(None);
    };
    let id = Uuid::from_slice(&pointer).map_err(|_| CatalogError::Corrupt("原组指针无效"))?;
    if pending.items.len() != 1
        || pending.next.is_some()
        || pending.items[0].0 != key(parent, b'u', Some(id), None)
    {
        return Err(CatalogError::Corrupt("原组pending索引不完整"));
    }
    let header: Header = decode(
        &tr.get(&key(parent, b'h', Some(id), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    if parse_uuid(&header.group)? != id || header.owner > owner || header.phase == Phase::Closed {
        return Err(CatalogError::Corrupt("原active组身份/owner/终态错误"));
    }
    let rows = read_page(
        tr,
        &member_prefix(parent, id),
        None,
        usize::from(MAX_MEMBERS) + 1,
        false,
    )
    .await?;
    if rows.next.is_some() || rows.items.len() != usize::from(header.count) {
        return Err(CatalogError::Corrupt("原组成员行数量不同"));
    }
    let mut members = Vec::with_capacity(rows.items.len());
    for (ordinal, (key, raw)) in rows.items.into_iter().enumerate() {
        if key != self::key(parent, b'm', Some(id), Some(ordinal as u8)) {
            return Err(CatalogError::Corrupt("原组ordinal键改变"));
        }
        members.push(decode(&raw)?);
    }
    let group = ReclaimCopyGroup { header, members };
    group.require_members(parent)?;
    group_layout::require_identity_at(tr, parent, &group).await?;
    if group.header.sequence != progress.issued_count
        || group.header.sequence
            != progress
                .closed_count
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || pending.items[0].1 != group.header.sequence.to_be_bytes()
    {
        return Err(CatalogError::Corrupt("原active组与issued/u序号不同"));
    }
    source_state_at(tr, parent).await?;
    if matches!(group.header.phase, Phase::Selecting | Phase::Prepared) {
        require_previous_at(tr, parent, owner, &group).await?;
        let inventory = source::history_inventory_at(tr, parent).await?;
        for member in &group.members {
            if member.inventory != inventory {
                return Err(CatalogError::Invalid("原组完整源清单改变"));
            }
            require_source_at(tr, parent, member).await?;
        }
    }
    // v2 also owns bounded native-input and actual-plan rows. Validate their
    // complete manifests before allowing them into the domain whitelist.
    let canonical = group_layout::canonical_group_prefix(parent, id);
    let all = read_page(
        tr,
        &canonical,
        None,
        usize::from(MAX_MEMBERS) * 4 + 16,
        false,
    )
    .await?;
    let mut expected = std::collections::BTreeSet::from([key(parent, b'h', Some(id), None)]);
    for member in &group.members {
        expected.insert(key(parent, b'm', Some(id), Some(member.ordinal)));
        let reverse = key(parent, b'r', Some(member.file.operation()), None);
        let original = [id.as_bytes().as_slice(), &[member.ordinal]].concat();
        if tr.get(&reverse, false).await?.as_deref() != Some(original.as_slice()) {
            return Err(CatalogError::Corrupt("原组成员反向身份丢失"));
        }
    }
    if group.header.version == 3 {
        index_augmentation::extend_expected_at(tr, parent, &group, &mut expected).await?;
    }
    group_attempt::extend_expected_at(tr, parent, &group, &mut expected).await?;
    if all.next.is_some()
        || all.items.len() != expected.len()
        || all.items.iter().any(|(key, _)| !expected.contains(key))
    {
        return Err(CatalogError::Corrupt("原组域存在额外成员或孤儿记录"));
    }
    Ok(Some(group))
}
pub(super) async fn remaining_slots_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    volume: Uuid,
) -> Result<u64> {
    let pool_id = parent.spec().pool();
    let raw = tr
        .get(&super::super::pool_key(parent.namespace(), pool_id), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let pool = super::super::decode_pool(pool_id, &raw)?;
    let count = active_volume::file_count_at(tr, parent.serial(), volume).await?;
    // One member contributes its managed donor and its archive reference.
    // Both are physical LTFS files in the shared final index.
    Ok(pool.file_limit.saturating_sub(count) / 2)
}
async fn require_previous_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    let header = &group.header;
    parent_at(tr, parent, owner).await?;
    source_state_at(tr, parent).await?;
    source::require_metadata_idle_at(tr, parent).await?;
    mechanics::require_parent_idle_at(tr, parent).await?;
    require_parent_pool_at(tr, parent).await?;
    let layout = layout::at(tr, parent)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if layout.destination != header.destination
        || digest(b"tape-rs/reclaim-group-layout/1", &layout)? != header.layout
    {
        return Err(CatalogError::Invalid("原组固定layout改变"));
    }
    let (scan, index) =
        active_volume::observed_at(tr, parent.serial(), header.destination.volume).await?;
    let mut bytes = Vec::new();
    active_volume::encode_index(&mut bytes, index);
    if scan.id.to_string() != header.scan || bytes != header.previous {
        return Err(CatalogError::Invalid("原组目标previous/scan已改变"));
    }
    if group.members.iter().any(|member| member.child.is_some()) {
        let pins = prepared_group_pins_at(tr, parent, owner, group).await?;
        native_device::require_prepared_reclaim_group_quiescent_at(tr, &pins).await?;
    } else {
        // Selection may be constructing an as-yet-unpersisted member. Without
        // a prepared child there is no group-owned pin to exempt.
        native_device::require_volume_quiescent_at(
            tr,
            parent.namespace(),
            header.destination.volume,
        )
        .await?;
    }
    if u64::from(header.count) > remaining_slots_at(tr, parent, header.destination.volume).await? {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}
/// A read-version-bound exception for exact original history pins only.
/// This is neither a write permit nor a namespace-wide quiescence override.
pub(in crate::managed_catalog) struct PreparedGroupPins<'a> {
    transaction: &'a Transaction,
    namespace: Uuid,
    volume: Uuid,
    pins: std::collections::BTreeSet<Vec<u8>>,
}
impl PreparedGroupPins<'_> {
    pub(in crate::managed_catalog) fn require_transaction(&self, tr: &Transaction) -> Result<()> {
        if !std::ptr::eq(self.transaction, tr) {
            return Err(CatalogError::Invalid("原组历史pin证明属于其他FDB事务"));
        }
        Ok(())
    }
    pub(in crate::managed_catalog) fn namespace(&self) -> Uuid {
        self.namespace
    }
    pub(in crate::managed_catalog) fn volume(&self) -> Uuid {
        self.volume
    }
    pub(in crate::managed_catalog) fn keys(&self) -> &std::collections::BTreeSet<Vec<u8>> {
        &self.pins
    }
}
async fn prepared_group_pins_at<'a>(
    tr: &'a Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<PreparedGroupPins<'a>> {
    parent_at(tr, parent, owner).await?;
    group.require_members(parent)?;
    if !matches!(group.header.phase, Phase::Selecting | Phase::Prepared)
        || !group.header.selected
        || group.header.owner > owner
        || tr
            .get(&key(parent, b'h', Some(group.id()), None), false)
            .await?
            .as_deref()
            != Some(encode(&group.header)?.as_slice())
    {
        return Err(CatalogError::Invalid("历史pin例外必须属于原未准入持久组"));
    }
    group_layout::require_identity_at(tr, parent, group).await?;
    let mut pins = std::collections::BTreeSet::new();
    for member in &group.members {
        if tr
            .get(
                &key(parent, b'm', Some(group.id()), Some(member.ordinal)),
                false,
            )
            .await?
            .as_deref()
            != Some(encode(member)?.as_slice())
            || tr
                .get(
                    &key(parent, b'r', Some(member.file.operation()), None),
                    false,
                )
                .await?
                .as_deref()
                != Some(
                    [group.id().as_bytes().as_slice(), &[member.ordinal]]
                        .concat()
                        .as_slice(),
                )
        {
            return Err(CatalogError::Corrupt("原组历史pin成员或反向身份改变"));
        }
        require_source_at(tr, parent, member).await?;
        if member.child.is_some() {
            let original = original_child_at(tr, parent, owner, member).await?;
            let pin = children::history_pin_key(&original.request());
            if tr.get(&pin, false).await?.as_deref() != Some(&[][..]) || !pins.insert(pin) {
                return Err(CatalogError::Corrupt(
                    "原组已准备child历史pin缺失、非空或重复",
                ));
            }
        }
    }
    Ok(PreparedGroupPins {
        transaction: tr,
        namespace: parent.namespace(),
        volume: group.header.destination.volume,
        pins,
    })
}
async fn require_source_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    member: &Member,
) -> Result<()> {
    let path = pool_catalog_path(parent.spec().pool(), member.file.path().as_bytes())?;
    let Some(ResolvedPath::File {
        id,
        head,
        copy,
        placement,
        ..
    }) = lookup::resolve_active_path_at_mode(
        tr,
        parent.namespace(),
        lookup::path_components(&path)?,
        false,
    )
    .await?
    else {
        return Err(CatalogError::Invalid("原组源文件丢失"));
    };
    if id != member.file.file_id()
        || encode_head(&head) != member.source_head
        || <[u8; 32]>::from(Sha256::digest(copy_records::encode_physical_copy(&copy)?))
            != member.source_copy
        || copy.media.is_some()
        || copy.content_digest != ContentDigest::Sha256(member.known_sha256)
        || placement.barcode != parent.spec().barcode()
        || copy.volume_id != parent.spec().volume()
    {
        return Err(CatalogError::Invalid("原组源R/head/正文能力已改变"));
    }
    Ok(())
}
fn save_header(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    group.require_members(parent)?;
    tr.set(
        &key(parent, b'h', Some(group.id()), None),
        &encode(&group.header)?,
    );
    Ok(())
}

/// A prepared group has no physical attempt yet. Once admitted, advance its
/// live owner only through the original attempt validator, in this transaction.
async fn adopt_group_owner_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &mut ReclaimCopyGroup,
    owner: u64,
) -> Result<()> {
    if group.requires_observation() {
        isolation::require_group_action_at(tr, parent, owner, group).await?;
        group_attempt::adopt_at(tr, parent, group, owner).await?;
    } else {
        group.header.owner = owner;
    }
    save_header(tr, parent, group)
}

impl ManagedCatalog {
    /// One bounded selector transaction. No body, Q, receipt, or tape write.
    pub(crate) async fn select_reclaim_copy_group(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        limits: ReclaimCopyGroupLimits,
        choice: ReclaimCopyGroupChoice<'_>,
    ) -> Result<ReclaimCopyGroupSelection> {
        limits.validate()?;
        let id = Uuid::new_v4();
        let walk_operation = Uuid::new_v4();
        self.db
            .run(|tr, _| {
                let choice = &choice;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, parent, owner).await?;
                            source::group_only::require_ready_at(
                                &tr,
                                parent,
                                owner,
                                Some((limits.bytes(), u64::MAX)),
                            )
                            .await?;
                            source::group_only::require_selector_at(&tr, parent, limits, choice)
                                .await?;
                            let id = source::group_only::planned_group_at(&tr, parent)
                                .await?
                                .unwrap_or(id);
                            let mut group = load_at(&tr, parent, owner).await?;
                            if let Some(current) = &mut group {
                                if current.header.owner != owner {
                                    adopt_group_owner_at(&tr, parent, current, owner).await?;
                                }
                                if current.header.selected
                                    || current.header.phase != Phase::Selecting
                                {
                                    source::group_only::require_complete_selection_at(
                                        &tr, parent, current,
                                    )
                                    .await?;
                                    return Ok(ReclaimCopyGroupSelection::Group(current.clone()));
                                }
                            }
                            source_state_at(&tr, parent).await?;
                            // An uninitialised inventory is not an empty source.
                            source::history_inventory_at(&tr, parent).await?;
                            let state = walk::current_state_at(&tr, parent).await?;
                            let state = if state.current().is_none() && !state.complete() {
                                walk::advance_at(&tr, parent, owner, walk_operation).await?
                            } else {
                                state
                            };
                            if let Some(file) = state.current()
                                && tr
                                    .get(&key(parent, b's', Some(file.operation()), None), false)
                                    .await?
                                    .is_some()
                            {
                                if parent.spec().group_only() {
                                    return Err(CatalogError::Invalid(
                                        "group-only不接受原Single控制记录",
                                    ));
                                }
                                return Ok(ReclaimCopyGroupSelection::Single(file.clone()));
                            }
                            let Some(file) = state.current() else {
                                if state.complete() {
                                    if let Some(mut current) = group {
                                        source::group_only::require_complete_selection_at(
                                            &tr, parent, &current,
                                        )
                                        .await?;
                                        current.header.selected = true;
                                        save_header(&tr, parent, &current)?;
                                        return Ok(ReclaimCopyGroupSelection::Group(current));
                                    }
                                    return Ok(ReclaimCopyGroupSelection::Done);
                                }
                                return Ok(ReclaimCopyGroupSelection::Scanning);
                            };
                            let candidate = match choice {
                                ReclaimCopyGroupChoice::Advance => {
                                    return Ok(ReclaimCopyGroupSelection::NeedsCandidate(
                                        file.clone(),
                                    ));
                                }
                                ReclaimCopyGroupChoice::Single(original) if *original == file => {
                                    None
                                }
                                ReclaimCopyGroupChoice::Candidate(candidate)
                                    if candidate.file() == file =>
                                {
                                    Some(*candidate)
                                }
                                _ => return Err(CatalogError::Invalid("组选择改变原walk当前文件")),
                            };
                            let fallback = candidate.is_none() || !limits.fits(0, 0, file.size());
                            if fallback {
                                if parent.spec().group_only() {
                                    return Err(CatalogError::Invalid(
                                        "group-only原候选或单组预算失效，禁止回退",
                                    ));
                                }
                                if let Some(mut current) = group {
                                    current.header.selected = true;
                                    save_header(&tr, parent, &current)?;
                                    return Ok(ReclaimCopyGroupSelection::Group(current));
                                }
                                // Explicit non-group continuation; absence of a group edge is
                                // never sufficient to authorize a v2 generic writer.
                                tr.set(
                                    &key(parent, b's', Some(file.operation()), None),
                                    &encode(file)?,
                                );
                                return Ok(ReclaimCopyGroupSelection::Single(file.clone()));
                            }
                            let candidate = candidate.expect("checked candidate");
                            candidate.require_at(&tr, parent, owner).await?;
                            let target_slots = if let Some(current) = &group {
                                remaining_slots_at(&tr, parent, current.header.destination.volume)
                                    .await?
                            } else {
                                u64::from(MAX_MEMBERS)
                            };
                            if let Some(current) = &group
                                && (u64::from(current.header.count) >= target_slots
                                    || !current.header.limits.fits(
                                        current.header.count,
                                        current.header.bytes,
                                        file.size(),
                                    )
                                    || !limits.fits(
                                        current.header.count,
                                        current.header.bytes,
                                        file.size(),
                                    ))
                            {
                                if parent.spec().group_only() {
                                    return Err(CatalogError::Invalid(
                                        "group-only禁止因预算或额度变化截短原组",
                                    ));
                                }
                                let mut current = current.clone();
                                current.header.selected = true;
                                save_header(&tr, parent, &current)?;
                                return Ok(ReclaimCopyGroupSelection::Group(current));
                            }
                            let mut current = if let Some(current) = group {
                                current
                            } else {
                                children::require_no_children_at(&tr, parent).await?;
                                let layout = layout::at(&tr, parent)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                let (scan, index) = active_volume::observed_at(
                                    &tr,
                                    parent.serial(),
                                    layout.destination.volume,
                                )
                                .await?;
                                let mut previous = Vec::new();
                                active_volume::encode_index(&mut previous, index);
                                let slots =
                                    remaining_slots_at(&tr, parent, layout.destination.volume)
                                        .await?;
                                if slots == 0 {
                                    return Err(CatalogError::Incomplete);
                                }
                                let limits = ReclaimCopyGroupLimits {
                                    members: limits
                                        .members
                                        .min(slots.min(u64::from(MAX_MEMBERS)) as u8),
                                    bytes: limits.bytes,
                                };
                                ReclaimCopyGroup {
                                    header: Header {
                                        version: 3,
                                        parent: parent.fingerprint(),
                                        group: id.to_string(),
                                        sequence: group_layout::next_sequence_at(&tr, parent)
                                            .await?,
                                        batch: id.to_string(),
                                        owner,
                                        phase: Phase::Selecting,
                                        limits,
                                        selected: false,
                                        count: 0,
                                        bytes: 0,
                                        members: [0; 32],
                                        order: Vec::new(),
                                        destination: layout.destination.clone(),
                                        layout: digest(b"tape-rs/reclaim-group-layout/1", &layout)?,
                                        scan: scan.id.to_string(),
                                        previous,
                                        execution_epoch: None,
                                        current_attempt: None,
                                        commit: None,
                                    },
                                    members: Vec::new(),
                                }
                            };
                            let (first, end) = candidate.range();
                            let member = Member {
                                version: 1,
                                parent: parent.fingerprint(),
                                group: current.header.group.clone(),
                                ordinal: current.header.count,
                                file: file.clone(),
                                source_index: candidate.index().to_vec(),
                                source_head: candidate.head().to_vec(),
                                source_copy: candidate.copy_digest(),
                                inventory: candidate.inventory(),
                                node: candidate.node_digest(),
                                body_uid: candidate.body_uid(),
                                known_sha256: candidate.known_sha256(),
                                physical: candidate.physical_digest(),
                                block_size: candidate.block_size(),
                                first,
                                end,
                                child: None,
                                settlement: None,
                            };
                            if tr
                                .get(&key(parent, b'r', Some(file.operation()), None), false)
                                .await?
                                .is_some()
                            {
                                return Err(CatalogError::Corrupt("原walk operation已属于组"));
                            }
                            current.members.push(member.clone());
                            let (digest, order) = match membership(&current.members) {
                                Ok(value) => value,
                                Err(_)
                                    if current.header.count > 0 && !parent.spec().group_only() =>
                                {
                                    current.members.pop();
                                    current.header.selected = true;
                                    save_header(&tr, parent, &current)?;
                                    return Ok(ReclaimCopyGroupSelection::Group(current));
                                }
                                Err(error) => return Err(error),
                            };
                            current.header.count += 1;
                            current.header.bytes = current
                                .header
                                .bytes
                                .checked_add(file.size())
                                .ok_or(CatalogError::Corrupt("组字节计数溢出"))?;
                            current.header.members = digest;
                            current.header.order = order;
                            current.header.selected = current.header.count
                                == current.header.limits.members
                                || current.header.bytes == current.header.limits.bytes;
                            if current.header.selected {
                                source::group_only::require_complete_selection_at(
                                    &tr, parent, &current,
                                )
                                .await?;
                            }
                            require_previous_at(&tr, parent, owner, &current).await?;
                            let walk =
                                walk::transfer_current_to_group_at(&tr, parent, file).await?;
                            let member_raw = encode(&member)?;
                            let header_raw = encode(&current.header)?;
                            current.require_members(parent)?;
                            // All mutations commit atomically with the original root and cursor.
                            tr.set(
                                &key(parent, b'm', Some(current.id()), Some(member.ordinal)),
                                &member_raw,
                            );
                            tr.set(
                                &key(parent, b'r', Some(file.operation()), None),
                                &[current.id().as_bytes().as_slice(), &[member.ordinal]].concat(),
                            );
                            tr.set(&key(parent, b'h', Some(current.id()), None), &header_raw);
                            if current.header.count == 1 {
                                group_layout::issue_at(&tr, parent, &current).await?;
                            }
                            tr.set(&walk::state_key(parent), &walk);
                            Ok(ReclaimCopyGroupSelection::Group(current))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }

    pub(crate) async fn freeze_reclaim_copy_group(
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
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id {
                                return Err(CatalogError::Invalid("封组必须复用原group ID"));
                            }
                            source::group_only::require_complete_selection_at(&tr, parent, &group)
                                .await?;
                            if group.header.phase == Phase::Selecting && !group.header.selected {
                                require_previous_at(&tr, parent, owner, &group).await?;
                                group.header.selected = true;
                            }
                            adopt_group_owner_at(&tr, parent, &mut group, owner).await?;
                            Ok(group)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    /// Parent-only restoration never allocates a replacement group or batch.
    pub(crate) async fn resume_reclaim_copy_group(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimCopyGroup>> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let mut group = load_at(&tr, parent, owner).await?;
                            if let Some(group) = &mut group
                                && group.header.owner != owner
                            {
                                adopt_group_owner_at(&tr, parent, group, owner).await?;
                            }
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

impl ManagedCatalog {
    pub(crate) async fn step_reclaim_group_history_audit(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<bool> {
        if parent.intent()["version"] == 1 {
            return Ok(true);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_at(&tr, parent, owner).await?;
                            source::require_metadata_idle_at(&tr, parent).await?;
                            if !walk::current_state_at(&tr, parent).await?.complete() {
                                return Err(CatalogError::Incomplete);
                            }
                            let coverage =
                                source::require_complete_coverage_at(&tr, parent, owner).await?;
                            group_layout::step_audit_at(&tr, parent, owner, coverage).await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

/// Conservative first orchestration slice: no durable group state can be
/// interpreted as settled without the later independently checked close seal.
/// This includes Closed-shaped data, absent pointers, and explicit Singles.
/// Public v1 tasks with an empty domain retain their original behavior.
pub(super) async fn require_no_unsettled_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<()> {
    if parent.intent()["version"] == 1 {
        if requests::prefix_has_rows(tr, &prefix(parent)).await? {
            return Err(CatalogError::PoolBusy);
        }
        return Ok(());
    }
    let progress = group_layout::require_at(tr, parent).await?;
    if !progress.idle()
        || tr
            .get(&key(parent, b'a', None, None), false)
            .await?
            .is_some()
        || requests::prefix_has_rows(tr, &key(parent, b'u', None, None)).await?
        || requests::prefix_has_rows(tr, &key(parent, b's', None, None)).await?
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn require_no_group_recovery_at(
    tr: &Transaction,
    volume: Uuid,
) -> Result<()> {
    let Some(placement) = control::verified_placement(tr, volume).await? else {
        return Ok(());
    };
    let Some(token) = pool_gate_at(tr, placement.namespace, placement.pool_id).await? else {
        return Ok(());
    };
    let mut journal = domain(placement.namespace, b'j');
    journal.extend_from_slice(
        &token["epoch"]
            .as_u64()
            .ok_or(CatalogError::Incomplete)?
            .to_be_bytes(),
    );
    journal.extend_from_slice(
        parse_uuid(
            token["operation"]
                .as_str()
                .ok_or(CatalogError::Incomplete)?,
        )?
        .as_bytes(),
    );
    let raw = tr
        .get(&journal, false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let saved = super::decode(&raw)?;
    if saved["intent"]["version"] == 1 {
        return Ok(());
    }
    validate_parent_intent(&saved["intent"])?;
    let route = routing::decode_active(
        &tr.get(&routing::active_key(&placement.library_serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let parent = ReclaimReservation {
        intent: saved["intent"].clone(),
        route,
    };
    let owner = saved["owner_epoch"]
        .as_u64()
        .ok_or(CatalogError::Incomplete)?;
    if let Some(group) = load_at(tr, &parent, owner).await?
        && matches!(group.header.phase, Phase::Admitted | Phase::Settling)
        && [parent.spec().volume(), group.header.destination.volume].contains(&volume)
    {
        return Err(CatalogError::Invalid(
            "原已准入回收组必须先结清原attempt，不能普通收尾或取消",
        ));
    }
    Ok(())
}

fn require_upload_spec(
    member: &Member,
    parent: &ReclaimReservation,
    spec: &ActiveUploadSpec,
) -> Result<()> {
    if spec.pool_id != parent.spec().pool()
        || spec.operation_id != member.file.operation()
        || spec.path != member.file.path().as_bytes()
        || spec.expected_version != Some(member.file.version())
        || spec.size != member.file.size()
        || spec.digest != ContentDigest::Sha256(member.known_sha256)
    {
        return Err(CatalogError::Invalid("原组成员与固定复制请求不符"));
    }
    Ok(())
}
fn read_digest(proof: &ReclaimSourceRead) -> Result<[u8; 32]> {
    let bytes = proof.value()?.to_string().into_bytes();
    if bytes.len() > 47_000 {
        return Err(CatalogError::Invalid("原组源实读证明超限"));
    }
    Ok(Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-source-read/1")
        .chain_update(bytes)
        .finalize()
        .into())
}
async fn original_child_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    member: &Member,
) -> Result<ActiveFileIntent> {
    let child = member.child.as_ref().ok_or(CatalogError::Incomplete)?;
    let (mut original, proof) =
        children::require_group_child_at(tr, parent, owner, member.file.operation(), child.epoch)
            .await?;
    if original.request().fingerprint() != child.request
        || read_digest(&proof)? != child.source_read
        || original.file_id != member.file.file_id()
        || original.expected_version != Some(member.file.version())
        || original.expected_size != member.file.size()
        || original.expected_digest != ContentDigest::Sha256(member.known_sha256)
    {
        return Err(CatalogError::Corrupt("原组成员/子请求/源实读证明已改变"));
    }
    original.owner_epoch = owner;
    Ok(original)
}
impl ManagedCatalog {
    /// The sealed group-only plan already fixes the destination before the
    /// selector materializes the member rows. This grants only read access.
    pub(crate) async fn check_early_reclaim_destination_preflight(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        layout: &ReclaimLayout,
    ) -> Result<Option<(Uuid, VolumeScan, ActiveIndexIdentity)>> {
        if !parent.spec().group_only() || !parent.groups_archive_root() {
            return Ok(None);
        }
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, parent.serial(), parent.route, owner).await?;
        parent_at(&tr, parent, owner).await?;
        let Some((group_id, scan_id, head)) =
            source::group_only::planned_preflight_at(&tr, parent, layout).await?
        else {
            return Ok(None);
        };
        if let Some(pointer) = tr.get(&key(parent, b'a', None, None), false).await? {
            let id =
                Uuid::from_slice(&pointer).map_err(|_| CatalogError::Corrupt("原组指针无效"))?;
            if id != group_id {
                return Err(CatalogError::Invalid("回收目标预检原组改变"));
            }
            let header: Header = decode(
                &tr.get(&key(parent, b'h', Some(id), None), false)
                    .await?
                    .ok_or(CatalogError::Incomplete)?,
            )?;
            header.validate(parent)?;
            if header.phase != Phase::Selecting || header.owner != owner {
                return Ok(None);
            }
        }
        source_state_at(&tr, parent).await?;
        source::require_metadata_idle_at(&tr, parent).await?;
        require_parent_pool_at(&tr, parent).await?;
        match children::require_no_children_at(&tr, parent).await {
            Err(CatalogError::PoolBusy) => return Ok(None),
            other => other?,
        }
        if layout::at(&tr, parent).await?.as_ref() != Some(layout) {
            return Err(CatalogError::Invalid("回收目标预检原固定布局改变"));
        }
        let assignment =
            Self::verified_assignment(&tr, parent.serial(), &layout.destination.barcode)
                .await?
                .ok_or(CatalogError::Incomplete)?;
        if assignment.namespace != parent.namespace()
            || assignment.pool_id != parent.spec().pool()
            || assignment.volume_id != Some(layout.destination.volume)
            || assignment.revision != layout.destination.revision
            || !assignment.allows_io()
        {
            return Err(CatalogError::MediaUnavailable);
        }
        let actual =
            active_volume::observed_at(&tr, parent.serial(), layout.destination.volume).await?;
        if actual.0.id != scan_id || actual.1 != head {
            return Ok(None);
        }
        native_device::require_volume_quiescent_at(
            &tr,
            parent.namespace(),
            layout.destination.volume,
        )
        .await?;
        Ok(Some((group_id, actual.0, head)))
    }

    /// Read-only check for a whole group before preparing its first (or next)
    /// child. Existing original children must still be unstarted.
    pub(crate) async fn check_reclaim_group_source_selection(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        expected: &ReclaimCopyGroup,
        layout: &ReclaimLayout,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        self.require_group_source_selection_at(&tr, parent, owner, expected, layout)
            .await
    }

    /// Preflight can read the original target before source proofs create the
    /// children. It does not confer admission or permission to write.
    pub(crate) async fn check_reclaim_group_destination_preflight(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        expected: &ReclaimCopyGroup,
        layout: &ReclaimLayout,
    ) -> Result<(VolumeScan, ActiveIndexIdentity)> {
        let tr = self.db.create_trx()?;
        self.require_group_source_selection_at(&tr, parent, owner, expected, layout)
            .await?;
        require_previous_at(&tr, parent, owner, expected).await?;
        active_volume::observed_at(&tr, parent.serial(), layout.destination.volume).await
    }

    async fn require_group_source_selection_at(
        &self,
        tr: &Transaction,
        parent: &ReclaimReservation,
        owner: u64,
        expected: &ReclaimCopyGroup,
        layout: &ReclaimLayout,
    ) -> Result<()> {
        routing::require_runtime_active_owner(tr, parent.serial(), parent.route, owner).await?;
        let group = load_at(tr, parent, owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if !parent.spec().group_only()
            || group.header != expected.header
            || group.members != expected.members
            || !(group.source_ready() || group.prepared())
            || layout::at(tr, parent).await?.as_ref() != Some(layout)
        {
            return Err(CatalogError::Invalid(
                "顺序源读必须绑定原布局与完整未准入组",
            ));
        }
        source::group_only::require_complete_selection_at(tr, parent, &group).await?;
        for member in &group.members {
            if member.child.is_some() {
                let original = original_child_at(tr, parent, owner, member).await?;
                reservation::require_unstarted_at(tr, &original).await?;
            } else if children::has_original_identity_at(
                tr,
                parent.namespace(),
                member.file.operation(),
            )
            .await?
            {
                return Err(CatalogError::Corrupt("原child存在但缺固定组映射"));
            }
        }
        Ok(())
    }

    pub(crate) async fn check_reclaim_group_preparation(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        expected: &ReclaimCopyGroup,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let group = load_at(&tr, parent, owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if group.header != expected.header
            || group.members != expected.members
            || !group.header.selected
            || !matches!(group.header.phase, Phase::Selecting | Phase::Prepared)
        {
            return Err(CatalogError::Invalid("回收准备捕获必须绑定原完整未准入组"));
        }
        require_previous_at(&tr, parent, owner, &group).await?;
        for member in &group.members {
            let original = original_child_at(&tr, parent, owner, member).await?;
            reservation::require_unstarted_at(&tr, &original).await?;
        }
        Ok(())
    }
}

impl ManagedCatalog {
    /// Child preparation and its immutable group edge share one transaction.
    /// This accepts the original byte-read proof, not metadata-only Candidate.
    /// No Q, S4, CDB permit or shared source-scope proof is produced here.
    pub(crate) async fn prepare_reclaim_group_member(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        ordinal: u8,
        spec: &ActiveUploadSpec,
        proof: &ReclaimSourceRead,
    ) -> Result<ActiveFileIntent> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || !group.header.selected
                                || !matches!(group.header.phase, Phase::Selecting | Phase::Prepared)
                            {
                                return Err(CatalogError::Invalid(
                                    "只能准备已封集合的原Selecting组",
                                ));
                            }
                            let member = group
                                .members
                                .get(usize::from(ordinal))
                                .ok_or(CatalogError::Invalid("原组ordinal不存在"))?;
                            require_upload_spec(member, parent, spec)?;
                            if proof.sha256() != member.known_sha256
                                || proof.source_node().uid() != member.body_uid
                                || proof.index() != member.source_index
                            {
                                return Err(CatalogError::Invalid("原组源实读UID/卷头/SHA不同"));
                            }
                            let evidence = proof.value()?;
                            if evidence["inventory"] != json!(member.inventory)
                                || evidence["object"]["sha256"] != json!(member.node)
                            {
                                return Err(CatalogError::Invalid("组原库存或节点与实读证明不同"));
                            }
                            let proof_digest = read_digest(proof)?;
                            if let Some(child) = &member.child {
                                if child.source_read != proof_digest {
                                    return Err(CatalogError::Invalid("重放改变原源实读证明"));
                                }
                                let original =
                                    original_child_at(&tr, parent, owner, member).await?;
                                group.header.owner = owner;
                                save_header(&tr, parent, &group)?;
                                return Ok(original);
                            }
                            // An existing child without this exact durable edge is not fresh.
                            if children::has_original_identity_at(
                                &tr,
                                parent.namespace(),
                                member.file.operation(),
                            )
                            .await?
                            {
                                return Err(CatalogError::Corrupt("原child存在但缺固定组映射"));
                            }
                            let original = children::prepare_upload_at(
                                &tr,
                                parent,
                                owner,
                                &group.header.destination.barcode,
                                spec,
                                proof,
                            )
                            .await?;
                            let member = &mut group.members[usize::from(ordinal)];
                            member.child = Some(ChildRef {
                                operation: member.file.operation().to_string(),
                                epoch: original.request_epoch,
                                request: original.request().fingerprint(),
                                source_read: proof_digest,
                            });
                            let raw = encode(member)?;
                            group.header.owner = owner;
                            save_header(&tr, parent, &group)?;
                            tr.set(&key(parent, b'm', Some(id), Some(ordinal)), &raw);
                            Ok(original)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub(crate) async fn seal_reclaim_copy_group(
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
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            source::group_only::require_complete_selection_at(&tr, parent, &group)
                                .await?;
                            if group.id() != id || !group.header.selected {
                                return Err(CatalogError::Invalid("不能改变原封组ID或集合"));
                            }
                            if !matches!(group.header.phase, Phase::Selecting | Phase::Prepared) {
                                return Err(CatalogError::Invalid("原组已准入，先观察恢复"));
                            }
                            let previous = active_volume::decode_index(
                                group.header.destination.volume,
                                &group.header.previous,
                            )?;
                            let generation = previous
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("组目标代次溢出"))?;
                            for member in &group.members {
                                let original =
                                    original_child_at(&tr, parent, owner, member).await?;
                                if original.volume_generation != generation
                                    || original.volume_id != group.header.destination.volume
                                    || original.barcode_constraint.as_deref()
                                        != Some(group.header.destination.barcode.as_str())
                                    || original.volume_scan_id.to_string() != group.header.scan
                                {
                                    return Err(CatalogError::Invalid("组原子请求目标或scan不同"));
                                }
                                reservation::require_unstarted_at(&tr, &original).await?;
                            }
                            if group.header.version == 3 {
                                index_augmentation::require_intent_at(&tr, parent, &group).await?;
                            }
                            group.header.phase = Phase::Prepared;
                            group.header.owner = owner;
                            save_header(&tr, parent, &group)?;
                            Ok(group)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    /// Durable may-have-started checkpoint only. It is not a device permit.
    /// Unknown commit/replay/HA must always observe the same original batch.
    pub(crate) async fn admit_reclaim_copy_group(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
        previous: ActiveIndexIdentity,
    ) -> Result<ReclaimCopyGroupAdmission> {
        let result = self
            .db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id {
                                return Err(CatalogError::Invalid("准入须复用原组和batch"));
                            }
                            source::group_only::require_admission_at(&tr, parent, owner, &group)
                                .await?;
                            for member in &group.members {
                                original_child_at(&tr, parent, owner, member).await?;
                            }
                            if group.header.version == 3 {
                                index_augmentation::require_intent_at(&tr, parent, &group).await?;
                            }
                            if matches!(group.header.phase, Phase::Admitted | Phase::Settling) {
                                group_attempt::adopt_at(&tr, parent, &mut group, owner).await?;
                                save_header(&tr, parent, &group)?;
                                return Ok(ReclaimCopyGroupAdmission {
                                    group,
                                    needs_observation: true,
                                });
                            }
                            if group.header.phase != Phase::Prepared {
                                return Err(CatalogError::Invalid("原组尚未完成独立成员准备"));
                            }
                            let mut bytes = Vec::new();
                            active_volume::encode_index(&mut bytes, previous);
                            if bytes != group.header.previous {
                                return Err(CatalogError::Invalid("准入目标previous不是原组基线"));
                            }
                            for member in &group.members {
                                let original =
                                    original_child_at(&tr, parent, owner, member).await?;
                                reservation::require_unstarted_at(&tr, &original).await?;
                            }
                            group.header.phase = Phase::Admitted;
                            group.header.owner = owner;
                            group.header.execution_epoch = Some(owner);
                            group.header.current_attempt = Some(group.id().to_string());
                            group_attempt::create_at(&tr, parent, &group).await?;
                            save_header(&tr, parent, &group)?;
                            Ok(ReclaimCopyGroupAdmission {
                                group,
                                needs_observation: false,
                            })
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?;
        #[cfg(test)]
        if result
            .as_ref()
            .is_ok_and(|admission| !admission.needs_observation())
        {
            group_attempt::creation_fault::after_admission_commit(parent)?;
        }
        result
    }
}

#[cfg(test)]
#[path = "reclaim_copy_groups_tests.rs"]
mod tests;
