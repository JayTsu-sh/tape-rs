//! Bounded per-pool accounting. A complete namespace revision is published as
//! one state marker; partial UID counters are never inventory results.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn key(ns: Uuid, kind: u8, pool: Option<Uuid>) -> Vec<u8> {
    let mut out = management::prefix(ns, b'g');
    out.push(kind);
    if let Some(pool) = pool {
        out.extend_from_slice(pool.as_bytes());
    }
    out
}
fn scan_prefix(ns: Uuid, kind: u8, pool: Uuid, scan: Uuid) -> Vec<u8> {
    let mut out = key(ns, kind, Some(pool));
    out.extend_from_slice(scan.as_bytes());
    out
}
fn volume_counter(ns: Uuid, pool: Uuid, scan: Uuid, volume: Uuid) -> Vec<u8> {
    let mut out = scan_prefix(ns, b'c', pool, scan);
    out.extend_from_slice(volume.as_bytes());
    out
}
fn id(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == s)
        .ok_or(CatalogError::Corrupt("磁带统计UUID"))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    directory: String,
    name: String,
    after: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    pool: String,
    scan: String,
    revision: [u8; 10],
    stack: Vec<Frame>,
    complete: bool,
    started: u64,
    finished: Option<u64>,
}
fn path(stack: &[Frame]) -> String {
    if stack.len() <= 1 {
        "/".into()
    } else {
        format!(
            "/{}",
            stack[1..]
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    }
}
fn encode(state: &State) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(state).map_err(|_| CatalogError::Corrupt("磁带统计编码"))?;
    if raw.len() > 90_000 {
        return Err(CatalogError::Invalid("磁带统计检查点过大"));
    }
    Ok(raw)
}
async fn state_at(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<Option<State>> {
    let Some(raw) = tr.get(&key(ns, b's', Some(pool)), false).await? else {
        return Ok(None);
    };
    let state: State =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("磁带统计检查点JSON"))?;
    if encode(&state)?.as_slice() != raw.as_ref()
        || state.version != 1
        || state.pool != pool.to_string()
        || state.complete != state.stack.is_empty()
        || state.complete != state.finished.is_some()
        || state.started == 0
        || state.finished.is_some_and(|t| t < state.started)
        || state.stack.len() >= usize::from(MAX_DEPTH)
    {
        return Err(CatalogError::Corrupt("磁带统计检查点身份或阶段"));
    }
    id(&state.scan)?;
    for (at, frame) in state.stack.iter().enumerate() {
        id(&frame.directory)?;
        if at == 0 {
            if !frame.name.is_empty()
                || frame.directory != active_pools::root_id(ns, pool).to_string()
            {
                return Err(CatalogError::Corrupt("磁带统计根目录"));
            }
        } else {
            validate_name(frame.name.as_bytes())?;
        }
        if let Some(after) = &frame.after {
            validate_name(after.as_bytes())?;
        }
    }
    pool_catalog_path(pool, path(&state.stack).as_bytes())?;
    Ok(Some(state))
}
fn counter(raw: Option<&[u8]>) -> Result<(u64, u64)> {
    match raw {
        None => Ok((0, 0)),
        Some(raw) if raw.len() == 16 => Ok((read_u64(&raw[..8]), read_u64(&raw[8..]))),
        _ => Err(CatalogError::Corrupt("磁带统计计数长度")),
    }
}

