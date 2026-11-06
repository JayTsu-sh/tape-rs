//! A commit-version token for the visible namespace. Materialization and
//! idempotent publication retries do not advance this token.
use super::*;

fn revision_key(namespace: Uuid) -> Vec<u8> {
    let mut key = control::management::prefix(namespace, b'g');
    key.push(b'v');
    key
}

pub(super) async fn at(tr: &Transaction, namespace: Uuid) -> Result<[u8; 10]> {
    let Some(raw) = tr.get(&revision_key(namespace), false).await? else {
        return Ok([0; 10]);
    };
    if raw.len() != 12 || &raw[..2] != b"V1" || raw[2..] == [0; 10] || raw[2..] == [255; 10] {
        return Err(CatalogError::Corrupt("命名空间可见修订标记"));
    }
    Ok(raw[2..].try_into().expect("validated revision length"))
}

/// Call after the transaction's last catalog/notification read. A
/// versionstamped value is unreadable until its transaction has committed.
/// This adds no fallible work after the publication's first mutation.
pub(super) fn publish(tr: &Transaction, namespace: Uuid) {
    // FDB replaces ten bytes at offset 2, then strips the trailing LE offset.
    // The installed binding uses this same encoding for metadata versions.
    let mut value = b"V1".to_vec();
    value.extend_from_slice(&[255; 10]);
    value.extend_from_slice(&2u32.to_le_bytes());
    tr.atomic_op(
        &revision_key(namespace),
        &value,
        foundationdb::options::MutationType::SetVersionstampedValue,
    );
}

impl ManagedCatalog {
    pub async fn active_namespace_visibility(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
    ) -> Result<[u8; 10]> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        at(&tr, ns).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；只写随机测试域的可见修订标记"]
    async fn namespace_visibility_is_a_committed_version_and_does_not_escape_aborted_transactions()
    {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        assert_eq!(
            at(&catalog.db.create_trx().unwrap(), namespace)
                .await
                .unwrap(),
            [0; 10]
        );
        let tr = catalog.db.create_trx().unwrap();
        publish(&tr, namespace);
        tr.commit().await.unwrap();
        let first = at(&catalog.db.create_trx().unwrap(), namespace)
            .await
            .unwrap();
        assert_ne!(first, [0; 10]);
        assert_ne!(first, [255; 10]);
        let uncommitted = catalog.db.create_trx().unwrap();
        publish(&uncommitted, namespace);
        drop(uncommitted);
        assert_eq!(
            at(&catalog.db.create_trx().unwrap(), namespace)
                .await
                .unwrap(),
            first
        );
        let tr = catalog.db.create_trx().unwrap();
        publish(&tr, namespace);
        publish(&tr, namespace);
        tr.commit().await.unwrap();
        let second = at(&catalog.db.create_trx().unwrap(), namespace)
            .await
            .unwrap();
        assert!(
            second > first,
            "并发事务的版本号不能用同一个读版本冒充发布修订"
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&revision_key(namespace), b"V1bad");
        tr.commit().await.unwrap();
        assert!(matches!(
            at(&catalog.db.create_trx().unwrap(), namespace).await,
            Err(CatalogError::Corrupt(_))
        ));
    }
}
