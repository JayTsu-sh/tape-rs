//! Exercise the public CLI against loopback HTTP; no tape or FDB access.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;

fn query(args: &[&str], expected_path: &str, status: u16, body: &str) -> std::process::Output {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let expected = expected_path.to_owned();
    let body = body.to_owned();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let matched = line.split_whitespace().nth(1) == Some(expected.as_str());
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header.is_empty() {
                break;
            }
        }
        let (code, body) = if matched {
            (status, body.as_str())
        } else {
            (404, "{}")
        };
        write!(
            stream,
            "HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            &address,
            "-l",
            "B",
            "-p",
            "11111111-1111-4111-8111-111111111111",
        ])
        .args(args)
        .output()
        .unwrap();
    server.join().unwrap();
    output
}

#[test]
fn public_stat_reads_fdb_and_missing_paths_fail() {
    let route = "/v1/fdb/stat/B?path=L2ZpbGU&pool=11111111-1111-4111-8111-111111111111";
    let body = r#"{"library_serial":"B","namespace":"22222222-2222-4222-8222-222222222222","pool_id":"11111111-1111-4111-8111-111111111111","kind":"file","id":"33333333-3333-4333-8333-333333333333","version":3,"size":4,"generation":2,"barcode":"TESTL8","volume_id":"44444444-4444-4444-8444-444444444444","digest":null}"#;
    let out = query(&["stat", "/file"], route, 200, body);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("TESTL8"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let missing = query(&["stat", "/file"], route, 404, "{}");
    assert!(
        !missing.status.success(),
        "missing stat must return failure"
    );
}

#[test]
fn failed_public_get_does_not_create_or_truncate_output() {
    let root = std::env::temp_dir().join(format!("acceptance-get-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let output = root.join("out");
    let out = query(
        &["get", "/file", output.to_str().unwrap()],
        "/unused",
        404,
        "{}",
    );
    assert!(!out.status.success());
    assert!(!output.exists(), "failed GET published an empty output");
    std::fs::write(&output, b"keep original").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "-e",
            "127.0.0.1:1",
            "-l",
            "B",
            "-p",
            "11111111-1111-4111-8111-111111111111",
            "get",
            "/file",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&output).unwrap(), b"keep original");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cluster_uses_node_discovery_and_errors_are_not_null_success() {
    let out = query(&["cluster"], "/cluster", 200, r#"{"id":2,"libraries":[]}"#);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"id\":2"));
    let failed = query(&["cluster"], "/cluster", 500, r#"{"error":"unavailable"}"#);
    assert!(
        !failed.status.success(),
        "cluster HTTP failure must fail CLI"
    );
}
