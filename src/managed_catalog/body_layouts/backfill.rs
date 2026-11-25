//! Exact catalog snapshots and actual recorded-source evidence for LB2.
use super::*;
use crate::ltfs::label::LtfsLabel;
use crate::ltfs::recovery::CapturedIndexSource;

const MAX_SNAPSHOT_ROWS: usize = 32;
const MAX_SNAPSHOT_VALUE: usize = 100_000;
type RawRow = (Vec<u8>, Option<Vec<u8>>);

/// This ephemeral snapshot is minted only by the original lookup transaction.
/// The actual current H row is independent of the selected historical head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) struct CopyReadSnapshot {
    metadata: ReadMetadata,
    source: ActiveIndexIdentity,
    baseline: Option<IndexSourceIdentity>,
    scan: VolumeScan,
    rows: Arc<[RawRow]>,
    authority: Option<RetainedLedgerAuthority>,
}

impl CopyReadSnapshot {
    pub(in crate::managed_catalog) async fn capture_at(
        tr: &Transaction,
        serial: &str,
        resolved: &ResolvedPath,
        source: ActiveIndexIdentity,
        baseline: Option<IndexSourceIdentity>,
        scan: VolumeScan,
    ) -> Result<Option<Arc<Self>>> {
        Self::capture_with_authority_at(tr, serial, resolved, source, baseline, scan, None).await
    }
    pub(in crate::managed_catalog) async fn capture_retained_at(
        tr: &Transaction,
        serial: &str,
        resolved: &ResolvedPath,
        source: ActiveIndexIdentity,
        baseline: Option<IndexSourceIdentity>,
        scan: VolumeScan,
        authority: RetainedLedgerAuthority,
    ) -> Result<Arc<Self>> {
        Self::capture_with_authority_at(
            tr,
            serial,
            resolved,
            source,
            baseline,
            scan,
            Some(authority),
        )
        .await?
        .ok_or(CatalogError::Incomplete)
    }
    pub(in crate::managed_catalog) async fn capture_with_authority_at(
        tr: &Transaction,
        serial: &str,
        resolved: &ResolvedPath,
        source: ActiveIndexIdentity,
        baseline: Option<IndexSourceIdentity>,
        scan: VolumeScan,
        authority: Option<RetainedLedgerAuthority>,
    ) -> Result<Option<Arc<Self>>> {
        let ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            attributes,
            ..
        } = resolved
        else {
            return Err(CatalogError::Invalid("正文布局快照要求文件"));
        };
        let record_key = copy_key(placement.namespace, *id, head.copy_id);
        let record = tr.get(&record_key, false).await?;
        let Some(raw) = record else {
            return Ok(None);
        };
        if raw.len() > MAX_SNAPSHOT_VALUE {
            return Err(CatalogError::Corrupt("正文布局 R 快照值过长"));
        }
        if let Some(ledger) = &authority {
            ledger.require_at(tr, ledger.owner()).await?;
            if !ledger.matches_selection(placement.namespace, *id, head, copy) {
                return Err(CatalogError::Corrupt("保留布局快照与原 child 不同"));
            }
        } else {
            match decode_stored_copy(&raw)? {
                StoredCopy::Present(present) if &present == copy.as_ref() => {}
                StoredCopy::Present(_) => {
                    return Err(CatalogError::Corrupt("正文布局原 R 与选中副本不同"));
                }
                StoredCopy::Reclaimed { .. } => return Ok(None),
            }
        }
        let mut rows = Vec::with_capacity(MAX_SNAPSHOT_ROWS);
        rows.push((record_key, Some(raw.to_vec())));
        // Capture the actual row, including its real absence. Do not encode
        // the request's selected head: retained historical reads may differ.
        let keys = [
            binding_key(placement.namespace, *id, head.copy_id),
            required_copy_key(placement.namespace, *id, head.copy_id),
            head_key(placement.namespace, *id),
            namespace_key(placement.namespace),
            control::managed_key(placement.namespace),
            owner_key(serial),
            routing::active_key(serial),
            routing::route_key(serial),
            ready_volume_key(copy.volume_id),
            reconcile_volume_key(copy.volume_id),
            unavailable_volume_key(placement.namespace, copy.volume_id),
            scan_key(copy.volume_id, scan.id),
            source_key(copy.volume_id, scan.id),
            volume_key(
                copy.volume_id,
                copy.volume_scan_id,
                placement.namespace,
                *id,
                head.copy_id,
            ),
        ];
        for key in keys {
            let value = tr.get(&key, false).await?;
            if value.as_ref().is_some_and(|v| v.len() > MAX_SNAPSHOT_VALUE) {
                return Err(CatalogError::Corrupt("正文布局原事务快照值过长"));
            }
            rows.push((key, value.map(|v| v.to_vec())));
        }
        rows.extend(control::body_layout_snapshot_rows_at(tr, placement).await?);
        rows.extend(active_volume::body_layout_snapshot_rows_at(tr, serial, source).await?);
        if rows.len() > MAX_SNAPSHOT_ROWS
            || rows
                .iter()
                .any(|(_, v)| v.as_ref().is_some_and(|v| v.len() > MAX_SNAPSHOT_VALUE))
        {
            return Err(CatalogError::Corrupt("正文布局原事务快照超过边界"));
        }
        Ok(Some(Arc::new(Self {
            metadata: ReadMetadata {
                id: *id,
                head: head.clone(),
                copy: (**copy).clone(),
                placement: placement.clone(),
                attributes: attributes.clone(),
            },
            source,
            baseline,
            scan,
            rows: rows.into(),
            authority,
        })))
    }

    fn original_record(&self) -> &[u8] {
        match &self.authority {
            Some(ledger) => ledger.original_record(),
            None => self.rows[0]
                .1
                .as_deref()
                .expect("private Present R snapshot"),
        }
    }

    fn original_global_binding(&self) -> Option<&[u8]> {
        let key = binding_key(
            self.metadata.placement.namespace,
            self.metadata.id,
            self.metadata.head.copy_id,
        );
        self.rows
            .iter()
            .find(|(original, _)| original == &key)
            .expect("private snapshot includes original binding row")
            .1
            .as_deref()
    }

    pub(in crate::managed_catalog) async fn require_at(
        &self,
        tr: &Transaction,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<()> {
        self.require_context_at(tr, serial, route, owner, None)
            .await
    }

    async fn require_attachment_at(
        &self,
        tr: &Transaction,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        binding: &CopyLayoutBinding,
        raw: &[u8],
    ) -> Result<()> {
        self.require_context_at(tr, serial, route, owner, Some((binding, raw)))
            .await
    }

    async fn require_context_at(
        &self,
        tr: &Transaction,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        completed: Option<(&CopyLayoutBinding, &[u8])>,
    ) -> Result<()> {
        let namespace = routing::require_runtime_active_owner(tr, serial, route, owner).await?;
        if let Some(authority) = &self.authority {
            authority.require_at(tr, owner).await?;
        }
        if namespace != self.metadata.placement.namespace
            || serial != self.metadata.placement.library_serial
        {
            return Err(CatalogError::OwnerFenced);
        }
        let completed_keys = if let Some((binding, raw)) = completed {
            // A commit-unknown retry may observe only this operation's exact
            // completed pair. All original business/owner rows still match.
            binding_at(tr, binding.namespace, binding.file, binding.copy).await?;
            let key = binding_key(binding.namespace, binding.file, binding.copy);
            let marker = required_copy_key(binding.namespace, binding.file, binding.copy);
            if tr.get(&key, false).await?.as_deref() == Some(raw)
                && tr.get(&marker, false).await?.as_deref()
                    == Some(required_copy_value(raw).as_slice())
            {
                Some((key, marker))
            } else {
                None
            }
        } else {
            None
        };
        for (key, expected) in self.rows.iter() {
            if completed_keys
                .as_ref()
                .is_some_and(|(binding, marker)| key == binding || key == marker)
            {
                continue;
            }
            if tr.get(key, false).await?.as_deref() != expected.as_deref() {
                return Err(CatalogError::Incomplete);
            }
        }
        let placement = control::verified_placement(tr, self.metadata.copy.volume_id)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if placement != self.metadata.placement {
            return Err(CatalogError::Incomplete);
        }
        let scan = require_volume_ready(tr, self.metadata.copy.volume_id, Some(self.scan)).await?;
        if self.metadata.copy.volume_scan_id != scan.id
            || self.metadata.copy.volume_generation < scan.generation
        {
            return Err(CatalogError::Incomplete);
        }
        let (source, identity) = if self.metadata.copy.volume_generation == scan.generation {
            let identity =
                index_evidence::require_identity(tr, self.metadata.copy.volume_id, scan).await?;
            (ActiveIndexIdentity::from(identity.head()), Some(identity))
        } else {
            (
                active_volume::observed_copy_index_at(
                    tr,
                    serial,
                    self.metadata.copy.volume_id,
                    self.metadata.copy.volume_generation,
                )
                .await?
                .ok_or(CatalogError::Incomplete)?,
                None,
            )
        };
        if source != self.source || identity != self.baseline {
            return Err(CatalogError::Incomplete);
        }
        Ok(())
    }
}

