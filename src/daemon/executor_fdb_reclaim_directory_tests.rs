//! A real ordinary mkdir on the source precedes the public group-only request.
use super::*;
use crate::managed_catalog::{
    ActiveUploadSpec, ContentDigest, ManagementRequest, NodeMutation, NodeMutationSpec,
    NodeMutationTaskState, ReclaimReservation,
};
use sha2::Digest;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；真实source mkdir/Ready目录/原Full接管，Closed后的目录后继必须可完成"]
async fn public_group_only_known_full_covers_real_empty_directory_successor() {
    directory_case(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；Closed原组后真实目录Writing回执丢失，分类前直接mutator/重启/HA均须保源零IO"]
async fn public_group_only_directory_successor_unknown_blocks_mutation_and_replay() {
    directory_case(true, false).await;
}

fn directory_case(
    unknown: bool,
    grouped_root: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
    Box::pin(async move {
        let mut input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(std::env::var("FDB_TEST_CLUSTER").unwrap())
            .unwrap();
        let mut bytes = Vec::new();
        Read::take(&mut input, 4096)
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(
            bytes,
            b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\n"
        );
        let name = std::ffi::CString::new("directory-successor-private-fdb").unwrap();
        let fd = nix::sys::memfd::memfd_create(
            name.as_c_str(),
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC
                | nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
        )
        .unwrap();
        let mut sealed = std::fs::File::from(fd);
        sealed.write_all(&bytes).unwrap();
        nix::fcntl::fcntl(
            sealed.as_raw_fd(),
            nix::fcntl::FcntlArg::F_ADD_SEALS(
                nix::fcntl::SealFlag::F_SEAL_WRITE
                    | nix::fcntl::SealFlag::F_SEAL_GROW
                    | nix::fcntl::SealFlag::F_SEAL_SHRINK
                    | nix::fcntl::SealFlag::F_SEAL_SEAL,
            ),
        )
        .unwrap();
        let cluster = format!("/proc/self/fd/{}", sealed.as_raw_fd());
        let _network = unsafe { foundationdb::boot() };
        let catalog =
            Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
        let (mut f, control, pool, status, source, target, paths, dir_uid, original_directory) =
            setup(catalog.clone()).await;
        let mut heartbeat = Some(ParentPermitHeartbeat::new(&f));
        let assignment = catalog
            .tape_assignment(&control.serial, "LIFE00L8")
            .await
            .unwrap()
            .unwrap();
        let original = management(
            &control,
            json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":source.to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false,"mode":"group-only"}),
        );
        catalog
            .admit_management(&control.serial, control.route, f.owner.round, &original)
            .await
            .unwrap();
        let parent = if grouped_root {
            catalog
                .start_reclaim_management(
                    &control.serial,
                    control.route,
                    f.owner.round,
                    original.epoch(),
                    original.operation(),
                )
                .await
        } else {
            catalog
                .start_reclaim_copy_group_management(
                    &control.serial,
                    control.route,
                    f.owner.round,
                    original.epoch(),
                    original.operation(),
                )
                .await
        }
        .unwrap()
        .unwrap();
        assert_eq!(parent.groups_archive_root(), grouped_root);
        let pre_group_delta = u64::from(!grouped_root);
        let final_delta = pre_group_delta + 2;
        let stop = fdb_reclaim::arm_original_checkpoint_s4_stop_for_test(&parent);
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        trace(&mut f, &commands);
        let initial = catalog
            .observed_active_volume_index(&control.serial, target)
            .await
            .unwrap();
        let mut restarted = false;
        let mut closed_witness = false;
        let mut closed_snapshot = None;
        let mut metadata_loss = None;
        let mut done = false;
        let mut covered_checked = false;
        eprintln!(
            "DIRECTORY_SUCCESSOR_START namespace={} operation={} source={} target={} directory_uid={}",
            parent.namespace(),
            original.operation(),
            source,
            target,
            dir_uid
        );
        for step in 0..64 {
            let result = Box::pin(fdb_reclaim::advance(
                &mut f.owner,
                &f.permit,
                &control,
                &status,
            ))
            .await;
            let task = catalog
                .management_task(
                    &control.serial,
                    control.route,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            let head = catalog
                .observed_active_volume_index(&control.serial, target)
                .await
                .unwrap();
            eprintln!(
                "DIRECTORY_SUCCESSOR_STEP namespace={} step={} delta={} result={:?} status={}",
                parent.namespace(),
                step,
                head.generation - initial.generation,
                result,
                task["status"]
            );
            if let Some(facts) = stop.facts().filter(|_| !restarted) {
                assert!(result.is_err());
                assert_eq!(head.generation - initial.generation, pre_group_delta);
                drop(heartbeat.take());
                f.owner.fdb_reclaim_source_session = None;
                f.owner.fdb_read_session = None;
                f.owner.fdb_reads.clear();
                f.owner.write_session = None;
                let cache = f.owner.files.extent_cache_dir();
                if cache.exists() {
                    std::fs::remove_dir_all(cache).unwrap();
                }
                take_over_original_group(
                    &mut f,
                    &control,
                    &status,
                    &parent,
                    &original,
                    facts.group,
                    &commands,
                )
                .await;
                heartbeat = Some(ParentPermitHeartbeat::new(&f));
                restarted = true;
                if unknown {
                    metadata_loss = Some(catalog.lose_reclaim_metadata_admission_once(&parent));
                }
                continue;
            }
            if restarted && !closed_witness {
                closed_witness = witness_closed_directory(
                    &cluster,
                    &parent,
                    stop.facts().unwrap().group,
                    dir_uid,
                )
                .await;
                if closed_witness {
                    assert_eq!(
                        catalog
                            .copies_on_volume(source, None, 100)
                            .await
                            .unwrap()
                            .items
                            .len(),
                        0,
                        "original files were already retired by Closed, before the directory can enter Writing"
                    );
                    eprintln!(
                        "DIRECTORY_CLOSED_BEFORE_WRITING namespace={} source_file_copies=0 successor_writing=false",
                        parent.namespace()
                    );
                    closed_snapshot =
                        Some(closed_bytes(&cluster, &parent, stop.facts().unwrap().group).await);
                }
            }
            if metadata_loss.as_ref().is_some_and(|loss| loss.fired()) {
                assert!(unknown && closed_witness && result.is_err());
                let pending = catalog
                    .reclaim_metadata_checkpoint(&parent, f.owner.round)
                    .await
                    .unwrap()
                    .expect("original successor Writing facts");
                assert!(pending.writing());
                assert_eq!(pending.plan().source_uid(), dir_uid);
                assert_eq!(
                    pending.plan().path(),
                    format!("_reclaim_{}/node-{}", original.operation(), dir_uid)
                );
                eprintln!(
                    "DIRECTORY_SUCCESSOR_WRITING namespace={} uid={} task={} group_closed=true old_knownfull=true",
                    parent.namespace(),
                    dir_uid,
                    pending.id()
                );
                let protected =
                    super::isolation_tests::protected_rows(&cluster, &parent, source).await;
                let marker = commands.lock().unwrap().len();
                let negatives = catalog
                    .check_directory_successor_rejections_for_test(
                        &parent,
                        f.owner.round,
                        &pending.plan(),
                        &original_directory,
                    )
                    .await
                    .unwrap();
                assert_eq!(negatives.len(), 15);
                for reason in negatives {
                    eprintln!(
                        "DIRECTORY_SUCCESSOR_REJECT namespace={} predicate={reason}",
                        parent.namespace()
                    );
                }
                assert_eq!(commands.lock().unwrap().len(), marker);
                assert_eq!(
                    super::isolation_tests::protected_rows(&cluster, &parent, source).await,
                    protected
                );
                reject_successor_mutations(
                    &f, &control, &parent, &original, &cluster, source, &commands,
                )
                .await;
                Box::pin(
                    super::isolation_tests::assert_early_unknown_after_closed_directory(
                        &mut f,
                        &control,
                        &status,
                        &parent,
                        &original,
                        &cluster,
                        source,
                        &paths,
                        &commands,
                        &mut heartbeat,
                    ),
                )
                .await;
                eprintln!(
                    "DIRECTORY_SUCCESSOR_UNKNOWN_COMPLETE namespace={} uid={} repeated_restart_ha_zero_io=true",
                    parent.namespace(),
                    dir_uid
                );
                return;
            }
            if closed_witness {
                assert!(
                    result.is_ok(),
                    "qualified original empty directory must remain runnable after KnownFull: {result:?}"
                );
            } else {
                assert!(
                    result.is_ok(),
                    "source fixture or original S4 must reach Closed before checking successor: {result:?}"
                );
            }
            if head.generation - initial.generation == final_delta && !covered_checked {
                let saved = catalog
                    .reclaim_metadata_write_status(&parent, f.owner.round, dir_uid)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(saved.completed());
                let reason = catalog
                    .check_covered_directory_successor_for_test(
                        &parent,
                        f.owner.round,
                        &saved.plan(),
                    )
                    .await
                    .unwrap();
                eprintln!(
                    "DIRECTORY_COVERED_REJECT namespace={} uid={} reason={reason}",
                    parent.namespace(),
                    dir_uid
                );
                covered_checked = true;
            }
            assert!(head.generation - initial.generation <= final_delta);
            if task["status"] == "succeeded" {
                done = true;
                break;
            }
        }
        assert!(restarted && closed_witness && covered_checked && done);
        assert_eq!(
            catalog
                .observed_active_volume_index(&control.serial, target)
                .await
                .unwrap()
                .generation
                - initial.generation,
            final_delta
        );
        assert!(stop.resumed());
        assert_eq!(
            closed_bytes(&cluster, &parent, stop.facts().unwrap().group).await,
            closed_snapshot.unwrap(),
            "directory continuation must retain exact original Closed header/seal and issuer"
        );
        let current = inventory_inner(&f.owner.devices, true).unwrap();
        let target_drive = current
            .drives
            .iter()
            .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == "LAYOUTL8"))
            .unwrap()
            .dev;
        let mounted = LtfsVolume::mount(f.owner.devices[target_drive].dev.as_ref()).unwrap();
        for (ordinal, path) in paths.iter().enumerate() {
            let _ = path;
            let archive = format!(
                "_reclaim_{}/data-{}",
                original.operation(),
                ordinal as u64 + 2
            );
            let mut out = Vec::new();
            mounted.read_file_to_writer(&archive, &mut out).unwrap();
            assert_eq!(out, vec![ordinal as u8 + 37; 1 << 20]);
        }
        let directory = format!("_reclaim_{}/node-{}", original.operation(), dir_uid);
        let actual = mounted.index().find_directory(&directory).unwrap();
        let mut expected = original_directory;
        expected.name = actual.name.clone();
        expected.meta.file_uid = actual.meta.file_uid;
        assert_eq!(
            *actual, expected,
            "all original directory metadata and xattrs must survive relocation"
        );
        eprintln!(
            "DIRECTORY_SUCCESSOR_COMPLETE namespace={} target_delta=3 original_managed_bytes=2097152",
            parent.namespace()
        );
    })
}
fn management(control: &FdbOwnerControl, change: Value) -> ManagementRequest {
    ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap()
}
fn trace(f: &mut Fixture, commands: &Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>) {
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
        assert!(f.owner.devices[drive].dev.medium_change_token().is_none());
    }
}
fn setup(
    catalog: Arc<crate::managed_catalog::ManagedCatalog>,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = (
                    Fixture,
                    FdbOwnerControl,
                    crate::managed_catalog::PoolRecord,
                    SharedStatus,
                    Uuid,
                    Uuid,
                    Vec<String>,
                    u64,
                    crate::ltfs::index::DirectoryNode,
                ),
            >,
    >,
> {
    Box::pin(async move {
        let (mut f, control, pool, status) = Box::pin(native_fixture_from(
            catalog.clone(),
            None,
            Fixture::with_drives(2),
        ))
        .await;
        let _heartbeat = ParentPermitHeartbeat::new(&f);
        let source = Uuid::new_v4();
        let assign = management(
            &control,
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":source.to_string(),"confirm_format":true,"force_format":false}),
        );
        assert_eq!(
            execute(&mut f, &control, &status, &assign).await["status"],
            "succeeded"
        );
        let mut paths = Vec::new();
        for n in 0..2u8 {
            paths.push(upload(&mut f, &control, &status, pool.id, n).await);
        }
        let before = catalog
            .observed_active_volume_index(&control.serial, source)
            .await
            .unwrap();
        mkdir(&mut f, &control, &status, pool.id).await;
        let after = catalog
            .observed_active_volume_index(&control.serial, source)
            .await
            .unwrap();
        assert_eq!(after.generation, before.generation + 1);
        let current = inventory_inner(&f.owner.devices, true).unwrap();
        let drive = current
            .drives
            .iter()
            .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == "LIFE00L8"))
            .unwrap()
            .dev;
        let mut mounted = LtfsVolume::mount(f.owner.devices[drive].dev.as_ref()).unwrap();
        mounted.set_reservation_guard(Some(f.owner.key));
        let directory = mounted
            .index()
            .find_directory("z-empty")
            .expect("ordinary mkdir must create the real LTFS source directory");
        assert!(directory.files.is_empty() && directory.subdirs.is_empty());
        let uid = directory.meta.file_uid;
        let original_directory = directory.clone();
        assert_eq!(
            crate::managed_catalog::ActiveIndexIdentity::from(
                mounted.verified_committed_index_construct().unwrap()
            ),
            after
        );
        eprintln!(
            "DIRECTORY_SOURCE_REAL namespace={} uid={} path=z-empty source_generation={} native_mkdir=true",
            control.route.route.namespace, uid, after.generation
        );
        drop(mounted);
        f.lib
            .insert_cartridge(SimCartridge::blank("LAYOUTL8", 128 << 20), 1)
            .unwrap();
        let target = Uuid::new_v4();
        let assign = management(
            &control,
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LAYOUTL8","volume":target.to_string(),"confirm_format":true,"force_format":false}),
        );
        assert_eq!(
            execute(&mut f, &control, &status, &assign).await["status"],
            "succeeded"
        );
        (
            f,
            control,
            pool,
            status,
            source,
            target,
            paths,
            uid,
            original_directory,
        )
    })
}
fn upload<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    pool: Uuid,
    n: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + 'a>> {
    Box::pin(async move {
        let body = vec![n + 37; 1 << 20];
        let spec = ActiveUploadSpec {
            pool_id: pool,
            path: format!("/file-{n}").into_bytes(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: body.len() as u64,
            digest: ContentDigest::Sha256(sha2::Sha256::digest(&body).into()),
        };
        let intent = control
            .catalog
            .prepare_active_upload(
                &control.serial,
                control.route,
                f.owner.round,
                "LIFE00L8",
                &spec,
            )
            .await
            .unwrap();
        let path = intent.ltfs_path().unwrap();
        let mut tmp = crate::local_read_storage::download_file(
            f.owner.files.spool_dir(),
            spec.size,
            control.storage,
        )
        .unwrap();
        tmp.write_all(&body).unwrap();
        tmp.rewind().unwrap();
        assert_eq!(
            Box::pin(fdb_writing::write(
                &mut f.owner,
                &f.permit,
                control,
                status,
                intent.request(),
                tmp
            ))
            .await
            .unwrap(),
            1
        );
        path
    })
}
fn mkdir<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    pool: Uuid,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let spec = NodeMutationSpec {
            pool,
            operation: Uuid::new_v4(),
            path: "/z-empty".into(),
            expected_version: None,
            mutation: NodeMutation::CreateDirectory,
        };
        let request = control
            .catalog
            .prepare_node_mutation_request(
                &control.serial,
                control.route,
                f.owner.round,
                &spec,
                &crate::ltfs::label::ltfs_time_now(),
            )
            .await
            .unwrap();
        control
            .catalog
            .queue_node_mutation_request(
                control.route,
                f.owner.round,
                &request,
                control.storage.index_bytes as usize,
            )
            .await
            .unwrap();
        let mut cursor = None;
        for _ in 0..16 {
            Box::pin(fdb_nodes::advance(
                &mut f.owner,
                &f.permit,
                control,
                status,
                &mut cursor,
            ))
            .await
            .unwrap();
            let task = control
                .catalog
                .node_mutation_task_status(
                    &control.serial,
                    control.route,
                    request.context().request_epoch,
                    spec.operation,
                )
                .await
                .unwrap()
                .unwrap();
            if task.task.state == NodeMutationTaskState::Completed {
                return;
            }
            assert_ne!(
                task.task.state,
                NodeMutationTaskState::Rejected,
                "ordinary native mkdir rejected: {task:?}"
            );
        }
        panic!("ordinary mkdir did not complete");
    })
}
pub(super) fn parent_key(parent: &ReclaimReservation, kind: u8) -> Vec<u8> {
    let mut key = b"tape-rs/catalog/22/@/".to_vec();
    key.extend_from_slice(parent.namespace().as_bytes());
    key.extend_from_slice(&[b'R', kind]);
    key.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
    key.extend_from_slice(
        Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
            .unwrap()
            .as_bytes(),
    );
    key
}
fn checked(raw: &[u8], domain: &[u8]) -> Value {
    assert!(raw.len() > 32);
    let n = raw.len() - 32;
    assert_eq!(
        sha2::Sha256::new()
            .chain_update(domain)
            .chain_update(&raw[..n])
            .finalize()[..],
        raw[n..]
    );
    serde_json::from_slice(&raw[..n]).unwrap()
}
fn witness_closed_directory<'a>(
    cluster: &'a str,
    parent: &'a ReclaimReservation,
    group: Uuid,
    uid: u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + 'a>> {
    Box::pin(async move {
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        let ready = checked(
            &tr.get(&parent_key(parent, 0x83), false)
                .await
                .unwrap()
                .unwrap(),
            b"tape-rs/reclaim-group-only/1",
        );
        assert_eq!(ready["decision"]["kind"], "ready");
        let dirs = ready["decision"]["plan"]["directories"].as_array().unwrap();
        assert_eq!(dirs.len(), 2);
        assert!(
            dirs.iter()
                .any(|d| d["uid"] == uid && d["path"] == "z-empty")
        );
        assert_eq!(
            ready["decision"]["plan"]["members"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let Some(raw) = tr.get(&parent_key(parent, 0x85), false).await.unwrap() else {
            return false;
        };
        let isolation = checked(&raw, b"tape-rs/reclaim-isolation/1");
        if isolation["disposition"]["state"] != "observed_known_full" {
            return false;
        }
        assert_eq!(isolation["subject"]["group"], group.to_string());
        let mut h = parent_key(parent, 0x82);
        h.push(b'g');
        h.extend_from_slice(group.as_bytes());
        h.extend_from_slice(b"/h");
        let header: Value =
            serde_json::from_slice(&tr.get(&h, false).await.unwrap().unwrap()).unwrap();
        assert_eq!(header["phase"], "closed");
        assert_eq!(header["count"], 2);
        assert_eq!(header["execution_epoch"], 7);
        let mut entry = parent_key(parent, b'e');
        entry.extend_from_slice(&uid.to_be_bytes());
        let object: Value =
            serde_json::from_slice(&tr.get(&entry, false).await.unwrap().unwrap()).unwrap();
        assert_eq!(object["uid"], uid);
        assert_eq!(object["path"], "z-empty");
        let mut coverage = parent_key(parent, b'q');
        coverage.extend_from_slice(&uid.to_be_bytes());
        assert!(
            tr.get(&coverage, false).await.unwrap().is_none(),
            "the original empty directory must still need metadata"
        );
        let walk: Value = serde_json::from_slice(
            &tr.get(&parent_key(parent, b's'), false)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(walk["copied_files"], 2);
        assert_eq!(walk["copied_bytes"], 2 << 20);
        eprintln!(
            "DIRECTORY_READY_CLOSED_WITNESS namespace={} group={} root_and_empty_dirs=2 original_uid={} uncovered=true copied_files=2",
            parent.namespace(),
            group,
            uid
        );
        true
    })
}

fn reject_successor_mutations<'a>(
    f: &'a Fixture,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    original: &'a ManagementRequest,
    cluster: &'a str,
    source: Uuid,
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let before = super::isolation_tests::protected_rows(cluster, parent, source).await;
        let start = commands.lock().unwrap().len();
        let reap = control
            .catalog
            .reap_reclaim_parent_history_pins(parent, f.owner.round)
            .await;
        eprintln!(
            "DIRECTORY_WRITING_DIRECT_MUTATOR namespace={} mutator=reaper rejected={} before_dispatcher=true",
            parent.namespace(),
            reap.is_err()
        );
        assert!(
            reap.is_err(),
            "old KnownFull must not bypass the live successor Writing guard: {reap:?}"
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
        assert_eq!(commands.lock().unwrap().len(), start);
        assert_eq!(
            super::isolation_tests::protected_rows(cluster, parent, source).await,
            before
        );
        eprintln!(
            "DIRECTORY_WRITING_DIRECT_MUTATORS_COMPLETE namespace={} rejected=5 cdbs=0 protected_rows={} unchanged=true",
            parent.namespace(),
            before.len()
        );
    })
}

fn closed_bytes<'a>(
    cluster: &'a str,
    parent: &'a ReclaimReservation,
    group: Uuid,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = (Vec<u8>, Vec<u8>)> + 'a>> {
    Box::pin(async move {
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        let mut prefix = parent_key(parent, 0x82);
        prefix.push(b'g');
        prefix.extend_from_slice(group.as_bytes());
        let mut h = prefix.clone();
        h.extend_from_slice(b"/h");
        let mut c = prefix;
        c.extend_from_slice(b"/c");
        (
            tr.get(&h, false).await.unwrap().unwrap().to_vec(),
            tr.get(&c, false).await.unwrap().unwrap().to_vec(),
        )
    })
}
