//! Permanent inode retirement and native-import authority after a logical
//! deletion. These are neither file receipts nor imported tape tombstones.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NodeRetirement {
    pub namespace: Uuid,
    pub pool: Uuid,
    pub node: Uuid,
    pub path: String,
    pub kind: NodeKind,
    pub deleted_version: u64,
    pub previous_attributes: [u8; 32],
    pub previous_head: Option<FileHead>,
    pub management_epoch: u64,
    pub operation: Uuid,
    pub result_digest: [u8; 32],
}

impl NodeRetirement {
    fn validate(&self) -> Result<()> {
        if self.namespace.is_nil()
            || self.pool.is_nil()
            || self.node.is_nil()
            || self.operation.is_nil()
            || self.management_epoch == 0
            || self.deleted_version < 2
            || self.path == "/"
            || !crate::ltfs::xml_text_valid(&self.path)
            || pool_catalog_path(self.pool, self.path.as_bytes()).is_err()
        {
            return Err(CatalogError::Corrupt("节点删除记录身份、路径或版本无效"));
        }
        match (self.kind, &self.previous_head) {
            (NodeKind::Directory, None) => {}
            (NodeKind::File, Some(head))
                if head.version.checked_add(1) == Some(self.deleted_version) =>
            {
                decode_head(&encode_head(head))?;
            }
            _ => return Err(CatalogError::Corrupt("节点删除记录类型与历史 head 不符")),
        }
        Ok(())
    }
}

pub(super) fn encode(value: &NodeRetirement) -> Result<Vec<u8>> {
    value.validate()?;
    let mut out = b"NR1".to_vec();
    for id in [value.namespace, value.pool, value.node] {
        out.extend_from_slice(id.as_bytes());
    }
    let path_len = u16::try_from(value.path.len())
        .map_err(|_| CatalogError::Corrupt("节点删除记录路径过长"))?;
    out.extend_from_slice(&path_len.to_be_bytes());
    out.extend_from_slice(value.path.as_bytes());
    out.push(match value.kind {
        NodeKind::Directory => 0,
        NodeKind::File => 1,
    });
    out.extend_from_slice(&value.deleted_version.to_be_bytes());
    out.extend_from_slice(&value.previous_attributes);
    out.extend_from_slice(&value.management_epoch.to_be_bytes());
    out.extend_from_slice(value.operation.as_bytes());
    out.extend_from_slice(&value.result_digest);
    match &value.previous_head {
        None => out.push(0),
        Some(head) => {
            out.push(1);
            out.extend_from_slice(&encode_head(head));
        }
    }
    Ok(out)
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(len)
        .ok_or(CatalogError::Corrupt("节点删除记录长度溢出"))?;
    let value = bytes
        .get(*at..end)
        .ok_or(CatalogError::Corrupt("节点删除记录截断"))?;
    *at = end;
    Ok(value)
}

