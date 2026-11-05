//! Native operational metadata, bound to one library and catalog namespace.
//! No SQLite projection or SCSI access belongs in this module.
use super::*;
use axum::http::HeaderMap;
use uuid::Uuid;

const NAMESPACE: &str = "x-tapers-catalog-namespace";

fn boxed_reply(status: StatusCode, body: Value) -> Box<Response> {
    Box::new(reply(status, body))
}
fn bad(reason: &str) -> Box<Response> {
    Box::new(invalid(reason))
}
fn busy(reason: &str) -> Box<Response> {
    Box::new(unavailable(reason))
}
fn route(ctx: &Context, serial: &str) -> Result<ActiveLibraryRoute, Box<Response>> {
    ctx.routes.get(serial).copied().ok_or_else(|| {
        boxed_reply(
            StatusCode::NOT_FOUND,
            json!({"error":"library_not_found","detail":"带库未配置"}),
        )
    })
}
fn namespace(headers: &HeaderMap, route: ActiveLibraryRoute) -> Result<(), Box<Response>> {
    if headers.get_all(NAMESPACE).iter().count() != 1
        || headers.get(NAMESPACE).and_then(|v| v.to_str().ok())
            != Some(route.route.namespace.to_string().as_str())
    {
        return Err(boxed_reply(
            StatusCode::CONFLICT,
            json!({"error":"namespace_changed","detail":"目录命名空间缺失、重复或已改变；请核对原请求"}),
        ));
    }
    Ok(())
}
fn cursor_prefix(serial: &str, expected: ActiveLibraryRoute) -> Vec<u8> {
    let mut prefix = b"P1/".to_vec();
    prefix.extend_from_slice(&(serial.len() as u64).to_be_bytes());
    prefix.extend_from_slice(serial.as_bytes());
    prefix.extend_from_slice(expected.route.namespace.as_bytes());
    prefix
}
pub(super) fn pool_cursor(serial: &str, expected: ActiveLibraryRoute, raw: &[u8]) -> String {
    let mut bytes = cursor_prefix(serial, expected);
    bytes.extend_from_slice(raw);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub(super) fn decode_pool_cursor(
    serial: &str,
    expected: ActiveLibraryRoute,
    encoded: &str,
) -> Result<Vec<u8>, Box<Response>> {
    if encoded.len() > 4096 {
        return Err(bad("池分页游标过长"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| bad("池分页游标无效"))?;
    let prefix = cursor_prefix(serial, expected);
    bytes
        .strip_prefix(prefix.as_slice())
        .filter(|v| !v.is_empty())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| bad("池分页游标的带库或命名空间不符"))
}
pub(super) fn failure(error: CatalogError) -> Response {
    let mut domain = &error;
    while let CatalogError::Fdb(foundationdb::FdbBindingError::CustomError(inner)) = domain {
        let Some(next) = inner.downcast_ref::<CatalogError>() else {
            break;
        };
        domain = next;
    }
    let (status, kind) = match domain {
        CatalogError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
        CatalogError::ManagementQueueFull => (StatusCode::TOO_MANY_REQUESTS, "queue_full"),
        CatalogError::Corrupt(_) | CatalogError::Cache(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "catalog_corrupt")
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, "catalog_unavailable"),
    };
    reply(
        status,
        json!({"error":kind,"detail":domain.to_string(),"retryable":matches!(status,StatusCode::SERVICE_UNAVAILABLE|StatusCode::TOO_MANY_REQUESTS)}),
    )
}
fn budget(ctx: &Context) -> Result<OwnedSemaphorePermit, Box<Response>> {
    ctx.admin_requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy("管理并发已达上限，请稍后重试"))
}
pub(super) async fn context(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match ctx
        .catalog
        .active_management_window(&serial, expected)
        .await
    {
        Ok(w) => reply(
            StatusCode::OK,
            json!({"version":1,"library_serial":serial,"namespace":expected.route.namespace.to_string(),"window":{"first_retained":w.first_retained,"accepting":w.accepting}}),
        ),
        Err(e) => failure(e),
    }
}
pub(super) async fn list(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(text)) => match text.parse::<usize>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return invalid("池分页limit须在1..=100"),
        },
        Err(e) => return invalid(e),
    };
    let after = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(text)) => match decode_pool_cursor(&serial, expected, text) {
            Ok(raw) => Some(raw),
            Err(response) => return *response,
        },
        Err(e) => return invalid(e),
    };
    match ctx
        .catalog
        .list_active_pools_for_route(&serial, expected, after.as_deref(), limit)
        .await
    {
        Ok(page) => {
            let items: std::result::Result<Vec<_>, _> =
                page.items.iter().map(|p| p.to_json()).collect();
            match items {
                Ok(items) => reply(
                    StatusCode::OK,
                    json!({"library_serial":serial,"namespace":expected.route.namespace.to_string(),"items":items,"next":page.next.map(|v|pool_cursor(&serial,expected,&v))}),
                ),
                Err(e) => failure(e),
            }
        }
        Err(e) => failure(e),
    }
}
pub(super) async fn show(
    State(ctx): State<Arc<Context>>,
    Path((serial, selector)): Path<(String, String)>,
    request: Request,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    // UUID selectors never fall through to a different pool's name.
    let result = match Uuid::parse_str(&selector) {
        Ok(id) => {
            if id.is_nil() || id.to_string() != selector {
                return invalid("池UUID须规范且非零");
            }
            ctx.catalog
                .active_pool_for_route(&serial, expected, id)
                .await
        }
        Err(_) => {
            ctx.catalog
                .active_pool_named_for_route(&serial, expected, &selector)
                .await
        }
    };
    match result {
        Ok(Some(pool)) => match pool.to_json() {
            Ok(p) => reply(
                StatusCode::OK,
                json!({"library_serial":serial,"namespace":expected.route.namespace.to_string(),"pool":p}),
            ),
            Err(e) => failure(e),
        },
        Ok(None) => reply(
            StatusCode::NOT_FOUND,
            json!({"error":"pool_not_found","detail":"目标带库中没有该池"}),
        ),
        Err(e) => failure(e),
    }
}

