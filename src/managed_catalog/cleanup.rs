//! Reclaim invisible imports without ever retiring a serving namespace or media.

use super::*;

const MAX_PURGE_PAGE: usize = 100;
// Copies must go first: they are the bounded reverse lookup for volume rows.
// The e index is released only by the explicit owner-fenced media check.
// The managed marker a is cleared after all assignments have been released.
const NAMESPACE_ROWS: &[u8] = b"RtjsquvwnlkhgfoNCHmMPLZIFABDKEeayYWTUX";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PurgeProgress {
    /// Namespace-owned rows removed; each copy may also remove one volume row.
    pub removed: usize,
    /// The namespace prefixes are empty. Orphan volume rows without a copy
    /// record require separate cleanup of their explicitly retired scan.
    pub complete: bool,
}

fn namespace_prefix(kind: u8, namespace: Uuid) -> Vec<u8> {
    let mut prefix = key(kind);
    prefix.extend_from_slice(namespace.as_bytes());
    prefix
}

/// Validate the whole copy page before scheduling any deletion. A caller must
/// explicitly retire its volume scans through the owner-fenced media workflow.
async fn retired_copy_indexes(
    tr: &Transaction,
    namespace: Uuid,
    rows: &[(Vec<u8>, Vec<u8>)],
) -> Result<Vec<Vec<u8>>> {
    let prefix = namespace_prefix(b'R', namespace);
    let mut lookups = Vec::with_capacity(rows.len());
    for (key, bytes) in rows {
        if key.len() != prefix.len() + 32 || !key.starts_with(&prefix) {
            return Err(CatalogError::Corrupt("待清理副本键长度"));
        }
        let file = uuid_from(&key[prefix.len()..prefix.len() + 16])?;
        let id = uuid_from(&key[prefix.len() + 16..])?;
        let copy = decode_copy(bytes)?;
        let scan = VolumeScan {
            generation: copy.volume_generation,
            id: copy.volume_scan_id,
        };
        lookups.push((
            scan,
            volume_key(copy.volume_id, scan.id, namespace, file, id),
            tr.get(&scan_key(copy.volume_id, scan.id), false),
            tr.get(&ready_volume_key(copy.volume_id), false),
        ));
    }
    let mut indexes = Vec::with_capacity(rows.len());
    for (scan, index, manifest, ready) in lookups {
        let manifest = manifest
            .await?
            .ok_or(CatalogError::Corrupt("待清理副本的扫描记录缺失"))?;
        let (saved, _, state) = decode_scan_manifest(&manifest)?;
        if saved != scan {
            return Err(CatalogError::Corrupt("待清理副本的扫描身份不匹配"));
        }
        if state != ScanState::Retired {
            return Err(CatalogError::Invalid("清理前必须显式退役候选引用的卷扫描"));
        }
        if let Some(ready) = ready.await? {
            let (referenced, _) = decode_ready_volume(&ready)?;
            if referenced.id == scan.id {
                return Err(CatalogError::Corrupt("退役扫描仍被就绪指针引用"));
            }
        }
        indexes.push(index);
    }
    Ok(indexes)
}

async fn purge_page(tr: &Transaction, namespace: Uuid, limit: usize) -> Result<PurgeProgress> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    if state.as_deref().map(decode_namespace_state).transpose()? != Some(NamespaceState::Discarded)
    {
        return Err(CatalogError::Invalid("只能清理显式废弃的导入候选"));
    }
    if requests::has_namespace_holds(tr, namespace).await? {
        return Err(CatalogError::Invalid(
            "候选仍有待处理请求或保留引用，禁止清理",
        ));
    }
    let mut removed = 0;
    for kind in NAMESPACE_ROWS {
        let prefix = namespace_prefix(*kind, namespace);
        let page = read_page(tr, &prefix, None, limit - removed, false).await?;
        if *kind == b'R' {
            let indexes = retired_copy_indexes(tr, namespace, &page.items).await?;
            for index in indexes {
                tr.clear(&index);
            }
        }
        if *kind == b'e' && !page.items.is_empty() {
            for (key, value) in &page.items {
                control::decode_namespace_assignment(namespace, key, value)?;
            }
            return if removed > 0 {
                Ok(PurgeProgress {
                    removed,
                    complete: false,
                })
            } else {
                Err(CatalogError::Invalid(
                    "候选仍有磁带归属，须先核对介质并显式释放",
                ))
            };
        }
        for (key, _) in &page.items {
            tr.clear(key);
        }
        removed += page.items.len();
        if removed == limit {
            return Ok(PurgeProgress {
                removed,
                complete: false,
            });
        }
    }
    // Keep S permanently. Reusing this namespace must never resurrect an old
    // receipt after its per-namespace operation markers have been reclaimed.
    Ok(PurgeProgress {
        removed,
        complete: true,
    })
}

