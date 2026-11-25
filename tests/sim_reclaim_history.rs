#![cfg(feature = "fdb")]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::index::{IndexEntry, Xattr};
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::{
    FULL_HISTORY_MANIFEST_BYTES, FullHistoryDirectory, IndexSourceScope, RecoveryBudget,
    capture_complete_index_source, capture_complete_index_source_with_history_hint,
    capture_reclaim_preparation_index_source,
    capture_reclaim_preparation_index_source_with_history_hint,
};
use tape_rs::ltfs::volume::{LtfsVolume, read_label_only};
use tape_rs::scsi::device::ScsiResult;
use tape_rs::scsi::sim::{LogicalObject, SimCartridge, SimLibrary, SimTransport};
use tape_rs::scsi::transport::TapeTransport;
use tape_rs::tape::commands::TapeDrive;

const BARCODE: &str = "HIST01L8";

struct Trace<'a> {
    inner: &'a SimTransport,
    commands: RefCell<Vec<Vec<u8>>>,
    reads: RefCell<BTreeMap<u64, usize>>,
    read_sequence: RefCell<Vec<u64>>,
    deny_read_at: Cell<Option<u64>>,
    replace_repeat_dp_head: Cell<Option<u64>>,
    reblock_repeat_dp_head: Cell<Option<(u64, &'a SimLibrary)>>,
}

impl Trace<'_> {
    fn new(inner: &SimTransport) -> Trace<'_> {
        Trace {
            inner,
            commands: RefCell::new(Vec::new()),
            reads: RefCell::new(BTreeMap::new()),
            read_sequence: RefCell::new(Vec::new()),
            deny_read_at: Cell::new(None),
            replace_repeat_dp_head: Cell::new(None),
            reblock_repeat_dp_head: Cell::new(None),
        }
    }
    fn clear(&self) {
        self.commands.borrow_mut().clear();
        self.reads.borrow_mut().clear();
        self.read_sequence.borrow_mut().clear();
    }

    fn dp_locates(&self, eod: bool) -> usize {
        self.commands
            .borrow()
            .iter()
            .filter(|cdb| cdb[0] == 0x92 && cdb[3] == 1 && ((cdb[1] & 0x18) != 0) == eod)
            .count()
    }

    fn reads_at(&self, block: u64) -> usize {
        self.reads.borrow().get(&block).copied().unwrap_or(0)
    }

    fn backward_locates(&self) -> usize {
        let mut previous = None;
        self.commands
            .borrow()
            .iter()
            .filter(|command| {
                if command.first() != Some(&0x92) || command[3] != 1 || command[2] & 0x18 != 0 {
                    return false;
                }
                let block = u64::from_be_bytes(command[4..12].try_into().unwrap());
                let backward = previous.is_some_and(|last| block < last);
                previous = Some(block);
                backward
            })
            .count()
    }
}

impl TapeTransport for Trace<'_> {
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(cdb.to_vec());
        if let Some((block, lib)) = self.reblock_repeat_dp_head.get()
            && cdb == tape_rs::scsi::cdb::locate_16(1, block, true, false)
            && self.reads_at(block) != 0
        {
            self.reblock_repeat_dp_head.set(None);
            lib.with_cartridge_mut(BARCODE, |cart| {
                let part = &mut cart.partitions[1];
                let block = usize::try_from(block).unwrap();
                let LogicalObject::Record(xml) = &mut part.objects[block] else {
                    panic!("Full index record");
                };
                let second = xml.split_off(xml.len() / 2);
                part.objects
                    .insert(block + 1, LogicalObject::Record(second));
                part.flushed = part.objects.len();
            });
        }
        self.inner.execute_no_data(cdb, timeout_ms)
    }
    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(cdb.to_vec());
        // Test-only position observation records the actual physical objects,
        // not the supplied hint, before a successful READ(6).
        let position = if cdb.first() == Some(&0x08) {
            Some(TapeDrive::new(self.inner).read_position()?)
        } else {
            None
        };
        if position
            .as_ref()
            .is_some_and(|p| p.partition == 1 && self.deny_read_at.get() == Some(p.block_number))
        {
            return Err(TapeError::OwnershipLost {
                device: self.identity().to_owned(),
                detail: "history read test owner was fenced".into(),
            });
        }
        let result = self.inner.execute_read(cdb, buf, timeout_ms)?;
        if let Some(position) = position.filter(|p| p.partition == 1) {
            if self.replace_repeat_dp_head.get() == Some(position.block_number)
                && self.reads_at(position.block_number) != 0
            {
                let value = buf[..result.transferred]
                    .windows(b"history".len())
                    .position(|bytes| bytes == b"history")
                    .expect("simulated replacement must touch a real Full index");
                buf[value] = b'H';
            }
            *self
                .reads
                .borrow_mut()
                .entry(position.block_number)
                .or_default() += 1;
            self.read_sequence.borrow_mut().push(position.block_number);
        }
        Ok(result)
    }
    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(cdb.to_vec());
        self.inner.execute_write(cdb, buf, timeout_ms)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

