//! Original reclaim members share one physical body/archive checkpoint.
//! This lane never records an index observation or publishes a business result.
use super::*;
use crate::daemon::executor::{FreshReclaimGroupIo, ObserveReclaimGroupIo};
use crate::ltfs::recovery::CapturedIndexSource;
use crate::ltfs::volume::VolumeSession;
use crate::managed_catalog::ReclaimIndexAugmentation;

pub(crate) struct PendingCheckpoint<'a> {
    commit: ActiveMediaCommit<'a>,
    plan: ReclaimIndexAugmentation,
}

/// Move the original checkpoint across a FreshWrite -> Observe guard change.
/// No file, path, operation, batch, or physical proof is recreated here.
pub(crate) struct ReclaimCheckpointSession {
    volume: VolumeSession,
    batch_id: Uuid,
    previous: ActiveIndexIdentity,
    current: ActiveIndexIdentity,
    files: Vec<PreparedActiveFile>,
    physical_paths: HashMap<Uuid, String>,
    plan: ReclaimIndexAugmentation,
}

impl<'a> PendingCheckpoint<'a> {
    pub(crate) fn plan(&self) -> &ReclaimIndexAugmentation {
        &self.plan
    }
    pub(crate) fn previous_index(&self) -> ActiveIndexIdentity {
        self.commit.previous
    }
    pub(crate) fn current_index(&self) -> ActiveIndexIdentity {
        self.commit.current
    }
    pub(crate) fn batch_id(&self) -> Uuid {
        self.commit.batch_id
    }
    pub(crate) fn files(&self) -> &[PreparedActiveFile] {
        &self.commit.files
    }
    pub(crate) fn volume(&self) -> &LtfsVolume<'a> {
        &self.commit.volume
    }

    pub(crate) fn into_parts(self) -> (ActiveMediaCommit<'a>, ReclaimIndexAugmentation) {
        (self.commit, self.plan)
    }

    pub(crate) fn into_session(self) -> Result<ReclaimCheckpointSession> {
        let ActiveMediaCommit {
            volume,
            batch_id,
            previous,
            current,
            files,
            physical_paths,
        } = self.commit;
        let volume = volume.into_session().ok_or_else(|| {
            TapeError::NotReady("原回收检查点无法保持设备epoch；必须观察原batch，禁止重写".into())
        })?;
        Ok(ReclaimCheckpointSession {
            volume,
            batch_id,
            previous,
            current,
            files,
            physical_paths,
            plan: self.plan,
        })
    }
}

impl ReclaimCheckpointSession {
    /// Only the executor's original Observe authority can resume this same
    /// physical checkpoint. All binding checks precede the first device CDB.
    pub(in crate::daemon) fn resume<'a>(
        self,
        io: &'a ObserveReclaimGroupIo<'_>,
    ) -> Result<PendingCheckpoint<'a>> {
        io.require_checkpoint(self.batch_id, self.previous, self.current, &self.plan)?;
        let volume = io.resume_volume(self.volume)?;
        Ok(PendingCheckpoint {
            commit: ActiveMediaCommit {
                volume,
                batch_id: self.batch_id,
                previous: self.previous,
                current: self.current,
                files: self.files,
                physical_paths: self.physical_paths,
            },
            plan: self.plan,
        })
    }
}

fn required_file_count(existing: u64, members: usize, limit: u64) -> Result<u64> {
    let added = u64::try_from(members)
        .ok()
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| TapeError::Catalog("回收正文及归档文件数溢出".into()))?;
    existing
        .checked_add(added)
        .filter(|n| members > 0 && existing < limit && *n <= limit)
        .ok_or_else(|| TapeError::NotReady("目标卷容纳不下正文及完整归档节点".into()))
}

fn path_is_absent(index: &LtfsIndex, path: &str) -> bool {
    let mut dir = &index.root;
    let mut parts = path.trim_start_matches('/').split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() || dir.files.iter().any(|file| file.name == part) {
            return false;
        }
        let child = dir.subdirs.iter().find(|child| child.name == part);
        if parts.peek().is_none() {
            return child.is_none();
        }
        match child {
            Some(child) => dir = child,
            None => return true,
        }
    }
    false
}

