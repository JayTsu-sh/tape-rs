//! Holo 专用副本上的 Client 接口探针；显式环境变量门控。
use std::time::Duration;
use tape_rs::client::{Client, ClientError};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoints = std::env::var("TAPE_RS_INTEROP_ENDPOINTS")?;
    let mode = std::env::args().nth(1).unwrap_or_else(|| "read".into());
    let mut c = Client::new(endpoints.split(',').map(str::to_owned));
    c.retry_for = Duration::from_secs(30);
    let p = "rc/le-downgrade-20260924.bin";
    let mut expected: Vec<u8> = (0..65536).map(|i| i as u8).collect();
    expected.extend_from_slice(b"written by actual IBM LE 2.4.8.3\n");
    let s = c.stat(p)?.expect("LE 文件必须存在");
    assert_eq!(s.barcode, "LD2502L08", "只允许专用副本");
    assert_eq!(s.length, expected.len() as u64);
    assert_eq!(c.stat_path(p)?.unwrap().state, "committed");
    println!("STAT {s:?}");
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
    assert!(
        c.list()?
            .iter()
            .any(|(name, len)| name.trim_start_matches('/') == p && *len == s.length)
    );
    assert!(
        c.list_dir("rc", false)?
            .iter()
            .any(|e| e.name == "le-downgrade-20260924.bin")
    );
    assert!(c.stat("rc/interop-absent")?.is_none());
    assert!(matches!(
        c.get("rc/interop-absent"),
        Err(ClientError::NotFound)
    ));
    assert!(
        c.pools(None)?
            .iter()
            .any(|pool| pool.uuid == "0ba1b026-9595-4b86-9056-c54c7858e744")
    );
    println!("PASS Client stat/stat_path/get/get_to/get_range/list/list_dir/pools/404/416");
    if mode == "write" {
        assert_eq!(std::env::var("TAPE_RS_INTEROP_WRITE_BARCODE")?, "LD2502L08");
        let q = "rc/client-api-20260924.bin";
        assert!(c.stat(q)?.is_none());
        assert!(c.put_new(q, b"client first")?.committed);
        assert!(matches!(
            c.put_new(q, b"must not replace"),
            Err(ClientError::Exists(_))
        ));
        assert_eq!(c.get(q)?, b"client first");
        assert!(c.put(q, b"client replaced")?.committed);
        assert_eq!(c.get(q)?, b"client replaced");
        let local = std::env::temp_dir().join(format!("client-interop-{}.bin", std::process::id()));
        std::fs::write(&local, &expected)?;
        assert!(c.put_file(q, &local, false, true)?.committed);
        std::fs::remove_file(local)?;
        assert_eq!(c.get(q)?, expected);
        c.delete(q)?;
        assert!(c.stat(q)?.is_none());
        assert!(matches!(c.get(q), Err(ClientError::NotFound)));
        assert!(matches!(c.delete(q), Err(ClientError::NotFound)));
        assert_eq!(c.get(p)?, expected);
        println!("PASS Client put_new/Exists/put/put_file/delete，LE 文件仍完整");
    }
    Ok(())
}
