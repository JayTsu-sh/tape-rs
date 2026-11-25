//! Public mode admission and the catalog qualification boundary; no group I/O.
use super::*;
use std::os::fd::AsRawFd;

async fn setup(c: &ManagedCatalog, count: usize) -> (Fixture, ReclaimReservation) {
    Box::pin(setup_seeded(c, count, 0)).await
}
async fn setup_seeded(
    c: &ManagedCatalog,
    count: usize,
    seed: usize,
) -> (Fixture, ReclaimReservation) {
    let mut f = Box::pin(live_source(c, count)).await;
    let source = f._library.drive(0);
    let label = read_label_only(&source).unwrap();
    let mut budget = RecoveryBudget::default().with_complete_full_history();
    budget.index_scratch.directory = f._root.clone();
    budget.index_scratch.headroom_bytes = 0;
    f.captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&source),
        &label,
        &budget,
    )
    .unwrap();
    if seed != 0 {
        let target = seed_target_capture(&f, seed, &budget);
        Box::pin(import_captured(c, &f, "GROUPD00L8", &target)).await;
    }
    let mut change = f.spec.to_json();
    change["mode"] = json!("group-only");
    let value = json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change});
    let request = ManagementRequest::parse(value.clone()).unwrap();
    c.admit_management(&f.serial, f.route, 100, &request)
        .await
        .unwrap();
    let mut changed = value;
    changed["change"].as_object_mut().unwrap().remove("mode");
    let changed = ManagementRequest::parse(changed).unwrap();
    assert!(
        c.admit_management(&f.serial, f.route, 100, &changed)
            .await
            .is_err(),
        "saved mode cannot change on replay"
    );
    let public = c
        .start_reclaim_management(&f.serial, f.route, 100, 1, request.operation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        public.intent()["version"],
        2,
        "the public production starter selects v2 only from saved explicit mode"
    );
    assert!(public.spec().group_only());
    // Reuse only the existing fixed physical layout/inventory fixture setup.
    // The actual original parent above was created by the ordinary public API.
    let parent = Box::pin(parent_for_request(c, &f, &request)).await;
    assert_eq!(parent.intent(), public.intent());
    eprintln!(
        "GROUP_ONLY_PUBLIC_PARENT namespace={} operation={} serial={} protocol={} inventory_files={}",
        parent.namespace(),
        request.operation(),
        f.serial,
        parent.intent()["version"],
        count
    );
    (f, parent)
}
fn seed_target_capture(f: &Fixture, seed: usize, budget: &RecoveryBudget) -> CapturedIndexSource {
    let target = f._library.drive(1);
    let key = ReservationKey::new(1, 100);
    reservation::fence(&target, key).unwrap();
    let mut volume = LtfsVolume::mount(&target).unwrap();
    volume.set_reservation_guard(Some(key));
    for n in 0..seed {
        volume
            .append_file(&format!("/baseline-{n}"), &mut &b"seed"[..])
            .unwrap();
    }
    volume.sync().unwrap();
    drop(volume);
    let label = read_label_only(&target).unwrap();
    let target = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&target),
        &label,
        budget,
    )
    .unwrap();
    target
}

async fn candidates(
    c: &ManagedCatalog,
    f: &Fixture,
    parent: &ReclaimReservation,
    draft: &ReclaimGroupOnlyDraft,
) -> Vec<(
    source::ReclaimGroupCandidate,
    body_layouts::CopyLayoutBinding,
)> {
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let mut out = Vec::new();
    for file in draft.files() {
        let full = pool_catalog_path(parent.spec().pool(), file.path().as_bytes()).unwrap();
        let read = c
            .resolve_active_read_source_for_owner(&f.serial, &full, f.route, 100)
            .await
            .unwrap()
            .unwrap();
        let metadata = read.metadata();
        let binding = read.layout_binding().unwrap().clone();
        let budget = ReadStorageBudget {
            index_bytes: 64 << 20,
            cache_bytes: 16 << 20,
            download_bytes: 16 << 20,
            headroom_bytes: 0,
        };
        let tmp =
            crate::local_read_storage::download_file(&f._root, binding.encoded_bytes(), budget)
                .unwrap();
        let physical = read.prepare_body_plan(c, tmp, 1 << 20).await.unwrap();
        out.push((
            inventory
                .reclaim_group_candidate(parent, file, &metadata, &physical)
                .unwrap()
                .unwrap(),
            binding,
        ));
    }
    out
}
fn commands(f: &Fixture) -> [usize; 2] {
    [0, 1].map(|n| f._library.drive(n).command_log().len())
}

