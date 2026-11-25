//! 离线核对封存控制链与 app22 行。输出是组件证据，不能授权重置。
use super::{Directory, LibrariesConfig, digest, lineage, sha};
use crate::daemon::{directory::CutoverAudit, multilib::GATEWAY, state::ControlState};
use crate::error::TapeError;
use crate::managed_catalog::{
    ArchivedNativeExpectation, CatalogError, verify_archived_native_source,
};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, OpenHow, ResolveFlag, fcntl, openat2},
};
use rusqlite::{Connection, ffi};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, Metadata, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, PathBuf},
};
use uuid::Uuid;

const REQUEST_LIMIT: u64 = 16 << 10;
const METADATA_LIMIT: u64 = 64 << 10;
const REPORT_LIMIT: u64 = 8 << 20;
const IMAGE_LIMIT: u64 = 64 << 20;
const BUNDLE_PART_LIMIT: usize = 1 << 20;
const EXPORT_LIMIT: u64 = 64 << 20;
const FILES: [&str; 5] = [
    "cluster-directory.snap.db",
    "directory.db",
    "libraries.json",
    "raft.db",
    "status.json",
];
const BEGIN: &[u8] = b"tape-rs/catalog/22/";
const END: &[u8] = b"tape-rs/catalog/220";
const PROFILE: &str = "b-retained-current-owner-v1";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Source(String),
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Invalid(_) => 2,
            Self::Unsupported(_) => 3,
            Self::Source(_) => 4,
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

fn invalid(detail: impl Into<String>) -> Error {
    Error::Invalid(detail.into())
}

fn source(detail: impl Into<String>) -> Error {
    Error::Source(detail.into())
}

fn unsupported(detail: impl Into<String>) -> Error {
    Error::Unsupported(detail.into())
}

fn tape(stage: &str, error: TapeError) -> Error {
    if matches!(&error, TapeError::Io(_) | TapeError::Nix(_)) {
        source(format!("{stage}: {error}"))
    } else {
        invalid(format!("{stage}: {error}"))
    }
}

fn sqlite(stage: &str, error: rusqlite::Error) -> Error {
    invalid(format!("{stage}: {error}"))
}

fn duplicate(fd: RawFd, stage: &str) -> Result<File> {
    if fd <= 2 {
        return Err(invalid(format!("{stage}: 需要独立传入的 fd (>2)")));
    }
    let duplicated = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(3))
        .map_err(|e| invalid(format!("{stage}: fd {fd}: {e}")))?;
    // SAFETY: F_DUPFD_CLOEXEC 成功分配新 fd，File 独占关闭权。
    Ok(unsafe { File::from_raw_fd(duplicated) })
}

fn metadata(file: &File, stage: &str) -> Result<Metadata> {
    file.metadata().map_err(|e| source(format!("{stage}: {e}")))
}

