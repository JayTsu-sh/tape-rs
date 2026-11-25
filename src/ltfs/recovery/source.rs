//! Verified on-disk node payloads for native Full/Incremental source intake.
//! The original construct identities remain separate from the replayed view.
use super::*;
use crate::ltfs::index::IndexEntry;
use crate::ltfs::index_replay::IndexReplay;
use crate::ltfs::volume::CommittedIndexConstruct;

/// A read-only source whose complete DP/IP and required delta-chain proof has
/// succeeded. Its private database disappears on drop. Callers still own all
/// Serving/PR checks and must revalidate media before catalog publication.
pub struct CapturedIndexSource {
    construct: CommittedIndexConstruct,
    header: LtfsIndex,
    replay: IndexReplay,
    predecessors: Vec<CommittedIndexConstruct>,
    source_bytes: u64,
    construct_end: u64,
    scope: IndexSourceScope,
    full_history: Option<FullHistoryDirectory>,
    history_hint_used: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexSourceScope {
    Current {
        dp_eod: u64,
        ip_generation: Option<u64>,
        history_depth: u32,
        history_truncated: bool,
    },
    Recorded,
}

/// Expected physical bounds of a previously verified complete Current Full.
/// May carry its bounded physical history schedule for a fresh readback.
pub struct ReclaimReadbackFullHead {
    construct_end: u64,
    scope: IndexSourceScope,
    history: FullHistoryManifest,
    schedule: Option<FullHistoryDirectory>,
}

impl ReclaimReadbackFullHead {
    /// Independently read the current Full, all predecessors, and DP/IP.
    /// An exact raw Full match preserves the already-verified tree and
    /// member layouts; no second replay database is needed.
    pub fn verify(
        self,
        drive: &TapeDrive<'_>,
        label: &LtfsLabel,
        budget: &RecoveryBudget,
    ) -> Result<()> {
        let (proof, history) = if let Some(schedule) = self
            .schedule
            .filter(|schedule| Self::schedule_fits(schedule, budget.index_scratch.max_bytes))
        {
            if schedule.manifest() != self.history
                || label.volume_uuid != self.history.head.volume_uuid
                || !self
                    .history
                    .matches(label, self.history.head, self.construct_end)
            {
                return Err(TapeError::RecoveryRestricted {
                    reason: "回收组历史排程与原卷头、标签或EOD不符".into(),
                });
            }
            let mut remaining = budget.clone();
            remaining.index_scratch.max_bytes -= schedule.reserved_bytes();
            let proof = super::streamed::verify_scheduled_full_recovery(
                drive, label, &remaining, &schedule,
            )?;
            (proof, self.history)
        } else {
            super::streamed::verify_complete_full_recovery(drive, label, budget)?
        };
        let scope = IndexSourceScope::Current {
            dp_eod: proof.dp_eod,
            ip_generation: proof.ip_generation,
            history_depth: proof.history_depth,
            history_truncated: proof.history_truncated,
        };
        if proof.construct != self.history.head
            || proof.dp_eod != self.construct_end
            || scope != self.scope
            || history != self.history
        {
            return Err(TapeError::RecoveryRestricted {
                reason: "回收组正文读后实际卷头、历史或EOD/closing改变".into(),
            });
        }
        Ok(())
    }

    fn schedule_fits(schedule: &FullHistoryDirectory, scratch_bytes: u64) -> bool {
        // The old history directory still holds its comparison reservation.
        // Leave at least that much for the fresh ascending comparison.
        let reserved = schedule.reserved_bytes();
        scratch_bytes.saturating_sub(reserved) >= reserved
    }
}

impl CapturedIndexSource {
    // A complete Full capture keeps the replay and history directory while
    // allocating the final DP/IP comparison. Reserve one chunk for each.
    pub(crate) const RECLAIM_PREFLIGHT_SCRATCH_MIN_BYTES: u64 =
        3 * super::super::index_scratch::CHUNK;
    pub(crate) const RECLAIM_PREFLIGHT_WITH_HINT_SCRATCH_MIN_BYTES: u64 =
        Self::RECLAIM_PREFLIGHT_SCRATCH_MIN_BYTES + super::super::index_scratch::CHUNK;

    pub(crate) fn scratch_reserved_bytes(&self) -> u64 {
        self.replay.reserved_bytes().saturating_add(
            self.full_history
                .as_ref()
                .map_or(0, FullHistoryDirectory::reserved_bytes),
        )
    }

