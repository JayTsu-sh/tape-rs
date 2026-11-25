use super::*;
use crate::scsi::{cdb, device::ScsiResult};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug)]
pub(super) enum Fault {
    BodySha,
    TailLabel,
    TailLayout,
    SourceReservation,
    SourceReservationUa,
}

#[derive(Clone, Copy)]
pub(super) struct Case {
    pub(super) files: u8,
    pub(super) fault: Option<Fault>,
    pub(super) target_preflight: Option<TargetPreflightCase>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetPreflightCase {
    Overlap,
    ChangedAppendPosition,
    ChangeToken,
}

/// The source body read must overlap the target's real mount commands.
/// The timeout makes a serialized implementation fail without hanging the test.
#[derive(Default)]
pub(super) struct EarlyOverlap {
    reached: Mutex<[bool; 2]>,
    changed: std::sync::Condvar,
}

impl EarlyOverlap {
    fn read(&self, drive: usize) -> Result<()> {
        let mut reached = self.reached.lock().unwrap();
        reached[drive] = true;
        self.changed.notify_all();
        let (final_state, _) = self
            .changed
            .wait_timeout_while(reached, std::time::Duration::from_secs(15), |state| {
                !state[1 - drive]
            })
            .unwrap();
        if final_state[1 - drive] {
            Ok(())
        } else {
            Err(TapeError::NotReady("目标预检与源正文未重叠".into()))
        }
    }

    pub(super) fn verify(&self) {
        assert_eq!(*self.reached.lock().unwrap(), [true, true]);
    }
}

pub(super) struct TargetTrace {
    pub(super) inner: ParentSourceTrace,
    pub(super) overlap: Arc<EarlyOverlap>,
    pub(super) position_changed: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl TargetTrace {
    fn during_mount_or_handoff(&self, test: impl FnOnce(&[(usize, Vec<u8>)]) -> bool) -> bool {
        let trace = self.inner.commands.lock().unwrap();
        let mount = trace
            .iter()
            .enumerate()
            .filter(|(_, (drive, command))| {
                *drive == self.inner.drive && *command == cdb::locate_eod_16(0)
            })
            .nth(1)
            .map(|(at, _)| at);
        mount.is_some_and(|mount| test(&trace[mount..]))
    }
}

impl TapeTransport for TargetTrace {
    fn execute_no_data(&self, command: &[u8], timeout: u32) -> Result<ScsiResult> {
        let result = self.inner.execute_no_data(command, timeout)?;
        if command == cdb::locate_eod_16(0) && self.during_mount_or_handoff(|_| true) {
            assert_eq!(
                self.inner.medium_change_token().is_some(),
                self.inner.trusted_change_evidence
            );
            self.overlap.read(1)?;
        }
        Ok(result)
    }

    fn execute_read(&self, command: &[u8], buffer: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        let result = self.inner.execute_read(command, buffer, timeout)?;
        if command == cdb::read_position()
            && let Some(changed) = &self.position_changed
            && self.during_mount_or_handoff(|trace| {
                trace.iter().any(|(drive, command)| {
                    *drive == self.inner.drive && *command == cdb::test_unit_ready()
                })
            })
            && !changed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            assert!(result.transferred >= 20);
            buffer[7] ^= 1;
        }
        Ok(result)
    }

    fn execute_write(&self, command: &[u8], buffer: &[u8], timeout: u32) -> Result<ScsiResult> {
        let before = self.inner.medium_change_token();
        let result = self.inner.execute_write(command, buffer, timeout)?;
        if command[0] == cdb::opcode::WRITE_6 && self.inner.trusted_change_evidence {
            assert!(before.is_some());
            assert_ne!(self.inner.medium_change_token(), before);
        }
        Ok(result)
    }

    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.inner.medium_change_token()
    }

    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }

    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

