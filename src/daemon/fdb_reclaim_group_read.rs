//! Actual destination bytes, sealed only after the group's final physical
//! capture. Temporary pending digests never escape as settlement evidence.
use super::*;
use crate::daemon::executor::{ObserveReclaimGroupIo, fdb_operation_guard::Guard};
use crate::ltfs::index::FileNode;
use crate::ltfs::recovery::CapturedIndexSource;
use crate::ltfs::volume::{VolumeSession, read_extent_plan_sequential_to_writer};
use crate::managed_catalog::{ReclaimGroupMemberReadContext, ReclaimIndexAugmentation};

/// Only this actor can seal evidence, after actual body READs and capture B.
/// Settlement rechecks the immutable original context in its own transaction.
pub(crate) struct ReclaimGroupMemberReadback {
    context: ReclaimGroupMemberReadContext,
    bytes: u64,
    sha256: [u8; 32],
    md5: Option<[u8; 16]>,
}
impl ReclaimGroupMemberReadback {
    pub(crate) fn context(&self) -> &ReclaimGroupMemberReadContext {
        &self.context
    }
    pub(crate) fn metadata(&self) -> &ReadMetadata {
        self.context.metadata()
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(crate) fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub(crate) fn md5(&self) -> Option<[u8; 16]> {
        self.md5
    }
}

struct PendingByteRead {
    bytes: u64,
    sha256: [u8; 32],
    md5: Option<[u8; 16]>,
}

fn error(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(detail.to_string())
}

impl ActiveMediaReader<'_> {
    fn require_group_reader(&self, io: &ObserveReclaimGroupIo<'_>) -> Result<()> {
        if !std::ptr::eq(self.catalog, io.catalog())
            || self.key != io.key()
            || self.device.identity() != io.transport().identity()
            || self.device.execution_guard() != io.transport().execution_guard()
            || io.transport().execution_guard()
                != Some((
                    self.key.round(),
                    self.permission as *const Permission as usize,
                ))
            || io.scope().parent().serial() != self.serial
            || io.scope().parent().namespace() != self.route.route.namespace
        {
            return Err(error("回收组正文读取器不是原Observe设备、轮次或多数派许可"));
        }
        Ok(())
    }

    fn require_group_member(
        &self,
        io: &ObserveReclaimGroupIo<'_>,
        context: &ReclaimGroupMemberReadContext,
    ) -> Result<()> {
        let parent = context.scope().parent();
        let metadata = context.metadata();
        if context.owner() != io.key().round()
            || parent.intent() != io.scope().parent().intent()
            || context.group_id() != io.scope().group().id()
            || context.receipt().library_serial != self.serial
            || context.receipt().namespace != self.route.route.namespace
            || metadata.placement.library_serial != self.serial
            || metadata.placement.namespace != self.route.route.namespace
            || metadata.placement.pool_id != parent.spec().pool()
            || metadata.copy.volume_id != context.head().volume_uuid
            || metadata.placement.volume_id != context.head().volume_uuid
            || context.donor().length != context.receipt().size
            || context.donor().meta.file_uid != context.receipt().ltfs_file_uid
        {
            return Err(error("回收组正文回读不是原成员、目标副本或执行身份"));
        }
        Ok(())
    }

