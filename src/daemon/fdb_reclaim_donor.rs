//! Re-read the original donor at a repaired head. Neither the extent cache nor
//! retained byte hashes can substitute for the actual READ(6) stream here.
use super::*;
use crate::catalog::volume_extents::CachedFile;
use crate::managed_catalog::ReclaimMetadataDonorEvidence;
use sha2::{Digest, Sha256};
use std::io::Write;

pub struct ReclaimMetadataDonorRead {
    target: CapturedIndexSource,
    proof: ReclaimMetadataDonorEvidence,
}
impl ReclaimMetadataDonorRead {
    pub fn target(&self) -> &CapturedIndexSource {
        &self.target
    }
    pub fn evidence(&self) -> &ReclaimMetadataDonorEvidence {
        &self.proof
    }
}

pub(super) struct BodyHash {
    pub(super) sha: Sha256,
    pub(super) md5: Option<md5::Md5>,
    pub(super) length: u64,
    pub(super) expected: u64,
}
impl Write for BodyHash {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = self
            .length
            .checked_add(bytes.len() as u64)
            .filter(|length| *length <= self.expected)
            .ok_or_else(|| std::io::Error::other("donor 实读超过原正文长度"))?;
        self.sha.update(bytes);
        if let Some(md5) = &mut self.md5 {
            md5.update(bytes);
        }
        self.length = length;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ReclaimMetadataWriter<'_> {
    pub fn read_donor<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<ReclaimMetadataDonorRead>> + 'a>>
    {
        Box::pin(async move {
            self.require_scope(reservation).await?;
            let context = self
                .catalog
                .reclaim_metadata_donor_context(
                    reservation,
                    self.key.round(),
                    file,
                    request,
                    original,
                )
                .await
                .map_err(catalog)?;
            if context.task() != original {
                return Err(catalog("新 donor 上下文改变原恢复任务"));
            }
            let _activity = self.permission.begin_device_activity(self.key.round())?;
            let checked = Guard::new(self.device, || {
                futures_executor::block_on(self.require_scope(reservation))?;
                let actual =
                    futures_executor::block_on(self.catalog.reclaim_metadata_donor_context(
                        reservation,
                        self.key.round(),
                        file,
                        request,
                        original,
                    ))
                    .map_err(catalog)?;
                if actual != context {
                    return Err(catalog("donor 实读原输入或当前卷头改变"));
                }
                verify_holder(self.device, self.key)
            });
            let label = read_label_only(&checked)?;
            if label.volume_uuid != context.current().volume_uuid {
                return Err(catalog("donor 已装入介质不是原目标卷"));
            }
            let target =
                capture_complete_index_source(&TapeDrive::new(&checked), &label, self.budget)?;
            let node = context.require_donor(&target).map_err(catalog)?;
            let mut body = BodyHash {
                sha: Sha256::new(),
                md5: node
                    .xattrs
                    .iter()
                    .any(|attr| attr.key == crate::ltfs::index::XATTR_MD5)
                    .then(md5::Md5::new),
                length: 0,
                expected: node.length,
            };
            let descriptor = CachedFile {
                path: context.donor_path().into(),
                length: node.length,
                symlink: None,
                extents: node.extents,
            };
            let length = crate::ltfs::volume::read_cached_file_to_writer(
                &checked,
                &label,
                &descriptor,
                &mut body,
            )?;
            if length != body.length || length != body.expected {
                return Err(catalog("donor 正文实读残缺"));
            }
            let after =
                capture_complete_index_source(&TapeDrive::new(&checked), &label, self.budget)?;
            if after.identity()? != target.identity()?
                || after.construct_end() != target.construct_end()
            {
                return Err(catalog("donor 正文读取期间实际索引或闭合边界改变"));
            }
            checked.verify_proof()?;
            let proof = ReclaimMetadataDonorEvidence::from_read(
                &context,
                self.key.round(),
                &target,
                length,
                body.sha.finalize().into(),
                body.md5.map(|md5| md5.finalize().into()),
            )
            .map_err(catalog)?;
            Ok(ReclaimMetadataDonorRead { target, proof })
        })
    }
}
