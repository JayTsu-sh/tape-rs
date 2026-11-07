//! FDB node-mutation media component; SimLibrary only, no FDB server or devices.
#![cfg(feature = "fdb")]
use std::io::Cursor;
use tape_rs::ltfs::index::{LtfsIndex, Xattr};
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::managed_catalog::{
    NodeAttributes, NodeKind, NodeMutation, NodeMutationAbsenceKind, NodeMutationAncestor,
    NodeMutationContext, NodeMutationPlan, XattrMode,
};
use tape_rs::scsi::cdb::opcode;
use tape_rs::scsi::reservation::{ReservationKey, fence};
use tape_rs::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};
use uuid::Uuid;

const TIME: &str = "2026-10-31T08:00:00.000000000Z";
const MARKER: &str = "tapers.fdb.nodeMutation";
fn formatted() -> (SimLibrary, SimTransport) {
    let library = SimLibrary::new(1, 4, 1);
    library
        .insert_cartridge(SimCartridge::blank("NODE001L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("NODE001L8", 0).unwrap();
    let dev = library.drive(0);
    mkltfs(
        &dev,
        &MkltfsOptions {
            volume_id: "NOD001".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    fence(&dev, ReservationKey::new(1, 100)).unwrap();
    (library, dev)
}
fn mounted(dev: &SimTransport) -> LtfsVolume<'_> {
    let mut volume = LtfsVolume::mount(dev).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(1, 100)));
    volume
}
// Explicit physical-derived logical inputs are fixture data, not a catalog
// authority or a production fallback for missing canonical metadata.
fn prepare_canonical(
    context: NodeMutationContext,
    mutation: NodeMutation,
    index: &LtfsIndex,
    time: &str,
) -> tape_rs::managed_catalog::Result<NodeMutationPlan> {
    let parent = context
        .path
        .rsplit_once('/')
        .map(|p| if p.0.is_empty() { "/" } else { p.0 })
        .unwrap_or("/");
    let mut paths = vec!["/".to_owned()];
    let mut current = String::new();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        paths.push(current.clone());
    }
    let mut ancestors = Vec::new();
    for path in &paths {
        let Some(directory) = index.find_directory(path) else {
            break;
        };
        ancestors.push(NodeMutationAncestor {
            path: path.clone(),
            id: Uuid::from_u128(u128::from(directory.meta.file_uid) + 1000),
            version: 1,
            attributes: NodeAttributes::new(
                directory.meta.clone(),
                directory
                    .xattrs
                    .iter()
                    .filter(|attr| attr.key != MARKER)
                    .cloned()
                    .collect(),
                None,
            )?,
        });
    }
    if let Some(last) = ancestors.last_mut() {
        last.id = context.parent;
        last.version = context.parent_version;
    }
    let target = if let Some(file) = index.find_file(&context.path) {
        Some((NodeKind::File, NodeAttributes::from_file(file)?))
    } else if let Some(directory) = index.find_directory(&context.path) {
        Some((
            NodeKind::Directory,
            NodeAttributes::from_directory(directory)?,
        ))
    } else {
        None
    };
    NodeMutationPlan::prepare(
        context,
        mutation,
        index,
        time,
        &ancestors,
        target.as_ref().map(|(kind, attrs)| (*kind, attrs)),
    )
}
fn prepare(volume: &mut LtfsVolume<'_>, path: &str, mutation: NodeMutation) -> NodeMutationPlan {
    let creating = matches!(
        mutation,
        NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. }
    );
    let previous = volume.verified_committed_index_construct().unwrap().into();
    prepare_canonical(
        NodeMutationContext {
            library_serial: "SIM-NODES".into(),
            namespace: Uuid::new_v4(),
            pool: Uuid::new_v4(),
            operation: Uuid::new_v4(),
            request_epoch: 1,
            owner_epoch: 100,
            parent: Uuid::new_v4(),
            parent_version: 1,
            node: Uuid::new_v4(),
            expected_version: (!creating).then_some(1),
            scan: Uuid::new_v4(),
            previous,
            path: path.into(),
        },
        mutation,
        volume.index(),
        TIME,
    )
    .unwrap()
}
fn commit(volume: &mut LtfsVolume<'_>, plan: &NodeMutationPlan) {
    volume.stage_node_mutation(plan).unwrap();
    assert!(
        volume.node_mutation_evidence(plan).is_err(),
        "working changes have no S4"
    );
    volume.sync_batch().unwrap();
    let proof = volume.node_mutation_evidence(plan).unwrap();
    assert_eq!(proof.fingerprint(), plan.fingerprint());
    assert_eq!(
        proof.current().generation,
        plan.context().previous.generation + 1
    );
    assert_eq!(proof.context(), plan.context());
}
fn media(library: &SimLibrary) -> Vec<Vec<tape_rs::scsi::sim::LogicalObject>> {
    let mut snapshot = None;
    assert!(library.with_cartridge_mut("NODE001L8", |c| {
        snapshot = Some(c.partitions.iter().map(|p| p.objects.clone()).collect());
    }));
    snapshot.unwrap()
}
fn attr(key: &str, value: &str) -> Xattr {
    Xattr {
        key: key.into(),
        value: value.into(),
        base64: false,
    }
}

#[test]
fn unchanged_node_attempt_needs_clean_source_and_current_pr_without_writes() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/kept", &mut Cursor::new(b"kept"))
        .unwrap();
    volume.sync().unwrap();
    let prior = prepare(&mut volume, "/prior", NodeMutation::CreateDirectory);
    commit(&mut volume, &prior);
    let plan = prepare(&mut volume, "/wanted", NodeMutation::CreateDirectory);
    let before = media(&library);
    let proof = volume.node_mutation_absence_evidence(&plan).unwrap();
    assert_eq!(proof.kind(), NodeMutationAbsenceKind::Unchanged);
    assert_eq!(proof.current(), plan.context().previous);
    assert_eq!(proof.file_count(), 1);
    assert_eq!(proof.fingerprint(), plan.fingerprint());
    assert_eq!(proof.context(), plan.context());
    volume.stage_node_mutation(&plan).unwrap();
    assert!(volume.node_mutation_absence_evidence(&plan).is_err());
    assert!(volume.discard_node_mutation_tail(&plan).is_err());
    assert_eq!(media(&library), before);
    drop(volume);
    let mut volume = mounted(&dev);
    assert_eq!(volume.node_mutation_absence_evidence(&plan).unwrap(), proof);
    fence(&dev, ReservationKey::new(2, 101)).unwrap();
    assert!(volume.node_mutation_absence_evidence(&plan).is_err());
    drop(volume);
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    assert!(volume.node_mutation_absence_evidence(&plan).is_err());
    volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
    assert_eq!(volume.node_mutation_absence_evidence(&plan).unwrap(), proof);
    assert_eq!(media(&library), before);
}

#[test]
fn node_t1_t2_discard_keeps_the_source_and_requires_a_fresh_physical_plan() {
    use tape_rs::ltfs::recovery::TailKind;
    use tape_rs::tape::commands::TapeDrive;
    for unindexed in [false, true] {
        let (library, dev) = formatted();
        let mut volume = mounted(&dev);
        volume
            .append_file("/kept", &mut Cursor::new(b"retained-body"))
            .unwrap();
        volume.set_root_xattr("foreign.root", "retained").unwrap();
        volume.sync().unwrap();
        let prior = prepare(&mut volume, "/prior", NodeMutation::CreateDirectory);
        volume.stage_node_mutation(&prior).unwrap();
        if unindexed {
            volume.commit_incremental().unwrap();
        } else {
            volume.sync().unwrap();
        }
        let plan = prepare(&mut volume, "/wanted", NodeMutation::CreateDirectory);
        assert_eq!(plan.context().previous.incremental, unindexed);
        let root_meta = volume.index().root.meta.clone();
        let original_marker = volume.root_xattr(MARKER).unwrap().to_owned();
        volume.stage_node_mutation(&plan).unwrap();
        if unindexed {
            // Persist orphan bytes, but never their working-tree file entry.
            volume
                .append_file("/orphan", &mut Cursor::new(b"unindexed"))
                .unwrap();
            TapeDrive::new(&dev).write_filemark(0).unwrap();
        } else {
            let mut fault = medium_error(opcode::WRITE_FILEMARKS_6);
            fault.after = 1;
            dev.inject(fault);
            assert!(volume.sync_batch().is_err());
            library.with_cartridge_mut("NODE001L8", |cartridge| {
                for partition in &mut cartridge.partitions {
                    partition.flushed = partition.objects.len();
                }
            });
        }
        drop(volume);
        fence(&dev, ReservationKey::new(2, 101)).unwrap();
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        assert_eq!(
            volume.recovery().dp.tail,
            if unindexed {
                TailKind::UnindexedData
            } else {
                TailKind::TruncatedIndex
            }
        );
        assert!(volume.node_mutation_absence_evidence(&plan).is_err());
        let before = media(&library);
        assert!(volume.discard_node_mutation_tail(&plan).is_err());
        assert_eq!(media(&library), before);
        volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
        let mut wrong = plan.context().clone();
        wrong.previous.sha256[0] ^= 1;
        let wrong =
            prepare_canonical(wrong, plan.mutation().clone(), volume.index(), TIME).unwrap();
        assert!(volume.discard_node_mutation_tail(&wrong).is_err());
        assert_eq!(media(&library), before);
        let proof = volume.discard_node_mutation_tail(&plan).unwrap();
        assert_eq!(proof.kind(), NodeMutationAbsenceKind::Discarded);
        assert_eq!(proof.file_count(), 1);
        assert_eq!(
            proof.current().generation,
            plan.context().previous.generation + 1
        );
        assert!(!proof.current().incremental);
        assert_eq!(volume.index().root.meta, root_meta);
        assert_eq!(volume.root_xattr("foreign.root"), Some("retained"));
        assert_eq!(volume.root_xattr(MARKER), Some(original_marker.as_str()));
        assert!(volume.index().find_directory("/wanted").is_none());
        assert!(volume.index().find_file("/orphan").is_none());
        assert!(volume.node_mutation_evidence(&plan).is_err());
        assert!(
            media(&library)[1].starts_with(&before[1]),
            "discard only appends to DP"
        );
        assert!(volume.stage_node_mutation(&plan).is_err());
        drop(volume);
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
        assert_eq!(volume.node_mutation_absence_evidence(&plan).unwrap(), proof);
        let mut body = Vec::new();
        volume.read_file_to_writer("/kept", &mut body).unwrap();
        assert_eq!(body, b"retained-body");
        let mut context = plan.context().clone();
        context.previous = proof.current();
        context.owner_epoch = 101;
        let rebased =
            prepare_canonical(context, plan.mutation().clone(), volume.index(), TIME).unwrap();
        assert_ne!(rebased.fingerprint(), plan.fingerprint());
        commit(&mut volume, &rebased);
        assert!(volume.node_mutation_absence_evidence(&rebased).is_err());
    }
}

