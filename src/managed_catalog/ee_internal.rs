//! Explicit physical ownership of the root EE data directory. The marker is
//! covered by node source/result tree proofs and is never a logical inode.
use super::{CatalogError, NodeKind, Result};
use crate::ltfs::index::Xattr;

pub(super) const DIRECTORY: &str = ".LTFSEE_DATA";
pub(super) const MARKER: &str = "tapers.fdb.eeInternalDirectory";

pub(super) fn declared(xattrs: &[Xattr]) -> Result<bool> {
    let mut markers = xattrs.iter().filter(|attr| attr.key == MARKER);
    let Some(marker) = markers.next() else {
        return Ok(false);
    };
    if markers.next().is_some() || marker.base64 || marker.value != "1" {
        return Err(CatalogError::Invalid("EE 内部目录归属标记无效"));
    }
    Ok(true)
}

pub(super) fn require_location(xattrs: &[Xattr], path: &str, kind: NodeKind) -> Result<bool> {
    let owned = declared(xattrs)?;
    if owned && (kind != NodeKind::Directory || !matches!(path, DIRECTORY | "/.LTFSEE_DATA")) {
        return Err(CatalogError::Invalid(
            "EE 归属标记只能位于卷根的内部数据目录",
        ));
    }
    Ok(owned)
}

pub(super) fn stamp(xattrs: &[Xattr]) -> Result<Vec<Xattr>> {
    let mut after = xattrs.to_vec();
    if !declared(xattrs)? {
        after.push(Xattr {
            key: MARKER.into(),
            value: "1".into(),
            base64: false,
        });
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::NodeMeta;
    use crate::managed_catalog::NodeAttributes;
    use crate::managed_catalog::node_mutation::{NodeMutation, XattrMode};

    #[test]
    fn ownership_is_explicit_strict_location_bound_and_not_user_mutable() {
        let private = Xattr {
            key: "foreign.private".into(),
            value: "kept".into(),
            base64: false,
        };
        let marked = stamp(std::slice::from_ref(&private)).unwrap();
        assert_eq!(marked[0], private);
        assert_eq!(stamp(&marked).unwrap(), marked);
        assert!(!declared(&[]).unwrap());
        assert!(require_location(&marked, DIRECTORY, NodeKind::Directory).unwrap());
        for (path, kind) in [
            ("", NodeKind::Directory),
            ("other/.LTFSEE_DATA", NodeKind::Directory),
            ("//.LTFSEE_DATA", NodeKind::Directory),
            ("other", NodeKind::Directory),
            (DIRECTORY, NodeKind::File),
        ] {
            assert!(require_location(&marked, path, kind).is_err());
        }
        let marker = marked[1].clone();
        assert!(declared(&[marker.clone(), marker.clone()]).is_err());
        for bad in [
            Xattr {
                value: "2".into(),
                ..marker.clone()
            },
            Xattr {
                value: "MQ==".into(),
                base64: true,
                ..marker.clone()
            },
        ] {
            assert!(declared(&[bad]).is_err());
        }
        let meta = NodeMeta {
            file_uid: 2,
            ..Default::default()
        };
        let attributes = NodeAttributes::new(meta.clone(), marked.clone(), None).unwrap();
        assert_eq!(
            NodeAttributes::decode(attributes.bytes().unwrap()).unwrap(),
            attributes
        );
        assert!(NodeAttributes::new(meta, marked, Some("target".into())).is_err());
        let mut bytes = attributes.bytes().unwrap().to_vec();
        let at = bytes
            .windows(MARKER.len())
            .position(|w| w == MARKER.as_bytes())
            .unwrap()
            + MARKER.len()
            + 4;
        assert_eq!(bytes[at], b'1');
        bytes[at] = b'2';
        assert!(NodeAttributes::decode(&bytes).is_err());
        for mutation in [
            NodeMutation::SetXattr {
                attribute: marker,
                mode: XattrMode::Upsert,
            },
            NodeMutation::RemoveXattr { key: MARKER.into() },
        ] {
            assert!(mutation.validate_input().is_err());
        }
    }
}
