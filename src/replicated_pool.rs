//! Administrative request identities shared by the SDK and FDB service.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateReplicatedPool {
    pub version: u8,
    pub id: Uuid,
    pub name: String,
    pub created_at_millis: u64,
    pub members: Vec<ReplicatedPoolMember>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplicatedPoolMember {
    pub library_serial: String,
    pub namespace: Uuid,
    pub logical_library: Option<Uuid>,
    pub pool_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub tapes: Vec<String>,
    #[serde(default)]
    pub confirm_format: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolveReplicatedPoolConflict {
    pub version: u8,
    pub path: String,
    pub revision: u64,
    /// Keep the bytes verified on this source in the replicated namespace.
    pub volume_uuid: Uuid,
}
