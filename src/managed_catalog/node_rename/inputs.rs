//! Canonical two-parent inputs under the original Prepared hold. No subtree
//! or physical projection is claimed by this metadata-only snapshot.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedNodeRenameInputs {
    pub task: NodeMutationTask,
    pub request: NodeRenameRequest,
    pub baseline: NodeRenameBaseline,
    pub source_ancestors: Vec<NodeMutationAncestor>,
    pub destination_ancestors: Vec<NodeMutationAncestor>,
}

fn attributes(value: &ResolvedPath) -> Result<&NodeAttributes> {
    let (ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. }) =
        value;
    attributes
        .as_ref()
        .ok_or(CatalogError::Corrupt("rename 原节点属性缺失"))
}

pub(in crate::managed_catalog::control) async fn require_inputs_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    inputs: &PreparedNodeRenameInputs,
) -> Result<()> {
    node_tasks::require_rename_prepared_at(tr, route, owner, &inputs.task, &inputs.request).await?;
    require_body_at(tr, route, owner, inputs).await
}

pub(super) async fn require_held_inputs_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    inputs: &PreparedNodeRenameInputs,
) -> Result<()> {
    node_tasks::require_rename_held_at(tr, route, owner, &inputs.task, &inputs.request).await?;
    require_body_at(tr, route, owner, inputs).await
}

async fn require_body_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    inputs: &PreparedNodeRenameInputs,
) -> Result<()> {
    let baseline = &inputs.baseline;
    let request = &inputs.request;
    let namespace = request.spec().source.namespace;
    if baseline.owner_epoch != owner || !request.matches_baseline(baseline)? {
        return Err(CatalogError::Invalid(
            "rename 规范输入与原请求或当前 owner 不符",
        ));
    }
    let current = revalidate_at(tr, route, owner, request).await?;
    for parent in [&current.source_parent, &current.destination_parent] {
        attribute_objects::require_attributes(tr, namespace, parent.attributes.as_ref()).await?;
    }
    for node in std::iter::once(&current.source).chain(current.target.as_ref()) {
        attribute_objects::require_attributes(tr, namespace, Some(attributes(node)?)).await?;
    }
    node_inputs::require_ancestor_chain_at(
        tr,
        namespace,
        request.spec().pool,
        &request.spec().from,
        &baseline.source_parent,
        &inputs.source_ancestors,
    )
    .await?;
    node_inputs::require_ancestor_chain_at(
        tr,
        namespace,
        request.spec().pool,
        &request.spec().to,
        &baseline.destination_parent,
        &inputs.destination_ancestors,
    )
    .await?;
    for node in std::iter::once(&baseline.source).chain(baseline.target.as_ref()) {
        node_inputs::require_full_attributes(
            attributes(node)?,
            matches!(node, ResolvedPath::Directory { .. }),
        )?;
    }
    Ok(())
}

impl ManagedCatalog {
    /// Revalidate in one transaction. A successful check is not admission or a media permit.
    pub async fn check_prepared_node_rename_inputs(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        inputs: &PreparedNodeRenameInputs,
    ) -> Result<()> {
        require_inputs_at(&self.db.create_trx()?, route, owner, inputs).await
    }

    /// Charge all declared attributes before chunk I/O, hydrate only captured
    /// immutable references, then recheck both chains and the original hold.
    pub async fn capture_prepared_node_rename_inputs(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        max_bytes: usize,
    ) -> Result<PreparedNodeRenameInputs> {
        self.capture_node_rename_inputs(route, owner, (serial, epoch, operation), max_bytes, false)
            .await
    }
    pub(super) async fn capture_held_node_rename_inputs(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        identity: (&str, u64, Uuid),
        max_bytes: usize,
    ) -> Result<PreparedNodeRenameInputs> {
        self.capture_node_rename_inputs(route, owner, identity, max_bytes, true)
            .await
    }
    async fn capture_node_rename_inputs(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        identity: (&str, u64, Uuid),
        max_bytes: usize,
        held: bool,
    ) -> Result<PreparedNodeRenameInputs> {
        let (serial, epoch, operation) = identity;
        let (task, request) = self
            .node_rename_task_request(serial, route, owner, epoch, operation, max_bytes)
            .await?;
        let mut used = request.encode()?.len();
        let tr = self.db.create_trx()?;
        require_capture_task_at(&tr, route, owner, &task, &request, held).await?;
        let mut baseline = revalidate_at(&tr, route, owner, &request).await?;
        let namespace = request.spec().source.namespace;
        let pool = request.spec().pool;
        let mut source_ancestors = node_inputs::capture_ancestor_chain_at(
            &tr,
            namespace,
            pool,
            &request.spec().from,
            &baseline.source_parent,
            (&mut used, max_bytes),
        )
        .await?;
        let mut destination_ancestors = node_inputs::capture_ancestor_chain_at(
            &tr,
            namespace,
            pool,
            &request.spec().to,
            &baseline.destination_parent,
            (&mut used, max_bytes),
        )
        .await?;
        for node in std::iter::once(&baseline.source).chain(baseline.target.as_ref()) {
            node_inputs::charge_attributes(&mut used, attributes(node)?, max_bytes)?;
        }
        drop(tr);
        for ancestor in source_ancestors
            .iter_mut()
            .chain(destination_ancestors.iter_mut())
        {
            require_capture_task_at(&self.db.create_trx()?, route, owner, &task, &request, held)
                .await?;
            ancestor.attributes = self
                .hydrate_attributes(namespace, ancestor.attributes.clone())
                .await?;
            node_inputs::require_full_attributes(&ancestor.attributes, true)?;
        }
        baseline.source_parent.attributes = Some(
            source_ancestors
                .last()
                .expect("checked source chain")
                .attributes
                .clone(),
        );
        baseline.destination_parent.attributes = Some(
            destination_ancestors
                .last()
                .expect("checked destination chain")
                .attributes
                .clone(),
        );
        for node in std::iter::once(&mut baseline.source).chain(baseline.target.as_mut()) {
            let directory = matches!(node, ResolvedPath::Directory { .. });
            let (ResolvedPath::Directory { attributes, .. }
            | ResolvedPath::File { attributes, .. }) = node;
            require_capture_task_at(&self.db.create_trx()?, route, owner, &task, &request, held)
                .await?;
            let value = self
                .hydrate_attributes(
                    namespace,
                    attributes.take().expect("checked original attributes"),
                )
                .await?;
            node_inputs::require_full_attributes(&value, directory)?;
            *attributes = Some(value);
        }
        let inputs = PreparedNodeRenameInputs {
            task,
            request,
            baseline,
            source_ancestors,
            destination_ancestors,
        };
        if held {
            require_held_inputs_at(&self.db.create_trx()?, route, owner, &inputs).await?;
        } else {
            self.check_prepared_node_rename_inputs(route, owner, &inputs)
                .await?;
        }
        Ok(inputs)
    }
}

async fn require_capture_task_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    task: &NodeMutationTask,
    request: &NodeRenameRequest,
    held: bool,
) -> Result<()> {
    if held {
        node_tasks::require_rename_held_at(tr, route, owner, task, request).await
    } else {
        node_tasks::require_rename_prepared_at(tr, route, owner, task, request).await
    }
}
