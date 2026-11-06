//! Small immutable projection of LTFS attributes for object queries. It shares
//! the namespace-owned M lifetime; reading it never hydrates attribute chunks.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;

const LIMIT: usize = 8192;
const HEADERS: &str = "tapers.s3.headers";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub modified: Option<i64>,
    pub headers: BTreeMap<String, String>,
}

impl ObjectMetadata {
    fn validate(&self) -> Result<()> {
        if self
            .modified
            .is_some_and(|time| chrono::DateTime::from_timestamp(time, 0).is_none())
            || self.headers.len() > 128
        {
            return Err(CatalogError::Invalid("对象时间或 HTTP 属性数量无效"));
        }
        for (name, value) in &self.headers {
            if !allowed(name)
                || axum::http::HeaderName::from_bytes(name.as_bytes()).is_err()
                || name.bytes().any(|b| b.is_ascii_uppercase())
                || axum::http::HeaderValue::from_str(value).is_err()
            {
                return Err(CatalogError::Invalid("对象 HTTP 属性无效"));
            }
        }
        if self.encode().len() > LIMIT {
            return Err(CatalogError::Invalid("对象 HTTP 属性超过 8 KiB"));
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&(1u8, self.modified, &self.headers)).expect("metadata JSON")
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > LIMIT {
            return Err(CatalogError::Corrupt("对象 HTTP 属性记录过长"));
        }
        let (version, modified, headers): (u8, Option<i64>, BTreeMap<String, String>) =
            serde_json::from_slice(bytes)
                .map_err(|_| CatalogError::Corrupt("对象 HTTP 属性编码"))?;
        let value = Self { modified, headers };
        if version != 1 || value.validate().is_err() || value.encode() != bytes {
            return Err(CatalogError::Corrupt("对象 HTTP 属性记录无效"));
        }
        Ok(value)
    }

    pub(super) fn from_attributes(attributes: &NodeAttributes) -> Result<Self> {
        let meta = attributes
            .meta()
            .ok_or(CatalogError::Invalid("对象属性尚未读取"))?;
        let modified = chrono::DateTime::parse_from_rfc3339(&meta.modify_time)
            .ok()
            .map(|t| t.timestamp());
        let mut saved = None;
        for attr in attributes.xattrs().expect("complete attributes") {
            if attr.key == HEADERS {
                if saved.is_some() {
                    return Err(CatalogError::Invalid("对象 HTTP 属性重复"));
                }
                saved = Some((attr.value.as_str(), attr.base64));
            }
        }
        Self::from_parts(modified, saved)
    }

    pub(super) fn from_parts(modified: Option<i64>, saved: Option<(&str, bool)>) -> Result<Self> {
        let mut value = Self {
            modified,
            headers: BTreeMap::new(),
        };
        if let Some((text, binary)) = saved {
            if text.len() > LIMIT.div_ceil(3) * 4 {
                return Err(CatalogError::Invalid("对象 HTTP 属性过长"));
            }
            let bytes = if binary {
                STANDARD
                    .decode(text)
                    .map_err(|_| CatalogError::Invalid("对象 HTTP 属性 Base64 无效"))?
            } else {
                text.as_bytes().to_vec()
            };
            if bytes.len() > LIMIT {
                return Err(CatalogError::Invalid("对象 HTTP 属性过长"));
            }
            let headers: BTreeMap<String, String> = serde_json::from_slice(&bytes)
                .map_err(|_| CatalogError::Invalid("对象 HTTP 属性 JSON 无效"))?;
            for (name, header_value) in headers {
                let name = name.to_ascii_lowercase();
                if !allowed(&name) {
                    return Err(CatalogError::Invalid("对象包含不支持的 HTTP 属性"));
                }
                if value.headers.insert(name, header_value).is_some() {
                    return Err(CatalogError::Invalid("对象 HTTP 属性大小写重复"));
                }
            }
        }
        value.validate()?;
        Ok(value)
    }
}

fn allowed(name: &str) -> bool {
    (name.starts_with("x-amz-meta-") && name.len() > "x-amz-meta-".len())
        || matches!(
            name,
            "content-type"
                | "content-encoding"
                | "content-language"
                | "content-disposition"
                | "cache-control"
                | "expires"
        )
}

fn metadata_key(namespace: Uuid, digest: [u8; 32]) -> Vec<u8> {
    let mut key = key(b'M');
    key.extend_from_slice(namespace.as_bytes());
    key.extend_from_slice(&digest);
    key.push(2);
    key
}

pub(super) async fn read_at(
    tr: &Transaction,
    namespace: Uuid,
    digest: Option<[u8; 32]>,
) -> Result<Option<ObjectMetadata>> {
    let Some(digest) = digest else {
        return Ok(None);
    };
    let bytes = tr
        .get(&metadata_key(namespace, digest), true)
        .await?
        .ok_or(CatalogError::Corrupt("对象元数据投影缺失"))?;
    ObjectMetadata::decode(&bytes).map(Some)
}

