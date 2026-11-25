//! Bounded, resumable full-domain validation before source formatting.
//! A freezes normal canonical mutation; every page rechecks the same L/z/Ry.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Basis {
    version: u8,
    parent: String,
    layout: [u8; 32],
    ledger: [u8; 32],
    coverage: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Issued,
    Rows,
    Complete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    basis: Basis,
    stage: Stage,
    next_sequence: u64,
    identity: [u8; 32],
    close_seals: [u8; 32],
    cursor: Option<Vec<u8>>,
    canonical_rows: u64,
    canonical_digest: [u8; 32],
}
fn row_seed(parent: &ReclaimReservation) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-canonical-audit/2")
        .chain_update(parent.fingerprint().as_bytes())
        .finalize()
        .into()
}
fn row_step(previous: [u8; 32], ordinal: u64, key: &[u8], value: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/reclaim-canonical-audit-row/2")
        .chain_update(previous)
        .chain_update(ordinal.to_be_bytes())
        .chain_update((key.len() as u64).to_be_bytes())
        .chain_update(key)
        .chain_update((value.len() as u64).to_be_bytes())
        .chain_update(value)
        .finalize()
        .into()
}
async fn basis_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    coverage: [u8; 32],
) -> Result<Basis> {
    let layout = tr
        .get(&control_key(parent, b'L', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let ledger = tr
        .get(&control_key(parent, b'z', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    Ok(Basis {
        version: 2,
        parent: parent.fingerprint(),
        layout: Sha256::digest(&layout).into(),
        ledger: Sha256::digest(&ledger).into(),
        coverage,
    })
}
fn seal(state: &State, progress: &Progress) -> AuditSeal {
    AuditSeal {
        version: 2,
        parent: state.basis.parent.clone(),
        layout: state.basis.layout,
        ledger: state.basis.ledger,
        coverage: state.basis.coverage,
        issued_count: progress.issued_count,
        identity: state.identity,
        close_seals: state.close_seals,
        canonical_rows: state.canonical_rows,
        canonical_digest: state.canonical_digest,
    }
}
async fn validate_basis_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    coverage: [u8; 32],
    state: &State,
) -> Result<Progress> {
    let progress = require_at(tr, parent).await?;
    super::super::require_no_unsettled_at(tr, parent).await?;
    if state.version != 2
        || state.basis != basis_at(tr, parent, coverage).await?
        || state.next_sequence == 0
        || state.next_sequence
            > progress
                .issued_count
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || tr
            .get(&control_key(parent, b'A', None), false)
            .await?
            .as_deref()
            != Some(encode(&state.basis)?.as_slice())
    {
        return Err(CatalogError::Corrupt(
            "原audit分页基线L/z/Ry或cursor身份改变",
        ));
    }
    Ok(progress)
}
pub(super) async fn step_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    owner: u64,
    coverage: [u8; 32],
) -> Result<bool> {
    super::super::require_no_unsettled_at(tr, parent).await?;
    let state_key = control_key(parent, b'P', None);
    let Some(raw) = tr.get(&state_key, false).await? else {
        if tr
            .get(&control_key(parent, b'A', None), false)
            .await?
            .is_some()
            || tr
                .get(&control_key(parent, b'F', None), false)
                .await?
                .is_some()
        {
            return Err(CatalogError::Corrupt("原audit出现半套marker/seal"));
        }
        let basis = basis_at(tr, parent, coverage).await?;
        let state = State {
            version: 2,
            basis: basis.clone(),
            stage: Stage::Issued,
            next_sequence: 1,
            identity: identity_seed(parent),
            close_seals: close_seed(parent),
            cursor: None,
            canonical_rows: 0,
            canonical_digest: row_seed(parent),
        };
        tr.set(&control_key(parent, b'A', None), &encode(&basis)?);
        tr.set(&state_key, &encode(&state)?);
        return Ok(false);
    };
    let mut state: State = decode(&raw)?;
    let progress = validate_basis_at(tr, parent, coverage, &state).await?;
    match state.stage {
        Stage::Issued => {
            if state.next_sequence <= progress.issued_count {
                let raw = tr
                    .get(&issued_key(parent, state.next_sequence), false)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
                let issued: Issued = decode(&raw)?;
                if issued.version != 2
                    || issued.parent != parent.fingerprint()
                    || issued.sequence != state.next_sequence
                {
                    return Err(CatalogError::Corrupt("audit issued序号缺口或身份不同"));
                }
                let (sequence, close, previous) = super::super::index_augmentation::audit_group_at(
                    tr,
                    parent,
                    owner,
                    parse_uuid(&issued.group)?,
                )
                .await?;
                if sequence != state.next_sequence || previous != state.close_seals {
                    return Err(CatalogError::Corrupt("audit close chain顺序不同"));
                }
                state.identity = identity_step(state.identity, &issued)?;
                state.close_seals = close_step(state.close_seals, &close);
                state.next_sequence = state
                    .next_sequence
                    .checked_add(1)
                    .ok_or(CatalogError::Incomplete)?;
            } else {
                if state.identity != progress.issued_identity_digest
                    || state.close_seals != progress.close_seal_digest
                {
                    return Err(CatalogError::Corrupt(
                        "audit完整issued/close chain与原z不同",
                    ));
                }
                state.stage = Stage::Rows;
            }
        }
        Stage::Rows => {
            let root = prefix(parent);
            let rows = read_page(tr, &root, state.cursor.as_deref(), 8, false).await?;
            for (key, value) in &rows.items {
                let tail = key
                    .strip_prefix(root.as_slice())
                    .ok_or(CatalogError::Incomplete)?;
                match tail {
                    [b'L'] => {
                        if <[u8; 32]>::from(Sha256::digest(value)) != state.basis.layout {
                            return Err(CatalogError::Corrupt("audit L改变"));
                        }
                    }
                    [b'z'] => {
                        if <[u8; 32]>::from(Sha256::digest(value)) != state.basis.ledger {
                            return Err(CatalogError::Corrupt("audit z改变"));
                        }
                    }
                    [b'A'] => {
                        if decode::<Basis>(value)? != state.basis {
                            return Err(CatalogError::Corrupt("audit marker改变"));
                        }
                        continue;
                    }
                    [b'P'] => {
                        if value.as_slice() != raw.as_ref() {
                            return Err(CatalogError::Corrupt("audit进度自身改变"));
                        }
                        continue;
                    }
                    [b'q', rest @ ..] => {
                        if rest.len() != 8 {
                            return Err(CatalogError::Corrupt("audit issued key长度"));
                        }
                        let sequence = u64::from_be_bytes(
                            rest.try_into().map_err(|_| CatalogError::Incomplete)?,
                        );
                        let row: Issued = decode(value)?;
                        if sequence == 0
                            || sequence > progress.issued_count
                            || row.version != 2
                            || row.parent != parent.fingerprint()
                            || row.sequence != sequence
                        {
                            return Err(CatalogError::Corrupt("audit多余issued key或身份"));
                        }
                        let header: Header = decode(
                            &tr.get(
                                &canonical_key(parent, parse_uuid(&row.group)?, b'h', None),
                                false,
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?,
                        )?;
                        if header.group != row.group
                            || header.sequence != sequence
                            || header.phase != Phase::Closed
                        {
                            return Err(CatalogError::Corrupt("audit issued route无原closed组"));
                        }
                    }
                    [b'g' | b'r' | b'b', ..] => {
                        super::super::index_augmentation::audit_row_at(tr, parent, tail, value)
                            .await?;
                    }
                    _ => return Err(CatalogError::Corrupt("audit存在pending、旧layout或未知key")),
                }
                state.canonical_rows = state
                    .canonical_rows
                    .checked_add(1)
                    .ok_or(CatalogError::Incomplete)?;
                state.canonical_digest =
                    row_step(state.canonical_digest, state.canonical_rows, key, value);
            }
            state.cursor = rows.next;
            if state.cursor.is_none() {
                if state.identity != progress.issued_identity_digest
                    || state.close_seals != progress.close_seal_digest
                {
                    return Err(CatalogError::Corrupt("audit终点完整链不同"));
                }
                state.stage = Stage::Complete;
                tr.set(
                    &control_key(parent, b'F', None),
                    &encode(&seal(&state, &progress))?,
                );
            }
        }
        Stage::Complete => {
            let stored = tr
                .get(&control_key(parent, b'F', None), false)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            if decode::<AuditSeal>(&stored)? != seal(&state, &progress) {
                return Err(CatalogError::Corrupt("原audit完成seal不一致"));
            }
            return Ok(true);
        }
    }
    let complete = state.stage == Stage::Complete;
    tr.set(&state_key, &encode(&state)?);
    Ok(complete)
}
pub(super) async fn require_final_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    expected: &AuditSeal,
) -> Result<()> {
    let raw = tr
        .get(&control_key(parent, b'P', None), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let state: State = decode(&raw)?;
    let progress = validate_basis_at(tr, parent, expected.coverage, &state).await?;
    if state.stage != Stage::Complete
        || state.cursor.is_some()
        || state.next_sequence
            != progress
                .issued_count
                .checked_add(1)
                .ok_or(CatalogError::Incomplete)?
        || seal(&state, &progress) != *expected
    {
        return Err(CatalogError::Corrupt("原audit尚未完整消费canonical历史"));
    }
    Ok(())
}
