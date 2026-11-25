//! A Ready plan must never turn into Single or a proper prefix of its group.
use super::*;
type Candidates = Vec<(
    source::ReclaimGroupCandidate,
    body_layouts::CopyLayoutBinding,
)>;
async fn qualified(
    c: &ManagedCatalog,
) -> (
    Fixture,
    ReclaimReservation,
    ReclaimCopyGroupLimits,
    Candidates,
) {
    let (f, parent) = Box::pin(setup(c, 2)).await;
    let inventory = ReclaimSourceInventory::from_captured(&f.captured).unwrap();
    let limits = ReclaimCopyGroupLimits::bounded(32, 1024).unwrap();
    let ReclaimGroupOnlyQualification::NeedsCandidates(draft) = c
        .prepare_reclaim_group_only(&parent, 100, &inventory, limits, 1 << 20)
        .await
        .unwrap()
    else {
        panic!("expected complete draft")
    };
    let candidates = Box::pin(candidates(c, &f, &parent, &draft)).await;
    assert!(matches!(
        c.seal_reclaim_group_only(&parent, 100, &draft, &candidates)
            .await
            .unwrap(),
        ReclaimGroupOnlyQualification::Ready
    ));
    c.require_reclaim_group_only_ready(&parent, 100, 1024, 1 << 20)
        .await
        .unwrap();
    (f, parent, limits, candidates)
}
async fn next(
    c: &ManagedCatalog,
    parent: &ReclaimReservation,
    limits: ReclaimCopyGroupLimits,
) -> ReclaimFile {
    for _ in 0..32 {
        match c
            .select_reclaim_copy_group(parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap()
        {
            ReclaimCopyGroupSelection::Scanning => {}
            ReclaimCopyGroupSelection::NeedsCandidate(file) => return file,
            _ => panic!("original planned candidate expected"),
        }
    }
    panic!("bounded walk did not produce a candidate")
}
async fn rejected_choice(partial: bool, smaller_budget: bool) {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let (f, parent, limits, candidates) = Box::pin(qualified(&c)).await;
    let mut file = Box::pin(next(&c, &parent, limits)).await;
    assert_eq!(&file, candidates[0].0.file());
    if partial {
        let ReclaimCopyGroupSelection::Group(group) = c
            .select_reclaim_copy_group(
                &parent,
                100,
                limits,
                ReclaimCopyGroupChoice::Candidate(&candidates[0].0),
            )
            .await
            .unwrap()
        else {
            panic!("first member must be selected")
        };
        assert_eq!(group.header.count, 1);
        assert!(!group.header.selected);
        assert_eq!(group.header.phase, Phase::Selecting);
        file = Box::pin(next(&c, &parent, limits)).await;
        assert_eq!(&file, candidates[1].0.file());
    } else {
        assert!(
            load_at(&c.db.create_trx().unwrap(), &parent, 100)
                .await
                .unwrap()
                .is_none()
        );
    }
    let before_commands = commands(&f);
    let (before_rv, before) = reclaim_rows(&c, &parent).await;
    let result = if smaller_budget {
        c.select_reclaim_copy_group(
            &parent,
            100,
            ReclaimCopyGroupLimits::bounded(1, 1024).unwrap(),
            ReclaimCopyGroupChoice::Candidate(&candidates[1].0),
        )
        .await
    } else {
        c.select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Single(&file))
            .await
    };
    let (after_rv, after) = reclaim_rows(&c, &parent).await;
    eprintln!(
        "GROUP_ONLY_FIXED_PLAN namespace={} operation={} partial={} smaller_budget={} before_rv={} after_rv={} rejected={} rows_unchanged={} before_rows={} after_rows={} cdb_delta={:?}",
        parent.namespace(),
        parent.intent()["operation"],
        partial,
        smaller_budget,
        before_rv,
        after_rv,
        result.is_err(),
        before == after,
        before.len(),
        after.len(),
        [
            commands(&f)[0] - before_commands[0],
            commands(&f)[1] - before_commands[1]
        ]
    );
    assert!(
        result.is_err(),
        "Ready must reject explicit Single and member-budget prefix selection"
    );
    assert_eq!(
        before, after,
        "original parent, Ready, group/header, Single marker and walk must remain byte-for-byte unchanged"
    );
    assert_eq!(commands(&f), before_commands);
}
#[tokio::test]
#[ignore = "固定private FDB；Ready未选组时Single必须拒绝且完整原行不变"]
async fn ready_without_group_rejects_single_without_persistent_change() {
    Box::pin(rejected_choice(false, false)).await;
}
#[tokio::test]
#[ignore = "固定private FDB；Ready半组选Single必须拒绝且selected/header/walk不变"]
async fn ready_partial_group_rejects_single_without_persistent_change() {
    Box::pin(rejected_choice(true, false)).await;
}
#[tokio::test]
#[ignore = "固定private FDB；Ready半组缩成员预算不能提前封组，完整原行不变"]
async fn ready_partial_group_rejects_smaller_member_budget_without_persistent_change() {
    Box::pin(rejected_choice(true, true)).await;
}
#[tokio::test]
#[ignore = "固定private FDB；旧内部非group-only v2 Single选择行为保持"]
async fn legacy_mode_single_selection_keeps_original_behavior() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let f = Box::pin(live_source(&c, 2)).await;
    let parent = Box::pin(parent(&c, &f)).await;
    assert!(!parent.spec().group_only());
    let limits = ReclaimCopyGroupLimits::bounded(32, 1024).unwrap();
    let file = Box::pin(next(&c, &parent, limits)).await;
    let before = commands(&f);
    let ReclaimCopyGroupSelection::Single(actual) = c
        .select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Single(&file))
        .await
        .unwrap()
    else {
        panic!("ordinary legacy Single behavior must remain")
    };
    assert_eq!(actual, file);
    assert!(
        c.db.create_trx()
            .unwrap()
            .get(&key(&parent, b's', Some(file.operation()), None), false)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(commands(&f), before);
}

#[tokio::test]
#[ignore = "固定private FDB；显式freeze同样不能封存Ready的部分组"]
async fn ready_partial_group_cannot_be_frozen_directly() {
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    let (f, parent, limits, candidates) = Box::pin(qualified(&c)).await;
    let file = Box::pin(next(&c, &parent, limits)).await;
    assert_eq!(&file, candidates[0].0.file());
    let ReclaimCopyGroupSelection::Group(group) = c
        .select_reclaim_copy_group(
            &parent,
            100,
            limits,
            ReclaimCopyGroupChoice::Candidate(&candidates[0].0),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(group.header.count, 1);
    assert!(!group.header.selected);
    let before = reclaim_rows(&c, &parent).await.1;
    let before_commands = commands(&f);
    assert!(
        c.freeze_reclaim_copy_group(&parent, 100, group.id())
            .await
            .is_err()
    );
    assert_eq!(before, reclaim_rows(&c, &parent).await.1);
    assert_eq!(commands(&f), before_commands);
}
