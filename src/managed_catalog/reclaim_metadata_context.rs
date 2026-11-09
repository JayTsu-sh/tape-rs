//! Restore the original execution arguments by bounded reads of sealed facts.
//! A restored request is not device permission or fresh media evidence.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum Context {
    File {
        file: ReclaimFile,
        epoch: u64,
        request: [u8; 32],
    },
    Raw {
        raw: Box<raw_read::ReadFact>,
    },
    Node {
        node: bool,
    },
}

/* Original file context remains an immutable child binding. Raw directory/link
 * work binds its sealed source node directly through the plan instead. */
impl Context {
    pub(super) fn is_node(&self) -> bool {
        matches!(self, Self::Node { .. })
    }
    pub(super) fn is_source_based(&self) -> bool {
        matches!(self, Self::Node { .. } | Self::Raw { .. })
    }
    pub(super) fn require_plan(&self, plan: &ReclaimMetadataPlan) -> Result<()> {
        self.validate()?;
        match self {
            Self::Raw { raw } => raw.require_plan(plan),
            Self::Node { .. } if plan.is_node_metadata() && !plan.is_raw_body() => Ok(()),
            Self::File { .. } if !plan.is_source_based() => Ok(()),
            _ => Err(CatalogError::Corrupt("原始元数据上下文与计划种类不同")),
        }
    }
    pub(super) fn node() -> Self {
        Self::Node { node: true }
    }
    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::Node { node: true } => Ok(()),
            Self::Raw { raw } => raw.validate(),
            Self::File { file, epoch, .. } if *epoch > 0 => file.validate(),
            _ => Err(CatalogError::Corrupt(
                "原始元数据上下文类型或子请求代次无效",
            )),
        }
    }
    pub(super) fn new(
        reservation: &ReclaimReservation,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<Self> {
        let value = Self::File {
            file: file.clone(),
            epoch: request.request_epoch,
            request: request.fingerprint(),
        };
        value.require(reservation, file, request)?;
        Ok(value)
    }
    pub(super) fn require(
        &self,
        reservation: &ReclaimReservation,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<()> {
        self.validate()?;
        file.validate()?;
        request.validate()?;
        let Self::File {
            file: original,
            epoch,
            request: fingerprint,
        } = self
        else {
            return Err(CatalogError::Invalid("原始节点任务没有文件子请求上下文"));
        };
        if original != file
            || *epoch != request.request_epoch
            || *fingerprint != request.fingerprint()
            || request.namespace != reservation.namespace()
            || request.library_serial != reservation.serial()
            || request.pool_id != Some(reservation.spec().pool)
            || file.operation() != request.operation_id
            || file.file_id() != request.file_id
            || Some(file.version()) != request.expected_version
            || file.path() != request.ltfs_path()?
            || file.size() != request.expected_size
        {
            return Err(CatalogError::Invalid("元数据恢复改变原文件或封存子请求"));
        }
        pool_catalog_path(reservation.spec().pool, file.path().as_bytes())?;
        Ok(())
    }
}

// Read an already claimed original handle; this never changes ownership.
pub(super) async fn claimed_task_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    original: &ReclaimMetadataWrite,
) -> Result<ReclaimMetadataWrite> {
    original.encode()?;
    parent_at(tr, reservation, owner).await?;
    let task = load_id_at(tr, reservation, original.plan().source_uid(), original.id())
        .await?
        .ok_or(CatalogError::Incomplete)?;
    require_phase_history(original, &task)?;
    if original.plan() != task.plan() {
        return Err(CatalogError::Invalid("元数据恢复改变原固定计划"));
    }
    if task.owner() != owner || original.owner() != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(task)
}

pub(super) async fn original_context_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    task: &ReclaimMetadataWrite,
) -> Result<(ReclaimFile, ActiveFileRequest)> {
    let context = &task.0.context;
    context.validate()?;
    let Context::File { file, epoch, .. } = context else {
        return Err(CatalogError::Invalid("原始节点任务没有正文子请求"));
    };
    let (intent, _) = children::child_at(
        tr,
        reservation.namespace(),
        reservation.spec().pool,
        *epoch,
        file.operation(),
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    let request = intent.request();
    context.require(reservation, file, &request)?;
    input_at(tr, reservation, owner, file, &request, &task.plan()).await?;
    Ok((file.clone(), request))
}

pub(super) async fn input_for_task_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    task: &ReclaimMetadataWrite,
) -> Result<Option<ReclaimMetadataInput>> {
    input_for_plan_at(tr, reservation, owner, &task.0.context, &task.plan()).await
}

