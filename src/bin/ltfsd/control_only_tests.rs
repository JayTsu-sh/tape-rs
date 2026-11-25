#[path = "../../daemon/control_only/binding_fixture.rs"]
mod binding_fixture;
use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use tape_rs::daemon::control_only::{
    AuthorityReader, BoundDirectoryAudit, CatalogExpectation, ControlOnlyBinding, ExpectedRoute,
    FileSeal, LibraryBinding,
};
use tape_rs::daemon::{
    directory::Directory,
    multilib::LibrariesConfig,
    state::{ControlSnapshot, ControlState, Executor},
};
struct Reader(ExpectedRoute);
impl AuthorityReader for Reader {
    fn active_route(&self, _: &str) -> tape_rs::error::Result<Option<ExpectedRoute>> {
        Ok(Some(self.0.clone()))
    }
}

#[test]
fn control_only_real_startup_has_no_provider_or_business_files_and_restarts() {
    let root = std::env::temp_dir().join(format!("control-startup-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let config=LibrariesConfig::parse(&serde_json::json!({"default_library":"B","libraries":[{"changer_serial":"B","drive_serials":["D1"]},{"changer_serial":"C","drive_serials":["D2"]}]})).unwrap();
    let config_path = root.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&config.value()).unwrap()).unwrap();
    std::fs::write(
        root.join("libraries.json"),
        serde_json::to_vec(&config.value()).unwrap(),
    )
    .unwrap();
    let cluster = root.join("fdb.cluster");
    std::fs::write(&cluster, b"injected-readonly-authority-no-sdk").unwrap();
    let route = ExpectedRoute {
        namespace: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        frozen_index: 2745,
        certificate_sha256: "ab".repeat(32),
        initial_owner_epoch: 1,
    };
    let reader = Arc::new(Reader(route.clone()));
    let mut libraries = BTreeMap::new();
    let mut originals = Vec::new();
    for library in &config.libraries {
        let dir = config.library_dir(&root, &library.changer_serial);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.db");
        drop(Directory::open(&path).unwrap());
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("INSERT OR REPLACE INTO meta VALUES ('applied_index',7133),('catalog_frozen_index',2745);").unwrap();
        drop(db);
        std::fs::create_dir(dir.join("spool")).unwrap();
        std::fs::write(dir.join("spool/original"), b"pending old body").unwrap();
        let seal = FileSeal::read(&path).unwrap();
        originals.push((path, seal.clone()));
        libraries.insert(
            library.changer_serial.clone(),
            LibraryBinding {
                directory: seal,
                quiesce_index: 2058,
                frozen_index: 2745,
                catalog: CatalogExpectation::ExistingRoute {
                    route: route.clone(),
                },
            },
        );
    }
    std::fs::create_dir(root.join("gateway-spool")).unwrap();
    std::fs::write(root.join("gateway-spool/original"), b"gateway old body").unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    drop(socket);
    let peers = HashMap::from([
        (1, address.clone()),
        (2, "127.0.0.1:1".into()),
        (3, "127.0.0.1:2".into()),
    ]);
    let mut store = RaftStore::open(&root.join("raft.db"), &[1, 2, 3]).unwrap();
    let ctl = ControlState {
        applied_index: 72616,
        catalog_quiesce_index: Some(2058),
        catalog_frozen_index: Some(2745),
        executor: Some(Executor {
            node: 1,
            term: 2,
            round: 36011,
            fenced: true,
        }),
        executor_ready: true,
        ..Default::default()
    };
    let value = serde_json::from_slice::<serde_json::Value>(
        &ControlSnapshot {
            control: ctl,
            source: 1,
            directory_index: 71998,
        }
        .encode(),
    )
    .unwrap();
    let mut snapshot = raft::eraftpb::Snapshot::default();
    snapshot.mut_metadata().index = 72616;
    snapshot.mut_metadata().term = 2;
    snapshot
        .mut_metadata()
        .set_conf_state(raft::eraftpb::ConfState::from((vec![1, 2, 3], vec![])));
    snapshot.set_data(serde_json::to_vec(&serde_json::json!({"cluster_version":2,"default_library":"B","source":1,"directory_index":71998,"controls":{"B":value,"C":value,"@gateway":value}})).unwrap().into());
    store.install_snapshot(&snapshot).unwrap();
    let mut hs = raft::eraftpb::HardState::default();
    hs.term = 2;
    hs.commit = 72616;
    store.set_hardstate(&hs).unwrap();
    drop(store);
    for cycle in 0..2 {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap().to_string();
        drop(socket);
        let mut peers = peers.clone();
        peers.insert(1, address.clone());
        STOPPING.store(false, std::sync::atomic::Ordering::SeqCst);
        let meta = std::fs::metadata(&root).unwrap();
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&config.value()).unwrap())
        );
        let mut binding = ControlOnlyBinding {
            version: 1,
            node: 1,
            members: peers.iter().map(|(id, a)| (*id, a.clone())).collect(),
            cluster_sha256: format!("{:x}", Sha256::digest(std::fs::read(&cluster).unwrap())),
            libraries_sha256: hash.clone(),
            group: hash,
            data_root: root.clone(),
            data_root_dev: meta.dev(),
            data_root_inode: meta.ino(),
            original_chain_sha256: String::new(),
            original_chain_manifest: PathBuf::new(),
            raft: FileSeal::read(&root.join("raft.db")).unwrap(),
            manifest: FileSeal::read(&root.join("libraries.json")).unwrap(),
            libraries: libraries.clone(),
        };
        binding_fixture::seal(&mut binding, &config);
        let binding = Arc::new(binding);
        let audits: BTreeMap<String, BoundDirectoryAudit> = config
            .libraries
            .iter()
            .map(|l| {
                (
                    l.changer_serial.clone(),
                    binding
                        .audit_source(
                            &l.changer_serial,
                            cluster.clone(),
                            config.clone(),
                            reader.clone(),
                        )
                        .unwrap(),
                )
            })
            .collect();
        let args = Args::try_parse_from([
            "ltfsd",
            "--id",
            "1",
            "--listen",
            &address,
            "--peer",
            &format!("1={address}"),
            "--peer",
            "2=127.0.0.1:1",
            "--peer",
            "3=127.0.0.1:2",
            "--data-dir",
            root.to_str().unwrap(),
            "--libraries-config",
            config_path.to_str().unwrap(),
            "--tick-ms",
            "20",
        ])
        .unwrap();
        let peers = peers.clone();
        let startup = std::thread::spawn(move || {
            #[cfg(feature = "fdb")]
            let result = run_libraries(args, peers, None, Some(ControlOnlyMode { audits }), None);
            #[cfg(not(feature = "fdb"))]
            let result = run_libraries(args, peers, None, Some(ControlOnlyMode { audits }));
            result
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while !root.join("status.json").exists() {
            if startup.is_finished() {
                panic!(
                    "startup cycle {cycle} returned before status: {:?}",
                    startup.join().unwrap()
                );
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
        startup.join().unwrap().unwrap();
        assert_eq!(
            CONTROL_ONLY_PROVIDER_CONSTRUCTIONS.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        for (path, seal) in &originals {
            assert_eq!(&FileSeal::read(path).unwrap(), seal);
            assert!(!path.with_extension("db-wal").exists());
            assert!(!path.with_extension("db-shm").exists());
            assert_eq!(
                std::fs::read(path.parent().unwrap().join("spool/original")).unwrap(),
                b"pending old body"
            );
        }
        assert_eq!(
            std::fs::read(root.join("gateway-spool/original")).unwrap(),
            b"gateway old body"
        );
        assert!(!root.join("fdb-owner-spool").exists());
        assert!(!root.join("fdb-gateway-spool").exists());
        assert!(!root.join("cluster-directory.db").exists());
        std::fs::remove_file(root.join("status.json")).unwrap();
    }
    STOPPING.store(false, std::sync::atomic::Ordering::SeqCst);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fdb")]
#[test]
fn control_only_cli_requires_binding_and_rejects_execution_flags() {
    let base = [
        "ltfsd",
        "--id",
        "1",
        "--listen",
        "127.0.0.1:7000",
        "--data-dir",
        "/tmp/control-cli",
        "--libraries-config",
        "/tmp/libs.json",
        "--fdb-cluster-file",
        "/tmp/fdb.cluster",
        "--fdb-control-only",
        "--control-binding",
        "/tmp/control.json",
    ];
    assert!(Args::try_parse_from(base).is_ok());
    for flag in [
        "--demo-write",
        "--salvage",
        "--fdb-owner-readonly",
        "--fdb-owner-writable",
        "--fdb-metadata-readonly",
        "--expand-libraries-offline",
    ] {
        let mut argv = base.to_vec();
        argv.push(flag);
        assert!(Args::try_parse_from(argv).is_err(), "{flag}");
    }
    assert!(Args::try_parse_from(&base[..base.len() - 2]).is_err());
}
