//! Unified API error type and RFC 9457 problem+json responses.
//!
//! [`ApiError`] is the single error type returned by every handler in this
//! crate. It covers all HTTP failure modes and provides [`From`] conversions
//! from the domain-layer error types (`StorageError`, `AuthError`,
//! `DomainError`, `GameError`) so that handlers can use `?` directly.
//!
//! # Security
//!
//! [`ApiError::Internal`] intentionally suppresses the underlying detail from
//! the response body. The real message is emitted via [`tracing::error!`]
//! instead. No other variant leaks sensitive internal state.
//!
//! # HTTP mapping
//!
//! | `ApiError` variant      | HTTP status |
//! |-------------------------|-------------|
//! | `NotFound`              | 404         |
//! | `Conflict`              | 409         |
//! | `BadRequest`            | 400         |
//! | `Unauthorized`          | 401         |
//! | `Forbidden`             | 403         |
//! | `UnprocessableEntity`   | 422         |
//! | `Internal`              | 500         |

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

use mcs_auth::AuthError;
use mcs_core::GameError;
use mcs_domain::DomainError;
use mcs_game::MatchmakingError;
use mcs_storage::error::StorageError;

// ---------------------------------------------------------------------------
// ApiError
// ---------------------------------------------------------------------------

/// The unified error type for every HTTP handler in the MCS API.
///
/// Each variant wraps a human-readable detail string that is safe to include
/// in the response body — it must never contain sensitive internal state.
/// The sole exception is [`ApiError::Internal`], whose detail string is
/// **not** included in the body; it is logged and replaced with a generic
/// message.
///
/// Handlers typically obtain an `ApiError` via the `?` operator together with
/// one of the provided [`From`] implementations.
#[derive(Debug, Error)]
pub enum ApiError {
    /// A requested resource could not be found.
    ///
    /// Maps to HTTP **404 Not Found**.
    #[error("not found: {0}")]
    NotFound(String),

    /// A write operation conflicted with existing data (e.g. unique constraint).
    ///
    /// Maps to HTTP **409 Conflict**.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The request is syntactically or semantically invalid.
    ///
    /// Maps to HTTP **400 Bad Request**.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The caller is not authenticated or the credential was rejected.
    ///
    /// Maps to HTTP **401 Unauthorized**.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The caller is authenticated but lacks permission.
    ///
    /// Maps to HTTP **403 Forbidden**.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The request is well-formed but the contained data failed domain
    /// validation (e.g. an invalid Ethereum address, a malformed UUID).
    ///
    /// Maps to HTTP **422 Unprocessable Entity**.
    #[error("unprocessable entity: {0}")]
    UnprocessableEntity(String),

    /// An unexpected server-side failure. The detail is **logged** but
    /// **never** included in the response body to avoid leaking internals.
    ///
    /// Maps to HTTP **500 Internal Server Error**.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// Returns the HTTP status code corresponding to this error.
    pub fn status_code(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::UnprocessableEntity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Returns the title string (the HTTP reason phrase) for this error.
    ///
    /// Used as the `"title"` field in RFC 9457 problem+json bodies.
    pub fn title(&self) -> &'static str {
        match self {
            ApiError::NotFound(_) => "Not Found",
            ApiError::Conflict(_) => "Conflict",
            ApiError::BadRequest(_) => "Bad Request",
            ApiError::Unauthorized(_) => "Unauthorized",
            ApiError::Forbidden(_) => "Forbidden",
            ApiError::UnprocessableEntity(_) => "Unprocessable Entity",
            ApiError::Internal(_) => "Internal Server Error",
        }
    }

    /// Returns the detail string that is safe to expose to callers.
    ///
    /// For [`ApiError::Internal`] this is always a generic message; the real
    /// detail is accessible only via the [`Display`](std::fmt::Display)
    /// implementation and the tracing log.
    pub fn safe_detail(&self) -> &str {
        match self {
            ApiError::NotFound(d)
            | ApiError::Conflict(d)
            | ApiError::BadRequest(d)
            | ApiError::Unauthorized(d)
            | ApiError::Forbidden(d)
            | ApiError::UnprocessableEntity(d) => d.as_str(),
            // Never leak internal detail to the caller.
            ApiError::Internal(_) => "An unexpected internal error occurred.",
        }
    }
}

// ---------------------------------------------------------------------------
// RFC 9457 problem+json body
// ---------------------------------------------------------------------------

