//! Bounded, read-only canonical source traversal under the original pool hold.
//! An in-process cursor is not a durable manifest or permission to write media.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameSubtreeEntry {
    /// Original pool-relative path, including the source root itself.
    pub path: String,
    pub destination: String,
    /// Attributes may be immutable external references, never fresh-path values.
    pub node: ResolvedPath,
}

impl NodeRenameSubtreeEntry {
    /// Canonical inventory bytes. These bytes alone are not capture authority.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let (ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. }) =
            &self.node;
        let attributes = attributes.as_ref().ok_or(invalid("rename 子树缺少属性"))?;
        let storage = if let Some(reference) = attributes.manifest() {
            json!({"digest":reference.digest,"len":reference.len,"chunks":reference.chunks})
        } else {
            json!({"inline":URL_SAFE_NO_PAD.encode(attributes.bytes().ok_or(invalid("rename 子树属性编码缺失"))?)})
        };
        let object = match &self.node {
            ResolvedPath::File {
                object_metadata: Some(value),
                ..
            } => json!({"modified":value.modified,"headers":value.headers}),
            _ => serde_json::Value::Null,
        };
        node_mutation::canonical(&json!({"version":1,"path":self.path,
            "destination":self.destination,"node":node_request::resolved_value(Some(&self.node)),
            "attributes":storage,"object_metadata":object}))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRenameSubtreeSummary {
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub encoded_bytes: u64,
    pub sha256: [u8; 32],
    pub complete: bool,
}

#[derive(Debug)]
pub struct NodeRenameSubtreePage {
    pub entries: Vec<NodeRenameSubtreeEntry>,
    pub summary: NodeRenameSubtreeSummary,
}

#[derive(Clone)]
struct Frame {
    id: Uuid,
    added: usize,
    after: Option<Vec<u8>>,
    finished: bool,
}

/// No caller-supplied cursor, uploaded hash or mutable public input can create
/// or advance this state. Persisted sealing/admission is a separate operation.
#[derive(Clone)]
pub struct NodeRenameSubtreeCapture {
    inputs: PreparedNodeRenameInputs,
    path: String,
    frames: Vec<Frame>,
    root_emitted: bool,
    entries: u64,
    files: u64,
    directories: u64,
    encoded_bytes: u64,
    hash: Sha256,
}

impl NodeRenameSubtreeCapture {
    pub fn summary(&self) -> NodeRenameSubtreeSummary {
        NodeRenameSubtreeSummary {
            entries: self.entries,
            files: self.files,
            directories: self.directories,
            encoded_bytes: self.encoded_bytes,
            sha256: self.hash.clone().finalize().into(),
            complete: self.root_emitted && self.frames.is_empty(),
        }
    }

    fn pop_finished(&mut self) {
        while self.frames.last().is_some_and(|frame| frame.finished) {
            let frame = self.frames.pop().expect("checked frame");
            self.path.truncate(self.path.len() - frame.added);
        }
    }

    fn row(&self, path: String, mut node: ResolvedPath) -> Result<NodeRenameSubtreeEntry> {
        let spec = self.inputs.request.spec();
        let suffix = path
            .strip_prefix(&spec.from)
            .ok_or(invalid("rename 子树越过源路径"))?;
        if !suffix.is_empty() && !suffix.starts_with('/') {
            return Err(invalid("rename 子树路径边界不符"));
        }
        let destination = format!("{}{suffix}", spec.to);
        // A valid source can still exceed the path depth at a deeper destination.
        pool_catalog_path(spec.pool, destination.as_bytes())?;
        let directory = matches!(node, ResolvedPath::Directory { .. });
        let (ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. }) =
            &mut node;
        let attributes = attributes.as_mut().ok_or(invalid("rename 子树属性缺失"))?;
        if directory && attributes.symlink().is_some() {
            return Err(invalid("rename 子树目录含链接属性"));
        }
        // Prepared inputs hydrate the root. Keep every returned large value a
        // compact immutable reference, including that already hydrated root.
        if let Some(reference) = attributes.manifest() {
            *attributes = NodeAttributes::from_manifest(reference)?;
        }
        Ok(NodeRenameSubtreeEntry {
            path,
            destination,
            node,
        })
    }

    fn record(
        &mut self,
        row: &NodeRenameSubtreeEntry,
        used: &mut usize,
        max_bytes: usize,
    ) -> Result<()> {
        let bytes = row.encode()?;
        let charge = bytes
            .len()
            .checked_add(8)
            .ok_or(invalid("rename 子树页长度溢出"))?;
        node_inputs::charge(used, charge, max_bytes)?;
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(charge as u64)
            .ok_or(invalid("rename 子树总长度溢出"))?;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(invalid("rename 子树计数溢出"))?;
        match row.node {
            ResolvedPath::Directory { .. } => self.directories += 1,
            ResolvedPath::File { .. } => self.files += 1,
        }
        self.hash.update((bytes.len() as u64).to_be_bytes());
        self.hash.update(bytes);
        Ok(())
    }
}