fn setup(fulls: usize) -> (SimLibrary, SimTransport) {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
        .unwrap();
    lib.load_into_drive(BARCODE, 0).unwrap();
    let dev = lib.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "HIST01".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    for n in 0..fulls {
        volume
            .set_node_xattr(
                "/",
                Xattr {
                    key: "history".into(),
                    value: n.to_string(),
                    base64: false,
                },
                n == 0,
                n > 0,
            )
            .unwrap();
        volume.commit().unwrap();
    }
    drop(volume);
    (lib, dev)
}

fn budget() -> RecoveryBudget {
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.max_bytes = 128 << 20;
    budget.index_scratch.headroom_bytes = 0;
    budget
}

#[test]
fn complete_history_hint_reads_all_70_predecessors_in_physical_order() {
    let (_lib, dev) = setup(70);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let mut discovery = capture_complete_index_source(&drive, &label, &budget()).unwrap();
    let head = discovery.construct();
    let hint = discovery
        .take_full_history_hint()
        .expect("complete Full discovery must retain locations");
    let mut blocks = Vec::new();
    hint.for_each_entry(|entry| {
        blocks.push(entry.start_block);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        blocks.len(),
        71,
        "head and every predecessor, including generation one"
    );
    let baseline_backward = trace.backward_locates();
    assert!(baseline_backward >= 70);
    drop(discovery);
    trace.clear();
    let captured =
        capture_complete_index_source_with_history_hint(&drive, &label, &budget(), Some(hint))
            .unwrap();
    assert_eq!(captured.construct(), head);
    assert!(matches!(
        captured.scope(),
        IndexSourceScope::Current {
            history_depth: 70,
            history_truncated: false,
            ..
        }
    ));
    for block in &blocks {
        assert!(
            trace.reads.borrow().get(block).copied().unwrap_or(0) > 0,
            "historical XML at {block} was not actually read"
        );
    }
    assert!(
        !trace
            .commands
            .borrow()
            .iter()
            .any(|command| matches!(command[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );
    let optimized_backward = trace.backward_locates();
    eprintln!(
        "complete Full history baseline_backward={baseline_backward} hinted_backward={optimized_backward} constructs={}",
        blocks.len()
    );
    assert!(
        optimized_backward <= 2,
        "complete cold reads must not seek backwards through every historical Full"
    );
}

fn discover_hint(dev: &SimTransport) -> FullHistoryDirectory {
    let label = read_label_only(dev).unwrap();
    let mut source =
        capture_complete_index_source(&TapeDrive::new(dev), &label, &budget()).unwrap();
    source.take_full_history_hint().unwrap()
}

fn assert_no_writes(trace: &Trace<'_>) {
    assert!(
        !trace
            .commands
            .borrow()
            .iter()
            .any(|cdb| matches!(cdb[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );
}

#[test]
fn stale_history_head_hint_falls_back_to_complete_current_discovery() {
    let (_lib, dev) = setup(4);
    let hint = discover_hint(&dev);
    let old_head = hint.manifest().head;
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .set_node_xattr(
            "/",
            Xattr {
                key: "history".into(),
                value: "new head".into(),
                base64: false,
            },
            false,
            true,
        )
        .unwrap();
    volume.commit().unwrap();
    volume
        .set_node_xattr(
            "/",
            Xattr {
                key: "history".into(),
                value: "second unseen head".into(),
                base64: false,
            },
            false,
            true,
        )
        .unwrap();
    volume.commit().unwrap();
    let new_head = volume.committed_index_construct();
    assert_ne!(new_head, old_head);
    drop(volume);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let result = capture_complete_index_source_with_history_hint(
        &TapeDrive::new(&trace),
        &label,
        &budget(),
        Some(hint),
    )
    .unwrap();
    assert_eq!(result.construct(), new_head);
    assert!(
        !result.history_hint_used(),
        "two unseen Full successors require original discovery"
    );
    assert!(trace.reads.borrow().get(&5).copied().unwrap_or(0) > 0);
    assert!(matches!(
        result.scope(),
        IndexSourceScope::Current {
            history_depth: 6,
            history_truncated: false,
            ..
        }
    ));
    assert_no_writes(&trace);
}

#[test]
fn forged_hint_raw_hash_is_not_a_substitute_for_complete_actual_history() {
    let (_lib, dev) = setup(4);
    let hint = discover_hint(&dev);
    let mut bytes = Vec::new();
    hint.write_to(&mut bytes).unwrap();
    drop(hint);
    bytes[FULL_HISTORY_MANIFEST_BYTES + 16] ^= 1;
    let sha: [u8; 32] = Sha256::digest(&bytes[FULL_HISTORY_MANIFEST_BYTES..]).into();
    bytes[95..127].copy_from_slice(&sha);
    let forged =
        FullHistoryDirectory::read_from(&mut bytes.as_slice(), &budget().index_scratch).unwrap();
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let source = capture_complete_index_source_with_history_hint(
        &TapeDrive::new(&trace),
        &label,
        &budget(),
        Some(forged),
    )
    .unwrap();
    assert_eq!(
        source.full_history_hint().unwrap().manifest().head,
        source.construct()
    );
    assert!(
        trace.reads.borrow().get(&5).copied().unwrap_or(0) >= 2,
        "bad location proof must rediscover the complete real chain"
    );
    assert_no_writes(&trace);
}

#[test]
fn hinted_middle_xml_error_and_owner_loss_propagate_without_more_commands() {
    for lost_owner in [false, true] {
        let (lib, dev) = setup(6);
        let hint = discover_hint(&dev);
        let mut blocks = Vec::new();
        hint.for_each_entry(|entry| {
            blocks.push(entry.start_block);
            Ok(())
        })
        .unwrap();
        let middle = blocks[3];
        let label = read_label_only(&dev).unwrap();
        let trace = Trace::new(&dev);
        if lost_owner {
            trace.deny_read_at.set(Some(middle));
        } else {
            lib.with_cartridge_mut(BARCODE, |cart| {
                let LogicalObject::Record(bytes) = &mut cart.partitions[1].objects[middle as usize]
                else {
                    panic!("index record");
                };
                let xml = String::from_utf8(bytes.clone()).unwrap();
                let changed = xml.replacen(
                    &format!("<startblock>{middle}</startblock>"),
                    &format!("<startblock>{}</startblock>", middle + 1),
                    1,
                );
                assert_ne!(changed, xml);
                *bytes = changed.into_bytes();
            });
        }
        let result = capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&trace),
            &label,
            &budget(),
            Some(hint),
        );
        let error = match result {
            Ok(_) => panic!("bad source must not be accepted"),
            Err(error) => error,
        };
        if lost_owner {
            assert!(matches!(error, TapeError::OwnershipLost { .. }));
            assert_eq!(trace.commands.borrow().last().unwrap()[0], 0x08);
        } else {
            assert_eq!(
                trace.reads.borrow().get(&middle),
                Some(&1),
                "actual invalid XML must not be retried as a hint miss"
            );
        }
        assert_no_writes(&trace);
    }
}

#[test]
fn optional_hint_disk_capacity_falls_back_without_rejecting_supported_source() {
    let (_lib, dev) = setup(4);
    let hint = discover_hint(&dev);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let mut small = budget();
    small.index_scratch.max_bytes = 32 << 20;
    let source = capture_complete_index_source_with_history_hint(
        &TapeDrive::new(&trace),
        &label,
        &small,
        Some(hint),
    )
    .unwrap();
    assert!(
        source.full_history_hint().is_none(),
        "the optional directory must yield space to the original complete proof"
    );
    assert!(matches!(
        source.scope(),
        IndexSourceScope::Current {
            history_depth: 4,
            history_truncated: false,
            ..
        }
    ));
    assert_no_writes(&trace);
}

#[test]
fn ascending_history_still_rejects_same_generation_logical_changes() {
    let (lib, dev) = setup(4);
    let hint = discover_hint(&dev);
    let mut bytes = Vec::new();
    hint.write_to(&mut bytes).unwrap();
    drop(hint);
    let mut raw_sha = [0; 32];
    lib.with_cartridge_mut(BARCODE, |cart| {
        let LogicalObject::Record(record) = &mut cart.partitions[1].objects[5] else {
            panic!("first Full");
        };
        let xml = String::from_utf8(record.clone()).unwrap();
        let changed = xml.replace(
            "<generationnumber>1</generationnumber>",
            "<generationnumber>2</generationnumber>",
        );
        assert_ne!(xml, changed);
        *record = changed.into_bytes();
        raw_sha = Sha256::digest(record).into();
    });
    bytes[FULL_HISTORY_MANIFEST_BYTES..FULL_HISTORY_MANIFEST_BYTES + 8]
        .copy_from_slice(&2u64.to_be_bytes());
    bytes[FULL_HISTORY_MANIFEST_BYTES + 16..FULL_HISTORY_MANIFEST_BYTES + 48]
        .copy_from_slice(&raw_sha);
    let sha: [u8; 32] = Sha256::digest(&bytes[FULL_HISTORY_MANIFEST_BYTES..]).into();
    bytes[95..127].copy_from_slice(&sha);
    let forged =
        FullHistoryDirectory::read_from(&mut bytes.as_slice(), &budget().index_scratch).unwrap();
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let result = capture_complete_index_source_with_history_hint(
        &TapeDrive::new(&trace),
        &label,
        &budget(),
        Some(forged),
    );
    let error = match result {
        Ok(_) => panic!("same-generation changed view must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("同代"), "{error}");
    assert_eq!(trace.reads.borrow().get(&8), Some(&1));
    assert_eq!(
        trace.reads.borrow().get(&11),
        None,
        "same-generation corruption must stop before later constructs"
    );
    assert_no_writes(&trace);
}

#[test]
fn ascending_history_accepts_identical_same_generation_fulls_above_generation_one() {
    use tape_rs::ltfs::index::{IndexLocation, LtfsIndex};
    let (lib, dev) = setup(4);
    lib.with_cartridge_mut(BARCODE, |cart| {
        let LogicalObject::Record(second) = &cart.partitions[1].objects[8] else {
            panic!("second Full");
        };
        let mut first = LtfsIndex::parse(second).unwrap();
        first.self_location = IndexLocation {
            partition: 'b',
            start_block: 5,
        };
        first.previous_location = None;
        cart.partitions[1].objects[5] = LogicalObject::Record(first.to_xml().unwrap().to_vec());
    });
    // Discovery uses the original descending comparison and accepts this
    // legitimate recovery rewrite whose oldest generation is already two.
    let hint = discover_hint(&dev);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let source = capture_complete_index_source_with_history_hint(
        &TapeDrive::new(&trace),
        &label,
        &budget(),
        Some(hint),
    )
    .unwrap();
    assert!(matches!(
        source.scope(),
        IndexSourceScope::Current {
            history_depth: 4,
            history_truncated: false,
            ..
        }
    ));
    assert!(trace.backward_locates() <= 2);
    assert_no_writes(&trace);
}

#[test]
fn complete_history_hint_extends_two_single_full_successors_and_reads_every_construct() {
    let (_lib, dev) = setup(70);
    let label = read_label_only(&dev).unwrap();
    let mut discovery =
        capture_complete_index_source(&TapeDrive::new(&dev), &label, &budget()).unwrap();
    let mut hint = discovery.take_full_history_hint().unwrap();
    drop(discovery);
    let mut known = Vec::new();
    hint.for_each_entry(|entry| {
        known.push(entry.start_block);
        Ok(())
    })
    .unwrap();
    for step in 0..2 {
        let old_head = hint.manifest().head;
        let old_reserved = hint.reserved_bytes();
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume
            .set_node_xattr(
                "/",
                Xattr {
                    key: "history".into(),
                    value: format!("successor-{step}"),
                    base64: false,
                },
                false,
                true,
            )
            .unwrap();
        volume.commit().unwrap();
        let head = volume.committed_index_construct();
        assert_eq!(volume.index().previous_location, Some(old_head.location));
        drop(volume);
        known.push(head.location.start_block);
        let trace = Trace::new(&dev);
        let mut actual = capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&trace),
            &label,
            &budget(),
            Some(hint),
        )
        .unwrap();
        assert_eq!(actual.construct(), head);
        for block in &known {
            assert!(
                trace.reads.borrow().get(block).copied().unwrap_or(0) > 0,
                "old/new construct {block} must be physically read"
            );
        }
        assert_no_writes(&trace);
        eprintln!(
            "single Full successor step={step} constructs={} hint_used={} backward={} old_hint_reserved={old_reserved}",
            known.len(),
            actual.history_hint_used(),
            trace.backward_locates()
        );
        assert!(
            actual.history_hint_used(),
            "a direct single Full successor must extend only its read-order candidate"
        );
        assert!(trace.backward_locates() <= 2);
        hint = actual.take_full_history_hint().unwrap();
        assert_eq!(hint.manifest().entries as usize, known.len());
    }
}

fn append_same_generation_full(lib: &SimLibrary, conflict: bool) {
    use tape_rs::ltfs::index::{IndexLocation, LtfsIndex};
    lib.with_cartridge_mut(BARCODE, |cart| {
        let dp = &mut cart.partitions[1];
        let old_start = dp
            .objects
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, object)| matches!(object, LogicalObject::Record(_)).then_some(index))
            .unwrap();
        let LogicalObject::Record(xml) = &dp.objects[old_start] else {
            unreachable!();
        };
        let mut head = LtfsIndex::parse(xml).unwrap();
        let old_location = head.self_location;
        let start = dp.objects.len() as u64 + 1;
        head.self_location = IndexLocation {
            partition: 'b',
            start_block: start,
        };
        head.previous_location = Some(old_location);
        if conflict {
            head.root
                .xattrs
                .iter_mut()
                .find(|attr| attr.key == "history")
                .unwrap()
                .value = "changed in same generation".into();
        }
        dp.objects.push(LogicalObject::Filemark);
        dp.objects
            .push(LogicalObject::Record(head.to_xml().unwrap().to_vec()));
        dp.objects.push(LogicalObject::Filemark);
        dp.flushed = dp.objects.len();
        // Same-generation IP independently names the new DP recovery Full.
        head.previous_location = Some(head.self_location);
        head.self_location = IndexLocation {
            partition: 'a',
            start_block: 5,
        };
        cart.partitions[0].objects[5] = LogicalObject::Record(head.to_xml().unwrap().to_vec());
        cart.partitions[0].flushed = cart.partitions[0].objects.len();
    });
}

#[test]
fn single_successor_candidate_keeps_same_generation_full_tree_checks() {
    for conflict in [false, true] {
        let (lib, dev) = setup(4);
        let hint = discover_hint(&dev);
        let old = hint.manifest();
        append_same_generation_full(&lib, conflict);
        let label = read_label_only(&dev).unwrap();
        let trace = Trace::new(&dev);
        let actual = capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&trace),
            &label,
            &budget(),
            Some(hint),
        );
        if conflict {
            let error = match actual {
                Ok(_) => panic!("same-generation logical change must not pass"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("同代"), "{error}");
        } else {
            let actual = actual.unwrap();
            assert!(actual.history_hint_used());
            assert_eq!(actual.construct().generation, old.head.generation);
            assert_eq!(
                actual.full_history_hint().unwrap().manifest().entries,
                old.entries + 1
            );
            assert!(trace.backward_locates() <= 2);
        }
        assert_no_writes(&trace);
    }
}

#[test]
fn single_successor_candidate_does_not_hide_an_invalid_actual_previous_pointer() {
    use tape_rs::ltfs::index::{IndexLocation, LtfsIndex};
    let (lib, dev) = setup(4);
    let hint = discover_hint(&dev);
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .set_node_xattr(
            "/",
            Xattr {
                key: "history".into(),
                value: "new".into(),
                base64: false,
            },
            false,
            true,
        )
        .unwrap();
    volume.commit().unwrap();
    let start = volume.committed_index_construct().location.start_block;
    drop(volume);
    lib.with_cartridge_mut(BARCODE, |cart| {
        let LogicalObject::Record(xml) = &cart.partitions[1].objects[start as usize] else {
            panic!("new Full");
        };
        let mut index = LtfsIndex::parse(xml).unwrap();
        index.previous_location = Some(IndexLocation {
            partition: 'b',
            start_block: start + 1,
        });
        cart.partitions[1].objects[start as usize] =
            LogicalObject::Record(index.to_xml().unwrap().to_vec());
    });
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    assert!(
        capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&trace),
            &label,
            &budget(),
            Some(hint)
        )
        .is_err()
    );
    assert_no_writes(&trace);
}
#[test]
fn full_successor_with_actual_delta_backpointer_keeps_original_full_audit_scope() {
    use tape_rs::ltfs::index::{IndexLocation, LtfsIndex};
    for invalid_delta_pointer in [false, true] {
        let (lib, dev) = setup(4);
        let hint = discover_hint(&dev);
        let old = hint.manifest();
        let mut full_starts = Vec::new();
        hint.for_each_entry(|entry| {
            full_starts.push(entry.start_block);
            Ok(())
        })
        .unwrap();
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume
            .set_node_xattr(
                "/",
                Xattr {
                    key: "history".into(),
                    value: "actual-delta".into(),
                    base64: false,
                },
                false,
                true,
            )
            .unwrap();
        volume.commit_incremental().unwrap();
        let delta = volume.committed_index_construct();
        assert!(delta.incremental);
        volume
            .set_node_xattr(
                "/",
                Xattr {
                    key: "history".into(),
                    value: "full-after-delta".into(),
                    base64: false,
                },
                false,
                true,
            )
            .unwrap();
        volume.commit().unwrap();
        let head = volume.committed_index_construct();
        assert!(!head.incremental);
        assert_eq!(volume.index().previous_location, Some(old.head.location));
        assert_eq!(
            volume.index().previous_incremental_location,
            Some(delta.location)
        );
        drop(volume);
        full_starts.push(head.location.start_block);
        if invalid_delta_pointer {
            lib.with_cartridge_mut(BARCODE, |cart| {
                let LogicalObject::Record(xml) =
                    &cart.partitions[1].objects[head.location.start_block as usize]
                else {
                    panic!("Full record")
                };
                let mut index = LtfsIndex::parse(xml).unwrap();
                index.previous_incremental_location = Some(IndexLocation {
                    partition: 'b',
                    start_block: head.location.start_block,
                });
                cart.partitions[1].objects[head.location.start_block as usize] =
                    LogicalObject::Record(index.to_xml().unwrap().to_vec());
            });
        }
        let trace = Trace::new(&dev);
        let label = read_label_only(&dev).unwrap();
        let captured = capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&trace),
            &label,
            &budget(),
            Some(hint),
        );
        if invalid_delta_pointer {
            assert!(captured.is_err());
        } else {
            let captured = captured.unwrap();
            assert!(captured.history_hint_used());
            assert_eq!(
                captured.full_history_hint().unwrap().manifest().entries,
                old.entries + 1,
                "Delta must not enter the Full scheduling directory"
            );
            for start in full_starts {
                assert!(
                    trace.reads.borrow().get(&start).copied().unwrap_or(0) > 0,
                    "Full {start} must actually be read"
                );
            }
            assert!(trace.backward_locates() <= 2);
        }
        assert_no_writes(&trace);
    }
}