#[tokio::test]
#[ignore = "固定private FDB；公开group-only父准入、资格全集只读、Ready固定身份/缺seal/预算/版本反例，无组IO"]
async fn group_only_public_mode_seals_complete_readonly_plan_and_blocks_missing_seal() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let (f, parent) = Box::pin(setup(&c, 2)).await;
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let limits = ReclaimCopyGroupLimits::bounded(32, 1024).unwrap();
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        record_at(&tr, &parent, 100).await.unwrap()["owner_epoch"],
        100
    );
    source_state_at(&tr, &parent).await.unwrap();
    let layout = layout::at(&tr, &parent).await.unwrap().unwrap();
    let (_, index) = active_volume::observed_at(&tr, parent.serial(), layout.destination.volume)
        .await
        .unwrap();
    let count = active_volume::file_count_at(&tr, parent.serial(), layout.destination.volume)
        .await
        .unwrap();
    let stored = active_volume::stored_file_count_at(&tr, index)
        .await
        .unwrap();
    let slots = remaining_slots_at(&tr, &parent, layout.destination.volume)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(stored.is_none());
    assert_eq!(slots, 50);
    eprintln!(
        "GROUP_ONLY_COUNT_PRECONDITION namespace={} stage=before_prepare native_count={} stored_count={:?} slots={} source_state_valid=true",
        parent.namespace(),
        count,
        stored,
        slots
    );
    drop(tr);
    let before_commands = commands(&f);
    let (_, before) = reclaim_rows(&c, &parent).await;
    let ReclaimGroupOnlyQualification::NeedsCandidates(draft) = c
        .prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
        .await
        .unwrap()
    else {
        panic!("two ordinary managed files must be eligible");
    };
    assert_eq!(draft.files().count(), 2);
    let planned = draft.files().cloned().collect::<Vec<_>>();
    let ReclaimGroupOnlyQualification::NeedsCandidates(replay) = c
        .prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(planned, replay.files().cloned().collect::<Vec<_>>());
    assert_eq!(
        before,
        reclaim_rows(&c, &parent).await.1,
        "qualification enumeration cannot move a cursor or write any original row"
    );
    let candidates = Box::pin(candidates(&c, &f, &parent, &draft)).await;
    assert!(matches!(
        c.seal_reclaim_group_only(&parent, 100, &draft, &candidates)
            .await
            .unwrap(),
        ReclaimGroupOnlyQualification::Ready
    ));
    let (_, ready) = reclaim_rows(&c, &parent).await;
    assert_eq!(
        ready.len(),
        before.len() + 1,
        "Ready is one bounded new immutable row"
    );
    for (key, value) in &before {
        assert_eq!(ready.get(key), Some(value));
    }
    assert!(matches!(
        c.seal_reclaim_group_only(&parent, 100, &draft, &candidates)
            .await
            .unwrap(),
        ReclaimGroupOnlyQualification::Ready
    ));
    let mut reversed = candidates.clone();
    reversed.reverse();
    assert!(
        c.seal_reclaim_group_only(&parent, 100, &draft, &reversed)
            .await
            .is_err(),
        "a replay cannot replace original ordered members"
    );
    assert_eq!(ready, reclaim_rows(&c, &parent).await.1);
    c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
        .await
        .unwrap();
    assert!(
        c.require_reclaim_group_only_ready(&parent, 100, 1023, 1 << 20)
            .await
            .is_err()
    );
    assert_eq!(
        commands(&f),
        before_commands,
        "pure qualification and layout proof preparation must not issue any device command"
    );
    // An admitted metadata plan alone cannot replace a missing Ready seal.
    let target = f._library.drive(1);
    let label = read_label_only(&target).unwrap();
    let mut budget = RecoveryBudget::default().with_complete_full_history();
    budget.index_scratch.directory = f._root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let target = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&target),
        &label,
        &budget,
    )
    .unwrap();
    let after_capture_commands = commands(&f);
    let root = c
        .reclaim_source_nodes(
            &parent,
            100,
            &inventory,
            &[f.captured.header().root.meta.file_uid],
        )
        .await
        .unwrap();
    let plan = inventory
        .plan_reclaimed_node(
            &parent,
            &root[0],
            &target,
            &format!(
                "_reclaim_{}",
                parent.intent()["operation"].as_str().unwrap()
            ),
        )
        .unwrap();
    let (seal_key, seal_bytes) = ready
        .iter()
        .find(|(k, _)| !before.contains_key(*k))
        .unwrap();
    let tr = c.db.create_trx().unwrap();
    tr.clear(seal_key);
    tr.commit().await.unwrap();
    let missing = reclaim_rows(&c, &parent).await.1;
    assert!(
        c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
            .await
            .is_err()
    );
    assert!(
        c.prepare_reclaim_metadata_node(&parent, 100, &plan)
            .await
            .is_err()
    );
    assert!(
        c.select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .is_err()
    );
    assert_eq!(missing, reclaim_rows(&c, &parent).await.1);
    let tr = c.db.create_trx().unwrap();
    tr.set(seal_key, seal_bytes);
    tr.commit().await.unwrap();
    // A head mutation after Ready must be caught before root metadata admission.
    let head_key = head_key(parent.namespace(), planned[0].file_id());
    let tr = c.db.create_trx().unwrap();
    let old = tr.get(&head_key, false).await.unwrap().unwrap().to_vec();
    let mut head = decode_head(&old).unwrap();
    head.version += 1;
    tr.set(&head_key, &encode_head(&head));
    tr.commit().await.unwrap();
    assert!(
        c.prepare_reclaim_metadata_node(&parent, 100, &plan)
            .await
            .is_err()
    );
    let tr = c.db.create_trx().unwrap();
    tr.set(&head_key, &old);
    tr.commit().await.unwrap();
    c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
        .await
        .unwrap();
    // The walk consumes original planned operations, rather than generating new ones.
    loop {
        match c
            .select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap()
        {
            ReclaimCopyGroupSelection::Scanning => {}
            ReclaimCopyGroupSelection::NeedsCandidate(file) => {
                assert_eq!(file, planned[0]);
                break;
            }
            _ => panic!("first bounded planned member expected"),
        }
    }
    assert_eq!(
        commands(&f),
        after_capture_commands,
        "metadata/cursor gates cannot issue CDBs"
    );
    eprintln!(
        "GROUP_ONLY_READY namespace={} planned_files={} mode=group-only public_parent=2 root_guard=true missing_seal_guard=true",
        parent.namespace(),
        planned.len()
    );
}

