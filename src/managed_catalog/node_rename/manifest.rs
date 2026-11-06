//! Trusted, namespace-owned canonical inventory. Generic object uploads remain
//! inert; only this producer associates captured pages with an original task.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

const PAGE_BYTES: usize = 8 << 20;
const PAGE_ROWS: usize = 64;
const PAGE_DOMAIN: &[u8] = b"tape-rs/node-rename-manifest-page/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    request: String,
    fingerprint: [u8; 32],
    prepared_owner: u64,
    prepared_time: u64,
    creator_owner: u64,
    pages: u64,
    initial_rows_sha256: [u8; 32],
    page_chain: [u8; 32],
    summary: NodeRenameSubtreeSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPage {
    version: u8,
    index: u64,
    before: [u8; 32],
    previous_pages: [u8; 32],
    object: String,
    entries: u16,
    limit: u16,
    summary: NodeRenameSubtreeSummary,
}

/// A live producer, never deserialized from a caller-supplied cursor.
pub struct NodeRenameManifestCapture {
    inputs: PreparedNodeRenameInputs,
    cursor: NodeRenameSubtreeCapture,
    header: Header,
}

impl NodeRenameManifestCapture {
    pub fn summary(&self) -> &NodeRenameSubtreeSummary {
        &self.header.summary
    }
    pub fn pages(&self) -> u64 {
        self.header.pages
    }
}

#[derive(Clone)]
pub struct NodeRenameManifest {
    task: NodeMutationTask,
    request: NodeRenameRequest,
    header: Header,
}

impl NodeRenameManifest {
    pub fn summary(&self) -> &NodeRenameSubtreeSummary {
        &self.header.summary
    }
    pub fn pages(&self) -> u64 {
        self.header.pages
    }
    pub fn creator_owner_epoch(&self) -> u64 {
        self.header.creator_owner
    }
}

fn prefix(task: &NodeMutationTask) -> Vec<u8> {
    // Q/M is reserved for trusted rename manifests. Namespace retirement owns Q.
    let mut key = management::prefix(task.namespace, b'Q');
    key.push(b'M');
    append_component(&mut key, task.library_serial.as_bytes());
    key.extend_from_slice(&task.request_epoch.to_be_bytes());
    key.extend_from_slice(task.operation.as_bytes());
    key
}
fn header_key(task: &NodeMutationTask) -> Vec<u8> {
    let mut k = prefix(task);
    k.push(0);
    k
}
fn pages_prefix(task: &NodeMutationTask) -> Vec<u8> {
    let mut k = prefix(task);
    k.push(1);
    k
}
fn page_key(task: &NodeMutationTask, index: u64) -> Vec<u8> {
    let mut k = pages_prefix(task);
    k.extend_from_slice(&index.to_be_bytes());
    k
}
pub struct NodeRenameManifestReader {
    manifest: NodeRenameManifest,
    next: u64,
    page_chain: [u8; 32],
    previous: NodeRenameSubtreeSummary,
}
impl NodeRenameManifestReader {
    pub fn verified_pages(&self) -> u64 {
        self.next
    }
    pub fn complete(&self) -> bool {
        self.next == self.manifest.header.pages
    }
}

