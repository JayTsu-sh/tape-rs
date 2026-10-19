//! 性能探针专用：原样转发命令，按阶段聚合；不添加设备查询或重试。
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::time::Instant;

use serde_json::{Value, json};
use tape_rs::error::Result;
use tape_rs::scsi::device::ScsiResult;
use tape_rs::scsi::transport::TapeTransport;

#[derive(Default)]
struct CommandStats {
    calls: u64,
    errors: u64,
    requested_bytes: u64,
    successful_transferred_bytes: u64,
    seconds: f64,
    max_seconds: f64,
}

pub struct MeasuredTransport<'a> {
    inner: &'a dyn TapeTransport,
    stats: RefCell<BTreeMap<String, CommandStats>>,
    enabled: bool,
    reports: RefCell<Vec<Value>>,
}

impl<'a> MeasuredTransport<'a> {
    pub fn new(inner: &'a dyn TapeTransport, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            reports: RefCell::new(Vec::new()),
            stats: RefCell::new(BTreeMap::new()),
        }
    }

    /// 阶段不可嵌套；无逐 CDB 输出，失败也先输出该阶段的测量和原始错误。
    pub fn phase<T, E: Display>(
        &self,
        name: &str,
        f: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if !self.enabled {
            return f();
        }
        self.stats.borrow_mut().clear();
        let start = Instant::now();
        let result = f();
        let wall_seconds = start.elapsed().as_secs_f64();
        self.reports
            .borrow_mut()
            .push(self.report(name, wall_seconds, result.as_ref().err()));
        if result.is_err() {
            self.emit();
        }
        result
    }

    pub fn emit(&self) {
        for report in self.reports.borrow_mut().drain(..) {
            println!("{report}");
        }
    }

    fn report<E: Display>(&self, name: &str, wall_seconds: f64, error: Option<&E>) -> Value {
        let stats = self.stats.borrow();
        let transport_seconds: f64 = stats.values().map(|s| s.seconds).sum();
        let commands: Vec<_> = stats
            .iter()
            .map(|(command, s)| {
                json!({
                    "command":command,"calls":s.calls,"errors":s.errors,
                    "requested_bytes":s.requested_bytes,
                    "successful_transferred_bytes":s.successful_transferred_bytes,
                    "seconds":s.seconds,"max_seconds":s.max_seconds,
                })
            })
            .collect();
        json!({"kind":"phase","phase":name,"ok":error.is_none(),
            "error":error.map(ToString::to_string),"wall_seconds":wall_seconds,
            "transport_seconds":transport_seconds,
            "outside_transport_seconds":(wall_seconds-transport_seconds).max(0.0),
            "commands":commands})
    }

    fn execute(
        &self,
        cdb: &[u8],
        direction: &str,
        requested: usize,
        f: impl FnOnce() -> Result<ScsiResult>,
    ) -> Result<ScsiResult> {
        if !self.enabled {
            return f();
        }
        let start = Instant::now();
        let result = f();
        let seconds = start.elapsed().as_secs_f64();
        let mut command = format!("{:02x}/{direction}", cdb.first().copied().unwrap_or(0));
        // 区分 opening/closing FM 与零计数屏障，不把它们合成一个耗时。
        if cdb.first() == Some(&0x10) && cdb.len() >= 5 {
            let count = u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]);
            command.push_str(&format!("/immed={}/count={count}", cdb[1] & 1));
        }
        let mut stats = self.stats.borrow_mut();
        let s = stats.entry(command).or_default();
        s.calls += 1;
        s.errors += u64::from(result.is_err());
        s.requested_bytes += requested as u64;
        if let Ok(r) = &result {
            s.successful_transferred_bytes += r.transferred as u64;
        }
        s.seconds += seconds;
        s.max_seconds = s.max_seconds.max(seconds);
        result
    }
}

