//! Original S3 replacement attributes, bound before any physical append.
use super::*;
use crate::ltfs::index::{NodeMeta, Xattr};
use crate::ltfs::volume::FileMetadata;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LIMIT: usize = 32_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    request: [u8; 32],
    meta: NodeMeta,
    xattrs: Vec<Xattr>,
    checksum: [u8; 32],
}
fn key(request: &ActiveFileRequest) -> Vec<u8> {
    let mut key = control::management::prefix(request.namespace, b'Q');
    key.push(b'U');
    key.extend_from_slice(&request.request_epoch.to_be_bytes());
    key.extend_from_slice(request.operation_id.as_bytes());
    key
}
fn encode(request: &ActiveFileRequest, metadata: &FileMetadata) -> Result<Vec<u8>> {
    request.validate()?;
    let fields = serde_json::to_vec(&(request.fingerprint(), &metadata.meta, &metadata.xattrs))
        .map_err(|_| CatalogError::Invalid("S3 上传属性编码失败"))?;
    let mut digest = Sha256::new();
    digest.update(b"tape-rs/s3-upload-attributes/1");
    digest.update(&fields);
    let bytes = serde_json::to_vec(&Wire {
        version: 1,
        request: request.fingerprint(),
        meta: metadata.meta.clone(),
        xattrs: metadata.xattrs.clone(),
        checksum: digest.finalize().into(),
    })
    .map_err(|_| CatalogError::Invalid("S3 上传属性编码失败"))?;
    if bytes.len() > LIMIT {
        return Err(CatalogError::Invalid("S3 上传属性超过预算"));
    }
    Ok(bytes)
}
fn decode(bytes: &[u8], request: &ActiveFileRequest) -> Result<FileMetadata> {
    if bytes.len() > LIMIT {
        return Err(CatalogError::Corrupt("S3 上传属性超过预算"));
    }
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("S3 上传属性记录损坏"))?;
    let metadata = FileMetadata {
        meta: wire.meta,
        xattrs: wire.xattrs,
    };
    if wire.version != 1
        || wire.request != request.fingerprint()
        || encode(request, &metadata)? != bytes
    {
        return Err(CatalogError::Corrupt("S3 上传属性与原请求或摘要不一致"));
    }
    Ok(metadata)
}
impl ManagedCatalog {
    pub(crate) async fn remember_s3_upload_metadata(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &ActiveFileRequest,
        metadata: &FileMetadata,
    ) -> Result<()> {
        let bytes = encode(request, metadata)?;
        self.db
            .run(|tr, _| {
                let bytes = &bytes;
                async move {
                    requests::retry_result(
                        async {
                            if routing::require_runtime_active_owner(
                                &tr,
                                &request.library_serial,
                                route,
                                owner,
                            )
                            .await?
                                != request.namespace
                            {
                                return Err(CatalogError::Incomplete);
                            }
                            let key = key(request);
                            if let Some(saved) = tr.get(&key, false).await? {
                                decode(&saved, request)?;
                                if saved.as_ref() != bytes.as_slice() {
                                    return Err(CatalogError::IdempotencyMismatch);
                                }
                            } else {
                                tr.set(&key, bytes);
                            }
                            Ok(())
                        }
                        .await,
                    )
                }
            })
            .await?
    }
    pub(crate) async fn s3_upload_metadata(
        &self,
        route: ActiveLibraryRoute,
        request: &ActiveFileRequest,
    ) -> Result<Option<FileMetadata>> {
        let tr = self.db.create_trx()?;
        if routing::require_expected_active_namespace(&tr, &request.library_serial, Some(route))
            .await?
            != request.namespace
        {
            return Err(CatalogError::Incomplete);
        }
        tr.get(&key(request), false)
            .await?
            .map(|bytes| decode(&bytes, request))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_s3_attributes_reject_corruption_and_another_upload_identity() {
        let request = ActiveFileRequest {
            namespace: Uuid::new_v4(),
            parent: Uuid::new_v4(),
            parent_path: b"/".to_vec(),
            name: b"object".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            barcode: None,
            expected_size: 3,
            expected_digest: ContentDigest::Sha256(Sha256::digest(b"abc").into()),
        };
        let metadata = FileMetadata::replacement(vec![Xattr {
            key: "tapers.s3.headers".into(),
            value: "e30=".into(),
            base64: true,
        }]);
        let bytes = encode(&request, &metadata).unwrap();
        assert_eq!(decode(&bytes, &request).unwrap().xattrs, metadata.xattrs);
        let other = ActiveFileRequest {
            operation_id: Uuid::new_v4(),
            ..request.clone()
        };
        assert!(decode(&bytes, &other).is_err());
        assert_ne!(key(&request), key(&other));
        let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        corrupt["xattrs"][0]["value"] = serde_json::json!("changed");
        assert!(decode(&serde_json::to_vec(&corrupt).unwrap(), &request).is_err());
        assert!(decode(&bytes[..bytes.len() - 1], &request).is_err());
        let huge = FileMetadata::replacement(vec![Xattr {
            key: "large".into(),
            value: "x".repeat(LIMIT),
            base64: false,
        }]);
        assert!(encode(&request, &huge).is_err());
    }
}
