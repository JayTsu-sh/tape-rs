//! Compact superseded physical descriptions without forgetting immutable IDs.

use super::*;
use std::collections::BTreeMap;

const MAX_COMPACT_PAGE: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyCompactionPage {
    pub scanned: usize,
    /// Count for the final successful attempt. An unknown-commit retry may
    /// undercount work already committed; this is not a durable audit counter.
    pub compacted: usize,
    /// Expired superseded records removed; also a retry-local count.
    pub forgotten: usize,
    /// Resume after this key, even if the preceding page compacted no records.
    /// None ends this sweep, not a snapshot of all concurrent updates.
    pub next: Option<Vec<u8>>,
}

fn copy_ids(namespace: Uuid, key: &[u8]) -> Result<(Uuid, Uuid)> {
    let prefix = copy_prefix(namespace);
    if !key.starts_with(&prefix) || key.len() != prefix.len() + 32 {
        return Err(CatalogError::Corrupt("历史副本键不属于命名空间或长度错误"));
    }
    let file = uuid_from(&key[prefix.len()..prefix.len() + 16])?;
    let copy = uuid_from(&key[prefix.len() + 16..])?;
    if file.is_nil() || copy.is_nil() {
        return Err(CatalogError::Corrupt("历史副本引用 nil ID"));
    }
    Ok((file, copy))
}

