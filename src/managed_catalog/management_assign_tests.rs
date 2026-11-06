//! Private FDB tests of the original assignment checkpoint, never device tests.
use super::*;
use crate::changer::element::ElementAddressMap;
#[tokio::test]
#[ignore = "需要显式隔离FDB_TEST_CLUSTER；不访问设备"]
async fn media_assign_journal_pins_format_intent_and_atomic_membership() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster)).unwrap();
    let ns = Uuid::new_v4();
    let serial = format!("ASSIGN-{ns}");
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

    let pool = PoolRecord {
        id: Uuid::new_v4(),
        library_serial: serial.clone(),
        name: "assign-test".into(),
        file_limit: 100,
        created_at_millis: now(),
        policy: PoolPolicy::default(),
        revision: 1,
    };
    c.create_active_pool(&serial, route, owner, &pool)
        .await
        .unwrap();
    let volume = Uuid::new_v4();
    let r = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-assign","pool":pool.id.to_string(),"barcode":"TEST01L8","volume":volume.to_string(),"confirm_format":true,"force_format":false}})).unwrap();
    let p = AssignPlan {
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
        drive: MoveLocation {
            address: 1,
            kind: MoveLocationKind::Drive,
            drive_serial: Some("SIMDRV0000".into()),
        },
        home_address: 1001,
        pool_revision: 1,
    };
    for field in ["home_address", "pool_revision", "drive", "captured_at"] {
        let mut raw = p.value();
        raw.as_object_mut().unwrap().remove(field);
        assert!(AssignPlan::parse(raw).is_err());
    }
    let mut bad = p.clone();
    bad.source.kind = MoveLocationKind::ImportExport;
    bad.source.address = 101;
    assert!(bad.validate().is_err());
    let queued = c.admit_management(&serial, route, owner, &r).await.unwrap();
    assert!(queued.as_object().unwrap().contains_key("execution"));
    let running = c
        .prepare_media_assign(&serial, route, owner, &r, &p)
        .await
        .unwrap();
    assert_eq!(running["execution"]["plan"], p.value());
    assert!(
        c.tape_assignment(&serial, "TEST01L8")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        c.require_active_library_readable(&serial, route, owner)
            .await
            .is_err()
    );
    assert!(
        c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Threading)
            .await
            .is_err(),
        "must not skip MOVE checkpoint for a storage source"
    );
    assert!(
        c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Formatting)
            .await
            .is_err()
    );
    c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Moving)
        .await
        .unwrap();
    c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Threading)
        .await
        .unwrap();
    c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Inspecting)
        .await
        .unwrap();
    c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Formatting)
        .await
        .unwrap();
    c.defer_media_assign(
        &serial,
        route,
        owner,
        &r,
        "FORMAT结果未知，不换卷UUID，不重发FORMAT",
    )
    .await
    .unwrap();
    assert!(
        c.reject_media_assign(&serial, route, owner, &r, "不能当作尚未格式化清理")
            .await
            .is_err()
    );
    assert!(
        c.cancel_management(&serial, route, owner, r.epoch(), r.operation())
            .await
            .is_err()
    );
    let mut competing = r.value().clone();
    competing["operation"] = json!(Uuid::new_v4().to_string());
    let competing = ManagementRequest::parse(competing).unwrap();
    c.admit_management(&serial, route, owner, &competing)
        .await
        .unwrap();
    assert!(
        c.prepare_media_assign(&serial, route, owner, &competing, &p)
            .await
            .is_err(),
        "another task cannot claim the original volume UUID"
    );
    c.cancel_management(
        &serial,
        route,
        owner,
        competing.epoch(),
        competing.operation(),
    )
    .await
    .unwrap();
    let next = owner + 1;
    c.claim_active_owner_epoch(&serial, next).await.unwrap();
    assert!(
        c.stage_media_assign(&serial, route, owner, &r, AssignPhase::Formatting)
            .await
            .is_err()
    );
    let adopted = c
        .prepare_media_assign(&serial, route, next, &r, &p)
        .await
        .unwrap();
    assert_eq!(adopted["request"], *r.value());
    assert_eq!(adopted["execution"]["phase"], "formatting");
    let mut changed = p.clone();
    changed.drive.drive_serial = Some("OTHER".into());
    assert!(
        c.prepare_media_assign(&serial, route, next, &r, &changed)
            .await
            .is_err()
    );
    let at = crate::ltfs::label::ltfs_time_now();
    let inv = json!({"num_of_slots":8,"num_of_ieslots":1,"scsi_vendor_id":"SIM","scsi_product_id":"SIM-CHANGER","scsi_firmware_revision":"0001","library_serial":serial,"owner":2,"round":next,"observed_at":at,"volumes":[{"barcode":"TEST01L8","address":1,"element_type":4,"medium_type":1,"source_address":1001,"home_slot":null}],"drives":[{"serial":"SIMDRV0000","address":1,"barcode":"TEST01L8"}]});
    c.record_active_library_inventory(&serial, route, next, &inv)
        .await
        .unwrap();
    c.record_library_monitor(&serial, route, next, &at, None)
        .await
        .unwrap();
    let index = ActiveIndexIdentity {
        volume_uuid: volume,
        generation: 1,
        location: crate::ltfs::index::IndexLocation {
            partition: crate::ltfs::label::PART_DATA,
            start_block: crate::ltfs::volume::FIRST_INDEX_BLOCK,
        },
        sha256: [7; 32],
        incremental: false,
    };
    let source = NativeMediaSource {
        identity: crate::ltfs::recovery::IndexSourceIdentity::new(
            crate::ltfs::volume::CommittedIndexConstruct {
                volume_uuid: index.volume_uuid,
                generation: index.generation,
                location: index.location,
                sha256: index.sha256,
                incremental: false,
            },
            Vec::new(),
        )
        .unwrap(),
        files: 0,
        paths: 0,
    };
    let done = c
        .complete_media_assign(&serial, route, next, &r, &source, &at)
        .await
        .unwrap();
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["result"]["index_imported"], true);
    let assignment = c
        .tape_assignment(&serial, "TEST01L8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(assignment.volume_id, Some(volume));
    assert_eq!(assignment.pool_id, pool.id);
    assert_eq!(
        c.volume_placement(volume).await.unwrap().unwrap().barcode,
        "TEST01L8"
    );
    c.require_active_library_readable(&serial, route, next)
        .await
        .unwrap();
    assert!(
        c.active_loaded_volume_head(&serial, "TEST01L8", route, next)
            .await
            .unwrap()
            .is_some(),
        "assignment publishes the verified empty source with readiness atomically"
    );
    assert_eq!(
        c.complete_media_assign(&serial, route, next, &r, &source, &at)
            .await
            .unwrap(),
        done
    );
    assert_eq!(
        c.admit_management(&serial, route, next, &r).await.unwrap(),
        done
    );
    assert!(
        c.active_library_mechanical_task(&serial, route, next)
            .await
            .unwrap()
            .is_none()
    );
}
