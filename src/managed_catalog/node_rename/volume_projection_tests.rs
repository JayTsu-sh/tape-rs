use super::super::physical_source::tests::{Fixture, fixture};
use super::*;
use crate::ltfs::index::NodeMeta;

pub(crate) fn build(f: &mut Fixture, budget: u64) -> Result<NodeRenameVolumeProjection> {
    let request = NodeRenameRequest::from_baseline(&f.baseline, "2026-11-02T10:00:00.000000000Z")?;
    let mut source = f
        .guard
        .physical_source(&f.baseline, &mut f.projection, &f.index, 16 << 20)?;
    let highest = source.highest_uid();
    let volume = source.source_index().volume_uuid;
    let expected_files = f.guard.stats().source_files;
    let expected_directories = f.projection.stats().directories;
    let mut builder = Builder::new(request, &mut source, &mut f.guard, highest, budget)?;
    let ancestor = NodeMutationAncestor {
        path: "/".into(),
        id: f.baseline.source_parent.id,
        version: 1,
        attributes: f
            .baseline
            .source_parent
            .attributes
            .as_ref()
            .unwrap()
            .clone(),
    };
    builder.ancestor(&ancestor, true)?;
    builder.ancestor(&ancestor, true)?;
    if parent(&f.baseline.spec.to) != "/" {
        let destination = NodeMutationAncestor {
            path: parent(&f.baseline.spec.to).into(),
            id: f.baseline.destination_parent.id,
            version: f.baseline.destination_parent.version,
            attributes: f
                .baseline
                .destination_parent
                .attributes
                .as_ref()
                .unwrap()
                .clone(),
        };
        builder.ancestor(&destination, true)?;
    }
    builder.target(&f.baseline)?;
    let mut after = None;
    loop {
        let page = f
            .projection
            .directories_page(after.as_deref(), 32, 8 << 20)?;
        for row in page.items {
            let ResolvedPath::Directory { attributes, .. } = &row.node else {
                unreachable!()
            };
            builder.directory(&row, attributes.clone().unwrap())?;
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let mut after = None;
    loop {
        let page = f
            .projection
            .volume_files_page(volume, after.as_deref(), 32, 8 << 20)?;
        for row in page.items {
            let ResolvedPath::File { attributes, .. } = &row.node else {
                unreachable!()
            };
            builder.file(&row, attributes.clone().unwrap())?;
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    builder.finish(expected_files, expected_directories)
}
fn stale(name: &str, uid: u64) -> FileNode {
    FileNode {
        name: name.into(),
        meta: NodeMeta {
            file_uid: uid,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn volume_projection_moves_only_original_current_nodes_and_preserves_outside_shared_data() {
    let mut f = fixture();
    f.index.root.subdirs[0].files.push(stale("deleted", 99));
    f.index.root.files.push(stale("dst", 98));
    f.index.root.xattrs.push(Xattr {
        key: "volume.private".into(),
        value: "preserved".into(),
        base64: false,
    });
    let before = f.index.to_xml().unwrap();
    let data = f.index.find_file("/.LTFSEE_DATA/object").unwrap().clone();
    let other = f.index.find_file("/other").unwrap().clone();
    let native = f.index.find_file("/native").unwrap().clone();
    let high = f.index.find_file("/high").unwrap().clone();
    let view = build(&mut f, 16 << 20).unwrap();
    assert_eq!(
        (
            view.stats().source_files,
            view.stats().source_directories,
            view.stats().highest_uid
        ),
        (2, 1, u64::MAX)
    );
    assert_eq!(view.file(22).unwrap().unwrap().0, "/dst/a");
    assert_eq!(view.file(23).unwrap().unwrap().0, "/dst/b");
    assert_eq!(
        view.file(11).unwrap().unwrap(),
        ("/.LTFSEE_DATA/object".into(), data)
    );
    assert_eq!(view.file(24).unwrap().unwrap(), ("/other".into(), other));
    assert_eq!(view.file(25).unwrap().unwrap(), ("/native".into(), native));
    assert_eq!(
        view.file(u64::MAX).unwrap().unwrap(),
        ("/high".into(), high)
    );
    assert!(view.file(98).unwrap().is_none());
    assert!(view.file(99).unwrap().is_none());
    assert!(view.directory("/src").unwrap().is_none());
    let root = view.directory("/").unwrap().unwrap();
    assert_eq!(root.meta().unwrap().file_uid, 1);
    assert_eq!(
        root.meta().unwrap().modify_time,
        "2026-11-02T10:00:00.000000000Z"
    );
    assert_eq!(root.xattrs().unwrap(), f.index.root.xattrs);
    assert_eq!(
        view.directory("/dst")
            .unwrap()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        7
    );
    assert_eq!(f.index.to_xml().unwrap(), before);
    assert!(view.conn.execute("DELETE FROM nodes", []).is_err());
    f.index.root.files.reverse();
    f.index.root.subdirs.reverse();
    for d in &mut f.index.root.subdirs {
        d.files.reverse();
    }
    let reordered = build(&mut f, 16 << 20).unwrap();
    assert_eq!(reordered.stats().after, view.stats().after);
    view.conn.pragma_update(None, "query_only", false).unwrap();
    let original_sha: Vec<u8> = view
        .conn
        .query_row(
            "SELECT sha FROM nodes WHERE uid=?",
            [22u64.to_be_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    view.conn
        .execute(
            "UPDATE nodes SET sha=zeroblob(32) WHERE uid=?",
            [22u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(view.file(22).is_err());
    assert!(view.verify().is_err());
    view.conn
        .execute(
            "UPDATE nodes SET sha=? WHERE uid=?",
            rusqlite::params![original_sha, 22u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(view.verify().is_ok());
    view.conn
        .execute(
            "DELETE FROM nodes WHERE uid=?",
            [24u64.to_be_bytes().as_slice()],
        )
        .unwrap();
    assert!(view.verify().is_err());
}

#[test]
fn volume_projection_fails_full_budget_without_changing_original_source_or_guard() {
    let mut f = fixture();
    for i in 100..1100 {
        f.index.root.files.push(stale(&format!("outside-{i}"), i));
    }
    let before = f.index.to_xml().unwrap();
    let guard = f.guard.stats().clone();
    let canonical = f.projection.stats().clone();
    assert!(build(&mut f, 64 << 10).is_err());
    assert_eq!(f.index.to_xml().unwrap(), before);
    assert_eq!(f.guard.stats(), &guard);
    assert_eq!(f.projection.stats(), &canonical);
    assert!(build(&mut f, 16 << 20).is_ok());
}

#[test]
fn volume_projection_rebases_only_ee_links_materializes_empty_dirs_and_updates_both_parents() {
    use super::super::live_guard::tests::file;
    use crate::ltfs::index::DirectoryNode;
    use NodeRenameReferenceRole::{Other, Source};
    let mut f = fixture();
    let binding = f.guard.original_projection_binding().2;
    f.baseline.spec.to = "/deep/moved".into();
    let deep = Uuid::new_v4();
    let canonical_parent = NodeAttributes::new(
        NodeMeta {
            file_uid: 1000,
            creation_time: "2020-01-01T00:00:00.000000000Z".into(),
            ..Default::default()
        },
        vec![Xattr {
            key: "logical.deep".into(),
            value: "kept".into(),
            base64: false,
        }],
        None,
    )
    .unwrap();
    f.baseline.destination_parent = NodeMutationParent {
        id: deep,
        version: 1,
        attributes: Some(canonical_parent.clone()),
    };
    f.baseline.spec.destination_parent.inode = deep;
    let request =
        NodeRenameRequest::from_baseline(&f.baseline, "2026-11-02T10:00:00.000000000Z").unwrap();
    let files = vec![
        file(&binding, 22, 11, Source, false),
        file(&binding, 23, 11, Source, false),
        file(&binding, 25, 25, Source, true),
        file(&binding, 24, 11, Other, false),
    ];
    f.index.root.files.retain(|n| n.meta.file_uid != u64::MAX);
    let native_position = f
        .index
        .root
        .files
        .iter()
        .position(|n| n.meta.file_uid == 25)
        .unwrap();
    let native = f.index.root.files.remove(native_position);
    let literal = native.symlink.clone();
    f.index.root.subdirs[0].files.push(native);
    f.index.root.subdirs.push(DirectoryNode {
        name: "deep".into(),
        meta: NodeMeta {
            file_uid: 50,
            ..Default::default()
        },
        ..Default::default()
    });
    f.index.highest_file_uid = 100;
    let empty_attrs = NodeAttributes::new(
        NodeMeta {
            file_uid: 9000,
            ..Default::default()
        },
        vec![],
        None,
    )
    .unwrap();
    let mut rows = vec![
        NodeRenameSubtreeEntry {
            path: "/src".into(),
            destination: "/deep/moved".into(),
            node: f.baseline.source.clone(),
        },
        NodeRenameSubtreeEntry {
            path: "/src/empty".into(),
            destination: "/deep/moved/empty".into(),
            node: ResolvedPath::Directory {
                id: Uuid::new_v4(),
                version: 1,
                attributes: Some(empty_attrs),
            },
        },
    ];
    for (name, (reference, copy)) in ["a", "b", "native"].into_iter().zip(&files) {
        let physical = f.index.find_file(&format!("/src/{name}")).unwrap();
        rows.push(NodeRenameSubtreeEntry {
            path: format!("/src/{name}"),
            destination: format!("/deep/moved/{name}"),
            node: ResolvedPath::File {
                id: reference.node.inode,
                head: FileHead {
                    version: 1,
                    size: copy.size,
                    copy_id: reference.copy_id,
                },
                copy: Box::new(copy.clone()),
                attributes: Some(NodeAttributes::from_file(physical).unwrap()),
                placement: binding.placement.clone(),
                object_metadata: None,
            },
        });
    }
    let summary = NodeRenameSubtreeSummary {
        entries: rows.len() as u64,
        files: 3,
        directories: 2,
        encoded_bytes: rows
            .iter()
            .map(|r| r.encode().unwrap().len() as u64 + 8)
            .sum(),
        sha256: [9; 32],
        complete: true,
    };
    let mut canonical = NodeRenameProjectionSpool::new(&request, &summary, 16 << 20).unwrap();
    canonical.append(&rows).unwrap();
    canonical.seal().unwrap();
    f.projection = canonical;
    let mut guard =
        NodeRenameVolumeGuard::new(&request, summary.sha256, binding, 16 << 20).unwrap();
    guard.append(&files).unwrap();
    guard.seal(3, 0).unwrap();
    f.guard = guard;
    let original = f.index.to_xml().unwrap();
    let view = build(&mut f, 16 << 20).unwrap();
    assert_eq!(view.stats().highest_uid, 101);
    assert_eq!(
        view.directory("/deep/moved/empty")
            .unwrap()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        101
    );
    assert_eq!(
        view.directory("/deep/moved")
            .unwrap()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        7
    );
    for uid in [22, 23] {
        assert_eq!(
            view.file(uid).unwrap().unwrap().1.symlink.as_deref(),
            Some("../../.LTFSEE_DATA/object")
        );
    }
    assert_eq!(view.file(25).unwrap().unwrap().1.symlink, literal);
    assert_eq!(view.file(24).unwrap().unwrap().0, "/other");
    let root = view.directory("/").unwrap().unwrap();
    let destination = view.directory("/deep").unwrap().unwrap();
    assert_eq!(root.meta().unwrap().modify_time, request.time());
    assert_eq!(destination.meta().unwrap().modify_time, request.time());
    assert_eq!(
        destination.meta().unwrap().creation_time,
        canonical_parent.meta().unwrap().creation_time
    );
    assert_eq!(destination.meta().unwrap().file_uid, 50);
    assert_eq!(destination.xattrs(), canonical_parent.xattrs());
    assert_eq!(f.index.to_xml().unwrap(), original);
}

#[test]
fn volume_projection_refuses_corrupt_component_order_even_when_row_bodies_are_unchanged() {
    let mut f = fixture();
    let view = build(&mut f, 16 << 20).unwrap();
    view.verify().unwrap();
    view.conn.pragma_update(None, "query_only", false).unwrap();
    view.conn
        .execute("UPDATE nodes SET tree_key=x'ff' WHERE path='/dst'", [])
        .unwrap();
    assert!(view.verify().is_err());
    assert!(tree_from_rows(&view, "root".into()).is_err());
}
