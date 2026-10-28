//! Owner-fenced LTFS S4 path for an Active FDB file batch.
//!
//! This consumes the mounted volume on every error. In particular, a failed
//! FDB operation after appending data must never leave a dirty volume in a
//! caller that might later call `sync_batch` or `unmount`. The next owner must
//! inspect the medium and FDB reservation/batch records before proceeding.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::daemon::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::index::{DirectoryNode, IndexEntry, LtfsIndex};
use crate::ltfs::recovery::TailKind;
use crate::ltfs::volume::{
    CommittedIndexConstruct, LtfsVolume, TailPolicy, XATTR_EMPTY_RECOVERY, empty_recovery_marker,
};
use crate::managed_catalog::{
    ActiveBatchOutcome, ActiveBatchPhase, ActiveCompactionOutcome, ActiveFileIntent,
    ActiveFinalizationOutcome, ActiveIndexIdentity, ActiveIndexOutcome,
    ActiveMaterializationOutcome, ActiveOwnerOutcome, ActivePreparationOutcome,
    ActivePublicationOutcome, ActiveRecoveryOutcome, CommitReceipt, ManagedCatalog,
    PreparedActiveFile, PublishOutcome, prepare_active_file, prepare_committed_active_file,
};
use crate::scsi::reservation::ReservationKey;

pub struct ActiveMediaUpload {
    pub intent: ActiveFileIntent,
    pub spool: PathBuf,
}

pub struct ActiveMediaRecovered<'a> {
    volume: LtfsVolume<'a>,
    index: ActiveIndexIdentity,
}

impl<'a> ActiveMediaRecovered<'a> {
    pub fn volume(&self) -> &LtfsVolume<'a> {
        &self.volume
    }

    pub fn index(&self) -> ActiveIndexIdentity {
        self.index
    }
}

/// Reconcile an interrupted, unsealed write after the new owner fenced the
/// drive and mounted the same medium with a PR guard. A clean unchanged mount
/// cancels Q/I and clears the gate; a persisted T1/T2 tail is discarded into
/// a no-file S4 generation. A failed close-tail is indeterminate and leaves
/// the FDB gate durable for the next owner to retry.
pub async fn recover_active_media_tail<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    serial: &str,
    owner_epoch: u64,
) -> Result<ActiveMediaRecovered<'a>> {
    let mounted = ActiveIndexIdentity::from(volume.verified_recovery_index_construct()?);
    let observed = catalog
        .observed_active_volume_index(serial, mounted.volume_uuid)
        .await
        .map_err(|error| catalog_error("恢复前核对 FDB 索引", error))?;
    let tail = volume.recovery().dp.tail;
    let already_closed = Some(mounted.generation) == observed.generation.checked_add(1)
        && tail == TailKind::Complete
        && volume.root_xattr(XATTR_EMPTY_RECOVERY)
            == Some(
                empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
                    volume_uuid: observed.volume_uuid,
                    generation: observed.generation,
                    location: observed.location,
                    sha256: observed.sha256,
                    incremental: observed.incremental,
                })
                .as_str(),
            );
    if !(mounted == observed
        && matches!(
            tail,
            TailKind::Complete | TailKind::UnindexedData | TailKind::TruncatedIndex
        ))
        && !already_closed
    {
        return Err(TapeError::Catalog(
            "FDB 恢复时介质与已观察索引不符或存在未知 S4".into(),
        ));
    }
    expect_outcome(
        "冻结卷预约",
        catalog
            .begin_active_volume_recovery(serial, owner_epoch, observed)
            .await
            .map_err(|error| catalog_error("冻结卷预约", error))?,
        &[
            ActiveRecoveryOutcome::Begun,
            ActiveRecoveryOutcome::AlreadyBegun,
        ],
    )?;
    let recovered = if already_closed {
        Some(mounted)
    } else if matches!(tail, TailKind::UnindexedData | TailKind::TruncatedIndex) {
        let report = volume.close_tail(TailPolicy::Discard)?;
        if report.salvaged.is_some() {
            return Err(TapeError::Catalog("空收尾意外恢复出文件".into()));
        }
        let current = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
        if Some(current.generation) != observed.generation.checked_add(1)
            || volume.root_xattr(XATTR_EMPTY_RECOVERY)
                != Some(
                    empty_recovery_marker(crate::ltfs::volume::CommittedIndexConstruct {
                        volume_uuid: observed.volume_uuid,
                        generation: observed.generation,
                        location: observed.location,
                        sha256: observed.sha256,
                        incremental: observed.incremental,
                    })
                    .as_str(),
                )
        {
            return Err(TapeError::Catalog("空收尾介质证明不一致".into()));
        }
        Some(current)
    } else {
        None
    };

    if let Some(batch) = catalog
        .active_batch_status(serial, observed.volume_uuid, observed.generation + 1)
        .await
        .map_err(|error| catalog_error("核对未封存批次", error))?
    {
        if batch.completed || batch.phase == ActiveBatchPhase::Sealed {
            return Err(TapeError::Catalog("收尾遇到已封存批次".into()));
        }
        expect_outcome(
            "中止未封存批次",
            catalog
                .abort_open_active_batch(
                    serial,
                    observed.volume_uuid,
                    observed.generation + 1,
                    batch.batch_id,
                    owner_epoch,
                )
                .await
                .map_err(|error| catalog_error("中止未封存批次", error))?,
            &[
                ActiveBatchOutcome::Aborted,
                ActiveBatchOutcome::AlreadyAborted,
            ],
        )?;
    }

    loop {
        let page = catalog
            .pending_active_volume_reservations(
                serial,
                observed.volume_uuid,
                observed
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| TapeError::Catalog("卷代数溢出".into()))?,
                None,
                100,
            )
            .await
            .map_err(|error| catalog_error("列举卷预约", error))?;
        if page.items.is_empty() {
            break;
        }
        for mut intent in page.items {
            intent.owner_epoch = owner_epoch;
            expect_outcome(
                "取消未封存预约",
                catalog
                    .cancel_recovering_active_file(&intent)
                    .await
                    .map_err(|error| catalog_error("取消未封存预约", error))?,
                &[PublishOutcome::RequestCancelled],
            )?;
        }
    }
    loop {
        let page = catalog
            .pending_active_volume_upgrades(
                serial,
                observed.volume_uuid,
                observed
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| TapeError::Catalog("卷代数溢出".into()))?,
                None,
                100,
            )
            .await
            .map_err(|error| catalog_error("列举升级收据", error))?;
        if page.items.is_empty() {
            break;
        }
        for mut intent in page.items {
            intent.owner_epoch = owner_epoch;
            expect_outcome(
                "取消未封存升级收据",
                catalog
                    .cancel_recovering_active_file(&intent)
                    .await
                    .map_err(|error| catalog_error("取消未封存升级收据", error))?,
                &[PublishOutcome::RequestCancelled],
            )?;
        }
    }
    if ActiveIndexIdentity::from(volume.verified_committed_index_construct()?)
        != recovered.unwrap_or(observed)
    {
        return Err(TapeError::Catalog(
            "空收尾登记前介质索引或设备预留已改变".into(),
        ));
    }
    expect_outcome(
        "完成空收尾",
        catalog
            .finish_active_volume_recovery(serial, owner_epoch, observed, recovered)
            .await
            .map_err(|error| catalog_error("完成空收尾", error))?,
        if recovered.is_some() {
            &[
                ActiveRecoveryOutcome::Recorded,
                ActiveRecoveryOutcome::AlreadyRecorded,
            ]
        } else {
            &[ActiveRecoveryOutcome::Cleared]
        },
    )?;
    Ok(ActiveMediaRecovered {
        volume,
        index: recovered.unwrap_or(observed),
    })
}