#[test]
fn semantic_noop_with_a_committed_node_marker_is_never_an_absence_proof() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/kept", &mut Cursor::new(b"body"))
        .unwrap();
    volume.sync().unwrap();
    let mutation = NodeMutation::ChangeMetadata {
        readonly: Some(false),
        access_time: None,
        modify_time: None,
    };
    let first = prepare(&mut volume, "/kept", mutation.clone());
    commit(&mut volume, &first);
    let noop = prepare(&mut volume, "/kept", mutation);
    let wire: serde_json::Value = serde_json::from_slice(&noop.encode().unwrap()).unwrap();
    assert_eq!(wire["tree_before"], wire["tree_after"]);
    commit(&mut volume, &noop);
    let before = media(&library);
    assert!(volume.node_mutation_absence_evidence(&noop).is_err());
    assert!(volume.discard_node_mutation_tail(&noop).is_err());
    assert_eq!(media(&library), before);
    drop(volume);
    let mut volume = mounted(&dev);
    volume.node_mutation_evidence(&noop).unwrap();
    assert!(volume.node_mutation_absence_evidence(&noop).is_err());
    assert_eq!(media(&library), before);
}

#[test]
fn an_unknown_node_s4_with_a_later_tail_is_rejected_before_discard_writes() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    let original = prepare(&mut volume, "/original", NodeMutation::CreateDirectory);
    let other = prepare(&mut volume, "/other", NodeMutation::CreateDirectory);
    commit(&mut volume, &other);
    volume
        .append_file("/orphan", &mut Cursor::new(b"unindexed"))
        .unwrap();
    tape_rs::tape::commands::TapeDrive::new(&dev)
        .write_filemark(0)
        .unwrap();
    drop(volume);
    let mut volume = mounted(&dev);
    let before = media(&library);
    assert!(volume.node_mutation_absence_evidence(&original).is_err());
    assert!(volume.discard_node_mutation_tail(&original).is_err());
    assert_eq!(media(&library), before);
    assert!(volume.index().find_directory("/other").is_some());
    assert!(volume.index().find_directory("/original").is_none());
}

#[test]
fn node_discard_proof_retains_prior_ranges_and_rejects_actual_construct_tampering() {
    use tape_rs::scsi::sim::LogicalObject;
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/kept", &mut Cursor::new(b"body"))
        .unwrap();
    volume.set_root_xattr("foreign.root", "retained").unwrap();
    volume.sync().unwrap();
    let mut previous_ranges = None::<String>;
    for _ in 0..2 {
        let plan = prepare(&mut volume, "/wanted", NodeMutation::CreateDirectory);
        volume
            .append_file("/orphan", &mut Cursor::new(b"orphan"))
            .unwrap();
        tape_rs::tape::commands::TapeDrive::new(&dev)
            .write_filemark(0)
            .unwrap();
        drop(volume);
        volume = mounted(&dev);
        let proof = volume.discard_node_mutation_tail(&plan).unwrap();
        let range = volume
            .root_xattr(tape_rs::ltfs::volume::XATTR_ABANDONED)
            .unwrap()
            .to_owned();
        if let Some(previous) = previous_ranges {
            assert!(range.starts_with(&format!("{previous};")));
        }
        previous_ranges = Some(range.clone());
        let marker = volume
            .root_xattr(tape_rs::ltfs::volume::XATTR_EMPTY_RECOVERY)
            .unwrap()
            .to_owned();
        let original = media(&library);
        drop(volume);
        for (before, after) in [
            ("retained".to_owned(), "tampered".to_owned()),
            (range.clone(), "b:0-0".to_owned()),
            (marker.clone(), "unbound-recovery".to_owned()),
            (
                "<length>4</length>".to_owned(),
                "<length>5</length>".to_owned(),
            ),
            (
                "<readonly>false</readonly>".to_owned(),
                "<readonly>true</readonly>".to_owned(),
            ),
        ] {
            let mut changed = 0;
            library.with_cartridge_mut("NODE001L8", |cartridge| {
                for partition in &mut cartridge.partitions {
                    for object in &mut partition.objects {
                        let LogicalObject::Record(bytes) = object else {
                            continue;
                        };
                        let Ok(text) = std::str::from_utf8(bytes) else {
                            continue;
                        };
                        if text.starts_with("<?xml")
                            && text.contains(&format!(
                                "<generationnumber>{}</generationnumber>",
                                proof.current().generation
                            ))
                        {
                            assert!(text.contains(&before), "tamper fixture missing {before}");
                            *bytes = text.replace(&before, &after).into_bytes();
                            changed += 1;
                        }
                    }
                }
                cartridge.vcr += 1;
            });
            assert_eq!(changed, 2, "tamper the real DP and IP constructs");
            let damaged = media(&library);
            if let Ok(mut recovered) = LtfsVolume::mount(&dev) {
                recovered.set_reservation_guard(Some(ReservationKey::new(1, 100)));
                assert!(
                    recovered.node_mutation_absence_evidence(&plan).is_err(),
                    "{before}"
                );
                assert!(
                    recovered.discard_node_mutation_tail(&plan).is_err(),
                    "{before}"
                );
            }
            assert_eq!(
                media(&library),
                damaged,
                "rejection must not repair {before}"
            );
            library.with_cartridge_mut("NODE001L8", |cartridge| {
                for (partition, objects) in cartridge.partitions.iter_mut().zip(&original) {
                    partition.objects = objects.clone();
                    partition.flushed = partition.objects.len();
                }
                cartridge.vcr += 1;
            });
        }
        volume = mounted(&dev);
        assert_eq!(volume.node_mutation_absence_evidence(&plan).unwrap(), proof);
    }
}

