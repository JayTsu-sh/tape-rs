use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOrigin {
    FileRequest {
        request_epoch: u64,
    },
    Imported,
    NodeLink {
        management_epoch: u64,
        operation: Uuid,
    },
}

impl CopyOrigin {
    pub(super) fn receipt_epoch(self) -> Option<u64> {
        match self {
            Self::FileRequest { request_epoch } => Some(request_epoch),
            Self::Imported | Self::NodeLink { .. } => None,
        }
    }

    fn validate(self) -> Result<()> {
        match self {
            Self::FileRequest { request_epoch: 0 } => {
                Err(CatalogError::Corrupt("副本文件请求代次为零"))
            }
            Self::NodeLink {
                management_epoch,
                operation,
            } if management_epoch == 0 || operation.is_nil() => {
                Err(CatalogError::Corrupt("副本链接管理来源身份无效"))
            }
            _ => Ok(()),
        }
    }

    pub(super) fn bytes(self) -> Vec<u8> {
        let mut bytes = Vec::new();
        match self {
            Self::FileRequest { request_epoch } => {
                bytes.push(1);
                bytes.extend_from_slice(&request_epoch.to_be_bytes());
            }
            Self::Imported => bytes.push(2),
            Self::NodeLink {
                management_epoch,
                operation,
            } => {
                bytes.push(3);
                bytes.extend_from_slice(&management_epoch.to_be_bytes());
                bytes.extend_from_slice(operation.as_bytes());
            }
        }
        bytes
    }
}

pub(super) enum StoredCopy {
    Present(PhysicalCopy),
    Reclaimed { version: u64, origin: CopyOrigin },
}

impl StoredCopy {
    pub(super) fn version(&self) -> u64 {
        match self {
            Self::Present(copy) => copy.version,
            Self::Reclaimed { version, .. } => *version,
        }
    }

    pub(super) fn receipt_epoch(&self) -> Option<u64> {
        match self {
            Self::Present(copy) => copy.origin.receipt_epoch(),
            Self::Reclaimed { origin, .. } => origin.receipt_epoch(),
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(CatalogError::Corrupt("副本记录长度溢出"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or(CatalogError::Corrupt("副本记录截断"))?;
        self.at = end;
        Ok(bytes)
    }
    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self
            .take(N)?
            .try_into()
            .expect("checked record field length"))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.fixed()?))
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.fixed::<1>()?[0])
    }
    fn uuid(&mut self) -> Result<Uuid> {
        Ok(Uuid::from_bytes(self.fixed()?))
    }
    fn origin(&mut self) -> Result<CopyOrigin> {
        let origin = match self.byte()? {
            1 => CopyOrigin::FileRequest {
                request_epoch: self.u64()?,
            },
            2 => CopyOrigin::Imported,
            3 => CopyOrigin::NodeLink {
                management_epoch: self.u64()?,
                operation: self.uuid()?,
            },
            _ => return Err(CatalogError::Corrupt("副本来源类型未知")),
        };
        origin.validate()?;
        Ok(origin)
    }
    fn finish(self) -> Result<()> {
        if self.at != self.bytes.len() {
            return Err(CatalogError::Corrupt("副本记录含多余尾部"));
        }
        Ok(())
    }
}

fn validate(copy: &PhysicalCopy) -> Result<()> {
    if copy.version == 0 || copy.ltfs_file_uid == 0 {
        return Err(CatalogError::Corrupt("物理副本版本或数据 UID 为零"));
    }
    validate_scan(
        copy.volume_id,
        VolumeScan {
            generation: copy.volume_generation,
            id: copy.volume_scan_id,
        },
    )
    .map_err(|_| CatalogError::Corrupt("物理副本卷或扫描身份无效"))?;
    copy.origin.validate()?;
    if let Some(media) = &copy.media {
        validate_media(media, copy.ltfs_file_uid)
            .map_err(|_| CatalogError::Corrupt("物理副本介质来源无效"))?;
        if media.native_symlink && (copy.size != 0 || copy.content_digest != ContentDigest::Unknown)
        {
            return Err(CatalogError::Corrupt("原生链接副本不能携带文件数据"));
        }
    }
    match copy.origin {
        CopyOrigin::Imported if copy.media.is_none() => {
            Err(CatalogError::Corrupt("整卷导入副本缺少介质映射"))
        }
        CopyOrigin::NodeLink { .. }
            if !copy
                .media
                .as_ref()
                .is_some_and(|media| media.native_symlink) =>
        {
            Err(CatalogError::Corrupt("节点链接来源要求原生链接介质类型"))
        }
        _ => Ok(()),
    }
}

