//! Strict decoding of the existing canonical inventory representation.
//! A decoded row alone is inert; only a complete trusted manifest binds it.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use std::collections::BTreeMap;

const LIMIT: usize = 8 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    path: String,
    destination: String,
    node: NodeWire,
    attributes: AttributesWire,
    object_metadata: Option<ObjectWire>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum NodeWire {
    #[serde(rename = "directory")]
    Directory {
        id: String,
        version: u64,
        attributes: [u8; 32],
    },
    #[serde(rename = "file")]
    File {
        id: String,
        head: HeadWire,
        copy: Box<CopyWire>,
        placement: PlacementWire,
        attributes: [u8; 32],
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadWire {
    version: u64,
    size: u64,
    copy: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyWire {
    version: u64,
    origin: Vec<u8>,
    volume: String,
    generation: u64,
    scan: String,
    uid: u64,
    size: u64,
    digest_kind: u8,
    digest: [u8; 32],
    media: Option<MediaWire>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaWire {
    version: (u64, u64),
    logical_uid: u64,
    path: String,
    ee_alias: bool,
    native_symlink: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlacementWire {
    namespace: String,
    library: String,
    pool: String,
    barcode: String,
}
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum AttributesWire {
    Inline {
        inline: String,
    },
    External {
        digest: [u8; 32],
        len: u32,
        chunks: u16,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectWire {
    modified: Option<i64>,
    headers: BTreeMap<String, String>,
}

fn corrupt() -> CatalogError {
    CatalogError::Corrupt("rename 清单节点编码或身份无效")
}

fn attributes(value: AttributesWire) -> Result<NodeAttributes> {
    match value {
        AttributesWire::Inline { inline } => {
            let bytes = URL_SAFE_NO_PAD.decode(inline).map_err(|_| corrupt())?;
            let attributes = NodeAttributes::decode(&bytes)?;
            if attributes.manifest().is_some() {
                return Err(corrupt());
            }
            Ok(attributes)
        }
        AttributesWire::External {
            digest,
            len,
            chunks,
        } => NodeAttributes::from_manifest(AttributeManifest {
            digest,
            len,
            chunks,
        }),
    }
}
fn origin(raw: &[u8]) -> Result<CopyOrigin> {
    match raw {
        [2] => Ok(CopyOrigin::Imported),
        [1, epoch @ ..] if epoch.len() == 8 => Ok(CopyOrigin::FileRequest {
            request_epoch: u64::from_be_bytes(epoch.try_into().map_err(|_| corrupt())?),
        }),
        [3, rest @ ..] if rest.len() == 24 => Ok(CopyOrigin::NodeLink {
            management_epoch: u64::from_be_bytes(rest[..8].try_into().map_err(|_| corrupt())?),
            operation: Uuid::from_bytes(rest[8..].try_into().map_err(|_| corrupt())?),
        }),
        _ => Err(corrupt()),
    }
}
fn copy(value: CopyWire) -> Result<PhysicalCopy> {
    let copy = PhysicalCopy {
        version: value.version,
        origin: origin(&value.origin)?,
        volume_id: node_mutation::id(&value.volume)?,
        volume_generation: value.generation,
        volume_scan_id: node_mutation::id(&value.scan)?,
        ltfs_file_uid: value.uid,
        size: value.size,
        content_digest: ContentDigest::decode(value.digest_kind, &value.digest)?,
        media: value.media.map(|media| MediaProvenance {
            version: media.version,
            logical_file_uid: media.logical_uid,
            physical_path: media.path,
            ee_alias: media.ee_alias,
            native_symlink: media.native_symlink,
        }),
    };
    // Reuse the current R1 invariants; do not add a permissive second decoder.
    copy_records::encode_physical_copy(&copy)?;
    Ok(copy)
}
fn object(value: Option<ObjectWire>) -> Result<Option<ObjectMetadata>> {
    value
        .map(|wire| {
            let text = serde_json::to_string(&wire.headers).map_err(|_| corrupt())?;
            let object = ObjectMetadata::from_parts(wire.modified, Some((&text, false)))?;
            if object.headers != wire.headers {
                return Err(corrupt());
            }
            Ok(object)
        })
        .transpose()
}
fn path(value: &str) -> Result<()> {
    if value.len() > MAX_POOL_PATH_BYTES
        || value == "/"
        || !crate::ltfs::xml_text_valid(value)
        || lookup::path_components(value.as_bytes())?.len() >= usize::from(MAX_DEPTH)
    {
        return Err(corrupt());
    }
    Ok(())
}

impl NodeRenameSubtreeEntry {
    /// Strict current-format decoding. It neither re-resolves paths nor grants
    /// canonical capture, publication or physical execution authority.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > LIMIT {
            return Err(invalid("rename 清单节点超过字节预算"));
        }
        let wire: Wire = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
        if wire.version != 1 {
            return Err(corrupt());
        }
        path(&wire.path)?;
        path(&wire.destination)?;
        let attributes = attributes(wire.attributes)?;
        let metadata = object(wire.object_metadata)?;
        let node = match wire.node {
            NodeWire::Directory {
                id,
                version,
                attributes: digest,
            } => {
                if version == 0
                    || attributes.digest() != digest
                    || attributes.symlink().is_some()
                    || metadata.is_some()
                {
                    return Err(corrupt());
                }
                ResolvedPath::Directory {
                    id: node_mutation::id(&id)?,
                    version,
                    attributes: Some(attributes),
                }
            }
            NodeWire::File {
                id,
                head,
                copy: wire_copy,
                placement,
                attributes: digest,
            } => {
                let head = FileHead {
                    version: head.version,
                    size: head.size,
                    copy_id: node_mutation::id(&head.copy)?,
                };
                decode_head(&encode_head(&head))?;
                let copy = copy(*wire_copy)?;
                if head.version != copy.version
                    || head.size != copy.size
                    || attributes.digest() != digest
                {
                    return Err(corrupt());
                }
                let placement = VolumePlacement {
                    volume_id: copy.volume_id,
                    namespace: node_mutation::id(&placement.namespace)?,
                    pool_id: node_mutation::id(&placement.pool)?,
                    library_serial: placement.library,
                    barcode: placement.barcode,
                };
                control::validate_placement(&placement)?;
                ResolvedPath::File {
                    id: node_mutation::id(&id)?,
                    head,
                    copy: Box::new(copy),
                    placement,
                    attributes: Some(attributes),
                    object_metadata: metadata,
                }
            }
        };
        let entry = Self {
            path: wire.path,
            destination: wire.destination,
            node,
        };
        if entry.encode()? != bytes {
            return Err(corrupt());
        }
        Ok(entry)
    }
}

pub(super) fn decode_entries(
    bytes: &[u8],
    count: u64,
    request: &NodeRenameRequest,
) -> Result<Vec<NodeRenameSubtreeEntry>> {
    if bytes.len() > LIMIT || count > MAX_PAGE as u64 {
        return Err(corrupt());
    }
    let mut offset = 0usize;
    let mut rows = Vec::new();
    for _ in 0..count {
        let end = offset
            .checked_add(8)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(corrupt)?;
        let len = u64::from_be_bytes(bytes[offset..end].try_into().map_err(|_| corrupt())?);
        offset = end;
        let end = offset
            .checked_add(usize::try_from(len).map_err(|_| corrupt())?)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(corrupt)?;
        let entry = NodeRenameSubtreeEntry::decode(&bytes[offset..end])?;
        let spec = request.spec();
        let suffix = entry.path.strip_prefix(&spec.from).ok_or_else(corrupt)?;
        if (!suffix.is_empty() && !suffix.starts_with('/'))
            || entry.destination != format!("{}{suffix}", spec.to)
        {
            return Err(corrupt());
        }
        if let ResolvedPath::File { placement, .. } = &entry.node
            && (placement.namespace != spec.source.namespace
                || placement.pool_id != spec.pool
                || placement.library_serial != request.library())
        {
            return Err(corrupt());
        }
        rows.push(entry);
        offset = end;
    }
    if offset != bytes.len() {
        return Err(corrupt());
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
