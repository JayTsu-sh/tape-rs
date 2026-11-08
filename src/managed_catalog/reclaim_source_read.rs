//! Evidence of source bytes, distinct from destination and erase proofs.
use super::*;

/// Only the owner-fenced reader can construct this evidence from actual bytes
/// and a complete current capture. It is never accepted as HTTP input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimSourceRead(ReadEvidence);

/// Original physical source and its retained byte proof at one FDB read version.
/// The lookup is historical evidence, not a fresh physical read or a write permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimReadSource {
    source: ActiveReadSource,
    proof: ReclaimSourceRead,
}

impl ReclaimReadSource {
    pub fn source(&self) -> &ActiveReadSource {
        &self.source
    }
    pub fn byte_proof(&self) -> &ReclaimSourceRead {
        &self.proof
    }
    pub(crate) fn into_source(self) -> ActiveReadSource {
        self.source
    }
    pub(in crate::managed_catalog) fn new(
        source: ActiveReadSource,
        proof: ReclaimSourceRead,
    ) -> Self {
        Self { source, proof }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadEvidence {
    version: u8,
    parent: String,
    file: ReclaimFile,
    inventory: [u8; 32],
    source_index: Vec<u8>,
    object: Object,
    file_head: Vec<u8>,
    copy_sha256: [u8; 32],
    sha256: [u8; 32],
    md5: Option<[u8; 16]>,
}

impl ReclaimSourceRead {
    pub fn sha256(&self) -> [u8; 32] {
        self.0.sha256
    }

    /// Present only when the original data node records an MD5 and its bytes
    /// were checked against that value, including when SHA-256 is preferred.
    pub fn md5(&self) -> Option<[u8; 16]> {
        self.0.md5
    }

    pub fn source_node(&self) -> ReclaimSourceNode {
        ReclaimSourceNode(self.0.object.clone())
    }

    pub(super) fn object(&self) -> &Object {
        &self.0.object
    }
    pub(super) fn inventory(&self) -> [u8; 32] {
        self.0.inventory
    }

    pub(in crate::managed_catalog) fn value(&self) -> Result<Value> {
        serde_json::to_value(&self.0).map_err(cache)
    }

    pub(in crate::managed_catalog) fn require_source(
        value: &Value,
        parent: &str,
        head: &FileHead,
        copy: &PhysicalCopy,
        intent: &ActiveFileIntent,
    ) -> Result<Self> {
        let proof: ReadEvidence = serde_json::from_value(value.clone()).map_err(cache)?;
        let index = active_volume::decode_index(copy.volume_id, &proof.source_index)?;
        if proof.version != 2
            || proof.parent != parent
            || serde_json::to_value(&proof).map_err(cache)? != *value
            || value["file"]
                != json!({"path":intent.ltfs_path()?,"file":intent.file_id.to_string(),
                "version":intent.expected_version,"size":intent.expected_size,"operation":intent.operation_id.to_string()})
            || proof.file_head != encode_head(head)
            || proof.object.kind != ReclaimSourceNodeKind::File
            || proof.object.uid != copy.ltfs_file_uid
            || proof.object.length != Some(copy.size)
            || proof.copy_sha256
                != <[u8; 32]>::from(Sha256::digest(copy_records::encode_physical_copy(copy)?))
            || intent.expected_digest != ContentDigest::Sha256(proof.sha256)
            || matches!(copy.content_digest, ContentDigest::Sha256(v) if v != proof.sha256)
            || matches!(copy.content_digest, ContentDigest::Md5(v) if Some(v) != proof.md5)
            || index.generation < copy.volume_generation
            || copy.media.as_ref().is_some_and(|m| m.native_symlink)
        {
            return Err(CatalogError::Invalid(
                "回收源实读证明与原文件、物理副本或正文摘要不符",
            ));
        }
        proof.object.encode()?;
        Ok(Self(proof))
    }

    pub(in crate::managed_catalog) fn index(&self) -> &[u8] {
        &self.0.source_index
    }
}

fn expected_md5(node: &crate::ltfs::index::FileNode) -> Result<Option<[u8; 16]>> {
    // Validate both algorithms and reject duplicate/malformed attributes even
    // though the preferred digest may mask the secondary one.
    ContentDigest::from_ltfs_xattrs(&node.xattrs)?;
    node.xattrs
        .iter()
        .find(|attr| attr.key == crate::ltfs::index::XATTR_MD5)
        .map(crate::managed_catalog::parse_ltfs_hex::<16>)
        .transpose()
}

impl ReclaimSourceInventory<'_> {
    pub(crate) fn expected_read_md5(&self, uid: u64) -> Result<Option<[u8; 16]>> {
        match self.source.entry_by_uid(uid).map_err(cache)? {
            Some(IndexEntry::File { node, .. }) if node.symlink.is_none() => expected_md5(&node),
            _ => Err(CatalogError::Invalid("回收数据 UID 不是普通文件")),
        }
    }

    pub(crate) fn bind_verified_read(
        &self,
        reservation: &ReclaimReservation,
        file: &ReclaimFile,
        read: &ResolvedRead,
        verified_head: ActiveIndexIdentity,
        sha256: [u8; 32],
        md5: Option<[u8; 16]>,
    ) -> Result<ReclaimSourceRead> {
        let mut index = Vec::new();
        active_volume::encode_index(&mut index, verified_head);
        let spec = reservation.spec();
        if self.summary.volume != spec.volume.to_string()
            || self.summary.head != index
            || read.copy.volume_id != spec.volume
            || read.placement.barcode != spec.barcode
            || read.placement.namespace != reservation.namespace()
            || read.placement.pool_id != spec.pool
            || read.placement.library_serial != reservation.serial()
            || read.id != file.file_id()
            || read.head.version != file.version()
            || read.copy.version != read.head.version
            || read.head.size != file.size()
            || read.copy.size != file.size()
            || read.physical.length != file.size()
            || read.physical.symlink.is_some()
            || read.copy.media.as_ref().is_some_and(|m| m.native_symlink)
        {
            return Err(CatalogError::Invalid(
                "回收源实读不属于当前完整源清单或原扫描文件",
            ));
        }
        if let ContentDigest::Sha256(expected) = read.copy.content_digest
            && expected != sha256
        {
            return Err(CatalogError::Invalid("回收源实读 SHA-256 与已知正文不一致"));
        }
        let entry = self
            .source
            .entry_by_uid(read.copy.ltfs_file_uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File { node, .. } = &entry else {
            return Err(CatalogError::Invalid("回收数据 UID 不是文件"));
        };
        // Native rename may change the current path, but LTFS body locations
        // must match exactly. EE aliases use their data UID, never link UID.
        if node.symlink.is_some()
            || node.length != read.physical.length
            || node.extents != read.physical.extents
        {
            return Err(CatalogError::Invalid(
                "当前源对象的正文 extent 与实读来源不同",
            ));
        }
        if expected_md5(node)? != md5
            || matches!(read.copy.content_digest, ContentDigest::Md5(v) if Some(v) != md5)
        {
            return Err(CatalogError::Invalid("回收源实读 MD5 与原始数据节点不一致"));
        }
        if matches!(ContentDigest::from_ltfs_xattrs(&node.xattrs)?, ContentDigest::Sha256(v) if v != sha256)
        {
            return Err(CatalogError::Invalid(
                "回收源实读 SHA-256 与原始数据节点不一致",
            ));
        }
        Ok(ReclaimSourceRead(ReadEvidence {
            version: 2,
            parent: reservation.fingerprint(),
            file: file.clone(),
            inventory: self.summary.digest,
            source_index: index,
            object: Object::capture(entry)?,
            file_head: encode_head(&read.head),
            copy_sha256: Sha256::digest(copy_records::encode_physical_copy(&read.copy)?).into(),
            sha256,
            md5,
        }))
    }
}
