//! Private parent-directory plan for an ordinary S3 create. The directory
//! edges are published only by the file batch's visibility transaction.
use super::*;
use crate::ltfs::index::LtfsIndex;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog) struct Parents {
    version: u8,
    operation: Uuid,
    pool: Uuid,
    base: Uuid,
    base_version: u64,
    base_path: String,
    dirs: Vec<(String, Uuid)>,
    name: String,
    size: u64,
    sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedParents {
    version: u8,
    operation: Uuid,
    plan_digest: [u8; 32],
    batch: Uuid,
    receipt: [u8; 32],
    epoch: u64,
    directories: Vec<(u64, String)>,
}

fn plan_key(namespace: Uuid, operation: Uuid) -> Vec<u8> {
    let mut key = control::management::prefix(namespace, b'Q');
    key.push(b'Z');
    key.extend_from_slice(operation.as_bytes());
    key
}

fn evidence_fingerprint(receipt: &CommitReceipt) -> [u8; 32] {
    receipt_fingerprint(receipt)
        .try_into()
        .expect("SHA-256 length")
}

fn observed_key(volume: Uuid, generation: u64) -> Vec<u8> {
    let mut key = super::super::key(b'p');
    key.extend_from_slice(b"s3-parent/");
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    key
}

fn decode_record<T: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("S3 隐含目录记录损坏"))
}

fn encode_record<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| CatalogError::Invalid("S3 隐含目录无法编码"))?;
    if bytes.len() > 90_000 {
        return Err(CatalogError::Invalid("S3 隐含目录记录超过预算"));
    }
    Ok(bytes)
}

impl Parents {
    pub(in crate::managed_catalog) fn parent(&self) -> Result<Uuid> {
        self.dirs
            .last()
            .map(|(_, id)| *id)
            .ok_or(CatalogError::Corrupt("S3 隐含目录清单为空"))
    }

    pub(in crate::managed_catalog) fn parent_path(&self) -> String {
        let mut path = self.base_path.clone();
        for (name, _) in &self.dirs {
            path.push('/');
            path.push_str(name);
        }
        path
    }

    pub(in crate::managed_catalog) fn matches_spec(&self, spec: &ActiveUploadSpec) -> bool {
        spec.expected_version.is_none()
            && self.pool == spec.pool_id
            && self.operation == spec.operation_id
            && self.size == spec.size
            && spec.digest == ContentDigest::Sha256(self.sha256)
            && spec.path == format!("{}/{}", self.parent_path(), self.name).as_bytes()
    }

    fn digest(&self) -> Result<[u8; 32]> {
        Ok(Sha256::digest(encode_record(self)?).into())
    }

    fn matches_receipt(&self, receipt: &CommitReceipt) -> Result<bool> {
        Ok(receipt.operation_id == self.operation
            && receipt.pool_id == Some(self.pool)
            && receipt.parent == self.parent()?
            && receipt.name == self.name.as_bytes()
            && receipt.expected_version.is_none()
            && receipt.size == self.size
            && receipt.content_digest == ContentDigest::Sha256(self.sha256))
    }

    pub(in crate::managed_catalog) fn claim(&self, ns: Uuid) -> Result<Vec<u8>> {
        let (name, _) = self
            .dirs
            .first()
            .ok_or(CatalogError::Corrupt("S3 隐含目录清单为空"))?;
        Ok(reservation::path_key_for(ns, self.base, name.as_bytes()))
    }

    /// Validate the original visible ancestor and every absent destination.
    pub(in crate::managed_catalog) async fn baseline(
        &self,
        tr: &Transaction,
        ns: Uuid,
    ) -> Result<bool> {
        Ok(self.validated_base_version(tr, ns).await? == Some(self.base_version))
    }

