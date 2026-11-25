//! Actual source extraction for import. Public DTOs are comparison inputs only.
//! The guarded actor capability and original FDB snapshot wrap this private
//! kernel; this module deliberately exposes no constructor from a staged DTO.
use super::*;
use crate::ltfs::index::{FileNode, IndexEntry};
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::CapturedIndexSource;

/// Extraction alone does not grant publication or device authority. The one
/// decoded body's extent Vec is moved here, never duplicated for staging.
struct CapturedImportBody {
    source: IndexSourceIdentity,
    scan: Uuid,
    logical_uid: u64,
    media_version: (u64, u64),
    ee_alias: bool,
    physical_path: String,
    content_digest: ContentDigest,
    layout: BodyLayout<'static>,
}

enum CapturedImportEntry {
    Data(CapturedImportBody),
    MetadataOnly,
}

impl CapturedImportBody {
    // The original projection transaction chooses the inode/copy/version and
    // encodes exact R first. No stage record predicts or reserves those IDs.
    fn bind_exact_copy(
        &self,
        namespace: Uuid,
        file: Uuid,
        copy_id: Uuid,
        copy: &PhysicalCopy,
        exact_raw: &[u8],
    ) -> Result<CopyLayoutBinding> {
        let media = copy
            .media
            .as_ref()
            .ok_or(CatalogError::Corrupt("导入正文缺少原物理投影"))?;
        if !matches!(decode_stored_copy(exact_raw)?, StoredCopy::Present(actual) if &actual == copy)
            || copy.origin != CopyOrigin::Imported
            || copy.volume_id != self.layout.volume
            || copy.volume_generation != self.source.head().generation
            || copy.volume_scan_id != self.scan
            || copy.ltfs_file_uid != self.layout.uid
            || copy.size != self.layout.length
            || copy.content_digest != self.content_digest
            || media.native_symlink
            || media.logical_file_uid != self.logical_uid
            || media.version != self.media_version
            || media.ee_alias != self.ee_alias
            || format!("/{}", media.physical_path) != self.physical_path
        {
            return Err(CatalogError::Corrupt("导入最终 R 与实际正文来源不同"));
        }
        let binding = CopyLayoutBinding {
            namespace,
            file,
            copy: copy_id,
            version: copy.version,
            scan: copy.volume_scan_id,
            body_uid: copy.ltfs_file_uid,
            length: copy.size,
            content_digest: copy.content_digest,
            source: self.source.head().into(),
            provenance: LayoutProvenance::VerifiedCopySource {
                copy_record_sha: copy_record_digest(exact_raw),
                source_identity_sha: self.source.fingerprint(),
                source_identity: self.source.clone(),
            },
            path: self.physical_path.clone(),
            manifest: self.layout.manifest()?,
        };
        binding.validate()?;
        if !binding.matches_raw_copy(Some(exact_raw))
            || !binding.matches_copy(namespace, file, copy_id, copy, self.source.head().into())
        {
            return Err(CatalogError::Corrupt("导入最终布局绑定与原 R 不同"));
        }
        Ok(binding)
    }
}

fn source_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Cache(error.to_string())
}

fn file_at_uid(captured: &CapturedIndexSource, uid: u64) -> Result<(String, FileNode)> {
    match captured.entry_by_uid(uid).map_err(source_error)? {
        Some(IndexEntry::File { path, node }) if node.meta.file_uid == uid => Ok((path, node)),
        _ => Err(CatalogError::Corrupt("导入真实来源 fileuid 不是唯一文件")),
    }
}

