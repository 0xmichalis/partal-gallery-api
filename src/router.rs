use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::IntoResponse,
    routing::get,
};
use subtle::ConstantTimeEq;
use tracing::error;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::{ApiProblem, GalleryResponse, ProblemJson, PutGalleryRequest};
use crate::db::{Db, GalleryRow, WriteCondition};

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
        .merge(SwaggerUi::new("/v1/docs").url("/v1/openapi.json", ApiDoc::openapi()));

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
        Ok(Some(row)) => gallery_response(row),
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
    params(
        ("address" = String, Path, description = "Wallet address (lowercased server-side)"),
        ("If-Match" = Option<String>, Header, description = "Strong ETag from GET; rejects stale writes"),
        ("If-None-Match" = Option<String>, Header, description = "Use * to create only when absent")
    ),
    request_body = PutGalleryRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Upserted gallery document", body = GalleryResponse),
        (status = 400, description = "`data` is not a JSON array, or the body is invalid", body = ApiProblem),
        (status = 401, description = "Missing or invalid bearer token", body = ApiProblem),
        (status = 412, description = "Document changed since the read", body = ApiProblem)
    )
)]
async fn put_gallery(
    State(state): State<AppState>,
    Path(address): Path<String>,
    headers: HeaderMap,
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

    let condition = match write_condition(&headers) {
        Ok(condition) => condition,
        Err(problem) => return problem.into_response(),
    };
    match state
        .db
        .upsert_gallery(&address, &body.data, condition)
        .await
    {
        Ok(Some(row)) => gallery_response(row),
        Ok(None) => precondition_failed(),
        Err(e) => internal_error("put_gallery", &address, e),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/galleries/{address}",
    tag = "galleries",
    params(
        ("address" = String, Path, description = "Wallet address (lowercased server-side)"),
        ("If-Match" = Option<String>, Header, description = "Strong ETag from GET; rejects stale deletes"),
        ("If-None-Match" = Option<String>, Header, description = "Use * to require absence")
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 400, description = "Invalid write precondition", body = ApiProblem),
        (status = 204, description = "Deleted (idempotent — also 204 when nothing existed)"),
        (status = 401, description = "Missing or invalid bearer token", body = ApiProblem),
        (status = 412, description = "Document changed since the read", body = ApiProblem)
    )
)]
async fn delete_gallery(
    State(state): State<AppState>,
    Path(address): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    let address = normalize_address(&address);
    let condition = match write_condition(&headers) {
        Ok(condition) => condition,
        Err(problem) => return problem.into_response(),
    };
    match state.db.delete_gallery(&address, condition).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => precondition_failed(),
        Err(e) => internal_error("delete_gallery", &address, e),
    }
}

fn gallery_response(row: GalleryRow) -> axum::response::Response {
    let etag = format!("\"{}\"", row.revision);
    ([(header::ETAG, etag)], Json(GalleryResponse::from(row))).into_response()
}

fn precondition_failed() -> axum::response::Response {
    ProblemJson::from_status(
        StatusCode::PRECONDITION_FAILED,
        Some("Gallery document changed; reload before saving".to_string()),
        None,
    )
    .into_response()
}

fn write_condition(headers: &HeaderMap) -> Result<WriteCondition, ProblemJson> {
    let invalid = || {
        ProblemJson::from_status(
            StatusCode::BAD_REQUEST,
            Some("Invalid write precondition".to_string()),
            None,
        )
    };
    if let Some(value) = headers.get(header::IF_NONE_MATCH) {
        if value == "*" && !headers.contains_key(header::IF_MATCH) {
            return Ok(WriteCondition::Absent);
        }
        return Err(invalid());
    }
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(WriteCondition::Any);
    };
    let value = value.to_str().map_err(|_| invalid())?;
    let revision = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|revision| *revision > 0)
        .ok_or_else(invalid)?;
    Ok(WriteCondition::Matches(revision))
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

#[cfg(test)]
mod precondition_tests {
    use super::{WriteCondition, write_condition};
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn accepts_strong_revision_or_create_only_and_rejects_ambiguous_conditions() {
        let mut headers = HeaderMap::new();
        assert!(matches!(write_condition(&headers), Ok(WriteCondition::Any)));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(matches!(
            write_condition(&headers),
            Ok(WriteCondition::Absent)
        ));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"7\""));
        assert!(write_condition(&headers).is_err());
        headers.remove(header::IF_NONE_MATCH);
        assert!(matches!(
            write_condition(&headers),
            Ok(WriteCondition::Matches(7))
        ));
        for invalid in ["*", "W/\"7\"", "\"0\"", "\"-1\"", "\"1\", \"2\"", "7"] {
            headers.insert(header::IF_MATCH, HeaderValue::from_str(invalid).unwrap());
            assert!(write_condition(&headers).is_err(), "{invalid}");
        }
    }
}
