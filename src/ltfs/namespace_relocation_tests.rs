use super::*;
use crate::ltfs::index::{Extent, IndexEntry};

fn meta(uid: u64) -> NodeMeta {
    NodeMeta {
        file_uid: uid,
        readonly: true,
        creation_time: "2020-01-02T03:04:05.000000001Z".into(),
        change_time: "2021-02-03T04:05:06.000000002Z".into(),
        modify_time: "2022-03-04T05:06:07.000000003Z".into(),
        access_time: "2023-04-05T06:07:08.000000004Z".into(),
        backup_time: "2024-05-06T07:08:09.000000005Z".into(),
    }
}
fn xattrs() -> Vec<Xattr> {
    vec![
        Xattr {
            key: "user.label".into(),
            value: "原始属性".into(),
            base64: false,
        },
        Xattr {
            key: "user.binary".into(),
            value: "AP9B".into(),
            base64: true,
        },
    ]
}
fn directory() -> IndexEntry {
    IndexEntry::Directory {
        path: "source".into(),
        node: DirectoryNode {
            name: "source".into(),
            meta: meta(900),
            xattrs: xattrs(),
            ..Default::default()
        },
    }
}
fn link() -> IndexEntry {
    IndexEntry::File {
        path: "source-link".into(),
        node: FileNode {
            name: "source-link".into(),
            meta: meta(901),
            xattrs: xattrs(),
            symlink: Some("../missing".into()),
            open_for_write: Some(false),
            ..Default::default()
        },
    }
}
fn index() -> LtfsIndex {
    let mut index = LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
    index.create_directory("/kept").unwrap();
    index.root.subdirs[0].files.push(FileNode {
        name: "body".into(),
        meta: NodeMeta {
            file_uid: 3,
            ..Default::default()
        },
        length: 4,
        extents: vec![Extent {
            partition: 'b',
            start_block: 100,
            byte_count: 4,
            byte_offset: 0,
            file_offset: 0,
        }],
        ..Default::default()
    });
    index.root.files.push(FileNode {
        name: "link".into(),
        meta: NodeMeta {
            file_uid: 4,
            ..Default::default()
        },
        symlink: Some("../missing".into()),
        ..Default::default()
    });
    index.highest_file_uid = 4;
    index
}
fn expected_meta(uid: u64) -> NodeMeta {
    NodeMeta {
        file_uid: uid,
        ..meta(900)
    }
}

#[test]
fn root_and_directory_preservation_keeps_children_uids_and_all_original_attributes() {
    let mut index = index();
    let children = index.root.subdirs.clone();
    let files = index.root.files.clone();
    let uid = index.root.meta.file_uid;
    index
        .preserve_reclaimed_metadata("/", &directory())
        .unwrap();
    assert_eq!(index.root.meta, expected_meta(uid));
    assert_eq!(index.root.xattrs, xattrs());
    assert_eq!(index.root.subdirs, children);
    assert_eq!(index.root.files, files);
    let parent = index.root.meta.clone();
    let dir = index.root.subdirs[0].clone();
    index
        .preserve_reclaimed_metadata("/kept", &directory())
        .unwrap();
    assert_eq!(index.root.subdirs[0].meta, expected_meta(dir.meta.file_uid));
    assert_eq!(index.root.subdirs[0].xattrs, xattrs());
    assert_eq!(index.root.subdirs[0].files, dir.files);
    assert_eq!(index.root.meta, parent, "属性迁移不改父目录时间");
    assert_eq!(index.highest_file_uid, 4);
}

#[test]
fn link_preservation_keeps_target_text_and_uid_without_dereferencing() {
    let mut index = index();
    let parent = index.root.clone();
    index.preserve_reclaimed_metadata("/link", &link()).unwrap();
    let node = &index.root.files[0];
    assert_eq!(node.meta, expected_meta(4));
    assert_eq!(node.xattrs, xattrs());
    assert_eq!(node.symlink.as_deref(), Some("../missing"));
    assert_eq!(node.open_for_write, Some(false));
    assert!(node.extents.is_empty());
    assert_eq!(node.length, 0);
    assert_eq!(index.root.meta, parent.meta);
    assert_eq!(index.root.subdirs, parent.subdirs);
}