/// Private factory proof from the exact snapshot and real recorded capture.
/// The one decoded node's extents are moved, never cloned into a second array.
pub(crate) struct VerifiedExistingCopyLayout {
    snapshot: Arc<CopyReadSnapshot>,
    binding: CopyLayoutBinding,
    layout: BodyLayout<'static>,
    location: BindingLocation,
    original_binding: Option<Vec<u8>>,
}

impl VerifiedExistingCopyLayout {
    #[cfg(test)]
    pub(in crate::managed_catalog) fn test_binding(&self) -> &CopyLayoutBinding {
        &self.binding
    }
    pub(crate) fn from_captured(
        source: &ActiveReadSource,
        captured: &CapturedIndexSource,
        label: &LtfsLabel,
    ) -> Result<Self> {
        if source.retained_authority().is_some() {
            return Err(CatalogError::Incomplete);
        }
        Self::from_actual_source(source, captured, label, false)
    }
    pub(crate) fn from_retained_captured(
        source: &ActiveReadSource,
        captured: &CapturedIndexSource,
        label: &LtfsLabel,
    ) -> Result<Self> {
        if source.retained_authority().is_none() {
            return Err(CatalogError::Incomplete);
        }
        Self::from_actual_source(source, captured, label, true)
    }
    fn from_actual_source(
        source: &ActiveReadSource,
        captured: &CapturedIndexSource,
        label: &LtfsLabel,
        retained: bool,
    ) -> Result<Self> {
        let snapshot = source
            .existing_snapshot()
            .ok_or(CatalogError::Incomplete)?
            .clone();
        if snapshot.source != source.source
            || snapshot.metadata.copy != *source.copy()
            || label.volume_uuid != source.source.volume_uuid
        {
            return Err(CatalogError::Corrupt("LB2 工厂与原事务来源不同"));
        }
        let (path, node) = source.captured_body(captured)?;
        let expected = source.copy().content_digest;
        if expected != ContentDigest::Unknown {
            let key = match expected {
                ContentDigest::Sha256(_) => crate::ltfs::index::XATTR_SHA256,
                ContentDigest::Md5(_) => crate::ltfs::index::XATTR_MD5,
                ContentDigest::Unknown => unreachable!(),
            };
            let attrs = node
                .xattrs
                .iter()
                .filter(|x| x.key == key)
                .cloned()
                .collect::<Vec<_>>();
            if ContentDigest::from_ltfs_xattrs(&attrs)? != expected {
                return Err(CatalogError::Corrupt("LB2 已知摘要与原始正文节点不同"));
            }
        }
        let identity = captured
            .identity()
            .map_err(|e| CatalogError::Cache(e.to_string()))?;
        let layout = BodyLayout::new(
            label.volume_uuid,
            node.meta.file_uid,
            node.length,
            label.blocksize,
            [label.index_partition, label.data_partition],
            node.extents,
        )?;
        let metadata = &snapshot.metadata;
        let mut binding = CopyLayoutBinding {
            namespace: metadata.placement.namespace,
            file: metadata.id,
            copy: metadata.head.copy_id,
            version: metadata.copy.version,
            scan: metadata.copy.volume_scan_id,
            body_uid: metadata.copy.ltfs_file_uid,
            length: metadata.copy.size,
            content_digest: metadata.copy.content_digest,
            source: source.source,
            provenance: LayoutProvenance::VerifiedCopySource {
                copy_record_sha: copy_record_digest(snapshot.original_record()),
                source_identity_sha: identity.fingerprint(),
                source_identity: identity,
            },
            path: format!("/{path}"),
            manifest: layout.manifest()?,
        };
        binding.validate()?;
        // A legacy value is upgraded only after the same actual-source proof.
        // Its provenance and exact bytes remain unchanged, including LB1.
        let original_binding = snapshot.original_global_binding().map(Vec::from);
        if let Some(raw) = &original_binding {
            let original = CopyLayoutBinding::decode(raw)?;
            let mut physical = binding.clone();
            physical.provenance = original.provenance.clone();
            if physical != original
                || source.layout_binding() != Some(&original)
                || !original.matches_raw_copy(Some(snapshot.original_record()))
            {
                return Err(CatalogError::Corrupt("旧布局与本轮实际来源或原绑定不同"));
            }
            binding = original;
        }
        let location = if retained {
            BindingLocation::Retained {
                snapshot: snapshot.clone(),
                authority: source
                    .retained_authority()
                    .ok_or(CatalogError::Incomplete)?
                    .clone(),
                scoped: original_binding.is_none(),
            }
        } else {
            BindingLocation::Ordinary
        };
        Ok(Self {
            snapshot,
            binding,
            layout,
            location,
            original_binding,
        })
    }
}

