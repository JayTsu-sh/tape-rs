//! Read-only discovery and capture of a clean Full DP head. The writer and
//! callbacks remain provisional until the complete proof returns successfully.
use std::io::{BufReader, Read, Write};

use super::*;
use crate::ltfs::index::IndexEntry;
use crate::ltfs::index_equivalence::FullIndexComparison;
use crate::ltfs::volume::CommittedIndexConstruct;

#[derive(Debug, Clone, Copy)]
pub struct StreamedFullRecovery {
    pub construct: CommittedIndexConstruct,
    pub source_bytes: u64,
    pub dp_eod: u64,
    pub ip_generation: u64,
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
) -> Result<StreamedConstruct> {
    drive.locate(1, start, true)?;
    let mut reader = ConstructReader::new(
        drive,
        start,
        label.blocksize as usize,
        budget.max_index_bytes,
    );
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
    let comparison = FullIndexComparison::new(&budget.index_scratch)?;
    let mut head = full_at(
        drive,
        label,
        start,
        budget,
        |entry| comparison.record(false, entry),
        Some(output),
    )?;
    if head.next.checked_sub(1) != Some(closing) || head.next != eod.block_number {
        return Err(TapeError::RecoveryRestricted {
            reason: "DP Full 构造未紧邻数据分区 EOD".into(),
        });
    }
    comparison.validate_highest_uid(false, head.index.highest_file_uid)?;
    let construct = CommittedIndexConstruct {
        volume_uuid: label.volume_uuid,
        generation: head.index.generation,
        location: head.index.self_location,
        sha256: head.sha256,
        incremental: false,
    };
    let source_bytes = head.bytes;
    let mut depth = 0;
    let mut truncated = false;
    while let Some(previous) = head.index.previous_location {
        if previous.partition != label.data_partition {
            break;
        }
        if depth >= budget.max_chain_len {
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
        )?;
        comparison.validate_highest_uid(true, prior.index.highest_file_uid)?;
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
    drop(comparison);
    let ip_generation = verify_dp_ip_head(drive, label, construct, budget)?;
    Ok(StreamedFullRecovery {
        construct,
        source_bytes,
        dp_eod: eod.block_number,
        ip_generation,
        history_depth: depth,
        history_truncated: truncated,
    })
}
