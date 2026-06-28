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

use crate::api::{GalleryResponse, ProblemJson, PutGalleryRequest};
use crate::db::Db;

/// Maximum request body size for gallery uploads (10 MiB). Galleries store only
/// minimal NFT references, so this comfortably covers very large collections.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth_token: Arc<String>,
}

pub fn build_router(state: AppState) -> Router {
    // `/health` is public (liveness probe); everything else requires the token.
    let public = Router::new().route("/health", get(health));

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
