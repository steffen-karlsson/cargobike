//! HTTP plumbing: RFC 9457 problem details and health surfaces.
//!
//! Errors: `{type, title, status, detail, instance, code}` ; the
//! extension member `code` is one of the vocabulary constants.

use axum::Json;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;

/// HTTP error codes lives in the api crate's vocabulary later;
/// the server's survival constants here are the subset routes raise.
pub const RELEASE_NOT_FOUND: &str = "ReleaseNotFound";
pub const INTERNAL_ERROR: &str = "InternalError";
pub const INVALID_REQUEST: &str = "InvalidRequest";
pub const STATE_CONFLICT: &str = "StateConflict";
pub const TEMPLATE_NOT_FOUND: &str = "TemplateNotFound";
const CANONICAL_FORBIDDEN_SLUG: &str = "forbidden-resource";
const FORBIDDEN_RESOURCE: &str = "ForbiddenResource";

/// An API error with the RFC 9457 shape baked in .
#[derive(Debug)]
pub struct ApiError {
    /// HTTP status of the problem response.
    pub status: StatusCode,
    /// Code constant (the vocabulary).
    pub code: &'static str,
    /// Human detail.
    pub detail: String,
    /// Slug for `type` (a kebab-case hint), e.g. `release-not-found`.
    pub slug: &'static str,
}

impl ApiError {
    /// A problem response ready for `IntoResponse`.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        slug: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            slug,
            detail: detail.into(),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "type": format!("https://cargobike.dev/errors/{}", self.slug),
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "detail": self.detail,
            "instance": "",
            "code": self.code,
        });
        (self.status, Json(body)).into_response()
    }
}

impl ApiError {
    /// A 403 problem ( `ForbiddenResource`).
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(
            axum::http::StatusCode::FORBIDDEN,
            FORBIDDEN_RESOURCE,
            CANONICAL_FORBIDDEN_SLUG,
            detail,
        )
    }
}

/// A 500 from an internal failure .
pub fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        INTERNAL_ERROR,
        "internal-error",
        format!("{error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn test_problem_details_bear_type_title_status_detail_code() {
        let error = internal("boom");
        let response = error.into_response();
        expect(response, 500, "InternalError");
    }

    #[test]
    fn test_not_found_problem_shape() {
        let error = ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            RELEASE_NOT_FOUND,
            "release-not-found",
            "no release",
        );
        expect(error.into_response(), 404, "ReleaseNotFound");
    }

    fn expect(response: Response, status: u16, code: &str) {
        assert_eq!(response.status().as_u16(), status);
        let body = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt")
            .block_on(async {
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body")
            });
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["status"], status);
        assert_eq!(json["code"], code);
        let expected = if status == 500 {
            "Internal Server Error"
        } else {
            "Not Found"
        };
        assert_eq!(json["title"], expected);
    }
}
