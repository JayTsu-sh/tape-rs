//! Bounded request admission and resumable metadata housekeeping. The driver
//! advances a durable logical clock from acknowledged monotonic elapsed time;
//! it never uses a node's wall clock to expire results or cancel media work.

use super::*;
use std::time::{Duration, Instant};

const SHARDS: u16 = 64;
const PROBES: usize = 8;
const BATCH: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenancePolicy {
    pub epoch_ms: u64,
    pub result_retention_ms: u64,
    pub max_pending: u32,
    pub max_requests_per_epoch: u32,
    pub max_retained_epochs: u16,
}

impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            epoch_ms: 3_600_000,
            result_retention_ms: 86_400_000,
            max_pending: 4096,
            max_requests_per_epoch: 1_000_000,
            max_retained_epochs: 128,
        }
    }
}

impl MaintenancePolicy {
    fn validate(self) -> Result<Self> {
        if self.epoch_ms == 0
            || self.result_retention_ms == 0
            || self.max_pending == 0
            || self.max_requests_per_epoch < self.max_pending
            || !(2..=4096).contains(&self.max_retained_epochs)
            || self
                .epoch_ms
                .checked_add(self.result_retention_ms)
                .is_none()
        {
            return Err(CatalogError::Invalid(
                "维护策略的时长、准入预算或代次上限无效",
            ));
        }
        Ok(self)
    }
    fn shards(self) -> u16 {
        self.max_pending.min(u32::from(SHARDS)) as u16
    }
    fn share(self, total: u32, shard: u16) -> u64 {
        u64::from(
            total / u32::from(self.shards())
                + u32::from(u32::from(shard) < total % u32::from(self.shards())),
        )
    }
}

fn ns_key(kind: u8, ns: Uuid) -> Vec<u8> {
    let mut out = key(kind);
    out.extend_from_slice(ns.as_bytes());
    out
}
fn epoch_key(kind: u8, ns: Uuid, epoch: u64) -> Vec<u8> {
    let mut out = ns_key(kind, ns);
    out.extend_from_slice(&epoch.to_be_bytes());
    out
}
fn quota_key(ns: Uuid, epoch: u64, shard: u16) -> Vec<u8> {
    let mut out = epoch_key(b'B', ns, epoch);
    out.extend_from_slice(&shard.to_be_bytes());
    out
}
fn encode_policy(p: MaintenancePolicy) -> Vec<u8> {
    [
        p.epoch_ms,
        p.result_retention_ms,
        u64::from(p.max_pending),
        u64::from(p.max_requests_per_epoch),
        u64::from(p.max_retained_epochs),
    ]
    .into_iter()
    .flat_map(u64::to_be_bytes)
    .collect()
}
fn decode_policy(bytes: &[u8]) -> Result<MaintenancePolicy> {
    let invalid = || CatalogError::Corrupt("维护策略记录");
    if bytes.len() != 40 {
        return Err(invalid());
    }
    MaintenancePolicy {
        epoch_ms: read_u64(bytes),
        result_retention_ms: read_u64(&bytes[8..]),
        max_pending: read_u64(&bytes[16..]).try_into().map_err(|_| invalid())?,
        max_requests_per_epoch: read_u64(&bytes[24..]).try_into().map_err(|_| invalid())?,
        max_retained_epochs: read_u64(&bytes[32..]).try_into().map_err(|_| invalid())?,
    }
    .validate()
    .map_err(|_| invalid())
}
async fn policy(tr: &Transaction, ns: Uuid) -> Result<MaintenancePolicy> {
    let bytes = tr.get(&ns_key(b'A', ns), false).await?;
    decode_policy(
        bytes
            .as_deref()
            .ok_or(CatalogError::Corrupt("维护策略缺失"))?,
    )
}

#[derive(Clone, Copy)]
struct Epoch {
    opened: u64,
    sealed: Option<u64>,
    drained: Option<u64>,
    retention: u64,
}
fn encode_epoch(e: Epoch) -> Vec<u8> {
    [
        e.opened,
        e.sealed.unwrap_or(u64::MAX),
        e.drained.unwrap_or(u64::MAX),
        e.retention,
    ]
    .into_iter()
    .flat_map(u64::to_be_bytes)
    .collect()
}
fn decode_epoch(bytes: &[u8]) -> Result<Epoch> {
    if bytes.len() != 32 {
        return Err(CatalogError::Corrupt("维护代次长度"));
    }
    let e = Epoch {
        opened: read_u64(bytes),
        sealed: (read_u64(&bytes[8..]) != u64::MAX).then(|| read_u64(&bytes[8..])),
        drained: (read_u64(&bytes[16..]) != u64::MAX).then(|| read_u64(&bytes[16..])),
        retention: read_u64(&bytes[24..]),
    };
    if e.opened == u64::MAX
        || e.retention == 0
        || e.sealed.is_some_and(|v| v < e.opened || v == u64::MAX)
        || e.drained
            .is_some_and(|v| e.sealed.is_none_or(|sealed| v < sealed) || v == u64::MAX)
    {
        return Err(CatalogError::Corrupt("维护代次顺序"));
    }
    Ok(e)
}
async fn epoch(tr: &Transaction, ns: Uuid, number: u64) -> Result<Epoch> {
    let bytes = tr.get(&epoch_key(b'D', ns, number), false).await?;
    decode_epoch(
        bytes
            .as_deref()
            .ok_or(CatalogError::Corrupt("维护代次缺失"))?,
    )
}

async fn count(tr: &Transaction, key: &[u8], snapshot: bool, maximum: u64) -> Result<u64> {
    let raw = tr.get(key, snapshot).await?;
    decode_count(raw.as_deref(), maximum)
}

fn decode_count(raw: Option<&[u8]>, maximum: u64) -> Result<u64> {
    let n = match raw {
        None => 0,
        Some(raw) if raw.len() == 8 => read_u64(raw),
        _ => return Err(CatalogError::Corrupt("准入配额计数长度")),
    };
    if n > maximum {
        return Err(CatalogError::Corrupt("准入配额计数越界"));
    }
    Ok(n)
}

async fn totals(
    tr: &Transaction,
    ns: Uuid,
    number: u64,
    p: MaintenancePolicy,
    with_pending: bool,
) -> Result<(u64, u64)> {
    // Submit independent reads before awaiting; avoid 64 network round trips
    // when the shared catalog is remote from its maintenance leader.
    let reads: Vec<_> = (0..p.shards())
        .map(|shard| {
            (
                shard,
                with_pending.then(|| tr.get(&quota_key(ns, 0, shard), true)),
                tr.get(&quota_key(ns, number, shard), true),
            )
        })
        .collect();
    let mut pending = 0;
    let mut admitted = 0;
    for (shard, live, total) in reads {
        if let Some(live) = live {
            pending += decode_count(live.await?.as_deref(), p.share(p.max_pending, shard))?;
        }
        admitted += decode_count(
            total.await?.as_deref(),
            p.share(p.max_requests_per_epoch, shard),
        )?;
    }
    Ok((pending, admitted))
}

pub(super) async fn reserve(tr: &Transaction, r: &CommitReceipt) -> Result<Option<u16>> {
    reserve_identity(tr, r.namespace, r.request_epoch, r.operation_id).await
}

