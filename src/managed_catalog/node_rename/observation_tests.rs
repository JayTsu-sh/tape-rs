use super::*;
use crate::managed_catalog::control::node_rename::{physical_source, volume_projection};

fn observation() -> NodeRenameObservation {
    let mut fixture = physical_source::tests::fixture();
    let view = volume_projection::tests::build(&mut fixture, 16 << 20).unwrap();
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
    let current = ActiveIndexIdentity {
        generation: binding.source.generation + 1,
        location: crate::ltfs::index::IndexLocation {
            partition: binding.source.location.partition,
            start_block: binding.source.location.start_block + 8,
        },
        sha256: [9; 32],
        incremental: false,
        ..binding.source
    };
    let file_count = plan.recipe().record.facts.files;
    NodeRenameObservation {
        intent: NodeRenameVolumeIntent { record, plan },
        current,
        file_count,
        owner: 3,
        time: 12,
    }
}
#[test]
fn rename_observation_is_self_contained_strict_and_preserves_original_audit() {
    let value = observation();
    require_observation_budget(&value.intent).unwrap();
    let bytes = encode_observation(&value).unwrap();
    assert_eq!(decode_observation(&bytes).unwrap(), value);
    for len in [0, 1, bytes.len() - 1] {
        assert!(decode_observation(&bytes[..len]).is_err());
    }
    let mut spaced = bytes.clone();
    spaced.push(b'\n');
    assert!(decode_observation(&spaced).is_err());
    assert!(decode_observation(&vec![b' '; MAX_OBSERVATION + 1]).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["unknown"] = true.into();
    assert!(decode_observation(&node_mutation::canonical(&wire).unwrap()).is_err());
    let duplicate = format!(
        "{{\"version\":1,{}",
        std::str::from_utf8(&bytes[1..]).unwrap()
    );
    assert!(decode_observation(duplicate.as_bytes()).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["checksum"][0] = (value_to_checksum(&bytes) ^ 0x80).into();
    assert!(decode_observation(&node_mutation::canonical(&wire).unwrap()).is_err());
}
fn value_to_checksum(bytes: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["checksum"][0]
        .as_u64()
        .unwrap()
}
#[test]
fn rename_observation_rejects_cross_scope_and_unqualified_physical_results() {
    let value = observation();
    let changes: [fn(&mut NodeRenameObservation); 15] = [
        |v| v.intent.record.library = "FOREIGN".into(),
        |v| v.intent.record.namespace = Uuid::new_v4().to_string(),
        |v| v.intent.record.pool = Uuid::new_v4().to_string(),
        |v| v.intent.record.epoch += 1,
        |v| v.intent.record.operation = Uuid::new_v4().to_string(),
        |v| v.intent.record.logical = [8; 32],
        |v| v.intent.record.pool_revision += 1,
        |v| v.intent.record.assignment_revision += 1,
        |v| v.current.volume_uuid = Uuid::new_v4(),
        |v| v.current.generation += 1,
        |v| v.current.location.start_block = 1,
        |v| v.current.incremental = true,
        |v| v.file_count += 1,
        |v| v.owner = 1,
        |v| v.time = 10,
    ];
    for change in changes {
        let mut bad = value.clone();
        change(&mut bad);
        bad.intent.record = bad.intent.record.seal().unwrap();
        assert!(encode_observation(&bad).is_err());
    }
}
