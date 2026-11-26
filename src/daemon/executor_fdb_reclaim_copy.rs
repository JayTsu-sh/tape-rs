//! Bounded parent work: original logical uploads first, then every physical
//! source node, and only then the two independent initialization gates.
use super::*;
use crate::managed_catalog::{
    ActiveFileRequest, ActiveUploadSpec, ContentDigest, ReclaimCopyGroupChoice,
    ReclaimCopyGroupLimits, ReclaimCopyGroupSelection, ReclaimFile, ReclaimMetadataWrite,
    ReclaimNodeDestination, ReclaimNodeReadRequest, ReclaimSourceInventory, ReclaimSourceNode,
    ReclaimSourceNodeKind,
};
use log::debug;
use std::time::Instant;
#[path = "executor_fdb_reclaim_group_copy.rs"]
mod grouping;

struct Copy<'a> {
    permit: &'a Arc<Permission>,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    source: &'a CapturedIndexSource,
    inventory: &'a ReclaimSourceInventory<'a>,
}
impl Copy<'_> {
    fn archive(&self) -> String {
        format!(
            "_reclaim_{}",
            self.parent.intent()["operation"]
                .as_str()
                .expect("validated operation")
        )
    }
    async fn task(
        &self,
        owner: &Active,
        uid: u64,
    ) -> std::result::Result<Option<ReclaimMetadataWrite>, ReadError> {
        let Some(task) = self
            .control
            .catalog
            .discover_reclaim_metadata_write(self.parent, owner.round, uid)
            .await
            .map_err(parent_error)?
        else {
            return Ok(None);
        };
        self.control
            .catalog
            .claim_reclaim_metadata_attempt(self.parent, owner.round, &task)
            .await
            .map(Some)
            .map_err(parent_error)
    }
    fn execute<'a>(
        &'a self,
        owner: &'a mut Active,
        task: &'a ReclaimMetadataWrite,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        metadata::step(
            owner,
            self.permit,
            self.control,
            self.parent,
            self.layout,
            self.source,
            task,
        )
    }
    fn node<'a>(
        &'a self,
        owner: &'a mut Active,
        node: &'a ReclaimSourceNode,
        path: &'a str,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            if self
                .control
                .catalog
                .reclaim_source_node_covered(self.parent, owner.round, self.inventory, node.uid())
                .await
                .map_err(parent_error)?
            {
                return Ok(true);
            }
            let mut raw_spool = None;
            let task = if let Some(task) = self.task(owner, node.uid()).await? {
                task
            } else {
                debug!(
                    "reclaim 节点 uid={} kind={:?} path={} round={} 准备新元数据任务，捕获目的带",
                    node.uid(),
                    node.kind(),
                    path,
                    owner.round
                );
                let history_context = self
                    .control
                    .catalog
                    .reclaim_node_destination_history_context(
                        self.parent,
                        owner.round,
                        self.layout,
                        node,
                    )
                    .await
                    .map_err(parent_error)?;
                let current = capture_destination_with_history_context(
                    owner,
                    self.permit,
                    self.control,
                    self.parent,
                    self.layout,
                    &history_context,
                )
                .await?;
                if node.kind() == ReclaimSourceNodeKind::File
                    || self.inventory.is_ee_alias(node).map_err(parent_error)?
                {
                    let source_drive = device(owner, &self.layout.source.drive_serial)?;
                    let storage = self.control.storage;
                    #[cfg(test)]
                    let storage = crate::daemon::executor::fdb_reclaim_spool_test_hook::budget(
                        &self.control.catalog,
                        self.parent,
                        owner.round,
                        node.uid(),
                        self.inventory,
                        storage,
                    )
                    .await;
                    let mut spool = metadata::RawBodySpool::new(
                        owner.files.spool_dir(),
                        self.inventory
                            .raw_body_length(node.uid())
                            .map_err(parent_error)?,
                        storage,
                    )?;
                    // The original read fixes its proof and the bounded anonymous
                    // bytes together; only its fresh exact Prepared task may use it.
                    let bytes = reader(owner, self.permit, self.control, source_drive)
                        .read_reclaim_raw_source_to(
                            self.parent,
                            node.uid(),
                            owner.files.spool_dir(),
                            &mut spool,
                        )
                        .await
                        .map_err(lost)?;
                    let target = device(owner, &self.layout.destination.drive_serial)?;
                    let checked = fdb_loading::operation_guard(
                        owner,
                        self.permit,
                        owner.devices[target].dev.as_ref(),
                        || {
                            futures_executor::block_on(
                                self.control
                                    .catalog
                                    .require_reclaim_destination_history_context(
                                        self.parent,
                                        owner.round,
                                        &history_context,
                                    ),
                            )
                            .map_err(|e| TapeError::Catalog(e.to_string()))
                        },
                    );
                    let label = crate::ltfs::volume::read_label_only(&checked).map_err(lost)?;
                    let plan = self
                        .inventory
                        .plan_reclaimed_raw_body(self.parent, &bytes, &current, &label, path)
                        .map_err(parent_error)?;
                    checked.verify_proof().map_err(lost)?;
                    let task = self
                        .control
                        .catalog
                        .prepare_reclaim_raw_body(self.parent, owner.round, &plan, &bytes)
                        .await
                        .map_err(parent_error)?;
                    raw_spool = spool.bind(&task, &bytes)?;
                    task
                } else {
                    let plan = self
                        .inventory
                        .plan_reclaimed_node(self.parent, node, &current, path)
                        .map_err(parent_error)?;
                    self.control
                        .catalog
                        .prepare_reclaim_metadata_node(self.parent, owner.round, &plan)
                        .await
                        .map_err(parent_error)?
                }
            };
            if !metadata::step_with_spool(
                owner,
                self.permit,
                self.control,
                self.parent,
                self.layout,
                (self.source, raw_spool),
                &task,
            )
            .await?
            {
                return Ok(false);
            }
            if task.plan().is_raw_body() {
                self.control
                    .catalog
                    .seal_reclaim_raw_file_coverage(self.parent, owner.round, node.uid(), task.id())
                    .await
                    .map_err(parent_error)?;
            } else {
                let target = device(owner, &self.layout.destination.drive_serial)?;
                let observations = reader(owner, self.permit, self.control, target)
                    .read_reclaim_node_metadata(
                        self.parent,
                        self.inventory,
                        &[ReclaimNodeReadRequest {
                            original: node.clone(),
                            destination: ReclaimNodeDestination {
                                volume: task.plan().volume(),
                                uid: task.plan().uid(),
                            },
                        }],
                        &owner.files.extent_cache_dir(),
                        owner.files.spool_dir(),
                    )
                    .await
                    .map_err(lost)?;
                self.control
                    .catalog
                    .record_reclaim_node_observations(self.parent, owner.round, &observations)
                    .await
                    .map_err(parent_error)?;
                self.control
                    .catalog
                    .seal_reclaim_node_coverage(self.parent, owner.round, &observations[0])
                    .await
                    .map_err(parent_error)?;
            }
            Ok(true)
        })
    }
    // Construct the mutually exclusive fresh-plan future on its own call
    // frame, so executing an existing task does not retain its temporaries.
    fn prepare_archive_file<'a>(
        &'a self,
        owner: &'a mut Active,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
        input: &'a crate::managed_catalog::ReclaimMetadataInput,
        uid: u64,
    ) -> std::pin::Pin<
        Box<
            impl std::future::Future<Output = std::result::Result<ReclaimMetadataWrite, ReadError>> + 'a,
        >,
    > {
        Box::pin(async move {
            debug!(
                "reclaim 文件 uid={} path={} round={} 准备新元数据任务，捕获目的带",
                uid,
                file.path(),
                owner.round
            );
            let history_context = self
                .control
                .catalog
                .reclaim_archive_destination_history_context(
                    self.parent,
                    owner.round,
                    self.layout,
                    file,
                    request,
                    input,
                )
                .await
                .map_err(parent_error)?;
            let target = capture_destination_with_history_context(
                owner,
                self.permit,
                self.control,
                self.parent,
                self.layout,
                &history_context,
            )
            .await?;
            let plan = self
                .inventory
                .plan_reclaimed_body(input, &target, &format!("{}/data-{uid}", self.archive()))
                .map_err(parent_error)?;
            self.control
                .catalog
                .prepare_reclaim_metadata_write(self.parent, owner.round, file, request, &plan)
                .await
                .map_err(parent_error)
        })
    }
    fn archive_file<'a>(
        &'a self,
        owner: &'a mut Active,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            let input = self
                .control
                .catalog
                .reclaim_metadata_input(self.parent, owner.round, file, request)
                .await
                .map_err(parent_error)?;
            let uid = input.source().source().copy().ltfs_file_uid;
            if self
                .control
                .catalog
                .reclaim_source_node_covered(self.parent, owner.round, self.inventory, uid)
                .await
                .map_err(parent_error)?
            {
                return Ok(true);
            }
            let task = if let Some(task) = self.task(owner, uid).await? {
                task
            } else {
                self.prepare_archive_file(owner, file, request, &input, uid)
                    .await?
            };
            if !self.execute(owner, &task).await? {
                return Ok(false);
            }
            self.control
                .catalog
                .seal_reclaim_raw_file_coverage(self.parent, owner.round, uid, task.id())
                .await
                .map_err(parent_error)?;
            Ok(true)
        })
    }
    fn settle_child_body<'a>(
        &'a self,
        owner: &'a mut Active,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>,
    > {
        Box::pin(async move {
            let verified = self
                .control
                .catalog
                .reclaim_file_body_verified(self.parent, owner.round, file, request)
                .await
                .map_err(parent_error)?;
            if !verified
                && self
                    .control
                    .catalog
                    .active_upload_result(request, self.control.route)
                    .await
                    .map_err(parent_error)?
                    .is_none()
            {
                let source_drive = device(owner, &self.layout.source.drive_serial)?;
                let read = reader(owner, self.permit, self.control, source_drive)
                    .read_reclaim_with_cache(
                        self.parent,
                        request,
                        &owner.files.extent_cache_dir(),
                        owner.files.spool_dir(),
                    )
                    .await
                    .map_err(lost)?
                    .ok_or_else(|| lost("原子请求源正文缺失"))?;
                fdb_writing::write(
                    owner,
                    self.permit,
                    self.control,
                    self.status,
                    request.clone(),
                    read.file,
                )
                .await?;
            }
            let target = device(owner, &self.layout.destination.drive_serial)?;
            let (read, proof) = reader(owner, self.permit, self.control, target)
                .read_reclaim_metadata_with_cache(
                    self.parent,
                    request,
                    self.inventory,
                    &owner.files.extent_cache_dir(),
                    owner.files.spool_dir(),
                )
                .await
                .map_err(lost)?;
            if !verified {
                self.control
                    .catalog
                    .verify_reclaim_file_metadata(
                        self.parent,
                        owner.round,
                        file,
                        request,
                        &read.metadata,
                        read.verified_head,
                    )
                    .await
                    .map_err(parent_error)?;
            }
            self.control
                .catalog
                .retire_reclaim_file_source(self.parent, owner.round, file, request)
                .await
                .map_err(parent_error)?;
            self.control
                .catalog
                .seal_reclaim_file_metadata(self.parent, owner.round, file, request, &proof)
                .await
                .map_err(parent_error)?;
            Ok(())
        })
    }
    fn child<'a>(
        &'a self,
        owner: &'a mut Active,
        file: &'a ReclaimFile,
        request: &'a ActiveFileRequest,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            let sealed = self
                .control
                .catalog
                .resume_reclaim_file_metadata(self.parent, owner.round, file, request)
                .await
                .map_err(parent_error)?
                .is_some();
            if !sealed {
                self.settle_child_body(owner, file, request).await?;
            }
            let original = self
                .control
                .catalog
                .reclaim_read_source(self.parent, owner.round, request)
                .await
                .map_err(parent_error)?;
            if original
                .source()
                .copy()
                .media
                .as_ref()
                .is_some_and(|m| m.ee_alias)
            {
                self.control
                    .catalog
                    .seal_reclaim_alias_coverage(self.parent, owner.round, file, request)
                    .await
                    .map_err(parent_error)?;
            }
            self.archive_file(owner, file, request).await
        })
    }
    fn settle_existing_children<'a>(
        &'a self,
        owner: &'a mut Active,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            let mut after = None;
            loop {
                let page = self
                    .control
                    .catalog
                    .reclaim_uploads_page(self.parent, owner.round, after.as_deref(), 16)
                    .await
                    .map_err(parent_error)?;
                for request in page.items {
                    if self
                        .control
                        .catalog
                        .retry_cancelled_reclaim_upload(self.parent, owner.round, &request)
                        .await
                        .map_err(parent_error)?
                        .is_some()
                    {
                        continue;
                    }
                    let file = self
                        .control
                        .catalog
                        .reclaim_original_file(self.parent, owner.round, &request)
                        .await
                        .map_err(parent_error)?;
                    if !self.child(owner, &file, &request).await? {
                        return Ok(false);
                    }
                }
                let Some(next) = page.next else {
                    break;
                };
                after = Some(next);
            }
            Ok(true)
        })
    }
    fn walk<'a>(
        &'a self,
        owner: &'a mut Active,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            if self.parent.intent()["version"] == 2 {
                return grouping::walk(self, owner).await;
            }
            // Settle the original children before consuming another namespace item.
            // A crash after verify advanced the walk must not hide metadata work.
            if !self.settle_existing_children(owner).await? {
                return Ok(false);
            }
            let walk = self
                .control
                .catalog
                .step_reclaim_walk(self.parent, owner.round)
                .await
                .map_err(parent_error)?;
            let Some(file) = walk.current() else {
                return Ok(walk.complete());
            };
            let full = crate::managed_catalog::pool_catalog_path(
                self.parent.spec().pool(),
                file.path().as_bytes(),
            )
            .map_err(parent_error)?;
            let Some(crate::managed_catalog::ResolvedPath::File { copy: logical, .. }) = self
                .control
                .catalog
                .resolve_active_path_for_route(&self.control.serial, &full, self.control.route)
                .await
                .map_err(parent_error)?
            else {
                return Err(lost("原扫描文件已丢失或不是文件"));
            };
            if logical.media.as_ref().is_some_and(|m| m.native_symlink) {
                let uid = logical
                    .media
                    .as_ref()
                    .expect("native link")
                    .logical_file_uid;
                let nodes = self
                    .control
                    .catalog
                    .reclaim_source_nodes(self.parent, owner.round, self.inventory, &[uid])
                    .await
                    .map_err(parent_error)?;
                if self
                    .node(owner, &nodes[0], file.path().trim_start_matches('/'))
                    .await?
                {
                    let task = self
                        .task(owner, uid)
                        .await?
                        .ok_or_else(|| lost("原链接任务缺失"))?;
                    self.control
                        .catalog
                        .relocate_reclaim_native_link(
                            self.parent,
                            owner.round,
                            file,
                            &task,
                            self.source,
                        )
                        .await
                        .map_err(parent_error)?;
                }
                return Ok(false);
            }
            let source_drive = device(owner, &self.layout.source.drive_serial)?;
            let (read, proof) = reader(owner, self.permit, self.control, source_drive)
                .read_reclaim_file_with_cache(
                    self.parent,
                    file,
                    self.inventory,
                    &owner.files.extent_cache_dir(),
                    owner.files.spool_dir(),
                )
                .await
                .map_err(lost)?;
            let spec = ActiveUploadSpec {
                pool_id: self.parent.spec().pool(),
                path: file.path().as_bytes().to_vec(),
                operation_id: file.operation(),
                expected_version: Some(file.version()),
                size: file.size(),
                digest: ContentDigest::Sha256(proof.sha256()),
            };
            let intent = self
                .control
                .catalog
                .prepare_reclaim_upload(
                    self.parent,
                    owner.round,
                    &self.layout.destination.barcode,
                    &spec,
                    &proof,
                )
                .await
                .map_err(parent_error)?;
            // The sealed request owns the attempt; this first spool is passed
            // directly rather than making a second source read before writing.
            fdb_writing::write(
                owner,
                self.permit,
                self.control,
                self.status,
                intent.request(),
                read.file,
            )
            .await?;
            self.child(owner, file, &intent.request()).await?;
            Ok(false)
        })
    }
}

