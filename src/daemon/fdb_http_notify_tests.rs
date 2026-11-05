//! Opt-in network tests use only a private FDB and loopback HTTP; no SCSI.
use super::*;
use crate::client::fdb::FdbClient;
use crate::managed_catalog::verify_native_barriers;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

type Server = tokio::task::JoinHandle<()>;
async fn serve(ctx: Arc<Context>) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    (
        address,
        tokio::spawn(async move {
            axum::serve(listener, metadata_router(ctx)).await.unwrap();
        }),
    )
}
async fn request(endpoint: &str, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let stream = tokio::net::TcpStream::connect(endpoint).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", endpoint)
        .body(http_body_util::Full::new(bytes::Bytes::copy_from_slice(
            body,
        )))
        .unwrap();
    let response = sender.send_request(req).await.unwrap();
    let status = response.status().as_u16();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    connection.abort();
    (status, serde_json::from_slice(&raw).unwrap())
}
async fn sdk<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}
fn client(endpoints: impl IntoIterator<Item = String>) -> FdbClient {
    FdbClient::new(endpoints)
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(2))
}
fn generation(value: &Value) -> Uuid {
    Uuid::parse_str(value["generation"].as_str().unwrap()).unwrap()
}
fn delivery(value: &Value) -> Uuid {
    Uuid::parse_str(value["delivery_id"].as_str().unwrap()).unwrap()
}
async fn ack(c: Arc<FdbClient>, serial: String, batch: Value) {
    sdk(move || {
        c.acknowledge_notify(
            &serial,
            "external",
            generation(&batch),
            delivery(&batch),
            batch["through"].as_u64().unwrap(),
        )
        .unwrap()
    })
    .await;
}
async fn inject(c: Arc<FdbClient>, serial: String, kind: &'static str, n: u64) -> Value {
    sdk(move || {
        c.inject_notification(&serial, Uuid::new_v4(), kind, json!({"n":n}))
            .unwrap()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "需要隔离 FDB_TEST_CLUSTER 和默认构建的 LTFS_CTL_BIN；仅本地 HTTP/FDB/断线注入，不访问设备"]
async fn isolated_fdb_notify_http_cli_replay_failover_widen_and_backpressure() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let cli = std::env::var("LTFS_CTL_BIN").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let catalog = Arc::new(ManagedCatalog::open(Some(&cluster)).unwrap());
    let ns = Uuid::new_v4();
    let serial = format!("NOTIFY-NET-{ns}");
    let challenge = Uuid::new_v4();
    let replies:Vec<_>=(1..=3).map(|id|json!({
        "version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,
        "term":7,"leader":2,"role":if id==2{"Leader"}else{"Follower"},
        "members":{"1":"127.0.0.1:8001","2":"127.0.0.1:8002","3":"127.0.0.1:8003"},
        "quiesce_index":89,"frozen_index":90,"applied_index":92,
        "executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true
    })).collect();
    let route = catalog
        .initialize_native_library(
            ns,
            &verify_native_barriers(&serial, challenge, &replies).unwrap(),
        )
        .await
        .unwrap();
    let context = || {
        Arc::new(Context {
            catalog: catalog.clone(),
            routes: BTreeMap::from([(serial.clone(), route)]),
            owner: None,
            notify_streams: Arc::new(Semaphore::new(1)),
            notify_requests: Arc::new(Semaphore::new(2)),
        })
    };
    let ctx_a = context();
    let ctx_b = context();
    let (a, server_a) = serve(ctx_a.clone()).await;
    let (b, server_b) = serve(ctx_b.clone()).await;
    let c = Arc::new(client([b.clone()]));
    let task = "task.status.changed";
    let state = "tape.state.changed";
    inject(c.clone(), serial.clone(), task, 1).await;
    let state2 = inject(c.clone(), serial.clone(), state, 2).await;
    inject(c.clone(), serial.clone(), task, 3).await;
    let created = {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || {
            c.create_notify_consumer(&serial, "external", json!({"event_types":[state]}), true)
                .unwrap()
        })
        .await
    };
    let gen_id = generation(&created);
    let path = format!("/admin/notify/consumers/external?library={serial}");
    // A failed body parse cannot replace a field or mutate the consumer.
    for body in [
        br#"{"filter":{},"from_start":true,"from_start":false}"#.as_slice(),
        br#"{"filter":{"event_types":[],"event_types":["tape.state.changed"]},"from_start":true}"#,
        br#"{"filter":{},"from_start":true,"callback":"http://external.invalid"}"#,
    ] {
        assert_eq!(request(&a, "PUT", &path, body).await.0, 400);
    }
    assert_eq!(
        request(&a, "PUT", &path, &vec![b'x'; 16 * 1024 + 1])
            .await
            .0,
        400
    );
    // Exhausted RPC permits reject before reading a body or touching FDB.
    let permits = ctx_a
        .notify_requests
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    assert_eq!(request(&a, "GET", &path, b"").await.0, 503);
    drop(permits);
    // An incomplete body releases its permit after the request deadline.
    let mut slow = tokio::net::TcpStream::connect(&a).await.unwrap();
    slow.write_all(
        format!("PUT {path} HTTP/1.1\r\nHost: {a}\r\nContent-Length: 100\r\n\r\n{{").as_bytes(),
    )
    .await
    .unwrap();
    let mut raw = Vec::new();
    let mut byte = [0];
    tokio::time::timeout(Duration::from_secs(13), async {
        while !raw.ends_with(b"\r\n\r\n") {
            assert!(slow.read(&mut byte).await.unwrap() > 0);
            raw.push(byte[0]);
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8(raw).unwrap().starts_with("HTTP/1.1 408"));
    drop(slow);
    assert_eq!(ctx_a.notify_requests.available_permits(), 2);
    // Close an established SSE transport after first delivery. The SDK must
    // reconnect through B with the same generation and immutable pending batch.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap().to_string();
    let (disconnect, disconnected) = tokio::sync::oneshot::channel::<()>();
    let upstream = a.clone();
    let proxy_task = tokio::spawn(async move {
        let (mut incoming, _) = listener.accept().await.unwrap();
        drop(listener);
        let mut outgoing = tokio::net::TcpStream::connect(upstream).await.unwrap();
        tokio::select! { _=tokio::io::copy_bidirectional(&mut incoming,&mut outgoing)=>{}, _=disconnected=>{} }
    });
    let batches = {
        let serial = serial.clone();
        let b = b.clone();
        sdk(move || {
            let mut disconnect = Some(disconnect);
            let mut batches = Vec::new();
            client([proxy, b])
                .watch_notifications(&serial, "external", gen_id, None, |batch| {
                    batches.push(batch.clone());
                    if let Some(close) = disconnect.take() {
                        close.send(()).unwrap();
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                })
                .unwrap();
            batches
        })
        .await
    };
    proxy_task.await.unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0], batches[1]);
    assert_eq!(batches[0]["events"], json!([state2]));
    assert_eq!(batches[0]["through"], 3);
    let first = batches[0].clone();
    let current = {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || c.notify_consumer(&serial, "external").unwrap()).await
    };
    assert_eq!(current["acknowledged"], 0);
    assert_eq!(current["delivery_status"], "waiting_ack");
    // Wait for disconnected server writers to observe closure; no hidden stream
    // continues using the FDB network after the test shuts it down.
    async fn drained(ctx: &Context) {
        tokio::time::timeout(Duration::from_secs(16), async {
            while ctx.notify_streams.available_permits() != 1 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    }
    drained(&ctx_a).await;
    drained(&ctx_b).await;
    let state4 = inject(c.clone(), serial.clone(), state, 4).await;
    inject(c.clone(), serial.clone(), task, 5).await;
    // Stream quota is checked before expanding types or reserving a new batch.
    let held = ctx_b.notify_streams.clone().acquire_owned().await.unwrap();
    let stream_path = format!(
        "/admin/notify/consumers/external/stream?library={serial}&generation={gen_id}&event_types={state},{task}"
    );
    assert_eq!(request(&b, "GET", &stream_path, b"").await.0, 503);
    drop(held);
    let status = catalog
        .notify_consumer(&serial, route, "external")
        .await
        .unwrap();
    assert_eq!(status["filter"]["event_types"], json!([state]));
    let widened = {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || {
            let mut got = None;
            c.watch_notifications(
                &serial,
                "external",
                gen_id,
                Some(&[state.into(), task.into()]),
                |batch| {
                    got = Some(batch.clone());
                    Ok(false)
                },
            )
            .unwrap();
            got.unwrap()
        })
        .await
    };
    assert_eq!(widened, first);
    drained(&ctx_b).await;
    ack(c.clone(), serial.clone(), first.clone()).await;
    // Original types replay their backlog; task events before widening do not.
    let backlog = {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || c.notify_batch(&serial, "external", gen_id).unwrap()).await
    };
    assert_eq!(backlog["events"], json!([state4]));
    assert_eq!(backlog["through"], 5);
    ack(c.clone(), serial.clone(), backlog).await;
    let state6 = inject(c.clone(), serial.clone(), state, 6).await;
    let task7 = inject(c.clone(), serial.clone(), task, 7).await;
    let recent = {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || c.notify_batch(&serial, "external", gen_id).unwrap()).await
    };
    assert_eq!(recent["events"], json!([state6, task7]));
    ack(c.clone(), serial.clone(), recent).await;
    assert_eq!(
        request(
            &b,
            "POST",
            &format!("/admin/notify/consumers/external/ack?library={serial}"),
            format!(
                r#"{{"generation":"{gen_id}","delivery_id":"{}","through":999}}"#,
                delivery(&first)
            )
            .as_bytes()
        )
        .await
        .0,
        400
    );
    // Lose the injection response after HTTP A has actually committed it. The
    // SDK retries through B using the original UUID, returning the same event.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = listener.local_addr().unwrap().to_string();
    let upstream = a.clone();
    let lost = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Read, Write};
        fn read_message(reader: &mut BufReader<std::net::TcpStream>) -> Vec<u8> {
            let mut raw = Vec::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                raw.extend_from_slice(line.as_bytes());
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let at = raw.len();
            raw.resize(at + length, 0);
            reader.read_exact(&mut raw[at..]).unwrap();
            raw
        }
        let (incoming, _) = listener.accept().unwrap();
        drop(listener);
        incoming
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_message(&mut BufReader::new(incoming.try_clone().unwrap()));
        let mut upstream = std::net::TcpStream::connect(upstream).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        upstream.write_all(&request).unwrap();
        let committed = read_message(&mut BufReader::new(upstream));
        drop(incoming);
        let committed = String::from_utf8(committed).unwrap();
        assert!(committed.starts_with("HTTP/1.1 200"));
        serde_json::from_str::<Value>(committed.split_once("\r\n\r\n").unwrap().1).unwrap()
    });
    let returned = {
        let serial = serial.clone();
        let b = b.clone();
        sdk(move || {
            client([proxy, b])
                .inject_notification(&serial, Uuid::new_v4(), task, json!({"lost_response":true}))
                .unwrap()
        })
        .await
    };
    let committed = lost.join().unwrap();
    assert_eq!(returned, committed);
    assert_eq!(returned["sequence"], 8);
    let after = catalog
        .notify_consumer(&serial, route, "external")
        .await
        .unwrap();
    assert_eq!(after["head"], 8);
    // CLI uses the ordinary default binary: no native FDB client dependency.
    let run_cli = |args: Vec<String>| {
        let cli = cli.clone();
        let b = b.clone();
        let serial = serial.clone();
        async move {
            sdk(move || {
                let output = std::process::Command::new(cli)
                    .args(["--endpoint", &b, "--library", &serial, "notify"])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "CLI failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice::<Value>(&output.stdout).unwrap()
            })
            .await
        }
    };
    let cli_created = run_cli(vec![
        "create".into(),
        "cli".into(),
        "--from-start".into(),
        "--event-type".into(),
        task.into(),
    ])
    .await;
    let cli_gen = generation(&cli_created).to_string();
    let op = Uuid::new_v4().to_string();
    let injected = run_cli(vec![
        "inject".into(),
        "--operation".into(),
        op,
        "--event-type".into(),
        task.into(),
        "--data".into(),
        "{\"cli\":true}".into(),
    ])
    .await;
    assert_eq!(injected["injected"], true);
    let list = run_cli(vec!["list".into()]).await;
    assert_eq!(list["consumers"].as_array().unwrap().len(), 2);
    assert_eq!(
        run_cli(vec!["show".into(), "cli".into()]).await["generation"],
        cli_gen
    );
    let cli_batch = run_cli(vec![
        "read".into(),
        "cli".into(),
        "--generation".into(),
        cli_gen.clone(),
    ])
    .await;
    assert!(cli_batch["events"].as_array().unwrap().contains(&injected));
    // watch prints a batch but does not ACK it. Kill only this test's child.
    {
        let cli = cli.clone();
        let b = b.clone();
        let serial = serial.clone();
        let cli_gen = cli_gen.clone();
        let expected = cli_batch.clone();
        sdk(move || {
            use std::io::{BufRead, BufReader};
            use std::process::Stdio;
            struct Child(std::process::Child);
            impl Drop for Child {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut child = Child(
                std::process::Command::new(cli)
                    .args([
                        "--endpoint",
                        &b,
                        "--library",
                        &serial,
                        "notify",
                        "watch",
                        "cli",
                        "--generation",
                        &cli_gen,
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut line = String::new();
                let r = BufReader::new(stdout).read_line(&mut line);
                let _ = tx.send((r, line));
            });
            let read = rx.recv_timeout(Duration::from_secs(10));
            drop(child);
            reader.join().unwrap();
            let (r, line) = read.unwrap();
            assert!(r.unwrap() > 0);
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
        })
        .await;
    }
    drained(&ctx_b).await;
    assert_eq!(
        catalog
            .notify_consumer(&serial, route, "cli")
            .await
            .unwrap()["acknowledged"],
        0
    );
    run_cli(vec![
        "ack".into(),
        "cli".into(),
        "--generation".into(),
        cli_gen.clone(),
        "--delivery-id".into(),
        delivery(&cli_batch).to_string(),
        "--through".into(),
        cli_batch["through"].to_string(),
    ])
    .await;
    run_cli(vec![
        "delete".into(),
        "cli".into(),
        "--generation".into(),
        cli_gen,
        "--abandon-unacknowledged".into(),
    ])
    .await;
    assert_eq!(
        catalog
            .list_notify_consumers(&serial, route, None, 100)
            .await
            .unwrap()["consumers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let final_batch = catalog
        .notify_batch(&serial, route, "external", gen_id)
        .await
        .unwrap();
    ack(c.clone(), serial.clone(), final_batch).await;
    // Continuous watch permits explicit ACK and another RPC inside the callback.
    // Neither user code nor nested SDK calls execute inside the polling runtime.
    inject(c.clone(), serial.clone(), task, 10).await;
    {
        let c = c.clone();
        let serial = serial.clone();
        sdk(move || {
            let mut calls = 0;
            c.watch_notifications(&serial, "external", gen_id, None, |batch| {
                calls += 1;
                assert_eq!(batch["events"].as_array().unwrap().len(), 1);
                assert_eq!(batch["events"][0]["data"]["n"], 9 + calls);
                c.acknowledge_notify(
                    &serial,
                    "external",
                    gen_id,
                    delivery(batch),
                    batch["through"].as_u64().unwrap(),
                )?;
                if calls == 1 {
                    c.inject_notification(&serial, Uuid::new_v4(), task, json!({"n":11}))?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .unwrap();
            assert_eq!(calls, 2);
        })
        .await;
    }
    drained(&ctx_b).await;
    let status = catalog
        .notify_consumer(&serial, route, "external")
        .await
        .unwrap();
    assert_eq!(status["acknowledged"], 11);
    assert_eq!(status["head"], 11);
    // A real FDB transaction dependency timeout is a temporary error. This
    // separate connection file cannot reach or interrupt the healthy database.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_address = dead.local_addr().unwrap();
    drop(dead);
    let cluster_file = std::env::temp_dir().join(format!("notify-unreachable-{ns}.cluster"));
    std::fs::write(
        &cluster_file,
        format!("unreachable:{}@{dead_address}", ns.simple()),
    )
    .unwrap();
    let unavailable_catalog = ManagedCatalog::open(cluster_file.to_str()).unwrap();
    let unavailable_ctx = Arc::new(Context {
        catalog: Arc::new(unavailable_catalog),
        routes: ctx_b.routes.clone(),
        owner: None,
        notify_streams: Arc::new(Semaphore::new(1)),
        notify_requests: Arc::new(Semaphore::new(1)),
    });
    let (unavailable_endpoint, unavailable_server) = serve(unavailable_ctx.clone()).await;
    let unavailable = tokio::time::timeout(
        Duration::from_secs(8),
        request(&unavailable_endpoint, "GET", &path, b""),
    )
    .await
    .unwrap();
    assert_eq!(unavailable.0, 503);
    assert_eq!(unavailable.1["retryable"], true);
    unavailable_server.abort();
    let _ = unavailable_server.await;
    std::fs::remove_file(cluster_file).unwrap();
    assert_eq!(unavailable_ctx.notify_requests.available_permits(), 1);
    assert_eq!(
        catalog
            .notify_consumer(&serial, route, "external")
            .await
            .unwrap()["acknowledged"],
        11
    );
    // Corrupt only this random test namespace's notification head. Check the
    // actual key exists before modifying the documented catalog/22 key layout.
    let db = foundationdb::Database::new(Some(&cluster)).unwrap();
    let mut prefix = b"tape-rs/catalog/22/~/".to_vec();
    prefix.extend_from_slice(ns.as_bytes());
    prefix.extend_from_slice(serial.as_bytes());
    prefix.extend_from_slice(&[0, 0]);
    let mut head = prefix.clone();
    head.push(b'h');
    let tr = db.create_trx().unwrap();
    let original = tr.get(&head, false).await.unwrap().unwrap().to_vec();
    assert_eq!(original, 11u64.to_be_bytes());
    tr.set(&head, b"corrupt");
    tr.commit().await.unwrap();
    let op = Uuid::new_v4();
    let body = json!({"operation":op.to_string(),"event_type":task,"data":{"n":12}}).to_string();
    let bad = request(
        &b,
        "POST",
        &format!("/admin/notify/events/inject?library={serial}"),
        body.as_bytes(),
    )
    .await;
    assert_eq!(bad.0, 500);
    assert_eq!(bad.1["retryable"], false);
    let tr = db.create_trx().unwrap();
    assert_eq!(
        tr.get(&head, false).await.unwrap().unwrap().as_ref(),
        b"corrupt"
    );
    let mut next = prefix.clone();
    next.push(b'e');
    next.extend_from_slice(&12u64.to_be_bytes());
    let mut receipt = prefix;
    receipt.push(b'i');
    receipt.extend_from_slice(op.to_string().as_bytes());
    receipt.extend_from_slice(&[0, 0]);
    assert!(tr.get(&next, false).await.unwrap().is_none());
    assert!(tr.get(&receipt, false).await.unwrap().is_none());
    tr.set(&head, &original);
    tr.commit().await.unwrap();
    server_a.abort();
    server_b.abort();
    let _ = server_a.await;
    let _ = server_b.await;
    assert_eq!(ctx_a.notify_requests.available_permits(), 2);
    assert_eq!(ctx_b.notify_requests.available_permits(), 2);
    eprintln!(
        "verified namespace={ns}; HTTP disconnect/replay, type widening, response-loss retry, slow body, backpressure, and all CLI notification commands; no hardware"
    );
}