struct Serving {
    library: Arc<HttpContext>,
    node: u64,
    round: u64,
    local: bool,
}
fn serving(
    ctx: &Context,
    serial: &str,
    expected: ActiveLibraryRoute,
) -> Result<Serving, Box<Response>> {
    let owner = ctx.owner.as_ref().ok_or_else(|| {
        boxed_reply(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"metadata_readonly","detail":"修改池需要受管owner运行模式"}),
        )
    })?;
    let library = owner
        .libraries
        .get(serial)
        .ok_or_else(|| busy("带库执行上下文不存在"))?
        .clone();
    let status = library
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let executor = status
        .executor
        .as_ref()
        .filter(|e| {
            status.executor_ready
                && !status.catalog_pending
                && e.fenced
                && e.node != 0
                && !status.shutting_down
                && status.catalog_frozen_index == Some(expected.frozen_index)
        })
        .ok_or_else(|| busy("带库owner尚未就绪"))?;
    let result = Serving {
        library: library.clone(),
        node: executor.node,
        round: executor.round,
        local: executor.node == status.id,
    };
    if result.local && !live(&result, expected) {
        return Err(busy("带库Serving多数派许可已失效"));
    }
    Ok(result)
}
fn live(owner: &Serving, expected: ActiveLibraryRoute) -> bool {
    let status = owner
        .library
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    owner.local
        && !status.shutting_down
        && status.id == owner.node
        && status.executor_ready
        && !status.catalog_pending
        && status.catalog_frozen_index == Some(expected.frozen_index)
        && status
            .executor
            .as_ref()
            .is_some_and(|e| e.node == owner.node && e.round == owner.round && e.fenced)
        && owner.library.files.serving_deadline(owner.round).is_some()
}
// Dropping an in-flight FDB transaction is not evidence that it did not commit.
// Stop retries on permission loss; the original UUID/ledger remains the recovery key.
async fn permitted<T>(
    owner: &Serving,
    expected: ActiveLibraryRoute,
    future: impl std::future::Future<Output = Result<T, CatalogError>>,
) -> Result<T, Box<Response>> {
    let mut future = std::pin::pin!(future);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if !live(owner, expected) || std::time::Instant::now() >= deadline {
            return Err(boxed_reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"outcome_unknown","detail":"owner许可失效或FDB等待超时；保留原请求身份并查询"}),
            ));
        }
        match tokio::time::timeout(std::time::Duration::from_millis(25), &mut future).await {
            Ok(result) => {
                return if live(owner, expected) {
                    result.map_err(|e| Box::new(failure(e)))
                } else {
                    Err(boxed_reply(
                        StatusCode::SERVICE_UNAVAILABLE,
                        json!({"error":"outcome_unknown","detail":"FDB回执期间owner已变化，请按原请求查询"}),
                    ))
                };
            }
            Err(_) => continue,
        }
    }
}
async fn writer(
    ctx: &Context,
    serial: &str,
    expected: ActiveLibraryRoute,
    request: Request,
) -> Result<(Serving, Request), Box<Response>> {
    let local = serving(ctx, serial, expected)?;
    if local.local {
        return Ok((local, request));
    }
    if request.headers().contains_key("x-tapers-forwarded") {
        return Err(busy("owner已变化，请按原请求查询"));
    }
    let owner = ctx.owner.as_ref().expect("serving verified owner context");
    let address = owner
        .peers
        .get(&local.node)
        .ok_or_else(|| busy("owner物理地址未配置"))?;
    let permit = owner
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy("owner转发并发已达上限"))?;
    Err(Box::new(
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            gateway::forward_request(request, address, permit, serial),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => reply(
                StatusCode::GATEWAY_TIMEOUT,
                json!({"error":"outcome_unknown","detail":"管理回执超时；保留原请求身份并查询，不要换ID"}),
            ),
        },
    ))
}
async fn body<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, Box<Response>> {
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(request.into_body(), 4096),
    )
    .await
    .map_err(|_| {
        boxed_reply(
            StatusCode::REQUEST_TIMEOUT,
            json!({"error":"body_timeout","detail":"管理请求体读取超时"}),
        )
    })?
    .map_err(|_| bad("管理请求体读取失败或超过4KiB"))?;
    serde_json::from_slice(&bytes).map_err(|_| bad("管理JSON字段无效、重复或超出类型范围"))
}
fn canonical_id(text: &str) -> Result<Uuid, Box<Response>> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or_else(|| bad("标识须为规范的非零UUID"))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPolicy {
    media_restriction: String,
    mount_limit: u16,
    low_space_warning_enabled: bool,
    low_space_warning_threshold: u16,
    no_space_warning_enabled: bool,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPool {
    version: u8,
    id: String,
    name: String,
    file_limit: u64,
    created_at_millis: u64,
    policy: NewPolicy,
}
pub(super) async fn create(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    use crate::managed_catalog::{ActivePoolCreation, PoolPolicy, PoolRecord};
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let (owner, request) = match writer(&ctx, &serial, expected, request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let input: NewPool = match body(request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if input.version != 1 {
        return invalid("池创建请求版本无效");
    }
    let id = match canonical_id(&input.id) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let pool = PoolRecord {
        id,
        library_serial: serial.clone(),
        name: input.name,
        file_limit: input.file_limit,
        created_at_millis: input.created_at_millis,
        revision: 1,
        policy: PoolPolicy {
            media_restriction: input.policy.media_restriction,
            mount_limit: input.policy.mount_limit,
            low_space_warning_enabled: input.policy.low_space_warning_enabled,
            low_space_warning_threshold: input.policy.low_space_warning_threshold,
            no_space_warning_enabled: input.policy.no_space_warning_enabled,
        },
    };
    if let Err(e) = pool.to_json() {
        return failure(e);
    }
    if !live(&owner, expected) {
        return unavailable("请求读取期间owner许可已失效，未开始创建");
    }
    let result = permitted(
        &owner,
        expected,
        ctx.catalog
            .create_active_pool(&serial, expected, owner.round, &pool),
    )
    .await;
    if !live(&owner, expected) {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"outcome_unknown","pool":id.to_string(),"detail":"创建期间owner已变化，请按原UUID查询或重试"}),
        );
    }
    match result {
        Ok(result) => {
            let (pool, created) = match result {
                ActivePoolCreation::Created(p) => (p, true),
                ActivePoolCreation::AlreadyExists(p) => (p, false),
                ActivePoolCreation::IdConflict
                | ActivePoolCreation::NameConflict
                | ActivePoolCreation::Retired => {
                    return reply(
                        StatusCode::CONFLICT,
                        json!({"error":"pool_conflict","id":id.to_string(),"detail":"池ID、名称或退役状态冲突；请查询原UUID，不要重新生成ID"}),
                    );
                }
            };
            match pool.to_json() {
                Ok(p) => reply(
                    if created {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    },
                    json!({"library_serial":serial,"namespace":expected.route.namespace.to_string(),"created":created,"pool":p}),
                ),
                Err(e) => failure(e),
            }
        }
        Err(response) => *response,
    }
}