pub(super) async fn compact_page(
    tr: &Transaction,
    namespace: Uuid,
    after: Option<&[u8]>,
    limit: usize,
) -> Result<CopyCompactionPage> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    if state.as_deref().map(decode_namespace_state).transpose()? != Some(NamespaceState::Ready) {
        return Err(CatalogError::Incomplete);
    }
    let window = requests::window(tr, namespace).await?;
    let mut pins = key(b'F');
    pins.extend_from_slice(namespace.as_bytes());
    if requests::prefix_has_rows(tr, &pins).await? {
        return Err(CatalogError::Incomplete);
    }
    let page = read_page(tr, &copy_prefix(namespace), after, limit, false).await?;
    let scanned = page.items.len();
    let mut records = Vec::with_capacity(scanned);
    let mut files = BTreeMap::new();
    for (key, value) in page.items {
        let (file, id) = copy_ids(namespace, &key)?;
        let stored = decode_stored_copy(&value)?;
        files.entry(file).or_insert_with(|| {
            (
                tr.get(&node_key(namespace, file), false),
                tr.get(&head_key(namespace, file), false),
            )
        });
        records.push((key, file, id, stored));
    }
    let mut primaries = Vec::with_capacity(files.len());
    for (file, (node, head)) in files {
        let node = node.await?;
        let node = decode_node(
            node.as_deref()
                .ok_or(CatalogError::Corrupt("历史副本的文件 inode 缺失"))?,
        )?;
        if node.kind != NodeKind::File {
            return Err(CatalogError::Corrupt("历史副本的 inode 不是文件"));
        }
        let head = head.await?;
        let head = decode_head(
            head.as_deref()
                .ok_or(CatalogError::Corrupt("历史副本的文件 head 缺失"))?,
        )?;
        let primary = tr.get(&copy_key(namespace, file, head.copy_id), false);
        primaries.push((file, head, primary));
    }
    let mut heads = BTreeMap::new();
    for (file, head, primary) in primaries {
        let primary = primary.await?;
        // A retired current copy still describes the latest known file. Never
        // require it to be online or replace its descriptor with a tombstone.
        let primary = decode_copy(
            primary
                .as_deref()
                .ok_or(CatalogError::Corrupt("当前副本缺失，禁止回收历史"))?,
        )?;
        if primary.version != head.version || primary.request_epoch > window.accepting {
            return Err(CatalogError::Corrupt("当前副本与 head 版本不一致"));
        }
        heads.insert(file, head);
    }
    let mut forget = Vec::new();
    let mut candidates = Vec::new();
    let mut scans = BTreeMap::new();
    for (key, file, id, stored) in records {
        let head = &heads[&file];
        let version = stored.version();
        if version > head.version || (id == head.copy_id && version != head.version) {
            return Err(CatalogError::Corrupt("历史副本与当前 head 不一致"));
        }
        let copy = match stored {
            StoredCopy::Reclaimed {
                version,
                request_epoch,
            } => {
                if version >= head.version || id == head.copy_id {
                    return Err(CatalogError::Corrupt("当前副本或版本被错误回收"));
                }
                if request_epoch > window.accepting {
                    return Err(CatalogError::Corrupt("副本请求代次未开放"));
                }
                if request_epoch < window.first_retained {
                    forget.push(key);
                }
                continue;
            }
            StoredCopy::Present(copy) => copy,
        };
        if copy.request_epoch > window.accepting {
            return Err(CatalogError::Corrupt("副本请求代次未开放"));
        }
        if version == head.version {
            continue;
        }
        let scan = VolumeScan {
            generation: copy.volume_generation,
            id: copy.volume_scan_id,
        };
        scans.entry((copy.volume_id, scan.id)).or_insert_with(|| {
            (
                tr.get(&scan_key(copy.volume_id, scan.id), false),
                tr.get(&ready_volume_key(copy.volume_id), false),
            )
        });
        candidates.push((key, file, id, copy, scan));
    }
    let mut manifests = BTreeMap::new();
    for (id, (manifest, ready)) in scans {
        let manifest = manifest.await?;
        let manifest = decode_scan_manifest(
            manifest
                .as_deref()
                .ok_or(CatalogError::Corrupt("历史副本的扫描记录缺失"))?,
        )?;
        let ready = ready.await?;
        let ready = ready.as_deref().map(decode_ready_volume).transpose()?;
        manifests.insert(id, (manifest, ready));
    }
    // Domain errors must be found before adding any mutations: db.run commits
    // an Ok(Err(domain_error)) result just like any other successful closure.
    let mut mutations = Vec::new();
    for (key, file, id, copy, scan) in candidates {
        let ((saved, _, state), ready) = &manifests[&(copy.volume_id, scan.id)];
        if *saved != scan {
            return Err(CatalogError::Corrupt("历史副本的扫描身份不匹配"));
        }
        if *state != ScanState::Retired {
            continue;
        }
        if ready.as_ref().is_some_and(|(ready, _)| ready.id == scan.id) {
            return Err(CatalogError::Corrupt("退役扫描仍被就绪指针引用"));
        }
        mutations.push((
            key,
            if copy.request_epoch < window.first_retained {
                None
            } else {
                Some(encode_reclaimed_copy(copy.version, copy.request_epoch))
            },
            volume_key(copy.volume_id, scan.id, namespace, file, id),
        ));
    }
    let compacted = mutations
        .iter()
        .filter(|(_, marker, _)| marker.is_some())
        .count();
    let forgotten = forget.len() + mutations.len() - compacted;
    for key in forget {
        tr.clear(&key);
    }
    for (key, tombstone, reverse) in mutations {
        if let Some(marker) = tombstone {
            tr.set(&key, &marker);
        } else {
            tr.clear(&key);
        }
        tr.clear(&reverse);
    }
    Ok(CopyCompactionPage {
        scanned,
        compacted,
        forgotten,
        next: page.next,
    })
}

