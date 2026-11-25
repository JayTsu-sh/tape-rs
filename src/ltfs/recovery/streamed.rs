//! Read-only discovery and capture of a clean Full DP head. The writer and
//! callbacks remain provisional until the complete proof returns successfully.
use std::io::{BufReader, Read, Write};

use super::history::{FullHistoryDirectory, FullHistoryEntry, FullHistoryManifest, HistoryBuilder};
use super::source::HeadPayload;
use super::*;
use crate::ltfs::index::IndexEntry;
use crate::ltfs::index_equivalence::FullIndexComparison;
use crate::ltfs::volume::CommittedIndexConstruct;

/// Assign needs to prove emptiness, not construct a writable in-memory volume.
/// A Full entry can safely refuse early, without claiming a valid whole index.
/// Empty deltas require the existing disk replay and final object set instead.
/// All byte/event allocations and chain headers are bounded independently of
/// file count; the parser already caps XML nesting at 512.
pub(crate) fn probe_empty_volume(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<bool> {
    let mut budget = budget.clone();
    budget.max_index_bytes = budget.max_index_bytes.min(16 << 20);
    budget.max_chain_len = budget.max_chain_len.min(8);
    if label.blocksize as usize > budget.max_index_bytes {
        return Err(TapeError::LocalCapacity {
            reason: "非强制空卷检查的块大小超出内存预算".into(),
        });
    }
    drive.locate_eod(1)?;
    let eod = drive.read_position()?;
    if eod.partition != 1 {
        return Err(TapeError::RecoveryRestricted {
            reason: "空卷检查 DP EOD 分区不符".into(),
        });
    }
    let closing = back_fm(drive)?.ok_or_else(|| TapeError::RecoveryRestricted {
        reason: "空卷检查 DP 缺少结束文件标记".into(),
    })?;
    let opening = back_fm(drive)?.ok_or_else(|| TapeError::RecoveryRestricted {
        reason: "空卷检查 DP 缺少起始文件标记".into(),
    })?;
    let start = opening
        .checked_add(1)
        .ok_or_else(|| TapeError::Ltfs("DP 构造位置溢出".into()))?;
    if start < P1_DATA_START || start >= closing {
        return Err(TapeError::RecoveryRestricted {
            reason: "空卷检查 DP 末构造位置无效".into(),
        });
    }
    drive.locate(1, start, true)?;
    let mut reader = ConstructReader::new(
        drive,
        start,
        label.blocksize as usize,
        budget.max_index_bytes,
    )?;
    let mut found_object = false;
    let parsed = LtfsIndex::scan_any_reader_to_disk(&mut reader, |entry| {
        let nonempty_full = match entry {
            IndexEntry::File { node, .. } => node.delta_fields.is_none(),
            IndexEntry::Directory { path, node } => !path.is_empty() && node.delta_fields.is_none(),
        };
        if nonempty_full {
            found_object = true;
            return Err(TapeError::Refused {
                reason: "当前 Full 构造中发现文件或目录，未证明空卷".into(),
            });
        }
        Ok(())
    });
    let head = match parsed {
        Err(TapeError::Refused { .. }) if found_object && reader.error.is_none() => {
            return Ok(false);
        }
        Err(error) => return Err(reader.error.take().unwrap_or(error)),
        Ok(head) => head,
    };
    if !reader.ended
        || reader.next.checked_sub(1) != Some(closing)
        || reader.next != eod.block_number
    {
        return Err(TapeError::RecoveryRestricted {
            reason: "空卷检查 DP 构造未闭合或不紧邻 EOD".into(),
        });
    }
    validate_candidate(&head, label, label.data_partition, start)
        .map_err(|reason| TapeError::RecoveryRestricted { reason })?;
    let expected = CommittedIndexConstruct {
        volume_uuid: label.volume_uuid,
        generation: head.generation,
        location: head.self_location,
        sha256: reader.sha.finalize().into(),
        incremental: head.incremental,
    };
    let proof = verify_dp_ip_head_with_evidence(drive, label, expected, &budget)?;
    if proof.ip_generation != Some(expected.generation) || !proof.empty_unlocked {
        return Ok(false);
    }
    // The index proof uses P0's identity. Both physical labels must agree
    // before emptiness can authorize clearing the volume by FORMAT.
    for (partition, location) in [(0, label.index_partition), (1, label.data_partition)] {
        drive.locate(partition, crate::ltfs::volume::LABEL_BLOCK, true)?;
        let mut bytes = vec![0; 64 * 1024];
        let n = drive.read_block(&mut bytes)?;
        let mut actual = LtfsLabel::parse(bytes.get(..n).ok_or(TapeError::InvalidResponse {
            expected: bytes.len(),
            actual: n,
        })?)?;
        if actual.location != location || drive.read_block(&mut bytes)? != 0 {
            return Err(TapeError::RecoveryRestricted {
                reason: "空卷检查分区标签位置或闭合标记不符".into(),
            });
        }
        actual.location = label.location;
        if actual.to_xml()? != label.to_xml()? {
            return Err(TapeError::RecoveryRestricted {
                reason: "空卷检查两分区标签不一致".into(),
            });
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy)]
pub struct StreamedFullRecovery {
    pub construct: CommittedIndexConstruct,
    pub source_bytes: u64,
    pub dp_eod: u64,
    pub ip_generation: Option<u64>,
    pub history_depth: u32,
    /// Same bounded historical audit semantics as `verify_chain`. This is not
    /// a claim that every generation was audited, nor permission to write.
    pub history_truncated: bool,
}

struct CaptureReader<R, W> {
    input: R,
    output: W,
}

impl<R: Read, W: Write> Read for CaptureReader<R, W> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.input.read(bytes)?;
        self.output.write_all(&bytes[..n])?;
        Ok(n)
    }
}