#[test]
fn typed_node_changes_commit_real_nodes_and_preserve_file_content() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file_with_xattrs(
            "/dir/file",
            &mut Cursor::new(b"original-content"),
            &[("foreign", "preserved")],
        )
        .unwrap();
    volume.sync_batch().unwrap();
    let original = volume.index().find_file("/dir/file").unwrap().clone();
    let create = prepare(&mut volume, "/new-dir", NodeMutation::CreateDirectory);
    let bytes = create.encode().unwrap();
    let create = NodeMutationPlan::decode(&bytes).unwrap();
    volume.stage_node_mutation(&create).unwrap();
    assert!(volume.index().find_directory("/new-dir").is_none());
    assert!(volume.node_mutation_evidence(&create).is_err());
    volume.sync_batch().unwrap();
    let evidence = volume.node_mutation_evidence(&create).unwrap();
    assert!(evidence.attributes().unwrap().symlink().is_none());
    assert!(volume.index().find_file("/new-dir").is_none());
    assert!(volume.index().find_directory("/new-dir").is_some());

    let link = prepare(
        &mut volume,
        "/dir/link",
        NodeMutation::CreateSymlink {
            target: "file".into(),
        },
    );
    commit(&mut volume, &link);
    assert_eq!(
        volume
            .node_mutation_evidence(&link)
            .unwrap()
            .attributes()
            .unwrap()
            .symlink(),
        Some("file")
    );
    assert!(
        volume
            .index()
            .find_file("/dir/link")
            .unwrap()
            .extents
            .is_empty()
    );
    let update = prepare(
        &mut volume,
        "/dir/file",
        NodeMutation::SetXattr {
            attribute: attr("foreign", "updated"),
            mode: XattrMode::Replace,
        },
    );
    commit(&mut volume, &update);
    let updated = volume.index().find_file("/dir/file").unwrap();
    assert_eq!(updated.extents, original.extents);
    assert_eq!(updated.length, original.length);
    assert_eq!(updated.meta.file_uid, original.meta.file_uid);
    assert_eq!(
        updated.xattr("ltfs.hash.sha256sum"),
        original.xattr("ltfs.hash.sha256sum")
    );
    assert_eq!(updated.xattr("foreign"), Some("updated"));
    let change = prepare(
        &mut volume,
        "/dir/file",
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: Some(TIME.into()),
            modify_time: None,
        },
    );
    commit(&mut volume, &change);
    assert!(volume.index().find_file("/dir/file").unwrap().meta.readonly);
    let remove_attr = prepare(
        &mut volume,
        "/dir/file",
        NodeMutation::RemoveXattr {
            key: "foreign".into(),
        },
    );
    commit(&mut volume, &remove_attr);
    assert!(
        volume
            .index()
            .find_file("/dir/file")
            .unwrap()
            .xattr("foreign")
            .is_none()
    );
    let unlink = prepare(&mut volume, "/dir/link", NodeMutation::Unlink);
    commit(&mut volume, &unlink);
    assert!(
        volume
            .node_mutation_evidence(&unlink)
            .unwrap()
            .attributes()
            .is_none()
    );
    assert!(volume.index().find_file("/dir/link").is_none());
    let rmdir = prepare(&mut volume, "/new-dir", NodeMutation::RemoveDirectory);
    commit(&mut volume, &rmdir);
    assert!(volume.index().find_directory("/new-dir").is_none());
    let mut output = Vec::new();
    volume
        .read_file_to_writer("/dir/file", &mut output)
        .unwrap();
    assert_eq!(output, b"original-content");
    drop(volume);
    let mut recovered = mounted(&dev);
    recovered.node_mutation_evidence(&rmdir).unwrap();
    assert!(recovered.index().find_directory("/new-dir").is_none());
    let mut output = Vec::new();
    recovered
        .read_file_to_writer("/dir/file", &mut output)
        .unwrap();
    assert_eq!(output, b"original-content");
}

#[test]
fn bad_paths_types_conditions_and_reserved_attributes_fail_before_staging() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file_with_xattrs(
            "/dir/file",
            &mut Cursor::new(b"body"),
            &[("existing", "old")],
        )
        .unwrap();
    volume.sync_batch().unwrap();
    let baseline = volume.index().root.clone();
    let before = media(&library);
    let template = prepare(&mut volume, "/okay", NodeMutation::CreateDirectory);
    let context = template.context().clone();
    let mut cases = vec![
        ("/dir", NodeMutation::RemoveDirectory),
        ("/dir", NodeMutation::Unlink),
        ("/dir/file", NodeMutation::RemoveDirectory),
        ("/dir/file", NodeMutation::CreateDirectory),
        (
            "/dir/file",
            NodeMutation::SetXattr {
                attribute: attr("existing", "new"),
                mode: XattrMode::Create,
            },
        ),
        (
            "/dir/file",
            NodeMutation::SetXattr {
                attribute: attr("missing", "new"),
                mode: XattrMode::Replace,
            },
        ),
        (
            "/dir/file",
            NodeMutation::RemoveXattr {
                key: "missing".into(),
            },
        ),
        (
            "/dir/file",
            NodeMutation::ChangeMetadata {
                readonly: None,
                access_time: None,
                modify_time: None,
            },
        ),
        (
            "/bad-link",
            NodeMutation::CreateSymlink {
                target: "bad\0target".into(),
            },
        ),
        (
            "/bad-link",
            NodeMutation::CreateSymlink {
                target: String::new(),
            },
        ),
    ];
    for path in [
        "/",
        "relative",
        "/dir//child",
        "/dir/../child",
        "/dir/./child",
        "/dir/child/",
        "/missing/child",
        "/bad\0name",
    ] {
        cases.push((path, NodeMutation::CreateDirectory));
    }
    for key in [
        "ltfs.hash.sha256sum",
        "ltfs.hash.md5sum",
        "tapers.version",
        MARKER,
        "tapers.fdb.nodeRename",
        "",
    ] {
        cases.push((
            "/dir/file",
            NodeMutation::SetXattr {
                attribute: attr(key, "bad"),
                mode: XattrMode::Upsert,
            },
        ));
    }
    cases.push((
        "/dir/file",
        NodeMutation::SetXattr {
            attribute: Xattr {
                key: "binary".into(),
                value: "?".into(),
                base64: true,
            },
            mode: XattrMode::Upsert,
        },
    ));
    for (path, mutation) in cases {
        let creating = matches!(
            mutation,
            NodeMutation::CreateDirectory | NodeMutation::CreateSymlink { .. }
        );
        let mut context = context.clone();
        context.path = path.into();
        context.expected_version = (!creating).then_some(1);
        assert!(
            prepare_canonical(context, mutation, volume.index(), TIME).is_err(),
            "{path}"
        );
    }
    assert_eq!(volume.index().root, baseline);
    assert_eq!(media(&library), before);
    assert_eq!(
        volume
            .verified_committed_index_construct()
            .unwrap()
            .generation,
        context.previous.generation
    );
}

#[test]
fn plan_codec_binds_scope_and_rejects_noncanonical_or_inconsistent_projection() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let plan = prepare(&mut volume, "/new", NodeMutation::CreateDirectory);
    let bytes = plan.encode().unwrap();
    assert_eq!(NodeMutationPlan::decode(&bytes).unwrap(), plan);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (field, wrong) in [
        ("version", serde_json::json!(1)),
        ("version", serde_json::json!(2)),
        ("version", serde_json::json!(4)),
        ("owner_epoch", serde_json::json!(0)),
        ("parent_version", serde_json::json!(0)),
        ("parent_version", serde_json::json!(u64::MAX)),
        ("namespace", serde_json::json!(Uuid::nil().to_string())),
        ("generation", serde_json::json!(u64::MAX)),
        ("expected_version", serde_json::json!(1)),
        ("tree_after", serde_json::json!([0])),
        ("next_uid", serde_json::json!(99)),
        ("after", serde_json::json!({"kind":"missing"})),
        ("time", serde_json::json!("2026-10-31T08:00:00Z")),
        ("extra", serde_json::json!(true)),
    ] {
        let mut changed = value.clone();
        changed[field] = wrong;
        assert!(
            NodeMutationPlan::decode(changed.to_string().as_bytes()).is_err(),
            "{field}"
        );
    }
    let mut newline = bytes.clone();
    newline.push(b'\n');
    assert!(NodeMutationPlan::decode(&newline).is_err());
    let duplicate = format!(
        "{{\"version\":1,{}",
        std::str::from_utf8(&bytes[1..]).unwrap()
    );
    assert!(NodeMutationPlan::decode(duplicate.as_bytes()).is_err());
    let mut missing_parent_version = value.clone();
    missing_parent_version
        .as_object_mut()
        .unwrap()
        .remove("parent_version");
    assert!(NodeMutationPlan::decode(missing_parent_version.to_string().as_bytes()).is_err());
    for field in [
        "library",
        "namespace",
        "pool",
        "operation",
        "parent",
        "parent_version",
        "node",
        "scan",
        "volume",
        "request_epoch",
        "owner_epoch",
    ] {
        let mut changed = value.clone();
        changed[field] = if field.ends_with("epoch") || field == "parent_version" {
            serde_json::json!(2)
        } else if field == "library" {
            serde_json::json!("OTHER-LIBRARY")
        } else {
            serde_json::json!(Uuid::new_v4().to_string())
        };
        if matches!(field, "parent" | "parent_version") {
            assert!(NodeMutationPlan::decode(changed.to_string().as_bytes()).is_err());
            let replacement = changed[field].clone();
            let last = changed["ancestors"]
                .as_array_mut()
                .unwrap()
                .last_mut()
                .unwrap();
            last[if field == "parent" {
                "inode"
            } else {
                "version"
            }] = replacement;
        }
        let changed = NodeMutationPlan::decode(changed.to_string().as_bytes()).unwrap();
        assert_ne!(changed.fingerprint(), plan.fingerprint(), "{field}");
    }
}