/// The takeover decision for one PR-fenced mounted volume. Select the
/// recovery path from the physical DP generation and FDB observation, never
/// from an in-memory task status. Every branch consumes the volume; failure
/// leaves no caller with a mutable session that could append or checkpoint.
pub async fn reconcile_active_media_on_takeover<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    serial: &str,
    owner_epoch: u64,
) -> Result<ActiveMediaRecovered<'a>> {
    let current = ActiveIndexIdentity::from(volume.verified_recovery_index_construct()?);
    let observed = catalog
        .observed_active_volume_index(serial, current.volume_uuid)
        .await
        .map_err(|error| catalog_error("接任核对已观察索引", error))?;
    let status = catalog
        .active_batch_status(serial, current.volume_uuid, current.generation)
        .await
        .map_err(|error| catalog_error("接任核对物理批次", error))?;
    if current == observed {
        if let Some(batch) = status {
            if batch.completed {
                return recover_active_media_tail(catalog, volume, serial, owner_epoch).await;
            }
            if batch.phase == ActiveBatchPhase::Sealed {
                if volume.recovery().dp.tail != TailKind::Complete {
                    return Err(TapeError::Catalog("已观察批次的介质尾部不完整".into()));
                }
                return resume_observed_active_media_batch(catalog, volume, serial, owner_epoch)
                    .await;
            }
            return Err(TapeError::Catalog("已观察代数存在未封存批次".into()));
        }
        return recover_active_media_tail(catalog, volume, serial, owner_epoch).await;
    }
    if observed.generation.checked_add(1) != Some(current.generation) {
        return Err(TapeError::Catalog("接任介质代数与 FDB 观察不连续".into()));
    }
    if let Some(batch) = status
        && batch.phase == ActiveBatchPhase::Sealed
    {
        if volume.recovery().dp.tail != TailKind::Complete {
            return Err(TapeError::Catalog("未观察封存批次的介质尾部不完整".into()));
        }
        let committed =
            recover_sealed_active_media_batch(catalog, volume, serial, owner_epoch).await?;
        let committed = publish_active_media_batch(catalog, committed).await?;
        return Ok(ActiveMediaRecovered {
            index: committed.current,
            volume: committed.volume,
        });
    }
    recover_active_media_tail(catalog, volume, serial, owner_epoch).await
}

/// Enter FDB's Active authority only after the Raft execution permit and the
/// same mounted drive's PR holder have both been verified. A failed recovery
/// leaves the advanced epoch as a fence; the same round may safely retry.
pub async fn establish_active_media_owner<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    serial: &str,
    key: ReservationKey,
    permission: &Permission,
) -> Result<ActiveMediaRecovered<'a>> {
    let round = key.round();
    if !key.is_ours()
        || round == 0
        || !(permission.allows(round, Phase::Execution) || permission.allows(round, Phase::Serving))
    {
        return Err(TapeError::NotReady(
            "FDB 接任缺少当前轮次的多数派执行许可".into(),
        ));
    }
    if volume.guarded_execution() != Some((round, permission as *const Permission as usize)) {
        return Err(TapeError::NotReady(
            "FDB 接任卷未使用当前轮次的多数派许可设备守护".into(),
        ));
    }
    volume.set_reservation_guard(Some(key));
    volume.verified_recovery_index_construct()?;
    match catalog
        .claim_active_owner_epoch(serial, round)
        .await
        .map_err(|error| catalog_error("接任推进 FDB owner", error))?
    {
        ActiveOwnerOutcome::Advanced | ActiveOwnerOutcome::AlreadyCurrent => {}
        ActiveOwnerOutcome::Stale => {
            return Err(TapeError::Catalog("FDB owner 已由更新轮次持有".into()));
        }
    }
    if !(permission.allows(round, Phase::Execution) || permission.allows(round, Phase::Serving)) {
        return Err(TapeError::NotReady(
            "FDB 接任期间多数派执行许可已失效".into(),
        ));
    }
    volume.verified_recovery_index_construct()?;
    reconcile_active_media_on_takeover(catalog, volume, serial, round).await
}

