//! Original automatic mechanical command, exclusive with explicit device tasks.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutomaticMediaAction {
    Load,
    MoveIn,
    UnloadMove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticMediaPlan {
    pub action: AutomaticMediaAction,
    pub barcode: String,
    pub volume: String,
    pub pool: String,
    pub drive_serial: String,
    pub source: u16,
    pub destination: u16,
    pub role: char,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticMediaCommand {
    version: u8,
    operation: String,
    owner: u64,
    route: Vec<u8>,
    pub plan: AutomaticMediaPlan,
}
fn invalid() -> CatalogError {
    CatalogError::Corrupt("自动机械原检查点或计划无效")
}
fn canonical_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or_else(invalid)
}
fn validate_plan(plan: &AutomaticMediaPlan) -> Result<()> {
    validate_barcode(&plan.barcode)?;
    validate_serial(&plan.drive_serial)?;
    canonical_uuid(&plan.volume)?;
    canonical_uuid(&plan.pool)?;
    if !matches!(plan.role, 'r' | 'm')
        || plan.drive_serial.contains('/')
        || plan.drive_serial.chars().any(char::is_whitespace)
        || ((plan.action == AutomaticMediaAction::Load) != (plan.source == plan.destination))
    {
        return Err(invalid());
    }
    Ok(())
}
fn encoded(command: &AutomaticMediaCommand) -> Result<Vec<u8>> {
    serde_json::to_value(command)
        .map(|value| value.to_string().into_bytes())
        .map_err(|e| CatalogError::Cache(e.to_string()))
}
pub(super) fn gate(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = management::prefix(ns, b'K');
    key.push(b'a');
    append_component(&mut key, serial.as_bytes());
    key
}
pub(super) async fn pending_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<Option<AutomaticMediaCommand>> {
    let Some(raw) = tr.get(&gate(ns, serial), false).await? else {
        return Ok(None);
    };
    if raw.len() > 4096 {
        return Err(invalid());
    }
    let command: AutomaticMediaCommand = serde_json::from_slice(&raw).map_err(|_| invalid())?;
    validate_plan(&command.plan)?;
    canonical_uuid(&command.operation)?;
    if command.version != 1
        || command.owner == 0
        || encoded(&command)?.as_slice() != raw.as_ref()
        || routing::decode_active(&command.route)?.route.namespace != ns
    {
        return Err(invalid());
    }
    Ok(Some(command))
}
async fn original(
    tr: &Transaction,
    serial: &str,
    route: ActiveLibraryRoute,
    owner: u64,
    command: &AutomaticMediaCommand,
) -> Result<()> {
    let ns = routing::require_runtime_active_owner(tr, serial, route, owner).await?;
    if command.owner > owner
        || command.route != routing::encode_active(route)
        || pending_at(tr, ns, serial).await?.as_ref() != Some(command)
    {
        return Err(CatalogError::OwnerFenced);
    }
    // No explicit mechanical task can be admitted beside the automatic gate.
    if tr
        .get(&native_device::library_gate(ns, serial), false)
        .await?
        .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    let assignment = ManagedCatalog::verified_assignment(tr, serial, &command.plan.barcode)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if assignment.namespace != ns
        || assignment.pool_id != canonical_uuid(&command.plan.pool)?
        || assignment.volume_id != Some(canonical_uuid(&command.plan.volume)?)
    {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}
impl ManagedCatalog {
    pub async fn automatic_media_pending(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<Option<AutomaticMediaCommand>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        let saved = pending_at(&tr, ns, serial).await?;
        if let Some(command) = &saved {
            original(&tr, serial, route, owner, command).await?;
        }
        Ok(saved)
    }
    /// The nonce belongs to this invocation, including its FDB retries. A later
    /// caller only observes the original command and cannot acquire permission.
    pub async fn issue_automatic_media(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        plan: &AutomaticMediaPlan,
    ) -> Result<(AutomaticMediaCommand, bool)> {
        validate_plan(plan)?;
        let proposed = AutomaticMediaCommand {
            version: 1,
            operation: Uuid::new_v4().to_string(),
            owner,
            route: routing::encode_active(route).to_vec(),
            plan: plan.clone(),
        };
        self.db
            .run(|tr, _| {
                let proposed = &proposed;
                async move {
                    requests::retry_result(
                        async {
                            let ns =
                                routing::require_runtime_active_owner(&tr, serial, route, owner)
                                    .await?;
                            if let Some(saved) = pending_at(&tr, ns, serial).await? {
                                original(&tr, serial, route, owner, &saved).await?;
                                if saved.plan != *plan {
                                    return Err(CatalogError::PoolBusy);
                                }
                                let granted = saved == *proposed;
                                return Ok((saved, granted));
                            }
                            native_device::require_library_available(&tr, ns, serial).await?;
                            tr.set(&gate(ns, serial), &encoded(proposed)?);
                            original(&tr, serial, route, owner, proposed).await?;
                            Ok((proposed.clone(), true))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                }
            })
            .await?
    }
    pub async fn require_automatic_media(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        command: &AutomaticMediaCommand,
    ) -> Result<()> {
        original(&self.db.create_trx()?, serial, route, owner, command).await
    }
    /// Only the device actor calls this after the original physical postcondition.
    pub async fn confirm_automatic_media(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        command: &AutomaticMediaCommand,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        original(&tr, serial, route, owner, command).await?;
                        tr.clear(&gate(route.route.namespace, serial));
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}