#[test]
fn ee_unlink_owns_the_private_directory_and_missing_s4_ownership_is_rejected() {
    const OWNER: &str = "tapers.fdb.eeInternalDirectory";
    for omit_ownership in [false, true] {
        let (library, dev) = formatted();
        let mut volume = mounted(&dev);
        volume
            .append_file("/.LTFSEE_DATA/data", &mut Cursor::new(b"kept body"))
            .unwrap();
        volume.add_symlink("/alias", ".LTFSEE_DATA/data").unwrap();
        volume.add_symlink("/second", ".LTFSEE_DATA/data").unwrap();
        volume
            .set_node_xattr(
                "/.LTFSEE_DATA",
                attr("foreign.private", "kept"),
                true,
                false,
            )
            .unwrap();
        volume.sync().unwrap();
        let directory = volume
            .index()
            .find_directory("/.LTFSEE_DATA")
            .unwrap()
            .clone();
        let data = volume
            .index()
            .find_file("/.LTFSEE_DATA/data")
            .unwrap()
            .clone();
        let plan = prepare(&mut volume, "/alias", NodeMutation::Unlink);
        let wire: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
        assert_eq!(wire["version"], 5);
        let before = media(&library);
        volume.stage_node_mutation(&plan).unwrap();
        assert_eq!(media(&library), before);
        // index() exposes the committed view until sync_batch succeeds.
        assert_eq!(
            volume.index().find_directory("/.LTFSEE_DATA").unwrap(),
            &directory
        );
        if omit_ownership {
            volume.remove_node_xattr("/.LTFSEE_DATA", OWNER).unwrap();
        }
        volume.sync_batch().unwrap();
        if omit_ownership {
            assert!(
                volume.node_mutation_evidence(&plan).is_err(),
                "matching target and request marker cannot hide omitted directory ownership"
            );
            continue;
        }
        let after = volume.index().find_directory("/.LTFSEE_DATA").unwrap();
        assert_eq!(after.meta, directory.meta);
        assert_eq!(after.files, directory.files);
        assert!(
            directory
                .xattrs
                .iter()
                .all(|attr| after.xattrs.contains(attr))
        );
        assert_eq!(
            after.xattrs.iter().filter(|attr| attr.key == OWNER).count(),
            1
        );

        assert_eq!(
            volume.node_mutation_evidence(&plan).unwrap().file_count(),
            2
        );
        drop(volume);
        let mut volume = mounted(&dev);
        volume.node_mutation_evidence(&plan).unwrap();
        let second = prepare(&mut volume, "/second", NodeMutation::Unlink);
        commit(&mut volume, &second);
        assert_eq!(
            volume.node_mutation_evidence(&second).unwrap().file_count(),
            1
        );
        assert_eq!(
            volume.index().find_file("/.LTFSEE_DATA/data").unwrap(),
            &data
        );
        assert!(volume.index().find_file("/alias").is_none());
        assert!(volume.index().find_file("/second").is_none());
        let after = volume.index().find_directory("/.LTFSEE_DATA").unwrap();
        assert_eq!(after.meta, directory.meta);
        assert_eq!(
            after.xattrs.iter().filter(|attr| attr.key == OWNER).count(),
            1
        );
    }
}

#[test]
fn a_logical_directory_can_materialize_on_a_medium_missing_the_target() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let previous = volume.verified_committed_index_construct().unwrap().into();
    let mut logical_meta = volume.index().root.meta.clone();
    logical_meta.file_uid = 900;
    let logical = NodeAttributes::new(
        logical_meta.clone(),
        vec![attr("foreign.directory", "retained")],
        None,
    )
    .unwrap();
    let parent = Uuid::new_v4();
    let root = NodeMutationAncestor {
        path: "/".into(),
        id: parent,
        version: 1,
        attributes: NodeAttributes::from_directory(&volume.index().root).unwrap(),
    };
    let context = NodeMutationContext {
        library_serial: "SIM-DIRECTORY-TARGET".into(),
        namespace: Uuid::new_v4(),
        pool: Uuid::new_v4(),
        operation: Uuid::new_v4(),
        request_epoch: 1,
        owner_epoch: 100,
        parent,
        parent_version: 1,
        node: Uuid::new_v4(),
        expected_version: Some(7),
        scan: Uuid::new_v4(),
        previous,
        path: "/logical-directory".into(),
    };
    let plan = NodeMutationPlan::prepare(
        context,
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
        volume.index(),
        TIME,
        &[root],
        Some((NodeKind::Directory, &logical)),
    )
    .unwrap();
    commit(&mut volume, &plan);
    let target = volume.index().find_directory("/logical-directory").unwrap();
    assert_eq!(target.meta.file_uid, 2);
    assert_ne!(target.meta.file_uid, logical_meta.file_uid);
    assert!(target.meta.readonly);
    assert_eq!(target.meta.creation_time, logical_meta.creation_time);
    assert_eq!(target.xattrs, logical.xattrs().unwrap());
    assert_eq!(volume.index().highest_file_uid, 2);
    assert_eq!(
        plan.logical_attributes_after()
            .unwrap()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        900
    );
    drop(volume);
    let mut volume = mounted(&dev);
    let proof = volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(proof.file_count(), 0);
    assert_eq!(proof.attributes().unwrap().meta().unwrap().file_uid, 2);
    assert_eq!(
        proof.logical_attributes().unwrap().meta().unwrap().file_uid,
        900
    );
}

#[test]
fn an_absent_physical_directory_can_be_retired_without_inventing_a_uid() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let template = prepare(&mut volume, "/template", NodeMutation::CreateDirectory);
    let mut context = template.context().clone();
    context.path = "/absent-directory".into();
    context.expected_version = Some(7);
    let root = NodeMutationAncestor {
        path: "/".into(),
        id: context.parent,
        version: context.parent_version,
        attributes: NodeAttributes::from_directory(&volume.index().root).unwrap(),
    };
    let mut meta = volume.index().root.meta.clone();
    meta.file_uid = 900;
    let logical =
        NodeAttributes::new(meta, vec![attr("foreign.directory", "retained")], None).unwrap();
    let highest = volume.index().highest_file_uid;
    let plan = NodeMutationPlan::prepare(
        context,
        NodeMutation::RemoveDirectory,
        volume.index(),
        TIME,
        &[root],
        Some((NodeKind::Directory, &logical)),
    )
    .unwrap();
    commit(&mut volume, &plan);
    assert!(volume.index().find_directory("/absent-directory").is_none());
    assert_eq!(volume.index().highest_file_uid, highest);
    assert!(plan.logical_attributes_after().unwrap().is_none());
    assert_eq!(volume.index().root.meta.modify_time, TIME);
    assert_eq!(
        volume.node_mutation_evidence(&plan).unwrap().file_count(),
        0
    );
    assert!(
        volume.node_mutation_absence_evidence(&plan).is_err(),
        "a committed deletion marker is not an unchanged-source proof"
    );
}

#[test]
fn missing_files_and_mismatched_target_kinds_are_rejected_before_staging() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    let template = prepare(&mut volume, "/template", NodeMutation::CreateDirectory);
    let mut context = template.context().clone();
    context.path = "/missing-file".into();
    context.expected_version = Some(7);
    let root = NodeMutationAncestor {
        path: "/".into(),
        id: context.parent,
        version: context.parent_version,
        attributes: NodeAttributes::from_directory(&volume.index().root).unwrap(),
    };
    let mut meta = volume.index().root.meta.clone();
    meta.file_uid = 900;
    let logical = NodeAttributes::new(meta, vec![], None).unwrap();
    let before = media(&library);
    let mutation = NodeMutation::ChangeMetadata {
        readonly: Some(true),
        access_time: None,
        modify_time: None,
    };
    assert!(
        NodeMutationPlan::prepare(
            context.clone(),
            mutation.clone(),
            volume.index(),
            TIME,
            std::slice::from_ref(&root),
            Some((NodeKind::File, &logical))
        )
        .is_err()
    );
    assert!(
        NodeMutationPlan::prepare(
            context.clone(),
            mutation.clone(),
            volume.index(),
            TIME,
            std::slice::from_ref(&root),
            None
        )
        .is_err()
    );
    assert_eq!(media(&library), before);
    let plan = NodeMutationPlan::prepare(
        context,
        mutation,
        volume.index(),
        TIME,
        std::slice::from_ref(&root),
        Some((NodeKind::Directory, &logical)),
    )
    .unwrap();
    let mut exhausted = volume.index().clone();
    exhausted.highest_file_uid = u64::MAX;
    assert!(
        NodeMutationPlan::prepare(
            plan.context().clone(),
            plan.mutation().clone(),
            &exhausted,
            TIME,
            std::slice::from_ref(&root),
            Some((NodeKind::Directory, &logical))
        )
        .is_err()
    );
    assert_eq!(media(&library), before);
    let encoded: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
    assert_eq!(encoded["version"], 5);
    assert_eq!(encoded["target_kind"], "directory");
    for replacement in [
        serde_json::json!("file"),
        serde_json::json!("unknown"),
        serde_json::json!(null),
    ] {
        let mut changed = encoded.clone();
        changed["target_kind"] = replacement;
        assert!(NodeMutationPlan::decode(changed.to_string().as_bytes()).is_err());
    }
    let mut omitted = encoded;
    omitted.as_object_mut().unwrap().remove("target_kind");
    assert!(NodeMutationPlan::decode(omitted.to_string().as_bytes()).is_err());
}

