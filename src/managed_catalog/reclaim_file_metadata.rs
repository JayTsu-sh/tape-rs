//! File/EE alias metadata coverage, separate from body and erase proofs.
use super::super::{children, walk};
use super::*;
use crate::ltfs::index::{FileNode, XATTR_MD5, XATTR_SHA256, Xattr};
use std::collections::BTreeMap;

/// Constructed only from owner-fenced reads of actual current index nodes.
/// No public deserializer accepts caller-supplied evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimFileMetadata(MetadataEvidence);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataEvidence {
    version: u8,
    parent: String,
    request: [u8; 32],
    inventory: [u8; 32],
    source_copy: [u8; 32],
    data: Object,
    node: Object,
    target: Object,
    target_copy: Vec<u8>,
    target_head: Vec<u8>,
    target_index: Vec<u8>,
    target_attributes: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimMetadataProgress {
    version: u8,
    parent: String,
    inventory: [u8; 32],
    files: u64,
    aliases: u64,
    total_files: u64,
    total_symlinks: u64,
}
impl ReclaimMetadataProgress {
    pub fn files(&self) -> u64 {
        self.files
    }
    pub fn aliases(&self) -> u64 {
        self.aliases
    }
}

// Compare all user attributes without relying on their XML order. Reject
// duplicates even for fields whose value is deliberately regenerated.
pub(super) fn xattrs(attrs: &[Xattr]) -> Result<BTreeMap<&str, (&str, bool)>> {
    let mut result = BTreeMap::new();
    for attr in attrs {
        if result
            .insert(attr.key.as_str(), (attr.value.as_str(), attr.base64))
            .is_some()
        {
            return Err(CatalogError::Invalid("迁移元数据包含重复扩展属性"));
        }
    }
    Ok(result)
}

fn require_attributes(source: &NodeAttributes, target: &FileNode, sha: [u8; 32]) -> Result<()> {
    let (expected, _) = source.data_relocation()?;
    let mut actual_meta = target.meta.clone();
    actual_meta.file_uid = expected.meta.file_uid;
    if actual_meta != expected.meta || target.symlink.is_some() {
        return Err(CatalogError::Invalid(
            "迁移未保留原时间、只读属性或节点类型",
        ));
    }
    let original = source.xattrs().ok_or(CatalogError::Incomplete)?;
    let mut before = xattrs(original)?;
    let mut after = xattrs(&target.xattrs)?;
    // Both hash fields must be well-formed even when SHA-256 takes precedence.
    let old_hash = ContentDigest::from_ltfs_xattrs(original)?;
    if matches!(old_hash, ContentDigest::Sha256(value) if value != sha)
        || ContentDigest::from_ltfs_xattrs(&target.xattrs)? != ContentDigest::Sha256(sha)
    {
        return Err(CatalogError::Invalid("迁移元数据的正文摘要不一致"));
    }
    // The append recomputes an existing MD5. Case changes in hex are harmless;
    // losing it, changing its bytes or adding an unproved MD5 is not.
    let md5 = |attrs: &[Xattr]| -> Result<Option<ContentDigest>> {
        let attrs: Vec<_> = attrs
            .iter()
            .filter(|x| x.key == XATTR_MD5)
            .cloned()
            .collect();
        Ok(match ContentDigest::from_ltfs_xattrs(&attrs)? {
            ContentDigest::Unknown => None,
            digest => Some(digest),
        })
    };
    if md5(original)? != md5(&target.xattrs)? {
        return Err(CatalogError::Invalid("迁移未保留原 MD5 属性"));
    }
    for key in [XATTR_SHA256, XATTR_MD5, crate::ltfs::volume::XATTR_VERSION] {
        before.remove(key);
        after.remove(key);
    }
    if before != after {
        return Err(CatalogError::Invalid("迁移未完整保留用户扩展属性"));
    }
    Ok(())
}

