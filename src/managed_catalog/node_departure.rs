//! Permanent native-import barrier for a moved, still-live logical inode.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) struct NodeDeparture {
    pub namespace: Uuid,
    pub pool: Uuid,
    pub node: Uuid,
    pub path: String,
    pub previous_version: u64,
    pub management_epoch: u64,
    pub operation: Uuid,
    pub result_digest: [u8; 32],
}
impl NodeDeparture {
    fn encode(&self) -> Result<Vec<u8>> {
        if [self.namespace, self.pool, self.node, self.operation]
            .into_iter()
            .any(|id| id.is_nil())
            || self.management_epoch == 0
            || self.previous_version == 0
            || self.previous_version == u64::MAX
            || self.path == "/"
            || !crate::ltfs::xml_text_valid(&self.path)
        {
            return Err(CatalogError::Corrupt("rename 迁离屏障身份或原版本无效"));
        }
        pool_catalog_path(self.pool, self.path.as_bytes())?;
        let mut out = b"B2".to_vec();
        for id in [self.namespace, self.pool, self.node, self.operation] {
            out.extend_from_slice(id.as_bytes());
        }
        out.extend_from_slice(&self.management_epoch.to_be_bytes());
        out.extend_from_slice(&self.previous_version.to_be_bytes());
        out.extend_from_slice(&self.result_digest);
        let len = u16::try_from(self.path.len())
            .map_err(|_| CatalogError::Corrupt("rename 迁离路径过长"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(self.path.as_bytes());
        let checksum: [u8; 32] = Sha256::new()
            .chain_update(b"tape-rs/node-departure-barrier/1")
            .chain_update(&out)
            .finalize()
            .into();
        out.extend_from_slice(&checksum);
        Ok(out)
    }
    pub(super) fn decode(raw: &[u8]) -> Result<Self> {
        let mut at = 0;
        if take(raw, &mut at, 2)? != b"B2" {
            return Err(CatalogError::Corrupt("rename 迁离屏障版本"));
        }
        let namespace = uuid_from(take(raw, &mut at, 16)?)?;
        let pool = uuid_from(take(raw, &mut at, 16)?)?;
        let node = uuid_from(take(raw, &mut at, 16)?)?;
        let operation = uuid_from(take(raw, &mut at, 16)?)?;
        let management_epoch = read_u64(take(raw, &mut at, 8)?);
        let previous_version = read_u64(take(raw, &mut at, 8)?);
        let result_digest = take(raw, &mut at, 32)?.try_into().expect("fixed digest");
        let len =
            u16::from_be_bytes(take(raw, &mut at, 2)?.try_into().expect("fixed length")) as usize;
        if len > 2048 {
            return Err(CatalogError::Corrupt("rename 迁离路径过长"));
        }
        let path = std::str::from_utf8(take(raw, &mut at, len)?)
            .map_err(|_| CatalogError::Corrupt("rename 迁离路径编码"))?
            .to_owned();
        take(raw, &mut at, 32)?;
        let value = Self {
            namespace,
            pool,
            node,
            operation,
            management_epoch,
            previous_version,
            result_digest,
            path,
        };
        if at != raw.len() || value.encode()? != raw {
            return Err(CatalogError::Corrupt("rename 迁离屏障摘要或长度不符"));
        }
        Ok(value)
    }
}

pub(in crate::managed_catalog) async fn prepare_departure(
    tr: &Transaction,
    value: &NodeDeparture,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let encoded = value.encode()?;
    barrier_at(tr, value.namespace, value.pool, &value.path).await?;
    Ok((
        barrier_key(value.namespace, value.pool, &value.path),
        encoded,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> NodeDeparture {
        NodeDeparture {
            namespace: Uuid::new_v4(),
            pool: Uuid::new_v4(),
            node: Uuid::new_v4(),
            path: "/旧目录".into(),
            previous_version: 6,
            management_epoch: 1,
            operation: Uuid::new_v4(),
            result_digest: [5; 32],
        }
    }
    #[test]
    fn departure_is_a_self_contained_scoped_record_without_a_retired_inode() {
        let value = fixture();
        let raw = value.encode().unwrap();
        assert_eq!(NodeDeparture::decode(&raw).unwrap(), value);
        for len in 0..raw.len() {
            assert!(NodeDeparture::decode(&raw[..len]).is_err());
        }
        for at in [2, 18, 34, 50, 66, 74, 82, 114, raw.len() - 1] {
            let mut bad = raw.clone();
            bad[at] ^= 1;
            assert!(NodeDeparture::decode(&bad).is_err());
        }
        let mut bad = raw;
        bad.push(0);
        assert!(NodeDeparture::decode(&bad).is_err());
        let mut bad = value.clone();
        bad.path = "/".into();
        assert!(bad.encode().is_err());
        let mut bad = value;
        bad.previous_version = u64::MAX;
        assert!(bad.encode().is_err());
    }
    #[test]
    #[ignore = "需要隔离FDB；仅元数据验证活源迁离屏障/子路径/独立历史及损坏不修复"]
    fn live_source_departure_blocks_native_old_prefix_without_retiring_or_requiring_source_inode() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = foundationdb::Database::new(Some(&cluster)).unwrap();
            let value = fixture();
            let tr = db.create_trx().unwrap();
            let node_key = node_key(value.namespace, value.node);
            let live = encode_node(&Node {
                kind: NodeKind::Directory,
                directory_version: Some(7),
                parent: Uuid::new_v4(),
                name: b"new".to_vec(),
                attributes: None,
            });
            let (key, raw) = prepare_departure(&tr, &value).await.unwrap();
            tr.set(&node_key, &live);
            tr.set(&key, &raw);
            tr.commit().await.unwrap();
            let tr = db.create_trx().unwrap();
            assert!(
                require_native_path_allowed(&tr, value.namespace, value.pool, &value.path)
                    .await
                    .is_err()
            );
            assert!(
                require_native_path_allowed(
                    &tr,
                    value.namespace,
                    value.pool,
                    &(value.path.clone() + "/child")
                )
                .await
                .is_err()
            );
            require_native_path_allowed(&tr, value.namespace, value.pool, "/new")
                .await
                .unwrap();
            assert_eq!(
                tr.get(&node_key, false).await.unwrap().unwrap().as_ref(),
                live
            );
            tr.clear(&node_key);
            tr.commit().await.unwrap();
            let tr = db.create_trx().unwrap();
            assert!(
                require_native_path_allowed(&tr, value.namespace, value.pool, &value.path)
                    .await
                    .is_err(),
                "permanent departure cannot depend on later live or retired N"
            );
            let mut bad = raw.clone();
            let last = bad.len() - 1;
            bad[last] ^= 1;
            tr.set(&key, &bad);
            tr.commit().await.unwrap();
            let tr = db.create_trx().unwrap();
            assert!(
                require_native_path_allowed(&tr, value.namespace, value.pool, &value.path)
                    .await
                    .is_err()
            );
            assert_eq!(tr.get(&key, false).await.unwrap().unwrap().as_ref(), bad);
            tr.clear(&key);
            tr.commit().await.unwrap();
        });
    }
}
