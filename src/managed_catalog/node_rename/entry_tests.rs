use super::*;
use crate::ltfs::index::{NodeMeta, Xattr};
use serde_json::{Value, json};

fn attributes(uid: u64, target: Option<&str>) -> NodeAttributes {
    NodeAttributes::new(
        NodeMeta {
            file_uid: uid,
            ..Default::default()
        },
        vec![],
        target.map(str::to_string),
    )
    .unwrap()
}
fn directory() -> NodeRenameSubtreeEntry {
    NodeRenameSubtreeEntry {
        path: "/src".into(),
        destination: "/dst".into(),
        node: ResolvedPath::Directory {
            id: Uuid::from_bytes([0xab; 16]),
            version: 3,
            attributes: Some(attributes(7, None)),
        },
    }
}
fn file() -> NodeRenameSubtreeEntry {
    let volume = Uuid::new_v4();
    NodeRenameSubtreeEntry {
        path: "/src/file".into(),
        destination: "/dst/file".into(),
        node: ResolvedPath::File {
            id: Uuid::new_v4(),
            head: FileHead {
                version: 4,
                size: 4,
                copy_id: Uuid::new_v4(),
            },
            copy: Box::new(PhysicalCopy {
                version: 4,
                origin: CopyOrigin::FileRequest { request_epoch: 7 },
                volume_id: volume,
                volume_generation: 2,
                volume_scan_id: Uuid::new_v4(),
                ltfs_file_uid: 11,
                size: 4,
                content_digest: ContentDigest::Sha256([5; 32]),
                media: None,
            }),
            placement: VolumePlacement {
                volume_id: volume,
                namespace: Uuid::new_v4(),
                pool_id: Uuid::new_v4(),
                library_serial: "SIM-ENTRY".into(),
                barcode: "ENTRY0L8".into(),
            },
            attributes: Some(attributes(11, None)),
            object_metadata: Some(ObjectMetadata {
                modified: Some(1_798_000_000),
                headers: BTreeMap::from([(
                    "content-type".into(),
                    "application/octet-stream".into(),
                )]),
            }),
        },
    }
}
fn mutate(row: &NodeRenameSubtreeEntry, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(&row.encode().unwrap()).unwrap();
    edit(&mut value);
    node_mutation::canonical(&value).unwrap()
}

#[test]
fn inventory_rows_roundtrip_directories_native_links_aliases_digests_and_attribute_references() {
    let mut rows = vec![directory(), file()];
    let mut md5 = file();
    if let ResolvedPath::File { copy, .. } = &mut md5.node {
        copy.content_digest = ContentDigest::Md5([2; 16]);
    }
    rows.push(md5);
    let mut link = file();
    if let ResolvedPath::File {
        head,
        copy,
        attributes: a,
        ..
    } = &mut link.node
    {
        head.size = 0;
        copy.size = 0;
        copy.content_digest = ContentDigest::Unknown;
        copy.origin = CopyOrigin::NodeLink {
            management_epoch: 9,
            operation: Uuid::new_v4(),
        };
        copy.media = Some(MediaProvenance {
            version: (9, 1),
            logical_file_uid: 11,
            physical_path: "old/native-link".into(),
            ee_alias: false,
            native_symlink: true,
        });
        *a = Some(attributes(11, Some("../target")));
    }
    rows.push(link);
    let mut alias = file();
    if let ResolvedPath::File {
        copy,
        attributes: a,
        ..
    } = &mut alias.node
    {
        copy.origin = CopyOrigin::Imported;
        copy.media = Some(MediaProvenance {
            version: (3, 4),
            logical_file_uid: 22,
            physical_path: ".LTFSEE_DATA/object".into(),
            ee_alias: true,
            native_symlink: false,
        });
        *a = Some(attributes(22, Some(".LTFSEE_DATA/object")));
    }
    rows.push(alias);
    let mut native = file();
    if let ResolvedPath::File { copy, .. } = &mut native.node {
        copy.origin = CopyOrigin::Imported;
        copy.content_digest = ContentDigest::Unknown;
        copy.media = Some(MediaProvenance {
            version: (1, 2),
            logical_file_uid: 11,
            physical_path: "old/data".into(),
            ee_alias: false,
            native_symlink: false,
        });
    }
    rows.push(native);
    let large = NodeAttributes::new(
        NodeMeta {
            file_uid: 7,
            ..Default::default()
        },
        vec![Xattr {
            key: "opaque".into(),
            value: "原".repeat(40_000),
            base64: false,
        }],
        None,
    )
    .unwrap();
    let mut external = directory();
    if let ResolvedPath::Directory { attributes: a, .. } = &mut external.node {
        *a = Some(NodeAttributes::from_manifest(large.manifest().unwrap()).unwrap());
    }
    rows.push(external);
    for row in rows {
        let bytes = row.encode().unwrap();
        let decoded = NodeRenameSubtreeEntry::decode(&bytes).unwrap();
        assert_eq!(decoded, row);
        assert_eq!(decoded.encode().unwrap(), bytes);
    }
}