#[test]
fn source_and_whole_result_tree_are_checked_not_just_the_target() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/other", &mut Cursor::new(b"body"))
        .unwrap();
    volume.sync_batch().unwrap();
    let plan = prepare(&mut volume, "/new", NodeMutation::CreateDirectory);
    let before = media(&library);
    let mut wrong = plan.context().clone();
    wrong.previous.sha256[0] ^= 1;
    let wrong =
        prepare_canonical(wrong, NodeMutation::CreateDirectory, volume.index(), TIME).unwrap();
    assert!(volume.stage_node_mutation(&wrong).is_err());
    assert_eq!(media(&library), before);
    volume.stage_node_mutation(&plan).unwrap();
    assert_eq!(media(&library), before, "staging never writes the medium");
    assert!(
        volume.stage_node_mutation(&plan).is_err(),
        "a dirty working view cannot admit another plan"
    );
    volume
        .set_node_xattr(
            "/other",
            attr(MARKER, "unrelated-file-change"),
            false,
            false,
        )
        .unwrap();
    volume.sync_batch().unwrap();
    assert!(
        volume.node_mutation_evidence(&plan).is_err(),
        "a matching marker and target cannot hide an unrelated edit"
    );
    assert!(
        volume.stage_node_mutation(&plan).is_err(),
        "a committed plan cannot be replayed"
    );
}

#[test]
fn complete_view_hash_ignores_sibling_order_and_preserves_large_unknown_attributes() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file_with_xattrs(
            "/z",
            &mut Cursor::new(b"z"),
            &[("foreign.big", &"x".repeat(2_000_000))],
        )
        .unwrap();
    volume.append_file("/a", &mut Cursor::new(b"a")).unwrap();
    volume.sync_batch().unwrap();
    let plan = prepare(
        &mut volume,
        "/z",
        NodeMutation::SetXattr {
            attribute: Xattr {
                key: "binary".into(),
                value: "AAH/".into(),
                base64: true,
            },
            mode: XattrMode::Create,
        },
    );
    let mut reordered = volume.index().clone();
    reordered.root.files.reverse();
    let other = prepare_canonical(
        plan.context().clone(),
        plan.mutation().clone(),
        &reordered,
        TIME,
    )
    .unwrap();
    assert_eq!(other.encode().unwrap(), plan.encode().unwrap());
    let plan = NodeMutationPlan::decode(&plan.encode().unwrap()).unwrap();
    commit(&mut volume, &plan);
    drop(volume);
    let mut volume = mounted(&dev);
    volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(
        volume
            .index()
            .find_file("/z")
            .unwrap()
            .xattr("foreign.big")
            .unwrap()
            .len(),
        2_000_000
    );
    assert!(
        volume
            .index()
            .find_file("/z")
            .unwrap()
            .xattrs
            .iter()
            .find(|a| a.key == "binary")
            .unwrap()
            .base64
    );
}

fn medium_error(opcode: u8) -> Fault {
    Fault {
        opcode,
        action: FaultAction::CheckCondition {
            sense_key: 0x03,
            asc: 0x0c,
            ascq: 0x00,
        },
        after: 0,
        remaining: 1,
    }
}
#[test]
fn failed_commit_is_never_a_proof_but_unobserved_s4_can_be_verified_after_remount() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let plan = prepare(&mut volume, "/unobserved", NodeMutation::CreateDirectory);
    volume.stage_node_mutation(&plan).unwrap();
    dev.inject(medium_error(opcode::WRITE_ATTRIBUTE));
    assert!(volume.sync_batch().is_err());
    assert!(volume.node_mutation_evidence(&plan).is_err());
    assert!(volume.index().find_directory("/unobserved").is_none());
    drop(volume);
    fence(&dev, ReservationKey::new(2, 101)).unwrap();
    let mut volume = LtfsVolume::mount(&dev).unwrap();
    volume.set_reservation_guard(Some(ReservationKey::new(2, 101)));
    let proof = volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(
        proof.context().owner_epoch,
        100,
        "media proof keeps the original intent; the new FDB owner must authorize its publication"
    );
    assert!(volume.index().find_directory("/unobserved").is_some());
}

#[test]
fn missing_s4_and_lost_pr_do_not_mint_node_evidence() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let plan = prepare(&mut volume, "/not-committed", NodeMutation::CreateDirectory);
    volume.stage_node_mutation(&plan).unwrap();
    dev.inject(medium_error(opcode::WRITE_6));
    assert!(volume.sync_batch().is_err());
    assert!(volume.node_mutation_evidence(&plan).is_err());
    drop(volume);
    let mut volume = mounted(&dev);
    assert!(volume.node_mutation_evidence(&plan).is_err());
    assert!(volume.index().find_directory("/not-committed").is_none());
    // A separate clean cartridge proves that a valid result still needs PR.
    let (_other, other_dev) = formatted();
    let mut other = mounted(&other_dev);
    let plan = prepare(&mut other, "/committed", NodeMutation::CreateDirectory);
    commit(&mut other, &plan);
    fence(&other_dev, ReservationKey::new(2, 101)).unwrap();
    assert!(other.node_mutation_evidence(&plan).is_err());
}

#[test]
fn directory_and_link_attributes_preserve_children_and_unlink_keeps_data_blocks() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/dir/file", &mut Cursor::new(b"retained-data"))
        .unwrap();
    volume.sync_batch().unwrap();
    let child = volume.index().find_file("/dir/file").unwrap().clone();
    let dir = volume.index().find_directory("/dir").unwrap().clone();
    let update = prepare(
        &mut volume,
        "/dir",
        NodeMutation::SetXattr {
            attribute: attr("directory.attr", "preserved"),
            mode: XattrMode::Create,
        },
    );
    commit(&mut volume, &update);
    assert_eq!(
        volume.index().find_directory("/dir").unwrap().meta.file_uid,
        dir.meta.file_uid
    );
    assert_eq!(volume.index().find_file("/dir/file").unwrap(), &child);
    let link = prepare(
        &mut volume,
        "/dir/link",
        NodeMutation::CreateSymlink {
            target: "file".into(),
        },
    );
    commit(&mut volume, &link);
    let update = prepare(
        &mut volume,
        "/dir/link",
        NodeMutation::SetXattr {
            attribute: attr("link.attr", "only-link"),
            mode: XattrMode::Create,
        },
    );
    commit(&mut volume, &update);
    assert!(
        volume
            .index()
            .find_file("/dir/file")
            .unwrap()
            .xattr("link.attr")
            .is_none()
    );
    assert_eq!(
        volume
            .index()
            .find_file("/dir/link")
            .unwrap()
            .symlink
            .as_deref(),
        Some("file")
    );
    drop(volume);
    let mut volume = mounted(&dev);
    let unlink = prepare(&mut volume, "/dir/file", NodeMutation::Unlink);
    let before = media(&library);
    commit(&mut volume, &unlink);
    assert!(volume.index().find_file("/dir/file").is_none());
    assert!(volume.index().find_file("/dir/link").is_some());
    let after = media(&library);
    assert!(
        after[1].starts_with(&before[1]),
        "unlink appends an index and never overwrites retained data"
    );
    assert!(
        volume
            .node_mutation_evidence(&unlink)
            .unwrap()
            .attributes()
            .is_none()
    );
}

#[test]
fn malformed_source_marker_and_uid_exhaustion_are_rejected_without_writes() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    let template = prepare(&mut volume, "/new", NodeMutation::CreateDirectory);
    let before = media(&library);
    for value in ["bad", &format!("1:{}", "A".repeat(64))] {
        let mut index = volume.index().clone();
        index.root.xattrs.push(attr(MARKER, value));
        assert!(
            prepare_canonical(
                template.context().clone(),
                NodeMutation::CreateDirectory,
                &index,
                TIME
            )
            .is_err()
        );
    }
    let mut index = volume.index().clone();
    index.highest_file_uid = u64::MAX;
    assert!(
        prepare_canonical(
            template.context().clone(),
            NodeMutation::CreateDirectory,
            &index,
            TIME
        )
        .is_err()
    );
    assert_eq!(media(&library), before);
}

#[test]
fn physical_request_marker_corruption_cannot_be_replaced_by_target_presence() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    let plan = prepare(&mut volume, "/new", NodeMutation::CreateDirectory);
    commit(&mut volume, &plan);
    let current = volume.verified_committed_index_construct().unwrap();
    let marker = volume
        .index()
        .root
        .xattrs
        .iter()
        .find(|a| a.key == MARKER)
        .unwrap()
        .value
        .clone();
    drop(volume);
    let mut changed = false;
    assert!(library.with_cartridge_mut("NODE001L8", |cartridge| {
        let record = &mut cartridge.partitions[1].objects[current.location.start_block as usize];
        if let tape_rs::scsi::sim::LogicalObject::Record(bytes) = record
            && let Some(offset) = bytes
                .windows(marker.len())
                .position(|v| v == marker.as_bytes())
        {
            bytes[offset] = b'2';
            changed = true;
        }
    }));
    assert!(
        changed,
        "tamper the actual committed DP construct, not a synthetic copy"
    );
    let mut volume = mounted(&dev);
    assert!(volume.index().find_directory("/new").is_some());
    assert!(volume.node_mutation_evidence(&plan).is_err());
}

