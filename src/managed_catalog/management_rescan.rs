//! Only the read-only library-rescan kind can be restarted from a running task.
//! No media mutation may use this resume rule.
use super::*;

fn rescan_request(
    serial: &str,
    route: ActiveLibraryRoute,
    request: &ManagementRequest,
) -> Result<()> {
    if request.value()["library"] != serial
        || request.namespace() != route.route.namespace
        || request.value()["change"]["kind"] != "library-rescan"
    {
        return Err(CatalogError::Invalid(
            "rescan请求与库、命名空间或操作类型不符",
        ));
    }
    Ok(())
}
async fn original_task(
    tr: &Transaction,
    serial: &str,
    request: &ManagementRequest,
) -> Result<Value> {
    let task = read_task_at(
        tr,
        request.namespace(),
        serial,
        request.epoch(),
        request.operation(),
    )
    .await?
    .ok_or(CatalogError::Corrupt("已排队rescan原请求缺失"))?;
    if task["fingerprint"] != request.fingerprint() {
        return Err(CatalogError::Corrupt("rescan原请求身份不符"));
    }
    Ok(task)
}
impl ManagedCatalog {
    /// Confirm running before a device read. A new owner may resume this exact
    /// read-only request; an existing terminal receipt is returned unchanged.
    pub async fn begin_library_rescan(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
    ) -> Result<Value> {
        rescan_request(serial, route, request)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = original_task(&tr, serial, request).await?;
                        if !matches!(task["status"].as_str(), Some("queued" | "running")) {
                            return Ok(task);
                        }
                        if task["owner_epoch"].as_u64().is_some_and(|old| old > owner) {
                            return Err(CatalogError::Corrupt("rescan执行轮次超过当前owner"));
                        }
                        let claim = exclusion::LibraryTaskClaim::for_request(request)?;
                        if task["status"] == "queued" {
                            exclusion::require_available_at(&tr, ns, serial).await?;
                        } else {
                            claim.require_owned(&tr).await?;
                        }
                        native_device::require_library_available(&tr, ns, serial).await?;
                        if task["status"] == "running" && task["owner_epoch"] == owner {
                            return Ok(task);
                        }
                        task["status"] = json!("running");
                        task["owner_epoch"] = json!(owner);
                        write_task(&tr, request, &task).await?;
                        claim.acquire(&tr);
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
    /// Derive a final receipt only from the original, confirmed inventory and
    /// monitor. The actor pauses new samples until this acknowledgement is known.
    pub async fn complete_library_rescan(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ManagementRequest,
        captured_at: &str,
        error: Option<&str>,
    ) -> Result<Value> {
        rescan_request(serial, route, request)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner)
                            .await?;
                        let mut task = original_task(&tr, serial, request).await?;
                        if matches!(
                            task["status"].as_str(),
                            Some("succeeded" | "failed" | "cancelled")
                        ) {
                            return Ok(task);
                        }
                        if task["status"] != "running" || task["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        let claim = exclusion::LibraryTaskClaim::for_request(request)?;
                        claim.require_owned(&tr).await?;
                        let result = library_observations::rescan_result_at(
                            &tr,
                            ns,
                            serial,
                            owner,
                            captured_at,
                            error,
                        )
                        .await?;
                        task["status"] = json!(if error.is_some() {
                            "failed"
                        } else {
                            "succeeded"
                        });
                        task["result"] = result;
                        task["completed_at_millis"] = json!(
                            now().max(task["created_at_millis"].as_u64().expect("validated time"))
                        );
                        write_task(&tr, request, &task).await?;
                        tr.clear(&identity_key(
                            ns,
                            serial,
                            b'q',
                            request.epoch(),
                            request.operation(),
                        ));
                        claim.release(&tr);
                        Ok(task)
                    }
                    .await,
                )
            })
            .await?
    }
}
