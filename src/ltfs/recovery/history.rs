//! Disposable locations for a completely verified Full history. Persisted
//! forms are only scheduling hints: every construct must be read again.

use std::io::{Read, Write};

use rusqlite::params;
use sha2::{Digest, Sha256};

use super::{IndexLocation, LtfsLabel, P1_DATA_START};
use crate::error::{Result, TapeError};
use crate::ltfs::index_equivalence::FullIndexComparison;
use crate::ltfs::index_scratch::{IndexScratchBudget, ScratchDatabase};
use crate::ltfs::volume::CommittedIndexConstruct;

pub const FULL_HISTORY_ENTRY_BYTES: usize = 69;
pub const FULL_HISTORY_MANIFEST_BYTES: usize = 127;

fn invalid() -> TapeError {
    TapeError::RecoveryRestricted {
        reason: "完整 Full 历史位置提示不完整或绑定不符".into(),
    }
}

/// An address hint, never a substitute for the actual XML/filemark/hash read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FullHistoryEntry {
    pub generation: u64,
    pub start_block: u64,
    pub sha256: [u8; 32],
    pub end_block: u64,
    pub previous: Option<IndexLocation>,
}

impl FullHistoryEntry {
    pub fn to_bytes(self) -> [u8; FULL_HISTORY_ENTRY_BYTES] {
        let mut out = [0; FULL_HISTORY_ENTRY_BYTES];
        out[..8].copy_from_slice(&self.generation.to_be_bytes());
        out[8..16].copy_from_slice(&self.start_block.to_be_bytes());
        out[16..48].copy_from_slice(&self.sha256);
        out[48..56].copy_from_slice(&self.end_block.to_be_bytes());
        if let Some(previous) = self.previous {
            out[56] = 1;
            out[57..61].copy_from_slice(&(previous.partition as u32).to_be_bytes());
            out[61..69].copy_from_slice(&previous.start_block.to_be_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != FULL_HISTORY_ENTRY_BYTES {
            return Err(invalid());
        }
        let previous = match bytes[56] {
            0 if bytes[57..].iter().all(|v| *v == 0) => None,
            1 => Some(IndexLocation {
                partition: char::from_u32(u32::from_be_bytes(bytes[57..61].try_into().unwrap()))
                    .ok_or_else(invalid)?,
                start_block: u64::from_be_bytes(bytes[61..69].try_into().unwrap()),
            }),
            _ => return Err(invalid()),
        };
        let entry = Self {
            generation: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            start_block: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
            sha256: bytes[16..48].try_into().unwrap(),
            end_block: u64::from_be_bytes(bytes[48..56].try_into().unwrap()),
            previous,
        };
        if entry.generation == 0
            || entry.start_block < P1_DATA_START
            || entry
                .start_block
                .checked_add(2)
                .is_none_or(|n| n > entry.end_block)
        {
            return Err(invalid());
        }
        Ok(entry)
    }
}

/// Complete directory scope and immutable content identity. This metadata
/// supplies locations only; matching it never authorizes publication or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FullHistoryManifest {
    pub head: CommittedIndexConstruct,
    pub dp_eod: u64,
    pub entries: u64,
    pub block_size: u32,
    pub index_partition: char,
    pub data_partition: char,
    pub entries_sha256: [u8; 32],
}

impl FullHistoryManifest {
    pub fn to_bytes(self) -> [u8; FULL_HISTORY_MANIFEST_BYTES] {
        let mut out = [0; FULL_HISTORY_MANIFEST_BYTES];
        out[..3].copy_from_slice(b"FH1");
        out[3..19].copy_from_slice(self.head.volume_uuid.as_bytes());
        out[19..27].copy_from_slice(&self.head.generation.to_be_bytes());
        out[27..35].copy_from_slice(&self.head.location.start_block.to_be_bytes());
        out[35..67].copy_from_slice(&self.head.sha256);
        out[67..75].copy_from_slice(&self.dp_eod.to_be_bytes());
        out[75..83].copy_from_slice(&self.entries.to_be_bytes());
        out[83..87].copy_from_slice(&self.block_size.to_be_bytes());
        out[87..91].copy_from_slice(&(self.index_partition as u32).to_be_bytes());
        out[91..95].copy_from_slice(&(self.data_partition as u32).to_be_bytes());
        out[95..127].copy_from_slice(&self.entries_sha256);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != FULL_HISTORY_MANIFEST_BYTES || &bytes[..3] != b"FH1" {
            return Err(invalid());
        }
        let data_partition = char::from_u32(u32::from_be_bytes(bytes[91..95].try_into().unwrap()))
            .ok_or_else(invalid)?;
        let manifest = Self {
            head: CommittedIndexConstruct {
                volume_uuid: uuid::Uuid::from_slice(&bytes[3..19]).map_err(|_| invalid())?,
                generation: u64::from_be_bytes(bytes[19..27].try_into().unwrap()),
                location: IndexLocation {
                    partition: data_partition,
                    start_block: u64::from_be_bytes(bytes[27..35].try_into().unwrap()),
                },
                sha256: bytes[35..67].try_into().unwrap(),
                incremental: false,
            },
            dp_eod: u64::from_be_bytes(bytes[67..75].try_into().unwrap()),
            entries: u64::from_be_bytes(bytes[75..83].try_into().unwrap()),
            block_size: u32::from_be_bytes(bytes[83..87].try_into().unwrap()),
            index_partition: char::from_u32(u32::from_be_bytes(bytes[87..91].try_into().unwrap()))
                .ok_or_else(invalid)?,
            data_partition,
            entries_sha256: bytes[95..127].try_into().unwrap(),
        };
        if manifest.head.generation == 0
            || manifest.head.location.start_block < P1_DATA_START
            || manifest
                .head
                .location
                .start_block
                .checked_add(2)
                .is_none_or(|n| n > manifest.dp_eod)
            || manifest.entries == 0
            || manifest.entries > u64::from(u32::MAX) + 1
            || manifest.entries > manifest.head.location.start_block
            || manifest.block_size == 0
            || manifest.block_size as usize > crate::tape::commands::SCSI_6_TRANSFER_MAX
            || manifest.index_partition == manifest.data_partition
        {
            return Err(invalid());
        }
        Ok(manifest)
    }

