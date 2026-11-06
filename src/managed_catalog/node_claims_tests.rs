use super::*;
fn claim() -> RenameVolumeClaim {
    RenameVolumeClaim {
        namespace: Uuid::new_v4(),
        pool: Uuid::new_v4(),
        library: "CLAIM-LIB".into(),
        epoch: u64::MAX,
        operation: Uuid::new_v4(),
        global: Uuid::new_v4(),
        volume: Uuid::new_v4(),
        intent: Uuid::new_v4(),
        page: u64::MAX,
        owner: 0xFFFF_FFFF_FFFF,
        fingerprint: [255; 32],
        closed: None,
    }
}
#[test]
fn shared_claim_codec_is_strictly_typed_and_preserves_original_unsigned_scope() {
    let mut closed = claim();
    closed.closed = Some([7; 32]);
    let closed = NodeVolumeClaim::Rename(Box::new(closed));
    assert_eq!(decode(&encode(&closed).unwrap()).unwrap(), closed);
    let value = NodeVolumeClaim::Rename(Box::new(claim()));
    let bytes = encode(&value).unwrap();
    assert_eq!(&bytes[..2], &[1, 1]);
    assert_eq!(decode(&bytes).unwrap(), value);
    assert!(
        decode(&bytes[2..]).is_err(),
        "unframed records have no compatibility reader"
    );
    for len in [0, 1, 2, bytes.len() - 1] {
        assert!(decode(&bytes[..len]).is_err());
    }
    for (index, value) in [(0, 0), (0, 2), (1, 0), (1, 2)] {
        let mut bad = bytes.clone();
        bad[index] = value;
        assert!(decode(&bad).is_err());
    }
    let mut bad = bytes.clone();
    bad.push(b'\n');
    assert!(decode(&bad).is_err());
    assert!(decode(&vec![0; 8195]).is_err());
    let mut raw: serde_json::Value = serde_json::from_slice(&bytes[2..]).unwrap();
    raw["unknown"] = true.into();
    let mut bad = vec![1, 1];
    bad.extend(node_mutation::canonical(&raw).unwrap());
    assert!(decode(&bad).is_err());
    let mut old: serde_json::Value = serde_json::from_slice(&bytes[2..]).unwrap();
    old.as_object_mut().unwrap().remove("closed");
    let mut unsealed = vec![1, 1];
    unsealed.extend(node_mutation::canonical(&old).unwrap());
    assert!(decode(&unsealed).is_err());
    let duplicate = format!(
        "{{\"owner\":1,{}",
        std::str::from_utf8(&bytes[3..]).unwrap()
    );
    let mut bad = vec![1, 1];
    bad.extend(duplicate.as_bytes());
    assert!(decode(&bad).is_err());
}
#[test]
fn shared_claim_rejects_invalid_scope_without_a_loose_fallback() {
    let changes: [fn(&mut RenameVolumeClaim); 9] = [
        |v| v.namespace = Uuid::nil(),
        |v| v.pool = Uuid::nil(),
        |v| v.operation = Uuid::nil(),
        |v| v.global = Uuid::nil(),
        |v| v.volume = Uuid::nil(),
        |v| v.intent = Uuid::nil(),
        |v| v.owner = 0,
        |v| v.owner = 1 << 48,
        |v| v.epoch = 0,
    ];
    for change in changes {
        let mut value = claim();
        change(&mut value);
        assert!(encode(&NodeVolumeClaim::Rename(Box::new(value))).is_err());
    }
}
