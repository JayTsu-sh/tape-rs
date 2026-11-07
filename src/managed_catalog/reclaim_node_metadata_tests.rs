use super::*;
use crate::ltfs::index::{DirectoryNode, Extent, FileNode, Xattr};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{RecoveryBudget, capture_index_source, capture_recorded_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary};
use crate::tape::commands::TapeDrive;

fn reservation(inventory: &ReclaimSourceInventory<'_>) -> ReclaimReservation {
    let ns = Uuid::new_v4();
    ReclaimReservation {
        route: ActiveLibraryRoute {
            route: PreparedLibraryRoute {
                namespace: ns,
                revision: 1,
            },
            frozen_index: 1,
            certificate_sha256: [1; 32],
            initial_owner_epoch: 2,
        },
        intent: json!({"version":1,"library":"NODE-LIB","namespace":ns.to_string(),
            "epoch":1,"operation":Uuid::new_v4().to_string(),"scan":Uuid::new_v4().to_string(),
            "scan_generation":1,"index":inventory.summary.head,
            "change":{"kind":"media-reclaim","pool":Uuid::new_v4().to_string(),
                "barcode":"NODE00L8","volume":inventory.summary.volume,
                "revision":1,"confirm_format":true,"unassign":false}}),
    }
}

fn attr() -> Xattr {
    Xattr {
        key: "user.binary".into(),
        value: "AP9B".into(),
        base64: true,
    }
}

