//! 原链档案是部署者封存的输入，不能由运行副本更新。所有结论从真实制品和正式 codec 重算。
use super::super::{
    state::{Command, ControlSnapshot, ControlState},
    store::fixed_voters,
};
use super::*;
use protobuf::Message;
use raft::eraftpb::{ConfState, Entry, EntryType, HardState, Snapshot};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}
impl Artifact {
    pub fn read(&self) -> Result<Vec<u8>> {
        let bytes = stable_bytes(&self.path)?;
        if !sha(&self.sha256) || digest(&bytes) != self.sha256 {
            return Err(invalid("原链制品摘要不符"));
        }
        Ok(bytes)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceManifest {
    pub version: u32,
    pub node: u64,
    pub host: String,
    pub copy_manifest: Artifact,
    pub copy_root: PathBuf,
    pub baseline_raft: Artifact,
    pub baseline_libraries: Artifact,
    pub original_replay: Artifact,
}
fn stable_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    if !path.is_absolute() || std::fs::canonicalize(path)? != path {
        return Err(invalid("原链制品必须是无符号链接的绝对路径"));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NOATIME)
        .open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > 64 << 20 {
        return Err(invalid("原链制品类型或长度无效"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    let identity = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if identity(&before) != identity(&after)
        || identity(&before) != identity(&named)
        || named.file_type().is_symlink()
    {
        return Err(invalid("原链制品读取期间发生变化"));
    }
    Ok(bytes)
}
pub(super) struct Image {
    snapshot: Vec<u8>,
    decoded: Snapshot,
    hard: HardState,
    membership: Vec<u64>,
    local_instance: u64,
    entries: Vec<(u64, Vec<u8>, Entry)>,
}
pub(super) struct ReplayedImage {
    pub source: u64,
    pub directory_index: u64,
    pub controls: BTreeMap<String, ControlState>,
}

impl Image {
    fn read(path: &Path, immutable: bool) -> Result<Self> {
        if immutable {
            check_no_journal(path)?;
        }
        use std::os::unix::ffi::OsStrExt;
        let encoded: String = path
            .as_os_str()
            .as_bytes()
            .iter()
            .map(|b| format!("%{b:02X}"))
            .collect();
        let uri = format!(
            "file:{encoded}?mode=ro{}",
            if immutable { "&immutable=1" } else { "" }
        );
        let mut db = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| invalid(format!("只读原链连接失败：{e}")))?;
        // 在线来源须保留 SQLite 的真实 WAL 读取。
        Self::read_connection(&mut db)
    }

    pub(super) fn read_connection(db: &mut Connection) -> Result<Self> {
        // 同一 SQLite 事务固定 snapshot、HardState 和 entries 的视图。
        let tx = db.transaction().map_err(|e| invalid(e.to_string()))?;
        let blob = |table: &str| -> Result<Vec<u8>> {
            tx.query_row(&format!("SELECT data FROM {table} WHERE id=1"), [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| invalid(e.to_string()))?
            .ok_or_else(|| invalid(format!("原链缺少 {table}")))
        };
        let snapshot = blob("snapshot")?;
        let decoded = Snapshot::parse_from_bytes(&snapshot).map_err(|e| invalid(e.to_string()))?;
        let hard =
            HardState::parse_from_bytes(&blob("hardstate")?).map_err(|e| invalid(e.to_string()))?;
        let membership = fixed_voters(
            &ConfState::parse_from_bytes(&blob("membership")?)
                .map_err(|e| invalid(e.to_string()))?,
        )?;
        let sm = decoded.get_metadata();
        if fixed_voters(sm.get_conf_state())? != membership
            || sm.index == 0
            || sm.term == 0
            || sm.index > hard.commit
            || sm.term > hard.term
            || hard.vote != 0 && !membership.contains(&hard.vote)
        {
            return Err(invalid("原链 snapshot/HardState/membership 不一致"));
        }
        let has_instance:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_instance')",[],|r|r.get(0)).map_err(|e|invalid(e.to_string()))?;
        let local_instance = if has_instance {
            tx.query_row("SELECT value FROM local_instance WHERE id=1", [], |r| {
                r.get::<_, u64>(0)
            })
            .optional()
            .map_err(|e| invalid(e.to_string()))?
            .unwrap_or(0)
        } else {
            0
        };
        let mut stmt = tx
            .prepare("SELECT idx,data FROM entries WHERE idx>?1 ORDER BY idx")
            .map_err(|e| invalid(e.to_string()))?;
        let rows = stmt
            .query_map([sm.index], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| invalid(e.to_string()))?;
        let mut entries = Vec::new();
        let mut term = sm.term;
        for row in rows {
            let (idx, raw) = row.map_err(|e| invalid(e.to_string()))?;
            let entry = Entry::parse_from_bytes(&raw).map_err(|e| invalid(e.to_string()))?;
            if entries.len() >= 100_000
                || idx != sm.index + entries.len() as u64 + 1
                || entry.index != idx
                || entry.term < term
                || entry.term > hard.term
                || entry.get_entry_type() != EntryType::EntryNormal
            {
                return Err(invalid("原链日志不连续、任期无效或出现未审定成员变更"));
            }
            term = entry.term;
            entries.push((idx, raw, entry));
        }
        if hard.commit > entries.last().map_or(sm.index, |e| e.0) {
            return Err(invalid("原链 committed tail 不完整"));
        }
        drop(stmt);
        tx.commit().map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            snapshot,
            decoded,
            hard,
            membership,
            local_instance,
            entries,
        })
    }
    fn commands(&self) -> Vec<(u64, Vec<u8>)> {
        self.entries
            .iter()
            .filter(|(idx, _, e)| *idx <= self.hard.commit && !e.data.is_empty())
            .map(|(idx, _, e)| (*idx, e.data.to_vec()))
            .collect()
    }
    pub(super) fn snapshot_sha256(&self) -> String {
        digest(&self.snapshot)
    }

    pub(super) fn term(&self) -> u64 {
        self.hard.term
    }

    pub(super) fn commit(&self) -> u64 {
        self.hard.commit
    }

    pub(super) fn tail_sha256(&self) -> String {
        let mut hash = Sha256::new();
        for (idx, raw, _) in self.entries.iter().take_while(|e| e.0 <= self.hard.commit) {
            hash.update(idx.to_be_bytes());
            hash.update((raw.len() as u64).to_be_bytes());
            hash.update(raw);
        }
        format!("{:x}", hash.finalize())
    }

    pub(super) fn verify_baseline(
        &self,
        config: &LibrariesConfig,
        members: &[u64],
    ) -> Result<ReplayedImage> {
        if self.membership != members {
            return Err(invalid("原链固定成员与绑定不同"));
        }
        let original = self.replay(config)?;
        for state in original.controls.values() {
            if state.applied_index > self.commit()
                || state.executor.as_ref().is_some_and(|actor| {
                    !members.contains(&actor.node)
                        || actor.round == 0
                        || actor.round > state.applied_index
                        || actor.term == 0
                        || actor.term > self.term()
                        || state.executor_ready && !actor.fenced
                })
            {
                return Err(invalid("原链 actor、轮次或应用水位与固定成员/commit 不符"));
            }
        }
        Ok(original)
    }

    pub(super) fn verify_report(
        &self,
        original: &ReplayedImage,
        report: &Value,
        raft_sha256: &str,
        config_sha256: &str,
    ) -> Result<()> {
        if report["snapshot_blob_sha256"] != self.snapshot_sha256()
            || report["hardstate"]["commit"].as_u64() != Some(self.commit())
            || report["hardstate"]["term"].as_u64() != Some(self.term())
            || report["hardstate"]["vote"].as_u64() != Some(self.hard.vote)
            || report["input_sha256"]["raft.db"] != raft_sha256
            || report["input_sha256"]["libraries.json"] != config_sha256
            || report["tail_sha256"] != self.tail_sha256()
            || report["replay"]["replayed_controls"]
                != encode_controls(
                    &original.controls,
                    original.source,
                    original.directory_index,
                )?
        {
            return Err(invalid("原官方 codec 报告与实际完整 committed 回放不同"));
        }
        Ok(())
    }

    #[cfg(feature = "fdb")]
    pub(super) fn verify_peer_prefix(&self, other: &Self) -> Result<()> {
        let left = self.decoded.get_metadata();
        let right = other.decoded.get_metadata();
        let common_commit = self.commit().min(other.commit());
        if left.index != right.index
            || left.term != right.term
            || !self.same_prefix(other, common_commit)
        {
            return Err(invalid(
                "归档节点的共同 committed prefix 或 snapshot 水位不符",
            ));
        }
        Ok(())
    }

    fn same_prefix(&self, other: &Self, through: u64) -> bool {
        let committed = |e: &&(u64, Vec<u8>, Entry)| e.0 <= through;
        self.entries
            .iter()
            .take_while(committed)
            .map(|(idx, raw, _)| (idx, raw))
            .eq(other
                .entries
                .iter()
                .take_while(committed)
                .map(|(idx, raw, _)| (idx, raw)))
    }

    fn replay(&self, config: &LibrariesConfig) -> Result<ReplayedImage> {
        let (source, directory_index, controls) = super::super::multilib::readonly_control_replay(
            config,
            self.decoded.get_data(),
            &self.commands(),
        )?;
        if !self.membership.contains(&source) || directory_index > self.decoded.get_metadata().index
        {
            return Err(invalid("原链 snapshot 来源或目录水位无效"));
        }
        Ok(ReplayedImage {
            source,
            directory_index,
            controls,
        })
    }
}
fn copy_source(copy: &Value, host: &str, name: &str, source: &Path, seal: &FileSeal) -> Result<()> {
    let row = &copy[host][name];
    if row["path"].as_str() != source.to_str()
        || row["no_wal"] != true
        || row["sha256"] != seal.sha256
        || row["st_dev"].as_u64() != Some(seal.dev)
        || row["st_ino"].as_u64() != Some(seal.inode)
    {
        return Err(invalid("原链 copy manifest 与本节点来源身份不符"));
    }
    Ok(())
}
fn encode_controls(
    controls: &BTreeMap<String, ControlState>,
    source: u64,
    directory_index: u64,
) -> Result<Value> {
    controls
        .iter()
        .map(|(id, control)| {
            serde_json::from_slice(
                &ControlSnapshot {
                    control: control.clone(),
                    source,
                    directory_index,
                }
                .encode(),
            )
            .map(|v| (id.clone(), v))
            .map_err(|e| invalid(e.to_string()))
        })
        .collect::<Result<serde_json::Map<String, Value>>>()
        .map(Value::Object)
}
/// 对照封存的 snapshot/committed prefix，允许原正式 Coordinator 的无业务后继日志。
pub(super) struct VerifiedLineage {
    pub term: u64,
    pub controls: BTreeMap<String, ControlState>,
}

pub(super) fn verify(
    binding: &ControlOnlyBinding,
    config: &LibrariesConfig,
) -> Result<VerifiedLineage> {
    let bytes = stable_bytes(&binding.original_chain_manifest)?;
    if digest(&bytes) != binding.original_chain_sha256 {
        return Err(invalid("原链 manifest 的实际字节摘要不符"));
    }
    let manifest: EvidenceManifest =
        serde_json::from_slice(&bytes).map_err(|e| invalid(format!("原链 manifest 无效：{e}")))?;
    if manifest.version != 1 || manifest.node != binding.node || manifest.host.is_empty() {
        return Err(invalid("原链 manifest 节点或版本不符"));
    }
    let copy: Value = serde_json::from_slice(&manifest.copy_manifest.read()?)
        .map_err(|e| invalid(e.to_string()))?;
    for (host, files) in copy
        .as_object()
        .ok_or_else(|| invalid("copy manifest 不是对象"))?
    {
        if host.is_empty() || host.contains('/') || host == "." || host == ".." {
            return Err(invalid("copy manifest 主机名无效"));
        }
        for (name, row) in files
            .as_object()
            .ok_or_else(|| invalid("copy manifest 文件列表无效"))?
        {
            if Path::new(name)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(invalid("copy manifest 文件路径无效"));
            }
            let path = manifest.copy_root.join(host).join(name);
            check_no_journal(&path)?;
            let bytes = stable_bytes(&path)?;
            if row["no_wal"] != true
                || row["st_size"].as_u64() != Some(bytes.len() as u64)
                || row["sha256"] != digest(&bytes)
            {
                return Err(invalid("原只读 copy manifest 输入缺失或内容不符"));
            }
        }
    }
    let config_bytes = manifest.baseline_libraries.read()?;
    if config_bytes != stable_bytes(&binding.data_root.join("libraries.json"))? {
        return Err(invalid("原链配置档案与本节点 manifest 不同"));
    }
    let archived_config = LibrariesConfig::parse(
        &serde_json::from_slice(&config_bytes).map_err(|e| invalid(e.to_string()))?,
    )?;
    if archived_config.value() != config.value() {
        return Err(invalid("原链配置与运行库集合不同"));
    }
    copy_source(
        &copy,
        &manifest.host,
        "raft.db",
        &binding.data_root.join("raft.db"),
        &binding.raft,
    )?;
    copy_source(
        &copy,
        &manifest.host,
        "libraries.json",
        &binding.data_root.join("libraries.json"),
        &binding.manifest,
    )?;
    for (serial, library) in &binding.libraries {
        let path = config
            .library_dir(&binding.data_root, serial)
            .join("directory.db");
        let name = path
            .strip_prefix(&binding.data_root)
            .map_err(|e| invalid(e.to_string()))?
            .to_str()
            .ok_or_else(|| invalid("目录来源路径不是 UTF-8"))?;
        copy_source(&copy, &manifest.host, name, &path, &library.directory)?;
    }
    let archive_bytes = manifest.baseline_raft.read()?;
    if digest(&archive_bytes) != binding.raft.sha256 {
        return Err(invalid("原链 raft 档案与原 copy 来源摘要不同"));
    }
    let baseline = Image::read(&manifest.baseline_raft.path, true)?;
    if manifest.baseline_raft.read()? != archive_bytes {
        return Err(invalid("原链 raft 档案在回放期间变化"));
    }
    let members: Vec<_> = binding.members.keys().copied().collect();
    let original = baseline.verify_baseline(config, &members)?;

    let report: Value = serde_json::from_slice(&manifest.original_replay.read()?)
        .map_err(|e| invalid(e.to_string()))?;
    let report = report["nodes"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["node"].as_u64().map(|n| n.to_string()) == Some(manifest.host.clone()))
        })
        .ok_or_else(|| invalid("原官方回放报告缺少该节点"))?;
    baseline.verify_report(
        &original,
        report,
        &digest(&archive_bytes),
        &digest(&config_bytes),
    )?;
    let path = binding.data_root.join("raft.db");
    let before = std::fs::symlink_metadata(&path)?;
    if before.file_type().is_symlink()
        || before.dev() != binding.raft.dev
        || before.ino() != binding.raft.inode
    {
        return Err(invalid("当前原链 raft 文件身份已更换"));
    }
    let current = Image::read(&path, false)?;
    let after = std::fs::symlink_metadata(&path)?;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(invalid("当前 Raft 文件身份在回放期间变化"));
    }
    let replayed = verify_images(
        &baseline,
        &current,
        config,
        &members,
        &binding.group,
        &original.controls,
    )?;
    for (serial, expected) in &binding.libraries {
        if original.controls.get(serial).is_some_and(|old| {
            old.catalog_quiesce_index != Some(expected.quiesce_index)
                || old.catalog_frozen_index != Some(expected.frozen_index)
        }) {
            return Err(invalid("原链冻结点与绑定不同"));
        }
    }
    Ok(VerifiedLineage {
        term: current.term(),
        controls: replayed,
    })
}

