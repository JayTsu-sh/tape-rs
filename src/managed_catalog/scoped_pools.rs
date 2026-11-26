//! Pool administration under the active physical or logical library scope.
use super::*;
use logical_libraries::{pool_library_at, pool_membership_prefix, require_business_scope_at};

async fn read_pool(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    logical: Option<Uuid>,
    id: Uuid,
) -> Result<Option<PoolRecord>> {
    if selected_pool::is_pending_at(tr, ns, serial, id).await? {
        return Ok(None);
    }
    let Some(raw) = tr.get(&pool_key(ns, id), false).await? else {
        return Ok(None);
    };
    let pool = decode_pool(id, &raw)?;
    if pool.library_serial != serial || pool_library_at(tr, ns, serial, id).await? != logical {
        return Ok(None);
    }
    require_pool_library(tr, ns, id, serial).await?;
    active_pools::require_indexes(tr, ns, &pool).await?;
    Ok(Some(pool))
}

impl ManagedCatalog {
    pub(crate) async fn admin_pool(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        logical: Option<Uuid>,
        selector: &str,
    ) -> Result<Option<PoolRecord>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        require_business_scope_at(&tr, ns, serial, logical).await?;
        if let Ok(id) = Uuid::parse_str(selector) {
            if id.is_nil() || id.to_string() != selector {
                return Err(CatalogError::Invalid("池 UUID 须规范且非零"));
            }
            return read_pool(&tr, ns, serial, logical, id).await;
        }
        validate_pool_name(selector)?;
        let Some(raw) = tr.get(&pool_name_key(ns, selector), false).await? else {
            return Ok(None);
        };
        let pool = read_pool(&tr, ns, serial, logical, uuid_from(&raw)?).await?;
        if pool.as_ref().is_some_and(|pool| pool.name != selector) {
            return Err(CatalogError::Corrupt("池名称索引与主记录不符"));
        }
        Ok(pool)
    }

    pub(crate) async fn admin_pool_page(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        logical: Option<Uuid>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<PoolRecord>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        require_business_scope_at(&tr, ns, serial, logical).await?;
        let prefix = match logical {
            Some(id) => pool_membership_prefix(ns, serial, id),
            None => pool_prefix(ns),
        };
        if !(1..=100).contains(&limit)
            || after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 16)
        {
            return Err(CatalogError::Invalid("池分页范围或游标无效"));
        }
        let page = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, raw) in page.items {
            let id = uuid_from(&key[prefix.len()..])?;
            if selected_pool::is_pending_at(&tr, ns, serial, id).await? {
                continue;
            }
            let pool = read_pool(&tr, ns, serial, logical, id).await?;
            if logical.is_some() && (!raw.is_empty() || pool.is_none()) {
                return Err(CatalogError::Corrupt("逻辑带库的池索引与主记录不符"));
            }
            items.extend(pool);
        }
        Ok(Page {
            items,
            next: page.next,
        })
    }
}
