//! An original retained history pin is not a foreign volume write claim.
use super::*;

#[tokio::test]
#[ignore = "原任务固定private FDB；原组第2member准入与外来/缺失/非空pin、0/I、失owner门禁，无组物理写"]
async fn second_member_retains_only_exact_group_history_pins() {
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
    let name = std::ffi::CString::new("quiescence-private-fdb").unwrap();
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
    let cluster = format!("/proc/self/fd/{}", sealed.as_raw_fd());
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let mut f = Box::pin(live_source(&c, 2)).await;
    // Production group selection uses a complete current/history capture.
    // The older shared fixture's default capture deliberately has no hint.
    let source_device = f._library.drive(0);
    let source_label = read_label_only(&source_device).unwrap();
    let mut complete = RecoveryBudget::default().with_complete_full_history();
    complete.index_scratch.directory = f._root.clone();
    complete.index_scratch.headroom_bytes = 0;
    f.captured = crate::ltfs::recovery::capture_complete_index_source(
        &TapeDrive::new(&source_device),
        &source_label,
        &complete,
    )
    .unwrap();
    let parent = Box::pin(parent(&c, &f)).await;
    let (group, proofs) = Box::pin(selected(&c, &f, &parent, 2)).await;
    assert_eq!(group.members.len(), 2);
    let specs = group
        .members
        .iter()
        .map(|member| ActiveUploadSpec {
            pool_id: f.spec.pool(),
            path: member.file.path().as_bytes().to_vec(),
            operation_id: member.file.operation(),
            expected_version: Some(member.file.version()),
            size: member.file.size(),
            digest: ContentDigest::Sha256(member.known_sha256),
        })
        .collect::<Vec<_>>();
    let first = c
        .prepare_reclaim_group_member(&parent, 100, group.id(), 0, &specs[0], &proofs[0])
        .await
        .unwrap();
    let pin = children::history_pin_key(&first.request());
    let target = group.header.destination.volume;
    let before = f._library.drive(1).command_log().len();
    let tr = c.db.create_trx().unwrap();
    assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
    assert!(
        matches!(
            native_device::require_volume_quiescent_at(&tr, parent.namespace(), target).await,
            Err(CatalogError::PoolBusy)
        ),
        "generic v1 quiescence must still reject the exact same F pin"
    );
    drop(tr);
    // Every mutation belongs to this new isolated namespace and is restored
    // before trying the next dependency. A rejection must not create child1.
    let mut foreign = crate::managed_catalog::key(b'F');
    foreign.extend_from_slice(parent.namespace().as_bytes());
    foreign.extend_from_slice(&1u64.to_be_bytes());
    foreign.extend_from_slice(Uuid::new_v4().as_bytes());
    let mut zero = crate::managed_catalog::key(b'0');
    zero.extend_from_slice(parent.namespace().as_bytes());
    zero.extend_from_slice(Uuid::new_v4().as_bytes());
    let mut intent = crate::managed_catalog::key(b'I');
    intent.extend_from_slice(parent.namespace().as_bytes());
    intent.extend_from_slice(Uuid::new_v4().as_bytes());
    for (label, k, value) in [
        ("foreign F", foreign, Some(Vec::new())),
        ("missing F", pin.clone(), None),
        ("nonempty F", pin.clone(), Some(vec![1])),
        ("0 hold", zero, Some(Vec::new())),
        ("I hold", intent, Some(Vec::new())),
    ] {
        let tr = c.db.create_trx().unwrap();
        let saved = tr.get(&k, false).await.unwrap().map(|v| v.to_vec());
        if let Some(value) = &value {
            tr.set(&k, value)
        } else {
            tr.clear(&k)
        };
        tr.commit().await.unwrap();
        assert!(
            c.prepare_reclaim_group_member(&parent, 100, group.id(), 1, &specs[1], &proofs[1])
                .await
                .is_err(),
            "{label} must fail closed"
        );
        let tr = c.db.create_trx().unwrap();
        let raw = tr
            .get(&key(&parent, b'm', Some(group.id()), Some(1)), false)
            .await
            .unwrap()
            .unwrap();
        let still_unprepared: Member = decode(&raw).unwrap();
        assert!(
            still_unprepared.child.is_none(),
            "{label} rejection must not prepare member1"
        );
        if let Some(saved) = saved {
            tr.set(&k, &saved)
        } else {
            tr.clear(&k)
        };
        tr.commit().await.unwrap();
    }
    let second = c
        .prepare_reclaim_group_member(&parent, 100, group.id(), 1, &specs[1], &proofs[1])
        .await
        .unwrap();
    assert_ne!(first.operation_id, second.operation_id);
    let current = c
        .resume_reclaim_copy_group(&parent, 100)
        .await
        .unwrap()
        .unwrap();
    assert!(current.members.iter().all(|member| member.child.is_some()));
    let tr = c.db.create_trx().unwrap();
    for request in [first.request(), second.request()] {
        assert_eq!(
            tr.get(&children::history_pin_key(&request), false)
                .await
                .unwrap()
                .as_deref(),
            Some(&[][..])
        );
    }
    drop(tr);
    c.claim_active_owner_epoch(&f.serial, 101).await.unwrap();
    assert!(
        c.prepare_reclaim_group_member(&parent, 100, group.id(), 1, &specs[1], &proofs[1])
            .await
            .is_err(),
        "old owner must stop with original pins intact"
    );
    assert_eq!(
        before,
        f._library.drive(1).command_log().len(),
        "quiescence validation cannot issue CDBs"
    );
}
