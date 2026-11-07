use super::*;
use std::process::{Command, Output};
use tape_rs::client::fdb::RenameJournal;

fn cli(endpoint: &str, arguments: &[&str]) -> Output {
    Command::new(
        std::env::var_os("TAPE_RS_FDB_CLI_BIN")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_ltfsctl").into()),
    )
    .args(["-e", endpoint])
    .args(arguments)
    .output()
    .unwrap()
}
fn json_output(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[ignore = "需要隔离 FDB；真实 CLI/HTTP/owner 与模拟磁带，不访问设备"]
fn rename_cli_prepares_original_conditions_and_recovers_offline_first_result() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_mode(&f, 12, json!({}), true);
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(10));
    let pool = f.pools[0].to_string();
    let namespace = f.route.route.namespace.to_string();
    let source = client
        .stat(&f.serial, f.pools[0], "/file-0")
        .unwrap()
        .unwrap();
    let target = client
        .stat(&f.serial, f.pools[0], "/file-1")
        .unwrap()
        .unwrap();
    let source_version = source["version"].as_u64().unwrap().to_string();
    let target_version = target["version"].as_u64().unwrap().to_string();
    for (name, expected, ns, replacement) in [
        (
            "wrong-version",
            "18446744073709551614",
            namespace.as_str(),
            true,
        ),
        (
            "wrong-namespace",
            source_version.as_str(),
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            true,
        ),
        (
            "target-must-be-absent",
            source_version.as_str(),
            namespace.as_str(),
            false,
        ),
    ] {
        let directory = f.root.join(name);
        let mut args = vec![
            "--library",
            &f.serial,
            "--pool",
            &pool,
            "fdb",
            "rename",
            "prepare",
            "/file-0",
            "/file-1",
            "--expected-version",
            expected,
            "--namespace",
            ns,
            "--request",
            directory.to_str().unwrap(),
        ];
        if replacement {
            args.extend(["--expected-target-version", &target_version]);
        }
        assert!(!cli(&server.address, &args).status.success());
        assert!(!directory.exists());
    }
    let directory = f.root.join("rename-cli-original");
    let prepared = cli(
        &server.address,
        &[
            "--library",
            &f.serial,
            "--pool",
            &pool,
            "fdb",
            "rename",
            "prepare",
            "/file-0",
            "/file-1",
            "--expected-version",
            &source_version,
            "--expected-target-version",
            &target_version,
            "--namespace",
            &namespace,
            "--request",
            directory.to_str().unwrap(),
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let saved = RenameJournal::open(&directory).unwrap();
    let operation = saved.operation();
    assert_eq!(
        client.rename_status(saved.prepared().unwrap()).unwrap(),
        None,
        "prepare must not queue or execute a rename"
    );
    drop(saved);
    let first = json_output(cli(
        &server.address,
        &["fdb", "rename", "run", directory.to_str().unwrap()],
    ));
    assert_eq!(first["status"], "completed");
    assert_eq!(first["operation"], operation.to_string());
    assert_eq!(first["result"]["source"]["inode"], source["id"]);
    let output = f.root.join("cli-readback");
    client
        .download_new(&f.serial, f.pools[0], "/file-1", &output)
        .unwrap();
    assert_eq!(fs::read(output).unwrap(), b"content-0");
    mutate(&client, &f, "/file-0", json!({"kind":"create-directory"}));
    drop(client);
    drop(server);
    drop(owner);
    let header = fs::read(directory.join("request.json")).unwrap();
    let receipt = fs::read(directory.join("receipt.json")).unwrap();
    for action in ["run", "status", "cancel"] {
        assert_eq!(
            json_output(cli(
                "127.0.0.1:0",
                &["fdb", "rename", action, directory.to_str().unwrap()]
            )),
            first
        );
    }
    assert!(
        !cli(
            "127.0.0.1:0",
            &[
                "--library",
                &f.serial,
                "fdb",
                "rename",
                "run",
                directory.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(directory.join("request.json")).unwrap(), header);
    assert_eq!(fs::read(directory.join("receipt.json")).unwrap(), receipt);
    fs::write(directory.join("receipt.json"), b"{}").unwrap();
    assert!(
        !cli(
            "127.0.0.1:0",
            &["fdb", "rename", "status", directory.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(directory.join("receipt.json")).unwrap(), b"{}");
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
#[ignore = "需要隔离 FDB；CLI 取消原无介质排队任务并离线查询，仅模拟磁带"]
fn rename_cli_cancels_original_queued_request_and_preserves_first_cancellation() {
    let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
    let _network = unsafe { foundationdb::boot() };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let f = rt.block_on(Box::pin(fixture(&cluster)));
    let owner = Owner::start_with_policy_mode(
        &f,
        12,
        json!({"tapes":{BARCODES[0]:{"online":false},BARCODES[1]:{"online":false}}}),
        true,
    );
    let server = writer(&f, &owner, 12);
    let client = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let spec = rename_sdk_spec(&client, &f, "/file-0", "/file-1", false);
    let directory = f.root.join("rename-cli-cancel");
    client.prepare_rename(&f.serial, &spec, &directory).unwrap();
    let no_wait = FdbClient::new([&server.address])
        .unwrap()
        .with_timeouts(Duration::ZERO, Duration::from_secs(5));
    assert!(matches!(
        no_wait.run_rename_saved(&directory),
        Err(tape_rs::client::ClientError::Indeterminate(_))
    ));
    let first = json_output(cli(
        &server.address,
        &["fdb", "rename", "cancel", directory.to_str().unwrap()],
    ));
    assert_eq!(first["status"], "cancelled");
    drop(server);
    drop(owner);
    assert_eq!(
        json_output(cli(
            "127.0.0.1:0",
            &["fdb", "rename", "status", directory.to_str().unwrap()]
        )),
        first
    );
    assert!(
        !cli(
            "127.0.0.1:0",
            &["fdb", "rename", "run", directory.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert_eq!(
        json_output(cli(
            "127.0.0.1:0",
            &["fdb", "rename", "cancel", directory.to_str().unwrap()]
        )),
        first
    );
    fs::remove_dir_all(&f.root).unwrap();
}
