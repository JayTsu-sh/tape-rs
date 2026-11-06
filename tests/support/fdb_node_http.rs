use super::*;
use std::collections::BTreeMap;
use tape_rs::daemon::fdb_http;
use tape_rs::managed_catalog::{NodeMutation, NodeMutationRequest};

#[path = "fdb_node_attributes.rs"]
mod attributes;
#[path = "fdb_node_client.rs"]
mod client;
#[path = "fdb_fuse_readonly.rs"]
mod fuse_readonly;
#[path = "fdb_fuse_writing.rs"]
mod fuse_writing;
#[path = "fdb_node_journal.rs"]
mod node_journal;
#[path = "fdb_node_rename.rs"]
mod rename;
#[path = "fdb_upload_status.rs"]
mod upload_status;

fn original_condition(
    client: &tape_rs::client::fdb::FdbClient,
    library: &str,
    namespace: Uuid,
    pool: Uuid,
    path: &str,
    expected: Option<u64>,
) -> tape_rs::client::fdb::NodeCondition {
    use tape_rs::client::{FileVersion, fdb::NodeCondition};
    let parent_path = path
        .rsplit_once('/')
        .map(|(p, _)| if p.is_empty() { "/" } else { p })
        .unwrap();
    let parent = client.stat(library, pool, parent_path).unwrap().unwrap();
    assert_eq!(parent["namespace"], namespace.to_string());
    assert_eq!(parent["kind"], "directory");
    let parent = FileVersion::Fdb {
        namespace,
        inode: Uuid::parse_str(parent["id"].as_str().unwrap()).unwrap(),
        version: parent["version"].as_u64().unwrap(),
    };
    let node = expected.map(|version| {
        let node = client.stat(library, pool, path).unwrap().unwrap();
        assert_eq!(node["namespace"], namespace.to_string());
        FileVersion::Fdb {
            namespace,
            inode: Uuid::parse_str(node["id"].as_str().unwrap()).unwrap(),
            version,
        }
    });
    NodeCondition { parent, node }
}

fn base(f: &Fixture) -> String {
    format!("/v1/fdb/nodes/{}", f.serial)
}

fn send(
    server: &ReadHttpServer,
    f: &Fixture,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let (status, _, bytes) = server.request_body(method, path, Some(&format!(
        "x-tapers-fdb-namespace: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        f.route.route.namespace, body.len())), body);
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes))),
    )
}

fn prepare(
    server: &ReadHttpServer,
    f: &Fixture,
    pool: Uuid,
    path: &str,
    version: Option<u64>,
    mutation: NodeMutation,
) -> serde_json::Value {
    let input = json!({"version":1,"pool":pool.to_string(),"operation":Uuid::new_v4().to_string(),
        "path":path,"expected_version":version,"time":"2026-11-01T12:00:00.000000000Z","mutation":mutation});
    let (status, value) = send(
        server,
        f,
        "POST",
        &format!("{}/prepare", base(f)),
        input.to_string().as_bytes(),
    );
    assert_eq!(status, 200, "{value}");
    let request = NodeMutationRequest::decode(value["request"].to_string().as_bytes()).unwrap();
    assert_eq!(request.time(), input["time"].as_str().unwrap());
    assert_eq!(value["namespace"], f.route.route.namespace.to_string());
    assert_eq!(
        value["fingerprint"],
        request
            .fingerprint()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    value
}

fn route(f: &Fixture, prepared: &serde_json::Value) -> String {
    format!(
        "{}/requests/{}/{}",
        base(f),
        prepared["request"]["request_epoch"],
        prepared["request"]["operation"].as_str().unwrap()
    )
}

fn terminal(
    server: &ReadHttpServer,
    f: &Fixture,
    prepared: &serde_json::Value,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (code, value) = send(server, f, "GET", &route(f, prepared), &[]);
        assert_eq!(code, 200, "{value}");
        assert_eq!(value["fingerprint"], prepared["fingerprint"]);
        match value["status"].as_str() {
            Some("completed" | "rejected" | "cancelled") => return value,
            Some("queued" | "prepared" | "writing") => {}
            _ => panic!("unexpected terminal receipt: {value}"),
        }
        assert!(
            Instant::now() < deadline,
            "Node HTTP did not complete: {value}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn run(
    server: &ReadHttpServer,
    reader: &ReadHttpServer,
    f: &Fixture,
    prepared: &serde_json::Value,
) -> serde_json::Value {
    let (code, value) = send(
        server,
        f,
        "POST",
        &format!("{}/requests", base(f)),
        prepared["request"].to_string().as_bytes(),
    );
    assert!(matches!(code, 200 | 202), "{code}: {value}");
    let value = terminal(reader, f, prepared);
    assert_eq!(value["status"], "completed", "{value}");
    value
}

fn lose_admission_reply(server: &ReadHttpServer, f: &Fixture, prepared: &serde_json::Value) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let target = server.address.clone();
    let body = prepared["request"].to_string();
    let request = format!("POST {}/requests HTTP/1.1\r\nHost: test\r\nConnection: close\r\nx-tapers-fdb-namespace: {}\r\nContent-Length: {}\r\n\r\n{}",base(f),f.route.route.namespace,body.len(),body).into_bytes();
    let original = request.clone();
    let proxy = std::thread::spawn(move || {
        let (mut downstream, _) = listener.accept().unwrap();
        downstream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut received = vec![0; original.len()];
        downstream.read_exact(&mut received).unwrap();
        assert_eq!(received, original);
        let mut upstream = std::net::TcpStream::connect(target).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        upstream.write_all(&received).unwrap();
        let mut answer = Vec::new();
        upstream.read_to_end(&mut answer).unwrap();
        let code = std::str::from_utf8(&answer)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        assert!(matches!(code, "200" | "202"));
        // The actual owner has replied, but the caller receives no response.
        drop(downstream);
    });
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    client.write_all(&request).unwrap();
    let mut answer = Vec::new();
    client.read_to_end(&mut answer).unwrap();
    assert!(answer.is_empty());
    proxy.join().unwrap();
}

fn writer(f: &Fixture, owner: &Owner, round: u64) -> ReadHttpServer {
    ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(
            f.serial.clone(),
            super::active_pools::context(owner, 71, round),
        )]),
        HashMap::new(),
        Default::default(),
    ))
}