fn identity(m: &Metadata) -> (u64, u64, u32, u64, u64, i64, i64, i64, i64) {
    (
        m.dev(),
        m.ino(),
        m.mode(),
        m.nlink(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

fn read_regular(file: &File, before: &Metadata, limit: u64, stage: &str) -> Result<Vec<u8>> {
    if !before.is_file() || before.len() == 0 {
        return Err(invalid(format!("{stage}: 输入必须为非空普通文件")));
    }
    if before.len() > limit {
        return Err(unsupported(format!("{stage}: 输入超过 {limit} 字节限制")));
    }
    let mut bytes = vec![0; before.len() as usize];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|e| source(format!("{stage}: {e}")))?;
    if identity(before) != identity(&metadata(file, stage)?) {
        return Err(source(format!("{stage}: 文件在读取期间变化")));
    }
    Ok(bytes)
}

struct Sources(File);

impl Sources {
    fn pin(&self, name: &str) -> Result<File> {
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_XDEV,
            );
        let fd = openat2(self.0.as_raw_fd(), name, how).map_err(|e| match e {
            Errno::ELOOP | Errno::EXDEV => invalid(format!("{name}: 来源是路径别名: {e}")),
            Errno::ENOSYS => unsupported(format!("{name}: 系统缺少 openat2: {e}")),
            _ => source(format!("{name}: 找不到或无法固定来源: {e}")),
        })?;
        // SAFETY: openat2 成功返回新 fd，File 独占关闭权。
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn read(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        if !sha(name) {
            return Err(invalid("证据引用必须是 64 位小写十六进制 SHA-256"));
        }
        let pin = self.pin(name)?;
        let before = metadata(&pin, name)?;
        if !before.is_file() || before.nlink() != 1 || before.mode() & 0o222 != 0 {
            return Err(invalid(format!(
                "{name}: 来源必须是只读普通文件，且无硬链接"
            )));
        }
        // 只在 O_PATH 验证类型后打开固定文件。SQLite 在内存读取校验后的字节。
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOATIME | nix::libc::O_NONBLOCK)
            .open(format!("/proc/self/fd/{}", pin.as_raw_fd()))
            .map_err(|e| source(format!("{name}: 固定 fd 无法读取: {e}")))?;
        if identity(&before) != identity(&metadata(&file, name)?) {
            return Err(source(format!("{name}: 读取 fd 与固定来源不同")));
        }
        let bytes = read_regular(&file, &before, limit, name)?;
        if identity(&before) != identity(&metadata(&self.pin(name)?, name)?) {
            return Err(source(format!("{name}: 名称在读取期间变化")));
        }
        if digest(&bytes) != name {
            return Err(invalid(format!("{name}: 来源 SHA-256 不符")));
        }
        Ok(bytes)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    profile: String,
    component: String,
    scope: String,
    challenge: String,
    control: ControlRequest,
    export: Option<ExportRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    serial: String,
    nodes: BTreeMap<String, u64>,
    baseline_manifest_sha256: String,
    current_manifest_sha256: String,
    report_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    records_sha256: String,
    read_version: u64,
    record_count: usize,
    byte_count: usize,
    namespace: String,
    operation: String,
    operation_epoch: u64,
    task_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CopySource {
    no_wal: bool,
    path: PathBuf,
    sha256: String,
    st_ctime_ns: u64,
    st_dev: u64,
    st_ino: u64,
    st_mtime_ns: u64,
    st_size: u64,
}

type CopyManifest = BTreeMap<String, BTreeMap<String, CopySource>>;

fn uuid(value: &str, stage: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == value)
        .ok_or_else(|| invalid(format!("{stage}: 需要规范非空 UUID")))
}

fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8], stage: &str) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| invalid(format!("{stage}: {e}")))
}

fn read_copy_manifest(
    sources: &Sources,
    hash: &str,
    nodes: &BTreeMap<String, u64>,
) -> Result<CopyManifest> {
    let bytes = sources.read(hash, METADATA_LIMIT)?;
    let manifest: CopyManifest = parse(&bytes, hash)?;
    if manifest.keys().ne(nodes.keys()) {
        return Err(invalid(format!("{hash}: copy manifest 主机集合不符")));
    }
    for (host, files) in &manifest {
        if files.keys().map(String::as_str).ne(FILES) {
            return Err(invalid(format!(
                "{hash}: {host} 必须具有固定的五个控制原件"
            )));
        }
        let mut identities = BTreeSet::new();
        for (name, row) in files {
            if !row.no_wal
                || row.st_dev == 0
                || row.st_ino == 0
                || row.st_size == 0
                || row.st_mtime_ns == 0
                || row.st_ctime_ns < row.st_mtime_ns
                || !sha(&row.sha256)
                || !row.path.is_absolute()
                || row
                    .path
                    .components()
                    .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
                || row.path != files["raft.db"].path.with_file_name(name)
                || !identities.insert((row.st_dev, row.st_ino))
            {
                return Err(invalid(format!(
                    "{hash}: {host}/{name} 原来源身份、摘要或 sidecar 声明无效"
                )));
            }
        }
    }
    Ok(manifest)
}

