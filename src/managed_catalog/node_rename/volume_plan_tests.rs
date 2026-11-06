use super::super::physical_source::tests::fixture;
use super::super::volume_projection::tests::build;
use super::*;

fn record() -> (Record, NodeRenameRequest) {
    let mut f = fixture();
    let view = build(&mut f, 16 << 20).unwrap();
    let request = view.original_request().clone();
    let object = NodePlanObject::for_bytes(
        request.library(),
        request.spec().source.namespace,
        &request.encode().unwrap(),
        8 << 20,
    )
    .unwrap();
    let facts = Facts::from_projection(
        &view,
        &URL_SAFE_NO_PAD.encode(object.encode()),
        [5; 32],
        [6; 32],
        1,
        10,
    )
    .unwrap();
    (Record::new(facts, 1).unwrap(), request)
}
#[test]
fn volume_plan_codec_keeps_original_unsigned_scope_and_owner_independent_fingerprint() {
    let (record, request) = record();
    let bytes = record.encode().unwrap();
    assert_eq!(Record::decode(&bytes).unwrap(), record);
    assert_eq!(record.facts.highest_before, u64::MAX);
    assert_eq!(record.facts.highest_after, u64::MAX);
    let takeover = Record::new(record.facts.clone(), 2).unwrap();
    assert_eq!(takeover.facts.fingerprint(), record.facts.fingerprint());
    assert_ne!(takeover.encode().unwrap(), bytes);
    record
        .facts
        .require_scope(&request, &record.facts.request, [5; 32], [6; 32], 1, 10)
        .unwrap();
    let (_, foreign) = self::record();
    assert!(
        record
            .facts
            .require_scope(&foreign, &record.facts.request, [5; 32], [6; 32], 1, 10)
            .is_err()
    );
    assert!(
        record
            .facts
            .require_scope(&request, &record.facts.request, [7; 32], [6; 32], 1, 10)
            .is_err()
    );
    assert!(Record::new(record.facts.clone(), 0).is_err());
    assert!(Record::new(record.facts.clone(), 1 << 48).is_err());
    let plan = NodeRenameVolumePlan { record };
    assert_eq!(plan.highest_file_uid(), u64::MAX);
    assert_eq!(plan.creator_owner_epoch(), 1);
    assert_eq!(
        plan.source_index().volume_uuid,
        plan.record.facts.binding().unwrap().source.volume_uuid
    );
}
#[test]
fn volume_plan_codec_rejects_bad_shapes_scope_constraints_checksum_and_noncanonical_bytes() {
    let (record, _) = record();
    let bytes = record.encode().unwrap();
    for len in [0, 1, bytes.len() - 1] {
        assert!(Record::decode(&bytes[..len]).is_err());
    }
    let mut spaced = bytes.clone();
    spaced.push(b'\n');
    assert!(Record::decode(&spaced).is_err());
    assert!(Record::decode(&vec![b' '; 64_001]).is_err());
    for field in ["before_files", "recovery_abandoned", "recovery_marker"] {
        let mut missing: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        missing["facts"].as_object_mut().unwrap().remove(field);
        assert!(Record::decode(&node_mutation::canonical(&missing).unwrap()).is_err());
    }
    let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    unknown["unknown"] = true.into();
    assert!(Record::decode(&node_mutation::canonical(&unknown).unwrap()).is_err());
    let duplicate = format!(
        "{{\"creator\":1,{}",
        std::str::from_utf8(&bytes[1..]).unwrap()
    );
    assert!(Record::decode(duplicate.as_bytes()).is_err());
    let mut bad: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    bad["facts"]["after"][0] = u64::from(record.facts.after[0] ^ 0x80).into();
    assert!(Record::decode(&node_mutation::canonical(&bad).unwrap()).is_err());
    let changes: [fn(&mut Facts); 16] = [
        |f| f.version = 2,
        |f| f.epoch = 0,
        |f| f.namespace = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into(),
        |f| f.pool = Uuid::nil().to_string(),
        |f| f.operation = Uuid::nil().to_string(),
        |f| f.source.volume = Uuid::nil().to_string(),
        |f| f.source.scan = Uuid::nil().to_string(),
        |f| f.source.partition = 'x',
        |f| f.source.block = 0,
        |f| f.source.scan_generation = f.source.generation + 1,
        |f| f.source.assignment_revision = 0,
        |f| f.highest_before = 0,
        |f| f.highest_after = 1,
        |f| f.directories = 0,
        |f| f.source_files = f.files + 1,
        |f| f.request = "not-a-reference".into(),
    ];
    for change in changes {
        let mut facts = record.facts.clone();
        change(&mut facts);
        assert!(Record::new(facts, 1).is_err());
    }
}
