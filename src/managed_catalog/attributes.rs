//! Exact LTFS node attributes kept with the logical inode, not the local extent cache.

use crate::ltfs::index::{DirectoryNode, FileNode, NodeMeta, Xattr};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use super::{CatalogError, Result};

pub(super) const INLINE_LIMIT: usize = 64_000;
pub(super) const CHUNK_SIZE: usize = 64_000;
// Encoding bound, independent of transaction and processing budgets. The v1
// reference uses u16 chunk numbers; conversions must never silently truncate.
pub(super) const MAX_REPRESENTABLE: usize = u16::MAX as usize * CHUNK_SIZE;
const DOMAIN: &[u8] = b"tape-rs/node-attributes/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AttributeManifest {
    pub digest: [u8; 32],
    pub len: u32,
    pub chunks: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AttributeStorage {
    Complete {
        meta: NodeMeta,
        xattrs: Vec<Xattr>,
        symlink: Option<String>,
        encoded: Vec<u8>,
        digest: [u8; 32],
    },
    External(AttributeManifest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAttributes {
    storage: Arc<AttributeStorage>,
}

impl NodeAttributes {
    pub fn new(meta: NodeMeta, xattrs: Vec<Xattr>, symlink: Option<String>) -> Result<Self> {
        if meta.file_uid == 0 {
            return Err(CatalogError::Invalid("LTFS 节点 fileuid 必须大于零"));
        }
        super::link_kind::classify(&xattrs, symlink.as_deref())?;
        if super::ee_internal::declared(&xattrs)? && symlink.is_some() {
            return Err(CatalogError::Invalid("符号链接不能携带 EE 内部目录标记"));
        }
        let encoded = encode(&meta, &xattrs, symlink.as_deref())?;
        let digest = hash(&encoded);
        Ok(Self {
            storage: Arc::new(AttributeStorage::Complete {
                meta,
                xattrs,
                symlink,
                encoded,
                digest,
            }),
        })
    }

    pub fn from_file(node: &FileNode) -> Result<Self> {
        super::ee_internal::require_location(&node.xattrs, "", super::NodeKind::File)?;
        Self::new(node.meta.clone(), node.xattrs.clone(), node.symlink.clone())
    }

    pub fn from_directory(node: &DirectoryNode) -> Result<Self> {
        Self::new(node.meta.clone(), node.xattrs.clone(), None)
    }

    pub fn meta(&self) -> Option<&NodeMeta> {
        match self.storage.as_ref() {
            AttributeStorage::Complete { meta, .. } => Some(meta),
            AttributeStorage::External(_) => None,
        }
    }

    pub fn xattrs(&self) -> Option<&[Xattr]> {
        match self.storage.as_ref() {
            AttributeStorage::Complete { xattrs, .. } => Some(xattrs),
            AttributeStorage::External(_) => None,
        }
    }

    pub fn symlink(&self) -> Option<&str> {
        match self.storage.as_ref() {
            AttributeStorage::Complete { symlink, .. } => symlink.as_deref(),
            AttributeStorage::External(_) => None,
        }
    }

    /// 普通内容替换保留逻辑属性，重新分配时间、UID及内容摘要。
    /// 同时返回原属性编码长度，供整个媒体批次累计读取预算。
    pub(crate) fn content_replacement(&self) -> Result<(crate::ltfs::volume::FileMetadata, usize)> {
        let AttributeStorage::Complete {
            meta,
            xattrs,
            encoded,
            ..
        } = self.storage.as_ref()
        else {
            return Err(CatalogError::Incomplete);
        };
        if self.link_kind()? == super::link_kind::LinkKind::Native {
            return Err(CatalogError::Invalid("普通内容覆盖不能替换原生符号链接"));
        }
        let mut replacement = crate::ltfs::volume::FileMetadata::replacement(
            xattrs
                .iter()
                .filter(|attr| attr.key != crate::ltfs::volume::XATTR_VERSION)
                .cloned()
                .collect(),
        );
        replacement.meta.readonly = meta.readonly;
        Ok((replacement, encoded.len()))
    }

    /// Relocating bytes preserves logical timestamps and access flags. Physical
    /// UID, extents and hashes are supplied by the destination append itself.
    pub(crate) fn data_relocation(&self) -> Result<(crate::ltfs::volume::FileMetadata, usize)> {
        let AttributeStorage::Complete {
            meta,
            xattrs,
            encoded,
            ..
        } = self.storage.as_ref()
        else {
            return Err(CatalogError::Incomplete);
        };
        if self.link_kind()? == super::link_kind::LinkKind::Native {
            return Err(CatalogError::Invalid(
                "原生符号链接必须作为节点迁移，不能读取其目标",
            ));
        }
        Ok((
            crate::ltfs::volume::FileMetadata {
                meta: meta.clone(),
                xattrs: xattrs
                    .iter()
                    .filter(|attr| attr.key != crate::ltfs::volume::XATTR_VERSION)
                    .cloned()
                    .collect(),
            },
            encoded.len(),
        ))
    }

    pub(super) fn link_kind(&self) -> Result<super::link_kind::LinkKind> {
        super::link_kind::classify(
            self.xattrs().ok_or(CatalogError::Incomplete)?,
            self.symlink(),
        )
    }

    pub fn digest(&self) -> [u8; 32] {
        match self.storage.as_ref() {
            AttributeStorage::Complete { digest, .. } => *digest,
            AttributeStorage::External(manifest) => manifest.digest,
        }
    }

    pub(super) fn bytes(&self) -> Option<&[u8]> {
        match self.storage.as_ref() {
            AttributeStorage::Complete { encoded, .. } => Some(encoded),
            AttributeStorage::External(_) => None,
        }
    }

    pub(super) fn inline_bytes(&self) -> Option<&[u8]> {
        self.bytes().filter(|bytes| bytes.len() <= INLINE_LIMIT)
    }

    pub(super) fn manifest(&self) -> Option<AttributeManifest> {
        match self.storage.as_ref() {
            AttributeStorage::External(manifest) => Some(*manifest),
            AttributeStorage::Complete {
                encoded, digest, ..
            } if encoded.len() > INLINE_LIMIT => Some(AttributeManifest {
                digest: *digest,
                len: encoded.len() as u32,
                chunks: encoded.len().div_ceil(CHUNK_SIZE) as u16,
            }),
            _ => None,
        }
    }

    pub(super) fn is_external(&self) -> bool {
        matches!(self.storage.as_ref(), AttributeStorage::External(_))
    }

    pub(super) fn from_manifest(manifest: AttributeManifest) -> Result<Self> {
        let len = manifest.len as usize;
        if !(INLINE_LIMIT + 1..=MAX_REPRESENTABLE).contains(&len)
            || manifest.chunks as usize != len.div_ceil(CHUNK_SIZE)
        {
            return Err(CatalogError::Corrupt("LTFS 属性分块清单长度"));
        }
        Ok(Self {
            storage: Arc::new(AttributeStorage::External(manifest)),
        })
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_REPRESENTABLE {
            return Err(CatalogError::Corrupt("LTFS 节点属性长度超出编码格式范围"));
        }
        Self::decode_owned(bytes.to_vec())
    }

    pub(super) fn decode_owned(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_REPRESENTABLE {
            return Err(CatalogError::Corrupt("LTFS 节点属性过大"));
        }
        let mut at = 0;
        if take(&bytes, &mut at, 1)?[0] != 1 {
            return Err(CatalogError::Corrupt("LTFS 节点属性版本"));
        }
        let readonly = match take(&bytes, &mut at, 1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(CatalogError::Corrupt("LTFS 只读位")),
        };
        let file_uid = u64::from_be_bytes(take(&bytes, &mut at, 8)?.try_into().unwrap());
        let mut times = Vec::with_capacity(5);
        for _ in 0..5 {
            let len = u16::from_be_bytes(take(&bytes, &mut at, 2)?.try_into().unwrap()) as usize;
            let value = std::str::from_utf8(take(&bytes, &mut at, len)?)
                .map_err(|_| CatalogError::Corrupt("LTFS 时间编码"))?;
            times.push(value.to_owned());
        }
        let count = u16::from_be_bytes(take(&bytes, &mut at, 2)?.try_into().unwrap()) as usize;
        let mut xattrs = Vec::with_capacity(count);
        for _ in 0..count {
            let key_len =
                u16::from_be_bytes(take(&bytes, &mut at, 2)?.try_into().unwrap()) as usize;
            let key = std::str::from_utf8(take(&bytes, &mut at, key_len)?)
                .map_err(|_| CatalogError::Corrupt("LTFS xattr 键编码"))?
                .to_owned();
            let value_len =
                u32::from_be_bytes(take(&bytes, &mut at, 4)?.try_into().unwrap()) as usize;
            let value = std::str::from_utf8(take(&bytes, &mut at, value_len)?)
                .map_err(|_| CatalogError::Corrupt("LTFS xattr 值编码"))?
                .to_owned();
            let base64 = match take(&bytes, &mut at, 1)?[0] {
                0 => false,
                1 => true,
                _ => return Err(CatalogError::Corrupt("LTFS xattr 类型")),
            };
            xattrs.push(Xattr { key, value, base64 });
        }
        let symlink = match take(&bytes, &mut at, 1)?[0] {
            0 => None,
            1 => {
                let len =
                    u32::from_be_bytes(take(&bytes, &mut at, 4)?.try_into().unwrap()) as usize;
                Some(
                    std::str::from_utf8(take(&bytes, &mut at, len)?)
                        .map_err(|_| CatalogError::Corrupt("LTFS 符号链接编码"))?
                        .to_owned(),
                )
            }
            _ => return Err(CatalogError::Corrupt("LTFS 符号链接标志")),
        };
        if at != bytes.len() {
            return Err(CatalogError::Corrupt("LTFS 节点属性尾部数据"));
        }
        if file_uid == 0 {
            return Err(CatalogError::Corrupt("LTFS 节点属性 fileuid 必须大于零"));
        }
        super::link_kind::classify(&xattrs, symlink.as_deref())
            .map_err(|_| CatalogError::Corrupt("LTFS 原生符号链接类型标记无效"))?;
        if super::ee_internal::declared(&xattrs)
            .map_err(|_| CatalogError::Corrupt("EE 内部目录归属标记无效"))?
            && symlink.is_some()
        {
            return Err(CatalogError::Corrupt("符号链接携带 EE 内部目录标记"));
        }
        let meta = NodeMeta {
            readonly,
            creation_time: times.remove(0),
            change_time: times.remove(0),
            modify_time: times.remove(0),
            access_time: times.remove(0),
            backup_time: times.remove(0),
            file_uid,
        };
        let digest = hash(&bytes);
        Ok(Self {
            storage: Arc::new(AttributeStorage::Complete {
                meta,
                xattrs,
                symlink,
                encoded: bytes,
                digest,
            }),
        })
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    hash.update(bytes);
    hash.finalize().into()
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(len)
        .ok_or(CatalogError::Corrupt("LTFS 节点属性长度溢出"))?;
    let value = bytes
        .get(*at..end)
        .ok_or(CatalogError::Corrupt("LTFS 节点属性截断"))?;
    *at = end;
    Ok(value)
}

fn put_text(out: &mut Vec<u8>, value: &str) -> Result<()> {
    let len = u16::try_from(value.len())
        .map_err(|_| CatalogError::Invalid("LTFS 时间或 xattr 键过长"))?;
    if out.len().saturating_add(2 + value.len()) > MAX_REPRESENTABLE {
        return Err(CatalogError::Invalid("LTFS 节点属性长度超出编码格式范围"));
    }
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode(meta: &NodeMeta, xattrs: &[Xattr], symlink: Option<&str>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.push(1);
    out.push(u8::from(meta.readonly));
    out.extend_from_slice(&meta.file_uid.to_be_bytes());
    for value in [
        &meta.creation_time,
        &meta.change_time,
        &meta.modify_time,
        &meta.access_time,
        &meta.backup_time,
    ] {
        put_text(&mut out, value)?;
    }
    let count =
        u16::try_from(xattrs.len()).map_err(|_| CatalogError::Invalid("LTFS xattr 数量过多"))?;
    out.extend_from_slice(&count.to_be_bytes());
    for xattr in xattrs {
        put_text(&mut out, &xattr.key)?;
        let len = u32::try_from(xattr.value.len())
            .map_err(|_| CatalogError::Invalid("LTFS xattr 值过长"))?;
        if out.len().saturating_add(5 + xattr.value.len()) > MAX_REPRESENTABLE {
            return Err(CatalogError::Invalid("LTFS 节点属性长度超出编码格式范围"));
        }
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(xattr.value.as_bytes());
        out.push(u8::from(xattr.base64));
    }
    match symlink {
        None => out.push(0),
        Some(target) => {
            out.push(1);
            let len = u32::try_from(target.len())
                .map_err(|_| CatalogError::Invalid("LTFS 符号链接过长"))?;
            if out.len().saturating_add(4 + target.len()) > MAX_REPRESENTABLE {
                return Err(CatalogError::Invalid("LTFS 节点属性长度超出编码格式范围"));
            }
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(target.as_bytes());
        }
    }
    if out.len() > MAX_REPRESENTABLE {
        return Err(CatalogError::Invalid("LTFS 节点属性长度超出编码格式范围"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_replacement_preserves_attributes_without_reusing_source_identity() {
        let binary = Xattr {
            key: "user.binary".into(),
            value: "AP9B".into(),
            base64: true,
        };
        let attrs = NodeAttributes::new(
            NodeMeta {
                readonly: true,
                file_uid: 99,
                modify_time: "2020-01-02T03:04:05.000000000Z".into(),
                ..Default::default()
            },
            vec![
                binary.clone(),
                Xattr {
                    key: crate::ltfs::volume::XATTR_VERSION.into(),
                    value: "17.23".into(),
                    base64: false,
                },
            ],
            None,
        )
        .unwrap();
        let (replacement, bytes) = attrs.content_replacement().unwrap();
        assert_eq!(bytes, attrs.bytes().unwrap().len());
        assert_eq!(replacement.xattrs, vec![binary.clone()]);
        assert!(replacement.meta.readonly);
        assert_eq!(replacement.meta.file_uid, 0);
        assert_ne!(
            replacement.meta.modify_time,
            attrs.meta().unwrap().modify_time
        );
        assert_eq!(attrs.meta().unwrap().file_uid, 99);
        assert_eq!(attrs.xattrs().unwrap().len(), 2);
        let (relocated, bytes) = attrs.data_relocation().unwrap();
        assert_eq!(bytes, attrs.bytes().unwrap().len());
        assert_eq!(relocated.meta, *attrs.meta().unwrap());
        assert_eq!(relocated.xattrs, vec![binary]);
    }

    #[test]
    fn content_replacement_rejects_native_links_and_missing_attribute_content() {
        let meta = NodeMeta {
            file_uid: 1,
            ..Default::default()
        };
        for markers in [vec![], vec![super::super::link_kind::native_marker()]] {
            let native =
                NodeAttributes::new(meta.clone(), markers, Some("missing".into())).unwrap();
            assert!(matches!(
                native.content_replacement(),
                Err(CatalogError::Invalid(_))
            ));
            assert!(matches!(
                native.data_relocation(),
                Err(CatalogError::Invalid(_))
            ));
        }
        let external = NodeAttributes::from_manifest(AttributeManifest {
            digest: [1; 32],
            len: (INLINE_LIMIT + 1) as u32,
            chunks: (INLINE_LIMIT + 1).div_ceil(CHUNK_SIZE) as u16,
        })
        .unwrap();
        assert!(matches!(
            external.content_replacement(),
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            external.data_relocation(),
            Err(CatalogError::Incomplete)
        ));
        let alias = NodeAttributes::new(meta, vec![], Some(".LTFSEE_DATA/body".into())).unwrap();
        assert!(alias.content_replacement().is_ok());
        assert_eq!(
            alias.data_relocation().unwrap().0.meta,
            *alias.meta().unwrap()
        );
    }

    #[test]
    fn roundtrip_preserves_exact_ltfs_fields_and_rejects_corruption() {
        let attrs = NodeAttributes::new(
            NodeMeta {
                readonly: true,
                creation_time: "2026-10-23T00:00:00.000000000Z".into(),
                change_time: "2026-10-23T00:00:01.000000000Z".into(),
                modify_time: "2026-10-23T00:00:02.000000000Z".into(),
                access_time: "2026-10-23T00:00:03.000000000Z".into(),
                backup_time: "2026-10-23T00:00:04.000000000Z".into(),
                file_uid: u64::MAX,
            },
            vec![Xattr {
                key: "origin".into(),
                value: "5pel5pys".into(),
                base64: true,
            }],
            Some(".LTFSEE_DATA/目标".into()),
        )
        .unwrap();
        assert_eq!(
            NodeAttributes::decode(attrs.bytes().unwrap()).unwrap(),
            attrs
        );
        assert_eq!(attrs.symlink(), Some(".LTFSEE_DATA/目标"));
        assert_ne!(
            attrs.digest(),
            NodeAttributes::new(
                NodeMeta {
                    file_uid: 1,
                    ..Default::default()
                },
                vec![],
                None
            )
            .unwrap()
            .digest()
        );
        let mut truncated = attrs.bytes().unwrap().to_vec();
        truncated.pop();
        assert!(NodeAttributes::decode(&truncated).is_err());
        let mut invalid = attrs.bytes().unwrap().to_vec();
        invalid[1] = 2;
        assert!(NodeAttributes::decode(&invalid).is_err());
        let large = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                ..Default::default()
            },
            (0..9)
                .map(|i| Xattr {
                    key: format!("perf_padding_{i}"),
                    value: "x".repeat(65_536),
                    base64: false,
                })
                .collect(),
            None,
        )
        .unwrap();
        assert!(large.manifest().is_some());
        assert_eq!(
            NodeAttributes::decode(large.bytes().unwrap()).unwrap(),
            large
        );
        let over_old_limit = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                ..Default::default()
            },
            vec![Xattr {
                key: "big".into(),
                value: "x".repeat(12_000_000),
                base64: false,
            }],
            None,
        )
        .unwrap();
        assert_eq!(
            NodeAttributes::decode(over_old_limit.bytes().unwrap()).unwrap(),
            over_old_limit
        );
        let manifest = over_old_limit.manifest().unwrap();
        assert!(manifest.len > 4_000_000);
        assert!(
            NodeAttributes::from_manifest(AttributeManifest {
                len: u32::MAX,
                ..manifest
            })
            .is_err()
        );
        assert!(
            NodeAttributes::from_manifest(AttributeManifest {
                chunks: 0,
                ..manifest
            })
            .is_err()
        );
    }
}
