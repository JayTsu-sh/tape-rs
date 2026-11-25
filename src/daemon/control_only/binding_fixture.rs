//! 软件测试专用：封存实际 SQLite/protobuf 输入，不授予设备权限。
use super::*;
use protobuf::Message as _;

pub fn seal(binding: &mut ControlOnlyBinding, config: &LibrariesConfig) {
    let root = &binding.data_root;
    let archive = root.join(format!("lineage-evidence-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&archive).unwrap();
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let host = binding.node.to_string();
    let mut copy = serde_json::Map::new();
    for name in std::iter::once("raft.db".to_owned())
        .chain(std::iter::once("libraries.json".to_owned()))
        .chain(config.libraries.iter().map(|l| {
            config
                .library_dir(root, &l.changer_serial)
                .join("directory.db")
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        }))
    {
        let path = root.join(&name);
        let seal = FileSeal::read(&path).unwrap();
        let m = std::fs::metadata(&path).unwrap();
        copy.insert(name.clone(),serde_json::json!({"path":path,"no_wal":true,"sha256":seal.sha256,"st_dev":seal.dev,"st_ino":seal.inode,"st_size":m.len()}));
        let dest = archive.join(&host).join(&name);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(path, dest).unwrap();
    }
    binding.raft = FileSeal::read(&root.join("raft.db")).unwrap();
    let copy_value = serde_json::json!({host.clone():copy});
    let copy_path = archive.join("READONLY-COPY-MANIFEST.json");
    std::fs::write(&copy_path, serde_json::to_vec(&copy_value).unwrap()).unwrap();
    let db = rusqlite::Connection::open_with_flags(
        root.join("raft.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let snap: Vec<u8> = db
        .query_row("SELECT data FROM snapshot WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let snap = raft::eraftpb::Snapshot::parse_from_bytes(&snap).unwrap();
    let hard: Vec<u8> = db
        .query_row("SELECT data FROM hardstate WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let hard = raft::eraftpb::HardState::parse_from_bytes(&hard).unwrap();
    let rows = db
        .prepare("SELECT idx,data FROM entries WHERE idx>?1 AND idx<=?2 ORDER BY idx")
        .unwrap()
        .query_map([snap.get_metadata().index, hard.commit], |r| {
            Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let mut tail = Sha256::new();
    for (idx, raw) in rows {
        let entry = raft::eraftpb::Entry::parse_from_bytes(&raw).unwrap();
        assert!(
            entry.data.is_empty()
                && entry.get_entry_type() == raft::eraftpb::EntryType::EntryNormal,
            "fixture generator only handles complete snapshot plus empty entries; production verifier always fully replays"
        );
        tail.update(idx.to_be_bytes());
        tail.update((raw.len() as u64).to_be_bytes());
        tail.update(raw);
    }
    let tail_sha = format!("{:x}", tail.finalize());
    let raw_snap: Vec<u8> = db
        .query_row("SELECT data FROM snapshot WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(snap.get_data()).unwrap();
    let report = serde_json::json!({"nodes":[{"node":binding.node,"snapshot_blob_sha256":hash(&raw_snap),"hardstate":{"commit":hard.commit,"term":hard.term,"vote":hard.vote},"input_sha256":{"raft.db":hash(&std::fs::read(root.join("raft.db")).unwrap()),"libraries.json":hash(&std::fs::read(root.join("libraries.json")).unwrap())},"tail_sha256":tail_sha,"replay":{"replayed_controls":value["controls"]}}]});
    let report_path = archive.join("REPLAY.json");
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    let artifact = |path: PathBuf| serde_json::json!({"sha256":hash(&std::fs::read(&path).unwrap()),"path":path});
    let manifest = serde_json::json!({"version":1,"node":binding.node,"host":host,"copy_manifest":artifact(copy_path),"copy_root":archive,"baseline_raft":artifact(archive.join(&host).join("raft.db")),"baseline_libraries":artifact(archive.join(&host).join("libraries.json")),"original_replay":artifact(report_path)});
    let path = archive.join("evidence.json");
    let bytes = serde_json::to_vec(&manifest).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    binding.original_chain_manifest = path;
    binding.original_chain_sha256 = hash(&bytes);
}
