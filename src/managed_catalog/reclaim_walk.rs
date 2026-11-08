//! Bounded namespace traversal under the pool write fence. Only names and
//! directory identities are stacked; file payloads and whole trees are not.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    directory: String,
    name: String,
    after: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimFile {
    path: String,
    file: String,
    version: u64,
    size: u64,
    operation: String,
}
impl ReclaimFile {
    pub(super) fn validate(&self) -> Result<()> {
        uuid(&self.file)?;
        uuid(&self.operation)?;
        lookup::path_components(self.path.as_bytes())?;
        if self.version == 0 || self.path == "/" || self.path.len() > MAX_POOL_PATH_BYTES {
            return Err(CatalogError::Corrupt("回收当前文件身份或路径无效"));
        }
        Ok(())
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn file_id(&self) -> Uuid {
        Uuid::parse_str(&self.file).expect("validated file")
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn operation(&self) -> Uuid {
        Uuid::parse_str(&self.operation).expect("validated operation")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimWalk {
    version: u8,
    reservation: String,
    stack: Vec<Frame>,
    current: Option<ReclaimFile>,
    complete: bool,
    copied_files: u64,
    copied_bytes: u64,
}
impl ReclaimWalk {
    pub fn current(&self) -> Option<&ReclaimFile> {
        self.current.as_ref()
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn copied_files(&self) -> u64 {
        self.copied_files
    }
    pub fn copied_bytes(&self) -> u64 {
        self.copied_bytes
    }
}

fn uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or(CatalogError::Corrupt("回收扫描 UUID"))
}
fn state_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = reservation.journal_key();
    // Separate row, sharing the immutable parent operation suffix.
    let kind = domain(reservation.namespace(), b'j').len() - 1;
    key[kind] = b's';
    key
}
pub(super) fn verification_key(reservation: &ReclaimReservation, file: &ReclaimFile) -> Vec<u8> {
    let mut key = state_key(reservation);
    let kind = domain(reservation.namespace(), b'j').len() - 1;
    key[kind] = b'v';
    key.extend_from_slice(file.operation().as_bytes());
    key
}

/// The first verification must read the original live terminal. Its exact
/// bytes are then retained in our ledger before releasing the epoch pin.
pub(super) async fn terminal_evidence_at(
    tr: &Transaction,
    request: &ActiveFileRequest,
    intent: &ActiveFileIntent,
    version: u64,
    previous: Option<&Value>,
) -> Result<Vec<u8>> {
    let sealed = previous
        .map(|proof| {
            serde_json::from_value::<Vec<u8>>(proof["terminal"].clone())
                .map_err(|_| CatalogError::Corrupt("回收实读证明缺少原终态"))
        })
        .transpose()?;
    let live = tr
        .get(
            &op_key(
                request.namespace,
                request.request_epoch,
                request.operation_id,
            ),
            false,
        )
        .await?;
    let terminal = match (live, sealed) {
        (Some(live), Some(sealed)) => {
            if live.as_ref() != sealed {
                return Err(CatalogError::Corrupt("原上传终态与封存证明不一致"));
            }
            sealed
        }
        (Some(live), None) => live.to_vec(),
        (None, Some(sealed)) => sealed,
        (None, None) => return Err(CatalogError::Incomplete),
    };
    if requests::reservation_terminal_result(
        &terminal,
        &reservation::cancellation_fingerprint(intent),
    )? != PublishOutcome::AlreadyApplied(version)
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(terminal)
}

pub(super) async fn clear_reserved_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    let state = state_at(tr, reservation).await?;
    let mut prefix = state_key(reservation);
    prefix[domain(reservation.namespace(), b'j').len() - 1] = b'v';
    if state.copied_files != 0
        || state.copied_bytes != 0
        || requests::prefix_has_rows(tr, &prefix).await?
    {
        return Err(CatalogError::PoolBusy);
    }
    children::require_no_children_at(tr, reservation).await?;
    tr.clear(&state_key(reservation));
    Ok(())
}
fn path(stack: &[Frame]) -> String {
    if stack.len() <= 1 {
        return "/".into();
    }
    format!(
        "/{}",
        stack[1..]
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join("/")
    )
}
fn encode(state: &ReclaimWalk) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(state).map_err(|_| CatalogError::Corrupt("回收扫描编码"))?;
    if raw.len() > 90_000 {
        return Err(CatalogError::Invalid("回收扫描检查点过大"));
    }
    Ok(raw)
}
async fn state_at(tr: &Transaction, reservation: &ReclaimReservation) -> Result<ReclaimWalk> {
    let Some(raw) = tr.get(&state_key(reservation), false).await? else {
        return Ok(ReclaimWalk {
            version: 1,
            reservation: reservation.fingerprint(),
            stack: vec![Frame {
                directory: active_pools::root_id(reservation.namespace(), reservation.spec().pool)
                    .to_string(),
                name: String::new(),
                after: None,
            }],
            current: None,
            complete: false,
            copied_files: 0,
            copied_bytes: 0,
        });
    };
    let state: ReclaimWalk =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("回收扫描结构"))?;
    if encode(&state)?.as_slice() != raw.as_ref()
        || state.version != 1
        || state.reservation != reservation.fingerprint()
        || state.stack.len() >= usize::from(MAX_DEPTH)
        || state.complete != state.stack.is_empty()
        || state.complete && state.current.is_some()
    {
        return Err(CatalogError::Corrupt("回收扫描身份或阶段"));
    }
    for (i, frame) in state.stack.iter().enumerate() {
        uuid(&frame.directory)?;
        if i == 0 {
            if !frame.name.is_empty()
                || frame.directory
                    != active_pools::root_id(reservation.namespace(), reservation.spec().pool)
                        .to_string()
            {
                return Err(CatalogError::Corrupt("回收扫描根目录"));
            }
        } else {
            validate_name(frame.name.as_bytes())?;
        }
        if let Some(after) = &frame.after {
            validate_name(after.as_bytes())?;
        }
    }
    pool_catalog_path(reservation.spec().pool, path(&state.stack).as_bytes())?;
    if let Some(file) = &state.current {
        file.validate()?;
        let full = pool_catalog_path(reservation.spec().pool, file.path.as_bytes())?;
        lookup::path_components(&full)?;
        if state.stack.is_empty() {
            return Err(CatalogError::Corrupt("回收当前文件"));
        }
    }
    Ok(state)
}

