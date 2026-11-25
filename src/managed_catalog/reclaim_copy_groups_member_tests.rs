//! Ordinary SDK uploads have media=None; imported records are not forged into
//! this capability. Actual source body reads precede original proof binding.
use super::*;
use crate::changer::commands::MediumChanger;
use crate::daemon::fdb_media::{
    ActiveMediaUpload, commit_active_media_batch, publish_active_media_batch,
};
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::reservation::{self, ReservationKey};

async fn live_source(c: &ManagedCatalog, count: usize) -> Fixture {
    let mut f = Box::pin(fixture(c)).await;
    let source = f._library.drive(0);
    let key = ReservationKey::new(1, 100);
    reservation::fence(&source, key).unwrap();
    let mut volume = LtfsVolume::mount(&source).unwrap();
    volume.set_reservation_guard(Some(key));
    let mut uploads = Vec::new();
    for at in 0..count {
        let path = format!("/file{at:02}");
        let body = b"body";
        let spec = ActiveUploadSpec {
            pool_id: f.spec.pool(),
            path: path.into_bytes(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: 4,
            digest: ContentDigest::Sha256(Sha256::digest(body).into()),
        };
        let intent = c
            .prepare_active_upload(&f.serial, f.route, 100, f.spec.barcode(), &spec)
            .await
            .unwrap();
        let spool = f._root.join(format!("source-{at}.bin"));
        std::fs::write(&spool, body).unwrap();
        uploads.push(ActiveMediaUpload { intent, spool });
    }
    let committed = Box::pin(commit_active_media_batch(
        c,
        volume,
        Uuid::new_v4(),
        &uploads,
        16 << 20,
    ))
    .await
    .unwrap();
    let committed = Box::pin(publish_active_media_batch(c, committed))
        .await
        .unwrap();
    drop(committed.into_volume());
    let label = read_label_only(&source).unwrap();
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = f._root.clone();
    budget.index_scratch.headroom_bytes = 0;
    f.captured = capture_index_source(&TapeDrive::new(&source), &label, &budget).unwrap();
    // Add the genuinely blank, separately assigned fixed destination.
    f._library
        .insert_cartridge(SimCartridge::blank("GROUPD00L8", 64 << 20), 1)
        .unwrap();
    let changer = f._library.changer();
    let mut robot = MediumChanger::new(&changer);
    let map = robot.load_address_map().unwrap().clone();
    robot
        .move_medium(map.storage_start + 1, map.dt_start + 1)
        .unwrap();
    let target = f._library.drive(1);
    TapeDrive::new(&target).load().unwrap();
    mkltfs(
        &target,
        &MkltfsOptions {
            volume_id: "GROUPD".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let label = read_label_only(&target).unwrap();
    let captured = capture_index_source(&TapeDrive::new(&target), &label, &budget).unwrap();
    import_captured(c, &f, "GROUPD00L8", &captured).await;
    f
}
async fn import_captured(
    c: &ManagedCatalog,
    f: &Fixture,
    barcode: &str,
    captured: &CapturedIndexSource,
) {
    let expected = ExpectedIndex {
        volume_uuid: captured.construct().volume_uuid,
        generation: captured.construct().generation,
        location: captured.construct().location,
        sha256: captured.construct().sha256,
    };
    let cache = f
        ._root
        .join(format!("import-cache-{barcode}-{}", Uuid::new_v4()));
    let mut build = CacheBuild::new(
        &cache,
        ReadStorageBudget {
            index_bytes: 64 << 20,
            cache_bytes: 16 << 20,
            download_bytes: 16 << 20,
            headroom_bytes: 0,
        },
    )
    .unwrap();
    let path = cache.join("import.sqlite");
    ImportSpool::build_captured(captured, &path, &mut build).unwrap();
    build.finish().unwrap();
    let spool = ImportSpool::open(&path, expected).unwrap();
    let proof = NativeMediaSource {
        identity: captured.identity().unwrap(),
        files: spool.stats().files,
        paths: spool.candidate_stats().entries,
    };
    let request=ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":f.spec.pool().to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}})).unwrap();
    c.admit_management(&f.serial, f.route, 100, &request)
        .await
        .unwrap();
    let import = c
        .start_native_media(&f.serial, f.route, 100, 1, request.operation())
        .await
        .unwrap()
        .unwrap();
    c.bind_native_media_source(&import, 100, &proof)
        .await
        .unwrap();
    c.begin_native_media_projection(&import, 100).await.unwrap();
    let mut after = None;
    loop {
        let page = spool.candidate_page(after.as_deref(), 8).unwrap();
        for candidate in page.candidates {
            let entry = import.entry(&spool, candidate).unwrap();
            c.project_native_media_entry(&import, 100, &entry, true)
                .await
                .unwrap();
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    c.finish_native_media(&import, 100, &proof).await.unwrap();
}
async fn parent(c: &ManagedCatalog, f: &Fixture) -> ReclaimReservation {
    let r = request(c, f).await;
    parent_for_request(c, f, &r).await
}
async fn parent_for_request(
    c: &ManagedCatalog,
    f: &Fixture,
    r: &ManagementRequest,
) -> ReclaimReservation {
    let parent = c
        .start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    let changer = f._library.changer();
    let mut robot = MediumChanger::new(&changer);
    let map = robot.load_address_map().unwrap().clone();
    let rows = robot.read_all_status().unwrap();
    let mut binding = Vec::new();
    for (at, barcode) in [(0, "GROUPS00L8"), (1, "GROUPD00L8")] {
        let a = c
            .tape_assignment(&f.serial, barcode)
            .await
            .unwrap()
            .unwrap();
        let drive = rows
            .iter()
            .find(|r| r.address == map.dt_start + at)
            .unwrap();
        binding.push(ReclaimDriveBinding {
            barcode: barcode.into(),
            volume: a.volume_id.unwrap(),
            revision: a.revision,
            drive_serial: drive.drive_serial().unwrap().into(),
            address: drive.address,
            home: map.storage_start + at,
        });
    }
    let proof = ReclaimLayoutEvidence::from_read(
        &parent,
        100,
        binding.remove(0),
        binding.remove(0),
        &map,
        &rows,
    )
    .unwrap();
    c.prepare_reclaim_layout(&parent, 100, &proof)
        .await
        .unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    loop {
        if c.seal_reclaim_source_inventory_page(&parent, 100, &inventory)
            .await
            .unwrap()
            .complete()
        {
            break;
        }
    }
    parent
}
async fn candidate(
    c: &ManagedCatalog,
    f: &Fixture,
    parent: &ReclaimReservation,
    file: &ReclaimFile,
) -> (source::ReclaimGroupCandidate, ReclaimSourceRead) {
    let path = pool_catalog_path(f.spec.pool(), file.path().as_bytes()).unwrap();
    let selected = c
        .resolve_active_read_source_for_owner(&f.serial, &path, f.route, 100)
        .await
        .unwrap()
        .unwrap();
    let metadata = selected.metadata();
    assert!(
        metadata.copy.media.is_none(),
        "真实普通SDK R，不伪造Imported字段"
    );
    let budget = ReadStorageBudget {
        index_bytes: 64 << 20,
        cache_bytes: 16 << 20,
        download_bytes: 16 << 20,
        headroom_bytes: 0,
    };
    let plan_file = crate::local_read_storage::download_file(&f._root, 1 << 20, budget).unwrap();
    let physical = selected
        .prepare_body_plan(c, plan_file, 1 << 20)
        .await
        .unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let candidate = inventory
        .reclaim_group_candidate(parent, file, &metadata, &physical)
        .unwrap()
        .expect("当前SDK单调extent应有候选能力");
    let device = f._library.drive(0);
    let volume = LtfsVolume::mount(&device).unwrap();
    let mut body = Vec::new();
    volume
        .read_file_to_writer(&format!("/{}", physical.path()), &mut body)
        .unwrap();
    assert_eq!(body, b"body");
    let proof = inventory
        .bind_verified_read_plan(
            parent,
            file,
            &metadata,
            &physical,
            f.captured.construct().into(),
            Sha256::digest(&body).into(),
            None,
        )
        .unwrap();
    (candidate, proof)
}
async fn selected(
    c: &ManagedCatalog,
    f: &Fixture,
    parent: &ReclaimReservation,
    limit: u8,
) -> (ReclaimCopyGroup, Vec<ReclaimSourceRead>) {
    let limits = ReclaimCopyGroupLimits::bounded(limit, 512 << 20).unwrap();
    let mut proofs = Vec::new();
    loop {
        match c
            .select_reclaim_copy_group(parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap()
        {
            ReclaimCopyGroupSelection::NeedsCandidate(file) => {
                let (candidate, proof) = candidate(c, f, parent, &file).await;
                proofs.push(proof);
                let result = c
                    .select_reclaim_copy_group(
                        parent,
                        100,
                        limits,
                        ReclaimCopyGroupChoice::Candidate(&candidate),
                    )
                    .await
                    .unwrap();
                if let ReclaimCopyGroupSelection::Group(group) = result {
                    if group.header.selected {
                        return (group, proofs);
                    }
                } else {
                    panic!("候选必须固定到原组");
                }
            }
            ReclaimCopyGroupSelection::Group(group) if group.header.selected => {
                return (group, proofs);
            }
            ReclaimCopyGroupSelection::Group(_) | ReclaimCopyGroupSelection::Scanning => {}
            _ => panic!("小普通live源不应fallback或Done"),
        }
    }
}
#[tokio::test]
#[ignore = "隔离FDB_TEST_CLUSTER；实际SDK live源+固定2member/原proof准备/旧lane拒/unknownadmit，不执行组物理写"]
async fn fixed_members_and_unknown_admission_keep_original_batch_and_children() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let f = Box::pin(live_source(&c, 2)).await;
    let parent = Box::pin(parent(&c, &f)).await;
    let (group, proofs) = Box::pin(selected(&c, &f, &parent, 2)).await;
    assert_eq!(group.members.len(), 2);
    let id = group.id();
    let original_members = group.header.members;
    assert!(
        c.seal_reclaim_copy_group(&parent, 100, id).await.is_err(),
        "缺原子请求不能Prepared"
    );
    let mut requests = Vec::new();
    for (at, member) in group.members.iter().enumerate() {
        let spec = ActiveUploadSpec {
            pool_id: f.spec.pool(),
            path: member.file.path().as_bytes().to_vec(),
            operation_id: member.file.operation(),
            expected_version: Some(member.file.version()),
            size: member.file.size(),
            digest: ContentDigest::Sha256(member.known_sha256),
        };
        let first = c
            .prepare_reclaim_group_member(&parent, 100, id, at as u8, &spec, &proofs[at])
            .await
            .unwrap();
        let replay = c
            .prepare_reclaim_group_member(&parent, 100, id, at as u8, &spec, &proofs[at])
            .await
            .unwrap();
        assert_eq!(first.request(), replay.request());
        requests.push(first.request());
    }
    let prepared = c.seal_reclaim_copy_group(&parent, 100, id).await.unwrap();
    assert_eq!(prepared.header.members, original_members);
    assert_eq!(prepared.header.phase, Phase::Prepared);
    let before = f._library.drive(1).command_log().len();
    for members in [&requests[..1], &requests[..]] {
        let tr = c.db.create_trx().unwrap();
        assert!(
            children::require_write_group_lane_at(&tr, parent.namespace(), &f.serial, 100, members)
                .await
                .is_err(),
            "原strict-intent1旧lane拒真正2member"
        );
    }
    let previous =
        active_volume::decode_index(group.header.destination.volume, &group.header.previous)
            .unwrap();
    let first = c
        .admit_reclaim_copy_group(&parent, 100, id, previous)
        .await
        .unwrap();
    assert!(!first.needs_observation());
    assert_eq!(first.group().batch_id(), id);
    drop(first);
    let replay = c
        .admit_reclaim_copy_group(&parent, 100, id, previous)
        .await
        .unwrap();
    assert!(
        replay.needs_observation(),
        "丢首次返回后，Q全缺也先按原batch观察"
    );
    assert_eq!(replay.group().id(), id);
    assert_eq!(replay.group().header.members, original_members);
    assert_eq!(
        replay
            .group()
            .members
            .iter()
            .map(|m| m.child.as_ref().unwrap().operation.clone())
            .collect::<Vec<_>>(),
        requests
            .iter()
            .map(|r| r.operation_id.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        f._library.drive(1).command_log().len(),
        before,
        "所有组编排没有目标CDB"
    );
    let tr = c.db.create_trx().unwrap();
    assert!(require_no_unsettled_at(&tr, &parent).await.is_err());
}

#[path = "reclaim_group_quiescence_tests.rs"]
mod quiescence_tests;

#[path = "reclaim_group_quota_tests.rs"]
mod quota_tests;

#[path = "reclaim_group_owner_abort_tests.rs"]
mod owner_abort_tests;
