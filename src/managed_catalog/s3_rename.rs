//! Durable S3 client-token binding to one immutable filesystem rename request.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3RenameInput {
    pub library: String,
    #[serde(
        serialize_with = "serialize_uuid",
        deserialize_with = "deserialize_uuid"
    )]
    pub namespace: Uuid,
    #[serde(
        serialize_with = "serialize_uuid",
        deserialize_with = "deserialize_uuid"
    )]
    pub pool: Uuid,
    pub token: String,
    pub from: String,
    pub to: String,
    /// Source and destination: match, none-match, modified-since, unmodified-since.
    pub conditions: [Option<String>; 8],
}

fn serialize_uuid<S: serde::Serializer>(
    id: &Uuid,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}
fn deserialize_uuid<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Uuid, D::Error> {
    let text = String::deserialize(deserializer)?;
    let id = Uuid::parse_str(&text).map_err(serde::de::Error::custom)?;
    if id.is_nil() || id.to_string() != text {
        return Err(serde::de::Error::custom("UUID 必须规范且非空"));
    }
    Ok(id)
}

impl S3RenameInput {
    fn validate(&self) -> Result<()> {
        control::validate_physical_serial(&self.library)?;
        if self.namespace.is_nil()
            || self.pool.is_nil()
            || self.token.is_empty()
            || self.token.len() > 64
            || !self.token.bytes().all(|b| (33..=126).contains(&b))
            || self.conditions.iter().flatten().any(|v| v.len() > 1024)
        {
            return Err(CatalogError::Invalid("S3 rename 作用域、token 或条件无效"));
        }
        for path in [&self.from, &self.to] {
            pool_catalog_path(self.pool, path.as_bytes())?;
        }
        Ok(())
    }
    fn key(&self) -> Vec<u8> {
        let mut key = control::management::prefix(self.namespace, b'Q');
        key.push(b'J');
        key.extend_from_slice(&(self.library.len() as u16).to_be_bytes());
        key.extend_from_slice(self.library.as_bytes());
        key.extend_from_slice(self.pool.as_bytes());
        key.extend_from_slice(&Sha256::digest(self.token.as_bytes()));
        key
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    input: S3RenameInput,
    request: Vec<u8>,
    fingerprint: [u8; 32],
}
fn encode(input: &S3RenameInput, request: &NodeRenameRequest) -> Result<Vec<u8>> {
    input.validate()?;
    let spec = request.spec();
    if request.library() != input.library
        || spec.pool != input.pool
        || spec.source.namespace != input.namespace
        || spec.from != input.from
        || spec.to != input.to
        || spec.no_replace != (input.conditions[5].as_deref() == Some("*"))
    {
        return Err(CatalogError::Corrupt("S3 rename 原参数与规范请求不一致"));
    }
    let bytes = serde_json::to_vec(&Wire {
        version: 1,
        input: input.clone(),
        request: request.encode()?,
        fingerprint: request.fingerprint(),
    })
    .map_err(|_| CatalogError::Corrupt("S3 rename 记录编码失败"))?;
    if bytes.len() > 32_000 {
        return Err(CatalogError::Invalid("S3 rename 记录超过预算"));
    }
    Ok(bytes)
}
fn decode(bytes: &[u8], input: &S3RenameInput) -> Result<NodeRenameRequest> {
    if bytes.len() > 32_000 {
        return Err(CatalogError::Corrupt("S3 rename 记录超过预算"));
    }
    let wire: Wire = serde_json::from_slice(bytes)
        .map_err(|_| CatalogError::Corrupt("S3 rename 记录格式无效"))?;
    let request = NodeRenameRequest::decode(&wire.request)?;
    if wire.version != 1
        || wire.fingerprint != request.fingerprint()
        || encode(&wire.input, &request)? != bytes
        || wire.input.key() != input.key()
    {
        return Err(CatalogError::Corrupt("S3 rename 原记录作用域或摘要无效"));
    }
    if wire.input != *input {
        return Err(CatalogError::IdempotencyMismatch);
    }
    Ok(request)
}

impl ManagedCatalog {
    /// Ownerless lookup precedes all current-path resolution and condition checks.
    pub async fn s3_rename_request(
        &self,
        route: ActiveLibraryRoute,
        input: &S3RenameInput,
    ) -> Result<Option<NodeRenameRequest>> {
        input.validate()?;
        let tr = self.db.create_trx()?;
        if routing::require_expected_active_namespace(&tr, &input.library, Some(route)).await?
            != input.namespace
        {
            return Err(CatalogError::Incomplete);
        }
        tr.get(&input.key(), false)
            .await?
            .map(|bytes| decode(&bytes, input))
            .transpose()
    }