fn require_absent_paths(index: &LtfsIndex, donors: &[String], archives: &[&str]) -> Result<()> {
    if donors.len() != archives.len() || donors.is_empty() {
        return Err(TapeError::Catalog("回收正文和归档路径数量不符".into()));
    }
    let mut ordered = Vec::with_capacity(donors.len() * 2);
    for path in donors
        .iter()
        .map(String::as_str)
        .chain(archives.iter().copied())
    {
        if !path_is_absent(index, path) {
            return Err(TapeError::Catalog(
                "原目标已有同名节点或文件祖先，拒绝覆盖原生对象".into(),
            ));
        }
        ordered.push(path.trim_start_matches('/'));
    }
    let all: HashSet<_> = ordered.iter().copied().collect();
    if all.len() != ordered.len() {
        return Err(TapeError::Catalog("回收组正文与归档路径重复".into()));
    }
    for path in ordered {
        let mut ancestor = path.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(parent) = ancestor {
            if all.contains(parent) {
                return Err(TapeError::Catalog(
                    "回收组正文与归档路径重复或祖先冲突".into(),
                ));
            }
            ancestor = parent.rsplit_once('/').map(|(parent, _)| parent);
        }
    }
    Ok(())
}

fn require_fresh_attempts(observed: &[bool], members: usize) -> Result<()> {
    if members == 0 || observed.len() != members || observed.iter().any(|seen| *seen) {
        return Err(TapeError::Catalog(
            "原回收组已有或缺少物理尝试事实，必须观察原结果，禁止再次追加".into(),
        ));
    }
    Ok(())
}

