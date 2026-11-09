//! Native links have no upload body. Move only the exact current logical copy
//! after its original raw node has an independent committed target outcome.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    parent: String,
    file: ReclaimFile,
    attempt: String,
    attributes: [u8; 32],
    source_head: Vec<u8>,
    source_copy: Vec<u8>,
    target_head: Vec<u8>,
    target_copy: Vec<u8>,
}
impl Receipt {
    fn encode(&self) -> Result<Vec<u8>> {
        self.file.validate()?;
        node_mutation::id(&self.attempt)?;
        decode_head(&self.source_head)?;
        decode_head(&self.target_head)?;
        copy_records::decode_copy(&self.source_copy)?;
        copy_records::decode_copy(&self.target_copy)?;
        if self.version != 1 || self.parent.len() != 64 {
            return Err(CatalogError::Corrupt("原生链接迁移回执身份无效"));
        }
        let mut raw = serde_json::to_vec(self).map_err(cache)?;
        if raw.len() > 48_000 {
            return Err(CatalogError::Invalid("原生链接迁移回执超出预算"));
        }
        let sum = Sha256::new()
            .chain_update(b"tape-rs/reclaim-native-link/1")
            .chain_update(&raw)
            .finalize();
        raw.extend_from_slice(&sum);
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=48_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("原生链接迁移回执长度无效"));
        }
        let value: Self = serde_json::from_slice(&raw[..raw.len() - 32]).map_err(cache)?;
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("原生链接迁移回执完整性校验失败"));
        }
        Ok(value)
    }
}
fn key(reservation: &ReclaimReservation, file: &ReclaimFile) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'l');
    key.extend_from_slice(file.operation().as_bytes());
    key
}
fn reverse_value(copy: &PhysicalCopy) -> Result<Vec<u8>> {
    let mut raw = copy.version.to_be_bytes().to_vec();
    raw.extend_from_slice(&Sha256::digest(copy_records::encode_physical_copy(copy)?));
    Ok(raw)
}
impl ManagedCatalog {
    /// No CDB and no fake empty upload. The source capture is immutable input;
    /// only the original task's actual committed result authorizes relocation.
    pub async fn relocate_reclaim_native_link(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        file: &ReclaimFile,
        original: &ReclaimMetadataWrite,
        source: &CapturedIndexSource,
    ) -> Result<ReclaimWalk> {
        file.validate()?;
        let plan = original.plan();
        plan.validate_source(source)?;
        if plan.0.source.kind != ReclaimSourceNodeKind::Symlink || file.size() != 0 {
            return Err(CatalogError::Invalid(
                "原生链接迁移要求原始链接任务和零数据扫描节点",
            ));
        }
        let entry = source
            .entry_by_uid(plan.source_uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let original_attrs = node_metadata::attributes_of(&entry)?;
        let ns = reservation.namespace();
        let full = pool_catalog_path(reservation.spec().pool, file.path().as_bytes())?;
        // Hydrate a potentially external attribute object outside the atomic
        // publication transaction; its immutable digest is checked again there.
        let tr = self.db.create_trx()?;
        let resolved =
            lookup::resolve_active_path_at_mode(&tr, ns, lookup::path_components(&full)?, false)
                .await?;
        let Some(ResolvedPath::File {
            attributes: Some(attributes),
            ..
        }) = resolved
        else {
            return Err(CatalogError::Incomplete);
        };
        let logical = self.hydrate_attributes(ns, attributes).await?;
        if logical.link_kind()? != link_kind::LinkKind::Native
            || logical.symlink() != original_attrs.symlink()
        {
            return Err(CatalogError::Invalid("原生链接当前目标与封存物理链接不同"));
        }
        self.db
            .run(|tr, _| {
                let plan = &plan;
                let full = &full;
                let logical = &logical;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            if record_at(&tr, reservation, owner).await?["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let (verified, scan, index) = metadata_write::destination_at(
                                &tr,
                                reservation,
                                owner,
                                plan.source_uid(),
                                original.id(),
                            )
                            .await?;
                            if verified != *plan {
                                return Err(CatalogError::Invalid("原生链接迁移改变原固定计划"));
                            }
                            source_state_at(&tr, reservation).await?;
                            let Some(ResolvedPath::File {
                                id,
                                head,
                                copy,
                                attributes: Some(attributes),
                                ..
                            }) = lookup::resolve_active_path_at_mode(
                                &tr,
                                ns,
                                lookup::path_components(full)?,
                                false,
                            )
                            .await?
                            else {
                                return Err(CatalogError::Incomplete);
                            };
                            if id != file.file_id() || attributes.digest() != logical.digest() {
                                return Err(CatalogError::Invalid("原生链接 inode 或逻辑属性改变"));
                            }
                            attribute_objects::require_attributes(&tr, ns, Some(&attributes))
                                .await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                ns,
                                reservation.spec().volume(),
                            )
                            .await?;
                            let receipt_key = key(reservation, file);
                            if let Some(raw) = tr.get(&receipt_key, false).await? {
                                let receipt = Receipt::decode(&raw)?;
                                let old_head = decode_head(&receipt.source_head)?;
                                let old_copy = copy_records::decode_copy(&receipt.source_copy)?;
                                if receipt.parent != reservation.fingerprint()
                                    || receipt.file != *file
                                    || receipt.attempt != original.id().to_string()
                                    || receipt.attributes != attributes.digest()
                                    || receipt.target_head != encode_head(&head)
                                    || receipt.target_copy
                                        != copy_records::encode_physical_copy(&copy)?
                                    || tr
                                        .get(&copy_key(ns, id, old_head.copy_id), false)
                                        .await?
                                        .as_deref()
                                        != Some(
                                            copy_records::encode_reclaimed_copy(
                                                old_copy.version,
                                                old_copy.origin,
                                            )
                                            .as_slice(),
                                        )
                                    || tr
                                        .get(
                                            &volume_key(
                                                old_copy.volume_id,
                                                old_copy.volume_scan_id,
                                                ns,
                                                id,
                                                old_head.copy_id,
                                            ),
                                            false,
                                        )
                                        .await?
                                        .is_some()
                                    || tr
                                        .get(
                                            &volume_key(
                                                copy.volume_id,
                                                copy.volume_scan_id,
                                                ns,
                                                id,
                                                head.copy_id,
                                            ),
                                            false,
                                        )
                                        .await?
                                        .as_deref()
                                        != Some(reverse_value(&copy)?.as_slice())
                                {
                                    return Err(CatalogError::Corrupt(
                                        "原生链接迁移结果与原回执不同",
                                    ));
                                }
                                return walk::current_state_at(&tr, reservation).await;
                            }
                            let media = copy
                                .media
                                .as_ref()
                                .ok_or(CatalogError::Invalid("原生链接缺少物理来源"))?;
                            if head.version != file.version()
                                || head.size != 0
                                || copy.size != 0
                                || copy.content_digest != ContentDigest::Unknown
                                || copy.volume_id != reservation.spec().volume()
                                || copy.ltfs_file_uid != plan.source_uid()
                                || !media.native_symlink
                                || media.ee_alias
                                || media.logical_file_uid != plan.source_uid()
                                || media.physical_path != plan.0.source.path
                            {
                                return Err(CatalogError::Invalid(
                                    "原生链接迁移改变原当前副本或物理 UID",
                                ));
                            }
                            let source_raw = copy_records::encode_physical_copy(&copy)?;
                            if tr.get(&head_key(ns, id), false).await?.as_deref()
                                != Some(encode_head(&head).as_slice())
                                || tr
                                    .get(&copy_key(ns, id, head.copy_id), false)
                                    .await?
                                    .as_deref()
                                    != Some(source_raw.as_slice())
                                || tr
                                    .get(
                                        &volume_key(
                                            copy.volume_id,
                                            copy.volume_scan_id,
                                            ns,
                                            id,
                                            head.copy_id,
                                        ),
                                        false,
                                    )
                                    .await?
                                    .as_deref()
                                    != Some(reverse_value(&copy)?.as_slice())
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            let mut target = (*copy).clone();
                            target.version = head
                                .version
                                .checked_add(1)
                                .ok_or(CatalogError::Invalid("原生链接版本溢出"))?;
                            target.volume_id = plan.volume();
                            target.volume_scan_id = scan;
                            target.volume_generation = index.generation;
                            target.ltfs_file_uid = plan.uid();
                            let target_media =
                                target.media.as_mut().expect("validated native link");
                            target_media.logical_file_uid = plan.uid();
                            target_media.physical_path = plan.path().into();
                            let digest = Sha256::new()
                                .chain_update(b"tape-rs/reclaim-link-copy/1")
                                .chain_update(file.operation().as_bytes())
                                .chain_update(original.id().as_bytes())
                                .chain_update(id.as_bytes())
                                .finalize();
                            let copy_id = Uuid::from_bytes(digest[..16].try_into().unwrap());
                            if copy_id.is_nil() || copy_id == head.copy_id {
                                return Err(CatalogError::Corrupt("原生链接目标副本身份冲突"));
                            }
                            let target_key = copy_key(ns, id, copy_id);
                            let reverse_key = volume_key(target.volume_id, scan, ns, id, copy_id);
                            if tr.get(&target_key, false).await?.is_some()
                                || tr.get(&reverse_key, false).await?.is_some()
                            {
                                return Err(CatalogError::Corrupt("原生链接目标已有孤立副本"));
                            }
                            let target_head = FileHead {
                                version: target.version,
                                size: 0,
                                copy_id,
                            };
                            let target_raw = copy_records::encode_physical_copy(&target)?;
                            let receipt = Receipt {
                                version: 1,
                                parent: reservation.fingerprint(),
                                file: file.clone(),
                                attempt: original.id().to_string(),
                                attributes: attributes.digest(),
                                source_head: encode_head(&head),
                                source_copy: source_raw,
                                target_head: encode_head(&target_head),
                                target_copy: target_raw.clone(),
                            }
                            .encode()?;
                            let (state, state_key, state_raw) =
                                walk::finish_native_link_at(&tr, reservation, file).await?;
                            let reverse = reverse_value(&target)?;
                            let marker =
                                copy_records::encode_reclaimed_copy(copy.version, copy.origin);
                            // All fallible proof/encoding precedes the first mutation.
                            tr.set(&target_key, &target_raw);
                            tr.set(&reverse_key, &reverse);
                            tr.set(&head_key(ns, id), &encode_head(&target_head));
                            tr.set(&copy_key(ns, id, head.copy_id), &marker);
                            tr.clear(&volume_key(
                                copy.volume_id,
                                copy.volume_scan_id,
                                ns,
                                id,
                                head.copy_id,
                            ));
                            tr.set(&receipt_key, &receipt);
                            tr.set(&state_key, &state_raw);
                            namespace_visibility::publish(&tr, ns);
                            Ok(state)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}