pub(super) async fn input_for_plan_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    context: &Context,
    plan: &ReclaimMetadataPlan,
) -> Result<Option<ReclaimMetadataInput>> {
    context.require_plan(plan)?;
    if let Context::Raw { raw } = context {
        raw.require_at(tr, reservation, plan).await?;
        return Ok(None);
    }
    if context.is_node() {
        let (_, index) = source_state_at(tr, reservation).await?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, index);
        let manifest = ledger::require_complete_at(tr, reservation).await?;
        if !plan.is_node_metadata()
            || head != plan.0.source_index
            || plan.0.inventory != manifest.summary.digest
            || tr
                .get(&ledger::object_key(reservation, plan.source_uid()), false)
                .await?
                .as_deref()
                != Some(plan.0.source.encode()?.as_slice())
        {
            return Err(CatalogError::Corrupt("原始节点任务与封存来源或类型不同"));
        }
        return Ok(None);
    }
    let Context::File { file, epoch, .. } = context else {
        unreachable!()
    };
    let (intent, _) = children::child_at(
        tr,
        reservation.namespace(),
        reservation.spec().pool,
        *epoch,
        file.operation(),
        owner,
    )
    .await?
    .ok_or(CatalogError::Incomplete)?;
    let request = intent.request();
    context.require(reservation, file, &request)?;
    input_at(tr, reservation, owner, file, &request, plan)
        .await
        .map(Some)
}

impl ManagedCatalog {
    /// Original file inputs for parent scheduling, including a retained
    /// resolution. Restoring facts does not admit or retry any device command.
    pub async fn reclaim_metadata_file_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataWrite,
    ) -> Result<Option<(ReclaimFile, ActiveFileRequest)>> {
        let tr = self.db.create_trx()?;
        let task = claimed_task_at(&tr, reservation, owner, original).await?;
        if task.0.context.is_source_based() {
            return Ok(None);
        }
        original_context_at(&tr, reservation, owner, &task)
            .await
            .map(Some)
    }

    /// Recover only the exact active attempt after explicit owner claiming.
    /// All arguments and retained input proofs are checked at one read version;
    /// no namespace walk, checkpoint mutation, request admission or CDB occurs.
    pub fn reclaim_metadata_execution_context<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<(ReclaimFile, ActiveFileRequest)>> + 'a>,
    > {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            let task = claimed_task_at(&tr, reservation, owner, original).await?;
            if !task.holds_claim() {
                return Err(CatalogError::Invalid("终态元数据尝试不再需要执行恢复"));
            }
            original_context_at(&tr, reservation, owner, &task).await
        })
    }

    /// Restore a settled Closed attempt's original retry arguments, never I/O
    /// permission. Only an explicit retry may subsequently prepare a successor.
    /// Completed publication uses its independent history instead of this path.
    pub fn reclaim_metadata_retry_context<'a>(
        &'a self,
        reservation: &'a ReclaimReservation,
        owner: u64,
        original: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<(ReclaimFile, ActiveFileRequest)>> + 'a>,
    > {
        Box::pin(async move {
            let tr = self.db.create_trx()?;
            let task = claimed_task_at(&tr, reservation, owner, original).await?;
            if !task.closed() {
                return Err(CatalogError::Invalid("只有原 Closed 尝试可恢复重试参数"));
            }
            original_context_at(&tr, reservation, owner, &task).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_context_rejects_invalid_persisted_file_before_uuid_getters() {
        let value = json!({"file":{"path":"/a","file":Uuid::new_v4().to_string(),
            "version":1,"size":0,"operation":Uuid::new_v4().to_string()},
            "epoch":1,"request":vec![7u8;32]});
        let context: Context = serde_json::from_value(value.clone()).unwrap();
        context.validate().unwrap();
        for (field, changed) in [
            ("file", json!("invalid")),
            ("operation", json!(Uuid::nil().to_string())),
            ("version", json!(0)),
            ("path", json!("/")),
            ("path", json!("/../escape")),
        ] {
            let mut bad = value.clone();
            bad["file"][field] = changed;
            let decoded: Context = serde_json::from_value(bad).unwrap();
            assert!(decoded.validate().is_err(), "未拒绝 {field}");
        }
        let mut bad = value.clone();
        bad["epoch"] = json!(0);
        assert!(
            serde_json::from_value::<Context>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut unknown = value;
        unknown["permission"] = json!(true);
        assert!(serde_json::from_value::<Context>(unknown).is_err());
    }
}
