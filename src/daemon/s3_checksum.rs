//! Fixed-length S3 upload checksums, shared by both storage routes.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum UploadChecksum {
    None,
    Sha256([u8; 32]),
    Crc32([u8; 4]),
}

impl UploadChecksum {
    pub(super) fn parse(headers: &HeaderMap) -> Result<Self, S3Error> {
        let mut checksum = Self::None;
        for name in headers.keys() {
            if !name.as_str().starts_with("x-amz-checksum-")
                && name != "x-amz-sdk-checksum-algorithm"
            {
                continue;
            }
            if headers.get_all(name).iter().count() != 1 {
                return Err(S3Error::new(400, "InvalidArgument", "上传校验请求头重复"));
            }
            if name == "x-amz-sdk-checksum-algorithm" {
                continue;
            }
            let length = match name.as_str() {
                "x-amz-checksum-sha256" => 32,
                "x-amz-checksum-crc32" => 4,
                _ => return Err(unsupported()),
            };
            if !matches!(checksum, Self::None) {
                return Err(S3Error::new(
                    400,
                    "InvalidArgument",
                    "一次上传只支持一种 checksum",
                ));
            }
            let bytes = STANDARD
                .decode(header_text(headers, name.as_str())?.expect("present header"))
                .ok()
                .filter(|v| v.len() == length)
                .ok_or_else(|| S3Error::new(400, "InvalidDigest", "上传 checksum 格式无效"))?;
            checksum = if length == 4 {
                Self::Crc32(bytes.try_into().expect("CRC32 length"))
            } else {
                Self::Sha256(bytes.try_into().expect("SHA256 length"))
            };
        }
        if let Some(algorithm) = header_text(headers, "x-amz-sdk-checksum-algorithm")? {
            if !matches!(algorithm, "SHA256" | "CRC32") {
                return Err(unsupported());
            }
            if !matches!(
                (algorithm, checksum),
                ("SHA256", Self::Sha256(_)) | ("CRC32", Self::Crc32(_))
            ) {
                return Err(S3Error::new(
                    400,
                    "InvalidRequest",
                    "checksum 算法与校验头不匹配",
                ));
            }
        }
        Ok(checksum)
    }

    pub(super) fn matches(self, sha256: &[u8], crc32: u32) -> bool {
        match self {
            Self::None => true,
            Self::Sha256(expected) => expected == sha256,
            Self::Crc32(expected) => expected == crc32.to_be_bytes(),
        }
    }

    pub(super) fn header(self) -> Option<(&'static str, String)> {
        match self {
            Self::None => None,
            Self::Sha256(bytes) => Some(("x-amz-checksum-sha256", STANDARD.encode(bytes))),
            Self::Crc32(bytes) => Some(("x-amz-checksum-crc32", STANDARD.encode(bytes))),
        }
    }

    pub(super) fn save_crc32(self, saved: &mut BTreeMap<String, String>, sha256: &[u8]) {
        if let Self::Crc32(_) = self {
            let (name, value) = self.header().expect("CRC32 header");
            saved.insert(name.into(), value);
            // A later FUSE write may preserve these attributes. Only expose
            // CRC32 while this binding still matches the current content.
            saved.insert("x-amz-checksum-sha256".into(), STANDARD.encode(sha256));
        }
    }
}

pub(super) fn stored_crc32<'a>(
    saved: &'a BTreeMap<String, String>,
    sha256: &[u8],
) -> Option<&'a str> {
    let bound = STANDARD.decode(saved.get("x-amz-checksum-sha256")?).ok()?;
    let value = saved.get("x-amz-checksum-crc32")?;
    (bound == sha256 && STANDARD.decode(value).ok()?.len() == 4).then_some(value)
}

fn unsupported() -> S3Error {
    S3Error::new(501, "NotImplemented", "仅支持 SHA256 和 CRC32 checksum")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn crc32_is_streamed_in_network_byte_order_and_checks_corruption() {
        // Independent IEEE CRC32 check vector; big endian Base64 is y/Q5Jg==.
        let checksum = UploadChecksum::parse(&headers(&[
            ("x-amz-checksum-crc32", "y/Q5Jg=="),
            ("x-amz-sdk-checksum-algorithm", "CRC32"),
        ]))
        .unwrap();
        let mut crc = crc32fast::Hasher::new();
        for part in b"123456789".chunks(2) {
            crc.update(part);
        }
        let crc = crc.finalize();
        assert_eq!(crc, 0xcbf43926);
        assert!(checksum.matches(&[], crc));
        assert!(!checksum.matches(&[], crc32fast::hash(b"123456788")));
        assert_eq!(
            checksum.header(),
            Some(("x-amz-checksum-crc32", "y/Q5Jg==".into()))
        );
        let empty =
            UploadChecksum::parse(&headers(&[("x-amz-checksum-crc32", "AAAAAA==")])).unwrap();
        assert!(empty.matches(&[], crc32fast::hash(b"")));
    }

    #[test]
    fn ambiguous_unsupported_and_malformed_checksums_fail_before_body() {
        for values in [
            vec![("x-amz-checksum-crc32", "bad")],
            vec![("x-amz-checksum-crc32", "AAAA")],
            vec![("x-amz-checksum-crc32", "AAAAAAA=")],
            vec![("x-amz-checksum-crc32c", "AAAAAA==")],
            vec![("x-amz-sdk-checksum-algorithm", "CRC32")],
            vec![("x-amz-sdk-checksum-algorithm", "SHA1")],
            vec![
                ("x-amz-checksum-crc32", "AAAAAA=="),
                ("x-amz-sdk-checksum-algorithm", "SHA256"),
            ],
            vec![
                ("x-amz-checksum-crc32", "AAAAAA=="),
                (
                    "x-amz-checksum-sha256",
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                ),
            ],
            vec![
                ("x-amz-checksum-crc32", "AAAAAA=="),
                ("x-amz-checksum-crc32", "AAAAAA=="),
            ],
            vec![
                ("x-amz-sdk-checksum-algorithm", "CRC32"),
                ("x-amz-sdk-checksum-algorithm", "CRC32"),
            ],
        ] {
            assert!(
                UploadChecksum::parse(&headers(&values)).is_err(),
                "{values:?}"
            );
        }
    }

    #[test]
    fn preserved_attributes_do_not_report_crc32_for_replaced_content() {
        let checksum = UploadChecksum::Crc32(0xcbf43926u32.to_be_bytes());
        let original = Sha256::digest(b"123456789");
        let replacement = Sha256::digest(b"other");
        let mut saved = BTreeMap::new();
        checksum.save_crc32(&mut saved, &original);
        assert_eq!(stored_crc32(&saved, &original), Some("y/Q5Jg=="));
        assert_eq!(stored_crc32(&saved, &replacement), None);
    }
}
