//! Original immutable S4 identity with a separately fenced live owner.
//! FDB authority only: cold media recovery must still verify the actual tape.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OriginalCheckpointPublication {
    Sealed,
    Published,
    Compacted,
}

pub(crate) struct OriginalCheckpointContext {
    scope: ReclaimGroupObservation,
    original_binding: [u8; 32],
    issuer: u64,
    attempt: Uuid,
    plan: ReclaimIndexAugmentation,
    expected: Observed,
    files: Vec<PreparedActiveFile>,
    paths: Vec<(Uuid, String)>,
    manifest: [u8; 32],
    phase: OriginalCheckpointPublication,
}
impl OriginalCheckpointContext {
    pub(crate) fn owner(&self) -> u64 {
        self.scope.owner()
    }
    pub(crate) fn issuing_execution_epoch(&self) -> u64 {
        self.issuer
    }
    pub(crate) fn namespace(&self) -> Uuid {
        self.scope.parent().namespace()
    }
    pub(crate) fn library_serial(&self) -> &str {
        self.scope.parent().serial()
    }
    pub(crate) fn group_id(&self) -> Uuid {
        self.scope.group().id()
    }
    pub(crate) fn attempt_id(&self) -> Uuid {
        self.attempt
    }
    pub(crate) fn batch_id(&self) -> Uuid {
        self.scope.batch_id()
    }
    pub(crate) fn previous_index(&self) -> ActiveIndexIdentity {
        self.plan.previous().expect("validated original checkpoint")
    }
    pub(crate) fn current_index(&self) -> ActiveIndexIdentity {
        active_volume::decode_index(self.previous_index().volume_uuid, &self.expected.index)
            .expect("validated original checkpoint observation")
    }
    pub(crate) fn scope(&self) -> &ReclaimGroupObservation {
        &self.scope
    }
    pub(crate) fn plan(&self) -> &ReclaimIndexAugmentation {
        &self.plan
    }
    pub(crate) fn files(&self) -> &[PreparedActiveFile] {
        &self.files
    }
    pub(crate) fn physical_paths(&self) -> impl Iterator<Item = (Uuid, &str)> {
        self.paths.iter().map(|(id, path)| (*id, path.as_str()))
    }
    pub(crate) fn publication_phase(&self) -> OriginalCheckpointPublication {
        self.phase
    }
    pub(crate) fn require_io_scope(&self, scope: &ReclaimGroupObservation) -> Result<()> {
        if !std::ptr::eq(scope, &self.scope) {
            return Err(CatalogError::OwnerFenced);
        }
        self.plan.require_checkpoint_binding(
            scope,
            self.batch_id(),
            self.previous_index(),
            self.current_index(),
        )
    }
    pub(crate) fn require_capture(&self, capture: &CapturedIndexSource) -> Result<()> {
        if self.plan.observe(capture)?.0 != self.expected {
            return Err(CatalogError::Invalid("原checkpoint完整物理观察改变"));
        }
        Ok(())
    }
    pub(in crate::managed_catalog) async fn require_at(
        &self,
        tr: &Transaction,
    ) -> Result<OriginalCheckpointPublication> {
        self.scope.require_at(tr).await?;
        let parent = self.scope.parent();
        let group = self.scope.group();
        let original = group_attempt::scope_at(tr, parent, self.owner(), group).await?;
        if original.binding_digest() != self.original_binding
            || original.issuing_execution_epoch() != self.issuer
            || original.attempt_id() != self.attempt
            || original.batch_id() != self.batch_id()
        {
            return Err(CatalogError::OwnerFenced);
        }
        let actual = require_actual_at(tr, parent, group).await?;
        if actual.fingerprint()? != self.plan.fingerprint()? {
            return Err(CatalogError::Corrupt("原checkpoint Actual改变"));
        }
        self.expected.require_full_dp(&actual)?;
        if self.expected.plan != actual.fingerprint()? {
            return Err(CatalogError::Invalid("原checkpoint期望不是原Actual"));
        }
        let mut files = Vec::with_capacity(group.members.len());
        for ordinal in 0..group.header.count {
            files.push(
                original_member_receipt_at(tr, parent, self.owner(), group, &actual, ordinal)
                    .await?,
            );
        }
        files.sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
        if files.len() != self.files.len()
            || files.iter().zip(&self.files).any(|(a, b)| {
                a.receipt != b.receipt
                    || a.attributes != b.attributes
                    || a.receipt.owner_epoch != self.issuer
            })
        {
            return Err(CatalogError::Invalid("原checkpoint收据、属性或issuer改变"));
        }
        let raw = tr
            .get(&key(parent, b'b', Some(self.batch_id()), None), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let binding: BatchBinding = native_decode(&raw)?;
        let mut fingerprints = files
            .iter()
            .map(|file| receipt_fingerprint(&file.receipt))
            .collect::<Vec<_>>();
        fingerprints.sort();
        if binding.version != 1
            || binding.batch != self.batch_id().to_string()
            || binding.plan != actual.fingerprint()?
            || binding.manifest != self.manifest
            || binding.receipts != digest(b"tape-rs/reclaim-index-batch-receipts/1", &fingerprints)?
        {
            return Err(CatalogError::Corrupt("原checkpoint批次binding改变"));
        }
        let global = tr
            .get(&batch_route_key(self.namespace(), self.batch_id()), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let route: BatchRoute = native_decode(&global)?;
        if route.version != 2
            || route.parent != *parent.intent()
            || route.group != self.group_id().to_string()
            || route.physical_attempt != self.attempt.to_string()
            || route.batch != self.batch_id().to_string()
            || route.manifest != self.manifest
            || route.plan != actual.fingerprint()?
            || tr
                .get(
                    &group_layout::control_key(parent, b'b', Some(self.batch_id())),
                    false,
                )
                .await?
                .as_deref()
                != Some(global.as_ref())
        {
            return Err(CatalogError::Corrupt(
                "原checkpoint mandatory route改变或缺失",
            ));
        }
        if let Some(raw) = tr
            .get(&key(parent, b'p', Some(group.id()), None), false)
            .await?
        {
            let saved: Observed = native_decode(&raw)?;
            saved.require_full_dp(&actual)?;
            if saved != self.expected {
                return Err(CatalogError::Invalid("原checkpoint已存观察与本轮期望不同"));
            }
        }
        let receipts = files
            .iter()
            .map(|file| file.receipt.clone())
            .collect::<Vec<_>>();
        active_batch::require_original_checkpoint_header_at(
            tr,
            self.library_serial(),
            self.owner(),
            self.issuer,
            self.batch_id(),
            self.manifest,
            self.current_index(),
            &receipts,
        )
        .await
    }
    pub(in crate::managed_catalog) async fn require_publishing_at(
        &self,
        tr: &Transaction,
    ) -> Result<OriginalCheckpointPublication> {
        let phase = self.require_at(tr).await?;
        let index = self.current_index();
        let observed = if phase == OriginalCheckpointPublication::Compacted {
            active_volume::observed_completed_batch_at(
                tr,
                self.library_serial(),
                index.volume_uuid,
                index.generation,
            )
            .await?
        } else {
            active_volume::observed_batch_at(
                tr,
                self.library_serial(),
                index.volume_uuid,
                index.generation,
            )
            .await?
        };
        if observed != Some((self.batch_id(), index)) {
            return Err(CatalogError::Invalid(
                "原checkpoint发布必须先登记原完整物理索引",
            ));
        }
        Ok(phase)
    }

    pub(in crate::managed_catalog) async fn require_member_at(
        &self,
        tr: &Transaction,
        batch: Uuid,
        receipt: &CommitReceipt,
        index: ActiveIndexIdentity,
    ) -> Result<()> {
        self.require_publishing_at(tr).await?;
        if batch != self.batch_id()
            || index != self.current_index()
            || receipt.owner_epoch != self.issuer
            || !self.files.iter().any(|file| file.receipt == *receipt)
        {
            return Err(CatalogError::Invalid("原checkpoint布局不是原issuer成员"));
        }
        active_batch::require_stored_checkpoint_at(
            tr,
            batch,
            index,
            Some(self.plan.target_file_count()),
        )
        .await
    }
}
impl ManagedCatalog {
    pub(crate) async fn open_original_checkpoint(
        &self,
        original: &OriginalAttemptObservation,
        expected: &ReclaimIndexObservation,
    ) -> Result<OriginalCheckpointContext> {
        let tr = self.db.create_trx()?;
        let scope = group_observation::at(
            &tr,
            original.parent(),
            original.owner(),
            original.group_id(),
        )
        .await?;
        let current =
            group_attempt::scope_at(&tr, original.parent(), original.owner(), scope.group())
                .await?;
        if current.binding_digest() != original.binding_digest()
            || current.attempt_id() != original.attempt_id()
            || current.batch_id() != original.batch_id()
        {
            return Err(CatalogError::OwnerFenced);
        }
        let plan = require_actual_at(&tr, original.parent(), scope.group()).await?;
        expected.0.require_full_dp(&plan)?;
        let identity = IndexSourceIdentity::from_bytes(&expected.0.source).map_err(cache)?;
        if expected.0.version != 1
            || expected.0.plan != plan.fingerprint()?
            || ActiveIndexIdentity::from(identity.head())
                != active_volume::decode_index(plan.previous()?.volume_uuid, &expected.0.index)?
            || expected.0.construct_end != expected.0.dp_eod
        {
            return Err(CatalogError::Invalid("原checkpoint期望观察与Actual不符"));
        }
        plan.require_checkpoint_binding(
            &scope,
            original.batch_id(),
            plan.previous()?,
            identity.head().into(),
        )?;
        let mut files = Vec::with_capacity(scope.group().members.len());
        let mut paths = Vec::with_capacity(scope.group().members.len());
        for ordinal in 0..scope.group().header.count {
            let file = original_member_receipt_at(
                &tr,
                original.parent(),
                original.owner(),
                scope.group(),
                &plan,
                ordinal,
            )
            .await?;
            paths.push((
                file.receipt.file_id,
                plan.members[usize::from(ordinal)].donor_path.clone(),
            ));
            files.push(file);
        }
        files.sort_by_key(|file| (file.receipt.request_epoch, file.receipt.operation_id));
        let raw = tr
            .get(
                &key(original.parent(), b'b', Some(original.batch_id()), None),
                false,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let binding: BatchBinding = native_decode(&raw)?;
        let mut context = OriginalCheckpointContext {
            scope,
            original_binding: original.binding_digest(),
            issuer: original.issuing_execution_epoch(),
            attempt: original.attempt_id(),
            plan,
            expected: expected.0.clone(),
            files,
            paths,
            manifest: binding.manifest,
            phase: OriginalCheckpointPublication::Sealed,
        };
        context.phase = context.require_at(&tr).await?;
        Ok(context)
    }
    pub(crate) async fn check_original_checkpoint_context(
        &self,
        context: &OriginalCheckpointContext,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        context.require_at(&tr).await.map(|_| ())
    }
}
