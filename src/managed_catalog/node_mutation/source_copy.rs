use super::*;
use crate::managed_catalog::{ContentDigest, PhysicalCopy};

/// A compact binding to an unchanged EE data entry. The complete entry is
/// hashed from the actual full index before staging and from the actual S4.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileSource {
    path: String,
    uid: u64,
    length: u64,
    checksum: Vec<u8>,
    sha256: [u8; 32],
}

fn ee_path(path: &str, before: &Snapshot) -> Result<Option<String>> {
    let Snapshot::File { node } = before else {
        return Ok(None);
    };
    if crate::managed_catalog::link_kind::classify(&node.xattrs, node.symlink.as_deref())?
        != crate::managed_catalog::link_kind::LinkKind::EeAlias
    {
        return Ok(None);
    }
    let target = node.symlink.as_deref().expect("classified EE alias");
    crate::managed_catalog::import_spool::resolve_ee_alias_target(
        path.trim_start_matches('/'),
        target,
    )
    .map(Some)
    .map_err(|_| invalid("节点 EE 别名的卷内数据路径无效"))
}

impl FileSource {
    fn checksum(&self) -> Result<ContentDigest> {
        let (tag, bytes) = self
            .checksum
            .split_first()
            .ok_or(invalid("节点数据来源缺少摘要算法"))?;
        ContentDigest::decode_compact(*tag, bytes).map_err(|_| invalid("节点数据来源摘要编码无效"))
    }
}

pub(super) fn capture(
    index: &LtfsIndex,
    path: &str,
    before: &Snapshot,
) -> Result<Option<FileSource>> {
    let Some(path) = ee_path(path, before)? else {
        return Ok(None);
    };
    let node = index
        .find_file(&path)
        .ok_or(invalid("节点 EE 数据对象不存在"))?;
    if node.symlink.is_some()
        || node.delta_fields.is_some()
        || node.open_for_write.unwrap_or(false)
        || node.meta.file_uid == 0
        || node.meta.file_uid > index.highest_file_uid
    {
        return Err(invalid("节点 EE 数据对象不是完整、关闭的直接数据文件"));
    }
    let digest = ContentDigest::from_ltfs_xattrs(&node.xattrs)?;
    let mut checksum = vec![digest.tag()];
    checksum.extend_from_slice(&digest.payload()[..digest.compact_len()]);
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/node-source-file/1");
    hash_file(&mut hash, node)?;
    Ok(Some(FileSource {
        path,
        uid: node.meta.file_uid,
        length: node.length,
        checksum,
        sha256: hash.finalize().into(),
    }))
}

pub(super) fn validate(wire: &Wire) -> Result<()> {
    match (ee_path(&wire.path, &wire.before)?, &wire.source_data) {
        (None, None) => {}
        (Some(path), Some(data))
            if path == data.path && data.uid != 0 && data.uid <= wire.highest_uid =>
        {
            crate::managed_catalog::import_spool::validate_path(&data.path)
                .map_err(|_| invalid("节点 EE 数据来源路径无效"))?;
            data.checksum()?;
            let Snapshot::File { node } = &wire.before else {
                unreachable!()
            };
            if node.meta.file_uid == data.uid {
                return Err(invalid("节点 EE 别名与数据对象复用了 UID"));
            }
        }
        _ => return Err(invalid("节点 EE 数据来源与原始别名不符")),
    }
    Ok(())
}

pub(super) fn require(index: &LtfsIndex, wire: &Wire) -> Result<()> {
    if capture(index, &wire.path, &wire.before)? != wire.source_data {
        return Err(invalid("节点计划的数据对象证明与实际完整索引不符"));
    }
    Ok(())
}

pub(super) fn validate_copy(wire: &Wire, copy: &PhysicalCopy) -> Result<()> {
    let Snapshot::File { node } = &wire.before else {
        return Err(invalid("节点源副本要求既有文件或链接"));
    };
    if copy.ltfs_file_uid > wire.highest_uid {
        return Err(invalid("节点源文件 UID 与固定副本映射不符"));
    }
    if let Snapshot::File { node: after } = &wire.after
        && after.symlink != node.symlink
    {
        return Err(invalid("节点属性投影改变了原始符号链接目标"));
    }
    let data = wire
        .source_data
        .as_ref()
        .map(|data| -> Result<_> {
            Ok(PhysicalDataIdentity {
                path: &data.path,
                uid: data.uid,
                length: data.length,
                checksum: data.checksum()?,
            })
        })
        .transpose()?;
    validate_physical_file_copy(copy, node, id(&wire.volume)?, wire.generation, data)?;
    if let Snapshot::File { node: after } = &wire.after
        && !copy
            .media
            .as_ref()
            .is_some_and(|m| m.ee_alias || m.native_symlink)
    {
        validate_physical_file_copy(copy, after, id(&wire.volume)?, wire.generation, None)?;
    }
    Ok(())
}