/// The fresh group guard either rechecks the same drive's read-only prepared
/// mount or mounts it directly. Body/S4 commands always require original
/// permission, PR and fresh-group authorization.
pub(in crate::daemon) fn commit_reclaim_media_checkpoint<'a>(
    io: &'a FreshReclaimGroupIo<'_>,
    target: &CapturedIndexSource,
    max_attributes_bytes: usize,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<PendingCheckpoint<'a>>>>> {
    Box::pin(async move {
        if max_attributes_bytes == 0 {
            return Err(TapeError::Catalog("回收属性字节预算必须大于零".into()));
        }
        let catalog = io.catalog();
        let scope = io.scope();
        let uploads = io.uploads();
        scope
            .intent()
            .require_full_dp_checkpoint_policy()
            .map_err(|e| catalog_error("回收组挂载前核对Full DP检查点策略", e))?;
        scope
            .intent()
            .require_target(target)
            .map_err(|e| catalog_error("回收组挂载前核对原目标捕获", e))?;
        io.verify()?;
        let mut volume = io.mount(target.construct())?;
        let previous = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
        let batch_id = scope.batch_id();
        let paths = paths_for_batch(previous.volume_uuid, previous.generation, batch_id, uploads)?;
        if ActiveIndexIdentity::from(target.construct()) != previous {
            return Err(TapeError::Catalog(
                "原目标捕获与实际挂载卷头不同，禁止追加".into(),
            ));
        }
        let first = &uploads[0].intent;
        let serial = &first.library_serial;
        let owner = first.owner_epoch;
        let pool_id = first
            .pool_id
            .ok_or_else(|| TapeError::Catalog("回收组缺少池身份".into()))?;
        let route = catalog
            .active_library_route(serial)
            .await
            .map_err(|e| catalog_error("回收组查询活动路由", e))?
            .ok_or_else(|| TapeError::NotReady("活动带库不存在".into()))?;
        let intents: Vec<_> = uploads.iter().map(|upload| upload.intent.clone()).collect();
        let observed = catalog
            .check_reclaim_write_group(scope, &intents)
            .await
            .map_err(|e| catalog_error("回收组原物理尝试预检", e))?;
        require_fresh_attempts(&observed, uploads.len())?;
        for upload in uploads {
            catalog
                .check_reclaim_upload_gate(route, owner, &upload.intent.request(), scope)
                .await
                .map_err(|e| pool_gate_error("回收组写前池屏障", pool_id, e))?;
        }
        if catalog
            .observed_active_volume_index(serial, previous.volume_uuid)
            .await
            .map_err(|e| catalog_error("回收组写前卷头", e))?
            != previous
        {
            return Err(TapeError::Catalog("原回收组的FDB与实际卷头不一致".into()));
        }
        require_media_io(catalog, serial, route, owner, previous.volume_uuid).await?;
        let pool = catalog
            .active_pool_for_route(serial, route, pool_id)
            .await
            .map_err(|e| catalog_error("回收组池策略", e))?
            .ok_or_else(|| TapeError::Catalog("回收池不存在".into()))?;
        let count = catalog
            .active_volume_file_count(serial, route, owner, previous.volume_uuid)
            .await
            .map_err(|e| catalog_error("回收组原物理文件数", e))?;
        if count != index_file_count(volume.index()) {
            return Err(TapeError::Catalog(
                "回收组原卷文件数与已核验索引不符".into(),
            ));
        }
        let expected_count = required_file_count(count, uploads.len(), pool.file_limit)?;
        let archives: Vec<_> = scope.intent().archive_paths().collect();
        require_absent_paths(volume.index(), &paths, &archives)?;
        scope
            .intent()
            .require_archive_base(volume.index())
            .map_err(|e| catalog_error("回收组归档根写前核验", e))?;
        let mut remaining = max_attributes_bytes;
        let mut metadata = Vec::with_capacity(uploads.len());
        for upload in uploads {
            let (attributes, bytes) = scope
                .relocation_metadata(&upload.intent)
                .map_err(|e| catalog_error("读取原封存回收属性", e))?;
            remaining = remaining
                .checked_sub(bytes)
                .ok_or_else(|| TapeError::Catalog("回收属性超过批次字节预算".into()))?;
            metadata.push(attributes);
        }
        #[cfg(feature = "hardware-fault-tests")]
        let test_gate = super::s4_gate::arm_from_environment(previous, batch_id, uploads)?;
        let mut files = Vec::with_capacity(uploads.len());
        for ((upload, path), metadata) in uploads.iter().zip(&paths).zip(&metadata) {
            expect_outcome(
                "回收组写前预约",
                catalog
                    .reserve_reclaim_group_file(&upload.intent, scope)
                    .await
                    .map_err(|e| pool_gate_error("回收组写前预约", pool_id, e))?,
                &[PublishOutcome::Registered],
            )?;
            io.confirm_reserved(&upload.intent)?;
            let mut spool = BufReader::new(File::open(&upload.spool)?);
            volume.append_replacement_file(path, &mut spool, metadata)?;
            let prepared = prepare_active_file(&volume, path, &upload.intent)
                .map_err(|e| catalog_error("回收组实际正文收据", e))?;
            expect_outcome(
                "回收组升级正文收据",
                catalog
                    .complete_reclaim_group_file(&prepared.receipt, scope)
                    .await
                    .map_err(|e| catalog_error("回收组升级正文收据", e))?,
                &[PublishOutcome::Registered],
            )?;
            files.push(prepared);
        }
        let receipts: Vec<_> = files.iter().map(|file| file.receipt.clone()).collect();
        let plan = ReclaimIndexAugmentation::derive(
            scope.intent(),
            scope.group(),
            &receipts,
            &paths,
            &volume,
            target,
        )
        .map_err(|e| catalog_error("原回收组实际索引计划", e))?;
        if plan.target_file_count() != expected_count {
            return Err(TapeError::Catalog(
                "原实际计划的正文及归档文件数与写前预算不符".into(),
            ));
        }
        catalog
            .stage_reclaim_index_augmentation(
                scope.parent(),
                owner,
                scope.group().id(),
                &plan,
                &receipts,
            )
            .await
            .map_err(|e| catalog_error("持久化原回收组实际索引计划", e))?;
        volume.stage_reclaim_pending_references(&plan)?;
        let (pending, _) = volume.reclaim_pending_index()?;
        if index_file_count(pending) != expected_count {
            return Err(TapeError::Catalog(
                "回收组待提交索引遗漏正文或归档节点".into(),
            ));
        }
        expect_outcome(
            "封存原回收共享检查点",
            catalog
                .stage_reclaim_active_batch(scope, &receipts)
                .await
                .map_err(|e| catalog_error("封存原回收共享检查点", e))?,
            &[
                ActiveBatchOutcome::Staged,
                ActiveBatchOutcome::AlreadyStaged,
            ],
        )?;
        // Exactly one S4 covers both actual donors and their original raw metadata.
        let current = sync_reclaim_full_dp(&mut volume)?;
        scope
            .intent()
            .require_full_dp_construct(volume.committed_index_construct())
            .map_err(|e| catalog_error("回收组实际Full DP检查点策略", e))?;
        if current.volume_uuid != previous.volume_uuid
            || Some(current.generation) != previous.generation.checked_add(1)
            || index_file_count(volume.index()) != expected_count
        {
            return Err(TapeError::Catalog(
                "原回收共享S4实际卷头或完整文件数偏离计划".into(),
            ));
        }
        #[cfg(feature = "hardware-fault-tests")]
        if let Some(gate) = test_gate {
            gate.after_s4(current)?;
        }
        io.verify()?;
        Ok(PendingCheckpoint {
            commit: ActiveMediaCommit {
                volume,
                batch_id,
                previous,
                current,
                files,
                physical_paths: uploads
                    .iter()
                    .zip(paths)
                    .map(|(upload, path)| (upload.intent.file_id, path))
                    .collect(),
            },
            plan,
        })
    })
}

