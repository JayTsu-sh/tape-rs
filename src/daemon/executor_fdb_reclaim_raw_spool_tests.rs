// Included inside lifecycle::tests: exercise the complete original parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；未映射raw正文单次实读、独立目标验证和完整回收"]
async fn native_reclaim_unmapped_raw_body_reads_source_once() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    Box::pin(native_reclaim_parent_case(
        catalog,
        &cluster,
        NativeParentCase::RawBodySingleRead,
    ))
    .await;
}

struct RawBodyReadCounter {
    inner: Box<dyn TapeTransport + Send>,
    observed: Arc<std::sync::atomic::AtomicU64>,
    physical: Arc<std::sync::atomic::AtomicU64>,
    body_position: (u8, u64),
    position: std::sync::Mutex<(Option<(u8, u64)>, bool)>,
}
impl TapeTransport for RawBodyReadCounter {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.inner.execute_no_data(cdb, timeout)?;
        if matches!(cdb.first(), Some(0x92 | 0x2b | 0x11 | 0x01 | 0x1b)) {
            let mut position = self.position.lock().unwrap();
            *position = if cdb.first() == Some(&0x92) && cdb.len() >= 12 && result.status == 0 {
                let target = (cdb[3], u64::from_be_bytes(cdb[4..12].try_into().unwrap()));
                (Some(target), target == self.body_position)
            } else {
                (None, false)
            };
        }
        Ok(result)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        self.inner.execute_write(cdb, data, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        data: &mut [u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.inner.execute_read(cdb, data, timeout);
        // A single successful LOCATE to the actual body extent arms exactly
        // one READ response. ASC's forward gap reads keep their own LOCATE
        // target (the later index), and remain in the physical total only.
        let (last_locate, primary) = if cdb.first() == Some(&0x08) {
            let mut position = self.position.lock().unwrap();
            let previous = *position;
            position.1 = false;
            previous
        } else {
            (None, false)
        };
        let result = result?;
        if cdb.first() == Some(&0x08)
            && result.transferred == b"unmapped original bytes".len()
            && data.get(..result.transferred) == Some(b"unmapped original bytes".as_slice())
        {
            self.physical
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if primary {
                self.observed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            eprintln!(
                "C1 raw payload response: last_locate={last_locate:?} body_position={:?} primary={primary}",
                self.body_position
            );
        }
        Ok(result)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn medium_change_token(&self) -> Option<[u8; 32]> {
        self.inner.medium_change_token()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离FDB_TEST_CLUSTER；实际raw UID暂存准入失败，原任务/覆盖不变并按原父任务恢复"]
async fn native_reclaim_raw_spool_budget_failure_preserves_original_parent() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(crate::managed_catalog::ManagedCatalog::open(Some(&cluster)).unwrap());
    Box::pin(native_reclaim_parent_case(
        catalog,
        &cluster,
        NativeParentCase::RawBodyBudget,
    ))
    .await;
}

fn install_raw_body_counter(
    fixture: &mut Fixture,
    target: &str,
    body_position: (u8, u64),
    observed: Arc<std::sync::atomic::AtomicU64>,
    physical: Arc<std::sync::atomic::AtomicU64>,
) {
    let current = inventory(&fixture.owner.devices).unwrap();
    let target_drive = current
        .drives
        .iter()
        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == target))
        .unwrap()
        .dev;
    let source_drive = (0..fixture.lib.drive_count())
        .find(|d| *d != target_drive)
        .unwrap();
    let inner = std::mem::replace(
        &mut fixture.owner.devices[source_drive].dev,
        Box::new(fixture.lib.drive_as(source_drive, 1)),
    );
    fixture.owner.devices[source_drive].dev = Box::new(RawBodyReadCounter {
        inner,
        observed,
        physical,
        body_position,
        position: std::sync::Mutex::new((None, false)),
    });
}

async fn recover_raw_spool_budget_failure(
    fixture: &mut Fixture,
    control: &FdbOwnerControl,
    parent: &crate::managed_catalog::ReclaimReservation,
    original: &crate::managed_catalog::ManagementRequest,
    hook: super::super::fdb_reclaim_spool_test_hook::BudgetHook,
    source: &str,
) {
    let current = inventory(&fixture.owner.devices).unwrap();
    let source_drive = current
        .drives
        .iter()
        .find(|d| d.loaded.as_ref().is_some_and(|(b, _)| b == source))
        .unwrap()
        .dev;
    {
        let reader = crate::daemon::fdb_read::ActiveMediaReader {
            catalog: &control.catalog,
            device: fixture.owner.devices[source_drive].dev.as_ref(),
            serial: &control.serial,
            route: control.route,
            key: fixture.owner.key,
            permission: &fixture.permit,
            storage: control.storage,
        };
        // A fresh guarded full capture independently checks the original source;
        // its forward gap reads are audit reads, not a main raw-body stream.
        let captured = reader
            .capture_reclaim_source(parent, fixture.owner.files.spool_dir())
            .await
            .unwrap();
        let inventory =
            crate::managed_catalog::ReclaimSourceInventory::from_captured(&captured).unwrap();
        hook.assert_failed(&control.catalog, parent, fixture.owner.round, &inventory)
            .await;
    }
    let recovered = control
        .catalog
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
        recovered.intent(),
        parent.intent(),
        "budget failure must preserve the original persistent parent intent"
    );
    drop(hook); // Restore the real admission budget before retrying this parent.
    // Match the daemon's Lost boundary: fence and destroy the old Active, then
    // continue the same journal/request with a newly established owner.
    fixture.permit.close(fixture.owner.round);
    let round = fixture.owner.round + 100;
    let permit = Arc::new(Permission::default());
    permit.renew(
        round,
        Phase::Fencing,
        Instant::now() + Duration::from_secs(60),
    );
    let adopted = takeover(
        &Provider {
            lib: fixture.lib.clone(),
            commands: fixture.commands.clone(),
        },
        &fixture.opts,
        round,
        Some(&permit),
        fixture.owner.files.clone(),
    )
    .unwrap();
    drop(std::mem::replace(&mut fixture.owner, adopted));
    fixture.permit = permit;
    fixture.permit.renew(
        round,
        Phase::Execution,
        Instant::now() + Duration::from_secs(60),
    );
    control
        .catalog
        .claim_active_owner_epoch(&control.serial, round)
        .await
        .unwrap();
}