/// Data identity already bound to a complete, closed physical data entry.
pub(in crate::managed_catalog) struct PhysicalDataIdentity<'a> {
    pub path: &'a str,
    pub uid: u64,
    pub length: u64,
    pub checksum: ContentDigest,
}

/// Shared Node7/rename check. Metadata may have moved since the original data
/// source was committed; the retained data UID and extents do not change.
pub(in crate::managed_catalog) fn validate_physical_file_copy(
    copy: &PhysicalCopy,
    node: &FileNode,
    volume: Uuid,
    generation: u64,
    data: Option<PhysicalDataIdentity<'_>>,
) -> Result<()> {
    if copy.volume_id != volume || copy.volume_generation > generation {
        return Err(invalid("节点数据副本与当前物理来源卷或代数不符"));
    }
    if let Some(media) = &copy.media {
        super::super::validate_media(media, copy.ltfs_file_uid)?;
    }
    let expected_uid = copy
        .media
        .as_ref()
        .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid);
    if node.meta.file_uid != expected_uid {
        return Err(invalid("节点源文件 UID 与固定副本映射不符"));
    }
    let media = copy.media.as_ref();
    if media.is_some_and(|m| m.ee_alias) {
        let data = data.ok_or(invalid("节点 EE 副本缺少数据对象证明"))?;
        if node.symlink.is_none()
            || node.length != 0
            || !node.extents.is_empty()
            || copy.ltfs_file_uid != data.uid
            || copy.size != data.length
            || copy.content_digest != data.checksum
            || media.unwrap().physical_path != data.path
        {
            return Err(invalid(
                "节点 EE 副本与固定数据对象的路径、UID、长度或摘要不符",
            ));
        }
    } else if media.is_some_and(|m| m.native_symlink) {
        if data.is_some()
            || node.symlink.is_none()
            || node.length != 0
            || !node.extents.is_empty()
            || copy.size != 0
            || copy.content_digest != ContentDigest::Unknown
        {
            return Err(invalid("原生链接副本类型或数据身份无效"));
        }
    } else if data.is_some()
        || node.symlink.is_some()
        || node.length != copy.size
        || ContentDigest::from_ltfs_xattrs(&node.xattrs)? != copy.content_digest
    {
        return Err(invalid("节点源文件长度或内容摘要与固定副本不符"));
    }
    Ok(())
}

/// A directory move can change an EE link's physical relative depth while
/// keeping its canonical inode attributes and original data copy unchanged.
/// Native links always retain their literal target.
pub(super) fn logical_link_matches(
    wire: &Wire,
    node: &FileNode,
    attributes: &projection::LogicalAttributes,
) -> Result<bool> {
    logical_file_link_matches(
        node,
        &attributes.xattrs,
        attributes.symlink.as_deref(),
        wire.source_data.as_ref().map(|d| d.path.as_str()),
    )
}