// Keep the phase's recovery/write futures out of the parent poll frame.
#[allow(clippy::too_many_arguments)] // Explicit original scope and optional format evidence.
pub(super) fn step<'a>(
    owner: &'a mut Active,
    permit: &'a Arc<Permission>,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    captured: (&'a CapturedIndexSource, &'a ReclaimSourceInventory<'a>),
    prepared_format_source: Option<Box<PreparedReclaimFormatSource>>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>>
{
    Box::pin(async move {
        control
            .catalog
            .require_reclaim_group_only_ready(
                parent,
                owner.round,
                control.storage.download_bytes,
                control.storage.index_bytes,
            )
            .await
            .map_err(parent_error)?;
        // Q/I belong to the original physical request, possibly under the previous
        // owner. Reconcile them before creating metadata or entering a fresh writer.
        if parent.intent()["version"] == 1
            && upload_recovery::step(owner, permit, control, parent, layout).await?
        {
            return Ok(());
        }
        let context = Copy {
            permit,
            control,
            status,
            parent,
            layout,
            source: captured.0,
            inventory: captured.1,
        };
        if let Some(task) = control
            .catalog
            .reclaim_metadata_checkpoint(parent, owner.round)
            .await
            .map_err(parent_error)?
            && !context.execute(owner, &task).await?
        {
            return Ok(());
        }
        if !parent.groups_archive_root() {
            let root = control
                .catalog
                .reclaim_source_nodes(
                    parent,
                    owner.round,
                    context.inventory,
                    &[context.source.header().root.meta.file_uid],
                )
                .await
                .map_err(parent_error)?;
            if !context.node(owner, &root[0], &context.archive()).await? {
                return Ok(());
            }
        }
        let just_closed = !context.walk(owner).await?;
        if just_closed {
            if !parent.spec().group_only()
                || !parent.groups_archive_root()
                || context.inventory.objects() > 64
            {
                return Ok(());
            }
            // Only the fresh readback/settlement path can place this proof
            // in Active during this call, and only after durable Close.
            let Some(prepared) = &owner.fdb_reclaim_format_source else {
                return Ok(());
            };
            let source_drive = device(owner, &layout.source.drive_serial)?;
            if !prepared.matches_loaded_source(owner, parent, layout, source_drive) {
                return Err(lost("本轮关闭组的源格式化候选与设备身份不同"));
            }
            prepared
                .proof
                .require_capture(context.source)
                .map_err(parent_error)?;
            let limits = ReclaimCopyGroupLimits::bounded(32, control.storage.download_bytes)
                .map_err(parent_error)?;
            let selection_started = Instant::now();
            let selection = control
                .catalog
                .select_reclaim_copy_group(
                    parent,
                    owner.round,
                    limits,
                    ReclaimCopyGroupChoice::Advance,
                )
                .await
                .map_err(parent_error)?;
            debug!(
                "回收关闭组后选择耗时: 操作={} 耗时={:?}",
                parent.intent()["operation"],
                selection_started.elapsed(),
            );
            if !matches!(selection, ReclaimCopyGroupSelection::Done) {
                return Ok(());
            }
        }
        let coverage_started = Instant::now();
        let mut coverage_pages = 0;
        let mut coverage_complete = false;
        // Each existing transaction checks up to sixteen objects. After a
        // fresh Close, finish at most four such pages and do no physical work.
        for _ in 0..if just_closed { 4 } else { 1 } {
            let work = control
                .catalog
                .reclaim_uncovered_source_nodes(parent, owner.round, context.inventory)
                .await
                .map_err(parent_error)?;
            if let Some(node) = work.first() {
                if !just_closed {
                    context
                        .node(
                            owner,
                            node,
                            &format!("{}/node-{}", context.archive(), node.uid()),
                        )
                        .await?;
                }
                return Ok(());
            }
            let progress = control
                .catalog
                .step_reclaim_source_coverage(parent, owner.round, context.inventory)
                .await
                .map_err(parent_error)?;
            coverage_pages += 1;
            if progress.complete() {
                coverage_complete = true;
                break;
            }
        }
        debug!(
            "回收源覆盖审计耗时: 操作={} 页数={} 完成={} 耗时={:?}",
            parent.intent()["operation"],
            coverage_pages,
            coverage_complete,
            coverage_started.elapsed(),
        );
        if coverage_complete {
            let references_started = Instant::now();
            let retained_references = control
                .catalog
                .reclaim_source_has_retained_references(parent, owner.round)
                .await
                .map_err(parent_error)?;
            debug!(
                "回收源保留引用检查耗时: 操作={} 仍有引用={} 耗时={:?}",
                parent.intent()["operation"],
                retained_references,
                references_started.elapsed(),
            );
            if retained_references {
                control.catalog.set_reclaim_dependency(parent, owner.round, Some("源卷仍有历史版本、扫描或其他命名空间引用，等待按原保留策略合法释放；不会清除 V 或初始化介质")).await.map_err(parent_error)?;
                return Ok(());
            }
            control
                .catalog
                .set_reclaim_dependency(parent, owner.round, None)
                .await
                .map_err(parent_error)?;
            // Small histories complete in one invocation without recapturing
            // the source after each metadata-only audit page.
            let history_started = Instant::now();
            let mut history_pages = 0;
            let mut audited = false;
            for _ in 0..32 {
                let complete = control
                    .catalog
                    .step_reclaim_group_history_audit(parent, owner.round)
                    .await
                    .map_err(parent_error)?;
                history_pages += 1;
                if complete {
                    audited = true;
                    break;
                }
            }
            debug!(
                "回收组历史审计耗时: 操作={} 页数={} 完成={} 耗时={:?}",
                parent.intent()["operation"],
                history_pages,
                audited,
                history_started.elapsed(),
            );
            if !audited {
                return Ok(());
            }
            let source_drive = device(owner, &layout.source.drive_serial)?;
            let prepared_format_source = if just_closed {
                Some(
                    owner
                        .fdb_reclaim_format_source
                        .take()
                        .ok_or_else(|| lost("本轮关闭组的源格式化候选丢失"))?,
                )
            } else {
                prepared_format_source
            };
            let proof = if let Some(prepared) = prepared_format_source {
                if !just_closed {
                    prepared
                        .proof
                        .require_capture(context.source)
                        .map_err(parent_error)?;
                }
                #[cfg(test)]
                checkpoint::record_prepared_source_used(parent);
                log::info!(
                    "回收组使用并行读取的源格式化来源: 操作={} 驱动器={}",
                    parent.intent()["operation"],
                    layout.source.drive_serial,
                );
                prepared.proof
            } else {
                reader(owner, permit, control, source_drive)
                    .read_reclaim_format_source(parent, owner.files.spool_dir())
                    .await
                    .map_err(|error| lost(format!("format_source_read: {error}")))?
            };
            let plan_started = Instant::now();
            control
                .catalog
                .prepare_reclaim_format(parent, owner.round, uuid::Uuid::new_v4(), &proof)
                .await
                .map_err(|error| match parent_error(error) {
                    ReadError::Busy(reason) => ReadError::Busy(format!("prepare_format: {reason}")),
                    ReadError::Lost(reason) => ReadError::Lost(format!("prepare_format: {reason}")),
                    other => other,
                })?;
            debug!(
                "回收源初始化计划保存耗时: 操作={} 耗时={:?}",
                parent.intent()["operation"],
                plan_started.elapsed(),
            );
            if just_closed {
                log::info!(
                    "回收组结清后保存同轮源格式化计划: 操作={} 驱动器={} 对象={}",
                    parent.intent()["operation"],
                    layout.source.drive_serial,
                    context.inventory.objects(),
                );
            }
        }
        Ok(())
    })
}
