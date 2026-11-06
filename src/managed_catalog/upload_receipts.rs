//! Original S4 upload evidence, retained independently of mutable inode heads.
//! @u and its inline attribute child belong to namespace cleanup, just like M.
use super::control::management;
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct ActiveUploadReceipt {
    pub receipt: CommitReceipt,
    pub version: u64,
    pub barcode: String,
    pub attributes: NodeAttributes,
    pub native_symlink: bool,
}

#[derive(Clone, Debug)]
pub enum UploadReceiptOutcome {
    Unconfirmed,
    Committed(Box<ActiveUploadReceipt>),
    Rejected(PublishOutcome),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    intent: String,
    barcode: String,
    attributes_len: u32,
    attributes_digest: [u8; 32],
}

fn record_key(namespace: Uuid, epoch: u64, operation: Uuid) -> Vec<u8> {
    let mut out = management::prefix(namespace, b'u');
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(operation.as_bytes());
    out
}

fn inline_key(key: &[u8]) -> Vec<u8> {
    [key, &[0]].concat()
}

impl Record {
    fn encode(&self) -> Result<Vec<u8>> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| CatalogError::Invalid("原回执编码失败"))?;
        let hash = Sha256::new()
            .chain_update(b"tape-rs/upload-receipt/1")
            .chain_update(&bytes)
            .finalize();
        Ok([hash.as_slice(), bytes.as_slice()].concat())
    }

    fn decode(
        bytes: &[u8],
        namespace: Uuid,
        epoch: u64,
        operation: Uuid,
    ) -> Result<(Self, CommitReceipt)> {
        let corrupt = || CatalogError::Corrupt("原上传回执编码或身份无效");
        if !(33..=16_384).contains(&bytes.len())
            || Sha256::new()
                .chain_update(b"tape-rs/upload-receipt/1")
                .chain_update(&bytes[32..])
                .finalize()
                .as_slice()
                != &bytes[..32]
        {
            return Err(corrupt());
        }
        let record: Self = serde_json::from_slice(&bytes[32..]).map_err(|_| corrupt())?;
        if record.version != 1
            || record.barcode.is_empty()
            || record.barcode.len() > 64
            || record
                .barcode
                .bytes()
                .any(|b| !b.is_ascii_graphic() || b == b'/')
            || !(1..=MAX_REPRESENTABLE).contains(&(record.attributes_len as usize))
        {
            return Err(corrupt());
        }
        let intent = URL_SAFE_NO_PAD
            .decode(&record.intent)
            .map_err(|_| corrupt())?;
        let receipt = requests::decode_intent(&intent, namespace, epoch, operation)?;
        if receipt.metadata_digest != Some(record.attributes_digest) {
            return Err(corrupt());
        }
        Ok((record, receipt))
    }

    fn manifest(&self) -> AttributeManifest {
        AttributeManifest {
            len: self.attributes_len,
            digest: self.attributes_digest,
            chunks: (self.attributes_len as usize).div_ceil(CHUNK_SIZE) as u16,
        }
    }
}

pub(super) struct Preparation {
    key: Vec<u8>,
    value: Vec<u8>,
    inline: Option<Vec<u8>>,
}

impl Preparation {
    pub(super) fn apply(self, tr: &Transaction) {
        if let Some(inline) = self.inline {
            tr.set(&inline_key(&self.key), &inline);
        }
        tr.set(&self.key, &self.value);
    }
}