// All fallible reads and arithmetic precede the first mutation. A returned
// domain error therefore cannot commit partial counters under retry_result.
async fn advance(tr: &Transaction, ns: Uuid, pool: Uuid, new_scan: Uuid) -> Result<Value> {
    native_media::require_pool_readable(tr, ns, pool).await?;
    let revision = namespace_visibility::at(tr, ns).await?;
    let previous = state_at(tr, ns, pool).await?;
    let mut cleanup = None;
    let mut state = match previous {
        Some(state) if state.revision == revision => state,
        old => {
            cleanup = old.map(|state| id(&state.scan)).transpose()?;
            State {
                version: 1,
                pool: pool.to_string(),
                scan: new_scan.to_string(),
                revision,
                stack: vec![Frame {
                    directory: active_pools::root_id(ns, pool).to_string(),
                    name: String::new(),
                    after: None,
                }],
                complete: false,
                started: management::now(),
                finished: None,
            }
        }
    };
    if state.complete {
        return Ok(json!({"pool":pool.to_string(),"phase":"complete"}));
    }
    let scan = id(&state.scan)?;
    let frame = state.stack.last().expect("unfinished stack");
    let parent = id(&frame.directory)?;
    let after = frame
        .after
        .as_ref()
        .map(|name| child_key(ns, parent, name.as_bytes()));
    let page = lookup::list_active_children_at(tr, ns, parent, after.as_deref(), 16).await?;
    let mut uids: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let mut counts: BTreeMap<Uuid, (u64, u64)> = BTreeMap::new();
    if page.items.is_empty() {
        if let Some(next) = page.next {
            let prefix = child_prefix(ns, parent);
            let name = String::from_utf8(decode_name(&next[prefix.len()..])?)
                .map_err(|_| CatalogError::Corrupt("磁带统计游标UTF-8"))?;
            state.stack.last_mut().unwrap().after = Some(name);
        } else {
            state.stack.pop();
        }
    } else {
        let base = path(&state.stack);
        for child in page.items {
            let name = std::str::from_utf8(&child.name)
                .map_err(|_| CatalogError::Corrupt("磁带统计文件名UTF-8"))?;
            let relative = if base == "/" {
                format!("/{name}")
            } else {
                format!("{base}/{name}")
            };
            let full = pool_catalog_path(pool, relative.as_bytes())?;
            let resolved =
                lookup::resolve_active_path_at_mode(tr, ns, lookup::path_components(&full)?, false)
                    .await?
                    .ok_or(CatalogError::Corrupt("磁带统计分页目标缺失"))?;
            state.stack.last_mut().unwrap().after = Some(name.into());
            match resolved {
                ResolvedPath::Directory { id: node, .. } if node == child.id => {
                    if state.stack.len() + 1 >= usize::from(MAX_DEPTH) {
                        return Err(CatalogError::Invalid("磁带统计目录深度超限"));
                    }
                    state.stack.push(Frame {
                        directory: node.to_string(),
                        name: name.into(),
                        after: None,
                    });
                    break;
                }
                ResolvedPath::File {
                    id: node,
                    copy,
                    placement,
                    ..
                } if node == child.id => {
                    if placement.pool_id != pool {
                        return Err(CatalogError::Corrupt("磁带统计发现跨池副本"));
                    }
                    if copy.media.as_ref().is_some_and(|m| m.native_symlink) {
                        continue;
                    }
                    let mut uid = scan_prefix(ns, b'u', pool, scan);
                    uid.extend_from_slice(copy.volume_id.as_bytes());
                    uid.extend_from_slice(&copy.ltfs_file_uid.to_be_bytes());
                    if let Some(size) = uids.get(&uid) {
                        if *size != copy.size {
                            return Err(CatalogError::Corrupt("同一数据UID的长度不一致"));
                        }
                        continue;
                    }
                    if let Some(raw) = tr.get(&uid, false).await? {
                        if raw.len() != 8 || read_u64(&raw) != copy.size {
                            return Err(CatalogError::Corrupt("数据UID统计长度不一致"));
                        }
                        continue;
                    }
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        counts.entry(copy.volume_id)
                    {
                        let raw = tr
                            .get(&volume_counter(ns, pool, scan, copy.volume_id), false)
                            .await?;
                        entry.insert(counter(raw.as_deref())?);
                    }
                    let total = counts.get_mut(&copy.volume_id).expect("loaded count");
                    total.0 = total
                        .0
                        .checked_add(1)
                        .ok_or(CatalogError::Invalid("磁带有效对象数溢出"))?;
                    total.1 = total
                        .1
                        .checked_add(copy.size)
                        .ok_or(CatalogError::Invalid("磁带有效字节数溢出"))?;
                    uids.insert(uid, copy.size);
                }
                _ => return Err(CatalogError::Corrupt("磁带统计分页inode不一致")),
            }
        }
    }
    state.complete = state.stack.is_empty();
    if state.complete {
        state.finished = Some(management::now().max(state.started));
    }
    let raw = encode(&state)?;
    if let Some(old) = cleanup {
        for kind in *b"uc" {
            let prefix = scan_prefix(ns, kind, pool, old);
            tr.clear_range(&prefix, &prefix_end(&prefix));
        }
    }
    for (uid, size) in uids {
        tr.set(&uid, &size.to_be_bytes());
    }
    for (volume, (objects, bytes)) in counts {
        let mut raw = objects.to_be_bytes().to_vec();
        raw.extend_from_slice(&bytes.to_be_bytes());
        tr.set(&volume_counter(ns, pool, scan, volume), &raw);
    }
    tr.set(&key(ns, b's', Some(pool)), &raw);
    if state.complete {
        let prefix = scan_prefix(ns, b'u', pool, scan);
        tr.clear_range(&prefix, &prefix_end(&prefix));
    }
    Ok(json!({"pool":pool.to_string(),"phase":if state.complete{"complete"}else{"scanning"}}))
}

