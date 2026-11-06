use super::*;
use crate::daemon::fdb_media::establish_active_media_owner;
use crate::daemon::permission::{GuardedTransport, Permission, Phase};
use crate::scsi::cdb::opcode;
use crate::scsi::device::ScsiResult;
use crate::scsi::sim::{Fault, FaultAction, SimTransport};
use crate::scsi::transport::TapeTransport;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// Forward every command to the real simulator while retaining a write count
// outside the owning GuardedTransport. This does not generate media evidence.
struct TrackedTransport {
    inner: SimTransport,
    writes: Arc<AtomicUsize>,
}

impl TrackedTransport {
    fn count(&self, cdb: &[u8]) {
        if matches!(
            cdb.first(),
            Some(&opcode::WRITE_6 | &opcode::WRITE_FILEMARKS_6 | &opcode::WRITE_ATTRIBUTE)
        ) {
            self.writes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl TapeTransport for TrackedTransport {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> crate::error::Result<ScsiResult> {
        self.count(cdb);
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        data: &mut [u8],
        timeout: u32,
    ) -> crate::error::Result<ScsiResult> {
        self.count(cdb);
        self.inner.execute_read(cdb, data, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> crate::error::Result<ScsiResult> {
        self.count(cdb);
        self.inner.execute_write(cdb, data, timeout)
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
}

#[derive(Clone, Copy)]
enum Case {
    Unobserved,
    Observed,
    Unchanged,
    Incomplete,
    Unknown,
    Closed,
}

#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 owner 接管恢复未观察节点 S4，不接触硬件"]
async fn takeover_completes_unobserved_node_s4_without_reappending() {
    scenario(Case::Unobserved).await;
}
#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 owner 接管发布已观察节点 S4，不接触硬件"]
async fn takeover_publishes_observed_node_s4_without_reappending() {
    scenario(Case::Observed).await;
}
#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 owner 接管关闭未发生尝试并重建，不接触硬件"]
async fn takeover_closes_unchanged_node_attempt_and_rebases() {
    scenario(Case::Unchanged).await;
}
#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；实际 owner 接管关闭不完整尾部并重建，不接触硬件"]
async fn takeover_closes_incomplete_node_attempt_and_rebases() {
    scenario(Case::Incomplete).await;
}
#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；未知节点 S4 拒绝 owner 接管，不接触硬件"]
async fn takeover_refuses_unknown_node_s4_and_keeps_writing() {
    scenario(Case::Unknown).await;
}
#[tokio::test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；失效许可不推进 owner 或节点恢复，不接触硬件"]
async fn closed_permission_does_not_claim_owner_or_resume_node_writes() {
    scenario(Case::Closed).await;
}

async fn scenario(case: Case) {
    let _network = unsafe { foundationdb::boot() };
    let fixture = Box::pin(publication_scenario(false, false, false, true))
        .await
        .unwrap();
    Box::pin(workflow(fixture, case)).await;
}

async fn workflow(fixture: PublicationFixture, case: Case) {
    let PublicationFixture {
        catalog,
        serial,
        route,
        owner,
        pool,
        scan,
        device,
        root,
    } = fixture;
    let scope = Scope {
        catalog: &catalog,
        serial: &serial,
        route,
        owner,
        pool,
        scan,
    };
    let old_key = ReservationKey::new(3, owner);
    fence(&device, old_key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(old_key));
    let spec = scope.spec("/takeover", None, NodeMutation::CreateDirectory);
    let (_request, plan, attempt) =
        Box::pin(scope.admit(&mut volume, &spec, "2026-11-01T00:20:00.000000000Z")).await;
    match case {
        Case::Observed => Box::pin(scope.commit(&mut volume, &plan, &attempt)).await,
        Case::Unchanged => {}
        _ => {
            volume.stage_node_mutation(&plan).unwrap();
            device.inject(Fault {
                opcode: if matches!(case, Case::Incomplete) {
                    opcode::WRITE_6
                } else {
                    opcode::WRITE_ATTRIBUTE
                },
                after: 0,
                remaining: 1,
                action: FaultAction::CheckCondition {
                    sense_key: 0x03,
                    asc: 0x0c,
                    ascq: 0,
                },
            });
            assert!(volume.sync_batch().is_err());
            device.clear_faults();
        }
    }
    drop(volume);
    if matches!(case, Case::Unknown) {
        let mut foreign = LtfsVolume::mount(&device).unwrap();
        foreign.set_reservation_guard(Some(old_key));
        foreign
            .set_root_xattr("foreign.change", "unknown S4")
            .unwrap();
        foreign.sync_batch().unwrap();
    }
    let task = catalog
        .node_mutation_task(&serial, route, attempt.epoch, attempt.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.state, NodeMutationTaskState::Writing);
    let next_owner = owner + 1;
    let key = ReservationKey::new(2, next_owner);
    fence(&device, key).unwrap();
    let permission = Arc::new(Permission::default());
    assert!(permission.renew(
        next_owner,
        Phase::Execution,
        Instant::now() + Duration::from_secs(120)
    ));
    let writes = Arc::new(AtomicUsize::new(0));
    let guarded = GuardedTransport::new(
        Box::new(TrackedTransport {
            inner: device,
            writes: writes.clone(),
        }),
        permission.clone(),
        next_owner,
    );
    let volume = LtfsVolume::mount(&guarded).unwrap();
    let source = volume.committed_index_construct();
    if matches!(case, Case::Closed) {
        permission.close(next_owner);
    }
    let written = writes.load(Ordering::Relaxed);
    let result = Box::pin(establish_active_media_owner(
        &catalog,
        volume,
        &serial,
        key,
        &permission,
        4 << 20,
    ))
    .await;
    if matches!(case, Case::Unknown | Case::Closed) {
        assert!(result.is_err());
        assert_eq!(writes.load(Ordering::Relaxed), written);
        let current_owner = if matches!(case, Case::Closed) {
            owner
        } else {
            next_owner
        };
        assert_eq!(
            catalog.active_library_owner(&serial).await.unwrap(),
            Some((route, current_owner))
        );
        assert_eq!(
            catalog
                .pending_node_mutation_attempt(&serial, route, current_owner, source.volume_uuid)
                .await
                .unwrap(),
            Some(attempt)
        );
        assert_eq!(
            catalog
                .node_mutation_task(&serial, route, task.request_epoch, task.operation)
                .await
                .unwrap()
                .unwrap()
                .state,
            NodeMutationTaskState::Writing
        );
    } else {
        let recovered = result.unwrap();
        let published = catalog
            .node_mutation_publication(
                &serial,
                route,
                next_owner,
                task.request_epoch,
                task.operation,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.index(), published.current());
        assert_eq!(published.completed_owner(), next_owner);
        assert!(
            catalog
                .pending_node_mutation_attempt(&serial, route, next_owner, source.volume_uuid)
                .await
                .unwrap()
                .is_none()
        );
        if matches!(case, Case::Observed | Case::Unobserved) {
            assert_eq!(published.attempt(), &attempt);
            assert_eq!(published.current(), ActiveIndexIdentity::from(source));
            assert_eq!(writes.load(Ordering::Relaxed), written);
        } else {
            assert_ne!(published.attempt().id, attempt.id);
            let closure = catalog
                .node_mutation_attempt_closure(route, next_owner, &attempt)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(published.attempt().previous, closure.current());
            if matches!(case, Case::Unchanged) {
                assert_eq!(closure.kind(), NodeMutationAbsenceKind::Unchanged);
            }
            assert!(writes.load(Ordering::Relaxed) > written);
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