impl ManagedCatalog {
    /// Advance at most one merged directory row or one stack-pop per call.
    /// A selected source remains current until its verified child is recorded.
    pub async fn step_reclaim_walk(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<ReclaimWalk> {
        let operation = Uuid::new_v4();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let parent = record_at(&tr, reservation, owner).await?;
                        if parent["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        let mut state = state_at(&tr, reservation).await?;
                        if state.current.is_some() || state.complete {
                            return Ok(state);
                        }
                        let frame = state.stack.last().expect("nonempty unfinished stack");
                        let directory = uuid(&frame.directory)?;
                        let after = frame.after.as_ref().map(|name| {
                            child_key(reservation.namespace(), directory, name.as_bytes())
                        });
                        let page = lookup::list_active_children_at(
                            &tr,
                            reservation.namespace(),
                            directory,
                            after.as_deref(),
                            1,
                        )
                        .await?;
                        // A merged page can contain only tombstones. Its opaque cursor is
                        // still progress and must never be mistaken for end-of-directory.
                        if page.items.is_empty() {
                            if let Some(next) = page.next {
                                let prefix = child_prefix(reservation.namespace(), directory);
                                let name = decode_name(&next[prefix.len()..])?;
                                state.stack.last_mut().unwrap().after = Some(
                                    String::from_utf8(name)
                                        .map_err(|_| CatalogError::Corrupt("回收游标非 UTF-8"))?,
                                );
                            } else {
                                state.stack.pop();
                                state.complete = state.stack.is_empty();
                            }
                        } else {
                            let child = &page.items[0];
                            let name = std::str::from_utf8(&child.name)
                                .map_err(|_| CatalogError::Corrupt("回收文件名非 UTF-8"))?;
                            let current = path(&state.stack);
                            let relative = if current == "/" {
                                format!("/{name}")
                            } else {
                                format!("{current}/{name}")
                            };
                            let full =
                                pool_catalog_path(reservation.spec().pool, relative.as_bytes())?;
                            let resolved = lookup::resolve_active_path_at_mode(
                                &tr,
                                reservation.namespace(),
                                lookup::path_components(&full)?,
                                false,
                            )
                            .await?
                            .ok_or(CatalogError::Corrupt("回收分页目标不存在"))?;
                            state.stack.last_mut().unwrap().after = Some(name.into());
                            match resolved {
                                ResolvedPath::Directory { id, .. } if id == child.id => {
                                    if state.stack.len() + 1 >= usize::from(MAX_DEPTH) {
                                        return Err(CatalogError::Invalid("回收目录深度超限"));
                                    }
                                    state.stack.push(Frame {
                                        directory: id.to_string(),
                                        name: name.into(),
                                        after: None,
                                    });
                                }
                                ResolvedPath::File { id, head, copy, .. } if id == child.id => {
                                    if copy.volume_id == reservation.spec().volume {
                                        state.current = Some(ReclaimFile {
                                            path: relative,
                                            file: id.to_string(),
                                            version: head.version,
                                            size: copy.size,
                                            operation: operation.to_string(),
                                        });
                                    }
                                }
                                _ => return Err(CatalogError::Corrupt("回收分页 inode 不一致")),
                            }
                        }
                        let raw = encode(&state)?;
                        tr.set(&state_key(reservation), &raw);
                        Ok(state)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Record the exact destination returned by a checked physical read. This
    /// proof is separate from S4: it is never inferred from a successful PUT.
    pub async fn verify_reclaim_file(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        verified: &ResolvedRead,
        verified_head: ActiveIndexIdentity,
    ) -> Result<ReclaimWalk> {
        self.db.run(|tr,_| async move {requests::retry_result(async {
            let parent=record_at(&tr,reservation,owner).await?;
            if parent["owner_epoch"]!=owner {return Err(CatalogError::OwnerFenced);}
            children::require_request_identity_at(&tr,request,owner).await?;
            if file.operation()!=request.operation_id || file.file_id()!=request.file_id
                || Some(file.version)!=request.expected_version || file.path!=request.ltfs_path()?
                || file.size!=request.expected_size {
                return Err(CatalogError::Invalid("回收验证改变原扫描文件"));
            }
            let (intent,_)=children::child_at(&tr,request.namespace,reservation.spec().pool,request.request_epoch,request.operation_id,owner).await?.ok_or(CatalogError::Incomplete)?;
            let destination_version=file.version.checked_add(1).ok_or(CatalogError::Invalid("回收目标版本溢出"))?;
            let previous=tr.get(&verification_key(reservation,file),false).await?
                .map(|raw|decode(&raw)).transpose()?;
            let terminal=terminal_evidence_at(&tr,request,&intent,destination_version,previous.as_ref()).await?;
            let mut index_bytes=Vec::new(); active_volume::encode_index(&mut index_bytes,verified_head);
            let proof=json!({"version":1,"parent":reservation.fingerprint(),"request":request.fingerprint(),"file":file,"head":encode_head(&verified.head),"copy":copy_records::encode_physical_copy(&verified.copy)?,"index":index_bytes,"terminal":terminal});
            let mut state=state_at(&tr,reservation).await?;
            if let Some(old)=previous {
                if old!=proof {return Err(CatalogError::Invalid("回收验证重试改变原证据"));}
                return Ok(state);
            }
            let pin=children::history_pin_key(request);
            if tr.get(&pin,false).await?.as_deref()!=Some(&[][..]) {
                return Err(CatalogError::Corrupt("回收子请求的历史保留引用缺失或损坏"));
            }
            let full=pool_catalog_path(reservation.spec().pool,file.path.as_bytes())?;
            let Some(ResolvedPath::File {id,head,copy,placement,..})=lookup::resolve_active_path_at_mode(&tr,request.namespace,lookup::path_components(&full)?,false).await? else {
                return Err(CatalogError::Incomplete);
            };
            let (_,index)=active_volume::observed_at(&tr,reservation.serial(),intent.volume_id).await?;
            if id!=verified.id || head!=verified.head || *copy!=verified.copy || placement!=verified.placement
                || index!=verified_head || copy.volume_id!=intent.volume_id || copy.volume_generation!=intent.volume_generation
                || copy.volume_scan_id!=intent.volume_scan_id || copy.content_digest!=intent.expected_digest
                || copy.size!=file.size || head.version!=destination_version {
                return Err(CatalogError::Invalid("回收目标实读证据与当前副本不一致"));
            }
            if state.current.as_ref()!=Some(file) {return Err(CatalogError::Invalid("回收验证不属于当前检查点"));}
            state.copied_files=state.copied_files.checked_add(1).ok_or(CatalogError::Invalid("回收文件计数溢出"))?;
            state.copied_bytes=state.copied_bytes.checked_add(file.size).ok_or(CatalogError::Invalid("回收字节计数溢出"))?;
            state.current=None;
            let raw=encode(&state)?;
            tr.set(&verification_key(reservation,file),proof.to_string().as_bytes());
            tr.set(&state_key(reservation),&raw);
            tr.clear(&pin);
            Ok(state)
        }.await)}).await?
    }
}
