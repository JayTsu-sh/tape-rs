//! Active media intake. The pool gate hides provisional nodes until the whole
//! immutable source is projected; a durable undo journal supports cancellation.
use super::*;
use crate::catalog::volume_extents::ExpectedIndex;
use crate::ltfs::recovery::IndexSourceIdentity;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "native_media_load.rs"]
pub(super) mod load;
#[path = "native_media_refresh.rs"]
mod refresh;
#[path = "native_media_target.rs"]
mod target;
pub use load::{
    NativeMediaEvictionPlan, NativeMediaLoadPhase, NativeMediaLoadPlan, NativeMediaLoadProgress,
};

#[derive(Clone, Debug)]
pub struct NativeMediaImport {
    request: ManagementRequest,
    route: ActiveLibraryRoute,
    refresh: Option<refresh::Refresh>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeMediaSource {
    pub identity: IndexSourceIdentity,
    /// All physical file entries, including hidden EE data objects and links.
    pub files: u64,
    /// Logical import candidates; independent of the physical file count.
    pub paths: u64,
}

impl NativeMediaSource {
    pub fn index(&self) -> ExpectedIndex {
        index_evidence::expected_index(&self.identity)
    }
}

impl NativeMediaImport {
    pub fn entry(
        &self,
        spool: &ImportSpool,
        candidate: ImportCandidate,
    ) -> Result<StagedImportEntry> {
        let (attributes, uid, data) = staged_import::source_evidence(spool, &candidate)?;
        Ok(StagedImportEntry {
            path: ReconciledPath {
                pool_id: self.pool(),
                logical_path: candidate.logical_path.clone(),
                sources: vec![SourceCandidate {
                    barcode: self.barcode().into(),
                    volume_id: spool.source().volume_uuid,
                    generation: spool.source().generation,
                    candidate,
                }],
                winner: 0,
            },
            current_attributes: attributes,
            source_file_uids: vec![uid],
            source_data: Some(vec![data]),
        })
    }
    pub fn assigned_volume(&self) -> Result<Option<Uuid>> {
        self.refresh
            .as_ref()
            .map(refresh::Refresh::volume)
            .transpose()
    }
    pub fn request(&self) -> &ManagementRequest {
        &self.request
    }
    pub fn route(&self) -> ActiveLibraryRoute {
        self.route
    }

