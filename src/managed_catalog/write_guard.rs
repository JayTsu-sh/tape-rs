//! One fresh read version for each per-CDB upload-group authority check.
//! No proof is cached across device commands; the transport still checks PR,
//! runtime permission and the original authority before and after every CDB.
use super::*;

impl ManagedCatalog {
    /// Check the whole original group in one coherent read-only transaction.
    /// A true entry means the exact original reservation or upgraded receipt
    /// exists. The caller must latch any disappearance after a previous check.
    pub async fn check_active_write_group(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        intents: &[ActiveFileIntent],
    ) -> Result<Vec<bool>> {
        if intents.is_empty() {
            return Err(CatalogError::Invalid("写入原请求组不能为空"));
        }
        let requests = intents
            .iter()
            .map(ActiveFileIntent::request)
            .collect::<Vec<_>>();
        for request in &requests {
            request.validate()?;
            if request.library_serial != serial || request.namespace != route.route.namespace {
                return Err(CatalogError::Invalid("写入原请求不属于当前库或命名空间"));
            }
        }
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        management::moves::require_readable_at(&tr, ns, serial).await?;
        let mut pools = std::collections::BTreeSet::new();
        let mut observed = Vec::with_capacity(intents.len());
        for (request, original) in requests.iter().zip(intents) {
            if let Some(pool) = request.pool_id
                && pools.insert(pool)
            {
                require_pool_library(&tr, ns, pool, serial).await?;
            }
            reclaim::require_request_at(&tr, request, owner).await?;
            let current = reservation::active_file_attempt_at(&tr, request).await?;
            if current.as_ref().is_some_and(|current| current != original) {
                return Err(CatalogError::Invalid("上传原物理尝试已改变"));
            }
            observed.push(current.is_some());
        }
        Ok(observed)
    }
}