/// Rebuild a sealed but unobserved S4 batch from the mounted committed index.
/// Every manifest receipt is checked against its physical LTFS file before
/// the index CAS; a crash after that CAS is safe to retry with the next owner.
pub async fn recover_sealed_active_media_batch<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    serial: &str,
    owner_epoch: u64,
) -> Result<ActiveMediaCommit<'a>> {
    let current = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    let observed = catalog
        .observed_active_volume_index(serial, current.volume_uuid)
        .await
        .map_err(|error| catalog_error("封存恢复核对索引", error))?;
    let previous = if observed == current {
        catalog
            .observed_active_volume_generation(
                serial,
                current.volume_uuid,
                current
                    .generation
                    .checked_sub(1)
                    .ok_or_else(|| TapeError::Catalog("封存恢复代数无效".into()))?,
            )
            .await
            .map_err(|error| catalog_error("封存恢复查前代索引", error))?
            .ok_or_else(|| TapeError::Catalog("封存恢复前代索引缺失".into()))?
    } else if observed.generation.checked_add(1) == Some(current.generation) {
        observed
    } else {
        return Err(TapeError::Catalog(
            "封存恢复介质与 FDB 索引代数不连续".into(),
        ));
    };
    let batch = catalog
        .active_batch_status(serial, current.volume_uuid, current.generation)
        .await
        .map_err(|error| catalog_error("封存恢复读取批次", error))?
        .ok_or_else(|| TapeError::Catalog("介质新代数没有封存批次".into()))?;
    if batch.completed || batch.phase != ActiveBatchPhase::Sealed {
        return Err(TapeError::Catalog("介质新代数批次尚未封存".into()));
    }
    let mut files = Vec::with_capacity(batch.items as usize);
    let mut expected_delta = Vec::with_capacity(batch.items as usize);
    for page in 0..batch.pages {
        let sealed = catalog
            .sealed_active_batch_page(
                serial,
                owner_epoch,
                current.volume_uuid,
                current.generation,
                batch.batch_id,
                page,
            )
            .await
            .map_err(|error| catalog_error("读取封存批次页", error))?;
        if sealed.next_page != (page + 1 < batch.pages).then_some(page + 1) {
            return Err(TapeError::Catalog("封存批次页游标不一致".into()));
        }
        for (mut receipt, mut intent) in sealed.files {
            intent.owner_epoch = owner_epoch;
            let path = intent
                .ltfs_path()
                .map_err(|error| catalog_error("封存恢复物理路径", error))?;
            let prepared = prepare_committed_active_file(&volume, &path, &intent)
                .map_err(|error| catalog_error("核对已提交文件", error))?;
            receipt.owner_epoch = owner_epoch;
            if receipt != prepared.receipt {
                return Err(TapeError::Catalog(
                    "封存批次收据与已提交介质文件不一致".into(),
                ));
            }
            expected_delta.push((path, prepared.receipt.ltfs_file_uid));
            files.push(prepared);
        }
    }
    if files.len() != batch.items as usize
        || ActiveIndexIdentity::from(volume.verified_committed_index_construct()?) != current
    {
        return Err(TapeError::Catalog("封存批次数量或介质索引已改变".into()));
    }
    if current.incremental {
        let raw_index = volume.verified_committed_raw_dp_index()?;
        audit_sealed_incremental_delta(&raw_index, current, &expected_delta)?;
    } else if !previous.incremental {
        let mut audit = SealedFullAudit::new(&expected_delta)?;
        let prior = volume.scan_verified_full_dp_index(committed_construct(previous), |entry| {
            audit.record_previous(entry)
        })?;
        let latest = volume.scan_verified_full_dp_index(committed_construct(current), |entry| {
            audit.check_current(entry)
        })?;
        if prior.generation.checked_add(1) != Some(latest.generation)
            || latest.previous_location != Some(prior.self_location)
        {
            return Err(TapeError::Catalog("封存 Full 索引与前代回指不一致".into()));
        }
        audit.finish()?;
    } else {
        let (prior, predecessors) =
            volume.verified_materialized_dp_index(committed_construct(previous))?;
        let mut next_generation = previous.generation;
        for predecessor in predecessors {
            if predecessor.generation.checked_add(1) != Some(next_generation) {
                return Err(TapeError::Catalog("前代增量链跳过了中间代数".into()));
            }
            next_generation = predecessor.generation;
            if catalog
                .observed_active_volume_generation(
                    serial,
                    predecessor.volume_uuid,
                    predecessor.generation,
                )
                .await
                .map_err(|error| catalog_error("核对前代增量链", error))?
                != Some(ActiveIndexIdentity::from(predecessor))
            {
                return Err(TapeError::Catalog("前代增量链缺少 FDB 逐代摘要证明".into()));
            }
        }
        let raw_index = volume.verified_committed_raw_dp_index()?;
        audit_sealed_full_difference(&prior, &raw_index, current, &expected_delta)?;
    }
    if ActiveIndexIdentity::from(volume.verified_committed_index_construct()?) != current {
        return Err(TapeError::Catalog("登记前介质索引或设备预留已改变".into()));
    }
    expect_outcome(
        "接任登记已提交索引",
        catalog
            .record_active_volume_index(
                serial,
                owner_epoch,
                current.volume_uuid,
                batch.batch_id,
                previous,
                current,
            )
            .await
            .map_err(|error| catalog_error("接任登记已提交索引", error))?,
        &[
            ActiveIndexOutcome::Recorded,
            ActiveIndexOutcome::AlreadyRecorded,
        ],
    )?;
    Ok(ActiveMediaCommit {
        volume,
        batch_id: batch.batch_id,
        previous,
        current,
        files,
    })
}

