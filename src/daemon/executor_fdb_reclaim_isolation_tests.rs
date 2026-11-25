//! Actual lost-admission driver replay, not a handcrafted FDB state.
use super::*;
use crate::managed_catalog::{ManagementRequest, ReclaimReservation};
use std::collections::BTreeMap;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；显式group-only原admit丢回执，重复/缓存丢失/同owner重建/HA均早隔离，保源C/V/读证明"]
async fn public_group_only_unknown_blocks_before_source_or_target_device_io() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, true, None, false, true, false, true,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；正常public group-only原Full S4重启/接管，限原目标只读且不重写正文"]
async fn public_group_only_known_full_restarts_without_source_replay() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, false, None, true, true, false, false,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；正常public group-only root MetadataTask Writing未知必须早隔离，尚无组"]
async fn public_group_only_root_metadata_unknown_blocks_before_device_io() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, false, None, false, true, false, true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；已stage/完整Full但VCI原块不匹配不能当KnownFull，后续零IO并保源"]
async fn public_group_only_known_full_bad_mam_is_persistently_blocked() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, false, None, true, true, false, true,
    ))
    .await;
}
pub(super) fn inject_wrong_vci(f: &Fixture, facts: &fdb_reclaim::OriginalS4StopFacts) {
    use crate::ltfs::mam::{Mam, VolumeCoherencyInfo};
    let mam = Mam::with_partition(f.owner.devices[facts.drive].dev.as_ref(), 1);
    let vcr = mam.read_vcr().unwrap().unwrap();
    mam.write_vci(&VolumeCoherencyInfo {
        vcr: vcr.to_vec(),
        volume_uuid: facts.current.volume_uuid,
        generation: facts.previous.generation,
        block: facts.previous.location.start_block,
    })
    .unwrap();
}

pub(super) fn protected_rows<'a>(
    cluster: &'a str,
    parent: &'a ReclaimReservation,
    source: uuid::Uuid,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = BTreeMap<Vec<u8>, Vec<u8>>> + 'a>> {
    Box::pin(async move {
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        let mut scoped = b"tape-rs/catalog/22/@/".to_vec();
        scoped.extend_from_slice(parent.namespace().as_bytes());
        scoped.push(b'R');
        let skip = |kind: u8| {
            let mut k = scoped.clone();
            k.push(kind);
            k.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
            k.extend_from_slice(
                uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                    .unwrap()
                    .as_bytes(),
            );
            k
        };
        let excluded = [skip(b'j'), skip(b'D'), skip(0x85)];
        let mut prefixes = vec![scoped];
        for kind in [b'R', b'0', b'I', b'F'] {
            let mut p = b"tape-rs/catalog/22/".to_vec();
            p.extend_from_slice(&[kind, b'/']);
            p.extend_from_slice(parent.namespace().as_bytes());
            prefixes.push(p);
        }
        let mut reverse = b"tape-rs/catalog/22/V/".to_vec();
        reverse.extend_from_slice(source.as_bytes());
        prefixes.push(reverse);
        let mut rows = BTreeMap::new();
        for prefix in prefixes {
            let mut end = prefix.clone();
            while end.last() == Some(&u8::MAX) {
                end.pop();
            }
            *end.last_mut().unwrap() += 1;
            let mut range = foundationdb::RangeOption {
                limit: Some(4096),
                ..foundationdb::RangeOption::from((prefix, end))
            };
            let mut seen = 0usize;
            let mut total_bytes = 0usize;
            let mut iteration = 1;
            loop {
                let page = tr.get_range(&range, iteration, false).await.unwrap();
                for row in page.iter() {
                    seen += 1;
                    total_bytes += row.key().len() + row.value().len();
                    assert!(
                        seen <= 4096 && total_bytes <= 8 << 20,
                        "bounded original proof snapshot exceeded"
                    );
                    if !excluded.iter().any(|k| k.as_slice() == row.key()) {
                        rows.insert(row.key().to_vec(), row.value().to_vec());
                    }
                }
                let more = page.more();
                match range.next_range(&page) {
                    Some(next) if seen < 4096 => {
                        range = next;
                        iteration += 1;
                    }
                    _ => {
                        assert!(!more, "original proof snapshot truncated at row cap");
                        break;
                    }
                }
            }
        }
        rows
    })
}
pub(super) fn assert_early_unknown<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    original: &'a ManagementRequest,
    cluster: &'a str,
    source: uuid::Uuid,
    paths: &'a [String],
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
    heartbeat: &'a mut Option<ParentPermitHeartbeat>,
    known_fault: Option<uuid::Uuid>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    assert_early_unknown_with_copies(
        f,
        control,
        status,
        parent,
        original,
        cluster,
        source,
        paths,
        commands,
        heartbeat,
        known_fault,
        2,
    )
}