#[derive(Default)]
pub(super) struct Trace {
    label_locates: Vec<usize>,
    body_start: Option<usize>,
    last_body_read: Option<usize>,
    last_label_read: Option<usize>,
    label_read: bool,
    bodies: Vec<(u8, usize)>,
    read_sizes: Vec<(usize, usize)>,
    pending_pr_ua: bool,
    injected_pr_ua: Option<usize>,
    pub(super) fired: bool,
}

impl Trace {
    pub(super) fn report_counts(
        &self,
        commands: &[(usize, Vec<u8>)],
        drive: usize,
        tick_start: usize,
    ) {
        let counts = |phase: &str, start: usize, end: usize| {
            let mut ordinary = 0;
            let mut eod = 0;
            let mut space = 0;
            let mut reads = 0;
            let mut switches = 0;
            let mut last_partition = None;
            for (_, command) in commands[start..end].iter().filter(|(at, _)| *at == drive) {
                match command[0] {
                    cdb::opcode::LOCATE_16 => {
                        if command[1] & 0x18 == 0x18 {
                            eod += 1;
                        } else {
                            ordinary += 1;
                        }
                        if last_partition
                            .replace(command[3])
                            .is_some_and(|p| p != command[3])
                        {
                            switches += 1;
                        }
                    }
                    0x11 => space += 1,
                    cdb::opcode::READ_6 => reads += 1,
                    _ => {}
                }
            }
            let transferred = self
                .read_sizes
                .iter()
                .filter(|(at, _)| (start..end).contains(at))
                .map(|(_, bytes)| bytes)
                .sum::<usize>();
            eprintln!(
                "SOURCE_GROUP_CDB phase={phase} locate={ordinary} locate_eod={eod} partition_switches={switches} space={space} read={reads} transferred_bytes={transferred}"
            );
        };
        counts("whole-copy-tick", tick_start, commands.len());
        if self.label_locates.len() >= 3
            && let Some(body) = self.body_start
            && let (Some(last_body), Some(last_label)) = (self.last_body_read, self.last_label_read)
        {
            let a = self.label_locates[1];
            counts("capture-a", a, body);
            counts("body", body, last_body + 1);
            counts("capture-b", last_body + 1, last_label + 1);
        }
    }

    pub(super) fn verify_order(&self, commands: &[(usize, Vec<u8>)], drive: usize, files: u8) {
        let start = self.body_start.expect("actual source body seek");
        let last_body = self.last_body_read.expect("actual source body READ");
        let last_label = self.last_label_read.expect("actual source B label READ");
        let source_locates: Vec<_> = commands[start..=last_body]
            .iter()
            .filter(|(device, cdb)| *device == drive && cdb[0] == cdb::opcode::LOCATE_16)
            .map(|(_, cdb)| cdb[3])
            .collect();
        assert_eq!(source_locates, [1], "short source gaps must stream in P1");
        let tail_locates: Vec<_> = commands[last_body + 1..=last_label]
            .iter()
            .filter(|(device, cdb)| *device == drive && cdb[0] == cdb::opcode::LOCATE_16)
            .map(|(_, cdb)| cdb.as_slice())
            .collect();
        assert_eq!(
            tail_locates.first().map(|cdb| cdb[3]),
            Some(1),
            "source B must inspect the current DP first"
        );
        let dp_eod = tail_locates
            .iter()
            .position(|&command| command == cdb::locate_eod_16(1))
            .expect("source B must probe DP EOD");
        let ip_eod = tail_locates
            .iter()
            .position(|&command| command == cdb::locate_eod_16(0))
            .expect("source B must probe IP EOD");
        assert!(dp_eod < ip_eod);
        assert!(tail_locates[ip_eod..].iter().all(|cdb| cdb[3] == 0));
        assert_eq!(
            tail_locates.last().copied(),
            Some(cdb::locate_16(0, 2, true, false).as_slice()),
            "source B must reread its physical label during the final P0 visit"
        );
        let mut bodies = Vec::<(u8, usize)>::new();
        for (file, bytes) in &self.bodies {
            match bodies.last_mut() {
                Some((last, size)) if last == file => *size += bytes,
                _ => bodies.push((*file, *bytes)),
            }
        }
        assert_eq!(
            bodies,
            (37..37 + files)
                .map(|file| (file, 1 << 20))
                .collect::<Vec<_>>(),
            "source bytes must follow physical order, reverse of key order"
        );
        eprintln!(
            "SOURCE_GROUP_PHYSICAL bodies={bodies:?} between_endpoints_source_locate_partitions={source_locates:?}"
        );
    }
}