/// Resume an index already observed in FDB after a crash during preparation,
/// publication, request finalization or compaction. The volume is consumed so
/// no caller can accidentally checkpoint an unverified generation on error.
pub async fn resume_observed_active_media_batch<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    serial: &str,
    owner_epoch: u64,
) -> Result<ActiveMediaRecovered<'a>> {
    let current = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    let observed = catalog
        .observed_active_volume_index(serial, current.volume_uuid)
        .await
        .map_err(|error| catalog_error("恢复发布核对索引", error))?;
    if observed != current {
        return Err(TapeError::Catalog("恢复发布时介质与 FDB 索引不一致".into()));
    }
    let status = catalog
        .active_batch_status(serial, current.volume_uuid, current.generation)
        .await
        .map_err(|error| catalog_error("恢复发布读取批次", error))?
        .ok_or_else(|| TapeError::Catalog("恢复发布缺少批次".into()))?;
    if status.phase != ActiveBatchPhase::Sealed {
        return Err(TapeError::Catalog("恢复发布批次未封存".into()));
    }
    let mut pages = Vec::with_capacity(usize::from(status.pages));
    for page in 0..status.pages {
        let recovered = catalog
            .recover_observed_active_batch_page(
                serial,
                owner_epoch,
                current.volume_uuid,
                current.generation,
                status.batch_id,
                page,
            )
            .await
            .map_err(|error| catalog_error("恢复发布读取页", error))?
            .ok_or_else(|| TapeError::Catalog("恢复发布索引观察缺失".into()))?;
        if recovered.index != current
            || recovered.next_page != (page + 1 < status.pages).then_some(page + 1)
        {
            return Err(TapeError::Catalog("恢复发布页索引或游标不一致".into()));
        }
        let mut files = Vec::with_capacity(recovered.receipts.len());
        for mut receipt in recovered.receipts {
            let mut intent = catalog
                .recover_upgraded_active_file_intent(&receipt, owner_epoch)
                .await
                .map_err(|error| catalog_error("恢复发布读取路径意图", error))?;
            intent.owner_epoch = owner_epoch;
            receipt.owner_epoch = owner_epoch;
            let path = intent
                .ltfs_path()
                .map_err(|error| catalog_error("恢复发布物理路径", error))?;
            let prepared = prepare_committed_active_file(&volume, &path, &intent)
                .map_err(|error| catalog_error("恢复发布核对介质文件", error))?;
            if prepared.receipt != receipt {
                return Err(TapeError::Catalog("恢复发布文件与介质收据不一致".into()));
            }
            files.push(prepared);
        }
        pages.push(files);
    }
    if ActiveIndexIdentity::from(volume.verified_committed_index_construct()?) != current {
        return Err(TapeError::Catalog("恢复发布前介质索引或预留已改变".into()));
    }
    if !status.completed {
        if !status.published {
            for (page, files) in pages.iter().enumerate() {
                for file in files {
                    expect_outcome(
                        "恢复准备文件",
                        catalog
                            .prepare_observed_active_file(
                                serial,
                                owner_epoch,
                                status.batch_id,
                                &file.receipt,
                                &file.attributes,
                            )
                            .await
                            .map_err(|error| catalog_error("恢复准备文件", error))?,
                        &[
                            ActivePreparationOutcome::Prepared,
                            ActivePreparationOutcome::AlreadyPrepared,
                        ],
                    )?;
                }
                expect_outcome(
                    "恢复准备页",
                    catalog
                        .mark_observed_active_batch_page_prepared(
                            serial,
                            owner_epoch,
                            current.volume_uuid,
                            current.generation,
                            status.batch_id,
                            page as u16,
                        )
                        .await
                        .map_err(|error| catalog_error("恢复准备页", error))?,
                    &[
                        ActivePreparationOutcome::Prepared,
                        ActivePreparationOutcome::AlreadyPrepared,
                    ],
                )?;
            }
            expect_outcome(
                "恢复封存准备",
                catalog
                    .seal_observed_active_preparation(
                        serial,
                        owner_epoch,
                        current.volume_uuid,
                        current.generation,
                        status.batch_id,
                    )
                    .await
                    .map_err(|error| catalog_error("恢复封存准备", error))?,
                &[
                    ActivePreparationOutcome::Prepared,
                    ActivePreparationOutcome::AlreadyPrepared,
                ],
            )?;
            if ActiveIndexIdentity::from(volume.verified_committed_index_construct()?) != current {
                return Err(TapeError::Catalog("恢复发布前介质索引已改变".into()));
            }
            expect_outcome(
                "恢复发布批次",
                catalog
                    .publish_observed_active_batch(
                        serial,
                        owner_epoch,
                        current.volume_uuid,
                        current.generation,
                        status.batch_id,
                    )
                    .await
                    .map_err(|error| catalog_error("恢复发布批次", error))?,
                &[
                    ActivePublicationOutcome::Published,
                    ActivePublicationOutcome::AlreadyPublished,
                ],
            )?;
        }
        for (page, files) in pages.iter().enumerate() {
            let page = page as u16;
            expect_outcome(
                "恢复终结页",
                catalog
                    .finalize_published_active_batch_page(
                        serial,
                        owner_epoch,
                        current.volume_uuid,
                        current.generation,
                        status.batch_id,
                        page,
                    )
                    .await
                    .map_err(|error| catalog_error("恢复终结页", error))?,
                &[
                    ActiveFinalizationOutcome::Finalized,
                    ActiveFinalizationOutcome::AlreadyFinalized,
                ],
            )?;
            for file in files {
                expect_outcome(
                    "恢复归并文件",
                    catalog
                        .materialize_published_active_file(
                            serial,
                            owner_epoch,
                            status.batch_id,
                            page,
                            &file.receipt,
                        )
                        .await
                        .map_err(|error| catalog_error("恢复归并文件", error))?,
                    &[
                        ActiveMaterializationOutcome::Materialized,
                        ActiveMaterializationOutcome::AlreadyMaterialized,
                    ],
                )?;
            }
            expect_outcome(
                "恢复完成归并页",
                catalog
                    .finish_materialized_active_batch_page(
                        serial,
                        owner_epoch,
                        current.volume_uuid,
                        current.generation,
                        status.batch_id,
                        page,
                    )
                    .await
                    .map_err(|error| catalog_error("恢复完成归并页", error))?,
                &[
                    ActiveCompactionOutcome::Completed,
                    ActiveCompactionOutcome::AlreadyCompleted,
                ],
            )?;
        }
        expect_outcome(
            "恢复封存归并批次",
            catalog
                .seal_materialized_active_batch(
                    serial,
                    owner_epoch,
                    current.volume_uuid,
                    current.generation,
                    status.batch_id,
                )
                .await
                .map_err(|error| catalog_error("恢复封存归并批次", error))?,
            &[
                ActiveCompactionOutcome::Completed,
                ActiveCompactionOutcome::AlreadyCompleted,
            ],
        )?;
    }
    for page in 0..status.pages {
        expect_outcome(
            "恢复清理归并页",
            catalog
                .cleanup_materialized_active_batch_page(
                    serial,
                    owner_epoch,
                    current.volume_uuid,
                    current.generation,
                    status.batch_id,
                    page,
                )
                .await
                .map_err(|error| catalog_error("恢复清理归并页", error))?,
            &[
                ActiveCompactionOutcome::Completed,
                ActiveCompactionOutcome::AlreadyCompleted,
            ],
        )?;
    }
    Ok(ActiveMediaRecovered {
        volume,
        index: current,
    })
}

