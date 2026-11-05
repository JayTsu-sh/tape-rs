//! 显式实物观测验证；默认测试不打开设备。
use super::*;
use crate::scsi::{device::ScsiResult, inquiry::read_unit_serial};
use std::sync::Mutex;

struct ReadOnlyDevice {
    device: ScsiDevice,
    commands: Arc<Mutex<Vec<Value>>>,
}
impl TapeTransport for ReadOnlyDevice {
    fn identity(&self) -> &str {
        self.device.path()
    }
    fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
        panic!("库存实测不允许机械或定位命令")
    }
    fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
        panic!("库存实测不允许写入")
    }
    fn execute_read(&self, cdb: &[u8], out: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        assert!(matches!(cdb.first(), Some(0x12 | 0x5e | 0x5a | 0xb8)));
        self.commands
            .lock()
            .unwrap()
            .push(json!({"device":self.device.path(),"cdb":cdb}));
        self.device.execute_read(cdb, out, timeout)
    }
}

#[test]
#[ignore = "需要现场确认当前owner、原PR、空闲设备队列及显式B设备配置；仅只读CDB"]
fn real_inventory_observation_uses_one_element_status_reply() {
    let _ = env_logger::try_init();
    let config = std::env::var("TAPE_RS_LIBRARY_OBSERVATION_CONFIG").expect("须显式提供现场配置");
    let config: Value = serde_json::from_str(&config).unwrap();
    let serial = config["library"].as_str().unwrap();
    let node = u8::try_from(config["node"].as_u64().unwrap()).unwrap();
    let round = config["round"].as_u64().unwrap();
    assert!(node > 0 && round > 0 && round < 1 << 48);
    let rows = config["devices"].as_array().unwrap();
    assert!(rows.len() >= 2 && rows.len() <= 32);
    let mut paths = std::collections::BTreeSet::new();
    let mut serials = std::collections::BTreeSet::new();
    // Validate every explicit target before opening any device.
    for row in rows {
        let path = row["path"].as_str().unwrap();
        let expected = row["serial"].as_str().unwrap();
        assert!(paths.insert(path) && serials.insert(expected) && !expected.is_empty());
        assert!(matches!(row["kind"].as_str(), Some("changer" | "drive")));
        let name = path.strip_prefix("/dev/sg").unwrap();
        assert!(!name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()));
        let raw =
            std::fs::read(format!("/sys/class/scsi_generic/sg{name}/device/vpd_pg80")).unwrap();
        assert!(raw.len() >= 4);
        let end = 4 + usize::from(u16::from_be_bytes([raw[2], raw[3]]));
        assert!(end <= raw.len());
        assert_eq!(
            std::str::from_utf8(&raw[4..end])
                .unwrap()
                .trim_matches([' ', '\0']),
            expected
        );
    }
    let changers: Vec<_> = rows.iter().filter(|r| r["kind"] == "changer").collect();
    assert_eq!(changers.len(), 1);
    assert_eq!(changers[0]["serial"], serial);
    let commands = Arc::new(Mutex::new(Vec::new()));
    let devices: Vec<_> = rows
        .iter()
        .map(|row| ManagedDevice {
            name: row["path"].as_str().unwrap().into(),
            serial: row["serial"].as_str().unwrap().into(),
            kind: if row["kind"] == "changer" {
                DeviceKind::Changer
            } else {
                DeviceKind::Drive
            },
            dev: Box::new(ReadOnlyDevice {
                device: ScsiDevice::open(row["path"].as_str().unwrap()).unwrap(),
                commands: commands.clone(),
            }),
        })
        .collect();
    let key = reservation::ReservationKey::new(node, round).0;
    let mut before = Vec::new();
    for device in &devices {
        assert_eq!(
            read_unit_serial(device.dev.as_ref()).as_deref(),
            Some(device.serial.as_str())
        );
        let status = reservation::read_status(device.dev.as_ref()).unwrap();
        assert_eq!(
            status.holder,
            Some((key, reservation::PR_TYPE_EXCLUSIVE_ACCESS))
        );
        before.push(status);
    }
    let observation = observe_devices(&devices, node, round).unwrap();
    assert_eq!(observation["library_serial"], serial);
    assert_eq!(observation["owner"], node);
    assert_eq!(observation["round"], round);
    let volumes = observation["volumes"].as_array().unwrap();
    let drives = observation["drives"].as_array().unwrap();
    assert_eq!(drives.len(), devices.len() - 1);
    let configured: std::collections::BTreeSet<_> = devices
        .iter()
        .filter(|d| d.kind == DeviceKind::Drive)
        .map(|d| d.serial.as_str())
        .collect();
    let observed: std::collections::BTreeSet<_> = drives
        .iter()
        .map(|d| d["serial"].as_str().unwrap())
        .collect();
    assert_eq!(observed, configured);
    assert_eq!(observed.len(), drives.len(), "驱动器身份不能重复");
    let addresses: std::collections::BTreeSet<_> = drives
        .iter()
        .map(|d| d["address"].as_u64().unwrap())
        .collect();
    assert_eq!(addresses.len(), drives.len(), "驱动器地址不能重复");
    for drive in drives {
        let loaded: Vec<_> = volumes
            .iter()
            .filter(|v| v["element_type"] == 4 && v["address"] == drive["address"])
            .collect();
        if drive["barcode"].is_null() {
            assert!(loaded.is_empty());
        } else {
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0]["barcode"], drive["barcode"]);
        }
    }
    let reads: Vec<_> = commands
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["cdb"][0] == 0xb8)
        .cloned()
        .collect();
    assert_eq!(reads.len(), 1, "一份观测不能混用两份库存响应");
    assert_eq!(reads[0]["device"], changers[0]["path"]);
    for (device, original) in devices.iter().zip(&before) {
        assert_eq!(
            reservation::read_status(device.dev.as_ref()).unwrap(),
            *original
        );
        assert_eq!(
            read_unit_serial(device.dev.as_ref()).as_deref(),
            Some(device.serial.as_str())
        );
    }
    println!(
        "PHYSICAL_LIBRARY_OBSERVATION {}",
        json!({"observation":observation,"commands":*commands.lock().unwrap(),"owner_key":format!("{key:#018x}"),"pr_unchanged":true})
    );
}
