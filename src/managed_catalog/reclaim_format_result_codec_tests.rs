//! Codec versions are distinct: result/goal stay v2, original parent is v1/v2.
use super::*;
use crate::ltfs::index::IndexLocation;
use crate::ltfs::volume::CommittedIndexConstruct;

fn value(parent_version: u8) -> ReclaimFormatResult {
    value_with_root(parent_version, false)
}

fn value_with_root(parent_version: u8, grouped_root: bool) -> ReclaimFormatResult {
    let namespace = Uuid::from_u128(1);
    let pool = Uuid::from_u128(2);
    let source = Uuid::from_u128(3);
    let output = Uuid::from_u128(7);
    let formatted_at = "2026-11-18T00:00:00.000000000Z";
    let records_for = |volume| {
        crate::ltfs::mkltfs::reset_partition_records(
            &crate::ltfs::mkltfs::MkltfsOptions {
                volume_uuid: Some(volume),
                quick: true,
                ..Default::default()
            },
            formatted_at,
            1,
        )
        .unwrap()
    };
    let before = records_for(source);
    let old = IndexSourceIdentity::new(
        CommittedIndexConstruct {
            volume_uuid: source,
            generation: 1,
            location: IndexLocation {
                partition: 'b',
                start_block: crate::ltfs::volume::FIRST_INDEX_BLOCK,
            },
            sha256: Sha256::digest(before[5].as_ref().unwrap()).into(),
            incremental: false,
        },
        Vec::new(),
    )
    .unwrap();
    let mut old_head = Vec::new();
    active_volume::encode_index(&mut old_head, old.head().into());
    let mut intent = json!({"version":parent_version,"library":"SIM-CODEC","namespace":namespace.to_string(),"epoch":1,"operation":Uuid::from_u128(4).to_string(),"change":{"kind":"media-reclaim","pool":pool.to_string(),"barcode":"CODEC00L8","volume":source.to_string(),"revision":1,"confirm_format":true,"unassign":false},"scan":Uuid::from_u128(5).to_string(),"scan_generation":1,"index":old_head});
    if grouped_root {
        intent["change"]["mode"] = json!("group-only");
        intent["group_index_protocol"] = json!(3);
    }
    let parent = format!("{:x}", Sha256::digest(intent.to_string().as_bytes()));
    let source_label = crate::ltfs::mkltfs::reset_partition_records(
        &crate::ltfs::mkltfs::MkltfsOptions {
            volume_uuid: Some(source),
            quick: true,
            ..Default::default()
        },
        formatted_at,
        0,
    )
    .unwrap()[2]
        .clone()
        .unwrap();
    let source_evidence=serde_json::from_value(json!({"parent":parent,"inventory":vec![2u8;32],"head":old_head,"identity":old.to_bytes(),"dp_eod":8,"label":source_label.to_vec(),"cartridge":{"manufacturer":*b"IBM     ","serial":format!("{:<32}","CODEC-ORIGINAL-CARTRIDGE").into_bytes()}})).unwrap();
    let after = records_for(output);
    let identity = IndexSourceIdentity::new(
        CommittedIndexConstruct {
            volume_uuid: output,
            generation: 1,
            location: IndexLocation {
                partition: 'b',
                start_block: crate::ltfs::volume::FIRST_INDEX_BLOCK,
            },
            sha256: Sha256::digest(after[5].as_ref().unwrap()).into(),
            incremental: false,
        },
        Vec::new(),
    )
    .unwrap();
    ReclaimFormatResult(Wire {
        version: 2,
        namespace: namespace.to_string(),
        library: "SIM-CODEC".into(),
        intent,
        goal: Goal {
            version: 2,
            parent,
            source: source.to_string(),
            output: output.to_string(),
            scan: Uuid::from_u128(6).to_string(),
            coverage: [3; 32],
            issued: true,
            attempt: 1,
            last_observation: None,
            formatted_at: formatted_at.into(),
            source_evidence,
        },
        owner: 7,
        source: identity.to_bytes(),
        returned_home: None,
        logical_files: 2,
        logical_bytes: 2 << 20,
    })
}

#[test]
fn grouped_root_parent_remains_bound_in_terminal_format_receipt() {
    let original = value_with_root(2, true);
    let raw = original.encode().unwrap();
    assert_eq!(ReclaimFormatResult::decode(&raw).unwrap(), original);
    assert!(value_with_root(1, true).encode().is_err());

    for marker in [json!(2), json!(4), Value::Null] {
        let mut changed = original.clone();
        changed.0.intent["group_index_protocol"] = marker;
        assert!(changed.encode().is_err());
    }
    let mut removed = original.clone();
    removed
        .0
        .intent
        .as_object_mut()
        .unwrap()
        .remove("group_index_protocol");
    assert!(
        removed.encode().is_err(),
        "a legal legacy intent still cannot replace the original fingerprint"
    );
    let mut extra = original.clone();
    extra.0.intent["unexpected"] = json!(true);
    assert!(extra.encode().is_err());
    let mut default = original;
    default.0.intent["change"]
        .as_object_mut()
        .unwrap()
        .remove("mode");
    assert!(default.encode().is_err());
}
#[test]
fn original_parent_v1_and_v2_roundtrip_without_changing_result_codec() {
    for version in [1, 2] {
        let original = value(version);
        let raw = original.encode().unwrap();
        if version == 1 {
            assert_eq!(
                format!("{:x}", Sha256::digest(&raw)),
                "8ea4d3f3b70896196761720b72e25d975cc20437fdedfa7a4b60e3f7f8f3ea84",
                "the original legal v1 bytes must stay identical"
            );
            eprintln!("FORMAT_RESULT_V1_SHA256={:x}", Sha256::digest(&raw));
        }
        assert_eq!(ReclaimFormatResult::decode(&raw).unwrap(), original);
        assert_eq!(original.0.intent["version"], version);
        assert_eq!(original.0.version, 2);
        assert_eq!(original.0.goal.version, 2);
    }
}
#[test]
fn unknown_versions_parent_rebinding_and_coverage_tamper_are_rejected() {
    for version in [0, 3, 255] {
        assert!(value(version).encode().is_err());
    }
    let valid = value(1);
    let raw = valid.encode().unwrap();
    let mut wrong = valid.clone();
    wrong.0.version = 1;
    assert!(wrong.encode().is_err());
    let mut wrong = valid.clone();
    wrong.0.goal.version = 1;
    assert!(wrong.encode().is_err());
    let mut wrong = valid.clone();
    wrong.0.intent["version"] = json!(2);
    assert!(
        wrong.encode().is_err(),
        "cannot relabel an original v1 parent without its fixed binding"
    );
    let mut corrupt: Value = serde_json::from_slice(&raw[..raw.len() - 32]).unwrap();
    corrupt["goal"]["coverage"][0] = json!(4);
    let mut changed = serde_json::to_vec(&corrupt).unwrap();
    changed.extend_from_slice(&raw[raw.len() - 32..]);
    assert!(
        ReclaimFormatResult::decode(&changed).is_err(),
        "coverage mutation must invalidate original receipt"
    );
}
