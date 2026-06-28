use std::sync::Arc;

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use subtle::ConstantTimeEq;
use tracing::error;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::{ApiProblem, GalleryResponse, ProblemJson, PutGalleryRequest};
use crate::db::Db;

/// Maximum request body size for gallery uploads (10 MiB). Galleries store only
/// minimal NFT references, so this comfortably covers very large collections.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth_token: Arc<String>,
}

#[derive(OpenApi)]
#[openapi(
    paths(health, get_gallery, put_gallery, delete_gallery),
    components(schemas(GalleryResponse, PutGalleryRequest, ApiProblem)),
    tags(
        (name = "galleries", description = "Per-address gallery document storage"),
        (name = "system", description = "Health and liveness")
    ),
    info(
        title = "Partal Gallery API",
        version = env!("CARGO_PKG_VERSION"),
        description = "A thin, bearer-token authenticated per-address JSON document store for Partal user galleries (a self-hosted replacement for Supabase).",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    security(("bearer_auth" = [])),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                utoipa::openapi::security::SecurityScheme::Http(
                    utoipa::openapi::security::HttpBuilder::new()
                        .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                        .description(Some(
                            "Shared bearer token (the server's GALLERY_AUTH_TOKEN)",
                        ))
                        .build(),
                ),
            );
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    // `/health` and the API docs are public; everything else requires the token.
    let public = Router::new()
        .route("/health", get(health))
        .merge(SwaggerUi::new("/v1/swagger-ui").url("/v1/openapi.json", ApiDoc::openapi()));

    let authed = Router::new()
        .route(
            "/v1/galleries/{address}",
            get(get_gallery).put(put_gallery).delete(delete_gallery),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    public.merge(authed)
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "system",
    responses((status = 200, description = "Service is healthy", body = String))
)]
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Bearer-token auth. The only client is Partal's backend (which performs the
/// real user auth), so a single shared secret, compared in constant time, is
/// sufficient and matches the nftbk-server model.
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let expected = format!("Bearer {}", state.auth_token);
    let authorized = provided
        .map(|h| h.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1)
        .unwrap_or(false);

    if authorized {
        return next.run(req).await;
    }

    (
        [(header::WWW_AUTHENTICATE, "Bearer")],
        ProblemJson::from_status(
            StatusCode::UNAUTHORIZED,
            Some("Unauthorized".to_string()),
            Some(req.uri().to_string()),
        ),
    )
        .into_response()
}

/// Normalize an address to the storage key. Lowercasing matches the legacy
/// Supabase behavior (addresses were stored lowercased for all chains), so
/// existing keys stay consistent and reads/writes remain symmetric.
fn normalize_address(address: &str) -> String {
    address.to_lowercase()
}

#[utoipa::path(
    get,
    path = "/v1/galleries/{address}",
    tag = "galleries",
    params(("address" = String, Path, description = "Wallet address (lowercased server-side)")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Stored gallery document", body = GalleryResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ApiProblem),
        (status = 404, description = "No gallery stored for this address", body = ApiProblem)
    )
)]
async fn get_gallery(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> axum::response::Response {
    let address = normalize_address(&address);
    match state.db.get_gallery(&address).await {
        Ok(Some(row)) => Json(GalleryResponse::from(row)).into_response(),
        Ok(None) => ProblemJson::from_status(
            StatusCode::NOT_FOUND,
            Some("Gallery not found".to_string()),
            Some(format!("/v1/galleries/{address}")),
        )
        .into_response(),
        Err(e) => internal_error("get_gallery", &address, e),
    }
}

#[utoipa::path(
    put,
    path = "/v1/galleries/{address}",
    tag = "galleries",
    params(("address" = String, Path, description = "Wallet address (lowercased server-side)")),
    request_body = PutGalleryRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Upserted gallery document", body = GalleryResponse),
        (status = 400, description = "`data` is not a JSON array, or the body is invalid", body = ApiProblem),
        (status = 401, description = "Missing or invalid bearer token", body = ApiProblem)
    )
)]
async fn put_gallery(
    State(state): State<AppState>,
    Path(address): Path<String>,
    // Take the extractor result so malformed/invalid JSON, wrong content-type,
    // and over-limit bodies become problem+json like every other error path,
    // instead of axum's default text/plain rejection.
    body: Result<Json<PutGalleryRequest>, JsonRejection>,
) -> axum::response::Response {
    let address = normalize_address(&address);
    let instance = format!("/v1/galleries/{address}");

    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => {
            return ProblemJson::from_status(rej.status(), Some(rej.body_text()), Some(instance))
                .into_response();
        }
    };

    if !body.data.is_array() {
        return ProblemJson::from_status(
            StatusCode::BAD_REQUEST,
            Some("`data` must be a JSON array of galleries".to_string()),
            Some(instance),
        )
        .into_response();
    }

    match state.db.upsert_gallery(&address, &body.data).await {
        Ok(row) => Json(GalleryResponse::from(row)).into_response(),
        Err(e) => internal_error("put_gallery", &address, e),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/galleries/{address}",
    tag = "galleries",
    params(("address" = String, Path, description = "Wallet address (lowercased server-side)")),
    security(("bearer_auth" = [])),
    responses(
        (status = 204, description = "Deleted (idempotent — also 204 when nothing existed)"),
        (status = 401, description = "Missing or invalid bearer token", body = ApiProblem)
    )
)]
async fn delete_gallery(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> axum::response::Response {
    let address = normalize_address(&address);
    match state.db.delete_gallery(&address).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error("delete_gallery", &address, e),
    }
}

fn internal_error(op: &str, address: &str, e: sqlx::Error) -> axum::response::Response {
    error!("{op} failed for {address}: {e}");
    ProblemJson::from_status(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("Internal server error".to_string()),
        None,
    )
    .into_response()
}

#[cfg(test)]
mod openapi_tests {
    use super::ApiDoc;
    use utoipa::OpenApi;

    #[test]
    fn spec_documents_endpoints_and_security() {
        let json = serde_json::to_string(&ApiDoc::openapi()).unwrap();
        assert!(json.contains("Partal Gallery API"));
        assert!(json.contains("/v1/galleries/{address}"));
        assert!(json.contains("/health"));
        assert!(json.contains("bearer_auth"));
    }
}

#[cfg(test)]
mod normalize_address_tests {
    use super::normalize_address;

    #[test]
    fn lowercases_evm_addresses() {
        assert_eq!(
            normalize_address("0xAbCdEf0000000000000000000000000000000001"),
            "0xabcdef0000000000000000000000000000000001"
        );
    }

    #[test]
    fn lowercases_consistently_so_reads_match_writes() {
        // Tezos addresses are case-sensitive, but the legacy store lowercased
        // every key; we keep doing so for compatibility, and since both writes
        // and reads normalize identically the key stays internally consistent.
        let mixed = "tz1Burnxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        assert_eq!(normalize_address(mixed), mixed.to_lowercase());
        assert_eq!(
            normalize_address(mixed),
            normalize_address(&mixed.to_uppercase())
        );
    }
}
