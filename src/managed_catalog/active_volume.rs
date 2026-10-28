//! Active media index observation, separate from the immutable import scan.
//! This records a successful on-tape generation without publishing file heads.

use super::*;
use crate::catalog::volume_extents::ExpectedIndex;
use crate::ltfs::index::IndexLocation;

const INDEX_LEN: usize = 50;
const RECORD_LEN: usize = 1 + 2 * INDEX_LEN;
const HISTORY_LEN: usize = 1 + 16 + INDEX_LEN;
// Uppercase B belongs to request quota shards and is cleared by namespace
// maintenance. Active volume evidence must have a disjoint top-level prefix.
const ACTIVE_VOLUME_KIND: u8 = b'r';
const RECOVERY_KIND: u8 = b'3';

/// Exact on-tape index construct. An incremental construct is not a full
/// source for `VolumeExtentCache::build`; its chain must be materialized first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveIndexIdentity {
    pub volume_uuid: Uuid,
    pub generation: u64,
    pub location: IndexLocation,
    pub sha256: [u8; 32],
    pub incremental: bool,
}

/// One already-loaded cartridge bound to the current Active library owner.
/// This is FDB metadata evidence only; the device label and physical DP tail
/// must still be checked before any file content can be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveLoadedVolumeHead {
    pub volume_id: Uuid,
    pub pool_id: Uuid,
    pub index: ActiveIndexIdentity,
}

impl From<ExpectedIndex> for ActiveIndexIdentity {
    fn from(source: ExpectedIndex) -> Self {
        Self {
            volume_uuid: source.volume_uuid,
            generation: source.generation,
            location: source.location,
            sha256: source.sha256,
            incremental: false,
        }
    }
}