pub(super) fn encode_physical_copy(copy: &PhysicalCopy) -> Result<Vec<u8>> {
    validate(copy)?;
    let mut out = b"R1".to_vec();
    out.extend_from_slice(&copy.version.to_be_bytes());
    out.extend_from_slice(copy.volume_id.as_bytes());
    out.extend_from_slice(&copy.volume_generation.to_be_bytes());
    out.extend_from_slice(copy.volume_scan_id.as_bytes());
    out.extend_from_slice(&copy.ltfs_file_uid.to_be_bytes());
    out.extend_from_slice(&copy.size.to_be_bytes());
    out.push(copy.content_digest.tag());
    out.extend_from_slice(&copy.content_digest.payload()[..copy.content_digest.compact_len()]);
    out.extend_from_slice(&copy.origin.bytes());
    match &copy.media {
        None => out.push(0),
        Some(media) => {
            out.push(if media.native_symlink {
                3
            } else if media.ee_alias {
                2
            } else {
                1
            });
            out.extend_from_slice(&media.version.0.to_be_bytes());
            out.extend_from_slice(&media.version.1.to_be_bytes());
            out.extend_from_slice(&media.logical_file_uid.to_be_bytes());
            out.extend_from_slice(&(media.physical_path.len() as u16).to_be_bytes());
            out.extend_from_slice(media.physical_path.as_bytes());
        }
    }
    Ok(out)
}

pub(super) fn decode_copy(bytes: &[u8]) -> Result<PhysicalCopy> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.fixed::<2>()? != *b"R1" {
        return Err(CatalogError::Corrupt("物理副本必须使用 R1 格式"));
    }
    let version = reader.u64()?;
    let volume_id = reader.uuid()?;
    let volume_generation = reader.u64()?;
    let volume_scan_id = reader.uuid()?;
    let ltfs_file_uid = reader.u64()?;
    let size = reader.u64()?;
    let tag = reader.byte()?;
    let length = match tag {
        0 => 0,
        1 => 32,
        2 => 16,
        _ => return Err(CatalogError::Corrupt("物理副本摘要算法未知")),
    };
    let content_digest = ContentDigest::decode_compact(tag, reader.take(length)?)?;
    let origin = reader.origin()?;
    let kind = reader.byte()?;
    let media = match kind {
        0 => None,
        1..=3 => {
            let version = (reader.u64()?, reader.u64()?);
            let logical_file_uid = reader.u64()?;
            let length = u16::from_be_bytes(reader.fixed()?) as usize;
            let physical_path = std::str::from_utf8(reader.take(length)?)
                .map_err(|_| CatalogError::Corrupt("副本物理路径不是 UTF-8"))?
                .to_owned();
            Some(MediaProvenance {
                version,
                logical_file_uid,
                physical_path,
                ee_alias: kind == 2,
                native_symlink: kind == 3,
            })
        }
        _ => return Err(CatalogError::Corrupt("物理副本介质类型未知")),
    };
    reader.finish()?;
    let copy = PhysicalCopy {
        version,
        origin,
        volume_id,
        volume_generation,
        volume_scan_id,
        ltfs_file_uid,
        size,
        content_digest,
        media,
    };
    validate(&copy)?;
    Ok(copy)
}

pub(super) fn encode_reclaimed_copy(version: u64, origin: CopyOrigin) -> Vec<u8> {
    let mut out = b"X1".to_vec();
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&origin.bytes());
    out
}