// All reads and checks precede the caller's first transaction mutation.
pub(super) async fn prepare_at(
    tr: &Transaction,
    receipt: &CommitReceipt,
    permit: u16,
    barcode: &str,
    attributes: &NodeAttributes,
    already_prepared: bool,
) -> Result<Option<Preparation>> {
    let bytes = attributes.bytes().ok_or(CatalogError::Incomplete)?;
    let key = record_key(
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    );
    let existing = tr.get(&key, false).await?;
    let inline = tr.get(&inline_key(&key), false).await?;
    if already_prepared {
        let (record, saved) = Record::decode(
            existing
                .as_deref()
                .ok_or(CatalogError::Corrupt("已准备文件缺少原回执"))?,
            receipt.namespace,
            receipt.request_epoch,
            receipt.operation_id,
        )?;
        if receipt_fingerprint(&saved) != receipt_fingerprint(receipt)
            || record.barcode != barcode
            || record.attributes_len as usize != bytes.len()
            || inline.as_deref() != attributes.inline_bytes()
        {
            return Err(CatalogError::Corrupt("已准备文件的原回执不一致"));
        }
        return Ok(None);
    }
    if existing.is_some() || inline.is_some() {
        return Err(CatalogError::Corrupt("原上传回执缺少文件准备记录"));
    }
    let record = Record {
        version: 1,
        intent: URL_SAFE_NO_PAD.encode(requests::encode_intent(receipt, permit)),
        barcode: barcode.to_owned(),
        attributes_len: bytes.len() as u32,
        attributes_digest: attributes.digest(),
    };
    let value = record.encode()?;
    Record::decode(
        &value,
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    )?;
    Ok(Some(Preparation {
        key,
        value,
        inline: attributes.inline_bytes().map(<[u8]>::to_vec),
    }))
}

