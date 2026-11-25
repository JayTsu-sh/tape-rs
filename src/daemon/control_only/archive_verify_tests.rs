use super::*;
use crate::{
    daemon::{
        multilib::readonly_control_replay,
        state::{Command, ControlSnapshot, Executor},
        store::RaftStore,
    },
    managed_catalog::{ManagementRequest, archived_reclaim_fixture_rows},
};
use protobuf::Message;
use raft::eraftpb::{ConfState, Entry, Snapshot};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

const SERIAL: &str = "SIM-ARCHIVED-B";
const FROZEN: u64 = 90;
const QUIESCE: u64 = 89;
const OWNER: u64 = 110;

type Rows = BTreeMap<&'static str, (Vec<u8>, Vec<u8>)>;

struct Fixture {
    root: PathBuf,
    sources: Sources,
    request: Value,
    baseline: Value,
    current: Value,
    rows: Rows,
}

impl Fixture {
    fn new(root: PathBuf) -> Self {
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("sources")).unwrap();
        let sources = Sources(File::open(root.join("sources")).unwrap());
        let mut baseline = serde_json::Map::new();
        let mut current = serde_json::Map::new();
        let mut reports = vec![];
        for node in 1..=3 {
            let (old, new, report) = make_control(&root, node);
            let host = (node + 30).to_string();
            baseline.insert(host.clone(), old);
            current.insert(host, new);
            reports.push(report);
        }
        let (baseline, current) = (Value::Object(baseline), Value::Object(current));
        let source_dir = root.join("sources");
        let namespace = Uuid::from_u128(1);
        let operation = Uuid::from_u128(2);
        let management = ManagementRequest::parse(json!({
            "version":1, "library":SERIAL, "namespace":namespace.to_string(), "operation":operation.to_string(),
            "epoch":3,
            "change":{"kind":"media-reclaim","pool":Uuid::from_u128(4).to_string(),"barcode":"L00284L8",
                "volume":Uuid::from_u128(5).to_string(),"revision":1,"confirm_format":true,"unassign":false}
        })).unwrap();
        let rows = archived_reclaim_fixture_rows(&management, FROZEN, QUIESCE, OWNER);
        let request = json!({
            "version":1, "profile":PROFILE, "scope":"private", "component":"control-fdb",
            "challenge":Uuid::from_u128(7).to_string(),
            "control":{
                "serial":SERIAL, "nodes":{"31":1, "32":2, "33":3},
                "baseline_manifest_sha256":put(&source_dir, baseline.to_string().as_bytes()),
                "current_manifest_sha256":put(&source_dir, current.to_string().as_bytes()),
                "report_sha256":put(&source_dir, json!({"nodes":reports}).to_string().as_bytes())
            },
            "export":export(&source_dir, &rows, namespace, operation)
        });
        Self {
            root,
            sources,
            request,
            baseline,
            current,
            rows,
        }
    }

    fn make() -> Self {
        Self::new(std::env::temp_dir().join(format!("facts3-archive-{}", Uuid::new_v4())))
    }

    fn run(&self) -> Result<Value> {
        verify_request(self.request.to_string().as_bytes(), &self.sources)
    }

    fn replace_current(&mut self, host: &str, name: &str, path: &Path) {
        let raw = fs::read(path).unwrap();
        self.current[host][name]["sha256"] = json!(put(&self.root.join("sources"), &raw));
        self.current[host][name]["st_size"] = json!(raw.len());
        self.request["control"]["current_manifest_sha256"] = json!(put(
            &self.root.join("sources"),
            self.current.to_string().as_bytes(),
        ));
    }

    fn replace_export(&mut self, rows: &Rows) {
        self.request["export"] = export(
            &self.root.join("sources"),
            rows,
            Uuid::from_u128(1),
            Uuid::from_u128(2),
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::env::var_os("TAPE_RS_FACTS3_FIXTURE_OUTPUT").as_deref()
            != Some(self.root.as_os_str())
        {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}

fn put(root: &Path, raw: &[u8]) -> String {
    let hash = digest(raw);
    let path = root.join(&hash);
    if !path.exists() {
        fs::write(&path, raw).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    }
    hash
}

fn export(dir: &Path, rows: &Rows, namespace: Uuid, operation: Uuid) -> Value {
    let mut sorted: Vec<_> = rows.values().collect();
    sorted.sort_unstable();
    let mut raw = Vec::new();
    let mut count = 0;
    for (key, value) in sorted {
        for field in [key, value] {
            raw.extend_from_slice(&(field.len() as u32).to_be_bytes());
            raw.extend_from_slice(field);
            count += field.len();
        }
    }
    json!({
        "records_sha256":put(dir, &raw), "read_version":234, "record_count":rows.len(),
        "byte_count":count, "namespace":namespace.to_string(), "operation":operation.to_string(),
        "operation_epoch":3, "task_sha256":digest(&rows["task"].1)
    })
}

fn snapshot(node: u64, library_round: u64) -> Snapshot {
    let control = |actor, round, instance, is_library: bool| {
        serde_json::from_slice::<Value>(
            &ControlSnapshot {
                source: node,
                directory_index: 118,
                control: ControlState {
                    applied_index: 120,
                    executor: Some(Executor {
                        node: actor,
                        term: 2,
                        round,
                        fenced: true,
                    }),
                    executor_instance: instance,
                    executor_ready: true,
                    catalog_quiesce_index: is_library.then_some(QUIESCE),
                    catalog_frozen_index: is_library.then_some(FROZEN),
                    ..ControlState::default()
                },
            }
            .encode(),
        )
        .unwrap()
    };
    let mut snap = Snapshot::default();
    snap.mut_metadata().index = 120;
    snap.mut_metadata().term = 2;
    snap.mut_metadata()
        .set_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
    snap.set_data(
        json!({
            "cluster_version":2,"default_library":SERIAL,"source":node,"directory_index":118,
            "controls":{(SERIAL):control(1,library_round,9,true), (GATEWAY):control(2,100,77,false)}
        })
        .to_string()
        .into_bytes()
        .into(),
    );
    snap
}

fn entry(index: u64, data: Vec<u8>) -> Entry {
    let mut entry = Entry {
        index,
        term: 2,
        ..Default::default()
    };
    entry.set_data(data.into());
    entry
}

fn copy_record(root: &Path, name: &str, original: &Path, sources: &Path) -> Value {
    let raw = fs::read(root.join(name)).unwrap();
    let m = fs::metadata(original.join(name)).unwrap();
    json!({
        "path":original.join(name), "sha256":put(sources,&raw), "no_wal":true,
        "st_dev":m.dev(), "st_ino":m.ino(),
        "st_ctime_ns":1, "st_mtime_ns":1, "st_size":raw.len()
    })
}

fn make_control(root: &Path, node: u64) -> (Value, Value, Value) {
    let original = root.join(format!("original-{node}"));
    fs::create_dir(&original).unwrap();
    let sources = root.join("sources");
    let raft = original.join("raft.db");
    let mut store = RaftStore::open(&raft, &[1, 2, 3]).unwrap();
    let snap = snapshot(node, OWNER);
    store.install_snapshot(&snap).unwrap();
    store.append(&[entry(121, vec![])]).unwrap();
    store.set_commit(121).unwrap();
    drop(store);

    let directory = original.join("directory.db");
    drop(Directory::open(&directory).unwrap());
    let db = Connection::open(&directory).unwrap();
    db.execute_batch(
        "INSERT OR REPLACE INTO meta VALUES ('applied_index',113),('catalog_frozen_index',90)",
    )
    .unwrap();
    drop(db);
    let bundle = Connection::open(original.join("cluster-directory.snap.db")).unwrap();
    bundle.execute_batch("CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL);
        CREATE TABLE libraries(id TEXT NOT NULL,part INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(id,part));
        INSERT INTO meta VALUES('applied_index',113);").unwrap();
    bundle
        .execute(
            "INSERT INTO libraries VALUES(?1,0,?2)",
            rusqlite::params![SERIAL, fs::read(directory).unwrap()],
        )
        .unwrap();
    drop(bundle);
    let config = json!({"default_library":SERIAL,"libraries":[{"changer_serial":SERIAL,"drive_serials":["D1","D2"]}]});
    fs::write(original.join("libraries.json"), config.to_string()).unwrap();
    fs::write(
        original.join("status.json"),
        json!({"id":node,"default_library":SERIAL}).to_string(),
    )
    .unwrap();
    let baseline: serde_json::Map<String, Value> = FILES
        .iter()
        .map(|name| {
            (
                name.to_string(),
                copy_record(&original, name, &original, &sources),
            )
        })
        .collect();
    let image = image(&fs::read(&raft).unwrap(), "fixture").unwrap();
    let config = LibrariesConfig::parse(&config).unwrap();
    let (source, directory_index, controls) =
        readonly_control_replay(&config, snap.get_data(), &[]).unwrap();
    let report_controls: serde_json::Map<String, Value> = controls
        .into_iter()
        .map(|(serial, control)| {
            (
                serial,
                serde_json::from_slice(
                    &ControlSnapshot {
                        control,
                        source,
                        directory_index,
                    }
                    .encode(),
                )
                .unwrap(),
            )
        })
        .collect();
    let report = json!({
        "node":node+30, "hardstate":{"term":2,"vote":0,"commit":121},
        "snapshot_blob_sha256":digest(&snap.write_to_bytes().unwrap()),"tail_sha256":image.tail_sha256(),
        "input_sha256": baseline.iter().map(|(file,row)|(file.clone(),row["sha256"].clone())).collect::<serde_json::Map<String,Value>>(),
        "replay":{"replayed_controls":report_controls},
    });
    let mut store = RaftStore::open(&raft, &[1, 2, 3]).unwrap();
    store
        .append(&[entry(
            122,
            Command::Abandoned {
                node: 1,
                round: OWNER,
                reason: "fixture".into(),
            }
            .encode(),
        )])
        .unwrap();
    store.set_commit(122).unwrap();
    drop(store);
    let current = FILES
        .iter()
        .map(|name| {
            (
                name.to_string(),
                copy_record(&original, name, &original, &sources),
            )
        })
        .collect();
    (Value::Object(baseline), Value::Object(current), report)
}

#[test]
fn control_suffix_and_native_catalog_join() {
    let fixture = Fixture::make();
    let path = fixture.root.join("request.json");
    fs::write(&path, fixture.request.to_string()).unwrap();
    let fd = File::open(&path).unwrap();
    let result = verify(fd.as_raw_fd(), fixture.sources.0.as_raw_fd()).unwrap();
    assert_eq!(result["schema"], "fresh.native.component.summary.v1");
    assert_eq!(
        result["actor"],
        json!({"node":1,"term":2,"round":OWNER,"instance":9})
    );
    assert_eq!(result["export"]["owner_round"], OWNER);
    assert!(result["current_actor"].is_null());
    assert_eq!(result["controls"][0]["commit"], 122);
    assert_eq!(result["controls"][0]["baseline_commit"], 121);
    assert_eq!(
        result["controls"][1]["audit"]["current"]["directory"]["applied_index"],
        113
    );
}

#[test]
fn rejects_committed_prefix_changes_and_directory_business_rows() {
    let mut fixture = Fixture::make();
    let path = fixture.root.join("original-1/raft.db");
    let db = Connection::open(&path).unwrap();
    let mut changed = entry(121, vec![]);
    changed.set_context(b"changed committed context".to_vec().into());
    db.execute(
        "UPDATE entries SET data=?1 WHERE idx=121",
        [changed.write_to_bytes().unwrap()],
    )
    .unwrap();
    drop(db);
    fixture.replace_current("31", "raft.db", &path);
    assert!(
        fixture
            .run()
            .unwrap_err()
            .to_string()
            .contains("committed prefix")
    );

    let mut fixture = Fixture::make();
    let path = fixture.root.join("original-1/directory.db");
    let db = Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO pools VALUES('foreign-pool','foreign-name',1)",
        [],
    )
    .unwrap();
    drop(db);
    fixture.replace_current("31", "directory.db", &path);
    assert!(fixture.run().unwrap_err().to_string().contains("业务行"));
}

#[test]
fn rejects_gateway_owner_native_binding_and_original_task_changes() {
    let mut fixture = Fixture::make();
    for name in [
        "active",
        "certificate",
        "namespace",
        "marker",
        "layout",
        "root",
        "owner",
        "task",
        "route",
    ] {
        let mut rows = fixture.rows.clone();
        let value = &mut rows.get_mut(name).unwrap().1;
        match name {
            "active" => value[33] ^= 1,
            "certificate" => value[0] = b'I',
            "owner" => *value = 100u64.to_be_bytes().to_vec(),
            "task" => {
                let mut body: Value = serde_json::from_slice(value).unwrap();
                body["fingerprint"] = json!("0".repeat(64));
                *value = body.to_string().into_bytes();
            }
            _ => *value = vec![0xff],
        }
        fixture.replace_export(&rows);
        assert!(fixture.run().is_err(), "{name}");
    }
    let rows = fixture.rows.clone();
    fixture.replace_export(&rows);
    fixture.request["export"]["namespace"] = json!(Uuid::from_u128(999).to_string());
    assert!(fixture.run().unwrap_err().to_string().contains("归档 FDB"));

    let mut rows = fixture.rows.clone();
    let value = &mut rows.get_mut("task").unwrap().1;
    let mut task: Value = serde_json::from_slice(value).unwrap();
    task["owner_epoch"] = json!(100);
    *value = task.to_string().into_bytes();
    fixture.replace_export(&rows);
    assert!(
        fixture
            .run()
            .unwrap_err()
            .to_string()
            .contains("回执状态或 owner")
    );

    let mut control = ControlState {
        executor: Some(Executor {
            node: 2,
            round: 100,
            term: 2,
            fenced: true,
        }),
        executor_instance: 77,
        ..ControlState::default()
    };
    let gateway = control.clone();
    control.catalog_frozen_index = Some(FROZEN);
    let controls = BTreeMap::from([(SERIAL.into(), control), (GATEWAY.into(), gateway)]);
    assert!(
        actor(&controls, SERIAL, "fixture")
            .unwrap_err()
            .to_string()
            .contains("gateway")
    );
}

#[test]
fn rejects_missing_sources_links_wal_over_limit_and_unsafe_sqlite_images() {
    let mut fixture = Fixture::make();
    let baseline_hash = fixture.request["control"]["baseline_manifest_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let file = fixture.root.join("sources").join(&baseline_hash);
    let original = fixture.root.join("saved-manifest");
    fs::rename(&file, &original).unwrap();
    assert_eq!(fixture.run().unwrap_err().exit_code(), 4);
    symlink(&original, &file).unwrap();
    assert_eq!(fixture.run().unwrap_err().exit_code(), 2);
    fs::remove_file(&file).unwrap();
    fs::hard_link(&original, &file).unwrap();
    assert_eq!(fixture.run().unwrap_err().exit_code(), 2);
    fs::remove_file(&file).unwrap();
    fs::rename(&original, &file).unwrap();
    let mut baseline = fixture.baseline.clone();
    baseline["31"]["raft.db"]["no_wal"] = json!(false);
    fixture.request["control"]["baseline_manifest_sha256"] = json!(put(
        &fixture.root.join("sources"),
        baseline.to_string().as_bytes(),
    ));
    assert!(fixture.run().unwrap_err().to_string().contains("sidecar"));

    assert_eq!(
        fixture
            .sources
            .read(&baseline_hash, 20)
            .unwrap_err()
            .exit_code(),
        3
    );
    let malformed = vec![0; 100];
    assert!(memory_sqlite(&malformed, "fixture").is_err());

    let fake = fixture.root.join("schema-view.sqlite");
    let db = Connection::open(&fake).unwrap();
    db.execute_batch("CREATE VIEW meta AS SELECT 'applied_index' k, 113 v")
        .unwrap();
    drop(db);
    let db = memory_sqlite(&fs::read(fake).unwrap(), "fixture").unwrap();
    assert!(check_tables(&db, &["meta"], "fixture").is_err());
}

#[test]
fn sqlite_working_copy_is_immutable_and_rejects_damaged_pages() {
    let fixture = Fixture::make();
    let mut raw = fs::read(fixture.root.join("original-1/directory.db")).unwrap();
    assert_eq!(&raw[18..20], &[2, 2]);
    let db = memory_sqlite(&raw, "fixture").unwrap();
    db.execute_batch("PRAGMA query_only=OFF").unwrap();
    assert!(db.execute("DELETE FROM meta", []).is_err());
    assert_eq!(
        Directory::audit_readonly_connection(&db)
            .unwrap()
            .frozen_index,
        Some(FROZEN)
    );
    assert_eq!(&raw[18..20], &[2, 2]);
    drop(db);
    raw[100] = 0xff;
    assert!(directory_audit(&raw, FROZEN, 122, "fixture").is_err());
}

#[test]
fn validates_export_frames_and_input_fd() {
    let mut data = vec![];
    let key = b"tape-rs/catalog/22/A/record";
    for raw in [key.as_slice(), b"value"] {
        data.extend((raw.len() as u32).to_be_bytes());
        data.extend(raw);
    }
    assert!(export_rows(&data, 1, key.len() + 5).is_ok());
    assert!(export_rows(&data, 2, key.len() + 5).is_err());
    assert!(export_rows(&data[..data.len() - 1], 1, key.len() + 5).is_err());
    data.extend_from_within(..);
    assert!(export_rows(&data, 2, 2 * (key.len() + 5)).is_err());
    assert_eq!(verify(0, 1).unwrap_err().exit_code(), 2);
    let fixture = Fixture::make();
    let path = fixture.root.join("request.json");
    let writable = File::create(&path).unwrap();
    assert_eq!(
        verify(writable.as_raw_fd(), fixture.sources.0.as_raw_fd())
            .unwrap_err()
            .exit_code(),
        2
    );
}

#[test]
#[ignore = "显式保存 test-only codec 归档供独立 CLI syscall 验证"]
fn save_native_fixture_for_cli() {
    let root = PathBuf::from(
        std::env::var_os("TAPE_RS_FACTS3_FIXTURE_OUTPUT").expect("明确指定独立输出目录"),
    );
    let fixture = Fixture::new(root);
    fs::write(
        fixture.root.join("request.json"),
        fixture.request.to_string(),
    )
    .unwrap();
    assert!(fixture.run().is_ok());
}