pub(super) fn decode(bytes: &[u8]) -> Result<NodeRetirement> {
    let mut at = 0;
    if take(bytes, &mut at, 3)? != b"NR1" {
        return Err(CatalogError::Corrupt("节点删除记录版本"));
    }
    let namespace = uuid_from(take(bytes, &mut at, 16)?)?;
    let pool = uuid_from(take(bytes, &mut at, 16)?)?;
    let node = uuid_from(take(bytes, &mut at, 16)?)?;
    let len =
        u16::from_be_bytes(take(bytes, &mut at, 2)?.try_into().expect("fixed length")) as usize;
    if len > 2048 {
        return Err(CatalogError::Corrupt("节点删除记录路径过长"));
    }
    let path = std::str::from_utf8(take(bytes, &mut at, len)?)
        .map_err(|_| CatalogError::Corrupt("节点删除记录路径编码"))?
        .to_owned();
    let kind = match take(bytes, &mut at, 1)?[0] {
        0 => NodeKind::Directory,
        1 => NodeKind::File,
        _ => return Err(CatalogError::Corrupt("节点删除记录类型")),
    };
    let deleted_version = read_u64(take(bytes, &mut at, 8)?);
    let previous_attributes = take(bytes, &mut at, 32)?.try_into().expect("fixed length");
    let management_epoch = read_u64(take(bytes, &mut at, 8)?);
    let operation = uuid_from(take(bytes, &mut at, 16)?)?;
    let result_digest = take(bytes, &mut at, 32)?.try_into().expect("fixed length");
    let previous_head = match take(bytes, &mut at, 1)?[0] {
        0 => None,
        1 => Some(decode_head(take(bytes, &mut at, 32)?)?),
        _ => return Err(CatalogError::Corrupt("节点删除记录历史 head 标记")),
    };
    if at != bytes.len() {
        return Err(CatalogError::Corrupt("节点删除记录尾部数据"));
    }
    let value = NodeRetirement {
        namespace,
        pool,
        node,
        path,
        kind,
        deleted_version,
        previous_attributes,
        previous_head,
        management_epoch,
        operation,
        result_digest,
    };
    value.validate()?;
    Ok(value)
}

/// Classify one N record without accepting an unknown record type as live.
/// Ordinary path readers still use decode_node and reject retired references.
pub(super) fn from_node_record(
    namespace: Uuid,
    node: Uuid,
    bytes: &[u8],
) -> Result<Option<NodeRetirement>> {
    if bytes.starts_with(b"N\x01") {
        return Ok(None);
    }
    let value = decode(bytes)?;
    if value.namespace != namespace || value.node != node {
        return Err(CatalogError::Corrupt("节点删除记录与 inode 键不符"));
    }
    Ok(Some(value))
}

fn barrier_key(namespace: Uuid, pool: Uuid, path: &str) -> Vec<u8> {
    let mut out = control::management::prefix(namespace, b'Q');
    out.push(b'b');
    out.extend_from_slice(pool.as_bytes());
    out.extend_from_slice(
        &Sha256::new()
            .chain_update(b"tape-rs/node-retirement-path/1")
            .chain_update(path.as_bytes())
            .finalize(),
    );
    out
}

async fn barrier_at(tr: &Transaction, namespace: Uuid, pool: Uuid, path: &str) -> Result<bool> {
    let Some(raw) = tr.get(&barrier_key(namespace, pool, path), false).await? else {
        return Ok(false);
    };
    if raw.starts_with(b"B2") {
        let value = NodeDeparture::decode(&raw)?;
        if value.namespace != namespace || value.pool != pool || value.path != path {
            return Err(CatalogError::Corrupt("rename 迁离屏障与路径键不符"));
        }
        return Ok(true);
    }
    if raw.len() != 50 || &raw[..2] != b"B1" {
        return Err(CatalogError::Corrupt("节点删除导入屏障版本或长度"));
    }
    let node = uuid_from(&raw[2..18])?;
    let record = tr
        .get(&node_key(namespace, node), false)
        .await?
        .ok_or(CatalogError::Corrupt("节点删除导入屏障缺少 inode 记录"))?;
    let value = from_node_record(namespace, node, &record)?
        .ok_or(CatalogError::Corrupt("节点删除导入屏障指向活跃 inode"))?;
    if value.pool != pool || value.path != path || value.result_digest != raw[18..] {
        return Err(CatalogError::Corrupt("节点删除导入屏障与历史身份不符"));
    }
    Ok(true)
}

/// Prepare all fallible checks before the publication's first catalog write.
/// Explicit recreation preserves the older path's native-import barrier.
pub(super) async fn prepare_barrier(
    tr: &Transaction,
    value: &NodeRetirement,
) -> Result<(Vec<u8>, Vec<u8>)> {
    value.validate()?;
    barrier_at(tr, value.namespace, value.pool, &value.path).await?;
    let mut raw = b"B1".to_vec();
    raw.extend_from_slice(value.node.as_bytes());
    raw.extend_from_slice(&value.result_digest);
    Ok((barrier_key(value.namespace, value.pool, &value.path), raw))
}

