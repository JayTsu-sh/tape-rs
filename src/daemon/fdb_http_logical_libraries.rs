//! Logical library metadata endpoints in the physical library's admin scope.

use super::*;
use crate::logical_library::{ChangeLogicalLibrary, CreateLogicalLibrary, MAX_REQUEST_BYTES};
use crate::managed_catalog::{LogicalLibraryChange, LogicalLibraryCreation};

fn envelope(mut value: Value, serial: &str, route: ActiveLibraryRoute) -> Value {
    value["version"] = json!(1);
    value["library_serial"] = json!(serial);
    value["namespace"] = json!(route.route.namespace);
    value
}

pub(super) async fn create(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    let (_permit, route) = match queries::scoped(&ctx, &serial, &request, &[]) {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let (owner, request) = match pools::writer(&ctx, &serial, route, request).await {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let input: CreateLogicalLibrary = match pools::body_with_limit(request, MAX_REQUEST_BYTES).await
    {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let result = pools::permitted(
        &owner,
        route,
        ctx.catalog
            .create_logical_library(&serial, route, owner.round, input),
    )
    .await;
    let (status, record) = match result {
        Ok(LogicalLibraryCreation::Created(record)) => (StatusCode::CREATED, record),
        Ok(LogicalLibraryCreation::Replayed(record)) => (StatusCode::OK, record),
        Ok(LogicalLibraryCreation::Conflict(detail)) => {
            return reply(
                StatusCode::CONFLICT,
                json!({"error":"logical_library_conflict","detail":detail}),
            );
        }
        Ok(LogicalLibraryCreation::InventoryUnavailable) => {
            return reply(
                StatusCode::PRECONDITION_FAILED,
                json!({"error":"inventory_unavailable","detail":"所需的本轮完整库存未就绪或已过期，请刷新物理带库库存"}),
            );
        }
        Err(error) => return *error,
    };
    reply(
        status,
        envelope(json!({"logical_library":record}), &serial, route),
    )
}

async fn view(
    ctx: Arc<Context>,
    serial: String,
    selector: Option<&str>,
    request: Request,
) -> Response {
    let (_permit, route) = match queries::scoped(&ctx, &serial, &request, &[]) {
        Ok(value) => value,
        Err(error) => return *error,
    };
    match ctx
        .catalog
        .logical_library_view(&serial, route, selector)
        .await
    {
        Ok(Some(value)) => reply(StatusCode::OK, envelope(value, &serial, route)),
        Ok(None) => reply(
            StatusCode::NOT_FOUND,
            json!({"error":"logical_library_not_found"}),
        ),
        Err(error) => pools::failure(error),
    }
}

pub(super) async fn list(
    State(ctx): State<Arc<Context>>,
    Path(serial): Path<String>,
    request: Request,
) -> Response {
    view(ctx, serial, None, request).await
}

pub(super) async fn show(
    State(ctx): State<Arc<Context>>,
    Path((serial, selector)): Path<(String, String)>,
    request: Request,
) -> Response {
    view(ctx, serial, Some(&selector), request).await
}

pub(super) async fn replace(
    State(ctx): State<Arc<Context>>,
    Path((serial, target)): Path<(String, String)>,
    request: Request,
) -> Response {
    change(ctx, serial, target, request, false).await
}

pub(super) async fn delete(
    State(ctx): State<Arc<Context>>,
    Path((serial, target)): Path<(String, String)>,
    request: Request,
) -> Response {
    change(ctx, serial, target, request, true).await
}

async fn change(
    ctx: Arc<Context>,
    serial: String,
    target: String,
    request: Request,
    deleting: bool,
) -> Response {
    let (_permit, route) = match queries::scoped(&ctx, &serial, &request, &[]) {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let id = match pools::canonical_id(&target) {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let (owner, request) = match pools::writer(&ctx, &serial, route, request).await {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let input =
        match pools::body_with_limit::<ChangeLogicalLibrary>(request, MAX_REQUEST_BYTES).await {
            Ok(value) if value.replacement.is_none() == deleting => value,
            Ok(_) => return invalid("变更内容与 HTTP 操作不符"),
            Err(error) => return *error,
        };
    match pools::permitted(
        &owner,
        route,
        ctx.catalog
            .change_logical_library(&serial, route, owner.round, id, input),
    )
    .await
    {
        Ok(LogicalLibraryChange::Applied(receipt)) => {
            reply(StatusCode::OK, envelope(receipt, &serial, route))
        }
        Ok(LogicalLibraryChange::Conflict(detail)) => reply(
            StatusCode::CONFLICT,
            json!({"error":"logical_library_conflict","detail":detail}),
        ),
        Ok(LogicalLibraryChange::InventoryUnavailable) => reply(
            StatusCode::PRECONDITION_FAILED,
            json!({"error":"inventory_unavailable","detail":"变更资源前须核对本轮完整库存"}),
        ),
        Err(error) => *error,
    }
}

pub(super) async fn change_status(
    State(ctx): State<Arc<Context>>,
    Path((serial, target, operation)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let (_permit, route) = match queries::scoped(&ctx, &serial, &request, &[]) {
        Ok(value) => value,
        Err(error) => return *error,
    };
    let (id, operation) = match pools::canonical_id(&target)
        .and_then(|id| pools::canonical_id(&operation).map(|operation| (id, operation)))
    {
        Ok(value) => value,
        Err(error) => return *error,
    };
    match ctx
        .catalog
        .logical_library_change_receipt(&serial, route, id, operation)
        .await
    {
        Ok(Some(receipt)) => reply(StatusCode::OK, envelope(receipt, &serial, route)),
        Ok(None) => reply(
            StatusCode::NOT_FOUND,
            json!({"error":"logical_library_change_not_found"}),
        ),
        Err(error) => pools::failure(error),
    }
}
