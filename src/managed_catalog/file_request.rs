//! Immutable user operation; physical media and execution fences are selected
//! only when this operation is admitted, and are not part of its identity.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveFileRequest {
    pub namespace: Uuid,
    pub parent: Uuid,
    pub parent_path: Vec<u8>,
    pub name: Vec<u8>,
    pub file_id: Uuid,
    pub expected_version: Option<u64>,
    pub operation_id: Uuid,
    pub request_epoch: u64,
    pub library_serial: String,
    pub pool_id: Option<Uuid>,
    pub barcode: Option<String>,
    pub expected_size: u64,
    pub expected_digest: ContentDigest,
}

impl ActiveFileRequest {
    pub fn ltfs_path(&self) -> Result<String> {
        self.validate()?;
        let parent = std::str::from_utf8(&self.parent_path).expect("validated parent");
        let name = std::str::from_utf8(&self.name).expect("validated name");
        Ok(if parent == "/" {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        })
    }

    pub(super) fn validate(&self) -> Result<()> {
        let length = self
            .parent_path
            .len()
            .checked_add(self.name.len())
            .and_then(|n| n.checked_add(usize::from(self.parent_path != b"/")))
            .ok_or(CatalogError::Invalid("活动文件完整路径长度溢出"))?;
        if length > 2048 {
            return Err(CatalogError::Invalid("活动文件完整路径超过 2048 字节"));
        }
        let parent = std::str::from_utf8(&self.parent_path)
            .map_err(|_| CatalogError::Invalid("LTFS 父路径必须是 UTF-8"))?;
        let name = std::str::from_utf8(&self.name)
            .map_err(|_| CatalogError::Invalid("LTFS 文件名必须是 UTF-8"))?;
        if !crate::ltfs::xml_text_valid(parent) || !crate::ltfs::xml_text_valid(name) {
            return Err(CatalogError::Invalid("LTFS 写入路径包含 XML 1.0 非法字符"));
        }
        lookup::path_components(&self.parent_path)?;
        validate_name(&self.name)?;
        control::validate_physical_serial(&self.library_serial)?;
        if self.namespace.is_nil()
            || self.file_id.is_nil()
            || self.operation_id.is_nil()
            || self.pool_id.is_some_and(|id| id.is_nil())
            || self.request_epoch == 0
            || self.expected_digest == ContentDigest::Unknown
            || self.expected_version == Some(0)
            || self.expected_version.unwrap_or(0).checked_add(1).is_none()
            || self.barcode.as_ref().is_some_and(|s| {
                s.is_empty()
                    || s.len() > 64
                    || s.bytes().any(|b| !b.is_ascii_graphic() || b == b'/')
            })
        {
            return Err(CatalogError::Invalid("活动文件原逻辑请求身份或摘要无效"));
        }
        Ok(())
    }

    pub(super) fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/active-file-request/1");
        for id in [self.namespace, self.parent, self.file_id, self.operation_id] {
            hash.update(id.as_bytes());
        }
        hash.update(self.request_epoch.to_be_bytes());
        hash.update([u8::from(self.expected_version.is_some())]);
        hash.update(self.expected_version.unwrap_or(0).to_be_bytes());
        hash.update([u8::from(self.pool_id.is_some())]);
        if let Some(id) = self.pool_id {
            hash.update(id.as_bytes());
        }
        for bytes in [
            self.parent_path.as_slice(),
            self.name.as_slice(),
            self.library_serial.as_bytes(),
        ] {
            hash.update((bytes.len() as u32).to_be_bytes());
            hash.update(bytes);
        }
        hash.update([u8::from(self.barcode.is_some())]);
        if let Some(barcode) = &self.barcode {
            hash.update((barcode.len() as u32).to_be_bytes());
            hash.update(barcode.as_bytes());
        }
        hash.update(self.expected_size.to_be_bytes());
        hash.update([self.expected_digest.tag()]);
        hash.update(self.expected_digest.payload());
        hash.finalize().into()
    }
}