pub(super) async fn reserve_identity(
    tr: &Transaction,
    namespace: Uuid,
    request_epoch: u64,
    operation_id: Uuid,
) -> Result<Option<u16>> {
    let p = policy(tr, namespace).await?;
    if epoch(tr, namespace, request_epoch).await?.sealed.is_some() {
        return Ok(None);
    }
    let start = (read_u64(operation_id.as_bytes()) % u64::from(p.shards())) as u16;
    // Snapshot probes avoid adding conflicts on every counter examined. Only
    // the selected shard is read again non-snapshot before its atomic reserve.
    for offset in 0..PROBES.min(usize::from(p.shards())) {
        let shard = (start + offset as u16) % p.shards();
        let pending = quota_key(namespace, 0, shard);
        let total = quota_key(namespace, request_epoch, shard);
        let pending_cap = p.share(p.max_pending, shard);
        let total_cap = p.share(p.max_requests_per_epoch, shard);
        if count(tr, &pending, true, pending_cap).await? >= pending_cap
            || count(tr, &total, true, total_cap).await? >= total_cap
        {
            continue;
        }
        let live = count(tr, &pending, false, pending_cap).await?;
        let used = count(tr, &total, false, total_cap).await?;
        if live >= pending_cap || used >= total_cap {
            continue;
        }
        tr.set(&pending, &(live + 1).to_be_bytes());
        tr.set(&total, &(used + 1).to_be_bytes());
        return Ok(Some(shard));
    }
    Ok(None)
}

pub(super) struct PermitRelease {
    key: Vec<u8>,
    remaining: u64,
}
impl PermitRelease {
    pub(super) fn apply(self, tr: &Transaction) {
        if self.remaining == 0 {
            tr.clear(&self.key);
        } else {
            tr.set(&self.key, &self.remaining.to_be_bytes());
        }
    }
}
pub(super) async fn release(tr: &Transaction, ns: Uuid, shard: u16) -> Result<PermitRelease> {
    release_many(tr, ns, shard, 1).await
}

