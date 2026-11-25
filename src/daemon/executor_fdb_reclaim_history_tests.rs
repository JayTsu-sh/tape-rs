// Included inside lifecycle::tests to reuse its real parent/owner fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；原父源历史提示发布/冷载/损坏回退/失权拒绝"]
async fn native_reclaim_source_history_hints_roundtrip_and_fail_closed() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    Box::pin(native_reclaim_parent_case(
        catalog,
        &cluster,
        NativeParentCase::SourceHistoryHints,
    ))
    .await;
}

async fn verify_source_history_hints(
    fixture: &Fixture,
    control: &FdbOwnerControl,
    parent: &crate::managed_catalog::ReclaimReservation,
    barcode: &str,
    trace: &Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
    cluster: &str,
) {
    use sha2::{Digest, Sha256};
    let _heartbeat = ParentPermitHeartbeat::new(fixture);
    let current = inventory(&fixture.owner.devices).unwrap();
    let source = current
        .drives
        .iter()
        .find(|drive| drive.loaded.as_ref().is_some_and(|(b, _)| b == barcode))
        .unwrap()
        .dev;
    assert!(
        fixture.owner.fdb_reclaim_source_session.is_none(),
        "transport without trusted token must not retain A cache"
    );
    let reader = super::super::super::fdb_read::ActiveMediaReader {
        catalog: &control.catalog,
        device: fixture.owner.devices[source].dev.as_ref(),
        serial: &control.serial,
        route: control.route,
        key: fixture.owner.key,
        permission: &fixture.permit,
        storage: control.storage,
    };
    let parent_before = control
        .catalog
        .reserved_reclaim_status(parent, fixture.owner.round)
        .await
        .unwrap();
    let mut scratch = crate::ltfs::recovery::IndexScratchBudget::default();
    scratch.directory = fixture.owner.files.spool_dir().into();
    scratch.max_bytes = control.storage.index_bytes;
    scratch.headroom_bytes = control.storage.headroom_bytes;
    let hint = control
        .catalog
        .load_reclaim_full_history_hint(parent, fixture.owner.round, &scratch)
        .await
        .unwrap()
        .expect("first guarded complete capture published immutable hints");
    let manifest = hint.manifest();
    drop(hint);
    let before = trace.lock().unwrap().len();
    let captured = reader
        .capture_reclaim_source(parent, fixture.owner.files.spool_dir())
        .await
        .unwrap();
    assert!(
        captured.history_hint_used(),
        "fresh no-token source must actually execute ASC cold hint reads"
    );
    assert_eq!(captured.full_history_hint().unwrap().manifest(), manifest);
    drop(captured);
    assert!(
        trace.lock().unwrap()[before..]
            .iter()
            .any(|(drive, cdb)| *drive == source && cdb[0] == 0x08)
    );
    assert!(
        !trace.lock().unwrap()[before..]
            .iter()
            .any(|(_, cdb)| matches!(cdb[0], 0x0a | 0x10 | 0x8d | 0x04 | 0x19))
    );

    let db = foundationdb::Database::from_path(cluster).unwrap();
    let mut prefix = b"tape-rs/catalog/22/\x81/".to_vec();
    prefix.extend_from_slice(control.route.route.namespace.as_bytes());
    let mut object = prefix.clone();
    object.push(b'o');
    object.extend_from_slice(&manifest.fingerprint());
    let mut chunk = object.clone();
    chunk.push(b'c');
    chunk.extend_from_slice(&0u64.to_be_bytes());
    let mut complete = object;
    complete.push(b'm');
    let mut pointer = prefix;
    pointer.push(b'h');
    let encoded: Vec<u8> = serde_json::from_value(parent.intent()["index"].clone()).unwrap();
    pointer.extend_from_slice(&Sha256::digest(encoded));
    for (case, key) in [
        ("missing-complete", complete),
        ("wrong-head", pointer),
        ("corrupt-chunk", chunk),
    ] {
        let tr = db.create_trx().unwrap();
        let original = tr
            .get(&key, false)
            .await
            .unwrap()
            .expect("published key")
            .to_vec();
        drop(tr);
        let mut changed = original.clone();
        if case == "wrong-head" {
            changed[26] ^= 1;
        }
        if case == "corrupt-chunk" {
            changed[16] ^= 1;
        }
        let tr = db.create_trx().unwrap();
        if case == "missing-complete" {
            tr.clear(&key);
        } else {
            tr.set(&key, &changed);
        }
        tr.commit().await.unwrap();
        assert!(
            control
                .catalog
                .load_reclaim_full_history_hint(parent, fixture.owner.round, &scratch)
                .await
                .unwrap()
                .is_none(),
            "{case}: invalid hint must not be admitted"
        );
        let actual = reader
            .capture_reclaim_source(parent, fixture.owner.files.spool_dir())
            .await
            .unwrap();
        assert!(
            !actual.history_hint_used(),
            "{case}: corrupt hint cannot stand in for real discovery"
        );
        assert_eq!(actual.construct(), manifest.head);
        drop(actual);
        let tr = db.create_trx().unwrap();
        tr.set(&key, &original);
        tr.commit().await.unwrap();
        assert_eq!(
            control
                .catalog
                .reserved_reclaim_status(parent, fixture.owner.round)
                .await
                .unwrap(),
            parent_before
        );
        eprintln!(
            "source history hint {case}: actual complete fallback, original parent unchanged"
        );
    }
    let before = trace.lock().unwrap().len();
    let boundary = Arc::new(std::sync::Mutex::new(None));
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook = control.catalog.install_reclaim_history_hint_test_hook(
        control.route.route.namespace,
        Arc::new({
            let catalog = control.catalog.clone();
            let serial = control.serial.clone();
            let trace = trace.clone();
            let boundary = boundary.clone();
            let fired = fired.clone();
            let next_owner = fixture.owner.round + 1;
            move |point, _, _| {
                let catalog = catalog.clone();
                let serial = serial.clone();
                let trace = trace.clone();
                let boundary = boundary.clone();
                let fired = fired.clone();
                Box::pin(async move {
                    if point == "load-finished" {
                        assert_eq!(fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                        *boundary.lock().unwrap() = Some(trace.lock().unwrap().len());
                        catalog
                            .claim_active_owner_epoch(&serial, next_owner)
                            .await?;
                    }
                    Ok(())
                })
            }
        }),
    );
    assert!(
        reader
            .capture_reclaim_source(parent, fixture.owner.files.spool_dir())
            .await
            .is_err()
    );
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    let after_hint = boundary
        .lock()
        .unwrap()
        .expect("actual final hint read boundary");
    assert!(after_hint >= before);
    assert_eq!(
        trace.lock().unwrap().len(),
        after_hint,
        "the real per-CDB guard must reject before the first post-hint source/target/FORMAT command"
    );
    drop(hook);
    assert!(
        control
            .catalog
            .load_reclaim_full_history_hint(parent, fixture.owner.round, &scratch)
            .await
            .is_err()
    );
    assert!(
        reader
            .capture_reclaim_source(parent, fixture.owner.files.spool_dir())
            .await
            .is_err()
    );
    assert_eq!(
        trace.lock().unwrap().len(),
        after_hint,
        "subsequent calls stay fenced before device IO"
    );
    eprintln!(
        "source history hint owner loss after final load: {} pre-boundary label/probe commands were already allowed; zero commands after transfer, no target/FORMAT effects",
        after_hint - before
    );
}

struct HistoryTransactionFixture {
    fixture: Fixture,
    control: FdbOwnerControl,
    parent: crate::managed_catalog::ReclaimReservation,
    captured: crate::ltfs::recovery::CapturedIndexSource,
}

fn history_transaction_fixture(
    catalog: Arc<crate::managed_catalog::ManagedCatalog>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = HistoryTransactionFixture>>> {
    Box::pin(async move {
        use crate::managed_catalog::ManagementRequest;
        let (mut fixture, control, pool, status) = Box::pin(native_fixture_from(
            catalog.clone(),
            None,
            Fixture::with_drives(1),
        ))
        .await;
        let heartbeat = ParentPermitHeartbeat::new(&fixture);
        fixture.format(uuid::Uuid::new_v4());
        {
            let mut volume = LtfsVolume::mount(fixture.device()).unwrap();
            volume
                .append_bytes("/history-body", b"original history transaction body")
                .unwrap();
            for n in 0..1600 {
                volume
                    .set_node_xattr(
                        "/",
                        crate::ltfs::index::Xattr {
                            key: "history-transaction".into(),
                            value: n.to_string(),
                            base64: false,
                        },
                        n == 0,
                        n > 0,
                    )
                    .unwrap();
                volume.commit().unwrap();
            }
        }
        let mut budget = crate::ltfs::recovery::RecoveryBudget::default();
        budget.index_scratch.directory = fixture.root.clone();
        budget.index_scratch.max_bytes = 64 << 20;
        budget.index_scratch.headroom_bytes = 0;
        let captured = crate::ltfs::recovery::capture_complete_index_source(
            &TapeDrive::new(fixture.device()),
            &crate::ltfs::volume::read_label_only(fixture.device()).unwrap(),
            &budget,
        )
        .unwrap();
        assert!(captured.full_history_hint().unwrap().manifest().entries > 4 * 384);
        let request = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        assert_eq!(
            execute(&mut fixture, &control, &status, &import).await["status"],
            "succeeded"
        );
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let original = request(
            json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,"volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false}),
        );
        catalog
            .admit_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                &original,
            )
            .await
            .unwrap();
        let parent = catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                fixture.owner.round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            catalog
                .reclaim_source_head(&parent, fixture.owner.round)
                .await
                .unwrap(),
            crate::managed_catalog::ActiveIndexIdentity::from(captured.construct()),
            "the actual complete capture must be this original FDB source head"
        );
        drop(heartbeat);
        HistoryTransactionFixture {
            fixture,
            control,
            parent,
            captured,
        }
    })
}

