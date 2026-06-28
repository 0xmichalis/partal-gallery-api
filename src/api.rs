use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::db::GalleryRow;

/// Response body for a stored gallery document.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct GalleryResponse {
    #[schema(example = "0xabc0000000000000000000000000000000000001")]
    pub address: String,
    /// The galleries for this address as an opaque JSON array (GalleryMinimal[]).
    #[schema(value_type = Vec<Object>)]
    pub data: serde_json::Value,
    /// ISO 8601 timestamp.
    #[schema(example = "2026-06-28T12:00:00+00:00")]
    pub created_at: String,
    /// ISO 8601 timestamp.
    #[schema(example = "2026-06-28T12:00:00+00:00")]
    pub updated_at: String,
}

impl From<GalleryRow> for GalleryResponse {
    fn from(row: GalleryRow) -> Self {
        Self {
            address: row.address,
            data: row.data,
            created_at: row.created_at.to_rfc3339(),
            updated_at: row.updated_at.to_rfc3339(),
        }
    }
}

/// Request body for upserting a gallery document.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutGalleryRequest {
    /// The full galleries array to store for the address. Must be a JSON array.
    #[schema(value_type = Vec<Object>)]
    pub data: serde_json::Value,
}

/// RFC 7807 problem+json error shape.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApiProblem {
    #[serde(default = "default_problem_type")]
    pub r#type: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

fn default_problem_type() -> String {
    "about:blank".to_string()
}

impl ApiProblem {
    pub fn new(status: StatusCode, detail: Option<String>, instance: Option<String>) -> Self {
        Self {
            r#type: default_problem_type(),
            title: status.canonical_reason().unwrap_or("Error").to_string(),
            status: status.as_u16(),
            detail,
            instance,
        }
    }
}

/// Responder that serializes an `ApiProblem` with `application/problem+json`.
pub struct ProblemJson(pub ApiProblem);

impl ProblemJson {
    pub fn from_status(
        status: StatusCode,
        detail: Option<String>,
        instance: Option<String>,
    ) -> Self {
        Self(ApiProblem::new(status, detail, instance))
    }
}

impl IntoResponse for ProblemJson {
    fn into_response(self) -> axum::response::Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, headers, Json(self.0)).into_response()
    }
}

#[cfg(test)]
mod api_problem_tests {
    use super::ApiProblem;
    use axum::http::StatusCode;

    #[test]
    fn derives_title_from_status() {
        let p = ApiProblem::new(
            StatusCode::NOT_FOUND,
            Some("Gallery not found".to_string()),
            Some("/v1/galleries/0xabc".to_string()),
        );
        assert_eq!(p.title, "Not Found");
        assert_eq!(p.status, 404);
        assert_eq!(p.r#type, "about:blank");
        assert_eq!(p.instance.as_deref(), Some("/v1/galleries/0xabc"));
        assert_eq!(p.detail.as_deref(), Some("Gallery not found"));
    }

    #[test]
    fn serializes_and_deserializes() {
        let p = ApiProblem::new(
            StatusCode::UNAUTHORIZED,
            Some("Unauthorized".to_string()),
            None,
        );
        let json = serde_json::to_string(&p).unwrap();
        // `instance` is omitted when None.
        assert!(!json.contains("instance"));
        let back: ApiProblem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title, p.title);
        assert_eq!(back.status, p.status);
        assert_eq!(back.detail, p.detail);
    }
}
