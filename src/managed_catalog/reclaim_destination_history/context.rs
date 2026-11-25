//! Only catalog factories construct these readonly, operation-bound contexts.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Origin {
    Node(ReclaimSourceNode),
    Archive {
        file: ReclaimFile,
        request: ActiveFileRequest,
        input: ReclaimMetadataInput,
    },
    Attempt(ReclaimMetadataWrite),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DestinationHistoryContext {
    parent: String,
    owner: u64,
    layout: ReclaimLayout,
    origin: Origin,
    scan: Uuid,
    scan_generation: u64,
    barcode: String,
    assignment_revision: u64,
    head: ActiveIndexIdentity,
    count: u64,
    source: [u8; 32],
}
impl DestinationHistoryContext {
    pub(crate) fn expected_head(&self) -> CommittedIndexConstruct {
        committed(self.head)
    }
    pub(super) fn require_capture(&self, captured: &CapturedIndexSource) -> Result<()> {
        let Some(directory) = captured.full_history_hint() else {
            return Err(invalid());
        };
        if captured.construct().volume_uuid != self.head.volume_uuid
            || directory.manifest().head != captured.construct()
            || directory.manifest().dp_eod != captured.construct_end()
            || !matches!(
                captured.scope(),
                crate::ltfs::recovery::IndexSourceScope::Current {
                    history_truncated: false,
                    ..
                }
            )
        {
            return Err(CatalogError::Invalid("目的历史提示缺少本轮真实完整捕获"));
        }
        if !matches!(self.origin, Origin::Attempt(_))
            && captured.construct() != self.expected_head()
        {
            return Err(CatalogError::Invalid("规划目的实际卷头与原FDB上下文不同"));
        }
        if let Origin::Attempt(task) = &self.origin {
            let actual = captured.construct();
            let expected = self.expected_head();
            if actual != expected {
                let previous = task.plan().previous_source()?.head();
                if previous != expected
                    || actual.generation < expected.generation
                    || actual.location.start_block <= expected.location.start_block
                    || (if expected.incremental {
                        captured.header().previous_incremental_location
                    } else {
                        captured.header().previous_location
                    }) != Some(expected.location)
                {
                    return Err(CatalogError::Invalid(
                        "目的提示的实际后继不属于原尝试目标卷头",
                    ));
                }
            }
        }
        // An existing original attempt may have reached physical S4 while
        // FDB still names its predecessor. Its business observation and write
        // admission remain independent; this merely stores actual read order.
        Ok(())
    }
    pub(super) fn prefix(&self, reservation: &ReclaimReservation) -> Vec<u8> {
        let mut key = parent_prefix(reservation);
        key.extend_from_slice(b"s\x02"); // Explicit Destination role, no Source alias.
        key.extend_from_slice(self.head.volume_uuid.as_bytes());
        key.extend_from_slice(self.scan.as_bytes());
        key
    }
    pub(super) async fn require_at(
        &self,
        tr: &Transaction,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<()> {
        if self.owner != owner || self.parent != reservation.fingerprint() {
            return Err(CatalogError::OwnerFenced);
        }
        let current = capture_at(tr, reservation, owner, &self.layout, self.origin.clone()).await?;
        if current != *self {
            return Err(CatalogError::Invalid(
                "目的只读规划或原尝试上下文在使用期间改变",
            ));
        }
        Ok(())
    }
}
fn capture_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
    owner: u64,
    expected_layout: &'a ReclaimLayout,
    origin: Origin,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<DestinationHistoryContext>> + 'a>> {
    Box::pin(async move {
        let parent = record_at(tr, reservation, owner).await?;
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        if !matches!(parent["phase"].as_str(), Some("reserved" | "copying")) {
            return Err(CatalogError::Invalid("当前父阶段不能捕获迁移目的索引"));
        }
        source_state_at(tr, reservation).await?;
        mechanics::require_parent_idle_at(tr, reservation).await?;
        if tr
            .get(
                &native_device::library_gate(reservation.namespace(), reservation.serial()),
                false,
            )
            .await?
            .is_some()
            || automatic_media::pending_at(tr, reservation.namespace(), reservation.serial())
                .await?
                .is_some()
        {
            return Err(CatalogError::PoolBusy);
        }
        let layout = layout::at(tr, reservation)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if &layout != expected_layout {
            return Err(CatalogError::Invalid("目的捕获缺少精确的原持久布局"));
        }
        let (volume, expected_scan, attempt_stamp) =
            origin_at(tr, reservation, owner, &layout, &origin).await?;
        // Existing persisted attempts can bind a replan target outside the initial
        // destination. They never fall back to the initial Planning context.
        let placement = verified_placement(tr, volume)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let assignment =
            ManagedCatalog::verified_assignment(tr, reservation.serial(), &placement.barcode)
                .await?
                .ok_or(CatalogError::Incomplete)?;
        let (scan, head) = active_volume::observed_at(tr, reservation.serial(), volume).await?;
        let count = active_volume::file_count_at(tr, reservation.serial(), volume).await?;
        if placement.namespace != reservation.namespace()
            || placement.library_serial != reservation.serial()
            || placement.pool_id != reservation.spec().pool()
            || assignment.namespace != reservation.namespace()
            || assignment.pool_id != reservation.spec().pool()
            || assignment.volume_id != Some(volume)
            || !assignment.allows_io()
            || volume == reservation.spec().volume()
            || expected_scan.is_some_and(|expected| expected != scan.id)
        {
            return Err(CatalogError::MediaUnavailable);
        }
        if matches!(origin, Origin::Node(_) | Origin::Archive { .. }) {
            if placement.barcode != layout.destination.barcode
                || assignment.revision != layout.destination.revision
            {
                return Err(CatalogError::Invalid("规划目的分配修订或条码已改变"));
            }
            require_unsettled_free_at(tr, reservation, head).await?;
        }
        let inventory = source::history_inventory_at(tr, reservation).await?;
        let source = Sha256::new()
            .chain_update(inventory)
            .chain_update(attempt_stamp)
            .finalize()
            .into();
        Ok(DestinationHistoryContext {
            parent: reservation.fingerprint(),
            owner,
            layout,
            origin,
            scan: scan.id,
            scan_generation: scan.generation,
            barcode: placement.barcode,
            assignment_revision: assignment.revision,
            head,
            count,
            source,
        })
    })
}
type OriginEvidence = (Uuid, Option<Uuid>, [u8; 32]);
type OriginFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<OriginEvidence>> + Send + 'a>>;

