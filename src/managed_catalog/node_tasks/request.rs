//! One ledger and pool interlock for both immutable node request protocols.
use super::*;

pub(super) struct NodeTaskScope {
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool: Uuid,
    pub request_epoch: u64,
    pub operation: Uuid,
    pub kind: NodeMutationTaskKind,
}

pub(super) trait LogicalNodeRequest: Sync {
    fn task_scope(&self) -> NodeTaskScope;
    fn encode(&self) -> Result<Vec<u8>>;
    fn fingerprint(&self) -> [u8; 32];
    fn revalidate_at<'a>(
        &'a self,
        tr: &'a Transaction,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
}

impl LogicalNodeRequest for NodeMutationRequest {
    fn task_scope(&self) -> NodeTaskScope {
        let c = self.context();
        NodeTaskScope {
            library_serial: c.library_serial.clone(),
            namespace: c.namespace,
            pool: c.pool,
            request_epoch: c.request_epoch,
            operation: c.operation,
            kind: NodeMutationTaskKind::SingleNode,
        }
    }
    fn encode(&self) -> Result<Vec<u8>> {
        NodeMutationRequest::encode(self)
    }
    fn fingerprint(&self) -> [u8; 32] {
        NodeMutationRequest::fingerprint(self)
    }
    async fn revalidate_at<'a>(
        &'a self,
        tr: &'a Transaction,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<()> {
        node_request::revalidate_at(tr, route, owner, self)
            .await
            .map(|_| ())
    }
}
impl LogicalNodeRequest for NodeRenameRequest {
    fn task_scope(&self) -> NodeTaskScope {
        let spec = self.spec();
        NodeTaskScope {
            library_serial: self.library().into(),
            namespace: spec.source.namespace,
            pool: spec.pool,
            request_epoch: self.request_epoch(),
            operation: spec.operation,
            kind: NodeMutationTaskKind::Rename,
        }
    }
    fn encode(&self) -> Result<Vec<u8>> {
        NodeRenameRequest::encode(self)
    }
    fn fingerprint(&self) -> [u8; 32] {
        NodeRenameRequest::fingerprint(self)
    }
    async fn revalidate_at<'a>(
        &'a self,
        tr: &'a Transaction,
        route: ActiveLibraryRoute,
        owner: u64,
    ) -> Result<()> {
        node_rename::revalidate_at(tr, route, owner, self)
            .await
            .map(|_| ())
    }
}

impl ManagedCatalog {
    pub(super) async fn queue_logical_node_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &impl LogicalNodeRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        let bytes = request.encode()?;
        let c = request.task_scope();
        let object = NodePlanObject::for_bytes(&c.library_serial, c.namespace, &bytes, max_bytes)?;
        let preflight = self.db.create_trx()?;
        if let Some(task) = known_at(&preflight, route, owner, request, &object).await? {
            return Ok(task);
        }
        require_request_pool(&preflight, request).await?;
        drop(preflight);
        self.upload_node_plan_object(route, owner, &object, &mut bytes.as_slice())
            .await?;
        let created_at_millis = management::now();
        self.db
            .run(|tr, _| {
                let object = &object;
                let c = &c;
                async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                256_000,
                            ))?;
                            if let Some(task) = known_at(&tr, route, owner, request, object).await?
                            {
                                return Ok(task);
                            }
                            require_request_pool(&tr, request).await?;
                            node_plan_objects::require_sealed_at(&tr, object).await?;
                            let task = NodeMutationTask {
                                kind: c.kind,
                                library_serial: c.library_serial.clone(),
                                namespace: c.namespace,
                                pool: c.pool,
                                request_epoch: c.request_epoch,
                                operation: c.operation,
                                object: object.clone(),
                                fingerprint: request.fingerprint(),
                                state: NodeMutationTaskState::Queued,
                                accepted_owner_epoch: owner,
                                created_at_millis,
                                prepared_owner_epoch: None,
                                prepared_at_millis: None,
                                active_attempt: None,
                                completed_owner_epoch: None,
                                completed_at_millis: None,
                                publication_digest: None,
                                rejection_reason: None,
                            };
                            let encoded = encode(&task)?;
                            // Everything fallible precedes the first write in this transaction.
                            tr.set(
                                &identity_key(
                                    c.namespace,
                                    &c.library_serial,
                                    b'o',
                                    c.request_epoch,
                                    c.operation,
                                ),
                                &encoded,
                            );
                            tr.set(
                                &identity_key(
                                    c.namespace,
                                    &c.library_serial,
                                    b'q',
                                    c.request_epoch,
                                    c.operation,
                                ),
                                b"",
                            );
                            tr.set(
                                &global_pending_key(
                                    c.namespace,
                                    &c.library_serial,
                                    c.request_epoch,
                                    c.operation,
                                ),
                                c.library_serial.as_bytes(),
                            );
                            Ok(task)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub(super) async fn prepare_logical_node_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &impl LogicalNodeRequest,
        max_bytes: usize,
    ) -> Result<NodeMutationTask> {
        let c = request.task_scope();
        let bytes = request.encode()?;
        let object = NodePlanObject::for_bytes(&c.library_serial, c.namespace, &bytes, max_bytes)?;
        let now = management::now();
        self.db
            .run(|tr, _| {
                let object = &object;
                let c = &c;
                async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                256_000,
                            ))?;
                            let mut task = known_at(&tr, route, owner, request, object)
                                .await?
                                .ok_or(CatalogError::Invalid("节点任务尚未登记"))?;
                            if task.state != NodeMutationTaskState::Queued {
                                return Ok(task);
                            }
                            require_pool_writable_at(&tr, task.namespace, task.pool).await?;
                            if requests::has_file_holds(&tr, task.namespace).await? {
                                return Err(CatalogError::Incomplete);
                            }
                            node_plan_objects::require_sealed_at(&tr, object).await?;
                            request.revalidate_at(&tr, route, owner).await?;
                            task.state = NodeMutationTaskState::Prepared;
                            task.prepared_owner_epoch = Some(owner);
                            task.prepared_at_millis = Some(now.max(task.created_at_millis));
                            let encoded = encode(&task)?;
                            let held = hold_value(&task)?;
                            tr.set(
                                &identity_key(
                                    task.namespace,
                                    &c.library_serial,
                                    b'o',
                                    task.request_epoch,
                                    task.operation,
                                ),
                                &encoded,
                            );
                            tr.set(&hold_key(task.namespace, task.pool), &held);
                            Ok(task)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}