    pub fn pool(&self) -> Uuid {
        Uuid::parse_str(
            self.request.value()["change"]["pool"]
                .as_str()
                .expect("validated pool"),
        )
        .expect("validated pool")
    }
    pub fn serial(&self) -> &str {
        self.request.value()["library"]
            .as_str()
            .expect("validated library")
    }
    pub fn barcode(&self) -> &str {
        self.request.value()["change"]["barcode"]
            .as_str()
            .expect("validated barcode")
    }
    pub fn scan(&self) -> Uuid {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/native-media-scan/1");
        hash.update(self.request.namespace().as_bytes());
        hash.update(self.request.operation().as_bytes());
        hash.update(self.request.epoch().to_be_bytes());
        let mut bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("SHA prefix");
        bytes[6] = (bytes[6] & 15) | 0x80;
        bytes[8] = (bytes[8] & 63) | 0x80;
        Uuid::from_bytes(bytes)
    }
    fn token(&self) -> Vec<u8> {
        [
            self.request.namespace().as_bytes().as_slice(),
            self.pool().as_bytes().as_slice(),
            &self.request.epoch().to_be_bytes(),
            self.request.operation().as_bytes().as_slice(),
        ]
        .concat()
    }
    fn gate(&self) -> Vec<u8> {
        pool_gate(self.request.namespace(), self.pool())
    }
    fn journal(&self) -> Vec<u8> {
        let mut key = management::prefix(self.request.namespace(), b'j');
        append_component(&mut key, self.serial().as_bytes());
        key.extend_from_slice(&self.request.epoch().to_be_bytes());
        key.extend_from_slice(self.pool().as_bytes());
        key.extend_from_slice(self.request.operation().as_bytes());
        key
    }
    fn row(&self, kind: u8, path: &str) -> Vec<u8> {
        let mut key = self.journal();
        key.push(kind);
        key.extend_from_slice(&Sha256::digest(path.as_bytes()));
        key
    }
}

pub(super) fn volume_claim_key(volume: Uuid) -> Vec<u8> {
    let mut k = key(b'%');
    k.extend_from_slice(volume.as_bytes());
    k
}
fn pool_gate(ns: Uuid, pool: Uuid) -> Vec<u8> {
    let mut k = management::prefix(ns, b'i');
    k.extend_from_slice(pool.as_bytes());
    k
}
pub(crate) async fn require_pool_readable(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<()> {
    if tr.get(&pool_gate(ns, pool), false).await?.is_some() {
        return Err(CatalogError::PoolImporting);
    }
    native_device::require_pool_available(tr, ns, pool).await?;
    Ok(())
}
fn encode(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}
fn decode(bytes: &[u8]) -> Result<Value> {
    let v: Value =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("原生介质状态 JSON"))?;
    if encode(&v) != bytes || v["version"] != 1 {
        return Err(CatalogError::Corrupt("原生介质状态格式"));
    }
    Ok(v)
}
pub(super) fn assigned_base_key(ns: Uuid, volume: Uuid) -> Vec<u8> {
    refresh::base_key(ns, volume)
}
pub(super) fn source_value(source: &NativeMediaSource) -> Value {
    json!({"identity":source.identity.to_bytes(),"files":source.files,"paths":source.paths})
}

fn write_undo(
    tr: &Transaction,
    import: &NativeMediaImport,
    kind: u8,
    path: &str,
    fingerprint: &str,
    writes: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    let value = json!({"version":1,"kind":String::from_utf8(vec![kind]).map_err(|_|CatalogError::Corrupt("介质回滚类型"))?,"path":path,"fingerprint":fingerprint,"keys":writes.iter().map(|(key,value)|(key.clone(),Sha256::digest(value).to_vec())).collect::<Vec<_>>()});
    decode_journal(&encode(&value))?;
    tr.set(&import.row(kind, path), &encode(&value));
    Ok(())
}
fn source(v: &Value) -> Result<Option<NativeMediaSource>> {
    if v.is_null() {
        return Ok(None);
    }
    let bad = || CatalogError::Corrupt("原生介质来源身份");
    let bytes: Vec<u8> = serde_json::from_value(v["identity"].clone()).map_err(|_| bad())?;
    let s = NativeMediaSource {
        identity: IndexSourceIdentity::from_bytes(&bytes).map_err(|_| bad())?,
        files: v["files"].as_u64().ok_or_else(bad)?,
        paths: v["paths"].as_u64().ok_or_else(bad)?,
    };
    if source_value(&s) != *v {
        return Err(bad());
    }
    Ok(Some(s))
}

async fn state(tr: &Transaction, import: &NativeMediaImport, owner: u64) -> Result<Value> {
    let value = pending_state(tr, import, owner).await?;
    let task = management::read_task(tr, import.serial(), &import.request).await?;
    if task["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(value)
}

// Startup may inspect the original task before start_native_media claims it
// for the new owner. Projection still requires state() and the claimed epoch.
async fn pending_state(tr: &Transaction, import: &NativeMediaImport, owner: u64) -> Result<Value> {
    routing::require_runtime_active_owner(tr, import.serial(), import.route, owner).await?;
    native_device::require_import_gate_at(tr, import).await?;
    let raw = tr
        .get(&import.gate(), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let value = decode(&raw)?;
    if value["operation"] != import.request.operation().to_string()
        || value["epoch"] != import.request.epoch()
        || value["namespace"] != import.request.namespace().to_string()
        || value["pool"] != import.pool().to_string()
        || !matches!(
            value["phase"].as_str(),
            Some("preflight" | "materialize" | "rollback")
        )
        || !value["abort"].is_boolean()
        || value["rows"].as_u64().is_none()
        || !(value["after"].is_null() || value["after"].is_string())
    {
        return Err(CatalogError::Corrupt("原生介质状态与请求不符"));
    }
    validate_state(&value)?;
    let assignment = tr
        .get(&assignment_key(import.serial(), import.barcode()), false)
        .await?;
    let reverse = tr
        .get(
            &namespace_assignment_key(
                import.request.namespace(),
                import.pool(),
                import.serial(),
                import.barcode(),
            ),
            false,
        )
        .await?;
    if import.refresh.is_some() {
        refresh::require(tr, import).await?;
    }
    if (import.refresh.is_none()
        && assignment.as_deref()
            != Some(encode_assignment(import.request.namespace(), import.pool(), None).as_slice()))
        || reverse.as_deref()
            != Some(encode_namespace_assignment(import.serial(), import.barcode()).as_slice())
    {
        return Err(CatalogError::Corrupt("介质任务条码分配或反向索引不符"));
    }
    let task = management::read_task(tr, import.serial(), &import.request).await?;
    if task["status"] != "running" {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(value)
}

fn validate_state(value: &Value) -> Result<()> {
    let fields = [
        "version",
        "namespace",
        "pool",
        "epoch",
        "operation",
        "source",
        "phase",
        "after",
        "rows",
        "abort",
        "error",
    ];
    let proof = source(&value["source"])?;
    let rows = value["rows"]
        .as_u64()
        .ok_or(CatalogError::Corrupt("介质任务行数"))?;
    if !matches!(
        value["phase"].as_str(),
        Some("preflight" | "materialize" | "rollback")
    ) || !value["abort"].is_boolean()
        || !value["epoch"].as_u64().is_some_and(|n| n > 0)
        || ["namespace", "pool", "operation"].iter().any(|field| {
            !value[field].as_str().is_some_and(|s| {
                Uuid::parse_str(s).is_ok_and(|id| !id.is_nil() && id.to_string() == s)
            })
        })
        || !value.as_object().is_some_and(|object| {
            object.len() == fields.len() && object.keys().all(|k| fields.contains(&k.as_str()))
        })
        || (value["abort"] == true) != (value["phase"] == "rollback")
        || !(value["after"].is_null() || value["after"].is_string())
        || (rows == 0) != value["after"].is_null()
        || value["after"].as_str().is_some_and(|path| {
            lookup::path_components(path.as_bytes()).is_err() || path == "/" || path.len() > 2048
        })
        || (value["phase"] == "preflight" && rows != 0)
        || (proof.is_none() && (rows != 0 || value["phase"] == "materialize"))
        || proof.is_some_and(|p| rows > p.paths)
        || !(value["error"].is_null()
            || value["error"]
                .as_str()
                .is_some_and(|s| s.len() <= 2048 && value["phase"] == "rollback"))
    {
        return Err(CatalogError::Corrupt("介质任务阶段、进度或字段损坏"));
    }
    Ok(())
}

/// Derive public progress from the same read version as the retained task.
/// Unknown work is null; a queued request is never reported as completed.
pub(super) async fn task_progress(tr: &Transaction, task: &Value) -> Result<Value> {
    let kind = task["request"]["change"]["kind"].as_str();
    if !kind.is_some_and(|kind| kind == "media-import" || management::device_kind(kind)) {
        return Ok(Value::Null);
    }
    let request = ManagementRequest::parse(task["request"].clone())?;
    if request.value()["change"]["kind"]
        .as_str()
        .is_some_and(management::device_kind)
    {
        return native_device::task_progress(tr, task).await;
    }
    if !matches!(
        request.value()["change"]["kind"].as_str(),
        Some("media-import")
    ) {
        return Ok(Value::Null);
    }
    let status = task["status"]
        .as_str()
        .ok_or(CatalogError::Corrupt("接入任务状态"))?;
    if status != "running" {
        return Ok(json!({"phase":status,
            "processed_paths":if status=="succeeded"{task["result"]["source"]["paths"].clone()}else{Value::Null},
            "total_paths":task["result"]["source"]["paths"],
            "physical_files":task["result"]["source"]["files"]}));
    }
    let pool = request.value()["change"]["pool"]
        .as_str()
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or(CatalogError::Corrupt("接入任务池身份"))?;
    let raw = tr
        .get(&pool_gate(request.namespace(), pool), false)
        .await?
        .ok_or(CatalogError::Corrupt("运行中的接入任务缺少进度"))?;
    let value = decode(&raw)?;
    validate_state(&value)?;
    if value["epoch"] != request.epoch()
        || value["operation"] != request.operation().to_string()
        || value["namespace"] != request.namespace().to_string()
        || value["pool"] != pool.to_string()
    {
        return Err(CatalogError::Corrupt("接入进度不属于原请求"));
    }
    let source = source(&value["source"])?;
    let phase = if value["abort"] == true {
        "rollback"
    } else if source.is_none() {
        "source_pending"
    } else if value["phase"] == "preflight" {
        "preflight"
    } else {
        "materialize"
    };
    Ok(
        json!({"phase":phase,"processed_paths":if phase=="materialize"||phase=="rollback"{value["rows"].clone()}else{Value::Null},
        "total_paths":source.as_ref().map(|s|s.paths),"physical_files":source.as_ref().map(|s|s.files)}),
    )
}

pub(super) async fn inventory_task(
    tr: &Transaction,
    serial: &str,
    ns: Uuid,
    pool: Uuid,
    barcode: &str,
) -> Result<Option<Value>> {
    let Some(raw) = tr.get(&pool_gate(ns, pool), false).await? else {
        return native_device::inventory_task(tr, serial, ns, pool, barcode).await;
    };
    let value = decode(&raw)?;
    validate_state(&value)?;
    let epoch = value["epoch"]
        .as_u64()
        .ok_or(CatalogError::Corrupt("接入进度代次"))?;
    let operation = value["operation"]
        .as_str()
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or(CatalogError::Corrupt("接入进度操作"))?;
    let (request, task) = management::task_for_identity(tr, serial, ns, epoch, operation).await?;
    if request.value()["change"]["barcode"] != barcode {
        return Ok(None);
    }
    let progress = task_progress(tr, &task).await?;
    Ok(Some(
        json!({"task_id":crate::management_task_id::ManagementTaskId::new(ns,serial,epoch,operation).map_err(CatalogError::Corrupt)?.to_string(),"progress":progress}),
    ))
}

pub(super) async fn request_abort(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    epoch: u64,
    operation: Uuid,
) -> Result<()> {
    let key = pool_gate(ns, pool);
    let raw = tr.get(&key, false).await?.ok_or(CatalogError::Incomplete)?;
    let mut value = decode(&raw)?;
    validate_state(&value)?;
    if value["epoch"] != epoch || value["operation"] != operation.to_string() {
        return Err(CatalogError::Corrupt("取消介质任务身份不符"));
    }
    value["abort"] = json!(true);
    value["phase"] = json!("rollback");
    tr.set(&key, &encode(&value));
    Ok(())
}

impl ManagedCatalog {
    /// Resume the original assigned-volume refresh before requiring the old
    /// observed media head. The pool gate and undo snapshot remain authoritative.
    pub(crate) async fn loaded_native_refresh_pending(
        &self,
        serial: &str,
        barcode: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<bool> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let Some(assignment) = Self::verified_assignment(&tr, serial, barcode).await? else {
            return Ok(false);
        };
        if assignment.namespace != ns {
            return Err(CatalogError::Corrupt("刷新分配命名空间不符"));
        }
        let Some(raw) = tr.get(&pool_gate(ns, assignment.pool_id), false).await? else {
            return Ok(false);
        };
        let gate = decode(&raw)?;
        validate_state(&gate)?;
        let epoch = gate["epoch"]
            .as_u64()
            .ok_or(CatalogError::Corrupt("刷新任务代次无效"))?;
        let operation = gate["operation"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(CatalogError::Corrupt("刷新任务身份无效"))?;
        let (request, task) =
            management::task_for_identity(&tr, serial, ns, epoch, operation).await?;
        if request.value()["change"]["barcode"] != barcode
            || !matches!(
                request.value()["change"]["kind"].as_str(),
                Some("media-import")
            )
        {
            return Ok(false);
        }
        if task["status"] != "running" {
            return Err(CatalogError::Corrupt("刷新屏障缺少运行中原任务"));
        }
        let mut import = NativeMediaImport {
            request,
            route,
            refresh: None,
        };
        native_device::require_import_gate_at(&tr, &import).await?;
        import.refresh = refresh::load(&tr, &import).await?;
        if import.assigned_volume()? != assignment.volume_id || import.refresh.is_none() {
            return Err(CatalogError::Corrupt("刷新快照与已分配卷不符"));
        }
        pending_state(&tr, &import, owner).await?;
        Ok(true)
    }

    /// Validate or project one independently verified logical entry. Existing
    /// directories keep their identity and attributes; files never overwrite.
    pub async fn project_native_media_entry(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        entry: &StagedImportEntry,
        write: bool,
    ) -> Result<()> {
        if write {
            self.upload_attributes(import.request.namespace(), &entry.current_attributes)
                .await?;
        }
        loop {
            let complete = self
                .db
                .run(|tr, _| async move {
                    requests::retry_result(
                        async {
                            let mut value = state(&tr, import, owner).await?;
                            if value["abort"] == true {
                                return Err(CatalogError::PoolImporting);
                            }
                            if write && value["phase"] != "materialize" {
                                return Err(CatalogError::Incomplete);
                            }
                            let proof =
                                source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
                            if entry.path.pool_id != import.pool()
                                || entry.path.sources.len() != 1
                                || entry.path.winner != 0
                            {
                                return Err(CatalogError::Invalid(
                                    "原生介质入口来源不唯一或池不符",
                                ));
                            }
                            let path = &entry.path.logical_path;
                            let current = entry.path.current();
                            if current.volume_id != proof.index().volume_uuid
                                || current.generation != proof.index().generation
                                || current.barcode != import.barcode()
                                || entry.source_file_uids.len() != 1
                                || entry.current_attributes.meta().map(|m| m.file_uid)
                                    != entry.source_file_uids.first().copied()
                            {
                                return Err(CatalogError::Corrupt("介质入口与固定来源身份不符"));
                            }
                            let candidate = &current.candidate;
                            if candidate.logical_path != *path {
                                return Err(CatalogError::Corrupt("介质入口路径与来源不符"));
                            }
                            if import.refresh.is_some() {
                                return refresh::project(&tr, import, entry, write, value, &proof)
                                    .await
                                    .map(|()| true);
                            }
                            retired_nodes::require_native_path_allowed(
                                &tr,
                                import.request.namespace(),
                                import.pool(),
                                path,
                            )
                            .await?;
                            let journal = import.row(b'e', path);
                            let fingerprint = entry_fingerprint(entry, &proof);
                            let mapping = target::saved(&tr, import, path, &fingerprint).await?;
                            if let Some(raw) = tr.get(&journal, false).await? {
                                let row = decode(&raw)?;
                                if row["kind"] != "e"
                                    || row["path"] != *path
                                    || row["fingerprint"] != fingerprint
                                    || mapping.is_none()
                                {
                                    return Err(CatalogError::Corrupt("介质物化回执与来源不符"));
                                }
                                for (key, hash) in decode_journal(&raw)? {
                                    validate_undo_key(import, &proof, &key)?;
                                    let saved = tr
                                        .get(&key, false)
                                        .await?
                                        .ok_or(CatalogError::Corrupt("介质物化回执目标缺失"))?;
                                    if Sha256::digest(&saved).as_slice() != hash {
                                        return Err(CatalogError::Corrupt(
                                            "介质物化回执目标已改变",
                                        ));
                                    }
                                }
                                return Ok(true);
                            }
                            if mapping.is_some() {
                                return Err(CatalogError::Corrupt("目标映射缺少原子物化回执"));
                            }
                            let Some(mapping) =
                                target::choose(&tr, import, entry, &fingerprint).await?
                            else {
                                return Ok(false);
                            };
                            let binding = ImportSourceBinding {
                                placement: VolumePlacement {
                                    volume_id: proof.index().volume_uuid,
                                    namespace: import.request.namespace(),
                                    pool_id: import.pool(),
                                    library_serial: import.serial().into(),
                                    barcode: import.barcode().into(),
                                },
                                scan: VolumeScan {
                                    generation: proof.index().generation,
                                    id: import.scan(),
                                },
                                source: proof.index(),
                            };
                            let parent = mapping.parent;
                            let name = mapping.name.as_bytes().to_vec();
                            let child = child_key(import.request.namespace(), parent, &name);
                            let mut writes = Vec::<(Vec<u8>, Vec<u8>)>::new();
                            let tomb = import_materialize::tombstone_key(
                                import.request.namespace(),
                                import.pool(),
                                &mapping.target,
                            );
                            if candidate.deleted {
                                let mut plan = import_materialize::plan_tombstone(
                                    import.request.namespace(),
                                    entry,
                                    std::slice::from_ref(&binding),
                                )?;
                                plan.path = mapping.target.clone();
                                writes.push((tomb, import_materialize::encode_tombstone(&plan)?));
                            } else if candidate.kind == NodeKind::Directory {
                                if entry.current_attributes.symlink().is_some()
                                    || entry.source_data.as_deref()
                                        != Some(&[StagedSourceData::NoCopy])
                                {
                                    return Err(CatalogError::Corrupt("导入目录来源数据类型"));
                                }
                                if !mapping.existing_directory {
                                    writes.push((child, mapping.id.as_bytes().to_vec()));
                                    writes.push((
                                        node_key(import.request.namespace(), mapping.id),
                                        encode_node(&Node {
                                            kind: NodeKind::Directory,
                                            directory_version: Some(1),
                                            parent,
                                            name,
                                            attributes: Some(entry.current_attributes.clone()),
                                        }),
                                    ));
                                }
                            } else {
                                let mut plan = import_materialize::plan_data_file(
                                    import.request.namespace(),
                                    entry,
                                    std::slice::from_ref(&binding),
                                )?
                                .ok_or(CatalogError::Incomplete)?;
                                plan.id = mapping.id;
                                plan.parent = parent;
                                let copy = &plan.copies[0];
                                let copy_value = encode_physical_copy(&copy.copy)?;
                                let mut reverse = copy.copy.version.to_be_bytes().to_vec();
                                reverse.extend_from_slice(&Sha256::digest(&copy_value));
                                writes.extend([
                                    (child, plan.id.as_bytes().to_vec()),
                                    (
                                        node_key(import.request.namespace(), plan.id),
                                        encode_node(&Node {
                                            kind: NodeKind::File,
                                            directory_version: None,
                                            parent,
                                            name,
                                            attributes: Some(plan.attributes),
                                        }),
                                    ),
                                    (
                                        head_key(import.request.namespace(), plan.id),
                                        encode_head(&plan.head),
                                    ),
                                    (
                                        copy_key(import.request.namespace(), plan.id, copy.id),
                                        copy_value,
                                    ),
                                    (
                                        volume_key(
                                            copy.copy.volume_id,
                                            copy.copy.volume_scan_id,
                                            import.request.namespace(),
                                            plan.id,
                                            copy.id,
                                        ),
                                        reverse,
                                    ),
                                ]);
                            }
                            if !candidate.deleted {
                                writes.push(refresh::origin_write(
                                    import,
                                    &proof,
                                    entry.source_file_uids[0],
                                    path,
                                    &mapping.target,
                                )?);
                            }
                            for (key, _) in &writes {
                                if tr.get(key, false).await?.is_some() {
                                    return Err(CatalogError::Corrupt(
                                        "导入目标存在孤立或未登记记录",
                                    ));
                                }
                            }
                            if !write {
                                return Ok(true);
                            }
                            target::require_ready(&tr, import, &proof).await?;
                            if value["after"]
                                .as_str()
                                .is_some_and(|after| after >= path.as_str())
                            {
                                return Err(CatalogError::Corrupt("介质物化游标未按路径推进"));
                            }
                            if !candidate.deleted {
                                let raw = tr
                                    .get(&node_key(import.request.namespace(), parent), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                if decode_node(&raw)?.kind != NodeKind::Directory {
                                    return Err(CatalogError::Corrupt("介质物化父节点不是目录"));
                                }
                                attribute_objects::require_attributes(
                                    &tr,
                                    import.request.namespace(),
                                    Some(&entry.current_attributes),
                                )
                                .await?;
                            }
                            let rows = value["rows"]
                                .as_u64()
                                .expect("validated rows")
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("介质导入行数溢出"))?;
                            if rows > proof.paths {
                                return Err(CatalogError::Corrupt("介质导入行数超过已核验清单"));
                            }
                            target::commit(
                                &tr,
                                import,
                                &mapping,
                                !candidate.deleted && candidate.kind == NodeKind::Directory,
                            )
                            .await?;
                            write_undo(&tr, import, b'e', path, &fingerprint, &writes)?;
                            for (key, bytes) in writes {
                                tr.set(&key, &bytes);
                            }
                            value["rows"] = json!(rows);
                            value["after"] = json!(path);
                            tr.set(&import.gate(), &encode(&value));
                            Ok(true)
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
                .await??;
            if complete {
                return Ok(());
            }
        }
    }

    /// Publishing source/scan/assignment and removing the pool gate is one
    /// transaction. No pending copy can become readable before this point.
    pub async fn finish_native_media(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        observed: &NativeMediaSource,
    ) -> Result<Value> {
        if import.refresh.is_some() {
            let tr = self.db.create_trx()?;
            routing::require_runtime_active_owner(&tr, import.serial(), import.route, owner)
                .await?;
            let saved = management::read_task(&tr, import.serial(), &import.request).await?;
            if saved["status"] == "succeeded" {
                if saved["result"]["source"] != source_value(observed) {
                    return Err(CatalogError::Invalid("已完成刷新任务来源与重试不符"));
                }
                return Ok(saved);
            }
            drop(tr);
            loop {
                let complete = self
                    .db
                    .run(|tr, _| async move {
                        requests::retry_result(
                            async {
                                let value = state(&tr, import, owner).await?;
                                if value["abort"] == true {
                                    return Err(CatalogError::PoolImporting);
                                }
                                refresh::sweep(&tr, import, &value).await
                            }
                            .await
                            .map_err(notifications::atomic_error),
                        )
                    })
                    .await??;
                if complete {
                    break;
                }
            }
        }
        self.db.run(|tr,_|async move{requests::retry_result(async{
            routing::require_runtime_active_owner(&tr,import.serial(),import.route,owner).await?;
            let saved=management::read_task(&tr,import.serial(),&import.request).await?;
            if matches!(saved["status"].as_str(),Some("succeeded"|"failed"|"cancelled")) {
                if saved["status"]=="succeeded" && saved["result"]["source"]!=source_value(observed) {return Err(CatalogError::Invalid("已完成介质任务来源与重试不符"));}
                return Ok(saved);
            }
            let value=state(&tr,import,owner).await?;
            let proof=source(&value["source"])?.ok_or(CatalogError::Incomplete)?;

            target::require_ready(&tr,import,&proof).await?;
            if value["abort"]==true {return Err(CatalogError::PoolImporting);}
            if value["phase"]!="materialize" || value["rows"]!=proof.paths || &proof!=observed {return Err(CatalogError::Incomplete);}
            if import.refresh.is_some() {
                let result = refresh::publish(&tr, import, &proof).await?;
                native_device::publish_intake_home_at(&tr,import,proof.index().volume_uuid).await?;
                namespace_visibility::publish(&tr, import.request.namespace());
                return Ok(result);
            }
            let ns=import.request.namespace();let volume=proof.index().volume_uuid;let scan=VolumeScan{generation:proof.index().generation,id:import.scan()};
            if tr.get(&volume_claim_key(volume),false).await?.as_deref()!=Some(import.token().as_slice()){return Err(CatalogError::Corrupt("介质卷身份占用缺失或冲突"));}
            let assigned=tr.get(&assignment_key(import.serial(),import.barcode()),false).await?.ok_or(CatalogError::Corrupt("介质分配缺失"))?;
            if assigned.as_ref()!=encode_assignment(ns,import.pool(),None){return Err(CatalogError::Corrupt("介质分配已改变"));}
            for key in [placement_key(volume),barcode_key(import.serial(),import.barcode()),ready_volume_key(volume),reconcile_volume_key(volume),scan_key(volume,scan.id),source_key(volume,scan.id)] {
                if tr.get(&key,false).await?.is_some(){return Err(CatalogError::Corrupt("介质发布目标已有记录"));}
            }
            let revision=source_revision(&tr,ns).await?.checked_add(1).ok_or(CatalogError::Corrupt("介质来源修订号溢出"))?;
            let placement=VolumePlacement{volume_id:volume,namespace:ns,pool_id:import.pool(),library_serial:import.serial().into(),barcode:import.barcode().into()};
            let mut task=management::read_task(&tr,import.serial(),&import.request).await?;
            task["status"]=json!("succeeded");task["result"]=json!({"pool":import.pool().to_string(),"barcode":import.barcode(),"source":source_value(&proof)});task["completed_at_millis"]=json!(management::now().max(task["created_at_millis"].as_u64().expect("validated time")));
            native_device::publish_intake_home_at(&tr,import,volume).await?;
            tr.set(&placement_key(volume),&encode_placement(&placement));tr.set(&barcode_key(import.serial(),import.barcode()),volume.as_bytes());
            tr.set(&assignment_key(import.serial(),import.barcode()),&encode_assignment(ns,import.pool(),Some(volume)));
            tr.set(&ready_volume_key(volume),&encode_ready_volume(scan,import.serial()));tr.set(&scan_key(volume,scan.id),&encode_scan_manifest(scan,import.serial(),ScanState::Ready));
            tr.set(&source_key(volume,scan.id),&index_evidence::encode_source(&proof.identity,proof.files));tr.set(&source_revision_key(ns),&revision.to_be_bytes());
            tr.set(&refresh::base_key(ns,volume),target::base(import)?.as_bytes());
            management::write_task(&tr,&import.request,&task).await?;management::clear_queued(&tr,&import.request);
            tr.clear(&volume_claim_key(volume));tr.clear(&import.gate());native_device::clear_import_gate_at(&tr,import);
            let journal=import.journal();tr.clear_range(&journal,&prefix_end(&journal));
            let mapping=target::prefix(import);tr.clear_range(&mapping,&prefix_end(&mapping));
            notifications::tape_changed_at(&tr, ns, import.serial(), import.barcode(), "import_completed").await?;
            namespace_visibility::publish(&tr, ns);
            Ok(task)
        }.await.map_err(notifications::atomic_error))}).await?
    }

    /// Admit the explicit cartridge before any device access. Existing bound
    /// assignments cannot be silently rebound or scanned into a second pool.
    pub async fn start_native_media(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Option<NativeMediaImport>> {
        self.db.run(|tr,_|async move{requests::retry_result(async{
            routing::require_runtime_active_owner(&tr,serial,route,owner).await?;
            let (request,mut task)=management::task_for_identity(&tr,serial,route.route.namespace,epoch,operation).await?;
            if !matches!(request.value()["change"]["kind"].as_str(),Some("media-import")){return Err(CatalogError::Invalid("不是原生介质任务"));}
            if !matches!(task["status"].as_str(),Some("queued"|"running")){return Ok(None);}
            let mut import=NativeMediaImport{request,route,refresh:None};
            if task["status"]=="queued" {
                let pool=decode_pool(import.pool(),&tr.get(&pool_key(route.route.namespace,import.pool()),false).await?.ok_or(CatalogError::Invalid("目标池不存在"))?)?;
                require_pool_library(&tr,route.route.namespace,import.pool(),serial).await?;
                reclaim::require_pool_write_at(&tr,route.route.namespace,import.pool()).await?;
                native_device::require_pool_available(&tr,route.route.namespace,import.pool()).await?;
                active_pools::require_indexes(&tr,route.route.namespace,&pool).await?;
                if pool.policy.media_restriction!="none" && !import.barcode().ends_with(&pool.policy.media_restriction) {return Err(CatalogError::Invalid("条码不符合池介质限制"));}
                if requests::has_namespace_holds(&tr,route.route.namespace).await?{return Err(CatalogError::Incomplete);}
                if tr.get(&import.gate(),false).await?.is_some(){return Err(CatalogError::Incomplete);}
                native_device::acquire_import_gate_at(&tr, &import).await?;
                import.refresh = refresh::prepare(&tr, &import).await?;
                if import.refresh.is_none() {
                for key in [assignment_key(serial,import.barcode()),barcode_key(serial,import.barcode()),namespace_assignment_key(route.route.namespace,import.pool(),serial,import.barcode())] {
                    if tr.get(&key,false).await?.is_some(){return Err(CatalogError::Invalid("条码已分配或已有介质记录"));}
                }
                }
                let value=json!({"version":1,"namespace":route.route.namespace.to_string(),"pool":import.pool().to_string(),"epoch":epoch,"operation":operation.to_string(),"source":null,"phase":"preflight","after":null,"rows":0,"abort":false,"error":null});
                tr.set(&import.gate(),&encode(&value));
                if import.refresh.is_none() {
                    tr.set(&assignment_key(serial,import.barcode()),&encode_assignment(route.route.namespace,import.pool(),None));
                    tr.set(&namespace_assignment_key(route.route.namespace,import.pool(),serial,import.barcode()),&encode_namespace_assignment(serial,import.barcode()));
                }
            } else {
                if task["owner_epoch"].as_u64().is_none_or(|old|old==0 || old>owner) { return Err(CatalogError::OwnerFenced); }
                import.refresh = refresh::load(&tr, &import).await?;
                pending_state(&tr, &import, owner).await?;
                let raw=tr.get(&import.gate(),false).await?.ok_or(CatalogError::Corrupt("运行中的介质任务缺少池屏障"))?;let value=decode(&raw)?;
                validate_state(&value)?;
                if value["namespace"]!=route.route.namespace.to_string()||value["pool"]!=import.pool().to_string()||value["epoch"]!=epoch||value["operation"]!=operation.to_string(){return Err(CatalogError::Corrupt("接任介质任务身份不符"));}
            }
            task["status"]=json!("running");task["owner_epoch"]=json!(owner);
            management::write_task(&tr,&import.request,&task).await?;
            notifications::tape_changed_at(&tr, import.request.namespace(), serial, import.barcode(), "import_started").await?;
            Ok(Some(import))
        }.await.map_err(notifications::atomic_error))}).await?
    }

    pub async fn native_media_status(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<Value> {
        let tr = self.db.create_trx()?;
        state(&tr, import, owner).await
    }

    pub async fn bind_native_media_source(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        proof: &NativeMediaSource,
    ) -> Result<()> {
        if import.request.value()["change"]["volume"]
            .as_str()
            .is_some_and(|id| id != proof.index().volume_uuid.to_string())
        {
            return Err(CatalogError::Invalid("介质 UUID 与显式预期不符"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let mut value = state(&tr, import, owner).await?;

                        if value["abort"] == true {
                            return Err(CatalogError::PoolImporting);
                        }
                        if let Some(old) = source(&value["source"])? {
                            if &old != proof {
                                return Err(CatalogError::Invalid("介质与首次捕获的索引来源不符"));
                            }
                            if tr
                                .get(&volume_claim_key(proof.index().volume_uuid), false)
                                .await?
                                .as_deref()
                                != Some(import.token().as_slice())
                            {
                                return Err(CatalogError::Corrupt("介质卷身份占用缺失或冲突"));
                            }
                            target::bind(&tr, import, proof, false).await?;
                            return Ok(());
                        }
                        let volume = proof.index().volume_uuid;
                        if import.refresh.is_some() {
                            refresh::bind(&tr, import, proof).await?;
                        } else {
                            for key in [
                                placement_key(volume),
                                ready_volume_key(volume),
                                reconcile_volume_key(volume),
                            ] {
                                if tr.get(&key, false).await?.is_some() {
                                    return Err(CatalogError::Invalid("卷身份已有归属或扫描记录"));
                                }
                            }
                            let claim = volume_claim_key(volume);
                            if let Some(old) = tr.get(&claim, false).await?
                                && old.as_ref() != import.token().as_slice()
                            {
                                return Err(CatalogError::Invalid("卷身份正由另一任务接入"));
                            }
                            tr.set(&claim, &import.token());
                        }
                        value["source"] = source_value(proof);
                        target::bind(&tr, import, proof, true).await?;
                        tr.set(&import.gate(), &encode(&value));
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await??;
        if import.refresh.is_some() {
            loop {
                let complete = self
                    .db
                    .run(|tr, _| async move {
                        requests::retry_result(
                            async {
                                let value = state(&tr, import, owner).await?;
                                if value["abort"] == true {
                                    return Err(CatalogError::PoolImporting);
                                }
                                refresh::capture(&tr, import).await
                            }
                            .await
                            .map_err(notifications::atomic_error),
                        )
                    })
                    .await??;
                if complete {
                    break;
                }
            }
        }
        Ok(())
    }

    pub async fn begin_native_media_projection(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let value = state(&tr, import, owner).await?;
        if value["abort"] == true {
            return Err(CatalogError::PoolImporting);
        }
        let proof = source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
        let attributes = target::attributes(&tr, import, &proof).await?;
        drop(tr);
        self.upload_attributes(import.request.namespace(), &attributes)
            .await?;
        loop {
            let complete = self
                .db
                .run(|tr, _| {
                    let attributes = attributes.clone();
                    async move {
                        requests::retry_result(
                            async {
                                let value = state(&tr, import, owner).await?;
                                if value["abort"] == true {
                                    return Err(CatalogError::PoolImporting);
                                }
                                let proof =
                                    source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
                                target::prefix_step(&tr, import, &proof, &attributes).await
                            }
                            .await
                            .map_err(notifications::atomic_error),
                        )
                    }
                })
                .await??;
            if complete {
                break;
            }
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let mut value = state(&tr, import, owner).await?;
                        if value["abort"] == true {
                            return Err(CatalogError::PoolImporting);
                        }
                        let proof = source(&value["source"])?.ok_or(CatalogError::Incomplete)?;
                        target::require_ready(&tr, import, &proof).await?;
                        if value["phase"] == "preflight" {
                            value["phase"] = json!("materialize");
                            tr.set(&import.gate(), &encode(&value));
                        }
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    pub async fn abort_native_media(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        error: Option<&str>,
    ) -> Result<()> {
        if error.is_some_and(|s| s.len() > 2048) {
            return Err(CatalogError::Invalid("介质错误上下文过长"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let mut value = state(&tr, import, owner).await?;
                        if value["abort"] == true {
                            return Ok(());
                        }
                        value["abort"] = json!(true);
                        value["phase"] = json!("rollback");
                        if let Some(error) = error {
                            value["error"] = json!(error);
                        }
                        tr.set(&import.gate(), &encode(&value));
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    /// Clear at most 100 journal rows. Every key is compared with its recorded
    /// value digest before deleting, preserving unrelated or changed data.
    pub async fn rollback_native_media_page(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<bool> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(
                            &tr,
                            import.serial(),
                            import.route,
                            owner,
                        )
                        .await?;
                        let saved =
                            management::read_task(&tr, import.serial(), &import.request).await?;
                        if matches!(saved["status"].as_str(), Some("failed" | "cancelled")) {
                            return Ok(true);
                        }
                        let value = state(&tr, import, owner).await?;
                        load::require_settled_at(&tr, import).await?;

                        if value["phase"] != "rollback" {
                            return Err(CatalogError::Invalid("任务未进入回滚"));
                        }
                        if import.refresh.is_some() {
                            let complete = refresh::rollback(&tr, import, &value).await?;
                            if complete {
                                native_device::clear_intake_home_at(&tr, import);
                            }
                            return Ok(complete);
                        }
                        let page = read_page(&tr, &import.journal(), None, 100, false).await?;
                        let mut keys = Vec::new();
                        for (key, raw) in &page.items {
                            let record = decode(raw)?;
                            let kind = match record["kind"].as_str() {
                                Some("e") => b'e',
                                Some("d") => b'd',
                                _ => return Err(CatalogError::Corrupt("回滚清单类型身份不符")),
                            };
                            if record["path"]
                                .as_str()
                                .map(|path| import.row(kind, path))
                                .as_deref()
                                != Some(key.as_slice())
                            {
                                return Err(CatalogError::Corrupt("回滚清单路径身份不符"));
                            }
                            let row = decode_journal(raw)?;
                            for (key, hash) in row {
                                validate_undo_key(
                                    import,
                                    &source(&value["source"])?.ok_or(CatalogError::Incomplete)?,
                                    &key,
                                )?;
                                let saved = tr
                                    .get(&key, false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("回滚目标已缺失"))?;
                                if Sha256::digest(&saved).as_slice() != hash {
                                    return Err(CatalogError::Corrupt(
                                        "回滚目标被修改，保留池屏障",
                                    ));
                                }
                                keys.push(key);
                            }
                            keys.push(key.clone());
                        }
                        if !keys.is_empty() {
                            for key in keys {
                                tr.clear(&key);
                            }
                            return Ok(false);
                        }
                        let claimed =
                            source(&value["source"])?.map(|source| source.index().volume_uuid);
                        if let Some(volume) = claimed {
                            let claim = volume_claim_key(volume);
                            if tr.get(&claim, false).await?.as_deref()
                                != Some(import.token().as_slice())
                            {
                                return Err(CatalogError::Corrupt("回滚卷身份占用不符"));
                            }
                        }
                        let assigned = tr
                            .get(&assignment_key(import.serial(), import.barcode()), false)
                            .await?
                            .ok_or(CatalogError::Corrupt("回滚条码分配缺失"))?;
                        if assigned.as_ref()
                            != encode_assignment(import.request.namespace(), import.pool(), None)
                        {
                            return Err(CatalogError::Corrupt("回滚条码已有绑定"));
                        }
                        let mut task =
                            management::read_task(&tr, import.serial(), &import.request).await?;
                        task["status"] = json!(if value["error"].is_null() {
                            "cancelled"
                        } else {
                            "failed"
                        });
                        task["result"] = if value["error"].is_null() {
                            json!({"cancelled":true})
                        } else {
                            json!({"error":value["error"]})
                        };

                        task["completed_at_millis"] = json!(
                            management::now()
                                .max(task["created_at_millis"].as_u64().expect("validated time"))
                        );
                        if let Some(volume) = claimed {
                            tr.clear(&volume_claim_key(volume));
                        }
                        management::write_task(&tr, &import.request, &task).await?;
                        management::clear_queued(&tr, &import.request);
                        tr.clear(&assignment_key(import.serial(), import.barcode()));
                        tr.clear(&namespace_assignment_key(
                            import.request.namespace(),
                            import.pool(),
                            import.serial(),
                            import.barcode(),
                        ));
                        tr.clear(&import.gate());
                        native_device::clear_import_gate_at(&tr, import);
                        native_device::clear_intake_home_at(&tr, import);
                        let mapping = target::prefix(import);
                        tr.clear_range(&mapping, &prefix_end(&mapping));

                        notifications::tape_changed_at(
                            &tr,
                            import.request.namespace(),
                            import.serial(),
                            import.barcode(),
                            "import_rolled_back",
                        )
                        .await?;
                        Ok(true)
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}

fn entry_fingerprint(entry: &StagedImportEntry, proof: &NativeMediaSource) -> String {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/native-media-entry/1");
    hash.update(proof.identity.fingerprint());
    hash.update(entry.path.logical_path.as_bytes());
    hash.update(entry.current_attributes.digest());
    for uid in &entry.source_file_uids {
        hash.update(uid.to_be_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn validate_undo_key(
    import: &NativeMediaImport,
    proof: &NativeMediaSource,
    key: &[u8],
) -> Result<()> {
    let ns = import.request.namespace();
    let namespace_owned = b"NCHRt".iter().any(|kind| {
        let mut prefix = super::super::key(*kind);
        prefix.extend_from_slice(ns.as_bytes());
        key.starts_with(&prefix)
    });
    let mut volume = volume_prefix(proof.index().volume_uuid, import.scan());
    volume.extend_from_slice(ns.as_bytes());
    let origin = refresh::origin_prefix(ns, proof.index().volume_uuid, import.scan());
    let suppressed = refresh::suppression_prefix(ns, proof.index().volume_uuid, import.scan());
    if !namespace_owned
        && !key.starts_with(&volume)
        && !key.starts_with(&origin)
        && !key.starts_with(&suppressed)
    {
        return Err(CatalogError::Corrupt("介质回滚目标超出本次命名空间或卷"));
    }
    Ok(())
}

async fn target_parent(
    tr: &Transaction,
    import: &NativeMediaImport,
    path: &str,
) -> Result<(Uuid, Vec<u8>)> {
    let catalog = pool_catalog_path(import.pool(), path.as_bytes())?;
    let components = lookup::path_components(&catalog)?;
    let name = components
        .last()
        .ok_or(CatalogError::Invalid("介质路径缺少名称"))?
        .to_vec();
    if components.len() < 2 {
        return Err(CatalogError::Invalid("介质逻辑路径不能为池根"));
    }
    let mut parent =
        import_materialize::import_inode(import.request.namespace(), import.pool(), "/");
    for depth in 2..components.len() {
        let prefix = components[..depth].to_vec();
        let relative = components[1..depth]
            .iter()
            .map(|b| {
                std::str::from_utf8(b).map_err(|_| CatalogError::Invalid("介质路径不是 UTF-8"))
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        let virtual_path = format!("/{relative}");
        parent = match lookup::resolve_active_path_at_mode(
            tr,
            import.request.namespace(),
            prefix,
            false,
        )
        .await?
        {
            Some(ResolvedPath::Directory { id, .. }) => id,
            Some(_) => return Err(CatalogError::Invalid("导入路径父目录被已有文件占用")),
            None => import_materialize::import_inode(
                import.request.namespace(),
                import.pool(),
                &virtual_path,
            ),
        };
        target::require_directory(tr, import, &virtual_path, parent).await?;
    }
    Ok((parent, name))
}

fn decode_journal(raw: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let value = decode(raw)?;
    let keys: Vec<(Vec<u8>, Vec<u8>)> = serde_json::from_value(value["keys"].clone())
        .map_err(|_| CatalogError::Corrupt("介质回滚清单"))?;
    if !value.as_object().is_some_and(|o| {
        o.len() == 5
            && o.keys()
                .all(|k| ["version", "kind", "path", "fingerprint", "keys"].contains(&k.as_str()))
    }) || !matches!(value["kind"].as_str(), Some("e" | "d"))
        || !value["path"]
            .as_str()
            .is_some_and(|p| p.starts_with('/') && p != "/" && !p.ends_with('/'))
        || !value["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        || keys
            .iter()
            .map(|(k, _)| k)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != keys.len()
        || keys.len() > 8
        || keys
            .iter()
            .any(|(key, hash)| key.is_empty() || hash.len() != 32)
    {
        return Err(CatalogError::Corrupt("介质回滚清单长度"));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::volume_extents::VolumeExtentCache;
    use crate::ltfs::index::{DirectoryNode, FileNode, LtfsIndex};

    pub(super) async fn fixture(
        catalog: &ManagedCatalog,
    ) -> (String, ActiveLibraryRoute, PoolRecord) {
        let ns = Uuid::new_v4();
        let serial = format!("SIM-INTAKE-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "intake".into(),
            file_limit: 200_000,
            created_at_millis: management::now(),
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        (serial, route, pool)
    }

    async fn start(
        catalog: &ManagedCatalog,
        serial: &str,
        route: ActiveLibraryRoute,
        pool: Uuid,
        owner: u64,
        barcode: &str,
    ) -> NativeMediaImport {
        let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}})).unwrap();
        catalog
            .admit_management(serial, route, owner, &request)
            .await
            .unwrap();
        catalog
            .start_native_media(serial, route, owner, 1, request.operation())
            .await
            .unwrap()
            .unwrap()
    }

    pub(super) async fn start_target(
        catalog: &ManagedCatalog,
        serial: &str,
        route: ActiveLibraryRoute,
        pool: Uuid,
        owner: u64,
        barcode: &str,
        target: (&str, bool),
    ) -> NativeMediaImport {
        let (path, merge) = target;
        let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.to_string(),"barcode":barcode,"volume":null,"path":path,"merge_tapes":merge}})).unwrap();
        catalog
            .admit_management(serial, route, owner, &request)
            .await
            .unwrap();
        catalog
            .start_native_media(serial, route, owner, 1, request.operation())
            .await
            .unwrap()
            .unwrap()
    }

    fn target_source(root: &std::path::Path, files: usize) -> (ImportSpool, NativeMediaSource) {
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "target-source".into(), 'b');
        index.self_location.start_block = 5;
        if files != 0 {
            let mut directory = DirectoryNode {
                name: "docs".into(),
                meta: index.root.meta.clone(),
                ..Default::default()
            };
            directory.meta.file_uid = 2;
            for at in 0..files {
                let mut file = FileNode {
                    name: if at == 0 {
                        "conflict.txt".into()
                    } else {
                        format!("conflict.txt_{at}")
                    },
                    meta: index.root.meta.clone(),
                    ..Default::default()
                };
                file.meta.file_uid = at as u64 + 3;
                directory.files.push(file);
            }
            index.root.subdirs.push(directory);
            index.highest_file_uid = files as u64 + 2;
        }
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = root.join(format!("{}.sqlite", index.volume_uuid));
        ImportSpool::build(&xml[..], &path, expected).unwrap();
        let spool = ImportSpool::open(&path, expected).unwrap();
        let proof = NativeMediaSource {
            identity: spool.identity().clone(),
            files: files as u64,
            paths: spool.candidate_stats().entries,
        };
        (spool, proof)
    }

    pub(super) fn target_entries(
        import: &NativeMediaImport,
        spool: &ImportSpool,
    ) -> Vec<StagedImportEntry> {
        let mut entries = Vec::new();
        let mut after = None;
        loop {
            let page = spool.candidate_page(after.as_deref(), 8).unwrap();
            for candidate in page.candidates {
                entries.push(import.entry(spool, candidate).unwrap());
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        entries
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；已分配磁带同池索引刷新，不操作设备"]
    async fn native_refresh_assigned_volume_updates_in_place_and_preserves_assignment() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let root = std::env::temp_dir().join(format!("native-refresh-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let (spool, proof) = target_source(&root, 2);
        let initial = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            100,
            "REFRESH0L8",
            ("/custom", true),
        )
        .await;
        catalog
            .bind_native_media_source(&initial, 100, &proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&initial, 100)
            .await
            .unwrap();
        for entry in target_entries(&initial, &spool) {
            catalog
                .project_native_media_entry(&initial, 100, &entry, true)
                .await
                .unwrap();
        }
        catalog
            .finish_native_media(&initial, 100, &proof)
            .await
            .unwrap();
        let assignment = catalog
            .tape_assignment(&serial, "REFRESH0L8")
            .await
            .unwrap()
            .unwrap();
        let path = pool_catalog_path(pool.id, b"/custom/docs/conflict.txt").unwrap();
        let before = catalog
            .resolve_active_path_for_route(&serial, &path, route)
            .await
            .unwrap()
            .unwrap();
        // A new task for the already-bound cartridge is a refresh, not another
        // import tree with suffixed copies of the same paths.
        let refresh = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            100,
            "REFRESH0L8",
            ("/IMPORT", false),
        )
        .await;
        catalog
            .bind_native_media_source(&refresh, 100, &proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&refresh, 100)
            .await
            .unwrap();
        for entry in target_entries(&refresh, &spool) {
            catalog
                .project_native_media_entry(&refresh, 100, &entry, true)
                .await
                .unwrap();
        }
        catalog
            .finish_native_media(&refresh, 100, &proof)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .tape_assignment(&serial, "REFRESH0L8")
                .await
                .unwrap()
                .unwrap(),
            assignment
        );
        let after = catalog
            .resolve_active_path_for_route(&serial, &path, route)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!((&before, &after), (ResolvedPath::File { id: a, .. }, ResolvedPath::File { id: b, .. }) if a == b)
        );
        assert!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    &pool_catalog_path(pool.id, b"/IMPORT/REFRESH0L8").unwrap(),
                    route
                )
                .await
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_import_request_requires_explicit_current_target_spec() {
        let request = json!({"version":1,"library":"SIM-TARGET","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":Uuid::new_v4().to_string(),"barcode":"TARGET0L8","volume":null,"path":"/IMPORT","merge_tapes":false}});
        assert!(ManagementRequest::parse(request.clone()).is_ok());
        for missing in ["path", "merge_tapes"] {
            let mut invalid = request.clone();
            invalid["change"].as_object_mut().unwrap().remove(missing);
            assert!(ManagementRequest::parse(invalid).is_err());
        }
        for path in [
            "",
            "relative",
            "/bad/",
            "/bad//component",
            "/bad/../component",
        ] {
            let mut invalid = request.clone();
            invalid["change"]["path"] = json!(path);
            assert!(ManagementRequest::parse(invalid).is_err());
        }
        let mut invalid = request;
        invalid["change"]["merge_tapes"] = json!("false");
        assert!(ManagementRequest::parse(invalid).is_err());
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；真实EE1.3.4索引结构映射，不操作设备"]
    async fn native_target_keeps_physical_ee_structure_and_extents() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let ns = route.route.namespace;
        for merge in [false, true] {
            let bytes = include_bytes!("../../tests/fixtures/ee-aa18-1.3.4.xml");
            let mut index = LtfsIndex::parse(bytes).unwrap();
            assert_eq!(
                index.volume_uuid.to_string(),
                "ab1b7863-d243-40e1-be65-e732fe5232d0"
            );
            assert_eq!(index.generation, 7);
            // A fresh volume identity isolates reruns in one test FDB. All physical
            // directory paths, UIDs, attributes and extents remain the captured ones.
            index.volume_uuid = Uuid::new_v4();
            let xml = index.to_xml().unwrap();
            let expected = ExpectedIndex {
                volume_uuid: index.volume_uuid,
                generation: index.generation,
                location: index.self_location,
                sha256: Sha256::digest(&xml).into(),
            };
            let root = std::env::temp_dir().join(format!("native-target-ee-{}", Uuid::new_v4()));
            eprintln!("test_workspace={}", root.display());
            std::fs::create_dir(&root).unwrap();
            let path = root.join("source.sqlite");
            let cache_path = root.join("extents.sqlite");
            ImportSpool::build(&xml[..], &path, expected).unwrap();
            VolumeExtentCache::build(&xml[..], &cache_path, expected).unwrap();
            let spool = ImportSpool::open(&path, expected).unwrap();
            let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
            spool.verify_extent_cache(&cache).unwrap();
            let proof = NativeMediaSource {
                identity: spool.identity().clone(),
                files: cache.file_count().unwrap(),
                paths: spool.candidate_stats().entries,
            };
            assert_eq!(proof.files, 6);
            let import = start_target(
                &catalog,
                &serial,
                route,
                pool.id,
                100,
                if merge { "AA0019L8" } else { "AA0018L8" },
                ("/custom", merge),
            )
            .await;
            catalog
                .bind_native_media_source(&import, 100, &proof)
                .await
                .unwrap();
            let entries = target_entries(&import, &spool);
            assert!(
                entries
                    .iter()
                    .all(|e| !e.path.logical_path.starts_with("/.LTFSEE_DATA"))
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|e| e.path.current().candidate.kind == NodeKind::File)
                    .count(),
                3
            );
            let evidence = entries
                .iter()
                .map(|e| {
                    (
                        e.path.logical_path.clone(),
                        e.current_attributes.clone(),
                        e.source_file_uids.clone(),
                        e.source_data.clone(),
                    )
                })
                .collect::<Vec<_>>();
            catalog
                .begin_native_media_projection(&import, 100)
                .await
                .unwrap();
            for entry in &entries {
                catalog
                    .project_native_media_entry(&import, 100, entry, true)
                    .await
                    .unwrap();
            }
            catalog
                .finish_native_media(&import, 100, &proof)
                .await
                .unwrap();
            for (entry, original) in entries.iter().zip(evidence) {
                assert_eq!(
                    (
                        &entry.path.logical_path,
                        &entry.current_attributes,
                        &entry.source_file_uids,
                        &entry.source_data
                    ),
                    (&original.0, &original.1, &original.2, &original.3)
                );
                if entry.path.current().candidate.kind != NodeKind::File {
                    continue;
                }
                let base = if merge { "/custom" } else { "/custom/AA0018L8" };
                let target = format!("{base}{}", entry.path.logical_path);
                assert!(target.starts_with(&format!("{base}/ee-gpfs/ee-rea-R-")));
                let id = import_materialize::import_inode(ns, pool.id, &target);
                let tr = catalog.db.create_trx().unwrap();
                let head =
                    decode_head(&tr.get(&head_key(ns, id), false).await.unwrap().unwrap()).unwrap();
                let copy = decode_copy(
                    &tr.get(&copy_key(ns, id, head.copy_id), false)
                        .await
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                let resolved = spool
                    .resolve_file_data(&entry.path.current().candidate.physical_path)
                    .unwrap()
                    .unwrap();
                assert_eq!(copy.ltfs_file_uid, resolved.data_file_uid);
                assert!([2, 3, 4].contains(&copy.ltfs_file_uid));
                assert_eq!(copy.size, resolved.length);
                let media = copy.media.unwrap();
                assert_eq!(media.physical_path, resolved.data_path);
                assert!(media.physical_path.starts_with(".LTFSEE_DATA/"));
                assert!(media.ee_alias);
                let original_physical = cache.required_file_by_uid(copy.ltfs_file_uid).unwrap();
                let data = index.find_file(&media.physical_path).unwrap();
                assert_eq!(original_physical.extents, data.extents);
                assert_eq!(original_physical.length, data.length);
                assert_eq!(
                    cache
                        .resolve_data_file(&entry.path.current().candidate.physical_path)
                        .unwrap()
                        .1,
                    original_physical
                );
                assert!(
                    catalog
                        .resolve_active_path_for_route(
                            &serial,
                            pool_catalog_path(pool.id, target.as_bytes())
                                .unwrap()
                                .as_slice(),
                            route
                        )
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            assert!(
                catalog
                    .resolve_active_path_for_route(
                        &serial,
                        format!("/{}/custom/AA0018L8/.LTFSEE_DATA", pool.id).as_bytes(),
                        route
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(spool);
            drop(cache);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；目标映射、后缀分页、owner恢复与有界撤销，不操作设备"]
    async fn native_target_paths_merge_suffix_takeover_and_rollback() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let ns = route.route.namespace;
        let root = std::env::temp_dir().join(format!("native-target-{}", Uuid::new_v4()));
        eprintln!("test_workspace={}", root.display());
        std::fs::create_dir(&root).unwrap();
        let (spool, proof) = target_source(&root, 18);
        let import = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            100,
            "TARGET0L8",
            ("/IMPORT", false),
        )
        .await;
        catalog
            .bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        let entries = target_entries(&import, &spool);
        for entry in &entries {
            catalog
                .project_native_media_entry(&import, 100, entry, false)
                .await
                .unwrap();
        }
        catalog
            .begin_native_media_projection(&import, 100)
            .await
            .unwrap();
        catalog
            .project_native_media_entry(&import, 100, &entries[0], true)
            .await
            .unwrap();
        let progress = catalog.native_media_status(&import, 100).await.unwrap();
        assert_eq!(progress["rows"], 1);
        assert_eq!(progress["after"], "/docs");
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .project_native_media_entry(&import, 100, &entries[1], true)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::OwnerFenced)
        ));
        let resumed = catalog
            .start_native_media(&serial, route, 101, 1, import.request.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .bind_native_media_source(&resumed, 101, &proof)
            .await
            .unwrap();
        for entry in &entries {
            catalog
                .project_native_media_entry(&resumed, 101, entry, true)
                .await
                .unwrap();
        }
        let done = catalog
            .finish_native_media(&resumed, 101, &proof)
            .await
            .unwrap();
        assert_eq!(done["result"]["source"]["paths"], proof.paths);
        let directory = import_materialize::import_inode(ns, pool.id, "/IMPORT/TARGET0L8/docs");
        let tr = catalog.db.create_trx().unwrap();
        let original_directory = tr
            .get(&node_key(ns, directory), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        assert!(
            tr.get(&target::header_key(&resumed), false)
                .await
                .unwrap()
                .is_none()
        );
        drop(tr);
        let old = catalog
            .resolve_active_read_source_for_owner(
                &serial,
                format!("/{}/IMPORT/TARGET0L8/docs/conflict.txt", pool.id).as_bytes(),
                route,
                101,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.placement().volume_id, proof.index().volume_uuid);

        // All 18 original filenames remain live. A second source must cross a
        // suffix probe transaction boundary and preserve the chosen mapping.
        let (second_spool, second_proof) = target_source(&root, 1);
        let merge = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            101,
            "TARGET1L8",
            ("/IMPORT/TARGET0L8", true),
        )
        .await;
        catalog
            .bind_native_media_source(&merge, 101, &second_proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&merge, 101)
            .await
            .unwrap();
        let merged = target_entries(&merge, &second_spool);
        catalog
            .project_native_media_entry(&merge, 101, &merged[0], true)
            .await
            .unwrap();
        let tr = catalog.db.create_trx().unwrap();
        state(&tr, &merge, 101).await.unwrap();
        assert!(
            target::choose(
                &tr,
                &merge,
                &merged[1],
                &entry_fingerprint(&merged[1], &second_proof)
            )
            .await
            .unwrap()
            .is_none()
        );
        tr.commit().await.unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 102)
            .await
            .unwrap();
        let new_owner = catalog
            .start_native_media(&serial, route, 102, 1, merge.request.operation())
            .await
            .unwrap()
            .unwrap();
        catalog
            .bind_native_media_source(&new_owner, 102, &second_proof)
            .await
            .unwrap();
        catalog
            .project_native_media_entry(&new_owner, 102, &merged[1], true)
            .await
            .unwrap();
        // Replaying after a lost commit response must not pick suffix _19.
        catalog
            .project_native_media_entry(&new_owner, 102, &merged[1], true)
            .await
            .unwrap();
        let tr = catalog.db.create_trx().unwrap();
        let mapping = target::saved(
            &tr,
            &new_owner,
            "/docs/conflict.txt",
            &entry_fingerprint(&merged[1], &second_proof),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(mapping.target, "/IMPORT/TARGET0L8/docs/conflict.txt_18");
        let copy_raw = tr
            .get(&head_key(ns, mapping.id), false)
            .await
            .unwrap()
            .unwrap();
        let head = decode_head(&copy_raw).unwrap();
        let copy_raw = tr
            .get(&copy_key(ns, mapping.id, head.copy_id), false)
            .await
            .unwrap()
            .unwrap();
        let physical = decode_copy(&copy_raw).unwrap();
        assert_eq!(physical.volume_id, second_proof.index().volume_uuid);
        assert_eq!(physical.ltfs_file_uid, merged[1].source_file_uids[0]);
        assert_eq!(
            physical.media.as_ref().unwrap().physical_path,
            "docs/conflict.txt"
        );
        assert_eq!(
            tr.get(&node_key(ns, directory), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            original_directory.as_slice()
        );
        drop(tr);
        catalog
            .finish_native_media(&new_owner, 102, &second_proof)
            .await
            .unwrap();
        assert!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    format!("/{}/IMPORT/TARGET0L8/docs/conflict.txt_19", pool.id).as_bytes(),
                    route
                )
                .await
                .unwrap()
                .is_none()
        );

        let (third_spool, third_proof) = target_source(&root, 1);
        let cancel = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            102,
            "TARGET2L8",
            ("/custom/deep", false),
        )
        .await;
        catalog
            .bind_native_media_source(&cancel, 102, &third_proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&cancel, 102)
            .await
            .unwrap();
        for entry in target_entries(&cancel, &third_spool) {
            catalog
                .project_native_media_entry(&cancel, 102, &entry, true)
                .await
                .unwrap();
        }
        let tr = catalog.db.create_trx().unwrap();
        let synthetic_key = node_key(ns, import_materialize::import_inode(ns, pool.id, "/custom"));
        let original = tr
            .get(&synthetic_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.set(&synthetic_key, b"changed-external-record");
        tr.commit().await.unwrap();
        catalog
            .abort_native_media(&cancel, 102, None)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .rollback_native_media_page(&cancel, 102)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&synthetic_key, &original);
        tr.commit().await.unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 103)
            .await
            .unwrap();
        let cancel_owner = catalog
            .start_native_media(&serial, route, 103, 1, cancel.request.operation())
            .await
            .unwrap()
            .unwrap();
        while !catalog
            .rollback_native_media_page(&cancel_owner, 103)
            .await
            .unwrap()
        {}
        assert!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    format!("/{}/custom", pool.id).as_bytes(),
                    route
                )
                .await
                .unwrap()
                .is_none()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            !requests::prefix_has_rows(&tr, &target::prefix(&cancel_owner))
                .await
                .unwrap()
        );
        assert_eq!(
            tr.get(&node_key(ns, directory), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            original_directory.as_slice()
        );
        drop(tr);

        // Empty media still creates the explicit virtual import directory,
        // without inventing a source row or changing physical file totals.
        let (_empty_spool, empty_proof) = target_source(&root, 0);
        let empty = start_target(
            &catalog,
            &serial,
            route,
            pool.id,
            103,
            "TARGET3L8",
            ("/empty/base", true),
        )
        .await;
        catalog
            .bind_native_media_source(&empty, 103, &empty_proof)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&empty, 103)
            .await
            .unwrap();
        let done = catalog
            .finish_native_media(&empty, 103, &empty_proof)
            .await
            .unwrap();
        assert_eq!(done["result"]["source"]["paths"], 0);
        assert!(matches!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    format!("/{}/empty/base", pool.id).as_bytes(),
                    route
                )
                .await
                .unwrap(),
            Some(ResolvedPath::Directory { .. })
        ));
        drop(spool);
        drop(second_spool);
        drop(third_spool);
        drop(_empty_spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；验证写前预约与介质接入互斥，不操作设备"]
    async fn native_intake_and_write_reservations_exclude_each_other() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let ns = route.route.namespace;
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "reservation-guard".into(), 'b');
        index.self_location.start_block = 5;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: 1,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let root = std::env::temp_dir().join(format!("intake-reservation-{}", Uuid::new_v4()));
        eprintln!("test_workspace={}", root.display());
        std::fs::create_dir(&root).unwrap();
        let spool_path = root.join("source.sqlite");
        ImportSpool::build(&xml[..], &spool_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let source = NativeMediaSource {
            identity: spool.identity().clone(),
            files: 0,
            paths: 0,
        };
        let ready = start(&catalog, &serial, route, pool.id, 100, "READY0L8").await;
        catalog
            .bind_native_media_source(&ready, 100, &source)
            .await
            .unwrap();
        catalog
            .begin_native_media_projection(&ready, 100)
            .await
            .unwrap();
        catalog
            .finish_native_media(&ready, 100, &source)
            .await
            .unwrap();
        let spec = ActiveUploadSpec {
            pool_id: pool.id,
            path: b"/pending".to_vec(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: 3,
            digest: ContentDigest::Sha256(Sha256::digest(b"one").into()),
        };
        let reserved = catalog
            .prepare_active_upload(&serial, route, 100, "READY0L8", &spec)
            .await
            .unwrap();
        assert_eq!(
            catalog.reserve_active_file(&reserved).await.unwrap(),
            PublishOutcome::Registered
        );
        // The reservation is still 0, before an I receipt or F batch exists.
        let tr = catalog.db.create_trx().unwrap();
        for kind in *b"IF" {
            assert!(
                !requests::prefix_has_rows(&tr, &[key(kind), ns.as_bytes().to_vec()].concat())
                    .await
                    .unwrap()
            );
        }
        assert!(
            requests::prefix_has_rows(
                &tr,
                &requests::epoch_prefix(b'0', ns, reserved.request_epoch)
            )
            .await
            .unwrap()
        );
        let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":"WAIT00L8","volume":null,"path":"/","merge_tapes":true}})).unwrap();
        assert_eq!(
            catalog
                .admit_management(&serial, route, 100, &request)
                .await
                .unwrap()["status"],
            "queued"
        );
        assert!(
            matches!(
                catalog
                    .start_native_media(&serial, route, 100, 1, request.operation())
                    .await
                    .as_ref()
                    .map_err(|error| error.domain()),
                Err(CatalogError::Incomplete)
            ),
            "写前0预约尚未升级为I，也必须阻止接入"
        );
        assert_eq!(
            catalog
                .admit_management(&serial, route, 100, &request)
                .await
                .unwrap()["status"],
            "queued"
        );
        assert!(
            catalog
                .tape_assignment(&serial, "WAIT00L8")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .resolve_active_metadata_for_route(
                    &serial,
                    format!("/{}", pool.id).as_bytes(),
                    route
                )
                .await
                .unwrap()
                .is_some()
        );

        assert_eq!(
            catalog
                .cancel_active_file_reservation(&reserved)
                .await
                .unwrap(),
            PublishOutcome::RequestCancelled
        );
        let pending = catalog
            .prepare_active_upload(
                &serial,
                route,
                100,
                "READY0L8",
                &ActiveUploadSpec {
                    operation_id: Uuid::new_v4(),
                    ..spec
                },
            )
            .await
            .unwrap();
        let intake = catalog
            .start_native_media(&serial, route, 100, 1, request.operation())
            .await
            .unwrap()
            .unwrap();
        // Reverse order: a prepared but unreserved upload must recheck the
        // pool gate inside its admission transaction, without creating rows.
        assert!(matches!(
            catalog
                .check_active_pool_gate(&serial, route, 100, pool.id)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        assert!(
            matches!(
                catalog
                    .reserve_active_file(&pending)
                    .await
                    .as_ref()
                    .map_err(|error| error.domain()),
                Err(CatalogError::PoolImporting)
            ),
            "旧prepared上传不能绕过正在接入的池屏障"
        );
        // The direct receipt admission entry must honor the same exclusion.
        // This synthetic receipt tests admission only, and proves no media S4.
        let mut receipt = CommitReceipt {
            namespace: ns,
            parent: pending.parent,
            name: pending.name.clone(),
            file_id: pending.file_id,
            expected_version: pending.expected_version,
            operation_id: pending.operation_id,
            request_epoch: pending.request_epoch,
            copy_id: Uuid::nil(),
            library_serial: serial.clone(),
            pool_id: Some(pool.id),
            owner_epoch: 100,
            volume_id: pending.volume_id,
            volume_generation: pending.volume_generation,
            volume_scan_id: pending.volume_scan_id,
            ltfs_file_uid: 2,
            size: pending.expected_size,
            content_digest: pending.expected_digest,
            metadata_digest: None,
        };
        receipt.copy_id = derive_copy_id(&receipt);
        assert!(matches!(
            catalog
                .register_active_request(&receipt)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            !requests::prefix_has_rows(
                &tr,
                &requests::epoch_prefix(b'0', ns, pending.request_epoch)
            )
            .await
            .unwrap()
        );
        // A terminal retry is still returned before consulting the busy gate.
        assert_eq!(
            catalog.reserve_active_file(&reserved).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        catalog
            .cancel_management(&serial, route, 100, 1, request.operation())
            .await
            .unwrap();
        assert!(
            catalog
                .rollback_native_media_page(&intake, 100)
                .await
                .unwrap()
        );
        assert_eq!(
            catalog.reserve_active_file(&pending).await.unwrap(),
            PublishOutcome::Registered
        );
        catalog
            .cancel_active_file_reservation(&pending)
            .await
            .unwrap();
        for n in 0..8 {
            let spec = ActiveUploadSpec {
                pool_id: pool.id,
                path: format!("/race{n}").into_bytes(),
                operation_id: Uuid::new_v4(),
                expected_version: None,
                size: 3,
                digest: pending.expected_digest,
            };
            let intent = catalog
                .prepare_active_upload(&serial, route, 100, "READY0L8", &spec)
                .await
                .unwrap();
            let barcode = format!("RACE{n:02}L8");
            let request = ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-import","pool":pool.id.to_string(),"barcode":barcode,"volume":null,"path":"/","merge_tapes":true}})).unwrap();
            catalog
                .admit_management(&serial, route, 100, &request)
                .await
                .unwrap();
            let (reservation, intake) = tokio::join!(
                catalog.reserve_active_file(&intent),
                catalog.start_native_media(&serial, route, 100, 1, request.operation())
            );
            match (reservation, intake) {
                (Ok(PublishOutcome::Registered), Err(error))
                    if matches!(
                        error.domain(),
                        CatalogError::Incomplete | CatalogError::PoolBusy
                    ) =>
                {
                    catalog
                        .cancel_active_file_reservation(&intent)
                        .await
                        .unwrap();
                    assert_eq!(
                        catalog
                            .cancel_management(&serial, route, 100, 1, request.operation())
                            .await
                            .unwrap()["status"],
                        "cancelled"
                    );
                }
                (Err(error), Ok(Some(intake)))
                    if matches!(
                        error.domain(),
                        CatalogError::PoolImporting | CatalogError::PoolBusy
                    ) =>
                {
                    catalog
                        .cancel_management(&serial, route, 100, 1, request.operation())
                        .await
                        .unwrap();
                    assert!(
                        catalog
                            .rollback_native_media_page(&intake, 100)
                            .await
                            .unwrap()
                    );
                }
                unexpected => panic!("并发接入与预约只能有一个成功: {unexpected:?}"),
            }
            assert!(
                catalog
                    .tape_assignment(&serial, &barcode)
                    .await
                    .unwrap()
                    .is_none()
            );
            catalog
                .check_active_pool_gate(&serial, route, 100, pool.id)
                .await
                .unwrap();
        }
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；验证原生接入事务，不操作设备"]
    async fn native_media_projection_is_hidden_resumable_and_reversible() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&catalog).await;
        let ns = route.route.namespace;
        let sibling = PoolRecord {
            id: Uuid::new_v4(),
            name: "sibling".into(),
            ..pool.clone()
        };
        catalog
            .create_active_pool(&serial, route, 100, &sibling)
            .await
            .unwrap();
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "native-test".into(), 'b');
        index.self_location.start_block = 5;
        let mut dir = DirectoryNode {
            name: "shared".into(),
            meta: index.root.meta.clone(),
            ..Default::default()
        };
        dir.meta.file_uid = 2;
        let mut data = FileNode {
            name: "data".into(),
            meta: index.root.meta.clone(),
            ..Default::default()
        };
        data.meta.file_uid = 3;
        let mut link = FileNode {
            name: "link".into(),
            symlink: Some("data".into()),
            meta: index.root.meta.clone(),
            ..Default::default()
        };
        link.meta.file_uid = 4;
        dir.files = vec![data, link];
        index.root.subdirs.push(dir);
        index.highest_file_uid = 4;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: 1,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let root = std::env::temp_dir().join(format!("native-intake-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let spool_path = root.join("source.sqlite");
        let cache_path = root.join("cache.sqlite");
        ImportSpool::build(&xml[..], &spool_path, expected).unwrap();
        VolumeExtentCache::build(&xml[..], &cache_path, expected).unwrap();
        let spool = ImportSpool::open(&spool_path, expected).unwrap();
        let cache = VolumeExtentCache::open(&cache_path, expected).unwrap();
        spool.verify_extent_cache(&cache).unwrap();
        let proof = NativeMediaSource {
            identity: spool.identity().clone(),
            files: cache.file_count().unwrap(),
            paths: spool.candidate_stats().entries,
        };
        assert_eq!(proof.paths, 3);
        // A directory created by an earlier operation has a different identity
        // and attributes. Intake must attach children to that exact identity.
        let existing = Uuid::new_v4();
        let pool_root = import_materialize::import_inode(ns, pool.id, "/");
        let attributes = NodeAttributes::from_directory(&index.root).unwrap();
        let node = encode_node(&Node {
            kind: NodeKind::Directory,
            directory_version: Some(1),
            parent: pool_root,
            name: b"shared".to_vec(),
            attributes: Some(attributes),
        });
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&child_key(ns, pool_root, b"shared"), existing.as_bytes());
        tr.set(&node_key(ns, existing), &node);
        tr.commit().await.unwrap();
        let intake = start(&catalog, &serial, route, pool.id, 100, "NATIVE0L8").await;
        let path = format!("/{}/shared", pool.id);
        assert!(matches!(
            catalog
                .resolve_active_metadata_for_route(&serial, path.as_bytes(), route)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        assert!(matches!(
            catalog
                .resolve_active_path_for_route(&serial, path.as_bytes(), route)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        assert!(
            catalog
                .resolve_active_path_for_route(
                    &serial,
                    format!("/{}", sibling.id).as_bytes(),
                    route
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            catalog
                .list_active_children_for_route(&serial, path.as_bytes(), None, 100, route)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        assert!(matches!(
            catalog
                .list_active_objects_for_route(
                    &serial,
                    route,
                    ObjectListRequest {
                        pool: pool.id,
                        prefix: "",
                        delimiter: "",
                        max_keys: 1000,
                        start_after: ""
                    }
                )
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        catalog
            .bind_native_media_source(&intake, 100, &proof)
            .await
            .unwrap();
        let candidates = spool.candidate_page(None, 8).unwrap().candidates;
        let entries = candidates
            .into_iter()
            .map(|c| intake.entry(&spool, c).unwrap())
            .collect::<Vec<_>>();
        for entry in &entries {
            catalog
                .project_native_media_entry(&intake, 100, entry, false)
                .await
                .unwrap();
        }
        catalog
            .begin_native_media_projection(&intake, 100)
            .await
            .unwrap();
        for entry in &entries[..2] {
            catalog
                .project_native_media_entry(&intake, 100, entry, true)
                .await
                .unwrap();
        }
        assert!(
            catalog
                .finish_native_media(&intake, 100, &proof)
                .await
                .is_err()
        );
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        reopened
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .native_media_status(&intake, 100)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::OwnerFenced)
        ));
        let resumed = reopened
            .start_native_media(&serial, route, 101, 1, intake.request.operation())
            .await
            .unwrap()
            .unwrap();
        for entry in &entries {
            reopened
                .project_native_media_entry(&resumed, 101, entry, true)
                .await
                .unwrap();
        }
        let done = reopened
            .finish_native_media(&resumed, 101, &proof)
            .await
            .unwrap();
        assert_eq!(done["status"], "succeeded");
        assert_eq!(
            reopened
                .admit_management(&serial, route, 101, intake.request())
                .await
                .unwrap(),
            done
        );
        assert!(
            matches!(reopened.resolve_active_path_for_route(&serial,path.as_bytes(),route).await.unwrap(),Some(ResolvedPath::Directory{id,..}) if id==existing)
        );
        let tr = reopened.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&node_key(ns, existing), false)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            node.as_slice()
        );
        let resolved = reopened
            .resolve_active_read_source_for_owner(
                &serial,
                format!("{path}/data").as_bytes(),
                route,
                101,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.placement().volume_id, expected.volume_uuid);
        assert!(
            reopened
                .volume_placement(expected.volume_uuid)
                .await
                .unwrap()
                .is_some()
        );

        // A second physical UUID cannot be claimed from another library, and
        // a published UUID cannot be silently attached a second time.
        let (other_serial, other_route, other_pool) = fixture(&reopened).await;
        let second = start(
            &reopened,
            &other_serial,
            other_route,
            other_pool.id,
            100,
            "NATIVE1L8",
        )
        .await;
        assert!(
            reopened
                .bind_native_media_source(&second, 100, &proof)
                .await
                .is_err()
        );
        reopened
            .abort_native_media(&second, 100, Some("卷身份已存在"))
            .await
            .unwrap();
        assert!(
            reopened
                .rollback_native_media_page(&second, 100)
                .await
                .unwrap()
        );

        // A fresh source is staged and cancelled. Metadata becomes visible
        // again only after bounded undo; preexisting content is unchanged.
        let cancel = start(&reopened, &serial, route, pool.id, 101, "NATIVE2L8").await;
        let mut other_proof = proof.clone();
        let mut head = other_proof.identity.head();
        head.volume_uuid = Uuid::new_v4();
        other_proof.identity = IndexSourceIdentity::new(head, Vec::new()).unwrap();
        reopened
            .bind_native_media_source(&cancel, 101, &other_proof)
            .await
            .unwrap();
        let concurrent = start(
            &reopened,
            &other_serial,
            other_route,
            other_pool.id,
            100,
            "NATIVE3L8",
        )
        .await;
        assert!(matches!(
            reopened
                .bind_native_media_source(&concurrent, 100, &other_proof)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::Invalid(_))
        ));
        reopened
            .abort_native_media(&concurrent, 100, None)
            .await
            .unwrap();
        assert!(
            reopened
                .rollback_native_media_page(&concurrent, 100)
                .await
                .unwrap()
        );
        let mut cancellation_entries = entries.clone();
        for entry in &mut cancellation_entries {
            entry.path.sources[0].volume_id = other_proof.index().volume_uuid;
            entry.path.sources[0].barcode = cancel.barcode().into();
        }
        // Existing files are preserved; materialization will choose a suffix.
        reopened
            .project_native_media_entry(&cancel, 101, &cancellation_entries[1], false)
            .await
            .unwrap();
        // A new directory and a tombstone are both removed by their undo journals.
        let mut directory = cancellation_entries[0].clone();
        directory.path.logical_path = "/temporary".into();
        directory.path.sources[0].candidate.logical_path = "/temporary".into();
        reopened
            .begin_native_media_projection(&cancel, 101)
            .await
            .unwrap();
        reopened
            .project_native_media_entry(&cancel, 101, &directory, true)
            .await
            .unwrap();
        let mut tombstone = cancellation_entries[1].clone();
        tombstone.path.logical_path = "/zz_deleted".into();
        let candidate = &mut tombstone.path.sources[0].candidate;
        candidate.logical_path = "/zz_deleted".into();
        candidate.physical_path = crate::ltfs::volume::tombstone_path("/zz_deleted");
        candidate.deleted = true;
        tombstone.source_data = Some(vec![StagedSourceData::NoCopy]);
        reopened
            .project_native_media_entry(&cancel, 101, &tombstone, true)
            .await
            .unwrap();
        let temporary_id = import_materialize::import_inode(ns, pool.id, "/temporary");
        let temporary_key = node_key(ns, temporary_id);
        let tr = reopened.db.create_trx().unwrap();
        let original = tr
            .get(&temporary_key, false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        tr.set(&temporary_key, b"corrupt-external-change");
        tr.commit().await.unwrap();
        let cancelled = reopened
            .cancel_management(&serial, route, 101, 1, cancel.request.operation())
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "running");
        assert!(
            reopened
                .begin_native_media_projection(&cancel, 101)
                .await
                .is_err()
        );
        assert!(matches!(
            reopened
                .rollback_native_media_page(&cancel, 101)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            reopened
                .resolve_active_metadata_for_route(&serial, path.as_bytes(), route)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolImporting)
        ));
        let tr = reopened.db.create_trx().unwrap();
        tr.set(&temporary_key, &original);
        tr.commit().await.unwrap();
        assert!(
            !reopened
                .rollback_native_media_page(&cancel, 101)
                .await
                .unwrap()
        );
        assert!(
            reopened
                .rollback_native_media_page(&cancel, 101)
                .await
                .unwrap()
        );
        assert!(
            reopened
                .resolve_active_path_for_route(
                    &serial,
                    format!("/{}/temporary", pool.id).as_bytes(),
                    route
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .resolve_active_path_for_route(&serial, format!("{path}/data").as_bytes(), route)
                .await
                .unwrap()
                .is_some()
        );
        let tr = reopened.db.create_trx().unwrap();
        assert!(
            tr.get(
                &import_materialize::tombstone_key(ns, pool.id, "/zz_deleted"),
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            reopened
                .tape_assignment(&serial, "NATIVE2L8")
                .await
                .unwrap()
                .is_none()
        );
        drop(cache);
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；回收池写屏障/原请求重放/owner接任，不操作真实设备"]
    async fn reclaim_reservation_fences_pool_writes_and_survives_new_owner() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&c).await;
        let root = std::env::temp_dir().join(format!("reclaim-reservation-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let (_spool, proof) = target_source(&root, 0);
        let import = start_target(&c, &serial, route, pool.id, 100, "RECLAIML8", ("/", true)).await;
        c.bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        c.finish_native_media(&import, 100, &proof).await.unwrap();
        let assignment = c
            .tape_assignment(&serial, "RECLAIML8")
            .await
            .unwrap()
            .unwrap();
        let spec = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":assignment.barcode,"volume":assignment.volume_id.unwrap().to_string(),"revision":assignment.revision,"confirm_format":true,"unassign":false})).unwrap();
        let operation = Uuid::new_v4();
        let reserved = c
            .reserve_reclaim_pool(&serial, route, 100, 1, operation, &spec)
            .await
            .unwrap();
        let retried = c
            .reserve_reclaim_pool(&serial, route, 100, 1, operation, &spec)
            .await
            .unwrap();
        assert_eq!(reserved.intent(), retried.intent());
        assert!(
            c.step_reclaim_walk(&reserved, 100)
                .await
                .unwrap()
                .complete()
        );
        assert_eq!(
            c.reserved_reclaim_status(&reserved, 100).await.unwrap()["phase"],
            "reserved"
        );
        let upload = ActiveUploadSpec {
            pool_id: pool.id,
            path: b"/new".to_vec(),
            operation_id: Uuid::new_v4(),
            expected_version: None,
            size: 0,
            digest: ContentDigest::Sha256(Sha256::digest([]).into()),
        };
        assert!(matches!(
            c.prepare_active_upload(&serial, route, 100, spec.barcode(), &upload)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        assert!(matches!(
            c.check_active_pool_gate(&serial, route, 100, pool.id)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        assert!(
            c.resolve_active_path_for_route(
                &serial,
                &pool_catalog_path(pool.id, b"/").unwrap(),
                route
            )
            .await
            .unwrap()
            .is_some(),
            "回收写屏障不关闭读取"
        );
        let disabled=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":route.route.namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"tape","pool":pool.id.to_string(),"volume":spec.volume().to_string(),"barcode":spec.barcode(),"revision":spec.revision(),"patch":{"online":false}}})).unwrap();
        assert!(matches!(
            c.admit_management(&serial, route, 100, &disabled)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            c.tape_assignment(&serial, spec.barcode())
                .await
                .unwrap()
                .unwrap(),
            assignment
        );
        let other = PoolRecord {
            id: Uuid::new_v4(),
            name: "other".into(),
            ..pool.clone()
        };
        c.create_active_pool(&serial, route, 100, &other)
            .await
            .unwrap();
        c.check_active_pool_gate(&serial, route, 100, other.id)
            .await
            .unwrap();
        assert!(matches!(
            c.reserve_reclaim_pool(&serial, route, 100, 1, Uuid::new_v4(), &spec)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        c.claim_active_owner_epoch(&serial, 101).await.unwrap();
        assert!(c.reserved_reclaim_status(&reserved, 100).await.is_err());
        assert!(c.release_reserved_reclaim(&reserved, 100).await.is_err());
        assert!(
            c.reserved_reclaim_status(&reserved, 101).await.is_err(),
            "接任不能仅用旧句柄绕过显式领取"
        );
        let resumed = c
            .reserve_reclaim_pool(&serial, route, 101, 1, operation, &spec)
            .await
            .unwrap();
        assert_eq!(reserved.intent(), resumed.intent());
        assert_eq!(
            c.reserved_reclaim_status(&resumed, 101).await.unwrap()["owner_epoch"],
            101
        );
        c.release_reserved_reclaim(&resumed, 101).await.unwrap();
        c.release_reserved_reclaim(&resumed, 101).await.unwrap();
        let mut scan_prefix = management::prefix(route.route.namespace, b'R');
        scan_prefix.push(b's');
        assert!(
            !requests::prefix_has_rows(&c.db.create_trx().unwrap(), &scan_prefix)
                .await
                .unwrap(),
            "无子写入取消应原子清理扫描检查点"
        );
        c.check_active_pool_gate(&serial, route, 101, pool.id)
            .await
            .unwrap();
        let mut rebound = spec.to_json();
        rebound["volume"] = json!(Uuid::new_v4().to_string());
        let rebound = ReclaimSpec::parse(&rebound).unwrap();
        assert!(
            c.reserve_reclaim_pool(&serial, route, 101, 1, Uuid::new_v4(), &rebound)
                .await
                .is_err()
        );
        c.check_active_pool_gate(&serial, route, 101, pool.id)
            .await
            .unwrap();
        let pending = c
            .prepare_active_upload(&serial, route, 101, spec.barcode(), &upload)
            .await
            .unwrap();
        assert_eq!(
            c.reserve_active_file(&pending).await.unwrap(),
            PublishOutcome::Registered
        );
        assert!(
            matches!(
                c.reserve_reclaim_pool(&serial, route, 101, 1, Uuid::new_v4(), &spec)
                    .await
                    .as_ref()
                    .map_err(|error| error.domain()),
                Err(CatalogError::PoolBusy)
            ),
            "旧在途文件未结清不能取得回收写屏障"
        );
        c.check_active_pool_gate(&serial, route, 101, pool.id)
            .await
            .unwrap();
        drop(_spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(super) fn current_child_source(
        root: &std::path::Path,
        barcode: &str,
        with_file: bool,
    ) -> (
        crate::ltfs::recovery::CapturedIndexSource,
        ImportSpool,
        NativeMediaSource,
    ) {
        use crate::local_read_storage::{CacheBuild, ReadStorageBudget};
        use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
        use crate::ltfs::recovery::{RecoveryBudget, capture_index_source};
        use crate::ltfs::volume::{LtfsVolume, read_label_only};
        use crate::scsi::sim::{SimCartridge, SimLibrary};
        use crate::tape::commands::TapeDrive;
        let library = SimLibrary::new(1, 2, 1);
        library
            .insert_cartridge(SimCartridge::blank(barcode, 64 << 20), 0)
            .unwrap();
        library.load_into_drive(barcode, 0).unwrap();
        let device = library.drive(0);
        mkltfs(
            &device,
            &MkltfsOptions {
                volume_id: "CHILD0".into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
        if with_file {
            let mut volume = LtfsVolume::mount(&device).unwrap();
            volume
                .append_file("/docs/conflict.txt", &mut std::io::Cursor::new(b""))
                .unwrap();
            volume.commit().unwrap();
        }
        let captured = capture_index_source(
            &TapeDrive::new(&device),
            &read_label_only(&device).unwrap(),
            &RecoveryBudget::default(),
        )
        .unwrap();
        let storage = ReadStorageBudget {
            index_bytes: 16 << 20,
            cache_bytes: 16 << 20,
            download_bytes: 16 << 20,
            headroom_bytes: 0,
        };
        let directory = root.join(barcode);
        let mut build = CacheBuild::new(&directory, storage).unwrap();
        let path = directory.join("source.sqlite");
        ImportSpool::build_captured(&captured, &path, &mut build).unwrap();
        build.finish().unwrap();
        let head = captured.construct();
        let spool = ImportSpool::open(
            &path,
            ExpectedIndex {
                volume_uuid: head.volume_uuid,
                generation: head.generation,
                location: head.location,
                sha256: head.sha256,
            },
        )
        .unwrap();
        let proof = NativeMediaSource {
            identity: spool.identity().clone(),
            files: spool.stats().files,
            paths: spool.candidate_stats().entries,
        };
        (captured, spool, proof)
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；封存回收子请求，不操作设备"]
    async fn reclaim_children_bind_original_source_and_exact_destination_across_takeover() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&c).await;
        let root = std::env::temp_dir().join(format!("reclaim-children-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut captured_source = None;
        for (barcode, count) in [("RECLAIMS1L8", 1), ("RECLAIMD1L8", 0)] {
            let (captured, spool, proof) = current_child_source(&root, barcode, count != 0);
            if count != 0 {
                captured_source = Some(captured);
            }
            let import = start_target(&c, &serial, route, pool.id, 100, barcode, ("/", true)).await;
            c.bind_native_media_source(&import, 100, &proof)
                .await
                .unwrap();
            c.begin_native_media_projection(&import, 100).await.unwrap();
            for entry in target_entries(&import, &spool) {
                c.project_native_media_entry(&import, 100, &entry, true)
                    .await
                    .unwrap();
            }
            c.finish_native_media(&import, 100, &proof).await.unwrap();
        }
        let source = c
            .tape_assignment(&serial, "RECLAIMS1L8")
            .await
            .unwrap()
            .unwrap();
        let change = ReclaimSpec::parse(&json!({"kind":"media-reclaim","pool":pool.id.to_string(),"barcode":source.barcode,"volume":source.volume_id.unwrap().to_string(),"revision":source.revision,"confirm_format":true,"unassign":false})).unwrap();
        let parent = Uuid::new_v4();
        let reservation = c
            .reserve_reclaim_pool(&serial, route, 100, 1, parent, &change)
            .await
            .unwrap();
        let scanned = loop {
            let state = c.step_reclaim_walk(&reservation, 100).await.unwrap();
            if let Some(file) = state.current() {
                break file.clone();
            }
            assert!(!state.complete());
        };
        assert_eq!(scanned.path(), "/docs/conflict.txt");
        let path = b"/docs/conflict.txt";
        let before = c
            .resolve_active_path_for_route(
                &serial,
                &pool_catalog_path(pool.id, path).unwrap(),
                route,
            )
            .await
            .unwrap()
            .unwrap();
        let ResolvedPath::File { head, .. } = &before else {
            panic!("file");
        };
        let inventory =
            ReclaimSourceInventory::from_captured(captured_source.as_ref().unwrap()).unwrap();
        let cache_directory = root.join("extent-cache");
        let cache_path = cache_directory.join("current-source-extents.sqlite");
        let storage = crate::local_read_storage::ReadStorageBudget {
            index_bytes: 16 << 20,
            cache_bytes: 16 << 20,
            download_bytes: 16 << 20,
            headroom_bytes: 0,
        };
        let mut build =
            crate::local_read_storage::CacheBuild::new(&cache_directory, storage).unwrap();
        crate::catalog::volume_extents::VolumeExtentCache::build_captured(
            captured_source.as_ref().unwrap(),
            &cache_path,
            &mut build,
        )
        .unwrap();
        build.finish().unwrap();
        let original_read = c
            .resolve_active_read_source_for_owner(
                &serial,
                &pool_catalog_path(pool.id, path).unwrap(),
                route,
                100,
            )
            .await
            .unwrap()
            .unwrap()
            .plan_read(&cache_path)
            .unwrap();
        let source_head = c
            .observed_active_volume_index(&serial, source.volume_id.unwrap())
            .await
            .unwrap();
        let source_read = inventory
            .bind_verified_read(
                &reservation,
                &scanned,
                &original_read.read,
                source_head,
                Sha256::digest([]).into(),
            )
            .unwrap();
        let original_read_metadata = original_read.read.clone();
        for case in 0..6 {
            let mut wrong = original_read.read.clone();
            let mut observed = source_head;
            match case {
                0 => wrong.copy.ltfs_file_uid = 1,
                1 => wrong.copy.ltfs_file_uid = u64::MAX,
                2 => wrong.physical.symlink = Some("elsewhere".into()),
                3 => wrong.physical.extents.push(crate::ltfs::index::Extent {
                    partition: 'b',
                    start_block: 99,
                    byte_offset: 0,
                    byte_count: 1,
                    file_offset: 0,
                }),
                4 => observed.sha256[0] ^= 1,
                _ => wrong.copy.content_digest = ContentDigest::Sha256([9; 32]),
            }
            assert!(
                inventory
                    .bind_verified_read(
                        &reservation,
                        &scanned,
                        &wrong,
                        observed,
                        Sha256::digest([]).into()
                    )
                    .is_err()
            );
        }
        assert_eq!(original_read.read, original_read_metadata);
        let spec = ActiveUploadSpec {
            pool_id: pool.id,
            path: path.to_vec(),
            operation_id: scanned.operation(),
            expected_version: Some(head.version),
            size: 0,
            digest: ContentDigest::Sha256(Sha256::digest([]).into()),
        };
        assert!(
            c.prepare_reclaim_upload(&reservation, 100, "RECLAIMS1L8", &spec, &source_read)
                .await
                .is_err()
        );
        let mut altered = spec.clone();
        altered.size = 1;
        assert!(
            c.prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &altered, &source_read)
                .await
                .is_err()
        );
        assert_eq!(
            c.reserved_reclaim_status(&reservation, 100).await.unwrap()["children"],
            0
        );
        // A refused draft cannot consume budget or leave a child/pin behind.
        let policy = c.maintenance_policy(route.route.namespace).await.unwrap();
        let shards = policy.max_pending.min(64) as u16;
        let tr = c.db.create_trx().unwrap();
        let mut quota_fixture = Vec::new();
        for shard in 0..shards {
            let mut quota = key(b'B');
            quota.extend_from_slice(route.route.namespace.as_bytes());
            quota.extend_from_slice(&0u64.to_be_bytes());
            quota.extend_from_slice(&shard.to_be_bytes());
            let original = tr.get(&quota, false).await.unwrap().map(|v| v.to_vec());
            let cap = policy.max_pending / u32::from(shards)
                + u32::from(u32::from(shard) < policy.max_pending % u32::from(shards));
            let full = u64::from(cap).to_be_bytes();
            tr.set(&quota, &full);
            quota_fixture.push((quota, original, full));
        }
        tr.commit().await.unwrap();
        assert!(matches!(
            c.prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            c.reserved_reclaim_status(&reservation, 100).await.unwrap()["children"],
            0
        );
        let mut reverse = management::prefix(route.route.namespace, b'R');
        reverse.push(b'i');
        reverse.extend_from_slice(spec.operation_id.as_bytes());
        let mut pin = requests::epoch_prefix(b'F', route.route.namespace, 1);
        pin.extend_from_slice(spec.operation_id.as_bytes());
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&reverse, false).await.unwrap().is_none());
        assert!(tr.get(&pin, false).await.unwrap().is_none());
        for (quota, original, full) in quota_fixture {
            assert_eq!(tr.get(&quota, false).await.unwrap().unwrap().as_ref(), full);
            match original {
                Some(value) => tr.set(&quota, &value),
                None => tr.clear(&quota),
            }
        }
        tr.commit().await.unwrap();
        let original_pending = c
            .maintenance_status(route.route.namespace)
            .await
            .unwrap()
            .pending;
        let saved = c
            .prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
            .await
            .unwrap();
        let request = saved.request();
        let ResolvedPath::File {
            head: original_head,
            copy: original_copy,
            ..
        } = &before
        else {
            panic!("file");
        };
        for (field, bad) in [
            ("extra", json!(true)),
            ("parent", json!("other-parent")),
            ("sha256", json!(vec![7; 32])),
            (
                "file",
                json!({"path":"/docs/conflict.txt","file":"malformed-uuid","version":1,"size":0,"operation":scanned.operation().to_string()}),
            ),
        ] {
            let mut value = source_read.value().unwrap();
            value[field] = bad;
            assert!(
                ReclaimSourceRead::require_source(
                    &value,
                    &format!(
                        "{:x}",
                        Sha256::digest(reservation.intent().to_string().as_bytes())
                    ),
                    original_head,
                    original_copy,
                    &saved
                )
                .is_err()
            );
        }

        assert!(
            c.active_file_attempt(&request, route)
                .await
                .unwrap()
                .is_none(),
            "Q 必须等挂载恢复完成后才登记"
        );

        c.check_active_upload_gate(route, 100, &request)
            .await
            .unwrap();
        assert!(matches!(
            c.rotate_active_request_epoch(
                route.route.namespace,
                &serial,
                100,
                request.request_epoch
            )
            .await
            .unwrap(),
            crate::managed_catalog::WindowOutcome::Advanced(_)
        ));
        assert_eq!(
            c.prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
                .await
                .unwrap(),
            saved,
            "文件请求代次轮换不能重新创建子任务或改变原物理请求"
        );
        assert_eq!(
            c.advance_active_request_floor(
                route.route.namespace,
                &serial,
                100,
                request.request_epoch,
                request.request_epoch + 1
            )
            .await
            .unwrap(),
            crate::managed_catalog::WindowOutcome::RetainedReferences
        );
        assert_eq!(
            c.maintenance_status(route.route.namespace)
                .await
                .unwrap()
                .pending,
            original_pending + 1,
            "准备重试只保留一个原准入名额"
        );
        let original_attempt = c.active_file_attempt(&request, route).await.unwrap();
        // Removing an active fence must not turn a sealed maintenance child
        // into an ordinary upload. This also models the future parent close.
        let mut gate = management::prefix(route.route.namespace, b'R');
        gate.push(b'p');
        gate.extend_from_slice(pool.id.as_bytes());
        let tr = c.db.create_trx().unwrap();
        let gate_value = tr.get(&gate, false).await.unwrap().unwrap().to_vec();
        tr.clear(&gate);
        tr.commit().await.unwrap();
        assert!(
            c.check_active_upload_gate(route, 100, &request)
                .await
                .is_err(),
            "原回收子身份不能随父屏障消失而获得普通上传准入"
        );
        assert!(c.reserve_active_file(&saved).await.is_err());
        assert_eq!(
            c.active_file_attempt(&request, route).await.unwrap(),
            original_attempt,
            "底层准入拒绝不能新增或改变原 Q 预留"
        );
        let mut ordinary = request.clone();
        ordinary.operation_id = Uuid::new_v4();
        ordinary.request_epoch = request.request_epoch + 1;
        c.check_active_upload_gate(route, 100, &ordinary)
            .await
            .unwrap();
        let mut ordinary_intent = saved.clone();
        ordinary_intent.operation_id = ordinary.operation_id;
        assert_eq!(
            c.reserve_active_file(&ordinary_intent).await.unwrap(),
            PublishOutcome::RequestEpochClosed,
            "普通旧代次请求不能借用回收的保留许可"
        );
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&gate, false).await.unwrap().is_none());
        tr.set(&gate, &gate_value);
        tr.commit().await.unwrap();
        let mut reverse = management::prefix(route.route.namespace, b'R');
        reverse.push(b'i');
        reverse.extend_from_slice(request.operation_id.as_bytes());
        let original_reverse =
            c.db.create_trx()
                .unwrap()
                .get(&reverse, false)
                .await
                .unwrap()
                .unwrap()
                .to_vec();
        for replacement in [None, Some(b"corrupt-child-identity".as_slice())] {
            let tr = c.db.create_trx().unwrap();
            match replacement {
                None => tr.clear(&reverse),
                Some(bytes) => tr.set(&reverse, bytes),
            }
            tr.commit().await.unwrap();
            assert!(
                c.check_active_upload_gate(route, 100, &request)
                    .await
                    .is_err()
            );
            assert!(
                c.prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
                    .await
                    .is_err()
            );
            assert_eq!(
                c.reserved_reclaim_status(&reservation, 100).await.unwrap()["children"],
                1
            );
            let tr = c.db.create_trx().unwrap();
            tr.set(&reverse, &original_reverse);
            tr.commit().await.unwrap();
        }
        assert_eq!(
            c.prepare_reclaim_upload(&reservation, 100, "RECLAIMD1L8", &spec, &source_read)
                .await
                .unwrap(),
            saved
        );
        assert_eq!(
            c.reserved_reclaim_status(&reservation, 100).await.unwrap()["children"],
            1
        );
        assert!(matches!(
            c.release_reserved_reclaim(&reservation, 100)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        for variant in 0..5 {
            let mut changed = request.clone();
            match variant {
                0 => changed.name = b"different".to_vec(),
                1 => changed.expected_size += 1,
                2 => changed.expected_digest = ContentDigest::Sha256([9; 32]),
                3 => changed.barcode = Some("RECLAIMS1L8".into()),
                _ => changed.operation_id = Uuid::new_v4(),
            }
            assert!(
                c.check_active_upload_gate(route, 100, &changed)
                    .await
                    .is_err()
            );
        }
        let mut wrong_physical = saved.clone();
        wrong_physical.volume_id = source.volume_id.unwrap();
        assert!(c.reserve_active_file(&wrong_physical).await.is_err());
        let mut pin = requests::epoch_prefix(b'F', request.namespace, request.request_epoch);
        pin.extend_from_slice(request.operation_id.as_bytes());
        for replacement in [None, Some(b"invalid-pin".as_slice())] {
            let tr = c.db.create_trx().unwrap();
            assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
            match replacement {
                None => tr.clear(&pin),
                Some(value) => tr.set(&pin, value),
            }
            tr.commit().await.unwrap();
            assert!(c.reserve_active_file(&saved).await.is_err());
            assert!(
                c.active_file_attempt(&request, route)
                    .await
                    .unwrap()
                    .is_none()
            );
            let tr = c.db.create_trx().unwrap();
            tr.set(&pin, &[]);
            tr.commit().await.unwrap();
        }
        assert_eq!(
            c.reserve_active_file(&saved).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            c.maintenance_status(route.route.namespace)
                .await
                .unwrap()
                .pending,
            original_pending + 1,
            "Q 复用封存名额，不重复增加 quota"
        );
        assert!(
            matches!(
                c.cancel_unstarted_reclaim_upload(&reservation, 100, &request)
                    .await
                    .as_ref()
                    .map_err(|error| error.domain()),
                Err(CatalogError::PoolBusy)
            ),
            "真实Q已登记时必须走设备恢复，不能当未执行取消"
        );
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            original_pending + 1
        );
        // The I receipt is synthetic: this isolated fixture has not written
        // media. A quota error must leave Q, claims and I entirely intact.
        let mut receipt = CommitReceipt {
            namespace: saved.namespace,
            parent: saved.parent,
            name: saved.name.clone(),
            file_id: saved.file_id,
            expected_version: saved.expected_version,
            operation_id: saved.operation_id,
            request_epoch: saved.request_epoch,
            copy_id: Uuid::nil(),
            library_serial: saved.library_serial.clone(),
            pool_id: saved.pool_id,
            owner_epoch: saved.owner_epoch,
            volume_id: saved.volume_id,
            volume_generation: saved.volume_generation,
            volume_scan_id: saved.volume_scan_id,
            ltfs_file_uid: 1,
            size: saved.expected_size,
            content_digest: saved.expected_digest,
            metadata_digest: None,
        };
        receipt.copy_id = derive_copy_id(&receipt);
        assert_eq!(
            c.complete_active_file(&receipt).await.unwrap(),
            PublishOutcome::Registered
        );
        let intent_key = requests::intent_key(&receipt);
        let tr = c.db.create_trx().unwrap();
        let raw = tr.get(&intent_key, false).await.unwrap().unwrap();
        let permit = requests::matching_permit(&raw, &receipt).unwrap().unwrap();
        let mut quota = key(b'B');
        quota.extend_from_slice(saved.namespace.as_bytes());
        quota.extend_from_slice(&0u64.to_be_bytes());
        quota.extend_from_slice(&permit.to_be_bytes());
        let quota_raw = tr.get(&quota, false).await.unwrap().unwrap().to_vec();
        let mut q = requests::epoch_prefix(b'0', saved.namespace, saved.request_epoch);
        q.extend_from_slice(saved.operation_id.as_bytes());
        let mut reverse = reservation::volume_prefix(saved.volume_id, saved.volume_generation);
        reverse.extend_from_slice(&saved.request_epoch.to_be_bytes());
        reverse.extend_from_slice(saved.operation_id.as_bytes());
        let mut upgraded = reservation::upgraded_prefix(saved.volume_id, saved.volume_generation);
        upgraded.extend_from_slice(&saved.request_epoch.to_be_bytes());
        upgraded.extend_from_slice(saved.operation_id.as_bytes());
        let keys = [
            q,
            reverse,
            reservation::path_key_for(saved.namespace, saved.parent, &saved.name),
            reservation::file_key_for(saved.namespace, saved.file_id),
            intent_key,
            upgraded,
        ];
        let mut original = Vec::new();
        for key in &keys {
            original.push(tr.get(key, false).await.unwrap().unwrap().to_vec());
        }
        tr.clear(&quota);
        tr.commit().await.unwrap();
        assert!(matches!(
            c.cancel_request(&receipt).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = c.db.create_trx().unwrap();
        for (key, value) in keys.iter().zip(&original) {
            assert_eq!(tr.get(key, false).await.unwrap().unwrap().as_ref(), value);
        }
        assert!(
            tr.get(
                &op_key(saved.namespace, saved.request_epoch, saved.operation_id),
                false
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(tr.get(&quota, false).await.unwrap().is_none());
        tr.set(&quota, &quota_raw);
        // Restore only this fixture's injected I to its prior Q-only state.
        tr.clear(&keys[4]);
        tr.clear(&keys[5]);
        tr.commit().await.unwrap();
        c.claim_active_owner_epoch(&serial, 101).await.unwrap();
        assert!(
            c.reclaim_upload_attempt(route, 100, &request)
                .await
                .is_err()
        );
        assert!(
            c.reclaim_upload_attempt(route, 101, &request)
                .await
                .is_err(),
            "接任须先领取原父任务"
        );
        let mut unclaimed = saved.clone();
        unclaimed.owner_epoch = 101;
        assert!(
            matches!(
                c.reserve_active_file(&unclaimed)
                    .await
                    .as_ref()
                    .map_err(|error| error.domain()),
                Err(CatalogError::OwnerFenced)
            ),
            "Q恢复前必须先领取父屏障"
        );
        assert!(
            c.claim_reclaim_reservations_page(&serial, route, 100, None, 1)
                .await
                .is_err()
        );
        assert!(
            c.claim_reclaim_reservations_page(&serial, route, 101, None, 0)
                .await
                .is_err()
        );
        let poison_pool = PoolRecord {
            id: Uuid::from_bytes([255; 16]),
            name: "recovery-poison-test".into(),
            ..pool.clone()
        };
        c.create_active_pool(&serial, route, 101, &poison_pool)
            .await
            .unwrap();
        let mut poison = management::prefix(route.route.namespace, b'R');
        poison.push(b'p');
        poison.extend_from_slice(poison_pool.id.as_bytes());
        let tr = c.db.create_trx().unwrap();
        tr.set(&poison, b"{}");
        tr.commit().await.unwrap();
        assert!(matches!(
            c.claim_reclaim_reservations_page(&serial, route, 101, None, 16)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let mut journal = management::prefix(route.route.namespace, b'R');
        journal.push(b'j');
        journal.extend_from_slice(&1u64.to_be_bytes());
        journal.extend_from_slice(parent.as_bytes());
        let raw =
            c.db.create_trx()
                .unwrap()
                .get(&journal, false)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&raw).unwrap()["owner_epoch"],
            100,
            "页尾损坏不能提交页首任务的接任"
        );
        let tr = c.db.create_trx().unwrap();
        tr.clear(&poison);
        tr.commit().await.unwrap();
        let claimed = c
            .claim_reclaim_reservations_page(&serial, route, 101, None, 1)
            .await
            .unwrap();
        assert_eq!(claimed.items.len(), 1);
        assert!(claimed.next.is_none());
        assert_eq!(claimed.items[0].intent(), reservation.intent());
        assert_eq!(
            c.reserved_reclaim_status(&claimed.items[0], 101)
                .await
                .unwrap()["owner_epoch"],
            101
        );
        let resumed = c
            .reserve_reclaim_pool(&serial, route, 101, 1, parent, &change)
            .await
            .unwrap();
        assert_eq!(
            c.step_reclaim_walk(&resumed, 101).await.unwrap().current(),
            Some(&scanned),
            "接任复用原扫描文件及子请求号"
        );
        let next = c
            .reclaim_upload_attempt(route, 101, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.request(), request);
        assert_eq!(next.owner_epoch, 101);
        assert_eq!(next.volume_id, saved.volume_id);
        assert_eq!(
            c.reserve_active_file(&next).await.unwrap(),
            PublishOutcome::Registered
        );
        assert!(matches!(
            c.release_reserved_reclaim(&resumed, 101)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        assert_eq!(
            c.resolve_active_path_for_route(
                &serial,
                &pool_catalog_path(pool.id, path).unwrap(),
                route
            )
            .await
            .unwrap()
            .unwrap(),
            before,
            "封存与预约未发布新文件头"
        );
        assert!(matches!(
            c.check_active_pool_gate(&serial, route, 101, pool.id)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        let pending_before_cancel = c
            .maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending;
        assert!(
            c.seal_reclaim_upload_cancellation(&resumed, 101, &request)
                .await
                .is_err(),
            "仍在Q/I中的请求没有已恢复取消终态，不能仅清F引用"
        );
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            pending_before_cancel
        );
        // This test never submitted body/index writes. The generic cancellation
        // is therefore known safe; actual interrupted-device recovery is separate.
        assert_eq!(
            c.cancel_active_file_reservation(&next).await.unwrap(),
            PublishOutcome::RequestCancelled
        );
        let after_cancel = c
            .maintenance_status(request.namespace)
            .await
            .unwrap()
            .pending;
        assert_eq!(after_cancel + 1, pending_before_cancel);
        let mut original_p = requests::epoch_prefix(b'P', request.namespace, request.request_epoch);
        original_p.extend_from_slice(request.operation_id.as_bytes());
        let tr = c.db.create_trx().unwrap();
        let original_terminal = tr.get(&original_p, false).await.unwrap().unwrap().to_vec();
        assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
        tr.set(&original_p, b"fixture-not-a-cancelled-P");
        tr.commit().await.unwrap();
        assert!(
            c.seal_reclaim_upload_cancellation(&resumed, 101, &request)
                .await
                .is_err()
        );
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            after_cancel
        );
        let tr = c.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&original_p, false).await.unwrap().unwrap().as_ref(),
            b"fixture-not-a-cancelled-P"
        );
        assert_eq!(tr.get(&pin, false).await.unwrap().as_deref(), Some(&[][..]));
        tr.set(&original_p, &original_terminal);
        tr.commit().await.unwrap();
        let original_children = c
            .reclaim_uploads_page(&resumed, 101, None, 1)
            .await
            .unwrap();
        assert_eq!(
            original_children.items,
            vec![request.clone()],
            "Q已清但父恢复仍须枚举原child"
        );
        assert!(original_children.next.is_none());
        assert!(
            c.reclaim_uploads_page(&resumed, 100, None, 1)
                .await
                .is_err()
        );
        assert!(
            c.reclaim_uploads_page(&resumed, 101, None, 0)
                .await
                .is_err()
        );
        assert!(
            c.reclaim_uploads_page(&resumed, 101, Some(b"foreign-cursor"), 1)
                .await
                .is_err()
        );
        let observer = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (first, second) = tokio::join!(
            c.seal_reclaim_upload_cancellation(&resumed, 101, &request),
            observer.seal_reclaim_upload_cancellation(&resumed, 101, &request)
        );
        assert_eq!(first.unwrap(), PublishOutcome::RequestCancelled);
        assert_eq!(second.unwrap(), PublishOutcome::RequestCancelled);
        assert_eq!(
            c.maintenance_status(request.namespace)
                .await
                .unwrap()
                .pending,
            after_cancel,
            "generic cancel已释放原permit，parent终态不能再次释放"
        );
        let tr = c.db.create_trx().unwrap();
        assert!(tr.get(&pin, false).await.unwrap().is_none());
        assert_eq!(
            tr.get(&original_p, false).await.unwrap().unwrap().as_ref(),
            original_terminal
        );
        assert!(matches!(
            c.release_reserved_reclaim(&resumed, 101)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::PoolBusy)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；模拟MAM样本与卷头证明，不访问设备"]
    async fn capacity_sample_requires_its_own_current_index_proof() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&c).await;
        let root = std::env::temp_dir().join(format!("mam-stamp-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let (spool, proof) = target_source(&root, 0);
        let import =
            start_target(&c, &serial, route, pool.id, 100, "MAMSTAMP0L8", ("/", true)).await;
        c.bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        c.finish_native_media(&import, 100, &proof).await.unwrap();
        let volume = proof.index().volume_uuid;
        let head = c
            .observed_active_volume_index(&serial, volume)
            .await
            .unwrap();
        let visibility = c.active_namespace_visibility(&serial, route).await.unwrap();
        let mut sample = TapeMonitorSample {
            drive_serial: "SIMDRV0000".into(),
            observed_at: crate::ltfs::label::ltfs_time_now(),
            write_protected: Some(false),
            data: Some(TapeCapacity {
                partition: 1,
                maximum: 1024 << 20,
                remaining: 512 << 20,
            }),
            index: Some(TapeCapacity {
                partition: 0,
                maximum: 128 << 20,
                remaining: 127 << 20,
            }),
            alerts: Some(vec![]),
            errors: vec![],
        };
        assert!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap()
                .is_none()
        );
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            Some(&sample),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap()
                .is_none(),
            "未核验索引的样本不能计入容量"
        );
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            None,
            None,
            Some((head, false)),
        )
        .await
        .unwrap();
        assert!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap()
                .is_none(),
            "后来确认索引不能把旧MAM样本冒充新采样"
        );
        sample.observed_at = crate::ltfs::label::ltfs_time_now();
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            Some(&sample),
            None,
            None,
        )
        .await
        .unwrap();
        let evidence = c
            .active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evidence["maximum"], 1024u64 << 20);
        assert_eq!(evidence["generation"], head.generation);
        assert_eq!(evidence["observed_at"], sample.observed_at);
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            None,
            None,
            Some((head, false)),
        )
        .await
        .unwrap();
        assert_eq!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap(),
            Some(evidence),
            "索引观测不刷新容量采样时间"
        );
        sample.errors.push("模拟MAM采样未完整成功".into());
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            Some(&sample),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap()
                .is_none()
        );
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "MAMSTAMP0L8",
            volume,
            None,
            None,
            Some((head, false)),
        )
        .await
        .unwrap();
        assert!(
            c.active_tape_capacity_evidence(&serial, route, "MAMSTAMP0L8")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            c.active_namespace_visibility(&serial, route).await.unwrap(),
            visibility,
            "容量采集不改变目录可见修订"
        );
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；分页有效UID统计与模拟MAM，不访问设备"]
    async fn tape_statistics_deduplicate_ee_data_and_resume_without_exposing_partial_counts() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let c = ManagedCatalog::open(Some(&cluster)).unwrap();
        let (serial, route, pool) = fixture(&c).await;
        let root = std::env::temp_dir().join(format!("tape-stats-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut index = LtfsIndex::empty(Uuid::new_v4(), "stats-source".into(), 'b');
        index.self_location.start_block = 200;
        let meta = index.root.meta.clone();
        let make = |name: &str, uid: u64, length: u64, block: u64, target: Option<&str>| {
            let mut f = FileNode {
                name: name.into(),
                meta: meta.clone(),
                length,
                symlink: target.map(str::to_owned),
                ..Default::default()
            };
            f.meta.file_uid = uid;
            if length > 0 {
                f.extents.push(crate::ltfs::index::Extent {
                    partition: 'b',
                    start_block: block,
                    byte_offset: 0,
                    byte_count: length,
                    file_offset: 0,
                });
            }
            f
        };
        let mut data = DirectoryNode {
            name: ".LTFSEE_DATA".into(),
            meta: meta.clone(),
            ..Default::default()
        };
        data.meta.file_uid = 2;
        data.files.push(make("body", 3, 2 << 20, 10, None));
        let mut docs = DirectoryNode {
            name: "docs".into(),
            meta: meta.clone(),
            ..Default::default()
        };
        docs.meta.file_uid = 8;
        docs.files
            .push(make("alias", 6, 0, 0, Some("../.LTFSEE_DATA/body")));
        docs.files.push(make("zero", 9, 0, 0, None));
        let mut empty = DirectoryNode {
            name: "empty".into(),
            meta: meta.clone(),
            ..Default::default()
        };
        empty.meta.file_uid = 10;
        index.root.subdirs = vec![data, docs, empty];
        index.root.files = vec![
            make("a-alias", 5, 0, 0, Some(".LTFSEE_DATA/body")),
            make("native-link", 7, 0, 0, Some("missing")),
            make("z-data", 4, 1 << 20, 50, None),
        ];
        for at in 0..20 {
            index
                .root
                .files
                .push(make(&format!("m{at:02}.empty"), 20 + at, 0, 0, None));
        }
        index.highest_file_uid = 39;
        let xml = index.to_xml().unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: Sha256::digest(&xml).into(),
        };
        let path = root.join("index.sqlite");
        ImportSpool::build(&xml[..], &path, expected).unwrap();
        let spool = ImportSpool::open(&path, expected).unwrap();
        let mut files = 0;
        index.walk_files(|_, _| files += 1);
        let proof = NativeMediaSource {
            identity: spool.identity().clone(),
            files,
            paths: spool.candidate_stats().entries,
        };
        let import = start_target(&c, &serial, route, pool.id, 100, "STATSL8", ("/", true)).await;
        c.bind_native_media_source(&import, 100, &proof)
            .await
            .unwrap();
        c.begin_native_media_projection(&import, 100).await.unwrap();
        for entry in target_entries(&import, &spool) {
            c.project_native_media_entry(&import, 100, &entry, true)
                .await
                .unwrap();
        }
        c.finish_native_media(&import, 100, &proof).await.unwrap();
        let head = c
            .observed_active_volume_index(&serial, index.volume_uuid)
            .await
            .unwrap();
        let mut sample = TapeMonitorSample {
            drive_serial: "SIMDRV0000".into(),
            observed_at: crate::ltfs::label::ltfs_time_now(),
            write_protected: Some(false),
            data: Some(TapeCapacity {
                partition: 1,
                maximum: 1024 << 20,
                remaining: 512 << 20,
            }),
            index: Some(TapeCapacity {
                partition: 0,
                maximum: 128 << 20,
                remaining: 127 << 20,
            }),
            alerts: Some(vec![]),
            errors: vec![],
        };
        c.record_tape_monitor(
            &serial,
            route,
            100,
            "STATSL8",
            index.volume_uuid,
            Some(&sample),
            None,
            Some((head, false)),
        )
        .await
        .unwrap();
        assert_eq!(
            c.active_tape_statistics(&serial, route, "STATSL8")
                .await
                .unwrap()["quality"],
            "calculating"
        );
        c.advance_active_pool_statistics(&serial, route, 100)
            .await
            .unwrap();
        let partial = c
            .active_tape_statistics(&serial, route, "STATSL8")
            .await
            .unwrap();
        assert_eq!(partial["quality"], "calculating");
        assert!(partial["live_bytes"].is_null());
        c.claim_active_owner_epoch(&serial, 101).await.unwrap();
        assert!(matches!(
            c.advance_active_pool_statistics(&serial, route, 100)
                .await
                .as_ref()
                .map_err(|error| error.domain()),
            Err(CatalogError::OwnerFenced)
        ));
        // A publication during the scan invalidates every partial UID counter.
        let tr = c.db.create_trx().unwrap();
        namespace_visibility::publish(&tr, route.route.namespace);
        tr.commit().await.unwrap();
        let mut result = Value::Null;
        for _ in 0..200 {
            c.advance_active_pool_statistics(&serial, route, 101)
                .await
                .unwrap();
            result = c
                .active_tape_statistics(&serial, route, "STATSL8")
                .await
                .unwrap();
            if result["quality"] == "estimated" {
                break;
            }
        }
        assert_eq!(result["quality"], "estimated", "{result}");
        assert_eq!(
            result["live_bytes"],
            3u64 << 20,
            "EE别名不能重复累计正文大小"
        );
        assert_eq!(
            result["live_data_objects"], 23,
            "原生链接无正文，空文件仍保留独立数据UID"
        );
        assert_eq!(result["reclaimable_bytes"], 509u64 << 20);
        assert_eq!(result["reclaimable"], 49.71);
        // A new publication hides the completed cache until a new full scan.
        let tr = c.db.create_trx().unwrap();
        namespace_visibility::publish(&tr, route.route.namespace);
        tr.commit().await.unwrap();
        let stale = c
            .active_tape_statistics(&serial, route, "STATSL8")
            .await
            .unwrap();
        assert_eq!(stale["quality"], "calculating");
        assert!(stale["live_bytes"].is_null());
        for _ in 0..200 {
            c.advance_active_pool_statistics(&serial, route, 101)
                .await
                .unwrap();
            if c.active_tape_statistics(&serial, route, "STATSL8")
                .await
                .unwrap()["quality"]
                == "estimated"
            {
                break;
            }
        }
        sample.data.as_mut().unwrap().remaining = (1024 << 20) - (2 << 20);
        sample.observed_at = crate::ltfs::label::ltfs_time_now();
        c.record_tape_monitor(
            &serial,
            route,
            101,
            "STATSL8",
            index.volume_uuid,
            Some(&sample),
            None,
            None,
        )
        .await
        .unwrap();
        let compressed = c
            .active_tape_statistics(&serial, route, "STATSL8")
            .await
            .unwrap();
        assert_eq!(compressed["quality"], "unknown");
        assert!(compressed["reclaimable"].is_null());
        assert_eq!(compressed["live_bytes"], 3u64 << 20);
        drop(spool);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::ltfs::volume::CommittedIndexConstruct;

    #[test]
    fn source_state_requires_kind_and_every_predecessor_with_no_prototype_defaults() {
        let volume = Uuid::new_v4();
        let construct = |generation, incremental| CommittedIndexConstruct {
            volume_uuid: volume,
            generation,
            location: crate::ltfs::index::IndexLocation {
                partition: 'b',
                start_block: generation * 10,
            },
            sha256: [generation as u8; 32],
            incremental,
        };
        let head = construct(2, true);
        let proof = NativeMediaSource {
            identity: IndexSourceIdentity::new(head, vec![construct(1, false)]).unwrap(),
            files: 2,
            paths: 3,
        };
        assert_eq!(source(&source_value(&proof)).unwrap(), Some(proof.clone()));
        let mut previous = construct(1, false);
        previous.sha256[0] ^= 1;
        let changed = NativeMediaSource {
            identity: IndexSourceIdentity::new(head, vec![previous]).unwrap(),
            ..proof.clone()
        };
        assert_eq!(proof.identity.head(), changed.identity.head());
        assert_ne!(proof, changed);
        assert_ne!(source_value(&proof), source_value(&changed));
        let mut invalid = source_value(&proof);
        invalid.as_object_mut().unwrap().remove("identity");
        assert!(source(&invalid).is_err());
        let hidden_data = NativeMediaSource {
            files: 4,
            ..proof.clone()
        };
        assert_eq!(
            source(&source_value(&hidden_data)).unwrap(),
            Some(hidden_data)
        );
        let mut invalid = source_value(&proof);
        invalid["files"] = json!("4");
        assert!(source(&invalid).is_err());
        let mut invalid = source_value(&proof);
        invalid["unexpected"] = json!(0);
        assert!(source(&invalid).is_err());
        assert!(source(&json!({"volume":volume.to_string(),"generation":2,"partition":"b","block":20,"sha256":[0],"files":2,"paths":3})).is_err());
    }
}

#[cfg(test)]
#[path = "native_media_refresh_tests.rs"]
mod refresh_tests;

#[cfg(test)]
#[path = "native_media_reclaim_source_tests.rs"]
mod reclaim_source_tests;

#[cfg(test)]
#[path = "native_media_reclaim_cancel_tests.rs"]
mod reclaim_cancel_tests;