impl ManagedCatalog {
    /// Stage and attach only this exact original copy. Each short transaction
    /// retains the current fence and all original raw facts. No media I/O occurs.
    pub(crate) async fn backfill_existing_body_layout(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        proof: &VerifiedExistingCopyLayout,
    ) -> Result<CopyLayoutBinding> {
        let binding = &proof.binding;
        let namespace = binding.namespace;
        let manifest = binding.manifest;
        let marker = progress_key(namespace, manifest);
        let mut next = 0;
        while proof.original_binding.is_none() && next < manifest.chunks {
            let expected = next;
            next = self
                .db
                .run(|tr, _| {
                    let marker = marker.clone();
                    async move {
                        requests::retry_result(
                            async {
                                tr.set_option(
                                    foundationdb::options::TransactionOption::SizeLimit(512_000),
                                )?;
                                proof.snapshot.require_at(&tr, serial, route, owner).await?;
                                if proof.location.retained() {
                                    binding_at(&tr, binding.namespace, binding.file, binding.copy)
                                        .await?;
                                }
                                let saved = tr.get(&marker, false).await?;
                                let first = saved
                                    .as_deref()
                                    .map(|v| decode_progress(v, manifest))
                                    .transpose()?
                                    .unwrap_or(0);
                                if first > expected {
                                    return Ok(first);
                                }
                                if first < expected {
                                    return Err(CatalogError::Corrupt("LB2 暂存进度倒退"));
                                }
                                let end =
                                    (first + CHUNKS_PER_TRANSACTION as u32).min(manifest.chunks);
                                for index in first..end {
                                    let bytes = proof.layout.chunk(manifest, index)?;
                                    let key = chunk_key(namespace, manifest, index);
                                    if let Some(old) = tr.get(&key, false).await? {
                                        if old.as_ref() != bytes {
                                            return Err(CatalogError::Corrupt("LB2 分片重试冲突"));
                                        }
                                    } else {
                                        tr.set(&key, &bytes);
                                    }
                                }
                                tr.set(&marker, &progress(manifest, end));
                                Ok(end)
                            }
                            .await,
                        )
                    }
                })
                .await??;
        }
        self.verify_body_layout_object_at(namespace, manifest, Some(proof.location.clone()))
            .await?;
        let raw = match &proof.original_binding {
            Some(original) => original.clone(),
            None => binding.encode()?,
        };
        let key = proof.location.key(binding);
        #[cfg(test)]
        attach_test_fault::fire(namespace, attach_test_fault::AttachTestPoint::BeforeSubmit)?;
        self.db
            .run(|tr, _| {
                let raw = raw.clone();
                let key = key.clone();
                async move {
                    requests::retry_result(
                        async {
                            if proof.original_binding.is_some() || !proof.location.retained() {
                                proof
                                    .snapshot
                                    .require_attachment_at(&tr, serial, route, owner, binding, &raw)
                                    .await?;
                            } else {
                                proof.snapshot.require_at(&tr, serial, route, owner).await?;
                            }
                            if proof.location.retained() {
                                binding_at(&tr, binding.namespace, binding.file, binding.copy)
                                    .await?;
                            }
                            require_complete(&tr, namespace, manifest).await?;
                            if proof.location.retained() && proof.original_binding.is_none() {
                                if let Some(old) = tr.get(&key, false).await? {
                                    if old.as_ref() != raw {
                                        return Err(CatalogError::Corrupt("LB2 保留绑定冲突"));
                                    }
                                } else {
                                    tr.set(&key, &raw);
                                }
                            } else {
                                install_global_binding_at(&tr, binding, &raw).await?;
                            }
                            Ok(())
                        }
                        .await,
                    )
                }
            })
            .await??;
        #[cfg(test)]
        attach_test_fault::fire(
            namespace,
            attach_test_fault::AttachTestPoint::AfterCommitted,
        )?;
        Ok(binding.clone())
    }
}
