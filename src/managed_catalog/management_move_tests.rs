//! SDK-only mechanical journal tests. No SCSI commands or hardware access.
use super::*;
use crate::changer::element::ElementAddressMap;
fn request(ns: Uuid, serial: &str, destination: &str) -> ManagementRequest {
    ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":destination}})).unwrap()
}
fn plan() -> MovePlan {
    MovePlan {
        version: 1,
        barcode: "TEST01L8".into(),
        captured_at: crate::ltfs::label::ltfs_time_now(),
        map: ElementAddressMap {
            transport_start: 0,
            transport_count: 1,
            storage_start: 1001,
            storage_count: 3,
            ie_start: 101,
            ie_count: 1,
            dt_start: 1,
            dt_count: 1,
        },
        source: MoveLocation {
            address: 1001,
            kind: MoveLocationKind::Storage,
            drive_serial: None,
        },
        destination: MoveLocation {
            address: 101,
            kind: MoveLocationKind::ImportExport,
            drive_serial: None,
        },
        home_address: Some(1001),
    }
}
async fn observe(
    c: &ManagedCatalog,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    address: u16,
    element_type: u8,
) -> String {
    let at = crate::ltfs::label::ltfs_time_now();
    let value = json!({"library_serial":serial,"owner":2,"round":owner,"observed_at":at,"volumes":[{"barcode":"TEST01L8","address":address,"element_type":element_type,"medium_type":1,"source_address":null,"home_slot":if element_type==2{Some(1)}else{None}}],"drives":[{"serial":"SIMDRV0000","address":1,"barcode":null}]});
    c.record_active_library_inventory(serial, route, owner, &value)
        .await
        .unwrap();
    c.record_library_monitor(serial, route, owner, &at, None)
        .await
        .unwrap();
    at
}
#[tokio::test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；验证机械意图/检查点/gate/home/原回执，不访问设备"]
async fn media_move_journal_keeps_original_plan_and_gate_until_confirmed() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("MOVE-{ns}");
    let challenge = Uuid::new_v4();
    let replies=(1..=3).map(|id|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
    let route = c
        .initialize_native_library(
            ns,
            &crate::managed_catalog::verify_native_barriers(&serial, challenge, &replies).unwrap(),
        )
        .await
        .unwrap();
    let owner = route.initial_owner_epoch;
    let r = request(ns, &serial, "ieslot");
    let p = plan();
    let queued = c.admit_management(&serial, route, owner, &r).await.unwrap();
    assert!(queued.as_object().unwrap().contains_key("execution") && queued["execution"].is_null());
    assert!(
        c.stage_media_move(&serial, route, owner, &r, MovePhase::Moving)
            .await
            .is_err()
    );
    let running = c
        .prepare_media_move(&serial, route, owner, &r, &p)
        .await
        .unwrap();
    assert_eq!(running["status"], "running");
    assert_eq!(running["execution"]["plan"], p.value());
    assert_eq!(running["execution"]["phase"], "prepared");
    let witness = ManagedCatalog::open(Some(&cluster)).unwrap();
    assert!(matches!(
        c.require_active_library_readable(&serial, route, owner)
            .await
            .unwrap_err()
            .domain(),
        CatalogError::PoolBusy
    ));

    assert_eq!(
        witness
            .active_library_mechanical_task(&serial, route, owner)
            .await
            .unwrap(),
        Some(running.clone())
    );
    assert_eq!(
        witness
            .active_tape_home_plan(&serial, route, owner, "TEST01L8")
            .await
            .unwrap(),
        Some(p.clone())
    );
    let other = request(ns, &serial, "ieslot");
    c.admit_management(&serial, route, owner, &other)
        .await
        .unwrap();
    assert!(
        c.prepare_media_move(&serial, route, owner, &other, &p)
            .await
            .is_err()
    );
    assert!(
        c.cancel_management(&serial, route, owner, r.epoch(), r.operation())
            .await
            .is_err()
    );
    assert!(
        c.fail_unprepared_media_move(&serial, route, owner, &r, "不能将已准备操作当作没执行")
            .await
            .is_err()
    );
    assert!(
        c.stage_media_move(&serial, route, owner, &r, MovePhase::Unloading)
            .await
            .is_err()
    );
    let next = owner + 1;
    c.claim_active_owner_epoch(&serial, next).await.unwrap();
    assert!(
        c.stage_media_move(&serial, route, owner, &r, MovePhase::Moving)
            .await
            .is_err()
    );
    let adopted = witness
        .prepare_media_move(&serial, route, next, &r, &p)
        .await
        .unwrap();
    assert_eq!(adopted["owner_epoch"], next);
    assert_eq!(adopted["request"], *r.value());
    let mut drift = p.clone();
    drift.captured_at = "2026-11-06T00:00:01.000000000Z".into();
    assert!(
        c.prepare_media_move(&serial, route, next, &r, &drift)
            .await
            .is_err()
    );
    let moving = c
        .stage_media_move(&serial, route, next, &r, MovePhase::Moving)
        .await
        .unwrap();
    assert_eq!(moving["execution"]["phase"], "moving");
    let unknown = c
        .defer_media_move(&serial, route, next, &r, "MOVE回执超时，原结果未知")
        .await
        .unwrap();
    assert_eq!(unknown["status"], "running");
    assert_eq!(unknown["execution"]["error"], "MOVE回执超时，原结果未知");
    assert!(
        c.active_library_mechanical_task(&serial, route, next)
            .await
            .unwrap()
            .is_some()
    );
    let at = observe(&c, &serial, route, next, 1001, 2).await;
    assert!(
        c.complete_media_move(&serial, route, next, &r, &at)
            .await
            .is_err(),
        "仍在源位置不能成功"
    );
    c.stage_media_move(&serial, route, next, &r, MovePhase::Moving)
        .await
        .unwrap();
    let at = observe(&c, &serial, route, next, 101, 3).await;
    assert!(
        c.complete_media_move(&serial, route, next, &r, "2026-11-06T00:00:00.000000000Z")
            .await
            .is_err()
    );
    let done = c
        .complete_media_move(&serial, route, next, &r, &at)
        .await
        .unwrap();
    assert_eq!(done["status"], "succeeded");
    c.require_active_library_readable(&serial, route, next)
        .await
        .unwrap();
    assert_eq!(done["execution"]["phase"], "confirmed");
    assert_eq!(done["result"]["from"], 1001);
    assert_eq!(done["result"]["to"], 101);
    assert!(
        c.active_library_mechanical_task(&serial, route, next)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        witness
            .complete_media_move(&serial, route, next, &r, &at)
            .await
            .unwrap(),
        done
    );
    assert_eq!(
        witness
            .prepare_media_move(&serial, route, next, &r, &p)
            .await
            .unwrap(),
        done
    );
    assert_eq!(
        witness
            .admit_management(&serial, route, next, &r)
            .await
            .unwrap(),
        done
    );
    let home = c
        .active_tape_home_plan(&serial, route, next, "TEST01L8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(home, p);
    // Resolve the opposite direction from the same original home proof.
    let back = request(ns, &serial, "homeslot");
    c.admit_management(&serial, route, next, &back)
        .await
        .unwrap();
    let mut reverse = p.clone();
    reverse.source = p.destination.clone();
    reverse.destination = p.source.clone();
    reverse.captured_at = crate::ltfs::label::ltfs_time_now();
    c.prepare_media_move(&serial, route, next, &back, &reverse)
        .await
        .unwrap();
    c.stage_media_move(&serial, route, next, &back, MovePhase::Moving)
        .await
        .unwrap();
    let at = observe(&c, &serial, route, next, 1001, 2).await;
    let returned = c
        .complete_media_move(&serial, route, next, &back, &at)
        .await
        .unwrap();
    assert_eq!(returned["status"], "succeeded");
    assert_eq!(returned["result"]["to"], 1001);
    let canceled = request(ns, &serial, "homeslot");
    c.admit_management(&serial, route, next, &canceled)
        .await
        .unwrap();
    let canceled = c
        .cancel_management(&serial, route, next, canceled.epoch(), canceled.operation())
        .await
        .unwrap();
    assert_eq!(canceled["status"], "cancelled");
    assert!(canceled["execution"].is_null());
    // A pre-plan failure is honest not_started and cannot release another job.
    let failure = c
        .fail_unprepared_media_move(&serial, route, next, &other, "没有可确认的目标槽")
        .await
        .unwrap();
    assert_eq!(failure["status"], "failed");
    assert_eq!(failure["result"]["effects"], "not_started");
    assert!(
        c.active_library_mechanical_task(&serial, route, next)
            .await
            .unwrap()
            .is_none()
    );
}