#[test]
fn invalid_sources_targets_and_wrong_types_leave_the_entire_index_untouched() {
    let IndexEntry::Directory { node: dir, .. } = directory() else {
        unreachable!()
    };
    let IndexEntry::File { node: sym, .. } = link() else {
        unreachable!()
    };
    let bad_dirs = [
        DirectoryNode {
            delta_fields: Some(Default::default()),
            ..dir.clone()
        },
        DirectoryNode {
            files: vec![FileNode::default()],
            ..dir.clone()
        },
        DirectoryNode {
            subdirs: vec![DirectoryNode::default()],
            ..dir.clone()
        },
        DirectoryNode {
            meta: NodeMeta {
                file_uid: 0,
                ..dir.meta.clone()
            },
            ..dir.clone()
        },
        DirectoryNode {
            xattrs: vec![xattrs()[0].clone(), xattrs()[0].clone()],
            ..dir.clone()
        },
    ];
    let bad_links = [
        FileNode {
            symlink: None,
            ..sym.clone()
        },
        FileNode {
            open_for_write: Some(true),
            ..sym.clone()
        },
        FileNode {
            delta_fields: Some(Default::default()),
            ..sym.clone()
        },
        FileNode {
            extents: vec![Extent {
                partition: 'b',
                start_block: 3,
                byte_count: 1,
                byte_offset: 0,
                file_offset: 0,
            }],
            ..sym.clone()
        },
        FileNode {
            symlink: Some("different".into()),
            ..sym.clone()
        },
        FileNode {
            length: 1,
            ..sym.clone()
        },
    ];
    for source in bad_dirs
        .into_iter()
        .map(|node| IndexEntry::Directory {
            path: "source".into(),
            node,
        })
        .chain(bad_links.into_iter().map(|node| IndexEntry::File {
            path: "source".into(),
            node,
        }))
    {
        let mut index = index();
        let before = index.root.clone();
        let highest = index.highest_file_uid;
        let path = if matches!(source, IndexEntry::Directory { .. }) {
            "/kept"
        } else {
            "/link"
        };
        assert!(index.preserve_reclaimed_metadata(path, &source).is_err());
        assert_eq!(index.root, before);
        assert_eq!(index.highest_file_uid, highest);
    }
    for (path, source) in [
        ("/link", directory()),
        ("/kept", link()),
        ("/kept/body", link()),
        ("/missing", directory()),
        ("/missing/link", link()),
        ("/", link()),
        ("/../kept", directory()),
    ] {
        let mut index = index();
        let before = index.root.clone();
        assert!(index.preserve_reclaimed_metadata(path, &source).is_err());
        assert_eq!(index.root, before);
    }
}

