//! 原导入任务的机械检查点。记录已发命令，不以新 owner 许可重放未知命令。
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMediaLoadPlan {
    pub source: u16,
    pub destination: u16,
    pub drive_serial: String,
    pub home: Option<u16>,
    pub eviction: Option<NativeMediaEvictionPlan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMediaEvictionPlan {
    pub drive_serial: String,
    pub source: u16,
    pub barcode: String,
    #[serde(with = "uuid_text")]
    pub volume: Uuid,
    #[serde(with = "uuid_text")]
    pub pool: Uuid,
    pub home: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeMediaLoadPhase {
    Planned,
    EvictionIssued,
    Evicted,
    MoveIssued,
    Moved,
    LoadIssued,
    Loaded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeMediaLoadProgress {
    pub plan: NativeMediaLoadPlan,
    pub phase: NativeMediaLoadPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    #[serde(with = "uuid_text")]
    nonce: Uuid,
    owner: u64,
    command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    request: Value,
    plan: NativeMediaLoadPlan,
    phase: NativeMediaLoadPhase,
    attempt: Option<Attempt>,
}
fn key(import: &NativeMediaImport) -> Vec<u8> {
    let mut key = management::prefix(import.request().namespace(), b'K');
    key.push(b'L');
    append_component(&mut key, import.serial().as_bytes());
    key.extend_from_slice(&import.request().epoch().to_be_bytes());
    key.extend_from_slice(import.request().operation().as_bytes());
    key
}
fn bytes(record: &Record) -> Result<Vec<u8>> {
    Ok(serde_json::to_value(record)
        .map_err(|e| CatalogError::Cache(e.to_string()))?
        .to_string()
        .into_bytes())
}
fn validate_plan(plan: &NativeMediaLoadPlan) -> Result<()> {
    validate_serial(&plan.drive_serial)?;
    if plan.drive_serial.contains('/') || plan.drive_serial.chars().any(char::is_whitespace) {
        return Err(CatalogError::Invalid("导入机械计划的驱动器序列号无效"));
    }
    if let Some(eviction) = &plan.eviction {
        validate_barcode(&eviction.barcode)?;
        validate_serial(&eviction.drive_serial)?;
        if eviction.volume.is_nil() || eviction.pool.is_nil() || plan.source == plan.destination {
            return Err(CatalogError::Invalid(
                "导入退带计划的卷、池或驱动器地址无效",
            ));
        }
    }
    Ok(())
}
async fn read_at(tr: &Transaction, import: &NativeMediaImport) -> Result<Option<Record>> {
    let Some(raw) = tr.get(&key(import), false).await? else {
        return Ok(None);
    };
    if raw.len() > 16_384 {
        return Err(CatalogError::Corrupt("导入机械检查点过大"));
    }
    let record: Record =
        serde_json::from_slice(&raw).map_err(|e| CatalogError::Cache(e.to_string()))?;
    validate_plan(&record.plan)?;
    if bytes(&record)?.as_slice() != raw.as_ref()
        || record.version != 1
        || record.request != *import.request().value()
        || record.attempt.as_ref().is_some_and(|a| {
            a.nonce.is_nil()
                || a.owner == 0
                || !matches!(a.command.as_str(), "move" | "load" | "evict")
        })
        || (record.phase == NativeMediaLoadPhase::Planned && record.attempt.is_some())
        || (record.phase == NativeMediaLoadPhase::MoveIssued
            && !record.attempt.as_ref().is_some_and(|a| a.command == "move"))
        || (record.phase == NativeMediaLoadPhase::LoadIssued
            && !record.attempt.as_ref().is_some_and(|a| a.command == "load"))
        || (record.phase == NativeMediaLoadPhase::MoveIssued
            && record.plan.source == record.plan.destination)
        || (matches!(
            record.phase,
            NativeMediaLoadPhase::EvictionIssued | NativeMediaLoadPhase::Evicted
        ) && (record.plan.eviction.is_none()
            || !record
                .attempt
                .as_ref()
                .is_some_and(|a| a.command == "evict")))
    {
        return Err(CatalogError::Corrupt("导入原机械计划、阶段或命令身份不符"));
    }
    Ok(Some(record))
}
fn progress(record: Record) -> NativeMediaLoadProgress {
    NativeMediaLoadProgress {
        plan: record.plan,
        phase: record.phase,
    }
}
async fn active_at(
    tr: &Transaction,
    import: &NativeMediaImport,
    owner: u64,
) -> Result<(Value, Option<Record>)> {
    let state = super::state(tr, import, owner).await?;
    let record = read_at(tr, import).await?;
    if record
        .as_ref()
        .and_then(|r| r.attempt.as_ref())
        .is_some_and(|a| a.owner > owner)
    {
        return Err(CatalogError::OwnerFenced);
    }
    Ok((state, record))
}
pub(in super::super) async fn require_settled_at(
    tr: &Transaction,
    import: &NativeMediaImport,
) -> Result<()> {
    if read_at(tr, import).await?.is_some_and(|r| {
        matches!(
            r.phase,
            NativeMediaLoadPhase::EvictionIssued
                | NativeMediaLoadPhase::MoveIssued
                | NativeMediaLoadPhase::LoadIssued
        )
    }) {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}
pub(in super::super) fn clear_at(tr: &Transaction, import: &NativeMediaImport) {
    tr.clear(&key(import));
}

impl ManagedCatalog {
    pub async fn native_media_load_progress(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<Option<NativeMediaLoadProgress>> {
        let tr = self.db.create_trx()?;
        Ok(active_at(&tr, import, owner).await?.1.map(progress))
    }

    /// 固定原驱动器、地址和 home；重建的调用方只能取回同一计划。
    pub async fn prepare_native_media_load(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        plan: &NativeMediaLoadPlan,
    ) -> Result<NativeMediaLoadProgress> {
        validate_plan(plan)?;
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let (state, saved) = active_at(&tr, import, owner).await?;
                        if let Some(saved) = saved {
                            if saved.plan != *plan {
                                return Err(CatalogError::Invalid("原导入机械计划不能改目标"));
                            }
                            return Ok(progress(saved));
                        }
                        if state["abort"] == true {
                            return Err(CatalogError::PoolImporting);
                        }
                        let record = Record {
                            version: 1,
                            request: import.request().value().clone(),
                            plan: plan.clone(),
                            phase: NativeMediaLoadPhase::Planned,
                            attempt: None,
                        };
                        if let Some(home) = plan.home {
                            native_device::stage_intake_home_at(&tr, import, home).await?;
                        }
                        tr.set(&key(import), &bytes(&record)?);
                        Ok(progress(record))
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }

    /// 每次调用的 nonce 在事务重试之间固定，不能由下一次调用或接管复用。
    pub async fn issue_native_media_eviction(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<bool> {
        self.issue_native_media_command(
            import,
            owner,
            "evict",
            NativeMediaLoadPhase::Planned,
            NativeMediaLoadPhase::EvictionIssued,
        )
        .await
    }
    pub async fn issue_native_media_move(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<bool> {
        self.issue_native_media_command(
            import,
            owner,
            "move",
            NativeMediaLoadPhase::Planned,
            NativeMediaLoadPhase::MoveIssued,
        )
        .await
    }
    pub async fn issue_native_media_load(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<bool> {
        self.issue_native_media_command(
            import,
            owner,
            "load",
            NativeMediaLoadPhase::Moved,
            NativeMediaLoadPhase::LoadIssued,
        )
        .await
    }
    async fn issue_native_media_command(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        command: &str,
        from: NativeMediaLoadPhase,
        to: NativeMediaLoadPhase,
    ) -> Result<bool> {
        let nonce = Uuid::new_v4();
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let (state, record) = active_at(&tr, import, owner).await?;
                        if state["abort"] == true {
                            return Err(CatalogError::PoolImporting);
                        }
                        let mut record = record.ok_or(CatalogError::Incomplete)?;
                        if record.phase == to {
                            return Ok(record.attempt.as_ref().is_some_and(|a| {
                                a.nonce == nonce && a.owner == owner && a.command == command
                            }));
                        }
                        let phase_matches = if command == "move" && record.plan.eviction.is_some() {
                            record.phase == NativeMediaLoadPhase::Evicted
                        } else {
                            record.phase == from
                        };
                        if !phase_matches
                            || (command == "move" && record.plan.source == record.plan.destination)
                            || (command == "evict" && record.plan.eviction.is_none())
                        {
                            return Err(CatalogError::Invalid("原导入机械命令阶段不符"));
                        }
                        record.phase = to;
                        record.attempt = Some(Attempt {
                            nonce,
                            owner,
                            command: command.into(),
                        });
                        tr.set(&key(import), &bytes(&record)?);
                        Ok(true)
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
    pub async fn confirm_native_media_moved(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<()> {
        self.confirm_native_media_mechanical(import, owner, NativeMediaLoadPhase::Moved)
            .await
    }
    pub async fn confirm_native_media_evicted(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<()> {
        self.confirm_native_media_mechanical(import, owner, NativeMediaLoadPhase::Evicted)
            .await
    }
    pub async fn confirm_native_media_loaded(
        &self,
        import: &NativeMediaImport,
        owner: u64,
    ) -> Result<()> {
        self.confirm_native_media_mechanical(import, owner, NativeMediaLoadPhase::Loaded)
            .await
    }
    async fn confirm_native_media_mechanical(
        &self,
        import: &NativeMediaImport,
        owner: u64,
        to: NativeMediaLoadPhase,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let (_, record) = active_at(&tr, import, owner).await?;
                        let mut record = record.ok_or(CatalogError::Incomplete)?;
                        if record.phase == to {
                            return Ok(());
                        }
                        let allowed = match to {
                            NativeMediaLoadPhase::Evicted => {
                                record.phase == NativeMediaLoadPhase::EvictionIssued
                            }
                            NativeMediaLoadPhase::Moved => {
                                record.phase == NativeMediaLoadPhase::MoveIssued
                                    || (record.phase == NativeMediaLoadPhase::Planned
                                        && record.plan.source == record.plan.destination)
                            }
                            NativeMediaLoadPhase::Loaded => matches!(
                                record.phase,
                                NativeMediaLoadPhase::Moved | NativeMediaLoadPhase::LoadIssued
                            ),
                            _ => false,
                        };
                        if !allowed {
                            return Err(CatalogError::Invalid("原导入机械完成阶段不符"));
                        }
                        record.phase = to;
                        tr.set(&key(import), &bytes(&record)?);
                        Ok(())
                    }
                    .await
                    .map_err(notifications::atomic_error),
                )
            })
            .await?
    }
}

mod uuid_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(
        value: &Uuid,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        Uuid::parse_str(&text)
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == text)
            .ok_or_else(|| serde::de::Error::custom("目标 UUID 不规范"))
    }
}