#[test]
fn reclaim_preparation_reuses_one_actual_dp_head_read() {
    let (_lib, dev) = setup(4);
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/nested/file", &mut &b"extent bytes"[..])
        .unwrap();
    volume.commit().unwrap();
    drop(volume);

    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let original = capture_complete_index_source(&drive, &label, &budget()).unwrap();
    let head = original.construct().location.start_block;
    let read_count = trace.reads_at(head);
    let ordinary = trace.dp_locates(false);
    let eod = trace.dp_locates(true);
    let original_reads = trace.reads.borrow().clone();
    trace.clear();

    let prepared = capture_reclaim_preparation_index_source(&drive, &label, &budget()).unwrap();
    assert_eq!(original.construct(), prepared.construct());
    assert_eq!(original.identity().unwrap(), prepared.identity().unwrap());
    assert_eq!(original.construct_end(), prepared.construct_end());
    assert_eq!(original.scope(), prepared.scope());
    assert_eq!(original.source_bytes(), prepared.source_bytes());
    assert_eq!(
        original.full_history_hint().unwrap().manifest(),
        prepared.full_history_hint().unwrap().manifest()
    );
    let mut original_entries = Vec::new();
    original
        .for_each_entry(|entry| {
            original_entries.push(entry);
            Ok(())
        })
        .unwrap();
    let mut prepared_entries = Vec::new();
    prepared
        .for_each_entry(|entry| {
            prepared_entries.push(entry);
            Ok(())
        })
        .unwrap();
    assert_eq!(original_entries, prepared_entries);
    assert_eq!(read_count, 3);
    eprintln!(
        "preparation current DP READ {}→{}; DP LOCATE {}→{}; DP EOD LOCATE {}→{}",
        read_count,
        trace.reads_at(head),
        ordinary,
        trace.dp_locates(false),
        eod,
        trace.dp_locates(true)
    );
    assert_eq!(
        trace.reads_at(head),
        2,
        "initial and independent final read"
    );
    assert_eq!(trace.dp_locates(false) + 1, ordinary);
    assert_eq!(trace.dp_locates(true) + 1, eod);
    for (&block, &reads) in &original_reads {
        if block < head - 1 {
            assert_eq!(trace.reads_at(block), reads, "historical block {block}");
        }
    }
    assert_no_writes(&trace);
}