pub(super) struct SourceTrace {
    pub(super) inner: ParentSourceTrace,
    pub(super) trace: Arc<Mutex<Trace>>,
    pub(super) lib: SimLibrary,
    pub(super) fault: Option<Fault>,
    pub(super) source_volume: uuid::Uuid,
    pub(super) first_block: u64,
    pub(super) binding_key: Option<Vec<u8>>,
    pub(super) cluster: String,
    pub(super) overlap: Option<Arc<EarlyOverlap>>,
}

impl TapeTransport for SourceTrace {
    fn execute_no_data(&self, cdb_bytes: &[u8], timeout: u32) -> Result<ScsiResult> {
        let index = self.inner.commands.lock().unwrap().len();
        let mut trace = self.trace.lock().unwrap();
        if cdb_bytes == cdb::locate_16(0, 2, true, false) {
            trace.label_locates.push(index);
            trace.label_read = true;
        } else if trace.label_locates.len() == 2
            && cdb_bytes == cdb::locate_16(1, self.first_block, true, false)
        {
            assert!(trace.body_start.replace(index).is_none());
        }
        drop(trace);
        self.inner.execute_no_data(cdb_bytes, timeout)
    }

    fn execute_read(
        &self,
        cdb_bytes: &[u8],
        buffer: &mut [u8],
        timeout: u32,
    ) -> Result<ScsiResult> {
        if cdb_bytes[0] == cdb::opcode::READ_6
            && let Some(overlap) = &self.overlap
            && self.trace.lock().unwrap().body_start.is_some()
        {
            overlap.read(0)?;
        }
        if cdb_bytes[0] == cdb::opcode::PERSISTENT_RESERVE_IN {
            let mut trace = self.trace.lock().unwrap();
            if std::mem::take(&mut trace.pending_pr_ua) {
                trace.fired = true;
                let mut commands = self.inner.commands.lock().unwrap();
                trace.injected_pr_ua = Some(commands.len());
                commands.push((self.inner.drive, cdb_bytes.to_vec()));
                drop(commands);
                let mut raw = [0; 18];
                raw[0] = 0x70;
                raw[2] = 6;
                raw[7] = 10;
                raw[12] = 0x28;
                return crate::scsi::device::finish_command(
                    2,
                    crate::scsi::sense::SenseInfo::from_bytes(&raw),
                    0,
                    0,
                    self.identity(),
                );
            }
        }
        let completion = self.inner.execute_read(cdb_bytes, buffer, timeout)?;
        if cdb_bytes[0] != cdb::opcode::READ_6 {
            return Ok(completion);
        }
        let mut trace = self.trace.lock().unwrap();
        let read_at = self
            .inner
            .commands
            .lock()
            .unwrap()
            .iter()
            .rposition(|(drive, _)| *drive == self.inner.drive)
            .expect("recorded source READ");
        trace.read_sizes.push((read_at, completion.transferred));
        let is_label = std::mem::take(&mut trace.label_read);
        if is_label && trace.label_locates.len() == 3 {
            trace.last_label_read = Some(read_at);
        }
        if is_label
            && trace.label_locates.len() == 3
            && matches!(self.fault, Some(Fault::TailLabel))
        {
            // Keep valid XML/UUID, but return another medium's identity from B.
            let uuid = self.source_volume.to_string();
            let pos = buffer[..completion.transferred]
                .windows(uuid.len())
                .position(|bytes| bytes == uuid.as_bytes())
                .expect("actual LTFS source volume UUID");
            buffer[pos] = if buffer[pos] == b'0' { b'1' } else { b'0' };
            trace.fired = true;
        }
        if trace.body_start.is_some()
            && trace.label_locates.len() == 2
            && completion.transferred >= 64 << 10
            && (37..=44).contains(&buffer[0])
            && buffer[..completion.transferred]
                .iter()
                .all(|byte| *byte == buffer[0])
        {
            trace.bodies.push((buffer[0], completion.transferred));
            trace.last_body_read = Some(read_at);
            if buffer[0] == 38 && !trace.fired {
                match self.fault {
                    Some(Fault::BodySha) => {
                        buffer[0] ^= 0x80;
                        trace.fired = true;
                    }
                    Some(Fault::SourceReservation) => {
                        self.lib.reset_drive(self.inner.drive);
                        trace.fired = true;
                    }
                    Some(Fault::SourceReservationUa) => trace.pending_pr_ua = true,
                    _ => {}
                }
            }
        }
        Ok(completion)
    }

