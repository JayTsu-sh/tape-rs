//! Opt-in physical changer reads. Never MOVE, INITIALIZE, LOAD, PR OUT or write.
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Mutex;
use tape_rs::{
    changer::commands::MediumChanger,
    error::{Result, TapeError},
    scsi::{
        cdb,
        device::{ScsiDevice, ScsiResult},
        inquiry::{fc_target_port, normalize_fc_target_port},
        reservation::{ReservationKey, read_status, verify_holder},
        transport::TapeTransport,
    },
};

#[derive(Clone, Copy)]
enum ReplyScope {
    Header,
    OneElement,
    Full,
}
struct ReadProbe<'a> {
    device: &'a ScsiDevice,
    key: ReservationKey,
    scope: ReplyScope,
    replies: Mutex<Vec<Value>>,
}
impl TapeTransport for ReadProbe<'_> {
    fn identity(&self) -> &str {
        self.device.identity()
    }
    fn execute_no_data(&self, _: &[u8], _: u32) -> Result<ScsiResult> {
        Err(TapeError::Refused {
            reason: "只读库存探针禁止无数据命令".into(),
        })
    }
    fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> Result<ScsiResult> {
        Err(TapeError::Refused {
            reason: "只读库存探针禁止写命令".into(),
        })
    }
    fn execute_read(&self, command: &[u8], data: &mut [u8], timeout: u32) -> Result<ScsiResult> {
        if !matches!(command.first(), Some(0x5a | 0xb8)) {
            return Err(TapeError::Refused {
                reason: "库存探针仅允许MODE SENSE和READ ELEMENT STATUS".into(),
            });
        }
        verify_holder(self.device, self.key)?;
        let mut actual = command.to_vec();
        let result = if command[0] == 0xb8 {
            assert_eq!(actual.len(), 12);
            match self.scope {
                ReplyScope::Header => {
                    actual[7..10].copy_from_slice(&[0, 0, 8]);
                    let mut header = [0u8; 8];
                    let result = self.device.execute_read(&actual, &mut header, timeout)?;
                    assert!(result.transferred <= header.len() && data.len() >= header.len());
                    data[..result.transferred].copy_from_slice(&header[..result.transferred]);
                    result
                }
                ReplyScope::OneElement => {
                    assert!(u16::from_be_bytes([actual[4], actual[5]]) > 1);
                    actual[4..6].copy_from_slice(&1u16.to_be_bytes());
                    self.device.execute_read(&actual, data, timeout)?
                }
                ReplyScope::Full => self.device.execute_read(&actual, data, timeout)?,
            }
        } else {
            self.device.execute_read(&actual, data, timeout)?
        };
        verify_holder(self.device, self.key)?;
        if command[0] == 0xb8 {
            assert_eq!(result.status, 0);
            assert!(result.transferred >= 8);
            self.replies.lock().unwrap().push(json!({
                "cdb":actual,"status":result.status,"sense":format!("{:?}",result.sense),
                "transferred":result.transferred,"duration_ms":result.duration_ms,
                "reported_elements":u16::from_be_bytes([data[2],data[3]]),
                "reported_bytes":8+u32::from_be_bytes([0,data[5],data[6],data[7]]) as usize,
            }));
        }
        Ok(result)
    }
}

#[test]
#[ignore = "显式指定真实changer路径、序列号、FC主端口及当前PR键；只读三种状态响应，不改介质或预留"]
fn physical_changer_rejects_partial_inventory_and_accepts_complete_map() {
    let path = std::env::var("TAPE_RS_HW_CHANGER").expect("须指定当前主路径changer");
    let serial = std::env::var("TAPE_RS_HW_CHANGER_SERIAL").expect("须指定真实序列号");
    let port =
        normalize_fc_target_port(&std::env::var("TAPE_RS_HW_CHANGER_PORT").expect("须指定主WWPN"))
            .unwrap();
    let key = std::env::var("TAPE_RS_HW_CHANGER_PR_KEY").expect("须指定当前预留键");
    let key = ReservationKey(u64::from_str_radix(key.trim_start_matches("0x"), 16).unwrap());
    assert!(key.is_ours() && key.node() > 0 && key.round() > 0);
    assert_eq!(
        fc_target_port(Path::new(&path)).unwrap(),
        Some(port.clone()),
        "先核端口再发任何CDB"
    );
    let device = ScsiDevice::open(&path).unwrap();
    let mut identity = [0u8; 252];
    let reply = device
        .execute_read(&cdb::inquiry_vpd(0x80, 252), &mut identity, 10_000)
        .unwrap();
    assert!(reply.transferred >= 4 && identity[1] == 0x80);
    let end = 4 + usize::from(u16::from_be_bytes([identity[2], identity[3]]));
    assert!(end > 4 && end <= reply.transferred);
    assert_eq!(
        String::from_utf8_lossy(&identity[4..end])
            .trim_matches(|c: char| c == '\0' || c.is_whitespace()),
        serial
    );
    verify_holder(&device, key).unwrap();
    let before = read_status(&device).unwrap();
    let mut cases = Vec::new();
    for (label, scope) in [
        ("header_only", ReplyScope::Header),
        ("one_element", ReplyScope::OneElement),
        ("full", ReplyScope::Full),
    ] {
        let probe = ReadProbe {
            device: &device,
            key,
            scope,
            replies: Mutex::new(Vec::new()),
        };
        let mut changer = MediumChanger::new(&probe);
        let map = changer.load_address_map().unwrap().clone();
        let expected = usize::from(map.transport_count)
            + usize::from(map.storage_count)
            + usize::from(map.ie_count)
            + usize::from(map.dt_count);
        let result = changer.read_all_status();
        let (count, error) = match result {
            Ok(rows) => (Some(rows.len()), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let replies = probe.replies.lock().unwrap().clone();
        assert_eq!(replies.len(), 1);
        match scope {
            ReplyScope::Header => {
                assert!(error.is_some());
                assert_eq!(replies[0]["transferred"], 8);
                assert!(replies[0]["reported_bytes"].as_u64().unwrap() > 8);
            }
            ReplyScope::OneElement => {
                assert!(error.is_some());
                assert_eq!(replies[0]["reported_elements"], 1);
            }
            ReplyScope::Full => {
                assert_eq!(count, Some(expected));
                assert!(error.is_none());
            }
        }
        cases.push(json!({"scope":label,"expected_elements":expected,"returned_count":count,"core_error":error,"replies":replies}));
    }
    verify_holder(&device, key).unwrap();
    let after = read_status(&device).unwrap();
    assert_eq!(before.generation, after.generation);
    assert_eq!(before.keys, after.keys);
    assert_eq!(before.holder, after.holder);
    println!(
        "LIVE_CHANGER_STATUS_READ_AUDIT {}",
        json!({"device":path,"serial":serial,"port":port,"pr_key":format!("{:#018x}",key.0),"cases":cases,"pr_generation_before_after":after.generation,"no_media_or_PR_OUT_commands":true})
    );
}
