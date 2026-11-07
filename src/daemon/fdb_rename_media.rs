//! Serial execution of one current volume intent. No fencing or changer I/O.
//! Errors consume the mounted volume; only a clean, confirmed volume returns.
use super::permission::{Permission, Phase};
use crate::error::{Result, TapeError};
use crate::ltfs::{recovery::TailKind, volume::LtfsVolume};
use crate::managed_catalog::{
    ActiveIndexIdentity, ActiveLibraryRoute, CatalogError, ManagedCatalog, NodeRenameAbsenceKind,
    NodeRenameAttempt, NodeRenameAttemptPhase, NodeRenameIntentClosure, NodeRenameObservation,
    NodeRenameVolumeIntent, NodeRenameVolumeProjection,
};
use crate::scsi::reservation::ReservationKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeRenameVolumeOutcome {
    Observed(NodeRenameObservation),
    Closed(NodeRenameIntentClosure),
    NeedsProjection,
}
pub struct NodeRenameVolumeCommit<'a> {
    volume: LtfsVolume<'a>,
    outcome: NodeRenameVolumeOutcome,
}
impl<'a> NodeRenameVolumeCommit<'a> {
    pub fn outcome(&self) -> &NodeRenameVolumeOutcome {
        &self.outcome
    }
    pub fn volume(&self) -> &LtfsVolume<'a> {
        &self.volume
    }
    pub fn into_volume(self) -> LtfsVolume<'a> {
        self.volume
    }
}

