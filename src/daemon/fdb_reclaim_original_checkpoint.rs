//! Cold recovery of an original group checkpoint keeps issuer receipts intact.
//! Catalog contexts authorize the original operation; only a real cold mount
//! supplies the current physical index view. This lane never appends or syncs.
use super::*;
use crate::ltfs::index::FileNode;

use crate::daemon::executor::ObserveReclaimGroupIo;
use crate::ltfs::recovery::RecoveryBudget;
use crate::managed_catalog::{OriginalCheckpointContext, OriginalCheckpointPublication};

pub(in crate::daemon) async fn resume_original_reclaim_checkpoint<'a>(
    io: &'a ObserveReclaimGroupIo<'_>,
    context: &OriginalCheckpointContext,
    budget: &RecoveryBudget,
) -> Result<ActiveMediaCommit<'a>> {
    context
        .require_io_scope(io.scope())
        .map_err(|e| catalog_error("冷恢复核对原Observe范围", e))?;
    io.require_checkpoint(
        context.batch_id(),
        context.previous_index(),
        context.current_index(),
        context.plan(),
    )?;
    require_original_epochs(
        context.owner(),
        io.key().round(),
        context.issuing_execution_epoch(),
        context.files(),
    )?;
    io.catalog()
        .check_original_checkpoint_context(context)
        .await
        .map_err(|e| catalog_error("冷恢复核对原持久检查点", e))?;
    io.verify()?;
    let mut volume = LtfsVolume::mount_with_budget(io.transport(), budget)?;
    volume.set_reservation_guard(Some(io.key()));
    require_original_current(&mut volume, context.current_index())?;
    let physical_paths = original_physical_paths(
        &mut volume,
        context.files(),
        context.plan().references().map(|r| (r.donor_path, r.donor)),
    )?;
    let expected_paths: HashMap<_, _> = context
        .physical_paths()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();
    if expected_paths != physical_paths {
        return Err(TapeError::Catalog("原回收检查点持久路径映射改变".into()));
    }
    io.catalog()
        .check_original_checkpoint_context(context)
        .await
        .map_err(|e| catalog_error("冷恢复结束核对原持久检查点", e))?;
    require_original_current(&mut volume, context.current_index())?;
    let completed_epoch = io
        .transport()
        .session_epoch()
        .ok_or_else(|| TapeError::NotReady("原回收冷恢复缺少可核验设备epoch".into()))?;
    io.verify()?;
    if io.transport().session_epoch() != Some(completed_epoch) {
        return Err(TapeError::NotReady(
            "原回收冷恢复最终核验期间设备epoch改变".into(),
        ));
    }
    Ok(ActiveMediaCommit {
        volume,
        batch_id: context.batch_id(),
        previous: context.previous_index(),
        current: context.current_index(),
        files: context.files().to_vec(),
        physical_paths,
    })
}

