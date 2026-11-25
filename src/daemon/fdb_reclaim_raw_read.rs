//! Streaming reads of every sealed physical data UID, including unmapped EE
//! objects. No logical lookup, payload cache, publication or erase authority.
use super::*;
use crate::catalog::volume_extents::CachedFile;
use crate::daemon::executor::fdb_operation_guard::Guard;
use crate::managed_catalog::ReclaimRawSourceRead;

pub(super) struct StreamHash<'a, W> {
    pub(super) out: &'a mut W,
    pub(super) remaining: u64,
    pub(super) sha: Sha256,
    pub(super) md5: Option<Md5>,
}
impl<W: Write> Write for StreamHash<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("回收正文超过封存长度"));
        }
        let n = self.out.write(bytes)?;
        self.remaining -= n as u64;
        self.sha.update(&bytes[..n]);
        if let Some(md5) = &mut self.md5 {
            md5.update(&bytes[..n]);
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}
fn error(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(detail.to_string())
}

impl ActiveMediaReader<'_> {
    /// The executor serializes the already-loaded source drive. Output is
    /// bounded by the sealed length and streamed; no whole body is retained.
    pub async fn read_reclaim_raw_source_to<W: Write>(
        &self,
        reservation: &ReclaimReservation,
        uid: u64,
        scratch: &Path,
        out: &mut W,
    ) -> Result<ReclaimRawSourceRead> {
        if reservation.intent()["library"] != self.serial
            || reservation.intent()["namespace"] != self.route.route.namespace.to_string()
        {
            return Err(error("原始正文读取作用域与当前执行器不符"));
        }
        let before = self
            .catalog
            .reclaim_raw_read_context(reservation, self.key.round(), uid)
            .await
            .map_err(error)?;
        self.check_device_owner()?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.device, || {
            let current = futures_executor::block_on(self.catalog.reclaim_raw_read_context(
                reservation,
                self.key.round(),
                uid,
            ))
            .map_err(error)?;
            if current != before {
                return Err(error("原始正文清单或 owner 在读取期间改变"));
            }
            self.check_device_owner()
        });
        let label = read_label_only(&checked)?;
        if label.volume_uuid != reservation.spec().volume() {
            return Err(error("原始正文装入了其他介质"));
        }
        let mut budget = RecoveryBudget::default();
        budget.index_scratch.directory = scratch.into();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let actual = capture_complete_index_source(&TapeDrive::new(&checked), &label, &budget)?;
        let node = before.require_capture(&actual).map_err(error)?;
        let mut writer = StreamHash {
            out,
            remaining: node.length,
            sha: Sha256::new(),
            md5: node
                .xattrs
                .iter()
                .any(|attr| attr.key == crate::ltfs::index::XATTR_MD5)
                .then(Md5::new),
        };
        let physical = CachedFile {
            path: before.node().path().into(),
            length: node.length,
            symlink: None,
            extents: node.extents,
        };
        let length = read_cached_file_to_writer(&checked, &label, &physical, &mut writer)?;
        if length != physical.length || writer.remaining != 0 {
            return Err(error("原始正文实际读取长度不足"));
        }
        let sha = writer.sha.finalize().into();
        let md5 = writer.md5.map(|hash| hash.finalize().into());
        let after = capture_complete_index_source(&TapeDrive::new(&checked), &label, &budget)?;
        if after.identity()? != actual.identity()?
            || after.construct_end() != actual.construct_end()
        {
            return Err(error("原始正文实际索引或闭合边界在读取期间改变"));
        }
        checked.verify_proof()?;
        ReclaimRawSourceRead::from_read(&before, self.key.round(), &after, sha, md5).map_err(error)
    }
}

impl ActiveMediaReader<'_> {
    pub async fn read_reclaim_format_source(
        &self,
        reservation: &ReclaimReservation,
        scratch: &Path,
    ) -> Result<crate::managed_catalog::ReclaimFormatSourceEvidence> {
        let source = self.capture_reclaim_source(reservation, scratch).await?;
        let original = self
            .catalog
            .reclaim_source_head(reservation, self.key.round())
            .await
            .map_err(error)?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.device, || {
            if futures_executor::block_on(
                self.catalog
                    .reclaim_source_head(reservation, self.key.round()),
            )
            .map_err(error)?
                != original
            {
                return Err(error("格式化来源捕获期间原源卷头改变"));
            }
            self.check_device_owner()?;
            crate::scsi::reservation::verify_holder(self.device, self.key)
        });
        crate::ltfs::mkltfs::require_two_partition_layout(&checked)?;
        let label = crate::ltfs::volume::read_label_pair(&checked)?;
        if label.volume_uuid != reservation.spec().volume() {
            return Err(error("格式化原源标签身份改变"));
        }
        let medium = crate::ltfs::mam::Mam::new(&checked).read_medium_identity()?;
        checked.verify_proof()?;
        crate::managed_catalog::ReclaimFormatSourceEvidence::from_read(
            reservation,
            self.key.round(),
            &source,
            medium,
            &label,
        )
        .map_err(error)
    }

    /// Capture the original sealed head before inventory admission. This reads
    /// actual current media and never infers a source from the logical catalog.
    pub async fn capture_reclaim_source(
        &self,
        reservation: &ReclaimReservation,
        scratch: &Path,
    ) -> Result<crate::ltfs::recovery::CapturedIndexSource> {
        if reservation.intent()["library"] != self.serial
            || reservation.intent()["namespace"] != self.route.route.namespace.to_string()
        {
            return Err(error("原始索引捕获与当前执行器作用域不同"));
        }
        let original = self
            .catalog
            .reclaim_source_head(reservation, self.key.round())
            .await
            .map_err(error)?;
        self.check_device_owner()?;
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        let checked = Guard::new(self.device, || {
            if futures_executor::block_on(
                self.catalog
                    .reclaim_source_head(reservation, self.key.round()),
            )
            .map_err(error)?
                != original
            {
                return Err(error("原始索引捕获期间封存卷头改变"));
            }
            self.check_device_owner()?;
            crate::scsi::reservation::verify_holder(self.device, self.key)
        });
        let label = crate::ltfs::volume::read_label_only(&checked)?;
        if label.volume_uuid != reservation.spec().volume() {
            return Err(error("装入介质不是原回收源卷"));
        }
        let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
        budget.index_scratch.directory = scratch.into();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let hint = self
            .catalog
            .load_reclaim_full_history_hint(reservation, self.key.round(), &budget.index_scratch)
            .await
            .map_err(error)?;
        let captured = crate::ltfs::recovery::capture_complete_index_source_with_history_hint(
            &TapeDrive::new(&checked),
            &label,
            &budget,
            hint,
        )?;
        if crate::managed_catalog::ActiveIndexIdentity::from(captured.construct()) != original {
            return Err(error("回收源实际索引与原 FDB 卷头不同"));
        }
        crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).map_err(error)?;
        checked.verify_proof()?;
        Ok(captured)
    }
}
