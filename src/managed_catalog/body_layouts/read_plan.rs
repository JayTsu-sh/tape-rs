//! A fully checked FDB layout backed by an anonymous, read-only plan file.

use super::*;
use crate::error::{Result as TapeResult, TapeError};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};

fn catalog_error(error: CatalogError) -> TapeError {
    TapeError::Catalog(error.to_string())
}

/// A fresh capture is transitional provenance, never a claim that this layout
/// has already been attached to the authoritative FDB copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalPlanProvenance {
    FdbLayout,
    FreshCapture,
}

/// Minted only after every FDB chunk, layout field and the final binding pass.
/// The writable descriptor is closed before this read-only descriptor escapes.
pub struct VerifiedPhysicalPlan {
    binding: Option<CopyLayoutBinding>,
    path: String,
    volume: Uuid,
    uid: u64,
    length: u64,
    block_size: u32,
    partitions: [char; 2],
    extents: u32,
    file: Arc<File>,
}

impl VerifiedPhysicalPlan {
    #[cfg(test)]
    pub(crate) fn binding(&self) -> &CopyLayoutBinding {
        self.binding.as_ref().expect("FDB layout plan")
    }

    /// This describes provenance only; neither kind grants device access or
    /// replaces the caller's final catalog, owner and physical-head checks.
    pub fn provenance(&self) -> PhysicalPlanProvenance {
        if self.binding.is_some() {
            PhysicalPlanProvenance::FdbLayout
        } else {
            PhysicalPlanProvenance::FreshCapture
        }
    }

    pub(crate) fn scratch_bytes(&self) -> u64 {
        u64::from(self.extents) * EXTENT_BYTES as u64
    }

    pub(crate) fn same_location(&self, other: &Self) -> TapeResult<bool> {
        if self.path != other.path
            || self.volume != other.volume
            || self.uid != other.uid
            || self.length != other.length
            || self.block_size != other.block_size
            || self.partitions != other.partitions
        {
            return Ok(false);
        }
        let mut left = self.extents();
        let mut right = other.extents();
        loop {
            match (left.next().transpose()?, right.next().transpose()?) {
                (None, None) => return Ok(true),
                (Some(a), Some(b)) if a == b => {}
                _ => return Ok(false),
            }
        }
    }

    pub(crate) fn matches_extents(&self, expected: &[Extent]) -> TapeResult<bool> {
        let mut actual = self.extents();
        for extent in expected {
            match actual.next().transpose()? {
                Some(value) if &value == extent => {}
                _ => return Ok(false),
            }
        }
        Ok(actual.next().transpose()?.is_none())
    }

    pub(crate) fn from_current_read(
        source: &ActiveReadSource,
        captured: &crate::ltfs::recovery::CapturedReadIndex,
        label: &crate::ltfs::label::LtfsLabel,
        directory: &std::path::Path,
        budget: crate::local_read_storage::ReadStorageBudget,
    ) -> TapeResult<Option<Self>> {
        // Only this real capture can supply the decoded node; callers cannot
        // mint a physical proof from a CachedFile or an arbitrary extent Vec.
        let Some(entry) = captured.entry_by_uid(source.copy().ltfs_file_uid)? else {
            return Ok(None);
        };
        let Some(plan) = source.plan_current_body(entry) else {
            return Ok(None);
        };
        let crate::catalog::volume_extents::CachedFile {
            path,
            length,
            extents,
            symlink,
        } = plan.physical;
        if symlink.is_some() || length != source.copy().size {
            return Err(TapeError::Catalog("顺序读实测正文类型或长度不同".into()));
        }
        Self::seal_extent_slice(source, path, &extents, label, directory, budget).map(Some)
    }

