//! Holo RT2502L8 隔离副本：保留旧 Leader 缓存验证只读故障切换。
use std::time::{Duration, Instant};
use tape_rs::client::Client;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(std::env::var("TAPE_RS_READ_BARCODE")?, "RT2502L8");
    let endpoints = std::env::var("TAPE_RS_READ_ENDPOINTS")?;
    let gate = std::env::var("TAPE_RS_READ_GATE")?;
    assert!(gate.starts_with("/home/rocky/tape-rs-read-ha-20260924/"));
    let mut clients: Vec<_> = (0..3).map(|_| Client::new(endpoints.split(','))).collect();
    for c in &mut clients {
        c.retry_for = Duration::from_secs(60);
        c.io_timeout = Duration::from_secs(45);
        assert_eq!(c.stat("rc/ha-upload.bin")?.unwrap().barcode, "RT2502L8");
    }
    let expected: Vec<u8> = (0..4 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    for c in &mut clients {
        assert_eq!(c.get("rc/ha-upload.bin")?, expected);
    }
    std::fs::write(format!("{gate}.ready"), b"cached old leader")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !std::path::Path::new(&format!("{gate}.go")).exists() {
        assert!(Instant::now() < deadline, "等待故障窗口超时");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut output = Vec::new();
    let n = clients[0].get_to("rc/ha-upload.bin", &mut output)?;
    assert_eq!(n, expected.len() as u64);
    assert_eq!(output, expected);
    println!("PASS get_to: 4 MiB完整且无重复拼接");
    assert_eq!(clients[1].get("rc/ha-upload.bin")?, expected);
    assert_eq!(
        clients[2].get_range("rc/ha-upload.bin", 12345, 65536)?,
        expected[12345..12345 + 65536]
    );
    println!("PASS get/get_range: 保留旧缓存后自动切换");
    Ok(())
}