struct StreamedConstruct {
    index: LtfsIndex,
    sha256: [u8; 32],
    next: u64,
    bytes: u64,
}

fn full_at(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    start: u64,
    budget: &RecoveryBudget,
    mut emit: impl FnMut(IndexEntry) -> Result<()>,
    output: Option<&mut dyn Write>,
    forward: bool,
) -> Result<StreamedConstruct> {
    if !forward {
        drive.locate(1, start, true)?;
    }
    let mut reader = ConstructReader::new(
        drive,
        start,
        label.blocksize as usize,
        budget.max_index_bytes,
    )?;
    if forward {
        drive.read_forward_to(1, start, &mut reader.buf)?;
    }
    let parsed = if let Some(output) = output {
        LtfsIndex::scan_full_reader(
            BufReader::new(CaptureReader {
                input: &mut reader,
                output,
            }),
            &mut emit,
        )
    } else {
        LtfsIndex::scan_full_reader(&mut reader, &mut emit)
    };
    let index = match parsed {
        Ok(index) => index,
        Err(error) => return Err(reader.error.take().unwrap_or(error)),
    };
    if !reader.ended {
        return Err(TapeError::RecoveryRestricted {
            reason: "流式 Full 构造缺少结束文件标记".into(),
        });
    }
    validate_candidate(&index, label, label.data_partition, start)
        .map_err(|reason| TapeError::RecoveryRestricted { reason })?;
    Ok(StreamedConstruct {
        index,
        sha256: reader.sha.finalize().into(),
        next: reader.next,
        bytes: reader.bytes as u64,
    })
}

pub(super) fn clean_dp_head_position(drive: &TapeDrive<'_>) -> Result<(u64, u64, u64)> {
    drive.locate_eod(1)?;
    let eod = drive.read_position()?;
    if eod.partition != 1 {
        return Err(TapeError::RecoveryRestricted {
            reason: "流式 DP EOD 分区不符".into(),
        });
    }
    let closing = back_fm(drive)?.ok_or_else(|| TapeError::RecoveryRestricted {
        reason: "DP 末尾缺少结束文件标记".into(),
    })?;
    let opening = back_fm(drive)?.ok_or_else(|| TapeError::RecoveryRestricted {
        reason: "DP 末尾缺少起始文件标记".into(),
    })?;
    let start = opening
        .checked_add(1)
        .ok_or_else(|| TapeError::Ltfs("DP 构造位置溢出".into()))?;
    if start < P1_DATA_START || start >= closing {
        return Err(TapeError::RecoveryRestricted {
            reason: "DP 末构造位置无效".into(),
        });
    }
    Ok((start, closing, eod.block_number))
}