fn reader(f: &Fixture) -> ReadHttpServer {
    // No local owner context: retained results must be readable even after
    // permission loss, without using an accepted/old epoch as current owner.
    ReadHttpServer::start(fdb_http::owner_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::new(),
        HashMap::new(),
    ))
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；真实 HTTP 转发、自动 owner 与模拟介质，不访问真实设备"]
fn node_http_preserves_original_results_across_changes_recreation_and_takeover() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    eprintln!("node_http_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let entry = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(
            f.serial.clone(),
            super::active_pools::context(&owner, 72, 12),
        )]),
        HashMap::from([(71, server.address.clone())]),
        Default::default(),
    ));
    let directory = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir",
        None,
        NodeMutation::CreateDirectory,
    );
    let stale = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-stale",
        None,
        NodeMutation::CreateDirectory,
    );
    assert_eq!(send(&reader, &f, "GET", &route(&f, &directory), &[]).0, 404);
    assert_eq!(
        send(
            &reader,
            &f,
            "POST",
            &format!("{}/requests", base(&f)),
            directory["request"].to_string().as_bytes()
        )
        .0,
        405
    );
    lose_admission_reply(&entry, &f, &directory);
    let first = terminal(&reader, &f, &directory);
    assert_eq!(first["status"], "completed");
    assert_eq!(run(&entry, &reader, &f, &directory), first);
    assert_eq!(first["result"]["version"], 1);
    assert_eq!(first["result"]["deleted"], false);
    assert!(matches!(
        send(
            &entry,
            &f,
            "POST",
            &format!("{}/requests", base(&f)),
            stale["request"].to_string().as_bytes()
        )
        .0,
        200 | 202
    ));
    let rejected = terminal(&reader, &f, &stale);
    assert_eq!(rejected["status"], "rejected");
    assert!(!rejected["rejection_reason"].as_str().unwrap().is_empty());
    assert!(rejected["result"].is_null());
    let change = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir",
        Some(1),
        NodeMutation::SetXattr {
            attribute: tape_rs::ltfs::index::Xattr {
                key: "user.http".into(),
                value: "保留原结果".into(),
                base64: false,
            },
            mode: tape_rs::managed_catalog::XattrMode::Create,
        },
    );
    assert_eq!(run(&entry, &reader, &f, &change)["result"]["version"], 2);
    let link = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir/link",
        None,
        NodeMutation::CreateSymlink {
            target: "/file-0".into(),
        },
    );
    assert_eq!(run(&entry, &reader, &f, &link)["result"]["version"], 1);
    let xattr = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir/link",
        Some(1),
        NodeMutation::SetXattr {
            attribute: tape_rs::ltfs::index::Xattr {
                key: "user.link".into(),
                value: "eA==".into(),
                base64: true,
            },
            mode: tape_rs::managed_catalog::XattrMode::Upsert,
        },
    );
    assert_eq!(run(&entry, &reader, &f, &xattr)["result"]["version"], 2);
    let remove = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir/link",
        Some(2),
        NodeMutation::RemoveXattr {
            key: "user.link".into(),
        },
    );
    assert_eq!(run(&entry, &reader, &f, &remove)["result"]["version"], 3);
    let change = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir/link",
        Some(3),
        NodeMutation::ChangeMetadata {
            readonly: Some(true),
            access_time: None,
            modify_time: None,
        },
    );
    assert_eq!(run(&entry, &reader, &f, &change)["result"]["version"], 4);
    let unlink = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir/link",
        Some(4),
        NodeMutation::Unlink,
    );
    assert_eq!(run(&entry, &reader, &f, &unlink)["result"]["deleted"], true);
    let rmdir = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir",
        Some(4),
        NodeMutation::RemoveDirectory,
    );
    let deleted = run(&entry, &reader, &f, &rmdir);
    assert_eq!(deleted["result"]["deleted"], true);
    assert_eq!(deleted["result"]["node"], first["result"]["node"]);
    let recreated = prepare(
        &entry,
        &f,
        f.pools[0],
        "/http-dir",
        None,
        NodeMutation::CreateDirectory,
    );
    let new = run(&entry, &reader, &f, &recreated);
    assert_ne!(new["result"]["node"], first["result"]["node"]);
    assert_eq!(run(&entry, &reader, &f, &directory), first);
    assert_eq!(run(&entry, &reader, &f, &rmdir), deleted);
    owner.expect(0);
    drop(entry);
    drop(server);
    owner.permission.close(12);
    drop(owner);
    assert_eq!(
        send(&reader, &f, "GET", &route(&f, &directory), &[]).1,
        first
    );
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    assert_eq!(run(&server, &reader, &f, &directory), first);
    assert_eq!(run(&server, &reader, &f, &rmdir), deleted);
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &format!("{}/requests", base(&f)),
            stale["request"].to_string().as_bytes()
        )
        .1,
        rejected
    );
    owner.expect(0);
    drop(server);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB_TEST_CLUSTER；规范请求、namespace、只读、无介质与接管取消，不访问真实设备"]
