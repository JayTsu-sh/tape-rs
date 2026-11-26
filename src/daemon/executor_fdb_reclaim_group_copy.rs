//! Bounded physical-order scheduling over original per-file requests.
//! Fresh admission and its S4 stay in one known invocation, never across ticks.
use super::*;
use crate::daemon::fdb_media::ActiveMediaUpload;
use crate::managed_catalog::{
    ReclaimCopyGroup, ReclaimCopyGroupChoice, ReclaimCopyGroupLimits, ReclaimCopyGroupSelection,
};
use std::os::fd::AsRawFd;

fn stage_error(stage: &str, error: crate::managed_catalog::CatalogError) -> ReadError {
    match parent_error(error) {
        ReadError::Busy(reason) => ReadError::Busy(format!("回收组 {stage}: {reason}")),
        ReadError::Lost(reason) => ReadError::Lost(format!("回收组 {stage}: {reason}")),
        other => other,
    }
}

enum GroupSelection {
    Pending(bool),
    Selected(Box<ReclaimCopyGroup>),
    NeedsFullBudget,
}

async fn select_group(
    context: &Copy<'_>,
    owner: &Active,
    limits: ReclaimCopyGroupLimits,
    index_bytes: u64,
) -> std::result::Result<GroupSelection, ReadError> {
    for _ in 0..128 {
        let selected = context
            .control
            .catalog
            .select_reclaim_copy_group(
                context.parent,
                owner.round,
                limits,
                ReclaimCopyGroupChoice::Advance,
            )
            .await
            .map_err(parent_error)?;
        match selected {
            ReclaimCopyGroupSelection::Scanning => continue,
            ReclaimCopyGroupSelection::Done => return Ok(GroupSelection::Pending(true)),
            ReclaimCopyGroupSelection::Single(_) => {
                return Err(lost(
                    "原v2选择已固定为unsupported single；保留原请求，禁止降级或换新请求重写",
                ));
            }
            ReclaimCopyGroupSelection::NeedsCandidate(file) => {
                let full = crate::managed_catalog::pool_catalog_path(
                    context.parent.spec().pool(),
                    file.path().as_bytes(),
                )
                .map_err(parent_error)?;
                let selected = context
                    .control
                    .catalog
                    .resolve_reclaim_file_source(context.parent, owner.round, &full)
                    .await
                    .map_err(parent_error)?
                    .ok_or_else(|| lost("原组候选源文件已丢失"))?;
                let metadata = selected.metadata();
                let Some(binding) = selected.layout_binding().filter(|_| {
                    metadata.copy.media.is_none()
                        && matches!(metadata.copy.content_digest, ContentDigest::Sha256(_))
                }) else {
                    if context.parent.spec().group_only() {
                        return Err(lost(
                            "group-only原Ready正文能力改变，保留原计划且禁止Single回退",
                        ));
                    }
                    context
                        .control
                        .catalog
                        .select_reclaim_copy_group(
                            context.parent,
                            owner.round,
                            limits,
                            ReclaimCopyGroupChoice::Single(&file),
                        )
                        .await
                        .map_err(parent_error)?;
                    return Err(lost(
                        "原v2源缺普通managed已知摘要/FDB布局能力，保留明确single选择",
                    ));
                };
                let temporary = crate::local_read_storage::download_file(
                    owner.files.spool_dir(),
                    binding.encoded_bytes(),
                    crate::local_read_storage::ReadStorageBudget {
                        download_bytes: index_bytes,
                        ..context.control.storage
                    },
                );
                let physical = match temporary {
                    Ok(temporary) => {
                        selected
                            .prepare_body_plan(&context.control.catalog, temporary, index_bytes)
                            .await
                    }
                    Err(error) => Err(error),
                };
                let physical = match physical {
                    Err(TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. })
                        if index_bytes < context.control.storage.index_bytes =>
                    {
                        return Ok(GroupSelection::NeedsFullBudget);
                    }
                    result => result.map_err(lost)?,
                };
                let candidate = context
                    .inventory
                    .reclaim_group_candidate(context.parent, &file, &metadata, &physical)
                    .map_err(parent_error)?;
                let choice = match candidate.as_ref() {
                    Some(candidate) => ReclaimCopyGroupChoice::Candidate(candidate),
                    None if context.parent.spec().group_only() => {
                        return Err(lost(
                            "group-only原Ready extent能力改变，保留原计划且禁止Single回退",
                        ));
                    }
                    None => ReclaimCopyGroupChoice::Single(&file),
                };
                match context
                    .control
                    .catalog
                    .select_reclaim_copy_group(context.parent, owner.round, limits, choice)
                    .await
                    .map_err(parent_error)?
                {
                    ReclaimCopyGroupSelection::Group(found)
                        if found.source_ready() || found.requires_observation() =>
                    {
                        return Ok(GroupSelection::Selected(Box::new(found)));
                    }
                    ReclaimCopyGroupSelection::Single(_) => {
                        return Err(lost(
                            "原v2源不具备普通managed/FDB extent能力，禁止普通writer回退",
                        ));
                    }
                    _ => continue,
                }
            }
            ReclaimCopyGroupSelection::Group(found) => {
                if found.source_ready() || found.requires_observation() || found.prepared() {
                    return Ok(GroupSelection::Selected(Box::new(found)));
                }
                continue;
            }
        }
    }
    Ok(GroupSelection::Pending(false))
}