#[test]
fn a_construct_without_its_closing_filemark_is_not_a_committed_node_change() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let plan = prepare(&mut volume, "/incomplete", NodeMutation::CreateDirectory);
    volume.stage_node_mutation(&plan).unwrap();
    let mut fault = medium_error(opcode::WRITE_FILEMARKS_6);
    fault.after = 1;
    dev.inject(fault);
    assert!(volume.sync_batch().is_err());
    assert!(volume.node_mutation_evidence(&plan).is_err());
    drop(volume);
    let mut volume = mounted(&dev);
    assert!(volume.index().find_directory("/incomplete").is_none());
    assert!(volume.node_mutation_evidence(&plan).is_err());
}

#[test]
fn incremental_mutations_use_the_complete_view_and_reject_raw_delta_plans() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/old", &mut Cursor::new(b"old-data"))
        .unwrap();
    volume.sync_batch().unwrap();
    let first = prepare(&mut volume, "/new-dir", NodeMutation::CreateDirectory);
    volume.stage_node_mutation(&first).unwrap();
    volume.commit_incremental().unwrap();
    assert!(
        volume
            .node_mutation_evidence(&first)
            .unwrap()
            .current()
            .incremental
    );
    drop(volume);
    let mut volume = mounted(&dev);
    volume.node_mutation_evidence(&first).unwrap();
    assert!(volume.index().find_file("/old").is_some());
    let next = prepare(&mut volume, "/next-dir", NodeMutation::CreateDirectory);
    let raw = volume.verified_committed_raw_dp_index().unwrap();
    assert!(raw.incremental);
    assert!(
        prepare_canonical(next.context().clone(), next.mutation().clone(), &raw, TIME).is_err()
    );
    volume.stage_node_mutation(&next).unwrap();
    volume.commit_incremental().unwrap();
    assert!(
        volume
            .node_mutation_evidence(&next)
            .unwrap()
            .current()
            .incremental
    );
    let mut output = Vec::new();
    volume.read_file_to_writer("/old", &mut output).unwrap();
    assert_eq!(output, b"old-data");
}

#[test]
fn exhausted_parent_versions_reject_entry_changes_but_allow_target_only_metadata() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/file", &mut Cursor::new(b"body"))
        .unwrap();
    volume.create_directory("/empty").unwrap();
    volume.sync_batch().unwrap();
    let before = media(&library);
    for (path, mutation) in [
        ("/new", NodeMutation::CreateDirectory),
        (
            "/new-link",
            NodeMutation::CreateSymlink {
                target: "file".into(),
            },
        ),
        ("/file", NodeMutation::Unlink),
        ("/empty", NodeMutation::RemoveDirectory),
    ] {
        let template = prepare(&mut volume, path, mutation.clone());
        let mut context = template.context().clone();
        context.parent_version = u64::MAX;
        assert!(prepare_canonical(context, mutation, volume.index(), TIME).is_err());
    }
    let mutation = NodeMutation::SetXattr {
        attribute: attr("target-only", "value"),
        mode: XattrMode::Create,
    };
    let template = prepare(&mut volume, "/file", mutation.clone());
    let mut context = template.context().clone();
    context.parent_version = u64::MAX;
    let prepared = prepare_canonical(context, mutation, volume.index(), TIME).unwrap();
    NodeMutationPlan::decode(&prepared.encode().unwrap()).unwrap();
    assert_eq!(
        media(&library),
        before,
        "unpublishable parent revisions are rejected before staging or media I/O"
    );
}

#[test]
fn canonical_ancestors_preserve_physical_uids_source_tree_and_unknown_root_metadata() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume.create_directory("/dir").unwrap();
    volume
        .append_file("/dir/file", &mut Cursor::new(b"original bytes"))
        .unwrap();
    volume
        .set_node_xattr("/", attr("volume.private", "keep"), false, false)
        .unwrap();
    volume.sync_batch().unwrap();
    let mutation = NodeMutation::SetXattr {
        attribute: attr("requested", "change"),
        mode: XattrMode::Create,
    };
    let template = prepare(&mut volume, "/dir/file", mutation.clone());
    let context = template.context().clone();
    let before = volume.index().clone();
    let mut logical_root = before.root.meta.clone();
    logical_root.file_uid = 999_999;
    logical_root.creation_time = "2000-01-01T00:00:00.000000000Z".into();
    let mut logical_parent = before.find_directory("/dir").unwrap().meta.clone();
    logical_parent.file_uid = 888_888;
    logical_parent.creation_time = "2001-01-01T00:00:00.000000000Z".into();
    let mut logical_file = before.find_file("/dir/file").unwrap().meta.clone();
    logical_file.file_uid = 777_777;
    logical_file.creation_time = "2002-01-01T00:00:00.000000000Z".into();
    let ancestors = vec![
        NodeMutationAncestor {
            path: "/".into(),
            id: Uuid::new_v4(),
            version: 7,
            attributes: NodeAttributes::new(
                logical_root,
                vec![attr("logical.root", "virtual")],
                None,
            )
            .unwrap(),
        },
        NodeMutationAncestor {
            path: "/dir".into(),
            id: context.parent,
            version: context.parent_version,
            attributes: NodeAttributes::new(
                logical_parent.clone(),
                vec![Xattr {
                    key: "logical.parent".into(),
                    value: "AAH/".into(),
                    base64: true,
                }],
                None,
            )
            .unwrap(),
        },
    ];
    let mut attrs = before.find_file("/dir/file").unwrap().xattrs.clone();
    attrs.push(attr("logical.target", "preserved"));
    let target = NodeAttributes::new(logical_file.clone(), attrs, None).unwrap();
    let plan = NodeMutationPlan::prepare(
        context,
        mutation,
        volume.index(),
        TIME,
        &ancestors,
        Some((NodeKind::File, &target)),
    )
    .unwrap();
    assert_eq!(
        volume.index().root,
        before.root,
        "canonical inputs never mutate the real source before planning"
    );
    assert_eq!(
        plan.logical_parent_attributes_after().unwrap(),
        ancestors[1].attributes
    );
    let wire: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
    assert_ne!(wire["tree_before"], wire["tree_after"]);
    commit(&mut volume, &plan);
    let evidence = volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(
        evidence
            .logical_attributes()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        777_777
    );
    assert_eq!(
        evidence.attributes().unwrap().meta().unwrap().file_uid,
        before.find_file("/dir/file").unwrap().meta.file_uid
    );
    assert_eq!(volume.index().root.meta, before.root.meta);
    assert!(
        volume
            .index()
            .root
            .xattrs
            .iter()
            .any(|a| a.key == "volume.private" && a.value == "keep")
    );
    assert!(
        !volume
            .index()
            .root
            .xattrs
            .iter()
            .any(|a| a.key == "logical.root")
    );
    let parent = volume.index().find_directory("/dir").unwrap();
    assert_eq!(
        parent.meta.file_uid,
        before.find_directory("/dir").unwrap().meta.file_uid
    );
    assert_eq!(parent.meta.creation_time, logical_parent.creation_time);
    assert_eq!(parent.xattrs, ancestors[1].attributes.xattrs().unwrap());
    let file = volume.index().find_file("/dir/file").unwrap();
    assert_eq!(
        file.meta.file_uid,
        before.find_file("/dir/file").unwrap().meta.file_uid
    );
    assert_eq!(file.meta.creation_time, logical_file.creation_time);
    assert_eq!(file.extents, before.find_file("/dir/file").unwrap().extents);
    assert_eq!(file.length, before.find_file("/dir/file").unwrap().length);
    assert!(file.xattrs.iter().any(|a| a.key == "logical.target"));
    assert!(file.xattrs.iter().any(|a| a.key == "requested"));
    // Even a logical root field that does not alter the per-volume tree is
    // bound by the request marker; old S4 cannot prove changed logical inputs.
    let mut changed: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
    changed["ancestors"][0]["logical"]["meta"]["creation_time"] =
        serde_json::json!("2003-01-01T00:00:00.000000000Z");
    let changed = NodeMutationPlan::decode(changed.to_string().as_bytes()).unwrap();
    assert_ne!(changed.fingerprint(), plan.fingerprint());
    assert!(volume.node_mutation_evidence(&changed).is_err());
}

