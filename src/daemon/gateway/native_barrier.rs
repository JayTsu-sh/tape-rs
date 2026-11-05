//! 从明确选中的本节点采集原生初始化证据；不转发、不授予设备许可。
use super::*;
use crate::daemon::{directory::CutoverAudit, node::NodeStatus};
use serde_json::Value;
use uuid::Uuid;

fn build(
    serial: &str,
    challenge: Uuid,
    members: &HashMap<u64, String>,
    before: &NodeStatus,
    after: &NodeStatus,
    audit: CutoverAudit,
) -> Result<Value, &'static str> {
    let Some(frozen) = before.catalog_frozen_index else {
        return Err("目录尚未冻结");
    };
    let valid = |s: &NodeStatus| {
        s.id > 0
            && s.id <= 255
            && s.term > 0
            && members.contains_key(&s.id)
            && members.contains_key(&s.leader)
            && s.role
                == if s.id == s.leader {
                    "Leader"
                } else {
                    "Follower"
                }
            && s.executor.is_none()
            && !s.executor_ready
            && !s.catalog_pending
            && !s.shutting_down
            && s.pools.is_empty()
            && s.tapes.is_empty()
            && s.applied_index >= frozen
    };
    if challenge.is_nil()
        || members.len() != 3
        || members.keys().any(|id| !(1..=255).contains(id))
        || members
            .values()
            .any(|a| a.parse::<std::net::SocketAddr>().is_err())
        || members
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != 3
        || !valid(before)
        || !valid(after)
        || before.id != after.id
        || before.term != after.term
        || before.leader != after.leader
        || before.role != after.role
        || before.catalog_quiesce_index != after.catalog_quiesce_index
        || before.catalog_frozen_index != after.catalog_frozen_index
        || before
            .catalog_quiesce_index
            .is_none_or(|q| q == 0 || q > frozen)
        || frozen == 0
        || frozen >= 0xFFFF_FFFF_FFFF
        || audit.frozen_index != Some(frozen)
        || audit.applied_index < frozen
        || after.applied_index < before.applied_index.max(audit.applied_index)
        || audit.has_staging
        || audit.has_unresolved_file_tasks
        || audit.has_catalog_rows
    {
        return Err("原生初始化要求稳定的三节点配置、空目录及已排空的冻结水位");
    }
    Ok(json!({
        "version":1,"library_serial":serial,"challenge":challenge.to_string(),
        "node_id":after.id,"term":after.term,"leader":after.leader,"role":after.role,
        "members":members,"quiesce_index":after.catalog_quiesce_index,
        "frozen_index":frozen,"applied_index":after.applied_index,
        "executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true
    }))
}

pub(super) async fn respond(
    serial: &str,
    members: &HashMap<u64, String>,
    ctx: Arc<HttpContext>,
    requests: Arc<Semaphore>,
    req: Request,
) -> Response {
    if req.method() != "GET" {
        return response(405, json!({"error":"method_not_allowed"}));
    }
    let challenge = req
        .uri()
        .query()
        .and_then(|q| q.strip_prefix("challenge="))
        .and_then(|q| Uuid::parse_str(q).ok())
        .filter(|id| !id.is_nil());
    let Some(challenge) = challenge else {
        return response(
            400,
            json!({"error":"bad_request","detail":"须且仅须指定非nil challenge UUID"}),
        );
    };
    if req.headers().contains_key(header::TRANSFER_ENCODING)
        || req
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_some_and(|n| n != "0")
    {
        return response(
            400,
            json!({"error":"bad_request","detail":"屏障查询不接受请求体"}),
        );
    }
    let Ok(permit) = requests.try_acquire_owned() else {
        return response(503, json!({"error":"http_busy"}));
    };
    let serial = serial.to_owned();
    let members = members.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let before = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let audit = ctx
            .files
            .catalog_cutover_audit()
            .map_err(|e| e.to_string())?;
        let after = ctx.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok::<_, String>(build(&serial, challenge, &members, &before, &after, audit))
    })
    .await
    {
        Ok(Ok(Ok(value))) => response(200, value),
        Ok(Ok(Err(detail))) => response(
            409,
            json!({"error":"native_barrier_not_ready","detail":detail}),
        ),
        Ok(Err(detail)) => response(503, json!({"error":"catalog_unavailable","detail":detail})),
        Err(error) => response(
            503,
            json!({"error":"catalog_unavailable","detail":error.to_string()}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barrier_refuses_stale_nonempty_or_changing_facts() {
        let members = HashMap::from([
            (1, "127.0.0.1:8011".into()),
            (2, "127.0.0.1:8012".into()),
            (3, "127.0.0.1:8013".into()),
        ]);
        let status = NodeStatus {
            id: 2,
            leader: 1,
            term: 7,
            role: "Follower".into(),
            applied_index: 10,
            catalog_quiesce_index: Some(8),
            catalog_frozen_index: Some(9),
            ..Default::default()
        };
        let audit = CutoverAudit {
            applied_index: 10,
            frozen_index: Some(9),
            has_staging: false,
            has_unresolved_file_tasks: false,
            has_catalog_rows: false,
        };
        let challenge = Uuid::new_v4();
        let make = |after: &NodeStatus, a| build("LIB-B", challenge, &members, &status, after, a);
        let value = make(&status, audit).unwrap();
        assert_eq!(value["node_id"], 2);
        assert_eq!(value["library_serial"], "LIB-B");
        for change in 0..14 {
            let mut after = status.clone();
            match change {
                0 => after.term += 1,
                1 => after.leader = 3,
                2 => after.id = 3,
                3 => after.role = "Candidate".into(),
                4 => after.catalog_quiesce_index = None,
                5 => after.catalog_frozen_index = Some(10),
                6 => {
                    after.executor = Some(crate::daemon::state::Executor {
                        node: 1,
                        round: 1,
                        term: 1,
                        fenced: true,
                    })
                }
                7 => after.executor_ready = true,
                8 => after.catalog_pending = true,
                9 => after.shutting_down = true,
                10 => after.applied_index = 8,
                11 => after.term = 0,
                12 => {
                    after.tapes.insert("T1".into(), Default::default());
                }
                _ => after.pools.push(crate::daemon::directory::PoolRow {
                    uuid: "P".into(),
                    name: "p".into(),
                    file_limit: 0,
                    tapes: vec![],
                }),
            }
            assert!(make(&after, audit).is_err(), "{change}");
            assert!(
                build("LIB-B", challenge, &members, &after, &status, audit).is_err(),
                "before {change}"
            );
        }
        for change in 0..5 {
            let mut a = audit;
            match change {
                0 => a.has_staging = true,
                1 => a.has_unresolved_file_tasks = true,
                2 => a.has_catalog_rows = true,
                3 => a.applied_index = 8,
                _ => a.frozen_index = None,
            }
            assert!(make(&status, a).is_err());
        }
        for bad in [
            HashMap::new(),
            HashMap::from([
                (1, "127.0.0.1:8011".into()),
                (2, "127.0.0.1:8011".into()),
                (3, "127.0.0.1:8013".into()),
            ]),
        ] {
            assert!(build("LIB-B", challenge, &bad, &status, &status, audit).is_err());
        }
    }
}