pub(super) async fn require_native_path_allowed(
    tr: &Transaction,
    namespace: Uuid,
    pool: Uuid,
    path: &str,
) -> Result<()> {
    pool_catalog_path(pool, path.as_bytes())?;
    for end in path
        .match_indices('/')
        .skip(1)
        .map(|(at, _)| at)
        .chain(std::iter::once(path.len()))
    {
        if barrier_at(tr, namespace, pool, &path[..end]).await? {
            return Err(CatalogError::Invalid(
                "介质包含已删除或迁离的历史路径，不能导入当前池",
            ));
        }
    }
    Ok(())
}

#[path = "node_departure.rs"]
mod departure;
pub(super) use departure::{NodeDeparture, prepare_departure};

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> NodeRetirement {
        NodeRetirement {
            namespace: Uuid::new_v4(),
            pool: Uuid::new_v4(),
            node: Uuid::new_v4(),
            path: "/目录/file".into(),
            kind: NodeKind::File,
            deleted_version: 4,
            previous_attributes: [3; 32],
            previous_head: Some(FileHead {
                version: 3,
                size: 123,
                copy_id: Uuid::new_v4(),
            }),
            management_epoch: 9,
            operation: Uuid::new_v4(),
            result_digest: [7; 32],
        }
    }

    #[test]
    fn retirement_codec_is_strict_typed_and_never_a_live_inode() {
        let file = fixture();
        let barrier = barrier_key(file.namespace, file.pool, &file.path);
        assert!(
            cleanup::NAMESPACE_ROWS.iter().any(|kind| {
                let mut prefix = key(*kind);
                prefix.extend_from_slice(file.namespace.as_bytes());
                barrier.starts_with(&prefix)
            }),
            "retirement barrier must belong to namespace-owned cleanup"
        );
        for value in [
            file.clone(),
            NodeRetirement {
                kind: NodeKind::Directory,
                previous_head: None,
                ..file.clone()
            },
        ] {
            let bytes = encode(&value).unwrap();
            assert_eq!(decode(&bytes).unwrap(), value);
            assert_eq!(
                from_node_record(value.namespace, value.node, &bytes).unwrap(),
                Some(value.clone())
            );
            assert!(decode_node(&bytes).is_err());
            for end in 0..bytes.len() {
                assert!(decode(&bytes[..end]).is_err());
            }
            let mut bad = bytes.clone();
            bad.push(0);
            assert!(decode(&bad).is_err());
            let mut bad = bytes.clone();
            bad[2] = b'2';
            assert!(decode(&bad).is_err());
            assert!(from_node_record(Uuid::new_v4(), value.node, &bytes).is_err());
            assert!(from_node_record(value.namespace, Uuid::new_v4(), &bytes).is_err());
        }
        for value in [
            NodeRetirement {
                deleted_version: 1,
                ..file.clone()
            },
            NodeRetirement {
                deleted_version: 5,
                ..file.clone()
            },
            NodeRetirement {
                kind: NodeKind::Directory,
                ..file.clone()
            },
            NodeRetirement {
                previous_head: None,
                ..file.clone()
            },
            NodeRetirement {
                namespace: Uuid::nil(),
                ..file.clone()
            },
            NodeRetirement {
                pool: Uuid::nil(),
                ..file.clone()
            },
            NodeRetirement {
                node: Uuid::nil(),
                ..file.clone()
            },
            NodeRetirement {
                operation: Uuid::nil(),
                ..file.clone()
            },
            NodeRetirement {
                management_epoch: 0,
                ..file.clone()
            },
            NodeRetirement {
                path: "/".into(),
                ..file.clone()
            },
            NodeRetirement {
                path: "/a/../b".into(),
                ..file.clone()
            },
        ] {
            assert!(encode(&value).is_err());
        }
    }
}
