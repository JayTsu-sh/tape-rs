//! Verify a frozen namespace without loading its directory tree into memory.

use super::*;
use sha2::{Digest, Sha256};

const MAX_VERIFY_PAGE: usize = 100;
const DOMAIN: &[u8] = b"tape-rs/namespace-summary/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceSummary {
    /// Includes the canonical root and empty directories.
    pub nodes: u64,
    pub files: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NamespacePhase {
    Nodes = 0,
    Children = 1,
    Heads = 2,
    Copies = 3,
    Reachability = 4,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceVerificationOutcome {
    Frozen,
    Progress {
        phase: NamespacePhase,
        actual: NamespaceSummary,
    },
    Verified(NamespaceSummary),
    Mismatch {
        expected: NamespaceSummary,
        actual: NamespaceSummary,
    },
    NotVerifying,
    ExpectationConflict,
}

/// Stream the independent source namespace in ascending inode UUID byte order.
/// Volume scan verification separately proves the physical copy manifests.
pub struct NamespaceDigest {
    summary: NamespaceSummary,
    last: Option<Uuid>,
}

impl NamespaceDigest {
    pub fn new(namespace: Uuid) -> Result<Self> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace ID 不能为 nil"));
        }
        let mut hash = Sha256::new();
        hash.update(DOMAIN);
        hash.update(namespace.as_bytes());
        Ok(Self {
            summary: NamespaceSummary {
                nodes: 0,
                files: 0,
                digest: hash.finalize().into(),
            },
            last: None,
        })
    }

    pub fn push(&mut self, id: Uuid, node: &Node, head: Option<&FileHead>) -> Result<()> {
        validate_shape(id, node)?;
        if self.last.is_some_and(|last| last >= id) {
            return Err(CatalogError::Invalid(
                "命名空间摘要必须按 inode 排序且无重复",
            ));
        }
        let head = match (node.kind, head) {
            (NodeKind::Directory, None) => Vec::new(),
            (NodeKind::File, Some(head)) => {
                let bytes = encode_head(head);
                decode_head(&bytes)?;
                bytes
            }
            _ => return Err(CatalogError::Invalid("inode 类型与文件 head 不一致")),
        };
        self.summary = fold(self.summary, id, &encode_node(node), &head)?;
        self.last = Some(id);
        Ok(())
    }

    pub fn summary(&self) -> NamespaceSummary {
        self.summary
    }
}

fn validate_shape(id: Uuid, node: &Node) -> Result<()> {
    if node.kind == NodeKind::Directory
        && node
            .attributes
            .as_ref()
            .is_some_and(|attributes| attributes.symlink().is_some())
    {
        return Err(CatalogError::Invalid("目录不能带符号链接属性"));
    }
    if id.is_nil() {
        if node.kind != NodeKind::Directory || !node.parent.is_nil() || !node.name.is_empty() {
            return Err(CatalogError::Corrupt("命名空间根 inode 非规范"));
        }
    } else if node.parent == id || validate_name(&node.name).is_err() {
        return Err(CatalogError::Corrupt("inode 名称或父引用无效"));
    }
    Ok(())
}

fn fold(
    previous: NamespaceSummary,
    id: Uuid,
    node: &[u8],
    head: &[u8],
) -> Result<NamespaceSummary> {
    let nodes = previous
        .nodes
        .checked_add(1)
        .ok_or(CatalogError::Corrupt("inode 计数溢出"))?;
    let files = previous
        .files
        .checked_add(u64::from(!head.is_empty()))
        .ok_or(CatalogError::Corrupt("文件计数溢出"))?;
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    hash.update(previous.digest);
    hash.update(id.as_bytes());
    hash.update((node.len() as u32).to_be_bytes());
    hash.update(node);
    hash.update((head.len() as u32).to_be_bytes());
    hash.update(head);
    Ok(NamespaceSummary {
        nodes,
        files,
        digest: hash.finalize().into(),
    })
}

#[derive(Clone)]
struct Progress {
    expected: NamespaceSummary,
    actual: NamespaceSummary,
    phase: NamespacePhase,
    cursor: Option<Vec<u8>>,
    stack: Vec<Uuid>,
    walk: Option<Uuid>,
}

fn phase_prefix(namespace: Uuid, phase: NamespacePhase) -> Vec<u8> {
    let mut prefix = key(match phase {
        NamespacePhase::Nodes | NamespacePhase::Reachability => b'N',
        NamespacePhase::Children => b'C',
        NamespacePhase::Heads => b'H',
        NamespacePhase::Copies => b'R',
    });
    prefix.extend_from_slice(namespace.as_bytes());
    prefix
}

fn progress_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'Z');
    out.extend_from_slice(namespace.as_bytes());
    out
}

fn encode_progress(progress: &Progress) -> Vec<u8> {
    let mut out = Vec::new();
    for summary in [progress.expected, progress.actual] {
        out.extend_from_slice(&summary.nodes.to_be_bytes());
        out.extend_from_slice(&summary.files.to_be_bytes());
        out.extend_from_slice(&summary.digest);
    }
    out.push(progress.phase as u8);
    let cursor = progress.cursor.as_deref().unwrap_or_default();
    out.extend_from_slice(&(cursor.len() as u16).to_be_bytes());
    out.extend_from_slice(cursor);
    out.extend_from_slice(&(progress.stack.len() as u16).to_be_bytes());
    for id in &progress.stack {
        out.extend_from_slice(id.as_bytes());
    }
    if let Some(id) = progress.walk {
        out.extend_from_slice(id.as_bytes());
    }
    out
}