    /// Find the current version while the original ancestor and missing path agree.
    pub(in crate::managed_catalog) async fn validated_base_version(
        &self,
        tr: &Transaction,
        ns: Uuid,
    ) -> Result<Option<u64>> {
        let path = pool_catalog_path(
            self.pool,
            if self.base_path.is_empty() {
                b"/"
            } else {
                self.base_path.as_bytes()
            },
        )?;
        let existing =
            lookup::resolve_path_at_mode(tr, ns, lookup::path_components(&path)?, false, false)
                .await?;
        let Some(ResolvedPath::Directory { id, version, .. }) = existing else {
            return Ok(None);
        };
        if id != self.base || version < self.base_version || version == u64::MAX {
            return Ok(None);
        }
        let mut parent = self.base;
        for (name, id) in &self.dirs {
            if tr
                .get(&child_key(ns, parent, name.as_bytes()), false)
                .await?
                .is_some()
                || tr.get(&node_key(ns, *id), false).await?.is_some()
                || lookup::published_child_id_at(tr, ns, parent, name.as_bytes())
                    .await?
                    .is_some()
                || tr
                    .get(
                        &active_batch::path_claim_key_for(ns, parent, name.as_bytes()),
                        false,
                    )
                    .await?
                    .is_some()
            {
                return Ok(None);
            }
            parent = *id;
        }
        Ok(Some(version))
    }

    /// Pin the validated revision in the same transaction as the first reservation.
    pub(in crate::managed_catalog) fn pin_base_version(
        mut self,
        tr: &Transaction,
        ns: Uuid,
        version: u64,
    ) -> Result<()> {
        if self.base_version != version {
            self.base_version = version;
            tr.set(&plan_key(ns, self.operation), &encode_record(&self)?);
        }
        Ok(())
    }

    fn directory_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.dirs
            .iter()
            .scan(self.base_path.clone(), |path, (name, _)| {
                path.push('/');
                path.push_str(name);
                Some(path.clone())
            })
    }
}

pub(in crate::managed_catalog) async fn get_at(
    tr: &Transaction,
    ns: Uuid,
    operation: Uuid,
) -> Result<Option<Parents>> {
    let Some(bytes) = tr.get(&plan_key(ns, operation), false).await? else {
        return Ok(None);
    };
    let plan: Parents = decode_record(&bytes)?;
    if plan.version != 1
        || plan.operation != operation
        || plan.dirs.is_empty()
        || plan.base_version == 0
        || plan.base_version == u64::MAX
        || plan.base.is_nil()
        || encode_record(&plan)?.as_slice() != bytes.as_ref()
    {
        return Err(CatalogError::Corrupt("S3 隐含目录与原操作不一致"));
    }
    Ok(Some(plan))
}

