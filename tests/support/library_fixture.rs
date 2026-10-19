use std::path::PathBuf;
use tape_rs::library::{Library, LibraryDrive};
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::scsi::reservation::ReservationKey;
use tape_rs::scsi::sim::{SimCartridge, SimLibrary, SimTransport};
#[derive(Clone)]
pub struct SharedTransport(pub std::sync::Arc<SimTransport>);
impl tape_rs::scsi::transport::TapeTransport for SharedTransport {
    fn execute_no_data(
        &self,
        c: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_no_data(c, t)
    }
    fn execute_read(
        &self,
        c: &[u8],
        b: &mut [u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_read(c, b, t)
    }
    fn execute_write(
        &self,
        c: &[u8],
        b: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        self.0.execute_write(c, b, t)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.0.session_epoch()
    }
    fn identity(&self) -> &str {
        self.0.identity()
    }
}
pub struct ReservedChanger {
    changer: SharedTransport,
    pr: SharedTransport,
}
impl tape_rs::scsi::transport::TapeTransport for ReservedChanger {
    fn execute_no_data(
        &self,
        c: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0x5f {
            self.pr.execute_no_data(c, t)
        } else {
            self.changer.execute_no_data(c, t)
        }
    }
    fn execute_read(
        &self,
        c: &[u8],
        b: &mut [u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0x5e {
            self.pr.execute_read(c, b, t)
        } else {
            self.changer.execute_read(c, b, t)
        }
    }
    fn execute_write(
        &self,
        c: &[u8],
        b: &[u8],
        t: u32,
    ) -> tape_rs::error::Result<tape_rs::scsi::device::ScsiResult> {
        if c[0] == 0x5f {
            self.pr.execute_write(c, b, t)
        } else {
            self.changer.execute_write(c, b, t)
        }
    }
    fn identity(&self) -> &str {
        "reserved-test-changer"
    }
}
pub struct Fixture {
    pub sim: SimLibrary,
    changer_pr: SharedTransport,
    pub drive: std::sync::Arc<SimTransport>,
    pub changer: std::sync::Arc<SimTransport>,
    pub path: PathBuf,
    drive_count: usize,
}
impl Fixture {
    pub fn new() -> Self {
        Self::with_drive_count(1)
    }
    pub fn with_drive_count(drive_count: usize) -> Self {
        let sim = SimLibrary::new(drive_count, 3, 1);
        for (i, b) in ["LIB001L8", "LIB002L8"].iter().enumerate() {
            sim.insert_cartridge(SimCartridge::blank(b, 256 << 20), i)
                .unwrap();
        }
        let path = std::env::temp_dir().join(format!("tape-library-{}.db", uuid::Uuid::new_v4()));
        Self {
            drive: std::sync::Arc::new(sim.drive(0)),
            changer: std::sync::Arc::new(sim.changer()),
            sim,
            changer_pr: SharedTransport(std::sync::Arc::new(SimLibrary::new(1, 1, 0).drive(0))),
            path,
            drive_count,
        }
    }
    pub fn open(&self) -> Library<ReservedChanger, SharedTransport> {
        Library::open(
            ReservedChanger {
                changer: SharedTransport(self.changer.clone()),
                pr: self.changer_pr.clone(),
            },
            (0..self.drive_count)
                .map(|i| LibraryDrive {
                    offset: i as u16,
                    device: if i == 0 {
                        SharedTransport(self.drive.clone())
                    } else {
                        SharedTransport(std::sync::Arc::new(self.sim.drive(i)))
                    },
                })
                .collect(),
            &self.path,
            ReservationKey::new(1, 100),
        )
        .unwrap()
    }
    pub fn formatted(&self) -> Library<ReservedChanger, SharedTransport> {
        let mut lib = self.open();
        for b in ["LIB001L8", "LIB002L8"] {
            lib.assign("pool", b).unwrap();
            // 格式化只用于模拟器测试，先经调度选卷。
            // 以错误 mount 取得正确机械位置，随后显式模拟器格式化。
            let error = lib
                .inspect_volume(&format!("/pool/{b}"), |_, _| Ok(()))
                .unwrap_err();
            assert!(error.to_string().contains("sense_key=0x08"));
            mkltfs(
                self.drive.as_ref(),
                &MkltfsOptions {
                    volume_id: b[..6].into(),
                    block_size: 64 << 10,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        lib
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        let _ = std::fs::remove_file(self.path.with_extension("library-lock"));
    }
}