#[test]
fn virtual_root_logical_publication_keeps_its_own_uid_and_creation_event() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .set_node_xattr("/", attr("volume.private", "preserved"), false, false)
        .unwrap();
    volume.sync_batch().unwrap();
    let template = prepare(&mut volume, "/new", NodeMutation::CreateDirectory);
    let before = volume.index().root.clone();
    let mut logical = before.meta.clone();
    logical.file_uid = u64::MAX;
    logical.creation_time = "2001-01-01T00:00:00.000000000Z".into();
    logical.access_time = "2002-01-01T00:00:00.000000000Z".into();
    let ancestor = NodeMutationAncestor {
        path: "/".into(),
        id: template.context().parent,
        version: template.context().parent_version,
        attributes: NodeAttributes::new(
            logical.clone(),
            vec![attr("logical.private", "virtual")],
            None,
        )
        .unwrap(),
    };
    let plan = NodeMutationPlan::prepare(
        template.context().clone(),
        NodeMutation::CreateDirectory,
        volume.index(),
        TIME,
        &[ancestor],
        None,
    )
    .unwrap();
    let logical_after = plan.logical_parent_attributes_after().unwrap();
    assert_eq!(logical_after.meta().unwrap().file_uid, u64::MAX);
    assert_eq!(
        logical_after.meta().unwrap().creation_time,
        logical.creation_time
    );
    assert_eq!(
        logical_after.meta().unwrap().access_time,
        logical.access_time
    );
    assert_eq!(logical_after.meta().unwrap().modify_time, TIME);
    assert_eq!(logical_after.meta().unwrap().change_time, TIME);
    commit(&mut volume, &plan);
    let physical = &volume.index().root.meta;
    assert_eq!(physical.file_uid, before.meta.file_uid);
    assert_eq!(physical.creation_time, before.meta.creation_time);
    assert_eq!(physical.access_time, before.meta.access_time);
    assert_eq!(physical.modify_time, TIME);
    assert_eq!(physical.change_time, TIME);
    assert!(
        volume
            .index()
            .root
            .xattrs
            .iter()
            .any(|a| a.key == "volume.private")
    );
    assert!(
        !volume
            .index()
            .root
            .xattrs
            .iter()
            .any(|a| a.key == "logical.private")
    );
    assert_eq!(volume.index().highest_file_uid, 2);
}

#[test]
fn canonical_input_codec_rejects_omitted_reordered_aliased_or_wrong_kind_metadata() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume.create_directory("/dir").unwrap();
    volume
        .append_file("/dir/file", &mut Cursor::new(b"body"))
        .unwrap();
    volume.sync_batch().unwrap();
    let plan = prepare(
        &mut volume,
        "/dir/file",
        NodeMutation::SetXattr {
            attribute: attr("new", "v"),
            mode: XattrMode::Create,
        },
    );
    let value: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
    let before = media(&library);
    let root = volume.index().root.clone();
    for corruption in 0..10 {
        let mut changed = value.clone();
        match corruption {
            0 => {
                changed["ancestors"].as_array_mut().unwrap().pop();
            }
            1 => {
                changed["ancestors"].as_array_mut().unwrap().swap(0, 1);
            }
            2 => {
                changed["ancestors"][1]["path"] = serde_json::json!("/other");
            }
            3 => {
                changed["ancestors"][1]["inode"] = changed["ancestors"][0]["inode"].clone();
            }
            4 => {
                changed["ancestors"][1]["version"] = serde_json::json!(0);
            }
            5 => {
                changed["ancestors"][1]["before_meta"]["file_uid"] = serde_json::json!(0);
            }
            6 => {
                changed["ancestors"][1]["logical"]["symlink"] = serde_json::json!("file");
            }
            7 => {
                changed["ancestors"][0]["logical"]["xattrs"] =
                    serde_json::json!([{"key":MARKER,"value":"bad","base64":false}]);
            }
            8 => {
                changed["target_attributes"] = serde_json::Value::Null;
            }
            9 => {
                changed["target_attributes"]["symlink"] = serde_json::json!("retarget");
            }
            _ => unreachable!(),
        }
        assert!(
            NodeMutationPlan::decode(changed.to_string().as_bytes()).is_err(),
            "corruption {corruption}"
        );
    }
    assert_eq!(volume.index().root, root);
    assert_eq!(media(&library), before);
}

fn logical_chain_on_another_volume(
    context: &NodeMutationContext,
    root: &tape_rs::ltfs::index::DirectoryNode,
) -> Vec<NodeMutationAncestor> {
    ["/", "/grand", "/grand/parent"]
        .iter()
        .enumerate()
        .map(|(offset, path)| {
            let mut meta = root.meta.clone();
            meta.file_uid = 100_000 + offset as u64;
            meta.creation_time = "2000-01-01T00:00:00.000000000Z".into();
            NodeMutationAncestor {
                path: (*path).into(),
                id: if offset == 2 {
                    context.parent
                } else {
                    Uuid::from_u128(10_000 + offset as u128)
                },
                version: if offset == 2 {
                    context.parent_version
                } else {
                    5 + offset as u64
                },
                attributes: NodeAttributes::new(
                    meta,
                    vec![attr("logical.unknown", "preserved")],
                    None,
                )
                .unwrap(),
            }
        })
        .collect()
}

#[test]
fn missing_physical_ancestors_are_materialized_with_real_uids_and_full_tree_evidence() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .set_node_xattr("/", attr("volume.private", "keep"), false, false)
        .unwrap();
    volume.sync_batch().unwrap();
    let template = prepare(&mut volume, "/template", NodeMutation::CreateDirectory);
    let mut context = template.context().clone();
    context.path = "/grand/parent/new".into();
    context.parent = Uuid::new_v4();
    context.parent_version = 7;
    let canonical = logical_chain_on_another_volume(&context, &volume.index().root);
    let before = volume.index().root.clone();
    let plan = NodeMutationPlan::prepare(
        context,
        NodeMutation::CreateDirectory,
        volume.index(),
        TIME,
        &canonical,
        None,
    )
    .unwrap();
    assert_eq!(volume.index().root, before);
    assert!(volume.index().find_directory("/grand").is_none());
    assert_eq!(
        plan.logical_parent_attributes_after()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        100_002
    );
    commit(&mut volume, &plan);
    assert_eq!(volume.index().highest_file_uid, 4);
    assert_eq!(volume.index().root.meta.file_uid, before.meta.file_uid);
    assert_eq!(
        volume.index().root.meta.creation_time,
        before.meta.creation_time
    );
    assert_eq!(volume.index().root.meta.modify_time, TIME);
    assert!(
        volume
            .index()
            .root
            .xattrs
            .iter()
            .any(|a| a.key == "volume.private")
    );
    for (path, uid) in [
        ("/grand", 2),
        ("/grand/parent", 3),
        ("/grand/parent/new", 4),
    ] {
        assert_eq!(
            volume.index().find_directory(path).unwrap().meta.file_uid,
            uid
        );
    }
    assert_eq!(
        volume
            .index()
            .find_directory("/grand")
            .unwrap()
            .meta
            .creation_time,
        "2000-01-01T00:00:00.000000000Z"
    );
    assert_eq!(
        volume
            .index()
            .find_directory("/grand/parent")
            .unwrap()
            .meta
            .modify_time,
        TIME
    );
    let evidence = volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(
        evidence
            .ancestors()
            .iter()
            .map(|a| (a.path(), a.physical_uid(), a.newly_materialized()))
            .collect::<Vec<_>>(),
        vec![
            ("/", 1, false),
            ("/grand", 2, true),
            ("/grand/parent", 3, true)
        ]
    );
    assert_eq!(evidence.ancestors()[2].id(), canonical[2].id);
    assert_eq!(evidence.ancestors()[2].version_before(), 7);
    assert_eq!(
        evidence
            .logical_parent_attributes()
            .meta()
            .unwrap()
            .file_uid,
        100_002
    );
    assert_eq!(evidence.parent_meta().file_uid, 3);
    assert_eq!(
        evidence
            .logical_attributes()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        4
    );
    let mut changed: serde_json::Value = serde_json::from_slice(&plan.encode().unwrap()).unwrap();
    changed["ancestors"][1]["physical_uid"] = serde_json::json!(99);
    assert!(NodeMutationPlan::decode(changed.to_string().as_bytes()).is_err());
}

#[test]
fn missing_ancestor_collision_and_uid_exhaustion_fail_before_any_staging() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/grand", &mut Cursor::new(b"retained file"))
        .unwrap();
    volume.sync_batch().unwrap();
    let template = prepare(&mut volume, "/template", NodeMutation::CreateDirectory);
    let mut context = template.context().clone();
    context.path = "/grand/parent/new".into();
    context.parent = Uuid::new_v4();
    context.parent_version = 7;
    let canonical = logical_chain_on_another_volume(&context, &volume.index().root);
    let before = media(&library);
    let root = volume.index().root.clone();
    let error = NodeMutationPlan::prepare(
        context.clone(),
        NodeMutation::CreateDirectory,
        volume.index(),
        TIME,
        &canonical,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("文件或链接"));
    let mut missing = volume.index().clone();
    missing.root.files.clear();
    missing.highest_file_uid = u64::MAX;
    let error = NodeMutationPlan::prepare(
        context,
        NodeMutation::CreateDirectory,
        &missing,
        TIME,
        &canonical,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("fileuid 已耗尽"));
    assert_eq!(volume.index().root, root);
    assert_eq!(media(&library), before);
}

