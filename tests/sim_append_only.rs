//! 模拟真实 HH8 的 append-only 模式：DP追加成功，IP重写需一次精确位置授权。
use std::cell::{Cell, RefCell};
use tape_rs::error::{Result, TapeError};
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::reservation::{ReservationKey, fence};
use tape_rs::scsi::sim::{SimCartridge, SimLibrary, SimTransport};
use tape_rs::scsi::transport::TapeTransport;
use tape_rs::scsi::{cdb, device::ScsiResult};
use tape_rs::tape::commands::TapeDrive;
struct AppendOnly<'a> {
    inner: &'a SimTransport,
    permission: Cell<Option<(u8, u64)>>,
    allows: RefCell<Vec<Vec<u8>>>,
}
impl TapeTransport for AppendOnly<'_> {
    fn execute_no_data(&self, c: &[u8], timeout: u32) -> Result<ScsiResult> {
        if c[0] == 0x82 {
            let p = TapeDrive::new(self.inner).read_position()?;
            let block = u64::from_be_bytes(c[4..12].try_into().unwrap());
            assert_eq!(c[2], 1);
            assert_eq!((u32::from(c[3]), block), (p.partition, p.block_number));
            self.permission.set(Some((c[3], block)));
            self.allows.borrow_mut().push(c.to_vec());
            return self.inner.execute_no_data(&cdb::test_unit_ready(), timeout);
        }
        if c[0] == cdb::opcode::LOCATE_16 {
            self.permission.set(None);
        }
        if c[0] == cdb::opcode::WRITE_FILEMARKS_6 && c[2..5] != [0, 0, 0] {
            let p = TapeDrive::new(self.inner).read_position()?;
            if p.partition == 0 && p.block_number == 4 && self.permission.get() != Some((0, 4)) {
                return Err(TapeError::ScsiCommand {
                    status: 2,
                    sense_key: 7,
                    asc: 0x5a,
                    ascq: 2,
                });
            }
            self.permission.set(None);
        }
        self.inner.execute_no_data(c, timeout)
    }
    fn execute_read(&self, c: &[u8], b: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        if c[0] == 0x5a && c[2] == 0x10 && c[3] == 1 {
            b[..48].fill(0);
            b[1] = 46;
            b[7] = 8;
            b[16] = 0xd0;
            b[17] = 1;
            b[19] = 28;
            b[21] = 0x12;
            let mut good = self
                .inner
                .execute_no_data(&cdb::test_unit_ready(), timeout)?;
            good.transferred = 48;
            return Ok(good);
        }
        self.inner.execute_read(c, b, timeout)
    }
    fn execute_write(&self, c: &[u8], b: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_write(c, b, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}
fn setup() -> (SimLibrary, SimTransport) {
    let sim = SimLibrary::new(1, 1, 0);
    sim.insert_cartridge(SimCartridge::blank("APPENDL8", 64 << 20), 0)
        .unwrap();
    sim.load_into_drive("APPENDL8", 0).unwrap();
    let dev = sim.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "APPEND".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    (sim, dev)
}
#[test]
fn ip_checkpoint_in_append_only_mode_uses_one_position_bound_authorization() {
    let (_sim, dev) = setup();
    let key = ReservationKey::new(1, 10);
    fence(&dev, key).unwrap();
    let ao = AppendOnly {
        inner: &dev,
        permission: Cell::new(None),
        allows: RefCell::new(Vec::new()),
    };
    for i in 0..2 {
        let mut v = LtfsVolume::mount(&ao).unwrap();
        v.set_reservation_guard(Some(key));
        v.append_bytes(&format!("file{i}"), b"safe").unwrap();
        v.commit().unwrap();
        assert!(ao.permission.get().is_none());
    }
    let v = LtfsVolume::mount(&ao).unwrap();
    assert!(v.writable());
    assert!(!v.recovery().ip_debt);
    let mut out = Vec::new();
    v.read_file_to_writer("file1", &mut out).unwrap();
    assert_eq!(out, b"safe");
    assert_eq!(
        ao.allows.borrow().as_slice(),
        &[
            cdb::allow_overwrite_current(0, 4).to_vec(),
            cdb::allow_overwrite_current(0, 4).to_vec()
        ]
    );
}
#[test]
fn append_only_ip_override_is_refused_without_pr_guard() {
    let (_sim, dev) = setup();
    let ao = AppendOnly {
        inner: &dev,
        permission: Cell::new(None),
        allows: RefCell::new(Vec::new()),
    };
    let mut v = LtfsVolume::mount(&ao).unwrap();
    v.append_bytes("file", b"safe").unwrap();
    let error = v.commit().unwrap_err();
    assert!(error.to_string().contains("PR guard"));
    assert!(ao.allows.borrow().is_empty());
}

#[test]
fn authorization_refuses_wrong_position_and_does_not_retry_reset_ua() {
    let (_sim, dev) = setup();
    let ao = AppendOnly {
        inner: &dev,
        permission: Cell::new(None),
        allows: RefCell::new(Vec::new()),
    };
    TapeDrive::new(&dev).locate(0, 6, true).unwrap();
    assert!(TapeDrive::new(&ao).allow_overwrite_current(0, 4).is_err());
    assert!(ao.allows.borrow().is_empty());
    dev.inject(tape_rs::scsi::sim::Fault {
        opcode: cdb::opcode::READ_POSITION,
        action: tape_rs::scsi::sim::FaultAction::CheckCondition {
            sense_key: 6,
            asc: 0x29,
            ascq: 0,
        },
        after: 0,
        remaining: 1,
    });
    let error = TapeDrive::new(&ao)
        .allow_overwrite_current(0, 4)
        .unwrap_err();
    assert!(matches!(
        error,
        TapeError::ScsiCommand {
            sense_key: 6,
            asc: 0x29,
            ..
        }
    ));
    assert!(ao.allows.borrow().is_empty());
}
