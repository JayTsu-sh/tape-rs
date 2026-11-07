use super::*;
use crate::ltfs::index::{Extent, FileNode, LtfsIndex, NodeMeta, Xattr};
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::recovery::{RecoveryBudget, capture_index_source, capture_recorded_index_source};
use crate::ltfs::volume::{LtfsVolume, read_label_only};
use crate::scsi::sim::{SimCartridge, SimLibrary, SimTransport};
use crate::tape::commands::TapeDrive;
use std::io::Cursor;

fn media() -> (SimLibrary, SimTransport) {
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("PROOF0L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("PROOF0L8", 0).unwrap();
    let drive = library.drive(0);
    mkltfs(
        &drive,
        &MkltfsOptions {
            volume_id: "PROOF0".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    (library, drive)
}

#[test]
fn source_inventory_includes_root_empty_directories_links_and_unmapped_ee_data() {
    let (_library, device) = media();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    for i in 0..40 {
        volume
            .append_file(&format!("/f{i:02}"), &mut Cursor::new(b"data"))
            .unwrap();
    }
    volume.create_directory("/empty").unwrap();
    volume
        .append_file("/.LTFSEE_DATA/unmapped", &mut Cursor::new(b"orphan"))
        .unwrap();
    volume.append_file("/zero", &mut Cursor::new(b"")).unwrap();
    volume.add_symlink("/opaque", "../outside").unwrap();
    volume
        .add_symlink("/alias", ".LTFSEE_DATA/unmapped")
        .unwrap();
    volume.commit().unwrap();
    drop(volume);
    let before = device.command_log().len();
    let source = capture_index_source(
        &TapeDrive::new(&device),
        &read_label_only(&device).unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap();
    let inventory = ReclaimSourceInventory::from_captured(&source).unwrap();
    assert_eq!(
        (
            inventory.objects(),
            inventory.directories(),
            inventory.files(),
            inventory.symlinks(),
            inventory.data_bytes()
        ),
        (47, 3, 42, 2, 166)
    );
    let first = inventory.page(None).unwrap();
    assert_eq!(first.len(), PAGE);
    assert_eq!(first[0].path, "");
    let last = inventory.page(Some(&first.last().unwrap().path)).unwrap();
    assert_eq!(last.len(), 15);
    assert!(
        inventory
            .page(Some(&last.last().unwrap().path))
            .unwrap()
            .is_empty()
    );
    let rows: Vec<_> = first.into_iter().chain(last).collect();
    assert!(
        rows.iter()
            .any(|row| row.path == ".LTFSEE_DATA/unmapped" && row.kind == Kind::File)
    );
    assert!(
        rows.iter()
            .any(|row| row.path == "empty" && row.kind == Kind::Directory)
    );
    assert!(
        rows.iter()
            .any(|row| row.path == "opaque" && row.kind == Kind::Symlink)
    );
    let mut digest = inventory.summary.seed();
    for (i, row) in rows.iter().enumerate() {
        digest = advance_digest(digest, i as u64 + 1, &row.encode().unwrap());
    }
    assert_eq!(digest, inventory.summary.digest);
    assert!(
        !device.command_log()[before..]
            .iter()
            .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
    );
}

#[test]
fn incremental_source_inventory_uses_replayed_nodes_and_refuses_recorded_scope() {
    let (_library, device) = media();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume
        .append_file("/gone", &mut Cursor::new(b"gone"))
        .unwrap();
    volume
        .append_file("/kept", &mut Cursor::new(b"kept"))
        .unwrap();
    volume.commit().unwrap();
    let baseline = volume.committed_index_construct();
    volume.remove_file("/gone").unwrap();
    volume.create_directory("/empty").unwrap();
    volume.rename_path("/kept", "/renamed").unwrap();
    volume.commit_incremental().unwrap();
    assert!(volume.committed_index_construct().incremental);
    drop(volume);
    let label = read_label_only(&device).unwrap();
    let drive = TapeDrive::new(&device);
    let before = device.command_log().len();
    let source = capture_index_source(&drive, &label, &RecoveryBudget::default()).unwrap();
    assert_eq!(source.predecessors().last(), Some(&baseline));
    let inventory = ReclaimSourceInventory::from_captured(&source).unwrap();
    assert_eq!(
        (
            inventory.objects(),
            inventory.directories(),
            inventory.files(),
            inventory.data_bytes()
        ),
        (3, 2, 1, 4)
    );
    assert_eq!(
        inventory
            .page(None)
            .unwrap()
            .iter()
            .map(|o| o.path.as_str())
            .collect::<Vec<_>>(),
        ["", "empty", "renamed"]
    );
    let recorded = capture_recorded_index_source(
        &drive,
        &label,
        &source.identity().unwrap(),
        &RecoveryBudget::default(),
    )
    .unwrap();
    assert!(ReclaimSourceInventory::from_captured(&recorded).is_err());
    assert!(
        !device.command_log()[before..]
            .iter()
            .any(|op| matches!(op, 0x04 | 0x0a | 0x10 | 0x19 | 0x8d))
    );
}

#[test]
fn source_inventory_rejects_partial_history_unknown_fields_and_ambiguous_objects() {
    let mut header = LtfsIndex::empty(Uuid::new_v4(), "proof".into(), 'b');
    let current = IndexSourceScope::Current {
        dp_eod: 12,
        ip_generation: Some(2),
        history_depth: 1,
        history_truncated: false,
    };
    assert!(current_proof(current, &header).is_ok());
    assert!(current_proof(IndexSourceScope::Recorded, &header).is_err());
    assert!(
        current_proof(
            IndexSourceScope::Current {
                dp_eod: 12,
                ip_generation: Some(2),
                history_depth: 1,
                history_truncated: true
            },
            &header
        )
        .is_err()
    );
    header.unknown_elements.push("vendor-mystery".into());
    assert!(current_proof(current, &header).is_err());
    let mut node = FileNode {
        name: "file".into(),
        meta: NodeMeta {
            file_uid: 2,
            ..header.root.meta.clone()
        },
        ..Default::default()
    };
    let original = Object::capture(IndexEntry::File {
        path: "file".into(),
        node: node.clone(),
    })
    .unwrap();
    node.xattrs.push(Xattr {
        key: "private".into(),
        value: "must-preserve".into(),
        base64: false,
    });
    assert_ne!(
        Object::capture(IndexEntry::File {
            path: "file".into(),
            node: node.clone()
        })
        .unwrap()
        .sha256,
        original.sha256
    );
    node.open_for_write = Some(true);
    assert!(
        Object::capture(IndexEntry::File {
            path: "file".into(),
            node: node.clone()
        })
        .is_err()
    );
    node.open_for_write = None;
    node.symlink = Some("target".into());
    node.extents.push(Extent {
        partition: 'b',
        start_block: 10,
        byte_offset: 0,
        byte_count: 1,
        file_offset: 0,
    });
    assert!(
        Object::capture(IndexEntry::File {
            path: "file".into(),
            node
        })
        .is_err()
    );
}