impl From<crate::ltfs::volume::CommittedIndexConstruct> for ActiveIndexIdentity {
    fn from(construct: crate::ltfs::volume::CommittedIndexConstruct) -> Self {
        Self {
            volume_uuid: construct.volume_uuid,
            generation: construct.generation,
            location: construct.location,
            sha256: construct.sha256,
            incremental: construct.incremental,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveIndexOutcome {
    Recorded,
    AlreadyRecorded,
    StaleOwner,
    IndexConflict,
    BatchConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveRecoveryOutcome {
    Begun,
    AlreadyBegun,
    Recorded,
    AlreadyRecorded,
    Cleared,
    StaleOwner,
    IndexConflict,
    BatchConflict,
    ReservationsPending,
}

fn recovery_key(volume: Uuid) -> Vec<u8> {
    let mut out = key(RECOVERY_KIND);
    out.extend_from_slice(volume.as_bytes());
    out
}

pub(super) async fn recovery_active(tr: &Transaction, volume: Uuid) -> Result<bool> {
    Ok(tr.get(&recovery_key(volume), false).await?.is_some())
}

fn observed_key(volume: Uuid) -> Vec<u8> {
    let mut out = key(ACTIVE_VOLUME_KIND);
    out.extend_from_slice(volume.as_bytes());
    out
}

fn generation_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut out = observed_key(volume);
    out.extend_from_slice(&generation.to_be_bytes());
    out
}

fn valid_index(volume: Uuid, index: ActiveIndexIdentity) -> bool {
    index.volume_uuid == volume
        && index.generation > 0
        && matches!(index.location.partition, 'a' | 'b')
        && index.location.start_block > 0
}

fn encode_index(out: &mut Vec<u8>, index: ActiveIndexIdentity) {
    out.extend_from_slice(&index.generation.to_be_bytes());
    out.push(index.location.partition as u8);
    out.extend_from_slice(&index.location.start_block.to_be_bytes());
    out.extend_from_slice(&index.sha256);
    out.push(u8::from(index.incremental));
}

fn decode_index(volume: Uuid, bytes: &[u8]) -> Result<ActiveIndexIdentity> {
    if bytes.len() != INDEX_LEN || bytes[49] > 1 {
        return Err(CatalogError::Corrupt("活动卷索引身份长度"));
    }
    let index = ActiveIndexIdentity {
        volume_uuid: volume,
        generation: read_u64(bytes),
        location: IndexLocation {
            partition: bytes[8] as char,
            start_block: read_u64(&bytes[9..17]),
        },
        sha256: bytes[17..49].try_into().expect("validated length"),
        incremental: bytes[49] == 1,
    };
    if !valid_index(volume, index) {
        return Err(CatalogError::Corrupt("活动卷索引身份或位置"));
    }
    Ok(index)
}

fn encode_history(batch_id: Uuid, index: ActiveIndexIdentity) -> Vec<u8> {
    let mut out = Vec::with_capacity(HISTORY_LEN);
    out.push(if batch_id.is_nil() { 2 } else { 1 });
    out.extend_from_slice(batch_id.as_bytes());
    encode_index(&mut out, index);
    out
}

fn decode_history(volume: Uuid, bytes: &[u8]) -> Result<(Uuid, ActiveIndexIdentity)> {
    if bytes.len() != HISTORY_LEN || !matches!(bytes[0], 1 | 2) {
        return Err(CatalogError::Corrupt("活动卷历史批次版本或长度"));
    }
    let batch_id = uuid_from(&bytes[1..17])?;
    if batch_id.is_nil() != (bytes[0] == 2) {
        return Err(CatalogError::Corrupt("活动卷历史批次 ID"));
    }
    Ok((batch_id, decode_index(volume, &bytes[17..])?))
}

fn encode_record(previous: ActiveIndexIdentity, current: ActiveIndexIdentity) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_LEN);
    out.push(1);
    encode_index(&mut out, previous);
    encode_index(&mut out, current);
    out
}

fn decode_record(volume: Uuid, bytes: &[u8]) -> Result<(ActiveIndexIdentity, ActiveIndexIdentity)> {
    if bytes.len() != RECORD_LEN || bytes[0] != 1 {
        return Err(CatalogError::Corrupt("活动卷索引记录版本或长度"));
    }
    let previous = decode_index(volume, &bytes[1..1 + INDEX_LEN])?;
    let current = decode_index(volume, &bytes[1 + INDEX_LEN..])?;
    if previous.generation.checked_add(1) != Some(current.generation) {
        return Err(CatalogError::Corrupt("活动卷索引代数不连续"));
    }
    Ok((previous, current))
}

fn require_after_baseline(
    baseline: ActiveIndexIdentity,
    previous: ActiveIndexIdentity,
    current: ActiveIndexIdentity,
) -> Result<()> {
    if previous.generation < baseline.generation
        || (previous.generation == baseline.generation && previous != baseline)
        || current.generation <= baseline.generation
    {
        return Err(CatalogError::Corrupt("活动卷索引早于或偏离导入基线"));
    }
    Ok(())
}

async fn active_baseline(
    tr: &Transaction,
    serial: &str,
    volume: Uuid,
) -> Result<(VolumeScan, ActiveIndexIdentity)> {
    let placement = control::verified_placement(tr, volume)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.library_serial != serial {
        return Err(CatalogError::Invalid("活动卷与带库序列号不一致"));
    }
    let scan = require_volume_ready(tr, volume, None).await?;
    let ready = tr
        .get(&ready_volume_key(volume), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let (ready_scan, ready_serial) = decode_ready_volume(&ready)?;
    if ready_scan != scan || ready_serial != serial {
        return Err(CatalogError::Corrupt("活动卷就绪扫描带库序列号不一致"));
    }
    Ok((
        scan,
        index_evidence::require_source(tr, volume, scan)
            .await?
            .into(),
    ))
}

pub(super) async fn observed_at(
    tr: &Transaction,
    serial: &str,
    volume: Uuid,
) -> Result<(VolumeScan, ActiveIndexIdentity)> {
    let (scan, baseline) = active_baseline(tr, serial, volume).await?;
    let Some(value) = tr.get(&observed_key(volume), false).await? else {
        return Ok((scan, baseline));
    };
    let (previous, current) = decode_record(volume, &value)?;
    require_after_baseline(baseline, previous, current)?;
    let recorded = tr
        .get(&generation_key(volume, current.generation), false)
        .await?
        .ok_or(CatalogError::Corrupt("活动卷最新代数缺少索引身份"))?;
    if decode_history(volume, &recorded)?.1 != current {
        return Err(CatalogError::Corrupt("活动卷最新代数与索引身份不一致"));
    }
    Ok((scan, current))
}

/// The caller already checked the Active route and owner in this transaction.
/// Return None for a receipt bound to an obsolete import scan or placement.
pub(super) async fn request_current_index(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<Option<ActiveIndexIdentity>> {
    let placement = control::verified_placement(tr, receipt.volume_id)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if placement.namespace != receipt.namespace
        || placement.library_serial != receipt.library_serial
    {
        return Ok(None);
    }
    let (scan, current) = observed_at(tr, &receipt.library_serial, receipt.volume_id).await?;
    Ok((scan.id == receipt.volume_scan_id).then_some(current))
}

/// Resolve the immutable batch proof at one observed generation using the
/// caller's FDB read version. An unobserved generation is not a tape commit.
pub(super) async fn observed_batch_at(
    tr: &Transaction,
    serial: &str,
    volume: Uuid,
    generation: u64,
) -> Result<Option<(Uuid, ActiveIndexIdentity)>> {
    let (scan, current) = observed_at(tr, serial, volume).await?;
    if generation <= scan.generation || generation > current.generation {
        return Ok(None);
    }
    let bytes = tr
        .get(&generation_key(volume, generation), false)
        .await?
        .ok_or(CatalogError::Corrupt("活动卷历史代数缺少索引身份"))?;
    let (batch_id, index) = decode_history(volume, &bytes)?;
    if index.generation != generation || (generation == current.generation && index != current) {
        return Err(CatalogError::Corrupt("活动卷历史代数与最新游标不一致"));
    }
    Ok((!batch_id.is_nil()).then_some((batch_id, index)))
}

impl ManagedCatalog {
    /// Resolve loaded-tape placement and the observed DP head from one FDB
    /// read version, after verifying the exact route and owner epoch.
    /// Unassigned and assigned-but-unbound cartridges return None and must
    /// not be probed by the device executor.
    pub async fn active_loaded_volume_head(
        &self,
        serial: &str,
        barcode: &str,
        route: ActiveLibraryRoute,
        owner_epoch: u64,
    ) -> Result<Option<ActiveLoadedVolumeHead>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_runtime_active_owner(&tr, serial, route, owner_epoch).await?;
        let Some(assignment) = Self::verified_assignment(&tr, serial, barcode).await? else {
            return Ok(None);
        };
        if assignment.namespace != namespace {
            return Err(CatalogError::Corrupt("已装载磁带与活动带库命名空间不一致"));
        }
        let Some(volume_id) = assignment.volume_id else {
            return Ok(None);
        };
        let (_, index) = observed_at(&tr, serial, volume_id).await?;
        Ok(Some(ActiveLoadedVolumeHead {
            volume_id,
            pool_id: assignment.pool_id,
            index,
        }))
    }

    /// Bind every predecessor consumed by disk-backed media replay to an
    /// immutable FDB identity at the current owner's single read version.
    /// This never authorizes an unobserved or pre-import history construct.
    pub async fn verify_active_index_predecessors(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner_epoch: u64,
        head: ActiveIndexIdentity,
        predecessors: &[ActiveIndexIdentity],
    ) -> Result<()> {
        control::validate_physical_serial(serial)?;
        if predecessors.len() > 64 || head.incremental == predecessors.is_empty() {
            return Err(CatalogError::Invalid("活动索引前驱数量或类型不符"));
        }
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_runtime_active_owner(&tr, serial, route, owner_epoch).await?;
        let placement = control::verified_placement(&tr, head.volume_uuid)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace || placement.library_serial != serial {
            return Err(CatalogError::Corrupt("活动索引重放卷与带库路由不一致"));
        }
        let (_, current) = observed_at(&tr, serial, head.volume_uuid).await?;
        if current != head {
            return Err(CatalogError::Incomplete);
        }
        let (_, baseline) = active_baseline(&tr, serial, head.volume_uuid).await?;
        let mut successor = head;
        for (offset, predecessor) in predecessors.iter().enumerate() {
            if predecessor.volume_uuid != head.volume_uuid
                || predecessor.location.partition != head.location.partition
                || predecessor.location.start_block >= successor.location.start_block
                || predecessor.generation >= successor.generation
                || predecessor.incremental != (offset + 1 != predecessors.len())
            {
                return Err(CatalogError::Invalid("活动索引前驱顺序或身份不符"));
            }
            let recorded = if predecessor.generation == baseline.generation {
                baseline
            } else {
                if predecessor.generation < baseline.generation {
                    return Err(CatalogError::Incomplete);
                }
                let bytes = tr
                    .get(
                        &generation_key(head.volume_uuid, predecessor.generation),
                        false,
                    )
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
                decode_history(head.volume_uuid, &bytes)?.1
            };
            if recorded != *predecessor {
                return Err(CatalogError::Corrupt("介质重放前驱与 FDB 索引身份不符"));
            }
            successor = *predecessor;
        }
        Ok(())
    }

    /// Read the latest observed committed index. It is a recovery cursor, not
    /// a file read plan; the import scan and file heads remain unchanged.
    pub async fn observed_active_volume_index(
        &self,
        serial: &str,
        volume: Uuid,
    ) -> Result<ActiveIndexIdentity> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace {
            return Err(CatalogError::Invalid("活动卷与活动命名空间不一致"));
        }
        let (_, current) = observed_at(&tr, serial, volume).await?;
        Ok(current)
    }

    /// Look up a durable index identity for an older Active copy. The import
    /// baseline remains in its immutable source record; each later generation
    /// has a separate immutable record. This is recovery evidence only, not a
    /// file read plan or proof that a local extent cache is available.
    pub async fn observed_active_volume_generation(
        &self,
        serial: &str,
        volume: Uuid,
        generation: u64,
    ) -> Result<Option<ActiveIndexIdentity>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_active_namespace(&tr, serial).await?;
        let placement = control::verified_placement(&tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement.namespace != namespace {
            return Err(CatalogError::Invalid("活动卷与活动命名空间不一致"));
        }
        let (_, baseline) = active_baseline(&tr, serial, volume).await?;
        if generation < baseline.generation {
            return Ok(None);
        }
        if generation == baseline.generation {
            return Ok(Some(baseline));
        }
        let Some(latest) = tr.get(&observed_key(volume), false).await? else {
            return Ok(None);
        };
        let (previous, current) = decode_record(volume, &latest)?;
        require_after_baseline(baseline, previous, current)?;
        if generation > current.generation {
            return Ok(None);
        }
        let bytes = tr
            .get(&generation_key(volume, generation), false)
            .await?
            .ok_or(CatalogError::Corrupt("活动卷历史代数缺少索引身份"))?;
        let (_, index) = decode_history(volume, &bytes)?;
        if index.generation != generation || (generation == current.generation && index != current)
        {
            return Err(CatalogError::Corrupt("活动卷历史代数与最新游标不一致"));
        }
        Ok(Some(index))
    }

    /// Record an already successful LTFS commit after device PR verification.
    /// This does not publish any file: a later runtime batch must reconcile
    /// and atomically publish the media delta with its logical head/copy rows.
    pub async fn record_active_volume_index(
        &self,
        serial: &str,
        owner_epoch: u64,
        volume: Uuid,
        batch_id: Uuid,
        previous: ActiveIndexIdentity,
        current: ActiveIndexIdentity,
    ) -> Result<ActiveIndexOutcome> {
        control::validate_physical_serial(serial)?;
        if batch_id.is_nil()
            || !valid_index(volume, previous)
            || !valid_index(volume, current)
            || previous.generation.checked_add(1) != Some(current.generation)
        {
            return Err(CatalogError::Invalid("活动卷索引提交身份或连续代数无效"));
        }
        let observed = observed_key(volume);
        let generation = generation_key(volume, current.generation);
        self.db
            .run(|tr, _| {
                let observed = observed.clone();
                let generation = generation.clone();
                async move {
                    requests::retry_result(
                        async {
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.library_serial != serial {
                                return Err(CatalogError::Invalid("活动卷与带库序列号不一致"));
                            }
                            if !routing::require_active_owner(
                                &tr,
                                placement.namespace,
                                serial,
                                owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActiveIndexOutcome::StaleOwner);
                            }
                            let (scan, baseline) = active_baseline(&tr, serial, volume).await?;
                            let saved = tr.get(&observed, false).await?;
                            let observed_index = if let Some(bytes) = saved {
                                let (saved_previous, saved_current) =
                                    decode_record(volume, &bytes)?;
                                require_after_baseline(baseline, saved_previous, saved_current)?;
                                let recorded = tr
                                    .get(&generation_key(volume, saved_current.generation), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("活动卷最新代数缺少索引身份"))?;
                                let (recorded_batch, recorded_index) =
                                    decode_history(volume, &recorded)?;
                                if recorded_index != saved_current {
                                    return Err(CatalogError::Corrupt(
                                        "活动卷最新代数与索引身份不一致",
                                    ));
                                }
                                if saved_previous == previous && saved_current == current {
                                    return Ok(if recorded_batch == batch_id {
                                        ActiveIndexOutcome::AlreadyRecorded
                                    } else {
                                        ActiveIndexOutcome::BatchConflict
                                    });
                                }
                                saved_current
                            } else {
                                baseline
                            };
                            if observed_index != previous {
                                return Ok(ActiveIndexOutcome::IndexConflict);
                            }
                            if recovery_active(&tr, volume).await? {
                                return Ok(ActiveIndexOutcome::IndexConflict);
                            }
                            if let Some(existing) = tr.get(&generation, false).await? {
                                let (existing_batch, existing_index) =
                                    decode_history(volume, &existing)?;
                                if existing_index != current {
                                    return Ok(ActiveIndexOutcome::IndexConflict);
                                }
                                if existing_batch != batch_id {
                                    return Ok(ActiveIndexOutcome::BatchConflict);
                                }
                            }
                            if !active_batch::sealed_for_index(
                                &tr,
                                placement.namespace,
                                volume,
                                current.generation,
                                batch_id,
                                owner_epoch,
                                scan.id,
                            )
                            .await?
                            {
                                return Ok(ActiveIndexOutcome::BatchConflict);
                            }
                            tr.set(&observed, &encode_record(previous, current));
                            tr.set(&generation, &encode_history(batch_id, current));
                            Ok(ActiveIndexOutcome::Recorded)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Fence new admissions while a PR-holding owner reconciles an unindexed
    /// tail. The marker survives owner changes and unknown FDB outcomes.
    pub async fn begin_active_volume_recovery(
        &self,
        serial: &str,
        owner_epoch: u64,
        previous: ActiveIndexIdentity,
    ) -> Result<ActiveRecoveryOutcome> {
        control::validate_physical_serial(serial)?;
        if owner_epoch == 0 || !valid_index(previous.volume_uuid, previous) {
            return Err(CatalogError::Invalid("活动卷收尾身份无效"));
        }
        let volume = previous.volume_uuid;
        let marker = recovery_key(volume);
        let mut evidence = Vec::with_capacity(INDEX_LEN);
        encode_index(&mut evidence, previous);
        self.db
            .run(|tr, _| {
                let marker = marker.clone();
                let evidence = evidence.clone();
                async move {
                    requests::retry_result(
                        async {
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.library_serial != serial {
                                return Err(CatalogError::Invalid("活动卷收尾带库不一致"));
                            }
                            if !routing::require_active_owner(
                                &tr,
                                placement.namespace,
                                serial,
                                owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActiveRecoveryOutcome::StaleOwner);
                            }
                            let (_, current) = observed_at(&tr, serial, volume).await?;
                            if current != previous {
                                return Ok(ActiveRecoveryOutcome::IndexConflict);
                            }
                            let next = previous
                                .generation
                                .checked_add(1)
                                .ok_or(CatalogError::Invalid("活动卷代数溢出"))?;
                            if !active_batch::unsealed_or_absent_for_recovery(&tr, volume, next)
                                .await?
                            {
                                return Ok(ActiveRecoveryOutcome::BatchConflict);
                            }
                            if let Some(saved) = tr.get(&marker, false).await? {
                                return Ok(if saved.as_ref() == evidence {
                                    ActiveRecoveryOutcome::AlreadyBegun
                                } else {
                                    ActiveRecoveryOutcome::IndexConflict
                                });
                            }
                            tr.set(&marker, &evidence);
                            Ok(ActiveRecoveryOutcome::Begun)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Complete a media-proven empty close-tail generation, or clear the gate
    /// when power loss left no persistent tail. All Q rows must be drained.
    pub async fn finish_active_volume_recovery(
        &self,
        serial: &str,
        owner_epoch: u64,
        previous: ActiveIndexIdentity,
        recovered: Option<ActiveIndexIdentity>,
    ) -> Result<ActiveRecoveryOutcome> {
        control::validate_physical_serial(serial)?;
        let volume = previous.volume_uuid;
        if owner_epoch == 0
            || !valid_index(volume, previous)
            || recovered.is_some_and(|index| {
                !valid_index(volume, index)
                    || previous.generation.checked_add(1) != Some(index.generation)
            })
        {
            return Err(CatalogError::Invalid("活动卷收尾索引身份无效"));
        }
        let marker = recovery_key(volume);
        let next = previous
            .generation
            .checked_add(1)
            .ok_or(CatalogError::Invalid("活动卷代数溢出"))?;
        let observed = observed_key(volume);
        let history = generation_key(volume, next);
        self.db
            .run(|tr, _| {
                let marker = marker.clone();
                let observed = observed.clone();
                let history = history.clone();
                async move {
                    requests::retry_result(
                        async {
                            let placement = control::verified_placement(&tr, volume)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if placement.library_serial != serial {
                                return Err(CatalogError::Invalid("活动卷收尾带库不一致"));
                            }
                            if !routing::require_active_owner(
                                &tr,
                                placement.namespace,
                                serial,
                                owner_epoch,
                            )
                            .await?
                            {
                                return Ok(ActiveRecoveryOutcome::StaleOwner);
                            }
                            let (_, current) = observed_at(&tr, serial, volume).await?;
                            if let Some(index) = recovered
                                && current == index
                            {
                                let saved = tr
                                    .get(&history, false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("空收尾历史缺失"))?;
                                return Ok(
                                    if decode_history(volume, &saved)? == (Uuid::nil(), index) {
                                        ActiveRecoveryOutcome::AlreadyRecorded
                                    } else {
                                        ActiveRecoveryOutcome::IndexConflict
                                    },
                                );
                            }
                            if current != previous {
                                return Ok(ActiveRecoveryOutcome::IndexConflict);
                            }
                            let mut evidence = Vec::with_capacity(INDEX_LEN);
                            encode_index(&mut evidence, previous);
                            if tr.get(&marker, false).await?.as_deref() != Some(evidence.as_slice())
                            {
                                return Ok(ActiveRecoveryOutcome::IndexConflict);
                            }
                            if tr
                                .get(&active_batch::batch_key(volume, next), false)
                                .await?
                                .is_some()
                            {
                                return Ok(ActiveRecoveryOutcome::BatchConflict);
                            }
                            if !reservation::volume_reservations_empty(&tr, volume, next).await? {
                                return Ok(ActiveRecoveryOutcome::ReservationsPending);
                            }
                            if let Some(index) = recovered {
                                if tr.get(&history, false).await?.is_some() {
                                    return Ok(ActiveRecoveryOutcome::IndexConflict);
                                }
                                tr.set(&observed, &encode_record(previous, index));
                                tr.set(&history, &encode_history(Uuid::nil(), index));
                            }
                            tr.clear(&marker);
                            Ok(if recovered.is_some() {
                                ActiveRecoveryOutcome::Recorded
                            } else {
                                ActiveRecoveryOutcome::Cleared
                            })
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_index_codec_rejects_truncation_corruption_and_generation_gaps() {
        let volume = Uuid::new_v4();
        let previous = ActiveIndexIdentity {
            volume_uuid: volume,
            generation: 8,
            location: IndexLocation {
                partition: 'b',
                start_block: 17,
            },
            sha256: [4; 32],
            incremental: false,
        };
        let current = ActiveIndexIdentity {
            generation: 9,
            location: IndexLocation {
                partition: 'b',
                start_block: 23,
            },
            sha256: [5; 32],
            incremental: true,
            ..previous
        };
        let encoded = encode_record(previous, current);
        assert_eq!(
            decode_record(volume, &encoded).unwrap(),
            (previous, current)
        );
        for end in 0..encoded.len() {
            assert!(decode_record(volume, &encoded[..end]).is_err());
        }
        for offset in [0, 9, 59] {
            let mut invalid = encoded.clone();
            invalid[offset] = 0;
            assert!(decode_record(volume, &invalid).is_err());
        }
        let mut invalid = encoded;
        invalid[51..59].copy_from_slice(&11_u64.to_be_bytes());
        assert!(decode_record(volume, &invalid).is_err());
        invalid[51..59].copy_from_slice(&9_u64.to_be_bytes());
        invalid[100] = 2;
        assert!(decode_record(volume, &invalid).is_err());

        let batch_id = Uuid::new_v4();
        let history = encode_history(batch_id, current);
        assert_eq!(
            decode_history(volume, &history).unwrap(),
            (batch_id, current)
        );
        for end in 0..history.len() {
            assert!(decode_history(volume, &history[..end]).is_err());
        }
        let mut invalid = history.clone();
        invalid[0] = 2;
        assert!(decode_history(volume, &invalid).is_err());
        invalid = history.clone();
        invalid[1..17].fill(0);
        assert!(decode_history(volume, &invalid).is_err());
        invalid = history;
        invalid[25] = 0;
        assert!(decode_history(volume, &invalid).is_err());

        let empty = encode_history(Uuid::nil(), current);
        assert_eq!(empty[0], 2);
        assert_eq!(
            decode_history(volume, &empty).unwrap(),
            (Uuid::nil(), current)
        );
        let mut invalid = empty.clone();
        invalid[0] = 1;
        assert!(decode_history(volume, &invalid).is_err());
        invalid = empty;
        invalid[1..17].copy_from_slice(batch_id.as_bytes());
        assert!(decode_history(volume, &invalid).is_err());
    }
}
