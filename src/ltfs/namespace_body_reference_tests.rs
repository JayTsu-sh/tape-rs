use super::*;
use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use crate::ltfs::volume::{HashPolicy, LtfsVolume};
use crate::scsi::sim::{Fault, FaultAction, SimCartridge, SimLibrary, SimTransport};
use std::io::Cursor;

fn media() -> (SimLibrary, SimTransport) {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("REF000L8", 64 << 20), 0)
        .unwrap();
    lib.load_into_drive("REF000L8", 0).unwrap();
    let device = lib.drive(0);
    mkltfs(
        &device,
        &MkltfsOptions {
            volume_id: "REF000".into(),
            block_size: 64 << 10,
            ..Default::default()
        },
    )
    .unwrap();
    (lib, device)
}

fn source(body: &FileNode) -> IndexEntry {
    let mut node = body.clone();
    node.name = "original-blob".into();
    node.meta.file_uid = 900;
    node.meta.readonly = true;
    for (i, time) in [
        &mut node.meta.creation_time,
        &mut node.meta.change_time,
        &mut node.meta.modify_time,
        &mut node.meta.access_time,
        &mut node.meta.backup_time,
    ]
    .into_iter()
    .enumerate()
    {
        *time = format!("2020-01-02T03:04:05.00000000{i}Z");
    }
    node.xattrs.push(Xattr {
        key: "user.binary".into(),
        value: "AP9B".into(),
        base64: true,
    });
    // These belong to another volume and must never reach the target.
    for extent in &mut node.extents {
        extent.start_block += 10_000;
    }
    IndexEntry::File {
        path: ".LTFSEE_DATA/original-blob".into(),
        node,
    }
}

fn seed(device: &SimTransport, bytes: &[u8]) -> FileNode {
    let mut volume = LtfsVolume::mount(device).unwrap();
    volume.set_hash_policy(HashPolicy {
        md5: true,
        sha256: true,
    });
    volume
        .append_file("/body", &mut Cursor::new(bytes))
        .unwrap();
    volume.create_directory("/metadata").unwrap();
    volume.commit().unwrap();
    volume.index().find_file("body").unwrap().clone()
}

#[test]
fn shared_body_preserves_raw_attributes_without_rewriting_data_and_recovers_both_indexes() {
    for bytes in [Vec::new(), (0..150_013).map(|i| (i % 251) as u8).collect()] {
        for incremental in [false, true] {
            let (lib, device) = media();
            let body = seed(&device, &bytes);
            let source = source(&body);
            let mut volume = LtfsVolume::mount(&device).unwrap();
            let before = volume.index().root.clone();
            let uid = volume.index().highest_file_uid + 1;
            let count = device.command_log().len();
            volume
                .reference_reclaimed_body(
                    "/metadata/blob",
                    uid,
                    "/body",
                    body.meta.file_uid,
                    &source,
                )
                .unwrap();
            assert_eq!(device.command_log().len(), count, "staging issues no CDB");
            assert_eq!(volume.index().root, before, "unpublished until S4");
            let count = device.command_log().len();
            if incremental {
                volume.commit_incremental().unwrap();
            } else {
                volume.commit().unwrap();
            }
            // Exactly one index WRITE; no second file-body WRITE is issued.
            assert_eq!(
                device.command_log()[count..]
                    .iter()
                    .filter(|op| **op == 0x0a)
                    .count(),
                if incremental { 1 } else { 2 }
            );
            drop(volume);
            lib.power_cut();
            let volume = LtfsVolume::mount(&device).unwrap();
            let after = volume.index().find_file("metadata/blob").unwrap();
            let IndexEntry::File { node: original, .. } = &source else {
                unreachable!()
            };
            let mut meta = original.meta.clone();
            meta.file_uid = uid;
            assert_eq!(after.meta, meta);
            assert_eq!(after.xattrs, original.xattrs);
            assert_eq!(after.extents, body.extents);
            assert_eq!(after.open_for_write, original.open_for_write);
            assert_eq!(volume.index().find_file("body"), Some(&body));
            assert_eq!(volume.index().root.meta, before.meta);
            assert_eq!(
                volume.index().find_directory("metadata").unwrap().meta,
                before.subdirs[0].meta
            );
            for path in ["/body", "/metadata/blob"] {
                let mut out = Vec::new();
                volume.read_file_to_writer(path, &mut out).unwrap();
                assert_eq!(out, bytes);
                assert!(matches!(
                    volume.verify_file(path).unwrap(),
                    crate::ltfs::volume::HashVerdict::Match { .. }
                ));
            }
        }
    }
}

