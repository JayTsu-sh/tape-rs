//! 隔离 Holo 属性故障验收：仅 RT2502L8、显式端点门控。
use base64::Engine;
use std::time::Duration;
use tape_rs::client::{Client, ClientError};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_XATTR_FAULT_BARCODE")?, "RT2502L8");
    let endpoints = std::env::var("TAPE_RS_XATTR_FAULT_ENDPOINTS")?;
    let mut c = Client::new(endpoints.split(',').map(str::to_owned));
    c.retry_for = Duration::from_secs(30);
    assert_eq!(c.stat("rc/keep.bin")?.unwrap().barcode, "RT2502L8");
    let p = "rc/xattr-fault-20260925";
    let mode = std::env::args().nth(1).ok_or("缺少模式")?;
    match mode.as_str() {
        "prepare" => {
            c.put_new(p, b"xattr fault unchanged data")?;
            c.setxattr(p, "user.cut", b"old", 1)?;
            c.setxattr(p, "user.delete", b"old", 1)?;
        }
        "cut" | "lost-set" | "lost-delete" | "barrier-cut" | "vci-cut" => {
            let result = match mode.as_str() {
                "cut" => c.setxattr(p, "user.cut", b"uncommitted", 2),
                "barrier-cut" => c.setxattr(p, "user.barrier", b"\x00\xffbarrier", 1),
                "vci-cut" => c.setxattr(p, "user.vci", b"\x00\xffvci", 1),
                "lost-set" => c.setxattr(p, "user.set", b"\x00\xffcommitted", 1),
                _ => c.removexattr(p, "user.delete"),
            };
            assert!(
                matches!(result, Err(ClientError::Indeterminate(_))),
                "{result:?}"
            );
            println!("PASS {mode}: Indeterminate");
        }
        "verify-cut" | "verify-set" | "verify-delete" => {
            assert_eq!(c.get(p)?, b"xattr fault unchanged data");
            let s = c.stat(p)?.unwrap();
            let attrs = s.metadata["xattrs"].as_array().unwrap();
            let attr = |key: &str| {
                attrs.iter().find(|x| x["key"] == key).map(|x| {
                    let v = x["value"].as_str().unwrap();
                    if x["base64"].as_bool().unwrap_or(false) {
                        base64::engine::general_purpose::STANDARD.decode(v).unwrap()
                    } else {
                        v.as_bytes().to_vec()
                    }
                })
            };
            assert_eq!(attr("cut"), Some(b"old".to_vec()));
            if mode != "verify-cut" {
                assert_eq!(attr("set"), Some(b"\x00\xffcommitted".to_vec()));
            }
            if mode == "verify-delete" {
                assert_eq!(attr("delete"), None);
            } else {
                assert_eq!(attr("delete"), Some(b"old".to_vec()));
            }
            println!(
                "PASS {mode}: generation {} version {:?}",
                s.generation, s.version
            );
        }
        _ => return Err("未知模式".into()),
    }
    Ok(())
}