    /// Only a verified complete Current Full with a canonical history
    /// directory can take the compact destination readback path.
    pub fn reclaim_readback_full_head(&self) -> Option<ReclaimReadbackFullHead> {
        let IndexSourceScope::Current {
            dp_eod,
            history_truncated: false,
            ..
        } = self.scope
        else {
            return None;
        };
        let history = self.full_history.as_ref()?.manifest();
        if self.construct.incremental
            || !self.predecessors.is_empty()
            || self.construct != history.head
            || self.construct_end != dp_eod
            || history.dp_eod != dp_eod
        {
            return None;
        }
        Some(ReclaimReadbackFullHead {
            construct_end: self.construct_end,
            scope: self.scope,
            history,
            schedule: None,
        })
    }

    /// Keep only the eligible history directory across the body read. The
    /// replay can be dropped before B obtains its own comparison database.
    pub fn take_reclaim_readback_full_head(
        &mut self,
        scratch_bytes: u64,
    ) -> Option<ReclaimReadbackFullHead> {
        let mut full_head = self.reclaim_readback_full_head()?;
        full_head.schedule = self
            .take_full_history_hint()
            .filter(|schedule| ReclaimReadbackFullHead::schedule_fits(schedule, scratch_bytes));
        Some(full_head)
    }

    /// Whether this capture actually completed physical ascending hint reads.
    pub fn history_hint_used(&self) -> bool {
        self.history_hint_used
    }

    /// Complete Full-history addresses from this actual capture. These remain
    /// scheduling hints: a later capture must read every construct again.
    pub fn full_history_hint(&self) -> Option<&FullHistoryDirectory> {
        self.full_history.as_ref()
    }

    pub fn take_full_history_hint(&mut self) -> Option<FullHistoryDirectory> {
        self.full_history.take()
    }

    pub fn scope(&self) -> IndexSourceScope {
        self.scope
    }
    /// Exclusive logical block immediately after this head's closing filemark,
    /// observed by the same verified read. Recorded heads may precede DP EOD.
    pub fn construct_end(&self) -> u64 {
        self.construct_end
    }
    pub fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    pub fn identity(&self) -> Result<IndexSourceIdentity> {
        IndexSourceIdentity::new(self.construct, self.predecessors.clone())
    }
    /// Exact raw on-tape head, including its raw SHA and incremental flag.
    pub fn construct(&self) -> CommittedIndexConstruct {
        self.construct
    }
    /// Child-free view header and root attributes. Enumerate entries for the
    /// complete directory contents; serializing this header is not a source.
    pub fn header(&self) -> &LtfsIndex {
        &self.header
    }
    /// Immediate predecessor first, ending at the required Full baseline.
    /// Full heads use the separately bounded historical audit instead.
    pub fn predecessors(&self) -> &[CommittedIndexConstruct] {
        &self.predecessors
    }
    /// Emit complete nodes in bytewise path order, including the root. Each
    /// directory is child-free. Keep callback output provisional until success.
    pub fn for_each_entry(&self, emit: impl FnMut(IndexEntry) -> Result<()>) -> Result<()> {
        self.replay.for_each_payload(emit)
    }

    /// Resume the same verified on-disk view in bytewise path order, retaining
    /// at most one decoded node in the reader. Callback results are provisional
    /// until this call succeeds. The first page includes the root.
    pub fn for_each_entry_page(
        &self,
        after: Option<&str>,
        limit: usize,
        emit: impl FnMut(IndexEntry) -> Result<()>,
    ) -> Result<()> {
        self.replay.for_each_payload_page(after, limit, emit)
    }

    pub(crate) fn entry_by_uid(&self, uid: u64) -> Result<Option<IndexEntry>> {
        self.replay.payload_by_uid(uid)
    }
    pub(crate) fn entry_by_path(&self, path: &str) -> Result<Option<IndexEntry>> {
        self.replay.payload_by_path(path)
    }
}

/// Capture complete history for reclaim, bounded by the actual head's block
/// position. Every verified predecessor must occupy a strictly earlier block,
/// so this finite geometric bound replaces the general historical-depth cap.
/// Incremental replay retains its original depth/identity budget. XML,
/// node/scratch storage and tail-search budgets remain unchanged. A
/// truncated history is an error, never permission to migrate or initialize.
pub fn capture_complete_index_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    capture_complete_index_source_with_history_hint(drive, label, budget, None)
}

/// Capture the target preparation view and audit its Full history under the
/// same read-only guard. The private replay seeds the audit, which still
/// independently rereads the current DP head and verifies IP before return.
pub fn capture_reclaim_preparation_index_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    capture_reclaim_preparation_index_source_with_history_hint(drive, label, budget, None)
}

