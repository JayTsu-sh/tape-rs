//! LE 普通 sync（DP）与清洁卸载（IP）的独立边界。
use std::cell::{Cell, RefCell};
use std::io::Cursor;

use tape_rs::error::Result;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::device::ScsiResult;
use tape_rs::scsi::sim::{SimCartridge, SimLibrary, SimTransport};
use tape_rs::scsi::transport::TapeTransport;

struct Trace {
    inner: SimTransport,
    commands: RefCell<Vec<Vec<u8>>>,
    writes: RefCell<Vec<(usize, usize)>>,
    mode_parameters: RefCell<Vec<Vec<u8>>>,
    short_write: Cell<bool>,
    position_flags: Cell<u8>,
    reject_eod: Cell<bool>,
    no_data_timeouts: RefCell<Vec<(u8, u32)>>,
}
impl TapeTransport for Trace {
    fn execute_no_data(&self, c: &[u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
        self.no_data_timeouts.borrow_mut().push((c[0], t));
        if c[0] == 0x92 && c[1] & 0x38 == 0x18 && self.reject_eod.get() {
            return Err(tape_rs::error::TapeError::ScsiCommand {
                status: 2,
                sense_key: 5,
                asc: 0x24,
                ascq: 0,
            });
        }
        self.inner.execute_no_data(c, t)
    }
    fn execute_read(&self, c: &[u8], b: &mut [u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
        let result = self.inner.execute_read(c, b, t)?;
        if c[0] == 0x34 && result.transferred >= 20 {
            b[0] |= self.position_flags.get();
        }
        Ok(result)
    }
    fn execute_write(&self, c: &[u8], b: &[u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
        if c[0] == 0x55 {
            self.mode_parameters.borrow_mut().push(b.to_vec());
        }
        if c[0] == 0x0a {
            self.writes
                .borrow_mut()
                .push((b.as_ptr() as usize, b.len()));
        }
        let mut result = self.inner.execute_write(c, b, t)?;
        if c[0] == 0x0a && self.short_write.replace(false) {
            result.transferred = b.len().saturating_sub(1);
        }
        Ok(result)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}
fn setup() -> (SimLibrary, Trace) {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("IOS001L8", 64 << 20), 0)
        .unwrap();
    lib.load_into_drive("IOS001L8", 0).unwrap();
    let dev = Trace {
        inner: lib.drive(0),
        commands: RefCell::default(),
        writes: RefCell::default(),
        mode_parameters: RefCell::default(),
        short_write: Cell::new(false),
        position_flags: Cell::new(0),
        reject_eod: Cell::new(false),
        no_data_timeouts: RefCell::default(),
    };
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "IOS001".into(),
            ..Default::default()
        },
    )
    .unwrap();
    (lib, dev)
}

#[test]
fn ordinary_sync_leaves_ip_then_unmount_checkpoints_without_new_dp_generation() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_file("data", &mut Cursor::new(vec![7; 700_001]))
        .unwrap();
    dev.commands.borrow_mut().clear();
    vol.sync().unwrap();
    let generation = vol.index().generation;
    assert!(vol.needs_checkpoint());
    assert!(
        !dev.commands
            .borrow()
            .iter()
            .any(|c| c[0] == 0x92 && c[3] == 0)
    );
    drop(vol);
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(vol.index().generation, generation);
    assert!(vol.needs_checkpoint());
    let mut content = Vec::new();
    vol.read_file_to_writer("data", &mut content).unwrap();
    assert_eq!(content, vec![7; 700_001]);
    dev.commands.borrow_mut().clear();
    vol.unmount().unwrap();
    let commands = dev.commands.borrow();
    assert!(commands.iter().any(|c| c[0] == 0x92 && c[3] == 0));
    assert!(!commands.iter().any(|c| c[0] == 0x92 && c[3] == 1));
    drop(commands);
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(!vol.needs_checkpoint());
    assert_eq!(vol.index().generation, generation);
}

#[test]
fn clean_sync_does_not_touch_the_tape() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    dev.commands.borrow_mut().clear();
    vol.sync().unwrap();
    assert!(dev.commands.borrow().is_empty());
}

#[test]
fn borrowed_append_passes_original_slices_to_transport_without_a_copy() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    let block = vol.block_size() as usize;
    let data = vec![0x5a; block * 2 + 17];
    dev.writes.borrow_mut().clear();
    assert_eq!(
        vol.append_bytes("borrowed", &data).unwrap(),
        data.len() as u64
    );
    assert_eq!(
        *dev.writes.borrow(),
        vec![
            (data.as_ptr() as usize, block),
            (data.as_ptr() as usize + block, block),
            (data.as_ptr() as usize + 2 * block, 17),
        ]
    );
    vol.sync().unwrap();
    let mut actual = Vec::new();
    vol.read_file_to_writer("borrowed", &mut actual).unwrap();
    assert_eq!(actual, data);
}

