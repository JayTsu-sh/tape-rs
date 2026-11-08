//! Library and pool scoped operational inventory reads.
use super::*;

impl FdbClient {
    fn inventory_get(&self, library: &str, resource: &str) -> Result<Value> {
        Client::new(Vec::<String>::new()).with_library(library)?;
        let context = self.management_context(library)?;
        let namespace = context["namespace"]
            .as_str()
            .ok_or_else(|| invalid("库存上下文缺少命名空间"))?;
        let response = self.retry_request(
            "GET",
            &format!("{}{resource}", management::base(library)?),
            None,
            &[("x-tapers-catalog-namespace", namespace)],
        )?;
        if response.status != 200 {
            return Err(rejected(response));
        }
        let value = response.json();
        if value["library_serial"] != library || value["namespace"] != namespace {
            return Err(ClientError::Verification(
                "库存响应的带库或命名空间不符".into(),
            ));
        }
        Ok(value)
    }
    pub fn library_inventory(&self, library: &str) -> Result<Value> {
        self.inventory_get(library, "/library")
    }
    /// 当前库的驱动器或参与节点；与 HTTP 管理查询共用 namespace 核验。
    pub fn operational_inventory(
        &self,
        library: &str,
        resource: &str,
        target: Option<&str>,
    ) -> Result<Value> {
        if !matches!(resource, "drives" | "nodes")
            || target.is_some_and(|id| {
                id.is_empty()
                    || id.len() > 128
                    || id.contains('/')
                    || id.chars().any(char::is_control)
            })
        {
            return Err(invalid("驱动器或节点查询目标无效"));
        }
        let value = self.inventory_get(
            library,
            &format!(
                "/{resource}{}",
                target
                    .map(|id| format!("/{}", encode_query(id)))
                    .unwrap_or_default()
            ),
        )?;
        if target.is_some() && !value["item"].is_object()
            || target.is_none() && !value["items"].is_array()
        {
            return Err(ClientError::Verification("驱动器或节点响应缺少结果".into()));
        }
        Ok(value)
    }
    pub fn tape_inventory_page(
        &self,
        library: &str,
        pool: Option<Uuid>,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Value> {
        if !(1..=100).contains(&limit)
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
