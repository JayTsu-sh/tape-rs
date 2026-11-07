use super::super::live_guard::tests::{file, setup};
use super::*;

fn meta(uid: u64) -> NodeMeta {
    NodeMeta {
        file_uid: uid,
        ..Default::default()
    }
}
fn attributes(uid: u64) -> NodeAttributes {
    NodeAttributes::new(meta(uid), vec![], None).unwrap()
}
fn native(name: &str, uid: u64) -> FileNode {
    FileNode {
        name: name.into(),
        meta: meta(uid),
        length: 4,
        xattrs: vec![Xattr {
            key: crate::ltfs::index::XATTR_SHA256.into(),
            value: "02".repeat(32),
            base64: false,
        }],
        ..Default::default()
    }
}
fn alias(name: &str, uid: u64, target: &str) -> FileNode {
    FileNode {
        name: name.into(),
        meta: meta(uid),
        symlink: Some(target.into()),
        ..Default::default()
    }
}
pub(crate) struct Fixture {
    pub(crate) baseline: NodeRenameBaseline,
    pub(crate) projection: NodeRenameProjectionSpool,
    pub(crate) guard: NodeRenameVolumeGuard,
    pub(crate) index: LtfsIndex,
}
pub(crate) fn fixture() -> Fixture {
    use NodeRenameReferenceRole::*;
    let (request, binding) = setup();
    let baseline = NodeRenameBaseline {
        library_serial: request.library().into(),
        owner_epoch: 1,
        request_epoch: 1,
        spec: request.spec().clone(),
        source_parent: NodeMutationParent {
            id: request.spec().source_parent.inode,
            version: 1,
            attributes: Some(attributes(7)),
        },
        destination_parent: NodeMutationParent {
            id: request.spec().destination_parent.inode,
            version: 1,
            attributes: Some(attributes(7)),
        },
        source: ResolvedPath::Directory {
            id: request.spec().source.inode,
            version: 1,
            attributes: Some(attributes(7)),
        },
        target: None,
    };
    assert!(request.matches_baseline(&baseline).unwrap());
    let files = vec![
        file(&binding, 22, 11, Source, false),
        file(&binding, 23, 11, Source, false),
        file(&binding, 24, 11, Other, false),
        file(&binding, 25, 25, Other, true),
        file(&binding, u64::MAX, u64::MAX, Other, false),
    ];
    let mut rows = vec![NodeRenameSubtreeEntry {
        path: "/src".into(),
        destination: "/dst".into(),
        node: baseline.source.clone(),
    }];
    for (name, (file, copy)) in ["a", "b"].into_iter().zip(&files) {
        let attr = NodeAttributes::new(
            meta(file.metadata_uid),
            vec![],
            Some("../.LTFSEE_DATA/object".into()),
        )
        .unwrap();
        rows.push(NodeRenameSubtreeEntry {
            path: format!("/src/{name}"),
            destination: format!("/dst/{name}"),
            node: ResolvedPath::File {
                id: file.node.inode,
                head: FileHead {
                    version: copy.version,
                    size: copy.size,
                    copy_id: file.copy_id,
                },
                copy: Box::new(copy.clone()),
                attributes: Some(attr),
                placement: binding.placement.clone(),
                object_metadata: None,
            },
        });
    }
    let summary = NodeRenameSubtreeSummary {
        entries: rows.len() as u64,
        files: 2,
        directories: 1,
        encoded_bytes: rows
            .iter()
            .map(|r| r.encode().unwrap().len() as u64 + 8)
            .sum(),
        sha256: [5; 32],
        complete: true,
    };
    let mut projection = NodeRenameProjectionSpool::new(&request, &summary, 16 << 20).unwrap();
    projection.append(&rows).unwrap();
    projection.seal().unwrap();
    let mut guard =
        NodeRenameVolumeGuard::new(&request, summary.sha256, binding.clone(), 16 << 20).unwrap();
    guard.append(&files).unwrap();
    guard.seal(2, 0).unwrap();
    let mut index = LtfsIndex::empty(
        binding.source.volume_uuid,
        "physical-source-test".into(),
        'b',
    );
    index.generation = binding.source.generation;
    index.self_location = binding.source.location;
    index.highest_file_uid = u64::MAX;
    index.root.meta = meta(1);
    index.root.subdirs = vec![
        DirectoryNode {
            name: "src".into(),
            meta: meta(7),
            files: vec![
                alias("a", 22, "../.LTFSEE_DATA/object"),
                alias("b", 23, "../.LTFSEE_DATA/object"),
            ],
            ..Default::default()
        },
        DirectoryNode {
            name: ".LTFSEE_DATA".into(),
            meta: meta(2),
            files: vec![native("object", 11)],
            xattrs: ee_internal::stamp(&[]).unwrap(),
            ..Default::default()
        },
    ];
    let mut link = alias("native", 25, "literal/../missing");
    link.xattrs = vec![link_kind::native_marker()];
    index.root.files = vec![
        alias("other", 24, ".LTFSEE_DATA/object"),
        link,
        native("high", u64::MAX),
    ];
    Fixture {
        baseline,
        projection,
        guard,
        index,
    }
}

