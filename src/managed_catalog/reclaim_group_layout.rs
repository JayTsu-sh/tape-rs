//! Versioned group-centered canonical storage. Control indexes are derived;
//! history is retained and never mistaken for pending work by prefix presence.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u8,
    parent: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Progress {
    version: u8,
    parent: String,
    pub(super) issued_count: u64,
    issued_identity_digest: [u8; 32],
    pub(super) closed_count: u64,
    closed_identity_digest: [u8; 32],
    close_seal_digest: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Issued {
    version: u8,
    parent: String,
    sequence: u64,
    group: String,
}

#[path = "reclaim_group_audit.rs"]
mod audit;

pub(super) fn control_key(parent: &ReclaimReservation, kind: u8, id: Option<Uuid>) -> Vec<u8> {
    let mut out = prefix(parent);
    out.push(kind);
    if let Some(id) = id {
        out.extend_from_slice(id.as_bytes());
    }
    out
}
pub(super) fn canonical_group_prefix(parent: &ReclaimReservation, group: Uuid) -> Vec<u8> {
    let mut out = control_key(parent, b'g', Some(group));
    out.push(b'/');
    out
}
pub(super) fn canonical_key(
    parent: &ReclaimReservation,
    group: Uuid,
    kind: u8,
    ordinal: Option<u8>,
) -> Vec<u8> {
    let mut out = canonical_group_prefix(parent, group);
    out.push(kind);
    if let Some(ordinal) = ordinal {
        out.push(ordinal);
    }
    out
}
pub(super) fn attempt_key(
    parent: &ReclaimReservation,
    group: Uuid,
    attempt: Uuid,
    kind: u8,
    ordinal: Option<u8>,
) -> Vec<u8> {
    let mut out = canonical_group_prefix(parent, group);
    out.push(b't');
    out.extend_from_slice(attempt.as_bytes());
    out.push(b'/');
    out.push(kind);
    if let Some(ordinal) = ordinal {
        out.push(ordinal);
    }
    out
}
fn issued_key(parent: &ReclaimReservation, sequence: u64) -> Vec<u8> {
    let mut out = control_key(parent, b'q', None);
    out.extend_from_slice(&sequence.to_be_bytes());
    out
}
fn identity_seed(parent: &ReclaimReservation) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-identities/2")
        .chain_update(parent.fingerprint().as_bytes())
        .finalize()
        .into()
}
fn close_seed(parent: &ReclaimReservation) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-close-seals/2")
        .chain_update(parent.fingerprint().as_bytes())
        .finalize()
        .into()
}
fn identity_step(previous: [u8; 32], issued: &Issued) -> Result<[u8; 32]> {
    digest(b"tape-rs/reclaim-group-identity-row/2", &(previous, issued))
}
impl Progress {
    fn new(parent: &ReclaimReservation) -> Self {
        Self {
            version: 2,
            parent: parent.fingerprint(),
            issued_count: 0,
            closed_count: 0,
            issued_identity_digest: identity_seed(parent),
            closed_identity_digest: identity_seed(parent),
            close_seal_digest: close_seed(parent),
        }
    }
    fn validate(&self, parent: &ReclaimReservation) -> Result<()> {
        if self.version != 2
            || self.parent != parent.fingerprint()
            || self.closed_count > self.issued_count
            || self.issued_count - self.closed_count > 1
            || (self.issued_count == self.closed_count
                && self.issued_identity_digest != self.closed_identity_digest)
            || (self.issued_count == 0 && self.issued_identity_digest != identity_seed(parent))
            || (self.closed_count == 0
                && (self.closed_identity_digest != identity_seed(parent)
                    || self.close_seal_digest != close_seed(parent)))
        {
            return Err(CatalogError::Corrupt(
                "回收layout2 issued/closed派生检查点损坏",
            ));
        }
        Ok(())
    }
    pub(super) fn idle(&self) -> bool {
        self.issued_count == self.closed_count
            && self.issued_identity_digest == self.closed_identity_digest
    }
}