/// Serializable body for an RFC 9457 `application/problem+json` response.
#[derive(Debug, Serialize)]
struct ProblemBody<'a> {
    /// A URI reference identifying the problem type. `"about:blank"` means the
    /// description is the HTTP status code's title phrase.
    #[serde(rename = "type")]
    problem_type: &'a str,
    /// Short, human-readable summary of the problem type.
    title: &'a str,
    /// The HTTP status code.
    status: u16,
    /// A human-readable explanation specific to this occurrence.
    detail: &'a str,
}

// ---------------------------------------------------------------------------
// IntoResponse
// ---------------------------------------------------------------------------

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log internal errors before we throw away the detail.
        if let ApiError::Internal(ref detail) = self {
            tracing::error!(error = %detail, "internal API error");
        }

        let status = self.status_code();
        let body = ProblemBody {
            problem_type: "about:blank",
            title: self.title(),
            status: status.as_u16(),
            detail: self.safe_detail(),
        };

        let json = serde_json::to_vec(&body).expect("ProblemBody is always serializable");

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/problem+json")
            .body(axum::body::Body::from(json))
            .expect("response is always valid")
    }
}

// ---------------------------------------------------------------------------
// ApiResult
// ---------------------------------------------------------------------------

/// Shorthand for `Result<T, ApiError>`.
///
/// All HTTP handlers in this crate return this type. Downstream handler code
/// can propagate domain errors with `?` via the [`From`] implementations
/// provided for [`StorageError`], [`AuthError`], [`DomainError`], and
/// [`GameError`].
pub type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// From<StorageError>
// ---------------------------------------------------------------------------

/// Maps [`StorageError`] to [`ApiError`].
///
/// | `StorageError` variant | `ApiError` variant |
/// |------------------------|--------------------|
/// | `NotFound`             | `NotFound`         |
/// | `Conflict`             | `Conflict`         |
/// | `Backend`              | `Internal`         |
/// | `Serialization`        | `Internal`         |
///
/// `Backend` and `Serialization` details are **not** forwarded to the
/// caller — they are captured inside `Internal` and logged.
impl From<StorageError> for ApiError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound => ApiError::NotFound("resource not found".to_owned()),
            StorageError::Conflict(detail) => ApiError::Conflict(detail),
            StorageError::Backend(detail) => {
                ApiError::Internal(format!("storage backend error: {detail}"))
            }
            StorageError::Serialization(detail) => {
                ApiError::Internal(format!("storage serialization error: {detail}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// From<AuthError>
// ---------------------------------------------------------------------------

/// Maps [`AuthError`] to [`ApiError`].
///
/// | `AuthError` variant        | `ApiError` variant |
/// |----------------------------|--------------------|
/// | `InvalidMessage`           | `Unauthorized`     |
/// | `SignatureVerification`    | `Unauthorized`     |
/// | `AddressMismatch`          | `Unauthorized`     |
/// | `Expired`                  | `Unauthorized`     |
/// | `InvalidToken`             | `Unauthorized`     |
/// | `Other`                    | `Internal`         |
///
/// All authentication failures use a single generic message to avoid leaking
/// which specific check failed. The `Other` variant goes to `Internal`.
impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidMessage
            | AuthError::SignatureVerification
            | AuthError::AddressMismatch
            | AuthError::Expired
            | AuthError::InvalidToken => ApiError::Unauthorized("authentication failed".to_owned()),
            AuthError::Other(ref detail) => {
                ApiError::Internal(format!("authentication system error: {detail}"))
            }
            // AuthError is #[non_exhaustive]; any future variants that don't
            // fit the above are treated as internal failures.
            _ => ApiError::Internal(format!("authentication error: {err}")),
        }
    }
}

// ---------------------------------------------------------------------------
// From<DomainError>
// ---------------------------------------------------------------------------

