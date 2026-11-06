use super::*;
fn group() -> GroupRecord {
    let mut record = GroupRecord {
        version: 1,
        library: "RENAME-ATTEMPT".into(),
        namespace: Uuid::new_v4().to_string(),
        pool: Uuid::new_v4().to_string(),
        epoch: u64::MAX,
        operation: Uuid::new_v4().to_string(),
        id: Uuid::new_v4().to_string(),
        owner: 12,
        created: 123,
        logical: [1; 32],
        global: [2; 32],
        volumes: 33,
        pages: 2,
        phase: NodeRenameAttemptPhase::Preparing,
        progress_revision: 0,
        completion: None,
        ready_pages: 0,
        ready_volumes: 0,
        ready_chain: [0; 32],
        closed_pages: 0,
        closed_time: None,
        closed_owner: None,
        checksum: [0; 32],
    };
    record.ready_chain = record.initial_chain().unwrap();
    record.seal().unwrap()
}
fn page(group: &GroupRecord, index: u64, count: u128, first: u128) -> ReadyPage {
    ReadyPage {
        version: 1,
        group: group.fingerprint().unwrap(),
        index,
        previous: group.ready_chain,
        intents: (first..first + count)
            .map(|i| IntentRef {
                volume: Uuid::from_u128(i).to_string(),
                id: Uuid::from_u128(i + 100).to_string(),
                fingerprint: [255; 32],
            })
            .collect(),
        checksum: [0; 32],
    }
    .seal()
    .unwrap()
}
#[test]
fn original_group_identity_survives_bounded_preparation_and_explicit_never_writing_closure() {
    let mut record = group();
    let fingerprint = record.fingerprint().unwrap();
    let original = record_bytes(&record).unwrap();
    assert_eq!(GroupRecord::decode(&original).unwrap(), record);
    let first = page(&record, 0, 32, 1);
    first.require_group(&record).unwrap();
    assert_eq!(
        ReadyPage::decode(&encode_value(&first).unwrap()).unwrap(),
        first
    );
    record.ready_pages = 1;
    record.ready_volumes = 32;
    record.ready_chain = first.chain().unwrap();
    record = record.seal().unwrap();
    assert_eq!(record.fingerprint().unwrap(), fingerprint);
    let last = page(&record, 1, 1, 33);
    last.require_group(&record).unwrap();
    record.ready_pages = 2;
    record.ready_volumes = 33;
    record.ready_chain = last.chain().unwrap();
    record.phase = NodeRenameAttemptPhase::Ready;
    record = record.seal().unwrap();
    assert_eq!(record.fingerprint().unwrap(), fingerprint);
    record.phase = NodeRenameAttemptPhase::ClosingPrepared;
    record.closed_pages = 1;
    record = record.seal().unwrap();
    record.phase = NodeRenameAttemptPhase::ClosedPrepared;
    record.closed_pages = 2;
    record.closed_time = Some(124);
    record.closed_owner = Some(13);
    record = record.seal().unwrap();
    assert_eq!(record.fingerprint().unwrap(), fingerprint);
    assert_eq!(
        GroupRecord::decode(&record_bytes(&record).unwrap()).unwrap(),
        record
    );
    assert_ne!(record_bytes(&record).unwrap(), original);
}
#[test]
fn attempt_codec_rejects_bad_progress_phases_checksums_and_noncanonical_records() {
    let record = group();
    let raw = record_bytes(&record).unwrap();
    for len in [0, 1, raw.len() - 1] {
        assert!(GroupRecord::decode(&raw[..len]).is_err());
    }
    let mut spaced = raw.clone();
    spaced.push(b'\n');
    assert!(GroupRecord::decode(&spaced).is_err());
    assert!(GroupRecord::decode(&vec![b' '; 8193]).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    value["checksum"][0] = (u64::from(record.checksum[0]) ^ 0x80).into();
    assert!(GroupRecord::decode(&node_mutation::canonical(&value).unwrap()).is_err());
    let changes: [fn(&mut GroupRecord); 12] = [
        |r| r.version = 2,
        |r| r.id = Uuid::nil().to_string(),
        |r| r.owner = 0,
        |r| r.owner = 1 << 48,
        |r| r.created = 0,
        |r| r.volumes = 0,
        |r| r.pages = 1,
        |r| r.ready_pages = 3,
        |r| r.ready_volumes = 1,
        |r| r.phase = NodeRenameAttemptPhase::Ready,
        |r| r.closed_pages = 1,
        |r| r.closed_owner = Some(13),
    ];
    for change in changes {
        let mut bad = record.clone();
        change(&mut bad);
        assert!(bad.seal().is_err());
    }
    let ready = page(&record, 0, 32, 1);
    let mut bad = ready.clone();
    bad.intents.reverse();
    assert!(bad.seal().is_err());
    let mut bad = ready.clone();
    bad.previous = [4; 32];
    let bad = bad.seal().unwrap();
    assert!(bad.require_group(&record).is_err());
}

#[test]
fn writing_group_requires_the_exact_task_kind_state_and_active_identity() {
    let mut record = group();
    record.ready_pages = record.pages;
    record.ready_volumes = record.volumes;
    record.ready_chain = [7; 32];
    record.phase = NodeRenameAttemptPhase::Writing;
    record.progress_revision = 1;
    let record = record.seal().unwrap();
    let namespace = record_id(&record.namespace).unwrap();
    let task = NodeMutationTask {
        kind: NodeMutationTaskKind::Rename,
        library_serial: record.library.clone(),
        namespace,
        pool: record_id(&record.pool).unwrap(),
        request_epoch: record.epoch,
        operation: record_id(&record.operation).unwrap(),
        object: NodePlanObject::new(&record.library, namespace, [3; 32], 1, 1).unwrap(),
        fingerprint: record.logical,
        state: NodeMutationTaskState::Writing,
        accepted_owner_epoch: record.owner,
        created_at_millis: record.created,
        prepared_owner_epoch: Some(record.owner),
        prepared_at_millis: Some(record.created),
        active_attempt: Some(record_id(&record.id).unwrap()),
        completed_owner_epoch: None,
        completed_at_millis: None,
        publication_digest: None,
        rejection_reason: None,
    };
    record.require_task(&task).unwrap();
    let mut bad = task.clone();
    bad.active_attempt = Some(Uuid::new_v4());
    assert!(record.require_task(&bad).is_err());
    bad.active_attempt = None;
    assert!(record.require_task(&bad).is_err());
    bad = task.clone();
    bad.kind = NodeMutationTaskKind::SingleNode;
    assert!(record.require_task(&bad).is_err());
    bad = task.clone();
    bad.state = NodeMutationTaskState::Prepared;
    bad.active_attempt = None;
    assert!(record.require_task(&bad).is_err());
    let mut unarmed = record.clone();
    unarmed.phase = NodeRenameAttemptPhase::Ready;
    unarmed.progress_revision = 0;
    let unarmed = unarmed.seal().unwrap();
    unarmed.require_task(&bad).unwrap();
    assert!(unarmed.require_task(&task).is_err());
    let mut incomplete = record;
    incomplete.ready_pages -= 1;
    incomplete.ready_volumes = 32;
    assert!(incomplete.seal().is_err());
}

#[test]
fn rebased_lineage_keeps_the_original_anchor_and_rejects_missing_or_cyclic_identities() {
    let group = group();
    let root = IntentRecord {
        version: 1,
        library: group.library.clone(),
        namespace: group.namespace.clone(),
        pool: group.pool.clone(),
        epoch: group.epoch,
        operation: group.operation.clone(),
        group: group.id.clone(),
        volume: Uuid::new_v4().to_string(),
        id: Uuid::new_v4().to_string(),
        owner: group.owner,
        created: group.created,
        logical: group.logical,
        global: group.global,
        recipe: [3; 32],
        physical: [4; 32],
        page: 0,
        pool_revision: 1,
        assignment_revision: 1,
        lineage: IntentLineage::Original,
        checksum: [0; 32],
    }
    .seal()
    .unwrap();
    let reference = root.original_reference().unwrap();
    let mut next = root.clone();
    next.id = Uuid::new_v4().to_string();
    next.owner += 1;
    next.created += 1;
    next.lineage = IntentLineage::Rebased {
        root: reference.clone(),
        parent: ClosureRef {
            id: root.id.clone(),
            fingerprint: [5; 32],
        },
    };
    let next = next.seal().unwrap();
    assert_eq!(next.original_reference().unwrap(), reference);
    assert_ne!(next.fingerprint().unwrap(), root.fingerprint().unwrap());
    assert_eq!(
        IntentRecord::decode(&record_bytes(&next).unwrap()).unwrap(),
        next
    );
    let mut missing: serde_json::Value =
        serde_json::from_slice(&record_bytes(&root).unwrap()).unwrap();
    missing.as_object_mut().unwrap().remove("lineage");
    assert!(IntentRecord::decode(&node_mutation::canonical(&missing).unwrap()).is_err());
    for change in 0..4 {
        let mut bad = next.clone();
        let IntentLineage::Rebased { root, parent } = &mut bad.lineage else {
            unreachable!()
        };
        match change {
            0 => root.id = bad.id.clone(),
            1 => parent.id = bad.id.clone(),
            2 => root.volume = Uuid::new_v4().to_string(),
            _ => parent.id = Uuid::nil().to_string(),
        }
        assert!(bad.seal().is_err());
    }
}

#[test]
fn all_s4_seal_requires_a_writing_revision_and_keeps_original_group_identity() {
    let mut record = group();
    let fingerprint = record.fingerprint().unwrap();
    record.ready_pages = record.pages;
    record.ready_volumes = record.volumes;
    record.ready_chain = [9; 32];
    record.phase = NodeRenameAttemptPhase::Writing;
    assert!(record.clone().seal().is_err());
    record.progress_revision = 1;
    let record = record.seal().unwrap();
    let advanced = record.advance_writing().unwrap();
    assert_eq!(advanced.progress_revision, 2);
    assert_eq!(advanced.fingerprint().unwrap(), fingerprint);
    assert_ne!(advanced.checksum, record.checksum);
    let mut publishing = advanced.clone();
    publishing.phase = NodeRenameAttemptPhase::Publishing;
    assert!(publishing.clone().seal().is_err());
    publishing.completion = Some(CompletionSeal {
        revision: 1,
        chain: [8; 32],
        released_pages: 0,
        released_chain: sealing::initial_chain(&publishing).unwrap(),
    });
    assert!(publishing.clone().seal().is_err());
    publishing.completion.as_mut().unwrap().revision = 2;
    let publishing = publishing.seal().unwrap();
    assert_eq!(publishing.fingerprint().unwrap(), fingerprint);
    assert!(publishing.advance_writing().is_err());
    assert_eq!(
        GroupRecord::decode(&record_bytes(&publishing).unwrap()).unwrap(),
        publishing
    );
    let mut bad = publishing.clone();
    bad.phase = NodeRenameAttemptPhase::Writing;
    assert!(bad.seal().is_err());
    let mut overflow = advanced;
    overflow.progress_revision = u64::MAX;
    let overflow = overflow.seal().unwrap();
    assert!(overflow.advance_writing().is_err());
    let mut raw: serde_json::Value =
        serde_json::from_slice(&record_bytes(&publishing).unwrap()).unwrap();
    raw.as_object_mut().unwrap().remove("progress_revision");
    assert!(GroupRecord::decode(&node_mutation::canonical(&raw).unwrap()).is_err());
    let mut raw: serde_json::Value =
        serde_json::from_slice(&record_bytes(&publishing).unwrap()).unwrap();
    raw.as_object_mut().unwrap().remove("completion");
    assert!(GroupRecord::decode(&node_mutation::canonical(&raw).unwrap()).is_err());
}