pub(super) async fn release_many(
    tr: &Transaction,
    ns: Uuid,
    shard: u16,
    released: u64,
) -> Result<PermitRelease> {
    let p = policy(tr, ns).await?;
    if shard >= p.shards() || released == 0 {
        return Err(CatalogError::Corrupt("准入配额分片越界"));
    }
    let key = quota_key(ns, 0, shard);
    let live = count(tr, &key, false, p.share(p.max_pending, shard)).await?;
    if live < released {
        return Err(CatalogError::Corrupt("准入配额已被释放或丢失"));
    }
    Ok(PermitRelease {
        key,
        remaining: live - released,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MaintenanceAction {
    Idle = 0,
    Rotated = 1,
    Drained = 2,
    Expired = 3,
    WaitingRequests = 4,
    WaitingReferences = 5,
    Retaining = 6,
    AdmissionPaused = 7,
    PurgedResults = 8,
    History = 9,
    PurgedControl = 10,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub logical_ms: u64,
    pub window: RequestWindow,
    pub action: MaintenanceAction,
    pub examined: u64,
    pub compacted: u64,
    pub forgotten: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceStatus {
    pub report: MaintenanceReport,
    pub policy: MaintenancePolicy,
    pub accepting_requests: bool,
    pub pending: u64,
    pub admitted_current_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryReference {
    pub request_epoch: u64,
    pub reference_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingRequestId {
    pub request_epoch: u64,
    pub operation_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceBlocker {
    PendingRequest(PendingRequestId),
    HistoryReference(HistoryReference),
    AwaitingDrain(u64),
    Retaining {
        request_epoch: u64,
        remaining_ms: u64,
    },
    ReadyToExpire(u64),
    ExpiredResults(u64),
    ExpiredCounters(u64),
    ExpiredDescriptor(u64),
    AdmissionPaused(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceDiagnosis {
    pub status: MaintenanceStatus,
    pub oldest_budget_epoch: u64,
    pub blocker: Option<MaintenanceBlocker>,
}
#[derive(Clone)]
struct State {
    clock: u64,
    owner: Uuid,
    sequence: u64,
    phase: u8,
    cursor: Option<Vec<u8>>,
    drain_after: u64,
    report: MaintenanceReport,
}
fn initial_state() -> State {
    State {
        clock: 0,
        owner: Uuid::nil(),
        sequence: 0,
        phase: 0,
        cursor: None,
        drain_after: 0,
        report: MaintenanceReport {
            logical_ms: 0,
            window: RequestWindow {
                first_retained: 1,
                accepting: 1,
            },
            action: MaintenanceAction::Idle,
            examined: 0,
            compacted: 0,
            forgotten: 0,
        },
    }
}
fn encode_state(s: &State) -> Vec<u8> {
    let mut out = s.clock.to_be_bytes().to_vec();
    out.extend_from_slice(s.owner.as_bytes());
    out.extend_from_slice(&s.sequence.to_be_bytes());
    out.push(s.phase);
    let cursor = s.cursor.as_deref().unwrap_or_default();
    out.extend_from_slice(&(cursor.len() as u16).to_be_bytes());
    out.extend_from_slice(cursor);
    out.push(s.report.action as u8);
    out.extend_from_slice(&requests::encode_window(s.report.window));
    for n in [s.report.examined, s.report.compacted, s.report.forgotten] {
        out.extend_from_slice(&n.to_be_bytes());
    }
    out.extend_from_slice(&s.drain_after.to_be_bytes());
    out
}
fn decode_state(ns: Uuid, bytes: &[u8]) -> Result<State> {
    let bad = || CatalogError::Corrupt("维护进度记录");
    if bytes.len() < 84 || bytes[32] > 4 {
        return Err(bad());
    }
    let length = u16::from_be_bytes([bytes[33], bytes[34]]) as usize;
    if bytes.len() != 84 + length {
        return Err(bad());
    }
    let cursor = if length == 0 {
        None
    } else {
        let cursor = bytes[35..35 + length].to_vec();
        if cursor.len() != copy_prefix(ns).len() + 32 || !cursor.starts_with(&copy_prefix(ns)) {
            return Err(bad());
        }
        Some(cursor)
    };
    let base = 35 + length;
    let action = match bytes[base] {
        0 => MaintenanceAction::Idle,
        1 => MaintenanceAction::Rotated,
        2 => MaintenanceAction::Drained,
        3 => MaintenanceAction::Expired,
        4 => MaintenanceAction::WaitingRequests,
        5 => MaintenanceAction::WaitingReferences,
        6 => MaintenanceAction::Retaining,
        7 => MaintenanceAction::AdmissionPaused,
        8 => MaintenanceAction::PurgedResults,
        9 => MaintenanceAction::History,
        10 => MaintenanceAction::PurgedControl,
        _ => return Err(bad()),
    };
    let clock = read_u64(bytes);
    if clock == u64::MAX {
        return Err(bad());
    }
    Ok(State {
        clock,
        owner: uuid_from(&bytes[8..24])?,
        sequence: read_u64(&bytes[24..32]),
        phase: bytes[32],
        cursor,
        drain_after: read_u64(&bytes[base + 41..]),
        report: MaintenanceReport {
            logical_ms: clock,
            window: requests::decode_window(&bytes[base + 1..base + 17])?,
            action,
            examined: read_u64(&bytes[base + 17..]),
            compacted: read_u64(&bytes[base + 25..]),
            forgotten: read_u64(&bytes[base + 33..]),
        },
    })
}
async fn state(tr: &Transaction, ns: Uuid) -> Result<State> {
    let bytes = tr.get(&ns_key(b'K', ns), false).await?;
    decode_state(
        ns,
        bytes
            .as_deref()
            .ok_or(CatalogError::Corrupt("维护进度缺失"))?,
    )
}

pub(super) fn initialize(tr: &Transaction, ns: Uuid) {
    let p = MaintenancePolicy::default();
    tr.set(&ns_key(b'A', ns), &encode_policy(p));
    tr.set(&ns_key(b'K', ns), &encode_state(&initial_state()));
    tr.set(
        &epoch_key(b'D', ns, 1),
        &encode_epoch(Epoch {
            opened: 0,
            sealed: None,
            drained: None,
            retention: p.result_retention_ms,
        }),
    );
}

pub(super) async fn rotate(tr: &Transaction, ns: Uuid, expected: u64) -> Result<WindowOutcome> {
    let w = requests::window(tr, ns).await?;
    if w.accepting != expected {
        return Ok(WindowOutcome::Conflict(w));
    }
    let p = policy(tr, ns).await?;
    if !has_epoch_capacity(tr, ns, w, p).await? {
        return Ok(WindowOutcome::RetentionLimited);
    }
    let clock = state(tr, ns).await?.clock;
    let mut old = epoch(tr, ns, w.accepting).await?;
    let next = w
        .accepting
        .checked_add(1)
        .ok_or(CatalogError::Invalid("请求代次溢出"))?;
    old.sealed.get_or_insert(clock);
    tr.set(&epoch_key(b'D', ns, w.accepting), &encode_epoch(old));
    tr.set(
        &epoch_key(b'D', ns, next),
        &encode_epoch(Epoch {
            opened: clock,
            sealed: None,
            drained: None,
            retention: p.result_retention_ms,
        }),
    );
    let w = RequestWindow {
        accepting: next,
        ..w
    };
    tr.set(&requests::window_key(ns), &requests::encode_window(w));
    Ok(WindowOutcome::Advanced(w))
}

// Include expired but uncollected epochs: advancing the retry floor must not
// let a stalled result collector bypass the storage budget.
async fn has_epoch_capacity(
    tr: &Transaction,
    ns: Uuid,
    w: RequestWindow,
    p: MaintenancePolicy,
) -> Result<bool> {
    let prefix = ns_key(b'D', ns);
    let rows = read_page(tr, &prefix, None, 1, false).await?;
    let first = rows
        .items
        .first()
        .ok_or(CatalogError::Corrupt("维护代次缺失"))?;
    if first.0.len() != prefix.len() + 8 {
        return Err(CatalogError::Corrupt("维护代次键长度"));
    }
    let oldest = read_u64(&first.0[prefix.len()..]);
    if oldest == 0 || oldest > w.first_retained {
        return Err(CatalogError::Corrupt("维护代次窗口不完整"));
    }
    Ok(w.accepting - oldest + 1 < u64::from(p.max_retained_epochs))
}

async fn lifecycle(
    tr: &Transaction,
    ns: Uuid,
    now: u64,
    p: MaintenancePolicy,
) -> Result<MaintenanceAction> {
    let mut w = requests::window(tr, ns).await?;
    let mut waiting = MaintenanceAction::Idle;
    if w.first_retained < w.accepting {
        let mut oldest = epoch(tr, ns, w.first_retained).await?;
        if oldest.sealed.is_none_or(|sealed| sealed > now)
            || oldest.drained.is_some_and(|drained| drained > now)
        {
            return Err(CatalogError::Corrupt("旧准入代次时间异常"));
        }
        if requests::prefix_has_rows(tr, &epoch_key(b'I', ns, w.first_retained)).await?
            || requests::prefix_has_rows(tr, &epoch_key(b'0', ns, w.first_retained)).await?
        {
            waiting = MaintenanceAction::WaitingRequests;
        } else if oldest.drained.is_none() {
            now.checked_add(oldest.retention)
                .ok_or(CatalogError::Invalid("保留期限溢出"))?;
            oldest.drained = Some(now);
            tr.set(
                &epoch_key(b'D', ns, w.first_retained),
                &encode_epoch(oldest),
            );
            return Ok(MaintenanceAction::Drained);
        } else if requests::prefix_has_rows(tr, &epoch_key(b'F', ns, w.first_retained)).await? {
            waiting = MaintenanceAction::WaitingReferences;
        } else if now
            >= oldest
                .drained
                .expect("checked")
                .checked_add(oldest.retention)
                .ok_or(CatalogError::Corrupt("保留期限溢出"))?
        {
            w.first_retained += 1;
            tr.set(&requests::window_key(ns), &requests::encode_window(w));
            return Ok(MaintenanceAction::Expired);
        } else {
            waiting = MaintenanceAction::Retaining;
        }
    }
    let mut current = epoch(tr, ns, w.accepting).await?;
    if current.opened > now
        || current.sealed.is_some_and(|sealed| sealed > now)
        || current.drained.is_some()
    {
        return Err(CatalogError::Corrupt("准入代次时间领先维护时钟"));
    }
    let (_, used) = totals(tr, ns, w.accepting, p, false).await?;
    if current.sealed.is_some()
        || now - current.opened >= p.epoch_ms
        || used >= u64::from(p.max_requests_per_epoch)
    {
        if has_epoch_capacity(tr, ns, w, p).await? {
            let next = w
                .accepting
                .checked_add(1)
                .ok_or(CatalogError::Invalid("请求代次溢出"))?;
            current.sealed.get_or_insert(now);
            tr.set(&epoch_key(b'D', ns, w.accepting), &encode_epoch(current));
            tr.set(
                &epoch_key(b'D', ns, next),
                &encode_epoch(Epoch {
                    opened: now,
                    sealed: None,
                    drained: None,
                    retention: p.result_retention_ms,
                }),
            );
            tr.set(
                &requests::window_key(ns),
                &requests::encode_window(RequestWindow {
                    accepting: next,
                    ..w
                }),
            );
            return Ok(MaintenanceAction::Rotated);
        }
        if current.sealed.is_none() {
            current.sealed = Some(now);
            tr.set(&epoch_key(b'D', ns, w.accepting), &encode_epoch(current));
        }
        return Ok(if waiting == MaintenanceAction::Idle {
            MaintenanceAction::AdmissionPaused
        } else {
            waiting
        });
    }
    Ok(waiting)
}

async fn purge_control(tr: &Transaction, ns: Uuid, before: u64) -> Result<usize> {
    if requests::older_rows(tr, b'P', ns, before).await? {
        return Ok(0);
    }
    // Keep epoch descriptors until both results and counters are gone. They
    // hold back rotation while either collector has an expired backlog.
    for kind in *b"BD" {
        let range = RangeOption {
            begin: KeySelector::first_greater_or_equal(epoch_key(kind, ns, 1)),
            end: KeySelector::first_greater_or_equal(epoch_key(kind, ns, before)),
            limit: Some(BATCH),
            ..RangeOption::default()
        };
        let rows = tr.get_range(&range, 1, false).await?;
        for row in rows.iter() {
            tr.clear(row.key());
        }
        if !rows.is_empty() {
            return Ok(rows.len());
        }
    }
    Ok(0)
}

// Observe all sealed epochs in bounded pages. Only looking at the oldest
// would start each retention period after its predecessor expires, eventually
// stalling an otherwise healthy cluster with a chain of 24-hour delays.
async fn drain_page(
    tr: &Transaction,
    ns: Uuid,
    after: u64,
    now: u64,
) -> Result<(u64, usize, usize)> {
    let w = requests::window(tr, ns).await?;
    let first = after.saturating_add(1).max(w.first_retained);
    if first >= w.accepting {
        return Ok((0, 0, 0));
    }
    let prefix = ns_key(b'D', ns);
    let range = RangeOption {
        begin: KeySelector::first_greater_or_equal(epoch_key(b'D', ns, first)),
        end: KeySelector::first_greater_or_equal(epoch_key(b'D', ns, w.accepting)),
        limit: Some(BATCH),
        ..RangeOption::default()
    };
    let rows = tr.get_range(&range, 1, false).await?;
    let mut changes = Vec::new();
    let mut last = 0;
    for row in rows.iter() {
        if row.key().len() != prefix.len() + 8 {
            return Err(CatalogError::Corrupt("维护代次键长度"));
        }
        last = read_u64(&row.key()[prefix.len()..]);
        let mut e = decode_epoch(row.value())?;
        if e.sealed.is_none_or(|sealed| sealed > now) || e.drained.is_some_and(|v| v > now) {
            return Err(CatalogError::Corrupt("旧准入代次时间异常"));
        }
        if e.drained.is_none()
            && !requests::prefix_has_rows(tr, &epoch_key(b'I', ns, last)).await?
            && !requests::prefix_has_rows(tr, &epoch_key(b'0', ns, last)).await?
        {
            now.checked_add(e.retention)
                .ok_or(CatalogError::Invalid("保留期限溢出"))?;
            e.drained = Some(now);
            changes.push((row.key().to_vec(), encode_epoch(e)));
        }
    }
    for (key, value) in &changes {
        tr.set(key, value);
    }
    Ok((
        if rows.len() == BATCH || range.next_range(&rows).is_some() {
            last
        } else {
            0
        },
        rows.len(),
        changes.len(),
    ))
}

async fn tick(
    tr: &Transaction,
    ns: Uuid,
    owner: Uuid,
    sequence: u64,
    elapsed_ms: u64,
) -> Result<MaintenanceReport> {
    requests::require_namespace(tr, ns).await?;
    let mut s = state(tr, ns).await?;
    if s.owner != owner {
        return Err(CatalogError::MaintenanceFenced);
    }
    if s.sequence == sequence {
        return Ok(s.report);
    }
    if s.sequence.checked_add(1) != Some(sequence) {
        return Err(CatalogError::Invalid("维护步序不连续"));
    }
    let now = s
        .clock
        .checked_add(elapsed_ms)
        .filter(|n| *n < u64::MAX)
        .ok_or(CatalogError::Invalid("维护时钟溢出"))?;
    let p = policy(tr, ns).await?;
    let mut report = MaintenanceReport {
        logical_ms: now,
        window: requests::window(tr, ns).await?,
        action: MaintenanceAction::Idle,
        examined: 0,
        compacted: 0,
        forgotten: 0,
    };
    match s.phase {
        0 => report.action = lifecycle(tr, ns, now, p).await?,
        1 => {
            report.forgotten = requests::purge_expired_page(tr, ns, BATCH).await? as u64;
            report.action = MaintenanceAction::PurgedResults;
        }
        2 => {
            let namespace = tr.get(&namespace_key(ns), false).await?;
            if namespace.as_deref() == Some(&[NamespaceState::Ready as u8]) {
                match history::compact_page(tr, ns, s.cursor.as_deref(), BATCH).await {
                    Ok(page) => {
                        report.action = MaintenanceAction::History;
                        report.examined = page.scanned as u64;
                        report.compacted = page.compacted as u64;
                        report.forgotten = page.forgotten as u64;
                        s.cursor = page.next;
                    }
                    Err(CatalogError::Incomplete) => {
                        report.action = MaintenanceAction::WaitingReferences
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        3 => {
            report.forgotten = purge_control(tr, ns, report.window.first_retained).await? as u64;
            report.action = MaintenanceAction::PurgedControl;
        }
        4 => {
            let (after, scanned, drained) = drain_page(tr, ns, s.drain_after, now).await?;
            s.drain_after = after;
            report.examined = scanned as u64;
            report.action = if drained > 0 {
                MaintenanceAction::Drained
            } else {
                MaintenanceAction::Idle
            };
        }
        _ => return Err(CatalogError::Corrupt("维护阶段")),
    }
    report.window = requests::window(tr, ns).await?;
    s.clock = now;
    s.sequence = sequence;
    s.phase = (s.phase + 1) % 5;
    s.report = report;
    tr.set(&ns_key(b'K', ns), &encode_state(&s));
    Ok(report)
}

pub struct MaintenanceDriver<'a> {
    catalog: &'a ManagedCatalog,
    namespace: Uuid,
    owner: Uuid,
    sequence: u64,
    acknowledged: Instant,
}
impl MaintenanceDriver<'_> {
    /// Elapsed time is sampled once for this sequence. Unknown commit retries
    /// return its durable report, never incrementing the clock twice.
    pub async fn poll(&mut self) -> Result<MaintenanceReport> {
        let elapsed: u64 = self
            .acknowledged
            .elapsed()
            .as_millis()
            .try_into()
            .map_err(|_| CatalogError::Invalid("维护时间跨度溢出"))?;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(CatalogError::Invalid("维护步序溢出"))?;
        let namespace = self.namespace;
        let owner = self.owner;
        let report = self
            .catalog
            .db
            .run(|tr, _| async move {
                requests::retry_result(tick(&tr, namespace, owner, sequence, elapsed).await)
            })
            .await??;
        self.sequence = sequence;
        self.acknowledged = Instant::now();
        Ok(report)
    }

    /// Production callers run one driver per namespace under their task leader.
    /// Losing ownership fences this loop; it never cancels requests or pins.
    pub async fn run_until(&mut self, stop: impl std::future::Future<Output = ()>) -> Result<()> {
        tokio::pin!(stop);
        let mut quiet = 0;
        loop {
            let pause = if quiet >= 5 {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(10)
            };
            tokio::select! { _ = &mut stop => return Ok(()), _ = tokio::time::sleep(pause) => {} }
            let r = self.poll().await?;
            let productive = r.forgotten > 0
                || r.compacted > 0
                || (r.action == MaintenanceAction::History && r.examined >= BATCH as u64)
                || matches!(
                    r.action,
                    MaintenanceAction::Rotated
                        | MaintenanceAction::Drained
                        | MaintenanceAction::Expired
                );
            quiet = if productive { 0 } else { (quiet + 1).min(5) };
        }
    }
}

impl ManagedCatalog {
    pub async fn maintenance_policy(&self, namespace: Uuid) -> Result<MaintenancePolicy> {
        let tr = self.db.create_trx()?;
        requests::require_recorded_namespace(&tr, namespace).await?;
        policy(&tr, namespace).await
    }

    /// Policy is immutable once requests or copies exist. Live policy changes
    /// need a separate migration/retention contract, not an implicit resize.
    pub async fn configure_maintenance(
        &self,
        namespace: Uuid,
        new: MaintenancePolicy,
    ) -> Result<bool> {
        new.validate()?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        requests::require_namespace(&tr, namespace).await?;
                        if policy(&tr, namespace).await? == new {
                            return Ok(false);
                        }
                        let w = requests::window(&tr, namespace).await?;
                        if w.first_retained != 1 || w.accepting != 1 {
                            return Err(CatalogError::Invalid("只能配置尚未使用的请求窗口"));
                        }
                        for kind in *b"IPRFB" {
                            if requests::prefix_has_rows(&tr, &ns_key(kind, namespace)).await? {
                                return Err(CatalogError::Invalid(
                                    "已有请求、副本或引用，不能改变维护策略",
                                ));
                            }
                        }
                        let now = state(&tr, namespace).await?.clock;
                        tr.set(&ns_key(b'A', namespace), &encode_policy(new));
                        tr.set(
                            &epoch_key(b'D', namespace, 1),
                            &encode_epoch(Epoch {
                                opened: now,
                                sealed: None,
                                drained: None,
                                retention: new.result_retention_ms,
                            }),
                        );
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Claiming a new driver fences the previous metadata maintainer. Clock and
    /// cursors survive takeover; downtime is conservatively not counted.
    pub async fn maintenance(&self, namespace: Uuid) -> Result<MaintenanceDriver<'_>> {
        let owner = Uuid::new_v4();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        requests::require_namespace(&tr, namespace).await?;
                        policy(&tr, namespace).await?;
                        let mut s = state(&tr, namespace).await?;
                        if s.owner != owner {
                            s.owner = owner;
                            s.sequence = 0;
                            tr.set(&ns_key(b'K', namespace), &encode_state(&s));
                        }
                        Ok(())
                    }
                    .await,
                )
            })
            .await??;
        Ok(MaintenanceDriver {
            catalog: self,
            namespace,
            owner,
            sequence: 0,
            acknowledged: Instant::now(),
        })
    }

    pub async fn maintenance_status(&self, namespace: Uuid) -> Result<MaintenanceStatus> {
        let tr = self.db.create_trx()?;
        requests::require_recorded_namespace(&tr, namespace).await?;
        maintenance_status_in(&tr, namespace).await
    }

    /// A bounded, transaction-consistent view of why the oldest budget slot
    /// cannot be recycled. IDs are diagnostic samples, not cancellation proof.
    pub async fn maintenance_diagnosis(&self, namespace: Uuid) -> Result<MaintenanceDiagnosis> {
        let tr = self.db.create_trx()?;
        requests::require_recorded_namespace(&tr, namespace).await?;
        let status = maintenance_status_in(&tr, namespace).await?;
        let w = status.report.window;
        let first = read_page(&tr, &ns_key(b'D', namespace), None, 1, true).await?;
        let (key, bytes) = first
            .items
            .first()
            .ok_or(CatalogError::Corrupt("维护代次缺失"))?;
        let prefix = ns_key(b'D', namespace);
        if key.len() != prefix.len() + 8 {
            return Err(CatalogError::Corrupt("维护代次键长度"));
        }
        let oldest = read_u64(&key[prefix.len()..]);
        if oldest == 0 || oldest > w.first_retained {
            return Err(CatalogError::Corrupt("维护代次窗口不完整"));
        }
        let e = decode_epoch(bytes)?;
        if e.opened > status.report.logical_ms
            || e.sealed
                .is_some_and(|sealed| sealed > status.report.logical_ms)
            || e.drained
                .is_some_and(|drained| drained > status.report.logical_ms)
            || (oldest < w.accepting && e.sealed.is_none())
        {
            return Err(CatalogError::Corrupt("维护代次时间异常"));
        }
        let blocker = if oldest < w.first_retained {
            if requests::older_rows(&tr, b'P', namespace, w.first_retained).await? {
                Some(MaintenanceBlocker::ExpiredResults(oldest))
            } else {
                let range = RangeOption {
                    begin: KeySelector::first_greater_or_equal(epoch_key(b'B', namespace, 1)),
                    end: KeySelector::first_greater_or_equal(epoch_key(
                        b'B',
                        namespace,
                        w.first_retained,
                    )),
                    limit: Some(1),
                    ..RangeOption::default()
                };
                if !tr.get_range(&range, 1, true).await?.is_empty() {
                    Some(MaintenanceBlocker::ExpiredCounters(oldest))
                } else {
                    Some(MaintenanceBlocker::ExpiredDescriptor(oldest))
                }
            }
        } else if oldest < w.accepting {
            let intent_prefix = epoch_key(b'I', namespace, oldest);
            let intents = read_page(&tr, &intent_prefix, None, 1, true).await?;
            if let Some((key, value)) = intents.items.first() {
                if key.len() != intent_prefix.len() + 16 {
                    return Err(CatalogError::Corrupt("待处理请求键长度"));
                }
                let operation_id = uuid_from(&key[intent_prefix.len()..])?;
                requests::decode_intent(value, namespace, oldest, operation_id)?;
                Some(MaintenanceBlocker::PendingRequest(PendingRequestId {
                    request_epoch: oldest,
                    operation_id,
                }))
            } else if let Some((key, value)) =
                read_page(&tr, &epoch_key(b'0', namespace, oldest), None, 1, true)
                    .await?
                    .items
                    .first()
            {
                let prefix = epoch_key(b'0', namespace, oldest);
                if key.len() != prefix.len() + 16 {
                    return Err(CatalogError::Corrupt("写前预约键长度"));
                }
                let operation_id = uuid_from(&key[prefix.len()..])?;
                reservation::decode(value, namespace, oldest, operation_id)?;
                Some(MaintenanceBlocker::PendingRequest(PendingRequestId {
                    request_epoch: oldest,
                    operation_id,
                }))
            } else if e.drained.is_none() {
                Some(MaintenanceBlocker::AwaitingDrain(oldest))
            } else {
                let ref_prefix = epoch_key(b'F', namespace, oldest);
                let refs = read_page(&tr, &ref_prefix, None, 1, true).await?;
                if let Some((key, value)) = refs.items.first() {
                    Some(MaintenanceBlocker::HistoryReference(decode_reference(
                        &ref_prefix,
                        key,
                        value,
                        oldest,
                    )?))
                } else {
                    let deadline = e
                        .drained
                        .ok_or(CatalogError::Corrupt("维护排空时间缺失"))?
                        .checked_add(e.retention)
                        .ok_or(CatalogError::Corrupt("保留期限溢出"))?;
                    if deadline > status.report.logical_ms {
                        Some(MaintenanceBlocker::Retaining {
                            request_epoch: oldest,
                            remaining_ms: deadline - status.report.logical_ms,
                        })
                    } else {
                        Some(MaintenanceBlocker::ReadyToExpire(oldest))
                    }
                }
            }
        } else if !status.accepting_requests {
            Some(MaintenanceBlocker::AdmissionPaused(oldest))
        } else {
            None
        };
        Ok(MaintenanceDiagnosis {
            status,
            oldest_budget_epoch: oldest,
            blocker,
        })
    }

    /// List stable pin IDs for explicit review. Releasing a pin requires the
    /// caller's own proof that its reader/export is finished.
    pub async fn history_references(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<HistoryReference>> {
        let prefix = ns_key(b'F', namespace);
        if !(1..=100).contains(&limit)
            || after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 24)
        {
            return Err(CatalogError::Invalid("历史引用分页参数无效"));
        }
        let tr = self.db.create_trx()?;
        requests::require_recorded_namespace(&tr, namespace).await?;
        let w = requests::window(&tr, namespace).await?;
        if let Some(after) = after {
            let epoch = read_u64(&after[prefix.len()..]);
            if epoch < w.first_retained || epoch > w.accepting {
                return Err(CatalogError::Invalid("历史引用游标代次已过期"));
            }
        }
        let page = read_page(&tr, &prefix, after, limit, true).await?;
        let items = page
            .items
            .iter()
            .map(|(key, value)| {
                if key.len() != prefix.len() + 24 {
                    return Err(CatalogError::Corrupt("历史引用键长度"));
                }
                let epoch = read_u64(&key[prefix.len()..]);
                if epoch < w.first_retained || epoch > w.accepting {
                    return Err(CatalogError::Corrupt("历史引用代次越界"));
                }
                decode_reference(&key[..prefix.len() + 8], key, value, epoch)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: page.next,
        })
    }
}

fn decode_reference(
    prefix: &[u8],
    key: &[u8],
    value: &[u8],
    epoch: u64,
) -> Result<HistoryReference> {
    if key.len() != prefix.len() + 16 || !key.starts_with(prefix) || !value.is_empty() {
        return Err(CatalogError::Corrupt("历史引用记录"));
    }
    let id = uuid_from(&key[prefix.len()..])?;
    if id.is_nil() {
        return Err(CatalogError::Corrupt("历史引用 ID 为 nil"));
    }
    Ok(HistoryReference {
        request_epoch: epoch,
        reference_id: id,
    })
}

async fn maintenance_status_in(tr: &Transaction, namespace: Uuid) -> Result<MaintenanceStatus> {
    let mut report = state(tr, namespace).await?.report;
    report.window = requests::window(tr, namespace).await?;
    let p = policy(tr, namespace).await?;
    let (pending, admitted) = totals(tr, namespace, report.window.accepting, p, true).await?;
    let open = epoch(tr, namespace, report.window.accepting)
        .await?
        .sealed
        .is_none();
    Ok(MaintenanceStatus {
        report,
        policy: p,
        accepting_requests: open
            && admitted < u64::from(p.max_requests_per_epoch)
            && pending < u64::from(p.max_pending),
        pending,
        admitted_current_epoch: admitted,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn policy_shares_and_persistent_state_validation() {
        let ns = Uuid::new_v4();
        for pending in [1, 2, 63, 64, 65, 4096, u32::MAX] {
            let p = MaintenancePolicy {
                max_pending: pending,
                max_requests_per_epoch: pending,
                ..MaintenancePolicy::default()
            };
            assert_eq!(decode_policy(&encode_policy(p)).unwrap(), p);
            assert_eq!(
                (0..p.shards()).map(|s| p.share(pending, s)).sum::<u64>(),
                u64::from(pending)
            );
        }
        let p = MaintenancePolicy::default();
        for bad in [
            MaintenancePolicy { epoch_ms: 0, ..p },
            MaintenancePolicy {
                result_retention_ms: 0,
                ..p
            },
            MaintenancePolicy {
                max_pending: 0,
                ..p
            },
            MaintenancePolicy {
                max_requests_per_epoch: 1,
                ..p
            },
            MaintenancePolicy {
                max_retained_epochs: 1,
                ..p
            },
            MaintenancePolicy {
                max_retained_epochs: 4097,
                ..p
            },
            MaintenancePolicy {
                epoch_ms: u64::MAX,
                ..p
            },
        ] {
            assert!(bad.validate().is_err());
            assert!(decode_policy(&encode_policy(bad)).is_err());
        }
        let mut s = initial_state();
        s.cursor = Some(copy_key(ns, Uuid::new_v4(), Uuid::new_v4()));
        let bytes = encode_state(&s);
        assert_eq!(decode_state(ns, &bytes).unwrap().cursor, s.cursor);
        for n in 0..bytes.len() {
            assert!(decode_state(ns, &bytes[..n]).is_err());
        }
        assert!(decode_state(Uuid::new_v4(), &bytes).is_err());
        for (offset, value) in [
            (0, 0xff),
            (32, 5),
            (35 + s.cursor.as_ref().unwrap().len(), 11),
        ] {
            let mut bad = bytes.clone();
            if offset == 0 {
                bad[..8].fill(value);
            } else {
                bad[offset] = value;
            }
            assert!(decode_state(ns, &bad).is_err());
        }
        let e = Epoch {
            opened: 5,
            sealed: Some(6),
            drained: Some(7),
            retention: 9,
        };
        let encoded = encode_epoch(e);
        for n in 0..32 {
            assert!(decode_epoch(&encoded[..n]).is_err());
        }
        for bad in [
            Epoch {
                sealed: Some(4),
                ..e
            },
            Epoch { sealed: None, ..e },
            Epoch {
                drained: Some(5),
                ..e
            },
        ] {
            assert!(decode_epoch(&encode_epoch(bad)).is_err());
        }
    }

    fn receipt(ns: Uuid, serial: &str, epoch: u64) -> CommitReceipt {
        let file = Uuid::new_v4();
        test_receipt(CommitReceipt {
            namespace: ns,
            parent: Uuid::nil(),
            name: file.to_string().into_bytes(),
            file_id: file,
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: epoch,
            copy_id: Uuid::nil(),
            library_serial: serial.into(),
            pool_id: None,
            owner_epoch: 10,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 1,
            size: 1,
            content_digest: ContentDigest::Sha256([8; 32]),
            metadata_digest: None,
        })
    }

    async fn step(d: &mut MaintenanceDriver<'_>, elapsed: u64) -> MaintenanceReport {
        let ns = d.namespace;
        let owner = d.owner;
        let seq = d.sequence + 1;
        let report =
            d.catalog
                .db
                .run(|tr, _| async move {
                    requests::retry_result(tick(&tr, ns, owner, seq, elapsed).await)
                })
                .await
                .unwrap()
                .unwrap();
        // Simulate a lost commit acknowledgement. A retry must reuse its report
        // even when the caller supplies another elapsed interval.
        let repeated = d
            .catalog
            .db
            .run(|tr, _| async move {
                requests::retry_result(tick(&tr, ns, owner, seq, elapsed + 123).await)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report, repeated);
        d.sequence = seq;
        report
    }

    async fn cycle(d: &mut MaintenanceDriver<'_>, elapsed: u64) -> MaintenanceReport {
        let r = step(d, elapsed).await;
        for _ in 0..4 {
            step(d, 0).await;
        }
        r
    }

    pub(crate) async fn exercise(c: &ManagedCatalog, cluster_file: &str) {
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        let p = MaintenancePolicy {
            epoch_ms: 10,
            result_retention_ms: 20,
            max_pending: 2,
            max_requests_per_epoch: 4,
            max_retained_epochs: 2,
        };
        assert!(c.configure_maintenance(ns, p).await.unwrap());
        assert!(!c.configure_maintenance(ns, p).await.unwrap());
        let serial = format!("maintenance-{}", Uuid::new_v4());
        c.advance_owner_epoch(&serial, 10).await.unwrap();
        let a = receipt(ns, &serial, 1);
        let b = receipt(ns, &serial, 1);
        let extra = receipt(ns, &serial, 1);
        let (ra, rb, re) = tokio::join!(
            c.register_request(&a),
            c.register_request(&b),
            c.register_request(&extra)
        );
        let results = [ra.unwrap(), rb.unwrap(), re.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|v| **v == PublishOutcome::Registered)
                .count(),
            2
        );
        assert_eq!(
            results
                .iter()
                .filter(|v| **v == PublishOutcome::AdmissionLimited)
                .count(),
            1
        );
        let admitted: Vec<_> = [&a, &b, &extra]
            .into_iter()
            .zip(results)
            .filter_map(|(r, result)| (result == PublishOutcome::Registered).then_some(r))
            .collect();
        let a = admitted[0];
        let b = admitted[1];
        assert_eq!(c.maintenance_status(ns).await.unwrap().pending, 2);
        assert_eq!(
            c.register_request(a).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(c.publish(a).await.unwrap(), PublishOutcome::VolumeNotReady);
        assert_eq!(c.maintenance_status(ns).await.unwrap().pending, 2);
        assert!(
            c.configure_maintenance(ns, MaintenancePolicy::default())
                .await
                .is_err()
        );

        // Broken quota must not cancel the intent or publish a partial result.
        let tr = c.db.create_trx().unwrap();
        let raw = tr
            .get(&requests::intent_key(a), false)
            .await
            .unwrap()
            .unwrap();
        let permit = requests::matching_permit(&raw, a).unwrap().unwrap();
        let quota = quota_key(ns, 0, permit);
        let saved = tr.get(&quota, false).await.unwrap().unwrap().to_vec();
        tr.clear(&quota);
        tr.commit().await.unwrap();
        assert!(matches!(
            c.cancel_request(a).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert_eq!(
            c.pending_requests(ns, None, 100).await.unwrap().items.len(),
            2
        );
        let tr = c.db.create_trx().unwrap();
        tr.set(&quota, &saved);
        tr.commit().await.unwrap();
        assert_eq!(
            c.cancel_request(a).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert_eq!(
            c.cancel_request(a).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        assert_eq!(c.maintenance_status(ns).await.unwrap().pending, 1);
        // Resolve another two requests; total admission is never released.
        let r = receipt(ns, &serial, 1);
        assert_eq!(
            c.register_request(&r).await.unwrap(),
            PublishOutcome::Registered
        );
        c.cancel_request(&r).await.unwrap();
        c.cancel_request(b).await.unwrap();
        let b = receipt(ns, &serial, 1);
        assert_eq!(
            c.register_request(&b).await.unwrap(),
            PublishOutcome::Registered
        );
        let blocked = receipt(ns, &serial, 1);
        assert_eq!(
            c.register_request(&blocked).await.unwrap(),
            PublishOutcome::AdmissionLimited
        );
        let status = c.maintenance_status(ns).await.unwrap();
        assert_eq!(status.admitted_current_epoch, 4);
        assert!(!status.accepting_requests);

        let mut driver = c.maintenance(ns).await.unwrap();
        assert_eq!(
            cycle(&mut driver, 0).await.action,
            MaintenanceAction::Rotated
        );
        let next = receipt(ns, &serial, 2);
        assert_eq!(
            c.register_request(&next).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            cycle(&mut driver, 100).await.action,
            MaintenanceAction::WaitingRequests
        );
        assert_eq!(
            c.pending_requests(ns, None, 100).await.unwrap().items.len(),
            2
        );
        assert!(!c.maintenance_status(ns).await.unwrap().accepting_requests);
        assert_eq!(
            c.rotate_request_epoch(ns, 2).await.unwrap(),
            WindowOutcome::RetentionLimited
        );
        c.cancel_request(&b).await.unwrap();
        c.cancel_request(&next).await.unwrap();
        assert_eq!(
            cycle(&mut driver, 0).await.action,
            MaintenanceAction::Drained
        );
        let pin = Uuid::new_v4();
        c.pin_request_history(ns, 1, pin).await.unwrap();
        assert_eq!(
            cycle(&mut driver, 100).await.action,
            MaintenanceAction::WaitingReferences
        );
        assert_eq!(c.request_window(ns).await.unwrap().first_retained, 1);
        c.release_request_history(ns, 1, pin).await.unwrap();
        assert_eq!(
            cycle(&mut driver, 0).await.action,
            MaintenanceAction::Expired
        );
        // P was collected, B was collected, but D has not yet been collected:
        // even expired epochs still hold a place in the storage budget.
        assert_eq!(
            c.rotate_request_epoch(ns, 2).await.unwrap(),
            WindowOutcome::RetentionLimited
        );
        cycle(&mut driver, 0).await;
        assert_eq!(
            cycle(&mut driver, 0).await.action,
            MaintenanceAction::Rotated
        );
        assert_eq!(
            c.register_request(a).await.unwrap(),
            PublishOutcome::RequestExpired
        );
        let before = c.maintenance_status(ns).await.unwrap();
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let mut successor = reopened.maintenance(ns).await.unwrap();
        assert!(matches!(
            driver.poll().await,
            Err(CatalogError::MaintenanceFenced)
        ));
        assert_eq!(reopened.maintenance_status(ns).await.unwrap(), before);
        let r = step(&mut successor, 1).await;
        assert_eq!(r.logical_ms, before.report.logical_ms + 1);
        successor.run_until(std::future::ready(())).await.unwrap();

        // Retention starts when sealed work is actually drained, not at close.
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        c.configure_maintenance(ns, p).await.unwrap();
        let r = receipt(ns, &serial, 1);
        c.register_request(&r).await.unwrap();
        let mut d = c.maintenance(ns).await.unwrap();
        assert_eq!(cycle(&mut d, 10).await.action, MaintenanceAction::Rotated);
        assert_eq!(
            cycle(&mut d, 1000).await.action,
            MaintenanceAction::WaitingRequests
        );
        c.cancel_request(&r).await.unwrap();
        assert_eq!(cycle(&mut d, 0).await.action, MaintenanceAction::Drained);
        assert_eq!(cycle(&mut d, 19).await.action, MaintenanceAction::Retaining);
        assert_eq!(c.request_window(ns).await.unwrap().first_retained, 1);
        assert_eq!(cycle(&mut d, 1).await.action, MaintenanceAction::Expired);

        // Old descriptors must survive until every expired terminal is gone.
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        c.configure_maintenance(ns, p).await.unwrap();
        c.rotate_request_epoch(ns, 1).await.unwrap();
        c.advance_request_floor(ns, 1, 2).await.unwrap();
        let tr = c.db.create_trx().unwrap();
        for _ in 0..201 {
            tr.set(&op_key(ns, 1, Uuid::new_v4()), &[0; 41]);
        }
        tr.commit().await.unwrap();
        let mut d = c.maintenance(ns).await.unwrap();
        assert_eq!(
            cycle(&mut d, 10).await.action,
            MaintenanceAction::AdmissionPaused
        );
        assert_eq!(
            c.rotate_request_epoch(ns, 2).await.unwrap(),
            WindowOutcome::RetentionLimited
        );
        cycle(&mut d, 0).await;
        assert_eq!(
            c.rotate_request_epoch(ns, 2).await.unwrap(),
            WindowOutcome::RetentionLimited
        );
        let mut rotated = false;
        for _ in 0..100 {
            let r = step(&mut d, 0).await;
            assert!(r.forgotten <= 100);
            if r.action == MaintenanceAction::Rotated {
                rotated = true;
                break;
            }
        }
        assert!(rotated);

        // Later epochs drain independently while the oldest is still retained.
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        c.configure_maintenance(
            ns,
            MaintenancePolicy {
                max_retained_epochs: 4,
                ..p
            },
        )
        .await
        .unwrap();
        c.rotate_request_epoch(ns, 1).await.unwrap();
        c.rotate_request_epoch(ns, 2).await.unwrap();
        let mut d = c.maintenance(ns).await.unwrap();
        cycle(&mut d, 0).await;
        let tr = c.db.create_trx().unwrap();
        assert_eq!(epoch(&tr, ns, 1).await.unwrap().drained, Some(0));
        assert_eq!(epoch(&tr, ns, 2).await.unwrap().drained, Some(0));
        assert_eq!(c.request_window(ns).await.unwrap().first_retained, 1);

        // FDB may return a short range page with more=true. Drain observation
        // must resume after that page, rather than revisiting its prefix forever.
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        c.configure_maintenance(
            ns,
            MaintenancePolicy {
                max_retained_epochs: 256,
                ..p
            },
        )
        .await
        .unwrap();
        for epoch in 1..=200 {
            c.rotate_request_epoch(ns, epoch).await.unwrap();
        }
        let mut d = c.maintenance(ns).await.unwrap();
        let mut complete = false;
        for _ in 0..100 {
            cycle(&mut d, 0).await;
            if epoch(&c.db.create_trx().unwrap(), ns, 200)
                .await
                .unwrap()
                .drained
                .is_some()
            {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert_eq!(c.request_window(ns).await.unwrap().first_retained, 1);

        // Real catalog publications release permits atomically. The automatic
        // history cursor survives takeover without deleting current files.
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        c.begin_volume_reconcile(&serial, 10, volume, scan)
            .await
            .unwrap();
        super::super::tests::verify_scan(c, &serial, 10, volume, scan, &[]).await;
        c.mark_volume_ready(&serial, 10, volume, scan)
            .await
            .unwrap();
        let mut receipts = Vec::new();
        for _ in 0..101 {
            let r = test_receipt(CommitReceipt {
                volume_id: volume,
                volume_scan_id: scan.id,
                ..receipt(ns, &serial, 1)
            });
            assert_eq!(
                c.register_request(&r).await.unwrap(),
                PublishOutcome::Registered
            );
            if receipts.is_empty() {
                let tr = c.db.create_trx().unwrap();
                let raw = tr
                    .get(&requests::intent_key(&r), false)
                    .await
                    .unwrap()
                    .unwrap();
                let permit = requests::matching_permit(&raw, &r).unwrap().unwrap();
                let quota = quota_key(ns, 0, permit);
                let saved = tr.get(&quota, false).await.unwrap().unwrap().to_vec();
                tr.clear(&quota);
                tr.commit().await.unwrap();
                assert!(matches!(c.publish(&r).await, Err(CatalogError::Corrupt(_))));
                assert!(c.file_head(ns, r.file_id).await.unwrap().is_none());
                assert!(c.node(ns, r.file_id).await.unwrap().is_none());
                let tr = c.db.create_trx().unwrap();
                tr.set(&quota, &saved);
                tr.commit().await.unwrap();
            }
            assert_eq!(c.publish(&r).await.unwrap(), PublishOutcome::Applied(1));
            assert_eq!(
                c.publish(&r).await.unwrap(),
                PublishOutcome::AlreadyApplied(1)
            );
            receipts.push(r);
        }
        assert_eq!(c.maintenance_status(ns).await.unwrap().pending, 0);
        let mut d = c.maintenance(ns).await.unwrap();
        step(&mut d, 0).await;
        step(&mut d, 0).await;
        let page = step(&mut d, 0).await;
        assert_eq!(page.examined, 100);
        let saved = state(&c.db.create_trx().unwrap(), ns).await.unwrap();
        assert!(saved.cursor.is_some());
        let mut successor = reopened.maintenance(ns).await.unwrap();
        let resumed = state(&c.db.create_trx().unwrap(), ns).await.unwrap();
        assert_eq!(resumed.cursor, saved.cursor);
        assert_eq!(resumed.phase, saved.phase);
        for _ in 0..4 {
            step(&mut successor, 0).await;
        }
        let page = step(&mut successor, 0).await;
        assert_eq!(page.examined, 1);
        assert_eq!(page.forgotten, 0);
        assert!(
            state(&c.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
                .cursor
                .is_none()
        );
        for r in receipts {
            assert!(c.file_head(ns, r.file_id).await.unwrap().is_some());
            assert!(
                c.legacy_copy_record_for_test(ns, r.file_id, r.copy_id)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        exercise_diagnosis(c, cluster_file, &serial).await;
    }

    async fn exercise_diagnosis(c: &ManagedCatalog, cluster_file: &str, serial: &str) {
        let ns = Uuid::new_v4();
        c.create_namespace(ns).await.unwrap();
        let p = MaintenancePolicy {
            epoch_ms: 10,
            result_retention_ms: 20,
            max_pending: 2,
            max_requests_per_epoch: 4,
            max_retained_epochs: 2,
        };
        c.configure_maintenance(ns, p).await.unwrap();
        let healthy = c.maintenance_diagnosis(ns).await.unwrap();
        assert_eq!(healthy.oldest_budget_epoch, 1);
        assert_eq!(healthy.blocker, None);

        let r = receipt(ns, serial, 1);
        c.register_request(&r).await.unwrap();
        c.rotate_request_epoch(ns, 1).await.unwrap();
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::PendingRequest(PendingRequestId {
                request_epoch: 1,
                operation_id: r.operation_id,
            }))
        );
        // Corrupt intent must not be reported as a trustworthy task identity.
        let key = requests::intent_key(&r);
        let tr = c.db.create_trx().unwrap();
        let saved = tr.get(&key, false).await.unwrap().unwrap().to_vec();
        tr.set(&key, b"broken");
        tr.commit().await.unwrap();
        assert!(matches!(
            c.maintenance_diagnosis(ns).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        tr.set(&key, &saved);
        tr.commit().await.unwrap();
        c.cancel_request(&r).await.unwrap();
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::AwaitingDrain(1))
        );

        let ids = [
            Uuid::from_u128(11),
            Uuid::from_u128(22),
            Uuid::from_u128(33),
        ];
        for id in ids {
            c.pin_request_history(ns, 1, id).await.unwrap();
        }
        assert!(c.history_references(ns, None, 0).await.is_err());
        assert!(c.history_references(ns, None, 101).await.is_err());
        assert!(c.history_references(ns, Some(&[0; 45]), 1).await.is_err());
        let first = c.history_references(ns, None, 2).await.unwrap();
        assert_eq!(
            first
                .items
                .iter()
                .map(|r| r.reference_id)
                .collect::<Vec<_>>(),
            ids[..2]
        );
        let cursor = first.next.unwrap();
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let second = reopened
            .history_references(ns, Some(&cursor), 2)
            .await
            .unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|r| r.reference_id)
                .collect::<Vec<_>>(),
            ids[2..]
        );
        assert!(second.next.is_none());

        let mut first_key = epoch_key(b'F', ns, 1);
        first_key.extend_from_slice(ids[0].as_bytes());
        let tr = c.db.create_trx().unwrap();
        tr.set(&first_key, &[1]);
        tr.commit().await.unwrap();
        assert!(matches!(
            c.history_references(ns, None, 2).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        tr.set(&first_key, &[]);
        tr.commit().await.unwrap();

        let mut driver = c.maintenance(ns).await.unwrap();
        assert_eq!(
            cycle(&mut driver, 0).await.action,
            MaintenanceAction::Drained
        );
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::HistoryReference(HistoryReference {
                request_epoch: 1,
                reference_id: ids[0],
            }))
        );
        for id in ids {
            c.release_request_history(ns, 1, id).await.unwrap();
        }
        assert!(
            c.history_references(ns, None, 2)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::Retaining {
                request_epoch: 1,
                remaining_ms: 20
            })
        );
        assert_eq!(
            cycle(&mut driver, 19).await.action,
            MaintenanceAction::Retaining
        );
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::Retaining {
                request_epoch: 1,
                remaining_ms: 1
            })
        );
        assert_eq!(
            c.rotate_request_epoch(ns, 2).await.unwrap(),
            WindowOutcome::RetentionLimited
        );
        assert_eq!(
            step(&mut driver, 1).await.action,
            MaintenanceAction::Expired
        );
        assert!(c.history_references(ns, Some(&cursor), 1).await.is_err());
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::ExpiredResults(1))
        );
        assert_eq!(
            step(&mut driver, 0).await.action,
            MaintenanceAction::PurgedResults
        );
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::ExpiredCounters(1))
        );
        step(&mut driver, 0).await;
        assert_eq!(
            step(&mut driver, 0).await.action,
            MaintenanceAction::PurgedControl
        );
        assert_eq!(
            c.maintenance_diagnosis(ns).await.unwrap().blocker,
            Some(MaintenanceBlocker::ExpiredDescriptor(1))
        );
        step(&mut driver, 0).await;
        step(&mut driver, 0).await;
        step(&mut driver, 0).await;
        step(&mut driver, 0).await;
        step(&mut driver, 0).await;
        assert_eq!(
            c.maintenance_diagnosis(ns)
                .await
                .unwrap()
                .oldest_budget_epoch,
            2
        );
    }
}