pub(super) fn assert_early_unknown_after_closed_directory<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    original: &'a ManagementRequest,
    cluster: &'a str,
    source: uuid::Uuid,
    paths: &'a [String],
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
    heartbeat: &'a mut Option<ParentPermitHeartbeat>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    assert_early_unknown_with_copies(
        f, control, status, parent, original, cluster, source, paths, commands, heartbeat, None, 0,
    )
}

fn assert_early_unknown_with_copies<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    original: &'a ManagementRequest,
    cluster: &'a str,
    source: uuid::Uuid,
    paths: &'a [String],
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
    heartbeat: &'a mut Option<ParentPermitHeartbeat>,
    known_fault: Option<uuid::Uuid>,
    expected_source_copies: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let before = Box::pin(protected_rows(cluster, parent, source)).await;
        let query_start = commands.lock().unwrap().len();
        let group_id = if let Some(group) = known_fault {
            // This case has no diagnostic-write failure injection. Require the
            // real durable BlockedUnknown before testing any further admission.
            let db = foundationdb::Database::from_path(cluster).unwrap();
            let tr = db.create_trx().unwrap();
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(parent.namespace().as_bytes());
            key.extend_from_slice(&[b'R', 0x85]);
            key.extend_from_slice(&original.epoch().to_be_bytes());
            key.extend_from_slice(original.operation().as_bytes());
            let raw = tr
                .get(&key, false)
                .await
                .unwrap()
                .expect("original isolation row");
            assert!(raw.len() > 32);
            let payload = &raw[..raw.len() - 32];
            let row: Value = serde_json::from_slice(payload).unwrap();
            use sha2::Digest;
            assert_eq!(
                sha2::Sha256::new()
                    .chain_update(b"tape-rs/reclaim-isolation/1")
                    .chain_update(payload)
                    .finalize()[..],
                raw[raw.len() - 32..]
            );
            assert_eq!(
                row["parent"],
                format!(
                    "{:x}",
                    sha2::Sha256::digest(parent.intent().to_string().as_bytes())
                )
            );
            assert_eq!(
                row["disposition"]["state"], "blocked_unknown",
                "MAM failure must persist BlockedUnknown, not silently leave ObservationStarted"
            );
            assert_eq!(row["disposition"]["owner"], f.owner.round);
            assert_eq!(row["subject"]["kind"], "group");
            for field in ["group", "attempt", "batch"] {
                assert_eq!(row["subject"][field], group.to_string());
            }
            assert_eq!(row["subject"]["issuer"], 7);
            assert_eq!(row["subject"]["identity"].as_array().unwrap().len(), 32);
            drop(tr);
            let fact = control
                .catalog
                .resume_reclaim_copy_group(parent, f.owner.round)
                .await
                .unwrap()
                .expect("original group facts remain readable");
            assert_eq!(fact.id(), group);
            assert!(
                fact.requires_observation(),
                "facts must not become fresh execution"
            );
            assert_eq!(
                Box::pin(protected_rows(cluster, parent, source)).await,
                before,
                "same-owner fact query must not mutate original records"
            );
            assert_eq!(
                commands.lock().unwrap().len(),
                query_start,
                "same-owner fact query must issue no device command"
            );
            let error = control
                .catalog
                .adopt_reclaim_original_attempt(parent, f.owner.round, group)
                .await
                .err()
                .expect("blocked original must not mint another observation scope");
            assert!(
                error.to_string().contains("BlockedUnknown"),
                "must reject on isolation: {error}"
            );
            eprintln!(
                "GROUP_ONLY_BLOCKED_FACT_READ namespace={} group={} state=blocked_unknown task_cdbs=0 original_rows_unchanged=true adopt_rejected=true",
                parent.namespace(),
                group
            );
            group
        } else if let Some(group) = control
            .catalog
            .resume_reclaim_copy_group(parent, f.owner.round)
            .await
            .unwrap()
        {
            let original_scope = control
                .catalog
                .adopt_reclaim_original_attempt(parent, f.owner.round, group.id())
                .await
                .unwrap();
            assert!(original_scope.actual().is_none());
            group.id()
        } else {
            let task = control
                .catalog
                .reclaim_metadata_checkpoint(parent, f.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert!(task.writing());
            eprintln!(
                "GROUP_ONLY_ROOT_METADATA_UNKNOWN namespace={} task={} writing=true no_group=true",
                parent.namespace(),
                task.id()
            );
            uuid::Uuid::nil()
        };
        if !group_id.is_nil() {
            assert!(
                before
                    .iter()
                    .any(|(k, v)| k.starts_with(b"tape-rs/catalog/22/F/") && v.is_empty()),
                "original request history pins must really exist"
            );
        }
        let copies = control
            .catalog
            .copies_on_volume(source, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(copies.len(), expected_source_copies);
        // Anonymous download files are cache, not durable payload. Drop all
        // reusable captures and remove only this fixture's owned extent cache.
        f.owner.fdb_reclaim_source_session = None;
        f.owner.fdb_read_session = None;
        f.owner.fdb_reads.clear();
        f.owner.write_session = None;
        let cache = f.owner.files.extent_cache_dir();
        if cache.exists() {
            std::fs::remove_dir_all(&cache).unwrap();
        }
        assert!(!cache.exists());
        for phase in 0..3 {
            let start = commands.lock().unwrap().len();
            assert!(
                fdb_reclaim::recovery_barrier(&f.owner, control)
                    .await
                    .unwrap(),
                "Recover must discover every protected parent before generic recovery"
            );
            assert_eq!(
                commands.lock().unwrap().len(),
                start,
                "recovery barrier must be FDB-only"
            );
            let result = Box::pin(fdb_reclaim::advance(
                &mut f.owner,
                &f.permit,
                control,
                status,
            ))
            .await;
            let added = commands.lock().unwrap().len() - start;
            let after = Box::pin(protected_rows(cluster, parent, source)).await;
            eprintln!(
                "GROUP_ONLY_EARLY_UNKNOWN namespace={} phase={} owner={} rejected={} task_cdbs={} protected_rows={} unchanged={}",
                parent.namespace(),
                phase,
                f.owner.round,
                result.is_err(),
                added,
                before.len(),
                before == after
            );
            assert!(
                result.is_err(),
                "unresolved original attempt is waiting recovery"
            );
            assert_eq!(
                added, 0,
                "Unknown must stop before history reaper, layout, load, source capture or probe"
            );
            assert_eq!(after, before);
            assert!(
                control
                    .catalog
                    .reap_reclaim_parent_history_pins(parent, f.owner.round)
                    .await
                    .is_err()
            );
            assert!(
                control
                    .catalog
                    .cancel_management(
                        &control.serial,
                        control.route,
                        f.owner.round,
                        original.epoch(),
                        original.operation()
                    )
                    .await
                    .is_err()
            );
            assert!(
                control
                    .catalog
                    .step_reclaim_cleanup(parent, f.owner.round)
                    .await
                    .is_err()
            );
            assert!(
                control
                    .catalog
                    .step_reclaim_obsolete_copies(parent, f.owner.round)
                    .await
                    .is_err()
            );
            assert!(
                control
                    .catalog
                    .release_reserved_reclaim(parent, f.owner.round)
                    .await
                    .is_err()
            );
            assert_eq!(
                Box::pin(protected_rows(cluster, parent, source)).await,
                before,
                "rejected cancel/cleanup/reaper/retirement must not mutate original facts"
            );
            let db = foundationdb::Database::from_path(cluster).unwrap();
            let tr = db.create_trx().unwrap();
            let mut key = b"tape-rs/catalog/22/@/".to_vec();
            key.extend_from_slice(parent.namespace().as_bytes());
            key.extend_from_slice(&[b'R', 0x85]);
            key.extend_from_slice(&original.epoch().to_be_bytes());
            key.extend_from_slice(original.operation().as_bytes());
            let raw = tr.get(&key, false).await.unwrap().unwrap();
            let row: Value = serde_json::from_slice(&raw[..raw.len() - 32]).unwrap();
            assert_eq!(row["disposition"]["state"], "blocked_unknown");
            assert_eq!(row["parent"].as_str().unwrap().len(), 64);
            assert_eq!(
                row["subject"]["kind"],
                if group_id.is_nil() {
                    "metadata"
                } else {
                    "group"
                }
            );
            eprintln!(
                "GROUP_ONLY_ISOLATION_PERSISTED namespace={} phase={} state=blocked_unknown subtype={} mutators_rejected=5 original_rows_unchanged=true",
                parent.namespace(),
                phase,
                row["subject"]["kind"]
            );

            assert_eq!(
                control
                    .catalog
                    .copies_on_volume(source, None, 100)
                    .await
                    .unwrap()
                    .items,
                copies
            );
            assert!(
                control
                    .catalog
                    .reclaim_format_plan(parent, f.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            if phase == 0 {
                drop(heartbeat.take());
                let round = f.owner.round;
                f.permit.close(round);
                f.permit = Arc::new(Permission::default());
                assert!(f.permit.renew(
                    round,
                    Phase::Fencing,
                    Instant::now() + Duration::from_secs(60)
                ));
                let next = takeover(
                    &Provider {
                        lib: f.lib.clone(),
                        commands: f.commands.clone(),
                    },
                    &f.opts,
                    round,
                    Some(&f.permit),
                    f.owner.files.clone(),
                )
                .unwrap();
                f.owner = next;
                assert!(f.permit.renew(
                    round,
                    Phase::Serving,
                    Instant::now() + Duration::from_secs(60)
                ));
                f.owner.serving = true;
                for drive in 0..f.lib.drive_count() {
                    let inner = std::mem::replace(
                        &mut f.owner.devices[drive].dev,
                        Box::new(f.lib.drive_as(drive, 1)),
                    );
                    f.owner.devices[drive].dev = Box::new(ParentSourceTrace {
                        inner,
                        drive,
                        commands: commands.clone(),
                        no_epoch: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        trusted_change_evidence: false,
                    });
                }
                *heartbeat = Some(ParentPermitHeartbeat::new(f));
            } else if phase == 1 && (known_fault.is_some() || expected_source_copies == 0) {
                drop(heartbeat.take());
                take_over_blocked(&mut *f, control, parent, original, status, commands).await;
                *heartbeat = Some(ParentPermitHeartbeat::new(f));
            } else if phase == 1 {
                drop(heartbeat.take());
                Box::pin(take_over_original_group(
                    f, control, status, parent, original, group_id, commands,
                ))
                .await;
                *heartbeat = Some(ParentPermitHeartbeat::new(f));
            }
        }
        // Separate explicit test inspector reads: prove the retained source is
        // real durable payload, not just a surviving local filename/FD.
        let layout = control
            .catalog
            .reclaim_layout(parent, f.owner.round)
            .await
            .unwrap()
            .unwrap();
        let drive = f
            .owner
            .devices
            .iter()
            .position(|d| d.serial == layout.source.drive_serial)
            .unwrap();
        let mounted = LtfsVolume::mount(f.owner.devices[drive].dev.as_ref()).unwrap();
        assert_eq!(mounted.index().volume_uuid, source);
        for (i, path) in paths.iter().enumerate() {
            let mut bytes = Vec::new();
            mounted.read_file_to_writer(path, &mut bytes).unwrap();
            assert_eq!(bytes, vec![i as u8 + 37; 1 << 20]);
        }
        eprintln!(
            "GROUP_ONLY_DURABLE_SOURCE namespace={} verified_original_bytes=2097152 local_extent_cache_missing=true original_ids_retained=true",
            parent.namespace()
        );
    })
}

fn take_over_blocked<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    original: &'a ManagementRequest,
    status: &'a SharedStatus,
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let next = f.owner.round + 1;
        control
            .catalog
            .claim_active_owner_epoch(&control.serial, next)
            .await
            .unwrap();
        let start = commands.lock().unwrap().len();
        assert!(
            Box::pin(fdb_reclaim::advance(
                &mut f.owner,
                &f.permit,
                control,
                status
            ))
            .await
            .is_err()
        );
        assert_eq!(
            commands.lock().unwrap().len(),
            start,
            "old blocked actor must stop before CDB"
        );
        let restored = control
            .catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                next,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.intent(), parent.intent());
        f.permit.close(f.owner.round);
        f.permit = Arc::new(Permission::default());
        assert!(f.permit.renew(
            next,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(60)
        ));
        f.owner = takeover(
            &Provider {
                lib: f.lib.clone(),
                commands: f.commands.clone(),
            },
            &f.opts,
            next,
            Some(&f.permit),
            f.owner.files.clone(),
        )
        .unwrap();
        assert!(f.permit.renew(
            next,
            Phase::Serving,
            Instant::now() + Duration::from_secs(60)
        ));
        f.owner.serving = true;
        for drive in 0..f.lib.drive_count() {
            let inner = std::mem::replace(
                &mut f.owner.devices[drive].dev,
                Box::new(f.lib.drive_as(drive, 1)),
            );
            f.owner.devices[drive].dev = Box::new(ParentSourceTrace {
                inner,
                drive,
                commands: commands.clone(),
                no_epoch: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                trusted_change_evidence: false,
            });
        }
    })
}