fn initial_page_chain(fingerprint: [u8; 32], initial: [u8; 32]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-manifest-pages/1")
        .chain_update(fingerprint)
        .chain_update(initial)
        .finalize()
        .into()
}
fn page_chain(record: &StoredPage) -> Result<[u8; 32]> {
    Ok(Sha256::new()
        .chain_update(b"tape-rs/node-rename-manifest-pages/1")
        .chain_update(record.previous_pages)
        .chain_update(node_mutation::canonical(record)?)
        .finalize()
        .into())
}
fn corrupt() -> CatalogError {
    CatalogError::Corrupt("rename 规范清单记录不一致")
}
fn decode<T: DeserializeOwned + Serialize>(raw: &[u8]) -> Result<T> {
    if raw.len() > 64_000 {
        return Err(corrupt());
    }
    let value: T = serde_json::from_slice(raw).map_err(|_| corrupt())?;
    if node_mutation::canonical(&value)? != raw {
        return Err(corrupt());
    }
    Ok(value)
}
fn valid_summary(value: &NodeRenameSubtreeSummary) -> Result<()> {
    if value
        .entries
        .checked_mul(8)
        .is_none_or(|minimum| value.encoded_bytes < minimum)
        || value.files.checked_add(value.directories) != Some(value.entries)
        || (value.entries == 0 && (value.encoded_bytes != 0 || value.complete))
    {
        return Err(corrupt());
    }
    Ok(())
}
fn validate_header(header: &Header, task: &NodeMutationTask) -> Result<()> {
    valid_summary(&header.summary)?;
    if header.version != 1
        || header.creator_owner == 0
        || header.creator_owner < header.prepared_owner
        || header.request != URL_SAFE_NO_PAD.encode(task.object.encode())
        || header.fingerprint != task.fingerprint
        || Some(header.prepared_owner) != task.prepared_owner_epoch
        || Some(header.prepared_time) != task.prepared_at_millis
        || header.pages > header.summary.entries.saturating_add(1)
        || (header.pages == 0 && header.summary.entries != 0)
        || (header.summary.complete && header.pages == 0)
        || (header.pages == 0
            && (header.summary.sha256 != header.initial_rows_sha256
                || header.page_chain
                    != initial_page_chain(task.fingerprint, header.initial_rows_sha256)))
    {
        return Err(corrupt());
    }
    Ok(())
}
fn object(value: &StoredPage, task: &NodeMutationTask) -> Result<NodePlanObject> {
    let raw = URL_SAFE_NO_PAD
        .decode(&value.object)
        .map_err(|_| corrupt())?;
    let object = NodePlanObject::decode(&raw)?;
    if object.namespace() != task.namespace
        || object.library() != task.library_serial
        || object.byte_len() as usize > PAGE_BYTES
        || value.version != 1
        || !(1..=PAGE_ROWS).contains(&usize::from(value.limit))
        || value.entries > value.limit
    {
        return Err(corrupt());
    }
    valid_summary(&value.summary)?;
    Ok(object)
}

async fn no_extra_pages_at(tr: &Transaction, task: &NodeMutationTask, count: u64) -> Result<()> {
    let after = count.checked_sub(1).map(|n| page_key(task, n));
    if !read_page(tr, &pages_prefix(task), after.as_deref(), 1, false)
        .await?
        .items
        .is_empty()
    {
        return Err(corrupt());
    }
    Ok(())
}
async fn require_header_at(
    tr: &Transaction,
    task: &NodeMutationTask,
    expected: &Header,
) -> Result<()> {
    let raw = tr
        .get(&header_key(task), false)
        .await?
        .ok_or_else(corrupt)?;
    let saved: Header = decode(&raw)?;
    validate_header(&saved, task)?;
    if &saved != expected {
        return Err(corrupt());
    }
    no_extra_pages_at(tr, task, saved.pages).await
}

fn page_bytes(
    task: &NodeMutationTask,
    index: u64,
    before: [u8; 32],
    page: &NodeRenameSubtreePage,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PAGE_DOMAIN);
    bytes.extend_from_slice(&task.fingerprint);
    bytes.extend_from_slice(&index.to_be_bytes());
    bytes.extend_from_slice(&before);
    for entry in &page.entries {
        let row = entry.encode()?;
        bytes.extend_from_slice(&(row.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&row);
    }
    if bytes.len() > PAGE_BYTES {
        return Err(invalid("rename 规范清单页超过预算"));
    }
    Ok(bytes)
}

