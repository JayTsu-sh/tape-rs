// Internal v2 management admission plus the real parent driver. This is not
// default public-owner scheduling or recovery acceptance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "仅原任务固定private FDB；Sim两普通managed文件、NoneToken、合法v2内部入口debug"]
async fn internal_v2_two_managed_files_reach_format_and_cold_read() {
    Box::pin(internal_v2_managed_group_case(false, None)).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实settle AfterRv domain拒绝、精确无半套与dirty Stored拒绝"]
async fn internal_v2_after_rv_failure_is_atomic_and_dirty_stored_is_rejected() {
    Box::pin(internal_v2_managed_group_case(true, None)).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实typed stage全局route冲突不提交半套"]
async fn internal_v2_stage_global_route_conflict_changes_no_original_rows() {
    Box::pin(internal_v2_managed_group_case(
        false,
        Some(crate::managed_catalog::GroupStageRouteConflict::Global),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实typed stage父route冲突不提交半套"]
async fn internal_v2_stage_parent_route_conflict_changes_no_original_rows() {
    Box::pin(internal_v2_managed_group_case(
        false,
        Some(crate::managed_catalog::GroupStageRouteConflict::Parent),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实Header3后的page冲突保持原行"]
async fn internal_v2_stage_page_missing_binding_changes_no_original_rows() {
    Box::pin(internal_v2_managed_group_case(
        false,
        Some(crate::managed_catalog::GroupStageRouteConflict::PageMissingBinding),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实Header3后的page冲突保持原行"]
async fn internal_v2_stage_valid_binding_preserves_batch_conflict() {
    Box::pin(internal_v2_managed_group_case(
        false,
        Some(crate::managed_catalog::GroupStageRouteConflict::PageConflict),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "原任务固定private FDB；真实driver原admit回执丢失/无Header原attempt仅只读探测"]
async fn internal_v2_admitted_without_header_probes_original_attempt_without_writes() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, true, None, false, false, false, false,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；真实opaque读组封存后wrapper不得再发CDB"]
async fn internal_v2_group_readback_wrapper_has_no_postseal_device_io() {
    Box::pin(internal_v2_managed_group_case_mode(
        false,
        None,
        false,
        Some(fdb_reclaim::ReadbackSealMode::Clean),
        false,
        false,
        false,
        false,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；B实读后monitor owner失权不得结清source"]
async fn internal_v2_group_monitor_owner_failure_does_not_settle() {
    Box::pin(internal_v2_managed_group_case_mode(
        false,
        None,
        false,
        Some(fdb_reclaim::ReadbackSealMode::MonitorOwnerFenced),
        false,
        false,
        false,
        false,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；原Full S4成功后issuer7到liveowner8冷恢复，不重写正文"]
async fn internal_v2_original_full_s4_restarts_under_new_owner_without_body_rewrite() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, false, None, true, false, false, false,
    ))
    .await;
}
async fn internal_v2_managed_group_case(
    after_rv_fault: bool,
    stage_fault: Option<crate::managed_catalog::GroupStageRouteConflict>,
) {
    Box::pin(internal_v2_managed_group_case_mode(
        after_rv_fault,
        stage_fault,
        false,
        None,
        false,
        false,
        false,
        false,
    ))
    .await;
}
async fn internal_v2_managed_group_case_mode(
    after_rv_fault: bool,
    stage_fault: Option<crate::managed_catalog::GroupStageRouteConflict>,
    attempt_probe: bool,
    readback_mode: Option<fdb_reclaim::ReadbackSealMode>,
    original_restart: bool,
    group_only: bool,
    lineage_guards: bool,
    unknown_guard: bool,
) {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        after_rv_fault,
        stage_fault,
        attempt_probe,
        readback_mode,
        original_restart,
        group_only,
        lineage_guards,
        unknown_guard,
        None,
        false,
    ))
    .await;
}

async fn internal_v2_managed_group_case_with_source_scan(
    after_rv_fault: bool,
    stage_fault: Option<crate::managed_catalog::GroupStageRouteConflict>,
    attempt_probe: bool,
    readback_mode: Option<fdb_reclaim::ReadbackSealMode>,
    original_restart: bool,
    group_only: bool,
    lineage_guards: bool,
    unknown_guard: bool,
    source_scan: Option<source_group_tests::Case>,
    grouped_root: bool,
) {
    use crate::managed_catalog::{ActiveUploadSpec, ContentDigest, ManagementRequest};
    use sha2::Digest;
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut cluster_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(std::env::var("FDB_TEST_CLUSTER").unwrap())
        .unwrap();
    let mut cluster_bytes = Vec::new();
    Read::take(&mut cluster_file, 4096)
        .read_to_end(&mut cluster_bytes)
        .unwrap();
    assert_eq!(
        cluster_bytes,
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\n"
    );
    let name = std::ffi::CString::new("group-happy-private-fdb").unwrap();
    let fd = nix::sys::memfd::memfd_create(
        name.as_c_str(),
        nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC
            | nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
    )
    .unwrap();
    let mut sealed = std::fs::File::from(fd);
    sealed.write_all(&cluster_bytes).unwrap();
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
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    let (mut f, mut control, pool, status) = Box::pin(native_fixture_from(
        catalog.clone(),
        None,
        Fixture::with_drives(2),
    ))
    .await;
    if source_scan.is_some_and(|case| case.target_preflight.is_some()) {
        control.storage.index_bytes = 256 << 20;
    }
    let mut heartbeat = Some(ParentPermitHeartbeat::new(&f));
    let request = |change: Value| {
        ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
    };
    let source_volume = uuid::Uuid::new_v4();
    if grouped_root {
        seed_grouped_root_source(&mut f, &control, &status, pool.id, source_volume).await;
    } else {
        let assign = request(
            json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LIFE00L8","volume":source_volume.to_string(),"confirm_format":true,"force_format":false}),
        );
        assert_eq!(
            execute(&mut f, &control, &status, &assign).await["status"],
            "succeeded"
        );
    }
    let mut donor_paths = Vec::new();
    let file_count = source_scan.map_or(2, |case| case.files);
    let key_number = |i| {
        if source_scan.is_some() {
            file_count - i - 1
        } else {
            i
        }
    };
    for i in 0..file_count {
        let body = vec![i + 37; 1 << 20];
        let spec = ActiveUploadSpec {
            pool_id: pool.id,
            path: format!("/file-{}", key_number(i)).into_bytes(),
            operation_id: uuid::Uuid::new_v4(),
            expected_version: None,
            size: body.len() as u64,
            digest: ContentDigest::Sha256(sha2::Sha256::digest(&body).into()),
        };
        let intent = catalog
            .prepare_active_upload(
                &control.serial,
                control.route,
                f.owner.round,
                "LIFE00L8",
                &spec,
            )
            .await
            .unwrap();
        donor_paths.push(intent.ltfs_path().unwrap());
        let mut temporary = crate::local_read_storage::download_file(
            f.owner.files.spool_dir(),
            spec.size,
            control.storage,
        )
        .unwrap();
        temporary.write_all(&body).unwrap();
        temporary.rewind().unwrap();
        assert_eq!(
            Box::pin(fdb_writing::write(
                &mut f.owner,
                &f.permit,
                &control,
                &status,
                intent.request(),
                temporary
            ))
            .await
            .unwrap(),
            1
        );
    }
    let (source_drive, source_nodes, original_root) = {
        let current = inventory_inner(&f.owner.devices, true).unwrap();
        let source_drive = current
            .drives
            .iter()
            .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == "LIFE00L8"))
            .unwrap()
            .dev;
        let volume = LtfsVolume::mount(f.owner.devices[source_drive].dev.as_ref()).unwrap();
        let mut root = volume.index().root.clone();
        root.files.clear();
        root.subdirs.clear();
        (
            source_drive,
            donor_paths
                .iter()
                .map(|path| volume.index().find_file(path).unwrap().clone())
                .collect::<Vec<_>>(),
            root,
        )
    };
    f.lib
        .insert_cartridge(SimCartridge::blank("LAYOUTL8", 128 << 20), 1)
        .unwrap();
    let target_volume = uuid::Uuid::new_v4();
    let assign = request(
        json!({"kind":"media-assign","pool":pool.id.to_string(),"barcode":"LAYOUTL8","volume":target_volume.to_string(),"confirm_format":true,"force_format":false}),
    );
    assert_eq!(
        execute(&mut f, &control, &status, &assign).await["status"],
        "succeeded"
    );
    let old_target = if grouped_root {
        Some(seed_grouped_root_target(&mut f, &control, &status, pool.id).await)
    } else {
        None
    };
    let before = catalog
        .observed_active_volume_index(&control.serial, target_volume)
        .await
        .unwrap();
    let source = catalog
        .tape_assignment(&control.serial, "LIFE00L8")
        .await
        .unwrap()
        .unwrap();
    let mut reclaim_change = json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source_volume.to_string(),"revision":source.revision,"confirm_format":true,"unassign":false});
    if group_only {
        reclaim_change["mode"] = json!("group-only");
    }
    let original = request(reclaim_change);
    catalog
        .admit_management(&control.serial, control.route, f.owner.round, &original)
        .await
        .unwrap();
    let parent = if grouped_root {
        assert!(group_only);
        catalog
            .start_reclaim_management(
                &control.serial,
                control.route,
                f.owner.round,
                original.epoch(),
                original.operation(),
            )
            .await
            .unwrap()
            .unwrap()
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
            .unwrap()
            .unwrap()
    };
    assert_eq!(parent.intent()["version"], 2);
    assert_eq!(parent.groups_archive_root(), grouped_root);
    let pre_group_delta = u64::from(!grouped_root);
    let group_delta = pre_group_delta + 1;
    let fault = after_rv_fault.then(|| catalog.fail_reclaim_settlement_after_rv_once(&parent, 0));
    let metadata_unknown = (unknown_guard && !attempt_probe && !original_restart)
        .then(|| catalog.lose_reclaim_metadata_admission_once(&parent));

    let s4_stop =
        original_restart.then(|| fdb_reclaim::arm_original_checkpoint_s4_stop_for_test(&parent));
    let mut s4_restarted = false;
    let mut limited_observation_checked = false;
    let mut recovery_trace = None;
    let mut attempt_creation_fault =
        attempt_probe.then(|| catalog.fail_original_attempt_creation_once(&parent));
    let mut attempt_fault = None;
    let stage_fault =
        stage_fault.map(|which| catalog.conflict_group_stage_route_once(&parent, which));
    let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
    let source_trace =
        source_scan.map(|_| Arc::new(std::sync::Mutex::new(source_group_tests::Trace::default())));
    let target_preflight = source_scan.and_then(|case| case.target_preflight);
    let check_parallel_preflight = target_preflight
        .is_some()
        .then(|| Arc::new(source_group_tests::EarlyOverlap::default()));
    let mount_position_changed = (target_preflight
        == Some(source_group_tests::TargetPreflightCase::ChangedAppendPosition))
    .then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let source_fault = if let Some(case) = source_scan.filter(|c| c.fault.is_some()) {
        Some(
            Box::pin(source_group_tests::FaultSetup::new(
                &catalog,
                &cluster,
                source_volume,
                case.fault.unwrap(),
            ))
            .await,
        )
    } else {
        None
    };
    for drive in 0..f.lib.drive_count() {
        let trusted_change_evidence = drive != source_drive
            && target_preflight == Some(source_group_tests::TargetPreflightCase::ChangeToken);
        let inner = std::mem::replace(
            &mut f.owner.devices[drive].dev,
            Box::new(f.lib.drive_as(drive, 1)),
        );
        let inner = ParentSourceTrace {
            inner,
            drive,
            commands: commands.clone(),
            no_epoch: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            trusted_change_evidence,
        };
        f.owner.devices[drive].dev =
            if let Some(trace) = source_trace.as_ref().filter(|_| drive == source_drive) {
                Box::new(source_group_tests::SourceTrace {
                    inner,
                    trace: trace.clone(),
                    lib: f.lib.clone(),
                    fault: source_scan.unwrap().fault,
                    source_volume,
                    first_block: source_nodes[0].extents[0].start_block,
                    binding_key: source_fault
                        .as_ref()
                        .and_then(|setup| setup.binding_key.clone()),
                    cluster: cluster.clone(),
                    overlap: check_parallel_preflight.clone(),
                })
            } else if let Some(overlap) = &check_parallel_preflight {
                Box::new(source_group_tests::TargetTrace {
                    inner,
                    overlap: overlap.clone(),
                    position_changed: mount_position_changed.clone(),
                })
            } else {
                Box::new(inner)
            };
        if !trusted_change_evidence {
            assert!(f.owner.devices[drive].dev.medium_change_token().is_none());
        }
    }
    let source_records_before = if readback_mode.is_some() {
        Some(capture_readback_source_records(&catalog, &cluster, source_volume).await)
    } else {
        None
    };
    let readback_marker = readback_mode
        .map(|mode| fdb_reclaim::arm_readback_seal_for_test(&parent, commands.clone(), mode));
    let mut readback_marker_checked = false;
    let mut parallel_format_plan = None;
    let started = Instant::now();
    eprintln!(
        "GROUP_HAPPY_ADMITTED namespace={} operation={} parent={}",
        control.route.route.namespace,
        original.operation(),
        parent.intent()
    );
    let mut done = false;
    let mut lineage_checked = false;
    let mut last_generation = before.generation;
    for step in 0..64 {
        let trace_start = commands.lock().unwrap().len();
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
            .observed_active_volume_index(&control.serial, target_volume)
            .await
            .unwrap();
        eprintln!(
            "GROUP_HAPPY step={step} elapsed={:.3} delta={} result={result:?} task={task}",
            started.elapsed().as_secs_f64(),
            head.generation - before.generation
        );
        if mount_position_changed
            .as_ref()
            .is_some_and(|changed| changed.load(std::sync::atomic::Ordering::SeqCst))
        {
            use crate::scsi::cdb::opcode;
            let error = result.expect_err("actual append-position change must refuse the mount");
            assert!(
                error.to_string().contains("实际追加位置或设备会话已改变"),
                "{error}"
            );
            check_parallel_preflight.as_ref().unwrap().verify();
            assert_eq!(head, before);
            assert_eq!(task["status"], "running");
            assert_eq!(task["progress"]["phase"], "copying");
            assert!(
                commands.lock().unwrap()[trace_start..]
                    .iter()
                    .all(|(_, command)| {
                        !matches!(
                            command[0],
                            opcode::WRITE_6
                                | opcode::WRITE_ATTRIBUTE
                                | opcode::WRITE_FILEMARKS_6
                                | opcode::FORMAT_MEDIUM
                        )
                    })
            );
            assert!(
                catalog
                    .reclaim_format_plan(&parent, f.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            return;
        }
        if let Some(setup) = source_fault.as_ref()
            && source_trace.as_ref().unwrap().lock().unwrap().fired
        {
            assert!(
                result.is_err(),
                "injected source group fault accepted: {result:?}"
            );
            assert_eq!(
                head.generation - before.generation,
                1,
                "only the existing root checkpoint is allowed"
            );
            assert_eq!(task["progress"]["children"], 0);
            assert_eq!(task["operation"], original.operation().to_string());
            assert_eq!(task["status"], "running");
            Box::pin(setup.verify(
                &catalog,
                &cluster,
                &parent,
                f.owner.round,
                source_volume,
                source_trace.as_ref().unwrap(),
                &commands,
                &result,
            ))
            .await;
            return;
        }
        assert!(
            head.generation >= last_generation,
            "observed target head must not decrease"
        );
        last_generation = head.generation;
        if readback_marker
            .as_ref()
            .is_some_and(|marker| marker.destination_read_failed())
        {
            assert!(grouped_root);
            assert!(readback_marker.as_ref().unwrap().reads_overlapped());
            assert!(f.owner.fdb_reclaim_format_source.is_none(), "failed target READ must discard speculative source evidence");
            assert_eq!(head.generation - before.generation, 1);
            let error = result
                .as_ref()
                .expect_err("failed physical READ cannot return a checkpoint")
                .to_string();
            assert!(error.contains("SCSI host error: 0x0021"), "{error}");
            assert_eq!(task["status"], "running");
            assert_eq!(task["progress"]["phase"], "copying");
            let (copies, records) = source_records_before.as_ref().unwrap();
            check_failed_grouped_root_readback(
                &cluster,
                &control,
                &parent,
                f.owner.round,
                source_volume,
                original_root.meta.file_uid,
                &source_nodes,
                copies,
                records,
            )
            .await;
            {
                use crate::scsi::cdb::opcode;
                let trace = commands.lock().unwrap();
                let source_writes: Vec<_> = trace[trace_start..]
                    .iter()
                    .filter(|(drive, cdb)| {
                        *drive == source_drive
                            && matches!(
                                cdb.first().copied(),
                                Some(
                                    opcode::WRITE_6
                                        | opcode::WRITE_FILEMARKS_6
                                        | opcode::WRITE_ATTRIBUTE
                                        | opcode::FORMAT_MEDIUM
                                )
                            )
                    })
                    .collect();
                assert!(
                    source_writes.is_empty(),
                    "source reset CDBs: {source_writes:?}"
                );
            }
            assert!(
                catalog
                    .reclaim_format_plan(&parent, f.owner.round)
                    .await
                    .unwrap()
                    .is_none()
            );
            return;
        }
        if metadata_unknown.as_ref().is_some_and(|fault| fault.fired()) {
            assert!(result.is_err());
            Box::pin(isolation_tests::assert_early_unknown(
                &mut f,
                &control,
                &status,
                &parent,
                &original,
                &cluster,
                source_volume,
                &donor_paths,
                &commands,
                &mut heartbeat,
                None,
            ))
            .await;
            return;
        }

        if group_only && s4_restarted && !limited_observation_checked {
            let (target_drive, trace_start) = recovery_trace.unwrap();
            {
                let trace = commands.lock().unwrap();
                assert!(
                    trace[trace_start..]
                        .iter()
                        .all(|(drive, _)| *drive == target_drive),
                    "limited original observation must not touch the source or changer"
                );
                eprintln!(
                    "GROUP_ONLY_ORIGINAL_TARGET_ONLY namespace={} task_cdbs={} source_or_changer_cdbs=0 cache_lost=true",
                    parent.namespace(),
                    trace.len() - trace_start
                );
            }
            if unknown_guard {
                assert!(
                    result.as_ref().unwrap_err().to_string().contains("MAM"),
                    "must reach actual current MAM predicate: {result:?}"
                );
                Box::pin(isolation_tests::assert_early_unknown(
                    &mut f,
                    &control,
                    &status,
                    &parent,
                    &original,
                    &cluster,
                    source_volume,
                    &donor_paths,
                    &commands,
                    &mut heartbeat,
                    Some(s4_stop.as_ref().unwrap().facts().unwrap().group),
                ))
                .await;
                return;
            }
            assert!(result.is_ok());
            limited_observation_checked = true;
        }
        if group_only && !lineage_checked && head.generation == before.generation + group_delta {
            catalog
                .require_reclaim_group_only_ready(
                    &parent,
                    f.owner.round,
                    control.storage.download_bytes,
                    control.storage.index_bytes,
                )
                .await
                .unwrap();
            if grouped_root {
                check_grouped_root_close(
                    &cluster,
                    &control,
                    &parent,
                    f.owner.round,
                    original_root.meta.file_uid,
                    usize::from(file_count),
                    head,
                    task["progress"]["phase"] == "reformatting",
                )
                .await;
            }
            eprintln!(
                "GROUP_ONLY_REAL_LINEAGE namespace={} root_generation={} group_generation={} ready_after_original_root_and_group=true",
                control.route.route.namespace,
                head.generation - pre_group_delta,
                head.generation
            );
            lineage_checked = true;
            if let Some(source_trace) = &source_trace {
                if let Some(overlap) = &check_parallel_preflight {
                    overlap.verify();
                }
                let trace = commands.lock().unwrap();
                if check_parallel_preflight.is_some() {
                    let group_write = trace[trace_start..]
                        .iter()
                        .position(|(drive, command)| {
                            *drive != source_drive && command[0] == crate::scsi::cdb::opcode::WRITE_6
                        })
                        .map(|at| trace_start + at)
                        .expect("a group data WRITE follows preflight and Fresh mount");
                    let target_labels = (trace_start..group_write)
                        .filter(|&at| {
                            let (drive, command) = &trace[at];
                            *drive != source_drive
                                && *command == crate::scsi::cdb::locate_16(0, 2, true, false)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        target_labels.len(),
                        1,
                        "the mount shares the preflight label, CDB range {trace_start}..{group_write}, target label indexes {target_labels:?}"
                    );
                    let source_group_capture = (trace_start..group_write)
                        .filter(|&at| {
                            let (drive, command) = &trace[at];
                            *drive == source_drive
                                && *command == crate::scsi::cdb::locate_16(0, 2, true, false)
                        })
                        .nth(1)
                        .expect("source A must run after group selection");
                    assert!(target_labels[0] < source_group_capture);
                    let mount = (target_labels[0]..group_write)
                        .filter(|&at| {
                            let (drive, command) = &trace[at];
                            *drive != source_drive && *command == crate::scsi::cdb::locate_eod_16(0)
                        })
                        .nth(1)
                        .expect("the mount still freshly scans IP EOD");
                    let target_mount = trace[mount..group_write]
                        .iter()
                        .filter(|(drive, _)| *drive != source_drive)
                        .map(|(_, command)| command.as_slice())
                        .collect::<Vec<_>>();
                    let resume = target_mount
                        .iter()
                        .position(|&command| command == crate::scsi::cdb::test_unit_ready())
                        .expect("prepared mount handoff must revalidate TUR");
                    assert!(
                        target_mount[..resume]
                            .contains(&crate::scsi::cdb::locate_eod_16(0).as_slice())
                    );
                    assert!(
                        !target_mount[..resume]
                            .contains(&crate::scsi::cdb::locate_eod_16(1).as_slice())
                    );
                    assert!(
                        target_mount[resume..]
                            .contains(&crate::scsi::cdb::read_position().as_slice())
                    );
                    assert!(
                        target_mount[resume..]
                            .iter()
                            .all(|command| command[0] != crate::scsi::cdb::opcode::LOCATE_16),
                        "no remount or relocalization is allowed after the handoff"
                    );
                }
                let source_group_end = trace[trace_start..]
                    .iter()
                    .rposition(|(drive, command)| {
                        *drive != source_drive && command[0] == crate::scsi::cdb::opcode::WRITE_6
                    })
                    .map_or(trace.len(), |at| trace_start + at);
                let labels = trace[trace_start..source_group_end]
                    .iter()
                    .filter(|(drive, command)| {
                        *drive == source_drive
                            && *command == crate::scsi::cdb::locate_16(0, 2, true, false)
                    })
                    .count();
                eprintln!("SOURCE_GROUP_SCAN files={file_count} source_label_locates={labels}");
                let source_trace = source_trace.lock().unwrap();
                source_trace.report_counts(&trace[..source_group_end], source_drive, trace_start);
                assert_eq!(
                    labels, 3,
                    "one outer inventory scan and two group endpoint scans"
                );
                source_trace.verify_order(&trace, source_drive, file_count);
            }
            if lineage_guards {
                Box::pin(check_group_only_real_lineage_faults(
                    &catalog,
                    &cluster,
                    &control,
                    &parent,
                    f.owner.round,
                    before.generation,
                    target_volume,
                    &commands,
                ))
                .await;
                return;
            }
        }
        if let Some(facts) = s4_stop
            .as_ref()
            .and_then(|stop| stop.facts())
            .filter(|_| !s4_restarted)
        {
            assert!(
                result.is_err(),
                "original known S4 interruption must not silently complete"
            );
            assert_eq!(facts.current.generation - before.generation, group_delta);
            assert_eq!(
                facts.previous.generation - before.generation,
                pre_group_delta
            );
            assert_eq!(
                head.generation - before.generation,
                pre_group_delta,
                "S4 succeeded before FDB observation/publication"
            );
            if group_only {
                f.owner.fdb_reclaim_source_session = None;
                f.owner.fdb_read_session = None;
                f.owner.fdb_reads.clear();
                f.owner.write_session = None;
                let cache = f.owner.files.extent_cache_dir();
                if cache.exists() {
                    std::fs::remove_dir_all(cache).unwrap();
                }
                if unknown_guard {
                    isolation_tests::inject_wrong_vci(&f, &facts);
                }
            }
            drop(heartbeat.take());
            let start = take_over_original_group(
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
            recovery_trace = Some((facts.drive, start));
            s4_restarted = true;
            eprintln!(
                "ORIGINAL_S4_RESTART namespace={} operation={} issuer=7 liveowner={} original_batch={} physical_head={:?}",
                control.route.route.namespace,
                original.operation(),
                f.owner.round,
                facts.batch,
                facts.current
            );
            continue;
        }
        if let Some(marker) = readback_marker
            .as_ref()
            .filter(|marker| marker.sealed().is_some())
        {
            if !readback_marker_checked {
                if matches!(
                    readback_mode,
                    Some(fdb_reclaim::ReadbackSealMode::ParallelFormatPreparation)
                ) {
                    assert!(marker.reads_overlapped());
                    assert_eq!(task["progress"]["phase"], "reformatting");
                    let saved = catalog
                        .reclaim_format_plan(&parent, f.owner.round)
                        .await
                        .unwrap()
                        .expect("the advance that closes the group must save the format plan");
                    assert!(!saved.issued());
                    assert_eq!(saved.attempt(), 0);
                    assert!(
                        f.owner.fdb_reclaim_format_source.is_none(),
                        "the complete concurrent source proof must be consumed once"
                    );
                    let trace = commands.lock().unwrap();
                    let after_readback = marker.returned().unwrap().0;
                    assert!(
                        trace[after_readback..]
                            .iter()
                            .all(|(drive, _)| *drive != source_drive),
                        "FDB-only continuation must not recapture the source"
                    );
                    assert!(
                        trace[trace_start..].iter().all(|(drive, command)| {
                            *drive != source_drive
                                || !matches!(command[0], 0x04 | 0x0a | 0x10 | 0x8d)
                        }),
                        "the group-close advance must only prepare the format issue"
                    );
                    parallel_format_plan = Some((step, saved.output()));
                    eprintln!(
                        "GROUP_FORMAT_SOURCE_SAME_ADVANCE plan_saved=true issued=false source_after_target_readback_cdbs=0"
                    );
                }
                let (source_copies, source_records) = source_records_before.as_ref().unwrap();
                if (GroupReadbackAssertion {
                    sealed: marker.sealed().unwrap(),
                    returned: marker.returned(),
                    mode: readback_mode.unwrap(),
                    result: &result,
                    task: &task,
                    control: &control,
                    original: &original,
                    cluster: &cluster,
                    source_volume,
                    target_volume,
                    head,
                    source_copies,
                    source_records,
                    commands: &commands,
                })
                .run()
                .await
                {
                    return;
                }
                readback_marker_checked = true;
            }
        }
        if attempt_creation_fault
            .as_ref()
            .is_some_and(|fault| fault.fired())
        {
            assert!(
                result.is_err(),
                "attempt creation fault must abort original admission"
            );
            let count = attempt_creation_fault
                .as_ref()
                .unwrap()
                .verify_unchanged(&catalog)
                .await
                .unwrap();
            let prepared = catalog
                .resume_reclaim_copy_group(&parent, f.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert!(prepared.prepared());
            assert_eq!(head.generation - before.generation, 1);
            eprintln!(
                "ORIGINAL_ATTEMPT_CREATION_ATOMIC rows={count} no_attempt_or_current_edge=true"
            );
            drop(attempt_creation_fault.take());
            attempt_fault = Some(catalog.lose_original_attempt_admission_once(&parent));
            continue;
        }
        if attempt_fault.as_ref().is_some_and(|fault| fault.fired()) {
            assert!(result.is_err(), "admission response lost must not append");
            if unknown_guard {
                Box::pin(isolation_tests::assert_early_unknown(
                    &mut f,
                    &control,
                    &status,
                    &parent,
                    &original,
                    &cluster,
                    source_volume,
                    &donor_paths,
                    &commands,
                    &mut heartbeat,
                    None,
                ))
                .await;
                return;
            }
            let group = catalog
                .resume_reclaim_copy_group(&parent, f.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                head.generation - before.generation,
                1,
                "only original root checkpoint is allowed before original attempt probe"
            );
            assert!(
                catalog
                    .active_batch_status(&control.serial, target_volume, head.generation + 1)
                    .await
                    .unwrap()
                    .is_none()
            );
            let scope = catalog
                .adopt_reclaim_original_attempt(&parent, f.owner.round, group.id())
                .await
                .unwrap();
            assert!(scope.actual().is_none());
            let before_probe = commands.lock().unwrap().len();
            let seal_marker =
                fdb_reclaim::arm_original_probe_seal_for_test(&parent, commands.clone());
            let evidence = Box::pin(fdb_reclaim::probe_original_for_test(
                &f.owner,
                &f.permit,
                &control,
                &parent,
                group.id(),
            ))
            .await
            .unwrap();
            let (sealed_count, sealed_epoch) = seal_marker.sealed();
            assert!(
                sealed_epoch.is_some(),
                "original media proof must seal a real Some epoch"
            );
            assert_eq!(
                commands.lock().unwrap().len(),
                sealed_count,
                "wrapper performed device I/O after the original media proof final epoch gate"
            );
            eprintln!(
                "ORIGINAL_ATTEMPT_RETURN_SEAL cdbs={sealed_count} epoch={sealed_epoch:?} no_postseal_io=true"
            );
            assert_eq!(
                crate::managed_catalog::ActiveIndexIdentity::from(evidence.base().construct()),
                head
            );
            assert_eq!(
                evidence.current().unwrap().construct(),
                evidence.base().construct()
            );
            evidence.require_scope(&scope).unwrap();
            let trace = commands.lock().unwrap();
            assert!(
                trace.len() > before_probe,
                "opaque probe must execute actual media reads"
            );
            assert!(
                trace[before_probe..].iter().all(|(_, cdb)| !matches!(
                    cdb.first(),
                    Some(0x0a | 0x10 | 0x8d | 0x5f | 0xa5 | 0xa6)
                )),
                "probe cannot WRITE/FM/MAM/PRout/move"
            );
            eprintln!(
                "ORIGINAL_ATTEMPT_TYPED_PROBE cdbs={} no_header=true actual_none=true no_writes=true",
                trace.len() - before_probe
            );
            drop(trace);
            // Corruption belongs only to this new random test namespace. Its
            // exact original row is restored; no scope or replacement is forged.
            catalog
                .reject_missing_original_attempt_for_test(&scope)
                .await
                .unwrap();
            let physical_identity = catalog
                .original_attempt_identity_for_test(&scope)
                .await
                .unwrap();
            let old_owner = f.owner.round;
            let next_owner = old_owner + 1;
            catalog
                .claim_active_owner_epoch(&control.serial, next_owner)
                .await
                .unwrap();
            let next_parent = catalog
                .start_reclaim_copy_group_management(
                    &control.serial,
                    control.route,
                    next_owner,
                    original.epoch(),
                    original.operation(),
                )
                .await
                .unwrap()
                .unwrap();
            let adopted = catalog
                .adopt_reclaim_original_attempt(&next_parent, next_owner, group.id())
                .await
                .unwrap();
            assert_eq!(adopted.issuing_execution_epoch(), old_owner);
            assert_eq!(adopted.owner(), next_owner);
            assert_eq!(adopted.attempt_id(), scope.attempt_id());
            assert_eq!(adopted.base_source(), scope.base_source());
            assert_ne!(adopted.binding_digest(), scope.binding_digest());
            assert_eq!(
                physical_identity,
                catalog
                    .original_attempt_identity_for_test(&adopted)
                    .await
                    .unwrap()
            );
            assert!(
                catalog
                    .check_reclaim_original_attempt(&scope)
                    .await
                    .is_err()
            );
            catalog
                .check_reclaim_original_attempt(&adopted)
                .await
                .unwrap();
            let before_fenced_probe = commands.lock().unwrap().len();
            assert!(
                Box::pin(fdb_reclaim::probe_original_for_test(
                    &f.owner,
                    &f.permit,
                    &control,
                    &parent,
                    group.id()
                ))
                .await
                .is_err()
            );
            assert_eq!(
                commands.lock().unwrap().len(),
                before_fenced_probe,
                "fenced old actor must not issue a new probe CDB"
            );
            eprintln!(
                "ORIGINAL_ATTEMPT_OWNER_CAS issuer={old_owner} owner={next_owner} original_batch=true oldscope_fenced=true"
            );
            return;
        }
        if let Some(fault) = stage_fault.as_ref().filter(|fault| fault.fired()) {
            assert!(
                result.is_err(),
                "original conflicting route must reject stage"
            );
            if fault.expects_business_conflict() {
                assert_eq!(
                    fault.outcome(),
                    Some(crate::managed_catalog::ActiveBatchOutcome::BatchConflict),
                    "valid original binding must preserve successful conflict outcome"
                );
            }
            let rows = fault
                .before()
                .expect("original conflicting pre-stage snapshot");
            let db = foundationdb::Database::from_path(&cluster).unwrap();
            let tr = db.create_trx().unwrap();
            for (key, value) in &rows {
                assert_eq!(
                    tr.get(key, false).await.unwrap().map(|v| v.to_vec()),
                    *value,
                    "stage conflict changed original row {key:?}"
                );
            }
            eprintln!(
                "GROUP_STAGE_CONFLICT_ATOMIC rows={} no_new_binding_or_header=true",
                rows.len()
            );
            return;
        }
        if let Some(fault) = fault.as_ref().filter(|fault| fault.fired()) {
            assert!(
                result.is_err(),
                "AfterRv fault must return a domain failure"
            );
            let before = fault
                .before()
                .expect("real settlement before-write snapshot");
            assert!(
                before
                    .rows
                    .iter()
                    .any(|(key, value)| key == &before.rv && value.is_none())
            );
            let db = foundationdb::Database::from_path(&cluster).unwrap();
            let tr = db.create_trx().unwrap();
            let after_rv = tr.get_read_version().await.unwrap();
            for (key, value) in &before.rows {
                assert_eq!(
                    tr.get(key, false).await.unwrap().map(|v| v.to_vec()),
                    *value,
                    "atomic rejection changed original row {key:?}"
                );
            }
            drop(tr);
            eprintln!(
                "AFTER_RV_ATOMIC_REJECT rows={} verification_absent=true after_rv={after_rv}",
                before.rows.len()
            );
            // Preserve a known old-prototype partial Rv in this isolated
            // namespace, then prove the Stored lane refuses to fill it.
            let dirty = fault.staged_rv().expect("the actual staged Rv bytes");
            let tr = db.create_trx().unwrap();
            tr.set(&before.rv, &dirty);
            tr.commit().await.unwrap();
            let group = catalog
                .resume_reclaim_copy_group(&parent, f.owner.round)
                .await
                .unwrap()
                .unwrap();
            assert!(
                catalog
                    .check_stored_reclaim_group_coverage_for_test(
                        &parent,
                        f.owner.round,
                        group.id(),
                        0
                    )
                    .await
                    .is_err()
            );
            let tr = db.create_trx().unwrap();
            for (key, value) in &before.rows {
                let expected = if key == &before.rv {
                    Some(dirty.clone())
                } else {
                    value.clone()
                };
                assert_eq!(
                    tr.get(key, false).await.unwrap().map(|v| v.to_vec()),
                    expected,
                    "Stored rejection changed original row {key:?}"
                );
            }
            eprintln!(
                "DIRTY_STORED_REJECT preserved_rv_bytes={} no_bundle_repair=true",
                dirty.len()
            );
            return;
        }
        assert!(result.is_ok(), "real driver step {step}: {result:?}");
        assert!(
            head.generation - before.generation <= group_delta,
            "target checkpoint bound exceeded"
        );
        if task["status"] == "succeeded" {
            if let Some((planned_at, output)) = parallel_format_plan {
                use crate::ltfs::volume::LABEL_BLOCK;
                use crate::scsi::cdb::{locate_16, locate_eod_16, opcode};
                assert_eq!(step, planned_at + 1);
                assert_eq!(task["result"]["volume"], output.to_string());
                let trace = commands.lock().unwrap();
                let source: Vec<_> = trace[trace_start..]
                    .iter()
                    .filter(|(drive, _)| *drive == source_drive)
                    .map(|(_, command)| command.as_slice())
                    .collect();
                let first_write = source
                    .iter()
                    .position(|command| {
                        matches!(
                            command[0],
                            opcode::WRITE_6 | opcode::WRITE_FILEMARKS_6 | opcode::WRITE_ATTRIBUTE
                        )
                    })
                    .expect("format requires a source write");
                let observation = &source[..first_write];
                assert!(observation.contains(&locate_16(0, LABEL_BLOCK, true, false).as_slice()));
                assert!(observation.contains(&locate_16(1, LABEL_BLOCK, true, false).as_slice()));
                assert!(observation.contains(&locate_eod_16(1).as_slice()));
                assert!(
                    observation
                        .iter()
                        .any(|command| command[0] == opcode::READ_6)
                );
            }
            done = true;
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "original software budget"
        );
    }
    assert!(source_fault.is_none(), "group source fault did not fire");
    assert!(done, "original parent did not reach terminal");
    if matches!(readback_mode, Some(fdb_reclaim::ReadbackSealMode::ParallelFormatPreparation)) {
        assert!(
            readback_marker.as_ref().unwrap().checked_parallel_source(),
            "must use the original source proof from concurrent physical READs"
        );
        eprintln!("GROUP_FORMAT_SOURCE_OVERLAP physical_reads_rendezvous=true typed_source_proof_consumed=true");
    }
    if grouped_root {
        use crate::ltfs::volume::LABEL_BLOCK;
        use crate::scsi::cdb::{locate_16, locate_eod_16, opcode};
        let trace = commands.lock().unwrap();
        let last_write = trace
            .iter()
            .rposition(|(drive, cdb)| {
                *drive == source_drive
                    && matches!(
                        cdb[0],
                        opcode::WRITE_6 | opcode::WRITE_FILEMARKS_6 | opcode::WRITE_ATTRIBUTE
                    )
            })
            .expect("source reset must write the original fixed recipe");
        let observation: Vec<_> = trace[last_write + 1..]
            .iter()
            .filter(|(drive, _)| *drive == source_drive)
            .map(|(_, cdb)| cdb.as_slice())
            .collect();
        let label_locates = observation
            .iter()
            .filter(|cdb| {
                **cdb == locate_16(0, LABEL_BLOCK, true, false)
                    || **cdb == locate_16(1, LABEL_BLOCK, true, false)
            })
            .count();
        assert_eq!(
            label_locates, 0,
            "post-reset labels are part of the exact recipe read"
        );
        let ip = observation
            .iter()
            .rposition(|cdb| *cdb == locate_16(0, 0, true, false))
            .unwrap();
        let dp = observation
            .iter()
            .position(|cdb| *cdb == locate_16(1, 0, true, false))
            .unwrap();
        let dp_eod = observation
            .iter()
            .position(|cdb| *cdb == locate_eod_16(1))
            .unwrap();
        for range in [ip..dp, dp..dp_eod] {
            assert_eq!(
                observation[range]
                    .iter()
                    .filter(|cdb| cdb[0] == opcode::READ_6)
                    .count(),
                8,
                "must read all seven fixed recipe records and attempt the EOD read"
            );
        }
        eprintln!(
            "SOURCE_FORMAT_POST_RESET label_locates={label_locates} recipe_reads_per_partition=8 physical_recipe_and_eod_verified=true"
        );
    }
    if let Some((drive, start)) = recovery_trace {
        assert!(
            s4_stop.as_ref().unwrap().resumed(),
            "original receipt bytes must be checked during resumed checkpoint"
        );
        let trace = commands.lock().unwrap();
        let writes: Vec<_> = trace[start..]
            .iter()
            .filter(|(at, cdb)| {
                *at == drive && matches!(cdb.first(), Some(0x0a | 0x10 | 0x8d | 0x04))
            })
            .collect();
        assert!(
            writes.is_empty(),
            "known original Full S4 must not append/sync/repair the target: {writes:?}"
        );
        eprintln!(
            "ORIGINAL_S4_RECEIPTS_PRESERVED target_write_commands=0 final_owner={}",
            f.owner.round
        );
    }
    if readback_marker.is_some() {
        assert!(readback_marker_checked);
    }
    let after = catalog
        .observed_active_volume_index(&control.serial, target_volume)
        .await
        .unwrap();
    assert_eq!(after.generation - before.generation, group_delta);
    let formatted = catalog
        .reclaim_format_result(&parent, f.owner.round)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(formatted.output(), source_volume);
    let reset = catalog
        .tape_assignment(&control.serial, "LIFE00L8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reset.volume_id, Some(formatted.output()));
    if grouped_root {
        drop(heartbeat.take());
        check_grouped_root_after_cleanup(
            &mut f,
            &control,
            &cluster,
            &parent,
            original_root.meta.file_uid,
            after,
        )
        .await;
        heartbeat = Some(ParentPermitHeartbeat::new(&f));
    }
    let current = inventory_inner(&f.owner.devices, true).unwrap();
    let drive = current
        .drives
        .iter()
        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == "LAYOUTL8"))
        .unwrap()
        .dev;
    let mounted = LtfsVolume::mount(f.owner.devices[drive].dev.as_ref()).unwrap();
    if let Some(old_path) = old_target {
        let mut body = Vec::new();
        mounted.read_file_to_writer(&old_path, &mut body).unwrap();
        assert_eq!(body, b"original target contents");
        let path = format!("_reclaim_{}", original.operation());
        let root = mounted.index().find_directory(&path).unwrap();
        let mut expected = original_root;
        expected.name = path;
        expected.meta.file_uid = root.meta.file_uid;
        assert_eq!(root.meta, expected.meta);
        assert_eq!(root.name, expected.name);
        assert_eq!(root.xattrs, expected.xattrs);
        assert!(
            root.xattrs
                .iter()
                .any(|attr| attr.key == "source-root-original" && attr.value == "原始根属性")
        );
        assert!(root.meta.file_uid > mounted.index().find_file(&old_path).unwrap().meta.file_uid);
        assert_eq!(root.files.len(), usize::from(file_count));
        assert!(
            root.files
                .iter()
                .all(|node| node.meta.file_uid > root.meta.file_uid)
        );
        assert!(root.subdirs.is_empty());
    }
    for i in 0..file_count {
        let full = crate::managed_catalog::pool_catalog_path(
            pool.id,
            format!("/file-{}", key_number(i)).as_bytes(),
        )
        .unwrap();
        let Some(crate::managed_catalog::ResolvedPath::File { copy, .. }) = catalog
            .resolve_active_path_for_route(&control.serial, &full, control.route)
            .await
            .unwrap()
        else {
            panic!("missing published file")
        };
        assert_eq!(copy.volume_id, target_volume);
        let mut body = Vec::new();
        mounted
            .read_file_to_writer(&donor_paths[usize::from(i)], &mut body)
            .unwrap();
        assert_eq!(body, vec![i + 37; 1 << 20]);
        let original_node = &source_nodes[usize::from(i)];
        let archive_path = format!(
            "_reclaim_{}/data-{}",
            original.operation(),
            original_node.meta.file_uid
        );
        let archive = mounted.index().find_file(&archive_path).unwrap();
        let donor = mounted
            .index()
            .find_file(&donor_paths[usize::from(i)])
            .unwrap();
        assert_eq!(archive.extents, donor.extents);
        let mut expected = original_node.clone();
        expected.name = archive.name.clone();
        expected.meta.file_uid = archive.meta.file_uid;
        expected.extents = archive.extents.clone();
        assert_eq!(*archive, expected, "native archive metadata must survive");
        let mut archived = Vec::new();
        mounted
            .read_file_to_writer(&archive_path, &mut archived)
            .unwrap();
        assert_eq!(archived, body);
    }
    eprintln!(
        "INTERNAL_V2_GROUP_HAPPY_SECONDS={:.6} target_delta={group_delta}",
        started.elapsed().as_secs_f64(),
    );
    drop(heartbeat);
}

fn seed_grouped_root_source<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    pool_id: uuid::Uuid,
    source: uuid::Uuid,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        use crate::managed_catalog::ManagementRequest;
        f.format(source);
        {
            let mut volume = LtfsVolume::mount(f.device()).unwrap();
            volume.set_reservation_guard(Some(f.owner.key));
            volume
                .set_root_xattr("source-root-original", "原始根属性")
                .unwrap();
            volume.commit().unwrap();
        }
        let request = |change: Value| {
            ManagementRequest::parse(json!({"version":1,"library":control.serial,"namespace":control.route.route.namespace.to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":change})).unwrap()
        };
        let home = request(
            json!({"kind":"media-move","pool":null,"barcode":"LIFE00L8","volume":null,"revision":null,"destination":"homeslot"}),
        );
        assert_eq!(
            execute(f, control, status, &home).await["status"],
            "succeeded"
        );
        let import = request(
            json!({"kind":"media-import","pool":pool_id.to_string(),"barcode":"LIFE00L8","volume":null,"path":"/","merge_tapes":true}),
        );
        let result = execute(f, control, status, &import).await;
        assert_eq!(result["status"], "succeeded", "{result}");
    })
}

fn check_grouped_root_close<'a>(
    cluster: &'a str,
    control: &'a FdbOwnerControl,
    parent: &'a crate::managed_catalog::ReclaimReservation,
    owner: u64,
    source_root_uid: u64,
    expected_members: usize,
    target: crate::managed_catalog::ActiveIndexIdentity,
    format_prepared: bool,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        use sha2::Digest;
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let mut coverage_key = directory_successor::parent_key(parent, b'q');
        coverage_key.extend_from_slice(&source_root_uid.to_be_bytes());
        let tr = db.create_trx().unwrap();
        let coverage = tr
            .get(&coverage_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let coverage_row: Value = serde_json::from_slice(&coverage).unwrap();
        assert_eq!(coverage_row["version"], 3);
        assert_eq!(coverage_row["proof"]["kind"], "group_root");
        let group = uuid::Uuid::parse_str(
            coverage_row["proof"]["reference"]["group"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let mut close_key = directory_successor::parent_key(parent, 0x82);
        close_key.push(b'g');
        close_key.extend_from_slice(group.as_bytes());
        close_key.extend_from_slice(b"/c");
        let close = tr.get(&close_key, false).await.unwrap().unwrap();
        let close: Value = serde_json::from_slice(&close).unwrap();
        assert_eq!(close["version"], 4);
        let coverage_digest: [u8; 32] = sha2::Sha256::new()
            .chain_update(b"tape-rs/reclaim-group-root-coverage/3")
            .chain_update(&coverage)
            .finalize()
            .into();
        assert_eq!(close["root_coverage"], serde_json::json!(coverage_digest));
        assert_eq!(close["plan"], coverage_row["proof"]["reference"]["plan"]);
        assert_eq!(
            close["observation"],
            coverage_row["proof"]["reference"]["observation"]
        );
        assert_eq!(close["members"].as_array().unwrap().len(), expected_members);
        assert!(
            control
                .catalog
                .active_batch_status(&control.serial, target.volume_uuid, target.generation)
                .await
                .unwrap()
                .unwrap()
                .completed
        );
        let mut root_key = directory_successor::parent_key(parent, b'e');
        root_key.extend_from_slice(&source_root_uid.to_be_bytes());
        for key in [
            directory_successor::parent_key(parent, b'm'),
            root_key,
            coverage_key.clone(),
        ] {
            let tr = db.create_trx().unwrap();
            let saved = tr.get(&key, false).await.unwrap().unwrap().to_vec();
            tr.clear(&key);
            tr.commit().await.unwrap();
            assert!(
                control
                    .catalog
                    .active_batch_status(&control.serial, target.volume_uuid, target.generation)
                    .await
                    .is_err(),
                "a closed batch before terminal format still needs its source ledger"
            );
            let tr = db.create_trx().unwrap();
            tr.set(&key, &saved);
            tr.commit().await.unwrap();
        }
        let metadata_status = control
            .catalog
            .reclaim_metadata_write_status(parent, owner, source_root_uid)
            .await;
        if format_prepared {
            let error = metadata_status.expect_err("the format plan freezes migration metadata");
            assert!(error.to_string().contains("源卷已进入原持久格式化阶段"));
            eprintln!(
                "GROUPED_ROOT_CLOSE group={group} atomic_rq_and_seal=true format_phase_metadata_blocked=true"
            );
            return;
        }
        assert!(metadata_status.unwrap().is_none());

        for corrupt in [false, true] {
            let tr = db.create_trx().unwrap();
            if corrupt {
                let mut forged = coverage_row.clone();
                forged["proof"]["reference"]["target_uid"] = serde_json::json!(u64::MAX);
                tr.set(&coverage_key, &serde_json::to_vec(&forged).unwrap());
            } else {
                tr.clear(&coverage_key);
            }
            tr.commit().await.unwrap();
            let rejection = control
                .catalog
                .require_reclaim_group_only_ready(
                    parent,
                    owner,
                    control.storage.download_bytes,
                    control.storage.index_bytes,
                )
                .await
                .expect_err("the original closed group needs the exact root coverage");
            assert!(
                rejection.to_string().contains("原关闭组缺正确的归档根覆盖"),
                "unexpected rejection: {rejection}"
            );
        }
        let tr = db.create_trx().unwrap();
        tr.set(&coverage_key, &coverage);
        tr.commit().await.unwrap();
        eprintln!(
            "GROUPED_ROOT_CLOSE group={group} atomic_rq_and_seal=true missing_and_forged_rq_rejected=true"
        );
    })
}

fn check_grouped_root_after_cleanup<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    cluster: &'a str,
    parent: &'a crate::managed_catalog::ReclaimReservation,
    source_root_uid: u64,
    target: crate::managed_catalog::ActiveIndexIdentity,
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let catalog = &control.catalog;
        let owner = f.owner.round;
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let manifest = directory_successor::parent_key(parent, b'm');
        let mut root = directory_successor::parent_key(parent, b'e');
        root.extend_from_slice(&source_root_uid.to_be_bytes());
        let mut coverage = directory_successor::parent_key(parent, b'q');
        coverage.extend_from_slice(&source_root_uid.to_be_bytes());
        let check =
            || catalog.active_batch_status(&control.serial, target.volume_uuid, target.generation);
        let before = check().await.unwrap().unwrap();
        assert!(before.completed);

        let mut saw_partial = false;
        let mut complete = false;
        for _ in 0..64 {
            complete = catalog.step_reclaim_cleanup(parent, owner).await.unwrap();
            if complete {
                break;
            }
            if !saw_partial {
                let tr = db.create_trx().unwrap();
                if tr.get(&root, false).await.unwrap().is_none() {
                    assert!(tr.get(&coverage, false).await.unwrap().is_some());
                    assert!(tr.get(&manifest, false).await.unwrap().is_some());
                    assert_eq!(
                        check()
                            .await
                            .expect("completed batch during source cleanup")
                            .unwrap()
                            .batch_id,
                        before.batch_id
                    );
                    saw_partial = true;
                }
            }
        }
        assert!(saw_partial && complete);
        let tr = db.create_trx().unwrap();
        for key in [&manifest, &root, &coverage] {
            assert!(tr.get(key, false).await.unwrap().is_none());
        }
        drop(tr);
        assert_eq!(
            check()
                .await
                .expect("completed batch after source cleanup")
                .unwrap()
                .batch_id,
            before.batch_id
        );

        // Only the exact, checksummed terminal receipt may replace this
        // ledger evidence. Corrupt its checksum and restore the original.
        let receipt_key = directory_successor::parent_key(parent, b'f');
        let tr = db.create_trx().unwrap();
        let receipt = tr.get(&receipt_key, false).await.unwrap().unwrap().to_vec();
        let mut corrupt = receipt.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        tr.set(&receipt_key, &corrupt);
        tr.commit().await.unwrap();
        assert!(check().await.is_err());
        let tr = db.create_trx().unwrap();
        tr.set(&receipt_key, &receipt);
        tr.commit().await.unwrap();

        let next = owner + 1;
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
        let before_recovery = f.commands.lock().unwrap().len();
        let current = inventory_inner(&f.owner.devices, true).unwrap();
        let drive = current
            .drives
            .iter()
            .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == "LAYOUTL8"))
            .unwrap()
            .dev;
        let mounted = LtfsVolume::mount(f.owner.devices[drive].dev.as_ref()).unwrap();
        let recovered = crate::daemon::fdb_media::establish_active_media_owner(
            catalog,
            mounted,
            &control.serial,
            f.owner.key,
            &f.permit,
            control.storage.index_bytes as usize,
        )
        .await
        .expect("fresh owner must recover the completed physical batch after cleanup");
        assert_eq!(recovered.index(), target);
        let trace = f.commands.lock().unwrap();
        assert!(
            trace[before_recovery..]
                .iter()
                .all(|(_, op)| !matches!(op, 0x04 | 0x0a | 0x10 | 0x8d)),
            "recovery of the closed batch must not write"
        );
        eprintln!(
            "GROUPED_ROOT_CLEANUP batch={} owner={next} partial_and_full=true cold_recovery_no_writes=true",
            before.batch_id
        );
    })
}

fn check_failed_grouped_root_readback<'a>(
    cluster: &'a str,
    control: &'a FdbOwnerControl,
    parent: &'a crate::managed_catalog::ReclaimReservation,
    owner: u64,
    source_volume: uuid::Uuid,
    source_root_uid: u64,
    source_nodes: &'a [crate::ltfs::index::FileNode],
    copies: &'a [crate::managed_catalog::VolumeCopy],
    source_records: &'a [(Vec<u8>, Vec<u8>)],
) -> std::pin::Pin<Box<impl std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let catalog = &control.catalog;
        let remaining = catalog
            .copies_on_volume(source_volume, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(copies, remaining);
        assert!(
            catalog
                .reclaim_format_result(parent, owner)
                .await
                .unwrap()
                .is_none()
        );
        let group = catalog
            .resume_reclaim_copy_group(parent, owner)
            .await
            .unwrap()
            .unwrap();
        assert!(group.requires_observation());
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let tr = db.create_trx().unwrap();
        let walk_key = directory_successor::parent_key(parent, b's');
        let walk = tr.get(&walk_key, false).await.unwrap().unwrap();
        let walk: Value = serde_json::from_slice(&walk).unwrap();
        assert_eq!(walk["copied_files"], 0);
        assert_eq!(walk["copied_bytes"], 0);
        for (key, raw) in source_records {
            assert_eq!(tr.get(key, false).await.unwrap().unwrap().as_ref(), raw);
        }
        for uid in std::iter::once(source_root_uid)
            .chain(source_nodes.iter().map(|node| node.meta.file_uid))
        {
            let mut key = directory_successor::parent_key(parent, b'q');
            key.extend_from_slice(&uid.to_be_bytes());
            assert!(tr.get(&key, false).await.unwrap().is_none());
        }
        eprintln!(
            "GROUPED_ROOT_TARGET_READ_FAILURE after_publication=true original_group={} source_copies={} coverage_absent=true",
            group.id(),
            remaining.len()
        );
    })
}

fn seed_grouped_root_target<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    pool_id: uuid::Uuid,
) -> std::pin::Pin<Box<impl std::future::Future<Output = String> + 'a>> {
    Box::pin(async move {
        use crate::managed_catalog::{ActiveUploadSpec, ContentDigest};
        use sha2::Digest;
        use std::io::{Seek, Write};
        let body = b"original target contents";
        let spec = ActiveUploadSpec {
            pool_id,
            path: b"/target-original".to_vec(),
            operation_id: uuid::Uuid::new_v4(),
            expected_version: None,
            size: body.len() as u64,
            digest: ContentDigest::Sha256(sha2::Sha256::digest(body).into()),
        };
        let intent = control
            .catalog
            .prepare_active_upload(
                &control.serial,
                control.route,
                f.owner.round,
                "LAYOUTL8",
                &spec,
            )
            .await
            .unwrap();
        let mut spool = crate::local_read_storage::download_file(
            f.owner.files.spool_dir(),
            spec.size,
            control.storage,
        )
        .unwrap();
        spool.write_all(body).unwrap();
        spool.rewind().unwrap();
        assert_eq!(
            Box::pin(fdb_writing::write(
                &mut f.owner,
                &f.permit,
                control,
                status,
                intent.request(),
                spool,
            ))
            .await
            .unwrap(),
            1
        );
        intent.ltfs_path().unwrap()
    })
}

// Keep new FDB assertion futures out of the existing large parent-driver poll
// frame. This does not change the test thread or production stack size.
fn capture_readback_source_records<'a>(
    catalog: &'a crate::managed_catalog::ManagedCatalog,
    cluster: &'a str,
    source_volume: uuid::Uuid,
) -> std::pin::Pin<
    Box<
        impl std::future::Future<
            Output = (
                Vec<crate::managed_catalog::VolumeCopy>,
                Vec<(Vec<u8>, Vec<u8>)>,
            ),
        > + 'a,
    >,
> {
    Box::pin(async move {
        let source_copies_before = catalog
            .copies_on_volume(source_volume, None, 100)
            .await
            .unwrap()
            .items;
        assert_eq!(source_copies_before.len(), 2);
        let source_records = {
            let db = foundationdb::Database::from_path(&cluster).unwrap();
            let tr = db.create_trx().unwrap();
            let mut rows = Vec::new();
            for copy in &source_copies_before {
                let mut key = b"tape-rs/catalog/22/R/".to_vec();
                key.extend_from_slice(copy.namespace.as_bytes());
                key.extend_from_slice(copy.file_id.as_bytes());
                key.extend_from_slice(copy.copy_id.as_bytes());
                rows.push((
                    key.clone(),
                    tr.get(&key, false).await.unwrap().unwrap().to_vec(),
                ));
            }
            rows
        };
        (source_copies_before, source_records)
    })
}
struct GroupReadbackAssertion<'a> {
    sealed: (usize, Option<u64>),
    returned: Option<(usize, Option<u64>)>,
    mode: fdb_reclaim::ReadbackSealMode,
    result: &'a std::result::Result<bool, ReadError>,
    task: &'a Value,
    control: &'a FdbOwnerControl,
    original: &'a crate::managed_catalog::ManagementRequest,
    cluster: &'a str,
    source_volume: uuid::Uuid,
    target_volume: uuid::Uuid,
    head: crate::managed_catalog::ActiveIndexIdentity,
    source_copies: &'a [crate::managed_catalog::VolumeCopy],
    source_records: &'a [(Vec<u8>, Vec<u8>)],
    commands: &'a std::sync::Mutex<Vec<(usize, Vec<u8>)>>,
}
impl<'a> GroupReadbackAssertion<'a> {
    fn run(self) -> std::pin::Pin<Box<impl std::future::Future<Output = bool> + 'a>> {
        Box::pin(async move {
            let Self {
                result,
                task,
                control,
                original,
                cluster,
                source_volume,
                target_volume,
                head,
                commands,
                ..
            } = self;
            let catalog = &control.catalog;
            let (sealed_count, sealed_epoch) = self.sealed;
            assert!(
                sealed_epoch.is_some(),
                "actual reader must seal Some device epoch"
            );
            if matches!(self.mode, fdb_reclaim::ReadbackSealMode::MonitorOwnerFenced) {
                let error = result
                    .as_ref()
                    .expect_err("monitor owner loss must reject checkpoint")
                    .to_string();
                assert!(
                    error.contains("磁带监控权威不可用"),
                    "must reach real monitor rejection: {error}"
                );
                assert!(self.returned.is_none());
                let db = foundationdb::Database::from_path(&cluster).unwrap();
                let tr = db.create_trx().unwrap();
                let rv = tr.get_read_version().await.unwrap();
                let mut walk_key = b"tape-rs/catalog/22/@/".to_vec();
                walk_key.extend_from_slice(control.route.route.namespace.as_bytes());
                walk_key.extend_from_slice(b"Rs");
                walk_key.extend_from_slice(&original.epoch().to_be_bytes());
                walk_key.extend_from_slice(original.operation().as_bytes());
                let raw_walk = tr.get(&walk_key, false).await.unwrap().unwrap();
                let walk: Value = serde_json::from_slice(&raw_walk).unwrap();
                assert_eq!(walk["copied_files"], 0);
                assert_eq!(walk["copied_bytes"], 0);
                for (key, raw) in self.source_records {
                    assert_eq!(
                        tr.get(key, false).await.unwrap().as_deref(),
                        Some(raw.as_slice()),
                        "monitor rejection must not retire the source copy"
                    );
                }
                drop(tr);
                eprintln!(
                    "GROUP_MONITOR_REJECT_SOURCE_RV={rv} copied_files=0 copied_bytes=0 source_records_unchanged=2"
                );
                assert_eq!(
                    catalog
                        .copies_on_volume(source_volume, None, 100)
                        .await
                        .unwrap()
                        .items,
                    self.source_copies
                );
                let source_now = catalog
                    .tape_assignment(&control.serial, "LIFE00L8")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(source_now.volume_id, Some(source_volume));
                assert_ne!(task["status"], "succeeded");
                eprintln!(
                    "GROUP_MONITOR_OWNER_REJECT copied=0 source_reverse_unchanged=true no_format=true sealed_cdbs={sealed_count}"
                );
                return true;
            }
            let (returned_count, returned_epoch) = self
                .returned
                .expect("checkpoint wrapper must return after monitor success");
            let trace = commands.lock().unwrap();
            let extra: Vec<_> = trace[sealed_count..returned_count]
                .iter()
                .map(|(_, cdb)| cdb[0])
                .collect();
            eprintln!(
                "GROUP_READBACK_RETURN_SEAL sealed={sealed_count} returned={returned_count} epoch={sealed_epoch:?}/{returned_epoch:?} extra_opcodes={extra:?}"
            );
            assert_eq!(
                returned_count, sealed_count,
                "wrapper issued device I/O after sealed actual body/B evidence: {extra:?}"
            );
            assert_eq!(returned_epoch, sealed_epoch);
            drop(trace);
            let monitor = catalog
                .active_tape_monitor(&control.serial, control.route, "LAYOUTL8")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(monitor.volume, target_volume);
            assert_eq!(monitor.verified_generation, Some(head.generation));
            assert!(!monitor.requires_validation);
            false
        })
    }
}

fn take_over_original_group<'a>(
    f: &'a mut Fixture,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a crate::managed_catalog::ReclaimReservation,
    original: &'a crate::managed_catalog::ManagementRequest,
    group: uuid::Uuid,
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = usize> + 'a>> {
    Box::pin(async move {
        let old = f.owner.round;
        assert_eq!(old, 7);
        let next = old + 1;
        control
            .catalog
            .claim_active_owner_epoch(&control.serial, next)
            .await
            .unwrap();
        let before = commands.lock().unwrap().len();
        assert!(
            Box::pin(fdb_reclaim::probe_original_for_test(
                &f.owner, &f.permit, control, parent, group
            ))
            .await
            .is_err()
        );
        assert_eq!(
            commands.lock().unwrap().len(),
            before,
            "old actor must stop before any probe CDB"
        );
        let restored = control
            .catalog
            .start_reclaim_copy_group_management(
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
        assert!(f.permit.renew(
            next,
            Phase::Fencing,
            Instant::now() + Duration::from_secs(60)
        ));
        let next_actor = takeover(
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
        assert!(next_actor.fdb_reclaim_source_session.is_none());
        assert!(next_actor.write_session.is_none());
        f.owner = next_actor;
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
            assert!(f.owner.devices[drive].dev.medium_change_token().is_none());
        }
        if let Some(executor) = status.lock().unwrap().executor.as_mut() {
            executor.round = next;
        }
        commands.lock().unwrap().len()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；原group-only四文件无介质变化token、物理顺序读取"]
async fn public_group_only_source_group_reads_with_two_endpoint_scans() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        None,
        false,
        true,
        false,
        false,
        Some(source_group_tests::Case {
            files: 4,
            fault: None,
            target_preflight: None,
        }),
        false,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；两个Sim驱动器实际命令证明目标预挂载与源正文重叠"]
async fn public_grouped_archive_root_overlaps_target_preflight_with_source_read() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        None,
        false,
        true,
        false,
        false,
        Some(source_group_tests::Case {
            files: 4,
            fault: None,
            target_preflight: Some(source_group_tests::TargetPreflightCase::Overlap),
        }),
        true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；在交接后的真实READ POSITION上模拟追加位置变化"]
async fn public_grouped_archive_root_refuses_changed_prepared_append_position() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        None,
        false,
        true,
        false,
        false,
        Some(source_group_tests::Case {
            files: 4,
            fault: None,
            target_preflight: Some(source_group_tests::TargetPreflightCase::ChangedAppendPosition),
        }),
        true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；目标写入更新介质变化令牌，原组正常完成"]
async fn public_grouped_archive_root_prepared_target_mount_accepts_its_own_token_change() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        None,
        false,
        true,
        false,
        false,
        Some(source_group_tests::Case {
            files: 4,
            fault: None,
            target_preflight: Some(source_group_tests::TargetPreflightCase::ChangeToken),
        }),
        true,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；显式group-only正常public start真实root/group/format，仅两文件非CLI8"]
async fn public_group_only_two_managed_files_validate_real_lineage_and_format() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false, None, false, None, false, true, false, false, None, true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；Sim实际双驱动 READ rendezvous 与格式化候选消费"]
async fn public_group_only_prepares_format_source_during_target_readback() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        Some(fdb_reclaim::ReadbackSealMode::ParallelFormatPreparation),
        false,
        true,
        false,
        false,
        None,
        true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；同代归档根原Full成功后丢回执，接管只读原目标"]
async fn public_grouped_archive_root_s4_restarts_without_source_replay() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false, None, false, None, true, true, false, false, None, true,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；原目标publication后实际READ故障，保留全部源引用且无根覆盖"]
async fn public_grouped_archive_root_read_failure_cannot_retire_source() {
    Box::pin(internal_v2_managed_group_case_with_source_scan(
        false,
        None,
        false,
        Some(fdb_reclaim::ReadbackSealMode::DestinationBodyReadFails),
        false,
        true,
        false,
        false,
        None,
        true,
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "固定private FDB；原root及组真实已提交后，坏目录pub/foreignbatch/repair只读门禁拒绝，禁止继续format"]
async fn public_group_only_real_lineage_rejects_missing_publication_and_foreign_history() {
    Box::pin(internal_v2_managed_group_case_mode(
        false, None, false, None, false, true, true, false,
    ))
    .await;
}
fn check_group_only_real_lineage_faults<'a>(
    catalog: &'a crate::managed_catalog::ManagedCatalog,
    cluster: &'a str,
    control: &'a FdbOwnerControl,
    parent: &'a crate::managed_catalog::ReclaimReservation,
    owner: u64,
    initial: u64,
    target: uuid::Uuid,
    commands: &'a Arc<std::sync::Mutex<Vec<(usize, Vec<u8>)>>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let db = foundationdb::Database::from_path(cluster).unwrap();
        let mut pubkey = b"tape-rs/catalog/22/@/".to_vec();
        pubkey.extend_from_slice(control.route.route.namespace.as_bytes());
        pubkey.extend_from_slice(b"RH");
        pubkey.extend_from_slice(target.as_bytes());
        pubkey.extend_from_slice(&(initial + 1).to_be_bytes());
        let history = |generation: u64| {
            let mut key = b"tape-rs/catalog/22/r/".to_vec();
            key.extend_from_slice(target.as_bytes());
            key.extend_from_slice(&generation.to_be_bytes());
            key
        };
        let root_key = history(initial + 1);
        let group_key = history(initial + 2);
        let mut ready_key = b"tape-rs/catalog/22/@/".to_vec();
        ready_key.extend_from_slice(control.route.route.namespace.as_bytes());
        ready_key.extend_from_slice(&[b'R', 0x83]);
        ready_key.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
        ready_key.extend_from_slice(
            uuid::Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        let original = {
            let tr = db.create_trx().unwrap();
            let mut rows = Vec::new();
            for key in [&pubkey, &root_key, &group_key, &ready_key] {
                rows.push((
                    key.clone(),
                    tr.get(key, false).await.unwrap().unwrap().to_vec(),
                ));
            }
            rows
        };
        assert_eq!(original[1].1[0], 7);
        assert_eq!(original[2].1[0], 1);
        assert!(
            catalog
                .reclaim_format_plan(parent, owner)
                .await
                .unwrap()
                .is_none()
        );
        let before_commands = commands.lock().unwrap().len();
        for case in 0..5 {
            let key = if case == 0 {
                &pubkey
            } else if case == 3 {
                &group_key
            } else {
                &root_key
            };
            let mut bad = if case == 3 {
                original[2].1.clone()
            } else {
                original[1].1.clone()
            };
            if case != 0 {
                bad[0] = if case == 2 {
                    8
                } else if case == 4 {
                    7
                } else {
                    1
                };
                bad[1..17].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            }
            let tr = db.create_trx().unwrap();
            if case == 0 {
                tr.clear(key);
            } else {
                tr.set(key, &bad);
            }
            tr.commit().await.unwrap();
            assert!(
                catalog
                    .require_reclaim_group_only_ready(
                        parent,
                        owner,
                        control.storage.download_bytes,
                        control.storage.index_bytes
                    )
                    .await
                    .is_err(),
                "bad physical history case {case} must be rejected"
            );
            assert_eq!(commands.lock().unwrap().len(), before_commands);
            assert!(
                catalog
                    .reclaim_format_plan(parent, owner)
                    .await
                    .unwrap()
                    .is_none()
            );
            let tr = db.create_trx().unwrap();
            assert_eq!(
                tr.get(&ready_key, false).await.unwrap().unwrap().as_ref(),
                original[3].1.as_slice()
            );
            assert_eq!(
                tr.get(key, false).await.unwrap().map(|v| v.to_vec()),
                if case == 0 { None } else { Some(bad.clone()) }
            );
            eprintln!(
                "GROUP_ONLY_LINEAGE_REJECT namespace={} case={} no_cdb=true no_format_goal=true ready_unchanged=true",
                control.route.route.namespace, case
            );
            // Restore only to query the next independent read-only counterfactual.
            // The driver is never called after any corruption; final namespace
            // deliberately retains the final foreign metadata attempt for diagnostics.
            if case != 4 {
                let tr = db.create_trx().unwrap();
                let raw = &original[if case == 0 {
                    0
                } else if case == 3 {
                    2
                } else {
                    1
                }]
                .1;
                tr.set(key, raw);
                tr.commit().await.unwrap();
            }
        }
    })
}

#[path = "executor_fdb_reclaim_isolation_tests.rs"]
mod isolation_tests;

#[path = "executor_fdb_reclaim_directory_tests.rs"]
mod directory_successor;

#[path = "executor_fdb_reclaim_source_group_tests.rs"]
mod source_group_tests;
