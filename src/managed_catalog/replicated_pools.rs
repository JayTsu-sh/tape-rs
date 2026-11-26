//! A multi-library creation has one FDB admission and one publication boundary.
use super::*;
use crate::replicated_pool::{
    CreateReplicatedPool, ReplicatedPoolMember, ResolveReplicatedPoolConflict,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[path = "replicated_archive.rs"]
pub(in crate::managed_catalog) mod archive;
#[path = "replicated_pool_files.rs"]
pub(super) mod files;

const MAX_MEMBERS: usize = 16;
const MAX_TAPES: usize = 100;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Creation {
    input: CreateReplicatedPool,
    published: bool,
}

fn domain(kind: u8) -> Vec<u8> {
    let mut out = key(0x84);
    out.extend_from_slice(b"replicated-pools/1/");
    out.push(kind);
    out
}

fn task_key(id: Uuid, kind: u8) -> Vec<u8> {
    let mut out = domain(kind);
    out.extend_from_slice(id.as_bytes());
    out
}

fn ownership_key(ns: Uuid, member: Uuid) -> Vec<u8> {
    let mut out = domain(b'm');
    out.extend_from_slice(ns.as_bytes());
    out.extend_from_slice(member.as_bytes());
    out
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| CatalogError::Cache(error.to_string()))
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T> {
    serde_json::from_slice(raw).map_err(|_| CatalogError::Corrupt("多副本池记录编码无效"))
}

fn pool_record(input: &CreateReplicatedPool, member: &ReplicatedPoolMember) -> PoolRecord {
    PoolRecord {
        id: member.pool_id,
        name: member.name.clone(),
        library_serial: member.library_serial.clone(),
        created_at_millis: input.created_at_millis,
        file_limit: 200_000,
        policy: PoolPolicy::default(),
        revision: 1,
    }
}

fn validate(input: &CreateReplicatedPool) -> Result<()> {
    if input.version != 1 || input.id.is_nil() || !(2..=MAX_MEMBERS).contains(&input.members.len())
    {
        return Err(CatalogError::Invalid(
            "多副本池须使用非零 UUID 和 2 至 16 个底层池",
        ));
    }
    validate_pool_name(&input.name)?;
    let mut ids = BTreeSet::from([input.id]);
    let mut names = BTreeSet::new();
    let mut tapes = BTreeSet::new();
    for member in &input.members {
        validate_pool(&pool_record(input, member))?;
        if member.namespace.is_nil()
            || member.logical_library.is_some_and(|id| id.is_nil())
            || !ids.insert(member.pool_id)
            || !names.insert((member.namespace, &member.name))
        {
            return Err(CatalogError::Invalid("底层池标识、名称重复或带库范围无效"));
        }
        if member.tapes.is_empty() {
            if member.confirm_format {
                return Err(CatalogError::Invalid("确认初始化须指定磁带"));
            }
        } else {
            logical_libraries::validate_pool_selection(&member.tapes)?;
        }
        for tape in &member.tapes {
            if !tapes.insert(tape) {
                return Err(CatalogError::Invalid("多副本池的底层池不可重复选择条码"));
            }
        }
    }
    if tapes.len() > MAX_TAPES {
        return Err(CatalogError::Invalid("一次多副本池创建最多选择 100 盘磁带"));
    }
    Ok(())
}

async fn read_at(tr: &Transaction, id: Uuid) -> Result<Option<Creation>> {
    let Some(raw) = tr.get(&task_key(id, b'i'), false).await? else {
        return Ok(None);
    };
    let creation: Creation = decode(&raw)?;
    if creation.input.id != id || validate(&creation.input).is_err() {
        return Err(CatalogError::Corrupt("多副本池记录与原请求身份不符"));
    }
    Ok(Some(creation))
}

pub(in crate::managed_catalog::control) async fn parent_at(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
) -> Result<Option<Uuid>> {
    tr.get(&ownership_key(ns, pool), false)
        .await?
        .map(|raw| uuid_from(&raw))
        .transpose()
}

pub(in crate::managed_catalog::control) async fn require_independent_at(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
) -> Result<()> {
    if parent_at(tr, ns, pool).await?.is_some() {
        return Err(CatalogError::Invalid(
            "多副本池底层池不能独立变更或访问文件",
        ));
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn require_reclaim_at(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    unassign: bool,
) -> Result<()> {
    if let Some(id) = parent_at(tr, ns, pool).await? {
        let parent = read_at(tr, id).await?.ok_or(CatalogError::Incomplete)?;
        if !parent.published || unassign {
            return Err(CatalogError::Invalid(
                "多副本池只允许已发布成员在本池内回收",
            ));
        }
    }
    Ok(())
}

fn require_configured(
    input: &CreateReplicatedPool,
    routes: &BTreeMap<String, ActiveLibraryRoute>,
) -> Result<()> {
    for member in &input.members {
        let route = routes
            .get(&member.library_serial)
            .ok_or(CatalogError::Invalid("底层池物理带库未配置"))?;
        if route.route.namespace != member.namespace {
            return Err(CatalogError::Invalid("底层池目录命名空间与物理带库不符"));
        }
    }
    Ok(())
}

async fn owner_at(tr: &Transaction, serial: &str) -> Result<u64> {
    let raw = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if raw.len() != 8 || read_u64(&raw) == 0 {
        return Err(CatalogError::Corrupt("多副本成员带库 owner 轮次无效"));
    }
    Ok(read_u64(&raw))
}

async fn require_member(
    tr: &Transaction,
    parent: Uuid,
    member: &ReplicatedPoolMember,
) -> Result<()> {
    if parent_at(tr, member.namespace, member.pool_id).await? != Some(parent) {
        return Err(CatalogError::Corrupt("多副本池缺少底层池所有权"));
    }
    Ok(())
}

pub(super) async fn has_conflict(tr: &Transaction, id: Uuid) -> Result<bool> {
    requests::prefix_has_rows(tr, &task_key(id, b'c')).await
}

pub(super) async fn try_publish_at(tr: &Transaction, id: Uuid) -> Result<bool> {
    let mut creation = read_at(tr, id)
        .await?
        .ok_or(CatalogError::Corrupt("多副本池原任务缺失"))?;
    if creation.published || has_conflict(tr, id).await? {
        return Ok(false);
    }
    for member in &creation.input.members {
        require_member(tr, id, member).await?;
        match routing::require_active_namespace(tr, &member.library_serial).await {
            Ok(ns) if ns == member.namespace => {}
            Ok(_) | Err(CatalogError::Incomplete) => return Ok(false),
            Err(error) => return Err(error),
        }
        if !selected_pool::replicated_ready_at(tr, member.namespace, member.pool_id, id).await? {
            return Ok(false);
        }
    }
    for member in &creation.input.members {
        selected_pool::release_replicated_at(tr, member.namespace, member.pool_id, id).await?;
    }
    creation.published = true;
    tr.set(&task_key(id, b'i'), &encode(&creation)?);
    Ok(true)
}

async fn view_at(tr: &Transaction, creation: &Creation, current_scope: bool) -> Result<Value> {
    let input = &creation.input;
    let mut members = Vec::with_capacity(input.members.len());
    let mut domains = BTreeMap::<&str, Vec<Uuid>>::new();
    for member in &input.members {
        require_member(tr, input.id, member).await?;
        let mut row =
            selected_pool::replicated_status_at(tr, member.namespace, member.pool_id, input.id)
                .await?;
        row["pool_id"] = json!(member.pool_id);
        row["original_logical_library"] = json!(member.logical_library);
        if current_scope {
            row["logical_library"] = json!(
                logical_libraries::pool_library_at(
                    tr,
                    member.namespace,
                    &member.library_serial,
                    member.pool_id,
                )
                .await?
            );
        }
        if !creation.published && !matches!(row["status"].as_str(), Some("succeeded" | "blocked")) {
            let owner = owner_at(tr, &member.library_serial).await?;
            row["current_owner_epoch"] = json!(owner);
            row["inventory_status"] = json!(
                logical_libraries::pool_creation_inventory_status(
                    tr,
                    member.namespace,
                    &member.library_serial,
                    owner,
                )
                .await?
            );
        }
        if !creation.published && row["status"] == "succeeded" {
            row["status"] = json!("prepared");
            row["pool"] = Value::Null;
        }
        domains
            .entry(&member.library_serial)
            .or_default()
            .push(member.pool_id);
        members.push(row);
    }
    let conflict = files::conflict_at(tr, input.id).await?;
    let status = if creation.published {
        "succeeded"
    } else if conflict.is_some() || members.iter().any(|row| row["status"] == "blocked") {
        "blocked"
    } else if members
        .iter()
        .any(|row| row["active_tape"]["progress"]["phase"] == "format_result_pending")
    {
        "result_unknown"
    } else if members.iter().any(|row| {
        row["inventory_status"]
            .as_str()
            .is_some_and(|state| state != "current")
    }) {
        "waiting_library"
    } else {
        "running"
    };
    Ok(json!({
        "version": 1, "id": input.id, "name": input.name, "created_at_millis": input.created_at_millis,
        "status": status, "members": members, "conflict": conflict,
        "failure_domains": domains.into_iter().map(|(serial, ids)| json!({"library_serial":serial,"pool_ids":ids})).collect::<Vec<_>>(),
    }))
}

impl ManagedCatalog {
    pub(crate) async fn require_independent_file_pool(&self, ns: Uuid, pool: Uuid) -> Result<()> {
        let tr = self.db.create_trx()?;
        require_independent_at(&tr, ns, pool).await
    }

    pub(crate) async fn admit_replicated_pool(
        &self,
        input: &CreateReplicatedPool,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Value> {
        validate(input)?;
        require_configured(input, routes)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        if let Some(saved) = read_at(&tr, input.id).await? {
                            if saved.input != *input {
                                return Err(CatalogError::IdempotencyMismatch);
                            }
                            return view_at(&tr, &saved, false).await;
                        }
                        let mut name = domain(b'n');
                        append_component(&mut name, input.name.as_bytes());
                        if tr.get(&name, false).await?.is_some() {
                            return Err(CatalogError::Invalid("多副本池名称已存在"));
                        }
                        for member in &input.members {
                            let route = routes[&member.library_serial];
                            let owner = owner_at(&tr, &member.library_serial).await?;
                            let ns = routing::require_runtime_active_owner(
                                &tr,
                                &member.library_serial,
                                route,
                                owner,
                            )
                            .await?;
                            require_independent_at(&tr, ns, member.pool_id).await?;
                            let status = selected_pool::admit_at(
                                &tr,
                                ns,
                                owner,
                                member.logical_library,
                                &pool_record(input, member),
                                selected_pool::TapeSelection {
                                    barcodes: &member.tapes,
                                    confirm_format: member.confirm_format,
                                },
                                Some(input.id),
                            )
                            .await?;
                            if status["status"] == "blocked" {
                                return Err(CatalogError::ReplicatedSelectionBlocked(status));
                            }
                            tr.set(&ownership_key(ns, member.pool_id), input.id.as_bytes());
                        }
                        let creation = Creation {
                            input: input.clone(),
                            published: false,
                        };
                        tr.set(&name, input.id.as_bytes());
                        tr.set(&task_key(input.id, b'i'), &encode(&creation)?);
                        view_at(&tr, &creation, false).await
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    pub(crate) async fn replicated_pool_status(
        &self,
        id: Uuid,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let Some(creation) = read_at(&tr, id).await? else {
            return Ok(None);
        };
        require_configured(&creation.input, routes)?;
        view_at(&tr, &creation, true).await.map(Some)
    }

    pub(crate) async fn replicated_pool_files(
        &self,
        id: Uuid,
        after: Option<&str>,
        limit: usize,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let Some(creation) = read_at(&tr, id).await? else {
            return Ok(None);
        };
        require_configured(&creation.input, routes)?;
        if !creation.published {
            return Err(CatalogError::PoolImporting);
        }
        files::page(&tr, id, after, limit).await.map(Some)
    }

    pub(crate) async fn resolve_replicated_pool_conflict(
        &self,
        id: Uuid,
        choice: &ResolveReplicatedPoolConflict,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<Option<Value>> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let Some(creation) = read_at(&tr, id).await? else {
                            return Ok(None);
                        };
                        require_configured(&creation.input, routes)?;
                        if choice.version != 1
                            || choice.volume_uuid.is_nil()
                            || choice.revision == 0
                        {
                            return Err(CatalogError::Invalid("冲突处理版本或来源卷 UUID 无效"));
                        }
                        files::resolve_at(&tr, id, choice).await?;
                        if try_publish_at(&tr, id).await? {
                            let published = Creation {
                                published: true,
                                ..creation
                            };
                            view_at(&tr, &published, true).await.map(Some)
                        } else {
                            view_at(&tr, &creation, true).await.map(Some)
                        }
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}

impl ManagedCatalog {
    pub(crate) async fn replica_bucket_exists(
        &self,
        pool: Uuid,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
    ) -> Result<bool> {
        let tr = self.db.create_trx()?;
        Ok(read_at(&tr, pool).await?.is_some_and(|creation| {
            creation.published && require_configured(&creation.input, routes).is_ok()
        }))
    }

    pub(crate) async fn replicated_buckets(
        &self,
        routes: &BTreeMap<String, ActiveLibraryRoute>,
        after: Option<&[u8]>,
    ) -> Result<Page<(Uuid, u64)>> {
        let tr = self.db.create_trx()?;
        let prefix = domain(b'i');
        let page = read_page(&tr, &prefix, after, 100, false).await?;
        let mut items = Vec::new();
        for (key, raw) in page.items {
            let id = uuid_from(&key[prefix.len()..])?;
            let creation: Creation = decode(&raw)?;
            if creation.input.id != id || validate(&creation.input).is_err() {
                return Err(CatalogError::Corrupt("多副本池记录与原请求身份不符"));
            }
            if creation.published && require_configured(&creation.input, routes).is_ok() {
                items.push((id, creation.input.created_at_millis));
            }
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }
}