/// Only the original new-parent reservation transaction may create L/z.
/// Existing private prototypes are forensic inputs, never auto-migrated.
pub(super) async fn initialize_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<()> {
    if parent.intent()["version"] != 2 {
        return Err(CatalogError::Invalid("layout3仅用于新v2回收父任务"));
    }
    let rows = read_page(tr, &prefix(parent), None, 1, false).await?;
    if !rows.items.is_empty() {
        return Err(CatalogError::Invalid("已有私有回收布局不支持自动迁移"));
    }
    tr.set(
        &control_key(parent, b'L', None),
        &encode(&Marker {
            version: 3,
            parent: parent.fingerprint(),
        })?,
    );
    tr.set(
        &control_key(parent, b'z', None),
        &encode(&Progress::new(parent))?,
    );
    Ok(())
}
pub(super) async fn require_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<Progress> {
    let raw = tr
        .get(&control_key(parent, b'L', None), false)
        .await?
        .ok_or(CatalogError::Invalid(
            "旧私有v2回收原型缺layout3 marker；禁止自动恢复或降级",
        ))?;
    let marker: Marker = decode(&raw)?;
    if marker.version != 3 || marker.parent != parent.fingerprint() {
        return Err(CatalogError::Corrupt("回收私有布局版本或原父身份不同"));
    }
    let raw = tr
        .get(&control_key(parent, b'z', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let progress: Progress = decode(&raw)?;
    progress.validate(parent)?;
    // These were canonical top-level kinds only in the old private layout.
    for kind in *b"hmidnp" {
        if requests::prefix_has_rows(tr, &control_key(parent, kind, None)).await? {
            return Err(CatalogError::Corrupt("新旧私有回收key布局混合"));
        }
    }
    Ok(progress)
}
pub(super) async fn writable_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<Progress> {
    let progress = require_at(tr, parent).await?;
    if requests::prefix_has_rows(tr, &control_key(parent, b'A', None)).await? {
        return Err(CatalogError::Invalid(
            "原回收历史已进入format audit，禁止新组或attempt",
        ));
    }
    Ok(progress)
}
pub(super) async fn next_sequence_at(tr: &Transaction, parent: &ReclaimReservation) -> Result<u64> {
    let progress = writable_at(tr, parent).await?;
    if !progress.idle() {
        return Err(CatalogError::PoolBusy);
    }
    progress
        .issued_count
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("回收issued序号溢出"))
}
pub(super) async fn issue_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    let mut progress = writable_at(tr, parent).await?;
    if !progress.idle()
        || group.header.sequence
            != progress
                .issued_count
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || tr
            .get(&control_key(parent, b'a', None), false)
            .await?
            .is_some()
        || requests::prefix_has_rows(tr, &control_key(parent, b'u', None)).await?
    {
        return Err(CatalogError::Corrupt("新回收组issued/a/u原子前置条件不同"));
    }
    let issued = Issued {
        version: 2,
        parent: parent.fingerprint(),
        sequence: group.header.sequence,
        group: group.header.group.clone(),
    };
    let key = issued_key(parent, issued.sequence);
    if tr.get(&key, false).await?.is_some() {
        return Err(CatalogError::Corrupt("回收issued序号已有原组"));
    }
    progress.issued_count = issued.sequence;
    progress.issued_identity_digest = identity_step(progress.issued_identity_digest, &issued)?;
    tr.set(&key, &encode(&issued)?);
    tr.set(&control_key(parent, b'a', None), group.id().as_bytes());
    tr.set(
        &control_key(parent, b'u', Some(group.id())),
        &issued.sequence.to_be_bytes(),
    );
    tr.set(&control_key(parent, b'z', None), &encode(&progress)?);
    Ok(())
}
pub(super) async fn require_identity_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
) -> Result<()> {
    let progress = require_at(tr, parent).await?;
    let raw = tr
        .get(&issued_key(parent, group.header.sequence), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let issued: Issued = decode(&raw)?;
    if issued.version != 2
        || issued.parent != parent.fingerprint()
        || issued.sequence != group.header.sequence
        || issued.group != group.header.group
        || issued.sequence > progress.issued_count
    {
        return Err(CatalogError::Corrupt("回收canonical组与原issued身份不同"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Synthetic identity only: these pure tests mint no owner/admission or I/O
    // authority and do not claim a reachable business fixture.
    fn parent() -> ReclaimReservation {
        let namespace = Uuid::new_v4();
        ReclaimReservation {
            intent: json!({"version":2,"library":"SIM-LAYOUT-PURE","namespace":namespace.to_string(),
            "epoch":1,"operation":Uuid::new_v4().to_string()}),
            route: ActiveLibraryRoute {
                route: crate::managed_catalog::routing::PreparedLibraryRoute {
                    namespace,
                    revision: 1,
                },
                frozen_index: 1,
                certificate_sha256: [1; 32],
                initial_owner_epoch: 1,
            },
        }
    }
    #[test]
    fn canonical_history_and_attempts_have_distinct_key_domains() {
        let parent = parent();
        let group = Uuid::new_v4();
        let other = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let first = canonical_key(&parent, group, b'h', None);
        assert!(first.starts_with(&canonical_group_prefix(&parent, group)));
        assert!(!first.starts_with(&canonical_group_prefix(&parent, other)));
        assert_ne!(first, attempt_key(&parent, group, attempt, b'h', None));
        assert_ne!(
            attempt_key(&parent, group, attempt, b'd', Some(0)),
            attempt_key(&parent, group, other, b'd', Some(0))
        );
        assert_ne!(
            canonical_key(&parent, group, b'm', Some(0)),
            canonical_key(&parent, group, b'm', Some(1))
        );
        assert_ne!(control_key(&parent, b'r', Some(group)), first);
    }
    #[test]
    fn issued_progress_remains_pending_without_control_pointer_facts() {
        let parent = parent();
        let mut progress = Progress::new(&parent);
        assert!(progress.idle());
        progress.validate(&parent).unwrap();
        let issued = Issued {
            version: 2,
            parent: parent.fingerprint(),
            sequence: 1,
            group: Uuid::new_v4().to_string(),
        };
        progress.issued_count = 1;
        progress.issued_identity_digest =
            identity_step(progress.issued_identity_digest, &issued).unwrap();
        progress.validate(&parent).unwrap();
        assert!(!progress.idle());
        let mut damaged = progress.clone();
        damaged.closed_count = 1;
        assert!(
            damaged.validate(&parent).is_err(),
            "equal count cannot discard original identity chain"
        );
        damaged = progress.clone();
        damaged.issued_count = 2;
        assert!(
            damaged.validate(&parent).is_err(),
            "only one logical group may be outstanding"
        );
        let mut changed = issued.clone();
        changed.group = Uuid::new_v4().to_string();
        assert_ne!(
            identity_step(identity_seed(&parent), &issued).unwrap(),
            identity_step(identity_seed(&parent), &changed).unwrap()
        );
    }
}

pub(super) async fn previous_close_digest_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
) -> Result<[u8; 32]> {
    Ok(writable_at(tr, parent).await?.close_seal_digest)
}
fn close_step(previous: [u8; 32], seal: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-group-close-row/2")
        .chain_update(previous)
        .chain_update((seal.len() as u64).to_be_bytes())
        .chain_update(seal)
        .finalize()
        .into()
}
pub(super) async fn record_close_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    seal: &[u8],
    previous: [u8; 32],
) -> Result<()> {
    let mut progress = writable_at(tr, parent).await?;
    require_identity_at(tr, parent, group).await?;
    if group.header.phase != Phase::Closed
        || group.header.sequence != progress.issued_count
        || group.header.sequence
            != progress
                .closed_count
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || previous != progress.close_seal_digest
        || tr
            .get(&control_key(parent, b'a', None), false)
            .await?
            .as_deref()
            != Some(group.id().as_bytes())
        || tr
            .get(&control_key(parent, b'u', Some(group.id())), false)
            .await?
            .as_deref()
            != Some(group.header.sequence.to_be_bytes().as_slice())
    {
        return Err(CatalogError::Corrupt("回收关闭事务与原issued/a/u不一致"));
    }
    let close_key = canonical_key(parent, group.id(), b'c', None);
    if tr.get(&close_key, false).await?.is_some() {
        return Err(CatalogError::Corrupt("未关闭原组已有半套close seal"));
    }
    let raw = tr
        .get(&issued_key(parent, group.header.sequence), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let issued: Issued = decode(&raw)?;
    progress.closed_count = issued.sequence;
    progress.closed_identity_digest = identity_step(progress.closed_identity_digest, &issued)?;
    progress.close_seal_digest = close_step(previous, seal);
    progress.validate(parent)?;
    tr.set(
        &canonical_key(parent, group.id(), b'h', None),
        &encode(&group.header)?,
    );
    tr.set(&close_key, seal);
    tr.set(&control_key(parent, b'z', None), &encode(&progress)?);
    tr.clear(&control_key(parent, b'a', None));
    tr.clear(&control_key(parent, b'u', Some(group.id())));
    Ok(())
}
pub(super) async fn require_closed_sequence_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    group: &ReclaimCopyGroup,
    seal: &[u8],
    previous: [u8; 32],
) -> Result<()> {
    let progress = require_at(tr, parent).await?;
    if group.header.phase != Phase::Closed || group.header.sequence > progress.closed_count {
        return Err(CatalogError::Corrupt("原closed组不属于原已关闭序号前缀"));
    }
    if group.header.sequence == progress.closed_count
        && close_step(previous, seal) != progress.close_seal_digest
    {
        return Err(CatalogError::Corrupt("原末次close seal与原z不一致"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditSeal {
    version: u8,
    parent: String,
    layout: [u8; 32],
    ledger: [u8; 32],
    coverage: [u8; 32],
    issued_count: u64,
    identity: [u8; 32],
    close_seals: [u8; 32],
    canonical_rows: u64,
    canonical_digest: [u8; 32],
}
/// The audit producer is paginated and must freeze its original L/z before
/// enumerating history. There is deliberately no caller-supplied success flag.
pub(super) async fn require_format_audit_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    coverage: [u8; 32],
) -> Result<[u8; 32]> {
    let progress = require_at(tr, parent).await?;
    if !progress.idle() {
        return Err(CatalogError::Incomplete);
    }
    let layout = tr
        .get(&control_key(parent, b'L', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let ledger = tr
        .get(&control_key(parent, b'z', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let raw = tr
        .get(&control_key(parent, b'F', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let seal: AuditSeal = decode(&raw)?;
    if seal.version != 2
        || seal.parent != parent.fingerprint()
        || seal.coverage != coverage
        || seal.layout != <[u8; 32]>::from(Sha256::digest(&layout))
        || seal.ledger != <[u8; 32]>::from(Sha256::digest(&ledger))
        || seal.issued_count != progress.issued_count
        || seal.identity != progress.issued_identity_digest
        || seal.close_seals != progress.close_seal_digest
    {
        return Err(CatalogError::Corrupt(
            "源format缺少绑定原L/z/Ry的完整历史audit seal",
        ));
    }
    audit::require_final_at(tr, parent, &seal).await?;
    digest(
        b"tape-rs/reclaim-format-audited-coverage/2",
        &(coverage, seal),
    )
}

pub(super) async fn step_audit_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    coverage: [u8; 32],
) -> Result<bool> {
    audit::step_at(tr, parent, owner, coverage).await
}
