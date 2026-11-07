//! Raw directory/native-link readback evidence, separate from body relocation,
//! logical publication, complete source coverage and erase authorization.
use super::*;

#[path = "reclaim_node_observations.rs"]
mod observations;

fn encode_observation(evidence: &NodeEvidence) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(evidence).map_err(cache)?;
    if raw.len() > 90_000 {
        return Err(CatalogError::Invalid("原始节点元数据证明超出预算"));
    }
    Ok(raw)
}

fn observation_fingerprint(raw: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/reclaim-node-observation/1");
    hash.update((raw.len() as u64).to_be_bytes());
    hash.update(raw);
    hash.finalize().into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReclaimNodeDestination {
    pub volume: Uuid,
    pub uid: u64,
}

/// One source node and its already-loaded physical destination in a read batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimNodeReadRequest {
    pub original: ReclaimSourceNode,
    pub destination: ReclaimNodeDestination,
}

/// Only the owner-fenced actual-index reader constructs this observation.
/// It has no public deserializer and grants no authority to write or format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimNodeMetadata(NodeEvidence);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeEvidence {
    version: u8,
    parent: String,
    inventory: [u8; 32],
    source: Object,
    target: Object,
    target_volume: String,
    target_barcode: String,
    target_index: Vec<u8>,
    target_attributes: [u8; 32],
}

impl ReclaimNodeMetadata {
    pub fn source_uid(&self) -> u64 {
        self.0.source.uid
    }
    pub fn destination(&self) -> ReclaimNodeDestination {
        ReclaimNodeDestination {
            volume: Uuid::parse_str(&self.0.target_volume).expect("verified target volume"),
            uid: self.0.target.uid,
        }
    }
    pub fn target_path(&self) -> &str {
        &self.0.target.path
    }
    pub fn fingerprint(&self) -> Result<[u8; 32]> {
        Ok(observation_fingerprint(&encode_observation(&self.0)?))
    }
}

fn attributes_of(entry: &IndexEntry) -> Result<NodeAttributes> {
    // Shared with the raw LTFS staging kernel: reject invalid copied values
    // and incomplete/non-metadata nodes even if both sides compare equal.
    entry.validate_reclaimed_metadata().map_err(cache)?;
    match entry {
        IndexEntry::Directory { path, node } => {
            ee_internal::require_location(&node.xattrs, path, NodeKind::Directory)?;
            NodeAttributes::from_directory(node)
        }
        IndexEntry::File { node, .. } => {
            let attrs = NodeAttributes::from_file(node)?;
            if attrs.link_kind()? != link_kind::LinkKind::Native {
                return Err(CatalogError::Invalid("EE 别名必须使用正文及别名迁移证明"));
            }
            Ok(attrs)
        }
    }
}

fn require_metadata(before: &IndexEntry, after: &IndexEntry) -> Result<[u8; 32]> {
    let source = attributes_of(before)?;
    let target = attributes_of(after)?;
    match (before, after) {
        (IndexEntry::Directory { .. }, IndexEntry::Directory { .. }) => {}
        (IndexEntry::File { node: a, .. }, IndexEntry::File { node: b, .. })
            if a.length == b.length && a.open_for_write == b.open_for_write => {}
        _ => {
            return Err(CatalogError::Invalid(
                "回收原始节点类型、长度或关闭状态改变",
            ));
        }
    }
    let original = source.meta().ok_or(CatalogError::Incomplete)?;
    let mut actual = target.meta().ok_or(CatalogError::Incomplete)?.clone();
    actual.file_uid = original.file_uid;
    if &actual != original
        || source.symlink() != target.symlink()
        || metadata::xattrs(source.xattrs().ok_or(CatalogError::Incomplete)?)?
            != metadata::xattrs(target.xattrs().ok_or(CatalogError::Incomplete)?)?
    {
        return Err(CatalogError::Invalid(
            "目的节点未完整保留原时间、只读、链接及扩展属性",
        ));
    }
    Ok(target.digest())
}

impl ReclaimSourceInventory<'_> {
    pub(crate) fn bind_verified_node_metadata(
        &self,
        reservation: &ReclaimReservation,
        original: &ReclaimSourceNode,
        destination: ReclaimNodeDestination,
        placement: &VolumePlacement,
        verified_head: ActiveIndexIdentity,
        captured: &CapturedIndexSource,
    ) -> Result<ReclaimNodeMetadata> {
        let spec = reservation.spec();
        current_proof(captured.scope(), captured.header())?;
        if self.summary.volume != spec.volume.to_string()
            || reservation.intent["index"] != json!(self.summary.head)
            || destination.uid == 0
            || destination.volume.is_nil()
            || destination.volume == spec.volume
            || placement.volume_id != destination.volume
            || placement.namespace != reservation.namespace()
            || placement.pool_id != spec.pool
            || placement.library_serial != reservation.serial()
            || verified_head.volume_uuid != destination.volume
            || ActiveIndexIdentity::from(captured.construct()) != verified_head
        {
            return Err(CatalogError::Invalid(
                "原始节点目的实读不属于原回收池或实际索引",
            ));
        }
        let source = self
            .source
            .entry_by_uid(original.uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let target = captured
            .entry_by_uid(destination.uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let target_attributes = require_metadata(&source, &target)?;
        let source = Object::capture(source)?;
        if source != original.0 {
            return Err(CatalogError::Invalid("原始元数据节点与实际封存源不同"));
        }
        let mut target_index = Vec::new();
        active_volume::encode_index(&mut target_index, verified_head);
        let proof = NodeEvidence {
            version: 1,
            parent: reservation.fingerprint(),
            inventory: self.summary.digest,
            source,
            target: Object::capture(target)?,
            target_volume: destination.volume.to_string(),
            target_barcode: placement.barcode.clone(),
            target_index,
            target_attributes,
        };
        encode_observation(&proof)?;
        Ok(ReclaimNodeMetadata(proof))
    }
}

#[cfg(test)]
#[path = "reclaim_node_metadata_tests.rs"]
mod tests;