#[test]
fn maximum_depth_missing_ancestors_keep_the_real_source_untouched() {
    let (_library, dev) = formatted();
    let mut volume = mounted(&dev);
    let template = prepare(&mut volume, "/template", NodeMutation::CreateDirectory);
    let mut context = template.context().clone();
    context.parent = Uuid::from_u128(1024);
    let mut ancestors = Vec::new();
    let mut path = String::new();
    for offset in 0..1024 {
        if offset > 0 {
            path.push_str("/a");
        }
        let mut meta = volume.index().root.meta.clone();
        meta.file_uid = 100_000 + offset;
        ancestors.push(NodeMutationAncestor {
            path: if offset == 0 {
                "/".into()
            } else {
                path.clone()
            },
            id: Uuid::from_u128(u128::from(offset) + 1),
            version: 1,
            attributes: NodeAttributes::new(meta, vec![], None).unwrap(),
        });
    }
    context.path = format!("{path}/n");
    assert_eq!(context.path.len(), 2048);
    let plan = NodeMutationPlan::prepare(
        context,
        NodeMutation::CreateDirectory,
        volume.index(),
        TIME,
        &ancestors,
        None,
    )
    .unwrap();
    assert!(volume.index().root.subdirs.is_empty());
    assert_eq!(
        plan.logical_attributes_after()
            .unwrap()
            .unwrap()
            .meta()
            .unwrap()
            .file_uid,
        1025
    );
    commit(&mut volume, &plan);
    drop(volume);
    let mut volume = mounted(&dev);
    let evidence = volume.node_mutation_evidence(&plan).unwrap();
    assert_eq!(evidence.ancestors().len(), 1024);
    let next = prepare(&mut volume, "/next-at-root", NodeMutation::CreateDirectory);
    let absent = volume.node_mutation_absence_evidence(&next).unwrap();
    assert_eq!(absent.kind(), NodeMutationAbsenceKind::Unchanged);
    assert_eq!(absent.file_count(), 0);
}
#[test]
fn ee_metadata_after_a_directory_move_keeps_the_actual_data_target_and_original_logical_identity() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .append_file("/.LTFSEE_DATA/data", &mut Cursor::new(b"kept body"))
        .unwrap();
    volume
        .add_symlink("/deep/branch/alias", "../../.LTFSEE_DATA/data")
        .unwrap();
    volume.add_symlink("/outside", ".LTFSEE_DATA/data").unwrap();
    volume.commit().unwrap();
    let data = volume
        .index()
        .find_file("/.LTFSEE_DATA/data")
        .unwrap()
        .clone();
    let alias = volume
        .index()
        .find_file("/deep/branch/alias")
        .unwrap()
        .clone();
    let previous = volume.verified_committed_index_construct().unwrap().into();
    let parent = Uuid::new_v4();
    let context = NodeMutationContext {
        library_serial: "SIM-NODES".into(),
        namespace: Uuid::new_v4(),
        pool: Uuid::new_v4(),
        operation: Uuid::new_v4(),
        request_epoch: 1,
        owner_epoch: 100,
        parent,
        parent_version: 1,
        node: Uuid::new_v4(),
        expected_version: Some(1),
        scan: Uuid::new_v4(),
        previous,
        path: "/deep/branch/alias".into(),
    };
    let ancestors: Vec<_> = ["/", "/deep", "/deep/branch"]
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let directory = volume.index().find_directory(path).unwrap();
            NodeMutationAncestor {
                path: path.into(),
                id: if i == 2 { parent } else { Uuid::new_v4() },
                version: 1,
                attributes: NodeAttributes::from_directory(directory).unwrap(),
            }
        })
        .collect();
    // The original logical value came from a root-level alias. A directory
    // move changes physical relative links without rewriting every child H/N.
    let mut logical_meta = alias.meta.clone();
    logical_meta.file_uid = 100_000;
    let logical = NodeAttributes::new(
        logical_meta,
        alias.xattrs.clone(),
        Some(".LTFSEE_DATA/data".into()),
    )
    .unwrap();
    let mutation = NodeMutation::ChangeMetadata {
        readonly: Some(true),
        access_time: None,
        modify_time: None,
    };
    let before = media(&library);
    let plan=NodeMutationPlan::prepare(context.clone(),mutation.clone(),volume.index(),TIME,&ancestors,Some((NodeKind::File,&logical)))
        .expect("EE physical link depth may differ from retained logical representation of the same data object");
    assert_eq!(media(&library), before, "planning emits no writes");
    assert_eq!(
        NodeMutationPlan::decode(&plan.encode().unwrap()).unwrap(),
        plan
    );
    let projected = plan.logical_attributes_after().unwrap().unwrap();
    assert_eq!(projected.meta().unwrap().file_uid, 100_000);
    assert_eq!(projected.symlink(), Some("../../.LTFSEE_DATA/data"));
    for target in [
        ".LTFSEE_DATA/data",
        "../.LTFSEE_DATA/data",
        "./.LTFSEE_DATA/folder/../data",
        "../../.LTFSEE_DATA/data",
    ] {
        let same_data = NodeAttributes::new(
            logical.meta().unwrap().clone(),
            logical.xattrs().unwrap().to_vec(),
            Some(target.into()),
        )
        .unwrap();
        let equivalent = NodeMutationPlan::prepare(
            context.clone(),
            mutation.clone(),
            volume.index(),
            TIME,
            &ancestors,
            Some((NodeKind::File, &same_data)),
        )
        .unwrap();
        assert_eq!(
            equivalent
                .logical_attributes_after()
                .unwrap()
                .unwrap()
                .symlink(),
            alias.symlink.as_deref()
        );
    }
    for target in [
        ".LTFSEE_DATA/another",
        "/.LTFSEE_DATA/data",
        ".LTFSEE_DATA//data",
        "../other/.LTFSEE_DATA/data",
    ] {
        let wrong = NodeAttributes::new(
            logical.meta().unwrap().clone(),
            logical.xattrs().unwrap().to_vec(),
            Some(target.into()),
        )
        .unwrap();
        assert!(
            NodeMutationPlan::prepare(
                context.clone(),
                mutation.clone(),
                volume.index(),
                TIME,
                &ancestors,
                Some((NodeKind::File, &wrong))
            )
            .is_err()
        );
    }
    let mut wrong_kind_attrs = logical.xattrs().unwrap().to_vec();
    wrong_kind_attrs.push(attr("tapers.fdb.nativeSymlink", "1"));
    let wrong_kind = NodeAttributes::new(
        logical.meta().unwrap().clone(),
        wrong_kind_attrs,
        alias.symlink.clone(),
    )
    .unwrap();
    assert!(
        NodeMutationPlan::prepare(
            context,
            mutation,
            volume.index(),
            TIME,
            &ancestors,
            Some((NodeKind::File, &wrong_kind))
        )
        .is_err()
    );
    commit(&mut volume, &plan);
    let after = volume.index().find_file("/deep/branch/alias").unwrap();
    assert_eq!(after.meta.file_uid, alias.meta.file_uid);
    assert!(after.meta.readonly);
    assert_eq!(after.symlink, alias.symlink);
    assert_eq!(
        volume.index().find_file("/.LTFSEE_DATA/data").unwrap(),
        &data
    );
    let mut body = Vec::new();
    volume
        .read_file_to_writer("/deep/branch/alias", &mut body)
        .unwrap();
    assert_eq!(body, b"kept body");
    body.clear();
    volume.read_file_to_writer("/outside", &mut body).unwrap();
    assert_eq!(body, b"kept body");
}

#[test]
fn native_link_with_ee_like_text_keeps_literal_target_after_a_directory_move() {
    let (library, dev) = formatted();
    let mut volume = mounted(&dev);
    volume
        .add_symlink("/deep/branch/native", "../../.LTFSEE_DATA/data")
        .unwrap();
    volume
        .set_node_xattr(
            "/deep/branch/native",
            attr("tapers.fdb.nativeSymlink", "1"),
            false,
            false,
        )
        .unwrap();
    volume.commit().unwrap();
    let reference = prepare(
        &mut volume,
        "/deep/branch/native",
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    );
    let context = reference.context().clone();
    let ancestors: Vec<_> = ["/", "/deep", "/deep/branch"]
        .into_iter()
        .enumerate()
        .map(|(i, path)| NodeMutationAncestor {
            path: path.into(),
            id: if i == 2 {
                context.parent
            } else {
                Uuid::new_v4()
            },
            version: 1,
            attributes: NodeAttributes::from_directory(
                volume.index().find_directory(path).unwrap(),
            )
            .unwrap(),
        })
        .collect();
    let node = volume.index().find_file("/deep/branch/native").unwrap();
    let wrong = NodeAttributes::new(
        node.meta.clone(),
        node.xattrs.clone(),
        Some(".LTFSEE_DATA/data".into()),
    )
    .unwrap();
    let before = media(&library);
    assert!(
        NodeMutationPlan::prepare(
            context,
            reference.mutation().clone(),
            volume.index(),
            TIME,
            &ancestors,
            Some((NodeKind::File, &wrong))
        )
        .is_err()
    );
    assert_eq!(media(&library), before);
}
