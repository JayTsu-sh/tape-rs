//! One complete, unchanged source capture owned by this execution instance.
//! The retained SQLite database is not an I/O or source-format permit.
use super::*;
use crate::daemon::executor::fdb_operation_guard::Guard;
use crate::ltfs::mam::{Mam, vcr_is_valid, vcr_matches};
use crate::ltfs::recovery::CapturedIndexSource;

pub(crate) struct ReclaimSourceSession {
    intent: serde_json::Value,
    library: String,
    route: ActiveLibraryRoute,
    key: ReservationKey,
    guard: (u64, usize),
    device: String,
    head: ActiveIndexIdentity,
    epoch: Option<u64>,
    change: Option<[u8; 32]>,
    vcr: Option<Vec<u8>>,
    captured: CapturedIndexSource,
}

impl ReclaimSourceSession {
    pub(crate) fn captured(&self) -> &CapturedIndexSource {
        &self.captured
    }

    #[cfg(test)]
    pub(crate) fn test_change_token(&self) -> Option<[u8; 32]> {
        self.change
    }

    fn matches(&self, reader: &ActiveMediaReader<'_>, parent: &ReclaimReservation) -> bool {
        self.intent == *parent.intent()
            && self.library == reader.serial
            && self.route == reader.route
            && self.key == reader.key
            && Some(self.guard) == reader.device.execution_guard()
            && self.device == reader.device.identity()
            && self.change.is_some()
            && self.change == reader.device.medium_change_token()
            && self.epoch.is_some()
            && self.epoch == reader.device.session_epoch()
    }
}

fn error(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(detail.to_string())
}

enum VcrRead {
    Value(Vec<u8>),
    Unavailable,
    Unsupported,
}

fn read_vcr(device: &impl TapeTransport) -> Result<VcrRead> {
    // Unsupported attributes disable reuse. A transport/authority failure is
    // still rejected by the enclosing Guard's final verify_proof.
    match Mam::with_partition(device, 0).read_vcr() {
        Ok(vcr) => Ok(match vcr.filter(|v| vcr_is_valid(v)) {
            Some(vcr) => VcrRead::Value(vcr.to_vec()),
            None => VcrRead::Unavailable,
        }),
        Err(error @ TapeError::ScsiCommand { sense_key: 5, .. })
            if crate::daemon::executor::fdb_operation_guard::confirmed_device_failure(&error) =>
        {
            Ok(VcrRead::Unsupported)
        }
        Err(error) => Err(error),
    }
}