    fn execute_write(&self, cdb: &[u8], buffer: &[u8], timeout: u32) -> Result<ScsiResult> {
        self.inner.execute_write(cdb, buffer, timeout)
    }

    fn session_epoch(&self) -> Option<u64> {
        let mut trace = self.trace.lock().unwrap();
        if trace.label_locates.len() == 3
            && !trace.label_read
            && !trace.fired
            && matches!(self.fault, Some(Fault::TailLayout))
        {
            // The first epoch snapshot after B precedes the final FDB member
            // checks. Remove one real layout binding in this test namespace.
            let db = foundationdb::Database::from_path(&self.cluster).unwrap();
            let tr = db.create_trx().unwrap();
            let key = self.binding_key.as_ref().unwrap();
            futures_executor::block_on(async {
                assert!(tr.get(key, false).await.unwrap().is_some());
                tr.clear(key);
                tr.commit().await.unwrap();
            });
            trace.fired = true;
        }
        self.inner.session_epoch()
    }

    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }

    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

pub(super) struct FaultSetup {
    fault: Fault,
    source_records: Vec<(Vec<u8>, Vec<u8>)>,
    pub(super) binding_key: Option<Vec<u8>>,
}

impl FaultSetup {
    pub(super) async fn new(
        catalog: &crate::managed_catalog::ManagedCatalog,
        cluster: &str,
        source_volume: uuid::Uuid,
        fault: Fault,
    ) -> Self {
        let copies = catalog
            .copies_on_volume(source_volume, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(copies.len(), 3);
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        let mut source_records = Vec::new();
        for copy in &copies {
            let key = [
                b"tape-rs/catalog/22/R/".as_slice(),
                copy.namespace.as_bytes(),
                copy.file_id.as_bytes(),
                copy.copy_id.as_bytes(),
            ]
            .concat();
            let raw = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            source_records.push((key, raw));
        }
        let binding_key = if matches!(fault, Fault::TailLayout) {
            let copy = &copies[0];
            let key = [
                b"tape-rs/catalog/22/\x80/".as_slice(),
                copy.namespace.as_bytes(),
                &[1],
                copy.file_id.as_bytes(),
                copy.copy_id.as_bytes(),
            ]
            .concat();
            assert!(tr.get(&key, false).await.unwrap().is_some());
            Some(key)
        } else {
            None
        };
        Self {
            fault,
            source_records,
            binding_key,
        }
    }

    pub(super) async fn verify(
        &self,
        catalog: &crate::managed_catalog::ManagedCatalog,
        cluster: &str,
        parent: &crate::managed_catalog::ReclaimReservation,
        owner: u64,
        source_volume: uuid::Uuid,
        trace: &Mutex<Trace>,
        commands: &Mutex<Vec<(usize, Vec<u8>)>>,
        result: &std::result::Result<bool, ReadError>,
    ) {
        let message = result.as_ref().unwrap_err().to_string();
        let (labels, source_start, injected_pr_ua) = {
            let trace = trace.lock().unwrap();
            let expected = match self.fault {
                Fault::BodySha => "源实读长度、SHA256",
                Fault::TailLabel => "介质标签与文件副本不一致",
                Fault::TailLayout => "布局",
                Fault::SourceReservation => "执行资格已失去",
                Fault::SourceReservationUa => "sense_key=0x06, asc=0x28, ascq=0x00",
            };
            assert!(
                message.contains(expected),
                "unexpected source fault: {message}"
            );
            let labels = match self.fault {
                Fault::TailLabel | Fault::TailLayout => 3,
                _ => 2,
            };
            assert_eq!(trace.label_locates.len(), labels);
            assert!(trace.body_start.is_some());
            (labels, trace.label_locates[1], trace.injected_pr_ua)
        };
        if let Some(index) = injected_pr_ua {
            let commands = commands.lock().unwrap();
            let (source_drive, injected) = &commands[index];
            assert_eq!(injected[0], cdb::opcode::PERSISTENT_RESERVE_IN);
            assert!(
                commands[index + 1..]
                    .iter()
                    .all(|(drive, _)| drive != source_drive),
                "source drive issued another CDB after PR IN reported UA"
            );
        }
        assert!(
            commands.lock().unwrap()[source_start..]
                .iter()
                .all(|(_, cdb)| { !matches!(cdb[0], 0x0a | 0x10 | 0x8d | 0x04) }),
            "source group failure issued a write, filemark, MAM write or format"
        );
        let retained = catalog
            .resume_reclaim_copy_group(parent, owner)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.source_ready());
        assert!(!retained.prepared() && !retained.requires_observation());
        assert_eq!(retained.files().count(), self.source_records.len());
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        for (key, raw) in &self.source_records {
            assert_eq!(
                tr.get(key, false).await.unwrap().as_deref(),
                Some(raw.as_slice())
            );
        }
        let copies = catalog
            .copies_on_volume(source_volume, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(copies.len(), self.source_records.len());
        eprintln!(
            "SOURCE_GROUP_FAULT fault={:?} labels={labels} source_records={} original_group={} child_count=0 write_cdbs=0",
            self.fault,
            copies.len(),
            retained.id()
        );
    }
}

async fn run(case: Case) {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        None,
        false,
        true,
        false,
        false,
        Some(case),
        false,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；同一组端点开销适用于三个物理顺序文件"]
async fn public_group_only_three_files_use_two_source_endpoint_scans() {
    Box::pin(run(Case {
        files: 3,
        fault: None,
        target_preflight: None,
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；源组第二成员坏SHA，拒绝原组准入与写带"]
async fn public_group_only_source_body_sha_failure_keeps_original_group_unstarted() {
    Box::pin(run(Case {
        files: 3,
        fault: Some(Fault::BodySha),
        target_preflight: None,
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；源组B卷标与原卷不同，拒绝原组准入与写带"]
async fn public_group_only_source_tail_label_failure_keeps_original_group_unstarted() {
    Box::pin(run(Case {
        files: 3,
        fault: Some(Fault::TailLabel),
        target_preflight: None,
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；源组B之后清除原布局，拒绝原组准入与写带"]
async fn public_group_only_source_tail_fdb_layout_change_keeps_original_group_unstarted() {
    Box::pin(run(Case {
        files: 3,
        fault: Some(Fault::TailLayout),
        target_preflight: None,
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；源组成员间实际PR丢失，拒绝原组准入与写带"]
async fn public_group_only_source_reservation_loss_keeps_original_group_unstarted() {
    Box::pin(run(Case {
        files: 3,
        fault: Some(Fault::SourceReservation),
        target_preflight: None,
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；源组成员间PR IN报告UA，原组不得继续读或准入"]
async fn public_group_only_source_pr_ua_keeps_original_group_unstarted() {
    Box::pin(run(Case {
        files: 3,
        fault: Some(Fault::SourceReservationUa),
        target_preflight: None,
    }))
    .await;
}
