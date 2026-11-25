//! Original-attempt observation interface. Fields are private and will only be
//! constructed by the versioned catalog adoption transaction, never by media.
use super::*;
use crate::ltfs::recovery::IndexSourceIdentity;

pub(crate) struct OriginalAttemptObservation {
    parent: ReclaimReservation,
    group: Uuid,
    attempt: Uuid,
    batch: Uuid,
    owner: u64,
    issuing_execution_epoch: u64,
    destination: ReclaimDriveBinding,
    base_source: IndexSourceIdentity,
    base_dp_eod: u64,
    actual: Option<ReclaimIndexAugmentation>,
    binding_digest: [u8; 32],
}
impl OriginalAttemptObservation {
    pub(crate) fn parent(&self) -> &ReclaimReservation {
        &self.parent
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
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }
    pub(crate) fn issuing_execution_epoch(&self) -> u64 {
        self.issuing_execution_epoch
    }
    pub(crate) fn namespace(&self) -> Uuid {
        self.parent.namespace()
    }
    pub(crate) fn destination_drive_serial(&self) -> &str {
        &self.destination.drive_serial
    }
    pub(crate) fn destination_volume(&self) -> Uuid {
        self.destination.volume
    }
    pub(crate) fn destination_barcode(&self) -> &str {
        &self.destination.barcode
    }
    pub(crate) fn base_source(&self) -> &IndexSourceIdentity {
        &self.base_source
    }
    pub(crate) fn base_dp_eod(&self) -> u64 {
        self.base_dp_eod
    }
    pub(crate) fn actual(&self) -> Option<&ReclaimIndexAugmentation> {
        self.actual.as_ref()
    }
    /// Original attempt plus the current adopted owner, not a physical permit.
    pub(crate) fn binding_digest(&self) -> [u8; 32] {
        self.binding_digest
    }
}