/// Capture the exact latest clean Full DP XML while scanning child-free nodes
/// into disposable SQLite, then verify its bounded Full history and DP/IP view.
/// Memory depends on one node/XML event, directory depth, and fixed DB buffers,
/// rather than the number of files. Large individual attributes still matter.
///
/// Caller must serialize device use and enforce current owner/PR before and
/// after every command. Output is provisional until success: never publish a
/// partial file. This returns read evidence, not writable recovery, a general
/// T1/T2 repair verdict, or an incremental-chain bootstrap. An incremental head
/// or predecessor is explicitly rejected; the FDB reader has a separate exact
/// recorded-source replay path. No on-tape state is changed here.
pub fn capture_full_recovery(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    output: &mut dyn Write,
) -> Result<StreamedFullRecovery> {
    capture_full_recovery_collect(drive, label, budget, output, false).map(|(proof, _)| proof)
}

pub(super) fn capture_full_recovery_collect(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    output: &mut dyn Write,
    collect_history: bool,
) -> Result<(StreamedFullRecovery, Option<FullHistoryDirectory>)> {
    capture_full_recovery_from_drive(drive, label, budget, output, collect_history, false)
}

pub(super) fn verify_complete_full_recovery(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
) -> Result<(StreamedFullRecovery, FullHistoryManifest)> {
    let complete = budget.clone().with_complete_full_history();
    let (proof, directory) = capture_full_recovery_from_drive(
        drive,
        label,
        &complete,
        &mut std::io::sink(),
        true,
        true,
    )?;
    let Some(directory) = directory.filter(|_| !proof.history_truncated) else {
        return Err(TapeError::RecoveryRestricted {
            reason: "回收组尾捕获缺少完整 Full 历史".into(),
        });
    };
    Ok((proof, directory.manifest()))
}

fn capture_full_recovery_from_drive(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    output: &mut dyn Write,
    collect_history: bool,
    require_stable_eod: bool,
) -> Result<(StreamedFullRecovery, Option<FullHistoryDirectory>)> {
    let (start, closing, eod) = clean_dp_head_position(drive)?;
    let storage = FullAuditStorage::new(budget, collect_history)?;
    let head = full_at(
        drive,
        label,
        start,
        budget,
        |entry| storage.comparison().record(false, entry),
        Some(output),
        false,
    )?;
    if head.next.checked_sub(1) != Some(closing) || head.next != eod {
        return Err(TapeError::RecoveryRestricted {
            reason: "DP Full 构造未紧邻数据分区 EOD".into(),
        });
    }
    audit_full_history(drive, label, budget, storage, head, eod, require_stable_eod)
}

pub(super) fn capture_full_recovery_from_payload(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    payload: &HeadPayload,
    collect_history: bool,
) -> Result<(StreamedFullRecovery, Option<FullHistoryDirectory>)> {
    let storage = FullAuditStorage::new(budget, collect_history)?;
    // The Full replay holds the same streamed child-free entries from the
    // verified current construct, without any delta application.
    payload
        .replay
        .for_each_payload(|entry| storage.comparison().record(false, entry))?;
    let head = StreamedConstruct {
        index: payload.head.clone(),
        sha256: payload.construct.sha256,
        next: payload.eod,
        bytes: payload.bytes,
    };
    audit_full_history(drive, label, budget, storage, head, payload.eod, true)
}

