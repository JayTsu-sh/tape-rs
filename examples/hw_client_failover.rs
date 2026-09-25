//! Holo 专用 RT2502L8 副本故障验证；显式环境变量门控。
use std::time::Duration;
use tape_rs::client::{Client, ClientError};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_HA_BARCODE")?, "RT2502L8");
    let direct = std::env::var("TAPE_RS_HA_DIRECT")?;
    let endpoints = std::env::var("TAPE_RS_HA_ENDPOINTS")?;
    let mut check = Client::new(direct.split(','));
    check.retry_for = Duration::from_secs(90);
    assert_eq!(check.stat("rc/keep.bin")?.unwrap().barcode, "RT2502L8");
    let mut c = Client::new(endpoints.split(','));
    c.retry_for = Duration::from_secs(90);
    c.io_timeout = Duration::from_secs(120);
    let mode = std::env::args()
        .nth(1)
        .expect("prepare/upload/delete/verify");
    let data: Vec<u8> = (0..4 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    match mode.as_str() {
        "prepare" => {
            assert!(
                check
                    .put_new("rc/ha-delete.bin", b"before delete")?
                    .committed
            );
        }
        "upload" => {
            let result = c.put_new("rc/ha-upload.bin", &data)?;
            println!(
                "UPLOAD committed={} attempts={} resolved_by_query={}",
                result.committed, result.attempts, result.resolved_by_query
            );
            assert!(result.committed);
            assert_eq!(check.get("rc/ha-upload.bin")?, data);
            println!("PASS interrupted upload readback");
        }
        "delete" => {
            let result = c.delete("rc/ha-delete.bin");
            println!("DELETE {result:?}");
            assert!(matches!(result, Err(ClientError::Indeterminate(_))));
            assert!(check.stat("rc/ha-delete.bin")?.is_none());
            assert!(
                check
                    .put_new("rc/ha-delete.bin", b"new version after uncertain delete")?
                    .committed
            );
            std::thread::sleep(Duration::from_secs(2));
            assert_eq!(
                check.get("rc/ha-delete.bin")?,
                b"new version after uncertain delete"
            );
            println!("PASS uncertain delete not replayed; newer version preserved");
        }
        "verify" => {
            assert_eq!(check.get("rc/ha-upload.bin")?, data);
            assert_eq!(
                check.get("rc/ha-delete.bin")?,
                b"new version after uncertain delete"
            );
            let metadata = check.stat("rc/edit.bin")?.unwrap().metadata;
            assert!(metadata["xattrs"].as_array().unwrap().iter().any(|x| x["key"] == "roundtrip.binary" && x["value"] == "AP9MRS1yb3VuZHRyaXA="));
            println!("PASS final acknowledged data and binary xattr");
        }
        _ => panic!("unknown mode"),
    }
    Ok(())
}
