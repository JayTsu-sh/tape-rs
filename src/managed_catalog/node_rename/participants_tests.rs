use super::*;
fn member(uid: u128, files: u64, target: bool, anchor: bool) -> NodeRenameParticipant {
    NodeRenameParticipant {
        volume: Uuid::from_u128(uid),
        source_files: files,
        target,
        anchor,
        recipe: [5; 32],
    }
}
fn page() -> ParticipantPage {
    ParticipantPage::new(
        PageFacts {
            version: 1,
            logical: [1; 32],
            manifest: [2; 32],
            index: 0,
            previous: [3; 32],
            members: vec![member(1, u64::MAX, false, false), member(2, 0, true, false)],
        },
        12,
    )
    .unwrap()
}
#[test]
fn participant_codec_preserves_unsigned_counts_and_original_facts_across_owner_takeover() {
    let page = page();
    let raw = encode_value(&page).unwrap();
    assert_eq!(ParticipantPage::decode(&raw).unwrap(), page);
    let other = ParticipantPage::new(page.facts.clone(), 13).unwrap();
    assert_ne!(encode_value(&other).unwrap(), raw);
    assert_eq!(page.chain().unwrap(), other.chain().unwrap());
    let mut summary = NodeRenameParticipantsSummary {
        volumes: 0,
        source_files: 0,
        target_volumes: 0,
        anchors: 0,
        sha256: [3; 32],
    };
    let mut last = None;
    add(
        &mut summary,
        &page,
        &mut last,
        None,
        Some(Uuid::from_u128(2)),
    )
    .unwrap();
    assert_eq!(
        (
            summary.volumes,
            summary.source_files,
            summary.target_volumes,
            summary.anchors
        ),
        (2, u64::MAX, 1, 0)
    );
    assert_eq!(last, Some(Uuid::from_u128(2)));
    assert_eq!(summary.sha256, page.chain().unwrap());
    assert!(
        add(
            &mut summary,
            &page,
            &mut last,
            None,
            Some(Uuid::from_u128(2))
        )
        .is_err()
    );
    let anchor = ParticipantPage::new(
        PageFacts {
            members: vec![member(7, 0, false, true)],
            ..page.facts.clone()
        },
        12,
    )
    .unwrap();
    let mut summary = NodeRenameParticipantsSummary {
        volumes: 0,
        source_files: 0,
        target_volumes: 0,
        anchors: 0,
        sha256: [3; 32],
    };
    add(
        &mut summary,
        &anchor,
        &mut None,
        Some(Uuid::from_u128(7)),
        None,
    )
    .unwrap();
    assert_eq!(
        (
            summary.volumes,
            summary.source_files,
            summary.target_volumes,
            summary.anchors
        ),
        (1, 0, 0, 1)
    );
}
#[test]
fn participant_codec_rejects_shape_order_roles_checksum_and_noncanonical_records() {
    let page = page();
    let raw = encode_value(&page).unwrap();
    for len in [0, 1, raw.len() - 1] {
        assert!(ParticipantPage::decode(&raw[..len]).is_err());
    }
    let mut extra = raw.clone();
    extra.push(b'\n');
    assert!(ParticipantPage::decode(&extra).is_err());
    assert!(ParticipantPage::decode(&vec![b' '; 64_001]).is_err());
    let duplicate = format!(
        "{{\"creator\":12,{}",
        std::str::from_utf8(&raw[1..]).unwrap()
    );
    assert!(ParticipantPage::decode(duplicate.as_bytes()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    value["unexpected"] = true.into();
    assert!(ParticipantPage::decode(&node_mutation::canonical(&value).unwrap()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    value["checksum"][0] = (u64::from(page.checksum[0]) ^ 0x80).into();
    assert!(ParticipantPage::decode(&node_mutation::canonical(&value).unwrap()).is_err());
    for members in [
        vec![],
        vec![member(0, 1, false, false)],
        vec![member(2, 1, false, false), member(1, 1, false, false)],
        vec![member(1, 1, false, false), member(1, 1, false, false)],
        vec![member(1, 0, false, false)],
        vec![member(1, 1, false, true)],
        vec![member(1, 0, true, true)],
        (1..=33).map(|id| member(id, 1, false, false)).collect(),
    ] {
        assert!(
            ParticipantPage::new(
                PageFacts {
                    members,
                    ..page.facts.clone()
                },
                12
            )
            .is_err()
        );
    }
    for owner in [0, 1 << 48] {
        assert!(ParticipantPage::new(page.facts.clone(), owner).is_err());
    }
    assert!(
        ParticipantPage::new(
            PageFacts {
                version: 2,
                ..page.facts.clone()
            },
            12
        )
        .is_err()
    );
    let mut summary = NodeRenameParticipantsSummary {
        volumes: 0,
        source_files: 0,
        target_volumes: 0,
        anchors: 0,
        sha256: [3; 32],
    };
    assert!(
        add(
            &mut summary,
            &page,
            &mut None,
            None,
            Some(Uuid::from_u128(1))
        )
        .is_err()
    );
}

#[test]
fn participant_merge_matches_exact_union_and_deduplicates_shared_source_target_across_pages() {
    use std::collections::BTreeMap;
    for count in [0u128, 1, 31, 32, 33, 70] {
        for target in [None, Some(1), Some(2), Some(32), Some(2 * count + 1)] {
            let mut rows: VecDeque<_> = (1..=count)
                .map(|id| NodeRenameProjectionVolume {
                    volume: Uuid::from_u128(id * 2),
                    files: id as u64,
                })
                .collect();
            let mut expected = BTreeMap::new();
            for row in &rows {
                expected.insert(row.volume, (row.files, false, false));
            }
            let original_target = target.map(Uuid::from_u128);
            if let Some(target) = original_target {
                expected.entry(target).or_insert((0, false, false)).1 = true;
            }
            let original_anchor = if count == 0 && target.is_none() {
                Some(Uuid::from_u128(999))
            } else {
                None
            };
            if let Some(anchor) = original_anchor {
                expected.insert(anchor, (0, false, true));
            }
            let mut target = original_target;
            let mut anchor = original_anchor;
            let mut actual = BTreeMap::new();
            let mut last = None;
            let mut summary = NodeRenameParticipantsSummary {
                volumes: 0,
                source_files: 0,
                target_volumes: 0,
                anchors: 0,
                sha256: [3; 32],
            };
            let mut index = 0;
            loop {
                let mut members = Vec::new();
                while members.len() < 32 {
                    let Some(member) = next_member(rows.front(), &mut target, &mut anchor) else {
                        break;
                    };
                    if rows.front().is_some_and(|row| row.volume == member.volume) {
                        rows.pop_front();
                    }
                    assert!(
                        actual
                            .insert(
                                member.volume,
                                (member.source_files, member.target, member.anchor)
                            )
                            .is_none()
                    );
                    members.push(member);
                }
                if members.is_empty() {
                    break;
                }
                let page = ParticipantPage::new(
                    PageFacts {
                        version: 1,
                        logical: [1; 32],
                        manifest: [2; 32],
                        index,
                        previous: summary.sha256,
                        members,
                    },
                    12,
                )
                .unwrap();
                add(
                    &mut summary,
                    &page,
                    &mut last,
                    original_anchor,
                    original_target,
                )
                .unwrap();
                index += 1;
            }
            assert_eq!(actual, expected);
            assert_eq!(summary.volumes, expected.len() as u64);
            assert_eq!(summary.source_files, (1..=count as u64).sum::<u64>());
        }
    }
}
