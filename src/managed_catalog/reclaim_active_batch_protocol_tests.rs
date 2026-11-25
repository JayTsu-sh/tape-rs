use super::*;

fn header(protocol: Protocol) -> Header {
    Header {
        protocol,
        batch_id: Uuid::from_bytes([1; 16]),
        namespace: Uuid::from_bytes([2; 16]),
        owner_epoch: 7,
        scan_id: Uuid::from_bytes([3; 16]),
        items: 2,
        pages: 1,
        digest: [4; 32],
        phase: ActiveBatchPhase::Sealed,
    }
}

#[test]
fn legacy_header_keeps_its_exact_version_two_wire_format() {
    let original = header(Protocol::Single);
    let raw = original.encode();
    assert_eq!(raw.len(), 96);
    assert_eq!(raw[0], 2);
    assert_eq!(raw[1], 1);
    assert_eq!(&raw[2..18], &[1; 16]);
    assert_eq!(&raw[18..34], &[2; 16]);
    assert_eq!(&raw[34..42], &7u64.to_be_bytes());
    assert_eq!(&raw[42..58], &[3; 16]);
    assert_eq!(&raw[58..62], &2u32.to_be_bytes());
    assert_eq!(&raw[62..64], &1u16.to_be_bytes());
    assert_eq!(&raw[64..], &[4; 32]);
    assert_eq!(Header::decode(&raw).unwrap(), original);
}

#[test]
fn required_group_is_in_header_and_cannot_match_a_legacy_manifest() {
    let legacy = header(Protocol::Single);
    let required = header(Protocol::ReclaimGroup);
    let raw = required.encode();
    assert_eq!(raw[0], 3);
    assert_eq!(&raw[1..], &legacy.encode()[1..]);
    assert_eq!(Header::decode(&raw).unwrap(), required);
    assert!(!legacy.same_manifest(required));
    for version in [0, 1, 4, 255] {
        let mut changed = raw;
        changed[0] = version;
        assert!(Header::decode(&changed).is_err());
    }
    for length in 0..raw.len() {
        assert!(Header::decode(&raw[..length]).is_err());
    }
}

#[tokio::test]
#[ignore = "仅FDB_TEST_CLUSTER=任务私有44247；存储门禁负例，不创建设备、不伪造业务成功"]
async fn missing_required_route_is_rejected_by_the_real_stored_checkpoint_gate() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    assert!(
        std::fs::read_to_string(&cluster)
            .unwrap()
            .contains("127.0.0.1:44247")
    );
    let _network = unsafe { foundationdb::boot() };
    let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
    let mut legacy = header(Protocol::Single);
    legacy.namespace = Uuid::new_v4();
    legacy.batch_id = Uuid::new_v4();
    let volume = Uuid::new_v4();
    let index = ActiveIndexIdentity {
        volume_uuid: volume,
        generation: 2,
        location: crate::ltfs::index::IndexLocation {
            partition: 'b',
            start_block: 5,
        },
        sha256: [9; 32],
        incremental: false,
    };
    // Deliberately synthetic header rows exercise only the storage gate. No
    // index observation, copy, receipt, coverage, or format success is written.
    let key = batch_key(volume, index.generation);
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&key, &legacy.encode());
    tr.commit().await.unwrap();
    let tr = catalog.db.create_trx().unwrap();
    require_stored_checkpoint_at(&tr, legacy.batch_id, index, None)
        .await
        .unwrap();
    drop(tr);
    let required = Header {
        protocol: Protocol::ReclaimGroup,
        ..legacy
    };
    let tr = catalog.db.create_trx().unwrap();
    tr.set(&key, &required.encode());
    tr.commit().await.unwrap();
    let tr = catalog.db.create_trx().unwrap();
    let error = require_stored_checkpoint_at(&tr, required.batch_id, index, None)
        .await
        .unwrap_err();
    assert!(matches!(error, CatalogError::Incomplete), "{error:?}");
    assert_eq!(
        tr.get(&key, false).await.unwrap().unwrap().as_ref(),
        required.encode()
    );
    eprintln!(
        "RECLAIM_HEADER3_ROUTE_NEGATIVE={}",
        serde_json::json!({
        "namespace":required.namespace.to_string(),"volume":volume.to_string(),"batch":required.batch_id.to_string(),
        "legacy_version":2,"required_version":3,"missing_route_rejected":true,
        "business_success_written":false,"hardware":false,"error":error.to_string()})
    );
}