fn decode_progress(namespace: Uuid, bytes: &[u8]) -> Result<Progress> {
    if bytes.len() < 101 {
        return Err(CatalogError::Corrupt("命名空间校验进度长度"));
    }
    let phase = match bytes[96] {
        0 => NamespacePhase::Nodes,
        1 => NamespacePhase::Children,
        2 => NamespacePhase::Heads,
        3 => NamespacePhase::Copies,
        4 => NamespacePhase::Reachability,
        _ => return Err(CatalogError::Corrupt("命名空间校验阶段")),
    };
    let len = u16::from_be_bytes([bytes[97], bytes[98]]) as usize;
    if bytes.len() < 101 + len {
        return Err(CatalogError::Corrupt("命名空间校验游标长度"));
    }
    let cursor = if len == 0 {
        None
    } else {
        let prefix = phase_prefix(namespace, phase);
        let key = &bytes[99..99 + len];
        if !key.starts_with(&prefix) {
            return Err(CatalogError::Corrupt("命名空间校验游标范围"));
        }
        validate_suffix(phase, &key[prefix.len()..])?;
        Some(key.to_vec())
    };
    let stack_len = u16::from_be_bytes([bytes[99 + len], bytes[100 + len]]) as usize;
    let expected_len = 101 + len + stack_len * 16 + usize::from(stack_len > 0) * 16;
    if stack_len > MAX_DEPTH as usize
        || bytes.len() != expected_len
        || (stack_len > 0 && phase != NamespacePhase::Reachability)
    {
        return Err(CatalogError::Corrupt("命名空间连通性校验栈"));
    }
    let mut stack = Vec::with_capacity(stack_len);
    for bytes in bytes[101 + len..101 + len + stack_len * 16]
        .as_chunks::<16>()
        .0
    {
        stack.push(uuid_from(bytes)?);
    }
    let walk = if stack_len == 0 {
        None
    } else {
        Some(uuid_from(&bytes[expected_len - 16..])?)
    };
    let decode = |bytes: &[u8]| -> Result<NamespaceSummary> {
        let nodes = read_u64(bytes);
        let files = read_u64(&bytes[8..]);
        if files > nodes {
            return Err(CatalogError::Corrupt("文件计数超过 inode 计数"));
        }
        Ok(NamespaceSummary {
            nodes,
            files,
            digest: bytes[16..48].try_into().expect("fixed length"),
        })
    };
    Ok(Progress {
        expected: decode(&bytes[..48])?,
        actual: decode(&bytes[48..96])?,
        phase,
        cursor,
        stack,
        walk,
    })
}