async fn require_row_at(
    tr: &Transaction,
    capture: &NodeRenameSubtreeCapture,
    row: &NodeRenameSubtreeEntry,
) -> Result<()> {
    let spec = capture.inputs.request.spec();
    let namespace = spec.source.namespace;
    node_available_at(
        tr,
        capture.inputs.request.library(),
        namespace,
        spec.pool,
        &row.node,
    )
    .await?;
    let id = revision(namespace, &row.node).inode;
    let (ResolvedPath::Directory { attributes, .. } | ResolvedPath::File { attributes, .. }) =
        &row.node;
    let attributes = attributes
        .as_ref()
        .ok_or(invalid("rename 子树节点属性缺失"))?;
    attribute_objects::require_attributes(tr, namespace, Some(attributes)).await?;
    if matches!(row.node, ResolvedPath::Directory { .. }) {
        if attributes.symlink().is_some() {
            return Err(invalid("rename 子树目录含链接属性"));
        }
        for prefix in [
            reservation::path_prefix_for(namespace, id),
            active_batch::path_claim_prefix_for(namespace, id),
        ] {
            if requests::prefix_has_rows(tr, &prefix).await? {
                return Err(CatalogError::Incomplete);
            }
        }
    } else if requests::prefix_has_rows(tr, &child_prefix(namespace, id)).await? {
        return Err(CatalogError::Corrupt("rename 子树文件含目录项"));
    }
    Ok(())
}

impl ManagedCatalog {
    pub async fn begin_node_rename_subtree_capture(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        inputs: &PreparedNodeRenameInputs,
    ) -> Result<NodeRenameSubtreeCapture> {
        self.check_prepared_node_rename_inputs(route, owner, inputs)
            .await?;
        let hash = initial_hasher(inputs)?;
        Ok(NodeRenameSubtreeCapture {
            inputs: inputs.clone(),
            path: inputs.request.spec().from.clone(),
            frames: Vec::new(),
            root_emitted: false,
            entries: 0,
            files: 0,
            directories: 0,
            encoded_bytes: 0,
            hash,
        })
    }

    /// Depth-first traversal in canonical child-key order. Errors leave the
    /// caller's cursor unchanged; a smaller page or a larger byte budget can retry.
    pub async fn next_node_rename_subtree_page(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &mut NodeRenameSubtreeCapture,
        limit: usize,
        max_bytes: usize,
    ) -> Result<NodeRenameSubtreePage> {
        if !(1..=MAX_PAGE).contains(&limit) || !(1..=8 << 20).contains(&max_bytes) {
            return Err(invalid("rename 子树页条数或字节预算无效"));
        }
        self.check_prepared_node_rename_inputs(route, owner, &capture.inputs)
            .await?;
        let mut next = capture.clone();
        let mut entries = Vec::new();
        let mut used = 0;
        while entries.len() < limit {
            let tr = self.db.create_trx()?;
            node_tasks::require_rename_prepared_at(
                &tr,
                route,
                owner,
                &next.inputs.task,
                &next.inputs.request,
            )
            .await?;
            let row = if !next.root_emitted {
                let row = next.row(next.path.clone(), next.inputs.baseline.source.clone())?;
                require_row_at(&tr, &next, &row).await?;
                next.root_emitted = true;
                row
            } else {
                next.pop_finished();
                let Some(frame) = next.frames.last() else {
                    break;
                };
                let prefix = child_prefix(next.inputs.request.spec().source.namespace, frame.id);
                let page = read_page(&tr, &prefix, frame.after.as_deref(), 1, false).await?;
                let Some((key, raw_id)) = page.items.into_iter().next() else {
                    if page.next.is_some() {
                        return Err(CatalogError::Corrupt("rename 子树空页含游标"));
                    }
                    next.frames.last_mut().expect("checked frame").finished = true;
                    continue;
                };
                let name = decode_name(&key[prefix.len()..])?;
                let id = uuid_from(&raw_id)?;
                let name =
                    std::str::from_utf8(&name).map_err(|_| invalid("rename 子树名称不是 UTF-8"))?;
                let path = format!("{}/{name}", next.path);
                let full = pool_catalog_path(next.inputs.request.spec().pool, path.as_bytes())?;
                let node = lookup::resolve_path_at_mode(
                    &tr,
                    next.inputs.request.spec().source.namespace,
                    lookup::path_components(&full)?,
                    false,
                    true,
                )
                .await?
                .ok_or(CatalogError::Corrupt("rename 子树目录项缺少节点"))?;
                if id.is_nil()
                    || revision(next.inputs.request.spec().source.namespace, &node).inode != id
                    || next.frames.iter().any(|frame| frame.id == id)
                {
                    return Err(CatalogError::Corrupt("rename 子树节点身份冲突或成环"));
                }
                let row = next.row(path, node)?;
                require_row_at(&tr, &next, &row).await?;
                let frame = next.frames.last_mut().expect("checked parent frame");
                frame.after = Some(key);
                frame.finished = page.next.is_none();
                row
            };
            next.record(&row, &mut used, max_bytes)?;
            if let ResolvedPath::Directory { id, .. } = row.node {
                let added = row.path.len() - next.path.len();
                next.path.clone_from(&row.path);
                next.frames.push(Frame {
                    id,
                    added,
                    after: None,
                    finished: false,
                });
            }
            entries.push(row);
        }
        next.pop_finished();
        self.check_prepared_node_rename_inputs(route, owner, &next.inputs)
            .await?;
        let summary = next.summary();
        *capture = next;
        Ok(NodeRenameSubtreePage { entries, summary })
    }
}

#[cfg(test)]
#[path = "subtree_tests.rs"]
mod tests;

pub(super) fn initial_hasher(inputs: &PreparedNodeRenameInputs) -> Result<Sha256> {
    let mut hash = Sha256::new()
        .chain_update(b"tape-rs/node-rename-subtree/1")
        .chain_update(inputs.request.fingerprint());
    for chain in [&inputs.source_ancestors, &inputs.destination_ancestors] {
        hash.update((chain.len() as u64).to_be_bytes());
        for ancestor in chain {
            let bytes = node_mutation::canonical(&json!({"path":ancestor.path,
                    "inode":ancestor.id.to_string(),"version":ancestor.version,
                    "attributes":ancestor.attributes.digest()}))?;
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
    }
    Ok(hash)
}