#[test]
fn physical_source_preserves_shared_ee_data_unsigned_uids_and_order_independent_rows() {
    let mut f = fixture();
    let before = f.index.to_xml().unwrap();
    let proof = f
        .guard
        .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20)
        .unwrap();
    assert_eq!(
        (
            proof.stats().source_files,
            proof.stats().other_files,
            proof.stats().referenced_data_files
        ),
        (2, 3, 2)
    );
    assert_eq!(proof.file(22).unwrap().unwrap().0, "/src/a");
    assert_eq!(proof.file(11).unwrap().unwrap().0, "/.LTFSEE_DATA/object");
    assert_eq!(proof.file(u64::MAX).unwrap().unwrap().1.length, 4);
    assert!(proof.file(1).is_err());
    assert!(proof.file(0).is_err());
    assert!(proof.file(100).unwrap().is_none());
    assert!(proof.conn.execute("DELETE FROM nodes", []).is_err());
    assert_eq!(f.index.to_xml().unwrap(), before);
    f.index.root.files.reverse();
    f.index.root.subdirs.reverse();
    for d in &mut f.index.root.subdirs {
        d.files.reverse();
    }
    let reordered = f
        .guard
        .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20)
        .unwrap();
    assert_eq!(reordered.stats().sha256, proof.stats().sha256);
    proof.conn.pragma_update(None, "query_only", false).unwrap();
    let original_sha: Vec<u8> = proof
        .conn
        .query_row(
            "SELECT sha FROM nodes WHERE uid=?",
            [22u64.to_be_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    proof
        .conn
        .execute(
            "UPDATE nodes SET sha=zeroblob(32) WHERE uid=?",
            [22u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(proof.file(22).is_err());
    assert!(proof.verify().is_err());
    proof
        .conn
        .execute(
            "UPDATE nodes SET sha=? WHERE uid=?",
            rusqlite::params![original_sha, 22u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(proof.verify().is_ok());
    proof
        .conn
        .execute(
            "DELETE FROM nodes WHERE uid=?",
            [24u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(proof.verify().is_err());
}

#[test]
fn physical_source_refuses_missing_moved_corrupt_or_open_current_identities() {
    let mut f = fixture();
    let original = f.index.clone();
    let corruptions: [fn(&mut LtfsIndex); 11] = [
        |i| i.generation += 1,
        |i| i.unknown_elements.push("unknown".into()),
        |i| i.volume_lock_state = Some("locked".into()),
        |i| {
            i.root.files.remove(0);
        },
        |i| i.root.subdirs[0].files[0].name = "moved".into(),
        |i| i.root.subdirs[0].files[0].open_for_write = Some(true),
        |i| i.root.subdirs[0].files[0].symlink = Some("../.LTFSEE_DATA/wrong".into()),
        |i| i.root.subdirs[1].files[0].length += 1,
        |i| i.root.subdirs[1].files[0].xattrs[0].value = "03".repeat(32),
        |i| i.root.subdirs[0].files[0].meta.file_uid = 23,
        |i| {
            let child = i.root.subdirs[0].clone();
            i.root.subdirs.push(child)
        },
    ];
    for change in corruptions {
        f.index = original.clone();
        change(&mut f.index);
        assert!(
            f.guard
                .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20)
                .is_err()
        );
    }
    f.index = original;
    let mut changed = f.baseline.clone();
    changed.spec.operation = Uuid::new_v4();
    assert!(
        f.guard
            .physical_source(&changed, &mut f.projection, &f.index, 16 << 20)
            .is_err()
    );
    assert!(
        f.guard
            .physical_source(&f.baseline, &mut f.projection, &f.index, 1024)
            .is_err()
    );
}

#[test]
fn physical_source_refuses_a_moving_data_object_still_referenced_outside_the_subtree() {
    let mut f = fixture();
    // An outside alias must retain its exact physical data path. Renaming a
    // subtree containing that data object requires a separate relocation plan.
    let data = f.index.root.subdirs[1].files.remove(0);
    f.index.root.subdirs[0].files.push(data);
    f.index.root.files[0].symlink = Some("src/object".into());
    assert!(matches!(
        f.guard
            .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20),
        Err(CatalogError::Invalid(
            "rename 物理路径仍被范围外数据引用保护"
        ))
    ));
}

#[test]
fn physical_source_rejects_full_disk_budget_and_large_rows_without_mutating_inputs() {
    let mut f = fixture();
    let normal = f.index.clone();
    for i in 100..1100 {
        f.index.root.files.push(native(&format!("stale-{i}"), i));
    }
    let before = f.index.to_xml().unwrap();
    let view_before = f.projection.stats().clone();
    let guard_before = f.guard.stats().clone();
    assert!(
        f.guard
            .physical_source(&f.baseline, &mut f.projection, &f.index, 64 << 10)
            .is_err()
    );
    assert_eq!(f.index.to_xml().unwrap(), before);
    assert_eq!(f.projection.stats(), &view_before);
    assert_eq!(f.guard.stats(), &guard_before);
    f.index = normal;
    f.index.root.xattrs.push(Xattr {
        key: "oversize".into(),
        value: "x".repeat(9 << 20),
        base64: false,
    });
    assert!(matches!(
        f.guard
            .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20),
        Err(CatalogError::Invalid("rename 物理节点属性超过8MiB预算"))
    ));
    f.index.root.xattrs.pop();
    assert!(
        f.guard
            .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20)
            .is_ok()
    );
}