fn read_copy(sources: &Sources, row: &CopySource, limit: u64) -> Result<Vec<u8>> {
    let bytes = sources.read(&row.sha256, limit)?;
    if bytes.len() as u64 != row.st_size {
        return Err(invalid(format!("{}: copy 与原清单长度不同", row.sha256)));
    }
    Ok(bytes)
}

/// SQLite 只访问已校验摘要的有界内存。冻结 WAL 模式主文件只在工作副本降为 rollback header。
fn memory_sqlite(bytes: &[u8], stage: &str) -> Result<Connection> {
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return Err(invalid(format!("{stage}: SQLite 文件头损坏")));
    }
    let encoded_page_size = u16::from_be_bytes([bytes[16], bytes[17]]);
    let page_size = if encoded_page_size == 1 {
        65_536
    } else {
        usize::from(encoded_page_size)
    };
    let header_pages = u32::from_be_bytes(bytes[28..32].try_into().expect("固定页数字段"));
    if !(512..=65_536).contains(&page_size)
        || !page_size.is_power_of_two()
        || (header_pages as usize).checked_mul(page_size) != Some(bytes.len())
        || !matches!(&bytes[18..20], [1, 1] | [2, 2])
    {
        return Err(invalid(format!("{stage}: SQLite 页长度或文件版本不一致")));
    }
    let db = Connection::open_in_memory().map_err(|e| sqlite(stage, e))?;
    // SAFETY: SQLite 的缓冲区恰为已验页边界的非空输入长度。复制后仅规范化两个
    // 有效 WAL header 字节。FREEONCLOSE 将释放责任交给同一个 SQLite allocator，
    // bundled sqlite3.c 保证失败也释放。READONLY 且无 RESIZEABLE，Rust 不再接触缓冲区。
    let result = unsafe {
        let buffer = ffi::sqlite3_malloc64(bytes.len() as u64).cast::<u8>();
        if buffer.is_null() {
            return Err(source(format!("{stage}: SQLite 工作缓冲区分配失败")));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
        buffer.add(18).write(1);
        buffer.add(19).write(1);
        ffi::sqlite3_deserialize(
            db.handle(),
            c"main".as_ptr(),
            buffer,
            bytes.len() as i64,
            bytes.len() as i64,
            ffi::SQLITE_DESERIALIZE_READONLY | ffi::SQLITE_DESERIALIZE_FREEONCLOSE,
        )
    };
    if result != ffi::SQLITE_OK {
        return Err(invalid(format!(
            "{stage}: SQLite 只读内存装载返回 {result}"
        )));
    }
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY")
        .map_err(|e| sqlite(stage, e))?;
    Ok(db)
}

fn check_tables(db: &Connection, tables: &[&str], stage: &str) -> Result<()> {
    for table in tables {
        let real_table: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='main' AND name=?1 AND type='table')",
                [table], |r| r.get(0),
            )
            .map_err(|e| sqlite(stage, e))?;
        if !real_table {
            return Err(invalid(format!("{stage}: 缺少实际 SQLite 表 {table}")));
        }
    }
    let integrity: String = db
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(|e| sqlite(stage, e))?;
    if integrity != "ok" {
        return Err(invalid(format!("{stage}: SQLite quick_check: {integrity}")));
    }
    Ok(())
}

fn image(bytes: &[u8], stage: &str) -> Result<lineage::Image> {
    let mut db = memory_sqlite(bytes, stage)?;
    check_tables(
        &db,
        &["snapshot", "hardstate", "membership", "entries"],
        stage,
    )?;
    lineage::Image::read_connection(&mut db).map_err(|e| tape(stage, e))
}

fn directory_audit(bytes: &[u8], frozen: u64, ceiling: u64, stage: &str) -> Result<CutoverAudit> {
    let db = memory_sqlite(bytes, stage)?;
    check_tables(
        &db,
        &["meta", "pools", "tapes", "files", "staging", "file_tasks"],
        stage,
    )?;
    let audit = Directory::audit_readonly_connection(&db).map_err(|e| tape(stage, e))?;
    if audit.frozen_index != Some(frozen)
        || audit.applied_index > ceiling
        || audit.has_catalog_rows
        || audit.has_staging
        || audit.has_unresolved_file_tasks
    {
        return Err(invalid(format!("{stage}: 冻结目录水位或业务行不符")));
    }
    Ok(audit)
}

