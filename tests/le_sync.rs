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
    short_write: Cell<bool>,
}
impl TapeTransport for Trace {
    fn execute_no_data(&self, c: &[u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
        self.inner.execute_no_data(c, t)
    }
    fn execute_read(&self, c: &[u8], b: &mut [u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
        self.inner.execute_read(c, b, t)
    }
    fn execute_write(&self, c: &[u8], b: &[u8], t: u32) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(c.to_vec());
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
        short_write: Cell::new(false),
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
