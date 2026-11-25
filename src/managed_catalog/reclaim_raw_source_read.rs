//! Physical files absent from the logical namespace still require actual
//! source bytes. Reading them never creates or resurrects a user-visible file.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFact {
    version: u8,
    parent: String,
    inventory: [u8; 32],
    source_index: Vec<u8>,
    source_identity: [u8; 32],
    pub(super) object: Object,
    pub(super) body: Object,
    pub(super) sha256: [u8; 32],
    pub(super) md5: Option<[u8; 16]>,
}

impl ReadFact {
    pub(super) fn fingerprint(&self) -> Result<[u8; 32]> {
        self.validate()?;
        Ok(Sha256::new()
            .chain_update(b"tape-rs/reclaim-raw-read/1")
            .chain_update(serde_json::to_vec(self).map_err(cache)?)
            .finalize()
            .into())
    }
    pub(super) fn validate(&self) -> Result<()> {
        self.object.encode()?;
        self.body.encode()?;
        if self.version != 2
            || self.parent.len() != 64
            || !self
                .parent
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !matches!(
                self.object.kind,
                ReclaimSourceNodeKind::File | ReclaimSourceNodeKind::Symlink
            )
            || self.body.kind != ReclaimSourceNodeKind::File
            || (self.object.kind == ReclaimSourceNodeKind::File && self.object != self.body)
            || self.source_index.len() != 50
        {
            return Err(CatalogError::Corrupt("原始正文实读事实身份或类型无效"));
        }
        Ok(())
    }
    pub(super) fn require_plan(&self, plan: &ReclaimMetadataPlan) -> Result<()> {
        self.validate()?;
        let raw = plan
            .0
            .raw_body
            .as_ref()
            .ok_or(CatalogError::Corrupt("原始正文事实缺少追加计划"))?;
        if self.parent != plan.0.parent
            || self.inventory != plan.0.inventory
            || self.source_index != plan.0.source_index
            || self.object != plan.0.source
            || self.body != raw.body
            || self.fingerprint()? != plan.0.input
            || self.sha256 != raw.sha256
            || self.md5 != raw.md5
        {
            return Err(CatalogError::Corrupt("原始正文事实与固定计划不同"));
        }
        Ok(())
    }
    pub(super) fn require_inventory(
        &self,
        reservation: &ReclaimReservation,
        inventory: &ReclaimSourceInventory<'_>,
    ) -> Result<()> {
        self.validate()?;
        if self.parent != reservation.fingerprint()
            || self.inventory != inventory.summary.digest
            || self.source_index != inventory.summary.head
            || self.source_identity != inventory.summary.identity
            || inventory.summary.volume != reservation.spec().volume().to_string()
            || inventory
                .source
                .entry_by_uid(self.object.uid)
                .map_err(cache)?
                .map(Object::capture)
                .transpose()?
                != Some(self.object.clone())
            || source_body(inventory.source, self.object.uid)?.1 != self.body
        {
            return Err(CatalogError::Invalid("原始正文实读事实与原清单不同"));
        }
        Ok(())
    }
    pub(super) async fn require_at(
        &self,
        tr: &Transaction,
        reservation: &ReclaimReservation,
        plan: &ReclaimMetadataPlan,
    ) -> Result<()> {
        self.require_plan(plan)?;
        let (_, current) = source_state_at(tr, reservation).await?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, current);
        if head != self.source_index {
            return Err(CatalogError::Corrupt("原始正文任务的原源卷头改变"));
        }

        let manifest = ledger::require_complete_at(tr, reservation).await?;
        if self.parent != reservation.fingerprint()
            || self.inventory != manifest.summary.digest
            || self.source_index != manifest.summary.head
            || self.source_identity != manifest.summary.identity
            || tr
                .get(&ledger::object_key(reservation, self.object.uid), false)
                .await?
                .as_deref()
                != Some(self.object.encode()?.as_slice())
            || tr
                .get(&ledger::object_key(reservation, self.body.uid), false)
                .await?
                .as_deref()
                != Some(self.body.encode()?.as_slice())
        {
            return Err(CatalogError::Corrupt("原始正文任务丢失原封存实读来源"));
        }
        Ok(())
    }
}