#[test]
fn reclaim_preparation_schedules_fresh_history_from_prior_capture() {
    let (_lib, dev) = setup(7);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let mut before = capture_reclaim_preparation_index_source(&drive, &label, &budget()).unwrap();
    let baseline_backward = trace.backward_locates();
    let hint = before.take_full_history_hint().unwrap();
    let manifest = hint.manifest();
    let mut bytes = Vec::new();
    hint.write_to(&mut bytes).unwrap();
    drop(hint);
    drop(before);
    let imported =
        FullHistoryDirectory::read_from(&mut bytes.as_slice(), &budget().index_scratch).unwrap();
    trace.clear();
    let captured = capture_reclaim_preparation_index_source_with_history_hint(
        &drive,
        &label,
        &budget(),
        Some(imported),
    )
    .unwrap();
    assert!(captured.history_hint_used());
    let actual = captured.full_history_hint().unwrap();
    assert_eq!(actual.manifest(), manifest);
    actual
        .for_each_entry(|entry| {
            assert!(trace.reads_at(entry.start_block) > 0);
            Ok(())
        })
        .unwrap();
    assert!(trace.backward_locates() < baseline_backward);
    assert_no_writes(&trace);
}

#[test]
fn reclaim_preparation_final_dp_read_rejects_changed_head() {
    let (_lib, dev) = setup(4);
    let label = read_label_only(&dev).unwrap();
    let head = discover_hint(&dev).manifest().head.location.start_block;
    let trace = Trace::new(&dev);
    trace.replace_repeat_dp_head.set(Some(head));
    let actual =
        capture_reclaim_preparation_index_source(&TapeDrive::new(&trace), &label, &budget());
    let Err(error) = actual else {
        panic!("independent final capture accepted a changed raw head");
    };
    assert!(error.to_string().contains("摘要"), "{error}");
    assert_eq!(trace.reads_at(head), 2);
    assert_eq!(trace.reads_at(5), 1);
    assert_no_writes(&trace);
}