const MAX_ATTEMPT_BYTES: usize = 64 << 10;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptMember {
    ordinal: u8,
    child: ChildRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PhysicalPhase {
    Unresolved,
}
/// This is a may-have-started physical attempt, never a durable data receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhysicalAttempt {
    version: u8,
    parent: String,
    group: String,
    attempt: String,
    batch: String,
    predecessor: Option<String>,
    owner: u64,
    issuing_execution_epoch: u64,
    phase: PhysicalPhase,
    destination: ReclaimDriveBinding,
    scan: String,
    base_source: Vec<u8>,
    base_dp_eod: u64,
    intent: [u8; 32],
    members: [u8; 32],
    inventory: [u8; 32],
    ordered: Vec<AttemptMember>,
}
fn encode_attempt(row: &PhysicalAttempt) -> Result<Vec<u8>> {
    row.validate_shape()?;
    let raw = serde_json::to_vec(row).map_err(cache)?;
    if raw.len() > MAX_ATTEMPT_BYTES {
        return Err(CatalogError::Invalid("原physical attempt单行超过64KiB预算"));
    }
    Ok(raw)
}
fn decode_attempt(raw: &[u8]) -> Result<PhysicalAttempt> {
    if raw.len() > MAX_ATTEMPT_BYTES {
        return Err(CatalogError::Corrupt("原physical attempt单行超过64KiB预算"));
    }
    let row: PhysicalAttempt = serde_json::from_slice(raw).map_err(cache)?;
    if encode_attempt(&row)? != raw {
        return Err(CatalogError::Corrupt("原physical attempt编码不规范"));
    }
    Ok(row)
}
fn row_key(parent: &ReclaimReservation, group: &ReclaimCopyGroup) -> Result<Vec<u8>> {
    let attempt = parse_uuid(
        group
            .header
            .current_attempt
            .as_deref()
            .ok_or(CatalogError::Incomplete)?,
    )?;
    Ok(group_layout::attempt_key(
        parent,
        group.id(),
        attempt,
        b'w',
        None,
    ))
}
fn ordered_members(group: &ReclaimCopyGroup) -> Result<Vec<AttemptMember>> {
    group
        .header
        .order
        .iter()
        .map(|ordinal| {
            let member = group
                .members
                .get(usize::from(*ordinal))
                .ok_or(CatalogError::Incomplete)?;
            Ok(AttemptMember {
                ordinal: *ordinal,
                child: member.child.clone().ok_or(CatalogError::Incomplete)?,
            })
        })
        .collect()
}
impl PhysicalAttempt {
    fn validate_shape(&self) -> Result<()> {
        let base = IndexSourceIdentity::from_bytes(&self.base_source).map_err(cache)?;
        let expected = (0..self.ordered.len())
            .map(|ordinal| ordinal as u8)
            .collect::<std::collections::BTreeSet<_>>();
        let ordinals = self
            .ordered
            .iter()
            .map(|member| member.ordinal)
            .collect::<std::collections::BTreeSet<_>>();
        let mut operations = std::collections::BTreeSet::new();
        if self.version != 1
            || self.issuing_execution_epoch == 0
            || self.owner < self.issuing_execution_epoch
            || self.ordered.is_empty()
            || self.ordered.len() > usize::from(MAX_MEMBERS)
            || ordinals != expected
            || self.destination.volume != base.head().volume_uuid
            || self.base_dp_eod <= base.head().location.start_block
            || self.parent.len() != 64
            || self.destination.revision == 0
            || self.destination.drive_serial.is_empty()
        {
            return Err(CatalogError::Corrupt("原physical attempt结构或预算无效"));
        }
        parse_uuid(&self.group)?;
        parse_uuid(&self.attempt)?;
        parse_uuid(&self.batch)?;
        parse_uuid(&self.scan)?;
        if let Some(previous) = &self.predecessor {
            parse_uuid(previous)?;
        }
        for member in &self.ordered {
            let operation = parse_uuid(&member.child.operation)?;
            if member.child.epoch == 0 || !operations.insert(operation) {
                return Err(CatalogError::Corrupt("原physical attempt请求成员重复"));
            }
        }
        Ok(())
    }
    fn require(
        &self,
        parent: &ReclaimReservation,
        group: &ReclaimCopyGroup,
        intent: &ReclaimIndexIntent,
    ) -> Result<()> {
        let (base, eod) = intent.attempt_base()?;
        if self.version != 1
            || self.parent != parent.fingerprint()
            || self.group != group.header.group
            || Some(self.attempt.as_str()) != group.header.current_attempt.as_deref()
            || self.batch != group.header.batch
            || self.owner != group.header.owner
            || Some(self.issuing_execution_epoch) != group.header.execution_epoch
            || self.owner < self.issuing_execution_epoch
            || self.issuing_execution_epoch == 0
            || self.destination != group.header.destination
            || self.scan != group.header.scan
            || self.base_source != base.to_bytes()
            || self.base_dp_eod != eod
            || self.intent != intent.fingerprint()?
            || self.members != group.header.members
            || self.inventory
                != group
                    .members
                    .first()
                    .ok_or(CatalogError::Incomplete)?
                    .inventory
            || self.ordered != ordered_members(group)?
        {
            return Err(CatalogError::Corrupt(
                "原physical attempt/完整成员/索引意图封存改变",
            ));
        }
        // Linked successors require their own proven resolution protocol. This
        // first batch accepts only the original attempt; it never invents one.
        if self.predecessor.is_some() || self.attempt != self.group || self.batch != self.group {
            return Err(CatalogError::Invalid(
                "原physical attempt后继尚未完成恢复协议",
            ));
        }
        parse_uuid(&self.attempt)?;
        parse_uuid(&self.batch)?;
        parse_uuid(&self.scan)?;
        if eod == 0
            || ActiveIndexIdentity::from(base.head())
                != active_volume::decode_index(self.destination.volume, &group.header.previous)?
        {
            return Err(CatalogError::Corrupt(
                "原physical attempt基线/EOD不是原目标",
            ));
        }
        encode_attempt(self)?;
        Ok(())
    }
}
pub(super) async fn require_group_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    if matches!(group.header.phase, Phase::Selecting | Phase::Prepared) {
        return Ok(());
    }
    let intent = index_augmentation::require_intent_at(tr, parent, group).await?;
    read_at(tr, parent, group, &intent).await.map(|_| ())
}
async fn read_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    intent: &ReclaimIndexIntent,
) -> Result<PhysicalAttempt> {
    let raw = tr
        .get(&row_key(parent, group)?, false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let row = decode_attempt(&raw)?;
    row.require(parent, group, intent)?;
    Ok(row)
}
pub(super) async fn extend_expected_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    expected: &mut std::collections::BTreeSet<Vec<u8>>,
) -> Result<()> {
    if matches!(group.header.phase, Phase::Selecting | Phase::Prepared) {
        return Ok(());
    }
    require_group_at(tr, parent, group).await?;
    expected.insert(row_key(parent, group)?);
    Ok(())
}
pub(super) async fn create_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    if group.header.version != 3
        || group.header.phase != Phase::Admitted
        || group.header.execution_epoch != Some(group.header.owner)
    {
        return Err(CatalogError::Invalid("仅原首次准入可创建physical attempt"));
    }
    let intent = index_augmentation::require_intent_at(tr, parent, group).await?;
    let (base, eod) = intent.attempt_base()?;
    let row = PhysicalAttempt {
        version: 1,
        parent: parent.fingerprint(),
        group: group.header.group.clone(),
        attempt: group.id().to_string(),
        batch: group.batch_id().to_string(),
        predecessor: None,
        owner: group.header.owner,
        issuing_execution_epoch: group
            .header
            .execution_epoch
            .ok_or(CatalogError::Incomplete)?,
        phase: PhysicalPhase::Unresolved,
        destination: group.header.destination.clone(),
        scan: group.header.scan.clone(),
        base_source: base.to_bytes(),
        base_dp_eod: eod,
        intent: intent.fingerprint()?,
        members: group.header.members,
        inventory: group
            .members
            .first()
            .ok_or(CatalogError::Incomplete)?
            .inventory,
        ordered: ordered_members(group)?,
    };
    row.require(parent, group, &intent)?;
    let key = row_key(parent, group)?;
    if tr.get(&key, false).await?.is_some() {
        return Err(CatalogError::Corrupt("首次准入已有未登记physical attempt"));
    }
    let raw = encode_attempt(&row)?;
    #[cfg(test)]
    creation_fault::before_attempt_write(tr, parent, group).await?;
    tr.set(&key, &raw);
    #[cfg(test)]
    creation_fault::after_attempt_write(parent)?;
    Ok(())
}
pub(super) async fn adopt_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &mut ReclaimCopyGroup,
    owner: u64,
) -> Result<()> {
    let intent = index_augmentation::require_intent_at(tr, parent, group).await?;
    let mut row = read_at(tr, parent, group, &intent).await?;
    if owner < row.owner {
        return Err(CatalogError::OwnerFenced);
    }
    for member in &group.members {
        original_child_at(tr, parent, owner, member).await?;
    }
    row.owner = owner;
    group.header.owner = owner;
    row.require(parent, group, &intent)?;
    let raw = encode_attempt(&row)?;
    tr.set(&row_key(parent, group)?, &raw);
    Ok(())
}
pub(super) async fn scope_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    group: &ReclaimCopyGroup,
) -> Result<OriginalAttemptObservation> {
    if !matches!(group.header.phase, Phase::Admitted | Phase::Settling)
        || group.header.owner != owner
    {
        return Err(CatalogError::OwnerFenced);
    }
    let intent = index_augmentation::require_intent_at(tr, parent, group).await?;
    let row = read_at(tr, parent, group, &intent).await?;
    for member in &group.members {
        original_child_at(tr, parent, owner, member).await?;
    }
    let actual = index_augmentation::actual_for_attempt_at(tr, parent, group).await?;
    let actual_digest = actual
        .as_ref()
        .map(ReclaimIndexAugmentation::fingerprint)
        .transpose()?;
    let binding_digest = Sha256::new()
        .chain_update(b"tape-rs/reclaim-original-attempt-scope/1")
        .chain_update(encode_attempt(&row)?)
        .chain_update([u8::from(actual_digest.is_some())])
        .chain_update(actual_digest.unwrap_or([0; 32]))
        .finalize()
        .into();
    Ok(OriginalAttemptObservation {
        parent: parent.clone(),
        group: group.id(),
        attempt: parse_uuid(&row.attempt)?,
        batch: parse_uuid(&row.batch)?,
        owner,
        issuing_execution_epoch: row.issuing_execution_epoch,
        destination: row.destination.clone(),
        base_source: IndexSourceIdentity::from_bytes(&row.base_source).map_err(cache)?,
        base_dp_eod: row.base_dp_eod,
        actual,
        binding_digest,
    })
}
impl ManagedCatalog {
    /// Adopts only a fully persisted original attempt. Header3, Q/I and Actual
    /// may be absent; their absence never authorizes a replacement or append.
    pub(crate) async fn adopt_reclaim_original_attempt(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        id: Uuid,
    ) -> Result<OriginalAttemptObservation> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            routing::require_runtime_active_owner(
                                &tr,
                                parent.serial(),
                                parent.route,
                                owner,
                            )
                            .await?;
                            let mut group = load_at(&tr, parent, owner)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if group.id() != id
                                || !matches!(group.header.phase, Phase::Admitted | Phase::Settling)
                            {
                                return Err(CatalogError::Invalid(
                                    "接管必须发现原admitted physical attempt",
                                ));
                            }
                            isolation::require_group_action_at(&tr, parent, owner, &group).await?;
                            adopt_at(&tr, parent, &mut group, owner).await?;
                            save_header(&tr, parent, &group)?;
                            scope_at(&tr, parent, owner, &group).await
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub(crate) async fn check_reclaim_original_attempt(
        &self,
        scope: &OriginalAttemptObservation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        require_scope_at(&tr, scope).await
    }
}

