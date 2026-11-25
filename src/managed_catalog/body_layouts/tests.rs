use super::*;
use crate::ltfs::index::IndexLocation;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::scsi::reservation::{ReservationKey, fence};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use std::io::Cursor;

#[path = "publisher_tests.rs"]
mod publisher;

fn layout(count: usize) -> BodyLayout<'static> {
    BodyLayout::new(
        Uuid::from_u128(10),
        17,
        count as u64,
        64 * 1024,
        ['a', 'b'],
        (0..count)
            .map(|index| Extent {
                partition: if index % 2 == 0 { 'b' } else { 'a' },
                start_block: 100 + index as u64 / 4,
                byte_offset: (index % 4) as u64,
                byte_count: 1,
                file_offset: index as u64,
            })
            .collect(),
    )
    .unwrap()
}

fn verified(layout: BodyLayout<'static>) -> VerifiedCommittedBodyLayout<'static> {
    let mut receipt = CommitReceipt {
        namespace: Uuid::from_u128(1),
        parent: Uuid::nil(),
        name: b"file".to_vec(),
        file_id: Uuid::from_u128(2),
        expected_version: None,
        operation_id: Uuid::from_u128(3),
        request_epoch: 1,
        copy_id: Uuid::nil(),
        library_serial: "SIM".into(),
        pool_id: Some(Uuid::from_u128(4)),
        owner_epoch: 7,
        volume_id: layout.volume,
        volume_generation: 5,
        volume_scan_id: Uuid::from_u128(6),
        ltfs_file_uid: layout.uid,
        size: layout.length,
        content_digest: ContentDigest::Sha256([7; 32]),
        metadata_digest: Some([8; 32]),
    };
    receipt.copy_id = derive_copy_id(&receipt);
    VerifiedCommittedBodyLayout {
        source: ActiveIndexIdentity {
            volume_uuid: layout.volume,
            generation: receipt.volume_generation,
            location: IndexLocation {
                partition: 'b',
                start_block: 500,
            },
            sha256: [9; 32],
            incremental: false,
        },
        receipt,
        path: "/file".into(),
        layout,
    }
}

#[test]
fn complete_layout_preserves_multiextent_shared_blocks_and_byte_offsets() {
    let body = layout(12);
    let manifest = body.manifest().unwrap();
    let bytes = body.chunk(manifest, 0).unwrap();
    assert_eq!(&bytes[..3], b"BL1");
    assert_eq!(&bytes[3..19], body.volume.as_bytes());
    assert_eq!(bytes.len(), LAYOUT_HEADER + 12 * EXTENT_BYTES);
    assert_eq!(manifest.extents, 12);
    assert_eq!(manifest.chunks, 1);
    assert_eq!(body.extents()[0].start_block, body.extents()[2].start_block);
    assert_eq!(body.extents()[2].byte_offset, 2);
    assert_eq!(body.extents()[3].partition, 'a');
    assert_eq!(body.extents()[3].file_offset, 3);
    assert_eq!(
        &bytes[LAYOUT_HEADER + 2 * EXTENT_BYTES..LAYOUT_HEADER + 3 * EXTENT_BYTES],
        &BodyLayout::extent_bytes(&body.extents()[2])
    );
}

#[test]
fn chunks_cover_record_boundaries_without_materializing_a_second_full_layout() {
    let body = layout(10_000);
    let manifest = body.manifest().unwrap();
    let mut hash = Sha256::new();
    hash.update(LAYOUT_DOMAIN);
    let mut total = 0;
    for index in 0..manifest.chunks {
        let bytes = body.chunk(manifest, index).unwrap();
        assert!(bytes.len() <= CHUNK_BYTES);
        if index + 1 != manifest.chunks {
            assert_eq!(bytes.len(), CHUNK_BYTES);
        }
        total += bytes.len();
        hash.update(bytes);
    }
    assert_eq!(total, manifest.len as usize);
    assert_eq!(hash.finalize().as_slice(), manifest.digest);
    assert!(body.chunk(manifest, manifest.chunks).is_err());
    assert_eq!(
        LayoutManifest::decode(&manifest.encode()).unwrap(),
        manifest
    );
}

#[test]
fn optional_projection_size_preflight_is_checked_without_allocating_extents() {
    let maximum = (u32::MAX as usize - LAYOUT_HEADER) / EXTENT_BYTES;
    assert_eq!(projection_size(0), Some(LAYOUT_HEADER as u32));
    assert!(projection_size(maximum).is_some());
    assert!(projection_size(maximum + 1).is_none());
    assert!(projection_size(usize::MAX).is_none());
    let proof = verified(layout(0));
    let mut binding = CopyLayoutBinding::from_verified(Uuid::from_u128(11), &proof).unwrap();
    assert!(binding.fits_read_budget(LAYOUT_HEADER as u64));
    assert!(!binding.fits_read_budget(LAYOUT_HEADER as u64 - 1));
    binding.manifest.extents = (MAX_READ_LAYOUT_BYTES / EXTENT_BYTES as u64) as u32;
    binding.manifest.len = LAYOUT_HEADER as u32 + binding.manifest.extents * EXTENT_BYTES as u32;
    binding.manifest.chunks = (binding.manifest.len as usize).div_ceil(CHUNK_BYTES) as u32;
    binding.manifest.validate().unwrap();
    assert!(u64::from(binding.manifest.len) > MAX_READ_LAYOUT_BYTES);
    assert!(!binding.fits_read_budget(128 << 30));
    let maximum_read_extents = (MAX_READ_LAYOUT_BYTES as usize - LAYOUT_HEADER) / EXTENT_BYTES;
    // Vec -> Arc conversion and layout -> read-plan cloning each have at most
    // two extent arrays alive; the final plan clone occurs after layout drops.
    let extent_storage = 2 * maximum_read_extents * std::mem::size_of::<Extent>();
    println!(
        "body_layout memory: Extent={} bytes, max_extents={}, two_arrays={} bytes",
        std::mem::size_of::<Extent>(),
        maximum_read_extents,
        extent_storage,
    );
    assert!(extent_storage <= 24 << 20);
}

#[test]
fn committed_layout_view_above_old_read_budget_borrows_the_original_extent_array() {
    let count = (MAX_READ_LAYOUT_BYTES as usize - LAYOUT_HEADER) / EXTENT_BYTES + 1;
    let original = layout(count);
    let view = BodyLayout::borrowed(
        original.volume,
        original.uid,
        original.length,
        original.block_size,
        original.partitions,
        original.extents(),
    )
    .unwrap();
    assert!(matches!(&view.extents, Cow::Borrowed(_)));
    assert_eq!(view.extents().as_ptr(), original.extents().as_ptr());
    assert_eq!(view.clone().extents().as_ptr(), original.extents().as_ptr());
    assert!(u64::from(view.manifest().unwrap().len) > MAX_READ_LAYOUT_BYTES);
    assert_eq!(view.manifest().unwrap(), original.manifest().unwrap());
}

#[test]
fn empty_body_is_representable_and_layout_identity_excludes_path_and_copy() {
    let body = layout(0);
    assert!(body.extents().is_empty());
    assert_eq!(body.length(), 0);
    assert_eq!(body.manifest().unwrap().len as usize, LAYOUT_HEADER);
    let first = verified(body);
    let mut renamed = verified(first.layout.clone());
    renamed.path = "/renamed".into();
    renamed.receipt.name = b"renamed".to_vec();
    renamed.receipt.file_id = Uuid::new_v4();
    renamed.receipt.copy_id = derive_copy_id(&renamed.receipt);
    let a = CopyLayoutBinding::from_verified(Uuid::from_u128(11), &first).unwrap();
    let b = CopyLayoutBinding::from_verified(Uuid::from_u128(12), &renamed).unwrap();
    assert_eq!(a.manifest, b.manifest);
    assert_ne!(a.encode().unwrap(), b.encode().unwrap());
}

#[test]
fn layout_rejects_holes_wrong_partitions_lengths_and_overflow() {
    let base = layout(2);
    for case in 0..8 {
        let mut candidate = base.clone();
        let mut extents = candidate.extents.to_vec();
        match case {
            0 => candidate.volume = Uuid::nil(),
            1 => candidate.uid = 0,
            2 => candidate.block_size = 0,
            3 => candidate.partitions = ['a', 'a'],
            4 => extents[0].partition = 'z',
            5 => extents[1].file_offset = 4,
            6 => extents[0].byte_offset = u64::from(candidate.block_size),
            7 => {
                extents[0].byte_offset = u64::MAX;
                extents[0].byte_count = 2;
            }
            _ => unreachable!(),
        }
        candidate.extents = extents.into();
        assert!(candidate.validate().is_err(), "case {case}");
    }
    let mut wrong_length = base.clone();
    wrong_length.length += 1;
    assert!(wrong_length.validate().is_err());
    wrong_length = base;
    wrong_length.extents = Vec::new().into();
    assert!(wrong_length.validate().is_err());
    assert!(
        BodyLayout::new(
            Uuid::new_v4(),
            1,
            2,
            1,
            ['a', 'b'],
            vec![Extent {
                partition: 'b',
                start_block: u64::MAX,
                byte_offset: 0,
                byte_count: 2,
                file_offset: 0,
            }],
        )
        .is_err()
    );
}

#[test]
fn manifest_and_binding_decoders_reject_all_truncations_and_trailing_bytes() {
    let proof = verified(layout(10));
    let binding = CopyLayoutBinding::from_verified(Uuid::from_u128(11), &proof).unwrap();
    let bytes = binding.encode().unwrap();
    assert_eq!(CopyLayoutBinding::decode(&bytes).unwrap(), binding);
    for end in 0..bytes.len() {
        assert!(CopyLayoutBinding::decode(&bytes[..end]).is_err(), "{end}");
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(CopyLayoutBinding::decode(&extra).is_err());
    let mut manifest = binding.manifest.encode();
    for end in 0..manifest.len() {
        assert!(LayoutManifest::decode(&manifest[..end]).is_err());
    }
    manifest[43..47].copy_from_slice(&0u32.to_be_bytes());
    assert!(LayoutManifest::decode(&manifest).is_err());
}

#[test]
fn binding_requires_original_copy_and_construct_and_is_namespace_owned() {
    let proof = verified(layout(2));
    let binding = CopyLayoutBinding::from_verified(Uuid::from_u128(11), &proof).unwrap();
    let copy = copy_records::decode_copy(&encode_copy(&proof.receipt, 1)).unwrap();
    assert!(binding.matches_copy(
        proof.receipt.namespace,
        proof.receipt.file_id,
        proof.receipt.copy_id,
        &copy,
        proof.source,
    ));
    assert!(!binding.matches_copy(
        proof.receipt.namespace,
        proof.receipt.file_id,
        Uuid::new_v4(),
        &copy,
        proof.source,
    ));
    let mut changed_source = proof.source;
    changed_source.location.start_block += 1;
    assert!(!binding.matches_copy(
        proof.receipt.namespace,
        proof.receipt.file_id,
        proof.receipt.copy_id,
        &copy,
        changed_source,
    ));
    let mut wrong_origin = copy.clone();
    wrong_origin.origin = CopyOrigin::FileRequest { request_epoch: 2 };
    assert!(!binding.matches_copy(
        proof.receipt.namespace,
        proof.receipt.file_id,
        proof.receipt.copy_id,
        &wrong_origin,
        proof.source,
    ));
    let prefix = namespace_prefix(proof.receipt.namespace);
    assert!(progress_key(proof.receipt.namespace, binding.manifest).starts_with(&prefix));
    assert!(binding_key(binding.namespace, binding.file, binding.copy).starts_with(&prefix));
    assert!(!binding_key(Uuid::new_v4(), binding.file, binding.copy).starts_with(&prefix));
}

#[test]
fn committed_factory_rejects_pending_and_changed_original_receipts() {
    let library = SimLibrary::new(1, 4, 1);
    library
        .insert_cartridge(SimCartridge::blank("LAYOUTL8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("LAYOUTL8", 0).unwrap();
    let drive = library.drive(0);
    mkltfs(
        &drive,
        &MkltfsOptions {
            volume_id: "LAYOUT".into(),
            block_size: 64 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let key = ReservationKey::new(1, 7);
    fence(&drive, key).unwrap();
    let mut volume = LtfsVolume::mount(&drive).unwrap();
    volume.set_reservation_guard(Some(key));
    let data = b"committed layout bytes";
    let intent = ActiveFileIntent {
        namespace: Uuid::new_v4(),
        parent: Uuid::nil(),
        parent_path: b"/".to_vec(),
        name: b"file".to_vec(),
        file_id: Uuid::new_v4(),
        expected_version: None,
        operation_id: Uuid::new_v4(),
        request_epoch: 1,
        library_serial: "SIM-CHANGER".into(),
        pool_id: Some(Uuid::new_v4()),
        barcode_constraint: None,
        owner_epoch: 7,
        volume_id: volume.label().volume_uuid,
        volume_generation: volume.index().generation + 1,
        volume_scan_id: Uuid::new_v4(),
        expected_size: data.len() as u64,
        expected_digest: ContentDigest::Sha256(Sha256::digest(data).into()),
    };
    volume.append_file("/file", &mut Cursor::new(data)).unwrap();
    let pending = prepare_active_file(&volume, "/file", &intent).unwrap();
    assert!(CommittedLayoutSource::new(&mut volume).is_err());
    volume.sync_batch().unwrap();
    let source = CommittedLayoutSource::new(&mut volume).unwrap();
    assert!(source.projection_supported("/file").unwrap());
    let proof = source.file(&intent, &pending.receipt).unwrap().unwrap();
    assert!(matches!(&proof.layout.extents, Cow::Borrowed(_)));
    assert_eq!(
        proof.layout.extents().as_ptr(),
        source
            .volume
            .index()
            .find_file("/file")
            .unwrap()
            .extents
            .as_ptr()
    );
    assert_eq!(proof.layout.length(), data.len() as u64);
    assert_eq!(proof.layout.uid(), pending.receipt.ltfs_file_uid);
    let mut wrong = pending.receipt.clone();
    wrong.size += 1;
    assert!(source.file(&intent, &wrong).is_err());
    wrong = pending.receipt.clone();
    wrong.copy_id = Uuid::new_v4();
    assert!(source.file(&intent, &wrong).is_err());
}

#[tokio::test]
#[ignore = "需要 FDB_TEST_CLUSTER；隔离命名空间布局分片/恢复/损坏测试，不访问设备"]
async fn immutable_layout_chunks_resume_and_reject_missing_or_corrupt_data() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    catalog.begin_import(namespace).await.unwrap();
    let mut proof = verified(layout(10_000));
    proof.receipt.namespace = namespace;
    proof.receipt.copy_id = derive_copy_id(&proof.receipt);
    let batch = Uuid::new_v4();
    let manifest = proof.layout.manifest().unwrap();

    let tr = catalog.db.create_trx().unwrap();
    tr.set(
        &chunk_key(namespace, manifest, 0),
        &proof.layout.chunk(manifest, 0).unwrap(),
    );
    tr.set(&progress_key(namespace, manifest), &progress(manifest, 1));
    tr.commit().await.unwrap();
    let binding = catalog.stage_test_body_layout(batch, &proof).await.unwrap();
    assert_eq!(
        catalog.stage_test_body_layout(batch, &proof).await.unwrap(),
        binding
    );
    let requirement = BatchLayoutRequirement {
        namespace,
        batch,
        source: proof.source,
        manifest: [19; 32],
        items: 1,
        pages: 1,
    };
    let mut raw = 1u16.to_be_bytes().to_vec();
    raw.extend_from_slice(&proof.receipt.request_epoch.to_be_bytes());
    raw.extend_from_slice(proof.receipt.operation_id.as_bytes());
    raw.extend_from_slice(&receipt_fingerprint(&proof.receipt));
    let mut original = [0; 49];
    original[0] = 1;
    original[1..17].copy_from_slice(batch.as_bytes());
    let mut page_hash = Sha256::new();
    page_hash.update(b"tape-rs/active-prepared-page/1");
    page_hash.update(&raw);
    original[17..].copy_from_slice(&page_hash.finalize());
    let tr = catalog.db.create_trx().unwrap();
    // The SDK/legacy path has no physical-layout constraint.
    assert!(requirement.complete_at(&tr, &[original]).await.unwrap());
    requirement.declare_at(&tr).await.unwrap();
    assert!(!requirement.complete_at(&tr, &[original]).await.unwrap());
    tr.set(
        &requests::intent_key(&proof.receipt),
        &requests::encode_intent(&proof.receipt, 0),
    );
    requirement.prepare_page_at(&tr, 0, &raw).await.unwrap();
    assert!(requirement.complete_at(&tr, &[original]).await.unwrap());
    let mut changed = original;
    changed[48] ^= 1;
    assert!(requirement.complete_at(&tr, &[changed]).await.is_err());
    let mut changed = requirement;
    changed.manifest[0] ^= 1;
    assert!(changed.exists_at(&tr).await.is_err());
    tr.commit().await.unwrap();
    let loaded = catalog
        .load_body_layout(&binding, u64::from(manifest.len))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded, proof.layout);
    assert!(
        catalog
            .load_body_layout(&binding, u64::from(manifest.len) - 1)
            .await
            .unwrap()
            .is_none()
    );
    let tr = catalog.db.create_trx().unwrap();
    require_binding_at(&tr, batch, &proof.receipt, proof.source)
        .await
        .unwrap();
    assert!(
        require_binding_at(&tr, Uuid::new_v4(), &proof.receipt, proof.source)
            .await
            .is_err()
    );
    drop(tr);
    let tr = catalog.db.create_trx().unwrap();
    tr.clear(&chunk_key(namespace, manifest, 1));
    tr.commit().await.unwrap();
    assert!(catalog.load_body_layout(&binding, u64::MAX).await.is_err());
    assert!(catalog.stage_test_body_layout(batch, &proof).await.is_err());

    let tr = catalog.db.create_trx().unwrap();
    let mut corrupt = proof.layout.chunk(manifest, 1).unwrap();
    corrupt[0] ^= 1;
    tr.set(&chunk_key(namespace, manifest, 1), &corrupt);
    tr.commit().await.unwrap();
    assert!(catalog.load_body_layout(&binding, u64::MAX).await.is_err());
    let tr = catalog.db.create_trx().unwrap();
    tr.set(
        &chunk_key(namespace, manifest, 1),
        &proof.layout.chunk(manifest, 1).unwrap(),
    );
    tr.set(&chunk_key(namespace, manifest, manifest.chunks), b"extra");
    tr.commit().await.unwrap();
    assert!(catalog.load_body_layout(&binding, u64::MAX).await.is_err());
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；新布局域namespace初始化/Discarded清理隔离，不访问设备"]
async fn namespace_cleanup_owns_layout_domain_and_rejects_orphan_reinitialization() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    let protected = Uuid::new_v4();
    catalog.begin_import(namespace).await.unwrap();
    catalog.begin_import(protected).await.unwrap();
    let mut proof = verified(layout(2));
    proof.receipt.namespace = namespace;
    proof.receipt.copy_id = derive_copy_id(&proof.receipt);
    let batch = Uuid::new_v4();
    let binding = catalog.stage_test_body_layout(batch, &proof).await.unwrap();
    let requirement = BatchLayoutRequirement {
        namespace,
        batch,
        source: proof.source,
        manifest: [12; 32],
        items: 1,
        pages: 1,
    };
    let tr = catalog.db.create_trx().unwrap();
    requirement.declare_at(&tr).await.unwrap();
    let domain_key = |kind: u8, namespace: Uuid| {
        let mut key = key(kind);
        key.extend_from_slice(namespace.as_bytes());
        key.extend_from_slice(b"cleanup-test");
        key
    };
    let history_key = domain_key(FULL_HISTORY_HINT_KIND, namespace);
    let protected_history_key = domain_key(FULL_HISTORY_HINT_KIND, protected);
    tr.set(&history_key, b"history hint fixture");
    tr.set(&protected_history_key, b"protected history hint fixture");
    tr.commit().await.unwrap();
    proof.receipt.namespace = protected;
    proof.receipt.copy_id = derive_copy_id(&proof.receipt);
    let protected_binding = catalog
        .stage_test_body_layout(Uuid::new_v4(), &proof)
        .await
        .unwrap();
    assert!(
        catalog
            .purge_discarded_namespace(namespace, 2)
            .await
            .is_err()
    );
    catalog.discard_import(namespace).await.unwrap();
    // A retained operation must protect every registered domain, including
    // the new hint domain. No decoding/repair or deletion bypasses this gate.
    let mut hold_key = control::node_hold_prefix(namespace);
    hold_key.extend_from_slice(Uuid::new_v4().as_bytes());
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&hold_key, b"retained operation fixture");
    tr.commit().await.unwrap();
    assert!(
        catalog
            .purge_discarded_namespace(namespace, 2)
            .await
            .is_err()
    );
    let tr = catalog.db.create_trx().unwrap();
    assert!(tr.get(&history_key, false).await.unwrap().is_some());
    assert!(
        tr.get(&binding_key(namespace, binding.file, binding.copy), false)
            .await
            .unwrap()
            .is_some()
    );
    tr.clear(&hold_key);
    tr.commit().await.unwrap();
    let mut complete = false;
    for _ in 0..100 {
        let page = catalog
            .purge_discarded_namespace(namespace, 2)
            .await
            .unwrap();
        assert!(page.removed <= 2);
        if page.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    let tr = catalog.db.create_trx().unwrap();
    assert!(tr.get(&history_key, false).await.unwrap().is_none());
    assert_eq!(
        tr.get(&protected_history_key, false)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"protected history hint fixture"
    );
    assert!(
        layout_chunks_at(&tr, &namespace_prefix(namespace), None, 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        binding_at(&tr, namespace, binding.file, binding.copy)
            .await
            .unwrap()
            .is_none()
    );
    drop(tr);
    assert!(
        catalog
            .load_body_layout(&protected_binding, 8 << 20)
            .await
            .unwrap()
            .is_some()
    );
    for kind in [BODY_LAYOUT_KIND, FULL_HISTORY_HINT_KIND] {
        let orphan = Uuid::new_v4();
        let orphan_key = domain_key(kind, orphan);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&orphan_key, b"orphan domain record");
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.begin_import(orphan).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&orphan_key);
        tr.commit().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；大于8MiB布局分页到匿名fd/末片/资源错误，不接触设备"]
async fn paged_read_plan_validates_large_layout_before_sealing_and_bounds_scratch() {
    use crate::local_read_storage::{ReadStorageBudget, download_file};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let namespace = Uuid::new_v4();
    catalog.begin_import(namespace).await.unwrap();
    let count = (MAX_READ_LAYOUT_BYTES as usize - LAYOUT_HEADER) / EXTENT_BYTES + 1;
    let mut proof = verified(layout(count));
    proof.receipt.namespace = namespace;
    proof.receipt.copy_id = derive_copy_id(&proof.receipt);
    let binding = catalog
        .stage_test_body_layout(Uuid::new_v4(), &proof)
        .await
        .unwrap();
    let manifest = binding.manifest;
    assert!(u64::from(manifest.len) > MAX_READ_LAYOUT_BYTES);
    let directory = std::env::temp_dir().join(format!("body-plan-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let allocate = || {
        download_file(
            &directory,
            u64::from(manifest.len),
            ReadStorageBudget {
                download_bytes: 16 << 20,
                index_bytes: 16 << 20,
                headroom_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap()
    };
    assert!(matches!(
        catalog
            .prepare_body_read_plan(&binding, allocate(), u64::from(manifest.len) - 1)
            .await,
        Err(crate::error::TapeError::LocalCapacity { .. })
    ));
    let plan = catalog
        .prepare_body_read_plan(&binding, allocate(), 16 << 20)
        .await
        .unwrap();
    assert_eq!(plan.length(), proof.layout.length());
    assert_eq!(plan.uid(), proof.layout.uid());
    assert_eq!(plan.volume(), proof.layout.volume());
    assert_eq!(plan.block_size(), proof.layout.block_size());
    assert_eq!(plan.partitions(), proof.layout.partitions());
    assert_eq!(plan.path(), "file");
    assert_eq!(plan.binding().path(), "/file");
    assert_eq!(plan.binding(), &binding);
    let mut expected = proof.layout.extents().iter();
    let mut actual = plan.extents();
    loop {
        match (expected.next(), actual.next()) {
            (Some(expected), Some(actual)) => assert_eq!(expected, &actual.unwrap()),
            (None, None) => break,
            _ => panic!("sealed plan extent count changed"),
        }
    }
    let mut first = plan.extents();
    first.next().unwrap().unwrap();
    let mut second = first.clone();
    assert_eq!(
        first.next().unwrap().unwrap(),
        second.next().unwrap().unwrap()
    );
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    let last = manifest.chunks - 1;
    let original = proof.layout.chunk(manifest, last).unwrap();
    let mut corrupt = original.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&chunk_key(namespace, manifest, last), &corrupt);
    tr.commit().await.unwrap();
    assert!(
        catalog
            .prepare_body_read_plan(&binding, allocate(), 16 << 20)
            .await
            .is_err()
    );
    let tr = catalog.db.create_trx().unwrap();
    tr.clear(&chunk_key(namespace, manifest, last));
    tr.commit().await.unwrap();
    assert!(
        catalog
            .prepare_body_read_plan(&binding, allocate(), 16 << 20)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    drop(first);
    drop(second);
    drop(plan);
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn lb2_codec_keeps_full_canonical_identity_and_strict_value_bounds() {
    use crate::ltfs::volume::CommittedIndexConstruct;
    for count in [0usize, 1, 64] {
        let proof = verified(layout(0));
        let mut binding = CopyLayoutBinding::from_verified(Uuid::from_u128(11), &proof).unwrap();
        let head = CommittedIndexConstruct {
            volume_uuid: binding.source.volume_uuid,
            generation: count as u64 + 1,
            location: IndexLocation {
                partition: 'b',
                start_block: 1000,
            },
            sha256: [21; 32],
            incremental: count > 0,
        };
        let predecessors = (0..count)
            .map(|n| CommittedIndexConstruct {
                generation: (count - n) as u64,
                location: IndexLocation {
                    partition: 'b',
                    start_block: 999 - n as u64,
                },
                sha256: [n as u8; 32],
                incremental: n + 1 < count,
                ..head
            })
            .collect();
        let identity = IndexSourceIdentity::new(head, predecessors).unwrap();
        assert_eq!(identity.to_bytes().len(), 68 + 66 * count);
        binding.source = head.into();
        binding.path = format!("/{}", "x".repeat(u16::MAX as usize - 1));
        binding.provenance = LayoutProvenance::VerifiedCopySource {
            copy_record_sha: copy_record_digest(b"exact original R bytes"),
            source_identity_sha: identity.fingerprint(),
            source_identity: identity.clone(),
        };
        let bytes = binding.encode().unwrap();
        assert_eq!(&bytes[..3], b"LB2");
        assert_eq!(
            bytes.len(),
            LB2_FIXED_BYTES + binding.path.len() + identity.to_bytes().len()
        );
        assert!(bytes.len() <= MAX_LB2_BINDING_BYTES);
        assert_eq!(CopyLayoutBinding::decode(&bytes).unwrap(), binding);
        assert_eq!(binding.source_identity(), Some(&identity));
        assert!(binding.matches_raw_copy(Some(b"exact original R bytes")));
        assert!(!binding.matches_raw_copy(Some(b"changed original R bytes")));
        assert!(!binding.matches_raw_copy(None));
        for end in [0, 2, 190, 191, 222, 254, 256, 257, 258, bytes.len() - 1] {
            assert!(
                CopyLayoutBinding::decode(&bytes[..end]).is_err(),
                "truncation {end}"
            );
        }
        for at in [190usize, 223, 257] {
            let mut bad = bytes.clone();
            bad[at] ^= 0xff;
            assert!(CopyLayoutBinding::decode(&bad).is_err(), "field {at}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(CopyLayoutBinding::decode(&trailing).is_err());
        binding.path.push('x');
        assert!(binding.encode().is_err());
    }
    let mut oversized = vec![0; MAX_LB2_BINDING_BYTES + 1];
    oversized[..3].copy_from_slice(b"LB2");
    assert!(CopyLayoutBinding::decode(&oversized).is_err());
}

#[test]
fn lb1_encoding_preserves_original_receipt_layout_byte_order() {
    let proof = verified(layout(1));
    let batch = Uuid::from_u128(11);
    let binding = CopyLayoutBinding::from_verified(batch, &proof).unwrap();
    let mut golden = b"LB1".to_vec();
    for id in [
        proof.receipt.namespace,
        proof.receipt.file_id,
        proof.receipt.copy_id,
    ] {
        golden.extend_from_slice(id.as_bytes());
    }
    golden.extend_from_slice(&1u64.to_be_bytes());
    golden.extend_from_slice(proof.receipt.volume_scan_id.as_bytes());
    golden.extend_from_slice(proof.source.volume_uuid.as_bytes());
    active_volume::encode_index(&mut golden, proof.source);
    golden.extend_from_slice(&proof.receipt.ltfs_file_uid.to_be_bytes());
    golden.extend_from_slice(&proof.receipt.size.to_be_bytes());
    golden.push(proof.receipt.content_digest.tag());
    golden.extend_from_slice(&proof.receipt.content_digest.payload());
    golden.extend_from_slice(&receipt_fingerprint(&proof.receipt));
    golden.extend_from_slice(&proof.receipt.request_epoch.to_be_bytes());
    golden.extend_from_slice(batch.as_bytes());
    golden.extend_from_slice(&5u16.to_be_bytes());
    golden.extend_from_slice(b"/file");
    golden.extend_from_slice(&proof.layout.manifest().unwrap().encode());
    assert_eq!(binding.encode().unwrap(), golden);
    assert_eq!(CopyLayoutBinding::decode(&golden).unwrap(), binding);
    assert_eq!(binding.source_identity(), None);
}

#[test]
fn required_copy_marker_has_fixed_version_and_domain_separated_digest() {
    let value = required_copy_value(b"binding");
    assert_eq!(&value[..3], b"LC1");
    assert_eq!(value.len(), 35);
    assert_eq!(
        &value[3..],
        &[
            0xa2, 0xd1, 0xd2, 0x8b, 0x45, 0x20, 0xea, 0x7e, 0xc9, 0xa1, 0xe8, 0xf4, 0xa3, 0x54,
            0xb5, 0xec, 0xc1, 0x82, 0x92, 0x3d, 0x53, 0xe5, 0x3c, 0x4f, 0xaf, 0x78, 0x2f, 0x88,
            0xee, 0xa0, 0xeb, 0x06,
        ]
    );
    assert_ne!(value, required_copy_value(b"other binding"));
}
