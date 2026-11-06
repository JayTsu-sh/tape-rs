//! All probes run on the existing fenced owner, between foreground requests.
//! Never LOAD, position, MOVE, write, or mount a cartridge just for monitoring.
use super::*;
use crate::ltfs::mam::{ATTR_MAXIMUM_CAPACITY, ATTR_REMAINING_CAPACITY, Mam};
use crate::managed_catalog::{TapeCapacity, TapeMonitorError, TapeMonitorSample};
use std::sync::Mutex;

const INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct Schedule {
    next: Option<Instant>,
    pending: Vec<usize>,
}

/// Captures the actual failing opcode, rather than guessing from an outer
/// upload/read label (mount and commit also issue MAM and positioning CDBs).
pub(super) struct Tracked<'a> {
    device: &'a dyn TapeTransport,
    operation: &'a str,
    error: Mutex<Option<TapeMonitorError>>,
}
impl<'a> Tracked<'a> {
    pub fn new(device: &'a dyn TapeTransport, operation: &'a str) -> Self {
        Self {
            device,
            operation,
            error: Mutex::new(None),
        }
    }
    fn returned(&self, cdb: &[u8], result: &Result<crate::scsi::device::ScsiResult>) {
        if let Err(error) = result {
            // Reservation loss belongs to the execution fence, not the tape.
            if ownership_lost(error) {
                return;
            }
            let (status, sense_key, asc, ascq) = match error {
                TapeError::ScsiCommand {
                    status,
                    sense_key,
                    asc,
                    ascq,
                    ..
                } => (Some(*status), Some(*sense_key), Some(*asc), Some(*ascq)),
                _ => (None, None, None, None),
            };
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(TapeMonitorError {
                operation: self.operation.into(),
                cdb: cdb.to_vec(),
                observed_at: crate::ltfs::label::ltfs_time_now(),
                status,
                sense_key,
                asc,
                ascq,
                message: error.to_string().chars().take(500).collect(),
            });
        }
    }
    pub fn error(&self) -> Option<TapeMonitorError> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
impl TapeTransport for Tracked<'_> {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.device.execute_no_data(cdb, timeout);
        self.returned(cdb, &result);
        result
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        out: &mut [u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.device.execute_read(cdb, out, timeout);
        self.returned(cdb, &result);
        result
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.device.execute_write(cdb, data, timeout);
        self.returned(cdb, &result);
        result
    }
    fn identity(&self) -> &str {
        self.device.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.device.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.device.execution_guard()
    }
}

fn lost(error: impl std::fmt::Display) -> ReadError {
    ReadError::Lost(format!("磁带监控权威不可用: {error}"))
}

pub(super) async fn index_state(
    owner: &Active,
    control: &FdbOwnerControl,
    barcode: &str,
    index: crate::managed_catalog::ActiveIndexIdentity,
    import_required: bool,
) -> std::result::Result<(), ReadError> {
    control
        .catalog
        .record_tape_monitor(
            &control.serial,
            control.route,
            owner.round,
            barcode,
            index.volume_uuid,
            None,
            None,
            Some((index, import_required)),
        )
        .await
        .map_err(lost)?;
    Ok(())
}

pub(super) async fn io_error(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    barcode: &str,
    volume: uuid::Uuid,
    error: Option<TapeMonitorError>,
) {
    let Some(error) = error else {
        return;
    };
    if !permit.allows(owner.round, super::super::permission::Phase::Serving) {
        return;
    }
    if let Err(failure) = control
        .catalog
        .record_tape_monitor(
            &control.serial,
            control.route,
            owner.round,
            barcode,
            volume,
            None,
            Some(&error),
            None,
        )
        .await
    {
        warn!("磁带 {barcode} 故障观测未能持久化，原请求仍按失败处理: {failure}");
    }
}

fn capacity(device: &dyn TapeTransport, partition: u8) -> Result<TapeCapacity> {
    let mam = Mam::with_partition(device, partition);
    // Both values must be present and checked. Never substitute zero for an
    // unavailable attribute or use saturating arithmetic as a capacity proof.
    let maximum = mam.read_u64(ATTR_MAXIMUM_CAPACITY)?;
    let remaining = mam.read_u64(ATTR_REMAINING_CAPACITY)?;
    let (maximum, remaining) = maximum
        .zip(remaining)
        .and_then(|(m, r)| m.checked_mul(1 << 20).zip(r.checked_mul(1 << 20)))
        .filter(|(m, r)| *m > 0 && *m <= i64::MAX as u64 && r <= m)
        .ok_or_else(|| {
            TapeError::NotReady(format!("分区 {partition} 的真实 MAM 容量缺失或无效"))
        })?;
    Ok(TapeCapacity {
        partition,
        maximum,
        remaining,
    })
}