#[test]
fn data_write_failure_freezes_borrowed_append_without_publishing() {
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    dev.inner.inject(Fault {
        opcode: 0x0a,
        after: 1,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 3,
            asc: 0x0c,
            ascq: 0,
        },
    });
    assert!(
        vol.append_bytes("partial", &vec![1; vol.block_size() as usize + 7])
            .is_err()
    );
    assert!(!vol.writable());
    assert!(vol.index().find_file("partial").is_none());
    assert!(vol.append_bytes("later", b"unsafe").is_err());
}

#[test]
fn ordinary_sync_index_failure_keeps_the_previous_view_and_freezes() {
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_bytes("old", b"durable").unwrap();
    vol.commit().unwrap();
    let generation = vol.index().generation;
    vol.append_bytes("new", b"pending").unwrap();
    dev.inner.inject(Fault {
        opcode: 0x0a,
        after: 0,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 3,
            asc: 0x0c,
            ascq: 0,
        },
    });
    assert!(vol.sync().is_err());
    assert!(!vol.writable());
    assert_eq!(vol.index().generation, generation);
    assert!(vol.index().find_file("new").is_none());
    drop(vol);
    let recovered = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(recovered.index().generation, generation);
    let mut bytes = Vec::new();
    recovered.read_file_to_writer("old", &mut bytes).unwrap();
    assert_eq!(bytes, b"durable");
}

#[test]
fn ip_checkpoint_failure_preserves_the_already_synced_dp_generation() {
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_bytes("data", b"durable DP").unwrap();
    vol.sync().unwrap();
    let generation = vol.index().generation;
    dev.inner.inject(Fault {
        opcode: 0x0a,
        after: 0,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 3,
            asc: 0x0c,
            ascq: 0,
        },
    });
    assert!(vol.commit().is_err());
    assert!(!vol.writable());
    drop(vol);
    let recovered = LtfsVolume::mount(&dev).unwrap();
    assert_eq!(recovered.index().generation, generation);
    assert!(recovered.needs_checkpoint());
    let mut bytes = Vec::new();
    recovered.read_file_to_writer("data", &mut bytes).unwrap();
    assert_eq!(bytes, b"durable DP");
}

#[test]
fn short_write_freezes_volume_without_publishing_or_retrying() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    dev.writes.borrow_mut().clear();
    dev.short_write.set(true);
    assert!(vol.append_bytes("short", b"partial").is_err());
    assert!(!vol.writable());
    assert!(vol.index().find_file("short").is_none());
    assert_eq!(dev.writes.borrow().len(), 1);
}

#[test]
fn source_error_after_a_record_freezes_the_unindexed_tail() {
    struct BrokenSource {
        remaining: usize,
    }
    impl std::io::Read for BrokenSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("source failure"));
            }
            let n = self.remaining.min(buf.len());
            buf[..n].fill(17);
            self.remaining -= n;
            Ok(n)
        }
    }
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    let mut source = BrokenSource {
        remaining: vol.block_size() as usize,
    };
    dev.writes.borrow_mut().clear();
    assert!(vol.append_file("broken", &mut source).is_err());
    assert!(!vol.writable());
    assert_eq!(dev.writes.borrow().len(), 1);
    assert!(vol.index().find_file("broken").is_none());
}

#[test]
fn load_allows_lto9_initialization_without_resubmitting_the_command() {
    let (_lib, dev) = setup();
    dev.commands.borrow_mut().clear();
    dev.no_data_timeouts.borrow_mut().clear();
    tape_rs::tape::commands::TapeDrive::new(&dev)
        .load()
        .unwrap();
    assert_eq!(*dev.commands.borrow(), vec![vec![0x1b, 0, 0, 0, 1, 0]]);
    let timeouts = dev.no_data_timeouts.borrow();
    assert_eq!(timeouts.len(), 1);
    assert!(timeouts[0].1 >= 2 * 60 * 60 * 1000);
}

#[test]
fn sequential_appends_do_not_reposition_an_already_correct_drive() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    // 首个追加可能需要从挂载扫描的停留位置移到写入点。
    vol.append_bytes("seed", b"seed").unwrap();
    dev.commands.borrow_mut().clear();
    for i in 0..8 {
        vol.append_bytes(&format!("small-{i}"), &[i; 64]).unwrap();
    }
    assert!(!dev.commands.borrow().iter().any(|c| c[0] == 0x92));
    vol.commit().unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    for i in 0..8 {
        let mut actual = Vec::new();
        vol.read_file_to_writer(&format!("small-{i}"), &mut actual)
            .unwrap();
        assert_eq!(actual, vec![i; 64]);
    }
}