    /// First writer wins, including races and an unknown transaction commit result.
    pub async fn remember_s3_rename_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        input: &S3RenameInput,
        request: &NodeRenameRequest,
    ) -> Result<NodeRenameRequest> {
        let bytes = encode(input, request)?;
        self.db
            .run(|tr, _| {
                let bytes = &bytes;
                async move {
                    requests::retry_result(
                        async {
                            if routing::require_runtime_active_owner(
                                &tr,
                                &input.library,
                                route,
                                owner,
                            )
                            .await?
                                != input.namespace
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            let key = input.key();
                            if let Some(saved) = tr.get(&key, false).await? {
                                return decode(&saved, input);
                            }
                            tr.set(&key, bytes);
                            Ok(request.clone())
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (S3RenameInput, NodeRenameRequest) {
        let namespace = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let revision = |inode| serde_json::json!({"namespace":namespace.to_string(),"inode":inode,"version":1});
        let mut input = S3RenameInput {
            library: "SIM".into(),
            namespace,
            pool: Uuid::new_v4(),
            token: "original-token".into(),
            from: "/a".into(),
            to: "/b".into(),
            conditions: Default::default(),
        };
        input.conditions[5] = Some("*".into());
        let value = serde_json::json!({"version":1,"library":input.library,"pool":input.pool.to_string(),
            "operation":Uuid::new_v4().to_string(),"request_epoch":1,"from":input.from,"to":input.to,
            "source":revision(Uuid::new_v4().to_string()),"source_parent":revision(parent.to_string()),
            "destination_parent":revision(parent.to_string()),"target":null,"no_replace":true,
            "time":"2026-11-03T00:00:00.000000000Z","baseline_sha256":vec![0u8;32]});
        let request = NodeRenameRequest::decode(value.to_string().as_bytes()).unwrap();
        (input, request)
    }
    #[test]
    fn token_record_is_canonical_scoped_and_keeps_the_original_request() {
        let (input, request) = fixture();
        let bytes = encode(&input, &request).unwrap();
        assert_eq!(decode(&bytes, &input).unwrap(), request);
        let mut changed = input.clone();
        changed.to = "/later".into();
        assert!(matches!(
            decode(&bytes, &changed),
            Err(CatalogError::IdempotencyMismatch)
        ));
        changed = input.clone();
        changed.conditions[0] = Some("*".into());
        assert!(matches!(
            decode(&bytes, &changed),
            Err(CatalogError::IdempotencyMismatch)
        ));
        changed = input.clone();
        changed.pool = Uuid::new_v4();
        assert_ne!(changed.key(), input.key());
        assert!(decode(&bytes, &changed).is_err());
        assert!(decode(&[bytes.clone(), b"\n".to_vec()].concat(), &input).is_err());
        assert!(decode(b"{}", &input).is_err());
        let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        wire["fingerprint"][0] = (wire["fingerprint"][0].as_u64().unwrap() ^ 255).into();
        assert!(decode(wire.to_string().as_bytes(), &input).is_err());
        wire = serde_json::from_slice(&bytes).unwrap();
        wire["input"]["pool"] = input.pool.to_string().to_uppercase().into();
        // UUIDs containing hexadecimal letters must not admit another spelling.
        if input.pool.to_string() != input.pool.to_string().to_uppercase() {
            assert!(decode(wire.to_string().as_bytes(), &input).is_err());
        }
    }
    #[test]
    fn token_and_request_scope_validation_precedes_persistence() {
        let (input, request) = fixture();
        for token in ["".into(), "with space".into(), "x".repeat(65), "é".into()] {
            let mut bad = input.clone();
            bad.token = token;
            assert!(encode(&bad, &request).is_err());
        }
        let mut bad = input.clone();
        bad.from = "/other".into();
        assert!(encode(&bad, &request).is_err());
        bad = input.clone();
        bad.conditions[5] = None;
        assert!(encode(&bad, &request).is_err());
    }
}