    pub fn fingerprint(self) -> [u8; 32] {
        Sha256::digest(self.to_bytes()).into()
    }

    pub(crate) fn matches(
        self,
        label: &LtfsLabel,
        head: CommittedIndexConstruct,
        eod: u64,
    ) -> bool {
        self.head == head
            && self.dp_eod == eod
            && self.block_size == label.blocksize
            && self.index_partition == label.index_partition
            && self.data_partition == label.data_partition
    }
}

/// One private, bounded SQLite directory. No history-length Vec is allocated.
/// A deserialized directory remains untrusted until every real construct and
/// its predecessor relationship pass the complete current capture again.
pub struct FullHistoryDirectory {
    db: ScratchDatabase,
    manifest: FullHistoryManifest,
    // One newly read head may extend an untrusted scheduling candidate. Keep
    // it outside SQLite so admission cannot grow the input reservation behind
    // the capture's already-deducted scratch budget. Actual successful capture
    // emits a new directory whose complete rows have no overlay.
    successor: Option<FullHistoryEntry>,
}

impl FullHistoryDirectory {
    pub(crate) fn try_append_successor_candidate(
        &mut self,
        label: &LtfsLabel,
        head: CommittedIndexConstruct,
        end: u64,
        previous: Option<IndexLocation>,
    ) -> Result<bool> {
        let old = self.manifest;
        let Some(entries) = old.entries.checked_add(1) else {
            return Ok(false);
        };
        if self.successor.is_some()
            || head.incremental
            || old.head.volume_uuid != head.volume_uuid
            || head.volume_uuid != label.volume_uuid
            || old.block_size != label.blocksize
            || old.index_partition != label.index_partition
            || old.data_partition != label.data_partition
            || head.location.partition != label.data_partition
            || head.generation < old.head.generation
            || head.location.start_block <= old.head.location.start_block
            || previous != Some(old.head.location)
            || old.dp_eod > head.location.start_block
            || head
                .location
                .start_block
                .checked_add(2)
                .is_none_or(|n| n > end)
            || entries > u64::from(u32::MAX) + 1
        {
            return Ok(false);
        }
        // Recheck the bytes actually consumed from the old private directory;
        // a stored digest is not an authority to skip either SQL or tape data.
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut last = None;
        self.for_each_entry(|entry| {
            hash.update(entry.to_bytes());
            count += 1;
            last = Some(entry);
            Ok(())
        })?;
        if count != old.entries
            || <[u8; 32]>::from(hash.clone().finalize()) != old.entries_sha256
            || last.is_none_or(|entry| {
                entry.start_block != old.head.location.start_block
                    || entry.generation != old.head.generation
                    || entry.sha256 != old.head.sha256
                    || entry.end_block != old.dp_eod
            })
        {
            return Ok(false);
        }
        let successor = FullHistoryEntry {
            generation: head.generation,
            start_block: head.location.start_block,
            sha256: head.sha256,
            end_block: end,
            previous,
        };
        hash.update(successor.to_bytes());
        self.successor = Some(successor);
        self.manifest = FullHistoryManifest {
            head,
            dp_eod: end,
            entries,
            entries_sha256: hash.finalize().into(),
            ..old
        };
        Ok(true)
    }