impl ManagedCatalog {
    /// Permanently abandon a never-served import. Ready and Active namespaces
    /// are excluded even when empty; discard and seal race on the same state key.
    /// This does not cancel volume scans, change media pointers, or touch tapes.
    pub async fn discard_import(&self, namespace: Uuid) -> Result<bool> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace ID 不能为 nil"));
        }
        self.db
            .run(|tr, _| async move {
                let state_key = namespace_key(namespace);
                let state = tr.get(&state_key, false).await?;
                match state.as_deref().map(decode_namespace_state).transpose() {
                    Ok(Some(NamespaceState::Discarded)) => Ok(Ok(false)),
                    Ok(Some(NamespaceState::Ready | NamespaceState::Active)) => {
                        Ok(Err(CatalogError::Invalid("不能废弃已开放的命名空间")))
                    }
                    Ok(Some(_)) => {
                        match requests::has_namespace_holds(&tr, namespace).await {
                            Ok(false) => {}
                            Ok(true) => {
                                return Ok(Err(CatalogError::Invalid(
                                    "候选仍有待处理请求或保留引用，禁止废弃",
                                )));
                            }
                            Err(CatalogError::Fdb(error)) => return Err(error),
                            Err(CatalogError::Client(error)) => return Err(error.into()),
                            Err(error) => return Ok(Err(error)),
                        }
                        tr.set(&state_key, &[NamespaceState::Discarded as u8]);
                        Ok(Ok(true))
                    }
                    Ok(None) => Ok(Err(CatalogError::Incomplete)),
                    Err(error) => Ok(Err(error)),
                }
            })
            .await?
    }

    /// Delete at most 100 namespace rows plus their volume indexes per transaction.
    /// All referenced scans must already be retired. Repeat until complete; no
    /// caller cursor or in-memory inventory is needed to resume after a crash.
    pub async fn purge_discarded_namespace(
        &self,
        namespace: Uuid,
        limit: usize,
    ) -> Result<PurgeProgress> {
        if namespace.is_nil() || !(1..=MAX_PURGE_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid(
                "namespace 必须有效，候选清理批次必须在 1..=100",
            ));
        }
        self.db
            .run(|tr, _| async move {
                match purge_page(&tr, namespace, limit).await {
                    Ok(progress) => Ok(Ok(progress)),
                    Err(CatalogError::Fdb(error)) => Err(error),
                    Err(CatalogError::Client(error)) => Err(error.into()),
                    Err(error) => Ok(Err(error)),
                }
            })
            .await?
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn cleanup_prefixes_isolate_namespace_and_operations() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let op = Uuid::new_v4();
        assert_ne!(op_key(first, 1, op), op_key(second, 1, op));
        assert!(op_key(first, 1, op).starts_with(&namespace_prefix(b'P', first)));
        assert_eq!(
            decode_namespace_state(&[5]).unwrap(),
            NamespaceState::Discarded
        );
        assert_eq!(
            decode_namespace_state(&[6]).unwrap(),
            NamespaceState::Sealed
        );
        assert_eq!(
            decode_namespace_state(&[7]).unwrap(),
            NamespaceState::Active
        );
        assert!(decode_namespace_state(&[8]).is_err());
        assert_eq!(NAMESPACE_ROWS[0], b'R');
        assert!(!NAMESPACE_ROWS.contains(&b'S'));
        assert!(NAMESPACE_ROWS.contains(&b'f'));
        assert!(NAMESPACE_ROWS.contains(&b'g'));
        assert!(NAMESPACE_ROWS.contains(&b'h'));
        assert!(NAMESPACE_ROWS.contains(&b'j'));
        assert!(NAMESPACE_ROWS.contains(&b'q'));
        assert!(NAMESPACE_ROWS.contains(&b's'));
        assert!(NAMESPACE_ROWS.contains(&b'v'));
        assert!(NAMESPACE_ROWS.contains(&b'w'));
        assert!(NAMESPACE_ROWS.ends_with(b"TUX"));
    }

    async fn raw(catalog: &ManagedCatalog, key: &[u8]) -> Option<Vec<u8>> {
        catalog
            .db
            .create_trx()
            .unwrap()
            .get(key, true)
            .await
            .unwrap()
            .map(|value| value.to_vec())
    }

    async fn finish(catalog: &ManagedCatalog, namespace: Uuid) -> usize {
        let mut removed = 0;
        for _ in 0..1000 {
            let page = catalog
                .purge_discarded_namespace(namespace, 97)
                .await
                .unwrap();
            assert!(page.removed <= 97);
            removed += page.removed;
            if page.complete {
                return removed;
            }
        }
        panic!("候选清理未收敛");
    }

    pub(crate) async fn exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let serving = Uuid::new_v4();
        catalog.create_namespace(serving).await.unwrap();
        assert!(catalog.discard_import(serving).await.is_err());
        assert!(
            catalog
                .purge_discarded_namespace(serving, 10)
                .await
                .is_err()
        );
        assert!(catalog.purge_discarded_namespace(serving, 0).await.is_err());
        assert!(
            catalog
                .purge_discarded_namespace(serving, 101)
                .await
                .is_err()
        );
        assert!(catalog.discard_import(Uuid::nil()).await.is_err());
        assert!(catalog.discard_import(Uuid::new_v4()).await.is_err());

        let namespace = Uuid::new_v4();
        catalog.begin_import(namespace).await.unwrap();
        let serial = format!("purge-{}", Uuid::new_v4());
        catalog.advance_owner_epoch(&serial, 10).await.unwrap();
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        catalog
            .begin_volume_reconcile(&serial, 10, volume, scan)
            .await
            .unwrap();
        let mut receipts = Vec::new();
        for n in 1..=103 {
            let receipt = test_receipt(CommitReceipt {
                request_epoch: 1,
                namespace,
                parent: Uuid::nil(),
                name: format!("file-{n}").into_bytes(),
                file_id: Uuid::from_u128(n),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                copy_id: Uuid::new_v4(),
                library_serial: serial.clone(),
                pool_id: None,
                owner_epoch: 10,
                volume_id: volume,
                volume_generation: 1,
                volume_scan_id: scan.id,
                ltfs_file_uid: 1,
                size: n as u64,
                content_digest: ContentDigest::Sha256([3; 32]),
                metadata_digest: None,
            });
            assert_eq!(
                test_publish_imported(catalog, &receipt).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            receipts.push(receipt);
        }
        for n in 1..=1001 {
            catalog
                .create_import_directory(
                    namespace,
                    Uuid::nil(),
                    format!("dir-{n}").as_bytes(),
                    Uuid::from_u128(1000 + n),
                )
                .await
                .unwrap();
        }
        super::super::tests::verify_scan(catalog, &serial, 10, volume, scan, &receipts).await;
        // A read-modify-write import/seal/verification transaction started before
        // discard must conflict instead of reviving this candidate.
        let in_flight = catalog.db.create_trx().unwrap();
        in_flight
            .get(&namespace_key(namespace), false)
            .await
            .unwrap();
        in_flight.set(&namespace_key(namespace), &[NamespaceState::Ready as u8]);
        assert!(catalog.discard_import(namespace).await.unwrap());
        assert!(in_flight.commit().await.is_err());
        assert!(!catalog.discard_import(namespace).await.unwrap());
        assert!(catalog.begin_import(namespace).await.is_err());
        assert!(catalog.create_namespace(namespace).await.is_err());
        assert!(catalog.seal_import(namespace).await.is_err());
        assert_eq!(
            test_publish_imported(catalog, &receipts[0]).await.unwrap(),
            PublishOutcome::NamespaceNotBuilding
        );
        assert_eq!(
            test_publish(catalog, &receipts[0]).await.unwrap(),
            PublishOutcome::NamespaceNotReady
        );
        assert!(catalog.node(namespace, Uuid::nil()).await.is_err());
        assert_eq!(
            catalog.verify_namespace_page(namespace, 10).await.unwrap(),
            NamespaceVerificationOutcome::NotVerifying
        );
        let first = &receipts[0];
        let copy = copy_key(namespace, first.file_id, first.copy_id);
        let index = volume_key(volume, scan.id, namespace, first.file_id, first.copy_id);
        assert!(matches!(
            catalog.purge_discarded_namespace(namespace, 10).await,
            Err(CatalogError::Invalid(_))
        ));
        assert!(raw(catalog, &copy).await.is_some());
        assert!(raw(catalog, &index).await.is_some());
        assert!(raw(catalog, &reconcile_volume_key(volume)).await.is_some());
        catalog
            .cancel_volume_reconcile(&serial, 10, volume, scan)
            .await
            .unwrap();

        // A contradictory ready pointer must stop cleanup even if the scan's
        // manifest says Retired. No pointer is repaired implicitly by GC.
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &ready_volume_key(volume),
            &encode_ready_volume(scan, &serial),
        );
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.purge_discarded_namespace(namespace, 10).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(raw(catalog, &copy).await.is_some());
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&ready_volume_key(volume));
        tr.commit().await.unwrap();

        // Invalid metadata late in a batch must not commit earlier deletions.
        let last = &receipts[96];
        let last_copy = copy_key(namespace, last.file_id, last.copy_id);
        let saved = raw(catalog, &last_copy).await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&last_copy, &[0]);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.purge_discarded_namespace(namespace, 97).await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(raw(catalog, &copy).await.is_some());
        assert!(raw(catalog, &index).await.is_some());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&last_copy, &saved);
        tr.commit().await.unwrap();

        // Preserve another namespace even when it uses the same operation UUID.
        let protected_op = op_key(serving, 1, first.operation_id);
        let serving_volume = Uuid::new_v4();
        let serving_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        catalog
            .begin_volume_reconcile(&serial, 10, serving_volume, serving_scan)
            .await
            .unwrap();
        super::super::tests::verify_scan(catalog, &serial, 10, serving_volume, serving_scan, &[])
            .await;
        catalog
            .mark_volume_ready(&serial, 10, serving_volume, serving_scan)
            .await
            .unwrap();
        let protected_receipt = test_receipt(CommitReceipt {
            namespace: serving,
            volume_id: serving_volume,
            volume_scan_id: serving_scan.id,
            copy_id: Uuid::new_v4(),
            ..first.clone()
        });
        assert_eq!(
            test_publish(catalog, &protected_receipt).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        let protected_marker = raw(catalog, &protected_op).await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&namespace_prefix(b'L', namespace), &0u16.to_be_bytes());
        tr.set(&namespace_prefix(b'Z', namespace), b"abandoned progress");
        tr.commit().await.unwrap();
        let quota_rows = read_page(
            &catalog.db.create_trx().unwrap(),
            &namespace_prefix(b'B', namespace),
            None,
            100,
            true,
        )
        .await
        .unwrap()
        .items
        .len();
        let first_page = catalog
            .purge_discarded_namespace(namespace, 97)
            .await
            .unwrap();
        assert_eq!(
            first_page,
            PurgeProgress {
                removed: 97,
                complete: false
            }
        );
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        // 103 copy/head/op/file node/child groups, 1001 directories, root,
        // default node group, and the explicitly inserted proof/progress rows.
        assert_eq!(
            first_page.removed + finish(&reopened, namespace).await,
            103 * 5 + 1001 * 2 + 4 + 4 + quota_rows
        );
        assert_eq!(finish(&reopened, namespace).await, 0);
        assert_eq!(
            catalog
                .freeze_namespace(
                    namespace,
                    NamespaceSummary {
                        nodes: 1,
                        files: 0,
                        digest: [0; 32]
                    }
                )
                .await
                .unwrap(),
            NamespaceVerificationOutcome::NotVerifying
        );
        for kind in NAMESPACE_ROWS {
            let tr = catalog.db.create_trx().unwrap();
            assert!(
                read_page(&tr, &namespace_prefix(*kind, namespace), None, 1, true)
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
        }
        for receipt in &receipts {
            assert!(
                raw(
                    catalog,
                    &volume_key(volume, scan.id, namespace, receipt.file_id, receipt.copy_id)
                )
                .await
                .is_none()
            );
        }
        assert_eq!(raw(catalog, &protected_op).await.unwrap(), protected_marker);
        assert_eq!(
            test_publish(catalog, &protected_receipt).await.unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );
        assert!(
            catalog
                .file_head(serving, protected_receipt.file_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            catalog
                .legacy_copy_record_for_test(
                    serving,
                    protected_receipt.file_id,
                    protected_receipt.copy_id
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(catalog.node(serving, Uuid::nil()).await.unwrap().is_some());
        assert_eq!(
            catalog.namespace_state(namespace).await.unwrap(),
            Some(NamespaceState::Discarded)
        );
        assert!(catalog.begin_import(namespace).await.is_err());
        assert_eq!(
            test_publish_imported(catalog, first).await.unwrap(),
            PublishOutcome::NamespaceNotBuilding
        );
        assert!(raw(catalog, &scan_key(volume, scan.id)).await.is_some());
        assert!(raw(catalog, &reconcile_volume_key(volume)).await.is_some());
        let verification_key = verification::progress_key(volume, scan);
        assert!(raw(catalog, &verification_key).await.is_some());
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, scan, 10)
                .await
                .unwrap(),
            0
        );
        assert!(raw(catalog, &verification_key).await.is_none());
        assert!(raw(catalog, &scan_key(volume, scan.id)).await.is_some());

        // Structural verification failures (Verifying), mismatches (Rejected),
        // and fully verified but unserved imports are all discardable.
        for state in [
            NamespaceState::Verifying,
            NamespaceState::Rejected,
            NamespaceState::Verified,
        ] {
            let ns = Uuid::new_v4();
            catalog.begin_import(ns).await.unwrap();
            super::super::tests::verify_import_namespace(catalog, ns, &[], &[]).await;
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&namespace_key(ns), &[state as u8]);
            tr.commit().await.unwrap();
            assert!(catalog.discard_import(ns).await.unwrap());
            assert_eq!(
                catalog.verify_namespace_page(ns, 10).await.unwrap(),
                NamespaceVerificationOutcome::NotVerifying
            );
            assert!(finish(catalog, ns).await > 0);
        }
        let ns = Uuid::new_v4();
        catalog.begin_import(ns).await.unwrap();
        super::super::tests::verify_import_namespace(catalog, ns, &[], &[]).await;
        catalog.seal_import(ns).await.unwrap();
        assert!(raw(catalog, &namespace_prefix(b'Z', ns)).await.is_some());
        assert_eq!(catalog.purge_namespace_proofs(ns, 1).await.unwrap(), 1);
        assert_eq!(catalog.purge_namespace_proofs(ns, 1).await.unwrap(), 0);
        assert!(raw(catalog, &namespace_prefix(b'Z', ns)).await.is_none());
        assert!(catalog.node(ns, Uuid::nil()).await.unwrap().is_some());
        assert_eq!(
            catalog.verify_namespace_page(ns, 10).await.unwrap(),
            NamespaceVerificationOutcome::NotVerifying
        );
        assert!(catalog.discard_import(ns).await.is_err());
    }
}