impl ManagedCatalog {
    /// One fair, bounded, metadata-only maintenance step for this library.
    pub async fn advance_active_pool_statistics(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<Value> {
        let new_scan = Uuid::new_v4();
        self.db.run(|tr,_|async move {requests::retry_result(async {
            let ns=routing::require_runtime_active_owner(&tr,serial,route,owner).await?;
            let prefix=pool_prefix(ns);let scheduler=key(ns,b'a',None);
            let after=tr.get(&scheduler,false).await?;
            if after.as_ref().is_some_and(|raw|!raw.starts_with(&prefix) || raw.len()!=prefix.len()+16) {return Err(CatalogError::Corrupt("磁带统计调度游标"));}
            let page=read_page(&tr,&prefix,after.as_deref(),1,false).await?;
            let Some((selected,raw))=page.items.into_iter().next() else {tr.clear(&scheduler);return Ok(json!({"phase":"idle"}));};
            if selected.len()!=prefix.len()+16 {return Err(CatalogError::Corrupt("磁带统计池索引长度"));}
            let pool=uuid_from(&selected[prefix.len()..])?;let pool_record=decode_pool(pool,&raw)?;
            if pool_record.library_serial!=serial {return Err(CatalogError::Corrupt("磁带统计池带库作用域"));}
            let result=match advance(&tr,ns,pool,new_scan).await {
                Ok(value)=>{tr.clear(&key(ns,b'e',Some(pool)));value},
                Err(CatalogError::Client(error))=>return Err(CatalogError::Client(error)),
                Err(CatalogError::OwnerFenced)=>return Err(CatalogError::OwnerFenced),
                Err(error)=>{let value=json!({"pool":pool.to_string(),"phase":"unavailable","error":error.to_string()});tr.set(&key(ns,b'e',Some(pool)),value.to_string().as_bytes());value},
            };
            tr.set(&scheduler,&selected);Ok(result)
        }.await)}).await?
    }

    pub async fn active_tape_statistics(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        barcode: &str,
    ) -> Result<Value> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let mut result = json!({"quality":"unknown","reason":"unassigned","live_bytes":null,"live_data_objects":null,"reclaimable":null,"reclaimable_bytes":null,"capacity_resolution_bytes":1048576});
        let Some(assignment) = Self::verified_assignment(&tr, serial, barcode)
            .await?
            .filter(|a| a.namespace == ns)
        else {
            return Ok(result);
        };
        let Some(volume) = assignment.volume_id else {
            return Ok(result);
        };
        let pool = assignment.pool_id;
        match native_media::require_pool_readable(&tr, ns, pool).await {
            Ok(()) => {}
            Err(CatalogError::PoolImporting | CatalogError::PoolBusy) => {
                result["reason"] = json!("pool_unavailable");
                return Ok(result);
            }
            Err(error) => return Err(error),
        }
        if let Some(error) = tr.get(&key(ns, b'e', Some(pool)), false).await? {
            result["reason"] = json!("scan_unavailable");
            result["diagnostic"] = serde_json::from_slice::<Value>(&error)
                .map_err(|_| CatalogError::Corrupt("磁带统计错误记录"))?;
            return Ok(result);
        }
        let revision = namespace_visibility::at(&tr, ns).await?;
        let Some(state) = state_at(&tr, ns, pool).await? else {
            result["quality"] = json!("calculating");
            result["reason"] = json!("not_scanned");
            return Ok(result);
        };
        if state.revision != revision || !state.complete {
            result["quality"] = json!("calculating");
            result["reason"] = json!("namespace_changed_or_scanning");
            return Ok(result);
        }
        let raw = tr
            .get(&volume_counter(ns, pool, id(&state.scan)?, volume), false)
            .await?;
        let (objects, bytes) = counter(raw.as_deref())?;
        result["live_bytes"] = json!(bytes);
        result["live_data_objects"] = json!(objects);
        result["finished_at_millis"] = json!(state.finished);
        result["revision"] = json!(
            revision
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let Some(capacity) = tape_capacity::at(&tr, ns, serial, barcode, volume).await? else {
            result["reason"] = json!("capacity_missing_or_stale");
            return Ok(result);
        };
        let maximum = capacity["maximum"].as_u64().expect("validated capacity");
        let remaining = capacity["remaining"].as_u64().expect("validated capacity");
        result["capacity"] = capacity;
        let Some(garbage) = maximum
            .checked_sub(remaining)
            .and_then(|used| used.checked_sub(bytes))
        else {
            result["reason"] = json!("capacity_resolution_or_compression");
            return Ok(result);
        };
        if garbage < 1048576 && maximum != remaining {
            result["reason"] = json!("capacity_resolution");
            return Ok(result);
        }
        result["quality"] = json!("estimated");
        result["reason"] = Value::Null;
        result["reclaimable_bytes"] = json!(garbage);
        result["reclaimable"] =
            json!(((garbage as f64 / maximum as f64) * 10000.0).round() / 100.0);
        Ok(result)
    }
}