#[test]
fn append_repositions_after_external_movement_or_unknown_position() {
    for flags in [0, 0x04, 0x02] {
        let (_lib, dev) = setup();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_bytes("first", b"keep").unwrap();
        if flags == 0 {
            tape_rs::tape::commands::TapeDrive::new(&dev)
                .locate(0, 0, true)
                .unwrap();
        }
        dev.position_flags.set(flags);
        dev.commands.borrow_mut().clear();
        vol.append_bytes("second", b"new").unwrap();
        assert!(dev.commands.borrow().iter().any(|c| c[0] == 0x92));
        dev.position_flags.set(0);
        vol.commit().unwrap();
        let vol = LtfsVolume::mount(&dev).unwrap();
        let mut actual = Vec::new();
        vol.read_file_to_writer("first", &mut actual).unwrap();
        assert_eq!(actual, b"keep");
    }
}

#[test]
fn position_query_error_prevents_append_write() {
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    dev.inner.inject(Fault {
        opcode: 0x34,
        after: 0,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 4,
            asc: 0x44,
            ascq: 0,
        },
    });
    dev.commands.borrow_mut().clear();
    assert!(vol.append_bytes("unsafe", b"no").is_err());
    assert!(!dev.commands.borrow().iter().any(|c| c[0] == 0x0a));
}

#[test]
fn sync_uses_live_position_and_recovers_from_external_movement() {
    for moved in [false, true] {
        let (_lib, dev) = setup();
        let mut vol = LtfsVolume::mount(&dev).unwrap();
        vol.append_bytes("data", b"durable").unwrap();
        if moved {
            tape_rs::tape::commands::TapeDrive::new(&dev)
                .locate(0, 0, true)
                .unwrap();
        }
        dev.commands.borrow_mut().clear();
        vol.sync().unwrap();
        assert_eq!(dev.commands.borrow().iter().any(|c| c[0] == 0x92), moved);
        drop(vol);
        let vol = LtfsVolume::mount(&dev).unwrap();
        let mut actual = Vec::new();
        vol.read_file_to_writer("data", &mut actual).unwrap();
        assert_eq!(actual, b"durable");
    }
}

#[test]
fn sync_position_error_freezes_without_writing_an_index() {
    use tape_rs::error::TapeError;
    use tape_rs::scsi::sim::{Fault, FaultAction};
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_bytes("pending", b"no index yet").unwrap();
    let generation = vol.index().generation;
    dev.inner.inject(Fault {
        opcode: 0x34,
        after: 0,
        remaining: 1,
        action: FaultAction::CheckCondition {
            sense_key: 4,
            asc: 0x44,
            ascq: 0,
        },
    });
    dev.commands.borrow_mut().clear();
    assert!(matches!(vol.sync(), Err(TapeError::CommitFailed { stage, .. }) if stage == "locate"));
    assert!(!vol.writable());
    assert_eq!(vol.index().generation, generation);
    assert!(vol.index().find_file("pending").is_none());
    assert!(
        !dev.commands
            .borrow()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10))
    );
}

#[test]
fn sequential_reads_avoid_reposition_and_random_reads_still_seek() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    for (name, bytes) in [("first", b"aaa".as_slice()), ("second", b"bbbb")] {
        vol.append_bytes(name, bytes).unwrap();
    }
    vol.commit().unwrap();
    let mut out = Vec::new();
    vol.read_file_to_writer("first", &mut out).unwrap();
    assert_eq!(out, b"aaa");
    out.clear();
    dev.commands.borrow_mut().clear();
    vol.read_file_to_writer("second", &mut out).unwrap();
    assert_eq!(out, b"bbbb");
    assert!(!dev.commands.borrow().iter().any(|c| c[0] == 0x92));
    out.clear();
    dev.commands.borrow_mut().clear();
    vol.read_file_to_writer("first", &mut out).unwrap();
    assert_eq!(out, b"aaa");
    assert!(dev.commands.borrow().iter().any(|c| c[0] == 0x92));
}