impl ManagedCatalog {
    /// Resume by replaying the saved prefix under the same original Prepared
    /// hold. This performs O(saved history) reads once per process restart.
    pub async fn resume_node_rename_manifest_capture(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        inputs: &PreparedNodeRenameInputs,
    ) -> Result<NodeRenameManifestCapture> {
        let mut cursor = self
            .begin_node_rename_subtree_capture(route, owner, inputs)
            .await?;
        let initial = Header {
            version: 1,
            request: URL_SAFE_NO_PAD.encode(inputs.task.object.encode()),
            fingerprint: inputs.task.fingerprint,
            prepared_owner: inputs.task.prepared_owner_epoch.ok_or_else(corrupt)?,
            prepared_time: inputs.task.prepared_at_millis.ok_or_else(corrupt)?,
            creator_owner: owner,
            pages: 0,
            initial_rows_sha256: cursor.summary().sha256,
            page_chain: initial_page_chain(inputs.task.fingerprint, cursor.summary().sha256),
            summary: cursor.summary(),
        };
        let task = &inputs.task;
        let header = self
            .db
            .run(|tr, _| {
                let initial = initial.clone();
                async move {
                    requests::retry_result(
                        async {
                            inputs::require_inputs_at(&tr, route, owner, inputs).await?;
                            if let Some(raw) = tr.get(&header_key(task), false).await? {
                                let header: Header = decode(&raw)?;
                                validate_header(&header, task)?;
                                no_extra_pages_at(&tr, task, header.pages).await?;
                                return Ok(header);
                            }
                            no_extra_pages_at(&tr, task, 0).await?;
                            let raw = node_mutation::canonical(&initial)?;
                            tr.set(&header_key(task), &raw);
                            Ok(initial)
                        }
                        .await,
                    )
                }
            })
            .await??;
        let mut previous = cursor.summary();
        if header.initial_rows_sha256 != previous.sha256 {
            return Err(corrupt());
        }
        let mut chain = initial_page_chain(task.fingerprint, previous.sha256);
        for index in 0..header.pages {
            let tr = self.db.create_trx()?;
            inputs::require_inputs_at(&tr, route, owner, inputs).await?;
            require_header_at(&tr, task, &header).await?;
            let raw = tr
                .get(&page_key(task, index), false)
                .await?
                .ok_or_else(corrupt)?;
            let stored: StoredPage = decode(&raw)?;
            let saved_object = object(&stored, task)?;
            if stored.index != index
                || stored.before != previous.sha256
                || stored.previous_pages != chain
                || previous.complete
            {
                return Err(corrupt());
            }
            drop(tr);
            let page = self
                .next_node_rename_subtree_page(
                    route,
                    owner,
                    &mut cursor,
                    usize::from(stored.limit),
                    PAGE_BYTES - 256,
                )
                .await?;
            if page.entries.len() != usize::from(stored.entries) || page.summary != stored.summary {
                return Err(corrupt());
            }
            let bytes = page_bytes(task, index, previous.sha256, &page)?;
            let expected = NodePlanObject::for_bytes(
                &task.library_serial,
                task.namespace,
                &bytes,
                PAGE_BYTES,
            )?;
            if saved_object != expected {
                return Err(corrupt());
            }
            // A sealed marker alone does not validate bytes. Stream every
            // original chunk and compare it; never repair a damaged object.
            self.upload_node_plan_object(route, owner, &saved_object, &mut &bytes[..])
                .await?;
            previous = page.summary;
            chain = page_chain(&stored)?;
        }
        if previous != header.summary || chain != header.page_chain {
            return Err(corrupt());
        }
        let tr = self.db.create_trx()?;
        inputs::require_inputs_at(&tr, route, owner, inputs).await?;
        require_header_at(&tr, task, &header).await?;
        Ok(NodeRenameManifestCapture {
            inputs: inputs.clone(),
            cursor,
            header,
        })
    }

