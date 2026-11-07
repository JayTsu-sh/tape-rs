use super::*;
use crate::daemon::permission::{GuardedTransport, Phase};
use crate::scsi::device::ScsiResult;
use crate::scsi::sim::{SimCartridge, SimLibrary};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Trace {
    inner: GuardedTransport,
    commands: RefCell<Vec<u8>>,
    fail_after_format: Cell<bool>,
}

impl Trace {
    fn invoke(&self, cdb: &[u8], io: impl FnOnce() -> Result<ScsiResult>) -> Result<ScsiResult> {
        self.commands.borrow_mut().push(cdb[0]);
        let result = io();
        if result.is_ok() && cdb[0] == 0x04 && self.fail_after_format.replace(false) {
            return Err(TapeError::ScsiHost(3));
        }
        result
    }

    fn formats(&self) -> usize {
        self.commands
            .borrow()
            .iter()
            .filter(|op| **op == 0x04)
            .count()
    }
}

impl TapeTransport for Trace {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(cdb, || self.inner.execute_no_data(cdb, timeout))
    }
    fn execute_read(&self, cdb: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(cdb, || self.inner.execute_read(cdb, data, timeout))
    }
    fn execute_write(&self, cdb: &[u8], data: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.invoke(cdb, || self.inner.execute_write(cdb, data, timeout))
    }
}

fn fixture() -> (SimLibrary, Arc<Permission>, ReservationKey, Trace) {
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("INIT01L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("INIT01L8", 0).unwrap();
    let raw = library.drive_as(0, 71);
    let key = ReservationKey::new(71, 12);
    crate::scsi::reservation::fence(&raw, key).unwrap();
    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        12,
        Phase::Execution,
        Instant::now() + Duration::from_secs(60)
    ));
    let device = Trace {
        inner: GuardedTransport::new(Box::new(raw), permission.clone(), 12),
        commands: RefCell::new(Vec::new()),
        fail_after_format: Cell::new(false),
    };
    (library, permission, key, device)
}

#[test]
fn initialization_and_observation_share_one_original_format() {
    let (_lib, permission, key, device) = fixture();
    let expected = uuid::Uuid::new_v4();
    let mut attempted = false;
    let first = initialize_empty(
        &device,
        &permission,
        key,
        expected,
        "INIT01L8",
        &mut attempted,
    )
    .unwrap();
    assert!(attempted);
    assert_eq!(first.volume_uuid, expected);
    assert_eq!(first.generation, 1);
    assert!(!first.incremental);
    assert_eq!(device.formats(), 1);
    let observed = initialize_empty(
        &device,
        &permission,
        key,
        expected,
        "INIT01L8",
        &mut attempted,
    )
    .unwrap();
    assert_eq!(observed, first);
    assert_eq!(device.formats(), 1);
}

#[test]
fn unknown_format_and_partial_labels_never_authorize_a_second_format() {
    let (_lib, permission, key, device) = fixture();
    device.fail_after_format.set(true);
    let expected = uuid::Uuid::new_v4();
    let mut attempted = false;
    assert!(
        initialize_empty(
            &device,
            &permission,
            key,
            expected,
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    assert!(attempted);
    assert_eq!(device.formats(), 1);
    let before = device.commands.borrow().len();
    assert!(
        initialize_empty(
            &device,
            &permission,
            key,
            expected,
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    assert_eq!(device.formats(), 1);
    assert!(
        !device.commands.borrow()[before..]
            .iter()
            .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x8d | 0x55))
    );
}

#[test]
fn wrong_uuid_and_nonempty_result_remain_observation_only() {
    let (_lib, permission, key, device) = fixture();
    let expected = uuid::Uuid::new_v4();
    let mut attempted = false;
    initialize_empty(
        &device,
        &permission,
        key,
        expected,
        "INIT01L8",
        &mut attempted,
    )
    .unwrap();
    assert!(
        initialize_empty(
            &device,
            &permission,
            key,
            uuid::Uuid::new_v4(),
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    let mut volume = crate::ltfs::volume::LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    volume
        .append_file("keep", &mut std::io::Cursor::new(b"original"))
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    assert!(
        initialize_empty(
            &device,
            &permission,
            key,
            expected,
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    assert_eq!(device.formats(), 1);
    let mut body = Vec::new();
    crate::ltfs::volume::LtfsVolume::mount(&device)
        .unwrap()
        .read_file_to_writer("keep", &mut body)
        .unwrap();
    assert_eq!(body, b"original");
}

#[test]
fn initialization_requires_the_exact_guarded_permission_instance() {
    let (lib, permission, key, device) = fixture();
    let other = Permission::default();
    assert!(other.renew(
        12,
        Phase::Execution,
        Instant::now() + Duration::from_secs(60)
    ));
    let expected = uuid::Uuid::new_v4();
    let mut attempted = false;
    assert!(initialize_empty(&device, &other, key, expected, "INIT01L8", &mut attempted).is_err());
    assert!(device.commands.borrow().is_empty());
    assert!(!attempted);
    assert!(
        initialize_empty(
            &lib.drive_as(0, 71),
            &permission,
            key,
            expected,
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    assert!(!attempted);
    assert!(
        initialize_empty(
            &device,
            &permission,
            key,
            uuid::Uuid::nil(),
            "INIT01L8",
            &mut attempted
        )
        .is_err()
    );
    assert!(device.commands.borrow().is_empty());
}