fn history_test_prefix(namespace: uuid::Uuid) -> Vec<u8> {
    let mut prefix = b"tape-rs/catalog/22/\x81/".to_vec();
    prefix.extend_from_slice(namespace.as_bytes());
    prefix
}

async fn history_test_rows(
    db: &foundationdb::Database,
    namespace: uuid::Uuid,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = history_test_prefix(namespace);
    let mut end = prefix.clone();
    for index in (0..end.len()).rev() {
        if end[index] != 0xff {
            end[index] += 1;
            end.truncate(index + 1);
            break;
        }
    }
    let tr = db.create_trx().unwrap();
    let mut range = foundationdb::RangeOption {
        begin: foundationdb::KeySelector::first_greater_or_equal(prefix),
        end: foundationdb::KeySelector::first_greater_or_equal(end),
        limit: Some(64),
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut iteration = 1;
    loop {
        let values = tr.get_range(&range, iteration, false).await.unwrap();
        rows.extend(
            values
                .iter()
                .map(|kv| (kv.key().to_vec(), kv.value().to_vec())),
        );
        match range.next_range(&values) {
            Some(next) if rows.len() < 64 => {
                range = next;
                iteration += 1;
            }
            _ => return rows,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；>1536真实Full的分片跨事务失权与原parent续接"]
async fn native_reclaim_history_upload_fences_between_transactions_and_resumes_original() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    let context = history_transaction_fixture(catalog.clone()).await;
    let namespace = context.control.route.route.namespace;
    let owner = context.fixture.owner.round;
    let next_owner = owner + 1;
    let parent_before = catalog
        .reserved_reclaim_status(&context.parent, owner)
        .await
        .unwrap();
    let head_before = catalog
        .reclaim_source_head(&context.parent, owner)
        .await
        .unwrap();
    let commands_before = context.fixture.commands.lock().unwrap().clone();
    let fired = Arc::new(AtomicUsize::new(0));
    let hook = catalog.install_reclaim_history_hint_test_hook(
        namespace,
        Arc::new({
            let catalog = catalog.clone();
            let serial = context.control.serial.clone();
            let fired = fired.clone();
            move |point, first, end| {
                let catalog = catalog.clone();
                let serial = serial.clone();
                let fired = fired.clone();
                Box::pin(async move {
                    if point == "chunks-committed" {
                        assert_eq!((first, end), (0, 4));
                        assert_eq!(fired.fetch_add(1, Ordering::SeqCst), 0);
                        catalog
                            .claim_active_owner_epoch(&serial, next_owner)
                            .await?;
                    }
                    Ok(())
                })
            }
        }),
    );
    let result = catalog
        .publish_reclaim_full_history_hint(&context.parent, owner, &context.captured)
        .await;
    assert!(matches!(
        result.unwrap_err().domain(),
        crate::managed_catalog::CatalogError::OwnerFenced
    ));
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    drop(hook);
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let partial = history_test_rows(&db, namespace).await;
    assert_eq!(
        partial.len(),
        4,
        "old owner must stop before a fifth chunk, complete or pointer"
    );
    assert!(partial.iter().all(|(key, _)| key[key.len() - 9] == b'c'));
    let claimed = catalog
        .claim_reclaim_reservations_page(
            &context.control.serial,
            context.control.route,
            next_owner,
            None,
            1,
        )
        .await
        .unwrap();
    let resumed = claimed
        .items
        .first()
        .expect("original parent still recoverable");
    assert_eq!(resumed.intent(), context.parent.intent());
    assert_eq!(
        catalog
            .reclaim_source_head(resumed, next_owner)
            .await
            .unwrap(),
        head_before
    );
    assert!(
        catalog
            .publish_reclaim_full_history_hint(resumed, next_owner, &context.captured)
            .await
            .unwrap()
    );
    let final_rows = history_test_rows(&db, namespace).await;
    assert_eq!(
        final_rows.len(),
        context
            .captured
            .full_history_hint()
            .unwrap()
            .manifest()
            .entries
            .div_ceil(384) as usize
            + 2
    );
    for (key, value) in partial {
        assert!(
            final_rows.contains(&(key, value)),
            "original immutable chunks must be retained exactly"
        );
    }
    let mut after = catalog
        .reserved_reclaim_status(resumed, next_owner)
        .await
        .unwrap();
    after["owner_epoch"] = parent_before["owner_epoch"].clone();
    assert_eq!(after, parent_before);
    assert_eq!(
        *context.fixture.commands.lock().unwrap(),
        commands_before,
        "metadata continuation must not issue new device commands"
    );
    eprintln!(
        "history upload: four real chunks committed; owner transferred; old owner fenced; same parent/head resumed without device IO"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；实际chunk/complete commit后丢回执及原immutable键重查"]
async fn native_reclaim_history_unknown_receipts_recheck_committed_original_keys() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    let context = history_transaction_fixture(catalog.clone()).await;
    let namespace = context.control.route.route.namespace;
    let owner = context.fixture.owner.round;
    let before = catalog
        .reserved_reclaim_status(&context.parent, owner)
        .await
        .unwrap();
    let head = catalog
        .reclaim_source_head(&context.parent, owner)
        .await
        .unwrap();
    let commands_before = context.fixture.commands.lock().unwrap().clone();
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    for point in ["chunks-committed", "complete-committed"] {
        // Fault setup removes only this fixture's optional hint keys. Original
        // parent, source facts and operation identity remain untouched.
        let old = history_test_rows(&db, namespace).await;
        let tr = db.create_trx().unwrap();
        for (key, _) in old {
            tr.clear(&key);
        }
        tr.commit().await.unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = catalog.install_reclaim_history_hint_test_hook(
            namespace,
            Arc::new({
                let fired = fired.clone();
                move |event, _, _| {
                    let fired = fired.clone();
                    Box::pin(async move {
                        if event == point && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                            // The hook is after successful commit().await, never before
                            // mutation. Hide its receipt as commit_unknown_result.
                            return Err(crate::managed_catalog::CatalogError::Client(
                                foundationdb::FdbError::from_code(1021),
                            ));
                        }
                        Ok(())
                    })
                }
            }),
        );
        let error = catalog
            .publish_reclaim_full_history_hint(&context.parent, owner, &context.captured)
            .await
            .unwrap_err();
        assert!(
            matches!(error.domain(),crate::managed_catalog::CatalogError::Client(e) if e.code()==1021)
        );
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        drop(hook);
        let committed = history_test_rows(&db, namespace).await;
        let total = context
            .captured
            .full_history_hint()
            .unwrap()
            .manifest()
            .entries
            .div_ceil(384) as usize
            + 2;
        assert_eq!(
            committed.len(),
            if point == "chunks-committed" {
                4
            } else {
                total
            },
            "the hidden acknowledgement must follow actual durable writes"
        );
        assert!(
            catalog
                .publish_reclaim_full_history_hint(&context.parent, owner, &context.captured)
                .await
                .unwrap()
        );
        let retried = history_test_rows(&db, namespace).await;
        assert_eq!(retried.len(), total);
        for row in committed {
            assert!(
                retried.contains(&row),
                "retry must check the same committed immutable keys"
            );
        }
        assert_eq!(
            catalog
                .reserved_reclaim_status(&context.parent, owner)
                .await
                .unwrap(),
            before
        );
        assert_eq!(
            catalog
                .reclaim_source_head(&context.parent, owner)
                .await
                .unwrap(),
            head
        );
        eprintln!(
            "history {point}: actual commit durable, acknowledgement deliberately hidden, original fixed keys rechecked, count={total}"
        );
    }
    assert_eq!(*context.fixture.commands.lock().unwrap(), commands_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；真实多批历史装载期间assignment/owner改变后拒绝"]
async fn native_reclaim_history_load_rechecks_assignment_and_owner_between_batches() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    let context = history_transaction_fixture(catalog.clone()).await;
    let namespace = context.control.route.route.namespace;
    let owner = context.fixture.owner.round;
    let before = catalog
        .reserved_reclaim_status(&context.parent, owner)
        .await
        .unwrap();
    let head = catalog
        .reclaim_source_head(&context.parent, owner)
        .await
        .unwrap();
    let commands_before = context.fixture.commands.lock().unwrap().clone();
    assert!(
        catalog
            .publish_reclaim_full_history_hint(&context.parent, owner, &context.captured)
            .await
            .unwrap()
    );
    let db = foundationdb::Database::from_path(&cluster).unwrap();
    let hints_before = history_test_rows(&db, namespace).await;
    let mut assignment_key = b"tape-rs/catalog/22/d/".to_vec();
    for part in [context.control.serial.as_bytes(), b"LIFE00L8"] {
        for byte in part {
            if *byte == 0 {
                assignment_key.extend_from_slice(&[0, 0xff]);
            } else {
                assignment_key.push(*byte);
            }
        }
        assignment_key.extend_from_slice(&[0, 0]);
    }
    let tr = db.create_trx().unwrap();
    let assignment = tr
        .get(&assignment_key, false)
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    drop(tr);
    assert_eq!(assignment.len(), 60);
    let mut advanced_assignment = assignment.clone();
    let revision = u64::from_be_bytes(assignment[34..42].try_into().unwrap());
    advanced_assignment[34..42].copy_from_slice(&(revision + 1).to_be_bytes());
    let mut scratch = crate::ltfs::recovery::IndexScratchBudget::default();
    scratch.directory = context.fixture.root.clone();
    scratch.max_bytes = 64 << 20;
    scratch.headroom_bytes = 0;
    for mode in ["assignment-revision", "owner"] {
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = catalog.install_reclaim_history_hint_test_hook(
            namespace,
            Arc::new({
                let catalog = catalog.clone();
                let serial = context.control.serial.clone();
                let key = assignment_key.clone();
                let value = advanced_assignment.clone();
                let cluster = cluster.clone();
                let fired = fired.clone();
                move |point, first, end| {
                    let catalog = catalog.clone();
                    let serial = serial.clone();
                    let key = key.clone();
                    let value = value.clone();
                    let cluster = cluster.clone();
                    let fired = fired.clone();
                    Box::pin(async move {
                        if point == "load-batch-read" {
                            assert_eq!((first, end), (0, 4));
                            assert_eq!(fired.fetch_add(1, Ordering::SeqCst), 0);
                            if mode == "owner" {
                                catalog.claim_active_owner_epoch(&serial, owner + 1).await?;
                            } else {
                                // Fault injection advances the real assignment CAS
                                // revision. No source intent/head is rewritten.
                                let db = foundationdb::Database::from_path(&cluster).unwrap();
                                let tr = db.create_trx().unwrap();
                                tr.set(&key, &value);
                                tr.commit().await.unwrap();
                            }
                        }
                        Ok(())
                    })
                }
            }),
        );
        assert!(
            catalog
                .load_reclaim_full_history_hint(&context.parent, owner, &scratch)
                .await
                .is_err()
        );
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "the second batch must not finish after authority changes"
        );
        drop(hook);
        assert_eq!(history_test_rows(&db, namespace).await, hints_before);
        assert_eq!(
            *context.fixture.commands.lock().unwrap(),
            commands_before,
            "loading candidates never grants device access"
        );
        if mode == "assignment-revision" {
            let tr = db.create_trx().unwrap();
            tr.set(&assignment_key, &assignment);
            tr.commit().await.unwrap();
            assert_eq!(
                catalog
                    .reclaim_source_head(&context.parent, owner)
                    .await
                    .unwrap(),
                head
            );
            assert_eq!(
                catalog
                    .reserved_reclaim_status(&context.parent, owner)
                    .await
                    .unwrap(),
                before
            );
        }
        eprintln!(
            "history multi-batch load {mode}: first four chunks actually read, changed authority rejected before next batch, zero device IO"
        );
    }
}