impl ActiveMediaReader<'_> {
    fn reclaim_source_guard<'a>(
        &'a self,
        parent: &'a ReclaimReservation,
        head: ActiveIndexIdentity,
    ) -> Guard<'a, impl Fn() -> Result<()> + 'a> {
        Guard::new(self.device, move || {
            if futures_executor::block_on(
                self.catalog.reclaim_source_head(parent, self.key.round()),
            )
            .map_err(error)?
                != head
            {
                return Err(error("回收源捕获会话的原卷头改变"));
            }
            self.check_device_owner()?;
            verify_holder(self.device, self.key)
        })
    }

    /// Take the old capture before any check, so a failure cannot renew it.
    /// Only a strict unchanged epoch can reuse a prior complete history scan.
    pub(crate) async fn capture_reclaim_source_session(
        &self,
        parent: &ReclaimReservation,
        scratch: &Path,
        previous: Option<Box<ReclaimSourceSession>>,
    ) -> Result<Box<ReclaimSourceSession>> {
        if parent.intent()["library"] != self.serial
            || parent.intent()["namespace"] != self.route.route.namespace.to_string()
        {
            return Err(error("回收源捕获会话与当前执行范围不同"));
        }
        let head = self
            .catalog
            .reclaim_source_head(parent, self.key.round())
            .await
            .map_err(error)?;
        let epoch = self.device.session_epoch();
        let change = self.device.medium_change_token();
        let guard = self
            .device
            .execution_guard()
            .ok_or_else(|| TapeError::NotReady("回收源捕获缺少当前设备守卫".into()))?;
        self.check_device_owner()?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = self.reclaim_source_guard(parent, head);
        let candidate =
            previous.filter(|session| session.matches(self, parent) && session.head == head);
        if change.is_none() {
            // No transport-wide content evidence: preserve the original full
            // capture path, without optional VCR commands or later promotion.
            drop(candidate);
            let captured = self.capture_reclaim_source(parent, scratch).await?;
            self.catalog
                .publish_reclaim_full_history_hint(parent, self.key.round(), &captured)
                .await
                .map_err(error)?;
            return Ok(Box::new(ReclaimSourceSession {
                intent: parent.intent().clone(),
                library: self.serial.into(),
                route: self.route,
                key: self.key,
                guard,
                device: self.device.identity().into(),
                head,
                epoch: None,
                change: None,
                vcr: None,
                captured,
            }));
        }
        if candidate.is_some() {
            // Do not consume UA/reset with TUR's normal bounded retry helper
            // and then accept a proof captured before that device change.
            checked.execute_no_data(&crate::scsi::cdb::test_unit_ready(), 10_000)?;
        }
        let before_vcr = read_vcr(&checked)?;
        checked.verify_proof()?;
        if let Some(session) = candidate
            && session.epoch == epoch
            && self.device.session_epoch() == epoch
            && session.change == change
            && self.device.medium_change_token() == change
            && matches!(&before_vcr, VcrRead::Value(current) if {
                session
                    .vcr
                    .as_ref()
                    .is_some_and(|saved| vcr_matches(saved, current))
            })
        {
            // Revalidate the owned immutable database as well as its medium.
            ReclaimSourceInventory::from_captured(&session.captured).map_err(error)?;
            checked.verify_proof()?;
            if self.device.session_epoch() != epoch || self.device.medium_change_token() != change {
                return Err(TapeError::NotReady("回收源捕获复用期间设备代次改变".into()));
            }
            return Ok(session);
        }
        let captured = self.capture_reclaim_source(parent, scratch).await?;
        let after_capture_change = self.device.medium_change_token();
        if after_capture_change != change {
            return Err(TapeError::NotReady(
                "回收源完整捕获期间共享介质变化证明改变或丢失".into(),
            ));
        }
        let finished_epoch = self.device.session_epoch();
        // An unsupported/absent VCR has already disabled caching. The original
        // complete capture remains valid for this call; do not add another
        // unsupported command and mistake its confirmed rejection for a reset.
        let after_vcr = if matches!(before_vcr, VcrRead::Value(_)) {
            read_vcr(&checked)?
        } else {
            VcrRead::Unavailable
        };
        checked.verify_proof()?;
        let after_epoch = self.device.session_epoch();
        let confirmed_unsupported = matches!(after_vcr, VcrRead::Unsupported)
            && finished_epoch.and_then(|epoch| epoch.checked_add(1)) == after_epoch;
        if after_epoch != finished_epoch && !confirmed_unsupported {
            return Err(TapeError::NotReady("回收源捕获结清期间设备代次改变".into()));
        }
        // This independent content/identity proof covers the entire capture
        // and final authority/VCR checks. Unsupported VCR can explain only its
        // own handle epoch increment, never a shared medium change.
        if self.device.medium_change_token() != change {
            return Err(TapeError::NotReady(
                "回收源捕获结清期间共享介质变化证明改变或丢失".into(),
            ));
        }
        let vcr = match (before_vcr, after_vcr) {
            (VcrRead::Value(before), VcrRead::Value(after)) => {
                if !vcr_matches(&before, &after) {
                    return Err(error("回收源完整捕获期间介质 VCR 改变"));
                }
                Some(after)
            }
            // This fresh complete capture remains usable by the current call;
            // an unavailable VCR prevents retaining it for another tick.
            _ => None,
        };
        // This only stores read-order hints after the original complete proof
        // and token checks. Future users still read every physical construct.
        self.catalog
            .publish_reclaim_full_history_hint(parent, self.key.round(), &captured)
            .await
            .map_err(error)?;
        Ok(Box::new(ReclaimSourceSession {
            intent: parent.intent().clone(),
            library: self.serial.into(),
            route: self.route,
            key: self.key,
            guard,
            device: self.device.identity().into(),
            head,
            epoch: finished_epoch,
            change,
            vcr,
            captured,
        }))
    }

    /// A successful parent step cannot bless an epoch changed by its I/O.
    /// Strictly unchanged sources are checked again before their capture is kept.
    pub(crate) async fn finish_reclaim_source_session(
        &self,
        parent: &ReclaimReservation,
        session: Box<ReclaimSourceSession>,
    ) -> Result<Option<Box<ReclaimSourceSession>>> {
        if !session.matches(self, parent) || session.vcr.is_none() {
            return Ok(None);
        }
        let head = self
            .catalog
            .reclaim_source_head(parent, self.key.round())
            .await
            .map_err(error)?;
        if head != session.head {
            return Err(error("回收源捕获结清时原卷头改变"));
        }
        self.check_device_owner()?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = self.reclaim_source_guard(parent, head);
        let after_vcr = read_vcr(&checked)?;
        checked.verify_proof()?;
        if !session.matches(self, parent)
            || !matches!(&after_vcr, VcrRead::Value(current) if {
                session
                    .vcr
                    .as_ref()
                    .is_some_and(|saved| vcr_matches(saved, current))
            })
        {
            return Ok(None);
        }
        Ok(Some(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailedVcr {
        key: u8,
        complete: bool,
    }
    impl TapeTransport for FailedVcr {
        fn execute_no_data(&self, _: &[u8], _: u32) -> Result<crate::scsi::device::ScsiResult> {
            unreachable!()
        }
        fn execute_write(
            &self,
            _: &[u8],
            _: &[u8],
            _: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            unreachable!()
        }
        fn execute_read(
            &self,
            cdb: &[u8],
            _: &mut [u8],
            _: u32,
        ) -> Result<crate::scsi::device::ScsiResult> {
            assert_eq!(cdb.first(), Some(&0x8c));
            if self.key == 255 {
                return Err(TapeError::ScsiHost(3));
            }
            let mut raw = [0; 18];
            raw[0] = 0x70;
            raw[2] = self.key;
            raw[7] = 10;
            raw[12] = if self.key == 6 { 0x29 } else { 0x24 };
            crate::scsi::device::finish_command(
                2,
                crate::scsi::sense::SenseInfo::from_bytes(
                    &raw[..if self.complete { 18 } else { 14 }],
                ),
                0,
                0,
                self.identity(),
            )
        }
        fn identity(&self) -> &str {
            "vcr-error-fixture"
        }
    }

    #[test]
    fn optional_vcr_never_swallows_reset_transport_or_incomplete_sense() {
        assert!(
            FailedVcr {
                key: 5,
                complete: true
            }
            .medium_change_token()
            .is_none(),
            "默认transport没有可信变化证明"
        );
        assert!(matches!(
            read_vcr(&FailedVcr {
                key: 5,
                complete: true
            }),
            Ok(VcrRead::Unsupported)
        ));
        for (key, complete) in [(5, false), (6, true), (255, true)] {
            assert!(read_vcr(&FailedVcr { key, complete }).is_err());
        }
    }
}