fn audit_sealed_incremental_delta(
    delta: &LtfsIndex,
    current: ActiveIndexIdentity,
    files: &[(String, u64)],
) -> Result<()> {
    if !current.incremental
        || !delta.incremental
        || delta.generation != current.generation
        || delta.self_location != current.location
    {
        return Err(TapeError::Catalog("封存增量索引身份不一致".into()));
    }
    audit_sealed_delta(delta, files)
}

fn committed_construct(index: ActiveIndexIdentity) -> CommittedIndexConstruct {
    CommittedIndexConstruct {
        volume_uuid: index.volume_uuid,
        generation: index.generation,
        location: index.location,
        sha256: index.sha256,
        incremental: index.incremental,
    }
}

/// A disposable on-disk view for comparing two verified Full constructs. The
/// largest in-memory object is one streamed LTFS node, plus the sealed batch.
struct SealedFullAudit {
    path: PathBuf,
    db: Connection,
    expected: HashMap<String, u64>,
    ancestors: HashSet<String>,
    previous_count: u64,
    current_count: u64,
}

impl SealedFullAudit {
    fn new(files: &[(String, u64)]) -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("tape-rs-sealed-full-{}.sqlite", Uuid::new_v4()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let db = match Connection::open(&path) {
            Ok(db) => db,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error.into());
            }
        };
        let mut audit = Self {
            path,
            db,
            expected: HashMap::with_capacity(files.len()),
            ancestors: HashSet::new(),
            previous_count: 0,
            current_count: 0,
        };
        for (path, uid) in files {
            let relative = path.trim_start_matches('/');
            if audit.expected.insert(relative.to_owned(), *uid).is_some() {
                return Err(TapeError::Catalog("封存批次物理路径重复".into()));
            }
            let mut parent = String::new();
            for component in relative.split('/').take(relative.matches('/').count()) {
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(component);
                audit.ancestors.insert(parent.clone());
            }
        }
        audit.db.execute_batch(
            "PRAGMA journal_mode=OFF;
             PRAGMA synchronous=OFF;
             PRAGMA temp_store=FILE;
             PRAGMA cache_size=-8192;
             CREATE TABLE previous(path TEXT PRIMARY KEY, kind INTEGER NOT NULL,
                                   uid TEXT NOT NULL, digest BLOB NOT NULL, seen INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE current(path TEXT PRIMARY KEY);
             BEGIN IMMEDIATE;",
        )?;
        Ok(audit)
    }

    fn record_previous(&mut self, entry: IndexEntry) -> Result<()> {
        let (path, kind, uid, digest) = node_identity(entry);
        self.db.execute(
            "INSERT INTO previous(path,kind,uid,digest) VALUES (?1,?2,?3,?4)",
            params![path, kind, uid.to_string(), &digest[..]],
        )?;
        self.previous_count += 1;
        if self.previous_count.is_multiple_of(1000) {
            self.db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
        Ok(())
    }

    fn check_current(&mut self, entry: IndexEntry) -> Result<()> {
        let (path, kind, uid, digest) = node_identity(entry);
        self.db
            .execute("INSERT INTO current(path) VALUES (?1)", [&path])?;
        let previous: Option<(i64, String, Vec<u8>, i64)> = self
            .db
            .query_row(
                "SELECT kind,uid,digest,seen FROM previous WHERE path=?1",
                [&path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let changed = if let Some((old_kind, old_uid, old_digest, seen)) = previous {
            if seen != 0 || old_kind != kind || (kind == 0 && old_uid != uid.to_string()) {
                return Err(TapeError::Catalog("封存批次与介质文件差异不一致".into()));
            }
            self.db
                .execute("UPDATE previous SET seen=1 WHERE path=?1", [&path])?;
            old_digest != digest
        } else {
            true
        };
        if changed
            && (if kind == 1 {
                self.expected.remove(&path) != Some(uid)
            } else {
                !path.is_empty() && !self.ancestors.contains(&path)
            })
        {
            return Err(TapeError::Catalog("封存批次与介质文件差异不一致".into()));
        }
        self.current_count += 1;
        if self.current_count.is_multiple_of(1000) {
            self.db.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        let missing: i64 = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM previous WHERE seen=0)",
            [],
            |row| row.get(0),
        )?;
        if missing != 0 || !self.expected.is_empty() {
            return Err(TapeError::Catalog("封存批次与介质文件差异不一致".into()));
        }
        self.db.execute_batch("COMMIT;")?;
        Ok(())
    }
}

impl Drop for SealedFullAudit {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// Debug includes every field used by FileNode/DirectoryNode equality and is
// only compared within this process. SHA-256 keeps the temporary DB compact.
fn node_identity(entry: IndexEntry) -> (String, i64, u64, [u8; 32]) {
    match entry {
        IndexEntry::File { path, node } => {
            let digest = Sha256::digest(format!("{node:?}").as_bytes()).into();
            (path, 1, node.meta.file_uid, digest)
        }
        IndexEntry::Directory { path, node } => {
            let digest = Sha256::digest(format!("{node:?}").as_bytes()).into();
            (path, 0, node.meta.file_uid, digest)
        }
    }
}

fn audit_sealed_full_difference(
    previous: &LtfsIndex,
    current_index: &LtfsIndex,
    current: ActiveIndexIdentity,
    files: &[(String, u64)],
) -> Result<()> {
    if current.incremental
        || current_index.incremental
        || current_index.generation != current.generation
        || current_index.self_location != current.location
    {
        return Err(TapeError::Catalog("封存 Full 索引身份不一致".into()));
    }
    let delta = current_index.incremental_since(previous)?;
    audit_sealed_delta(&delta, files)
}

fn audit_sealed_delta(delta: &LtfsIndex, files: &[(String, u64)]) -> Result<()> {
    let mut expected = HashMap::with_capacity(files.len());
    let mut directories = HashSet::new();
    for (path, uid) in files {
        let relative = path.trim_start_matches('/');
        if expected.insert(relative.to_owned(), *uid).is_some() {
            return Err(TapeError::Catalog("封存批次物理路径重复".into()));
        }
        let mut parent = String::new();
        for component in relative.split('/').take(relative.matches('/').count()) {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            directories.insert(parent.clone());
        }
    }
    let mut extra = false;
    delta.walk_files(|path, node| {
        if expected.remove(path) != Some(node.meta.file_uid) {
            extra = true;
        }
    });
    if extra || !expected.is_empty() || !delta_dirs_match(&delta.root, "", &directories) {
        return Err(TapeError::Catalog("封存批次与介质文件差异不一致".into()));
    }
    Ok(())
}

fn delta_dirs_match(dir: &DirectoryNode, parent: &str, expected: &HashSet<String>) -> bool {
    dir.subdirs.iter().all(|child| {
        let path = if parent.is_empty() {
            child.name.clone()
        } else {
            format!("{parent}/{}", child.name)
        };
        expected.contains(&path)
            && !child
                .delta_fields
                .as_ref()
                .is_some_and(|fields| fields.contains("deleted"))
            && delta_dirs_match(child, &path, expected)
    })
}

pub struct ActiveMediaCommit<'a> {
    volume: LtfsVolume<'a>,
    batch_id: Uuid,
    previous: ActiveIndexIdentity,
    current: ActiveIndexIdentity,
    files: Vec<PreparedActiveFile>,
}

impl<'a> ActiveMediaCommit<'a> {
    /// Read-only access for the current owner. Dropping the result leaves the
    /// committed DP index intact; callers cannot trigger an unrecorded IP
    /// checkpoint through `LtfsVolume::unmount`.
    pub fn volume(&self) -> &LtfsVolume<'a> {
        &self.volume
    }

    pub fn previous_index(&self) -> ActiveIndexIdentity {
        self.previous
    }

    pub fn current_index(&self) -> ActiveIndexIdentity {
        self.current
    }

    pub fn files(&self) -> &[PreparedActiveFile] {
        &self.files
    }
}