fn require_group_body_budget(
    context: &Copy<'_>,
    group: &ReclaimCopyGroup,
) -> std::result::Result<(), ReadError> {
    let bytes = group
        .files()
        .try_fold(0u64, |sum, file| sum.checked_add(file.size()))
        .ok_or_else(|| lost("原组源正文总预算溢出"))?;
    if bytes > context.control.storage.download_bytes {
        return Err(ReadError::Busy(
            "当前节点暂存预算容纳不下原组；保留原组等待足够资源".into(),
        ));
    }
    Ok(())
}

pub(super) fn walk<'a>(
    context: &'a Copy<'a>,
    owner: &'a mut Active,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>>
{
    Box::pin(async move {
        let limits = ReclaimCopyGroupLimits::bounded(32, context.control.storage.download_bytes)
            .map_err(parent_error)?;
        let source_drive = device(owner, &context.layout.source.drive_serial)?;
        let parallel_index_bytes = context
            .control
            .storage
            .index_bytes
            .saturating_sub(context.source.scratch_reserved_bytes())
            / 2;
        let allow_parallel = context.parent.spec().group_only()
            && parallel_index_bytes >= CapturedIndexSource::RECLAIM_PREFLIGHT_SCRATCH_MIN_BYTES
            && parallel_index_bytes >= context.source.source_bytes()
            && !context.source.construct().incremental
            && context
                .permit
                .allows(owner.round, crate::daemon::permission::Phase::Serving)
            && {
                let destination = device(owner, &context.layout.destination.drive_serial)?;
                if destination == source_drive {
                    return Err(lost("回收组并行读取必须使用两个固定驱动器"));
                }
                let execution = Some((owner.round, Arc::as_ptr(context.permit) as usize));
                owner.devices[destination].dev.session_epoch().is_some()
                    && owner.devices[destination].dev.execution_guard() == execution
                    && owner.devices[source_drive].dev.execution_guard() == execution
            };
        let early = if allow_parallel {
            context
                .control
                .catalog
                .check_early_reclaim_destination_preflight(
                    context.parent,
                    owner.round,
                    context.layout,
                )
                .await
                .map_err(parent_error)?
        } else {
            None
        };
        let (group, mut target_preflight, mut parallel_reads) = if let Some(plan) = early {
            let mut worker = checkpoint::start_target_preflight(
                owner,
                context.permit,
                context.control,
                context.parent,
                checkpoint::TargetPreflightScope::Selecting(plan),
                context.layout,
                parallel_index_bytes,
            )?;
            match select_group(context, worker.owner(), limits, parallel_index_bytes).await? {
                GroupSelection::Pending(done) => {
                    if done {
                        drop(worker.finish()?);
                    }
                    return Ok(done);
                }
                GroupSelection::NeedsFullBudget => {
                    // Release scratch before resuming the same durable selector
                    // at its original full budget. No child has been prepared.
                    drop(worker.finish()?);
                    match select_group(context, owner, limits, context.control.storage.index_bytes)
                        .await?
                    {
                        GroupSelection::Selected(group) => (*group, None, None),
                        GroupSelection::Pending(done) => return Ok(done),
                        GroupSelection::NeedsFullBudget => unreachable!("full selection budget"),
                    }
                }
                GroupSelection::Selected(group) => {
                    let group = *group;
                    if !group.source_ready() || group.id() != plan.0 {
                        return Err(lost("回收预检选择期间原完整未准入组改变"));
                    }
                    require_group_body_budget(context, &group)?;
                    let source_owner = worker.owner();
                    let mut source =
                        reader(source_owner, context.permit, context.control, source_drive);
                    source.storage.index_bytes = parallel_index_bytes;
                    let reads = source
                        .read_reclaim_source_group(
                            context.parent,
                            &group,
                            context.layout,
                            context.inventory,
                            &source_owner.files.extent_cache_dir(),
                            source_owner.files.spool_dir(),
                        )
                        .await;
                    let target = worker.finish()?;
                    let (target, reads) = match reads {
                        Ok(Some(reads)) => (target, Some(Some(reads))),
                        Ok(None) => (None, Some(None)),
                        Err(TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. }) => {
                            (None, None)
                        }
                        Err(error) => return Err(lost(error)),
                    };
                    if let Some(target) = &target {
                        target
                            .confirm_selection(
                                owner,
                                context.permit,
                                context.control,
                                context.parent,
                                &group,
                                context.layout,
                            )
                            .await?;
                    }
                    (group, target, reads)
                }
            }
        } else {
            match select_group(context, owner, limits, context.control.storage.index_bytes).await? {
                GroupSelection::Selected(group) => (*group, None, None),
                GroupSelection::Pending(done) => return Ok(done),
                GroupSelection::NeedsFullBudget => unreachable!("full selection budget"),
            }
        };
        if group.requires_observation() {
            if context.parent.spec().group_only() {
                return Err(lost(
                    "原group-only已准入，须经早隔离门禁续接原目标，禁止重读source",
                ));
            }
            let completed = super::super::checkpoint::resume_original(
                owner,
                context.permit,
                context.control,
                context.parent,
                context.layout,
                group.id(),
                None,
            )
            .await?;
            settle(context, owner.round, group.id(), &completed).await?;
            return Ok(false);
        }
        let files = group.files().cloned().collect::<Vec<_>>();
        let order = group.ordered_ordinals().collect::<Vec<_>>();
        require_group_body_budget(context, &group)?;
        if early.is_none() && allow_parallel && files.len() >= 3 {
            let mut target = checkpoint::start_target_preflight(
                owner,
                context.permit,
                context.control,
                context.parent,
                checkpoint::TargetPreflightScope::Selected(&group),
                context.layout,
                parallel_index_bytes,
            )?;
            let source_owner = target.owner();
            let mut source = reader(source_owner, context.permit, context.control, source_drive);
            source.storage.index_bytes = parallel_index_bytes;
            let reads = source
                .read_reclaim_source_group(
                    context.parent,
                    &group,
                    context.layout,
                    context.inventory,
                    &source_owner.files.extent_cache_dir(),
                    source_owner.files.spool_dir(),
                )
                .await;
            parallel_reads = match reads {
                Ok(Some(reads)) => {
                    target_preflight = target.finish()?;
                    Some(Some(reads))
                }
                Ok(None) => {
                    // The original per-file fallback needs its own full storage budget.
                    drop(target.finish()?);
                    Some(None)
                }
                Err(TapeError::LocalCapacity { .. } | TapeError::LocalSpace { .. }) => {
                    // Both drives are back before going through the old path with its
                    // original budget. A real error on the peer still blocks admission.
                    drop(target.finish()?);
                    None
                }
                Err(error) => return Err(lost(error)),
            };
        }
        let mut source_reads = match parallel_reads {
            Some(reads) => reads,
            None => reader(owner, context.permit, context.control, source_drive)
                .read_reclaim_source_group(
                    context.parent,
                    &group,
                    context.layout,
                    context.inventory,
                    &owner.files.extent_cache_dir(),
                    owner.files.spool_dir(),
                )
                .await
                .map_err(lost)?,
        }
        .map(Vec::into_iter);
        let mut held = Vec::with_capacity(files.len());
        let mut uploads = Vec::with_capacity(files.len());
        for ordinal in order {
            let file = files
                .get(usize::from(ordinal))
                .ok_or_else(|| lost("原组源ordinal不完整"))?;
            let (spool, proof) = if let Some(reads) = &mut source_reads {
                let (_, spool, proof) = reads
                    .next()
                    .filter(|(read_ordinal, _, _)| *read_ordinal == ordinal)
                    .ok_or_else(|| lost("回收组顺序实读结果缺少原ordinal"))?;
                (spool, proof)
            } else {
                let (read, proof) = reader(owner, context.permit, context.control, source_drive)
                    .read_reclaim_file_with_cache(
                        context.parent,
                        file,
                        context.inventory,
                        &owner.files.extent_cache_dir(),
                        owner.files.spool_dir(),
                    )
                    .await
                    .map_err(lost)?;
                (read.file, proof)
            };
            let spec = ActiveUploadSpec {
                pool_id: context.parent.spec().pool(),
                path: file.path().as_bytes().to_vec(),
                operation_id: file.operation(),
                expected_version: Some(file.version()),
                size: file.size(),
                digest: ContentDigest::Sha256(proof.sha256()),
            };
            let intent = context
                .control
                .catalog
                .prepare_reclaim_group_member(
                    context.parent,
                    owner.round,
                    group.id(),
                    ordinal,
                    &spec,
                    &proof,
                )
                .await
                .map_err(|error| {
                    stage_error(&format!("prepare_member ordinal={ordinal}"), error)
                })?;
            uploads.push(ActiveMediaUpload {
                intent,
                spool: format!("/proc/self/fd/{}", spool.as_raw_fd()).into(),
            });
            held.push(spool);
        }
        let group = context
            .control
            .catalog
            .resume_reclaim_copy_group(context.parent, owner.round)
            .await
            .map_err(|error| stage_error("resume_prepared", error))?
            .ok_or_else(|| lost("原完整准备组丢失"))?;
        let (target, prepared_mount) = match target_preflight {
            Some(preflight) => {
                preflight
                    .bind(
                        owner,
                        context.permit,
                        context.control,
                        context.parent,
                        &group,
                    )
                    .await?
            }
            None => (
                super::super::checkpoint::capture_prepared_group(
                    owner,
                    context.permit,
                    context.control,
                    context.parent,
                    &group,
                    context.layout,
                )
                .await?,
                None,
            ),
        };
        let intent = context
            .control
            .catalog
            .prepare_reclaim_index_intent(
                context.parent,
                owner.round,
                group.id(),
                context.source,
                &target,
            )
            .await
            .map_err(|error| stage_error("prepare_native_intent", error))?;
        context
            .control
            .catalog
            .seal_reclaim_copy_group(context.parent, owner.round, group.id())
            .await
            .map_err(|error| stage_error("seal_prepared", error))?;
        let admission = context
            .control
            .catalog
            .admit_reclaim_copy_group(
                context.parent,
                owner.round,
                group.id(),
                target.construct().into(),
            )
            .await
            .map_err(|error| stage_error("admit", error))?;
        let scope = admission
            .execution_scope(context.parent, &intent)
            .map_err(parent_error)?;
        let mut completed = super::super::checkpoint::execute(
            owner,
            context.permit,
            context.control,
            context.parent,
            context.layout,
            scope,
            &uploads,
            target,
            prepared_mount,
        )
        .await?;
        settle(context, owner.round, group.id(), &completed).await?;
        owner.fdb_reclaim_format_source = completed.prepared_format_source.take();
        drop(held);
        Ok(false)
    })
}