impl ReclaimSourceInventory<'_> {
    pub(crate) fn bind_verified_metadata(
        &self,
        reservation: &ReclaimReservation,
        request: &ActiveFileRequest,
        source: &PhysicalCopy,
        target: (&ResolvedRead, ActiveIndexIdentity),
        captured: &CapturedIndexSource,
    ) -> Result<ReclaimFileMetadata> {
        request.validate()?;
        current_proof(captured.scope(), captured.header())?;
        let (read, verified_head) = target;
        let spec = reservation.spec();
        let ContentDigest::Sha256(sha) = request.expected_digest else {
            return Err(CatalogError::Incomplete);
        };
        if request.namespace != reservation.namespace()
            || request.library_serial != reservation.serial()
            || request.pool_id != Some(spec.pool)
            || source.volume_id != spec.volume
            || source.size != request.expected_size
            || Some(source.version) != request.expected_version
            || read.id != request.file_id
            || Some(
                read.head
                    .version
                    .checked_sub(1)
                    .ok_or(CatalogError::Incomplete)?,
            ) != request.expected_version
            || read.head.size != request.expected_size
            || read.copy.version != read.head.version
            || read.copy.size != request.expected_size
            || read.copy.content_digest != request.expected_digest
            || read.copy.volume_id == spec.volume
            || read.copy.volume_id != verified_head.volume_uuid
            || ActiveIndexIdentity::from(captured.construct()) != verified_head
            || read.placement.namespace != reservation.namespace()
            || read.placement.pool_id != spec.pool
            || read.placement.library_serial != reservation.serial()
            || read.physical.symlink.is_some()
            || read.copy.media.is_some()
        {
            return Err(CatalogError::Invalid("元数据证明与原请求或目的实读不符"));
        }
        let data_entry = self
            .source
            .entry_by_uid(source.ltfs_file_uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File {
            node: data_node, ..
        } = &data_entry
        else {
            return Err(CatalogError::Invalid("原数据对象不是文件"));
        };
        if data_node.symlink.is_some() || data_node.length != source.size {
            return Err(CatalogError::Invalid("原数据对象类型或长度已改变"));
        }
        let data = Object::capture(data_entry)?;
        let logical_uid = source
            .media
            .as_ref()
            .map_or(source.ltfs_file_uid, |m| m.logical_file_uid);
        let entry = self
            .source
            .entry_by_uid(logical_uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File { path, node } = &entry else {
            return Err(CatalogError::Invalid("原元数据对象不是文件或 EE 别名"));
        };
        let attrs = NodeAttributes::from_file(node)?;
        match attrs.link_kind()? {
            link_kind::LinkKind::None
                if logical_uid == data.uid
                    && !source
                        .media
                        .as_ref()
                        .is_some_and(|m| m.ee_alias || m.native_symlink) => {}
            link_kind::LinkKind::EeAlias
                if source.media.as_ref().is_some_and(|m| {
                    m.ee_alias && !m.native_symlink && m.physical_path == data.path
                }) && import_spool::resolve_ee_alias_target(
                    path,
                    attrs.symlink().ok_or(CatalogError::Incomplete)?,
                )
                .map_err(cache)?
                    == data.path => {}
            _ => {
                return Err(CatalogError::Invalid(
                    "原节点不是对应数据的普通文件或 EE 别名",
                ));
            }
        }
        let target_entry = captured
            .entry_by_uid(read.copy.ltfs_file_uid)
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File {
            path: target_path,
            node: target_node,
        } = &target_entry
        else {
            return Err(CatalogError::Invalid("目的元数据对象不是文件"));
        };
        if target_path != request.ltfs_path()?.trim_start_matches('/')
            || target_path != &read.physical.path
            || target_node.length != read.physical.length
            || target_node.length != read.copy.size
            || target_node.extents != read.physical.extents
        {
            return Err(CatalogError::Invalid("目的元数据与实际读取位置不一致"));
        }
        let target_attrs = NodeAttributes::from_file(target_node)?;
        if read.attributes.as_ref().map(NodeAttributes::digest) != Some(target_attrs.digest()) {
            return Err(CatalogError::Invalid("目的介质属性与 FDB 已发布属性不同"));
        }
        require_attributes(&attrs, target_node, sha)?;
        let mut target_index = Vec::new();
        active_volume::encode_index(&mut target_index, verified_head);
        Ok(ReclaimFileMetadata(MetadataEvidence {
            version: 1,
            parent: reservation.fingerprint(),
            request: request.fingerprint(),
            inventory: self.summary.digest,
            source_copy: Sha256::digest(copy_records::encode_physical_copy(source)?).into(),
            data,
            node: Object::capture(entry)?,
            target: Object::capture(target_entry)?,
            target_copy: copy_records::encode_physical_copy(&read.copy)?,
            target_head: encode_head(&read.head),
            target_index,
            target_attributes: target_attrs.digest(),
        }))
    }
}

pub(in super::super) async fn require_no_metadata_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    for kind in *b"hk" {
        if requests::prefix_has_rows(tr, &ledger::own_key(reservation, kind)).await? {
            return Err(CatalogError::Corrupt(
                "回收元数据账本在预期空状态下仍有记录",
            ));
        }
    }
    Ok(())
}