#[test]
fn inventory_decoder_rejects_noncanonical_or_mismatched_node_identity_and_copy_fields() {
    let dir = directory();
    let data = file();
    let cases = vec![
        mutate(&dir, |v| v["version"] = json!(2)),
        mutate(&dir, |v| v["extra"] = json!(1)),
        mutate(&dir, |v| v["node"]["extra"] = json!(1)),
        mutate(&dir, |v| v["node"]["id"] = json!(Uuid::nil().to_string())),
        mutate(&dir, |v| {
            v["node"]["id"] = json!(Uuid::from_bytes([0xab; 16]).to_string().to_uppercase())
        }),
        mutate(&dir, |v| v["node"]["version"] = json!(0)),
        mutate(&dir, |v| v["node"]["attributes"] = json!(vec![0u8; 32])),
        mutate(&dir, |v| {
            v["object_metadata"] = json!({"modified":null,"headers":{}})
        }),
        mutate(&dir, |v| v["attributes"]["extra"] = json!(1)),
        mutate(&data, |v| {
            v["node"]["head"]["copy"] = json!(Uuid::nil().to_string())
        }),
        mutate(&data, |v| v["node"]["head"]["version"] = json!(5)),
        mutate(&data, |v| v["node"]["head"]["size"] = json!(5)),
        mutate(&data, |v| v["node"]["copy"]["origin"] = json!([1, 0])),
        mutate(&data, |v| v["node"]["copy"]["origin"] = json!([2, 0])),
        mutate(&data, |v| v["node"]["copy"]["uid"] = json!(0)),
        mutate(&data, |v| {
            v["node"]["copy"]["volume"] = json!(Uuid::nil().to_string())
        }),
        mutate(&data, |v| {
            v["node"]["copy"]["scan"] = json!(Uuid::nil().to_string())
        }),
        mutate(&data, |v| v["node"]["copy"]["digest_kind"] = json!(0)),
        mutate(&data, |v| v["node"]["copy"]["digest_kind"] = json!(2)),
        mutate(&data, |v| {
            v["node"]["placement"]["library"] = json!("bad/library")
        }),
        mutate(&data, |v| {
            v["node"]["placement"]["pool"] = json!(Uuid::nil().to_string())
        }),
        mutate(&data, |v| {
            v["node"]["placement"]["barcode"] = json!("bad/barcode")
        }),
        mutate(&data, |v| {
            v["object_metadata"]["headers"] = json!({"CONTENT-TYPE":"text/plain"})
        }),
        mutate(&data, |v| {
            v["object_metadata"]["headers"] = json!({"content-type":"bad\r\nvalue"})
        }),
    ];
    for bytes in cases {
        assert!(
            NodeRenameSubtreeEntry::decode(&bytes).is_err(),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
    let bytes = dir.encode().unwrap();
    assert!(NodeRenameSubtreeEntry::decode(&bytes[..bytes.len() - 1]).is_err());
    let duplicate = format!(
        "{{\"version\":1,{}",
        &String::from_utf8(bytes.clone()).unwrap()[1..]
    );
    assert!(NodeRenameSubtreeEntry::decode(duplicate.as_bytes()).is_err());
    let mut missing: Value = serde_json::from_slice(&bytes).unwrap();
    missing.as_object_mut().unwrap().remove("object_metadata");
    assert!(NodeRenameSubtreeEntry::decode(&node_mutation::canonical(&missing).unwrap()).is_err());
    assert!(NodeRenameSubtreeEntry::decode(&vec![b' '; LIMIT + 1]).is_err());
}

#[test]
fn inventory_decoder_rejects_bad_paths_and_attribute_reference_shapes() {
    let row = directory();
    for bad in ["/", "relative", "/a//b", "/a/../b", "/a/\u{1}"] {
        for field in ["path", "destination"] {
            assert!(
                NodeRenameSubtreeEntry::decode(&mutate(&row, |v| v[field] = json!(bad))).is_err()
            );
        }
    }
    let deep = format!("/{}", vec!["x"; usize::from(MAX_DEPTH)].join("/"));
    assert!(
        NodeRenameSubtreeEntry::decode(&mutate(&row, |v| v["destination"] = json!(deep))).is_err()
    );
    for storage in [
        json!({"digest":vec![0u8;32],"len":64,"chunks":1}),
        json!({"digest":vec![0u8;32],"len":120000,"chunks":0}),
        json!({"inline":"!bad"}),
        json!({"inline":"AA","digest":vec![0u8;32],"len":120000,"chunks":2}),
    ] {
        assert!(
            NodeRenameSubtreeEntry::decode(&mutate(&row, |v| v["attributes"] = storage)).is_err()
        );
    }
}

#[test]
fn inventory_frame_decoder_checks_eof_original_path_mapping_and_placement_scope() {
    let row = directory();
    let parent = Uuid::new_v4();
    let namespace = Uuid::new_v4();
    let pool = Uuid::new_v4();
    let revision = |inode, version| NodeRenameRevision {
        namespace,
        inode,
        version,
    };
    let source = revision(
        match row.node {
            ResolvedPath::Directory { id, .. } => id,
            _ => unreachable!(),
        },
        3,
    );
    let parent_snapshot = NodeMutationParent {
        id: parent,
        version: 1,
        attributes: Some(attributes(1, None)),
    };
    let baseline = NodeRenameBaseline {
        library_serial: "SIM-ENTRY".into(),
        owner_epoch: 1,
        request_epoch: 1,
        spec: NodeRenameSpec {
            pool,
            operation: Uuid::new_v4(),
            from: "/src".into(),
            to: "/dst".into(),
            source,
            source_parent: revision(parent, 1),
            destination_parent: revision(parent, 1),
            target: None,
            no_replace: true,
        },
        source_parent: parent_snapshot.clone(),
        destination_parent: parent_snapshot,
        source: row.node.clone(),
        target: None,
    };
    let request =
        NodeRenameRequest::from_baseline(&baseline, "2026-11-02T10:00:00.000000000Z").unwrap();
    let frame = |row: &NodeRenameSubtreeEntry| {
        let bytes = row.encode().unwrap();
        let mut f = (bytes.len() as u64).to_be_bytes().to_vec();
        f.extend(bytes);
        f
    };
    let good = frame(&row);
    assert_eq!(
        decode_entries(&good, 1, &request).unwrap().as_slice(),
        std::slice::from_ref(&row)
    );
    assert!(decode_entries(&good[..good.len() - 1], 1, &request).is_err());
    assert!(decode_entries(&good, 0, &request).is_err());
    assert!(decode_entries(&good, 1001, &request).is_err());
    let mut wrong = row.clone();
    wrong.path = "/src-other".into();
    assert!(decode_entries(&frame(&wrong), 1, &request).is_err());
    let mut wrong = row.clone();
    wrong.destination = "/dst-other".into();
    assert!(decode_entries(&frame(&wrong), 1, &request).is_err());
    let mut wrong = file();
    assert!(decode_entries(&frame(&wrong), 1, &request).is_err());
    if let ResolvedPath::File { placement, .. } = &mut wrong.node {
        placement.namespace = namespace;
        placement.pool_id = pool;
    }
    assert_eq!(
        decode_entries(&frame(&wrong), 1, &request).unwrap(),
        [wrong]
    );
}

#[test]
fn inventory_paths_obey_the_same_pool_relative_byte_limit_on_decode() {
    let row = directory();
    let boundary = format!("/{}", vec!["x".repeat(255); 8].join("/"));
    assert_eq!(boundary.len(), 2048);
    let pool = Uuid::new_v4();
    assert!(pool_catalog_path(pool, boundary.as_bytes()).is_ok());
    let overlong = format!("{boundary}/z");
    assert!(pool_catalog_path(pool, overlong.as_bytes()).is_err());
    for field in ["path", "destination"] {
        assert!(
            NodeRenameSubtreeEntry::decode(&mutate(&row, |v| v[field] = json!(boundary))).is_ok()
        );
        assert!(
            NodeRenameSubtreeEntry::decode(&mutate(&row, |v| v[field] = json!(overlong))).is_err(),
            "decoder accepted a path forbidden by the canonical pool model"
        );
    }
}