pub(in crate::daemon) async fn publish_original_reclaim_checkpoint<'a>(
    catalog: &ManagedCatalog,
    context: &OriginalCheckpointContext,
    mut committed: ActiveMediaCommit<'a>,
) -> Result<ActiveMediaCommit<'a>> {
    catalog
        .check_original_checkpoint_context(context)
        .await
        .map_err(|e| catalog_error("发布核对原持久检查点", e))?;
    require_original_commit(context, &committed)?;
    require_original_current(&mut committed.volume, context.current_index())?;
    let current = context.current_index();
    let serial = context.library_serial();
    let volume = current.volume_uuid;
    let observed = catalog
        .observed_active_volume_index(serial, volume)
        .await
        .map_err(|error| catalog_error("发布前核对索引", error))?;
    if observed != current {
        return Err(TapeError::Catalog(
            "FDB 活动发布时已观察索引与介质不一致".into(),
        ));
    }
    let pages = committed.files.chunks(128).count();

    if matches!(
        context.publication_phase(),
        OriginalCheckpointPublication::Compacted
    ) {
        return Ok(committed);
    }
    let publisher = catalog.original_checkpoint_publisher(context);
    if matches!(
        context.publication_phase(),
        OriginalCheckpointPublication::Sealed
    ) {
        {
            let source = CommittedLayoutSource::new(&mut committed.volume)
                .map_err(|error| catalog_error("核对已提交正文布局", error))?;
            let mut supported = true;
            for file in &committed.files {
                supported &= source
                    .projection_supported(layout_path(&committed.physical_paths, file)?)
                    .map_err(|error| catalog_error("正文布局尺寸预检", error))?;
            }
            if supported {
                expect_outcome(
                    "声明正文布局要求",
                    publisher
                        .require_layouts()
                        .await
                        .map_err(|error| catalog_error("声明正文布局要求", error))?,
                    &[
                        ActivePreparationOutcome::Prepared,
                        ActivePreparationOutcome::AlreadyPrepared,
                    ],
                )?;
                for file in &committed.files {
                    let layout = source
                        .prepared(layout_path(&committed.physical_paths, file)?, file)
                        .map_err(|error| catalog_error("提取已提交正文布局", error))?
                        .ok_or_else(|| TapeError::Catalog("正文布局支持范围在预检后改变".into()))?;
                    publisher
                        .stage_layout(&layout)
                        .await
                        .map_err(|error| catalog_error("持久化已提交正文布局", error))?;
                }
            }
        }

        for file in &committed.files {
            expect_outcome(
                "准备已观察文件",
                publisher
                    .prepare_file(file)
                    .await
                    .map_err(|error| catalog_error("准备已观察文件", error))?,
                &[
                    ActivePreparationOutcome::Prepared,
                    ActivePreparationOutcome::AlreadyPrepared,
                ],
            )?;
        }
        for page in 0..pages {
            expect_outcome(
                "准备批次页",
                publisher
                    .mark_prepared_page(page as u16)
                    .await
                    .map_err(|error| catalog_error("准备批次页", error))?,
                &[
                    ActivePreparationOutcome::Prepared,
                    ActivePreparationOutcome::AlreadyPrepared,
                ],
            )?;
        }
        expect_outcome(
            "封存准备批次",
            publisher
                .seal_preparation()
                .await
                .map_err(|error| catalog_error("封存准备批次", error))?,
            &[
                ActivePreparationOutcome::Prepared,
                ActivePreparationOutcome::AlreadyPrepared,
            ],
        )?;
    }
    if ActiveIndexIdentity::from(committed.volume.verified_committed_index_construct()?)
        != committed.current
    {
        return Err(TapeError::Catalog("FDB 活动发布前介质索引已改变".into()));
    }
    catalog
        .check_original_checkpoint_context(context)
        .await
        .map_err(|e| catalog_error("发布前核对原持久检查点", e))?;
    expect_outcome(
        "发布已观察批次",
        publisher
            .publish()
            .await
            .map_err(|error| catalog_error("发布已观察批次", error))?,
        &[
            ActivePublicationOutcome::Published,
            ActivePublicationOutcome::AlreadyPublished,
        ],
    )?;
    for (page, files) in committed.files.chunks(128).enumerate() {
        let page = page as u16;
        expect_outcome(
            "终结发布页",
            publisher
                .finalize_page(page)
                .await
                .map_err(|error| catalog_error("终结发布页", error))?,
            &[
                ActiveFinalizationOutcome::Finalized,
                ActiveFinalizationOutcome::AlreadyFinalized,
            ],
        )?;
        for file in files {
            expect_outcome(
                "归并已发布文件",
                publisher
                    .materialize_file(page, &file.receipt)
                    .await
                    .map_err(|error| catalog_error("归并已发布文件", error))?,
                &[
                    ActiveMaterializationOutcome::Materialized,
                    ActiveMaterializationOutcome::AlreadyMaterialized,
                ],
            )?;
        }
        expect_outcome(
            "完成归并页",
            publisher
                .finish_page(page)
                .await
                .map_err(|error| catalog_error("完成归并页", error))?,
            &[
                ActiveCompactionOutcome::Completed,
                ActiveCompactionOutcome::AlreadyCompleted,
            ],
        )?;
    }
    expect_outcome(
        "封存归并批次",
        publisher
            .seal_materialized()
            .await
            .map_err(|error| catalog_error("封存归并批次", error))?,
        &[
            ActiveCompactionOutcome::Completed,
            ActiveCompactionOutcome::AlreadyCompleted,
        ],
    )?;
    for page in 0..pages {
        expect_outcome(
            "清理归并页",
            publisher
                .cleanup_page(page as u16)
                .await
                .map_err(|error| catalog_error("清理归并页", error))?,
            &[
                ActiveCompactionOutcome::Completed,
                ActiveCompactionOutcome::AlreadyCompleted,
            ],
        )?;
    }
    catalog
        .check_original_checkpoint_context(context)
        .await
        .map_err(|e| catalog_error("发布结束核对原持久检查点", e))?;
    Ok(committed)
}

fn require_original_commit(
    context: &OriginalCheckpointContext,
    committed: &ActiveMediaCommit<'_>,
) -> Result<()> {
    let paths: HashMap<_, _> = context
        .physical_paths()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();
    if committed.batch_id != context.batch_id()
        || committed.previous != context.previous_index()
        || committed.current != context.current_index()
        || committed.physical_paths != paths
        || committed.files.len() != context.files().len()
        || committed
            .files
            .iter()
            .zip(context.files())
            .any(|(a, b)| a.receipt != b.receipt || a.attributes.digest() != b.attributes.digest())
    {
        return Err(TapeError::Catalog(
            "原回收发布不能替换检查点或原收据字节".into(),
        ));
    }
    require_original_epochs(
        context.owner(),
        context.owner(),
        context.issuing_execution_epoch(),
        &committed.files,
    )
}