#[test]
fn reclaim_preparation_rejects_bad_history_ip_and_tail() {
    for case in ["history", "ip", "tail"] {
        let (lib, dev) = setup(4);
        let label = read_label_only(&dev).unwrap();
        lib.with_cartridge_mut(BARCODE, |cart| match case {
            "history" => cart.partitions[1].objects[5] = LogicalObject::Record(b"bad".to_vec()),
            "ip" => {
                let LogicalObject::Record(bytes) = &mut cart.partitions[0].objects[5] else {
                    panic!("IP Full record");
                };
                let offset = bytes
                    .windows(b"history".len())
                    .position(|value| value == b"history")
                    .unwrap();
                bytes[offset] = b'H';
            }
            "tail" => {
                let dp = &mut cart.partitions[1];
                dp.objects
                    .push(LogicalObject::Record(b"unindexed tail".to_vec()));
                dp.flushed = dp.objects.len();
            }
            _ => unreachable!(),
        });
        let trace = Trace::new(&dev);
        let drive = TapeDrive::new(&trace);
        let original = capture_complete_index_source(&drive, &label, &budget());
        trace.clear();
        let prepared = capture_reclaim_preparation_index_source(&drive, &label, &budget());
        assert!(
            original.is_err() && prepared.is_err(),
            "accepted damaged {case}"
        );
        assert_no_writes(&trace);
    }
}