impl TapeTransport for MeasuredTransport<'_> {
    fn execute_no_data(&self, cdb: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.execute(cdb, "none", 0, || {
            self.inner.execute_no_data(cdb, timeout_ms)
        })
    }

    fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.execute(cdb, "read", buf.len(), || {
            self.inner.execute_read(cdb, buf, timeout_ms)
        })
    }

    fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout_ms: u32) -> Result<ScsiResult> {
        self.execute(cdb, "write", buf.len(), || {
            self.inner.execute_write(cdb, buf, timeout_ms)
        })
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }

    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_rs::error::TapeError;
    use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use tape_rs::ltfs::volume::LtfsVolume;
    use tape_rs::scsi::sense::SenseInfo;
    use tape_rs::scsi::sim::{SimCartridge, SimLibrary};

    #[derive(Default)]
    struct RecordingTransport {
        calls: RefCell<Vec<(Vec<u8>, Vec<u8>, u32)>>,
    }

    impl TapeTransport for RecordingTransport {
        fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<ScsiResult> {
            self.calls
                .borrow_mut()
                .push((cdb.to_vec(), vec![], timeout));
            Err(TapeError::ScsiCommand {
                status: 2,
                sense_key: 6,
                asc: 0x2a,
                ascq: 5,
            })
        }
        fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> Result<ScsiResult> {
            self.calls
                .borrow_mut()
                .push((cdb.to_vec(), vec![0; buf.len()], timeout));
            buf[0] = 73;
            Ok(ScsiResult {
                status: 0,
                sense: SenseInfo::from_bytes(&[]),
                transferred: 1,
                duration_ms: 7,
            })
        }
        fn execute_write(&self, cdb: &[u8], buf: &[u8], timeout: u32) -> Result<ScsiResult> {
            self.calls
                .borrow_mut()
                .push((cdb.to_vec(), buf.to_vec(), timeout));
            Ok(ScsiResult {
                status: 0,
                sense: SenseInfo::from_bytes(&[]),
                transferred: buf.len(),
                duration_ms: 8,
            })
        }
        fn session_epoch(&self) -> Option<u64> {
            Some(42)
        }
        fn identity(&self) -> &str {
            "recording"
        }
    }

    #[test]
    fn preserves_commands_buffers_timeouts_errors_and_epoch() {
        let inner = RecordingTransport::default();
        let measured = MeasuredTransport::new(&inner, true);
        let mut buf = [0; 9];
        assert_eq!(
            measured
                .execute_read(&[0x08, 0, 1], &mut buf, 123)
                .unwrap()
                .transferred,
            1
        );
        assert_eq!(buf[0], 73);
        assert_eq!(
            measured
                .execute_write(&[0x0a, 0, 2], &[5, 6], 456)
                .unwrap()
                .duration_ms,
            8
        );
        assert!(matches!(
            measured.execute_no_data(&[0x10, 0, 0, 0, 0, 0], 789),
            Err(TapeError::ScsiCommand {
                sense_key: 6,
                asc: 0x2a,
                ascq: 5,
                ..
            })
        ));
        assert_eq!(
            &*inner.calls.borrow(),
            &[
                (vec![0x08, 0, 1], vec![0; 9], 123),
                (vec![0x0a, 0, 2], vec![5, 6], 456),
                (vec![0x10, 0, 0, 0, 0, 0], vec![], 789),
            ]
        );
        assert_eq!(measured.session_epoch(), Some(42));
        assert_eq!(measured.identity(), "recording");
        let stats = measured.stats.borrow();
        assert_eq!(stats["08/read"].successful_transferred_bytes, 1);
        assert_eq!(stats["08/read"].requested_bytes, 9);
        assert_eq!(stats["10/none/immed=0/count=0"].errors, 1);
    }

    #[test]
    fn failed_phase_is_reported_without_retry_and_disabled_profile_is_transparent() {
        let inner = RecordingTransport::default();
        let measured = MeasuredTransport::new(&inner, true);
        let result = measured.phase("failed_barrier", || {
            measured.execute_no_data(&[0x10, 0, 0, 0, 0, 0], 12)
        });
        assert!(result.is_err());
        assert_eq!(inner.calls.borrow().len(), 1);
        let report = measured.report("failed_barrier", 1.0, result.as_ref().err());
        assert_eq!(report["ok"], false);
        assert!(
            report["error"].as_str().unwrap().contains("2A")
                || report["error"].as_str().unwrap().contains("2a")
        );
        let disabled = MeasuredTransport::new(&inner, false);
        disabled
            .phase("write", || disabled.execute_write(&[0x0a], &[1], 34))
            .unwrap();
        assert!(disabled.stats.borrow().is_empty());
        assert!(disabled.reports.borrow().is_empty());
    }

    #[test]
    fn simulated_64mib_roundtrip_separates_data_sync_checkpoint_and_remount() {
        use sha2::{Digest, Sha256};
        let lib = SimLibrary::new(1, 4, 1);
        lib.insert_cartridge(SimCartridge::blank("SIM064L8", 512 << 20), 0)
            .unwrap();
        lib.load_into_drive("SIM064L8", 0).unwrap();
        let inner = lib.drive(0);
        mkltfs(&inner, &MkltfsOptions::default()).unwrap();
        let measured = MeasuredTransport::new(&inner, true);
        let mut vol = measured
            .phase("mount", || LtfsVolume::mount(&measured))
            .unwrap();
        let data: Vec<u8> = (0..64 * 1024 * 1024)
            .map(|n| (n as u8).wrapping_mul(31))
            .collect();
        let expected = Sha256::digest(&data);
        measured
            .phase("data_write", || {
                vol.append_file("comparison/perf64.bin", &mut std::io::Cursor::new(&data))
            })
            .unwrap();
        let stats = measured.stats.borrow();
        assert_eq!(stats["0a/write"].calls, 128);
        assert_eq!(stats["0a/write"].successful_transferred_bytes, 64 << 20);
        drop(stats);
        measured.phase("dp_sync", || vol.sync()).unwrap();
        assert!(vol.needs_checkpoint());
        assert_eq!(measured.stats.borrow()["10/none/immed=0/count=0"].calls, 1);
        measured.phase("checkpoint", || vol.commit()).unwrap();
        assert!(!vol.needs_checkpoint());
        drop(vol);
        let vol = measured
            .phase("fresh_mount", || LtfsVolume::mount(&measured))
            .unwrap();
        for label in ["first", "repeat"] {
            let mut out = Vec::new();
            measured
                .phase(label, || {
                    vol.read_file_to_writer("comparison/perf64.bin", &mut out)
                })
                .unwrap();
            assert_eq!(Sha256::digest(&out), expected);
            assert_eq!(measured.stats.borrow()["08/read"].calls, 128);
        }
        drop(vol);
        measured
            .phase("rewind", || {
                tape_rs::tape::commands::TapeDrive::new(&measured).rewind()
            })
            .unwrap();
        println!(
            "{}",
            json!({"kind":"environment","simulated":true,"hardware_performance":false,"bytes":data.len()})
        );
        measured.emit();
    }
}