    fn seal_extent_slice(
        source: &ActiveReadSource,
        path: String,
        extents: &[Extent],
        label: &crate::ltfs::label::LtfsLabel,
        directory: &std::path::Path,
        budget: crate::local_read_storage::ReadStorageBudget,
    ) -> TapeResult<Self> {
        let copy = source.copy();
        if label.volume_uuid != copy.volume_id {
            return Err(TapeError::Catalog("临时正文计划的实际卷与副本不同".into()));
        }
        let partitions = [label.index_partition, label.data_partition];
        if label.blocksize == 0
            || partitions[0] == partitions[1]
            || !partitions.iter().all(|part| matches!(part, 'a' | 'b'))
        {
            return Err(TapeError::Catalog("临时正文计划的介质几何无效".into()));
        }
        let mut offset = 0;
        for extent in extents {
            validate_layout_extent(extent, label.blocksize, partitions, &mut offset)
                .map_err(catalog_error)?;
        }
        if offset != copy.size {
            return Err(TapeError::Catalog("临时正文计划的长度或几何不同".into()));
        }
        let count = u32::try_from(extents.len()).map_err(|_| TapeError::LocalCapacity {
            reason: "临时正文计划 extent 数超过表示范围".into(),
        })?;
        let bytes = u64::from(count) * EXTENT_BYTES as u64;
        let file = crate::local_read_storage::download_file(
            directory,
            bytes,
            crate::local_read_storage::ReadStorageBudget {
                download_bytes: budget.index_bytes,
                ..budget
            },
        )?;
        let metadata = file.metadata()?;
        let mut output = BufWriter::with_capacity(CHUNK_BYTES, &file);
        for extent in extents {
            output.write_all(&BodyLayout::extent_bytes(extent))?;
        }
        output.flush()?;
        drop(output);
        let readonly = OpenOptions::new()
            .read(true)
            .open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        let sealed = readonly.metadata()?;
        if sealed.ino() != metadata.ino()
            || sealed.dev() != metadata.dev()
            || sealed.nlink() != 0
            || sealed.len() != bytes
        {
            return Err(TapeError::Catalog("临时正文计划 inode 或长度改变".into()));
        }
        drop(file);
        Ok(Self {
            binding: None,
            path,
            volume: copy.volume_id,
            uid: copy.ltfs_file_uid,
            length: copy.size,
            block_size: label.blocksize,
            partitions,
            extents: count,
            file: Arc::new(readonly),
        })
    }

    pub fn volume(&self) -> Uuid {
        self.volume
    }