#[test]
fn reclaim_preparation_preserves_incremental_and_optional_capacity_paths() {
    for incremental in [true, false] {
        let (_lib, dev) = setup(2);
        let mut limit = budget();
        if incremental {
            let mut volume = LtfsVolume::mount(&dev).unwrap();
            volume
                .append_file("/delta-file", &mut &b"delta body"[..])
                .unwrap();
            volume.commit_incremental().unwrap();
            assert!(volume.committed_index_construct().incremental);
        } else {
            limit.index_scratch.max_bytes = 32 << 20;
        }
        let label = read_label_only(&dev).unwrap();
        let trace = Trace::new(&dev);
        let drive = TapeDrive::new(&trace);
        let original = capture_complete_index_source(&drive, &label, &limit).unwrap();
        assert!(original.full_history_hint().is_none());
        let identity = original.identity().unwrap();
        let scope = original.scope();
        let commands = trace.commands.borrow().clone();
        drop(original);
        trace.clear();

        let prepared = capture_reclaim_preparation_index_source(&drive, &label, &limit).unwrap();
        assert!(prepared.full_history_hint().is_none());
        assert_eq!(prepared.identity().unwrap(), identity);
        assert_eq!(prepared.scope(), scope);
        if incremental {
            assert_eq!(*trace.commands.borrow(), commands);
        }
        assert_no_writes(&trace);
    }
}

