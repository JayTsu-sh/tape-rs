//! Library and pool scoped operational inventory reads.
use super::*;

impl FdbClient {
    fn inventory_get(&self, library: &str, resource: &str) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        let response = self.retry_request(
            "GET",
            &format!("/admin/libraries/{library}{resource}"),
            None,
            &[],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        Ok(response.json())
    }
    pub fn library_inventory(&self, library: &str) -> Result<Value> {
        self.inventory_get(library, "")
    }
    pub fn tape_inventory_page(
        &self,
        library: &str,
        pool: Option<Uuid>,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Value> {
        if !(1..=1000).contains(&limit)
            || pool.is_some_and(|id| id.is_nil())
            || after.is_some_and(|s| {
                s.len() > 16384
                    || !s
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            })
        {
            return Err(invalid("磁带库存分页或池身份无效"));
        }
        let base = pool.map_or_else(|| "/tapes".into(), |id| format!("/pools/{id}/tapes"));
        self.inventory_get(
            library,
            &format!(
                "{base}?limit={limit}{}",
                after.map(|s| format!("&after={s}")).unwrap_or_default()
            ),
        )
    }
    pub fn tape_inventory_entry(
        &self,
        library: &str,
        pool: Option<Uuid>,
        barcode: &str,
    ) -> Result<Value> {
        if !valid_management_barcode(barcode) || pool.is_some_and(|id| id.is_nil()) {
            return Err(invalid("条码或池身份无效"));
        }
        let base = pool.map_or_else(|| "/tapes".into(), |id| format!("/pools/{id}/tapes"));
        self.inventory_get(library, &format!("{base}/{}", encode_query(barcode)))
    }
}