/// Validate caller scope first. This immutable projection is written in the
/// caller's transaction so a new logical node and its metadata appear together.
pub(super) async fn ensure_at(
    tr: &Transaction,
    namespace: Uuid,
    attributes: &NodeAttributes,
) -> Result<()> {
    let value = ObjectMetadata::from_attributes(attributes)?.encode();
    let key = metadata_key(namespace, attributes.digest());
    if let Some(saved) = tr.get(&key, false).await? {
        if saved.as_ref() != value.as_slice() {
            return Err(CatalogError::Corrupt("对象元数据投影与属性冲突"));
        }
    } else {
        tr.set(&key, &value);
    }
    Ok(())
}

impl ManagedCatalog {
    pub(super) async fn upload_object_metadata(
        &self,
        namespace: Uuid,
        attributes: &NodeAttributes,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        let state = tr.get(&namespace_key(namespace), false).await?;
                        if !matches!(
                            state.as_deref().map(decode_namespace_state).transpose()?,
                            Some(
                                NamespaceState::Building
                                    | NamespaceState::Ready
                                    | NamespaceState::Active
                            )
                        ) {
                            return Err(CatalogError::Incomplete);
                        }
                        ensure_at(&tr, namespace, attributes).await?;
                        Ok(())
                    }
                    .await,
                )
            })
            .await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::{NodeMeta, Xattr};

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；仅元数据和大属性，不访问设备"]
    async fn projection_is_immutable_verified_and_namespace_owned() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        catalog.begin_import(namespace).await.unwrap();
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                modify_time: "2026-10-31T01:02:03.999999999Z".into(),
                ..Default::default()
            },
            vec![Xattr {
                key: "padding".into(),
                value: "x".repeat(2_000_000),
                base64: false,
            }],
            None,
        )
        .unwrap();
        catalog
            .set_import_root_attributes(namespace, &attributes)
            .await
            .unwrap();
        let expected = ObjectMetadata::from_attributes(&attributes).unwrap();
        let key = metadata_key(namespace, attributes.digest());
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            read_at(&tr, namespace, Some(attributes.digest()))
                .await
                .unwrap(),
            Some(expected.clone())
        );
        let mut changed = expected.clone();
        changed.modified = changed.modified.map(|time| time + 1);
        tr.set(&key, &changed.encode());
        tr.commit().await.unwrap();
        let manifest = attributes.manifest().unwrap();
        assert!(
            catalog
                .verify_attribute_object(namespace, manifest)
                .await
                .is_err()
        );
        assert!(
            catalog
                .upload_attributes(namespace, &attributes)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&key, &expected.encode());
        tr.commit().await.unwrap();
        catalog
            .verify_attribute_object(namespace, manifest)
            .await
            .unwrap();
        catalog.discard_import(namespace).await.unwrap();
        while !catalog
            .purge_discarded_namespace(namespace, 100)
            .await
            .unwrap()
            .complete
        {}
        let tr = catalog.db.create_trx().unwrap();
        assert!(tr.get(&key, false).await.unwrap().is_none());
    }

    #[test]
    fn projection_is_bounded_and_uses_exact_attributes() {
        let headers = r#"{"Content-Type":"text/plain","x-amz-meta-origin":"fixture"}"#;
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                modify_time: "2026-10-31T01:02:03.999999999Z".into(),
                ..Default::default()
            },
            vec![
                Xattr {
                    key: HEADERS.into(),
                    value: STANDARD.encode(headers),
                    base64: true,
                },
                Xattr {
                    key: "unrelated".into(),
                    value: "x".repeat(2_000_000),
                    base64: false,
                },
            ],
            None,
        )
        .unwrap();
        let value = ObjectMetadata::from_attributes(&attributes).unwrap();
        assert_eq!(value.modified, Some(1793408523));
        assert_eq!(value.headers["content-type"], "text/plain");
        assert_eq!(ObjectMetadata::decode(&value.encode()).unwrap(), value);
        assert!(value.encode().len() < 256);
        for bad in [
            r#"{"content-length":"1"}"#.to_owned(),
            r#"{"x-amz-meta-a":"bad\r\nheader"}"#.to_owned(),
            format!("{{\"x-amz-meta-a\":\"{}\"}}", "x".repeat(LIMIT)),
            r#"{"Content-Type":"a","content-type":"b"}"#.to_owned(),
        ] {
            assert!(ObjectMetadata::from_parts(None, Some((&bad, false))).is_err());
        }
        let mut raw = value.encode();
        raw.push(b' ');
        assert!(ObjectMetadata::decode(&raw).is_err());
    }
}