#[test]
fn reclaim_readback_rereads_current_head_and_all_history_without_second_replay() {
    let (_lib, dev) = setup(4);
    let original_body = b"target donor body";
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume
        .append_file("/donor", &mut &original_body[..])
        .unwrap();
    volume.commit().unwrap();
    drop(volume);

    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let mut limit = budget();
    limit.max_chain_len = 1;
    let mut baseline = capture_complete_index_source(&drive, &label, &limit).unwrap();
    let head = baseline.construct().location.start_block;
    let hint = baseline.take_full_history_hint().unwrap();
    let mut history_blocks = Vec::new();
    hint.for_each_entry(|entry| {
        history_blocks.push(entry.start_block);
        Ok(())
    })
    .unwrap();
    assert_eq!(history_blocks.len(), 6);
    let read_count = trace.reads_at(head);
    let ordinary = trace.dp_locates(false);
    let eod = trace.dp_locates(true);
    drop(baseline);
    trace.clear();

    // A may already use an untrusted schedule. B must still reread every
    // physical history entry matching A's independently verified directory.
    let mut capture_a =
        capture_complete_index_source_with_history_hint(&drive, &label, &limit, Some(hint))
            .unwrap();
    assert!(capture_a.history_hint_used());
    assert!(trace.reads_at(head) > 0);
    let mut body_start = None;
    capture_a
        .for_each_entry(|entry| {
            if let IndexEntry::File { path, node } = entry {
                assert_eq!(path, "donor");
                assert_eq!(node.extents.len(), 1);
                body_start = Some(node.extents[0].start_block);
            }
            Ok(())
        })
        .unwrap();
    let full_head = capture_a
        .take_reclaim_readback_full_head(limit.index_scratch.max_bytes)
        .unwrap();
    drop(capture_a);

    trace.clear();
    let body_start = body_start.unwrap();
    drive.locate(1, body_start, true).unwrap();
    let mut buffer = vec![0; label.blocksize as usize];
    let bytes = drive.read_block(&mut buffer).unwrap();
    assert_eq!(&buffer[..bytes], original_body);
    assert_eq!(trace.reads_at(body_start), 1);

    trace.clear();
    full_head.verify(&drive, &label, &limit).unwrap();
    let actual_history = trace
        .read_sequence
        .borrow()
        .iter()
        .copied()
        .filter(|block| history_blocks.contains(block))
        .collect::<Vec<_>>();
    let mut expected_history = history_blocks.clone();
    expected_history.push(head); // Final independent DP/IP verification reads the head again.
    assert_eq!(actual_history, expected_history);
    eprintln!(
        "readback B current DP READ {}→{}; DP LOCATE {}→{}; DP EOD LOCATE {}→{}",
        read_count,
        trace.reads_at(head),
        ordinary,
        trace.dp_locates(false),
        eod,
        trace.dp_locates(true)
    );
    assert_eq!(read_count, 3);
    assert_eq!(trace.reads_at(head), 2);
    assert!(trace.dp_locates(false) + 1 < ordinary);
    assert_eq!(trace.dp_locates(true) + 1, eod);
    for &block in &history_blocks[..history_blocks.len() - 1] {
        assert_eq!(trace.reads_at(block), 1, "historical Full at {block}");
    }
    assert_no_writes(&trace);
}

