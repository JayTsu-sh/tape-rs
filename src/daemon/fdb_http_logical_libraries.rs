//! Logical library metadata endpoints in the physical library's admin scope.

use super::*;
use crate::logical_library::{CreateLogicalLibrary, MAX_REQUEST_BYTES};
use crate::managed_catalog::LogicalLibraryCreation;

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