fn catalog_error(stage: &str, error: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!("FDB 活动写入 {stage} 失败：{error}"))
}

fn expect_outcome<T: std::fmt::Debug + PartialEq>(
    stage: &str,
    actual: T,
    expected: &[T],
) -> Result<()> {
    if expected.contains(&actual) {
        Ok(())
    } else {
        Err(TapeError::Catalog(format!(
            "FDB 活动写入 {stage} 返回拒绝结果：{actual:?}"
        )))
    }
}

/// Preflight the entire media batch before creating even one FDB reservation.
/// This rejects invalid/duplicate targets while the volume is still clean.
fn paths_for_batch(
    volume: Uuid,
    previous_generation: u64,
    batch_id: Uuid,
    uploads: &[ActiveMediaUpload],
) -> Result<Vec<String>> {
    if batch_id.is_nil() || uploads.is_empty() || uploads.len() > 65_536 {
        return Err(TapeError::Catalog("FDB 活动批次 ID 或文件数量无效".into()));
    }
    let first = &uploads[0].intent;
    let mut operations = HashSet::with_capacity(uploads.len());
    let mut paths = HashSet::with_capacity(uploads.len());
    let mut file_ids = HashSet::with_capacity(uploads.len());
    let mut physical = Vec::with_capacity(uploads.len());
    for upload in uploads {
        let intent = &upload.intent;
        let path = intent
            .ltfs_path()
            .map_err(|error| catalog_error("批次路径预检", error))?;
        if intent.namespace != first.namespace
            || intent.library_serial != first.library_serial
            || intent.owner_epoch != first.owner_epoch
            || intent.volume_id != volume
            || Some(intent.volume_generation) != previous_generation.checked_add(1)
            || intent.volume_scan_id != first.volume_scan_id
            || !operations.insert((intent.request_epoch, intent.operation_id))
            || !paths.insert((intent.parent, intent.name.clone()))
            || !file_ids.insert(intent.file_id)
        {
            return Err(TapeError::Catalog(
                "FDB 活动批次存在跨卷、跨 owner 或重复文件身份".into(),
            ));
        }
        physical.push(path);
    }
    Ok(physical)
}