pub struct NodeRenameExecution<'a> {
    catalog: &'a ManagedCatalog,
    route: ActiveLibraryRoute,
    key: ReservationKey,
    permission: &'a Permission,
    budget: u64,
}
impl<'a> NodeRenameExecution<'a> {
    pub fn new(
        catalog: &'a ManagedCatalog,
        route: ActiveLibraryRoute,
        key: ReservationKey,
        permission: &'a Permission,
        budget: u64,
    ) -> Result<Self> {
        if !key.is_ours() || key.round() == 0 || budget == 0 {
            return Err(TapeError::Catalog("rename 执行作用域或字节预算无效".into()));
        }
        let value = Self {
            catalog,
            route,
            key,
            permission,
            budget,
        };
        value.guard()?;
        Ok(value)
    }
    fn guard(&self) -> Result<()> {
        if !(self.permission.allows(self.key.round(), Phase::Execution)
            || self.permission.allows(self.key.round(), Phase::Serving))
        {
            return Err(TapeError::OwnershipLost {
                device: "FDB rename".into(),
                detail: "rename 多数派执行许可已失效".into(),
            });
        }
        Ok(())
    }
    fn error(
        &self,
        intent: &NodeRenameVolumeIntent,
        stage: &str,
        error: CatalogError,
    ) -> TapeError {
        let detail = format!(
            "FDB rename {}/{} 卷 {} {stage}: {error}",
            intent.group(),
            intent.id(),
            intent.volume()
        );
        if matches!(error, CatalogError::OwnerFenced) {
            TapeError::OwnershipLost {
                device: intent.volume().to_string(),
                detail,
            }
        } else {
            TapeError::Catalog(detail)
        }
    }
    async fn append_policy(
        &self,
        attempt: &NodeRenameAttempt,
        intent: &NodeRenameVolumeIntent,
    ) -> Result<()> {
        self.guard()?;
        let request = attempt.global_plan().request();
        let placement = self
            .catalog
            .volume_placement(intent.volume())
            .await
            .map_err(|e| self.error(intent, "核对卷归属", e))?
            .ok_or_else(|| TapeError::Catalog("rename 卷归属缺失".into()))?;
        if placement.namespace != request.spec().source.namespace
            || placement.pool_id != request.spec().pool
            || placement.library_serial != request.library()
        {
            return Err(TapeError::Catalog("rename 卷与原任务归属不符".into()));
        }
        let head = self
            .catalog
            .active_loaded_volume_head(
                request.library(),
                &placement.barcode,
                self.route,
                self.key.round(),
            )
            .await
            .map_err(|e| self.error(intent, "核对追加策略", e))?
            .ok_or_else(|| TapeError::Catalog("rename 卷活动归属缺失".into()))?;
        if head.volume_id != intent.volume()
            || head.pool_id != request.spec().pool
            || head.index != intent.media_plan().recipe().source_index()
        {
            return Err(TapeError::Catalog("rename 追加前的原卷来源已改变".into()));
        }
        if !head.online || head.blacklisted {
            return Err(TapeError::MediaUnavailable {
                barcode: placement.barcode,
            });
        }
        self.guard()
    }
    /// A projection is disposable and owned, so SQLite is not borrowed across
    /// awaits. NeedsProjection performs no metadata transition or media append.
    pub async fn execute<'v>(
        &self,
        mut volume: LtfsVolume<'v>,
        attempt: &NodeRenameAttempt,
        intent: &NodeRenameVolumeIntent,
        projection: Option<NodeRenameVolumeProjection>,
    ) -> Result<NodeRenameVolumeCommit<'v>> {
        self.guard()?;
        if volume.guarded_execution()
            != Some((
                self.key.round(),
                self.permission as *const Permission as usize,
            ))
        {
            return Err(TapeError::NotReady(
                "rename 卷未使用当前多数派许可设备守护".into(),
            ));
        }
        let current = self
            .catalog
            .node_rename_attempt(self.route, self.key.round(), attempt.global_plan())
            .await
            .map_err(|e| self.error(intent, "读取原组", e))?
            .ok_or_else(|| TapeError::Catalog("rename 原组缺失".into()))?;
        if current.id() != attempt.id()
            || current.fingerprint() != attempt.fingerprint()
            || current.phase() != NodeRenameAttemptPhase::Writing
            || intent.group() != current.id()
        {
            return Err(TapeError::Catalog(
                "rename 介质执行要求同一原 Writing 组".into(),
            ));
        }
        let intents = self
            .catalog
            .node_rename_volume_intents(
                self.route,
                self.key.round(),
                &current,
                intent.preparation_page(),
            )
            .await
            .map_err(|e| self.error(intent, "核对当前逐卷意图", e))?;
        if !intents.contains(intent) {
            return Err(TapeError::Catalog("rename 原按卷意图已被推进".into()));
        }
        volume.set_reservation_guard(Some(self.key));
        let actual: ActiveIndexIdentity = volume.verified_recovery_index_construct()?.into();
        if actual.volume_uuid != intent.volume() {
            return Err(TapeError::Catalog("rename 所挂载介质与原意图卷不同".into()));
        }
        if let Some(saved) = self
            .catalog
            .node_rename_observation(self.route, self.key.round(), intent)
            .await
            .map_err(|e| self.error(intent, "读取原 S4", e))?
        {
            let clean: ActiveIndexIdentity = volume.verified_committed_index_construct()?.into();
            if clean != saved.current_index() || actual != clean {
                return Err(TapeError::Catalog(
                    "rename 已观察 S4 与实际干净介质不符".into(),
                ));
            }
            self.guard()?;
            return Ok(NodeRenameVolumeCommit {
                volume,
                outcome: NodeRenameVolumeOutcome::Observed(saved),
            });
        }
        if let Some(saved) = self
            .catalog
            .node_rename_intent_closure(self.route, self.key.round(), intent)
            .await
            .map_err(|e| self.error(intent, "读取原关闭结果", e))?
        {
            let clean: ActiveIndexIdentity = volume.verified_committed_index_construct()?.into();
            if clean != saved.current_index() || actual != clean {
                return Err(TapeError::Catalog("rename 原关闭结果与实际介质不符".into()));
            }
            self.guard()?;
            return Ok(NodeRenameVolumeCommit {
                volume,
                outcome: NodeRenameVolumeOutcome::Closed(saved),
            });
        }
        let source = intent.media_plan().recipe().source_index();
        let marker = format!(
            "1:{}",
            intent
                .media_plan()
                .fingerprint()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let own_s4 = Some(actual.generation) == source.generation.checked_add(1)
            && volume.index().root.xattrs.iter().any(|attr| {
                attr.key == crate::managed_catalog::NODE_RENAME_MARKER
                    && !attr.base64
                    && attr.value == marker
            });
        if own_s4 {
            let evidence = volume.node_rename_evidence(intent.media_plan(), self.budget)?;
            self.guard()?;
            let saved = self
                .catalog
                .observe_node_rename_intent(
                    self.route,
                    self.key.round(),
                    &current,
                    intent,
                    &evidence,
                )
                .await
                .map_err(|e| self.error(intent, "接纳原物理 S4", e))?;
            self.guard()?;
            return Ok(NodeRenameVolumeCommit {
                volume,
                outcome: NodeRenameVolumeOutcome::Observed(saved),
            });
        }
        let absence = if volume.recovery().dp.tail == TailKind::Complete {
            volume.node_rename_absence_evidence(intent.media_plan(), self.budget)?
        } else {
            self.append_policy(&current, intent).await?;
            self.guard()?;
            volume.discard_node_rename_tail(intent.media_plan(), self.budget)?
        };
        if absence.kind() == NodeRenameAbsenceKind::Unchanged
            && intent.original_owner_epoch() == self.key.round()
        {
            let Some(projection) = projection else {
                self.guard()?;
                return Ok(NodeRenameVolumeCommit {
                    volume,
                    outcome: NodeRenameVolumeOutcome::NeedsProjection,
                });
            };
            self.append_policy(&current, intent).await?;
            self.catalog
                .check_node_rename_write_intent(self.route, self.key.round(), &current, intent)
                .await
                .map_err(|e| self.error(intent, "原意图写入门控", e))?;
            self.guard()?;
            volume.stage_node_rename(intent.media_plan(), &projection, self.budget)?;
            volume.sync_batch()?;
            let evidence = volume.node_rename_evidence(intent.media_plan(), self.budget)?;
            self.guard()?;
            let saved = self
                .catalog
                .observe_node_rename_intent(
                    self.route,
                    self.key.round(),
                    &current,
                    intent,
                    &evidence,
                )
                .await
                .map_err(|e| self.error(intent, "记录新 S4", e))?;
            self.guard()?;
            return Ok(NodeRenameVolumeCommit {
                volume,
                outcome: NodeRenameVolumeOutcome::Observed(saved),
            });
        }
        self.guard()?;
        let saved = self
            .catalog
            .close_node_rename_intent(self.route, self.key.round(), &current, intent, &absence)
            .await
            .map_err(|e| self.error(intent, "封存原未发生结果", e))?;
        self.guard()?;
        Ok(NodeRenameVolumeCommit {
            volume,
            outcome: NodeRenameVolumeOutcome::Closed(saved),
        })
    }
}
