//! Bounded parent work: original logical uploads first, then every physical
//! source node, and only then the two independent initialization gates.
use super::*;
use crate::managed_catalog::{
    ActiveFileRequest, ActiveUploadSpec, ContentDigest, ReclaimFile, ReclaimMetadataWrite,
    ReclaimNodeDestination, ReclaimNodeReadRequest, ReclaimSourceInventory, ReclaimSourceNode,
    ReclaimSourceNodeKind,
};

struct Copy<'a> {
    permit: &'a Permission,
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
    async fn node(
        &self,
        owner: &mut Active,
        node: &ReclaimSourceNode,
        path: &str,
    ) -> std::result::Result<bool, ReadError> {
        if self
            .control
            .catalog
            .reclaim_source_node_covered(self.parent, owner.round, self.inventory, node.uid())
            .await
            .map_err(parent_error)?
        {
            return Ok(true);
        }
        let task = if let Some(task) = self.task(owner, node.uid()).await? {
            task
        } else {
            let current =
                capture_destination(owner, self.permit, self.control, self.parent, self.layout)?;
            if node.kind() == ReclaimSourceNodeKind::File
                || self.inventory.is_ee_alias(node).map_err(parent_error)?
            {
                let source_drive = device(owner, &self.layout.source.drive_serial)?;
                let mut discard = std::io::sink();
                // This first uncached read fixes SHA/MD5 before admission. The
                // actual first write independently streams and verifies again.
                let bytes = reader(owner, self.permit, self.control, source_drive)
                    .read_reclaim_raw_source_to(
                        self.parent,
                        node.uid(),
                        owner.files.spool_dir(),
                        &mut discard,
                    )
                    .await
                    .map_err(lost)?;
                let target = device(owner, &self.layout.destination.drive_serial)?;
                let checked = fdb_loading::operation_guard(
                    owner,
                    self.permit,
                    owner.devices[target].dev.as_ref(),
                    || {
                        futures_executor::block_on(self.control.catalog.require_reclaim_read_lane(
                            self.parent,
                            owner.round,
                            self.layout.destination.volume,
                        ))
                        .map_err(|e| TapeError::Catalog(e.to_string()))
                    },
                );
                let label = crate::ltfs::volume::read_label_only(&checked).map_err(lost)?;
                let plan = self
                    .inventory
                    .plan_reclaimed_raw_body(self.parent, &bytes, &current, &label, path)
                    .map_err(parent_error)?;
                checked.verify_proof().map_err(lost)?;
                self.control
                    .catalog
                    .prepare_reclaim_raw_body(self.parent, owner.round, &plan, &bytes)
                    .await
                    .map_err(parent_error)?
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
        if !self.execute(owner, &task).await? {
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
    }
    async fn archive_file(
        &self,
        owner: &mut Active,
        file: &ReclaimFile,
        request: &ActiveFileRequest,
    ) -> std::result::Result<bool, ReadError> {
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
            let target =
                capture_destination(owner, self.permit, self.control, self.parent, self.layout)?;
            let plan = self
                .inventory
                .plan_reclaimed_body(&input, &target, &format!("{}/data-{uid}", self.archive()))
                .map_err(parent_error)?;
            self.control
                .catalog
                .prepare_reclaim_metadata_write(self.parent, owner.round, file, request, &plan)
                .await
                .map_err(parent_error)?
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
                        .verify_reclaim_file(
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
    fn walk<'a>(
        &'a self,
        owner: &'a mut Active,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = std::result::Result<bool, ReadError>> + 'a>,
    > {
        Box::pin(async move {
            // Settle the original children before consuming another namespace item.
            // A crash after verify advanced the walk must not hide metadata work.
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
pub(super) fn step<'a>(
    owner: &'a mut Active,
    permit: &'a Permission,
    control: &'a FdbOwnerControl,
    status: &'a SharedStatus,
    parent: &'a ReclaimReservation,
    layout: &'a ReclaimLayout,
    captured: (&'a CapturedIndexSource, &'a ReclaimSourceInventory<'a>),
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>>
{
    Box::pin(async move {
        // Q/I belong to the original physical request, possibly under the previous
        // owner. Reconcile them before creating metadata or entering a fresh writer.
        if upload_recovery::step(owner, permit, control, parent, layout).await? {
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
        if !context.walk(owner).await? {
            return Ok(());
        }
        let work = control
            .catalog
            .reclaim_uncovered_source_nodes(parent, owner.round, context.inventory)
            .await
            .map_err(parent_error)?;
        if let Some(node) = work.first() {
            context
                .node(
                    owner,
                    node,
                    &format!("{}/node-{}", context.archive(), node.uid()),
                )
                .await?;
            return Ok(());
        }
        if control
            .catalog
            .step_reclaim_source_coverage(parent, owner.round, context.inventory)
            .await
            .map_err(parent_error)?
            .complete()
        {
            if control
                .catalog
                .reclaim_source_has_retained_references(parent, owner.round)
                .await
                .map_err(parent_error)?
            {
                control.catalog.set_reclaim_dependency(parent, owner.round, Some("源卷仍有历史版本、扫描或其他命名空间引用，等待按原保留策略合法释放；不会清除 V 或初始化介质")).await.map_err(parent_error)?;
                return Ok(());
            }
            control
                .catalog
                .set_reclaim_dependency(parent, owner.round, None)
                .await
                .map_err(parent_error)?;
            let source_drive = device(owner, &layout.source.drive_serial)?;
            let proof = reader(owner, permit, control, source_drive)
                .read_reclaim_format_source(parent, owner.files.spool_dir())
                .await
                .map_err(lost)?;
            control
                .catalog
                .prepare_reclaim_format(parent, owner.round, uuid::Uuid::new_v4(), &proof)
                .await
                .map_err(parent_error)?;
        }
        Ok(())
    })
}
