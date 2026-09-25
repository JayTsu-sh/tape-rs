//! 保留 Holo 副本上的 rename 验收，显式端点和条码门控。
use std::time::Duration;
use tape_rs::client::{Client, ClientError};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_RENAME_BARCODE")?, "RT2502L8");
    let endpoints = std::env::var("TAPE_RS_RENAME_ENDPOINTS")?;
    let mut c = Client::new(endpoints.split(',').map(str::to_owned));
    c.retry_for = Duration::from_secs(30);
    assert_eq!(c.stat("rc/keep.bin")?.unwrap().barcode, "RT2502L8");
    let p = "rc/rename-interop-20260924";
    match std::env::args().nth(1).as_deref() {
        Some("prepare") => {
            assert!(c.stat(p)?.is_none());
            c.mkdir(p)?;
            c.put_new(&format!("{p}/client-source"), b"client rename")?;
            c.rename(
                &format!("{p}/client-source"),
                &format!("{p}/client-target"),
                true,
            )?;
            assert!(c.stat(&format!("{p}/client-source"))?.is_none());
            assert_eq!(c.get(&format!("{p}/client-target"))?, b"client rename");
            c.mkdir(&format!("{p}/lost-source"))?;
            c.mkdir(&format!("{p}/lost-source/empty"))?;
            c.put_new(&format!("{p}/lost-source/file"), b"atomic lost response")?;
        }
        Some("lost") => {
            let result = c.rename(
                &format!("{p}/lost-source"),
                &format!("{p}/lost-target"),
                false,
            );
            assert!(
                matches!(result, Err(ClientError::Indeterminate(_))),
                "{result:?}"
            );
            println!("PASS 响应丢失返回 Indeterminate，没有重放改名");
        }
        Some("verify") => {
            assert!(c.stat(&format!("{p}/lost-source"))?.is_none());
            assert!(
                c.list_dir(&format!("{p}/lost-target/empty"), false)?
                    .is_empty()
            );
            assert_eq!(
                c.get(&format!("{p}/lost-target/file"))?,
                b"atomic lost response"
            );
        }
        _ => return Err("用法：prepare/lost/verify".into()),
    }
    println!("PASS Client rename 验收");
    Ok(())
}
