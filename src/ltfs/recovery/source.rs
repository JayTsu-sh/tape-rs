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

impl CapturedIndexSource {
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
}

/// Discover a clean latest DP head and retain complete replayed node payloads
/// on bounded local disk. No index XML, file tree or path set grows in memory.
/// This does not format, repair, checkpoint, write MAM or authorize appends.
pub fn capture_index_source(
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
    let (header, predecessors, ip_generation, history_depth, history_truncated) =
        if head.incremental {
            let (proof, view) =
                verify_incremental_replay(drive, label, construct, budget, &replay, head)?;
            let depth = proof.predecessors.len() as u32;
            (view, proof.predecessors, proof.ip_generation, depth, false)
        } else {
            replay.load_full(0, head.highest_file_uid)?;
            let mut remaining = budget.clone();
            remaining.index_scratch.max_bytes = budget
                .index_scratch
                .max_bytes
                .saturating_sub(replay.reserved_bytes());
            let proof = capture_full_recovery(drive, label, &remaining, &mut std::io::sink())?;
            if proof.construct != construct || proof.dp_eod != eod {
                return Err(TapeError::RecoveryRestricted {
                    reason: "Full 来源在捕获和核验之间改变".into(),
                });
            }
            (
                head,
                Vec::new(),
                proof.ip_generation,
                proof.history_depth,
                proof.history_truncated,
            )
        };
    Ok(CapturedIndexSource {
        construct,
        header,
        replay,
        predecessors,
        source_bytes: bytes,
        construct_end: eod,
        scope: IndexSourceScope::Current {
            dp_eod: eod,
            ip_generation,
            history_depth,
            history_truncated,
        },
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

struct HeadPayload {
    construct: CommittedIndexConstruct,
    head: LtfsIndex,
    replay: IndexReplay,
    bytes: u64,
    eod: u64,
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
    let target = expected.head();
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
    if predecessors != expected.predecessors() {
        return Err(TapeError::Catalog(
            "历史来源完整前驱链与持久证据不符".into(),
        ));
    }
    Ok(CapturedIndexSource {
        construct: target,
        header,
        replay,
        predecessors,
        source_bytes,
        construct_end,
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
