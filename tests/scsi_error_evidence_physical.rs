//! 显式实带测试：指定空闲、当前 owner 主机上的设备、序列号和原 PR key。
//! 仅发送 INQUIRY / PR IN；不装卸、不定位、不读正文、不采消费性日志。
use std::path::Path;
use tape_rs::error::{Result, TapeError};
use tape_rs::scsi::{
    cdb,
    device::{ScsiDevice, ScsiResult},
    inquiry::read_unit_serial,
    reservation,
    transport::TapeTransport,
};

struct ReadOnlyProbe(ScsiDevice);
impl TapeTransport for ReadOnlyProbe {
    fn identity(&self) -> &str {
        self.0.path()
    }
    fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
        panic!("只读诊断不允许无数据命令")
    }
    fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
        panic!("只读诊断不允许写入")
    }
    fn execute_read(&self, cdb: &[u8], out: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        assert!(matches!(cdb.first(), Some(0x12 | 0x5e)));
        self.0.execute_read(cdb, out, timeout)
    }
}

#[test]
#[ignore = "需要明确指定真实设备/SN/原owner PR key，且由现场协调器确认无并发设备任务"]
fn unsupported_vpd_preserves_real_check_condition_without_media_changes() {
    let _ = env_logger::try_init();
    let path = std::env::var("TAPE_RS_SCSI_EVIDENCE_DEVICE").expect("须显式指定设备");
    let serial = std::env::var("TAPE_RS_SCSI_EVIDENCE_SERIAL").expect("须显式指定序列号");
    let key = std::env::var("TAPE_RS_SCSI_EVIDENCE_OWNER_KEY").expect("须显式指定原owner PR key");
    let key = u64::from_str_radix(key.trim_start_matches("0x"), 16).unwrap();
    assert!(!serial.is_empty() && key != 0);
    let node = Path::new(&path);
    assert_eq!(node.parent(), Some(Path::new("/dev")));
    let name = node.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("sg") && name[2..].bytes().all(|b| b.is_ascii_digit()));
    // Verify the target before opening it, so a renumbered path is not probed.
    let vpd = std::fs::read(format!("/sys/class/scsi_generic/{name}/device/vpd_pg80")).unwrap();
    assert!(vpd.len() >= 4);
    let end = 4 + usize::from(u16::from_be_bytes([vpd[2], vpd[3]]));
    assert!(end <= vpd.len());
    assert_eq!(
        std::str::from_utf8(&vpd[4..end])
            .unwrap()
            .trim_matches([' ', '\0']),
        serial
    );
    let device = ReadOnlyProbe(ScsiDevice::open(&path).unwrap());
    assert_eq!(read_unit_serial(&device).as_deref(), Some(serial.as_str()));
    let before = reservation::read_status(&device).unwrap();
    assert_eq!(
        before.holder,
        Some((key, reservation::PR_TYPE_EXCLUSIVE_ACCESS))
    );

    let mut supported = [0u8; 252];
    let response = device
        .execute_read(&cdb::inquiry_vpd(0, 252), &mut supported, 10_000)
        .unwrap();
    assert!(response.transferred >= 4 && response.transferred <= supported.len());
    assert_eq!(supported[1], 0);
    let end = 4 + usize::from(u16::from_be_bytes([supported[2], supported[3]]));
    assert!(end <= response.transferred);
    let page = (1..=0x7f)
        .rev()
        .find(|p| !supported[4..end].contains(p))
        .unwrap();
    let command = cdb::inquiry_vpd(page, 252);
    let error = device
        .execute_read(&command, &mut supported, 10_000)
        .unwrap_err();
    assert!(matches!(
        &error,
        TapeError::ScsiCommand {
            status: 2,
            sense_key: 5,
            ..
        }
    ));
    let sense = error
        .sense_info()
        .expect("真实 CHECK CONDITION 必须保留原始 sense");
    assert_eq!(sense.raw_bytes().len(), sense.received_len());
    assert_eq!(sense.current_condition().map(|v| v.0), Some(5));
    assert!(matches!(sense.response_code, 0x70 | 0x72));
    let original_raw = sense.raw_bytes().to_vec();
    let result = serde_json::json!({
        "device":path,"serial":serial,"cdb":command,"response_code":sense.response_code,
        "sense_raw":sense.raw_bytes(),"received_len":sense.received_len(),
        "effective_len":sense.effective_len(),"sense_key":sense.sense_key,
        "asc":sense.asc,"ascq":sense.ascq,"error":error.to_string(),
        "pr_generation":before.generation,"owner_key":format!("{key:#018x}")
    });
    let wrapped = TapeError::CommitFailed {
        stage: "只读诊断包装验证".into(),
        source: Box::new(error),
    };
    assert_eq!(wrapped.sense_info().unwrap().raw_bytes(), original_raw);
    assert_eq!(reservation::read_status(&device).unwrap(), before);
    assert_eq!(read_unit_serial(&device).as_deref(), Some(serial.as_str()));
    println!("PHYSICAL_SCSI_EVIDENCE {result}");
}
