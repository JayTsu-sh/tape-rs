//! Replicated pool management, addressed by the original creation UUID.
use super::*;
use crate::replicated_pool::{CreateReplicatedPool, ResolveReplicatedPoolConflict};

const BASE: &str = "/admin/replicated-pools";

impl FdbClient {
    /// Submit a complete request whose UUIDs and fields were saved before submission.
    pub fn create_replicated_pool(&self, request: &CreateReplicatedPool) -> Result<Value> {
        let body = serde_json::to_vec(request).map_err(|_| invalid("多副本池请求无法编码"))?;
        let value =
            self.replicated_pool_request(request.id, "POST", BASE, Some(&body), &[200, 202])?;
        let request_matches = value["name"] == request.name
            && value["created_at_millis"] == request.created_at_millis
            && value["members"].as_array().is_some_and(|rows| {
                rows.len() == request.members.len()
                    && rows.iter().zip(&request.members).all(|(row, member)| {
                        row["library_serial"] == member.library_serial
                            && row["namespace"] == member.namespace.to_string()
                            && row["pool_id"] == member.pool_id.to_string()
                            && row["name"] == member.name
                            && row["original_logical_library"]
                                == serde_json::json!(member.logical_library)
                            && row["confirm_format"] == member.confirm_format
                            && row["tapes"].as_array().is_some_and(|tapes| {
                                tapes
                                    .iter()
                                    .map(|tape| tape["barcode"].as_str())
                                    .eq(member.tapes.iter().map(|barcode| Some(barcode.as_str())))
                            })
                    })
            });
        if !request_matches {
            return Err(ClientError::Verification(
                "多副本池回执与原创建参数不符".into(),
            ));
        }
        Ok(value)
    }

    pub fn replicated_pool_status(&self, id: Uuid) -> Result<Value> {
        self.replicated_pool_request(id, "GET", &format!("{BASE}/{id}"), None, &[200])
    }

    /// Pass the previous response's `next` path as `after`.
    pub fn replicated_pool_files(
        &self,
        id: Uuid,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        if !(1..=100).contains(&limit) || after.is_some_and(|path| path.len() > 2048) {
            return Err(invalid("多副本池文件分页参数无效"));
        }
        let mut route = format!("{BASE}/{id}/files?limit={limit}");
        if let Some(path) = after {
            route.push_str("&after=");
            route.push_str(&URL_SAFE_NO_PAD.encode(path));
        }
        self.replicated_pool_request(id, "GET", &route, None, &[200])
    }

    pub fn resolve_replicated_pool_conflict(
        &self,
        id: Uuid,
        choice: &ResolveReplicatedPoolConflict,
    ) -> Result<Value> {
        let body = serde_json::to_vec(choice).map_err(|_| invalid("多副本池冲突处理无法编码"))?;
        self.replicated_pool_request(
            id,
            "POST",
            &format!("{BASE}/{id}/resolve"),
            Some(&body),
            &[200],
        )
    }

    fn replicated_pool_request(
        &self,
        id: Uuid,
        method: &str,
        route: &str,
        body: Option<&[u8]>,
        statuses: &[u16],
    ) -> Result<Value> {
        if id.is_nil() {
            return Err(invalid("多副本池 UUID 不能为零"));
        }
        let response =
            self.retry_request(method, route, body, &[("Content-Type", "application/json")])?;
        let value = management::parsed(response, statuses)?;
        if value["version"] != 1 || value["id"] != id.to_string() {
            return Err(ClientError::Verification(
                "多副本池回执与原 UUID 不符".into(),
            ));
        }
        Ok(value)
    }
}
