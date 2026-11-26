//! Pool administration under an explicit logical library scope.
use super::*;
use logical_libraries::{pool_library_at, pool_membership_prefix, require_library_at};

async fn read_pool(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    logical: Uuid,
    id: Uuid,
) -> Result<Option<PoolRecord>> {
    let Some(raw) = tr.get(&pool_key(ns, id), false).await? else {
        return Ok(None);
    };
    let pool = decode_pool(id, &raw)?;
    if pool.library_serial != serial || pool_library_at(tr, ns, serial, id).await? != Some(logical)
    {
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
        logical_libraries::require_business_route_at(&tr, serial).await?;
        let logical = logical.ok_or(CatalogError::Invalid("池查询须指定逻辑带库 UUID"))?;
        require_library_at(&tr, ns, serial, logical).await?;
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
        logical_libraries::require_business_route_at(&tr, serial).await?;
        let logical = logical.ok_or(CatalogError::Invalid("池列表须指定逻辑带库 UUID"))?;
        require_library_at(&tr, ns, serial, logical).await?;
        let prefix = pool_membership_prefix(ns, serial, logical);
        if !(1..=100).contains(&limit)
            || after.is_some_and(|key| !key.starts_with(&prefix) || key.len() != prefix.len() + 16)
        {
            return Err(CatalogError::Invalid("池分页范围或游标无效"));
        }
        let page = read_page(&tr, &prefix, after, limit, false).await?;
        let mut items = Vec::with_capacity(page.items.len());
        for (key, raw) in page.items {
            let id = uuid_from(&key[prefix.len()..])?;
            let pool = read_pool(&tr, ns, serial, logical, id).await?;
            if !raw.is_empty() || pool.is_none() {
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
