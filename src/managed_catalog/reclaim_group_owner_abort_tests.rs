//! Selecting owner adoption must abort with a later invalid candidate.
use super::*;

fn pinned_private_cluster() -> std::fs::File {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut original = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(std::env::var("FDB_TEST_CLUSTER").unwrap())
        .unwrap();
    let mut bytes = Vec::new();
    Read::take(&mut original, 4096)
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(
        bytes,
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\n"
    );
    let name = std::ffi::CString::new("quota-private-fdb").unwrap();
    let fd = nix::sys::memfd::memfd_create(
        name.as_c_str(),
        nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC
            | nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
    )
    .unwrap();
    let mut sealed = std::fs::File::from(fd);
    sealed.write_all(&bytes).unwrap();
    nix::fcntl::fcntl(
        sealed.as_raw_fd(),
        nix::fcntl::FcntlArg::F_ADD_SEALS(
            nix::fcntl::SealFlag::F_SEAL_WRITE
                | nix::fcntl::SealFlag::F_SEAL_GROW
                | nix::fcntl::SealFlag::F_SEAL_SHRINK
                | nix::fcntl::SealFlag::F_SEAL_SEAL,
        ),
    )
    .unwrap();
    sealed
}

async fn reclaim_rows(
    c: &ManagedCatalog,
    parent: &ReclaimReservation,
) -> (i64, std::collections::BTreeMap<Vec<u8>, Vec<u8>>) {
    let tr = c.db.create_trx().unwrap();
    let version = tr.get_read_version().await.unwrap();
    // Every selector mutation is in this namespace's R domain, including
    // original group control/history, source inventory, parent and walk.
    let rows = read_page(
        &tr,
        &management::prefix(parent.namespace(), b'R'),
        None,
        2048,
        false,
    )
    .await
    .unwrap();
    assert!(
        rows.next.is_none(),
        "the complete bounded reclaim domain must fit"
    );
    let mut rows = rows
        .items
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let pool = control::pool_key(parent.namespace(), parent.spec().pool());
    if let Some(raw) = tr.get(&pool, false).await.unwrap() {
        rows.insert(pool, raw.to_vec());
    }
    (version, rows)
}

#[tokio::test]
#[ignore = "固定private FDB；Selecting错误Candidate在owner推进后的domain拒绝保持所有原行；不执行组IO"]
async fn selecting_owner_then_invalid_candidate_preserves_all_original_rows() {
    use std::os::fd::AsRawFd;
    let cluster = pinned_private_cluster();
    let cluster_path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster_path)).unwrap();
    let mut f = Box::pin(live_source(&c, 2)).await;
    let device = f._library.drive(0);
    let label = read_label_only(&device).unwrap();
    let mut budget = RecoveryBudget::default().with_complete_full_history();
    budget.index_scratch.directory = f._root.clone();
    budget.index_scratch.headroom_bytes = 0;
    f.captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&device),
        &label,
        &budget,
    )
    .unwrap();
    let parent = Box::pin(parent(&c, &f)).await;
    let limits = ReclaimCopyGroupLimits::bounded(32, 512 << 20).unwrap();
    let first = loop {
        match c
            .select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap()
        {
            ReclaimCopyGroupSelection::NeedsCandidate(file) => break file,
            ReclaimCopyGroupSelection::Scanning => {}
            _ => panic!("expected first ordinary managed file"),
        }
    };
    let (original_candidate, _) = Box::pin(candidate(&c, &f, &parent, &first)).await;
    let ReclaimCopyGroupSelection::Group(group) = c
        .select_reclaim_copy_group(
            &parent,
            100,
            limits,
            ReclaimCopyGroupChoice::Candidate(&original_candidate),
        )
        .await
        .unwrap()
    else {
        panic!("the first file must create its original group")
    };
    assert_eq!(group.header.phase, Phase::Selecting);
    assert_eq!(group.header.owner, 100);
    assert_eq!(group.header.count, 1);
    assert!(!group.header.selected);
    let second = loop {
        match c
            .select_reclaim_copy_group(&parent, 100, limits, ReclaimCopyGroupChoice::Advance)
            .await
            .unwrap()
        {
            ReclaimCopyGroupSelection::NeedsCandidate(file) => break file,
            ReclaimCopyGroupSelection::Scanning => {}
            _ => panic!("expected the second original walk file"),
        }
    };
    assert_ne!(first.operation(), second.operation());
    c.claim_active_owner_epoch(&f.serial, 101).await.unwrap();
    let adopted_parent = c
        .start_reclaim_copy_group_management(
            &f.serial,
            f.route,
            101,
            1,
            Uuid::parse_str(parent.intent()["operation"].as_str().unwrap()).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(adopted_parent.intent(), parent.intent());
    let tr = c.db.create_trx().unwrap();
    let current = walk::current_state_at(&tr, &adopted_parent).await.unwrap();
    assert_eq!(current.current(), Some(&second));
    let raw = tr
        .get(&key(&parent, b'h', Some(group.id()), None), false)
        .await
        .unwrap()
        .unwrap();
    let original_header: Header = decode(&raw).unwrap();
    assert_eq!(original_header.owner, 100);
    drop(tr);
    let commands = [0, 1].map(|at| f._library.drive(at).command_log().len());
    let (before_version, before) = reclaim_rows(&c, &adopted_parent).await;
    let error = match c
        .select_reclaim_copy_group(
            &adopted_parent,
            101,
            limits,
            ReclaimCopyGroupChoice::Candidate(&original_candidate),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("the original candidate must not replace the current walk file"),
    };
    assert!(
        matches!(
            error.domain(),
            CatalogError::Invalid("组选择改变原walk当前文件")
        ),
        "{error}"
    );
    let (after_version, after) = reclaim_rows(&c, &adopted_parent).await;
    assert_eq!(
        before, after,
        "a rejected candidate must not commit an earlier owner/header update"
    );
    assert_eq!(
        commands,
        [0, 1].map(|at| f._library.drive(at).command_log().len())
    );
    eprintln!(
        "SELECTING_OWNER_ABORT namespace={} group={} oldowner=100 newowner=101 before_rv={} after_rv={} exact_rows={} unchanged=true cdb_delta=0",
        parent.namespace(),
        group.id(),
        before_version,
        after_version,
        before.len()
    );
}

#[path = "reclaim_group_only_tests.rs"]
mod group_only_tests;