fn node_http_rejects_invalid_inputs_and_retains_no_media_tasks_for_takeover_cancel() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let empty_pool = Uuid::new_v4();
    let f = rt.block_on(Box::pin(fixture_with_extra_pool(
        &cluster,
        &[],
        Some(empty_pool),
    )));
    eprintln!("node_http_workspace={}", f.root.display());
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let reader = reader(&f);
    let prepared = prepare(
        &server,
        &f,
        empty_pool,
        "/wait",
        None,
        NodeMutation::CreateDirectory,
    );
    let uri = route(&f, &prepared);
    let submit = format!("{}/requests", base(&f));
    let raw = prepared["request"].to_string();
    let limited = ReadHttpServer::start(fdb_http::writer_router(
        f.catalog.clone(),
        BTreeMap::from([(f.serial.clone(), f.route)]),
        BTreeMap::from([(
            f.serial.clone(),
            super::active_pools::context(&owner, 71, 12),
        )]),
        HashMap::new(),
        tape_rs::daemon::fdb_read::ReadStorageBudget {
            index_bytes: 512,
            ..Default::default()
        },
    ));
    assert!(raw.len() > 512);
    let (code, refusal) = send(&limited, &f, "POST", &submit, raw.as_bytes());
    assert_eq!(code, 400);
    assert!(refusal["error"].as_str().unwrap().contains("超过限制"));
    let input = json!({"version":1,"pool":empty_pool.to_string(),"operation":Uuid::new_v4().to_string(),
        "path":"/limit","expected_version":null,"time":"2026-11-01T12:00:00.000000000Z","mutation":{"kind":"create-directory"}});
    assert!(input.to_string().len() < 512);
    let (code, refusal) = send(
        &limited,
        &f,
        "POST",
        &format!("{}/prepare", base(&f)),
        input.to_string().as_bytes(),
    );
    assert_eq!(code, 400);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("规范节点请求超过")
    );
    drop(limited);
    for field in [
        "expected_version",
        "time",
        "mutation",
        "operation",
        "unknown",
    ] {
        let mut bad = input.clone();
        match field {
            "expected_version" => {
                bad.as_object_mut().unwrap().remove(field);
            }
            "time" => bad[field] = json!("not-an-LTFS-time"),
            "mutation" => bad[field] = json!({"kind":"physical-plan"}),
            "operation" => bad[field] = json!(Uuid::nil().to_string()),
            _ => bad[field] = json!(true),
        }
        assert_eq!(
            send(
                &server,
                &f,
                "POST",
                &format!("{}/prepare", base(&f)),
                bad.to_string().as_bytes()
            )
            .0,
            400,
            "{field}"
        );
    }
    assert_eq!(
        send(
            &reader,
            &f,
            "POST",
            &format!("{}/prepare", base(&f)),
            input.to_string().as_bytes()
        )
        .0,
        405
    );
    let mut malformed = prepared["request"].clone();
    malformed["unknown"] = json!(true);
    for bytes in [
        malformed.to_string(),
        format!(" {raw}"),
        raw.replace("\"version\":1", "\"version\":1,\"version\":1"),
    ] {
        assert_eq!(send(&server, &f, "POST", &submit, bytes.as_bytes()).0, 400);
    }
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &format!("{submit}?pool=other"),
            raw.as_bytes()
        )
        .0,
        400
    );
    assert_eq!(
        server
            .request_body(
                "POST",
                &submit,
                Some(&format!("Content-Length: {}\r\n", raw.len())),
                raw.as_bytes()
            )
            .0,
        409
    );
    assert_eq!(server.request_body("POST", &submit, Some(&format!("x-tapers-fdb-namespace: {}\r\nx-tapers-fdb-namespace: {}\r\nContent-Length: {}\r\n",f.route.route.namespace,f.route.route.namespace,raw.len())),raw.as_bytes()).0,409);
    assert_eq!(
        server
            .request_body(
                "POST",
                &submit,
                Some(&format!(
                    "x-tapers-fdb-namespace: {}\r\nContent-Length: {}\r\n",
                    Uuid::new_v4(),
                    raw.len()
                )),
                raw.as_bytes()
            )
            .0,
        409
    );
    assert_eq!(send(&reader, &f, "POST", &submit, raw.as_bytes()).0, 405);
    assert_eq!(send(&reader, &f, "DELETE", &uri, &[]).0, 405);
    assert_eq!(send(&reader, &f, "GET", &uri, &[]).0, 404);
    let (code, accepted) = send(&server, &f, "POST", &submit, raw.as_bytes());
    assert_eq!(code, 202, "{accepted}");
    std::thread::sleep(Duration::from_millis(1200));
    let (code, waiting) = send(&reader, &f, "GET", &uri, &[]);
    assert_eq!(code, 200);
    assert_eq!(waiting, accepted);
    assert_eq!(waiting["status"], "queued");
    assert!(waiting["result"].is_null());
    malformed = prepared["request"].clone();
    malformed["time"] = json!("2026-11-01T12:00:01.000000000Z");
    assert_eq!(
        send(
            &server,
            &f,
            "POST",
            &submit,
            malformed.to_string().as_bytes()
        )
        .0,
        400
    );
    assert_eq!(send(&reader, &f, "GET", &uri, &[]).1, accepted);
    assert!(owner.permission.allows(12, Phase::Serving));
    drop(server);
    owner.permission.close(12);
    drop(owner);
    assert_eq!(send(&reader, &f, "GET", &uri, &[]).1, accepted);
    let owner = Owner::start_mode(&f, 13, json!({}), true);
    let server = writer(&f, &owner, 13);
    let (code, cancelled) = send(&server, &f, "DELETE", &uri, &[]);
    assert_eq!(code, 200, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["accepted_owner_epoch"], 12);
    assert_eq!(cancelled["completed_owner_epoch"], 13);
    assert_eq!(send(&server, &f, "DELETE", &uri, &[]).1, cancelled);
    assert_eq!(
        send(&server, &f, "POST", &submit, raw.as_bytes()).1,
        cancelled
    );
    drop(server);
    drop(reader);
    drop(owner);
    fs::remove_dir_all(&f.root).unwrap();
}