    /// One serialized activity covers all member reads and the final physical
    /// capture. Capture A is the original publication proof; B is made here,
    /// after the last body READ. No per-member formal evidence API exists.
    pub(in crate::daemon) async fn read_reclaim_group_destination(
        &self,
        io: &ObserveReclaimGroupIo<'_>,
        original: VolumeSession,
        mut capture_a: CapturedIndexSource,
        plan: &ReclaimIndexAugmentation,
        contexts: Vec<ReclaimGroupMemberReadContext>,
        scratch: &Path,
    ) -> Result<Vec<ReclaimGroupMemberReadback>> {
        self.require_group_reader(io)?;
        let first = contexts
            .first()
            .ok_or_else(|| error("回收组回读成员为空"))?;
        io.require_checkpoint(
            first.batch_id(),
            plan.previous().map_err(error)?,
            first.head(),
            plan,
        )?;
        if !original.matches_device(io.transport(), io.key())
            || ActiveIndexIdentity::from(original.committed_index_construct()) != first.head()
            || contexts.len() != io.scope().group().files().count()
            || contexts.len() != io.scope().group().ordered_ordinals().count()
        {
            return Err(error("回收组回读必须保留原干净物理会话与完整成员"));
        }
        let fingerprint = plan.fingerprint().map_err(error)?;
        let identity = capture_a.identity()?.to_bytes();
        let original_end = capture_a.construct_end();
        let original_scope = capture_a.scope();
        plan.observe(&capture_a).map_err(error)?;
        for (ordinal, context) in io.scope().group().ordered_ordinals().zip(contexts.iter()) {
            self.require_group_member(io, context)?;
            if context.ordinal() != ordinal
                || context.batch_id() != first.batch_id()
                || context.attempt_id() != first.attempt_id()
                || context.plan_digest() != fingerprint
                || context.observation_digest() != first.observation_digest()
                || context.head() != first.head()
                || context.source_identity().to_bytes() != identity
            {
                return Err(error("回收组回读不能替换原顺序、计划、attempt或Observed"));
            }
            context.require_capture(&capture_a).map_err(error)?;
        }
        let full_head = capture_a.take_reclaim_readback_full_head(self.storage.index_bytes);
        let _activity = self.permission.begin_device_activity(self.key.round())?;
        // Every donor is already bound. Keep A's exact identity and bounded
        // history schedule, releasing its replay before allocating capture B.
        drop(capture_a);
        for context in &contexts {
            io.catalog()
                .check_reclaim_group_member_read_context(context)
                .await
                .map_err(error)?;
        }
        io.verify()?;
        let volume = io.resume_volume(original)?;
        if volume.label().volume_uuid != first.head().volume_uuid {
            return Err(error("回收组原会话的目标卷标改变"));
        }
        let mut pending = Vec::with_capacity(contexts.len());
        for context in &contexts {
            let checked = Guard::new(io.transport(), || {
                futures_executor::block_on(
                    io.catalog()
                        .check_reclaim_group_member_read_context(context),
                )
                .map_err(error)
            });
            checked.verify_proof()?;
            let expected_sha = match context.receipt().content_digest {
                ContentDigest::Sha256(sha) => sha,
                _ => return Err(error("回收组目标回读必须保留原成员SHA256")),
            };
            let (bytes, sha256, md5) = read_member_bytes(
                &checked,
                volume.label(),
                context.physical_path(),
                context.donor(),
                expected_sha,
                context.source_md5(),
            )?;
            checked.verify_proof()?;
            io.verify()?;
            pending.push(PendingByteRead { bytes, sha256, md5 });
        }
        let mut budget = RecoveryBudget::default();
        budget.max_index_bytes = budget
            .max_index_bytes
            .min(usize::try_from(self.storage.index_bytes).unwrap_or(usize::MAX));
        budget.index_scratch.directory = scratch.to_path_buf();
        budget.index_scratch.max_bytes = self.storage.index_bytes;
        budget.index_scratch.headroom_bytes = self.storage.headroom_bytes;
        let drive = TapeDrive::new(io.transport());
        let capture_b = if let Some(full_head) = full_head {
            // The exact Full raw index binds the plan and donor nodes proved
            // by A. B still reads the current head and complete history.
            full_head.verify(&drive, volume.label(), &budget)?;
            None
        } else {
            Some(capture_complete_index_source(
                &drive,
                volume.label(),
                &budget,
            )?)
        };
        let completed_epoch = io
            .transport()
            .session_epoch()
            .ok_or_else(|| TapeError::NotReady("回收组尾捕获后无法保持原设备epoch".into()))?;
        if let Some(capture_b) = capture_b {
            plan.observe(&capture_b).map_err(error)?;
            if capture_b.identity()?.to_bytes() != identity
                || capture_b.construct_end() != original_end
                || capture_b.scope() != original_scope
            {
                return Err(error("回收组正文读后实际卷头、历史或EOD/closing改变"));
            }
            for context in &contexts {
                context.require_capture(&capture_b).map_err(error)?;
            }
        }
        io.catalog()
            .check_reclaim_group_member_read_contexts(&contexts)
            .await
            .map_err(error)?;
        io.verify()?;
        if io.transport().session_epoch() != Some(completed_epoch) {
            return Err(TapeError::NotReady(
                "回收组最终授权核验期间设备状态改变".into(),
            ));
        }
        Ok(pending
            .into_iter()
            .zip(contexts)
            .map(|(item, context)| ReclaimGroupMemberReadback {
                context,
                bytes: item.bytes,
                sha256: item.sha256,
                md5: item.md5,
            })
            .collect())
    }
}

