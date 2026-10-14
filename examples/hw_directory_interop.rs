//! 保留 Holo 副本上的目录接口验收；不会自动格式化或归属介质。
use std::time::Duration;
use tape_rs::client::{Client, ClientError};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_DIRECTORY_BARCODE")?, "RT2502L8");
    let endpoints = std::env::var("TAPE_RS_DIRECTORY_ENDPOINTS")?;
    let mut c = Client::new(endpoints.split(',').map(str::to_owned));
    c.retry_for = Duration::from_secs(30);
    let before = c.get("rc/keep.bin")?;
    assert_eq!(c.stat("rc/keep.bin")?.unwrap().barcode, "RT2502L8");
    let root = "rc/dir-interop-20260924";
    match std::env::args().nth(1).as_deref() {
        Some("prepare") => {
            assert!(c.stat(root)?.is_none(), "测试路径必须不存在");
            c.mkdir(root)?;
            c.mkdir(&format!("{root}/client-empty"))?;
            c.mkdir(&format!("{root}/client-remove"))?;
            assert!(matches!(
                c.rmdir(root),
                Err(ClientError::Rejected { status: 409, .. })
            ));
            c.rmdir(&format!("{root}/client-remove"))?;
            assert!(c.stat(&format!("{root}/client-remove"))?.is_none());
            assert!(
                c.list_dir(&format!("{root}/client-empty"), false)?
                    .is_empty()
            );
        }
        Some("verify-le") => {
            assert!(c.stat(&format!("{root}/client-empty"))?.is_none());
            for name in ["fuse-empty", "nested", "nested/child", "le-empty"] {
                let s = c.stat(&format!("{root}/{name}"))?.expect("目录必须持久化");
                assert_eq!(s.metadata["kind"], "directory");
            }
            assert_eq!(
                c.get(&format!("{root}/le-file"))?,
                b"LE directory roundtrip\n"
            );
        }
        _ => return Err("用法：prepare 或 verify-le".into()),
    }
    assert_eq!(c.get("rc/keep.bin")?, before);
    println!("PASS Client 目录验收，原 keep.bin 未改变");
    Ok(())
}