fn require_original_epochs(
    live_owner: u64,
    key_round: u64,
    issuer: u64,
    files: &[PreparedActiveFile],
) -> Result<()> {
    if live_owner == 0
        || live_owner != key_round
        || issuer == 0
        || files.is_empty()
        || files.iter().any(|f| f.receipt.owner_epoch != issuer)
    {
        return Err(TapeError::Catalog(
            "原回收检查点当前执行权或原issuer收据改变".into(),
        ));
    }
    Ok(())
}

fn require_original_current(
    volume: &mut LtfsVolume<'_>,
    expected: ActiveIndexIdentity,
) -> Result<()> {
    let actual = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    if actual != expected || actual.incremental {
        return Err(TapeError::Catalog(
            "原回收检查点冷挂载的完整物理索引身份改变".into(),
        ));
    }
    Ok(())
}

/// The caller supplies only donors from the opaque original Actual plan.
/// Receipt/attribute bytes are checked against the real mounted node and are
/// never rewritten to the current executor's epoch.
fn original_physical_paths<'p>(
    volume: &mut LtfsVolume<'_>,
    files: &[PreparedActiveFile],
    donors: impl IntoIterator<Item = (&'p str, &'p FileNode)>,
) -> Result<HashMap<Uuid, String>> {
    let mut by_uid = HashMap::new();
    let mut donor_paths = HashSet::new();
    for (path, donor) in donors {
        if !donor_paths.insert(path)
            || by_uid.insert(donor.meta.file_uid, (path, donor)).is_some()
            || volume.index().find_file(path) != Some(donor)
        {
            return Err(TapeError::Catalog(
                "原回收检查点donor路径、UID或完整介质节点改变".into(),
            ));
        }
    }
    if files.is_empty() || files.len() != by_uid.len() {
        return Err(TapeError::Catalog("原回收检查点完整成员集合不符".into()));
    }
    let source = CommittedLayoutSource::new(volume)
        .map_err(|e| catalog_error("冷恢复核对原正文布局来源", e))?;
    let mut paths = HashMap::with_capacity(files.len());
    for file in files {
        let (path, _) = by_uid
            .remove(&file.receipt.ltfs_file_uid)
            .ok_or_else(|| TapeError::Catalog("原回收检查点收据缺少独一Actual donor".into()))?;
        source
            .prepared(path, file)
            .map_err(|e| catalog_error("冷恢复核对原收据及属性", e))?
            .ok_or_else(|| TapeError::NotReady("原回收检查点正文布局超出受支持边界".into()))?;
        if paths.insert(file.receipt.file_id, path.into()).is_some() {
            return Err(TapeError::Catalog("原回收检查点文件身份重复".into()));
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::managed_catalog::ContentDigest;
    use crate::scsi::reservation::fence;
    use crate::scsi::sim::{SimCartridge, SimLibrary, SimTransport};

    const BARCODE: &str = "ORIG01L8";
    fn original_checkpoint() -> (
        SimLibrary,
        SimTransport,
        ActiveIndexIdentity,
        PreparedActiveFile,
        FileNode,
    ) {
        let library = SimLibrary::new(1, 2, 1);
        library
            .insert_cartridge(SimCartridge::blank(BARCODE, 64 << 20), 0)
            .unwrap();
        library.load_into_drive(BARCODE, 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                block_size: 4096,
                ..Default::default()
            },
        )
        .unwrap();
        let key = ReservationKey::new(1, 7);
        fence(&device, key).unwrap();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(key));
        let body = b"the original committed bytes";
        let intent = ActiveFileIntent {
            namespace: Uuid::new_v4(),
            parent: Uuid::new_v4(),
            parent_path: b"/".to_vec(),
            name: b"one".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: Some(1),
            operation_id: Uuid::new_v4(),
            request_epoch: 3,
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            barcode_constraint: Some(BARCODE.into()),
            owner_epoch: 7,
            volume_id: volume.label().volume_uuid,
            volume_generation: volume.index().generation + 1,
            volume_scan_id: Uuid::new_v4(),
            expected_size: body.len() as u64,
            expected_digest: ContentDigest::Sha256(Sha256::digest(body).into()),
        };
        volume.append_bytes("one", body).unwrap();
        let prepared = prepare_active_file(&volume, "/one", &intent).unwrap();
        volume.sync().unwrap();
        let current = volume.verified_committed_index_construct().unwrap().into();
        let donor = volume.index().find_file("one").unwrap().clone();
        drop(volume);
        library.power_cut();
        (library, device, current, prepared, donor)
    }

    #[test]
    fn cold_actual_full_head_preserves_issuer_receipt_under_new_fence_without_writing() {
        let (library, device, current, file, donor) = original_checkpoint();
        let old_receipt = file.receipt.clone();
        let old_attributes = file.attributes.digest();
        let live_key = ReservationKey::new(1, 8);
        require_original_epochs(8, live_key.round(), 7, std::slice::from_ref(&file)).unwrap();
        fence(&device, live_key).unwrap();
        let before = library.cartridge(BARCODE).unwrap();
        let start = device.command_log().len();
        let mut volume = LtfsVolume::mount_with_budget(
            &device,
            &crate::ltfs::recovery::RecoveryBudget::default(),
        )
        .unwrap();
        volume.set_reservation_guard(Some(live_key));
        require_original_current(&mut volume, current).unwrap();
        let paths =
            original_physical_paths(&mut volume, std::slice::from_ref(&file), [("/one", &donor)])
                .unwrap();
        assert_eq!(paths[&file.receipt.file_id], "/one");
        assert_eq!(file.receipt, old_receipt);
        assert_eq!(file.receipt.owner_epoch, 7);
        assert_eq!(file.attributes.digest(), old_attributes);
        let after = library.cartridge(BARCODE).unwrap();
        assert_eq!(before.mam, after.mam);
        for (left, right) in before.partitions.iter().zip(after.partitions.iter()) {
            assert_eq!(left.objects, right.objects);
        }
        assert!(
            !device.command_log()[start..]
                .iter()
                .any(|op| matches!(*op, 0x0a | 0x10 | 0x8d))
        );
    }

    #[test]
    fn live_authority_and_original_issuer_are_separate_and_both_required() {
        let (_library, _device, _current, file, _donor) = original_checkpoint();
        let files = std::slice::from_ref(&file);
        assert!(require_original_epochs(8, 8, 7, files).is_ok());
        assert!(require_original_epochs(7, 7, 7, files).is_ok());
        assert!(require_original_epochs(8, 7, 7, files).is_err());
        assert!(require_original_epochs(0, 0, 7, files).is_err());
        assert!(require_original_epochs(8, 8, 0, files).is_err());
        assert!(require_original_epochs(8, 8, 7, &[]).is_err());
        let mut rebound = file.clone();
        rebound.receipt.owner_epoch = 8;
        assert!(require_original_epochs(8, 8, 7, &[rebound]).is_err());
    }

    #[test]
    fn cold_head_is_read_from_medium_and_rejects_foreign_context_identity() {
        let (_library, device, current, _file, _donor) = original_checkpoint();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
        for field in 0..6 {
            let mut foreign = current;
            match field {
                0 => foreign.volume_uuid = Uuid::new_v4(),
                1 => foreign.generation += 1,
                2 => foreign.location.start_block += 1,
                3 => foreign.sha256[0] ^= 1,
                4 => foreign.incremental = true,
                _ => foreign.location.partition = 'a',
            }
            assert!(require_original_current(&mut volume, foreign).is_err());
        }
        require_original_current(&mut volume, current).unwrap();
    }

    #[test]
    fn original_donor_and_receipt_must_match_actual_nodes_and_exact_membership() {
        let (_library, device, _current, file, donor) = original_checkpoint();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(1, 7)));
        let files = std::slice::from_ref(&file);
        let mut foreign = donor.clone();
        foreign.extents[0].start_block += 1;
        assert!(original_physical_paths(&mut volume, files, [("/one", &foreign)]).is_err());
        assert!(original_physical_paths(&mut volume, files, [("/else", &donor)]).is_err());
        assert!(
            original_physical_paths(&mut volume, files, [("/one", &donor), ("/one", &donor)])
                .is_err()
        );
        assert!(original_physical_paths(&mut volume, &[], [("/one", &donor)]).is_err());
        let mut wrong_receipt = file.clone();
        wrong_receipt.receipt.size += 1;
        assert!(
            original_physical_paths(&mut volume, &[wrong_receipt], [("/one", &donor)]).is_err()
        );
        let mut wrong_metadata = file.clone();
        wrong_metadata.receipt.metadata_digest = Some([0; 32]);
        assert!(
            original_physical_paths(&mut volume, &[wrong_metadata], [("/one", &donor)]).is_err()
        );
        assert!(original_physical_paths(&mut volume, files, [("/one", &donor)]).is_ok());
    }
}