    /// Capture and persist the next page. No supplied bytes/object/hash enter
    /// the trusted association. Errors retain the producer's original cursor.
    pub async fn next_node_rename_manifest_page(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &mut NodeRenameManifestCapture,
    ) -> Result<NodeRenameSubtreeSummary> {
        let inputs = &capture.inputs;
        self.check_prepared_node_rename_inputs(route, owner, inputs)
            .await?;
        let tr = self.db.create_trx()?;
        // A prior unknown commit may have saved exactly this page. The commit
        // below accepts that exact after-state; divergent progress is refused.
        if capture.header.summary.complete {
            require_header_at(&tr, &inputs.task, &capture.header).await?;
            return Ok(capture.header.summary.clone());
        }
        drop(tr);
        let mut cursor = capture.cursor.clone();
        let mut limit = PAGE_ROWS;
        let page = loop {
            match self
                .next_node_rename_subtree_page(route, owner, &mut cursor, limit, PAGE_BYTES - 256)
                .await
            {
                Err(CatalogError::Invalid("节点输入超过属性字节预算")) if limit > 1 => {
                    limit /= 2;
                }
                result => break result?,
            }
        };
        let index = capture.header.pages;
        let bytes = page_bytes(&inputs.task, index, capture.header.summary.sha256, &page)?;
        let reference = NodePlanObject::for_bytes(
            &inputs.task.library_serial,
            inputs.task.namespace,
            &bytes,
            PAGE_BYTES,
        )?;
        self.upload_node_plan_object(route, owner, &reference, &mut &bytes[..])
            .await?;
        let stored = StoredPage {
            version: 1,
            index,
            before: capture.header.summary.sha256,
            previous_pages: capture.header.page_chain,
            object: URL_SAFE_NO_PAD.encode(reference.encode()),
            entries: u16::try_from(page.entries.len()).map_err(|_| corrupt())?,
            limit: limit as u16,
            summary: page.summary.clone(),
        };
        let after = Header {
            pages: index.checked_add(1).ok_or_else(corrupt)?,
            summary: page.summary.clone(),
            page_chain: page_chain(&stored)?,
            ..capture.header.clone()
        };
        validate_header(&after, &inputs.task)?;
        let encoded_page = node_mutation::canonical(&stored)?;
        let encoded_after = node_mutation::canonical(&after)?;
        let before = &capture.header;
        self.db
            .run(|tr, _| {
                let reference = &reference;
                let after = &after;
                let encoded_page = &encoded_page;
                let encoded_after = &encoded_after;
                async move {
                    requests::retry_result(
                        async {
                            inputs::require_inputs_at(&tr, route, owner, inputs).await?;
                            node_plan_objects::require_sealed_at(&tr, reference).await?;
                            let raw = tr
                                .get(&header_key(&inputs.task), false)
                                .await?
                                .ok_or_else(corrupt)?;
                            let saved: Header = decode(&raw)?;
                            validate_header(&saved, &inputs.task)?;
                            if &saved == after {
                                if tr
                                    .get(&page_key(&inputs.task, index), false)
                                    .await?
                                    .as_deref()
                                    != Some(encoded_page.as_slice())
                                {
                                    return Err(corrupt());
                                }
                                no_extra_pages_at(&tr, &inputs.task, saved.pages).await?;
                                return Ok(());
                            }
                            if &saved != before {
                                return Err(corrupt());
                            }
                            no_extra_pages_at(&tr, &inputs.task, saved.pages).await?;
                            // All validation/encoding precedes the first mutation.
                            tr.set(&page_key(&inputs.task, index), encoded_page);
                            tr.set(&header_key(&inputs.task), encoded_after);
                            Ok(())
                        }
                        .await,
                    )
                }
            })
            .await??;
        capture.cursor = cursor;
        capture.header = after;
        Ok(page.summary)
    }

    pub async fn finish_node_rename_manifest_capture(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &NodeRenameManifestCapture,
    ) -> Result<NodeRenameManifest> {
        if !capture.header.summary.complete {
            return Err(CatalogError::Incomplete);
        }
        let tr = self.db.create_trx()?;
        inputs::require_inputs_at(&tr, route, owner, &capture.inputs).await?;
        require_header_at(&tr, &capture.inputs.task, &capture.header).await?;
        Ok(NodeRenameManifest {
            task: capture.inputs.task.clone(),
            request: capture.inputs.request.clone(),
            header: capture.header.clone(),
        })
    }

    /// A reader verifies every page in order, including the full sealed object
    /// bytes and the header's terminal page chain. It grants no media permit.
    pub async fn open_node_rename_manifest_reader(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
    ) -> Result<NodeRenameManifestReader> {
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        if !manifest.header.summary.complete {
            return Err(CatalogError::Incomplete);
        }
        Ok(NodeRenameManifestReader {
            manifest: manifest.clone(),
            next: 0,
            page_chain: initial_page_chain(
                manifest.task.fingerprint,
                manifest.header.initial_rows_sha256,
            ),
            previous: NodeRenameSubtreeSummary {
                entries: 0,
                files: 0,
                directories: 0,
                encoded_bytes: 0,
                sha256: manifest.header.initial_rows_sha256,
                complete: false,
            },
        })
    }

