//! Restore the original execution arguments by bounded reads of sealed facts.
//! A restored request is not device permission or fresh media evidence.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Context {
    file: ReclaimFile,
    epoch: u64,
    request: [u8; 32],
}

impl Context {
    pub(super) fn validate(&self) -> Result<()> {
        self.file.validate()?;
        if self.epoch == 0 {
            return Err(CatalogError::Corrupt("元数据恢复缺少原子请求 epoch"));
        }
        Ok(())
    }

    pub(super) fn new(
        reservation: &ReclaimReservation,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> Result<Self> {
        let value = Self {
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
        if self.file != *file
            || self.epoch != request.request_epoch
            || self.request != request.fingerprint()
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
        // Apply the same pool-relative path limits as the original request.
        pool_catalog_path(reservation.spec().pool, file.path().as_bytes())?;
        Ok(())
    }
}

impl ManagedCatalog {
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
            original.encode()?;
            let tr = self.db.create_trx()?;
            parent_at(&tr, reservation, owner).await?;
            let task = load_id_at(
                &tr,
                reservation,
                original.plan().source_uid(),
                original.id(),
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            require_phase_history(original, &task)?;
            if original.plan() != task.plan() {
                return Err(CatalogError::Invalid("元数据恢复改变原固定计划"));
            }
            if task.owner() != owner || original.owner() != owner {
                return Err(CatalogError::OwnerFenced);
            }
            if !task.holds_claim() {
                return Err(CatalogError::Invalid("终态元数据尝试不再需要执行恢复"));
            }
            let context = &task.0.context;
            let (intent, _) = children::child_at(
                &tr,
                reservation.namespace(),
                reservation.spec().pool,
                context.epoch,
                context.file.operation(),
                owner,
            )
            .await?
            .ok_or(CatalogError::Incomplete)?;
            let request = intent.request();
            context.require(reservation, &context.file, &request)?;
            input_at(
                &tr,
                reservation,
                owner,
                &context.file,
                &request,
                &task.plan(),
            )
            .await?;
            Ok((context.file.clone(), request))
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
