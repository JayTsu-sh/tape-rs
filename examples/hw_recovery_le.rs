//! 故障恢复后 LE 回写的隔离 Client 只读探针。
use base64::Engine;
use std::time::Duration;
use tape_rs::client::{Client, ClientError};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_RECOVERY_LE_BARCODE")?, "RT2502L8");
    let endpoints = std::env::var("TAPE_RS_RECOVERY_LE_ENDPOINTS")?;
    let mut c = Client::new(endpoints.split(',').map(str::to_owned));
    c.retry_for = Duration::from_secs(30);
    let p = "rc/post-recovery-le-20260925.bin";
    let mut expected: Vec<u8> = (0..65536).map(|i| i as u8).collect();
    expected.extend_from_slice(b"LE after barrier VCI recovery\n");
    let s = c.stat(p)?.expect("LE 新文件存在");
    assert_eq!(s.barcode, "RT2502L8");
    assert_eq!(s.generation, 85);
    assert_eq!(s.length, expected.len() as u64);
    assert_eq!(s.version, (0, 0));
    assert_eq!(c.stat_path(p)?.unwrap().state, "committed");
    assert_eq!(c.get(p)?, expected);
    let mut out = Vec::new();
    assert_eq!(c.get_to(p, &mut out)?, expected.len() as u64);
    assert_eq!(out, expected);
    assert_eq!(c.get_range(p, 255, 4097)?, expected[255..4352]);
    assert_eq!(c.get_range(p, 65560, 100)?, expected[65560..]);
    assert!(c.get_range(p, 0, 0)?.is_empty());
    assert!(matches!(
        c.get_range(p, 999999, 1),
        Err(ClientError::Rejected { status: 416, .. })
    ));
    let attrs = s.metadata["xattrs"].as_array().unwrap();
    for (name, bytes) in [
        ("le.binary", b"\x00\xffLE-after-recovery".as_slice()),
        ("le.empty", b"".as_slice()),
    ] {
        let x = attrs.iter().find(|x| x["key"] == name).unwrap();
        let value = x["value"].as_str().unwrap();
        let actual = if x["base64"] == true {
            base64::engine::general_purpose::STANDARD.decode(value)?
        } else {
            value.as_bytes().to_vec()
        };
        assert_eq!(actual, bytes);
    }
    assert!(
        c.list()?
            .iter()
            .any(|(name, len)| name.trim_start_matches('/') == p && *len == s.length)
    );
    assert!(
        c.list_dir("rc", false)?
            .iter()
            .any(|e| e.name == "post-recovery-le-20260925.bin")
    );
    assert_eq!(
        c.get("rc/xattr-fault-20260925")?,
        b"xattr fault unchanged data"
    );
    println!(
        "PASS Client gen85 stat/stat_path/get/get_to/get_range/list/list_dir/xattrs/version: {s:?}"
    );
    Ok(())
}