pub(in crate::managed_catalog) async fn for_receipt(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<Option<Parents>> {
    let Some(plan) = get_at(tr, receipt.namespace, receipt.operation_id).await? else {
        return Ok(None);
    };
    if !plan.matches_receipt(receipt)? {
        return Err(CatalogError::Corrupt("S3 隐含目录与文件收据不一致"));
    }
    Ok(Some(plan))
}

async fn ancestor_at(
    tr: &Transaction,
    ns: Uuid,
    pool: Uuid,
    path: &str,
    pins: &std::collections::BTreeMap<String, Uuid>,
) -> Result<(Uuid, u64, usize)> {
    let catalog_path = pool_catalog_path(
        pool,
        if path.is_empty() {
            b"/"
        } else {
            path.as_bytes()
        },
    )?;
    let components = lookup::path_components(&catalog_path)?;
    let Some(ResolvedPath::Directory {
        mut id,
        mut version,
        ..
    }) = lookup::resolve_path_at_mode(tr, ns, vec![components[0]], false, false).await?
    else {
        return Err(CatalogError::Invalid("S3 池根目录不存在"));
    };
    let check = |len, id| {
        if pins
            .get(&path[..len])
            .is_none_or(|expected| *expected == id)
        {
            Ok(())
        } else {
            Err(CatalogError::Invalid("S3 原祖先目录身份已改变"))
        }
    };
    let mut len = 0;
    check(len, id)?;
    for component in &components[1..] {
        let Some(child) = tr.get(&child_key(ns, id, component), false).await? else {
            if pins.keys().any(|pin| pin.len() > len) {
                return Err(CatalogError::Invalid("S3 原祖先目录已消失"));
            }
            if requests::prefix_has_rows(tr, &unavailable_volume_prefix(ns)).await? {
                return Err(CatalogError::Incomplete);
            }
            break;
        };
        let child = uuid_from(&child)?;
        let node = tr
            .get(&node_key(ns, child), false)
            .await?
            .ok_or(CatalogError::Corrupt("目录项指向缺失 inode"))?;
        let node = decode_node(&node)?;
        if child.is_nil() || node.parent != id || node.name != *component {
            return Err(CatalogError::Corrupt("目录项与 inode 不一致"));
        }
        if node.kind != NodeKind::Directory {
            return Err(CatalogError::Invalid("S3 父路径不是目录"));
        }
        id = child;
        version = node
            .directory_version
            .ok_or(CatalogError::Corrupt("目录属性版本缺失"))?;
        len += component.len() + 1;
        check(len, id)?;
    }
    Ok((id, version, len))
}

impl ManagedCatalog {
    pub(crate) async fn s3_upload_parent_anchor(
        &self,
        route: ActiveLibraryRoute,
        serial: &str,
        round: u64,
        pool: Uuid,
        parent: &str,
    ) -> Result<(String, Uuid)> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, round).await?;
        let (id, _, len) = ancestor_at(&tr, ns, pool, parent, &Default::default()).await?;
        Ok((parent[..len].to_owned(), id))
    }

    /// Record a private plan before the existing file-prepare RPC. No public
    /// directory or device side effect is produced here.
    pub(crate) async fn plan_s3_upload_parents(
        &self,
        route: ActiveLibraryRoute,
        serial: &str,
        round: u64,
        spec: &ActiveUploadSpec,
        pins: &std::collections::BTreeMap<String, Uuid>,
    ) -> Result<(Uuid, Vec<(String, Uuid)>)> {
        pool_catalog_path(spec.pool_id, &spec.path)?;
        let path = std::str::from_utf8(&spec.path)
            .map_err(|_| CatalogError::Invalid("S3 路径不是 UTF-8"))?;
        let (parent, name) = path
            .rsplit_once('/')
            .ok_or(CatalogError::Invalid("S3 对象缺少名称"))?;
        let ContentDigest::Sha256(sha256) = spec.digest else {
            return Err(CatalogError::Invalid("S3 PUT 缺少 SHA-256"));
        };
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, round)
                            .await?;
                        control::require_pool_writable_at(&tr, ns, spec.pool_id).await?;
                        let (base, base_version, len) =
                            ancestor_at(&tr, ns, spec.pool_id, parent, pins).await?;
                        let key = plan_key(ns, spec.operation_id);
                        if let Some(saved) = get_at(&tr, ns, spec.operation_id).await? {
                            if !saved.matches_spec(spec) {
                                return Err(CatalogError::IdempotencyMismatch);
                            }
                            return Ok((
                                saved.parent()?,
                                saved
                                    .directory_paths()
                                    .zip(saved.dirs.iter().map(|(_, id)| *id))
                                    .collect(),
                            ));
                        }
                        if len == parent.len() {
                            return Ok((base, Vec::new()));
                        }
                        if spec.expected_version.is_some() || base_version == u64::MAX {
                            return Err(CatalogError::Invalid("S3 无法规划缺失的父目录"));
                        }
                        let dirs = parent[len + 1..]
                            .split('/')
                            .map(|part| (part.to_owned(), Uuid::new_v4()))
                            .collect();
                        let plan = Parents {
                            version: 1,
                            operation: spec.operation_id,
                            pool: spec.pool_id,
                            base,
                            base_version,
                            base_path: parent[..len].to_owned(),
                            dirs,
                            name: name.to_owned(),
                            size: spec.size,
                            sha256,
                        };
                        if tr.get(&plan.claim(ns)?, false).await?.is_some() {
                            return Err(CatalogError::Incomplete);
                        }
                        tr.set(&key, &encode_record(&plan)?);
                        Ok((
                            plan.parent()?,
                            plan.directory_paths()
                                .zip(plan.dirs.iter().map(|(_, id)| *id))
                                .collect(),
                        ))
                    }
                    .await,
                )
            })
            .await?
    }

    /// The S3 route calls this only before handing the original operation
    /// to the write actor. Read-only prepare cannot admit a media attempt.
    pub(crate) async fn discard_unsubmitted_s3_parents(
        &self,
        route: ActiveLibraryRoute,
        serial: &str,
        round: u64,
        operation: Uuid,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, round)
                            .await?;
                        if let Some(plan) = get_at(&tr, ns, operation).await?
                            && !tr.get(&plan.claim(ns)?, false).await?.is_some_and(|value| {
                                value.len() == 24 && value.ends_with(operation.as_bytes())
                            })
                        {
                            tr.clear(&plan_key(ns, operation));
                        }
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }

    pub(crate) async fn has_s3_upload_parents(&self, request: &ActiveFileRequest) -> Result<bool> {
        let tr = self.db.create_trx()?;
        Ok(get_at(&tr, request.namespace, request.operation_id)
            .await?
            .is_some())
    }

    /// The write actor rejected this request. Discard its private metadata
    /// only after proving that this owner never admitted the original attempt.
    pub(crate) async fn discard_rejected_s3_parents(
        &self,
        route: ActiveLibraryRoute,
        round: u64,
        intent: &ActiveFileIntent,
    ) -> Result<bool> {
        let request = intent.request();
        let receipt = reservation::receipt_stub(intent);
        self.db
            .run(|tr, _| {
                let request = &request;
                let receipt = &receipt;
                async move {
                    requests::retry_result(
                        async {
                            let ns = routing::require_runtime_active_owner(
                                &tr,
                                &request.library_serial,
                                route,
                                round,
                            )
                            .await?;
                            if ns != request.namespace || round != intent.owner_epoch {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let Some(plan) = for_receipt(&tr, receipt).await? else {
                                return Ok(false);
                            };
                            let window = requests::window(&tr, ns).await?;
                            let claim = claim_value(request.request_epoch, request.operation_id);
                            if !(window.first_retained..=window.accepting)
                                .contains(&request.request_epoch)
                                || reservation::active_file_attempt_at(&tr, request)
                                    .await?
                                    .is_some()
                                || tr
                                    .get(
                                        &op_key(ns, request.request_epoch, request.operation_id),
                                        false,
                                    )
                                    .await?
                                    .is_some()
                                || tr.get(&plan.claim(ns)?, false).await?.as_deref()
                                    == Some(claim.as_slice())
                            {
                                return Ok(false);
                            }
                            let metadata_key = super::key(request);
                            if let Some(raw) = tr.get(&metadata_key, false).await? {
                                super::decode(&raw, request)?;
                                tr.clear(&metadata_key);
                            }
                            forget_plan(&tr, receipt);
                            Ok(true)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub(crate) async fn check_s3_volume_parents(
        &self,
        intent: &ActiveFileIntent,
        index: &LtfsIndex,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        if let Some(plan) = for_receipt(&tr, &reservation::receipt_stub(intent)).await? {
            for path in plan.directory_paths() {
                if index.find_file(&path).is_some() || index.find_directory(&path).is_some() {
                    return Err(CatalogError::Invalid("待创建 S3 目录在目标卷索引中已存在"));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn capture_s3_parent_evidence(
        &self,
        receipt: &CommitReceipt,
        batch: Uuid,
        index: &LtfsIndex,
    ) -> Result<Option<Vec<u8>>> {
        let tr = self.db.create_trx()?;
        let Some(plan) = for_receipt(&tr, receipt).await? else {
            return Ok(None);
        };
        let directories = plan
            .directory_paths()
            .map(|path| {
                let node = index
                    .find_directory(&path)
                    .ok_or(CatalogError::Invalid("实际索引缺少原 S3 隐含目录"))?;
                if index.find_file(&path).is_some() {
                    return Err(CatalogError::Invalid("S3 隐含目录与卷文件冲突"));
                }
                let recorded = (node.meta.file_uid, node.meta.creation_time.clone());
                if !node.xattrs.is_empty() || node.meta != new_directory_meta(&recorded) {
                    return Err(CatalogError::Invalid(
                        "实际 S3 隐含目录属性与新建目录不一致",
                    ));
                }
                Ok(recorded)
            })
            .collect::<Result<Vec<_>>>()?;
        encode_record(&ObservedParents {
            version: 1,
            operation: plan.operation,
            plan_digest: plan.digest()?,
            batch,
            receipt: evidence_fingerprint(receipt),
            epoch: receipt.request_epoch,
            directories,
        })
        .map(Some)
    }

    pub(crate) async fn verify_s3_parent_evidence(
        &self,
        receipt: &CommitReceipt,
        batch: Uuid,
        index: &LtfsIndex,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let saved = tr
            .get(
                &observed_key(receipt.volume_id, receipt.volume_generation),
                false,
            )
            .await?;
        // A successor verifies the original request even if the device
        // execution epoch advanced.
        if let Some(bytes) = saved.as_deref() {
            let evidence: ObservedParents = decode_record(bytes)?;
            if evidence.operation != receipt.operation_id {
                return Err(CatalogError::Corrupt("S3 隐含目录证明属于其他操作"));
            }
            let expected = self
                .capture_s3_parent_evidence(receipt, batch, index)
                .await?;
            if Some(bytes) != expected.as_deref() || evidence.batch != batch {
                return Err(CatalogError::Corrupt("S3 隐含目录的提交证明不匹配"));
            }
        } else if for_receipt(&tr, receipt).await?.is_some() {
            return Err(CatalogError::Corrupt("原 S3 隐含目录提交证明缺失"));
        }
        Ok(())
    }
}

pub(in crate::managed_catalog) async fn bind_evidence(
    tr: &Transaction,
    receipt: &CommitReceipt,
    batch: Uuid,
    evidence: &[u8],
) -> Result<()> {
    let saved: ObservedParents = decode_record(evidence)?;
    let plan = for_receipt(tr, receipt)
        .await?
        .ok_or(CatalogError::Corrupt("原 S3 隐含目录计划缺失"))?;
    if saved.version != 1
        || saved.batch != batch
        || saved.operation != plan.operation
        || saved.receipt != evidence_fingerprint(receipt)
        || saved.epoch != receipt.request_epoch
        || plan.digest()? != saved.plan_digest
        || !plan.baseline(tr, receipt.namespace).await?
        || saved.directories.len() != plan.dirs.len()
    {
        return Err(CatalogError::Invalid("S3 隐含目录证明与原计划不一致"));
    }
    let key = observed_key(receipt.volume_id, receipt.volume_generation);
    if let Some(previous) = tr.get(&key, false).await? {
        if previous.as_ref() != evidence {
            return Err(CatalogError::IdempotencyMismatch);
        }
    } else {
        tr.set(&key, evidence);
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn publish_at(
    tr: &Transaction,
    ns: Uuid,
    volume: Uuid,
    generation: u64,
    batch: Uuid,
) -> Result<()> {
    let raw = tr
        .get(&observed_key(volume, generation), false)
        .await?
        .ok_or(CatalogError::Corrupt("S3 隐含目录证明缺失"))?;
    let evidence: ObservedParents = decode_record(&raw)?;
    let plan = get_at(tr, ns, evidence.operation)
        .await?
        .ok_or(CatalogError::Corrupt("原 S3 隐含目录计划缺失"))?;
    if evidence.version != 1
        || evidence.batch != batch
        || evidence.directories.len() != plan.dirs.len()
        || plan.digest()? != evidence.plan_digest
        || !plan.baseline(tr, ns).await?
    {
        return Err(CatalogError::Invalid("S3 隐含目录的原祖先或路径已改变"));
    }
    let claim = plan.claim(ns)?;
    let expected = claim_value(evidence.epoch, plan.operation);
    if tr.get(&claim, false).await?.as_deref() != Some(expected.as_slice()) {
        return Err(CatalogError::Corrupt("S3 隐含目录预约缺失"));
    }
    tr.clear(&claim);
    let base_key = node_key(ns, plan.base);
    let raw = tr
        .get(&base_key, false)
        .await?
        .ok_or(CatalogError::Corrupt("S3 隐含目录祖先缺失"))?;
    let mut base = decode_node(&raw)?;
    base.directory_version = Some(
        plan.base_version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("S3 祖先目录版本溢出"))?,
    );
    tr.set(&base_key, &encode_node(&base));
    let count = plan.dirs.len();
    let mut parent = plan.base;
    for (position, ((name, id), attrs)) in
        plan.dirs.into_iter().zip(evidence.directories).enumerate()
    {
        let name = name.into_bytes();
        let attributes = NodeAttributes::new(new_directory_meta(&attrs), Vec::new(), None)?;
        tr.set(&child_key(ns, parent, &name), id.as_bytes());
        tr.set(
            &node_key(ns, id),
            &encode_node(&Node {
                kind: NodeKind::Directory,
                directory_version: Some(if position + 1 < count { 2 } else { 1 }),
                parent,
                name,
                attributes: Some(attributes),
            }),
        );
        parent = id;
    }
    Ok(())
}

fn new_directory_meta((uid, time): &(u64, String)) -> NodeMeta {
    NodeMeta {
        file_uid: *uid,
        creation_time: time.clone(),
        change_time: time.clone(),
        modify_time: time.clone(),
        access_time: time.clone(),
        backup_time: time.clone(),
        ..Default::default()
    }
}

pub(in crate::managed_catalog) fn claim_value(epoch: u64, operation: Uuid) -> [u8; 24] {
    let mut value = [0; 24];
    value[..8].copy_from_slice(&epoch.to_be_bytes());
    value[8..].copy_from_slice(operation.as_bytes());
    value
}

pub(in crate::managed_catalog) async fn release_claim(
    tr: &Transaction,
    receipt: &CommitReceipt,
) -> Result<bool> {
    if let Some(plan) = for_receipt(tr, receipt).await? {
        let key = plan.claim(receipt.namespace)?;
        let expected = claim_value(receipt.request_epoch, receipt.operation_id);
        if tr.get(&key, false).await?.as_deref() == Some(expected.as_slice()) {
            tr.clear(&key);
        }
        return Ok(true);
    }
    Ok(false)
}

pub(in crate::managed_catalog) fn forget_plan(tr: &Transaction, receipt: &CommitReceipt) {
    tr.clear(&plan_key(receipt.namespace, receipt.operation_id));
}

pub(in crate::managed_catalog) async fn finish_at(
    tr: &Transaction,
    ns: Uuid,
    volume: Uuid,
    generation: u64,
    batch: Uuid,
) -> Result<()> {
    let key = observed_key(volume, generation);
    let raw = tr
        .get(&key, false)
        .await?
        .ok_or(CatalogError::Corrupt("已发布 S3 隐含目录证明缺失"))?;
    let evidence: ObservedParents = decode_record(&raw)?;
    let plan = get_at(tr, ns, evidence.operation)
        .await?
        .ok_or(CatalogError::Corrupt("已发布 S3 隐含目录计划缺失"))?;
    if evidence.version != 1 || evidence.batch != batch || evidence.plan_digest != plan.digest()? {
        return Err(CatalogError::Corrupt("已发布 S3 隐含目录证明不一致"));
    }
    tr.clear(&plan_key(ns, evidence.operation));
    tr.clear(&key);
    Ok(())
}

pub(in crate::managed_catalog) fn clear_evidence(tr: &Transaction, volume: Uuid, generation: u64) {
    tr.clear(&observed_key(volume, generation));
}