/// Append an already spooled batch and record the exact committed index in FDB.
/// The caller must hold the drive PR and set the volume reservation guard.
/// This method intentionally stops at index observation: visibility, request
/// finalization, and compaction are resumed from the durable sealed batch.
pub async fn commit_active_media_batch<'a>(
    catalog: &ManagedCatalog,
    mut volume: LtfsVolume<'a>,
    batch_id: Uuid,
    uploads: &[ActiveMediaUpload],
) -> Result<ActiveMediaCommit<'a>> {
    let previous = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    let volume_id = previous.volume_uuid;
    let physical_paths = paths_for_batch(volume_id, previous.generation, batch_id, uploads)?;
    let serial = &uploads[0].intent.library_serial;
    let observed = catalog
        .observed_active_volume_index(serial, volume_id)
        .await
        .map_err(|error| catalog_error("核对已观察索引", error))?;
    if observed != previous {
        return Err(TapeError::Catalog(
            "FDB 已观察索引与当前受 PR 保护的介质不一致".into(),
        ));
    }

    let mut files = Vec::with_capacity(uploads.len());
    for (upload, path) in uploads.iter().zip(&physical_paths) {
        expect_outcome(
            "写前预约",
            catalog
                .reserve_active_file(&upload.intent)
                .await
                .map_err(|error| catalog_error("写前预约", error))?,
            &[PublishOutcome::Registered],
        )?;
        let mut spool = BufReader::new(File::open(&upload.spool)?);
        volume.append_file(path, &mut spool)?;
        let prepared = prepare_active_file(&volume, path, &upload.intent)
            .map_err(|error| catalog_error("核对物理收据", error))?;
        expect_outcome(
            "升级物理收据",
            catalog
                .complete_active_file(&prepared.receipt)
                .await
                .map_err(|error| catalog_error("升级物理收据", error))?,
            &[PublishOutcome::Registered],
        )?;
        files.push(prepared);
    }

    let receipts: Vec<CommitReceipt> = files.iter().map(|file| file.receipt.clone()).collect();
    expect_outcome(
        "封存写入批次",
        catalog
            .stage_active_batch(batch_id, &receipts)
            .await
            .map_err(|error| catalog_error("封存写入批次", error))?,
        &[
            ActiveBatchOutcome::Staged,
            ActiveBatchOutcome::AlreadyStaged,
        ],
    )?;
    volume.sync_batch()?;
    let current = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    expect_outcome(
        "登记已提交索引",
        catalog
            .record_active_volume_index(
                serial,
                uploads[0].intent.owner_epoch,
                volume_id,
                batch_id,
                previous,
                current,
            )
            .await
            .map_err(|error| catalog_error("登记已提交索引", error))?,
        &[
            ActiveIndexOutcome::Recorded,
            ActiveIndexOutcome::AlreadyRecorded,
        ],
    )?;
    Ok(ActiveMediaCommit {
        volume,
        batch_id,
        previous,
        current,
        files,
    })
}