impl ManagedCatalog {
    /// Seal a raw metadata proof and body/retirement evidence in one transaction.
    /// This counts only the exact file/alias node, never directories or blobs
    /// whose attributes were not materialized at the destination.
    pub async fn seal_reclaim_file_metadata(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        evidence: &ReclaimFileMetadata,
    ) -> Result<ReclaimMetadataProgress> {
        request.validate()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            Box::pin(seal_metadata_at(
                                &tr,
                                reservation,
                                owner,
                                file,
                                request,
                                &evidence.0,
                            ))
                            .await
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

impl ManagedCatalog {
    /// Recover already sealed metadata after a lost reply or HA restart.
    /// The original persisted proof is revalidated; no medium read is repeated.
    pub async fn resume_reclaim_file_metadata(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<Option<ReclaimMetadataProgress>> {
        request.validate()?;
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            if request.namespace != reservation.namespace()
                                || request.pool_id != Some(reservation.spec().pool)
                            {
                                return Err(CatalogError::Invalid(
                                    "元数据恢复必须属于原命名空间和池",
                                ));
                            }
                            let (intent, _) = children::child_at(
                                &tr,
                                request.namespace,
                                reservation.spec().pool,
                                request.request_epoch,
                                request.operation_id,
                                owner,
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if intent.request() != *request {
                                return Err(CatalogError::Invalid("元数据恢复必须复用原请求"));
                            }
                            let child = decode(
                                &tr.get(
                                    &children::child_key(
                                        reservation.namespace(),
                                        &reservation.token(),
                                        request.operation_id,
                                    )?,
                                    false,
                                )
                                .await?
                                .ok_or(CatalogError::Incomplete)?,
                            )?;
                            let copy = copy_records::decode_copy(
                                &serde_json::from_value::<Vec<u8>>(child["source_copy"].clone())
                                    .map_err(cache)?,
                            )?;
                            let uid = copy
                                .media
                                .as_ref()
                                .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
                            let mut key = ledger::own_key(reservation, b'h');
                            key.extend_from_slice(&uid.to_be_bytes());
                            let Some(raw) = tr.get(&key, false).await? else {
                                return Ok(None);
                            };
                            let proof: MetadataEvidence =
                                serde_json::from_slice(&raw).map_err(cache)?;
                            if serde_json::to_vec(&proof).map_err(cache)?.as_slice() != raw.as_ref()
                            {
                                return Err(CatalogError::Corrupt("封存元数据证明编码不规范"));
                            }
                            Box::pin(seal_metadata_at(
                                &tr,
                                reservation,
                                owner,
                                file,
                                request,
                                &proof,
                            ))
                            .await
                            .map(Some)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

async fn seal_metadata_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    file: &ReclaimFile,
    request: &ActiveFileRequest,
    proof: &MetadataEvidence,
) -> Result<ReclaimMetadataProgress> {
    let manifest = ledger::require_complete_at(tr, reservation).await?;
    let target = decode(
        &tr.get(&walk::verification_key(reservation, file), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let child = decode(
        &tr.get(
            &children::child_key(
                reservation.namespace(),
                &reservation.token(),
                request.operation_id,
            )?,
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?,
    )?;
    let source_raw: Vec<u8> =
        serde_json::from_value(child["source_copy"].clone()).map_err(cache)?;
    let source = copy_records::decode_copy(&source_raw)?;
    let logical_uid = source
        .media
        .as_ref()
        .map_or(source.ltfs_file_uid, |m| m.logical_file_uid);
    if proof.version != 1
        || proof.parent != reservation.fingerprint()
        || proof.request != request.fingerprint()
        || proof.inventory != manifest.summary.digest
        || proof.source_copy != <[u8; 32]>::from(Sha256::digest(&source_raw))
        || proof.node.uid != logical_uid
        || proof.data.uid != source.ltfs_file_uid
        || (proof.node.kind == ReclaimSourceNodeKind::Symlink)
            != source.media.as_ref().is_some_and(|m| m.ee_alias)
        || !matches!(
            proof.node.kind,
            ReclaimSourceNodeKind::File | ReclaimSourceNodeKind::Symlink
        )
        || target["head"] != json!(proof.target_head)
        || target["copy"] != json!(proof.target_copy)
        || target["index"] != json!(proof.target_index)
        || proof.target.kind != ReclaimSourceNodeKind::File
    {
        return Err(CatalogError::Invalid(
            "原始元数据证明与原子请求、源副本或目的回执不符",
        ));
    }
    for object in [&proof.node, &proof.data] {
        if tr
            .get(&ledger::object_key(reservation, object.uid), false)
            .await?
            .as_deref()
            != Some(object.encode()?.as_slice())
        {
            return Err(CatalogError::Corrupt("元数据证明不属于完整原始源清单"));
        }
    }
    // Published attribute changes after the read invalidate coverage.
    let full = pool_catalog_path(reservation.spec().pool, request.ltfs_path()?.as_bytes())?;
    let Some(ResolvedPath::File {
        attributes: Some(attrs),
        ..
    }) = lookup::resolve_active_path_at_mode(
        tr,
        reservation.namespace(),
        lookup::path_components(&full)?,
        false,
    )
    .await?
    else {
        return Err(CatalogError::Incomplete);
    };
    if attrs.digest() != proof.target_attributes {
        return Err(CatalogError::Invalid("目的元数据在核验后发生变化"));
    }
    let mut key = ledger::own_key(reservation, b'h');
    key.extend_from_slice(&proof.node.uid.to_be_bytes());
    let raw = serde_json::to_vec(proof).map_err(cache)?;
    if raw.len() > 90_000 {
        return Err(CatalogError::Invalid("回收元数据证明超出持久化预算"));
    }
    let previous = tr.get(&key, false).await?;
    if previous.as_ref().is_some_and(|p| p.as_ref() != raw) {
        return Err(CatalogError::Corrupt("原元数据对象的目的证明冲突"));
    }
    let progress_key = ledger::own_key(reservation, b'k');
    let existing = tr.get(&progress_key, false).await?;
    let mut progress = if let Some(existing) = &existing {
        let value: ReclaimMetadataProgress = serde_json::from_slice(existing).map_err(cache)?;
        if serde_json::to_vec(&value).map_err(cache)?.as_slice() != existing.as_ref()
            || value.version != 1
            || value.parent != reservation.fingerprint()
            || value.inventory != manifest.summary.digest
            || value.total_files != manifest.summary.counts.files
            || value.total_symlinks != manifest.summary.counts.symlinks
            || value.files > value.total_files
            || value.aliases > value.total_symlinks
        {
            return Err(CatalogError::Corrupt("元数据迁移计数损坏"));
        }
        value
    } else {
        require_no_metadata_at(tr, reservation).await?;
        ReclaimMetadataProgress {
            version: 1,
            parent: reservation.fingerprint(),
            inventory: manifest.summary.digest,
            files: 0,
            aliases: 0,
            total_files: manifest.summary.counts.files,
            total_symlinks: manifest.summary.counts.symlinks,
        }
    };
    if previous.is_none() {
        let count = if proof.node.kind == ReclaimSourceNodeKind::File {
            &mut progress.files
        } else {
            &mut progress.aliases
        };
        *count = count
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("元数据迁移计数溢出"))?;
        if progress.files > progress.total_files || progress.aliases > progress.total_symlinks {
            return Err(CatalogError::Corrupt("元数据迁移超出原清单"));
        }
    }
    let progress_raw = serde_json::to_vec(&progress).map_err(cache)?;
    // All fallible metadata work precedes the shared helper's writes.
    Box::pin(relocation::seal_data_at(
        tr,
        reservation,
        owner,
        file,
        request,
    ))
    .await?;
    tr.set(&key, &raw);
    tr.set(&progress_key, &progress_raw);
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::NodeMeta;

    #[test]
    fn relocation_metadata_preserves_all_user_fields_and_validates_hashes() {
        let sha = [0xab; 32];
        let md5 = "CD".repeat(16);
        let original = NodeAttributes::new(
            NodeMeta {
                readonly: true,
                file_uid: 7,
                creation_time: "2020-01-02T03:04:05.000000000Z".into(),
                change_time: "2021-01-02T03:04:05.000000000Z".into(),
                modify_time: "2022-01-02T03:04:05.000000000Z".into(),
                access_time: "2023-01-02T03:04:05.000000000Z".into(),
                backup_time: "2024-01-02T03:04:05.000000000Z".into(),
            },
            vec![
                Xattr {
                    key: "custom.binary".into(),
                    value: "AP9B".into(),
                    base64: true,
                },
                Xattr {
                    key: XATTR_MD5.into(),
                    value: md5,
                    base64: false,
                },
                Xattr {
                    key: crate::ltfs::volume::XATTR_VERSION.into(),
                    value: "1.2".into(),
                    base64: false,
                },
            ],
            Some("../.LTFSEE_DATA/blob".into()),
        )
        .unwrap();
        let mut target = FileNode {
            meta: original.meta().unwrap().clone(),
            xattrs: original.xattrs().unwrap().to_vec(),
            ..Default::default()
        };
        target.meta.file_uid = 99;
        target.set_xattr(XATTR_MD5, &"cd".repeat(16));
        target.set_xattr(XATTR_SHA256, &"ab".repeat(32));
        target.set_xattr(crate::ltfs::volume::XATTR_VERSION, "9.10");
        target.xattrs.reverse();
        require_attributes(&original, &target, sha).unwrap();
        for change in 0..11 {
            let mut bad = target.clone();
            match change {
                0 => bad.meta.readonly = false,
                1 => bad.meta.backup_time.clear(),
                2 => bad.meta.access_time.clear(),
                3 => bad.xattrs.retain(|x| x.key != "custom.binary"),
                4 => {
                    bad.xattrs
                        .iter_mut()
                        .find(|x| x.key == "custom.binary")
                        .unwrap()
                        .base64 = false
                }
                5 => bad.set_xattr("unknown.user.field", "unexpected"),
                6 => bad.set_xattr(XATTR_MD5, &"11".repeat(16)),
                7 => bad.xattrs.retain(|x| x.key != XATTR_MD5),
                8 => bad.set_xattr(XATTR_SHA256, &"ac".repeat(32)),
                9 => bad.xattrs.push(bad.xattrs[0].clone()),
                _ => bad.symlink = Some("opaque".into()),
            }
            assert!(
                require_attributes(&original, &bad, sha).is_err(),
                "case {change}"
            );
        }
        let native = NodeAttributes::new(
            original.meta().unwrap().clone(),
            vec![],
            Some("../outside".into()),
        )
        .unwrap();
        assert!(require_attributes(&native, &target, sha).is_err());
    }
}