pub(super) fn decode_stored_copy(bytes: &[u8]) -> Result<StoredCopy> {
    if bytes.starts_with(b"X1") {
        let mut reader = Reader { bytes, at: 2 };
        let version = reader.u64()?;
        let origin = reader.origin()?;
        reader.finish()?;
        if version == 0 {
            return Err(CatalogError::Corrupt("回收副本版本为零"));
        }
        Ok(StoredCopy::Reclaimed { version, origin })
    } else {
        decode_copy(bytes).map(StoredCopy::Present)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(origin: CopyOrigin, kind: u8) -> PhysicalCopy {
        // Codec data only; no claim of a device commit or serving permission.
        PhysicalCopy {
            version: u64::MAX,
            origin,
            volume_id: Uuid::new_v4(),
            volume_generation: 3,
            volume_scan_id: Uuid::new_v4(),
            ltfs_file_uid: 5,
            size: if kind == 3 { 0 } else { 23 },
            content_digest: if kind == 3 {
                ContentDigest::Unknown
            } else {
                ContentDigest::Sha256([9; 32])
            },
            media: (kind != 0).then(|| MediaProvenance {
                version: (12, 34),
                logical_file_uid: if kind == 2 { 6 } else { 5 },
                physical_path: if kind == 2 {
                    ".LTFSEE_DATA/body"
                } else {
                    "body"
                }
                .into(),
                ee_alias: kind == 2,
                native_symlink: kind == 3,
            }),
        }
    }

    #[test]
    fn copy_origin_roundtrips_with_strict_current_records_and_media_types() {
        for (origin, kind) in [
            (CopyOrigin::FileRequest { request_epoch: 7 }, 0),
            (CopyOrigin::Imported, 1),
            (CopyOrigin::Imported, 2),
            (CopyOrigin::Imported, 3),
            (
                CopyOrigin::NodeLink {
                    management_epoch: 9,
                    operation: Uuid::new_v4(),
                },
                3,
            ),
        ] {
            let value = fixture(origin, kind);
            let bytes = encode_physical_copy(&value).unwrap();
            assert!(bytes.starts_with(b"R1"));
            assert_eq!(decode_copy(&bytes).unwrap(), value);
            for end in 0..bytes.len() {
                assert!(
                    decode_copy(&bytes[..end]).is_err(),
                    "truncated {kind} at {end}"
                );
            }
            let mut bad = bytes.clone();
            bad.push(0);
            assert!(decode_copy(&bad).is_err());
            let mut bad = bytes.clone();
            bad[1] = b'2';
            assert!(decode_copy(&bad).is_err());
            assert!(
                decode_copy(&bytes[2..]).is_err(),
                "unmarked prototype record"
            );
            assert!(decode_stored_copy(&[0xff; 17]).is_err());
        }
        let mut bad = fixture(CopyOrigin::FileRequest { request_epoch: 0 }, 0);
        assert!(encode_physical_copy(&bad).is_err());
        bad.origin = CopyOrigin::Imported;
        assert!(
            encode_physical_copy(&bad).is_err(),
            "import has no physical mapping"
        );
        bad.origin = CopyOrigin::NodeLink {
            management_epoch: 9,
            operation: Uuid::new_v4(),
        };
        assert!(
            encode_physical_copy(&bad).is_err(),
            "node link has no native-link mapping"
        );
        bad = fixture(
            CopyOrigin::NodeLink {
                management_epoch: 9,
                operation: Uuid::nil(),
            },
            3,
        );
        assert!(encode_physical_copy(&bad).is_err());
        bad.origin = CopyOrigin::NodeLink {
            management_epoch: 9,
            operation: Uuid::new_v4(),
        };
        bad.size = 1;
        assert!(encode_physical_copy(&bad).is_err());
        bad.size = 0;
        bad.content_digest = ContentDigest::Sha256([0; 32]);
        assert!(encode_physical_copy(&bad).is_err());
        let mut bad = fixture(CopyOrigin::FileRequest { request_epoch: 7 }, 0);
        bad.volume_scan_id = Uuid::nil();
        assert!(
            encode_physical_copy(&bad).is_err(),
            "all origins validate base identity"
        );
    }

    #[test]
    fn reclaimed_origins_keep_their_own_identity_and_only_file_requests_have_receipt_epochs() {
        for origin in [
            CopyOrigin::FileRequest { request_epoch: 7 },
            CopyOrigin::Imported,
            CopyOrigin::NodeLink {
                management_epoch: 9,
                operation: Uuid::new_v4(),
            },
        ] {
            for version in [1, u64::MAX] {
                let bytes = encode_reclaimed_copy(version, origin);
                let stored = decode_stored_copy(&bytes).unwrap();
                assert!(
                    matches!(stored, StoredCopy::Reclaimed { version: v, origin: o } if v == version && o == origin)
                );
                assert_eq!(stored.receipt_epoch(), origin.receipt_epoch());
                assert_eq!(
                    stored.receipt_epoch().is_some(),
                    matches!(origin, CopyOrigin::FileRequest { .. })
                );
                assert!(decode_copy(&bytes).is_err());
                for end in 0..bytes.len() {
                    assert!(decode_stored_copy(&bytes[..end]).is_err());
                }
                let mut bad = bytes.clone();
                bad.push(0);
                assert!(decode_stored_copy(&bad).is_err());
                let mut bad = bytes;
                bad[1] = b'2';
                assert!(decode_stored_copy(&bad).is_err());
            }
        }
        assert!(decode_stored_copy(&encode_reclaimed_copy(0, CopyOrigin::Imported)).is_err());
        assert!(
            decode_stored_copy(&encode_reclaimed_copy(
                1,
                CopyOrigin::FileRequest { request_epoch: 0 }
            ))
            .is_err()
        );
    }
}
