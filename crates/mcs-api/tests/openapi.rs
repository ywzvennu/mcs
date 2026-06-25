//! Integration tests for the OpenAPI document and docs UI (#127).
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound) and assert that
//! `GET /openapi.json` returns a valid OpenAPI 3.x document covering the known
//! paths and component schemas, and that the Scalar docs UI at `GET /docs`
//! serves an HTML page.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::Duration;
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Builds the full API router backed by a fresh in-memory SQLite database.
async fn test_app() -> axum::Router {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let session = SessionConfig::new(
        b"test-secret-key-that-is-definitely-32-bytes!!".to_vec(),
        Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let siwe = SiweConfig::new(
        "localhost".to_owned(),
        "https://localhost".to_owned(),
        1,
        "Sign in to MCS.".to_owned(),
        Duration::minutes(10),
    );

    let state = AppState::new(storage, Arc::new(VariantRegistry::new()), session, siwe);
    router(state)
}

async fn body_bytes(body: Body) -> Vec<u8> {
    to_bytes(body, usize::MAX).await.unwrap().to_vec()
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = body_bytes(response.into_body()).await;
    (status, headers, bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `GET /openapi.json` returns 200 with a valid OpenAPI 3.x document.
#[tokio::test]
async fn openapi_json_is_served_and_valid() {
    let app = test_app().await;

    let (status, headers, bytes) = get(&app, "/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("application/json")),
        "openapi.json must be served as JSON"
    );

    let doc: Value = serde_json::from_slice(&bytes).expect("body is valid JSON");

    // It is an OpenAPI 3.x document.
    let version = doc["openapi"].as_str().expect("`openapi` version string");
    assert!(
        version.starts_with("3."),
        "must be an OpenAPI 3.x document, got {version}"
    );
    assert!(doc["info"]["title"].is_string(), "`info.title` is present");
    assert!(doc["paths"].is_object(), "`paths` is an object");
}

/// The document contains the known REST paths across every area.
#[tokio::test]
async fn openapi_json_covers_known_paths() {
    let app = test_app().await;
    let (_, _, bytes) = get(&app, "/openapi.json").await;
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    let paths = doc["paths"].as_object().expect("paths object");

    // A representative path from every area of the API.
    for expected in [
        "/health",
        "/ready",
        "/metrics",
        "/variants",
        "/auth/nonce",
        "/auth/verify",
        "/auth/logout",
        "/seeks",
        "/seeks/{id}/accept",
        "/seeks/{id}",
        "/challenges",
        "/challenges/{id}/accept",
        "/challenges/{id}/decline",
        "/challenges/{id}",
        "/games/{id}/rematch",
        "/games",
        "/games/{id}",
        "/games/{id}/moves",
        "/games/{id}/pgn",
        "/leaderboard",
        "/profile",
        "/users/{id}",
        "/users/{id}/status",
        "/users/{id}/ratings",
        "/users/{id}/rating-history",
    ] {
        assert!(
            paths.contains_key(expected),
            "OpenAPI document is missing path {expected}"
        );
    }

    // `GET` and `PUT` on `/profile` must both be documented.
    assert!(paths["/profile"]["get"].is_object(), "GET /profile");
    assert!(paths["/profile"]["put"].is_object(), "PUT /profile");
}

/// The document registers the expected component schemas, including the
/// problem+json error shape, and declares the bearer security scheme.
#[tokio::test]
async fn openapi_json_has_schemas_and_security() {
    let app = test_app().await;
    let (_, _, bytes) = get(&app, "/openapi.json").await;
    let doc: Value = serde_json::from_slice(&bytes).unwrap();

    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("component schemas object");
    for expected in [
        "ProblemDetails",
        "VerifyRequest",
        "VerifyResponse",
        "GameDto",
        "ChallengeDto",
        "SeekDto",
        "UserRatingsResponse",
        "RatingHistoryResponse",
        "TimeControl",
        "GameLifecycle",
    ] {
        assert!(
            schemas.contains_key(expected),
            "OpenAPI document is missing component schema {expected}"
        );
    }

    // The bearer security scheme must be declared for the authenticated routes.
    let security = doc["components"]["securitySchemes"]
        .as_object()
        .expect("security schemes object");
    assert!(
        security.contains_key("bearerAuth"),
        "bearerAuth security scheme must be declared"
    );

    // An authenticated route references it; a public one does not.
    assert!(
        doc["paths"]["/auth/logout"]["post"]["security"].is_array(),
        "POST /auth/logout must require bearer auth"
    );
}

/// The `/docs` Scalar UI route returns 200 with an HTML body.
#[tokio::test]
async fn docs_ui_serves_html() {
    let app = test_app().await;

    let (status, headers, bytes) = get(&app, "/docs").await;
    assert_eq!(status, StatusCode::OK);

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.contains("text/html"),
        "docs UI must be served as HTML, got {content_type}"
    );

    let body = String::from_utf8(bytes).expect("HTML body is UTF-8");
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html")
            || body.to_ascii_lowercase().contains("<html"),
        "docs UI body must be an HTML document"
    );
}