fn audit_full_history(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    storage: FullAuditStorage,
    mut head: StreamedConstruct,
    eod: u64,
    require_stable_eod: bool,
) -> Result<(StreamedFullRecovery, Option<FullHistoryDirectory>)> {
    let comparison = storage.comparison();
    comparison.validate_highest_uid(false, head.index.highest_file_uid)?;
    storage.record(&head)?;
    let construct = CommittedIndexConstruct {
        volume_uuid: label.volume_uuid,
        generation: head.index.generation,
        location: head.index.self_location,
        sha256: head.sha256,
        incremental: false,
    };
    let source_bytes = head.bytes;
    let mut depth = 0;
    let history_limit = budget.full_history_limit(head.index.self_location.start_block);
    let mut truncated = false;
    while let Some(previous) = head.index.previous_location {
        if previous.partition != label.data_partition {
            break;
        }
        if depth >= history_limit {
            truncated = true;
            break;
        }
        if previous.start_block >= head.index.self_location.start_block {
            return Err(TapeError::RecoveryRestricted {
                reason: "Full 历史回指未向前".into(),
            });
        }
        let prior = full_at(
            drive,
            label,
            previous.start_block,
            budget,
            |entry| comparison.record(true, entry),
            None,
            false,
        )?;
        comparison.validate_highest_uid(true, prior.index.highest_file_uid)?;
        storage.record(&prior)?;
        if prior.index.generation > head.index.generation {
            return Err(TapeError::RecoveryRestricted {
                reason: "Full 历史代数倒置".into(),
            });
        }
        if prior.index.generation == head.index.generation {
            if !same_full_view(&head.index, &prior.index) {
                return Err(TapeError::RecoveryRestricted {
                    reason: "同代历史 Full 逻辑头不一致".into(),
                });
            }
            comparison.require_equal()?;
        }
        comparison.advance_history()?;
        head = prior;
        depth += 1;
    }
    let directory = storage.finish(label, construct, eod, !truncated)?;
    let mut ip_budget = budget.clone();
    ip_budget.index_scratch.max_bytes = budget.index_scratch.max_bytes.saturating_sub(
        directory
            .as_ref()
            .map_or(0, FullHistoryDirectory::reserved_bytes),
    );
    let dp_ip = verify_dp_ip_head_with_evidence(drive, label, construct, &ip_budget)?;
    if require_stable_eod && dp_ip.full_dp_eod != Some(eod) {
        return Err(TapeError::RecoveryRestricted {
            reason: "Full 历史核验期间 DP 构造结束位置或 EOD 改变".into(),
        });
    }
    Ok((
        StreamedFullRecovery {
            construct,
            source_bytes,
            dp_eod: eod,
            ip_generation: dp_ip.ip_generation,
            history_depth: depth,
            history_truncated: truncated,
        },
        directory,
    ))
}

pub(super) fn capture_full_recovery_from_hint(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: &FullHistoryDirectory,
    collect_history: bool,
) -> Result<Option<(StreamedFullRecovery, Option<FullHistoryDirectory>)>> {
    read_full_recovery_from_schedule(drive, label, budget, hint, collect_history, true)
}

/// The schedule came from the exact earlier physical capture. A mismatch
/// invalidates the readback, even if a newly discovered head would be usable.
pub(super) fn verify_scheduled_full_recovery(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    schedule: &FullHistoryDirectory,
) -> Result<StreamedFullRecovery> {
    read_full_recovery_from_schedule(drive, label, budget, schedule, false, true)?
        .map(|(proof, _)| proof)
        .ok_or_else(|| TapeError::RecoveryRestricted {
            reason: "回收组正文读后实际卷头、历史或EOD/closing改变".into(),
        })
}