impl ActiveFileIntent {
    pub fn request(&self) -> ActiveFileRequest {
        ActiveFileRequest {
            namespace: self.namespace,
            parent: self.parent,
            parent_path: self.parent_path.clone(),
            name: self.name.clone(),
            file_id: self.file_id,
            expected_version: self.expected_version,
            operation_id: self.operation_id,
            request_epoch: self.request_epoch,
            library_serial: self.library_serial.clone(),
            pool_id: self.pool_id,
            barcode: self.barcode_constraint.clone(),
            expected_size: self.expected_size,
            expected_digest: self.expected_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_identity_binds_user_scope_and_body_not_execution_or_media() {
        let request = ActiveFileRequest {
            namespace: Uuid::new_v4(),
            parent: Uuid::new_v4(),
            parent_path: b"/dir".to_vec(),
            name: b"file".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: Some(1),
            operation_id: Uuid::new_v4(),
            request_epoch: 2,
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            barcode: Some("REQ0L8".into()),
            expected_size: 3,
            expected_digest: ContentDigest::Sha256([7; 32]),
        };
        request.validate().unwrap();
        let fingerprint = request.fingerprint();
        let changed = [
            ActiveFileRequest {
                namespace: Uuid::new_v4(),
                ..request.clone()
            },
            ActiveFileRequest {
                parent: Uuid::new_v4(),
                ..request.clone()
            },
            ActiveFileRequest {
                parent_path: b"/other".to_vec(),
                ..request.clone()
            },
            ActiveFileRequest {
                name: b"other".to_vec(),
                ..request.clone()
            },
            ActiveFileRequest {
                file_id: Uuid::new_v4(),
                ..request.clone()
            },
            ActiveFileRequest {
                expected_version: None,
                ..request.clone()
            },
            ActiveFileRequest {
                operation_id: Uuid::new_v4(),
                ..request.clone()
            },
            ActiveFileRequest {
                request_epoch: 3,
                ..request.clone()
            },
            ActiveFileRequest {
                library_serial: "OTHER".into(),
                ..request.clone()
            },
            ActiveFileRequest {
                pool_id: Some(Uuid::new_v4()),
                ..request.clone()
            },
            ActiveFileRequest {
                barcode: None,
                ..request.clone()
            },
            ActiveFileRequest {
                barcode: Some("REQ1L8".into()),
                ..request.clone()
            },
            ActiveFileRequest {
                expected_size: 4,
                ..request.clone()
            },
            ActiveFileRequest {
                expected_digest: ContentDigest::Sha256([8; 32]),
                ..request.clone()
            },
        ];
        for value in changed {
            value.validate().unwrap();
            assert_ne!(value.fingerprint(), fingerprint);
        }
        let physical = ActiveFileIntent {
            namespace: request.namespace,
            parent: request.parent,
            parent_path: request.parent_path.clone(),
            name: request.name.clone(),
            file_id: request.file_id,
            expected_version: request.expected_version,
            operation_id: request.operation_id,
            request_epoch: request.request_epoch,
            library_serial: request.library_serial.clone(),
            pool_id: request.pool_id,
            barcode_constraint: request.barcode.clone(),
            owner_epoch: 12,
            volume_id: Uuid::new_v4(),
            volume_generation: 3,
            volume_scan_id: Uuid::new_v4(),
            expected_size: request.expected_size,
            expected_digest: request.expected_digest,
        };
        let another = ActiveFileIntent {
            owner_epoch: 13,
            volume_id: Uuid::new_v4(),
            volume_generation: 7,
            volume_scan_id: Uuid::new_v4(),
            ..physical.clone()
        };
        assert_eq!(physical.request(), request);
        assert_eq!(another.request().fingerprint(), fingerprint);
        physical::validate_active_file_intent(&physical).unwrap();
        physical::validate_active_file_intent(&another).unwrap();
        assert_ne!(physical, another);
    }
}