pub(super) async fn require_at(
    tr: &Transaction,
    receipt: &CommitReceipt,
    barcode: &str,
    manifest: AttributeManifest,
) -> Result<()> {
    let raw = tr
        .get(
            &record_key(
                receipt.namespace,
                receipt.request_epoch,
                receipt.operation_id,
            ),
            false,
        )
        .await?
        .ok_or(CatalogError::Corrupt("活动终结缺少原上传回执"))?;
    let (record, saved) = Record::decode(
        &raw,
        receipt.namespace,
        receipt.request_epoch,
        receipt.operation_id,
    )?;
    if receipt_fingerprint(&saved) != receipt_fingerprint(receipt)
        || record.barcode != barcode
        || record.manifest() != manifest
    {
        return Err(CatalogError::Corrupt("原上传回执与活动终结不一致"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Capture the terminal fingerprint and original evidence at one read version.
    /// Never resolve the current pathname, inode head, placement or owner.
    pub async fn active_upload_receipt(
        &self,
        intent: &ActiveFileRequest,
        route: ActiveLibraryRoute,
        max_attributes_bytes: usize,
    ) -> Result<UploadReceiptOutcome> {
        intent.validate()?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, &intent.library_serial, Some(route))
                .await?;
        if namespace != intent.namespace {
            return Err(CatalogError::Invalid("上传不属于活动命名空间"));
        }
        let window = requests::window(&tr, namespace).await?;
        if intent.request_epoch < window.first_retained {
            return Ok(UploadReceiptOutcome::Rejected(
                PublishOutcome::RequestExpired,
            ));
        }
        if intent.request_epoch > window.accepting {
            return Ok(UploadReceiptOutcome::Rejected(
                PublishOutcome::RequestEpochClosed,
            ));
        }
        let Some(terminal) = tr
            .get(
                &op_key(namespace, intent.request_epoch, intent.operation_id),
                false,
            )
            .await?
        else {
            return Ok(UploadReceiptOutcome::Unconfirmed);
        };
        let version = match requests::reservation_terminal_result(&terminal, &intent.fingerprint())?
        {
            PublishOutcome::AlreadyApplied(version) => version,
            result => return Ok(UploadReceiptOutcome::Rejected(result)),
        };
        let key = record_key(namespace, intent.request_epoch, intent.operation_id);
        let raw = tr
            .get(&key, false)
            .await?
            .ok_or(CatalogError::Corrupt("上传终态缺少原回执"))?;
        let (record, receipt) =
            Record::decode(&raw, namespace, intent.request_epoch, intent.operation_id)?;
        if receipt_fingerprint(&receipt) != terminal[1..33]
            || receipt.expected_version.unwrap_or(0) + 1 != version
        {
            return Err(CatalogError::Corrupt("原上传回执与终态不一致"));
        }
        if record.attributes_len as usize > max_attributes_bytes {
            return Err(CatalogError::Invalid("原上传属性超过读取预算"));
        }
        let attributes = if record.attributes_len as usize <= INLINE_LIMIT {
            let bytes = tr
                .get(&inline_key(&key), false)
                .await?
                .ok_or(CatalogError::Corrupt("原上传属性缺失"))?;
            NodeAttributes::decode(&bytes)?
        } else {
            NodeAttributes::from_manifest(record.manifest())?
        };
        drop(tr);
        let attributes = self.hydrate_attributes(namespace, attributes).await?;
        if attributes.digest() != record.attributes_digest
            || attributes.bytes().map(<[u8]>::len) != Some(record.attributes_len as usize)
        {
            return Err(CatalogError::Corrupt("原上传属性长度或摘要不一致"));
        }
        Ok(UploadReceiptOutcome::Committed(Box::new(
            ActiveUploadReceipt {
                receipt,
                version,
                barcode: record.barcode,
                native_symlink: attributes.link_kind()? == link_kind::LinkKind::Native,
                attributes,
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_record_binds_original_scope_attributes_and_detects_damage() {
        let attributes = NodeAttributes::new(
            crate::ltfs::index::NodeMeta {
                file_uid: 11,
                ..Default::default()
            },
            vec![],
            None,
        )
        .unwrap();
        let mut receipt = CommitReceipt {
            namespace: Uuid::new_v4(),
            parent: Uuid::new_v4(),
            name: b"a".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: Some(3),
            operation_id: Uuid::new_v4(),
            request_epoch: 7,
            copy_id: Uuid::nil(),
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            owner_epoch: 12,
            volume_id: Uuid::new_v4(),
            volume_generation: 2,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 11,
            size: 3,
            content_digest: ContentDigest::Sha256([3; 32]),
            metadata_digest: Some(attributes.digest()),
        };
        receipt.copy_id = derive_copy_id(&receipt);
        let mut record = Record {
            version: 1,
            intent: URL_SAFE_NO_PAD.encode(requests::encode_intent(&receipt, 3)),
            barcode: "TEST01L8".into(),
            attributes_len: attributes.bytes().unwrap().len() as u32,
            attributes_digest: attributes.digest(),
        };
        let bytes = record.encode().unwrap();
        let decode =
            |bytes: &[u8]| Record::decode(bytes, receipt.namespace, 7, receipt.operation_id);
        let (_, saved) = decode(&bytes).unwrap();
        assert_eq!(receipt_fingerprint(&saved), receipt_fingerprint(&receipt));
        for end in 0..bytes.len() {
            assert!(decode(&bytes[..end]).is_err(), "{end}");
        }
        for index in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[index] ^= 1;
            assert!(decode(&damaged).is_err(), "{index}");
        }
        assert!(Record::decode(&bytes, Uuid::new_v4(), 7, receipt.operation_id).is_err());
        assert!(Record::decode(&bytes, receipt.namespace, 8, receipt.operation_id).is_err());
        assert!(Record::decode(&bytes, receipt.namespace, 7, Uuid::new_v4()).is_err());
        record.attributes_digest[0] ^= 1;
        assert!(decode(&record.encode().unwrap()).is_err());
        record.attributes_digest = attributes.digest();
        record.attributes_len = 0;
        assert!(decode(&record.encode().unwrap()).is_err());
        let record_key = record_key(receipt.namespace, 7, receipt.operation_id);
        assert!(
            record_key.starts_with(
                &[key(b'@'), receipt.namespace.as_bytes().to_vec(), vec![b'u']].concat()
            )
        );
        assert!(cleanup::NAMESPACE_ROWS.contains(&b'@'));
    }
}