#[test]
fn formatting_selects_buffered_writes_without_changing_drive_speed() {
    let (_lib, dev) = setup();
    let modes = dev.mode_parameters.borrow();
    let partition = modes
        .iter()
        .find(|p| p.len() == 24 && p[8] == 0x11)
        .unwrap();
    // IBM GA32-0928-08 §6.6.1: byte3[6:4]=001, speed=0。
    assert_eq!(partition[3], 0x10);
    assert_eq!(&partition[6..8], &[0, 0]);
    assert_eq!(partition[11], 1);
    assert_eq!(partition[10], 3, "保留 MODE SENSE 的最大分区数");
    assert_eq!(partition[13], 3, "保留格式识别只读字段");
    assert_eq!(partition[14], 9, "PSUM=11 的单位指数必须是9");
    assert_eq!(&partition[16..20], &[0, 1, 0xff, 0xff]);
}

#[test]
fn reused_stream_buffer_keeps_short_records_and_hashes_exact() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    let block = vol.block_size() as usize;
    let files = [
        vec![0xa5; block + 17],
        vec![0x33; 1],
        vec![],
        vec![0x7e; block - 1],
    ];
    dev.writes.borrow_mut().clear();
    for (i, data) in files.iter().enumerate() {
        vol.append_file(&format!("f{i}"), &mut Cursor::new(data))
            .unwrap();
    }
    let lengths: Vec<_> = dev.writes.borrow().iter().map(|(_, n)| *n).collect();
    assert_eq!(lengths, [block, 17, 1, block - 1]);
    vol.commit().unwrap();
    let vol = LtfsVolume::mount(&dev).unwrap();
    for (i, data) in files.iter().enumerate() {
        let mut out = Vec::new();
        vol.read_file_to_writer(&format!("f{i}"), &mut out).unwrap();
        assert_eq!(&out, data);
        vol.verify_file(&format!("f{i}")).unwrap();
    }
}

#[test]
fn index_opening_filemark_is_immediate_but_closing_and_barrier_are_durable() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_bytes("file", b"content").unwrap();
    dev.commands.borrow_mut().clear();
    vol.sync().unwrap();
    let marks: Vec<_> = dev
        .commands
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x10)
        .cloned()
        .collect();
    assert_eq!(
        marks,
        [
            vec![0x10, 1, 0, 0, 1, 0],
            vec![0x10, 0, 0, 0, 1, 0],
            vec![0x10, 0, 0, 0, 0, 0]
        ]
    );
    assert_eq!(
        tape_rs::scsi::cdb::write_filemarks_immediate(0x123456),
        [0x10, 1, 0x12, 0x34, 0x56, 0]
    );
}

#[test]
fn mount_locates_eod_directly_without_space_or_bop_detour() {
    let (_lib, dev) = setup();
    let mut vol = LtfsVolume::mount(&dev).unwrap();
    vol.append_bytes("data", b"preserved").unwrap();
    vol.commit().unwrap();
    dev.commands.borrow_mut().clear();
    let vol = LtfsVolume::mount(&dev).unwrap();
    assert!(vol.writable());
    let commands = dev.commands.borrow();
    let eod: Vec<_> = commands
        .iter()
        .filter(|c| c[0] == 0x92 && c[1] & 0x38 == 0x18)
        .collect();
    assert_eq!(eod.len(), 2);
    for c in eod {
        assert_eq!(c[1], 0x1a);
        assert_eq!(&c[4..], &[0; 12]);
    }
    assert!(
        !commands
            .iter()
            .any(|c| c[0] == 0x92 && c[1] == 2 && c[4..12] == [0; 8])
    );
    assert!(!commands.iter().any(|c| c[0] == 0x11 && c[1] & 7 == 3));
    drop(commands);
    let mut data = Vec::new();
    vol.read_file_to_writer("data", &mut data).unwrap();
    assert_eq!(data, b"preserved");
}

#[test]
fn unsupported_eod_fails_mount_without_fallback() {
    let (_lib, dev) = setup();
    dev.reject_eod.set(true);
    dev.commands.borrow_mut().clear();
    assert!(matches!(
        LtfsVolume::mount(&dev),
        Err(tape_rs::error::TapeError::ScsiCommand {
            status: 2,
            sense_key: 5,
            asc: 0x24,
            ascq: 0,
        })
    ));
    let commands = dev.commands.borrow();
    assert_eq!(commands.last().unwrap()[1], 0x1a);
    assert!(
        !commands
            .iter()
            .any(|c| c[0] == 0x11 || c[0] == 0x0a || c[0] == 0x10)
    );
}

#[test]
fn reformat_from_data_partition_positions_at_partition_zero_first() {
    let (_lib, dev) = setup();
    tape_rs::tape::commands::TapeDrive::new(&dev)
        .locate(1, 0, true)
        .unwrap();
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "IOS001".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(LtfsVolume::mount(&dev).unwrap().writable());
}