    async fn read_node_rename_manifest_body(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        reader: &mut NodeRenameManifestReader,
    ) -> Result<Option<Vec<u8>>> {
        let manifest = &reader.manifest;
        let index = reader.next;
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        if index == manifest.header.pages {
            return Ok(None);
        }
        let raw = tr
            .get(&page_key(&manifest.task, index), false)
            .await?
            .ok_or_else(corrupt)?;
        let stored: StoredPage = decode(&raw)?;
        if stored.index != index
            || stored.previous_pages != reader.page_chain
            || stored.before != reader.previous.sha256
            || stored.summary.entries.checked_sub(reader.previous.entries)
                != Some(u64::from(stored.entries))
            || stored.summary.files < reader.previous.files
            || stored.summary.directories < reader.previous.directories
            || stored.summary.encoded_bytes < reader.previous.encoded_bytes
            || stored.summary.complete != (index + 1 == manifest.header.pages)
        {
            return Err(corrupt());
        }
        let chain = page_chain(&stored)?;
        if index + 1 == manifest.header.pages
            && (chain != manifest.header.page_chain || stored.summary != manifest.header.summary)
        {
            return Err(corrupt());
        }
        let reference = object(&stored, &manifest.task)?;
        node_plan_objects::require_sealed_at(&tr, &reference).await?;
        drop(tr);
        let mut bytes = Vec::with_capacity(reference.byte_len() as usize);
        self.node_plan_object_to(route, owner, &reference, PAGE_BYTES, &mut bytes)
            .await?;
        let mut expected = Vec::new();
        expected.extend_from_slice(PAGE_DOMAIN);
        expected.extend_from_slice(&manifest.task.fingerprint);
        expected.extend_from_slice(&index.to_be_bytes());
        expected.extend_from_slice(&stored.before);
        if !bytes.starts_with(&expected) {
            return Err(corrupt());
        }
        let mut offset = expected.len();
        for _ in 0..stored.entries {
            let end = offset
                .checked_add(8)
                .filter(|n| *n <= bytes.len())
                .ok_or_else(corrupt)?;
            let len = u64::from_be_bytes(bytes[offset..end].try_into().map_err(|_| corrupt())?);
            offset = end
                .checked_add(usize::try_from(len).map_err(|_| corrupt())?)
                .filter(|n| *n <= bytes.len())
                .ok_or_else(corrupt)?;
        }
        if offset != bytes.len()
            || stored
                .summary
                .encoded_bytes
                .checked_sub(reader.previous.encoded_bytes)
                != Some((offset - expected.len()) as u64)
        {
            return Err(corrupt());
        }
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        reader.next += 1;
        reader.page_chain = chain;
        reader.previous = stored.summary;
        Ok(Some(bytes))
    }
}

impl ManagedCatalog {
    /// Decode only the exact original rows from the verified manifest. A bad
    /// row, foreign scope or owner change leaves the original reader unchanged.
    pub async fn next_node_rename_manifest_entries(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        reader: &mut NodeRenameManifestReader,
    ) -> Result<Option<Vec<NodeRenameSubtreeEntry>>> {
        let mut next = NodeRenameManifestReader {
            manifest: reader.manifest.clone(),
            next: reader.next,
            page_chain: reader.page_chain,
            previous: reader.previous.clone(),
        };
        let Some(bytes) = self
            .read_node_rename_manifest_body(route, owner, &mut next)
            .await?
        else {
            *reader = next;
            return Ok(None);
        };
        let count = next
            .previous
            .entries
            .checked_sub(reader.previous.entries)
            .ok_or_else(corrupt)?;
        let offset = PAGE_DOMAIN.len() + 32 + 8 + 32;
        let payload = bytes.get(offset..).ok_or_else(corrupt)?;
        let entries = entry::decode_entries(payload, count, &reader.manifest.request)?;
        if reader.previous.entries == 0 {
            let spec = reader.manifest.request.spec();
            let root = entries.first().ok_or_else(corrupt)?;
            if root.path != spec.from || revision(spec.source.namespace, &root.node) != spec.source
            {
                return Err(corrupt());
            }
        }
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(
            &tr,
            route,
            owner,
            &reader.manifest.task,
            &reader.manifest.request,
        )
        .await?;
        require_header_at(&tr, &reader.manifest.task, &reader.manifest.header).await?;
        *reader = next;
        Ok(Some(entries))
    }
}
impl ManagedCatalog {
    /// Build a disposable view only from the complete verified original
    /// manifest. No path or external SQLite file is accepted as evidence.
    pub async fn build_node_rename_projection_spool(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        max_database_bytes: u64,
    ) -> Result<NodeRenameProjectionSpool> {
        let mut reader = self
            .open_node_rename_manifest_reader(route, owner, manifest)
            .await?;
        let mut spool = projection::NodeRenameProjectionSpool::new(
            &manifest.request,
            &manifest.header.summary,
            max_database_bytes,
        )?;
        while let Some(entries) = self
            .next_node_rename_manifest_entries(route, owner, &mut reader)
            .await?
        {
            spool.append(&entries)?;
        }
        if !reader.complete() {
            return Err(CatalogError::Incomplete);
        }
        spool.seal()?;
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        Ok(spool)
    }
}

