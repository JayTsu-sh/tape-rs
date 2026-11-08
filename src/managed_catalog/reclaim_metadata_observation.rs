//! Read-only current-media evidence. No catalog mutation, I/O admission,
//! claim release, metadata coverage or source erase authority is produced.
use super::*;
use metadata_plan::TargetTree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimMetadataObservationKind {
    Unchanged,
    Committed,
}

/// A live, owner/attempt-bound result produced by the guarded device actor.
/// Unlike a retained observation, this type cannot be restored from FDB.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimMetadataCommitEvidence =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimMetadataCommitEvidence {
    owner: u64,
    attempt: Uuid,
    scan: Uuid,
    plan: [u8; 32],
    observation: ReclaimMetadataObservation,
}
impl ReclaimMetadataCommitEvidence {
    pub fn observation(&self) -> &ReclaimMetadataObservation {
        &self.observation
    }
    pub(crate) fn from_current(
        task: &ReclaimMetadataWrite,
        owner: u64,
        target: &CapturedIndexSource,
        predecessor: Option<&CapturedIndexSource>,
    ) -> Result<Self> {
        let observation = task.plan().observe_current(target, predecessor)?;
        if !task.writing()
            || task.owner() != owner
            || observation.kind() != ReclaimMetadataObservationKind::Committed
            || task.observation()?.as_ref() != Some(&observation)
        {
            return Err(CatalogError::Invalid("缺少原任务本轮实读提交证明"));
        }
        Ok(Self {
            owner,
            attempt: task.id(),
            scan: task.scan_id(),
            plan: task.plan().fingerprint()?,
            observation,
        })
    }
    pub(crate) fn require(
        &self,
        task: &ReclaimMetadataWrite,
        owner: u64,
    ) -> Result<&ReclaimMetadataObservation> {
        if self.owner != owner
            || task.owner() != owner
            || self.attempt != task.id()
            || self.scan != task.scan_id()
            || self.plan != task.plan().fingerprint()?
        {
            return Err(CatalogError::Invalid(
                "元数据实读证明改变原 owner、尝试或计划",
            ));
        }
        Ok(&self.observation)
    }
}

#[cfg(test)]
#[path = "reclaim_metadata_observation_tests.rs"]
mod tests;

/// Constructed only from the original plan and a verified current on-disk view.
/// Unchanged describes the observed clean head, never permission to replay I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMetadataObservation {
    plan: [u8; 32],
    current: ActiveIndexIdentity,
    kind: ReclaimMetadataObservationKind,
    physical_files: u64,
}
impl ReclaimMetadataObservation {
    pub fn plan_fingerprint(&self) -> [u8; 32] {
        self.plan
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.current
    }
    pub fn kind(&self) -> ReclaimMetadataObservationKind {
        self.kind
    }
    pub fn physical_files(&self) -> u64 {
        self.physical_files
    }

    // Private durable codec: callers cannot deserialize their own evidence.
    pub(super) fn encode(&self, plan: &ReclaimMetadataPlan) -> Result<Vec<u8>> {
        let mut raw = vec![
            1,
            match self.kind {
                ReclaimMetadataObservationKind::Unchanged => 0,
                ReclaimMetadataObservationKind::Committed => 1,
            },
        ];
        raw.extend_from_slice(&self.plan);
        active_volume::encode_index(&mut raw, self.current);
        raw.extend_from_slice(&self.physical_files.to_be_bytes());
        raw.extend_from_slice(&Self::checksum(&raw));
        if Self::decode(plan, &raw)? != *self {
            return Err(CatalogError::Corrupt("回收元数据观察的卷身份不同"));
        }
        Ok(raw)
    }

    pub(super) fn decode(plan: &ReclaimMetadataPlan, raw: &[u8]) -> Result<Self> {
        if raw.len() != 124 || raw[0] != 1 || raw[1] > 1 {
            return Err(CatalogError::Corrupt("回收元数据观察版本或长度无效"));
        }
        if Self::checksum(&raw[..92]) != raw[92..124] {
            return Err(CatalogError::Corrupt("回收元数据观察完整性校验失败"));
        }
        let fingerprint = plan.fingerprint()?;
        let current = active_volume::decode_index(plan.volume(), &raw[34..84])?;
        let previous = active_volume::decode_index(plan.volume(), &plan.0.previous)?;
        let files = u64::from_be_bytes(raw[84..92].try_into().unwrap());
        let kind = if raw[1] == 0 {
            ReclaimMetadataObservationKind::Unchanged
        } else {
            ReclaimMetadataObservationKind::Committed
        };
        let matches = match kind {
            ReclaimMetadataObservationKind::Unchanged => {
                current == previous && files == plan.0.physical_files
            }
            ReclaimMetadataObservationKind::Committed => {
                previous.generation.checked_add(1) == Some(current.generation)
                    && current.location.partition == previous.location.partition
                    && current.location.start_block > previous.location.start_block
                    && plan.0.physical_files.checked_add(1) == Some(files)
            }
        };
        if raw[2..34] != fingerprint || !matches {
            return Err(CatalogError::Corrupt("回收元数据观察与固定计划不同"));
        }
        Ok(Self {
            plan: fingerprint,
            current,
            kind,
            physical_files: files,
        })
    }