    pub fn manifest(&self) -> FullHistoryManifest {
        self.manifest
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.db.reserved_bytes()
    }

    pub(crate) fn entry_page(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<FullHistoryEntry>> {
        if !(1..=1536).contains(&limit) {
            return Err(invalid());
        }
        let after_block = after;
        let after = after
            .map(|block| block.to_be_bytes().to_vec())
            .unwrap_or_default();
        let mut statement = self
            .db
            .prepare("SELECT entry FROM history WHERE start > ?1 ORDER BY start LIMIT ?2")?;
        let mut rows = statement.query(params![after, limit])?;
        let mut entries = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            entries.push(FullHistoryEntry::from_bytes(&bytes)?);
        }
        if entries.len() < limit
            && let Some(successor) = self.successor
            && after_block.is_none_or(|after| successor.start_block > after)
        {
            entries.push(successor);
        }
        Ok(entries)
    }

    pub fn for_each_entry(
        &self,
        mut emit: impl FnMut(FullHistoryEntry) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self
            .db
            .prepare("SELECT entry FROM history ORDER BY start")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            emit(FullHistoryEntry::from_bytes(&bytes)?)?;
        }
        if let Some(successor) = self.successor {
            emit(successor)?;
        }
        Ok(())
    }

    pub fn write_to(&self, output: &mut dyn Write) -> Result<()> {
        output.write_all(&self.manifest.to_bytes())?;
        self.for_each_entry(|entry| {
            output.write_all(&entry.to_bytes())?;
            Ok(())
        })
    }

    pub fn read_from(input: &mut dyn Read, budget: &IndexScratchBudget) -> Result<Self> {
        let mut header = [0; FULL_HISTORY_MANIFEST_BYTES];
        input.read_exact(&mut header)?;
        let manifest = FullHistoryManifest::from_bytes(&header)?;
        let builder = HistoryBuilder::new(budget)?;
        for _ in 0..manifest.entries {
            let mut bytes = [0; FULL_HISTORY_ENTRY_BYTES];
            input.read_exact(&mut bytes)?;
            builder.record(FullHistoryEntry::from_bytes(&bytes)?)?;
        }
        if input.read(&mut [0])? != 0 {
            return Err(invalid());
        }
        builder.finish_manifest(manifest)
    }
}