fn capture_body(
    captured: &CapturedIndexSource,
    label: &LtfsLabel,
    row: &StagedImportEntry,
    source_ordinal: usize,
    binding: &ImportSourceBinding,
) -> Result<CapturedImportEntry> {
    let selected = row
        .path
        .sources
        .get(source_ordinal)
        .ok_or(CatalogError::Incomplete)?;
    let uid = *row
        .source_file_uids
        .get(source_ordinal)
        .ok_or(CatalogError::Incomplete)?;
    let data = row
        .source_data
        .as_ref()
        .and_then(|sources| sources.get(source_ordinal))
        .ok_or(CatalogError::Incomplete)?;
    let identity = captured.identity().map_err(source_error)?;
    if label.volume_uuid != selected.volume_id
        || label.volume_uuid != binding.placement.volume_id
        || index_evidence::expected_index(&identity) != binding.source
        || binding.placement.pool_id != row.path.pool_id
        || binding.placement.barcode != selected.barcode
        || binding.scan.generation != selected.generation
        || selected.candidate.logical_path != row.path.logical_path
        || !staged_import::source_data_matches(&selected.candidate, uid, data, true)
    {
        return Err(CatalogError::Corrupt("导入实际来源与原候选、卷或构造不同"));
    }
    let (logical_path, logical) = file_at_uid(captured, uid)?;
    let attributes = NodeAttributes::from_file(&logical)?;
    let candidate = import_spool::import_candidate(
        &logical_path,
        NodeKind::File,
        Some(logical.length),
        &attributes,
    )
    .map_err(source_error)?;
    if logical_path != selected.candidate.physical_path
        || candidate.as_ref() != Some(&selected.candidate)
        || (source_ordinal == row.path.winner && attributes != row.current_attributes)
    {
        return Err(CatalogError::Corrupt("导入候选与真实逻辑节点不同"));
    }
    let StagedSourceData::Data {
        file_uid,
        length,
        content_digest,
        ee_alias,
        data_path,
    } = data
    else {
        return match data {
            StagedSourceData::NativeSymlink
                if logical.symlink.is_some()
                    && attributes.link_kind()? == super::super::link_kind::LinkKind::Native =>
            {
                Ok(CapturedImportEntry::MetadataOnly)
            }
            _ => Err(CatalogError::Corrupt("导入元数据节点与真实来源类型不同")),
        };
    };
    let (physical_path, body) = if *ee_alias {
        if attributes.link_kind()? != super::super::link_kind::LinkKind::EeAlias {
            return Err(CatalogError::Corrupt("导入 EE 正文没有原别名证明"));
        }
        let target = import_spool::resolve_ee_alias_target(
            &logical_path,
            logical.symlink.as_deref().ok_or(CatalogError::Incomplete)?,
        )
        .map_err(source_error)?;
        if data_path.as_deref() != Some(target.as_str()) {
            return Err(CatalogError::Corrupt("导入 EE 正文路径与原别名不同"));
        }
        // Drop the alias before decoding the body. Only one body extent array
        // survives into the producer; a native symlink is never empty Data.
        drop(logical);
        let (actual_path, actual) = file_at_uid(captured, *file_uid)?;
        if actual_path != target {
            return Err(CatalogError::Corrupt("导入 EE 正文 UID 与路径不同"));
        }
        (actual_path, actual)
    } else {
        if logical.symlink.is_some() || *file_uid != uid || data_path.is_some() {
            return Err(CatalogError::Corrupt("导入普通正文与原逻辑 UID 不同"));
        }
        (logical_path, logical)
    };
    if body.symlink.is_some()
        || body.meta.file_uid != *file_uid
        || body.length != *length
        || ContentDigest::from_ltfs_xattrs(&body.xattrs)? != *content_digest
    {
        return Err(CatalogError::Corrupt("导入真实正文 UID、长度或摘要不同"));
    }
    let layout = BodyLayout::new(
        label.volume_uuid,
        body.meta.file_uid,
        body.length,
        label.blocksize,
        [label.index_partition, label.data_partition],
        body.extents,
    )?;
    Ok(CapturedImportEntry::Data(CapturedImportBody {
        source: identity,
        scan: binding.scan.id,
        logical_uid: uid,
        media_version: selected.candidate.version,
        ee_alias: *ee_alias,
        physical_path: format!("/{physical_path}"),
        content_digest: *content_digest,
        layout,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
    use crate::ltfs::volume::read_label_only;
    use crate::scsi::sim::{SimCartridge, SimLibrary};
    use crate::tape::commands::TapeDrive;

    // This proves extraction against actual committed media, not the future
    // actor's Permission/PR or final FDB publication contract.
    #[test]
    fn actual_committed_import_body_checks_rows_without_trusting_dto() {
        let root = std::env::temp_dir().join(format!("import-body-core-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let library = SimLibrary::new(1, 2, 1);
        let barcode = "LAYOUTI1L8";
        library
            .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
            .unwrap();
        library.load_into_drive(barcode, 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                volume_id: "IMPORT".into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
        let bytes = (0..131_089usize)
            .map(|at| (at % 251) as u8)
            .collect::<Vec<_>>();
        {
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume
                .append_file("/nonempty.bin", &mut std::io::Cursor::new(&bytes))
                .unwrap();
            volume
                .append_file("/empty.bin", &mut std::io::Cursor::new(&[]))
                .unwrap();
            volume.commit().unwrap();
        }
        let label = read_label_only(&device).unwrap();
        let mut budget = RecoveryBudget::default();
        budget.index_scratch.directory = root.clone();
        budget.index_scratch.headroom_bytes = 0;
        let captured = capture_index_source(&TapeDrive::new(&device), &label, &budget).unwrap();
        let namespace = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let binding = ImportSourceBinding {
            placement: VolumePlacement {
                namespace,
                pool_id: pool,
                volume_id: label.volume_uuid,
                library_serial: "IMPORT-CORE".into(),
                barcode: barcode.into(),
            },
            scan: VolumeScan {
                generation: captured.construct().generation,
                id: Uuid::new_v4(),
            },
            source: index_evidence::expected_index(&captured.identity().unwrap()),
        };
        for path in ["nonempty.bin", "empty.bin"] {
            let IndexEntry::File { node, .. } = captured.entry_by_path(path).unwrap().unwrap()
            else {
                panic!("file");
            };
            let attributes = NodeAttributes::from_file(&node).unwrap();
            let candidate = import_spool::import_candidate(
                path,
                NodeKind::File,
                Some(node.length),
                &attributes,
            )
            .unwrap()
            .unwrap();
            let digest = ContentDigest::from_ltfs_xattrs(&node.xattrs).unwrap();
            let row = StagedImportEntry {
                path: ReconciledPath {
                    pool_id: pool,
                    logical_path: candidate.logical_path.clone(),
                    sources: vec![SourceCandidate {
                        barcode: barcode.into(),
                        volume_id: label.volume_uuid,
                        generation: binding.scan.generation,
                        candidate,
                    }],
                    winner: 0,
                },
                current_attributes: attributes,
                source_file_uids: vec![node.meta.file_uid],
                source_data: Some(vec![StagedSourceData::Data {
                    file_uid: node.meta.file_uid,
                    length: node.length,
                    content_digest: digest,
                    ee_alias: false,
                    data_path: None,
                }]),
            };
            let CapturedImportEntry::Data(actual) =
                capture_body(&captured, &label, &row, 0, &binding).unwrap()
            else {
                panic!("data");
            };
            assert_eq!(actual.physical_path, format!("/{path}"));
            assert_eq!(actual.source, captured.identity().unwrap());
            assert_eq!(actual.content_digest, digest);
            assert_eq!(actual.layout.length, node.length);
            assert_eq!(actual.layout.extents.as_ref(), node.extents);
            assert_eq!(actual.layout.extents.is_empty(), path == "empty.bin");
            assert_eq!(
                actual.layout.manifest().unwrap().extents as usize,
                node.extents.len()
            );
            let plan =
                import_materialize::plan_data_file(namespace, &row, std::slice::from_ref(&binding))
                    .unwrap()
                    .unwrap();
            let planned = &plan.copies[0];
            let raw = encode_physical_copy(&planned.copy).unwrap();
            let bound = actual
                .bind_exact_copy(namespace, plan.id, planned.id, &planned.copy, &raw)
                .unwrap();
            assert_eq!(bound.manifest, actual.layout.manifest().unwrap());
            assert_eq!(bound.path, actual.physical_path);
            assert!(bound.matches_raw_copy(Some(&raw)));
            let mut changed = planned.copy.clone();
            changed.size += 1;
            assert!(
                actual
                    .bind_exact_copy(
                        namespace,
                        plan.id,
                        planned.id,
                        &changed,
                        &encode_physical_copy(&changed).unwrap(),
                    )
                    .is_err()
            );
            assert!(
                actual
                    .bind_exact_copy(namespace, plan.id, planned.id, &changed, &raw)
                    .is_err()
            );
            changed = planned.copy.clone();
            changed
                .media
                .as_mut()
                .unwrap()
                .physical_path
                .push_str("-wrong");
            assert!(
                actual
                    .bind_exact_copy(
                        namespace,
                        plan.id,
                        planned.id,
                        &changed,
                        &encode_physical_copy(&changed).unwrap(),
                    )
                    .is_err()
            );
            let mut wrong = row.clone();
            wrong.source_file_uids[0] += 10_000;
            assert!(capture_body(&captured, &label, &wrong, 0, &binding).is_err());
            wrong = row.clone();
            wrong.path.sources[0]
                .candidate
                .physical_path
                .push_str("-wrong");
            assert!(capture_body(&captured, &label, &wrong, 0, &binding).is_err());
            wrong = row.clone();
            let StagedSourceData::Data { length, .. } = &mut wrong.source_data.as_mut().unwrap()[0]
            else {
                unreachable!();
            };
            *length += 1;
            assert!(capture_body(&captured, &label, &wrong, 0, &binding).is_err());
            wrong = row.clone();
            let StagedSourceData::Data { content_digest, .. } =
                &mut wrong.source_data.as_mut().unwrap()[0]
            else {
                unreachable!();
            };
            *content_digest = ContentDigest::Sha256([73; 32]);
            assert!(capture_body(&captured, &label, &wrong, 0, &binding).is_err());
            wrong = row.clone();
            wrong.source_data = Some(vec![StagedSourceData::NativeSymlink]);
            assert!(capture_body(&captured, &label, &wrong, 0, &binding).is_err());
            let mut wrong_source = binding.clone();
            wrong_source.source.sha256[0] ^= 1;
            assert!(capture_body(&captured, &label, &row, 0, &wrong_source).is_err());
            let mut wrong_label = label.clone();
            wrong_label.volume_uuid = Uuid::new_v4();
            assert!(capture_body(&captured, &wrong_label, &row, 0, &binding).is_err());
        }
        drop(captured);
        std::fs::remove_dir_all(root).unwrap();
    }
}
