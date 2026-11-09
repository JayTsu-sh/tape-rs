//! A cancelled physical attempt remains immutable. Its unique successor owns a
//! new request and a fresh source read; it never reuses an old Q/I or terminal.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    version: u8,
    parent: String,
    input: [u8; 32],
    root: String,
    attempt: u8,
}
fn identity_prefix(parent: &ReclaimReservation) -> Vec<u8> {
    let mut key = parent.journal_key();
    key[domain(parent.namespace(), b'j').len() - 1] = b'r';
    key
}
fn identity_key(parent: &ReclaimReservation, operation: Uuid) -> Vec<u8> {
    let mut key = identity_prefix(parent);
    key.extend_from_slice(operation.as_bytes());
    key
}
fn input(file: &ReclaimFile) -> Result<[u8; 32]> {
    file.validate()?;
    Ok(Sha256::digest(
        serde_json::to_vec(file).map_err(|_| CatalogError::Corrupt("原复制谱系文件编码"))?,
    )
    .into())
}
fn root_identity(parent: &ReclaimReservation, file: &ReclaimFile) -> Result<Identity> {
    Ok(Identity {
        version: 1,
        parent: parent.fingerprint(),
        input: input(file)?,
        root: file.operation().to_string(),
        attempt: 0,
    })
}
fn encode_identity(identity: &Identity) -> Result<Vec<u8>> {
    Ok(serde_json::to_value(identity)
        .map_err(|_| CatalogError::Corrupt("原复制谱系身份编码"))?
        .to_string()
        .into_bytes())
}
pub(super) async fn prepare_root_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    file: &ReclaimFile,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let key = identity_key(parent, file.operation());
    if tr.get(&key, false).await?.is_some() {
        return Err(CatalogError::Corrupt("原扫描文件身份已被使用"));
    }
    Ok((key, encode_identity(&root_identity(parent, file)?)?))
}
pub(super) async fn identity_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    file: &ReclaimFile,
) -> Result<Identity> {
    let raw = tr
        .get(&identity_key(parent, file.operation()), false)
        .await?
        .ok_or(CatalogError::Corrupt("原复制谱系身份缺失"))?;
    let identity: Identity = serde_json::from_value(decode(&raw)?)
        .map_err(|_| CatalogError::Corrupt("原复制谱系身份结构"))?;
    let root = node_mutation::id(&identity.root)?;
    if identity.version != 1
        || identity.parent != parent.fingerprint()
        || identity.input != input(file)?
        || identity.attempt > 3
        || (identity.attempt == 0) != (root == file.operation())
    {
        return Err(CatalogError::Corrupt("原复制谱系身份、文件或预算不符"));
    }
    let root_file = file.successor(root);
    let expected = encode_identity(&root_identity(parent, &root_file)?)?;
    if tr.get(&identity_key(parent, root), false).await?.as_deref() != Some(expected.as_slice()) {
        return Err(CatalogError::Corrupt("原复制谱系失去独立根身份"));
    }
    Ok(identity)
}
pub(super) async fn clear_reserved_at(
    tr: &Transaction,
    parent: &ReclaimReservation,
    file: Option<&ReclaimFile>,
) -> Result<()> {
    let page = read_page(tr, &identity_prefix(parent), None, 2, false).await?;
    if page.next.is_some() || page.items.len() != usize::from(file.is_some()) {
        return Err(CatalogError::Corrupt("零子任务回收的谱系身份数量不符"));
    }
    if let Some(file) = file {
        let identity = identity_at(tr, parent, file).await?;
        if identity.attempt != 0 || page.items[0].0 != identity_key(parent, file.operation()) {
            return Err(CatalogError::Corrupt("零子任务回收仍有后继谱系"));
        }
        tr.clear(&page.items[0].0);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version: u8,
    parent: String,
    previous: ReclaimFile,
    next: ReclaimFile,
    epoch: u64,
    request: [u8; 32],
    cancellation: [u8; 32],
    attempt: u8,
}
fn key(parent: &ReclaimReservation, operation: Uuid, suffix: &[u8]) -> Vec<u8> {
    let mut key = parent.journal_key();
    key[domain(parent.namespace(), b'j').len() - 1] = b'z';
    key.extend_from_slice(operation.as_bytes());
    key.extend_from_slice(suffix);
    key
}
impl Retry {
    fn decode(raw: &[u8], parent: &ReclaimReservation) -> Result<Self> {
        let value: Self = serde_json::from_value(decode(raw)?)
            .map_err(|_| CatalogError::Corrupt("取消后继记录结构"))?;
        value.previous.validate()?;
        value.next.validate()?;
        if value.version != 1
            || value.parent != parent.fingerprint()
            || !(1..=3).contains(&value.attempt)
            || value.epoch == 0
            || value.previous.operation() == value.next.operation()
            || value.previous.successor(value.next.operation()) != value.next
        {
            return Err(CatalogError::Corrupt("取消后继改变原文件或超过尝试预算"));
        }
        Ok(value)
    }
    async fn check_at(
        &self,
        tr: &Transaction,
        parent: &ReclaimReservation,
        owner: u64,
    ) -> Result<()> {
        let previous = identity_at(tr, parent, &self.previous).await?;
        let next = identity_at(tr, parent, &self.next).await?;
        if next.root != previous.root
            || previous.attempt.checked_add(1) != Some(self.attempt)
            || next.attempt != self.attempt
        {
            return Err(CatalogError::Corrupt("取消后继与独立谱系身份不符"));
        }
        let (intent, _) = children::child_at(
            tr,
            parent.namespace(),
            parent.spec().pool(),
            self.epoch,
            self.previous.operation(),
            owner,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
        let request = intent.request();
        if ReclaimFile::from_request(&request)? != self.previous
            || self.request != request.fingerprint()
            || prepared_cancel::cancelled_at(tr, parent, owner, &request).await?
                != Some(self.cancellation)
        {
            return Err(CatalogError::Corrupt("取消后继与原独立取消证明不符"));
        }
        let next = tr
            .get(&key(parent, self.previous.operation(), b"/next"), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let previous = tr
            .get(&key(parent, self.next.operation(), b"/from"), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if next.as_ref() != previous.as_ref() || Self::decode(&next, parent)? != *self {
            return Err(CatalogError::Corrupt("取消后继双向身份缺失或冲突"));
        }
        Ok(())
    }
}

impl ManagedCatalog {
    /// Return None for a non-cancelled child. Replaying a cancelled predecessor
    /// returns the same successor, even after that successor advanced the walk.
    /// This allocates no upload permit and issues no device command.
    pub async fn retry_cancelled_reclaim_upload(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<Option<ReclaimFile>> {
        let operation = Uuid::new_v4();
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            if record_at(&tr, parent, owner).await?["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let Some(cancellation) =
                                prepared_cancel::cancelled_at(&tr, parent, owner, request).await?
                            else {
                                return Ok(None);
                            };
                            let file = ReclaimFile::from_request(request)?;
                            let next_key = key(parent, file.operation(), b"/next");
                            if let Some(raw) = tr.get(&next_key, false).await? {
                                let retry = Retry::decode(&raw, parent)?;
                                retry.check_at(&tr, parent, owner).await?;
                                if retry.previous != file
                                    || retry.request != request.fingerprint()
                                    || retry.cancellation != cancellation
                                {
                                    return Err(CatalogError::Corrupt("取消重放改变原后继输入"));
                                }
                                return Ok(Some(retry.next));
                            }
                            source::require_metadata_idle_at(&tr, parent).await?;
                            mechanics::require_parent_idle_at(&tr, parent).await?;
                            // Verify the complete bounded predecessor chain. A damaged or
                            // forged retry counter must not manufacture a fresh budget.
                            let role = identity_at(&tr, parent, &file).await?;
                            let mut current = file.clone();
                            let mut count = role.attempt;
                            loop {
                                let current_role = identity_at(&tr, parent, &current).await?;
                                if current_role.root != role.root || current_role.attempt != count {
                                    return Err(CatalogError::Corrupt("取消后继谱系链或预算无效"));
                                }
                                let previous = tr
                                    .get(&key(parent, current.operation(), b"/from"), false)
                                    .await?;
                                if count == 0 {
                                    if previous.is_some() {
                                        return Err(CatalogError::Corrupt(
                                            "原复制根不能有后继反向边",
                                        ));
                                    }
                                    break;
                                }
                                let previous = Retry::decode(
                                    &previous.ok_or(CatalogError::Corrupt("取消后继反向边缺失"))?,
                                    parent,
                                )?;
                                if previous.next != current || previous.attempt != count {
                                    return Err(CatalogError::Corrupt("取消后继链或预算无效"));
                                }
                                previous.check_at(&tr, parent, owner).await?;
                                current = previous.previous;
                                count -= 1;
                            }
                            let attempt = role.attempt + 1;
                            if attempt > 3 {
                                return Err(CatalogError::Invalid(
                                    "原复制已用完三次取消后重试预算",
                                ));
                            }
                            let next = file.successor(operation);
                            let state =
                                walk::replace_cancelled_at(&tr, parent, &file, &next).await?;
                            let reverse = key(parent, next.operation(), b"/from");
                            if tr.get(&reverse, false).await?.is_some()
                                || tr
                                    .get(
                                        &children::child_key(
                                            parent.namespace(),
                                            &parent.token(),
                                            next.operation(),
                                        )?,
                                        false,
                                    )
                                    .await?
                                    .is_some()
                            {
                                return Err(CatalogError::Invalid("取消后继身份已被占用"));
                            }
                            let role_key = identity_key(parent, next.operation());
                            if tr.get(&role_key, false).await?.is_some() {
                                return Err(CatalogError::Corrupt("取消后继谱系身份已被使用"));
                            }
                            let role_raw = encode_identity(&Identity {
                                version: 1,
                                parent: parent.fingerprint(),
                                input: input(&next)?,
                                root: role.root,
                                attempt,
                            })?;
                            let retry = Retry {
                                version: 1,
                                parent: parent.fingerprint(),
                                previous: file,
                                next: next.clone(),
                                epoch: request.request_epoch,
                                request: request.fingerprint(),
                                cancellation,
                                attempt,
                            };
                            let raw = serde_json::to_value(&retry)
                                .map_err(|_| CatalogError::Corrupt("取消后继编码"))?
                                .to_string();
                            // All reads and validation precede the atomic successor edge.
                            tr.set(&role_key, &role_raw);
                            tr.set(&next_key, raw.as_bytes());
                            tr.set(&reverse, raw.as_bytes());
                            tr.set(&walk::state_key(parent), &state);
                            Ok(Some(next))
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