#[test]
fn refused_shared_body_inputs_leave_the_whole_index_clean_and_issue_no_cdb() {
    let (_lib, device) = media();
    let body = seed(&device, b"original bytes");
    let source = source(&body);
    let IndexEntry::File { node, .. } = &source else {
        unreachable!()
    };
    let mut inputs = Vec::new();
    for case in 0..11 {
        let mut bad = node.clone();
        match case {
            0 => bad.meta.file_uid = 0,
            1 => bad.length += 1,
            2 => bad.symlink = Some("/body".into()),
            3 => bad.open_for_write = Some(true),
            4 => bad.delta_fields = Some(Default::default()),
            5 => bad.meta.creation_time = "not a time".into(),
            6 => bad.xattrs.push(Xattr {
                key: "bad".into(),
                value: "NUL\0".into(),
                base64: false,
            }),
            7 => bad.xattrs.push(Xattr {
                key: "bad".into(),
                value: "invalid*".into(),
                base64: true,
            }),
            8 => bad.xattrs.push(bad.xattrs[0].clone()),
            9 => bad.set_xattr(crate::ltfs::index::XATTR_SHA256, &"00".repeat(32)),
            _ => bad.set_xattr(XATTR_MD5, &"00".repeat(16)),
        }
        inputs.push(IndexEntry::File {
            path: "source".into(),
            node: bad,
        });
    }
    inputs.push(IndexEntry::Directory {
        path: "".into(),
        node: DirectoryNode::default(),
    });
    for source in inputs {
        let mut volume = LtfsVolume::mount(&device).unwrap();
        let before = format!("{:?}", volume.index());
        let count = device.command_log().len();
        assert!(
            volume
                .reference_reclaimed_body(
                    "/metadata/blob",
                    volume.index().highest_file_uid + 1,
                    "/body",
                    body.meta.file_uid,
                    &source
                )
                .is_err()
        );
        assert_eq!(format!("{:?}", volume.index()), before);
        assert!(!volume.needs_checkpoint());
        assert_eq!(device.command_log().len(), count);
        volume.commit().unwrap();
        assert_eq!(
            device.command_log().len(),
            count,
            "failed staging did not dirty working index"
        );
    }
    for (path, delta, donor, donor_uid) in [
        ("/body", 1, "/body", body.meta.file_uid),
        ("/metadata", 1, "/body", body.meta.file_uid),
        ("/missing/blob", 1, "/body", body.meta.file_uid),
        ("/metadata/blob", 0, "/body", body.meta.file_uid),
        ("/metadata/blob", 2, "/body", body.meta.file_uid),
        ("/metadata/blob", 1, "/body", 0),
        ("/metadata/blob", 1, "/missing", body.meta.file_uid),
        ("/../blob", 1, "/body", body.meta.file_uid),
        ("/", 1, "/body", body.meta.file_uid),
        ("/bad\0", 1, "/body", body.meta.file_uid),
    ] {
        let mut volume = LtfsVolume::mount(&device).unwrap();
        let count = device.command_log().len();
        assert!(
            volume
                .reference_reclaimed_body(
                    path,
                    volume.index().highest_file_uid + delta,
                    donor,
                    donor_uid,
                    &source
                )
                .is_err()
        );
        assert!(!volume.needs_checkpoint());
        volume.commit().unwrap();
        assert_eq!(device.command_log().len(), count);
    }
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_node_readonly("/body", true).unwrap();
    let count = device.command_log().len();
    assert!(
        volume
            .reference_reclaimed_body(
                "/metadata/blob",
                volume.index().highest_file_uid + 1,
                "/body",
                body.meta.file_uid,
                &source
            )
            .is_err()
    );
    assert_eq!(device.command_log().len(), count);
}

#[test]
fn shared_body_unknown_commit_freezes_without_replaying_or_publishing() {
    for opcode in [0x0a, 0x8d] {
        let (lib, device) = media();
        let body = seed(&device, b"committed body");
        let mut volume = LtfsVolume::mount(&device).unwrap();
        let source = source(&body);
        let before = volume.index().root.clone();
        volume
            .reference_reclaimed_body(
                "/metadata/blob",
                volume.index().highest_file_uid + 1,
                "/body",
                body.meta.file_uid,
                &source,
            )
            .unwrap();
        device.inject(Fault {
            opcode,
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
        let count = device.command_log().len();
        assert!(
            volume
                .reference_reclaimed_body(
                    "/metadata/other",
                    volume.index().highest_file_uid + 1,
                    "/body",
                    body.meta.file_uid,
                    &source
                )
                .is_err()
        );
        assert_eq!(device.command_log().len(), count);
        drop(volume);
        lib.power_cut();
        let recovered = LtfsVolume::mount(&device).unwrap();
        assert_eq!(recovered.index().find_file("body"), Some(&body));
        if opcode == 0x0a {
            assert!(recovered.index().find_file("metadata/blob").is_none());
        } else {
            assert_eq!(
                recovered
                    .index()
                    .find_file("metadata/blob")
                    .unwrap()
                    .extents,
                body.extents
            );
        }
    }
}
