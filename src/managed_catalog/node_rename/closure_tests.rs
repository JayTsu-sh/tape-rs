use super::*;
use crate::managed_catalog::control::node_rename::{physical_source, volume_projection};
fn value() -> NodeRenameIntentClosure {
    let mut f = physical_source::tests::fixture();
    let view = volume_projection::tests::build(&mut f, 16 << 20).unwrap();
    let request = view.original_request();
    let object = NodePlanObject::for_bytes(
        request.library(),
        request.spec().source.namespace,
        &request.encode().unwrap(),
        8 << 20,
    )
    .unwrap();
    let facts = volume_plan::Facts::from_projection(
        &view,
        &URL_SAFE_NO_PAD.encode(object.encode()),
        [5; 32],
        [6; 32],
        1,
        10,
    )
    .unwrap();
    let recipe = NodeRenameVolumePlan {
        record: volume_plan::Record::new(facts, 1).unwrap(),
    };
    let binding = recipe.record.facts.binding().unwrap();
    let id = Uuid::new_v4();
    let plan = NodeRenameMediaPlan::new(recipe, id, 2).unwrap();
    let record = IntentRecord {
        version: 1,
        library: request.library().into(),
        namespace: request.spec().source.namespace.to_string(),
        pool: request.spec().pool.to_string(),
        epoch: request.request_epoch(),
        operation: request.spec().operation.to_string(),
        group: Uuid::new_v4().to_string(),
        volume: binding.source.volume_uuid.to_string(),
        id: id.to_string(),
        owner: 2,
        created: 11,
        logical: request.fingerprint(),
        global: [7; 32],
        recipe: plan.recipe().fingerprint(),
        physical: plan.fingerprint(),
        page: 0,
        pool_revision: binding.pool_revision,
        assignment_revision: binding.assignment_revision,
        lineage: IntentLineage::Original,
        checksum: [0; 32],
    }
    .seal()
    .unwrap();
    let count = plan.recipe().record.facts.before_files;
    NodeRenameIntentClosure {
        intent: NodeRenameVolumeIntent { record, plan },
        kind: NodeRenameAbsenceKind::Unchanged,
        current: binding.source,
        file_count: count,
        owner: 3,
        time: 12,
    }
}
#[test]
fn original_closure_codec_is_self_contained_strict_and_keeps_its_first_audit() {
    let value = value();
    require_budget(&value.intent).unwrap();
    let bytes = encode(&value).unwrap();
    assert_eq!(decode(&bytes).unwrap(), value);
    assert_eq!(fingerprint(&bytes), value.fingerprint());
    let mut discarded = value.clone();
    discarded.kind = NodeRenameAbsenceKind::Discarded;
    discarded.current.generation += 1;
    discarded.current.location.start_block += 8;
    discarded.current.incremental = false;
    assert_eq!(decode(&encode(&discarded).unwrap()).unwrap(), discarded);
    for end in [0, 1, bytes.len() - 1] {
        assert!(decode(&bytes[..end]).is_err());
    }
    let mut bad = bytes.clone();
    bad.push(b'\n');
    assert!(decode(&bad).is_err());
    assert!(decode(&vec![b' '; MAX_CLOSURE + 1]).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["unknown"] = true.into();
    assert!(decode(&node_mutation::canonical(&wire).unwrap()).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["checksum"][0] = (wire["checksum"][0].as_u64().unwrap() ^ 0x80).into();
    assert!(decode(&node_mutation::canonical(&wire).unwrap()).is_err());
    let duplicate = format!(
        "{{\"version\":1,{}",
        std::str::from_utf8(&bytes[1..]).unwrap()
    );
    assert!(decode(duplicate.as_bytes()).is_err());
}
#[test]
fn closure_cannot_relabel_s4_or_another_original_attempt_as_unchanged() {
    let value = value();
    let changes: [fn(&mut NodeRenameIntentClosure); 8] = [
        |v| v.current.generation += 1,
        |v| v.current.volume_uuid = Uuid::new_v4(),
        |v| v.file_count += 1,
        |v| v.owner = 1,
        |v| v.time = 10,
        |v| v.intent.record.id = Uuid::new_v4().to_string(),
        |v| v.intent.record.pool_revision += 1,
        |v| v.intent.record.operation = Uuid::new_v4().to_string(),
    ];
    for change in changes {
        let mut bad = value.clone();
        change(&mut bad);
        bad.intent.record = bad.intent.record.seal().unwrap();
        assert!(encode(&bad).is_err());
    }
}