/// Use a bounded history schedule without losing the preparation-specific
/// head reuse when the schedule is missing or cannot be used.
pub fn capture_reclaim_preparation_index_source_with_history_hint(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: Option<FullHistoryDirectory>,
) -> Result<CapturedIndexSource> {
    capture_complete_index_source_inner(drive, label, budget, hint, true)
}

/// A hint changes read order only. Ownership is transferred so its temporary
/// disk reservation can be released before an optional-capacity fallback.
/// Every historical XML/filemark/hash and original DP/IP check still runs.
pub fn capture_complete_index_source_with_history_hint(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: Option<FullHistoryDirectory>,
) -> Result<CapturedIndexSource> {
    capture_complete_index_source_inner(drive, label, budget, hint, false)
}

fn capture_complete_index_source_inner(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: Option<FullHistoryDirectory>,
    reuse_captured_full: bool,
) -> Result<CapturedIndexSource> {
    let mut hint = hint;
    let complete = budget.clone().with_complete_full_history();
    let mut with_hint = complete.clone();
    with_hint.index_scratch.max_bytes = complete.index_scratch.max_bytes.saturating_sub(
        hint.as_ref()
            .map_or(0, FullHistoryDirectory::reserved_bytes),
    );
    let mut optional_capacity = false;
    let result = capture_index_source_inner(
        drive,
        label,
        &with_hint,
        hint.as_mut(),
        true,
        reuse_captured_full,
        &mut optional_capacity,
    );
    let captured = match result {
        // Only policy capacity from the optional directory may try the exact
        // original complete capture without it. Device/IO/parse errors escape.
        Err(TapeError::LocalCapacity { .. }) if optional_capacity => {
            drop(hint);
            capture_index_source_inner(
                drive,
                label,
                &complete,
                None,
                false,
                reuse_captured_full,
                &mut false,
            )?
        }
        other => other?,
    };
    if matches!(
        captured.scope(),
        IndexSourceScope::Current {
            history_truncated: true,
            ..
        }
    ) {
        return Err(TapeError::RecoveryRestricted {
            reason: "完整历史超出可计数深度，禁止接受部分证明".into(),
        });
    }
    Ok(captured)
}

/// Discover a clean latest DP head and retain complete replayed node payloads
/// on bounded local disk. No index XML, file tree or path set grows in memory.
/// This does not format, repair, checkpoint, write MAM or authorize appends.
pub fn capture_index_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    capture_index_source_inner(drive, label, budget, None, false, false, &mut false)
}

fn capture_index_source_inner(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: Option<&mut FullHistoryDirectory>,
    collect_history: bool,
    reuse_captured_full: bool,
    optional_capacity: &mut bool,
) -> Result<CapturedIndexSource> {
    *optional_capacity = hint.is_some();
    let mut full_history = None;
    let mut history_hint_used = false;
    let payload = capture_head_payload(drive, label, budget)?;
    let construct = payload.construct;
    let eod = payload.eod;
    let (header, predecessors, ip_generation, history_depth, history_truncated) =
        if construct.incremental {
            let (proof, view) = verify_incremental_replay(
                drive,
                label,
                construct,
                budget,
                &payload.replay,
                payload.head,
            )?;
            let depth = proof.predecessors.len() as u32;
            (view, proof.predecessors, proof.ip_generation, depth, false)
        } else {
            payload.replay.load_full(0, payload.head.highest_file_uid)?;
            *optional_capacity |= collect_history;
            let mut remaining = budget.clone();
            remaining.index_scratch.max_bytes = budget
                .index_scratch
                .max_bytes
                .saturating_sub(payload.replay.reserved_bytes());
            let candidate = match hint {
                Some(hint) if hint.manifest().matches(label, construct, eod) => Some(hint),
                Some(hint) => {
                    if hint.try_append_successor_candidate(
                        label,
                        construct,
                        eod,
                        payload.head.previous_location,
                    )? {
                        Some(hint)
                    } else {
                        None
                    }
                }
                None => None,
            };
            let hinted = if let Some(hint) = candidate {
                super::streamed::capture_full_recovery_from_hint(
                    drive,
                    label,
                    &remaining,
                    hint,
                    collect_history,
                )?
            } else {
                None
            };
            history_hint_used = hinted.is_some();
            let (proof, directory) = match hinted {
                Some(proof) => proof,
                None if reuse_captured_full => super::streamed::capture_full_recovery_from_payload(
                    drive,
                    label,
                    &remaining,
                    &payload,
                    collect_history,
                )?,
                None => super::streamed::capture_full_recovery_collect(
                    drive,
                    label,
                    &remaining,
                    &mut std::io::sink(),
                    collect_history,
                )?,
            };
            full_history = directory;
            if proof.construct != construct || proof.dp_eod != eod {
                return Err(TapeError::RecoveryRestricted {
                    reason: "Full 来源在捕获和核验之间改变".into(),
                });
            }
            (
                payload.head,
                Vec::new(),
                proof.ip_generation,
                proof.history_depth,
                proof.history_truncated,
            )
        };
    Ok(CapturedIndexSource {
        construct,
        header,
        replay: payload.replay,
        predecessors,
        source_bytes: payload.bytes,
        construct_end: eod,
        full_history,
        history_hint_used,
        scope: IndexSourceScope::Current {
            dp_eod: eod,
            ip_generation,
            history_depth,
            history_truncated,
        },
    })
}