impl ManagedCatalog {
    /// Compact older-version descriptions only on retired scans in a serving
    /// namespace. Retain tombstones until the request window fences their epoch.
    /// Neither current-version replicas nor media contents are reclaimed.
    pub async fn compact_superseded_copies(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<CopyCompactionPage> {
        if namespace.is_nil() || !(1..=MAX_COMPACT_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid(
                "namespace 必须有效，历史回收批次必须在 1..=100",
            ));
        }
        if after.is_some_and(|key| copy_ids(namespace, key).is_err()) {
            return Err(CatalogError::Invalid("历史回收游标不属于当前命名空间"));
        }
        let after = after.map(<[u8]>::to_vec);
        self.db
            .run(|tr, _| {
                let after = after.clone();
                async move {
                    match compact_page(&tr, namespace, after.as_deref(), limit).await {
                        Ok(page) => Ok(Ok(page)),
                        Err(CatalogError::Fdb(error)) => Err(error),
                        Err(CatalogError::Client(error)) => Err(error.into()),
                        Err(error) => Ok(Err(error)),
                    }
                }
            })
            .await?
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn reclaimed_copy_codec_and_cursor_boundaries() {
        for version in [1, u64::MAX] {
            let encoded = encode_reclaimed_copy(version, 1);
            assert_eq!(encoded.len(), 17);
            assert!(
                matches!(decode_stored_copy(&encoded).unwrap(), StoredCopy::Reclaimed { version: v, request_epoch: 1 } if v == version)
            );
            assert!(decode_copy(&encoded).is_err());
            for length in 0..encoded.len() {
                assert!(decode_stored_copy(&encoded[..length]).is_err());
            }
        }
        assert!(decode_stored_copy(&encode_reclaimed_copy(0, 1)).is_err());
        let mut unknown = encode_reclaimed_copy(1, 1);
        unknown[0] = 0xfe;
        assert!(decode_stored_copy(&unknown).is_err());
        unknown.push(0);
        assert!(decode_stored_copy(&unknown).is_err());
        let namespace = Uuid::new_v4();
        let key = copy_key(namespace, Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(
            copy_ids(namespace, &key).unwrap(),
            (Uuid::from_u128(1), Uuid::from_u128(2))
        );
        assert!(copy_ids(Uuid::new_v4(), &key).is_err());
        assert!(copy_ids(namespace, &key[..key.len() - 1]).is_err());
        assert!(copy_ids(namespace, &copy_key(namespace, Uuid::nil(), Uuid::new_v4())).is_err());
    }

    async fn raw(catalog: &ManagedCatalog, key: &[u8]) -> Option<Vec<u8>> {
        catalog
            .db
            .create_trx()
            .unwrap()
            .get(key, true)
            .await
            .unwrap()
            .map(|v| v.to_vec())
    }

    async fn ready(catalog: &ManagedCatalog, serial: &str, volume: Uuid, scan: VolumeScan) {
        catalog
            .begin_volume_reconcile(serial, 10, volume, scan)
            .await
            .unwrap();
        super::super::tests::verify_scan(catalog, serial, 10, volume, scan, &[]).await;
        catalog
            .mark_volume_ready(serial, 10, volume, scan)
            .await
            .unwrap();
    }

    fn receipt(
        namespace: Uuid,
        serial: &str,
        volume: Uuid,
        scan: VolumeScan,
        file: u128,
        version: u64,
    ) -> CommitReceipt {
        test_receipt(CommitReceipt {
            request_epoch: 1,
            namespace,
            parent: Uuid::nil(),
            name: format!("file-{file}").into_bytes(),
            file_id: Uuid::from_u128(file),
            expected_version: if version == 1 {
                None
            } else {
                Some(version - 1)
            },
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::from_u128(version as u128),
            library_serial: serial.into(),
            pool_id: None,
            owner_epoch: 10,
            volume_id: volume,
            volume_generation: scan.generation,
            volume_scan_id: scan.id,
            ltfs_file_uid: 1,
            size: version,
            content_digest: ContentDigest::Sha256([3; 32]),
            metadata_digest: None,
        })
    }

    pub(crate) async fn exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let namespace = Uuid::new_v4();
        catalog.create_namespace(namespace).await.unwrap();
        let serial = format!("history-{}", Uuid::new_v4());
        catalog.advance_owner_epoch(&serial, 10).await.unwrap();
        let volume = Uuid::new_v4();
        let old_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        ready(catalog, &serial, volume, old_scan).await;
        let first = receipt(namespace, &serial, volume, old_scan, 1, 1);
        assert_eq!(
            test_publish(catalog, &first).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        let mut history = vec![first.clone()];
        for version in 2..=1001 {
            let r = receipt(namespace, &serial, volume, old_scan, 1, version);
            assert_eq!(
                test_publish(catalog, &r).await.unwrap(),
                PublishOutcome::Applied(version)
            );
            history.push(r);
        }
        let offline = receipt(namespace, &serial, volume, old_scan, 2, 1);
        test_publish(catalog, &offline).await.unwrap();
        let redundant = receipt(namespace, &serial, volume, old_scan, 4, 1);
        test_publish(catalog, &redundant).await.unwrap();
        let extra = CommitReceipt {
            copy_id: Uuid::from_u128(5),
            ..redundant.clone()
        };
        // The current public API has no add-replica verb yet. Install its valid
        // current-version secondary-copy shape to protect it from compaction.
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &copy_key(namespace, extra.file_id, extra.copy_id),
            &encode_copy(&extra, 1),
        );
        let mut reverse = 1u64.to_be_bytes().to_vec();
        reverse.extend_from_slice(&receipt_fingerprint(&extra));
        tr.set(
            &volume_key(volume, old_scan.id, namespace, extra.file_id, extra.copy_id),
            &reverse,
        );
        tr.commit().await.unwrap();
        let active_volume = Uuid::new_v4();
        let active_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        ready(catalog, &serial, active_volume, active_scan).await;
        let mut active_old_copy = Uuid::nil();
        for version in 1..=2 {
            let r = receipt(namespace, &serial, active_volume, active_scan, 3, version);
            if version == 1 {
                active_old_copy = r.copy_id;
            }
            test_publish(catalog, &r).await.unwrap();
        }
        let unchanged = catalog
            .compact_superseded_copies(namespace, None, 97)
            .await
            .unwrap();
        assert_eq!(unchanged.scanned, 97);
        assert_eq!(unchanged.compacted, 0);
        assert!(unchanged.next.is_some(), "零回收页仍须提供下一页游标");
        let new_scan = VolumeScan {
            generation: 2,
            id: Uuid::new_v4(),
        };
        ready(catalog, &serial, volume, new_scan).await;
        let in_flight = catalog.db.create_trx().unwrap();
        assert!(
            compact_page(&in_flight, namespace, None, 5)
                .await
                .unwrap()
                .compacted
                > 0
        );
        let current = receipt(namespace, &serial, volume, new_scan, 1, 1002);
        test_publish(catalog, &current).await.unwrap();
        assert!(
            in_flight.commit().await.is_err(),
            "head 更新必须使在途回收冲突"
        );
        let first_key = copy_key(namespace, first.file_id, first.copy_id);
        assert_eq!(raw(catalog, &first_key).await.unwrap().len(), 105);
        let in_flight = catalog.db.create_trx().unwrap();
        compact_page(&in_flight, namespace, None, 5).await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &ready_volume_key(volume),
            &encode_ready_volume(old_scan, &serial),
        );
        tr.commit().await.unwrap();
        assert!(
            in_flight.commit().await.is_err(),
            "就绪指针更新必须使在途回收冲突"
        );
        assert!(matches!(
            catalog.compact_superseded_copies(namespace, None, 5).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &ready_volume_key(volume),
            &encode_ready_volume(new_scan, &serial),
        );
        tr.commit().await.unwrap();

        let first_page = catalog
            .compact_superseded_copies(namespace, None, 97)
            .await
            .unwrap();
        assert!((96..=97).contains(&first_page.compacted));
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let mut scanned = first_page.scanned;
        let mut compacted = first_page.compacted;
        let mut cursor = first_page.next.clone();
        let mut pages = 1;
        while let Some(after) = cursor {
            let page = reopened
                .compact_superseded_copies(namespace, Some(&after), 97)
                .await
                .unwrap();
            assert!(page.scanned <= 97 && page.compacted <= page.scanned);
            scanned += page.scanned;
            compacted += page.compacted;
            cursor = page.next;
            pages += 1;
            assert!(pages < 20);
        }
        assert_eq!((scanned, compacted), (1007, 1001));
        assert!(pages > 10);
        let mut cursor = None;
        loop {
            let page = reopened
                .compact_superseded_copies(namespace, cursor.as_deref(), 100)
                .await
                .unwrap();
            assert_eq!(page.compacted, 0);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert!(
            catalog
                .legacy_copy_record_for_test(namespace, first.file_id, first.copy_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .legacy_copy_record_for_test(namespace, current.file_id, current.copy_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog
                .file_head(namespace, first.file_id)
                .await
                .unwrap()
                .unwrap()
                .version,
            1002
        );
        assert_eq!(
            catalog
                .resolve_child(namespace, Uuid::nil(), &first.name)
                .await
                .unwrap(),
            Some(first.file_id)
        );
        assert!(matches!(
            catalog
                .legacy_copy_record_for_test(namespace, offline.file_id, offline.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        assert!(
            catalog
                .file_head(namespace, offline.file_id)
                .await
                .unwrap()
                .is_some()
        );
        for r in [&current, &offline, &redundant, &extra] {
            assert_eq!(
                raw(catalog, &copy_key(namespace, r.file_id, r.copy_id))
                    .await
                    .unwrap()
                    .len(),
                105
            );
        }
        assert_eq!(
            raw(
                catalog,
                &copy_key(namespace, Uuid::from_u128(3), active_old_copy)
            )
            .await
            .unwrap()
            .len(),
            105
        );
        for r in &history {
            assert_eq!(
                raw(catalog, &copy_key(namespace, r.file_id, r.copy_id))
                    .await
                    .unwrap(),
                encode_reclaimed_copy(r.expected_version.unwrap_or(0) + 1, 1)
            );
            assert!(
                raw(
                    catalog,
                    &volume_key(volume, old_scan.id, namespace, r.file_id, r.copy_id)
                )
                .await
                .is_none()
            );
        }
        assert_eq!(
            test_publish(catalog, &first).await.unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        let changed = CommitReceipt {
            size: first.size + 1,
            ..first.clone()
        };
        assert_eq!(
            test_publish(catalog, &changed).await.unwrap(),
            PublishOutcome::OperationConflict
        );
        let reuse = CommitReceipt {
            operation_id: Uuid::new_v4(),
            copy_id: first.copy_id,
            expected_version: Some(1002),
            ..current.clone()
        };
        assert_eq!(
            test_publish(catalog, &reuse).await.unwrap(),
            PublishOutcome::InvalidCopyId
        );
        assert!(
            catalog
                .compact_superseded_copies(namespace, None, 0)
                .await
                .is_err()
        );
        assert!(
            catalog
                .compact_superseded_copies(namespace, None, 101)
                .await
                .is_err()
        );
        assert!(
            catalog
                .compact_superseded_copies(Uuid::new_v4(), first_page.next.as_deref(), 10)
                .await
                .is_err()
        );
        assert!(
            catalog
                .compact_superseded_copies(namespace, Some(&[0]), 10)
                .await
                .is_err()
        );
        let building = Uuid::new_v4();
        catalog.begin_import(building).await.unwrap();
        assert!(matches!(
            catalog.compact_superseded_copies(building, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        catalog.discard_import(building).await.unwrap();
        assert!(matches!(
            catalog.compact_superseded_copies(building, None, 10).await,
            Err(CatalogError::Incomplete)
        ));
        namespace_verification::tests::check_copy_rows(catalog, namespace).await;
        exercise_corruption(catalog, namespace, &first, &current).await;
    }

    async fn exercise_corruption(
        catalog: &ManagedCatalog,
        protected: Uuid,
        old: &CommitReceipt,
        current: &CommitReceipt,
    ) {
        for case in 0..11 {
            let namespace = Uuid::new_v4();
            catalog.create_namespace(namespace).await.unwrap();
            let tr = catalog.db.create_trx().unwrap();
            for file in 1..=2 {
                let first = CommitReceipt {
                    namespace,
                    file_id: Uuid::from_u128(file),
                    name: format!("file-{file}").into_bytes(),
                    ..old.clone()
                };
                let latest = CommitReceipt {
                    namespace,
                    file_id: first.file_id,
                    name: first.name.clone(),
                    copy_id: Uuid::from_u128(9999),
                    ..current.clone()
                };
                tr.set(
                    &node_key(namespace, first.file_id),
                    &encode_node(&Node {
                        kind: NodeKind::File,
                        parent: Uuid::nil(),
                        name: first.name.clone(),
                        attributes: None,
                    }),
                );
                tr.set(
                    &child_key(namespace, Uuid::nil(), &first.name),
                    first.file_id.as_bytes(),
                );
                tr.set(
                    &head_key(namespace, first.file_id),
                    &encode_head(&FileHead {
                        version: 2,
                        size: 2,
                        copy_id: latest.copy_id,
                    }),
                );
                tr.set(
                    &copy_key(namespace, first.file_id, first.copy_id),
                    &encode_copy(&first, 1),
                );
                tr.set(
                    &copy_key(namespace, latest.file_id, latest.copy_id),
                    &encode_copy(&latest, 2),
                );
                let mut reverse = 1u64.to_be_bytes().to_vec();
                reverse.extend_from_slice(&receipt_fingerprint(&first));
                tr.set(
                    &volume_key(
                        first.volume_id,
                        first.volume_scan_id,
                        namespace,
                        first.file_id,
                        first.copy_id,
                    ),
                    &reverse,
                );
                if file == 2 {
                    let old_key = copy_key(namespace, first.file_id, first.copy_id);
                    let primary_key = copy_key(namespace, latest.file_id, latest.copy_id);
                    match case {
                        0 => tr.clear(&head_key(namespace, first.file_id)),
                        1 => tr.clear(&node_key(namespace, first.file_id)),
                        2 => tr.clear(&primary_key),
                        3 => tr.set(&primary_key, &encode_reclaimed_copy(2, 1)),
                        4 => tr.set(&primary_key, &encode_copy(&latest, 3)),
                        5 => tr.set(&old_key, &encode_copy(&first, 3)),
                        6 => tr.set(&old_key, &[0]),
                        7 => tr.set(
                            &old_key,
                            &encode_copy(
                                &CommitReceipt {
                                    volume_generation: first.volume_generation + 1,
                                    ..first.clone()
                                },
                                1,
                            ),
                        ),
                        8 => tr.set(
                            &old_key,
                            &encode_copy(
                                &CommitReceipt {
                                    volume_id: Uuid::new_v4(),
                                    ..first.clone()
                                },
                                1,
                            ),
                        ),
                        9 => tr.set(&old_key, &encode_reclaimed_copy(2, 1)),
                        10 => tr.set(
                            &node_key(namespace, first.file_id),
                            &encode_node(&Node {
                                kind: NodeKind::Directory,
                                parent: Uuid::nil(),
                                name: first.name.clone(),
                                attributes: None,
                            }),
                        ),
                        _ => unreachable!(),
                    }
                }
            }
            tr.commit().await.unwrap();
            assert!(
                matches!(
                    catalog
                        .compact_superseded_copies(namespace, None, 100)
                        .await,
                    Err(CatalogError::Corrupt(_))
                ),
                "损坏用例 {case}"
            );
            let first_key = copy_key(namespace, Uuid::from_u128(1), old.copy_id);
            assert_eq!(
                raw(catalog, &first_key).await.unwrap().len(),
                105,
                "错误页不得提交前项回收 {case}"
            );
            assert!(
                raw(
                    catalog,
                    &volume_key(
                        old.volume_id,
                        old.volume_scan_id,
                        namespace,
                        Uuid::from_u128(1),
                        old.copy_id
                    )
                )
                .await
                .is_some()
            );
            assert_eq!(
                catalog
                    .file_head(protected, current.file_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                1002
            );
            if case == 3 {
                assert!(matches!(
                    catalog
                        .legacy_copy_record_for_test(
                            namespace,
                            Uuid::from_u128(2),
                            Uuid::from_u128(9999)
                        )
                        .await,
                    Err(CatalogError::Corrupt(_))
                ));
            }
        }
    }
}