pub(super) async fn require_scope_at(
    tr: &Transaction,
    scope: &OriginalAttemptObservation,
) -> Result<()> {
    routing::require_runtime_active_owner(
        tr,
        scope.parent.serial(),
        scope.parent.route,
        scope.owner,
    )
    .await?;
    let group = load_at(tr, &scope.parent, scope.owner)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let current = scope_at(tr, &scope.parent, scope.owner, &group).await?;
    if current.binding_digest != scope.binding_digest
        || current.group != scope.group
        || current.attempt != scope.attempt
        || current.batch != scope.batch
        || current.destination != scope.destination
        || current.base_source != scope.base_source
        || current.base_dp_eod != scope.base_dp_eod
    {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}

fn attempt_digest<T: Serialize>(domain: &[u8], value: &T) -> Result<[u8; 32]> {
    let raw = serde_json::to_vec(value).map_err(cache)?;
    if raw.len() > MAX_ATTEMPT_BYTES {
        return Err(CatalogError::Invalid("原attempt身份封存超过64KiB"));
    }
    Ok(Sha256::new()
        .chain_update(domain)
        .chain_update(raw)
        .finalize()
        .into())
}
pub(super) fn raw_fingerprint(raw: &[u8]) -> Result<[u8; 32]> {
    let row = decode_attempt(raw)?;
    // Historical identity excludes live adoption owner and mutable resolution
    // phase; live scope authority separately binds the complete current row.
    attempt_digest(
        b"tape-rs/reclaim-physical-attempt-identity/1",
        &(
            row.version,
            row.parent,
            row.group,
            row.attempt,
            row.batch,
            row.predecessor,
            row.issuing_execution_epoch,
            row.destination,
            row.scan,
            row.base_source,
            row.base_dp_eod,
            row.intent,
            row.members,
            row.inventory,
            row.ordered,
        ),
    )
}
pub(super) async fn fingerprint_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<[u8; 32]> {
    require_group_at(tr, parent, group).await?;
    raw_fingerprint(
        &tr.get(&row_key(parent, group)?, false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    // Pure private codec fixture: this does not mint an observation capability.
    fn row() -> PhysicalAttempt {
        let volume = Uuid::from_u128(1);
        let group = Uuid::from_u128(2);
        let base = IndexSourceIdentity::new(
            crate::ltfs::volume::CommittedIndexConstruct {
                volume_uuid: volume,
                generation: 1,
                location: crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 5,
                },
                sha256: [4; 32],
                incremental: false,
            },
            Vec::new(),
        )
        .unwrap();
        PhysicalAttempt {
            version: 1,
            parent: "a".repeat(64),
            group: group.to_string(),
            attempt: group.to_string(),
            batch: group.to_string(),
            predecessor: None,
            owner: 9,
            issuing_execution_epoch: 7,
            phase: PhysicalPhase::Unresolved,
            destination: ReclaimDriveBinding {
                barcode: "ATTEMPTL8".into(),
                volume,
                revision: 1,
                drive_serial: "SIM-ATTEMPT".into(),
                address: 100,
                home: 200,
            },
            scan: Uuid::from_u128(3).to_string(),
            base_source: base.to_bytes(),
            base_dp_eod: 8,
            intent: [5; 32],
            members: [6; 32],
            inventory: [7; 32],
            ordered: vec![AttemptMember {
                ordinal: 0,
                child: ChildRef {
                    operation: Uuid::from_u128(4).to_string(),
                    epoch: 1,
                    request: [8; 32],
                    source_read: [9; 32],
                },
            }],
        }
    }
    #[test]
    fn canonical_attempt_keeps_original_issuer_separate_from_adopted_owner() {
        let row = row();
        let raw = encode_attempt(&row).unwrap();
        let decoded = decode_attempt(&raw).unwrap();
        assert_eq!(row, decoded);
        assert_eq!(decoded.owner, 9);
        assert_eq!(decoded.issuing_execution_epoch, 7);
        let mut changed = decoded.clone();
        changed.owner = 10;
        assert_eq!(
            raw_fingerprint(&raw).unwrap(),
            raw_fingerprint(&encode_attempt(&changed).unwrap()).unwrap()
        );
        assert_eq!(changed.issuing_execution_epoch, row.issuing_execution_epoch);
    }
    #[test]
    fn bounded_attempt_supports_all_members_and_full_predecessor_chain() {
        let mut row = row();
        let volume = row.destination.volume;
        let head = crate::ltfs::volume::CommittedIndexConstruct {
            volume_uuid: volume,
            generation: 65,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: 650,
            },
            sha256: [255; 32],
            incremental: true,
        };
        let predecessors = (0..64u64)
            .map(|at| crate::ltfs::volume::CommittedIndexConstruct {
                volume_uuid: volume,
                generation: 64 - at,
                location: crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 640 - at * 10,
                },
                sha256: [254; 32],
                incremental: at < 63,
            })
            .collect();
        row.base_source = IndexSourceIdentity::new(head, predecessors)
            .unwrap()
            .to_bytes();
        row.base_dp_eod = 654;
        let member = row.ordered[0].clone();
        row.ordered = (0..MAX_MEMBERS)
            .rev()
            .map(|ordinal| {
                let mut member = member.clone();
                member.ordinal = ordinal;
                member.child.operation = Uuid::from_u128(100 + u128::from(ordinal)).to_string();
                member
            })
            .collect();
        let raw = encode_attempt(&row).unwrap();
        assert!(raw.len() > MAX_CONTROL_BYTES);
        assert!(raw.len() <= MAX_ATTEMPT_BYTES);
        assert_eq!(decode_attempt(&raw).unwrap(), row);
        raw_fingerprint(&raw).unwrap();
    }
    #[test]
    fn attempt_codec_rejects_unknown_version_duplicate_members_and_truncated_identity() {
        let valid = row();
        for change in 0..5 {
            let mut row = valid.clone();
            match change {
                0 => row.version = 2,
                1 => row.owner = 6,
                2 => row.ordered.push(row.ordered[0].clone()),
                3 => {
                    row.base_source.pop();
                }
                _ => row.ordered[0].ordinal = 1,
            };
            let raw = serde_json::to_vec(&row).unwrap();
            assert!(decode_attempt(&raw).is_err(), "mutation{change}");
        }
        let mut value = serde_json::to_value(&valid).unwrap();
        value["unexpected"] = json!(true);
        assert!(decode_attempt(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut raw = encode_attempt(&valid).unwrap();
        raw.push(b' ');
        assert!(decode_attempt(&raw).is_err());
    }
}

#[cfg(test)]
pub(super) mod creation_fault {
    use super::*;
    use std::sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    };
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Point {
        AfterWrite,
        AfterCommit,
    }
    struct Armed {
        point: Point,
        parent: String,
        fired: AtomicBool,
        snapshot: Mutex<Option<Snapshot>>,
    }
    struct Snapshot {
        prefixes: Vec<Vec<u8>>,
        rows: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    }
    fn arms() -> &'static Mutex<Vec<Arc<Armed>>> {
        static ARMS: OnceLock<Mutex<Vec<Arc<Armed>>>> = OnceLock::new();
        ARMS.get_or_init(|| Mutex::new(Vec::new()))
    }
    pub(crate) struct Fault(Arc<Armed>);
    impl Fault {
        pub(crate) async fn verify_unchanged(&self, catalog: &ManagedCatalog) -> Result<usize> {
            let (prefixes, before) = {
                let saved = self.0.snapshot.lock().unwrap();
                let saved = saved.as_ref().ok_or(CatalogError::Incomplete)?;
                (saved.prefixes.clone(), saved.rows.clone())
            };
            let tr = catalog.db.create_trx()?;
            let after = snapshot_at(&tr, &prefixes).await?;
            if before != after {
                return Err(CatalogError::Corrupt(
                    "原attempt拒绝后Header/current/0I/F/Q/quota或卷占用发生半套提交",
                ));
            }
            Ok(before.len())
        }
        pub(crate) fn fired(&self) -> bool {
            self.0.fired.load(Ordering::SeqCst)
        }
    }
    impl Drop for Fault {
        fn drop(&mut self) {
            arms()
                .lock()
                .unwrap()
                .retain(|old| !Arc::ptr_eq(old, &self.0));
        }
    }
    pub(crate) fn arm(parent: &ReclaimReservation) -> Fault {
        arm_at(parent, Point::AfterWrite)
    }
    pub(crate) fn arm_unknown(parent: &ReclaimReservation) -> Fault {
        arm_at(parent, Point::AfterCommit)
    }
    fn arm_at(parent: &ReclaimReservation, point: Point) -> Fault {
        let armed = Arc::new(Armed {
            point,
            parent: parent.fingerprint(),
            fired: AtomicBool::new(false),
            snapshot: Mutex::new(None),
        });
        let mut all = arms().lock().unwrap();
        assert!(!all.iter().any(|old| old.parent == armed.parent));
        all.push(armed.clone());
        Fault(armed)
    }
    async fn snapshot_at(
        tr: &Transaction,
        prefixes: &[Vec<u8>],
    ) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut out = std::collections::BTreeMap::new();
        for prefix in prefixes {
            let rows = read_page(tr, prefix, None, 2048, false).await?;
            if rows.next.is_some() {
                return Err(CatalogError::Incomplete);
            }
            out.extend(rows.items);
        }
        Ok(out)
    }
    pub(super) async fn before_attempt_write(
        tr: &Transaction,
        parent: &ReclaimReservation,
        group: &ReclaimCopyGroup,
    ) -> Result<()> {
        let armed = arms()
            .lock()
            .unwrap()
            .iter()
            .find(|armed| {
                armed.point == Point::AfterWrite
                    && armed.parent == parent.fingerprint()
                    && !armed.fired.load(Ordering::SeqCst)
            })
            .cloned();
        let Some(armed) = armed else {
            return Ok(());
        };
        let mut prefixes = vec![prefix(parent)];
        for kind in *b"0IFBDAKQq1" {
            let mut key = crate::managed_catalog::key(kind);
            key.extend_from_slice(parent.namespace().as_bytes());
            prefixes.push(key);
        }
        for kind in *b"24p" {
            let mut key = crate::managed_catalog::key(kind);
            key.extend_from_slice(group.header.destination.volume.as_bytes());
            prefixes.push(key);
        }
        let rows = snapshot_at(tr, &prefixes).await?;
        *armed.snapshot.lock().unwrap() = Some(Snapshot { prefixes, rows });
        Ok(())
    }
    pub(super) fn after_attempt_write(parent: &ReclaimReservation) -> Result<()> {
        if let Some(armed) = arms().lock().unwrap().iter().find(|armed| {
            armed.point == Point::AfterWrite
                && armed.parent == parent.fingerprint()
                && !armed.fired.load(Ordering::SeqCst)
        }) {
            armed.fired.store(true, Ordering::SeqCst);
            return Err(CatalogError::Invalid(
                "测试原attempt已暂存但Header/currentedge写前domain拒绝",
            ));
        }
        Ok(())
    }
    pub(crate) fn after_admission_commit(parent: &ReclaimReservation) -> Result<()> {
        if let Some(armed) = arms().lock().unwrap().iter().find(|armed| {
            armed.point == Point::AfterCommit
                && armed.parent == parent.fingerprint()
                && !armed.fired.load(Ordering::SeqCst)
        }) {
            armed.fired.store(true, Ordering::SeqCst);
            return Err(CatalogError::Invalid(
                "测试原admit已提交但回执丢失，必须只读观察原attempt",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
impl ManagedCatalog {
    pub(crate) fn fail_original_attempt_creation_once(
        &self,
        parent: &ReclaimReservation,
    ) -> creation_fault::Fault {
        creation_fault::arm(parent)
    }
    pub(crate) fn lose_original_attempt_admission_once(
        &self,
        parent: &ReclaimReservation,
    ) -> creation_fault::Fault {
        creation_fault::arm_unknown(parent)
    }
}

#[cfg(test)]
impl ManagedCatalog {
    pub(crate) async fn original_attempt_identity_for_test(
        &self,
        scope: &OriginalAttemptObservation,
    ) -> Result<[u8; 32]> {
        let tr = self.db.create_trx()?;
        let group = load_at(&tr, &scope.parent, scope.owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        fingerprint_at(&tr, &scope.parent, &group).await
    }
    pub(crate) async fn reject_missing_original_attempt_for_test(
        &self,
        scope: &OriginalAttemptObservation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let group = load_at(&tr, &scope.parent, scope.owner)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let key = row_key(&scope.parent, &group)?;
        let raw = tr
            .get(&key, false)
            .await?
            .ok_or(CatalogError::Incomplete)?
            .to_vec();
        tr.clear(&key);
        tr.commit().await.map_err(|error| {
            CatalogError::Cache(format!("测试原attempt缺失提交未知: {error:?}"))
        })?;
        let tr = self.db.create_trx()?;
        let before = read_page(&tr, &prefix(&scope.parent), None, 2048, false).await?;
        if before.next.is_some() {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);
        let result = self
            .adopt_reclaim_original_attempt(&scope.parent, scope.owner, scope.group)
            .await;
        let tr = self.db.create_trx()?;
        let after = read_page(&tr, &prefix(&scope.parent), None, 2048, false).await?;
        let unchanged = before.items == after.items
            && before.next == after.next
            && tr.get(&key, false).await?.is_none()
            && result.is_err();
        tr.set(&key, &raw);
        tr.commit().await.map_err(|error| {
            CatalogError::Cache(format!("测试原attempt精确恢复提交未知: {error:?}"))
        })?;
        if !unchanged {
            return Err(CatalogError::Corrupt("缺原attempt拒绝后补造或更改原组"));
        }
        Ok(())
    }
}