/// Resolve one exact EE alias against the captured path index. The materialized
/// node keeps alias attributes and the data node's extents/known hashes; native
/// links and chains are never followed as byte sources.
pub(super) fn source_body(actual: &CapturedIndexSource, uid: u64) -> Result<(IndexEntry, Object)> {
    let original = actual
        .entry_by_uid(uid)
        .map_err(cache)?
        .ok_or(CatalogError::Incomplete)?;
    let IndexEntry::File { path, node } = &original else {
        return Err(CatalogError::Invalid("原始正文 UID 不是文件"));
    };
    if node.symlink.is_none() {
        return Ok((original.clone(), Object::capture(original)?));
    }
    let attrs = NodeAttributes::from_file(node)?;
    if attrs.link_kind()? != link_kind::LinkKind::EeAlias {
        return Err(CatalogError::Invalid("原生符号链接不能作为原始正文读取"));
    }
    let target = import_spool::resolve_ee_alias_target(
        path,
        attrs.symlink().ok_or(CatalogError::Incomplete)?,
    )
    .map_err(cache)?;
    let entry = actual
        .entry_by_path(&target)
        .map_err(cache)?
        .ok_or(CatalogError::Incomplete)?;
    let IndexEntry::File { node: data, .. } = &entry else {
        return Err(CatalogError::Invalid("EE 别名目标不是普通文件"));
    };
    if data.symlink.is_some() || data.open_for_write == Some(true) || data.delta_fields.is_some() {
        return Err(CatalogError::Invalid("EE 别名目标不是完整关闭的普通正文"));
    }
    let body = Object::capture(entry.clone())?;
    let (metadata, _) = attrs.data_relocation()?;
    let mut materialized = data.clone();
    materialized.name = node.name.clone();
    materialized.meta = metadata.meta;
    materialized.xattrs = metadata.xattrs;
    materialized.open_for_write = node.open_for_write;
    // Validate both known digest policies before replacing data-node attributes.
    let data_digest = ContentDigest::from_ltfs_xattrs(&data.xattrs)?;
    let alias_digest = ContentDigest::from_ltfs_xattrs(&materialized.xattrs)?;
    if matches!((alias_digest, data_digest), (ContentDigest::Sha256(a), ContentDigest::Sha256(b)) if a != b)
        || matches!((read::expected_md5(node)?, read::expected_md5(data)?), (Some(a), Some(b)) if a != b)
    {
        return Err(CatalogError::Invalid("EE 别名与正文已知摘要冲突"));
    }
    for key in [
        crate::ltfs::index::XATTR_SHA256,
        crate::ltfs::index::XATTR_MD5,
    ] {
        if !materialized.xattrs.iter().any(|a| a.key == key) {
            materialized
                .xattrs
                .extend(data.xattrs.iter().filter(|a| a.key == key).cloned());
        }
    }
    Ok((
        IndexEntry::File {
            path: path.clone(),
            node: materialized,
        },
        body,
    ))
}