fn settle<'a>(
    context: &'a Copy<'a>,
    owner: u64,
    group: uuid::Uuid,
    completed: &'a super::super::checkpoint::CompletedCheckpoint,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>>
{
    Box::pin(async move {
        let started = std::time::Instant::now();
        for readback in &completed.readbacks {
            let member = readback.context();
            context
                .control
                .catalog
                .settle_reclaim_group_member(
                    context.parent,
                    owner,
                    member.group_id(),
                    member.attempt_id(),
                    member.ordinal(),
                    readback,
                )
                .await
                .map_err(|error| {
                    stage_error(&format!("settle ordinal={}", member.ordinal()), error)
                })?;
        }
        let members_time = started.elapsed();
        let close_started = std::time::Instant::now();
        context
            .control
            .catalog
            .close_reclaim_copy_group(context.parent, owner, group)
            .await
            .map_err(|error| stage_error("close", error))?;
        log::info!(
            "回收组FDB逐文件结清及关闭完成: 操作={} 组={} 文件={} 结清耗时={:.3}s 关闭耗时={:.3}s",
            context.parent.intent()["operation"],
            group,
            completed.readbacks.len(),
            members_time.as_secs_f64(),
            close_started.elapsed().as_secs_f64(),
        );
        Ok(())
    })
}
