//! CLI wiring only; loopback HTTP, no FDB or hardware access.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;

#[test]
fn native_barrier_cli_requires_single_explicit_scope_and_prints_local_proof() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let challenge = uuid::Uuid::new_v4().to_string();
    for extra in [
        vec![],
        vec!["--library", "B", "--pool", "P"],
        vec!["--library", "B", "--endpoint", &address],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
            .args(["--endpoint", &address])
            .args(extra)
            .args(["library", "native-barrier", "--challenge", &challenge])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    listener.set_nonblocking(false).unwrap();
    let endpoint = address.clone();
    let expected = challenge.clone();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            line,
            format!(
                "GET /libraries/B/admin/catalog/current/native-barrier?challenge={expected} HTTP/1.1\r\n"
            )
        );
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
        }
        let body=serde_json::json!({"version":1,"node_id":2,"library_serial":"B","challenge":expected,"members":{"2":endpoint}}).to_string();
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_ltfsctl"))
        .args([
            "--endpoint",
            &address,
            "--library",
            "B",
            "library",
            "native-barrier",
            "--challenge",
            &challenge,
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["node_id"], 2);
    assert_eq!(value["library_serial"], "B");
    assert_eq!(value["challenge"], challenge);
}