// Dispatch before constructing a future. Each mutually exclusive validation
// retains its complete original body and borrows the same transaction.
fn origin_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
    owner: u64,
    layout: &'a ReclaimLayout,
    origin: &'a Origin,
) -> OriginFuture<'a> {
    match origin {
        Origin::Node(node) => node_origin_at(tr, reservation, owner, layout, node),
        Origin::Archive {
            file,
            request,
            input,
        } => archive_origin_at(tr, reservation, owner, layout, file, request, input),
        Origin::Attempt(task) => attempt_origin_at(tr, reservation, owner, task),
    }
}
fn node_origin_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
    owner: u64,
    layout: &'a ReclaimLayout,
    node: &'a ReclaimSourceNode,
) -> OriginFuture<'a> {
    Box::pin(async move {
        source::require_planning_uid_at(tr, reservation, owner, node).await?;
        Ok((layout.destination.volume, None, [0; 32]))
    })
}
fn archive_origin_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
    owner: u64,
    layout: &'a ReclaimLayout,
    file: &'a ReclaimFile,
    request: &'a ActiveFileRequest,
    input: &'a ReclaimMetadataInput,
) -> OriginFuture<'a> {
    Box::pin(async move {
        let uid = input.source().byte_proof().source_node().uid();
        source::require_planning_uid_at(
            tr,
            reservation,
            owner,
            &input.source().byte_proof().source_node(),
        )
        .await?;
        source::require_planning_archive_at(tr, reservation, owner, file, request, input).await?;
        if uid == 0 {
            return Err(invalid());
        }
        Ok((layout.destination.volume, None, [0; 32]))
    })
}
fn attempt_origin_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
    owner: u64,
    task: &'a ReclaimMetadataWrite,
) -> OriginFuture<'a> {
    Box::pin(
        async move { Ok(source::require_history_attempt_at(tr, reservation, owner, task).await?) },
    )
}

async fn require_unsettled_free_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    head: ActiveIndexIdentity,
) -> Result<()> {
    let volume = head.volume_uuid;
    native_device::require_volume_quiescent_at(tr, reservation.namespace(), volume).await?;
    let next = head.generation.checked_add(1).ok_or(invalid())?;
    if active_volume::recovery_active(tr, volume).await?
        || tr
            .get(&control::node_attempts::volume_key(volume), false)
            .await?
            .is_some()
        || requests::prefix_has_rows(tr, &reservation::volume_prefix(volume, next)).await?
        || requests::prefix_has_rows(tr, &reservation::upgraded_prefix(volume, next)).await?
        || tr
            .get(&active_batch::batch_key(volume, next), false)
            .await?
            .is_some()
        || tr
            .get(&active_batch::prepared_batch_key(volume, next), false)
            .await?
            .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    // A parent's completed body child is legal here. The original ledger and
    // input checks above bind it; no check assumes the parent has no children.
    let _ = reservation;
    Ok(())
}
impl ManagedCatalog {
    pub(crate) async fn reclaim_node_destination_history_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        layout: &ReclaimLayout,
        node: &ReclaimSourceNode,
    ) -> Result<DestinationHistoryContext> {
        let tr = self.db.create_trx()?;
        capture_at(&tr, reservation, owner, layout, Origin::Node(node.clone())).await
    }
    pub(crate) async fn reclaim_archive_destination_history_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        layout: &ReclaimLayout,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
        input: &ReclaimMetadataInput,
    ) -> Result<DestinationHistoryContext> {
        let tr = self.db.create_trx()?;
        capture_at(
            &tr,
            reservation,
            owner,
            layout,
            Origin::Archive {
                file: file.clone(),
                request: request.clone(),
                input: input.clone(),
            },
        )
        .await
    }
    pub(crate) async fn reclaim_attempt_destination_history_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        attempt: &ReclaimMetadataWrite,
    ) -> Result<DestinationHistoryContext> {
        let tr = self.db.create_trx()?;
        let layout = layout::at(&tr, reservation)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        capture_at(
            &tr,
            reservation,
            owner,
            &layout,
            Origin::Attempt(attempt.clone()),
        )
        .await
    }
    pub(crate) async fn require_reclaim_destination_history_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        context.require_at(&tr, reservation, owner).await
    }
}