pub(in crate::managed_catalog) fn logical_file_link_matches(
    node: &FileNode,
    logical_xattrs: &[Xattr],
    logical_target: Option<&str>,
    data_path: Option<&str>,
) -> Result<bool> {
    use crate::managed_catalog::link_kind::{LinkKind, classify};
    let actual = classify(&node.xattrs, node.symlink.as_deref())?;
    let logical = classify(logical_xattrs, logical_target)?;
    if actual != logical {
        return Ok(false);
    }
    if node.symlink.as_deref() == logical_target {
        return Ok(true);
    }
    if actual != LinkKind::EeAlias {
        return Ok(false);
    }
    let Some(data_path) = data_path else {
        return Ok(false);
    };
    let Some(target) = logical_target else {
        return Ok(false);
    };
    if target.starts_with('/') || target.is_empty() {
        return Ok(false);
    }
    // Normalize only the data suffix. Leading parent steps reflect the old
    // metadata depth and do not choose the primary copy or the data object.
    let mut parts = Vec::new();
    for part in target.split('/') {
        match part {
            "" => return Ok(false),
            "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    Ok(parts.first() == Some(&".LTFSEE_DATA") && parts.join("/") == data_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{Extent, IndexLocation};

    fn fixture(link: bool) -> (NodeMutationPlan, PhysicalCopy) {
        use sha2::Digest;
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "renamed metadata fixture".into(), 'b');
        index.generation = 3;
        index.self_location = IndexLocation {
            partition: 'b',
            start_block: 30,
        };
        index.previous_location = Some(IndexLocation {
            partition: 'b',
            start_block: 15,
        });
        index.highest_file_uid = 2;
        let mut file = FileNode {
            name: "new-name".into(),
            meta: NodeMeta {
                file_uid: 2,
                ..index.root.meta.clone()
            },
            ..Default::default()
        };
        if link {
            file.symlink = Some("relative-target".into());
            file.xattrs
                .push(crate::managed_catalog::link_kind::native_marker());
        } else {
            file.length = 3;
            file.extents.push(Extent {
                partition: 'b',
                start_block: 10,
                byte_offset: 0,
                byte_count: 3,
                file_offset: 0,
            });
            file.set_xattr(
                "ltfs.hash.sha256sum",
                &format!("{:x}", Sha256::digest(b"abc")),
            );
        }
        index.root.files.push(file.clone());
        let parent = Uuid::new_v4();
        let namespace = Uuid::new_v4();
        let scan = Uuid::new_v4();
        let previous = ActiveIndexIdentity {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(index.to_xml().unwrap()).into(),
            incremental: false,
        };
        let context = NodeMutationContext {
            library_serial: "RENAME".into(),
            namespace,
            pool: Uuid::new_v4(),
            operation: Uuid::new_v4(),
            request_epoch: 1,
            owner_epoch: 100,
            parent,
            parent_version: 1,
            node: Uuid::new_v4(),
            expected_version: Some(1),
            scan,
            previous,
            path: "/new-name".into(),
        };
        let ancestors = [super::super::super::NodeMutationAncestor {
            path: "/".into(),
            id: parent,
            version: 1,
            attributes: NodeAttributes::from_directory(&index.root).unwrap(),
        }];
        let attributes = NodeAttributes::from_file(&file).unwrap();
        let plan = NodeMutationPlan::prepare(
            context,
            NodeMutation::ChangeMetadata {
                readonly: Some(true),
                access_time: None,
                modify_time: None,
            },
            &index,
            "2026-11-02T10:00:00.000000000Z",
            &ancestors,
            Some((super::super::super::NodeKind::File, &attributes)),
        )
        .unwrap();
        let copy = PhysicalCopy {
            version: 1,
            origin: crate::managed_catalog::CopyOrigin::Imported,
            volume_id: index.volume_uuid,
            volume_generation: 2,
            volume_scan_id: scan,
            ltfs_file_uid: 2,
            size: if link { 0 } else { 3 },
            content_digest: if link {
                ContentDigest::Unknown
            } else {
                ContentDigest::Sha256(Sha256::digest(b"abc").into())
            },
            media: Some(crate::managed_catalog::MediaProvenance {
                version: (0, 0),
                logical_file_uid: 2,
                physical_path: "old-name".into(),
                ee_alias: false,
                native_symlink: link,
            }),
        };
        (plan, copy)
    }
    #[test]
    fn moved_native_file_metadata_preserves_original_data_provenance() {
        let (plan, copy) = fixture(false);
        validate_copy(&plan.wire, &copy)
            .expect("current metadata filename is independent of the original data path");
        for corrupt in 0..6 {
            let mut bad = copy.clone();
            match corrupt {
                0 => bad.ltfs_file_uid += 1,
                1 => bad.size += 1,
                2 => bad.content_digest = ContentDigest::Sha256([0; 32]),
                3 => bad.volume_id = Uuid::new_v4(),
                4 => bad.volume_generation = plan.context().previous.generation + 1,
                _ => bad.media.as_mut().unwrap().physical_path = "../escape".into(),
            };
            assert!(validate_copy(&plan.wire, &bad).is_err());
        }
    }
    #[test]
    fn moved_native_link_metadata_preserves_original_link_provenance() {
        let (plan, copy) = fixture(true);
        validate_copy(&plan.wire, &copy)
            .expect("current link name must not rewrite the retained original source");
        let mut bad = copy.clone();
        bad.media.as_mut().unwrap().native_symlink = false;
        assert!(validate_copy(&plan.wire, &bad).is_err());
        let mut bad = copy;
        bad.media.as_mut().unwrap().ee_alias = true;
        assert!(validate_copy(&plan.wire, &bad).is_err());
    }
}
