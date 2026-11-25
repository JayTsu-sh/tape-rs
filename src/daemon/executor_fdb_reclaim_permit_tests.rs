// Test-only confirmations for the healthy Default fixture. This is not a
// production quorum implementation or permission fallback.
#[derive(Clone, Debug, Default)]
struct HealthyParentPermitReport {
    renewals: u64,
    failure: Option<&'static str>,
}
struct HealthyParentPermit {
    stop: Arc<std::sync::atomic::AtomicBool>,
    report: Arc<std::sync::Mutex<HealthyParentPermitReport>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl HealthyParentPermit {
    fn new(permit: Arc<Permission>, round: u64, control: &FdbOwnerControl) -> Self {
        assert!(permit.allows(round, Phase::Serving));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let report = Arc::new(std::sync::Mutex::new(HealthyParentPermitReport::default()));
        let flag = stop.clone();
        let observed = report.clone();
        let catalog = control.catalog.clone();
        let serial = control.serial.clone();
        let route = control.route;
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let scope_deadline = Instant::now() + Duration::from_secs(600);
            loop {
                std::thread::park_timeout(Duration::from_millis(250));
                if flag.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let failure = if Instant::now() >= scope_deadline {
                    Some("健康多数派模拟超过600秒预算")
                } else if !permit.allows(round, Phase::Serving) {
                    Some("原轮次/Serving许可已失效或改变")
                } else {
                    // A dropped/unknown FDB confirmation cannot renew a lease.
                    // The read future is cancelled after 2s so Drop can join.
                    let confirmation_started = Instant::now();
                    let owner = runtime.block_on(async {
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            catalog.active_library_owner(&serial),
                        )
                        .await
                    });
                    match owner {
                        Ok(Ok(Some((current, owner)))) if current == route && owner == round => {
                            if flag.load(std::sync::atomic::Ordering::Acquire) {
                                break;
                            }
                            if !permit.allows(round, Phase::Serving)
                                || !permit.renew(
                                    round,
                                    Phase::Serving,
                                    confirmation_started + Duration::from_secs(60),
                                )
                            {
                                Some("原许可不可续期，不能复活或改变阶段")
                            } else {
                                observed.lock().unwrap().renewals += 1;
                                None
                            }
                        }
                        Ok(Ok(_)) => Some("原FDB owner或完整route已改变"),
                        _ => Some("原FDB owner多数派确认不可核实"),
                    }
                };
                if let Some(failure) = failure {
                    observed.lock().unwrap().failure = Some(failure);
                    // Only close this scope's original round. A newer round
                    // cannot be revoked or revived by the old fixture scope.
                    permit.close(round);
                    break;
                }
            }
        });
        Self {
            stop,
            report,
            thread: Some(thread),
        }
    }
    fn stop_and_join(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            if thread.join().is_err() {
                self.report.lock().unwrap().failure = Some("健康多数派模拟线程异常退出");
            }
        }
    }
    fn finish(mut self) -> HealthyParentPermitReport {
        self.stop_and_join();
        self.report.lock().unwrap().clone()
    }
}
impl Drop for HealthyParentPermit {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；仅测试健康多数派模拟停止/实际owner失权，默认60秒续期不变"]
async fn native_parent_quorum_heartbeat_stops_without_revival_or_new_cdb() {
    use crate::daemon::permission::GuardedTransport;
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    let (fixture, control, _, _) =
        Box::pin(native_fixture_from(catalog.clone(), None, Fixture::new())).await;
    let round = fixture.owner.round;

    // Fault-only short initial grant. Stop before the first confirmation; the
    // healthy helper still always issues exactly 60s grants, never short TTLs.
    let short = Arc::new(Permission::default());
    let expiry = Instant::now() + Duration::from_millis(100);
    assert!(short.renew(round, Phase::Serving, expiry));
    let guard = GuardedTransport::new(
        Box::new(Recorded {
            inner: Box::new(fixture.lib.drive_as(0, 1)),
            kind: DeviceKind::Drive,
            commands: fixture.commands.clone(),
        }),
        short.clone(),
        round,
    );
    let heartbeat = HealthyParentPermit::new(short.clone(), round, &control);
    let report = heartbeat.finish();
    assert_eq!(report.renewals, 0, "停止后不能补发首次确认");
    assert!(report.failure.is_none());
    assert_eq!(short.deadline(round, Phase::Serving), Some(expiry));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(!short.renew(
        round,
        Phase::Serving,
        Instant::now() + Duration::from_secs(60)
    ));
    let before = fixture.commands.lock().unwrap().len();
    assert!(matches!(
        guard.execute_no_data(&[0; 6], 1000),
        Err(TapeError::OwnershipLost { .. })
    ));
    assert_eq!(fixture.commands.lock().unwrap().len(), before);

    // Confirm a real current FDB owner, then move that actual owner fence.
    let heartbeat = HealthyParentPermit::new(fixture.permit.clone(), round, &control);
    let wait = Instant::now() + Duration::from_secs(5);
    while heartbeat.report.lock().unwrap().renewals == 0 {
        assert!(
            Instant::now() < wait,
            "须先完成实际owner确认及同轮次60秒续期"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    catalog
        .claim_active_owner_epoch(&control.serial, round + 1)
        .await
        .unwrap();
    let wait = Instant::now() + Duration::from_secs(4);
    while heartbeat.report.lock().unwrap().failure.is_none() {
        assert!(Instant::now() < wait, "实际owner改变须停止确认");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let report = heartbeat.finish();
    assert_eq!(report.failure, Some("原FDB owner或完整route已改变"));
    assert!(report.renewals > 0);
    assert!(!fixture.permit.allows(round, Phase::Serving));
    assert!(!fixture.permit.renew(
        round,
        Phase::Serving,
        Instant::now() + Duration::from_secs(60)
    ));
    let before = fixture.commands.lock().unwrap().len();
    assert!(matches!(
        fixture.device().execute_no_data(&[0; 6], 1000),
        Err(TapeError::OwnershipLost { .. })
    ));
    assert_eq!(
        fixture.commands.lock().unwrap().len(),
        before,
        "失权后原guard零新增CDB"
    );
    assert_eq!(
        catalog.active_library_owner(&control.serial).await.unwrap(),
        Some((control.route, round + 1))
    );
}