#[test]
fn reclaim_readback_rejects_current_full_history_ip_and_eod_changes() {
    for case in [
        "raw head",
        "reblocked head",
        "raw history",
        "damaged history",
        "different IP",
        "unindexed tail",
    ] {
        let (lib, dev) = setup(4);
        let label = read_label_only(&dev).unwrap();
        let trace = Trace::new(&dev);
        let drive = TapeDrive::new(&trace);
        let mut capture_a = capture_complete_index_source(&drive, &label, &budget()).unwrap();
        let head = capture_a.construct().location.start_block as usize;
        let full_head = capture_a
            .take_reclaim_readback_full_head(budget().index_scratch.max_bytes)
            .unwrap();
        drop(capture_a);
        lib.with_cartridge_mut(BARCODE, |cart| {
            let (partition, block) = match case {
                "raw history" | "damaged history" => (1, 5),
                "different IP" => (0, 5),
                _ => (1, head),
            };
            let part = &mut cart.partitions[partition];
            let LogicalObject::Record(xml) = &mut part.objects[block] else {
                panic!("Full index record");
            };
            match case {
                "raw head" | "raw history" => xml.push(b'\n'),
                "reblocked head" => {
                    let second = xml.split_off(xml.len() / 2);
                    part.objects
                        .insert(block + 1, LogicalObject::Record(second));
                }
                "damaged history" => *xml = b"bad history".to_vec(),
                "different IP" => {
                    let offset = xml
                        .windows(b"history".len())
                        .position(|value| value == b"history")
                        .unwrap();
                    xml[offset] = b'H';
                }
                "unindexed tail" => {
                    part.objects
                        .push(LogicalObject::Record(b"unindexed data".to_vec()));
                }
                _ => unreachable!(),
            }
            part.flushed = part.objects.len();
        });
        if matches!(case, "raw head" | "reblocked head" | "raw history") {
            // Still a valid current Full. Check that B detects the change
            // against A, beyond validating only the tape at B's instant.
            let changed = capture_complete_index_source(&drive, &label, &budget()).unwrap();
            assert_eq!(changed.construct().location.start_block, head as u64);
            assert_eq!(changed.construct().generation, 5);
        }
        trace.clear();
        let error = full_head.verify(&drive, &label, &budget()).unwrap_err();
        if matches!(case, "raw head" | "reblocked head" | "raw history") {
            assert!(error.to_string().contains("EOD/closing"), "{case}: {error}");
        }
        assert_no_writes(&trace);
    }
}

#[test]
fn reclaim_readback_historical_device_error_stops_without_retry() {
    let (_lib, dev) = setup(4);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let mut capture_a = capture_complete_index_source(&drive, &label, &budget()).unwrap();
    let head = capture_a.construct().location.start_block;
    let full_head = capture_a
        .take_reclaim_readback_full_head(budget().index_scratch.max_bytes)
        .unwrap();
    drop(capture_a);
    trace.clear();
    trace.deny_read_at.set(Some(8));
    let error = full_head.verify(&drive, &label, &budget()).unwrap_err();
    assert!(matches!(error, TapeError::OwnershipLost { .. }));
    assert_eq!(trace.commands.borrow().last().unwrap()[0], 0x08);
    assert_eq!(trace.reads_at(5), 1);
    assert_eq!(trace.reads_at(head), 0);
    assert_no_writes(&trace);
}

#[test]
fn reclaim_scheduled_readback_rejects_reblocked_second_head_with_same_digest() {
    let (lib, dev) = setup(4);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let limit = budget();
    let mut capture_a = capture_complete_index_source(&drive, &label, &limit).unwrap();
    let head = capture_a.construct().location.start_block;
    let full_head = capture_a
        .take_reclaim_readback_full_head(limit.index_scratch.max_bytes)
        .unwrap();
    drop(capture_a);

    trace.clear();
    trace.reblock_repeat_dp_head.set(Some((head, &lib)));
    let error = full_head.verify(&drive, &label, &limit).unwrap_err();
    assert!(trace.reblock_repeat_dp_head.get().is_none());
    assert!(error.to_string().contains("DP 构造结束位置或 EOD 改变"));
    assert_no_writes(&trace);
}

#[test]
fn reclaim_readback_with_small_scratch_uses_original_pass_without_speculating() {
    let (_lib, dev) = setup(4);
    let label = read_label_only(&dev).unwrap();
    let trace = Trace::new(&dev);
    let drive = TapeDrive::new(&trace);
    let mut capture_a = capture_complete_index_source(&drive, &label, &budget()).unwrap();
    let original = capture_a.reclaim_readback_full_head().unwrap();
    let mut limit = budget();
    let reservation = capture_a.full_history_hint().unwrap().reserved_bytes();
    limit.index_scratch.max_bytes = reservation * 2 - 4096;
    let small_scratch = capture_a
        .take_reclaim_readback_full_head(limit.index_scratch.max_bytes)
        .unwrap();
    drop(capture_a);

    trace.clear();
    original.verify(&drive, &label, &limit).unwrap();
    let expected = trace.commands.take();
    trace.clear();
    small_scratch.verify(&drive, &label, &limit).unwrap();
    assert_eq!(*trace.commands.borrow(), expected);
    assert_no_writes(&trace);
}

#[test]
fn reclaim_readback_without_complete_history_directory_keeps_original_capture() {
    let (_lib, dev) = setup(4);
    let label = read_label_only(&dev).unwrap();
    let mut limit = budget();
    limit.index_scratch.max_bytes = 32 << 20;
    let captured = capture_complete_index_source(&TapeDrive::new(&dev), &label, &limit).unwrap();
    assert!(captured.full_history_hint().is_none());
    assert!(captured.reclaim_readback_full_head().is_none());
}