/// Maps [`DomainError`] to [`ApiError`].
///
/// All domain errors represent caller-supplied data that failed validation,
/// so they map to client-error variants.
///
/// | `DomainError` variant | `ApiError` variant      |
/// |-----------------------|-------------------------|
/// | `InvalidAddress`      | `UnprocessableEntity`   |
/// | `InvalidId`           | `UnprocessableEntity`   |
impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        // DomainError is #[non_exhaustive]; handle the known cases by name and
        // use a fallback for any future variants.
        match &err {
            DomainError::InvalidAddress(detail) => {
                ApiError::UnprocessableEntity(format!("invalid Ethereum address: {detail}"))
            }
            DomainError::InvalidId(detail) => {
                ApiError::UnprocessableEntity(format!("invalid id: {detail}"))
            }
            // Future variants: treat as a generic validation failure.
            _ => ApiError::UnprocessableEntity(err.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// From<GameError>
// ---------------------------------------------------------------------------

/// Maps [`GameError`] to [`ApiError`].
///
/// | `GameError` variant       | `ApiError` variant |
/// |---------------------------|--------------------|
/// | `UnknownVariant`          | `BadRequest`       |
/// | `NotYourTurn`             | `BadRequest`       |
/// | `IllegalAction`           | `BadRequest`       |
/// | `Finished`                | `Conflict`         |
/// | `InvalidActionPayload`    | `BadRequest`       |
/// | `Serialization`           | `Internal`         |
/// | `Other`                   | `Internal`         |
///
/// `Finished` maps to `Conflict` because the action was reasonable but
/// inapplicable due to the current game state (already ended). The
/// serialization and catch-all variants are internal failures.
impl From<GameError> for ApiError {
    fn from(err: GameError) -> Self {
        match err {
            GameError::UnknownVariant(v) => {
                ApiError::BadRequest(format!("unknown game variant: {v}"))
            }
            GameError::NotYourTurn => ApiError::BadRequest("it is not your turn to act".to_owned()),
            GameError::IllegalAction => {
                ApiError::BadRequest("illegal action in the current position".to_owned())
            }
            GameError::Finished => ApiError::Conflict("the game is already finished".to_owned()),
            GameError::InvalidActionPayload(detail) => {
                ApiError::BadRequest(format!("invalid action payload: {detail}"))
            }
            GameError::Serialization(detail) => {
                ApiError::Internal(format!("game serialization error: {detail}"))
            }
            GameError::Other(detail) => ApiError::Internal(format!("game error: {detail}")),
        }
    }
}

// ---------------------------------------------------------------------------
// From<MatchmakingError>
// ---------------------------------------------------------------------------

/// Maps [`MatchmakingError`] to [`ApiError`].
///
/// Matchmaking only fails when its underlying [`SeekRepo`](mcs_storage::SeekRepo)
/// does, so the error is forwarded through the existing [`StorageError`] mapping
/// (a not-found seek to 404, a conflict to 409, and backend/serialization
/// failures to a logged 500 whose detail never reaches the caller).
impl From<MatchmakingError> for ApiError {
    fn from(err: MatchmakingError) -> Self {
        match err {
            MatchmakingError::Storage(storage) => storage.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    use mcs_auth::AuthError;
    use mcs_core::GameError;
    use mcs_domain::DomainError;
    use mcs_storage::error::StorageError;

    use super::ApiError;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Assert that `into_response()` produces the expected status code and an
    /// `application/problem+json` content-type header.
    async fn assert_response(err: ApiError, expected_status: StatusCode) {
        let response = err.into_response();
        assert_eq!(response.status(), expected_status);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("content-type header must be present")
            .to_str()
            .expect("content-type must be valid UTF-8");
        assert_eq!(
            content_type, "application/problem+json",
            "content-type must be application/problem+json"
        );
    }

    /// Read the response body as a `serde_json::Value`.
    async fn body_json(err: ApiError) -> serde_json::Value {
        let response = err.into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        serde_json::from_slice(&bytes).expect("body must be valid JSON")
    }

    // -----------------------------------------------------------------------
    // ApiError::safe_detail — internal must not leak
    // -----------------------------------------------------------------------

    #[test]
    fn internal_safe_detail_is_generic() {
        let err = ApiError::Internal("secret db password: hunter2".to_owned());
        assert!(
            !err.safe_detail().contains("hunter2"),
            "safe_detail must not expose internal detail"
        );
    }

    // -----------------------------------------------------------------------
    // IntoResponse — status codes and content-type
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn not_found_response() {
        assert_response(ApiError::NotFound("game".into()), StatusCode::NOT_FOUND).await;
    }

    #[tokio::test]
    async fn conflict_response() {
        assert_response(
            ApiError::Conflict("users.address".into()),
            StatusCode::CONFLICT,
        )
        .await;
    }

    #[tokio::test]
    async fn bad_request_response() {
        assert_response(
            ApiError::BadRequest("missing field".into()),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    #[tokio::test]
    async fn unauthorized_response() {
        assert_response(
            ApiError::Unauthorized("authentication failed".into()),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }

    #[tokio::test]
    async fn forbidden_response() {
        assert_response(
            ApiError::Forbidden("not your game".into()),
            StatusCode::FORBIDDEN,
        )
        .await;
    }

    #[tokio::test]
    async fn unprocessable_entity_response() {
        assert_response(
            ApiError::UnprocessableEntity("invalid address".into()),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }

    #[tokio::test]
    async fn internal_response() {
        assert_response(
            ApiError::Internal("db connection failed".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // IntoResponse — Internal must not leak detail in the body
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn internal_body_does_not_leak_detail() {
        let sensitive = "secret connection string: postgres://user:pass@host/db";
        let err = ApiError::Internal(sensitive.to_owned());
        let json = body_json(err).await;
        let detail = json["detail"]
            .as_str()
            .expect("detail field must be a string");
        assert!(
            !detail.contains("secret"),
            "internal error body must not contain sensitive detail; got: {detail}"
        );
        assert!(
            !detail.contains("postgres"),
            "internal error body must not contain sensitive detail; got: {detail}"
        );
    }

    // -----------------------------------------------------------------------
    // IntoResponse — problem+json fields
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn problem_json_has_required_fields() {
        let err = ApiError::NotFound("game xyz".into());
        let json = body_json(err).await;
        assert!(json["type"].is_string(), "problem+json must have 'type'");
        assert!(json["title"].is_string(), "problem+json must have 'title'");
        assert!(
            json["status"].is_number(),
            "problem+json must have 'status'"
        );
        assert!(
            json["detail"].is_string(),
            "problem+json must have 'detail'"
        );
        assert_eq!(json["status"].as_u64().unwrap(), 404);
    }

    // -----------------------------------------------------------------------
    // From<StorageError>
    // -----------------------------------------------------------------------

    #[test]
    fn storage_not_found_maps_to_api_not_found() {
        let err: ApiError = StorageError::NotFound.into();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[tokio::test]
    async fn storage_not_found_status() {
        let err: ApiError = StorageError::NotFound.into();
        assert_response(err, StatusCode::NOT_FOUND).await;
    }

    #[test]
    fn storage_conflict_maps_to_api_conflict() {
        let err: ApiError = StorageError::Conflict("users.address".into()).into();
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    #[tokio::test]
    async fn storage_conflict_status() {
        let err: ApiError = StorageError::Conflict("users.address".into()).into();
        assert_response(err, StatusCode::CONFLICT).await;
    }

    #[test]
    fn storage_backend_maps_to_api_internal() {
        let err: ApiError = StorageError::Backend("connection refused".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn storage_backend_status() {
        let err: ApiError = StorageError::Backend("connection refused".into()).into();
        assert_response(err, StatusCode::INTERNAL_SERVER_ERROR).await;
    }

    #[tokio::test]
    async fn storage_backend_does_not_leak_detail() {
        let err: ApiError = StorageError::Backend("connection refused to 10.0.0.1".into()).into();
        let json = body_json(err).await;
        let detail = json["detail"].as_str().unwrap();
        assert!(!detail.contains("10.0.0.1"));
    }

    #[test]
    fn storage_serialization_maps_to_api_internal() {
        let err: ApiError = StorageError::Serialization("unexpected token".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn storage_serialization_status() {
        let err: ApiError = StorageError::Serialization("unexpected token".into()).into();
        assert_response(err, StatusCode::INTERNAL_SERVER_ERROR).await;
    }

    // -----------------------------------------------------------------------
    // From<AuthError>
    // -----------------------------------------------------------------------

    #[test]
    fn auth_invalid_message_maps_to_unauthorized() {
        let err: ApiError = AuthError::InvalidMessage.into();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn auth_invalid_message_status() {
        let err: ApiError = AuthError::InvalidMessage.into();
        assert_response(err, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn auth_signature_verification_maps_to_unauthorized() {
        let err: ApiError = AuthError::SignatureVerification.into();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn auth_signature_verification_status() {
        let err: ApiError = AuthError::SignatureVerification.into();
        assert_response(err, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn auth_address_mismatch_maps_to_unauthorized() {
        let err: ApiError = AuthError::AddressMismatch.into();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn auth_address_mismatch_status() {
        let err: ApiError = AuthError::AddressMismatch.into();
        assert_response(err, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn auth_expired_maps_to_unauthorized() {
        let err: ApiError = AuthError::Expired.into();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn auth_expired_status() {
        let err: ApiError = AuthError::Expired.into();
        assert_response(err, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn auth_invalid_token_maps_to_unauthorized() {
        let err: ApiError = AuthError::InvalidToken.into();
        assert!(matches!(err, ApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn auth_invalid_token_status() {
        let err: ApiError = AuthError::InvalidToken.into();
        assert_response(err, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn auth_other_maps_to_internal() {
        let err: ApiError = AuthError::Other("clock skew".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn auth_other_status() {
        let err: ApiError = AuthError::Other("clock skew".into()).into();
        assert_response(err, StatusCode::INTERNAL_SERVER_ERROR).await;
    }

    // -----------------------------------------------------------------------
    // From<DomainError>
    // -----------------------------------------------------------------------

    #[test]
    fn domain_invalid_address_maps_to_unprocessable() {
        let err: ApiError = DomainError::InvalidAddress("0xbad".into()).into();
        assert!(matches!(err, ApiError::UnprocessableEntity(_)));
    }

    #[tokio::test]
    async fn domain_invalid_address_status() {
        let err: ApiError = DomainError::InvalidAddress("0xbad".into()).into();
        assert_response(err, StatusCode::UNPROCESSABLE_ENTITY).await;
    }

    #[test]
    fn domain_invalid_id_maps_to_unprocessable() {
        let err: ApiError = DomainError::InvalidId("not-a-uuid".into()).into();
        assert!(matches!(err, ApiError::UnprocessableEntity(_)));
    }

    #[tokio::test]
    async fn domain_invalid_id_status() {
        let err: ApiError = DomainError::InvalidId("not-a-uuid".into()).into();
        assert_response(err, StatusCode::UNPROCESSABLE_ENTITY).await;
    }

    // -----------------------------------------------------------------------
    // From<GameError>
    // -----------------------------------------------------------------------

    #[test]
    fn game_unknown_variant_maps_to_bad_request() {
        let err: ApiError = GameError::UnknownVariant("fog-of-war".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn game_unknown_variant_status() {
        let err: ApiError = GameError::UnknownVariant("fog-of-war".into()).into();
        assert_response(err, StatusCode::BAD_REQUEST).await;
    }

    #[test]
    fn game_not_your_turn_maps_to_bad_request() {
        let err: ApiError = GameError::NotYourTurn.into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn game_not_your_turn_status() {
        let err: ApiError = GameError::NotYourTurn.into();
        assert_response(err, StatusCode::BAD_REQUEST).await;
    }

    #[test]
    fn game_illegal_action_maps_to_bad_request() {
        let err: ApiError = GameError::IllegalAction.into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn game_illegal_action_status() {
        let err: ApiError = GameError::IllegalAction.into();
        assert_response(err, StatusCode::BAD_REQUEST).await;
    }

    #[test]
    fn game_finished_maps_to_conflict() {
        let err: ApiError = GameError::Finished.into();
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    #[tokio::test]
    async fn game_finished_status() {
        let err: ApiError = GameError::Finished.into();
        assert_response(err, StatusCode::CONFLICT).await;
    }

    #[test]
    fn game_invalid_action_payload_maps_to_bad_request() {
        let err: ApiError = GameError::InvalidActionPayload("missing 'from'".into()).into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn game_invalid_action_payload_status() {
        let err: ApiError = GameError::InvalidActionPayload("missing 'from'".into()).into();
        assert_response(err, StatusCode::BAD_REQUEST).await;
    }

    #[test]
    fn game_serialization_maps_to_internal() {
        let err: ApiError = GameError::Serialization("serde: trailing comma".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn game_serialization_status() {
        let err: ApiError = GameError::Serialization("serde: trailing comma".into()).into();
        assert_response(err, StatusCode::INTERNAL_SERVER_ERROR).await;
    }

    #[tokio::test]
    async fn game_serialization_does_not_leak_detail() {
        let err: ApiError =
            GameError::Serialization("sensitive payload: {secret: true}".into()).into();
        let json = body_json(err).await;
        let detail = json["detail"].as_str().unwrap();
        assert!(!detail.contains("sensitive"));
        assert!(!detail.contains("secret"));
    }

    #[test]
    fn game_other_maps_to_internal() {
        let err: ApiError = GameError::Other("unexpected engine panic".into()).into();
        assert!(matches!(err, ApiError::Internal(_)));
    }

    #[tokio::test]
    async fn game_other_status() {
        let err: ApiError = GameError::Other("unexpected engine panic".into()).into();
        assert_response(err, StatusCode::INTERNAL_SERVER_ERROR).await;
    }
}