impl ReclaimSourceInventory<'_> {
    /// Budget only: use the actual resolved data UID for EE aliases. This is
    /// not a read proof, and the later uncached read still validates its context.
    pub(crate) fn raw_body_length(&self, uid: u64) -> Result<u64> {
        source_body(self.source, uid)?
            .1
            .length
            .ok_or(CatalogError::Corrupt("原始正文缺少封存长度"))
    }

    pub(crate) fn is_ee_alias(&self, node: &ReclaimSourceNode) -> Result<bool> {
        let entry = self
            .source
            .entry_by_uid(node.uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        if Object::capture(entry.clone())? != node.0 {
            return Err(CatalogError::Invalid("原始别名改变源节点"));
        }
        Ok(
            matches!(entry, IndexEntry::File {node,..} if NodeAttributes::from_file(&node)?.link_kind()? == link_kind::LinkKind::EeAlias),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimRawReadContext {
    parent: String,
    inventory: [u8; 32],
    index: Vec<u8>,
    identity: [u8; 32],
    dp_eod: u64,
    ip_generation: Option<u64>,
    history_depth: u32,
    node: ReclaimSourceNode,
}
impl ReclaimRawReadContext {
    pub fn node(&self) -> &ReclaimSourceNode {
        &self.node
    }
    pub(crate) fn require_capture(
        &self,
        actual: &CapturedIndexSource,
    ) -> Result<crate::ltfs::index::FileNode> {
        let scope = current_proof(actual.scope(), actual.header())?;
        let mut index = Vec::new();
        active_volume::encode_index(&mut index, actual.construct().into());
        let entry = actual
            .entry_by_uid(self.node.uid())
            .map_err(cache)?
            .ok_or(CatalogError::Incomplete)?;
        let IndexEntry::File { .. } = &entry else {
            return Err(CatalogError::Invalid("原始正文 UID 不是文件"));
        };
        if index != self.index
            || actual.identity().map_err(cache)?.fingerprint() != self.identity
            || scope != (self.dp_eod, self.ip_generation, self.history_depth)
            || Object::capture(entry.clone())? != self.node.0
        {
            return Err(CatalogError::Invalid(
                "原始正文实际索引、UID 或节点类型改变",
            ));
        }
        let (IndexEntry::File { node, .. }, _) = source_body(actual, self.node.uid())? else {
            return Err(CatalogError::Incomplete);
        };
        read::expected_md5(&node)?;
        Ok(node)
    }
}

/// Live proof produced only after the source's actual uncached byte stream and
/// final owner check. Saved facts alone do not grant device authority.
///
/// ```compile_fail
/// let _: tape_rs::managed_catalog::ReclaimRawSourceRead =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimRawSourceRead {
    owner: u64,
    pub(super) fact: ReadFact,
}
impl ReclaimRawSourceRead {
    pub fn observed_owner(&self) -> u64 {
        self.owner
    }
    pub fn source_node(&self) -> ReclaimSourceNode {
        ReclaimSourceNode(self.fact.object.clone())
    }
    pub fn sha256(&self) -> [u8; 32] {
        self.fact.sha256
    }
    pub fn md5(&self) -> Option<[u8; 16]> {
        self.fact.md5
    }
    pub(crate) fn from_read(
        context: &ReclaimRawReadContext,
        owner: u64,
        actual: &CapturedIndexSource,
        sha256: [u8; 32],
        md5: Option<[u8; 16]>,
    ) -> Result<Self> {
        let node = context.require_capture(actual)?;
        if owner == 0
            || read::expected_md5(&node)? != md5
            || matches!(ContentDigest::from_ltfs_xattrs(&node.xattrs)?,ContentDigest::Sha256(value) if value!=sha256)
        {
            return Err(CatalogError::Invalid("原始正文实读摘要与已知索引属性不符"));
        }
        Ok(Self {
            owner,
            fact: ReadFact {
                version: 2,
                parent: context.parent.clone(),
                inventory: context.inventory,
                source_index: context.index.clone(),
                source_identity: context.identity,
                object: context.node.0.clone(),
                body: source_body(actual, context.node.uid())?.1,
                sha256,
                md5,
            },
        })
    }
}

impl ManagedCatalog {
    /// Resolve a sealed physical UID, independently of logical file heads.
    /// The returned context is read intent, never source/destination evidence.
    pub async fn reclaim_raw_read_context(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        uid: u64,
    ) -> Result<ReclaimRawReadContext> {
        if uid == 0 {
            return Err(CatalogError::Invalid("原始正文 UID 不能为零"));
        }
        let tr = self.db.create_trx()?;
        let parent = record_at(&tr, reservation, owner).await?;
        if parent["phase"] == "reformatting" {
            return Err(CatalogError::Invalid(
                "源卷已进入格式化阶段，不能重读旧正文",
            ));
        }
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let (_, index) = source_state_at(&tr, reservation).await?;
        let manifest = ledger::require_complete_at(&tr, reservation).await?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, index);
        let raw = tr
            .get(&ledger::object_key(reservation, uid), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let object: Object = serde_json::from_slice(&raw).map_err(cache)?;
        if object.encode()?.as_slice() != raw.as_ref()
            || object.uid != uid
            || !matches!(
                object.kind,
                ReclaimSourceNodeKind::File | ReclaimSourceNodeKind::Symlink
            )
            || manifest.summary.head != head
            || manifest.summary.volume != reservation.spec().volume().to_string()
        {
            return Err(CatalogError::Invalid("原始正文不是原清单中的普通文件"));
        }
        Ok(ReclaimRawReadContext {
            parent: reservation.fingerprint(),
            inventory: manifest.summary.digest,
            index: head,
            identity: manifest.summary.identity,
            dp_eod: manifest.summary.dp_eod,
            ip_generation: manifest.summary.ip_generation,
            history_depth: manifest.summary.history_depth,
            node: ReclaimSourceNode(object),
        })
    }
}

#[cfg(test)]
#[path = "reclaim_raw_body_plan_tests.rs"]
mod plan_tests;
