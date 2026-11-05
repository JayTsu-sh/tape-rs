use super::*;
use crate::scsi::sim::{SimCartridge, SimLibrary};
use serde_json::json;

fn request(destination: &str) -> ManagementRequest {
    ManagementRequest::parse(json!({"version":1,"library":"LIB1","namespace":uuid::Uuid::new_v4().to_string(),"epoch":1,"operation":uuid::Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":destination}})).unwrap()
}
fn snapshot(lib: &SimLibrary) -> Snapshot {
    let devices = vec![
        ManagedDevice {
            name: "changer".into(),
            serial: "LIB1".into(),
            kind: DeviceKind::Changer,
            dev: Box::new(lib.changer()),
        },
        ManagedDevice {
            name: "drive".into(),
            serial: "SIMDRV0000".into(),
            kind: DeviceKind::Drive,
            dev: Box::new(lib.drive(0)),
        },
    ];
    let (map, rows) = management::elements(&devices).unwrap();
    let observation = management::observation_from_elements(&devices, 2, 92, &map, &rows).unwrap();
    Snapshot {
        map,
        rows,
        observation,
    }
}
#[test]
fn original_move_plan_rejects_inaccessible_disabled_exceptional_or_occupied_elements() {
    let lib = SimLibrary::new(1, 3, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    let r = request("ieslot");
    let original = snapshot(&lib);
    let plan = original.plan(&r, None).unwrap();
    assert_eq!(plan.source.address, original.map.storage_start);
    assert_eq!(plan.destination.address, original.map.ie_start);
    assert_eq!(plan.home_address, Some(original.map.storage_start));
    assert!(!original.reconcile(&plan).unwrap());
    let same_home = original.plan(&request("homeslot"), None).unwrap();
    assert_eq!(same_home.source, same_home.destination);
    assert!(original.reconcile(&same_home).unwrap());
    for address in [
        plan.source.address,
        plan.destination.address,
        original.map.transport_start,
    ] {
        for flag in 0..3 {
            let mut sample = snapshot(&lib);
            let element = sample
                .rows
                .iter_mut()
                .find(|e| e.address == address)
                .unwrap();
            match flag {
                0 => element.exception = true,
                1 => element.disabled = true,
                _ => element.accessible = Some(false),
            }
            if address == original.map.transport_start && flag == 2 {
                continue;
            } // no Access bit on transport
            assert!(
                sample.plan(&r, None).is_err(),
                "address={address} flag={flag}"
            );
            assert!(
                sample.reconcile(&plan).is_err(),
                "address={address} flag={flag}"
            );
        }
    }
    let changer = lib.changer();
    let mut mc = MediumChanger::new(&changer);
    mc.load_address_map().unwrap();
    mc.move_medium(plan.source.address, plan.destination.address)
        .unwrap();
    let at_ie = snapshot(&lib);
    assert!(at_ie.reconcile(&plan).unwrap());
    assert!(
        at_ie.plan(&request("homeslot"), None).is_err(),
        "不能从I/E凭空学习home"
    );
    let back = at_ie.plan(&request("homeslot"), Some(&plan)).unwrap();
    assert_eq!(back.destination, plan.source);
    let mut changed_map = plan.clone();
    changed_map.map.storage_count += 1;
    assert!(at_ie.reconcile(&changed_map).is_err());
    lib.insert_cartridge(SimCartridge::blank("OTHER01L8", 1 << 20), 0)
        .unwrap();
    assert!(
        snapshot(&lib).reconcile(&plan).is_err(),
        "原源位置被其他介质占用不能假成功"
    );
    assert!(
        snapshot(&lib)
            .plan(&request("homeslot"), Some(&plan))
            .is_err()
    );
}
#[test]
fn drive_access_and_home_follow_actual_simulated_unload_without_requiring_ltfs() {
    let lib = SimLibrary::new(1, 2, 1);
    lib.insert_cartridge(SimCartridge::blank("TEST01L8", 1 << 20), 0)
        .unwrap();
    let before = snapshot(&lib);
    let changer = lib.changer();
    let mut mc = MediumChanger::new(&changer);
    mc.load_address_map().unwrap();
    mc.move_medium(before.map.storage_start, before.map.dt_start)
        .unwrap();
    let threaded = snapshot(&lib);
    let drive = threaded.at(before.map.dt_start).unwrap();
    assert_eq!(drive.accessible, Some(false));
    assert_eq!(drive.source_address, Some(before.map.storage_start));
    let plan = threaded.plan(&request("homeslot"), None).unwrap();
    assert_eq!(plan.source.kind, MoveLocationKind::Drive);
    assert_eq!(plan.source.drive_serial.as_deref(), Some("SIMDRV0000"));
    assert!(!threaded.reconcile(&plan).unwrap());
    TapeDrive::new(&lib.drive(0)).unload().unwrap();
    let unthreaded = snapshot(&lib);
    assert_eq!(
        unthreaded.at(before.map.dt_start).unwrap().accessible,
        Some(true)
    );
    assert!(!unthreaded.reconcile(&plan).unwrap());
    mc.move_medium(plan.source.address, plan.destination.address)
        .unwrap();
    assert!(snapshot(&lib).reconcile(&plan).unwrap());
    assert_eq!(lib.loaded_barcode(0), None);
}