#[derive(serde::Serialize)]
struct UniquePatch(std::collections::BTreeMap<String, Value>);
impl<'de> serde::Deserialize<'de> for UniquePatch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PatchVisitor;
        impl<'de> serde::de::Visitor<'de> for PatchVisitor {
            type Value = UniquePatch;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("不含重复字段的池策略对象")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut result = std::collections::BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if result.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("池策略字段重复"));
                    }
                }
                Ok(UniquePatch(result))
            }
        }
        deserializer.deserialize_map(PatchVisitor)
    }
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum PoolChange {
    #[serde(rename = "pool")]
    Policy {
        id: String,
        revision: u64,
        patch: UniquePatch,
    },
    #[serde(rename = "pool-delete")]
    Delete { id: String, revision: u64 },
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    library: String,
    namespace: String,
    epoch: u64,
    operation: String,
    change: PoolChange,
}
async fn admit(
    ctx: Arc<Context>,
    serial: String,
    id: String,
    request: Request,
    delete: bool,
) -> Response {
    use crate::managed_catalog::ManagementRequest;
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let target = match canonical_id(&id) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let (owner, request) = match writer(&ctx, &serial, expected, request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let envelope: Envelope = match body(request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let envelope = serde_json::to_value(envelope).expect("validated envelope serializes");
    if envelope["change"]["id"] != target.to_string()
        || envelope["change"]["kind"] != if delete { "pool-delete" } else { "pool" }
    {
        return invalid("请求操作或池UUID与HTTP路径不符");
    }
    let intent = match ManagementRequest::parse(envelope) {
        Ok(v) => v,
        Err(e) => return failure(e),
    };
    if !live(&owner, expected) {
        return unavailable("提交前owner许可已失效，保留原请求");
    }
    match permitted(
        &owner,
        expected,
        ctx.catalog
            .admit_management(&serial, expected, owner.round, &intent),
    )
    .await
    {
        Ok(task) => reply(StatusCode::ACCEPTED, task),
        Err(response) => *response,
    }
}
pub(super) async fn set(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    admit(ctx, serial, id, request, false).await
}
pub(super) async fn delete(
    State(ctx): State<Arc<Context>>,
    Path((serial, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    admit(ctx, serial, id, request, true).await
}
fn task_id(epoch: &str, id: &str) -> Result<(u64, Uuid), Box<Response>> {
    let number = epoch
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == epoch)
        .ok_or_else(|| bad("管理请求代次必须为规范正整数"))?;
    Ok((number, canonical_id(id)?))
}
pub(super) async fn task(
    State(ctx): State<Arc<Context>>,
    Path((serial, epoch, id)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let (epoch, id) = match task_id(&epoch, &id) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match ctx
        .catalog
        .management_task(&serial, expected, epoch, id)
        .await
    {
        Ok(Some(task)) => reply(StatusCode::OK, task),
        Ok(None) => reply(StatusCode::NOT_FOUND, json!({"error":"task_not_found"})),
        Err(e) => failure(e),
    }
}
pub(super) async fn tasks(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let limit = match query_value(&request, "limit") {
        Ok(None) => 100,
        Ok(Some(s)) => match s.parse::<usize>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return invalid("任务分页limit须在1..=100"),
        },
        Err(e) => return invalid(e),
    };
    let after = match query_value(&request, "after") {
        Ok(None) => None,
        Ok(Some(s)) => match URL_SAFE_NO_PAD.decode(s) {
            Ok(v) if !v.is_empty() => Some(v),
            _ => return invalid("任务分页游标无效"),
        },
        Err(e) => return invalid(e),
    };
    match ctx
        .catalog
        .list_management_tasks(&serial, expected, after.as_deref(), limit)
        .await
    {
        Ok(page) => reply(
            StatusCode::OK,
            json!({"library_serial":serial,"namespace":expected.route.namespace.to_string(),"items":page.items,"next":page.next.map(|v|URL_SAFE_NO_PAD.encode(v))}),
        ),
        Err(e) => failure(e),
    }
}
pub(super) async fn cancel(
    State(ctx): State<Arc<Context>>,
    Path((serial, epoch, id)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let _permit = match budget(&ctx) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let expected = match route(&ctx, &serial) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = namespace(request.headers(), expected) {
        return *r;
    }
    let (epoch, id) = match task_id(&epoch, &id) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let (owner, _request) = match writer(&ctx, &serial, expected, request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if !live(&owner, expected) {
        return unavailable("取消前owner许可已失效");
    }
    match permitted(
        &owner,
        expected,
        ctx.catalog
            .cancel_management(&serial, expected, owner.round, epoch, id),
    )
    .await
    {
        Ok(task) => reply(StatusCode::OK, task),
        Err(response) => *response,
    }
}

pub(super) async fn poll(
    ctx: &Context,
    serial: &str,
    after: &mut Option<(u64, Uuid)>,
) -> Result<(), CatalogError> {
    let expected = ctx.routes[serial];
    let owner = match serving(ctx, serial, expected) {
        Ok(s) if s.local => s,
        _ => return Ok(()),
    };
    let Ok(_permit) = ctx.admin_requests.clone().try_acquire_owned() else {
        return Ok(());
    };
    let window = ctx
        .catalog
        .active_management_window(serial, expected)
        .await?;
    if after.is_some_and(|(epoch, _)| epoch < window.first_retained || epoch > window.accepting) {
        *after = None;
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        if !live(&owner, expected) {
            return Ok(());
        }
        let Some(identity) = ctx
            .catalog
            .next_management(serial, expected, owner.round, *after)
            .await?
        else {
            return Ok(());
        };
        if !seen.insert(identity) {
            return Ok(());
        }
        *after = Some(identity);
        let task = ctx
            .catalog
            .management_task(serial, expected, identity.0, identity.1)
            .await?
            .ok_or(CatalogError::Corrupt("管理队列缺少任务"))?;
        if !matches!(
            task["request"]["change"]["kind"].as_str(),
            Some("pool" | "pool-delete")
        ) {
            continue;
        }
        if live(&owner, expected)
            && permitted(&owner, expected, async {
                let result = ctx
                    .catalog
                    .apply_pool_management(serial, expected, owner.round, identity.0, identity.1)
                    .await;
                if let Err(error) = &result {
                    log::warn!(
                        "带库{}池任务epoch={} operation={} FDB apply失败：{}",
                        serial,
                        identity.0,
                        identity.1,
                        error
                    );
                }
                result
            })
            .await
            .is_err()
        {
            log::warn!(
                "带库{}池任务epoch={} operation={}等待依赖或原回执核对",
                serial,
                identity.0,
                identity.1
            );
            return Err(CatalogError::Incomplete);
        }
        return Ok(());
    }
    Ok(())
}
fn poll_delay(failures: u32) -> std::time::Duration {
    if failures == 0 {
        return std::time::Duration::from_millis(250);
    }
    let base = (250u64 << failures.min(7)).min(30_000);
    std::time::Duration::from_millis(base + u64::from(Uuid::new_v4().as_bytes()[0]))
}
/// The worker holds only Weak<Context> between polls. It never runs device work
/// or blocks the Raft thread; each library has an independently paced future.
pub(super) fn start_worker(ctx: &Arc<Context>) -> std::io::Result<()> {
    let names = ctx.routes.keys().cloned().collect::<Vec<_>>();
    let weak = Arc::downgrade(ctx);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("fdb-pool-metadata".into())
        .spawn(move || {
            runtime.block_on(async move {
                let mut jobs = tokio::task::JoinSet::new();
                for serial in names {
                    let weak = weak.clone();
                    jobs.spawn(async move {
                        let mut after = None;
                        let mut failures = 0u32;
                        loop {
                            let Some(context) = weak.upgrade() else {
                                return;
                            };
                            match poll(&context, &serial, &mut after).await {
                                Ok(()) => failures = 0,
                                Err(error) => {
                                    failures = failures.saturating_add(1);
                                    log::warn!(
                                        "带库{}池管理依赖不可用，保留原任务（游标{:?}）：{}",
                                        serial,
                                        after,
                                        error
                                    );
                                }
                            }
                            drop(context);
                            tokio::time::sleep(poll_delay(failures)).await;
                        }
                    });
                }
                while let Some(result) = jobs.join_next().await {
                    if let Err(error) = result {
                        log::error!("池元数据执行器退出：{}", error);
                    }
                }
            })
        })?;
    Ok(())
}

#[cfg(test)]
mod backoff_tests {
    use super::*;
    #[test]
    fn dependency_failures_back_off_with_a_bounded_budget() {
        assert_eq!(poll_delay(0), std::time::Duration::from_millis(250));
        for failures in [1, 2, 7, u32::MAX] {
            let ms = poll_delay(failures).as_millis();
            let base = (250u128 << failures.min(7)).min(30_000);
            assert!((base..=base + 255).contains(&ms));
        }
    }
}
