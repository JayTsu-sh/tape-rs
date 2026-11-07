use super::{CatalogError, Result};
use crate::ltfs::index::Xattr;

pub(super) const NATIVE_SYMLINK: &str = "tapers.fdb.nativeSymlink";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinkKind {
    None,
    Native,
    EeAlias,
}

pub(super) fn native_marker() -> Xattr {
    Xattr {
        key: NATIVE_SYMLINK.into(),
        value: "1".into(),
        base64: false,
    }
}

/// Explicit native links retain their type even when their arbitrary target
/// resembles an EE data path. Unstamped foreign LTFS links use EE conventions.
pub(super) fn classify(xattrs: &[Xattr], target: Option<&str>) -> Result<LinkKind> {
    let mut markers = xattrs.iter().filter(|attr| attr.key == NATIVE_SYMLINK);
    if let Some(marker) = markers.next() {
        if markers.next().is_some() || marker.base64 || marker.value != "1" || target.is_none() {
            return Err(CatalogError::Invalid("原生符号链接类型标记无效"));
        }
        return Ok(LinkKind::Native);
    }
    Ok(match target {
        None => LinkKind::None,
        Some(target) if target.contains(".LTFSEE_DATA/") => LinkKind::EeAlias,
        Some(_) => LinkKind::Native,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_native_type_overrides_ee_target_text_and_rejects_bad_stamps() {
        for target in [
            "body",
            "../missing",
            ".LTFSEE_DATA/missing",
            "/.LTFSEE_DATA/a",
        ] {
            assert_eq!(
                classify(&[native_marker()], Some(target)).unwrap(),
                LinkKind::Native
            );
        }
        assert_eq!(classify(&[], None).unwrap(), LinkKind::None);
        assert_eq!(classify(&[], Some("body")).unwrap(), LinkKind::Native);
        assert_eq!(
            classify(&[], Some(".LTFSEE_DATA/a")).unwrap(),
            LinkKind::EeAlias
        );
        assert!(classify(&[native_marker()], None).is_err());
        assert!(classify(&[native_marker(), native_marker()], Some("a")).is_err());
        for marker in [
            Xattr {
                value: "2".into(),
                ..native_marker()
            },
            Xattr {
                value: "MQ==".into(),
                base64: true,
                ..native_marker()
            },
        ] {
            assert!(classify(&[marker], Some("a")).is_err());
        }
    }

    #[test]
    fn attributes_and_mutation_inputs_enforce_the_type_stamp() {
        use super::super::NodeAttributes;
        use super::super::node_mutation::{NodeMutation, XattrMode};
        use crate::ltfs::index::NodeMeta;
        let meta = NodeMeta {
            file_uid: 1,
            ..Default::default()
        };
        let attrs = NodeAttributes::new(
            meta.clone(),
            vec![native_marker()],
            Some(".LTFSEE_DATA/missing".into()),
        )
        .unwrap();
        assert_eq!(
            NodeAttributes::decode(attrs.bytes().unwrap()).unwrap(),
            attrs
        );
        assert!(NodeAttributes::new(meta, vec![native_marker()], None).is_err());
        // Change the actual stored marker value without changing the record's
        // valid lengths; decoding must validate the type as well as framing.
        let mut bytes = attrs.bytes().unwrap().to_vec();
        let at = bytes
            .windows(NATIVE_SYMLINK.len())
            .position(|w| w == NATIVE_SYMLINK.as_bytes())
            .unwrap()
            + NATIVE_SYMLINK.len()
            + 4;
        assert_eq!(bytes[at], b'1');
        bytes[at] = b'2';
        assert!(NodeAttributes::decode(&bytes).is_err());
        for mutation in [
            NodeMutation::SetXattr {
                attribute: native_marker(),
                mode: XattrMode::Upsert,
            },
            NodeMutation::RemoveXattr {
                key: NATIVE_SYMLINK.into(),
            },
        ] {
            assert!(mutation.validate_input().is_err());
        }
    }
}