    fn checksum(raw: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-metadata-observation/1");
        hash.update(raw);
        hash.finalize().into()
    }
}

impl ReclaimMetadataPlan {
    /// Observe a clean current DP head and its fully replayed object tree.
    /// Every original object, including root/empty-directory/link attributes,
    /// must remain exact; a committed result adds only the fixed new raw node.
    /// Full heads additionally require an actual recorded read of the original
    /// predecessor: their header pointer alone cannot prove its raw SHA.
    pub fn observe_current(
        &self,
        target: &CapturedIndexSource,
        predecessor: Option<&CapturedIndexSource>,
    ) -> Result<ReclaimMetadataObservation> {
        self.encode()?;
        current_proof(target.scope(), target.header())?;
        let current: ActiveIndexIdentity = target.construct().into();
        let previous = active_volume::decode_index(self.volume(), &self.0.previous)?;
        let original = self.previous_source()?;
        let header = target.header();
        if current.volume_uuid != self.volume()
            || header.volume_uuid != current.volume_uuid
            || header.generation != current.generation
            || header.self_location != current.location
        {
            return Err(CatalogError::Invalid(
                "回收元数据观察的当前卷头不属于原计划",
            ));
        }
        let kind = if current == previous {
            if header.highest_file_uid.checked_add(1) != Some(self.uid())
                || target.identity().map_err(cache)? != original
            {
                return Err(CatalogError::Invalid(
                    "回收元数据未变化卷头的 UID 或来源链与原计划不同",
                ));
            }
            ReclaimMetadataObservationKind::Unchanged
        } else {
            let linked = if current.incremental {
                let chain = target.predecessors();
                chain.first() == Some(&original.head())
                    && chain.get(1..) == Some(original.predecessors())
            } else {
                let pointer = if previous.incremental {
                    header.previous_incremental_location
                } else {
                    header.previous_location
                };
                match predecessor {
                    Some(source) if source.scope() == IndexSourceScope::Recorded => {
                        pointer == Some(previous.location)
                            && source.identity().map_err(cache)? == original
                    }
                    _ => false,
                }
            };
            if previous.generation.checked_add(1) != Some(current.generation)
                || current.location.partition != previous.location.partition
                || current.location.start_block <= previous.location.start_block
                || header.highest_file_uid != self.uid()
                || !linked
            {
                return Err(CatalogError::Invalid(
                    "回收元数据观察偏离原代次、UID 或索引前驱",
                ));
            }
            ReclaimMetadataObservationKind::Committed
        };
        let donor = target
            .entry_by_uid(self.0.donor.uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if Object::capture(donor)? != self.0.donor {
            return Err(CatalogError::Invalid("回收元数据观察的原正文节点改变"));
        }
        let mut tree = TargetTree::new(self.volume());
        let mut added = false;
        let mut files = 0u64;
        target
            .for_each_entry(|entry| {
                if matches!(entry, IndexEntry::File { .. }) {
                    files = files.checked_add(1).ok_or_else(|| {
                        callback(CatalogError::Invalid("回收元数据观察文件计数溢出"))
                    })?;
                }
                let object = Object::capture(entry).map_err(callback)?;
                if object.uid == self.uid() {
                    if kind != ReclaimMetadataObservationKind::Committed
                        || added
                        || object != self.0.expected
                    {
                        return Err(callback(CatalogError::Invalid(
                            "回收元数据观察的新节点与固定计划不同",
                        )));
                    }
                    added = true;
                } else {
                    tree.add(&object).map_err(callback)?;
                }
                Ok(())
            })
            .map_err(cache)?;
        let expected_files = if kind == ReclaimMetadataObservationKind::Committed {
            self.0.physical_files.checked_add(1)
        } else {
            Some(self.0.physical_files)
        };
        if added != (kind == ReclaimMetadataObservationKind::Committed)
            || Some(files) != expected_files
            || tree.objects != self.0.target_objects
            || tree.finish() != self.0.target_tree
        {
            return Err(CatalogError::Invalid(
                "回收元数据观察改变原对象树或缺少固定新增节点",
            ));
        }
        Ok(ReclaimMetadataObservation {
            plan: self.fingerprint()?,
            current,
            kind,
            physical_files: files,
        })
    }
}