fn media() -> (crate::scsi::sim::SimLibrary, crate::scsi::sim::SimTransport) {
    use crate::scsi::sim::{SimCartridge, SimLibrary};
    let library = SimLibrary::new(1, 2, 1);
    library
        .insert_cartridge(SimCartridge::blank("META00L8", 64 << 20), 0)
        .unwrap();
    library.load_into_drive("META00L8", 0).unwrap();
    let device = library.drive(0);
    crate::ltfs::mkltfs::mkltfs(
        &device,
        &crate::ltfs::mkltfs::MkltfsOptions {
            volume_id: "META00".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    (library, device)
}

#[test]
fn invalid_copied_values_leave_working_index_clean_and_send_no_cdb() {
    use crate::ltfs::volume::LtfsVolume;
    let IndexEntry::Directory { node, .. } = directory() else {
        unreachable!()
    };
    let mut invalid = Vec::new();
    for at in 0..5 {
        for bad in ["invalid\0XML", "", "2020-01-01T00:00:00Z"] {
            let mut source = node.clone();
            let times = [
                &mut source.meta.creation_time,
                &mut source.meta.change_time,
                &mut source.meta.modify_time,
                &mut source.meta.access_time,
                &mut source.meta.backup_time,
            ];
            *times.into_iter().nth(at).unwrap() = bad.into();
            invalid.push(source);
        }
    }
    for attr in [
        Xattr {
            key: "".into(),
            value: "value".into(),
            base64: false,
        },
        Xattr {
            key: "bad\0key".into(),
            value: "value".into(),
            base64: false,
        },
        Xattr {
            key: "user.bad".into(),
            value: "invalid\0XML".into(),
            base64: false,
        },
        Xattr {
            key: "user.bad".into(),
            value: "\u{1}".into(),
            base64: false,
        },
        Xattr {
            key: "user.bad".into(),
            value: "not base64!".into(),
            base64: true,
        },
    ] {
        invalid.push(DirectoryNode {
            xattrs: vec![attr],
            ..node.clone()
        });
    }
    let (_library, device) = media();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    let commands = device.command_log().len();
    assert!(!volume.needs_checkpoint());
    for node in invalid {
        let source = IndexEntry::Directory {
            path: "source".into(),
            node,
        };
        let mut index = index();
        let before = index.to_xml().unwrap();
        assert!(index.preserve_reclaimed_metadata("/", &source).is_err());
        assert_eq!(index.to_xml().unwrap(), before);
        assert!(volume.preserve_reclaimed_metadata("/", &source).is_err());
        assert!(!volume.needs_checkpoint(), "拒绝源输入不设置dirty");
        volume.commit_incremental().unwrap();
        assert_eq!(device.command_log().len(), commands);
    }
}

#[test]
fn preserved_metadata_is_unpublished_until_sync_and_survives_incremental_recovery() {
    use crate::ltfs::volume::LtfsVolume;
    let (library, device) = media();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    let bytes = b"already committed data";
    volume
        .append_file("/kept/body", &mut std::io::Cursor::new(bytes))
        .unwrap();
    volume.create_directory("/empty").unwrap();
    volume.add_symlink("/link", "../missing").unwrap();
    volume.commit().unwrap();
    let before = volume.index().root.clone();
    let count = device.command_log().len();
    for path in ["/kept", "/empty", "/"] {
        volume
            .preserve_reclaimed_metadata(path, &directory())
            .unwrap();
    }
    volume
        .preserve_reclaimed_metadata("/link", &link())
        .unwrap();
    assert_eq!(device.command_log().len(), count, "暂存属性不发送CDB");
    assert_eq!(volume.index().root, before, "S4之前不发布新属性");
    volume.commit_incremental().unwrap();
    assert!(volume.committed_index_construct().incremental);
    drop(volume);
    library.power_cut();
    let volume = LtfsVolume::mount(&device).unwrap();
    for path in ["/kept", "/empty", "/"] {
        let node = volume.index().find_directory(path).unwrap();
        let uid = if path == "/" {
            before.meta.file_uid
        } else {
            before
                .subdirs
                .iter()
                .find(|n| n.name == path.trim_start_matches('/'))
                .unwrap()
                .meta
                .file_uid
        };
        assert_eq!(node.meta, expected_meta(uid));
        assert_eq!(node.xattrs, xattrs());
    }
    let node = volume.index().find_file("link").unwrap();
    assert_eq!(node.meta, expected_meta(before.files[0].meta.file_uid));
    assert_eq!(node.xattrs, xattrs());
    assert_eq!(
        volume.index().find_file("kept/body").unwrap(),
        &before.subdirs[0].files[0]
    );
    let mut out = Vec::new();
    volume.read_file_to_writer("/kept/body", &mut out).unwrap();
    assert_eq!(out, bytes);
}

#[test]
fn indeterminate_metadata_sync_freezes_without_replaying_or_publishing() {
    use crate::ltfs::volume::LtfsVolume;
    use crate::scsi::{
        cdb::opcode,
        sim::{Fault, FaultAction},
    };
    for command in [opcode::WRITE_6, opcode::WRITE_ATTRIBUTE] {
        let (library, device) = media();
        let mut volume = LtfsVolume::mount(&device).unwrap();
        volume.create_directory("/kept").unwrap();
        volume.commit().unwrap();
        let before = volume.index().root.clone();
        let generation = volume.index().generation;
        volume
            .preserve_reclaimed_metadata("/kept", &directory())
            .unwrap();
        device.inject(Fault {
            opcode: command,
            action: FaultAction::CheckCondition {
                sense_key: 3,
                asc: 0x0c,
                ascq: 0,
            },
            after: 0,
            remaining: 1,
        });
        assert!(matches!(
            volume.commit_incremental(),
            Err(TapeError::CommitFailed { .. })
        ));
        assert_eq!(volume.index().root, before);
        assert!(!volume.writable());
        let count = device.command_log().len();
        assert!(
            volume
                .preserve_reclaimed_metadata("/kept", &directory())
                .is_err()
        );
        assert_eq!(
            device.command_log().len(),
            count,
            "未知提交后不继续写或重放"
        );
        drop(volume);
        library.power_cut();
        let recovered = LtfsVolume::mount(&device).unwrap();
        if command == opcode::WRITE_6 {
            assert_eq!(recovered.index().root, before);
        } else {
            assert_eq!(recovered.index().generation, generation + 1);
            assert_eq!(
                recovered.index().find_directory("kept").unwrap().meta,
                expected_meta(before.subdirs[0].meta.file_uid)
            );
        }
    }
}