    pub fn uid(&self) -> u64 {
        self.uid
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn partitions(&self) -> [char; 2] {
        self.partitions
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn extents(&self) -> PlanExtents {
        PlanExtents {
            file: self.file.clone(),
            remaining: self.extents,
            length: u64::from(self.extents) * EXTENT_BYTES as u64,
            file_offset: 0,
            buffer: vec![0; CHUNK_BYTES].into_boxed_slice(),
            offset: 0,
            filled: 0,
        }
    }
}

/// Independent position-based cursors share only the sealed read-only inode.
/// Cloning a cursor copies one fixed buffer; it never clones all extents.
#[derive(Clone)]
pub struct PlanExtents {
    file: Arc<File>,
    remaining: u32,
    length: u64,
    file_offset: u64,
    buffer: Box<[u8]>,
    offset: usize,
    filled: usize,
}

impl PlanExtents {
    fn record(&mut self) -> TapeResult<[u8; EXTENT_BYTES]> {
        let mut out = [0; EXTENT_BYTES];
        let mut at = 0;
        while at < out.len() {
            if self.offset == self.filled {
                let count = (self.length - self.file_offset).min(CHUNK_BYTES as u64) as usize;
                if count == 0 {
                    return Err(TapeError::Catalog("正文临时读取计划截断".into()));
                }
                self.file
                    .read_exact_at(&mut self.buffer[..count], self.file_offset)?;
                self.file_offset += count as u64;
                self.offset = 0;
                self.filled = count;
            }
            let count = (out.len() - at).min(self.filled - self.offset);
            out[at..at + count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
            self.offset += count;
            at += count;
        }
        Ok(out)
    }
}

impl Iterator for PlanExtents {
    type Item = TapeResult<Extent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        match self.record() {
            Ok(bytes) => {
                self.remaining -= 1;
                Some(Ok(Extent {
                    partition: bytes[0] as char,
                    start_block: u64::from_be_bytes(bytes[1..9].try_into().expect("fixed record")),
                    byte_offset: u64::from_be_bytes(bytes[9..17].try_into().expect("fixed record")),
                    byte_count: u64::from_be_bytes(bytes[17..25].try_into().expect("fixed record")),
                    file_offset: u64::from_be_bytes(
                        bytes[25..33].try_into().expect("fixed record"),
                    ),
                }))
            }
            Err(error) => {
                self.remaining = 0;
                Some(Err(error))
            }
        }
    }
}

impl ManagedCatalog {
    async fn require_read_plan_binding(
        &self,
        binding: &CopyLayoutBinding,
        location: &BindingLocation,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        require_namespace(&tr, binding.namespace).await?;
        require_complete(&tr, binding.namespace, binding.manifest).await?;
        location.require_at(&tr).await?;
        if location.selected_binding_at(&tr, binding).await?.as_ref() != Some(binding) {
            return Err(CatalogError::Corrupt("正文临时读取计划绑定改变"));
        }
        Ok(())
    }

    /// The caller admits this private anonymous file against its disk scratch
    /// budget before calling. Any partial file is consumed and dropped on error.
    pub(crate) async fn prepare_body_read_plan(
        &self,
        binding: &CopyLayoutBinding,
        file: File,
        maximum_bytes: u64,
    ) -> TapeResult<VerifiedPhysicalPlan> {
        self.prepare_body_read_plan_at(binding, &BindingLocation::Ordinary, file, maximum_bytes)
            .await
    }
    pub(in crate::managed_catalog) async fn prepare_body_read_plan_at(
        &self,
        binding: &CopyLayoutBinding,
        location: &BindingLocation,
        mut file: File,
        maximum_bytes: u64,
    ) -> TapeResult<VerifiedPhysicalPlan> {
        binding.validate().map_err(catalog_error)?;
        let path = binding
            .path()
            .strip_prefix('/')
            .ok_or_else(|| TapeError::Catalog("正文布局物理路径缺少卷根".into()))?
            .to_owned();
        let manifest = binding.manifest;
        if u64::from(manifest.len) > maximum_bytes {
            return Err(TapeError::LocalCapacity {
                reason: "FDB 正文布局超过本次临时读取计划磁盘预算".into(),
            });
        }
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 0
            || metadata.mode() & 0o077 != 0
            || metadata.len() != 0
        {
            return Err(TapeError::Catalog(
                "正文读取计划要求空的私有匿名文件".into(),
            ));
        }
        self.require_read_plan_binding(binding, location)
            .await
            .map_err(catalog_error)?;
        let mut hash = Sha256::new();
        hash.update(LAYOUT_DOMAIN);
        let mut reader = LayoutReader {
            catalog: self,
            namespace: binding.namespace,
            manifest,
            next: 0,
            chunk: Vec::new(),
            offset: 0,
            consumed: 0,
            hash,
            location: Some(location.clone()),
        };
        if reader.fixed::<3>().await.map_err(catalog_error)? != *b"BL1" {
            return Err(TapeError::Catalog("正文布局版本".into()));
        }
        let volume = Uuid::from_bytes(reader.fixed().await.map_err(catalog_error)?);
        let uid = u64::from_be_bytes(reader.fixed().await.map_err(catalog_error)?);
        let length = u64::from_be_bytes(reader.fixed().await.map_err(catalog_error)?);
        let block_size = u32::from_be_bytes(reader.fixed().await.map_err(catalog_error)?);
        let partitions = reader
            .fixed::<2>()
            .await
            .map_err(catalog_error)?
            .map(char::from);
        let extents = u32::from_be_bytes(reader.fixed().await.map_err(catalog_error)?);
        if volume != binding.source.volume_uuid
            || uid != binding.body_uid
            || length != binding.length
            || block_size == 0
            || partitions[0] == partitions[1]
            || !partitions.iter().all(|part| matches!(part, 'a' | 'b'))
            || extents != manifest.extents
        {
            return Err(TapeError::Catalog(
                "正文读取计划头与绑定、几何或清单不一致".into(),
            ));
        }
        let mut expected = 0;
        let mut output = BufWriter::with_capacity(CHUNK_BYTES, &mut file);
        for _ in 0..extents {
            let bytes = reader
                .fixed::<EXTENT_BYTES>()
                .await
                .map_err(catalog_error)?;
            let extent = Extent {
                partition: bytes[0] as char,
                start_block: u64::from_be_bytes(bytes[1..9].try_into().expect("fixed record")),
                byte_offset: u64::from_be_bytes(bytes[9..17].try_into().expect("fixed record")),
                byte_count: u64::from_be_bytes(bytes[17..25].try_into().expect("fixed record")),
                file_offset: u64::from_be_bytes(bytes[25..33].try_into().expect("fixed record")),
            };
            validate_layout_extent(&extent, block_size, partitions, &mut expected)
                .map_err(catalog_error)?;
            output.write_all(&bytes)?;
        }
        if expected != length {
            return Err(TapeError::Catalog("正文读取计划 extent 总长度不符".into()));
        }
        output.flush()?;
        drop(output);
        reader.finish().map_err(catalog_error)?;
        self.require_read_plan_binding(binding, location)
            .await
            .map_err(catalog_error)?;
        if file.metadata()?.len() != u64::from(extents) * EXTENT_BYTES as u64 {
            return Err(TapeError::Catalog("正文临时读取计划文件长度不符".into()));
        }
        // Reopen our own unlinked inode read-only, then drop its only writer.
        let readonly = OpenOptions::new()
            .read(true)
            .open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        let reopened = readonly.metadata()?;
        if reopened.dev() != metadata.dev()
            || reopened.ino() != metadata.ino()
            || reopened.nlink() != 0
        {
            return Err(TapeError::Catalog("正文临时读取计划 inode 改变".into()));
        }
        drop(file);
        Ok(VerifiedPhysicalPlan {
            binding: Some(binding.clone()),
            path,
            volume,
            uid,
            length,
            block_size,
            partitions,
            extents,
            file: Arc::new(readonly),
        })
    }
}