pub(super) fn verify_images(
    baseline: &Image,
    current: &Image,
    config: &LibrariesConfig,
    members: &[u64],
    expected_group: &str,
    original: &BTreeMap<String, ControlState>,
) -> Result<BTreeMap<String, ControlState>> {
    if current.snapshot != baseline.snapshot
        || current.membership != members
        || current.local_instance < baseline.local_instance
        || (current.hard.term == baseline.hard.term
            && baseline.hard.vote != 0
            && current.hard.vote != baseline.hard.vote)
        || current.hard.term < baseline.hard.term
        || current.hard.commit < baseline.hard.commit
    {
        return Err(invalid(
            "当前 Raft 不是原链后继，或快照已越过封存基线且缺归档桥",
        ));
    }
    if !baseline.same_prefix(current, baseline.commit()) {
        return Err(invalid("原链 committed prefix 被修改或缺失"));
    }
    for (_, _, entry) in current
        .entries
        .iter()
        .filter(|(idx, _, _)| *idx > baseline.hard.commit)
    {
        if entry.data.is_empty() {
            continue;
        }
        let data: Value =
            serde_json::from_slice(&entry.data).map_err(|e| invalid(e.to_string()))?;
        let (scope, raw) = if data.get("cluster_version").is_some() {
            (
                data["library"]
                    .as_str()
                    .ok_or_else(|| invalid("后继缺少库作用域"))?,
                &data["command"],
            )
        } else {
            (config.default_library.as_str(), &data)
        };
        let command =
            Command::decode(&serde_json::to_vec(raw).map_err(|e| invalid(e.to_string()))?)
                .ok_or_else(|| invalid("原链后继命令损坏"))?;
        let diagnostic = format!("{command:?}, scope={scope}");
        match command {
            Command::Candidate {
                node,
                instance,
                sequence,
                group,
                eligible: false,
                drives: 0,
            } if members.contains(&node)
                && instance > 0
                && sequence > 0
                && group
                    == format!(
                        "{}:{}",
                        expected_group,
                        if scope == super::super::multilib::GATEWAY {
                            "gateway"
                        } else {
                            scope
                        }
                    ) => {}
            Command::Abandoned { node, round, .. }
                if original
                    .get(scope)
                    .and_then(|s| s.executor.as_ref())
                    .is_some_and(|e| e.node == node && e.round == round) => {}
            Command::ExecutorReport {
                node,
                instance,
                round,
                sequence,
                command,
            } if original.get(scope).is_some_and(|state| {
                state
                    .executor
                    .as_ref()
                    .is_some_and(|e| e.node == node && e.round == round)
                    && state.executor_instance == instance
                    && sequence > state.executor_report_sequence
            }) && matches!(*command,Command::Abandoned{node:inner,round:inner_round,..} if inner==node&&inner_round==round) =>
                {}
            _ => {
                return Err(invalid(format!(
                    "control-only 后继出现业务、外来候选或错误原轮次命令: {diagnostic}"
                )));
            }
        }
    }
    let replayed = current.replay(config)?.controls;
    if original.keys().ne(replayed.keys()) {
        return Err(invalid("原链后继库集合变化"));
    }
    for (serial, control) in &replayed {
        let old = &original[serial];
        if control.catalog_quiesce_index != old.catalog_quiesce_index
            || control.catalog_frozen_index != old.catalog_frozen_index
            || !control.pools.is_empty()
            || !control.tapes.is_empty()
        {
            return Err(invalid("原链后继改变 Frozen/quiesce 或含业务目录"));
        }
    }
    Ok(replayed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "只读取由主会话指定的本地封存证据；不访问 FDB、网络或设备"]
    fn control_only_offline_deployed_archive_matches_official_report() {
        let root = PathBuf::from(
            std::env::var_os("TAPE_RS_CONTROL_EVIDENCE_ROOT")
                .expect("explicit local evidence root"),
        );
        let copy: Value = serde_json::from_slice(
            &stable_bytes(&root.join("READONLY-COPY-MANIFEST.json")).unwrap(),
        )
        .unwrap();
        let report: Value =
            serde_json::from_slice(&stable_bytes(&root.join("decoder/REPLAY.stdout")).unwrap())
                .unwrap();
        for (host, files) in copy.as_object().unwrap() {
            let directory = root.join(host);
            for (name, row) in files.as_object().unwrap() {
                let bytes = stable_bytes(&directory.join(name)).unwrap();
                assert_eq!(row["sha256"], digest(&bytes));
                assert_eq!(row["st_size"].as_u64(), Some(bytes.len() as u64));
            }
            let config = LibrariesConfig::parse(
                &serde_json::from_slice(&stable_bytes(&directory.join("libraries.json")).unwrap())
                    .unwrap(),
            )
            .unwrap();
            let image = Image::read(&directory.join("raft.db"), true).unwrap();
            let controls = image.replay(&config).unwrap().controls;
            let source = report["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["node"].as_u64().unwrap().to_string() == *host)
                .unwrap();
            assert_eq!(source["snapshot_blob_sha256"], digest(&image.snapshot));
            assert_eq!(
                source["hardstate"]["commit"].as_u64(),
                Some(image.hard.commit)
            );
            let (node, index, _) = super::super::super::multilib::readonly_control_replay(
                &config,
                image.decoded.get_data(),
                &image.commands(),
            )
            .unwrap();
            assert_eq!(
                source["replay"]["replayed_controls"],
                encode_controls(&controls, node, index).unwrap()
            );
        }
    }
}
