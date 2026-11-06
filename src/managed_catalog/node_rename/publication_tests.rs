use super::*;
fn fixture() -> NodeRenamePublication {
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let source = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let revision = |inode: Uuid, version: u64| serde_json::json!({"namespace":namespace.to_string(),"inode":inode.to_string(),"version":version});
    let wire = serde_json::json!({"version":1,"library":"RENAME-PUBLICATION","pool":pool.to_string(),"operation":operation.to_string(),"request_epoch":1,"from":"/old","to":"/new","source":revision(source,4),"source_parent":revision(parent,3),"destination_parent":revision(parent,3),"target":null,"no_replace":true,"time":"2026-10-23T00:00:00.000000000Z","baseline_sha256":vec![7_u8;32]});
    let request = NodeRenameRequest::decode(&node_mutation::canonical(&wire).unwrap()).unwrap();
    let group = GroupRecord {
        version: 1,
        library: request.library().into(),
        namespace: namespace.to_string(),
        pool: pool.to_string(),
        epoch: 1,
        operation: operation.to_string(),
        id: Uuid::new_v4().to_string(),
        owner: 13,
        created: 123,
        logical: request.fingerprint(),
        global: [1; 32],
        volumes: 1,
        pages: 1,
        phase: NodeRenameAttemptPhase::Publishing,
        progress_revision: 3,
        completion: Some(CompletionSeal {
            revision: 3,
            chain: [8; 32],
            released_pages: 1,
            released_chain: [8; 32],
        }),
        ready_pages: 1,
        ready_volumes: 1,
        ready_chain: [7; 32],
        closed_pages: 0,
        closed_time: None,
        closed_owner: None,
        checksum: [0; 32],
    }
    .seal()
    .unwrap();
    NodeRenamePublication {
        request,
        group,
        directory: true,
        before_copy: None,
        attributes: [2; 32],
        source_parent_attributes: [3; 32],
        destination_parent_attributes: [3; 32],
        replaced: None,
        owner: 14,
        time: 124,
    }
}
#[test]
fn retained_two_path_result_is_strict_and_requires_complete_original_release() {
    let value = fixture();
    let raw = encode(&value).unwrap();
    assert_eq!(decode(&raw).unwrap(), value);
    assert_eq!(value.source().version, 5);
    assert_eq!(value.source_parent(), value.destination_parent());
    assert_eq!(value.source_parent().version, 4);
    for len in [0, 1, raw.len() - 1] {
        assert!(decode(&raw[..len]).is_err());
    }
    let mut bad = raw.clone();
    bad.push(b'\n');
    assert!(decode(&bad).is_err());
    let mut bad = value.clone();
    bad.group.completion.as_mut().unwrap().released_pages = 0;
    bad.group.completion.as_mut().unwrap().released_chain =
        sealing::initial_chain(&bad.group).unwrap();
    bad.group = bad.group.seal().unwrap();
    assert!(encode(&bad).is_err());
    let mut bad = value.clone();
    bad.destination_parent_attributes = [4; 32];
    assert!(encode(&bad).is_err());
    let mut bad = value.clone();
    bad.owner = 12;
    assert!(encode(&bad).is_err());
    let mut bad = value.clone();
    bad.time = 122;
    assert!(encode(&bad).is_err());
    let mut bad = value.clone();
    bad.group.operation = Uuid::new_v4().to_string();
    bad.group = bad.group.seal().unwrap();
    assert!(encode(&bad).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    wire["extra"] = true.into();
    assert!(decode(&node_mutation::canonical(&wire).unwrap()).is_err());
}
#[test]
fn rename_copy_advances_only_metadata_version_for_original_upload_import_and_native_provenance() {
    for (origin, native) in [
        (CopyOrigin::FileRequest { request_epoch: 7 }, false),
        (CopyOrigin::Imported, false),
        (
            CopyOrigin::NodeLink {
                management_epoch: 9,
                operation: Uuid::new_v4(),
            },
            true,
        ),
    ] {
        let mut value = fixture();
        value.directory = false;
        let source = PhysicalCopy {
            version: 4,
            origin,
            volume_id: Uuid::new_v4(),
            volume_generation: 6,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 5,
            size: if native { 0 } else { 23 },
            content_digest: if native {
                ContentDigest::Unknown
            } else {
                ContentDigest::Sha256([9; 32])
            },
            media: Some(MediaProvenance {
                version: (12, 34),
                logical_file_uid: if native { 5 } else { 6 },
                physical_path: if native {
                    "original-link"
                } else {
                    ".LTFSEE_DATA/body"
                }
                .into(),
                ee_alias: !native,
                native_symlink: native,
            }),
        };
        let old_id = Uuid::new_v4();
        value.before_copy = Some((old_id, source.clone()));
        let raw = encode(&value).unwrap();
        let result = decode(&raw).unwrap();
        let (id, copy) = result.physical_copy().unwrap();
        assert_ne!(id, old_id);
        let mut expected = source;
        expected.version = 5;
        assert_eq!(copy, expected);
        assert_eq!(result.physical_copy(), value.physical_copy());
        assert_ne!(id, Uuid::nil());
    }
}
#[test]
fn completed_rename_matches_its_original_object_and_cannot_be_a_single_node_ack() {
    let value = fixture();
    let request = &value.request;
    let encoded = request.encode().unwrap();
    let task = NodeMutationTask {
        kind: NodeMutationTaskKind::Rename,
        library_serial: request.library().into(),
        namespace: request.spec().source.namespace,
        pool: request.spec().pool,
        request_epoch: request.request_epoch(),
        operation: request.spec().operation,
        object: NodePlanObject::for_bytes(
            request.library(),
            request.spec().source.namespace,
            &encoded,
            encoded.len(),
        )
        .unwrap(),
        fingerprint: request.fingerprint(),
        state: NodeMutationTaskState::Completed,
        accepted_owner_epoch: 13,
        created_at_millis: 100,
        prepared_owner_epoch: Some(13),
        prepared_at_millis: Some(110),
        active_attempt: None,
        completed_owner_epoch: Some(value.owner),
        completed_at_millis: Some(value.time),
        publication_digest: Some(value.fingerprint()),
        rejection_reason: None,
    };
    require_task_match(&value, &task).unwrap();
    let mut bad = task.clone();
    bad.kind = NodeMutationTaskKind::SingleNode;
    assert!(require_task_match(&value, &bad).is_err());
    let mut bad = task.clone();
    bad.publication_digest = Some([0; 32]);
    assert!(require_task_match(&value, &bad).is_err());
    let mut bad = task.clone();
    bad.completed_owner_epoch = Some(15);
    assert!(require_task_match(&value, &bad).is_err());
    let mut bad = task;
    bad.fingerprint = [0; 32];
    assert!(require_task_match(&value, &bad).is_err());
}

#[test]
fn publication_page_progress_cannot_finish_without_the_original_all_s4_chain() {
    let mut group = fixture().group;
    group.pages = 2;
    group.volumes = 33;
    group.ready_pages = 2;
    group.ready_volumes = 33;
    let initial = sealing::initial_chain(&group).unwrap();
    let complete = group.completion.as_mut().unwrap();
    complete.released_pages = 0;
    complete.released_chain = initial;
    let mut group = group.seal().unwrap();
    let identity = group.fingerprint().unwrap();
    group.completion.as_mut().unwrap().released_pages = 1;
    group.completion.as_mut().unwrap().released_chain = [6; 32];
    let mut group = group.seal().unwrap();
    assert_eq!(group.fingerprint().unwrap(), identity);
    group.completion.as_mut().unwrap().released_pages = 2;
    assert!(group.clone().seal().is_err());
    group.completion.as_mut().unwrap().released_chain = [8; 32];
    let mut group = group.seal().unwrap();
    assert_eq!(group.fingerprint().unwrap(), identity);
    group.completion.as_mut().unwrap().released_pages = 3;
    assert!(group.seal().is_err());
}