#[test]
fn actual_root_empty_directory_and_native_link_readback_binds_original_nodes_and_target_head() {
    let library = SimLibrary::new(2, 4, 1);
    for (i, barcode) in ["NODE00L8", "NODE01L8"].into_iter().enumerate() {
        library
            .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), i)
            .unwrap();
        library.load_into_drive(barcode, i).unwrap();
        mkltfs(
            &library.drive(i),
            &MkltfsOptions {
                volume_id: barcode[..6].into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let source_device = library.drive(0);
    let target_device = library.drive(1);
    let mut source_volume = LtfsVolume::mount(&source_device).unwrap();
    source_volume.create_directory("/empty").unwrap();
    source_volume.add_symlink("/link", "../missing").unwrap();
    source_volume
        .set_node_xattr("/link", link_kind::native_marker(), false, false)
        .unwrap();
    source_volume
        .set_node_xattr("/empty", attr(), false, false)
        .unwrap();
    source_volume
        .set_node_xattr("/", attr(), false, false)
        .unwrap();
    source_volume.set_node_readonly("/empty", true).unwrap();
    source_volume.commit().unwrap();
    drop(source_volume);
    let source = capture_index_source(
        &TapeDrive::new(&source_device),
        &read_label_only(&source_device).unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&source).unwrap();
    let parent = reservation(&inventory);
    let mut entries = Vec::new();
    source
        .for_each_entry(|entry| {
            entries.push(entry);
            Ok(())
        })
        .unwrap();
    assert_eq!(entries.len(), 3);
    let mut target_volume = LtfsVolume::mount(&target_device).unwrap();
    target_volume.create_directory("/padding").unwrap();
    target_volume.create_directory("/empty").unwrap();
    target_volume.add_symlink("/link", "../missing").unwrap();
    for entry in &entries {
        let path = match entry {
            IndexEntry::Directory { path, .. } | IndexEntry::File { path, .. } => path,
        };
        target_volume
            .preserve_reclaimed_metadata(&format!("/{path}"), entry)
            .unwrap();
    }
    target_volume.commit_incremental().unwrap();
    let head = ActiveIndexIdentity::from(target_volume.committed_index_construct());
    assert!(head.incremental);
    drop(target_volume);
    let label = read_label_only(&target_device).unwrap();
    let drive = TapeDrive::new(&target_device);
    let target = capture_index_source(&drive, &label, &RecoveryBudget::default()).unwrap();
    let recorded = capture_recorded_index_source(
        &drive,
        &label,
        &target.identity().unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap();
    let placement = VolumePlacement {
        volume_id: head.volume_uuid,
        namespace: parent.namespace(),
        pool_id: parent.spec().pool(),
        library_serial: parent.serial().into(),
        barcode: "NODE01L8".into(),
    };
    let before_source = source_device.command_log();
    let before_target = target_device.command_log();
    for entry in &entries {
        let original = ReclaimSourceNode(Object::capture(entry.clone()).unwrap());
        let mut target_uid = None;
        target
            .for_each_entry(|e| {
                let (path, uid) = match e {
                    IndexEntry::Directory { path, node } => (path, node.meta.file_uid),
                    IndexEntry::File { path, node } => (path, node.meta.file_uid),
                };
                if path == original.path() {
                    target_uid = Some(uid);
                }
                Ok(())
            })
            .unwrap();
        let destination = ReclaimNodeDestination {
            volume: head.volume_uuid,
            uid: target_uid.unwrap(),
        };
        let observed = inventory
            .bind_verified_node_metadata(&parent, &original, destination, &placement, head, &target)
            .unwrap();
        assert_eq!(observed.source_uid(), original.uid());
        assert_eq!(observed.destination(), destination);
        assert_eq!(observed.target_path(), original.path());
        assert_eq!(
            inventory
                .bind_verified_node_metadata(
                    &parent,
                    &original,
                    destination,
                    &placement,
                    head,
                    &target
                )
                .unwrap(),
            observed
        );
        let mut wrong_head = head;
        wrong_head.generation += 1;
        assert!(
            inventory
                .bind_verified_node_metadata(
                    &parent,
                    &original,
                    destination,
                    &placement,
                    wrong_head,
                    &target
                )
                .is_err()
        );
        assert!(
            inventory
                .bind_verified_node_metadata(
                    &parent,
                    &original,
                    destination,
                    &placement,
                    head,
                    &recorded
                )
                .is_err()
        );
        for bad in [
            ReclaimNodeDestination {
                volume: Uuid::nil(),
                ..destination
            },
            ReclaimNodeDestination {
                uid: 0,
                ..destination
            },
            ReclaimNodeDestination {
                uid: u64::MAX,
                ..destination
            },
            ReclaimNodeDestination {
                volume: parent.spec().volume(),
                ..destination
            },
        ] {
            assert!(
                inventory
                    .bind_verified_node_metadata(&parent, &original, bad, &placement, head, &target)
                    .is_err()
            );
        }
        for wrong in [
            VolumePlacement {
                namespace: Uuid::new_v4(),
                ..placement.clone()
            },
            VolumePlacement {
                pool_id: Uuid::new_v4(),
                ..placement.clone()
            },
            VolumePlacement {
                library_serial: "OTHER-LIB".into(),
                ..placement.clone()
            },
        ] {
            assert!(
                inventory
                    .bind_verified_node_metadata(
                        &parent,
                        &original,
                        destination,
                        &wrong,
                        head,
                        &target
                    )
                    .is_err()
            );
        }
        let mut altered = original.clone();
        altered.0.sha256[0] ^= 1;
        assert!(
            inventory
                .bind_verified_node_metadata(
                    &parent,
                    &altered,
                    destination,
                    &placement,
                    head,
                    &target
                )
                .is_err()
        );
    }
    assert_eq!(source_device.command_log(), before_source);
    assert_eq!(target_device.command_log(), before_target);
}

#[test]
fn node_metadata_rejects_changed_attributes_type_and_ee_alias_as_a_native_link() {
    let header = crate::ltfs::index::LtfsIndex::empty(Uuid::new_v4(), "proof".into(), 'b');
    let original = IndexEntry::Directory {
        path: "".into(),
        node: DirectoryNode {
            name: "".into(),
            meta: header.root.meta.clone(),
            xattrs: vec![attr()],
            ..Default::default()
        },
    };
    let IndexEntry::Directory { node, .. } = &original else {
        unreachable!()
    };
    for bad in [
        DirectoryNode {
            meta: crate::ltfs::index::NodeMeta {
                readonly: !node.meta.readonly,
                ..node.meta.clone()
            },
            ..node.clone()
        },
        DirectoryNode {
            xattrs: vec![],
            ..node.clone()
        },
        DirectoryNode {
            xattrs: vec![Xattr {
                base64: false,
                ..attr()
            }],
            ..node.clone()
        },
        DirectoryNode {
            meta: crate::ltfs::index::NodeMeta {
                backup_time: "invalid\0time".into(),
                ..node.meta.clone()
            },
            ..node.clone()
        },
    ] {
        assert!(
            require_metadata(
                &original,
                &IndexEntry::Directory {
                    path: "".into(),
                    node: bad
                }
            )
            .is_err()
        );
    }
    let native = IndexEntry::File {
        path: "link".into(),
        node: FileNode {
            name: "link".into(),
            meta: header.root.meta.clone(),
            symlink: Some(".LTFSEE_DATA/missing".into()),
            xattrs: vec![link_kind::native_marker(), attr()],
            ..Default::default()
        },
    };
    assert!(
        require_metadata(&native, &native).is_ok(),
        "显式原生链接不转换成EE别名"
    );
    assert!(require_metadata(&original, &native).is_err());
    let IndexEntry::File { node, .. } = &native else {
        unreachable!()
    };
    for bad in [
        FileNode {
            xattrs: vec![attr()],
            ..node.clone()
        },
        FileNode {
            symlink: Some("changed".into()),
            ..node.clone()
        },
        FileNode {
            symlink: None,
            ..node.clone()
        },
        FileNode {
            extents: vec![Extent {
                partition: 'b',
                start_block: 10,
                byte_count: 1,
                byte_offset: 0,
                file_offset: 0,
            }],
            ..node.clone()
        },
        FileNode {
            open_for_write: Some(true),
            ..node.clone()
        },
        FileNode {
            length: 1,
            ..node.clone()
        },
    ] {
        assert!(
            require_metadata(
                &native,
                &IndexEntry::File {
                    path: "link".into(),
                    node: bad
                }
            )
            .is_err()
        );
    }
}