/// Finish the observed S4 batch while the same owner still holds the PR. Each
/// FDB step is idempotent; after a failure the next fenced owner resumes from
/// durable batch, index and page records rather than appending again.
pub async fn publish_active_media_batch<'a>(
    catalog: &ManagedCatalog,
    mut committed: ActiveMediaCommit<'a>,
) -> Result<ActiveMediaCommit<'a>> {
    let current = ActiveIndexIdentity::from(committed.volume.verified_committed_index_construct()?);
    if current != committed.current {
        return Err(TapeError::Catalog(
            "FDB 活动发布时介质索引身份已经改变".into(),
        ));
    }
    let first = committed
        .files
        .first()
        .ok_or_else(|| TapeError::Catalog("FDB 活动发布批次为空".into()))?;
    let receipt = first.receipt.clone();
    let serial = receipt.library_serial.as_str();
    let volume = receipt.volume_id;
    let generation = receipt.volume_generation;
    let owner_epoch = receipt.owner_epoch;
    let observed = catalog
        .observed_active_volume_index(serial, volume)
        .await
        .map_err(|error| catalog_error("发布前核对索引", error))?;
    if observed != current {
        return Err(TapeError::Catalog(
            "FDB 活动发布时已观察索引与介质不一致".into(),
        ));
    }

    committed
        .files
        .sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
    for file in &committed.files {
        expect_outcome(
            "准备已观察文件",
            catalog
                .prepare_observed_active_file(
                    serial,
                    owner_epoch,
                    committed.batch_id,
                    &file.receipt,
                    &file.attributes,
                )
                .await
                .map_err(|error| catalog_error("准备已观察文件", error))?,
            &[
                ActivePreparationOutcome::Prepared,
                ActivePreparationOutcome::AlreadyPrepared,
            ],
        )?;
    }
    let pages = committed.files.chunks(128).count();
    for page in 0..pages {
        expect_outcome(
            "准备批次页",
            catalog
                .mark_observed_active_batch_page_prepared(
                    serial,
                    owner_epoch,
                    volume,
                    generation,
                    committed.batch_id,
                    page as u16,
                )
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
        catalog
            .seal_observed_active_preparation(
                serial,
                owner_epoch,
                volume,
                generation,
                committed.batch_id,
            )
            .await
            .map_err(|error| catalog_error("封存准备批次", error))?,
        &[
            ActivePreparationOutcome::Prepared,
            ActivePreparationOutcome::AlreadyPrepared,
        ],
    )?;
    if ActiveIndexIdentity::from(committed.volume.verified_committed_index_construct()?)
        != committed.current
    {
        return Err(TapeError::Catalog("FDB 活动发布前介质索引已改变".into()));
    }
    expect_outcome(
        "发布已观察批次",
        catalog
            .publish_observed_active_batch(
                serial,
                owner_epoch,
                volume,
                generation,
                committed.batch_id,
            )
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
            catalog
                .finalize_published_active_batch_page(
                    serial,
                    owner_epoch,
                    volume,
                    generation,
                    committed.batch_id,
                    page,
                )
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
                catalog
                    .materialize_published_active_file(
                        serial,
                        owner_epoch,
                        committed.batch_id,
                        page,
                        &file.receipt,
                    )
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
            catalog
                .finish_materialized_active_batch_page(
                    serial,
                    owner_epoch,
                    volume,
                    generation,
                    committed.batch_id,
                    page,
                )
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
        catalog
            .seal_materialized_active_batch(
                serial,
                owner_epoch,
                volume,
                generation,
                committed.batch_id,
            )
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
            catalog
                .cleanup_materialized_active_batch_page(
                    serial,
                    owner_epoch,
                    volume,
                    generation,
                    committed.batch_id,
                    page as u16,
                )
                .await
                .map_err(|error| catalog_error("清理归并页", error))?,
            &[
                ActiveCompactionOutcome::Completed,
                ActiveCompactionOutcome::AlreadyCompleted,
            ],
        )?;
    }
    Ok(committed)
}

#[cfg(test)]
mod sealed_full_audit_tests {
    use super::*;
    use crate::ltfs::index::{DirectoryNode, FileNode};

    fn file(path: &str, uid: u64, length: u64) -> IndexEntry {
        IndexEntry::File {
            path: path.into(),
            node: FileNode {
                name: path.rsplit('/').next().unwrap().into(),
                length,
                meta: crate::ltfs::index::NodeMeta {
                    file_uid: uid,
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    fn root() -> IndexEntry {
        IndexEntry::Directory {
            path: String::new(),
            node: DirectoryNode::default(),
        }
    }

    #[test]
    fn disk_comparison_accepts_one_sealed_change_among_many_files() {
        let mut audit = SealedFullAudit::new(&[("/dir/f4999".into(), 5000)]).unwrap();
        audit.record_previous(root()).unwrap();
        for n in 0..5000 {
            audit
                .record_previous(file(&format!("dir/f{n}"), n + 1, 1))
                .unwrap();
        }
        audit.check_current(root()).unwrap();
        for n in 0..5000 {
            audit
                .check_current(file(
                    &format!("dir/f{n}"),
                    n + 1,
                    if n == 4999 { 2 } else { 1 },
                ))
                .unwrap();
        }
        let path = audit.path.clone();
        audit.finish().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn disk_comparison_rejects_unsealed_metadata_change_and_missing_file() {
        let mut audit = SealedFullAudit::new(&[("/dir/f1".into(), 2)]).unwrap();
        audit.record_previous(root()).unwrap();
        audit.record_previous(file("dir/f0", 1, 1)).unwrap();
        audit.record_previous(file("dir/f1", 2, 1)).unwrap();
        audit.check_current(root()).unwrap();
        assert!(audit.check_current(file("dir/f0", 1, 2)).is_err());

        let mut audit = SealedFullAudit::new(&[]).unwrap();
        audit.record_previous(root()).unwrap();
        audit.record_previous(file("dir/f0", 1, 1)).unwrap();
        audit.check_current(root()).unwrap();
        assert!(audit.finish().is_err());
    }

    #[test]
    fn disk_comparison_accepts_replacement_with_new_file_uid() {
        let mut audit = SealedFullAudit::new(&[("/replaced".into(), 8)]).unwrap();
        audit.record_previous(root()).unwrap();
        audit.record_previous(file("replaced", 7, 1)).unwrap();
        audit.check_current(root()).unwrap();
        audit.check_current(file("replaced", 8, 2)).unwrap();
        audit.finish().unwrap();
    }
}
