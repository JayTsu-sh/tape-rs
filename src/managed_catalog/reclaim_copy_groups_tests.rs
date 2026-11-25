//! Original FDB requests plus an actually captured/imported empty Sim medium.
//! No physical group writer is enabled by these orchestration tests.
use super::*;
use crate::catalog::volume_extents::ExpectedIndex;
use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{CapturedIndexSource, RecoveryBudget, capture_index_source};
use crate::ltfs::volume::read_label_only;
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;

#[path = "reclaim_index_public_gate_tests.rs"]
mod index_public_gate_tests;

struct Fixture {
    serial: String,
    route: ActiveLibraryRoute,
    spec: ReclaimSpec,
    captured: CapturedIndexSource,
    _root: std::path::PathBuf,
    _library: SimLibrary,
}
async fn fixture(c: &ManagedCatalog) -> Fixture {
    let ns = Uuid::new_v4();
    let serial = format!("SIM-COPY-GROUP-{ns}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let route = c
        .initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &replies).unwrap(),
        )
        .await
        .unwrap();
    c.claim_active_owner_epoch(&serial, 100).await.unwrap();
    let pool = PoolRecord {
        id: Uuid::new_v4(),
        library_serial: serial.clone(),
        name: "groups".into(),
        file_limit: 100,
        created_at_millis: management::now(),
        policy: PoolPolicy::default(),
        revision: 1,
    };
    c.create_active_pool(&serial, route, 100, &pool)
        .await
        .unwrap();
    let root = std::env::temp_dir().join(format!("reclaim-groups-{ns}"));
    std::fs::create_dir(&root).unwrap();
    let library = SimLibrary::new(2, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("GROUPS00L8", 64 << 20), 0)
        .unwrap();
    let changer = library.changer();
    let mut robot = crate::changer::commands::MediumChanger::new(&changer);
    let map = robot.load_address_map().unwrap().clone();
    robot.move_medium(map.storage_start, map.dt_start).unwrap();
    let device = library.drive(0);
    TapeDrive::new(&device).load().unwrap();
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "GROUPS".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    let label = read_label_only(&device).unwrap();
    let mut budget = RecoveryBudget::default();
    budget.index_scratch.directory = root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let captured = capture_index_source(&TapeDrive::new(&device), &label, &budget).unwrap();
    let expected = ExpectedIndex {
        volume_uuid: captured.construct().volume_uuid,
        generation: captured.construct().generation,
        location: captured.construct().location,
        sha256: captured.construct().sha256,
    };
    // Recovery scratch preallocates in root. It is not an evictable extent
    // cache; keep the import cache's accounting in its own owned directory.
    let cache = root.join("import-cache");
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
    ImportSpool::build_captured(&captured, &path, &mut build).unwrap();
    build.finish().unwrap();
    let spool = ImportSpool::open(&path, expected).unwrap();
    let source = NativeMediaSource {
        identity: captured.identity().unwrap(),
        files: spool.stats().files,
        paths: spool.candidate_stats().entries,
    };
    let request=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"GROUPS00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
    c.admit_management(&serial, route, 100, &request)
        .await
        .unwrap();
    let import = c
        .start_native_media(&serial, route, 100, 1, request.operation())
        .await
        .unwrap()
        .unwrap();
    c.bind_native_media_source(&import, 100, &source)
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
    c.finish_native_media(&import, 100, &source).await.unwrap();
    let assignment = c
        .tape_assignment(&serial, "GROUPS00L8")
        .await
        .unwrap()
        .unwrap();
    let spec=ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,"volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false})).unwrap();
    Fixture {
        serial,
        route,
        spec,
        captured,
        _root: root,
        _library: library,
    }
}
async fn request(c: &ManagedCatalog, f: &Fixture) -> ManagementRequest {
    let request=ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":f.spec.to_json()})).unwrap();
    c.admit_management(&f.serial, f.route, 100, &request)
        .await
        .unwrap();
    request
}
#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；原v1不迁移、fresh私有v2原子walk和丢回执恢复，无组物理写"]
async fn original_v1_and_fresh_v2_keep_request_and_walk_identity() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let f = Box::pin(fixture(&c)).await;
    let r = request(&c, &f).await;
    let v1 = c
        .start_reclaim_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    let tr = c.db.create_trx().unwrap();
    let original = tr
        .get(&v1.journal_key(), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    assert!(
        tr.get(&walk::state_key(&v1), false)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    let replay = c
        .start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.intent(), v1.intent());
    assert_eq!(replay.intent()["version"], 1);
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&v1.journal_key(), false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        original
    );
    assert!(
        tr.get(&walk::state_key(&v1), false)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    c.cancel_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap();
    let r = request(&c, &f).await;
    // A journal lost alongside durable group state is not a new operation.
    let mut orphan = domain(
        f.route.route.namespace,
        crate::managed_catalog::RECLAIM_COPY_GROUP_DOMAIN,
    );
    orphan.extend_from_slice(&r.epoch().to_be_bytes());
    orphan.extend_from_slice(r.operation().as_bytes());
    orphan.push(b'h');
    orphan.extend_from_slice(Uuid::new_v4().as_bytes());
    let tr = c.db.create_trx().unwrap();
    tr.set(&orphan, b"original-group-with-missing-parent");
    tr.commit().await.unwrap();
    assert!(
        c.start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
            .await
            .is_err()
    );
    let tr = c.db.create_trx().unwrap();
    assert!(
        tr.get(
            &parent_journal_key(f.route.route.namespace, 1, r.operation()),
            false
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        tr.get(&gate_key(f.route.route.namespace, f.spec.pool()), false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        tr.get(&orphan, false).await.unwrap().unwrap().as_ref(),
        b"original-group-with-missing-parent"
    );
    tr.clear(&orphan);
    tr.commit().await.unwrap();
    let v2 = c
        .start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v2.intent()["version"], 2);
    let tr = c.db.create_trx().unwrap();
    let saved_walk = tr
        .get(&walk::state_key(&v2), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    let saved_parent = tr
        .get(&v2.journal_key(), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    assert_eq!(
        serde_json::from_slice::<Value>(&saved_walk).unwrap()["version"],
        2
    );
    drop(tr);
    // Discard the first return value as an unknown acknowledgement; only the
    // original request identity is supplied to restore it.
    drop(v2);
    let restored = c
        .start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&restored.journal_key(), false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        saved_parent
    );
    assert_eq!(
        tr.get(&walk::state_key(&restored), false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        saved_walk
    );
    drop(tr);
    assert_eq!(
        c.management_task(&f.serial, f.route, 1, r.operation())
            .await
            .unwrap()
            .unwrap()["request"],
        *r.value()
    );
    let tr = c.db.create_trx().unwrap();
    tr.clear(&walk::state_key(&restored));
    tr.commit().await.unwrap();
    assert!(
        c.start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
            .await
            .is_err(),
        "v2缺walk不能补写或降级"
    );
    let tr = c.db.create_trx().unwrap();
    assert!(
        tr.get(&walk::state_key(&restored), false)
            .await
            .unwrap()
            .is_none()
    );
    tr.set(&walk::state_key(&restored), &saved_walk);
    tr.commit().await.unwrap();
    c.cancel_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap();
    let tr = c.db.create_trx().unwrap();
    assert!(
        tr.get(&restored.journal_key(), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tr.get(&walk::state_key(&restored), false)
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
#[ignore = "需要隔离FDB_TEST_CLUSTER；未封库存不等于空卷、孤儿group成员阻取消与清理，原任务不变"]
async fn unsealed_inventory_and_orphan_members_cannot_become_done_or_cancelled() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let f = Box::pin(fixture(&c)).await;
    let r = request(&c, &f).await;
    let parent = c
        .start_reclaim_copy_group_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap()
        .unwrap();
    let limits = ReclaimCopyGroupLimits::bounded(32, 512 << 20).unwrap();
    assert!(
        c.select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .is_err(),
        "尚未实读封存不能报告Done"
    );
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let sealed = c
        .seal_reclaim_source_inventory_page(&parent, 100, &inventory)
        .await
        .unwrap();
    assert!(sealed.complete());
    assert!(matches!(
        c.select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap(),
        ReclaimCopyGroupSelection::Done
    ));
    let orphan = key(&parent, b'm', Some(Uuid::new_v4()), Some(0));
    let tr = c.db.create_trx().unwrap();
    let original = tr
        .get(&parent.journal_key(), false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    tr.set(&orphan, b"orphan-not-a-valid-member");
    tr.commit().await.unwrap();
    assert!(c.resume_reclaim_copy_group(&parent, 100).await.is_err());
    let tr = c.db.create_trx().unwrap();
    assert!(require_no_unsettled_at(&tr, &parent).await.is_err());
    drop(tr);
    assert!(
        c.cancel_management(&f.serial, f.route, 100, 1, r.operation())
            .await
            .is_err()
    );
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        tr.get(&parent.journal_key(), false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        original
    );
    assert_eq!(
        tr.get(&orphan, false).await.unwrap().unwrap().as_ref(),
        b"orphan-not-a-valid-member"
    );
    tr.clear(&orphan);
    tr.commit().await.unwrap();
    c.cancel_management(&f.serial, f.route, 100, 1, r.operation())
        .await
        .unwrap();
}

#[path = "reclaim_copy_groups_member_tests.rs"]
mod member_tests;