fn bundle_audit(
    bytes: &[u8],
    serial: &str,
    frozen: u64,
    ceiling: u64,
    stage: &str,
) -> Result<(CutoverAudit, String)> {
    let db = memory_sqlite(bytes, stage)?;
    check_tables(&db, &["meta", "libraries"], stage)?;
    let applied: u64 = db
        .query_row("SELECT v FROM meta WHERE k='applied_index'", [], |r| {
            r.get(0)
        })
        .map_err(|e| sqlite(stage, e))?;
    let mut stmt = db
        .prepare("SELECT id,part,data FROM libraries ORDER BY id,part")
        .map_err(|e| sqlite(stage, e))?;
    let mut rows = stmt.query([]).map_err(|e| sqlite(stage, e))?;
    let mut embedded = Vec::new();
    let mut sequence = 0u64;
    while let Some(row) = rows.next().map_err(|e| sqlite(stage, e))? {
        let id: String = row.get(0).map_err(|e| sqlite(stage, e))?;
        let part: u64 = row.get(1).map_err(|e| sqlite(stage, e))?;
        let raw = row
            .get_ref(2)
            .and_then(|r| r.as_blob().map_err(Into::into))
            .map_err(|e| sqlite(stage, e))?;
        if id != serial || part != sequence || raw.is_empty() || raw.len() > BUNDLE_PART_LIMIT {
            return Err(invalid(format!(
                "{stage}: 目录包库身份、分片编号或分片长度无效"
            )));
        }
        if embedded.len() + raw.len() > IMAGE_LIMIT as usize {
            return Err(unsupported(format!("{stage}: 目录包内嵌库超过限制")));
        }
        embedded.extend_from_slice(raw);
        sequence += 1;
    }
    let audit = directory_audit(&embedded, frozen, ceiling, stage)?;
    if audit.applied_index != applied {
        return Err(invalid(format!("{stage}: 内嵌目录与目录包水位不符")));
    }
    Ok((audit, digest(&embedded)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Actor {
    node: u64,
    term: u64,
    round: u64,
    instance: u64,
}

impl Actor {
    fn from_control(control: &ControlState) -> Option<Self> {
        let actor = control.executor.as_ref()?;
        Some(Self {
            node: actor.node,
            term: actor.term,
            round: actor.round,
            instance: control.executor_instance,
        })
    }

    fn value(&self) -> Value {
        json!({"node":self.node, "term":self.term, "round":self.round, "instance":self.instance})
    }
}

fn actor(controls: &BTreeMap<String, ControlState>, serial: &str, stage: &str) -> Result<Actor> {
    let library = controls
        .get(serial)
        .ok_or_else(|| invalid(format!("{stage}: 原 B 库回放缺失")))?;
    let actor = Actor::from_control(library)
        .filter(|a| a.instance > 0 && library.executor.as_ref().is_some_and(|e| e.fenced))
        .ok_or_else(|| invalid(format!("{stage}: 原 B 库缺少有效的 Fenced actor")))?;
    if controls
        .get(GATEWAY)
        .and_then(Actor::from_control)
        .is_some_and(|gateway| gateway.round == actor.round)
    {
        return Err(invalid(format!(
            "{stage}: 原 B 库与 gateway 混用了 actor 轮次"
        )));
    }
    Ok(actor)
}

fn audit_value(audit: CutoverAudit) -> Value {
    json!({"applied_index":audit.applied_index, "frozen_index":audit.frozen_index,
        "has_catalog_rows":audit.has_catalog_rows, "has_staging":audit.has_staging,
        "has_unresolved_file_tasks":audit.has_unresolved_file_tasks})
}

fn read_config(
    sources: &Sources,
    files: &BTreeMap<String, CopySource>,
    node: u64,
    serial: &str,
) -> Result<LibrariesConfig> {
    let bytes = read_copy(sources, &files["libraries.json"], METADATA_LIMIT)?;
    let config =
        LibrariesConfig::parse(&parse(&bytes, "原多库配置")?).map_err(|e| tape("原多库配置", e))?;
    if config.libraries.len() != 1 || config.default_library != serial {
        return Err(unsupported("仅支持 B 库为唯一配置库的归档"));
    }
    let status: Value = parse(
        &read_copy(sources, &files["status.json"], METADATA_LIMIT)?,
        "原 status.json",
    )?;
    if status["id"].as_u64() != Some(node) || status["default_library"] != serial {
        return Err(invalid("原 status.json 的节点或库标识与清单不同"));
    }
    Ok(config)
}

fn verify_directory_sources(
    sources: &Sources,
    files: &BTreeMap<String, CopySource>,
    serial: &str,
    frozen: u64,
    commit: u64,
    snapshot_directory_index: u64,
    stage: &str,
) -> Result<Value> {
    let directory = &files["directory.db"];
    let audit = directory_audit(
        &read_copy(sources, directory, IMAGE_LIMIT)?,
        frozen,
        commit,
        stage,
    )?;
    let bundle = &files["cluster-directory.snap.db"];
    let (bundle_audit, embedded_sha256) = bundle_audit(
        &read_copy(sources, bundle, IMAGE_LIMIT)?,
        serial,
        frozen,
        snapshot_directory_index.min(audit.applied_index),
        stage,
    )?;
    Ok(json!({
        "directory_sha256":directory.sha256,
        "directory":audit_value(audit),
        "bundle_sha256":bundle.sha256,
        "bundle_directory_sha256":embedded_sha256,
        "bundle_directory":audit_value(bundle_audit)
    }))
}

#[derive(PartialEq, Eq)]
struct ControlAuthority {
    actor: Actor,
    current_actor: Option<Actor>,
    quiesce: u64,
    frozen: u64,
}

fn advance_reference(
    reference: &mut Option<lineage::Image>,
    current: lineage::Image,
    stage: &str,
) -> Result<()> {
    if let Some(prior) = reference {
        prior
            .verify_peer_prefix(&current)
            .map_err(|e| tape(stage, e))?;
        if prior.commit() >= current.commit() {
            return Ok(());
        }
    }
    *reference = Some(current);
    Ok(())
}

fn verify_controls(
    sources: &Sources,
    control: &ControlRequest,
    members: &[u64],
) -> Result<(ControlAuthority, Vec<Value>)> {
    let baseline = read_copy_manifest(sources, &control.baseline_manifest_sha256, &control.nodes)?;
    let current = read_copy_manifest(sources, &control.current_manifest_sha256, &control.nodes)?;
    let report: Value = parse(
        &sources.read(&control.report_sha256, REPORT_LIMIT)?,
        "原回放报告",
    )?;
    let report_rows = report["nodes"]
        .as_array()
        .filter(|rows| rows.len() == 3)
        .ok_or_else(|| invalid("原回放报告缺少三节点"))?;
    let mut common = None;
    let mut config_hash = None;
    let mut results = Vec::new();
    let (mut baseline_reference, mut current_reference) = (None, None);
    for (host, &node) in &control.nodes {
        let baseline_files = &baseline[host];
        let current_files = &current[host];
        let stage = format!("host {host}/node {node}");
        let host_config = &baseline_files["libraries.json"].sha256;
        if config_hash.is_some_and(|expected| expected != host_config) {
            return Err(invalid(format!("{stage}: 三节点原库配置不同")));
        }
        config_hash = Some(host_config);
        for name in FILES {
            let old = &baseline_files[name];
            let new = &current_files[name];
            if old.path != new.path
                || (name != "status.json" && (old.st_dev, old.st_ino) != (new.st_dev, new.st_ino))
                || (name != "status.json"
                    && (new.st_ctime_ns < old.st_ctime_ns || new.st_mtime_ns < old.st_mtime_ns))
                || (name == "libraries.json" && old.sha256 != new.sha256)
            {
                return Err(invalid(format!("{stage}: {name} 原路径、身份或配置变化")));
            }
        }
        let serial = control.serial.as_str();
        let config = read_config(sources, baseline_files, node, serial)?;
        read_config(sources, current_files, node, serial)?;
        let group = digest(config.value().to_string().as_bytes());
        let old_raft = &baseline_files["raft.db"];
        let new_raft = &current_files["raft.db"];
        let old = image(&read_copy(sources, old_raft, IMAGE_LIMIT)?, &stage)?;
        let new = image(&read_copy(sources, new_raft, IMAGE_LIMIT)?, &stage)?;
        let report_row = report_rows
            .iter()
            .find(|row| {
                row["node"]
                    .as_u64()
                    .is_some_and(|observed| observed.to_string() == *host)
            })
            .ok_or_else(|| invalid(format!("{stage}: 原官方回放报告没有该主机")))?;
        for name in ["directory.db", "cluster-directory.snap.db"] {
            if report_row["input_sha256"][name] != baseline_files[name].sha256 {
                return Err(invalid(format!(
                    "{stage}: 官方回放的 {name} 摘要与原件不符"
                )));
            }
        }
        let original = old
            .verify_baseline(&config, members)
            .map_err(|e| tape(&stage, e))?;
        old.verify_report(
            &original,
            report_row,
            &old_raft.sha256,
            &baseline_files["libraries.json"].sha256,
        )
        .map_err(|e| tape(&stage, e))?;
        let replayed =
            lineage::verify_images(&old, &new, &config, members, &group, &original.controls)
                .map_err(|e| tape(&stage, e))?;
        let baseline_actor = actor(&original.controls, serial, &stage)?;
        let current_actor = Actor::from_control(&replayed[serial]);
        if current_actor.as_ref().is_some_and(|a| a != &baseline_actor)
            || current_actor.is_some()
                && !replayed[serial].executor.as_ref().is_some_and(|e| e.fenced)
        {
            return Err(invalid(format!(
                "{stage}: 后继 B 库执行者不等于原 Fenced actor"
            )));
        }
        let state = &original.controls[serial];
        let (quiesce, frozen) = match (state.catalog_quiesce_index, state.catalog_frozen_index) {
            (Some(q), Some(f)) if q > 0 && q <= f && f <= state.applied_index => (q, f),
            _ => return Err(invalid(format!("{stage}: 原 B 库缺少有效的冻结/暂停水位"))),
        };
        let authority = ControlAuthority {
            actor: baseline_actor,
            current_actor,
            quiesce,
            frozen,
        };
        if common
            .as_ref()
            .is_some_and(|expected| expected != &authority)
        {
            return Err(invalid(format!(
                "{stage}: 三节点原 actor、当前 actor 或冻结点不一致"
            )));
        }
        common = Some(authority);
        let (snapshot_source, snapshot_directory_index) =
            (original.source, original.directory_index);
        let baseline_audit = verify_directory_sources(
            sources,
            baseline_files,
            serial,
            frozen,
            old.commit(),
            snapshot_directory_index,
            &stage,
        )?;
        let current_audit = verify_directory_sources(
            sources,
            current_files,
            serial,
            frozen,
            new.commit(),
            snapshot_directory_index,
            &stage,
        )?;
        let audit = json!({"baseline": baseline_audit, "current": current_audit});
        results.push(json!({
            "host":host, "node":node,
            "baseline_sha256":old_raft.sha256, "current_sha256":new_raft.sha256,
            "snapshot_sha256":new.snapshot_sha256(), "snapshot_source":snapshot_source,
            "snapshot_directory_index":snapshot_directory_index,
            "baseline_tail_sha256":old.tail_sha256(), "tail_sha256":new.tail_sha256(),
            "baseline_commit":old.commit(), "commit":new.commit(), "term":new.term(),
            "quiesce":quiesce, "frozen":frozen,
            "audit_sha256":digest(audit.to_string().as_bytes()), "audit":audit
        }));
        advance_reference(&mut baseline_reference, old, &stage)?;
        advance_reference(&mut current_reference, new, &stage)?;
    }
    Ok((
        common.ok_or_else(|| invalid("原控制链没有可核对的节点"))?,
        results,
    ))
}

fn field<'a>(bytes: &mut &'a [u8], limit: usize) -> Result<&'a [u8]> {
    let (len, remaining) = bytes
        .split_first_chunk::<4>()
        .ok_or_else(|| invalid("app22 export 截断的帧长度"))?;
    let len = u32::from_be_bytes(*len) as usize;
    if len > limit {
        return Err(unsupported("app22 export 帧超过支持的字段长度"));
    }
    let (value, rest) = remaining
        .split_at_checked(len)
        .ok_or_else(|| invalid("app22 export 截断的帧正文"))?;
    *bytes = rest;
    Ok(value)
}

fn export_rows(
    mut bytes: &[u8],
    count: usize,
    bytes_expected: usize,
) -> Result<BTreeMap<&[u8], &[u8]>> {
    let mut rows = BTreeMap::new();
    let mut byte_count = 0;
    let mut previous = None;
    while !bytes.is_empty() {
        let key = field(&mut bytes, 65_536)?;
        let value = field(&mut bytes, 8 << 20)?;
        if key < BEGIN || key >= END || previous.is_some_and(|prior| key <= prior) {
            return Err(invalid("app22 export 键越界、乱序或重复"));
        }
        if rows.len() >= 100_000 {
            return Err(unsupported("app22 export 超过行数限制"));
        }
        byte_count += key.len() + value.len();
        previous = Some(key);
        rows.insert(key, value);
    }
    if rows.len() != count || byte_count != bytes_expected {
        return Err(invalid("app22 export 行数或字节数与输入声明不同"));
    }
    Ok(rows)
}

fn verify_export(
    sources: &Sources,
    export: &ExportRequest,
    serial: &str,
    authority: &ControlAuthority,
    members: &[u64],
) -> Result<Value> {
    let namespace = uuid(&export.namespace, "旧 namespace")?;
    let operation = uuid(&export.operation, "原 operation")?;
    if export.read_version == 0 || export.operation_epoch == 0 || !sha(&export.task_sha256) {
        return Err(invalid(
            "app22 export read version、原操作 epoch 或任务摘要无效",
        ));
    }
    let bytes = sources.read(&export.records_sha256, EXPORT_LIMIT)?;
    let rows = export_rows(&bytes, export.record_count, export.byte_count)?;
    let owner = authority.actor.round;
    let (active, task_sha256) = verify_archived_native_source(
        &rows,
        ArchivedNativeExpectation {
            serial,
            namespace,
            operation_epoch: export.operation_epoch,
            operation,
            owner,
            frozen: authority.frozen,
            quiesce: authority.quiesce,
            members,
        },
    )
    .map_err(|e| {
        let detail = match e {
            CatalogError::Invalid(detail) | CatalogError::Corrupt(detail) => detail,
            CatalogError::Incomplete => "缺少必须的归档原始行",
            _ => "离线 codec 返回了意外的错误分类",
        };
        invalid(format!("归档 FDB 原生控制与任务: {detail}"))
    })?;
    if task_sha256 != export.task_sha256 {
        return Err(invalid("归档原 reclaim 任务摘要不符"));
    }
    let certificate_sha256: String = active
        .certificate_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(json!({
        "old_namespace":namespace.to_string(), "route_revision":active.route.revision,
        "certificate_sha256":certificate_sha256, "initial_owner_epoch":active.initial_owner_epoch,
        "owner_round":owner, "operation":operation.to_string(),
        "operation_epoch":export.operation_epoch, "task_sha256":task_sha256,
        "records_sha256":export.records_sha256, "claimed_read_version":export.read_version,
        "record_count":export.record_count, "byte_count":export.byte_count
    }))
}

fn verify_request(request_bytes: &[u8], sources: &Sources) -> Result<Value> {
    let wire: Value = parse(request_bytes, "组件请求")?;
    if wire.to_string().as_bytes() != request_bytes || wire.get("export").is_none() {
        return Err(invalid("组件请求必须为规范 JSON 并显式声明 export"));
    }
    let request: Request = parse(request_bytes, "组件请求")?;
    if request.version != 1
        || request.profile != PROFILE
        || !matches!(request.scope.as_str(), "field" | "private")
    {
        return Err(unsupported("组件请求版本、profile 或 scope 不受支持"));
    }
    match (request.component.as_str(), request.export.is_some()) {
        ("control-only", false) | ("control-fdb", true) => {}
        ("control-only" | "control-fdb", _) => return Err(invalid("组件类型与 export 不符")),
        _ => return Err(unsupported("组件类型不受支持")),
    }
    uuid(&request.challenge, "challenge")?;
    let control = &request.control;
    if control.nodes.len() != 3
        || control.nodes.iter().any(|(host, node)| {
            !host
                .parse::<u64>()
                .is_ok_and(|n| n > 0 && n.to_string() == *host)
                || !(1..=255).contains(node)
        })
    {
        return Err(invalid("需要三个不同的规范 host 与 Raft node"));
    }
    let mut members: Vec<_> = control.nodes.values().copied().collect();
    members.sort_unstable();
    if members.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("三个 host 必须映射到三个不同的 Raft node"));
    }
    if request.scope == "field"
        && (control.serial != "55L3A78011W9LL01"
            || control
                .nodes
                .keys()
                .map(String::as_str)
                .ne(["207", "208", "209"]))
    {
        return Err(unsupported("field scope 仅支持封存 B 库与其三个固定主机"));
    }
    let (authority, controls) = verify_controls(sources, control, &members)?;
    let export = request
        .export
        .as_ref()
        .map(|e| verify_export(sources, e, &control.serial, &authority, &members))
        .transpose()?;
    Ok(json!({
        "schema":"fresh.native.component.summary.v1",
        "challenge":request.challenge, "scope":request.scope, "component":request.component,
        "request_sha256":digest(request_bytes),
        "serial":control.serial, "actor":authority.actor.value(),
        "current_actor":authority.current_actor.as_ref().map(Actor::value),
        "baseline_manifest_sha256":control.baseline_manifest_sha256,
        "current_manifest_sha256":control.current_manifest_sha256, "report_sha256":control.report_sha256,
        "sqlite_reader":"readonly-memory-rollback-header-v1", "controls":controls, "export":export
    }))
}

/// 读取固定请求 fd 和 SHA 命名目录 fd。结果只适用于离线组件检查。
pub fn verify(request_fd: RawFd, evidence_dir_fd: RawFd) -> Result<Value> {
    if request_fd == evidence_dir_fd {
        return Err(invalid("请求 fd 与证据目录 fd 必须不同"));
    }
    let request = duplicate(request_fd, "请求输入")?;
    let flags = fcntl(request.as_raw_fd(), FcntlArg::F_GETFL)
        .map(OFlag::from_bits_truncate)
        .map_err(|e| source(format!("请求 fd: {e}")))?;
    if flags.intersects(OFlag::O_ACCMODE | OFlag::O_PATH) {
        return Err(invalid("请求 fd 必须以只读文件模式打开"));
    }
    let dir = duplicate(evidence_dir_fd, "证据目录")?;
    if !metadata(&dir, "证据目录")?.is_dir() {
        return Err(invalid("证据输入必须是目录 fd"));
    }
    let bytes = read_regular(
        &request,
        &metadata(&request, "请求输入")?,
        REQUEST_LIMIT,
        "请求输入",
    )?;
    verify_request(&bytes, &Sources(dir))
}

#[cfg(test)]
#[path = "archive_verify_tests.rs"]
mod tests;