/// DP-only capture for the dedicated IP-checkpoint precondition. This remains
/// Recorded, and cannot enter ordinary current observations or source intake.
/// The caller must independently match its end/head and complete mounted
/// history before granting the narrowly scoped IP repair.
pub(crate) fn capture_dp_checkpoint_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    let HeadPayload {
        construct,
        head,
        replay,
        bytes,
        eod,
    } = capture_head_payload(drive, label, budget)?;
    let (predecessors, header) = if construct.incremental {
        replay_captured_chain(drive, label, budget, &replay, head)?
    } else {
        replay.load_full(0, head.highest_file_uid)?;
        (Vec::new(), head)
    };
    Ok(CapturedIndexSource {
        construct,
        header,
        replay,
        predecessors,
        source_bytes: bytes,
        construct_end: eod,
        full_history: None,
        history_hint_used: false,
        scope: IndexSourceScope::Recorded,
    })
}

/// A complete current read view. This type cannot supply historical-source
/// identity or recovery/write evidence to the intake and reclaim interfaces.
pub(crate) struct CapturedReadIndex {
    construct: CommittedIndexConstruct,
    replay: IndexReplay,
    predecessors: Vec<CommittedIndexConstruct>,
}

impl CapturedReadIndex {
    pub(crate) fn construct(&self) -> CommittedIndexConstruct {
        self.construct
    }
    pub(crate) fn predecessors(&self) -> &[CommittedIndexConstruct] {
        &self.predecessors
    }
    pub(crate) fn entry_by_uid(&self, uid: u64) -> Result<Option<IndexEntry>> {
        self.replay.payload_by_uid(uid)
    }
}

