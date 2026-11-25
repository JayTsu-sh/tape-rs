//! Real Current target count and the original intent preparation boundary.
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

#[tokio::test]
#[ignore = "固定private FDB；真实98file目标/1member/100与101物理额度，99不足拒绝，不写组正文"]
async fn one_member_intent_uses_remaining_pair_slots_once() {
    use std::os::fd::AsRawFd;
    let cluster = pinned_private_cluster();
    let path = format!("/proc/self/fd/{}", cluster.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&path)).unwrap();
    Box::pin(pair_case(&c, 100)).await;
    Box::pin(pair_case(&c, 101)).await;
}

async fn pair_case(c: &ManagedCatalog, limit: u64) {
    let mut f = Box::pin(live_source(c, 1)).await;
    let source = f._library.drive(0);
    let mut budget = RecoveryBudget::default().with_complete_full_history();
    budget.index_scratch.directory = f._root.clone();
    budget.index_scratch.headroom_bytes = 0;
    let source_label = read_label_only(&source).unwrap();
    f.captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&source),
        &source_label,
        &budget,
    )
    .unwrap();
    // Choose only the request ID; admission and all group state remain real.
    let request = ManagementRequest::parse(json!({"version":1,"library":f.serial,"namespace":f.route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":f.spec.to_json()})).unwrap();
    let target = f._library.drive(1);
    let guard = ReservationKey::new(1, 100);
    reservation::fence(&target, guard).unwrap();
    let mut volume = LtfsVolume::mount(&target).unwrap();
    volume.set_reservation_guard(Some(guard));
    volume
        .create_directory(&format!("/_reclaim_{}", request.operation()))
        .unwrap();
    for at in 0..98 {
        volume
            .append_file(&format!("/seed-{at:03}"), &mut std::io::empty())
            .unwrap();
    }
    volume.sync().unwrap();
    drop(volume);
    let label = read_label_only(&target).unwrap();
    let captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&target),
        &label,
        &budget,
    )
    .unwrap();
    let mut physical_files = 0;
    captured
        .for_each_entry(|entry| {
            physical_files +=
                u64::from(matches!(entry, crate::ltfs::index::IndexEntry::File { .. }));
            Ok(())
        })
        .unwrap();
    assert_eq!(physical_files, 98);
    Box::pin(import_captured(c, &f, "GROUPD00L8", &captured)).await;
    let ns = f.route.route.namespace;
    let pool_key = crate::managed_catalog::control::pool_key(ns, f.spec.pool());
    let tr = c.db.create_trx().unwrap();
    let pool_raw = tr.get(&pool_key, false).await.unwrap().unwrap().to_vec();
    let mut pool = crate::managed_catalog::control::decode_pool(f.spec.pool(), &pool_raw).unwrap();
    pool.file_limit = limit;
    tr.set(
        &pool_key,
        &crate::managed_catalog::control::encode_pool(&pool),
    );
    tr.commit().await.unwrap();
    c.admit_management(&f.serial, f.route, 100, &request)
        .await
        .unwrap();
    let parent = Box::pin(parent_for_request(c, &f, &request)).await;
    let (group, proofs) = Box::pin(selected(c, &f, &parent, 1)).await;
    assert_eq!(group.members.len(), 1);
    let member = &group.members[0];
    let spec = ActiveUploadSpec {
        pool_id: f.spec.pool(),
        path: member.file.path().as_bytes().to_vec(),
        operation_id: member.file.operation(),
        expected_version: Some(member.file.version()),
        size: member.file.size(),
        digest: ContentDigest::Sha256(member.known_sha256),
    };
    c.prepare_reclaim_group_member(&parent, 100, group.id(), 0, &spec, &proofs[0])
        .await
        .unwrap();
    let tr = c.db.create_trx().unwrap();
    assert_eq!(
        active_volume::file_count_at(&tr, &f.serial, group.header.destination.volume)
            .await
            .unwrap(),
        98
    );
    assert_eq!(
        remaining_slots_at(&tr, &parent, group.header.destination.volume)
            .await
            .unwrap(),
        1
    );
    let original = load_at(&tr, &parent, 100).await.unwrap().unwrap();
    require_previous_at(&tr, &parent, 100, &original)
        .await
        .unwrap();
    assert!(
        tr.get(&key(&parent, b'i', Some(group.id()), None), false)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    let before_commands = target.command_log().len();
    // A valid but insufficient limit must reject without creating native rows.
    pool.file_limit = 99;
    let tr = c.db.create_trx().unwrap();
    tr.set(
        &pool_key,
        &crate::managed_catalog::control::encode_pool(&pool),
    );
    tr.commit().await.unwrap();
    assert!(
        c.prepare_reclaim_index_intent(&parent, 100, group.id(), &f.captured, &captured)
            .await
            .is_err()
    );
    let tr = c.db.create_trx().unwrap();
    assert!(
        tr.get(&key(&parent, b'i', Some(group.id()), None), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tr.get(&key(&parent, b'n', Some(group.id()), Some(0)), false)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    pool.file_limit = limit;
    let tr = c.db.create_trx().unwrap();
    tr.set(
        &pool_key,
        &crate::managed_catalog::control::encode_pool(&pool),
    );
    tr.commit().await.unwrap();
    let result = c
        .prepare_reclaim_index_intent(&parent, 100, group.id(), &f.captured, &captured)
        .await;
    eprintln!(
        "QUOTA_PAIR_INTENT current=98 limit={limit} pair_slots=1 members=1 result={:?}",
        result.as_ref().map(|_| "prepared")
    );
    assert!(
        result.is_ok(),
        "one member consumes the already-computed single pair slot exactly once: {:?}",
        result.err()
    );
    assert_eq!(
        target.command_log().len(),
        before_commands,
        "catalog preparation never writes a group body or checkpoint"
    );
}
