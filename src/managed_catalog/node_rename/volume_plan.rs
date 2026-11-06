//! An immutable recipe for the original per-volume projection. This retained
//! descriptor is not a physical attempt, S4 proof or media permission.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    volume: String,
    generation: u64,
    partition: char,
    block: u64,
    sha256: [u8; 32],
    incremental: bool,
    scan: String,
    scan_generation: u64,
    barcode: String,
    pool_revision: u64,
    assignment_revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Facts {
    version: u8,
    library: String,
    namespace: String,
    pool: String,
    epoch: u64,
    operation: String,
    request: String,
    pub(super) fingerprint: [u8; 32],
    manifest: [u8; 32],
    initial: [u8; 32],
    prepared_owner: u64,
    prepared_time: u64,
    source: Source,
    pub(super) guard: [u8; 32],
    before: [u8; 32],
    after: [u8; 32],
    pub(super) highest_before: u64,
    highest_after: u64,
    pub(super) before_files: u64,
    pub(super) recovery_abandoned: Option<RecoveryAttribute>,
    pub(super) recovery_marker: Option<RecoveryAttribute>,
    pub(super) files: u64,
    pub(super) directories: u64,
    source_files: u64,
    source_directories: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub facts: Facts,
    pub creator: u64,
    checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameVolumePlan {
    pub(super) record: Record,
}
fn corrupt() -> CatalogError {
    CatalogError::Corrupt("rename 原按卷计划不一致")
}
fn id(text: &str) -> Result<Uuid> {
    node_mutation::id(text).map_err(|_| corrupt())
}
impl Facts {
    pub(super) fn source_files(&self) -> u64 {
        self.source_files
    }
    pub(super) fn from_projection(
        view: &NodeRenameVolumeProjection,
        request_ref: &str,
        manifest: [u8; 32],
        initial: [u8; 32],
        prepared_owner: u64,
        prepared_time: u64,
    ) -> Result<Self> {
        view.verify()?;
        if manifest != view.manifest_sha256() {
            return Err(corrupt());
        }
        let request = view.original_request();
        let b = view.original_binding();
        let s = view.stats();
        let value = Self {
            version: 1,
            library: request.library().into(),
            namespace: request.spec().source.namespace.to_string(),
            pool: request.spec().pool.to_string(),
            epoch: request.request_epoch(),
            operation: request.spec().operation.to_string(),
            request: request_ref.into(),
            fingerprint: request.fingerprint(),
            manifest,
            initial,
            prepared_owner,
            prepared_time,
            source: Source {
                volume: s.source.volume_uuid.to_string(),
                generation: s.source.generation,
                partition: s.source.location.partition,
                block: s.source.location.start_block,
                sha256: s.source.sha256,
                incremental: s.source.incremental,
                scan: b.scan.id.to_string(),
                scan_generation: b.scan.generation,
                barcode: b.placement.barcode.clone(),
                pool_revision: b.pool_revision,
                assignment_revision: b.assignment_revision,
            },
            guard: view.guard_sha256(),
            before: s.before,
            after: s.after,
            highest_before: view.highest_before(),
            highest_after: s.highest_uid,
            before_files: view.before_files(),
            recovery_abandoned: view.recovery_abandoned().clone(),
            recovery_marker: view.recovery_marker().clone(),
            files: s.files,
            directories: s.directories,
            source_files: s.source_files,
            source_directories: s.source_directories,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.epoch == 0
            || self.prepared_owner == 0
            || self.prepared_owner > 0xFFFF_FFFF_FFFF
            || self.prepared_time == 0
            || self.source.generation == 0
            || self.source.scan_generation == 0
            || self.source.scan_generation > self.source.generation
            || !matches!(self.source.partition, 'a' | 'b')
            || self.source.block == 0
            || self.source.pool_revision == 0
            || self.source.assignment_revision == 0
            || self.highest_before == 0
            || self.highest_after < self.highest_before
            || self.directories == 0
            || self.source_files > self.files
            || self.source_directories > self.directories
            || self.files.checked_add(self.directories).is_none()
            || self.request.len() > 4096
        {
            return Err(corrupt());
        }
        if self.before_files < self.files {
            return Err(corrupt());
        }
        for value in [&self.recovery_abandoned, &self.recovery_marker]
            .into_iter()
            .flatten()
        {
            value.validate()?;
        }
        if self
            .recovery_abandoned
            .as_ref()
            .zip(self.recovery_marker.as_ref())
            .is_some_and(|(a, b)| a.position == b.position)
        {
            return Err(corrupt());
        }
        id(&self.operation)?;
        let raw = URL_SAFE_NO_PAD
            .decode(&self.request)
            .map_err(|_| corrupt())?;
        let reference = NodePlanObject::decode(&raw).map_err(|_| corrupt())?;
        if URL_SAFE_NO_PAD.encode(reference.encode()) != self.request {
            return Err(corrupt());
        }
        if reference.namespace() != id(&self.namespace)? || reference.library() != self.library {
            return Err(corrupt());
        }
        validate_placement(&self.binding()?.placement).map_err(|_| corrupt())?;
        Ok(())
    }
    pub(super) fn binding(&self) -> Result<live_guard::Binding> {
        let volume = id(&self.source.volume)?;
        Ok(live_guard::Binding {
            source: ActiveIndexIdentity {
                volume_uuid: volume,
                generation: self.source.generation,
                location: crate::ltfs::index::IndexLocation {
                    partition: self.source.partition,
                    start_block: self.source.block,
                },
                sha256: self.source.sha256,
                incremental: self.source.incremental,
            },
            scan: VolumeScan {
                id: id(&self.source.scan)?,
                generation: self.source.scan_generation,
            },
            placement: VolumePlacement {
                volume_id: volume,
                namespace: id(&self.namespace)?,
                pool_id: id(&self.pool)?,
                library_serial: self.library.clone(),
                barcode: self.source.barcode.clone(),
            },
            pool_revision: self.source.pool_revision,
            assignment_revision: self.source.assignment_revision,
        })
    }
    pub(super) fn require_scope(
        &self,
        request: &NodeRenameRequest,
        reference: &str,
        manifest: [u8; 32],
        initial: [u8; 32],
        owner: u64,
        time: u64,
    ) -> Result<()> {
        if self.library != request.library()
            || self.namespace != request.spec().source.namespace.to_string()
            || self.pool != request.spec().pool.to_string()
            || self.operation != request.spec().operation.to_string()
            || self.epoch != request.request_epoch()
            || self.request != reference
            || self.fingerprint != request.fingerprint()
            || self.manifest != manifest
            || self.initial != initial
            || self.prepared_owner != owner
            || self.prepared_time != time
        {
            return Err(corrupt());
        }
        Ok(())
    }
    pub(super) fn fingerprint(&self) -> [u8; 32] {
        Sha256::new()
            .chain_update(b"tape-rs/node-rename-volume-plan/1")
            .chain_update(node_mutation::canonical(self).expect("validated volume recipe"))
            .finalize()
            .into()
    }
}
impl Record {
    pub(super) fn new(facts: Facts, creator: u64) -> Result<Self> {
        facts.validate()?;
        if creator < facts.prepared_owner || creator > 0xFFFF_FFFF_FFFF {
            return Err(corrupt());
        }
        let checksum = Sha256::new()
            .chain_update(b"tape-rs/node-rename-volume-plan-record/1")
            .chain_update(node_mutation::canonical(&(&facts, creator))?)
            .finalize()
            .into();
        let value = Self {
            facts,
            creator,
            checksum,
        };
        value.encode()?;
        Ok(value)
    }
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 64_000 {
            return Err(corrupt());
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
        let checksum = value.checksum;
        let value = Self::new(value.facts, value.creator)?;
        if value.checksum != checksum {
            return Err(corrupt());
        }
        if value.encode()? != bytes {
            return Err(corrupt());
        }
        Ok(value)
    }
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let bytes = node_mutation::canonical(self)?;
        if bytes.len() > 64_000 {
            return Err(invalid("rename 原按卷计划超过封口预算"));
        }
        Ok(bytes)
    }
}
impl NodeRenameVolumePlan {
    pub(super) fn require_projection(&self, view: &NodeRenameVolumeProjection) -> Result<()> {
        let f = &self.record.facts;
        if Facts::from_projection(
            view,
            &f.request,
            f.manifest,
            f.initial,
            f.prepared_owner,
            f.prepared_time,
        )? != *f
        {
            return Err(corrupt());
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.record.facts.fingerprint()
    }
    pub fn creator_owner_epoch(&self) -> u64 {
        self.record.creator
    }
    pub fn source_index(&self) -> ActiveIndexIdentity {
        self.record
            .facts
            .binding()
            .expect("validated recipe binding")
            .source
    }
    pub fn before_sha256(&self) -> [u8; 32] {
        self.record.facts.before
    }
    pub fn after_sha256(&self) -> [u8; 32] {
        self.record.facts.after
    }
    pub fn highest_file_uid(&self) -> u64 {
        self.record.facts.highest_after
    }
}

#[cfg(test)]
#[path = "volume_plan_tests.rs"]
mod tests;

impl NodeRenameVolumePlan {
    pub(super) fn require_intent_scope(
        &self,
        claim: &node_claims::RenameVolumeClaim,
        logical: [u8; 32],
        created: u64,
    ) -> Result<()> {
        let facts = &self.record.facts;
        facts.validate()?;
        if facts.library != claim.library
            || facts.namespace != claim.namespace.to_string()
            || facts.pool != claim.pool.to_string()
            || facts.epoch != claim.epoch
            || facts.operation != claim.operation.to_string()
            || facts.fingerprint != logical
            || facts.binding()?.source.volume_uuid != claim.volume
            || self.record.creator > claim.owner
            || facts.prepared_owner > claim.owner
            || facts.prepared_time > created
        {
            return Err(corrupt());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryAttribute {
    pub position: u32,
    pub value: String,
}
impl RecoveryAttribute {
    pub(super) fn capture(attrs: &[crate::ltfs::index::Xattr], key: &str) -> Result<Option<Self>> {
        let mut matching = attrs.iter().enumerate().filter(|(_, a)| a.key == key);
        let Some((position, attr)) = matching.next() else {
            return Ok(None);
        };
        if attr.base64
            || matching.next().is_some()
            || attr.value.is_empty()
            || attr.value.len() > 64_000
        {
            return Err(invalid("rename 原修尾属性编码或封口预算不符"));
        }
        let result = Self {
            position: position.try_into().map_err(|_| corrupt())?,
            value: attr.value.clone(),
        };
        result.validate()?;
        Ok(Some(result))
    }
    fn validate(&self) -> Result<()> {
        if self.position >= 8_388_608
            || self.value.is_empty()
            || self.value.len() > 64_000
            || !crate::ltfs::xml_text_valid(&self.value)
        {
            return Err(corrupt());
        }
        Ok(())
    }
}

impl NodeRenameVolumePlan {
    pub(super) fn require_rebase_root(&self, root: &Self) -> Result<()> {
        let a = &self.record.facts;
        let b = &root.record.facts;
        if a.library != b.library
            || a.namespace != b.namespace
            || a.pool != b.pool
            || a.epoch != b.epoch
            || a.operation != b.operation
            || a.request != b.request
            || a.fingerprint != b.fingerprint
            || a.manifest != b.manifest
            || a.initial != b.initial
            || a.prepared_owner != b.prepared_owner
            || a.prepared_time != b.prepared_time
            || a.source.volume != b.source.volume
            || a.source.scan != b.source.scan
            || a.source.scan_generation != b.source.scan_generation
            || a.source.barcode != b.source.barcode
            || a.highest_before != b.highest_before
            || a.highest_after != b.highest_after
            || a.before_files != b.before_files
            || a.files != b.files
            || a.directories != b.directories
            || a.source_files != b.source_files
            || a.source_directories != b.source_directories
        {
            return Err(invalid("rename 新物理配方偏离原逻辑参与卷"));
        }
        Ok(())
    }
}