// Group-only checkpoint policy; ordinary upload and Single keep their own
// adaptive synchronization. This helper returns the existing committed view.
fn sync_reclaim_full_dp(volume: &mut LtfsVolume<'_>) -> Result<ActiveIndexIdentity> {
    // sync requests a Full DP (incremental=false, checkpoint=false). It uses
    // the same DP/FM/barrier/VCI and failure path as ordinary adaptive batches.
    volume.sync()?;
    let current = ActiveIndexIdentity::from(volume.verified_committed_index_construct()?);
    if current.incremental || volume.index().incremental {
        return Err(TapeError::Catalog(
            "回收组检查点必须是完整Full DP索引".into(),
        ));
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{DirectoryNode, FileNode};

    use crate::ltfs::mam::Mam;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::ltfs::recovery::TailKind;
    use crate::scsi::cdb::opcode;
    use crate::scsi::reservation::fence;
    use crate::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};

    const FULL_BARCODE: &str = "FULL01L8";
    fn nonempty_target() -> (SimLibrary, SimTransport, ReservationKey) {
        let library = SimLibrary::new(1, 2, 1);
        library
            .insert_cartridge(SimCartridge::blank(FULL_BARCODE, 64 << 20), 0)
            .unwrap();
        library.load_into_drive(FULL_BARCODE, 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                block_size: 4096,
                ..Default::default()
            },
        )
        .unwrap();
        let key = ReservationKey::new(1, 17);
        fence(&device, key).unwrap();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(key));
        for ordinal in 0..32 {
            volume
                .append_bytes(&format!("kept/{ordinal:02}"), b"original body")
                .unwrap();
        }
        volume.commit().unwrap();
        assert!(volume.index().to_xml().unwrap().len() > 4 * 4096);
        drop(volume);
        (library, device, key)
    }
    fn append_two_bodies(volume: &mut LtfsVolume<'_>) {
        volume.append_bytes("new/one", b"first new body").unwrap();
        volume.append_bytes("new/two", b"second new body").unwrap();
    }
    fn assert_dp_only_s4(
        library: &SimLibrary,
        device: &SimTransport,
        before_commands: usize,
        before_ip: &[crate::scsi::sim::LogicalObject],
        previous: ActiveIndexIdentity,
        current: ActiveIndexIdentity,
    ) {
        assert_eq!(current.generation, previous.generation + 1);
        assert_eq!(current.volume_uuid, previous.volume_uuid);
        assert_eq!(
            library.cartridge(FULL_BARCODE).unwrap().partitions[0].objects,
            before_ip
        );
        let commands = device.command_log();
        assert_eq!(
            commands[before_commands..]
                .iter()
                .filter(|&&op| op == opcode::WRITE_FILEMARKS_6)
                .count(),
            3
        );
        let ip_vci = Mam::with_partition(device, 0).read_vci().unwrap().unwrap();
        let dp_vci = Mam::with_partition(device, 1).read_vci().unwrap().unwrap();
        assert!(ip_vci.generation < current.generation);
        assert_eq!(dp_vci.generation, current.generation);
        assert_eq!(dp_vci.block, current.location.start_block);
    }

    #[test]
    fn nonempty_target_adaptive_is_delta_but_group_sync_is_full_without_extra_ip_commit() {
        // A real nonempty target, valid minimum block size and unchanged tree
        // make the ordinary adaptive choice demonstrably Delta, not assumed.
        let (_control_library, control, key) = nonempty_target();
        let mut adaptive = LtfsVolume::mount(&control).unwrap();
        adaptive.set_reservation_guard(Some(key));
        append_two_bodies(&mut adaptive);
        adaptive.sync_batch().unwrap();
        assert!(adaptive.committed_index_construct().incremental);
        assert!(adaptive.index().incremental);

        let (library, device, key) = nonempty_target();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(key));
        let previous =
            ActiveIndexIdentity::from(volume.verified_committed_index_construct().unwrap());
        let before_ip = library.cartridge(FULL_BARCODE).unwrap().partitions[0]
            .objects
            .clone();
        append_two_bodies(&mut volume);
        let before_commands = device.command_log().len();
        let current = sync_reclaim_full_dp(&mut volume).unwrap();
        assert!(
            !current.incremental,
            "group checkpoint must be Full on a nonempty target"
        );
        assert!(!volume.index().incremental);
        assert_eq!(volume.index().previous_location, Some(previous.location));
        assert_eq!(volume.index().previous_incremental_location, None);
        assert_dp_only_s4(
            &library,
            &device,
            before_commands,
            &before_ip,
            previous,
            current,
        );
        drop(volume);
        library.power_cut();
        fence(&device, key).unwrap();
        let mut reopened = LtfsVolume::mount(&device).unwrap();
        reopened.set_reservation_guard(Some(key));
        assert_eq!(
            ActiveIndexIdentity::from(reopened.verified_committed_index_construct().unwrap()),
            current
        );
        assert_eq!(reopened.recovery().dp.tail, TailKind::Complete);
        assert!(reopened.recovery().ip_debt);
        assert!(!reopened.index().incremental);
        let mut bytes = Vec::new();
        reopened.read_file_to_writer("new/two", &mut bytes).unwrap();
        assert_eq!(bytes, b"second new body");
        bytes.clear();
        reopened.read_file_to_writer("kept/00", &mut bytes).unwrap();
        assert_eq!(bytes, b"original body");
    }

    #[test]
    fn delta_base_group_sync_is_full_with_original_full_and_delta_backlinks() {
        let (library, device, key) = nonempty_target();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(key));
        let full_base = volume.committed_index_construct();
        volume
            .append_bytes("delta-base", b"retained delta")
            .unwrap();
        volume.commit_incremental().unwrap();
        let delta_base = volume.committed_index_construct();
        assert!(delta_base.incremental);
        let previous = ActiveIndexIdentity::from(delta_base);
        let before_ip = library.cartridge(FULL_BARCODE).unwrap().partitions[0]
            .objects
            .clone();
        append_two_bodies(&mut volume);
        let before_commands = device.command_log().len();
        let current = sync_reclaim_full_dp(&mut volume).unwrap();
        assert!(
            !current.incremental,
            "a Delta base must still produce a Full group checkpoint"
        );
        assert!(!volume.index().incremental);
        assert_eq!(volume.index().previous_location, Some(full_base.location));
        assert_eq!(
            volume.index().previous_incremental_location,
            Some(delta_base.location)
        );
        assert_dp_only_s4(
            &library,
            &device,
            before_commands,
            &before_ip,
            previous,
            current,
        );
        drop(volume);
        library.power_cut();
        let reopened = LtfsVolume::mount(&device).unwrap();
        assert_eq!(
            ActiveIndexIdentity::from(reopened.committed_index_construct()),
            current
        );
        assert!(!reopened.index().incremental);
        let mut bytes = Vec::new();
        reopened
            .read_file_to_writer("delta-base", &mut bytes)
            .unwrap();
        assert_eq!(bytes, b"retained delta");
    }

    #[test]
    fn group_full_sync_barrier_and_vci_failure_keep_old_committed_identity_and_freeze() {
        for (opcode, after, expected_stage) in
            [(opcode::WRITE_FILEMARKS_6, 2, "barrier"), (0x8d, 0, "vci")]
        {
            let (_library, device, key) = nonempty_target();
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume.set_reservation_guard(Some(key));
            let previous = volume.committed_index_construct();
            append_two_bodies(&mut volume);
            device.inject(Fault {
                opcode,
                action: FaultAction::CheckCondition {
                    sense_key: 3,
                    asc: 0x0c,
                    ascq: 0,
                },
                after,
                remaining: 1,
            });
            let error = sync_reclaim_full_dp(&mut volume).unwrap_err();
            assert!(
                matches!(error, TapeError::CommitFailed { ref stage, .. } if stage == expected_stage)
            );
            assert_eq!(volume.committed_index_construct(), previous);
            assert!(!volume.writable());
            assert!(volume.verified_committed_index_construct().is_err());
        }
    }

    #[test]
    fn no_dirty_sync_and_adaptive_sync_preserve_head_and_issue_no_write_on_full_or_delta_base() {
        for delta_base in [false, true] {
            let (library, device, key) = nonempty_target();
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume.set_reservation_guard(Some(key));
            if delta_base {
                volume
                    .append_bytes("existing-delta", b"kept delta")
                    .unwrap();
                volume.commit_incremental().unwrap();
                assert!(volume.committed_index_construct().incremental);
            }
            let previous = volume.verified_committed_index_construct().unwrap();
            let before = library.cartridge(FULL_BARCODE).unwrap();
            let commands_before = device.command_log().len();
            volume.sync().unwrap();
            assert_eq!(
                volume.verified_committed_index_construct().unwrap(),
                previous
            );
            volume.sync_batch().unwrap();
            assert_eq!(
                volume.verified_committed_index_construct().unwrap(),
                previous
            );
            let after = library.cartridge(FULL_BARCODE).unwrap();
            assert_eq!(before.mam, after.mam);
            for (left, right) in before.partitions.iter().zip(&after.partitions) {
                assert_eq!(left.objects, right.objects);
            }
            assert!(
                !device.command_log()[commands_before..]
                    .iter()
                    .any(|op| matches!(*op, 0x0a | 0x10 | 0x8d))
            );
        }
    }

    #[test]
    fn zero_byte_bodies_still_commit_metadata_as_one_full_dp_generation() {
        let (library, device, key) = nonempty_target();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.set_reservation_guard(Some(key));
        let previous =
            ActiveIndexIdentity::from(volume.verified_committed_index_construct().unwrap());
        let before_ip = library.cartridge(FULL_BARCODE).unwrap().partitions[0]
            .objects
            .clone();
        volume.append_bytes("empty/one", b"").unwrap();
        volume.append_bytes("empty/two", b"").unwrap();
        let before_commands = device.command_log().len();
        let current = sync_reclaim_full_dp(&mut volume).unwrap();
        assert!(!current.incremental);
        assert_eq!(volume.index().find_file("empty/one").unwrap().length, 0);
        assert_eq!(volume.index().find_file("empty/two").unwrap().length, 0);
        assert_dp_only_s4(
            &library,
            &device,
            before_commands,
            &before_ip,
            previous,
            current,
        );
        drop(volume);
        library.power_cut();
        let reopened = LtfsVolume::mount(&device).unwrap();
        assert_eq!(
            ActiveIndexIdentity::from(reopened.committed_index_construct()),
            current
        );
        let mut bytes = Vec::new();
        assert_eq!(
            reopened
                .read_file_to_writer("empty/one", &mut bytes)
                .unwrap(),
            0
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn quota_includes_each_archive_and_checks_exact_boundary() {
        assert_eq!(required_file_count(3, 2, 7).unwrap(), 7);
        assert!(required_file_count(3, 2, 6).is_err());
        assert!(required_file_count(u64::MAX, 1, u64::MAX).is_err());
        assert!(required_file_count(0, 0, 1).is_err());
    }

    #[test]
    fn preflight_rejects_existing_nodes_and_cross_member_ancestors() {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "test".into(), 'b');
        index.root.files.push(FileNode {
            name: "existing".into(),
            ..Default::default()
        });
        index.root.subdirs.push(DirectoryNode {
            name: "directory".into(),
            ..Default::default()
        });
        for path in ["/existing", "/existing/child", "/directory"] {
            assert!(require_absent_paths(&index, &[path.into()], &["archive/new"]).is_err());
        }
        assert!(require_absent_paths(&index, &["new".into()], &["new/child"]).is_err());
        assert!(require_absent_paths(&index, &["/new".into()], &["new"]).is_err());
        assert!(require_absent_paths(&index, &["new".into()], &["directory/archive"]).is_ok());
    }

    #[test]
    fn existing_attempt_or_missing_members_never_authorizes_append() {
        assert!(require_fresh_attempts(&[false, false], 2).is_ok());
        assert!(require_fresh_attempts(&[false, true], 2).is_err());
        assert!(require_fresh_attempts(&[], 2).is_err());
        assert!(require_fresh_attempts(&[], 0).is_err());
    }

    #[test]
    fn nonadjacent_sorted_paths_still_reject_file_ancestor_conflicts() {
        let index = LtfsIndex::empty(Uuid::new_v4(), "test".into(), 'b');
        assert!(
            require_absent_paths(
                &index,
                &["a".into(), "a-thing".into()],
                &["a/child", "archive/second"]
            )
            .is_err()
        );
    }
}