pub(super) async fn require_verified_summary(
    tr: &Transaction,
    namespace: Uuid,
    expected: NamespaceSummary,
) -> Result<()> {
    let bytes = tr
        .get(&progress_key(namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let progress = decode_progress(namespace, &bytes)?;
    if progress.phase != NamespacePhase::Reachability
        || progress.expected != expected
        || progress.actual != expected
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

fn validate_suffix(phase: NamespacePhase, suffix: &[u8]) -> Result<()> {
    match phase {
        NamespacePhase::Nodes | NamespacePhase::Heads | NamespacePhase::Reachability
            if suffix.len() == 16 =>
        {
            Ok(())
        }
        NamespacePhase::Copies if suffix.len() == 32 => Ok(()),
        NamespacePhase::Children if (19..=16 + 2 * MAX_NAME + 2).contains(&suffix.len()) => {
            let name = decode_name(&suffix[16..])?;
            validate_name(&name).map_err(|_| CatalogError::Corrupt("目录项名称"))
        }
        _ => Err(CatalogError::Corrupt("命名空间索引键长度")),
    }
}

fn mismatch(progress: &Progress) -> NamespaceVerificationOutcome {
    NamespaceVerificationOutcome::Mismatch {
        expected: progress.expected,
        actual: progress.actual,
    }
}

fn required_node(bytes: Option<&[u8]>) -> Result<Node> {
    decode_node(bytes.ok_or(CatalogError::Corrupt("inode 引用缺失"))?)
}

fn require_current_copy_matches_head(head: &FileHead, copy: &PhysicalCopy) -> Result<()> {
    if copy.version != head.version || copy.size != head.size {
        return Err(CatalogError::Corrupt(
            "文件 head 与当前副本版本或长度不一致",
        ));
    }
    Ok(())
}

async fn verify_nodes(
    tr: &Transaction,
    namespace: Uuid,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    mut actual: NamespaceSummary,
    shapes: &std::collections::HashMap<[u8; 32], super::attribute_objects::VerifiedShape>,
) -> Result<NamespaceSummary> {
    let prefix = phase_prefix(namespace, NamespacePhase::Nodes);
    let mut checks = Vec::with_capacity(rows.len());
    for (key, raw) in rows {
        let suffix = &key[prefix.len()..];
        validate_suffix(NamespacePhase::Nodes, suffix)?;
        let id = uuid_from(suffix)?;
        let node = decode_node(&raw)?;
        if let Some(manifest) = node.attributes.as_ref().and_then(NodeAttributes::manifest) {
            super::attribute_objects::require_complete(tr, namespace, manifest).await?;
            let shape = shapes
                .get(&manifest.digest)
                .ok_or(CatalogError::Incomplete)?;
            if node.kind == NodeKind::Directory && shape.symlink {
                return Err(CatalogError::Corrupt("目录属性对象包含符号链接"));
            }
        }
        validate_shape(id, &node)?;
        checks.push((
            id,
            raw,
            node.clone(),
            tr.get(&node_key(namespace, node.parent), false),
            tr.get(&child_key(namespace, node.parent, &node.name), false),
            tr.get(&head_key(namespace, id), false),
        ));
    }
    let mut copies = Vec::with_capacity(checks.len());
    for (id, raw, node, parent, child, head) in checks {
        let parent = parent.await?;
        let child = child.await?;
        let head = head.await?;
        if !id.is_nil() {
            let parent = required_node(parent.as_deref())?;
            if parent.kind != NodeKind::Directory
                || child.as_deref() != Some(id.as_bytes().as_slice())
            {
                return Err(CatalogError::Corrupt("父目录或目录项反查不一致"));
            }
        }
        match node.kind {
            NodeKind::Directory => {
                if head.is_some() {
                    return Err(CatalogError::Corrupt("目录 inode 存在文件 head"));
                }
                copies.push((id, raw, Vec::new(), None));
            }
            NodeKind::File => {
                let head = head.ok_or(CatalogError::Corrupt("文件 head 缺失"))?;
                let decoded = decode_head(&head)?;
                copies.push((
                    id,
                    raw,
                    head.to_vec(),
                    Some(tr.get(&copy_key(namespace, id, decoded.copy_id), false)),
                ));
            }
        }
    }
    for (id, raw, head, copy) in copies {
        if let Some(copy) = copy {
            let copy = copy
                .await?
                .ok_or(CatalogError::Corrupt("文件当前副本缺失"))?;
            let head = decode_head(&head)?;
            let copy = decode_copy(&copy)?;
            require_current_copy_matches_head(&head, &copy)?;
            if copy
                .media
                .as_ref()
                .is_some_and(|media| media.native_symlink)
            {
                let node = decode_node(&raw)?;
                let symlink = node.attributes.as_ref().is_some_and(|attributes| {
                    attributes.manifest().map_or_else(
                        || attributes.symlink().is_some(),
                        |manifest| {
                            shapes
                                .get(&manifest.digest)
                                .is_some_and(|shape| shape.symlink)
                        },
                    )
                });
                if !symlink {
                    return Err(CatalogError::Corrupt("原生符号链接副本缺少链接属性"));
                }
            }
        }
        actual = fold(actual, id, &raw, &head)?;
    }
    Ok(actual)
}

async fn verify_indexes(
    tr: &Transaction,
    namespace: Uuid,
    phase: NamespacePhase,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    let prefix = phase_prefix(namespace, phase);
    let accepting = if phase == NamespacePhase::Copies {
        Some(requests::window(tr, namespace).await?.accepting)
    } else {
        None
    };
    let mut checks = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let suffix = &key[prefix.len()..];
        validate_suffix(phase, suffix)?;
        let (id, parent) = if phase == NamespacePhase::Children {
            (uuid_from(&value)?, uuid_from(&suffix[..16])?)
        } else {
            (uuid_from(&suffix[..16])?, Uuid::nil())
        };
        if id.is_nil() {
            return Err(CatalogError::Corrupt("索引引用根 inode"));
        }
        checks.push((
            suffix.to_vec(),
            value,
            id,
            tr.get(&node_key(namespace, id), false),
            tr.get(&node_key(namespace, parent), false),
            tr.get(&head_key(namespace, id), false),
        ));
    }
    let mut physical = Vec::with_capacity(checks.len());
    for (suffix, value, id, node, parent, head) in checks {
        let node = required_node(node.await?.as_deref())?;
        let parent = parent.await?;
        let head = head.await?;
        match phase {
            NamespacePhase::Children => {
                let parent = required_node(parent.as_deref())?;
                if node.parent != uuid_from(&suffix[..16])?
                    || node.name != decode_name(&suffix[16..])?
                    || parent.kind != NodeKind::Directory
                {
                    return Err(CatalogError::Corrupt("目录项与 inode 或父目录不一致"));
                }
            }
            NamespacePhase::Heads => {
                decode_head(&value)?;
                if node.kind != NodeKind::File {
                    return Err(CatalogError::Corrupt("文件 head 指向目录"));
                }
            }
            NamespacePhase::Copies => {
                if uuid_from(&suffix[16..])?.is_nil() {
                    return Err(CatalogError::Corrupt("副本 ID 不能为 nil"));
                }
                let stored = decode_stored_copy(&value)?;
                if accepting.is_some_and(|epoch| stored.request_epoch() > epoch) {
                    return Err(CatalogError::Corrupt("副本请求代次未开放"));
                }
                let head = decode_head(
                    head.as_deref()
                        .ok_or(CatalogError::Corrupt("副本文件 head 缺失"))?,
                )?;
                if node.kind != NodeKind::File || stored.version() > head.version {
                    return Err(CatalogError::Corrupt("副本引用或版本不一致"));
                }
                let copy = match stored {
                    StoredCopy::Present(copy) => copy,
                    StoredCopy::Reclaimed { version, .. } => {
                        if version >= head.version || uuid_from(&suffix[16..])? == head.copy_id {
                            return Err(CatalogError::Corrupt("当前副本或版本被错误回收"));
                        }
                        continue;
                    }
                };
                if uuid_from(&suffix[16..])? == head.copy_id {
                    require_current_copy_matches_head(&head, &copy)?;
                }
                let registry = tr.get(&scan_key(copy.volume_id, copy.volume_scan_id), false);
                let reverse = tr.get(
                    &volume_key(
                        copy.volume_id,
                        copy.volume_scan_id,
                        namespace,
                        id,
                        uuid_from(&suffix[16..])?,
                    ),
                    false,
                );
                physical.push((copy, registry, reverse));
            }
            NamespacePhase::Nodes | NamespacePhase::Reachability => {
                unreachable!("separate verifier")
            }
        }
        validate_shape(id, &node)?;
    }
    for (copy, registry, reverse) in physical {
        let registry = registry
            .await?
            .ok_or(CatalogError::Corrupt("副本卷扫描记录缺失"))?;
        let (scan, _, state) = decode_scan_manifest(&registry)?;
        if scan.generation != copy.volume_generation || scan.id != copy.volume_scan_id {
            return Err(CatalogError::Corrupt("副本与卷扫描身份不一致"));
        }
        let reverse = reverse.await?;
        match reverse {
            Some(value) if value.len() == 40 && read_u64(&value) == copy.version => {}
            None if state == ScanState::Retired => {}
            _ => return Err(CatalogError::Corrupt("副本卷反向索引缺失或版本不一致")),
        }
    }
    Ok(())
}

fn proof_prefix(namespace: Uuid) -> Vec<u8> {
    let mut key = super::key(b'L');
    key.extend_from_slice(namespace.as_bytes());
    key
}

fn proof_key(namespace: Uuid, id: Uuid) -> Vec<u8> {
    let mut key = proof_prefix(namespace);
    key.extend_from_slice(id.as_bytes());
    key
}

fn decode_proof(id: Uuid, bytes: &[u8]) -> Result<u16> {
    if bytes.len() != 2 {
        return Err(CatalogError::Corrupt("连通性证明长度"));
    }
    let depth = u16::from_be_bytes([bytes[0], bytes[1]]);
    if depth > MAX_DEPTH || (id.is_nil() && depth != 0) || (!id.is_nil() && depth == 0) {
        return Err(CatalogError::Corrupt("连通性证明深度"));
    }
    Ok(depth)
}

async fn finish_walk(
    tr: &Transaction,
    namespace: Uuid,
    progress: &mut Progress,
    budget: &mut usize,
) -> Result<bool> {
    while !progress.stack.is_empty() && *budget > 0 {
        *budget -= 1;
        let id = progress
            .walk
            .ok_or(CatalogError::Corrupt("连通性校验父链缺失"))?;
        if let Some(bytes) = tr.get(&proof_key(namespace, id), false).await? {
            let mut depth = decode_proof(id, &bytes)?;
            if depth as usize + progress.stack.len() > MAX_DEPTH as usize {
                return Err(CatalogError::Corrupt("父链超过 1024 层"));
            }
            for id in progress.stack.iter().rev() {
                depth += 1;
                tr.set(&proof_key(namespace, *id), &depth.to_be_bytes());
            }
            progress.cursor = Some(node_key(namespace, progress.stack[0]));
            progress.stack.clear();
            progress.walk = None;
        } else {
            if progress.stack.contains(&id) {
                return Err(CatalogError::Corrupt("目录父链存在环"));
            }
            if progress.stack.len() >= MAX_DEPTH as usize {
                return Err(CatalogError::Corrupt("父链超过 1024 层"));
            }
            let bytes = tr.get(&node_key(namespace, id), false).await?;
            let node = required_node(bytes.as_deref())?;
            if node.kind != NodeKind::Directory {
                return Err(CatalogError::Corrupt("父链指向文件"));
            }
            progress.stack.push(id);
            progress.walk = Some(node.parent);
        }
    }
    Ok(progress.stack.is_empty())
}

async fn verify_reachability(
    tr: &Transaction,
    namespace: Uuid,
    progress: &mut Progress,
    mut budget: usize,
) -> Result<bool> {
    if !finish_walk(tr, namespace, progress, &mut budget).await? || budget == 0 {
        return Ok(false);
    }
    let prefix = phase_prefix(namespace, NamespacePhase::Nodes);
    let page = read_page(tr, &prefix, progress.cursor.as_deref(), budget, false).await?;
    let mut lookups = Vec::with_capacity(page.items.len());
    for (key, raw) in page.items {
        let id = uuid_from(&key[prefix.len()..])?;
        let node = decode_node(&raw)?;
        lookups.push((
            key,
            id,
            node.parent,
            tr.get(&proof_key(namespace, id), false),
            tr.get(&proof_key(namespace, node.parent), false),
        ));
    }
    for (key, id, parent, own_proof, parent_proof) in lookups {
        if budget == 0 {
            return Ok(false);
        }
        budget -= 1;
        if let Some(proof) = own_proof.await? {
            decode_proof(id, &proof)?;
            progress.cursor = Some(key);
        } else if let Some(proof) = parent_proof.await? {
            let depth = decode_proof(parent, &proof)?;
            if depth >= MAX_DEPTH {
                return Err(CatalogError::Corrupt("父链超过 1024 层"));
            }
            tr.set(&proof_key(namespace, id), &(depth + 1).to_be_bytes());
            progress.cursor = Some(key);
        } else {
            progress.stack.push(id);
            progress.walk = Some(parent);
            if !finish_walk(tr, namespace, progress, &mut budget).await? {
                return Ok(false);
            }
        }
    }
    Ok(page.next.is_none())
}

impl ManagedCatalog {
    /// Freeze all logical namespace mutations and bind an independent source
    /// digest. Volume availability is checked separately when selecting a copy.
    pub async fn freeze_namespace(
        &self,
        namespace: Uuid,
        expected: NamespaceSummary,
    ) -> Result<NamespaceVerificationOutcome> {
        if expected.nodes == 0 || expected.files >= expected.nodes {
            return Err(CatalogError::Invalid("命名空间期望必须包含根目录"));
        }
        let initial = NamespaceDigest::new(namespace)?.summary();
        self.db
            .run(|tr, _| async move {
                let state_key = namespace_key(namespace);
                let state = tr.get(&state_key, false).await?;
                let state = match state.as_deref().map(decode_namespace_state).transpose() {
                    Ok(Some(state)) => state,
                    Ok(None) => return Ok(Err(CatalogError::Incomplete)),
                    Err(error) => return Ok(Err(error)),
                };
                let progress_key = progress_key(namespace);
                if state == NamespaceState::Building {
                    tr.set(&state_key, &[NamespaceState::Verifying as u8]);
                    tr.set(&proof_key(namespace, Uuid::nil()), &0u16.to_be_bytes());
                    tr.set(
                        &progress_key,
                        &encode_progress(&Progress {
                            expected,
                            actual: initial,
                            phase: NamespacePhase::Nodes,
                            cursor: None,
                            stack: Vec::new(),
                            walk: None,
                        }),
                    );
                    return Ok(Ok(NamespaceVerificationOutcome::Frozen));
                }
                if matches!(
                    state,
                    NamespaceState::Ready | NamespaceState::Active | NamespaceState::Discarded
                ) {
                    return Ok(Ok(NamespaceVerificationOutcome::NotVerifying));
                }
                let progress = tr.get(&progress_key, false).await?;
                let progress = match progress
                    .as_deref()
                    .map(|bytes| decode_progress(namespace, bytes))
                    .transpose()
                {
                    Ok(Some(progress)) => progress,
                    Ok(None) => return Ok(Err(CatalogError::Corrupt("命名空间校验进度缺失"))),
                    Err(error) => return Ok(Err(error)),
                };
                if progress.expected != expected {
                    return Ok(Ok(NamespaceVerificationOutcome::ExpectationConflict));
                }
                Ok(Ok(match state {
                    NamespaceState::Verified => {
                        NamespaceVerificationOutcome::Verified(progress.actual)
                    }
                    NamespaceState::Rejected => mismatch(&progress),
                    _ => NamespaceVerificationOutcome::Frozen,
                }))
            })
            .await?
    }

    pub async fn verify_namespace_page(
        &self,
        namespace: Uuid,
        limit: usize,
    ) -> Result<NamespaceVerificationOutcome> {
        if namespace.is_nil() || !(1..=MAX_VERIFY_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid(
                "namespace 必须有效，校验批次必须在 1..=100",
            ));
        }
        // Verify immutable objects outside the state/progress write transaction.
        // Each object uses short chunk reads and a bounded streaming decoder.
        // A concurrent verifier may advance the cursor; compare it below before
        // using these proofs, so this pass cannot accidentally skip an inode.
        let mut shapes = std::collections::HashMap::new();
        let mut verified_cursor = None;
        let tr = self.db.create_trx()?;
        let state = tr.get(&namespace_key(namespace), true).await?;
        if state.as_deref() == Some(&[NamespaceState::Verifying as u8]) {
            let raw = tr
                .get(&progress_key(namespace), true)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            let progress = decode_progress(namespace, &raw)?;
            if progress.phase == NamespacePhase::Nodes {
                verified_cursor = Some(progress.cursor.clone());
                let prefix = phase_prefix(namespace, NamespacePhase::Nodes);
                let page = read_page(&tr, &prefix, progress.cursor.as_deref(), limit, true).await?;
                let mut manifests = Vec::new();
                for (_, raw) in page.items {
                    if let Some(manifest) = decode_node(&raw)?
                        .attributes
                        .as_ref()
                        .and_then(NodeAttributes::manifest)
                    {
                        manifests.push(manifest);
                    }
                }
                drop(tr);
                for manifest in manifests {
                    let shape = self.verify_attribute_object(namespace, manifest).await?;
                    shapes.insert(manifest.digest, shape);
                }
            }
        }
        let shapes = &shapes;
        let verified_cursor = &verified_cursor;
        self.db
            .run(|tr, _| async move {
                let state_key = namespace_key(namespace);
                let state = tr.get(&state_key, false).await?;
                let state = match state.as_deref().map(decode_namespace_state).transpose() {
                    Ok(Some(state)) => state,
                    Ok(None) => return Ok(Err(CatalogError::Incomplete)),
                    Err(error) => return Ok(Err(error)),
                };
                if matches!(
                    state,
                    NamespaceState::Building
                        | NamespaceState::Ready
                        | NamespaceState::Active
                        | NamespaceState::Discarded
                ) {
                    return Ok(Ok(NamespaceVerificationOutcome::NotVerifying));
                }
                let progress_key = progress_key(namespace);
                let bytes = tr.get(&progress_key, false).await?;
                let mut progress = match bytes
                    .as_deref()
                    .map(|bytes| decode_progress(namespace, bytes))
                    .transpose()
                {
                    Ok(Some(progress)) => progress,
                    Ok(None) => return Ok(Err(CatalogError::Corrupt("命名空间校验进度缺失"))),
                    Err(error) => return Ok(Err(error)),
                };
                if state == NamespaceState::Verified {
                    return Ok(Ok(NamespaceVerificationOutcome::Verified(progress.actual)));
                }
                if state == NamespaceState::Rejected {
                    return Ok(Ok(mismatch(&progress)));
                }
                if progress.phase == NamespacePhase::Reachability {
                    let done = match verify_reachability(&tr, namespace, &mut progress, limit).await
                    {
                        Ok(done) => done,
                        Err(CatalogError::Fdb(error)) => return Err(error),
                        Err(CatalogError::Client(error)) => return Err(error.into()),
                        Err(error) => return Ok(Err(error)),
                    };
                    tr.set(&progress_key, &encode_progress(&progress));
                    if done {
                        tr.set(&state_key, &[NamespaceState::Verified as u8]);
                        return Ok(Ok(NamespaceVerificationOutcome::Verified(progress.actual)));
                    }
                    return Ok(Ok(NamespaceVerificationOutcome::Progress {
                        phase: progress.phase,
                        actual: progress.actual,
                    }));
                }
                let prefix = phase_prefix(namespace, progress.phase);
                let page =
                    read_page(&tr, &prefix, progress.cursor.as_deref(), limit, false).await?;
                if progress.phase == NamespacePhase::Nodes
                    && verified_cursor.as_ref() != Some(&progress.cursor)
                {
                    return Ok(Ok(NamespaceVerificationOutcome::Progress {
                        phase: progress.phase,
                        actual: progress.actual,
                    }));
                }
                let result = if progress.phase == NamespacePhase::Nodes {
                    verify_nodes(&tr, namespace, page.items, progress.actual, shapes).await
                } else {
                    verify_indexes(&tr, namespace, progress.phase, page.items)
                        .await
                        .map(|()| progress.actual)
                };
                progress.actual = match result {
                    Ok(actual) => actual,
                    Err(CatalogError::Fdb(error)) => return Err(error),
                    Err(CatalogError::Client(error)) => return Err(error.into()),
                    Err(error) => return Ok(Err(error)),
                };
                progress.cursor = page.next;
                if progress.cursor.is_none() {
                    match progress.phase {
                        NamespacePhase::Nodes => {
                            if progress.actual != progress.expected {
                                tr.set(&state_key, &[NamespaceState::Rejected as u8]);
                                tr.set(&progress_key, &encode_progress(&progress));
                                return Ok(Ok(mismatch(&progress)));
                            }
                            progress.phase = NamespacePhase::Children;
                        }
                        NamespacePhase::Children => progress.phase = NamespacePhase::Heads,
                        NamespacePhase::Heads => progress.phase = NamespacePhase::Copies,
                        NamespacePhase::Copies => {
                            progress.phase = NamespacePhase::Reachability;
                        }
                        NamespacePhase::Reachability => unreachable!("handled separately"),
                    }
                }
                tr.set(&progress_key, &encode_progress(&progress));
                Ok(Ok(NamespaceVerificationOutcome::Progress {
                    phase: progress.phase,
                    actual: progress.actual,
                }))
            })
            .await?
    }

    /// Proofs are temporary verification data, never mutable inode metadata.
    /// Delete in bounded batches only after verification can no longer resume.
    /// Once a Ready namespace has no proofs, also reclaim its resumable progress.
    pub async fn purge_namespace_proofs(&self, namespace: Uuid, limit: usize) -> Result<usize> {
        if namespace.is_nil() || !(1..=MAX_PAGE).contains(&limit) {
            return Err(CatalogError::Invalid(
                "namespace 必须有效，清理批次必须在 1..=1000",
            ));
        }
        self.db
            .run(|tr, _| async move {
                let state = tr.get(&namespace_key(namespace), false).await?;
                match state.as_deref().map(decode_namespace_state).transpose() {
                    Ok(Some(
                        NamespaceState::Ready | NamespaceState::Verified | NamespaceState::Rejected,
                    )) => {}
                    Ok(_) => return Ok(Err(CatalogError::Incomplete)),
                    Err(error) => return Ok(Err(error)),
                }
                let page = read_page(&tr, &proof_prefix(namespace), None, limit, false).await?;
                for (key, _) in &page.items {
                    tr.clear(key);
                }
                if page.items.is_empty() && state.as_deref() == Some(&[NamespaceState::Ready as u8])
                {
                    tr.clear(&progress_key(namespace));
                }
                Ok(Ok(page.items.len()))
            })
            .await?
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn current_copy_must_match_head_version_and_length() {
        let head = FileHead {
            version: 2,
            size: u64::MAX,
            copy_id: Uuid::new_v4(),
        };
        let mut copy = PhysicalCopy {
            version: 2,
            request_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 1,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 1,
            size: u64::MAX,
            content_digest: ContentDigest::Unknown,
            media: None,
        };
        require_current_copy_matches_head(&head, &copy).unwrap();
        copy.size -= 1;
        assert!(require_current_copy_matches_head(&head, &copy).is_err());
        copy.size = head.size;
        copy.version -= 1;
        assert!(require_current_copy_matches_head(&head, &copy).is_err());
    }
    use std::collections::BTreeMap;

    pub(crate) async fn check_copy_rows(catalog: &ManagedCatalog, namespace: Uuid) {
        let prefix = phase_prefix(namespace, NamespacePhase::Copies);
        let mut cursor = None;
        loop {
            let tr = catalog.db.create_trx().unwrap();
            let page = read_page(&tr, &prefix, cursor.as_deref(), 100, false)
                .await
                .unwrap();
            verify_indexes(&tr, namespace, NamespacePhase::Copies, page.items)
                .await
                .unwrap();
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
    }

    type Records = BTreeMap<Uuid, (Node, Option<FileHead>)>;

    fn root() -> Node {
        Node {
            kind: NodeKind::Directory,
            parent: Uuid::nil(),
            name: Vec::new(),
            attributes: None,
        }
    }
    fn directory(name: &[u8]) -> Node {
        Node {
            kind: NodeKind::Directory,
            parent: Uuid::nil(),
            name: name.to_vec(),
            attributes: None,
        }
    }
    fn summary(namespace: Uuid, records: &Records) -> NamespaceSummary {
        let mut digest = NamespaceDigest::new(namespace).unwrap();
        for (id, (node, head)) in records {
            digest.push(*id, node, head.as_ref()).unwrap();
        }
        digest.summary()
    }
    fn file(
        serial: &str,
        namespace: Uuid,
        volume: Uuid,
        scan: VolumeScan,
        n: u128,
    ) -> CommitReceipt {
        test_receipt(CommitReceipt {
            request_epoch: 1,
            namespace,
            parent: Uuid::from_u128(1),
            name: format!("file-{n}").into_bytes(),
            file_id: Uuid::from_u128(2000 + n),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            copy_id: Uuid::new_v4(),
            library_serial: serial.into(),
            pool_id: None,
            owner_epoch: 10,
            volume_id: volume,
            volume_generation: scan.generation,
            volume_scan_id: scan.id,
            ltfs_file_uid: 1,
            size: n as u64,
            content_digest: ContentDigest::Sha256([3; 32]),
            metadata_digest: None,
        })
    }
    fn source_file(receipt: &CommitReceipt) -> (Node, Option<FileHead>) {
        (
            Node {
                kind: NodeKind::File,
                parent: receipt.parent,
                name: receipt.name.clone(),
                attributes: None,
            },
            Some(FileHead {
                version: 1,
                size: receipt.size,
                copy_id: receipt.copy_id,
            }),
        )
    }

    #[test]
    fn source_digest_binds_empty_directories_heads_and_order() {
        let namespace = Uuid::new_v4();
        let mut digest = NamespaceDigest::new(namespace).unwrap();
        digest.push(Uuid::nil(), &root(), None).unwrap();
        let before = digest.summary();
        assert!(digest.push(Uuid::nil(), &root(), None).is_err());
        assert_eq!(digest.summary(), before);
        digest
            .push(Uuid::from_u128(1), &directory(b"empty"), None)
            .unwrap();
        assert_eq!(digest.summary().nodes, 2);
        let node = Node {
            kind: NodeKind::File,
            parent: Uuid::nil(),
            name: b"file".to_vec(),
            attributes: None,
        };
        assert!(digest.push(Uuid::from_u128(2), &node, None).is_err());
        let head = FileHead {
            version: 1,
            size: 100,
            copy_id: Uuid::new_v4(),
        };
        digest.push(Uuid::from_u128(2), &node, Some(&head)).unwrap();
        assert_eq!(digest.summary().files, 1);
        let mut other = NamespaceDigest::new(namespace).unwrap();
        other.push(Uuid::nil(), &root(), None).unwrap();
        other
            .push(Uuid::from_u128(1), &directory(b"empty"), None)
            .unwrap();
        let changed = FileHead {
            copy_id: Uuid::new_v4(),
            ..head
        };
        other
            .push(Uuid::from_u128(2), &node, Some(&changed))
            .unwrap();
        assert_ne!(digest.summary(), other.summary());
    }

    #[test]
    fn progress_rejects_truncation_phase_and_foreign_cursor() {
        let namespace = Uuid::new_v4();
        let mut digest = NamespaceDigest::new(namespace).unwrap();
        digest.push(Uuid::nil(), &root(), None).unwrap();
        let progress = Progress {
            expected: digest.summary(),
            actual: digest.summary(),
            phase: NamespacePhase::Nodes,
            cursor: Some(node_key(namespace, Uuid::nil())),
            stack: Vec::new(),
            walk: None,
        };
        let bytes = encode_progress(&progress);
        assert_eq!(
            decode_progress(namespace, &bytes).unwrap().actual,
            digest.summary()
        );
        for end in 0..bytes.len() {
            assert!(decode_progress(namespace, &bytes[..end]).is_err());
        }
        assert!(decode_progress(Uuid::new_v4(), &bytes).is_err());
        let mut invalid = bytes;
        invalid[96] = 5;
        assert!(decode_progress(namespace, &invalid).is_err());
        assert!(decode_head(&[0; 32]).is_err());
        assert!(decode_copy(&[0; 80]).is_err());
        assert_eq!(decode_proof(Uuid::nil(), &[0, 0]).unwrap(), 0);
        assert!(decode_proof(Uuid::nil(), &[0, 1]).is_err());
        assert!(decode_proof(Uuid::from_u128(1), &[0, 0]).is_err());
        assert_eq!(
            decode_proof(Uuid::from_u128(1), &MAX_DEPTH.to_be_bytes()).unwrap(),
            MAX_DEPTH
        );
        assert!(decode_proof(Uuid::from_u128(1), &(MAX_DEPTH + 1).to_be_bytes()).is_err());
    }

    pub(crate) async fn exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let serial = format!("namespace-{}", Uuid::new_v4());
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        let namespace = Uuid::new_v4();
        assert!(catalog.begin_import(namespace).await.unwrap());
        let mut records = Records::from([(Uuid::nil(), (root(), None))]);
        for n in 1..=1001 {
            let id = Uuid::from_u128(n);
            let node = directory(format!("dir-{n}").as_bytes());
            assert_eq!(
                catalog
                    .create_import_directory(namespace, node.parent, &node.name, id)
                    .await
                    .unwrap(),
                DirectoryOutcome::Created
            );
            records.insert(id, (node, None));
        }
        let mut first = None;
        for n in 1..=101 {
            let entry = file(&serial, namespace, volume, scan, n);
            assert_eq!(
                test_publish_imported(catalog, &entry).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            records.insert(entry.file_id, source_file(&entry));
            if n == 1 {
                first = Some(entry);
            }
        }
        let first = first.unwrap();
        let expected = summary(namespace, &records);
        assert_eq!(expected.nodes, 1103);
        assert_eq!(expected.files, 101);
        assert!(matches!(
            catalog.seal_import(namespace).await,
            Err(CatalogError::Incomplete)
        ));
        let in_flight = catalog.db.create_trx().unwrap();
        assert!(
            in_flight
                .get(&namespace_key(namespace), false)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            catalog.freeze_namespace(namespace, expected).await.unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        assert!(matches!(
            catalog.purge_namespace_proofs(namespace, 1000).await,
            Err(CatalogError::Incomplete)
        ));
        in_flight.set(
            &node_key(namespace, Uuid::new_v4()),
            &encode_node(&directory(b"late")),
        );
        assert!(in_flight.commit().await.is_err());
        let mut wrong = expected;
        wrong.nodes += 1;
        assert_eq!(
            catalog.freeze_namespace(namespace, wrong).await.unwrap(),
            NamespaceVerificationOutcome::ExpectationConflict
        );
        assert_eq!(
            catalog
                .create_import_directory(namespace, Uuid::nil(), b"late", Uuid::new_v4())
                .await
                .unwrap(),
            DirectoryOutcome::NamespaceNotBuilding
        );
        let mut late = first.clone();
        late.operation_id = Uuid::new_v4();
        late.copy_id = derive_copy_id(&late);
        assert_eq!(
            test_publish_imported(catalog, &late).await.unwrap(),
            PublishOutcome::NamespaceNotBuilding
        );
        assert!(matches!(
            catalog
                .resolve_child(namespace, Uuid::nil(), b"unknown")
                .await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog.verify_namespace_page(namespace, 100).await.unwrap(),
            NamespaceVerificationOutcome::Progress {
                phase: NamespacePhase::Nodes,
                actual: NamespaceSummary { nodes: 100, .. }
            }
        ));
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        assert_eq!(
            reopened
                .freeze_namespace(namespace, expected)
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        let mut phases = [false; 5];
        loop {
            match reopened
                .verify_namespace_page(namespace, 100)
                .await
                .unwrap()
            {
                NamespaceVerificationOutcome::Progress { phase, actual } => {
                    phases[phase as usize] = true;
                    assert!(actual.nodes >= 100);
                    assert!(matches!(
                        catalog.seal_import(namespace).await,
                        Err(CatalogError::Incomplete)
                    ));
                }
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, expected);
                    break;
                }
                unexpected => panic!("unexpected namespace result: {unexpected:?}"),
            }
        }
        assert!(phases.into_iter().all(|seen| seen));
        assert_eq!(
            reopened
                .verify_namespace_page(namespace, 100)
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Verified(expected)
        );
        assert_eq!(
            catalog.freeze_namespace(namespace, expected).await.unwrap(),
            NamespaceVerificationOutcome::Verified(expected)
        );
        assert!(catalog.seal_import(namespace).await.unwrap());
        assert!(!catalog.seal_import(namespace).await.unwrap());
        assert_eq!(
            catalog
                .node(namespace, first.file_id)
                .await
                .unwrap()
                .unwrap()
                .parent,
            Uuid::from_u128(1)
        );
        assert_eq!(
            catalog
                .file_head(namespace, first.file_id)
                .await
                .unwrap()
                .unwrap()
                .copy_id,
            first.copy_id
        );
        assert!(
            matches!(
                catalog
                    .physical_copy(namespace, first.file_id, first.copy_id)
                    .await,
                Err(CatalogError::Incomplete)
            ),
            "命名空间完整不代表卷已经可读"
        );

        let mut removed = 0;
        loop {
            let count = catalog
                .purge_namespace_proofs(namespace, 1000)
                .await
                .unwrap();
            if count == 0 {
                break;
            }
            removed += count;
        }
        assert_eq!(removed, 1103);
        assert!(
            catalog
                .file_head(namespace, first.file_id)
                .await
                .unwrap()
                .is_some()
        );

        for failure in 0..15 {
            let namespace = Uuid::new_v4();
            assert!(catalog.begin_import(namespace).await.unwrap());
            let dir = Uuid::from_u128(1);
            let node = directory(b"dir");
            assert_eq!(
                catalog
                    .create_import_directory(namespace, Uuid::nil(), &node.name, dir)
                    .await
                    .unwrap(),
                DirectoryOutcome::Created
            );
            let entry = file(&serial, namespace, volume, scan, 1);
            assert_eq!(
                test_publish_imported(catalog, &entry).await.unwrap(),
                PublishOutcome::Applied(1)
            );
            let records = Records::from([
                (Uuid::nil(), (root(), None)),
                (dir, (node.clone(), None)),
                (entry.file_id, source_file(&entry)),
            ]);
            let mut expected = summary(namespace, &records);
            if failure == 7 {
                expected.nodes += 1;
            }
            if failure == 8 {
                expected.digest[0] ^= 1;
            }
            catalog
                .db
                .run(|tr, _| {
                    let entry = entry.clone();
                    let node = node.clone();
                    async move {
                        match failure {
                            0 => tr.clear(&node_key(namespace, dir)),
                            1 => tr.clear(&child_key(namespace, dir, &entry.name)),
                            2 => tr.set(
                                &child_key(namespace, Uuid::nil(), b"extra"),
                                entry.file_id.as_bytes(),
                            ),
                            3 => tr.set(
                                &head_key(namespace, Uuid::from_u128(99)),
                                &encode_head(&FileHead {
                                    version: 1,
                                    size: 1,
                                    copy_id: entry.copy_id,
                                }),
                            ),
                            4 => tr.set(
                                &copy_key(namespace, Uuid::from_u128(99), entry.copy_id),
                                &encode_copy(&entry, 1),
                            ),
                            5 => tr.clear(&copy_key(namespace, entry.file_id, entry.copy_id)),
                            6 => {
                                let other = Uuid::from_u128(3);
                                let other_node = Node {
                                    parent: dir,

                                    ..directory(b"cycle")
                                };
                                let dir_node = Node {
                                    parent: other,

                                    ..node
                                };
                                tr.set(&node_key(namespace, dir), &encode_node(&dir_node));
                                tr.set(&node_key(namespace, other), &encode_node(&other_node));
                                tr.clear(&child_key(namespace, Uuid::nil(), &dir_node.name));
                                tr.set(
                                    &child_key(namespace, other, &dir_node.name),
                                    dir.as_bytes(),
                                );
                                tr.set(
                                    &child_key(namespace, dir, &other_node.name),
                                    other.as_bytes(),
                                );
                                let file_node = Node {
                                    ..source_file(&entry).0
                                };
                                tr.set(
                                    &node_key(namespace, entry.file_id),
                                    &encode_node(&file_node),
                                );
                            }
                            9 => tr.clear(&node_key(namespace, Uuid::nil())),
                            10 => tr.set(
                                &head_key(namespace, entry.file_id),
                                &encode_head(&FileHead {
                                    version: 2,
                                    size: 1,
                                    copy_id: entry.copy_id,
                                }),
                            ),
                            11 => tr.set(
                                &node_key(namespace, Uuid::from_u128(99)),
                                &encode_node(&directory(b"orphan")),
                            ),
                            12 => tr.clear(&volume_key(
                                volume,
                                scan.id,
                                namespace,
                                entry.file_id,
                                entry.copy_id,
                            )),
                            13 => {
                                let unknown = CommitReceipt {
                                    volume_id: Uuid::new_v4(),
                                    volume_scan_id: Uuid::new_v4(),
                                    ..entry.clone()
                                };
                                tr.set(
                                    &copy_key(namespace, entry.file_id, entry.copy_id),
                                    &encode_copy(&unknown, 1),
                                );
                            }
                            14 => tr.set(
                                &volume_key(
                                    volume,
                                    scan.id,
                                    namespace,
                                    entry.file_id,
                                    entry.copy_id,
                                ),
                                &[0; 40],
                            ),
                            _ => {}
                        }
                        Ok(())
                    }
                })
                .await
                .unwrap();
            if failure == 6 {
                let mut cyclic = records.clone();
                cyclic.insert(
                    dir,
                    (
                        Node {
                            parent: Uuid::from_u128(3),
                            ..node.clone()
                        },
                        None,
                    ),
                );
                cyclic.insert(
                    Uuid::from_u128(3),
                    (
                        Node {
                            parent: dir,
                            ..directory(b"cycle")
                        },
                        None,
                    ),
                );
                expected = summary(namespace, &cyclic);
            }
            assert_eq!(
                catalog.freeze_namespace(namespace, expected).await.unwrap(),
                NamespaceVerificationOutcome::Frozen
            );
            let mut rejected = false;
            for _ in 0..10 {
                match catalog.verify_namespace_page(namespace, 100).await {
                    Ok(NamespaceVerificationOutcome::Progress { .. }) => {}
                    Ok(NamespaceVerificationOutcome::Mismatch { .. })
                        if failure == 7 || failure == 8 =>
                    {
                        rejected = true;
                        break;
                    }
                    Err(CatalogError::Corrupt(_)) if failure != 7 && failure != 8 => {
                        rejected = true;
                        break;
                    }
                    unexpected => panic!("failure {failure}: {unexpected:?}"),
                }
            }
            assert!(rejected, "failure {failure} must be rejected");
            assert!(matches!(
                catalog.seal_import(namespace).await,
                Err(CatalogError::Incomplete)
            ));
            assert!(matches!(
                catalog.file_head(namespace, entry.file_id).await,
                Err(CatalogError::Incomplete)
            ));
        }
        // Retired physical availability must not erase known logical files.
        let namespace = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        assert!(catalog.begin_import(namespace).await.unwrap());
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog
                .create_import_directory(namespace, Uuid::nil(), b"dir", Uuid::from_u128(1))
                .await
                .unwrap(),
            DirectoryOutcome::Created
        );
        let entry = file(&serial, namespace, volume, scan, 1);
        assert_eq!(
            test_publish_imported(catalog, &entry).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Cancelled
        );
        assert_eq!(
            catalog
                .purge_retired_volume_index(volume, scan, 100)
                .await
                .unwrap(),
            1
        );
        let records = Records::from([
            (Uuid::nil(), (root(), None)),
            (Uuid::from_u128(1), (directory(b"dir"), None)),
            (entry.file_id, source_file(&entry)),
        ]);
        let expected = summary(namespace, &records);
        assert_eq!(
            catalog.freeze_namespace(namespace, expected).await.unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        loop {
            match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
                NamespaceVerificationOutcome::Progress { .. } => {}
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, expected);
                    break;
                }
                unexpected => panic!("retired copy verification: {unexpected:?}"),
            }
        }
        assert!(catalog.seal_import(namespace).await.unwrap());
        assert!(
            catalog
                .file_head(namespace, entry.file_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            catalog
                .physical_copy(namespace, entry.file_id, entry.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));

        // Parents deliberately sort after children; a long walk crosses calls.
        let namespace = Uuid::new_v4();
        assert!(catalog.begin_import(namespace).await.unwrap());
        let mut records = Records::from([(Uuid::nil(), (root(), None))]);
        for n in (1..=150).rev() {
            let id = Uuid::from_u128(n);
            let parent = if n == 150 {
                Uuid::nil()
            } else {
                Uuid::from_u128(n + 1)
            };
            let node = Node {
                parent,
                ..directory(format!("level-{n}").as_bytes())
            };
            assert_eq!(
                catalog
                    .create_import_directory(namespace, parent, &node.name, id)
                    .await
                    .unwrap(),
                DirectoryOutcome::Created
            );
            records.insert(id, (node, None));
        }
        let expected = summary(namespace, &records);
        assert_eq!(
            catalog.freeze_namespace(namespace, expected).await.unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        let mut reopened = None;
        let mut resumed_stack = false;
        loop {
            let reader = reopened.as_ref().unwrap_or(catalog);
            match reader.verify_namespace_page(namespace, 20).await.unwrap() {
                NamespaceVerificationOutcome::Progress {
                    phase: NamespacePhase::Reachability,
                    ..
                } => {
                    let tr = catalog.db.create_trx().unwrap();
                    let bytes = tr
                        .get(&progress_key(namespace), true)
                        .await
                        .unwrap()
                        .unwrap();
                    let progress = decode_progress(namespace, &bytes).unwrap();
                    if !progress.stack.is_empty() && reopened.is_none() {
                        resumed_stack = true;
                        reopened = Some(ManagedCatalog::open(Some(cluster_file)).unwrap());
                    }
                }
                NamespaceVerificationOutcome::Progress { .. } => {}
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, expected);
                    break;
                }
                unexpected => panic!("long parent-chain verification: {unexpected:?}"),
            }
        }
        assert!(resumed_stack);
        assert!(catalog.seal_import(namespace).await.unwrap());

        let namespace = Uuid::new_v4();
        assert!(catalog.begin_import(namespace).await.unwrap());
        let mut records = Records::from([(Uuid::nil(), (root(), None))]);
        for n in 1..=u128::from(MAX_DEPTH) + 1 {
            let parent = if n == u128::from(MAX_DEPTH) + 1 {
                Uuid::nil()
            } else {
                Uuid::from_u128(n + 1)
            };
            records.insert(
                Uuid::from_u128(n),
                (
                    Node {
                        parent,
                        ..directory(format!("deep-{n}").as_bytes())
                    },
                    None,
                ),
            );
        }
        catalog
            .db
            .run(|tr, _| {
                let records = records.clone();
                async move {
                    for (id, (node, _)) in records {
                        if id.is_nil() {
                            continue;
                        }
                        tr.set(&node_key(namespace, id), &encode_node(&node));
                        tr.set(
                            &child_key(namespace, node.parent, &node.name),
                            id.as_bytes(),
                        );
                    }
                    Ok(())
                }
            })
            .await
            .unwrap();
        assert_eq!(
            catalog
                .freeze_namespace(namespace, summary(namespace, &records))
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        let mut depth_rejected = false;
        for _ in 0..100 {
            match catalog.verify_namespace_page(namespace, 100).await {
                Ok(NamespaceVerificationOutcome::Progress { .. }) => {}
                Err(CatalogError::Corrupt(_)) => {
                    depth_rejected = true;
                    break;
                }
                unexpected => panic!("over-depth parent chain: {unexpected:?}"),
            }
        }
        assert!(depth_rejected);
        assert!(matches!(
            catalog.seal_import(namespace).await,
            Err(CatalogError::Incomplete)
        ));
    }
}