fn collect<T>(
    result: Result<T>,
    errors: &mut Vec<String>,
) -> std::result::Result<Option<T>, ReadError> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) if ownership_lost(&e) => Err(lost(e)),
        Err(e) => {
            errors.push(e.to_string().chars().take(240).collect());
            Ok(None)
        }
    }
}

pub(super) async fn sample(
    owner: &Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    drive: usize,
    barcode: &str,
    volume: uuid::Uuid,
) -> std::result::Result<(), ReadError> {
    let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
    fdb_loading::guard(owner, permit, control).await?;
    let device = owner.devices[drive].dev.as_ref();
    let tape = TapeDrive::new(device);
    let mut errors = Vec::new();
    // Ready is checked without loading. Unthreaded or absent cartridges are
    // not grounds to attribute a previous medium's alerts to this barcode.
    let ready = collect(tape.test_unit_ready(), &mut errors)? == Some(true);
    if !ready && errors.is_empty() {
        errors.push("磁带未就绪，保留未知状态".into());
    }
    let (alerts, write_protected, data, index) = if ready {
        // Single, full-size LOG SENSE only. No automatic retry after a possibly
        // consumed TapeAlert response, including malformed/truncated replies.
        let mut alerts = collect(tape.read_tape_alerts(), &mut errors)?;
        if let Some(alerts) = alerts.as_mut() {
            alerts.sort_unstable();
        }
        let wp = collect(tape.write_protected(), &mut errors)?;
        // This project's LTFS labels fix a=index/P0 and b=data/P1. We only
        // sample sessions already identity-verified by this owner.
        let data = collect(capacity(device, 1), &mut errors)?;
        let index = collect(capacity(device, 0), &mut errors)?;
        (alerts, wp, data, index)
    } else {
        (None, None, None, None)
    };
    fdb_loading::guard(owner, permit, control).await?;
    let sample = TapeMonitorSample {
        drive_serial: owner.devices[drive].serial.clone(),
        observed_at: crate::ltfs::label::ltfs_time_now(),
        write_protected,
        data,
        index,
        alerts,
        errors,
    };
    control
        .catalog
        .record_tape_monitor(
            &control.serial,
            control.route,
            owner.round,
            barcode,
            volume,
            Some(&sample),
            None,
            None,
        )
        .await
        .map_err(lost)?;
    Ok(())
}

pub(super) async fn write_allowed(
    owner: &Active,
    control: &FdbOwnerControl,
    barcode: &str,
    data: bool,
) -> std::result::Result<(), ReadError> {
    if let Some(record) = control
        .catalog
        .active_tape_monitor(&control.serial, control.route, barcode)
        .await
        .map_err(lost)?
        && !(if data {
            record.data_write_allowed()
        } else {
            record.write_allowed()
        })
    {
        return Err(ReadError::Busy(format!(
            "磁带 {barcode} 的监控状态 {} 禁止本次写入",
            record.state()
        )));
    }
    let _ = owner;
    Ok(())
}

pub(super) async fn tick(
    owner: &mut Active,
    permit: &Permission,
    control: &FdbOwnerControl,
    opts: &ExecOptions,
) -> std::result::Result<(), ReadError> {
    if owner
        .fdb_monitor
        .next
        .is_none_or(|due| Instant::now() >= due)
    {
        owner.fdb_monitor.next = Some(Instant::now() + INTERVAL);
        let _activity = permit.begin_device_activity(owner.round).map_err(lost)?;
        fdb_loading::guard(owner, permit, control).await?;
        match management::observe(owner, opts) {
            Ok(observation) => {
                fdb_loading::guard(owner, permit, control).await?;
                control
                    .catalog
                    .record_active_library_inventory(
                        &control.serial,
                        control.route,
                        owner.round,
                        &observation,
                    )
                    .await
                    .map_err(lost)?;
                control
                    .catalog
                    .record_library_monitor(&control.serial, control.route, owner.round, None)
                    .await
                    .map_err(lost)?;
            }
            Err(e) if ownership_lost(&e) => return Err(lost(e)),
            Err(e) => {
                let message = e.to_string().chars().take(500).collect::<String>();
                control
                    .catalog
                    .record_library_monitor(
                        &control.serial,
                        control.route,
                        owner.round,
                        Some(&message),
                    )
                    .await
                    .map_err(lost)?;
                warn!("带库库存监控失败，保留上次成功观测: {message}");
            }
        }
        owner.fdb_monitor.pending = owner.fdb_reads.keys().copied().collect();
    }
    // At most one already-verified drive per owner loop. Foreground requests
    // and cancellation keep their turn between samples.
    if let Some(drive) = owner.fdb_monitor.pending.pop()
        && let Some(mount) = owner.fdb_reads.get(&drive)
    {
        sample(owner, permit, control, drive, &mount.barcode, mount.volume).await?;
    }
    Ok(())
}