/// Bounded digest sink over the existing validated physical extent reader.
/// The returned digests come only from READ bytes, never from index attributes.
fn read_member_bytes(
    device: &dyn TapeTransport,
    label: &LtfsLabel,
    path: &str,
    donor: &FileNode,
    expected_sha: [u8; 32],
    expected_md5: Option<[u8; 16]>,
) -> Result<(u64, [u8; 32], Option<[u8; 16]>)> {
    if donor.symlink.is_some() {
        return Err(error("回收组实际donor不能替换为符号链接"));
    }
    let mut sink = HashSink {
        remaining: donor.length,
        sha: Sha256::new(),
        md5: expected_md5.map(|_| Md5::new()),
    };
    let bytes = read_extent_plan_sequential_to_writer(
        device,
        label,
        path,
        donor.length,
        donor.extents.iter().cloned().map(Ok),
        &mut sink,
    )?;
    let sha256 = sink.sha.finalize().into();
    let md5 = sink.md5.map(|hash| hash.finalize().into());
    if sink.remaining != 0 || bytes != donor.length || sha256 != expected_sha || md5 != expected_md5
    {
        return Err(error("回收组目标实读长度、SHA256或原MD5不符"));
    }
    Ok((bytes, sha256, md5))
}

struct HashSink {
    remaining: u64,
    sha: Sha256,
    md5: Option<Md5>,
}
impl Write for HashSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "回收组正文超过原长度")
            })?;
        self.sha.update(bytes);
        if let Some(md5) = &mut self.md5 {
            md5.update(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::Extent;
    use crate::scsi::cdb::opcode;
    use crate::scsi::sim::{LogicalObject, SimCartridge, SimLibrary};

    fn media(records: Vec<LogicalObject>) -> (SimLibrary, LtfsLabel) {
        let lib = SimLibrary::new(1, 2, 1);
        let mut cart = SimCartridge::blank("GROUPREAD", 1 << 20);
        cart.partitions.push(cart.partitions[0].clone());
        cart.partitions[1].objects = records;
        cart.partitions[1].flushed = cart.partitions[1].objects.len();
        lib.insert_cartridge(cart, 0).unwrap();
        lib.load_into_drive("GROUPREAD", 0).unwrap();
        (lib, LtfsLabel::new(Uuid::nil(), 'a', 16, "test".into()))
    }
    fn donor(block: u64, offset: u64, size: u64) -> FileNode {
        FileNode {
            name: "body".into(),
            length: size,
            extents: vec![Extent {
                partition: 'b',
                start_block: block,
                byte_offset: offset,
                byte_count: size,
                file_offset: 0,
            }],
            ..Default::default()
        }
    }
    fn sha(body: &[u8]) -> [u8; 32] {
        Sha256::digest(body).into()
    }
    fn md5(body: &[u8]) -> [u8; 16] {
        Md5::digest(body).into()
    }

    #[test]
    fn real_extent_bytes_preserve_offsets_sha_and_original_md5() {
        let (lib, label) = media(vec![
            LogicalObject::Record(b"xxhello".to_vec()),
            LogicalObject::Record(b"world".to_vec()),
        ]);
        let actual = read_member_bytes(
            &lib.drive(0),
            &label,
            "/body",
            &donor(0, 2, 10),
            sha(b"helloworld"),
            Some(md5(b"helloworld")),
        )
        .unwrap();
        assert_eq!(actual, (10, sha(b"helloworld"), Some(md5(b"helloworld"))));
    }

    #[test]
    fn next_member_reads_current_position_without_mount_mam_or_extra_locate() {
        let (lib, label) = media(vec![
            LogicalObject::Record(b"first".to_vec()),
            LogicalObject::Record(b"second".to_vec()),
        ]);
        let dev = lib.drive(0);
        read_member_bytes(&dev, &label, "/first", &donor(0, 0, 5), sha(b"first"), None).unwrap();
        let before = dev.command_log().len();
        read_member_bytes(
            &dev,
            &label,
            "/second",
            &donor(1, 0, 6),
            sha(b"second"),
            None,
        )
        .unwrap();
        let commands = dev.command_log();
        assert_eq!(
            commands[before..]
                .iter()
                .filter(|&&op| op == opcode::READ_6)
                .count(),
            1
        );
        assert!(
            !commands[before..]
                .iter()
                .any(|op| matches!(*op, 0x2b | 0x92 | 0x8c | 0x8d))
        );
    }

    #[test]
    fn corrupt_bytes_cannot_be_proved_by_unchanged_index_hash() {
        let (lib, label) = media(vec![LogicalObject::Record(b"wrong".to_vec())]);
        let mut node = donor(0, 0, 5);
        node.set_xattr(
            crate::ltfs::index::XATTR_SHA256,
            &sha(b"right")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
        let dev = lib.drive(0);
        assert!(read_member_bytes(&dev, &label, "/body", &node, sha(b"right"), None).is_err());
        assert!(dev.command_log().contains(&opcode::READ_6));
    }

    #[test]
    fn correct_sha_does_not_bypass_original_md5() {
        let (lib, label) = media(vec![LogicalObject::Record(b"right".to_vec())]);
        assert!(
            read_member_bytes(
                &lib.drive(0),
                &label,
                "/body",
                &donor(0, 0, 5),
                sha(b"right"),
                Some(md5(b"wrong"))
            )
            .is_err()
        );
    }

    #[test]
    fn short_tape_body_and_invalid_extent_never_return_evidence() {
        let (lib, label) = media(vec![
            LogicalObject::Record(b"part".to_vec()),
            LogicalObject::Filemark,
        ]);
        let dev = lib.drive(0);
        assert!(
            read_member_bytes(
                &dev,
                &label,
                "/body",
                &donor(0, 0, 8),
                sha(b"partbody"),
                None
            )
            .is_err()
        );
        let before = dev.command_log().len();
        let mut bad = donor(0, 0, 4);
        bad.extents[0].file_offset = 1;
        assert!(read_member_bytes(&dev, &label, "/body", &bad, sha(b"part"), None).is_err());
        assert_eq!(dev.command_log().len(), before);
    }

    #[test]
    fn authority_loss_after_first_read_latches_without_reading_remaining_blocks() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct LoseAfterRead<'a> {
            inner: &'a dyn TapeTransport,
            alive: &'a AtomicBool,
        }
        impl TapeTransport for LoseAfterRead<'_> {
            fn identity(&self) -> &str {
                self.inner.identity()
            }
            fn execute_no_data(
                &self,
                cdb: &[u8],
                ms: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.inner.execute_no_data(cdb, ms)
            }
            fn execute_read(
                &self,
                cdb: &[u8],
                out: &mut [u8],
                ms: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                let result = self.inner.execute_read(cdb, out, ms);
                if cdb[0] == opcode::READ_6 {
                    self.alive.store(false, Ordering::SeqCst);
                }
                result
            }
            fn execute_write(
                &self,
                cdb: &[u8],
                data: &[u8],
                ms: u32,
            ) -> Result<crate::scsi::device::ScsiResult> {
                self.inner.execute_write(cdb, data, ms)
            }
        }
        let (lib, label) = media(vec![
            LogicalObject::Record(b"first".to_vec()),
            LogicalObject::Record(b"second".to_vec()),
        ]);
        let dev = lib.drive(0);
        let alive = AtomicBool::new(true);
        let device = LoseAfterRead {
            inner: &dev,
            alive: &alive,
        };
        let guarded = Guard::new(&device, || {
            if alive.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(error("test authority lost"))
            }
        });
        assert!(
            read_member_bytes(
                &guarded,
                &label,
                "/body",
                &donor(0, 0, 11),
                sha(b"firstsecond"),
                None
            )
            .is_err()
        );
        let before = dev.command_log().len();
        assert_eq!(
            dev.command_log()
                .iter()
                .filter(|&&op| op == opcode::READ_6)
                .count(),
            1
        );
        alive.store(true, Ordering::SeqCst);
        assert!(
            read_member_bytes(
                &guarded,
                &label,
                "/body",
                &donor(1, 0, 6),
                sha(b"second"),
                None
            )
            .is_err()
        );
        assert_eq!(dev.command_log().len(), before);
    }
}
