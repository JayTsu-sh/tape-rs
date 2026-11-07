//! One typed exclusion slot for node, rename and reclaim metadata media work.
//! Parsing is not authorization: the corresponding original ledger must match.
use super::*;
use serde::{Deserialize, Serialize};
const MAX_BODY: usize = 8192;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) struct RenameVolumeClaim {
    pub namespace: Uuid,
    pub pool: Uuid,
    pub library: String,
    pub epoch: u64,
    pub operation: Uuid,
    pub global: Uuid,
    pub volume: Uuid,
    pub intent: Uuid,
    pub page: u64,
    pub owner: u64,
    pub fingerprint: [u8; 32],
    pub closed: Option<[u8; 32]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) enum NodeVolumeClaim {
    Single(Box<NodeMutationAttempt>),
    Rename(Box<RenameVolumeClaim>),
    ReclaimMetadata(Box<ReclaimMetadataVolumeClaim>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::managed_catalog) struct ReclaimMetadataVolumeClaim {
    pub namespace: String,
    pub volume: String,
    pub task: Vec<u8>,
    pub fingerprint: [u8; 32],
}
impl ReclaimMetadataVolumeClaim {
    fn encode(&self) -> Result<Vec<u8>> {
        let ns = node_mutation::id(&self.namespace)?;
        node_mutation::id(&self.volume)?;
        let mut prefix = management::prefix(ns, b'R');
        prefix.push(b'W');
        if !self.task.starts_with(&prefix)
            || self.task.len() != prefix.len() + 64
            || self.task[self.task.len() - 32..] != self.fingerprint
        {
            return Err(bad());
        }
        let suffix = &self.task[prefix.len()..];
        if suffix[..8] == [0; 8] || suffix[8..24] == [0; 16] || suffix[24..32] == [0; 8] {
            return Err(bad());
        }
        node_mutation::canonical(self)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(raw).map_err(|_| bad())?;
        if value.encode()? != raw {
            return Err(bad());
        }
        Ok(value)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameWire {
    version: u8,
    namespace: String,
    pool: String,
    library: String,
    epoch: u64,
    operation: String,
    global: String,
    volume: String,
    intent: String,
    page: u64,
    owner: u64,
    fingerprint: [u8; 32],
    closed: Option<[u8; 32]>,
}
fn bad() -> CatalogError {
    CatalogError::Corrupt("节点卷占用类型、编码或作用域不符")
}
impl RenameVolumeClaim {
    fn wire(&self) -> Result<RenameWire> {
        validate_physical_serial(&self.library).map_err(|_| bad())?;
        if self.epoch == 0
            || self.owner == 0
            || self.owner > 0xFFFF_FFFF_FFFF
            || [
                self.namespace,
                self.pool,
                self.operation,
                self.global,
                self.volume,
                self.intent,
            ]
            .into_iter()
            .any(|id| id.is_nil())
        {
            return Err(bad());
        }
        Ok(RenameWire {
            version: 1,
            namespace: self.namespace.to_string(),
            pool: self.pool.to_string(),
            library: self.library.clone(),
            epoch: self.epoch,
            operation: self.operation.to_string(),
            global: self.global.to_string(),
            volume: self.volume.to_string(),
            intent: self.intent.to_string(),
            page: self.page,
            owner: self.owner,
            fingerprint: self.fingerprint,
            closed: self.closed,
        })
    }
    fn encode(&self) -> Result<Vec<u8>> {
        node_mutation::canonical(&self.wire()?)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() > MAX_BODY {
            return Err(bad());
        }
        let w: RenameWire = serde_json::from_slice(raw).map_err(|_| bad())?;
        if w.version != 1 {
            return Err(bad());
        }
        let value = Self {
            namespace: node_mutation::id(&w.namespace)?,
            pool: node_mutation::id(&w.pool)?,
            library: w.library,
            epoch: w.epoch,
            operation: node_mutation::id(&w.operation)?,
            global: node_mutation::id(&w.global)?,
            volume: node_mutation::id(&w.volume)?,
            intent: node_mutation::id(&w.intent)?,
            page: w.page,
            owner: w.owner,
            fingerprint: w.fingerprint,
            closed: w.closed,
        };
        if value.encode()? != raw {
            return Err(bad());
        }
        Ok(value)
    }
}
pub(in crate::managed_catalog) fn encode(claim: &NodeVolumeClaim) -> Result<Vec<u8>> {
    let (kind, body) = match claim {
        NodeVolumeClaim::Single(value) => (0, node_attempts::encode(value)?),
        NodeVolumeClaim::Rename(value) => (1, value.encode()?),
        NodeVolumeClaim::ReclaimMetadata(value) => (2, value.encode()?),
    };
    if body.len() > MAX_BODY {
        return Err(bad());
    }
    let mut raw = Vec::with_capacity(body.len() + 2);
    raw.extend_from_slice(&[1, kind]);
    raw.extend_from_slice(&body);
    Ok(raw)
}
pub(in crate::managed_catalog) fn decode(raw: &[u8]) -> Result<NodeVolumeClaim> {
    if raw.len() < 3 || raw.len() > MAX_BODY + 2 || raw[0] != 1 {
        return Err(bad());
    }
    match raw[1] {
        0 => Ok(NodeVolumeClaim::Single(Box::new(node_attempts::decode(
            &raw[2..],
        )?))),
        1 => Ok(NodeVolumeClaim::Rename(Box::new(
            RenameVolumeClaim::decode(&raw[2..])?,
        ))),
        2 => Ok(NodeVolumeClaim::ReclaimMetadata(Box::new(
            ReclaimMetadataVolumeClaim::decode(&raw[2..])?,
        ))),
        _ => Err(bad()),
    }
}

#[cfg(test)]
#[path = "node_claims_tests.rs"]
mod tests;
