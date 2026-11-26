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

/// The caller retains this identity and the original bytes until publication.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplicatedArchive {
    pub operation: Uuid,
    pub path: String,
    pub previous: Option<u64>,
    pub size: u64,
    #[serde(with = "hex_digest")]
    pub sha256: [u8; 32],
}

mod hex_digest {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &[u8; 32],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(
            &value
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 32], D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(
                "归档摘要须为 64 位十六进制 SHA-256",
            ));
        }
        let mut digest = [0u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(digest)
    }
}

pub(crate) fn inode(pool: Uuid, path: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let hash = Sha256::new()
        .chain_update(b"tape-rs/replica-inode/1")
        .chain_update(pool.as_bytes())
        .chain_update(path.as_bytes())
        .finalize();
    Uuid::from_bytes(hash[..16].try_into().expect("SHA-256"))
}