/// None means the schedule disagreed with valid, freshly read facts.
/// Parsing, media, guard and local IO errors are preserved.
fn read_full_recovery_from_schedule(
    drive: &TapeDrive<'_>,
    label: &LtfsLabel,
    budget: &RecoveryBudget,
    hint: &FullHistoryDirectory,
    collect_history: bool,
    require_stable_eod: bool,
) -> Result<Option<(StreamedFullRecovery, Option<FullHistoryDirectory>)>> {
    let manifest = hint.manifest();
    let (start, closing, eod) = clean_dp_head_position(drive)?;
    if manifest.dp_eod != eod || manifest.head.location.start_block != start {
        return Ok(None);
    }
    let storage = FullAuditStorage::new(budget, collect_history)?;
    let comparison = storage.comparison();
    let mut prior: Option<StreamedConstruct> = None;
    let mut count = 0u64;
    let mut sha = Sha256::new();
    let mut mismatch = false;
    let read = hint.for_each_entry(|expected| {
        let successor = prior.is_some();
        let actual = full_at(
            drive,
            label,
            expected.start_block,
            budget,
            |entry| comparison.record(successor, entry),
            None,
            true,
        )?;
        comparison.validate_highest_uid(successor, actual.index.highest_file_uid)?;
        let observed = FullHistoryEntry {
            generation: actual.index.generation,
            start_block: actual.index.self_location.start_block,
            sha256: actual.sha256,
            end_block: actual.next,
            previous: actual.index.previous_location,
        };
        if let Some(prior) = &prior {
            if actual.index.previous_location != Some(prior.index.self_location)
                || actual.index.self_location.start_block <= prior.index.self_location.start_block
                || actual.index.generation < prior.index.generation
            {
                mismatch = true;
                return Err(TapeError::Refused {
                    reason: "Full 历史调度提示未覆盖真实相邻回指，重新发现".into(),
                });
            }
            if actual.index.generation == prior.index.generation {
                if !same_full_view(&actual.index, &prior.index) {
                    return Err(TapeError::RecoveryRestricted {
                        reason: "同代历史 Full 逻辑头不一致".into(),
                    });
                }
                comparison.require_equal()?;
            }
            // The just-read successor becomes the left-hand comparison view.
            // Logical equality is symmetric; every adjacent same-generation
            // pair gets the exact original full-node comparison.
            comparison.advance_history()?;
        } else if actual
            .index
            .previous_location
            .is_some_and(|p| p.partition == label.data_partition)
        {
            mismatch = true;
            return Err(TapeError::Refused {
                reason: "Full 历史调度提示遗漏最早前驱，重新发现".into(),
            });
        }
        // A valid construct that disagrees is a schedule mismatch. The
        // caller decides whether this permits discovery or rejects readback.
        if observed != expected {
            mismatch = true;
            return Err(TapeError::Refused {
                reason: "Full 历史调度提示与实读构造不符，重新发现".into(),
            });
        }
        storage.record(&actual)?;
        sha.update(observed.to_bytes());
        count = count
            .checked_add(1)
            .ok_or_else(|| TapeError::RecoveryRestricted {
                reason: "完整 Full 历史计数溢出".into(),
            })?;
        prior = Some(actual);
        Ok(())
    });
    if let Err(error) = read {
        return if mismatch { Ok(None) } else { Err(error) };
    }
    let Some(head) = prior else {
        return Ok(None);
    };
    let construct = CommittedIndexConstruct {
        volume_uuid: label.volume_uuid,
        generation: head.index.generation,
        location: head.index.self_location,
        sha256: head.sha256,
        incremental: false,
    };
    if count != manifest.entries
        || <[u8; 32]>::from(sha.finalize()) != manifest.entries_sha256
        || construct != manifest.head
        || head.next != eod
        || head.next.checked_sub(1) != Some(closing)
    {
        return Ok(None);
    }
    let depth = u32::try_from(count - 1).map_err(|_| TapeError::RecoveryRestricted {
        reason: "完整 Full 历史超出可计数深度".into(),
    })?;
    let source_bytes = head.bytes;
    let directory = storage.finish(label, construct, eod, true)?;
    let mut ip_budget = budget.clone();
    ip_budget.index_scratch.max_bytes = budget.index_scratch.max_bytes.saturating_sub(
        directory
            .as_ref()
            .map_or(0, FullHistoryDirectory::reserved_bytes),
    );
    let dp_ip = verify_dp_ip_head_with_evidence(drive, label, construct, &ip_budget)?;
    if require_stable_eod && dp_ip.full_dp_eod != Some(eod) {
        return Err(TapeError::RecoveryRestricted {
            reason: "Full 历史核验期间 DP 构造结束位置或 EOD 改变".into(),
        });
    }
    Ok(Some((
        StreamedFullRecovery {
            construct,
            source_bytes,
            dp_eod: eod,
            ip_generation: dp_ip.ip_generation,
            history_depth: depth,
            history_truncated: false,
        },
        directory,
    )))
}

enum FullAuditStorage {
    Comparison(FullIndexComparison),
    History(HistoryBuilder),
}
impl FullAuditStorage {
    fn new(budget: &RecoveryBudget, collect_history: bool) -> Result<Self> {
        if collect_history {
            Ok(Self::History(HistoryBuilder::new(&budget.index_scratch)?))
        } else {
            Ok(Self::Comparison(FullIndexComparison::new(
                &budget.index_scratch,
            )?))
        }
    }

    fn comparison(&self) -> &FullIndexComparison {
        match self {
            Self::Comparison(comparison) => comparison,
            Self::History(builder) => &builder.comparison,
        }
    }
    fn record(&self, construct: &StreamedConstruct) -> Result<()> {
        if let Self::History(builder) = self {
            builder.record(FullHistoryEntry {
                generation: construct.index.generation,
                start_block: construct.index.self_location.start_block,
                sha256: construct.sha256,
                end_block: construct.next,
                previous: construct.index.previous_location,
            })?;
        }
        Ok(())
    }
    fn finish(
        self,
        label: &LtfsLabel,
        head: CommittedIndexConstruct,
        eod: u64,
        complete: bool,
    ) -> Result<Option<FullHistoryDirectory>> {
        match self {
            Self::History(builder) if complete => builder.finish(label, head, eod).map(Some),
            _ => Ok(None),
        }
    }
}