/// Validate the latest complete DP construct, DP/IP consistency and every
/// required delta through its Full baseline. Independent older Full history
/// remains the explicit audit source's responsibility.
pub(crate) fn capture_current_read_index(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<CapturedReadIndex> {
    let HeadPayload {
        construct,
        head,
        replay,
        ..
    } = capture_head_payload(drive, label, budget)?;
    let predecessors = if head.incremental {
        verify_incremental_replay(drive, label, construct, budget, &replay, head)?
            .0
            .predecessors
    } else {
        replay.load_full(0, head.highest_file_uid)?;
        let mut remaining = budget.clone();
        remaining.index_scratch.max_bytes = budget
            .index_scratch
            .max_bytes
            .saturating_sub(replay.reserved_bytes());
        verify_dp_ip_head_with_evidence(drive, label, construct, &remaining)?.predecessors
    };
    Ok(CapturedReadIndex {
        construct,
        replay,
        predecessors,
    })
}

pub(super) struct HeadPayload {
    pub(super) construct: CommittedIndexConstruct,
    pub(super) head: LtfsIndex,
    pub(super) replay: IndexReplay,
    pub(super) bytes: u64,
    pub(super) eod: u64,
}

fn capture_head_payload(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<HeadPayload> {
    let (start, closing, eod) = super::streamed::clean_dp_head_position(drive)?;
    let replay = IndexReplay::capture(&budget.index_scratch, label.index_partition)?;
    drive.locate(1, start, true)?;
    let mut reader = ConstructReader::new(
        drive,
        start,
        label.blocksize as usize,
        budget.max_index_bytes,
    )?;
    let parsed = LtfsIndex::scan_any_reader_to_disk(&mut reader, |entry| replay.stage(0, entry));
    let head = match parsed {
        Ok(head) => head,
        Err(error) => return Err(reader.error.take().unwrap_or(error)),
    };
    if !reader.ended || reader.next.checked_sub(1) != Some(closing) || reader.next != eod {
        return Err(TapeError::RecoveryRestricted {
            reason: "捕获的 DP 构造没有紧邻 EOD".into(),
        });
    }
    validate_candidate(&head, label, label.data_partition, start)
        .map_err(|reason| TapeError::RecoveryRestricted { reason })?;
    let bytes = reader.bytes as u64;
    let construct = CommittedIndexConstruct {
        volume_uuid: head.volume_uuid,
        generation: head.generation,
        location: head.self_location,
        sha256: reader.sha.finalize().into(),
        incremental: head.incremental,
    };
    Ok(HeadPayload {
        construct,
        head,
        replay,
        bytes,
        eod,
    })
}

/// Capture an exact previously recorded source, which may precede the current
/// DP EOD and IP generation. The caller must independently validate the latest
/// mounted head/owner before content is released. Every predecessor must match
/// the supplied durable descriptor; discovery alone is never substituted.
pub fn capture_recorded_index_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    expected: &IndexSourceIdentity,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    let captured = capture_recorded_construct_source(drive, label, expected.head(), budget)?;
    if captured.predecessors != expected.predecessors() {
        return Err(TapeError::Catalog(
            "历史来源完整前驱链与持久证据不符".into(),
        ));
    }
    Ok(captured)
}

/// Capture an exact durable construct and discover its actual replay chain.
/// This grants no latest-head or owner authority; callers must validate both
/// independently before releasing content. A durable identity, when present,
/// must use `capture_recorded_index_source` to retain its exact-chain check.
pub(crate) fn capture_recorded_construct_source(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    target: CommittedIndexConstruct,
    budget: &RecoveryBudget,
) -> Result<CapturedIndexSource> {
    if target.volume_uuid != label.volume_uuid || target.location.partition != label.data_partition
    {
        return Err(TapeError::Catalog("历史来源与介质标签不符".into()));
    }
    let replay = IndexReplay::capture(&budget.index_scratch, label.index_partition)?;
    let mut source_bytes = 0;
    let (head, construct_end) = with_verified_dp_construct_bounds(
        drive,
        label,
        target.location,
        target.sha256,
        budget,
        false,
        |reader| {
            let mut counted = CountingRead {
                inner: reader,
                count: 0,
            };
            let parsed = LtfsIndex::scan_any_reader_to_disk(
                std::io::BufReader::new(&mut counted),
                |entry| replay.stage(0, entry),
            );
            source_bytes = counted.count;
            parsed
        },
    )?;
    validate_candidate(
        &head,
        label,
        label.data_partition,
        target.location.start_block,
    )
    .map_err(TapeError::Ltfs)?;
    if head.generation != target.generation || head.incremental != target.incremental {
        return Err(TapeError::Catalog("历史来源代数或种类不符".into()));
    }
    let (predecessors, header) = if target.incremental {
        replay_captured_chain(drive, label, budget, &replay, head)?
    } else {
        replay.load_full(0, head.highest_file_uid)?;
        (Vec::new(), head)
    };
    Ok(CapturedIndexSource {
        construct: target,
        header,
        replay,
        predecessors,
        source_bytes,
        construct_end,
        full_history: None,
        history_hint_used: false,
        scope: IndexSourceScope::Recorded,
    })
}

/// Revalidate every raw construct of an already recorded source. This does
/// not discover or admit new metadata; the current head and owner are checked
/// separately by the reader, including when an exact replay cache is warm.
pub(crate) fn verify_recorded_source_constructs(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    identity: &IndexSourceIdentity,
    budget: &RecoveryBudget,
) -> Result<()> {
    for construct in std::iter::once(identity.head()).chain(identity.predecessors().iter().copied())
    {
        if construct.volume_uuid != label.volume_uuid
            || construct.location.partition != label.data_partition
        {
            return Err(TapeError::Catalog("历史来源与介质标签不符".into()));
        }
        with_verified_dp_construct(
            drive,
            label,
            construct.location,
            construct.sha256,
            budget,
            false,
            |reader| {
                std::io::copy(reader, &mut std::io::sink())?;
                Ok(())
            },
        )?;
    }
    Ok(())
}

struct CountingRead<R> {
    inner: R,
    count: u64,
}
impl<R: std::io::Read> std::io::Read for CountingRead<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        self.count += n as u64;
        Ok(n)
    }
}