impl ManagedCatalog {
    /// Capture current primary references through the canonical volume index.
    /// The exclusive cache borrow permits a Send future without sharing SQLite.
    pub async fn build_node_rename_volume_guard(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        projection: &mut NodeRenameProjectionSpool,
        volume: Uuid,
        max_database_bytes: u64,
    ) -> Result<NodeRenameVolumeGuard> {
        projection.require_manifest(&manifest.request, &manifest.header.summary)?;
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        let baseline = revalidate_at(&tr, route, owner, &manifest.request).await?;
        let binding = live_guard::binding_at(&tr, &manifest.request, volume).await?;
        let source_count = projection.volume_file_count(volume)?;
        let target_count = matches!(&baseline.target,Some(ResolvedPath::File { copy,.. }) if copy.volume_id==volume)
            as u64;
        drop(tr);
        let mut guard = live_guard::NodeRenameVolumeGuard::new(
            &manifest.request,
            manifest.header.summary.sha256,
            binding.clone(),
            max_database_bytes,
        )?;
        let mut after = None;
        loop {
            let tr = self.db.create_trx()?;
            node_tasks::require_rename_held_at(
                &tr,
                route,
                owner,
                &manifest.task,
                &manifest.request,
            )
            .await?;
            require_header_at(&tr, &manifest.task, &manifest.header).await?;
            if live_guard::binding_at(&tr, &manifest.request, volume).await? != binding {
                return Err(invalid("rename 卷引用防护的介质基线已改变"));
            }
            let page = lookup::list_active_volume_copies_at(
                &tr,
                volume,
                binding.scan,
                after.as_deref(),
                32,
            )
            .await?;
            let mut files = Vec::new();
            for row in &page.items {
                let Some((head, copy)) =
                    live_guard::current_at(&tr, manifest.task.namespace, row, &binding).await?
                else {
                    continue;
                };
                let role = if let Some(source) = projection.source_file(row.file_id)? {
                    let ResolvedPath::File {
                        head: expected,
                        copy: expected_copy,
                        placement,
                        ..
                    } = &source.node
                    else {
                        unreachable!()
                    };
                    if expected != &head
                        || expected_copy.as_ref() != &copy
                        || placement != &binding.placement
                    {
                        return Err(CatalogError::Corrupt("rename 源文件与原清单的当前副本不符"));
                    }
                    NodeRenameReferenceRole::Source
                } else if let Some(ResolvedPath::File {
                    id,
                    head: expected,
                    copy: expected_copy,
                    ..
                }) = &baseline.target
                    && *id == row.file_id
                {
                    if expected != &head || expected_copy.as_ref() != &copy {
                        return Err(CatalogError::Corrupt("rename 原替换目标的当前副本不符"));
                    }
                    NodeRenameReferenceRole::Target
                } else {
                    NodeRenameReferenceRole::Other
                };
                files.push((
                    NodeRenameLiveFile {
                        node: NodeRenameRevision {
                            namespace: row.namespace,
                            inode: row.file_id,
                            version: head.version,
                        },
                        copy_id: head.copy_id,
                        metadata_uid: copy
                            .media
                            .as_ref()
                            .map_or(copy.ltfs_file_uid, |m| m.logical_file_uid),
                        data_uid: (!copy.media.as_ref().is_some_and(|m| m.native_symlink))
                            .then_some(copy.ltfs_file_uid),
                        role,
                    },
                    copy,
                ));
            }
            drop(tr);
            guard.append(&files)?;
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        guard.seal(source_count, target_count)?;
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        revalidate_at(&tr, route, owner, &manifest.request).await?;
        if live_guard::binding_at(&tr, &manifest.request, volume).await? != binding {
            return Err(invalid("rename 卷引用防护的介质基线已改变"));
        }
        Ok(guard)
    }
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;

async fn projection_context(
    catalog: &ManagedCatalog,
    route: ActiveLibraryRoute,
    owner: u64,
    manifest: &NodeRenameManifest,
    binding: &live_guard::Binding,
) -> Result<()> {
    let tr = catalog.db.create_trx()?;
    node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
        .await?;
    require_header_at(&tr, &manifest.task, &manifest.header).await?;
    if live_guard::binding_at(&tr, &manifest.request, binding.source.volume_uuid).await? != *binding
    {
        return Err(invalid("rename 按卷投影的原介质绑定已改变"));
    }
    Ok(())
}
fn row_attributes(row: &NodeRenameSubtreeEntry) -> Result<NodeAttributes> {
    let (ResolvedPath::File { attributes, .. } | ResolvedPath::Directory { attributes, .. }) =
        &row.node;
    let attrs = attributes.as_ref().ok_or_else(corrupt)?;
    if attrs.manifest().is_some_and(|m| m.len as usize > 8 << 20)
        || attrs.bytes().is_some_and(|b| b.len() > 8 << 20)
    {
        return Err(invalid("rename 投影单节点属性超过8MiB预算"));
    }
    Ok(attrs.clone())
}
impl ManagedCatalog {
    /// Build desired complete physical rows from the original canonical nodes.
    /// This is neither a durable physical attempt nor permission to stage I/O.
    pub async fn build_node_rename_volume_projection(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        projection: &mut NodeRenameProjectionSpool,
        source: &mut NodeRenamePhysicalSource,
        max_database_bytes: u64,
    ) -> Result<NodeRenameVolumeProjection> {
        projection.require_manifest(&manifest.request, &manifest.header.summary)?;
        let volume = source.source_index().volume_uuid;
        let mut guard = self
            .build_node_rename_volume_guard(
                route,
                owner,
                manifest,
                projection,
                volume,
                max_database_bytes,
            )
            .await?;
        source.require_original(&manifest.request, manifest.header.summary.sha256, &guard)?;
        let binding = guard.original_projection_binding().2;
        let expected_files = guard.stats().source_files;
        if expected_files == 0
            && guard.stats().target_files == 0
            && manifest.header.summary.files > 0
        {
            return Err(invalid("该卷不属于原 rename 源文件或替换目标的参与卷"));
        }
        let inputs = self
            .capture_held_node_rename_inputs(
                route,
                owner,
                (
                    manifest.request.library(),
                    manifest.request.request_epoch(),
                    manifest.request.spec().operation,
                ),
                8 << 20,
            )
            .await?;
        let initial: [u8; 32] = subtree::initial_hasher(&inputs)?.finalize().into();
        if initial != manifest.header.initial_rows_sha256 {
            return Err(corrupt());
        }
        let highest = source.highest_uid();
        let mut builder = volume_projection::Builder::new(
            manifest.request.clone(),
            source,
            &mut guard,
            highest,
            max_database_bytes,
        )?;
        for chain in [&inputs.source_ancestors, &inputs.destination_ancestors] {
            for ancestor in chain {
                let changed = ancestor.id == inputs.baseline.source_parent.id
                    || ancestor.id == inputs.baseline.destination_parent.id;
                builder.ancestor(ancestor, changed)?;
            }
        }
        builder.target(&inputs.baseline)?;
        let namespace = manifest.task.namespace;
        let mut after = None;
        loop {
            projection_context(self, route, owner, manifest, &binding).await?;
            let page = projection.directories_page(after.as_deref(), 32, 8 << 20)?;
            for row in page.items {
                let attributes = self
                    .hydrate_attributes(namespace, row_attributes(&row)?)
                    .await?;
                builder.directory(&row, attributes)?;
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        if expected_files > 0 {
            let mut after = None;
            loop {
                projection_context(self, route, owner, manifest, &binding).await?;
                let page = projection.volume_files_page(volume, after.as_deref(), 32, 8 << 20)?;
                for row in page.items {
                    let attributes = self
                        .hydrate_attributes(namespace, row_attributes(&row)?)
                        .await?;
                    builder.file(&row, attributes)?;
                }
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        let result = builder.finish(expected_files, manifest.header.summary.directories)?;
        projection_context(self, route, owner, manifest, &binding).await?;
        inputs::require_held_inputs_at(&self.db.create_trx()?, route, owner, &inputs).await?;
        Ok(result)
    }
}

fn volume_plan_key(task: &NodeMutationTask, volume: Uuid) -> Vec<u8> {
    let mut key = prefix(task);
    key.push(2);
    key.extend_from_slice(volume.as_bytes());
    key
}
fn require_volume_plan_scope(
    record: &volume_plan::Record,
    manifest: &NodeRenameManifest,
    owner: u64,
) -> Result<()> {
    record.facts.require_scope(
        &manifest.request,
        &manifest.header.request,
        manifest.header.summary.sha256,
        manifest.header.initial_rows_sha256,
        manifest.header.prepared_owner,
        manifest.header.prepared_time,
    )?;
    if record.creator > owner {
        return Err(CatalogError::Corrupt("rename 按卷计划创建者超出当前 owner"));
    }
    Ok(())
}
impl ManagedCatalog {
    /// Save the exact original recipe. Retrying keeps its original creator and
    /// fingerprint; the record itself does not admit any physical operation.
    pub async fn save_node_rename_volume_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        projection: &mut NodeRenameVolumeProjection,
    ) -> Result<NodeRenameVolumePlan> {
        if projection.original_request() != &manifest.request {
            return Err(invalid("rename 按卷投影不属于原请求"));
        }
        let facts = volume_plan::Facts::from_projection(
            projection,
            &manifest.header.request,
            manifest.header.summary.sha256,
            manifest.header.initial_rows_sha256,
            manifest.header.prepared_owner,
            manifest.header.prepared_time,
        )?;
        let binding = facts.binding()?;
        let key = volume_plan_key(&manifest.task, binding.source.volume_uuid);
        let record = self
            .db
            .run(|tr, _| {
                let facts = facts.clone();
                let binding = binding.clone();
                let key = key.clone();
                async move {
                    requests::retry_result(
                        async {
                            node_tasks::require_rename_prepared_at(
                                &tr,
                                route,
                                owner,
                                &manifest.task,
                                &manifest.request,
                            )
                            .await?;
                            require_header_at(&tr, &manifest.task, &manifest.header).await?;
                            if let Some(raw) = tr.get(&key, false).await? {
                                let saved = volume_plan::Record::decode(&raw)?;
                                require_volume_plan_scope(&saved, manifest, owner)?;
                                if saved.facts != facts {
                                    return Err(invalid("rename 原按卷计划与本次投影不符"));
                                }
                                return Ok(saved);
                            }
                            participants::require_unsealed_at(&tr, manifest).await?;
                            participants::require_unreferenced_recipe_at(
                                &tr,
                                manifest,
                                owner,
                                binding.source.volume_uuid,
                            )
                            .await?;
                            revalidate_at(&tr, route, owner, &manifest.request).await?;
                            if live_guard::binding_at(
                                &tr,
                                &manifest.request,
                                binding.source.volume_uuid,
                            )
                            .await?
                                != binding
                            {
                                return Err(invalid("rename 按卷计划的原介质绑定已改变"));
                            }
                            let record = volume_plan::Record::new(facts, owner)?;
                            let raw = record.encode()?;
                            if raw.len() > 64_000 {
                                return Err(invalid("rename 按卷计划描述符超过预算"));
                            }
                            tr.set(&key, &raw);
                            Ok(record)
                        }
                        .await,
                    )
                }
            })
            .await??;
        Ok(NodeRenameVolumePlan { record })
    }
    /// Load only the associated original record; absence and corruption never
    /// cause a descriptor to be rebuilt or substituted by a current pathname.
    pub async fn node_rename_volume_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        volume: Uuid,
    ) -> Result<Option<NodeRenameVolumePlan>> {
        if volume.is_nil() {
            return Err(invalid("rename 计划卷 UUID 为空"));
        }
        let tr = self.db.create_trx()?;
        node_tasks::require_rename_held_at(&tr, route, owner, &manifest.task, &manifest.request)
            .await?;
        require_header_at(&tr, &manifest.task, &manifest.header).await?;
        let Some(raw) = tr
            .get(&volume_plan_key(&manifest.task, volume), false)
            .await?
        else {
            participants::require_unreferenced_recipe_at(&tr, manifest, owner, volume).await?;
            return Ok(None);
        };
        let record = volume_plan::Record::decode(&raw)?;
        require_volume_plan_scope(&record, manifest, owner)?;
        if record.facts.binding()?.source.volume_uuid != volume {
            return Err(CatalogError::Corrupt("rename 按卷计划的卷索引不符"));
        }
        Ok(Some(NodeRenameVolumePlan { record }))
    }
}

#[path = "participants.rs"]
mod participants;
pub use participants::{
    NodeRenameGlobalPlan, NodeRenameParticipant, NodeRenameParticipantsCapture,
    NodeRenameParticipantsSummary,
};

pub use participants::{NodeRenameAttempt, NodeRenameAttemptPhase, NodeRenameVolumeIntent};
pub(in crate::managed_catalog) use participants::{
    require_no_prepared_attempt_at, require_prepared_volume_claim_at,
};

pub(in crate::managed_catalog) use participants::require_no_missing_prepared_claim_at;

pub(in crate::managed_catalog) use participants::require_writing_task_at;

pub use participants::NodeRenameObservation;
pub(in crate::managed_catalog) use participants::require_rename_observation_at;

pub use participants::NodeRenameIntentClosure;
pub(in crate::managed_catalog) use participants::require_rename_discard_at;

pub use participants::NodeRenamePublication;
pub(in crate::managed_catalog) use participants::require_rename_task_outcome_at;