#[tokio::test]
#[ignore = "固定private FDB；完整输入超单组预算持久Unsupported，重复及接管不得新cursor/元数据/格式化，无组IO"]
async fn group_only_unsupported_is_persistent_and_preserves_original_request() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let (f, parent) = Box::pin(setup(&c, 2)).await;
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let limits = ReclaimCopyGroupLimits::bounded(1, 1024).unwrap();
    let before_commands = commands(&f);
    let (_, before) = reclaim_rows(&c, &parent).await;
    let ReclaimGroupOnlyQualification::Unsupported(reason) = c
        .prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
        .await
        .unwrap()
    else {
        panic!("the complete two-file input cannot be split into a one-member plan");
    };
    let saved = reclaim_rows(&c, &parent).await.1;
    assert_eq!(saved.len(), before.len() + 1);
    for (key, value) in &before {
        assert_eq!(saved.get(key), Some(value));
    }
    assert_eq!(
        c.reclaim_group_only_status(&parent, 100).await.unwrap(),
        Some(reason.clone())
    );
    assert!(
        c.select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .is_err()
    );
    assert!(
        c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
            .await
            .is_err()
    );
    assert!(
        c.reclaim_group_only_precopy_ready(&parent, 100)
            .await
            .is_err()
    );
    assert!(matches!(
        c.prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
            .await
            .unwrap(),
        ReclaimGroupOnlyQualification::Unsupported(_)
    ));
    assert_eq!(saved, reclaim_rows(&c, &parent).await.1);
    c.claim_active_owner_epoch(&f.serial, 101).await.unwrap();
    let operation = Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap();
    let adopted = c
        .start_reclaim_management(&f.serial, f.route, 101, 1, operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(adopted.intent(), parent.intent());
    let after_adoption = reclaim_rows(&c, &adopted).await.1;
    assert_eq!(
        c.reclaim_group_only_status(&adopted, 101).await.unwrap(),
        Some(reason)
    );
    assert!(
        c.select_reclaim_copy_group(&adopted, 101, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .is_err()
    );
    assert_eq!(after_adoption, reclaim_rows(&c, &adopted).await.1);
    assert_eq!(commands(&f), before_commands);
    eprintln!(
        "GROUP_ONLY_UNSUPPORTED namespace={} original_parent_retained=true takeover=101 cdb_delta=0",
        parent.namespace()
    );
}

#[tokio::test]
#[ignore = "固定private FDB；真实3file native-import目标无S4counter时资格使用权威count，缺baseline proof拒绝，不写组正文"]
async fn group_only_nonzero_import_baseline_and_missing_evidence_are_strict() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let (f, parent) = Box::pin(setup_seeded(&c, 2, 3)).await;
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let limits = ReclaimCopyGroupLimits::bounded(32, 1024).unwrap();
    let tr = c.db.create_trx().unwrap();
    let layout = layout::at(&tr, &parent).await.unwrap().unwrap();
    let (scan, index) = active_volume::observed_at(&tr, parent.serial(), layout.destination.volume)
        .await
        .unwrap();
    assert_eq!(
        active_volume::file_count_at(&tr, parent.serial(), layout.destination.volume)
            .await
            .unwrap(),
        3
    );
    assert!(
        active_volume::stored_file_count_at(&tr, index)
            .await
            .unwrap()
            .is_none()
    );
    let proof_key = source_key(layout.destination.volume, scan.id);
    let proof = tr.get(&proof_key, false).await.unwrap().unwrap().to_vec();
    tr.clear(&proof_key);
    tr.commit().await.unwrap();
    let before_commands = commands(&f);
    let (_, before) = reclaim_rows(&c, &parent).await;
    assert!(
        c.prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
            .await
            .is_err(),
        "missing baseline proof must never become zero files"
    );
    assert_eq!(before, reclaim_rows(&c, &parent).await.1);
    let tr = c.db.create_trx().unwrap();
    tr.set(&proof_key, &proof);
    tr.commit().await.unwrap();
    let ReclaimGroupOnlyQualification::NeedsCandidates(draft) = c
        .prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
        .await
        .unwrap()
    else {
        panic!("verified nonzero baseline should qualify");
    };
    let candidates = Box::pin(candidates(&c, &f, &parent, &draft)).await;
    assert!(matches!(
        c.seal_reclaim_group_only(&parent, 100, &draft, &candidates)
            .await
            .unwrap(),
        ReclaimGroupOnlyQualification::Ready
    ));
    c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
        .await
        .unwrap();
    let after = reclaim_rows(&c, &parent).await.1;
    let (_, row) = after
        .iter()
        .find(|(key, _)| !before.contains_key(*key))
        .unwrap();
    let row: Value = serde_json::from_slice(&row[..row.len() - 32]).unwrap();
    assert_eq!(row["decision"]["plan"]["target_files"], 3);
    assert_eq!(commands(&f), before_commands);
    eprintln!(
        "GROUP_ONLY_NONZERO_BASELINE namespace={} target_count=3 stored_counter_absent=true missing_proof_rejected=true cdb_delta=0",
        parent.namespace()
    );
}

#[tokio::test]
#[ignore = "固定private FDB；公开无mode请求及其重放必须保持旧v1，不进入group-only资格"]
async fn ordinary_public_reclaim_without_mode_preserves_v1_and_replay() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let f = Box::pin(live_source(&c, 1)).await;
    let request = request(&c, &f).await;
    let before = commands(&f);
    let original = c
        .start_reclaim_management(&f.serial, f.route, 100, 1, request.operation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.intent()["version"], 1);
    assert!(!original.spec().group_only());
    let replay = c
        .start_reclaim_management(&f.serial, f.route, 100, 1, request.operation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.intent(), replay.intent());
    assert!(
        c.reclaim_group_only_precopy_ready(&original, 100)
            .await
            .unwrap()
    );
    assert_eq!(
        c.reclaim_group_only_status(&original, 100).await.unwrap(),
        None
    );
    assert_eq!(commands(&f), before);
    eprintln!(
        "GROUP_ONLY_DEFAULT_COMPAT namespace={} operation={} parent_version=1 replay_original=true cdb_delta=0",
        original.namespace(),
        request.operation()
    );
}

#[path = "reclaim_group_only_fixed_plan_tests.rs"]
mod fixed_plan;