/// Bounded loader for untrusted immutable FDB chunks. Finishing validates only
/// the hint encoding and completeness, never the physical source or ownership.
pub(crate) struct FullHistoryHintBuilder {
    builder: HistoryBuilder,
    manifest: FullHistoryManifest,
    received: u64,
}

impl FullHistoryHintBuilder {
    pub(crate) fn new(manifest: FullHistoryManifest, budget: &IndexScratchBudget) -> Result<Self> {
        FullHistoryManifest::from_bytes(&manifest.to_bytes())?;
        Ok(Self {
            builder: HistoryBuilder::new(budget)?,
            manifest,
            received: 0,
        })
    }

    pub(crate) fn append_entries(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty()
            || bytes.len() > 1536 * FULL_HISTORY_ENTRY_BYTES
            || !bytes.len().is_multiple_of(FULL_HISTORY_ENTRY_BYTES)
        {
            return Err(invalid());
        }
        for bytes in bytes.chunks_exact(FULL_HISTORY_ENTRY_BYTES) {
            if self.received >= self.manifest.entries {
                return Err(invalid());
            }
            self.builder.record(FullHistoryEntry::from_bytes(bytes)?)?;
            self.received += 1;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<FullHistoryDirectory> {
        if self.received != self.manifest.entries {
            return Err(invalid());
        }
        self.builder.finish_manifest(self.manifest)
    }
}

pub(super) struct HistoryBuilder {
    pub(super) comparison: FullIndexComparison,
}

impl HistoryBuilder {
    pub(super) fn new(budget: &IndexScratchBudget) -> Result<Self> {
        let comparison = FullIndexComparison::new(budget)?;
        comparison.database().execute_batch(
            "CREATE TABLE history(start BLOB PRIMARY KEY, entry BLOB NOT NULL) WITHOUT ROWID;",
        )?;
        Ok(Self { comparison })
    }

    pub(super) fn record(&self, entry: FullHistoryEntry) -> Result<()> {
        let bytes = entry.to_bytes();
        FullHistoryEntry::from_bytes(&bytes)?;
        self.comparison.database().ensure_room(bytes.len() + 8)?;
        self.comparison
            .database()
            .execute(
                "INSERT INTO history(start,entry) VALUES(?1,?2)",
                params![entry.start_block.to_be_bytes().as_slice(), bytes.as_slice()],
            )
            .map_err(crate::ltfs::index_scratch::index_row_error)?;
        Ok(())
    }

    pub(super) fn finish(
        self,
        label: &LtfsLabel,
        head: CommittedIndexConstruct,
        dp_eod: u64,
    ) -> Result<FullHistoryDirectory> {
        let entries =
            self.comparison
                .database()
                .query_row("SELECT count(*) FROM history", [], |row| row.get(0))?;
        let mut sha = Sha256::new();
        let mut statement = self
            .comparison
            .database()
            .prepare("SELECT entry FROM history ORDER BY start")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            sha.update(&bytes);
        }
        drop(rows);
        drop(statement);
        self.finish_manifest(FullHistoryManifest {
            head,
            dp_eod,
            entries,
            block_size: label.blocksize,
            index_partition: label.index_partition,
            data_partition: label.data_partition,
            entries_sha256: sha.finalize().into(),
        })
    }

    fn finish_manifest(self, manifest: FullHistoryManifest) -> Result<FullHistoryDirectory> {
        FullHistoryManifest::from_bytes(&manifest.to_bytes())?;
        self.comparison
            .database()
            .execute_batch("DROP TABLE dp; DROP TABLE ip;")?;
        let directory = FullHistoryDirectory {
            db: self.comparison.into_database(),
            manifest,
            successor: None,
        };
        let mut previous: Option<FullHistoryEntry> = None;
        let mut count = 0u64;
        let mut sha = Sha256::new();
        directory.for_each_entry(|entry| {
            sha.update(entry.to_bytes());
            if let Some(prior) = previous {
                if entry.previous
                    != Some(IndexLocation {
                        partition: manifest.data_partition,
                        start_block: prior.start_block,
                    })
                    || entry.generation < prior.generation
                    || prior.end_block > entry.start_block
                {
                    return Err(invalid());
                }
            } else if entry
                .previous
                .is_some_and(|p| p.partition == manifest.data_partition)
            {
                return Err(invalid());
            }
            count = count.checked_add(1).ok_or_else(invalid)?;
            previous = Some(entry);
            Ok(())
        })?;
        let last = previous.ok_or_else(invalid)?;
        if count != manifest.entries
            || <[u8; 32]>::from(sha.finalize()) != manifest.entries_sha256
            || last.start_block != manifest.head.location.start_block
            || last.generation != manifest.head.generation
            || last.sha256 != manifest.head.sha256
            || last.end_block != manifest.dp_eod
        {
            return Err(invalid());
        }
        Ok(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label() -> LtfsLabel {
        LtfsLabel {
            version: "2.5.1".into(),
            creator: "history-test".into(),
            format_time: "2026-11-16T00:00:00.000000000Z".into(),
            volume_uuid: uuid::Uuid::new_v4(),
            location: 'a',
            index_partition: 'a',
            data_partition: 'b',
            blocksize: 64 << 10,
            compression: false,
        }
    }

    fn budget(bytes: u64) -> IndexScratchBudget {
        IndexScratchBudget {
            max_bytes: bytes,
            headroom_bytes: 0,
            ..Default::default()
        }
    }

    fn directory() -> FullHistoryDirectory {
        let label = label();
        let builder = HistoryBuilder::new(&budget(1 << 20)).unwrap();
        for (generation, start_block, previous) in [
            (
                9,
                5,
                Some(IndexLocation {
                    partition: 'a',
                    start_block: 3,
                }),
            ),
            (
                9,
                8,
                Some(IndexLocation {
                    partition: 'b',
                    start_block: 5,
                }),
            ),
            (
                10,
                11,
                Some(IndexLocation {
                    partition: 'b',
                    start_block: 8,
                }),
            ),
        ] {
            builder
                .record(FullHistoryEntry {
                    generation,
                    start_block,
                    sha256: [start_block as u8; 32],
                    end_block: start_block + 2,
                    previous,
                })
                .unwrap();
        }
        builder
            .finish(
                &label,
                CommittedIndexConstruct {
                    volume_uuid: label.volume_uuid,
                    generation: 10,
                    location: IndexLocation {
                        partition: 'b',
                        start_block: 11,
                    },
                    sha256: [11; 32],
                    incremental: false,
                },
                13,
            )
            .unwrap()
    }

    #[test]
    fn directory_roundtrip_keeps_same_generation_and_cross_partition_termination() {
        let directory = directory();
        let mut bytes = Vec::new();
        directory.write_to(&mut bytes).unwrap();
        assert_eq!(
            bytes.len(),
            FULL_HISTORY_MANIFEST_BYTES + 3 * FULL_HISTORY_ENTRY_BYTES
        );
        let read =
            FullHistoryDirectory::read_from(&mut bytes.as_slice(), &budget(1 << 20)).unwrap();
        assert_eq!(read.manifest(), directory.manifest());
        let mut generations = Vec::new();
        read.for_each_entry(|entry| {
            generations.push(entry.generation);
            Ok(())
        })
        .unwrap();
        assert_eq!(generations, [9, 9, 10]);
    }

    #[test]
    fn directory_rejects_missing_extra_wrong_link_and_changed_content() {
        let mut bytes = Vec::new();
        directory().write_to(&mut bytes).unwrap();
        for end in [0, FULL_HISTORY_MANIFEST_BYTES - 1, bytes.len() - 1] {
            assert!(FullHistoryDirectory::read_from(&mut &bytes[..end], &budget(1 << 20)).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(FullHistoryDirectory::read_from(&mut extra.as_slice(), &budget(1 << 20)).is_err());
        let mut changed = bytes.clone();
        changed[FULL_HISTORY_MANIFEST_BYTES + 16] ^= 1;
        assert!(
            FullHistoryDirectory::read_from(&mut changed.as_slice(), &budget(1 << 20)).is_err()
        );
        let mut omitted = bytes[..FULL_HISTORY_MANIFEST_BYTES].to_vec();
        omitted.extend_from_slice(&bytes[FULL_HISTORY_MANIFEST_BYTES + FULL_HISTORY_ENTRY_BYTES..]);
        // Even a recomputed manifest/hash cannot make an incomplete chain valid.
        omitted[75..83].copy_from_slice(&2u64.to_be_bytes());
        let sha: [u8; 32] = Sha256::digest(&omitted[FULL_HISTORY_MANIFEST_BYTES..]).into();
        omitted[95..127].copy_from_slice(&sha);
        assert!(
            FullHistoryDirectory::read_from(&mut omitted.as_slice(), &budget(1 << 20)).is_err()
        );
    }

    #[test]
    fn directory_and_comparison_share_one_hard_disk_ceiling() {
        let builder = HistoryBuilder::new(&budget(64 << 10)).unwrap();
        let mut denied = false;
        for n in 0..4096u64 {
            let start = 5 + 3 * n;
            match builder.record(FullHistoryEntry {
                generation: n + 1,
                start_block: start,
                sha256: [0; 32],
                end_block: start + 2,
                previous: (n > 0).then_some(IndexLocation {
                    partition: 'b',
                    start_block: start - 3,
                }),
            }) {
                Ok(()) => {}
                Err(TapeError::LocalCapacity { .. }) => {
                    denied = true;
                    break;
                }
                Err(error) => panic!("unexpected history storage error: {error}"),
            }
        }
        assert!(
            denied,
            "history rows must not escape the comparison database quota"
        );
        assert!(builder.comparison.database().reserved_bytes() <= 64 << 10);
    }

    #[test]
    fn bounded_pages_and_chunk_loader_cover_every_entry_exactly_once() {
        let label = label();
        let builder = HistoryBuilder::new(&budget(2 << 20)).unwrap();
        for n in 0..1000u64 {
            builder
                .record(FullHistoryEntry {
                    generation: n + 1,
                    start_block: 5 + 3 * n,
                    sha256: [n as u8; 32],
                    end_block: 7 + 3 * n,
                    previous: (n > 0).then_some(IndexLocation {
                        partition: 'b',
                        start_block: 2 + 3 * n,
                    }),
                })
                .unwrap();
        }
        let directory = builder
            .finish(
                &label,
                CommittedIndexConstruct {
                    volume_uuid: label.volume_uuid,
                    generation: 1000,
                    location: IndexLocation {
                        partition: 'b',
                        start_block: 3002,
                    },
                    sha256: [999u64 as u8; 32],
                    incremental: false,
                },
                3004,
            )
            .unwrap();
        let mut loader =
            FullHistoryHintBuilder::new(directory.manifest(), &budget(2 << 20)).unwrap();
        let mut after = None;
        let mut total = 0;
        loop {
            let page = directory.entry_page(after, 384).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 384);
            let mut bytes = Vec::new();
            for entry in &page {
                bytes.extend_from_slice(&entry.to_bytes());
            }
            assert!(bytes.len() <= 384 * FULL_HISTORY_ENTRY_BYTES);
            loader.append_entries(&bytes).unwrap();
            total += page.len();
            after = page.last().map(|entry| entry.start_block);
        }
        assert_eq!(total, 1000);
        assert_eq!(loader.finish().unwrap().manifest(), directory.manifest());
        assert!(directory.entry_page(None, 0).is_err());
        assert!(directory.entry_page(None, 1537).is_err());
        let mut incomplete =
            FullHistoryHintBuilder::new(directory.manifest(), &budget(2 << 20)).unwrap();
        let first = directory.entry_page(None, 1).unwrap();
        incomplete.append_entries(&first[0].to_bytes()).unwrap();
        assert!(incomplete.finish().is_err());
    }
}
